//! A read-only LevelDB reader for the `db` folder of a Bedrock world (Mojang's fork of LevelDB
//! 1.x, as BDS writes it).
//!
//! Opening writes nothing: no `LOCK`, no replay of the logs into new tables, no compaction
//! (rusty-leveldb does all three to the source folder). `CURRENT` names the
//! `MANIFEST`, whose version edits (google/leveldb `db/version_edit.cc`: tags 1 comparator,
//! 2 log number, 3 next file number, 4 last sequence, 5 compaction pointer, 6 deleted table,
//! 7 new table, 9 previous log number; BDS 1.26.52 writes 1 to 5 and 7) give the live tables per
//! level; tables on disk that the `MANIFEST` does not list are leftovers of a compaction and are
//! not read, or they would bring back deleted values. The logs numbered at or above the log
//! number (and the previous log, if any) hold the writes not yet in a table; they are read into
//! memory at open. Every key in a table or log has a sequence number; a read takes the newest
//! version of each user key and skips deleted keys.
//!
//! Limits, each checked before the bytes it covers are read or allocated: `CURRENT` 256 bytes,
//! `MANIFEST` 64 MiB, the live logs 64 MiB together and 1,048,576 distinct keys, 65,536 files in
//! the folder, a block 16 MiB stored or decompressed, a user key 1 KiB, 256 MiB of blocks held by
//! one scan (and 64 MiB of cached blocks).

pub(crate) mod log;
pub(crate) mod table;

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, btree_map};
use std::ffi::OsString;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::time::SystemTime;

use self::table::{Cursor, TableFile, Tables};
use super::{Error, Result, read_file};
use crate::storage::seal::Crc32c;

/// The longest user key.
pub(crate) const MAX_KEY: usize = 1024;
const MAX_CURRENT: u64 = 256;
const MAX_MANIFEST: u64 = 64 << 20;
const MAX_LOG_BYTES: u64 = 64 << 20;
const MAX_LOG_KEYS: usize = 1 << 20;
const MAX_FILES: usize = 65_536;
/// Decompressed blocks one scan may hold at once.
const MAX_HELD: usize = 256 << 20;
/// Levels in a LevelDB version.
pub(crate) const LEVELS: usize = 7;
/// The sequence and kind after the user key in an internal key.
pub(crate) const TAG: usize = 8;
/// The largest sequence number: 56 bits.
const MAX_SEQUENCE: u64 = (1 << 56) - 1;
/// Key kinds.
const DELETION: u8 = 0;
pub(crate) const VALUE: u8 = 1;
/// The only key order the reader knows: bytewise.
pub(crate) const COMPARATOR: &[u8] = b"leveldb.BytewiseComparator";
/// The masked CRC-32C (google/leveldb `util/crc32c.h`) adds this after rotating right by 15.
const MASK_DELTA: u32 = 0xa282_ead8;

/// The masked CRC-32C of `parts` put together, as LevelDB stores checksums.
pub(crate) fn masked_crc(parts: &[&[u8]]) -> u32 {
    let mut crc = Crc32c::new();
    for part in parts {
        crc.update(part);
    }
    crc.finish().rotate_right(15).wrapping_add(MASK_DELTA)
}

/// Internal key order: user key ascending, then sequence and kind descending (newest first).
fn compare(a: &[u8], b: &[u8]) -> Ordering {
    let (a_user, a_tag) = split(a);
    let (b_user, b_tag) = split(b);
    a_user.cmp(b_user).then_with(|| b_tag.cmp(&a_tag))
}

/// The user key and tag of an internal key (decoders reject keys shorter than a tag).
fn split(key: &[u8]) -> (&[u8], u64) {
    match key.split_last_chunk::<TAG>() {
        Some((user, tag)) => (user, u64::from_le_bytes(*tag)),
        None => (key, 0),
    }
}

/// The internal key that sorts before every version of `user`.
fn seek_key(user: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(user.len() + TAG);
    key.extend_from_slice(user);
    key.extend_from_slice(&(MAX_SEQUENCE << 8 | u64::from(VALUE)).to_le_bytes());
    key
}

/// Reads LevelDB's fixed-width and variable-length integers.
struct Bytes<'a>(&'a [u8]);

