//! The `sweep` command: step one fan from 100 % down to 0 and back up, and
//! print the speed it settles at for each duty.
//!
//! It measures the two numbers a fan's `min_duty` depends on: the duty at
//! which the fan stops on the way down, and the higher duty it needs to start
//! again on the way up.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use svalka_core::pwm;

use crate::binding::{BoundOutput, BoundSensor};
use crate::hwmon::{HandBack, ManualOverride, PwmState};
use crate::record::OutputRecord;
use crate::run::Paths;
use crate::setup::Setup;

/// Any of the fan's sources reaching this ends the sweep: a slowed or stopped
/// fan must not cook what it cools.
const MAX_CELSIUS: f64 = 80.0;

/// Readings per step at most. A fan still drifting after that is recorded as
/// it reads then.
const MAX_SAMPLES: u32 = 15;

/// How often a wait checks for Ctrl-C.
const TICK: Duration = Duration::from_millis(100);

/// Which fan to sweep and how.
pub struct Options<'a> {
    /// A fan name from the config, e.g. `exhaust`.
    pub fan: &'a str,
    /// Percent between steps, e.g. 5.
    pub step: u8,
    /// Time between tachometer readings. The nct6775 driver refreshes its
    /// readings every 1.5–2 s; reading faster returns the same value twice,
    /// which would look like a settled fan.
    pub sample: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Down,
    Up,
}

/// The speed one duty settled at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Point {
    direction: Direction,
    percent: u8,
    rpm: u32,
}

/// Why the sweep ended before its last step.
#[derive(Debug, Clone, PartialEq)]
enum Interrupt {
    /// Ctrl-C or SIGTERM.
    Stopped,
    /// Something else wrote to the output, which now reads like this.
    Interfered(PwmState),
    /// A source got too hot or could not be read, e.g. `cpu reads 81.0 °C`.
    Unsafe(String),
}

