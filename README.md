# onlycurves

Fan control for one PC: read temperatures, map them through piecewise-linear
curves, write fan PWM. Replaces CoolerControl on an ASUS B550-I / Ryzen 7
5700X3D / RX 9070 XT box. The design doc lives in Claude Docs
("onlycurves — Design Doc").

## Status

Milestones 1 and 2 of 6 are done: `status` and `check`. Both are read-only
and need no root.

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

## Layout

```text
crates/onlycurves-core/     no hardware access; shared with the later TUI
  src/curve.rs              piecewise-linear curves and their rules
  src/config.rs             parsing and validating the TOML config
  src/duration.rs           "8s" / "500ms" values
  src/pwm.rs                percent <-> raw 0-255
crates/onlycurves/          the daemon and CLI (library + thin main.rs)
  src/main.rs               command line only
  src/hwmon.rs              chip discovery and identity (name + device path)
  src/hwmon/sensors.rs      reading temperatures, fans, PWM
  src/hwmon/resolve.rs      picking one chip by name and path
  src/binding.rs            tying config sensors and fans to real inputs/outputs
  src/status.rs             the `status` command
  src/check.rs              the `check` command
  src/sysfs.rs              reading one-value sysfs files
  tests/cli.rs              runs the binary against a fake sysfs tree
examples/onlycurves.toml    the config for this PC
```

`onlycurves-tui` joins the workspace later.

## License

MIT
