//! Copies of a data file: `<unix seconds>[-<n>].redb` in the file's own backup folder
//! ([`Place::backups`](super::Place), such as `data/backups/players/`).
//!
//! A copy ([`Online`]) is taken from one read transaction of the open file, in steps, while the
//! server runs: the start's backup of its first commit, and those the backup command asks for. A
//! backup is never overwritten, and its name sorts after every backup already there: a second
//! backup in the same second adds `-<n>`, and after the clock went back the name keeps the newest
//! second found. So the newest backups are the last names. A copy is written under
//! `<name>.tmp` and renamed when whole; then all but the `max-backups` newest are removed.

use std::io;
use std::iter::Peekable;
use std::path::{Path, PathBuf};

use redb::backends::FileBackend;
use redb::{Builder, Database, Durability, ReadableDatabase, TableDefinition, TableHandle};
use tracing::warn;

use super::{CACHE_BYTES, Error, Result};
use crate::{clock, fs};

/// Names tried when another process takes the chosen one first.
const ATTEMPTS: u32 = 100;
/// Key and value bytes an online backup copies (and a check reads) in one step, unless one value
/// alone is larger: the save thread serves its queued requests between steps, so a load waits at
/// most one step.
pub(super) const STEP_BYTES: usize = 8 << 20;
/// The extension of a backup's name.
const SUFFIX: &str = ".redb";
/// Added to a backup's name while it is being written.
const TEMP_SUFFIX: &str = ".tmp";

/// A backup's temporary file: removed when dropped, unless renamed to its backup name first.
#[derive(Debug)]
struct Temp {
    path: PathBuf,
    kept: bool,
}

impl Temp {
    /// Gives the file its backup name `to` and flushes the folder, so a crash does not take the
    /// name back.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread names a whole backup"
    )]
    fn rename(mut self, to: &Path) -> Result<()> {
        std::fs::rename(&self.path, to).map_err(Error::Io)?;
        self.kept = true;
        fs::sync_parent(to).map_err(Error::Io)
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
            warn!(
                path = %self.path.display(),
                error = &e as &(dyn std::error::Error + 'static),
                "could not remove an incomplete backup"
            );
        }
    }
}

/// An entry of a table of a data file as redb reads it.
type Entry = <redb::Range<'static, &'static [u8], &'static [u8]> as Iterator>::Item;

/// A table of a data file as an online backup copies it (or a check reads it): its name and the
/// entries left.
pub(super) type Source = (
    String,
    Peekable<redb::Range<'static, &'static [u8], &'static [u8]>>,
);

/// An online backup being made: every table of one read transaction of an open data file, copied
/// in steps of at most [`STEP_BYTES`], one commit each, into a new database under `<backup>.tmp`,
/// which is then committed durably once, closed, flushed to disk and renamed, and its folder
/// flushed. The step commits are not flushed: only the whole copy has to reach the disk, and a
/// copy cut short is a `.tmp` file, never a backup. The read transaction is a consistent view of
/// one commit, so commits made between steps are not in the copy. A backup begins only after the
/// main file's commit of the save it copies (or at start, after the roll back to that save), so
/// its rename and the pruning after it come after that commit too. Dropped before it is whole, it
/// removes its temporary file.
pub(super) struct Online {
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
    /// The newest backups of `dir` kept once this one is whole ([`prune`]).
    max_backups: usize,
}

