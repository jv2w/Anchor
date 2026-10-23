//! Exporting a persistent world as a Bedrock (LevelDB) world folder, the reverse of the import:
//! [`Server::export_world`](crate::Server::export_world) reads the world's file and writes a
//! folder that BDS and the game open, and returns a [`Report`].
//!
//! The world file is opened read-only and read in one read transaction (`storage::Snapshot`), so
//! the folder holds one commit; a file a server has open is refused. Its chunks are read in
//! Bedrock key order (x, then z, little-endian) a step at a time, then decoded and converted on
//! the CPU pool, and the step's values are made into compressed data blocks on the pool by runs
//! of `RUN_BYTES`. The pool keeps input order, and the LevelDB writer (`leveldb`) puts the
//! blocks in tables in that order: the same file gives the same bytes whatever the thread count.
//! A chunk's terrain is its
//! sub-chunk values (`chunk`), or what the world's generator makes when it has none (a chunk whose
//! extras alone were saved); its block entities go as they are saved, in Bedrock form. Entities
//! and scheduled block updates are left out and counted: their saved form is Anchor's. Chunks
//! the file does not hold (generated and never changed) are not written: the `level.dat`
//! (`level_dat`) gives the Bedrock world a flat generator that makes them.
//!
//! Everything is written into `<folder>.tmp`, flushed to disk and renamed to the folder at the
//! end, so the folder appears whole or not at all; a failure removes the temporary folder.

mod chunk;
mod level_dat;
mod leveldb;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use crate::import::MAX_CHUNKS;
use crate::pool::Pool;
use crate::save::{self, WorldSpec};
use crate::storage::codec::{ChunkRecord, WorldRecord};
use crate::storage::seal::unseal;
use crate::storage::{ChunkId, ChunkKind, ChunkPart, Key, Place, Snapshot};
use crate::world::{Chunk, ChunkPos, Generator};

/// Most chunks read, then converted, per step: bounds the chunk data held at once.
const STEP: usize = 256;
/// Read value bytes after which a step ends, if it has not reached [`STEP`] chunks.
const STEP_BYTES: usize = 256 << 20;
/// Value bytes after which a run of a step's values, made into data blocks on one pool item,
/// ends: a few blocks, so few blocks end short at the end of a run.
const RUN_BYTES: usize = 4 * leveldb::BLOCK_BYTES;

/// What [`Server::export_world`](crate::Server::export_world) wrote and what it left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    chunks: u64,
    block_entities: u64,
    entities: u64,
    scheduled_updates: u64,
}

impl Report {
    /// The chunks written: every chunk the world file holds.
    pub fn chunks(&self) -> u64 {
        self.chunks
    }

    /// The block entities written.
    pub fn block_entities(&self) -> u64 {
        self.block_entities
    }

    /// The entities left out: Anchor saves them in its own form.
    pub fn entities(&self) -> u64 {
        self.entities
    }

    /// The scheduled block updates left out: Anchor saves them in its own form.
    pub fn scheduled_updates(&self) -> u64 {
        self.scheduled_updates
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
    /// there. Nothing was written.
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
            Self::NoWorld { name } => write!(f, "the world {name:?} has no saved file"),
            Self::FolderExists { path } => write!(f, "{} is already there", path.display()),
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

/// The error of writing `path`.
fn written(path: &Path) -> impl Fn(io::Error) -> Error + '_ {
    move |source| Error::Write {
        path: path.to_path_buf(),
        source,
    }
}

/// A converted chunk: its values in key order and what it adds to the report.
#[derive(Debug)]
struct Converted {
    values: Vec<(Vec<u8>, Vec<u8>)>,
    block_entities: u64,
    entities: u64,
    scheduled_updates: u64,
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
    let snapshot = Snapshot::open(&Place::world(data_dir, &file).file)?;
    let key = Key::World;
    let record: WorldRecord = match snapshot.world()? {
        Some(sealed) => unseal(&key.to_bytes(), &sealed)?,
        None => return Err(no_world().into()),
    };
    let mut ids = snapshot.chunk_ids(record.number, MAX_CHUNKS)?;
    ids.sort_unstable_by_key(|id| crate::import::chunk::prefix(id.x, id.z));

