# onlycurves

Fan control for one PC: read temperatures, map them through piecewise-linear
curves, write fan PWM. Replaces CoolerControl on an ASUS B550-I / Ryzen 7
5700X3D / RX 9070 XT box. The design doc lives in Claude Docs
("onlycurves — Design Doc").

## Status

Milestones 1–4 and 6 of 7 are done: `status`, `check`, `identify`, `run`
(with `restore`) and the systemd service. `sweep` (5) is postponed. `status`
and `check` are read-only and need no root; the others write to the fans.

### `onlycurves status`

Every temperature, fan speed and PWM output, by chip.

```console
$ onlycurves status --chip nct6798
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

### `onlycurves check`

Validates the config (default `/etc/onlycurves.toml`, or `--config FILE`)
against this PC and shows the duty each fan would get right now.

```console
$ onlycurves --config examples/onlycurves.toml check
examples/onlycurves.toml is valid.

Sensors
  cpu  k10temp Tctl      44.6 °C
  gpu  amdgpu junction   34.0 °C

Fans, at the current temperatures (no smoothing or hysteresis)
  cpu_cooler  nct6798 pwm2  cpu   44.6 °C  →   42 %  107/255  now  42 %  107/255
  exhaust     nct6798 pwm1  gpu   34.0 °C  →   33 %   85/255  now  29 %   74/255
```

`now` is what the output is at today, i.e. what CoolerControl set. When the
config is wrong, `check` lists every problem and exits 1. The config is
checked first, then the hardware: chips must resolve, labels and outputs must
exist, `AUXTIN*` inputs are refused, and every sensor must read 0–125 °C.
[`examples/onlycurves.toml`](examples/onlycurves.toml) documents the format.

### `sudo onlycurves identify FAN`

Runs one fan from the config at 100 % for 5 s (`--seconds N` to change),
prints every tachometer on its chip once a second, then restores the output's
duty and mode exactly. It shows which physical fan a `pwmN` drives (watch or
listen) and which `fanN` reports it (the one that rises).

```console
$ sudo onlycurves --config examples/onlycurves.toml identify exhaust
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

The nct6775 driver refreshes readings every 1.5–2 s, hence the pairs.
Stop CoolerControl first (`sudo systemctl stop coolercontrold`): if another
program writes the output during the test, `identify` stops and restores it.
Ctrl-C and SIGTERM also restore before exiting.

### `sudo onlycurves run`

The control loop. It takes every configured fan into manual mode, then once
per `interval`: reads every sensor, takes each fan's hottest source, smooths
it (`smoothing`), holds small drops (`hysteresis`), applies the curve and
`min_duty`, and writes the duty when it changed. Each tick it also writes
`/run/onlycurves/state.json` (refused unless that is on tmpfs).

- A sensor that cannot be read, or jumps by more than 20 °C in one tick,
  puts its fans at `failsafe_duty` for that tick.
- If the output is switched away (resume from suspend, another program), it
  logs that once and takes it back.
- A fan reading 0 rpm at 30 % or more for 5 ticks is logged as stalled.
- Ctrl-C and SIGTERM stop it; SIGHUP reloads the config. On exit, and on
  errors, every output goes back to the chip's own SmartFan IV curve
  (`pwmN_enable = 5`).
- Logs go to stderr: plain in a terminal, with journald priorities under
  systemd.

### `sudo onlycurves restore`

Hands fan outputs back to the chip's own curve. systemd runs it after `run`
exits for any reason, including `kill -9`. It hands back every output that
`run` recorded as held in `/run/onlycurves/outputs`, plus every output in the
config. The record is what counts: the config on disk may have been edited or
broken since `run` read it, so it is only required when there is no record.
Sensors are never checked, so a broken sensor cannot block it.

### As a systemd service

```console
$ mise run install                       # binary, unit, and /etc/onlycurves.toml if missing
$ sudo systemctl disable --now coolercontrold
$ sudo systemctl enable --now onlycurves
$ systemctl status onlycurves            # Status: line shows each fan's duty and driver
$ sudo systemctl reload onlycurves       # after editing /etc/onlycurves.toml
$ journalctl -u onlycurves -f
```

[`dist/onlycurves.service`](dist/onlycurves.service) runs `onlycurves run` as
`Type=notify`: it reports ready after the first tick, pets a 10 s watchdog
every tick, and shows each fan's duty as its status. `systemctl reload` sends
SIGHUP: the new config is checked against the hardware first, and if it fails
the running one stays (see the journal). `ExecStopPost` runs `restore` after
every stop, so the fans go back to the chip's own curve even after `kill -9`.
`Conflicts=coolercontrold.service` keeps the two from fighting.

## Developing

Toolchain and tasks come from [mise](https://mise.jdx.dev/):

| Command | Does |
| --- | --- |
| `mise run build` | Test, then build |
| `mise run test` | Run tests only |
| `mise run lint` | `clippy` (pedantic, warnings are errors) + `rustfmt --check` |
| `mise run fmt` | Format |
| `mise run status` | `cargo run -- status` |
| `mise run check` | `check` with `examples/onlycurves.toml` |
| `mise run install` | Release build, then install binary + unit (sudo) |

## Layout

```text
crates/onlycurves-core/     no hardware access; shared with the later TUI
  src/curve.rs              piecewise-linear curves and their rules
  src/config.rs             parsing and validating the TOML config
  src/duration.rs           "8s" / "500ms" values
  src/controller.rs         one tick of one fan: smoothing, hysteresis, failsafe
  src/state.rs              the state.json snapshot format
  src/pwm.rs                percent <-> raw 0-255
crates/onlycurves/          the daemon and CLI (library + thin main.rs)
  src/main.rs               command line only
  src/hwmon.rs              chip discovery and identity (name + device path)
  src/hwmon/sensors.rs      reading temperatures, fans, PWM
  src/hwmon/resolve.rs      picking one chip by name and path
  src/binding.rs            tying config sensors and fans to real inputs/outputs
  src/status.rs             the `status` command
  src/check.rs              the `check` command
  src/identify.rs           the `identify` command
  src/run.rs                the `run` command: the control loop
  src/restore.rs            the `restore` command
  src/record.rs             /run/onlycurves/outputs: what run holds, for restore
  src/statefile.rs          state.json, tmpfs only, atomic writes
  src/log.rs                stderr logging, journald priorities under systemd
  src/notify.rs             sd_notify: READY, STATUS, WATCHDOG, RELOADING
  src/setup.rs              load config + bind, shared by commands
  src/signals.rs            SIGINT/SIGTERM -> stop, SIGHUP -> reload
  src/hwmon/control.rs      manual PWM control that always restores
  src/sysfs.rs              reading one-value sysfs files
  tests/cli.rs              runs the binary against a fake sysfs tree
examples/onlycurves.toml    the config for this PC
dist/onlycurves.service     the systemd unit
```

`onlycurves-tui` joins the workspace later.

## License

MIT
