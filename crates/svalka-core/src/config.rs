//! The config file, `/etc/svalka.toml`.
//!
//! Parsing happens in two steps. Serde reads the text into `…File` structs that
//! mirror the TOML exactly (unknown keys are errors, so typos fail loudly).
//! Validation then turns those into [`Config`], collecting every problem it
//! finds instead of stopping at the first. Checks that need the hardware, such
//! as whether a chip exists, happen later in the daemon.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde::Deserialize;

use crate::controller::FanController;
use crate::curve::{Curve, is_percent};
use crate::duration;

/// A validated config.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// How often the control loop runs.
    pub interval: Duration,
    /// How often the fan outputs are read back (duty, mode, rpm) to notice a
    /// stall or something else taking an output over. Duties are still
    /// written every `interval`. On ASUS boards each read of the nct6798 costs
    /// about 84 ms of CPU, so this is longer than `interval` by default.
    pub readback_interval: Duration,
    /// Duty in percent for fans whose sources cannot be read.
    pub failsafe_duty: f64,
    /// Temperature inputs, by the name fans refer to them with.
    pub sensors: BTreeMap<String, SensorRef>,
    /// Fans, by name.
    pub fans: BTreeMap<String, Fan>,
}

/// Where a temperature comes from: a chip and the label of one of its inputs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorRef {
    /// Driver name, e.g. `k10temp`.
    pub chip: String,
    /// Device path, only needed when several chips share the name.
    pub path: Option<String>,
    /// The input's label, e.g. `Tctl`, or `tempN` for an unlabelled input.
    pub label: String,
}

/// A PWM output: a chip and the output's number.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputRef {
    /// Driver name, e.g. `nct6798`.
    pub chip: String,
    /// Device path, only needed when several chips share the name.
    pub path: Option<String>,
    /// N in `pwmN`.
    pub pwm: u32,
}

impl fmt::Display for OutputRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} pwm{}", self.chip, self.pwm)?;
        if let Some(path) = &self.path {
            write!(f, " at {path}")?;
        }
        Ok(())
    }
}

/// One fan and how it is driven.
#[derive(Debug, Clone, PartialEq)]
pub struct Fan {
    pub output: OutputRef,
    /// Names of sensors in [`Config::sensors`]; the hottest one drives the fan.
    pub sources: Vec<String>,
    pub curve: Curve,
    /// Window of the moving average over the input temperature.
    pub smoothing: Duration,
    /// °C the temperature must fall before the duty drops.
    pub hysteresis: f64,
    /// Duty floor in percent.
    pub min_duty: f64,
}

impl Fan {
    /// The duty in percent at `celsius`: the curve's value, never below `min_duty`.
    #[must_use]
    pub fn duty_at(&self, celsius: f64) -> f64 {
        self.curve.duty_at(celsius).max(self.min_duty)
    }
}

/// Why a config was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The text is not valid TOML, or does not have the expected shape.
    Syntax(String),
    /// The TOML parsed, but breaks one or more rules. One message per problem.
    Invalid(Vec<String>),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(message) => write!(f, "{}", message.trim_end()),
            Self::Invalid(problems) => {
                write!(f, "{} problem(s):", problems.len())?;
                for problem in problems {
                    write!(f, "\n  - {problem}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// A controller for `fan`, ticking at this config's interval.
    #[must_use]
    pub fn controller(&self, fan: &Fan) -> FanController {
        FanController::new(fan.clone(), self.interval, self.failsafe_duty)
    }

    /// Parses and validates the text of a config file.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Syntax`] when the text cannot be read as a config,
    /// [`ConfigError::Invalid`] listing every rule it breaks.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let file: ConfigFile =
            toml::from_str(text).map_err(|error| ConfigError::Syntax(error.to_string()))?;
        file.validate()
    }
}

/// The top level of the file, exactly as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(
        default = "default_interval",
        deserialize_with = "duration::deserialize"
    )]
    interval: Duration,
    #[serde(
        default = "default_readback_interval",
        deserialize_with = "duration::deserialize"
    )]
    readback_interval: Duration,
    #[serde(default = "default_failsafe_duty")]
    failsafe_duty: f64,
    #[serde(default)]
    sensors: BTreeMap<String, SensorRef>,
    #[serde(default, rename = "fan")]
    fans: BTreeMap<String, FanFile>,
}

