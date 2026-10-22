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

use anyhow::{Context, anyhow, bail};
use onlycurves_core::config::ConfigError;

use crate::binding;
use crate::hwmon::{self, Chip};
use crate::record::{HeldOutput, OutputRecord};
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
    for output in union(recorded.unwrap_or_default(), configured) {
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
    let chips = hwmon::discover(hwmon_root)
        .with_context(|| format!("cannot list hwmon chips in {}", hwmon_root.display()))?;
    let outputs = binding::bind_outputs(&config, &chips).map_err(|problems| {
        anyhow!(
            "{}: {}",
            config_path.display(),
            ConfigError::Invalid(problems)
        )
    })?;
    outputs
        .values()
        .map(|output| {
            let mode = output.chip().automatic_mode().ok_or_else(|| {
                anyhow!(
                    "{}: this chip's automatic mode is unknown",
                    output.describe()
                )
            })?;
            Ok(HeldOutput::of(output, mode))
        })
        .collect()
}

/// `recorded` followed by the `configured` outputs it does not already hold.
fn union(recorded: Vec<HeldOutput>, configured: Vec<HeldOutput>) -> Vec<HeldOutput> {
    let mut all = recorded;
    for output in configured {
        if !all.iter().any(|held| held.same_output(&output)) {
            all.push(output);
        }
    }
    all
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::hwmon::PwmEnable;

    #[test]
    fn union_keeps_each_output_once_preferring_the_record() {
        let held = |dir: &str, pwm, mode| HeldOutput {
            chip_dir: PathBuf::from(dir),
            pwm,
            mode: PwmEnable::Automatic(mode),
        };

        let all = union(
            vec![held("/hwmon7", 2, 5)],
            vec![held("/hwmon7", 2, 3), held("/hwmon7", 1, 5)],
        );

        assert_eq!(all, [held("/hwmon7", 2, 5), held("/hwmon7", 1, 5)]);
    }
}
