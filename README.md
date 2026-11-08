# svalka

> **svalka** *(Swedish)*
> 1. *verb* — to cool, to refresh: *en svalkande bris*, a cooling breeze.
> 2. *noun* — coolness: *kvällens svalka*, the cool of the evening.
>
> **svalka** *(software)* — a small fan-control daemon for Linux.

svalka reads temperatures from hwmon, maps them through piecewise-linear
curves and writes fan PWM. One TOML file, one binary, no GUI, no API.

```toml
[sensors]
cpu = { chip = "k10temp", label = "Tctl" }

[fan.cpu_cooler]
output = { chip = "nct6798", pwm = 2 }
sources = ["cpu"]
curve = [[35, 30], [55, 55], [75, 90], [85, 100]]   # [°C, %]
```

## Hardware

- **Temperatures:** any hwmon temperature input (`k10temp`, `coretemp`,
  `amdgpu`, `nvme`, …), chosen by chip name and input label.
- **Fans:** PWM outputs of Super-I/O chips under the kernel's `nct6775` driver
  (nct6106 … nct6799). On exit, svalka hands each output back to the
  chip's own curve (SmartFan IV); it only takes over outputs whose driver it
  knows how to hand back, so other chips are refused rather than left stuck.
- Developed and tested on an nct6798 (ASUS AM4 board) with k10temp and amdgpu
  sensors. Reports from other boards are welcome.

## Install

Needs Rust (see `mise.toml` for the version) and systemd.

```console
$ mise run install                       # release build; binary, unit, and /etc/svalka.toml if missing
$ sudoedit /etc/svalka.toml          # see examples/svalka.toml
$ svalka check                       # validate the config against your hardware
$ sudo systemctl enable --now svalka
```

Stop any other fan-control daemon first: two programs writing the same PWM
outputs fight over them.

## Commands

`status` and `check` are read-only and need no root; the others write to the
fans.

### `svalka status`

Every temperature, fan speed and PWM output, by chip.

```console
$ svalka status --chip nct6798
nct6798 · nct6775.656 · now hwmon7
  Temperatures
    SYSTIN                     38.0 °C
    ...
  Fans
    fan1                        735 rpm
  PWM outputs
    pwm1                       74/255   29 %   manual (1)
```

- Chips are found by driver name plus device path, never by `hwmonN`, which
  changes between boots. The heading shows the current number for reference.
- `--chip NAME` shows one chip; add `--path DEVICE` when several chips share a
  name (e.g. two NVMe drives). The path matches the end of the device path, so
  `nct6775.656` and `platform/nct6775.656` both work.

### `svalka check`

Validates the config (default `/etc/svalka.toml`, or `--config FILE`)
against the hardware and shows the duty each fan would get right now.

```console
$ svalka --config examples/svalka.toml check
examples/svalka.toml is valid.

Sensors
  cpu  k10temp Tctl      44.6 °C
  gpu  amdgpu junction   34.0 °C

Fans, at the current temperatures (no smoothing or hysteresis)
  cpu_cooler  nct6798 pwm2  cpu   44.6 °C  →   42 %  107/255  now  42 %  107/255
  exhaust     nct6798 pwm1  gpu   34.0 °C  →   30 %   77/255  now  29 %   74/255
```

`now` is what the output is at, set by whatever controls it at the moment.
When the config is wrong, `check` lists every problem and exits 1. The config
is checked first, then the hardware: chips must resolve, labels and outputs
must exist, `AUXTIN*` inputs are refused, and every sensor must read
0–125 °C. [`examples/svalka.toml`](examples/svalka.toml) documents the
format.

### `sudo svalka identify FAN`

Runs one fan from the config at 100 % for 5 s (`--seconds N` to change),
prints every tachometer on its chip once a second, then restores the output's
duty and mode exactly. It shows which physical fan a `pwmN` drives (watch or
listen) and which `fanN` reports it (the one that rises).

```console
$ sudo svalka --config examples/svalka.toml identify exhaust
Running exhaust (nct6798 pwm1) at 100 % for 5 s. Watch or listen for the fan that speeds up.

            fan1    fan2    fan5
before       931    1044       0 rpm
1 s          931    1044       0 rpm
2 s         1293    1036       0 rpm
3 s         1293    1036       0 rpm
4 s         1764    1027       0 rpm
5 s         1764    1027       0 rpm

Restored nct6798 pwm1 to 98/255, automatic (5).
fan1 rose from 931 to 1764 rpm: pwm1 drives the fan that fan1 reports.
```

The nct6775 driver refreshes readings every 1.5–2 s, hence the pairs. If
another program writes the output during the test, `identify` stops and
restores it. Ctrl-C and SIGTERM also restore before exiting.

### `sudo svalka sweep FAN`

Steps one fan from 100 % down to 0 and back up in 5 % steps (`--step N` to
change), waits at each step until the speed settles, and prints duty against
rpm. Then it restores the output's duty and mode and says where the fan stops
on the way down, where it starts again on the way up, and suggests a
`min_duty` one step above the start duty. Readings are 2 s apart, so a sweep
takes 3–5 minutes.

```console
$ sudo svalka sweep cpu_cooler
Sweeping cpu_cooler (nct6798 pwm2) from 100 % to 0 and back in 5 % steps. Ctrl-C stops it and restores the output.

down  100 %   2093 rpm
down   95 %   2027 rpm
...
down   20 %    588 rpm
down   15 %    382 rpm
down   10 %      0 rpm
...
up     15 %      0 rpm
up     20 %    527 rpm
up     25 %    666 rpm
...
up    100 %   2125 rpm

Restored nct6798 pwm2 to 96/255, automatic (5).
cpu_cooler stops at 10 % on the way down and starts again at 20 % on the way up.
Suggested: min_duty = 25 for fan.cpu_cooler, one step above the start duty.
```

