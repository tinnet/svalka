//! Reading sysfs attribute files.
//!
//! Every sysfs attribute holds exactly one value as text, usually followed by
//! a newline. These helpers read one such file and parse its value.

use std::fs;
use std::io;
use std::path::Path;
use std::str::FromStr;

/// Reads an attribute file and returns its contents without surrounding whitespace.
pub fn read_string(path: &Path) -> io::Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

/// Reads an attribute file and parses it as a number (or any other `FromStr` type).
pub fn read_value<T: FromStr>(path: &Path) -> io::Result<T> {
    let text = read_string(path)?;
    text.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: cannot parse {text:?}", path.display()),
        )
    })
}

/// Writes one value to an attribute file.
pub fn write_value(path: &Path, value: impl std::fmt::Display) -> io::Result<()> {
    fs::write(path, value.to_string())
}

/// Parses the index out of an indexed attribute name such as `temp3_input`.
///
/// Returns `Some(3)` for `("temp3_input", "temp", "_input")`, and `None` when the
/// name has a different prefix or suffix, or no number between them.
pub fn attribute_index(file_name: &str, prefix: &str, suffix: &str) -> Option<u32> {
    file_name
        .strip_prefix(prefix)?
        .strip_suffix(suffix)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_string_trims_the_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("name");
        fs::write(&file, "nct6798\n").unwrap();

        assert_eq!(read_string(&file).unwrap(), "nct6798");
    }

    #[test]
    fn read_value_parses_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("temp1_input");
        fs::write(&file, "49125\n").unwrap();

        assert_eq!(read_value::<i64>(&file).unwrap(), 49_125);
    }

    #[test]
    fn read_value_reports_unparsable_contents_as_invalid_data() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pwm1");
        fs::write(&file, "fast\n").unwrap();

        let error = read_value::<u8>(&file).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn attribute_index_extracts_the_number() {
        assert_eq!(attribute_index("temp3_input", "temp", "_input"), Some(3));
        assert_eq!(attribute_index("pwm12", "pwm", ""), Some(12));
    }

    #[test]
    fn attribute_index_rejects_other_attributes() {
        assert_eq!(attribute_index("temp3_label", "temp", "_input"), None);
        assert_eq!(attribute_index("pwm1_enable", "pwm", ""), None);
        assert_eq!(attribute_index("temp_input", "temp", "_input"), None);
    }
}
