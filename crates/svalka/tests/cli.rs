//! Black-box tests: run the `svalka` binary against a fake sysfs tree.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;

/// A small hwmon layout (nct6798, k10temp, amdgpu), with two NVMe drives so that
/// one name is shared. `class/hwmon` holds the chips, `devices` their devices.
/// It has everything `examples/svalka.toml` refers to.
struct FakeSysfs {
    root: TempDir,
}

impl FakeSysfs {
    fn example_pc() -> Self {
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
        sysfs.bind_driver("platform/nct6775.656", "nct6775");
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
        let path = self.root.path().join("svalka.toml");
        fs::write(&path, text).unwrap();
        path
    }

    /// Binds `device` (a path below `devices/`) to kernel driver `driver`.
    fn bind_driver(&self, device: &str, driver: &str) {
        let driver_dir = self.root.path().join("bus/drivers").join(driver);
        fs::create_dir_all(&driver_dir).unwrap();
        symlink(
            &driver_dir,
            self.root.path().join("devices").join(device).join("driver"),
        )
        .unwrap();
    }

    fn hwmon_root(&self) -> PathBuf {
        self.root.path().join("class/hwmon")
    }

    /// `svalka --hwmon-root <fake> <args…>`
    fn svalka(&self, args: &[&str]) -> Command {
        let mut command = svalka_bin();
        command
            .arg("--hwmon-root")
            .arg(self.hwmon_root())
            .args(args);
        command
    }

    /// `svalka --hwmon-root <fake> --config <config> --state-dir <state_dir> <command>`
    fn svalka_with(&self, config: &Path, state_dir: &Path, command: &str) -> Command {
        let mut svalka = self.svalka(&[]);
        svalka
            .arg("--config")
            .arg(config)
            .arg("--state-dir")
            .arg(state_dir)
            .arg(command);
        svalka
    }
}

/// The binary under test.
fn svalka_bin() -> Command {
    Command::new(assert_cmd::cargo_bin!("svalka"))
}

/// The example config shipped in the repository.
fn example_config() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/svalka.toml");
    fs::read_to_string(path).unwrap()
}

fn write_attribute(dir: &Path, file: &str, value: &str) {
    fs::write(dir.join(file), format!("{value}\n")).unwrap();
}

#[test]
fn help_lists_the_status_command() {
    svalka_bin()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("status"));
}

#[test]
fn version_prints_the_crate_version() {
    svalka_bin()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"svalka \d+\.\d+\.\d+").unwrap());
}

#[test]
fn status_shows_every_chip_with_sensors() {
    let sysfs = FakeSysfs::example_pc();

    sysfs
        .svalka(&["status"])
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
    let sysfs = FakeSysfs::example_pc();

    sysfs
        .svalka(&["status", "--chip", "k10temp"])
        .assert()
        .success()
        .stdout(predicate::str::contains("k10temp · 0000:00:18.3"))
        .stdout(predicate::str::contains("nct6798").not());
}

#[test]
fn status_chip_with_a_shared_name_asks_for_a_path() {
    let sysfs = FakeSysfs::example_pc();

    sysfs
        .svalka(&["status", "--chip", "nvme"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "2 hwmon chips are named \"nvme\"; add a device path to pick one of: nvme0, nvme1",
        ));
}

