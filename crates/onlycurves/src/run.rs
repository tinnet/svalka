//! The `run` command: the control loop.
//!
//! Startup takes every configured fan output into manual mode. Each tick reads
//! every sensor once, lets each fan's [`FanController`] pick a duty, writes it
//! when it changed, writes the state snapshot and pets systemd's watchdog.
//! SIGHUP reloads the config. On exit (SIGINT, SIGTERM or an error) every
//! output goes back to the chip's own automatic mode, so the BIOS curve runs
//! the fans.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::mem;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail};
use onlycurves_core::controller::{FanController, Tick};
use onlycurves_core::pwm;
use onlycurves_core::state::{FanMode, FanSnapshot, Snapshot, VERSION};

use crate::binding::{Bindings, BoundOutput};
use crate::hwmon::{HandBack, ManualOverride, automatic_mode, is_held};
use crate::log;
use crate::notify::Notifier;
use crate::record::{HeldOutput, OutputRecord};
use crate::setup::Setup;
use crate::signals::DaemonSignals;
use crate::statefile::StateFile;

/// At or above this duty, a tachometer reading 0 rpm means a stalled fan.
const STALL_MIN_PERCENT: f64 = 30.0;

/// Ticks of 0 rpm before a stall is reported. The nct6775 driver refreshes
/// rpm only every 1.5–2 s, and a fan needs a moment to spin up.
const STALL_TICKS: u32 = 5;

/// Ticks in a row a fan's output may fail before the daemon gives up. A
/// single EIO from the Super-I/O (e.g. around a resume) should not stop the
/// daemon; an output that stays broken should.
const MAX_OUTPUT_ERRORS: u32 = 10;

/// Shown in systemd's status until a reload succeeds.
const RELOAD_FAILED: &str = "reload failed, still running the previous config; see the journal";

/// How often the loop pets systemd's watchdog while waiting for the next
/// tick. Well under the unit's `WatchdogSec=10s`.
const WATCHDOG_PET: Duration = Duration::from_secs(1);

/// How often a sleeping loop checks for signals.
const SIGNAL_CHECK: Duration = Duration::from_millis(100);

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
    // Recorded before the takeover, so `restore` finds every output even if
    // we die halfway through it.
    let record = OutputRecord::in_dir(state_file.dir());
    record
        .write(&held_outputs(setup.bindings.outputs.values())?)
        .with_context(|| format!("cannot write {}", record.path().display()))?;
    let fans = take_over_fans(&setup)?;
    let mut daemon = Daemon {
        paths,
        setup,
        fans,
        state_file,
        record,
        notifier,
        state_write_failed: false,
        status_note: None,
    };
    daemon.log_start();

    let result = daemon.control_loop(signals);
    notifier.stopping();
    daemon.shut_down();
    result
}

/// Everything the running loop owns.
struct Daemon<'a> {
    paths: &'a Paths<'a>,
    setup: Setup,
    fans: Vec<FanLoop>,
    state_file: StateFile,
    /// The outputs we hold, for `restore`.
    record: OutputRecord,
    notifier: &'a Notifier,
    state_write_failed: bool,
    /// Appended to systemd's status line, e.g. after a failed reload.
    status_note: Option<&'static str>,
}

/// What ended a wait between ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wake {
    /// Time for the next tick.
    Tick,
    Stop,
    Reload,
}

