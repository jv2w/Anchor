//! The replay tool against live runs with the real save thread on a temporary server file: a crash
//! log replays from the state of its last commit to the same states (a task sends the state hash of
//! every tick in both runs) and panics where the server did; a commit the tick never learnt of is
//! where the replay starts; an opaque checkpoint and a server built otherwise are refused; a run
//! that loads many stored chunks replays to the same states whatever the CPU pool's size.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use super::packet;
use crate::config::Config;
use crate::event::{Command, Listeners, Priority};
use crate::math::Vec3;
use crate::protocol::Uuid;
use crate::protocol::packet::{Packet, RequestChunkRadius, SetLocalPlayerAsInitialised};
use crate::replay::{End, Error};
use crate::runtime::ThreadCounts;
use crate::save::{Request, Response, WorldSpec};
use crate::storage::ChunkKind;
use crate::tick::log::EntryKind;
use crate::tick::queue::{
    Identity, Input, InputKind, InputSender, Measure, Outbound, SessionId, SessionInputs,
    input_queue, outbound_queue,
};
use crate::tick::{
    Fingerprints, InputLog, Output, Phase, SaveLink, Settings, Setup, Shared, State, Tasks,
    boundary, deliver, run, step, write_crash_log,
};
use crate::world::{BlockPos, Chunk, ChunkPos, Flat, Kind};

/// The ticks between two stops of the tour.
const STOP_TICKS: u64 = 12;
/// The chunks the tour stops at, one every [`STOP_TICKS`] ticks from [`TOUR_START`].
const STOPS: u64 = 8;
/// Chests in each chunk of the tour's file, along z = 1 at the height of the first air.
const TOUR_CHESTS: u8 = 6;
/// The tick of the tour's first stop.
const TOUR_START: u64 = 13;
/// The ticks of a tour run: the tour and the loads of its last stop.
const TOUR_TICKS: u64 = TOUR_START + STOP_TICKS * STOPS + 20;

/// The (tick, state hash) a run's task sends every tick.
type Hashes = Vec<(u64, u64)>;

/// What the task of a run does besides sending hashes.
#[derive(Debug, Clone, Copy)]
struct Plan {
    /// Ticks at which it asks for a save.
    saves: &'static [u64],
    /// The tick at which it panics, if any.
    panic_at: Option<u64>,
    /// The tick at which it schedules a task far in the future (it never runs).
    schedule_at: Option<u64>,
    /// The console sends a command once the live run reached this tick, and a `Command`
    /// listener panics.
    command_at: Option<u64>,
}

fn specs() -> Vec<WorldSpec> {
    vec![WorldSpec {
        name: "world".to_owned(),
        kind: Kind::Persistent,
        generator: Arc::new(Flat),
        seed: Some(0),
        spawn: None,
    }]
}

/// A fresh data folder of this test.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test prepares its data folder"
)]
fn fresh(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("anchor-replay-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).unwrap_or(());
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test removes its data folder"
)]
fn remove(dir: &Path) {
    std::fs::remove_dir_all(dir).unwrap();
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test reads a file of its data folder"
)]
fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

/// The bytes of every data file of the data folder `dir`, by path: the main file, the players
/// file and the world files.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test lists the world files of its data folder"
)]
fn data_files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir.join(crate::storage::WORLD_DIR))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.push(dir.join("main.redb"));
    paths.push(dir.join("players.redb"));
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let bytes = read(&path);
            (path, bytes)
        })
        .collect()
}