#[test]
fn status_chip_and_path_pick_one_of_several() {
    let sysfs = FakeSysfs::example_pc();

    sysfs
        .svalka(&["status", "--chip", "nvme", "--path", "nvme1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("nvme · nvme1"))
        .stdout(predicate::str::contains("nvme0").not());
}

#[test]
fn status_path_needs_a_chip() {
    let sysfs = FakeSysfs::example_pc();

    sysfs
        .svalka(&["status", "--path", "nvme1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--chip"));
}

#[test]
fn status_reports_a_missing_hwmon_root() {
    svalka_bin()
        .args(["--hwmon-root", "/nonexistent/hwmon", "status"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "cannot list hwmon chips in /nonexistent/hwmon",
        ));
}

#[test]
fn check_accepts_the_example_config_and_shows_duties() {
    let sysfs = FakeSysfs::example_pc();
    let config = sysfs.config(&example_config());

    sysfs
        .svalka(&["--config", config.to_str().unwrap(), "check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("svalka.toml is valid."))
        .stdout(predicate::str::contains("cpu  k10temp Tctl      49.1 °C"))
        .stdout(predicate::str::contains(
            "cpu_cooler  nct6798 pwm2  cpu   49.1 °C  →   48 %  122/255  now  47 %  120/255",
        ))
        .stdout(predicate::str::contains(
            "exhaust     nct6798 pwm1  gpu   47.0 °C  →   34 %   85/255  now  40 %  102/255",
        ));
}

#[test]
fn check_lists_every_hardware_problem() {
    let sysfs = FakeSysfs::example_pc();
    let text = example_config()
        .replace(r#"label = "junction""#, r#"label = "hotspot""#)
        .replace("pwm = 1 }", "pwm = 4 }");
    let config = sysfs.config(&text);

    sysfs
        .svalka(&["--config", config.to_str().unwrap(), "check"])
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
    let sysfs = FakeSysfs::example_pc();
    let text = example_config().replace("[[35, 30], [55, 55]", "[[55, 30], [35, 55]");
    let config = sysfs.config(&text);

    sysfs
        .svalka(&["--config", config.to_str().unwrap(), "check"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "fan.cpu_cooler: curve: point 2 (35 °C) must be hotter than point 1 (55 °C)",
        ));
}

#[test]
fn check_reports_a_missing_config() {
    let sysfs = FakeSysfs::example_pc();

    sysfs
        .svalka(&["--config", "/nonexistent/svalka.toml", "check"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "cannot read /nonexistent/svalka.toml",
        ));
}

#[test]
fn identify_runs_the_fan_and_restores_its_output() {
    let sysfs = FakeSysfs::example_pc();
    let config = sysfs.config(&example_config());

    sysfs
        .svalka(&[
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

    assert_eq!(nct6798_attribute(&sysfs, "pwm2"), "120");
}

#[test]
fn identify_lists_the_fans_when_the_name_is_unknown() {
    let sysfs = FakeSysfs::example_pc();
    let config = sysfs.config(&example_config());

    sysfs
        .svalka(&["--config", config.to_str().unwrap(), "identify", "intake"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "no fan named \"intake\" in the config; it has: cpu_cooler, exhaust",
        ));
}

#[test]
fn identify_explains_that_writing_needs_root() {
    use std::os::unix::fs::PermissionsExt;

    let sysfs = FakeSysfs::example_pc();
    let config = sysfs.config(&example_config());
    let pwm1_enable = sysfs.hwmon_root().join("hwmon7/pwm1_enable");
    fs::set_permissions(&pwm1_enable, fs::Permissions::from_mode(0o444)).unwrap();

    sysfs
        .svalka(&["--config", config.to_str().unwrap(), "identify", "exhaust"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Writing to fans needs root; run this with sudo.",
        ));
}

#[test]
fn sweep_steps_the_fan_and_restores_its_output() {
    let sysfs = FakeSysfs::example_pc();
    let config = sysfs.config(&example_config());
    let state_dir = tempfile::tempdir().unwrap();

    sysfs
        .svalka_with(&config, state_dir.path(), "sweep")
        .args(["exhaust", "--step", "50", "--sample-ms", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Sweeping exhaust (nct6798 pwm1) from 100 % to 0 and back in 50 % steps.",
        ))
        .stdout(predicate::str::contains(
            "down  100 %    971 rpm\ndown   50 %    971 rpm\ndown    0 %    971 rpm\n\
             up     50 %    971 rpm\nup    100 %    971 rpm\n",
        ))
        .stdout(predicate::str::contains(
            "Restored nct6798 pwm1 to 102/255, manual (1).",
        ))
        .stdout(predicate::str::contains(
            "exhaust kept spinning even at 0 %",
        ));

    assert_eq!(nct6798_attribute(&sysfs, "pwm1"), "102");
}

#[test]
fn sweep_refuses_a_fan_that_run_is_controlling() {
    let sysfs = FakeSysfs::example_pc();
    let config = sysfs.config(&example_config());
    let state_dir = tempfile::tempdir().unwrap();
    let chip_dir = sysfs.hwmon_root().join("hwmon7");
    fs::write(
        state_dir.path().join("outputs"),
        format!("5 1 {}\n", chip_dir.display()),
    )
    .unwrap();

    sysfs
        .svalka_with(&config, state_dir.path(), "sweep")
        .arg("exhaust")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "svalka run is controlling nct6798 pwm1. Stop it first",
        ));

    assert_eq!(nct6798_attribute(&sysfs, "pwm1_enable"), "1");
}

#[test]
fn sweep_needs_a_tachometer() {
    let sysfs = FakeSysfs::example_pc();
    let config = sysfs.config(&example_config());
    let state_dir = tempfile::tempdir().unwrap();

    sysfs
        .svalka_with(&config, state_dir.path(), "sweep")
        .arg("cpu_cooler")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "nct6798 pwm2 has no tachometer fan2, so sweep cannot see the fan's speed",
        ));
}

/// Reads one attribute of the fake nct6798.
fn nct6798_attribute(sysfs: &FakeSysfs, file: &str) -> String {
    fs::read_to_string(sysfs.hwmon_root().join("hwmon7").join(file))
        .unwrap()
        .trim()
        .to_owned()
}

/// Polls `condition` every 50 ms for up to 5 s.
fn wait_for(condition: impl Fn() -> bool) {
    for _ in 0..100 {
        if condition() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("timed out");
}

#[test]
fn run_drives_the_fans_and_hands_them_back_on_sigterm() {
    let sysfs = FakeSysfs::example_pc();
    let config = sysfs.config(&example_config());
    let state_dir = tempfile::tempdir_in("/dev/shm").unwrap();
    let state_file = state_dir.path().join("state.json");

    let child = sysfs
        .svalka_with(&config, state_dir.path(), "run")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(|| state_file.exists());

    // cpu 49.125 °C on [[35, 30], [55, 55]] -> 47.7 % -> 122; gpu 47 °C on [[40, 30], [60, 40]] -> 33.5 % -> 85.
    assert_eq!(nct6798_attribute(&sysfs, "pwm2"), "122");
    assert_eq!(nct6798_attribute(&sysfs, "pwm1"), "85");
    assert_eq!(nct6798_attribute(&sysfs, "pwm1_enable"), "1");
    let snapshot =
        svalka_core::state::Snapshot::from_json(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let cooler = &snapshot.fans["cpu_cooler"];
    assert_eq!(cooler.source.as_deref(), Some("cpu"));
    assert_eq!(cooler.pwm_raw, 122);
    assert_eq!(snapshot.fans["exhaust"].rpm, Some(971));
    let record = fs::read_to_string(state_dir.path().join("outputs")).unwrap();
    assert_eq!(record.lines().count(), 2, "{record}");

    signal(&child, "TERM");
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("cpu_cooler: handed nct6798 pwm2 back to the chip"),
        "{stderr}"
    );
    assert_eq!(nct6798_attribute(&sysfs, "pwm1_enable"), "5");
    assert_eq!(nct6798_attribute(&sysfs, "pwm2_enable"), "5");
    assert!(
        !state_file.exists(),
        "a stopped daemon leaves no state behind"
    );
    assert!(!state_dir.path().join("outputs").exists());
}

#[test]
fn run_refuses_a_state_directory_on_disk_before_touching_fans() {
    let sysfs = FakeSysfs::example_pc();
    let config = sysfs.config(&example_config());
    // The root filesystem is a disk on any real system; a checkout may not be.
    let on_disk = Path::new("/");

    sysfs
        .svalka_with(&config, on_disk, "run")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not tmpfs"));
    assert_eq!(nct6798_attribute(&sysfs, "pwm1"), "102");
    assert_eq!(nct6798_attribute(&sysfs, "pwm1_enable"), "1");
}

#[test]
fn restore_hands_every_configured_fan_back_to_the_chip() {
    let sysfs = FakeSysfs::example_pc();
    let config = sysfs.config(&example_config());
    let state_dir = tempfile::tempdir().unwrap();

    sysfs
        .svalka_with(&config, state_dir.path(), "restore")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "nct6798 pwm2 handed back to the chip, automatic (5)",
        ))
        .stdout(predicate::str::contains(
            "nct6798 pwm1 handed back to the chip, automatic (5)",
        ));
    assert_eq!(nct6798_attribute(&sysfs, "pwm1_enable"), "5");
    assert_eq!(nct6798_attribute(&sysfs, "pwm2_enable"), "5");
}

#[test]
fn restore_ignores_broken_sensors() {
    let sysfs = FakeSysfs::example_pc();
    let text = example_config().replace(r#"label = "Tctl""#, r#"label = "Tcl""#);
    let config = sysfs.config(&text);
    let state_dir = tempfile::tempdir().unwrap();

    sysfs
        .svalka_with(&config, state_dir.path(), "restore")
        .assert()
        .success();
    assert_eq!(nct6798_attribute(&sysfs, "pwm2_enable"), "5");
}

#[test]
fn restore_uses_the_record_when_the_config_is_broken() {
    let sysfs = FakeSysfs::example_pc();
    let config = sysfs.config("this is not toml");
    let state_dir = tempfile::tempdir().unwrap();
    let chip_dir = sysfs.hwmon_root().join("hwmon7");
    fs::write(
        state_dir.path().join("outputs"),
        format!("5 2 {}\n", chip_dir.display()),
    )
    .unwrap();

    sysfs
        .svalka_with(&config, state_dir.path(), "restore")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "using only the record; the config does not load",
        ))
        .stdout(predicate::str::contains(
            "nct6798 pwm2 handed back to the chip, automatic (5)",
        ));
    assert_eq!(nct6798_attribute(&sysfs, "pwm2_enable"), "5");
    assert_eq!(
        nct6798_attribute(&sysfs, "pwm1_enable"),
        "1",
        "not recorded, not configured"
    );
    assert!(!state_dir.path().join("outputs").exists());
}

#[test]
fn restore_fails_without_record_or_config() {
    let sysfs = FakeSysfs::example_pc();
    let config = sysfs.config("this is not toml");
    let state_dir = tempfile::tempdir().unwrap();

    sysfs
        .svalka_with(&config, state_dir.path(), "restore")
        .assert()
        .failure();
}

#[test]
fn restore_has_nothing_to_do_when_the_configured_chip_is_gone() {
    let sysfs = FakeSysfs::example_pc();
    let config = sysfs.config(&example_config());
    let state_dir = tempfile::tempdir().unwrap();
    fs::remove_dir_all(sysfs.hwmon_root().join("hwmon7")).unwrap();

    sysfs
        .svalka_with(&config, state_dir.path(), "restore")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            r#"fan.cpu_cooler: nothing to hand back: no hwmon chip named "nct6798""#,
        ));
}

