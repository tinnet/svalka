//! Turning SIGINT, SIGTERM and SIGHUP into a flag that long-running commands
//! check, so they can hand outputs back before exiting.
//!
//! SIGHUP stops too until config reload exists (milestone 6): its default
//! action would kill the process without handing the fans back.

use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

/// Returns a flag that becomes `true` on Ctrl-C (SIGINT), SIGTERM or SIGHUP.
///
/// From then on these signals no longer end the process; the caller must
/// check the flag and exit on its own.
///
/// # Errors
///
/// Fails when a handler cannot be registered.
pub fn stop_flag() -> io::Result<Arc<AtomicBool>> {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [SIGINT, SIGTERM, SIGHUP] {
        signal_hook::flag::register(signal, Arc::clone(&stop))?;
    }
    Ok(stop)
}
