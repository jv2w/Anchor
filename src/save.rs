//! The save thread (PLAN.md 6, 10.6): it loads chunks and player data for the tick and commits the
//! tick's save snapshots, so the tick never touches the disk.
//!
//! The tick sends [`Request`]s and gets [`Response`]s back as inputs of a later tick. Loads read
//! the world file in one read transaction per batch; chunks it does not hold are generated.
//! Decoding, generating and encoding run on the CPU pool. A commit writes a whole snapshot in one
//! write transaction; requests are handled in order, so commits land in the order they were taken.

use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::block::{PropertyValue, State};
use crate::nbt::{self, Value};
use crate::pool::Pool;
use crate::storage::codec::WorldRecord;
use crate::storage::codec::{Biomes, BlockLayer, ChunkRecord, Meta, PlayerRecord, SubChunkRecord};
use crate::storage::seal::{seal, unseal};
use crate::storage::{self, Batch, ChunkKey, ChunkKind, Key, PlayerKey, Storage};
use crate::world::{
    BlockPos, Chunk, ChunkPos, Entry, Generator, Kind, PLAINS, Paletted, SUB_CHUNKS, SubChunk,
    World,
};

/// A world of the server, by its place in the tick's world list (the default world is 0).
pub(crate) type WorldIndex = usize;

/// The `version` of every block state written: the version the reference list gives every state
/// of the protocol version (PLAN.md 7). A version upgrade (PLAN.md 11) reads older ones through
/// the upgrade rules.
const BLOCK_STATE_VERSION: i32 = 18_168_865;
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
    /// The saved data of a player.
    Player(PlayerKey),
    /// A snapshot to commit.
    Commit(Box<Snapshot>),
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
}

/// Where a loaded chunk came from.
#[derive(Debug)]
pub(crate) enum Loaded {
    /// Read from the world file.
    Disk(Box<Chunk>),
    /// Never saved, so generated.
    Generated(Box<Chunk>),
    /// Saved, but damaged or unreadable: the position is quarantined and the saved bytes are
    /// kept.
    Damaged,
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

/// What Anchor keeps of a player between joins (PLAN.md 10.6; M7 adds more).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlayerData {
    /// The name of the world the player is in.
    pub(crate) world: String,
    /// The eye position.
    pub(crate) position: [f32; 3],
}

/// The state of a tick boundary that a commit writes: everything changed since the last
/// snapshot (PLAN.md 10.6).
#[derive(Debug, Default)]
pub(crate) struct Snapshot {
    /// The tick the snapshot was taken before; the engine starts there after a restart.
    pub(crate) start: u64,
    /// Changed chunks of persistent worlds. They share their sub-chunks with the live world.
    pub(crate) chunks: Vec<(WorldIndex, ChunkPos, Chunk)>,
    /// Changed player data.
    pub(crate) players: Vec<(PlayerKey, PlayerData)>,
    /// The spawn of every persistent world.
    pub(crate) spawns: Vec<(WorldIndex, BlockPos)>,
}

/// A world the server runs, as configured.
pub(crate) struct WorldSpec {
    pub(crate) name: String,
    pub(crate) kind: Kind,
    pub(crate) generator: Arc<dyn Generator>,
    /// The seed of a new world; `None` draws one.
    pub(crate) seed: Option<i64>,
}

impl std::fmt::Debug for WorldSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorldSpec")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("generator", &self.generator.name())
            .field("seed", &self.seed)
            .finish()
    }
}

/// A world as the save thread sees it.
struct Plan {
    name: String,
    /// The `chunks` key number of a persistent world.
    number: Option<u32>,
    seed: i64,
    generator: Arc<dyn Generator>,
}

/// The world file and the worlds, ready to run.
pub(crate) struct Opened {
    /// The worlds for the tick, in the order of the specs.
    pub(crate) worlds: Vec<World>,
    /// The engine tick of the last commit (0 for a new file).
    pub(crate) tick: u64,
    /// The save thread's side.
    pub(crate) thread: Thread,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("worlds", &self.worlds.len())
            .field("tick", &self.tick)
            .finish_non_exhaustive()
    }
}

/// What the save thread runs with.
pub(crate) struct Thread {
    storage: Storage,
    plans: Vec<Plan>,
}

