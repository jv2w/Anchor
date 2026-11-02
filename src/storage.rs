//! One data file: start policy, batched reads and commits. The server's data is split
//! into such files ([`Place`]): `data/main.redb` (the save tick, operators, bans, whitelist and the
//! user's server data), `data/players.redb` and `data/worlds/<world>.redb` for each persistent
//! world (its record and chunks). The save thread
//! (`save`) commits one snapshot to each file that changed, the main file last.
//!
//! Only the save thread, the world import before the server runs, a replay
//! ([`Storage::open_replica`]) and the world export ([`Snapshot`]) use this module; every call
//! blocks on file I/O. Tables map raw keys
//! to sealed values ([`seal`](seal::seal)): `meta` (one [`Meta`]: the save it last committed; in
//! the main file also the deletion marks of the worlds that save deleted, [`Key::Deleted`]),
//! `worlds` (in a world file, [`WORLD_KEY`] → [`WorldRecord`](codec::WorldRecord)), `chunks`
//! ([`ChunkKey`] → the record its [`ChunkPart`] names), `players` ([`PlayerKey`] →
//! [`PlayerRecord`](codec::PlayerRecord)), `access` ([`AccessKey`] → [`Mark`](codec::Mark) for
//! operators and the whitelist, [`BanRecord`](codec::BanRecord) for bans) and `user` (a server data
//! name → [`UserRecord`](codec::UserRecord)). Each file uses the tables of its part. Values are
//! external input when read back: the seal's checksum and the record decoders check them.
//!
//! Start ([`Storage::open`]): check the closed file without writing to it ([`verify`]): redb's
//! recovery of a crash, redb's `check_integrity()` as `integrity-check` says (with
//! `after-unclean-shutdown`, only when the file lacks the mark a normal stop leaves,
//! [`CLOSED_KEY`]) and the format of the last commit, all on a backend that keeps redb's writes
//! in memory. A damaged file or a newer format is refused and keeps its bytes. Then open
//! the file for the run (redb recovers a crash here again), create the tables and, with
//! `backups = true`, begin an online backup of that first commit, which the save thread copies in
//! steps while the server runs; it prunes to the [`backup::KEPT`] newest once whole. So a refused
//! file never has a start backup. A start does not compact: freed pages are reused by later
//! commits, and only data that shrinks leaves free pages at the end.
//!
//! Commits ([`Storage::commit`]) write a whole batch in one quick-repair write transaction. A
//! failure drops the handle, reopens the file and retries, at most [`RETRIES`] times. Values are
//! absolute, so a retry of a commit that did land writes the same state again. Batched reads
//! ([`Storage::read`]) retry the same way. The world import ([`Storage::begin_import`]) writes a
//! whole imported world in one such transaction, committed once.
//!
//! The main file's commit is the one point a save is made at: the players and world files commit
//! before it, each keeping a redb persistent savepoint of itself as it was
//! ([`Storage::commit_with_savepoint`]), one per file. A start rolls every file later than the
//! main file back to its savepoint before it opens it ([`settle`]), so after a stop anywhere the
//! files hold the main file's last save together. Paths that only read refuse such a file
//! ([`Error::Ahead`]).
//!
//! An online backup ([`Storage::begin_backup`]) copies the last commit from one read transaction
//! in steps, between which reads and commits go on; a reopen of the file stops it. A check
//! ([`Storage::begin_check`], `/check`) reads the last commit the same way.

mod backup;
mod check;
pub(crate) mod codec;
pub(crate) mod seal;

pub(crate) use check::{Checked, verify};

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
use tracing::{debug, error, info, warn};

use crate::config::IntegrityCheck;
use codec::Meta;
use seal::{FORMAT_VERSION, unseal};

/// The table of the operators, bans and whitelist.
const ACCESS: Table = TableDefinition::new("access");
/// The backup folder in the data folder.
pub(crate) const BACKUP_DIR: &str = "backups";
/// redb's page cache. Loaded chunks live in the world, and the OS caches the file, so this only
/// spares a system call for pages read again; redb's default of 1 GiB would mostly hold copies.
const CACHE_BYTES: usize = 64 << 20;
/// The bytes a chunk's keys start with ([`ChunkId`]); the rest tell its [`ChunkPart`].
pub(crate) const CHUNK_PREFIX: usize = 12;
/// The table of a world's chunk values.
const CHUNKS: Table = TableDefinition::new("chunks");
/// The mark of a normal stop, in the `meta` table with an empty value: the last commit of
/// [`Storage::close`] writes it and the first commit of a start removes it, so it is there only
/// while the file is closed after a normal stop (`integrity-check = "after-unclean-shutdown"`).
const CLOSED_KEY: &[u8] = b"closed";
/// What the keys of the deletion marks ([`Key::Deleted`]) in the `meta` table start with, before
/// the world's name.
const DELETED_PREFIX: &[u8] = b"deleted:";
/// The extension of every data file.
pub(crate) const EXTENSION: &str = "redb";
/// The characters a world name that is a file name never holds.
const FORBIDDEN: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
/// The file of the save's tick and commit identity, the operators, bans and whitelist, and the
/// user's server data, committed last of a save.
pub(crate) const MAIN_FILE: &str = "main";
/// Most `access` values [`Storage::access`] returns: every list full.
const MAX_ACCESS: usize = 4 * crate::permission::MAX_ENTRIES;
/// Most bytes of a `chunks` key after the chunk's prefix: the kind byte and the page number of a
/// later extras page.
const MAX_CHUNK_SUFFIX: usize = 3;
/// Most bytes of a world name that is a file name: `<name>.redb` fits the 255 bytes of a file
/// name on Linux and the 255 UTF-16 units on Windows.
pub(crate) const MAX_FILE_NAME: usize = 250;
/// Most world files `data/worlds/` may hold (`save::world_names` counts them, and
/// `save::new_place` gives no place for a new one past them).
pub(crate) const MAX_WORLDS: usize = 1024;
/// The table of the file's meta value and marks.
const META: Table = TableDefinition::new("meta");
/// The key of the `meta` value.
const META_KEY: &[u8] = b"meta";
/// What the key of a player without an XUID starts with, before the lowercase name.
const OFFLINE_PREFIX: &str = "offline:";
/// The table of the players' records.
const PLAYERS: Table = TableDefinition::new("players");
/// The file of the players' data.
pub(crate) const PLAYERS_FILE: &str = "players";
/// Failed commits retried before [`Storage::commit`] gives up.
pub(crate) const RETRIES: usize = 3;
/// The sign bit of a chunk coordinate, flipped in the key so that keys sort by coordinate.
const SIGN_BIT: u32 = 0x8000_0000;
/// The bits of a sub-chunk value's kind byte that hold the sub-chunk's index.
const SUB_CHUNK_INDEX_MASK: u8 = 0x1f;
/// The kind byte of the sub-chunk values: `0x20 | index`.
const SUB_CHUNK_KIND: u8 = 0x20;
/// The table of the user's server data: one value per registered name.
const USER: Table = TableDefinition::new("user");
/// The folder of the world files in the data folder.
pub(crate) const WORLD_DIR: &str = "worlds";
/// The key of a world file's record in its `worlds` table: the file's name is the world's.
const WORLD_KEY: &[u8] = b"world";
/// The table of a world file's record.
const WORLDS: Table = TableDefinition::new("worlds");

type Table = TableDefinition<'static, &'static [u8], &'static [u8]>;

/// The values a replay committed on top of its file, by table in [`Key::pick`] order and key
/// bytes; `None` is a deleted value.
type Overlay = [std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>>; 6];

/// A data file and the folder of its backups: `data/<name>.redb` and `data/backups/<name>/` for
/// the server's files, `data/worlds/<world>.redb` and `data/backups/worlds/<world>/` for a world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Place {
    /// The file.
    pub(crate) file: PathBuf,
    /// The folder of its backups.
    pub(crate) backups: PathBuf,
}

impl Place {
    /// The server's file `name` ([`MAIN_FILE`], [`PLAYERS_FILE`]) in `data_dir`.
    pub(crate) fn data(data_dir: &Path, name: &str) -> Self {
        Self {
            file: data_dir.join(format!("{name}.{EXTENSION}")),
            backups: data_dir.join(BACKUP_DIR).join(name),
        }
    }

    /// The file of the world `name` in `data_dir`; `name` passed [`is_world_file_name`].
    pub(crate) fn world(data_dir: &Path, name: &str) -> Self {
        Self {
            file: data_dir.join(WORLD_DIR).join(format!("{name}.{EXTENSION}")),
            backups: data_dir.join(BACKUP_DIR).join(WORLD_DIR).join(name),
        }
    }
}

/// A failure of a data file or of one stored value.
#[derive(Debug)]
pub(crate) enum Error {
    /// A file or folder under the data folder could not be read or written.
    Io(io::Error),
    /// Another process has the data file open.
    InUse,
    /// redb failed: I/O, or a page it found corrupted, which [`is_damage`](Self::is_damage) tells
    /// apart.
    Database(redb::Error),
    /// The data file failed redb's integrity check (or redb repaired it, which means it was
    /// damaged); the source is redb's report, if it gave one.
    DamagedFile(Option<redb::Error>),
    /// A value does not match its checksum.
    Checksum,
    /// A stored value or key is invalid: a value's header, a length that disagrees with its body,
    /// or a key no Anchor writes.
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
    /// `data/worlds/` holds more than [`MAX_WORLDS`] world files, or a new one would make it.
    TooManyWorlds,
    /// [`Storage::access`] found more values than the lists hold.
    TooManyAccessEntries,
    /// [`Snapshot::chunk_ids`] found more chunks than it lists.
    TooManyChunks {
        /// The most it lists.
        limit: usize,
    },
    /// The world's data file is not open: it failed to open, or a replay has no copy of it.
    Closed,
    /// A data file holds a later save than the main file (a save stopped before the main file),
    /// so a path that only reads refuses it: one start of the server rolls it back ([`settle`]).
    Ahead,
    /// A data file holds a later save than the main file, and no savepoint of it rolls it back to
    /// that save or an earlier one: damage.
    NoRollback,
}

impl Error {
    /// A redb failure of any kind, as [`Error::Database`]; [`Error::is_damage`] tells corruption
    /// apart.
    pub(crate) fn database(e: impl Into<redb::Error>) -> Self {
        Self::Database(e.into())
    }

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
                | Self::NoRollback
        )
    }

    /// The error of a failed open of the file.
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
            Self::Io(_) => f.write_str("data file i/o failed"),
            Self::InUse => f.write_str("another process has a data file open"),
            Self::Database(_) => f.write_str("a data file's database failed"),
            Self::DamagedFile(_) => f.write_str("a data file failed its integrity check"),
            Self::Checksum => f.write_str("a stored value does not match its checksum"),
            Self::Malformed(reason) => write!(f, "a stored value is malformed: {reason}"),
            Self::Compression(_) => f.write_str("a stored value does not decompress"),
            Self::Record(_) => f.write_str("a stored record does not decode"),
            Self::NewerFormat { version } => write!(
                f,
                "a stored value has format {version}, newer than {FORMAT_VERSION}"
            ),
            Self::Encode(_) => f.write_str("a record cannot be encoded"),
            Self::TooManyWorlds => write!(
                f,
                "data/{WORLD_DIR} holds or would hold more than {MAX_WORLDS} world files; move the \
                 extra ones out of it"
            ),
            Self::TooManyAccessEntries => {
                write!(
                    f,
                    "more than {MAX_ACCESS} operator, ban and whitelist entries are stored"
                )
            }
            Self::TooManyChunks { limit } => {
                write!(f, "a world file holds more than {limit} chunks")
            }
            Self::Closed => f.write_str("the world's data file is not open"),
            Self::Ahead => f.write_str(
                "a data file holds a later save than the main file; start the server once to roll \
                 it back to the main file's save",
            ),
            Self::NoRollback => f.write_str(
                "a data file holds a later save than the main file and cannot be rolled back to \
                 it",
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::InUse
            | Self::DamagedFile(None)
            | Self::Checksum
            | Self::Malformed(_)
            | Self::NewerFormat { .. }
            | Self::TooManyWorlds
            | Self::TooManyAccessEntries
            | Self::TooManyChunks { .. }
            | Self::Closed
            | Self::Ahead
            | Self::NoRollback => None,
            Self::Database(e) | Self::DamagedFile(Some(e)) => Some(e),
            Self::Compression(e) => Some(e),
            Self::Record(e) | Self::Encode(e) => Some(e),
        }
    }
}

