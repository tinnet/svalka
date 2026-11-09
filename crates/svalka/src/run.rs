//! The `run` command: the control loop.
//!
//! Startup takes every configured fan output into manual mode. Each tick reads
//! every sensor once, lets each fan's [`FanController`] pick a duty, writes it
//! when it changed, writes the state snapshot, appends it to the history and
//! pets systemd's watchdog.
//! SIGHUP reloads the config. On exit (SIGINT, SIGTERM or an error) every
//! output goes back to the chip's own automatic mode, so the BIOS curve runs
//! the fans.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::mem;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use svalka_core::controller::{FanController, Tick};
use svalka_core::history::Line;
use svalka_core::pwm;
use svalka_core::state::{FanMode, FanSnapshot, Snapshot, VERSION};

use crate::binding::{self, BoundOutput, BoundSensor};
use crate::historyfile::HistoryFile;
use crate::hwmon::{HandBack, ManualOverride, PwmEnable, PwmState};
use crate::log;
use crate::notify::Notifier;
use crate::record::{self, HeldOutput, OutputRecord};
use crate::setup::Setup;
use crate::signals::DaemonSignals;
use crate::statefile::StateFile;

/// At or above this duty, a tachometer reading 0 rpm means a stalled fan.
const STALL_MIN_PERCENT: f64 = 30.0;

/// How long a fan must read 0 rpm before a stall is reported. The nct6775
/// driver refreshes rpm only every 1.5–2 s, and a fan needs a moment to spin up.
const STALL_TIME: Duration = Duration::from_secs(5);

/// Attempts in a row a fan's output may fail before the daemon gives up. A
/// single EIO from the Super-I/O (e.g. around a resume) should not stop the
/// daemon; an output that stays broken should.
const MAX_OUTPUT_ERRORS: u32 = 10;

/// Shown in systemd's status until a reload succeeds.
const RELOAD_FAILED: &str = "reload failed, still running the previous config; see the journal";

/// How often the loop pets systemd's watchdog while waiting for the next
/// tick. Well under the unit's `WatchdogSec=10s`.
const WATCHDOG_PET: Duration = Duration::from_secs(1);

/// Where `run` finds its config and hardware and puts its state.
pub struct Paths<'a> {
    pub config: &'a Path,
    pub hwmon_root: &'a Path,
    pub state_dir: &'a Path,
}

/// Runs the control loop until a stop signal or an error, then hands every
/// output back to the chip.
///
/// # Errors
///
/// Fails when the config does not load, the state directory is unusable, an
/// output cannot be taken over, or writing a duty fails. Outputs already
/// taken over are handed back first.
pub fn run(paths: &Paths, signals: &DaemonSignals, notifier: &Notifier) -> anyhow::Result<()> {
    let setup = Setup::load(paths.config, paths.hwmon_root)?;
    let state_file = StateFile::prepare(paths.state_dir)?;
    let mut daemon = Daemon::new(paths, state_file, notifier);

    let result = daemon.switch_to(setup).and_then(|()| {
        daemon.log_start();
        daemon.control_loop(signals)
    });
    notifier.stopping();
    daemon.shut_down();
    result
}

/// Everything the running loop owns.
struct Daemon<'a> {
    paths: &'a Paths<'a>,
    interval: Duration,
    readback_interval: Duration,
    /// When the fan outputs are next read back.
    next_readback: Instant,
    sensors: BTreeMap<String, BoundSensor>,
    fans: Vec<FanLoop>,
    state_file: StateFile,
    history: HistoryFile,
    /// The outputs we hold, for `restore`.
    record: OutputRecord,
    notifier: &'a Notifier,
    state_write_failing: Episode,
    history_write_failing: Episode,
    /// Appended to systemd's status line, e.g. after a failed reload.
    status_note: Option<&'static str>,
    /// The status line systemd has, so it is only sent when it changes.
    sent_status: String,
}

/// What ended a wait between ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wake {
    /// Time for the next tick.
    Tick,
    Stop,
    Reload,
}

impl<'a> Daemon<'a> {
    /// A daemon holding nothing yet; [`Daemon::switch_to`] gives it a config.
    fn new(paths: &'a Paths<'a>, state_file: StateFile, notifier: &'a Notifier) -> Self {
        Self {
            paths,
            interval: Duration::ZERO,
            readback_interval: Duration::ZERO,
            next_readback: Instant::now(),
            sensors: BTreeMap::new(),
            fans: Vec::new(),
            record: OutputRecord::in_dir(state_file.dir()),
            history: HistoryFile::open(state_file.dir()),
            state_file,
            notifier,
            state_write_failing: Episode::default(),
            history_write_failing: Episode::default(),
            status_note: None,
            sent_status: String::new(),
        }
    }

