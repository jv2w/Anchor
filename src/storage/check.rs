//! The checks of a data file: redb's integrity check of the closed file at start ([`verify`]),
//! which never writes the file, and the check of the open file while the server runs (`/check`,
//! [`Check`]).
//!
//! redb's integrity check needs the file closed, and it writes to it: the open writes the header,
//! a repair commits, and the close commits the allocator state. So [`verify`] opens the file on a
//! backend ([`Overlay`], [`open_overlay`]) that reads the file and keeps every write in memory,
//! and then drops it: the file keeps its bytes whatever the check finds. A start's roll back of a
//! file (`settle`) is tried the same way before it is made on the file.
//!
//! The check, the roll back and the run open one file handle ([`Handle`]) in turn, and redb's lock
//! stays with the handle from the check's open until the handle is dropped: so the file the run
//! opens is the file checked, and no other process opens it in between.
//!
//! `/check` reads every value of one read transaction of the open file, in steps of at most
//! [`STEP_BYTES`] between the save thread's other work, and checks it against its seal (header
//! and checksum, [`seal::verify`]). It reads every page that holds a value and finds what redb
//! reports damaged on the way.

use std::collections::BTreeMap;
use std::io;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use redb::backends::FileBackend;
use redb::{BackendError, Builder, Database, StorageBackend};
use tracing::{debug, warn};

use super::backup::{self, STEP_BYTES, Source};
use super::{Error, Result, check_format, check_integrity, has_closed_mark, seal};
use crate::config::IntegrityCheck;

/// The unit [`Overlay`] keeps writes in: redb's page.
const BLOCK_BYTES: u64 = 4096;
/// Most bytes of writes [`Overlay`] keeps for one check. A check writes redb's header and, as
/// redb closes it, its allocator state, which grows with the file: 24 KiB for a 96 KiB file and
/// 52 KiB for a 368 MiB world, clean or left by a crash. A repair (`Ok(false)`) commits only the
/// header. So 16 MiB holds the writes of files hundreds of times larger than that world.
const MAX_WRITTEN_BYTES: usize = 16 << 20;
/// Read-ahead blocks [`Overlay`] keeps, the least recently used replaced first: 4 MiB.
const READ_AHEAD_BLOCKS: usize = 16;
/// Bytes of the file one read of [`Overlay`] takes at once.
const READ_AHEAD_BYTES: u64 = 256 << 10;

/// A redb backend over a closed data file that never writes to it: writes and length changes stay
/// in memory, at most [`MAX_WRITTEN_BYTES`] (a write past that fails), and reads see them over the
/// file's bytes. Syncs do nothing. Locks are its [`Handle`]'s.
///
/// Reads of the file take [`READ_AHEAD_BYTES`] at once and keep the last [`READ_AHEAD_BLOCKS`]:
/// redb's check reads page by page, mostly in file order. On a 368 MiB world this read the file
/// in 3,241 calls instead of 185,770 and took 350 to 481 ms instead of 631 to 878 ms; 64 KiB to
/// 1 MiB blocks, or fewer of them, read more and were slower.
struct Overlay {
    file: Arc<Handle>,
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: redb's backend trait takes &self; one thread checks a file"
    )]
    state: std::sync::Mutex<State>,
}

impl Overlay {
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: redb's backend trait takes &self; one thread checks a file"
    )]
    fn new(file: Arc<Handle>) -> Result<Self> {
        let len = file.len()?;
        Ok(Self {
            file,
            state: std::sync::Mutex::new(State {
                len,
                file_end: len,
                written: BTreeMap::new(),
                ahead: Vec::with_capacity(READ_AHEAD_BLOCKS),
            }),
        })
    }

    /// The state. Nothing panics while holding it, so a poisoned lock still holds a whole state.
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl std::fmt::Debug for Overlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Overlay")
            .field("file", &self.file)
            .finish_non_exhaustive()
    }
}

impl StorageBackend for Overlay {
    fn len(&self) -> io::Result<u64> {
        Ok(self.state().len)
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.state().read(&self.file.file, offset, out)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.state().set_len(len);
        Ok(())
    }

    fn sync_data(&self) -> io::Result<()> {
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.state().write(&self.file.file, offset, data)
    }