/// Sweeps the fan named in `options` and prints its speed at every step, then
/// restores the output and says where the fan stops and starts.
///
/// `stop` is set by a signal handler; the sweep ends early when it is.
///
/// # Errors
///
/// Fails when the config does not load, the fan is unknown, has no
/// tachometer or is held by `svalka run`, writing needs root, the sweep is
/// interrupted, or printing fails. In every case after the takeover, the
/// output is restored first.
pub fn run(
    paths: &Paths,
    options: &Options,
    stop: &AtomicBool,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let mut setup = Setup::load(paths.config, paths.hwmon_root)?;
    let output = find_output(&setup, options.fan)?.clone();
    refuse_if_held_by_run(&output, paths.state_dir)?;
    if output.read_rpm()?.is_none() {
        bail!(
            "{} has no tachometer fan{}, so sweep cannot see the fan's speed",
            output.describe(),
            output.pwm()
        );
    }
    let mut sources = fan_sources(&mut setup, options.fan);

    let manual = output.take_over(HandBack::Saved)?;
    writeln!(
        out,
        "Sweeping {} ({}) from 100 % to 0 and back in {} % steps. Ctrl-C stops it and \
         restores the output.\n",
        options.fan,
        output.describe(),
        options.step
    )?;
    let mut points = Vec::new();
    let result = sweep(
        &manual,
        &output,
        &mut sources,
        options,
        stop,
        &mut points,
        out,
    );
    let saved = manual.saved();
    manual
        .hand_back()
        .with_context(|| format!("could not restore {}", output.describe()))?;
    writeln!(out, "\nRestored {} to {saved}.", output.describe())?;

    match result? {
        None => writeln!(out, "{}", verdict(options.fan, &points, options.step))?,
        Some(Interrupt::Stopped) => bail!("stopped before the sweep was done"),
        Some(Interrupt::Interfered(found)) => bail!(
            "{} changed to {found} during the sweep, so another program wrote to it. \
             Stop any other fan-control program and try again.",
            output.describe()
        ),
        Some(Interrupt::Unsafe(reason)) => bail!("{reason}, so the sweep stopped early"),
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

/// A running daemon would take the output back within one readback, so the
/// sweep would only measure the fight between the two.
fn refuse_if_held_by_run(output: &BoundOutput, state_dir: &Path) -> anyhow::Result<()> {
    let record = OutputRecord::in_dir(state_dir);
    let held = record
        .read()
        .with_context(|| format!("cannot read {}", record.path().display()))?
        .unwrap_or_default();
    if held
        .iter()
        .any(|held| held.chip_dir == output.chip().dir() && held.pwm == output.pwm())
    {
        bail!(
            "svalka run is controlling {}. Stop it first (sudo systemctl stop svalka), \
             and start it again after the sweep.",
            output.describe()
        );
    }
    Ok(())
}

/// The fan's sources by name, taken out of `setup`: the sweep watches them.
fn fan_sources(setup: &mut Setup, fan: &str) -> BTreeMap<String, BoundSensor> {
    // `bind` succeeded, so every source is a bound sensor.
    setup.config.fans[fan]
        .sources
        .iter()
        .filter_map(|name| Some((name.clone(), setup.bindings.sensors.remove(name)?)))
        .collect()
}

/// Steps through [`duties`], recording into `points` the speed each settles
/// at and printing it. Returns why it ended early, if it did.
fn sweep(
    manual: &ManualOverride,
    output: &BoundOutput,
    sources: &mut BTreeMap<String, BoundSensor>,
    options: &Options,
    stop: &AtomicBool,
    points: &mut Vec<Point>,
    out: &mut impl Write,
) -> anyhow::Result<Option<Interrupt>> {
    for (direction, percent) in duties(options.step) {
        let raw = pwm::percent_to_raw(f64::from(percent));
        manual
            .set(raw)
            .with_context(|| format!("cannot write {}", output.describe()))?;
        let mut previous = None;
        for _ in 0..MAX_SAMPLES {
            if let Some(interrupt) = next_sample(manual, raw, sources, options.sample, stop)? {
                return Ok(Some(interrupt));
            }
            let rpm = output.read_rpm()?.unwrap_or(0);
            let done = previous.is_some_and(|previous| settled(previous, rpm));
            previous = Some(rpm);
            if done {
                break;
            }
        }
        let point = Point {
            direction,
            percent,
            rpm: previous.unwrap_or(0),
        };
        print_point(out, point)?;
        points.push(point);
    }
    Ok(None)
}

/// 100 % down to 0 in `step`s, then back up to 100 %. 0 and 100 are always
/// included, even when `step` does not divide 100.
fn duties(step: u8) -> Vec<(Direction, u8)> {
    let mut down: Vec<u8> = (0..=100u8)
        .rev()
        .step_by(usize::from(step.max(1)))
        .collect();
    if down.last() != Some(&0) {
        down.push(0);
    }
    let up = down
        .iter()
        .rev()
        .skip(1)
        .map(|&percent| (Direction::Up, percent));
    down.iter()
        .map(|&percent| (Direction::Down, percent))
        .chain(up)
        .collect()
}

/// Whether two readings in a row are close enough to call the fan settled:
/// within 3 % or 30 rpm, whichever is larger, since a steady fan's
/// tachometer still wobbles by 10–20 rpm.
fn settled(previous: u32, rpm: u32) -> bool {
    previous.abs_diff(rpm) <= (previous / 33).max(30)
}

/// Waits for the next reading, then checks that the sweep may go on: not
/// stopped, the output still ours, every source readable and cool enough.
fn next_sample(
    manual: &ManualOverride,
    raw: u8,
    sources: &mut BTreeMap<String, BoundSensor>,
    sample: Duration,
    stop: &AtomicBool,
) -> io::Result<Option<Interrupt>> {
    if let Some(interrupt) = wait(sample, stop) {
        return Ok(Some(interrupt));
    }
    if let Some(interrupt) = interference(manual, raw)? {
        return Ok(Some(interrupt));
    }
    Ok(unsafe_source(sources))
}

/// Sleeps `duration` in short ticks; `Some` if stopped meanwhile.
fn wait(duration: Duration, stop: &AtomicBool) -> Option<Interrupt> {
    let mut left = duration;
    while !left.is_zero() {
        if stop.load(Ordering::Relaxed) {
            return Some(Interrupt::Stopped);
        }
        let nap = left.min(TICK);
        thread::sleep(nap);
        left -= nap;
    }
    stop.load(Ordering::Relaxed).then_some(Interrupt::Stopped)
}

/// `Some` if the output no longer holds what we wrote.
fn interference(manual: &ManualOverride, raw: u8) -> io::Result<Option<Interrupt>> {
    let state = manual.current()?;
    Ok((!state.is_held_at(raw)).then_some(Interrupt::Interfered(state)))
}

/// `Some` if a source is too hot or cannot be read.
fn unsafe_source(sources: &mut BTreeMap<String, BoundSensor>) -> Option<Interrupt> {
    sources
        .iter_mut()
        .find_map(|(name, sensor)| match sensor.read_or_find_again() {
            Ok(celsius) if celsius >= MAX_CELSIUS => {
                Some(Interrupt::Unsafe(format!("{name} reads {celsius:.1} °C")))
            }
            Ok(_) => None,
            Err(error) => Some(Interrupt::Unsafe(format!("{name} cannot be read: {error}"))),
        })
}

/// E.g. `down   35 %    812 rpm`.
fn print_point(out: &mut impl Write, point: Point) -> io::Result<()> {
    let direction = match point.direction {
        Direction::Down => "down",
        Direction::Up => "up",
    };
    writeln!(
        out,
        "{direction:<5}{:>4} %{:>7} rpm",
        point.percent, point.rpm
    )
}

/// The first duty on the way down that reads 0 rpm, and the first on the way
/// up that reads more.
fn stop_and_start(points: &[Point]) -> (Option<u8>, Option<u8>) {
    let first = |direction, stopped: bool| {
        points
            .iter()
            .find(|point| point.direction == direction && (point.rpm == 0) == stopped)
            .map(|point| point.percent)
    };
    (first(Direction::Down, true), first(Direction::Up, false))
}

/// Where the fan stops and starts, and the `min_duty` that keeps it running.
fn verdict(fan: &str, points: &[Point], step: u8) -> String {
    match stop_and_start(points) {
        (None, _) => format!(
            "{fan} kept spinning even at 0 %: this header never stops it, so it needs no min_duty."
        ),
        (Some(stopped), None) => format!(
            "{fan} stopped at {stopped} % on the way down and did not start again on the way up. \
             Check the fan and its cable."
        ),
        (Some(stopped), Some(started)) => format!(
            "{fan} stops at {stopped} % on the way down and starts again at {started} % on the \
             way up.\nSuggested: min_duty = {} for fan.{fan}, one step above the start duty.",
            started.saturating_add(step).min(100)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwmon::testing::FakeSysfs;
    use crate::hwmon::{Chip, PwmEnable};

    fn point(direction: Direction, percent: u8, rpm: u32) -> Point {
        Point {
            direction,
            percent,
            rpm,
        }
    }

    /// The exhaust fan as the 2026-09-14 calibration measured it: stops below 6 %,
    /// starts at 10 %.
    fn exhaust_points() -> Vec<Point> {
        vec![
            point(Direction::Down, 100, 1849),
            point(Direction::Down, 10, 410),
            point(Direction::Down, 5, 0),
            point(Direction::Down, 0, 0),
            point(Direction::Up, 5, 0),
            point(Direction::Up, 10, 395),
            point(Direction::Up, 100, 1851),
        ]
    }

    #[test]
    fn duties_go_down_to_zero_and_back_up() {
        let percents = |step| -> Vec<u8> { duties(step).into_iter().map(|(_, p)| p).collect() };

        assert_eq!(percents(25), [100, 75, 50, 25, 0, 25, 50, 75, 100]);
        assert_eq!(percents(30), [100, 70, 40, 10, 0, 10, 40, 70, 100]);
        assert_eq!(duties(5).len(), 41);
        assert_eq!(duties(25)[4], (Direction::Down, 0));
        assert_eq!(duties(25)[5], (Direction::Up, 25));
    }

    #[test]
    fn settled_allows_a_wobble_but_not_a_coasting_fan() {
        assert!(settled(1849, 1830));
        assert!(settled(0, 0));
        assert!(settled(400, 425));
        assert!(!settled(1849, 1500));
        assert!(!settled(400, 0));
    }

    #[test]
    fn verdict_suggests_one_step_above_the_start_duty() {
        assert_eq!(
            verdict("exhaust", &exhaust_points(), 5),
            "exhaust stops at 5 % on the way down and starts again at 10 % on the way up.\n\
             Suggested: min_duty = 15 for fan.exhaust, one step above the start duty."
        );
    }

    #[test]
    fn verdict_for_a_fan_that_never_stops_or_never_restarts() {
        let never_stops = [point(Direction::Down, 0, 380), point(Direction::Up, 5, 400)];
        assert!(verdict("cpu_cooler", &never_stops, 5).contains("kept spinning even at 0 %"));

        let never_restarts = [point(Direction::Down, 0, 0), point(Direction::Up, 100, 0)];
        assert!(verdict("cpu_cooler", &never_restarts, 5).contains("did not start again"));
    }

    #[test]
    fn wait_ends_at_once_when_stopped() {
        assert_eq!(
            wait(Duration::from_secs(5), &AtomicBool::new(true)),
            Some(Interrupt::Stopped)
        );
        assert_eq!(
            wait(Duration::from_millis(1), &AtomicBool::new(false)),
            None
        );
    }

    #[test]
    fn interference_notices_another_writer() {
        let sysfs = FakeSysfs::new();
        let fake = sysfs.chip("hwmon7", "nct6798", "platform/nct6775.656");
        fake.attr("pwm1", "74").attr("pwm1_enable", "5");
        let chip = Chip::open(fake.dir()).unwrap();
        let manual = ManualOverride::take(&chip, 1, HandBack::Saved).unwrap();
        manual.set(128).unwrap();
        assert_eq!(interference(&manual, 128).unwrap(), None);

        chip.write_pwm_enable(1, PwmEnable::Automatic(5)).unwrap();
        assert!(matches!(
            interference(&manual, 128).unwrap(),
            Some(Interrupt::Interfered(_))
        ));
    }
}
