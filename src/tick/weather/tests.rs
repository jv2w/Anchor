//! Tests of the weather and lightning (PLAN.md 10.9 날씨와 번개): the level events of each
//! change as BDS sends them once settled (P6 `weather`), who is told of a change, what a join and
//! a move to another world say, the send and start-game hooks, and lightning bolts with their
//! damage and fire options. Saving is tested with the world record (`save`, `storage::codec`),
//! replays with the world lifecycle script (`world_api::lifecycle`).

use tokio::sync::mpsc;

use super::*;
use crate::Server;
use crate::block;
use crate::config::Difficulty;
use crate::event::Listeners;
use crate::math::Vec3;
use crate::player;
use crate::tick::queue::{Outbound, SessionId};
use crate::tick::tests::{initialised, join, new_state, radius, received, settings, tick};
use crate::tick::{Context, Output, Phase, State, survival};
use crate::world::{self, BlockPos, Kind, Lightning, Options};

const WORLD: world::Id = world::Id::DEFAULT;

/// The start of rain or thunder and its stop, as (event type, data).
fn start(event: i32) -> (i32, i32) {
    (event, 65_535)
}

fn stop(event: i32) -> (i32, i32) {
    (event, 0)
}

/// (event type, data) of the weather level events in `packets`, in order.
fn events(packets: &[Packet]) -> Vec<(i32, i32)> {
    packets
        .iter()
        .filter_map(|p| match p {
            Packet::LevelEvent(e) if (3001..=3004).contains(&e.event_type) => {
                Some((e.event_type, e.event_data))
            }
            _ => None,
        })
        .collect()
}

/// The weather level events `rx` was sent.
fn sent_events(rx: &mut mpsc::Receiver<Outbound>) -> Vec<(i32, i32)> {
    events(&received(rx).0)
}

/// A server with `sessions` spawned at the world spawn with a view of radius 2, their queues
/// emptied.
fn spawned<const N: usize>(sessions: [u64; N]) -> (State, Output, [mpsc::Receiver<Outbound>; N]) {
    let mut state = new_state(settings(|_| {}), Listeners::default());
    let mut output = Output::default();
    let mut joins = Vec::new();
    let mut queues = sessions.map(|s| {
        let (j, rx) = join(s, 0);
        joins.push(j);
        rx
    });
    tick(&mut state, &mut joins, &mut output);
    let mut radii: Vec<_> = sessions.iter().map(|&s| radius(s, 1, 2)).collect();
    tick(&mut state, &mut radii, &mut output);
    let mut ready: Vec<_> = sessions
        .iter()
        .map(|&s| initialised(s, 2, state.players[&SessionId::new(s)].runtime_id))
        .collect();
    tick(&mut state, &mut ready, &mut output);
    for _ in 0..3 {
        tick(&mut state, &mut Vec::new(), &mut output);
    }
    for rx in &mut queues {
        received(rx);
    }
    (state, output, queues)
}

fn cx(state: &mut State) -> Context<'_> {
    Context { state }
}

/// The feet of a player standing at the spawn of world `w`.
fn spawn_feet(state: &State, w: usize) -> Vec3 {
    let spawn = state.worlds[w].spawn();
    Vec3::new(
        f64::from(spawn.x) + 0.5,
        f64::from(spawn.y),
        f64::from(spawn.z) + 0.5,
    )
}

/// A temporary world "arena", its index, after the ticks that load its spawn.
fn arena(state: &mut State, output: &mut Output) -> (world::Id, usize) {
    let options = Options::new().with_kind(Kind::Temporary).with_seed(3);
    let id = cx(state).create_world("arena", &options).unwrap();
    for _ in 0..4 {
        tick(state, &mut Vec::new(), output);
    }
    (id, id.index())
}

/// Moves session `s` to the spawn of world `w`.
fn move_to(state: &mut State, s: u64, w: usize) {
    let eye = spawn_feet(state, w) + Vec3::new(0.0, 1.62, 0.0);
    survival::teleport(state, SessionId::new(s), w, eye);
}

