//! The history format: one JSON line per tick, appended by the daemon to
//! `/run/svalka/history`, for `svalka history` and anything else that wants
//! the recent past.
//!
//! A line is the tick's snapshot plus whether the fan outputs were read back
//! on that tick. rpm and `pwmN_enable` are only read every
//! `readback_interval`, so they are left out of the other lines: repeating
//! the last reading would pass 10 s data off as 1 s data.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::state::{FanSnapshot, Snapshot};

/// Format version of [`Line`]. Readers skip lines of any other version.
pub const VERSION: u32 = 1;

/// One tick, as stored in the history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub version: u32,
    /// When the tick ran, in milliseconds since the Unix epoch.
    pub unix_millis: u64,
    /// Whether this tick read the fan outputs back; only then are each fan's
    /// `rpm` and `pwm_enable` set.
    pub readback: bool,
    /// Every sensor's reading in °C, by name; `None` when unreadable.
    pub sensors: BTreeMap<String, Option<f64>>,
    /// Every fan, by name.
    pub fans: BTreeMap<String, FanSnapshot>,
}

impl Line {
    /// The history line for a tick's snapshot. Temperatures and duties are
    /// rounded to 0.01, which is finer than any sensor reads and keeps lines
    /// short.
    #[must_use]
    pub fn of(snapshot: &Snapshot, readback: bool) -> Self {
        Self {
            version: VERSION,
            unix_millis: snapshot.unix_millis,
            readback,
            sensors: snapshot
                .sensors
                .iter()
                .map(|(name, celsius)| (name.clone(), celsius.map(round)))
                .collect(),
            fans: snapshot
                .fans
                .iter()
                .map(|(name, fan)| (name.clone(), fan_line(fan, readback)))
                .collect(),
        }
    }

    /// The line as one line of compact JSON, without the newline.
    ///
    /// # Errors
    ///
    /// Fails only if a value cannot be represented in JSON, e.g. NaN.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

fn fan_line(fan: &FanSnapshot, readback: bool) -> FanSnapshot {
    FanSnapshot {
        celsius: fan.celsius.map(round),
        smoothed_celsius: fan.smoothed_celsius.map(round),
        effective_celsius: fan.effective_celsius.map(round),
        duty: round(fan.duty),
        pwm_enable: fan.pwm_enable.filter(|_| readback),
        rpm: fan.rpm.filter(|_| readback),
        ..fan.clone()
    }
}

fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// The lines of one history file. Lines that do not parse, such as one the
/// daemon was writing when it was killed, and lines of another version are
/// skipped.
#[must_use]
pub fn parse(text: &str) -> Vec<Line> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<Line>(line).ok())
        .filter(|line| line.version == VERSION)
        .collect()
}

/// The lines of several files as one history, oldest first, each tick once.
#[must_use]
pub fn merge(files: impl IntoIterator<Item = Vec<Line>>) -> Vec<Line> {
    let mut seen = BTreeSet::new();
    let mut lines: Vec<Line> = files
        .into_iter()
        .flatten()
        .filter(|line| seen.insert(line.unix_millis))
        .collect();
    lines.sort_by_key(|line| line.unix_millis);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::FanMode;

    fn snapshot(unix_millis: u64) -> Snapshot {
        Snapshot {
            version: crate::state::VERSION,
            unix_millis,
            sensors: BTreeMap::from([("cpu".into(), Some(49.125)), ("gpu".into(), None)]),
            fans: BTreeMap::from([(
                "cpu_cooler".into(),
                FanSnapshot {
                    output: "nct6798 pwm2".into(),
                    mode: FanMode::Normal,
                    source: Some("cpu".into()),
                    celsius: Some(49.125),
                    smoothed_celsius: Some(48.421_552_931),
                    effective_celsius: Some(48.421_552_931),
                    duty: 46.776_941_164,
                    pwm_raw: 118,
                    pwm_enable: Some(1),
                    rpm: Some(1220),
                },
            )]),
        }
    }

    #[test]
    fn readings_of_the_outputs_appear_only_on_readback_lines() {
        let read_back = Line::of(&snapshot(0), true);
        let between = Line::of(&snapshot(0), false);

        let fan = |line: &Line| {
            let fan = &line.fans["cpu_cooler"];
            (fan.rpm, fan.pwm_enable)
        };
        assert_eq!(fan(&read_back), (Some(1220), Some(1)));
        assert_eq!(fan(&between), (None, None));
        assert_eq!(between.fans["cpu_cooler"].pwm_raw, 118);
    }

    #[test]
    fn values_are_rounded_to_hundredths() {
        let line = Line::of(&snapshot(0), true);
        let fan = &line.fans["cpu_cooler"];

        assert_eq!(line.sensors["cpu"], Some(49.13));
        assert_eq!(fan.smoothed_celsius, Some(48.42));
        assert!((fan.duty - 46.78).abs() < 1e-9, "{}", fan.duty);
    }

    #[test]
    fn a_line_is_one_line_of_json_and_parses_back() {
        let line = Line::of(&snapshot(5), true);
        let json = line.to_json().unwrap();

        assert!(!json.contains('\n'), "{json}");
        assert_eq!(parse(&json), vec![line]);
    }

    #[test]
    fn parse_skips_broken_lines_and_other_versions() {
        let good = Line::of(&snapshot(1), false).to_json().unwrap();
        let other_version = good.replacen(r#""version":1"#, r#""version":99"#, 1);
        let half = &good[..good.len() / 2];
        let text = format!("{good}\n{other_version}\n{half}");

        assert_eq!(parse(&text).len(), 1);
    }

    #[test]
    fn merge_orders_the_ticks_and_keeps_each_once() {
        let line = |millis| Line::of(&snapshot(millis), false);
        let current = vec![line(3), line(4)];
        let old = vec![line(1), line(2), line(3)];

        let merged = merge([current, old]);

        let millis: Vec<u64> = merged.iter().map(|line| line.unix_millis).collect();
        assert_eq!(millis, [1, 2, 3, 4]);
    }
}
