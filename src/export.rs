//! Exporting a persistent world as a Bedrock (LevelDB) world folder, the reverse of the import:
//! [`Server::export_world`](crate::Server::export_world) reads the world's file and writes a
//! folder that BDS and the game open, and returns a [`Report`].
//!
//! The world file is opened read-only and read in one read transaction (`storage::Snapshot`), so
//! the folder holds one commit; a file a server has open is refused, and so is one later than
//! the main file, which a start of the server rolls back. Its chunks are read in
//! Bedrock key order (x, then z, little-endian) a step at a time, then decoded and converted on
//! the CPU pool, and the step's values are made into compressed data blocks on the pool by runs
//! of `RUN_BYTES`. The pool keeps input order, and the LevelDB writer (`leveldb`) puts the
//! blocks in tables in that order: the same file gives the same bytes whatever the thread count.
//! A chunk's terrain is its sub-chunk values (`chunk`), or what the world's generator makes when
//! it has none (a chunk whose extras alone were saved); its block entities go as they are saved,
//! in Bedrock form. Entities and spills (the items of broken containers
//! waiting to drop) are left out and counted: their saved form is Anchor's. Chunks the file does
//! not hold (generated and never changed) are not written: the `level.dat` (`level_dat`) gives
//! the Bedrock world a flat generator that makes them.
//!
//! Everything is written into `<folder>.tmp`, flushed to disk and renamed to the folder at the
//! end, so the folder appears whole or not at all; a failure removes the export's own folder.

mod chunk;
mod level_dat;
mod leveldb;

use std::fmt;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use crate::import::MAX_CHUNKS;
use crate::nbt::{Compound, Value};
use crate::pool::Pool;
use crate::save::{self, WorldSpec};
use crate::storage::codec::WorldRecord;
use crate::storage::seal::unseal;
use crate::storage::{ChunkId, ChunkKind, ChunkPart, Key, MAIN_FILE, Place, Snapshot};
use crate::tick::SPILL_ID;
use crate::world::{Chunk, ChunkPos, Generator};

/// Value bytes after which a run of a step's values, made into data blocks on one pool item,
/// ends: a few blocks, so few blocks end short at the end of a run.
const RUN_BYTES: usize = 4 * leveldb::DATA_BLOCK_BYTES;
/// Most chunks read, then converted, per step: with [`STEP_BYTES`], bounds the chunk data
/// held at once.
const STEP: usize = 256;
/// Read value bytes after which a step ends, if it has not reached [`STEP`] chunks.
const STEP_BYTES: usize = 256 << 20;

/// What [`Server::export_world`](crate::Server::export_world) wrote and what it left out.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    chunks: u64,
    block_entities: u64,
    entities: u64,
    spills: u64,
}

impl Report {
    /// The number of chunks written: every chunk the world file holds.
    pub fn chunks(&self) -> u64 {
        self.chunks
    }

    /// The number of block entities written.
    pub fn block_entities(&self) -> u64 {
        self.block_entities
    }

    /// The number of entities left out: Anchor saves them in its own form.
    pub fn entities(&self) -> u64 {
        self.entities
    }

    /// The number of spills left out: the items of broken containers waiting to drop,
    /// which Anchor saves in its own form beside the block entities.
    pub fn spills(&self) -> u64 {
        self.spills
    }
}

/// A failure to export a world.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The world has no saved state: no file `data/worlds/<name>.redb` (in any letter case) or no
    /// world record in it, or the world is declared temporary.
    NoWorld {
        /// The name.
        name: String,
    },
    /// The folder to write, or the temporary folder `<folder>.tmp` next to it, is already
    /// there. Nothing was written. The export writes a new folder; the world is chosen by its
    /// name.
    FolderExists {
        /// The folder.
        path: PathBuf,
    },
    /// A file or folder of the export cannot be written; the source says why.
    Write {
        /// The file or folder.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
    /// The chunk at (`x`, `z`) cannot be read or written; the source says why.
    Chunk {
        /// The chunk's x coordinate.
        x: i32,
        /// The chunk's z coordinate.
        z: i32,
        /// Why.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoWorld { name } => write!(f, "the world {name:?} has no saved state"),
            Self::FolderExists { path } => write!(
                f,
                "{} is already there; the export writes a new folder and takes the world by its \
                 name",
                path.display()
            ),
            Self::Write { path, .. } => write!(f, "cannot write {}", path.display()),
            Self::Chunk { x, z, .. } => write!(f, "chunk {x} {z}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Write { source, .. } => Some(source),
            Self::Chunk { source, .. } => Some(source.as_ref()),
            Self::NoWorld { .. } | Self::FolderExists { .. } => None,
        }
    }
}

