//! The sounds of survival events, as PMMP plays them (`Entity::broadcastSound`,
//! `Player::broadcastSound`: the player and every player that sees it hear the sound at its
//! feet): experience pickup and level-up, the totem, burping after food, landing and falling,
//! items breaking, and melee hits.
//!
//! BDS was seen sending the experience pickup sound (`LevelEvent` 1051, m7-facts.md 3); the other
//! sounds follow PMMP's packets (verification needed (P6): BDS's sounds for the other events).

use super::{broadcast_from, feet};
use crate::math::Vec3;
use crate::protocol::consts::{LevelEvent, SoundEvent};
use crate::protocol::packet::{self, Packet};
use crate::tick::{SessionId, State};

/// The entity type PMMP names in the sounds a player makes (`Human::getNetworkTypeId`).
const PLAYER_TYPE: &str = "minecraft:player";

/// A level event at the player's feet (PMMP `XpCollectSound`, `TotemUseSound`).
pub(super) fn level_event(state: &mut State, session: SessionId, event: LevelEvent) {
    let Some(at) = state.players.get(&session).map(feet) else {
        return;
    };
    let packet = Packet::LevelEvent(packet::LevelEvent {
        event_type: event.into(),
        position: at.to_f32(),
        event_data: 0,
    });
    broadcast_from(state, session, &packet);
}

/// A sound at the player's feet that no entity makes (PMMP `LevelSoundEventPacket::
/// nonActorSound`: `XpLevelUpSound`, `BurpSound`, `ItemBreakSound`).
pub(super) fn plain(state: &mut State, session: SessionId, sound: SoundEvent, extra: i32) {
    let Some(at) = state.players.get(&session).map(feet) else {
        return;
    };
    broadcast_from(state, session, &sound_packet(sound, at, extra, ":", -1));
}

/// A sound the player makes (PMMP `EntityLandSound`, `EntityLongFallSound`,
/// `EntityShortFallSound`): it names the player's entity type and runtime ID.
pub(super) fn of_player(state: &mut State, session: SessionId, sound: SoundEvent, extra: i32) {
    let Some((at, runtime_id)) = state.players.get(&session).map(|p| (feet(p), p.runtime_id))
    else {
        return;
    };
    let id = i64::try_from(runtime_id).unwrap_or(i64::MAX);
    broadcast_from(
        state,
        session,
        &sound_packet(sound, at, extra, PLAYER_TYPE, id),
    );
}

/// A player's hit sound at `at`, which every player who sees `session`'s chunk hears (PMMP
/// `World::addSound` at the hit player: `EntityAttackSound`, `EntityAttackNoDamageSound`, which
/// name the player type but no entity).
pub(super) fn hit(state: &mut State, session: SessionId, at: Vec3, sound: SoundEvent) {
    broadcast_from(
        state,
        session,
        &sound_packet(sound, at, -1, PLAYER_TYPE, -1),
    );
}

/// The `LevelSoundEvent` packet. A sound of no entity has the entity type `:` and unique ID -1
/// (PMMP `LevelSoundEventPacket::nonActorSound`).
fn sound_packet(sound: SoundEvent, at: Vec3, extra: i32, entity_type: &str, id: i64) -> Packet {
    Packet::LevelSoundEvent(packet::LevelSoundEvent {
        sound_type: sound.as_str().to_owned(),
        position: at.to_f32(),
        extra_data: extra,
        entity_type: entity_type.to_owned(),
        baby_mob: false,
        disable_relative_volume: false,
        entity_unique_id: id,
        fire_at_position: None,
    })
}

/// The extra data of the level-up sound (PMMP `XpLevelUpSound`): higher levels play it louder,
/// in steps of 5 levels up to 30.
pub(super) fn level_up_data(level: u32) -> i32 {
    let step = i32::try_from(level.min(30) / 5).unwrap_or(6);
    0x1000_0000_i32.saturating_mul(step)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_up_volume_follows_pmmp() {
        assert_eq!(level_up_data(0), 0);
        assert_eq!(level_up_data(5), 0x1000_0000);
        assert_eq!(level_up_data(29), 0x5000_0000);
        assert_eq!(level_up_data(30), 0x6000_0000);
        assert_eq!(level_up_data(24_791), 0x6000_0000);
    }
}