/// One `[fan.NAME]` table, exactly as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FanFile {
    output: OutputRef,
    sources: Vec<String>,
    /// `[°C, %]` pairs.
    curve: Vec<(f64, f64)>,
    #[serde(default, deserialize_with = "duration::deserialize")]
    smoothing: Duration,
    #[serde(default)]
    hysteresis: f64,
    #[serde(default)]
    min_duty: f64,
}

fn default_interval() -> Duration {
    Duration::from_secs(1)
}

fn default_readback_interval() -> Duration {
    Duration::from_secs(10)
}

fn default_failsafe_duty() -> f64 {
    100.0
}

impl ConfigFile {
    fn validate(self) -> Result<Config, ConfigError> {
        let mut problems = Vec::new();
        self.check_top_level(&mut problems);

        let mut fans = BTreeMap::new();
        for (name, fan) in self.fans {
            let mut fan_problems = Vec::new();
            let validated = fan.validate(&self.sensors, &mut fan_problems);
            problems.extend(
                fan_problems
                    .into_iter()
                    .map(|problem| format!("fan.{name}: {problem}")),
            );
            if let Some(fan) = validated {
                fans.insert(name, fan);
            }
        }

        if problems.is_empty() {
            Ok(Config {
                interval: self.interval,
                readback_interval: self.readback_interval,
                failsafe_duty: self.failsafe_duty,
                sensors: self.sensors,
                fans,
            })
        } else {
            Err(ConfigError::Invalid(problems))
        }
    }

    fn check_top_level(&self, problems: &mut Vec<String>) {
        if self.interval.is_zero() {
            problems.push("interval must be longer than 0s".to_owned());
        }
        if self.readback_interval.is_zero() {
            problems.push("readback_interval must be longer than 0s".to_owned());
        }
        if !is_percent(self.failsafe_duty) {
            problems.push(format!(
                "failsafe_duty is {} %; it must be 0–100 %",
                self.failsafe_duty
            ));
        }
        if self.fans.is_empty() {
            problems.push("no fans configured; add at least one [fan.NAME] table".to_owned());
        }
    }
}

impl FanFile {
    /// Returns the validated fan, or `None` after pushing at least one problem.
    fn validate(
        self,
        sensors: &BTreeMap<String, SensorRef>,
        problems: &mut Vec<String>,
    ) -> Option<Fan> {
        check_sources(&self.sources, sensors, problems);
        if !self.hysteresis.is_finite() || self.hysteresis < 0.0 {
            problems.push(format!(
                "hysteresis is {} °C; it must be 0 or more",
                self.hysteresis
            ));
        }
        if !is_percent(self.min_duty) {
            problems.push(format!(
                "min_duty is {} %; it must be 0–100 %",
                self.min_duty
            ));
        }
        let curve = Curve::from_pairs(self.curve)
            .map_err(|error| problems.push(format!("curve: {error}")))
            .ok()?;

        problems.is_empty().then_some(Fan {
            output: self.output,
            sources: self.sources,
            curve,
            smoothing: self.smoothing,
            hysteresis: self.hysteresis,
            min_duty: self.min_duty,
        })
    }
}

