//! Worlds while the server runs: making, loading and unloading worlds with their events, the
//! default world, the time stop, each world's difficulty and the chunk tickets; cycles that leave
//! nothing behind, an unload the server file holds after a restart, and a script of world changes
//! that replays to the same states.

use std::cell::Cell;
use std::collections::BTreeMap;

use tokio::sync::mpsc;

use super::*;
use crate::event::{ChunkLoad, Event, Listeners, Priority};
use crate::math::Vec3;
use crate::pool::Pool;
use crate::protocol::packet::Packet as P;
use crate::save::{Request, Response};
use crate::tick::queue::{Input, InputKind, Outbound, SessionId, outbound_queue};
use crate::tick::tests::{
    answer, initialised, join, new_state, radius, received, settings, shared, tick,
};
use crate::tick::{EntryKind, InputLog, LoggedTick, Output, Setup, boundary, checkpoint, deliver};
use crate::world::{BlockPos, Flat, Lightning, Options, Ticket, Weather};

/// The far chunks a ticket of radius 1 holds in the default world.
const FAR: ChunkPos = ChunkPos::new(30, -30);

thread_local! {
    /// What the listeners of a test saw, in order.
    static LOG: Cell<Vec<String>> = const { Cell::new(Vec::new()) };
    /// Whether the unload listener cancels.
    static CANCEL: Cell<bool> = const { Cell::new(false) };
}

fn log(line: String) {
    let mut lines = LOG.take();
    lines.push(line);
    LOG.set(lines);
}

fn logged() -> Vec<String> {
    LOG.take()
}

fn on<E: Event>(listener: impl FnMut(&mut E, &mut Context<'_>) + Send + 'static) -> Listeners {
    let mut l = Listeners::default();
    l.push(Priority::Normal, listener);
    l
}

/// Listeners that log every world event, cancel unloads while [`CANCEL`] says so, and a monitor
/// that logs whether an unloaded world is still loaded.
fn world_listeners() -> Listeners {
    let mut l = on(|e: &mut WorldInit, _: &mut Context<'_>| {
        log(format!("init {}", e.world().index()));
    });
    l.push(
        Priority::Normal,
        |e: &mut WorldLoad, _: &mut Context<'_>| {
            log(format!("load {}", e.world().index()));
        },
    );
    l.push(
        Priority::Normal,
        |e: &mut WorldUnload, _: &mut Context<'_>| {
            log(format!("unload {}", e.world().index()));
            if CANCEL.get() {
                e.cancel();
            }
        },
    );
    l.push_monitor(|e: &WorldUnload, cx: &Context<'_>| {
        log(format!(
            "monitor {} {}",
            e.is_cancelled(),
            cx.world(e.world()).is_some()
        ));
    });
    l
}

/// A server with the flat default world, `listeners` (sorted), and session 1 spawned at its spawn
/// with a view of radius 2.
fn server(listeners: Listeners) -> (State, Output, mpsc::Receiver<Outbound>) {
    LOG.set(Vec::new());
    CANCEL.set(false);
    let mut state = new_state(settings(|_| {}), listeners);
    let mut output = Output::default();
    let (j, rx) = join(1, 0);
    tick(&mut state, &mut vec![j], &mut output);
    tick(&mut state, &mut vec![radius(1, 1, 2)], &mut output);
    let runtime_id = state.players[&SessionId::new(1)].runtime_id;
    tick(
        &mut state,
        &mut vec![initialised(1, 2, runtime_id)],
        &mut output,
    );
    tick(&mut state, &mut Vec::new(), &mut output);
    (state, output, rx)
}

fn cx(state: &mut State) -> Context<'_> {
    Context { state }
}

/// A temporary flat world's options with `seed`.
fn arena(seed: i64) -> Options {
    Options::new().with_kind(Kind::Temporary).with_seed(seed)
}

// pmmp: WorldManager::generateWorld, WorldInitEvent, WorldLoadEvent, WorldManager::isWorldGenerated, WorldManager::isWorldLoaded, World::isLoaded, WorldCreationOptions, WorldCreationOptions::create, WorldCreationOptions::getSeed, WorldCreationOptions::setSeed, WorldCreationOptions::getSpawnPosition, WorldCreationOptions::setSpawnPosition, WorldCreationOptions::getGeneratorClass, WorldCreationOptions::setGeneratorClass, WorldCreationOptions::getDifficulty, WorldCreationOptions::setDifficulty, GeneratorManager::addGenerator
#[test]
fn a_made_world_raises_init_then_load_and_loads_its_spawn_chunks() {
    let (mut state, mut output, _rx) = server(world_listeners());
    let spawn = BlockPos::new(40, -60, 40);
    let options = arena(9)
        .with_spawn(spawn)
        .with_generator("void")
        .with_difficulty(Difficulty::Hard);
    assert_eq!(
        (options.seed(), options.spawn(), options.generator()),
        (Some(9), Some(spawn), "void")
    );
    assert_eq!(options.difficulty(), Some(Difficulty::Hard));
    let mut c = cx(&mut state);
    let id = c.create_world("arena", &options).unwrap();
    assert_eq!(id, world::Id::of(1));
    assert!(c.is_world_generated("arena") && c.is_world_loaded("arena"));
    let w = c.world(id).unwrap();
    assert_eq!((w.seed(), w.spawn(), w.kind()), (9, spawn, Kind::Temporary));
    assert_eq!(w.difficulty(), Some(Difficulty::Hard));
    assert_eq!(c.world_named("arena").unwrap().id(), id);
    assert!(logged().is_empty(), "the events come in the view phase");
    tick(&mut state, &mut Vec::new(), &mut output);
    assert_eq!(logged(), ["init 1", "load 1"]);
    tick(&mut state, &mut Vec::new(), &mut output);
    assert!(state.worlds[1].is_spawn_chunk(spawn.chunk()));
    assert!(
        state.worlds[1].is_chunk_loaded(spawn.chunk()),
        "loads like any chunk"
    );
    let mut c = cx(&mut state);
    assert_eq!(
        c.create_world("arena", &arena(1)),
        Err(world::Error::WorldExists)
    );
    assert_eq!(
        c.create_world("", &arena(1)),
        Err(world::Error::InvalidName)
    );
    assert_eq!(
        c.create_world(&"n".repeat(257), &arena(1)),
        Err(world::Error::InvalidName)
    );
    assert_eq!(
        c.create_world("x", &arena(1).with_generator("normal")),
        Err(world::Error::UnknownGenerator)
    );
    // A player at the spawn would stand outside the world border.
    let past = BlockPos::new(world::BORDER, -60, 0);
    assert_eq!(
        c.create_world("x", &arena(1).with_spawn(past)),
        Err(world::Error::OutOfRange)
    );
    assert_eq!(
        c.load_world("nowhere", &Options::new()),
        Err(world::Error::NotGenerated)
    );
    assert!(!c.is_world_generated("nowhere") && !c.is_world_loaded("nowhere"));
    assert!(!c.is_world_generated("x"), "nothing made");
}

fn ticks(state: &mut State, output: &mut Output, n: usize) {
    for _ in 0..n {
        tick(state, &mut Vec::new(), output);
    }
}

/// The eye position on world `w`'s spawn block.
fn on_spawn(state: &State, w: WorldIndex) -> Vec3 {
    let spawn = state.worlds[w].spawn();
    Vec3::new(
        f64::from(spawn.x) + 0.5,
        f64::from(spawn.y),
        f64::from(spawn.z) + 0.5,
    )
}

