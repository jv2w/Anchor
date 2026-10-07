//! World clocks and sleeping (PMMP `World::$time`, `World::actuallyDoTick`, `World::checkSleep`,
//! `Bed::onInteract`, `Player::sleepOn`, `Player::stopSleep`).
//!
//! A world's time ([`time_ticks`](crate::world::World::time_ticks)) counts ticks from its creation
//! (BDS starts a new world at 0) and is saved with the world record at every commit, so a restart
//! continues the day cycle at the tick it was saved. Clients run the day cycle themselves; BDS
//! 1.26.52 corrects them with `SyncWorldClocks` every 256 ticks or so and never sends `SetTime`
//! (P6), so Anchor does the same, and again whenever time jumps.

use super::{Point, feet, first_session, next_session};
use tracing::debug;

use crate::block::{self, Class, PropertyValue};
use crate::event::{PlayerBedEnter, PlayerBedLeave};
use crate::nbt::{self, Encoding};
use crate::player::Id;
use crate::protocol::consts::{AnimateAction, ClockPayloadType, LevelEvent};
use crate::protocol::packet::{Animate, LevelEventGeneric, Packet, SyncWorldClocks};
use crate::protocol::{SyncWorldClockStateData, TimeMarkerData, WorldClockData, Writer};
use crate::save::WorldIndex;
use crate::tick::{Phase, Player, SessionId, State, interact};
use crate::world::{self, BlockPos, DAY};

/// When night starts and beds work (PMMP `World::TIME_NIGHT`).
const NIGHT: i64 = 13_000;
/// When night ends (PMMP `World::TIME_SUNRISE`).
const SUNRISE: i64 = 23_000;
/// Ticks between clock corrections: BDS sent them 257 ticks apart (P6 blocks capture).
const SYNC_TICKS: u32 = 256;
/// Ticks after someone lies down before the night may be skipped (PMMP `Player::sleepOn`).
const SLEEP_DELAY: u32 = 60;
/// The longest distance, squared, from the player's feet to a bed half (PMMP `Bed::onInteract`).
const BED_REACH_SQUARED: f64 = 4.0;
/// The overworld clock's ID, as BDS sends it in every run (P6).
const OVERWORLD_CLOCK: u64 = 7_194_480_507_151_251_734;
/// The overworld clock's time markers: ID, name, time of day (P6, fixed in every run).
const MARKERS: [(u64, &str, i32); 6] = [
    (2_625_810_911_898_139_949, "minecraft:sunrise", 23_000),
    (4_918_950_784_056_990_566, "minecraft:night", 13_000),
    (6_827_470_627_776_846_754, "minecraft:noon", 6_000),
    (11_262_090_321_339_182_944, "minecraft:midnight", 18_000),
    (13_638_948_813_458_750_018, "minecraft:day", 1_000),
    (16_664_792_990_819_124_822, "minecraft:sunset", 12_000),
];

/// The world clocks' timers (each world keeps its own time).
#[derive(Debug, Default)]
pub(in crate::tick) struct Clock {
    /// One per world, by world index (grown to the world count on the first tick).
    worlds: Vec<WorldClock>,
}

/// One world's clock timers.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct WorldClock {
    /// Ticks until the sleep check (PMMP `World::$sleepTicks`).
    sleep_ticks: u32,
    /// Ticks until the next clock correction.
    sync_in: u32,
}

crate::tick::checkpoint::fields!(Clock { worlds });
crate::tick::checkpoint::fields!(WorldClock {
    sleep_ticks,
    sync_in
});

impl Clock {
    #[cfg(test)]
    pub(in crate::tick) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self { worlds } = self;
        h.usize(worlds.len());
        for c in worlds {
            let WorldClock {
                sleep_ticks,
                sync_in,
            } = c;
            h.u64(u64::from(*sleep_ticks));
            h.u64(u64::from(*sync_in));
        }
    }

    /// The number of world clocks.
    #[cfg(test)]
    pub(in crate::tick) fn worlds_len(&self) -> usize {
        self.worlds.len()
    }
}

/// The time of `world` (0 for a world that does not exist).
pub(in crate::tick) fn time(state: &State, world: WorldIndex) -> i64 {
    state
        .worlds
        .get(world)
        .map_or(0, crate::world::World::time_ticks)
}

