//! Log lines on stderr.
//!
//! Under systemd, stderr goes to the journal, which reads a `<N>` prefix as the
//! line's syslog priority. In a terminal that prefix is noise, so it is only
//! added when `JOURNAL_STREAM` says stderr is the journal.

use std::fmt::{Display, Write as _};
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
    eprint!(
        "{}",
        format_message(level, &message.to_string(), to_journal())
    );
}

/// The text to print. journald reads each line as its own entry, so for the
/// journal every line gets the priority prefix, or a multi-line error (a list
/// of config problems) would put all but its first line at the default
/// priority, hidden from `journalctl -p err`.
fn format_message(level: Level, message: &str, journal: bool) -> String {
    if journal {
        let mut text = String::new();
        for line in message.lines() {
            let _ = writeln!(text, "<{}>{line}", level.priority());
        }
        text
    } else {
        format!("{}{message}\n", level.word())
    }
}

/// systemd sets `JOURNAL_STREAM` when it connects stderr to the journal.
fn to_journal() -> bool {
    static JOURNAL: OnceLock<bool> = OnceLock::new();
    *JOURNAL.get_or_init(|| std::env::var_os("JOURNAL_STREAM").is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_line_gets_the_priority_for_the_journal() {
        let message = "reload failed: 2 problem(s):\n  - fan.a: x\n  - fan.b: y";

        assert_eq!(
            format_message(Level::Error, message, true),
            "<3>reload failed: 2 problem(s):\n<3>  - fan.a: x\n<3>  - fan.b: y\n"
        );
    }

    #[test]
    fn a_terminal_gets_one_word_up_front() {
        assert_eq!(
            format_message(Level::Warning, "a\n  b", false),
            "warning: a\n  b\n"
        );
        assert_eq!(format_message(Level::Info, "stopping", false), "stopping\n");
    }
}
