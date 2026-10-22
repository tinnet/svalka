//! Reading a chip's temperatures, fan speeds and PWM outputs.
//!
//! Attribute names follow the kernel's hwmon sysfs ABI:
//! <https://docs.kernel.org/hwmon/sysfs-interface.html>

use std::fmt;
use std::fs;
use std::io;

use super::Chip;
use crate::sysfs;

/// A temperature input, `tempN_input`.
#[derive(Debug, Clone, PartialEq)]
pub struct Temp {
    pub label: String,
    /// `None` when the input could not be read.
    pub celsius: Option<f64>,
}

/// A fan tachometer input, `fanN_input`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fan {
    pub label: String,
    /// `None` when the input could not be read.
    pub rpm: Option<u32>,
}

/// A PWM output, `pwmN`, with its control mode `pwmN_enable`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pwm {
    pub label: String,
    /// Raw duty, 0–255. `None` when it could not be read.
    pub duty: Option<u8>,
    /// `None` when the chip has no `pwmN_enable` or it could not be read.
    pub enable: Option<PwmEnable>,
}

impl Pwm {
    /// The duty as a percentage, rounded to the nearest whole percent.
    pub fn duty_percent(&self) -> Option<u8> {
        self.duty.map(raw_duty_to_percent)
    }
}

/// Who controls a PWM output, from `pwmN_enable`.
///
/// 0 and 1 mean the same on every driver. What 2 and above mean depends on the
/// driver (on the nct6775 family, 5 is SmartFan IV), so they keep their number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PwmEnable {
    /// 0: no control, the fan runs at full speed.
    FullSpeed,
    /// 1: software sets the duty by writing `pwmN`.
    Manual,
    /// 2 and above: the chip or firmware runs its own control mode.
    Automatic(u8),
}

impl From<u8> for PwmEnable {
    fn from(raw: u8) -> Self {
        match raw {
            0 => Self::FullSpeed,
            1 => Self::Manual,
            mode => Self::Automatic(mode),
        }
    }
}

impl fmt::Display for PwmEnable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FullSpeed => write!(f, "full speed (0)"),
            Self::Manual => write!(f, "manual (1)"),
            Self::Automatic(mode) => write!(f, "automatic ({mode})"),
        }
    }
}

/// Everything a chip reported at one moment.
#[derive(Debug, Clone, PartialEq)]
pub struct ChipSnapshot {
    pub name: String,
    pub device_id: Option<String>,
    pub hwmon_number: String,
    pub temps: Vec<Temp>,
    pub fans: Vec<Fan>,
    pub pwms: Vec<Pwm>,
}

impl ChipSnapshot {
    /// Whether the chip has no temperatures, fans or PWM outputs at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.temps.is_empty() && self.fans.is_empty() && self.pwms.is_empty()
    }
}

impl Chip {
    /// Reads every temperature, fan and PWM output of the chip.
    ///
    /// A single unreadable value does not fail the snapshot; it shows up as `None`.
    ///
    /// # Errors
    ///
    /// Fails when the chip's directory cannot be listed.
    pub fn snapshot(&self) -> io::Result<ChipSnapshot> {
        Ok(ChipSnapshot {
            name: self.name().to_owned(),
            device_id: self.device_id(),
            hwmon_number: self.hwmon_number(),
            temps: self.read_temps()?,
            fans: self.read_fans()?,
            pwms: self.read_pwms()?,
        })
    }

    /// Reads every `tempN_input`, sorted by N.
    ///
    /// # Errors
    ///
    /// Fails when the chip's directory cannot be listed.
    pub fn read_temps(&self) -> io::Result<Vec<Temp>> {
        let temps = self
            .attribute_indices("temp", "_input")?
            .into_iter()
            .map(|n| Temp {
                label: self.label_or(&format!("temp{n}_label"), &format!("temp{n}")),
                celsius: self
                    .read_attr::<i64>(&format!("temp{n}_input"))
                    .map(millidegrees_to_celsius),
            })
            .collect();
        Ok(temps)
    }

    /// Reads every `fanN_input`, sorted by N.
    ///
    /// # Errors
    ///
    /// Fails when the chip's directory cannot be listed.
    pub fn read_fans(&self) -> io::Result<Vec<Fan>> {
        let fans = self
            .attribute_indices("fan", "_input")?
            .into_iter()
            .map(|n| Fan {
                label: self.label_or(&format!("fan{n}_label"), &format!("fan{n}")),
                rpm: self.read_attr(&format!("fan{n}_input")),
            })
            .collect();
        Ok(fans)
    }

    /// Reads every `pwmN` with its `pwmN_enable`, sorted by N.
    ///
    /// # Errors
    ///
    /// Fails when the chip's directory cannot be listed.
    pub fn read_pwms(&self) -> io::Result<Vec<Pwm>> {
        let pwms = self
            .attribute_indices("pwm", "")?
            .into_iter()
            .map(|n| Pwm {
                label: format!("pwm{n}"),
                duty: self.read_attr(&format!("pwm{n}")),
                enable: self
                    .read_attr::<u8>(&format!("pwm{n}_enable"))
                    .map(PwmEnable::from),
            })
            .collect();
        Ok(pwms)
    }