impl<'a> Bytes<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let (head, rest) = self
            .0
            .split_at_checked(n)
            .ok_or(Error::Malformed("value ends early"))?;
        self.0 = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8> {
        let (&b, rest) = self
            .0
            .split_first()
            .ok_or(Error::Malformed("value ends early"))?;
        self.0 = rest;
        Ok(b)
    }

    fn u32(&mut self) -> Result<u32> {
        let (head, rest) = self
            .0
            .split_first_chunk::<4>()
            .ok_or(Error::Malformed("value ends early"))?;
        self.0 = rest;
        Ok(u32::from_le_bytes(*head))
    }

    fn u64(&mut self) -> Result<u64> {
        let (head, rest) = self
            .0
            .split_first_chunk::<8>()
            .ok_or(Error::Malformed("value ends early"))?;
        self.0 = rest;
        Ok(u64::from_le_bytes(*head))
    }

    /// A varint of at most `bits` bits.
    fn varint(&mut self, bits: u32) -> Result<u64> {
        let mut value = 0_u64;
        for shift in (0..bits).step_by(7) {
            let b = self.u8()?;
            let part = u64::from(b & 0x7f);
            if bits - shift < 7 && part >> (bits - shift) != 0 {
                return Err(Error::Malformed("varint too long"));
            }
            value |= part << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Error::Malformed("varint too long"))
    }

    fn varint32(&mut self) -> Result<u32> {
        u32::try_from(self.varint(32)?).map_err(|_| Error::Malformed("varint too long"))
    }

    fn varint64(&mut self) -> Result<u64> {
        self.varint(64)
    }

    /// A varint32 length and that many bytes.
    fn prefixed(&mut self) -> Result<&'a [u8]> {
        let len = usize::try_from(self.varint32()?).map_err(|_| Error::Malformed("length"))?;
        self.take(len)
    }
}

/// A file's length and modification time: a source that changes during an import fails it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    pub(crate) fn new(len: u64, modified: SystemTime) -> Self {
        Self {
            len,
            modified: Some(modified),
        }
    }
}

/// The database folder: on disk, or in memory for tests.
pub(crate) enum Files {
    /// The folder, and the table files kept open between reads (at most 64).
    Disk {
        dir: PathBuf,
        #[expect(
            clippy::disallowed_types,
            reason = "off-tick: the import runs before the server starts"
        )]
        open: BTreeMap<String, std::fs::File>,
    },
    /// File names and contents.
    #[cfg(test)]
    Memory(BTreeMap<String, Vec<u8>>),
}

impl Files {
    /// The folder at `dir`.
    pub(crate) fn disk(dir: PathBuf) -> Self {
        Self::Disk {
            dir,
            open: BTreeMap::new(),
        }
    }

    fn path(&self, name: &std::ffi::OsStr) -> PathBuf {
        match self {
            Self::Disk { dir, .. } => dir.join(name),
            #[cfg(test)]
            Self::Memory(_) => PathBuf::from(name),
        }
    }

    /// `error` located in the file `name`.
    fn fault(&self, name: &str, error: Error) -> Error {
        Error::File {
            path: self.path(name.as_ref()),
            source: Box::new(error),
        }
    }