// pmmp: WorldManager::unloadWorld, WorldUnloadEvent, WorldUnloadEvent::cancel, WorldUnloadEvent::uncancel, WorldUnloadEvent::isCancelled, World::addOnUnloadCallback, World::removeOnUnloadCallback, WorldManager::getDefaultWorld, WorldManager::setDefaultWorld, WorldManager::getWorldByName
#[test]
fn an_unload_moves_the_players_to_the_default_world_and_a_cancel_keeps_the_world() {
    let (mut state, mut output, mut rx) = server(world_listeners());
    let id = cx(&mut state).create_world("arena", &arena(3)).unwrap();
    ticks(&mut state, &mut output, 2);
    logged();
    let eye = on_spawn(&state, 1);
    let mut c = cx(&mut state);
    let player = crate::player::Id::new(SessionId::new(1));
    c.teleport(player, id, eye).unwrap();
    ticks(&mut state, &mut output, 2);
    assert_eq!(state.players[&SessionId::new(1)].world, 1);
    received(&mut rx);

    // Cancelled, nothing happens and the monitors see nothing.
    CANCEL.set(true);
    assert_eq!(cx(&mut state).unload_world(id), Ok(()));
    tick(&mut state, &mut Vec::new(), &mut output);
    assert_eq!(logged(), ["unload 1"]);
    assert!(cx(&mut state).world(id).is_some());
    assert_eq!(
        state.players[&SessionId::new(1)].world,
        1,
        "a cancelled unload"
    );

    // Forced, the listeners are asked all the same, their cancel is ignored, and the monitors see
    // the unload not cancelled.
    assert_eq!(cx(&mut state).force_unload_world(id), Ok(()));
    tick(&mut state, &mut Vec::new(), &mut output);
    assert_eq!(logged(), ["unload 1", "monitor false false"]);
    let c = cx(&mut state);
    assert!(c.world(id).is_none() && c.world_named("arena").is_none());
    assert!(!c.is_world_loaded("arena") && !c.is_world_generated("arena"));
    let p = &state.players[&SessionId::new(1)];
    assert_eq!(p.world, 0, "moved to the default world");
    let spawn = state.worlds[0].spawn();
    assert_eq!(
        crate::world::BlockPos::containing(p.position).chunk(),
        spawn.chunk()
    );
    assert_eq!(state.worlds[1].loaded_chunks().len(), 0);
    assert!(
        received(&mut rx)
            .0
            .iter()
            .any(|p| matches!(p, P::MovePlayer(_)))
    );
    // A temporary world unloaded is gone; one made again takes its place.
    let mut c = cx(&mut state);
    assert_eq!(
        c.load_world("arena", &Options::new()),
        Err(world::Error::NotGenerated)
    );
    assert_eq!(c.unload_world(id), Err(world::Error::WorldNotLoaded));
    assert_eq!(
        c.unload_world(world::Id::DEFAULT),
        Err(world::Error::DefaultWorld)
    );
    assert_eq!(c.create_world("arena", &arena(4)), Ok(id));
    // The default world moves; the settings' world may unload then.
    CANCEL.set(false);
    assert_eq!(c.set_default_world(id), Ok(()));
    assert_eq!(c.default_world().id(), id);
    assert_eq!(c.unload_world(id), Err(world::Error::DefaultWorld));
    assert_eq!(c.unload_world(world::Id::DEFAULT), Ok(()));
    tick(&mut state, &mut Vec::new(), &mut output);
    assert_eq!(state.players[&SessionId::new(1)].world, 1);
    assert!(!state.catalog.is_loaded(0));
    let mut c = cx(&mut state);
    assert_eq!(
        c.set_default_world(world::Id::DEFAULT),
        Err(world::Error::WorldNotLoaded)
    );
    assert_eq!(
        c.load_world("world", &Options::new()),
        Ok(world::Id::DEFAULT)
    );
    assert!(c.world(world::Id::DEFAULT).is_some(), "loaded in its place");
}

// pmmp: Player::teleport
/// A teleport into a world that is not loaded fails at once, and one an unload listener marks
/// into the unloading world sends the player where the world's players go: no teleport that
/// succeeded is dropped.
#[test]
fn a_teleport_into_an_unloaded_world_fails_and_a_waiting_one_follows_the_unload() {
    let player = crate::player::Id::new(SessionId::new(1));
    let (mut state, mut output, _rx) = server(on(move |e: &mut WorldUnload, cx| {
        cx.teleport(player, e.world(), Vec3::new(0.5, -60.0, 0.5))
            .unwrap();
    }));
    let id = cx(&mut state).create_world("arena", &arena(3)).unwrap();
    ticks(&mut state, &mut output, 2);
    let aside = on_spawn(&state, 0) + Vec3::new(3.0, 0.0, 3.0);
    cx(&mut state)
        .teleport(player, world::Id::DEFAULT, aside)
        .unwrap();
    ticks(&mut state, &mut output, 2);
    assert_eq!(cx(&mut state).unload_world(id), Ok(()));
    tick(&mut state, &mut Vec::new(), &mut output);
    let home = state.worlds[0].safe_spawn(state.worlds[0].spawn());
    let mut c = cx(&mut state);
    assert_eq!(
        c.teleport(player, id, Vec3::new(0.5, -60.0, 0.5)),
        Err(crate::player::Error::WorldNotLoaded)
    );
    let p = c.player(player).unwrap();
    assert_eq!(p.world(), world::Id::DEFAULT);
    assert_eq!(BlockPos::containing(p.position()), home);
    tick(&mut state, &mut Vec::new(), &mut output);
    let p = &state.players[&SessionId::new(1)];
    assert_eq!((p.world, BlockPos::containing(p.position())), (0, home));
}

/// A listener that makes the unloading world the default one keeps it, even from a forced
/// unload: the default world never unloads, so the monitors see no unload.
#[test]
fn a_listener_that_makes_the_world_the_default_one_keeps_it() {
    let mut listeners = world_listeners();
    listeners.push(
        Priority::Normal,
        |e: &mut WorldUnload, cx: &mut Context<'_>| {
            cx.set_default_world(e.world()).unwrap();
        },
    );
    let (mut state, mut output, _rx) = server(listeners);
    let id = cx(&mut state).create_world("arena", &arena(3)).unwrap();
    ticks(&mut state, &mut output, 2);
    logged();
    assert_eq!(cx(&mut state).force_unload_world(id), Ok(()));
    tick(&mut state, &mut Vec::new(), &mut output);
    assert_eq!(logged(), ["unload 1"]);
    let c = cx(&mut state);
    assert!(c.world(id).is_some());
    assert_eq!(c.default_world().id(), id);
}

// pmmp: World::stopTime, World::startTime
#[test]
fn a_stopped_time_stands_still_for_the_world_and_its_players() {
    let (mut state, mut output, mut rx) = server(Listeners::default());
    received(&mut rx);
    let before = state.worlds[0].time_ticks();
    let mut c = cx(&mut state);
    assert!(c.stop_time(world::Id::DEFAULT));
    assert!(c.default_world().is_time_stopped());
    assert!(!c.stop_time(world::Id::of(5)), "no such world");
    ticks(&mut state, &mut output, 30);
    assert_eq!(state.worlds[0].time_ticks(), before);
    let paused = |rx: &mut mpsc::Receiver<Outbound>| -> Vec<bool> {
        received(rx)
            .0
            .into_iter()
            .filter_map(|p| match p {
                P::SyncWorldClocks(c) => c.sync_states.first().map(|s| s.paused),
                _ => None,
            })
            .collect()
    };
    assert_eq!(paused(&mut rx), [true], "the players' clocks pause at once");
    assert!(cx(&mut state).start_time(world::Id::DEFAULT));
    ticks(&mut state, &mut output, 3);
    assert_eq!(paused(&mut rx), [false]);
    assert_eq!(state.worlds[0].time_ticks(), before + 3);
}

