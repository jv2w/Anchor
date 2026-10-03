//! Entities on the tick: the store of every world, physics, tracking and the per-kind rules
//! (dropped items, experience orbs, falling blocks, primed TNT and explosions, projectiles,
//! potion clouds, NPCs) (PLAN.md 10.7 "엔티티", PMMP `Entity`, `ItemEntity`, `ExperienceOrb`,
//! `FallingBlock`, `PrimedTNT`, `Explosion`, `Projectile` and its kinds, `AreaEffectCloud`,
//! `Living`, `Human`, `Villager`, `World::dropItem`, `World::dropExperience`).
//!
//! # Store
//!
//! One store holds the entities of every world in slots: an [`Id`] is a slot and the slot's
//! generation, so a stale `Id` never finds a later entity. Iteration is in `Id` order. A chunk
//! index (world and chunk to the entities in it) answers "what is near" in the chunks around a
//! box, and a runtime ID index answers packets. Each world holds at most
//! [`MAX_ENTITIES_PER_WORLD`]; at the cap a new entity is refused and the caller keeps its items.
//!
//! Entities and players share the runtime ID counter (`State::next_runtime_id`; the unique ID is
//! the runtime ID). Runtime IDs are numbers of one run and are not saved: an entity created in
//! the tick gets one at once, and an entity loaded with its chunk (where the tick state is not at
//! hand) gets one at the start of the next entity tick, in `Id` order, before anything can see it.
//!
//! # Tick (PLAN.md 5)
//!
//! Entities in chunks within `tick-radius` of a player tick (like blocks; the rest wait).
//! 1. Compute (parallel, read-only): each entity's movement against the blocks
//!    ([`physics::step`]) on the CPU pool.
//! 2. Apply (serial, `Id` order): the new bodies (a projectile first hits the nearest entity or
//!    player on its ray, else the block the ray met), the blocks they are inside, burning, the
//!    void, then the kind's rules: items count down their pickup delay, merge with equal stacks
//!    around them and despawn after 6,000 ticks; orbs follow the nearest player within 8 blocks
//!    and give it their experience; falling blocks land as blocks or drop their item; primed TNT
//!    burns down and explodes ([`explosion`], within a per-tick budget); stuck arrows go after a
//!    minute; clouds shrink and give their effects; NPCs count down their hit cooldown and die.
//!    An explosion changes blocks and pushes entities, so the entities near it that have not
//!    moved yet this tick move again from their new state (serially), as PMMP's one-by-one
//!    update would. Then players pick up the items and stuck arrows they touch (what does not
//!    fit stays on the ground).
//! 3. Removals, then tracking: players who see an entity's chunk are sent it, its movement and
//!    its removal; players who see each other's chunk are sent each other. Every kind of packet
//!    has a per-tick bound, and what is over it waits for the next tick.
//!
//! # Saving
//!
//! Each loaded chunk's entities are its extras of kind 2 ([`saved`]). Compounds that are not an
//! entity Anchor knows, and entities over the world's cap, are kept as saved and written back.
//! A chunk changes when one of its entities ticks, arrives, leaves or is removed. Entities of
//! a chunk that unloads leave the store with it; viewers are sent their removal next tick.
//! Temporary worlds are never saved (the snapshot skips them).

mod explosion;
mod kind;
mod npc;
mod physics;
mod projectile;
mod saved;
mod track;

use std::collections::{BTreeMap, BTreeSet};

use tracing::{debug, warn};

use self::kind::{Cause, Character, DEFAULT_DESPAWN_DELAY, Falling, Form, Item, Kind, NEVER, Orb};
#[cfg(test)]
use self::kind::{Cloud, Projectile, Shot, Tnt};
use self::physics::{Body, Inside, Step};
use super::view::{Key, chunk_of};
use super::{Phase, Player, SessionId, State, inventory, survival};
use crate::block::{self, Class};
use crate::config::GameMode;
use crate::entity::Id;
use crate::item::Stack;
use crate::math::{Aabb, Vec3};
use crate::nbt::Compound;
use crate::pool::Pool;
use crate::protocol::consts::{ActorEvent, LevelEvent, SoundEvent};
use crate::protocol::packet::{
    ActorEvent as ActorEventPacket, LevelEvent as LevelEventPacket,
    LevelSoundEvent as LevelSoundEventPacket, Packet, TakeItemActor,
};
use crate::random::{self, Random};
use crate::save::WorldIndex;
use crate::world::{BlockPos, ChunkPos, MAX_Y, MIN_Y, World};

/// The most entities one world holds. 16,384 keeps the tick of a world full of falling items
/// within a few milliseconds (measured in `tick_cost`, PLAN.md 10.7) and the store's memory near
/// 6 MiB; PMMP and dragonfly have no cap. At the cap, actions that would create an entity are
/// refused and the caller keeps its items.
pub(super) const MAX_ENTITIES_PER_WORLD: usize = 16_384;
/// Spawn and removal packets one player is sent per tick; the rest wait (a view entering a chunk
/// full of items fills in over ticks instead of overflowing the outbox).
const MAX_SPAWNS_PER_PLAYER: u32 = 512;
/// Entities whose movement is sent per tick (each up to three packets per viewer); the others
/// keep their last sent state and are sent in a later tick, in turn.
const MAX_MOVEMENTS_PER_TICK: usize = 256;
/// Item merges per tick; later candidates check again two ticks on.
const MAX_MERGES_PER_TICK: usize = 256;
/// Entities removed per tick (despawned, picked up, burnt, merged); the rest are removed in the
/// next ticks and are ignored meanwhile.
const MAX_REMOVALS_PER_TICK: usize = 512;
/// Items one player picks up per tick.
const MAX_PICKUPS_PER_PLAYER: usize = 32;
/// The pickup delay of a dropped item, in ticks (PMMP `World::dropItem`).
const DROP_PICKUP_DELAY: i16 = 10;
/// Ticks between an item's merge checks (PMMP `MERGE_CHECK_PERIOD`).
const MERGE_CHECK_PERIOD: i32 = 2;
/// How far an orb looks for a player to follow (PMMP `MAX_TARGET_DISTANCE`).
const ORB_TARGET_DISTANCE: f64 = 8.0;
/// Ticks between an orb's searches for a player (PMMP `lookForTargetTime`).
const ORB_LOOK_PERIOD: u8 = 20;
/// The orb sizes a drop of experience splits into, largest first (PMMP `ORB_SPLIT_SIZES`).
const ORB_SPLIT_SIZES: [u16; 11] = [2477, 1237, 617, 307, 149, 73, 37, 17, 7, 3, 1];
/// The player's box and eye height (PMMP `Human`: 1.8 × 0.6, eyes at 1.62).
pub(super) const PLAYER_WIDTH: f64 = 0.6;
pub(super) const PLAYER_HEIGHT: f64 = 1.8;
pub(super) const PLAYER_EYE_HEIGHT: f64 = 1.62;
/// How far around a player's box it picks items up (PMMP `checkNearEntities`).
const PICKUP_REACH: Vec3 = Vec3::new(1.0, 0.5, 1.0);
/// How far around an item's box it looks for stacks to merge with.
const MERGE_REACH: Vec3 = Vec3::new(0.5, 0.5, 0.5);
/// The overlap two boxes need to touch (PMMP `intersectsWith`).
const EPSILON: f64 = 0.000_01;
/// The fall damage of an anvil per block fallen and at most (PMMP `Anvil`).
const ANVIL_DAMAGE_PER_BLOCK: f64 = 2.0;
const ANVIL_MAX_DAMAGE: f64 = 40.0;
/// The anvil blocks from whole to most damaged; one more hit on the last breaks it.
const ANVILS: [&str; 3] = [
    "minecraft:anvil",
    "minecraft:chipped_anvil",
    "minecraft:damaged_anvil",
];
/// The world border (as for players).
const MAX_COORDINATE: f64 = 30_000_000.0;
/// Random streams (`crate::random`), one per purpose.
const DROP_RANDOM: u64 = random::tag("entity-drop");
const UNSTICK_RANDOM: u64 = random::tag("entity-unstick");
const ANVIL_RANDOM: u64 = random::tag("entity-anvil-land");

/// One entity.
#[derive(Debug, Clone)]
struct Entity {
    /// 0 until the tick gives it one (loaded entities).
    runtime_id: u64,
    world: WorldIndex,
    /// The chunk the index lists it in: the chunk of its position.
    chunk: ChunkPos,
    body: Body,
    /// A block changed nearby: it moves at the next tick even at rest.
    forced: bool,
    /// Degrees; values a float holds exactly, as saved.
    yaw: f64,
    pitch: f64,
    health: f64,
    /// Ticks it keeps burning.
    fire_ticks: i16,
    /// Ticks it has been ticked.
    age: u32,
    kind: Kind,
    /// Gone: removed at the end of a tick, ignored until then.
    flagged: bool,
    /// Its motion was zero at the last movement check (PMMP `noClientPredictions`).
    still: bool,
    /// The players it was sent to.
    viewers: BTreeSet<SessionId>,
    /// What the viewers were last sent.
    sent: Sent,
}

/// The state players were last sent of an entity's movement and flags (PMMP `lastLocation`,
/// `lastMotion`).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Sent {
    position: Vec3,
    motion: Vec3,
    yaw: f64,
    pitch: f64,
    flags: u64,
    /// A cloud's radius (0 for other kinds).
    radius: f64,
}

/// What a hit did to an entity ([`Entity::attack`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hurt {
    /// Nothing: the kind ignores the cause, or a weaker hit came within an NPC's cooldown.
    Ignored,
    /// It took the damage; `cold` when an NPC's cooldown was over (knockback and the hurt
    /// animation follow, PMMP `Living::attack`).
    Took { cold: bool },
    /// It died: an NPC shows its death, anything else is removed.
    Died,
}

/// A knockback (PMMP `Living::knockBack`): away along (`x`, `z`) with `force`, up at most
/// `vertical`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Knock {
    x: f64,
    z: f64,
    force: f64,
    vertical: f64,
}

/// PMMP `Living::DEFAULT_KNOCKBACK_FORCE` and `DEFAULT_KNOCKBACK_VERTICAL_LIMIT`.
const KNOCKBACK: f64 = 0.4;

impl Knock {
    /// The default knockback away along (`x`, `z`).
    fn new(x: f64, z: f64) -> Self {
        Self {
            x,
            z,
            force: KNOCKBACK,
            vertical: KNOCKBACK,
        }
    }

    /// The motion of something moving at `motion` after this knockback; `None` without a
    /// horizontal direction (PMMP `knockBack`; knockback resistance is 0 for entities and
    /// players here).
    fn applied_to(self, motion: Vec3) -> Option<Vec3> {
        let f = (self.x * self.x + self.z * self.z).sqrt();
        if f.is_nan() || f <= 0.0 {
            return None;
        }
        let f = 1.0 / f;
        let y = motion.y / 2.0 + self.force;
        Some(Vec3::new(
            motion.x / 2.0 + self.x * f * self.force,
            crate::math::min(y, self.vertical),
            motion.z / 2.0 + self.z * f * self.force,
        ))
    }
}

impl Entity {
    /// A new entity at `body` (not yet in the store).
    fn new(world: WorldIndex, body: Body, yaw: f64, pitch: f64, kind: Kind) -> Self {
        let health = kind.max_health();
        let radius = match &kind {
            Kind::Cloud(c) => c.radius,
            _ => 0.0,
        };
        let mut e = Self {
            runtime_id: 0,
            world,
            chunk: chunk_of(body.position),
            body,
            forced: false,
            yaw,
            pitch,
            health,
            fire_ticks: 0,
            age: 0,
            kind,
            flagged: false,
            still: false,
            viewers: BTreeSet::new(),
            sent: Sent {
                position: body.position,
                motion: body.motion,
                yaw,
                pitch,
                flags: 0,
                radius,
            },
        };
        e.sent.flags = track::flags(&e.kind, false, false);
        e
    }

    /// Its box (PMMP `boundingBox`).
    fn bounding_box(&self) -> Aabb {
        let (width, height) = self.kind.size();
        let half = width / 2.0;
        let p = self.body.position;
        let y = p.y + self.body.y_size;
        Aabb::new(
            Vec3::new(p.x - half, y, p.z - half),
            Vec3::new(p.x + half, y + height, p.z + half),
        )
    }

    /// Damage (PMMP `Entity::attack`, and `Living::attack`'s cooldown for NPCs, without events):
    /// health goes down unless the kind ignores the cause.
    fn attack(&mut self, cause: Cause, amount: f64) -> Hurt {
        if !self.kind.takes(cause) || amount.is_nan() || amount < 0.0 {
            return Hurt::Ignored;
        }
        let Kind::Npc(c) = &mut self.kind else {
            self.health -= amount;
            return if self.health <= 0.0 {
                Hurt::Died
            } else {
                Hurt::Took { cold: false }
            };
        };
        if c.dead_ticks.is_some() {
            return Hurt::Ignored;
        }
        let cold = c.attack_time == 0;
        let dealt = if cold {
            amount
        } else if c.last_damage >= amount {
            return Hurt::Ignored;
        } else {
            amount - c.last_damage
        };
        c.last_damage = amount;
        if cold {
            c.attack_time = npc::ATTACK_COOLDOWN;
        }
        self.health = crate::math::max(self.health - dealt, 0.0);
        if self.health <= 0.0 {
            c.dead_ticks = Some(0);
            Hurt::Died
        } else {
            Hurt::Took { cold }
        }
    }

    /// Sets it on fire for `seconds` unless it burns longer already (PMMP `setOnFire`).
    fn set_on_fire(&mut self, seconds: i16) {
        let ticks = seconds.saturating_mul(20);
        if ticks > self.fire_ticks && !self.kind.is_fire_proof() {
            self.fire_ticks = ticks;
        }
    }

    /// One tick of burning (PMMP `doOnFireTick`): true when a burn's damage is due.
    fn burn(&mut self) -> bool {
        if self.fire_ticks <= 0 {
            return false;
        }
        if self.kind.is_fire_proof() {
            self.fire_ticks = 0;
            return false;
        }
        self.fire_ticks -= 1;
        self.fire_ticks % 20 == 0
    }

    #[cfg(test)]
    #[expect(
        clippy::too_many_lines,
        reason = "consistency: every field and kind is hashed in one place, destructured"
    )]
    fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            runtime_id,
            world,
            chunk,
            body,
            forced,
            yaw,
            pitch,
            health,
            fire_ticks,
            age,
            kind,
            flagged,
            still,
            viewers,
            sent,
        } = self;
        h.u64(*runtime_id);
        h.usize(*world);
        h.u64(u64::from(chunk.x.cast_unsigned()));
        h.u64(u64::from(chunk.z.cast_unsigned()));
        let Body {
            position,
            motion,
            on_ground,
            fall_distance,
            y_size,
        } = body;
        for v in [*position, *motion] {
            for c in [v.x, v.y, v.z] {
                h.u64(c.to_bits());
            }
        }
        h.u64(u64::from(*on_ground));
        h.u64(fall_distance.to_bits());
        h.u64(y_size.to_bits());
        h.u64(u64::from(*forced));
        h.u64(yaw.to_bits());
        h.u64(pitch.to_bits());
        h.u64(health.to_bits());
        h.u64(u64::from(fire_ticks.cast_unsigned()));
        h.u64(u64::from(*age));
        match kind {
            Kind::Item(Item {
                stack,
                pickup_delay,
                despawn_delay,
                owner,
                thrower,
            }) => {
                h.u64(0);
                super::inventory::hash_stack(h, stack);
                h.u64(u64::from(pickup_delay.cast_unsigned()));
                h.u64(u64::from(despawn_delay.cast_unsigned()));
                h.bytes(owner.as_bytes());
                h.bytes(thrower.as_bytes());
            }
            Kind::ExperienceOrb(Orb {
                value,
                despawn_delay,
                look_for_target,
                target,
            }) => {
                h.u64(1);
                h.u64(u64::from(*value));
                h.u64(u64::from(despawn_delay.cast_unsigned()));
                h.u64(u64::from(*look_for_target));
                h.u64(target.map_or(0, |s| s.get().wrapping_add(1)));
            }
            Kind::FallingBlock(Falling { block }) => {
                h.u64(2);
                h.u64(u64::from(block.network_id()));
            }
            Kind::Tnt(Tnt { fuse, underwater }) => {
                h.u64(3);
                h.u64(u64::from(*fuse));
                h.u64(u64::from(*underwater));
            }
            Kind::Projectile(p) => {
                h.u64(4);
                hash_projectile(h, p);
            }
            Kind::Cloud(c) => {
                h.u64(5);
                hash_cloud(h, c);
            }
            Kind::Npc(c) => {
                h.u64(6);
                hash_character(h, c);
            }
        }
        h.u64(u64::from(*flagged));
        h.u64(u64::from(*still));
        h.usize(viewers.len());
        for s in viewers {
            h.u64(s.get());
        }
        let Sent {
            position,
            motion,
            yaw,
            pitch,
            flags,
            radius,
        } = sent;
        for v in [*position, *motion] {
            for c in [v.x, v.y, v.z] {
                h.u64(c.to_bits());
            }
        }
        h.u64(yaw.to_bits());
        h.u64(pitch.to_bits());
        h.u64(*flags);
        h.u64(radius.to_bits());
    }
}