/// The build of a run: one task that sends the state hash, sets a block every other tick and
/// does what `plan` says, and `extra` tasks.
fn setup(plan: Plan, hashes: mpsc::Sender<(u64, u64)>, extra: usize) -> Setup {
    let stone = crate::block::State::find("minecraft:stone", &[]).unwrap();
    let mut tasks = Tasks::default();
    tasks.push(NonZeroU32::MIN, move |cx| {
        let tick = cx.tick();
        hashes.try_send((tick, cx.state.hash())).unwrap();
        let t = i32::try_from(tick % 1000).unwrap();
        if t % 2 == 0 {
            let at = BlockPos::new((t * 7) % 32 - 16, -60, (t * 13) % 32 - 16);
            let state = if t % 3 == 0 { crate::block::AIR } else { stone };
            // The spawn chunks load in the first ticks.
            match cx.default_world_mut().set_block(at, state) {
                Ok(()) | Err(crate::world::Error::NotLoaded) => {}
                Err(e) => panic!("{e}"),
            }
        }
        if plan.saves.contains(&tick) {
            cx.save();
        }
        if plan.schedule_at == Some(tick) {
            cx.schedule(1_000_000, |_| {}).unwrap();
        }
        assert!(plan.panic_at != Some(tick), "replayed panic at {tick}");
    });
    for _ in 0..extra {
        tasks.push(NonZeroU32::MIN, |_| {});
    }
    let mut listeners = Listeners::default();
    if plan.command_at.is_some() {
        listeners.push(Priority::Normal, |_: &mut Command, cx| {
            panic!("command listener failed at {}", cx.tick());
        });
    }
    Setup {
        listeners,
        tasks,
        ..Setup::default()
    }
}

/// The tick of the last save of the data files in `dir`.
fn committed(dir: &Path) -> u64 {
    crate::save::committed_tick(dir).unwrap()
}

/// The hashes sent so far.
fn drain(rx: &mut mpsc::Receiver<(u64, u64)>) -> Hashes {
    let mut out = Vec::new();
    while let Ok(h) = rx.try_recv() {
        out.push(h);
    }
    out
}

/// A player of session 1 joins, asks for its chunks and spawns, as a session would; its
/// outbound queue and its input sender.
fn join(inputs: &InputSender) -> (mpsc::Receiver<Outbound>, SessionInputs) {
    let (outbound, rx) = outbound_queue();
    let mut session = inputs.session(SessionId::new(1));
    session
        .try_send(InputKind::Join {
            identity: Identity {
                name: "replayer".to_owned(),
                xuid: None,
                uuid: Uuid::from_bytes([5; 16]),
                skin: crate::tick::queue::Skin::default(),
                device_os: -1,
                address: None,
                client: crate::tick::queue::Client::default(),
            },
            outbound,
        })
        .unwrap();
    for p in [
        Packet::RequestChunkRadius(RequestChunkRadius {
            chunk_radius: 2,
            max_chunk_radius: 0,
        }),
        Packet::SetLocalPlayerAsInitialised(SetLocalPlayerAsInitialised {
            entity_runtime_id: 1,
        }),
    ] {
        session.try_send(packet(p)).unwrap();
    }
    (rx, session)
}