    /// Every file in the folder with its stamp.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the import runs before the server starts"
    )]
    fn list(&self) -> Result<BTreeMap<OsString, Stamp>> {
        let mut listing = BTreeMap::new();
        let mut add = |name: OsString, stamp: Stamp| {
            if listing.len() == MAX_FILES {
                return Err(Error::TooLarge {
                    what: "files in the database folder",
                    limit: MAX_FILES as u64,
                });
            }
            listing.insert(name, stamp);
            Ok(())
        };
        let listed = match self {
            Self::Disk { dir, .. } => (|| {
                for entry in std::fs::read_dir(dir).map_err(Error::Read)? {
                    let entry = entry.map_err(Error::Read)?;
                    let meta = entry.metadata().map_err(Error::Read)?;
                    let modified = meta.modified().map_err(Error::Read)?;
                    add(entry.file_name(), Stamp::new(meta.len(), modified))?;
                }
                Ok(())
            })(),
            #[cfg(test)]
            Self::Memory(files) => files.iter().try_for_each(|(name, bytes)| {
                let stamp = Stamp {
                    len: bytes.len() as u64,
                    modified: None,
                };
                add(OsString::from(name), stamp)
            }),
        };
        listed.map_err(|e| Error::File {
            path: self.path("".as_ref()),
            source: Box::new(e),
        })?;
        Ok(listing)
    }

    /// The whole file `name`, at most `limit` bytes.
    fn read(&self, name: &str, limit: u64) -> Result<Vec<u8>> {
        let read = match self {
            Self::Disk { dir, .. } => read_file(&dir.join(name), limit),
            #[cfg(test)]
            Self::Memory(files) => match files.get(name) {
                Some(bytes) if bytes.len() as u64 > limit => Err(Error::TooLarge {
                    what: "file",
                    limit,
                }),
                Some(bytes) => Ok(bytes.clone()),
                None => Err(Error::Read(std::io::ErrorKind::NotFound.into())),
            },
        };
        read.map_err(|e| self.fault(name, e))
    }

    /// Fills `buf` from the file `name` at `offset`; callers check the range against its size.
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: the import runs before the server starts"
    )]
    fn read_at(&mut self, name: &str, offset: u64, buf: &mut [u8]) -> Result<()> {
        let read = match self {
            Self::Disk { dir, open } => {
                let mut read = || {
                    let file = match open.entry(name.to_owned()) {
                        btree_map::Entry::Occupied(file) => file.into_mut(),
                        btree_map::Entry::Vacant(slot) => {
                            slot.insert(std::fs::File::open(dir.join(name))?)
                        }
                    };
                    file.seek(SeekFrom::Start(offset))?;
                    file.read_exact(buf)
                };
                let read = read().map_err(Error::Read);
                if open.len() > table::MAX_OPEN {
                    open.pop_first();
                }
                read
            }
            #[cfg(test)]
            Self::Memory(files) => {
                let start = usize::try_from(offset).unwrap_or(usize::MAX);
                match files
                    .get(name)
                    .and_then(|bytes| bytes.get(start..start.checked_add(buf.len())?))
                {
                    Some(bytes) => {
                        buf.copy_from_slice(bytes);
                        Ok(())
                    }
                    None => Err(Error::Read(std::io::ErrorKind::UnexpectedEof.into())),
                }
            }
        };
        read.map_err(|e| self.fault(name, e))
    }
}

/// What a file name in the folder is.
enum Name {
    /// A table, `<number>.ldb`.
    Ldb(u64),
    /// A table under its older name, `<number>.sst`.
    Sst(u64),
    Log(u64),
    Other,
}

impl Name {
    fn parse(name: &str) -> Self {
        let Some((number, extension)) = name.split_once('.') else {
            return Self::Other;
        };
        let number = if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) {
            number.parse::<u64>().ok()
        } else {
            None
        };
        match (number, extension) {
            (Some(n), "ldb") => Self::Ldb(n),
            (Some(n), "sst") => Self::Sst(n),
            (Some(n), "log") => Self::Log(n),
            _ => Self::Other,
        }
    }
}

/// The `MANIFEST` file `CURRENT` names: `MANIFEST-<number>` and a line feed.
fn parse_current(bytes: &[u8]) -> Result<&str> {
    let name = bytes
        .strip_suffix(b"\n")
        .and_then(|name| std::str::from_utf8(name).ok())
        .ok_or(Error::Malformed(
            "CURRENT is not a file name and a line feed",
        ))?;
    let number = name
        .strip_prefix("MANIFEST-")
        .ok_or(Error::Malformed("CURRENT does not name a MANIFEST"))?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Malformed("CURRENT does not name a MANIFEST"));
    }
    Ok(name)
}

/// The tables and logs a `MANIFEST` describes.
#[derive(Debug, Default)]
struct Version {
    files: [BTreeMap<u64, TableFile>; LEVELS],
    log_number: Option<u64>,
    prev_log_number: u64,
}

/// Replays the version edits of a `MANIFEST`.
fn replay(manifest: &[u8]) -> Result<Version> {
    let mut version = Version::default();
    log::records(manifest, |record| apply(&mut version, record))?;
    if version.log_number.is_none() {
        return Err(Error::Malformed("MANIFEST has no log number"));
    }
    Ok(version)
}

