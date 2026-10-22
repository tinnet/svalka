//! Tying a config to real hardware: each sensor to one temperature input,
//! each fan to one PWM output.
//!
//! This is where the config rules that need the hardware live: chips must
//! resolve, labels and outputs must exist, and every sensor must read a
//! plausible temperature right now.

use std::collections::BTreeMap;
use std::io;

use onlycurves_core::config::{Config, OutputRef, SensorRef};

use crate::hwmon::{self, Chip};

/// Labels of inputs that are often unconnected on consumer boards and then
/// report garbage, such as AUXTIN0 at 84 °C on the B550-I.
const FLOATING_LABEL_PREFIX: &str = "AUXTIN";

/// A config sensor tied to one temperature input.
#[derive(Debug, Clone)]
pub struct BoundSensor {
    chip: Chip,
    index: u32,
    label: String,
}

impl BoundSensor {
    /// Reads the temperature in °C.
    ///
    /// # Errors
    ///
    /// Fails when the input cannot be read.
    pub fn read_celsius(&self) -> io::Result<f64> {
        self.chip.read_temp(self.index)
    }

    /// E.g. `k10temp Tctl`.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("{} {}", self.chip.name(), self.label)
    }
}

/// A fan tied to one PWM output.
#[derive(Debug, Clone)]
pub struct BoundOutput {
    chip: Chip,
    pwm: u32,
}

impl BoundOutput {
    /// Reads the raw duty (0–255) the output is at now.
    ///
    /// # Errors
    ///
    /// Fails when the output cannot be read.
    pub fn read_raw(&self) -> io::Result<u8> {
        self.chip.read_pwm(self.pwm)
    }

    /// E.g. `nct6798 pwm2`.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("{} pwm{}", self.chip.name(), self.pwm)
    }

    /// The chip the output belongs to.
    #[must_use]
    pub fn chip(&self) -> &Chip {
        &self.chip
    }

    /// N in `pwmN`.
    #[must_use]
    pub fn pwm(&self) -> u32 {
        self.pwm
    }
}

/// Every sensor and fan output of a config, tied to hardware.
#[derive(Debug, Clone)]
pub struct Bindings {
    /// By sensor name.
    pub sensors: BTreeMap<String, BoundSensor>,
    /// By fan name.
    pub outputs: BTreeMap<String, BoundOutput>,
}

/// Ties every sensor and fan output in `config` to one of `chips`.
///
/// # Errors
///
/// Returns one message per sensor or output that cannot be bound.
pub fn bind(config: &Config, chips: &[Chip]) -> Result<Bindings, Vec<String>> {
    let mut problems = Vec::new();

    let mut sensors = BTreeMap::new();
    for (name, sensor) in &config.sensors {
        match bind_sensor(sensor, chips) {
            Ok(bound) => _ = sensors.insert(name.clone(), bound),
            Err(problem) => problems.push(format!("sensors.{name}: {problem}")),
        }
    }

    let mut outputs = BTreeMap::new();
    for (name, fan) in &config.fans {
        match bind_output(&fan.output, chips) {
            Ok(bound) => _ = outputs.insert(name.clone(), bound),
            Err(problem) => problems.push(format!("fan.{name}: output: {problem}")),
        }
    }

    if problems.is_empty() {
        Ok(Bindings { sensors, outputs })
    } else {
        Err(problems)
    }
}

fn bind_sensor(sensor: &SensorRef, chips: &[Chip]) -> Result<BoundSensor, String> {
    if sensor.label.starts_with(FLOATING_LABEL_PREFIX) {
        return Err(format!(
            "{FLOATING_LABEL_PREFIX}* inputs are often unconnected and report garbage, so {:?} cannot be a source",
            sensor.label
        ));
    }
    let chip = hwmon::resolve(chips, &sensor.chip, sensor.path.as_deref())
        .map_err(|error| error.to_string())?;
    let index = find_labelled_temp(chip, &sensor.label)?;
    let bound = BoundSensor {
        chip: chip.clone(),
        index,
        label: sensor.label.clone(),
    };
    check_plausible(&bound)?;
    Ok(bound)
}

/// N of the input labelled `label`, or a message listing the labels there are.
fn find_labelled_temp(chip: &Chip, label: &str) -> Result<u32, String> {
    let unreadable = |error: io::Error| format!("cannot read {}: {error}", chip.dir().display());
    if let Some(index) = chip.find_temp(label).map_err(unreadable)? {
        return Ok(index);
    }
    let labels = chip.temp_labels().map_err(unreadable)?;
    Err(format!(
        "{} has no temperature labelled {label:?}; it has: {}",
        chip.name(),
        labels.join(", ")
    ))
}

fn check_plausible(sensor: &BoundSensor) -> Result<(), String> {
    let celsius = sensor
        .read_celsius()
        .map_err(|error| format!("cannot read {}: {error}", sensor.describe()))?;
    if is_plausible(celsius) {
        Ok(())
    } else {
        Err(format!(
            "{} reads {celsius} °C, outside 0–125 °C; a missing probe or an unconnected input?",
            sensor.describe()
        ))
    }
}