/// Opens the world file (PLAN.md 10.6: backup, integrity check, compaction) and resolves the
/// worlds: a persistent world the file holds keeps its seed, spawn and number; a new one gets the
/// spec's seed or a random one, the generator's spawn and the next free number. Runs before the
/// threads start.
pub(crate) fn open(
    data_dir: &std::path::Path,
    specs: Vec<WorldSpec>,
) -> Result<Opened, crate::Error> {
    let mut storage = Storage::open(data_dir)?;
    let mut stored = Vec::new();
    for (name, sealed) in storage.worlds()? {
        let record: WorldRecord = unseal(&Key::World(name.clone()).to_bytes(), &sealed)?;
        stored.push((name, record));
    }
    let tick = match storage.read(&[Key::Meta])?.into_iter().next() {
        Some(Ok(Some(sealed))) => unseal::<Meta>(&Key::Meta.to_bytes(), &sealed)?.tick,
        Some(Err(e)) => return Err(e.into()),
        Some(Ok(None)) | None => 0,
    };
    let mut next_number = stored
        .iter()
        .map(|(_, r)| r.number)
        .max()
        .map_or(0, |n| n.saturating_add(1));
    let mut worlds = Vec::with_capacity(specs.len());
    let mut plans = Vec::with_capacity(specs.len());
    for spec in specs {
        let saved = (spec.kind == Kind::Persistent)
            .then(|| stored.iter().find(|(n, _)| *n == spec.name))
            .flatten();
        let (number, seed, spawn) = if let Some((_, record)) = saved {
            if record.generator != spec.generator.name() {
                warn!(
                    world = spec.name,
                    saved = record.generator,
                    configured = spec.generator.name(),
                    "world was saved with another generator; unsaved chunks use the configured one"
                );
            }
            let [x, y, z] = record.spawn;
            (
                Some(record.number),
                record.seed,
                Some(BlockPos::new(x, y, z)),
            )
        } else {
            let seed = match spec.seed {
                Some(seed) => seed,
                None => random_seed()?,
            };
            let number = (spec.kind == Kind::Persistent).then(|| {
                let n = next_number;
                next_number = next_number.saturating_add(1);
                n
            });
            (number, seed, None)
        };
        let spawn = spawn.unwrap_or_else(|| spec.generator.spawn(seed));
        let world = World::new(spec.name.clone(), spec.kind, seed, spawn);
        info!(
            world = spec.name,
            seed,
            new = saved.is_none(),
            "world ready"
        );
        worlds.push(world);
        plans.push(Plan {
            name: spec.name,
            number,
            seed,
            generator: spec.generator,
        });
    }
    Ok(Opened {
        worlds,
        tick,
        thread: Thread { storage, plans },
    })
}

/// A seed for a new world with an empty `level-seed`: chosen once, then saved.
fn random_seed() -> Result<i64, crate::Error> {
    let mut bytes = [0; 8];
    crate::crypto::fill_random(&mut bytes)
        .map_err(|e| crate::Error::Start(std::io::Error::other(e)))?;
    Ok(i64::from_le_bytes(bytes))
}

/// The save thread: answers requests in order until the tick drops its sender, then compacts
/// and closes the file. Returns the error of a commit that failed after its retries or of the
/// final close.
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
    let Thread { mut storage, plans } = thread;
    let mut failure = None;
    while let Some(request) = requests.blocking_recv() {
        let answers = match request {
            Request::Chunks(keys) => load_chunks(&mut storage, &plans, pool, &keys),
            Request::Player(key) => vec![load_player(&mut storage, key)],
            Request::Commit(snapshot) => {
                let start = snapshot.start;
                match commit(&mut storage, &plans, pool, &snapshot) {
                    Ok(()) => vec![Response::Committed { start }],
                    Err(e) => {
                        error!(error = %e, start, "commit failed after its retries");
                        failure = Some(e);
                        vec![Response::CommitFailed { start }]
                    }
                }
            }
        };
        for answer in answers {
            // A closed queue means the tick has ended; nothing waits for the answer.
            if responses.blocking_send(answer).is_err() {
                break;
            }
        }
    }
    let closed = storage.close();
    match (failure, closed) {
        (Some(e), _) | (None, Err(e)) => Err(e),
        (None, Ok(())) => Ok(()),
    }
}

/// The `chunks` key of a chunk of a persistent world.
fn chunk_key(number: u32, pos: ChunkPos) -> Key {
    Key::Chunk(ChunkKey {
        world: number,
        x: pos.x,
        z: pos.z,
        kind: ChunkKind::Terrain,
    })
}

/// What a read gave for one key: the sealed value, nothing, or the error.
type Stored = storage::Result<Option<Vec<u8>>>;