/// Runs the server of `plan` with `tick::run` and the save thread on `dir` until its task
/// panics, writes the crash log and returns its path and the hashes the run sent.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test drains its player's queue and waits for its threads"
)]
fn crash(dir: &Path, plan: Plan) -> (PathBuf, Hashes) {
    let config = Config::default();
    let (hashes, mut sent) = mpsc::channel(4096);
    let mut setup = setup(plan, hashes, 0);
    setup.fingerprints = Fingerprints::of(&Settings::new(&config), &specs(), &setup);
    let opened = crate::save::open(dir, specs()).unwrap();
    let pool = Arc::new(crate::runtime::cpu_pool(2).unwrap());
    let (inputs, input_queue) = input_queue(1);
    let (_stop_tx, stop) = mpsc::channel(1);
    let (done, _done_rx) = oneshot::channel();
    let (crashed, mut crash_log) = oneshot::channel();
    let (requests, request_queue) = mpsc::channel(crate::save::REQUEST_CAPACITY);
    let (responder, responses) = mpsc::channel(crate::save::RESPONSE_CAPACITY);
    let save_pool = Arc::clone(&pool);
    let thread = opened.thread;
    let save_thread = crate::runtime::spawn_save(move || {
        crate::save::run(thread, &save_pool, request_queue, &responder)
    })
    .unwrap();
    let mut state = State::new(
        Settings::new(&config),
        shared(),
        setup,
        opened.worlds,
        opened.tick,
    );
    state.set_identity(opened.identity);
    let (mut rx, session) = join(&inputs);
    let reader = std::thread::spawn(move || while rx.blocking_recv().is_some() {});
    let link = SaveLink {
        requests,
        responses,
    };
    let tick_pool = Arc::clone(&pool);
    let tick_thread = crate::runtime::spawn_tick(move || {
        run(state, &tick_pool, input_queue, link, stop, crashed, done)
    })
    .unwrap();
    // The hashes until the panic drops the task; the console line goes once the run reached
    // its tick.
    let mut live = Vec::new();
    let mut console = inputs.session(SessionId::CONSOLE);
    while let Some((tick, hash)) = sent.blocking_recv() {
        live.push((tick, hash));
        if plan.command_at == Some(tick) {
            console
                .try_send(InputKind::Console("list".to_owned()))
                .unwrap();
        }
    }
    assert!(tick_thread.join().is_err(), "the panic ends the tick");
    drop((inputs, session, console));
    save_thread.join().unwrap().unwrap();
    reader.join().unwrap();
    let log = crash_log.try_recv().expect("the log is handed over");
    let path = write_crash_log(dir, &log).unwrap();
    (path, live)
}

fn shared() -> Shared {
    Shared::new(
        crate::protocol::batch::Settings::default(),
        &crate::custom::Registry::default(),
        &crate::tick::entity::Customs::default(),
    )
    .unwrap()
}

/// Replays `log` on `dir` with the build of `plan` (and `extra` more tasks) and `config`; the
/// replay's result and the hashes it sent.
fn replay(
    dir: &Path,
    log: &Path,
    plan: Plan,
    extra: usize,
    config: &Config,
) -> (crate::replay::Result<crate::replay::Replay>, Hashes) {
    let (hashes, mut sent) = mpsc::channel(4096);
    let threads = ThreadCounts { network: 1, cpu: 2 };
    let result = crate::replay::run(
        config,
        dir,
        threads,
        setup(plan, hashes, extra),
        &specs(),
        log,
    );
    (result, drain(&mut sent))
}

/// The hashes of `live` from tick `from` on.
fn from(live: &Hashes, tick: u64) -> Hashes {
    live.iter().copied().filter(|&(t, _)| t >= tick).collect()
}

#[test]
fn a_crash_log_replays_from_its_last_commit_to_the_same_states_and_panics_where_the_server_did() {
    let dir = fresh("panic");
    let plan = Plan {
        saves: &[10, 25],
        panic_at: Some(45),
        schedule_at: None,
        command_at: None,
    };
    let (log, live) = crash(&dir, plan);
    let before = data_files(&dir);
    let (result, replayed) = replay(&dir, &log, plan, 0, &Config::default());
    let replay = result.unwrap();
    // The server file's last commit: the snapshot asked for at tick 25, before tick 26, unless the
    // save thread was slower than the crash.
    let base = committed(&dir);
    assert_eq!(replay.base_tick(), base);
    assert_eq!(
        replay.end(),
        &End::Panicked {
            tick: 45,
            message: "replayed panic at 45".to_owned(),
        }
    );
    assert_eq!(
        (replay.last_tick(), replay.ticks()),
        (Some(45), 45 - base + 1)
    );
    assert_eq!(
        replayed,
        from(&live, base),
        "the replayed states are the live ones"
    );
    assert_eq!(data_files(&dir), before, "the replay writes nothing");
    // The live loop measured its ticks off the game logic and logged each time for the next
    // tick: the replay read the same times, so its states are the live ones.
    let written = InputLog::decode(&read(&log)).unwrap();
    let measured = written
        .ticks()
        .iter()
        .flat_map(|t| &t.inputs)
        .filter(|e| {
            e.session == SessionId::ENGINE
                && matches!(e.kind, EntryKind::Measure(Measure::TickTime(_)))
        })
        .count();
    assert!(measured > 0, "the run logged no tick time");
    remove(&dir);
}

