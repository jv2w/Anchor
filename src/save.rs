//! The save thread: it loads chunks and player data for the tick and commits the
//! tick's save snapshots, so the tick never touches the disk.
//!
//! The tick sends [`Request`]s and gets [`Response`]s back as inputs of a later tick. Loads read
//! a world's file in one read transaction per batch; chunks it does not hold are generated.
//! Decoding, generating, reading the chunk extras into the tick's typed form ([`ChunkRules`]) and
//! encoding run on the CPU pool. A commit writes one snapshot, taken at
//! one tick boundary, to every data file it changes, one write transaction each, the main file
//! last ([`commit`]); requests are handled in order, so commits land in the order they were
//! taken. Nothing else writes a file: no event (a player leaving, a chunk unloading) saves a part
//! of the state apart. A normal stop commits the last snapshot to every file; a
//! crash between two files of one save leaves them at different saves, which the next start
//! reports ([`check_save`]).
//!
//! A chunk is its terrain and its extras: block entities, entities and scheduled
//! block updates. Every value is under [`MAX_RECORD`] however
//! much a chunk holds: the terrain is one value per sub-chunk (none for an empty
//! sub-chunk of plains, [`seal_terrain`]) and each extras list is
//! split into pages, all under keys that share the chunk's prefix. One load reads them all in
//! one read transaction and answers them together; damage in any of them quarantines the chunk.
//!
//! The operator list, the bans and the whitelist are read once when the files
//! open and live in the tick; a snapshot carries the entries changed since the last one. The
//! user's server data (`user` table) is read once at start ([`Thread::read_user`]) and saved the
//! same way.

use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::block::{PropertyValue, State};
use crate::config::IntegrityCheck;
use crate::custom::Registry;
use crate::import::BLOCK_STATE_VERSION;
use crate::nbt::{self, Compound, Value};
use crate::permission::{self, BanEntry};
use crate::pool::Pool;
use crate::storage::codec::MAX_EXTRA_ENTRIES;
use crate::storage::codec::{
    BanRecord, Biomes, BlockLayer, ChunkRecord, ExtraRecord, Mark, Meta, PlayerRecord,
    SubChunkRecord, UserRecord, WorldRecord,
};
use crate::storage::seal::{Crc32c, MAX_RECORD, seal, seal_encoded, stored_checksum, unseal};
use crate::storage::{
    self, AccessKey, AccessKind, Batch, CHUNK_PREFIX, Checked, ChunkId, ChunkKey, ChunkKind,
    ChunkPart, ChunkValues, Key, MAX_WORLDS, Place, PlayerKey, Start, StartBackup, Storage,
};
use crate::tick::{ChunkRules, EntityTypes};
use crate::world::{
    BlockPos, Chunk, ChunkPos, Entry, Generator, Kind, PLAINS, Paletted, SUB_CHUNKS, SubChunk,
    World,
};

/// A world of the server, by its place in the tick's world list (the default world is 0).
pub(crate) type WorldIndex = usize;

/// Requests the save thread's queue holds. The tick sends at most one load batch per tick and
/// one snapshot at a time, so it fills only if the thread falls far behind.
pub(crate) const REQUEST_CAPACITY: usize = 64;
/// Answers the tick's queue holds: every chunk load in flight and the player loads and commit
/// answers on top, so the save thread never waits on a full queue in normal running.
pub(crate) const RESPONSE_CAPACITY: usize = 2048;

/// What the tick asks the save thread for.
#[derive(Debug)]
pub(crate) enum Request {
    /// Chunks to load or generate, in priority order.
    Chunks(Vec<(WorldIndex, ChunkPos)>),
    /// Worlds made or loaded while the server runs, in order: the save thread loads, generates
    /// and commits their chunks from now on. It comes before any request that names them.
    Worlds(Vec<NewWorld>),
    /// Worlds deleted while the server runs, in order: the save thread forgets them and deletes
    /// their files. It comes after every commit that writes them.
    DeleteWorlds(Vec<DeletedWorld>),
    /// The saved data of a player.
    Player(PlayerKey),
    /// A snapshot to commit.
    Commit(Box<Snapshot>),
    /// The tick stopped normally; nothing follows. Without it the tick died, and the save thread
    /// commits nothing more.
    Finish,
}

/// What the save thread answers.
#[derive(Debug)]
pub(crate) enum Response {
    /// A requested chunk.
    Chunk {
        world: WorldIndex,
        pos: ChunkPos,
        loaded: Loaded,
    },
    /// The saved data of a requested player.
    Player { key: PlayerKey, data: PlayerLoad },
    /// The snapshot taken before tick `start` is committed.
    Committed { start: u64 },
    /// The snapshot taken before tick `start` could not be committed, even after reopening the
    /// file: the server stops without saving.
    CommitFailed { start: u64 },
    /// A batch of chunks could not be read, even after reopening the file: the server stops
    /// without saving, as for a failed commit.
    LoadFailed,
    /// What the check a snapshot asked for found (`/check`).
    Checked(Checked),
}

/// Where a loaded chunk came from. The extras come from the file either way.
#[derive(Debug)]
pub(crate) enum Loaded {
    /// The terrain was read from the server file.
    Disk(Box<Chunk>, Stored),
    /// The terrain was never saved, so it was generated.
    Generated(Box<Chunk>, Stored),
    /// Saved, but the terrain or an extra is damaged or unreadable: the position is quarantined
    /// and the saved bytes are kept.
    Damaged,
}

/// The extras of one chunk: one raw NBT list per kind of [`ChunkKind::EXTRAS`].
/// An empty list is no value in the file.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ChunkExtras([Vec<Compound>; 3]);

impl ChunkExtras {
    /// The list of `kind`.
    pub(crate) fn get(&self, kind: ChunkKind) -> &[Compound] {
        let [block_entities, entities, updates] = &self.0;
        match kind {
            ChunkKind::BlockEntities => block_entities,
            ChunkKind::Entities => entities,
            ChunkKind::ScheduledUpdates => updates,
        }
    }

    /// The list of `kind` to change.
    pub(crate) fn get_mut(&mut self, kind: ChunkKind) -> &mut Vec<Compound> {
        let [block_entities, entities, updates] = &mut self.0;
        match kind {
            ChunkKind::BlockEntities => block_entities,
            ChunkKind::Entities => entities,
            ChunkKind::ScheduledUpdates => updates,
        }
    }
}

/// What a load read from the server file for a chunk besides its terrain.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Stored {
    /// The chunk's extras; empty when the file has none.
    pub(crate) extras: ChunkExtras,
    /// The extras read into the rule modules' typed form on the CPU pool, so the tick only puts
    /// them in its stores.
    pub(crate) rules: ChunkRules,
    /// What the input log keeps of the values read: CRC-32C over the key bytes
    /// after the chunk's prefix and the seal checksum of each value found, in key order; 0 when
    /// none was found. A replay from the same file reads the same bytes and gets the same value.
    pub(crate) checksum: u32,
}

/// The saved data of a player.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PlayerLoad {
    Found(PlayerData),
    /// Never saved: a new player.
    Absent,
    /// Saved, but damaged or unreadable: the player may not join until an operator repairs it.
    Damaged,
}

/// What Anchor keeps of a player between joins.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlayerData {
    /// The name of the world the player is in.
    pub(crate) world: String,
    /// The eye position, finite.
    pub(crate) position: [f64; 3],
    /// What the rule modules keep of the player, each under its own keys (`tick::extra`).
    pub(crate) extra: Compound,
}

/// The state of a tick boundary that a commit writes: everything changed since the last
/// snapshot.
#[derive(Debug, Default)]
pub(crate) struct Snapshot {
    /// The tick the snapshot was taken before; the engine starts there after a restart.
    pub(crate) start: u64,
    /// The commit identity of the state at `start` ([`InputLog::identity`](crate::tick::InputLog::identity)).
    pub(crate) identity: u64,
    /// Changed chunks of persistent worlds. They share their sub-chunks with the live world.
    pub(crate) chunks: Vec<(WorldIndex, ChunkPos, Chunk)>,
    /// Changed player data.
    pub(crate) players: Vec<(PlayerKey, PlayerData)>,
    /// Changed extras lists of chunks of persistent worlds; an empty list deletes the value.
    pub(crate) extras: Vec<(WorldIndex, ChunkPos, ChunkKind, Vec<Compound>)>,
    /// Worlds deleted since the last request that named them, as [`Request::DeleteWorlds`] gives
    /// them: the commit forgets them first, as a world in `new_worlds` may take a deleted one's
    /// name and index.
    pub(crate) deleted_worlds: Vec<DeletedWorld>,
    /// Worlds made or loaded since the last request that named them, as [`Request::Worlds`]
    /// gives them: the commit learns them before it writes their records.
    pub(crate) new_worlds: Vec<NewWorld>,
    /// What the record of every persistent world holds of its state, loaded or not.
    pub(crate) worlds: Vec<(WorldIndex, WorldValues)>,
    /// Changed operator, ban and whitelist entries, each once.
    pub(crate) access: Vec<(AccessKey, AccessWrite)>,
    /// Changed server data values of the user: name and JSON, each once.
    pub(crate) user: Vec<(String, Vec<u8>)>,
    /// Whether an online backup of the file is made once this snapshot is committed
    /// ([`Context::backup`](crate::Context::backup)).
    pub(crate) backup: bool,
    /// Whether a check of the file starts once this snapshot is committed (`/check`).
    pub(crate) check: bool,
}

/// What a world record keeps of the tick's world (`World`): its state. Its settings (seed,
/// generator, difficulty, time stop, weather) are not saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorldValues {
    /// `World::time_ticks`.
    pub(crate) time: i64,
    /// `World::spawn`.
    pub(crate) spawn: BlockPos,
}

impl WorldValues {
    /// What a record keeps of `world` now.
    pub(crate) fn of(world: &World) -> Self {
        Self {
            time: world.time_ticks(),
            spawn: world.spawn(),
        }
    }
}

/// A world the tick made or loaded while the server runs: what the save thread loads,
/// generates and commits its chunks with. A new one has the next index; one with an index the
/// save thread has replaces its plan (a temporary world made again).
pub(crate) struct NewWorld {
    pub(crate) index: WorldIndex,
    pub(crate) name: String,
    /// The `chunks` key number of a persistent world.
    pub(crate) number: Option<u32>,
    pub(crate) seed: i64,
    pub(crate) generator: Arc<dyn Generator>,
}

impl std::fmt::Debug for NewWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewWorld")
            .field("index", &self.index)
            .field("name", &self.name)
            .field("number", &self.number)
            .field("seed", &self.seed)
            .field("generator", &self.generator.name())
            .finish()
    }
}

/// A persistent world the tick deleted ([`Context::delete_world`](crate::Context::delete_world)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeletedWorld {
    /// Its name, in the letter case the program gave; its file is [`world_place`]'s.
    pub(crate) name: String,
    /// Its index, if the tick had it this run.
    pub(crate) index: Option<WorldIndex>,
}

/// What a commit writes for an `access` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AccessWrite {
    /// The entry is gone.
    Delete,
    /// An operator or whitelist entry.
    Mark,
    /// A ban.
    Ban(BanEntry),
}

/// The operator list, the bans and the whitelist as the server file holds them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AccessData {
    /// Operator names, ASCII lowercase.
    pub(crate) ops: Vec<String>,
    /// Name bans.
    pub(crate) bans: Vec<BanEntry>,
    /// IP bans.
    pub(crate) ip_bans: Vec<BanEntry>,
    /// Whitelisted names, ASCII lowercase.
    pub(crate) whitelist: Vec<String>,
}

/// A world the server runs, as configured: its settings, the same at every start.
pub(crate) struct WorldSpec {
    pub(crate) name: String,
    pub(crate) kind: Kind,
    pub(crate) generator: Arc<dyn Generator>,
    /// The seed; `None` is [`default_seed`] of the name.
    pub(crate) seed: Option<i64>,
    /// The spawn (`[worlds.<name>] spawn`); `None` is the generator's.
    pub(crate) spawn: Option<BlockPos>,
}

impl std::fmt::Debug for WorldSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorldSpec")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("generator", &self.generator.name())
            .field("seed", &self.seed)
            .field("spawn", &self.spawn)
            .finish()
    }
}

/// The seed of a world whose settings give none: made from its name, so it is the same at every
/// start (nothing saves it) and differs between worlds.
pub(crate) fn default_seed(name: &str) -> i64 {
    let [b0, b1, b2, b3, b4, b5, b6, b7, ..] = crate::crypto::sha256(&[name.as_bytes()]);
    i64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7])
}

/// The settings the world file of the world `name` is read or written with when the server does
/// not run (an import, an export): a declared world's, else the generator of the default world
/// (the first of `worlds`) and no seed of its own. `None` for a world declared temporary,
/// which has no file.
pub(crate) fn file_world(worlds: &[WorldSpec], name: &str) -> Option<WorldSpec> {
    let declared = worlds.iter().find(|w| w.name == name);
    if declared.is_some_and(|w| w.kind != Kind::Persistent) {
        return None;
    }
    let base = declared.or(worlds.first())?;
    Some(WorldSpec {
        name: name.to_owned(),
        kind: Kind::Persistent,
        generator: Arc::clone(&base.generator),
        seed: declared.and_then(|w| w.seed),
        spawn: None,
    })
}

/// A world the server runs, as its settings make it: `seed`, else [`default_seed`]; `spawn`,
/// else the generator's. A stored world goes on from its saved state ([`resume`]), its spawn
/// among it.
pub(crate) fn new_world(
    name: &str,
    kind: Kind,
    generator: &dyn Generator,
    seed: Option<i64>,
    spawn: Option<BlockPos>,
) -> World {
    let seed = seed.unwrap_or_else(|| default_seed(name));
    let spawn = spawn.unwrap_or_else(|| generator.spawn(seed));
    World::new(name, kind, seed, spawn)
}

/// Gives `world` the state its stored `record` keeps: its number, time and spawn.
pub(crate) fn resume(world: &mut World, record: WorldRecord) {
    world.set_number(Some(record.number));
    world.set_time(record.time);
    world.set_spawn(record.spawn);
}

/// A world as the save thread sees it.
struct Plan {
    name: String,
    /// The `chunks` key number of a persistent world: the one its file holds, or the tick's for a
    /// new file.
    number: Option<u32>,
    seed: i64,
    generator: Arc<dyn Generator>,
    /// The open file of a persistent world; `None` for a temporary one, and for a persistent one
    /// whose file failed to open (its chunks load damaged, so nothing of it changes or is saved).
    file: Option<Open>,
    /// What the world's record in its file holds, once read or written.
    values: Option<WorldValues>,
}

/// An open data file and where it is.
struct Open {
    storage: Storage,
    place: Place,
}

/// How the save thread opens the file of a world it learns of.
enum Opener {
    /// The world's file of the data folder ([`world_place`]), opened as the start says.
    Live { data_dir: PathBuf, start: Start },
    /// A replay's read-only copies, by [`storage::world_name_key`] of the world name.
    Replay(BTreeMap<String, PathBuf>),
}

impl Opener {
    /// Opens the file of the world `name`.
    fn open(&self, name: &str) -> Result<Open, storage::Error> {
        match self {
            Self::Live { data_dir, start } => {
                let place = world_place(data_dir, name)?;
                let storage = Storage::open(&place, *start)?;
                Ok(Open { storage, place })
            }
            Self::Replay(files) => {
                let file = files
                    .get(&storage::world_name_key(name))
                    .ok_or(storage::Error::Closed)?;
                Ok(Open {
                    storage: Storage::open_replica(file)?,
                    place: Place {
                        file: file.clone(),
                        backups: PathBuf::new(),
                    },
                })
            }
        }
    }
}

/// The plan of a world named `name`: a persistent one (with `number`, the tick's) opens its
/// file, whose record gives its number, time and spawn, and whose last save is checked against
/// the main file's (`saved`; a file holding a later one is added to `later`).
fn plan(
    opener: &Opener,
    saved: &Meta,
    later: &mut Vec<PathBuf>,
    name: &str,
    number: Option<u32>,
    seed: i64,
    generator: Arc<dyn Generator>,
) -> Plan {
    let mut plan = Plan {
        name: name.to_owned(),
        number,
        seed,
        generator,
        file: None,
        values: None,
    };
    if number.is_none() {
        return plan;
    }
    let read = opener.open(name).and_then(|mut file| {
        let (record, meta) = world_file(&mut file.storage)?;
        Ok((file, record, meta))
    });
    match read {
        Ok((open, record, meta)) => {
            if let Some(record) = record {
                plan.number = Some(record.number);
                plan.values = Some(WorldValues {
                    time: record.time,
                    spawn: record.spawn,
                });
            }
            if check_save(saved, &open.place.file, meta.as_ref()) {
                later.push(open.place.file.clone());
            }
            plan.file = Some(open);
        }
        Err(e) => error!(
            world = name,
            error = %e,
            "world file did not open; its chunks are quarantined and nothing of it is saved"
        ),
    }
    plan
}

/// The record and the meta value of the world file `storage`, if it holds them.
fn world_file(
    storage: &mut Storage,
) -> Result<(Option<WorldRecord>, Option<Meta>), storage::Error> {
    let mut values = storage.read(&[Key::World, Key::Meta])?.into_iter();
    let mut next = |key: Key| -> Result<_, storage::Error> {
        let Some(value) = values.next() else {
            return Ok(None);
        };
        value?
            .map(|sealed| Ok::<_, storage::Error>((key, sealed)))
            .transpose()
    };
    let record = next(Key::World)?
        .map(|(key, sealed)| unseal::<WorldRecord>(&key.to_bytes(), &sealed))
        .transpose()?;
    let meta = next(Key::Meta)?
        .map(|(key, sealed)| unseal::<Meta>(&key.to_bytes(), &sealed))
        .transpose()?;
    Ok((record, meta))
}

