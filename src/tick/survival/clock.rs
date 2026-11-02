//! World clocks and sleeping (PMMP `World::$time`, `World::actuallyDoTick`, `World::checkSleep`,
//! `Bed::onInteract`, `Player::sleepOn`, `Player::stopSleep`).
//!
//! A world's time ([`time_ticks`](crate::world::World::time_ticks)) is BDS's `minecraft:overworld`
//! clock: it starts at 0 in a new world, counts on past a day, and is saved with the world record
//! at every commit, so a restart continues the day cycle at the tick it was saved. Its age
//! ([`age_ticks`](crate::world::World::age_ticks)), BDS's game time, counts every tick the world
//! runs. Clients run the day cycle themselves; BDS 1.26.52 corrects them with `SyncWorldClocks`
//! every 256 ticks or so and never sends `SetTime` (P6), so Anchor does the same, and again
//! whenever time jumps.

use tracing::debug;

use super::{Point, feet, first_session, next_session};
use crate::block::{self, Class, PropertyValue};
use crate::event::{PlayerBedEnter, PlayerBedLeave};
use crate::math::Vec3;
use crate::nbt::{self, Encoding};
use crate::player::Id;
use crate::protocol::consts::{AnimateAction, ClockPayloadType, HandSlot, LevelEvent};
use crate::protocol::packet::{Animate, LevelEventGeneric, Packet, SyncWorldClocks};
use crate::protocol::{SyncWorldClockStateData, TimeMarkerData, WorldClockData, Writer};
use crate::save::WorldIndex;
use crate::tick::{Phase, Player, SessionId, Settings, State, interact};
use crate::world::{self, BlockPos, DAY, World};

/// The `occupied_bit` of a bed nobody sleeps in.
const BED_FREE: PropertyValue<'static> = PropertyValue::Byte(0);
/// The `occupied_bit` of a bed someone sleeps in.
const BED_OCCUPIED: PropertyValue<'static> = PropertyValue::Byte(1);
/// The longest distance, squared, from the player's feet to a bed half (PMMP `Bed::onInteract`).
const BED_REACH_SQUARED: f64 = 4.0;
/// The overworld clock's time markers: ID, name, time of day (P6, fixed in every run).
pub(in crate::tick) const MARKERS: [(u64, &str, i32); 6] = [
    (
        2_625_810_911_898_139_949,
        "minecraft:sunrise",
        SUNRISE.cast_signed(),
    ),
    (
        4_918_950_784_056_990_566,
        "minecraft:night",
        NIGHT.cast_signed(),
    ),
    (6_827_470_627_776_846_754, "minecraft:noon", 6000),
    (11_262_090_321_339_182_944, "minecraft:midnight", 18_000),
    (13_638_948_813_458_750_018, "minecraft:day", 1000),
    (16_664_792_990_819_124_822, "minecraft:sunset", 12_000),
];
/// The time of day night starts at, and beds work from (PMMP `World::TIME_NIGHT`).
const NIGHT: u32 = 13_000;
/// The name of the bed property that tells whether someone sleeps in it.
const OCCUPIED_BIT: &str = "occupied_bit";
/// The overworld clock's ID, as BDS sends it in every run (P6).
const OVERWORLD_CLOCK: u64 = 7_194_480_507_151_251_734;
/// The name of the overworld clock, every world's (BDS's one clock).
pub(in crate::tick) const OVERWORLD_CLOCK_NAME: &str = "minecraft:overworld";
/// Ticks after someone lies down before the night may be skipped (PMMP `Player::sleepOn`).
const SLEEP_DELAY_TICKS: u32 = 60;
/// The time of day night ends at (PMMP `World::TIME_SUNRISE`).
const SUNRISE: u32 = 23_000;
/// Ticks between clock corrections: BDS sent them 257 ticks apart (P6 blocks capture).
const SYNC_TICKS: u32 = 256;

