//! Finding one chip by its driver name and, when needed, its device path.

use std::fmt;
use std::path::Path;

use super::Chip;

/// Why a chip could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No chip has this name (and device path, if one was given).
    NotFound { name: String, path: Option<String> },
    /// Several chips share this name; a device path is needed to pick one.
    Ambiguous { name: String, devices: Vec<String> },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { name, path: None } => write!(f, "no hwmon chip named {name:?}"),
            Self::NotFound {
                name,
                path: Some(path),
            } => {
                write!(f, "no hwmon chip named {name:?} at device path {path:?}")
            }
            Self::Ambiguous { name, devices } => write!(
                f,
                "{} hwmon chips are named {name:?}; add a device path to pick one of: {}",
                devices.len(),
                devices.join(", ")
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Picks the one chip called `name` whose device path ends with `path`.
///
/// Without a `path`, the name alone must be unique.
///
/// # Errors
///
/// [`ResolveError::NotFound`] when nothing matches, [`ResolveError::Ambiguous`]
/// when more than one chip does.
pub fn resolve<'a>(
    chips: &'a [Chip],
    name: &str,
    path: Option<&str>,
) -> Result<&'a Chip, ResolveError> {
    let matches: Vec<&Chip> = chips
        .iter()
        .filter(|chip| chip.name() == name)
        .filter(|chip| path.is_none_or(|path| chip.device_matches(Path::new(path))))
        .collect();

    match matches.as_slice() {
        [chip] => Ok(chip),
        [] => Err(ResolveError::NotFound {
            name: name.to_owned(),
            path: path.map(str::to_owned),
        }),
        several => Err(ResolveError::Ambiguous {
            name: name.to_owned(),
            devices: several
                .iter()
                .map(|chip| chip.device_id().unwrap_or_else(|| "(no device)".to_owned()))
                .collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{discover, testing::FakeSysfs};
    use super::*;

    /// Two chips with the same driver name, as with two identical NVMe drives.
    fn two_nvme_drives() -> (FakeSysfs, Vec<Chip>) {
        let sysfs = FakeSysfs::new();
        sysfs.chip("hwmon0", "nvme", "pci0000:00/0000:01:00.0/nvme/nvme0");
        sysfs.chip("hwmon1", "nvme", "pci0000:00/0000:04:00.0/nvme/nvme1");
        sysfs.chip("hwmon7", "nct6798", "platform/nct6775.656");
        let chips = discover(&sysfs.hwmon_root()).unwrap();
        (sysfs, chips)
    }

    #[test]
    fn a_unique_name_is_enough() {
        let (_sysfs, chips) = two_nvme_drives();

        let chip = resolve(&chips, "nct6798", None).unwrap();

        assert_eq!(chip.device_id().as_deref(), Some("nct6775.656"));
    }

    #[test]
    fn a_shared_name_needs_a_path() {
        let (_sysfs, chips) = two_nvme_drives();

        let error = resolve(&chips, "nvme", None).unwrap_err();

        assert_eq!(
            error,
            ResolveError::Ambiguous {
                name: "nvme".into(),
                devices: vec!["nvme0".into(), "nvme1".into()]
            }
        );
    }

    #[test]
    fn a_path_picks_one_of_several() {
        let (_sysfs, chips) = two_nvme_drives();

        let chip = resolve(&chips, "nvme", Some("0000:04:00.0/nvme/nvme1")).unwrap();

        assert_eq!(chip.device_id().as_deref(), Some("nvme1"));
    }

    #[test]
    fn an_unknown_name_or_path_is_not_found() {
        let (_sysfs, chips) = two_nvme_drives();

        assert!(matches!(
            resolve(&chips, "it8686", None),
            Err(ResolveError::NotFound { .. })
        ));
        assert!(matches!(
            resolve(&chips, "nct6798", Some("nct6775.999")),
            Err(ResolveError::NotFound { .. })
        ));
    }
}
