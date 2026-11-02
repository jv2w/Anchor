//! Tests of the public world API: worlds by ID, the held spawn chunks following the spawn, the
//! clock, and sounds, particles and packets reaching only the players who see the place.

use tokio::sync::mpsc;

use super::*;
use crate::config::Config;
use crate::event::Listeners;
use crate::protocol::packet::{LevelEvent as LevelEventPacket, Packet as P};
use crate::tick::queue::Outbound;
use crate::tick::tests::{initialised, join, new_state, radius, received, settings, tick};
use crate::tick::{MAX_OUTBOX, Output, SessionId, Setup, Shared};
use crate::world::{BlockPos, ChunkPos, Flat, Generator, Kind};

const WORLD: world::Id = world::Id::DEFAULT;

fn context(state: &mut State) -> Context<'_> {
    Context { state }
}

// pmmp: WorldManager::getWorld, WorldManager::isWorldLoaded, Position::isValid, WorldManager::getWorlds, World::getId, World::getFolderName, World::getDisplayName
#[test]
fn worlds_are_found_by_their_id_in_list_order() {
    let config = Config::default();
    let worlds = vec![
        World::new("lobby", Kind::Persistent, 1, Flat.spawn(1)),
        World::new("arena", Kind::Temporary, 2, Flat.spawn(2)),
    ];
    let shared = Shared::new(
        crate::protocol::batch::Settings::default(),
        &crate::custom::Registry::default(),
        &crate::tick::entity::Customs::default(),
    )
    .unwrap();
    let mut state = State::new(
        crate::tick::Settings::new(&config),
        shared,
        Setup::default(),
        worlds,
        0,
    );
    let mut cx = context(&mut state);
    let names: Vec<(&str, world::Id)> = cx.worlds().map(|w| (w.name(), w.id())).collect();
    assert_eq!(
        names,
        vec![("lobby", world::Id::DEFAULT), ("arena", world::Id::of(1))]
    );
    assert_eq!(cx.world(world::Id::of(1)).unwrap().name(), "arena");
    assert!(cx.world(world::Id::of(2)).is_none());
    assert!(cx.world_mut(world::Id::of(7)).is_none());
    let arena = cx.world_mut(world::Id::of(1)).unwrap();
    assert_eq!(arena.id(), world::Id::of(1));
    assert_eq!(cx.worlds().count(), 2);
}

// pmmp: World::setSpawnLocation, World::getSpawnLocation, World::isSpawnChunk
#[test]
fn moving_the_spawn_moves_the_held_spawn_chunks() {
    let mut state = new_state(settings(|_| {}), Listeners::default());
    let mut output = Output::default();
    let old = state.worlds[0].spawn().chunk();
    let held = |state: &State, center: ChunkPos| {
        (-1..=1)
            .flat_map(|x| (-1..=1).map(move |z| ChunkPos::new(center.x + x, center.z + z)))
            .filter(|&p| state.chunks.holds((0, p)))
            .count()
    };
    assert_eq!(held(&state, old), 9);
    let before = state.chunks.held();
    let far = BlockPos::new(800, -60, -800);
    context(&mut state).world_mut(WORLD).unwrap().set_spawn(far);
    assert_eq!(state.worlds[0].spawn(), far);
    assert!(state.worlds[0].is_spawn_chunk(ChunkPos::new(50, -50)));
    assert!(state.worlds[0].is_spawn_chunk(ChunkPos::new(49, -51)));
    assert!(!state.worlds[0].is_spawn_chunk(old));
    tick(&mut state, &mut Vec::new(), &mut output);
    assert_eq!(held(&state, far.chunk()), 9, "the new chunks are held");
    assert_eq!(held(&state, old), 0, "the old ones are let go");
    assert_eq!(state.chunks.held(), before);
    assert!(!state.worlds[0].is_chunk_loaded(old), "and unloaded");
    // The loads answer and the new spawn chunks stay loaded without players.
    for _ in 0..3 {
        tick(&mut state, &mut Vec::new(), &mut output);
    }
    assert!(state.worlds[0].is_chunk_loaded(far.chunk()));
    // Setting the same spawn again changes nothing.
    let hash = state.hash();
    context(&mut state).world_mut(WORLD).unwrap().set_spawn(far);
    assert_eq!(state.hash(), hash);
}