#[test]
fn restore_still_hands_back_the_fans_whose_chip_is_there() {
    let sysfs = FakeSysfs::example_pc();
    let text = example_config().replace(
        r#"output     = { chip = "nct6798", pwm = 1 }"#,
        r#"output     = { chip = "nct6799", pwm = 1 }"#,
    );
    let config = sysfs.config(&text);
    let state_dir = tempfile::tempdir().unwrap();

    sysfs
        .svalka_with(&config, state_dir.path(), "restore")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            r#"fan.exhaust: nothing to hand back: no hwmon chip named "nct6799""#,
        ))
        .stdout(predicate::str::contains(
            "nct6798 pwm2 handed back to the chip, automatic (5)",
        ));
    assert_eq!(nct6798_attribute(&sysfs, "pwm2_enable"), "5");
}

#[test]
fn restore_has_nothing_to_do_when_the_recorded_chip_is_gone() {
    let sysfs = FakeSysfs::example_pc();
    let config = sysfs.config(&example_config());
    let state_dir = tempfile::tempdir().unwrap();
    let chip_dir = sysfs.hwmon_root().join("hwmon7");
    let record = state_dir.path().join("outputs");
    fs::write(&record, format!("5 2 {}\n", chip_dir.display())).unwrap();
    fs::remove_dir_all(&chip_dir).unwrap();

    sysfs
        .svalka_with(&config, state_dir.path(), "restore")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "{} pwm2: nothing to hand back: the chip is gone",
            chip_dir.display()
        )));
    assert!(!record.exists());
}

