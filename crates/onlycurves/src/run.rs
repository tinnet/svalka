//! The `run` command: the control loop.
//!
//! Startup takes every configured fan output into manual mode. Each tick reads
//! every sensor once, lets each fan's [`FanController`] pick a duty, writes it
//! when it changed, and writes the state snapshot. On exit (SIGINT, SIGTERM,
//! SIGHUP or an error) every output goes back to the chip's own automatic mode,
//! so the BIOS curve runs the fans.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow};
use onlycurves_core::controller::{FanController, Tick};
use onlycurves_core::pwm;
use onlycurves_core::state::{FanMode, FanSnapshot, Snapshot, VERSION};

use crate::binding::{Bindings, BoundOutput};
use crate::hwmon::{HandBack, ManualOverride, automatic_mode, is_held};
use crate::log;
use crate::setup::Setup;
use crate::statefile::StateFile;

/// At or above this duty, a tachometer reading 0 rpm means a stalled fan.
const STALL_MIN_PERCENT: f64 = 30.0;

/// Ticks of 0 rpm before a stall is reported. The nct6775 driver refreshes
/// rpm only every 1.5–2 s, and a fan needs a moment to spin up.
const STALL_TICKS: u32 = 5;

/// How often a sleeping loop checks for a stop signal.
const STOP_CHECK: Duration = Duration::from_millis(100);

/// Runs the control loop until `stop` is set or an error occurs, then hands
/// every output back to the chip.
///
/// # Errors
///
/// Fails when the config does not load, the state directory is unusable, an
/// output cannot be taken over, or writing a duty fails. Outputs already
/// taken over are handed back first.
pub fn run(
    config_path: &Path,
    hwmon_root: &Path,
    state_dir: &Path,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    let setup = Setup::load(config_path, hwmon_root)?;
    let state_file = StateFile::prepare(state_dir)?;
    let mut fans = take_over_fans(&setup)?;
    log::info(format!(
        "controlling {} every {:?}; state in {}",
        fans.iter()
            .map(|fan| fan.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        setup.config.interval,
        state_file.path().display()
    ));

    let result = control_loop(&setup, &mut fans, &state_file, stop);
    hand_back_all(fans);
    result
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
}

fn take_over_fans(setup: &Setup) -> anyhow::Result<Vec<FanLoop>> {
    let mut fans = Vec::new();
    for (name, fan) in &setup.config.fans {
        // `bind` succeeded, so every fan has an output.
        let output = setup.bindings.outputs[name].clone();
        let manual = take_over(&output)?;
        log::info(format!(
            "{name}: took over {} (was {}); it returns to the chip's own curve on exit",
            output.describe(),
            manual.saved()
        ));
        fans.push(FanLoop {
            name: name.clone(),
            controller: FanController::new(
                fan.clone(),
                setup.config.interval,
                setup.config.failsafe_duty,
            ),
            output,
            manual,
            written: None,
            in_failsafe: false,
            interfered: false,
            stalled_ticks: 0,
        });
    }
    Ok(fans)
}

/// Switches `output` to manual, to be handed back to the chip's automatic mode.
fn take_over(output: &BoundOutput) -> anyhow::Result<ManualOverride> {
    let mode = automatic_mode(output.chip()).ok_or_else(|| {
        anyhow!(
            "{}: onlycurves does not know this chip's automatic mode, so it could not \
             hand the fan back on exit; only nct6775-family chips are supported",
            output.describe()
        )
    })?;
    ManualOverride::take(output.chip(), output.pwm(), HandBack::Automatic(mode)).map_err(|error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            anyhow!(
                "writing {} needs root; run this with sudo",
                output.describe()
            )
        } else {
            anyhow!(error).context(format!("cannot take over {}", output.describe()))
        }
    })
}

fn control_loop(
    setup: &Setup,
    fans: &mut [FanLoop],
    state_file: &StateFile,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    let interval = setup.config.interval;
    let mut state_write_failed = false;
    let mut next = Instant::now();
    loop {
        let snapshot = tick(&setup.bindings, fans)?;
        write_state(state_file, &snapshot, &mut state_write_failed);

        next += interval;
        if !sleep_until(next, stop) {
            log::info("stopping");
            return Ok(());
        }
        // After a long stall (e.g. a debugger), restart the schedule instead of
        // running the missed ticks back to back.
        if Instant::now() > next + interval {
            next = Instant::now();
        }
    }
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

/// Every sensor's temperature, or `None` when it cannot be read.
fn read_sensors(bindings: &Bindings) -> BTreeMap<String, Option<f64>> {
    bindings
        .sensors
        .iter()
        .map(|(name, sensor)| (name.clone(), sensor.read_celsius().ok()))
        .collect()
}

impl FanLoop {
    fn tick(&mut self, sensors: &BTreeMap<String, Option<f64>>) -> anyhow::Result<FanSnapshot> {
        let tick = self.controller.tick(sensors);
        self.report_failsafe(&tick);
        let raw = pwm::percent_to_raw(tick.duty);
        let pwm_enable = self.write_duty(raw)?;
        let rpm = self.output.read_rpm().ok().flatten();
        self.check_stall(raw, rpm);
        Ok(self.snapshot(&tick, raw, pwm_enable, rpm))
    }

    /// Writes `raw` if it differs from what we wrote last, first taking the
    /// output back if something else changed it. Returns `pwmN_enable` as read.
    fn write_duty(&mut self, raw: u8) -> anyhow::Result<Option<u8>> {
        let label = format!("{}: {}", self.name, self.output.describe());
        let current = self
            .manual
            .current()
            .with_context(|| format!("cannot read {label}"))?;

        if let Some(written) = self.written
            && !is_held(current, written)
        {
            self.report_interference(&format!("{current}"));
            self.manual
                .reassert(raw)
                .with_context(|| format!("cannot take back {label}"))?;
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
                .with_context(|| format!("cannot write {label}"))?;
            self.written = Some(raw);
        }
        Ok(current.enable)
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

/// Sleeps until `deadline`, waking early when `stop` is set. Returns `false`
/// when stopped.
fn sleep_until(deadline: Instant, stop: &AtomicBool) -> bool {
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        thread::sleep((deadline - now).min(STOP_CHECK));
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
    fn sleep_until_returns_false_when_stopped() {
        let far = Instant::now() + Duration::from_secs(60);

        assert!(!sleep_until(far, &AtomicBool::new(true)));
        assert!(sleep_until(Instant::now(), &AtomicBool::new(false)));
    }
}
