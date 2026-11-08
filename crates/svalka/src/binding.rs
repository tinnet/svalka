//! Tying a config to real hardware: each sensor to one temperature input,
//! each fan to one PWM output.
//!
//! This is where the config rules that need the hardware live: chips must
//! resolve, labels and outputs must exist, and every sensor must read a
//! plausible temperature right now.

use std::collections::BTreeMap;
use std::io;

use svalka_core::config::{Config, OutputRef, SensorRef};

use anyhow::Context;

use crate::hwmon::{self, Chip, HandBack, ManualOverride, PwmEnable, ResolveError};

/// Labels of inputs that are often unconnected on consumer boards and then
/// report garbage, such as an AUXTIN reading 84 °C with nothing attached.
const FLOATING_LABEL_PREFIX: &str = "AUXTIN";

/// A config sensor tied to one temperature input.
#[derive(Debug, Clone)]
pub struct BoundSensor {
    chip: Chip,
    index: u32,
    /// What the config asked for, to find the input again if its chip moves.
    source: SensorRef,
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

    /// Reads the temperature in °C, looking the chip up again first if the read
    /// fails: a reloaded driver (`modprobe -r k10temp; modprobe k10temp`) can
    /// come back as another `hwmonN`, and the old directory never returns.
    ///
    /// # Errors
    ///
    /// Fails when the input cannot be read, before or after the lookup.
    pub fn read_or_find_again(&mut self) -> io::Result<f64> {
        self.read_celsius().or_else(|error| {
            if self.find_again() {
                self.read_celsius()
            } else {
                Err(error)
            }
        })
    }

    /// Binds to the chip that now matches the config, under the hwmon root the
    /// old one was found in. Keeps the old binding when nothing matches, so the
    /// next read tries again.
    fn find_again(&mut self) -> bool {
        let Some(chips) = self
            .chip
            .dir()
            .parent()
            .and_then(|hwmon_root| hwmon::discover(hwmon_root).ok())
        else {
            return false;
        };
        let Ok(chip) = hwmon::resolve(&chips, &self.source.chip, self.source.path.as_deref())
        else {
            return false;
        };
        let Ok(index) = find_labelled_temp(chip, &self.source.label) else {
            return false;
        };
        self.chip = chip.clone();
        self.index = index;
        true
    }