/// A result with the module's [`enum@Error`].
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

/// A kind of chunk extras: the kind byte of its `chunks` keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ChunkKind {
    /// Block entities.
    BlockEntities = 1,
    /// Entities.
    Entities = 2,
}

impl ChunkKind {
    /// Every kind, in key order: a chunk's extras.
    pub(crate) const EXTRAS: [Self; 2] = [Self::BlockEntities, Self::Entities];
}

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
    /// The key of this chunk's `part`.
    pub(crate) fn key(self, part: ChunkPart) -> Key {
        Key::Chunk(ChunkKey { chunk: self, part })
    }

    /// The chunk whose prefix `key` starts with; `None` for a key shorter than a prefix.
    fn from_key(key: &[u8]) -> Option<Self> {
        let [w0, w1, w2, w3, x0, x1, x2, x3, z0, z1, z2, z3] =
            *key.first_chunk::<CHUNK_PREFIX>()?;
        let unflip = |c: [u8; 4]| (u32::from_be_bytes(c) ^ SIGN_BIT).cast_signed();
        Some(Self {
            world: u32::from_be_bytes([w0, w1, w2, w3]),
            x: unflip([x0, x1, x2, x3]),
            z: unflip([z0, z1, z2, z3]),
        })
    }

    /// 12 bytes: world, x and z big-endian with the sign bit flipped (so keys sort by
    /// coordinate).
    fn prefix(self) -> [u8; CHUNK_PREFIX] {
        let flip = |c: i32| (c.cast_unsigned() ^ SIGN_BIT).to_be_bytes();
        let [w0, w1, w2, w3] = self.world.to_be_bytes();
        let [x0, x1, x2, x3] = flip(self.x);
        let [z0, z1, z2, z3] = flip(self.z);
        [w0, w1, w2, w3, x0, x1, x2, x3, z0, z1, z2, z3]
    }
}

/// What a `chunks` value holds, told by the bytes after its chunk's prefix: a kind byte, then
/// for an extras page after the first the page number. Every value stays under
/// [`MAX_RECORD`](seal::MAX_RECORD): the terrain is one value per sub-chunk and an extras list
/// is split into pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ChunkPart {
    /// A page of an extras list ([`ExtraRecord`](codec::ExtraRecord)), kind 1 or 2: page 0
    /// has no more bytes, a later page two more, big-endian.
    Extras(ChunkKind, u16),
    /// The terrain of one sub-chunk ([`SubChunkRecord`](codec::SubChunkRecord)), index 0
    /// (y -64) to 23: kind `0x20 | index`.
    SubChunk(u8),
}

impl ChunkPart {
    /// The part these key bytes after the chunk's prefix name; a key no Anchor writes is damage.
    pub(crate) fn from_suffix(bytes: &[u8]) -> Result<Self> {
        let kind = |k: u8| match k {
            1 => Some(ChunkKind::BlockEntities),
            2 => Some(ChunkKind::Entities),
            _ => None,
        };
        let sub_chunk = |k: u8| {
            k & !SUB_CHUNK_INDEX_MASK == SUB_CHUNK_KIND
                && usize::from(k & SUB_CHUNK_INDEX_MASK) < codec::SUB_CHUNKS
        };
        let part = match *bytes {
            [k] if sub_chunk(k) => Some(Self::SubChunk(k & SUB_CHUNK_INDEX_MASK)),
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
        let mut out = Vec::with_capacity(CHUNK_PREFIX + MAX_CHUNK_SUFFIX);
        out.extend_from_slice(&self.chunk.prefix());
        match self.part {
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
    /// The key of this text, as [`as_str`](Self::as_str) gave it (a logged key).
    pub(crate) fn from_text(text: &str) -> Self {
        Self(text.to_owned())
    }

    /// The key a program names with `text`, an XUID in decimal without leading zeros or
    /// `offline:` and a name ([`check_name`](crate::permission::check_name)); `None` for any other
    /// text.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        if let Some(name) = text.strip_prefix(OFFLINE_PREFIX) {
            return crate::permission::check_name(name)
                .ok()
                .map(|()| Self::offline(name));
        }
        let key = Self::xuid(text.parse().ok()?);
        (key.as_str() == text).then_some(key)
    }

    /// The key of an offline player: names that differ only in ASCII case are one player, as the
    /// tick matches them.
    pub(crate) fn offline(name: &str) -> Self {
        Self(format!("{OFFLINE_PREFIX}{}", name.to_ascii_lowercase()))
    }

    /// The key of a signed-in player.
    pub(crate) fn xuid(xuid: u64) -> Self {
        Self(xuid.to_string())
    }

    /// The key as text.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
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

    /// The kind byte, then the name's bytes.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.name.len() + 1);
        out.push(self.kind as u8);
        out.extend_from_slice(self.name.as_bytes());
        out
    }
}

/// A stored value's place: its table and key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Key {
    /// The `meta` value.
    Meta,
    /// The `worlds` value of a world file: its record.
    World,
    /// A `chunks` value.
    Chunk(ChunkKey),
    /// A `players` value.
    Player(PlayerKey),
    /// An `access` value.
    Access(AccessKey),
    /// A `user` value, by the server data's name.
    User(String),
    /// A deletion mark in the main file's `meta` table, by the world's name
    /// ([`Mark`](codec::Mark)): the save
    /// the main file holds deleted the world, so its file goes, at the latest at the next start.
    Deleted(String),
}

impl Key {
    /// The key's bytes in its table, which the value's seal also covers.
    pub(crate) fn to_bytes(&self) -> Cow<'_, [u8]> {
        match self {
            Self::Meta => Cow::Borrowed(META_KEY),
            Self::World => Cow::Borrowed(WORLD_KEY),
            Self::Chunk(key) => Cow::Owned(key.to_bytes()),
            Self::Player(PlayerKey(key)) => Cow::Borrowed(key.as_bytes()),
            Self::Access(key) => Cow::Owned(key.to_bytes()),
            Self::User(name) => Cow::Borrowed(name.as_bytes()),
            Self::Deleted(name) => Cow::Owned([DELETED_PREFIX, name.as_bytes()].concat()),
        }
    }

    /// The key's table among `tables`, given in the order `meta`, `worlds`, `chunks`, `players`,
    /// `access`, `user`.
    fn pick<'a, T>(&self, tables: &'a [T; 6]) -> &'a T {
        let [meta, worlds, chunks, players, access, user] = tables;
        match self {
            Self::Meta | Self::Deleted(_) => meta,
            Self::World => worlds,
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
            Self::Meta | Self::Deleted(_) => meta,
            Self::World => worlds,
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

impl Batch {
    /// An empty batch with room for `capacity` writes.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            writes: Vec::with_capacity(capacity),
        }
    }

    /// Whether the batch has no writes.
    pub(crate) fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }

    /// The number of writes.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.writes.len()
    }

    /// Deletes the world record and every chunk value the file holds: a world file another world
    /// takes over.
    pub(crate) fn clear(&mut self) {
        self.writes.push(Write::Clear);
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

    /// Stores `sealed` (from [`seal`](seal::seal)) under `key`.
    pub(crate) fn put(&mut self, key: Key, sealed: Vec<u8>) {
        self.writes.push(Write::Put(key, sealed));
    }
}

/// One write of a [`Batch`].
#[derive(Debug)]
enum Write {
    Put(Key, Vec<u8>),
    Delete(Key),
    /// Every page of the chunk's list of the kind, from the page on.
    DeletePages(ChunkId, ChunkKind, u16),
    /// Every value of the `worlds` and `chunks` tables.
    Clear,
}

/// Where the database lives: the data file, or a test's simulated disk.
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
        reason = "off-tick: the save thread opens the data file"
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

/// Whether [`Storage::open`] backs up the file it opened (`backups`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartBackup {
    /// Begin an online backup of the start's first commit ([`Storage::begin_backup`]), which keeps
    /// the [`backup::KEPT`] newest backups once whole. A file that existed before has one; a new
    /// one has nothing to back up.
    Make,
    /// Make none (`backups = false`).
    Skip,
}

/// How [`Storage::open`] starts on the data file: the `backups` and `integrity-check` settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Start {
    pub(crate) backup: StartBackup,
    pub(crate) check: IntegrityCheck,
}

impl Start {
    /// `backup` with a check, as the tests open data files.
    #[cfg(test)]
    pub(crate) const fn checked(backup: StartBackup) -> Self {
        Self {
            backup,
            check: IntegrityCheck::Always,
        }
    }

    /// The start the settings ask for: every open of the data file (the server's and the world
    /// import's) uses it.
    pub(crate) fn of(config: &crate::config::Config) -> Self {
        Self {
            backup: if config.general.backups {
                StartBackup::Make
            } else {
                StartBackup::Skip
            },
            check: config.developer.integrity_check,
        }
    }
}

/// The open data file. Dropping it closes the file without compacting it.
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

