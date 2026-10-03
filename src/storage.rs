//! The world file `data/world.redb`: start policy, batched reads and commits (PLAN.md 10.6).
//!
//! Only the save thread uses this module; every call blocks on file I/O. Tables map raw keys to
//! sealed values ([`seal`](seal::seal)): `meta` (one [`Meta`]), `worlds` (world name →
//! [`WorldRecord`](codec::WorldRecord)), `chunks` ([`ChunkKey`] →
//! [`ChunkRecord`](codec::ChunkRecord) for kind 0) and `players` ([`PlayerKey`] →
//! [`PlayerRecord`](codec::PlayerRecord)). Values are external input when read back: the
//! seal's checksum and the record decoders check them.
//!
//! Start ([`Storage::open`]): copy the closed file to `data/backups/`, open it (redb recovers a
//! crash here), run `check_integrity()`, refuse a damaged file or a newer format, compact, and
//! create the tables. A refused start writes the copy back, so the file is left exactly as it
//! was, and removes the copy. `check_integrity()` rewrites the file when it repairs it, and the
//! open itself writes the header, so the copy is what keeps the original bytes.
//!
//! Commits ([`Storage::commit`]) write a whole batch in one quick-repair write transaction. A
//! failure drops the handle, reopens the file and retries, at most [`RETRIES`] times. Values are
//! absolute, so a retry of a commit that did land writes the same state again.

mod backup;
pub(crate) mod codec;
pub(crate) mod seal;

use std::borrow::Cow;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use redb::backends::FileBackend;
use redb::{
    Builder, Database, DatabaseError, ReadOnlyTable, ReadTransaction, ReadableDatabase,
    ReadableTable, ReadableTableMetadata, StorageError, TableDefinition, TableError,
};
use tracing::{error, info, warn};

use codec::Meta;
use seal::{FORMAT_VERSION, unseal};

/// The world file in the data folder.
pub(crate) const WORLD_FILE: &str = "world.redb";
/// The backup folder in the data folder.
pub(crate) const BACKUP_DIR: &str = "backups";
/// Failed commits retried before [`Storage::commit`] gives up.
pub(crate) const RETRIES: usize = 3;
/// redb's page cache. Loaded chunks live in the world, and the OS caches the file, so this only
/// spares a system call for pages read again; redb's default of 1 GiB would mostly hold copies.
const CACHE_BYTES: usize = 64 << 20;
/// Most worlds [`Storage::worlds`] returns.
pub(crate) const MAX_WORLDS: usize = 1024;

type Table = TableDefinition<'static, &'static [u8], &'static [u8]>;
const META: Table = TableDefinition::new("meta");
const WORLDS: Table = TableDefinition::new("worlds");
const CHUNKS: Table = TableDefinition::new("chunks");
const PLAYERS: Table = TableDefinition::new("players");
/// The one key of the `meta` table.
const META_KEY: &[u8] = b"meta";

/// A failure of the world file or of one stored value.
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum Error {
    /// A file or folder under the data folder could not be read or written.
    Io(io::Error),
    /// Another process has the world file open.
    InUse,
    /// redb failed for a reason other than damage, such as I/O.
    Database(redb::Error),
    /// The world file failed redb's integrity check (or redb repaired it, which means it was
    /// damaged); the source is redb's report, if it gave one.
    DamagedFile(Option<redb::Error>),
    /// A value does not match its checksum.
    Checksum,
    /// A value's header is invalid, or its length disagrees with its body.
    Malformed(&'static str),
    /// A value's body does not decompress.
    Compression(crate::protocol::compression::Error),
    /// A value's record does not decode.
    Record(crate::protocol::Error),
    /// A value was written by a newer Anchor, in format `version`.
    NewerFormat {
        /// The value's format version.
        version: u16,
    },
    /// A record cannot be encoded: a field is invalid or it is larger than
    /// [`MAX_RECORD`](seal::MAX_RECORD).
    Encode(crate::protocol::Error),
    /// [`Storage::worlds`] found more than [`MAX_WORLDS`] worlds.
    TooManyWorlds,
}

/// The result of a storage operation.
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// Whether this is damage: of the file at start, of a value, or a page redb found corrupted.
    /// A damaged chunk is quarantined and a damaged player refused; the bytes stay as they are.
    pub(crate) fn is_damage(&self) -> bool {
        matches!(
            self,
            Self::DamagedFile(_)
                | Self::Checksum
                | Self::Malformed(_)
                | Self::Compression(_)
                | Self::Record(_)
                | Self::Database(redb::Error::Corrupted(_))
        )
    }

    /// From an open of the file.
    fn from_open(e: DatabaseError) -> Self {
        match e {
            DatabaseError::DatabaseAlreadyOpen => Self::InUse,
            DatabaseError::Storage(StorageError::Corrupted(_)) => Self::DamagedFile(Some(e.into())),
            e => Self::Database(e.into()),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => f.write_str("world file i/o failed"),
            Self::InUse => f.write_str("another process has the world file open"),
            Self::Database(_) => f.write_str("the world database failed"),
            Self::DamagedFile(_) => f.write_str("the world file failed its integrity check"),
            Self::Checksum => f.write_str("a stored value does not match its checksum"),
            Self::Malformed(reason) => write!(f, "a stored value is malformed: {reason}"),
            Self::Compression(_) => f.write_str("a stored value does not decompress"),
            Self::Record(_) => f.write_str("a stored record does not decode"),
            Self::NewerFormat { version } => write!(
                f,
                "a stored value has format {version}, newer than {FORMAT_VERSION}"
            ),
            Self::Encode(_) => f.write_str("a record cannot be encoded"),
            Self::TooManyWorlds => write!(f, "more than {MAX_WORLDS} worlds are stored"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Database(e) | Self::DamagedFile(Some(e)) => Some(e),
            Self::Compression(e) => Some(e),
            Self::Record(e) | Self::Encode(e) => Some(e),
            Self::InUse
            | Self::DamagedFile(None)
            | Self::Checksum
            | Self::Malformed(_)
            | Self::NewerFormat { .. }
            | Self::TooManyWorlds => None,
        }
    }
}

/// redb's errors are [`Error::Database`]; [`Error::is_damage`] tells corruption apart.
macro_rules! from_redb {
    ($($error:ty),*) => {$(
        impl From<$error> for Error {
            fn from(e: $error) -> Self {
                Self::Database(e.into())
            }
        }
    )*};
}

from_redb!(
    redb::Error,
    redb::TransactionError,
    TableError,
    redb::StorageError,
    redb::CommitError,
    redb::CompactionError,
    DatabaseError
);

/// What a `chunks` value holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ChunkKind {
    /// Blocks and biomes ([`ChunkRecord`](codec::ChunkRecord)). Block entities and entities
    /// come in M7.
    Terrain = 0,
}