#[cfg(test)]
fn hash_projectile(h: &mut super::Fnv, p: &Projectile) {
    let Projectile {
        shot,
        damage,
        stuck,
        owner,
        owner_runtime_id,
    } = p;
    match shot {
        Shot::Arrow(kind::Arrow {
            critical,
            pickup,
            punch,
            life,
        }) => {
            h.u64(0);
            h.u64(u64::from(*critical));
            h.u64(u64::from(pickup.to_byte()));
            h.u64(u64::from(*punch));
            h.u64(u64::from(life.cast_unsigned()));
        }
        Shot::Snowball => h.u64(1),
        Shot::Egg => h.u64(2),
        Shot::EnderPearl => h.u64(3),
        Shot::Potion { potion, linger } => {
            h.u64(4);
            h.u64(u64::from(potion.id()));
            h.u64(u64::from(*linger));
        }
        Shot::ExperienceBottle => h.u64(5),
    }
    h.u64(damage.to_bits());
    match stuck {
        Some(p) => {
            h.u64(1);
            for c in [p.x, p.y, p.z] {
                h.u64(u64::from(c.cast_unsigned()));
            }
        }
        None => h.u64(0),
    }
    h.u64(owner.map_or(0, |s| s.get().wrapping_add(1)));
    h.u64(*owner_runtime_id);
}

#[cfg(test)]
fn hash_cloud(h: &mut super::Fnv, c: &Cloud) {
    let Cloud {
        effects,
        age,
        max_age,
        max_age_on_use,
        reapplication_delay,
        pickup_count,
        initial_radius,
        radius,
        radius_on_pickup,
        radius_on_use,
        radius_per_tick,
        victims,
    } = c;
    h.usize(effects.len());
    for e in effects {
        h.u64(u64::from(e.effect().id()));
        h.u64(u64::from(e.duration()));
        h.u64(u64::from(e.amplifier()));
        h.u64(u64::from(e.is_visible()));
        h.u64(u64::from(e.is_ambient()));
    }
    h.u64(u64::from(*age));
    for v in [
        *max_age,
        *max_age_on_use,
        *reapplication_delay,
        *pickup_count,
    ] {
        h.u64(u64::from(v.cast_unsigned()));
    }
    for v in [
        *initial_radius,
        *radius,
        *radius_on_pickup,
        *radius_on_use,
        *radius_per_tick,
    ] {
        h.u64(v.to_bits());
    }
    h.usize(victims.len());
    for (v, until) in victims {
        match v {
            kind::Victim::Player(s) => {
                h.u64(0);
                h.u64(s.get());
            }
            kind::Victim::Entity(id) => {
                h.u64(1);
                hash_id(h, *id);
            }
        }
        h.u64(u64::from(*until));
    }
}

#[cfg(test)]
fn hash_character(h: &mut super::Fnv, c: &Character) {
    let Character {
        form,
        invulnerable,
        attack_time,
        last_damage,
        dead_ticks,
    } = c;
    match form {
        Form::Villager { profession, offers } => {
            h.u64(0);
            h.u64(u64::from(*profession));
            offers.hash_into(h);
        }
        Form::Human { name, uuid } => {
            h.u64(1);
            h.bytes(name.as_bytes());
            h.bytes(uuid.as_bytes());
        }
    }
    h.u64(u64::from(*invulnerable));
    h.u64(u64::from(*attack_time));
    h.u64(last_damage.to_bits());
    h.u64(dead_ticks.map_or(0, |t| u64::from(t) + 1));
}

/// A slot of the store.
#[derive(Debug, Default)]
struct Slot {
    /// The generation of the slot's current or next entity.
    generation: u32,
    entity: Option<Entity>,
}

/// The entities of one chunk and the players that see it.
#[derive(Debug, Default)]
struct Column {
    ids: BTreeSet<Id>,
    /// The players that saw the chunk at the last tracking pass.
    viewers: BTreeSet<SessionId>,
    /// Some entity's viewers may differ from `viewers` (it arrived, or a bound held a packet
    /// back): the next pass looks at each entity.
    stale: bool,
}

/// An entity that left with its chunk, still to be removed from its viewers.
#[derive(Debug, Clone, PartialEq)]
struct Departed {
    runtime_id: u64,
    viewers: BTreeSet<SessionId>,
}

/// What other players were last sent of a player.
#[derive(Debug, Clone, PartialEq)]
struct Watched {
    runtime_id: u64,
    viewers: BTreeSet<SessionId>,
    /// The eye position sent.
    position: Vec3,
    held: Stack,
    held_slot: u8,
    off_hand: Stack,
    armour: [Stack; 4],
}

/// Every non-player entity, and what players were sent of entities and of each other.
#[derive(Debug, Default)]
pub(super) struct Entities {
    slots: Vec<Slot>,
    /// Free slot indices below `slots.len()`; the lowest is reused first.
    free: BTreeSet<u32>,
    /// The first generation of a slot added at the end: above every generation a slot removed
    /// from the end had, so an `Id` of a removed slot never matches again.
    retired: u32,
    /// Live entities per world.
    counts: BTreeMap<WorldIndex, usize>,
    /// The chunk index: every live entity is in the column of its chunk.
    columns: BTreeMap<Key, Column>,
    /// Entities by runtime ID.
    runtime_ids: BTreeMap<u64, Id>,
    /// Loaded entities waiting for a runtime ID, in load order (at most the cap per world).
    unassigned: Vec<Id>,
    /// Saved compounds that are not entities here, per loaded chunk, written back as they came.
    kept: BTreeMap<Key, Vec<Compound>>,
    /// Chunks whose list changed since the last snapshot took it.
    changed: BTreeSet<Key>,
    /// Entities of unloaded chunks still to be removed from their viewers.
    departed: Vec<Departed>,
    /// Players shown to other players, by session.
    watched: BTreeMap<SessionId, Watched>,
    /// Flagged entities waiting for their removal (at most every entity).
    doomed: BTreeSet<Id>,
    /// The slot where the next movement pass starts, so every entity is sent in turn.
    cursor: u32,
    /// Entities near an explosion of this tick that move again from their new state when their
    /// turn comes (at most every entity; empty between ticks).
    restep: BTreeSet<Id>,
}

impl Entities {
    /// The live entity `id` names, if it still exists.
    fn get(&self, id: Id) -> Option<&Entity> {
        let slot = self.slots.get(usize::try_from(id.index()).ok()?)?;
        (slot.generation == id.generation())
            .then_some(slot.entity.as_ref())
            .flatten()
    }

    /// [`Entities::get`], to change.
    fn get_mut(&mut self, id: Id) -> Option<&mut Entity> {
        let slot = self.slots.get_mut(usize::try_from(id.index()).ok()?)?;
        if slot.generation != id.generation() {
            return None;
        }
        slot.entity.as_mut()
    }

    /// The live entities in `Id` order.
    #[cfg(test)]
    fn ids(&self) -> impl Iterator<Item = Id> + '_ {
        (0u32..)
            .zip(&self.slots)
            .filter(|(_, s)| s.entity.is_some())
            .map(|(i, s)| Id::new(i, s.generation))
    }

    /// Live entities of `world`.
    fn count(&self, world: WorldIndex) -> usize {
        self.counts.get(&world).copied().unwrap_or(0)
    }

    /// Puts an entity into a slot and the chunk index; the caller checked the cap.
    fn insert(&mut self, entity: Entity) -> Id {
        let key = (entity.world, entity.chunk);
        let world = entity.world;
        let id = if let Some(index) = self.free.pop_first() {
            let slot = self
                .slots
                .get_mut(usize::try_from(index).unwrap_or(usize::MAX));
            match slot {
                Some(slot) => {
                    slot.entity = Some(entity);
                    Id::new(index, slot.generation)
                }
                None => self.push(entity),
            }
        } else {
            self.push(entity)
        };
        *self.counts.entry(world).or_insert(0) += 1;
        let column = self.columns.entry(key).or_default();
        column.ids.insert(id);
        column.stale = true;
        self.changed.insert(key);
        id
    }

    /// Puts an entity into a new slot at the end.
    fn push(&mut self, entity: Entity) -> Id {
        let index = u32::try_from(self.slots.len()).unwrap_or(u32::MAX);
        self.slots.push(Slot {
            generation: self.retired,
            entity: Some(entity),
        });
        Id::new(index, self.retired)
    }

    /// Takes an entity out of its slot and every index. Trailing free slots are dropped, so the
    /// store returns to empty.
    fn take(&mut self, id: Id) -> Option<Entity> {
        let index = usize::try_from(id.index()).ok()?;
        let slot = self.slots.get_mut(index)?;
        if slot.generation != id.generation() {
            return None;
        }
        let entity = slot.entity.take()?;
        slot.generation = slot.generation.wrapping_add(1);
        self.free.insert(id.index());
        while self.slots.last().is_some_and(|s| s.entity.is_none()) {
            if let Some(last) = self.slots.pop() {
                self.retired = self.retired.max(last.generation);
                self.free
                    .remove(&u32::try_from(self.slots.len()).unwrap_or(u32::MAX));
            }
        }
        if let Some(n) = self.counts.get_mut(&entity.world) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.counts.remove(&entity.world);
            }
        }
        let key = (entity.world, entity.chunk);
        if let Some(column) = self.columns.get_mut(&key) {
            column.ids.remove(&id);
            if column.ids.is_empty() {
                self.columns.remove(&key);
            }
        }
        if entity.runtime_id != 0 {
            self.runtime_ids.remove(&entity.runtime_id);
        }
        self.doomed.remove(&id);
        self.restep.remove(&id);
        self.changed.insert(key);
        Some(entity)
    }

    /// Moves an entity's index entry to the chunk of its position; both chunks change.
    fn reindex(&mut self, id: Id) {
        let Some(e) = self.get_mut(id) else {
            return;
        };
        let to = chunk_of(e.body.position);
        let from = e.chunk;
        if to == from {
            return;
        }
        e.chunk = to;
        let world = e.world;
        if let Some(column) = self.columns.get_mut(&(world, from)) {
            column.ids.remove(&id);
            if column.ids.is_empty() {
                self.columns.remove(&(world, from));
            }
        }
        let column = self.columns.entry((world, to)).or_default();
        column.ids.insert(id);
        column.stale = true;
        self.changed.insert((world, from));
        self.changed.insert((world, to));
    }

    /// Flags an entity gone: everything ignores it from now on, and it is removed at the end of
    /// the tick (or a later one, within the removal bound). False if it was gone already.
    fn doom(&mut self, id: Id) -> bool {
        let Some(e) = self.get_mut(id) else {
            return false;
        };
        if e.flagged {
            return false;
        }
        e.flagged = true;
        let key = (e.world, e.chunk);
        self.changed.insert(key);
        self.doomed.insert(id);
        true
    }

    /// The live, unflagged entities of `world` whose box overlaps `area`, in chunk then `Id`
    /// order (PMMP `getNearbyEntities`: the chunks within 2 blocks of the box).
    fn nearby(&self, world: WorldIndex, area: Aabb) -> Vec<Id> {
        let chunk = |v: f64| physics_block(v) >> 4;
        let (x0, x1) = (chunk(area.min.x - 2.0), chunk(area.max.x + 2.0));
        let (z0, z1) = (chunk(area.min.z - 2.0), chunk(area.max.z + 2.0));
        let mut out = Vec::new();
        for x in x0..=x1 {
            for z in z0..=z1 {
                let Some(column) = self.columns.get(&(world, ChunkPos::new(x, z))) else {
                    continue;
                };
                for &id in &column.ids {
                    if let Some(e) = self.get(id)
                        && !e.flagged
                        && e.bounding_box().overlaps(area, EPSILON)
                    {
                        out.push(id);
                    }
                }
            }
        }
        out
    }

    /// A chunk's saved list arrived ([`super::rules`]): its entities enter the store, waiting
    /// for runtime IDs; compounds that are not entities here, and entities over the cap, are
    /// kept as saved.
    pub(super) fn load(&mut self, key: Key, list: &[Compound]) {
        let mut kept = Vec::new();
        let mut over = 0usize;
        for c in list {
            let Some(l) = saved::load(c, key.1) else {
                kept.push(c.clone());
                continue;
            };
            if self.count(key.0) >= MAX_ENTITIES_PER_WORLD {
                over += 1;
                kept.push(c.clone());
                continue;
            }
            let mut e = Entity::new(key.0, l.body, l.yaw, l.pitch, l.kind);
            e.health = l.health;
            e.fire_ticks = l.fire_ticks;
            e.sent.flags = track::flags(&e.kind, e.fire_ticks > 0, false);
            let id = self.insert(e);
            self.unassigned.push(id);
        }
        // Loading changes nothing to save.
        self.changed.remove(&key);
        if over > 0 {
            warn!(
                world = key.0,
                x = key.1.x,
                z = key.1.z,
                over,
                "the world's entity limit is reached; saved entities stay as saved and do not tick"
            );
        }
        if !kept.is_empty() {
            debug!(
                world = key.0,
                x = key.1.x,
                z = key.1.z,
                kept = kept.len(),
                "saved entities kept as they are"
            );
            self.kept.insert(key, kept);
        }
    }

    /// A chunk leaves: its entities leave the store (their viewers are sent the removal next
    /// tick). Returns the chunk's list if it changed since [`Self::take_changed`].
    pub(super) fn unload(&mut self, key: Key) -> Option<Vec<Compound>> {
        let changed = self.changed.remove(&key);
        let ids: Vec<Id> = self
            .columns
            .get(&key)
            .map(|c| c.ids.iter().copied().collect())
            .unwrap_or_default();
        let mut list = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(e) = self.take(id) else {
                continue;
            };
            if !e.flagged {
                list.push(saved::save(&e));
            }
            if e.runtime_id != 0 && !e.viewers.is_empty() {
                self.departed.push(Departed {
                    runtime_id: e.runtime_id,
                    viewers: e.viewers,
                });
            }
        }
        if !self.unassigned.is_empty() {
            let slots = &self.slots;
            self.unassigned.retain(|&id| {
                usize::try_from(id.index())
                    .ok()
                    .and_then(|i| slots.get(i))
                    .is_some_and(|s| s.generation == id.generation() && s.entity.is_some())
            });
        }
        list.extend(self.kept.remove(&key).unwrap_or_default());
        // Taking the entities out marked the chunk; what counts is whether it changed before.
        self.changed.remove(&key);
        changed.then_some(list)
    }

    /// The full lists of the chunks that changed since the last call, for a save snapshot.
    pub(super) fn take_changed(&mut self) -> Vec<(Key, Vec<Compound>)> {
        let keys = std::mem::take(&mut self.changed);
        keys.into_iter().map(|key| (key, self.list(key))).collect()
    }

    /// A chunk's saved list: its entities in `Id` order, then the kept compounds.
    fn list(&self, key: Key) -> Vec<Compound> {
        let mut list: Vec<Compound> = self
            .columns
            .get(&key)
            .map(|c| {
                c.ids
                    .iter()
                    .filter_map(|&id| self.get(id))
                    .filter(|e| !e.flagged)
                    .map(saved::save)
                    .collect()
            })
            .unwrap_or_default();
        if let Some(kept) = self.kept.get(&key) {
            list.extend(kept.iter().cloned());
        }
        list
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            slots,
            free,
            retired,
            counts,
            columns,
            runtime_ids,
            unassigned,
            kept,
            changed,
            departed,
            watched,
            doomed,
            cursor,
            restep,
        } = self;
        h.usize(slots.len());
        for s in slots {
            h.u64(u64::from(s.generation));
            match &s.entity {
                Some(e) => e.hash_into(h),
                None => h.u64(u64::MAX),
            }
        }
        h.usize(free.len());
        for &f in free {
            h.u64(u64::from(f));
        }
        h.u64(u64::from(*retired));
        for (&w, &n) in counts {
            h.usize(w);
            h.usize(n);
        }
        h.usize(columns.len());
        for (k, c) in columns {
            hash_key(h, *k);
            h.usize(c.ids.len());
            for &i in &c.ids {
                hash_id(h, i);
            }
            for s in &c.viewers {
                h.u64(s.get());
            }
            h.u64(u64::from(c.stale));
        }
        for (&r, &i) in runtime_ids {
            h.u64(r);
            hash_id(h, i);
        }
        h.usize(unassigned.len());
        for &i in unassigned {
            hash_id(h, i);
        }
        h.usize(kept.len());
        for (k, list) in kept {
            hash_key(h, *k);
            h.usize(list.len());
            for c in list {
                super::extra::hash_nbt(h, c);
            }
        }
        h.usize(changed.len());
        for &k in changed {
            hash_key(h, k);
        }
        h.usize(departed.len());
        for d in departed {
            h.u64(d.runtime_id);
            for s in &d.viewers {
                h.u64(s.get());
            }
        }
        h.usize(watched.len());
        for (s, w) in watched {
            h.u64(s.get());
            w.hash_into(h);
        }
        h.usize(doomed.len());
        for &i in doomed {
            hash_id(h, i);
        }
        h.u64(u64::from(*cursor));
        h.usize(restep.len());
        for &i in restep {
            hash_id(h, i);
        }
    }
}

#[cfg(test)]
fn hash_key(h: &mut super::Fnv, (w, p): Key) {
    h.usize(w);
    h.u64(u64::from(p.x.cast_unsigned()));
    h.u64(u64::from(p.z.cast_unsigned()));
}

#[cfg(test)]
fn hash_id(h: &mut super::Fnv, id: Id) {
    h.u64(u64::from(id.index()));
    h.u64(u64::from(id.generation()));
}