#[test]
fn changes_start_and_stop_rain_and_thunder_as_settled_bds_weather_does() {
    use Weather::{Clear, Rain, Thunder};
    // P6 `weather` (m9-facts.md WEATHER), every step after BDS's levels settled.
    let cases = [
        (Clear, Rain, vec![start(3001)]),
        (Rain, Rain, vec![]),
        (Rain, Thunder, vec![start(3002)]),
        (Thunder, Thunder, vec![]),
        (Thunder, Rain, vec![stop(3004)]),
        (Rain, Clear, vec![stop(3003)]),
        (Clear, Thunder, vec![start(3001), start(3002)]),
        (Thunder, Clear, vec![stop(3003), stop(3004)]),
        (Clear, Clear, vec![]),
    ];
    for (from, to, want) in cases {
        let sent: Vec<Packet> = changes(from, to).collect();
        assert_eq!(events(&sent), want, "{from:?} to {to:?}");
    }
    // P6 `joinrain` and `joinstorm`.
    let join = |w| events(&joined(w).into_iter().collect::<Vec<_>>());
    assert_eq!(join(Clear), []);
    assert_eq!(join(Rain), [start(3001)]);
    assert_eq!(join(Thunder), [start(3002)]);
    assert_eq!(levels(Clear), (0.0, 0.0));
    assert_eq!(levels(Rain), (1.0, 0.0));
    assert_eq!(levels(Thunder), (1.0, 1.0));
}

#[test]
fn a_change_reaches_the_players_of_its_world_only_and_stays() {
    let (mut state, mut output, [mut rx1, mut rx2]) = spawned([1, 2]);
    let (arena, a) = arena(&mut state, &mut output);
    move_to(&mut state, 2, a);
    tick(&mut state, &mut Vec::new(), &mut output);
    received(&mut rx1);
    received(&mut rx2);
    assert_eq!(
        state.worlds[0].weather(),
        Weather::Clear,
        "a new world is clear"
    );
    let mut c = cx(&mut state);
    assert!(c.set_weather(WORLD, Weather::Rain));
    assert!(
        c.set_weather(WORLD, Weather::Rain),
        "the same again tells nothing"
    );
    assert!(c.set_weather(arena, Weather::Thunder));
    assert!(!c.set_weather(world::Id::of(9), Weather::Rain));
    assert_eq!(c.world(WORLD).unwrap().weather(), Weather::Rain);
    assert_eq!(c.world(arena).unwrap().weather(), Weather::Thunder);
    tick(&mut state, &mut Vec::new(), &mut output);
    assert_eq!(sent_events(&mut rx1), [start(3001)]);
    assert_eq!(sent_events(&mut rx2), [start(3001), start(3002)]);
    // No cycle: it rains until changed.
    for _ in 0..100 {
        tick(&mut state, &mut Vec::new(), &mut output);
    }
    assert_eq!(state.worlds[0].weather(), Weather::Rain);
    assert_eq!(sent_events(&mut rx1), []);
}

#[test]
fn a_joining_player_is_told_the_weather_by_start_game_and_the_spawn_window() {
    for (weather, levels, told) in [
        (Weather::Clear, (0.0, 0.0), vec![]),
        (Weather::Rain, (1.0, 0.0), vec![start(3001)]),
        (Weather::Thunder, (1.0, 1.0), vec![start(3002)]),
    ] {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        assert!(cx(&mut state).set_weather(WORLD, weather));
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j], &mut output);
        tick(&mut state, &mut vec![radius(1, 1, 2)], &mut output);
        let packets = received(&mut rx).0;
        let start_game = packets.iter().find_map(|p| match p {
            Packet::StartGame(s) => Some((s.rain_level, s.lightning_level)),
            _ => None,
        });
        assert_eq!(start_game, Some(levels), "{weather:?}");
        assert_eq!(events(&packets), told, "{weather:?}");
        // BDS's place: right after `SetCommandsEnabled`.
        if !told.is_empty() {
            let at = packets
                .iter()
                .position(|p| matches!(p, Packet::SetCommandsEnabled(_)))
                .unwrap();
            assert!(
                matches!(packets[at + 1], Packet::LevelEvent(_)),
                "{weather:?}"
            );
        }
    }
}

#[test]
fn a_player_moving_to_a_world_of_other_weather_is_told_the_change() {
    let (mut state, mut output, [mut rx]) = spawned([1]);
    let (arena, a) = arena(&mut state, &mut output);
    let rx = &mut rx;
    assert!(cx(&mut state).set_weather(WORLD, Weather::Rain));
    // The weather of the world the player goes to, set while it is elsewhere, and what the move
    // tells it.
    for (to, id, weather, told) in [
        (a, arena, Weather::Clear, vec![stop(3003)]),
        (0, WORLD, Weather::Rain, vec![start(3001)]),
        (a, arena, Weather::Thunder, vec![start(3002)]),
        (0, WORLD, Weather::Rain, vec![stop(3004)]),
    ] {
        assert!(cx(&mut state).set_weather(id, weather));
        tick(&mut state, &mut Vec::new(), &mut output);
        received(rx);
        move_to(&mut state, 1, to);
        tick(&mut state, &mut Vec::new(), &mut output);
        assert_eq!(sent_events(rx), told, "to world {to} in {weather:?}");
    }
}

