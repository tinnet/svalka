# CLAUDE.md

## Project

onlycurves: a fan-control daemon + CLI for one Linux PC, in Rust. The design
doc (Claude Docs, "onlycurves — Design Doc") is the source of truth for scope
and milestones; the README says which milestone is done.

## Commands

- Toolchain comes from mise (`rust` pinned in `mise.toml`). If `mise exec` or
  `mise run` tries to install a missing tool and fails, use
  `eval "$(mise env)"` and call `cargo` directly.
- `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all --check` must all pass before a commit.

## Conventions

- Small functions with one purpose each; doc comments say why, not what.
- Clippy pedantic is on for the workspace. Public functions returning
  `Result` get an `# Errors` section.
- Never identify a hwmon chip by `hwmonN`; use name + device path.
- Code that reads sysfs takes its root as a parameter so tests can use a fake
  tree (`hwmon::testing::FakeSysfs` in unit tests, `FakeSysfs` in `tests/cli.rs`).
- `status` and anything else outside the control loop is read-only.
- CLI behaviour is covered by `tests/cli.rs`.
