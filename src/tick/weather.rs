//! What clients are told of a world's weather, as the official server tells them (P6 `weather`,
//! `joinrain`, `joinstorm`): `StartGame`'s rain and lightning levels and one level event in the
//! spawn window at a join, and the level events of a change for the players already in the world
//! ([`Context::set_weather`](crate::Context::set_weather)) or arriving from another world.
//!
//! The official server raises and lowers its levels over about five seconds and sends what the
//! levels reached call for; Anchor's weather changes at once, which is the official weather once
//! it settled: a change starts or stops the rain when the rain changes and the thunder when the
//! thunder changes, a start with the full level and a stop with 0, rain first.

use crate::protocol::consts::LevelEvent;
use crate::protocol::packet::{LevelEvent as LevelEventPacket, Packet};
use crate::world::Weather;

/// The data of a start event: the full level, as the official server sends.
const FULL_LEVEL: i32 = 65_535;

/// The level events that move a client from `from` to `to`, in the official server's order (none
/// for the same weather).
pub(super) fn changes(from: Weather, to: Weather) -> impl Iterator<Item = Packet> {
    let rain = (rains(from) != rains(to)).then(|| {
        if rains(to) {
            level_event(LevelEvent::StartRaining, FULL_LEVEL)
        } else {
            level_event(LevelEvent::StopRaining, 0)
        }
    });
    let thunder = (thunders(from) != thunders(to)).then(|| {
        if thunders(to) {
            level_event(LevelEvent::StartThunderstorm, FULL_LEVEL)
        } else {
            level_event(LevelEvent::StopThunderstorm, 0)
        }
    });
    rain.into_iter().chain(thunder)
}

/// The level event of the spawn window, after `SetCommandsEnabled`: thunder's start in thunder
/// (the rain is in `StartGame`), the rain's start in rain.
pub(super) fn joined(weather: Weather) -> Option<Packet> {
    match weather {
        Weather::Clear => None,
        Weather::Rain => Some(level_event(LevelEvent::StartRaining, FULL_LEVEL)),
        Weather::Thunder => Some(level_event(LevelEvent::StartThunderstorm, FULL_LEVEL)),
    }
}

/// `StartGame`'s rain and lightning levels.
pub(super) fn levels(weather: Weather) -> (f32, f32) {
    let level = |on: bool| if on { 1.0 } else { 0.0 };
    (level(rains(weather)), level(thunders(weather)))
}

/// Whether it rains in `weather` (thunder rains too).
fn rains(weather: Weather) -> bool {
    weather != Weather::Clear
}

/// Whether it thunders in `weather`.
fn thunders(weather: Weather) -> bool {
    weather == Weather::Thunder
}

/// The level event of type `event` with `data`.
fn level_event(event: LevelEvent, data: i32) -> Packet {
    Packet::LevelEvent(LevelEventPacket {
        event_type: event.into(),
        position: [0.0; 3],
        event_data: data,
    })
}

#[cfg(test)]
mod tests;