/// Whether the time of `world` was stopped there (PMMP `World::stopTime`): clients then pause
/// their clock too. With `daylight-cycle = false` the time stands still as BDS's does with
/// `doDayLightCycle` false: the rule holds the client's clock, which is not paused (P6 `rules`).
fn is_stopped(state: &State, world: WorldIndex) -> bool {
    state
        .worlds
        .get(world)
        .is_some_and(crate::world::World::is_time_stopped)
}

/// The day cycle lock time of `StartGame`: the time of day the time stands still at with
/// `daylight-cycle = false`, as BDS sends it with `doDayLightCycle` false (P6 `rulesjoin`), 0
/// otherwise.
pub(in crate::tick) fn day_cycle_lock_time(settings: &crate::tick::Settings, time: i64) -> i32 {
    if settings.daylight_cycle {
        0
    } else {
        packet_time(time_of_day(time))
    }
}

/// The time of day: 0 to 23,999 (PMMP `World::getTimeOfDay`).
fn time_of_day(time: i64) -> i64 {
    time.rem_euclid(DAY)
}

/// Whether beds work: 13,000 to 22,999 (PMMP `Bed::onInteract`).
fn is_night(time: i64) -> bool {
    (NIGHT..SUNRISE).contains(&time_of_day(time))
}

/// A time as the packets carry it (an `i32`, wrapping).
fn packet_time(time: i64) -> i32 {
    i32::try_from(time.rem_euclid(1 << 31)).unwrap_or(0)
}

/// One tick of every loaded world's clock (PMMP `World::actuallyDoTick`): time advances unless it
/// is stopped, clients are corrected now and then, and a pending sleep check runs. With
/// `daylight-cycle = false` no time passes anywhere; clients are still corrected, as BDS does.
pub(super) fn tick(state: &mut State) {
    let count = state.worlds.len();
    let daylight = state.settings.daylight_cycle;
    state.clock.worlds.resize(count, WorldClock::default());
    for world in 0..count {
        if !state.catalog.is_loaded(world) || !state.is_awake(world) {
            continue;
        }
        if daylight
            && let Some(w) = state.worlds.get_mut(world)
            && !w.is_time_stopped()
        {
            w.set_time(w.time_ticks().wrapping_add(1));
        }
        let Some(c) = state.clock.worlds.get_mut(world) else {
            continue;
        };
        let sync = {
            let sync = c.sync_in == 0;
            c.sync_in = if sync { SYNC_TICKS - 1 } else { c.sync_in - 1 };
            sync
        };
        let check = c.sleep_ticks > 0 && {
            c.sleep_ticks -= 1;
            c.sleep_ticks == 0
        };
        if sync {
            send_to_world(state, world, &clock_state(state, world));
        }
        if check {
            check_sleep(state, world);
        }
    }
}

/// `SyncWorldClocks` with the world's time (payload 0, as BDS sends before `StartGame` and to
/// correct clients).
pub(in crate::tick) fn clock_state(state: &State, world: WorldIndex) -> Packet {
    Packet::SyncWorldClocks(SyncWorldClocks {
        payload_type: u32::from(ClockPayloadType::SyncState),
        sync_states: vec![SyncWorldClockStateData {
            clock_id: OVERWORLD_CLOCK,
            time: packet_time(time(state, world)),
            paused: is_stopped(state, world),
        }],
        ..SyncWorldClocks::default()
    })
}

/// `SyncWorldClocks` defining the world's clock and its time markers (payload 1, as BDS sends
/// after `SetSpawnPosition`).
pub(in crate::tick) fn clock_definition(state: &State, world: WorldIndex) -> Packet {
    Packet::SyncWorldClocks(SyncWorldClocks {
        payload_type: u32::from(ClockPayloadType::InitializeRegistry),
        clocks: vec![WorldClockData {
            id: OVERWORLD_CLOCK,
            name: "minecraft:overworld".to_owned(),
            time: packet_time(time(state, world)),
            paused: is_stopped(state, world),
            time_markers: MARKERS
                .iter()
                .map(|&(id, name, time)| TimeMarkerData {
                    id,
                    name: name.to_owned(),
                    time,
                    period: Some(i32::try_from(DAY).unwrap_or(i32::MAX)),
                })
                .collect(),
        }],
        ..SyncWorldClocks::default()
    })
}