/// A state with session 1 spawned at the world spawn with a view of radius 2 and session 2
/// joined without a view, and their queues.
fn state() -> (State, [mpsc::Receiver<Outbound>; 2]) {
    let mut state = new_state(settings(|_| {}), Listeners::default());
    let mut output = Output::default();
    let (first, rx1) = join(1, 0);
    let (second, rx2) = join(2, 0);
    tick(&mut state, &mut vec![first, second], &mut output);
    tick(&mut state, &mut vec![radius(1, 1, 2)], &mut output);
    let runtime_id = state.players[&SessionId::new(1)].runtime_id;
    tick(
        &mut state,
        &mut vec![initialised(1, 2, runtime_id)],
        &mut output,
    );
    for _ in 0..3 {
        tick(&mut state, &mut Vec::new(), &mut output);
    }
    (state, [rx1, rx2])
}

/// The level events in what `rx` was sent.
fn level_events(rx: &mut mpsc::Receiver<Outbound>) -> Vec<LevelEventPacket> {
    received(rx)
        .0
        .into_iter()
        .filter_map(|p| match p {
            P::LevelEvent(e) => Some(e),
            _ => None,
        })
        .collect()
}

// pmmp: World::addSound, World::addParticle, World::broadcastPacketToViewers
#[test]
fn sounds_particles_and_packets_reach_only_the_players_who_see_the_place() {
    let (mut state, [mut rx1, mut rx2]) = state();
    let spawn = state.worlds[0].spawn();
    let here = Vec3::new(
        f64::from(spawn.x) + 0.5,
        f64::from(spawn.y),
        f64::from(spawn.z) + 0.5,
    );
    let far = Vec3::new(5000.0, -60.0, 5000.0);
    received(&mut rx1);
    received(&mut rx2);
    let mut cx = context(&mut state);
    assert!(cx.add_sound(WORLD, here, Sound::AnvilFall));
    assert!(cx.add_particle(WORLD, here, Particle::Flame));
    assert!(cx.add_particle(WORLD, far, Particle::Heart(1)));
    assert!(!cx.add_sound(world::Id::of(1), here, Sound::AnvilFall));
    assert!(!cx.add_particle(WORLD, Vec3::new(f64::NAN, 0.0, 0.0), Particle::Flame));
    assert!(!cx.broadcast_packet(WORLD, Vec3::new(4e7, 0.0, 0.0), &Sound::Burp.packet(here)));
    tick(&mut state, &mut Vec::new(), &mut Output::default());
    let seen = level_events(&mut rx1);
    let anvil = i32::from(crate::protocol::consts::LevelEvent::SoundAnvilLand);
    let flame = 0x4000 | 8;
    assert_eq!(
        seen.iter().map(|e| e.event_type).collect::<Vec<_>>(),
        vec![anvil, flame],
        "the far heart is not seen"
    );
    assert_eq!(seen[1].position, here.to_f32());
    assert!(level_events(&mut rx2).is_empty(), "no view, nothing seen");
    // A flood of effects stops before the outbox would disconnect the player.
    let mut cx = context(&mut state);
    for _ in 0..MAX_OUTBOX {
        cx.add_particle(WORLD, here, Particle::Flame);
    }
    assert_eq!(
        state.players[&SessionId::new(1)].outbox.len(),
        MAX_OUTBOX - EFFECT_RESERVE
    );
    tick(&mut state, &mut Vec::new(), &mut Output::default());
    assert!(state.players.contains_key(&SessionId::new(1)));
}