    fn close(&self) -> io::Result<()> {
        // The handle keeps the lock for the open after the check.
        Ok(())
    }

    fn try_lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<bool, BackendError> {
        self.file.try_lock((start, end), Share::Exclusive)
    }

    fn try_lock_shared_range(
        &self,
        start: Bound<u64>,
        end: Bound<u64>,
    ) -> Result<bool, BackendError> {
        self.file.try_lock((start, end), Share::Shared)
    }

    fn lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<(), BackendError> {
        self.file.lock((start, end), Share::Exclusive)
    }

    fn lock_shared_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<(), BackendError> {
        self.file.lock((start, end), Share::Shared)
    }

    fn unlock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<(), BackendError> {
        self.file.unlock((start, end))
    }

    fn query_lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<bool, BackendError> {
        self.file.file.query_lock_range(start, end)
    }
}

#[derive(Debug)]
struct State {
    /// The length redb sees.
    len: u64,
    /// The end of the file's bytes: from here on, what was not written reads as zeros. The file's
    /// length, or less once redb cut the length below it.
    file_end: u64,
    /// The written blocks of [`BLOCK_BYTES`] bytes, by their index in the file.
    written: BTreeMap<u64, Box<[u8]>>,
    /// Read-ahead blocks of the file: their offset and bytes, the most recently used last.
    ahead: Vec<(u64, Box<[u8]>)>,
}

impl State {
    /// Sets the length: a cut drops what was written past it, and the bytes a later growth adds
    /// read as zeros.
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: `len % BLOCK_BYTES` is within a block"
    )]
    fn set_len(&mut self, len: u64) {
        if len < self.len {
            drop(self.written.split_off(&len.div_ceil(BLOCK_BYTES)));
            if let Some(bytes) = self.written.get_mut(&(len / BLOCK_BYTES)) {
                bytes[index(len % BLOCK_BYTES)..].fill(0);
            }
            self.file_end = self.file_end.min(len);
        }
        self.len = len;
    }

    /// Writes `data` at `offset` into the written blocks.
    #[expect(
        clippy::indexing_slicing,
        clippy::expect_used,
        reason = "invariant: each block written to is kept, and the copied ranges lie within `data` \
                  and the block"
    )]
    fn write(&mut self, file: &FileBackend, offset: u64, data: &[u8]) -> io::Result<()> {
        let end = end_of(offset, data.len())?;
        if end > self.len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let mut at = offset;
        while at < end {
            let block = at / BLOCK_BYTES;
            let start = block * BLOCK_BYTES;
            if !self.written.contains_key(&block) {
                if self.written.len() >= MAX_WRITTEN_BYTES / index(BLOCK_BYTES) {
                    return Err(io::Error::other(
                        "the start check's writes exceed what it keeps in memory",
                    ));
                }
                let mut bytes = vec![0; index(BLOCK_BYTES)].into_boxed_slice();
                let whole = index((start + BLOCK_BYTES).min(self.len) - start);
                self.read(file, start, &mut bytes[..whole])?;
                self.written.insert(block, bytes);
                #[cfg(test)]
                tests::WRITTEN_PEAK.set(
                    tests::WRITTEN_PEAK
                        .get()
                        .max(self.written.len() * index(BLOCK_BYTES)),
                );
            }
            let to = (start + BLOCK_BYTES).min(end);
            let bytes = self
                .written
                .get_mut(&block)
                .expect("the block is written above");
            bytes[index(at - start)..index(to - start)]
                .copy_from_slice(&data[index(at - offset)..index(to - offset)]);
            at = to;
        }
        Ok(())
    }

    /// Reads `out` at `offset` as redb wrote it: written blocks over the file's bytes.
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: the copied ranges lie within `out` and the written block"
    )]
    fn read(&mut self, file: &FileBackend, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let end = end_of(offset, out.len())?;
        if end > self.len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if out.is_empty() {
            return Ok(());
        }
        let mut at = offset;
        let mut next = offset / BLOCK_BYTES;
        let last = (end - 1) / BLOCK_BYTES;
        while next <= last {
            let Some(block) = self.written.range(next..=last).next().map(|(&b, _)| b) else {
                break;
            };
            let start = block * BLOCK_BYTES;
            let from = start.max(offset);
            let to = (start + BLOCK_BYTES).min(end);
            if from > at {
                let gap = &mut out[index(at - offset)..index(from - offset)];
                self.read_file(file, at, gap)?;
            }
            let bytes = &self.written[&block][index(from - start)..index(to - start)];
            out[index(from - offset)..index(to - offset)].copy_from_slice(bytes);
            at = to;
            next = block + 1;
        }
        if at < end {
            self.read_file(file, at, &mut out[index(at - offset)..])?;
        }
        Ok(())
    }

    /// Reads `out` at `offset` from the file's bytes before [`file_end`](Self::file_end) and zeros
    /// after it, through the read-ahead blocks.
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: the copied ranges lie within `bytes` and the block, which covers `at`"
    )]
    fn read_file(&mut self, file: &FileBackend, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let end = end_of(offset, out.len())?;
        let in_file = index(end.min(self.file_end).saturating_sub(offset));
        let (bytes, zeros) = out.split_at_mut(in_file);
        zeros.fill(0);
        if bytes.len() >= index(READ_AHEAD_BYTES) {
            return file.read(offset, bytes);
        }
        let mut at = offset;
        let mut done = 0;
        while done < bytes.len() {
            let start = at - at % READ_AHEAD_BYTES;
            if let Some(i) = self.ahead.iter().position(|(s, _)| *s == start) {
                let used = self.ahead.remove(i);
                self.ahead.push(used);
            } else {
                let len = (start + READ_AHEAD_BYTES).min(self.file_end) - start;
                let mut block = vec![0; index(len)].into_boxed_slice();
                file.read(start, &mut block)?;
                if self.ahead.len() == READ_AHEAD_BLOCKS {
                    self.ahead.remove(0);
                }
                self.ahead.push((start, block));
            }
            let block = &self.ahead[self.ahead.len() - 1].1;
            let from = index(at - start);
            let n = (block.len() - from).min(bytes.len() - done);
            bytes[done..done + n].copy_from_slice(&block[from..from + n]);
            done += n;
            at += n as u64;
        }
        Ok(())
    }
}