fn check_sources(
    sources: &[String],
    sensors: &BTreeMap<String, SensorRef>,
    problems: &mut Vec<String>,
) {
    if sources.is_empty() {
        problems.push("sources is empty; name at least one sensor".to_owned());
    }
    for source in sources {
        if !sensors.contains_key(source) {
            problems.push(format!("source {source:?} is not defined in [sensors]"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete config: two fans on an nct6798.
    const EXAMPLE: &str = r#"
interval = "1s"          # control loop period
failsafe_duty = 100       # % used when any input sensor fails to read

[sensors]
cpu = { chip = "k10temp", label = "Tctl" }
gpu = { chip = "amdgpu",  label = "junction" }   # read-only; the GPU fan stays on firmware

[fan.cpu_cooler]
output     = { chip = "nct6798", pwm = 2 }
sources    = ["cpu"]           # several sources: the hottest one wins
curve      = [[35, 30], [55, 55], [75, 90], [85, 100]]   # [°C, %]
smoothing  = "8s"              # EMA window on the input temperature
hysteresis = 2.0               # °C the temperature must fall before duty drops
min_duty   = 20                # % floor: this fan never stops

[fan.exhaust]
output     = { chip = "nct6798", pwm = 1 }
sources    = ["gpu"]
curve      = [[26, 25], [50, 50], [75, 75], [100, 100]]
smoothing  = "8s"
hysteresis = 2.0
"#;

    fn problems(text: &str) -> Vec<String> {
        match Config::parse(text) {
            Err(ConfigError::Invalid(problems)) => problems,
            other => panic!("expected validation problems, got {other:?}"),
        }
    }

    #[test]
    fn parses_the_example_config() {
        let config = Config::parse(EXAMPLE).unwrap();

        assert_eq!(config.interval, Duration::from_secs(1));
        assert_eq!(config.sensors["cpu"].label, "Tctl");
        let cooler = &config.fans["cpu_cooler"];
        assert_eq!(
            cooler.output,
            OutputRef {
                chip: "nct6798".into(),
                path: None,
                pwm: 2
            }
        );
        assert_eq!(cooler.smoothing, Duration::from_secs(8));
        assert_eq!(cooler.curve.points().len(), 4);
        assert!((config.fans["exhaust"].min_duty - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn fan_duty_never_goes_below_min_duty() {
        let config = Config::parse(&EXAMPLE.replace("min_duty   = 20", "min_duty   = 40")).unwrap();
        let cooler = &config.fans["cpu_cooler"];

        assert!((cooler.duty_at(20.0) - 40.0).abs() < 1e-9); // curve says 30 %
        assert!((cooler.duty_at(65.0) - 72.5).abs() < 1e-9); // curve is above the floor
    }

    #[test]
    fn unknown_keys_are_syntax_errors() {
        let error = Config::parse(&EXAMPLE.replace("hysteresis = 2.0  ", "hysteresys = 2.0  "))
            .unwrap_err();

        assert!(
            matches!(&error, ConfigError::Syntax(message) if message.contains("hysteresys")),
            "{error}"
        );
    }

    #[test]
    fn reports_every_problem_at_once() {
        let text = EXAMPLE
            .replace(r#"sources    = ["gpu"]"#, r#"sources    = ["gpuu"]"#)
            .replace("[[35, 30], [55, 55]", "[[55, 30], [35, 55]")
            .replace("failsafe_duty = 100", "failsafe_duty = 120");

        assert_eq!(
            problems(&text),
            [
                "failsafe_duty is 120 %; it must be 0–100 %",
                "fan.cpu_cooler: curve: point 2 (35 °C) must be hotter than point 1 (55 °C)",
                "fan.exhaust: source \"gpuu\" is not defined in [sensors]",
            ]
        );
    }

    #[test]
    fn rejects_bad_fan_settings() {
        let text = EXAMPLE
            .replace(r#"sources    = ["cpu"] "#, "sources    = [] ")
            .replace("min_duty   = 20", "min_duty   = -5")
            .replace(
                "hysteresis = 2.0               #",
                "hysteresis = -1.0              #",
            );

        assert_eq!(
            problems(&text),
            [
                "fan.cpu_cooler: sources is empty; name at least one sensor",
                "fan.cpu_cooler: hysteresis is -1 °C; it must be 0 or more",
                "fan.cpu_cooler: min_duty is -5 %; it must be 0–100 %",
            ]
        );
    }

    #[test]
    fn a_config_needs_fans() {
        assert_eq!(
            problems(""),
            ["no fans configured; add at least one [fan.NAME] table"]
        );
    }

    #[test]
    fn interval_must_not_be_zero() {
        let text = EXAMPLE.replace(r#"interval = "1s""#, r#"interval = "0s""#);

        assert_eq!(problems(&text), ["interval must be longer than 0s"]);
    }

    #[test]
    fn readback_interval_defaults_to_ten_seconds_and_must_not_be_zero() {
        assert_eq!(
            Config::parse(EXAMPLE).unwrap().readback_interval,
            Duration::from_secs(10)
        );

        let text = format!("readback_interval = \"0s\"\n{EXAMPLE}");
        assert_eq!(
            problems(&text),
            ["readback_interval must be longer than 0s"]
        );
    }

    #[test]
    fn bad_durations_are_syntax_errors() {
        let error =
            Config::parse(&EXAMPLE.replace(r#"smoothing  = "8s"  "#, r#"smoothing  = "8"  "#))
                .unwrap_err();

        assert!(
            matches!(&error, ConfigError::Syntax(message) if message.contains("invalid duration")),
            "{error}"
        );
    }
}
