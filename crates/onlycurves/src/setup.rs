//! Loading the config and binding it to this PC: the first step of every
//! command that uses a config.

use std::fs;
use std::path::Path;

use anyhow::{Context, anyhow};
use onlycurves_core::config::{Config, ConfigError};

use crate::binding::{self, Bindings};
use crate::hwmon;

/// A valid config, tied to this PC's hardware.
#[derive(Debug, Clone)]
pub struct Setup {
    pub config: Config,
    pub bindings: Bindings,
}

impl Setup {
    /// Loads `config_path` and binds it to the chips under `hwmon_root`.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read, the config is invalid, or it does
    /// not match the hardware. Problems are listed one per line.
    pub fn load(config_path: &Path, hwmon_root: &Path) -> anyhow::Result<Self> {
        let config = load_config(config_path)?;
        let chips = hwmon::discover(hwmon_root)
            .with_context(|| format!("cannot list hwmon chips in {}", hwmon_root.display()))?;
        let bindings = binding::bind(&config, &chips)
            .map_err(|problems| invalid(config_path, &ConfigError::Invalid(problems)))?;
        Ok(Self { config, bindings })
    }
}

/// Reads and validates the config file, without looking at the hardware.
///
/// # Errors
///
/// Fails when the file cannot be read or the config is invalid.
pub fn load_config(path: &Path) -> anyhow::Result<Config> {
    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    Config::parse(&text).map_err(|error| invalid(path, &error))
}

/// E.g. `/etc/onlycurves.toml: 2 problem(s): …`, with every problem listed.
fn invalid(path: &Path, error: &ConfigError) -> anyhow::Error {
    anyhow!("{}: {error}", path.display())
}