/// Runs ticks until every world's light work is done (at most 1,000).
fn settle(state: &mut State, output: &mut Output) {
    let mut ran = 0;
    while !state.worlds.iter().all(World::is_light_idle) {
        assert!(ran < 1000, "the light work never ends");
        tick(state, &mut Vec::new(), output);
        ran += 1;
    }
}

/// A world made with its own view values (the `[worlds.<name>]` keys) uses them; the others keep
/// the global ones, and values out of their ranges make no world.
#[test]
fn a_made_world_uses_the_view_values_of_its_options() {
    let (mut state, mut output, _rx) = server(Listeners::default());
    let own = crate::config::WorldSettings {
        view_distance: Some(3),
        tick_radius: Some(6),
        ..crate::config::WorldSettings::default()
    };
    let w = cx(&mut state)
        .create_world("small", &arena(1).with_settings(own))
        .unwrap()
        .index();
    let plain = cx(&mut state)
        .create_world("plain", &arena(2))
        .unwrap()
        .index();
    let limits = state.settings.view(w);
    assert_eq!((limits.view_distance, limits.tick_radius), (3, 3));
    assert_eq!(state.settings.view(plain), state.settings.view(0));
    let far = crate::config::WorldSettings {
        view_distance: Some(65),
        ..crate::config::WorldSettings::default()
    };
    assert_eq!(
        cx(&mut state).create_world("far", &arena(3).with_settings(far)),
        Err(world::Error::InvalidSettings)
    );
    settle(&mut state, &mut output);
    let player = crate::player::Id::new(SessionId::new(1));
    assert_eq!(cx(&mut state).set_view_distance(player, 8), Some(8));
    let eye = on_spawn(&state, w);
    survival::teleport(&mut state, SessionId::new(1), w, eye);
    assert_eq!(state.players[&SessionId::new(1)].chunk_radius, Some(3));
    assert_eq!(arena(1).with_settings(own).settings(), own);
}

// pmmp: World::getDifficulty, World::setDifficulty
#[test]
fn a_world_follows_the_server_difficulty_until_it_has_its_own() {
    let (mut state, mut output, mut rx) = server(Listeners::default());
    let server_difficulty = state.settings.difficulty;
    assert_eq!(state.worlds[0].difficulty(), None);
    assert_eq!(difficulty(&state, 0), server_difficulty);
    received(&mut rx);
    let mut c = cx(&mut state);
    assert!(c.set_world_difficulty(world::Id::DEFAULT, Some(Difficulty::Peaceful)));
    assert!(!c.set_world_difficulty(world::Id::of(3), Some(Difficulty::Hard)));
    assert_eq!(c.default_world().difficulty(), Some(Difficulty::Peaceful));
    assert_eq!(difficulty(&state, 0), Difficulty::Peaceful);
    tick(&mut state, &mut Vec::new(), &mut output);
    let sent: Vec<i32> = received(&mut rx)
        .0
        .iter()
        .filter_map(|p| match p {
            P::SetDifficulty(d) => Some(d.difficulty.cast_signed()),
            _ => None,
        })
        .collect();
    assert_eq!(sent, [0]);
    // Peaceful feeds a hungry player of this world, whatever the server's difficulty.
    let player = crate::player::Id::new(SessionId::new(1));
    cx(&mut state)
        .player_mut(player)
        .unwrap()
        .set_food(10.0)
        .unwrap();
    ticks(&mut state, &mut output, 25);
    assert!(cx(&mut state).player(player).unwrap().food() > 10.0);
    // The server's difficulty set again replaces the worlds' own (PMMP `/difficulty`).
    assert!(cx(&mut state).set_difficulty(Difficulty::Hard));
    assert_eq!(state.worlds[0].difficulty(), None);
    assert_eq!(difficulty(&state, 0), Difficulty::Hard);
    let mut hardcore = new_state(
        settings(|c| c.general.hardcore = true),
        Listeners::default(),
    );
    assert!(!cx(&mut hardcore).set_world_difficulty(world::Id::DEFAULT, Some(Difficulty::Easy)));
    assert!(cx(&mut hardcore).set_world_difficulty(world::Id::DEFAULT, Some(Difficulty::Hard)));
}

// pmmp: World::registerChunkLoader, World::unregisterChunkLoader, World::getChunkLoaders, World::isChunkInUse, World::loadChunk, World::requestChunkPopulation, World::orderChunkPopulation, ChunkLoader
#[test]
fn a_ticket_keeps_its_chunks_loaded_without_players_until_it_is_released() {
    let loads = on(|e: &mut ChunkLoad, _: &mut Context<'_>| {
        log(format!("load {} {}", e.chunk().x, e.chunk().z));
    });
    let (mut state, mut output, _rx) = server(loads);
    logged();
    let mut c = cx(&mut state);
    let ticket = c.hold_chunks(world::Id::DEFAULT, FAR, 1).unwrap();
    let held: Vec<Ticket> = c.chunk_tickets(world::Id::DEFAULT, FAR).collect();
    assert_eq!(held, [ticket]);
    assert_eq!(
        c.chunk_tickets(world::Id::DEFAULT, ChunkPos::new(32, -30))
            .count(),
        0
    );
    assert!(
        !c.default_world().is_chunk_loaded(FAR),
        "loads in the view phase"
    );
    ticks(&mut state, &mut output, 2);
    assert_eq!(logged().len(), 9, "the 3 × 3 chunks load once");
    // No player is near; they stay.
    ticks(&mut state, &mut output, 50);
    let around = |state: &State| {
        (-1..=1)
            .flat_map(|x| (-1..=1).map(move |z| ChunkPos::new(FAR.x + x, FAR.z + z)))
            .filter(|&p| state.worlds[0].is_chunk_loaded(p))
            .count()
    };
    assert_eq!(around(&state), 9);
    assert!(cx(&mut state).release_chunks(ticket));
    assert!(!cx(&mut state).release_chunks(ticket), "released once");
    tick(&mut state, &mut Vec::new(), &mut output);
    assert_eq!(around(&state), 0);
    let mut c = cx(&mut state);
    assert_eq!(
        c.hold_chunks(world::Id::DEFAULT, FAR, 9),
        Err(world::Error::Full)
    );
    assert_eq!(
        c.hold_chunks(world::Id::of(4), FAR, 1),
        Err(world::Error::WorldNotLoaded)
    );
    assert_eq!(
        c.hold_chunks(world::Id::DEFAULT, ChunkPos::new(2_000_000, 0), 0),
        Err(world::Error::OutOfRange)
    );
    // The far ends of a public `ChunkPos` are refused too, holding nothing.
    let held = c.state.chunks.held();
    for center in [ChunkPos::new(i32::MIN, 0), ChunkPos::new(0, i32::MIN)] {
        assert_eq!(
            c.hold_chunks(world::Id::DEFAULT, center, 0),
            Err(world::Error::OutOfRange)
        );
    }
    assert_eq!(c.state.chunks.held(), held);
    // 14 tickets of 17 × 17 hold 4,046 chunks: no room for another such one.
    for i in 0..14 {
        let _ticket = c
            .hold_chunks(world::Id::DEFAULT, ChunkPos::new(100 * i, 0), 8)
            .unwrap();
    }
    assert_eq!(
        c.hold_chunks(world::Id::DEFAULT, FAR, 8),
        Err(world::Error::Full),
        "at most 4,096 chunks are held"
    );
    // A world that unloads drops its tickets.
    let id = c.create_world("arena", &arena(1)).unwrap();
    let ticket = c.hold_chunks(id, FAR, 0).unwrap();
    c.unload_world(id).unwrap();
    tick(&mut state, &mut Vec::new(), &mut output);
    assert!(!cx(&mut state).release_chunks(ticket));
}