/// Loads a batch: reads the persistent worlds' chunks in one read transaction, then decodes or
/// generates every chunk on the CPU pool. A failed read of the whole batch quarantines nothing:
/// its chunks are answered as damaged, which keeps their saved bytes untouched.
fn load_chunks(
    storage: &mut Storage,
    plans: &[Plan],
    pool: &Pool,
    keys: &[(WorldIndex, ChunkPos)],
) -> Vec<Response> {
    let stored: Vec<Key> = keys
        .iter()
        .filter_map(|&(w, pos)| Some(chunk_key(plans.get(w)?.number?, pos)))
        .collect();
    let mut read = match storage.read(&stored) {
        Ok(values) => values.into_iter(),
        Err(e) => {
            error!(error = %e, "chunk read failed");
            Vec::new().into_iter()
        }
    };
    let read_failed = read.len() != stored.len();
    // Each key with the stored value it got, if its world is persistent.
    let items: Vec<(WorldIndex, ChunkPos, Option<Stored>)> = keys
        .iter()
        .map(|&(w, pos)| {
            let persistent = plans.get(w).is_some_and(|p| p.number.is_some());
            let value = (persistent && !read_failed).then(|| read.next()).flatten();
            let value = if persistent && read_failed {
                Some(Err(storage::Error::Malformed("the batch read failed")))
            } else {
                value
            };
            (w, pos, value)
        })
        .collect();
    pool.map(&items, |(world, pos, value)| {
        let loaded = match (plans.get(*world), value) {
            (None, _) => Loaded::Damaged,
            (Some(plan), None | Some(Ok(None))) => {
                Loaded::Generated(Box::new(Chunk::generate(
                    plan.generator.as_ref(),
                    plan.seed,
                    *pos,
                )))
            }
            (Some(plan), Some(Ok(Some(sealed)))) => {
                let key = plan.number.map(|n| chunk_key(n, *pos));
                match key.map(|k| unseal::<ChunkRecord>(&k.to_bytes(), sealed)) {
                    Some(Ok(record)) => {
                        to_chunk(record).map_or(Loaded::Damaged, |c| Loaded::Disk(Box::new(c)))
                    }
                    Some(Err(e)) => {
                        warn!(world = plan.name, x = pos.x, z = pos.z, error = %e, "chunk value damaged");
                        Loaded::Damaged
                    }
                    None => Loaded::Damaged,
                }
            }
            (Some(plan), Some(Err(e))) => {
                warn!(world = plan.name, x = pos.x, z = pos.z, error = %e, "chunk read failed");
                Loaded::Damaged
            }
        };
        Response::Chunk {
            world: *world,
            pos: *pos,
            loaded,
        }
    })
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

/// Commits a snapshot: the meta value, every persistent world's record, the changed chunks
/// (sealed on the CPU pool) and the changed players, in one write transaction.
fn commit(
    storage: &mut Storage,
    plans: &[Plan],
    pool: &Pool,
    snapshot: &Snapshot,
) -> Result<(), storage::Error> {
    let mut batch = Batch::with_capacity(
        snapshot.chunks.len() + snapshot.players.len() + snapshot.spawns.len() + 1,
    );
    batch.put(
        Key::Meta,
        seal(
            &Key::Meta.to_bytes(),
            &Meta {
                tick: snapshot.start,
            },
        )?,
    );
    for &(world, spawn) in &snapshot.spawns {
        let Some(plan) = plans.get(world) else {
            continue;
        };
        let Some(number) = plan.number else {
            continue;
        };
        let key = Key::World(plan.name.clone());
        let record = WorldRecord {
            number,
            seed: plan.seed,
            spawn: [spawn.x, spawn.y, spawn.z],
            generator: plan.generator.name().to_owned(),
        };
        batch.put(key.clone(), seal(&key.to_bytes(), &record)?);
    }
    let sealed = pool.map(&snapshot.chunks, |(world, pos, chunk)| {
        let number = plans.get(*world).and_then(|p| p.number)?;
        let key = chunk_key(number, *pos);
        let value = seal(&key.to_bytes(), &to_record(chunk));
        Some((key, value))
    });
    for (key, value) in sealed.into_iter().flatten() {
        batch.put(key, value?);
    }
    for (key, data) in &snapshot.players {
        let key = Key::Player(key.clone());
        let record = PlayerRecord {
            world: data.world.clone(),
            position: data.position,
        };
        batch.put(key.clone(), seal(&key.to_bytes(), &record)?);
    }
    storage.commit(&batch)
}

/// The stored form of a chunk: block states as name, states and version (PLAN.md 6), unknown
/// states as they were read.
pub(crate) fn to_record(chunk: &Chunk) -> ChunkRecord {
    let sub_chunks = std::array::from_fn(|i| {
        let mut layers = Vec::new();
        if let Some(blocks) = chunk.sub_chunk(i) {
            let [blocks, water] = blocks.layers();
            layers.push(block_layer(chunk, blocks));
            if !water.is_uniform(Entry::AIR) {
                layers.push(block_layer(chunk, water));
            }
        }
        let biomes = chunk.biome_storage(i).map_or_else(
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
        );
        SubChunkRecord { layers, biomes }
    });
    ChunkRecord {
        sub_chunks: Box::new(sub_chunks),
    }
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
                Entry::Unknown(i) => usize::try_from(i)
                    .ok()
                    .and_then(|i| chunk.unknown().get(i))
                    .cloned()
                    .unwrap_or_else(|| state_nbt(crate::block::AIR)),
            })
            .collect(),
    }
}

