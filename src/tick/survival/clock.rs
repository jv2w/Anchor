//! World clocks: a world's time and age, and the corrections clients get.
//!
//! A world's time ([`time_ticks`](crate::world::World::time_ticks)) is the official server's
//! `minecraft:overworld` clock: it starts at 0 in a new world, counts on past a day, and is saved
//! with the world record at every commit, so a restart continues the day cycle at the tick it was
//! saved. Its age ([`age_ticks`](crate::world::World::age_ticks)), the official server's game
//! time, counts every tick the world runs. Clients run the day cycle themselves; the official
//! server corrects them with `SyncWorldClocks` every 256 ticks or so and never sends `SetTime`
//! (P6), so Anchor does the same, and again whenever time jumps.

use crate::protocol::consts::ClockPayloadType;
use crate::protocol::packet::{Packet, SyncWorldClocks};
use crate::protocol::{SyncWorldClockStateData, TimeMarkerData, WorldClockData};
use crate::save::WorldIndex;
use crate::tick::{Phase, Settings, State};
use crate::world::{DAY, World};

/// The overworld clock's time markers: ID, name, time of day (P6, fixed in every run).
const MARKERS: [(u64, &str, i32); 6] = [
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
/// The time of day night starts at.
const NIGHT: u32 = 13_000;
/// The overworld clock's ID, as the official server sends it in every run (P6).
const OVERWORLD_CLOCK: u64 = 7_194_480_507_151_251_734;
/// The name of the overworld clock, every world's (the official server's one clock).
const OVERWORLD_CLOCK_NAME: &str = "minecraft:overworld";
/// The time of day night ends at.
const SUNRISE: u32 = 23_000;
/// Ticks between clock corrections: the official server sent them 257 ticks apart (P6 blocks
/// capture).
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
            let WorldClock { sync_in } = c;
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
    /// Ticks until the next clock correction.
    sync_in: u32,
}

crate::tick::checkpoint::fields!(WorldClock { sync_in });

/// Counts one tick of every loaded world's clock: the world ages, time advances unless it is
/// stopped, and clients are corrected now and then. With `daylight-cycle = false` no time passes
/// anywhere, as with the official server's `doDayLightCycle` false, whose game time still goes
/// on (P6 `clocks`); clients are still corrected, as the official server does.
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
        any_corrected |= sync;
    }
    if any_corrected {
        correct_clients(state);
    }
}

/// `SyncWorldClocks` with the world's time (payload 0, as the official server sends before
/// `StartGame` and to correct clients).
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

/// `SyncWorldClocks` defining the world's clock and its time markers (payload 1, as the official
/// server sends after `SetSpawnPosition`).
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

/// The time of `world` (0 for a world that does not exist).
fn time(state: &State, world: WorldIndex) -> i64 {
    state.worlds.get(world).map_or(0, World::time_ticks)
}

/// The day cycle lock time of `StartGame`: the time of day the time stands still at with
/// `daylight-cycle = false`, as the official server sends it with `doDayLightCycle` false (P6
/// `rulesjoin`), 0 otherwise.
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

/// Whether `world`'s time is stopped through the API: clients then pause their clock too. With
/// `daylight-cycle = false` the time stands still as the official server's does with
/// `doDayLightCycle` false: the rule holds the client's clock, which is not paused (P6 `rules`).
fn is_stopped(state: &State, world: WorldIndex) -> bool {
    state.worlds.get(world).is_some_and(World::is_time_stopped)
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
    fn packet_times_wrap_into_i32() {
        assert_eq!(packet_time(26), 26);
        assert_eq!(packet_time(-1), i32::MAX);
        assert_eq!(packet_time(DAY), 24_000);
    }

    #[test]
    fn the_clock_of_a_world_newer_than_the_table_is_made_on_demand() {
        let mut clock = Clock::default();
        clock.world_mut(2).sync_in = SYNC_TICKS;
        assert_eq!(clock.worlds_len(), 3);
        assert_eq!(clock.world_mut(1).sync_in, 0);
        assert_eq!(clock.world_mut(2).sync_in, SYNC_TICKS);
        assert_eq!(clock.worlds_len(), 3);
    }
}