impl Online {
    /// A backup of `source`'s last commit into a new backup in `dir`, which keeps the
    /// `max_backups` newest once it is whole.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread starts an online backup"
    )]
    pub(super) fn begin(source: &Database, dir: &Path, max_backups: usize) -> Result<Self> {
        crate::fs::create_dir_all_durable(dir).map_err(Error::Io)?;
        let (path, temp, file) = new_file(dir)?;
        let mut builder = Builder::new();
        builder.set_cache_size(CACHE_BYTES);
        let backend = target(file).map_err(Error::database)?;
        let db = builder
            .create_with_backend(backend)
            .map_err(Error::from_open)?;
        Ok(Self {
            tables: tables(source)?,
            db,
            temp,
            path,
            dir: dir.to_path_buf(),
            max_backups,
        })
    }

    /// Whether every table is copied: [`finish`](Self::finish) is left.
    pub(super) fn is_copied(&self) -> bool {
        self.tables.is_empty()
    }

    /// Copies the next step ([`add_to_step`] says which entries it holds) and commits it, without
    /// flushing it to disk. Each step is its own commit, so no step writes more than its own
    /// entries, and none waits for the disk to sync.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread copies a step of an online backup"
    )]
    pub(super) fn step(&mut self) -> Result<()> {
        let mut txn = self.db.begin_write().map_err(Error::database)?;
        txn.set_durability(Durability::None)
            .map_err(Error::database)?;
        let mut bytes = 0_usize;
        while let Some((name, entries)) = self.tables.last_mut() {
            // Opening it creates it, so an empty table is in the copy too.
            let mut target = txn
                .open_table(TableDefinition::<&[u8], &[u8]>::new(name))
                .map_err(Error::database)?;
            while let Some(entry) = entries.peek() {
                let Some(taken) = add_to_step(bytes, entry) else {
                    drop(target);
                    txn.commit().map_err(Error::database)?;
                    return Ok(());
                };
                bytes = taken;
                if let Some(entry) = entries.next() {
                    let (key, value) = entry.map_err(Error::database)?;
                    drop(
                        target
                            .insert(key.value(), value.value())
                            .map_err(Error::database)?,
                    );
                }
            }
            drop(target);
            self.tables.pop();
        }
        txn.commit().map_err(Error::database)?;
        Ok(())
    }

    /// Commits the whole copy durably, closes and flushes it, gives it its backup name and removes
    /// old backups ([`prune`]); returns its path. The durable commit is the one that makes the
    /// steps' commits reach the disk; closing the copy alone could not report a failure.
    ///
    /// The copy is not compacted: `compact` is one call that took
    /// 3.7 to 4.1 s on copies of 116 and 456 MiB, which every request would wait for, and saves
    /// 10 to 13% of the file.
    #[expect(
        clippy::disallowed_types,
        clippy::disallowed_methods,
        reason = "off-tick: the save thread finishes an online backup"
    )]
    pub(super) fn finish(self) -> Result<PathBuf> {
        let Self {
            tables: _,
            db,
            temp,
            path,
            dir,
            max_backups,
        } = self;
        db.begin_write()
            .map_err(Error::database)?
            .commit()
            .map_err(Error::database)?;
        // Closed first: Windows cannot rename an open file.
        drop(db);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&temp.path)
            .and_then(|file| file.sync_all())
            .map_err(Error::Io)?;
        temp.rename(&path)?;
        if let Err(e) = prune(&dir, max_backups) {
            warn!(
                path = %dir.display(),
                error = &e as &(dyn std::error::Error + 'static),
                "could not remove old backups"
            );
        }
        Ok(path)
    }
}

impl std::fmt::Debug for Online {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Online")
            .field("tables_left", &self.tables.len())
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Removes the temporary files of backups that a killed process left in `dir`, if it exists. The
/// caller has the data file open, so no other process is writing a backup of it.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread cleans the backup folder once, before backing up"
)]
pub(super) fn remove_stale(dir: &Path) -> Result<()> {
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
            format!("{secs}{SUFFIX}")
        } else {
            format!("{secs}-{n}{SUFFIX}")
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

/// Removes all but the `max_backups` newest backups in `dir`. The backup just made sorts last and
/// `max_backups` is at least 1, so it stays. Other files are left alone.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread prunes backups after an online backup"
)]
fn prune(dir: &Path, max_backups: usize) -> Result<()> {
    let mut backups = backups(dir)?;
    backups.sort_unstable();
    let old = backups.len().saturating_sub(max_backups);
    for (_, path) in backups.iter().take(old) {
        std::fs::remove_file(path).map_err(Error::Io)?;
    }
    Ok(())
}