impl Watched {
    #[cfg(test)]
    fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            runtime_id,
            viewers,
            position,
            held,
            held_slot,
            off_hand,
            armour,
        } = self;
        h.u64(*runtime_id);
        for v in viewers {
            h.u64(v.get());
        }
        for c in [position.x, position.y, position.z] {
            h.u64(c.to_bits());
        }
        for stack in std::iter::once(held).chain([off_hand]).chain(armour) {
            super::inventory::hash_stack(h, stack);
        }
        h.u64(u64::from(*held_slot));
    }
}

/// The block coordinate of a world coordinate.
#[expect(
    clippy::cast_possible_truncation,
    reason = "invariant: entity coordinates are finite and inside the world border; `as` saturates otherwise"
)]
fn physics_block(v: f64) -> i32 {
    v.floor() as i32
}

/// The block position of a point.
fn block_at(v: Vec3) -> BlockPos {
    BlockPos::new(physics_block(v.x), physics_block(v.y), physics_block(v.z))
}

/// Whether a point may hold an entity: finite and inside the world border.
fn is_valid_position(v: Vec3) -> bool {
    v.is_finite() && v.x.abs() <= MAX_COORDINATE && v.z.abs() <= MAX_COORDINATE
}

/// The functions of the player modules (inventory, survival) the entity rules call on players, so
/// tests can stand in for them and record what the rules asked.
#[derive(Debug, Clone, Copy)]
struct Hooks {
    /// Puts a stack into a player's inventory; returns what did not fit.
    give: fn(&mut Player, Stack) -> Stack,
    /// A player picks up an orb's experience points (mending first); false, and the orb stays,
    /// while its pickup cooldown runs.
    pickup_experience: fn(&mut State, SessionId, u32) -> bool,
    /// Hurts a player; false if no damage was taken.
    damage: fn(&mut State, SessionId, f64, survival::Damage) -> bool,
    /// Whether a player's hit cooldown runs (a hit then does not knock it back).
    has_attack_cooldown: fn(&Player) -> bool,
    /// Gives a player an effect.
    add_effect: fn(&mut State, SessionId, crate::effect::Instance) -> bool,
    /// Heals a player by an amount (instant health of a potion, with its potency).
    heal: fn(&mut State, SessionId, f64),
    /// Sets a player on fire for a number of seconds.
    set_on_fire: fn(&mut State, SessionId, u32),
    /// Moves a player (eye position) in a world.
    teleport: fn(&mut State, SessionId, WorldIndex, Vec3),
    /// Whether a player is alive.
    is_alive: fn(&Player) -> bool,
    /// The yaw and pitch a player looks along, in degrees.
    look: fn(&Player) -> (f64, f64),
    /// Whether a player carries an arrow (off hand or inventory).
    has_arrow: fn(&Player) -> bool,
    /// Takes one arrow from a player, off hand first (PMMP `Bow::onReleaseUsing`); false if it
    /// has none.
    take_arrow: fn(&mut Player) -> bool,
}

/// The hooks of the running server.
const HOOKS: Hooks = Hooks {
    give: inventory::give,
    pickup_experience: survival::pickup_experience,
    damage: survival::damage,
    has_attack_cooldown: survival::has_attack_cooldown,
    add_effect: survival::add_effect,
    heal: survival::heal,
    set_on_fire: survival::set_on_fire,
    teleport: survival::teleport,
    is_alive: survival::is_alive,
    look: survival::rotation,
    has_arrow: inventory::has_arrow,
    take_arrow: inventory::take_arrow,
};

/// Creates an entity of `kind` at `position` with the motion and rotation (yaw, pitch) `motion`
/// gives once it has its runtime ID; `None` when the world does not exist, the position is
/// invalid or not loaded, the world is at its cap or runtime IDs ran out.
fn create(
    state: &mut State,
    world: WorldIndex,
    position: Vec3,
    kind: Kind,
    motion: impl FnOnce(&mut Random) -> (Vec3, f64, f64),
) -> Option<Id> {
    let w = state.worlds.get(world)?;
    if !is_valid_position(position)
        || !w.is_chunk_loaded(chunk_of(position))
        || state.rules.entities.count(world) >= MAX_ENTITIES_PER_WORLD
    {
        return None;
    }
    let runtime_id = state.next_runtime_id;
    let next = runtime_id.checked_add(1)?;
    let mut random = Random::new(
        w.seed(),
        state.tick,
        random::key(DROP_RANDOM, &[runtime_id]),
    );
    let (motion, yaw, pitch) = motion(&mut random);
    state.next_runtime_id = next;
    let body = Body {
        position,
        motion: physics::bounded(motion),
        ..Body::default()
    };
    let mut entity = Entity::new(world, body, yaw, pitch, kind);
    given(&mut entity, runtime_id);
    let entities = &mut state.rules.entities;
    let id = entities.insert(entity);
    entities.runtime_ids.insert(runtime_id, id);
    Some(id)
}

/// A random yaw as PMMP picks one (`getRandomFloat() * 360`), kept to float precision as saved.
fn random_yaw(random: &mut Random) -> f64 {
    f64::from(to_f32(random.unit() * 360.0))
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "consistency: rotations are floats in the saved form"
)]
/// An `f32` of a rotation (rounded to nearest).
fn to_f32(v: f64) -> f32 {
    v as f32
}

/// Drops `stack` as an item entity at `pos` with PMMP `World::dropItem`'s random motion and
/// pickup delay. False (nothing created, the caller keeps the stack) when the world's entity
/// limit is reached or the position is not loaded; true for an empty stack, which drops nothing.
#[doc(alias = "World::dropItem")]
pub(super) fn drop_item(state: &mut State, world: WorldIndex, pos: Vec3, stack: Stack) -> bool {
    drop_item_with(state, world, pos, stack, None, DROP_PICKUP_DELAY)
}

/// [`drop_item`] with a given motion (`None`: PMMP's random one) and pickup delay in ticks, for
/// items a player throws (PMMP `Player::dropItem`: its look direction × 0.4, delay 40).
pub(super) fn drop_item_with(
    state: &mut State,
    world: WorldIndex,
    pos: Vec3,
    stack: Stack,
    motion: Option<Vec3>,
    pickup_delay: i16,
) -> bool {
    if stack.is_empty() {
        return true;
    }
    let item = Item {
        stack,
        pickup_delay: max_delay(pickup_delay),
        despawn_delay: DEFAULT_DESPAWN_DELAY,
        owner: String::new(),
        thrower: String::new(),
    };
    create(state, world, pos, Kind::Item(item), |r| {
        let yaw = random_yaw(r);
        let motion = motion.unwrap_or_else(|| {
            let x = r.unit() * 0.2 - 0.1;
            let z = r.unit() * 0.2 - 0.1;
            Vec3::new(x, 0.2, z)
        });
        (motion, yaw, 0.0)
    })
    .is_some()
}

/// A pickup delay of at least [`NEVER`].
fn max_delay(d: i16) -> i16 {
    d.max(NEVER)
}

/// Drops `amount` experience as orbs at `pos` (PMMP `World::dropExperience`: split into the orb
/// sizes, each with a random motion). At the world's entity limit the rest is not dropped:
/// experience is not an item a caller can keep.
#[doc(alias = "World::dropExperience")]
pub(super) fn drop_experience(state: &mut State, world: WorldIndex, pos: Vec3, amount: u32) {
    let mut left = amount;
    while left > 0 {
        let size = ORB_SPLIT_SIZES
            .into_iter()
            .find(|&s| u32::from(s) <= left)
            .unwrap_or(1);
        left = left.saturating_sub(u32::from(size));
        let orb = Orb {
            value: size,
            despawn_delay: DEFAULT_DESPAWN_DELAY,
            look_for_target: 0,
            target: None,
        };
        let made = create(state, world, pos, Kind::ExperienceOrb(orb), |r| {
            let yaw = random_yaw(r);
            let x = (r.unit() * 0.2 - 0.1) * 2.0;
            let y = r.unit() * 0.4;
            let z = (r.unit() * 0.2 - 0.1) * 2.0;
            (Vec3::new(x, y, z), yaw, 0.0)
        });
        if made.is_none() {
            return;
        }
    }
}

/// Turns the block at `pos` into a falling block entity (sand, gravel, anvils…): the block
/// becomes air and the entity starts at the bottom centre of its cell (PMMP `FallableTrait`).
/// False, with the block left as it is, when the entity limit is reached or `pos` is not
/// loaded.
pub(super) fn fall(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    block: block::State,
) -> bool {
    let at = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y),
        f64::from(pos.z) + 0.5,
    );
    let Some(w) = state.worlds.get(world) else {
        return false;
    };
    if !(MIN_Y..=MAX_Y).contains(&pos.y)
        || !is_valid_position(at)
        || !w.is_chunk_loaded(pos.chunk())
        || state.rules.entities.count(world) >= MAX_ENTITIES_PER_WORLD
        || state.next_runtime_id == u64::MAX
    {
        return false;
    }
    let Some(w) = state.worlds.get_mut(world) else {
        return false;
    };
    if w.set_block(pos, block::AIR).is_err() {
        return false;
    }
    let kind = Kind::FallingBlock(Falling { block });
    create(state, world, at, kind, |_| (Vec3::ZERO, 0.0, 0.0)).is_some()
}

/// The entity with runtime ID `runtime_id`, for packets that name one (attacks and interactions
/// from wave C on).
pub(super) fn find(state: &State, runtime_id: u64) -> Option<Id> {
    state.rules.entities.runtime_ids.get(&runtime_id).copied()
}

/// Removes an entity at the end of this tick (killed, used up), with its removal sent to its
/// viewers. False if it is gone already.
pub(super) fn remove(state: &mut State, id: Id) -> bool {
    state.rules.entities.doom(id)
}