#[test]
fn a_listener_panic_replays_at_its_tick_with_its_message() {
    let dir = fresh("listener");
    let plan = Plan {
        saves: &[10, 25],
        panic_at: None,
        schedule_at: None,
        command_at: Some(30),
    };
    let (log, live) = crash(&dir, plan);
    // The line arrived in a tick after the run reached tick 30; the listener panicked in its
    // input phase, before the task, so the task's last tick is the one before.
    let tick = live.last().unwrap().0 + 1;
    assert!(tick > 30, "{tick}");
    let (result, replayed) = replay(&dir, &log, plan, 0, &Config::default());
    let replay = result.unwrap();
    let base = committed(&dir);
    assert_eq!(replay.base_tick(), base);
    assert_eq!(
        replay.end(),
        &End::Panicked {
            tick,
            message: format!("command listener failed at {tick}"),
        }
    );
    assert_eq!(replay.ticks(), tick - base + 1);
    assert_eq!(replayed, from(&live, base));
    remove(&dir);
}

#[test]
fn an_opaque_checkpoint_is_refused() {
    let dir = fresh("opaque");
    let plan = Plan {
        saves: &[10, 25],
        panic_at: Some(35),
        schedule_at: Some(5),
        command_at: None,
    };
    let (log, _) = crash(&dir, plan);
    let (result, replayed) = replay(&dir, &log, plan, 0, &Config::default());
    match result {
        Err(Error::Opaque { tick, what }) => {
            // Every snapshot came after the task was scheduled at tick 5.
            assert_eq!(tick, committed(&dir));
            assert!(tick > 5);
            assert_eq!(what, "1 scheduled tasks");
        }
        other => panic!("{other:?}"),
    }
    assert!(replayed.is_empty());
    remove(&dir);
}

#[test]
fn a_server_built_otherwise_is_refused() {
    let dir = fresh("differ");
    let plan = Plan {
        saves: &[10],
        panic_at: Some(15),
        schedule_at: None,
        command_at: None,
    };
    let (log, _) = crash(&dir, plan);
    let mut other = Config::default();
    other.general.view_distance = 9;
    let (result, _) = replay(&dir, &log, plan, 0, &other);
    assert!(matches!(result, Err(Error::SettingsDiffer)), "{result:?}");
    let (result, _) = replay(&dir, &log, plan, 1, &Config::default());
    assert!(
        matches!(result, Err(Error::RegistrationsDiffer)),
        "{result:?}"
    );
    // Without a server file of any segment's tick there is nothing to start from, and without
    // the log nothing to replay.
    let empty = fresh("differ-empty");
    let (result, _) = replay(&empty, &log, plan, 0, &Config::default());
    assert!(matches!(result, Err(Error::NoMatchingWorld)), "{result:?}");
    let (result, _) = replay(
        &empty,
        &empty.join("input.log"),
        plan,
        0,
        &Config::default(),
    );
    assert!(matches!(result, Err(Error::Io(_))), "{result:?}");
    remove(&empty);
    remove(&dir);
}

