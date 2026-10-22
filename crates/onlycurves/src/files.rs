//! The two file operations the state directory needs.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Replaces `path` with `contents`: writes `<path>.tmp`, then renames it over
/// `path`, so a reader sees the old file or the new one, never half of one.
///
/// # Errors
///
/// Fails when the temporary file cannot be written or renamed.
pub fn write_atomically(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let temp = temp_path(path);
    fs::write(&temp, contents)?;
    fs::rename(&temp, path)
}

/// Deletes `path`. A file that is already gone is fine.
///
/// # Errors
///
/// Fails when the file exists but cannot be deleted.
pub fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let mut temp = OsString::from(path);
    temp.push(".tmp");
    PathBuf::from(temp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_through_a_temporary_file_and_removes_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        write_atomically(&path, "{}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
        assert!(!dir.path().join("state.json.tmp").exists());

        remove_if_present(&path).unwrap();
        assert!(!path.exists());
        remove_if_present(&path).unwrap(); // already gone is fine
    }
}