/// Warns, loudly, when the data file `file` holds a later save (`meta`) than the main file's last
/// one (`saved`): a save stopped between its files (a crash or a kill), so the files may hold
/// different moments. The main file is committed last, so every file a
/// save reached before it stopped is later than the main file. Returns whether it warned.
fn check_save(saved: &Meta, file: &std::path::Path, meta: Option<&Meta>) -> bool {
    let Some(meta) = meta else {
        return false;
    };
    let later = meta.tick > saved.tick
        || (meta.tick == saved.tick && meta.identity.is_some() && meta.identity != saved.identity);
    if later {
        error!(
            file = %file.display(),
            file_save_tick = meta.tick,
            main_save_tick = saved.tick,
            "data files disagree: this file holds a later save than the main file. The server \
             stopped in the middle of a save (a crash or a kill), so this file is from save tick \
             {file_tick} while the others are from save tick {main_tick} or earlier: items may \
             be doubled or lost between them. Restore the files from one backup if that matters.",
            file_tick = meta.tick,
            main_tick = saved.tick,
        );
    }
    later
}

/// Learns the worlds the tick made or loaded: a new one is the next index (the tick numbers its
/// worlds in order and names each before any request about it), one made again replaces its plan.
fn add_worlds(plans: &mut Vec<Plan>, opener: &Opener, saved: &Meta, worlds: &[NewWorld]) {
    for world in worlds {
        let plan = plan(
            opener,
            saved,
            // Files the server did not open at start were checked then.
            &mut Vec::new(),
            &world.name,
            world.number,
            world.seed,
            Arc::clone(&world.generator),
        );
        match plans.get_mut(world.index) {
            Some(slot) => *slot = plan,
            None => plans.push(plan),
        }
    }
}

/// Deletes the files of the worlds the tick deleted: by name, and the plan at the index the tick
/// had it at, if any. A replay deletes no file.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread deletes a world's file"
)]
fn delete_worlds(plans: &mut [Plan], opener: &Opener, worlds: &[DeletedWorld]) {
    for world in worlds {
        if let Some(plan) = world.index.and_then(|i| plans.get_mut(i)) {
            // Closed before the file goes: Windows cannot remove an open file.
            plan.file = None;
            plan.number = None;
        }
        let Opener::Live { data_dir, .. } = opener else {
            continue;
        };
        let file = match world_file_name(data_dir, &world.name) {
            Ok(Some(name)) => Place::world(data_dir, &name).file,
            Ok(None) => continue,
            Err(e) => {
                error!(world = world.name, error = %e, "could not delete the world file");
                continue;
            }
        };
        match std::fs::remove_file(&file).and_then(|()| crate::fs::sync_parent(&file)) {
            Ok(()) => info!(world = world.name, file = %file.display(), "world file deleted"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => error!(
                world = world.name,
                file = %file.display(),
                error = %e,
                "could not delete the world file"
            ),
        }
    }
}

/// What the tick needs to make and load worlds while it runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Catalog {
    /// The persistent worlds of `data/worlds/` that no spec names in any letter case, by their
    /// file's name.
    pub(crate) stored: Vec<(String, WorldRecord)>,
    /// The `chunks` key number of the next new persistent world.
    pub(crate) next_number: u32,
}

/// The data files and the worlds, ready to run.
pub(crate) struct Opened {
    /// The worlds for the tick, in the order of the specs.
    pub(crate) worlds: Vec<World>,
    /// What the tick makes and loads worlds with.
    pub(crate) catalog: Catalog,
    /// The engine tick of the last save (0 for new files).
    pub(crate) tick: u64,
    /// The commit identity of the last save; `None` for new files.
    pub(crate) identity: Option<u64>,
    /// The operator list, the bans and the whitelist.
    pub(crate) access: AccessData,
    /// The data files holding a later save than the main file ([`check_save`]).
    pub(crate) later: Vec<PathBuf>,
    /// The save thread's side.
    pub(crate) thread: Thread,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("worlds", &self.worlds.len())
            .field("tick", &self.tick)
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

/// The server's data files besides the worlds'.
struct Files {
    /// The save's tick and commit identity, the operators, bans and whitelist, and the user's
    /// server data, committed last.
    main: Open,
    players: Open,
}

/// The data files a replay starts from: each opened read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplayFiles {
    pub(crate) main: PathBuf,
    pub(crate) players: PathBuf,
    /// The persistent worlds' files, by their file's name in `data/worlds/`.
    pub(crate) worlds: BTreeMap<String, PathBuf>,
}

/// What the save thread runs with.
pub(crate) struct Thread {
    files: Files,
    plans: Vec<Plan>,
    opener: Opener,
    /// The meta value of the main file's last commit.
    saved: Meta,
    /// Whether loaded and generated chunks are lit on the CPU pool (`lighting`).
    lighting: bool,
    /// The server's custom blocks, which saved unknown states are resolved against on the CPU
    /// pool as chunks load, before they are lit.
    custom: Arc<Registry>,
    /// The server's custom entity types, which saved entities are read with on the CPU pool.
    entity_types: Arc<EntityTypes>,
    /// The check being made over the open files (`/check`): what the files checked so far
    /// found.
    check: Option<Checked>,
}

impl Thread {
    /// Gives the thread the server's custom content and custom entity types (none by default).
    pub(crate) fn set_custom(&mut self, custom: Arc<Registry>, entity_types: Arc<EntityTypes>) {
        self.custom = custom;
        self.entity_types = entity_types;
    }

    /// Sets whether chunks are lit as they load (`lighting`, on by default). With it off the
    /// tick ignores light, so lighting here would only waste CPU time.
    pub(crate) fn set_lighting(&mut self, enabled: bool) {
        self.lighting = enabled;
    }

    /// Loads chunks as the save thread does for a [`Request::Chunks`] (a replay's loads after its
    /// start). A replay calls it on its own thread.
    pub(crate) fn load(
        &mut self,
        pool: &Pool,
        keys: &[(WorldIndex, ChunkPos)],
    ) -> Result<Vec<Response>, storage::Error> {
        let prepare = Prepare::Tick {
            lighting: self.lighting,
            entity_types: &self.entity_types,
        };
        let mut answers = Vec::with_capacity(keys.len());
        load_chunks(&mut self.plans, pool, keys, &self.custom, prepare, |a| {
            answers.push(a);
            true
        })?;
        Ok(answers)
    }

    /// Loads chunks as they are stored: neither lit nor their extras read (a replay's start:
    /// the chunks its checkpoint names take their light and typed state from it).
    pub(crate) fn load_stored(
        &mut self,
        pool: &Pool,
        keys: &[(WorldIndex, ChunkPos)],
    ) -> Result<Vec<Response>, storage::Error> {
        let mut answers = Vec::with_capacity(keys.len());
        load_chunks(
            &mut self.plans,
            pool,
            keys,
            &self.custom,
            Prepare::Stored,
            |a| {
                answers.push(a);
                true
            },
        )?;
        Ok(answers)
    }

    /// Commits `snapshot` as the save thread does (a replay's files keep it in memory).
    pub(crate) fn commit(
        &mut self,
        pool: &Pool,
        snapshot: &Snapshot,
    ) -> Result<(), storage::Error> {
        commit(self, pool, snapshot)
    }

    /// Learns the worlds of a [`Request::Worlds`] as the save thread does (a replay's requests).
    pub(crate) fn add_worlds(&mut self, worlds: &[NewWorld]) {
        add_worlds(&mut self.plans, &self.opener, &self.saved, worlds);
    }

    /// Forgets the worlds of a [`Request::DeleteWorlds`] as the save thread does; a replay
    /// deletes no file.
    pub(crate) fn delete_worlds(&mut self, worlds: &[DeletedWorld]) {
        delete_worlds(&mut self.plans, &self.opener, worlds);
    }

    /// Reads the user's server data values `names`, before the threads start: the
    /// JSON of each, or `None` for a value never saved. A damaged value fails the start and stays
    /// as it is; values of names not asked for are neither read nor touched.
    pub(crate) fn read_user(
        &mut self,
        names: &[&str],
    ) -> Result<Vec<Option<Vec<u8>>>, storage::Error> {
        let keys: Vec<Key> = names.iter().map(|&n| Key::User(n.to_owned())).collect();
        let values = self.files.main.storage.read(&keys)?;
        keys.iter()
            .zip(values)
            .map(|(key, value)| {
                value?
                    .map(|sealed| {
                        unseal::<UserRecord<'_>>(&key.to_bytes(), &sealed).map(|r| r.0.into_owned())
                    })
                    .transpose()
            })
            .collect()
    }

    /// The open file `name`: a server file's name ([`storage::MAIN_FILE`], ...) or a world's.
    #[cfg(test)]
    pub(crate) fn file(&mut self, name: &str) -> &mut Storage {
        let Files { main, players } = &mut self.files;
        let file = match name {
            storage::MAIN_FILE => main,
            storage::PLAYERS_FILE => players,
            _ => self
                .plans
                .iter_mut()
                .find(|p| p.name == name)
                .and_then(|p| p.file.as_mut())
                .expect("an open world file"),
        };
        &mut file.storage
    }

    /// Every open data file: the server's, then the worlds'.
    fn files(&mut self) -> impl Iterator<Item = &mut Open> {
        let Files { main, players } = &mut self.files;
        [main, players]
            .into_iter()
            .chain(self.plans.iter_mut().filter_map(|p| p.file.as_mut()))
    }
}

/// Opens the data files (backup and integrity check as `start` says) and resolves the worlds:
/// each gets the settings of its spec ([`new_world`]); a persistent world whose file
/// `data/worlds/` holds keeps its time and number, a new one starts at time 0 with the next free
/// number. The files of the start (main, players and each persistent world's) are checked
/// together on `pool` first ([`check_all`]), so a damaged one refuses the start before any file
/// is written. The other world files of `data/worlds/` are listed for the
/// tick to load ([`Catalog::stored`]); their record is read without writing them, unless a crash
/// left one to repair, which opens it as a start does, without a start backup (it gets one when
/// it is loaded). Every file holding a later save than the main file is reported
/// ([`check_save`]). Runs before the threads start.
pub(crate) fn open_with(
    data_dir: &std::path::Path,
    specs: Vec<WorldSpec>,
    start: Start,
    pool: &Pool,
) -> Result<Opened, crate::Error> {
    let mut places = vec![
        Place::data(data_dir, storage::MAIN_FILE),
        Place::data(data_dir, storage::PLAYERS_FILE),
    ];
    for spec in specs.iter().filter(|s| s.kind == Kind::Persistent) {
        places.push(world_place(data_dir, &spec.name)?);
    }
    check_all(&places, start.check, pool)?;
    // Checked above: their opens check the format alone.
    let checked = Start {
        check: IntegrityCheck::Never,
        ..start
    };
    let open = |name: &str| -> Result<Open, storage::Error> {
        let place = Place::data(data_dir, name);
        Ok(Open {
            storage: Storage::open(&place, checked)?,
            place,
        })
    };
    let mut main = open(storage::MAIN_FILE)?;
    let saved = stored_meta(&mut main.storage)?;
    let mut files = Files {
        main,
        players: open(storage::PLAYERS_FILE)?,
    };
    let mut later = Vec::new();
    let meta = stored_meta(&mut files.players.storage)?;
    if check_save(&saved, &files.players.place.file, Some(&meta)) {
        later.push(files.players.place.file.clone());
    }
    let mut stored = Vec::new();
    for name in world_names(data_dir)? {
        let key = storage::world_name_key(&name);
        if specs
            .iter()
            .any(|s| s.kind == Kind::Persistent && storage::world_name_key(&s.name) == key)
        {
            continue;
        }
        let place = Place::world(data_dir, &name);
        let read = match Storage::open_replica(&place.file) {
            Ok(mut storage) => world_file(&mut storage),
            // A crash left it for redb to repair, which only an open that writes does.
            Err(e) if !e.is_damage() => {
                let repair = Start {
                    backup: StartBackup::Skip,
                    ..start
                };
                Storage::open(&place, repair).and_then(|mut storage| {
                    let read = world_file(&mut storage)?;
                    storage.close()?;
                    Ok(read)
                })
            }
            Err(e) => Err(e),
        };
        match read {
            Ok((Some(record), meta)) => {
                if check_save(&saved, &place.file, meta.as_ref()) {
                    later.push(place.file.clone());
                }
                stored.push((name, record));
            }
            Ok((None, _)) => {
                warn!(file = %place.file.display(), "world file without a world record; it is left out");
            }
            Err(e) => error!(
                file = %place.file.display(),
                error = %e,
                "world file did not open; it is left out and as it is"
            ),
        }
    }
    let at_start = Opener::Live {
        data_dir: data_dir.to_path_buf(),
        start: checked,
    };
    let mut resolved = resolve(files, at_start, saved, later, stored, specs, None)?;
    // Worlds opened while the server runs are checked as the start says.
    resolved.thread.opener = Opener::Live {
        data_dir: data_dir.to_path_buf(),
        start,
    };
    Ok(resolved)
}

/// Checks the closed files at `places` together on `pool` ([`storage::verify`]), each in a turn
/// of its own. Every refused file is logged; the first one refuses the start. None is written.
fn check_all(places: &[Place], check: IntegrityCheck, pool: &Pool) -> Result<(), storage::Error> {
    let checked = pool.map_in_turns(places, |place| storage::verify(&place.file, check));
    let mut refused = None;
    for (place, result) in places.iter().zip(checked) {
        if let Err(e) = result {
            error!(file = %place.file.display(), error = %e, "data file refused; it is left as it was");
            refused.get_or_insert(e);
        }
    }
    refused.map_or(Ok(()), Err)
}

/// The names of the world files in `data/worlds/`, in name order: each file `<name>.redb` whose
/// name is a world name ([`storage::is_world_file_name`]); of names that differ only in letter
/// case, the first. Others are left out with a warning.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the data files open before the threads start"
)]
fn world_names(data_dir: &std::path::Path) -> Result<Vec<String>, storage::Error> {
    let dir = data_dir.join(storage::WORLD_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(storage::Error::Io(e)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(storage::Error::Io)?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(storage::EXTENSION) {
            continue;
        }
        match path.file_stem().and_then(|s| s.to_str()) {
            Some(name) if storage::is_world_file_name(name) => names.push(name.to_owned()),
            _ => {
                warn!(file = %path.display(), "a file of the world folder is not named as a world; it is left out");
            }
        }
    }
    names.sort_unstable();
    let mut seen = std::collections::BTreeSet::new();
    names.retain(|name| {
        let new = seen.insert(storage::world_name_key(name));
        if !new {
            warn!(
                world = name,
                "a world file differs from another only in letter case; it is left out"
            );
        }
        new
    });
    if names.len() > MAX_WORLDS {
        return Err(storage::Error::TooManyWorlds);
    }
    Ok(names)
}

/// The names of the world files of `data/worlds/`, in name order ([`world_names`]).
pub(crate) fn world_files(data_dir: &std::path::Path) -> Result<Vec<String>, storage::Error> {
    world_names(data_dir)
}

/// The name of the world file of `data/worlds/` that is the world `name`'s: the one whose name
/// differs from `name` at most in letter case ([`world_names`]), if any.
pub(crate) fn world_file_name(
    data_dir: &std::path::Path,
    name: &str,
) -> Result<Option<String>, storage::Error> {
    Ok(file_of(&world_names(data_dir)?, name).cloned())
}

/// Of the world file names `names`, the one whose name differs from `name` at most in letter
/// case, if any.
fn file_of<'a>(names: &'a [String], name: &str) -> Option<&'a String> {
    let key = storage::world_name_key(name);
    names.iter().find(|n| storage::world_name_key(n) == key)
}

/// Whether `data/worlds/` holds the file of the world `name`, or of one whose name differs only in
/// letter case.
pub(crate) fn world_file_exists(
    data_dir: &std::path::Path,
    name: &str,
) -> Result<bool, storage::Error> {
    Ok(world_file_name(data_dir, name)?.is_some())
}

/// The file of the world `name` and its backup folder: its world file's ([`world_file_name`]),
/// else `data/worlds/<name>.redb` for a new one. A case-sensitive file system would otherwise
/// give a name in another letter case a second file and backup folder. A new one is refused
/// while the folder holds [`MAX_WORLDS`] world files, as [`world_names`] refuses a folder holding
/// more.
fn world_place(data_dir: &std::path::Path, name: &str) -> Result<Place, storage::Error> {
    let names = world_names(data_dir)?;
    match file_of(&names, name) {
        Some(file) => Ok(Place::world(data_dir, file)),
        None if names.len() < MAX_WORLDS => Ok(Place::world(data_dir, name)),
        None => Err(storage::Error::TooManyWorlds),
    }
}

/// [`open_with`] a start backup and a check, as the tests open data files.
#[cfg(test)]
#[expect(
    clippy::panic_in_result_fn,
    reason = "consistency: a test helper fails by panicking when its pool does not stop"
)]
pub(crate) fn open(
    data_dir: &std::path::Path,
    specs: Vec<WorldSpec>,
) -> Result<Opened, crate::Error> {
    let pool = crate::runtime::cpu_pool(2).unwrap();
    let opened = open_with(data_dir, specs, Start::checked(StartBackup::Make), &pool);
    assert!(pool.shutdown());
    opened
}