    /// The sorted indices N of every attribute named `{prefix}N{suffix}`.
    fn attribute_indices(&self, prefix: &str, suffix: &str) -> io::Result<Vec<u32>> {
        let mut indices = Vec::new();
        for entry in fs::read_dir(self.dir())? {
            let file_name = entry?.file_name();
            if let Some(n) = sysfs::attribute_index(&file_name.to_string_lossy(), prefix, suffix) {
                indices.push(n);
            }
        }
        indices.sort_unstable();
        Ok(indices)
    }

    /// Reads and parses one attribute; `None` if it is missing or unreadable.
    fn read_attr<T: std::str::FromStr>(&self, file: &str) -> Option<T> {
        sysfs::read_value(&self.dir().join(file)).ok()
    }

    /// The text of a `…_label` attribute, or `fallback` when the chip has none.
    fn label_or(&self, label_file: &str, fallback: &str) -> String {
        sysfs::read_string(&self.dir().join(label_file)).unwrap_or_else(|_| fallback.to_owned())
    }
}

#[allow(clippy::cast_precision_loss)] // sensor values are far below 2^52
fn millidegrees_to_celsius(millidegrees: i64) -> f64 {
    millidegrees as f64 / 1000.0
}

fn raw_duty_to_percent(raw: u8) -> u8 {
    let percent = (u32::from(raw) * 100 + 127) / 255;
    u8::try_from(percent).expect("0–255 scales to at most 100")
}

#[cfg(test)]
mod tests {
    use super::super::testing::FakeSysfs;
    use super::*;

    fn nct6798(sysfs: &FakeSysfs) -> Chip {
        let fake = sysfs.chip("hwmon7", "nct6798", "platform/nct6775.656");
        fake.attr("temp1_input", "34000")
            .attr("temp1_label", "SYSTIN")
            .attr("temp2_input", "49125")
            .attr("fan2_input", "1220")
            .attr("fan1_input", "971")
            .attr("pwm1", "102")
            .attr("pwm1_enable", "1")
            .attr("pwm1_mode", "1")
            .attr("pwm5", "0")
            .attr("pwm5_enable", "5");
        Chip::open(fake.dir()).unwrap()
    }

    #[test]
    fn temps_are_in_celsius_with_labels_or_fallback_names() {
        let sysfs = FakeSysfs::new();
        let temps = nct6798(&sysfs).read_temps().unwrap();

        assert_eq!(
            temps,
            [
                Temp {
                    label: "SYSTIN".into(),
                    celsius: Some(34.0)
                },
                Temp {
                    label: "temp2".into(),
                    celsius: Some(49.125)
                },
            ]
        );
    }

    #[test]
    fn fans_are_sorted_by_index() {
        let sysfs = FakeSysfs::new();
        let fans = nct6798(&sysfs).read_fans().unwrap();

        assert_eq!(
            fans,
            [
                Fan {
                    label: "fan1".into(),
                    rpm: Some(971)
                },
                Fan {
                    label: "fan2".into(),
                    rpm: Some(1220)
                },
            ]
        );
    }

    #[test]
    fn pwms_ignore_their_enable_and_mode_files() {
        let sysfs = FakeSysfs::new();
        let pwms = nct6798(&sysfs).read_pwms().unwrap();

        assert_eq!(
            pwms,
            [
                Pwm {
                    label: "pwm1".into(),
                    duty: Some(102),
                    enable: Some(PwmEnable::Manual)
                },
                Pwm {
                    label: "pwm5".into(),
                    duty: Some(0),
                    enable: Some(PwmEnable::Automatic(5))
                },
            ]
        );
    }

    #[test]
    fn unreadable_values_become_none() {
        let sysfs = FakeSysfs::new();
        let fake = sysfs.chip("hwmon1", "amdgpu", "pci0000:00/0000:0a:00.0");
        fake.attr("temp1_input", "garbage").attr("pwm1", "0");
        let chip = Chip::open(fake.dir()).unwrap();

        assert_eq!(chip.read_temps().unwrap()[0].celsius, None);
        assert_eq!(chip.read_pwms().unwrap()[0].enable, None);
    }

    #[test]
    fn duty_percent_rounds_to_nearest() {
        let pwm = |duty| Pwm {
            label: "pwm1".into(),
            duty: Some(duty),
            enable: None,
        };

        assert_eq!(pwm(0).duty_percent(), Some(0));
        assert_eq!(pwm(102).duty_percent(), Some(40));
        assert_eq!(pwm(120).duty_percent(), Some(47));
        assert_eq!(pwm(255).duty_percent(), Some(100));
    }

    #[test]
    fn pwm_enable_names_the_shared_modes() {
        assert_eq!(PwmEnable::from(0), PwmEnable::FullSpeed);
        assert_eq!(PwmEnable::from(1), PwmEnable::Manual);
        assert_eq!(PwmEnable::from(5), PwmEnable::Automatic(5));
        assert_eq!(PwmEnable::Automatic(5).to_string(), "automatic (5)");
    }
}