impl Storage {
    /// Starts on the data file at `place`: checks the closed file as `start.check` says without
    /// writing to it ([`verify`]), so a damaged file or a newer format is refused with its bytes
    /// as they were; then opens it and, with [`StartBackup::Make`], begins an online backup of
    /// its first commit ([`Storage::begin_backup`]), which the caller steps to its end; a backup
    /// that cannot begin is logged and not made. A missing or empty file starts empty. Nothing is
    /// compacted.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread creates the data folder and opens a data file"
    )]
    pub(crate) fn open(place: &Place, start: Start) -> Result<Self> {
        let (world, backups) = (&place.file, &place.backups);
        if let Some(dir) = world.parent() {
            std::fs::create_dir_all(dir).map_err(Error::Io)?;
        }
        verify(world, start.check)?;
        // A new file has nothing to back up.
        let existed = has_data(world)?;
        let source = Source::File(world.clone());
        let db = source.open()?;
        let mut storage = Self::prepare(source, db)?;
        let path = world.display();
        // redb's lock keeps other processes out, so none is writing a backup of the file. A
        // backup folder that fails costs only backups, as at run time: a stale name left is
        // skipped by the next backup.
        if let Err(e) = backup::remove_stale(backups) {
            error!(
                %path,
                error = &e as &(dyn std::error::Error + 'static),
                "incomplete backups were not removed"
            );
        }
        if !existed {
            // Its first commit flushed the file, not its name in the folder.
            crate::fs::sync_parent(world).map_err(Error::Io)?;
            info!(%path, "data file created");
        } else if start.backup == StartBackup::Make {
            match storage.begin_backup(backups) {
                Ok(_) => debug!(%path, "data file opened; its start backup began"),
                Err(e) => {
                    error!(
                        %path,
                        error = &e as &(dyn std::error::Error + 'static),
                        "start backup failed; no backup file is left"
                    );
                }
            }
        } else {
            debug!(%path, "data file opened");
        }
        Ok(storage)
    }

    /// Opens `path` read-only for a replay: nothing is checked, compacted or backed up, and
    /// commits stay in memory ([`Replica`]). Fails if the file is missing, in use or damaged.
    pub(crate) fn open_replica(path: &Path) -> Result<Self> {
        let db = open_read_only(path)?;
        Ok(Self {
            backup: None,
            check: None,
            db: None,
            source: Source::File(path.to_path_buf()),
            replica: Some(Box::new(Replica {
                db,
                overlay: Overlay::default(),
            })),
        })
    }

    /// Whether an online backup is being made.
    pub(crate) fn is_backing_up(&self) -> bool {
        self.backup.is_some()
    }

    /// Whether a check is being made.
    pub(crate) fn is_checking(&self) -> bool {
        self.check.is_some()
    }

    /// Every stored operator, ban and whitelist entry: its key and sealed value, in key order.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread reads the data file"
    )]
    pub(crate) fn access(&mut self) -> Result<Vec<(AccessKey, Vec<u8>)>> {
        if let Some(replica) = &self.replica {
            let entries = replica.access(MAX_ACCESS)?;
            if entries.len() > MAX_ACCESS {
                return Err(Error::TooManyAccessEntries);
            }
            return entries
                .into_iter()
                .map(|(key, sealed)| Ok((AccessKey::from_bytes(&key)?, sealed)))
                .collect();
        }
        let txn = self.database()?.begin_read().map_err(Error::database)?;
        let Some(table) = open_read(&txn, ACCESS)? else {
            return Ok(Vec::new());
        };
        let count = usize::try_from(table.len().map_err(Error::database)?).unwrap_or(usize::MAX);
        if count > MAX_ACCESS {
            return Err(Error::TooManyAccessEntries);
        }
        let mut entries = Vec::with_capacity(count);
        for entry in table.iter().map_err(Error::database)?.take(MAX_ACCESS) {
            let (key, sealed) = entry.map_err(Error::database)?;
            entries.push((AccessKey::from_bytes(key.value())?, sealed.value().to_vec()));
        }
        Ok(entries)
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

    /// Starts an online backup of the last commit into `dir`, made in steps
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

    /// Starts the world import's write: one quick-repair write transaction that
    /// takes the import's batches as they come and commits once ([`Import::commit`]). Dropping
    /// the [`Import`] uncommitted, or any failure, leaves the file as it was. No retry: the
    /// import's writes are not kept to write again, and a failed import is run again whole.
    pub(crate) fn begin_import(&mut self) -> Result<Import> {
        begin_write(self.database()?).map(Import)
    }

    /// Takes the next step of the check being made, if any; returns what it found once it ends.
    pub(crate) fn check_step(&mut self) -> Option<Checked> {
        let found = self.check.as_mut()?.step();
        if found.is_some() {
            self.check = None;
        }
        found
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

    /// Writes `batch` as [`commit`](Self::commit) does, for a file committed before the main file
    /// in a save (players, worlds): the same write transaction first keeps a savepoint of the file
    /// as it was and deletes the one the previous save kept, so a start rolls the file back if the
    /// main file never got this save ([`settle`]). `batch` writes the meta value of its save. A
    /// retry that finds another meta value than the file held before the first try writes
    /// nothing: that try landed, and a new savepoint would hold this save and leave none of the
    /// one before.
    pub(crate) fn commit_with_savepoint(&mut self, batch: &Batch) -> Result<()> {
        if let Some(replica) = &mut self.replica {
            return replica.commit(batch);
        }
        let before = self.retrying(meta_value)?;
        self.retrying(|db| write_with_savepoint(db, batch, before.as_deref()))
    }

    /// The names of the worlds whose deletion marks ([`Key::Deleted`]) the main file's last commit
    /// holds, in key order. Values are external input: a name that is no world file name, a mark
    /// that does not unseal, or more marks than `data/worlds/` holds files is damage.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the start reads the main file before the threads start"
    )]
    pub(crate) fn deleted_worlds(&mut self) -> Result<Vec<String>> {
        let txn = self.database()?.begin_read().map_err(Error::database)?;
        let Some(table) = open_read(&txn, META)? else {
            return Ok(Vec::new());
        };
        let mut names = Vec::new();
        for entry in table
            .range::<&[u8]>(DELETED_PREFIX..)
            .map_err(Error::database)?
        {
            let (key, sealed) = entry.map_err(Error::database)?;
            let Some(name) = key.value().strip_prefix(DELETED_PREFIX) else {
                break;
            };
            let name = std::str::from_utf8(name)
                .ok()
                .filter(|name| is_world_file_name(name))
                .ok_or(Error::Malformed("a deletion mark names no world"))?;
            unseal::<codec::Mark>(key.value(), sealed.value())?;
            if names.len() == MAX_WORLDS {
                return Err(Error::TooManyWorlds);
            }
            names.push(name.to_owned());
        }
        Ok(names)
    }

    /// Reads `keys` in one read transaction. Each result is the sealed value (open it with
    /// [`unseal`] under [`Key::to_bytes`]), `None` if there is none, or the damage redb found for
    /// that key alone. A read that fails otherwise is retried as a commit is, then returns the
    /// last error.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread reads the data file"
    )]
    pub(crate) fn read(&mut self, keys: &[Key]) -> Result<Vec<Result<Option<Vec<u8>>>>> {
        if let Some(replica) = &self.replica {
            return replica.read(keys);
        }
        self.retrying(|db| {
            let txn = db.begin_read().map_err(Error::database)?;
            read_keys(&txn, keys, None)
        })
    }

    /// Every value of each of `chunks`, in key order, read in one read transaction: its part
    /// and sealed value (open it with [`unseal`] under its key's bytes), or the damage found in
    /// that chunk alone (a key no Anchor writes, or a page redb found corrupted). The key format
    /// bounds the values of a chunk: 24 sub-chunks and 65,536 pages of each extras kind. A read
    /// that fails otherwise is retried as a commit is, then returns the last error.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the save thread reads the data file"
    )]
    pub(crate) fn read_chunks(&mut self, chunks: &[ChunkId]) -> Result<Vec<ChunkValues>> {
        if let Some(replica) = &self.replica {
            return replica.read_chunks(chunks);
        }
        self.retrying(|db| {
            let txn = db.begin_read().map_err(Error::database)?;
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

    /// Deletes the value under `key` from the `players` table in one write transaction, without
    /// reading it, and says whether there was one. It is not retried: a try that landed before
    /// its call failed would answer false on the next.
    pub(crate) fn remove_player(&mut self, key: &PlayerKey) -> Result<bool> {
        remove_player(self.database()?, key)
    }

    /// Drops the check being made, if any, unreported.
    pub(crate) fn stop_check(&mut self) {
        self.check = None;
    }

    /// Deletes the file's savepoint, marks the file closed after a normal stop ([`CLOSED_KEY`])
    /// and closes it, without compacting it (a stop writes what changed and nothing more). Only a
    /// normal stop calls it, once every file holds the last save: a failed one drops the
    /// [`Storage`] instead, so its file has no mark and keeps its savepoint.
    pub(crate) fn close(mut self) -> Result<()> {
        // Nobody waits for its result.
        self.check = None;
        if let Some(db) = self.db.take() {
            mark_closed(&db)?;
        }
        Ok(())
    }

    /// Opens, checks and prepares the database at `source`, as a start that checks it.
    #[cfg(test)]
    fn start(source: Source) -> Result<Self> {
        let mut db = source.open()?;
        check_integrity(&mut db)?;
        Self::prepare(source, db)
    }

    /// Prepares `db`, the database just opened at `source`: its first commit creates the tables a
    /// new file lacks and removes the mark of a normal stop ([`CLOSED_KEY`]), so a stop that does
    /// not reach [`close`](Self::close) leaves none.
    fn prepare(source: Source, db: Database) -> Result<Self> {
        let mut storage = Self {
            backup: None,
            check: None,
            db: Some(db),
            source,
            replica: None,
        };
        storage.retrying(begin_run)?;
        Ok(storage)
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
                    warn!(
                        error = &e as &(dyn std::error::Error + 'static),
                        retry = failures,
                        "a data file call failed; reopening the file"
                    );
                    if let Some(backup) = self.backup.take() {
                        error!(?backup, "backup stopped: a data file is reopened");
                    }
                    if let Some(check) = &mut self.check {
                        check.stop("a data file was reopened after a failure".to_owned());
                    }
                    self.db = None;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// The open database, reopening the file if a failed commit dropped it.
    fn database(&mut self) -> Result<&Database> {
        let db = match self.db.take() {
            Some(db) => db,
            None => self.source.open()?,
        };
        Ok(self.db.insert(db))
    }
}

/// A data file opened read-only for a replay: commits go to memory on
/// top of the file, which is never written, and reads see them as they would see a commit.
struct Replica {
    db: redb::ReadOnlyDatabase,
    /// The values committed since the file was opened.
    overlay: Overlay,
}

impl Replica {
    /// Every `access` value (at most `max`), committed values in their place, in key order.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a replay reads the data file on its own thread"
    )]
    fn access(&self, max: usize) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let txn = self.db.begin_read().map_err(Error::database)?;
        let mut values = std::collections::BTreeMap::new();
        if let Some(table) = open_read(&txn, ACCESS)? {
            for entry in table
                .iter()
                .map_err(Error::database)?
                .take(max.saturating_add(1))
            {
                let (key, value) = entry.map_err(Error::database)?;
                values.insert(key.value().to_vec(), value.value().to_vec());
            }
        }
        let [_, _, _, _, written, _] = &self.overlay;
        for (key, value) in written {
            match value {
                Some(v) => values.insert(key.clone(), v.clone()),
                None => values.remove(key),
            };
        }
        Ok(values.into_iter().collect())
    }

    /// Reads `keys` as [`Storage::read`] does, committed values first.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a replay reads the data file on its own thread"
    )]
    fn read(&self, keys: &[Key]) -> Result<Vec<Result<Option<Vec<u8>>>>> {
        let txn = self.db.begin_read().map_err(Error::database)?;
        read_keys(&txn, keys, Some(&self.overlay))
    }

    /// Every value of each of `chunks` as [`Storage::read_chunks`] gives them, committed values
    /// in their place.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a replay reads the data file on its own thread"
    )]
    fn read_chunks(&self, chunks: &[ChunkId]) -> Result<Vec<ChunkValues>> {
        let txn = self.db.begin_read().map_err(Error::database)?;
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

    /// Applies `batch` to the values in memory, as [`write()`] does to the file.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a replay reads the data file on its own thread"
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
                    let (first, last) = pages_range(chunk, kind, first);
                    let txn = self.db.begin_read().map_err(Error::database)?;
                    let [_, _, written, _, _, _] = &mut self.overlay;
                    if let Some(table) = open_read(&txn, CHUNKS)? {
                        for entry in table
                            .range::<&[u8]>(&*first..=&*last)
                            .map_err(Error::database)?
                        {
                            let (key, _) = entry.map_err(Error::database)?;
                            written.insert(key.value().to_vec(), None);
                        }
                    }
                    for value in written.range_mut(first..=last) {
                        *value.1 = None;
                    }
                }
                Write::Clear => {
                    let txn = self.db.begin_read().map_err(Error::database)?;
                    let [_, worlds, chunks, _, _, _] = &mut self.overlay;
                    for (table, written) in [(WORLDS, worlds), (CHUNKS, chunks)] {
                        for value in written.values_mut() {
                            *value = None;
                        }
                        if let Some(table) = open_read(&txn, table)? {
                            for entry in table.iter().map_err(Error::database)? {
                                let (key, _) = entry.map_err(Error::database)?;
                                written.insert(key.value().to_vec(), None);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
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

/// The world import's open write transaction ([`Storage::begin_import`]). Dropping it uncommitted
/// aborts every write it took.
pub(crate) struct Import(WriteTransaction);

impl Import {
    /// Applies `batch`'s writes, in order, without committing them.
    pub(crate) fn write(&mut self, batch: &Batch) -> Result<()> {
        apply(&self.0, batch)
    }

    /// Commits every write taken, durably and at once.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the import writes the data file before the server runs"
    )]
    pub(crate) fn commit(self) -> Result<()> {
        self.0.commit().map_err(Error::database)?;
        Ok(())
    }
}

impl fmt::Debug for Import {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Import").finish_non_exhaustive()
    }
}

/// A world file opened read-only for an export: every read is of one read transaction, so of
/// one commit. Nothing is written, checked or backed up. A file another process has open is
/// refused ([`Error::InUse`]), and no server can open the world file or the main file for writing
/// while this is open, so no server starts and loads the world meanwhile.
pub(crate) struct Snapshot {
    /// A transaction of `_db`, so declared first to drop first.
    txn: ReadTransaction,
    _db: redb::ReadOnlyDatabase,
    /// The main file, if there is one, held open for the lifetime of the snapshot.
    _main: Option<redb::ReadOnlyDatabase>,
}

impl Snapshot {
    /// Opens the world file `path` of the data folder whose main file is `main`. Fails if it is
    /// missing, in use, damaged, left by a crash for redb to repair, or later than the main file
    /// ([`check_settled`]): a start of the server repairs and rolls it back.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the export reads a world file instead of running the server"
    )]
    pub(crate) fn open(path: &Path, main: &Path) -> Result<Self> {
        let held = if has_data(main)? {
            Some(open_read_only(main)?)
        } else {
            None
        };
        check_settled(path, main)?;
        let db = open_read_only(path)?;
        let txn = db.begin_read().map_err(Error::database)?;
        Ok(Self {
            txn,
            _db: db,
            _main: held,
        })
    }

    /// The chunks that hold values of the world numbered `world`, in key order;
    /// [`Error::TooManyChunks`] past `max`. A key too short for a chunk is damage.
    pub(crate) fn chunk_ids(&self, world: u32, max: usize) -> Result<Vec<ChunkId>> {
        let Some(table) = open_read(&self.txn, CHUNKS)? else {
            return Ok(Vec::new());
        };
        let first = ChunkId {
            world,
            x: i32::MIN,
            z: i32::MIN,
        };
        let last = ChunkId {
            world,
            x: i32::MAX,
            z: i32::MAX,
        };
        let (first, _) = chunk_range(first);
        let (_, last) = chunk_range(last);
        let mut ids: Vec<ChunkId> = Vec::new();
        for entry in table
            .range::<&[u8]>(&*first..=&*last)
            .map_err(Error::database)?
        {
            let (key, _) = entry.map_err(Error::database)?;
            let id = ChunkId::from_key(key.value())
                .ok_or(Error::Malformed("a chunk key no anchor writes"))?;
            if ids.last() == Some(&id) {
                continue;
            }
            if ids.len() == max {
                return Err(Error::TooManyChunks { limit: max });
            }
            ids.push(id);
        }
        Ok(ids)
    }

    /// Every value of `chunk`, in key order, as [`Storage::read_chunks`] gives them.
    pub(crate) fn read_chunk(&self, chunk: ChunkId) -> ChunkValues {
        match open_read(&self.txn, CHUNKS)? {
            Some(table) => read_chunk(&table, chunk),
            None => Ok(Vec::new()),
        }
    }

    /// The sealed world record, `None` if the file holds none.
    pub(crate) fn world(&self) -> Result<Option<Vec<u8>>> {
        let Some(table) = open_read(&self.txn, WORLDS)? else {
            return Ok(None);
        };
        Ok(table
            .get(WORLD_KEY)
            .map_err(Error::database)?
            .map(|v| v.value().to_vec()))
    }
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot").finish_non_exhaustive()
    }
}

