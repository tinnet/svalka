//! Lending one fan output from `svalka run` to `sweep` or `identify`, so they
//! can run without stopping the daemon.
//!
//! The daemon takes no requests over a socket or API. A borrower writes a loan
//! file, `/run/svalka/loans/<pid>`, and waits until the daemon's record of held
//! outputs (see [`crate::record`]) no longer lists the output. The daemon reads
//! the loan files every tick: it hands a lent output back to the chip, stops
//! writing it, and takes it back once the loan file is gone or the borrower is
//! no longer running, so a borrower that crashes cannot keep a fan forever.
//!
//! Only root can write `/run/svalka`, so only root can borrow a fan.
//!
//! A loan file holds one line, `<purpose> <start time> <pwm> <chip directory>`,
//! for example `sweep 81234 1 /sys/class/hwmon/hwmon7`. The start time is the
//! borrower's from `/proc/<pid>/stat`: with it, a later process that happens to
//! get the same pid does not count as the borrower.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

use crate::binding::BoundOutput;
use crate::files;
use crate::record::OutputRecord;

const DIR_NAME: &str = "loans";

/// How often a borrower checks whether the daemon has let go.
const POLL: Duration = Duration::from_millis(100);

/// How long a borrower waits for a daemon that ticks every `interval`: it
/// reads the loans once a tick, so two ticks and a second are plenty.
#[must_use]
pub fn wait_for_daemon(interval: Duration) -> Duration {
    interval * 2 + Duration::from_secs(1)
}

/// A borrower's request, as the daemon reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub pid: u32,
    /// What the output is lent to, e.g. `sweep`.
    pub purpose: String,
    pub chip_dir: PathBuf,
    pub pwm: u32,
}

impl Request {
    /// Whether this request is for `output`.
    #[must_use]
    pub fn is_for(&self, output: &BoundOutput) -> bool {
        self.chip_dir == output.chip().dir() && self.pwm == output.pwm()
    }
}

/// A borrowed output. Dropping it ends the loan: the daemon takes the output
/// back on its next tick.
#[derive(Debug)]
#[must_use = "dropping a loan hands the output back to the daemon at once"]
pub struct Loan {
    /// The loan file; `None` when no daemon held the output.
    path: Option<PathBuf>,
}

impl Loan {
    /// Whether a running daemon lent the output, as opposed to nobody holding it.
    #[must_use]
    pub fn from_daemon(&self) -> bool {
        self.path.is_some()
    }
}

impl Drop for Loan {
    fn drop(&mut self) {
        if let Some(path) = &self.path
            && let Err(error) = files::remove_if_present(path)
        {
            eprintln!("cannot remove {}: {error}", path.display());
        }
    }
}

/// Borrows `output` from `svalka run` for `purpose`, if the daemon holds it,
/// waiting up to `wait` for the daemon to let go.
///
/// # Errors
///
/// Fails when the record or loan file cannot be read or written (writing
/// needs root), when `stop` is set while waiting, or when the daemon does not
/// let go in time, e.g. because a stale record lists the output, or an older
/// daemon that cannot lend fans holds it.
pub fn borrow(
    output: &BoundOutput,
    state_dir: &Path,
    purpose: &str,
    wait: Duration,
    stop: &AtomicBool,
) -> anyhow::Result<Loan> {
    if !held_by_daemon(output, state_dir)? {
        return Ok(Loan { path: None });
    }
    let dir = state_dir.join(DIR_NAME);
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let pid = process::id();
    let path = dir.join(pid.to_string());
    let start_time = start_time(pid).context("cannot read this process's start time")?;
    let line = format!(
        "{purpose} {start_time} {} {}\n",
        output.pwm(),
        output.chip().dir().display()
    );
    files::write_atomically(&path, line)
        .with_context(|| format!("cannot write {}", path.display()))?;
    let loan = Loan { path: Some(path) };

    let deadline = Instant::now() + wait;
    while held_by_daemon(output, state_dir)? {
        if stop.load(Ordering::Relaxed) {
            bail!("stopped while waiting for svalka run to lend the fan");
        }
        if Instant::now() >= deadline {
            bail!(
                "svalka run did not lend {} within {} s. If it is running, it may be an \
                 older version that cannot lend fans: stop it first with `sudo systemctl \
                 stop svalka`. If it is not, a stale record lists the output: run `sudo \
                 svalka restore`.",
                output.describe(),
                wait.as_secs()
            );
        }
        thread::sleep(POLL);
    }
    Ok(loan)
}

fn held_by_daemon(output: &BoundOutput, state_dir: &Path) -> anyhow::Result<bool> {
    let record = OutputRecord::in_dir(state_dir);
    let held = record
        .read()
        .with_context(|| format!("cannot read {}", record.path().display()))?
        .unwrap_or_default();
    Ok(held
        .iter()
        .any(|held| held.chip_dir == output.chip().dir() && held.pwm == output.pwm()))
}