/// Applies one version edit: its deleted tables, then its new ones (LevelDB `VersionSet::Builder`).
fn apply(version: &mut Version, record: &[u8]) -> Result<()> {
    let mut bytes = Bytes(record);
    let mut deleted = Vec::new();
    let mut added = Vec::new();
    let level = |bytes: &mut Bytes<'_>| -> Result<usize> {
        usize::try_from(bytes.varint32()?)
            .ok()
            .filter(|&level| level < LEVELS)
            .ok_or(Error::Malformed("version edit level"))
    };
    let internal = |bytes: &mut Bytes<'_>| -> Result<Vec<u8>> {
        let key = bytes.prefixed()?;
        if key.len() > MAX_KEY + TAG {
            return Err(Error::TooLarge {
                what: "key",
                limit: MAX_KEY as u64,
            });
        }
        table::check_internal(key)?;
        Ok(key.to_vec())
    };
    while !bytes.0.is_empty() {
        match bytes.varint32()? {
            1 => {
                if bytes.prefixed()? != COMPARATOR {
                    return Err(Error::Malformed(
                        "comparator is not leveldb.BytewiseComparator",
                    ));
                }
            }
            2 => version.log_number = Some(bytes.varint64()?),
            3 | 4 => {
                bytes.varint64()?;
            }
            5 => {
                level(&mut bytes)?;
                internal(&mut bytes)?;
            }
            6 => deleted.push((level(&mut bytes)?, bytes.varint64()?)),
            7 => {
                let level = level(&mut bytes)?;
                let file = TableFile {
                    number: bytes.varint64()?,
                    size: bytes.varint64()?,
                    smallest: internal(&mut bytes)?,
                    largest: internal(&mut bytes)?,
                };
                if compare(&file.smallest, &file.largest) == Ordering::Greater {
                    return Err(Error::Malformed("table smallest key after its largest"));
                }
                added.push((level, file));
            }
            9 => version.prev_log_number = bytes.varint64()?,
            _ => return Err(Error::Malformed("unknown version edit tag")),
        }
    }
    for (level, number) in deleted {
        if let Some(files) = version.files.get_mut(level) {
            files.remove(&number);
        }
    }
    for (level, file) in added {
        if let Some(files) = version.files.get_mut(level) {
            files.insert(file.number, file);
        }
    }
    Ok(())
}

/// A live log write: the newest version of its key.
#[derive(Debug)]
struct Logged {
    key: Vec<u8>,
    sequence: u64,
    value: Option<Vec<u8>>,
}

/// Reads the live logs `(number, name, length)` in number order: the newest write of each key.
fn read_logs(files: &Files, logs: &[(u64, String, u64)]) -> Result<Vec<Logged>> {
    let mut writes: BTreeMap<Vec<u8>, (u64, Option<Vec<u8>>)> = BTreeMap::new();
    for (_, name, _) in logs {
        let data = files.read(name, MAX_LOG_BYTES)?;
        log::records(&data, |record| {
            log::batch(record, |write| {
                if let Some((sequence, value)) = writes.get_mut(write.key) {
                    if write.sequence > *sequence {
                        *sequence = write.sequence;
                        *value = write.value.map(<[u8]>::to_vec);
                    }
                    return Ok(());
                }
                if writes.len() == MAX_LOG_KEYS {
                    return Err(Error::TooLarge {
                        what: "keys in the live logs",
                        limit: MAX_LOG_KEYS as u64,
                    });
                }
                writes.insert(
                    write.key.to_vec(),
                    (write.sequence, write.value.map(<[u8]>::to_vec)),
                );
                Ok(())
            })
        })
        .map_err(|e| files.fault(name, e))?;
    }
    Ok(writes
        .into_iter()
        .map(|(key, (sequence, value))| Logged {
            key,
            sequence,
            value,
        })
        .collect())
}

/// An open database.
pub(crate) struct Db {
    /// Level 0 tables may overlap; the tables of each other level are sorted and disjoint.
    levels: [Vec<TableFile>; LEVELS],
    /// The live log writes, by key.
    memory: Vec<Logged>,
    tables: Tables,
    /// The folder's files when it was opened.
    listing: BTreeMap<OsString, Stamp>,
}