/// `LevelEventGeneric` with the sleeping counts of a world (BDS sends it before `StartGame`):
/// players who must sleep (all of them), players in the world, players asleep.
pub(in crate::tick) fn sleeping_players(state: &State, world: WorldIndex) -> Packet {
    let (mut players, mut asleep) = (0_i32, 0_i32);
    for p in state.players.values() {
        if p.world == world && p.phase != Phase::Loading {
            players = players.saturating_add(1);
            if p.survival.sleeping.is_some() {
                asleep = asleep.saturating_add(1);
            }
        }
    }
    let mut data = nbt::Compound::new();
    data.insert("ableToSleep".to_owned(), nbt::Value::Int(players));
    data.insert("overworldPlayerCount".to_owned(), nbt::Value::Int(players));
    data.insert("sleepingPlayerCount".to_owned(), nbt::Value::Int(asleep));
    Packet::LevelEventGeneric(LevelEventGeneric {
        event_id: i32::from(LevelEvent::SleepingPlayers),
        serialised_event_data: compound_body(&data),
    })
}

/// A compound's network NBT without the root tag's type and name (the event data BDS sends).
fn compound_body(data: &nbt::Compound) -> Vec<u8> {
    let mut w = Writer::new(Vec::new(), 0);
    if w.nbt(data, Encoding::NetworkLittleEndian).is_err() {
        return Vec::new();
    }
    // The root is a compound type byte and an empty name (one zero-length byte).
    w.as_bytes()
        .get(2..)
        .map(<[u8]>::to_vec)
        .unwrap_or_default()
}

/// Queues `packet` for every spawned player in `world`, as a viewer (`Player::send_to_viewer`):
/// one player's beds can send it again and again.
fn send_to_world(state: &mut State, world: WorldIndex, packet: &Packet) {
    for p in state.players.values_mut() {
        if p.world == world && p.phase == Phase::Spawned {
            p.send_to_viewer(packet.clone());
        }
    }
}

/// The sleep check (PMMP `World::checkSleep`): if every player in the world sleeps at night, the
/// time skips to the next morning and everyone wakes.
fn check_sleep(state: &mut State, world: WorldIndex) {
    let mut any = false;
    for p in state.players.values() {
        if p.world != world || p.phase == Phase::Loading {
            continue;
        }
        any = true;
        if p.survival.sleeping.is_none() {
            return;
        }
    }
    let now = time(state, world);
    if !any || !is_night(now) {
        return;
    }
    if let Some(w) = state.worlds.get_mut(world) {
        w.set_time(now.wrapping_add(DAY - time_of_day(now)));
    }
    send_to_world(state, world, &clock_state(state, world));
    let mut next = first_session(state);
    while let Some(session) = next {
        next = next_session(state, session);
        if state
            .players
            .get(&session)
            .is_some_and(|p| p.world == world)
        {
            wake(state, session);
        }
    }
}

/// The other half of a bed in `world` ([`interact::bed_other_half`]).
fn other_half(
    state: &State,
    world: WorldIndex,
    pos: BlockPos,
    bed: block::State,
) -> Option<BlockPos> {
    let w = state.worlds.get(world)?;
    interact::bed_other_half(w, pos, bed).map(|(other, _)| other)
}

/// Whether a bed block is the head half.
fn is_head(bed: block::State) -> bool {
    matches!(bed.value("head_piece_bit"), Some(PropertyValue::Byte(1)))
}

/// The player lies down in the bed at `pos` (PMMP `Bed::onInteract` then `Player::sleepOn`):
/// only at night, within 2 blocks of either half of a whole bed nobody sleeps in, and unless a
/// [`PlayerBedEnter`] listener keeps it up. The bed shows occupied, becomes the player's respawn
/// point, and the night may be skipped in 60 ticks.
pub(super) fn sleep(
    state: &mut State,
    session: SessionId,
    world: WorldIndex,
    pos: BlockPos,
) -> bool {
    let Some((head, other)) = bed_for(state, session, world, pos) else {
        return false;
    };
    let Ok(event) = super::ask(
        state,
        || PlayerBedEnter {
            player: Id::new(session),
            world: world::Id::of(world),
            bed: head,
            cancelled: false,
        },
        PlayerBedEnter::is_cancelled,
        |state, _| bed_for(state, session, world, pos) == Some((head, other)),
    ) else {
        return false;
    };
    lie_down(state, session, world, pos, (head, other));
    super::watch(state, event.as_ref());
    true
}