/// What [`settle`] did to a data file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Settled {
    /// The file holds the main file's save or an earlier one, and is left as it was.
    Kept,
    /// The file held a later save and is rolled back to its savepoint, which holds an earlier one.
    RolledBack,
    /// The file held a later save, and its savepoint holds no save and no world record: the
    /// later save made the file. It is removed.
    Removed,
}

/// What [`Storage::read_chunks`] gives for one chunk.
pub(crate) type ChunkValues = Result<Vec<(ChunkPart, Vec<u8>)>>;

/// Reads `keys` in `txn` as [`Storage::read`] gives them, the values of a replay's `overlay`
/// first.
fn read_keys(
    txn: &ReadTransaction,
    keys: &[Key],
    overlay: Option<&Overlay>,
) -> Result<Vec<Result<Option<Vec<u8>>>>> {
    let tables = [
        open_read(txn, META)?,
        open_read(txn, WORLDS)?,
        open_read(txn, CHUNKS)?,
        open_read(txn, PLAYERS)?,
        open_read(txn, ACCESS)?,
        open_read(txn, USER)?,
    ];
    let mut values = Vec::with_capacity(keys.len());
    for key in keys {
        let bytes = key.to_bytes();
        if let Some(value) = overlay.and_then(|overlay| key.pick(overlay).get(&*bytes)) {
            values.push(Ok(value.clone()));
            continue;
        }
        let value = match key.pick(&tables) {
            Some(table) => table
                .get(&*bytes)
                .map(|value| value.map(|v| v.value().to_vec()))
                .map_err(Error::database),
            None => Ok(None),
        };
        // Damage stays with its key; any other failure (I/O) is the read's, retried.
        match value {
            Err(e) if !e.is_damage() => return Err(e),
            value => values.push(value),
        }
    }
    Ok(values)
}

/// The values of one chunk, in key order.
fn read_chunk(table: &ReadOnlyTable<&'static [u8], &'static [u8]>, chunk: ChunkId) -> ChunkValues {
    let (first, last) = chunk_range(chunk);
    let mut values = Vec::new();
    for entry in table
        .range::<&[u8]>(&*first..=&*last)
        .map_err(Error::database)?
    {
        let (key, value) = entry.map_err(Error::database)?;
        let part = ChunkPart::from_suffix(key.value().get(CHUNK_PREFIX..).unwrap_or_default())?;
        values.push((part, value.value().to_vec()));
    }
    Ok(values)
}

/// The first and last key bytes of `chunk`'s values.
fn chunk_range(chunk: ChunkId) -> (Vec<u8>, Vec<u8>) {
    let prefix = chunk.prefix();
    let mut first = prefix.to_vec();
    first.push(0);
    let mut last = prefix.to_vec();
    last.extend_from_slice(&[0xff; MAX_CHUNK_SUFFIX]);
    (first, last)
}

/// Runs redb's integrity check on the file just opened and refuses a damaged file, and one redb
/// repaired (which was damaged).
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread checks the data file at start"
)]
fn check_integrity(db: &mut Database) -> Result<()> {
    #[cfg(test)]
    tests::CHECKS.set(tests::CHECKS.get() + 1);
    match db.check_integrity() {
        Ok(true) => Ok(()),
        // redb repaired the file in place: it was damaged.
        Ok(false) => Err(Error::DamagedFile(None)),
        Err(e @ DatabaseError::Storage(StorageError::Corrupted(_))) => {
            Err(Error::DamagedFile(Some(e.into())))
        }
        Err(e) => Err(Error::database(e)),
    }
}

/// Refuses the opened file `db` if its last commit was written by a newer Anchor (its meta
/// value's format) or its meta value is damaged.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread reads the meta value of a data file at start"
)]
fn check_format(db: &Database) -> Result<()> {
    let txn = db.begin_read().map_err(Error::database)?;
    committed(&txn)?;
    Ok(())
}

/// Refuses, as [`Error::Ahead`], the data file `file` when it holds a later save than the main
/// file `main` ([`is_later`]), both read without writing: a path that only reads (a replay, the
/// export) cannot roll it back as a start does ([`settle`]). A missing or empty file holds no
/// save, and a missing or empty main file none to compare with, as for a start.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: a replay and the export read data files without writing them"
)]
pub(crate) fn check_settled(file: &Path, main: &Path) -> Result<()> {
    if !has_data(file)? || !has_data(main)? {
        return Ok(());
    }
    let saved = committed(
        &open_read_only(main)?
            .begin_read()
            .map_err(Error::database)?,
    )?;
    let meta = committed(
        &open_read_only(file)?
            .begin_read()
            .map_err(Error::database)?,
    )?;
    let saved = saved.unwrap_or(Meta {
        tick: 0,
        identity: None,
    });
    if meta.is_some_and(|meta| is_later(&meta, &saved)) {
        return Err(Error::Ahead);
    }
    Ok(())
}

/// Whether a start checks the closed data file `world` when `integrity-check` is `check`.
fn checks(world: &Path, check: IntegrityCheck) -> bool {
    match check {
        IntegrityCheck::Always => true,
        IntegrityCheck::AfterUncleanShutdown => !is_closed_normally(world),
        IntegrityCheck::Never => false,
    }
}

/// Whether the closed data file `world` has the mark of a normal stop ([`CLOSED_KEY`]), read
/// without writing to it. A file that cannot be opened read-only (missing, damaged, in use, or
/// left for redb to repair) has none: the start then checks it, and the check meets the fault.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread reads the data file's mark once, before opening it"
)]
pub(crate) fn is_closed_normally(world: &Path) -> bool {
    let Ok(db) = open_read_only(world) else {
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

/// Opens redb read-only on `path`, writing nothing: a file another process has open is refused
/// ([`Error::InUse`]), as is one a crash left for redb to repair.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: a replay, the export and the start read a data file without writing it"
)]
fn open_read_only(path: &Path) -> Result<redb::ReadOnlyDatabase> {
    let mut builder = Builder::new();
    builder.set_cache_size(CACHE_BYTES);
    builder.open_read_only(path).map_err(Error::from_open)
}

/// Whether `file` exists and holds bytes: a missing or empty file is a new one.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the start looks at a data file before it opens it"
)]
pub(crate) fn has_data(file: &Path) -> Result<bool> {
    match std::fs::metadata(file) {
        Ok(metadata) => Ok(metadata.len() > 0),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Brings the closed players or world file `file` back to the main file's last save `saved`, at
/// start, before the file opens: a save commits every other file before the main file, so a save
/// that stopped between them leaves files later than the main file. Such a file is rolled back to
/// the savepoint its last commit kept ([`Storage::commit_with_savepoint`]), or removed when that
/// commit made it. The roll back is tried first on a backend that writes nothing
/// ([`check::open_overlay`]): a file without a savepoint, or whose savepoint is later than `saved`
/// too, is refused as [`Error::NoRollback`] with every byte as it was. Each step lands whole, so
/// a start stopped in the middle leaves the file as it was or as the step left it, and the next
/// start ends the same.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the start rolls a data file back before it opens it"
)]
pub(crate) fn settle(file: &Path, saved: &Meta) -> Result<Settled> {
    let Some(overlay) = check::open_overlay(file)? else {
        return Ok(Settled::Kept);
    };
    let Some((_, made)) = roll_back(&overlay, saved)? else {
        return Ok(Settled::Kept);
    };
    drop(overlay);
    let shown = file.display();
    if made {
        std::fs::remove_file(file).map_err(Error::Io)?;
        crate::fs::sync_parent(file).map_err(Error::Io)?;
        warn!(path = %shown, "data file removed: a save that stopped before the main file made it");
        return Ok(Settled::Removed);
    }
    let db = Source::File(file.to_path_buf()).open()?;
    // The file is the one just tried: redb's lock kept other processes out until the overlay
    // dropped, and the start takes the lock again here.
    let Some((txn, _)) = roll_back(&db, saved)? else {
        return Ok(Settled::Kept);
    };
    txn.commit().map_err(Error::database)?;
    warn!(
        path = %shown,
        main_save_tick = saved.tick,
        "data file rolled back to the main file's save: a save stopped before the main file"
    );
    Ok(Settled::RolledBack)
}

