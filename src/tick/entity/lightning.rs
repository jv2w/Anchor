//! Lightning bolts: [`Context::strike_lightning`](crate::Context::strike_lightning) shows a bolt as
//! BDS's `summon lightning_bolt` does (P6 `weather`): the `minecraft:lightning_bolt` actor at the
//! strike with its `thunder` and `explode` sounds to the players who see the chunk, and the actor's
//! removal a second later.
//!
//! The bolt is no entity of the store: it does nothing after its strike and nothing touches it,
//! so it is its packets and the removal it waits for ([`Bolt`]). What it does, it does at once
//! (BDS hurts in the strike's tick, P6): with [`Lightning::damage`] the players and living
//! entities in the box from 3 blocks around and below the strike to 9 above it (dragonfly
//! `lightningState.dealDamage`, the box of Java's bolt) burn for 8 seconds and take 5 damage;
//! with [`Lightning::fire`] the strike's block and 4 random blocks next to it catch fire where
//! fire can stand (dragonfly `spreadFire`, each try from the strike's block as in Java), at
//! normal and hard difficulty only, as BDS (P6 `boltpeaceful`, `bolteasy`, `bolthard`). Like
//! every direct call it raises no events.

use std::collections::BTreeSet;

use super::explosion::set_fire;
use super::kind::Cause;
use super::{EPSILON, HOOKS, chunk_of, hurt_in, is_valid_position, player_box, targetable, with};
use crate::block;
use crate::config::Difficulty;
use crate::entity::Id;
use crate::math::{Aabb, Vec3};
use crate::protocol::consts::{EntityDataKey, SoundEvent};
use crate::protocol::packet::{AddActor, LevelSoundEvent, Packet};
use crate::protocol::{EntityMetadata, MetadataValue};
use crate::random::{self, Random};
use crate::save::WorldIndex;
use crate::tick::queue::SessionId;
use crate::tick::{State, survival, world_api};
use crate::world::{BlockPos, Lightning};

/// The entity type of a bolt.
const BOLT: &str = "minecraft:lightning_bolt";
/// How long a bolt stays with its viewers: BDS removes it between one and two seconds after the
/// strike (P6), dragonfly after one.
const LIFETIME_TICKS: u64 = 20;
/// The most bolts waiting for their removal; a strike beyond it is refused.
pub(super) const MAX_BOLTS: usize = 1024;
/// The damage of a bolt (BDS: 5, P6 `weather`).
const DAMAGE: f64 = 5.0;
/// How long a struck player or entity burns (dragonfly and Java: 8 seconds).
const BURN_SECONDS: i16 = 8;
/// How far around and below the strike a bolt hurts, and how far above.
const REACH: f64 = 3.0;
const REACH_UP: f64 = 9.0;
/// The fires a bolt tries to start next to its block (dragonfly, Java).
const FIRE_TRIES: usize = 4;
/// The random stream of a bolt's fires.
const FIRE_RANDOM: u64 = random::tag("lightning-fire");

/// A bolt that was shown and waits for its removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Bolt {
    /// The tick of its removal.
    until: u64,
    runtime_id: u64,
    /// The players it was shown to.
    viewers: BTreeSet<SessionId>,
}

crate::tick::checkpoint::fields!(Bolt {
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

impl Bolt {
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

/// Strikes a bolt at `at` in the loaded `world` with what `lightning` asks for. False, with
/// nothing done, for an `at` that is not finite or outside the world border, [`MAX_BOLTS`] bolts
/// waiting, or runtime IDs run out.
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
    if lightning.fire {
        ignite(state, world, at, runtime_id);
    }
    true
}

/// Sends the bolt and its sounds to the players who see the chunk of `at` and keeps it for its
/// removal. A player whose outbox is nearly full is skipped, as for what every viewer gets
/// (`Player::has_viewer_room`).
fn show(state: &mut State, world: WorldIndex, at: Vec3, runtime_id: u64) {
    let position = at.to_f32();
    let unique = runtime_id.cast_signed();
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
    let chunk = chunk_of(at);
    let mut viewers = BTreeSet::new();
    for (&session, p) in &mut state.players {
        if p.world == world && p.view.sees(chunk) && p.has_viewer_room() {
            for packet in &packets {
                p.send_packet(packet.clone());
            }
            viewers.insert(session);
        }
    }
    if !viewers.is_empty() {
        state.rules.entities.bolts.push_back(Bolt {
            until: state.tick.saturating_add(LIFETIME_TICKS),
            runtime_id,
            viewers,
        });
    }
}

/// Sends the removal of every bolt whose time is up to the players it was shown to that are
/// still here (in any world: the bolt is no entity their view takes away).
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

/// The players and living entities near `at` burn and take the bolt's damage.
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
        with(state, id, |e| e.set_on_fire(BURN_SECONDS));
        let State { rules, players, .. } = &mut *state;
        hurt_in(
            &mut rules.entities,
            players,
            id,
            Cause::Custom,
            DAMAGE,
            None,
        );
    }
    let struck: Vec<SessionId> = state
        .players
        .iter()
        .filter(|(_, p)| {
            targetable(state, p, world, HOOKS) && player_box(p).overlaps(area, EPSILON)
        })
        .map(|(&s, _)| s)
        .collect();
    for session in struck {
        if let Some(p) = state.players.get_mut(&session) {
            p.set_on_fire(BURN_SECONDS.unsigned_abs().into());
        }
        survival::damage_quiet(state, session, DAMAGE, survival::Damage::Lightning);
    }
}

/// Sets fire in the block of `at` and tries [`FIRE_TRIES`] random blocks next to it, at normal
/// and hard difficulty only: BDS's bolt sets no fire at peaceful and easy (P6 `boltpeaceful`,
/// `bolteasy`; dragonfly and Java the same) and hurts at every difficulty.
fn ignite(state: &mut State, world: WorldIndex, at: Vec3, runtime_id: u64) {
    if matches!(
        world_api::difficulty(state, world),
        Difficulty::Peaceful | Difficulty::Easy
    ) {
        return;
    }
    let Some(seed) = state.worlds.get(world).map(crate::world::World::seed) else {
        return;
    };
    let key = random::key(FIRE_RANDOM, &[runtime_id]);
    let mut random = Random::new(seed, state.tick, key);
    let strike = BlockPos::containing(at);
    let flame = block::Kind::Fire.default_state();
    set_fire(state, world, strike, flame);
    for _ in 0..FIRE_TRIES {
        let mut near = || random.range(-1, 1);
        let (dx, dy, dz) = (near(), near(), near());
        let pos = BlockPos::new(
            strike.x.saturating_add(dx),
            strike.y.saturating_add(dy),
            strike.z.saturating_add(dz),
        );
        set_fire(state, world, pos, flame);
    }
}
