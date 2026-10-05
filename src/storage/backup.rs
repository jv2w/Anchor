//! Copies of the server file: `data/backups/server-<unix seconds>[-<n>].redb`. Backups an older
//! Anchor named `world-...` count as backups too, until pruned.
//!
//! A start copy is taken from the closed file, under an exclusive lock that also tells whether
//! another process has the file open, when a start backup or the start's integrity check asks
//! for one (`Storage::open`). An online copy ([`Online`], PLAN.md 10.9 백업) is taken
//! while the server runs, from one read transaction of the open file, in steps. A backup is never
//! overwritten, and its name sorts after every backup already there: a second backup in the same
//! second adds `-<n>`, and after the clock went back the name keeps the newest second found. So
//! the newest backups are the last names. A copy is written under `<name>.tmp` and renamed when
//! whole, except a start copy with `backups = false`, which keeps its temporary name and is
//! removed once the file opened (or renamed by [`keep`] if a failed start keeps it); both kinds
//! share the [`KEPT`] newest names.

use std::fs::TryLockError;
use std::io;
use std::iter::Peekable;
use std::path::{Path, PathBuf};

use redb::backends::FileBackend;
use redb::{Builder, Database, Durability, ReadableDatabase, TableDefinition, TableHandle};
use tracing::warn;

use super::{CACHE_BYTES, Error, Result, StartBackup};
use crate::clock;

/// Backups kept, of starts and online backups together.
pub(crate) const KEPT: usize = 3;
/// Key and value bytes an online backup copies (and a check reads) in one step, unless one value
/// alone is larger: the save thread serves its queued requests between steps, so a load waits at
/// most one step.
pub(super) const STEP_BYTES: usize = 8 << 20;
/// Names tried when another process takes the chosen one first.
const ATTEMPTS: u32 = 100;
const PREFIX: &str = "server-";
/// The prefix of the backups of an older Anchor, before the server file was renamed.
const OLD_PREFIX: &str = "world-";
const SUFFIX: &str = ".redb";
/// Added to a backup's name while it is being written.
const TEMP_SUFFIX: &str = ".tmp";

/// Locks the closed server file `world` and removes the temporary files a killed process left in
/// `dir`; returns the file, still locked, or `None` if there is none or it is empty (a new world).
/// The lock tells whether another process has the file open ([`Error::InUse`]); it ends when the
/// file is dropped, which must come before the file is opened as a database.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: the save thread locks the server file once, before opening it"
)]
pub(crate) fn lock(world: &Path, dir: &Path) -> Result<Option<std::fs::File>> {
    let source = match std::fs::File::open(world) {
        Ok(file) => {
            match file.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => return Err(Error::InUse),
                Err(TryLockError::Error(e)) => return Err(Error::Io(e)),
            }
            Some(file)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(Error::Io(e)),
    };
    // No process has the file open, or there is none, so no backup is being written.
    remove_stale(dir)?;
    let Some(source) = source else {
        return Ok(None);
    };
    if source.metadata().map_err(Error::Io)?.len() == 0 {
        return Ok(None);
    }
    Ok(Some(source))
}

/// [`lock`], then [`copy`]: the start copy of `world` the tests take.
#[cfg(test)]
pub(crate) fn create(world: &Path, dir: &Path, start: StartBackup) -> Result<Option<PathBuf>> {
    lock(world, dir)?
        .map(|mut source| copy(&mut source, dir, start))
        .transpose()
}

/// Copies `source`, the server file [`lock`] gave, into a new backup in `dir` and returns its
/// path. The bytes go to `<backup>.tmp`, which is flushed to disk and then renamed, so a process
/// killed mid-copy leaves no file with a backup name (it would count among the [`KEPT`] and push
/// out a whole one); the next start removes the temporary file. With
/// [`StartBackup::Transient`] the copy keeps its temporary name, and its temporary path is
/// returned: a start killed before removing it leaves no backup, and the next start removes it;
/// a start that fails and keeps it gives it its backup name ([`keep`]).
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread backs up the server file once, before opening it"
)]
pub(crate) fn copy(source: &mut impl io::Read, dir: &Path, start: StartBackup) -> Result<PathBuf> {
    #[cfg(test)]
    super::tests::STARTS.set((
        super::tests::STARTS.get().0,
        super::tests::STARTS.get().1 + 1,
    ));
    std::fs::create_dir_all(dir).map_err(Error::Io)?;
    let (path, temp, mut target) = new_file(dir)?;
    let copied = io::copy(source, &mut target).and_then(|_| target.sync_all());
    // Windows cannot rename or remove a file that is still open.
    drop(target);
    copied.map_err(Error::Io)?;
    match start {
        StartBackup::Make => {
            temp.rename(&path)?;
            Ok(path)
        }
        StartBackup::Transient => Ok(temp.keep()),
    }
}