/// A data file's handle, from its start check ([`verify`]) until the file closes. redb's lock
/// stays with it: the first redb open on it takes the lock through it, every later one (a start's
/// roll back, the run's open, a reopen after a failed call) takes the ranges it holds as its own,
/// and no redb close releases them. Dropping it releases the lock and closes the file.
#[derive(Debug)]
pub(crate) struct Handle {
    file: FileBackend,
    /// The ranges of redb's lock it holds.
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: redb's backend trait takes &self; one thread opens a file at a time"
    )]
    held: std::sync::Mutex<Vec<(Bound<u64>, Bound<u64>)>>,
}

impl Handle {
    /// The handle of a new data file at `path`, which its check found missing: [`Error::InUse`]
    /// if another process made the file since.
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: the save thread makes a data file"
    )]
    pub(crate) fn create(path: &Path) -> Result<Arc<Self>> {
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path);
        match opened {
            Ok(file) => Self::new(file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(Error::InUse),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// The handle of the data file at `path`; `None` when there is none.
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: the save thread opens a data file"
    )]
    pub(crate) fn open(path: &Path) -> Result<Option<Arc<Self>>> {
        // Opened for writing too: on Unix redb's lock is a write lock, which a read-only handle
        // cannot take.
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path);
        match opened {
            Ok(file) => Self::new(file).map(Some),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// The file's length in bytes.
    pub(crate) fn len(&self) -> Result<u64> {
        self.file.len().map_err(Error::Io)
    }

    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: redb's backend trait takes &self; one thread opens a file at a time"
    )]
    fn new(file: std::fs::File) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            file: FileBackend::new(file).map_err(Error::from_open)?,
            held: std::sync::Mutex::new(Vec::new()),
        }))
    }

    /// The ranges held. Nothing panics while holding them, so a poisoned lock still holds a whole
    /// list.
    fn held(&self) -> std::sync::MutexGuard<'_, Vec<(Bound<u64>, Bound<u64>)>> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Takes `range` of redb's lock, waiting for it, unless the handle holds it already.
    fn lock(&self, range: (Bound<u64>, Bound<u64>), share: Share) -> Result<(), BackendError> {
        let mut held = self.held();
        if held.contains(&range) {
            return Ok(());
        }
        match share {
            Share::Exclusive => self.file.lock_range(range.0, range.1)?,
            Share::Shared => self.file.lock_shared_range(range.0, range.1)?,
        }
        held.push(range);
        Ok(())
    }

    /// Takes `range` of redb's lock, unless the handle holds it already.
    fn try_lock(
        &self,
        range: (Bound<u64>, Bound<u64>),
        share: Share,
    ) -> Result<bool, BackendError> {
        let mut held = self.held();
        if held.contains(&range) {
            return Ok(true);
        }
        let taken = match share {
            Share::Exclusive => self.file.try_lock_range(range.0, range.1)?,
            Share::Shared => self.file.try_lock_shared_range(range.0, range.1)?,
        };
        if taken {
            held.push(range);
        }
        Ok(taken)
    }

    /// Releases `range` of redb's lock.
    fn unlock(&self, range: (Bound<u64>, Bound<u64>)) -> Result<(), BackendError> {
        let mut held = self.held();
        self.file.unlock_range(range.0, range.1)?;
        held.retain(|r| *r != range);
        Ok(())
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // Closing the file releases the lock as well, but Windows may do that some time later,
        // so a start right after this one could find the file in use.
        if let Err(e) = self.file.close() {
            warn!(
                error = &e as &(dyn std::error::Error + 'static),
                "a data file's lock is released only as the file closes"
            );
        }
    }
}

