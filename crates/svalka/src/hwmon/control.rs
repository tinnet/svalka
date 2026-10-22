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
    pub fn read_pwm_enable(&self, n: u32) -> io::Result<Option<PwmEnable>> {
        let mode: Option<u8> = sysfs::read_optional(&self.dir().join(format!("pwm{n}_enable")))?;
        Ok(mode.map(PwmEnable::from))
    }

    /// Reads `pwmN` and `pwmN_enable` together.
    ///
    /// # Errors
    ///
    /// Fails when either cannot be read.
    pub fn read_pwm_state(&self, n: u32) -> io::Result<PwmState> {
        Ok(PwmState {
            raw: self.read_pwm(n)?,
            enable: self.read_pwm_enable(n)?,
        })
    }

    /// Writes `pwmN_enable`.
    ///
    /// # Errors
    ///
    /// Fails when the write is refused, e.g. without root.
    pub fn write_pwm_enable(&self, n: u32, mode: PwmEnable) -> io::Result<()> {
        sysfs::write_value(&self.dir().join(format!("pwm{n}_enable")), u8::from(mode))
    }

    /// Writes the raw duty (0–255) of `pwmN`.
    ///
    /// # Errors
    ///
    /// Fails when the write is refused, e.g. without root.
    pub fn write_pwm(&self, n: u32, raw: u8) -> io::Result<()> {
        sysfs::write_value(&self.dir().join(format!("pwm{n}")), raw)
    }

    /// The `pwmN_enable` mode in which the chip runs its own fan curve, for
    /// the kernel drivers where it is known: SmartFan IV (5) under nct6775,
    /// which serves nct6106 … nct6799. Keyed on the driver, not the chip name:
    /// what the modes mean is up to the driver, and nct6683-family chips share
    /// the name prefix but not the driver.
    #[must_use]
    pub fn automatic_mode(&self) -> Option<PwmEnable> {
        match self.driver()? {
            "nct6775" => Some(PwmEnable::Automatic(5)),
            _ => None,
        }
    }
}

/// An output's duty and mode at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PwmState {
    pub raw: u8,
    /// `None` when the chip has no `pwmN_enable`.
    pub enable: Option<PwmEnable>,
}

impl PwmState {
    /// Whether this is still what we set: duty `raw` in manual mode, i.e.
    /// nothing else has written to the output since we did.
    ///
    /// The nct6775 driver reports a manual output at exactly 255 as
    /// `pwmN_enable = 0` (full speed). It recomputes that on its next register
    /// refresh, not at our write, so reading back right after writing cannot
    /// tell us what to expect: at 255 both values mean manual.
    #[must_use]
    pub fn is_held_at(&self, raw: u8) -> bool {
        let manual = match self.enable {
            None | Some(PwmEnable::Manual) => true,
            Some(PwmEnable::FullSpeed) => raw == u8::MAX,
            Some(PwmEnable::Automatic(_)) => false,
        };
        self.raw == raw && manual
    }
}

impl std::fmt::Display for PwmState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/255", self.raw)?;
        if let Some(enable) = self.enable {
            write!(f, ", {enable}")?;
        }
        Ok(())
    }
}

/// How a [`ManualOverride`] hands its output back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandBack {
    /// Write back the duty and mode the output had before, as `identify` does.
    Saved,
    /// Switch to this mode, letting the chip's own curve run the fan, as `run`
    /// does on exit (see [`Chip::automatic_mode`]).
    Automatic(PwmEnable),
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
        let saved = chip.read_pwm_state(pwm)?;
        switch_to_manual(chip, pwm, saved)?;
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
        switch_to_manual(&self.chip, self.pwm, self.saved)?;
        self.set(raw)
    }

    /// Reads the output's duty and mode as they are now.
    ///
    /// # Errors
    ///
    /// Fails when the output cannot be read.
    pub fn current(&self) -> io::Result<PwmState> {
        self.chip.read_pwm_state(self.pwm)
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
                "svalka: could not hand back {}: {error}",
                self.chip.describe_pwm(self.pwm)
            );
        }
    }
}

