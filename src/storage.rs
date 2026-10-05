//! The server file `data/server.redb`: start policy, batched reads and commits (PLAN.md 10.6).
//!
//! Only the save thread, the world import before the server runs, and a replay ([`replay_files`],
//! [`Storage::open_replica`]) use this module; every call blocks on file I/O. Tables map raw keys
//! to sealed values ([`seal`](seal::seal)): `meta` (one [`Meta`]), `worlds` (world name →
//! [`WorldRecord`](codec::WorldRecord)), `chunks` ([`ChunkKey`] → the record its [`ChunkPart`]
//! names), `players` ([`PlayerKey`] → [`PlayerRecord`](codec::PlayerRecord)), `access`
//! ([`AccessKey`] → [`Mark`](codec::Mark) for operators and the whitelist,
//! [`BanRecord`](codec::BanRecord) for bans) and `user` (a server data name →
//! [`UserRecord`](codec::UserRecord)). Values are external input when read back: the seal's
//! checksum and the record decoders check them.
//!
//! Start ([`Storage::open`]): copy the closed file to `data/backups/` (with `backups = false`, a
//! copy for this start only, under its temporary name, and only when the file is checked), open
//! it (redb recovers a crash here), run `check_integrity()` as `integrity-check` says (with
//! `after-unclean-shutdown`, only when the file lacks the mark a normal stop leaves,
//! [`CLOSED_KEY`]), refuse a damaged file or a newer format, create the tables, and compact a
//! file that existed. A refused start writes the copy back, so the file is left exactly as it
//! was, and removes the copy. `check_integrity()` rewrites the file when it repairs it, and the
//! open itself writes the header, so the copy is what keeps the original bytes. A start that
//! fails for another reason keeps its copy as a backup and prunes to the [`backup::KEPT`] newest.
//! Only a refused start whose copy cannot be written back keeps it without pruning, so copies of
//! a damaged file never push out older good backups; the operator is asked to act then.
//!
//! Commits ([`Storage::commit`]) write a whole batch in one quick-repair write transaction. A
//! failure drops the handle, reopens the file and retries, at most [`RETRIES`] times. Values are
//! absolute, so a retry of a commit that did land writes the same state again. Batched reads
//! ([`Storage::read`]) retry the same way. The world import ([`Storage::begin_import`]) writes a
//! whole imported world in one such transaction, committed once.
//!
//! An online backup ([`Storage::begin_backup`]) copies the last commit from one read transaction
//! in steps, between which reads and commits go on; a reopen of the file stops it. A check
//! ([`Storage::begin_check`], `/check`) reads the last commit the same way.

mod backup;
mod check;
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
    WriteTransaction,
};
use tracing::{error, info, warn};

use crate::config::IntegrityCheck;
pub(crate) use check::Checked;
use codec::Meta;
use seal::{FORMAT_VERSION, unseal};

/// The server file in the data folder.
pub(crate) const SERVER_FILE: &str = "server.redb";
/// The name the server file had before it held more than worlds: a start renames it to
/// [`SERVER_FILE`] ([`rename_old_file`]).
const OLD_FILE: &str = "world.redb";
/// The backup folder in the data folder.
pub(crate) const BACKUP_DIR: &str = "backups";
/// Failed commits retried before [`Storage::commit`] gives up.
pub(crate) const RETRIES: usize = 3;
/// redb's page cache. Loaded chunks live in the world, and the OS caches the file, so this only
/// spares a system call for pages read again; redb's default of 1 GiB would mostly hold copies.
const CACHE_BYTES: usize = 64 << 20;
/// Most worlds [`Storage::worlds`] returns.
pub(crate) const MAX_WORLDS: usize = 1024;
/// Most `access` values [`Storage::access`] returns: every list full.
const MAX_ACCESS: usize = 4 * crate::permission::MAX_ENTRIES;

type Table = TableDefinition<'static, &'static [u8], &'static [u8]>;
const META: Table = TableDefinition::new("meta");
const WORLDS: Table = TableDefinition::new("worlds");
const CHUNKS: Table = TableDefinition::new("chunks");
const PLAYERS: Table = TableDefinition::new("players");
const ACCESS: Table = TableDefinition::new("access");
/// The user's server data (PLAN.md 10.8): one value per registered name. A file without the table
/// (before M8) reads as having no values; an older Anchor ignores it.
const USER: Table = TableDefinition::new("user");
/// The key of the `meta` value.
const META_KEY: &[u8] = b"meta";
/// The mark of a normal stop, in the `meta` table with an empty value: the last commit of
/// [`Storage::close`] writes it and the first commit of a start removes it, so it is there only
/// while the file is closed after a normal stop (`integrity-check = "after-unclean-shutdown"`).
const CLOSED_KEY: &[u8] = b"closed";

/// A failure of the server file or of one stored value.
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum Error {
    /// A file or folder under the data folder could not be read or written.
    Io(io::Error),
    /// Another process has the server file open.
    InUse,
    /// redb failed for a reason other than damage, such as I/O.
    Database(redb::Error),
    /// The server file failed redb's integrity check (or redb repaired it, which means it was
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
    /// [`Storage::access`] found more values than the lists hold.
    TooManyAccessEntries,
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
            // redb raises `InvalidData` on an open only for a file without its magic number and
            // for an empty file opened read-only (page_manager.rs); both are damage.
            DatabaseError::Storage(StorageError::Io(ref io))
                if io.kind() == io::ErrorKind::InvalidData =>
            {
                Self::DamagedFile(Some(e.into()))
            }
            e => Self::Database(e.into()),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => f.write_str("server file i/o failed"),
            Self::InUse => f.write_str("another process has the server file open"),
            Self::Database(_) => f.write_str("the world database failed"),
            Self::DamagedFile(_) => f.write_str("the server file failed its integrity check"),
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
            Self::TooManyAccessEntries => {
                write!(
                    f,
                    "more than {MAX_ACCESS} operator, ban and whitelist entries are stored"
                )
            }
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
            | Self::TooManyWorlds
            | Self::TooManyAccessEntries => None,
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

/// A kind of chunk extras (PLAN.md 10.7): the kind byte of its `chunks` keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ChunkKind {
    /// Block entities.
    BlockEntities = 1,
    /// Entities.
    Entities = 2,
    /// Scheduled block updates.
    ScheduledUpdates = 3,
}

impl ChunkKind {
    /// Every kind, in key order: a chunk's extras.
    pub(crate) const EXTRAS: [Self; 3] =
        [Self::BlockEntities, Self::Entities, Self::ScheduledUpdates];
}

/// The bytes a chunk's keys start with ([`ChunkId`]); the rest tell its [`ChunkPart`].
pub(crate) const CHUNK_PREFIX: usize = 12;

/// The kind byte of the sub-chunk values: `0x20 | index`.
const SUB_CHUNK_KIND: u8 = 0x20;

/// A chunk column of a persistent world: the start its `chunks` keys share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ChunkId {
    /// The world's number ([`WorldRecord::number`](codec::WorldRecord::number)).
    pub(crate) world: u32,
    /// The chunk's x coordinate.
    pub(crate) x: i32,
    /// The chunk's z coordinate.
    pub(crate) z: i32,
}

impl ChunkId {
    /// 12 bytes: world, x and z big-endian with the sign bit flipped (so keys sort by
    /// coordinate).
    fn prefix(self) -> [u8; CHUNK_PREFIX] {
        let flip = |c: i32| (c.cast_unsigned() ^ 0x8000_0000).to_be_bytes();
        let [w0, w1, w2, w3] = self.world.to_be_bytes();
        let [x0, x1, x2, x3] = flip(self.x);
        let [z0, z1, z2, z3] = flip(self.z);
        [w0, w1, w2, w3, x0, x1, x2, x3, z0, z1, z2, z3]
    }

    /// The key of this chunk's `part`.
    pub(crate) fn key(self, part: ChunkPart) -> Key {
        Key::Chunk(ChunkKey { chunk: self, part })
    }
}

/// What a `chunks` value holds, told by the bytes after its chunk's prefix: a kind byte, then
/// for an extras page after the first the page number (PLAN.md 6). Every value stays under
/// [`MAX_RECORD`](seal::MAX_RECORD): the terrain is one value per sub-chunk and an extras list
/// is split into pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ChunkPart {
    /// The whole terrain as formats 1 to 3 wrote it ([`ChunkRecord`](codec::ChunkRecord)),
    /// kind 0: read, and removed in the commit that writes the terrain by sub-chunk.
    Column,
    /// A page of an extras list ([`ExtraRecord`](codec::ExtraRecord)), kind 1 to 3: page 0
    /// has no more bytes (so the one value of formats 2 and 3 is page 0), a later page two more,
    /// big-endian.
    Extras(ChunkKind, u16),
    /// The terrain of one sub-chunk ([`SubChunkRecord`](codec::SubChunkRecord), format 4), index
    /// 0 (y -64) to 23: kind `0x20 | index`.
    SubChunk(u8),
}

impl ChunkPart {
    /// The part these key bytes after the chunk's prefix name; a key no Anchor writes is damage.
    fn from_suffix(bytes: &[u8]) -> Result<Self> {
        let kind = |k: u8| match k {
            1 => Some(ChunkKind::BlockEntities),
            2 => Some(ChunkKind::Entities),
            3 => Some(ChunkKind::ScheduledUpdates),
            _ => None,
        };
        let sub_chunk =
            |k: u8| k & !0x1f == SUB_CHUNK_KIND && usize::from(k & 0x1f) < codec::SUB_CHUNKS;
        let part = match *bytes {
            [0] => Some(Self::Column),
            [k] if sub_chunk(k) => Some(Self::SubChunk(k & 0x1f)),
            [k] => kind(k).map(|kind| Self::Extras(kind, 0)),
            [k, p0, p1] => {
                let page = u16::from_be_bytes([p0, p1]);
                kind(k)
                    .filter(|_| page != 0)
                    .map(|kind| Self::Extras(kind, page))
            }
            _ => None,
        };
        part.ok_or(Error::Malformed("a chunk key no anchor writes"))
    }
}

/// The key of a `chunks` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ChunkKey {
    /// The chunk.
    pub(crate) chunk: ChunkId,
    /// What the value holds.
    pub(crate) part: ChunkPart,
}

impl ChunkKey {
    /// 13 or 15 bytes: the chunk's prefix, the kind byte, and the page of a later extras page.
    pub(crate) fn to_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(15);
        out.extend_from_slice(&self.chunk.prefix());
        match self.part {
            ChunkPart::Column => out.push(0),
            ChunkPart::Extras(kind, page) => {
                out.push(kind as u8);
                if page != 0 {
                    out.extend_from_slice(&page.to_be_bytes());
                }
            }
            ChunkPart::SubChunk(index) => out.push(SUB_CHUNK_KIND | index),
        }
        out
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
    pub(crate) fn from_text(text: &str) -> Self {
        Self(text.to_owned())
    }
}

/// What an `access` value holds: the first byte of its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AccessKind {
    /// An operator ([`Mark`](codec::Mark)).
    Op = 0,
    /// A banned name ([`BanRecord`](codec::BanRecord)).
    Ban = 1,
    /// A banned IP address ([`BanRecord`](codec::BanRecord)).
    IpBan = 2,
    /// A whitelisted name ([`Mark`](codec::Mark)).
    Whitelist = 3,
}

/// The key of an `access` value: the kind, then the name in ASCII lowercase or the IP address as
/// text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AccessKey {
    pub(crate) kind: AccessKind,
    pub(crate) name: String,
}

impl AccessKey {
    /// The kind byte, then the name's bytes.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.name.len() + 1);
        out.push(self.kind as u8);
        out.extend_from_slice(self.name.as_bytes());
        out
    }

    /// The key these bytes encode; the name is not checked beyond UTF-8.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let (&kind, name) = bytes
            .split_first()
            .ok_or(Error::Malformed("empty access key"))?;
        let kind = match kind {
            0 => AccessKind::Op,
            1 => AccessKind::Ban,
            2 => AccessKind::IpBan,
            3 => AccessKind::Whitelist,
            _ => return Err(Error::Malformed("unknown access kind")),
        };
        let name = String::from_utf8(name.to_vec())
            .map_err(|_| Error::Malformed("access name is not utf-8"))?;
        Ok(Self { kind, name })
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
    /// An `access` value.
    Access(AccessKey),
    /// A `user` value, by the server data's name.
    User(String),
}