// pmmp: World::setTime, World::getTime, World::getPlayers
#[test]
fn setting_the_time_tells_the_players_of_the_world_at_once() {
    let (mut state, [mut rx1, mut rx2]) = state();
    received(&mut rx1);
    received(&mut rx2);
    let mut cx = context(&mut state);
    assert!(cx.set_time(WORLD, 18_000));
    assert!(!cx.set_time(world::Id::of(1), 5));
    assert_eq!(cx.world(WORLD).unwrap().time_ticks(), 18_000);
    let players: Vec<player::Id> = cx.players_in(WORLD).collect();
    assert_eq!(
        players,
        vec![
            player::Id::new(SessionId::new(1)),
            player::Id::new(SessionId::new(2))
        ]
    );
    assert_eq!(cx.players_in(world::Id::of(1)).count(), 0);
    tick(&mut state, &mut Vec::new(), &mut Output::default());
    let clocks = |rx: &mut mpsc::Receiver<Outbound>| {
        received(rx)
            .0
            .into_iter()
            .filter_map(|p| match p {
                P::SyncWorldClocks(c) => c.sync_states.first().map(|s| s.time),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(clocks(&mut rx1), vec![18_000]);
    assert_eq!(
        clocks(&mut rx2),
        vec![18_000],
        "joined: its StartGame went out"
    );
}

// pmmp: World::getChunkPlayers, World::getViewersForPosition
#[test]
fn chunk_viewers_are_the_players_sent_the_chunk() {
    let (mut state, _rx) = state();
    let spawn = state.worlds[0].spawn().chunk();
    let cx = context(&mut state);
    let viewers: Vec<player::Id> = cx.chunk_viewers(WORLD, spawn).collect();
    assert_eq!(
        viewers,
        vec![player::Id::new(SessionId::new(1))],
        "session 2 has no view"
    );
    let far = ChunkPos::new(spawn.x + 40, spawn.z);
    assert_eq!(cx.chunk_viewers(WORLD, far).count(), 0);
    assert_eq!(cx.chunk_viewers(world::Id::of(1), spawn).count(), 0);
}

// pmmp: World::getSafeSpawn
#[test]
fn the_safe_spawn_is_the_first_two_free_blocks_up_the_column() {
    let (mut state, _rx) = state();
    let spawn = state.worlds[0].spawn();
    let column = |y| BlockPos::new(spawn.x + 1, y, spawn.z);
    let world = &mut state.worlds[0];
    // The flat world: bedrock at -64, dirt, grass at -61, air from -60.
    assert_eq!(world.safe_spawn(column(-64)), column(-60));
    assert_eq!(world.safe_spawn(column(-60)), column(-60));
    assert_eq!(
        world.safe_spawn(column(-1000)),
        column(-60),
        "kept in the world"
    );
    let stone = crate::block::Kind::Stone.default_state();
    world.set_block(column(-59), stone).unwrap();
    assert_eq!(
        world.safe_spawn(column(-61)),
        column(-58),
        "two blocks of room"
    );
    assert_eq!(world.safe_spawn(column(i32::MAX)), column(world::MAX_Y));
    // A chunk that is not loaded is free.
    let far = BlockPos::new(5000, -64, 5000);
    assert_eq!(world.safe_spawn(far), far);
}

/// The stored state of the block `name` with no properties, which Anchor does not know.
fn stored_state(name: &str) -> crate::nbt::Compound {
    use crate::nbt::{Compound, Value};
    Compound::from([
        ("name".to_owned(), Value::String(name.to_owned())),
        ("states".to_owned(), Value::Compound(Compound::new())),
        (
            "version".to_owned(),
            Value::Int(crate::import::BLOCK_STATE_VERSION),
        ),
    ])
}

/// The safe spawn stands no player in a block the entity rules find solid: a solid block only
/// clients know, or a block neither Anchor nor clients know.
#[test]
fn the_safe_spawn_finds_solid_the_blocks_anchor_does_not_know() {
    let (mut state, _rx) = state();
    let spawn = state.worlds[0].spawn();
    let column = |y| BlockPos::new(spawn.x + 1, y, spawn.z);
    let world = &mut state.worlds[0];
    world
        .put_unknown(column(-60), stored_state("minecraft:moss_block"))
        .unwrap();
    world
        .put_unknown(column(-58), stored_state("minecraft:no_such_block"))
        .unwrap();
    assert_eq!(world.safe_spawn(column(-61)), column(-57));
}

// pmmp: World::getBlockCollisionBoxes
#[test]
fn collision_boxes_are_the_blocks_boxes_in_the_world() {
    let (state, _rx) = state();
    let world = &state.worlds[0];
    let spawn = world.spawn();
    let (x, z) = (f64::from(spawn.x), f64::from(spawn.z));
    // Inside the grass block at -61: its one box, moved to its place.
    let inside = Aabb::new(
        Vec3::new(x + 0.2, -60.8, z + 0.2),
        Vec3::new(x + 0.8, -60.2, z + 0.8),
    );
    assert_eq!(
        world.collision_boxes(inside),
        Some(vec![Aabb::new(
            Vec3::new(x, -61.0, z),
            Vec3::new(x + 1.0, -60.0, z + 1.0)
        )])
    );
    let air = inside.offset(Vec3::new(0.0, 3.0, 0.0));
    assert_eq!(world.collision_boxes(air), Some(Vec::new()));
    // Down through the grass and both dirt blocks: three boxes.
    let column = Aabb::new(
        Vec3::new(x + 0.2, -62.8, z + 0.2),
        Vec3::new(x + 0.8, -60.2, z + 0.8),
    );
    assert_eq!(world.collision_boxes(column).map(|b| b.len()), Some(3));
    // A chunk that is not loaded is solid.
    let far = inside.offset(Vec3::new(5000.0, 10.0, 0.0));
    assert_eq!(world.collision_boxes(far).map(|b| b.len()), Some(1));
    let big = Aabb::new(Vec3::new(x, -61.0, z), Vec3::new(x + 33.0, -60.0, z));
    assert_eq!(world.collision_boxes(big), None);
    let nan = Aabb::new(Vec3::new(f64::NAN, 0.0, 0.0), Vec3::new(0.0, 0.0, 0.0));
    assert_eq!(world.collision_boxes(nan), None);
    let outside = inside.offset(Vec3::new(4e7, 0.0, 0.0));
    assert_eq!(world.collision_boxes(outside), None);
}

// pmmp: Explosion::__construct, Explosion::explodeA, Explosion::explodeB, Explosion::setFireChance
#[test]
fn explosions_break_blocks_and_refuse_what_cannot_explode() {
    let (mut state, _rx) = state();
    let spawn = state.worlds[0].spawn();
    let below = BlockPos::new(spawn.x + 6, spawn.y - 1, spawn.z);
    let center = Vec3::new(
        f64::from(below.x) + 0.5,
        f64::from(below.y) + 0.5,
        f64::from(below.z) + 0.5,
    );
    let before = state.worlds[0].block(below);
    let mut cx = context(&mut state);
    assert!(!cx.explode(world::Id::of(1), center, 4.0));
    assert!(!cx.explode(WORLD, center, 0.0));
    assert!(!cx.explode(WORLD, center, f64::NAN));
    assert!(!cx.explode_with_fire(WORLD, Vec3::new(f64::INFINITY, 0.0, 0.0), 4.0));
    assert!(cx.explode(WORLD, center, 4.0));
    assert_ne!(state.worlds[0].block(below), before, "the block is gone");
    assert!(context(&mut state).explode_with_fire(WORLD, center, 3.0));
}

#[test]
fn a_player_teleported_this_tick_is_in_its_new_world_for_every_query() {
    let (mut state, _rx) = state();
    let options = crate::world::Options::new()
        .with_kind(Kind::Temporary)
        .with_seed(3);
    let arena = context(&mut state).create_world("arena", &options).unwrap();
    for _ in 0..4 {
        tick(&mut state, &mut Vec::new(), &mut Output::default());
    }
    let id = player::Id::new(SessionId::new(1));
    let in_world = |cx: &Context<'_>, w: world::Id| cx.players_in(w).any(|p| p == id);
    let mut cx = context(&mut state);
    cx.teleport(id, arena, Vec3::new(0.5, -60.0, 0.5)).unwrap();
    assert_eq!(cx.player(id).unwrap().world(), arena);
    assert!(in_world(&cx, arena), "in the arena");
    assert!(!in_world(&cx, WORLD), "not in the old world");
    tick(&mut state, &mut Vec::new(), &mut Output::default());
    let cx = context(&mut state);
    assert_eq!(cx.player(id).unwrap().world(), arena);
    assert!(in_world(&cx, arena) && !in_world(&cx, WORLD));
}
