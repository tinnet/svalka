//! Turning signals into flags that long-running commands check, so they can
//! hand outputs back before exiting, or reload, at a safe point.

use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

/// Returns a flag that becomes `true` on Ctrl-C (SIGINT), SIGTERM or SIGHUP.
/// For commands without a config to reload, such as `identify`.
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

/// The daemon's flags: `stop` on SIGINT or SIGTERM, `reload` on SIGHUP.
#[derive(Debug, Clone)]
pub struct DaemonSignals {
    pub stop: Arc<AtomicBool>,
    pub reload: Arc<AtomicBool>,
}

impl DaemonSignals {
    /// Registers the handlers.
    ///
    /// # Errors
    ///
    /// Fails when a handler cannot be registered.
    pub fn register() -> io::Result<Self> {
        let signals = Self::unregistered();
        for signal in [SIGINT, SIGTERM] {
            signal_hook::flag::register(signal, Arc::clone(&signals.stop))?;
        }
        signal_hook::flag::register(SIGHUP, Arc::clone(&signals.reload))?;
        Ok(signals)
    }

    /// Flags that no signal sets, for tests that set them by hand.
    #[must_use]
    pub fn unregistered() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            reload: Arc::new(AtomicBool::new(false)),
        }
    }
}