impl Key {
    /// The key's bytes in its table, which the value's seal also covers.
    pub(crate) fn to_bytes(&self) -> Cow<'_, [u8]> {
        match self {
            Self::Meta => Cow::Borrowed(META_KEY),
            Self::World(name) | Self::User(name) => Cow::Borrowed(name.as_bytes()),
            Self::Chunk(key) => Cow::Owned(key.to_bytes()),
            Self::Player(PlayerKey(key)) => Cow::Borrowed(key.as_bytes()),
            Self::Access(key) => Cow::Owned(key.to_bytes()),
        }
    }

    /// The key's table among `tables`, given in the order `meta`, `worlds`, `chunks`, `players`,
    /// `access`, `user`.
    fn pick<'a, T>(&self, tables: &'a [T; 6]) -> &'a T {
        let [meta, worlds, chunks, players, access, user] = tables;
        match self {
            Self::Meta => meta,
            Self::World(_) => worlds,
            Self::Chunk(_) => chunks,
            Self::Player(_) => players,
            Self::Access(_) => access,
            Self::User(_) => user,
        }
    }

    /// [`pick`](Self::pick) for writing.
    fn pick_mut<'a, T>(&self, tables: &'a mut [T; 6]) -> &'a mut T {
        let [meta, worlds, chunks, players, access, user] = tables;
        match self {
            Self::Meta => meta,
            Self::World(_) => worlds,
            Self::Chunk(_) => chunks,
            Self::Player(_) => players,
            Self::Access(_) => access,
            Self::User(_) => user,
        }
    }
}

/// Writes for one commit, applied in order: each key gets a sealed value or is deleted, and
/// the pages of a chunk's extras list can be deleted from one on. Later writes to a key replace
/// earlier ones.
#[derive(Debug, Default)]
pub(crate) struct Batch {
    writes: Vec<Write>,
}

/// One write of a [`Batch`].
#[derive(Debug)]
enum Write {
    Put(Key, Vec<u8>),
    Delete(Key),
    /// Every page of the chunk's list of the kind, from the page on.
    DeletePages(ChunkId, ChunkKind, u16),
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
        self.writes.push(Write::Put(key, sealed));
    }

    /// Deletes the value under `key`, if any.
    pub(crate) fn delete(&mut self, key: Key) {
        self.writes.push(Write::Delete(key));
    }

    /// Deletes the pages of `chunk`'s `kind` list from `first` on, whichever there are: the
    /// pages a shorter list no longer fills.
    pub(crate) fn delete_pages(&mut self, chunk: ChunkId, kind: ChunkKind, first: u16) {
        self.writes.push(Write::DeletePages(chunk, kind, first));
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

/// Where the database lives: the server file, or a test's simulated disk.
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
        reason = "off-tick: the save thread opens the server file"
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

/// Whether [`Storage::open`] first copies the closed server file into the backups (`backups`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartBackup {
    /// Copy it, and keep the [`backup::KEPT`] newest backups.
    Make,
    /// Copy it for this start only (`backups = false`): a refused file is still put back as it
    /// was, and the copy is removed once the file opened, so no backup is kept. The copy keeps
    /// its temporary name, so one a killed start left is removed by the next start; one a failed
    /// start keeps (a refused file it could not put back, or another error) gets its backup name
    /// and counts among the [`backup::KEPT`] newest.
    Transient,
}

/// How [`Storage::open`] starts on the server file: the `backups` and `integrity-check` settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Start {
    pub(crate) backup: StartBackup,
    pub(crate) check: IntegrityCheck,
}

impl Start {
    /// The start the settings ask for: every open of the server file (the server's and the world
    /// import's) uses it.
    pub(crate) fn of(config: &crate::config::Config) -> Self {
        Self {
            backup: if config.general.backups {
                StartBackup::Make
            } else {
                StartBackup::Transient
            },
            check: config.developer.integrity_check,
        }
    }

    /// `backup` with a check, as the tests open server files.
    #[cfg(test)]
    pub(crate) const fn checked(backup: StartBackup) -> Self {
        Self {
            backup,
            check: IntegrityCheck::Always,
        }
    }
}

/// The open server file. Dropping it closes the file without compacting it.
#[derive(Debug)]
pub(crate) struct Storage {
    /// The online backup being made ([`Storage::begin_backup`]). Declared first: it holds a read
    /// transaction of `db`, so it is dropped first.
    backup: Option<backup::Online>,
    /// The check being made ([`Storage::begin_check`]); it holds a read transaction of `db` too.
    check: Option<check::Check>,
    /// `None` after a failed commit could not reopen the file; the next call tries again.
    db: Option<Database>,
    source: Source,
    /// A replay's read-only file and what its commits wrote ([`Storage::open_replica`]); every
    /// call goes there instead.
    replica: Option<Box<Replica>>,
}

/// A server file opened read-only for a replay (PLAN.md 10.9 재생 도구): commits go to memory on
/// top of the file, which is never written, and reads see them as they would see a commit.
struct Replica {
    db: redb::ReadOnlyDatabase,
    /// The values committed since the file was opened, by table in [`Key::pick`] order and key
    /// bytes; `None` is a deleted value.
    overlay: [std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>>; 6],
}

impl fmt::Debug for Replica {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Replica")
            .field(
                "written",
                &self
                    .overlay
                    .iter()
                    .map(std::collections::BTreeMap::len)
                    .sum::<usize>(),
            )
            .finish_non_exhaustive()
    }
}

/// The tables in [`Key::pick`] order.
const TABLES: [Table; 6] = [META, WORLDS, CHUNKS, PLAYERS, ACCESS, USER];

impl Replica {
    /// Reads `keys` as [`Storage::read`] does, committed values first.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a replay reads the server file on its own thread"
    )]
    fn read(&self, keys: &[Key]) -> Result<Vec<Result<Option<Vec<u8>>>>> {
        let txn = self.db.begin_read()?;
        let [meta, worlds, chunks, players, access, user] = TABLES;
        let tables = [
            open_read(&txn, meta)?,
            open_read(&txn, worlds)?,
            open_read(&txn, chunks)?,
            open_read(&txn, players)?,
            open_read(&txn, access)?,
            open_read(&txn, user)?,
        ];
        let mut values = Vec::with_capacity(keys.len());
        for key in keys {
            let bytes = key.to_bytes();
            if let Some(value) = key.pick(&self.overlay).get(&*bytes) {
                values.push(Ok(value.clone()));
                continue;
            }
            let value = match key.pick(&tables) {
                Some(table) => match table.get(&*bytes) {
                    Ok(value) => Ok(value.map(|v| v.value().to_vec())),
                    Err(e) => Err(Error::from(e)),
                },
                None => Ok(None),
            };
            match value {
                Err(e) if !e.is_damage() => return Err(e),
                value => values.push(value),
            }
        }
        Ok(values)
    }

    /// Every value of each of `chunks` as [`Storage::read_chunks`] gives them, committed values
    /// in their place.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a replay reads the server file on its own thread"
    )]
    fn read_chunks(&self, chunks: &[ChunkId]) -> Result<Vec<ChunkValues>> {
        let txn = self.db.begin_read()?;
        let table = open_read(&txn, CHUNKS)?;
        let [_, _, written, _, _, _] = &self.overlay;
        let mut out = Vec::with_capacity(chunks.len());
        for &chunk in chunks {
            let stored = match &table {
                Some(table) => read_chunk(table, chunk),
                None => Ok(Vec::new()),
            };
            let stored = match stored {
                Ok(stored) => stored,
                Err(e) if e.is_damage() => {
                    out.push(Err(e));
                    continue;
                }
                Err(e) => return Err(e),
            };
            let mut values: std::collections::BTreeMap<Vec<u8>, Vec<u8>> = stored
                .into_iter()
                .map(|(part, value)| (ChunkKey { chunk, part }.to_bytes(), value))
                .collect();
            let (first, last) = chunk_range(chunk);
            for (key, value) in written.range(first..=last) {
                match value {
                    Some(v) => values.insert(key.clone(), v.clone()),
                    None => values.remove(key),
                };
            }
            out.push(
                values
                    .into_iter()
                    .map(|(key, value)| {
                        let part =
                            ChunkPart::from_suffix(key.get(CHUNK_PREFIX..).unwrap_or_default())?;
                        Ok((part, value))
                    })
                    .collect(),
            );
        }
        Ok(out)
    }

    /// Every value of `table` (at most `max`), committed values in their place, in key order.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a replay reads the server file on its own thread"
    )]
    fn all(&self, table: Table, written: usize, max: usize) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let txn = self.db.begin_read()?;
        let mut values = std::collections::BTreeMap::new();
        if let Some(table) = open_read(&txn, table)? {
            for entry in table.iter()?.take(max.saturating_add(1)) {
                let (key, value) = entry?;
                values.insert(key.value().to_vec(), value.value().to_vec());
            }
        }
        if let Some(written) = self.overlay.get(written) {
            for (key, value) in written {
                match value {
                    Some(v) => values.insert(key.clone(), v.clone()),
                    None => values.remove(key),
                };
            }
        }
        Ok(values.into_iter().collect())
    }

    /// Applies `batch` to the values in memory, as [`write()`] does to the file.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a replay reads the server file on its own thread"
    )]
    fn commit(&mut self, batch: &Batch) -> Result<()> {
        for write in &batch.writes {
            match write {
                Write::Put(key, value) => {
                    key.pick_mut(&mut self.overlay)
                        .insert(key.to_bytes().into_owned(), Some(value.clone()));
                }
                Write::Delete(key) => {
                    key.pick_mut(&mut self.overlay)
                        .insert(key.to_bytes().into_owned(), None);
                }
                &Write::DeletePages(chunk, kind, first) => {
                    let first = chunk
                        .key(ChunkPart::Extras(kind, first))
                        .to_bytes()
                        .into_owned();
                    let last = ChunkKey {
                        chunk,
                        part: ChunkPart::Extras(kind, u16::MAX),
                    }
                    .to_bytes();
                    let txn = self.db.begin_read()?;
                    let [_, _, written, _, _, _] = &mut self.overlay;
                    if let Some(table) = open_read(&txn, CHUNKS)? {
                        for entry in table.range::<&[u8]>(&*first..=&*last)? {
                            let (key, _) = entry?;
                            written.insert(key.value().to_vec(), None);
                        }
                    }
                    for value in written.range_mut(first..=last) {
                        *value.1 = None;
                    }
                }
            }
        }
        Ok(())
    }
}

/// A `worlds` key as a world name.
fn world_name(key: Vec<u8>) -> Result<String> {
    String::from_utf8(key).map_err(|_| Error::Malformed("world name is not utf-8"))
}

/// The first and last key bytes of `chunk`'s values.
fn chunk_range(chunk: ChunkId) -> (Vec<u8>, Vec<u8>) {
    let prefix = chunk.prefix();
    let mut first = prefix.to_vec();
    first.push(0);
    let mut last = prefix.to_vec();
    last.extend_from_slice(&[0xff; 3]);
    (first, last)
}

/// The path of `copy`, the start copy a failed [`Storage::open`] keeps: a
/// [`StartBackup::Transient`] copy gets its backup name ([`backup::keep`]).
fn kept(copy: PathBuf, start: StartBackup) -> PathBuf {
    match start {
        StartBackup::Make => copy,
        StartBackup::Transient => backup::keep(&copy).unwrap_or_else(|e| {
            warn!(
                backup = %copy.display(),
                error = %e,
                "could not give the kept start copy its backup name; the next start removes it"
            );
            copy
        }),
    }
}

/// What a failed [`Storage::open`] does with `copy`, its start copy of `world`, after `error`.
/// After damage or a newer format it puts the copy back and removes it, and keeps it only if that
/// fails, without pruning: copies of a damaged file must not push out older good backups. After
/// any other error it keeps the copy as a backup with the [`backup::KEPT`] newest.
fn failed_start(world: &Path, backups: &Path, copy: PathBuf, start: StartBackup, error: &Error) {
    if error.is_damage() || matches!(error, Error::NewerFormat { .. }) {
        // The open and the check may have written to the file: put back the bytes it had, so
        // the operator finds it as it was.
        if let Err(restore) = backup::restore(&copy, world) {
            let copy = kept(copy, start);
            error!(
                backup = %copy.display(),
                error = %restore,
                "could not put the server file back; the backup holds its bytes"
            );
        }
    } else {
        let copy = kept(copy, start);
        if let Err(e) = backup::prune(backups, &copy) {
            warn!(error = %e, "could not remove old backups");
        }
        warn!(
            backup = %copy.display(),
            error = %error,
            "server file did not open; its backup is kept"
        );
    }
}

