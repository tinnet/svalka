//! The `status` command: a read-only table of every chip's sensors.

use std::fmt::Write;
use std::path::Path;

use anyhow::Context;

use crate::hwmon::{self, Chip, ChipSnapshot, Fan, Pwm, Temp};

/// Shown in place of a value that could not be read.
const UNREADABLE: &str = "unreadable";

/// Which chips `status` shows: all of them, or the one a name (and path) resolves to.
pub struct ChipFilter<'a> {
    pub name: Option<&'a str>,
    pub path: Option<&'a str>,
}

/// Reads the selected chips under `hwmon_root` and returns the rendered table.
///
/// # Errors
///
/// Fails when the chips cannot be listed, the filter does not resolve to one
/// chip, or a selected chip cannot be read.
pub fn run(hwmon_root: &Path, filter: &ChipFilter) -> anyhow::Result<String> {
    let chips = hwmon::discover(hwmon_root)
        .with_context(|| format!("cannot list hwmon chips in {}", hwmon_root.display()))?;
    let snapshots = select(&chips, filter)?
        .into_iter()
        .map(read_snapshot)
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(render(&snapshots))
}

fn select<'a>(
    chips: &'a [Chip],
    filter: &ChipFilter,
) -> Result<Vec<&'a Chip>, hwmon::ResolveError> {
    match filter.name {
        Some(name) => Ok(vec![hwmon::resolve(chips, name, filter.path)?]),
        None => Ok(chips.iter().collect()),
    }
}

fn read_snapshot(chip: &Chip) -> anyhow::Result<ChipSnapshot> {
    chip.snapshot()
        .with_context(|| format!("cannot read {}", chip.dir().display()))
}

/// Renders one block per chip. Chips without any sensors are left out.
pub fn render(snapshots: &[ChipSnapshot]) -> String {
    snapshots
        .iter()
        .filter(|snapshot| !snapshot.is_empty())
        .map(render_chip)
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_chip(chip: &ChipSnapshot) -> String {
    let width = label_width(chip);
    let mut out = chip_heading(chip);
    if !chip.temps.is_empty() {
        out += &section(
            "Temperatures",
            &chip.temps,
            width,
            |temp| &temp.label,
            temp_value,
        );
    }
    if !chip.fans.is_empty() {
        out += &section("Fans", &chip.fans, width, |fan| &fan.label, fan_value);
    }
    if !chip.pwms.is_empty() {
        out += &section(
            "PWM outputs",
            &chip.pwms,
            width,
            |pwm| &pwm.label,
            pwm_value,
        );
    }
    out
}

/// The longest label of any sensor on the chip, so all its values line up.
fn label_width(chip: &ChipSnapshot) -> usize {
    let temps = chip.temps.iter().map(|temp| &temp.label);
    let fans = chip.fans.iter().map(|fan| &fan.label);
    let pwms = chip.pwms.iter().map(|pwm| &pwm.label);
    temps
        .chain(fans)
        .chain(pwms)
        .map(|label| label.chars().count())
        .max()
        .unwrap_or(0)
}

/// E.g. `nct6798 · nct6775.656 · now hwmon7`.
fn chip_heading(chip: &ChipSnapshot) -> String {
    let device = chip.device_id.as_deref().unwrap_or("no device");
    format!("{} · {device} · now {}\n", chip.name, chip.hwmon_number)
}

/// A titled list of `label  value` lines, labels padded to `width`.
fn section<T>(
    title: &str,
    items: &[T],
    width: usize,
    label: impl Fn(&T) -> &str,
    value: impl Fn(&T) -> String,
) -> String {
    let mut out = format!("  {title}\n");
    for item in items {
        writeln!(out, "    {:<width$}  {}", label(item), value(item))
            .expect("writing to a String cannot fail");
    }
    out
}

/// E.g. ` 49.1 °C`.
fn temp_value(temp: &Temp) -> String {
    match temp.celsius {
        Some(celsius) => format!("{celsius:>5.1} °C"),
        None => UNREADABLE.to_owned(),
    }
}

/// E.g. ` 971 rpm`.
fn fan_value(fan: &Fan) -> String {
    match fan.rpm {
        Some(rpm) => format!("{rpm:>5} rpm"),
        None => UNREADABLE.to_owned(),
    }
}

/// E.g. `102/255   40 %   manual (1)`.
fn pwm_value(pwm: &Pwm) -> String {
    let duty = match (pwm.duty, pwm.duty_percent()) {
        (Some(raw), Some(percent)) => format!("{raw:>3}/255  {percent:>3} %"),
        _ => UNREADABLE.to_owned(),
    };
    match pwm.enable {
        Some(enable) => format!("{duty}   {enable}"),
        None => duty,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwmon::PwmEnable;

    fn nct6798() -> ChipSnapshot {
        ChipSnapshot {
            name: "nct6798".into(),
            device_id: Some("nct6775.656".into()),
            hwmon_number: "hwmon7".into(),
            temps: vec![
                Temp {
                    label: "SYSTIN".into(),
                    celsius: Some(34.0),
                },
                Temp {
                    label: "AUXTIN0".into(),
                    celsius: None,
                },
            ],
            fans: vec![Fan {
                label: "fan1".into(),
                rpm: Some(971),
            }],
            pwms: vec![
                Pwm {
                    label: "pwm1".into(),
                    duty: Some(102),
                    enable: Some(PwmEnable::Manual),
                },
                Pwm {
                    label: "pwm5".into(),
                    duty: Some(0),
                    enable: Some(PwmEnable::Automatic(5)),
                },
            ],
        }
    }

    fn empty_chip() -> ChipSnapshot {
        ChipSnapshot {
            name: "asus".into(),
            device_id: Some("eeepc-wmi".into()),
            hwmon_number: "hwmon5".into(),
            temps: vec![],
            fans: vec![],
            pwms: vec![],
        }
    }

    #[test]
    fn renders_every_section_with_aligned_values() {
        let expected = "\
nct6798 · nct6775.656 · now hwmon7
  Temperatures
    SYSTIN    34.0 °C
    AUXTIN0  unreadable
  Fans
    fan1       971 rpm
  PWM outputs
    pwm1     102/255   40 %   manual (1)
    pwm5       0/255    0 %   automatic (5)
";
        assert_eq!(render(&[nct6798()]), expected);
    }

    #[test]
    fn leaves_out_chips_without_sensors_and_separates_the_rest() {
        let output = render(&[empty_chip(), nct6798(), nct6798()]);

        assert!(!output.contains("asus"));
        assert_eq!(output.matches("nct6798 · ").count(), 2);
        assert!(output.contains("\n\nnct6798 · "));
    }

    #[test]
    fn pwm_without_enable_shows_only_the_duty() {
        let pwm = Pwm {
            label: "pwm1".into(),
            duty: Some(255),
            enable: None,
        };

        assert_eq!(pwm_value(&pwm), "255/255  100 %");
    }
}