/// Runs the server of `plan` for `ticks` ticks with the real save thread on `dir` but its own
/// loop, which holds back the answer to the commit of the snapshot at tick `held` (it waits for
/// the commit, so the server file has it); returns the log and the hashes sent.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test plays the tick's side of the save thread"
)]
fn held_commit(dir: &Path, plan: Plan, ticks: u64, held: u64) -> (PathBuf, Hashes) {
    let config = Config::default();
    let (hashes, mut sent) = mpsc::channel(4096);
    let mut setup = setup(plan, hashes, 0);
    setup.fingerprints = Fingerprints::of(&Settings::new(&config), &specs(), &setup);
    let opened = crate::save::open(dir, specs()).unwrap();
    let pool = crate::runtime::cpu_pool(2).unwrap();
    let save_pool = Arc::new(crate::runtime::cpu_pool(1).unwrap());
    let (requests, request_queue) = mpsc::channel(crate::save::REQUEST_CAPACITY);
    let (responder, mut responses) = mpsc::channel(crate::save::RESPONSE_CAPACITY);
    let thread = opened.thread;
    let worker = Arc::clone(&save_pool);
    let save_thread = crate::runtime::spawn_save(move || {
        crate::save::run(thread, &worker, request_queue, &responder)
    })
    .unwrap();
    let mut state = State::new(
        Settings::new(&config),
        shared(),
        setup,
        opened.worlds,
        opened.tick,
    );
    let (inputs, mut queue) = input_queue(1);
    let (mut rx, _session) = join(&inputs);
    let mut log = InputLog::new(state.checkpoint(&pool), state.fingerprints, opened.identity);
    let mut answers = Vec::new();
    let mut output = Output::default();
    for _ in 0..ticks {
        let mut batch: Vec<Input> = std::iter::from_fn(|| queue.try_recv().ok()).collect();
        answers.extend(std::iter::from_fn(|| responses.try_recv().ok()));
        step(
            &mut state,
            &mut batch,
            &mut answers,
            &pool,
            &mut output,
            &mut log,
        );
        deliver(&mut state, &mut output, &mut log);
        boundary(&mut state, &mut output, &mut log);
        if log.needs_checkpoint() {
            log.set_checkpoint(state.checkpoint(&pool));
        }
        while rx.try_recv().is_ok() {}
        for request in output.requests.drain(..) {
            let commit = match &request {
                Request::Commit(snapshot) => Some(snapshot.start),
                _ => None,
            };
            requests.blocking_send(request).unwrap();
            if let Some(commit) = commit {
                // Wait for the commit, keeping the other answers for the next ticks; the answer
                // to the held one never reaches the tick.
                loop {
                    match responses.blocking_recv().unwrap() {
                        Response::Committed { start } if start == commit => {
                            if start != held {
                                answers.push(Response::Committed { start });
                            }
                            break;
                        }
                        other => answers.push(other),
                    }
                }
            }
        }
    }
    drop(requests);
    save_thread.join().unwrap().unwrap();
    assert!(pool.shutdown());
    let path = write_crash_log(dir, &log).unwrap();
    (path, drain(&mut sent))
}

#[test]
fn a_commit_the_tick_never_learnt_of_is_where_the_replay_starts() {
    let dir = fresh("held");
    let plan = Plan {
        saves: &[10, 25],
        panic_at: None,
        schedule_at: None,
        command_at: None,
    };
    let (log, live) = held_commit(&dir, plan, 40, 26);
    // The log's newest committed segment is the one of tick 11; the server file is at tick 26.
    let written = InputLog::decode(&read(&log)).unwrap();
    let starts: Vec<u64> = written
        .segments()
        .map(crate::tick::Segment::start)
        .collect();
    assert_eq!(starts, [0, 11, 26]);
    assert_eq!(crate::save::committed_tick(&dir).unwrap(), 26);
    let (result, replayed) = replay(&dir, &log, plan, 0, &Config::default());
    let replay = result.unwrap();
    assert_eq!(
        replay.base_tick(),
        26,
        "the server file holds the commit of tick 26"
    );
    assert_eq!(replay.end(), &End::Completed);
    assert_eq!((replay.last_tick(), replay.ticks()), (Some(39), 14));
    assert_eq!(replayed, from(&live, 26));
    remove(&dir);
}