/// The world clocks' timers (each world keeps its own time).
#[derive(Debug, Default)]
pub(in crate::tick) struct Clock {
    /// One per world, by world index (grown to the world count every tick, and to a world newer
    /// than the last tick when its timers are first needed).
    worlds: Vec<WorldClock>,
}

impl Clock {
    /// Hashes every field (state hash).
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

    /// The timers of `world`, made first when the world is newer than the table.
    #[expect(
        clippy::expect_used,
        reason = "invariant: the table holds the world once it has grown"
    )]
    fn world_mut(&mut self, world: WorldIndex) -> &mut WorldClock {
        if self.worlds.len() <= world {
            let len = world.saturating_add(1);
            self.worlds.resize(len, WorldClock::default());
        }
        self.worlds.get_mut(world).expect("the world has a clock")
    }
}

crate::tick::checkpoint::fields!(Clock { worlds });

/// One world's clock timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct WorldClock {
    /// Ticks until the sleep check (PMMP `World::$sleepTicks`).
    sleep_ticks: u32,
    /// Ticks until the next clock correction.
    sync_in: u32,
}

crate::tick::checkpoint::fields!(WorldClock {
    sleep_ticks,
    sync_in
});

/// Where a player may lie down in a bed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// At any time of day and from any distance (the direct API).
    Anywhere,
    /// At night, within [`BED_REACH_SQUARED`] of either half (the player's click).
    Nearby,
}

/// Counts one tick of every loaded world's clock (PMMP `World::actuallyDoTick`): the world ages,
/// time advances unless it is stopped, a pending sleep check runs, and clients are corrected now
/// and then. With `daylight-cycle = false` no time passes anywhere, as with BDS's
/// `doDayLightCycle` false, whose game time still goes on (P6 `clocks`); clients are still
/// corrected, as BDS does.
pub(super) fn tick(state: &mut State) {
    let count = state.worlds.len();
    let daylight = state.settings.daylight_cycle;
    state.clock.worlds.resize(count, WorldClock::default());
    let mut any_corrected = false;
    for world in 0..count {
        if !state.catalog.is_loaded(world) || !state.is_awake(world) {
            continue;
        }
        if let Some(w) = state.worlds.get_mut(world) {
            w.add_age_tick();
            if daylight && !w.is_time_stopped() {
                w.set_time(w.time_ticks().wrapping_add(1));
            }
        }
        let c = state.clock.world_mut(world);
        let sync = c.sync_in == 0;
        c.sync_in = if sync { SYNC_TICKS - 1 } else { c.sync_in - 1 };
        let check = c.sleep_ticks > 0 && {
            c.sleep_ticks -= 1;
            c.sleep_ticks == 0
        };
        any_corrected |= sync;
        if check {
            check_sleep(state, world);
        }
    }
    if any_corrected {
        correct_clients(state);
    }
}

/// Lies the player of `session` down in the bed at `pos` (PMMP `Bed::onInteract` then
/// `Player::sleepOn`): only at night, within 2 blocks of either half of a whole bed nobody sleeps
/// in, and unless a [`PlayerBedEnter`] listener keeps it up. The bed shows occupied, becomes the
/// player's respawn point, and the night may be skipped in 60 ticks.
pub(super) fn sleep(
    state: &mut State,
    session: SessionId,
    world: WorldIndex,
    pos: BlockPos,
) -> bool {
    let Some((head, other)) = bed_at(state, session, world, pos, Reach::Nearby) else {
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
        |state, _| bed_at(state, session, world, pos, Reach::Nearby) == Some((head, other)),
    ) else {
        return false;
    };
    lie_down(state, session, world, pos, (head, other));
    super::watch(state, event.as_ref());
    true
}

/// Lies the player of `session` down in the bed at `pos` in its world, night or day and however
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
    let Some(bed) = bed_at(state, session, world, pos, Reach::Anywhere) else {
        return false;
    };
    lie_down(state, session, world, pos, bed);
    true
}

