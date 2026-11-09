//! The history files in the state directory: `history`, which the daemon
//! appends one line to every tick, and `history.old`, the one before it.
//!
//! When `history` reaches [`SEGMENT_LINES`] it is renamed over `history.old`
//! and a new one begins, so readers always find between one and two segments
//! of ticks: 15–30 minutes at a 1 s interval. A tick with two fans is about
//! 0.5 KB, so that is under 1 MB. Like the state file they live on tmpfs, and
//! the daemon never reads them back, except to count the lines of `history`
//! when it starts. They are kept when the daemon stops, so a restart does not
//! lose the recent past.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use svalka_core::history::{self, Line};

/// Lines per file before it is rotated.
pub const SEGMENT_LINES: usize = 900;

const CURRENT: &str = "history";
const OLD: &str = "history.old";

/// The daemon's side: appending, and rotating when a file is full.
#[derive(Debug)]
pub struct HistoryFile {
    current: PathBuf,
    old: PathBuf,
    segment_lines: usize,
    /// Lines in `current`.
    lines: usize,
    /// Whether `current` ends in the middle of a line, e.g. because the
    /// daemon was killed while writing it. The next line must not be glued
    /// onto it.
    unfinished: bool,
}

impl HistoryFile {
    /// Carries on with the history already in `dir`, if any.
    #[must_use]
    pub fn open(dir: &Path) -> Self {
        Self::with_segment_lines(dir, SEGMENT_LINES)
    }

    fn with_segment_lines(dir: &Path, segment_lines: usize) -> Self {
        let current = dir.join(CURRENT);
        // A file that cannot be read is treated as full: the first append
        // rotates it away and starts clean.
        let (lines, unfinished) = match fs::read(&current) {
            Ok(text) => (
                text.split(|&byte| byte == b'\n').count() - 1,
                text.last().is_some_and(|&byte| byte != b'\n'),
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (0, false),
            Err(_) => (segment_lines, false),
        };
        Self {
            current,
            old: dir.join(OLD),
            segment_lines,
            lines,
            unfinished,
        }
    }

    /// The file being appended to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.current
    }

    /// Appends `line`, first rotating `history` to `history.old` if it is full.
    ///
    /// # Errors
    ///
    /// Fails when the line cannot be serialized, or the file cannot be
    /// rotated or written.
    pub fn append(&mut self, line: &Line) -> io::Result<()> {
        if self.lines >= self.segment_lines {
            self.rotate()?;
        }
        let mut text = String::new();
        if self.unfinished {
            text.push('\n');
        }
        text.push_str(&line.to_json().map_err(io::Error::other)?);
        text.push('\n');
        // One write per line, so a reader sees whole lines but for, at most,
        // the one being written, which it skips.
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.current)?;
        file.write_all(text.as_bytes())?;
        self.lines += 1;
        self.unfinished = false;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        match fs::rename(&self.current, &self.old) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        self.lines = 0;
        self.unfinished = false;
        Ok(())
    }
}

/// Every tick in `dir`'s history files, oldest first.
///
/// `history` is read before `history.old`. If the daemon rotates in between,
/// the same segment is read twice and counted once, so the result covers
/// less time but is never missing recent ticks.
///
/// # Errors
///
/// Fails when a file exists but cannot be read.
pub fn read(dir: &Path) -> io::Result<Vec<Line>> {
    let current = read_if_present(&dir.join(CURRENT))?;
    let old = read_if_present(&dir.join(OLD))?;
    Ok(history::merge([
        history::parse(&old),
        history::parse(&current),
    ]))
}

fn read_if_present(path: &Path) -> io::Result<String> {
    match fs::read(path) {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn line(unix_millis: u64) -> Line {
        Line {
            version: history::VERSION,
            unix_millis,
            readback: false,
            sensors: BTreeMap::new(),
            fans: BTreeMap::new(),
        }
    }

    fn millis(lines: &[Line]) -> Vec<u64> {
        lines.iter().map(|line| line.unix_millis).collect()
    }

    #[test]
    fn appends_and_rotates_keeping_one_to_two_segments() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = HistoryFile::with_segment_lines(dir.path(), 3);

        for tick in 1..=7 {
            file.append(&line(tick)).unwrap();
        }

        // history.old holds 4–6, history holds 7.
        assert_eq!(millis(&read(dir.path()).unwrap()), [4, 5, 6, 7]);
        assert_eq!(fs::read_to_string(file.path()).unwrap().lines().count(), 1);
    }

    #[test]
    fn a_restarted_daemon_carries_on_counting() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = HistoryFile::with_segment_lines(dir.path(), 3);
        first.append(&line(1)).unwrap();
        first.append(&line(2)).unwrap();

        let mut second = HistoryFile::with_segment_lines(dir.path(), 3);
        second.append(&line(3)).unwrap();
        second.append(&line(4)).unwrap();

        assert_eq!(millis(&read(dir.path()).unwrap()), [1, 2, 3, 4]);
        assert_eq!(
            fs::read_to_string(second.path()).unwrap().lines().count(),
            1
        );
    }

    #[test]
    fn a_half_written_line_is_not_glued_to_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let whole = line(1).to_json().unwrap();
        fs::write(
            dir.path().join(CURRENT),
            format!("{whole}\n{{\"version\":1,\"unix"),
        )
        .unwrap();

        HistoryFile::open(dir.path()).append(&line(2)).unwrap();

        assert_eq!(millis(&read(dir.path()).unwrap()), [1, 2]);
    }

    #[test]
    fn no_history_is_empty() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(read(dir.path()).unwrap(), Vec::<Line>::new());
    }
}
