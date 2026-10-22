//! Converting between duty percentages and the raw 0–255 PWM scale.

/// The raw PWM value for `percent`, rounded to the nearest step.
/// Values outside 0–100 % are clamped.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to 0.0–255.0 first
pub fn percent_to_raw(percent: f64) -> u8 {
    (percent.clamp(0.0, 100.0) * 255.0 / 100.0).round() as u8
}

/// The duty of a raw PWM value, rounded to the nearest whole percent.
#[must_use]
pub fn raw_to_percent(raw: u8) -> u8 {
    let percent = (u32::from(raw) * 100 + 127) / 255;
    u8::try_from(percent).unwrap_or(100) // 0–255 scales to at most 100
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_to_raw_rounds_and_clamps() {
        assert_eq!(percent_to_raw(0.0), 0);
        assert_eq!(percent_to_raw(40.0), 102);
        assert_eq!(percent_to_raw(42.5), 108);
        assert_eq!(percent_to_raw(100.0), 255);
        assert_eq!(percent_to_raw(-5.0), 0);
        assert_eq!(percent_to_raw(150.0), 255);
    }

    #[test]
    fn raw_to_percent_rounds_to_nearest() {
        assert_eq!(raw_to_percent(0), 0);
        assert_eq!(raw_to_percent(102), 40);
        assert_eq!(raw_to_percent(120), 47);
        assert_eq!(raw_to_percent(255), 100);
    }

    #[test]
    fn whole_percents_survive_a_round_trip() {
        for percent in 0..=100u8 {
            assert_eq!(raw_to_percent(percent_to_raw(f64::from(percent))), percent);
        }
    }
}
