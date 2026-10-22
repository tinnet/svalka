//! Telling systemd how the daemon is doing (`sd_notify`), written by hand:
//! each message is one datagram of `KEY=value` lines to `$NOTIFY_SOCKET`.
//! <https://www.freedesktop.org/software/systemd/man/latest/sd_notify.html>
//!
//! Outside systemd `NOTIFY_SOCKET` is unset and every call does nothing.

use std::env;
use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::time::Duration;

use rustix::time::{ClockId, clock_gettime};

/// A connection to systemd's notification socket, if there is one.
#[derive(Debug)]
pub struct Notifier {
    target: Option<(UnixDatagram, SocketAddr)>,
    watchdog: bool,
}

impl Notifier {
    /// Reads `NOTIFY_SOCKET`, and `WATCHDOG_USEC` / `WATCHDOG_PID` for whether
    /// systemd expects watchdog pings from this process.
    ///
    /// # Errors
    ///
    /// Fails when `NOTIFY_SOCKET` is set but no socket can be created for it.
    pub fn from_env() -> io::Result<Self> {
        let Some(socket) = env::var_os("NOTIFY_SOCKET") else {
            return Ok(Self::disabled());
        };
        let address = socket_address(&socket.to_string_lossy())?;
        Ok(Self {
            target: Some((UnixDatagram::unbound()?, address)),
            watchdog: watchdog_expected(),
        })
    }

    /// A notifier that sends nothing.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            target: None,
            watchdog: false,
        }
    }

    /// Startup or a reload finished.
    pub fn ready(&self, status: &str) {
        self.send(&format!("READY=1\nSTATUS={status}"));
    }

    /// One tick went through: pets the watchdog and updates the status line.
    pub fn tick(&self, status: &str) {
        if self.watchdog {
            self.send(&format!("WATCHDOG=1\nSTATUS={status}"));
        } else {
            self.send(&format!("STATUS={status}"));
        }
    }

    /// A reload starts. systemd needs the monotonic time with it.
    pub fn reloading(&self) {
        self.send(&format!(
            "RELOADING=1\nMONOTONIC_USEC={}",
            monotonic_micros()
        ));
    }

    /// Shutdown starts.
    pub fn stopping(&self) {
        self.send("STOPPING=1");
    }

    /// Sends one message. Failures are ignored: telling systemd is best
    /// effort, and a watchdog that misses pings restarts us anyway.
    fn send(&self, message: &str) {
        if let Some((socket, address)) = &self.target {
            let _ = socket.send_to_addr(message.as_bytes(), address);
        }
    }

    /// For tests: a notifier sending to `address`.
    #[cfg(test)]
    fn to(address: SocketAddr, watchdog: bool) -> Self {
        Self {
            target: Some((UnixDatagram::unbound().unwrap(), address)),
            watchdog,
        }
    }
}

/// `NOTIFY_SOCKET` is a path, or an abstract socket name written as `@name`.
fn socket_address(socket: &str) -> io::Result<SocketAddr> {
    match socket.strip_prefix('@') {
        Some(name) => SocketAddr::from_abstract_name(name),
        None => SocketAddr::from_pathname(socket),
    }
}

/// Whether systemd set a watchdog for this process: `WATCHDOG_USEC` is set and,
/// if `WATCHDOG_PID` is too, it is our pid.
fn watchdog_expected() -> bool {
    let has_interval = env::var("WATCHDOG_USEC")
        .ok()
        .and_then(|usec| usec.parse::<u64>().ok())
        .is_some_and(|usec| usec > 0);
    let for_us = env::var("WATCHDOG_PID").map_or(true, |pid| {
        pid.parse::<u32>().ok() == Some(std::process::id())
    });
    has_interval && for_us
}

/// `CLOCK_MONOTONIC` in microseconds, as `MONOTONIC_USEC` wants it.
fn monotonic_micros() -> u128 {
    let now = clock_gettime(ClockId::Monotonic);
    let since_boot = Duration::new(
        u64::try_from(now.tv_sec).unwrap_or(0),
        u32::try_from(now.tv_nsec).unwrap_or(0),
    );
    since_boot.as_micros()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bound socket standing in for systemd, and a notifier pointed at it.
    fn systemd(watchdog: bool) -> (tempfile::TempDir, UnixDatagram, Notifier) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let listener = UnixDatagram::bind(&path).unwrap();
        let notifier = Notifier::to(SocketAddr::from_pathname(&path).unwrap(), watchdog);
        (dir, listener, notifier)
    }

    fn receive(listener: &UnixDatagram) -> String {
        let mut buffer = [0; 512];
        let length = listener.recv(&mut buffer).unwrap();
        String::from_utf8_lossy(&buffer[..length]).into_owned()
    }

    #[test]
    fn sends_ready_with_a_status() {
        let (_dir, listener, notifier) = systemd(false);

        notifier.ready("controlling cpu_cooler");

        assert_eq!(receive(&listener), "READY=1\nSTATUS=controlling cpu_cooler");
    }

    #[test]
    fn ticks_pet_the_watchdog_only_when_one_is_set() {
        let (_dir, listener, notifier) = systemd(true);
        notifier.tick("cpu_cooler 47 %");
        assert_eq!(receive(&listener), "WATCHDOG=1\nSTATUS=cpu_cooler 47 %");

        let (_dir, listener, notifier) = systemd(false);
        notifier.tick("cpu_cooler 47 %");
        assert_eq!(receive(&listener), "STATUS=cpu_cooler 47 %");
    }

    #[test]
    fn reloading_carries_the_monotonic_time() {
        let (_dir, listener, notifier) = systemd(false);

        notifier.reloading();

        let message = receive(&listener);
        let micros = message
            .strip_prefix("RELOADING=1\nMONOTONIC_USEC=")
            .unwrap();
        assert!(micros.parse::<u128>().unwrap() > 0);
    }

    #[test]
    fn abstract_names_start_with_an_at_sign() {
        let address = socket_address("@/org/freedesktop/systemd1/notify").unwrap();

        assert_eq!(
            address.as_abstract_name(),
            Some(&b"/org/freedesktop/systemd1/notify"[..])
        );
    }

    #[test]
    fn a_disabled_notifier_sends_nothing() {
        Notifier::disabled().ready("nothing to see");
    }
}
