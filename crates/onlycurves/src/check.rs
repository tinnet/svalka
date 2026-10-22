//! The `check` command: validate the config against the hardware and show the
//! duty each fan would get right now.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, anyhow};
use onlycurves_core::config::Config;
use onlycurves_core::controller::hottest;
use onlycurves_core::pwm;

use crate::binding::Bindings;
use crate::setup::Setup;

/// One sensor's reading.
#[derive(Debug, Clone, PartialEq)]
pub struct SensorReading {
    pub name: String,
    /// E.g. `k10temp Tctl`.
    pub input: String,
    pub celsius: f64,
}

/// What one fan would do at the current temperatures.
#[derive(Debug, Clone, PartialEq)]
pub struct FanReading {
    pub name: String,
    /// E.g. `nct6798 pwm2`.
    pub output: String,
    /// The hottest of the fan's sources, which drives it.
    pub source: String,
    pub celsius: f64,
    /// Duty in percent from the curve and `min_duty`.
    pub duty: f64,
    /// Raw duty the output is at now, set by whoever controls it today.
    pub current_raw: u8,
}

/// Everything `check` prints after the config passed.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub sensors: Vec<SensorReading>,
    pub fans: Vec<FanReading>,
}

/// Loads `config_path`, checks it against the chips under `hwmon_root` and
/// returns the rendered report.
///
/// # Errors
///
/// Fails when the file cannot be read, the config is invalid, it does not
/// match the hardware, or a sensor or output cannot be read.
pub fn run(config_path: &Path, hwmon_root: &Path) -> anyhow::Result<String> {
    let setup = Setup::load(config_path, hwmon_root)?;
    let report = read_report(&setup.config, &setup.bindings)?;
    Ok(render(config_path, &report))
}

/// Reads every sensor once and works out each fan's duty from those readings.
///
/// # Errors
///
/// Fails when a sensor or output cannot be read.
pub fn read_report(config: &Config, bindings: &Bindings) -> anyhow::Result<Report> {
    let mut sensors = Vec::new();
    for (name, sensor) in &bindings.sensors {
        let celsius = sensor
            .read_celsius()
            .with_context(|| format!("cannot read sensor {name} ({})", sensor.describe()))?;
        sensors.push(SensorReading {
            name: name.clone(),
            input: sensor.describe(),
            celsius,
        });
    }
    let temperatures: BTreeMap<&str, f64> = sensors
        .iter()
        .map(|reading| (reading.name.as_str(), reading.celsius))
        .collect();

    let mut fans = Vec::new();
    for (name, fan) in &config.fans {
        // `bind` succeeded, so every fan has an output.
        let output = &bindings.outputs[name];
        let readings = fan
            .sources
            .iter()
            .filter_map(|source| Some((source.as_str(), *temperatures.get(source.as_str())?)));
        let (source, celsius) =
            hottest(readings).ok_or_else(|| anyhow!("fan {name} has no readable sources"))?;
        fans.push(FanReading {
            name: name.clone(),
            output: output.describe(),
            source: source.to_owned(),
            celsius,
            duty: fan.duty_at(celsius),
            current_raw: output
                .read_raw()
                .with_context(|| format!("cannot read {}", output.describe()))?,
        });
    }
    Ok(Report { sensors, fans })
}

/// Renders the report under a line saying the config is valid.
#[must_use]
pub fn render(config_path: &Path, report: &Report) -> String {
    let sensor_rows: Vec<Vec<String>> = report.sensors.iter().map(sensor_row).collect();
    let fan_rows: Vec<Vec<String>> = report.fans.iter().map(fan_row).collect();
    format!(
        "{} is valid.\n\nSensors\n{}\nFans, at the current temperatures (no smoothing or hysteresis)\n{}",
        config_path.display(),
        align_columns(&sensor_rows),
        align_columns(&fan_rows),
    )
}

/// E.g. `cpu  k10temp Tctl  49.1 °C`.
fn sensor_row(sensor: &SensorReading) -> Vec<String> {
    vec![
        sensor.name.clone(),
        sensor.input.clone(),
        format!("{:>5.1} °C", sensor.celsius),
    ]
}

/// E.g. `cpu_cooler  nct6798 pwm2  cpu  49.1 °C  →   44 %  112/255  now  47 %  120/255`.
fn fan_row(fan: &FanReading) -> Vec<String> {
    let raw = pwm::percent_to_raw(fan.duty);
    vec![
        fan.name.clone(),
        fan.output.clone(),
        fan.source.clone(),
        format!("{:>5.1} °C  →", fan.celsius),
        format!("{:>3.0} %  {raw:>3}/255", fan.duty),
        format!(
            "now {:>3} %  {:>3}/255",
            pwm::raw_to_percent(fan.current_raw),
            fan.current_raw
        ),
    ]
}

/// Lays out rows as left-aligned columns, two spaces apart, each line indented.
fn align_columns(rows: &[Vec<String>]) -> String {
    let column_count = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..column_count)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();

    let mut out = String::new();
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, &width)| format!("{cell:<width$}"))
            .collect();
        out += "  ";
        out += cells.join("  ").trim_end();
        out += "\n";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Report {
        Report {
            sensors: vec![
                SensorReading {
                    name: "cpu".into(),
                    input: "k10temp Tctl".into(),
                    celsius: 49.125,
                },
                SensorReading {
                    name: "gpu".into(),
                    input: "amdgpu junction".into(),
                    celsius: 47.0,
                },
            ],
            fans: vec![
                FanReading {
                    name: "cpu_cooler".into(),
                    output: "nct6798 pwm2".into(),
                    source: "cpu".into(),
                    celsius: 49.125,
                    duty: 47.656_25,
                    current_raw: 120,
                },
                FanReading {
                    name: "exhaust".into(),
                    output: "nct6798 pwm1".into(),
                    source: "gpu".into(),
                    celsius: 47.0,
                    duty: 47.0,
                    current_raw: 102,
                },
            ],
        }
    }

    #[test]
    fn renders_sensors_and_fans_in_columns() {
        let expected = "\
/etc/onlycurves.toml is valid.

Sensors
  cpu  k10temp Tctl      49.1 °C
  gpu  amdgpu junction   47.0 °C

Fans, at the current temperatures (no smoothing or hysteresis)
  cpu_cooler  nct6798 pwm2  cpu   49.1 °C  →   48 %  122/255  now  47 %  120/255
  exhaust     nct6798 pwm1  gpu   47.0 °C  →   47 %  120/255  now  40 %  102/255
";
        assert_eq!(
            render(Path::new("/etc/onlycurves.toml"), &report()),
            expected
        );
    }

    #[test]
    fn align_columns_pads_every_column_but_the_last() {
        let rows = vec![
            vec!["a".to_owned(), "long cell".to_owned(), "x".to_owned()],
            vec!["bbb".to_owned(), "c".to_owned(), "y".to_owned()],
        ];

        assert_eq!(
            align_columns(&rows),
            "  a    long cell  x\n  bbb  c          y\n"
        );
    }
}
