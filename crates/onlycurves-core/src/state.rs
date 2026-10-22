//! The state snapshot the daemon writes every tick to
//! `/run/onlycurves/state.json`, for `status` and the later TUI.
//!
//! The daemon only writes it and never reads it back, so the file is not an
//! interface into the daemon. `version` changes whenever the format does.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Format version of [`Snapshot`].
pub const VERSION: u32 = 1;

/// The daemon's view of every sensor and fan at one tick.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    /// When the tick ran, in milliseconds since the Unix epoch.
    pub unix_millis: u64,
    /// Every sensor's reading in °C, by name; `None` when unreadable.
    pub sensors: BTreeMap<String, Option<f64>>,
    /// Every fan, by name.
    pub fans: BTreeMap<String, FanSnapshot>,
}

/// One fan at one tick.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FanSnapshot {
    /// E.g. `nct6798 pwm2`.
    pub output: String,
    pub mode: FanMode,
    /// The hottest source and its reading; `None` in failsafe.
    pub source: Option<String>,
    pub celsius: Option<f64>,
    /// After smoothing.
    pub smoothed_celsius: Option<f64>,
    /// After hysteresis: the temperature the curve was applied to.
    pub effective_celsius: Option<f64>,
    /// Target duty in percent.
    pub duty: f64,
    /// Raw duty written to the output, 0–255.
    pub pwm_raw: u8,
    /// `pwmN_enable` as read back this tick.
    pub pwm_enable: Option<u8>,
    /// The fan's tachometer, if it has one.
    pub rpm: Option<u32>,
}

/// How a fan's duty was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FanMode {
    /// From the curve.
    Normal,
    /// The failsafe duty, because a source was unreadable or implausible.
    Failsafe,
}

impl Snapshot {
    /// The snapshot as pretty-printed JSON.
    ///
    /// # Errors
    ///
    /// Fails only if a value cannot be represented in JSON, e.g. NaN.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// Parses a snapshot written by [`Snapshot::to_json`].
    ///
    /// # Errors
    ///
    /// Fails when the text is not a snapshot of this format.
    pub fn from_json(text: &str) -> serde_json::Result<Self> {
        serde_json::from_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            version: VERSION,
            unix_millis: 1_792_620_000_000,
            sensors: BTreeMap::from([("cpu".into(), Some(49.125)), ("gpu".into(), None)]),
            fans: BTreeMap::from([(
                "cpu_cooler".into(),
                FanSnapshot {
                    output: "nct6798 pwm2".into(),
                    mode: FanMode::Normal,
                    source: Some("cpu".into()),
                    celsius: Some(49.125),
                    smoothed_celsius: Some(48.0),
                    effective_celsius: Some(48.0),
                    duty: 46.25,
                    pwm_raw: 118,
                    pwm_enable: Some(1),
                    rpm: Some(1220),
                },
            )]),
        }
    }

    #[test]
    fn survives_a_json_round_trip() {
        let json = snapshot().to_json().unwrap();

        assert_eq!(Snapshot::from_json(&json).unwrap(), snapshot());
    }

    #[test]
    fn modes_are_lowercase_words() {
        let json = snapshot().to_json().unwrap();

        assert!(json.contains(r#""mode": "normal""#), "{json}");
    }
}