    // Started before the temporary folder is made, so a pool that fails to start leaves no folder.
    let pool = crate::runtime::cpu_pool(cpu)?;
    let made = (|| {
        if let Some(parent) = temp.parent() {
            std::fs::create_dir_all(parent).map_err(written(parent))?;
        }
        std::fs::create_dir(&temp).map_err(|e| match e.kind() {
            io::ErrorKind::AlreadyExists => Error::FolderExists { path: temp.clone() },
            _ => written(&temp)(e),
        })
    })();
    if let Err(e) = made {
        // Not removed: the folder is not the export's own when it could not be made.
        if !pool.shutdown() {
            warn!("the export's CPU pool threads did not all end");
        }
        return Err(e.into());
    }
    let exported = write(&temp, &snapshot, &pool, &spec, &record, &ids);
    if !pool.shutdown() {
        warn!("the export's CPU pool threads did not all end");
    }
    let finished = exported.and_then(|report| {
        std::fs::rename(&temp, &dir).map_err(written(&dir))?;
        crate::fs::sync_parent(&dir).map_err(written(&dir))?;
        Ok(report)
    });
    let report = match finished {
        Ok(report) => report,
        Err(e) => {
            // The temporary folder is the export's own: it goes whole.
            if let Err(e) = std::fs::remove_dir_all(&temp) {
                warn!(folder = %temp.display(), error = %e, "could not remove the failed export's folder");
            }
            return Err(e);
        }
    };
    info!(
        world = name,
        folder = %dir.display(),
        chunks = report.chunks,
        block_entities = report.block_entities,
        left_out_entities = report.entities,
        left_out_scheduled_updates = report.scheduled_updates,
        "world exported"
    );
    Ok(report)
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
    let seed = spec.seed.unwrap_or_else(|| save::default_seed(&spec.name));
    let mut writer = leveldb::Writer::new(db.clone());
    let mut report = Report::default();
    let mut ids = ids.iter();
    while ids.len() > 0 {
        let mut step = Vec::with_capacity(ids.len().min(STEP));
        let mut bytes = 0usize;
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
        let mut runs: Vec<Vec<(Vec<u8>, Vec<u8>)>> = Vec::new();
        let mut run_bytes = RUN_BYTES;
        for c in converted {
            let c = c?;
            report.chunks += 1;
            report.block_entities += c.block_entities;
            report.entities += c.entities;
            report.scheduled_updates += c.scheduled_updates;
            for entry in c.values {
                if run_bytes >= RUN_BYTES {
                    runs.push(Vec::new());
                    run_bytes = 0;
                }
                run_bytes = run_bytes.saturating_add(entry.0.len() + entry.1.len());
                if let Some(run) = runs.last_mut() {
                    run.push(entry);
                }
            }
        }
        for blocks in pool.map(&runs, |run| leveldb::blocks(run)) {
            for block in blocks.map_err(written(&db))? {
                writer.add(block).map_err(written(&db))?;
            }
        }
    }
    writer.finish().map_err(written(&db))?;
    let level_dat = temp.join("level.dat");
    leveldb::write_new(
        &level_dat,
        &level_dat::encode(&spec.name, record, spec.generator.as_ref()),
    )
    .map_err(written(&level_dat))?;
    crate::fs::sync_parent(&db.join("CURRENT")).map_err(written(&db))?;
    crate::fs::sync_parent(&level_dat).map_err(written(temp))?;
    Ok(report)
}

/// Creates the folder `dir`.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the export writes a world folder instead of running the server"
)]
fn create_dir(dir: &Path) -> Result<(), Error> {
    std::fs::create_dir(dir).map_err(written(dir))
}

/// Converts the chunk `id` from its stored `values` into its Bedrock values: a pure function of
/// its inputs, for the CPU pool. A chunk without terrain values gets what `generator` makes with
/// `seed`.
fn convert(
    id: ChunkId,
    values: &[(ChunkPart, Vec<u8>)],
    generator: &dyn Generator,
    seed: i64,
) -> Result<Converted, Error> {
    let pos = ChunkPos::new(id.x, id.z);
    let at = |source: Box<dyn std::error::Error + Send + Sync>| Error::Chunk {
        x: id.x,
        z: id.z,
        source,
    };
    let (terrain, stored) = save::read_values(id, values).map_err(|(_, e)| at(Box::new(e)))?;
    let record = if terrain.iter().all(Option::is_none) {
        save::to_record(&Chunk::generate(generator, seed, pos))
    } else {
        ChunkRecord {
            sub_chunks: Box::new(terrain.map(|s| s.unwrap_or_else(save::empty_sub_chunk))),
        }
    };
    let extras = &stored.extras;
    let block_entities = extras.get(ChunkKind::BlockEntities);
    let values = chunk::values(pos, &record, block_entities).map_err(|e| at(Box::new(e)))?;
    Ok(Converted {
        values,
        block_entities: block_entities.len() as u64,
        entities: extras.get(ChunkKind::Entities).len() as u64,
        scheduled_updates: extras.get(ChunkKind::ScheduledUpdates).len() as u64,
    })
}

#[cfg(test)]
mod tests;