    fn log_start(&self) {
        log::info(format!(
            "controlling {} every {:?}; state in {}",
            fan_names(&self.fans),
            self.interval,
            self.state_file.path().display()
        ));
    }

    fn control_loop(&mut self, signals: &DaemonSignals) -> anyhow::Result<()> {
        self.tick_and_publish()?;
        self.notifier.ready(&self.sent_status);
        let mut next = Instant::now();
        loop {
            next += self.interval;
            match wait_until(next, signals, self.notifier) {
                Wake::Tick => {}
                Wake::Stop => {
                    log::info("stopping");
                    return Ok(());
                }
                Wake::Reload => {
                    self.reload();
                    next = Instant::now();
                }
            }
            // After a long stall (e.g. a debugger), restart the schedule instead of
            // running the missed ticks back to back.
            if Instant::now() > next + self.interval {
                next = Instant::now();
            }
            self.tick_and_publish()?;
        }
    }

    /// One tick, then the state file, the history and systemd's watchdog and
    /// status.
    fn tick_and_publish(&mut self) -> anyhow::Result<()> {
        let sensors = binding::read_sensors(&mut self.sensors);
        let readback = self.readback_due();
        let mut fans = BTreeMap::new();
        for fan in &mut self.fans {
            fans.insert(fan.name.clone(), fan.tick(&sensors, readback)?);
        }
        let snapshot = Snapshot {
            version: VERSION,
            unix_millis: unix_millis(),
            sensors,
            fans,
        };
        self.write_state(&snapshot);
        self.append_history(&Line::of(&snapshot, readback));
        self.notifier.watchdog();
        self.publish_status(&snapshot);
        Ok(())
    }

    /// Whether this tick reads the fan outputs back. All fans read back on the
    /// same tick, so one refresh of the chip's register cache serves them all.
    fn readback_due(&mut self) -> bool {
        let now = Instant::now();
        if now < self.next_readback {
            return false;
        }
        self.next_readback = now + self.readback_interval;
        true
    }

    /// Writes the snapshot, logging a failure once rather than every tick:
    /// controlling the fans matters more than publishing state.
    fn write_state(&mut self, snapshot: &Snapshot) {
        let result = self.state_file.write(snapshot);
        match (self.state_write_failing.observe(result.is_err()), result) {
            (Some(Edge::Began), Err(error)) => log::error(format!(
                "cannot write {}: {error}",
                self.state_file.path().display()
            )),
            (Some(Edge::Ended), _) => log::info("writing the state file works again"),
            _ => {}
        }
    }

    /// Appends the tick to the history, logging a failure once, like
    /// [`Daemon::write_state`].
    fn append_history(&mut self, line: &Line) {
        let result = self.history.append(line);
        match (self.history_write_failing.observe(result.is_err()), result) {
            (Some(Edge::Began), Err(error)) => log::error(format!(
                "cannot append to {}: {error}",
                self.history.path().display()
            )),
            (Some(Edge::Ended), _) => log::info("appending to the history works again"),
            _ => {}
        }
    }

    /// Sends the status line, but only when it changed: each change makes
    /// systemd notify everything watching the unit.
    fn publish_status(&mut self, snapshot: &Snapshot) {
        let mut status = status_line(snapshot);
        if let Some(note) = self.status_note {
            let _ = write!(status, " | {note}");
        }
        if status != self.sent_status {
            self.notifier.status(&status);
            self.sent_status = status;
        }
    }

    /// Loads the config again and switches to it. On any problem the running
    /// config stays, and the problem is logged.
    fn reload(&mut self) {
        self.notifier.reloading();
        let outcome = Setup::load(self.paths.config, self.paths.hwmon_root)
            .and_then(|setup| self.switch_to(setup));
        match outcome {
            Ok(()) => {
                log::info(format!(
                    "reloaded {}; controlling {}",
                    self.paths.config.display(),
                    fan_names(&self.fans)
                ));
                self.status_note = None;
                self.notifier.ready("reloaded");
            }
            Err(error) => {
                log::error(format!(
                    "reload failed, keeping the running config: {error:#}"
                ));
                self.status_note = Some(RELOAD_FAILED);
                self.notifier.ready(RELOAD_FAILED);
            }
        }
    }

