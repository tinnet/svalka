//! hwmon chips: finding them in sysfs and telling them apart.
//!
//! The kernel numbers chips `hwmon0`, `hwmon1`, … in probe order, and that order
//! changes between boots. A chip is therefore identified by its driver `name`
//! plus the device it belongs to, never by its number.

mod control;
mod resolve;
mod sensors;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::sysfs;

pub use control::{HandBack, ManualOverride, PwmState, automatic_mode, is_held};
pub use resolve::{ResolveError, resolve};
pub use sensors::{ChipSnapshot, Fan, Pwm, PwmEnable, Temp};

/// One hwmon chip, such as `nct6798` or `amdgpu`.
#[derive(Debug, Clone)]
pub struct Chip {
    /// The chip's directory, e.g. `/sys/class/hwmon/hwmon7`.
    dir: PathBuf,
    /// Driver name from the `name` attribute, e.g. `nct6798`.
    name: String,
    /// Fully resolved path of the device the chip belongs to, if it has one.
    device: Option<PathBuf>,
}

impl Chip {
    /// Opens the chip in `dir`.
    ///
    /// # Errors
    ///
    /// Fails when the chip has no readable `name` attribute.
    pub fn open(dir: &Path) -> io::Result<Self> {
        Ok(Self {
            dir: dir.to_path_buf(),
            name: sysfs::read_string(&dir.join("name"))?,
            device: fs::canonicalize(dir.join("device")).ok(),
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The chip's current `hwmonN` number. Only useful for display: it changes between boots.
    #[must_use]
    pub fn hwmon_number(&self) -> String {
        file_name_of(&self.dir)
    }

    /// The last part of the device path, e.g. `nct6775.656` or `0000:0a:00.0`.
    pub fn device_id(&self) -> Option<String> {
        self.device.as_deref().map(file_name_of)
    }

    /// Whether the chip's device path ends with `path`, compared component by component.
    ///
    /// So `nct6775.656`, `platform/nct6775.656` and the full path all match the
    /// device `/sys/devices/platform/nct6775.656`, but `6775.656` does not.
    #[must_use]
    pub fn device_matches(&self, path: &Path) -> bool {
        self.device
            .as_deref()
            .is_some_and(|device| device.ends_with(path))
    }
}

/// Finds every chip under `hwmon_root` (normally `/sys/class/hwmon`).
///
/// Chips come back sorted by name and device, so the order is the same on every
/// boot. Directories that are not a readable chip are skipped.
///
/// # Errors
///
/// Fails when `hwmon_root` cannot be listed.
pub fn discover(hwmon_root: &Path) -> io::Result<Vec<Chip>> {
    let mut chips = Vec::new();
    for entry in fs::read_dir(hwmon_root)? {
        let dir = entry?.path();
        if is_hwmon_dir(&dir)
            && let Ok(chip) = Chip::open(&dir)
        {
            chips.push(chip);
        }
    }
    chips.sort_by(|a, b| (&a.name, &a.device).cmp(&(&b.name, &b.device)));
    Ok(chips)
}

fn is_hwmon_dir(path: &Path) -> bool {
    file_name_of(path).starts_with("hwmon")
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests {
    use super::testing::FakeSysfs;
    use super::*;

    #[test]
    fn discover_finds_chips_sorted_by_name_not_number() {
        let sysfs = FakeSysfs::new();
        sysfs.chip("hwmon0", "nvme", "pci0000:00/0000:01:00.0/nvme/nvme0");
        sysfs.chip("hwmon7", "amdgpu", "pci0000:00/0000:0a:00.0");
        sysfs.chip("hwmon3", "k10temp", "pci0000:00/0000:00:18.3");

        let chips = discover(&sysfs.hwmon_root()).unwrap();

        let names: Vec<_> = chips.iter().map(Chip::name).collect();
        assert_eq!(names, ["amdgpu", "k10temp", "nvme"]);
    }

    #[test]
    fn discover_skips_directories_without_a_name() {
        let sysfs = FakeSysfs::new();
        sysfs.chip("hwmon0", "nvme", "pci0000:00/0000:01:00.0/nvme/nvme0");
        fs::create_dir(sysfs.hwmon_root().join("hwmon1")).unwrap();

        let chips = discover(&sysfs.hwmon_root()).unwrap();

        assert_eq!(chips.len(), 1);
    }

    #[test]
    fn chip_knows_its_device_and_current_number() {
        let sysfs = FakeSysfs::new();
        sysfs.chip("hwmon7", "nct6798", "platform/nct6775.656");

        let chip = &discover(&sysfs.hwmon_root()).unwrap()[0];

        assert_eq!(chip.hwmon_number(), "hwmon7");
        assert_eq!(chip.device_id().as_deref(), Some("nct6775.656"));
    }

    #[test]
    fn device_matches_whole_trailing_components_only() {
        let sysfs = FakeSysfs::new();
        sysfs.chip("hwmon7", "nct6798", "platform/nct6775.656");
        let chip = &discover(&sysfs.hwmon_root()).unwrap()[0];

        assert!(chip.device_matches(Path::new("nct6775.656")));
        assert!(chip.device_matches(Path::new("platform/nct6775.656")));
        assert!(!chip.device_matches(Path::new("6775.656")));
        assert!(!chip.device_matches(Path::new("platform/other.1")));
    }
}
