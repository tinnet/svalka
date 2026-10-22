//! The `restore` command: hand every configured fan output back to the chip's
//! own automatic mode.
//!
//! systemd runs it after the daemon stops for any reason, including SIGKILL or
//! a panic, when the daemon could not hand the fans back itself. It binds only
//! the outputs, so a broken sensor cannot stop it, and it keeps going past an
//! output that fails.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, anyhow, bail};
use onlycurves_core::config::ConfigError;

use crate::binding::{self, BoundOutput};
use crate::hwmon::{self, automatic_mode};
use crate::setup;

/// Hands every output in the config back to its chip, reporting each on `out`.
///
/// # Errors
///
/// Fails when the config or the outputs cannot be loaded, or when any output
/// could not be handed back (after trying all of them).
pub fn run(config_path: &Path, hwmon_root: &Path, out: &mut impl Write) -> anyhow::Result<()> {
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

    let mut failures = Vec::new();
    for (name, output) in &outputs {
        match hand_to_chip(output) {
            Ok(mode) => writeln!(
                out,
                "{name}: {} handed back to the chip (mode {mode})",
                output.describe()
            )?,
            Err(error) => failures.push(format!("{name}: {error:#}")),
        }
    }
    if !failures.is_empty() {
        bail!(
            "could not hand back every fan:\n  - {}",
            failures.join("\n  - ")
        );
    }
    Ok(())
}

/// Switches `output` to its chip's automatic mode and returns that mode.
fn hand_to_chip(output: &BoundOutput) -> anyhow::Result<u8> {
    let mode = automatic_mode(output.chip()).ok_or_else(|| {
        anyhow!(
            "{}: this chip's automatic mode is unknown",
            output.describe()
        )
    })?;
    output
        .chip()
        .write_pwm_enable(output.pwm(), mode)
        .with_context(|| format!("cannot write {}", output.describe()))?;
    Ok(mode)
}
