//! The per-fan control logic of one loop tick, without any I/O.
//!
//! Each tick, for each fan (see the design doc's Architecture section):
//! 1. take the hottest of the fan's source temperatures,
//! 2. smooth it with an exponential moving average,
//! 3. apply hysteresis: while it falls, ignore drops smaller than `hysteresis`,
//! 4. map it through the curve, never below `min_duty`.
//!
//! If a source cannot be read, or jumps by more than [`MAX_JUMP_CELSIUS`] in
//! one tick, the fan runs at the failsafe duty for that tick instead.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use crate::config::Fan;

/// A larger change between two ticks is treated as a bad reading, not heat.
pub const MAX_JUMP_CELSIUS: f64 = 20.0;

/// Why a fan is at its failsafe duty this tick.
#[derive(Debug, Clone, PartialEq)]
pub enum Failsafe {
    /// The source could not be read.
    Unreadable { source: String },
    /// The source changed by more than [`MAX_JUMP_CELSIUS`] since the last tick.
    Jump { source: String, from: f64, to: f64 },
    /// The fan has no sources. Config validation prevents this.
    NoSources,
}

impl fmt::Display for Failsafe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable { source } => write!(f, "sensor {source} cannot be read"),
            Self::Jump { source, from, to } => write!(
                f,
                "sensor {source} jumped from {from:.1} °C to {to:.1} °C in one tick"
            ),
            Self::NoSources => write!(f, "the fan has no sources"),
        }
    }
}

/// The outcome of one tick for one fan.
#[derive(Debug, Clone, PartialEq)]
pub struct Tick {
    /// Duty in percent to write.
    pub duty: f64,
    /// The hottest source and its raw reading, if every source was readable.
    pub hottest: Option<(String, f64)>,
    /// The moving average of the hottest source.
    pub smoothed: Option<f64>,
    /// The temperature the curve was applied to, after hysteresis.
    pub effective: Option<f64>,
    /// Set when this tick used the failsafe duty.
    pub failsafe: Option<Failsafe>,
}

/// Control state of one fan, carried from tick to tick.
#[derive(Debug, Clone)]
pub struct FanController {
    fan: Fan,
    failsafe_duty: f64,
    /// Weight of a new reading in the moving average, 0 < alpha <= 1.
    alpha: f64,
    smoothed: Option<f64>,
    effective: Option<f64>,
    /// Each source's previous raw reading, for spotting jumps.
    previous: BTreeMap<String, f64>,
}

impl FanController {
    /// A controller for `fan`, ticking every `interval`.
    #[must_use]
    pub fn new(fan: Fan, interval: Duration, failsafe_duty: f64) -> Self {
        let alpha = smoothing_weight(interval, fan.smoothing);
        Self {
            fan,
            failsafe_duty,
            alpha,
            smoothed: None,
            effective: None,
            previous: BTreeMap::new(),
        }
    }

    /// Runs one tick on this tick's readings, keyed by sensor name. A missing
    /// key or `None` means the sensor could not be read.
    pub fn tick(&mut self, readings: &BTreeMap<String, Option<f64>>) -> Tick {
        let (source, celsius) = match self.check_sources(readings) {
            Ok(hottest) => hottest,
            Err(failsafe) => return self.failsafe_tick(failsafe),
        };
        let smoothed = self.smooth(celsius);
        let effective = self.apply_hysteresis(smoothed);
        Tick {
            duty: self.fan.duty_at(effective),
            hottest: Some((source, celsius)),
            smoothed: Some(smoothed),
            effective: Some(effective),
            failsafe: None,
        }
    }

    /// The hottest of this fan's readings, or why they cannot be trusted.
    /// Records every readable value as the next tick's "previous", even on
    /// failure, so one real jump causes one failsafe tick rather than many. An
    /// unreadable source has no "previous".
    fn check_sources(
        &mut self,
        readings: &BTreeMap<String, Option<f64>>,
    ) -> Result<(String, f64), Failsafe> {
        let mut hottest: Option<(&String, f64)> = None;
        let mut problem = None;
        for source in &self.fan.sources {
            let Some(celsius) = readings.get(source).copied().flatten() else {
                problem.get_or_insert(Failsafe::Unreadable {
                    source: source.clone(),
                });
                // Its next reading may come much later; comparing that with
                // this old value would report heat built up meanwhile as a jump.
                self.previous.remove(source);
                continue;
            };
            match self.previous.get_mut(source) {
                Some(previous) => {
                    if (celsius - *previous).abs() > MAX_JUMP_CELSIUS {
                        problem.get_or_insert(Failsafe::Jump {
                            source: source.clone(),
                            from: *previous,
                            to: celsius,
                        });
                    }
                    *previous = celsius;
                }
                None => _ = self.previous.insert(source.clone(), celsius),
            }
            if hottest.is_none_or(|(_, warmest)| celsius > warmest) {
                hottest = Some((source, celsius));
            }
        }
        if let Some(problem) = problem {
            return Err(problem);
        }
        hottest
            .map(|(source, celsius)| (source.clone(), celsius))
            .ok_or(Failsafe::NoSources)
    }

    fn failsafe_tick(&self, failsafe: Failsafe) -> Tick {
        Tick {
            duty: self.failsafe_duty,
            hottest: None,
            smoothed: self.smoothed,
            effective: self.effective,
            failsafe: Some(failsafe),
        }
    }

