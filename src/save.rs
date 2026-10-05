//! The save thread (PLAN.md 6, 10.6): it loads chunks and player data for the tick and commits the
//! tick's save snapshots, so the tick never touches the disk.
//!
//! The tick sends [`Request`]s and gets [`Response`]s back as inputs of a later tick. Loads read
//! the server file in one read transaction per batch; chunks it does not hold are generated.
//! Decoding, generating and encoding run on the CPU pool. A commit writes a whole snapshot in one
//! write transaction; requests are handled in order, so commits land in the order they were taken.
//!
//! A chunk is its terrain and its extras (PLAN.md 10.7): block entities, entities and scheduled
//! block updates. Every value is under [`MAX_RECORD`] however
//! much a chunk holds (PLAN.md 6): the terrain is one value per sub-chunk (none for an empty
//! sub-chunk of plains, [`seal_terrain`]) and each extras list is
//! split into pages, all under keys that share the chunk's prefix. One load reads them all in
//! one read transaction and answers them together; damage in any of them quarantines the chunk.
//!
//! The operator list, the bans and the whitelist (PLAN.md 10.8) are read once when the file
//! opens and live in the tick; a snapshot carries the entries changed since the last one. The
//! user's server data (`user` table) is read once at start ([`Thread::read_user`]) and saved the
//! same way.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::block::{PropertyValue, State};
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
    self, AccessKey, AccessKind, Batch, CHUNK_PREFIX, ChunkId, ChunkKey, ChunkKind, ChunkPart,
    ChunkValues, Key, PlayerKey, Storage,
};
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
    /// The saved data of a player.
    Player(PlayerKey),
    /// A snapshot to commit.
    Commit(Box<Snapshot>),
    /// The tick stopped normally; nothing follows. Without it the tick died, and the save thread
    /// commits nothing more (CLAUDE.md 4.6).
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
    /// file: the server stops without saving (PLAN.md 6).
    CommitFailed { start: u64 },
    /// A batch of chunks could not be read, even after reopening the file: the server stops
    /// without saving, as for a failed commit.
    LoadFailed,
    /// What the check a snapshot asked for found (`/check`).
    Checked(storage::Checked),
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

/// The extras of one chunk (PLAN.md 10.7): one raw NBT list per kind of [`ChunkKind::EXTRAS`].
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
    /// What the input log keeps of the values read (PLAN.md 10.6): CRC-32C over the key bytes
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

/// What Anchor keeps of a player between joins (PLAN.md 10.6, 10.7).
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
/// snapshot (PLAN.md 10.6).
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
/// spawn, generator, difficulty, time stop, weather) are not saved (PLAN.md 10.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorldValues {
    /// `World::time_ticks`.
    pub(crate) time: i64,
}