    /// E.g. `k10temp Tctl`.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("{} {}", self.chip.name(), self.source.label)
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
        self.chip.describe_pwm(self.pwm)
    }

    /// The fan's speed from the tachometer with the same number (`fanN` for
    /// `pwmN`, as on nct6775-family boards; `identify` shows the pairing), or
    /// `None` without one.
    ///
    /// # Errors
    ///
    /// Fails when the tachometer exists but cannot be read.
    pub fn read_rpm(&self) -> io::Result<Option<u32>> {
        self.chip.read_fan(self.pwm)
    }

    /// Whether `other` is the same physical output.
    #[must_use]
    pub fn same_output(&self, other: &BoundOutput) -> bool {
        self.chip.dir() == other.chip.dir() && self.pwm == other.pwm
    }

    /// Takes the output into manual mode (see [`ManualOverride`]).
    ///
    /// # Errors
    ///
    /// Fails when the output cannot be read or switched to manual.
    pub fn take_over(&self, hand_back: HandBack) -> anyhow::Result<ManualOverride> {
        ManualOverride::take(&self.chip, self.pwm, hand_back)
            .with_context(|| format!("cannot take over {}", self.describe()))
    }

    /// The mode that hands this output back to the chip's own curve.
    ///
    /// # Errors
    ///
    /// Fails for chips whose automatic mode svalka does not know.
    pub fn automatic_mode(&self) -> anyhow::Result<PwmEnable> {
        self.chip.automatic_mode().ok_or_else(|| {
            anyhow::anyhow!(
                "{}: svalka does not know this chip's automatic mode, so it could not \
                 hand the fan back; only chips under the nct6775 driver are supported",
                self.describe()
            )
        })
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

/// Every sensor's temperature by name, or `None` when it cannot be read. A
/// sensor whose chip moved is bound to it again (see
/// [`BoundSensor::read_or_find_again`]).
pub fn read_sensors(sensors: &mut BTreeMap<String, BoundSensor>) -> BTreeMap<String, Option<f64>> {
    sensors
        .iter_mut()
        .map(|(name, sensor)| (name.clone(), sensor.read_or_find_again().ok()))
        .collect()
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
    let outputs = bind_outputs(config, chips).unwrap_or_else(|output_problems| {
        problems.extend(output_problems);
        BTreeMap::new()
    });
    problems.extend(shared_outputs(&outputs));

    if problems.is_empty() {
        Ok(Bindings { sensors, outputs })
    } else {
        Err(problems)
    }
}

/// Ties only the fan outputs, by fan name, and checks nothing else.
fn bind_outputs(
    config: &Config,
    chips: &[Chip],
) -> Result<BTreeMap<String, BoundOutput>, Vec<String>> {
    let mut problems = Vec::new();
    let mut outputs = BTreeMap::new();
    for (name, fan) in &config.fans {
        match bind_output(&fan.output, chips) {
            Ok(bound) => _ = outputs.insert(name.clone(), bound),
            Err(problem) => problems.push(format!("fan.{name}: output: {problem}")),
        }
    }
    if problems.is_empty() {
        Ok(outputs)
    } else {
        Err(problems)
    }
}

/// The config's fan outputs as `restore` sees them.
#[derive(Debug, Default)]
pub struct PresentOutputs {
    pub bound: Vec<BoundOutput>,
    /// Fan names and why their chip was not found.
    pub gone: Vec<(String, ResolveError)>,
}

/// Ties the fan outputs and checks nothing else, so that no other rule can stop
/// `restore` from handing the fans back. A chip that is not there is kept apart
/// from real problems: its output has nothing left to hand back, and it must not
/// stop the other fans. This happens during shutdown.
///
/// # Errors
///
/// Returns one message per output that cannot be bound though its chip may exist.
pub fn bind_present_outputs(
    config: &Config,
    chips: &[Chip],
) -> Result<PresentOutputs, Vec<String>> {
    let mut problems = Vec::new();
    let mut present = PresentOutputs::default();
    for (name, fan) in &config.fans {
        let output = &fan.output;
        if let Err(error @ ResolveError::NotFound { .. }) =
            hwmon::resolve(chips, &output.chip, output.path.as_deref())
        {
            present.gone.push((name.clone(), error));
            continue;
        }
        match bind_output(output, chips) {
            Ok(bound) => present.bound.push(bound),
            Err(problem) => problems.push(format!("fan.{name}: output: {problem}")),
        }
    }
    if problems.is_empty() {
        Ok(present)
    } else {
        Err(problems)
    }
}

/// Two fans on one physical output would fight over it. Compared after the
/// chips are resolved, so every way of writing the same output is caught.
fn shared_outputs(outputs: &BTreeMap<String, BoundOutput>) -> Vec<String> {
    let mut problems = Vec::new();
    let named: Vec<(&String, &BoundOutput)> = outputs.iter().collect();
    for (index, (first, output)) in named.iter().enumerate() {
        for (second, other) in &named[index + 1..] {
            if output.same_output(other) {
                problems.push(format!(
                    "fan.{first} and fan.{second} both drive {}",
                    output.describe()
                ));
            }
        }
    }
    problems
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
        source: sensor.clone(),
    };
    check_plausible(&bound)?;
    Ok(bound)
}

/// N of the input labelled `label`, or a message listing the labels there are.
fn find_labelled_temp(chip: &Chip, label: &str) -> Result<u32, String> {
    let inputs = chip.temp_inputs().map_err(unreadable(chip))?;
    if let Some(&(n, _)) = inputs.iter().find(|(_, input)| input == label) {
        return Ok(n);
    }
    let labels: Vec<&str> = inputs.iter().map(|(_, input)| input.as_str()).collect();
    Err(format!(
        "{} has no temperature labelled {label:?}; it has: {}",
        chip.name(),
        labels.join(", ")
    ))
}

/// Turns an I/O error on `chip` into a problem message.
fn unreadable(chip: &Chip) -> impl Fn(io::Error) -> String + '_ {
    move |error| format!("cannot read {}: {error}", chip.dir().display())
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
    let numbers = chip.pwm_numbers().map_err(unreadable(chip))?;
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

    /// A fake PC's chips, as far as the config below needs them.
    fn example_pc() -> FakeSysfs {
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
        let sysfs = example_pc();
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
        let sysfs = example_pc();
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
    fn two_spellings_of_one_output_are_one_output() {
        let sysfs = example_pc();
        let second_fan = CPU_FAN.replace("[fan.cpu_cooler]", "[fan.copy]").replace(
            r#"chip = "nct6798", pwm = 2"#,
            r#"chip = "nct6798", path = "nct6775.656", pwm = 2"#,
        );
        let config = config(
            r#"cpu = { chip = "k10temp", label = "Tctl" }"#,
            &format!("{CPU_FAN}\n{second_fan}"),
        );

        assert_eq!(
            bind_problems(&config, &sysfs),
            ["fan.copy and fan.cpu_cooler both drive nct6798 pwm2"]
        );
    }

    #[test]
    fn rejects_floating_and_implausible_inputs() {
        let sysfs = example_pc();
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