/// A neighbour update reached the block at `pos`: the entities whose box touches its cell move
/// at their next tick even at rest, so an item falls when the block under it goes (PMMP's
/// neighbour update loop calls `onNearbyBlockChange`). Block updates call it for each neighbour
/// update they run.
pub(super) fn block_changed(state: &mut State, world: WorldIndex, pos: BlockPos) {
    let at = Vec3::new(f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
    let cell = Aabb::new(at, at + Vec3::new(1.0, 1.0, 1.0));
    let entities = &mut state.rules.entities;
    for id in entities.nearby(world, cell) {
        if let Some(e) = entities.get_mut(id) {
            e.forced = true;
        }
    }
}

/// One tick of every entity: physics on the CPU pool (read-only), then the results applied in
/// entity order, then pickups, removals and tracking (PLAN.md 5 phases 2 and 3).
pub(super) fn tick(state: &mut State, pool: &Pool) {
    tick_with(state, pool, HOOKS);
}

/// [`tick`] with the given hooks.
fn tick_with(state: &mut State, pool: &Pool, hooks: Hooks) {
    assign_runtime_ids(state);
    let steps = compute(state, pool);
    apply_steps(state, steps, hooks);
    pick_up(state, hooks);
    remove_flagged(state);
    track_entities(state);
    track_players(state);
}

/// The physics input of an entity this tick.
fn input(state: &State, e: &Entity) -> Option<physics::Input> {
    let seed = state.worlds.get(e.world)?.seed();
    let key = random::key(UNSTICK_RANDOM, &[e.runtime_id]);
    let stuck = match &e.kind {
        Kind::Projectile(p) => p.stuck,
        _ => None,
    };
    Some(physics::Input {
        body: e.body,
        physics: e.kind.physics(),
        forced: e.forced,
        random: (seed, state.tick, key),
        stuck,
    })
}

/// Phase 2: the movement of every ticking entity, on the CPU pool, in `Id` order.
fn compute(state: &State, pool: &Pool) -> Vec<(Id, Option<Step>)> {
    let inputs: Vec<(Id, WorldIndex, physics::Input)> = ticking(state)
        .into_iter()
        .filter_map(|id| {
            let e = state.rules.entities.get(id)?;
            Some((id, e.world, input(state, e)?))
        })
        .collect();
    let worlds = &state.worlds;
    let steps = pool.map(&inputs, |(_, world, input)| {
        worlds.get(*world).map(|w| physics::step(w, input))
    });
    inputs.iter().map(|&(id, _, _)| id).zip(steps).collect()
}

/// What the apply phase of one tick may still do: merges, and explosions with the blocks they
/// affect (PLAN.md 10.7 per-tick bounds).
#[derive(Debug, Default)]
struct Budget {
    /// Item merges so far ([`MAX_MERGES_PER_TICK`]).
    merges: usize,
    /// Explosions of primed TNT so far ([`explosion::MAX_EXPLOSIONS_PER_TICK`]).
    explosions: usize,
    /// Blocks those explosions affected so far ([`explosion::MAX_BLOCKS_PER_TICK`]).
    blocks: usize,
}

/// Phase 3: the steps applied and the kind rules run, in `Id` order; an entity near an explosion
/// of this tick moves again from its new state; then the damage falling anvils dealt.
fn apply_steps(state: &mut State, steps: Vec<(Id, Option<Step>)>, hooks: Hooks) {
    let mut budget = Budget::default();
    let mut hits = Vec::new();
    for (id, step) in steps {
        let step = if state.rules.entities.restep.remove(&id) {
            restep(state, id)
        } else {
            step
        };
        if let Some(step) = step {
            apply(state, id, step, hooks, &mut budget, &mut hits);
        }
    }
    state.rules.entities.restep.clear();
    for (session, damage) in hits {
        (hooks.damage)(state, session, damage, survival::Damage::Entity);
    }
}

/// An entity's movement computed again, serially, from its state now.
fn restep(state: &State, id: Id) -> Option<Step> {
    let e = state.rules.entities.get(id)?;
    let input = input(state, e)?;
    state.worlds.get(e.world).map(|w| physics::step(w, &input))
}

/// Gives the loaded entities their runtime IDs, in load order.
fn assign_runtime_ids(state: &mut State) {
    let entities = &mut state.rules.entities;
    let waiting = std::mem::take(&mut entities.unassigned);
    for (i, &id) in waiting.iter().enumerate() {
        let Some(next) = state.next_runtime_id.checked_add(1) else {
            entities.unassigned = waiting.get(i..).map(<[Id]>::to_vec).unwrap_or_default();
            return;
        };
        let Some(e) = entities.get_mut(id) else {
            continue;
        };
        let runtime_id = state.next_runtime_id;
        given(e, runtime_id);
        state.next_runtime_id = next;
        entities.runtime_ids.insert(runtime_id, id);
    }
}

/// Gives an entity its runtime ID (and a human NPC the player list UUID made from it).
fn given(e: &mut Entity, runtime_id: u64) {
    e.runtime_id = runtime_id;
    if let Kind::Npc(Character {
        form: Form::Human { uuid, .. },
        ..
    }) = &mut e.kind
    {
        *uuid = track::npc_uuid(runtime_id);
    }
}

/// The entities that tick: in chunks within `tick-radius` of a player of their world, with a
/// runtime ID and not flagged, in `Id` order.
fn ticking(state: &State) -> Vec<Id> {
    let r = i64::from(state.settings.tick_radius);
    let centres: Vec<(WorldIndex, ChunkPos)> = state
        .players
        .values()
        .filter(|p| p.view.is_open())
        .map(|p| (p.world, chunk_of(p.position)))
        .collect();
    if centres.is_empty() {
        return Vec::new();
    }
    let entities = &state.rules.entities;
    let mut ids = Vec::new();
    for (&(world, pos), column) in &entities.columns {
        let near = centres.iter().any(|&(w, c)| {
            let (dx, dz) = (
                i64::from(pos.x) - i64::from(c.x),
                i64::from(pos.z) - i64::from(c.z),
            );
            w == world && dx * dx + dz * dz <= r * r
        });
        if near {
            ids.extend(column.ids.iter().copied().filter(|&id| {
                entities
                    .get(id)
                    .is_some_and(|e| !e.flagged && e.runtime_id != 0)
            }));
        }
    }
    ids.sort_unstable();
    ids
}

/// Applies one entity's step and runs its rules (PMMP `onUpdate`: a dead NPC's death, the
/// movement with a projectile's hits, then `entityBaseTick`).
fn apply(
    state: &mut State,
    id: Id,
    step: Step,
    hooks: Hooks,
    budget: &mut Budget,
    hits: &mut Vec<(SessionId, f64)>,
) {
    let entities = &mut state.rules.entities;
    let Some(e) = entities.get_mut(id) else {
        return;
    };
    if e.flagged {
        return;
    }
    if let Kind::Npc(c) = &mut e.kind
        && let Some(t) = c.dead_ticks
    {
        // PMMP `Living::onDeathUpdate`: the death shows, then the NPC goes.
        let t = t.saturating_add(1);
        c.dead_ticks = Some(t);
        if t >= kind::MAX_DEAD_TICKS {
            entities.doom(id);
        }
        return;
    }
    let was_on_ground = e.body.on_ground;
    e.body = step.body;
    e.forced = false;
    if let Some((yaw, pitch)) = step.rotation {
        e.yaw = yaw;
        e.pitch = pitch;
    }
    if step.released
        && let Kind::Projectile(p) = &mut e.kind
    {
        p.stuck = None;
    }
    let key = (e.world, e.chunk);
    entities.changed.insert(key);
    entities.reindex(id);
    if let Some(flight) = step.flight {
        projectile::hit(state, id, flight, step.hit, was_on_ground, hooks);
    }
    if let Some(fall) = step.landed {
        landed(state, id, fall, hits, hooks);
    }
    if !is_live(state, id) {
        return;
    }
    for inside in step.inside {
        match inside {
            Inside::Fire(damage) => {
                hurt(state, id, Cause::Fire, f64::from(damage), None);
                with(state, id, |e| e.set_on_fire(8));
            }
            Inside::Lava => {
                hurt(state, id, Cause::Lava, 4.0, None);
                with(state, id, |e| e.set_on_fire(8));
            }
            Inside::Cactus => {
                hurt(state, id, Cause::Contact, 1.0, None);
            }
            Inside::Water => {
                with(state, id, |e| e.fire_ticks = 0);
            }
        }
    }
    let Some(e) = state.rules.entities.get_mut(id) else {
        return;
    };
    let void = e.body.position.y <= f64::from(MIN_Y - 16);
    if void {
        hurt(state, id, Cause::Void, 10.0, None);
    }
    if with(state, id, Entity::burn) == Some(true) {
        hurt(state, id, Cause::FireTick, 1.0, None);
    }
    let Some(e) = state.rules.entities.get_mut(id) else {
        return;
    };
    e.age = e.age.saturating_add(1);
    if e.flagged || matches!(&e.kind, Kind::Npc(c) if c.dead_ticks.is_some()) {
        return;
    }
    match rule_of(&e.kind) {
        Rule::Item => item_tick(state, id, &mut budget.merges),
        Rule::Orb => orb_tick(state, id, hooks),
        Rule::Falling => falling_tick(state, id),
        Rule::Tnt => explosion::tnt_tick(state, id, budget, hooks),
        Rule::Projectile => projectile::tick(state, id),
        Rule::Cloud => projectile::cloud_tick(state, id, hooks),
        Rule::Npc => npc::tick(state, id, hooks),
    }
    // The kind rules flag what they use up (despawned, merged, picked up, landed).
    let entities = &mut state.rules.entities;
    if entities.get(id).is_some_and(|e| e.flagged) {
        entities.doomed.insert(id);
    }
}

/// Whether the entity exists and is not flagged.
fn is_live(state: &State, id: Id) -> bool {
    state.rules.entities.get(id).is_some_and(|e| !e.flagged)
}

/// Runs `f` on the entity `id` if it exists and is not flagged.
fn with<T>(state: &mut State, id: Id, f: impl FnOnce(&mut Entity) -> T) -> Option<T> {
    state
        .rules
        .entities
        .get_mut(id)
        .filter(|e| !e.flagged)
        .map(f)
}

/// Hurts the entity `id` (PMMP `Entity::attack`, `Living::attack`): an NPC hit after its
/// cooldown is knocked back by `knock` and shows the hit, an NPC that dies shows its death,
/// anything else that dies is removed.
fn hurt(state: &mut State, id: Id, cause: Cause, amount: f64, knock: Option<Knock>) -> Hurt {
    let entities = &mut state.rules.entities;
    let Some(e) = entities.get_mut(id).filter(|e| !e.flagged) else {
        return Hurt::Ignored;
    };
    let result = e.attack(cause, amount);
    let living = matches!(e.kind, Kind::Npc(_));
    let shown = living && matches!(result, Hurt::Took { cold: true } | Hurt::Died);
    let event = if shown {
        if let Some(m) = knock.and_then(|k| k.applied_to(e.body.motion)) {
            e.body.motion = physics::bounded(m);
        }
        Some(if result == Hurt::Died {
            ActorEvent::Death
        } else {
            ActorEvent::Hurt
        })
    } else {
        None
    };
    let (runtime_id, viewers, key) = (e.runtime_id, e.viewers.clone(), (e.world, e.chunk));
    if result != Hurt::Ignored {
        entities.changed.insert(key);
    }
    if result == Hurt::Died && !living {
        entities.doom(id);
    }
    if let Some(event) = event {
        send_to(
            &mut state.players,
            &viewers,
            &actor_event(runtime_id, event, 0),
        );
    }
    result
}

/// Shows a level event to the players who see the chunk of `at`.
fn level_event(state: &mut State, world: WorldIndex, at: Vec3, event: LevelEvent, data: i32) {
    let packet = Packet::LevelEvent(LevelEventPacket {
        event_type: event.into(),
        position: at.to_f32(),
        event_data: data,
    });
    state.broadcast(world, chunk_of(at), &packet, None);
}

/// Plays a sound to the players who see the chunk of `at` (PMMP `LevelSoundEventPacket::
/// nonActorSound`).
fn sound(state: &mut State, world: WorldIndex, at: Vec3, sound: SoundEvent) {
    let packet = Packet::LevelSoundEvent(LevelSoundEventPacket {
        sound_type: sound.as_str().to_owned(),
        position: at.to_f32(),
        extra_data: -1,
        entity_type: ":".to_owned(),
        baby_mob: false,
        disable_relative_volume: false,
        entity_unique_id: -1,
        fire_at_position: None,
    });
    state.broadcast(world, chunk_of(at), &packet, None);
}

/// Pushes a player: its client is sent the motion, and so are the players who see it (PMMP
/// `Player::setMotion`; the server keeps no motion of players).
fn push_player(state: &mut State, session: SessionId, motion: Vec3) {
    let Some(p) = state.players.get(&session) else {
        return;
    };
    let motion = physics::bounded(motion);
    let packet = Packet::SetActorMotion(crate::protocol::packet::SetActorMotion {
        entity_runtime_id: p.runtime_id,
        velocity: motion.to_f32(),
        tick: 0,
    });
    let (world, chunk) = (p.world, chunk_of(p.position));
    state.broadcast(world, chunk, &packet, None);
}

/// The look direction of a yaw and pitch in degrees (PMMP `Entity::getDirectionVector`).
fn direction(yaw: f64, pitch: f64) -> Vec3 {
    let (yaw, pitch) = (crate::math::radians(yaw), crate::math::radians(pitch));
    let xz = crate::math::cos(pitch);
    Vec3::new(
        -xz * crate::math::sin(yaw),
        -crate::math::sin(pitch),
        xz * crate::math::cos(yaw),
    )
    .normalize()
}

/// The level of `enchantment` on `stack`, 0 without it.
fn level_of(stack: &Stack, enchantment: crate::item::Enchantment) -> u8 {
    stack
        .extra()
        .and_then(|x| x.enchantments.iter().find(|(e, _)| *e == enchantment))
        .map_or(0, |&(_, level)| level)
}

/// `stack` after `amount` points of wear (PMMP `Durable::applyDamage`: each point is avoided
/// with Unbreaking's chance; an unbreakable item does not wear); empty when it breaks.
fn worn(stack: &Stack, amount: u16, random: &mut Random) -> Stack {
    let Some(max) = stack.item().and_then(crate::item::Type::max_durability) else {
        return stack.clone();
    };
    let unbreakable = stack.extra().is_some_and(
        |x| matches!(x.nbt.get("Unbreakable"), Some(crate::nbt::Value::Byte(b)) if *b != 0),
    );
    if unbreakable {
        return stack.clone();
    }
    let level = level_of(stack, crate::item::Enchantment::Unbreaking);
    let mut wear = amount;
    if level > 0 {
        let chance = 1.0 / (f64::from(level) + 1.0);
        for _ in 0..amount {
            if random.unit() > chance {
                wear -= 1;
            }
        }
    }
    let damage = stack.damage().saturating_add(wear);
    if damage >= max {
        Stack::EMPTY
    } else {
        stack.with_damage(damage)
    }
}

/// An `ActorEvent` of an entity.
fn actor_event(runtime_id: u64, event: ActorEvent, data: i32) -> Packet {
    Packet::ActorEvent(ActorEventPacket {
        entity_runtime_id: runtime_id,
        event_type: event as u8,
        event_data: data,
        fire_at_position: None,
    })
}

/// Which kind rule an entity runs.
#[derive(Debug, Clone, Copy)]
enum Rule {
    /// [`item_tick`].
    Item,
    /// [`orb_tick`].
    Orb,
    /// [`falling_tick`].
    Falling,
    /// [`explosion::tnt_tick`].
    Tnt,
    /// [`projectile::tick`].
    Projectile,
    /// [`projectile::cloud_tick`].
    Cloud,
    /// [`npc::tick`].
    Npc,
}

/// The rule of a kind.
fn rule_of(kind: &Kind) -> Rule {
    match kind {
        Kind::Item(_) => Rule::Item,
        Kind::ExperienceOrb(_) => Rule::Orb,
        Kind::FallingBlock(_) => Rule::Falling,
        Kind::Tnt(_) => Rule::Tnt,
        Kind::Projectile(_) => Rule::Projectile,
        Kind::Cloud(_) => Rule::Cloud,
        Kind::Npc(_) => Rule::Npc,
    }
}

/// An item's tick (PMMP `ItemEntity::entityBaseTick`): the pickup delay, merging, despawning.
fn item_tick(state: &mut State, id: Id, merges: &mut usize) {
    let entities = &mut state.rules.entities;
    let Some(e) = entities.get_mut(id) else {
        return;
    };
    let moving = e.body.has_movement_update(false);
    let Kind::Item(item) = &mut e.kind else {
        return;
    };
    if item.pickup_delay > 0 {
        item.pickup_delay -= 1;
    }
    let check = moving && item.despawn_delay % MERGE_CHECK_PERIOD == 0;
    if check && is_merge_candidate(item) && *merges < MAX_MERGES_PER_TICK {
        merge(state, id, merges);
    }
    let Some(e) = state.rules.entities.get_mut(id) else {
        return;
    };
    if e.flagged {
        return;
    }
    let Kind::Item(item) = &mut e.kind else {
        return;
    };
    if item.despawn_delay != i32::from(NEVER) {
        item.despawn_delay -= 1;
        if item.despawn_delay <= 0 {
            e.flagged = true;
        }
    }
}

/// PMMP `isMergeCandidate`: it can be picked up and its stack is not full.
fn is_merge_candidate(item: &Item) -> bool {
    item.pickup_delay != NEVER && item.stack.count() < item.stack.max_count()
}

/// PMMP `isMergeable`: two candidates of stacking stacks that fit one stack.
fn is_mergeable(a: &Item, b: &Item) -> bool {
    is_merge_candidate(a)
        && is_merge_candidate(b)
        && a.stack.can_stack_with(&b.stack)
        && u16::from(a.stack.count()) + u16::from(b.stack.count()) <= u16::from(a.stack.max_count())
}

/// The item data of an unflagged dropped item.
fn as_item(e: &Entity) -> Option<&Item> {
    match &e.kind {
        Kind::Item(item) if !e.flagged => Some(item),
        _ => None,
    }
}

/// Merges the items around `id` into the one with the largest stack (PMMP: the merge check of
/// `ItemEntity::entityBaseTick` and `tryMergeInto`).
fn merge(state: &mut State, id: Id, merges: &mut usize) {
    let entities = &state.rules.entities;
    let Some(this) = entities.get(id) else {
        return;
    };
    let Some(item) = as_item(this) else {
        return;
    };
    let mut target = (id, item.stack.count());
    let mut mergeable = vec![id];
    for other in entities.nearby(this.world, this.bounding_box().grow(MERGE_REACH)) {
        if other == id {
            continue;
        }
        let Some(o) = entities.get(other).and_then(as_item) else {
            continue;
        };
        if is_mergeable(o, item) {
            mergeable.push(other);
            if o.stack.count() > target.1 {
                target = (other, o.stack.count());
            }
        }
    }
    let target = target.0;
    for source in mergeable {
        if source == target || *merges >= MAX_MERGES_PER_TICK {
            continue;
        }
        if merge_into(state, source, target) {
            *merges += 1;
        }
    }
}

/// Moves the stack of `source` into `target` if they can merge now; tells the target's viewers
/// its new count (PMMP `ItemEntityStackSizeChangeAnimation`).
fn merge_into(state: &mut State, source: Id, target: Id) -> bool {
    let entities = &mut state.rules.entities;
    let (Some(s), Some(t)) = (entities.get(source), entities.get(target)) else {
        return false;
    };
    let (Some(si), Some(ti)) = (as_item(s), as_item(t)) else {
        return false;
    };
    if !is_mergeable(si, ti) {
        return false;
    }
    let count = si.stack.count().saturating_add(ti.stack.count());
    let (delay, despawn) = (si.pickup_delay, si.despawn_delay);
    let source_key = (s.world, s.chunk);
    entities.doom(source);
    entities.changed.insert(source_key);
    let Some(t) = entities.get_mut(target) else {
        return false;
    };
    let Kind::Item(ti) = &mut t.kind else {
        return false;
    };
    ti.stack = ti.stack.with_count(count);
    ti.pickup_delay = ti.pickup_delay.max(delay);
    ti.despawn_delay = ti.despawn_delay.max(despawn);
    let packet = stack_size(t.runtime_id, count);
    let (viewers, key) = (t.viewers.clone(), (t.world, t.chunk));
    entities.changed.insert(key);
    send_to(&mut state.players, &viewers, &packet);
    true
}

/// `ActorEvent` with an item entity's new count.
fn stack_size(runtime_id: u64, count: u8) -> Packet {
    Packet::ActorEvent(ActorEventPacket {
        entity_runtime_id: runtime_id,
        event_type: ActorEvent::UpdateStackSize as u8,
        event_data: i32::from(count),
        fire_at_position: None,
    })
}

/// Queues `packet` for every viewer still online.
fn send_to(
    players: &mut BTreeMap<SessionId, Player>,
    viewers: &BTreeSet<SessionId>,
    packet: &Packet,
) {
    for s in viewers {
        if let Some(p) = players.get_mut(s) {
            p.send(packet.clone());
        }
    }
}

/// Whether a player picks up items and attracts orbs: spawned, alive and not a spectator (PMMP
/// `Player::onUpdate` checks nearby entities only while the player is alive).
fn collects(state: &State, player: &Player, hooks: Hooks) -> bool {
    player.phase == Phase::Spawned
        && (hooks.is_alive)(player)
        && survival::game_mode(state, player) != GameMode::Spectator
}

/// Whether projectiles, potions, clouds and falling anvils reach a player (PMMP
/// `canBeCollidedWith`: alive and not a spectator), in `world`.
fn targetable(state: &State, player: &Player, world: WorldIndex, hooks: Hooks) -> bool {
    player.world == world && collects(state, player, hooks)
}

/// A player's box (PMMP `Human`: feet at the eye position less 1.62).
fn player_box(player: &Player) -> Aabb {
    let feet = player.position - Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0);
    let half = PLAYER_WIDTH / 2.0;
    Aabb::new(
        Vec3::new(feet.x - half, feet.y, feet.z - half),
        Vec3::new(feet.x + half, feet.y + PLAYER_HEIGHT, feet.z + half),
    )
}

/// An orb's tick (PMMP `ExperienceOrb::entityBaseTick`): despawning, following the nearest
/// player within 8 blocks, and giving it the experience on touch.
fn orb_tick(state: &mut State, id: Id, hooks: Hooks) {
    let Some(e) = state.rules.entities.get(id) else {
        return;
    };
    let Kind::ExperienceOrb(orb) = &e.kind else {
        return;
    };
    let (world, position, bounds) = (e.world, e.body.position, e.bounding_box());
    let mut despawn = orb.despawn_delay;
    // PMMP counts "never" (-1) down too and removes such orbs at once; never means never here.
    if despawn != i32::from(NEVER) {
        despawn -= 1;
        if despawn <= 0 {
            if let Some(e) = state.rules.entities.get_mut(id) {
                e.flagged = true;
                if let Kind::ExperienceOrb(orb) = &mut e.kind {
                    orb.despawn_delay = despawn;
                }
            }
            return;
        }
    }
    let feet = |p: &Player| p.position - Vec3::new(0.0, PLAYER_EYE_HEIGHT, 0.0);
    let reach = ORB_TARGET_DISTANCE * ORB_TARGET_DISTANCE;
    let valid = |s: &SessionId| {
        state.players.get(s).filter(|p| {
            p.world == world
                && collects(state, p, hooks)
                && feet(p).distance_squared(position) <= reach
        })
    };
    let mut target = orb.target.filter(|s| valid(s).is_some());
    let mut look = orb.look_for_target;
    if look >= ORB_LOOK_PERIOD {
        if target.is_none() {
            let mut best = reach;
            for (&s, p) in &state.players {
                if p.world != world || !collects(state, p, hooks) {
                    continue;
                }
                let d = feet(p).distance_squared(position);
                if d < best {
                    best = d;
                    target = Some(s);
                }
            }
        }
        look = 0;
    } else {
        look += 1;
    }
    let mut pull = Vec3::ZERO;
    let mut picker = None;
    if let Some(p) = target.and_then(|s| state.players.get(&s)) {
        let aim = feet(p) + Vec3::new(0.0, PLAYER_EYE_HEIGHT / 2.0, 0.0);
        let vector = (aim - position) * (1.0 / ORB_TARGET_DISTANCE);
        let distance = vector.length_squared();
        if distance < 1.0 {
            let k = 1.0 - distance.sqrt();
            pull = vector.normalize() * (0.2 * k * k);
        }
        if bounds.overlaps(player_box(p), EPSILON) {
            picker = target;
        }
    }
    let Some(e) = state.rules.entities.get_mut(id) else {
        return;
    };
    e.body.motion = physics::bounded(e.body.motion + pull);
    let Kind::ExperienceOrb(orb) = &mut e.kind else {
        return;
    };
    orb.despawn_delay = despawn;
    orb.look_for_target = look;
    orb.target = target;
    let value = orb.value;
    // PMMP: the orb goes only when the player can pick it up (`ExperienceManager::canPickupXp`).
    if let Some(s) = picker
        && (hooks.pickup_experience)(state, s, u32::from(value))
        && let Some(e) = state.rules.entities.get_mut(id)
    {
        e.flagged = true;
    }
}

