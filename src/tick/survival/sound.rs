//! The sounds of survival events, as PMMP plays them (`Entity::broadcastSound`,
//! `Player::broadcastSound`: the player and every player that sees it hear the sound at its
//! feet): xp pickup and level-up, the totem, burping after food, landing and falling,
//! items breaking, and melee hits. All but the landing and falling sounds of the player (which
//! name the player, and `world::Sound` leaves out) go through `tick::emit` with their
//! [`WorldSound`](crate::event::WorldSound).
//!
//! BDS was seen sending the xp pickup sound (`LevelEvent` 1051); the other sounds follow
//! PMMP's packets.

use super::{broadcast_from, feet};
use crate::math::Vec3;
use crate::protocol::consts::SoundEvent;
use crate::protocol::packet::{self, Packet};
use crate::tick::{SessionId, State, emit, entity};
use crate::world::{PLAYER_ENTITY_TYPE, Sound};

/// Plays a sound at the player's feet (PMMP `XpCollectSound`, `TotemUseSound`,
/// `XpLevelUpSound`, `BurpSound`, `ItemBreakSound`).
pub(super) fn at_feet(state: &mut State, session: SessionId, sound: Sound) {
    let Some((world, at)) = state.players.get(&session).map(|p| (p.world, feet(p))) else {
        return;
    };
    emit::sound(state, world, at, sound);
}

/// Plays a sound the player makes (PMMP `EntityLandSound`, `EntityLongFallSound`,
/// `EntityShortFallSound`): it names the player's entity type and runtime ID.
pub(super) fn of_player(state: &mut State, session: SessionId, sound: SoundEvent, extra: i32) {
    let Some((at, runtime_id)) = state.players.get(&session).map(|p| (feet(p), p.runtime_id))
    else {
        return;
    };
    let packet = Packet::LevelSoundEvent(packet::LevelSoundEvent {
        sound_type: sound.as_str().to_owned(),
        position: at.to_f32(),
        extra_data: extra,
        entity_type: PLAYER_ENTITY_TYPE.to_owned(),
        baby_mob: false,
        disable_relative_volume: false,
        entity_unique_id: entity::unique_id(runtime_id),
        fire_at_position: None,
    });
    broadcast_from(state, session, &packet);
}

/// Plays a player's hit sound at `at` in the world of `session` (PMMP `World::addSound` at the
/// hit player or entity: `EntityAttackSound`, `EntityAttackNoDamageSound`).
pub(super) fn hit(state: &mut State, session: SessionId, at: Vec3, sound: Sound) {
    let Some(world) = state.players.get(&session).map(|p| p.world) else {
        return;
    };
    emit::sound(state, world, at, sound);
}
