//! Start-time copies of the world file: `data/backups/world-<unix seconds>[-<n>].redb`.
//!
//! The copy is taken from the closed file, under an exclusive lock that also tells whether
//! another process has the file open. A backup is never overwritten, and its name sorts after
//! every backup already there: a second start in the same second adds `-<n>`, and after the clock
//! went back the name keeps the newest second found. So the newest backups are the last names.

use std::fs::TryLockError;
use std::io;
use std::path::{Path, PathBuf};

use tracing::warn;

use super::{Error, Result};
use crate::clock;

/// Backups kept after a successful start.
pub(crate) const KEPT: usize = 3;
/// Names tried when another process takes the chosen one first.
const ATTEMPTS: u32 = 100;
const PREFIX: &str = "world-";
const SUFFIX: &str = ".redb";

/// Copies `world` into `dir` and returns the copy, or `None` if there is no file or it is empty
/// (a new world). Fails with [`Error::InUse`] if another process has the file open.
#[expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "off-tick: the save thread backs up the world file once, before opening it"
)]
pub(crate) fn create(world: &Path, dir: &Path) -> Result<Option<PathBuf>> {
    let mut source = match std::fs::File::open(world) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    match source.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Err(Error::InUse),
        Err(TryLockError::Error(e)) => return Err(Error::Io(e)),
    }
    if source.metadata().map_err(Error::Io)?.len() == 0 {
        return Ok(None);
    }
    std::fs::create_dir_all(dir).map_err(Error::Io)?;
    let (path, mut target) = new_file(dir)?;
    let copied = io::copy(&mut source, &mut target).and_then(|_| target.sync_all());
    if let Err(e) = copied {
        drop(target);
        if let Err(remove) = std::fs::remove_file(&path) {
            warn!(path = %path.display(), error = %remove, "could not remove an incomplete backup");
        }
        return Err(Error::Io(e));
    }
    Ok(Some(path))
}

/// Creates a backup file whose name sorts after every backup in `dir`.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: the save thread backs up the world file once, before opening it"
)]
fn new_file(dir: &Path) -> Result<(PathBuf, std::fs::File)> {
    // A clock before 1970 counts as second 0.
    let now = clock::unix_now().unwrap_or_default();
    let last = backups(dir)?.into_iter().map(|(order, _)| order).max();
    let (secs, mut n) = match last {
        Some((secs, n)) if secs >= now => (secs, n.saturating_add(1)),
        _ => (now, 0),
    };
    for _ in 0..ATTEMPTS {
        let name = if n == 0 {
            format!("{PREFIX}{secs}{SUFFIX}")
        } else {
            format!("{PREFIX}{secs}-{n}{SUFFIX}")
        };
        let path = dir.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n = n.saturating_add(1),
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Err(Error::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free backup name",
    )))
}

/// Writes the bytes of `backup` back over `world`, then removes `backup`. Used when a start is
/// refused, so the world file is left exactly as it was before the start opened it.
#[expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "off-tick: the save thread restores the world file once, after a refused start"
)]
pub(crate) fn restore(backup: &Path, world: &Path) -> Result<()> {
    let mut source = std::fs::File::open(backup).map_err(Error::Io)?;
    let mut target = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(world)
        .map_err(Error::Io)?;
    io::copy(&mut source, &mut target).map_err(Error::Io)?;
    target.sync_all().map_err(Error::Io)?;
    drop(source);
    std::fs::remove_file(backup).map_err(Error::Io)
}

/// Removes all but the [`KEPT`] newest backups in `dir`, never `newest` (the copy this start
/// made). Other files are left alone.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread prunes backups once, after a successful start"
)]
pub(crate) fn prune(dir: &Path, newest: &Path) -> Result<()> {
    let mut backups = backups(dir)?;
    backups.sort_unstable();
    let old = backups.len().saturating_sub(KEPT);
    for (_, path) in backups.iter().take(old) {
        if path != newest {
            std::fs::remove_file(path).map_err(Error::Io)?;
        }
    }
    Ok(())
}

/// The backups in `dir` with the order of their names; none if `dir` does not exist.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread lists backups at start"
)]
fn backups(dir: &Path) -> Result<Vec<((u64, u32), PathBuf)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(e)),
    };
    let mut backups = Vec::new();
    for entry in entries {
        let entry = entry.map_err(Error::Io)?;
        if let Some(order) = entry.file_name().to_str().and_then(order) {
            backups.push((order, entry.path()));
        }
    }
    Ok(backups)
}

/// The (seconds, n) a backup name sorts by, or `None` for any other name.
pub(crate) fn order(name: &str) -> Option<(u64, u32)> {
    let stem = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    match stem.split_once('-') {
        Some((secs, n)) => Some((number(secs)?, number(n).filter(|&n| n != 0)?)),
        None => Some((number(stem)?, 0)),
    }
}

/// A decimal number of digits only (no sign).
fn number<T: std::str::FromStr>(digits: &str) -> Option<T> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_backup_names_have_an_order() {
        assert_eq!(order("world-17.redb"), Some((17, 0)));
        assert_eq!(order("world-17-3.redb"), Some((17, 3)));
        for other in [
            "world-.redb",
            "world-17-0.redb",
            "world-17-.redb",
            "world-+1.redb",
            "world-1-+2.redb",
            "world-17.redb.tmp",
            "notes.txt",
            "world-a.redb",
        ] {
            assert_eq!(order(other), None, "{other}");
        }
        assert!(order("world-9-1.redb") < order("world-10.redb"));
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_new_name_sorts_after_backups_from_a_later_clock() {
        let dir = std::env::temp_dir().join(format!("anchor-backup-names-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).unwrap_or(());
        std::fs::create_dir_all(&dir).unwrap();
        // A backup from far in the future: the clock went back since.
        std::fs::write(dir.join("world-99999999999-4.redb"), b"").unwrap();
        let (path, file) = new_file(&dir).unwrap();
        drop(file);
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(name, "world-99999999999-5.redb");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