/// The key of a `chunks` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ChunkKey {
    /// The world's number ([`WorldRecord::number`](codec::WorldRecord::number)).
    pub(crate) world: u32,
    /// The chunk's x coordinate.
    pub(crate) x: i32,
    /// The chunk's z coordinate.
    pub(crate) z: i32,
    /// What the value holds.
    pub(crate) kind: ChunkKind,
}

impl ChunkKey {
    /// 13 bytes: world, x and z big-endian with the sign bit flipped (so keys sort by
    /// coordinate), then the kind.
    pub(crate) fn to_bytes(self) -> [u8; 13] {
        let flip = |c: i32| (c.cast_unsigned() ^ 0x8000_0000).to_be_bytes();
        let [w0, w1, w2, w3] = self.world.to_be_bytes();
        let [x0, x1, x2, x3] = flip(self.x);
        let [z0, z1, z2, z3] = flip(self.z);
        [
            w0,
            w1,
            w2,
            w3,
            x0,
            x1,
            x2,
            x3,
            z0,
            z1,
            z2,
            z3,
            self.kind as u8,
        ]
    }
}

/// The key of a `players` value: the XUID in decimal, or `offline:` and the lowercase name for
/// players who did not sign in to Xbox Live.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PlayerKey(String);

impl PlayerKey {
    /// The key of a signed-in player.
    pub(crate) fn xuid(xuid: u64) -> Self {
        Self(xuid.to_string())
    }

    /// The key of an offline player: names that differ only in ASCII case are one player, as the
    /// tick matches them (PMMP `strcasecmp`).
    pub(crate) fn offline(name: &str) -> Self {
        Self(format!("offline:{}", name.to_ascii_lowercase()))
    }

    /// The key as text.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// The key of this text, as [`as_str`](Self::as_str) gave it (a logged key).
    #[cfg(test)]
    pub(crate) fn from_text(text: &str) -> Self {
        Self(text.to_owned())
    }
}

/// A stored value's place: its table and key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Key {
    /// The `meta` value.
    Meta,
    /// A `worlds` value, by world name.
    World(String),
    /// A `chunks` value.
    Chunk(ChunkKey),
    /// A `players` value.
    Player(PlayerKey),
}

impl Key {
    /// The key's bytes in its table, which the value's seal also covers.
    pub(crate) fn to_bytes(&self) -> Cow<'_, [u8]> {
        match self {
            Self::Meta => Cow::Borrowed(META_KEY),
            Self::World(name) => Cow::Borrowed(name.as_bytes()),
            Self::Chunk(key) => Cow::Owned(key.to_bytes().to_vec()),
            Self::Player(PlayerKey(key)) => Cow::Borrowed(key.as_bytes()),
        }
    }

    /// The key's table among `tables`, given in the order `meta`, `worlds`, `chunks`, `players`.
    fn pick<'a, T>(&self, tables: &'a [T; 4]) -> &'a T {
        let [meta, worlds, chunks, players] = tables;
        match self {
            Self::Meta => meta,
            Self::World(_) => worlds,
            Self::Chunk(_) => chunks,
            Self::Player(_) => players,
        }
    }

    /// [`pick`](Self::pick) for writing.
    fn pick_mut<'a, T>(&self, tables: &'a mut [T; 4]) -> &'a mut T {
        let [meta, worlds, chunks, players] = tables;
        match self {
            Self::Meta => meta,
            Self::World(_) => worlds,
            Self::Chunk(_) => chunks,
            Self::Player(_) => players,
        }
    }
}

/// Writes for one commit: each key gets a sealed value or is deleted. Later writes to a key
/// replace earlier ones.
#[derive(Debug, Default)]
pub(crate) struct Batch {
    writes: Vec<(Key, Option<Vec<u8>>)>,
}

impl Batch {
    /// An empty batch with room for `capacity` writes.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            writes: Vec::with_capacity(capacity),
        }
    }

    /// Stores `sealed` (from [`seal`](seal::seal)) under `key`.
    pub(crate) fn put(&mut self, key: Key, sealed: Vec<u8>) {
        self.writes.push((key, Some(sealed)));
    }

    /// Deletes the value under `key`, if any.
    #[cfg(test)]
    pub(crate) fn delete(&mut self, key: Key) {
        self.writes.push((key, None));
    }

    /// The number of writes.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.writes.len()
    }

    /// Whether the batch has no writes.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }
}