#[test]
fn restore_still_fails_when_a_chip_is_ambiguous_rather_than_gone() {
    let sysfs = FakeSysfs::example_pc();
    let text = example_config().replace(
        r#"output     = { chip = "nct6798", pwm = 1 }"#,
        r#"output     = { chip = "nvme", pwm = 1 }"#,
    );
    let config = sysfs.config(&text);
    let state_dir = tempfile::tempdir().unwrap();

    sysfs
        .svalka_with(&config, state_dir.path(), "restore")
        .assert()
        .failure()
        .stderr(predicate::str::contains("add a device path"));
}

/// A socket standing in for systemd's `NOTIFY_SOCKET`.
struct FakeSystemd {
    _dir: TempDir,
    path: PathBuf,
    socket: std::os::unix::net::UnixDatagram,
}

impl FakeSystemd {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let socket = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        Self {
            _dir: dir,
            path,
            socket,
        }
    }

    /// Receives messages until one starts with `prefix`, and returns it.
    fn wait_for(&self, prefix: &str) -> String {
        let mut buffer = [0; 1024];
        loop {
            let length = self
                .socket
                .recv(&mut buffer)
                .expect("a message from the daemon");
            let message = String::from_utf8_lossy(&buffer[..length]).into_owned();
            if message.starts_with(prefix) {
                return message;
            }
        }
    }
}