/// How a range of redb's lock is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Share {
    Exclusive,
    Shared,
}

/// The run's redb backend: the file of a [`Handle`], read and written as it is. Locks are the
/// handle's, and a close leaves them to it.
#[derive(Debug)]
pub(super) struct Through(pub(super) Arc<Handle>);

impl StorageBackend for Through {
    fn len(&self) -> io::Result<u64> {
        self.0.file.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.0.file.read(offset, out)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.0.file.set_len(len)
    }

    fn sync_data(&self) -> io::Result<()> {
        self.0.file.sync_data()
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.0.file.write(offset, data)
    }

    fn close(&self) -> io::Result<()> {
        // A reopen after a failed call takes the lock on as it is.
        Ok(())
    }

    fn try_lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<bool, BackendError> {
        self.0.try_lock((start, end), Share::Exclusive)
    }

    fn try_lock_shared_range(
        &self,
        start: Bound<u64>,
        end: Bound<u64>,
    ) -> Result<bool, BackendError> {
        self.0.try_lock((start, end), Share::Shared)
    }

    fn lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<(), BackendError> {
        self.0.lock((start, end), Share::Exclusive)
    }

    fn lock_shared_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<(), BackendError> {
        self.0.lock((start, end), Share::Shared)
    }

    fn unlock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<(), BackendError> {
        self.0.unlock((start, end))
    }

    fn query_lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<bool, BackendError> {
        self.0.file.query_lock_range(start, end)
    }
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Checked {
    /// Every value read back whole: `values` of them.
    Passed { values: u64 },
    /// `damaged` of the `values` read are damaged; `first` tells the first one's table and fault.
    Damaged {
        values: u64,
        damaged: u64,
        first: String,
    },
    /// The check could not finish: why.
    Failed(String),
}

impl Checked {
    /// What a check of several files found: this, then `next`, the result of the file `file`.
    /// The first failure or damage is the one told.
    pub(crate) fn and(self, next: Self, file: &Path) -> Self {
        let (values, damaged, first) = match self {
            Self::Failed(_) => return self,
            Self::Passed { values } => (values, 0, None),
            Self::Damaged {
                values,
                damaged,
                first,
            } => (values, damaged, Some(first)),
        };
        let (more, more_damaged, next_first) = match next {
            Self::Failed(why) => return Self::Failed(format!("{}: {why}", file.display())),
            Self::Passed { values } => (values, 0, None),
            Self::Damaged {
                values,
                damaged,
                first,
            } => (
                values,
                damaged,
                Some(format!("{}: {first}", file.display())),
            ),
        };
        let values = values.saturating_add(more);
        match first.or(next_first) {
            Some(first) => Self::Damaged {
                values,
                damaged: damaged.saturating_add(more_damaged),
                first,
            },
            None => Self::Passed { values },
        }
    }
}

/// A check being made: the tables left of one read transaction, and what it found so far.
pub(crate) struct Check {
    /// The tables left to read, the next last.
    tables: Vec<Source>,
    values: u64,
    damaged: u64,
    /// The first damage's table and fault.
    first: Option<String>,
    /// Why the check ended early.
    failure: Option<String>,
}

impl Check {
    /// Starts a check of `source`'s last commit.
    pub(crate) fn begin(source: &Database) -> Result<Self> {
        Ok(Self {
            tables: backup::tables(source)?,
            values: 0,
            damaged: 0,
            first: None,
            failure: None,
        })
    }