/// Where the database lives: the world file, or a test's simulated disk.
#[derive(Debug)]
enum Source {
    File(PathBuf),
    #[cfg(test)]
    Memory(tests::Disk),
}

impl Source {
    /// Opens the database, creating an empty one if there is no file. redb recovers an
    /// interrupted commit here.
    #[expect(
        clippy::disallowed_types,
        clippy::disallowed_methods,
        reason = "off-tick: the save thread opens the world file"
    )]
    fn open(&self) -> Result<Database> {
        let mut builder = Builder::new();
        builder.set_cache_size(CACHE_BYTES);
        let opened = match self {
            Self::File(path) => {
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(path)
                    .map_err(Error::Io)?;
                FileBackend::new(file).and_then(|backend| builder.create_with_backend(backend))
            }
            #[cfg(test)]
            Self::Memory(disk) => builder.create_with_backend(disk.backend()),
        };
        opened.map_err(Error::from_open)
    }
}

/// The open world file. Dropping it closes the file without compacting it.
#[derive(Debug)]
pub(crate) struct Storage {
    /// `None` after a failed commit could not reopen the file; the next call tries again.
    db: Option<Database>,
    source: Source,
}

impl Storage {
    /// Starts on `<data_dir>/world.redb` (PLAN.md 10.6): backs up the closed file, opens it,
    /// checks it, refuses a damaged file or a newer format (leaving the file as it was), compacts
    /// it, and keeps the [`backup::KEPT`] newest backups. A missing or empty file starts empty.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread creates the data folder once, at start"
    )]
    pub(crate) fn open(data_dir: &Path) -> Result<Self> {
        let world = data_dir.join(WORLD_FILE);
        let backups = data_dir.join(BACKUP_DIR);
        std::fs::create_dir_all(data_dir).map_err(Error::Io)?;
        let copy = backup::create(&world, &backups)?;
        match Self::start(Source::File(world.clone())) {
            Ok(storage) => {
                if let Some(copy) = &copy {
                    if let Err(e) = backup::prune(&backups, copy) {
                        warn!(error = %e, "could not remove old world backups");
                    }
                    info!(backup = %copy.display(), "world file opened");
                } else {
                    info!("world file created");
                }
                Ok(storage)
            }
            Err(e) => {
                if let Some(copy) = copy {
                    if e.is_damage() || matches!(e, Error::NewerFormat { .. }) {
                        // The open and the check may have written to the file: put back the
                        // bytes it had, so the operator finds it as it was.
                        if let Err(restore) = backup::restore(&copy, &world) {
                            error!(
                                backup = %copy.display(),
                                error = %restore,
                                "could not put the world file back; the backup holds its bytes"
                            );
                        }
                    } else {
                        warn!(
                            backup = %copy.display(),
                            error = %e,
                            "world file did not open; its backup is kept"
                        );
                    }
                }
                Err(e)
            }
        }
    }

    /// Opens, checks and prepares the database at `source`.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread checks and compacts the world file at start"
    )]
    fn start(source: Source) -> Result<Self> {
        let mut db = source.open()?;
        match db.check_integrity() {
            Ok(true) => {}
            // redb repaired the file in place: it was damaged.
            Ok(false) => return Err(Error::DamagedFile(None)),
            Err(e @ DatabaseError::Storage(StorageError::Corrupted(_))) => {
                return Err(Error::DamagedFile(Some(e.into())));
            }
            Err(e) => return Err(e.into()),
        }
        let mut storage = Self {
            db: Some(db),
            source,
        };
        storage.check_format()?;
        if let Some(db) = &mut storage.db {
            db.compact()?;
        }
        // An empty commit creates the tables a new file lacks.
        storage.commit(&Batch::default())?;
        Ok(storage)
    }

    /// Refuses a file whose last commit was written by a newer Anchor (its meta value's format)
    /// or whose meta value is damaged.
    fn check_format(&mut self) -> Result<()> {
        let [meta] = <[_; 1]>::try_from(self.read(&[Key::Meta])?)
            .map_err(|_| Error::Malformed("one read gives one value"))?;
        if let Some(sealed) = meta? {
            let version = seal::version(&sealed)?;
            if version > FORMAT_VERSION {
                return Err(Error::NewerFormat { version });
            }
            unseal::<Meta>(META_KEY, &sealed)?;
        }
        Ok(())
    }

    /// The open database, reopening the file if a failed commit dropped it.
    fn database(&mut self) -> Result<&Database> {
        let db = match self.db.take() {
            Some(db) => db,
            None => self.source.open()?,
        };
        Ok(self.db.insert(db))
    }

    /// Reads `keys` in one read transaction. Each result is the sealed value (open it with
    /// [`unseal`] under [`Key::to_bytes`]), `None` if there is none, or the error redb gave for
    /// that key alone.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread reads the world file"
    )]
    pub(crate) fn read(&mut self, keys: &[Key]) -> Result<Vec<Result<Option<Vec<u8>>>>> {
        let txn = self.database()?.begin_read()?;
        let tables = [
            open_read(&txn, META)?,
            open_read(&txn, WORLDS)?,
            open_read(&txn, CHUNKS)?,
            open_read(&txn, PLAYERS)?,
        ];
        Ok(keys
            .iter()
            .map(|key| match key.pick(&tables) {
                Some(table) => Ok(table.get(&*key.to_bytes())?.map(|v| v.value().to_vec())),
                None => Ok(None),
            })
            .collect())
    }

    /// Every stored world: its name and sealed [`WorldRecord`](codec::WorldRecord), in name
    /// order.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread reads the world file"
    )]
    pub(crate) fn worlds(&mut self) -> Result<Vec<(String, Vec<u8>)>> {
        let txn = self.database()?.begin_read()?;
        let Some(table) = open_read(&txn, WORLDS)? else {
            return Ok(Vec::new());
        };
        let count = usize::try_from(table.len()?).unwrap_or(usize::MAX);
        if count > MAX_WORLDS {
            return Err(Error::TooManyWorlds);
        }
        let mut worlds = Vec::with_capacity(count);
        for entry in table.iter()?.take(MAX_WORLDS) {
            let (name, sealed) = entry?;
            let name = String::from_utf8(name.value().to_vec())
                .map_err(|_| Error::Malformed("world name is not utf-8"))?;
            worlds.push((name, sealed.value().to_vec()));
        }
        Ok(worlds)
    }

    /// Writes `batch` in one durable write transaction with quick repair. On failure it drops
    /// the handle, reopens the file and tries again, up to [`RETRIES`] times, then returns the
    /// last error. Either the whole batch is committed or none of it.
    pub(crate) fn commit(&mut self, batch: &Batch) -> Result<()> {
        let mut failures = 0;
        loop {
            let result = self.database().and_then(|db| write(db, batch));
            match result {
                Ok(()) => return Ok(()),
                Err(e) if failures < RETRIES => {
                    failures += 1;
                    warn!(error = %e, retry = failures, "commit failed; reopening the world file");
                    self.db = None;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Compacts the file after the last commit and closes it.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread compacts the world file at shutdown"
    )]
    pub(crate) fn close(mut self) -> Result<()> {
        if let Some(mut db) = self.db.take() {
            db.compact()?;
        }
        Ok(())
    }
}

/// Opens `table` for reading; `None` before the first commit of a new file created it.
fn open_read(
    txn: &ReadTransaction,
    table: Table,
) -> Result<Option<ReadOnlyTable<&'static [u8], &'static [u8]>>> {
    match txn.open_table(table) {
        Ok(table) => Ok(Some(table)),
        Err(TableError::TableDoesNotExist(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// One write transaction holding all of `batch`.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread writes the world file"
)]
fn write(db: &Database, batch: &Batch) -> Result<()> {
    let mut txn = db.begin_write()?;
    txn.set_quick_repair(true);
    {
        let mut tables = [
            txn.open_table(META)?,
            txn.open_table(WORLDS)?,
            txn.open_table(CHUNKS)?,
            txn.open_table(PLAYERS)?,
        ];
        for (key, value) in &batch.writes {
            let table = key.pick_mut(&mut tables);
            let key = key.to_bytes();
            match value {
                Some(value) => drop(table.insert(&*key, value.as_slice())?),
                None => drop(table.remove(&*key)?),
            }
        }
    }
    txn.commit()?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use redb::StorageBackend;

    use super::codec::tests::{empty_chunk, random_chunk, terrain_chunk};
    use super::codec::{ChunkRecord, PlayerRecord, WorldRecord};
    use super::seal::seal;
    use super::*;
    use crate::clock;
    use crate::protocol::Rng;

    /// The redb page size: the unit a torn write keeps or loses.
    const PAGE: usize = 4096;

    /// A backend call the simulated disk counts.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Call {
        Write = 0,
        SetLen = 1,
        Sync = 2,
    }

    /// When the simulated disk fails calls.
    #[derive(Debug, Clone, Copy, Default)]
    pub(crate) enum Fault {
        #[default]
        None,
        /// The `at`-th call of kind `call` from now fails; the rest succeed.
        Once { call: Call, at: u64 },
        /// Every call from the `at`-th on fails: the process died there (or the disk is gone).
        From { at: u64 },
    }

    #[derive(Debug)]
    enum Op {
        Write(usize, Vec<u8>),
        SetLen(usize),
    }

    #[derive(Debug, Default)]
    struct State {
        /// What survives a power loss for certain: the bytes at the last sync.
        durable: Vec<u8>,
        /// What reads see: the operating system's view.
        current: Vec<u8>,
        /// Writes and length changes since the last sync, in order.
        unsynced: Vec<Op>,
        /// Calls of each [`Call`] kind since the fault was armed.
        calls: [u64; 3],
        fault: Fault,
        /// Calls that failed.
        failed: u64,
    }

    impl State {
        /// Counts a call and tells whether it fails.
        fn fails(&mut self, call: Call) -> bool {
            self.calls[call as usize] += 1;
            let count = self.calls[call as usize];
            let total: u64 = self.calls.iter().sum();
            let fails = match self.fault {
                Fault::None => false,
                Fault::Once { call: kind, at } => {
                    let hit = kind == call && count == at;
                    if hit {
                        self.fault = Fault::None;
                    }
                    hit
                }
                Fault::From { at } => total >= at,
            };
            self.failed += u64::from(fails);
            fails
        }
    }

    /// A simulated disk shared by every backend opened on it: a test reopens the database on
    /// the same disk, as the server reopens the same file.
    #[derive(Debug, Clone, Default)]
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: redb's backend trait takes &self from any thread"
    )]
    pub(crate) struct Disk(Arc<std::sync::Mutex<State>>);

    impl Disk {
        pub(crate) fn backend(&self) -> Backend {
            Backend(self.clone())
        }

        /// Arms `fault`, counting calls from zero.
        pub(crate) fn arm(&self, fault: Fault) {
            let mut state = self.0.lock().unwrap();
            state.fault = fault;
            state.calls = [0; 3];
        }

        fn failed(&self) -> u64 {
            self.0.lock().unwrap().failed
        }

        /// Cuts the power: unsynced writes are lost, or, with `some_survive`, a random subset of
        /// them reaches the disk in order, each torn at page granularity. The disk then holds
        /// only what survived, and the fault is cleared.
        pub(crate) fn power_loss(&self, rng: &mut Rng, some_survive: bool) {
            let mut state = self.0.lock().unwrap();
            let mut image = state.durable.clone();
            if some_survive {
                for op in &state.unsynced {
                    if rng.below(2) == 0 {
                        continue;
                    }
                    match op {
                        Op::SetLen(len) => image.resize(*len, 0),
                        Op::Write(offset, data) => {
                            let mut at = *offset;
                            let first = PAGE - offset % PAGE;
                            let (head, tail) = data.split_at(first.min(data.len()));
                            for piece in std::iter::once(head).chain(tail.chunks(PAGE)) {
                                let end = (at + piece.len()).min(image.len());
                                if rng.below(4) != 0 && at < end {
                                    image[at..end].copy_from_slice(&piece[..end - at]);
                                }
                                at += piece.len();
                            }
                        }
                    }
                }
            }
            state.current = image.clone();
            state.durable = image;
            state.unsynced.clear();
            state.fault = Fault::None;
        }
    }

    #[derive(Debug)]
    pub(crate) struct Backend(Disk);

    fn injected() -> io::Error {
        io::Error::other("injected fault")
    }

    impl StorageBackend for Backend {
        fn len(&self) -> io::Result<u64> {
            Ok(u64::try_from(self.0.0.lock().unwrap().current.len()).unwrap())
        }

        fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
            let state = self.0.0.lock().unwrap();
            let start = usize::try_from(offset).map_err(|_| injected())?;
            let bytes = start
                .checked_add(out.len())
                .and_then(|end| state.current.get(start..end))
                .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
            out.copy_from_slice(bytes);
            Ok(())
        }

        fn set_len(&self, len: u64) -> io::Result<()> {
            let mut state = self.0.0.lock().unwrap();
            if state.fails(Call::SetLen) {
                return Err(injected());
            }
            let len = usize::try_from(len).unwrap();
            state.current.resize(len, 0);
            state.unsynced.push(Op::SetLen(len));
            Ok(())
        }

        fn sync_data(&self) -> io::Result<()> {
            let mut state = self.0.0.lock().unwrap();
            if state.fails(Call::Sync) {
                return Err(injected());
            }
            state.durable = state.current.clone();
            state.unsynced.clear();
            Ok(())
        }

        fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
            let mut state = self.0.0.lock().unwrap();
            if state.fails(Call::Write) {
                return Err(injected());
            }
            let start = usize::try_from(offset).unwrap();
            let end = start + data.len();
            if end > state.current.len() {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            state.current[start..end].copy_from_slice(data);
            state.unsynced.push(Op::Write(start, data.to_vec()));
            Ok(())
        }
    }

    fn memory(disk: &Disk) -> Storage {
        Storage::start(Source::Memory(disk.clone())).unwrap()
    }

    /// Every stored value under `keys`, which must all unseal.
    fn contents(storage: &mut Storage, keys: &[Key]) -> BTreeMap<Key, Vec<u8>> {
        let values = storage.read(keys).unwrap();
        keys.iter()
            .zip(values)
            .filter_map(|(key, value)| Some((key.clone(), value.unwrap()?)))
            .inspect(|(key, sealed)| check_value(key, sealed))
            .collect()
    }

    fn check_value(key: &Key, sealed: &[u8]) {
        let bytes = key.to_bytes();
        match key {
            Key::Meta => drop(unseal::<Meta>(&bytes, sealed).unwrap()),
            Key::World(_) => drop(unseal::<WorldRecord>(&bytes, sealed).unwrap()),
            Key::Chunk(_) => drop(unseal::<ChunkRecord>(&bytes, sealed).unwrap()),
            Key::Player(_) => drop(unseal::<PlayerRecord>(&bytes, sealed).unwrap()),
        }
    }

    fn chunk_key(x: i32, z: i32) -> Key {
        Key::Chunk(ChunkKey {
            world: 0,
            x,
            z,
            kind: ChunkKind::Terrain,
        })
    }

    /// A small chunk that differs by `n`.
    fn small_chunk(n: u32) -> ChunkRecord {
        let mut chunk = empty_chunk();
        chunk.sub_chunks[0].biomes.palette[0] = n;
        chunk
    }

    /// Applies a random batch to `model` and returns it: puts and deletes over a few chunks, a
    /// player and the meta tick.
    fn random_batch(rng: &mut Rng, model: &mut BTreeMap<Key, Vec<u8>>, tick: u64) -> Batch {
        let mut batch = Batch::default();
        for _ in 0..=rng.below(12) {
            let key = chunk_key(i32::try_from(rng.below(16)).unwrap() - 8, 3);
            if rng.below(5) == 0 {
                model.remove(&key);
                batch.delete(key);
            } else {
                let n = u32::try_from(rng.below(1000)).unwrap();
                let sealed = seal(&key.to_bytes(), &small_chunk(n)).unwrap();
                model.insert(key.clone(), sealed.clone());
                batch.put(key, sealed);
            }
        }
        let key = Key::Player(PlayerKey::offline("Steve"));
        let player = PlayerRecord {
            world: "world".to_owned(),
            position: [f32::from(rng.next().to_le_bytes()[0]), 64.0, 0.5],
        };
        let sealed = seal(&key.to_bytes(), &player).unwrap();
        model.insert(key.clone(), sealed.clone());
        batch.put(key, sealed);
        let sealed = seal(META_KEY, &Meta { tick }).unwrap();
        model.insert(Key::Meta, sealed.clone());
        batch.put(Key::Meta, sealed);
        batch
    }

    fn all_keys() -> Vec<Key> {
        let mut keys: Vec<Key> = (-8..8).map(|x| chunk_key(x, 3)).collect();
        keys.push(Key::Player(PlayerKey::offline("steve")));
        keys.push(Key::Meta);
        keys
    }

    #[test]
    fn keys_sort_by_world_then_coordinate() {
        let key = |world, x, z| {
            ChunkKey {
                world,
                x,
                z,
                kind: ChunkKind::Terrain,
            }
            .to_bytes()
        };
        assert_eq!(
            key(1, -1, 2),
            [0, 0, 0, 1, 0x7f, 0xff, 0xff, 0xff, 0x80, 0, 0, 2, 0]
        );
        let ordered = [
            key(0, i32::MIN, 0),
            key(0, -1, 5),
            key(0, 0, -5),
            key(0, 0, 4),
            key(0, i32::MAX, i32::MIN),
            key(1, i32::MIN, i32::MIN),
        ];
        assert!(ordered.is_sorted());
        assert_eq!(PlayerKey::xuid(2_535_000).0, "2535000");
        assert_eq!(PlayerKey::offline("StEve").0, "offline:steve");
    }

    #[test]
    fn values_round_trip_through_a_commit_and_a_reopen() {
        let disk = Disk::default();
        let mut storage = memory(&disk);
        let mut rng = Rng(11);
        let chunk = random_chunk(&mut rng);
        let world = WorldRecord {
            number: 3,
            seed: -9,
            spawn: [1, 2, 3],
            generator: "flat".to_owned(),
        };
        let keys = [
            chunk_key(-1, 1),
            Key::World("lobby".to_owned()),
            Key::Meta,
            Key::Player(PlayerKey::xuid(7)),
        ];
        let mut batch = Batch::with_capacity(4);
        batch.put(keys[0].clone(), seal(&keys[0].to_bytes(), &chunk).unwrap());
        batch.put(keys[1].clone(), seal(&keys[1].to_bytes(), &world).unwrap());
        batch.put(Key::Meta, seal(META_KEY, &Meta { tick: 40 }).unwrap());
        assert_eq!(batch.len(), 3);
        assert!(!batch.is_empty());
        storage.commit(&batch).unwrap();
        storage.close().unwrap();
        let mut storage = memory(&disk);
        let values = storage.read(&keys).unwrap();
        let [chunk_value, world_value, meta_value, player_value] =
            <[_; 4]>::try_from(values).unwrap();
        let unsealed: ChunkRecord =
            unseal(&keys[0].to_bytes(), &chunk_value.unwrap().unwrap()).unwrap();
        assert_eq!(unsealed, chunk);
        let world_value = world_value.unwrap().unwrap();
        assert_eq!(
            unseal::<WorldRecord>(b"lobby", &world_value).unwrap(),
            world
        );
        assert_eq!(
            unseal::<Meta>(META_KEY, &meta_value.unwrap().unwrap()).unwrap(),
            Meta { tick: 40 }
        );
        assert!(player_value.unwrap().is_none());
        assert_eq!(
            storage.worlds().unwrap(),
            [("lobby".to_owned(), world_value)]
        );
        // A deletion removes the value.
        let mut batch = Batch::default();
        batch.delete(keys[0].clone());
        storage.commit(&batch).unwrap();
        assert!(
            storage.read(&keys[..1]).unwrap()[0]
                .as_ref()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_failed_call_is_retried_and_the_commit_lands() {
        let mut rng = Rng(0x5eed_0001);
        let disk = Disk::default();
        let mut storage = memory(&disk);
        let mut model = BTreeMap::new();
        let mut tick = 0;
        for call in [Call::Write, Call::SetLen, Call::Sync] {
            for at in 1..=12 {
                tick += 1;
                let batch = random_batch(&mut rng, &mut model, tick);
                disk.arm(Fault::Once { call, at });
                storage.commit(&batch).unwrap();
                assert_eq!(contents(&mut storage, &all_keys()), model, "{call:?} {at}");
            }
        }
        assert!(disk.failed() >= 12, "only {} calls failed", disk.failed());
        disk.arm(Fault::None);
        storage.close().unwrap();
        let mut storage = memory(&disk);
        assert_eq!(contents(&mut storage, &all_keys()), model);
    }

    #[test]
    fn a_commit_fails_after_three_retries() {
        let mut rng = Rng(0x5eed_0002);
        for at in [1, 3, 6] {
            let disk = Disk::default();
            let mut storage = memory(&disk);
            let mut model = BTreeMap::new();
            let first = random_batch(&mut rng, &mut model, 1);
            storage.commit(&first).unwrap();
            let before = model.clone();
            let second = random_batch(&mut rng, &mut model, 2);
            disk.arm(Fault::From { at });
            assert!(storage.commit(&second).is_err(), "{at}");
            // The failed commit, then three reopens that each fail on their first write.
            assert!(disk.failed() >= 4, "{} calls failed", disk.failed());
            drop(storage);
            disk.power_loss(&mut rng, false);
            let mut storage = memory(&disk);
            assert_eq!(contents(&mut storage, &all_keys()), before);
        }
    }

    /// A crash at a random call of a commit (or of the compaction when closing after it), then a
    /// power loss: the file passes the integrity check and holds exactly the previous commit or
    /// the new one (always the new one if the commit returned). 1,000 runs, fixed seed.
    #[test]
    fn a_crash_at_any_point_of_a_commit_keeps_one_whole_commit() {
        let mut rng = Rng(0x00c0_ffee);
        let (mut old, mut new) = (0, 0);
        for run in 0..1_000 {
            let disk = Disk::default();
            let mut storage = memory(&disk);
            let mut model = BTreeMap::new();
            for tick in 1..=1 + rng.below(2) {
                let batch = random_batch(&mut rng, &mut model, u64::try_from(tick).unwrap());
                storage.commit(&batch).unwrap();
            }
            let before = model.clone();
            let batch = random_batch(&mut rng, &mut model, 100);
            let crash = Fault::From {
                at: 1 + u64::try_from(rng.below(40)).unwrap(),
            };
            // One run in four commits and crashes in the compaction of the close instead.
            let landed = if rng.below(4) == 0 {
                storage.commit(&batch).unwrap();
                disk.arm(crash);
                drop(storage.close());
                true
            } else {
                disk.arm(crash);
                let landed = storage.commit(&batch).is_ok();
                drop(storage);
                landed
            };
            let some_survive = rng.below(4) != 0;
            disk.power_loss(&mut rng, some_survive);
            let mut storage = Storage::start(Source::Memory(disk.clone()))
                .unwrap_or_else(|e| panic!("run {run}: {e}: {:?}", std::error::Error::source(&e)));
            let found = contents(&mut storage, &all_keys());
            if found == model {
                new += 1;
            } else {
                assert!(!landed, "run {run}: a returned commit was lost");
                assert_eq!(
                    found, before,
                    "run {run}: neither the old nor the new commit"
                );
                old += 1;
            }
        }
        // Both outcomes must occur, or the crash points miss the commit.
        assert!(old > 100 && new > 100, "old {old}, new {new}");
    }

    /// A per-test folder under the system temp folder, removed on drop.
    struct TempDir(PathBuf);

    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("anchor-storage-{name}-{}", std::process::id()));
            std::fs::remove_dir_all(&dir).unwrap_or(());
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn world(&self) -> Vec<u8> {
            std::fs::read(self.0.join(WORLD_FILE)).unwrap()
        }

        /// The file's length; readable while the file is open, unlike its bytes on Windows.
        fn world_len(&self) -> u64 {
            std::fs::metadata(self.0.join(WORLD_FILE)).unwrap().len()
        }

        fn set_world(&self, bytes: &[u8]) {
            std::fs::write(self.0.join(WORLD_FILE), bytes).unwrap();
        }

        /// Backup names, oldest first.
        fn backups(&self) -> Vec<String> {
            let Ok(entries) = std::fs::read_dir(self.0.join(BACKUP_DIR)) else {
                return Vec::new();
            };
            let mut names: Vec<String> = entries
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort_by_key(|name| backup::order(name).unwrap());
            names
        }
    }

    impl Drop for TempDir {
        #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap_or(());
        }
    }

    #[test]
    fn a_new_folder_starts_empty_and_keeps_what_was_committed() {
        let dir = TempDir::new("new");
        let mut storage = Storage::open(&dir.0).unwrap();
        assert!(
            storage.read(&[Key::Meta]).unwrap()[0]
                .as_ref()
                .unwrap()
                .is_none()
        );
        assert!(storage.worlds().unwrap().is_empty());
        let mut batch = Batch::default();
        batch.put(Key::Meta, seal(META_KEY, &Meta { tick: 9 }).unwrap());
        storage.commit(&batch).unwrap();
        storage.close().unwrap();
        assert!(
            dir.backups().is_empty(),
            "a new file has nothing to back up"
        );
        let mut storage = Storage::open(&dir.0).unwrap();
        let meta = storage
            .read(&[Key::Meta])
            .unwrap()
            .remove(0)
            .unwrap()
            .unwrap();
        assert_eq!(unseal::<Meta>(META_KEY, &meta).unwrap(), Meta { tick: 9 });
        assert_eq!(dir.backups().len(), 1);
    }

    #[test]
    fn a_second_process_cannot_open_the_file() {
        let dir = TempDir::new("in-use");
        let storage = Storage::open(&dir.0).unwrap();
        assert!(matches!(Storage::open(&dir.0), Err(Error::InUse)));
        assert!(dir.backups().is_empty(), "no copy of a file in use");
        storage.close().unwrap();
    }

    #[test]
    fn backups_are_capped_at_three_and_never_overwritten() {
        let dir = TempDir::new("backups");
        Storage::open(&dir.0).unwrap().close().unwrap();
        let mut made = Vec::new();
        for _ in 0..5 {
            Storage::open(&dir.0).unwrap().close().unwrap();
            let names = dir.backups();
            assert!(names.len() <= backup::KEPT, "{names:?}");
            let newest = names.last().unwrap().clone();
            assert!(!made.contains(&newest), "{newest} was made twice");
            made.push(newest);
        }
        assert_eq!(dir.backups(), made[made.len() - backup::KEPT..]);
    }

    #[test]
    fn a_damaged_file_is_refused_and_left_as_it_was() {
        let dir = TempDir::new("damaged");
        let mut storage = Storage::open(&dir.0).unwrap();
        // Random letters do not compress, so the name is stored as it is and can be found.
        let mut rng = Rng(5);
        let name: String = (0..200)
            .map(|_| char::from(b'a' + u8::try_from(rng.below(26)).unwrap()))
            .collect();
        let key = Key::Player(PlayerKey::xuid(1));
        let player = PlayerRecord {
            world: name.clone(),
            position: [0.0; 3],
        };
        let mut batch = Batch::default();
        batch.put(key.clone(), seal(&key.to_bytes(), &player).unwrap());
        storage.commit(&batch).unwrap();
        storage.close().unwrap();
        let mut bytes = dir.world();
        let mut hits = 0;
        for at in 0..bytes.len() - name.len() {
            if bytes[at..].starts_with(name.as_bytes()) {
                bytes[at + 100] ^= 0x20;
                hits += 1;
            }
        }
        assert!(hits > 0);
        dir.set_world(&bytes);
        let e = Storage::open(&dir.0).unwrap_err();
        assert!(e.is_damage(), "{e:?}");
        assert!(dir.world().eq(&bytes), "the file is left as it was");
        assert!(dir.backups().is_empty(), "the copy went back into the file");
        // A second try finds the same file and refuses it the same way.
        assert!(Storage::open(&dir.0).unwrap_err().is_damage());
        assert!(dir.world().eq(&bytes), "the file is unchanged");
    }

    #[test]
    fn a_newer_format_is_refused_and_left_as_it_was() {
        let dir = TempDir::new("newer");
        let mut storage = Storage::open(&dir.0).unwrap();
        let mut sealed = seal(META_KEY, &Meta { tick: 1 }).unwrap();
        sealed[..2].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let mut batch = Batch::default();
        batch.put(Key::Meta, sealed);
        storage.commit(&batch).unwrap();
        storage.close().unwrap();
        let bytes = dir.world();
        let e = Storage::open(&dir.0).unwrap_err();
        assert!(
            matches!(e, Error::NewerFormat { version } if version == FORMAT_VERSION + 1),
            "{e:?}"
        );
        assert!(dir.world().eq(&bytes), "the file is unchanged");
        assert!(dir.backups().is_empty());
    }

    #[test]
    fn errors_are_damage_only_for_damage() {
        assert!(Error::Checksum.is_damage());
        assert!(Error::Database(redb::Error::Corrupted(String::new())).is_damage());
        assert!(!Error::InUse.is_damage());
        assert!(!Error::NewerFormat { version: 2 }.is_damage());
        assert!(!Error::Io(io::Error::other("x")).is_damage());
        // At start they become the crate's errors.
        assert!(matches!(
            crate::Error::from(Error::Checksum),
            crate::Error::WorldDamaged(_)
        ));
        assert!(matches!(
            crate::Error::from(Error::NewerFormat { version: 9 }),
            crate::Error::WorldNewerFormat {
                version: 9,
                supported: FORMAT_VERSION
            }
        ));
        assert!(matches!(
            crate::Error::from(Error::InUse),
            crate::Error::Storage(_)
        ));
    }

    /// Wall-clock milliseconds since `start`.
    fn ms(start: std::time::Instant) -> f64 {
        clock::now().duration_since(start).as_secs_f64() * 1e3
    }

    /// Measurements for PLAN.md: `cargo test --release --lib storage::tests::measure_world_file
    /// -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement"]
    #[expect(
        clippy::print_stdout,
        reason = "consistency: a measurement prints its results"
    )]
    fn measure_world_file() {
        let dir = TempDir::new("measure");
        let mut rng = Rng(77);
        let chunks: Vec<(Key, Vec<u8>)> = (0..10_000)
            .map(|i| {
                let key = chunk_key(i % 100, i / 100);
                let sealed = seal(&key.to_bytes(), &terrain_chunk(&mut rng)).unwrap();
                (key, sealed)
            })
            .collect();
        let bytes: usize = chunks.iter().map(|(_, v)| v.len()).sum();
        println!(
            "10,000 terrain chunks, {} bytes sealed on average",
            bytes / chunks.len()
        );
        let mut storage = Storage::open(&dir.0).unwrap();
        for count in [1_000, 10_000] {
            let mut batch = Batch::with_capacity(count);
            for (key, value) in &chunks[..count] {
                batch.put(key.clone(), value.clone());
            }
            let start = clock::now();
            storage.commit(&batch).unwrap();
            println!("commit of {count} chunks: {:.1} ms", ms(start));
        }
        let keys: Vec<Key> = chunks.iter().map(|(k, _)| k.clone()).collect();
        let start = clock::now();
        for batch in keys.chunks(64) {
            storage.read(batch).unwrap();
        }
        println!(
            "read in batches of 64 (warm): {:.2} µs per chunk",
            ms(start) * 1e3 / 10_000.0
        );
        println!("file after the first commits: {} bytes", dir.world_len());
        let mut sizes = Vec::new();
        for round in 0..50 {
            let mut batch = Batch::with_capacity(1_000);
            for (key, value) in &chunks[(round % 10) * 1_000..][..1_000] {
                batch.put(key.clone(), value.clone());
            }
            storage.commit(&batch).unwrap();
            if round % 10 == 9 {
                sizes.push(dir.world_len());
            }
        }
        println!("file after every 10 of 50 commits of 1,000 rewritten chunks: {sizes:?}");
        let start = clock::now();
        storage.close().unwrap();
        println!(
            "close (compact): {:.1} ms, file {} bytes",
            ms(start),
            dir.world_len()
        );
        let start = clock::now();
        let mut storage = Storage::open(&dir.0).unwrap();
        println!("open (backup, check, compact): {:.1} ms", ms(start));
        let start = clock::now();
        for batch in keys.chunks(64) {
            storage.read(batch).unwrap();
        }
        println!(
            "read in batches of 64 (after open): {:.2} µs per chunk",
            ms(start) * 1e3 / 10_000.0
        );
    }
}
