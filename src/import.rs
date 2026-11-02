//! Importing a Bedrock (LevelDB) world: reading the source.
//!
//! `Source` opens a world folder read-only (`level.dat` and the LevelDB in `db/`), lists its
//! overworld chunks in numeric (x, z) order (`Catalog`) and reads the values of one chunk
//! (`chunk::Entries`); `chunk::decode` turns those into raw data (`chunk::Raw`): block
//! palettes as stored (NBT `{name, states, version}`, older `{name, val}`, or legacy id and meta
//! arrays) and block entities as NBT. Biomes, entities and pending ticks are not imported, only
//! counted. Nothing is upgraded here.
//! Reading takes `&mut self` (one block cache); decoding takes shared data, so it can run on the
//! CPU pool. The source is never written; `Source::unchanged` fails if any of its files changed
//! since it was opened, so the world must not be open in another program.
//!
//! [`Server::import_world`](crate::Server::import_world) runs the import (`api`), upgrading old
//! block states and items (`upgrade`), and returns a [`Report`]. [`Db`] opens the same LevelDB
//! for the keys the import leaves out.

mod api;
pub(crate) mod chunk;
pub(crate) mod level_dat;
pub(crate) mod leveldb;
mod upgrade;

pub use api::Report;
pub(crate) use api::run;
pub use leveldb::Db;
pub(crate) use upgrade::BLOCK_STATE_VERSION;

use std::fmt;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use chunk::Entries;
use level_dat::WorldData;
use leveldb::Stamp;

/// The most overworld chunks a world may have: a square of 2,048 by 2,048 chunks (32,768 blocks
/// across). A list of positions this long takes 32 MiB. The export lists at most as many.
pub(crate) const MAX_CHUNKS: usize = 1 << 22;
/// The largest `level.dat`: its header and the 4 MiB NBT limit.
const MAX_LEVEL_DAT: u64 = 8 + crate::nbt::MAX_ROOT_BYTES as u64;

/// A failure to read the world being imported.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A file cannot be read.
    Read(io::Error),
    /// A file, block, key, value or list holds more than Anchor reads: more than `limit` of
    /// `what`.
    TooLarge {
        /// What was counted, with its unit, such as `bytes in a block` or `overworld chunks`.
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
    /// The file at `path` of the world folder cannot be read; the source says why.
    File {
        /// The file.
        path: PathBuf,
        /// Why.
        source: Box<Error>,
    },
    /// The overworld chunk at (`x`, `z`) cannot be read; the source says why, and is
    /// [`Error::Key`] when one of its values is at fault.
    Chunk {
        /// The chunk's x coordinate.
        x: i32,
        /// The chunk's z coordinate.
        z: i32,
        /// Why.
        source: Box<Error>,
    },
    /// The value of a database key cannot be read; the source says why.
    Key {
        /// The kind of key, by its name in Mojang's LevelDB documentation, such as
        /// `SubChunkPrefix` or `BlockEntity`.
        kind: &'static str,
        /// The key's bytes in hexadecimal.
        key: String,
        /// Why.
        source: Box<Error>,
    },
    /// A temporary world of this name, in any letter case, is declared on the builder
    /// ([`Kind::Temporary`](crate::world::Kind::Temporary)): it is never saved, so nothing can be
    /// imported into it.
    TemporaryWorld {
        /// The name.
        name: String,
    },
    /// The world's file `data/worlds/<name>.redb` (or one whose name differs only in letter case)
    /// is already there. An import only creates a world, so running it again changes nothing.
    WorldExists {
        /// The name.
        name: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(_) => f.write_str("cannot read the file"),
            Self::TooLarge { what, limit } => write!(f, "more than {limit} {what}"),
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
            Self::File { path, .. } => write!(f, "source file {}", path.display()),
            Self::Chunk { x, z, .. } => write!(f, "overworld chunk {x} {z}"),
            Self::Key { kind, key, .. } => write!(f, "{kind} key {key}"),
            Self::TemporaryWorld { name } => {
                write!(f, "the world {name:?} is declared temporary")
            }
            Self::WorldExists { name } => {
                write!(f, "the world {name:?} already has a file")
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
            Self::File { source, .. } | Self::Chunk { source, .. } | Self::Key { source, .. } => {
                Some(source.as_ref())
            }
            Self::TooLarge { .. }
            | Self::Malformed(_)
            | Self::Checksum
            | Self::UnknownCompression(_)
            | Self::NewerFormat { .. }
            | Self::SourceChanged { .. }
            | Self::TemporaryWorld { .. }
            | Self::WorldExists { .. } => None,
        }
    }
}