impl Db {
    /// Opens the database in `files`, reading `CURRENT`, the `MANIFEST` and the live logs.
    pub(crate) fn open(files: Files) -> Result<Self> {
        let listing = files.list()?;
        let current = files.read("CURRENT", MAX_CURRENT)?;
        let manifest = parse_current(&current).map_err(|e| files.fault("CURRENT", e))?;
        let edits = files.read(manifest, MAX_MANIFEST)?;
        let version = replay(&edits).map_err(|e| files.fault(manifest, e))?;
        let manifest = manifest.to_owned();
        let in_manifest = |e| files.fault(&manifest, e);

        let mut tables = BTreeMap::new();
        let mut logs = Vec::new();
        for (name, stamp) in &listing {
            let Some(name) = name.to_str() else { continue };
            match Name::parse(name) {
                // LevelDB opens `.ldb` first, then `.sst`.
                Name::Ldb(n) => {
                    tables.insert(n, (name.to_owned(), stamp.len));
                }
                Name::Sst(n) => {
                    tables
                        .entry(n)
                        .or_insert_with(|| (name.to_owned(), stamp.len));
                }
                Name::Log(n) => logs.push((n, name.to_owned(), stamp.len)),
                Name::Other => {}
            }
        }

        let mut levels: [Vec<TableFile>; LEVELS] = Default::default();
        let mut seen = BTreeSet::new();
        for (level, files_at) in version.files.into_iter().enumerate() {
            for (number, file) in files_at {
                if !seen.insert(number) {
                    return Err(in_manifest(Error::Malformed("table in two levels")));
                }
                match tables.get(&number) {
                    None => return Err(in_manifest(Error::Malformed("a listed table is missing"))),
                    Some((name, len)) if *len != file.size => {
                        return Err(files.fault(
                            name,
                            Error::Malformed("table size differs from the MANIFEST"),
                        ));
                    }
                    Some(_) => {}
                }
                if let Some(at) = levels.get_mut(level) {
                    at.push(file);
                }
            }
        }
        for level in levels.iter_mut().skip(1) {
            level.sort_by(|a, b| compare(&a.smallest, &b.smallest));
            let disjoint = level
                .windows(2)
                .all(|w| matches!(w, [a, b] if compare(&a.largest, &b.smallest) == Ordering::Less));
            if !disjoint {
                return Err(in_manifest(Error::Malformed("tables of a level overlap")));
            }
        }

        let log_number = version.log_number.unwrap_or(0);
        let prev = version.prev_log_number;
        logs.retain(|&(n, _, _)| n >= log_number || (prev != 0 && n == prev));
        logs.sort_unstable();
        let total = logs
            .iter()
            .fold(0_u64, |sum, &(_, _, len)| sum.saturating_add(len));
        if total > MAX_LOG_BYTES {
            return Err(Error::TooLarge {
                what: "live logs",
                limit: MAX_LOG_BYTES,
            });
        }
        let memory = read_logs(&files, &logs)?;
        let names = tables.into_iter().map(|(n, (name, _))| (n, name)).collect();
        Ok(Self {
            levels,
            memory,
            tables: Tables::new(files, names),
            listing,
        })
    }

    /// Calls `f` with each live key in `[start, end)` (to the last key without `end`) and its
    /// value, in key order, until `f` breaks.
    pub(crate) fn scan(
        &mut self,
        start: &[u8],
        end: Option<&[u8]>,
        mut f: impl FnMut(&[u8], &[u8]) -> Result<ControlFlow<()>>,
    ) -> Result<()> {
        let Self {
            levels,
            memory,
            tables,
            ..
        } = self;
        let (levels, memory) = (&*levels, memory.as_slice());
        let target = seek_key(start);
        let before_end = |file: &TableFile| end.is_none_or(|end| split(&file.smallest).0 < end);
        let mut sources = Vec::new();
        sources.push(Source::Memory {
            entries: memory,
            at: memory.partition_point(|e| e.key.as_slice() < start),
        });
        // Checked as each source opens, so the next one is not read once the bound is passed.
        let mut held = 0_usize;
        let [first_level, deeper_levels @ ..] = levels;
        for file in first_level {
            if split(&file.largest).0 >= start && before_end(file) {
                let cursor = Cursor::open(tables, file, Some(&target))?;
                held = held.saturating_add(cursor.held());
                check_held(held)?;
                sources.push(Source::Table { file, cursor });
            }
        }
        for files in deeper_levels {
            let at = files.partition_point(|file| split(&file.largest).0 < start);
            let mut source = Source::Level {
                files,
                at,
                cursor: None,
            };
            source.settle(tables, Some(&target), end)?;
            held = held.saturating_add(source.held());
            check_held(held)?;
            sources.push(source);
        }

        let mut heap = BinaryHeap::with_capacity(sources.len());
        for (i, source) in sources.iter().enumerate() {
            if let Some((user, tag, _)) = source.entry() {
                heap.push(Reverse((user.to_vec(), Reverse(tag), i)));
            }
        }
        let mut last: Option<Vec<u8>> = None;
        while let Some(Reverse((user, Reverse(tag), i))) = heap.pop() {
            if end.is_some_and(|end| user.as_slice() >= end) {
                break;
            }
            let Some(source) = sources.get_mut(i) else {
                break;
            };
            if last.as_deref() != Some(user.as_slice()) {
                if tag & 0xff == u64::from(VALUE) {
                    let value = source.entry().map_or(&[][..], |(_, _, value)| value);
                    if f(&user, value)?.is_break() {
                        return Ok(());
                    }
                }
                last = Some(user);
            }
            let before = source.held();
            source.advance(tables, end)?;
            held = held.saturating_sub(before).saturating_add(source.held());
            check_held(held)?;
            if let Some((user, tag, _)) = source.entry() {
                heap.push(Reverse((user.to_vec(), Reverse(tag), i)));
            }
        }
        Ok(())
    }

