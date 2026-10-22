# CLAUDE.md

## Project

svalka: a fan-control daemon + CLI for Linux, in Rust. The README
describes what it does and for which hardware.

## Commands

- Toolchain comes from mise (`rust` pinned in `mise.toml`); run tasks with
  `mise run <task>`.
- `mise run test` and `mise run lint` must pass before a commit. The hk
  pre-commit hook (`hk install`) formats and runs betterleaks on staged changes.

## Conventions

- Small functions with one purpose each; doc comments say why, not what.
- Clippy pedantic is on for the workspace. Public functions returning
  `Result` get an `# Errors` section.
- Never identify a hwmon chip by `hwmonN`; use name + device path.
- Code that reads sysfs takes its root as a parameter so tests can use a fake
  tree (`hwmon::testing::FakeSysfs` in unit tests, `FakeSysfs` in `tests/cli.rs`).
- `status` and anything else outside the control loop is read-only.
- CLI behaviour is covered by `tests/cli.rs`.