/// A result with the module's [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A Bedrock world folder opened for reading.
pub(crate) struct Source {
    level_dat_path: PathBuf,
    level_dat_stamp: Stamp,
    world: WorldData,
    db: Db,
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
        let db = Db::open(dir)?;
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

    /// Fails with [`Error::SourceChanged`] unless every file of the world is as it was when it
    /// was opened.
    pub(crate) fn unchanged(&self) -> Result<()> {
        // A `level.dat` that can no longer be read is not the one read.
        if stamp(&self.level_dat_path).ok().as_ref() != Some(&self.level_dat_stamp) {
            return Err(Error::SourceChanged {
                path: self.level_dat_path.clone(),
            });
        }
        self.db.unchanged()
    }

    /// Lists the overworld chunks and counts what will not be imported.
    pub(crate) fn catalog(&mut self) -> Result<Catalog> {
        let mut catalog = Catalog::default();
        self.db.scan(&[], None, |key, _| {
            catalog.add(key)?;
            Ok(ControlFlow::Continue(()))
        })?;
        // Keys come in byte order, in which little-endian x and z are not numeric order.
        catalog.chunks.sort_unstable();
        Ok(catalog)
    }

    /// Reads the values of the overworld chunk at (`x`, `z`).
    pub(crate) fn read(&mut self, x: i32, z: i32) -> Result<Entries> {
        let mut entries = Entries::new(x, z);
        let prefix = chunk::prefix(x, z);
        let end = chunk::after(&prefix);
        self.db
            .scan(&prefix, end.as_deref(), |key, value| {
                entries.add(key, value)?;
                Ok(ControlFlow::Continue(()))
            })
            .map_err(|e| Error::Chunk {
                x,
                z,
                source: Box::new(e),
            })?;
        Ok(entries)
    }
}

impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Source")
            .field("level_dat", &self.level_dat_path)
            .finish_non_exhaustive()
    }
}

/// What a world holds besides its overworld chunks, which Anchor does not import.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Catalog {
    /// The overworld chunks, in numeric (x, z) order.
    pub(crate) chunks: Vec<(i32, i32)>,
    /// Nether chunks (dimension 1).
    pub(crate) nether_chunks: u64,
    /// End chunks (dimension 2).
    pub(crate) end_chunks: u64,
    /// Chunks of other dimension numbers.
    pub(crate) other_dimension_chunks: u64,
    /// Entities of every dimension (`actorprefix` keys, one per entity since 1.18.30): Anchor
    /// imports none.
    pub(crate) entities: u64,
    /// Player keys (`~local_player`, `player_*`): Bedrock players have no XUID to match.
    pub(crate) players: u64,
    /// World-level keys (scoreboard, maps, villages, ticking areas, ...).
    pub(crate) other_keys: u64,
}

impl Catalog {
    /// Counts the database key `key`; keys come in key order.
    fn add(&mut self, key: &[u8]) -> Result<()> {
        match chunk::classify(key) {
            chunk::Key::Overworld {
                x,
                z,
                version: true,
            } => {
                // The keys of one chunk share its 8-byte prefix, so they come together: no other
                // chunk is listed between a chunk's two version keys.
                if self.chunks.last() == Some(&(x, z)) {
                    return Ok(());
                }
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
            chunk::Key::Entity => self.entities += 1,
            chunk::Key::Player => self.players += 1,
            chunk::Key::Other => self.other_keys += 1,
            chunk::Key::Overworld { .. }
            | chunk::Key::Dimension { .. }
            | chunk::Key::EntityList => {}
        }
        Ok(())
    }
}

/// Reads at most `limit` bytes of the file at `path`; a longer file is [`Error::TooLarge`],
/// found before anything is allocated.
fn read_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    crate::fs::read_bounded(path, limit).map_err(|e| match e.kind() {
        io::ErrorKind::FileTooLarge => Error::TooLarge {
            what: "bytes in a file",
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

#[cfg(test)]
mod tests;