/// Gives `copy`, a [`StartBackup::Transient`] copy that a failed start keeps, its backup name, so
/// the next start does not remove it as an incomplete one; returns the new path.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread names a start copy it keeps, once"
)]
pub(crate) fn keep(copy: &Path) -> Result<PathBuf> {
    // The copy's name is the backup's with `TEMP_SUFFIX`, one extension, after it.
    let path = copy.with_extension("");
    std::fs::rename(copy, &path).map_err(Error::Io)?;
    Ok(path)
}

/// A backup's temporary file: removed when dropped, unless renamed to its backup name or kept
/// first.
#[derive(Debug)]
struct Temp {
    path: PathBuf,
    kept: bool,
}

impl Temp {
    /// Gives the file its backup name `to`.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread names a whole backup"
    )]
    fn rename(mut self, to: &Path) -> Result<()> {
        std::fs::rename(&self.path, to).map_err(Error::Io)?;
        self.kept = true;
        Ok(())
    }

    /// Keeps the file under its temporary name and returns its path.
    fn keep(mut self) -> PathBuf {
        self.kept = true;
        std::mem::take(&mut self.path)
    }
}

impl Drop for Temp {
    /// Removes the incomplete backup; the file must be closed (Windows cannot remove an open
    /// file).
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread removes an incomplete backup"
    )]
    fn drop(&mut self) {
        if self.kept {
            return;
        }
        if let Err(e) = std::fs::remove_file(&self.path) {
            warn!(path = %self.path.display(), error = %e, "could not remove an incomplete backup");
        }
    }
}

/// Removes the temporary files of backups that a killed process left in `dir`, if it exists.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread cleans the backup folder once, before backing up"
)]
fn remove_stale(dir: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };
    for entry in entries {
        let entry = entry.map_err(Error::Io)?;
        let name = entry.file_name();
        let stale = name
            .to_str()
            .and_then(|name| name.strip_suffix(TEMP_SUFFIX))
            .and_then(order)
            .is_some();
        if stale {
            warn!(path = %entry.path().display(), "removing an incomplete backup");
            std::fs::remove_file(entry.path()).map_err(Error::Io)?;
        }
    }
    Ok(())
}

/// Creates the temporary file of a backup whose name sorts after every backup in `dir`; returns
/// the backup's path, the temporary file and the file open for reading and writing.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: the save thread creates a backup file"
)]
fn new_file(dir: &Path) -> Result<(PathBuf, Temp, std::fs::File)> {
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
        let temp = dir.join(format!("{name}{TEMP_SUFFIX}"));
        // Read too: an online backup is a database redb reads back as it writes.
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(file) => {
                let temp = Temp {
                    path: temp,
                    kept: false,
                };
                return Ok((dir.join(name), temp, file));
            }
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
/// refused, so the server file is left exactly as it was before the start opened it.
#[expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "off-tick: the save thread restores the server file once, after a refused start"
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

/// Removes all but the [`KEPT`] newest backups in `dir`, never `newest` (the backup just made).
/// Other files are left alone.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread prunes backups at start and after an online backup"
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
pub(super) fn backups(dir: &Path) -> Result<Vec<((u64, u32), PathBuf)>> {
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
    let stem = name
        .strip_prefix(PREFIX)
        .or_else(|| name.strip_prefix(OLD_PREFIX))?
        .strip_suffix(SUFFIX)?;
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

/// The backend of an online backup's file.
#[cfg(not(test))]
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: the save thread opens an online backup's file"
)]
fn target(file: std::fs::File) -> Result<FileBackend, redb::DatabaseError> {
    FileBackend::new(file)
}

/// The backend of an online backup's file, counting its syncs ([`tests::Counted`]).
#[cfg(test)]
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: the save thread opens an online backup's file"
)]
fn target(file: std::fs::File) -> Result<tests::Counted, redb::DatabaseError> {
    FileBackend::new(file).map(tests::Counted)
}

