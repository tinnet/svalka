//! Turning signals into flags that long-running commands check, so they can
//! hand outputs back before exiting, or reload, at a safe point.

use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

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

/// The daemon's flags, `stop` on SIGINT or SIGTERM and `reload` on SIGHUP,
/// plus a self-pipe the same signals write to, so [`DaemonSignals::sleep`]
/// wakes the moment one arrives instead of polling the flags.
#[derive(Debug)]
pub struct DaemonSignals {
    pub stop: Arc<AtomicBool>,
    pub reload: Arc<AtomicBool>,
    wake: UnixStream,
    /// The self-pipe's write end. Signal handlers write to clones of it; this
    /// one stays open so `wake` never reads end-of-file.
    waker: UnixStream,
}

impl DaemonSignals {
    /// Registers the handlers.
    ///
    /// # Errors
    ///
    /// Fails when the self-pipe cannot be created or a handler registered.
    pub fn register() -> io::Result<Self> {
        let signals = Self::unregistered()?;
        // Flags first: a handler runs its actions in registration order, so a
        // woken sleeper always finds its flag already set.
        for signal in [SIGINT, SIGTERM] {
            signal_hook::flag::register(signal, Arc::clone(&signals.stop))?;
        }
        signal_hook::flag::register(SIGHUP, Arc::clone(&signals.reload))?;
        for signal in [SIGINT, SIGTERM, SIGHUP] {
            signal_hook::low_level::pipe::register(signal, signals.waker.try_clone()?)?;
        }
        Ok(signals)
    }

    /// Flags and a self-pipe that no signal touches, for tests that set the
    /// flags by hand.
    ///
    /// # Errors
    ///
    /// Fails when the self-pipe cannot be created.
    pub fn unregistered() -> io::Result<Self> {
        let (wake, waker) = UnixStream::pair()?;
        Ok(Self {
            stop: Arc::new(AtomicBool::new(false)),
            reload: Arc::new(AtomicBool::new(false)),
            wake,
            waker,
        })
    }

    /// Sleeps for `timeout`, or less when a signal arrives.
    pub fn sleep(&self, timeout: Duration) {
        // A zero timeout would mean "block forever".
        let timeout = timeout.max(Duration::from_millis(1));
        if self.wake.set_read_timeout(Some(timeout)).is_ok() {
            // Woken by a signal, or timed out: the caller checks the flags either way.
            let _ = (&self.wake).read(&mut [0; 64]);
        }
    }
}