/// A server whose world is `weather` and sends one chunk a tick, and session 1 joining it: its
/// start-game packets have gone out (it is `Joined`), not yet its spawn window.
fn joining(weather: Weather) -> (State, Output, mpsc::Receiver<Outbound>) {
    let slow = settings(|c| c.developer.chunks_per_tick = 1);
    let mut state = new_state(slow, Listeners::default());
    assert!(cx(&mut state).set_weather(WORLD, weather));
    let mut output = Output::default();
    let (j, rx) = join(1, 0);
    tick(&mut state, &mut vec![j], &mut output);
    (state, output, rx)
}

/// Sends session 1's chunk radius request: its spawn window goes out, and it still waits for
/// its spawn chunks.
fn open_window(state: &mut State, output: &mut Output) {
    tick(state, &mut vec![radius(1, 1, 2)], output);
    assert_eq!(state.players[&SessionId::new(1)].phase, Phase::Joined);
}

/// Runs ticks until session 1 has its spawn chunks, then confirms its spawn.
fn finish_spawn(state: &mut State, output: &mut Output) {
    for _ in 0..100 {
        if state.players[&SessionId::new(1)].phase == Phase::Spawning {
            break;
        }
        tick(state, &mut Vec::new(), output);
    }
    let runtime_id = state.players[&SessionId::new(1)].runtime_id;
    tick(state, &mut vec![initialised(1, 2, runtime_id)], output);
    assert_eq!(state.players[&SessionId::new(1)].phase, Phase::Spawned);
}

#[test]
fn a_player_waiting_for_its_spawn_chunks_is_told_a_change() {
    // After the spawn window.
    let (mut state, mut output, mut rx) = joining(Weather::Clear);
    open_window(&mut state, &mut output);
    received(&mut rx);
    assert!(cx(&mut state).set_weather(WORLD, Weather::Rain));
    finish_spawn(&mut state, &mut output);
    assert_eq!(sent_events(&mut rx), [start(3001)]);
    // Before it, under the levels `StartGame` had: the window only starts weather.
    let (mut state, mut output, mut rx) = joining(Weather::Thunder);
    assert!(cx(&mut state).set_weather(WORLD, Weather::Clear));
    open_window(&mut state, &mut output);
    finish_spawn(&mut state, &mut output);
    assert_eq!(sent_events(&mut rx), [stop(3003), stop(3004)]);
}

#[test]
fn a_player_moving_while_it_waits_for_its_spawn_chunks_is_told_the_change() {
    let (mut state, mut output, mut rx) = joining(Weather::Rain);
    let (_, a) = arena(&mut state, &mut output);
    open_window(&mut state, &mut output);
    received(&mut rx);
    move_to(&mut state, 1, a);
    finish_spawn(&mut state, &mut output);
    assert_eq!(sent_events(&mut rx), [stop(3003)]);
}

#[test]
fn weather_and_lightning_packets_pass_the_send_hooks_and_start_game_keeps_the_levels() {
    let builder = Server::builder()
        .on_send(|_, p| !matches!(p, Packet::LevelEvent(_) | Packet::AddActor(_)))
        .start_game(|s| s.rain_level = 0.5);
    let mut state = new_state(settings(|_| {}), Listeners::default());
    state.hooks = builder.setup.hooks;
    assert!(cx(&mut state).set_weather(WORLD, Weather::Rain));
    let mut output = Output::default();
    let (j, mut rx) = join(1, 0);
    tick(&mut state, &mut vec![j], &mut output);
    let packets = received(&mut rx).0;
    let levels = packets.iter().find_map(|p| match p {
        Packet::StartGame(s) => Some(s.rain_level),
        _ => None,
    });
    assert_eq!(
        levels,
        Some(1.0),
        "the hook cannot change what the world decides"
    );
    let rx = &mut rx;
    let runtime_id = state.players[&SessionId::new(1)].runtime_id;
    tick(&mut state, &mut vec![radius(1, 1, 2)], &mut output);
    tick(
        &mut state,
        &mut vec![initialised(1, 2, runtime_id)],
        &mut output,
    );
    tick(&mut state, &mut Vec::new(), &mut output);
    assert_eq!(sent_events(rx), [], "the spawn window's event is dropped");
    let mut c = cx(&mut state);
    assert!(c.set_weather(WORLD, Weather::Thunder));
    let feet = spawn_feet(c.state, 0);
    assert!(c.strike_lightning(WORLD, feet, Lightning::default()));
    tick(&mut state, &mut Vec::new(), &mut output);
    let packets = received(rx).0;
    assert_eq!(events(&packets), []);
    assert!(!packets.iter().any(|p| matches!(p, Packet::AddActor(_))));
    assert!(
        packets
            .iter()
            .any(|p| matches!(p, Packet::LevelSoundEvent(s) if s.sound_type == "thunder")),
        "the sounds pass"
    );
}