/// Whether a working sensor could report `celsius`. A reading at or below 0 °C
/// or above 125 °C means a missing probe or an unconnected input.
fn is_plausible(celsius: f64) -> bool {
    celsius > 0.0 && celsius <= 125.0
}

fn bind_output(output: &OutputRef, chips: &[Chip]) -> Result<BoundOutput, String> {
    let chip = hwmon::resolve(chips, &output.chip, output.path.as_deref())
        .map_err(|error| error.to_string())?;
    let numbers = chip
        .pwm_numbers()
        .map_err(|error| format!("cannot read {}: {error}", chip.dir().display()))?;
    if !numbers.contains(&output.pwm) {
        let names: Vec<String> = numbers.iter().map(|n| format!("pwm{n}")).collect();
        return Err(format!(
            "{} has no pwm{}; it has: {}",
            chip.name(),
            output.pwm,
            names.join(", ")
        ));
    }
    Ok(BoundOutput {
        chip: chip.clone(),
        pwm: output.pwm,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwmon::{discover, testing::FakeSysfs};

    /// This PC's chips, as far as the config below needs them.
    fn this_pc() -> FakeSysfs {
        let sysfs = FakeSysfs::new();
        sysfs
            .chip("hwmon3", "k10temp", "pci0000:00/0000:00:18.3")
            .attr("temp1_input", "49125")
            .attr("temp1_label", "Tctl");
        sysfs
            .chip("hwmon1", "amdgpu", "pci0000:00/0000:0a:00.0")
            .attr("temp2_input", "47000")
            .attr("temp2_label", "junction");
        sysfs
            .chip("hwmon7", "nct6798", "platform/nct6775.656")
            .attr("temp3_input", "84000")
            .attr("temp3_label", "AUXTIN0")
            .attr("temp4_input", "-40000")
            .attr("temp4_label", "T_Sensor")
            .attr("pwm1", "102")
            .attr("pwm2", "120");
        sysfs
    }

    fn config(sensors: &str, fans: &str) -> Config {
        Config::parse(&format!("[sensors]\n{sensors}\n{fans}")).unwrap()
    }

    const CPU_FAN: &str = r#"
[fan.cpu_cooler]
output  = { chip = "nct6798", pwm = 2 }
sources = ["cpu"]
curve   = [[35, 30], [85, 100]]
"#;

    fn bind_problems(config: &Config, sysfs: &FakeSysfs) -> Vec<String> {
        let chips = discover(&sysfs.hwmon_root()).unwrap();
        bind(config, &chips).unwrap_err()
    }

    #[test]
    fn binds_sensors_and_outputs() {
        let sysfs = this_pc();
        let chips = discover(&sysfs.hwmon_root()).unwrap();
        let config = config(r#"cpu = { chip = "k10temp", label = "Tctl" }"#, CPU_FAN);

        let bindings = bind(&config, &chips).unwrap();

        let cpu = &bindings.sensors["cpu"];
        assert_eq!(cpu.describe(), "k10temp Tctl");
        assert!((cpu.read_celsius().unwrap() - 49.125).abs() < 1e-9);
        let output = &bindings.outputs["cpu_cooler"];
        assert_eq!(output.describe(), "nct6798 pwm2");
        assert_eq!(output.read_raw().unwrap(), 120);
    }

    #[test]
    fn reports_unknown_chips_labels_and_outputs() {
        let sysfs = this_pc();
        let config = config(
            r#"
cpu = { chip = "k10temp", label = "Tcl" }
gpu = { chip = "radeon", label = "junction" }
"#,
            &CPU_FAN.replace("pwm = 2", "pwm = 9"),
        );

        assert_eq!(
            bind_problems(&config, &sysfs),
            [
                r#"sensors.cpu: k10temp has no temperature labelled "Tcl"; it has: Tctl"#,
                r#"sensors.gpu: no hwmon chip named "radeon""#,
                "fan.cpu_cooler: output: nct6798 has no pwm9; it has: pwm1, pwm2",
            ]
        );
    }

    #[test]
    fn rejects_floating_and_implausible_inputs() {
        let sysfs = this_pc();
        let config = config(
            r#"
cpu   = { chip = "k10temp", label = "Tctl" }
aux   = { chip = "nct6798", label = "AUXTIN0" }
probe = { chip = "nct6798", label = "T_Sensor" }
"#,
            CPU_FAN,
        );

        assert_eq!(
            bind_problems(&config, &sysfs),
            [
                r#"sensors.aux: AUXTIN* inputs are often unconnected and report garbage, so "AUXTIN0" cannot be a source"#,
                "sensors.probe: nct6798 T_Sensor reads -40 °C, outside 0–125 °C; a missing probe or an unconnected input?",
            ]
        );
    }
}
