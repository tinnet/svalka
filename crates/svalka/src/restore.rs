//! The `restore` command: hand fan outputs back to the chip's own automatic
//! mode.
//!
//! systemd runs it after the daemon stops for any reason, including SIGKILL or
//! a panic, when the daemon could not hand the fans back itself. It hands back
//! every output `run` recorded as held (see [`crate::record`]) and every output
//! in the config, each once. The record is what matters: the config on disk
//! may have changed or broken since `run` read it, so a config that does not
//! load is only fatal when there is no record. Sensors are never bound, so a
//! broken sensor cannot block it, and it keeps going past an output that fails.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, bail};
use svalka_core::config::ConfigError;

use crate::binding;
use crate::hwmon::Chip;
use crate::record::{self, HeldOutput, OutputRecord};
use crate::setup;

/// Hands back every recorded and configured output, reporting each on `out`.
///
/// # Errors
///
/// Fails when there is neither a record nor a loadable config, or when any
/// output could not be handed back (after trying all of them).
pub fn run(
    config_path: &Path,
    hwmon_root: &Path,
    state_dir: &Path,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let record = OutputRecord::in_dir(state_dir);
    let recorded = match record.read() {
        Ok(recorded) => recorded,
        Err(error) => {
            writeln!(out, "ignoring a damaged record: {error}")?;
            None
        }
    };
    let configured = match (configured_outputs(config_path, hwmon_root), &recorded) {
        (Ok(configured), _) => configured,
        (Err(error), Some(_)) => {
            writeln!(
                out,
                "using only the record; the config does not load: {error:#}"
            )?;
            Vec::new()
        }
        (Err(error), None) => return Err(error),
    };

    let mut failures = Vec::new();
    for output in record::dedup(recorded.unwrap_or_default().into_iter().chain(configured)) {
        match hand_back(&output) {
            Ok(describe) => writeln!(out, "{describe} handed back to the chip, {}", output.mode)?,
            Err(error) => failures.push(format!("{error:#}")),
        }
    }
    if !failures.is_empty() {
        bail!(
            "could not hand back every fan:\n  - {}",
            failures.join("\n  - ")
        );
    }
    record
        .remove()
        .with_context(|| format!("cannot remove {}", record.path().display()))
}

/// The config's outputs with their chips' automatic modes.
fn configured_outputs(config_path: &Path, hwmon_root: &Path) -> anyhow::Result<Vec<HeldOutput>> {
    let config = setup::load_config(config_path)?;
    let chips = setup::discover_chips(hwmon_root)?;
    let outputs = binding::bind_outputs(&config, &chips)
        .map_err(|problems| setup::invalid(config_path, &ConfigError::Invalid(problems)))?;
    outputs.values().map(HeldOutput::of).collect()
}

/// Writes the output's automatic mode. Returns e.g. `nct6798 pwm2`.
fn hand_back(output: &HeldOutput) -> anyhow::Result<String> {
    let chip = Chip::open(&output.chip_dir)
        .with_context(|| format!("cannot open {}", output.chip_dir.display()))?;
    let describe = chip.describe_pwm(output.pwm);
    chip.write_pwm_enable(output.pwm, output.mode)
        .with_context(|| format!("cannot write {describe}"))?;
    Ok(describe)
}