/// The tickets of each chunk in `chunks` of the default world.
fn tickets_at(state: &mut State, chunks: &[ChunkPos]) -> Vec<Vec<Ticket>> {
    let c = cx(state);
    chunks
        .iter()
        .map(|&p| c.chunk_tickets(world::Id::DEFAULT, p).collect())
        .collect()
}

/// A state like [`server`]'s world list as a replay makes it from `state`'s checkpoint.
fn fresh_like(state: &State) -> State {
    let worlds = state
        .worlds
        .iter()
        .map(|w| {
            let mut fresh = World::new(w.name(), w.kind(), w.seed(), w.spawn());
            fresh.set_generator(w.generator());
            fresh.set_number(w.number());
            fresh
        })
        .collect();
    State::new(
        crate::tick::Settings::new(&crate::config::Config::default()),
        shared(),
        Setup::default(),
        worlds,
        0,
    )
}

#[test]
fn chunk_tickets_follow_holds_releases_restores_and_unloads() {
    let (mut state, mut output, _rx) = server(Listeners::default());
    let at = |dx: i32| ChunkPos::new(FAR.x + dx, FAR.z);
    let probes = [at(-3), at(-2), at(2), at(4), at(5)];
    let mut c = cx(&mut state);
    let a = c.hold_chunks(world::Id::DEFAULT, FAR, 2).unwrap();
    let b = c.hold_chunks(world::Id::DEFAULT, at(3), 1).unwrap();
    let d = c.hold_chunks(world::Id::DEFAULT, at(2), 0).unwrap();
    // In the order they were given, wherever they overlap.
    let want = vec![Vec::new(), vec![a], vec![a, b, d], vec![b], Vec::new()];
    assert_eq!(tickets_at(&mut state, &probes), want);
    ticks(&mut state, &mut output, 2);
    // A replay from a checkpoint finds the same tickets and the same room left.
    let pool = crate::runtime::cpu_pool(1).unwrap();
    let bytes = state.checkpoint(&pool).to_bytes();
    let mut restored = fresh_like(&state);
    let custom = std::sync::Arc::clone(&restored.custom);
    let entities = restored.rules.entities.take_customs();
    let read = checkpoint::Checkpoint::read(&bytes, &custom, &entities).unwrap();
    restored.rules.entities.set_customs(entities);
    restored
        .restore(&read, checkpoint::tests::extras_of(&state), |_| {
            outbound_queue().0
        })
        .unwrap();
    assert_eq!(tickets_at(&mut restored, &probes), want);
    let mut big = Vec::new();
    for s in [&mut state, &mut restored] {
        // 25 + 9 + 1 chunks held: 14 more tickets of 17 × 17 fit, and no 15th.
        let mut c = cx(s);
        big = (0..14)
            .map(|i| {
                c.hold_chunks(world::Id::DEFAULT, ChunkPos::new(100 * i, 0), 8)
                    .unwrap()
            })
            .collect();
        assert_eq!(
            c.hold_chunks(world::Id::DEFAULT, ChunkPos::new(-100, 0), 8),
            Err(world::Error::Full)
        );
    }
    assert!(pool.shutdown());
    let mut c = cx(&mut state);
    assert!(c.release_chunks(b));
    let want = vec![Vec::new(), vec![a], vec![a, d], Vec::new(), Vec::new()];
    assert_eq!(tickets_at(&mut state, &probes), want);
    // Released or dropped by their world's unload, tickets leave nothing behind.
    let mut c = cx(&mut state);
    for t in big.into_iter().chain([a, d]) {
        assert!(c.release_chunks(t));
    }
    let id = c.create_world("arena", &arena(1)).unwrap();
    let _first = c.hold_chunks(id, FAR, 3).unwrap();
    let _second = c.hold_chunks(id, FAR, 1).unwrap();
    c.unload_world(id).unwrap();
    ticks(&mut state, &mut output, 4);
    settle(&mut state, &mut output);
    assert_eq!(tickets_at(&mut state, &probes), vec![Vec::new(); 5]);
    assert!(state.chunks.is_idle());
}

// pmmp: WorldManager::generateWorld, WorldManager::loadWorld, World::getDifficulty
#[test]
fn a_hardcore_server_plays_every_world_on_hard() {
    let hardcore = settings(|c| {
        c.general.hardcore = true;
        c.general.difficulty = Difficulty::Hard;
    });
    let mut state = new_state(hardcore, Listeners::default());
    let mut output = Output::default();
    let calm = cx(&mut state)
        .create_world("calm", &arena(1).with_difficulty(Difficulty::Peaceful))
        .unwrap();
    state.catalog.stored.insert(
        "old".to_owned(),
        (
            "old".to_owned(),
            WorldRecord {
                number: 7,
                time: 0,
                spawn: BlockPos::new(0, -60, 0),
            },
        ),
    );
    let old = cx(&mut state)
        .load_world("old", &Options::new().with_difficulty(Difficulty::Easy))
        .unwrap();
    for id in [calm, old] {
        assert_eq!(difficulty(&state, id.index()), Difficulty::Hard);
    }
    assert_eq!(
        state.worlds[calm.index()].difficulty(),
        Some(Difficulty::Peaceful)
    );
    assert_eq!(
        state.worlds[old.index()].difficulty(),
        Some(Difficulty::Easy)
    );
    ticks(&mut state, &mut output, 4);
    // A player who joins either is told hard by `StartGame` and the spawn window.
    for (session, id) in [(1, calm), (2, old)] {
        cx(&mut state).set_default_world(id).unwrap();
        let (j, mut rx) = join(session, 0);
        tick(&mut state, &mut vec![j], &mut output);
        tick(&mut state, &mut vec![radius(session, 1, 2)], &mut output);
        let packets = received(&mut rx).0;
        let start = packets.iter().find_map(|p| match p {
            P::StartGame(s) => Some(s.difficulty),
            _ => None,
        });
        assert_eq!(start, Some(3), "world {}", id.index());
        let set: Vec<u32> = packets
            .iter()
            .filter_map(|p| match p {
                P::SetDifficulty(d) => Some(d.difficulty),
                _ => None,
            })
            .collect();
        assert_eq!(set, [3], "world {}", id.index());
    }
}

// pmmp: WorldManager::unloadWorld, WorldManager::loadWorld
#[test]
fn a_load_after_an_unload_in_the_same_call_keeps_the_world() {
    let (mut state, mut output, _rx) = server(world_listeners());
    let lobby = cx(&mut state)
        .create_world("lobby", &Options::new().with_seed(2))
        .unwrap();
    ticks(&mut state, &mut output, 2);
    logged();
    let mut c = cx(&mut state);
    c.unload_world(lobby).unwrap();
    assert_eq!(c.load_world("lobby", &Options::new()), Ok(lobby));
    ticks(&mut state, &mut output, 2);
    assert!(state.catalog.is_loaded(lobby.index()));
    assert!(cx(&mut state).world(lobby).is_some());
    assert_eq!(logged(), Vec::<String>::new(), "the unload was dropped");
}