/// A falling block's tick (PMMP `FallingBlock::entityBaseTick`): on the ground (or turned into
/// another block, as concrete powder by water) it is placed where it is if that block can be
/// replaced, otherwise its item form drops. If the item cannot drop (the entity limit), the
/// falling block stays and tries again next tick.
fn falling_tick(state: &mut State, id: Id) {
    let Some(e) = state.rules.entities.get(id).filter(|e| !e.flagged) else {
        return;
    };
    let Kind::FallingBlock(falling) = e.kind else {
        return;
    };
    let physics = e.kind.physics();
    let (world, position, on_ground) = (e.world, e.body.position, e.body.on_ground);
    let half = physics.width / 2.0;
    let pos = block_at(position + Vec3::new(-half, physics.height, -half));
    let Some(w) = state.worlds.get(world) else {
        return;
    };
    let turned = harden(w, pos, falling.block);
    if !on_ground && turned.is_none() {
        return;
    }
    let result = turned.unwrap_or(falling.block);
    let fits = (MIN_Y..=MAX_Y).contains(&pos.y)
        && w.block(pos).is_some_and(block::State::can_be_replaced)
        && !(on_ground && (position.y - position.y.floor()).abs() > 0.001);
    let done = if fits {
        state
            .worlds
            .get_mut(world)
            .is_some_and(|w| w.set_block(pos, result).is_ok())
    } else {
        match falling.block.item() {
            block::ItemForm::Item(t) => {
                drop_item_with(state, world, position, Stack::new(t, 1), None, 10)
            }
            block::ItemForm::None | block::ItemForm::BlockEntity => true,
        }
    };
    if done && let Some(e) = state.rules.entities.get_mut(id) {
        e.flagged = true;
        let key = (e.world, e.chunk);
        state.rules.entities.changed.insert(key);
    }
}

/// The block a falling block turns into before it lands (PMMP `Fallable::tickFalling`):
/// concrete powder next to water (not below it) becomes concrete.
fn harden(world: &World, pos: BlockPos, block: block::State) -> Option<block::State> {
    if !block.is(Class::ConcretePowder) {
        return None;
    }
    let wet = [(0, 1, 0), (0, 0, -1), (0, 0, 1), (-1, 0, 0), (1, 0, 0)]
        .into_iter()
        .any(|(dx, dy, dz)| {
            let side = BlockPos::new(
                pos.x.saturating_add(dx),
                pos.y.saturating_add(dy),
                pos.z.saturating_add(dz),
            );
            world.block(side).is_some_and(|s| s.is(Class::Water))
        });
    if !wet {
        return None;
    }
    let concrete = block.name().strip_suffix("_powder")?;
    block::State::find(concrete, &[])
}

/// An entity hit the ground after falling `fall` blocks (PMMP `onHitGround`): an NPC tramples
/// farmland and takes fall damage ([`npc::landed`]); a falling anvil hurts the players and NPCs
/// under it (PMMP `Anvil::onHitGround`) and may take damage, or break.
fn landed(state: &mut State, id: Id, fall: f64, hits: &mut Vec<(SessionId, f64)>, hooks: Hooks) {
    let Some(e) = state.rules.entities.get(id) else {
        return;
    };
    if matches!(e.kind, Kind::Npc(_)) {
        npc::landed(state, id, fall);
        return;
    }
    let Kind::FallingBlock(falling) = e.kind else {
        return;
    };
    if !falling.block.is(Class::Anvil) {
        return;
    }
    let (world, bounds, runtime_id) = (e.world, e.bounding_box(), e.runtime_id);
    let fallen = fall.round() - 1.0;
    if fallen > 0.0 {
        let damage = crate::math::min(fallen * ANVIL_DAMAGE_PER_BLOCK, ANVIL_MAX_DAMAGE);
        for (&s, p) in &state.players {
            if p.world == world
                && p.phase == Phase::Spawned
                && (hooks.is_alive)(p)
                && player_box(p).overlaps(bounds, EPSILON)
            {
                hits.push((s, damage));
            }
        }
        let at = e.body.position;
        for other in state.rules.entities.nearby(world, bounds) {
            let Some(o) = state.rules.entities.get(other) else {
                continue;
            };
            if other != id && o.kind.is_living() {
                let d = o.body.position - at;
                let knock = Knock::new(d.x, d.z);
                hurt(state, other, Cause::FallingBlock, damage, Some(knock));
            }
        }
    }
    let Some(w) = state.worlds.get(world) else {
        return;
    };
    let mut random = Random::new(
        w.seed(),
        state.tick,
        random::key(ANVIL_RANDOM, &[runtime_id]),
    );
    if random.unit() >= 0.05 + fallen * 0.05 {
        return;
    }
    let rank = ANVILS.iter().position(|&n| n == falling.block.name());
    let Some(e) = state.rules.entities.get_mut(id) else {
        return;
    };
    match rank.and_then(|i| ANVILS.get(i + 1)) {
        Some(next) => {
            let properties: Vec<(&str, block::PropertyValue<'_>)> = falling
                .block
                .properties()
                .map(|p| (p.name(), p.value()))
                .collect();
            if let Some(damaged) = block::State::find(next, &properties) {
                e.kind = Kind::FallingBlock(Falling { block: damaged });
            }
        }
        None => e.flagged = true,
    }
}

/// Players pick up the items they touch (PMMP `Player::checkNearEntities` and
/// `ItemEntity::onCollideWithPlayer`): what fits goes to the inventory; a stack taken whole is
/// shown taken (`TakeItemActor`, then `RemoveActor`), a part leaves the rest on the ground.
/// Stuck arrows are picked up as PMMP `Arrow::onCollideWithPlayer` says.
fn pick_up(state: &mut State, hooks: Hooks) {
    let sessions: Vec<SessionId> = state
        .players
        .iter()
        .filter(|(_, p)| collects(state, p, hooks))
        .map(|(&s, _)| s)
        .collect();
    for session in sessions {
        let Some(player) = state.players.get(&session) else {
            continue;
        };
        let (world, runtime_id) = (player.world, player.runtime_id);
        let reach = player_box(player).grow(PICKUP_REACH);
        let mut taken = 0;
        for id in state.rules.entities.nearby(world, reach) {
            if taken >= MAX_PICKUPS_PER_PLAYER {
                break;
            }
            if projectile::pick_up(state, session, id, hooks) {
                taken += 1;
                continue;
            }
            let Some(e) = state.rules.entities.get_mut(id) else {
                continue;
            };
            let Kind::Item(item) = &mut e.kind else {
                continue;
            };
            if item.pickup_delay != 0 {
                continue;
            }
            let Some(player) = state.players.get_mut(&session) else {
                break;
            };
            let before = item.stack.count();
            let rest = (hooks.give)(player, item.stack.clone());
            if !rest.is_empty() && rest.count() >= before {
                continue;
            }
            taken += 1;
            let key = (e.world, e.chunk);
            let viewers = e.viewers.clone();
            let item_id = e.runtime_id;
            let packet = if rest.is_empty() {
                e.flagged = true;
                state.rules.entities.doomed.insert(id);
                Packet::TakeItemActor(TakeItemActor {
                    item_entity_runtime_id: item_id,
                    taker_entity_runtime_id: runtime_id,
                })
            } else {
                let count = rest.count();
                item.stack = rest;
                stack_size(item_id, count)
            };
            state.rules.entities.changed.insert(key);
            send_to(&mut state.players, &viewers, &packet);
        }
    }
}

/// Removes up to [`MAX_REMOVALS_PER_TICK`] flagged entities, in `Id` order, and tells their
/// viewers.
fn remove_flagged(state: &mut State) {
    for _ in 0..MAX_REMOVALS_PER_TICK {
        let Some(id) = state.rules.entities.doomed.pop_first() else {
            return;
        };
        let Some(e) = state.rules.entities.take(id) else {
            continue;
        };
        if e.runtime_id != 0 {
            send_to(&mut state.players, &e.viewers, &track::remove(e.runtime_id));
        }
    }
}

/// The players who may see entities and other players: their view is open.
fn watchers(state: &State) -> Vec<(SessionId, WorldIndex)> {
    state
        .players
        .iter()
        .filter(|(_, p)| p.view.is_open())
        .map(|(&s, p)| (s, p.world))
        .collect()
}

/// Sends players the entities of the chunks they see and takes away the others, within each
/// player's per-tick bound; then the movement of up to [`MAX_MOVEMENTS_PER_TICK`] entities.
fn track_entities(state: &mut State) {
    let mut budget: BTreeMap<SessionId, u32> = state
        .players
        .keys()
        .map(|&s| (s, MAX_SPAWNS_PER_PLAYER))
        .collect();
    let departed = std::mem::take(&mut state.rules.entities.departed);
    for mut d in departed {
        d.viewers.retain(|s| {
            let Some(left) = budget.get_mut(s) else {
                return false;
            };
            if *left == 0 {
                return true;
            }
            *left -= 1;
            if let Some(p) = state.players.get_mut(s) {
                p.send(track::remove(d.runtime_id));
            }
            false
        });
        if !d.viewers.is_empty() {
            state.rules.entities.departed.push(d);
        }
    }
    let watchers = watchers(state);
    let keys: Vec<Key> = state.rules.entities.columns.keys().copied().collect();
    for key in keys {
        let want: BTreeSet<SessionId> = watchers
            .iter()
            .filter(|&&(s, w)| {
                w == key.0 && state.players.get(&s).is_some_and(|p| p.view.sees(key.1))
            })
            .map(|&(s, _)| s)
            .collect();
        let entities = &mut state.rules.entities;
        let Some(column) = entities.columns.get_mut(&key) else {
            continue;
        };
        if column.viewers == want && !column.stale {
            continue;
        }
        column.viewers.clone_from(&want);
        column.stale = false;
        let ids: Vec<Id> = column.ids.iter().copied().collect();
        let mut held_back = false;
        for id in ids {
            let Some(e) = state.rules.entities.get_mut(id) else {
                continue;
            };
            if e.runtime_id == 0 || e.flagged {
                continue;
            }
            let leaving: Vec<SessionId> = e.viewers.difference(&want).copied().collect();
            let coming: Vec<SessionId> = want.difference(&e.viewers).copied().collect();
            for s in leaving {
                let Some(player) = state.players.get_mut(&s) else {
                    e.viewers.remove(&s);
                    continue;
                };
                match budget.get_mut(&s) {
                    Some(left) if *left > 0 => {
                        *left -= 1;
                        player.send(track::remove(e.runtime_id));
                        e.viewers.remove(&s);
                    }
                    _ => held_back = true,
                }
            }
            for s in coming {
                let Some(player) = state.players.get_mut(&s) else {
                    continue;
                };
                match budget.get_mut(&s) {
                    Some(left) if *left > 0 => {
                        *left -= 1;
                        for packet in track::spawn(e) {
                            player.send(packet);
                        }
                        e.viewers.insert(s);
                    }
                    _ => held_back = true,
                }
            }
        }
        if held_back && let Some(column) = state.rules.entities.columns.get_mut(&key) {
            column.stale = true;
        }
    }
    send_movements(state);
}

/// Sends the movement, speed and flag changes of entities to their viewers (PMMP
/// `updateMovement`), starting where the last pass stopped, at most
/// [`MAX_MOVEMENTS_PER_TICK`] entities with viewers; the others wait in turn. The pass goes
/// over the slots in a ring: most entities rest, and the check of one is a few comparisons,
/// cheaper than keeping an index of the moved ones (measured in `tick_cost`).
fn send_movements(state: &mut State) {
    let entities = &mut state.rules.entities;
    let n = u32::try_from(entities.slots.len()).unwrap_or(u32::MAX);
    if n == 0 {
        entities.cursor = 0;
        return;
    }
    let start = entities.cursor % n;
    let mut sent = 0;
    for k in 0..n {
        let index = (start + k) % n;
        let Some(e) = state
            .rules
            .entities
            .slots
            .get_mut(usize::try_from(index).unwrap_or(usize::MAX))
            .and_then(|s| s.entity.as_mut())
        else {
            continue;
        };
        if e.flagged || e.runtime_id == 0 {
            continue;
        }
        let change = Change::of(e);
        if !change.is_due(e) {
            continue;
        }
        if !e.viewers.is_empty() {
            if sent >= MAX_MOVEMENTS_PER_TICK {
                state.rules.entities.cursor = index;
                return;
            }
            sent += 1;
            for packet in movement(e, change) {
                send_to(&mut state.players, &e.viewers, &packet);
            }
        }
        commit_movement(e, change);
    }
    state.rules.entities.cursor = start;
}

/// The packets an entity's change since it was last sent needs (PMMP `updateMovement`): the
/// flags (no client prediction while it rests, burning), the position, the speed.
fn movement(e: &Entity, change: Change) -> Vec<Packet> {
    let mut packets = Vec::with_capacity(3);
    if change.flags != e.sent.flags || change.radius.is_some() {
        packets.push(track::data(e.runtime_id, change.flags, change.radius));
    }
    if change.moved {
        packets.push(track::movement(e));
    }
    if change.sped {
        packets.push(track::motion(e));
    }
    packets
}

/// What changed of an entity since its viewers were last sent it.
#[derive(Debug, Clone, Copy)]
struct Change {
    /// Its motion is zero now.
    still: bool,
    /// The flags to send.
    flags: u64,
    /// The position or rotation is to be sent.
    moved: bool,
    /// The speed is to be sent.
    sped: bool,
    /// A cloud's radius to send (it changed by [`RADIUS_STEP`] or more).
    radius: Option<f64>,
}

/// The change of a cloud's radius that is sent (PMMP sends each tick's; a twentieth of a block
/// is too small to see and saves nine of ten packets).
const RADIUS_STEP: f64 = 0.05;

impl Change {
    /// What changed of `e`.
    fn of(e: &Entity) -> Self {
        let still = e.body.motion.length_squared() == 0.0;
        let was_still = e.sent.motion.length_squared() == 0.0;
        let now_still = if still == was_still { e.still } else { still };
        let turned = (e.yaw - e.sent.yaw) * (e.yaw - e.sent.yaw)
            + (e.pitch - e.sent.pitch) * (e.pitch - e.sent.pitch)
            > 1.0;
        let radius = match &e.kind {
            Kind::Cloud(c) if (c.radius - e.sent.radius).abs() >= RADIUS_STEP => Some(c.radius),
            _ => None,
        };
        Self {
            still: now_still,
            flags: track::flags(&e.kind, e.fire_ticks > 0, now_still),
            moved: track::has_moved(e) || turned || (!was_still && still),
            sped: (e.body.motion - e.sent.motion).length_squared() > 0.0025 || was_still != still,
            radius,
        }
    }

    /// Whether anything is to be sent.
    fn is_due(self, e: &Entity) -> bool {
        self.moved || self.sped || self.flags != e.sent.flags || self.radius.is_some()
    }
}

/// Records what [`movement`] sent as the entity's last sent state.
fn commit_movement(e: &mut Entity, change: Change) {
    e.still = change.still;
    e.sent.flags = change.flags;
    if let Some(r) = change.radius {
        e.sent.radius = r;
    }
    if change.moved {
        e.sent.position = e.body.position;
        e.sent.yaw = e.yaw;
        e.sent.pitch = e.pitch;
    }
    if change.sped {
        e.sent.motion = e.body.motion;
    }
}

/// Shows players to each other: a player whose chunk another sees is added to it, moved, its
/// equipment changes sent, and removed when it leaves the view, the world or the server.
fn track_players(state: &mut State) {
    let watchers = watchers(state);
    let gone: Vec<SessionId> = state
        .rules
        .entities
        .watched
        .keys()
        .filter(|s| {
            !state
                .players
                .get(s)
                .is_some_and(|p| p.phase == Phase::Spawned)
        })
        .copied()
        .collect();
    for s in gone {
        if let Some(w) = state.rules.entities.watched.remove(&s) {
            send_to(&mut state.players, &w.viewers, &track::remove(w.runtime_id));
        }
    }
    let targets: Vec<SessionId> = state
        .players
        .iter()
        .filter(|(_, p)| p.phase == Phase::Spawned)
        .map(|(&s, _)| s)
        .collect();
    for t in targets {
        let Some(target) = state.players.get(&t) else {
            continue;
        };
        let (world, chunk) = (target.world, chunk_of(target.position));
        let want: BTreeSet<SessionId> = watchers
            .iter()
            .filter(|&&(s, w)| {
                s != t && w == world && state.players.get(&s).is_some_and(|p| p.view.sees(chunk))
            })
            .map(|&(s, _)| s)
            .collect();
        let inv = &target.inventory;
        let now = Watched {
            runtime_id: target.runtime_id,
            viewers: want,
            position: target.position,
            held: inv.held().clone(),
            held_slot: inv.held_slot(),
            off_hand: inv.offhand().clone(),
            armour: inv.armor().clone(),
        };
        let shown = track::Shown {
            runtime_id: target.runtime_id,
            uuid: target.identity.uuid,
            name: &target.identity.name,
            eye: target.position,
            game_mode: survival::game_mode(state, target),
            held: inv.held(),
        };
        let add = track::add_player(shown);
        let before = state.rules.entities.watched.remove(&t);
        let mut packets: Vec<(SessionId, Packet)> = Vec::new();
        let old = before.as_ref().map(|w| &w.viewers);
        for &s in &now.viewers {
            if old.is_some_and(|v| v.contains(&s)) {
                continue;
            }
            packets.push((s, add.clone()));
            packets.push((s, track::armour(now.runtime_id, &now.armour)));
            packets.push((s, track::off_hand(now.runtime_id, &now.off_hand)));
        }
        if let Some(before) = &before {
            for &s in before.viewers.difference(&now.viewers) {
                packets.push((s, track::remove(now.runtime_id)));
            }
            let kept: Vec<SessionId> = before.viewers.intersection(&now.viewers).copied().collect();
            let mut changes = Vec::new();
            if before.position != now.position {
                changes.push(track::move_player(now.runtime_id, now.position, state.tick));
            }
            if before.held != now.held || before.held_slot != now.held_slot {
                changes.push(track::held(now.runtime_id, &now.held, now.held_slot));
            }
            if before.off_hand != now.off_hand {
                changes.push(track::off_hand(now.runtime_id, &now.off_hand));
            }
            if before.armour != now.armour {
                changes.push(track::armour(now.runtime_id, &now.armour));
            }
            for s in kept {
                packets.extend(changes.iter().map(|p| (s, p.clone())));
            }
        }
        for (s, packet) in packets {
            if let Some(p) = state.players.get_mut(&s) {
                p.send(packet);
            }
        }
        state.rules.entities.watched.insert(t, now);
    }
}

/// A non-player character to spawn ([`spawn_npc`]).
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Npc {
    /// A villager-type NPC (network actor `minecraft:villager_v2`, PLAN.md 8.6): no AI.
    Villager {
        /// The profession shown (`EntityVariant`).
        profession: u8,
        /// Its trades.
        offers: super::trade::Offers,
    },
    /// A human NPC (PMMP `Human`): shown as a player with `name`.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "consistency: the M8 API spawns human NPCs; the P6 console needs villagers only"
        )
    )]
    Human {
        /// The name tag.
        name: String,
    },
}