    /// Switches to `setup`, at startup (from no fans) and on reload. New
    /// outputs are taken over first, so if that fails nothing has changed.
    /// Outputs in both configs stay in manual mode throughout; outputs no
    /// longer configured are handed back.
    fn switch_to(&mut self, setup: Setup) -> anyhow::Result<()> {
        // Recorded before any takeover, so `restore` finds every output even if
        // we die halfway; while switching we may hold outputs of both configs.
        let running = self.fans.iter().map(|fan| &fan.output);
        self.write_record(running.chain(setup.bindings.outputs.values()))?;

        let mut taken = BTreeMap::new();
        for (name, output) in &setup.bindings.outputs {
            if !self.fans.iter().any(|fan| fan.output.same_output(output)) {
                taken.insert(name.clone(), take_over(output)?);
            }
        }

        let mut old = mem::take(&mut self.fans);
        for (name, fan) in &setup.config.fans {
            let output = setup.bindings.outputs[name].clone();
            let controller = setup.config.controller(fan);
            let stall_after = stall_readbacks(setup.config.readback_interval);
            let fan_loop = if let Some(manual) = taken.remove(name) {
                FanLoop::new(name, output, manual, controller, stall_after)
            } else {
                let index = old
                    .iter()
                    .position(|kept| kept.output.same_output(&output))
                    .expect("outputs not taken over above are in the running config");
                old.swap_remove(index)
                    .renamed(name, output, controller, stall_after)
            };
            self.fans.push(fan_loop);
        }
        hand_back_all(old);
        self.interval = setup.config.interval;
        self.readback_interval = setup.config.readback_interval;
        // Read back on the next tick: new outputs, and a fresh rpm for all.
        self.next_readback = Instant::now();
        self.sensors = setup.bindings.sensors;

        // Only the new outputs now. If this fails the record lists a few
        // outputs too many, which `restore` handles fine.
        let held = self.fans.iter().map(|fan| &fan.output);
        if let Err(error) = self.write_record(held) {
            log::error(format!("{error:#}"));
        }
        Ok(())
    }

    fn write_record<'o>(
        &self,
        outputs: impl Iterator<Item = &'o BoundOutput>,
    ) -> anyhow::Result<()> {
        let held = outputs
            .map(HeldOutput::of)
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.record
            .write(&record::dedup(held))
            .with_context(|| format!("cannot write {}", self.record.path().display()))
    }

    /// Hands the fans back and removes the state file and record. The history
    /// stays, so a restart does not lose the recent past.
    fn shut_down(self) {
        hand_back_all(self.fans);
        let removals = [
            (self.state_file.path(), self.state_file.remove()),
            (self.record.path(), self.record.remove()),
        ];
        for (path, result) in removals {
            if let Err(error) = result {
                log::error(format!("cannot remove {}: {error}", path.display()));
            }
        }
    }
}

/// A condition logged once when it begins and once when it ends, not on every
/// tick in between: failsafe, interference, stalls, failing writes.
#[derive(Debug, Clone, Copy)]
struct Episode {
    /// Ticks in a row the condition has held.
    ticks: u32,
    /// Ticks in a row it must hold before it counts as begun.
    after: u32,
}

/// Where an [`Episode`] changed this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    Began,
    Ended,
}

impl Episode {
    /// An episode that begins once its condition held for `ticks` ticks.
    const fn after(ticks: u32) -> Self {
        Self {
            ticks: 0,
            after: ticks,
        }
    }

    /// Records one tick in which the condition does or does not hold.
    fn observe(&mut self, holds: bool) -> Option<Edge> {
        if holds {
            self.ticks += 1;
            (self.ticks == self.after).then_some(Edge::Began)
        } else {
            let had_begun = self.ticks >= self.after;
            self.ticks = 0;
            had_begun.then_some(Edge::Ended)
        }
    }

    fn is_active(self) -> bool {
        self.ticks > 0
    }
}

impl Default for Episode {
    /// Begins on the first tick the condition holds.
    fn default() -> Self {
        Self::after(1)
    }
}

/// One fan's runtime state.
struct FanLoop {
    name: String,
    output: BoundOutput,
    /// `output.describe()`, kept for the per-tick snapshot and messages.
    description: String,
    manual: ManualOverride,
    controller: FanController,
    /// The raw duty we last wrote.
    written: Option<u8>,
    /// `pwmN_enable` and the tachometer at the last readback.
    pwm_enable: Option<PwmEnable>,
    rpm: Option<u32>,
    failsafe: Episode,
    interference: Episode,
    stall: Episode,
    /// Reading or writing the output failing, attempt after attempt.
    output_errors: Episode,
}