/// The server files a replay may start from, in the order it tries them: `data/server.redb`, then
/// the backups newest first (PLAN.md 10.9 재생 도구). Missing ones are left out.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: a replay lists the server files before it starts"
)]
pub(crate) fn replay_files(data_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let world = data_dir.join(SERVER_FILE);
    if std::fs::exists(&world).map_err(Error::Io)? {
        files.push(world);
    }
    let mut backups = backup::backups(&data_dir.join(BACKUP_DIR))?;
    backups.sort_unstable();
    files.extend(backups.into_iter().rev().map(|(_, path)| path));
    Ok(files)
}

impl Storage {
    /// Opens `path` read-only for a replay: nothing is checked, compacted or backed up, and
    /// commits stay in memory ([`Replica`]). Fails if the file is missing, in use or damaged.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a replay opens a server file read-only before it starts"
    )]
    pub(crate) fn open_replica(path: &Path) -> Result<Self> {
        let mut builder = Builder::new();
        builder.set_cache_size(CACHE_BYTES);
        let db = builder.open_read_only(path).map_err(Error::from_open)?;
        Ok(Self {
            backup: None,
            check: None,
            db: None,
            source: Source::File(path.to_path_buf()),
            replica: Some(Box::new(Replica {
                db,
                overlay: Default::default(),
            })),
        })
    }

    /// Starts on `<data_dir>/server.redb` (PLAN.md 10.6), first renaming an older Anchor's
    /// `world.redb` to it ([`rename_old_file`]): backs up the closed file, opens it,
    /// checks it as `start.check` says, refuses a damaged file or a newer format (leaving the file as
    /// it was), compacts it, and keeps the [`backup::KEPT`] newest backups, or none with
    /// [`StartBackup::Transient`]. A missing or empty file starts empty, without a compaction.
    ///
    /// The closed file is copied for a start backup ([`StartBackup::Make`]) and for a check,
    /// whose copy puts the file back when the check refuses it ([`StartBackup::Transient`]). A
    /// start without either makes no copy: a file refused for a newer format is then left as the
    /// open wrote it (redb's header), not byte for byte.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread creates the data folder and compacts the file at start"
    )]
    pub(crate) fn open(data_dir: &Path, start: Start) -> Result<Self> {
        let Start { backup, check } = start;
        let world = data_dir.join(SERVER_FILE);
        let backups = data_dir.join(BACKUP_DIR);
        std::fs::create_dir_all(data_dir).map_err(Error::Io)?;
        rename_old_file(data_dir)?;
        let checked = checks(&world, check);
        // The lock ends with the arm, before the file is opened as a database.
        let (existed, copy) = match backup::lock(&world, &backups)? {
            Some(mut source) => {
                let copy = if checked || backup == StartBackup::Make {
                    Some(backup::copy(&mut source, &backups, backup)?)
                } else {
                    None
                };
                (true, copy)
            }
            None => (false, None),
        };
        if existed && !checked {
            info!(?check, "server file opened without its integrity check");
        }
        let source = Source::File(world.clone());
        let started = source
            .open()
            .and_then(|mut db| {
                if checked {
                    check_integrity(&mut db)?;
                }
                Self::prepare(source, db)
            })
            .and_then(|mut storage| {
                // A new file has nothing to free; compacting it only costs its syncs.
                if existed && let Some(db) = &mut storage.db {
                    db.compact()?;
                }
                Ok(storage)
            });
        match started {
            Ok(storage) => {
                match (&copy, backup) {
                    (Some(copy), StartBackup::Make) => {
                        if let Err(e) = backup::prune(&backups, copy) {
                            warn!(error = %e, "could not remove old backups");
                        }
                        info!(backup = %copy.display(), "server file opened");
                    }
                    (Some(copy), StartBackup::Transient) => {
                        if let Err(e) = std::fs::remove_file(copy) {
                            warn!(
                                backup = %copy.display(),
                                error = %e,
                                "could not remove the start copy of the server file"
                            );
                        }
                        info!("server file opened");
                    }
                    (None, _) if existed => info!("server file opened"),
                    (None, _) => info!("server file created"),
                }
                Ok(storage)
            }
            Err(e) => {
                if let Some(copy) = copy {
                    failed_start(&world, &backups, copy, backup, &e);
                }
                Err(e)
            }
        }
    }

    /// Opens, checks and prepares the database at `source`, as a start that checks it.
    #[cfg(test)]
    fn start(source: Source) -> Result<Self> {
        let mut db = source.open()?;
        check_integrity(&mut db)?;
        Self::prepare(source, db)
    }

    /// Prepares `db`, the database just opened at `source`: refuses a newer format, then its
    /// first commit creates the tables a new file lacks and removes the mark of a normal stop
    /// ([`CLOSED_KEY`]), so a stop that does not reach [`close`](Self::close) leaves none.
    fn prepare(source: Source, db: Database) -> Result<Self> {
        let mut storage = Self {
            backup: None,
            check: None,
            db: Some(db),
            source,
            replica: None,
        };
        storage.check_format()?;
        storage.retrying(begin_run)?;
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
    /// [`unseal`] under [`Key::to_bytes`]), `None` if there is none, or the damage redb found for
    /// that key alone. A read that fails otherwise is retried as a commit is, then returns the
    /// last error.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread reads the server file"
    )]
    pub(crate) fn read(&mut self, keys: &[Key]) -> Result<Vec<Result<Option<Vec<u8>>>>> {
        if let Some(replica) = &self.replica {
            return replica.read(keys);
        }
        self.retrying(|db| {
            let txn = db.begin_read()?;
            let tables = [
                open_read(&txn, META)?,
                open_read(&txn, WORLDS)?,
                open_read(&txn, CHUNKS)?,
                open_read(&txn, PLAYERS)?,
                open_read(&txn, ACCESS)?,
                open_read(&txn, USER)?,
            ];
            let mut values = Vec::with_capacity(keys.len());
            for key in keys {
                let value = match key.pick(&tables) {
                    Some(table) => match table.get(&*key.to_bytes()) {
                        Ok(value) => Ok(value.map(|v| v.value().to_vec())),
                        Err(e) => Err(Error::from(e)),
                    },
                    None => Ok(None),
                };
                // Damage stays with its key; any other failure (I/O) is the read's, retried.
                match value {
                    Err(e) if !e.is_damage() => return Err(e),
                    value => values.push(value),
                }
            }
            Ok(values)
        })
    }

    /// Every value of each of `chunks`, in key order, read in one read transaction: its part
    /// and sealed value (open it with [`unseal`] under its key's bytes), or the damage found in
    /// that chunk alone (a key no Anchor writes, or a page redb found corrupted). The key format
    /// bounds the values of a chunk: one whole terrain, 24 sub-chunks and 65,536 pages of each
    /// extras kind. A read that fails otherwise is retried as a commit is, then returns the last
    /// error.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread reads the server file"
    )]
    pub(crate) fn read_chunks(&mut self, chunks: &[ChunkId]) -> Result<Vec<ChunkValues>> {
        if let Some(replica) = &self.replica {
            return replica.read_chunks(chunks);
        }
        self.retrying(|db| {
            let txn = db.begin_read()?;
            let table = open_read(&txn, CHUNKS)?;
            let mut out = Vec::with_capacity(chunks.len());
            for &chunk in chunks {
                let values = match &table {
                    Some(table) => read_chunk(table, chunk),
                    None => Ok(Vec::new()),
                };
                // Damage stays with its chunk; any other failure (I/O) is the read's, retried.
                match values {
                    Err(e) if !e.is_damage() => return Err(e),
                    values => out.push(values),
                }
            }
            Ok(out)
        })
    }

    /// Every stored world: its name and sealed [`WorldRecord`](codec::WorldRecord), in name
    /// order.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread reads the server file"
    )]
    pub(crate) fn worlds(&mut self) -> Result<Vec<(String, Vec<u8>)>> {
        if let Some(replica) = &self.replica {
            let entries = replica.all(WORLDS, 1, MAX_WORLDS)?;
            if entries.len() > MAX_WORLDS {
                return Err(Error::TooManyWorlds);
            }
            return entries
                .into_iter()
                .map(|(name, sealed)| Ok((world_name(name)?, sealed)))
                .collect();
        }
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
            worlds.push((world_name(name.value().to_vec())?, sealed.value().to_vec()));
        }
        Ok(worlds)
    }

    /// Every stored operator, ban and whitelist entry: its key and sealed value, in key order.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread reads the server file"
    )]
    pub(crate) fn access(&mut self) -> Result<Vec<(AccessKey, Vec<u8>)>> {
        if let Some(replica) = &self.replica {
            let entries = replica.all(ACCESS, 4, MAX_ACCESS)?;
            if entries.len() > MAX_ACCESS {
                return Err(Error::TooManyAccessEntries);
            }
            return entries
                .into_iter()
                .map(|(key, sealed)| Ok((AccessKey::from_bytes(&key)?, sealed)))
                .collect();
        }
        let txn = self.database()?.begin_read()?;
        let Some(table) = open_read(&txn, ACCESS)? else {
            return Ok(Vec::new());
        };
        let count = usize::try_from(table.len()?).unwrap_or(usize::MAX);
        if count > MAX_ACCESS {
            return Err(Error::TooManyAccessEntries);
        }
        let mut entries = Vec::with_capacity(count);
        for entry in table.iter()?.take(MAX_ACCESS) {
            let (key, sealed) = entry?;
            entries.push((AccessKey::from_bytes(key.value())?, sealed.value().to_vec()));
        }
        Ok(entries)
    }

    /// Writes `batch` in one durable write transaction with quick repair. On failure it drops
    /// the handle, reopens the file and tries again, up to [`RETRIES`] times, then returns the
    /// last error. Either the whole batch is committed or none of it.
    pub(crate) fn commit(&mut self, batch: &Batch) -> Result<()> {
        if let Some(replica) = &mut self.replica {
            return replica.commit(batch);
        }
        self.retrying(|db| write(db, batch))
    }

    /// Starts the world import's write (PLAN.md 10.9): one quick-repair write transaction that
    /// takes the import's batches as they come and commits once ([`Import::commit`]). Dropping
    /// the [`Import`] uncommitted, or any failure, leaves the file as it was. No retry: the
    /// import's writes are not kept to write again, and a failed import is run again whole.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the import writes the server file before the server runs"
    )]
    pub(crate) fn begin_import(&mut self) -> Result<Import> {
        let mut txn = self.database()?.begin_write()?;
        txn.set_quick_repair(true);
        Ok(Import(txn))
    }

    /// Starts an online backup of the last commit into `dir` (PLAN.md 10.9 백업), made in steps
    /// by [`backup_step`](Self::backup_step) while reads and commits go on. False, starting
    /// nothing, while another is being made.
    pub(crate) fn begin_backup(&mut self, dir: &Path) -> Result<bool> {
        if self.backup.is_some() {
            return Ok(false);
        }
        let online = backup::Online::begin(self.database()?, dir)?;
        self.backup = Some(online);
        Ok(true)
    }

    /// Whether an online backup is being made.
    pub(crate) fn is_backing_up(&self) -> bool {
        self.backup.is_some()
    }

    /// Takes the next step of the online backup being made, if any: a copy step, or once all is
    /// copied the last step that finishes it; returns the backup's path once it is whole. A
    /// failure ends the backup and leaves no file of it.
    pub(crate) fn backup_step(&mut self) -> Result<Option<PathBuf>> {
        let Some(mut online) = self.backup.take() else {
            return Ok(None);
        };
        if online.is_copied() {
            return online.finish().map(Some);
        }
        online.step()?;
        self.backup = Some(online);
        Ok(None)
    }

    /// Starts a check of the last commit (`/check`), made in steps by
    /// [`check_step`](Self::check_step) while reads and commits go on. False, starting nothing,
    /// while another is being made.
    pub(crate) fn begin_check(&mut self) -> Result<bool> {
        if self.check.is_some() {
            return Ok(false);
        }
        let check = check::Check::begin(self.database()?)?;
        self.check = Some(check);
        Ok(true)
    }

    /// Whether a check is being made.
    pub(crate) fn is_checking(&self) -> bool {
        self.check.is_some()
    }

    /// Takes the next step of the check being made, if any; returns what it found once it ends.
    pub(crate) fn check_step(&mut self) -> Option<Checked> {
        let found = self.check.as_mut()?.step();
        if found.is_some() {
            self.check = None;
        }
        found
    }

    /// Runs `call` on the open database. On failure it drops the handle, reopens the file and
    /// tries again, up to [`RETRIES`] times, then returns the last error. An online backup stops
    /// first, and a check ends: their read transactions would keep the old handle, and the file,
    /// open.
    fn retrying<T>(&mut self, call: impl Fn(&Database) -> Result<T>) -> Result<T> {
        let mut failures = 0;
        loop {
            match self.database().and_then(&call) {
                Ok(value) => return Ok(value),
                Err(e) if failures < RETRIES => {
                    failures += 1;
                    warn!(error = %e, retry = failures, "server file call failed; reopening the file");
                    if let Some(backup) = self.backup.take() {
                        error!(?backup, "backup stopped: the server file is reopened");
                    }
                    if let Some(check) = &mut self.check {
                        check.stop("the server file was reopened after a failure".to_owned());
                    }
                    self.db = None;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Compacts the file after the last commit, marks it closed after a normal stop
    /// ([`CLOSED_KEY`]) and closes it. Only a normal stop calls it: a failed one drops the
    /// [`Storage`] instead, so its file has no mark.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread compacts the server file at shutdown"
    )]
    pub(crate) fn close(mut self) -> Result<()> {
        // Its read transaction would keep the compaction waiting; nobody waits for its result.
        self.check = None;
        if let Some(mut db) = self.db.take() {
            db.compact()?;
            mark_closed(&db)?;
        }
        Ok(())
    }
}

/// The world import's open write transaction ([`Storage::begin_import`]). Dropping it uncommitted
/// aborts every write it took.
pub(crate) struct Import(WriteTransaction);

impl fmt::Debug for Import {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Import").finish_non_exhaustive()
    }
}