impl Daemon<'_> {
    fn log_start(&self) {
        log::info(format!(
            "controlling {} every {:?}; state in {}",
            fan_names(&self.fans),
            self.setup.config.interval,
            self.state_file.path().display()
        ));
    }

    fn control_loop(&mut self, signals: &DaemonSignals) -> anyhow::Result<()> {
        let mut next = Instant::now();
        let mut ready = false;
        loop {
            let status = self.tick_and_publish()?;
            if !ready {
                self.notifier.ready(&status);
                ready = true;
            }

            next += self.setup.config.interval;
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
            if Instant::now() > next + self.setup.config.interval {
                next = Instant::now();
            }
        }
    }

    /// One tick, then the state file and systemd's status. Returns the status line.
    fn tick_and_publish(&mut self) -> anyhow::Result<String> {
        let snapshot = tick(&self.setup.bindings, &mut self.fans)?;
        write_state(&self.state_file, &snapshot, &mut self.state_write_failed);
        let mut status = status_line(&snapshot);
        if let Some(note) = self.status_note {
            status = format!("{status} | {note}");
        }
        self.notifier.tick(&status);
        Ok(status)
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

    /// Switches to `setup`. New outputs are taken over first, so if that fails
    /// nothing has changed. Outputs in both configs stay in manual mode
    /// throughout; outputs no longer configured are handed back.
    fn switch_to(&mut self, setup: Setup) -> anyhow::Result<()> {
        // While switching we may hold outputs of both configs.
        let running = self.fans.iter().map(|fan| &fan.output);
        self.record
            .write(&held_outputs(
                running.chain(setup.bindings.outputs.values()),
            )?)
            .with_context(|| format!("cannot write {}", self.record.path().display()))?;

        let mut taken = BTreeMap::new();
        for (name, output) in &setup.bindings.outputs {
            if !self.fans.iter().any(|fan| fan.output.same_output(output)) {
                taken.insert(name.clone(), take_over(output)?);
            }
        }

        let mut old = mem::take(&mut self.fans);
        for (name, fan) in &setup.config.fans {
            let output = setup.bindings.outputs[name].clone();
            let controller = FanController::new(
                fan.clone(),
                setup.config.interval,
                setup.config.failsafe_duty,
            );
            let fan_loop = if let Some(manual) = taken.remove(name) {
                FanLoop::new(name, output, manual, controller)
            } else {
                let index = old
                    .iter()
                    .position(|kept| kept.output.same_output(&output))
                    .expect("outputs not taken over above are in the running config");
                old.swap_remove(index).renamed(name, output, controller)
            };
            self.fans.push(fan_loop);
        }
        hand_back_all(old);
        self.setup = setup;
        // Only the new outputs now. If this fails the record lists a few
        // outputs too many, which `restore` handles fine.
        let held = held_outputs(self.fans.iter().map(|fan| &fan.output))?;
        if let Err(error) = self.record.write(&held) {
            log::error(format!(
                "cannot write {}: {error}",
                self.record.path().display()
            ));
        }
        Ok(())
    }

    fn shut_down(self) {
        hand_back_all(self.fans);
        if let Err(error) = self.state_file.remove() {
            log::error(format!(
                "cannot remove {}: {error}",
                self.state_file.path().display()
            ));
        }
        if let Err(error) = self.record.remove() {
            log::error(format!(
                "cannot remove {}: {error}",
                self.record.path().display()
            ));
        }
    }
}

/// One fan's runtime state.
struct FanLoop {
    name: String,
    output: BoundOutput,
    manual: ManualOverride,
    controller: FanController,
    /// The raw duty we last wrote.
    written: Option<u8>,
    in_failsafe: bool,
    interfered: bool,
    stalled_ticks: u32,
    /// Ticks in a row in which reading or writing the output failed.
    output_errors: u32,
}

fn take_over_fans(setup: &Setup) -> anyhow::Result<Vec<FanLoop>> {
    let mut fans = Vec::new();
    for (name, fan) in &setup.config.fans {
        // `bind` succeeded, so every fan has an output.
        let output = setup.bindings.outputs[name].clone();
        let manual = take_over(&output)?;
        let controller = FanController::new(
            fan.clone(),
            setup.config.interval,
            setup.config.failsafe_duty,
        );
        fans.push(FanLoop::new(name, output, manual, controller));
    }
    Ok(fans)
}

/// Switches `output` to manual, to be handed back to the chip's automatic mode.
fn take_over(output: &BoundOutput) -> anyhow::Result<ManualOverride> {
    let mode = firmware_mode(output)?;
    let manual = output.take_over(HandBack::Automatic(mode))?;
    log::info(format!(
        "took over {} (was {}); it returns to the chip's own curve on exit",
        output.describe(),
        manual.saved()
    ));
    Ok(manual)
}

/// The chip's automatic mode for `output`, which it is handed back to.
fn firmware_mode(output: &BoundOutput) -> anyhow::Result<u8> {
    automatic_mode(output.chip()).ok_or_else(|| {
        anyhow!(
            "{}: onlycurves does not know this chip's automatic mode, so it could not \
             hand the fan back on exit; only nct6775-family chips are supported",
            output.describe()
        )
    })
}