    /// Reads and checks the next step: values while the step stays within [`STEP_BYTES`] (a
    /// larger value is a step alone). Returns what the check found once every table is read.
    /// Damage redb reports while reading a table ends that table, which is counted as one damaged
    /// value; any other read failure ends the check.
    pub(crate) fn step(&mut self) -> Option<Checked> {
        let mut bytes = 0_usize;
        while let Some((name, entries)) = self.tables.last_mut() {
            while let Some(entry) = entries.peek() {
                let size = match entry {
                    Ok((key, value)) => key.value().len().saturating_add(value.value().len()),
                    Err(_) => 0,
                };
                if bytes > 0 && bytes.saturating_add(size) > STEP_BYTES {
                    return None;
                }
                bytes = bytes.saturating_add(size);
                let (fault, table_ends) = match entries.next() {
                    Some(Ok((key, value))) => {
                        self.values = self.values.saturating_add(1);
                        (seal::verify(key.value(), value.value()).err(), false)
                    }
                    Some(Err(e)) => {
                        let e = Error::database(e);
                        if !e.is_damage() {
                            self.failure = Some(format!("table {name}: {e}"));
                            self.tables.clear();
                            return Some(self.end());
                        }
                        // The table cannot be read on past it.
                        (Some(e), true)
                    }
                    None => (None, true),
                };
                if let Some(fault) = fault {
                    self.damaged = self.damaged.saturating_add(1);
                    self.first
                        .get_or_insert_with(|| format!("table {name}: {fault}"));
                }
                if table_ends {
                    break;
                }
            }
            self.tables.pop();
        }
        Some(self.end())
    }

    /// Ends the check early because of `why`, letting go of its read transaction; the next
    /// [`step`](Self::step) reports it.
    pub(crate) fn stop(&mut self, why: String) {
        self.tables.clear();
        self.failure.get_or_insert(why);
    }

    /// What the check found, once it ended.
    fn end(&mut self) -> Checked {
        if let Some(why) = self.failure.take() {
            Checked::Failed(why)
        } else if let Some(first) = self.first.take() {
            Checked::Damaged {
                values: self.values,
                damaged: self.damaged,
                first,
            }
        } else {
            Checked::Passed {
                values: self.values,
            }
        }
    }
}

impl std::fmt::Debug for Check {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Check")
            .field("tables_left", &self.tables.len())
            .field("values", &self.values)
            .field("damaged", &self.damaged)
            .finish_non_exhaustive()
    }
}

/// Checks the closed data file `world` as a start with `integrity-check = check` does, without
/// writing to it: redb's recovery of a crash, redb's integrity check if `check` asks for it (a
/// repair, `Ok(false)`, is damage), and on every start the format of its last commit
/// ([`check_format`]). With `after-unclean-shutdown` the file is checked unless its last commit
/// holds the mark of a normal stop, read in the same open. Returns the file's handle, which keeps
/// redb's lock ([`Error::InUse`] for another process) until the run opens the file on it
/// ([`Storage::open_checked`](super::Storage::open_checked)); `None` for a missing file, which
/// holds nothing to check.
pub(crate) fn verify(world: &Path, check: IntegrityCheck) -> Result<Option<Arc<Handle>>> {
    let Some(handle) = Handle::open(world)? else {
        return Ok(None);
    };
    let mut db = open_overlay(&handle)?;
    let checked = match check {
        IntegrityCheck::Always => true,
        IntegrityCheck::AfterUncleanShutdown => !has_closed_mark(&db)?,
        IntegrityCheck::Never => false,
    };
    if !checked && check == IntegrityCheck::AfterUncleanShutdown {
        debug!(path = %world.display(), "data file closed normally; its integrity is not checked");
    }
    if checked {
        check_integrity(&mut db)?;
    }
    check_format(&db)?;
    drop(db);
    Ok(Some(handle))
}

