//! The `check` command: validate the config against the hardware and show the
//! duty each fan would get right now.

use std::path::Path;

use anyhow::Context;
use svalka_core::controller::Tick;
use svalka_core::pwm;

use crate::binding;
use crate::setup::Setup;

/// One sensor's reading.
#[derive(Debug, Clone, PartialEq)]
struct SensorReading {
    name: String,
    /// E.g. `k10temp Tctl`.
    input: String,
    /// `None` when unreadable.
    celsius: Option<f64>,
}

/// What one fan would do at the current temperatures.
#[derive(Debug, Clone, PartialEq)]
struct FanReading {
    name: String,
    /// E.g. `nct6798 pwm2`.
    output: String,
    /// The first tick a fresh controller runs, as `run` would at startup.
    tick: Tick,
    /// Raw duty the output is at now, set by whoever controls it today.
    current_raw: u8,
}

/// Everything `check` prints after the config passed.
#[derive(Debug, Clone, PartialEq)]
struct Report {
    sensors: Vec<SensorReading>,
    fans: Vec<FanReading>,
}

/// Loads `config_path`, checks it against the chips under `hwmon_root` and
/// returns the rendered report.
///
/// # Errors
///
/// Fails when the file cannot be read, the config is invalid, it does not
/// match the hardware, or an output cannot be read.
pub fn run(config_path: &Path, hwmon_root: &Path) -> anyhow::Result<String> {
    let setup = Setup::load(config_path, hwmon_root)?;
    let report = read_report(&setup)?;
    Ok(render(config_path, &report))
}

/// Reads every sensor once and runs each fan's controller for one tick on
/// those readings, exactly as `run`'s first tick would.
fn read_report(setup: &Setup) -> anyhow::Result<Report> {
    let readings = binding::read_sensors(&setup.bindings.sensors);
    let sensors = setup
        .bindings
        .sensors
        .iter()
        .map(|(name, sensor)| SensorReading {
            name: name.clone(),
            input: sensor.describe(),
            celsius: readings[name],
        })
        .collect();

    let mut fans = Vec::new();
    for (name, fan) in &setup.config.fans {
        // `bind` succeeded, so every fan has an output.
        let output = &setup.bindings.outputs[name];
        fans.push(FanReading {
            name: name.clone(),
            output: output.describe(),
            tick: setup.config.controller(fan).tick(&readings),
            current_raw: output
                .read_raw()
                .with_context(|| format!("cannot read {}", output.describe()))?,
        });
    }
    Ok(Report { sensors, fans })
}

/// Renders the report under a line saying the config is valid.
fn render(config_path: &Path, report: &Report) -> String {
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
    let reading = sensor.celsius.map_or_else(
        || "unreadable".to_owned(),
        |celsius| format!("{celsius:>5.1} °C"),
    );
    vec![sensor.name.clone(), sensor.input.clone(), reading]
}

/// E.g. `cpu_cooler  nct6798 pwm2  cpu   49.1 °C  →   48 %  122/255  now  47 %  120/255`,
/// or with `failsafe  sensor gpu cannot be read  →` in place of source and °C.
fn fan_row(fan: &FanReading) -> Vec<String> {
    let tick = &fan.tick;
    let (source, reason) = match (&tick.hottest, &tick.failsafe) {
        (_, Some(failsafe)) => ("failsafe".to_owned(), format!("{failsafe}  →")),
        (Some((source, celsius)), None) => (source.clone(), format!("{celsius:>5.1} °C  →")),
        (None, None) => (String::new(), "→".to_owned()),
    };
    let raw = pwm::percent_to_raw(tick.duty);
    vec![
        fan.name.clone(),
        fan.output.clone(),
        source,
        reason,
        format!("{:>3.0} %  {raw:>3}/255", tick.duty),
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
    use svalka_core::controller::Failsafe;

    use super::*;

    fn sensor(name: &str, input: &str, celsius: Option<f64>) -> SensorReading {
        SensorReading {
            name: name.into(),
            input: input.into(),
            celsius,
        }
    }

    fn fan(name: &str, output: &str, tick: Tick, current_raw: u8) -> FanReading {
        FanReading {
            name: name.into(),
            output: output.into(),
            tick,
            current_raw,
        }
    }

    fn normal(source: &str, celsius: f64, duty: f64) -> Tick {
        Tick {
            duty,
            hottest: Some((source.into(), celsius)),
            smoothed: Some(celsius),
            effective: Some(celsius),
            failsafe: None,
        }
    }

    fn report() -> Report {
        Report {
            sensors: vec![
                sensor("cpu", "k10temp Tctl", Some(49.125)),
                sensor("gpu", "amdgpu junction", Some(47.0)),
            ],
            fans: vec![
                fan(
                    "cpu_cooler",
                    "nct6798 pwm2",
                    normal("cpu", 49.125, 47.656_25),
                    120,
                ),
                fan("exhaust", "nct6798 pwm1", normal("gpu", 47.0, 47.0), 102),
            ],
        }
    }

    #[test]
    fn renders_sensors_and_fans_in_columns() {
        let expected = "\
/etc/svalka.toml is valid.

Sensors
  cpu  k10temp Tctl      49.1 °C
  gpu  amdgpu junction   47.0 °C

Fans, at the current temperatures (no smoothing or hysteresis)
  cpu_cooler  nct6798 pwm2  cpu   49.1 °C  →   48 %  122/255  now  47 %  120/255
  exhaust     nct6798 pwm1  gpu   47.0 °C  →   47 %  120/255  now  40 %  102/255
";
        assert_eq!(render(Path::new("/etc/svalka.toml"), &report()), expected);
    }

    #[test]
    fn an_unreadable_sensor_shows_as_failsafe() {
        let report = Report {
            sensors: vec![sensor("gpu", "amdgpu junction", None)],
            fans: vec![fan(
                "exhaust",
                "nct6798 pwm1",
                Tick {
                    duty: 100.0,
                    hottest: None,
                    smoothed: None,
                    effective: None,
                    failsafe: Some(Failsafe::Unreadable {
                        source: "gpu".into(),
                    }),
                },
                102,
            )],
        };

        let text = render(Path::new("x.toml"), &report);

        assert!(
            text.contains("  gpu  amdgpu junction  unreadable\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "exhaust  nct6798 pwm1  failsafe  sensor gpu cannot be read  →  100 %  255/255"
            ),
            "{text}"
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
