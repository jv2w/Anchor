//! Sounds and particles the game plays at a place: the one way rule code plays a [`Sound`] or a [`Particle`], which
//! raises [`WorldSound`] or [`WorldParticle`] when something listens. The players who see the
//! place's chunk hear and see it; a listener may change what plays or narrow who gets it, and those
//! it names who do not see the place are left out.
//!
//! A direct API call raises no event: [`Context::add_sound`](crate::Context::add_sound) does not
//! come here, and a rule that runs for a direct call (an explosion of `Context::explode`) plays
//! with [`Raise::Quiet`]. Packets the official server sends that no sound or particle makes (block
//! cracking, the generic particles of a projectile's hit, an
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
    let mut event = WorldSound {
        world: world::Id::of(world),
        position: at,
        sound,
        recipients: viewers(state, world, at),
        cancelled: false,
    };
    dispatch::call(state, &mut event);
    if event.cancelled {
        return;
    }
    send(state, world, at, &event.sound.packet(at), &event.recipients);
    dispatch::watch(state, &event);
}

/// Shows `particle` at `at` in `world` for the game, with [`WorldParticle`].
pub(super) fn particle(state: &mut State, world: WorldIndex, at: Vec3, particle: Particle) {
    if !dispatch::heard::<WorldParticle>(state) {
        state.broadcast(world, chunk_of(at), &particle.packet(at), None);
        return;
    }
    let mut event = WorldParticle {
        world: world::Id::of(world),
        position: at,
        particle,
        recipients: viewers(state, world, at),
        cancelled: false,
    };
    dispatch::call(state, &mut event);
    if event.cancelled {
        return;
    }
    send(
        state,
        world,
        at,
        &event.particle.packet(at),
        &event.recipients,
    );
    dispatch::watch(state, &event);
}

/// The players who see the chunk of `at` in `world`, in session order.
fn viewers(state: &State, world: WorldIndex, at: Vec3) -> Vec<crate::player::Id> {
    state
        .chunk_viewers(world, chunk_of(at))
        .map(crate::player::Id::new)
        .collect()
}

/// Sends `packet` to those of `recipients` who see the chunk of `at` in `world`, once each, as
/// viewers.
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
            && p.view.can_see(chunk)
        {
            p.send_to_viewer(packet.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::event::{Listeners, Priority};
    use crate::tick::tests::{initialised, join, new_state, radius, received, settings, tick};
    use crate::tick::{Context, Output};

    /// The packets player 1 gets in the tick after `play` ran in a server with `listeners`, the
    /// player standing at the world spawn.
    fn played(listeners: Listeners, play: fn(&mut State, Vec3)) -> Vec<Packet> {
        let mut state = new_state(settings(|_| {}), listeners);
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j], &mut output);
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
        received(&mut rx);
        let spawn = state.worlds[0].spawn();
        let at = Vec3::new(
            f64::from(spawn.x) + 0.5,
            f64::from(spawn.y),
            f64::from(spawn.z) + 0.5,
        );
        play(&mut state, at);
        tick(&mut state, &mut Vec::new(), &mut output);
        received(&mut rx).0
    }

    fn play_sound(state: &mut State, at: Vec3) {
        sound(state, 0, at, Sound::Explode);
    }

    fn play_particle(state: &mut State, at: Vec3) {
        particle(state, 0, at, Particle::AngryVillager);
    }

    /// Listeners and monitors for sounds and particles that change nothing.
    fn empty_listeners() -> Listeners {
        let mut listeners = Listeners::default();
        listeners.push(
            Priority::Normal,
            |_: &mut WorldSound, _: &mut Context<'_>| {},
        );
        listeners.push_monitor(|_: &WorldSound, _: &Context<'_>| {});
        listeners.push(
            Priority::Normal,
            |_: &mut WorldParticle, _: &mut Context<'_>| {},
        );
        listeners.push_monitor(|_: &WorldParticle, _: &Context<'_>| {});
        listeners
    }

    #[test]
    fn an_empty_listener_and_no_listener_play_the_same_packets() {
        let idle = played(Listeners::default(), |_, _| {});
        for play in [play_sound, play_particle] {
            let plain = played(Listeners::default(), play);
            assert_ne!(plain, idle, "the packets play");
            assert_eq!(played(empty_listeners(), play), plain);
        }
    }
}
