//! onlycurves: fan control for one PC.
//!
//! The binary in `main.rs` only parses the command line; everything it runs
//! lives in these modules.

pub mod hwmon;
pub mod status;
mod sysfs;
