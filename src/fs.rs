//! Small files read and written whole: the one way Anchor reads a file up to a size limit (the
//! settings file, the authorization cache, keys, packs, a world's `level.dat`, a crash log) and
//! the one way it replaces a small file so that a crash leaves the old or the new file, never a
//! mix (the settings file, the authorization cache, the identity key); and the one way it flushes
//! a folder's entries, so a file created, renamed or removed there stays so after a crash.

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
    #[expect(
        clippy::disallowed_methods,
        reason = "safety: `take` bounds the read to `limit + 1` bytes"
    )]
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_or(true, |read| read > limit) {
        return Err(io::ErrorKind::FileTooLarge.into());
    }
    Ok(bytes)
}

/// Writes `bytes` to a new `<path>.tmp` in the same folder, flushes it to disk, renames it over
/// `path`, and flushes the folder ([`sync_parent`]). The folder must exist: a settings path in a
/// missing folder is the caller's error, and the data folder is made when the first data file
/// opens, before anything else is written there.
#[expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "off-tick: callers run before the server starts or on the blocking pool"
)]
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], access: Access) -> io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    // A temporary file a crash left keeps the mode it was made with, and whoever opened it then
    // could read the new bytes through that handle: the bytes go to a new file instead.
    match std::fs::remove_file(&temp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
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
    std::fs::rename(&temp, path)?;
    sync_parent(path)
}

/// Makes the folder `dir` and its missing parents, and flushes the entry of each folder it made
/// into its parent ([`sync_parent`]), so the folders stay after a crash or power loss.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: callers run before the server starts, on the blocking pool or on the save thread"
)]
pub(crate) fn create_dir_all_durable(dir: &Path) -> io::Result<()> {
    if dir.as_os_str().is_empty() || dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        create_dir_all_durable(parent)?;
    }
    match std::fs::create_dir(dir) {
        Ok(()) => sync_parent(dir),
        // Another thread or process made it between the check and the call.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Flushes the entries of the folder holding `path` to disk, so `path` created, renamed or
/// removed before the call is still so after a crash or power loss: on Unix, flushing a file
/// does not make its name durable. A bare file name is in the current folder.
#[cfg(unix)]
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: callers run before the server starts, on the blocking pool or on the save thread"
)]
pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
    let dir = match path.parent() {
        Some(dir) if dir.as_os_str().is_empty() => Path::new("."),
        Some(dir) => dir,
        // A root or empty path names no file in a folder.
        None => return Err(io::ErrorKind::InvalidInput.into()),
    };
    std::fs::File::open(dir)?.sync_all()
}

/// Flushes nothing: Windows cannot flush a folder (opening one to flush it is refused), and NTFS
/// journals entry changes, so a crash leaves the old or the new entry.
#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "consistency: one signature on every platform; only the Unix version can fail"
)]
pub(crate) fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
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
    #[cfg(unix)]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn an_owner_only_file_stays_owner_only_over_a_temporary_file_a_crash_left() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("anchor-fs-mode-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).unwrap_or(());
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("identity.key");
        let temp = dir.join("identity.key.tmp");
        std::fs::write(&temp, b"stale").unwrap();
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&path, b"key", Access::OwnerOnly).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
        assert_eq!(std::fs::read(&path).unwrap(), b"key");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn the_folder_of_a_file_flushes() {
        let dir = std::env::temp_dir().join(format!("anchor-fs-sync-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).unwrap_or(());
        std::fs::create_dir(&dir).unwrap();
        sync_parent(&dir.join("file")).unwrap();
        // A bare file name is in the current folder.
        sync_parent(Path::new("file")).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn missing_folders_are_made_and_a_second_call_changes_nothing() {
        let dir = std::env::temp_dir().join(format!("anchor-fs-dirs-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).unwrap_or(());
        let nested = dir.join("a").join("b");
        create_dir_all_durable(&nested).unwrap();
        assert!(nested.is_dir());
        create_dir_all_durable(&nested).unwrap();
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