/// The center chunk of stop `i`: far enough from the others and the spawn that each stop loads
/// chunks of its own.
fn stop(i: u64) -> ChunkPos {
    ChunkPos::new(16 * (i32::try_from(i).unwrap() + 1), 16)
}

/// The first chest of chunk `pos` in the tour's file.
fn first_chest(pos: ChunkPos) -> BlockPos {
    BlockPos::new(pos.x * 16, -60, pos.z * 16 + 1)
}

/// The commit that puts the tour's chunks in the file: the 3 by 3 chunks around each stop, each
/// with chests full of sticks, a dropped stick, a pig and an orb.
fn tour_fixture(world: &crate::world::World) -> crate::save::Snapshot {
    use crate::nbt::{Compound, List, Value};
    let chest = crate::block::Kind::Chest.default_state();
    let sticks =
        |count: u8| crate::item::Stack::new(crate::item::Kind::Stick.default_type(), count);
    let entity = |identifier: &str, pos: [f64; 3], rest: Vec<(&str, Value)>| {
        let mut c = Compound::from([
            (
                "identifier".to_owned(),
                Value::String(identifier.to_owned()),
            ),
            ("Pos".to_owned(), Value::List(List::Double(pos.to_vec()))),
            (
                "Rotation".to_owned(),
                Value::List(List::Float(vec![0.0, 0.0])),
            ),
        ]);
        for (k, v) in rest {
            c.insert(k.to_owned(), v);
        }
        c
    };
    let mut snapshot = crate::save::Snapshot {
        start: 1,
        identity: 1,
        worlds: vec![(0, crate::save::WorldValues::of(world))],
        ..crate::save::Snapshot::default()
    };
    for i in 0..STOPS {
        for (dx, dz) in (-1..=1).flat_map(|dx| (-1..=1).map(move |dz| (dx, dz))) {
            let center = stop(i);
            let pos = ChunkPos::new(center.x + dx, center.z + dz);
            let mut terrain = Chunk::generate(&Flat, world.seed(), pos);
            let mut block_entities = Vec::new();
            for k in 0..TOUR_CHESTS {
                terrain.set_block(2 * k, -60, 1, chest).unwrap();
                let first = first_chest(pos);
                let at = BlockPos::new(first.x + 2 * i32::from(k), first.y, first.z);
                let items = (0_u8..27)
                    .map(|slot| {
                        let mut c = sticks(slot + 1).to_nbt();
                        c.insert("Slot".to_owned(), Value::Byte(slot));
                        c
                    })
                    .collect();
                block_entities.push(Compound::from([
                    ("id".to_owned(), Value::String("Chest".to_owned())),
                    ("x".to_owned(), Value::Int(at.x)),
                    ("y".to_owned(), Value::Int(at.y)),
                    ("z".to_owned(), Value::Int(at.z)),
                    ("Items".to_owned(), Value::List(List::Compound(items))),
                ]));
            }
            let (x, z) = (f64::from(pos.x * 16), f64::from(pos.z * 16));
            let entities = vec![
                entity(
                    "minecraft:item",
                    [x + 4.5, -60.0, z + 8.5],
                    vec![
                        ("Item", Value::Compound(sticks(5).to_nbt())),
                        ("Health", Value::Short(5)),
                        ("Age", Value::Short(10)),
                        ("PickupDelay", Value::Short(10)),
                    ],
                ),
                entity("minecraft:pig", [x + 8.5, -60.0, z + 8.5], Vec::new()),
                entity(
                    "minecraft:xp_orb",
                    [x + 12.5, -60.0, z + 8.5],
                    vec![("Value", Value::Short(3))],
                ),
            ];
            snapshot.chunks.push((0, pos, terrain));
            snapshot
                .extras
                .push((0, pos, ChunkKind::BlockEntities, block_entities));
            snapshot
                .extras
                .push((0, pos, ChunkKind::Entities, entities));
        }
    }
    snapshot
}