/// A result with the module's [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A converted chunk: its values in key order and what it adds to the report.
#[derive(Debug)]
struct Converted {
    values: Vec<(Vec<u8>, Vec<u8>)>,
    block_entities: u64,
    entities: u64,
    spills: u64,
}

/// Exports the persistent world `name` of `worlds` from the data folder `data_dir` into the new
/// folder `dir`, on a CPU pool of `cpu` threads that is joined before returning.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the export writes a world folder instead of running the server"
)]
pub(crate) fn run(
    data_dir: &Path,
    worlds: &[WorldSpec],
    cpu: usize,
    name: &str,
    dir: &Path,
) -> crate::Result<Report> {
    if !crate::storage::is_world_file_name(name) {
        return Err(crate::Error::InvalidWorldName {
            name: name.to_owned(),
        });
    }
    let no_world = || Error::NoWorld {
        name: name.to_owned(),
    };
    // The world by its file's name, which may differ from `name` in letter case.
    let file = save::world_file_name(data_dir, name)?.ok_or_else(no_world)?;
    let spec = save::file_world(worlds, &file).ok_or_else(no_world)?;
    // Without a trailing separator, which would put the temporary folder inside the folder.
    let dir: PathBuf = dir.components().collect();
    let mut temp = dir.clone().into_os_string();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    for path in [&dir, &temp] {
        if std::fs::exists(path).map_err(written(path))? {
            return Err(Error::FolderExists { path: path.clone() }.into());
        }
    }
    let snapshot = Snapshot::open(
        &Place::world(data_dir, &file).file,
        &Place::data(data_dir, MAIN_FILE).file,
    )?;
    let key = Key::World;
    let record: WorldRecord = match snapshot.world()? {
        Some(sealed) => unseal(&key.to_bytes(), &sealed)?,
        None => return Err(no_world().into()),
    };
    let mut ids = snapshot.chunk_ids(record.number, MAX_CHUNKS)?;
    ids.sort_unstable_by_key(|id| crate::import::chunk::prefix(id.x, id.z));

    // Started before the temporary folder is made, so a pool that fails to start leaves no folder.
    let pool = crate::runtime::cpu_pool(cpu)?;
    // A temporary folder that could not be made is not the export's own: it is not removed.
    let exported =
        create_temp(&temp).map(|()| write(&temp, &snapshot, &pool, &spec, &record, &ids));
    // The pool logs threads that do not end; the export's outcome does not depend on them.
    pool.shutdown();
    // The export's own folder: the temporary one until the rename, then `dir`.
    let mut own = temp.as_path();
    let finished = exported?.and_then(|report| {
        std::fs::rename(&temp, &dir).map_err(written(&dir))?;
        own = dir.as_path();
        crate::fs::sync_parent(&dir).map_err(written(&dir))?;
        Ok(report)
    });
    let report = match finished {
        Ok(report) => report,
        Err(e) => {
            // The export's own folder goes whole, so a failure leaves no folder.
            if let Err(e) = std::fs::remove_dir_all(own) {
                warn!(
                    path = %own.display(),
                    error = &e as &(dyn std::error::Error + 'static),
                    "could not remove the failed export's folder"
                );
            }
            return Err(e);
        }
    };
    info!(
        world = name,
        path = %dir.display(),
        chunks = report.chunks,
        block_entities = report.block_entities,
        left_out_entities = report.entities,
        left_out_spills = report.spills,
        "world exported"
    );
    Ok(report)
}

/// Creates the temporary folder `temp` and the folders above it that are missing; one already
/// there is [`Error::FolderExists`].
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the export writes a world folder instead of running the server"
)]
fn create_temp(temp: &Path) -> Result<()> {
    if let Some(parent) = temp.parent() {
        std::fs::create_dir_all(parent).map_err(written(parent))?;
    }
    std::fs::create_dir(temp).map_err(|e| match e.kind() {
        io::ErrorKind::AlreadyExists => Error::FolderExists {
            path: temp.to_path_buf(),
        },
        _ => written(temp)(e),
    })
}