/// The lightning packets of a tick.
#[derive(Debug, Default, PartialEq)]
struct Bolts {
    /// Bolts shown: runtime ID and position.
    shown: Vec<(u64, [f32; 3])>,
    /// Their sounds: type and unique ID.
    sounds: Vec<(String, i64)>,
    /// Removals: unique ID.
    removed: Vec<i64>,
}

/// The lightning packets in `packets`.
fn bolt_packets(packets: &[Packet]) -> Bolts {
    let mut bolts = Bolts::default();
    for p in packets {
        match p {
            Packet::AddActor(a) if a.entity_type == "minecraft:lightning_bolt" => {
                assert_eq!(a.entity_unique_id, a.entity_runtime_id.cast_signed());
                bolts.shown.push((a.entity_runtime_id, a.position));
            }
            Packet::LevelSoundEvent(s) if s.entity_type == "minecraft:lightning_bolt" => {
                assert_eq!((s.extra_data, s.position), (-1, bolts.shown[0].1));
                bolts
                    .sounds
                    .push((s.sound_type.clone(), s.entity_unique_id));
            }
            Packet::RemoveActor(r) => bolts.removed.push(r.entity_unique_id),
            _ => {}
        }
    }
    bolts
}

#[test]
fn a_bolt_shows_with_its_sounds_to_those_who_see_it_and_goes_a_second_later() {
    let (mut state, mut output, [mut rx1]) = spawned([1]);
    let (j, mut rx2) = join(2, 0);
    tick(&mut state, &mut vec![j], &mut output);
    let at = spawn_feet(&state, 0) + Vec3::new(3.0, 0.0, 0.0);
    let far = Vec3::new(5000.0, -60.0, 5000.0);
    let before = state.next_runtime_id;
    let mut c = cx(&mut state);
    assert!(c.strike_lightning(WORLD, at, Lightning::default()));
    assert!(
        c.strike_lightning(WORLD, far, Lightning::default()),
        "nobody sees it"
    );
    assert!(!c.strike_lightning(world::Id::of(9), at, Lightning::default()));
    assert!(!c.strike_lightning(WORLD, Vec3::new(f64::NAN, 0.0, 0.0), Lightning::default()));
    assert!(!c.strike_lightning(WORLD, Vec3::new(4e7, 0.0, 0.0), Lightning::default()));
    assert_eq!(
        state.rules.entities.bolts_waiting(),
        1,
        "only a bolt someone saw waits"
    );
    tick(&mut state, &mut Vec::new(), &mut output);
    let unique = before.cast_signed();
    let sounds = ["thunder", "explode"].map(|s| (s.to_owned(), unique));
    assert_eq!(
        bolt_packets(&received(&mut rx1).0),
        Bolts {
            shown: vec![(before, at.to_f32())],
            sounds: sounds.to_vec(),
            removed: vec![],
        }
    );
    assert_eq!(bolt_packets(&received(&mut rx2).0), Bolts::default());
    // The tick after the call is the strike's (it sends the bolt); the bolt goes 20 ticks later.
    for _ in 0..19 {
        tick(&mut state, &mut Vec::new(), &mut output);
    }
    let removed = bolt_packets(&received(&mut rx1).0).removed;
    assert!(removed.is_empty(), "still there");
    tick(&mut state, &mut Vec::new(), &mut output);
    assert_eq!(bolt_packets(&received(&mut rx1).0).removed, [unique]);
    assert_eq!(state.rules.entities.bolts_waiting(), 0);
    assert_eq!(
        state.next_runtime_id,
        before + 2,
        "each bolt has its runtime ID"
    );
}

#[test]
fn bolts_over_the_limit_are_refused_and_all_go_with_their_removal() {
    let (mut state, mut output, [mut rx]) = spawned([1]);
    let at = spawn_feet(&state, 0);
    for _ in 0..4 {
        for _ in 0..256 {
            assert!(cx(&mut state).strike_lightning(WORLD, at, Lightning::default()));
        }
        tick(&mut state, &mut Vec::new(), &mut output);
        received(&mut rx);
    }
    assert_eq!(state.rules.entities.bolts_waiting(), 1024);
    assert!(!cx(&mut state).strike_lightning(WORLD, at, Lightning::default()));
    for _ in 0..20 {
        tick(&mut state, &mut Vec::new(), &mut output);
    }
    assert_eq!(state.rules.entities.bolts_waiting(), 0);
    assert_eq!(bolt_packets(&received(&mut rx).0).removed.len(), 1024);
    assert!(cx(&mut state).strike_lightning(WORLD, at, Lightning::default()));
}

