//! Importing a Bedrock (LevelDB) world (PLAN.md 10.9 "가져오기 설계"): reading the source.
//!
//! `Source` opens a world folder read-only (`level.dat` and the LevelDB in `db/`), lists its
//! overworld chunks in numeric (x, z) order (`Catalog`) and reads the values of one chunk
//! (`chunk::Entries`); `chunk::decode` turns those into raw data (`chunk::Raw`): block
//! palettes as stored (NBT `{name, states, version}`, older `{name, val}`, or legacy id and meta
//! arrays), biomes, block entities, entities and pending ticks as NBT. Nothing is upgraded here.
//! Reading takes `&mut self` (one block cache); decoding takes shared data, so it can run on the
//! CPU pool. The source is never written; `Source::unchanged` fails if any of its files changed
//! since it was opened, so the world must not be open in another program.
//!
//! [`Server::import_world`](crate::Server::import_world) runs the import (`api`), upgrading old
//! block states and items (`upgrade`), and returns a [`Report`].

mod api;
mod chunk;
mod level_dat;
mod leveldb;
mod upgrade;

use std::fmt;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

pub use self::api::Report;
pub(crate) use self::api::run;
use self::chunk::Entries;
use self::level_dat::WorldData;
use self::leveldb::{Db, Files, Stamp};
pub(crate) use self::upgrade::BLOCK_STATE_VERSION;

/// A failure to read the world being imported.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A file cannot be read.
    Read(io::Error),
    /// A file, block, key, value or list is larger than Anchor reads: at most `limit` bytes or
    /// entries.
    TooLarge {
        /// What is too large.
        what: &'static str,
        /// The most accepted.
        limit: u64,
    },
    /// The data is not in the format Anchor reads; the text says what is wrong.
    Malformed(&'static str),
    /// A block or log record does not match its checksum.
    Checksum,
    /// A table block uses a compression other than 0 (none), 1 (Snappy), 2 (zlib) and 4 (raw
    /// DEFLATE), such as 3 (zstd).
    UnknownCompression(u8),
    /// A block does not decompress; the source says why.
    Compression(crate::protocol::compression::Error),
    /// An NBT value does not decode; the source says why.
    Nbt(crate::protocol::Error),
    /// The world was saved by a newer Minecraft than Anchor reads.
    NewerFormat {
        /// The version field.
        what: &'static str,
        /// The version found.
        version: i64,
        /// The newest version Anchor reads.
        supported: i64,
    },
    /// A file in the world folder was changed, added or removed while it was read: the world
    /// is open in another program.
    SourceChanged {
        /// The file.
        path: PathBuf,
    },
    /// The file at `path` cannot be read; the source says why.
    File {
        /// The file.
        path: PathBuf,
        /// Why.
        source: Box<Error>,
    },
    /// The chunk at (`x`, `z`) cannot be read; the source says why.
    Chunk {
        /// The chunk's x coordinate.
        x: i32,
        /// The chunk's z coordinate.
        z: i32,
        /// Why.
        source: Box<Error>,
    },
    /// No persistent world of this name is declared on the builder: the target must be one
    /// ([`ServerBuilder::world`](crate::ServerBuilder::world) with
    /// [`Kind::Persistent`](crate::world::Kind::Persistent), or `level-name`).
    UnknownWorld {
        /// The name.
        name: String,
    },
    /// The world file already holds the world. An import only creates a world, so running it
    /// again changes nothing.
    WorldExists {
        /// The name.
        name: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(_) => f.write_str("cannot read the file"),
            Self::TooLarge { what, limit } => write!(f, "{what} larger than {limit}"),
            Self::Malformed(what) => write!(f, "malformed data: {what}"),
            Self::Checksum => f.write_str("checksum mismatch"),
            Self::UnknownCompression(id) => write!(f, "unknown block compression {id}"),
            Self::Compression(_) => f.write_str("a block does not decompress"),
            Self::Nbt(_) => f.write_str("invalid nbt"),
            Self::NewerFormat {
                what,
                version,
                supported,
            } => write!(
                f,
                "{what} {version} is newer than the supported {supported}"
            ),
            Self::SourceChanged { path } => write!(
                f,
                "{} changed while the world was read; close every program using it",
                path.display()
            ),
            Self::File { path, .. } => write!(f, "world file {}", path.display()),
            Self::Chunk { x, z, .. } => write!(f, "chunk {x} {z}"),
            Self::UnknownWorld { name } => {
                write!(f, "no persistent world named {name:?} is declared")
            }
            Self::WorldExists { name } => {
                write!(f, "the world file already holds the world {name:?}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(e) => Some(e),
            Self::Compression(e) => Some(e),
            Self::Nbt(e) => Some(e),
            Self::File { source, .. } | Self::Chunk { source, .. } => Some(source.as_ref()),
            Self::TooLarge { .. }
            | Self::Malformed(_)
            | Self::Checksum
            | Self::UnknownCompression(_)
            | Self::NewerFormat { .. }
            | Self::SourceChanged { .. }
            | Self::UnknownWorld { .. }
            | Self::WorldExists { .. } => None,
        }
    }
}

/// The result of reading the world being imported.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The allocation budget for decoding one chunk or `level.dat` (the world file's own budget,
/// `storage::seal`).
const DECODE_BUDGET: usize = 4 * crate::storage::seal::MAX_RECORD;
/// The largest `level.dat`: its header and the 4 MiB NBT limit.
const MAX_LEVEL_DAT: u64 = 8 + crate::nbt::MAX_ROOT_BYTES as u64;
/// The most overworld chunks a world may have: a square of 2,048 by 2,048 chunks (32,768 blocks
/// across). A list of positions this long takes 32 MiB.
const MAX_CHUNKS: usize = 1 << 22;

/// Reads at most `limit` bytes of the file at `path`; a longer file is [`Error::TooLarge`],
/// found before anything is allocated.
fn read_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    crate::fs::read_bounded(path, limit).map_err(|e| match e.kind() {
        io::ErrorKind::FileTooLarge => Error::TooLarge {
            what: "file",
            limit,
        },
        _ => Error::Read(e),
    })
}