/// `outputs` as record entries, each physical output once.
fn held_outputs<'a>(
    outputs: impl Iterator<Item = &'a BoundOutput>,
) -> anyhow::Result<Vec<HeldOutput>> {
    let mut held: Vec<HeldOutput> = Vec::new();
    for output in outputs {
        let entry = HeldOutput::of(output, firmware_mode(output)?);
        if !held.iter().any(|known| known.same_output(&entry)) {
            held.push(entry);
        }
    }
    Ok(held)
}

/// One tick for every fan. Returns the snapshot to publish.
fn tick(bindings: &Bindings, fans: &mut [FanLoop]) -> anyhow::Result<Snapshot> {
    let sensors = read_sensors(bindings);
    let mut snapshots = BTreeMap::new();
    for fan in fans {
        snapshots.insert(fan.name.clone(), fan.tick(&sensors)?);
    }
    Ok(Snapshot {
        version: VERSION,
        unix_millis: unix_millis(),
        sensors,
        fans: snapshots,
    })
}

/// E.g. `cpu_cooler 48 % (cpu 49.1 °C), exhaust 100 % (failsafe)`, for
/// `systemctl status`.
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
                let _ = write!(line, " ({source} {celsius:.1} °C)");
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

/// Every sensor's temperature, or `None` when it cannot be read.
fn read_sensors(bindings: &Bindings) -> BTreeMap<String, Option<f64>> {
    bindings
        .sensors
        .iter()
        .map(|(name, sensor)| (name.clone(), sensor.read_celsius().ok()))
        .collect()
}

impl FanLoop {
    fn new(
        name: &str,
        output: BoundOutput,
        manual: ManualOverride,
        controller: FanController,
    ) -> Self {
        Self {
            name: name.to_owned(),
            output,
            manual,
            controller,
            written: None,
            in_failsafe: false,
            interfered: false,
            stalled_ticks: 0,
            output_errors: 0,
        }
    }

    /// The same output under a reloaded config: keeps the takeover and the duty
    /// last written, takes the new name and controller.
    fn renamed(self, name: &str, output: BoundOutput, controller: FanController) -> Self {
        Self {
            name: name.to_owned(),
            output,
            controller,
            in_failsafe: false,
            ..self
        }
    }

    fn tick(&mut self, sensors: &BTreeMap<String, Option<f64>>) -> anyhow::Result<FanSnapshot> {
        let tick = self.controller.tick(sensors);
        self.report_failsafe(&tick);
        let raw = pwm::percent_to_raw(tick.duty);
        let pwm_enable = match self.write_duty(raw) {
            Ok(pwm_enable) => {
                self.output_recovered();
                pwm_enable
            }
            Err(error) => {
                self.output_failed(&error)?;
                None
            }
        };
        let rpm = self.output.read_rpm().ok().flatten();
        self.check_stall(raw, rpm);
        Ok(self.snapshot(&tick, raw, pwm_enable, rpm))
    }

    /// Writes `raw` if it differs from what we wrote last, first taking the
    /// output back if something else changed it. Returns `pwmN_enable` as read.
    fn write_duty(&mut self, raw: u8) -> anyhow::Result<Option<u8>> {
        let current = self
            .manual
            .current()
            .with_context(|| format!("cannot read {}", self.label()))?;

        if self.output_errors > 0 {
            // After a failed tick we do not know what the output holds.
            self.manual
                .reassert(raw)
                .with_context(|| format!("cannot take back {}", self.label()))?;
            self.written = Some(raw);
            return Ok(current.enable);
        }
        if let Some(written) = self.written
            && !is_held(current, written)
        {
            self.report_interference(&format!("{current}"));
            self.manual
                .reassert(raw)
                .with_context(|| format!("cannot take back {}", self.label()))?;
            self.written = Some(raw);
            return Ok(current.enable);
        }
        if self.interfered {
            log::info(format!(
                "{}: {} is ours again",
                self.name,
                self.output.describe()
            ));
            self.interfered = false;
        }
        if self.written != Some(raw) {
            self.manual
                .set(raw)
                .with_context(|| format!("cannot write {}", self.label()))?;
            self.written = Some(raw);
        }
        Ok(current.enable)
    }