/// A strike at `at` in the default world of a server with player 1 at the spawn and a zombie
/// 2 blocks and one 8 blocks from it: the player's health and fire ticks, and each zombie's.
fn struck(lightning: Lightning) -> ((f64, u32), [(f64, u16); 2]) {
    let (mut state, mut output, _rx) = spawned([1]);
    let feet = spawn_feet(&state, 0);
    let mut c = cx(&mut state);
    let near = c
        .spawn_zombie(WORLD, feet + Vec3::new(2.0, 0.0, 0.0))
        .unwrap();
    let far = c
        .spawn_zombie(WORLD, feet + Vec3::new(8.0, 0.0, 0.0))
        .unwrap();
    assert!(c.strike_lightning(WORLD, feet, lightning));
    let player = c.player(player::Id::new(SessionId::new(1))).unwrap();
    let hurt = (player.health(), player.fire_ticks());
    let zombie = |id| {
        let e = c.entity(id).unwrap();
        (e.health(), e.fire_ticks())
    };
    let zombies = [zombie(near), zombie(far)];
    tick(&mut state, &mut Vec::new(), &mut output);
    (hurt, zombies)
}

#[test]
fn lightning_hurts_and_burns_what_is_near_only_with_its_damage() {
    let calm = struck(Lightning::default());
    assert_eq!(calm, ((20.0, 0), [(20.0, 0), (20.0, 0)]));
    let (player, [near, far]) = struck(Lightning {
        damage: true,
        fire: false,
    });
    assert_eq!(player, (15.0, 160), "BDS's 5 damage, 8 seconds of fire");
    assert_eq!(near, (15.0, 160));
    assert_eq!(far, (20.0, 0), "out of reach");
}

#[test]
fn lightning_sets_fire_around_its_block_only_with_fire() {
    let fires = |lightning: Lightning| {
        let (mut state, mut output, _rx) = spawned([1]);
        let at = spawn_feet(&state, 0) + Vec3::new(0.0, 0.0, 6.0);
        let strike = BlockPos::containing(at);
        assert!(cx(&mut state).strike_lightning(WORLD, at, lightning));
        let mut found = Vec::new();
        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    let pos = BlockPos::new(strike.x + dx, strike.y + dy, strike.z + dz);
                    let b = state.worlds[0].block(pos).unwrap();
                    if b.kind() == block::Kind::Fire {
                        found.push(pos);
                    }
                }
            }
        }
        tick(&mut state, &mut Vec::new(), &mut output);
        (strike, found)
    };
    let (_, none) = fires(Lightning {
        damage: true,
        fire: false,
    });
    assert!(none.is_empty());
    let (strike, found) = fires(Lightning {
        damage: false,
        fire: true,
    });
    assert!(found.contains(&strike), "{found:?}");
    assert!(found.len() <= 5);
    // Fire stands only on the grass layer: air with a full block under it.
    assert!(found.iter().all(|p| p.y == strike.y), "{found:?}");
}

#[test]
fn lightning_sets_fire_at_normal_and_hard_difficulty_only_and_hurts_at_every_one() {
    let bolt = Lightning {
        damage: true,
        fire: true,
    };
    for (difficulty, burns) in [
        (Difficulty::Peaceful, false),
        (Difficulty::Easy, false),
        (Difficulty::Normal, true),
        (Difficulty::Hard, true),
    ] {
        let (mut state, mut output, _rx) = spawned([1]);
        let feet = spawn_feet(&state, 0);
        let at = feet + Vec3::new(2.0, 0.0, 0.0);
        let mut c = cx(&mut state);
        assert!(c.set_difficulty(difficulty));
        assert!(c.strike_lightning(WORLD, at, bolt));
        let player = c.player(player::Id::new(SessionId::new(1))).unwrap();
        assert_eq!(player.health(), 15.0, "{difficulty:?}: BDS's 5 damage");
        let fire = state.worlds[0]
            .block(BlockPos::containing(at))
            .unwrap()
            .kind()
            == block::Kind::Fire;
        assert_eq!(
            fire, burns,
            "{difficulty:?}: fire as BDS's bolt (P6 bolt scenarios)"
        );
        tick(&mut state, &mut Vec::new(), &mut output);
    }
}