/// Puts `pwmN` in manual mode. Outputs without `pwmN_enable` (as `saved`
/// shows) take duty writes without a mode switch.
fn switch_to_manual(chip: &Chip, pwm: u32, saved: PwmState) -> io::Result<()> {
    if saved.enable.is_some() {
        chip.write_pwm_enable(pwm, PwmEnable::Manual)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwmon::testing::FakeSysfs;

    fn nct6798(sysfs: &FakeSysfs, enable: u8) -> Chip {
        let fake = sysfs.chip("hwmon7", "nct6798", "platform/nct6775.656");
        fake.driver("nct6775")
            .attr("pwm1", "74")
            .attr("pwm1_enable", &enable.to_string());
        Chip::open(fake.dir()).unwrap()
    }

    fn state(raw: u8, enable: u8) -> PwmState {
        PwmState {
            raw,
            enable: Some(PwmEnable::from(enable)),
        }
    }

    #[test]
    fn take_switches_to_manual_and_hand_back_restores_everything() {
        let sysfs = FakeSysfs::new();
        let chip = nct6798(&sysfs, 5);

        let manual = ManualOverride::take(&chip, 1, HandBack::Saved).unwrap();
        manual.set(255).unwrap();
        assert_eq!(chip.read_pwm_state(1).unwrap(), state(255, 1));
        assert!(manual.current().unwrap().is_held_at(255));

        manual.hand_back().unwrap();
        assert_eq!(chip.read_pwm_state(1).unwrap(), state(74, 5));
    }

    #[test]
    fn dropping_without_hand_back_still_hands_back() {
        let sysfs = FakeSysfs::new();
        let chip = nct6798(&sysfs, 1);

        {
            let manual = ManualOverride::take(&chip, 1, HandBack::Saved).unwrap();
            manual.set(255).unwrap();
        }

        assert_eq!(chip.read_pwm_state(1).unwrap(), state(74, 1));
    }

    #[test]
    fn is_held_at_notices_other_writers() {
        assert!(state(120, 1).is_held_at(120));
        assert!(!state(98, 1).is_held_at(120)); // duty changed
        assert!(!state(120, 5).is_held_at(120)); // chip took control back
    }

    #[test]
    fn is_held_at_accepts_full_speed_at_255() {
        // nct6775 reports manual mode at 255 as pwmN_enable = 0.
        assert!(state(255, 0).is_held_at(255));
        assert!(!state(120, 0).is_held_at(120));
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

        let untouched = PwmState {
            raw: 0,
            enable: None,
        };
        assert_eq!(chip.read_pwm_state(1).unwrap(), untouched);
    }

    #[test]
    fn automatic_hand_back_switches_to_the_chips_own_mode() {
        let sysfs = FakeSysfs::new();
        let chip = nct6798(&sysfs, 1);
        let mode = chip.automatic_mode().unwrap();

        let manual = ManualOverride::take(&chip, 1, HandBack::Automatic(mode)).unwrap();
        manual.set(200).unwrap();
        manual.hand_back().unwrap();

        assert_eq!(chip.read_pwm_state(1).unwrap(), state(200, 5));
    }

    #[test]
    fn automatic_mode_is_known_only_for_the_nct6775_driver() {
        let sysfs = FakeSysfs::new();
        let other = sysfs.chip("hwmon2", "nct6687", "platform/nct6683.2592");
        other.driver("nct6683");

        assert_eq!(
            nct6798(&sysfs, 5).automatic_mode(),
            Some(PwmEnable::Automatic(5))
        );
        assert_eq!(Chip::open(other.dir()).unwrap().automatic_mode(), None);
    }

    #[test]
    fn reassert_takes_the_output_back() {
        let sysfs = FakeSysfs::new();
        let chip = nct6798(&sysfs, 5);
        let manual = ManualOverride::take(&chip, 1, HandBack::Saved).unwrap();
        chip.write_pwm_enable(1, PwmEnable::Automatic(5)).unwrap(); // e.g. the BIOS after resume

        manual.reassert(150).unwrap();

        assert_eq!(chip.read_pwm_state(1).unwrap(), state(150, 1));
    }

    #[test]
    fn state_displays_duty_and_mode() {
        assert_eq!(state(74, 5).to_string(), "74/255, automatic (5)");
    }
}