/// redb opened on an [`Overlay`] over the closed data file of `handle`, so nothing done through
/// it reaches the file. An empty file opens as a new database.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the start reads a closed data file before it opens it"
)]
pub(super) fn open_overlay(handle: &Arc<Handle>) -> Result<Database> {
    let overlay = Overlay::new(Arc::clone(handle))?;
    let mut builder = Builder::new();
    // No page cache: each pass of the check reads a page once, and the overlay's read-ahead
    // holds the pages around it. On a 368 MiB world the check took 333 to 517 ms without one
    // and 545 to 630 ms with the run's 64 MiB, whose evictions cost more than they spare.
    builder.set_cache_size(0);
    builder
        .create_with_backend(overlay)
        .map_err(Error::from_open)
}

/// `offset + len`, failing past `u64`.
fn end_of(offset: u64, len: usize) -> io::Result<u64> {
    u64::try_from(len)
        .ok()
        .and_then(|len| offset.checked_add(len))
        .ok_or_else(|| io::ErrorKind::InvalidInput.into())
}

/// A distance within one read or write as an index: it is at most the length of a slice.
#[expect(
    clippy::expect_used,
    reason = "invariant: distances within a slice fit usize"
)]
fn index(bytes: u64) -> usize {
    usize::try_from(bytes).expect("a distance within a slice fits usize")
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    use std::cell::Cell;

    use redb::{ReadableDatabase, ReadableTableMetadata, TableDefinition};

    thread_local! {
        /// The most bytes of writes one [`Overlay`] on this thread kept.
        pub(crate) static WRITTEN_PEAK: Cell<usize> = const { Cell::new(0) };
    }

    /// The table the tests fill.
    const TABLE: TableDefinition<'static, &[u8], &[u8]> = TableDefinition::new("values");

    /// Values `from..from + n` of 64 KiB under their number, each filled with its low byte.
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn insert(db: &Database, from: u16, n: u16) -> redb::Result<(), redb::Error> {
        let txn = db.begin_write()?;
        {
            let mut table = txn.open_table(TABLE)?;
            for k in from..from + n {
                table.insert(
                    k.to_be_bytes().as_slice(),
                    vec![k.to_le_bytes()[0]; 64 << 10].as_slice(),
                )?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// redb opened on an [`Overlay`] over the file at `path`.
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn overlay(path: &Path) -> Database {
        let overlay = Overlay::new(Handle::open(path).unwrap().unwrap()).unwrap();
        Builder::new().create_with_backend(overlay).unwrap()
    }

    /// Commits that grow the file, and the close that cuts it again, stay in memory, where reads
    /// see them; the file keeps every byte. Writes past [`MAX_WRITTEN_BYTES`] fail.
    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn the_start_checks_backend_keeps_every_write_out_of_the_file() {
        let dir = std::env::temp_dir().join(format!("anchor-overlay-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).unwrap_or(());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.redb");
        insert(&Builder::new().create(&path).unwrap(), 0, 16).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let db = overlay(&path);
        insert(&db, 16, 64).unwrap();
        let read = db.begin_read().unwrap();
        let table = read.open_table(TABLE).unwrap();
        assert_eq!(table.len().unwrap(), 80);
        for k in [0_u16, 15, 16, 79] {
            let value = table.get(k.to_be_bytes().as_slice()).unwrap().unwrap();
            assert_eq!(value.value(), vec![k.to_le_bytes()[0]; 64 << 10]);
        }
        drop((table, read));
        let txn = db.begin_write().unwrap();
        txn.delete_table(TABLE).unwrap();
        txn.commit().unwrap();
        drop(db);
        assert!(
            std::fs::read(&path).unwrap() == bytes,
            "the file was written"
        );
        // More than the overlay keeps: the commit fails, and the file is still as it was.
        let db = overlay(&path);
        let n = u16::try_from(MAX_WRITTEN_BYTES >> 16).unwrap();
        assert!(insert(&db, 16, n).is_err());
        drop(db);
        assert!(
            std::fs::read(&path).unwrap() == bytes,
            "the file was written"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