    /// Feeds `celsius` into the moving average and returns the new average.
    /// The first reading starts the average.
    fn smooth(&mut self, celsius: f64) -> f64 {
        let smoothed = match self.smoothed {
            Some(average) => average + self.alpha * (celsius - average),
            None => celsius,
        };
        self.smoothed = Some(smoothed);
        smoothed
    }

    /// Rises follow at once; falls only once they reach `hysteresis` °C.
    fn apply_hysteresis(&mut self, smoothed: f64) -> f64 {
        let effective = match self.effective {
            Some(held) if smoothed < held && held - smoothed < self.fan.hysteresis => held,
            _ => smoothed,
        };
        self.effective = Some(effective);
        effective
    }
}

/// alpha = interval / smoothing, capped at 1. No smoothing means alpha = 1:
/// every reading replaces the average.
fn smoothing_weight(interval: Duration, smoothing: Duration) -> f64 {
    if smoothing.is_zero() {
        return 1.0;
    }
    (interval.as_secs_f64() / smoothing.as_secs_f64()).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const CONFIG: &str = r#"
[sensors]
cpu = { chip = "k10temp", label = "Tctl" }
gpu = { chip = "amdgpu",  label = "junction" }

[fan.case]
output     = { chip = "nct6798", pwm = 1 }
sources    = ["cpu", "gpu"]
curve      = [[40, 20], [80, 100]]   # 2 % per °C
smoothing  = "4s"
hysteresis = 2.0
"#;

    fn controller(smoothing: &str) -> FanController {
        let text = CONFIG.replace(
            r#"smoothing  = "4s""#,
            &format!("smoothing  = {smoothing:?}"),
        );
        let config = Config::parse(&text).unwrap();
        FanController::new(config.fans["case"].clone(), Duration::from_secs(1), 100.0)
    }

    fn readings(cpu: Option<f64>, gpu: Option<f64>) -> BTreeMap<String, Option<f64>> {
        BTreeMap::from([("cpu".to_owned(), cpu), ("gpu".to_owned(), gpu)])
    }

    fn assert_close(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("a value");
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn the_hottest_source_drives_the_fan() {
        let mut fan = controller("0s");

        let tick = fan.tick(&readings(Some(50.0), Some(60.0)));

        assert_eq!(tick.hottest, Some(("gpu".to_owned(), 60.0)));
        assert!((tick.duty - 60.0).abs() < 1e-9);
        assert_eq!(tick.failsafe, None);
    }

    #[test]
    fn smoothing_moves_a_quarter_of_the_way_each_tick() {
        let mut fan = controller("4s"); // alpha = 1 s / 4 s

        fan.tick(&readings(Some(40.0), Some(0.0))); // starts the average at 40
        let tick = fan.tick(&readings(Some(48.0), Some(0.0)));

        assert_close(tick.smoothed, 42.0); // 40 + (48 - 40) / 4
    }

    #[test]
    fn small_drops_are_held_and_big_ones_followed() {
        let mut fan = controller("0s");
        fan.tick(&readings(Some(60.0), Some(0.0)));

        assert_close(fan.tick(&readings(Some(58.5), Some(0.0))).effective, 60.0); // 1.5 °C drop: held
        assert_close(fan.tick(&readings(Some(57.5), Some(0.0))).effective, 57.5); // 2.5 °C: followed
        assert_close(fan.tick(&readings(Some(59.0), Some(0.0))).effective, 59.0); // rises: followed at once
    }

    #[test]
    fn an_unreadable_source_means_failsafe() {
        let mut fan = controller("0s");

        let tick = fan.tick(&readings(Some(50.0), None));

        assert!((tick.duty - 100.0).abs() < 1e-9);
        assert_eq!(
            tick.failsafe,
            Some(Failsafe::Unreadable {
                source: "gpu".into()
            })
        );
    }

    #[test]
    fn a_jump_means_one_failsafe_tick() {
        let mut fan = controller("0s");
        fan.tick(&readings(Some(45.0), Some(40.0)));

        let jump = fan.tick(&readings(Some(90.0), Some(40.0)));
        let after = fan.tick(&readings(Some(90.0), Some(40.0)));

        assert_eq!(
            jump.failsafe,
            Some(Failsafe::Jump {
                source: "cpu".into(),
                from: 45.0,
                to: 90.0
            })
        );
        assert_eq!(after.failsafe, None);
        assert!((after.duty - 100.0).abs() < 1e-9); // 90 °C is past the curve's end
    }

    #[test]
    fn a_sensor_coming_back_is_not_compared_with_its_old_value() {
        let mut fan = controller("0s");
        fan.tick(&readings(Some(45.0), Some(40.0)));
        fan.tick(&readings(None, Some(40.0))); // cpu unreadable for a while

        let back = fan.tick(&readings(Some(70.0), Some(40.0)));

        assert_eq!(back.failsafe, None);
    }

    #[test]
    fn failsafe_ticks_leave_the_average_alone() {
        let mut fan = controller("4s");
        fan.tick(&readings(Some(50.0), Some(0.0)));

        let failsafe = fan.tick(&readings(None, Some(0.0)));

        assert_close(failsafe.smoothed, 50.0);
    }

    #[test]
    fn smoothing_weight_is_capped_and_handles_zero() {
        let second = Duration::from_secs(1);

        assert!((smoothing_weight(second, Duration::ZERO) - 1.0).abs() < 1e-9);
        assert!((smoothing_weight(second, Duration::from_secs(8)) - 0.125).abs() < 1e-9);
        assert!((smoothing_weight(second, Duration::from_millis(500)) - 1.0).abs() < 1e-9);
    }
}