impl Import {
    /// Applies `batch`'s writes, in order, without committing them.
    pub(crate) fn write(&mut self, batch: &Batch) -> Result<()> {
        apply(&self.0, batch)
    }

    /// Commits every write taken, durably and at once.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the import writes the server file before the server runs"
    )]
    pub(crate) fn commit(self) -> Result<()> {
        self.0.commit()?;
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

/// What [`Storage::read_chunks`] gives for one chunk.
pub(crate) type ChunkValues = Result<Vec<(ChunkPart, Vec<u8>)>>;

/// The values of one chunk, in key order.
fn read_chunk(table: &ReadOnlyTable<&'static [u8], &'static [u8]>, chunk: ChunkId) -> ChunkValues {
    let prefix = chunk.prefix();
    let mut first = prefix.to_vec();
    first.push(0);
    let mut last = prefix.to_vec();
    last.extend_from_slice(&[0xff; 3]);
    let mut values = Vec::new();
    for entry in table.range::<&[u8]>(&*first..=&*last)? {
        let (key, value) = entry?;
        let part = ChunkPart::from_suffix(key.value().get(CHUNK_PREFIX..).unwrap_or_default())?;
        values.push((part, value.value().to_vec()));
    }
    Ok(values)
}

/// One write transaction holding all of `batch`.
/// redb's integrity check of the file just opened: refuses a damaged file, and one redb repaired
/// (which was damaged).
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread checks the server file at start"
)]
fn check_integrity(db: &mut Database) -> Result<()> {
    #[cfg(test)]
    tests::STARTS.set((tests::STARTS.get().0 + 1, tests::STARTS.get().1));
    match db.check_integrity() {
        Ok(true) => Ok(()),
        // redb repaired the file in place: it was damaged.
        Ok(false) => Err(Error::DamagedFile(None)),
        Err(e @ DatabaseError::Storage(StorageError::Corrupted(_))) => {
            Err(Error::DamagedFile(Some(e.into())))
        }
        Err(e) => Err(e.into()),
    }
}

/// Whether a start checks the closed server file `world` when `integrity-check` is `check`.
fn checks(world: &Path, check: IntegrityCheck) -> bool {
    match check {
        IntegrityCheck::Always => true,
        IntegrityCheck::AfterUncleanShutdown => !closed_normally(world),
        IntegrityCheck::Never => false,
    }
}

/// Renames the server file of an older Anchor, [`OLD_FILE`] in `data_dir`, to [`SERVER_FILE`]
/// when there is no [`SERVER_FILE`] yet. The rename is one atomic file system call, so the file
/// is under one of the two names whatever happens; a rename that fails fails the start and leaves
/// the file as it was. Refuses a file another process has open ([`Error::InUse`]): an older
/// Anchor still running keeps it. With both files there, the old one is left alone.
#[expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "off-tick: the save thread renames the server file once, before opening it"
)]
fn rename_old_file(data_dir: &Path) -> Result<()> {
    let (old, new) = (data_dir.join(OLD_FILE), data_dir.join(SERVER_FILE));
    if std::fs::exists(&new).map_err(Error::Io)? {
        if std::fs::exists(&old).map_err(Error::Io)? {
            warn!(file = %old.display(), "an old server file is left beside the server file");
        }
        return Ok(());
    }
    let file = match std::fs::File::open(&old) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Err(Error::InUse),
        Err(std::fs::TryLockError::Error(e)) => return Err(Error::Io(e)),
    }
    // Windows cannot rename a file that is still open.
    drop(file);
    std::fs::rename(&old, &new).map_err(Error::Io)?;
    info!(from = %old.display(), to = %new.display(), "server file renamed");
    Ok(())
}

/// Whether the closed server file `world` has the mark of a normal stop ([`CLOSED_KEY`]), read
/// without writing to it. A file that cannot be opened read-only (missing, damaged, in use, or
/// left for redb to repair) has none: the start then checks it, and the check meets the fault.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread reads the server file's mark once, before opening it"
)]
pub(crate) fn closed_normally(world: &Path) -> bool {
    let mut builder = Builder::new();
    builder.set_cache_size(CACHE_BYTES);
    let Ok(db) = builder.open_read_only(world) else {
        return false;
    };
    let Ok(txn) = db.begin_read() else {
        return false;
    };
    let Ok(meta) = txn.open_table(META) else {
        return false;
    };
    matches!(meta.get(CLOSED_KEY), Ok(Some(_)))
}

/// The first commit of a start: creates the tables a new file lacks and removes the mark of a
/// normal stop ([`CLOSED_KEY`]).
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread writes the server file at start"
)]
fn begin_run(db: &Database) -> Result<()> {
    let mut txn = db.begin_write()?;
    txn.set_quick_repair(true);
    apply(&txn, &Batch::default())?;
    drop(txn.open_table(META)?.remove(CLOSED_KEY)?);
    txn.commit()?;
    Ok(())
}

/// The last commit of a normal stop: writes the mark of a normal stop ([`CLOSED_KEY`]).
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread writes the server file at a normal stop"
)]
fn mark_closed(db: &Database) -> Result<()> {
    let mut txn = db.begin_write()?;
    txn.set_quick_repair(true);
    drop(txn.open_table(META)?.insert(CLOSED_KEY, &[][..])?);
    txn.commit()?;
    Ok(())
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread writes the server file"
)]
fn write(db: &Database, batch: &Batch) -> Result<()> {
    let mut txn = db.begin_write()?;
    txn.set_quick_repair(true);
    apply(&txn, batch)?;
    txn.commit()?;
    Ok(())
}