/// Wakes the player of `session`, if asleep; the [`PlayerBedLeave`] listeners and monitors hear
/// it once the bed is free.
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

/// Wakes a player that is not in the player map (it leaves): no event, the player is gone
/// ([`PlayerQuit`](crate::event::PlayerQuit) announced it).
pub(super) fn stop_sleep(state: &mut State, player: &mut Player) {
    let Some(bed) = player.survival.sleeping.take() else {
        return;
    };
    let world = player.world;
    player.data_dirty = true;
    after_wake(state, world, bed);
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
            name: OVERWORLD_CLOCK_NAME.to_owned(),
            time: packet_time(time(state, world)),
            paused: is_stopped(state, world),
            time_markers: MARKERS
                .iter()
                .map(|&(id, name, time)| TimeMarkerData {
                    id,
                    name: name.to_owned(),
                    time,
                    period: Some(packet_time(DAY)),
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
    for p in state.players.values().filter(|p| is_in_world(p, world)) {
        players = players.saturating_add(1);
        if p.survival.sleeping.is_some() {
            asleep = asleep.saturating_add(1);
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

/// The time of `world` (0 for a world that does not exist).
pub(in crate::tick) fn time(state: &State, world: WorldIndex) -> i64 {
    state.worlds.get(world).map_or(0, World::time_ticks)
}

/// The day cycle lock time of `StartGame`: the time of day the time stands still at with
/// `daylight-cycle = false`, as BDS sends it with `doDayLightCycle` false (P6 `rulesjoin`), 0
/// otherwise.
pub(in crate::tick) fn day_cycle_lock_time(settings: &Settings, world: &World) -> i32 {
    if settings.daylight_cycle {
        0
    } else {
        packet_time(i64::from(world.time_of_day()))
    }
}

/// Queues the clock correction for every spawned player whose world is corrected this tick: one
/// pass over the players however many worlds are corrected, as worlds made together correct
/// together.
fn correct_clients(state: &mut State) {
    // Taken out so the worlds can be read while the players are written; nothing in between
    // reads the player map.
    let mut players = std::mem::take(&mut state.players);
    for p in players.values_mut() {
        if p.phase == Phase::Spawned && is_corrected(state, p.world) {
            p.send_to_viewer(clock_state(state, p.world));
        }
    }
    state.players = players;
}

/// Whether `world`'s clients are corrected this tick: its clock ran and its correction timer
/// just started over.
fn is_corrected(state: &State, world: WorldIndex) -> bool {
    state.catalog.is_loaded(world)
        && state.is_awake(world)
        && state
            .clock
            .worlds
            .get(world)
            .is_some_and(|c| c.sync_in == SYNC_TICKS - 1)
}

/// Skips the night if every player in the world sleeps at night (PMMP `World::checkSleep`): the
/// time goes to the next morning and everyone wakes.
fn check_sleep(state: &mut State, world: WorldIndex) {
    let mut any = false;
    for p in state.players.values().filter(|p| is_in_world(p, world)) {
        any = true;
        if p.survival.sleeping.is_none() {
            return;
        }
    }
    let Some(w) = state.worlds.get_mut(world) else {
        return;
    };
    if !any || !is_night(w.time_of_day()) {
        return;
    }
    let now = w.time_ticks();
    w.set_time(now.wrapping_add(DAY - i64::from(w.time_of_day())));
    send_to_world_viewers(state, world, &clock_state(state, world));
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

/// Lies the player of `session` down in the bed whose head and other half are `bed`, `pos` being
/// one of them: the bed shows occupied and becomes its respawn point, and the night may be
/// skipped in 60 ticks.
#[expect(
    clippy::expect_used,
    reason = "invariant: the bed was found in this world by the caller"
)]
fn lie_down(
    state: &mut State,
    session: SessionId,
    world: WorldIndex,
    pos: BlockPos,
    (head, other): (BlockPos, BlockPos),
) {
    set_occupied(
        state,
        world,
        [head, block_at_other(head, pos, other)],
        BED_OCCUPIED,
    );
    let name = state
        .worlds
        .get(world)
        .expect("the bed's world exists")
        .name()
        .to_owned();
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
    state.clock.world_mut(world).sleep_ticks = SLEEP_DELAY_TICKS;
    send_to_world_viewers(state, world, &sleeping_players(state, world));
}

/// The head and the other half of the bed at `pos` the player of `session` may lie down in now
/// (PMMP `Bed::onInteract`): spawned, alive and awake in `world`, within `reach`, at a whole bed
/// nobody sleeps in.
fn bed_at(
    state: &State,
    session: SessionId,
    world: WorldIndex,
    pos: BlockPos,
    reach: Reach,
) -> Option<(BlockPos, BlockPos)> {
    let player = state.players.get(&session)?;
    let w = state.worlds.get(world)?;
    if player.phase != Phase::Spawned
        || player.world != world
        || player.survival.is_dead()
        || player.survival.sleeping.is_some()
        || (reach == Reach::Nearby && !is_night(w.time_of_day()))
    {
        return None;
    }
    let bed = w.block(pos).filter(|b| b.is(Class::Bed))?;
    let other = other_half(w, pos, bed)?;
    let at = feet(player);
    let near = |b: BlockPos| {
        at.distance_squared(Vec3::new(f64::from(b.x), f64::from(b.y), f64::from(b.z)))
            <= BED_REACH_SQUARED
    };
    if reach == Reach::Nearby && !near(pos) && !near(other) {
        return None;
    }
    let head = if is_head(bed) { pos } else { other };
    let occupied = w
        .block(head)
        .is_some_and(|b| b.value(OCCUPIED_BIT) == Some(BED_OCCUPIED));
    (!occupied).then_some((head, other))
}

/// The other half of a bed in `world` ([`interact::bed_other_half`]).
fn other_half(world: &World, pos: BlockPos, bed: block::State) -> Option<BlockPos> {
    interact::bed_other_half(world, pos, bed).map(|(other, _)| other)
}

/// Whether a bed block is the head half.
fn is_head(bed: block::State) -> bool {
    matches!(bed.value("head_piece_bit"), Some(PropertyValue::Byte(1)))
}

/// The half of a bed that is not `head`.
fn block_at_other(head: BlockPos, a: BlockPos, b: BlockPos) -> BlockPos {
    if a == head { b } else { a }
}

/// Sets the occupied bit of the bed halves that are still beds to `occupied`.
fn set_occupied(
    state: &mut State,
    world: WorldIndex,
    halves: [BlockPos; 2],
    occupied: PropertyValue<'_>,
) {
    let Some(w) = state.worlds.get_mut(world) else {
        return;
    };
    for pos in halves {
        let Some(b) = w.block(pos).filter(|b| b.is(Class::Bed)) else {
            continue;
        };
        if let Some(changed) = b.with(OCCUPIED_BIT, occupied)
            && changed != b
            && let Err(e) = w.set_block(pos, changed)
        {
            // A damaged chunk refuses changes: the bed keeps its look.
            debug!(
                ?pos,
                error = &e as &(dyn std::error::Error + 'static),
                "bed occupancy not changed"
            );
        }
    }
}

/// Tells the player it woke (PMMP `Player::stopSleep`: `Animate` stop sleep).
fn woke(player: &mut Player) {
    player.data_dirty = true;
    if player.phase == Phase::Spawned {
        let packet = Packet::Animate(Animate {
            action_type: u8::from(AnimateAction::StopSleep),
            entity_runtime_id: player.runtime_id,
            data: 0.0,
            swing_source: 0,
            hand: u8::from(HandSlot::MainHand),
        });
        player.send_packet(packet);
    }
}

/// Frees the bed after someone wakes, and lets the sleep check wait again for whoever still
/// sleeps. PMMP sets the wait to 0 on any wake, so the others' night was never skipped.
fn after_wake(state: &mut State, world: WorldIndex, bed: BlockPos) {
    let other = state.worlds.get(world).and_then(|w| {
        let half = w.block(bed).filter(|b| b.is(Class::Bed))?;
        other_half(w, bed, half)
    });
    set_occupied(state, world, [bed, other.unwrap_or(bed)], BED_FREE);
    let still = state
        .players
        .values()
        .any(|p| is_in_world(p, world) && p.survival.sleeping.is_some());
    state.clock.world_mut(world).sleep_ticks = if still { SLEEP_DELAY_TICKS } else { 0 };
    send_to_world_viewers(state, world, &sleeping_players(state, world));
}

/// Queues `packet` for every spawned player in `world`, as a viewer (`Player::send_to_viewer`):
/// one player's beds can send it again and again.
fn send_to_world_viewers(state: &mut State, world: WorldIndex, packet: &Packet) {
    for p in state.players.values_mut() {
        if p.world == world && p.phase == Phase::Spawned {
            p.send_to_viewer(packet.clone());
        }
    }
}

/// A compound's network NBT without the root tag's type and name (the event data BDS sends).
#[expect(
    clippy::expect_used,
    reason = "invariant: a small compound writes into a vector and starts with its root tag"
)]
fn compound_body(data: &nbt::Compound) -> Vec<u8> {
    let mut w = Writer::new(Vec::new(), 0);
    w.nbt(data, Encoding::NetworkLittleEndian)
        .expect("a compound writes into a vector");
    // The root is a compound type byte and an empty name (one zero-length byte).
    w.as_bytes()
        .get(2..)
        .expect("a written compound has its root tag")
        .to_vec()
}

/// Whether `world` is stopped (PMMP `World::stopTime`): clients then pause their clock too. With
/// `daylight-cycle = false` the time stands still as BDS's does with `doDayLightCycle` false: the
/// rule holds the client's clock, which is not paused (P6 `rules`).
fn is_stopped(state: &State, world: WorldIndex) -> bool {
    state.worlds.get(world).is_some_and(World::is_time_stopped)
}

/// Whether `player` is in `world` and has loaded: the players the sleep check and the sleeping
/// counts speak of.
fn is_in_world(player: &Player, world: WorldIndex) -> bool {
    player.world == world && player.phase != Phase::Loading
}

/// Whether beds work at `time_of_day`: 13,000 to 22,999 (PMMP `Bed::onInteract`).
fn is_night(time_of_day: u32) -> bool {
    (NIGHT..SUNRISE).contains(&time_of_day)
}

/// A time as the packets carry it (an `i32`, wrapping).
#[expect(
    clippy::expect_used,
    reason = "invariant: a remainder of 2^31 fits i32"
)]
fn packet_time(time: i64) -> i32 {
    i32::try_from(time.rem_euclid(1 << 31)).expect("a remainder of 2^31 fits i32")
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
    }

    #[test]
    fn packet_times_wrap_into_i32() {
        assert_eq!(packet_time(26), 26);
        assert_eq!(packet_time(-1), i32::MAX);
        assert_eq!(packet_time(DAY), 24_000);
    }

    #[test]
    fn the_clock_of_a_world_newer_than_the_table_is_made_on_demand() {
        let mut clock = Clock::default();
        clock.world_mut(2).sleep_ticks = SLEEP_DELAY_TICKS;
        assert_eq!(clock.worlds_len(), 3);
        assert_eq!(clock.world_mut(1).sleep_ticks, 0);
        assert_eq!(clock.world_mut(2).sleep_ticks, SLEEP_DELAY_TICKS);
        assert_eq!(clock.worlds_len(), 3);
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