/// Readbacks in a row that must show 0 rpm before a stall is reported: about
/// [`STALL_TIME`], but at least two, so one stale reading is not enough.
fn stall_readbacks(readback_interval: Duration) -> u32 {
    let readbacks = STALL_TIME.as_secs_f64() / readback_interval.as_secs_f64();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // small and positive
    let readbacks = readbacks.ceil() as u32;
    readbacks.max(2)
}

/// Switches `output` to manual, to be handed back to the chip's automatic mode.
fn take_over(output: &BoundOutput) -> anyhow::Result<ManualOverride> {
    let manual = output.take_over(HandBack::Automatic(output.automatic_mode()?))?;
    log::info(format!(
        "took over {} (was {}); it returns to the chip's own curve on exit",
        output.describe(),
        manual.saved()
    ));
    Ok(manual)
}

/// E.g. `cpu_cooler 48 % (cpu 49 °C), exhaust 100 % (failsafe)`, for
/// `systemctl status`. Whole degrees, so it does not change every tick.
fn status_line(snapshot: &Snapshot) -> String {
    let mut line = String::new();
    for (name, fan) in &snapshot.fans {
        if !line.is_empty() {
            line += ", ";
        }
        let _ = write!(line, "{name} {:.0} %", fan.duty);
        match (fan.mode, &fan.source, fan.celsius) {
            (FanMode::Failsafe, _, _) => line += " (failsafe)",
            (FanMode::Normal, Some(source), Some(celsius)) => {
                let _ = write!(line, " ({source} {celsius:.0} °C)");
            }
            (FanMode::Normal, _, _) => {}
        }
    }
    line
}

