//! The `identify` command: run one fan at 100 % for a few seconds, then put
//! its output back exactly as it was.
//!
//! It answers two questions the config takes on trust: which physical fan a
//! `pwmN` drives (watch or listen), and which tachometer `fanN` reports that
//! fan's speed (the one that rises).

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};

use crate::binding::BoundOutput;
use crate::hwmon::{Chip, HandBack, ManualOverride, PwmState};
use crate::lend;
use crate::run::Paths;
use crate::setup::Setup;

/// Full duty.
const FULL: u8 = 255;

/// How often the hold checks for Ctrl-C and for other programs writing.
const TICK: Duration = Duration::from_millis(100);

/// A tachometer must rise at least this much to count as the fan we spun up.
const MIN_RISE_RPM: u32 = 200;

/// What to identify and for how long.
pub struct Options<'a> {
    /// A fan name from the config, e.g. `exhaust`.
    pub fan: &'a str,
    pub seconds: u64,
}

/// One tachometer's speed before the test and its highest speed during it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RpmChange {
    label: String,
    before: Option<u32>,
    peak: Option<u32>,
}

/// Why the hold ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    /// The full time passed.
    Finished,
    /// Ctrl-C or SIGTERM.
    Stopped,
    /// Something else wrote to the output, which now reads like this.
    Interfered(PwmState),
}

/// Runs the fan named in `options` at full speed, printing every tachometer
/// on its chip once a second, then restores the output.
///
/// `stop` is set by a signal handler; the hold ends early when it is.
///
/// # Errors
///
/// Fails when the config does not load, the fan is unknown, `svalka run`
/// holds it and does not lend it, writing needs root, the hold is stopped or
/// interfered with, or printing fails. In every case after the takeover, the
/// output is restored first; a lent output then goes back to `svalka run`.
pub fn run(
    paths: &Paths,
    options: &Options,
    stop: &AtomicBool,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let setup = Setup::load(paths.config, paths.hwmon_root)?;
    let output = find_output(&setup, options.fan)?;
    let chip = output.chip();
    // Held until the end, after the output is restored below.
    let loan = lend::borrow(
        output,
        paths.state_dir,
        "identify",
        lend::wait_for_daemon(setup.config.interval),
        stop,
    )?;
    if loan.from_daemon() {
        writeln!(
            out,
            "svalka run lent {} for the test and takes it back afterwards.",
            options.fan
        )?;
    }

    // Speeds before the takeover, printed only once it worked.
    let mut changes = start_rpm_changes(chip)?;
    let manual = take_over(output)?;

    writeln!(
        out,
        "Running {} ({}) at 100 % for {} s. Watch or listen for the fan that speeds up.\n",
        options.fan,
        output.describe(),
        options.seconds
    )?;
    print_rpm_header(out, &changes)?;
    print_rpm_row(out, "before", changes.iter().map(|change| change.before))?;
    let hold = hold_full_speed(&manual, chip, options.seconds, stop, &mut changes, out)?;
    let saved = manual.saved();
    manual
        .hand_back()
        .with_context(|| format!("could not restore {}", output.describe()))?;
    writeln!(out, "\nRestored {} to {saved}.", output.describe())?;

    match hold {
        Hold::Finished => writeln!(out, "{}", verdict(&changes, output.pwm()))?,
        Hold::Stopped => bail!("stopped before the time was up"),
        Hold::Interfered(found) => bail!(
            "{} changed to {found} during the test, so another program wrote to it. \
             Stop any other fan-control program and try again.",
            output.describe()
        ),
    }
    Ok(())
}

fn find_output<'a>(setup: &'a Setup, fan: &str) -> anyhow::Result<&'a BoundOutput> {
    setup.bindings.outputs.get(fan).ok_or_else(|| {
        let names: Vec<&str> = setup.bindings.outputs.keys().map(String::as_str).collect();
        anyhow!(
            "no fan named {fan:?} in the config; it has: {}",
            names.join(", ")
        )
    })
}

/// Switches the output to manual at full duty.
fn take_over(output: &BoundOutput) -> anyhow::Result<ManualOverride> {
    let manual = output.take_over(HandBack::Saved)?;
    manual
        .set(FULL)
        .with_context(|| format!("cannot write {}", output.describe()))?;
    Ok(manual)
}

/// Holds full duty for `seconds`, printing the tachometers once a second and
/// recording their peaks in `changes`.
fn hold_full_speed(
    manual: &ManualOverride,
    chip: &Chip,
    seconds: u64,
    stop: &AtomicBool,
    changes: &mut [RpmChange],
    out: &mut impl Write,
) -> anyhow::Result<Hold> {
    for second in 1..=seconds {
        let hold = wait_one_second(manual, stop)?;
        if hold != Hold::Finished {
            return Ok(hold);
        }
        let rpms = read_rpms(chip)?;
        record_peaks(changes, &rpms);
        print_rpm_row(out, &format!("{second} s"), rpms.into_iter())?;
    }
    Ok(Hold::Finished)
}

/// Waits one second in short ticks, ending early on `stop` or when another
/// program changes the output.
fn wait_one_second(manual: &ManualOverride, stop: &AtomicBool) -> io::Result<Hold> {
    let ticks = Duration::from_secs(1).as_millis() / TICK.as_millis();
    for _ in 0..ticks {
        if stop.load(Ordering::Relaxed) {
            return Ok(Hold::Stopped);
        }
        thread::sleep(TICK);
    }
    let state = manual.current()?;
    if state.is_held_at(FULL) {
        Ok(Hold::Finished)
    } else {
        Ok(Hold::Interfered(state))
    }
}