/// Applies `batch`'s writes, in order, to an open write transaction.
fn apply(txn: &WriteTransaction, batch: &Batch) -> Result<()> {
    let mut tables = [
        txn.open_table(META)?,
        txn.open_table(WORLDS)?,
        txn.open_table(CHUNKS)?,
        txn.open_table(PLAYERS)?,
        txn.open_table(ACCESS)?,
        txn.open_table(USER)?,
    ];
    for write in &batch.writes {
        match write {
            Write::Put(key, value) => {
                let table = key.pick_mut(&mut tables);
                drop(table.insert(&*key.to_bytes(), value.as_slice())?);
            }
            Write::Delete(key) => {
                let table = key.pick_mut(&mut tables);
                drop(table.remove(&*key.to_bytes())?);
            }
            &Write::DeletePages(chunk, kind, first) => {
                let key = chunk.key(ChunkPart::Extras(kind, first));
                let first = key.to_bytes();
                let last = ChunkKey {
                    chunk,
                    part: ChunkPart::Extras(kind, u16::MAX),
                }
                .to_bytes();
                key.pick_mut(&mut tables)
                    .retain_in::<&[u8], _>(&*first..=&*last, |_, _| false)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use redb::StorageBackend;

    use super::codec::tests::{empty_chunk, random_chunk, random_compound, terrain_chunk};
    use super::codec::{
        BanRecord, ChunkRecord, ExtraRecord, Mark, PlayerRecord, SubChunkRecord, UserRecord,
        WorldRecord,
    };
    use super::seal::seal;
    use super::*;
    use crate::clock;
    use crate::protocol::Rng;

    thread_local! {
        /// The integrity checks and start copies this thread's starts made, in that order.
        pub(crate) static STARTS: std::cell::Cell<(u32, u32)> = const { std::cell::Cell::new((0, 0)) };
    }

    /// The redb page size: the unit a torn write keeps or loses.
    const PAGE: usize = 4096;

    /// A backend call the simulated disk counts.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Call {
        Write = 0,
        SetLen = 1,
        Sync = 2,
        Read = 3,
    }

    /// When the simulated disk fails calls.
    #[derive(Debug, Clone, Copy, Default)]
    pub(crate) enum Fault {
        #[default]
        None,
        /// The `at`-th call of kind `call` from now fails; the rest succeed.
        Once { call: Call, at: u64 },
        /// Every write, length change or sync from the `at`-th on fails: the process died there
        /// (or the disk is gone).
        From { at: u64 },
        /// Every call of kind `call` fails.
        Every { call: Call },
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
        calls: [u64; 4],
        fault: Fault,
        /// Calls that failed.
        failed: u64,
    }

    impl State {
        /// Counts a call and tells whether it fails.
        fn fails(&mut self, call: Call) -> bool {
            self.calls[call as usize] += 1;
            let count = self.calls[call as usize];
            let writes = [Call::Write, Call::SetLen, Call::Sync];
            let total: u64 = writes.iter().map(|&c| self.calls[c as usize]).sum();
            let fails = match self.fault {
                Fault::None => false,
                Fault::Once { call: kind, at } => {
                    let hit = kind == call && count == at;
                    if hit {
                        self.fault = Fault::None;
                    }
                    hit
                }
                Fault::From { at } => call != Call::Read && total >= at,
                Fault::Every { call: kind } => kind == call,
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
            state.calls = [0; 4];
        }

        pub(crate) fn failed(&self) -> u64 {
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
            let mut state = self.0.0.lock().unwrap();
            if state.fails(Call::Read) {
                return Err(injected());
            }
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

    pub(crate) fn memory(disk: &Disk) -> Storage {
        Storage::start(Source::Memory(disk.clone())).unwrap()
    }

    /// The database on `disk` as a failed commit leaves it: closed, so the next call opens it
    /// again and reads its pages from the disk.
    pub(crate) fn closed(disk: &Disk) -> Storage {
        Storage {
            backup: None,
            check: None,
            db: None,
            source: Source::Memory(disk.clone()),
            replica: None,
        }
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
            Key::Chunk(ChunkKey { part, .. }) => match part {
                ChunkPart::Column => drop(unseal::<ChunkRecord>(&bytes, sealed).unwrap()),
                ChunkPart::SubChunk(_) => drop(unseal::<SubChunkRecord>(&bytes, sealed).unwrap()),
                ChunkPart::Extras(..) => drop(unseal::<ExtraRecord<'_>>(&bytes, sealed).unwrap()),
            },
            Key::Player(_) => drop(unseal::<PlayerRecord>(&bytes, sealed).unwrap()),
            Key::Access(AccessKey {
                kind: AccessKind::Ban | AccessKind::IpBan,
                ..
            }) => drop(unseal::<BanRecord>(&bytes, sealed).unwrap()),
            Key::Access(_) => drop(unseal::<Mark>(&bytes, sealed).unwrap()),
            Key::User(_) => drop(unseal::<UserRecord<'_>>(&bytes, sealed).unwrap()),
        }
    }

    fn chunk(x: i32, z: i32) -> ChunkId {
        ChunkId { world: 0, x, z }
    }

    fn chunk_key(x: i32, z: i32) -> Key {
        chunk(x, z).key(ChunkPart::Column)
    }

    /// A small sub-chunk that differs by `n`.
    fn small_sub_chunk(n: u32) -> SubChunkRecord {
        let mut chunk = empty_chunk();
        chunk.sub_chunks[0].biomes.palette[0] = n;
        chunk.sub_chunks[0].clone()
    }

    /// The part of a chunk key the random batches use: a sub-chunk of three, or one of two
    /// pages of an extras kind.
    fn random_part(rng: &mut Rng) -> ChunkPart {
        match rng.below(6) {
            0 => ChunkPart::Extras(ChunkKind::BlockEntities, 0),
            1 => ChunkPart::Extras(ChunkKind::Entities, u16::try_from(rng.below(2)).unwrap()),
            2 => ChunkPart::Extras(ChunkKind::ScheduledUpdates, 1),
            _ => ChunkPart::SubChunk(u8::try_from(rng.below(3)).unwrap()),
        }
    }

    /// Applies a random batch to `model` and returns it: puts and deletes over a few chunks'
    /// sub-chunks and extras pages, page deletions from a page on, a player and the meta tick.
    fn random_batch(rng: &mut Rng, model: &mut BTreeMap<Key, Vec<u8>>, tick: u64) -> Batch {
        let mut batch = Batch::default();
        for _ in 0..=rng.below(12) {
            let id = chunk(i32::try_from(rng.below(16)).unwrap() - 8, 3);
            let part = random_part(rng);
            let key = id.key(part);
            match (rng.below(6), part) {
                (0, _) => {
                    model.remove(&key);
                    batch.delete(key);
                }
                (1, ChunkPart::Extras(kind, page)) => {
                    model.retain(|k, _| {
                        !matches!(k, Key::Chunk(ChunkKey {
                            chunk: c,
                            part: ChunkPart::Extras(kk, p),
                        }) if *c == id && *kk == kind && *p >= page)
                    });
                    batch.delete_pages(id, kind, page);
                }
                (_, ChunkPart::SubChunk(_)) => {
                    let n = u32::try_from(rng.below(1000)).unwrap();
                    let sealed = seal(&key.to_bytes(), &small_sub_chunk(n)).unwrap();
                    model.insert(key.clone(), sealed.clone());
                    batch.put(key, sealed);
                }
                _ => {
                    let list: Vec<_> = (0..=rng.below(4))
                        .map(|_| random_compound(rng, 2))
                        .collect();
                    let sealed = seal(&key.to_bytes(), &ExtraRecord(list.into())).unwrap();
                    model.insert(key.clone(), sealed.clone());
                    batch.put(key, sealed);
                }
            }
        }
        for _ in 0..=rng.below(3) {
            let n = rng.below(4);
            let key = access_key(n);
            if rng.below(3) == 0 {
                model.remove(&key);
                batch.delete(key);
            } else {
                let sealed = if n.is_multiple_of(2) {
                    seal(&key.to_bytes(), &Mark).unwrap()
                } else {
                    let record = BanRecord {
                        reason: format!("reason {}", rng.below(100)),
                        source: "console".to_owned(),
                        created: tick,
                        expires: (rng.below(2) == 0).then_some(tick + 100),
                    };
                    seal(&key.to_bytes(), &record).unwrap()
                };
                model.insert(key.clone(), sealed.clone());
                batch.put(key, sealed);
            }
        }
        let key = Key::User(format!("value-{}", rng.below(3)));
        if rng.below(4) == 0 {
            model.remove(&key);
            batch.delete(key);
        } else {
            let json = format!("{{\"n\":{}}}", rng.below(1000)).into_bytes();
            let sealed = seal(&key.to_bytes(), &UserRecord(json.into())).unwrap();
            model.insert(key.clone(), sealed.clone());
            batch.put(key, sealed);
        }
        let key = Key::Player(PlayerKey::offline("Steve"));
        let player = PlayerRecord {
            world: "world".to_owned(),
            position: [f64::from(rng.next().to_le_bytes()[0]), 64.0, 0.5],
            extra: random_compound(rng, 2),
        };
        let sealed = seal(&key.to_bytes(), &player).unwrap();
        model.insert(key.clone(), sealed.clone());
        batch.put(key, sealed);
        let sealed = seal(META_KEY, &Meta::at(tick)).unwrap();
        model.insert(Key::Meta, sealed.clone());
        batch.put(Key::Meta, sealed);
        batch
    }

    /// One of four `access` keys: an operator, a ban, a whitelist entry and an IP ban.
    fn access_key(n: usize) -> Key {
        let (kind, name) = match n {
            0 => (AccessKind::Op, "steve"),
            1 => (AccessKind::Ban, "alex"),
            2 => (AccessKind::Whitelist, "steve"),
            _ => (AccessKind::IpBan, "10.0.0.1"),
        };
        Key::Access(AccessKey {
            kind,
            name: name.to_owned(),
        })
    }

    fn all_keys() -> Vec<Key> {
        let parts = (0..3).map(ChunkPart::SubChunk).chain(
            ChunkKind::EXTRAS
                .into_iter()
                .flat_map(|kind| [0, 1].map(|page| ChunkPart::Extras(kind, page))),
        );
        let parts: Vec<ChunkPart> = parts.collect();
        let mut keys: Vec<Key> = (-8..8)
            .flat_map(|x| parts.iter().map(move |&part| chunk(x, 3).key(part)))
            .collect();
        keys.push(Key::Player(PlayerKey::offline("steve")));
        keys.push(Key::Meta);
        keys.extend((0..4).map(access_key));
        keys.extend((0..3).map(|n| Key::User(format!("value-{n}"))));
        keys
    }

    #[test]
    fn keys_sort_by_world_then_coordinate() {
        let key = |world, x, z| {
            ChunkKey {
                chunk: ChunkId { world, x, z },
                part: ChunkPart::Column,
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

    /// The bytes after a chunk's prefix: the whole terrain 0, extras pages kind then page (none
    /// for page 0, the key formats 2 and 3 wrote), sub-chunks `0x20 | index`; anything else is
    /// a key no Anchor writes. Within a chunk, pages sort in order.
    #[test]
    fn chunk_parts_have_one_key_each() {
        let parts = [
            (ChunkPart::Column, vec![0]),
            (ChunkPart::Extras(ChunkKind::BlockEntities, 0), vec![1]),
            (
                ChunkPart::Extras(ChunkKind::BlockEntities, 1),
                vec![1, 0, 1],
            ),
            (
                ChunkPart::Extras(ChunkKind::BlockEntities, 258),
                vec![1, 1, 2],
            ),
            (ChunkPart::Extras(ChunkKind::Entities, 0), vec![2]),
            (
                ChunkPart::Extras(ChunkKind::ScheduledUpdates, u16::MAX),
                vec![3, 0xff, 0xff],
            ),
            (ChunkPart::SubChunk(0), vec![0x20]),
            (ChunkPart::SubChunk(23), vec![0x37]),
        ];
        let id = chunk(5, -5);
        let mut keys = Vec::new();
        for (part, suffix) in parts {
            let key = ChunkKey { chunk: id, part }.to_bytes();
            assert_eq!(key[CHUNK_PREFIX..], suffix, "{part:?}");
            assert_eq!(ChunkPart::from_suffix(&suffix).unwrap(), part);
            keys.push(key);
        }
        assert!(keys.is_sorted());
        for bad in [
            &[][..],
            &[4],
            &[0x1f],
            &[0x38],
            &[1, 0, 0],
            &[0, 0, 1],
            &[1, 0],
            &[1, 0, 0, 1],
        ] {
            assert!(
                matches!(ChunkPart::from_suffix(bad), Err(Error::Malformed(_))),
                "{bad:?}"
            );
        }
    }

    /// A chunk's values come back in key order, every part of it and nothing of its neighbours;
    /// a key no Anchor writes damages that chunk alone, and a page deletion removes exactly the
    /// pages from its first on.
    #[test]
    fn a_chunk_read_gives_its_values_and_pages_delete_from_one_on() {
        let disk = Disk::default();
        let mut storage = memory(&disk);
        let (a, b, c) = (chunk(0, 0), chunk(0, 1), chunk(-1, 0));
        let parts = [
            ChunkPart::SubChunk(3),
            ChunkPart::Extras(ChunkKind::Entities, 2),
            ChunkPart::Column,
            ChunkPart::Extras(ChunkKind::Entities, 0),
            ChunkPart::Extras(ChunkKind::Entities, 1),
            ChunkPart::Extras(ChunkKind::BlockEntities, 0),
        ];
        let mut batch = Batch::default();
        for id in [a, b] {
            for part in parts {
                batch.put(id.key(part), vec![part_byte(part)]);
            }
        }
        // A key of chunk `c` with a suffix no Anchor writes.
        let mut odd = ChunkKey {
            chunk: c,
            part: ChunkPart::Column,
        }
        .to_bytes();
        odd[CHUNK_PREFIX] = 9;
        storage.commit(&batch).unwrap();
        drop(storage);
        raw_put(&disk, &odd);
        let mut storage = closed(&disk);
        let read = storage.read_chunks(&[a, c, chunk(7, 7)]).unwrap();
        let mut sorted = parts.to_vec();
        sorted.sort_by_key(|&part| ChunkKey { chunk: a, part }.to_bytes());
        let expected: Vec<(ChunkPart, Vec<u8>)> =
            sorted.iter().map(|&p| (p, vec![part_byte(p)])).collect();
        assert_eq!(read[0].as_ref().unwrap(), &expected);
        assert!(matches!(read[1], Err(Error::Malformed(_))));
        assert!(read[2].as_ref().unwrap().is_empty());
        let mut batch = Batch::default();
        batch.delete_pages(a, ChunkKind::Entities, 1);
        batch.delete_pages(b, ChunkKind::BlockEntities, 0);
        storage.commit(&batch).unwrap();
        let read = storage.read_chunks(&[a, b]).unwrap();
        let parts_of = |i: usize| -> Vec<ChunkPart> {
            read[i].as_ref().unwrap().iter().map(|(p, _)| *p).collect()
        };
        let without = |gone: &[ChunkPart]| -> Vec<ChunkPart> {
            sorted
                .iter()
                .copied()
                .filter(|p| !gone.contains(p))
                .collect()
        };
        assert_eq!(
            parts_of(0),
            without(&[
                ChunkPart::Extras(ChunkKind::Entities, 1),
                ChunkPart::Extras(ChunkKind::Entities, 2)
            ])
        );
        assert_eq!(
            parts_of(1),
            without(&[ChunkPart::Extras(ChunkKind::BlockEntities, 0)])
        );
    }

    /// A one-byte stand-in value telling the parts of a test apart.
    fn part_byte(part: ChunkPart) -> u8 {
        ChunkKey {
            chunk: chunk(0, 0),
            part,
        }
        .to_bytes()[CHUNK_PREFIX..]
            .iter()
            .fold(0, |a, &b| a ^ b)
    }

    /// Writes `key` with a one-byte value straight into the `chunks` table, bypassing the key
    /// types (a file another program wrote).
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a test writes the file"
    )]
    fn raw_put(disk: &Disk, key: &[u8]) {
        let db = Source::Memory(disk.clone()).open().unwrap();
        let txn = db.begin_write().unwrap();
        drop(
            txn.open_table(CHUNKS)
                .unwrap()
                .insert(key, &[1u8][..])
                .unwrap(),
        );
        txn.commit().unwrap();
    }

    #[test]
    fn values_round_trip_through_a_commit_and_a_reopen() {
        let disk = Disk::default();
        let mut storage = memory(&disk);
        let mut rng = Rng(11);
        let column = random_chunk(&mut rng);
        let world = WorldRecord { number: 3, time: 0 };
        let keys = [
            chunk_key(-1, 1),
            Key::World("lobby".to_owned()),
            Key::Meta,
            Key::Player(PlayerKey::xuid(7)),
        ];
        let mut batch = Batch::with_capacity(4);
        batch.put(keys[0].clone(), seal(&keys[0].to_bytes(), &column).unwrap());
        batch.put(keys[1].clone(), seal(&keys[1].to_bytes(), &world).unwrap());
        batch.put(Key::Meta, seal(META_KEY, &Meta::at(40)).unwrap());
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
        assert_eq!(unsealed, column);
        let world_value = world_value.unwrap().unwrap();
        assert_eq!(
            unseal::<WorldRecord>(b"lobby", &world_value).unwrap(),
            world
        );
        assert_eq!(
            unseal::<Meta>(META_KEY, &meta_value.unwrap().unwrap()).unwrap(),
            Meta::at(40)
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

    #[test]
    fn an_import_lands_whole_at_its_commit_and_not_at_all_when_dropped() {
        let mut rng = Rng(0x5eed_0003);
        let disk = Disk::default();
        let mut storage = memory(&disk);
        let mut model = BTreeMap::new();
        storage
            .commit(&random_batch(&mut rng, &mut model, 1))
            .unwrap();
        let before = model.clone();
        let mut import = storage.begin_import().unwrap();
        for tick in 2..5 {
            import
                .write(&random_batch(&mut rng, &mut model, tick))
                .unwrap();
        }
        drop(import);
        assert_eq!(contents(&mut storage, &all_keys()), before);
        let mut import = storage.begin_import().unwrap();
        let mut model = before.clone();
        for tick in 2..5 {
            import
                .write(&random_batch(&mut rng, &mut model, tick))
                .unwrap();
        }
        import.commit().unwrap();
        drop(storage);
        disk.power_loss(&mut rng, false);
        let mut storage = memory(&disk);
        assert_eq!(contents(&mut storage, &all_keys()), model);
    }

    /// A failure at each call of an import's commit in turn, then a power loss: the file holds
    /// what it held before the import, until the fault comes after the commit's last call.
    #[test]
    fn a_failed_import_commit_leaves_the_file_as_it_was() {
        let mut rng = Rng(0x5eed_0004);
        for at in 1.. {
            let disk = Disk::default();
            let mut storage = memory(&disk);
            let mut model = BTreeMap::new();
            storage
                .commit(&random_batch(&mut rng, &mut model, 1))
                .unwrap();
            let before = model.clone();
            let mut import = storage.begin_import().unwrap();
            import
                .write(&random_batch(&mut rng, &mut model, 2))
                .unwrap();
            disk.arm(Fault::From { at });
            let committed = import.commit().is_ok();
            drop(storage);
            disk.power_loss(&mut rng, true);
            let mut storage = memory(&disk);
            let expected = if committed { &model } else { &before };
            assert_eq!(&contents(&mut storage, &all_keys()), expected, "{at}");
            if committed {
                assert!(at > 2, "{at}");
                break;
            }
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
            std::fs::read(self.0.join(SERVER_FILE)).unwrap()
        }

        /// The file's length; readable while the file is open, unlike its bytes on Windows.
        fn world_len(&self) -> u64 {
            std::fs::metadata(self.0.join(SERVER_FILE)).unwrap().len()
        }

        fn set_world(&self, bytes: &[u8]) {
            std::fs::write(self.0.join(SERVER_FILE), bytes).unwrap();
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
        let mut storage = Storage::open(&dir.0, Start::checked(StartBackup::Make)).unwrap();
        assert!(
            storage.read(&[Key::Meta]).unwrap()[0]
                .as_ref()
                .unwrap()
                .is_none()
        );
        assert!(storage.worlds().unwrap().is_empty());
        let mut batch = Batch::default();
        batch.put(Key::Meta, seal(META_KEY, &Meta::at(9)).unwrap());
        storage.commit(&batch).unwrap();
        storage.close().unwrap();
        assert!(
            dir.backups().is_empty(),
            "a new file has nothing to back up"
        );
        let mut storage = Storage::open(&dir.0, Start::checked(StartBackup::Make)).unwrap();
        let meta = storage
            .read(&[Key::Meta])
            .unwrap()
            .remove(0)
            .unwrap()
            .unwrap();
        assert_eq!(unseal::<Meta>(META_KEY, &meta).unwrap(), Meta::at(9));
        assert_eq!(dir.backups().len(), 1);
    }

    #[test]
    fn a_second_process_cannot_open_the_file() {
        let dir = TempDir::new("in-use");
        let storage = Storage::open(&dir.0, Start::checked(StartBackup::Make)).unwrap();
        assert!(matches!(
            Storage::open(&dir.0, Start::checked(StartBackup::Make)),
            Err(Error::InUse)
        ));
        assert!(dir.backups().is_empty(), "no copy of a file in use");
        storage.close().unwrap();
    }

    /// An older Anchor's `world.redb` becomes the server file at start, byte for byte: renamed,
    /// not copied, and only when there is no server file yet. A file another process has open is
    /// left under its old name.
    #[test]
    #[expect(
        clippy::disallowed_types,
        clippy::disallowed_methods,
        reason = "off-tick: test fixtures on disk"
    )]
    fn an_old_server_file_is_renamed_once_and_kept_whole() {
        let dir = TempDir::new("old-name");
        let mut storage = Storage::open(&dir.0, Start::checked(StartBackup::Make)).unwrap();
        let mut batch = Batch::default();
        batch.put(Key::Meta, seal(META_KEY, &Meta::at(9)).unwrap());
        storage.commit(&batch).unwrap();
        storage.close().unwrap();
        let (old, new) = (dir.0.join(OLD_FILE), dir.0.join(SERVER_FILE));
        std::fs::rename(&new, &old).unwrap();
        let bytes = std::fs::read(&old).unwrap();
        // An older Anchor still runs it.
        let held = std::fs::File::open(&old).unwrap();
        held.lock().unwrap();
        assert!(matches!(
            Storage::open(&dir.0, Start::checked(StartBackup::Make)),
            Err(Error::InUse)
        ));
        drop(held);
        assert!(old.exists() && !new.exists());
        rename_old_file(&dir.0).unwrap();
        assert!(!old.exists());
        assert_eq!(dir.world(), bytes);
        let read_meta = || {
            let mut storage = Storage::open(&dir.0, Start::checked(StartBackup::Make)).unwrap();
            let meta = storage
                .read(&[Key::Meta])
                .unwrap()
                .remove(0)
                .unwrap()
                .unwrap();
            storage.close().unwrap();
            unseal::<Meta>(META_KEY, &meta).unwrap()
        };
        assert_eq!(read_meta(), Meta::at(9));
        // With both, the server file is the one opened and the old one stays as it is.
        std::fs::write(&old, b"old").unwrap();
        assert_eq!(read_meta(), Meta::at(9));
        assert_eq!(std::fs::read(&old).unwrap(), b"old");
    }

    #[test]
    fn backups_are_capped_at_three_and_never_overwritten() {
        let dir = TempDir::new("backups");
        Storage::open(&dir.0, Start::checked(StartBackup::Make))
            .unwrap()
            .close()
            .unwrap();
        let mut made = Vec::new();
        for _ in 0..5 {
            Storage::open(&dir.0, Start::checked(StartBackup::Make))
                .unwrap()
                .close()
                .unwrap();
            let names = dir.backups();
            assert!(names.len() <= backup::KEPT, "{names:?}");
            let newest = names.last().unwrap().clone();
            assert!(!made.contains(&newest), "{newest} was made twice");
            made.push(newest);
        }
        assert_eq!(dir.backups(), made[made.len() - backup::KEPT..]);
    }

    #[test]
    fn a_start_without_backups_keeps_no_copy() {
        let dir = TempDir::new("transient");
        for _ in 0..2 {
            Storage::open(&dir.0, Start::checked(StartBackup::Transient))
                .unwrap()
                .close()
                .unwrap();
            assert!(dir.backups().is_empty(), "{:?}", dir.backups());
        }
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_start_copy_left_by_a_killed_start_without_backups_is_removed() {
        let dir = TempDir::new("transient-killed");
        Storage::open(&dir.0, Start::checked(StartBackup::Transient))
            .unwrap()
            .close()
            .unwrap();
        let backups = dir.0.join(BACKUP_DIR);
        // A start killed after its copy never removes it.
        backup::create(&dir.0.join(SERVER_FILE), &backups, StartBackup::Transient)
            .unwrap()
            .unwrap();
        Storage::open(&dir.0, Start::checked(StartBackup::Transient))
            .unwrap()
            .close()
            .unwrap();
        let left: Vec<_> = std::fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_start_copy_kept_by_a_failed_start_without_backups_survives_the_next_start() {
        let dir = TempDir::new("transient-kept");
        Storage::open(&dir.0, Start::checked(StartBackup::Transient))
            .unwrap()
            .close()
            .unwrap();
        let original = dir.world();
        // A start that fails with an error that is not damage (here injected, so that no file
        // permission is needed, which root would bypass) keeps its copy.
        let (world, backups) = (dir.0.join(SERVER_FILE), dir.0.join(BACKUP_DIR));
        let copy = backup::create(&world, &backups, StartBackup::Transient)
            .unwrap()
            .unwrap();
        failed_start(
            &world,
            &backups,
            copy,
            StartBackup::Transient,
            &injected_io(),
        );
        Storage::open(&dir.0, Start::checked(StartBackup::Transient))
            .unwrap()
            .close()
            .unwrap();
        let names = dir.backups();
        assert_eq!(names.len(), 1, "{names:?}");
        let kept = std::fs::read(dir.0.join(BACKUP_DIR).join(&names[0])).unwrap();
        assert!(kept == original, "the kept copy holds other bytes");
    }

    /// An error that is not damage, as a failed open of a file in use or without access gives.
    fn injected_io() -> Error {
        Error::Io(io::Error::other("injected"))
    }

    #[test]
    fn failed_starts_keep_only_the_newest_backups() {
        for start in [StartBackup::Transient, StartBackup::Make] {
            let dir = TempDir::new(&format!("failed-starts-{start:?}"));
            Storage::open(&dir.0, Start::checked(StartBackup::Transient))
                .unwrap()
                .close()
                .unwrap();
            let (world, backups) = (dir.0.join(SERVER_FILE), dir.0.join(BACKUP_DIR));
            for _ in 0..backup::KEPT + 2 {
                let copy = backup::create(&world, &backups, start).unwrap().unwrap();
                failed_start(&world, &backups, copy, start, &injected_io());
            }
            let names = dir.backups();
            assert_eq!(names.len(), backup::KEPT, "{start:?}: {names:?}");
        }
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_damaged_file_that_cannot_be_put_back_prunes_no_backup() {
        let dir = TempDir::new("damage-kept");
        Storage::open(&dir.0, Start::checked(StartBackup::Transient))
            .unwrap()
            .close()
            .unwrap();
        let (world, backups) = (dir.0.join(SERVER_FILE), dir.0.join(BACKUP_DIR));
        for _ in 0..backup::KEPT {
            backup::create(&world, &backups, StartBackup::Make)
                .unwrap()
                .unwrap();
        }
        let older = dir.backups();
        // Putting the copy back fails: the world's folder is gone.
        let missing = dir.0.join("missing").join(SERVER_FILE);
        for _ in 0..2 {
            let copy = backup::create(&world, &backups, StartBackup::Make)
                .unwrap()
                .unwrap();
            failed_start(
                &missing,
                &backups,
                copy,
                StartBackup::Make,
                &Error::DamagedFile(None),
            );
        }
        let names = dir.backups();
        assert_eq!(names.len(), backup::KEPT + 2, "{names:?}");
        assert!(older.iter().all(|name| names.contains(name)), "{names:?}");
        assert!(std::fs::exists(&world).unwrap());
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_new_file_is_not_compacted_at_start_and_an_existing_one_is() {
        let dir = TempDir::new("start-compact");
        // A file redb creates with Anchor's tables, never compacted.
        let fresh = dir.0.join("fresh.redb");
        let mut storage = Storage {
            backup: None,
            check: None,
            db: None,
            source: Source::File(fresh.clone()),
            replica: None,
        };
        storage.commit(&Batch::default()).unwrap();
        drop(storage);
        let len = |path: &Path| std::fs::metadata(path).unwrap().len();
        let world = dir.0.join(SERVER_FILE);
        // Dropping closes the file without compacting it.
        drop(Storage::open(&dir.0, Start::checked(StartBackup::Make)).unwrap());
        let created = len(&world);
        assert_eq!(created, len(&fresh));
        drop(Storage::open(&dir.0, Start::checked(StartBackup::Make)).unwrap());
        assert!(len(&world) < created, "{} {created}", len(&world));
    }

    #[test]
    fn a_damaged_file_is_refused_and_left_as_it_was() {
        for start in [StartBackup::Make, StartBackup::Transient] {
            refuses_a_damaged_file(start);
        }
    }

    fn refuses_a_damaged_file(start: StartBackup) {
        let dir = TempDir::new("damaged");
        let bytes = damage_a_closed_file(&dir);
        let e = Storage::open(&dir.0, Start::checked(start)).unwrap_err();
        assert!(e.is_damage(), "{e:?}");
        assert!(dir.world().eq(&bytes), "the file is left as it was");
        assert!(dir.backups().is_empty(), "the copy went back into the file");
        // A second try finds the same file and refuses it the same way.
        assert!(
            Storage::open(&dir.0, Start::checked(start))
                .unwrap_err()
                .is_damage()
        );
        assert!(dir.world().eq(&bytes), "the file is unchanged");
    }

    /// Writes a player to a new server file in `dir`, closes it after a normal stop and damages
    /// the player's bytes, which only redb's integrity check reads at start; returns the bytes.
    fn damage_a_closed_file(dir: &TempDir) -> Vec<u8> {
        let mut storage = Storage::open(&dir.0, Start::checked(StartBackup::Make)).unwrap();
        // Random letters do not compress, so the name is stored as it is and can be found.
        let mut rng = Rng(5);
        let name: String = (0..200)
            .map(|_| char::from(b'a' + u8::try_from(rng.below(26)).unwrap()))
            .collect();
        let key = Key::Player(PlayerKey::xuid(1));
        let player = PlayerRecord {
            world: name.clone(),
            position: [0.0; 3],
            extra: crate::nbt::Compound::new(),
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
        bytes
    }

    #[test]
    fn a_start_checks_and_copies_the_file_as_its_settings_say() {
        use IntegrityCheck::{AfterUncleanShutdown, Always, Never};
        use StartBackup::{Make, Transient};
        let dir = TempDir::new("start-policy");
        // (backups, integrity-check) → (checked, copied), after a normal stop.
        for (backup, check, checked, copied) in [
            (Transient, Always, 1, 1),
            (Transient, AfterUncleanShutdown, 0, 0),
            (Transient, Never, 0, 0),
            (Make, Always, 1, 1),
            (Make, AfterUncleanShutdown, 0, 1),
            (Make, Never, 0, 1),
        ] {
            Storage::open(&dir.0, Start::checked(Transient))
                .unwrap()
                .close()
                .unwrap();
            STARTS.set((0, 0));
            Storage::open(&dir.0, Start { backup, check })
                .unwrap()
                .close()
                .unwrap();
            assert_eq!(STARTS.get(), (checked, copied), "{backup:?} {check:?}");
        }
        // After a stop that did not close the file, every start that may check does.
        for (check, checked) in [(Always, 1), (AfterUncleanShutdown, 1), (Never, 0)] {
            drop(Storage::open(&dir.0, Start::checked(Transient)).unwrap());
            STARTS.set((0, 0));
            let start = Start {
                backup: Transient,
                check,
            };
            Storage::open(&dir.0, start).unwrap().close().unwrap();
            assert_eq!(STARTS.get(), (checked, checked), "{check:?}");
        }
    }

    #[test]
    fn only_a_file_closed_after_a_normal_stop_skips_the_check_after_an_unclean_one() {
        let dir = TempDir::new("closed-mark");
        let world = dir.0.join(SERVER_FILE);
        let after_unclean = || checks(&world, IntegrityCheck::AfterUncleanShutdown);
        // No file: checking a new one costs nothing.
        assert!(after_unclean());
        let start = Start {
            backup: StartBackup::Transient,
            check: IntegrityCheck::AfterUncleanShutdown,
        };
        Storage::open(&dir.0, start).unwrap().close().unwrap();
        assert!(!after_unclean());
        assert!(checks(&world, IntegrityCheck::Always));
        assert!(!checks(&world, IntegrityCheck::Never));
        // A start removes the mark at once: a stop that does not close the file leaves none.
        drop(Storage::open(&dir.0, start).unwrap());
        assert!(after_unclean());
        // So does a start that does not check.
        Storage::open(&dir.0, start).unwrap().close().unwrap();
        let never = Start {
            check: IntegrityCheck::Never,
            ..start
        };
        drop(Storage::open(&dir.0, never).unwrap());
        assert!(after_unclean());
        // An online backup of the open file has no mark.
        let mut storage = Storage::open(&dir.0, start).unwrap();
        assert!(storage.begin_backup(&dir.0.join(BACKUP_DIR)).unwrap());
        let made = loop {
            if let Some(made) = storage.backup_step().unwrap() {
                break made;
            }
        };
        storage.close().unwrap();
        assert!(!after_unclean());
        assert!(checks(&made, IntegrityCheck::AfterUncleanShutdown));
    }

    #[test]
    fn a_file_that_is_not_redb_is_refused_and_keeps_the_older_backups() {
        for start in [StartBackup::Make, StartBackup::Transient] {
            refuses_a_file_that_is_not_redb(start);
        }
    }

    fn refuses_a_file_that_is_not_redb(start: StartBackup) {
        let dir = TempDir::new(&format!("not-redb-{start:?}"));
        for _ in 0..=backup::KEPT {
            Storage::open(&dir.0, Start::checked(StartBackup::Make))
                .unwrap()
                .close()
                .unwrap();
        }
        let older = dir.backups();
        assert_eq!(older.len(), backup::KEPT);
        // A torn first page or another file put at the path: redb finds no magic number.
        let bytes = b"not a redb file".to_vec();
        dir.set_world(&bytes);
        for _ in 0..=backup::KEPT {
            let e = Storage::open(&dir.0, Start::checked(start)).unwrap_err();
            assert!(e.is_damage(), "{start:?}: {e:?}");
            assert!(
                dir.world().eq(&bytes),
                "{start:?}: the file is left as it was"
            );
            assert_eq!(dir.backups(), older, "{start:?}");
        }
    }

    #[test]
    fn a_newer_format_is_refused_and_left_as_it_was() {
        for start in [StartBackup::Make, StartBackup::Transient] {
            refuses_a_newer_format(start);
        }
    }

    fn refuses_a_newer_format(start: StartBackup) {
        let dir = TempDir::new("newer");
        let mut storage = Storage::open(&dir.0, Start::checked(start)).unwrap();
        let mut sealed = seal(META_KEY, &Meta::at(1)).unwrap();
        sealed[..2].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let mut batch = Batch::default();
        batch.put(Key::Meta, sealed);
        storage.commit(&batch).unwrap();
        storage.close().unwrap();
        let bytes = dir.world();
        let e = Storage::open(&dir.0, Start::checked(start)).unwrap_err();
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
        let open_io = |kind| {
            Error::from_open(DatabaseError::Storage(StorageError::Io(io::Error::new(
                kind, "x",
            ))))
        };
        assert!(open_io(io::ErrorKind::InvalidData).is_damage());
        assert!(!open_io(io::ErrorKind::PermissionDenied).is_damage());
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

    /// Key and value bytes of each entry of each table, by table name.
    type Tables = BTreeMap<String, Vec<(Vec<u8>, Vec<u8>)>>;

    /// Every entry of every table of `db`.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a test reads a database"
    )]
    fn tables(db: &Database) -> Tables {
        use redb::TableHandle;
        let txn = db.begin_read().unwrap();
        txn.list_tables()
            .unwrap()
            .map(|handle| {
                let name = handle.name().to_owned();
                let table = txn
                    .open_table(TableDefinition::<&[u8], &[u8]>::new(&name))
                    .unwrap();
                let entries = table
                    .range::<&[u8]>(..)
                    .unwrap()
                    .map(|entry| {
                        let (key, value) = entry.unwrap();
                        (key.value().to_vec(), value.value().to_vec())
                    })
                    .collect();
                (name, entries)
            })
            .collect()
    }

    /// A batch of server data values of `mib` MiB of random bytes in all, which do not compress,
    /// 16 KiB each, and a meta tick: an online backup copies them in several steps.
    fn large_batch(rng: &mut Rng, mib: usize, tick: u64) -> Batch {
        let mut batch = Batch::default();
        for n in 0..mib * 64 {
            let key = Key::User(format!("large-{n}"));
            let bytes: Vec<u8> = (0..2 << 10)
                .flat_map(|_| rng.next().to_le_bytes())
                .collect();
            let sealed = seal(&key.to_bytes(), &UserRecord(bytes.into())).unwrap();
            batch.put(key, sealed);
        }
        batch.put(Key::Meta, seal(META_KEY, &Meta::at(tick)).unwrap());
        batch
    }

    /// The names in `dir`.
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn names_in(dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Steps the online backup to its end; returns its path and the steps taken.
    fn finish_backup(storage: &mut Storage) -> (PathBuf, usize) {
        let mut steps = 0;
        loop {
            steps += 1;
            if let Some(path) = storage.backup_step().unwrap() {
                return (path, steps);
            }
            assert!(storage.is_backing_up());
        }
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: a test opens a backup")]
    fn an_online_backup_holds_the_commit_it_began_at_table_by_table() {
        let dir = TempDir::new("online");
        let mut rng = Rng(0xbac0_0001);
        let mut storage = Storage::open(&dir.0, Start::checked(StartBackup::Make)).unwrap();
        let mut model = BTreeMap::new();
        storage
            .commit(&random_batch(&mut rng, &mut model, 0))
            .unwrap();
        storage.commit(&large_batch(&mut rng, 20, 1)).unwrap();
        let committed = tables(storage.database().unwrap());
        assert!(storage.begin_backup(&dir.0.join(BACKUP_DIR)).unwrap());
        assert_eq!(storage.backup_step().unwrap(), None);
        // Commits and reads go on between steps; the backup does not see the new commit.
        let later = random_batch(&mut rng, &mut model, 2);
        storage.commit(&later).unwrap();
        assert_eq!(contents(&mut storage, &all_keys()), model);
        let (path, steps) = finish_backup(&mut storage);
        assert!(steps >= 2, "{steps} more steps");
        assert_eq!(
            names_in(&dir.0.join(BACKUP_DIR)),
            [path.file_name().unwrap().to_str().unwrap()]
        );
        let mut backup = Builder::new().open(&path).unwrap();
        assert!(backup.check_integrity().unwrap());
        let copied = tables(&backup);
        assert_eq!(
            copied.keys().collect::<Vec<_>>(),
            committed.keys().collect::<Vec<_>>()
        );
        for (name, entries) in &committed {
            assert!(copied[name] == *entries, "table {name} differs");
        }
        drop(backup);
        assert_eq!(crate::save::committed_tick(&path).unwrap(), 1);
        storage.close().unwrap();
    }

    #[test]
    fn a_failure_mid_copy_leaves_no_backup_and_the_file_works_on() {
        let dir = TempDir::new("online-fault");
        let backups = dir.0.join(BACKUP_DIR);
        let mut rng = Rng(0xbac0_0002);
        let disk = Disk::default();
        let mut storage = memory(&disk);
        // More than redb's cache holds, so the copy has pages to read from the disk.
        let mib = (CACHE_BYTES >> 20) + 16;
        storage.commit(&large_batch(&mut rng, mib, 1)).unwrap();
        assert!(storage.begin_backup(&backups).unwrap());
        assert_eq!(names_in(&backups).len(), 1, "the temporary file");
        disk.arm(Fault::Every { call: Call::Read });
        let failed = loop {
            match storage.backup_step() {
                Ok(None) => {}
                Ok(Some(path)) => panic!("{path:?} was made without reading the disk"),
                Err(e) => break e,
            }
        };
        assert!(!failed.is_damage(), "{failed:?}");
        disk.arm(Fault::None);
        assert!(!storage.is_backing_up());
        assert!(names_in(&backups).is_empty(), "{:?}", names_in(&backups));
        // The file works on.
        let mut model = BTreeMap::new();
        storage
            .commit(&random_batch(&mut rng, &mut model, 2))
            .unwrap();
        assert_eq!(contents(&mut storage, &all_keys()), model);
        // A new backup can begin; dropped unfinished, it leaves no file either.
        assert!(storage.begin_backup(&backups).unwrap());
        drop(storage);
        assert!(names_in(&backups).is_empty(), "{:?}", names_in(&backups));
    }

    #[test]
    fn a_reopen_of_the_file_stops_the_backup_and_leaves_no_file_of_it() {
        let dir = TempDir::new("online-reopen");
        let backups = dir.0.join(BACKUP_DIR);
        let mut rng = Rng(0xbac0_0003);
        let disk = Disk::default();
        let mut storage = memory(&disk);
        storage.commit(&large_batch(&mut rng, 20, 1)).unwrap();
        assert!(storage.begin_backup(&backups).unwrap());
        assert_eq!(storage.backup_step().unwrap(), None);
        // The commit fails once and is retried on the reopened file.
        let mut model = BTreeMap::new();
        disk.arm(Fault::Once {
            call: Call::Write,
            at: 1,
        });
        storage
            .commit(&random_batch(&mut rng, &mut model, 2))
            .unwrap();
        assert_eq!(disk.failed(), 1);
        assert!(!storage.is_backing_up());
        assert!(names_in(&backups).is_empty(), "{:?}", names_in(&backups));
        assert_eq!(contents(&mut storage, &all_keys()), model);
    }

    #[test]
    fn a_second_backup_is_refused_while_one_is_being_made() {
        let dir = TempDir::new("online-second");
        let backups = dir.0.join(BACKUP_DIR);
        let disk = Disk::default();
        let mut storage = memory(&disk);
        storage.commit(&large_batch(&mut Rng(4), 10, 1)).unwrap();
        assert!(storage.begin_backup(&backups).unwrap());
        assert!(!storage.begin_backup(&backups).unwrap());
        let (first, _) = finish_backup(&mut storage);
        assert!(storage.begin_backup(&backups).unwrap());
        let (second, _) = finish_backup(&mut storage);
        assert_ne!(first, second);
        assert_eq!(names_in(&backups).len(), 2);
    }

    #[test]
    fn online_and_start_backups_keep_the_three_newest_together() {
        let dir = TempDir::new("online-kept");
        let backups = dir.0.join(BACKUP_DIR);
        Storage::open(&dir.0, Start::checked(StartBackup::Make))
            .unwrap()
            .close()
            .unwrap();
        let mut storage = Storage::open(&dir.0, Start::checked(StartBackup::Make)).unwrap();
        let start = dir.backups();
        assert_eq!(start.len(), 1);
        let mut made = Vec::new();
        for _ in 0..4 {
            assert!(storage.begin_backup(&backups).unwrap());
            let (path, _) = finish_backup(&mut storage);
            made.push(path.file_name().unwrap().to_str().unwrap().to_owned());
            assert!(dir.backups().len() <= backup::KEPT);
        }
        assert_eq!(dir.backups(), made[made.len() - backup::KEPT..]);
        storage.close().unwrap();
    }

    /// Steps the check being made to its end; returns what it found and the steps it took.
    fn finish_check(storage: &mut Storage) -> (Checked, usize) {
        let mut steps = 0;
        loop {
            steps += 1;
            if let Some(found) = storage.check_step() {
                return (found, steps);
            }
        }
    }

    #[test]
    fn a_check_reads_every_value_of_the_commit_it_started_at_in_steps() {
        let disk = Disk::default();
        let mut storage = memory(&disk);
        storage
            .commit(&large_batch(&mut Rng(0xc4ec), 20, 1))
            .unwrap();
        assert!(storage.begin_check().unwrap());
        assert!(!storage.begin_check().unwrap(), "one check at a time");
        assert_eq!(storage.check_step(), None);
        // Commits go on between steps; the check reads the commit it started at.
        let mut more = large_batch(&mut Rng(0xc4ed), 1, 2);
        more.put(
            Key::User("added".to_owned()),
            seal(b"added", &UserRecord(vec![1].into())).unwrap(),
        );
        storage.commit(&more).unwrap();
        let (found, steps) = finish_check(&mut storage);
        // The meta value and 64 values a MiB.
        assert_eq!(
            found,
            Checked::Passed {
                values: 1 + 20 * 64
            }
        );
        // 20 MiB in steps of 8 MiB: one step before the commit, two after it.
        assert_eq!(steps, 2);
        assert!(!storage.is_checking());
        // The next check sees the later commit.
        assert!(storage.begin_check().unwrap());
        let (found, _) = finish_check(&mut storage);
        assert_eq!(
            found,
            Checked::Passed {
                values: 2 + 20 * 64
            }
        );
    }

    #[test]
    fn a_check_counts_the_damaged_values_and_names_the_first() {
        let disk = Disk::default();
        let mut storage = memory(&disk);
        storage
            .commit(&large_batch(&mut Rng(0xc4ee), 1, 1))
            .unwrap();
        drop(storage);
        // Values whose seal does not match: a byte the redb file holds whole.
        raw_put(&disk, &chunk_key(0, 0).to_bytes());
        raw_put(&disk, &chunk_key(0, 1).to_bytes());
        let mut storage = memory(&disk);
        assert!(storage.begin_check().unwrap());
        let (found, _) = finish_check(&mut storage);
        let Checked::Damaged {
            values,
            damaged,
            first,
        } = found
        else {
            panic!("{found:?}");
        };
        assert_eq!((values, damaged), (1 + 64 + 2, 2));
        assert!(first.starts_with("table chunks: "), "{first}");
    }

    #[test]
    fn a_reopen_of_the_file_ends_the_check_and_says_why() {
        let mut rng = Rng(0xc4ef);
        let disk = Disk::default();
        let mut storage = memory(&disk);
        storage.commit(&large_batch(&mut rng, 20, 1)).unwrap();
        assert!(storage.begin_check().unwrap());
        assert_eq!(storage.check_step(), None);
        // The commit fails once and is retried on the reopened file.
        let mut model = BTreeMap::new();
        disk.arm(Fault::Once {
            call: Call::Write,
            at: 1,
        });
        storage
            .commit(&random_batch(&mut rng, &mut model, 2))
            .unwrap();
        assert_eq!(disk.failed(), 1);
        assert!(matches!(
            storage.check_step(),
            Some(Checked::Failed(why)) if why.contains("reopened")
        ));
        assert!(!storage.is_checking());
    }

    /// Wall-clock milliseconds since `start`.
    fn ms(start: std::time::Instant) -> f64 {
        clock::now().duration_since(start).as_secs_f64() * 1e3
    }

    /// Measurements for PLAN.md: `cargo test --release --lib storage::tests::measure_server_file
    /// -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement"]
    #[expect(
        clippy::print_stdout,
        reason = "consistency: a measurement prints its results"
    )]
    fn measure_server_file() {
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
        let mut storage = Storage::open(&dir.0, Start::checked(StartBackup::Make)).unwrap();
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
        let mut storage = Storage::open(&dir.0, Start::checked(StartBackup::Make)).unwrap();
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

    /// Commits chunks of `records` terrain by sub-chunk, 2,000 a commit, until the server file in
    /// `dir` reaches `target` bytes; returns how many.
    fn fill_world(
        storage: &mut Storage,
        dir: &TempDir,
        records: &[ChunkRecord],
        target: u64,
    ) -> u32 {
        let mut chunks = 0u32;
        while dir.world_len() < target {
            let mut batch = Batch::with_capacity(2_000 * 24 + 1);
            for _ in 0..2_000 {
                let id = ChunkId {
                    world: 0,
                    x: i32::try_from(chunks % 2048).unwrap(),
                    z: i32::try_from(chunks / 2048).unwrap(),
                };
                let record = &records[usize::try_from(chunks).unwrap() % records.len()];
                for (index, sub_chunk) in (0u8..).zip(record.sub_chunks.iter()) {
                    let key = id.key(ChunkPart::SubChunk(index));
                    batch.put(key.clone(), seal(&key.to_bytes(), sub_chunk).unwrap());
                }
                chunks += 1;
            }
            let tick = u64::from(chunks);
            batch.put(Key::Meta, seal(META_KEY, &Meta::at(tick)).unwrap());
            storage.commit(&batch).unwrap();
        }
        chunks
    }

    /// Start and online backup costs by file size for PLAN.md (10.9 저장 시작 비용): `cargo test
    /// --release --lib storage::tests::measure_start_cost -- --ignored --nocapture`. Each world
    /// is terrain chunks committed by sub-chunk as the save thread writes them, 2,000 a commit,
    /// until the file reaches the size; then it is closed (compacted) as at shutdown, and each
    /// part of the next start is timed alone, then the online backup.
    #[test]
    #[ignore = "measurement"]
    #[expect(
        clippy::print_stdout,
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "consistency: a measurement prints its results and reads files"
    )]
    fn measure_start_cost() {
        let mut rng = Rng(0x5747_0001);
        let records: Vec<ChunkRecord> = (0..64).map(|_| terrain_chunk(&mut rng)).collect();
        for target in [256u64 << 20, 1 << 30, 4 << 30] {
            let dir = TempDir::new(&format!("start-cost-{target}"));
            let world = dir.0.join(SERVER_FILE);
            let backups = dir.0.join(BACKUP_DIR);
            let mut storage = Storage::open(&dir.0, Start::checked(StartBackup::Make)).unwrap();
            let start = clock::now();
            let chunks = fill_world(&mut storage, &dir, &records, target);
            println!(
                "{} MiB: {chunks} chunks committed in {:.1} s",
                target >> 20,
                ms(start) / 1e3
            );
            let start = clock::now();
            storage.close().unwrap();
            let size = dir.world_len();
            println!(
                "  shutdown compact {:.0} ms, file {} MiB",
                ms(start),
                size >> 20
            );
            // The start's parts, each alone.
            let start = clock::now();
            let copy = backup::create(&world, &backups, StartBackup::Make)
                .unwrap()
                .unwrap();
            println!(
                "  start copy (io::copy, as the start does) {:.0} ms",
                ms(start)
            );
            std::fs::remove_file(&copy).unwrap();
            let start = clock::now();
            {
                let mut source = std::fs::File::open(&world).unwrap();
                let mut target = std::fs::File::create(dir.0.join("copy")).unwrap();
                let mut buffer = vec![0; 1 << 20];
                loop {
                    let n = io::Read::read(&mut source, &mut buffer).unwrap();
                    if n == 0 {
                        break;
                    }
                    io::Write::write_all(&mut target, &buffer[..n]).unwrap();
                }
                target.sync_all().unwrap();
            }
            println!(
                "  start copy (1 MiB buffer, for comparison) {:.0} ms",
                ms(start)
            );
            std::fs::remove_file(dir.0.join("copy")).unwrap();
            let start = clock::now();
            let mut db = Source::File(world.clone()).open().unwrap();
            println!("  open {:.0} ms", ms(start));
            let start = clock::now();
            assert!(db.check_integrity().unwrap());
            println!("  check_integrity {:.0} ms", ms(start));
            let start = clock::now();
            let compacted = db.compact().unwrap();
            println!(
                "  compact after a shutdown compact {:.0} ms (compacted: {compacted})",
                ms(start)
            );
            drop(db);
            let start = clock::now();
            let mut storage = Storage::open(&dir.0, Start::checked(StartBackup::Make)).unwrap();
            println!("  whole start {:.0} ms", ms(start));
            // The online backup, with the longest step: the longest a load waits.
            let start = clock::now();
            assert!(storage.begin_backup(&backups).unwrap());
            let mut steps = 0;
            let mut longest = 0.0f64;
            let (path, last) = loop {
                let step = clock::now();
                let done = storage.backup_step().unwrap();
                steps += 1;
                if let Some(path) = done {
                    break (path, ms(step));
                }
                longest = longest.max(ms(step));
            };
            println!(
                "  online backup {:.0} ms in {steps} steps, longest copy step {longest:.1} ms, \
                 last step {last:.1} ms, backup {} MiB",
                ms(start),
                std::fs::metadata(&path).unwrap().len() >> 20
            );
            storage.close().unwrap();
        }
    }
}