fn signal(child: &std::process::Child, name: &str) {
    let sent = Command::new("kill")
        .args([&format!("-{name}"), &child.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());
}

#[test]
fn run_talks_to_systemd_and_reloads_on_sighup() {
    let sysfs = FakeSysfs::example_pc();
    let config = sysfs.config(&example_config());
    let state_dir = tempfile::tempdir_in("/dev/shm").unwrap();
    let systemd = FakeSystemd::new();

    let child = sysfs
        .svalka_with(&config, state_dir.path(), "run")
        .env("NOTIFY_SOCKET", &systemd.path)
        .env("WATCHDOG_USEC", "10000000")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    systemd.wait_for("WATCHDOG=1");
    let status = systemd.wait_for("STATUS=");
    assert_eq!(
        status,
        "STATUS=cpu_cooler 48 % (cpu 49 °C), exhaust 34 % (gpu 47 °C)"
    );
    systemd.wait_for("READY=1");

    // A steeper CPU cooler curve: 49.125 °C on [[35, 50], [55, 80]] -> 71.2 % -> 182.
    sysfs.config(&example_config().replace("[[35, 30], [55, 55]", "[[35, 50], [55, 80]"));
    signal(&child, "HUP");
    systemd.wait_for("RELOADING=1\nMONOTONIC_USEC=");
    systemd.wait_for("READY=1");
    wait_for(|| nct6798_attribute(&sysfs, "pwm2") == "182");

    signal(&child, "TERM");
    systemd.wait_for("STOPPING=1");
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("reloaded"), "{stderr}");
    assert_eq!(nct6798_attribute(&sysfs, "pwm2_enable"), "5");
}
