//! Taking manual control of a PWM output and handing it back.
//!
//! Writing needs root. `pwmN_enable = 1` puts an output in manual mode, after
//! which writes to `pwmN` set its duty directly.

use std::io;

use super::{Chip, PwmEnable};
use crate::sysfs;

impl Chip {
    /// Reads `pwmN_enable`, or `None` when the chip has no such attribute.
    ///
    /// # Errors
    ///
    /// Fails when the attribute exists but cannot be read.
    pub fn read_pwm_enable(&self, n: u32) -> io::Result<Option<u8>> {
        let path = self.dir().join(format!("pwm{n}_enable"));
        if !path.exists() {
            return Ok(None);
        }
        sysfs::read_value(&path).map(Some)
    }

    /// Writes `pwmN_enable`.
    ///
    /// # Errors
    ///
    /// Fails when the write is refused, e.g. without root.
    pub fn write_pwm_enable(&self, n: u32, mode: u8) -> io::Result<()> {
        sysfs::write_value(&self.dir().join(format!("pwm{n}_enable")), mode)
    }

    /// Writes the raw duty (0–255) of `pwmN`.
    ///
    /// # Errors
    ///
    /// Fails when the write is refused, e.g. without root.
    pub fn write_pwm(&self, n: u32, raw: u8) -> io::Result<()> {
        sysfs::write_value(&self.dir().join(format!("pwm{n}")), raw)
    }
}

/// What an output was set to before we took it over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PwmState {
    pub raw: u8,
    /// `None` when the chip has no `pwmN_enable`.
    pub enable: Option<u8>,
}

impl std::fmt::Display for PwmState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/255", self.raw)?;
        if let Some(enable) = self.enable {
            write!(f, ", {}", PwmEnable::from(enable))?;
        }
        Ok(())
    }
}

/// How a [`ManualOverride`] hands its output back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandBack {
    /// Write back the duty and mode the output had before, as `identify` does.
    Saved,
    /// Switch to this `pwmN_enable` mode, letting the chip's own curve run the
    /// fan, as `run` does on exit (5 = SmartFan IV on the nct6775 family).
    Automatic(u8),
}

/// Manual control of one PWM output that always hands the output back.
///
/// [`ManualOverride::take`] saves the output's duty and mode and switches it
/// to manual. [`ManualOverride::hand_back`] returns it as [`HandBack`] says.
/// If the override is dropped without that (an early return or a panic),
/// `Drop` hands it back instead, so the fan never stays at whatever we set last.
#[derive(Debug)]
pub struct ManualOverride {
    chip: Chip,
    pwm: u32,
    saved: PwmState,
    hand_back: HandBack,
    handed_back: bool,
}

impl ManualOverride {
    /// Saves `pwmN`'s state and switches it to manual mode.
    ///
    /// # Errors
    ///
    /// Fails when the state cannot be read or manual mode cannot be set. On
    /// failure the output is left as it was.
    pub fn take(chip: &Chip, pwm: u32, hand_back: HandBack) -> io::Result<Self> {
        let saved = PwmState {
            raw: chip.read_pwm(pwm)?,
            enable: chip.read_pwm_enable(pwm)?,
        };
        if saved.enable.is_some() {
            chip.write_pwm_enable(pwm, MANUAL)?;
        }
        Ok(Self {
            chip: chip.clone(),
            pwm,
            saved,
            hand_back,
            handed_back: false,
        })
    }

    /// The state the output had before [`ManualOverride::take`].
    #[must_use]
    pub fn saved(&self) -> PwmState {
        self.saved
    }

    /// Sets the raw duty (0–255).
    ///
    /// # Errors
    ///
    /// Fails when the write is refused.
    pub fn set(&self, raw: u8) -> io::Result<()> {
        self.chip.write_pwm(self.pwm, raw)
    }

    /// Switches the output back to manual and sets `raw`, after something else
    /// (a resume from suspend, another program) took it over.
    ///
    /// # Errors
    ///
    /// Fails when a write is refused.
    pub fn reassert(&self, raw: u8) -> io::Result<()> {
        if self.saved.enable.is_some() {
            self.chip.write_pwm_enable(self.pwm, MANUAL)?;
        }
        self.set(raw)
    }

    /// Reads the output's duty and mode as they are now.
    ///
    /// # Errors
    ///
    /// Fails when the output cannot be read.
    pub fn current(&self) -> io::Result<PwmState> {
        Ok(PwmState {
            raw: self.chip.read_pwm(self.pwm)?,
            enable: self.chip.read_pwm_enable(self.pwm)?,
        })
    }

    /// Hands the output back as chosen in [`ManualOverride::take`].
    ///
    /// # Errors
    ///
    /// Fails when a write is refused. `Drop` will not try again.
    pub fn hand_back(mut self) -> io::Result<()> {
        self.handed_back = true;
        self.write_hand_back()
    }

    fn write_hand_back(&self) -> io::Result<()> {
        match self.hand_back {
            HandBack::Saved => self.write_saved(),
            HandBack::Automatic(mode) => self.chip.write_pwm_enable(self.pwm, mode),
        }
    }

    /// Duty first, while still in manual mode, then the mode: some drivers
    /// ignore duty writes once the chip controls the output again.
    fn write_saved(&self) -> io::Result<()> {
        self.chip.write_pwm(self.pwm, self.saved.raw)?;
        if let Some(enable) = self.saved.enable {
            self.chip.write_pwm_enable(self.pwm, enable)?;
        }
        Ok(())
    }
}