    /// The live value of `key` (the import reads chunks with [`Db::scan`] only).
    #[cfg(test)]
    pub(crate) fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut end = Vec::with_capacity(key.len() + 1);
        end.extend_from_slice(key);
        end.push(0);
        let mut found = None;
        self.scan(key, Some(&end), |_, value| {
            found = Some(value.to_vec());
            Ok(ControlFlow::Break(()))
        })?;
        Ok(found)
    }

    /// Fails with [`Error::SourceChanged`] unless the folder holds the same files, with the same
    /// lengths and modification times, as when it was opened.
    pub(crate) fn unchanged(&self) -> Result<()> {
        let now = self.tables.files.list()?;
        let changed = self
            .listing
            .iter()
            .find(|&(name, stamp)| now.get(name) != Some(stamp))
            .map(|(name, _)| name)
            .or_else(|| now.keys().find(|name| !self.listing.contains_key(*name)));
        match changed {
            None => Ok(()),
            Some(name) => Err(Error::SourceChanged {
                path: self.tables.files.path(name),
            }),
        }
    }
}

/// Fails once a scan holds more than [`MAX_HELD`] bytes of blocks.
fn check_held(held: usize) -> Result<()> {
    if held > MAX_HELD {
        return Err(Error::TooLarge {
            what: "blocks held by one scan",
            limit: MAX_HELD as u64,
        });
    }
    Ok(())
}

/// One sorted input of a scan.
enum Source<'a> {
    /// The live log writes.
    Memory { entries: &'a [Logged], at: usize },
    /// A level 0 table.
    Table { file: &'a TableFile, cursor: Cursor },
    /// A deeper level: its tables one after another. `at` is the next table to open; the cursor
    /// is in the one before.
    Level {
        files: &'a [TableFile],
        at: usize,
        cursor: Option<Cursor>,
    },
}

impl Source<'_> {
    /// The current user key, tag and value.
    fn entry(&self) -> Option<(&[u8], u64, &[u8])> {
        let (key, value) = match self {
            Self::Memory { entries, at } => {
                let e = entries.get(*at)?;
                let kind = if e.value.is_some() { VALUE } else { DELETION };
                let tag = e.sequence << 8 | u64::from(kind);
                return Some((&e.key, tag, e.value.as_deref().unwrap_or_default()));
            }
            Self::Table { cursor, .. } => cursor.entry()?,
            Self::Level { cursor, .. } => cursor.as_ref()?.entry()?,
        };
        let (user, tag) = split(key);
        Some((user, tag, value))
    }

    fn advance(&mut self, tables: &mut Tables, end: Option<&[u8]>) -> Result<()> {
        match self {
            Self::Memory { at, .. } => *at += 1,
            Self::Table { file, cursor } => cursor.advance(tables, file)?,
            Self::Level { files, at, cursor } => {
                if let (Some(c), Some(file)) = (
                    cursor.as_mut(),
                    at.checked_sub(1).and_then(|i| files.get(i)),
                ) {
                    c.advance(tables, file)?;
                }
                self.settle(tables, None, end)?;
            }
        }
        Ok(())
    }

    /// For a level: opens the next tables until the cursor is on an entry or none is left before
    /// `end`. Only the first table opened seeks `target`.
    fn settle(
        &mut self,
        tables: &mut Tables,
        mut target: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<()> {
        let Self::Level { files, at, cursor } = self else {
            return Ok(());
        };
        while cursor.as_ref().is_none_or(|c| c.entry().is_none()) {
            *cursor = None;
            let Some(file) = files.get(*at) else {
                return Ok(());
            };
            if end.is_some_and(|end| split(&file.smallest).0 >= end) {
                return Ok(());
            }
            *cursor = Some(Cursor::open(tables, file, target)?);
            target = None;
            *at += 1;
        }
        Ok(())
    }

    fn held(&self) -> usize {
        match self {
            Self::Memory { .. } => 0,
            Self::Table { cursor, .. } => cursor.held(),
            Self::Level { cursor, .. } => cursor.as_ref().map_or(0, Cursor::held),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