/// The loans of borrowers still running. Loan files of borrowers that are
/// gone are removed, so they cannot pile up.
///
/// # Errors
///
/// Fails when the loan directory exists but cannot be listed.
pub fn live_requests(state_dir: &Path) -> io::Result<Vec<Request>> {
    let dir = state_dir.join(DIR_NAME);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut requests = Vec::new();
    for entry in entries {
        let path = entry?.path();
        match read_request(&path) {
            Some((request, start_time)) if start_time_matches(request.pid, start_time) => {
                requests.push(request);
            }
            // A borrower that is gone, or a file being written: only the
            // first is old enough to remove.
            _ if !is_temporary(&path) => files::remove_if_present(&path)?,
            _ => {}
        }
    }
    requests.sort_by_key(|request| request.pid);
    Ok(requests)
}

fn is_temporary(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "tmp")
}

/// The request in a loan file and the borrower's start time.
fn read_request(path: &Path) -> Option<(Request, u64)> {
    let pid = path.file_name()?.to_str()?.parse().ok()?;
    let text = fs::read_to_string(path).ok()?;
    let mut fields = text.trim_end_matches('\n').splitn(4, ' ');
    let purpose = fields.next()?.to_owned();
    let start_time = fields.next()?.parse().ok()?;
    let pwm = fields.next()?.parse().ok()?;
    let chip_dir = PathBuf::from(fields.next()?);
    let request = Request {
        pid,
        purpose,
        chip_dir,
        pwm,
    };
    Some((request, start_time))
}

fn start_time_matches(pid: u32, expected: u64) -> bool {
    start_time(pid).is_ok_and(|start_time| start_time == expected)
}

/// When process `pid` started, in clock ticks since boot: field 22 of
/// `/proc/<pid>/stat`. The command name (field 2) may hold spaces and
/// parentheses, so fields are counted after its last `)`.
fn start_time(pid: u32) -> io::Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "unexpected /proc stat format");
    let (_, after_name) = stat.rsplit_once(')').ok_or_else(invalid)?;
    // after_name starts with field 3, so field 22 is the 20th.
    after_name
        .split_whitespace()
        .nth(19)
        .and_then(|field| field.parse().ok())
        .ok_or_else(invalid)
}

/// Writes a loan file for `pid` as a borrower would, for tests of the daemon.
#[cfg(test)]
pub(crate) fn write_request_for_test(state_dir: &Path, pid: u32, output: &BoundOutput) {
    let dir = state_dir.join(DIR_NAME);
    fs::create_dir_all(&dir).unwrap();
    let start_time = start_time(pid).unwrap_or(0);
    fs::write(
        dir.join(pid.to_string()),
        format!(
            "test {start_time} {} {}\n",
            output.pwm(),
            output.chip().dir().display()
        ),
    )
    .unwrap();
}

/// Removes `pid`'s loan file, for tests of the daemon.
#[cfg(test)]
pub(crate) fn remove_request_for_test(state_dir: &Path, pid: u32) {
    fs::remove_file(state_dir.join(DIR_NAME).join(pid.to_string())).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        fs::create_dir_all(dir.join(DIR_NAME)).unwrap();
        fs::write(dir.join(DIR_NAME).join(name), text).unwrap();
    }

    #[test]
    fn this_process_has_a_start_time() {
        assert!(start_time(process::id()).is_ok());
    }

    #[test]
    fn a_running_borrower_counts_and_a_gone_one_is_cleaned_up() {
        let state = tempfile::tempdir().unwrap();
        let me = process::id();
        let mine = start_time(me).unwrap();
        let mut child = process::Command::new("true").spawn().unwrap();
        let gone = child.id();
        child.wait().unwrap();
        write(
            state.path(),
            &me.to_string(),
            &format!("sweep {mine} 1 /sys/class/hwmon/hwmon7\n"),
        );
        write(
            state.path(),
            &gone.to_string(),
            "identify 1 2 /sys/class/hwmon/hwmon7\n",
        );

        let requests = live_requests(state.path()).unwrap();

        assert_eq!(
            requests,
            [Request {
                pid: me,
                purpose: "sweep".into(),
                chip_dir: "/sys/class/hwmon/hwmon7".into(),
                pwm: 1,
            }]
        );
        assert!(!state.path().join(DIR_NAME).join(gone.to_string()).exists());
    }

    #[test]
    fn a_reused_pid_is_not_the_borrower() {
        let state = tempfile::tempdir().unwrap();
        let me = process::id();
        let not_mine = start_time(me).unwrap() + 1;
        write(
            state.path(),
            &me.to_string(),
            &format!("sweep {not_mine} 1 /sys/class/hwmon/hwmon7\n"),
        );

        assert_eq!(live_requests(state.path()).unwrap(), Vec::new());
    }

    #[test]
    fn no_loans_directory_means_no_loans() {
        let state = tempfile::tempdir().unwrap();

        assert_eq!(live_requests(state.path()).unwrap(), Vec::new());
    }
}
