//! Lightning bolts, shown as the official server shows a summoned bolt.
//!
//! [`Context::strike_lightning`](crate::Context::strike_lightning) sends the
//! `minecraft:lightning_bolt` actor at the strike with its `thunder` and `explode` sounds to the
//! players who see the chunk, and the actor's removal a second later.
//!
//! The bolt is no entity of the store: it does nothing after its strike and nothing touches it,
//! so it is its packets and the removal it waits for ([`Bolt`]). What it does, it does at once
//! (the official server hurts in the strike's tick): with [`Lightning::damage`] the players and
//! living entities in the box from 3 blocks around and below the strike to 9 above it take 5
//! damage. A bolt sets nothing on fire. Like every direct call it raises no events.

use std::collections::BTreeSet;

use super::kind::Cause;
use super::{HOOKS, chunk_of, each_player_in, hurt_in, is_valid_position};
use crate::entity::Id;
use crate::math::{Aabb, Vec3};
use crate::protocol::consts::{EntityDataKey, SoundEvent};
use crate::protocol::packet::{AddActor, LevelSoundEvent, Packet};
use crate::protocol::{EntityMetadata, MetadataValue};
use crate::save::WorldIndex;
use crate::tick::checkpoint::fields;
use crate::tick::queue::SessionId;
use crate::tick::{State, survival};
use crate::world::Lightning;

/// The entity type of a bolt.
const BOLT: &str = "minecraft:lightning_bolt";
/// The damage of a bolt.
const DAMAGE: f64 = 5.0;
/// The most bolts waiting for their removal; a strike beyond it is refused.
pub(super) const MAX_BOLTS: usize = 1024;
/// How far around and below the strike a bolt hurts, in blocks.
const REACH: f64 = 3.0;
/// How far above the strike a bolt hurts, in blocks.
const REACH_UP: f64 = 9.0;

/// A bolt that was shown and waits for its removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Bolt {
    /// The tick of its removal.
    until: u64,
    /// Its runtime ID, which is also its unique ID.
    runtime_id: u64,
    /// The players it was shown to.
    viewers: BTreeSet<SessionId>,
}

impl Bolt {
    /// How long a bolt stays with its viewers, in ticks: the official server removes it between
    /// one and two seconds after the strike.
    const LIFETIME_TICKS: u64 = 20;

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            until,
            runtime_id,
            viewers,
        } = self;
        h.u64(*until);
        h.u64(*runtime_id);
        h.usize(viewers.len());
        for s in viewers {
            h.u64(s.get());
        }
    }
}

fields!(Bolt {
    until,
    runtime_id,
    viewers,
});

impl super::Entities {
    /// The bolts waiting for their removal.
    #[cfg(test)]
    pub(in crate::tick) fn bolts_waiting(&self) -> usize {
        self.bolts.len()
    }
}

/// Strikes a bolt at `at` in the loaded `world` with what `lightning` asks for.
/// False, with nothing done, for an `at` that is not finite or outside the world border,
/// [`MAX_BOLTS`] bolts waiting, or runtime IDs run out.
pub(super) fn strike(state: &mut State, world: WorldIndex, at: Vec3, lightning: Lightning) -> bool {
    if !is_valid_position(at) || state.rules.entities.bolts.len() >= MAX_BOLTS {
        return false;
    }
    let runtime_id = state.next_runtime_id;
    let Some(next) = runtime_id.checked_add(1) else {
        return false;
    };
    state.next_runtime_id = next;
    show(state, world, at, runtime_id);
    if lightning.damage {
        hurt(state, world, at);
    }
    true
}

/// Shows the bolt and its sounds to the players who see the chunk of `at`.
/// It keeps the bolt for its removal. A player whose outbox is nearly full is skipped, as for
/// what every viewer gets (`Player::has_viewer_room`).
fn show(state: &mut State, world: WorldIndex, at: Vec3, runtime_id: u64) {
    let position = at.to_f32();
    let unique = super::unique_id(runtime_id);
    let mut metadata = EntityMetadata::new();
    metadata.insert(EntityDataKey::Flags as u32, MetadataValue::Int64(0));
    metadata.insert(EntityDataKey::Scale as u32, MetadataValue::Float32(1.0));
    let actor = Packet::AddActor(Box::new(AddActor {
        entity_unique_id: unique,
        entity_runtime_id: runtime_id,
        entity_type: BOLT.to_owned(),
        position,
        entity_metadata: metadata,
        ..AddActor::default()
    }));
    let sound = |s: SoundEvent| {
        Packet::LevelSoundEvent(LevelSoundEvent {
            sound_type: s.as_str().to_owned(),
            position,
            extra_data: -1,
            entity_type: BOLT.to_owned(),
            baby_mob: false,
            disable_relative_volume: false,
            entity_unique_id: unique,
            fire_at_position: None,
        })
    };
    let packets = [
        actor,
        sound(SoundEvent::Thunder),
        sound(SoundEvent::Explode),
    ];
    let mut viewers = BTreeSet::new();
    let State {
        players, chunks, ..
    } = &mut *state;
    for session in chunks.viewers_of((world, chunk_of(at))) {
        if let Some(p) = players.get_mut(&session)
            && p.has_viewer_room()
        {
            for packet in &packets {
                p.send_packet(packet.clone());
            }
            viewers.insert(session);
        }
    }
    if !viewers.is_empty() {
        state.rules.entities.bolts.push_back(Bolt {
            until: state.tick.saturating_add(Bolt::LIFETIME_TICKS),
            runtime_id,
            viewers,
        });
    }
}

/// Sends the removal of every bolt whose time is up.
/// The players it was shown to that are still here get it, in any world: the bolt is no entity
/// their view takes away.
pub(super) fn remove_due(state: &mut State) {
    let State {
        rules,
        players,
        tick,
        ..
    } = state;
    let bolts = &mut rules.entities.bolts;
    while let Some(bolt) = bolts.pop_front_if(|b| b.until <= *tick) {
        let packet = super::track::remove(bolt.runtime_id);
        for session in &bolt.viewers {
            if let Some(p) = players.get_mut(session) {
                p.send_packet(packet.clone());
            }
        }
    }
}

/// Hurts the players and living entities near `at` with the bolt's damage.
fn hurt(state: &mut State, world: WorldIndex, at: Vec3) {
    let area = Aabb::new(
        at - Vec3::new(REACH, REACH, REACH),
        at + Vec3::new(REACH, REACH_UP, REACH),
    );
    let entities = &state.rules.entities;
    let living: Vec<Id> = entities
        .nearby(world, area)
        .into_iter()
        .filter(|&id| entities.get(id).is_some_and(|e| e.kind.is_living()))
        .collect();
    for id in living {
        let State { rules, players, .. } = &mut *state;
        hurt_in(
            &mut rules.entities,
            players,
            id,
            Cause::Lightning,
            DAMAGE,
            None,
        );
    }
    let mut struck: Vec<SessionId> = Vec::new();
    each_player_in(state, world, area, HOOKS, |s, _| {
        struck.push(s);
    });
    for session in struck {
        survival::damage_quiet(state, session, DAMAGE, survival::Damage::Lightning);
    }
}
