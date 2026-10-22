# onlycurves

Fan control for one PC: read temperatures, map them through piecewise-linear
curves, write fan PWM. Replaces CoolerControl on an ASUS B550-I / Ryzen 7
5700X3D / RX 9070 XT box. The design doc lives in Claude Docs
("onlycurves — Design Doc").

## Status

Milestone 1 of 6: `onlycurves status`, read-only.

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
- Nothing is written. No root needed.

## Developing

Toolchain and tasks come from [mise](https://mise.jdx.dev/):

| Command | Does |
| --- | --- |
| `mise run build` | Test, then build |
| `mise run test` | Run tests only |
| `mise run lint` | `clippy` (pedantic, warnings are errors) + `rustfmt --check` |
| `mise run fmt` | Format |
| `mise run status` | `cargo run -- status` |

## Layout

```text
crates/onlycurves/          the daemon and CLI (library + thin main.rs)
  src/main.rs               command line only
  src/hwmon.rs              chip discovery and identity (name + device path)
  src/hwmon/sensors.rs      reading temperatures, fans, PWM
  src/hwmon/resolve.rs      picking one chip by name and path
  src/status.rs             the `status` command and its table
  src/sysfs.rs              reading one-value sysfs files
  tests/cli.rs              runs the binary against a fake sysfs tree
```

`onlycurves-core` (config, curves, state snapshot) and `onlycurves-tui` join
the workspace in later milestones.

## License

MIT