A fan keeps turning below the duty it needs to start from a standstill, so
`min_duty` goes above the start duty, not the stop duty.

`svalka run` must be stopped first (`sudo systemctl stop svalka`); `sweep`
refuses an output that `run` holds. It stops early and restores the output
if one of the fan's sources reaches 80 °C or cannot be read, if another
program writes the output, or on Ctrl-C and SIGTERM.

### `sudo svalka run`

The control loop. It takes every configured fan into manual mode, then once
per `interval`: reads every sensor, takes each fan's hottest source, smooths
it (`smoothing`), holds small drops (`hysteresis`), applies the curve and
`min_duty`, and writes the duty when it changed. Each tick it also writes
`/run/svalka/state.json` (refused unless that is on tmpfs).

- A sensor that cannot be read, or jumps by more than 20 °C in one tick,
  puts its fans at `failsafe_duty` for that tick.
- The fan outputs themselves are read back only every `readback_interval`
  (default 10 s). On some ASUS boards the nct6775 driver reaches the chip
  through ACPI WMI, and each read after its 1.5 s cache expires costs ~84 ms
  of kernel CPU, so reading every second would cost ~4 % of a core. Duties
  are still written every tick; writes are cheap.
- If an output is switched away (resume from suspend, another program), it
  logs that once and takes it back at the next readback.
- A fan reading 0 rpm at 30 % or more for about 5 s (at least two readbacks
  in a row) is logged as stalled.
- Ctrl-C and SIGTERM stop it; SIGHUP reloads the config. On exit, and on
  errors, every output goes back to the chip's own SmartFan IV curve
  (`pwmN_enable = 5`).
- Logs go to stderr: plain in a terminal, with journald priorities under
  systemd.

### `sudo svalka restore`

Hands fan outputs back to the chip's own curve. systemd runs it after `run`
exits for any reason, including `kill -9`. It hands back every output that
`run` recorded as held in `/run/svalka/outputs`, plus every output in the
config. The record is what counts: the config on disk may have been edited or
broken since `run` read it, so it is only required when there is no record.
Sensors are never checked, so a broken sensor cannot block it. An output whose
chip is gone, as can happen during shutdown, has nothing to hand back: `restore`
says so and carries on, and this does not count as a failure.

## As a systemd service

```console
$ systemctl status svalka            # Status: line shows each fan's duty and driver
$ sudo systemctl reload svalka       # after editing /etc/svalka.toml
$ journalctl -u svalka -f
```

[`dist/svalka.service`](dist/svalka.service) runs `svalka run` as
`Type=notify`: it reports ready after the first tick, pets a 10 s watchdog,
and shows each fan's duty as its status. `systemctl reload` sends SIGHUP: the
new config is checked against the hardware first, and if it fails the running
one stays (see the journal). `ExecStopPost` runs `restore` after every stop,
so the fans go back to the chip's own curve even after `kill -9`.

## Developing

Toolchain and tasks come from [mise](https://mise.jdx.dev/):

| Command | Does |
| --- | --- |
| `mise run build` | Test, then build |
| `mise run test` | Run tests only |
| `mise run lint` | `clippy` (pedantic, warnings are errors) + `rustfmt --check` |
| `mise run fmt` | Format |
| `mise run status` | `cargo run -- status` |
| `mise run check` | `check` with `examples/svalka.toml` |
| `mise run install` | Release build, then install binary + unit (sudo) |

Tests run against a fake sysfs tree, so they need no special hardware or root.

## Layout

```text
crates/svalka-core/     no hardware access
  src/curve.rs              piecewise-linear curves and their rules
  src/config.rs             parsing and validating the TOML config
  src/duration.rs           "8s" / "500ms" values
  src/controller.rs         one tick of one fan: smoothing, hysteresis, failsafe
  src/state.rs              the state.json snapshot format
  src/pwm.rs                percent <-> raw 0-255
crates/svalka/          the daemon and CLI (library + thin main.rs)
  src/main.rs               command line only
  src/hwmon.rs              chip discovery and identity (name + device path)
  src/hwmon/sensors.rs      reading temperatures, fans, PWM
  src/hwmon/resolve.rs      picking one chip by name and path
  src/hwmon/control.rs      manual PWM control that always hands back
  src/binding.rs            tying config sensors and fans to real inputs/outputs
  src/setup.rs              load config + bind, shared by commands
  src/status.rs             the `status` command
  src/check.rs              the `check` command
  src/identify.rs           the `identify` command
  src/sweep.rs              the `sweep` command
  src/run.rs                the `run` command: the control loop
  src/restore.rs            the `restore` command
  src/record.rs             /run/svalka/outputs: what run holds, for restore
  src/statefile.rs          state.json, tmpfs only, atomic writes
  src/files.rs              atomic write, remove-if-present
  src/log.rs                stderr logging, journald priorities under systemd
  src/notify.rs             sd_notify: READY, STATUS, WATCHDOG, RELOADING
  src/signals.rs            SIGINT/SIGTERM -> stop, SIGHUP -> reload
  src/sysfs.rs              reading one-value sysfs files
  tests/cli.rs              runs the binary against a fake sysfs tree
examples/svalka.toml    an annotated example config
dist/svalka.service     the systemd unit
```

## License

MIT