/// A world whose spawn moved to another chunk in the tick of its unload holds nothing after the
/// unload: the view phase holds no spawn chunks of a world that is not loaded.
#[test]
fn a_spawn_moved_in_the_tick_of_its_worlds_unload_holds_nothing() {
    let (mut state, mut output, _rx) = server(Listeners::default());
    let base = state.chunks.held();
    let id = cx(&mut state).create_world("arena", &arena(1)).unwrap();
    ticks(&mut state, &mut output, 3);
    let mut c = cx(&mut state);
    c.world_mut(id)
        .unwrap()
        .set_spawn(BlockPos::new(800, -60, 800));
    c.unload_world(id).unwrap();
    ticks(&mut state, &mut output, 4);
    assert_eq!(state.worlds[1].loaded_chunks().len(), 0);
    assert_eq!(state.chunks.held(), base);
}

/// A server with the flat default world whose session 1 has had its start-game packets and its
/// spawn window and still waits for its spawn chunks (one is sent a tick), its queue emptied.
fn in_spawn_window() -> (State, Output, mpsc::Receiver<Outbound>) {
    let mut state = new_state(
        settings(|c| c.developer.chunks_per_tick = 1),
        Listeners::default(),
    );
    let mut output = Output::default();
    let (j, mut rx) = join(1, 0);
    tick(&mut state, &mut vec![j], &mut output);
    tick(&mut state, &mut vec![radius(1, 1, 2)], &mut output);
    let window = received(&mut rx).0;
    assert!(window.iter().any(|p| matches!(p, P::SetDifficulty(_))));
    assert_eq!(state.players[&SessionId::new(1)].phase, Phase::Joined);
    (state, output, rx)
}

// pmmp: World::stopTime, World::setTime, World::setDifficulty
#[test]
fn a_player_waiting_for_its_spawn_chunks_hears_of_time_and_difficulty_changes() {
    let (mut state, mut output, mut rx) = in_spawn_window();
    let mut c = cx(&mut state);
    assert!(c.stop_time(world::Id::DEFAULT));
    assert!(c.set_time(world::Id::DEFAULT, 1000));
    assert!(c.set_world_difficulty(world::Id::DEFAULT, Some(Difficulty::Easy)));
    assert!(c.set_difficulty(Difficulty::Hard));
    tick(&mut state, &mut Vec::new(), &mut output);
    assert_eq!(state.players[&SessionId::new(1)].phase, Phase::Joined);
    let packets = received(&mut rx).0;
    let clocks: Vec<(i32, bool)> = packets
        .iter()
        .filter_map(|p| match p {
            P::SyncWorldClocks(c) => c.sync_states.first().map(|s| (s.time, s.paused)),
            _ => None,
        })
        .collect();
    assert_eq!(clocks.first().map(|c| c.1), Some(true), "{clocks:?}");
    assert!(clocks.contains(&(1000, true)), "{clocks:?}");
    let set: Vec<u32> = packets
        .iter()
        .filter_map(|p| match p {
            P::SetDifficulty(d) => Some(d.difficulty),
            _ => None,
        })
        .collect();
    assert_eq!(set, [1, 3]);
}

#[test]
fn a_player_waiting_for_its_spawn_chunks_hears_of_a_random_tick_speed_change() {
    let (mut state, mut output, mut rx) = in_spawn_window();
    assert!(cx(&mut state).set_random_tick_speed(100));
    tick(&mut state, &mut Vec::new(), &mut output);
    assert_eq!(state.players[&SessionId::new(1)].phase, Phase::Joined);
    let speeds: Vec<_> = received(&mut rx)
        .0
        .into_iter()
        .filter_map(|p| match p {
            P::GameRulesChanged(c) => c
                .game_rules
                .into_iter()
                .find(|r| r.name == "randomTickSpeed")
                .map(|r| r.value),
            _ => None,
        })
        .collect();
    assert_eq!(speeds, [crate::protocol::GameRuleValue::Uint32(100)]);
}

/// Every table that world, chunk and rule state live in, by size.
fn tables(state: &State) -> Vec<usize> {
    let mut sizes = vec![
        state.chunks.held(),
        usize::from(state.chunks.is_idle()),
        state.chunks.extras.len(),
        state.rules.updates.len().0,
        state.catalog.pending.len(),
        state.catalog.announced.len(),
        state.catalog.unloads.len(),
        state.worlds.len(),
    ];
    let (a, b, c, d) = state.rules.block_entities.sizes();
    sizes.extend([a, b, c, d]);
    for w in &state.worlds {
        sizes.extend([
            w.loaded_chunks().len(),
            w.quarantined().len(),
            w.changed_len(),
            usize::from(w.is_light_idle()),
        ]);
    }
    sizes
}

fn stone() -> crate::block::State {
    crate::block::State::find("minecraft:stone", &[]).unwrap()
}

#[test]
fn make_hold_and_unload_cycles_return_every_table_to_its_start() {
    let (mut state, mut output, _rx) = server(world_listeners());
    // The places a temporary and a persistent world take, made once.
    let id = cx(&mut state).create_world("arena", &arena(1)).unwrap();
    let lobby = cx(&mut state)
        .create_world("lobby", &Options::new().with_seed(2))
        .unwrap();
    let player = crate::player::Id::new(SessionId::new(1));
    let home = on_spawn(&state, 0);
    for round in 0..12 {
        let mut c = cx(&mut state);
        c.unload_world(id).unwrap();
        c.unload_world(lobby).unwrap();
        ticks(&mut state, &mut output, 3);
        settle(&mut state, &mut output);
        logged();
        let start = tables(&state);
        let mut c = cx(&mut state);
        assert_eq!(c.create_world("arena", &arena(round)), Ok(id));
        assert_eq!(c.load_world("lobby", &Options::new()), Ok(lobby));
        let ticket = c.hold_chunks(lobby, FAR, 1).unwrap();
        ticks(&mut state, &mut output, 2);
        let eye = on_spawn(&state, 1);
        let mut c = cx(&mut state);
        c.teleport(player, id, eye).unwrap();
        c.world_mut(lobby)
            .unwrap()
            .set_block(BlockPos::new(30 * 16, -60, -30 * 16), stone())
            .unwrap();
        ticks(&mut state, &mut output, 3);
        let mut c = cx(&mut state);
        c.teleport(player, world::Id::DEFAULT, home).unwrap();
        if round % 2 == 0 {
            c.release_chunks(ticket);
        }
        c.unload_world(id).unwrap();
        c.unload_world(lobby).unwrap();
        // The commit the unload asked for lets the lobby's changes go.
        ticks(&mut state, &mut output, 4);
        settle(&mut state, &mut output);
        assert_eq!(tables(&state), start, "round {round}");
        assert!(state.saving.is_idle());
        assert_eq!(
            logged(),
            [
                "init 1",
                "load 1",
                "load 2",
                "unload 1",
                "monitor false false",
                "unload 2",
                "monitor false false"
            ]
        );
        cx(&mut state).create_world("arena", &arena(round)).unwrap();
        cx(&mut state).load_world("lobby", &Options::new()).unwrap();
        ticks(&mut state, &mut output, 1);
    }
}

