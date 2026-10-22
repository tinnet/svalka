//! Black-box tests: run the `onlycurves` binary against a fake sysfs tree.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;

/// A small copy of this PC's hwmon layout, with two NVMe drives added so that
/// one name is shared. `class/hwmon` holds the chips, `devices` their devices.
struct FakeSysfs {
    root: TempDir,
}

impl FakeSysfs {
    fn this_pc() -> Self {
        let sysfs = Self {
            root: tempfile::tempdir().unwrap(),
        };
        sysfs.chip(
            "hwmon7",
            "nct6798",
            "platform/nct6775.656",
            &[
                ("temp1_input", "34000"),
                ("temp1_label", "SYSTIN"),
                ("fan1_input", "971"),
                ("pwm1", "102"),
                ("pwm1_enable", "1"),
            ],
        );
        sysfs.chip(
            "hwmon3",
            "k10temp",
            "pci0000:00/0000:00:18.3",
            &[("temp1_input", "49125"), ("temp1_label", "Tctl")],
        );
        sysfs.chip(
            "hwmon0",
            "nvme",
            "pci0000:00/0000:01:00.0/nvme/nvme0",
            &[("temp1_input", "45000")],
        );
        sysfs.chip(
            "hwmon2",
            "nvme",
            "pci0000:00/0000:04:00.0/nvme/nvme1",
            &[("temp1_input", "41000")],
        );
        sysfs.chip("hwmon5", "asus", "platform/eeepc-wmi", &[]);
        sysfs
    }

    fn chip(&self, hwmon: &str, name: &str, device: &str, attributes: &[(&str, &str)]) {
        let device_dir = self.root.path().join("devices").join(device);
        let chip_dir = self.hwmon_root().join(hwmon);
        fs::create_dir_all(&device_dir).unwrap();
        fs::create_dir_all(&chip_dir).unwrap();
        symlink(&device_dir, chip_dir.join("device")).unwrap();
        write_attribute(&chip_dir, "name", name);
        for (file, value) in attributes {
            write_attribute(&chip_dir, file, value);
        }
    }

    fn hwmon_root(&self) -> PathBuf {
        self.root.path().join("class/hwmon")
    }

    /// `onlycurves --hwmon-root <fake> <args…>`
    fn onlycurves(&self, args: &[&str]) -> Command {
        let mut command = Command::new(assert_cmd::cargo_bin!("onlycurves"));
        command
            .arg("--hwmon-root")
            .arg(self.hwmon_root())
            .args(args);
        command
    }
}

fn write_attribute(dir: &Path, file: &str, value: &str) {
    fs::write(dir.join(file), format!("{value}\n")).unwrap();
}

#[test]
fn help_lists_the_status_command() {
    Command::new(assert_cmd::cargo_bin!("onlycurves"))
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("status"));
}

#[test]
fn version_prints_the_crate_version() {
    Command::new(assert_cmd::cargo_bin!("onlycurves"))
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"onlycurves \d+\.\d+\.\d+").unwrap());
}

#[test]
fn status_shows_every_chip_with_sensors() {
    let sysfs = FakeSysfs::this_pc();

    sysfs
        .onlycurves(&["status"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "nct6798 · nct6775.656 · now hwmon7",
        ))
        .stdout(predicate::str::contains("SYSTIN   34.0 °C"))
        .stdout(predicate::str::contains("fan1      971 rpm"))
        .stdout(predicate::str::contains(
            "pwm1    102/255   40 %   manual (1)",
        ))
        .stdout(predicate::str::contains("Tctl   49.1 °C"))
        .stdout(predicate::str::contains("nvme · nvme1 · now hwmon2"))
        .stdout(predicate::str::contains("asus").not());
}

#[test]
fn status_chip_shows_only_that_chip() {
    let sysfs = FakeSysfs::this_pc();

    sysfs
        .onlycurves(&["status", "--chip", "k10temp"])
        .assert()
        .success()
        .stdout(predicate::str::contains("k10temp · 0000:00:18.3"))
        .stdout(predicate::str::contains("nct6798").not());
}

#[test]
fn status_chip_with_a_shared_name_asks_for_a_path() {
    let sysfs = FakeSysfs::this_pc();

    sysfs
        .onlycurves(&["status", "--chip", "nvme"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "2 hwmon chips are named \"nvme\"; add a device path to pick one of: nvme0, nvme1",
        ));
}

#[test]
fn status_chip_and_path_pick_one_of_several() {
    let sysfs = FakeSysfs::this_pc();

    sysfs
        .onlycurves(&["status", "--chip", "nvme", "--path", "nvme1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("nvme · nvme1"))
        .stdout(predicate::str::contains("nvme0").not());
}

#[test]
fn status_path_needs_a_chip() {
    let sysfs = FakeSysfs::this_pc();

    sysfs
        .onlycurves(&["status", "--path", "nvme1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--chip"));
}

#[test]
fn status_reports_a_missing_hwmon_root() {
    Command::new(assert_cmd::cargo_bin!("onlycurves"))
        .args(["--hwmon-root", "/nonexistent/hwmon", "status"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "cannot list hwmon chips in /nonexistent/hwmon",
        ));
}