/// The length and modification time of the file at `path`.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the import runs before the server starts"
)]
fn stamp(path: &Path) -> Result<Stamp> {
    let meta = std::fs::metadata(path).map_err(Error::Read)?;
    Ok(Stamp::new(
        meta.len(),
        meta.modified().map_err(Error::Read)?,
    ))
}

/// What a world holds besides its overworld chunks, which Anchor does not import (PLAN.md 10.9).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Catalog {
    /// The overworld chunks, in numeric (x, z) order.
    pub(crate) chunks: Vec<(i32, i32)>,
    /// Nether chunks (dimension 1).
    pub(crate) nether_chunks: u64,
    /// End chunks (dimension 2).
    pub(crate) end_chunks: u64,
    /// Chunks of other dimension numbers.
    pub(crate) other_dimension_chunks: u64,
    /// Player keys (`~local_player`, `player_*`): Bedrock players have no XUID to match.
    pub(crate) players: u64,
    /// World-level keys (scoreboard, maps, villages, ticking areas, ...).
    pub(crate) other_keys: u64,
}

impl Catalog {
    /// Counts the database key `key`.
    fn add(&mut self, key: &[u8]) -> Result<()> {
        match chunk::classify(key) {
            chunk::Key::Overworld {
                x,
                z,
                version: true,
            } => {
                if self.chunks.len() == MAX_CHUNKS {
                    return Err(Error::TooLarge {
                        what: "overworld chunks",
                        limit: MAX_CHUNKS as u64,
                    });
                }
                self.chunks.push((x, z));
            }
            chunk::Key::Dimension {
                dimension,
                version: true,
            } => {
                let count = match dimension {
                    1 => &mut self.nether_chunks,
                    2 => &mut self.end_chunks,
                    _ => &mut self.other_dimension_chunks,
                };
                *count += 1;
            }
            chunk::Key::Player => self.players += 1,
            chunk::Key::Other => self.other_keys += 1,
            chunk::Key::Overworld { .. } | chunk::Key::Dimension { .. } | chunk::Key::Actor => {}
        }
        Ok(())
    }
}

/// A Bedrock world folder opened for reading.
pub(crate) struct Source {
    level_dat_path: PathBuf,
    level_dat_stamp: Stamp,
    world: WorldData,
    db: Db,
}

impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Source")
            .field("level_dat", &self.level_dat_path)
            .finish_non_exhaustive()
    }
}

impl Source {
    /// Opens the world in folder `dir`: reads `level.dat`, `db/CURRENT`, the `MANIFEST` and the
    /// live logs.
    pub(crate) fn open(dir: &Path) -> Result<Self> {
        let level_dat_path = dir.join("level.dat");
        let located = |e| Error::File {
            path: level_dat_path.clone(),
            source: Box::new(e),
        };
        let level_dat_stamp = stamp(&level_dat_path).map_err(located)?;
        let bytes = read_file(&level_dat_path, MAX_LEVEL_DAT).map_err(located)?;
        let world = level_dat::parse(&bytes).map_err(located)?;
        let db = Db::open(Files::disk(dir.join("db")))?;
        Ok(Self {
            level_dat_path,
            level_dat_stamp,
            world,
            db,
        })
    }

    /// The world's `level.dat`.
    pub(crate) fn world(&self) -> &WorldData {
        &self.world
    }

    /// Lists the overworld chunks and counts what will not be imported.
    pub(crate) fn catalog(&mut self) -> Result<Catalog> {
        let mut catalog = Catalog::default();
        self.db.scan(&[], None, |key, _| {
            catalog.add(key)?;
            Ok(ControlFlow::Continue(()))
        })?;
        // A chunk may have both version keys.
        catalog.chunks.sort_unstable();
        catalog.chunks.dedup();
        Ok(catalog)
    }

    /// Reads the values of the overworld chunk at (`x`, `z`) and the entities `digp` lists.
    pub(crate) fn read(&mut self, x: i32, z: i32) -> Result<Entries> {
        let at = |e| Error::Chunk {
            x,
            z,
            source: Box::new(e),
        };
        let mut entries = Entries::new(x, z);
        let prefix = chunk::prefix(x, z);
        let end = chunk::after(&prefix);
        self.db
            .scan(&prefix, end.as_deref(), |key, value| {
                entries.add(key, value)?;
                Ok(ControlFlow::Continue(()))
            })
            .map_err(at)?;
        if let Some(ids) = self.db.get(&chunk::digest_key(x, z)).map_err(at)? {
            entries.set_digest();
            for id in chunk::actor_ids(&ids).map_err(at)? {
                let actor = self.db.get(&chunk::actor_key(*id)).map_err(at)?;
                let actor =
                    actor.ok_or_else(|| at(Error::Malformed("an entity digp lists is missing")))?;
                entries.add_actor(actor).map_err(at)?;
            }
        }
        Ok(entries)
    }

    /// Fails with [`Error::SourceChanged`] unless every file of the world is as it was when it
    /// was opened.
    pub(crate) fn unchanged(&self) -> Result<()> {
        if stamp(&self.level_dat_path).ok().as_ref() != Some(&self.level_dat_stamp) {
            return Err(Error::SourceChanged {
                path: self.level_dat_path.clone(),
            });
        }
        self.db.unchanged()
    }
}

#[cfg(test)]
mod tests;