#[test]
fn a_load_in_flight_when_its_world_unloads_is_dropped_on_arrival() {
    let (mut state, mut output, _rx) = server(Listeners::default());
    let id = cx(&mut state).create_world("arena", &arena(1)).unwrap();
    let spawn = state.worlds[1].spawn().chunk();
    // The view phase asks for the spawn chunks; the unload comes before the answers.
    let pool = crate::runtime::cpu_pool(1).unwrap();
    let mut log = InputLog::default();
    let mut answers = Vec::new();
    crate::tick::step(
        &mut state,
        &mut Vec::new(),
        &mut answers,
        &pool,
        &mut output,
        &mut log,
    );
    let requests = std::mem::take(&mut output.requests);
    let late = answer(&state, requests);
    assert!(
        late.iter()
            .any(|a| matches!(a, Response::Chunk { world: 1, .. }))
    );
    cx(&mut state).unload_world(id).unwrap();
    // Made again with another seed before the old answers come: they are dropped.
    tick(&mut state, &mut Vec::new(), &mut output);
    cx(&mut state).create_world("arena", &arena(77)).unwrap();
    output.answers = late;
    tick(&mut state, &mut Vec::new(), &mut output);
    assert!(!state.worlds[1].is_chunk_loaded(spawn), "the stale answer");
    assert!(state.chunks.stale.is_empty());
    ticks(&mut state, &mut output, 2);
    assert!(
        state.worlds[1].is_chunk_loaded(spawn),
        "asked again and loaded"
    );
    assert!(pool.shutdown());
}

/// A server on the server file of `dir` with `listeners` (sorted) and the real save thread: the
/// state, the save link and the save thread.
fn open_server(
    dir: &crate::save::tests::TempDir,
    listeners: Listeners,
) -> (
    State,
    crate::tick::SaveLink,
    std::thread::JoinHandle<Result<(), crate::storage::Error>>,
) {
    let specs = vec![crate::save::WorldSpec {
        name: "world".to_owned(),
        kind: Kind::Persistent,
        generator: Arc::new(Flat),
        seed: Some(0),
        spawn: None,
    }];
    let opened = crate::save::open(&dir.0, specs).unwrap();
    let mut state = State::new(
        settings(|_| {}),
        shared(),
        Setup {
            listeners,
            ..Setup::default()
        },
        opened.worlds,
        opened.tick,
    );
    state.open_worlds(opened.catalog);
    let (requests, queue) = mpsc::channel(crate::save::REQUEST_CAPACITY);
    let (answers, responses) = mpsc::channel(crate::save::RESPONSE_CAPACITY);
    let thread = opened.thread;
    let handle = crate::runtime::spawn_save(move || {
        let pool = crate::runtime::cpu_pool(1).unwrap();
        let result = crate::save::run(thread, &pool, queue, &answers);
        assert!(pool.shutdown());
        result
    })
    .unwrap();
    let link = crate::tick::SaveLink {
        requests,
        responses,
    };
    (state, link, handle)
}

/// Runs `n` ticks against the real save thread, waiting after each for the answers to its
/// requests, which the next tick applies.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test waits for the save thread's answers between ticks"
)]
fn run_saved(state: &mut State, link: &mut crate::tick::SaveLink, pool: &Pool, n: usize) {
    let mut answers = Vec::new();
    for _ in 0..n {
        let mut output = Output::default();
        let mut log = InputLog::default();
        crate::tick::step(
            state,
            &mut Vec::new(),
            &mut answers,
            pool,
            &mut output,
            &mut log,
        );
        deliver(state, &mut output, &mut log);
        boundary(state, &mut output, &mut log);
        let mut expected = 0;
        for request in std::mem::take(&mut output.requests) {
            expected += match &request {
                Request::Chunks(keys) => keys.len(),
                Request::Player(_) | Request::Commit(_) => 1,
                Request::Worlds(_) | Request::DeleteWorlds(_) | Request::Finish => 0,
            };
            link.requests.blocking_send(request).unwrap();
        }
        for _ in 0..expected {
            answers.push(link.responses.blocking_recv().unwrap());
        }
    }
}

/// Ends the save thread as a stopping tick does.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test ends the save thread"
)]
fn finish(
    link: crate::tick::SaveLink,
    handle: std::thread::JoinHandle<Result<(), crate::storage::Error>>,
) {
    link.requests.blocking_send(Request::Finish).unwrap();
    drop(link);
    handle.join().unwrap().unwrap();
}

// pmmp: WorldManager::loadWorld, WorldManager::isWorldGenerated, World::stopTime, World::setDifficulty, World::getDifficulty
#[test]
fn an_unloaded_world_is_in_the_server_file_after_a_restart_and_loads_back() {
    let dir = crate::save::tests::TempDir::new("world-lifecycle");
    let pool = crate::runtime::cpu_pool(1).unwrap();
    let at = BlockPos::new(1, -60, 1);
    {
        let (mut state, mut link, handle) = open_server(&dir, Listeners::default());
        let id = cx(&mut state)
            .create_world("lobby", &Options::new().with_seed(5))
            .unwrap();
        run_saved(&mut state, &mut link, &pool, 3);
        let mut c = cx(&mut state);
        c.world_mut(id).unwrap().set_block(at, stone()).unwrap();
        assert!(c.stop_time(id));
        assert!(c.set_world_difficulty(id, Some(Difficulty::Easy)));
        assert!(c.set_weather(id, Weather::Rain));
        assert!(c.set_time(id, 4000));
        c.world_mut(id).unwrap().set_spawn(BlockPos::new(9, -60, 9));
        c.unload_world(id).unwrap();
        run_saved(&mut state, &mut link, &pool, 3);
        assert!(
            state.saving.is_idle() && state.chunks.unsaved.is_empty(),
            "committed"
        );
        finish(link, handle);
    }
    let opened = crate::save::open(&dir.0, Vec::new()).unwrap();
    let stored = &opened.catalog.stored;
    assert_eq!(stored.len(), 2, "{stored:?}");
    // The record keeps the state alone, the moved spawn among it: none of the settings the run
    // changed.
    let (_, lobby) = stored.iter().find(|(n, _)| n == "lobby").unwrap();
    assert!((4000..4010).contains(&lobby.time), "{lobby:?}");
    assert_eq!(lobby.spawn, BlockPos::new(9, -60, 9));
    drop(opened);
    let (mut state, mut link, handle) = open_server(&dir, Listeners::default());
    let mut c = cx(&mut state);
    assert!(c.is_world_generated("lobby") && !c.is_world_loaded("lobby"));
    assert_eq!(
        c.create_world("lobby", &Options::new()),
        Err(world::Error::WorldExists)
    );
    // The settings come from the load: its options, else the defaults. The options' spawn only
    // places a new world's: the stored one stays.
    let options = Options::new()
        .with_seed(6)
        .with_spawn(BlockPos::new(2, -60, 3))
        .with_difficulty(Difficulty::Hard);
    let id = c.load_world("lobby", &options).unwrap();
    run_saved(&mut state, &mut link, &pool, 3);
    let c = cx(&mut state);
    let lobby = c.world(id).unwrap();
    assert_eq!(lobby.block(at), Some(stone()), "the unload's commit");
    assert!(lobby.time_ticks() >= 4000, "the saved time goes on");
    assert_eq!((lobby.seed(), lobby.spawn()), (6, BlockPos::new(9, -60, 9)));
    assert!(!lobby.is_time_stopped());
    assert_eq!(lobby.difficulty(), Some(Difficulty::Hard));
    assert_eq!(lobby.weather(), Weather::Clear);
    finish(link, handle);
    assert!(pool.shutdown());
}