fn fan_names(fans: &[FanLoop]) -> String {
    fans.iter()
        .map(|fan| fan.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

impl FanLoop {
    fn new(
        name: &str,
        output: BoundOutput,
        manual: ManualOverride,
        controller: FanController,
        stall_after: u32,
    ) -> Self {
        Self {
            name: name.to_owned(),
            description: output.describe(),
            output,
            manual,
            controller,
            written: None,
            pwm_enable: None,
            rpm: None,
            failsafe: Episode::default(),
            interference: Episode::default(),
            stall: Episode::after(stall_after),
            output_errors: Episode::default(),
        }
    }

    /// The same output under a reloaded config: keeps the takeover and the duty
    /// last written, starts everything else afresh.
    fn renamed(
        self,
        name: &str,
        output: BoundOutput,
        controller: FanController,
        stall_after: u32,
    ) -> Self {
        Self {
            written: self.written,
            ..Self::new(name, output, self.manual, controller, stall_after)
        }
    }

    /// One tick: a duty from the controller, written if it changed. On
    /// `readback` ticks also reads the output and tachometer back.
    fn tick(
        &mut self,
        sensors: &BTreeMap<String, Option<f64>>,
        readback: bool,
    ) -> anyhow::Result<FanSnapshot> {
        let tick = self.controller.tick(sensors);
        self.report_failsafe(&tick);
        let raw = pwm::percent_to_raw(tick.duty);
        self.drive_output(raw, readback)?;
        if readback {
            self.rpm = self.output.read_rpm().ok().flatten();
            self.check_stall(raw);
        }
        Ok(self.snapshot(&tick, raw))
    }

    /// [`FanLoop::write_duty`], tolerating a failing output for up to
    /// [`MAX_OUTPUT_ERRORS`] attempts in a row; after that the daemon gives
    /// up, so the chip's own curve and `restore` take over. Ticks that leave
    /// the output alone neither count nor reset.
    fn drive_output(&mut self, raw: u8, readback: bool) -> anyhow::Result<()> {
        let result = self.write_duty(raw, readback);
        if matches!(result, Ok(false)) {
            return Ok(());
        }
        let edge = self.output_errors.observe(result.is_err());
        match (&result, edge) {
            (Err(error), Some(Edge::Began)) => {
                log::warning(format!("{error:#}; retrying every tick"));
            }
            (Ok(_), Some(Edge::Ended)) => {
                log::info(format!("{}: {} works again", self.name, self.description));
            }
            _ => {}
        }
        if self.output_errors.ticks >= MAX_OUTPUT_ERRORS {
            bail!(
                "{}: {} failed {MAX_OUTPUT_ERRORS} times in a row; giving up so the chip's \
                 own curve takes over",
                self.name,
                self.description
            );
        }
        Ok(())
    }

    /// Writes `raw` if it differs from what we wrote last. On `readback` ticks
    /// it first reads the output and takes it back if something else changed
    /// it; it also takes it back after a failed attempt, when we cannot know
    /// what it holds. Returns whether it touched the output at all.
    fn write_duty(&mut self, raw: u8, readback: bool) -> anyhow::Result<bool> {
        let interfered = readback && self.read_back()?;
        if interfered || self.output_errors.is_active() {
            self.manual
                .reassert(raw)
                .with_context(|| format!("cannot take back {}", self.label()))?;
        } else if self.written != Some(raw) {
            self.manual
                .set(raw)
                .with_context(|| format!("cannot write {}", self.label()))?;
        } else {
            return Ok(readback);
        }
        self.written = Some(raw);
        Ok(true)
    }

    /// Reads the output's duty and mode. Returns whether something else
    /// changed it since our last write.
    fn read_back(&mut self) -> anyhow::Result<bool> {
        self.pwm_enable = None;
        let current = self
            .manual
            .current()
            .with_context(|| format!("cannot read {}", self.label()))?;
        self.pwm_enable = current.enable;
        let interfered = self
            .written
            .is_some_and(|written| !current.is_held_at(written));
        self.report_interference(interfered, current);
        Ok(interfered)
    }

    /// E.g. `cpu_cooler: nct6798 pwm2`, for error messages.
    fn label(&self) -> String {
        format!("{}: {}", self.name, self.description)
    }

    fn report_failsafe(&mut self, tick: &Tick) {
        match (
            self.failsafe.observe(tick.failsafe.is_some()),
            &tick.failsafe,
        ) {
            (Some(Edge::Began), Some(reason)) => {
                log::warning(format!(
                    "{}: {reason}; running at {:.0} %",
                    self.name, tick.duty
                ));
            }
            (Some(Edge::Ended), _) => {
                log::info(format!(
                    "{}: sensors read normally again; back on the curve",
                    self.name
                ));
            }
            _ => {}
        }
    }

    /// Another program, or a resume from suspend, changed the output.
    fn report_interference(&mut self, interfered: bool, found: PwmState) {
        match self.interference.observe(interfered) {
            Some(Edge::Began) => log::warning(format!(
                "{}: {} changed to {found} behind our back (resume from suspend, or another \
                 fan-control program); taking it back",
                self.name, self.description
            )),
            Some(Edge::Ended) => {
                log::info(format!("{}: {} is ours again", self.name, self.description));
            }
            None => {}
        }
    }

    /// Warns once when the tachometer reads 0 rpm on several readbacks in a
    /// row (see [`stall_readbacks`]) at a duty that should turn the fan.
    fn check_stall(&mut self, raw: u8) {
        let stalled = self.rpm == Some(0) && raw >= pwm::percent_to_raw(STALL_MIN_PERCENT);
        match self.stall.observe(stalled) {
            Some(Edge::Began) => log::warning(format!(
                "{}: 0 rpm at {} % on {} readbacks in a row; the fan may be stalled or unplugged",
                self.name,
                pwm::raw_to_percent(raw),
                self.stall.after
            )),
            Some(Edge::Ended) => log::info(format!("{}: spinning again", self.name)),
            None => {}
        }
    }

    fn snapshot(&self, tick: &Tick, raw: u8) -> FanSnapshot {
        FanSnapshot {
            output: self.description.clone(),
            mode: if tick.failsafe.is_some() {
                FanMode::Failsafe
            } else {
                FanMode::Normal
            },
            source: tick.hottest.as_ref().map(|(source, _)| source.clone()),
            celsius: tick.hottest.as_ref().map(|&(_, celsius)| celsius),
            smoothed_celsius: tick.smoothed,
            effective_celsius: tick.effective,
            duty: tick.duty,
            pwm_raw: raw,
            pwm_enable: self.pwm_enable.map(u8::from),
            rpm: self.rpm,
        }
    }
}

/// Sleeps until `deadline`, waking at once for a stop or reload signal. A
/// reload request is consumed. Pets the watchdog every [`WATCHDOG_PET`]
/// meanwhile, so a long `interval` cannot trip it; a hung tick still does.
fn wait_until(deadline: Instant, signals: &DaemonSignals, notifier: &Notifier) -> Wake {
    let mut last_pet = Instant::now();
    loop {
        if signals.stop.load(Ordering::Relaxed) {
            return Wake::Stop;
        }
        if signals.reload.swap(false, Ordering::Relaxed) {
            return Wake::Reload;
        }
        let now = Instant::now();
        if now >= deadline {
            return Wake::Tick;
        }
        if now - last_pet >= WATCHDOG_PET {
            notifier.watchdog();
            last_pet = now;
        }
        signals.sleep((deadline - now).min(WATCHDOG_PET));
    }
}

fn hand_back_all(fans: Vec<FanLoop>) {
    for fan in fans {
        match fan.manual.hand_back() {
            Ok(()) => log::info(format!(
                "{}: handed {} back to the chip",
                fan.name, fan.description
            )),
            Err(error) => log::error(format!(
                "{}: could not hand back {}: {error}",
                fan.name, fan.description
            )),
        }
    }
}

/// The current time in milliseconds since the Unix epoch, as snapshots carry it.
#[must_use]
pub fn unix_millis() -> u64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since_epoch.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::hwmon::testing::FakeSysfs;

    const CONFIG: &str = r#"
[sensors]
cpu = { chip = "k10temp", label = "Tctl" }

[fan.cpu_cooler]
output  = { chip = "nct6798", pwm = 2 }
sources = ["cpu"]
curve   = [[40, 40], [60, 60]]
"#;

    /// A fake PC with the CPU at 50 °C (-> 50 % -> raw 128) and both nct6798
    /// outputs in SmartFan IV.
    fn fake_pc() -> FakeSysfs {
        let sysfs = FakeSysfs::new();
        sysfs
            .chip("hwmon3", "k10temp", "pci0000:00/0000:00:18.3")
            .attr("temp1_input", "50000")
            .attr("temp1_label", "Tctl");
        sysfs
            .chip("hwmon7", "nct6798", "platform/nct6775.656")
            .driver("nct6775")
            .attr("pwm1", "74")
            .attr("pwm1_enable", "5")
            .attr("pwm2", "98")
            .attr("pwm2_enable", "5")
            .attr("fan2_input", "1036");
        write_config(&sysfs, CONFIG);
        sysfs
    }

    fn config_path(sysfs: &FakeSysfs) -> std::path::PathBuf {
        sysfs.hwmon_root().join("../../svalka.toml")
    }

    fn write_config(sysfs: &FakeSysfs, text: &str) {
        fs::write(config_path(sysfs), text).unwrap();
    }

    /// One attribute of the fake nct6798.
    fn attribute(sysfs: &FakeSysfs, file: &str) -> String {
        fs::read_to_string(sysfs.hwmon_root().join("hwmon7").join(file))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn pwm2(sysfs: &FakeSysfs) -> (String, String) {
        (attribute(sysfs, "pwm2"), attribute(sysfs, "pwm2_enable"))
    }

    /// Runs `test` on a daemon started on the fake PC's `CONFIG`, after one tick.
    fn with_daemon(test: impl FnOnce(&FakeSysfs, &mut Daemon)) {
        let sysfs = fake_pc();
        let shm = tempfile::tempdir_in("/dev/shm").unwrap();
        let config = config_path(&sysfs);
        let hwmon_root = sysfs.hwmon_root();
        let paths = Paths {
            config: &config,
            hwmon_root: &hwmon_root,
            state_dir: shm.path(),
        };
        let notifier = Notifier::disabled();
        let mut daemon = Daemon::new(&paths, StateFile::prepare(shm.path()).unwrap(), &notifier);
        daemon
            .switch_to(Setup::load(&config, &hwmon_root).unwrap())
            .unwrap();
        daemon.tick_and_publish().unwrap();
        test(&sysfs, &mut daemon);
    }

    #[test]
    fn a_tick_writes_the_curve_duty_and_publishes_it() {
        with_daemon(|sysfs, daemon| {
            assert_eq!(pwm2(sysfs), ("128".into(), "1".into()));
            let snapshot =
                Snapshot::from_json(&fs::read_to_string(daemon.state_file.path()).unwrap())
                    .unwrap();
            let fan = &snapshot.fans["cpu_cooler"];
            assert_eq!(
                (fan.pwm_raw, fan.rpm, fan.mode),
                (128, Some(1036), FanMode::Normal)
            );
            assert_eq!(daemon.sent_status, "cpu_cooler 50 % (cpu 50 °C)");
        });
    }

    #[test]
    fn an_output_switched_away_is_taken_back() {
        with_daemon(|sysfs, daemon| {
            // What a resume from suspend can do: the BIOS takes the output back.
            daemon.fans[0]
                .output
                .chip()
                .write_pwm_enable(2, PwmEnable::Automatic(5))
                .unwrap();

            daemon.tick_and_publish().unwrap();
            assert_eq!(pwm2(sysfs).1, "5", "not noticed between readbacks");

            daemon.next_readback = Instant::now();
            daemon.tick_and_publish().unwrap();
            assert_eq!(pwm2(sysfs), ("128".into(), "1".into()));
            assert!(daemon.fans[0].interference.is_active());
        });
    }

    #[test]
    fn shutting_down_hands_outputs_back_and_cleans_up() {
        with_daemon(|sysfs, daemon| {
            let state = daemon.state_file.path().to_path_buf();
            let record = daemon.record.path().to_path_buf();
            assert!(state.exists() && record.exists());

            let daemon = mem::replace(
                daemon,
                Daemon::new(daemon.paths, daemon.state_file.clone(), daemon.notifier),
            );
            daemon.shut_down();

            assert_eq!(pwm2(sysfs).1, "5");
            assert!(!state.exists() && !record.exists());
        });
    }

    #[test]
    fn a_sensor_whose_driver_comes_back_as_another_chip_is_found_again() {
        with_daemon(|sysfs, daemon| {
            // `modprobe -r k10temp`: the input is gone, so the fan runs at failsafe.
            fs::remove_dir_all(sysfs.hwmon_root().join("hwmon3")).unwrap();
            daemon.tick_and_publish().unwrap();
            assert_eq!(pwm2(sysfs).0, "255");

            // `modprobe k10temp`, but the kernel numbers it hwmon8 this time.
            sysfs
                .chip("hwmon8", "k10temp", "pci0000:00/0000:00:18.3")
                .attr("temp1_input", "50000")
                .attr("temp1_label", "Tctl");
            daemon.tick_and_publish().unwrap();
            assert_eq!(pwm2(sysfs), ("128".into(), "1".into()));
        });
    }

    #[test]
    fn an_output_error_is_retried_and_only_fatal_when_it_lasts() {
        with_daemon(|sysfs, daemon| {
            let pwm2_file = sysfs.hwmon_root().join("hwmon7/pwm2");
            let set_mode =
                |mode| fs::set_permissions(&pwm2_file, fs::Permissions::from_mode(mode)).unwrap();

            // A changed duty forces a write, which now fails.
            set_mode(0o444);
            daemon.fans[0].written = Some(0);
            daemon.tick_and_publish().unwrap();
            assert_eq!(daemon.fans[0].output_errors.ticks, 1);

            set_mode(0o644);
            daemon.tick_and_publish().unwrap();
            assert!(!daemon.fans[0].output_errors.is_active());
            assert_eq!(pwm2(sysfs), ("128".into(), "1".into()));

            set_mode(0o444);
            for _ in 1..MAX_OUTPUT_ERRORS {
                daemon.fans[0].written = Some(0);
                daemon.tick_and_publish().unwrap();
            }
            daemon.fans[0].written = Some(0);
            assert!(daemon.tick_and_publish().is_err());
        });
    }

    #[test]
    fn a_stall_counts_only_at_a_duty_that_should_spin() {
        with_daemon(|_sysfs, daemon| {
            let fan = &mut daemon.fans[0];
            let after = fan.stall.after;
            fan.rpm = Some(0);

            fan.check_stall(pwm::percent_to_raw(20.0));
            assert!(!fan.stall.is_active());

            for _ in 0..after {
                fan.check_stall(pwm::percent_to_raw(50.0));
            }
            assert_eq!(fan.stall.ticks, after);

            fan.rpm = Some(800);
            fan.check_stall(pwm::percent_to_raw(50.0));
            assert!(!fan.stall.is_active());
        });
    }

    #[test]
    fn a_stall_needs_about_five_seconds_but_two_readbacks() {
        assert_eq!(stall_readbacks(Duration::from_secs(1)), 5);
        assert_eq!(stall_readbacks(Duration::from_secs(2)), 3);
        assert_eq!(stall_readbacks(Duration::from_secs(10)), 2);
    }

    #[test]
    fn ticks_between_readbacks_do_not_read_the_outputs() {
        with_daemon(|sysfs, daemon| {
            // Readback ran on the first tick; until the next one, a fan whose
            // duty does not change leaves its output untouched.
            fs::remove_file(sysfs.hwmon_root().join("hwmon7/fan2_input")).unwrap();
            daemon.tick_and_publish().unwrap();
            assert_eq!(
                daemon.fans[0].rpm,
                Some(1036),
                "kept from the last readback"
            );

            daemon.next_readback = Instant::now();
            daemon.tick_and_publish().unwrap();
            assert_eq!(daemon.fans[0].rpm, None);
        });
    }

    #[test]
    fn an_episode_begins_after_its_ticks_and_ends_once() {
        let mut stall = Episode::after(3);

        assert_eq!(stall.observe(true), None);
        assert_eq!(stall.observe(true), None);
        assert_eq!(stall.observe(true), Some(Edge::Began));
        assert_eq!(stall.observe(true), None);
        assert_eq!(stall.observe(false), Some(Edge::Ended));
        assert_eq!(stall.observe(false), None);

        let mut short = Episode::after(3);
        short.observe(true);
        assert_eq!(short.observe(false), None, "never began, so never ends");
    }

    #[test]
    fn wait_until_wakes_for_signals_and_deadlines() {
        let far = Instant::now() + Duration::from_secs(60);
        let signals = DaemonSignals::unregistered().unwrap();
        let notifier = Notifier::disabled();

        signals.reload.store(true, Ordering::Relaxed);
        assert_eq!(wait_until(far, &signals, &notifier), Wake::Reload);
        assert!(
            !signals.reload.load(Ordering::Relaxed),
            "the request is consumed"
        );

        let soon = Instant::now() + Duration::from_millis(20);
        assert_eq!(wait_until(soon, &signals, &notifier), Wake::Tick);

        signals.stop.store(true, Ordering::Relaxed);
        assert_eq!(wait_until(far, &signals, &notifier), Wake::Stop);
    }

    #[test]
    fn reload_applies_a_new_curve_without_letting_go() {
        with_daemon(|sysfs, daemon| {
            write_config(
                sysfs,
                &CONFIG.replace("[[40, 40], [60, 60]]", "[[40, 80], [60, 100]]"),
            );

            daemon.reload();
            assert_eq!(
                attribute(sysfs, "pwm2_enable"),
                "1",
                "still ours during the switch"
            );
            daemon.tick_and_publish().unwrap();

            assert_eq!(
                attribute(sysfs, "pwm2"),
                pwm::percent_to_raw(90.0).to_string()
            );
        });
    }

    #[test]
    fn reload_hands_back_removed_fans_and_takes_new_ones() {
        with_daemon(|sysfs, daemon| {
            write_config(
                sysfs,
                &CONFIG
                    .replace("[fan.cpu_cooler]", "[fan.exhaust]")
                    .replace("pwm = 2", "pwm = 1"),
            );

            daemon.reload();

            assert_eq!(fan_names(&daemon.fans), "exhaust");
            assert_eq!(attribute(sysfs, "pwm2_enable"), "5");
            assert_eq!(attribute(sysfs, "pwm1_enable"), "1");
        });
    }

    #[test]
    fn a_broken_config_keeps_the_running_one() {
        with_daemon(|sysfs, daemon| {
            write_config(
                sysfs,
                &CONFIG.replace("[[40, 40], [60, 60]]", "[[60, 40], [40, 60]]"),
            );

            daemon.reload();
            daemon.tick_and_publish().unwrap();

            assert_eq!(fan_names(&daemon.fans), "cpu_cooler");
            assert_eq!(attribute(sysfs, "pwm2_enable"), "1");
            assert!(
                daemon.sent_status.ends_with(RELOAD_FAILED),
                "{}",
                daemon.sent_status
            );
        });
    }

    #[test]
    fn status_line_names_each_fan_and_its_driver() {
        let fan = |duty, mode, source: Option<&str>, celsius| FanSnapshot {
            output: "nct6798 pwm2".into(),
            mode,
            source: source.map(str::to_owned),
            celsius,
            smoothed_celsius: None,
            effective_celsius: None,
            duty,
            pwm_raw: 0,
            pwm_enable: Some(1),
            rpm: None,
        };
        let snapshot = Snapshot {
            version: VERSION,
            unix_millis: 0,
            sensors: BTreeMap::new(),
            fans: BTreeMap::from([
                (
                    "cpu_cooler".into(),
                    fan(47.66, FanMode::Normal, Some("cpu"), Some(49.125)),
                ),
                ("exhaust".into(), fan(100.0, FanMode::Failsafe, None, None)),
            ]),
        };

        assert_eq!(
            status_line(&snapshot),
            "cpu_cooler 48 % (cpu 49 °C), exhaust 100 % (failsafe)"
        );
    }
}