impl Drop for ManualOverride {
    fn drop(&mut self) {
        if !self.handed_back
            && let Err(error) = self.write_hand_back()
        {
            eprintln!(
                "onlycurves: could not hand back {} pwm{}: {error}",
                self.chip.name(),
                self.pwm
            );
        }
    }
}

/// Whether `state` is still what we set: duty `raw` in manual mode, i.e.
/// nothing else has written to the output since we did.
///
/// The nct6775 driver family reports a manual output at exactly 255 as
/// `pwmN_enable = 0` (full speed), so at 255 both 0 and 1 mean manual.
#[must_use]
pub fn is_held(state: PwmState, raw: u8) -> bool {
    let manual = match state.enable {
        None | Some(MANUAL) => true,
        Some(FULL_SPEED) => raw == u8::MAX,
        Some(_) => false,
    };
    state.raw == raw && manual
}

/// `pwmN_enable` value for "no control, full speed".
const FULL_SPEED: u8 = 0;

/// `pwmN_enable` value for manual control.
const MANUAL: u8 = 1;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwmon::testing::FakeSysfs;

    fn nct6798(sysfs: &FakeSysfs, enable: u8) -> Chip {
        let fake = sysfs.chip("hwmon7", "nct6798", "platform/nct6775.656");
        fake.attr("pwm1", "74")
            .attr("pwm1_enable", &enable.to_string());
        Chip::open(fake.dir()).unwrap()
    }

    fn state(chip: &Chip) -> PwmState {
        PwmState {
            raw: chip.read_pwm(1).unwrap(),
            enable: chip.read_pwm_enable(1).unwrap(),
        }
    }

    #[test]
    fn take_switches_to_manual_and_hand_back_restores_everything() {
        let sysfs = FakeSysfs::new();
        let chip = nct6798(&sysfs, 5);

        let manual = ManualOverride::take(&chip, 1, HandBack::Saved).unwrap();
        manual.set(255).unwrap();
        assert_eq!(
            state(&chip),
            PwmState {
                raw: 255,
                enable: Some(1)
            }
        );
        assert!(is_held(manual.current().unwrap(), 255));

        manual.hand_back().unwrap();
        assert_eq!(
            state(&chip),
            PwmState {
                raw: 74,
                enable: Some(5)
            }
        );
    }

    #[test]
    fn dropping_without_hand_back_still_hands_back() {
        let sysfs = FakeSysfs::new();
        let chip = nct6798(&sysfs, 1);

        {
            let manual = ManualOverride::take(&chip, 1, HandBack::Saved).unwrap();
            manual.set(255).unwrap();
        }

        assert_eq!(
            state(&chip),
            PwmState {
                raw: 74,
                enable: Some(1)
            }
        );
    }

    #[test]
    fn is_held_notices_other_writers() {
        let state = |raw, enable| PwmState {
            raw,
            enable: Some(enable),
        };

        assert!(is_held(state(120, 1), 120));
        assert!(!is_held(state(98, 1), 120)); // duty changed
        assert!(!is_held(state(120, 5), 120)); // chip took control back
    }

    #[test]
    fn is_held_accepts_full_speed_at_255() {
        // nct6775 reports manual mode at 255 as pwmN_enable = 0.
        let full = PwmState {
            raw: 255,
            enable: Some(0),
        };

        assert!(is_held(full, 255));
        assert!(!is_held(
            PwmState {
                raw: 120,
                enable: Some(0)
            },
            120
        ));
    }

    #[test]
    fn outputs_without_enable_are_left_without_one() {
        let sysfs = FakeSysfs::new();
        let fake = sysfs.chip("hwmon1", "amdgpu", "pci0000:00/0000:0a:00.0");
        fake.attr("pwm1", "0");
        let chip = Chip::open(fake.dir()).unwrap();

        let manual = ManualOverride::take(&chip, 1, HandBack::Saved).unwrap();
        manual.set(255).unwrap();
        manual.hand_back().unwrap();

        assert_eq!(
            state(&chip),
            PwmState {
                raw: 0,
                enable: None
            }
        );
    }

    #[test]
    fn automatic_hand_back_switches_to_the_chips_own_mode() {
        let sysfs = FakeSysfs::new();
        let chip = nct6798(&sysfs, 1);

        let manual = ManualOverride::take(&chip, 1, HandBack::Automatic(5)).unwrap();
        manual.set(200).unwrap();
        manual.hand_back().unwrap();

        assert_eq!(
            state(&chip),
            PwmState {
                raw: 200,
                enable: Some(5)
            }
        );
    }

    #[test]
    fn reassert_takes_the_output_back() {
        let sysfs = FakeSysfs::new();
        let chip = nct6798(&sysfs, 5);
        let manual = ManualOverride::take(&chip, 1, HandBack::Saved).unwrap();
        chip.write_pwm_enable(1, 5).unwrap(); // e.g. the BIOS after resume

        manual.reassert(150).unwrap();

        assert_eq!(
            state(&chip),
            PwmState {
                raw: 150,
                enable: Some(1)
            }
        );
    }

    #[test]
    fn state_displays_duty_and_mode() {
        let state = PwmState {
            raw: 74,
            enable: Some(5),
        };

        assert_eq!(state.to_string(), "74/255, automatic (5)");
    }
}
