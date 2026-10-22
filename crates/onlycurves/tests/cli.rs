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
/// It has everything `examples/onlycurves.toml` refers to.
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
                ("pwm2", "120"),
                ("pwm2_enable", "1"),
            ],
        );
        sysfs.chip(
            "hwmon1",
            "amdgpu",
            "pci0000:00/0000:0a:00.0",
            &[("temp2_input", "47000"), ("temp2_label", "junction")],
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

    /// Writes a config file next to the fake tree and returns its path.
    fn config(&self, text: &str) -> PathBuf {
        let path = self.root.path().join("onlycurves.toml");
        fs::write(&path, text).unwrap();
        path
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

/// The example config shipped in the repository.
fn example_config() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/onlycurves.toml");
    fs::read_to_string(path).unwrap()
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

#[test]
fn check_accepts_the_example_config_and_shows_duties() {
    let sysfs = FakeSysfs::this_pc();
    let config = sysfs.config(&example_config());

    sysfs
        .onlycurves(&["--config", config.to_str().unwrap(), "check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("onlycurves.toml is valid."))
        .stdout(predicate::str::contains("cpu  k10temp Tctl      49.1 °C"))
        .stdout(predicate::str::contains(
            "cpu_cooler  nct6798 pwm2  cpu   49.1 °C  →   48 %  122/255  now  47 %  120/255",
        ))
        .stdout(predicate::str::contains(
            "exhaust     nct6798 pwm1  gpu   47.0 °C  →   47 %  120/255  now  40 %  102/255",
        ));
}

#[test]
fn check_lists_every_hardware_problem() {
    let sysfs = FakeSysfs::this_pc();
    let text = example_config()
        .replace(r#"label = "junction""#, r#"label = "hotspot""#)
        .replace("pwm = 1 }", "pwm = 4 }");
    let config = sysfs.config(&text);

    sysfs
        .onlycurves(&["--config", config.to_str().unwrap(), "check"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("2 problem(s):"))
        .stderr(predicate::str::contains(
            r#"sensors.gpu: amdgpu has no temperature labelled "hotspot"; it has: junction"#,
        ))
        .stderr(predicate::str::contains(
            "fan.exhaust: output: nct6798 has no pwm4; it has: pwm1, pwm2",
        ));
}

#[test]
fn check_reports_config_errors_before_touching_hardware() {
    let sysfs = FakeSysfs::this_pc();
    let text = example_config().replace("[[35, 30], [55, 55]", "[[55, 30], [35, 55]");
    let config = sysfs.config(&text);

    sysfs
        .onlycurves(&["--config", config.to_str().unwrap(), "check"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "fan.cpu_cooler: curve: point 2 (35 °C) must be hotter than point 1 (55 °C)",
        ));
}

#[test]
fn check_reports_a_missing_config() {
    let sysfs = FakeSysfs::this_pc();

    sysfs
        .onlycurves(&["--config", "/nonexistent/onlycurves.toml", "check"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "cannot read /nonexistent/onlycurves.toml",
        ));
}

#[test]
fn identify_runs_the_fan_and_restores_its_output() {
    let sysfs = FakeSysfs::this_pc();
    let config = sysfs.config(&example_config());

    sysfs
        .onlycurves(&[
            "--config",
            config.to_str().unwrap(),
            "identify",
            "cpu_cooler",
            "--seconds",
            "1",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Running cpu_cooler (nct6798 pwm2) at 100 % for 1 s.",
        ))
        .stdout(predicate::str::contains("before       971 rpm"))
        .stdout(predicate::str::contains("1 s          971 rpm"))
        .stdout(predicate::str::contains(
            "Restored nct6798 pwm2 to 120/255, manual (1).",
        ))
        .stdout(predicate::str::contains(
            "No tachometer rose by 200 rpm or more.",
        ));

    let pwm2 = sysfs.hwmon_root().join("hwmon7/pwm2");
    assert_eq!(fs::read_to_string(pwm2).unwrap().trim(), "120");
}

#[test]
fn identify_lists_the_fans_when_the_name_is_unknown() {
    let sysfs = FakeSysfs::this_pc();
    let config = sysfs.config(&example_config());

    sysfs
        .onlycurves(&["--config", config.to_str().unwrap(), "identify", "intake"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "no fan named \"intake\" in the config; it has: cpu_cooler, exhaust",
        ));
}

#[test]
fn identify_explains_that_writing_needs_root() {
    use std::os::unix::fs::PermissionsExt;

    let sysfs = FakeSysfs::this_pc();
    let config = sysfs.config(&example_config());
    let pwm1_enable = sysfs.hwmon_root().join("hwmon7/pwm1_enable");
    fs::set_permissions(&pwm1_enable, fs::Permissions::from_mode(0o444)).unwrap();

    sysfs
        .onlycurves(&["--config", config.to_str().unwrap(), "identify", "exhaust"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "writing nct6798 pwm1 needs root; run this with sudo",
        ));
}