/// Begins the write transaction that rolls `db` back to its savepoint, if its last save is later
/// than `saved`, and tells whether the savepoint holds neither a save nor a world record (a save
/// made the file); dropping it leaves the file as it was. [`Error::NoRollback`] when there is no
/// savepoint or it holds a save later than `saved` too.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the start reads a data file before it opens it"
)]
fn roll_back(db: &Database, saved: &Meta) -> Result<Option<(WriteTransaction, bool)>> {
    let meta = committed(&db.begin_read().map_err(Error::database)?)?;
    if !meta.is_some_and(|meta| is_later(&meta, saved)) {
        return Ok(None);
    }
    let mut txn = begin_write(db)?;
    let id = txn
        .list_persistent_savepoints()
        .map_err(Error::database)?
        .max()
        .ok_or(Error::NoRollback)?;
    let savepoint = txn.get_persistent_savepoint(id).map_err(Error::database)?;
    txn.restore_savepoint(&savepoint).map_err(Error::database)?;
    drop(savepoint);
    let restored = match txn
        .open_table(META)
        .map_err(Error::database)?
        .get(META_KEY)
        .map_err(Error::database)?
    {
        Some(sealed) => Some(unseal::<Meta>(META_KEY, sealed.value())?),
        None => None,
    };
    if restored.is_some_and(|meta| is_later(&meta, saved)) {
        return Err(Error::NoRollback);
    }
    let made = restored.is_none()
        && txn
            .open_table(WORLDS)
            .map_err(Error::database)?
            .get(WORLD_KEY)
            .map_err(Error::database)?
            .is_none();
    Ok(Some((txn, made)))
}

/// The meta value of the commit `txn` reads, if the file holds one. A newer format fails as
/// [`Error::NewerFormat`] ([`unseal`]).
fn committed(txn: &ReadTransaction) -> Result<Option<Meta>> {
    let Some(table) = open_read(txn, META)? else {
        return Ok(None);
    };
    let Some(sealed) = table.get(META_KEY).map_err(Error::database)? else {
        return Ok(None);
    };
    unseal::<Meta>(META_KEY, sealed.value()).map(Some)
}

/// Whether `meta`, a data file's last save, is later than `saved`, the main file's: a larger
/// tick, or the same tick with another commit identity. A file no save wrote (no identity) is not
/// later at the same tick.
pub(crate) fn is_later(meta: &Meta, saved: &Meta) -> bool {
    meta.tick > saved.tick
        || (meta.tick == saved.tick && meta.identity.is_some() && meta.identity != saved.identity)
}

/// Makes the first commit of a start: creates the tables a new file lacks and removes the mark
/// of a normal stop ([`CLOSED_KEY`]).
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread writes the data file at start"
)]
fn begin_run(db: &Database) -> Result<()> {
    let txn = begin_write(db)?;
    apply(&txn, &Batch::default())?;
    drop(
        txn.open_table(META)
            .map_err(Error::database)?
            .remove(CLOSED_KEY)
            .map_err(Error::database)?,
    );
    txn.commit().map_err(Error::database)?;
    Ok(())
}

/// Makes the last commit of a normal stop: deletes the savepoint, which no start needs once every
/// file holds the last save, then writes the mark of a normal stop ([`CLOSED_KEY`]).
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread writes the data file at a normal stop"
)]
fn mark_closed(db: &Database) -> Result<()> {
    let txn = begin_write(db)?;
    for id in txn.list_persistent_savepoints().map_err(Error::database)? {
        txn.delete_persistent_savepoint(id)
            .map_err(Error::database)?;
    }
    drop(
        txn.open_table(META)
            .map_err(Error::database)?
            .insert(CLOSED_KEY, &[][..])
            .map_err(Error::database)?,
    );
    txn.commit().map_err(Error::database)?;
    Ok(())
}

/// Begins a write transaction of `db` with quick repair, as every write transaction of a data
/// file has.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread and the import write the data file"
)]
fn begin_write(db: &Database) -> Result<WriteTransaction> {
    let mut txn = db.begin_write().map_err(Error::database)?;
    txn.set_quick_repair(true);
    Ok(txn)
}

/// Writes all of `batch` in one write transaction.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread writes the data file"
)]
fn write(db: &Database, batch: &Batch) -> Result<()> {
    let txn = begin_write(db)?;
    apply(&txn, batch)?;
    txn.commit().map_err(Error::database)?;
    Ok(())
}

/// Writes as [`write()`] does, keeping a savepoint of the file as it was, unless the file's meta
/// value is no longer `before`, the one it held before the commit's first try: that try landed
/// before its call failed.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread writes the data file"
)]
fn write_with_savepoint(db: &Database, batch: &Batch, before: Option<&[u8]>) -> Result<()> {
    if meta_value(db)?.as_deref() != before {
        return Ok(());
    }
    let txn = begin_write(db)?;
    // Before any table opens: redb keeps a savepoint only of a transaction that changed nothing.
    let kept = txn.persistent_savepoint().map_err(Error::database)?;
    for id in txn.list_persistent_savepoints().map_err(Error::database)? {
        if id != kept {
            txn.delete_persistent_savepoint(id)
                .map_err(Error::database)?;
        }
    }
    apply(&txn, batch)?;
    txn.commit().map_err(Error::database)?;
    Ok(())
}

/// Deletes the `players` value under `key` in one write transaction, and tells whether there was
/// one.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the program deletes a player record while no server runs"
)]
fn remove_player(db: &Database, key: &PlayerKey) -> Result<bool> {
    let txn = begin_write(db)?;
    let existed = txn
        .open_table(PLAYERS)
        .map_err(Error::database)?
        .remove(key.as_str().as_bytes())
        .map_err(Error::database)?
        .is_some();
    txn.commit().map_err(Error::database)?;
    Ok(existed)
}

/// The sealed meta value of the last commit of `db`, if it holds one.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread reads the data file"
)]
fn meta_value(db: &Database) -> Result<Option<Vec<u8>>> {
    let txn = db.begin_read().map_err(Error::database)?;
    let Some(table) = open_read(&txn, META)? else {
        return Ok(None);
    };
    Ok(table
        .get(META_KEY)
        .map_err(Error::database)?
        .map(|v| v.value().to_vec()))
}

/// Applies `batch`'s writes, in order, to an open write transaction.
fn apply(txn: &WriteTransaction, batch: &Batch) -> Result<()> {
    let open = |table| txn.open_table(table).map_err(Error::database);
    let mut tables = [
        open(META)?,
        open(WORLDS)?,
        open(CHUNKS)?,
        open(PLAYERS)?,
        open(ACCESS)?,
        open(USER)?,
    ];
    for write in &batch.writes {
        match write {
            Write::Put(key, value) => {
                let table = key.pick_mut(&mut tables);
                drop(
                    table
                        .insert(&*key.to_bytes(), value.as_slice())
                        .map_err(Error::database)?,
                );
            }
            Write::Delete(key) => {
                let table = key.pick_mut(&mut tables);
                drop(table.remove(&*key.to_bytes()).map_err(Error::database)?);
            }
            &Write::DeletePages(chunk, kind, first) => {
                let key = chunk.key(ChunkPart::Extras(kind, first));
                let (first, last) = pages_range(chunk, kind, first);
                key.pick_mut(&mut tables)
                    .retain_in::<&[u8], _>(&*first..=&*last, |_, _| false)
                    .map_err(Error::database)?;
            }
            Write::Clear => {
                let [_, worlds, chunks, _, _, _] = &mut tables;
                worlds.retain(|_, _| false).map_err(Error::database)?;
                chunks.retain(|_, _| false).map_err(Error::database)?;
            }
        }
    }
    Ok(())
}

/// The first and last key bytes of `chunk`'s extras pages of `kind` from page `first` on.
fn pages_range(chunk: ChunkId, kind: ChunkKind, first: u16) -> (Vec<u8>, Vec<u8>) {
    let page = |page| {
        ChunkKey {
            chunk,
            part: ChunkPart::Extras(kind, page),
        }
        .to_bytes()
    };
    (page(first), page(u16::MAX))
}

/// Opens `table` for reading; `None` before the first commit of a new file created it.
fn open_read(
    txn: &ReadTransaction,
    table: Table,
) -> Result<Option<ReadOnlyTable<&'static [u8], &'static [u8]>>> {
    match txn.open_table(table) {
        Ok(table) => Ok(Some(table)),
        Err(TableError::TableDoesNotExist(_)) => Ok(None),
        Err(e) => Err(Error::database(e)),
    }
}

/// Whether `name` is a world name Anchor can keep as a file name on Windows and Linux alike
/// (`data/worlds/<name>.redb`), by the rule of
/// [`world::Error::InvalidName`](crate::world::Error::InvalidName) with the length limit
/// [`MAX_FILE_NAME`]. Names that differ only in letter case name one file on Windows, so they
/// are one world on every system ([`world_name_key`]).
pub(crate) fn is_world_file_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > MAX_FILE_NAME
        || name.starts_with(' ')
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || FORBIDDEN.contains(&c))
    {
        return false;
    }
    let base = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches(' ')
        .to_ascii_lowercase();
    let device = match base.as_str() {
        "con" | "prn" | "aux" | "nul" | "conin$" | "conout$" => true,
        _ => base
            .strip_prefix("com")
            .or_else(|| base.strip_prefix("lpt"))
            .is_some_and(|n| {
                let mut chars = n.chars();
                matches!(
                    (chars.next(), chars.next()),
                    (Some('0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'), None)
                )
            }),
    };
    !device
}

/// What world names that name one file have in common: the name in one letter case.
pub(crate) fn world_name_key(name: &str) -> String {
    name.chars()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .collect()
}

