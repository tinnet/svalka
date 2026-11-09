//! svalka: fan control for Linux.
//!
//! The binary in `main.rs` only parses the command line; everything it runs
//! lives in these modules.

pub mod binding;
pub mod check;
mod files;
pub mod history;
pub mod historyfile;
pub mod hwmon;
pub mod identify;
pub mod log;
pub mod notify;
pub mod record;
pub mod restore;
pub mod run;
pub mod setup;
pub mod signals;
pub mod statefile;
pub mod status;
pub mod sweep;
mod sysfs;
