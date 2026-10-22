//! The list of outputs `run` holds in manual mode, kept next to state.json in
//! `/run/onlycurves/outputs`, so that `restore` hands back exactly those even
//! if the config on disk has changed or broken since `run` read it.
//!
//! One line per output: `<mode> <pwm> <chip directory>`, for example
//! `5 2 /sys/class/hwmon/hwmon7`: write 5 to `hwmon7/pwm2_enable` to hand it
//! back. Chip directories only hold for the current boot, which is all the
//! file needs: it lives on tmpfs.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::binding::BoundOutput;
use crate::files;
use crate::hwmon::PwmEnable;

const FILE_NAME: &str = "outputs";

/// One output and the `pwmN_enable` mode that hands it back to its chip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldOutput {
    pub chip_dir: PathBuf,
    pub pwm: u32,
    pub mode: PwmEnable,
}

impl HeldOutput {
    /// `output`, handed back to its chip's automatic mode.
    ///
    /// # Errors
    ///
    /// Fails for chips whose automatic mode onlycurves does not know.
    pub fn of(output: &BoundOutput) -> anyhow::Result<Self> {
        Ok(Self {
            chip_dir: output.chip().dir().to_path_buf(),
            pwm: output.pwm(),
            mode: output.automatic_mode()?,
        })
    }

    /// Whether `other` is the same physical output.
    #[must_use]
    pub fn same_output(&self, other: &HeldOutput) -> bool {
        self.chip_dir == other.chip_dir && self.pwm == other.pwm
    }
}

/// `outputs` with each physical output once; the first entry for it wins.
#[must_use]
pub fn dedup(outputs: impl IntoIterator<Item = HeldOutput>) -> Vec<HeldOutput> {
    let mut unique: Vec<HeldOutput> = Vec::new();
    for output in outputs {
        if !unique.iter().any(|known| known.same_output(&output)) {
            unique.push(output);
        }
    }
    unique
}

/// The record file in a state directory.
#[derive(Debug, Clone)]
pub struct OutputRecord {
    path: PathBuf,
}

impl OutputRecord {
    #[must_use]
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            path: dir.join(FILE_NAME),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Replaces the record with `outputs`.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be written.
    pub fn write(&self, outputs: &[HeldOutput]) -> io::Result<()> {
        let text: String = outputs.iter().map(format_line).collect();
        files::write_atomically(&self.path, text)
    }

    /// The recorded outputs, or `None` when there is no record.
    ///
    /// # Errors
    ///
    /// Fails when the record exists but cannot be read or parsed.
    pub fn read(&self) -> io::Result<Option<Vec<HeldOutput>>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let outputs = text
            .lines()
            .map(|line| {
                parse_line(line).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}: cannot parse {line:?}", self.path().display()),
                    )
                })
            })
            .collect::<io::Result<_>>()?;
        Ok(Some(outputs))
    }

    /// Deletes the record. A record that is already gone is fine.
    ///
    /// # Errors
    ///
    /// Fails when the file exists but cannot be deleted.
    pub fn remove(&self) -> io::Result<()> {
        files::remove_if_present(&self.path)
    }
}

fn format_line(output: &HeldOutput) -> String {
    format!(
        "{} {} {}\n",
        u8::from(output.mode),
        output.pwm,
        output.chip_dir.display()
    )
}

/// The inverse of [`format_line`]. The directory comes last, so it may hold spaces.
fn parse_line(line: &str) -> Option<HeldOutput> {
    let mut fields = line.splitn(3, ' ');
    let mode = PwmEnable::from(fields.next()?.parse::<u8>().ok()?);
    let pwm = fields.next()?.parse().ok()?;
    let chip_dir = PathBuf::from(fields.next()?);
    Some(HeldOutput {
        chip_dir,
        pwm,
        mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(dir: &str, pwm: u32) -> HeldOutput {
        HeldOutput {
            chip_dir: PathBuf::from(dir),
            pwm,
            mode: PwmEnable::Automatic(5),
        }
    }

    #[test]
    fn writes_reads_and_removes_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let record = OutputRecord::in_dir(dir.path());
        let outputs = [
            held("/sys/class/hwmon/hwmon7", 2),
            held("/tmp/with space/hwmon1", 1),
        ];

        assert_eq!(record.read().unwrap(), None);
        record.write(&outputs).unwrap();
        assert_eq!(
            fs::read_to_string(record.path()).unwrap(),
            "5 2 /sys/class/hwmon/hwmon7\n5 1 /tmp/with space/hwmon1\n"
        );
        assert_eq!(record.read().unwrap(), Some(outputs.to_vec()));

        record.remove().unwrap();
        assert_eq!(record.read().unwrap(), None);
        record.remove().unwrap();
    }

    #[test]
    fn dedup_keeps_the_first_entry_for_each_output() {
        let with_mode = |dir: &str, pwm, mode| HeldOutput {
            chip_dir: PathBuf::from(dir),
            pwm,
            mode: PwmEnable::Automatic(mode),
        };

        let unique = dedup([
            with_mode("/hwmon7", 2, 5),
            with_mode("/hwmon7", 2, 3),
            with_mode("/hwmon7", 1, 5),
        ]);

        assert_eq!(
            unique,
            [with_mode("/hwmon7", 2, 5), with_mode("/hwmon7", 1, 5)]
        );
    }

    #[test]
    fn a_damaged_record_is_an_error_not_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let record = OutputRecord::in_dir(dir.path());
        fs::write(record.path(), "five two hwmon7\n").unwrap();

        assert_eq!(
            record.read().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