/// Opens the data files `files` read-only for a replay ([`Storage::open_replica`]) and resolves
/// the worlds as [`open_with`] does, with the seeds a checkpoint kept (`seeds`, by world).
/// Commits of the thread stay in memory.
pub(crate) fn open_replica(
    files: &ReplayFiles,
    specs: Vec<WorldSpec>,
    seeds: &[i64],
) -> Result<Opened, crate::Error> {
    let open = |file: &PathBuf| -> Result<Open, storage::Error> {
        Ok(Open {
            storage: Storage::open_replica(file)?,
            place: Place {
                file: file.clone(),
                backups: PathBuf::new(),
            },
        })
    };
    let mut main = open(&files.main)?;
    let saved = stored_meta(&mut main.storage)?;
    let files_open = Files {
        main,
        players: open(&files.players)?,
    };
    let mut stored = Vec::new();
    for (name, file) in &files.worlds {
        if let (Some(record), _) = world_file(&mut Storage::open_replica(file)?)? {
            stored.push((name.clone(), record));
        }
    }
    let opener = Opener::Replay(
        files
            .worlds
            .iter()
            .map(|(name, file)| (storage::world_name_key(name), file.clone()))
            .collect(),
    );
    resolve(
        files_open,
        opener,
        saved,
        Vec::new(),
        stored,
        specs,
        Some(seeds),
    )
}

/// The engine tick and commit identity of the last commit of the data file `path`, opened
/// read-only (a replay looks for the files of its segment's state).
pub(crate) fn committed_meta(path: &std::path::Path) -> Result<Meta, crate::Error> {
    Ok(stored_meta(&mut Storage::open_replica(path)?)?)
}

/// The engine tick of the last save of the data folder `data_dir` (its main file).
#[cfg(test)]
pub(crate) fn committed_tick(data_dir: &std::path::Path) -> Result<u64, crate::Error> {
    Ok(committed_meta(&Place::data(data_dir, storage::MAIN_FILE).file)?.tick)
}

/// Resolves the worlds of `specs` against the open `files` and the stored worlds `stored` (see
/// [`open_with`]); `seeds`, by world, replaces every world's seed.
fn resolve(
    mut files: Files,
    opener: Opener,
    saved: Meta,
    mut later: Vec<PathBuf>,
    mut stored: Vec<(String, WorldRecord)>,
    specs: Vec<WorldSpec>,
    seeds: Option<&[i64]>,
) -> Result<Opened, crate::Error> {
    let access = read_access(&mut files.main.storage)?;
    let mut plans = Vec::with_capacity(specs.len());
    for (index, spec) in specs.iter().enumerate() {
        let given = seeds.and_then(|s| s.get(index).copied());
        let seed = given
            .or(spec.seed)
            .unwrap_or_else(|| default_seed(&spec.name));
        let number = (spec.kind == Kind::Persistent).then_some(0);
        plans.push(plan(
            &opener,
            &saved,
            &mut later,
            &spec.name,
            number,
            seed,
            Arc::clone(&spec.generator),
        ));
    }
    let numbers = stored.iter().map(|(_, r)| r.number).chain(
        plans
            .iter()
            .filter(|p| p.values.is_some())
            .filter_map(|p| p.number),
    );
    let mut next_number = next_world_number(numbers);
    let mut worlds = Vec::with_capacity(specs.len());
    for ((index, spec), plan) in specs.into_iter().enumerate().zip(&mut plans) {
        let given = seeds.and_then(|s| s.get(index).copied());
        let mut world = new_world(
            &spec.name,
            spec.kind,
            spec.generator.as_ref(),
            given.or(spec.seed),
            spec.spawn,
        );
        let new = plan.values.is_none();
        match (plan.number, plan.values) {
            (Some(number), Some(WorldValues { time, spawn })) => resume(
                &mut world,
                WorldRecord {
                    number,
                    time,
                    spawn,
                },
            ),
            (Some(_), None) => {
                // A new file: its keys carry the next free number.
                plan.number = Some(next_number);
                world.set_number(Some(next_number));
                next_number = next_number.saturating_add(1);
            }
            (None, _) => {}
        }
        info!(world = spec.name, kind = ?spec.kind, seed = world.seed(), new, "world loaded");
        world.set_generator(spec.generator.name());
        worlds.push(world);
    }
    stored.retain(|(name, _)| {
        let key = storage::world_name_key(name);
        !worlds
            .iter()
            .any(|w| storage::world_name_key(w.name()) == key)
    });
    Ok(Opened {
        worlds,
        catalog: Catalog {
            stored,
            next_number,
        },
        tick: saved.tick,
        identity: saved.identity,
        access,
        later,
        thread: Thread {
            files,
            plans,
            opener,
            saved,
            lighting: true,
            custom: Arc::default(),
            entity_types: Arc::default(),
            check: None,
        },
    })
}

/// Reads the operator list, the bans and the whitelist. Values are external input: a key must
/// be a valid list name in ASCII lowercase (an IP address in its canonical text for an IP ban)
/// and its value must be the record of its kind; anything else is damage and refuses the start,
/// leaving the file as it is.
fn read_access(storage: &mut Storage) -> Result<AccessData, storage::Error> {
    let mut data = AccessData::default();
    for (key, sealed) in storage.access()? {
        let bytes = key.to_bytes();
        let AccessKey { kind, name } = key;
        let ip = match kind {
            AccessKind::IpBan => {
                let ip: IpAddr = name
                    .parse()
                    .map_err(|_| storage::Error::Malformed("banned ip address"))?;
                if ip.to_string() != name {
                    return Err(storage::Error::Malformed("banned ip address"));
                }
                Some(ip)
            }
            AccessKind::Op | AccessKind::Ban | AccessKind::Whitelist => {
                if permission::check_name(&name).is_err() || name.to_ascii_lowercase() != name {
                    return Err(storage::Error::Malformed("access name"));
                }
                None
            }
        };
        match kind {
            AccessKind::Op | AccessKind::Whitelist => {
                unseal::<Mark>(&bytes, &sealed)?;
                if kind == AccessKind::Op {
                    data.ops.push(name);
                } else {
                    data.whitelist.push(name);
                }
            }
            AccessKind::Ban | AccessKind::IpBan => {
                let record: BanRecord = unseal(&bytes, &sealed)?;
                if record.source.len() > permission::MAX_SOURCE {
                    return Err(storage::Error::Malformed("ban source"));
                }
                let entry = BanEntry::from_parts(
                    name,
                    ip,
                    record.reason,
                    record.source,
                    record.created,
                    record.expires,
                );
                if kind == AccessKind::Ban {
                    data.bans.push(entry);
                } else {
                    data.ip_bans.push(entry);
                }
            }
        }
    }
    let lists = [
        data.ops.len(),
        data.bans.len(),
        data.ip_bans.len(),
        data.whitelist.len(),
    ];
    if lists.iter().any(|&n| n > permission::MAX_ENTRIES) {
        return Err(storage::Error::TooManyAccessEntries);
    }
    Ok(data)
}

/// The save thread: answers requests in order until [`Request::Finish`], then marks every open
/// file closed after a normal stop. If the tick drops its sender without `Finish` (it died), the
/// requests still queued are dropped and the files are closed as they are.
/// Returns the error of a commit or a chunk read that failed after its retries, or of the final
/// close. After such a failure no later snapshot is committed: it would write a state without the
/// changes of the one that failed.
///
/// A snapshot with its backup flag starts an online backup of every open file once committed,
/// unless one is being made: that request is refused. The backups are copied
/// in steps whenever no request waits, so a request waits at most one step; `Finish` completes
/// them before the files close, and a tick that died drops them. Their result is logged, not
/// answered: the tick's state does not depend on it. A snapshot with its check flag starts a check
/// of every open file the same way (`/check`), after the backup if both are asked for; its
/// result is logged and answered, and the files' close drops a check that has not ended.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread waits for the tick's requests and for room for its answers"
)]
pub(crate) fn run(
    mut thread: Thread,
    pool: &Pool,
    mut requests: mpsc::Receiver<Request>,
    responses: &mpsc::Sender<Response>,
) -> Result<(), storage::Error> {
    let mut failure = None;
    let mut finished = false;
    // The requests left once the tick has ended.
    let mut rest: Option<VecDeque<Request>> = None;
    loop {
        let request = match &mut rest {
            Some(rest) => rest.pop_front(),
            None if is_busy(&mut thread) => match requests.try_recv() {
                Ok(request) => Some(request),
                Err(mpsc::error::TryRecvError::Empty) => {
                    background_step(&mut thread, responses);
                    continue;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => None,
            },
            None => requests.blocking_recv(),
        };
        let Some(request) = request else { break };
        if rest.is_none() && requests.is_closed() {
            // The tick has ended, so everything it sent is queued. It counts only when it ends
            // with `Finish`; otherwise the tick died and nothing more is committed.
            let mut left: VecDeque<Request> = std::iter::once(request)
                .chain(std::iter::from_fn(|| requests.try_recv().ok()))
                .collect();
            if !matches!(left.back(), Some(Request::Finish)) {
                warn!(
                    dropped = left.len(),
                    "the tick ended without stopping; requests dropped"
                );
                left.clear();
            }
            rest = Some(left);
            continue;
        }
        let answers = match request {
            Request::Chunks(keys) => match load_requested(&mut thread, pool, &keys, responses) {
                Ok(()) => continue,
                Err(e) => {
                    error!(error = %e, "chunk read failed after its retries");
                    failure = Some(e);
                    vec![Response::LoadFailed]
                }
            },
            Request::Worlds(worlds) => {
                thread.add_worlds(&worlds);
                continue;
            }
            Request::DeleteWorlds(worlds) => {
                thread.delete_worlds(&worlds);
                continue;
            }
            Request::Player(key) => vec![load_player(&mut thread.files.players.storage, key)],
            Request::Commit(snapshot) if failure.is_some() => {
                vec![Response::CommitFailed {
                    start: snapshot.start,
                }]
            }
            Request::Commit(snapshot) => {
                let start = snapshot.start;
                match commit(&mut thread, pool, &snapshot) {
                    Ok(()) => committed(&mut thread, &snapshot),
                    Err(e) => {
                        error!(error = %e, start, "commit failed after its retries");
                        failure = Some(e);
                        vec![Response::CommitFailed { start }]
                    }
                }
            }
            Request::Finish => {
                finished = true;
                break;
            }
        };
        for answer in answers {
            // A closed queue means the tick has ended; nothing waits for the answer.
            if responses.blocking_send(answer).is_err() {
                break;
            }
        }
    }
    // A failed read or commit makes no normal stop: the files get no mark of one, so the next
    // start with `integrity-check = "after-unclean-shutdown"` checks them.
    if !finished || failure.is_some() {
        return failure.map_or(Ok(()), Err);
    }
    while thread.files().any(|f| f.storage.is_backing_up()) {
        backup_step(&mut thread);
    }
    let Thread { files, plans, .. } = thread;
    let Files { main, players } = files;
    // The main file last, as in a save.
    let mut closed = Ok(());
    for file in plans
        .into_iter()
        .filter_map(|p| p.file)
        .chain([players, main])
    {
        if let Err(e) = file.storage.close() {
            error!(file = %file.place.file.display(), error = %e, "data file did not close");
            closed = Err(e);
        }
    }
    closed
}

/// Loads the chunks of a [`Request::Chunks`] for the tick, sending each turn's answers as soon as
/// the turn is done. A closed queue means the tick has ended and nothing waits for the rest.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread waits for room for its answers"
)]
fn load_requested(
    thread: &mut Thread,
    pool: &Pool,
    keys: &[(WorldIndex, ChunkPos)],
    responses: &mpsc::Sender<Response>,
) -> Result<(), storage::Error> {
    let prepare = Prepare::Tick {
        lighting: thread.lighting,
        entity_types: &thread.entity_types,
    };
    load_chunks(
        &mut thread.plans,
        pool,
        keys,
        &thread.custom,
        prepare,
        |answer| responses.blocking_send(answer).is_ok(),
    )
}

/// Whether an online backup or a check waits for its next step.
fn is_busy(thread: &mut Thread) -> bool {
    thread.check.is_some() || thread.files().any(|f| f.storage.is_backing_up())
}

/// The answers to `snapshot`, just committed, after starting the backups and the check it asked
/// for.
fn committed(thread: &mut Thread, snapshot: &Snapshot) -> Vec<Response> {
    let start = snapshot.start;
    if snapshot.backup {
        begin_backup(thread, start);
    }
    let mut answers = vec![Response::Committed { start }];
    if snapshot.check
        && let Some(found) = begin_check(thread, start)
    {
        answers.push(Response::Checked(found));
    }
    answers
}

/// Starts the online backup of every open file the snapshot taken before tick `start` asked for,
/// each into its own backup folder, and logs what came of it.
fn begin_backup(thread: &mut Thread, start: u64) {
    if thread.files().any(|f| f.storage.is_backing_up()) {
        warn!(
            start,
            "a backup is already being made; this backup is not made"
        );
        return;
    }
    for file in thread.files() {
        if let Err(e) = file.storage.begin_backup(&file.place.backups) {
            error!(file = %file.place.file.display(), error = %e, start, "backup failed; no backup file is left");
        }
    }
    info!(start, "backup started");
}

/// Takes the next step of the work done while no request waits: the online backups, then the
/// check, whose result goes to the tick once every file is checked.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread waits for room for its answer"
)]
fn background_step(thread: &mut Thread, responses: &mpsc::Sender<Response>) {
    if thread.files().any(|f| f.storage.is_backing_up()) {
        backup_step(thread);
    } else if let Some(found) = check_step(thread) {
        // A closed queue means the tick has ended; nothing waits for the answer.
        drop(responses.blocking_send(Response::Checked(found)));
    }
}

/// Starts the check of every open file the snapshot taken before tick `start` asked for; returns
/// what it found if it could not start. The tick asks for one at a time.
fn begin_check(thread: &mut Thread, start: u64) -> Option<Checked> {
    if thread.check.is_some() {
        return Some(Checked::Failed("another check is being made".to_owned()));
    }
    let mut failed = None;
    for file in thread.files() {
        if let Err(e) = file.storage.begin_check() {
            failed = Some(e);
            break;
        }
    }
    if let Some(e) = failed {
        error!(error = %e, start, "data file check could not start");
        for file in thread.files() {
            file.storage.stop_check();
        }
        return Some(Checked::Failed(e.to_string()));
    }
    info!(start, "data file check started");
    thread.check = Some(Checked::Passed { values: 0 });
    None
}

/// Reads the next step of the check, and logs and returns what it found once every file is
/// checked.
fn check_step(thread: &mut Thread) -> Option<Checked> {
    let mut so_far = thread.check.take()?;
    let mut left = false;
    for file in thread.files() {
        if !file.storage.is_checking() {
            continue;
        }
        left = true;
        if let Some(found) = file.storage.check_step() {
            so_far = so_far.and(found, &file.place.file);
        }
        break;
    }
    if left {
        thread.check = Some(so_far);
        return None;
    }
    match &so_far {
        Checked::Passed { values } => info!(values, "data file check passed"),
        Checked::Damaged {
            values,
            damaged,
            first,
        } => error!(values, damaged, first, "data file check found damage"),
        Checked::Failed(error) => error!(error, "data file check failed"),
    }
    Some(so_far)
}

/// Copies the next step of the first online backup being made, and logs its end.
fn backup_step(thread: &mut Thread) {
    let Some(file) = thread.files().find(|f| f.storage.is_backing_up()) else {
        return;
    };
    match file.storage.backup_step() {
        Ok(None) => {}
        Ok(Some(path)) => info!(path = %path.display(), "backup made"),
        Err(e) => {
            error!(file = %file.place.file.display(), error = %e, "backup failed; no backup file is left");
        }
    }
}

/// The first `chunks` key number above every stored world's `numbers`: a new persistent world's.
/// An imported world's file holds it alone, with number 0.
pub(crate) fn next_world_number(numbers: impl Iterator<Item = u32>) -> u32 {
    numbers.max().map_or(0, |n| n.saturating_add(1))
}

/// The meta value of the file's last commit (tick 0 and no commit identity for a new file).
fn stored_meta(storage: &mut Storage) -> Result<Meta, storage::Error> {
    match storage.read(&[Key::Meta])?.into_iter().next() {
        Some(Ok(Some(sealed))) => unseal::<Meta>(&Key::Meta.to_bytes(), &sealed),
        Some(Err(e)) => Err(e),
        Some(Ok(None)) | None => Ok(Meta {
            tick: 0,
            identity: None,
        }),
    }
}

/// The chunk at `pos` of world `number` in the `chunks` table.
pub(crate) fn chunk_id(number: u32, pos: ChunkPos) -> ChunkId {
    ChunkId {
        world: number,
        x: pos.x,
        z: pos.z,
    }
}

/// What a load does on the CPU pool to the chunks it decoded or generated.
#[derive(Clone, Copy)]
enum Prepare<'a> {
    /// What the tick takes: lit when `lighting`, and the extras read into the rule modules' form
    /// ([`Stored::rules`]) with the server's custom entity types.
    Tick {
        lighting: bool,
        entity_types: &'a EntityTypes,
    },
    /// Nothing: a replay's start takes the light and typed state from its checkpoint.
    Stored,
}