/// The copies of the data file at `place` a replay may start from: the file, then its backups
/// newest first. Missing ones are left out.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: a replay lists the data files before it starts"
)]
pub(crate) fn copies(place: &Place) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if std::fs::exists(&place.file).map_err(Error::Io)? {
        files.push(place.file.clone());
    }
    let mut backups = backup::backups(&place.backups)?;
    backups.sort_unstable();
    files.extend(backups.into_iter().rev().map(|(_, path)| path));
    Ok(files)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::sync::Arc;

    use redb::StorageBackend;

    use super::codec::tests::{empty_chunk, random_chunk, random_compound, terrain_chunk};
    use super::codec::{
        BanRecord, ChunkRecord, ExtraRecord, Mark, PlayerRecord, SubChunkRecord, UserRecord,
        WorldRecord,
    };
    use super::seal::seal;
    use crate::clock;
    use crate::protocol::Rng;

    thread_local! {
        /// The integrity checks this thread's starts made.
        pub(crate) static CHECKS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
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
        /// The bytes reads see: what a process killed now leaves in its file.
        pub(crate) fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().current.clone()
        }

        pub(crate) fn failed(&self) -> u64 {
            self.0.lock().unwrap().failed
        }

        /// Arms `fault`, counting calls from zero.
        pub(crate) fn arm(&self, fault: Fault) {
            let mut state = self.0.lock().unwrap();
            state.fault = fault;
            state.calls = [0; 4];
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

        pub(crate) fn backend(&self) -> Backend {
            Backend(self.clone())
        }
    }

    #[derive(Debug)]
    pub(crate) struct Backend(Disk);

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

    fn injected() -> io::Error {
        io::Error::other("injected fault")
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
            Key::World => drop(unseal::<WorldRecord>(&bytes, sealed).unwrap()),
            Key::Chunk(ChunkKey { part, .. }) => match part {
                ChunkPart::SubChunk(_) => drop(unseal::<SubChunkRecord>(&bytes, sealed).unwrap()),
                ChunkPart::Extras(..) => drop(unseal::<ExtraRecord<'_>>(&bytes, sealed).unwrap()),
            },
            Key::Player(_) => drop(unseal::<PlayerRecord>(&bytes, sealed).unwrap()),
            Key::Access(AccessKey {
                kind: AccessKind::Ban | AccessKind::IpBan,
                ..
            }) => drop(unseal::<BanRecord>(&bytes, sealed).unwrap()),
            Key::User(_) => drop(unseal::<UserRecord<'_>>(&bytes, sealed).unwrap()),
            Key::Access(_) | Key::Deleted(_) => drop(unseal::<Mark>(&bytes, sealed).unwrap()),
        }
    }

    fn chunk(x: i32, z: i32) -> ChunkId {
        ChunkId { world: 0, x, z }
    }

    fn chunk_key(x: i32, z: i32) -> Key {
        chunk(x, z).key(ChunkPart::SubChunk(0))
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
                part: ChunkPart::SubChunk(0),
            }
            .to_bytes()
        };
        assert_eq!(
            key(1, -1, 2),
            [0, 0, 0, 1, 0x7f, 0xff, 0xff, 0xff, 0x80, 0, 0, 2, 0x20]
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

    /// The bytes after a chunk's prefix: extras pages kind then page (none for page 0),
    /// sub-chunks `0x20 | index`; anything else is a key no Anchor writes. Within a chunk, pages
    /// sort in order.
    #[test]
    fn chunk_parts_have_one_key_each() {
        let parts = [
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
                ChunkPart::Extras(ChunkKind::Entities, u16::MAX),
                vec![2, 0xff, 0xff],
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
            &[0],
            &[3],
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
            part: ChunkPart::SubChunk(0),
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
                .insert(key, &[1_u8][..])
                .unwrap(),
        );
        txn.commit().unwrap();
    }

    #[test]
    fn values_round_trip_through_a_commit_and_a_reopen() {
        let disk = Disk::default();
        let mut storage = memory(&disk);
        let mut rng = Rng(11);
        let sub_chunk = random_chunk(&mut rng).sub_chunks[5].clone();
        let world = WorldRecord {
            number: 3,
            time: 0,
            spawn: crate::world::BlockPos::new(0, -60, 0),
        };
        let keys = [
            chunk_key(-1, 1),
            Key::World,
            Key::Meta,
            Key::Player(PlayerKey::xuid(7)),
        ];
        let mut batch = Batch::with_capacity(4);
        batch.put(
            keys[0].clone(),
            seal(&keys[0].to_bytes(), &sub_chunk).unwrap(),
        );
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
        let unsealed: SubChunkRecord =
            unseal(&keys[0].to_bytes(), &chunk_value.unwrap().unwrap()).unwrap();
        assert_eq!(unsealed, sub_chunk);
        let world_value = world_value.unwrap().unwrap();
        assert_eq!(
            unseal::<WorldRecord>(WORLD_KEY, &world_value).unwrap(),
            world
        );
        assert_eq!(
            unseal::<Meta>(META_KEY, &meta_value.unwrap().unwrap()).unwrap(),
            Meta::at(40)
        );
        assert!(player_value.unwrap().is_none());
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

    /// A crash at a random call of a commit (or of the mark commit when closing after it), then a
    /// power loss: the file passes the integrity check and holds exactly the previous commit or
    /// the new one (always the new one if the commit returned). 1,000 runs, fixed seed.
    #[test]
    fn a_crash_at_any_point_of_a_commit_keeps_one_whole_commit() {
        let mut rng = Rng(0x00c0_ffee);
        let (mut old, mut new) = (0, 0);
        for run in 0..1000 {
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
            // One run in four commits and crashes in the mark commit of the close instead.
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

        /// The data file the tests open.
        fn place(&self) -> Place {
            Place::data(&self.0, MAIN_FILE)
        }

        fn world(&self) -> Vec<u8> {
            std::fs::read(self.place().file).unwrap()
        }

        /// The file's length; readable while the file is open, unlike its bytes on Windows.
        fn world_len(&self) -> u64 {
            std::fs::metadata(self.place().file).unwrap().len()
        }

        fn set_world(&self, bytes: &[u8]) {
            std::fs::write(self.place().file, bytes).unwrap();
        }

        /// Backup names, oldest first.
        fn backups(&self) -> Vec<String> {
            let Ok(entries) = std::fs::read_dir(self.place().backups) else {
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

    /// Opens the file of `dir` as a checking start with `backup` does, and steps its start backup,
    /// if one began, to its end.
    fn start_whole(dir: &TempDir, backup: StartBackup) -> Storage {
        let mut storage = Storage::open(&dir.place(), Start::checked(backup)).unwrap();
        while storage.is_backing_up() {
            storage.backup_step().unwrap();
        }
        storage
    }

    #[test]
    fn a_new_folder_starts_empty_and_keeps_what_was_committed() {
        let dir = TempDir::new("new");
        let mut storage = Storage::open(&dir.place(), Start::checked(StartBackup::Make)).unwrap();
        assert!(
            !storage.is_backing_up(),
            "a new file has nothing to back up"
        );
        assert!(
            storage.read(&[Key::Meta]).unwrap()[0]
                .as_ref()
                .unwrap()
                .is_none()
        );
        let mut batch = Batch::default();
        batch.put(Key::Meta, seal(META_KEY, &Meta::at(9)).unwrap());
        storage.commit(&batch).unwrap();
        storage.close().unwrap();
        assert!(dir.backups().is_empty());
        let mut storage = start_whole(&dir, StartBackup::Make);
        let meta = storage
            .read(&[Key::Meta])
            .unwrap()
            .remove(0)
            .unwrap()
            .unwrap();
        assert_eq!(unseal::<Meta>(META_KEY, &meta).unwrap(), Meta::at(9));
        let backups = dir.backups();
        assert_eq!(backups.len(), 1);
        // The start backup holds the file's last save.
        let backup = dir.place().backups.join(&backups[0]);
        assert_eq!(crate::save::committed_meta(&backup).unwrap().tick, 9);
    }

    #[test]
    fn a_second_process_cannot_open_the_file() {
        let dir = TempDir::new("in-use");
        start_whole(&dir, StartBackup::Make).close().unwrap();
        let storage = start_whole(&dir, StartBackup::Make);
        let backups = dir.backups();
        assert!(matches!(
            Storage::open(&dir.place(), Start::checked(StartBackup::Make)),
            Err(Error::InUse)
        ));
        assert_eq!(dir.backups(), backups, "no backup of a file in use");
        storage.close().unwrap();
    }

    #[test]
    fn backups_are_capped_at_three_and_never_overwritten() {
        let dir = TempDir::new("backups");
        start_whole(&dir, StartBackup::Make).close().unwrap();
        let mut made = Vec::new();
        for _ in 0..5 {
            start_whole(&dir, StartBackup::Make).close().unwrap();
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
        let dir = TempDir::new("no-backups");
        for _ in 0..2 {
            let storage = Storage::open(&dir.place(), Start::checked(StartBackup::Skip)).unwrap();
            assert!(!storage.is_backing_up());
            storage.close().unwrap();
            assert!(dir.backups().is_empty(), "{:?}", dir.backups());
        }
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_start_removes_what_a_killed_backup_left() {
        let dir = TempDir::new("backup-killed");
        start_whole(&dir, StartBackup::Skip).close().unwrap();
        let backups = dir.place().backups;
        // A process killed while it made a backup leaves its temporary file.
        std::fs::create_dir_all(&backups).unwrap();
        std::fs::write(backups.join("1.redb.tmp"), b"partial").unwrap();
        start_whole(&dir, StartBackup::Skip).close().unwrap();
        let left: Vec<_> = std::fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_backup_folder_that_fails_costs_only_the_start_backup() {
        let dir = TempDir::new("backup-folder-fails");
        start_whole(&dir, StartBackup::Skip).close().unwrap();
        let backups = dir.place().backups;
        // A file where the folder should be fails both cleaning it and backing up into it.
        std::fs::create_dir_all(backups.parent().unwrap()).unwrap();
        std::fs::write(&backups, b"not a folder").unwrap();
        let storage = Storage::open(&dir.place(), Start::checked(StartBackup::Make)).unwrap();
        assert!(!storage.is_backing_up());
        storage.close().unwrap();
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_start_and_a_stop_never_compact_the_file() {
        let dir = TempDir::new("start-compact");
        let len = |path: &Path| std::fs::metadata(path).unwrap().len();
        let world = dir.place().file;
        let mut storage = Storage::open(&dir.place(), Start::checked(StartBackup::Make)).unwrap();
        let mut rng = Rng(3);
        let mut model = BTreeMap::new();
        storage.commit(&large_batch(&mut rng, 4, 1)).unwrap();
        // Values written again free the pages of the old ones, which a compaction would cut.
        storage
            .commit(&random_batch(&mut rng, &mut model, 2))
            .unwrap();
        storage.close().unwrap();
        let closed = len(&world);
        for _ in 0..2 {
            Storage::open(&dir.place(), Start::checked(StartBackup::Make))
                .unwrap()
                .close()
                .unwrap();
            assert_eq!(len(&world), closed);
        }
    }

    #[test]
    fn a_damaged_file_is_refused_and_left_as_it_was() {
        for start in [StartBackup::Make, StartBackup::Skip] {
            refuses_a_damaged_file(start);
        }
    }

    fn refuses_a_damaged_file(start: StartBackup) {
        let dir = TempDir::new("damaged");
        let bytes = damage_a_closed_file(&dir);
        let e = Storage::open(&dir.place(), Start::checked(start)).unwrap_err();
        assert!(e.is_damage(), "{e:?}");
        assert!(dir.world().eq(&bytes), "the file is left as it was");
        assert!(dir.backups().is_empty(), "a refused file has no backup");
        // A second try finds the same file and refuses it the same way.
        assert!(
            Storage::open(&dir.place(), Start::checked(start))
                .unwrap_err()
                .is_damage()
        );
        assert!(dir.world().eq(&bytes), "the file is unchanged");
    }

    /// Flips a bit in the middle of `sealed`'s body wherever `bytes` holds it; returns how often.
    fn damage_value(bytes: &mut [u8], sealed: &[u8]) -> usize {
        let body = &sealed[12..76];
        let mut hits = 0;
        for at in 0..bytes.len() - body.len() {
            if bytes[at..].starts_with(body) {
                bytes[at + 32] ^= 0x20;
                hits += 1;
            }
        }
        hits
    }

    /// Writes a player to a new data file in `dir`, closes it after a normal stop and damages
    /// the player's bytes, which only redb's integrity check reads at start; returns the bytes.
    fn damage_a_closed_file(dir: &TempDir) -> Vec<u8> {
        let mut storage = Storage::open(&dir.place(), Start::checked(StartBackup::Make)).unwrap();
        // Random letters compress little, so the value's body is long enough to be found.
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
        let sealed = seal(&key.to_bytes(), &player).unwrap();
        let mut batch = Batch::default();
        batch.put(key.clone(), sealed.clone());
        storage.commit(&batch).unwrap();
        storage.close().unwrap();
        let mut bytes = dir.world();
        assert!(damage_value(&mut bytes, &sealed) > 0);
        dir.set_world(&bytes);
        bytes
    }

    /// A file a killed process left (redb recovers it at its next open) passes the start's check
    /// without a byte of it written, in little memory, and starts; with a damaged value it is
    /// refused and left as it was.
    #[test]
    fn a_file_left_by_a_crash_starts_and_one_damaged_is_refused() {
        let dir = TempDir::new("crash-left");
        let disk = Disk::default();
        let mut storage = memory(&disk);
        let mut batch = large_batch(&mut Rng(6), 2, 1);
        let key = Key::User("large-100".to_owned());
        let sealed = batch_value(&batch, &key);
        storage.commit(&batch).unwrap();
        batch = large_batch(&mut Rng(7), 1, 2);
        storage.commit(&batch).unwrap();
        // The process is killed here: the file has no mark of a normal stop, and its header asks
        // the next open to recover it.
        let crashed = disk.bytes();
        drop(storage);
        dir.set_world(&crashed);
        check::tests::WRITTEN_PEAK.set(0);
        verify(&dir.place().file, IntegrityCheck::Always).unwrap();
        assert!(dir.world().eq(&crashed), "the check wrote to the file");
        let written = check::tests::WRITTEN_PEAK.get();
        assert!(written <= 64 << 10, "{written} bytes of writes kept");
        let storage = start_whole(&dir, StartBackup::Make);
        assert_eq!(dir.backups().len(), 1);
        storage.close().unwrap();
        // Damage in it is found as in a file closed normally.
        let mut damaged = crashed;
        assert!(damage_value(&mut damaged, &sealed) > 0);
        dir.set_world(&damaged);
        let e = Storage::open(&dir.place(), Start::checked(StartBackup::Make)).unwrap_err();
        assert!(e.is_damage(), "{e:?}");
        assert!(dir.world().eq(&damaged), "the file is left as it was");
        assert_eq!(dir.backups().len(), 1, "a refused file gets no backup");
    }

    /// The sealed value `batch` puts under `key`.
    fn batch_value(batch: &Batch, key: &Key) -> Vec<u8> {
        batch
            .writes
            .iter()
            .find_map(|write| match write {
                Write::Put(k, value) if k == key => Some(value.clone()),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn a_start_checks_and_backs_up_the_file_as_its_settings_say() {
        use IntegrityCheck::{AfterUncleanShutdown, Always, Never};
        use StartBackup::{Make, Skip};
        let dir = TempDir::new("start-policy");
        // (backups, integrity-check) → (checked, backing up), after a normal stop.
        for (backup, check, checked, backing_up) in [
            (Skip, Always, 1, false),
            (Skip, AfterUncleanShutdown, 0, false),
            (Skip, Never, 0, false),
            (Make, Always, 1, true),
            (Make, AfterUncleanShutdown, 0, true),
            (Make, Never, 0, true),
        ] {
            start_whole(&dir, Skip).close().unwrap();
            CHECKS.set(0);
            let storage = Storage::open(&dir.place(), Start { backup, check }).unwrap();
            assert_eq!(
                (CHECKS.get(), storage.is_backing_up()),
                (checked, backing_up),
                "{backup:?} {check:?}"
            );
            storage.close().unwrap();
        }
        // After a stop that did not close the file, every start that may check does.
        for (check, checked) in [(Always, 1), (AfterUncleanShutdown, 1), (Never, 0)] {
            drop(start_whole(&dir, Skip));
            CHECKS.set(0);
            let start = Start {
                backup: Skip,
                check,
            };
            Storage::open(&dir.place(), start).unwrap().close().unwrap();
            assert_eq!(CHECKS.get(), checked, "{check:?}");
        }
    }

    #[test]
    fn only_a_file_closed_after_a_normal_stop_skips_the_check_after_an_unclean_one() {
        let dir = TempDir::new("closed-mark");
        let world = dir.place().file;
        let after_unclean = || checks(&world, IntegrityCheck::AfterUncleanShutdown);
        // No file: checking a new one costs nothing.
        assert!(after_unclean());
        let start = Start {
            backup: StartBackup::Skip,
            check: IntegrityCheck::AfterUncleanShutdown,
        };
        Storage::open(&dir.place(), start).unwrap().close().unwrap();
        assert!(!after_unclean());
        assert!(checks(&world, IntegrityCheck::Always));
        assert!(!checks(&world, IntegrityCheck::Never));
        // A start removes the mark at once: a stop that does not close the file leaves none.
        drop(Storage::open(&dir.place(), start).unwrap());
        assert!(after_unclean());
        // So does a start that does not check.
        Storage::open(&dir.place(), start).unwrap().close().unwrap();
        let never = Start {
            check: IntegrityCheck::Never,
            ..start
        };
        drop(Storage::open(&dir.place(), never).unwrap());
        assert!(after_unclean());
        // An online backup of the open file has no mark.
        let mut storage = Storage::open(&dir.place(), start).unwrap();
        assert!(storage.begin_backup(&dir.place().backups).unwrap());
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
        for start in [StartBackup::Make, StartBackup::Skip] {
            refuses_a_file_that_is_not_redb(start);
        }
    }

    fn refuses_a_file_that_is_not_redb(start: StartBackup) {
        let dir = TempDir::new(&format!("not-redb-{start:?}"));
        for _ in 0..=backup::KEPT {
            start_whole(&dir, StartBackup::Make).close().unwrap();
        }
        let older = dir.backups();
        assert_eq!(older.len(), backup::KEPT);
        // A torn first page or another file put at the path: redb finds no magic number.
        let bytes = b"not a redb file".to_vec();
        dir.set_world(&bytes);
        for _ in 0..=backup::KEPT {
            let e = Storage::open(&dir.place(), Start::checked(start)).unwrap_err();
            assert!(e.is_damage(), "{start:?}: {e:?}");
            assert!(
                dir.world().eq(&bytes),
                "{start:?}: the file is left as it was"
            );
            assert_eq!(dir.backups(), older, "{start:?}");
        }
    }

    /// A file of a newer format is refused with every byte as it was, by every start, also one
    /// that does not check the file.
    #[test]
    fn a_newer_format_is_refused_and_left_as_it_was() {
        use IntegrityCheck::{AfterUncleanShutdown, Always, Never};
        for backup in [StartBackup::Make, StartBackup::Skip] {
            for check in [Always, AfterUncleanShutdown, Never] {
                refuses_a_newer_format(Start { backup, check });
            }
        }
    }

    fn refuses_a_newer_format(start: Start) {
        let dir = TempDir::new("newer");
        let mut storage = Storage::open(&dir.place(), start).unwrap();
        let mut sealed = seal(META_KEY, &Meta::at(1)).unwrap();
        sealed[..2].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let mut crc = seal::Crc32c::new();
        crc.update(META_KEY);
        crc.update(&sealed[..8]);
        crc.update(&sealed[12..]);
        sealed[8..12].copy_from_slice(&crc.finish().to_le_bytes());
        let mut batch = Batch::default();
        batch.put(Key::Meta, sealed);
        storage.commit(&batch).unwrap();
        storage.close().unwrap();
        let bytes = dir.world();
        let e = Storage::open(&dir.place(), start).unwrap_err();
        assert!(
            matches!(e, Error::NewerFormat { version } if version == FORMAT_VERSION + 1),
            "{start:?}: {e:?}"
        );
        assert!(dir.world().eq(&bytes), "{start:?}: the file is unchanged");
        assert!(dir.backups().is_empty(), "{start:?}");
    }

    /// The persistent savepoints of the open `storage`'s file.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a test reads a database"
    )]
    fn savepoints(storage: &mut Storage) -> usize {
        let txn = storage.database().unwrap().begin_write().unwrap();
        txn.list_persistent_savepoints().unwrap().count()
    }

    /// Commits a random batch with meta tick `tick` to `model` and `storage`, keeping a savepoint.
    fn commit_saved(
        storage: &mut Storage,
        rng: &mut Rng,
        model: &mut BTreeMap<Key, Vec<u8>>,
        tick: u64,
    ) {
        let batch = random_batch(rng, model, tick);
        storage.commit_with_savepoint(&batch).unwrap();
    }

    /// A file committed with savepoints keeps one, the one of its last commit; a start rolls it
    /// back to its commit before when the main file's save is that one, whatever the commits
    /// before, and a second start changes nothing.
    #[test]
    fn a_file_later_than_the_main_file_rolls_back_to_the_commit_before_its_last() {
        let dir = TempDir::new("settle");
        let mut rng = Rng(0x5e77_0001);
        let mut storage = start_whole(&dir, StartBackup::Skip);
        let mut model = BTreeMap::new();
        for tick in 1..=3 {
            commit_saved(&mut storage, &mut rng, &mut model, tick);
            assert_eq!(savepoints(&mut storage), 1);
        }
        let saved = model.clone();
        let last = random_batch(&mut rng, &mut model, 4);
        storage.commit_with_savepoint(&last).unwrap();
        assert_eq!(savepoints(&mut storage), 1);
        // Killed: dropped without the close of a normal stop.
        drop(storage);
        assert_eq!(
            settle(&dir.place().file, &Meta::at(3)).unwrap(),
            Settled::RolledBack
        );
        let bytes = dir.world();
        assert_eq!(
            settle(&dir.place().file, &Meta::at(3)).unwrap(),
            Settled::Kept
        );
        assert_eq!(dir.world(), bytes, "a second start changes nothing");
        let mut storage = start_whole(&dir, StartBackup::Skip);
        assert_eq!(contents(&mut storage, &all_keys()), saved);
        // A normal stop deletes the savepoint.
        storage.close().unwrap();
        let mut storage = start_whole(&dir, StartBackup::Skip);
        assert_eq!(savepoints(&mut storage), 0);
        storage.close().unwrap();
    }

    /// A file a save made, later than the main file, is removed; one that is not later is kept as
    /// it was; one later without a savepoint to roll back to is refused, with every byte as it
    /// was, by every start.
    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_file_the_main_file_never_saved_is_removed_and_one_without_a_savepoint_is_refused() {
        let dir = TempDir::new("settle-made");
        let mut rng = Rng(0x5e77_0002);
        let mut model = BTreeMap::new();
        let mut storage = Storage::open(&dir.place(), Start::checked(StartBackup::Skip)).unwrap();
        commit_saved(&mut storage, &mut rng, &mut model, 1);
        drop(storage);
        let bytes = dir.world();
        assert_eq!(
            settle(&dir.place().file, &Meta::at(1)).unwrap(),
            Settled::Kept
        );
        assert_eq!(dir.world(), bytes, "a file not later is left as it was");
        assert_eq!(
            settle(&dir.place().file, &Meta::at(0)).unwrap(),
            Settled::Removed
        );
        assert!(!dir.place().file.exists());
        assert_eq!(
            settle(&dir.place().file, &Meta::at(0)).unwrap(),
            Settled::Kept
        );
        let mut storage = Storage::open(&dir.place(), Start::checked(StartBackup::Skip)).unwrap();
        storage
            .commit(&random_batch(&mut rng, &mut model, 2))
            .unwrap();
        drop(storage);
        let bytes = dir.world();
        for _ in 0..2 {
            assert!(matches!(
                settle(&dir.place().file, &Meta::at(1)),
                Err(Error::NoRollback)
            ));
            assert_eq!(dir.world(), bytes, "a refused file is left as it was");
        }
    }

    /// A path that only reads refuses a file later than the main file, which only a start rolls
    /// back; without a main file there is no save to compare with.
    #[test]
    fn a_reading_path_refuses_a_file_later_than_the_main_file() {
        let dir = TempDir::new("settled");
        // The players file stands in for the main file, and the test's file for a world's.
        let players = Place::data(&dir.0, PLAYERS_FILE);
        let main = players.file.clone();
        let mut rng = Rng(0x5e77_0003);
        let mut model = BTreeMap::new();
        let mut storage = start_whole(&dir, StartBackup::Skip);
        commit_saved(&mut storage, &mut rng, &mut model, 2);
        storage.close().unwrap();
        check_settled(&dir.place().file, &main).unwrap();
        for (tick, ahead) in [(2, false), (1, true)] {
            let mut storage = Storage::open(&players, Start::checked(StartBackup::Skip)).unwrap();
            let mut batch = Batch::default();
            batch.put(Key::Meta, seal(META_KEY, &Meta::at(tick)).unwrap());
            storage.commit(&batch).unwrap();
            storage.close().unwrap();
            let checked = check_settled(&dir.place().file, &main);
            assert_eq!(
                matches!(checked, Err(Error::Ahead)),
                ahead,
                "{tick}: {checked:?}"
            );
            assert_eq!(checked.is_ok(), !ahead, "{tick}: {checked:?}");
        }
    }

    /// An export's snapshot holds the main file too, so no server starts while it is open.
    #[test]
    fn a_snapshot_keeps_the_main_file_from_a_start() {
        let dir = TempDir::new("snapshot-main");
        let main = Place::data(&dir.0, PLAYERS_FILE);
        let mut storage = Storage::open(&main, Start::checked(StartBackup::Skip)).unwrap();
        let mut batch = Batch::default();
        batch.put(Key::Meta, seal(META_KEY, &Meta::at(2)).unwrap());
        storage.commit(&batch).unwrap();
        storage.close().unwrap();
        // The players file stands in for the main file, and the test's file for a world's.
        start_whole(&dir, StartBackup::Skip).close().unwrap();
        let snapshot = Snapshot::open(&dir.place().file, &main.file).unwrap();
        assert!(matches!(
            Storage::open(&main, Start::checked(StartBackup::Skip)),
            Err(Error::InUse)
        ));
        drop(snapshot);
        Storage::open(&main, Start::checked(StartBackup::Skip)).unwrap();
    }

    #[test]
    fn a_world_name_is_a_file_name_on_windows_and_linux() {
        let longest = "w".repeat(MAX_FILE_NAME);
        for good in [
            "world", "Lobby 2", "로비", "a.b", "con1", "comx", "lpt10", "_", &longest,
        ] {
            assert!(is_world_file_name(good), "{good}");
        }
        let too_long = "w".repeat(MAX_FILE_NAME + 1);
        for bad in [
            "",
            "a/b",
            "a\\b",
            "..",
            ".",
            "world.",
            "world ",
            " world",
            "a:b",
            "a*",
            "a?",
            "a|b",
            "a<b",
            "a>b",
            "a\"b",
            "a\u{0}b",
            "a\nb",
            "CON",
            "con",
            "Nul.txt",
            "aux .x",
            "COM1",
            "lpt9.redb",
            "com\u{b9}",
            "CONIN$",
            &too_long,
        ] {
            assert!(!is_world_file_name(bad), "{bad:?}");
        }
        assert_eq!(world_name_key("Lobby"), world_name_key("LOBBY"));
        assert_ne!(world_name_key("lobby"), world_name_key("lobby2"));
        let place = Place::world(Path::new("data"), "Lobby");
        assert_eq!(
            place.file,
            Path::new("data").join("worlds").join("Lobby.redb")
        );
        assert_eq!(
            place.backups,
            Path::new("data")
                .join("backups")
                .join("worlds")
                .join("Lobby")
        );
    }

    #[test]
    fn errors_are_damage_only_for_damage() {
        assert!(Error::Checksum.is_damage());
        assert!(Error::Database(redb::Error::Corrupted(String::new())).is_damage());
        assert!(Error::NoRollback.is_damage());
        assert!(!Error::Ahead.is_damage());
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
        let mut storage = Storage::open(&dir.place(), Start::checked(StartBackup::Make)).unwrap();
        let mut model = BTreeMap::new();
        storage
            .commit(&random_batch(&mut rng, &mut model, 0))
            .unwrap();
        storage.commit(&large_batch(&mut rng, 20, 1)).unwrap();
        let committed = tables(storage.database().unwrap());
        assert!(storage.begin_backup(&dir.place().backups).unwrap());
        assert_eq!(storage.backup_step().unwrap(), None);
        // Commits and reads go on between steps; the backup does not see the new commit.
        let later = random_batch(&mut rng, &mut model, 2);
        storage.commit(&later).unwrap();
        assert_eq!(contents(&mut storage, &all_keys()), model);
        let (path, steps) = finish_backup(&mut storage);
        assert!(steps >= 2, "{steps} more steps");
        assert_eq!(
            names_in(&dir.place().backups),
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
            assert_eq!(copied[name], *entries, "table {name} differs");
        }
        drop(backup);
        assert_eq!(crate::save::committed_meta(&path).unwrap().tick, 1);
        storage.close().unwrap();
    }

    #[test]
    fn a_failure_mid_copy_leaves_no_backup_and_the_file_works_on() {
        let dir = TempDir::new("online-fault");
        let backups = dir.place().backups;
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
        let backups = dir.place().backups;
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
        let backups = dir.place().backups;
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
        let backups = dir.place().backups;
        start_whole(&dir, StartBackup::Make).close().unwrap();
        let mut storage = start_whole(&dir, StartBackup::Make);
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

    /// Measures the data file: `cargo test --release --lib storage::tests::measure_server_file
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
        let mut storage = Storage::open(&dir.place(), Start::checked(StartBackup::Make)).unwrap();
        for count in [1000, 10_000] {
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
            let mut batch = Batch::with_capacity(1000);
            for (key, value) in &chunks[(round % 10) * 1000..][..1000] {
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
        println!("close: {:.1} ms, file {} bytes", ms(start), dir.world_len());
        let start = clock::now();
        let mut storage = Storage::open(&dir.place(), Start::checked(StartBackup::Make)).unwrap();
        println!("open (backup, check): {:.1} ms", ms(start));
        let start = clock::now();
        for batch in keys.chunks(64) {
            storage.read(batch).unwrap();
        }
        println!(
            "read in batches of 64 (after open): {:.2} µs per chunk",
            ms(start) * 1e3 / 10_000.0
        );
    }

    /// Commits chunks of `records` terrain by sub-chunk, 2,000 a commit, until the data file in
    /// `dir` reaches `target` bytes; returns how many.
    fn fill_world(
        storage: &mut Storage,
        dir: &TempDir,
        records: &[ChunkRecord],
        target: u64,
    ) -> u32 {
        let mut chunks = 0_u32;
        while dir.world_len() < target {
            let mut batch = Batch::with_capacity(2000 * 24 + 1);
            for _ in 0..2000 {
                let id = ChunkId {
                    world: 0,
                    x: i32::try_from(chunks % 2048).unwrap(),
                    z: i32::try_from(chunks / 2048).unwrap(),
                };
                let record = &records[usize::try_from(chunks).unwrap() % records.len()];
                for (index, sub_chunk) in (0_u8..).zip(record.sub_chunks.iter()) {
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

    /// Start and online backup costs by file size: `cargo test
    /// --release --lib storage::tests::measure_start_cost -- --ignored --nocapture`. Each world
    /// is terrain chunks committed by sub-chunk as the save thread writes them, 2,000 a commit,
    /// until the file reaches the size; then it is closed as at shutdown, and each
    /// part of the next start is timed alone, then the online backup.
    #[test]
    #[ignore = "measurement"]
    #[expect(
        clippy::print_stdout,
        clippy::disallowed_methods,
        reason = "consistency: a measurement prints its results and reads files"
    )]
    fn measure_start_cost() {
        let mut rng = Rng(0x5747_0001);
        let records: Vec<ChunkRecord> = (0..64).map(|_| terrain_chunk(&mut rng)).collect();
        for target in [256_u64 << 20, 1 << 30, 4 << 30] {
            let dir = TempDir::new(&format!("start-cost-{target}"));
            let world = dir.place().file;
            let mut storage =
                Storage::open(&dir.place(), Start::checked(StartBackup::Make)).unwrap();
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
                "  close as at shutdown {:.0} ms, file {} MiB",
                ms(start),
                size >> 20
            );
            // The start's parts, each alone.
            let start = clock::now();
            verify(&world, IntegrityCheck::Always).unwrap();
            println!("  start check (writes nothing) {:.0} ms", ms(start));
            let start = clock::now();
            let mut db = Source::File(world.clone()).open().unwrap();
            println!("  open {:.0} ms", ms(start));
            let start = clock::now();
            assert!(db.check_integrity().unwrap());
            println!("  check_integrity {:.0} ms", ms(start));
            let start = clock::now();
            let compacted = db.compact().unwrap();
            println!(
                "  compact (no start or stop does it) {:.0} ms (compacted: {compacted})",
                ms(start)
            );
            drop(db);
            let start = clock::now();
            let mut storage =
                Storage::open(&dir.place(), Start::checked(StartBackup::Make)).unwrap();
            println!("  whole start {:.0} ms", ms(start));
            // The start backup, with the longest step: the longest a load waits.
            let start = clock::now();
            assert!(storage.is_backing_up());
            let mut steps = 0;
            let mut longest = 0.0_f64;
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

    /// The start of a real data folder with `integrity-check = "always"`, as the server makes it,
    /// each part timed: `cargo test --release --lib storage::tests::measure_data_folder_start --
    /// --ignored --nocapture` with `ANCHOR_MEASURE_DATA` naming a copy of the data folder, which
    /// the start writes to (its first commit, the backups). The checks of every file run at once
    /// on a CPU pool of 4 threads, then the files open one by one; with backups, the start backups
    /// are then stepped to their end, as the save thread does while the server runs.
    /// `ANCHOR_MEASURE_BACKUPS=false` starts as `backups = false`; `ANCHOR_MEASURE_CRASH=1` first
    /// leaves every file as a killed process does; `ANCHOR_MEASURE_READ=1` instead reads each file
    /// once sequentially, the lower bound. For a cold start, evict the files from
    /// the OS cache first; on Windows, without administrator rights, opening each file once with
    /// `FILE_FLAG_NO_BUFFERING` does it.
    #[test]
    #[ignore = "measurement"]
    #[expect(
        clippy::print_stdout,
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "consistency: a measurement prints its results and reads files"
    )]
    fn measure_data_folder_start() {
        let data = PathBuf::from(
            std::env::var_os("ANCHOR_MEASURE_DATA").expect("ANCHOR_MEASURE_DATA names a data copy"),
        );
        let flag = |name: &str| std::env::var(name).ok();
        let backup = if flag("ANCHOR_MEASURE_BACKUPS").as_deref() == Some("false") {
            StartBackup::Skip
        } else {
            StartBackup::Make
        };
        let mut places = vec![
            Place::data(&data, MAIN_FILE),
            Place::data(&data, PLAYERS_FILE),
        ];
        for name in crate::save::world_files(&data).unwrap() {
            places.push(Place::world(&data, &name));
        }
        let name = |place: &Place| {
            let size = std::fs::metadata(&place.file).unwrap().len();
            let name = place.file.strip_prefix(&data).unwrap().display();
            format!("{name} ({} KiB)", size >> 10)
        };
        if flag("ANCHOR_MEASURE_READ").is_some() {
            let mut total = 0.0;
            for place in &places {
                let start = clock::now();
                let mut file = std::fs::File::open(&place.file).unwrap();
                let mut buffer = vec![0; 8 << 20];
                while io::Read::read(&mut file, &mut buffer).unwrap() > 0 {}
                let read = ms(start);
                total += read;
                println!("{}: sequential read {read:.1} ms", name(place));
            }
            println!("all files {total:.1} ms");
            return;
        }
        if flag("ANCHOR_MEASURE_CRASH").is_some() {
            // redb's recovery flag in the header's god byte, after its 9-byte magic number: what
            // a process killed after a quick-repair commit leaves.
            for place in &places {
                let mut bytes = std::fs::read(&place.file).unwrap();
                bytes[9] |= 2;
                std::fs::write(&place.file, bytes).unwrap();
            }
        }
        let pool = crate::runtime::cpu_pool(4).unwrap();
        let start = clock::now();
        let checks = pool.map_in_turns(&places, |place| {
            check::tests::WRITTEN_PEAK.set(0);
            let start = clock::now();
            verify(&place.file, IntegrityCheck::Always).unwrap();
            (ms(start), check::tests::WRITTEN_PEAK.get())
        });
        let checked = ms(start);
        let start = clock::now();
        let mut opened = Vec::new();
        for (place, (check, written)) in places.iter().zip(checks) {
            let at = clock::now();
            let unchecked = Start {
                backup,
                check: IntegrityCheck::Never,
            };
            let storage = Storage::open(place, unchecked).unwrap();
            println!(
                "{}: check {check:.1} ms keeping {} KiB of writes, open {:.1} ms",
                name(place),
                written >> 10,
                ms(at)
            );
            opened.push((name(place), storage));
        }
        let open = ms(start);
        println!(
            "all files: checks at once {checked:.1} ms, opens {open:.1} ms, start {:.1} ms",
            checked + open
        );
        // The start backups, which the save thread copies while the server runs.
        for (name, storage) in &mut opened {
            let start = clock::now();
            let mut steps = 0;
            let mut longest = 0.0_f64;
            while storage.is_backing_up() {
                let step = clock::now();
                storage.backup_step().unwrap();
                steps += 1;
                longest = longest.max(ms(step));
            }
            if steps > 0 {
                println!(
                    "{name}: start backup {:.1} ms in {steps} steps, longest step {longest:.1} ms",
                    ms(start)
                );
            }
        }
        assert!(pool.shutdown());
    }
}
