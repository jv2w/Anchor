//! Small files read and written whole: the one way Anchor reads a file up to a size limit (the
//! settings file, the authorization cache, keys, packs, a world's `level.dat`, a crash log) and
//! the one way it replaces a small file so that a crash leaves the old or the new file, never a
//! mix (the settings file, the authorization cache, the identity key).

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Who may read a file [`write_atomic`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// The OS default for new files.
    Shared,
    /// Only the owner, on Unix (mode 0600); Windows gives new files the folder's permissions.
    OwnerOnly,
}

/// Reads the file at `path` whole if it holds at most `limit` bytes. A longer file is
/// [`io::ErrorKind::FileTooLarge`], found before anything is allocated when its length says so,
/// and while reading if it grows after; a missing file is [`io::ErrorKind::NotFound`], for the
/// caller to tell apart.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: callers run before the server starts or on the blocking pool"
)]
pub(crate) fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len > limit {
        return Err(io::ErrorKind::FileTooLarge.into());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_or(true, |read| read > limit) {
        return Err(io::ErrorKind::FileTooLarge.into());
    }
    Ok(bytes)
}

/// Writes `bytes` to `<path>.tmp` in the same folder, flushes it to disk, and renames it over
/// `path`. The folder must exist: a settings path in a missing folder is the caller's error, and
/// the data folder is made when the world file opens, before anything else is written there.
#[expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "off-tick: callers run before the server starts or on the blocking pool"
)]
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], access: Access) -> io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    match access {
        Access::Shared => {}
        #[cfg(unix)]
        Access::OwnerOnly => {
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        }
        #[cfg(not(unix))]
        Access::OwnerOnly => {}
    }
    let mut file = options.open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    // Windows cannot rename a file that is still open.
    drop(file);
    std::fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn atomic_write_leaves_old_or_new_file() {
        let dir = std::env::temp_dir().join(format!("anchor-fs-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).unwrap_or(());
        let path = dir.join("file.json");
        // No folder is created.
        assert!(write_atomic(&path, b"old", Access::OwnerOnly).is_err());
        std::fs::create_dir(&dir).unwrap();
        write_atomic(&path, b"old", Access::OwnerOnly).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        // A write that fails before the rename leaves the old file.
        let temp = dir.join("file.json.tmp");
        std::fs::create_dir(&temp).unwrap();
        assert!(write_atomic(&path, b"new", Access::Shared).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        std::fs::remove_dir(&temp).unwrap();
        // A whole write replaces it and leaves no temporary file.
        write_atomic(&path, b"new", Access::Shared).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(!temp.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_bounded_read_takes_files_up_to_its_limit_and_tells_missing_from_too_large() {
        let dir = std::env::temp_dir().join(format!("anchor-fs-read-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).unwrap_or(());
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("file");
        let missing = read_bounded(&path, 4).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        std::fs::write(&path, b"1234").unwrap();
        assert_eq!(read_bounded(&path, 4).unwrap(), b"1234");
        std::fs::write(&path, b"12345").unwrap();
        let too_large = read_bounded(&path, 4).unwrap_err();
        assert_eq!(too_large.kind(), io::ErrorKind::FileTooLarge);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