/// Loads a batch: reads every value of the persistent worlds' chunks, in one read transaction per
/// world file ([`Storage::read_chunks`] retries a failed read on the reopened file), then decodes
/// or generates every chunk on the CPU pool, resolves its saved custom blocks and prepares it as
/// `prepare` says. Only damage quarantines a chunk (and every chunk of a world whose file did not
/// open); a read that fails after its retries fails the batch, answering none of its chunks.
///
/// The chunks go through the pool one turn (a chunk per pool thread) at a time, and each turn's
/// answers go to `send` in key order as soon as the turn is done, so a chunk waits only for the
/// turns before its own. `send` returns false once nobody takes answers any more; the load then
/// stops.
fn load_chunks(
    plans: &mut [Plan],
    pool: &Pool,
    keys: &[(WorldIndex, ChunkPos)],
    custom: &Registry,
    prepare: Prepare<'_>,
    mut send: impl FnMut(Response) -> bool,
) -> Result<(), storage::Error> {
    let mut by_world: BTreeMap<WorldIndex, Vec<ChunkId>> = BTreeMap::new();
    for &(w, pos) in keys {
        if let Some(number) = plans.get(w).and_then(|p| p.number) {
            by_world.entry(w).or_default().push(chunk_id(number, pos));
        }
    }
    let mut read: BTreeMap<WorldIndex, std::vec::IntoIter<ChunkValues>> = BTreeMap::new();
    for (w, ids) in by_world {
        let values = match plans.get_mut(w).and_then(|p| p.file.as_mut()) {
            Some(file) => file.storage.read_chunks(&ids)?,
            None => ids.iter().map(|_| Err(storage::Error::Closed)).collect(),
        };
        read.insert(w, values.into_iter());
    }
    // Each key with the values it got, if its world is persistent.
    let items: Vec<(WorldIndex, ChunkPos, Option<ChunkValues>)> = keys
        .iter()
        .map(|&(w, pos)| (w, pos, read.get_mut(&w).and_then(Iterator::next)))
        .collect();
    let plans: &[Plan] = plans;
    for turn in items.chunks(pool.threads()) {
        let mut answers = pool.map_in_turns(turn, |(world, pos, values)| {
            let mut loaded = match (plans.get(*world), values) {
                (None, _) => Loaded::Damaged,
                (Some(plan), None) => Loaded::Generated(
                    Box::new(Chunk::generate(plan.generator.as_ref(), plan.seed, *pos)),
                    Stored::default(),
                ),
                (Some(plan), Some(values)) => decode_chunk(plan, *pos, values, custom),
            };
            if let Prepare::Tick { lighting: true, .. } = prepare
                && let Loaded::Generated(chunk, _) | Loaded::Disk(chunk, _) = &mut loaded
            {
                chunk.populate_light();
            }
            Response::Chunk {
                world: *world,
                pos: *pos,
                loaded,
            }
        });
        if let Prepare::Tick { entity_types, .. } = prepare {
            read_rules(pool, &mut answers, custom, entity_types);
        }
        for answer in answers {
            if !send(answer) {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Reads the extras of the loaded chunks among `answers` into the rule modules' form
/// ([`Stored::rules`]), with the server's custom content and entity types: one pool item per
/// list that has entries, so an item costs at most one list's read (up to [`MAX_EXTRA_ENTRIES`]
/// entries, a container's slots each), not a chunk's decode and light on top.
fn read_rules(pool: &Pool, answers: &mut [Response], custom: &Registry, types: &EntityTypes) {
    let jobs: Vec<_> = answers
        .iter()
        .enumerate()
        .filter_map(|(i, answer)| match answer {
            Response::Chunk {
                world,
                pos,
                loaded: Loaded::Disk(chunk, stored) | Loaded::Generated(chunk, stored),
            } => Some((i, (*world, *pos), &**chunk, &stored.extras)),
            _ => None,
        })
        .flat_map(|(i, key, chunk, extras)| {
            ChunkKind::EXTRAS
                .into_iter()
                .filter(|kind| !extras.get(*kind).is_empty())
                .map(move |kind| (i, key, kind, chunk, extras))
        })
        .collect();
    let parts = pool.map_in_turns(&jobs, |&(_, key, kind, chunk, extras)| {
        ChunkRules::read_kind(key, kind, extras, chunk, custom, types)
    });
    // The answer each part is of; the jobs borrow the answers, which take the parts next.
    let owners: Vec<usize> = jobs.iter().map(|job| job.0).collect();
    drop(jobs);
    let mut parts = owners.into_iter().zip(parts).peekable();
    for (i, answer) in answers.iter_mut().enumerate() {
        if let Response::Chunk {
            loaded: Loaded::Disk(_, stored) | Loaded::Generated(_, stored),
            ..
        } = answer
        {
            while let Some((_, part)) = parts.next_if(|(j, _)| *j == i) {
                stored.rules.add(part);
            }
        }
    }
}

/// The sub-chunk records of a chunk's terrain values, `None` for a sub-chunk without a value.
pub(crate) type Terrain = [Option<SubChunkRecord>; SUB_CHUNKS];

/// Decodes the values read for a chunk of a persistent world (in key order) and generates its
/// terrain if none was saved ([`read_values`]); any damaged value makes the chunk damaged.
fn decode_chunk(plan: &Plan, pos: ChunkPos, values: &ChunkValues, custom: &Registry) -> Loaded {
    let damaged = |part: Option<ChunkPart>, e: &dyn std::fmt::Display| {
        warn!(world = plan.name, x = pos.x, z = pos.z, ?part, error = %e, "chunk value damaged");
        Loaded::Damaged
    };
    let Some(number) = plan.number else {
        return Loaded::Damaged;
    };
    let values = match values {
        Ok(values) => values,
        Err(e) => return damaged(None, e),
    };
    let (sub_chunks, stored) = match read_values(chunk_id(number, pos), values) {
        Ok(read) => read,
        Err((part, e)) => return damaged(Some(part), &e),
    };
    if sub_chunks.iter().all(Option::is_none) {
        return Loaded::Generated(
            Box::new(Chunk::generate(plan.generator.as_ref(), plan.seed, pos)),
            stored,
        );
    }
    let record = ChunkRecord {
        sub_chunks: Box::new(sub_chunks.map(|s| s.unwrap_or_else(empty_sub_chunk))),
    };
    match to_chunk(record, custom) {
        Some(mut chunk) => {
            chunk.set_split();
            Loaded::Disk(Box::new(chunk), stored)
        }
        None => damaged(None, &"the terrain values are not a valid chunk"),
    }
}

/// Reads the values of `chunk` (in key order): its terrain, a sub-chunk record per value, and
/// its extras, each list its pages from 0 on without a gap, at most [`MAX_EXTRA_ENTRIES`]
/// entries. Fails with the part at fault on the first damaged value.
pub(crate) fn read_values(
    chunk: ChunkId,
    values: &[(ChunkPart, Vec<u8>)],
) -> Result<(Terrain, Stored), (ChunkPart, storage::Error)> {
    let mut stored = Stored::default();
    let mut checksum = Crc32c::new();
    let mut sub_chunks: Terrain = Default::default();
    // The page each extras list expects next.
    let mut next_pages = [0u16; 3];
    for (part, sealed) in values {
        let key = ChunkKey { chunk, part: *part }.to_bytes();
        let read = match *part {
            ChunkPart::SubChunk(index) => unseal::<SubChunkRecord>(&key, sealed).map(|r| {
                if let Some(slot) = sub_chunks.get_mut(usize::from(index)) {
                    *slot = Some(r);
                }
            }),
            ChunkPart::Extras(kind, page) => add_page(
                &mut stored.extras,
                &mut next_pages,
                kind,
                page,
                &key,
                sealed,
            ),
        };
        read.map_err(|e| (*part, e))?;
        checksum.update(key.get(CHUNK_PREFIX..).unwrap_or_default());
        checksum.update(&stored_checksum(sealed).to_le_bytes());
    }
    stored.checksum = checksum.finish();
    Ok((sub_chunks, stored))
}

/// Adds page `page` of a chunk's `kind` list, the next one `next_pages` expects, to `extras`.
fn add_page(
    extras: &mut ChunkExtras,
    next_pages: &mut [u16; 3],
    kind: ChunkKind,
    page: u16,
    key: &[u8],
    sealed: &[u8],
) -> Result<(), storage::Error> {
    let [block_entities, entities, updates] = next_pages;
    let next = match kind {
        ChunkKind::BlockEntities => block_entities,
        ChunkKind::Entities => entities,
        ChunkKind::ScheduledUpdates => updates,
    };
    if page != *next {
        return Err(storage::Error::Malformed(
            "an earlier page of the list is missing",
        ));
    }
    let record = unseal::<ExtraRecord<'_>>(key, sealed)?;
    let list = extras.get_mut(kind);
    if list.len().saturating_add(record.0.len()) > MAX_EXTRA_ENTRIES {
        return Err(storage::Error::Malformed("the list has too many entries"));
    }
    list.extend(record.0.into_owned());
    *next = page.saturating_add(1);
    Ok(())
}

/// Loads one player's data from the players' file `storage`.
fn load_player(storage: &mut Storage, key: PlayerKey) -> Response {
    let stored = Key::Player(key.clone());
    let data = match storage.read(std::slice::from_ref(&stored)) {
        Ok(mut values) => match values.pop() {
            Some(Ok(Some(sealed))) => match unseal::<PlayerRecord>(&stored.to_bytes(), &sealed) {
                Ok(record) => PlayerLoad::Found(PlayerData {
                    world: record.world,
                    position: record.position,
                    extra: record.extra,
                }),
                Err(e) => {
                    warn!(player = key.as_str(), error = %e, "player data damaged");
                    PlayerLoad::Damaged
                }
            },
            Some(Ok(None)) => PlayerLoad::Absent,
            Some(Err(e)) => {
                warn!(player = key.as_str(), error = %e, "player data read failed");
                PlayerLoad::Damaged
            }
            None => PlayerLoad::Damaged,
        },
        Err(e) => {
            warn!(player = key.as_str(), error = %e, "player data read failed");
            PlayerLoad::Damaged
        }
    };
    Response::Player { key, data }
}

/// Commits a snapshot: to each world file that changed its changed chunks and extras
/// (sealed on the CPU pool) and its record, then the changed players to their file, then the
/// changed operators, bans, whitelist and server data with the meta value to the main file, last.
/// Each file gets its write transaction, and each written file the meta value too, so a start can
/// tell a save that stopped between files ([`check_save`]). Files with nothing changed are not
/// written. Every value is written in the current format. The worlds the snapshot deletes are
/// forgotten and the worlds it brings learnt first, so their records are written too.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one function builds every file's batch of one snapshot and commits them in order"
)]
fn commit(thread: &mut Thread, pool: &Pool, snapshot: &Snapshot) -> Result<(), storage::Error> {
    thread.delete_worlds(&snapshot.deleted_worlds);
    thread.add_worlds(&snapshot.new_worlds);
    let meta = Meta {
        tick: snapshot.start,
        identity: Some(snapshot.identity),
    };
    let plans: &[Plan] = &thread.plans;
    let mut worlds: BTreeMap<WorldIndex, Batch> = BTreeMap::new();
    for &(world, values) in &snapshot.worlds {
        if let Some(plan) = plans.get(world)
            && let Some(record) = world_record(plan, values)
        {
            worlds
                .entry(world)
                .or_default()
                .put(Key::World, seal(&Key::World.to_bytes(), &record)?);
        }
    }
    let sealed = pool.map_in_turns(&snapshot.chunks, |(world, pos, chunk)| {
        let number = plans.get(*world).and_then(|p| p.number)?;
        let id = chunk_id(number, *pos);
        Some((*world, seal_terrain(id, chunk)))
    });
    for (world, values) in sealed.into_iter().flatten() {
        let batch = worlds.entry(world).or_default();
        for (key, value) in values? {
            match value {
                Some(value) => batch.put(key, value),
                None => batch.delete(key),
            }
        }
    }
    let sealed = pool.map_in_turns(&snapshot.extras, |(world, pos, kind, list)| {
        let number = plans.get(*world).and_then(|p| p.number)?;
        let id = chunk_id(number, *pos);
        Some((*world, id, *kind, seal_pages(id, *kind, list)))
    });
    for (world, id, kind, pages) in sealed.into_iter().flatten() {
        let pages = pages?;
        let count = u16::try_from(pages.len())
            .map_err(|_| storage::Error::Encode(crate::protocol::Error::Size))?;
        let batch = worlds.entry(world).or_default();
        for (key, value) in pages {
            batch.put(key, value);
        }
        batch.delete_pages(id, kind, count);
    }
    let mut players = Batch::with_capacity(snapshot.players.len() + 1);
    for (key, data) in &snapshot.players {
        let key = Key::Player(key.clone());
        let record = PlayerRecord {
            world: data.world.clone(),
            position: data.position,
            extra: data.extra.clone(),
        };
        players.put(key.clone(), seal(&key.to_bytes(), &record)?);
    }
    let mut main = Batch::with_capacity(snapshot.access.len() + snapshot.user.len() + 1);
    for (key, write) in &snapshot.access {
        let key = Key::Access(key.clone());
        match write {
            AccessWrite::Delete => main.delete(key),
            AccessWrite::Mark => main.put(key.clone(), seal(&key.to_bytes(), &Mark)?),
            AccessWrite::Ban(entry) => {
                let record = BanRecord {
                    reason: entry.reason().to_owned(),
                    source: entry.source().to_owned(),
                    created: entry.created(),
                    expires: entry.expires(),
                };
                main.put(key.clone(), seal(&key.to_bytes(), &record)?);
            }
        }
    }
    for (name, json) in &snapshot.user {
        let key = Key::User(name.clone());
        let record = UserRecord(Cow::Borrowed(json));
        main.put(key.clone(), seal(&key.to_bytes(), &record)?);
    }
    let sealed_meta = seal(&Key::Meta.to_bytes(), &meta)?;
    for (world, mut batch) in worlds {
        let Some(plan) = thread.plans.get_mut(world) else {
            continue;
        };
        // A world whose file did not open holds only quarantined chunks: nothing to save.
        let Some(file) = &mut plan.file else {
            continue;
        };
        batch.put(Key::Meta, sealed_meta.clone());
        file.storage.commit(&batch)?;
        if let Some(&(_, values)) = snapshot.worlds.iter().find(|(w, _)| *w == world) {
            plan.values = Some(values);
        }
    }
    let Files {
        main: main_file,
        players: players_file,
    } = &mut thread.files;
    if !players.is_empty() {
        players.put(Key::Meta, sealed_meta.clone());
        players_file.storage.commit(&players)?;
    }
    main.put(Key::Meta, sealed_meta);
    main_file.storage.commit(&main)?;
    thread.saved = meta;
    Ok(())
}

/// The record of a world with `values`, if `plan` is of a persistent world and its file does not
/// hold these values already.
fn world_record(plan: &Plan, values: WorldValues) -> Option<WorldRecord> {
    if plan.values == Some(values) {
        return None;
    }
    let WorldValues { time, spawn } = values;
    Some(WorldRecord {
        number: plan.number?,
        time,
        spawn,
    })
}

/// What a commit does to a key: writes these sealed bytes, or with `None` removes its value.
pub(crate) type Write = (Key, Option<Vec<u8>>);

/// The sub-chunk values a commit writes for a changed chunk's terrain, by key: its changed
/// sub-chunks, or all 24 when the file does not hold it yet. An empty sub-chunk of
/// plains has no value, `None` here: a load reads it back from no value.
/// Only a chunk whose every sub-chunk is so keeps the value of its lowest one, written whether it
/// changed or not, so the file still holds the chunk and does not generate it.
pub(crate) fn seal_terrain(id: ChunkId, chunk: &Chunk) -> Result<Vec<Write>, storage::Error> {
    let empty = |i: usize| {
        chunk.sub_chunk(i).is_none() && chunk.biome_storage(i).is_none_or(|b| b.is_uniform(PLAINS))
    };
    let all_empty = (0..SUB_CHUNKS).all(empty);
    let saved = chunk.saved_sub_chunks() | u32::from(all_empty);
    (0..SUB_CHUNKS)
        .zip(0u8..)
        .filter(|&(i, _)| saved & (1 << i) != 0)
        .map(|(i, index)| {
            let key = id.key(ChunkPart::SubChunk(index));
            if empty(i) && !(all_empty && i == 0) {
                return Ok((key, None));
            }
            let value = seal(&key.to_bytes(), &sub_chunk_record(chunk, i))?;
            Ok((key, Some(value)))
        })
        .collect()
}

/// The terrain a sub-chunk without a value has: no blocks, all plains.
pub(crate) fn empty_sub_chunk() -> SubChunkRecord {
    SubChunkRecord {
        layers: Vec::new(),
        biomes: Biomes {
            bits: 0,
            words: Vec::new(),
            palette: vec![PLAINS],
        },
    }
}

/// The sealed pages of a chunk's extras list, in order; none for an empty list.
pub(crate) fn seal_pages(
    id: ChunkId,
    kind: ChunkKind,
    list: &[Compound],
) -> Result<Vec<(Key, Vec<u8>)>, storage::Error> {
    let pages = ExtraRecord::pages(list, MAX_RECORD).map_err(storage::Error::Encode)?;
    (0..=u16::MAX)
        .zip(pages)
        .map(|(page, bytes)| {
            let key = id.key(ChunkPart::Extras(kind, page));
            let value = seal_encoded(&key.to_bytes(), &bytes)?;
            Ok((key, value))
        })
        .collect()
}

/// The stored form of a chunk: block states as name, states and version, unknown
/// states as they were read.
pub(crate) fn to_record(chunk: &Chunk) -> ChunkRecord {
    ChunkRecord {
        sub_chunks: Box::new(std::array::from_fn(|i| sub_chunk_record(chunk, i))),
    }
}

/// The stored form of sub-chunk `i` of a chunk, with its biomes.
fn sub_chunk_record(chunk: &Chunk, i: usize) -> SubChunkRecord {
    let mut layers = Vec::new();
    if let Some(blocks) = chunk.sub_chunk(i) {
        let [blocks, water] = blocks.layers();
        layers.push(block_layer(chunk, blocks));
        if !water.is_uniform(Entry::AIR) {
            layers.push(block_layer(chunk, water));
        }
    }
    SubChunkRecord {
        layers,
        biomes: biome_record(chunk, i),
    }
}

/// The stored form of the biomes of sub-chunk `i` of a chunk.
pub(crate) fn biome_record(chunk: &Chunk, i: usize) -> Biomes {
    chunk.biome_storage(i).map_or_else(
        || Biomes {
            bits: 0,
            words: Vec::new(),
            palette: vec![PLAINS],
        },
        |b| Biomes {
            bits: b.bits(),
            words: b.words().to_vec(),
            palette: b.palette().to_vec(),
        },
    )
}

fn block_layer(chunk: &Chunk, layer: &Paletted<Entry>) -> BlockLayer {
    BlockLayer {
        bits: layer.bits(),
        words: layer.words().to_vec(),
        palette: layer
            .palette()
            .iter()
            .map(|&entry| match entry {
                Entry::Known(state) => state_nbt(state),
                Entry::Unknown(i) | Entry::Inert(_, i) => usize::try_from(i)
                    .ok()
                    .and_then(|i| chunk.unknown().get(i))
                    .cloned()
                    .unwrap_or_else(|| state_nbt(crate::block::AIR)),
            })
            .collect(),
    }
}

/// A block state as Bedrock's disk palette writes it: `{name, states, version}`.
pub(crate) fn state_nbt(state: State) -> nbt::Compound {
    let states = state
        .properties()
        .map(|p| {
            let value = match p.value() {
                PropertyValue::Byte(b) => Value::Byte(b),
                PropertyValue::Int(i) => Value::Int(i),
                PropertyValue::String(s) => Value::String(s.to_owned()),
            };
            (p.name().to_owned(), value)
        })
        .collect();
    nbt::Compound::from([
        ("name".to_owned(), Value::String(state.name().to_owned())),
        ("states".to_owned(), Value::Compound(states)),
        ("version".to_owned(), Value::Int(BLOCK_STATE_VERSION)),
    ])
}

/// The block state a disk palette entry names, if Anchor knows it or PMMP reads it as one of its
/// states ([`State::read`]).
pub(crate) fn find_state(entry: &nbt::Compound) -> Option<State> {
    let Some(Value::String(name)) = entry.get("name") else {
        return None;
    };
    let states = match entry.get("states") {
        Some(Value::Compound(states)) => states,
        None => &nbt::Compound::new(),
        Some(_) => return None,
    };
    let mut properties = Vec::with_capacity(states.len());
    for (key, value) in states {
        properties.push((
            key.as_str(),
            match value {
                Value::Byte(b) => PropertyValue::Byte(*b),
                Value::Int(i) => PropertyValue::Int(*i),
                Value::String(s) => PropertyValue::String(s),
                _ => return None,
            },
        ));
    }
    State::read(name, &properties)
}

/// The chunk of a stored record, or `None` if its parts are invalid (damage: the record decoded
/// but does not describe a chunk). A state Anchor does not know is one of `custom`'s blocks if it
/// names one, else inert if clients know it or unknown, keeping its NBT either way:
/// a stored chunk needs no new format to show its inert blocks.
pub(crate) fn to_chunk(record: ChunkRecord, custom: &Registry) -> Option<Chunk> {
    let mut unknown: Vec<nbt::Compound> = Vec::new();
    let mut sub_chunks: [Option<SubChunk>; SUB_CHUNKS] = Default::default();
    let mut biomes: [Option<Paletted<u32>>; SUB_CHUNKS] = Default::default();
    let records = record.sub_chunks.into_iter();
    for ((sub, biome), stored) in sub_chunks.iter_mut().zip(biomes.iter_mut()).zip(records) {
        let mut layers = Vec::with_capacity(2);
        for layer in stored.layers {
            let palette: Vec<Entry> = layer
                .palette
                .into_iter()
                .map(|entry| {
                    if let Some(state) =
                        find_state(&entry).or_else(|| custom.find_state_nbt(&entry))
                    {
                        return Entry::Known(state);
                    }
                    let index = u32::try_from(unknown.len()).unwrap_or(u32::MAX);
                    let read = Entry::unknown(index, &entry);
                    unknown.push(entry);
                    read
                })
                .collect();
            layers.push(Paletted::from_parts(layer.bits, &layer.words, &palette).ok()?);
        }
        let mut layers = layers.into_iter();
        if let Some(first) = layers.next() {
            let second = layers
                .next()
                .unwrap_or_else(|| Paletted::uniform(Entry::AIR));
            *sub = Some(SubChunk::from_layers([first, second]));
        }
        let b = stored.biomes;
        *biome = Some(Paletted::from_parts(b.bits, &b.words, &b.palette).ok()?);
    }
    let biomes = biomes.map(|b| b.unwrap_or_else(|| Paletted::uniform(PLAINS)));
    Chunk::from_parts(sub_chunks, biomes, unknown).ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::block;
    use crate::config::Difficulty;
    use crate::protocol::{Rng, Writer};
    use crate::storage::codec::tests::{encode, random_compound};
    use crate::world::Flat;
    use crate::world::{Flat as FlatGenerator, Weather};

    /// What a load of chunk `pos` of world `number` answers when the file holds exactly the
    /// non-empty lists of `extras` and no terrain (for stand-ins of the save thread).
    pub(crate) fn stored_as_read(number: u32, pos: ChunkPos, extras: ChunkExtras) -> Stored {
        let mut checksum = Crc32c::new();
        for kind in ChunkKind::EXTRAS {
            for (key, sealed) in seal_pages(chunk_id(number, pos), kind, extras.get(kind)).unwrap()
            {
                checksum.update(key.to_bytes().get(CHUNK_PREFIX..).unwrap());
                checksum.update(&stored_checksum(&sealed).to_le_bytes());
            }
        }
        Stored {
            extras,
            checksum: checksum.finish(),
            rules: ChunkRules::default(),
        }
    }

    /// Commits `snapshot` to the world of [`plans`] in memory and loads `keys` from it as the
    /// save thread does with the server's `custom` content and `entity_types`: the answers the
    /// tick gets for them.
    pub(crate) fn commit_and_load(
        snapshot: &Snapshot,
        keys: &[(WorldIndex, ChunkPos)],
        custom: Arc<Registry>,
        entity_types: Arc<EntityTypes>,
    ) -> Vec<Response> {
        use crate::storage::tests::{Disk, closed, memory};
        let disk = Disk::default();
        let pool = crate::runtime::cpu_pool(2).unwrap();
        commit(&mut thread_on(memory(&disk)), &pool, snapshot).unwrap();
        let mut thread = thread_on(closed(&disk));
        thread.set_custom(custom, entity_types);
        let answers = thread.load(&pool, keys).unwrap();
        assert!(pool.shutdown());
        answers
    }

    /// [`load_chunks`] for the tick, lit when `lighting`, without custom entity types: every
    /// answer, in key order.
    fn load_all(
        plans: &mut [Plan],
        pool: &Pool,
        keys: &[(WorldIndex, ChunkPos)],
        lighting: bool,
        custom: &Registry,
    ) -> Result<Vec<Response>, storage::Error> {
        let entity_types = EntityTypes::default();
        let prepare = Prepare::Tick {
            lighting,
            entity_types: &entity_types,
        };
        let mut answers = Vec::new();
        load_chunks(plans, pool, keys, custom, prepare, |a| {
            answers.push(a);
            true
        })?;
        Ok(answers)
    }

    /// Random extras: each kind empty or a few random compounds.
    fn random_extras(rng: &mut Rng) -> ChunkExtras {
        let mut extras = ChunkExtras::default();
        for kind in ChunkKind::EXTRAS {
            if rng.below(4) != 0 {
                *extras.get_mut(kind) = (0..=rng.below(5))
                    .map(|_| random_compound(rng, 3))
                    .collect();
            }
        }
        extras
    }

    /// The little-endian NBT bytes of a list, for byte comparisons.
    fn nbt_bytes(list: &[Compound]) -> Vec<u8> {
        let mut w = Writer::new(Vec::new(), 0);
        for c in list {
            w.nbt(c, nbt::Encoding::LittleEndian).unwrap();
        }
        w.into_inner()
    }

    /// A per-test folder under the system temp folder, removed on drop.
    pub(crate) struct TempDir(pub(crate) PathBuf);

    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    impl TempDir {
        pub(crate) fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("anchor-save-{name}-{}", std::process::id()));
            std::fs::remove_dir_all(&dir).unwrap_or(());
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap_or(());
        }
    }

    /// The names of the files (not folders) in `dir`, in name order.
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn data_files(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.file_type().unwrap().is_file())
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        names.sort_unstable();
        names
    }

    pub(crate) fn specs() -> Vec<WorldSpec> {
        vec![
            WorldSpec {
                name: "world".to_owned(),
                kind: Kind::Persistent,
                generator: Arc::new(Flat),
                seed: Some(5),
                spawn: None,
            },
            WorldSpec {
                name: "arena".to_owned(),
                kind: Kind::Temporary,
                generator: Arc::new(Flat),
                seed: Some(9),
                spawn: None,
            },
        ]
    }

    fn stone() -> State {
        State::find("minecraft:stone", &[]).unwrap()
    }

    /// World 0 of [`specs`] after some play: spawn and time moved, time stopped, hard, thunder.
    fn stopped_hard_world() -> World {
        let mut world = World::new("world", Kind::Persistent, 5, BlockPos::new(4, -60, 4));
        world.set_time(1_234_567);
        world.set_time_stopped(true);
        world.set_difficulty(Some(Difficulty::Hard));
        world.set_weather(Weather::Thunder);
        world
    }

    /// Asserts that `world` has the settings of its spec, whatever a run changed: the time
    /// running, no difficulty of its own, clear.
    fn assert_settings_of_spec(world: &World) {
        assert!(!world.is_time_stopped());
        assert_eq!(world.difficulty(), None);
        assert_eq!(world.weather(), Weather::Clear);
    }

    /// A flat chunk with a stone block at local (1, -60, 2).
    fn changed_chunk(pos: ChunkPos) -> Chunk {
        let mut chunk = Chunk::generate(&Flat, 5, pos);
        chunk.set_block(1, -60, 2, stone()).unwrap();
        chunk
    }

    /// Runs the save thread on `dir` and answers `requests` in order, ended like a stopping tick
    /// ends them; returns the answers and the thread's result.
    fn serve(
        dir: &TempDir,
        mut requests: Vec<Request>,
    ) -> (Vec<Response>, u64, Result<(), storage::Error>) {
        requests.push(Request::Finish);
        serve_exactly(dir, requests)
    }

    /// Runs the save thread on `dir` with `requests` queued and the tick gone.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test queues requests and waits for the save thread's answers"
    )]
    fn serve_exactly(
        dir: &TempDir,
        requests: Vec<Request>,
    ) -> (Vec<Response>, u64, Result<(), storage::Error>) {
        let opened = open(&dir.0, specs()).unwrap();
        let tick = opened.tick;
        let (tx, rx) = mpsc::channel(REQUEST_CAPACITY);
        let (answer, mut answers) = mpsc::channel(RESPONSE_CAPACITY);
        for request in requests {
            tx.blocking_send(request).unwrap();
        }
        drop(tx);
        let thread = opened.thread;
        let handle = crate::runtime::spawn_save(move || {
            let pool = crate::runtime::cpu_pool(2).unwrap();
            let result = run(thread, &pool, rx, &answer);
            assert!(pool.shutdown());
            result
        })
        .unwrap();
        let result = handle.join().unwrap();
        let mut out = Vec::new();
        while let Ok(a) = answers.try_recv() {
            out.push(a);
        }
        (out, tick, result)
    }

    #[test]
    fn a_chunk_and_its_unknown_states_survive_the_record_round_trip() {
        let chunk = changed_chunk(ChunkPos::new(3, -2));
        let mut record = to_record(&chunk);
        let future = nbt::Compound::from([
            (
                "name".to_owned(),
                Value::String("minecraft:future_block".to_owned()),
            ),
            ("version".to_owned(), Value::Int(99)),
        ]);
        record.sub_chunks[0].layers[0].palette[0] = future.clone();
        let read = to_chunk(record.clone(), &Registry::default()).unwrap();
        assert_eq!(read.block(1, -60, 2), Some(stone()));
        assert_eq!(read.unknown(), [future]);
        assert_eq!(
            to_record(&read),
            record,
            "the unknown state is written back as it was read"
        );
        assert_eq!(
            read.block(0, -64, 0),
            None,
            "an unknown state reads as none"
        );
    }

    #[test]
    fn a_commit_still_queued_when_the_tick_dies_is_not_written() {
        let dir = TempDir::new("no-finish");
        let snapshot = Snapshot {
            start: 77,
            chunks: vec![(0, ChunkPos::new(2, 2), changed_chunk(ChunkPos::new(2, 2)))],
            ..Snapshot::default()
        };
        // No `Finish`: the tick panicked after queuing the snapshot.
        let (answers, _, result) = serve_exactly(&dir, vec![Request::Commit(Box::new(snapshot))]);
        result.unwrap();
        assert!(answers.is_empty(), "{answers:?}");
        assert_eq!(open(&dir.0, specs()).unwrap().tick, 0);
    }

    #[test]
    fn the_save_thread_commits_a_snapshot_and_loads_it_back_after_a_restart() {
        let dir = TempDir::new("round-trip");
        let changed = ChunkPos::new(2, 2);
        let only_extras = ChunkPos::new(9, 9);
        let mut rng = Rng(0x5a7e_0001);
        let (extras, more) = (random_extras(&mut rng), random_extras(&mut rng));
        let player = PlayerKey::offline("Steve");
        let data = PlayerData {
            world: "world".to_owned(),
            position: [1.5, -58.38, 2.5],
            extra: random_compound(&mut rng, 4),
        };
        let mut snapshot = Snapshot {
            start: 77,
            chunks: vec![(0, changed, changed_chunk(changed))],
            players: vec![(player.clone(), data.clone())],
            worlds: vec![(0, WorldValues::of(&stopped_hard_world()))],
            ..Snapshot::default()
        };
        for (pos, extras) in [(changed, &extras), (only_extras, &more)] {
            for kind in ChunkKind::EXTRAS {
                let list = extras.get(kind).to_vec();
                // Temporary worlds are never saved.
                snapshot.extras.push((1, pos, kind, list.clone()));
                snapshot.extras.push((0, pos, kind, list));
            }
        }
        let (answers, tick, result) = serve(&dir, vec![Request::Commit(Box::new(snapshot))]);
        result.unwrap();
        assert_eq!(tick, 0, "a new file");
        assert!(matches!(answers[..], [Response::Committed { start: 77 }]));

        let loads = vec![
            Request::Chunks(vec![
                (0, changed),
                (0, only_extras),
                (0, ChunkPos::new(5, 5)),
                (1, changed),
            ]),
            Request::Player(player.clone()),
            Request::Player(PlayerKey::offline("Alex")),
        ];
        let opened = open(&dir.0, specs()).unwrap();
        assert_eq!((opened.tick, opened.worlds[0].seed()), (77, 5));
        let w = &opened.worlds[0];
        // The state comes back, the spawn moved while it ran (not the spec's) with the time.
        assert_eq!(WorldValues::of(w), WorldValues::of(&stopped_hard_world()));
        // The settings changed while it ran are not saved: the spec's come back.
        assert_settings_of_spec(w);
        assert_eq!(
            opened.worlds[1].time_ticks(),
            0,
            "a temporary world starts at 0"
        );
        drop(opened);
        let (answers, tick, result) = serve(&dir, loads);
        result.unwrap();
        assert_eq!(tick, 77);
        let [
            Response::Chunk {
                loaded: Loaded::Disk(disk, disk_stored),
                ..
            },
            Response::Chunk {
                loaded: Loaded::Generated(generated, generated_stored),
                ..
            },
            Response::Chunk {
                loaded: Loaded::Generated(new, new_stored),
                ..
            },
            Response::Chunk {
                loaded: Loaded::Generated(temporary, temporary_stored),
                ..
            },
            Response::Player { data: found, .. },
            Response::Player { data: absent, .. },
        ] = &answers[..]
        else {
            panic!("unexpected answers {answers:?}");
        };
        assert_eq!(disk.block(1, -60, 2), Some(stone()));
        assert_eq!(new.block(1, -60, 2), Some(block::AIR));
        assert_eq!(generated.block(1, -60, 2), Some(block::AIR));
        assert_eq!(
            temporary.block(1, -60, 2),
            Some(block::AIR),
            "a temporary world is never saved"
        );
        // The extras come back byte for byte, with terrain from the file or the generator.
        for (stored, extras) in [(disk_stored, &extras), (generated_stored, &more)] {
            assert_eq!(stored.extras, *extras);
            for kind in ChunkKind::EXTRAS {
                assert_eq!(
                    nbt_bytes(stored.extras.get(kind)),
                    nbt_bytes(extras.get(kind))
                );
            }
            assert_ne!(stored.checksum, 0);
        }
        let as_read = stored_as_read(0, only_extras, more);
        assert_eq!(generated_stored.checksum, as_read.checksum);
        assert_eq!(*new_stored, Stored::default());
        assert_eq!(*temporary_stored, Stored::default());
        assert_eq!(*found, PlayerLoad::Found(data));
        assert_eq!(*absent, PlayerLoad::Absent);
    }

    /// Random extras and player extras through commit, restart and load, 20 rounds over the same
    /// chunks: every load gives exactly the bytes of the last commit, and empty lists delete their
    /// values.
    #[test]
    fn extras_and_player_extras_round_trip_byte_identical() {
        let dir = TempDir::new("extras");
        let mut rng = Rng(0x5a7e_0002);
        let positions = [
            ChunkPos::new(0, 0),
            ChunkPos::new(-3, 7),
            ChunkPos::new(40, -40),
        ];
        let player = PlayerKey::xuid(7);
        let mut model: Vec<ChunkExtras> = vec![ChunkExtras::default(); positions.len()];
        for round in 0..20u32 {
            let mut snapshot = Snapshot {
                start: u64::from(round),
                ..Snapshot::default()
            };
            for (pos, held) in positions.iter().zip(&mut model) {
                for kind in ChunkKind::EXTRAS {
                    if rng.below(2) == 0 {
                        continue;
                    }
                    let list: Vec<Compound> = (0..rng.below(4))
                        .map(|_| random_compound(&mut rng, 3))
                        .collect();
                    *held.get_mut(kind) = list.clone();
                    snapshot.extras.push((0, *pos, kind, list));
                }
            }
            let data = PlayerData {
                world: "world".to_owned(),
                position: [f64::from(round) / 3.0, 64.0, -0.1],
                extra: random_compound(&mut rng, 5),
            };
            snapshot.players.push((player.clone(), data.clone()));
            let loads = vec![
                Request::Commit(Box::new(snapshot)),
                Request::Chunks(positions.iter().map(|&p| (0, p)).collect()),
                Request::Player(player.clone()),
            ];
            let (answers, _, result) = serve(&dir, loads);
            result.unwrap();
            let [
                Response::Committed { .. },
                chunks @ ..,
                Response::Player { data: found, .. },
            ] = &answers[..]
            else {
                panic!("unexpected answers {answers:?}");
            };
            for (answer, held) in chunks.iter().zip(&model) {
                let Response::Chunk {
                    loaded: Loaded::Generated(_, stored),
                    ..
                } = answer
                else {
                    panic!("unexpected answer {answer:?}");
                };
                for kind in ChunkKind::EXTRAS {
                    assert_eq!(
                        nbt_bytes(stored.extras.get(kind)),
                        nbt_bytes(held.get(kind))
                    );
                }
            }
            assert_eq!(*found, PlayerLoad::Found(data));
        }
        // Empty lists left no values behind.
        let mut opened = open(&dir.0, specs()).unwrap();
        let storage = opened.thread.file("world");
        for (pos, held) in positions.iter().zip(&model) {
            let read = storage.read_chunks(&[chunk_id(0, *pos)]).unwrap();
            let parts: Vec<ChunkPart> = read[0].as_ref().unwrap().iter().map(|v| v.0).collect();
            for kind in ChunkKind::EXTRAS {
                let present = parts.contains(&ChunkPart::Extras(kind, 0));
                assert_eq!(present, !held.get(kind).is_empty(), "{pos:?} {kind:?}");
            }
        }
    }

    fn access_key(kind: AccessKind, name: &str) -> AccessKey {
        AccessKey {
            kind,
            name: name.to_owned(),
        }
    }

    #[test]
    fn a_world_record_is_written_once_its_time_or_spawn_changes() {
        let values = WorldValues {
            time: 9,
            spawn: BlockPos::new(1, 2, 3),
        };
        let plan = Plan {
            name: "world".to_owned(),
            number: Some(4),
            seed: 5,
            generator: Arc::new(Flat),
            file: None,
            values: Some(values),
        };
        assert_eq!(world_record(&plan, values), None, "the file holds them");
        // The time stands still: the spawn alone moved.
        let moved = WorldValues {
            spawn: BlockPos::new(-7, 70, 8),
            ..values
        };
        assert_eq!(
            world_record(&plan, moved),
            Some(WorldRecord {
                number: 4,
                time: 9,
                spawn: BlockPos::new(-7, 70, 8),
            })
        );
    }

    #[test]
    fn operators_bans_and_the_whitelist_survive_a_commit_and_a_restart() {
        let dir = TempDir::new("access");
        let ban = BanEntry::new("griefer")
            .with_reason("tnt")
            .with_source("console")
            .with_expires(900)
            .checked(40)
            .unwrap();
        let ip: IpAddr = "2001:db8::1".parse().unwrap();
        let ip_ban = BanEntry::ip(ip).checked(41).unwrap();
        let first = Snapshot {
            start: 50,
            access: vec![
                (access_key(AccessKind::Op, "steve"), AccessWrite::Mark),
                (access_key(AccessKind::Op, "gone"), AccessWrite::Mark),
                (access_key(AccessKind::Whitelist, "alex"), AccessWrite::Mark),
                (
                    access_key(AccessKind::Ban, "griefer"),
                    AccessWrite::Ban(ban.clone()),
                ),
                (
                    access_key(AccessKind::IpBan, &ip.to_string()),
                    AccessWrite::Ban(ip_ban.clone()),
                ),
            ],
            ..Snapshot::default()
        };
        let second = Snapshot {
            start: 60,
            access: vec![(access_key(AccessKind::Op, "gone"), AccessWrite::Delete)],
            ..Snapshot::default()
        };
        let (answers, _, result) = serve(
            &dir,
            vec![
                Request::Commit(Box::new(first)),
                Request::Commit(Box::new(second)),
            ],
        );
        result.unwrap();
        assert_eq!(answers.len(), 2);
        let opened = open(&dir.0, specs()).unwrap();
        assert_eq!(opened.tick, 60);
        assert_eq!(
            opened.access,
            AccessData {
                ops: vec!["steve".to_owned()],
                bans: vec![ban],
                ip_bans: vec![ip_ban],
                whitelist: vec!["alex".to_owned()],
            }
        );
    }

    #[test]
    fn server_data_values_survive_a_commit_and_a_restart_and_only_named_ones_are_read() {
        let dir = TempDir::new("user");
        let first = Snapshot {
            start: 10,
            user: vec![
                ("kills".to_owned(), br#"{"total":3}"#.to_vec()),
                ("old".to_owned(), b"[1,2]".to_vec()),
            ],
            ..Snapshot::default()
        };
        let second = Snapshot {
            start: 20,
            user: vec![("kills".to_owned(), br#"{"total":4}"#.to_vec())],
            ..Snapshot::default()
        };
        let (answers, _, result) = serve(
            &dir,
            vec![
                Request::Commit(Box::new(first)),
                Request::Commit(Box::new(second)),
            ],
        );
        result.unwrap();
        assert_eq!(answers.len(), 2);
        let mut opened = open(&dir.0, specs()).unwrap();
        let read = opened.thread.read_user(&["kills", "never", "old"]).unwrap();
        assert_eq!(
            read,
            [
                Some(br#"{"total":4}"#.to_vec()),
                None,
                Some(b"[1,2]".to_vec())
            ]
        );
        assert_eq!(
            opened.thread.read_user(&[]).unwrap(),
            Vec::<Option<Vec<u8>>>::new()
        );
    }

    #[test]
    fn a_damaged_server_data_value_fails_its_read_and_stays_as_it_was() {
        let dir = TempDir::new("user-damaged");
        let key = Key::User("kills".to_owned());
        {
            let mut storage = Storage::open(
                &Place::data(&dir.0, storage::MAIN_FILE),
                crate::storage::Start::checked(crate::storage::StartBackup::Make),
            )
            .unwrap();
            let mut batch = Batch::default();
            let mut sealed = seal(&key.to_bytes(), &UserRecord(b"[1]".to_vec().into())).unwrap();
            let last = sealed.len() - 1;
            sealed[last] ^= 1;
            batch.put(key.clone(), sealed);
            storage.commit(&batch).unwrap();
            storage.close().unwrap();
        }
        let mut opened = open(&dir.0, specs()).unwrap();
        let e = opened.thread.read_user(&["kills"]).unwrap_err();
        assert!(e.is_damage(), "{e:?}");
        assert!(matches!(
            crate::Error::from(e),
            crate::Error::WorldDamaged(_)
        ));
        let before = opened
            .thread
            .file(storage::MAIN_FILE)
            .read(std::slice::from_ref(&key))
            .unwrap();
        drop(opened);
        let mut opened = open(&dir.0, specs()).unwrap();
        let storage = opened.thread.file(storage::MAIN_FILE);
        assert_eq!(
            storage.read(&[key]).unwrap()[0].as_ref().unwrap(),
            before[0].as_ref().unwrap()
        );
    }

    #[test]
    fn a_damaged_access_entry_refuses_the_start_and_stays_as_it_was() {
        for (key, record) in [
            // Not lowercase: no tick writes it.
            (access_key(AccessKind::Op, "Steve"), None),
            (access_key(AccessKind::IpBan, "1.2.3"), Some("x")),
            // A ban value under an operator key.
            (access_key(AccessKind::Op, "steve"), Some("x")),
        ] {
            let dir = TempDir::new("access-damaged");
            let mut storage = Storage::open(
                &Place::data(&dir.0, storage::MAIN_FILE),
                crate::storage::Start::checked(crate::storage::StartBackup::Make),
            )
            .unwrap();
            let key = Key::Access(key);
            let sealed = match record {
                None => seal(&key.to_bytes(), &Mark).unwrap(),
                Some(reason) => {
                    let record = BanRecord {
                        reason: reason.to_owned(),
                        source: String::new(),
                        created: 0,
                        expires: None,
                    };
                    seal(&key.to_bytes(), &record).unwrap()
                }
            };
            let mut batch = Batch::default();
            batch.put(key, sealed);
            storage.commit(&batch).unwrap();
            drop(storage);
            assert!(
                matches!(open(&dir.0, specs()), Err(crate::Error::WorldDamaged(_))),
                "a damaged entry refuses the start"
            );
        }
    }

    /// Every start makes the worlds from their specs: a spawn the spec gives, and without a seed
    /// one made from the name, the same at every start and different between worlds.
    #[test]
    fn every_start_takes_the_seed_and_spawn_of_the_spec() {
        let dir = TempDir::new("spec-settings");
        let spawn = BlockPos::new(7, -40, -7);
        let specs = || {
            let mut specs = specs();
            specs[0].seed = None;
            specs[0].spawn = Some(spawn);
            specs
        };
        for _ in 0..2 {
            let opened = open(&dir.0, specs()).unwrap();
            let w = &opened.worlds[0];
            assert_eq!((w.seed(), w.spawn()), (default_seed("world"), spawn));
            assert_eq!(opened.worlds[1].spawn(), FlatGenerator.spawn(9));
            drop(opened);
        }
        assert_eq!(default_seed("world"), default_seed("world"));
        assert_ne!(default_seed("world"), default_seed("arena"));
    }

    #[test]
    fn a_damaged_chunk_value_loads_as_damaged_and_stays_as_it_was() {
        let dir = TempDir::new("damaged");
        let pos = ChunkPos::new(0, 1);
        let key = chunk_id(0, pos).key(ChunkPart::SubChunk(0));
        {
            let mut opened = open(&dir.0, specs()).unwrap();
            let mut batch = Batch::default();
            batch.put(key.clone(), b"not a sealed value".to_vec());
            opened.thread.file("world").commit(&batch).unwrap();
        }
        let (answers, _, result) = serve(&dir, vec![Request::Chunks(vec![(0, pos)])]);
        result.unwrap();
        assert!(matches!(
            answers[..],
            [Response::Chunk {
                loaded: Loaded::Damaged,
                ..
            }]
        ));
        let mut opened = open(&dir.0, specs()).unwrap();
        let read = opened.thread.file("world").read(&[key]).unwrap();
        assert_eq!(
            read[0].as_ref().unwrap().as_deref(),
            Some(&b"not a sealed value"[..])
        );
    }

    /// Damage in any one extras value quarantines the whole chunk, whose good terrain and
    /// extras stay in the file untouched, as do the damaged bytes.
    #[test]
    fn a_chunk_with_damaged_extras_loads_as_damaged_and_every_value_stays() {
        let dir = TempDir::new("damaged-extras");
        let mut rng = Rng(0x5a7e_0003);
        for kind in ChunkKind::EXTRAS {
            let pos = ChunkPos::new(i32::from(kind as u8), 4);
            let mut snapshot = Snapshot {
                start: 1,
                chunks: vec![(0, pos, changed_chunk(pos))],
                ..Snapshot::default()
            };
            for k in ChunkKind::EXTRAS {
                let list = vec![random_compound(&mut rng, 2), random_compound(&mut rng, 2)];
                snapshot.extras.push((0, pos, k, list));
            }
            let (_, _, result) = serve(&dir, vec![Request::Commit(Box::new(snapshot))]);
            result.unwrap();
            let id = chunk_id(0, pos);
            let mut opened = open(&dir.0, specs()).unwrap();
            let storage = opened.thread.file("world");
            // One flipped bit in the middle of this kind's value.
            let mut before = storage.read_chunks(&[id]).unwrap().remove(0).unwrap();
            // The one sub-chunk with blocks (the empty ones have no value) and three extras.
            assert_eq!(before.len(), 1 + 3);
            let (_, damaged) = before
                .iter_mut()
                .find(|(part, _)| *part == ChunkPart::Extras(kind, 0))
                .unwrap();
            let at = damaged.len() / 2;
            damaged[at] ^= 0x10;
            let mut batch = Batch::default();
            batch.put(id.key(ChunkPart::Extras(kind, 0)), damaged.clone());
            storage.commit(&batch).unwrap();
            drop(opened);

            let (answers, _, result) = serve(&dir, vec![Request::Chunks(vec![(0, pos)])]);
            result.unwrap();
            assert!(
                matches!(
                    answers[..],
                    [Response::Chunk {
                        loaded: Loaded::Damaged,
                        ..
                    }]
                ),
                "{kind:?}: {answers:?}"
            );
            let mut opened = open(&dir.0, specs()).unwrap();
            let after = opened
                .thread
                .file("world")
                .read_chunks(&[id])
                .unwrap()
                .remove(0)
                .unwrap();
            assert_eq!(
                after, before,
                "{kind:?}: the file keeps every value as it was"
            );
        }
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_backup_holds_the_commit_it_followed_and_one_asked_while_it_is_made_is_not_made() {
        let dir = TempDir::new("online-backup");
        let pos = ChunkPos::new(2, 2);
        let snapshot = |start, backup| Snapshot {
            start,
            chunks: vec![(0, pos, changed_chunk(pos))],
            backup,
            ..Snapshot::default()
        };
        let requests = [(10, true), (20, true), (30, false)]
            .map(|(start, backup)| Request::Commit(Box::new(snapshot(start, backup))));
        let (answers, _, result) = serve(&dir, requests.into());
        result.unwrap();
        assert!(
            matches!(
                answers[..],
                [
                    Response::Committed { start: 10 },
                    Response::Committed { start: 20 },
                    Response::Committed { start: 30 }
                ]
            ),
            "{answers:?}"
        );
        // One backup of each open file, of the commit it followed: the save a replay looks for.
        for place in [
            Place::data(&dir.0, storage::MAIN_FILE),
            Place::data(&dir.0, storage::PLAYERS_FILE),
            Place::world(&dir.0, "world"),
        ] {
            let backups: Vec<_> = std::fs::read_dir(&place.backups)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            let [backup] = &backups[..] else {
                panic!("{backups:?}");
            };
            let meta = committed_meta(backup).unwrap();
            // A file a save did not change keeps its earlier save.
            let main = place.file.ends_with("main.redb") || place.file.ends_with("world.redb");
            assert_eq!(meta.tick, if main { 10 } else { 0 }, "{place:?}");
            // A normal stop marks the file.
            assert!(storage::closed_normally(&place.file));
        }
        assert_eq!(open(&dir.0, specs()).unwrap().tick, 30);
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test sends requests and waits for the save thread's answers"
    )]
    fn a_check_reads_the_commit_it_followed_and_answers_while_the_tick_runs() {
        let dir = TempDir::new("check");
        let pos = ChunkPos::new(2, 2);
        let opened = open(&dir.0, specs()).unwrap();
        let (tx, rx) = mpsc::channel(REQUEST_CAPACITY);
        let (answer, mut answers) = mpsc::channel(RESPONSE_CAPACITY);
        let thread = opened.thread;
        let handle = crate::runtime::spawn_save(move || {
            let pool = crate::runtime::cpu_pool(2).unwrap();
            let result = run(thread, &pool, rx, &answer);
            assert!(pool.shutdown());
            result
        })
        .unwrap();
        let snapshot = Snapshot {
            start: 10,
            chunks: vec![(0, pos, changed_chunk(pos))],
            check: true,
            ..Snapshot::default()
        };
        tx.blocking_send(Request::Commit(Box::new(snapshot)))
            .unwrap();
        assert!(matches!(
            answers.blocking_recv(),
            Some(Response::Committed { start: 10 })
        ));
        let found = answers.blocking_recv();
        let Some(Response::Checked(storage::Checked::Passed { values })) = found else {
            panic!("{found:?}");
        };
        // The meta value and the chunk's one sub-chunk with blocks at least.
        assert!(values >= 2, "{values}");
        tx.blocking_send(Request::Finish).unwrap();
        drop(tx);
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn a_check_still_being_made_at_a_stop_is_dropped() {
        let dir = TempDir::new("check-stop");
        let pos = ChunkPos::new(2, 2);
        let snapshot = Snapshot {
            start: 10,
            chunks: vec![(0, pos, changed_chunk(pos))],
            check: true,
            ..Snapshot::default()
        };
        let (answers, _, result) = serve(&dir, vec![Request::Commit(Box::new(snapshot))]);
        result.unwrap();
        assert!(
            matches!(answers[..], [Response::Committed { start: 10 }]),
            "{answers:?}"
        );
        // The stop closed the files normally all the same.
        assert!(storage::closed_normally(
            &Place::data(&dir.0, storage::MAIN_FILE).file
        ));
        assert!(storage::closed_normally(
            &Place::world(&dir.0, "world").file
        ));
    }

    #[test]
    fn a_commit_after_a_failed_one_is_refused() {
        let dir = TempDir::new("refused-after-failure");
        let pos = ChunkPos::new(2, 2);
        // The first snapshot holds an extras list larger than a record may be: its seal fails.
        let huge = Compound::from([("bytes".to_owned(), Value::ByteArray(vec![0; 17 << 20]))]);
        let failing = Snapshot {
            start: 1,
            extras: vec![(0, pos, ChunkKind::Entities, vec![huge])],
            ..Snapshot::default()
        };
        let later = Snapshot {
            start: 2,
            chunks: vec![(0, pos, changed_chunk(pos))],
            ..Snapshot::default()
        };
        let requests = vec![
            Request::Commit(Box::new(failing)),
            Request::Commit(Box::new(later)),
        ];
        let (answers, _, result) = serve(&dir, requests);
        assert!(result.is_err());
        assert!(
            matches!(
                answers[..],
                [
                    Response::CommitFailed { start: 1 },
                    Response::CommitFailed { start: 2 }
                ]
            ),
            "{answers:?}"
        );
        // Nothing after the failed snapshot was written: the file is as it was, without the
        // mark of a normal stop.
        assert!(!storage::closed_normally(
            &Place::data(&dir.0, storage::MAIN_FILE).file
        ));
        let mut opened = open(&dir.0, specs()).unwrap();
        assert_eq!(opened.tick, 0);
        let read = opened
            .thread
            .file("world")
            .read_chunks(&[chunk_id(0, pos)])
            .unwrap();
        assert!(matches!(&read[..], [Ok(values)] if values.is_empty()));
    }

    /// `storage` where a file would be.
    fn open_at(storage: Storage) -> Open {
        Open {
            storage,
            place: Place {
                file: PathBuf::new(),
                backups: PathBuf::new(),
            },
        }
    }

    /// The plan of the one persistent world of the memory tests, on `storage`.
    fn plans(storage: Storage) -> Vec<Plan> {
        vec![Plan {
            name: "world".to_owned(),
            number: Some(0),
            seed: 5,
            generator: Arc::new(Flat),
            file: Some(open_at(storage)),
            values: None,
        }]
    }

    /// A save thread's side with the world of [`plans`] on `storage` and the server's files on
    /// disks of their own.
    fn thread_on(storage: Storage) -> Thread {
        use crate::storage::tests::{Disk, memory};
        let file = || open_at(memory(&Disk::default()));
        Thread {
            files: Files {
                main: file(),
                players: file(),
            },
            plans: plans(storage),
            opener: Opener::Replay(BTreeMap::new()),
            saved: Meta {
                tick: 0,
                identity: None,
            },
            lighting: true,
            custom: Arc::default(),
            entity_types: Arc::default(),
            check: None,
        }
    }

    /// A sign's saved NBT with both sides at the text limit (PMMP `BaseSign`: 1000 bytes and
    /// three newlines a side), about 2.2 KB.
    fn max_text_sign(i: i32) -> Compound {
        let side = Value::Compound(Compound::from([
            (
                "Text".to_owned(),
                Value::String(vec!["w".repeat(250); 4].join("\n")),
            ),
            ("SignTextColor".to_owned(), Value::Int(-16_777_216)),
            ("IgnoreLighting".to_owned(), Value::Byte(0)),
            ("PersistFormatting".to_owned(), Value::Byte(1)),
        ]));
        Compound::from([
            ("id".to_owned(), Value::String("Sign".to_owned())),
            ("x".to_owned(), Value::Int(i & 15)),
            ("y".to_owned(), Value::Int(-64 + (i >> 8))),
            ("z".to_owned(), Value::Int((i >> 4) & 15)),
            ("isMovable".to_owned(), Value::Byte(1)),
            ("FrontText".to_owned(), side.clone()),
            ("BackText".to_owned(), side),
            ("IsWaxed".to_owned(), Value::Byte(0)),
        ])
    }

    /// One chunk full of signs at the text limit holds more block entity NBT than one value may:
    /// it commits all the same, reads back byte for byte, and a shorter list later
    /// leaves no page of the longer one behind.
    #[test]
    fn one_chunk_full_of_max_text_signs_commits_and_reads_back() {
        let dir = TempDir::new("signs");
        let pos = ChunkPos::new(1, 1);
        let signs: Vec<Compound> = (0..7_568).map(max_text_sign).collect();
        assert!(encode(&ExtraRecord(Cow::Borrowed(&signs))).len() > MAX_RECORD);
        for (start, list) in [(1, signs.clone()), (2, signs[..10].to_vec())] {
            let snapshot = Snapshot {
                start,
                extras: vec![(0, pos, ChunkKind::BlockEntities, list.clone())],
                ..Snapshot::default()
            };
            let requests = vec![
                Request::Commit(Box::new(snapshot)),
                Request::Chunks(vec![(0, pos)]),
            ];
            let (answers, _, result) = serve(&dir, requests);
            result.unwrap();
            let [
                Response::Committed { .. },
                Response::Chunk {
                    loaded: Loaded::Generated(_, stored),
                    ..
                },
            ] = &answers[..]
            else {
                panic!("unexpected answers {answers:?}");
            };
            let read = stored.extras.get(ChunkKind::BlockEntities);
            assert_eq!(read.len(), list.len());
            assert_eq!(nbt_bytes(read), nbt_bytes(&list));
        }
    }

    /// A chunk whose every sub-chunk holds 4096 states of the longest NBT: the vanilla ones in
    /// 22 sub-chunks, custom-like ones (128-byte names, 16 properties) in the other two. The
    /// whole column is far over what one value may hold; it commits and reads back.
    #[test]
    fn a_chunk_of_the_largest_states_in_every_sub_chunk_commits() {
        use crate::storage::codec::tests::pack;
        let dir = TempDir::new("largest-states");
        let pos = ChunkPos::new(-2, 3);
        let mut vanilla: Vec<Compound> = State::all().map(state_nbt).collect();
        vanilla.sort_by_key(|c| std::cmp::Reverse(nbt_bytes(std::slice::from_ref(c)).len()));
        vanilla.truncate(4096);
        let custom = |sub: usize| -> Vec<Compound> {
            (0..4096)
                .map(|i| {
                    let states = (0..16)
                        .map(|p| {
                            let value = format!("{sub}-{i}-{p}-{}", "v".repeat(90));
                            (format!("{p:02}{}", "k".repeat(30)), Value::String(value))
                        })
                        .collect();
                    Compound::from([
                        (
                            "name".to_owned(),
                            Value::String(format!("test:{}", "n".repeat(123))),
                        ),
                        ("states".to_owned(), Value::Compound(states)),
                        ("version".to_owned(), Value::Int(BLOCK_STATE_VERSION)),
                    ])
                })
                .collect()
        };
        let indices: Vec<u16> = (0..4096).collect();
        let mut record = to_record(&Chunk::new());
        for (i, sub) in record.sub_chunks.iter_mut().enumerate() {
            sub.layers = vec![BlockLayer {
                bits: 16,
                words: pack(16, &indices),
                palette: if i < 22 { vanilla.clone() } else { custom(i) },
            }];
        }
        assert!(encode(&record).len() > 2 * MAX_RECORD);
        let chunk = to_chunk(record.clone(), &Registry::default()).unwrap();
        let snapshot = Snapshot {
            start: 1,
            chunks: vec![(0, pos, chunk)],
            ..Snapshot::default()
        };
        let requests = vec![
            Request::Commit(Box::new(snapshot)),
            Request::Chunks(vec![(0, pos)]),
        ];
        let (answers, _, result) = serve(&dir, requests);
        result.unwrap();
        let [
            Response::Committed { start: 1 },
            Response::Chunk {
                loaded: Loaded::Disk(read, _),
                ..
            },
        ] = &answers[..]
        else {
            panic!("unexpected answers {answers:?}");
        };
        assert!(to_record(read) == record, "the chunk reads back as it was");
    }

    /// A saved custom block loads as its state on the CPU pool, and the chunk arrives lit with
    /// it: the lamp's light is there as soon as the tick puts the chunk in its world.
    #[test]
    fn a_loaded_chunk_with_custom_blocks_arrives_resolved_and_lit() {
        use crate::storage::tests::{Disk, closed, memory};
        let registry = Arc::new(crate::custom::tests::registry());
        let lamp = registry
            .block_state(&crate::custom::tests::LAMP)
            .and_then(|s| s.with("color", PropertyValue::String("blue")))
            .unwrap();
        let disk = Disk::default();
        let pool = crate::runtime::cpu_pool(2).unwrap();
        let pos = ChunkPos::new(0, 1);
        let mut chunk = changed_chunk(pos);
        chunk.put(1, -60, 2, Entry::Known(lamp)).unwrap();
        let snapshot = Snapshot {
            start: 1,
            chunks: vec![(0, pos, chunk)],
            ..Snapshot::default()
        };
        commit(&mut thread_on(memory(&disk)), &pool, &snapshot).unwrap();
        let answers = load_all(
            &mut plans(closed(&disk)),
            &pool,
            &[(0, pos)],
            true,
            &registry,
        )
        .unwrap();
        let Ok::<[Response; 1], _>(
            [
                Response::Chunk {
                    loaded: Loaded::Disk(chunk, _),
                    ..
                },
            ],
        ) = answers.try_into()
        else {
            panic!("the saved chunk is loaded");
        };
        assert_eq!(chunk.block(1, -60, 2), Some(lamp));
        let mut world = World::new("world", Kind::Persistent, 5, BlockPos::new(0, 0, 0));
        world.set_custom(Arc::clone(&registry));
        world.insert_chunk(pos, *chunk);
        let at = BlockPos::new(1, -60, 18);
        assert_eq!(world.block(at), Some(lamp));
        assert_eq!(world.block_light(at), Some(15), "lit on the pool");
        assert!(pool.shutdown());
    }

    /// A chunk the file does not hold yet is written whole by its first commit (no value for the
    /// empty sub-chunks); from then on a commit writes only the sub-chunks that changed.
    #[test]
    fn a_new_chunk_is_written_whole_once_and_then_only_changes_are_written() {
        use crate::storage::tests::{Disk, closed};
        let disk = Disk::default();
        let pool = crate::runtime::cpu_pool(2).unwrap();
        let pos = ChunkPos::new(0, 1);
        let id = chunk_id(0, pos);
        let load = |world: &mut World| {
            let answers = load_all(
                &mut plans(closed(&disk)),
                &pool,
                &[(0, pos)],
                false,
                &Registry::default(),
            )
            .unwrap();
            let Ok::<[Response; 1], _>(
                [
                    Response::Chunk {
                        loaded: Loaded::Disk(chunk, _),
                        ..
                    },
                ],
            ) = answers.try_into()
            else {
                panic!("the saved chunk is loaded");
            };
            world.remove_chunk(pos);
            world.insert_chunk(pos, *chunk);
        };
        let save = |world: &mut World| {
            let snapshot = Snapshot {
                start: 1,
                chunks: world
                    .take_dirty()
                    .into_iter()
                    .map(|(p, c)| (0, p, c))
                    .collect(),
                ..Snapshot::default()
            };
            commit(&mut thread_on(closed(&disk)), &pool, &snapshot).unwrap();
            closed(&disk).read_chunks(&[id]).unwrap().remove(0).unwrap()
        };
        let mut world = World::new("world", Kind::Persistent, 5, BlockPos::new(0, 0, 0));
        world.insert_chunk(pos, changed_chunk(pos));
        world.set_block(BlockPos::new(3, 100, 19), stone()).unwrap();
        let split = save(&mut world);
        let parts: Vec<ChunkPart> = split.iter().map(|v| v.0).collect();
        let sub_chunks = [ChunkPart::SubChunk(0), ChunkPart::SubChunk(10)];
        assert_eq!(parts, sub_chunks, "the empty sub-chunks have no value");
        load(&mut world);
        assert_eq!(world.block(BlockPos::new(1, -60, 18)), Some(stone()));
        assert_eq!(world.block(BlockPos::new(3, 100, 19)), Some(stone()));
        world.set_block(BlockPos::new(3, 0, 19), stone()).unwrap();
        let after = save(&mut world);
        let changed: Vec<ChunkPart> = after
            .iter()
            .filter(|value| !split.contains(value))
            .map(|value| value.0)
            .collect();
        assert_eq!(changed, [ChunkPart::SubChunk(4)], "y 0 is in sub-chunk 4");
        assert_eq!(after.len(), 3, "the other values stay as they were");
        assert!(pool.shutdown());
    }

    /// An empty sub-chunk of plains has no value, so an imported world
    /// does not hold a value for every sky sub-chunk; a chunk that is empty everywhere keeps the
    /// value of its lowest sub-chunk, so it is loaded and not generated again.
    #[test]
    fn empty_sub_chunks_have_no_value_and_an_empty_chunk_keeps_its_lowest() {
        use crate::storage::tests::{Disk, closed};
        let disk = Disk::default();
        let pool = crate::runtime::cpu_pool(2).unwrap();
        let pos = ChunkPos::new(0, 1);
        let id = chunk_id(0, pos);
        let save = |world: &mut World| {
            let snapshot = Snapshot {
                start: 1,
                chunks: world
                    .take_dirty()
                    .into_iter()
                    .map(|(p, c)| (0, p, c))
                    .collect(),
                ..Snapshot::default()
            };
            commit(&mut thread_on(closed(&disk)), &pool, &snapshot).unwrap();
            let values = closed(&disk).read_chunks(&[id]).unwrap().remove(0).unwrap();
            values.into_iter().map(|v| v.0).collect::<Vec<_>>()
        };
        let load = || {
            let answers = load_all(
                &mut plans(closed(&disk)),
                &pool,
                &[(0, pos)],
                false,
                &Registry::default(),
            )
            .unwrap();
            match <[Response; 1]>::try_from(answers) {
                Ok(
                    [
                        Response::Chunk {
                            loaded: Loaded::Disk(chunk, _),
                            ..
                        },
                    ],
                ) => chunk,
                other => panic!("the saved chunk is loaded: {other:?}"),
            }
        };
        let mut world = World::new("world", Kind::Persistent, 5, BlockPos::new(0, 0, 0));
        world.insert_chunk(pos, Chunk::generate(&crate::world::Void, 5, pos));
        let high = BlockPos::new(3, 100, 19);
        world.set_block(high, stone()).unwrap();
        world.set_block(high, crate::block::AIR).unwrap();
        assert_eq!(save(&mut world), [ChunkPart::SubChunk(0)]);
        load();
        world.set_block(high, stone()).unwrap();
        assert_eq!(
            save(&mut world),
            [ChunkPart::SubChunk(0), ChunkPart::SubChunk(10)]
        );
        assert_eq!(load().block(3, 100, 3), Some(stone()));
        world.set_block(high, crate::block::AIR).unwrap();
        assert_eq!(save(&mut world), [ChunkPart::SubChunk(0)]);
        assert_eq!(load().block(3, 100, 3), Some(crate::block::AIR));
        assert!(pool.shutdown());
    }

    /// Values that no commit writes together are damage: an extras page without the one before
    /// it. A sub-chunk without a value is empty.
    #[test]
    fn terrain_values_that_do_not_make_one_chunk_load_as_damaged() {
        type Fill<'a> = &'a dyn Fn(&mut Batch);
        use crate::storage::tests::{Disk, closed, memory};
        let pool = crate::runtime::cpu_pool(2).unwrap();
        let pos = ChunkPos::new(0, 1);
        let id = chunk_id(0, pos);
        let record = to_record(&changed_chunk(pos));
        let sub_chunk = |batch: &mut Batch, i: u8| {
            let key = id.key(ChunkPart::SubChunk(i));
            let value = seal(&key.to_bytes(), &record.sub_chunks[usize::from(i)]).unwrap();
            batch.put(key, value);
        };
        let page = |batch: &mut Batch, n: u16| {
            let key = id.key(ChunkPart::Extras(ChunkKind::Entities, n));
            let list = ExtraRecord(vec![Compound::new()].into());
            batch.put(key.clone(), seal(&key.to_bytes(), &list).unwrap());
        };
        let cases: [(&str, Fill<'_>, bool); 3] = [
            (
                "all sub-chunks",
                &|b| (0..24).for_each(|i| sub_chunk(b, i)),
                true,
            ),
            (
                "the sub-chunk with blocks alone",
                &|b| sub_chunk(b, 0),
                true,
            ),
            (
                "page 1 without page 0",
                &|b| {
                    sub_chunk(b, 0);
                    page(b, 1);
                },
                false,
            ),
        ];
        for (name, fill, good) in cases {
            let disk = Disk::default();
            let mut batch = Batch::default();
            fill(&mut batch);
            memory(&disk).commit(&batch).unwrap();
            let answers = load_all(
                &mut plans(closed(&disk)),
                &pool,
                &[(0, pos)],
                false,
                &Registry::default(),
            )
            .unwrap();
            let loaded = matches!(
                &answers[..],
                [Response::Chunk { loaded: Loaded::Disk(chunk, _), .. }]
                    if chunk.block(1, -60, 2) == Some(stone())
            );
            let damaged = matches!(
                answers[..],
                [Response::Chunk {
                    loaded: Loaded::Damaged,
                    ..
                }]
            );
            assert_eq!((loaded, damaged), (good, !good), "{name}: {answers:?}");
        }
        assert!(pool.shutdown());
    }

    #[test]
    fn a_transient_batch_read_failure_loads_normally_and_quarantines_nothing() {
        use crate::storage::tests::{Call, Disk, Fault, closed, memory};
        let disk = Disk::default();
        let pool = crate::runtime::cpu_pool(2).unwrap();
        let pos = ChunkPos::new(0, 1);
        let snapshot = Snapshot {
            start: 1,
            chunks: vec![(0, pos, changed_chunk(pos))],
            ..Snapshot::default()
        };
        commit(&mut thread_on(memory(&disk)), &pool, &snapshot).unwrap();
        // One read fails at each point of the batch read in turn, from the reopening of the file
        // on.
        for at in 1..=12 {
            let mut plans = plans(closed(&disk));
            disk.arm(Fault::Once {
                call: Call::Read,
                at,
            });
            let answers =
                load_all(&mut plans, &pool, &[(0, pos)], false, &Registry::default()).unwrap();
            assert!(
                matches!(
                    &answers[..],
                    [Response::Chunk { loaded: Loaded::Disk(chunk, _), .. }]
                        if chunk.block(1, -60, 2) == Some(stone())
                ),
                "read {at}: {answers:?}"
            );
        }
        assert!(disk.failed() >= 4, "only {} reads failed", disk.failed());
        assert!(pool.shutdown());
    }

    /// A load answers turn by turn in key order, and stops once nobody takes its answers.
    #[test]
    fn a_load_answers_in_key_order_and_stops_when_nobody_takes_the_answers() {
        use crate::storage::tests::{Disk, closed, memory};
        let disk = Disk::default();
        let pool = crate::runtime::cpu_pool(1).unwrap();
        drop(memory(&disk));
        let keys: Vec<_> = (0..3).map(|x| (0, ChunkPos::new(x, 0))).collect();
        let types = EntityTypes::default();
        let prepare = Prepare::Tick {
            lighting: false,
            entity_types: &types,
        };
        for taken in [1, 3] {
            let mut sent = Vec::new();
            let mut plans = plans(closed(&disk));
            load_chunks(
                &mut plans,
                &pool,
                &keys,
                &Registry::default(),
                prepare,
                |a| {
                    if let Response::Chunk { world, pos, .. } = a {
                        sent.push((world, pos));
                    }
                    sent.len() < taken
                },
            )
            .unwrap();
            assert_eq!(sent, keys[..taken]);
        }
        assert!(pool.shutdown());
    }

    #[test]
    fn a_batch_read_that_keeps_failing_fails_the_batch() {
        use crate::storage::tests::{Call, Disk, Fault, closed, memory};
        let disk = Disk::default();
        let pool = crate::runtime::cpu_pool(2).unwrap();
        drop(memory(&disk));
        let mut plans = plans(closed(&disk));
        disk.arm(Fault::Every { call: Call::Read });
        let pos = ChunkPos::new(0, 1);
        assert!(load_all(&mut plans, &pool, &[(0, pos)], false, &Registry::default()).is_err());
        // The first try and its three retries.
        assert!(disk.failed() >= 4, "only {} reads failed", disk.failed());
        assert!(pool.shutdown());
    }

    /// The specs of [`specs`] and a persistent world `lobby`.
    fn specs_with_lobby() -> Vec<WorldSpec> {
        let mut specs = specs();
        specs.push(WorldSpec {
            name: "lobby".to_owned(),
            kind: Kind::Persistent,
            generator: Arc::new(Flat),
            seed: Some(3),
            spawn: None,
        });
        specs
    }

    /// Damage in any one file of a start (main, players or a declared world's) refuses the start:
    /// every file is checked before any is opened, so no file is written and none is backed up.
    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_damaged_file_of_the_start_refuses_it_and_every_file_stays_as_it_was() {
        let dir = TempDir::new("start-damaged");
        drop(open(&dir.0, specs_with_lobby()).unwrap());
        let places = [
            Place::data(&dir.0, storage::MAIN_FILE),
            Place::data(&dir.0, storage::PLAYERS_FILE),
            Place::world(&dir.0, "world"),
            Place::world(&dir.0, "lobby"),
        ];
        // A value in each file whose bytes are easy to find: random letters compress little.
        let mut rng = Rng(0x5747_0005);
        let mut values = Vec::new();
        for place in &places {
            let key = Key::User("findable".to_owned());
            let text: Vec<u8> = (0..200)
                .map(|_| b'a' + u8::try_from(rng.below(26)).unwrap())
                .collect();
            let sealed = seal(&key.to_bytes(), &UserRecord(text.into())).unwrap();
            let mut storage = Storage::open(place, Start::checked(StartBackup::Skip)).unwrap();
            let mut batch = Batch::default();
            batch.put(key, sealed.clone());
            storage.commit(&batch).unwrap();
            storage.close().unwrap();
            values.push(sealed);
        }
        let read = || places.each_ref().map(|p| std::fs::read(&p.file).unwrap());
        let whole = read();
        for (i, place) in places.iter().enumerate() {
            let mut damaged = whole[i].clone();
            let body = &values[i][12..76];
            let at = damaged.windows(body.len()).position(|w| w == body).unwrap();
            damaged[at + 32] ^= 0x20;
            std::fs::write(&place.file, &damaged).unwrap();
            let before = read();
            let refused = open(&dir.0, specs_with_lobby());
            assert!(
                matches!(refused, Err(crate::Error::WorldDamaged(_))),
                "{place:?}: {:?}",
                refused.err()
            );
            assert!(read() == before, "{place:?}: a file was written");
            std::fs::write(&place.file, &whole[i]).unwrap();
        }
        assert!(
            !std::fs::exists(dir.0.join(storage::BACKUP_DIR)).unwrap(),
            "a refused start backs nothing up"
        );
        drop(open(&dir.0, specs_with_lobby()).unwrap());
    }

    /// A normal stop commits the last snapshot to every file it changes, the main file last, and
    /// marks every file closed: nothing is doubled or lost between the files.
    #[test]
    fn a_normal_stop_saves_the_last_snapshot_to_every_file() {
        let dir = TempDir::new("stop-all");
        let pos = ChunkPos::new(1, 1);
        let player = PlayerKey::xuid(9);
        let data = PlayerData {
            world: "world".to_owned(),
            position: [0.5, -59.0, 0.5],
            extra: Compound::new(),
        };
        let snapshot = Snapshot {
            start: 33,
            identity: 7,
            chunks: vec![(0, pos, changed_chunk(pos))],
            players: vec![(player.clone(), data.clone())],
            worlds: vec![(
                0,
                WorldValues {
                    time: 600,
                    spawn: BlockPos::new(3, -60, 3),
                },
            )],
            access: vec![(access_key(AccessKind::Op, "steve"), AccessWrite::Mark)],
            user: vec![("round".to_owned(), b"3".to_vec())],
            ..Snapshot::default()
        };
        let (answers, _, result) = serve(&dir, vec![Request::Commit(Box::new(snapshot))]);
        result.unwrap();
        assert!(matches!(answers[..], [Response::Committed { start: 33 }]));
        for place in [
            Place::data(&dir.0, storage::MAIN_FILE),
            Place::data(&dir.0, storage::PLAYERS_FILE),
            Place::world(&dir.0, "world"),
        ] {
            let meta = committed_meta(&place.file).unwrap();
            assert_eq!((meta.tick, meta.identity), (33, Some(7)), "{place:?}");
            assert!(storage::closed_normally(&place.file), "{place:?}");
        }
        assert_eq!(data_files(&dir.0), ["main.redb", "players.redb"]);
        assert_eq!(data_files(&dir.0.join("worlds")), ["world.redb"]);
        let mut opened = open(&dir.0, specs()).unwrap();
        assert!(opened.later.is_empty());
        assert_eq!(opened.access.ops, ["steve"]);
        assert_eq!(opened.worlds[0].time_ticks(), 600);
        assert_eq!(opened.worlds[0].spawn(), BlockPos::new(3, -60, 3));
        assert_eq!(
            opened.thread.read_user(&["round"]).unwrap(),
            [Some(b"3".to_vec())]
        );
        drop(opened);
        let (answers, _, _) = serve(
            &dir,
            vec![Request::Chunks(vec![(0, pos)]), Request::Player(player)],
        );
        assert!(
            matches!(
                &answers[..],
                [
                    Response::Chunk { loaded: Loaded::Disk(chunk, _), .. },
                    Response::Player { data: PlayerLoad::Found(found), .. },
                ] if chunk.block(1, -60, 2) == Some(stone()) && *found == data
            ),
            "{answers:?}"
        );
    }

    /// A save that stopped between its files (a kill after the world file's commit, before the
    /// main file's) is found at the next start: the file is named with its save.
    #[test]
    fn a_save_stopped_between_files_is_found_at_the_next_start() {
        let dir = TempDir::new("save-split");
        let pos = ChunkPos::new(1, 1);
        let snapshot = |start| Snapshot {
            start,
            identity: start,
            chunks: vec![(0, pos, changed_chunk(pos))],
            players: vec![(
                PlayerKey::xuid(9),
                PlayerData {
                    world: "world".to_owned(),
                    position: [0.5, -59.0, 0.5],
                    extra: Compound::new(),
                },
            )],
            ..Snapshot::default()
        };
        let (_, _, result) = serve(&dir, vec![Request::Commit(Box::new(snapshot(10)))]);
        result.unwrap();
        assert!(open(&dir.0, specs()).unwrap().later.is_empty());
        // The next save's world file and players file commit; the kill comes before the main
        // file's: as the save thread writes them, through its own commit of save 20.
        {
            let mut thread = open(&dir.0, specs()).unwrap().thread;
            let pool = crate::runtime::cpu_pool(1).unwrap();
            let main = Place::data(&dir.0, storage::MAIN_FILE);
            // The main file's commit stays in memory: a stand-in for the kill between the files.
            let disk = crate::storage::tests::Disk::default();
            drop(std::mem::replace(
                &mut thread.files.main.storage,
                crate::storage::tests::memory(&disk),
            ));
            thread.files.main.storage = Storage::open_replica(&main.file).unwrap();
            commit(&mut thread, &pool, &snapshot(20)).unwrap();
            assert!(pool.shutdown());
        }
        let opened = open(&dir.0, specs()).unwrap();
        assert_eq!(opened.tick, 10);
        let mut later = opened.later.clone();
        later.sort();
        let mut expected = vec![
            Place::data(&dir.0, storage::PLAYERS_FILE).file,
            Place::world(&dir.0, "world").file,
        ];
        expected.sort();
        assert_eq!(later, expected);
    }

    /// The world files of `data/worlds/` are the stored worlds, declared or not; a name that is
    /// no world name is left out, and of names that differ only in letter case the first.
    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn every_world_file_is_a_world_the_tick_may_load() {
        let dir = TempDir::new("world-files");
        for name in ["arena2", "Lobby"] {
            let mut storage = Storage::open(
                &Place::world(&dir.0, name),
                Start::checked(storage::StartBackup::Skip),
            )
            .unwrap();
            let mut batch = Batch::default();
            batch.put(
                Key::World,
                seal(
                    &Key::World.to_bytes(),
                    &WorldRecord {
                        number: 4,
                        time: 9,
                        spawn: BlockPos::new(1, 2, 3),
                    },
                )
                .unwrap(),
            );
            storage.commit(&batch).unwrap();
            storage.close().unwrap();
        }
        let world = dir.0.join(storage::WORLD_DIR);
        std::fs::copy(world.join("Lobby.redb"), world.join("con.redb")).unwrap();
        std::fs::write(world.join("notes.txt"), b"").unwrap();
        let opened = open(&dir.0, specs()).unwrap();
        let names: Vec<&str> = opened
            .catalog
            .stored
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(names, ["Lobby", "arena2"]);
        assert_eq!(world_files(&dir.0).unwrap(), ["Lobby", "arena2", "world"]);
        assert!(world_file_exists(&dir.0, "LOBBY").unwrap());
        assert!(!world_file_exists(&dir.0, "lobby2").unwrap());
    }

    /// A start whose declared world would be the world file past the most `data/worlds/` may
    /// hold is refused before it makes the file, so the next start still reads the folder.
    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_start_makes_no_world_file_past_the_most_the_folder_holds() {
        let dir = TempDir::new("world-limit");
        let worlds = dir.0.join(storage::WORLD_DIR);
        std::fs::create_dir_all(&worlds).unwrap();
        // Empty files are damaged ones: left out of the stored worlds, but world files still.
        for i in 0..MAX_WORLDS {
            std::fs::write(worlds.join(format!("w{i:04}.{}", storage::EXTENSION)), b"").unwrap();
        }
        let refused = open(&dir.0, specs()).err();
        assert!(
            matches!(
                &refused,
                Some(crate::Error::Storage(e))
                    if matches!(e.downcast_ref(), Some(storage::Error::TooManyWorlds))
            ),
            "{refused:?}"
        );
        assert!(!Place::world(&dir.0, "world").file.exists());
        assert_eq!(world_files(&dir.0).unwrap().len(), MAX_WORLDS);
    }

    /// A world the tick makes while `data/worlds/` holds the most world files (the tick does not
    /// count the files a start left out) gets no file, so the next start still reads the folder.
    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_world_made_at_the_most_world_files_makes_no_file() {
        let dir = TempDir::new("world-limit-run");
        let worlds = dir.0.join(storage::WORLD_DIR);
        std::fs::create_dir_all(&worlds).unwrap();
        // Damaged files the start leaves out; with the declared world's, the folder is full.
        for i in 1..MAX_WORLDS {
            std::fs::write(worlds.join(format!("w{i:04}.{}", storage::EXTENSION)), b"").unwrap();
        }
        let (tx, rx) = mpsc::channel(REQUEST_CAPACITY);
        let (answer, _answers) = mpsc::channel(RESPONSE_CAPACITY);
        tx.try_send(Request::Worlds(vec![NewWorld {
            index: 2,
            name: "extra".to_owned(),
            number: Some(1),
            seed: 0,
            generator: Arc::new(Flat),
        }]))
        .unwrap();
        tx.try_send(Request::Finish).unwrap();
        drop(tx);
        let thread = open(&dir.0, specs()).unwrap().thread;
        let handle = crate::runtime::spawn_save(move || {
            let pool = crate::runtime::cpu_pool(1).unwrap();
            let result = run(thread, &pool, rx, &answer);
            assert!(pool.shutdown());
            result
        })
        .unwrap();
        handle.join().unwrap().unwrap();
        assert!(!Place::world(&dir.0, "extra").file.exists());
        assert_eq!(world_files(&dir.0).unwrap().len(), MAX_WORLDS);
    }

    /// A declared world whose file has its name in another letter case opens that file, at a
    /// start and in a replay: its stored number and time go on, it is not a stored world as well,
    /// and no second file or backup folder is made.
    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_declared_world_opens_its_file_in_another_letter_case() {
        let dir = TempDir::new("declared-world-case");
        let mut storage = Storage::open(
            &Place::world(&dir.0, "lobby"),
            Start::checked(storage::StartBackup::Skip),
        )
        .unwrap();
        let mut batch = Batch::default();
        batch.put(
            Key::World,
            seal(
                &Key::World.to_bytes(),
                &WorldRecord {
                    number: 4,
                    time: 9,
                    spawn: BlockPos::new(1, 2, 3),
                },
            )
            .unwrap(),
        );
        storage.commit(&batch).unwrap();
        storage.close().unwrap();
        let specs = || {
            let mut specs = specs();
            specs.push(WorldSpec {
                name: "Lobby".to_owned(),
                kind: Kind::Persistent,
                generator: Arc::new(Flat),
                seed: None,
                spawn: None,
            });
            specs
        };
        let opened = open(&dir.0, specs()).unwrap();
        assert_eq!(opened.catalog.stored, []);
        let lobby = &opened.worlds[2];
        assert_eq!((lobby.number(), lobby.time_ticks()), (Some(4), 9));
        drop(opened);
        let worlds = dir.0.join(storage::WORLD_DIR);
        assert_eq!(data_files(&worlds), ["lobby.redb", "world.redb"]);
        let backups: Vec<String> =
            std::fs::read_dir(dir.0.join(storage::BACKUP_DIR).join(storage::WORLD_DIR))
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
        assert_eq!(backups, ["lobby"]);
        let files = ReplayFiles {
            main: Place::data(&dir.0, storage::MAIN_FILE).file,
            players: Place::data(&dir.0, storage::PLAYERS_FILE).file,
            worlds: ["lobby", "world"]
                .map(|n| {
                    (
                        n.to_owned(),
                        worlds.join(format!("{n}.{}", storage::EXTENSION)),
                    )
                })
                .into(),
        };
        let replayed = open_replica(&files, specs(), &[5, 9, 0]).unwrap();
        assert_eq!(replayed.catalog.stored, []);
        let lobby = &replayed.worlds[2];
        assert_eq!((lobby.number(), lobby.time_ticks()), (Some(4), 9));
    }

    /// Deleting a world removes its file alone, after the commits before it.
    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_deleted_world_loses_its_file_alone() {
        let dir = TempDir::new("delete");
        let pos = ChunkPos::new(1, 1);
        let snapshot = Snapshot {
            start: 5,
            chunks: vec![(0, pos, changed_chunk(pos)), (2, pos, changed_chunk(pos))],
            ..Snapshot::default()
        };
        let requests = vec![
            Request::Commit(Box::new(snapshot)),
            Request::DeleteWorlds(vec![DeletedWorld {
                name: "lobby".to_owned(),
                index: Some(2),
            }]),
        ];
        let (tx, rx) = mpsc::channel(REQUEST_CAPACITY);
        let (answer, _answers) = mpsc::channel(RESPONSE_CAPACITY);
        for request in requests {
            tx.try_send(request).unwrap();
        }
        tx.try_send(Request::Finish).unwrap();
        drop(tx);
        let thread = open(&dir.0, specs_with_lobby()).unwrap().thread;
        let handle = crate::runtime::spawn_save(move || {
            let pool = crate::runtime::cpu_pool(1).unwrap();
            let result = run(thread, &pool, rx, &answer);
            assert!(pool.shutdown());
            result
        })
        .unwrap();
        handle.join().unwrap().unwrap();
        assert!(!Place::world(&dir.0, "lobby").file.exists());
        assert!(Place::world(&dir.0, "world").file.exists());
        assert_eq!(world_files(&dir.0).unwrap(), ["world"]);
    }
}
