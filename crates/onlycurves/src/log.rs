//! Log lines on stderr.
//!
//! Under systemd, stderr goes to the journal, which reads a `<N>` prefix as the
//! line's syslog priority. In a terminal that prefix is noise, so it is only
//! added when `JOURNAL_STREAM` says stderr is the journal.

use std::fmt::Display;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy)]
enum Level {
    Error,
    Warning,
    Info,
}

impl Level {
    /// syslog priority, as journald reads it from a `<N>` prefix.
    fn priority(self) -> u8 {
        match self {
            Self::Error => 3,
            Self::Warning => 4,
            Self::Info => 6,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::Error => "error: ",
            Self::Warning => "warning: ",
            Self::Info => "",
        }
    }
}

pub fn error(message: impl Display) {
    emit(Level::Error, message);
}

pub fn warning(message: impl Display) {
    emit(Level::Warning, message);
}

pub fn info(message: impl Display) {
    emit(Level::Info, message);
}

fn emit(level: Level, message: impl Display) {
    if to_journal() {
        eprintln!("<{}>{message}", level.priority());
    } else {
        eprintln!("{}{message}", level.word());
    }
}

/// systemd sets `JOURNAL_STREAM` when it connects stderr to the journal.
fn to_journal() -> bool {
    static JOURNAL: OnceLock<bool> = OnceLock::new();
    *JOURNAL.get_or_init(|| std::env::var_os("JOURNAL_STREAM").is_some())
}