/// Throws the projectile of `item` from `session`'s eyes along its look (snowball, egg, ender
/// pearl, splash or lingering potion, experience bottle; PMMP `ProjectileItem::onClickAir`).
/// False if `item` throws nothing, the player cannot (not spawned, dead, spectator) or the
/// entity limit is reached (the caller keeps the item; it takes one away on true, and keeps the
/// ender pearl cooldown).
#[doc(alias = "ProjectileItem::onClickAir")]
pub(super) fn throw(state: &mut State, session: SessionId, item: &Stack) -> bool {
    projectile::throw_with(state, session, item, HOOKS)
}

/// Releases a bow drawn for `used_ticks` (PMMP `Bow::onReleaseUsing`): shoots an arrow from the
/// held bow, takes an arrow (unless Infinity, or creative) and wears the bow. False if nothing
/// was shot (no bow held, no arrow, drawn under 5 ticks, the entity limit).
#[doc(alias = "Bow::onReleaseUsing")]
pub(super) fn release_bow(state: &mut State, session: SessionId, used_ticks: u32) -> bool {
    projectile::release_bow_with(state, session, used_ticks, HOOKS)
}

/// Turns the TNT block at `pos` into primed TNT with `fuse` ticks (PMMP `TNT::ignite`). False at
/// the entity limit (the block stays) or if the block is not TNT.
#[doc(alias = "TNT::ignite")]
pub(super) fn ignite(state: &mut State, world: WorldIndex, pos: BlockPos, fuse: u32) -> bool {
    explosion::light(state, world, pos, fuse)
}

/// An explosion of `size` at `center` (PMMP `Explosion::explodeA/B`): breaks blocks through
/// `interact::destroy`, hurts and pushes entities and players, sets fire when `fire`. See
/// [`explosion`] for the bounds.
#[doc(alias = "Explosion::explodeB")]
#[expect(
    dead_code,
    reason = "consistency: the M8 API makes explosions (PMMP `Explosion`); primed TNT explodes \
              through `explosion` itself, which the tests run"
)]
pub(super) fn explode(state: &mut State, world: WorldIndex, center: Vec3, size: f64, fire: bool) {
    explosion::explode_with(state, world, center, size, fire, None, HOOKS);
}

/// `session` hits the entity `target` in melee (PMMP `Player::attackEntity`); see [`npc`].
#[doc(alias = "Player::attackEntity")]
pub(super) fn attack(state: &mut State, session: SessionId, target: Id) {
    npc::attack_with(state, session, target, HOOKS);
}

/// `session` interacts with the entity `target` (PMMP `Player::interactEntity`): a villager NPC
/// within reach (8 blocks, in front) opens trading through `trade::open`. False if nothing
/// happened.
#[doc(alias = "Player::interactEntity")]
pub(super) fn interact(state: &mut State, session: SessionId, target: Id) -> bool {
    npc::interact_with(state, session, target, HOOKS, super::trade::open)
}

/// Spawns `npc` at `pos` (feet); `None` at the entity limit, in an unloaded chunk, or for a
/// human name over 256 bytes. NPCs spawned here are invulnerable (only the void hurts them;
/// see [`npc`] for the M8 API).
pub(super) fn spawn_npc(state: &mut State, world: WorldIndex, pos: Vec3, npc: Npc) -> Option<Id> {
    npc::spawn(state, world, pos, npc)
}

/// Whether any entity or player whose box blocks placing intersects `area` in `world` (PMMP
/// `World::getCollidingEntities` for `canBePlaced`: alive entities that can be collided with,
/// players that are not spectators).
pub(super) fn occupied(state: &State, world: WorldIndex, area: Aabb) -> bool {
    let entities = &state.rules.entities;
    let entity = entities.nearby(world, area).into_iter().any(|id| {
        entities
            .get(id)
            .is_some_and(|e| e.kind.blocks_placing() && e.bounding_box().intersects(area))
    });
    entity
        || state.players.values().any(|p| {
            p.world == world && collects(state, p, HOOKS) && player_box(p).intersects(area)
        })
}

/// Whether `world` can take `count` more entities now (below its entity limit), so a caller can
/// refuse an action before it changes anything.
pub(super) fn has_room(state: &State, world: WorldIndex, count: usize) -> bool {
    state
        .rules
        .entities
        .count(world)
        .checked_add(count)
        .is_some_and(|n| n <= MAX_ENTITIES_PER_WORLD)
}

/// The live villager NPC `id` for trading: its world, position (feet), runtime ID (the unique
/// ID `UpdateTrade` names), profession and offers; `None` for anything else, a dead NPC or a stale `Id`.
pub(super) fn villager(
    state: &State,
    id: Id,
) -> Option<(WorldIndex, Vec3, u64, u8, &super::trade::Offers)> {
    let e = state.rules.entities.get(id).filter(|e| !e.flagged)?;
    match &e.kind {
        Kind::Npc(Character {
            form: Form::Villager { profession, offers },
            dead_ticks: None,
            ..
        }) => Some((e.world, e.body.position, e.runtime_id, *profession, offers)),
        _ => None,
    }
}

/// The trade tier or experience of the villager NPC `id` changed from `before` (a trade, a
/// change of its offers): its viewers get the changed keys (BDS, P6 `trade`).
pub(super) fn villager_traded(state: &mut State, id: Id, before: (u8, u32)) {
    let State { players, rules, .. } = state;
    let Some(e) = rules.entities.get(id).filter(|e| !e.flagged) else {
        return;
    };
    let Kind::Npc(Character {
        form: Form::Villager { offers, .. },
        ..
    }) = &e.kind
    else {
        return;
    };
    if let Some(packet) = track::trade_data(e.runtime_id, offers, before) {
        send_to(players, &e.viewers, &packet);
    }
}