/// A table of the server file as an online backup copies it (or a check reads it): its name and
/// the entries left.
pub(super) type Source = (
    String,
    Peekable<redb::Range<'static, &'static [u8], &'static [u8]>>,
);

/// Every table of one read transaction of `source`, the first last: a consistent view of its
/// last commit, which commits made meanwhile do not change.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread reads the server file for a backup or a check"
)]
pub(super) fn tables(source: &Database) -> Result<Vec<Source>> {
    let read = source.begin_read()?;
    let mut tables = Vec::new();
    for table in read.list_tables()? {
        let name = table.name().to_owned();
        let entries = read
            .open_table(TableDefinition::<&[u8], &[u8]>::new(&name))?
            .range::<&[u8]>(..)?;
        tables.push((name, entries.peekable()));
    }
    tables.reverse();
    Ok(tables)
}

/// An online backup being made (PLAN.md 10.9 백업): every table of one read transaction of the
/// open server file, copied in steps of at most [`STEP_BYTES`], one commit each, into a new
/// database under `<backup>.tmp`, which is then committed durably once, closed, flushed to disk
/// and renamed. The step commits are not flushed: only the whole copy has to reach the disk, and
/// a copy cut short is a `.tmp` file, never a backup. The read transaction is a consistent view
/// of one commit, so commits made between steps are not in the copy. Dropped before it is whole,
/// it removes its temporary file.
pub(crate) struct Online {
    // Dropped in this order: the source's read transaction, the copy, then its file, which
    // Windows cannot remove while it is open.
    /// The tables left to copy, the next last.
    tables: Vec<Source>,
    /// The copy.
    db: Database,
    temp: Temp,
    /// The backup's name.
    path: PathBuf,
    dir: PathBuf,
}

impl std::fmt::Debug for Online {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Online")
            .field("path", &self.path)
            .field("tables_left", &self.tables.len())
            .finish_non_exhaustive()
    }
}