/// The player of `session` lies down in the bed at `pos` in its world, night or day and however
/// far (PMMP `Player::sleepOn`, without its event: a direct API call raises none). False unless
/// it is spawned, alive and awake, and `pos` is a half of a whole bed nobody sleeps in; always
/// false with `beds = false`.
pub(super) fn sleep_on(state: &mut State, session: SessionId, pos: BlockPos) -> bool {
    if !state.settings.beds {
        return false;
    }
    let Some(world) = state.players.get(&session).map(|p| p.world) else {
        return false;
    };
    let Some(bed) = bed_at(state, session, world, pos, false) else {
        return false;
    };
    lie_down(state, session, world, pos, bed);
    true
}

/// The player lies down in the bed whose head and other half are `bed`, `pos` being one of
/// them: the bed shows occupied and becomes its respawn point, and the night may be skipped in
/// 60 ticks.
fn lie_down(
    state: &mut State,
    session: SessionId,
    world: WorldIndex,
    pos: BlockPos,
    (head, other): (BlockPos, BlockPos),
) {
    set_occupied(state, world, [head, block_at_other(head, pos, other)], true);
    let name = state
        .worlds
        .get(world)
        .map(|w| w.name().to_owned())
        .unwrap_or_default();
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    player.survival.sleeping = Some(head);
    player.survival.motion.in_bed = true;
    player.survival.spawn = Some(Point {
        world: name,
        pos: head,
    });
    player.data_dirty = true;
    if let Some(c) = state.clock.worlds.get_mut(world) {
        c.sleep_ticks = SLEEP_DELAY;
    }
    send_to_world(state, world, &sleeping_players(state, world));
}

/// The head and the other half of the bed at `pos` the player of `session` may lie down in now
/// (PMMP `Bed::onInteract`): spawned, alive and awake in `world` at night, within 2 blocks of
/// either half of a whole bed nobody sleeps in.
fn bed_for(
    state: &State,
    session: SessionId,
    world: WorldIndex,
    pos: BlockPos,
) -> Option<(BlockPos, BlockPos)> {
    bed_at(state, session, world, pos, true)
}

/// [`bed_for`], checking the night and the reach only when `nearby_at_night`.
fn bed_at(
    state: &State,
    session: SessionId,
    world: WorldIndex,
    pos: BlockPos,
    nearby_at_night: bool,
) -> Option<(BlockPos, BlockPos)> {
    let player = state.players.get(&session)?;
    if player.phase != Phase::Spawned
        || player.world != world
        || player.survival.is_dead()
        || player.survival.sleeping.is_some()
        || (nearby_at_night && !is_night(time(state, world)))
    {
        return None;
    }
    let bed = state.worlds.get(world).and_then(|w| w.block(pos))?;
    if !bed.is(Class::Bed) {
        return None;
    }
    let other = other_half(state, world, pos, bed)?;
    let at = feet(player);
    let near = |b: BlockPos| {
        at.distance_squared(crate::math::Vec3::new(
            f64::from(b.x),
            f64::from(b.y),
            f64::from(b.z),
        )) <= BED_REACH_SQUARED
    };
    if nearby_at_night && !near(pos) && !near(other) {
        return None;
    }
    let head = if is_head(bed) { pos } else { other };
    let occupied = state
        .worlds
        .get(world)
        .and_then(|w| w.block(head))
        .is_some_and(|b| matches!(b.value("occupied_bit"), Some(PropertyValue::Byte(1))));
    (!occupied).then_some((head, other))
}

/// The half of a bed that is not `head`.
fn block_at_other(head: BlockPos, a: BlockPos, b: BlockPos) -> BlockPos {
    if a == head { b } else { a }
}