fn start_rpm_changes(chip: &Chip) -> io::Result<Vec<RpmChange>> {
    let fans = chip.read_fans()?;
    Ok(fans
        .into_iter()
        .map(|fan| RpmChange {
            label: fan.label,
            before: fan.rpm,
            peak: fan.rpm,
        })
        .collect())
}

fn read_rpms(chip: &Chip) -> io::Result<Vec<Option<u32>>> {
    Ok(chip.read_fans()?.into_iter().map(|fan| fan.rpm).collect())
}

fn record_peaks(changes: &mut [RpmChange], rpms: &[Option<u32>]) {
    for (change, &rpm) in changes.iter_mut().zip(rpms) {
        change.peak = change.peak.max(rpm);
    }
}

/// E.g. `          fan1    fan2    fan5`.
fn print_rpm_header(out: &mut impl Write, changes: &[RpmChange]) -> io::Result<()> {
    write!(out, "{:<8}", "")?;
    for change in changes {
        write!(out, "{:>8}", change.label)?;
    }
    writeln!(out)
}

/// E.g. `1 s          1840    1109       0 rpm`.
fn print_rpm_row(
    out: &mut impl Write,
    title: &str,
    rpms: impl Iterator<Item = Option<u32>>,
) -> io::Result<()> {
    write!(out, "{title:<8}")?;
    for rpm in rpms {
        match rpm {
            Some(rpm) => write!(out, "{rpm:>8}")?,
            None => write!(out, "{:>8}", "?")?,
        }
    }
    writeln!(out, " rpm")
}

/// Which tachometer rose the most, if any rose enough to count.
#[must_use]
fn verdict(changes: &[RpmChange], pwm: u32) -> String {
    let biggest = changes
        .iter()
        .filter_map(|change| Some((change, change.before?, change.peak?)))
        .max_by_key(|&(_, before, peak)| peak.saturating_sub(before));
    match biggest {
        Some((change, before, peak)) if peak.saturating_sub(before) >= MIN_RISE_RPM => format!(
            "{} rose from {before} to {peak} rpm: pwm{pwm} drives the fan that {} reports.",
            change.label, change.label
        ),
        _ => format!(
            "No tachometer rose by {MIN_RISE_RPM} rpm or more. The fan may have no tachometer \
             wire, report to another chip, or already have been at full speed."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwmon::PwmEnable;
    use crate::hwmon::testing::FakeSysfs;

    fn change(label: &str, before: u32, peak: u32) -> RpmChange {
        RpmChange {
            label: label.into(),
            before: Some(before),
            peak: Some(peak),
        }
    }

    #[test]
    fn verdict_names_the_tachometer_that_rose_most() {
        let changes = [
            change("fan1", 737, 1890),
            change("fan2", 1109, 1130),
            change("fan5", 0, 0),
        ];

        assert_eq!(
            verdict(&changes, 1),
            "fan1 rose from 737 to 1890 rpm: pwm1 drives the fan that fan1 reports."
        );
    }

    #[test]
    fn verdict_ignores_small_wobbles() {
        let changes = [change("fan1", 737, 790), change("fan2", 1109, 1100)];

        assert!(verdict(&changes, 1).starts_with("No tachometer rose by 200 rpm"));
    }

    #[test]
    fn record_peaks_keeps_the_highest_reading() {
        let mut changes = [change("fan1", 700, 700)];

        record_peaks(&mut changes, &[Some(1500)]);
        record_peaks(&mut changes, &[Some(1400)]);

        assert_eq!(changes[0].peak, Some(1500));
    }

    fn manual_at_full(sysfs: &FakeSysfs) -> (Chip, ManualOverride) {
        let fake = sysfs.chip("hwmon7", "nct6798", "platform/nct6775.656");
        fake.attr("pwm1", "74").attr("pwm1_enable", "1");
        let chip = Chip::open(fake.dir()).unwrap();
        let manual = ManualOverride::take(&chip, 1, HandBack::Saved).unwrap();
        manual.set(FULL).unwrap();
        (chip, manual)
    }

    #[test]
    fn wait_ends_at_once_when_stopped() {
        let sysfs = FakeSysfs::new();
        let (_chip, manual) = manual_at_full(&sysfs);

        assert_eq!(
            wait_one_second(&manual, &AtomicBool::new(true)).unwrap(),
            Hold::Stopped
        );
    }

    #[test]
    fn wait_accepts_the_driver_reporting_full_speed_at_255() {
        let sysfs = FakeSysfs::new();
        let (chip, manual) = manual_at_full(&sysfs);
        chip.write_pwm_enable(1, PwmEnable::FullSpeed).unwrap(); // what nct6775 reports for manual at 255

        assert_eq!(
            wait_one_second(&manual, &AtomicBool::new(false)).unwrap(),
            Hold::Finished
        );
    }

    #[test]
    fn wait_notices_another_writer() {
        let sysfs = FakeSysfs::new();
        let (chip, manual) = manual_at_full(&sysfs);
        chip.write_pwm(1, 120).unwrap();

        assert_eq!(
            wait_one_second(&manual, &AtomicBool::new(false)).unwrap(),
            Hold::Interfered(PwmState {
                raw: 120,
                enable: Some(PwmEnable::Manual)
            })
        );
    }
}