/// The stops whose first chest is loaded (bit `i` for stop `i`) and the entities, each tick.
type Seen = Vec<(u64, u32, usize)>;

/// The build of a tour run: one task that sends the state hash and what is loaded of the tour,
/// and from [`TOUR_START`] teleports the player of session 1 to the next stop every
/// [`STOP_TICKS`] ticks.
fn tour_setup(hashes: mpsc::Sender<(u64, u64)>, seen: mpsc::Sender<(u64, u32, usize)>) -> Setup {
    let mut tasks = Tasks::default();
    tasks.push(NonZeroU32::MIN, move |cx| {
        let tick = cx.tick();
        hashes.try_send((tick, cx.state.hash())).unwrap();
        let world = crate::world::Id::DEFAULT;
        let chests = (0..STOPS)
            .filter(|&i| cx.block_entity(world, first_chest(stop(i))).is_some())
            .fold(0_u32, |bits, i| bits | 1 << i);
        seen.try_send((tick, chests, cx.entities(world).count()))
            .unwrap();
        let leg = tick.checked_sub(TOUR_START).filter(|t| t % STOP_TICKS == 0);
        if let Some(i) = leg.map(|t| t / STOP_TICKS)
            && i < STOPS
        {
            let center = stop(i);
            let feet = Vec3::new(
                f64::from(center.x * 16) + 8.5,
                -60.0,
                f64::from(center.z * 16) + 8.5,
            );
            let id = crate::player::Id::new(SessionId::new(1));
            if cx.player(id).is_some() {
                cx.teleport(id, world, feet).unwrap();
            }
        }
    });
    Setup {
        tasks,
        ..Setup::default()
    }
}

/// Puts the tour's chunks in the data files in `dir`, then runs the tour for [`TOUR_TICKS`]
/// ticks with the real save thread and a loop of its own, which plays the client's side of the
/// join; returns the log and what the run's task sent.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test plays the tick's side of the save thread"
)]
fn tour(dir: &Path) -> (PathBuf, Hashes, Seen) {
    {
        let mut opened = crate::save::open(dir, specs()).unwrap();
        let pool = crate::runtime::cpu_pool(1).unwrap();
        let snapshot = tour_fixture(&opened.worlds[0]);
        opened.thread.commit(&pool, &snapshot).unwrap();
        assert!(pool.shutdown());
    }
    let config = Config::default();
    let (hashes, mut sent) = mpsc::channel(4096);
    let (seen, mut seen_rx) = mpsc::channel(4096);
    let mut setup = tour_setup(hashes, seen);
    setup.fingerprints = Fingerprints::of(&Settings::new(&config), &specs(), &setup);
    let opened = crate::save::open(dir, specs()).unwrap();
    let pool = crate::runtime::cpu_pool(2).unwrap();
    let save_pool = Arc::new(crate::runtime::cpu_pool(2).unwrap());
    let (requests, request_queue) = mpsc::channel(crate::save::REQUEST_CAPACITY);
    let (responder, mut responses) = mpsc::channel(crate::save::RESPONSE_CAPACITY);
    let thread = opened.thread;
    let worker = Arc::clone(&save_pool);
    let save_thread = crate::runtime::spawn_save(move || {
        crate::save::run(thread, &worker, request_queue, &responder)
    })
    .unwrap();
    let mut state = State::new(
        Settings::new(&config),
        shared(),
        setup,
        opened.worlds,
        opened.tick,
    );
    state.set_identity(opened.identity);
    let (inputs, mut queue) = input_queue(1);
    let (mut rx, mut session) = join(&inputs);
    let mut log = InputLog::new(state.checkpoint(&pool), state.fingerprints, opened.identity);
    let mut answers = Vec::new();
    let mut output = Output::default();
    // The loads asked for and not answered yet: each tick waits for them, so the run does not
    // outpace the save thread.
    let mut loads = 0_usize;
    for _ in 0..TOUR_TICKS {
        let mut batch: Vec<Input> = std::iter::from_fn(|| queue.try_recv().ok()).collect();
        while loads > 0 {
            let answer = responses.blocking_recv().unwrap();
            if matches!(answer, Response::Chunk { .. } | Response::Player { .. }) {
                loads -= 1;
            }
            answers.push(answer);
        }
        step(
            &mut state,
            &mut batch,
            &mut answers,
            &pool,
            &mut output,
            &mut log,
        );
        deliver(&mut state, &mut output, &mut log);
        boundary(&mut state, &mut output, &mut log);
        if log.needs_checkpoint() {
            log.set_checkpoint(state.checkpoint(&pool));
        }
        while rx.try_recv().is_ok() {}
        // The client asks for its chunks once it has joined and says it is in once they came.
        let phase = state
            .players
            .values()
            .next()
            .map(|p| (p.phase, p.runtime_id));
        let reply = match phase {
            Some((Phase::Joined, _)) => Some(Packet::RequestChunkRadius(RequestChunkRadius {
                chunk_radius: 2,
                max_chunk_radius: 0,
            })),
            Some((Phase::Spawning, entity_runtime_id)) => {
                Some(Packet::SetLocalPlayerAsInitialised(
                    SetLocalPlayerAsInitialised { entity_runtime_id },
                ))
            }
            _ => None,
        };
        if let Some(p) = reply {
            session.try_send(packet(p)).unwrap();
        }
        for request in output.requests.drain(..) {
            loads += match &request {
                Request::Chunks(keys) => keys.len(),
                Request::Player(_) => 1,
                _ => 0,
            };
            requests.blocking_send(request).unwrap();
        }
    }
    drop(requests);
    save_thread.join().unwrap().unwrap();
    assert!(pool.shutdown());
    let path = write_crash_log(dir, &log).unwrap();
    let mut stops = Vec::new();
    while let Ok(s) = seen_rx.try_recv() {
        stops.push(s);
    }
    (path, drain(&mut sent), stops)
}