/// Sets or clears the occupied bit of the bed halves that are still beds.
fn set_occupied(state: &mut State, world: WorldIndex, halves: [BlockPos; 2], occupied: bool) {
    let Some(w) = state.worlds.get_mut(world) else {
        return;
    };
    for pos in halves {
        let Some(b) = w.block(pos) else {
            continue;
        };
        if !b.is(Class::Bed) {
            continue;
        }
        if let Some(changed) = b.with("occupied_bit", PropertyValue::Byte(u8::from(occupied)))
            && changed != b
            && let Err(e) = w.set_block(pos, changed)
        {
            // A damaged chunk refuses changes: the bed keeps its look.
            debug!(error = %e, x = pos.x, y = pos.y, z = pos.z, "bed occupancy not changed");
        }
    }
}

/// The player of `session` wakes up, if asleep; the [`PlayerBedLeave`] listeners and monitors
/// hear it once the bed is free.
pub(super) fn wake(state: &mut State, session: SessionId) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    player.survival.motion.in_bed = false;
    let Some(bed) = player.survival.sleeping.take() else {
        return;
    };
    let world = player.world;
    woke(player);
    after_wake(state, world, bed);
    super::notify(state, || PlayerBedLeave {
        player: Id::new(session),
        world: world::Id::of(world),
        bed,
    });
}

/// A player that is not in the player map wakes up (it leaves): no event, the player is gone
/// ([`PlayerQuit`](crate::event::PlayerQuit) announced it).
pub(super) fn stop_sleep(state: &mut State, player: &mut Player) {
    let Some(bed) = player.survival.sleeping.take() else {
        return;
    };
    let world = player.world;
    player.data_dirty = true;
    after_wake(state, world, bed);
}

/// The player is told it woke (PMMP `Player::stopSleep`: `Animate` stop sleep).
fn woke(player: &mut Player) {
    player.data_dirty = true;
    if player.phase == Phase::Spawned {
        let packet = Packet::Animate(Animate {
            action_type: u8::from(AnimateAction::StopSleep),
            entity_runtime_id: player.runtime_id,
            data: 0.0,
            swing_source: 0,
        });
        player.send_packet(packet);
    }
}

/// After someone wakes: the bed is free, and the sleep check waits again for whoever still
/// sleeps. PMMP sets the wait to 0 on any wake, so the others' night was never skipped.
fn after_wake(state: &mut State, world: WorldIndex, bed: BlockPos) {
    let other = state
        .worlds
        .get(world)
        .and_then(|w| w.block(bed))
        .filter(|b| b.is(Class::Bed))
        .and_then(|b| other_half(state, world, bed, b));
    set_occupied(state, world, [bed, other.unwrap_or(bed)], false);
    let still = state
        .players
        .values()
        .any(|p| p.world == world && p.survival.sleeping.is_some());
    if let Some(c) = state.clock.worlds.get_mut(world) {
        c.sleep_ticks = if still { SLEEP_DELAY } else { 0 };
    }
    send_to_world(state, world, &sleeping_players(state, world));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn night_is_from_13000_to_22999() {
        assert!(!is_night(12_999));
        assert!(is_night(13_000));
        assert!(is_night(22_999));
        assert!(!is_night(23_000));
        assert!(is_night(DAY * 5 + 18_000));
        assert_eq!(packet_time(26), 26);
        assert_eq!(packet_time(-1), i32::MAX);
    }

    #[test]
    fn the_sleeping_event_data_is_the_bds_compound_body() {
        let mut data = nbt::Compound::new();
        data.insert("ableToSleep".to_owned(), nbt::Value::Int(1));
        data.insert("overworldPlayerCount".to_owned(), nbt::Value::Int(1));
        data.insert("sleepingPlayerCount".to_owned(), nbt::Value::Int(0));
        // BDS 1.26.52 (P6), base64 "AwthYmxlVG9TbGVlcAIDFG92ZXJ3b3JsZFBsYXllckNvdW50AgMTc2xlZXBpbmdQbGF5ZXJDb3VudAAA".
        let mut want = vec![3, 11];
        want.extend(b"ableToSleep");
        want.extend([2, 3, 20]);
        want.extend(b"overworldPlayerCount");
        want.extend([2, 3, 19]);
        want.extend(b"sleepingPlayerCount");
        want.extend([0, 0]);
        assert_eq!(compound_body(&data), want);
    }
}
