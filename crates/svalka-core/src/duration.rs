//! Durations written as text in the config, such as `"1s"` or `"500ms"`.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Deserializer, de};

/// A duration that cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDurationError(String);

impl fmt::Display for ParseDurationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid duration {:?}; use a number with ms or s, e.g. \"500ms\" or \"8s\"",
            self.0
        )
    }
}

impl std::error::Error for ParseDurationError {}

/// Parses a whole number followed by `ms` or `s`.
///
/// # Errors
///
/// Fails on anything else, including fractions and missing units.
pub fn parse(text: &str) -> Result<Duration, ParseDurationError> {
    let error = || ParseDurationError(text.to_owned());
    let (number, to_duration): (&str, fn(u64) -> Duration) =
        if let Some(number) = text.strip_suffix("ms") {
            (number, Duration::from_millis)
        } else if let Some(number) = text.strip_suffix('s') {
            (number, Duration::from_secs)
        } else {
            return Err(error());
        };
    let value = number.parse().map_err(|_| error())?;
    Ok(to_duration(value))
}

/// For `#[serde(deserialize_with = "duration::deserialize")]`.
///
/// # Errors
///
/// Fails when the value is not a string or [`parse`] rejects it.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    let text = String::deserialize(deserializer)?;
    parse(&text).map_err(de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_seconds_and_milliseconds() {
        assert_eq!(parse("1s"), Ok(Duration::from_secs(1)));
        assert_eq!(parse("8s"), Ok(Duration::from_secs(8)));
        assert_eq!(parse("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse("0s"), Ok(Duration::ZERO));
    }

    #[test]
    fn rejects_other_formats() {
        for text in ["", "8", "8m", "1.5s", "-1s", "s", " 8s"] {
            assert!(parse(text).is_err(), "{text:?} should not parse");
        }
    }
}