    /// E.g. `cpu_cooler: nct6798 pwm2`, for error messages.
    fn label(&self) -> String {
        format!("{}: {}", self.name, self.output.describe())
    }

    /// Counts a tick in which the output could not be read or written. The
    /// first one is logged; after [`MAX_OUTPUT_ERRORS`] in a row the daemon
    /// gives up, so the chip's own curve and `restore` take over.
    fn output_failed(&mut self, error: &anyhow::Error) -> anyhow::Result<()> {
        self.output_errors += 1;
        if self.output_errors == 1 {
            log::warning(format!("{error:#}; retrying every tick"));
        }
        if self.output_errors >= MAX_OUTPUT_ERRORS {
            bail!(
                "{}: {} failed {MAX_OUTPUT_ERRORS} ticks in a row; giving up so the chip's \
                 own curve takes over",
                self.name,
                self.output.describe()
            );
        }
        Ok(())
    }

    fn output_recovered(&mut self) {
        if self.output_errors > 0 {
            log::info(format!(
                "{}: {} works again",
                self.name,
                self.output.describe()
            ));
            self.output_errors = 0;
        }
    }

    /// Logs entering and leaving failsafe, not every failsafe tick.
    fn report_failsafe(&mut self, tick: &Tick) {
        match (&tick.failsafe, self.in_failsafe) {
            (Some(reason), false) => {
                log::warning(format!(
                    "{}: {reason}; running at {:.0} %",
                    self.name, tick.duty
                ));
                self.in_failsafe = true;
            }
            (None, true) => {
                log::info(format!(
                    "{}: sensors read normally again; back on the curve",
                    self.name
                ));
                self.in_failsafe = false;
            }
            _ => {}
        }
    }

    /// Logs the first tick of each episode in which another program (or a
    /// resume from suspend) changed the output.
    fn report_interference(&mut self, found: &str) {
        if !self.interfered {
            log::warning(format!(
                "{}: {} changed to {found} behind our back (resume from suspend, or another \
                 program such as CoolerControl); taking it back",
                self.name,
                self.output.describe()
            ));
            self.interfered = true;
        }
    }

    /// Warns once when the tachometer reads 0 rpm for [`STALL_TICKS`] ticks at
    /// a duty that should turn the fan.
    fn check_stall(&mut self, raw: u8, rpm: Option<u32>) {
        let should_spin = raw >= pwm::percent_to_raw(STALL_MIN_PERCENT);
        match rpm {
            Some(0) if should_spin => {
                self.stalled_ticks += 1;
                if self.stalled_ticks == STALL_TICKS {
                    log::warning(format!(
                        "{}: 0 rpm at {} % for {STALL_TICKS} ticks; the fan may be stalled or unplugged",
                        self.name,
                        pwm::raw_to_percent(raw)
                    ));
                }
            }
            _ => {
                if self.stalled_ticks >= STALL_TICKS {
                    log::info(format!("{}: spinning again", self.name));
                }
                self.stalled_ticks = 0;
            }
        }
    }

    fn snapshot(
        &self,
        tick: &Tick,
        raw: u8,
        pwm_enable: Option<u8>,
        rpm: Option<u32>,
    ) -> FanSnapshot {
        FanSnapshot {
            output: self.output.describe(),
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
            pwm_enable,
            rpm,
        }
    }
}

/// Writes the snapshot, logging a failure once rather than every tick:
/// controlling the fans matters more than publishing state.
fn write_state(state_file: &StateFile, snapshot: &Snapshot, failed: &mut bool) {
    match (state_file.write(snapshot), *failed) {
        (Ok(()), true) => {
            log::info("writing the state file works again");
            *failed = false;
        }
        (Err(error), false) => {
            log::error(format!(
                "cannot write {}: {error}",
                state_file.path().display()
            ));
            *failed = true;
        }
        // Still working, or still failing: nothing new to say.
        (Ok(()), false) | (Err(_), true) => {}
    }
}