/// A block state as Bedrock's disk palette writes it: `{name, states, version}`.
fn state_nbt(state: State) -> nbt::Compound {
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

/// The block state a disk palette entry names, if Anchor knows it.
fn find_state(entry: &nbt::Compound) -> Option<State> {
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
    State::find(name, &properties)
}

/// The chunk of a stored record, or `None` if its parts are invalid (damage: the record decoded
/// but does not describe a chunk). Unknown states keep their NBT (PLAN.md 7).
pub(crate) fn to_chunk(record: ChunkRecord) -> Option<Chunk> {
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
                    if let Some(state) = find_state(&entry) {
                        return Entry::Known(state);
                    }
                    let index = u32::try_from(unknown.len()).unwrap_or(u32::MAX);
                    unknown.push(entry);
                    Entry::Unknown(index)
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
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::block;
    use crate::world::Flat;

    /// A per-test folder under the system temp folder, removed on drop.
    struct TempDir(PathBuf);

    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    impl TempDir {
        fn new(name: &str) -> Self {
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

    fn specs() -> Vec<WorldSpec> {
        vec![
            WorldSpec {
                name: "world".to_owned(),
                kind: Kind::Persistent,
                generator: Arc::new(Flat),
                seed: Some(5),
            },
            WorldSpec {
                name: "arena".to_owned(),
                kind: Kind::Temporary,
                generator: Arc::new(Flat),
                seed: Some(9),
            },
        ]
    }

    fn stone() -> State {
        State::find("minecraft:stone", &[]).unwrap()
    }

    /// A flat chunk with a stone block at local (1, -60, 2).
    fn changed_chunk(pos: ChunkPos) -> Chunk {
        let mut chunk = Chunk::generate(&Flat, 5, pos);
        chunk.set_block(1, -60, 2, stone()).unwrap();
        chunk
    }

    /// Runs the save thread on `dir` and answers `requests` in order; returns the answers and
    /// the thread's result.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test waits for the save thread's answers"
    )]
    fn serve(
        dir: &TempDir,
        requests: Vec<Request>,
    ) -> (Vec<Response>, u64, Result<(), storage::Error>) {
        let opened = open(&dir.0, specs()).unwrap();
        let tick = opened.tick;
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
        for request in requests {
            tx.blocking_send(request).unwrap();
        }
        drop(tx);
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
        let read = to_chunk(record.clone()).unwrap();
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
    fn the_save_thread_commits_a_snapshot_and_loads_it_back_after_a_restart() {
        let dir = TempDir::new("round-trip");
        let changed = ChunkPos::new(2, 2);
        let player = PlayerKey::offline("Steve");
        let data = PlayerData {
            world: "world".to_owned(),
            position: [1.5, -58.38, 2.5],
        };
        let snapshot = Snapshot {
            start: 77,
            chunks: vec![(0, changed, changed_chunk(changed))],
            players: vec![(player.clone(), data.clone())],
            spawns: vec![(0, BlockPos::new(4, -60, 4))],
        };
        let (answers, tick, result) = serve(&dir, vec![Request::Commit(Box::new(snapshot))]);
        result.unwrap();
        assert_eq!(tick, 0, "a new file");
        assert!(matches!(answers[..], [Response::Committed { start: 77 }]));

        let loads = vec![
            Request::Chunks(vec![(0, changed), (0, ChunkPos::new(9, 9)), (1, changed)]),
            Request::Player(player.clone()),
            Request::Player(PlayerKey::offline("Alex")),
        ];
        let opened = open(&dir.0, specs()).unwrap();
        assert_eq!((opened.tick, opened.worlds[0].seed()), (77, 5));
        assert_eq!(opened.worlds[0].spawn(), BlockPos::new(4, -60, 4));
        drop(opened);
        let (answers, tick, result) = serve(&dir, loads);
        result.unwrap();
        assert_eq!(tick, 77);
        let [
            Response::Chunk {
                loaded: Loaded::Disk(disk),
                ..
            },
            Response::Chunk {
                loaded: Loaded::Generated(new),
                ..
            },
            Response::Chunk {
                loaded: Loaded::Generated(temporary),
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
        assert_eq!(
            temporary.block(1, -60, 2),
            Some(block::AIR),
            "a temporary world is never saved"
        );
        assert_eq!(*found, PlayerLoad::Found(data));
        assert_eq!(*absent, PlayerLoad::Absent);
    }

    #[test]
    fn a_damaged_chunk_value_loads_as_damaged_and_stays_as_it_was() {
        let dir = TempDir::new("damaged");
        let pos = ChunkPos::new(0, 1);
        let key = chunk_key(0, pos);
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
}
