//! onlycurves: fan control for one PC.
//!
//! The binary in `main.rs` only parses the command line; everything it runs
//! lives in these modules.

pub mod binding;
pub mod check;
pub mod hwmon;
pub mod setup;
pub mod status;
mod sysfs;