impl Online {
    /// Starts a backup of `source`'s last commit into a new backup in `dir`.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread starts an online backup"
    )]
    pub(crate) fn begin(source: &Database, dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(Error::Io)?;
        let (path, temp, file) = new_file(dir)?;
        let mut builder = Builder::new();
        builder.set_cache_size(CACHE_BYTES);
        let backend = target(file).map_err(|e| Error::Database(e.into()))?;
        let db = builder
            .create_with_backend(backend)
            .map_err(Error::from_open)?;
        Ok(Self {
            tables: tables(source)?,
            db,
            temp,
            path,
            dir: dir.to_path_buf(),
        })
    }

    /// Whether every table is copied: [`finish`](Self::finish) is left.
    pub(crate) fn is_copied(&self) -> bool {
        self.tables.is_empty()
    }

    /// Copies the next step and commits it, without flushing it to disk: entries while the step
    /// stays within [`STEP_BYTES`] (a larger value is a step alone). Each step is its own commit,
    /// so no step writes more than its own entries, and none waits for the disk to sync.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread copies a step of an online backup"
    )]
    pub(crate) fn step(&mut self) -> Result<()> {
        let mut txn = self.db.begin_write()?;
        txn.set_durability(Durability::None)
            .map_err(redb::Error::from)?;
        let mut bytes = 0usize;
        while let Some((name, entries)) = self.tables.last_mut() {
            // Opening it creates it, so an empty table is in the copy too.
            let mut target = txn.open_table(TableDefinition::<&[u8], &[u8]>::new(name))?;
            while let Some(entry) = entries.peek() {
                let size = match entry {
                    Ok((key, value)) => key.value().len().saturating_add(value.value().len()),
                    Err(_) => 0,
                };
                if bytes > 0 && bytes.saturating_add(size) > STEP_BYTES {
                    drop(target);
                    txn.commit()?;
                    return Ok(());
                }
                if let Some(entry) = entries.next() {
                    let (key, value) = entry?;
                    drop(target.insert(key.value(), value.value())?);
                }
                bytes = bytes.saturating_add(size);
            }
            drop(target);
            self.tables.pop();
        }
        txn.commit()?;
        Ok(())
    }

    /// Commits the whole copy durably, closes and flushes it, gives it its backup name and keeps
    /// the [`KEPT`] newest backups; returns its path. The durable commit is the one that makes
    /// the steps' commits reach the disk; closing the copy alone could not report a failure.
    ///
    /// The copy is not compacted (PLAN.md 10.9 저장 시작 비용): `compact` is one call that took
    /// 3.7 to 4.1 s on copies of 116 and 456 MiB, which every request would wait for, and saves
    /// 10 to 13% of the file. A backup put back as the server file is compacted by that start.
    #[expect(
        clippy::disallowed_types,
        clippy::disallowed_methods,
        reason = "off-tick: the save thread finishes an online backup"
    )]
    pub(crate) fn finish(self) -> Result<PathBuf> {
        let Self {
            tables: _,
            db,
            temp,
            path,
            dir,
        } = self;
        db.begin_write()?.commit()?;
        // Closed first: Windows cannot rename an open file.
        drop(db);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&temp.path)
            .and_then(|file| file.sync_all())
            .map_err(Error::Io)?;
        temp.rename(&path)?;
        if let Err(e) = prune(&dir, &path) {
            warn!(error = %e, "could not remove old backups");
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_backup_names_have_an_order() {
        assert_eq!(order("server-17.redb"), Some((17, 0)));
        assert_eq!(order("server-17-3.redb"), Some((17, 3)));
        // An older Anchor's backups.
        assert_eq!(order("world-17.redb"), Some((17, 0)));
        assert_eq!(order("world-17-3.redb"), Some((17, 3)));
        for other in [
            "server-.redb",
            "server-17-0.redb",
            "server-17-.redb",
            "server-+1.redb",
            "server-1-+2.redb",
            "server-17.redb.tmp",
            "world-17.redb.tmp",
            "notes.txt",
            "server-a.redb",
            "worlds-17.redb",
        ] {
            assert_eq!(order(other), None, "{other}");
        }
        assert!(order("server-9-1.redb") < order("server-10.redb"));
        assert!(order("world-9-1.redb") < order("server-10.redb"));
    }

    /// A copy source whose second read never returns: the process is killed mid-copy.
    struct Killed(bool);

    impl io::Read for Killed {
        #[expect(
            clippy::panic_in_result_fn,
            reason = "consistency: the panic stands in for a kill, which runs no clean-up"
        )]
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            assert!(!self.0, "killed during the copy");
            self.0 = true;
            buf.fill(7);
            Ok(buf.len())
        }
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn an_interrupted_backup_is_not_kept_as_a_backup() {
        let dir = std::env::temp_dir().join(format!("anchor-backup-killed-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).unwrap_or(());
        std::fs::create_dir_all(&dir).unwrap();
        // A kill runs no clean-up: the unwinding panic stands in for it.
        let killed = std::panic::catch_unwind(|| copy(&mut Killed(false), &dir, StartBackup::Make));
        assert!(killed.is_err());
        assert!(
            backups(&dir).unwrap().is_empty(),
            "the incomplete copy has a backup name"
        );
        // The unwinding removed the temporary file; a kill runs no drop and leaves it.
        std::fs::write(dir.join("server-1.redb.tmp"), b"partial").unwrap();
        // An older Anchor's killed copy too.
        std::fs::write(dir.join("world-1.redb.tmp"), b"partial").unwrap();
        // The next start removes what the killed copy left and makes a whole backup.
        let world = dir.join("server.redb");
        std::fs::write(&world, b"world bytes").unwrap();
        let made = create(&world, &dir, StartBackup::Make).unwrap().unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        let made_name = made.file_name().unwrap().to_str().unwrap();
        assert_eq!(names, [made_name, "server.redb"]);
        assert_eq!(std::fs::read(&made).unwrap(), b"world bytes");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_start_without_a_server_file_removes_what_a_killed_copy_left() {
        let dir = std::env::temp_dir().join(format!("anchor-backup-gone-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).unwrap_or(());
        let backups = dir.join("backups");
        std::fs::create_dir_all(&backups).unwrap();
        std::fs::write(backups.join("server-1.redb.tmp"), b"partial").unwrap();
        std::fs::write(backups.join("server-2.redb"), b"whole").unwrap();
        let world = dir.join("server.redb");
        // No server file, then an empty one: neither is copied, and both clean the folder.
        assert_eq!(create(&world, &backups, StartBackup::Make).unwrap(), None);
        std::fs::write(backups.join("server-1.redb.tmp"), b"partial").unwrap();
        std::fs::write(&world, b"").unwrap();
        assert_eq!(create(&world, &backups, StartBackup::Make).unwrap(), None);
        let names: Vec<_> = std::fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["server-2.redb"]);
        // No backup folder at all is no error.
        std::fs::remove_dir_all(&backups).unwrap();
        assert_eq!(create(&world, &backups, StartBackup::Make).unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_new_name_sorts_after_backups_from_a_later_clock() {
        let dir = std::env::temp_dir().join(format!("anchor-backup-names-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).unwrap_or(());
        std::fs::create_dir_all(&dir).unwrap();
        // A backup from far in the future, by an older Anchor: the clock went back since.
        std::fs::write(dir.join("world-99999999999-4.redb"), b"").unwrap();
        let (path, _, file) = new_file(&dir).unwrap();
        drop(file);
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(name, "server-99999999999-5.redb");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    thread_local! {
        /// The syncs of online backup files this thread made ([`Counted`]).
        static SYNCS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        /// The times this thread made an online backup's file longer ([`Counted`]).
        static GROWTHS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    /// An online backup's file backend that counts its syncs and growths on this thread.
    #[derive(Debug)]
    pub(super) struct Counted(pub(super) FileBackend);

    impl redb::StorageBackend for Counted {
        fn len(&self) -> io::Result<u64> {
            self.0.len()
        }

        fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
            self.0.read(offset, out)
        }

        fn set_len(&self, len: u64) -> io::Result<()> {
            if len > self.0.len()? {
                GROWTHS.set(GROWTHS.get() + 1);
            }
            self.0.set_len(len)
        }

        fn sync_data(&self) -> io::Result<()> {
            SYNCS.set(SYNCS.get() + 1);
            self.0.sync_data()
        }

        fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
            self.0.write(offset, data)
        }

        fn close(&self) -> io::Result<()> {
            self.0.close()
        }
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: test fixtures on disk and the backup's steps"
    )]
    fn an_online_backup_step_syncs_only_to_grow_its_file() {
        let dir = std::env::temp_dir().join(format!("anchor-backup-syncs-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).unwrap_or(());
        std::fs::create_dir_all(&dir).unwrap();
        let source = Builder::new().create(dir.join("server.redb")).unwrap();
        let table = TableDefinition::<&[u8], &[u8]>::new("chunks");
        let value = vec![7u8; 1 << 20];
        let txn = source.begin_write().unwrap();
        {
            let mut t = txn.open_table(table).unwrap();
            for n in 0u32..48 {
                t.insert(n.to_be_bytes().as_slice(), value.as_slice())
                    .unwrap();
            }
        }
        txn.commit().unwrap();
        let backups = dir.join("backups");
        let mut online = Online::begin(&source, &backups).unwrap();
        // (syncs, file growths) of each step.
        let mut steps = Vec::new();
        while !online.is_copied() {
            let before = (SYNCS.get(), GROWTHS.get());
            online.step().unwrap();
            steps.push((SYNCS.get() - before.0, GROWTHS.get() - before.1));
        }
        let before = SYNCS.get();
        let made = online.finish().unwrap();
        let finish = SYNCS.get() - before;
        // A step syncs only to make a growth of the file durable (redb does, before a header can
        // name the larger file), never for its commit: before, every step synced once more than
        // it grew the file. The file grows by an eighth (PLAN.md 9.1 redb ③), so every step of
        // this small copy grows it.
        assert!(steps.len() >= 6, "{steps:?}");
        assert!(
            steps.iter().all(|&(syncs, grown)| syncs <= grown),
            "{steps:?}"
        );
        // The durable commit and the close: as many for any size of copy.
        assert!((1..=6).contains(&finish), "{finish} syncs to finish");
        // The copy is whole.
        drop(source);
        let copy = Builder::new().open(&made).unwrap();
        let read = copy.begin_read().unwrap();
        let t = read.open_table(table).unwrap();
        let mut n = 0;
        for entry in redb::ReadableTable::iter(&t).unwrap() {
            assert_eq!(entry.unwrap().1.value(), value.as_slice());
            n += 1;
        }
        assert_eq!(n, 48);
        drop(t);
        drop(read);
        drop(copy);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