/// The backups in `dir` with the order of their names; none if `dir` does not exist.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread and a replay list a backup folder"
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
pub(super) fn order(name: &str) -> Option<(u64, u32)> {
    let stem = name.strip_suffix(SUFFIX)?;
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

/// Every table of one read transaction of `source`, the first last: a consistent view of its
/// last commit, which commits made meanwhile do not change.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread reads a data file for a backup or a check"
)]
pub(super) fn tables(source: &Database) -> Result<Vec<Source>> {
    let read = source.begin_read().map_err(Error::database)?;
    let mut tables = Vec::new();
    for table in read.list_tables().map_err(Error::database)? {
        let name = table.name().to_owned();
        let entries = read
            .open_table(TableDefinition::<&[u8], &[u8]>::new(&name))
            .map_err(Error::database)?
            .range::<&[u8]>(..)
            .map_err(Error::database)?;
        tables.push((name, entries.peekable()));
    }
    tables.reverse();
    Ok(tables)
}

/// Adds `entry` to a step that holds `bytes` of keys and values: the step's bytes with it, or
/// `None` when the step ends before it. A step holds entries while it stays within
/// [`STEP_BYTES`]; a larger value is a step alone. An entry that failed to read counts as no bytes.
pub(super) fn add_to_step(bytes: usize, entry: &Entry) -> Option<usize> {
    let size = match entry {
        Ok((key, value)) => key.value().len().saturating_add(value.value().len()),
        Err(_) => 0,
    };
    let total = bytes.saturating_add(size);
    (bytes == 0 || total <= STEP_BYTES).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// The syncs of online backup files this thread made ([`Counted`]).
        static SYNCS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        /// The times this thread made an online backup's file longer ([`Counted`]).
        static GROWTHS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    #[test]
    fn only_backup_names_have_an_order() {
        assert_eq!(order("17.redb"), Some((17, 0)));
        assert_eq!(order("17-3.redb"), Some((17, 3)));
        for other in [
            ".redb",
            "17-0.redb",
            "17-.redb",
            "+1.redb",
            "1-+2.redb",
            "17.redb.tmp",
            "notes.txt",
            "a.redb",
        ] {
            assert_eq!(order(other), None, "{other}");
        }
        assert!(order("9-1.redb") < order("10.redb"));
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn what_a_killed_backup_left_is_removed_and_whole_backups_stay() {
        let dir = std::env::temp_dir().join(format!("anchor-backup-killed-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).unwrap_or(());
        std::fs::create_dir_all(&dir).unwrap();
        // A kill runs no drop, so the temporary file stays.
        std::fs::write(dir.join("1.redb.tmp"), b"partial").unwrap();
        std::fs::write(dir.join("2.redb"), b"whole").unwrap();
        std::fs::write(dir.join("notes.txt"), b"the operator's").unwrap();
        remove_stale(&dir).unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["2.redb", "notes.txt"]);
        // No backup folder at all is no error.
        std::fs::remove_dir_all(&dir).unwrap();
        remove_stale(&dir).unwrap();
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_new_name_sorts_after_backups_from_a_later_clock() {
        let dir = std::env::temp_dir().join(format!("anchor-backup-names-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).unwrap_or(());
        std::fs::create_dir_all(&dir).unwrap();
        // A backup from far in the future: the clock went back since.
        std::fs::write(dir.join("99999999999-4.redb"), b"").unwrap();
        let (path, _, file) = new_file(&dir).unwrap();
        drop(file);
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(name, "99999999999-5.redb");
        std::fs::remove_dir_all(&dir).unwrap();
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
        let value = vec![7_u8; 1 << 20];
        let txn = source.begin_write().unwrap();
        {
            let mut t = txn.open_table(table).unwrap();
            for n in 0_u32..48 {
                t.insert(n.to_be_bytes().as_slice(), value.as_slice())
                    .unwrap();
            }
        }
        txn.commit().unwrap();
        let backups = dir.join("backups");
        let mut online = Online::begin(&source, &backups, 1).unwrap();
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
        // name the larger file), never for its commit, so no step syncs more often than it grows
        // the file. redb grows the file by an eighth, so every step of this small copy grows it.
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
