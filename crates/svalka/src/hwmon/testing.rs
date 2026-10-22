//! A fake sysfs tree in a temporary directory, for tests.
//!
//! It mirrors the real layout: chips live in `class/hwmon/hwmonN` and their
//! `device` link points into `devices/…`.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

pub struct FakeSysfs {
    root: TempDir,
}

impl FakeSysfs {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("class/hwmon")).unwrap();
        Self { root }
    }

    /// The directory that plays `/sys/class/hwmon`.
    pub fn hwmon_root(&self) -> PathBuf {
        self.root.path().join("class/hwmon")
    }

    /// Adds chip `hwmon` (e.g. `hwmon7`) named `name`, belonging to `device`
    /// (a path below `devices/`, e.g. `platform/nct6775.656`).
    pub fn chip(&self, hwmon: &str, name: &str, device: &str) -> FakeChip {
        let device_dir = self.root.path().join("devices").join(device);
        let chip_dir = self.hwmon_root().join(hwmon);
        fs::create_dir_all(&device_dir).unwrap();
        fs::create_dir_all(&chip_dir).unwrap();
        symlink(&device_dir, chip_dir.join("device")).unwrap();

        let chip = FakeChip {
            dir: chip_dir,
            device_dir,
            drivers_dir: self.root.path().join("bus/drivers"),
        };
        chip.attr("name", name);
        chip
    }
}

pub struct FakeChip {
    dir: PathBuf,
    device_dir: PathBuf,
    drivers_dir: PathBuf,
}

impl FakeChip {
    /// Writes one attribute file, with the trailing newline the kernel adds.
    pub fn attr(&self, file: &str, value: &str) -> &Self {
        fs::write(self.dir.join(file), format!("{value}\n")).unwrap();
        self
    }

    /// Binds the chip's device to kernel driver `driver`, e.g. `nct6775`.
    pub fn driver(&self, driver: &str) -> &Self {
        let driver_dir = self.drivers_dir.join(driver);
        fs::create_dir_all(&driver_dir).unwrap();
        symlink(&driver_dir, self.device_dir.join("driver")).unwrap();
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}