/// Sleeps until `deadline`, waking early for a stop or reload signal. A
/// reload request is consumed. Pets the watchdog every [`WATCHDOG_PET`]
/// meanwhile, so a long `interval` cannot trip it; a hung tick still does.
fn wait_until(deadline: Instant, signals: &DaemonSignals, notifier: &Notifier) -> Wake {
    let mut last_pet = Instant::now();
    loop {
        if last_pet.elapsed() >= WATCHDOG_PET {
            notifier.watchdog();
            last_pet = Instant::now();
        }
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
        thread::sleep((deadline - now).min(SIGNAL_CHECK));
    }
}

fn hand_back_all(fans: Vec<FanLoop>) {
    for fan in fans {
        let describe = fan.output.describe();
        match fan.manual.hand_back() {
            Ok(()) => log::info(format!("{}: handed {describe} back to the chip", fan.name)),
            Err(error) => log::error(format!(
                "{}: could not hand back {describe}: {error}",
                fan.name
            )),
        }
    }
}

fn unix_millis() -> u64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since_epoch.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::fs;

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

    /// A fake PC with the CPU at 50 °C (-> 50 % -> raw 128) and pwm2 in SmartFan IV.
    fn fake_pc() -> (FakeSysfs, Setup) {
        let sysfs = FakeSysfs::new();
        sysfs
            .chip("hwmon3", "k10temp", "pci0000:00/0000:00:18.3")
            .attr("temp1_input", "50000")
            .attr("temp1_label", "Tctl");
        sysfs
            .chip("hwmon7", "nct6798", "platform/nct6775.656")
            .attr("pwm1", "74")
            .attr("pwm1_enable", "5")
            .attr("pwm2", "98")
            .attr("pwm2_enable", "5")
            .attr("fan2_input", "1036");
        let config = sysfs.hwmon_root().join("../../onlycurves.toml");
        fs::write(&config, CONFIG).unwrap();
        let setup = Setup::load(&config, &sysfs.hwmon_root()).unwrap();
        (sysfs, setup)
    }

    fn pwm2(sysfs: &FakeSysfs) -> (String, String) {
        let read = |file: &str| {
            fs::read_to_string(sysfs.hwmon_root().join("hwmon7").join(file))
                .unwrap()
                .trim()
                .to_owned()
        };
        (read("pwm2"), read("pwm2_enable"))
    }

    #[test]
    fn a_tick_writes_the_curve_duty_and_publishes_it() {
        let (sysfs, setup) = fake_pc();
        let mut fans = take_over_fans(&setup).unwrap();

        let snapshot = tick(&setup.bindings, &mut fans).unwrap();

        assert_eq!(pwm2(&sysfs), ("128".into(), "1".into()));
        let fan = &snapshot.fans["cpu_cooler"];
        assert_eq!(
            (fan.pwm_raw, fan.rpm, fan.mode),
            (128, Some(1036), FanMode::Normal)
        );
    }

    #[test]
    fn an_output_switched_away_is_taken_back() {
        let (sysfs, setup) = fake_pc();
        let mut fans = take_over_fans(&setup).unwrap();
        tick(&setup.bindings, &mut fans).unwrap();

        // What a resume from suspend can do: the BIOS takes the output back.
        fans[0].output.chip().write_pwm_enable(2, 5).unwrap();
        tick(&setup.bindings, &mut fans).unwrap();

        assert_eq!(pwm2(&sysfs), ("128".into(), "1".into()));
        assert!(fans[0].interfered);
    }

    #[test]
    fn hand_back_returns_outputs_to_the_chip() {
        let (sysfs, setup) = fake_pc();
        let mut fans = take_over_fans(&setup).unwrap();
        tick(&setup.bindings, &mut fans).unwrap();

        hand_back_all(fans);

        assert_eq!(pwm2(&sysfs).1, "5");
    }

    #[test]
    fn an_output_error_is_retried_and_only_fatal_when_it_lasts() {
        use std::os::unix::fs::PermissionsExt;

        let (sysfs, setup) = fake_pc();
        let mut fans = take_over_fans(&setup).unwrap();
        tick(&setup.bindings, &mut fans).unwrap();
        let pwm2_file = sysfs.hwmon_root().join("hwmon7/pwm2");
        let set_mode =
            |mode| fs::set_permissions(&pwm2_file, fs::Permissions::from_mode(mode)).unwrap();

        // A curve change forces a write, which now fails.
        set_mode(0o444);
        fans[0].written = Some(0);
        let snapshot = tick(&setup.bindings, &mut fans).unwrap();
        assert_eq!(fans[0].output_errors, 1);
        assert_eq!(snapshot.fans["cpu_cooler"].pwm_enable, None);

        set_mode(0o644);
        tick(&setup.bindings, &mut fans).unwrap();
        assert_eq!(fans[0].output_errors, 0);
        assert_eq!(pwm2(&sysfs), ("128".into(), "1".into()));

        set_mode(0o444);
        for _ in 1..MAX_OUTPUT_ERRORS {
            fans[0].written = Some(0);
            tick(&setup.bindings, &mut fans).unwrap();
        }
        fans[0].written = Some(0);
        assert!(tick(&setup.bindings, &mut fans).is_err());
    }

    #[test]
    fn a_stall_counts_only_at_a_duty_that_should_spin() {
        let (_sysfs, setup) = fake_pc();
        let mut fans = take_over_fans(&setup).unwrap();
        let fan = &mut fans[0];

        fan.check_stall(pwm::percent_to_raw(20.0), Some(0));
        assert_eq!(fan.stalled_ticks, 0);

        for _ in 0..STALL_TICKS {
            fan.check_stall(pwm::percent_to_raw(50.0), Some(0));
        }
        assert_eq!(fan.stalled_ticks, STALL_TICKS);

        fan.check_stall(pwm::percent_to_raw(50.0), Some(800));
        assert_eq!(fan.stalled_ticks, 0);
    }

    #[test]
    fn wait_until_wakes_for_signals_and_deadlines() {
        let far = Instant::now() + Duration::from_secs(60);
        let signals = DaemonSignals::unregistered();

        signals.reload.store(true, Ordering::Relaxed);
        let notifier = Notifier::disabled();
        assert_eq!(wait_until(far, &signals, &notifier), Wake::Reload);
        assert!(
            !signals.reload.load(Ordering::Relaxed),
            "the request is consumed"
        );

        assert_eq!(wait_until(Instant::now(), &signals, &notifier), Wake::Tick);

        signals.stop.store(true, Ordering::Relaxed);
        assert_eq!(wait_until(far, &signals, &notifier), Wake::Stop);
    }

    /// Runs `test` on a daemon for the fake PC's `CONFIG`, after one tick.
    fn with_daemon(test: impl FnOnce(&FakeSysfs, &mut Daemon)) {
        let (sysfs, setup) = fake_pc();
        let shm = tempfile::tempdir_in("/dev/shm").unwrap();
        let config = sysfs.hwmon_root().join("../../onlycurves.toml");
        let hwmon_root = sysfs.hwmon_root();
        let paths = Paths {
            config: &config,
            hwmon_root: &hwmon_root,
            state_dir: shm.path(),
        };
        let notifier = Notifier::disabled();
        let mut daemon = Daemon {
            paths: &paths,
            fans: take_over_fans(&setup).unwrap(),
            setup,
            state_file: StateFile::prepare(shm.path()).unwrap(),
            record: OutputRecord::in_dir(shm.path()),
            notifier: &notifier,
            state_write_failed: false,
            status_note: None,
        };
        daemon.tick_and_publish().unwrap();
        test(&sysfs, &mut daemon);
    }

    fn attribute(sysfs: &FakeSysfs, file: &str) -> String {
        fs::read_to_string(sysfs.hwmon_root().join("hwmon7").join(file))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn rewrite_config(sysfs: &FakeSysfs, text: &str) {
        fs::write(sysfs.hwmon_root().join("../../onlycurves.toml"), text).unwrap();
    }

    #[test]
    fn reload_applies_a_new_curve_without_letting_go() {
        with_daemon(|sysfs, daemon| {
            rewrite_config(
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
            rewrite_config(
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
            rewrite_config(
                sysfs,
                &CONFIG.replace("[[40, 40], [60, 60]]", "[[60, 40], [40, 60]]"),
            );

            daemon.reload();

            assert_eq!(fan_names(&daemon.fans), "cpu_cooler");
            assert_eq!(attribute(sysfs, "pwm2_enable"), "1");
            assert_eq!(daemon.status_note, Some(RELOAD_FAILED));
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
            "cpu_cooler 48 % (cpu 49.1 °C), exhaust 100 % (failsafe)"
        );
    }
}