/// A run that loads many chunks of a persistent world with block entities and entities (the
/// player is teleported from stop to stop) replays from its start to the same state hash every
/// tick, with a CPU pool of 1 thread and of 4.
#[test]
fn a_chunk_loading_tour_replays_to_the_same_states_on_one_and_four_threads() {
    let dir = fresh("tour");
    let (log, live, seen) = tour(&dir);
    let every_stop = (1_u32 << STOPS) - 1;
    let loaded = seen.iter().fold(0, |bits, &(_, stops, _)| bits | stops);
    assert_eq!(
        loaded, every_stop,
        "each stop's chests loaded from the file"
    );
    // The dropped stick and the orb of each of a stop's 9 chunks; the pig, no entity type of
    // the server, stays a saved compound.
    let most = seen.iter().map(|&(_, _, entities)| entities).max();
    assert_eq!(most, Some(9 * 2), "a stop's entities loaded from the file");
    for cpu in [1, 4] {
        let (hashes, mut sent) = mpsc::channel(4096);
        let (seen_tx, mut seen_rx) = mpsc::channel(4096);
        let result = crate::replay::run(
            &Config::default(),
            &dir,
            ThreadCounts { network: 1, cpu },
            tour_setup(hashes, seen_tx),
            &specs(),
            &log,
        );
        let replay = result.unwrap();
        assert_eq!(replay.base_tick(), 1, "from the start, {cpu} threads");
        assert_eq!(replay.end(), &End::Completed);
        assert_eq!(replay.ticks(), TOUR_TICKS);
        assert_eq!(drain(&mut sent), live, "{cpu} threads");
        let mut replayed = Vec::new();
        while let Ok(s) = seen_rx.try_recv() {
            replayed.push(s);
        }
        assert_eq!(replayed, seen, "{cpu} threads");
    }
    remove(&dir);
}
