//! Writing the state snapshot to `/run/onlycurves/state.json`.
//!
//! The daemon rewrites the file every tick, so it must live on tmpfs (RAM),
//! never on the SSD. Each write goes to a temporary file that is then renamed
//! over the old one, so a reader never sees half a file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use onlycurves_core::state::Snapshot;

const FILE_NAME: &str = "state.json";
const TEMP_NAME: &str = "state.json.tmp";

/// The state file in a directory known to be on tmpfs.
#[derive(Debug, Clone)]
pub struct StateFile {
    dir: PathBuf,
}

impl StateFile {
    /// Creates `dir` if needed and checks that it is on tmpfs.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created, its filesystem cannot be
    /// determined, or it is not tmpfs.
    pub fn prepare(dir: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let dir =
            fs::canonicalize(dir).with_context(|| format!("cannot resolve {}", dir.display()))?;
        let mountinfo = fs::read_to_string("/proc/self/mountinfo")
            .context("cannot read /proc/self/mountinfo")?;
        match filesystem_type(&mountinfo, &dir) {
            Some(fs_type) if fs_type == "tmpfs" => Ok(Self { dir }),
            Some(fs_type) => bail!(
                "{} is on {fs_type}, not tmpfs; the state file is rewritten every second and \
                 must not wear out a disk",
                dir.display()
            ),
            None => bail!("cannot tell which filesystem {} is on", dir.display()),
        }
    }

    /// The path readers use.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE_NAME)
    }

    /// Replaces the state file with `snapshot`.
    ///
    /// # Errors
    ///
    /// Fails when the snapshot cannot be serialized or written.
    pub fn write(&self, snapshot: &Snapshot) -> io::Result<()> {
        let json = snapshot.to_json().map_err(io::Error::other)?;
        let temp = self.dir.join(TEMP_NAME);
        fs::write(&temp, json)?;
        fs::rename(&temp, self.path())
    }
}

/// The type of the filesystem `path` (absolute, resolved) is on, from the text
/// of `/proc/self/mountinfo`: the mount with the longest mount point that
/// contains `path`.
///
/// A mountinfo line looks like
/// `36 35 0:31 / /run rw,nosuid shared:7 - tmpfs tmpfs rw,mode=755`:
/// field 5 is the mount point and the field after ` - ` the type.
fn filesystem_type(mountinfo: &str, path: &Path) -> Option<String> {
    mountinfo
        .lines()
        .filter_map(parse_mount_line)
        .filter(|(mount_point, _)| path.starts_with(mount_point))
        .max_by_key(|(mount_point, _)| mount_point.components().count())
        .map(|(_, fs_type)| fs_type)
}

/// The mount point and filesystem type of one mountinfo line.
fn parse_mount_line(line: &str) -> Option<(PathBuf, String)> {
    let (mount, filesystem) = line.split_once(" - ")?;
    let mount_point = mount.split(' ').nth(4)?;
    let fs_type = filesystem.split(' ').next()?;
    Some((PathBuf::from(unescape(mount_point)), fs_type.to_owned()))
}

/// Mountinfo writes spaces, tabs, newlines and backslashes as `\040`, `\011`,
/// `\012` and `\134`.
fn unescape(field: &str) -> String {
    field
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
23 1 0:22 / / rw,relatime shared:1 - btrfs /dev/nvme0n1p3 rw,ssd
24 23 0:23 / /run rw,nosuid,nodev shared:7 - tmpfs tmpfs rw,mode=755
25 24 0:24 / /run/user/1000 rw,nosuid shared:8 - tmpfs tmpfs rw,mode=700
26 23 0:25 / /mnt/my\\040disk rw shared:9 - ext4 /dev/sda1 rw
";

    #[test]
    fn the_longest_matching_mount_point_wins() {
        let fs_type = |path: &str| filesystem_type(MOUNTINFO, Path::new(path));

        assert_eq!(fs_type("/run/onlycurves").as_deref(), Some("tmpfs"));
        assert_eq!(fs_type("/var/lib/onlycurves").as_deref(), Some("btrfs"));
        assert_eq!(fs_type("/mnt/my disk/x").as_deref(), Some("ext4"));
    }

    #[test]
    fn mount_points_match_whole_components() {
        // /running is not inside /run.
        assert_eq!(
            filesystem_type(MOUNTINFO, Path::new("/running")).as_deref(),
            Some("btrfs")
        );
    }

    #[test]
    fn prepare_accepts_tmpfs_and_writes_atomically() {
        let shm = tempfile::tempdir_in("/dev/shm").unwrap();
        let state = StateFile::prepare(&shm.path().join("onlycurves")).unwrap();
        let snapshot = Snapshot {
            version: 1,
            unix_millis: 0,
            sensors: std::collections::BTreeMap::new(),
            fans: std::collections::BTreeMap::new(),
        };

        state.write(&snapshot).unwrap();

        let text = fs::read_to_string(state.path()).unwrap();
        assert_eq!(Snapshot::from_json(&text).unwrap(), snapshot);
        assert!(!state.dir.join(TEMP_NAME).exists());
    }

    #[test]
    fn prepare_refuses_a_disk() {
        let on_disk = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let error = StateFile::prepare(&on_disk).unwrap_err();

        assert!(error.to_string().contains("not tmpfs"), "{error}");
    }
}