/// A world is its file: a deleted world loses its file alone and its name is free again, a name
/// that cannot be a file name or differs only in letter case from a world's is refused.
#[test]
fn a_deleted_world_loses_its_file_alone_and_names_follow_the_file_rule() {
    let dir = crate::save::tests::TempDir::new("world-delete");
    let pool = crate::runtime::cpu_pool(1).unwrap();
    let files = || crate::save::world_files(&dir.0).unwrap();
    {
        let (mut state, mut link, handle) = open_server(&dir, Listeners::default());
        let mut c = cx(&mut state);
        assert_eq!(
            c.create_world("con", &Options::new()),
            Err(world::Error::InvalidName)
        );
        assert_eq!(
            c.create_world("a/b", &Options::new()),
            Err(world::Error::InvalidName)
        );
        assert_eq!(
            c.create_world("WORLD", &Options::new()),
            Err(world::Error::WorldExists)
        );
        let lobby = c.create_world("lobby", &Options::new()).unwrap();
        c.create_world("arena", &Options::new()).unwrap();
        run_saved(&mut state, &mut link, &pool, 3);
        let mut c = cx(&mut state);
        assert_eq!(c.delete_world("lobby"), Err(world::Error::WorldLoaded));
        assert_eq!(c.delete_world("world"), Err(world::Error::WorldLoaded));
        assert_eq!(c.delete_world("nope"), Err(world::Error::NotGenerated));
        c.world_mut(lobby)
            .unwrap()
            .set_block(BlockPos::new(1, -60, 1), stone())
            .unwrap();
        c.unload_world(lobby).unwrap();
        run_saved(&mut state, &mut link, &pool, 3);
        assert_eq!(files(), ["arena", "lobby", "world"]);
        let mut c = cx(&mut state);
        c.delete_world("lobby").unwrap();
        assert!(!c.is_world_generated("lobby"));
        assert_eq!(c.delete_world("lobby"), Err(world::Error::NotGenerated));
        // The name is free at once, in any letter case. No commit holds the deletion yet, so the
        // world made again takes the deleted world's file, under that file's name.
        c.create_world("Lobby", &Options::new()).unwrap();
        run_saved(&mut state, &mut link, &pool, 3);
        finish(link, handle);
    }
    assert_eq!(files(), ["arena", "lobby", "world"]);
    // A world stored and not loaded this run is deleted as well.
    let (mut state, mut link, handle) = open_server(&dir, Listeners::default());
    let mut c = cx(&mut state);
    c.delete_world("arena").unwrap();
    // The file goes once a commit holds the deletion.
    c.save();
    run_saved(&mut state, &mut link, &pool, 1);
    finish(link, handle);
    assert_eq!(files(), ["lobby", "world"]);
    assert!(pool.shutdown());
}

/// Names that differ only in letter case are one world: a stored world is generated, loads with
/// its saved time, its file's name and the seed of that name, and is deleted under any of them,
/// and keeps its one file.
#[test]
fn a_world_is_found_by_its_name_in_any_letter_case() {
    let dir = crate::save::tests::TempDir::new("world-case");
    let pool = crate::runtime::cpu_pool(1).unwrap();
    let files = || crate::save::world_files(&dir.0).unwrap();
    {
        let (mut state, mut link, handle) = open_server(&dir, Listeners::default());
        let id = cx(&mut state)
            .create_world("lobby", &Options::new())
            .unwrap();
        run_saved(&mut state, &mut link, &pool, 3);
        let mut c = cx(&mut state);
        assert!(c.set_time(id, 4000));
        c.unload_world(id).unwrap();
        run_saved(&mut state, &mut link, &pool, 3);
        finish(link, handle);
    }
    {
        let (mut state, mut link, handle) = open_server(&dir, Listeners::default());
        let mut c = cx(&mut state);
        assert!(c.is_world_generated("LOBBY"));
        let id = c.load_world("Lobby", &Options::new()).unwrap();
        let lobby = c.world(id).unwrap();
        assert!(lobby.time_ticks() >= 4000);
        assert_eq!(
            (lobby.name(), lobby.seed()),
            ("lobby", save::default_seed("lobby"))
        );
        assert_eq!(c.load_world("LOBBY", &Options::new()), Ok(id));
        assert!(c.is_world_loaded("lobby"));
        run_saved(&mut state, &mut link, &pool, 3);
        cx(&mut state).unload_world(id).unwrap();
        run_saved(&mut state, &mut link, &pool, 3);
        finish(link, handle);
    }
    assert_eq!(files(), ["lobby", "world"]);
    let (mut state, mut link, handle) = open_server(&dir, Listeners::default());
    let mut c = cx(&mut state);
    c.delete_world("LOBBY").unwrap();
    // The file goes once a commit holds the deletion.
    c.save();
    run_saved(&mut state, &mut link, &pool, 1);
    finish(link, handle);
    assert_eq!(files(), ["world"]);
    assert!(pool.shutdown());
}

/// A world deleted and made again under its name by a callback after the view phase (here one
/// an unload listener deferred, in a tick that saves) is saved like any new world.
#[test]
fn a_world_made_again_late_in_the_tick_of_its_deletion_is_saved() {
    let dir = crate::save::tests::TempDir::new("world-remake");
    let pool = crate::runtime::cpu_pool(1).unwrap();
    let at = BlockPos::new(1, -60, 1);
    let remake = on(|_: &mut WorldUnload, cx: &mut Context<'_>| {
        cx.defer(|cx| {
            cx.delete_world("lobby").unwrap();
            cx.create_world("lobby", &Options::new()).unwrap();
        })
        .unwrap();
    });
    {
        let (mut state, mut link, handle) = open_server(&dir, remake);
        let lobby = cx(&mut state)
            .create_world("lobby", &Options::new())
            .unwrap();
        run_saved(&mut state, &mut link, &pool, 3);
        cx(&mut state).unload_world(lobby).unwrap();
        run_saved(&mut state, &mut link, &pool, 3);
        let mut c = cx(&mut state);
        let again = c.worlds().find(|w| w.name() == "lobby").unwrap().id();
        c.world_mut(again).unwrap().set_block(at, stone()).unwrap();
        c.save();
        run_saved(&mut state, &mut link, &pool, 3);
        finish(link, handle);
    }
    assert_eq!(
        crate::save::world_files(&dir.0).unwrap(),
        ["lobby", "world"]
    );
    let (mut state, mut link, handle) = open_server(&dir, Listeners::default());
    let lobby = cx(&mut state).load_world("lobby", &Options::new()).unwrap();
    run_saved(&mut state, &mut link, &pool, 3);
    assert_eq!(
        cx(&mut state).world(lobby).unwrap().block(at),
        Some(stone())
    );
    finish(link, handle);
    assert!(pool.shutdown());
}

#[test]
fn a_world_made_by_a_load_listener_loads_its_spawn_chunks_from_the_save_thread() {
    let dir = crate::save::tests::TempDir::new("world-made-in-listener");
    let pool = crate::runtime::cpu_pool(1).unwrap();
    let listeners = on(|e: &mut WorldLoad, cx: &mut Context<'_>| {
        if cx.world(e.world()).is_some_and(|w| w.name() == "lobby") {
            cx.create_world("arena", &arena(4)).unwrap();
        }
    });
    let (mut state, mut link, handle) = open_server(&dir, listeners);
    cx(&mut state)
        .create_world("lobby", &Options::new().with_seed(5))
        .unwrap();
    run_saved(&mut state, &mut link, &pool, 4);
    let w = &state.worlds[state.catalog.names["arena"]];
    let spawn = w.spawn().chunk();
    for x in -1..=1 {
        for z in -1..=1 {
            let pos = ChunkPos::new(spawn.x + x, spawn.z + z);
            assert!(w.is_chunk_loaded(pos), "{pos:?} is quarantined or missing");
        }
    }
    finish(link, handle);
    assert!(pool.shutdown());
}

