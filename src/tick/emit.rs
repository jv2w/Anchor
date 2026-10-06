//! Sounds and particles the game plays at a place (PMMP `World::addSound`, `World::addParticle`,
//! `Entity::broadcastSound`): the one way rule code plays a [`Sound`] or a [`Particle`], which
//! raises [`WorldSound`] or [`WorldParticle`] when something listens. The players who see the
//! place's chunk hear and see it; a listener may change what plays or narrow who gets it, and those
//! it names who do not see the place are left out (PMMP `filterViewersForPosition`).
//!
//! A direct API call raises no event: [`Context::add_sound`](crate::Context::add_sound) does not
//! come here, and a rule that runs for a direct call (an explosion of `Context::explode`) plays
//! with [`Raise::Quiet`]. Packets BDS sends that no PMMP sound or particle makes (block
//! cracking, bone meal's growth sparkles, the generic particles of a projectile's hit, an
//! explosion's level event) and the sounds of an entity (landing, falling: `world::Sound` leaves
//! them out) are sent where they happen, without an event.

use super::entity::Raise;
use super::view::chunk_of;
use super::{SessionId, State, dispatch};
use crate::event::{WorldParticle, WorldSound};
use crate::math::Vec3;
use crate::protocol::packet::Packet;
use crate::save::WorldIndex;
use crate::world::{self, Particle, Sound};

/// Plays `sound` at `at` in `world` for the game, with [`WorldSound`].
pub(super) fn sound(state: &mut State, world: WorldIndex, at: Vec3, sound: Sound) {
    sound_as(state, world, at, sound, Raise::Events);
}

/// Plays `sound` at `at` in `world`, with [`WorldSound`] if `raise` says so.
pub(super) fn sound_as(state: &mut State, world: WorldIndex, at: Vec3, sound: Sound, raise: Raise) {
    if raise == Raise::Quiet || !dispatch::heard::<WorldSound>(state) {
        state.broadcast(world, chunk_of(at), &sound.packet(at), None);
        return;
    }
    let mut e = WorldSound {
        world: world::Id::of(world),
        position: at,
        sound,
        recipients: viewers(state, world, at),
        cancelled: false,
    };
    dispatch::call(state, &mut e);
    if e.cancelled {
        return;
    }
    send(state, world, at, &e.sound.packet(at), &e.recipients);
    dispatch::watch(state, &e);
}

/// Shows `particle` at `at` in `world` for the game, with [`WorldParticle`].
pub(super) fn particle(state: &mut State, world: WorldIndex, at: Vec3, particle: Particle) {
    if !dispatch::heard::<WorldParticle>(state) {
        state.broadcast(world, chunk_of(at), &particle.packet(at), None);
        return;
    }
    let mut e = WorldParticle {
        world: world::Id::of(world),
        position: at,
        particle,
        recipients: viewers(state, world, at),
        cancelled: false,
    };
    dispatch::call(state, &mut e);
    if e.cancelled {
        return;
    }
    send(state, world, at, &e.particle.packet(at), &e.recipients);
    dispatch::watch(state, &e);
}

/// The players who see the chunk of `at` in `world`, in session order.
fn viewers(state: &State, world: WorldIndex, at: Vec3) -> Vec<crate::player::Id> {
    let chunk = chunk_of(at);
    state
        .players
        .iter()
        .filter(|(_, p)| p.world == world && p.view.sees(chunk))
        .map(|(&s, _)| crate::player::Id::new(s))
        .collect()
}

/// Sends `packet` to those of `recipients` who see the chunk of `at` in `world`, once each.
fn send(
    state: &mut State,
    world: WorldIndex,
    at: Vec3,
    packet: &Packet,
    recipients: &[crate::player::Id],
) {
    let chunk = chunk_of(at);
    let mut sessions: Vec<SessionId> = recipients.iter().map(|p| p.session()).collect();
    sessions.sort_unstable();
    sessions.dedup();
    for session in sessions {
        if let Some(p) = state.players.get_mut(&session)
            && p.world == world
            && p.view.sees(chunk)
        {
            p.send_packet(packet.clone());
        }
    }
}