/// The offers of the villager NPC `id`, to change (a trade counted); its chunk is saved again.
pub(super) fn offers_mut(state: &mut State, id: Id) -> Option<&mut super::trade::Offers> {
    let entities = &mut state.rules.entities;
    let e = entities.get(id).filter(|e| !e.flagged)?;
    let key = (e.world, e.chunk);
    entities.changed.insert(key);
    match &mut entities.get_mut(id)?.kind {
        Kind::Npc(Character {
            form: Form::Villager { offers, .. },
            dead_ticks: None,
            ..
        }) => Some(offers),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use tokio::sync::mpsc;

    use super::super::queue::Message;
    use super::super::queue::{Input, Outbound};
    use super::super::tests::{
        answer, join, leave, moved_to, new_state, radius, settings, teleported,
    };
    use super::super::{InputLog, Output, boundary, deliver, step};
    use super::physics::tests::named;
    use super::saved::tests::stack;
    use super::*;
    use crate::event::Listeners;
    use crate::protocol::Rng;
    use crate::save::Response;

    thread_local! {
        /// Items the test inventories still take (one player per test takes items).
        static ROOM: Cell<u32> = const { Cell::new(0) };
        /// Items the test inventories took.
        static TAKEN: Cell<u64> = const { Cell::new(0) };
        /// Experience the test players got.
        static EXPERIENCE: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn room(n: u32) {
        ROOM.with(|r| r.set(n));
    }

    pub(super) fn taken() -> u64 {
        TAKEN.with(Cell::get)
    }

    /// A stand-in inventory: takes up to its room.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "consistency: the signature of `inventory::give` it stands in for"
    )]
    fn give(_: &mut Player, stack: Stack) -> Stack {
        let room = ROOM.with(Cell::get);
        let take = room.min(u32::from(stack.count()));
        ROOM.with(|r| r.set(room - take));
        TAKEN.with(|t| t.set(t.get() + u64::from(take)));
        stack.with_count(stack.count() - u8::try_from(take).unwrap())
    }

    /// A stand-in pickup: counts the points, refused for players in `Calls::xp_cooldown`.
    fn pickup_experience(_: &mut State, session: SessionId, amount: u32) -> bool {
        if calls(|c| c.xp_cooldown.contains(&session.get())) {
            return false;
        }
        EXPERIENCE.with(|x| x.set(x.get() + u64::from(amount)));
        true
    }

    /// What the stand-ins of other modules saw (a test's own thread).
    #[derive(Debug, Default)]
    pub(super) struct Calls {
        /// Damage dealt to players: session, amount, cause.
        pub(super) damage: Vec<(u64, f64, survival::Damage)>,
        /// Effects given to players.
        pub(super) effects: Vec<(u64, crate::effect::Instance)>,
        /// Healing of players.
        pub(super) heals: Vec<(u64, f64)>,
        /// Players set on fire: session, seconds.
        pub(super) fires: Vec<(u64, u32)>,
        /// Players whose hit cooldown runs.
        pub(super) hit_cooldown: BTreeSet<u64>,
        /// Players whose orb pickup cooldown runs.
        pub(super) xp_cooldown: BTreeSet<u64>,
        /// Teleports: session, world, eye position.
        pub(super) teleports: Vec<(u64, usize, Vec3)>,
        /// Dead players.
        pub(super) dead: BTreeSet<u64>,
        /// The look of every player: yaw, pitch.
        pub(super) look: (f64, f64),
        /// Arrows the test players carry.
        pub(super) arrows: u32,
    }

    thread_local! {
        /// The calls the stand-ins saw.
        static CALLS: Cell<Calls> = Cell::new(Calls::default());
    }

    /// Runs `f` on the recorded calls (not from inside another `calls`).
    pub(super) fn calls<T>(f: impl FnOnce(&mut Calls) -> T) -> T {
        CALLS.with(|c| {
            let mut calls = c.take();
            let out = f(&mut calls);
            c.set(calls);
            out
        })
    }

    fn damage(_: &mut State, session: SessionId, amount: f64, cause: survival::Damage) -> bool {
        calls(|c| {
            let alive = !c.dead.contains(&session.get());
            if alive {
                c.damage.push((session.get(), amount, cause));
            }
            alive
        })
    }

    fn add_effect(_: &mut State, session: SessionId, effect: crate::effect::Instance) -> bool {
        calls(|c| c.effects.push((session.get(), effect)));
        true
    }

    fn heal(_: &mut State, session: SessionId, amount: f64) {
        calls(|c| c.heals.push((session.get(), amount)));
    }

    fn set_on_fire(_: &mut State, session: SessionId, seconds: u32) {
        calls(|c| c.fires.push((session.get(), seconds)));
    }

    /// Players are named `player<session>` by the tick test harness.
    fn session_of(player: &Player) -> Option<u64> {
        player
            .identity
            .name
            .strip_prefix("player")
            .and_then(|n| n.parse::<u64>().ok())
    }

    fn has_attack_cooldown(player: &Player) -> bool {
        let session = session_of(player);
        calls(|c| session.is_some_and(|s| c.hit_cooldown.contains(&s)))
    }

    /// A stand-in teleport: records it and moves the player.
    fn teleport(state: &mut State, session: SessionId, world: usize, eye: Vec3) {
        calls(|c| c.teleports.push((session.get(), world, eye)));
        if let Some(p) = state.players.get_mut(&session) {
            p.position = eye;
        }
    }

    fn is_alive(player: &Player) -> bool {
        let session = session_of(player);
        calls(|c| session.is_none_or(|s| !c.dead.contains(&s)))
    }

    fn look(_: &Player) -> (f64, f64) {
        calls(|c| c.look)
    }

    fn has_arrow(_: &Player) -> bool {
        calls(|c| c.arrows > 0)
    }

    fn take_arrow(_: &mut Player) -> bool {
        calls(|c| {
            let had = c.arrows > 0;
            c.arrows = c.arrows.saturating_sub(1);
            had
        })
    }

    pub(super) const TEST_HOOKS: Hooks = Hooks {
        give,
        pickup_experience,
        damage,
        has_attack_cooldown,
        add_effect,
        heal,
        set_on_fire,
        teleport,
        is_alive,
        look,
        has_arrow,
        take_arrow,
    };

    /// The eye position of a player standing on the flat ground at block (x, z).
    pub(super) fn standing(x: f32, z: f32) -> [f32; 3] {
        [x + 0.5, -58.38, z + 0.5]
    }

    /// Runs whole ticks without the network: a stand-in save thread, and the packets the rules
    /// queued for each session collected.
    pub(super) struct Harness {
        pub(super) state: State,
        pub(super) pool: Pool,
        answers: Vec<Response>,
        /// Each session's connection end: the packets the output phase sent it.
        receivers: Vec<(u64, mpsc::Receiver<Outbound>)>,
        seen: BTreeMap<u64, Vec<Packet>>,
        seq: BTreeMap<u64, u64>,
    }

    impl Harness {
        pub(super) fn new(threads: usize) -> Self {
            let mut state = new_state(settings(|_| {}), Listeners::default());
            // The preloaded chunks' extras reach the rules as a load's would.
            let loaded: Vec<ChunkPos> = state.worlds[0].loaded().collect();
            for pos in loaded {
                if let Some(extras) = state.chunks.extras.get((0, pos)).cloned() {
                    state.rules.load((0, pos), &extras, &state.worlds[0]);
                }
            }
            Self {
                state,
                pool: crate::runtime::cpu_pool(threads).unwrap(),
                answers: Vec::new(),
                receivers: Vec::new(),
                seen: BTreeMap::new(),
                seq: BTreeMap::new(),
            }
        }

        fn next_seq(&mut self, session: u64) -> u64 {
            let seq = self.seq.entry(session).or_insert(0);
            *seq += 1;
            *seq
        }

        /// One whole tick.
        pub(super) fn step(&mut self, mut inputs: Vec<Input>) {
            let mut output = Output::default();
            let mut log = InputLog::default();
            let mut answers = std::mem::take(&mut self.answers);
            let state = &mut self.state;
            step(
                state,
                &mut inputs,
                &mut answers,
                &self.pool,
                &mut output,
                &mut log,
            );
            deliver(state, &mut output, &mut log);
            boundary(state, &mut output, &mut log);
            let requests = std::mem::take(&mut output.requests);
            self.answers = answer(state, requests);
            self.collect();
        }

        /// One entity tick alone, with the test inventories.
        pub(super) fn entities(&mut self) {
            tick_with(&mut self.state, &self.pool, TEST_HOOKS);
            self.collect();
        }

        /// Collects what the output phase sent each session, then what the rules queued since
        /// (an entity tick alone has no output phase).
        fn collect(&mut self) {
            for (session, rx) in &mut self.receivers {
                let seen = self.seen.entry(*session).or_default();
                while let Ok(out) = rx.try_recv() {
                    if let Outbound::Send(messages) = out {
                        seen.extend(messages.into_iter().filter_map(|m| match m {
                            Message::Packet(p) => Some(p),
                            Message::Shared(_) => None,
                        }));
                    }
                }
            }
            for (s, p) in &mut self.state.players {
                self.seen.entry(s.get()).or_default().append(&mut p.outbox);
            }
        }

        /// Joins `session` standing at `at` with a view of radius 3, spawned.
        pub(super) fn spawn(&mut self, session: u64, at: [f32; 3]) {
            let (j, rx) = join(session, 0);
            self.receivers.push((session, rx));
            self.seq.insert(session, 1);
            self.step(vec![j, radius(session, 1, 3)]);
            self.teleport(session, at);
            self.settle();
            self.state
                .players
                .get_mut(&SessionId::new(session))
                .unwrap()
                .phase = Phase::Spawned;
        }

        /// A client move (at most 15 blocks, the survival rules send farther ones back).
        pub(super) fn go(&mut self, session: u64, at: [f32; 3]) {
            let seq = self.next_seq(session);
            self.step(vec![moved_to(session, seq, at)]);
        }

        /// The client turns to `yaw` and `pitch` where it stands, then one tick (the survival
        /// rules keep the look).
        pub(super) fn look(&mut self, session: u64, yaw: f32, pitch: f32) {
            let seq = self.next_seq(session);
            let position = self.state.players[&SessionId::new(session)]
                .position
                .to_f32();
            let p = crate::protocol::packet::PlayerAuthInput {
                pitch,
                yaw,
                position,
                ..Default::default()
            };
            let input = super::super::tests::input(
                session,
                seq,
                super::super::tests::packet(Packet::PlayerAuthInput(Box::new(p))),
            );
            self.step(vec![input]);
        }

        /// A teleport anywhere, then one tick.
        pub(super) fn teleport(&mut self, session: u64, at: [f32; 3]) {
            teleported(&mut self.state, session, at);
            self.step(Vec::new());
        }

        pub(super) fn settle(&mut self) {
            for _ in 0..8 {
                self.step(Vec::new());
            }
        }

        pub(super) fn take(&mut self, session: u64) -> Vec<Packet> {
            self.seen.remove(&session).unwrap_or_default()
        }

        pub(super) fn runtime_id(&self, session: u64) -> u64 {
            self.state.players[&SessionId::new(session)].runtime_id
        }

        pub(super) fn live(&self) -> Vec<&Entity> {
            let entities = &self.state.rules.entities;
            entities.ids().filter_map(|id| entities.get(id)).collect()
        }

        /// Every item on the ground, by count.
        pub(super) fn ground(&self) -> u64 {
            self.live()
                .iter()
                .filter(|e| !e.flagged)
                .filter_map(|e| match &e.kind {
                    Kind::Item(i) => Some(u64::from(i.stack.count())),
                    _ => None,
                })
                .sum()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            calls(|c| *c = Calls::default());
            room(0);
            TAKEN.with(|t| t.set(0));
            EXPERIENCE.with(|x| x.set(0));
        }
    }

    pub(super) fn at(x: f64, y: f64, z: f64) -> Vec3 {
        Vec3::new(x, y, z)
    }

    pub(super) fn adds(packets: &[Packet]) -> Vec<u64> {
        packets
            .iter()
            .filter_map(|p| match p {
                Packet::AddItemActor(a) => Some(a.entity_runtime_id),
                Packet::AddActor(a) => Some(a.entity_runtime_id),
                Packet::AddPlayer(a) => Some(a.entity_runtime_id),
                _ => None,
            })
            .collect()
    }

    pub(super) fn removes(packets: &[Packet]) -> Vec<u64> {
        packets
            .iter()
            .filter_map(|p| match p {
                Packet::RemoveActor(r) => Some(r.entity_unique_id.cast_unsigned()),
                _ => None,
            })
            .collect()
    }

    fn moves(packets: &[Packet], runtime_id: u64) -> usize {
        packets
            .iter()
            .filter(|p| match p {
                Packet::MoveActorDelta(m) => m.entity_runtime_id == runtime_id,
                Packet::MovePlayer(m) => m.entity_runtime_id == runtime_id,
                _ => false,
            })
            .count()
    }

    #[test]
    fn whole_ticks_give_the_same_state_on_one_or_many_threads() {
        let mut hashes = Vec::new();
        for threads in [1, 4] {
            let mut h = Harness::new(threads);
            h.spawn(1, standing(0.0, 0.0));
            let mut rng = Rng(0xe7_e002);
            let mut trace = Vec::new();
            for tick in 0..120 {
                if tick % 3 == 0 {
                    let xi = i32::try_from(rng.below(16)).unwrap() - 8;
                    let zi = i32::try_from(rng.below(16)).unwrap() - 8;
                    let (x, z) = (f64::from(xi), f64::from(zi));
                    let s = stack("minecraft:stick", u8::try_from(rng.below(64) + 1).unwrap());
                    assert!(drop_item(&mut h.state, 0, at(x + 0.5, -55.0, z + 0.5), s));
                    drop_experience(&mut h.state, 0, at(z, -59.0, x), 40);
                    let pos = BlockPos::new(xi, -50, zi);
                    assert!(fall(&mut h.state, 0, pos, named("minecraft:sand")));
                }
                if tick % 7 == 0 {
                    let x = f32::from(u8::try_from(rng.below(8)).unwrap());
                    h.go(1, standing(x, 1.0));
                } else {
                    h.step(Vec::new());
                }
                trace.push(h.state.hash());
            }
            hashes.push(trace);
        }
        assert_eq!(hashes[0], hashes[1]);
    }

    #[test]
    fn pickups_and_merges_conserve_items() {
        let mut h = Harness::new(2);
        h.spawn(1, standing(0.0, 0.0));
        room(500);
        let mut rng = Rng(0xe7_e003);
        let kinds = [
            "minecraft:stick",
            "minecraft:ender_pearl",
            "minecraft:diamond_sword",
        ];
        let mut dropped = 0u64;
        for tick in 0..400 {
            if tick < 200 && tick % 2 == 0 {
                for _ in 0..3 {
                    let s = stack(kinds[rng.below(3)], 1);
                    let s = s.with_count(
                        u8::try_from(rng.below(usize::from(s.max_count())) + 1).unwrap(),
                    );
                    let x = f64::from(u32::try_from(rng.below(24)).unwrap()) - 12.0;
                    let z = f64::from(u32::try_from(rng.below(24)).unwrap()) - 12.0;
                    dropped += u64::from(s.count());
                    assert!(drop_item(&mut h.state, 0, at(x + 0.5, -58.0, z + 0.5), s));
                }
            }
            h.entities();
            assert_eq!(h.ground() + taken(), dropped, "tick {tick}");
        }
        let got = taken();
        assert!(got > 0 && got <= 500, "{got}");
        // Equal stacks dropped on one spot merge into the largest, and the counts add up.
        for _ in 0..20 {
            assert!(drop_item(
                &mut h.state,
                0,
                at(-12.5, -59.0, -12.5),
                stack("minecraft:stick", 5)
            ));
        }
        let before = h.live().len();
        for _ in 0..60 {
            h.entities();
        }
        assert_eq!(h.ground() + taken(), dropped + 100);
        assert!(
            h.live().len() + 15 <= before,
            "merged: {} of {before}",
            h.live().len()
        );
        let merges = h
            .take(1)
            .iter()
            .filter(|p| matches!(p, Packet::ActorEvent(_)))
            .count();
        assert!(merges > 0);
        assert!(h.live().iter().all(|e| match &e.kind {
            Kind::Item(i) => i.stack.count() <= i.stack.max_count(),
            _ => true,
        }));
    }

    #[test]
    fn what_does_not_fit_stays_on_the_ground_as_the_same_entity() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let player = h.runtime_id(1);
        room(3);
        assert!(drop_item(
            &mut h.state,
            0,
            at(0.5, -59.0, 0.5),
            stack("minecraft:stick", 10)
        ));
        let id = h.live()[0].runtime_id;
        for _ in 0..30 {
            h.entities();
        }
        let left = h.live();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].runtime_id, id);
        let Kind::Item(item) = &left[0].kind else {
            panic!()
        };
        assert_eq!(item.stack.count(), 7);
        assert_eq!(taken(), 3);
        let packets = h.take(1);
        assert!(packets.iter().any(|p| matches!(p,
            Packet::ActorEvent(e) if e.entity_runtime_id == id && e.event_data == 7)));
        assert!(
            !packets
                .iter()
                .any(|p| matches!(p, Packet::TakeItemActor(_)))
        );
        // Room again: the rest is taken whole and shown taken, then removed.
        room(64);
        h.entities();
        assert!(h.live().is_empty());
        let packets = h.take(1);
        let take = packets.iter().position(|p| {
            matches!(p,
            Packet::TakeItemActor(t) if t.item_entity_runtime_id == id
                && t.taker_entity_runtime_id == player)
        });
        let remove = packets.iter().position(|p| {
            matches!(p,
            Packet::RemoveActor(r) if r.entity_unique_id.cast_unsigned() == id)
        });
        assert!(take.unwrap() < remove.unwrap());
        assert_eq!(taken(), 10);
    }

    #[test]
    fn players_not_spawned_pick_nothing_up() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        room(64);
        h.state.players.get_mut(&SessionId::new(1)).unwrap().phase = Phase::Spawning;
        assert!(drop_item(
            &mut h.state,
            0,
            at(0.5, -59.0, 0.5),
            stack("minecraft:stick", 5)
        ));
        for _ in 0..30 {
            h.entities();
        }
        assert_eq!(h.ground(), 5);
    }

    #[test]
    fn a_resting_item_falls_when_a_neighbour_update_reaches_its_block() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(8.0, 8.0));
        let stone = BlockPos::new(0, -60, 0);
        h.state.worlds[0]
            .set_block(stone, named("minecraft:stone"))
            .unwrap();
        assert!(drop_item_with(
            &mut h.state,
            0,
            at(0.5, -58.5, 0.5),
            stack("minecraft:stick", 1),
            Some(Vec3::ZERO),
            10
        ));
        for _ in 0..20 {
            h.entities();
        }
        assert_eq!(h.live()[0].body.position.y, -59.0);
        h.state.worlds[0].set_block(stone, block::AIR).unwrap();
        h.entities();
        assert_eq!(
            h.live()[0].body.position.y,
            -59.0,
            "at rest, nothing moves it"
        );
        // The neighbour update of the block above the one that changed.
        block_changed(&mut h.state, 0, BlockPos::new(0, -59, 0));
        for _ in 0..20 {
            h.entities();
        }
        assert_eq!(h.live()[0].body.position.y, -60.0);
    }

    #[test]
    fn items_despawn_after_6000_ticks() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(8.0, 8.0));
        assert!(drop_item(
            &mut h.state,
            0,
            at(0.5, -59.0, 0.5),
            stack("minecraft:stick", 1)
        ));
        for _ in 0..5999 {
            h.entities();
        }
        assert_eq!(h.live().len(), 1);
        h.entities();
        assert!(h.live().is_empty());
    }

    #[test]
    fn fire_lava_and_cactus_destroy_items_and_fire_proof_items_survive() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(8.0, 8.0));
        for (x, block) in [
            (0, "minecraft:fire"),
            (2, "minecraft:lava"),
            (4, "minecraft:cactus"),
        ] {
            h.state.worlds[0]
                .set_block(BlockPos::new(x, -60, 0), named(block))
                .unwrap();
            let pos = at(f64::from(x) + 0.5, -59.9, 0.5);
            assert!(drop_item_with(
                &mut h.state,
                0,
                pos,
                stack("minecraft:stick", 1),
                Some(Vec3::ZERO),
                10
            ));
        }
        let pos = at(2.5, -59.9, 0.5);
        let ingot = stack("minecraft:netherite_ingot", 1);
        assert!(drop_item_with(
            &mut h.state,
            0,
            pos,
            ingot,
            Some(Vec3::ZERO),
            10
        ));
        for _ in 0..40 {
            h.entities();
        }
        let left = h.live();
        assert_eq!(left.len(), 1, "{left:?}");
        assert!(
            matches!(&left[0].kind, Kind::Item(i) if i.stack.item().unwrap().name() == "minecraft:netherite_ingot")
        );
    }

    #[test]
    fn falling_blocks_land_as_blocks_or_drop_their_item() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(8.0, 8.0));
        let sand = named("minecraft:sand");
        let w = &mut h.state.worlds[0];
        w.set_block(BlockPos::new(0, -50, 0), sand).unwrap();
        w.set_block(BlockPos::new(2, -60, 0), named("minecraft:torch"))
            .unwrap();
        w.set_block(BlockPos::new(2, -50, 0), sand).unwrap();
        let anvil = named("minecraft:anvil");
        w.set_block(BlockPos::new(4, -40, 0), anvil).unwrap();
        assert!(fall(&mut h.state, 0, BlockPos::new(0, -50, 0), sand));
        assert!(fall(&mut h.state, 0, BlockPos::new(2, -50, 0), sand));
        assert!(fall(&mut h.state, 0, BlockPos::new(4, -40, 0), anvil));
        assert_eq!(
            h.state.worlds[0].block(BlockPos::new(0, -50, 0)),
            Some(block::AIR)
        );
        for _ in 0..80 {
            h.entities();
        }
        let w = &h.state.worlds[0];
        assert_eq!(w.block(BlockPos::new(0, -60, 0)), Some(sand));
        assert_eq!(
            w.block(BlockPos::new(2, -60, 0)),
            Some(named("minecraft:torch"))
        );
        let landed = w.block(BlockPos::new(4, -60, 0)).unwrap();
        assert!(
            landed.is(Class::Anvil),
            "an anvil, maybe damaged: {landed:?}"
        );
        let left = h.live();
        assert_eq!(left.len(), 1, "the sand on the torch dropped as an item");
        assert!(
            matches!(&left[0].kind, Kind::Item(i) if i.stack.item().unwrap().name() == "minecraft:sand")
        );
    }

    #[test]
    fn orbs_follow_the_nearest_player_and_give_it_their_experience() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        drop_experience(&mut h.state, 0, at(2.5, -59.0, 0.5), 100);
        let values: Vec<u16> = h
            .live()
            .iter()
            .filter_map(|e| match &e.kind {
                Kind::ExperienceOrb(o) => Some(o.value),
                _ => None,
            })
            .collect();
        assert_eq!(values, vec![73, 17, 7, 3]);
        // Far orbs creep (PMMP's pull fades with distance and ground friction is strong).
        for _ in 0..1200 {
            h.entities();
        }
        assert!(h.live().is_empty());
        assert_eq!(EXPERIENCE.with(Cell::get), 100);
    }

    /// An item a player throws (the Q key) flies along its look at 0.4 and waits 40 ticks before
    /// anyone picks it up (PMMP `Player::dropItem`).
    #[test]
    fn thrown_items_fly_along_the_look() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.look(1, 270.0, 0.0);
        let s = SessionId::new(1);
        inventory::set_held(
            h.state.players.get_mut(&s).unwrap(),
            stack("minecraft:stick", 3),
        );
        assert!(inventory::drop_from_main(&mut h.state, s, 0, 1));
        let live = h.live();
        let [e] = live.as_slice() else {
            panic!("one item: {}", live.len());
        };
        assert!(matches!(&e.kind, Kind::Item(i) if i.pickup_delay == 40));
        let m = e.body.motion;
        assert!(
            (m.x - 0.4).abs() < 1e-9 && m.y.abs() < 1e-9 && m.z.abs() < 1e-9,
            "{m:?}"
        );
    }

    /// An orb the player touches while its pickup cooldown runs stays (PMMP
    /// `ExperienceManager::canPickupXp`), and goes once the player can pick it up.
    #[test]
    fn orbs_stay_while_the_pickup_cooldown_runs() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        calls(|c| c.xp_cooldown.insert(1));
        drop_experience(&mut h.state, 0, at(0.5, -60.0, 0.5), 3);
        for _ in 0..100 {
            h.entities();
        }
        assert_eq!(h.live().len(), 1, "the orb waits");
        assert_eq!(EXPERIENCE.with(Cell::get), 0);
        calls(|c| c.xp_cooldown.clear());
        for _ in 0..100 {
            h.entities();
        }
        assert!(h.live().is_empty());
        assert_eq!(EXPERIENCE.with(Cell::get), 3);
    }

    #[test]
    fn the_entity_cap_refuses_new_entities_and_callers_keep_their_items() {
        let mut h = Harness::new(1);
        for i in 0..u32::try_from(MAX_ENTITIES_PER_WORLD).unwrap() {
            let x = f64::from(i % 64) - 32.0;
            let z = f64::from(i / 64 % 64) - 32.0;
            assert!(drop_item(
                &mut h.state,
                0,
                at(x, -59.0, z),
                stack("minecraft:stick", 1)
            ));
        }
        let next = h.state.next_runtime_id;
        assert!(!drop_item(
            &mut h.state,
            0,
            at(0.5, -59.0, 0.5),
            stack("minecraft:stick", 1)
        ));
        let pos = BlockPos::new(1, -59, 1);
        h.state.worlds[0]
            .set_block(pos, named("minecraft:sand"))
            .unwrap();
        assert!(!fall(&mut h.state, 0, pos, named("minecraft:sand")));
        assert_eq!(h.state.worlds[0].block(pos), Some(named("minecraft:sand")));
        drop_experience(&mut h.state, 0, at(0.5, -59.0, 0.5), 10);
        assert_eq!(h.state.rules.entities.count(0), MAX_ENTITIES_PER_WORLD);
        assert_eq!(h.state.next_runtime_id, next, "no runtime ID is used");
        // Saved entities over the cap stay as saved.
        let mut e = h.live()[0].clone();
        e.body.position = at(100.5, -59.0, 0.5);
        let list = vec![saved::save(&e)];
        let key = (0, ChunkPos::new(6, 0));
        h.state.rules.entities.load(key, &list);
        assert_eq!(h.state.rules.entities.count(0), MAX_ENTITIES_PER_WORLD);
        assert_eq!(h.state.rules.entities.unload(key), None, "unchanged");
        // Not loaded or not a position: refused too.
        let far = at(1000.5, -59.0, 0.5);
        h.state.rules.entities = Entities::default();
        assert!(!drop_item(
            &mut h.state,
            0,
            far,
            stack("minecraft:stick", 1)
        ));
        assert!(!drop_item(
            &mut h.state,
            0,
            at(f64::NAN, 0.0, 0.0),
            stack("minecraft:stick", 1)
        ));
    }

    #[test]
    fn spawning_and_removing_ten_thousand_entities_leaves_every_table_at_its_start() {
        let mut h = Harness::new(2);
        h.spawn(1, standing(0.0, 0.0));
        for _ in 0..4 {
            h.entities();
        }
        h.state.rules.entities.take_changed();
        let mut ids = Vec::new();
        for i in 0..10_000i32 {
            let (xi, zi) = (i % 50 - 25, i / 50 % 50 - 25);
            let (x, z) = (f64::from(xi), f64::from(zi));
            let kind = i % 3;
            let pos = at(x + 0.5, -59.0, z + 0.5);
            match kind {
                0 => assert!(drop_item(
                    &mut h.state,
                    0,
                    pos,
                    stack("minecraft:diamond_sword", 1)
                )),
                1 => drop_experience(&mut h.state, 0, pos, 1),
                _ => {
                    let b = BlockPos::new(xi, -40, zi);
                    h.state.worlds[0]
                        .set_block(b, named("minecraft:gravel"))
                        .unwrap();
                    assert!(fall(&mut h.state, 0, b, named("minecraft:gravel")));
                }
            }
        }
        for _ in 0..5 {
            h.entities();
        }
        ids.extend(h.state.rules.entities.ids());
        assert!(ids.len() > 9_000);
        let numbers: Vec<u64> = h.live().iter().map(|e| e.runtime_id).collect();
        for &id in &ids {
            assert!(remove(&mut h.state, id));
        }
        for _ in 0..40 {
            h.entities();
        }
        h.state.rules.entities.take_changed();
        let Entities {
            slots,
            free,
            retired: _,
            counts,
            columns,
            runtime_ids,
            unassigned,
            kept,
            changed,
            departed,
            watched,
            doomed,
            cursor: _,
            restep,
        } = &h.state.rules.entities;
        assert!(slots.is_empty() && free.is_empty() && counts.is_empty());
        assert!(columns.is_empty() && runtime_ids.is_empty() && unassigned.is_empty());
        assert!(changed.is_empty() && departed.is_empty());
        assert!(doomed.is_empty() && restep.is_empty());
        assert!(
            kept.values().all(|k| !k.is_empty()),
            "only the saved non-entities"
        );
        assert_eq!(watched.len(), 1);
        for id in ids {
            assert!(
                h.state.rules.entities.get(id).is_none(),
                "stale IDs find nothing"
            );
        }
        assert!(numbers.iter().all(|&r| find(&h.state, r).is_none()));
        // Every viewer was told.
        let packets = h.take(1);
        assert_eq!(adds(&packets).len(), removes(&packets).len());
    }

    #[test]
    fn crossing_chunk_borders_and_unloading_keep_exactly_one_copy() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(150.0, 0.0));
        let start = at(159.9, -59.0, 0.5);
        let sword = stack("minecraft:diamond_sword", 1);
        assert!(drop_item_with(
            &mut h.state,
            0,
            start,
            sword.clone(),
            Some(at(0.3, 0.0, 0.0)),
            10
        ));
        h.state.rules.entities.take_changed();
        for _ in 0..4 {
            h.step(Vec::new());
        }
        let e = h.live()[0].clone();
        assert_eq!(e.chunk, ChunkPos::new(10, 0));
        let columns: Vec<Key> = h.state.rules.entities.columns.keys().copied().collect();
        assert!(columns.contains(&(0, ChunkPos::new(10, 0))));
        assert!(!columns.contains(&(0, ChunkPos::new(9, 0))));
        // The view leaves: the entity leaves with its chunk, which waits in memory with it.
        h.teleport(1, standing(0.0, 0.0));
        h.settle();
        assert!(h.live().is_empty());
        let key = (0, ChunkPos::new(10, 0));
        let list = h.state.chunks.unsaved[&key]
            .extras
            .extras()
            .get(crate::storage::ChunkKind::Entities)
            .to_vec();
        let saved: Vec<_> = list
            .iter()
            .filter(|c| c.contains_key("identifier"))
            .collect();
        assert_eq!(saved.len(), 1);
        // And comes back, once.
        let kept = saved::load(saved[0], ChunkPos::new(10, 0)).unwrap();
        h.teleport(1, standing(150.0, 0.0));
        let back = h.live();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].body, kept.body);
        assert_eq!(back[0].kind, kept.kind);
        h.settle();
        let back = h.live();
        assert_eq!(back.len(), 1);
        assert_ne!(
            back[0].runtime_id, e.runtime_id,
            "runtime IDs are not reused"
        );
    }

    #[test]
    fn unloading_and_loading_round_trips_a_chunk_list_with_what_is_not_ours() {
        let mut h = Harness::new(1);
        let key = (0, ChunkPos::new(0, 0));
        let pig = Compound::from([(
            "identifier".to_owned(),
            crate::nbt::Value::String("minecraft:pig".to_owned()),
        )]);
        let entities = &mut h.state.rules.entities;
        entities.unload(key);
        entities.load(key, std::slice::from_ref(&pig));
        assert_eq!(entities.unload(key), None, "nothing changed");
        entities.load(key, std::slice::from_ref(&pig));
        assert!(drop_item(
            &mut h.state,
            0,
            at(1.5, -59.0, 1.5),
            stack("minecraft:stick", 3)
        ));
        let list = h.state.rules.entities.unload(key).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1], pig);
        h.state.rules.entities.load(key, &list);
        assert_eq!(h.live().len(), 1);
        assert_eq!(h.state.rules.entities.take_changed(), Vec::new());
        h.state.rules.entities.changed.insert(key);
        let again = h.state.rules.entities.take_changed();
        assert_eq!(again, vec![(key, list)]);
    }

    #[test]
    fn players_are_sent_the_entities_of_the_chunks_they_see_and_only_those() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.spawn(2, standing(300.0, 0.0));
        h.take(1);
        h.take(2);
        assert!(drop_item(
            &mut h.state,
            0,
            at(1.5, -55.0, 1.5),
            stack("minecraft:stick", 1)
        ));
        let id = h.live()[0].runtime_id;
        h.step(Vec::new());
        assert_eq!(adds(&h.take(1)), vec![id]);
        assert!(adds(&h.take(2)).is_empty());
        for _ in 0..3 {
            h.step(Vec::new());
        }
        assert!(moves(&h.take(1), id) >= 2, "the fall is sent");
        assert_eq!(moves(&h.take(2), id), 0);
        // Player 2 comes near: it is sent the item; player 1 goes away: it is told it left.
        h.teleport(2, standing(4.0, 4.0));
        h.settle();
        assert_eq!(adds(&h.take(2)), vec![id, h.runtime_id(1)]);
        h.teleport(1, standing(300.0, 0.0));
        h.settle();
        let packets = h.take(1);
        assert!(removes(&packets).contains(&id));
        assert_eq!(
            removes(&h.take(2)),
            vec![h.runtime_id(1)],
            "1 left 2's view"
        );
        // The item goes: only player 2 is told.
        let entity = h.state.rules.entities.ids().next().unwrap();
        assert!(remove(&mut h.state, entity));
        h.step(Vec::new());
        assert_eq!(removes(&h.take(2)), vec![id]);
        assert!(removes(&h.take(1)).is_empty());
    }

    #[test]
    fn items_are_shown_and_moved_as_bds_sends_them() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.take(1);
        assert!(drop_item(
            &mut h.state,
            0,
            at(1.5, -55.0, 1.5),
            stack("minecraft:stick", 1)
        ));
        h.step(Vec::new());
        let (id, feet) = (h.live()[0].runtime_id, h.live()[0].body.position);
        let shown = |p: Vec3| [p.x, p.y + 0.125, p.z].map(track_f32);
        let added = h.take(1).into_iter().find_map(|p| match p {
            Packet::AddItemActor(a) if a.entity_runtime_id == id => Some(a),
            _ => None,
        });
        assert_eq!(
            added.unwrap().position,
            shown(feet),
            "BDS: 0.125 above the feet"
        );
        // While it falls, every tick sends its new position with Ticks 0.
        for _ in 0..5 {
            h.step(Vec::new());
            let feet = h.live()[0].body.position;
            let sent: Vec<crate::protocol::packet::MoveActorDelta> = h
                .take(1)
                .into_iter()
                .filter_map(|p| match p {
                    Packet::MoveActorDelta(m) if m.entity_runtime_id == id => Some(m),
                    _ => None,
                })
                .collect();
            let [moved] = sent.as_slice() else {
                panic!("one move a tick: {sent:?}");
            };
            assert_eq!(moved.ticks, 0);
            let [shown_x, shown_y, shown_z] = shown(feet);
            assert_eq!(moved.position_y, Some(shown_y));
            // An axis that did not change is left out.
            assert!(
                moved.position_x.is_none_or(|v| v == shown_x)
                    && moved.position_z.is_none_or(|v| v == shown_z)
            );
        }
    }

    /// An `f32` as packets carry it.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "consistency: the test rounds as the packets do"
    )]
    fn track_f32(v: f64) -> f32 {
        v as f32
    }

    #[test]
    fn a_view_full_of_entities_is_sent_within_the_per_tick_bound() {
        let mut h = Harness::new(1);
        for i in 0..1200u32 {
            let x = f64::from(i % 40) * 0.25;
            let z = f64::from(i / 40) * 0.25;
            let sword = stack("minecraft:diamond_sword", 1);
            assert!(drop_item_with(
                &mut h.state,
                0,
                at(x + 3.0, -60.0, z + 3.0),
                sword,
                Some(Vec3::ZERO),
                10
            ));
        }
        let (j, rx) = join(1, 0);
        h.receivers.push((1, rx));
        h.step(vec![j, radius(1, 1, 3)]);
        let mut counts = vec![adds(&h.take(1)).len()];
        for _ in 0..8 {
            h.step(Vec::new());
            counts.push(adds(&h.take(1)).len());
        }
        let sent: Vec<usize> = counts.into_iter().filter(|&n| n > 0).collect();
        assert_eq!(sent, [512, 512, 176]);
    }

    #[test]
    fn placing_is_refused_where_a_solid_entity_or_a_player_stands_and_room_is_counted() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let cell = |x: f64, y: f64, z: f64| Aabb::new(at(x, y, z), at(x + 1.0, y + 1.0, z + 1.0));
        assert!(occupied(&h.state, 0, cell(0.0, -60.0, 0.0)), "the player");
        assert!(!occupied(&h.state, 0, cell(4.0, -60.0, 0.0)));
        assert!(drop_item(
            &mut h.state,
            0,
            at(4.5, -60.0, 0.5),
            stack("minecraft:stick", 1)
        ));
        assert!(
            !occupied(&h.state, 0, cell(4.0, -60.0, 0.0)),
            "items do not block"
        );
        let tnt = Kind::Tnt(Tnt {
            fuse: 80,
            underwater: false,
        });
        assert!(
            create(&mut h.state, 0, at(6.5, -60.0, 0.5), tnt, |_| (
                Vec3::ZERO,
                0.0,
                0.0
            ))
            .is_some()
        );
        assert!(
            occupied(&h.state, 0, cell(6.0, -60.0, 0.0)),
            "primed TNT blocks"
        );
        let npc = Npc::Human {
            name: "Guide".to_owned(),
        };
        assert!(spawn_npc(&mut h.state, 0, at(8.5, -60.0, 0.5), npc).is_some());
        assert!(
            occupied(&h.state, 0, cell(8.0, -59.0, 0.0)),
            "an NPC blocks"
        );
        assert!(has_room(&h.state, 0, MAX_ENTITIES_PER_WORLD - 3));
        assert!(!has_room(&h.state, 0, MAX_ENTITIES_PER_WORLD - 2));
        assert!(!has_room(&h.state, 0, usize::MAX));
    }

    #[test]
    fn a_falling_anvil_hurts_the_players_and_npcs_under_it() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let anvil = named("minecraft:anvil");
        let pos = BlockPos::new(0, -48, 0);
        h.state.worlds[0].set_block(pos, anvil).unwrap();
        assert!(fall(&mut h.state, 0, pos, anvil));
        for _ in 0..60 {
            h.entities();
        }
        let hits = calls(|c| c.damage.clone());
        assert_eq!(hits.len(), 1, "{hits:?}");
        let (who, amount, cause) = hits[0];
        assert_eq!((who, cause), (1, survival::Damage::Entity));
        // It fell about 10 blocks onto the player's head: (round(fall) − 1) × 2.
        assert!(
            (14.0..=26.0).contains(&amount) && amount % 2.0 == 0.0,
            "{amount}"
        );
    }

    #[test]
    fn every_kind_spawned_and_removed_a_thousand_times_leaves_every_table_at_its_start() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        for _ in 0..4 {
            h.entities();
        }
        h.take(1);
        let kinds: Vec<Kind> = saved::tests::kinds()
            .into_iter()
            .map(|k| match k {
                Kind::Tnt(t) => Kind::Tnt(Tnt { fuse: 100, ..t }),
                Kind::ExperienceOrb(o) => Kind::ExperienceOrb(Orb {
                    despawn_delay: DEFAULT_DESPAWN_DELAY,
                    ..o
                }),
                other => other,
            })
            .collect();
        let mut spawned = 0usize;
        for round in 0..1000u32 {
            let mut ids = Vec::new();
            for (i, kind) in (0u32..).zip(&kinds) {
                let x = f64::from((round * 7 + i * 3) % 40) - 20.0;
                let z = f64::from((round * 3 + i * 5) % 40) - 20.0;
                let made = create(&mut h.state, 0, at(x, -30.0, z), kind.clone(), |_| {
                    (Vec3::ZERO, 0.0, 0.0)
                });
                ids.push(made.expect("room"));
            }
            spawned += ids.len();
            h.entities();
            for id in ids {
                remove(&mut h.state, id);
            }
            h.entities();
        }
        assert_eq!(spawned, 1000 * kinds.len());
        for _ in 0..5 {
            h.entities();
        }
        h.state.rules.entities.take_changed();
        let Entities {
            slots,
            free,
            counts,
            columns,
            runtime_ids,
            unassigned,
            changed,
            departed,
            doomed,
            restep,
            ..
        } = &h.state.rules.entities;
        assert!(slots.is_empty() && free.is_empty() && counts.is_empty());
        assert!(columns.is_empty() && runtime_ids.is_empty() && unassigned.is_empty());
        assert!(changed.is_empty() && departed.is_empty() && doomed.is_empty());
        assert!(restep.is_empty());
        let packets = h.take(1);
        assert_eq!(adds(&packets).len(), removes(&packets).len());
        assert!(adds(&packets).len() >= spawned, "every kind was shown");
    }

    #[test]
    fn players_see_each_other_move_and_leave() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.spawn(2, standing(3.0, 3.0));
        let (one, two) = (h.runtime_id(1), h.runtime_id(2));
        h.step(Vec::new());
        assert!(adds(&h.take(1)).contains(&two));
        assert!(adds(&h.take(2)).contains(&one));
        h.go(1, standing(1.0, 0.0));
        h.step(Vec::new());
        assert_eq!(moves(&h.take(2), one), 1);
        assert_eq!(moves(&h.take(1), one), 0, "not to itself");
        h.step(vec![leave(1, 99)]);
        h.step(Vec::new());
        assert_eq!(removes(&h.take(2)), vec![one]);
        assert!(h.state.rules.entities.watched.keys().all(|s| s.get() == 2));
    }

    /// Measures the entity tick (PLAN.md 10.7 "성능") with one player who sees every item:
    /// `cargo test --release --lib tick::entity::tests::tick_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement"]
    #[expect(
        clippy::print_stderr,
        clippy::disallowed_methods,
        reason = "consistency: a measurement prints its result and reads the wall clock"
    )]
    fn tick_cost() {
        let most = std::thread::available_parallelism().map_or(4, |n| n.get().max(4));
        for (threads, count) in [1, most]
            .into_iter()
            .flat_map(|t| [(t, 1_000u32), (t, 10_000)])
        {
            for falling in [false, true] {
                let mut h = Harness::new(threads);
                h.spawn(1, standing(0.0, 0.0));
                let y = if falling { 200.0 } else { -60.0 };
                for i in 0..count {
                    // Four to a block over 50 × 50 blocks around the player; swords do not merge.
                    let x = f64::from(i % 100) * 0.5 - 25.0;
                    let z = f64::from(i / 100) * 0.5 - 25.0;
                    let sword = stack("minecraft:diamond_sword", 1);
                    let pos = at(x + 0.25, y, z + 0.25);
                    assert!(drop_item_with(
                        &mut h.state,
                        0,
                        pos,
                        sword,
                        Some(Vec3::ZERO),
                        10
                    ));
                }
                // Idle: let them settle and be sent first; falling: measure the first ticks.
                let warm = if falling { 1 } else { 60 };
                for _ in 0..warm {
                    h.entities();
                    h.seen.clear();
                }
                let ticks = 40u32;
                let mut phases = [std::time::Duration::ZERO; 5];
                for _ in 0..ticks {
                    let state = &mut h.state;
                    let mut lap = std::time::Instant::now();
                    let mut mark = |i: usize, lap: &mut std::time::Instant| {
                        phases[i] += lap.elapsed();
                        *lap = std::time::Instant::now();
                    };
                    assign_runtime_ids(state);
                    let steps = compute(state, &h.pool);
                    mark(0, &mut lap);
                    apply_steps(state, steps, TEST_HOOKS);
                    mark(1, &mut lap);
                    pick_up(state, TEST_HOOKS);
                    remove_flagged(state);
                    mark(2, &mut lap);
                    track_entities(state);
                    mark(3, &mut lap);
                    track_players(state);
                    mark(4, &mut lap);
                    h.collect();
                    h.seen.clear();
                }
                let total: std::time::Duration = phases.iter().sum();
                let state = if falling { "falling" } else { "idle" };
                let [compute, apply, removals, tracking, players] = phases.map(|p| p / ticks);
                eprintln!(
                    "{count} {state} items, {threads} threads: {:?} per tick",
                    total / ticks
                );
                eprintln!(
                    "  compute {compute:?}, apply {apply:?}, pickups and removals {removals:?}"
                );
                eprintln!("  tracking {tracking:?}, players {players:?}");
            }
        }
    }
}