impl WorldValues {
    /// What a record keeps of `world` now.
    pub(crate) fn of(world: &World) -> Self {
        Self {
            time: world.time_ticks(),
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

/// A world the server runs, as its settings make it: `seed`, else [`default_seed`]; `spawn`,
/// else the generator's. A stored world goes on from its saved state ([`resume`]).
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

/// Gives `world` the state its stored `record` keeps: its number and time.
pub(crate) fn resume(world: &mut World, record: WorldRecord) {
    world.set_number(Some(record.number));
    world.set_time(record.time);
}

/// A world as the save thread sees it.
struct Plan {
    name: String,
    /// The `chunks` key number of a persistent world.
    number: Option<u32>,
    seed: i64,
    generator: Arc<dyn Generator>,
}

/// Learns the worlds the tick made or loaded: a new one is the next index (the tick numbers its
/// worlds in order and names each before any request about it), one made again replaces its plan.
fn add_worlds(plans: &mut Vec<Plan>, worlds: &[NewWorld]) {
    for world in worlds {
        let plan = Plan {
            name: world.name.clone(),
            number: world.number,
            seed: world.seed,
            generator: Arc::clone(&world.generator),
        };
        match plans.get_mut(world.index) {
            Some(slot) => *slot = plan,
            None => plans.push(plan),
        }
    }
}

/// What the tick needs to make and load worlds while it runs (PLAN.md 10.9 대응표 재검토).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Catalog {
    /// The persistent worlds the file holds that no spec names, by name.
    pub(crate) stored: Vec<(String, WorldRecord)>,
    /// The `chunks` key number of the next new persistent world.
    pub(crate) next_number: u32,
}

/// The server file and the worlds, ready to run.
pub(crate) struct Opened {
    /// The worlds for the tick, in the order of the specs.
    pub(crate) worlds: Vec<World>,
    /// What the tick makes and loads worlds with.
    pub(crate) catalog: Catalog,
    /// The engine tick of the last commit (0 for a new file).
    pub(crate) tick: u64,
    /// The commit identity of the last commit; `None` for a new file or one an older Anchor
    /// committed last.
    pub(crate) identity: Option<u64>,
    /// The operator list, the bans and the whitelist.
    pub(crate) access: AccessData,
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

/// What the save thread runs with.
pub(crate) struct Thread {
    storage: Storage,
    plans: Vec<Plan>,
    /// Whether loaded and generated chunks are lit on the CPU pool (`lighting`).
    lighting: bool,
    /// The server's custom blocks, which saved unknown states are resolved against on the CPU
    /// pool as chunks load, before they are lit (PLAN.md 10.8).
    custom: Arc<Registry>,
    /// Where online backups go (`data/backups`).
    backups: std::path::PathBuf,
}

impl Thread {
    /// Gives the thread the server's custom content (none by default).
    pub(crate) fn set_custom(&mut self, custom: Arc<Registry>) {
        self.custom = custom;
    }

    /// Sets whether chunks are lit as they load (`lighting`, on by default). With it off the
    /// tick ignores light, so lighting here would only waste CPU time.
    pub(crate) fn set_lighting(&mut self, enabled: bool) {
        self.lighting = enabled;
    }

    /// Loads chunks as the save thread does for a [`Request::Chunks`], lit as it lights them when
    /// `lit` (a replay's loads after its start; the chunks a checkpoint names take their light
    /// from it). A replay calls it on its own thread.
    pub(crate) fn load(
        &mut self,
        pool: &Pool,
        keys: &[(WorldIndex, ChunkPos)],
        lit: bool,
    ) -> Result<Vec<Response>, storage::Error> {
        load_chunks(
            &mut self.storage,
            &self.plans,
            pool,
            keys,
            lit && self.lighting,
            &self.custom,
        )
    }

    /// Commits `snapshot` as the save thread does (a replay's file keeps it in memory).
    pub(crate) fn commit(
        &mut self,
        pool: &Pool,
        snapshot: &Snapshot,
    ) -> Result<(), storage::Error> {
        commit(&mut self.storage, &mut self.plans, pool, snapshot)
    }

    /// Learns the worlds of a [`Request::Worlds`] as the save thread does (a replay's requests).
    pub(crate) fn add_worlds(&mut self, worlds: &[NewWorld]) {
        add_worlds(&mut self.plans, worlds);
    }

    /// Reads the user's server data values `names` (PLAN.md 10.8), before the threads start: the
    /// JSON of each, or `None` for a value never saved. A damaged value fails the start and stays
    /// as it is; values of names not asked for are neither read nor touched.
    pub(crate) fn read_user(
        &mut self,
        names: &[&str],
    ) -> Result<Vec<Option<Vec<u8>>>, storage::Error> {
        let keys: Vec<Key> = names.iter().map(|&n| Key::User(n.to_owned())).collect();
        let values = self.storage.read(&keys)?;
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
}

/// Opens the server file (PLAN.md 10.6: backup, integrity check, compaction) and resolves the
/// worlds: each gets the settings of its spec ([`new_world`]); a persistent world the file holds
/// keeps its time and number, a new one starts at time 0 with the next free number.
/// Keeps the start copy of the file unless `start` says [`storage::StartBackup::Transient`]
/// (`backups`), and checks the file as it says (`integrity-check`). Runs before the threads
/// start.
pub(crate) fn open_with(
    data_dir: &std::path::Path,
    specs: Vec<WorldSpec>,
    start: storage::Start,
) -> Result<Opened, crate::Error> {
    let backups = data_dir.join(storage::BACKUP_DIR);
    resolve(Storage::open(data_dir, start)?, specs, None, backups)
}

/// [`open_with`] a start backup and a check, as the tests open server files.
#[cfg(test)]
pub(crate) fn open(
    data_dir: &std::path::Path,
    specs: Vec<WorldSpec>,
) -> Result<Opened, crate::Error> {
    open_with(
        data_dir,
        specs,
        storage::Start::checked(storage::StartBackup::Make),
    )
}

/// Opens the server file `path` read-only for a replay ([`Storage::open_replica`]) and resolves the
/// worlds as [`open_with`] does, with the seeds a checkpoint kept (`seeds`, by world). Commits of
/// the thread stay in memory.
pub(crate) fn open_replica(
    path: &std::path::Path,
    specs: Vec<WorldSpec>,
    seeds: &[i64],
) -> Result<Opened, crate::Error> {
    // A replay's save thread never runs, so it makes no backup.
    let backups = std::path::PathBuf::new();
    resolve(Storage::open_replica(path)?, specs, Some(seeds), backups)
}

/// The engine tick and commit identity of the last commit of the server file `path`, opened
/// read-only (a replay looks for the file of its segment's state).
pub(crate) fn committed_meta(path: &std::path::Path) -> Result<Meta, crate::Error> {
    Ok(stored_meta(&mut Storage::open_replica(path)?)?)
}

/// The engine tick of the last commit of the server file `path`.
#[cfg(test)]
pub(crate) fn committed_tick(path: &std::path::Path) -> Result<u64, crate::Error> {
    Ok(committed_meta(path)?.tick)
}

/// Resolves the worlds of `specs` against the open `storage` (see [`open_with`]); `seeds`, by
/// world, replaces every world's seed. Online backups go to `backups`.
fn resolve(
    mut storage: Storage,
    specs: Vec<WorldSpec>,
    seeds: Option<&[i64]>,
    backups: std::path::PathBuf,
) -> Result<Opened, crate::Error> {
    let mut stored = stored_worlds(&mut storage)?;
    let Meta { tick, identity } = stored_meta(&mut storage)?;
    let access = read_access(&mut storage)?;
    let mut next_number = next_world_number(stored.iter().map(|(_, r)| r.number));
    let mut worlds = Vec::with_capacity(specs.len());
    let mut plans = Vec::with_capacity(specs.len());
    for (index, spec) in specs.into_iter().enumerate() {
        let given = seeds.and_then(|s| s.get(index).copied());
        let saved = (spec.kind == Kind::Persistent)
            .then(|| stored.iter().position(|(n, _)| *n == spec.name))
            .flatten()
            .map(|i| stored.swap_remove(i).1);
        let new = saved.is_none();
        let mut world = new_world(
            &spec.name,
            spec.kind,
            spec.generator.as_ref(),
            given.or(spec.seed),
            spec.spawn,
        );
        if let Some(record) = saved {
            resume(&mut world, record);
        } else if spec.kind == Kind::Persistent {
            world.set_number(Some(next_number));
            next_number = next_number.saturating_add(1);
        }
        info!(world = spec.name, kind = ?spec.kind, seed = world.seed(), new, "world loaded");
        world.set_generator(spec.generator.name());
        plans.push(Plan {
            name: spec.name,
            number: world.number(),
            seed: world.seed(),
            generator: spec.generator,
        });
        worlds.push(world);
    }
    Ok(Opened {
        worlds,
        catalog: Catalog {
            stored,
            next_number,
        },
        tick,
        identity,
        access,
        thread: Thread {
            storage,
            plans,
            lighting: true,
            custom: Arc::default(),
            backups,
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

/// A seed for a new world with an empty `level-seed`: chosen once, then saved.
/// The save thread: answers requests in order until [`Request::Finish`], then compacts and closes
/// the file. If the tick drops its sender without `Finish` (it died), the requests still queued
/// are dropped and the file is closed as it is (CLAUDE.md 4.6). Returns the error of a commit or
/// a chunk read that failed after its retries, or of the final close. After such a failure no later snapshot is committed: it would
/// write a state without the changes of the one that failed (CLAUDE.md 2.1).
///
/// A snapshot with its backup flag starts an online backup once committed (PLAN.md 10.9 백업),
/// unless one is being made: that request is refused. The backup is copied in steps whenever no
/// request waits, so a request waits at most one step; `Finish` completes it before the file
/// closes, and a tick that died drops it. Its result is logged, not answered: the tick's state
/// does not depend on it. A snapshot with its check flag starts a check the same way (`/check`),
/// after the backup if both are asked for; its result is logged and answered, and the file's
/// close drops a check that has not ended.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread waits for the tick's requests and for room for its answers"
)]
pub(crate) fn run(
    thread: Thread,
    pool: &Pool,
    mut requests: mpsc::Receiver<Request>,
    responses: &mpsc::Sender<Response>,
) -> Result<(), storage::Error> {
    let Thread {
        mut storage,
        mut plans,
        lighting,
        custom,
        backups,
    } = thread;
    let mut failure = None;
    let mut finished = false;
    // The requests left once the tick has ended.
    let mut rest: Option<VecDeque<Request>> = None;
    loop {
        let request = match &mut rest {
            Some(rest) => rest.pop_front(),
            None if storage.is_backing_up() || storage.is_checking() => match requests.try_recv() {
                Ok(request) => Some(request),
                Err(mpsc::error::TryRecvError::Empty) => {
                    background_step(&mut storage, responses);
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
            Request::Chunks(keys) => {
                match load_chunks(&mut storage, &plans, pool, &keys, lighting, &custom) {
                    Ok(answers) => answers,
                    Err(e) => {
                        error!(error = %e, "chunk read failed after its retries");
                        failure = Some(e);
                        vec![Response::LoadFailed]
                    }
                }
            }
            Request::Worlds(worlds) => {
                add_worlds(&mut plans, &worlds);
                continue;
            }
            Request::Player(key) => vec![load_player(&mut storage, key)],
            Request::Commit(snapshot) if failure.is_some() => {
                vec![Response::CommitFailed {
                    start: snapshot.start,
                }]
            }
            Request::Commit(snapshot) => {
                let start = snapshot.start;
                match commit(&mut storage, &mut plans, pool, &snapshot) {
                    Ok(()) => committed(&mut storage, &backups, &snapshot),
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
    // A failed read or commit makes no normal stop: the file gets no mark of one, so the next
    // start with `integrity-check = "after-unclean-shutdown"` checks it.
    let closed = if finished && failure.is_none() {
        while storage.is_backing_up() {
            backup_step(&mut storage);
        }
        storage.close()
    } else {
        drop(storage);
        Ok(())
    };
    match (failure, closed) {
        (Some(e), _) | (None, Err(e)) => Err(e),
        (None, Ok(())) => Ok(()),
    }
}

/// Starts the online backup the snapshot taken before tick `start` asked for, into `dir`, and
/// logs what came of it.
fn begin_backup(storage: &mut Storage, dir: &std::path::Path, start: u64) {
    match storage.begin_backup(dir) {
        Ok(true) => info!(start, "world backup started"),
        Ok(false) => warn!(
            start,
            "a world backup is already being made; this backup is not made"
        ),
        Err(e) => error!(error = %e, start, "world backup failed; no backup file is left"),
    }
}

/// The answers to `snapshot`, just committed, after starting the backup and the check it asked
/// for.
fn committed(
    storage: &mut Storage,
    backups: &std::path::Path,
    snapshot: &Snapshot,
) -> Vec<Response> {
    let start = snapshot.start;
    if snapshot.backup {
        begin_backup(storage, backups, start);
    }
    let mut answers = vec![Response::Committed { start }];
    if snapshot.check
        && let Some(found) = begin_check(storage, start)
    {
        answers.push(Response::Checked(found));
    }
    answers
}

/// Takes the next step of the work done while no request waits: the online backup, then the
/// check, whose result goes to the tick.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the save thread waits for room for its answer"
)]
fn background_step(storage: &mut Storage, responses: &mpsc::Sender<Response>) {
    if storage.is_backing_up() {
        backup_step(storage);
    } else if let Some(found) = check_step(storage) {
        // A closed queue means the tick has ended; nothing waits for the answer.
        drop(responses.blocking_send(Response::Checked(found)));
    }
}

/// Starts the check the snapshot taken before tick `start` asked for; returns what it found if it
/// could not start. The tick asks for one at a time.
fn begin_check(storage: &mut Storage, start: u64) -> Option<storage::Checked> {
    match storage.begin_check() {
        Ok(true) => {
            info!(start, "server file check started");
            None
        }
        Ok(false) => Some(storage::Checked::Failed(
            "another check is being made".to_owned(),
        )),
        Err(e) => {
            error!(error = %e, start, "server file check could not start");
            Some(storage::Checked::Failed(e.to_string()))
        }
    }
}

/// Reads the next step of the check, and logs and returns what it found once it ends.
fn check_step(storage: &mut Storage) -> Option<storage::Checked> {
    let found = storage.check_step()?;
    match &found {
        storage::Checked::Passed { values } => info!(values, "server file check passed"),
        storage::Checked::Damaged {
            values,
            damaged,
            first,
        } => error!(values, damaged, first, "server file check found damage"),
        storage::Checked::Failed(error) => error!(error, "server file check failed"),
    }
    Some(found)
}

/// Copies the next step of the online backup, and logs its end.
fn backup_step(storage: &mut Storage) {
    match storage.backup_step() {
        Ok(None) => {}
        Ok(Some(path)) => info!(path = %path.display(), "world backup made"),
        Err(e) => error!(error = %e, "world backup failed; no backup file is left"),
    }
}

/// The first `chunks` key number above every stored world's `numbers`: a new persistent world's
/// (the world import's too).
pub(crate) fn next_world_number(numbers: impl Iterator<Item = u32>) -> u32 {
    numbers.max().map_or(0, |n| n.saturating_add(1))
}

/// Every world the file holds, by name, with its record.
pub(crate) fn stored_worlds(
    storage: &mut Storage,
) -> Result<Vec<(String, WorldRecord)>, storage::Error> {
    let mut stored = Vec::new();
    for (name, sealed) in storage.worlds()? {
        let record: WorldRecord = unseal(&Key::World(name.clone()).to_bytes(), &sealed)?;
        stored.push((name, record));
    }
    Ok(stored)
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

/// Loads a batch: reads every value of the persistent worlds' chunks in one read transaction
/// ([`Storage::read_chunks`] retries a failed read on the reopened file), then decodes or
/// generates every chunk on the CPU pool, resolves its saved custom blocks and lights it. Only
/// damage quarantines a chunk; a read that fails after its retries fails the batch, answering
/// none of its chunks.
fn load_chunks(
    storage: &mut Storage,
    plans: &[Plan],
    pool: &Pool,
    keys: &[(WorldIndex, ChunkPos)],
    lighting: bool,
    custom: &Registry,
) -> Result<Vec<Response>, storage::Error> {
    let stored: Vec<ChunkId> = keys
        .iter()
        .filter_map(|&(w, pos)| Some(chunk_id(plans.get(w)?.number?, pos)))
        .collect();
    let mut read = storage.read_chunks(&stored)?.into_iter();
    // Each key with the values it got, if its world is persistent.
    let items: Vec<(WorldIndex, ChunkPos, Option<ChunkValues>)> = keys
        .iter()
        .map(|&(w, pos)| {
            let persistent = plans.get(w).is_some_and(|p| p.number.is_some());
            let values = persistent.then(|| read.next()).flatten();
            (w, pos, values)
        })
        .collect();
    Ok(pool.map(&items, |(world, pos, values)| {
        let mut loaded = match (plans.get(*world), values) {
            (None, _) => Loaded::Damaged,
            (Some(plan), None) => Loaded::Generated(
                Box::new(Chunk::generate(plan.generator.as_ref(), plan.seed, *pos)),
                Stored::default(),
            ),
            (Some(plan), Some(values)) => decode_chunk(plan, *pos, values, custom),
        };
        if lighting && let Loaded::Generated(chunk, _) | Loaded::Disk(chunk, _) = &mut loaded {
            chunk.populate_light();
        }
        Response::Chunk {
            world: *world,
            pos: *pos,
            loaded,
        }
    }))
}

/// Decodes the values read for a chunk of a persistent world (in key order) and generates its
/// terrain if none was saved. The terrain is either the 24 sub-chunk values or one whole value of
/// formats 1 to 3; an extras list is its pages from 0 on without a gap, at most
/// [`MAX_EXTRA_ENTRIES`] entries. Anything else, or any damaged value, makes the chunk damaged.
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
    let chunk = chunk_id(number, pos);
    let mut stored = Stored::default();
    let mut checksum = Crc32c::new();
    let mut column = None;
    let mut sub_chunks: [Option<SubChunkRecord>; SUB_CHUNKS] = Default::default();
    // The page each extras list expects next.
    let mut next_pages = [0u16; 3];
    for (part, sealed) in values {
        let key = ChunkKey { chunk, part: *part }.to_bytes();
        let read = match *part {
            ChunkPart::Column => unseal::<ChunkRecord>(&key, sealed).map(|r| column = Some(r)),
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
        if let Err(e) = read {
            return damaged(Some(*part), &e);
        }
        checksum.update(key.get(CHUNK_PREFIX..).unwrap_or_default());
        checksum.update(&stored_checksum(sealed).to_le_bytes());
    }
    stored.checksum = checksum.finish();
    let split = column.is_none();
    let found = sub_chunks.iter().flatten().count();
    let record = match (column, found) {
        (None, 0) => {
            return Loaded::Generated(
                Box::new(Chunk::generate(plan.generator.as_ref(), plan.seed, pos)),
                stored,
            );
        }
        (Some(record), 0) => Some(record),
        // A sub-chunk without a value is empty (format 9; formats 4 to 8 wrote all of them).
        (None, _) => Some(ChunkRecord {
            sub_chunks: Box::new(sub_chunks.map(|s| s.unwrap_or_else(empty_sub_chunk))),
        }),
        (Some(_), _) => None,
    };
    match record.and_then(|r| to_chunk(r, custom)) {
        Some(mut chunk) => {
            if split {
                chunk.set_split();
            }
            Loaded::Disk(Box::new(chunk), stored)
        }
        None => damaged(None, &"the terrain values are not a valid chunk"),
    }
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

/// Loads one player's data.
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

/// Commits a snapshot: the meta value, every persistent world's record, the changed chunks and
/// extras (sealed on the CPU pool) and the changed players, in one write transaction. Every value
/// is written in the current format, the meta value included, so a format 1 file is format 2
/// from its first commit on; its values not written again stay format 1 and still read. The
/// worlds the snapshot brings are learnt first, so their records are written too.
fn commit(
    storage: &mut Storage,
    plans: &mut Vec<Plan>,
    pool: &Pool,
    snapshot: &Snapshot,
) -> Result<(), storage::Error> {
    add_worlds(plans, &snapshot.new_worlds);
    let plans: &[Plan] = plans;
    let mut batch = Batch::with_capacity(
        snapshot.chunks.len()
            + snapshot.extras.len()
            + snapshot.players.len()
            + snapshot.worlds.len()
            + snapshot.access.len()
            + snapshot.user.len()
            + 1,
    );
    batch.put(
        Key::Meta,
        seal(
            &Key::Meta.to_bytes(),
            &Meta {
                tick: snapshot.start,
                identity: Some(snapshot.identity),
            },
        )?,
    );
    for &(world, values) in &snapshot.worlds {
        if let Some((key, record)) = world_record(plans.get(world), values) {
            batch.put(key.clone(), seal(&key.to_bytes(), &record)?);
        }
    }
    let sealed = pool.map(&snapshot.chunks, |(world, pos, chunk)| {
        let number = plans.get(*world).and_then(|p| p.number)?;
        let id = chunk_id(number, *pos);
        Some((id, chunk.is_split(), seal_terrain(id, chunk)))
    });
    for (id, split, values) in sealed.into_iter().flatten() {
        for (key, value) in values? {
            match value {
                Some(value) => batch.put(key, value),
                None => batch.delete(key),
            }
        }
        // The whole value of formats 1 to 3 goes in the commit that writes every sub-chunk.
        if !split {
            batch.delete(id.key(ChunkPart::Column));
        }
    }
    let sealed = pool.map(&snapshot.extras, |(world, pos, kind, list)| {
        let number = plans.get(*world).and_then(|p| p.number)?;
        let id = chunk_id(number, *pos);
        Some((id, *kind, seal_pages(id, *kind, list)))
    });
    for (id, kind, pages) in sealed.into_iter().flatten() {
        let pages = pages?;
        let count = u16::try_from(pages.len())
            .map_err(|_| storage::Error::Encode(crate::protocol::Error::Size))?;
        for (key, value) in pages {
            batch.put(key, value);
        }
        batch.delete_pages(id, kind, count);
    }
    for (key, data) in &snapshot.players {
        let key = Key::Player(key.clone());
        let record = PlayerRecord {
            world: data.world.clone(),
            position: data.position,
            extra: data.extra.clone(),
        };
        batch.put(key.clone(), seal(&key.to_bytes(), &record)?);
    }
    for (key, write) in &snapshot.access {
        let key = Key::Access(key.clone());
        match write {
            AccessWrite::Delete => batch.delete(key),
            AccessWrite::Mark => batch.put(key.clone(), seal(&key.to_bytes(), &Mark)?),
            AccessWrite::Ban(entry) => {
                let record = BanRecord {
                    reason: entry.reason().to_owned(),
                    source: entry.source().to_owned(),
                    created: entry.created(),
                    expires: entry.expires(),
                };
                batch.put(key.clone(), seal(&key.to_bytes(), &record)?);
            }
        }
    }
    for (name, json) in &snapshot.user {
        let key = Key::User(name.clone());
        let record = UserRecord(Cow::Borrowed(json));
        batch.put(key.clone(), seal(&key.to_bytes(), &record)?);
    }
    storage.commit(&batch)
}

/// The key and record of a world with `values`, if `plan` is of a persistent world.
fn world_record(plan: Option<&Plan>, values: WorldValues) -> Option<(Key, WorldRecord)> {
    let plan = plan?;
    let WorldValues { time } = values;
    let record = WorldRecord {
        number: plan.number?,
        time,
    };
    Some((Key::World(plan.name.clone()), record))
}

/// What a commit does to a key: writes these sealed bytes, or with `None` removes its value.
pub(crate) type Write = (Key, Option<Vec<u8>>);

/// The sub-chunk values a commit writes for a changed chunk's terrain, by key: its changed
/// sub-chunks, or all 24 when the file does not hold it by sub-chunk yet. An empty sub-chunk of
/// plains has no value (format 9, PLAN.md 6), `None` here: a load reads it back from no value.
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

/// The terrain a sub-chunk without a value has (format 9): no blocks, all plains.
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

/// The stored form of a chunk: block states as name, states and version (PLAN.md 6), unknown
/// states as they were read.
#[cfg(test)]
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
/// names one, else inert if clients know it or unknown, keeping its NBT either way (PLAN.md 7):
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
    use crate::storage::codec::tests::{encode, player_v1, random_compound, world_record_v7};
    use crate::storage::seal::{seal_v1, seal_v7};
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
        }
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

    /// Asserts that `world` has the settings of its spec, whatever a run changed: spawn `spawn`,
    /// the time running, no difficulty of its own, clear.
    fn assert_settings_of_spec(world: &World, spawn: BlockPos) {
        assert_eq!(world.spawn(), spawn);
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
        // No `Finish`: the tick panicked after queuing the snapshot (CLAUDE.md 4.6).
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
        assert_eq!(WorldValues::of(w), WorldValues::of(&stopped_hard_world()));
        // The settings changed while it ran are not saved: the spec's come back.
        assert_settings_of_spec(w, BlockPos::new(0, -60, 0));
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
        assert_eq!(*generated_stored, stored_as_read(0, only_extras, more));
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
        let mut storage = open(&dir.0, specs()).unwrap().thread.storage;
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
                &dir.0,
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
            .storage
            .read(std::slice::from_ref(&key))
            .unwrap();
        drop(opened);
        let mut storage = open(&dir.0, specs()).unwrap().thread.storage;
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
                &dir.0,
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
            opened.thread.storage.close().unwrap();
        }
        assert_eq!(default_seed("world"), default_seed("world"));
        assert_ne!(default_seed("world"), default_seed("arena"));
    }

    /// A file of format 7, whose world records held settings, opens with the settings of the
    /// specs and loses nothing of its state: chunks, extras, players, the world's number and time.
    /// The first commit rewrites the record without them (PLAN.md 10.6, M9 후속 U10).
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "consistency: one old file is checked from its writing to its next commit"
    )]
    fn a_format_7_file_drops_its_world_settings_and_keeps_every_chunk_and_player() {
        let dir = TempDir::new("format-7");
        let mut rng = Rng(0x0007_0010);
        let pos = ChunkPos::new(2, -3);
        let id = chunk_id(0, pos);
        let player = PlayerKey::offline("Kept");
        let data = PlayerData {
            world: "world".to_owned(),
            position: [3.5, -58.0, 4.5],
            extra: random_compound(&mut rng, 4),
        };
        let extras = random_extras(&mut rng);
        let world = Key::World("world".to_owned());
        {
            // What the Anchor before U10 wrote: format 7 values throughout, its world record with
            // seed 99, spawn (40, 7, -40), the void generator, time stopped, hard and thunder.
            let mut storage = open(&dir.0, specs()).unwrap().thread.storage;
            let mut batch = Batch::default();
            let v7 = |key: &Key, record: &[u8]| seal_v7(&key.to_bytes(), record);
            batch.put(Key::Meta, v7(&Key::Meta, &encode(&Meta::at(500))));
            let record = world_record_v7(
                WorldRecord {
                    number: 0,
                    time: 98_765,
                },
                99,
                [40, 7, -40],
                "void",
            );
            batch.put(world.clone(), v7(&world, &record));
            let chunk = changed_chunk(pos);
            for (i, index) in (0..SUB_CHUNKS).zip(0u8..) {
                let key = id.key(ChunkPart::SubChunk(index));
                batch.put(key.clone(), v7(&key, &encode(&sub_chunk_record(&chunk, i))));
            }
            for kind in ChunkKind::EXTRAS {
                let list = extras.get(kind).to_vec();
                if list.is_empty() {
                    continue;
                }
                let key = id.key(ChunkPart::Extras(kind, 0));
                let record = encode(&ExtraRecord(list.into()));
                batch.put(key.clone(), v7(&key, &record));
            }
            let key = Key::Player(player.clone());
            let record = encode(&PlayerRecord {
                world: data.world.clone(),
                position: data.position,
                extra: data.extra.clone(),
            });
            batch.put(key.clone(), v7(&key, &record));
            storage.commit(&batch).unwrap();
            storage.close().unwrap();
        }
        let opened = open(&dir.0, specs()).unwrap();
        assert_eq!(opened.tick, 500);
        let w = &opened.worlds[0];
        assert_eq!((w.seed(), w.number(), w.time_ticks()), (5, Some(0), 98_765));
        assert_eq!(w.generator(), "flat");
        assert_settings_of_spec(w, FlatGenerator.spawn(5));
        drop(opened);
        let loads = vec![
            Request::Chunks(vec![(0, pos)]),
            Request::Player(player.clone()),
            Request::Commit(Box::new(Snapshot {
                start: 501,
                worlds: vec![(0, WorldValues { time: 98_766 })],
                ..Snapshot::default()
            })),
        ];
        let (answers, tick, result) = serve(&dir, loads);
        result.unwrap();
        assert_eq!(tick, 500);
        let [
            Response::Chunk {
                loaded: Loaded::Disk(chunk, stored),
                ..
            },
            Response::Player {
                data: PlayerLoad::Found(found),
                ..
            },
            Response::Committed { start: 501 },
        ] = &answers[..]
        else {
            panic!("unexpected answers {answers:?}");
        };
        assert_eq!(to_record(chunk), to_record(&changed_chunk(pos)));
        assert_eq!(stored.extras, extras);
        assert_eq!(*found, data);
        let mut storage = open(&dir.0, specs()).unwrap().thread.storage;
        let sealed = storage
            .read(std::slice::from_ref(&world))
            .unwrap()
            .remove(0)
            .unwrap()
            .unwrap();
        assert_eq!(
            crate::storage::seal::version(&sealed).unwrap(),
            crate::storage::seal::FORMAT_VERSION
        );
        let record: WorldRecord = unseal(&world.to_bytes(), &sealed).unwrap();
        assert_eq!(
            record,
            WorldRecord {
                number: 0,
                time: 98_766
            }
        );
    }

    #[test]
    fn a_format_1_file_opens_reads_and_is_the_current_format_after_the_next_commit() {
        let dir = TempDir::new("format-1");
        let pos = ChunkPos::new(1, -1);
        let terrain = chunk_id(0, pos).key(ChunkPart::Column);
        let player = PlayerKey::offline("Old");
        {
            // What Anchor M6 wrote: format 1 meta, world, chunk and player values.
            let mut storage = open(&dir.0, specs()).unwrap().thread.storage;
            let world = WorldRecord { number: 0, time: 0 };
            let mut batch = Batch::default();
            // Format 1 meta values hold the tick alone.
            let meta = 300_u64.to_le_bytes();
            batch.put(Key::Meta, seal_v1(&Key::Meta.to_bytes(), &meta));
            let key = Key::World("world".to_owned());
            // Format 1 world records end after the generator name (no time, time stop,
            // difficulty or weather).
            let world = world_record_v7(world, 5, [0, -60, 0], "flat");
            let world = &world[..world.len() - 11];
            batch.put(key.clone(), seal_v1(&key.to_bytes(), world));
            let record = encode(&to_record(&changed_chunk(pos)));
            batch.put(terrain.clone(), seal_v1(&terrain.to_bytes(), &record));
            let key = Key::Player(player.clone());
            let record = player_v1("world", [2.5, -58.38, 3.5]);
            batch.put(key.clone(), seal_v1(&key.to_bytes(), &record));
            storage.commit(&batch).unwrap();
            storage.close().unwrap();
        }
        let loads = vec![
            Request::Chunks(vec![(0, pos)]),
            Request::Player(player.clone()),
            Request::Commit(Box::new(Snapshot {
                start: 301,
                worlds: vec![(
                    0,
                    WorldValues::of(&{
                        let mut world =
                            World::new("world", Kind::Persistent, 5, BlockPos::new(0, -60, 0));
                        world.set_time(24_301);
                        world
                    }),
                )],
                ..Snapshot::default()
            })),
        ];
        let (answers, tick, result) = serve(&dir, loads);
        result.unwrap();
        assert_eq!(tick, 300);
        let [
            Response::Chunk {
                loaded: Loaded::Disk(chunk, stored),
                ..
            },
            Response::Player {
                data: PlayerLoad::Found(data),
                ..
            },
            Response::Committed { start: 301 },
        ] = &answers[..]
        else {
            panic!("unexpected answers {answers:?}");
        };
        assert_eq!(chunk.block(1, -60, 2), Some(stone()));
        assert_eq!(stored.extras, ChunkExtras::default());
        assert_eq!(
            *data,
            PlayerData {
                world: "world".to_owned(),
                position: [2.5, f64::from(-58.38f32), 3.5],
                extra: Compound::new(),
            }
        );
        // The commit wrote the meta and world values in the current format, the world with its
        // time; the chunk not written again keeps format 1 and still loads.
        let opened = open(&dir.0, specs()).unwrap();
        assert_eq!(opened.worlds[0].time_ticks(), 24_301);
        let mut storage = opened.thread.storage;
        let world = Key::World("world".to_owned());
        let read = storage.read(&[Key::Meta, world, terrain]).unwrap();
        let versions: Vec<u16> = read
            .iter()
            .map(|v| crate::storage::seal::version(v.as_ref().unwrap().as_ref().unwrap()).unwrap())
            .collect();
        let current = crate::storage::seal::FORMAT_VERSION;
        assert_eq!(versions, [current, current, 1]);
        drop(storage);
        let (answers, tick, _) = serve(&dir, vec![Request::Chunks(vec![(0, pos)])]);
        assert_eq!(tick, 301);
        assert!(matches!(
            answers[..],
            [Response::Chunk {
                loaded: Loaded::Disk(..),
                ..
            }]
        ));
    }

    #[test]
    fn a_damaged_chunk_value_loads_as_damaged_and_stays_as_it_was() {
        let dir = TempDir::new("damaged");
        let pos = ChunkPos::new(0, 1);
        let key = chunk_id(0, pos).key(ChunkPart::Column);
        {
            let opened = open(&dir.0, specs()).unwrap();
            let mut storage = opened.thread.storage;
            let mut batch = Batch::default();
            batch.put(key.clone(), b"not a sealed value".to_vec());
            storage.commit(&batch).unwrap();
            storage.close().unwrap();
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
        let mut storage = open(&dir.0, specs()).unwrap().thread.storage;
        let read = storage.read(&[key]).unwrap();
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
            let mut storage = open(&dir.0, specs()).unwrap().thread.storage;
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
            storage.close().unwrap();

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
            let mut storage = open(&dir.0, specs()).unwrap().thread.storage;
            let after = storage.read_chunks(&[id]).unwrap().remove(0).unwrap();
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
        let backups: Vec<_> = std::fs::read_dir(dir.0.join(storage::BACKUP_DIR))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        // One backup, of the commit it followed: the tick a replay looks for.
        let [backup] = &backups[..] else {
            panic!("{backups:?}");
        };
        assert_eq!(committed_tick(backup).unwrap(), 10);
        // A normal stop marks the file.
        assert!(storage::closed_normally(&dir.0.join(storage::SERVER_FILE)));
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
        // The stop closed the file normally all the same.
        assert!(storage::closed_normally(&dir.0.join(storage::SERVER_FILE)));
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
        assert!(!storage::closed_normally(&dir.0.join(storage::SERVER_FILE)));
        let opened = open(&dir.0, specs()).unwrap();
        assert_eq!(opened.tick, 0);
        let mut storage = opened.thread.storage;
        let read = storage.read_chunks(&[chunk_id(0, pos)]).unwrap();
        assert!(matches!(&read[..], [Ok(values)] if values.is_empty()));
    }

    /// The plan of the one persistent world of the read fault tests.
    fn plans() -> Vec<Plan> {
        vec![Plan {
            name: "world".to_owned(),
            number: Some(0),
            seed: 5,
            generator: Arc::new(Flat),
        }]
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

    /// One chunk full of signs at the text limit holds more block entity NBT than one value may
    /// (PLAN.md 6): it commits all the same, reads back byte for byte, and a shorter list later
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
    /// whole column is far over what one value may hold (PLAN.md 6); it commits and reads back.
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
        let mut plans = plans();
        let pos = ChunkPos::new(0, 1);
        let mut chunk = changed_chunk(pos);
        chunk.put(1, -60, 2, Entry::Known(lamp)).unwrap();
        let snapshot = Snapshot {
            start: 1,
            chunks: vec![(0, pos, chunk)],
            ..Snapshot::default()
        };
        commit(&mut memory(&disk), &mut plans, &pool, &snapshot).unwrap();
        let answers = load_chunks(
            &mut closed(&disk),
            &plans,
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

    /// A whole terrain value of formats 1 to 3 is rewritten as sub-chunk values (none for the
    /// empty ones) and removed in the commit after its chunk changes; from then on a commit
    /// writes only the sub-chunks that changed.
    #[test]
    fn a_whole_terrain_value_is_split_by_the_next_commit_and_then_only_changes_are_written() {
        use crate::storage::tests::{Disk, closed, memory};
        let disk = Disk::default();
        let pool = crate::runtime::cpu_pool(2).unwrap();
        let plans = plans();
        let pos = ChunkPos::new(0, 1);
        let id = chunk_id(0, pos);
        let mut storage = memory(&disk);
        let mut batch = Batch::default();
        let column = id.key(ChunkPart::Column);
        let record = encode(&to_record(&changed_chunk(pos)));
        batch.put(column.clone(), seal_v1(&column.to_bytes(), &record));
        storage.commit(&batch).unwrap();
        drop(storage);
        let load = |world: &mut World| {
            let answers = load_chunks(
                &mut closed(&disk),
                &plans,
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
            commit(
                &mut closed(&disk),
                &mut super::tests::plans(),
                &pool,
                &snapshot,
            )
            .unwrap();
            closed(&disk).read_chunks(&[id]).unwrap().remove(0).unwrap()
        };
        let mut world = World::new("world", Kind::Persistent, 5, BlockPos::new(0, 0, 0));
        load(&mut world);
        world.set_block(BlockPos::new(3, 100, 19), stone()).unwrap();
        let split = save(&mut world);
        let parts: Vec<ChunkPart> = split.iter().map(|v| v.0).collect();
        let sub_chunks = [ChunkPart::SubChunk(0), ChunkPart::SubChunk(10)];
        assert_eq!(
            parts, sub_chunks,
            "the whole value and the empty sub-chunks are gone"
        );
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

    /// Format 9 (M9 후속 U14): an empty sub-chunk of plains has no value, so an imported world
    /// does not hold a value for every sky sub-chunk; a chunk that is empty everywhere keeps the
    /// value of its lowest sub-chunk, so it is loaded and not generated again.
    #[test]
    fn empty_sub_chunks_have_no_value_and_an_empty_chunk_keeps_its_lowest() {
        use crate::storage::tests::{Disk, closed};
        let disk = Disk::default();
        let pool = crate::runtime::cpu_pool(2).unwrap();
        let plans = plans();
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
            commit(
                &mut closed(&disk),
                &mut super::tests::plans(),
                &pool,
                &snapshot,
            )
            .unwrap();
            let values = closed(&disk).read_chunks(&[id]).unwrap().remove(0).unwrap();
            values.into_iter().map(|v| v.0).collect::<Vec<_>>()
        };
        let load = || {
            let answers = load_chunks(
                &mut closed(&disk),
                &plans,
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
        assert!(load().is_split());
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

    /// Values that no commit writes together are damage: the sub-chunks beside a whole terrain
    /// value, or an extras page without the one before it. A sub-chunk without a value is empty
    /// (format 9).
    #[test]
    fn terrain_values_that_do_not_make_one_chunk_load_as_damaged() {
        type Fill<'a> = &'a dyn Fn(&mut Batch);
        use crate::storage::tests::{Disk, closed, memory};
        let pool = crate::runtime::cpu_pool(2).unwrap();
        let plans = plans();
        let pos = ChunkPos::new(0, 1);
        let id = chunk_id(0, pos);
        let record = to_record(&changed_chunk(pos));
        let sub_chunk = |batch: &mut Batch, i: u8| {
            let key = id.key(ChunkPart::SubChunk(i));
            let value = seal(&key.to_bytes(), &record.sub_chunks[usize::from(i)]).unwrap();
            batch.put(key, value);
        };
        let column = |batch: &mut Batch| {
            let key = id.key(ChunkPart::Column);
            batch.put(key.clone(), seal(&key.to_bytes(), &record).unwrap());
        };
        let page = |batch: &mut Batch, n: u16| {
            let key = id.key(ChunkPart::Extras(ChunkKind::Entities, n));
            let list = ExtraRecord(vec![Compound::new()].into());
            batch.put(key.clone(), seal(&key.to_bytes(), &list).unwrap());
        };
        let cases: [(&str, Fill<'_>, bool); 5] = [
            (
                "all sub-chunks",
                &|b| (0..24).for_each(|i| sub_chunk(b, i)),
                true,
            ),
            ("the whole value", &|b| column(b), true),
            (
                "the sub-chunk with blocks alone",
                &|b| sub_chunk(b, 0),
                true,
            ),
            (
                "sub-chunks and the whole value",
                &|b| {
                    (0..24).for_each(|i| sub_chunk(b, i));
                    column(b);
                },
                false,
            ),
            (
                "page 1 without page 0",
                &|b| {
                    column(b);
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
            let answers = load_chunks(
                &mut closed(&disk),
                &plans,
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
        let mut plans = plans();
        let pos = ChunkPos::new(0, 1);
        let snapshot = Snapshot {
            start: 1,
            chunks: vec![(0, pos, changed_chunk(pos))],
            ..Snapshot::default()
        };
        commit(&mut memory(&disk), &mut plans, &pool, &snapshot).unwrap();
        // One read fails at each point of the batch read in turn, from the reopening of the file
        // on.
        for at in 1..=12 {
            let mut storage = closed(&disk);
            disk.arm(Fault::Once {
                call: Call::Read,
                at,
            });
            let answers = load_chunks(
                &mut storage,
                &plans,
                &pool,
                &[(0, pos)],
                false,
                &Registry::default(),
            )
            .unwrap();
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

    #[test]
    fn a_batch_read_that_keeps_failing_fails_the_batch() {
        use crate::storage::tests::{Call, Disk, Fault, closed, memory};
        let disk = Disk::default();
        let pool = crate::runtime::cpu_pool(2).unwrap();
        drop(memory(&disk));
        let mut storage = closed(&disk);
        disk.arm(Fault::Every { call: Call::Read });
        let pos = ChunkPos::new(0, 1);
        assert!(
            load_chunks(
                &mut storage,
                &plans(),
                &pool,
                &[(0, pos)],
                false,
                &Registry::default()
            )
            .is_err()
        );
        // The first try and its three retries.
        assert!(disk.failed() >= 4, "only {} reads failed", disk.failed());
        assert!(pool.shutdown());
    }
}