/// Writes the world `spec` names, with the state `record` and the chunks `ids` (in Bedrock key
/// order) of `snapshot`, into the folder `temp`: the database in `db` and `level.dat`, each
/// file flushed, and the folders' entries.
fn write(
    temp: &Path,
    snapshot: &Snapshot,
    pool: &Pool,
    spec: &WorldSpec,
    record: &WorldRecord,
    ids: &[ChunkId],
) -> crate::Result<Report> {
    let db = temp.join("db");
    create_dir(&db)?;
    // The seed the world runs with, so generated terrain is the world's own.
    let seed = save::seed_or_default(&spec.name, spec.seed);
    let mut writer = leveldb::Writer::new(db.clone());
    let mut report = Report::default();
    let mut ids = ids.iter();
    while ids.len() > 0 {
        let mut step = Vec::with_capacity(ids.len().min(STEP));
        let mut bytes = 0_usize;
        while step.len() < STEP
            && bytes < STEP_BYTES
            && let Some(&id) = ids.next()
        {
            let values = snapshot.read_chunk(id).map_err(|e| Error::Chunk {
                x: id.x,
                z: id.z,
                source: Box::new(e),
            })?;
            bytes = values
                .iter()
                .fold(bytes, |sum, (_, value)| sum.saturating_add(value.len()));
            step.push((id, values));
        }
        let converted = pool.map(&step, |(id, values)| {
            convert(*id, values, spec.generator.as_ref(), seed)
        });
        // The read values are done with: only their converted form is held from here on.
        drop(step);
        let mut runs: Vec<Vec<(Vec<u8>, Vec<u8>)>> = Vec::new();
        let mut run_bytes = 0_usize;
        for c in converted {
            let c = c?;
            report.chunks += 1;
            report.block_entities += c.block_entities;
            report.entities += c.entities;
            report.spills += c.spills;
            for entry in c.values {
                let entry_bytes = entry.0.len().saturating_add(entry.1.len());
                match runs.last_mut() {
                    Some(run) if run_bytes < RUN_BYTES => run.push(entry),
                    _ => {
                        runs.push(vec![entry]);
                        run_bytes = 0;
                    }
                }
                run_bytes = run_bytes.saturating_add(entry_bytes);
            }
        }
        let blocks = pool.map(&runs, |run| leveldb::blocks(run));
        // The values are done with: only their blocks are held from here on.
        drop(runs);
        for blocks in blocks {
            for block in blocks.map_err(written(&db))? {
                writer.add(block)?;
            }
        }
    }
    writer.finish()?;
    let level_dat = temp.join("level.dat");
    write_new(
        &level_dat,
        &level_dat::encode(&spec.name, record, spec.generator.as_ref()),
    )?;
    crate::fs::sync_parent(&db.join("CURRENT")).map_err(written(&db))?;
    crate::fs::sync_parent(&level_dat).map_err(written(temp))?;
    Ok(report)
}

/// Creates the folder `dir`.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the export writes a world folder instead of running the server"
)]
fn create_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir(dir).map_err(written(dir))
}

/// Creates the file `path`, which must not exist, writes `bytes` to it and flushes it to disk.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: the export writes a world folder instead of running the server"
)]
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()
    })()
    .map_err(written(path))
}

/// What turns an I/O error of writing `path` into [`Error::Write`].
fn written(path: &Path) -> impl Fn(io::Error) -> Error + '_ {
    move |source| Error::Write {
        path: path.to_path_buf(),
        source,
    }
}

/// Converts the chunk `id` from its stored `values` into its Bedrock values: a pure function of
/// its inputs, for the CPU pool. A chunk without terrain values gets what `generator` makes with
/// `seed`; its spills are left out.
fn convert(
    id: ChunkId,
    values: &[(ChunkPart, Vec<u8>)],
    generator: &dyn Generator,
    seed: i64,
) -> Result<Converted> {
    let pos = ChunkPos::new(id.x, id.z);
    let at = |source: Box<dyn std::error::Error + Send + Sync>| Error::Chunk {
        x: id.x,
        z: id.z,
        source,
    };
    let (terrain, stored) = save::read_values(id, values).map_err(|(_, e)| at(Box::new(e)))?;
    let record = save::stored_terrain(terrain)
        .unwrap_or_else(|| save::to_record(&Chunk::generate(generator, seed, pos)));
    let extras = &stored.extras;
    let saved = extras.get(ChunkKind::BlockEntities);
    let block_entities: Vec<&Compound> = saved.iter().filter(|nbt| !is_spill(nbt)).collect();
    let values = chunk::values(pos, &record, &block_entities).map_err(|e| at(Box::new(e)))?;
    Ok(Converted {
        values,
        block_entities: block_entities.len() as u64,
        entities: extras.get(ChunkKind::Entities).len() as u64,
        spills: (saved.len() - block_entities.len()) as u64,
    })
}

/// Whether the saved block entity entry `nbt` is a spill, which only Anchor reads.
fn is_spill(nbt: &Compound) -> bool {
    matches!(nbt.get("id"), Some(Value::String(id)) if id == SPILL_ID)
}

#[cfg(test)]
mod tests;