/// The task of the replay runs: it makes, holds, unloads and makes again worlds, stops time,
/// sets difficulties and weather, strikes lightning that hurts the player and sets fire (its bolt
/// waits for its removal across a save), and saves now and then.
fn script(state: &mut State) {
    let mut tasks = crate::tick::Tasks::default();
    tasks.push(std::num::NonZeroU32::MIN, |c: &mut Context<'_>| {
        // The runs start at tick 4 ([`server`]).
        match c.tick() % 40 {
            5 => {
                let id = c.create_world("arena", &Options::new().with_kind(Kind::Temporary));
                assert!(id.is_ok(), "{id:?}");
            }
            7 => {
                let lobby = c
                    .load_world("lobby", &Options::new())
                    .or_else(|_| c.create_world("lobby", &Options::new().with_generator("void")));
                let lobby = lobby.unwrap();
                let _ticket = c.hold_chunks(lobby, ChunkPos::new(5, 5), 1).unwrap();
                c.stop_time(lobby);
                c.set_world_difficulty(lobby, Some(Difficulty::Easy));
                c.set_weather(lobby, Weather::Thunder);
            }
            9 => {
                c.set_weather(world::Id::DEFAULT, Weather::Rain);
                let player = c.player(crate::player::Id::new(SessionId::new(1))).unwrap();
                let feet = player.position();
                let lightning = Lightning {
                    damage: true,
                    fire: true,
                };
                assert!(c.strike_lightning(world::Id::DEFAULT, feet, lightning));
            }
            12 => {
                let lobby = c.world_named("lobby").unwrap().id();
                let set = c
                    .world_mut(lobby)
                    .unwrap()
                    .set_block(BlockPos::new(80, 0, 80), stone());
                assert_eq!(set, Ok(()), "the ticket's chunk is loaded");
                c.save();
            }
            24 => {
                for name in ["arena", "lobby"] {
                    let id = c.world_named(name).unwrap().id();
                    c.unload_world(id).unwrap();
                }
            }
            _ => {}
        }
    });
    state.tasks = tasks;
}

/// Runs the script for `n` ticks with the stand-in save thread: the hash after each tick and the
/// log; every save boundary's checkpoint restores into a fresh state with the same hash.
fn live(n: usize, pool: &Pool) -> (Vec<u64>, InputLog, usize) {
    let (mut state, mut output, _rx) = server(Listeners::default());
    script(&mut state);
    let mut log = InputLog::default();
    let mut hashes = Vec::new();
    let mut restored = 0;
    for _ in 0..n {
        let mut answers = std::mem::take(&mut output.answers);
        crate::tick::step(
            &mut state,
            &mut Vec::new(),
            &mut answers,
            pool,
            &mut output,
            &mut log,
        );
        deliver(&mut state, &mut output, &mut log);
        boundary(&mut state, &mut output, &mut log);
        let requests = std::mem::take(&mut output.requests);
        if requests.iter().any(|r| matches!(r, Request::Commit(_))) {
            let mut fresh = fresh_like(&state);
            script(&mut fresh);
            restored += usize::from(checkpoint::tests::assert_round_trip(&state, fresh, pool));
        }
        output.answers = answer(&state, requests);
        hashes.push(state.hash());
    }
    (hashes, log, restored)
}

/// Replays `ticks` from the start state: the hash after each tick.
fn replay(ticks: &[LoggedTick], pool: &Pool) -> Vec<u64> {
    let (mut state, mut output, _rx) = server(Listeners::default());
    script(&mut state);
    let mut log = InputLog::default();
    let mut hashes = Vec::new();
    let mut receivers = BTreeMap::new();
    for t in ticks {
        assert_eq!(t.number, state.tick);
        let mut inputs: Vec<Input> = t
            .inputs
            .iter()
            .map(|e| Input {
                session: e.session,
                seq: e.seq,
                kind: match &e.kind {
                    EntryKind::Join(identity) => {
                        let (tx, rx) = outbound_queue();
                        receivers.insert(e.session, rx);
                        InputKind::Join {
                            identity: identity.clone(),
                            outbound: tx,
                        }
                    }
                    EntryKind::Packet(p) => crate::tick::tests::packet(p.clone()),
                    EntryKind::Leave => InputKind::Leave,
                    EntryKind::Console(line) => InputKind::Console(line.clone()),
                    EntryKind::Job(outcome) => InputKind::Job(outcome.clone()),
                    EntryKind::Measure(measure) => InputKind::Measure(*measure),
                },
            })
            .collect();
        let mut answers: Vec<Response> = t
            .responses
            .iter()
            .map(|a| {
                match a {
                    crate::tick::Answer::Chunk { world, pos, .. } => Some(Response::Chunk {
                        world: *world,
                        pos: *pos,
                        loaded: crate::tick::extra::tests::sample_loaded(
                            &state,
                            *world,
                            *pos,
                            Box::new(crate::world::Chunk::generate(
                                &Flat,
                                state.worlds[*world].seed(),
                                *pos,
                            )),
                        ),
                    }),
                    crate::tick::Answer::Committed(start) => {
                        Some(Response::Committed { start: *start })
                    }
                    _ => None,
                }
                .expect("the script gets only chunk and commit answers")
            })
            .collect();
        crate::tick::step(
            &mut state,
            &mut inputs,
            &mut answers,
            pool,
            &mut output,
            &mut log,
        );
        deliver(&mut state, &mut output, &mut log);
        boundary(&mut state, &mut output, &mut log);
        output.requests.clear();
        hashes.push(state.hash());
    }
    hashes
}

#[test]
fn a_script_that_makes_and_unloads_worlds_replays_to_the_same_states() {
    let one = crate::runtime::cpu_pool(1).unwrap();
    let many = crate::runtime::cpu_pool(4).unwrap();
    let (hashes, log, restored) = live(130, &one);
    let (again, _, _) = live(130, &many);
    assert_eq!(hashes, again, "1 and 4 pool threads");
    assert!(restored >= 3, "{restored} save boundaries restored");
    let read = InputLog::decode(&log.encode().unwrap()).unwrap();
    assert_eq!(replay(&read.ticks(), &one), hashes);
    assert!(one.shutdown() && many.shutdown());
}

/// The world files a start left out (damaged, without a record) count toward the most
/// `data/worlds/` holds: past them a persistent world is refused, also after a restore from a
/// checkpoint, while a temporary one, which has no file, is made.
#[test]
#[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
fn a_persistent_world_past_the_files_a_start_left_out_is_full() {
    let dir = crate::save::tests::TempDir::new("world-limit-tick");
    let worlds = dir.0.join(crate::storage::WORLD_DIR);
    std::fs::create_dir_all(&worlds).unwrap();
    // Empty files are damaged ones; with the declared world's, the folder is full.
    for i in 1..MAX_WORLDS {
        let file = worlds.join(format!("w{i:04}.{}", crate::storage::EXTENSION));
        std::fs::write(file, b"").unwrap();
    }
    let (mut state, link, handle) = open_server(&dir, Listeners::default());
    let pool = crate::runtime::cpu_pool(1).unwrap();
    let bytes = state.checkpoint(&pool).to_bytes();
    let mut restored = fresh_like(&state);
    let custom = Arc::clone(&restored.custom);
    let entities = restored.rules.entities.take_customs();
    let read = checkpoint::Checkpoint::read(&bytes, &custom, &entities).unwrap();
    restored.rules.entities.set_customs(entities);
    restored
        .restore(&read, checkpoint::tests::extras_of(&state), |_| {
            outbound_queue().0
        })
        .unwrap();
    for s in [&mut state, &mut restored] {
        let mut c = cx(s);
        assert_eq!(
            c.create_world("extra", &Options::new()),
            Err(world::Error::Full)
        );
        c.create_world("arena", &arena(1)).unwrap();
    }
    finish(link, handle);
    assert!(pool.shutdown());
}
