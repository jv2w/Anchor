//! Entities on the tick: the store of every world, physics, tracking and the per-kind rules
//! (dropped items, experience orbs, falling blocks, primed TNT and explosions, projectiles, potion
//! clouds, NPCs, custom entities ([`custom`]), paintings, end crystals and firework rockets
//! ([`object`])) (PMMP `Entity`, `ItemEntity`, `ExperienceOrb`, `FallingBlock`, `PrimedTNT`,
//! `Explosion`, `Projectile` and its kinds, `AreaEffectCloud`, `Living`, `Human`, `Villager`,
//! `Painting`, `EndCrystal`, `FireworkRocket`, `World::dropItem`, `World::dropExperience`), and
//! lightning bolts, which are shown like entities but are none ([`lightning`]).
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
//! # Tick
//!
//! Entities in chunks within `tick-radius` of a player tick (like blocks; the rest wait).
//! 1. Compute (parallel, read-only): each entity's movement against the blocks
//!    ([`physics::step`]) on the CPU pool.
//! 2. Apply (serial, `Id` order): the new bodies (a projectile first hits the nearest entity or
//!    player on its ray, else the block the ray met), the blocks they are inside, burning, the
//!    void, then the kind's rules: items count down their pickup delay, merge with equal stacks
//!    around them and despawn after `item-despawn-ticks`; orbs follow the nearest player within 8 blocks
//!    and give it their experience; falling blocks land as blocks or drop their item; primed TNT
//!    burns down and explodes ([`explosion`], within a per-tick budget); stuck arrows go after a
//!    minute; clouds shrink and give their effects; NPCs count down their hit cooldown and die;
//!    end crystals that died explode (within the same budget); firework rockets speed up and
//!    burst when their flight time is up.
//!    An entity whose body a rule changed after its step was computed (a knockback, a push), and
//!    the entities near an explosion of this tick (whose blocks changed), move again from their
//!    new state when their turn comes (serially), as PMMP's one-by-one update would. Then
//!    players pick up the items and stuck arrows they touch (what does not fit stays on the
//!    ground).
//! 3. Removals, then tracking: players who see an entity's chunk are sent it, its movement and
//!    its removal; players who see each other's chunk are sent each other. Showing and taking
//!    away entities and players share one bound per player and tick
//!    ([`MAX_SPAWNS_PER_PLAYER`]), and movements have their own ([`MAX_MOVEMENTS_PER_TICK`]);
//!    what is over a bound waits for the next tick. The removal of an entity that goes or of a
//!    player that leaves, the moves and equipment of players, and what a hit or a merge shows
//!    are sent at once.
//!
//! # Events
//!
//! The rules raise their events only in the serial parts of the tick: the physics computes in
//! parallel and its results apply in `Id` order, where the hits, pickups, merges, landings and
//! explosions raise theirs. An entity a listener changes through [`Mut`] while the steps apply
//! moves on from its new state, not by the step computed before. Spawns are announced by the
//! entity tick (before the physics and again before the removals; PMMP's `onFirstUpdate`), so a
//! rule that creates an entity in the middle of its work, and the direct API, never call
//! listeners there; removals are announced as they happen and entities that leave with their
//! chunk at the next entity tick. Direct API calls raise nothing ([`Raise::Quiet`]). Without
//! listeners and monitors no event is built and every rule runs as before.
//!
//! # Saving
//!
//! Each loaded chunk's entities are its extras of kind 2 ([`saved`]). Compounds that are not an
//! entity Anchor knows, and entities over the world's cap, are kept as saved and written back.
//! A chunk changes when one of its entities ticks, arrives, leaves or is removed. Entities of
//! a chunk that unloads leave the store with it; viewers are sent their removal next tick.
//! Temporary worlds are never saved (the snapshot skips them).

mod api;
mod custom;
mod explosion;
mod kind;
mod lightning;
mod npc;
mod object;
mod physics;
mod pick;
mod projectile;
mod saved;
mod track;

pub use api::Mut;
pub(crate) use custom::{Customs, EntityTypes};
pub(super) use explosion::Blast;
pub use kind::MAX_NAME_TAG_BYTES;
pub(super) use object::Placed;
pub(crate) use physics::bounded as bounded_motion;
pub(super) use physics::{
    Harm, INSIDE_INSET, MOTION_THRESHOLD, Touch, cells_along, collisions, is_inside_solid,
    is_solid, liquid_depth,
};
pub(super) use pick::on_actor_pick_request;
pub(super) use track::locator_life;

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use tracing::{debug, warn};

use super::interact::breaking::damage as worn;
use super::interact::drops::level as level_of;
use super::interact::level_event;
use super::survival::{EYE_HEIGHT, PLAYER_HEIGHT, PLAYER_WIDTH};
use super::view::{Key, View, chunk_of};
use super::{Phase, Player, SessionId, State, dispatch, inventory, survival};
use crate::block::{self, Class};
use crate::config::GameMode;
use crate::entity::Id;
use crate::event::{
    Actor, CombustSource, DamageCause, DamageModifier, DamageSource, EntityBlockChange,
    EntityCombust, EntityDamage, EntityDeath, EntityDespawn, EntityExtinguish, EntityItemPickup,
    EntityMotion, EntityRegainHealth, EntitySpawn, ExtinguishCause, ItemDespawn, ItemMerge,
    ItemSpawn, MAX_DAMAGE, RegainReason,
};
use crate::item::Stack;
use crate::math::{Aabb, Vec3, to_f32};
use crate::nbt::Compound;
use crate::pool::Pool;
use crate::protocol::consts::ActorEvent;
use crate::protocol::packet::{ActorEvent as ActorEventPacket, Packet, TakeItemActor};
use crate::random::{self, Random};
use crate::save::WorldIndex;
use crate::world::{self, BlockPos, ChunkPos, MAX_Y, MIN_Y, World};

use kind::{Cause, Character, DEFAULT_DESPAWN_DELAY, Falling, Form, Item, Kind, NEVER, Orb};
#[cfg(test)]
use kind::{Cloud, Crystal, Firework, Painting, Projectile, Shot, Tnt};
use physics::{Body, Inside, Step};

/// The most entities one world holds. `tick_cost` measures the tick of 10,000 falling items at a
/// few milliseconds, and 16,384 keeps the store's memory near 6 MiB; PMMP and dragonfly have no
/// cap. At the cap, actions that would create an entity are refused and the caller keeps its
/// items.
pub const MAX_ENTITIES_PER_WORLD: usize = 16_384;
/// The most players one player hides (`Context::hide_player`).
pub(super) const MAX_HIDDEN_PER_PLAYER: usize = 1024;
/// Entities and players one player is shown or has taken away per tick, whatever packets each
/// takes; the rest wait (a view entering a chunk full of items, or a crowd teleported together,
/// fills in over ticks instead of overflowing the outbox).
const MAX_SPAWNS_PER_PLAYER: u32 = 512;
/// Entities whose movement is sent per tick (each up to four packets per viewer); the others
/// keep their last sent state and are sent in a later tick, in turn.
const MAX_MOVEMENTS_PER_TICK: usize = 256;
/// Item merges per tick; later candidates check again two ticks on.
const MAX_MERGES_PER_TICK: usize = 256;
/// Entities removed per tick (despawned, picked up, burnt, merged); the rest are removed in the
/// next ticks and are ignored meanwhile.
const MAX_REMOVALS_PER_TICK: usize = 512;
/// The most chunks a "what is near" query looks up one by one; a larger area walks the chunk
/// index instead ([`Entities::nearby`]).
const MAX_CHUNK_LOOKUPS: u64 = 64;
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
/// How far around a player's box it picks items up (PMMP `checkNearEntities`).
const PICKUP_REACH: Vec3 = Vec3::new(1.0, 0.5, 1.0);
/// How far around an item's box it looks for stacks to merge with.
const MERGE_REACH: Vec3 = Vec3::new(0.5, 0.5, 0.5);
/// The overlap two boxes need to touch (PMMP `intersectsWith`).
pub(super) const EPSILON: f64 = 0.000_01;
/// The fall damage of an anvil per block fallen and at most (PMMP `Anvil`).
const ANVIL_DAMAGE_PER_BLOCK: f64 = 2.0;
const ANVIL_MAX_DAMAGE: f64 = 40.0;
/// The anvil blocks from whole to most damaged; one more hit on the last breaks it.
const ANVILS: [&str; 3] = [
    "minecraft:anvil",
    "minecraft:chipped_anvil",
    "minecraft:damaged_anvil",
];
/// Random streams (`crate::random`), one per purpose.
const DROP_RANDOM: u64 = random::tag("entity-drop");
const DEATH_RANDOM: u64 = random::tag("entity-death-drops");
const UNSTICK_RANDOM: u64 = random::tag("entity-unstick");
const ANVIL_RANDOM: u64 = random::tag("entity-anvil-land");
/// Horizontal knockback of a hit on an entity or a player (PMMP `Living::DEFAULT_KNOCKBACK_FORCE`).
pub(super) const KNOCKBACK: f64 = 0.4;
/// The most upward knockback (PMMP `Living::DEFAULT_KNOCKBACK_VERTICAL_LIMIT`).
pub(super) const KNOCKBACK_VERTICAL_LIMIT: f64 = 0.4;
/// Ticks within which a weaker hit on an entity or a player does nothing (PMMP
/// `EntityDamageEvent::$attackCooldown`).
pub(super) const ATTACK_COOLDOWN: u8 = 10;
/// Damage the void deals an entity or a player each tick (PMMP `Entity::entityBaseTick`).
pub(super) const VOID_DAMAGE: f64 = 10.0;
/// At or below this feet height the void hurts: 16 under the lowest block.
pub(super) const VOID_Y: f64 = (MIN_Y - 16) as f64;
/// Ticks between the damage of burning (PMMP `Entity::doOnFireTick`).
pub(super) const FIRE_DAMAGE_TICKS: u8 = 20;
/// Fall distance that does not hurt (PMMP `Living::calculateFallDamage`).
pub(super) const SAFE_FALL: f64 = 3.0;
/// Damage of a tick with the eyes inside a solid block (PMMP `Living::entityBaseTick`).
pub(super) const SUFFOCATION_DAMAGE: f64 = 1.0;
/// The experience a zombie drops (PMMP `Zombie::getXpDropAmount`).
const ZOMBIE_XP: u32 = 5;
/// The change of a cloud's radius that is sent (PMMP sends each tick's; a twentieth of a block
/// is too small to see and saves nine of ten packets).
const RADIUS_STEP: f64 = 0.05;
/// The fuse of lit TNT in ticks (PMMP `TNT::ignite` default, 4 seconds), and of saved primed TNT
/// without one (PMMP `PrimedTNT::initEntity`).
pub(super) const TNT_FUSE: u16 = 80;
/// The hooks of the running server.
const HOOKS: Hooks = Hooks {
    give: inventory::give,
    room,
    pickup_experience: survival::pickup_experience,
    damage: survival::damage_by,
    has_attack_cooldown: survival::has_attack_cooldown,
    add_effect: survival::add_effect,
    heal: survival::heal,
    set_on_fire: survival::set_on_fire_by,
    teleport: survival::teleport,
    is_alive: survival::is_alive,
    look: survival::rotation,
    has_arrow: inventory::has_arrow,
    take_arrow: inventory::take_arrow,
};

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
    /// The health (sent to the viewers of custom entities only).
    health: f64,
}

super::checkpoint::fields!(Sent {
    position,
    motion,
    yaw,
    pitch,
    flags,
    radius,
    health,
});

/// What a hit did to an entity ([`Entity::take_hit`]).
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

impl Knock {
    /// The default knockback away along (`x`, `z`).
    fn new(x: f64, z: f64) -> Self {
        Self {
            x,
            z,
            force: KNOCKBACK,
            vertical: KNOCKBACK_VERTICAL_LIMIT,
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

/// How a hit would hurt an entity ([`Entity::plan_hit`]).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Plan {
    /// An NPC's cooldown is over: the hit starts it, knocks back and shows.
    cold: bool,
    /// The base damage of the last hit within an NPC's cooldown, which this hit's damage counts
    /// less (PMMP `MODIFIER_PREVIOUS_DAMAGE_COOLDOWN`).
    previous: Option<f64>,
}

/// What a hit takes ([`Entity::take_hit`]): the planned values, or a damage event's.
#[derive(Debug, Clone, Copy)]
struct Taken {
    /// The damage before the modifiers, which an NPC remembers for its cooldown.
    base: f64,
    /// The health it loses.
    damage: f64,
    /// The cooldown a cold hit starts, in ticks.
    cooldown: u8,
}

impl Taken {
    /// A hit of `base` as planned, without an event.
    fn of(plan: Plan, base: f64) -> Self {
        Self {
            base,
            damage: plan.previous.map_or(base, |p| base - p),
            cooldown: ATTACK_COOLDOWN,
        }
    }
}

/// Damage a rule deals to an entity ([`hurt`]): why, how much, and what deals it (for
/// [`EntityDamage`]).
#[derive(Debug, Clone, Copy)]
struct Blow {
    cause: Cause,
    amount: f64,
    source: DamageSource,
}

impl Blow {
    /// `amount` of damage for `cause` from nothing in particular.
    fn of(cause: Cause, amount: f64) -> Self {
        Self {
            cause,
            amount,
            source: DamageSource::None,
        }
    }

    /// The same damage from `source`.
    fn by(self, source: DamageSource) -> Self {
        Self { source, ..self }
    }
}

/// Whether a rule raises the events of what it does: the game's own actions do, the direct API
/// (a [`Context`](crate::Context) call) does not (no re-entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Raise {
    /// The rule raises its events.
    Events,
    /// The rule raises none.
    Quiet,
}

/// An entity, as a [`Context`](crate::Context) lends it (PMMP `Entity`): what it is and where;
/// [`Mut`] changes it.
#[derive(Debug, Clone)]
// pmmp-class: Entity
pub struct Entity {
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
    /// The name tag (at most [`kind::MAX_NAME_TAG_BYTES`]); a human NPC's is its name.
    name_tag: String,
    /// The name tag shows when looked at (PMMP `nameTagVisible`).
    name_tag_visible: bool,
    /// The name tag shows always (PMMP `alwaysShowNameTag`; human NPCs start with it).
    name_tag_always_visible: bool,
    /// The size multiplier of its box and model, above 0 and within [`is_valid_scale`].
    scale: f64,
    /// Not drawn by clients (PMMP `invisible`).
    invisible: bool,
    /// Gravity pulls it (PMMP `gravityEnabled`).
    gravity: bool,
    /// Saved with its chunk (PMMP `savedWithChunk`); one that is not goes when the chunk unloads.
    saved: bool,
    /// Its metadata changed (name tag, scale, kind values): the viewers get it at the next
    /// movement pass.
    resend: bool,
    /// Gone: removed at the end of a tick, ignored until then.
    flagged: bool,
    /// Its motion was zero at the last movement check (PMMP `noClientPredictions`).
    still: bool,
    /// The players it was sent to.
    viewers: BTreeSet<SessionId>,
    /// What the viewers were last sent.
    sent: Sent,
}

impl Entity {
    /// A new entity at `body` (not yet in the store). One at rest is still from the start: PMMP
    /// sets `noClientPredictions` only when the motion changes, so an entity that never moves
    /// would be left to the client, which pushes it where the server never put it.
    fn new(world: WorldIndex, body: Body, yaw: f64, pitch: f64, kind: Kind) -> Self {
        let health = kind.max_health();
        let still = body.motion.length_squared() == 0.0;
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
            name_tag_always_visible: kind.shows_name_always(),
            name_tag: kind.default_name_tag().to_owned(),
            saved: kind.is_saved(),
            kind,
            name_tag_visible: true,
            scale: 1.0,
            invisible: false,
            gravity: true,
            resend: false,
            flagged: false,
            still,
            viewers: BTreeSet::new(),
            sent: Sent {
                position: body.position,
                motion: body.motion,
                yaw,
                pitch,
                flags: 0,
                radius,
                health,
            },
        };
        e.sent.flags = track::flags(&e, still);
        e
    }

    /// Width and height of its box: its kind's, times its scale (PMMP `EntitySizeInfo::scale`).
    fn size(&self) -> (f64, f64) {
        let (width, height) = self.kind.size();
        (width * self.scale, height * self.scale)
    }

    /// The physics constants it moves with: its kind's, scaled, and without gravity when it has
    /// none (a cloud keeps its kind's small box: only its drawn box follows its radius).
    fn physics(&self) -> kind::Physics {
        let kind = self.kind.physics();
        kind::Physics {
            width: kind.width * self.scale,
            height: kind.height * self.scale,
            gravity: if self.gravity { kind.gravity } else { 0.0 },
            ..*kind
        }
    }

    /// Its box (PMMP `boundingBox`): its size around its position; a painting's is the picture
    /// on its wall (PMMP `Painting::recalculateBoundingBox`).
    #[doc(alias = "Entity::getBoundingBox")]
    pub fn bounding_box(&self) -> Aabb {
        if let Kind::Painting(p) = &self.kind {
            return object::painting_box(
                BlockPos::containing(self.body.position),
                p.facing,
                p.motive,
            );
        }
        let (width, height) = self.size();
        let half = width / 2.0;
        let p = self.body.position;
        Aabb::new(
            Vec3::new(p.x - half, p.y, p.z - half),
            Vec3::new(p.x + half, p.y + height, p.z + half),
        )
    }

    /// Whether a hit of `amount` for `cause` would hurt it, and how (PMMP `Entity::attack` and
    /// `Living::attack`'s cooldown for NPCs, before the event): `None` when the kind ignores the
    /// cause, the amount is not within 0 through [`MAX_DAMAGE`], an NPC is dying, or a hit no
    /// stronger than the last comes within an NPC's cooldown.
    fn plan_hit(&self, cause: Cause, amount: f64) -> Option<Plan> {
        let dead_crystal = matches!(self.kind, Kind::EndCrystal(_)) && self.health <= 0.0;
        if !self.kind.takes(cause) || !(0.0..=MAX_DAMAGE).contains(&amount) || dead_crystal {
            return None;
        }
        let Kind::Npc(c) = &self.kind else {
            return Some(Plan {
                cold: false,
                previous: None,
            });
        };
        if c.dead_ticks.is_some() {
            return None;
        }
        let cold = c.attack_time == 0;
        if !cold && c.last_damage >= amount {
            return None;
        }
        Some(Plan {
            cold,
            previous: (!cold).then_some(c.last_damage),
        })
    }

    /// Takes a planned hit: `taken.damage` off its health; an NPC remembers the base damage and,
    /// on a cold hit, starts its cooldown.
    fn take_hit(&mut self, plan: Plan, taken: Taken) -> Hurt {
        let Kind::Npc(c) = &mut self.kind else {
            self.health -= taken.damage;
            return if self.health <= 0.0 {
                Hurt::Died
            } else {
                Hurt::Took { cold: false }
            };
        };
        c.last_damage = taken.base;
        if plan.cold {
            c.attack_time = taken.cooldown;
        }
        self.health = crate::math::max(self.health - taken.damage, 0.0);
        if self.health <= 0.0 {
            c.dead_ticks = Some(0);
            Hurt::Died
        } else {
            Hurt::Took { cold: plan.cold }
        }
    }

    /// What a hit that came to `result` does afterwards to the kinds that care (PMMP
    /// `EndCrystal::attack`): an end crystal hit by anything but the void is primed, and when dead
    /// it stays until its tick explodes it ([`explosion::crystal_died`]). True when a death removes
    /// the entity at once (every other kind that does not live).
    fn after_hit(&mut self, cause: Cause, result: Hurt) -> bool {
        match &mut self.kind {
            Kind::EndCrystal(c) => {
                if result != Hurt::Ignored && cause != Cause::Void {
                    c.primed = true;
                }
                false
            }
            _ => true,
        }
    }

    /// Whether placing a block in its box is refused (PMMP `canBeCollidedWith`: its kind's
    /// answer, and an end crystal only while its health is above 0).
    fn blocks_placing(&self) -> bool {
        self.kind.blocks_placing()
            && !(matches!(self.kind, Kind::EndCrystal(_)) && self.health <= 0.0)
    }

    /// Whether setting it on fire for `seconds` would change its burning (PMMP `setOnFire`).
    fn would_burn(&self, seconds: i16) -> bool {
        seconds.saturating_mul(20) > self.fire_ticks && !self.kind.is_fire_proof()
    }

    /// Sets it on fire for `seconds` unless it burns longer already (PMMP `setOnFire`).
    fn set_on_fire(&mut self, seconds: i16) {
        if self.would_burn(seconds) {
            self.fire_ticks = seconds.saturating_mul(20);
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
        self.fire_ticks % i16::from(FIRE_DAMAGE_TICKS) == 0
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
            name_tag,
            name_tag_visible,
            name_tag_always_visible,
            scale,
            invisible,
            gravity,
            saved,
            resend,
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
        } = body;
        for v in [*position, *motion] {
            for c in [v.x, v.y, v.z] {
                h.u64(c.to_bits());
            }
        }
        h.u64(u64::from(*on_ground));
        h.u64(fall_distance.to_bits());
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
            Kind::Painting(Painting {
                motive,
                facing,
                drops,
            }) => {
                h.u64(7);
                h.u64(*motive as u64);
                h.u64(*facing as u64);
                h.u64(u64::from(*drops));
            }
            Kind::EndCrystal(Crystal {
                show_base,
                beam_target,
                primed,
            }) => {
                h.u64(8);
                h.u64(u64::from(*show_base));
                match beam_target {
                    Some(p) => {
                        for c in [p.x, p.y, p.z] {
                            h.u64(u64::from(c.cast_unsigned()));
                        }
                    }
                    None => h.u64(u64::MAX),
                }
                h.u64(u64::from(*primed));
            }
            Kind::Firework(Firework {
                max_flight,
                explosions,
                owner,
                owner_runtime_id,
                flight: _,
            }) => {
                h.u64(9);
                h.u64(u64::from(*max_flight));
                h.usize(explosions.len());
                for c in explosions {
                    super::extra::hash_nbt(h, c);
                }
                h.u64(owner.map_or(0, |s| s.get().wrapping_add(1)));
                h.u64(*owner_runtime_id);
            }
        }
        h.bytes(name_tag.as_bytes());
        for b in [
            *name_tag_visible,
            *name_tag_always_visible,
            *invisible,
            *gravity,
            *saved,
            *resend,
        ] {
            h.u64(u64::from(b));
        }
        h.u64(scale.to_bits());
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
            health: sent_health,
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
        h.u64(sent_health.to_bits());
    }
}

super::checkpoint::fields!(Entity {
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
    name_tag,
    name_tag_visible,
    name_tag_always_visible,
    scale,
    invisible,
    gravity,
    saved,
    resend,
    flagged,
    still,
    viewers,
    sent,
});

/// A slot of the store.
#[derive(Debug, Default)]
struct Slot {
    /// The generation of the slot's current or next entity.
    generation: u32,
    entity: Option<Entity>,
}

super::checkpoint::fields!(Slot { generation, entity });

/// The entities of one chunk and the players that see it.
#[derive(Debug, Default)]
struct Column {
    ids: BTreeSet<Id>,
    /// The players that saw the chunk at the last tracking pass.
    viewers: BTreeSet<SessionId>,
    /// The entities whose viewers may differ from `viewers` (they arrived, or a bound held a
    /// packet of theirs back): the next pass compares only them, unless the chunk's players
    /// changed. An entity that left again is skipped there, so this holds at most the entities
    /// that came in since the last pass.
    arrived: BTreeSet<Id>,
}

super::checkpoint::fields!(Column {
    ids,
    viewers,
    arrived
});

/// An entity that left with its chunk, still to be removed from its viewers.
#[derive(Debug, Clone, PartialEq)]
struct Departed {
    runtime_id: u64,
    viewers: BTreeSet<SessionId>,
}

super::checkpoint::fields!(Departed {
    runtime_id,
    viewers
});

/// What other players were last sent of a player.
#[derive(Debug, Clone, PartialEq)]
struct Watched {
    runtime_id: u64,
    viewers: BTreeSet<SessionId>,
    /// The eye position sent.
    position: Vec3,
    /// The yaw and pitch sent.
    rotation: (f64, f64),
    held: Stack,
    held_slot: u8,
    off_hand: Stack,
    armour: [Stack; 4],
}

impl Watched {
    /// Whether the armour is what `before` holds, piece by piece ([`Stack::same`]).
    fn same_armour(&self, before: &Self) -> bool {
        self.armour
            .iter()
            .zip(&before.armour)
            .all(|(a, b)| a.same(b))
    }

    #[cfg(test)]
    fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            runtime_id,
            viewers,
            position,
            rotation: (yaw, pitch),
            held,
            held_slot,
            off_hand,
            armour,
        } = self;
        h.u64(*runtime_id);
        for v in viewers {
            h.u64(v.get());
        }
        for c in [position.x, position.y, position.z, *yaw, *pitch] {
            h.u64(c.to_bits());
        }
        for stack in std::iter::once(held).chain([off_hand]).chain(armour) {
            super::inventory::hash_stack(h, stack);
        }
        h.u64(u64::from(*held_slot));
    }
}

super::checkpoint::fields!(Watched {
    runtime_id,
    viewers,
    position,
    rotation,
    held,
    held_slot,
    off_hand,
    armour,
});

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
    /// (viewer, player) pairs where the viewer is not shown the player (PMMP `hidePlayer`): at
    /// most [`MAX_HIDDEN_PER_PLAYER`] per viewer, dropped when either leaves.
    hidden: BTreeSet<(SessionId, SessionId)>,
    /// Flagged entities waiting for their removal (at most every entity).
    doomed: BTreeSet<Id>,
    /// The slot where the next movement pass starts, so every entity is sent in turn.
    cursor: u32,
    /// Entities near an explosion of this tick, or changed by a listener while the steps apply,
    /// that move again from their new state when their turn comes even if their body is the
    /// same (at most every entity; empty between ticks).
    restep: BTreeSet<Id>,
    /// The steps are being applied: an entity a listener changes through [`Mut`] moves again
    /// from its new state ([`Self::restep`]) instead of the step computed before.
    applying: bool,
    /// Entities created or loaded since the last announcement, while someone listens to
    /// [`EntitySpawn`] or [`ItemSpawn`]; the entity tick announces them (at most every entity).
    spawned: BTreeSet<Id>,
    /// Announced entities that left with their chunk, while someone listens to
    /// [`EntityDespawn`]; the next entity tick announces them (at most every entity that was in
    /// the store at that tick, since only the entity tick announces).
    left: Vec<Id>,
    /// Someone listens to [`EntityDespawn`] (set by each entity tick: [`Self::unload`] has no
    /// listeners at hand).
    despawn_heard: bool,
    /// Lightning bolts shown and waiting for their removal, oldest first (at most
    /// [`lightning::MAX_BOLTS`]).
    bolts: VecDeque<lightning::Bolt>,
    /// The custom entity types, fixed at start.
    custom: Customs,
    /// Whether experience exists (`experience`, fixed at start): without it a load drops saved
    /// orbs.
    experience: bool,
}

impl Entities {
    /// The live entity `id` names, if it still exists.
    fn get(&self, id: Id) -> Option<&Entity> {
        live(&self.slots, id)
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
    pub(super) fn count(&self, world: WorldIndex) -> usize {
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
        column.arrived.insert(id);
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
        self.spawned.remove(&id);
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
        column.arrived.insert(id);
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
    /// order (PMMP `getNearbyEntities`: the chunks within 2 blocks of the box). A small area looks
    /// up each of its chunks; a large one walks the index's columns in its x range instead, so
    /// the cost is bounded by the entities' chunks whatever the area (API callers pass any box).
    fn nearby(&self, world: WorldIndex, area: Aabb) -> Vec<Id> {
        let mut out = Vec::new();
        self.each_nearby(world, area, |id| out.push(id));
        out
    }

    /// Calls `f` with each entity [`Self::nearby`] returns, in the same order, for callers that
    /// only read the store meanwhile.
    fn each_nearby(&self, world: WorldIndex, area: Aabb, mut f: impl FnMut(Id)) {
        let chunk = |v: f64| physics_block(v) >> 4;
        let (x0, x1) = (chunk(area.min.x - 2.0), chunk(area.max.x + 2.0));
        let (z0, z1) = (chunk(area.min.z - 2.0), chunk(area.max.z + 2.0));
        if x0 > x1 || z0 > z1 {
            return;
        }
        let mut visit = |column: &Column| {
            for &id in &column.ids {
                if let Some(e) = self.get(id)
                    && !e.flagged
                    && e.bounding_box().overlaps(area, EPSILON)
                {
                    f(id);
                }
            }
        };
        let span = |a: i32, b: i32| u64::from(b.abs_diff(a)).saturating_add(1);
        if span(x0, x1).saturating_mul(span(z0, z1)) <= MAX_CHUNK_LOOKUPS {
            for x in x0..=x1 {
                for z in z0..=z1 {
                    if let Some(column) = self.columns.get(&(world, ChunkPos::new(x, z))) {
                        visit(column);
                    }
                }
            }
        } else {
            let range = (world, ChunkPos::new(x0, i32::MIN))..=(world, ChunkPos::new(x1, i32::MAX));
            for ((_, pos), column) in self.columns.range(range) {
                if (z0..=z1).contains(&pos.z) {
                    visit(column);
                }
            }
        }
    }

    /// Takes whether orbs exist ([`super::Settings::has_orbs`]; at start, before any chunk
    /// loads).
    pub(super) fn set_experience(&mut self, on: bool) {
        self.experience = on;
    }

    /// A chunk's saved list arrived ([`mod@super::rules`]), `saved` read from `list` off the
    /// tick ([`read`]): its entities enter the store, waiting for runtime IDs; compounds that are
    /// not entities here, and entities over the cap, are kept as saved. Without experience, saved
    /// orbs are dropped and the chunk is saved without them.
    pub(super) fn load(&mut self, key: Key, saved: Vec<Saved>, list: &[Compound]) {
        let mut kept = Vec::new();
        let mut over = 0usize;
        let mut dropped = false;
        for (Saved(read), c) in saved.into_iter().zip(list) {
            let l = match read {
                Read::Entity(l) => *l,
                Read::Kept(c) => {
                    kept.push(c);
                    continue;
                }
            };
            if !self.experience && matches!(l.kind, Kind::ExperienceOrb(_)) {
                dropped = true;
                continue;
            }
            if self.count(key.0) >= MAX_ENTITIES_PER_WORLD {
                over += 1;
                #[cfg(test)]
                super::rules::reads::add(|r| r.clones += 1);
                kept.push(c.clone());
                continue;
            }
            let mut e = Entity::new(key.0, l.body, l.yaw, l.pitch, l.kind);
            e.health = l.health;
            e.fire_ticks = l.fire_ticks;
            let saved::Looks {
                name_tag,
                name_tag_visible,
                name_tag_always_visible,
                scale,
                invisible,
                gravity,
            } = l.looks;
            e.name_tag = name_tag;
            e.name_tag_visible = name_tag_visible;
            e.name_tag_always_visible =
                name_tag_always_visible.unwrap_or(e.name_tag_always_visible);
            e.scale = scale;
            e.invisible = invisible;
            e.gravity = gravity;
            e.sent.flags = track::flags(&e, e.still);
            let id = self.insert(e);
            self.unassigned.push(id);
        }
        // Loading changes nothing to save, but a dropped orb.
        if dropped {
            self.changed.insert(key);
        } else {
            self.changed.remove(&key);
        }
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

    /// [`read`] with the store's custom entity types and [`Self::load`] in one.
    #[cfg(test)]
    pub(super) fn load_list(
        &mut self,
        key: Key,
        list: &[Compound],
        content: &crate::custom::Registry,
    ) {
        let saved = read(key.1, list, self.types(), content);
        self.load(key, saved, list);
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
        // Only a chunk that changed hands its list back, so an unchanged one saves no entity.
        let mut list = if changed {
            Vec::with_capacity(ids.len())
        } else {
            Vec::new()
        };
        // Only an entity waiting for its runtime ID is in `unassigned`.
        let mut took_unassigned = false;
        for id in ids {
            // An entity whose spawn was not announced yet leaves unannounced.
            let announced = !self.spawned.contains(&id);
            let Some(e) = self.take(id) else {
                continue;
            };
            took_unassigned |= e.runtime_id == 0;
            if announced && self.despawn_heard && e.runtime_id != 0 {
                self.left.push(id);
            }
            if changed && !e.flagged && e.saved {
                list.push(saved::save(&e));
            }
            if e.runtime_id != 0 && !e.viewers.is_empty() {
                self.departed.push(Departed {
                    runtime_id: e.runtime_id,
                    viewers: e.viewers,
                });
            }
        }
        if took_unassigned {
            let slots = &self.slots;
            self.unassigned.retain(|&id| {
                usize::try_from(id.index())
                    .ok()
                    .and_then(|i| slots.get(i))
                    .is_some_and(|s| s.generation == id.generation() && s.entity.is_some())
            });
        }
        let kept = self.kept.remove(&key).unwrap_or_default();
        // Taking the entities out marked the chunk; what counts is whether it changed before.
        self.changed.remove(&key);
        changed.then(|| {
            list.extend(kept);
            list
        })
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
                    .filter(|e| !e.flagged && e.saved)
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
    #[expect(
        clippy::too_many_lines,
        reason = "consistency: every field of the store is hashed in one place, destructured"
    )]
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
            hidden,
            doomed,
            cursor,
            restep,
            applying,
            spawned,
            left,
            bolts,
            // A function of the listeners, which are code fixed at build.
            despawn_heard: _,
            // Fixed at start (definitions and code); entities hash their type index.
            custom: _,
            // A setting, fixed at start.
            experience: _,
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
            h.usize(c.arrived.len());
            for &i in &c.arrived {
                hash_id(h, i);
            }
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
        h.usize(hidden.len());
        for (v, t) in hidden {
            h.u64(v.get());
            h.u64(t.get());
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
        h.u64(u64::from(*applying));
        h.usize(spawned.len());
        for &i in spawned {
            hash_id(h, i);
        }
        h.usize(left.len());
        for &i in left {
            hash_id(h, i);
        }
        h.usize(bolts.len());
        for b in bolts {
            b.hash_into(h);
        }
    }

    /// Writes every field but the custom types and the experience setting, which are fixed at
    /// start.
    pub(super) fn checkpoint(&self, w: &mut super::checkpoint::Writer) {
        use super::checkpoint::Codec;
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
            hidden,
            doomed,
            cursor,
            restep,
            applying,
            spawned,
            left,
            despawn_heard,
            bolts,
            custom: _,
            experience: _,
        } = self;
        slots.encode(w);
        free.encode(w);
        retired.encode(w);
        counts.encode(w);
        columns.encode(w);
        runtime_ids.encode(w);
        unassigned.encode(w);
        kept.encode(w);
        changed.encode(w);
        departed.encode(w);
        watched.encode(w);
        hidden.encode(w);
        doomed.encode(w);
        cursor.encode(w);
        restep.encode(w);
        applying.encode(w);
        spawned.encode(w);
        left.encode(w);
        despawn_heard.encode(w);
        bolts.encode(w);
    }

    /// Reads what [`Self::checkpoint`] wrote; the custom types and the experience setting stay.
    pub(super) fn restore(
        &mut self,
        r: &mut super::checkpoint::Reader<'_>,
    ) -> super::checkpoint::Result<()> {
        use super::checkpoint::Codec;
        *self = Self {
            slots: Codec::decode(r)?,
            free: Codec::decode(r)?,
            retired: Codec::decode(r)?,
            counts: Codec::decode(r)?,
            columns: Codec::decode(r)?,
            runtime_ids: Codec::decode(r)?,
            unassigned: Codec::decode(r)?,
            kept: Codec::decode(r)?,
            changed: Codec::decode(r)?,
            departed: Codec::decode(r)?,
            watched: Codec::decode(r)?,
            hidden: Codec::decode(r)?,
            doomed: Codec::decode(r)?,
            cursor: Codec::decode(r)?,
            restep: Codec::decode(r)?,
            applying: Codec::decode(r)?,
            spawned: Codec::decode(r)?,
            left: Codec::decode(r)?,
            despawn_heard: Codec::decode(r)?,
            bolts: Codec::decode(r)?,
            custom: std::mem::take(&mut self.custom),
            experience: self.experience,
        };
        Ok(())
    }
}

/// One compound of a chunk's saved entity list, read off the tick ([`read`]).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Saved(Read);

/// What a saved compound is to the store.
#[derive(Debug, Clone, PartialEq)]
enum Read {
    /// An entity, waiting for the store to give it a slot.
    Entity(Box<saved::Loaded>),
    /// Not an entity here: the store keeps the compound as saved.
    Kept(Compound),
}

/// The functions of the player modules (inventory, survival) the entity rules call on players, so
/// tests can stand in for them and record what the rules asked.
#[derive(Debug, Clone, Copy)]
struct Hooks {
    /// Puts a stack into a player's inventory; returns what did not fit.
    give: fn(&mut Player, Stack) -> Stack,
    /// How much of a stack a player's inventory takes now ([`room`]).
    room: fn(&Player, &Stack) -> u8,
    /// A player picks up an orb's experience points (mending first); false, and the orb stays,
    /// while its pickup cooldown runs.
    pickup_experience: fn(&mut State, SessionId, u32) -> bool,
    /// Hurts a player, from a source the event names; false if no damage was taken.
    damage: fn(&mut State, SessionId, f64, survival::Damage, DamageSource) -> bool,
    /// Whether a player's hit cooldown runs (a hit then does not knock it back).
    has_attack_cooldown: fn(&Player) -> bool,
    /// Gives a player an effect.
    add_effect: fn(&mut State, SessionId, crate::effect::Instance) -> bool,
    /// Heals a player by an amount (instant health of a potion, with its potency).
    heal: fn(&mut State, SessionId, f64),
    /// Sets a player on fire for a number of seconds, by a source the event names.
    set_on_fire: fn(&mut State, SessionId, u32, CombustSource),
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

/// A non-player character to spawn ([`spawn_npc`]).
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Npc {
    /// A villager-type NPC (network actor `minecraft:villager_v2`): no AI.
    Villager {
        /// The profession shown (`EntityVariant`).
        profession: u8,
        /// Its trades.
        offers: super::trade::Offers,
    },
    /// A human NPC (PMMP `Human`): shown as a player with `name` and `skin` (`None`: the default
    /// skin).
    Human {
        /// The name tag.
        name: String,
        /// The skin.
        skin: Option<crate::entity::Skin>,
    },
    /// A zombie (PMMP `Zombie`): no AI, and unlike the other NPCs it takes damage.
    Zombie,
}

/// What the apply phase of one tick may still do: merges, and explosions with the blocks they
/// affect, bounded so that a chain of explosions spreads over ticks instead of stalling one.
#[derive(Debug, Default)]
struct Budget {
    /// Item merges so far ([`MAX_MERGES_PER_TICK`]).
    merges: usize,
    /// Explosions of primed TNT and end crystals so far
    /// ([`explosion::MAX_EXPLOSIONS_PER_TICK`]).
    explosions: usize,
    /// Blocks those explosions affected so far ([`explosion::MAX_BLOCKS_PER_TICK`]).
    blocks: usize,
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
    /// [`npc::tick`], then [`custom::tick`].
    Custom,
    /// [`object::firework_tick`].
    Firework,
    /// Nothing of its own (paintings and end crystals: PMMP's `entityBaseTick` only).
    Inert,
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
    /// The speed changed enough to record as the last seen one, which tells when the entity
    /// comes to rest.
    sped: bool,
    /// A cloud's radius to send (it changed by [`RADIUS_STEP`] or more).
    radius: Option<f64>,
    /// A custom entity's health is to be sent.
    health: bool,
}

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
            flags: track::flags(e, now_still),
            moved: track::has_moved(e) || turned || (!was_still && still),
            sped: (e.body.motion - e.sent.motion).length_squared() > 0.0025 || was_still != still,
            radius,
            health: e.kind.custom().is_some() && e.health != e.sent.health,
        }
    }

    /// Whether anything is to be sent.
    fn is_due(self, e: &Entity) -> bool {
        self.moved
            || self.sped
            || self.flags != e.sent.flags
            || self.radius.is_some()
            || self.health
            || e.resend
    }
}

/// Reads the saved entity list of chunk `pos` for [`Entities::load`], in list order: off the
/// tick, as it needs nothing of the tick's state. `types` are the server's custom entity types
/// and `content` its custom content, which saved items are read through.
pub(super) fn read(
    pos: ChunkPos,
    list: &[Compound],
    types: &EntityTypes,
    content: &crate::custom::Registry,
) -> Vec<Saved> {
    list.iter()
        .map(|c| {
            Saved(if let Some(l) = saved::load(c, pos, types, content) {
                Read::Entity(Box::new(l))
            } else {
                #[cfg(test)]
                super::rules::reads::add(|r| r.clones += 1);
                Read::Kept(c.clone())
            })
        })
        .collect()
}

/// The unique ID of a player or an entity: its runtime ID. Runtime IDs count up by one from 1
/// per player and entity, so they stay far below 2^63 and keep their value as `i64`.
pub(super) fn unique_id(runtime_id: u64) -> i64 {
    runtime_id.cast_signed()
}

/// The block coordinate of a world coordinate (entity positions, query areas, a damage value),
/// saturating at the `i32` range: Rust's `as` saturates and turns NaN into 0.
#[expect(
    clippy::cast_possible_truncation,
    reason = "consistency: a value past the i32 range saturates, which every caller accepts"
)]
fn physics_block(v: f64) -> i32 {
    v.floor() as i32
}

/// Whether a point may hold an entity: finite and inside the world border on every axis, as a
/// player's position (so its `f32` in packets is finite too).
pub(super) fn is_valid_position(v: Vec3) -> bool {
    super::inside_border(v)
}

/// Whether an explosion of `size` at `center` happens: a valid position and a finite size above
/// 0.
pub(super) fn is_valid_explosion(center: Vec3, size: f64) -> bool {
    is_valid_position(center) && size.is_finite() && size > 0.0
}

/// Whether `scale` may be the scale of an entity of `kind`: above 0 (PMMP), and its scaled box
/// at most [`custom::MAX_SIZE`] blocks on a side (physics walks every block the box covers), so
/// finite.
fn is_valid_scale(scale: f64, kind: &Kind) -> bool {
    let physics = kind.physics();
    scale > 0.0
        && physics.width * scale <= custom::MAX_SIZE
        && physics.height * scale <= custom::MAX_SIZE
}

/// Whether [`create`] makes an entity at `position` in `world`: the world exists, the position
/// is valid and loaded, the world is below its cap and runtime IDs remain. A caller that changes
/// the world for the entity asks first, so a refusal leaves the world as it was.
pub(super) fn can_create(state: &State, world: WorldIndex, position: Vec3) -> bool {
    state
        .worlds
        .get(world)
        .is_some_and(|w| is_valid_position(position) && w.is_chunk_loaded(chunk_of(position)))
        && state.rules.entities.count(world) < MAX_ENTITIES_PER_WORLD
        && state.next_runtime_id.checked_add(1).is_some()
}

/// Creates an entity of `kind` at `position` with the motion and rotation (yaw, pitch) `motion`
/// gives once it has its runtime ID; `None` when [`can_create`] refuses.
fn create(
    state: &mut State,
    world: WorldIndex,
    position: Vec3,
    kind: Kind,
    motion: impl FnOnce(&mut Random) -> (Vec3, f64, f64),
) -> Option<Id> {
    if !can_create(state, world, position) {
        return None;
    }
    let w = state.worlds.get(world)?;
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
    let heard = spawn_heard(state);
    let entities = &mut state.rules.entities;
    let id = entities.insert(entity);
    entities.runtime_ids.insert(runtime_id, id);
    if heard {
        entities.spawned.insert(id);
    }
    Some(id)
}

/// Whether someone listens to the spawn of entities ([`EntitySpawn`], [`ItemSpawn`]).
fn spawn_heard(state: &State) -> bool {
    dispatch::heard::<EntitySpawn>(state) || dispatch::heard::<ItemSpawn>(state)
}

/// Takes an entity that was never shown out of the store at once: a projectile whose launch a
/// listener cancelled. Its spawn is not announced and no despawn follows.
fn discard(state: &mut State, id: Id) {
    let _gone = state.rules.entities.take(id);
}

/// Announces the entities that left with their chunk ([`EntityDespawn`]) and those that
/// appeared since the last announcement ([`ItemSpawn`] for dropped items, then [`EntitySpawn`],
/// PMMP `onFirstUpdate`), in `Id` order. The entity tick calls it before the physics and again
/// before the removals, so every entity is announced before it goes. Entities a listener of
/// another event makes wait for the next announcement; those a spawn listener makes are never
/// announced, as [`spawn_heard`] finds no spawn listener while their list is out of the state
/// ([`dispatch::call`]).
fn announce(state: &mut State) {
    let left = std::mem::take(&mut state.rules.entities.left);
    if dispatch::heard::<EntityDespawn>(state) {
        for entity in left {
            let mut e = EntityDespawn { entity };
            dispatch::call(state, &mut e);
            dispatch::watch(state, &e);
        }
    }
    let spawned = std::mem::take(&mut state.rules.entities.spawned);
    for entity in spawned {
        let Some(e) = state.rules.entities.get(entity) else {
            continue;
        };
        if matches!(e.kind, Kind::Item(_)) && dispatch::heard::<ItemSpawn>(state) {
            let mut e = ItemSpawn { entity };
            dispatch::call(state, &mut e);
            dispatch::watch(state, &e);
        }
        if dispatch::heard::<EntitySpawn>(state) {
            let mut e = EntitySpawn { entity };
            dispatch::call(state, &mut e);
            dispatch::watch(state, &e);
        }
    }
}

/// A random yaw as PMMP picks one (`getRandomFloat() * 360`), kept to float precision as saved.
fn random_yaw(random: &mut Random) -> f64 {
    f64::from(to_f32(random.unit() * 360.0))
}

/// Drops `stack` as an item entity at `pos` with PMMP `World::dropItem`'s random motion and
/// pickup delay. False (nothing created, the caller keeps the stack) when the world's entity
/// limit is reached or the position is not loaded; true for an empty stack, which drops nothing.
pub(super) fn drop_item(state: &mut State, world: WorldIndex, pos: Vec3, stack: Stack) -> bool {
    drop_item_with(
        state,
        world,
        pos,
        stack,
        None,
        DROP_PICKUP_DELAY,
        String::new(),
    )
}

/// [`drop_item`] with a given motion (`None`: PMMP's random one), pickup delay in ticks and
/// thrower (the name of the player who throws it, saved with it; empty for none), for items a
/// player throws (PMMP `Player::dropItem`: its look direction × 0.4, delay 40).
pub(super) fn drop_item_with(
    state: &mut State,
    world: WorldIndex,
    pos: Vec3,
    stack: Stack,
    motion: Option<Vec3>,
    pickup_delay: i16,
    thrower: String,
) -> bool {
    stack.is_empty()
        || drop_stack(state, world, pos, stack, motion, pickup_delay, thrower).is_some()
}

/// [`drop_item_with`] for a stack that is not empty, giving the new entity.
fn drop_stack(
    state: &mut State,
    world: WorldIndex,
    pos: Vec3,
    stack: Stack,
    motion: Option<Vec3>,
    pickup_delay: i16,
    thrower: String,
) -> Option<Id> {
    let item = Item {
        stack,
        pickup_delay: max_delay(pickup_delay),
        despawn_delay: item_despawn_delay(state),
        owner: String::new(),
        thrower,
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
}

/// The ticks a dropped item lives (`item-despawn-ticks`, at most 72,000).
fn item_despawn_delay(state: &State) -> i32 {
    i32::try_from(state.settings.item_despawn_ticks).unwrap_or(i32::MAX)
}

/// A pickup delay of at least [`NEVER`].
fn max_delay(d: i16) -> i16 {
    d.max(NEVER)
}

/// Drops `amount` experience as orbs at `pos` (PMMP `World::dropExperience`: split into the orb
/// sizes, each with a random motion). At the world's entity limit the rest is not dropped:
/// experience is not an item a caller can keep. Without orbs ([`Settings::has_orbs`]) nothing
/// drops: every rule that gives experience (blocks, mobs, furnaces, trades, bottles, deaths)
/// comes here.
///
/// [`Settings::has_orbs`]: super::Settings::has_orbs
pub(super) fn drop_experience(state: &mut State, world: WorldIndex, pos: Vec3, amount: u32) {
    drop_orbs(state, world, pos, amount);
}

/// [`drop_experience`], returning the points dropped.
fn drop_orbs(state: &mut State, world: WorldIndex, pos: Vec3, amount: u32) -> u32 {
    if !state.settings.has_orbs() {
        return 0;
    }
    let mut left = amount;
    while left > 0 {
        let size = ORB_SPLIT_SIZES
            .into_iter()
            .find(|&s| u32::from(s) <= left)
            .unwrap_or(1);
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
            break;
        }
        left = left.saturating_sub(u32::from(size));
    }
    amount - left
}

/// Turns the block at `pos` into a falling block entity (sand, gravel, anvils…): the block
/// becomes air and the entity starts at the bottom centre of its cell (PMMP `FallableTrait`).
/// False, with the block left as it is, when [`can_create`] refuses or `pos` is outside the
/// world height.
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
    if !can_create(state, world, at) {
        return false;
    }
    let Some(w) = state.worlds.get_mut(world) else {
        return false;
    };
    if w.set_block(pos, block::AIR).is_err() {
        return false;
    }
    let kind = Kind::FallingBlock(Falling { block });
    #[expect(
        clippy::expect_used,
        reason = "invariant: `can_create` allowed it and setting a block changes neither the \
                  entities nor the runtime IDs"
    )]
    create(state, world, at, kind, |_| (Vec3::ZERO, 0.0, 0.0)).expect("the entity is made");
    true
}

/// The entity with runtime ID `runtime_id`, for packets that name one (attacks and interactions).
pub(super) fn find(state: &State, runtime_id: u64) -> Option<Id> {
    state.rules.entities.runtime_ids.get(&runtime_id).copied()
}

/// Removes an entity at the end of this tick (killed, used up), with its removal sent to its
/// viewers. False if it is gone already.
#[cfg(test)]
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
    let mut paintings = Vec::new();
    for id in entities.nearby(world, cell) {
        if let Some(e) = entities.get_mut(id) {
            e.forced = true;
            if matches!(e.kind, Kind::Painting(_)) {
                paintings.push(id);
            }
        }
    }
    // PMMP `Painting::onNearbyBlockChange`: a painting whose wall or room went falls.
    for id in paintings {
        object::check_painting(state, id);
    }
}

/// One tick of every entity: physics on the CPU pool (read-only), then the results applied in
/// entity order, then pickups, removals and tracking.
pub(super) fn tick(state: &mut State, pool: &Pool) {
    tick_with(state, pool, HOOKS);
}

/// [`tick`] with the given hooks.
fn tick_with(state: &mut State, pool: &Pool, hooks: Hooks) {
    state.rules.entities.despawn_heard = dispatch::heard::<EntityDespawn>(state);
    assign_runtime_ids(state);
    announce(state);
    let steps = compute(state, pool);
    apply_steps(state, steps, hooks);
    pick_up(state, hooks);
    announce(state);
    remove_flagged(state);
    lightning::remove_due(state);
    let mut budget = spawn_budget(state);
    track_entities(state, &mut budget);
    track_players(state, &mut budget);
}

/// The movement of `e` this tick, from its state now; `None` if its world is gone.
fn step_of(worlds: &[World], tick: u64, e: &Entity) -> Option<Step> {
    let world = worlds.get(e.world)?;
    let key = random::key(UNSTICK_RANDOM, &[e.runtime_id]);
    let stuck = match &e.kind {
        Kind::Projectile(p) => p.stuck,
        _ => None,
    };
    let input = physics::Input {
        body: e.body,
        physics: e.physics(),
        forced: e.forced,
        random: (world.seed(), tick, key),
        stuck,
    };
    Some(physics::step(world, &input))
}

/// Phase 2: the movement of every ticking entity, on the CPU pool, in `Id` order, with the body
/// it was computed from. Each pool task reads its entity and builds its input itself, so the
/// tick thread only lists the entities.
fn compute(state: &State, pool: &Pool) -> Vec<(Id, Option<(Body, Step)>)> {
    let (slots, worlds, tick) = (&state.rules.entities.slots, &state.worlds, state.tick);
    pool.map(&ticking(state), |&id| {
        let computed =
            live(slots, id).and_then(|e| step_of(worlds, tick, e).map(|step| (e.body, step)));
        (id, computed)
    })
}

/// The live entity `id` names among `slots` ([`Entities::get`]).
fn live(slots: &[Slot], id: Id) -> Option<&Entity> {
    let slot = slots.get(usize::try_from(id.index()).ok()?)?;
    (slot.generation == id.generation())
        .then_some(slot.entity.as_ref())
        .flatten()
}

/// Phase 3: the steps applied and the kind rules run, in `Id` order; an entity whose body
/// changed since its step was computed (a rule's knockback or push), or that is in
/// [`Entities::restep`], moves again from its new state; then the damage falling anvils dealt.
fn apply_steps(state: &mut State, steps: Vec<(Id, Option<(Body, Step)>)>, hooks: Hooks) {
    let mut budget = Budget::default();
    let mut hits = Vec::new();
    state.rules.entities.applying = true;
    for (id, computed) in steps {
        let marked = state.rules.entities.restep.remove(&id);
        let step = match computed {
            Some((from, step))
                if !marked && state.rules.entities.get(id).is_some_and(|e| e.body == from) =>
            {
                Some(step)
            }
            Some(_) => restep(state, id),
            None => None,
        };
        if let Some(step) = step {
            apply(state, id, step, hooks, &mut budget, &mut hits);
        }
    }
    state.rules.entities.applying = false;
    state.rules.entities.restep.clear();
    for (session, damage, anvil) in hits {
        (hooks.damage)(
            state,
            session,
            damage,
            survival::Damage::FallingBlock,
            DamageSource::Entity(Actor::Entity(anvil)),
        );
    }
}

/// An entity's movement computed again, serially, from its state now.
fn restep(state: &State, id: Id) -> Option<Step> {
    step_of(&state.worlds, state.tick, state.rules.entities.get(id)?)
}

/// Gives the loaded entities their runtime IDs, in load order.
fn assign_runtime_ids(state: &mut State) {
    let heard = spawn_heard(state);
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
        if heard {
            entities.spawned.insert(id);
        }
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

/// The entities that tick: in chunks within their world's `tick-radius` of a player of their
/// world whose view is open (as blocks, [`super::view::tick_centres`]), with a runtime ID and
/// not flagged, in `Id` order.
fn ticking(state: &State) -> Vec<Id> {
    let entities = &state.rules.entities;
    let mut ids = Vec::new();
    // By world, so only the columns of worlds with players are walked, each against its own.
    for (world, centres) in super::view::tick_centres(state) {
        let radius = state.settings.view(world).tick_radius;
        let all =
            (world, ChunkPos::new(i32::MIN, i32::MIN))..=(world, ChunkPos::new(i32::MAX, i32::MAX));
        for (&(_, pos), column) in entities.columns.range(all) {
            let near = centres
                .iter()
                .any(|&c| super::view::in_disc(c, pos, radius));
            if near {
                ids.extend(column.ids.iter().copied().filter(|&id| {
                    entities
                        .get(id)
                        .is_some_and(|e| !e.flagged && e.runtime_id != 0)
                }));
            }
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
    hits: &mut Vec<(SessionId, f64, Id)>,
) {
    let Some(e) = state.rules.entities.get(id) else {
        return;
    };
    if e.flagged {
        return;
    }
    if matches!(e.kind, Kind::EndCrystal(_)) && e.health <= 0.0 {
        // PMMP `EndCrystal::onDeathUpdate`.
        explosion::crystal_died(state, id, budget, hooks);
        return;
    }
    let entities = &mut state.rules.entities;
    let Some(e) = entities.get_mut(id) else {
        return;
    };
    if let Kind::Npc(c) = &mut e.kind
        && let Some(t) = c.dead_ticks
    {
        // PMMP `Living::onDeathUpdate`: the death shows, then the NPC goes.
        let t = t.saturating_add(1);
        c.dead_ticks = Some(t);
        let (world, at) = (e.world, e.body.position);
        if t >= kind::MAX_DEAD_TICKS {
            entities.doom(id);
        }
        if t == 1 {
            died(state, id, world, at);
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
        landed(state, id, fall, hooks, hits);
    }
    if !is_live(state, id) {
        return;
    }
    base_tick(state, id, step.inside);
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
        Rule::Custom => {
            npc::tick(state, id, hooks);
            custom::tick(state, id);
        }
        Rule::Firework => object::firework_tick(state, id, hooks),
        Rule::Inert => {}
    }
    // The kind rules flag what they use up (despawned, merged, picked up, landed).
    let entities = &mut state.rules.entities;
    if entities.get(id).is_some_and(|e| e.flagged) {
        entities.doomed.insert(id);
    }
}

/// The blocks an entity is inside, the void and burning (PMMP `entityBaseTick`, `onEntityInside`,
/// `doOnFireTick`). Entities never sneak nor wear boots: magma spares none that is living.
fn base_tick(state: &mut State, id: Id, inside: Vec<Inside>) {
    for inside in inside {
        match inside {
            Inside::Harm(harm, pos) => {
                let living = state
                    .rules
                    .entities
                    .get(id)
                    .is_some_and(|e| e.kind.is_living());
                if harm.living_only() && !living {
                    continue;
                }
                if harm.ends_fall() && harm.living_only() {
                    with(state, id, |e| e.body.fall_distance = 0.0);
                }
                let cause = match harm.cause() {
                    DamageCause::Lava => Cause::Lava,
                    DamageCause::Contact => Cause::Contact,
                    _ => Cause::Fire,
                };
                let blow = Blow::of(cause, harm.damage()).by(DamageSource::Block(pos));
                hurt(state, id, blow, None);
                if let Some(seconds) = harm.burn_secs() {
                    combust(state, id, i16::from(seconds), CombustSource::Block(pos));
                }
            }
            Inside::Water => {
                put_out(state, id, ExtinguishCause::Water);
            }
            // The block as it is now: an entity before this one may have changed it.
            Inside::WaterCauldron(pos) => {
                if let Some((world, block)) = block_at(state, id, pos)
                    && block.is(Class::WaterCauldron)
                    && put_out(state, id, ExtinguishCause::WaterCauldron)
                {
                    super::interact::lower_water_cauldron(state, world, pos, block);
                }
            }
            Inside::Campfire(pos) => {
                if with(state, id, |e| e.fire_ticks > 0) == Some(true)
                    && let Some((world, block)) = block_at(state, id, pos)
                    && block.is(Class::Campfire)
                    && Harm::of(block).is_none()
                {
                    super::interact::light_campfire(state, world, pos, block);
                }
            }
        }
    }
    let Some(e) = state.rules.entities.get_mut(id) else {
        return;
    };
    let void = e.body.position.y <= VOID_Y;
    if void {
        hurt(state, id, Blow::of(Cause::Void, VOID_DAMAGE), None);
    }
    let burnt = with(state, id, |e| {
        let (before, fire_proof) = (e.fire_ticks, e.kind.is_fire_proof());
        let due = e.burn();
        let out = (before > 0 && e.fire_ticks <= 0).then_some(if fire_proof {
            ExtinguishCause::FireProof
        } else {
            ExtinguishCause::Ticking
        });
        (due, out)
    });
    if let Some((due, out)) = burnt {
        if due {
            hurt(state, id, Blow::of(Cause::FireTick, 1.0), None);
        }
        if let Some(cause) = out {
            extinguished(state, id, cause);
        }
    }
}

/// Whether projectiles hit it (PMMP `Projectile::canCollideWith`: living things, and end
/// crystals while they live).
fn is_projectile_target(e: &Entity) -> bool {
    e.kind.is_living() || (matches!(e.kind, Kind::EndCrystal(_)) && e.health > 0.0)
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

/// Hurts the entity `id` as the game does, with the events (PMMP `Entity::attack`,
/// `Living::attack`): [`EntityDamage`] before the health changes (cancelling keeps it, and no
/// knockback follows) and [`EntityMotion`] for the knockback. An NPC hit after its cooldown is
/// knocked back by `knock` and shows the hit, an NPC that dies shows its death (its drops follow
/// at its next tick, [`died`]), anything else that dies is removed; an invulnerable NPC's hit by
/// an entity raises the event cancelled ([`shielded`]). Without listeners it is [`hurt_in`].
fn hurt(state: &mut State, id: Id, blow: Blow, knock: Option<Knock>) -> Hurt {
    if !(dispatch::heard::<EntityDamage>(state) || dispatch::heard::<EntityMotion>(state)) {
        let result = hurt_in(
            &mut state.rules.entities,
            &mut state.players,
            id,
            blow.cause,
            blow.amount,
            knock,
        );
        if result == Hurt::Died {
            broken_by(state, id, blow.source);
        }
        return result;
    }
    let Some(plan) = plan_of(state, id, blow) else {
        shielded(state, id, blow, knock);
        return Hurt::Ignored;
    };
    let mut taken = Taken::of(plan, blow.amount);
    let mut knock = knock;
    let mut event = None;
    if dispatch::heard::<EntityDamage>(state) {
        let Some((t, k, e)) = damage_event(state, id, blow, plan, knock) else {
            return Hurt::Ignored;
        };
        (taken, knock, event) = (t, k, Some(e));
    }
    let entities = &mut state.rules.entities;
    let Some(e) = entities.get_mut(id) else {
        return Hurt::Ignored;
    };
    let result = e.take_hit(plan, taken);
    let removed = e.after_hit(blow.cause, result);
    let living = matches!(e.kind, Kind::Npc(_));
    let key = (e.world, e.chunk);
    entities.changed.insert(key);
    if result == Hurt::Died && !living && removed {
        state.rules.entities.doom(id);
        broken_by(state, id, blow.source);
    }
    let shown = living && matches!(result, Hurt::Took { cold: true } | Hurt::Died);
    if shown {
        let motion = knock
            .zip(state.rules.entities.get(id))
            .and_then(|(k, e)| k.applied_to(e.body.motion));
        if let Some(m) = motion {
            push_entity(state, id, m);
        }
        let (shown_event, data) = if result == Hurt::Died {
            (ActorEvent::Death, 0)
        } else {
            (ActorEvent::Hurt, blow.cause.bds_cause())
        };
        if let Some(e) = state.rules.entities.get(id) {
            let packet = actor_event(e.runtime_id, shown_event, data);
            send_to(&mut state.players, &e.viewers, &packet);
        }
    }
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
    result
}

/// A painting that `source` broke drops nothing when that is a player with infinite items, or
/// a projectile such a player shot (PMMP `Painting::onDeath`: the last damage's damager).
fn broken_by(state: &mut State, id: Id, source: DamageSource) {
    let (DamageSource::Entity(Actor::Player(p))
    | DamageSource::Child {
        owner: Some(Actor::Player(p)),
        ..
    }) = source
    else {
        return;
    };
    let infinite = state.players.get(&p.session()).is_some_and(|player| {
        matches!(
            survival::game_mode(state, player),
            GameMode::Creative | GameMode::Spectator
        )
    });
    if infinite
        && let Some(e) = state.rules.entities.get_mut(id)
        && let Kind::Painting(painting) = &mut e.kind
    {
        painting.drops = false;
    }
}

/// How `blow` would hurt the live entity `id` ([`Entity::plan_hit`]).
fn plan_of(state: &State, id: Id, blow: Blow) -> Option<Plan> {
    state
        .rules
        .entities
        .get(id)
        .filter(|e| !e.flagged)
        .and_then(|e| e.plan_hit(blow.cause, blow.amount))
}

/// Raises [`EntityDamage`] for `blow` on the entity `id` as `plan`ned, with the knockback
/// `knock` gives: what the hit takes and its knockback as the listeners leave them, and the
/// event; `None` when a listener cancelled it or changed the game so that the hit no longer
/// goes as planned.
fn damage_event(
    state: &mut State,
    id: Id,
    blow: Blow,
    plan: Plan,
    knock: Option<Knock>,
) -> Option<(Taken, Option<Knock>, EntityDamage)> {
    let mut e = damage_of(id, blow, plan.previous, knock);
    dispatch::call(state, &mut e);
    if e.cancelled
        || (dispatch::listened::<EntityDamage>(state) && plan_of(state, id, blow) != Some(plan))
    {
        return None;
    }
    let taken = Taken {
        base: e.base_damage,
        damage: e.final_damage(),
        cooldown: u8::try_from(e.attack_cooldown_ticks).unwrap_or(u8::MAX),
    };
    let knock = knock.map(|k| Knock {
        force: e.knockback,
        vertical: e.vertical_knockback_limit,
        ..k
    });
    Some((taken, knock, e))
}

/// [`EntityDamage`] for `blow` on the entity `id`, not cancelled: with the cooldown modifier of a
/// hit within the cooldown of a `previous` one, and the knockback `knock` gives.
fn damage_of(id: Id, blow: Blow, previous: Option<f64>, knock: Option<Knock>) -> EntityDamage {
    let mut modifiers = [None; DamageModifier::ALL.len()];
    if let Some(previous) = previous
        && let Some(slot) = modifiers.get_mut(DamageModifier::PreviousDamageCooldown as usize)
    {
        *slot = Some(-previous);
    }
    let (force, vertical) = knock.map_or((KNOCKBACK, KNOCKBACK_VERTICAL_LIMIT), |k| {
        (k.force, k.vertical)
    });
    EntityDamage {
        entity: Actor::Entity(id),
        cause: blow.cause.event_cause(),
        source: blow.source,
        base_damage: blow.amount,
        original_base_damage: blow.amount,
        modifiers,
        original_modifiers: modifiers,
        attack_cooldown_ticks: u32::from(ATTACK_COOLDOWN),
        knockback: force,
        vertical_knockback_limit: vertical,
        cancelled: false,
    }
}

/// An invulnerable NPC ignores `blow` (see [`hurt`]); one an entity dealt (a player's hit, a
/// projectile) still raises [`EntityDamage`], already cancelled, so listeners learn who hit it,
/// as PMMP raises the event of a hit that does not land cancelled. The NPC takes nothing
/// whatever they do, and no monitor sees a cancelled event. A blow of nothing or a block (a
/// fall, a cactus) raises nothing.
fn shielded(state: &mut State, id: Id, blow: Blow, knock: Option<Knock>) {
    let dealt = matches!(
        blow.source,
        DamageSource::Entity(_) | DamageSource::Child { .. }
    );
    let invulnerable = state.rules.entities.get(id).is_some_and(|e| {
        !e.flagged && matches!(&e.kind, Kind::Npc(c) if c.invulnerable && c.dead_ticks.is_none())
    });
    if dealt
        && invulnerable
        && (0.0..=MAX_DAMAGE).contains(&blow.amount)
        && dispatch::heard::<EntityDamage>(state)
    {
        let mut e = damage_of(id, blow, None, knock);
        e.cancelled = true;
        dispatch::call(state, &mut e);
    }
}

/// The NPC `id` died at `at` (PMMP `Living::onDeath`), seen at its first tick after the death,
/// whatever killed it (a hit, the API): it drops its kind's drops and experience (a zombie's;
/// other NPCs drop nothing; no experience with `experience = false`), as [`EntityDeath`]'s
/// listeners leave them. Drops the world cannot take (the entity limit) are lost: they never
/// existed outside the rule.
fn died(state: &mut State, id: Id, world: WorldIndex, at: Vec3) {
    let Some(npc) = state.rules.entities.get(id) else {
        return;
    };
    let zombie = matches!(
        npc.kind,
        Kind::Npc(Character {
            form: Form::Zombie { .. },
            ..
        })
    );
    let (mut drops, mut xp) = (Vec::new(), 0);
    if zombie {
        let seed = state.worlds.get(world).map_or(0, World::seed);
        let key = random::key(
            DEATH_RANDOM,
            &[u64::from(id.index()), u64::from(id.generation())],
        );
        drops = zombie_drops(&mut Random::new(seed, state.tick, key));
        if state.settings.has_orbs() {
            xp = ZOMBIE_XP;
        }
    }
    let mut event = None;
    if dispatch::heard::<EntityDeath>(state) {
        let mut e = EntityDeath {
            entity: id,
            drops,
            xp_drop_amount: xp,
        };
        dispatch::call(state, &mut e);
        (drops, xp) = (e.drops.clone(), e.xp_drop_amount);
        event = Some(e);
    }
    for stack in drops {
        if !drop_item(state, world, at, stack) {
            debug!(world, "a death drop did not fit the world");
        }
    }
    drop_experience(state, world, at, xp);
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
}

/// What a dead zombie drops (PMMP `Zombie::getDrops`): 0 to 2 rotten flesh, and with a 5 in 200
/// chance one iron ingot, carrot or potato.
fn zombie_drops(r: &mut Random) -> Vec<Stack> {
    let one = |kind: crate::item::Kind, count: i32| {
        Stack::new(kind.default_type(), u8::try_from(count).unwrap_or(0))
    };
    let mut drops = vec![one(crate::item::Kind::RottenFlesh, r.range(0, 2))];
    if r.range(0, 199) < 5 {
        let rare = [
            crate::item::Kind::IronIngot,
            crate::item::Kind::Carrot,
            crate::item::Kind::Potato,
        ];
        let pick = usize::try_from(r.range(0, 2)).unwrap_or(0);
        drops.extend(rare.get(pick).map(|&k| one(k, 1)));
    }
    drops.retain(|s| !s.is_empty());
    drops
}

/// Sets the motion a rule gives the entity `id` (knockback, an explosion's push, a Punch arrow;
/// PMMP `Entity::setMotion`), through [`EntityMotion`]: cancelling keeps its motion, and a
/// listener's motion replaces the push.
fn push_entity(state: &mut State, id: Id, motion: Vec3) {
    let mut motion = physics::bounded(motion);
    let mut event = None;
    if dispatch::heard::<EntityMotion>(state) {
        let mut e = EntityMotion {
            entity: Actor::Entity(id),
            motion,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled || (dispatch::listened::<EntityMotion>(state) && !is_live(state, id)) {
            return;
        }
        motion = e.motion;
        event = Some(e);
    }
    with(state, id, |e| e.body.motion = motion);
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
}

/// Sets the entity `id` on fire for `seconds` as a rule does (fire, lava, a burning arrow, Fire
/// Aspect; PMMP `EntityCombustByBlockEvent`, `EntityCombustByEntityEvent`): through
/// [`EntityCombust`], whose listeners may change the time or cancel it.
fn combust(state: &mut State, id: Id, seconds: i16, source: CombustSource) {
    if !dispatch::heard::<EntityCombust>(state) {
        with(state, id, |e| e.set_on_fire(seconds));
        return;
    }
    let would = |state: &State| {
        state
            .rules
            .entities
            .get(id)
            .is_some_and(|e| !e.flagged && e.would_burn(seconds))
    };
    if !would(state) {
        return;
    }
    let mut e = EntityCombust {
        entity: Actor::Entity(id),
        duration_secs: u32::from(seconds.unsigned_abs()),
        source,
        cancelled: false,
    };
    dispatch::call(state, &mut e);
    if e.cancelled || (dispatch::listened::<EntityCombust>(state) && !would(state)) {
        return;
    }
    let seconds = i16::try_from(e.duration_secs).unwrap_or(i16::MAX);
    with(state, id, |e| e.set_on_fire(seconds));
    dispatch::watch(state, &e);
}

/// The entity `id` stopped burning ([`EntityExtinguish`], after the change).
fn extinguished(state: &mut State, id: Id, cause: ExtinguishCause) {
    if dispatch::heard::<EntityExtinguish>(state) {
        let mut e = EntityExtinguish {
            entity: Actor::Entity(id),
            cause,
        };
        dispatch::call(state, &mut e);
        dispatch::watch(state, &e);
    }
}

/// Puts the entity `id` out for `cause`; true if it was burning.
fn put_out(state: &mut State, id: Id, cause: ExtinguishCause) -> bool {
    let burning = with(state, id, |e| {
        let burning = e.fire_ticks > 0;
        e.fire_ticks = 0;
        burning
    }) == Some(true);
    if burning {
        extinguished(state, id, cause);
    }
    burning
}

/// The world of the entity `id` and its block at `pos`.
fn block_at(state: &State, id: Id, pos: BlockPos) -> Option<(WorldIndex, block::State)> {
    let world = state.rules.entities.get(id)?.world;
    Some((world, state.worlds.get(world)?.block(pos)?))
}

/// Heals the entity `id` by `amount` as a rule does (instant health; PMMP `Entity::heal`),
/// through [`EntityRegainHealth`] as a player heals: nothing happens, and no event is raised,
/// unless the amount [`survival::heals`] it; listeners may change the amount or cancel it, and
/// an amount they leave that does not heal it cancels it.
fn heal(state: &mut State, id: Id, amount: f64, reason: RegainReason) {
    let heals_now = |state: &State, amount: f64| {
        state.rules.entities.get(id).is_some_and(|e| {
            let alive = !e.flagged && e.is_alive();
            survival::heals(alive, e.health, e.kind.max_health(), amount)
        })
    };
    if !heals_now(state, amount) {
        return;
    }
    let mut amount = amount;
    let mut event = None;
    if dispatch::heard::<EntityRegainHealth>(state) {
        let mut e = EntityRegainHealth {
            entity: Actor::Entity(id),
            amount,
            regain_reason: reason,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled
            || (dispatch::listened::<EntityRegainHealth>(state) && !heals_now(state, e.amount))
        {
            return;
        }
        amount = e.amount;
        event = Some(e);
    }
    with(state, id, |e| {
        let max = e.kind.max_health();
        e.health = crate::math::min(e.health + amount, max);
    });
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
}

/// [`hurt`] on the store and the players, without events (the API and the rules' quiet path).
fn hurt_in(
    entities: &mut Entities,
    players: &mut BTreeMap<SessionId, Player>,
    id: Id,
    cause: Cause,
    amount: f64,
    knock: Option<Knock>,
) -> Hurt {
    let Some(e) = entities.get_mut(id).filter(|e| !e.flagged) else {
        return Hurt::Ignored;
    };
    let result = match e.plan_hit(cause, amount) {
        Some(plan) => e.take_hit(plan, Taken::of(plan, amount)),
        None => Hurt::Ignored,
    };
    let removed = e.after_hit(cause, result);
    let living = matches!(e.kind, Kind::Npc(_));
    let shown = living && matches!(result, Hurt::Took { cold: true } | Hurt::Died);
    let event = if shown {
        if let Some(m) = knock.and_then(|k| k.applied_to(e.body.motion)) {
            e.body.motion = physics::bounded(m);
        }
        Some(if result == Hurt::Died {
            (ActorEvent::Death, 0)
        } else {
            (ActorEvent::Hurt, cause.bds_cause())
        })
    } else {
        None
    };
    if let Some((event, data)) = event {
        send_to(players, &e.viewers, &actor_event(e.runtime_id, event, data));
    }
    let key = (e.world, e.chunk);
    if result != Hurt::Ignored {
        entities.changed.insert(key);
    }
    if result == Hurt::Died && !living && removed {
        entities.doom(id);
    }
    result
}

/// Plays `sound` at `at` for the game (PMMP `World::addSound`, [`super::emit`]).
fn play(state: &mut State, world: WorldIndex, at: Vec3, sound: crate::world::Sound) {
    super::emit::sound(state, world, at, sound);
}

/// Knocks a player back away along `(x, z)` with `force`, up at most `vertical` ([`Knock`]),
/// pushing it as everything else does ([`push_player`]): the survival rules' melee knockback.
pub(in crate::tick) fn knock_player(
    state: &mut State,
    session: SessionId,
    (x, z): (f64, f64),
    (force, vertical): (f64, f64),
    raise: Raise,
) {
    let knock = Knock {
        x,
        z,
        force,
        vertical,
    };
    if let Some(motion) = knock.applied_to(Vec3::ZERO) {
        push_player(state, session, motion, raise);
    }
}

/// Pushes a player: its client is sent the motion, and so are the players who see it (PMMP
/// `Player::setMotion`; the server keeps no motion of players). With [`Raise::Events`] it goes
/// through [`EntityMotion`]: cancelling sends nothing, and a listener's motion replaces the push.
fn push_player(state: &mut State, session: SessionId, motion: Vec3, raise: Raise) {
    let mut motion = physics::bounded(motion);
    let mut event = None;
    if raise == Raise::Events && dispatch::heard::<EntityMotion>(state) {
        if !state.players.contains_key(&session) {
            return;
        }
        let mut e = EntityMotion {
            entity: player_actor(session),
            motion,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled {
            return;
        }
        motion = e.motion;
        event = Some(e);
    }
    send_motion(state, session, motion);
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
}

/// Sends a player's motion to it and the players it is shown to ([`Entities::watched`]), so not
/// to those who hide it.
fn send_motion(state: &mut State, session: SessionId, motion: Vec3) {
    let State { players, rules, .. } = state;
    let Some(p) = players.get_mut(&session) else {
        return;
    };
    let packet = Packet::SetActorMotion(crate::protocol::packet::SetActorMotion {
        entity_runtime_id: p.runtime_id,
        velocity: motion.to_f32(),
        tick: 0,
    });
    p.send_to_viewer(packet.clone());
    let Some(watched) = rules.entities.watched.get(&session) else {
        return;
    };
    for s in &watched.viewers {
        if let Some(viewer) = players.get_mut(s) {
            viewer.send_to_viewer(packet.clone());
        }
    }
}

/// The look direction of a yaw and pitch in degrees, of an entity or a player (PMMP
/// `Entity::getDirectionVector`).
pub(super) fn direction(yaw: f64, pitch: f64) -> Vec3 {
    let (yaw, pitch) = (crate::math::radians(yaw), crate::math::radians(pitch));
    let xz = crate::math::cos(pitch);
    Vec3::new(
        -xz * crate::math::sin(yaw),
        -crate::math::sin(pitch),
        xz * crate::math::cos(yaw),
    )
    .normalize()
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

/// The rule of a kind.
fn rule_of(kind: &Kind) -> Rule {
    match kind {
        Kind::Item(_) => Rule::Item,
        Kind::ExperienceOrb(_) => Rule::Orb,
        Kind::FallingBlock(_) => Rule::Falling,
        Kind::Tnt(_) => Rule::Tnt,
        Kind::Projectile(_) => Rule::Projectile,
        Kind::Cloud(_) => Rule::Cloud,
        Kind::Npc(Character {
            form: Form::Custom(_),
            ..
        }) => Rule::Custom,
        Kind::Npc(_) => Rule::Npc,
        Kind::Firework(_) => Rule::Firework,
        Kind::Painting(_) | Kind::EndCrystal(_) => Rule::Inert,
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
    let heard = dispatch::heard::<ItemDespawn>(state);
    let Some(e) = state.rules.entities.get_mut(id) else {
        return;
    };
    if e.flagged {
        return;
    }
    let Kind::Item(item) = &mut e.kind else {
        return;
    };
    // Counting stops at 0, so the time being up never reads as NEVER (-1).
    if item.despawn_delay > 0 {
        item.despawn_delay -= 1;
    }
    if item.despawn_delay == 0 {
        if heard {
            despawn_item(state, id);
        } else {
            e.flagged = true;
        }
    }
}

/// A dropped item's time is up (PMMP `ItemEntity::entityBaseTick`): [`ItemDespawn`], whose
/// cancelling gives it the whole time again; a listener that gave it time itself keeps it.
fn despawn_item(state: &mut State, id: Id) {
    let due = |state: &State| {
        state
            .rules
            .entities
            .get(id)
            .and_then(as_item)
            .is_some_and(|i| i.despawn_delay == 0)
    };
    let mut e = ItemDespawn {
        entity: id,
        cancelled: false,
    };
    dispatch::call(state, &mut e);
    if e.cancelled {
        let delay = item_despawn_delay(state);
        if due(state)
            && let Some(Kind::Item(item)) = state.rules.entities.get_mut(id).map(|e| &mut e.kind)
        {
            item.despawn_delay = delay;
        }
        return;
    }
    if dispatch::listened::<ItemDespawn>(state) && !due(state) {
        return;
    }
    with(state, id, |e| e.flagged = true);
    dispatch::watch(state, &e);
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
    entities.each_nearby(this.world, this.bounding_box().grow(MERGE_REACH), |other| {
        if other == id {
            return;
        }
        let Some(o) = entities.get(other).and_then(as_item) else {
            return;
        };
        if is_mergeable(o, item) {
            mergeable.push(other);
            if o.stack.count() > target.1 {
                target = (other, o.stack.count());
            }
        }
    });
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
    let mergeable = |state: &State| {
        let entities = &state.rules.entities;
        match (
            entities.get(source).and_then(as_item),
            entities.get(target).and_then(as_item),
        ) {
            (Some(s), Some(t)) => source != target && is_mergeable(s, t),
            _ => false,
        }
    };
    if !mergeable(state) {
        return false;
    }
    let mut event = None;
    if dispatch::heard::<ItemMerge>(state) {
        let mut e = ItemMerge {
            entity: source,
            target,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled || (dispatch::listened::<ItemMerge>(state) && !mergeable(state)) {
            return false;
        }
        event = Some(e);
    }
    let merged = merge_now(state, source, target);
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
    merged
}

/// Moves the stack of `source` into `target`, which can merge ([`merge_into`]).
fn merge_now(state: &mut State, source: Id, target: Id) -> bool {
    let entities = &mut state.rules.entities;
    let (Some(s), Some(t)) = (entities.get(source), entities.get(target)) else {
        return false;
    };
    let (Some(si), Some(ti)) = (as_item(s), as_item(t)) else {
        return false;
    };
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
    let key = (t.world, t.chunk);
    send_to(&mut state.players, &t.viewers, &packet);
    entities.changed.insert(key);
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
            p.send_packet(packet.clone());
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
    let reach = ORB_TARGET_DISTANCE * ORB_TARGET_DISTANCE;
    let valid = |s: &SessionId| {
        state.players.get(s).filter(|p| {
            p.world == world
                && collects(state, p, hooks)
                && p.can_attract_xp_orbs()
                && survival::feet(p).distance_squared(position) <= reach
        })
    };
    let mut target = orb.target.filter(|s| valid(s).is_some());
    let mut look = orb.look_for_target;
    if look >= ORB_LOOK_PERIOD {
        if target.is_none() {
            let mut best = reach;
            for (&s, p) in &state.players {
                if p.world != world || !collects(state, p, hooks) || !p.can_attract_xp_orbs() {
                    continue;
                }
                let d = survival::feet(p).distance_squared(position);
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
        let aim = survival::feet(p) + Vec3::new(0.0, EYE_HEIGHT / 2.0, 0.0);
        let vector = (aim - position) * (1.0 / ORB_TARGET_DISTANCE);
        let distance = vector.length_squared();
        if distance < 1.0 {
            let k = 1.0 - distance.sqrt();
            pull = vector.normalize() * (0.2 * k * k);
        }
        if bounds.overlaps(survival::bounding_box(p), EPSILON) {
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
    let (width, height) = e.size();
    let (world, position, on_ground) = (e.world, e.body.position, e.body.on_ground);
    let half = width / 2.0;
    let pos = BlockPos::containing(position + Vec3::new(-half, height, -half));
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
        let before = w.block(pos).unwrap_or(block::AIR);
        land(state, id, world, pos, before, result)
    } else {
        match falling.block.item() {
            block::ItemForm::Item(t) => drop_item(state, world, position, Stack::new(t, 1)),
            block::ItemForm::None | block::ItemForm::BlockEntity => true,
        }
    };
    if done && let Some(e) = state.rules.entities.get_mut(id) {
        e.flagged = true;
        let key = (e.world, e.chunk);
        state.rules.entities.changed.insert(key);
    }
}

/// The falling block `id` becomes `to` at `pos`, where `before` is (PMMP `FallingBlock::
/// entityBaseTick`), through [`EntityBlockChange`]: cancelling, or a listener leaving a block
/// there that cannot be replaced, keeps the old block and the falling block goes all the same
/// (PMMP). True when the falling block is done; false to try again next tick (the block could
/// not be set).
fn land(
    state: &mut State,
    id: Id,
    world: WorldIndex,
    pos: BlockPos,
    before: block::State,
    to: block::State,
) -> bool {
    let mut event = None;
    if dispatch::heard::<EntityBlockChange>(state) {
        let mut e = EntityBlockChange {
            entity: id,
            world: world::Id::of(world),
            pos,
            block: before,
            to,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        let replaceable = |state: &State| {
            is_live(state, id)
                && state
                    .worlds
                    .get(world)
                    .and_then(|w| w.block(pos))
                    .is_some_and(block::State::can_be_replaced)
        };
        if e.cancelled || (dispatch::listened::<EntityBlockChange>(state) && !replaceable(state)) {
            return true;
        }
        event = Some(e);
    }
    let set = state
        .worlds
        .get_mut(world)
        .is_some_and(|w| w.set_block(pos, to).is_ok());
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
    set
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
/// farmland and takes fall damage ([`npc::landed`]); a falling anvil hurts the players (in
/// `hits`, with the anvil) and NPCs under it as a falling block (PMMP `Anvil::onHitGround`) and
/// may take damage, or break.
fn landed(
    state: &mut State,
    id: Id,
    fall: f64,
    hooks: Hooks,
    hits: &mut Vec<(SessionId, f64, Id)>,
) {
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
            if targetable(state, p, world, hooks)
                && survival::bounding_box(p).overlaps(bounds, EPSILON)
            {
                hits.push((s, damage, id));
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
                let blow = Blow::of(Cause::FallingBlock, damage)
                    .by(DamageSource::Entity(Actor::Entity(id)));
                hurt(state, other, blow, Some(knock));
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
    let Some(next) = rank.and_then(|i| ANVILS.get(i + 1)) else {
        state.rules.entities.doom(id);
        return;
    };
    let properties: Vec<(&str, block::PropertyValue<'_>)> = falling
        .block
        .properties()
        .map(|p| (p.name(), p.value()))
        .collect();
    if let Some(damaged) = block::State::find(next, &properties)
        && let Some(e) = state.rules.entities.get_mut(id)
    {
        e.kind = Kind::FallingBlock(Falling { block: damaged });
    }
}

/// Players pick up the items they touch (PMMP `Player::checkNearEntities` and
/// `ItemEntity::onCollideWithPlayer`): what fits goes to the inventory; a stack taken whole is
/// shown taken (`TakeItemActor`, then `RemoveActor`), a part leaves the rest on the ground.
/// Stuck arrows are picked up as PMMP `Arrow::onCollideWithPlayer` says. A player looks once
/// every `item-pickup-interval` ticks, on the ticks its session number staggers it to, so the
/// players' looks spread over the interval.
fn pick_up(state: &mut State, hooks: Hooks) {
    let (tick, interval) = (state.tick, u64::from(state.settings.item_pickup_interval));
    let sessions: Vec<SessionId> = state
        .players
        .iter()
        .filter(|(s, p)| {
            tick.wrapping_add(s.get()).is_multiple_of(interval) && collects(state, p, hooks)
        })
        .map(|(&s, _)| s)
        .collect();
    for session in sessions {
        let Some(player) = state.players.get(&session) else {
            continue;
        };
        let (world, runtime_id) = (player.world, player.runtime_id);
        let reach = survival::bounding_box(player).grow(PICKUP_REACH);
        let mut taken = 0;
        for id in state.rules.entities.nearby(world, reach) {
            if taken >= MAX_PICKUPS_PER_PLAYER {
                break;
            }
            if projectile::pick_up(state, session, id, hooks) {
                taken += 1;
                continue;
            }
            if dispatch::heard::<EntityItemPickup>(state) {
                if pick_up_heard(state, session, id, hooks) {
                    taken += 1;
                }
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
            let whole = rest.is_empty();
            let packet = if whole {
                Packet::TakeItemActor(TakeItemActor {
                    item_entity_runtime_id: e.runtime_id,
                    taker_entity_runtime_id: runtime_id,
                })
            } else {
                let count = rest.count();
                item.stack = rest;
                stack_size(e.runtime_id, count)
            };
            send_to(&mut state.players, &e.viewers, &packet);
            let entities = &mut state.rules.entities;
            if whole {
                entities.doom(id);
            }
            entities.changed.insert(key);
        }
    }
}

/// A player picks up the item `id` while someone listens to [`EntityItemPickup`] (PMMP
/// `ItemEntity::onCollideWithPlayer`): the event offers what fits; cancelling leaves the item
/// where it is, and so does a listener that changes the item, the player or its room so that
/// the offer no longer holds. An item a listener replaced is given instead (what of it does not
/// fit drops where the item was), and the item on the ground loses what was offered. True if the
/// item was taken, whole or in part.
fn pick_up_heard(state: &mut State, session: SessionId, id: Id, hooks: Hooks) -> bool {
    let offer = |state: &State| -> Option<(Stack, Stack)> {
        let e = state.rules.entities.get(id).filter(|e| !e.flagged)?;
        let Kind::Item(item) = &e.kind else {
            return None;
        };
        let player = state.players.get(&session)?;
        let reach = survival::bounding_box(player).grow(PICKUP_REACH);
        if item.pickup_delay != 0
            || !collects(state, player, hooks)
            || player.world != e.world
            || !e.bounding_box().overlaps(reach, EPSILON)
        {
            return None;
        }
        let room = (hooks.room)(player, &item.stack).min(item.stack.count());
        (room > 0).then(|| (item.stack.clone(), item.stack.with_count(room)))
    };
    let Some(offered) = offer(state) else {
        return false;
    };
    let Some(e) = pickup_event(state, session, id, &offered.1, |s| {
        offer(s).as_ref() == Some(&offered)
    }) else {
        return false;
    };
    let (stack, offered) = offered;
    let replaced = e.item != offered;
    let Some(player) = state.players.get_mut(&session) else {
        return false;
    };
    let taker = player.runtime_id;
    let rest = (hooks.give)(player, e.item.clone());
    let given = if replaced {
        offered.count()
    } else {
        offered.count().saturating_sub(rest.count())
    };
    if given == 0 {
        return false;
    }
    let left = stack.count().saturating_sub(given);
    let entities = &mut state.rules.entities;
    let Some(en) = entities.get_mut(id) else {
        return false;
    };
    let (key, at, world) = ((en.world, en.chunk), en.body.position, en.world);
    let packet = if left == 0 {
        Packet::TakeItemActor(TakeItemActor {
            item_entity_runtime_id: en.runtime_id,
            taker_entity_runtime_id: taker,
        })
    } else {
        if let Kind::Item(item) = &mut en.kind {
            item.stack = stack.with_count(left);
        }
        stack_size(en.runtime_id, left)
    };
    send_to(&mut state.players, &en.viewers, &packet);
    if left == 0 {
        entities.doom(id);
    }
    entities.changed.insert(key);
    if replaced && !drop_item(state, world, at, rest) {
        debug!(
            world,
            "what of a listener's pickup item did not fit was lost"
        );
    }
    dispatch::watch(state, &e);
    true
}

/// Raises [`EntityItemPickup`] for `session` picking up `origin` with `offered`: the event to
/// apply, or `None` when a listener cancelled it or `still` (the pickup's check) fails after
/// the listeners.
fn pickup_event(
    state: &mut State,
    session: SessionId,
    origin: Id,
    offered: &Stack,
    still: impl Fn(&State) -> bool,
) -> Option<EntityItemPickup> {
    let mut e = EntityItemPickup {
        entity: player_actor(session),
        origin,
        item: offered.clone(),
        cancelled: false,
    };
    dispatch::call(state, &mut e);
    if e.cancelled || (dispatch::listened::<EntityItemPickup>(state) && !still(state)) {
        return None;
    }
    Some(e)
}

/// The actor of a player's session in events.
fn player_actor(session: SessionId) -> Actor {
    Actor::Player(crate::player::Id::new(session))
}

/// How much of `stack` a player's inventory takes now, up to its count: what
/// [`inventory::give`] would put in (PMMP `getAddableItemQuantity`), so that a pickup is offered
/// before anything moves.
fn room(player: &Player, stack: &Stack) -> u8 {
    player
        .inventory(crate::inventory::Kind::Main)
        .addable_quantity(stack)
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
        if let Kind::Painting(p) = &e.kind
            && e.health <= 0.0
        {
            object::painting_broke(state, e.world, e.body.position, *p);
        }
        if dispatch::heard::<EntityDespawn>(state) {
            let mut e = EntityDespawn { entity: id };
            dispatch::call(state, &mut e);
            dispatch::watch(state, &e);
        }
    }
}

/// The players who may see entities and other players (their view is open), in session order,
/// with their world and view.
fn watchers(state: &State) -> Vec<(SessionId, WorldIndex, &View)> {
    state
        .players
        .iter()
        .filter(|(_, p)| p.view.is_open())
        .map(|(&s, p)| (s, p.world, &p.view))
        .collect()
}

/// Each player's bound of this tick's spawns and removals ([`MAX_SPAWNS_PER_PLAYER`]), which the
/// tracking of entities and then of players spend.
fn spawn_budget(state: &State) -> BTreeMap<SessionId, u32> {
    state
        .players
        .keys()
        .map(|&s| (s, MAX_SPAWNS_PER_PLAYER))
        .collect()
}

/// Sends players the entities of the chunks they see and takes away the others, within each
/// player's `budget`; then the movement of up to [`MAX_MOVEMENTS_PER_TICK`] entities.
fn track_entities(state: &mut State, budget: &mut BTreeMap<SessionId, u32>) {
    let departed = std::mem::take(&mut state.rules.entities.departed);
    for mut d in departed {
        d.viewers.retain(|&s| {
            if !spend(budget, s) {
                return true;
            }
            if let Some(p) = state.players.get_mut(&s) {
                p.send_packet(track::remove(d.runtime_id));
            }
            false
        });
        if !d.viewers.is_empty() {
            state.rules.entities.departed.push(d);
        }
    }
    // The players of each column are worked out before anything is sent (sending changes no
    // view); only the columns with arrivals and those whose players changed are kept. `watchers`
    // is in session order, as a column's players are, so comparing the sequences compares the
    // sets. A column whose players changed compares every entity; any other only its arrivals,
    // in `Id` order as the whole column would be.
    let watchers = watchers(state);
    let pending: Vec<(Key, BTreeSet<SessionId>, bool)> = state
        .rules
        .entities
        .columns
        .iter()
        .filter_map(|(&key, column)| {
            let want = watchers
                .iter()
                .filter(|&&(_, w, view)| w == key.0 && view.sees(key.1))
                .map(|&(s, _, _)| s);
            let changed = !column.viewers.iter().copied().eq(want.clone());
            (changed || !column.arrived.is_empty()).then(|| (key, want.collect(), changed))
        })
        .collect();
    for (key, want, changed) in pending {
        let entities = &mut state.rules.entities;
        let Some(column) = entities.columns.get_mut(&key) else {
            continue;
        };
        column.viewers.clone_from(&want);
        let arrived = std::mem::take(&mut column.arrived);
        let ids: Vec<Id> = if changed {
            column.ids.iter().copied().collect()
        } else {
            arrived.into_iter().collect()
        };
        let mut held_back = Vec::new();
        for id in ids {
            let Some(e) = state.rules.entities.get_mut(id) else {
                continue;
            };
            if e.runtime_id == 0 || e.flagged || (e.world, e.chunk) != key {
                continue;
            }
            if !show_to(&mut state.players, e, &want, budget) {
                held_back.push(id);
            }
        }
        if let Some(column) = state.rules.entities.columns.get_mut(&key) {
            column.arrived.extend(held_back);
        }
    }
    send_movements(state);
}

/// Sends `e` to the players of `want` that do not see it and takes it away from its other
/// viewers, within each player's `budget`; false if a bound held a packet back.
fn show_to(
    players: &mut BTreeMap<SessionId, Player>,
    e: &mut Entity,
    want: &BTreeSet<SessionId>,
    budget: &mut BTreeMap<SessionId, u32>,
) -> bool {
    #[cfg(test)]
    tests::count_tracked();
    let leaving: Vec<SessionId> = e.viewers.difference(want).copied().collect();
    let coming: Vec<SessionId> = want.difference(&e.viewers).copied().collect();
    let mut sent = true;
    for s in leaving {
        let Some(player) = players.get_mut(&s) else {
            e.viewers.remove(&s);
            continue;
        };
        if spend(budget, s) {
            player.send_packet(track::remove(e.runtime_id));
            e.viewers.remove(&s);
        } else {
            sent = false;
        }
    }
    for s in coming {
        let Some(player) = players.get_mut(&s) else {
            continue;
        };
        if spend(budget, s) {
            for packet in track::spawn(e) {
                player.send_packet(packet);
            }
            e.viewers.insert(s);
        } else {
            sent = false;
        }
    }
    sent
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
    let mut packets = Vec::new();
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
            movement(e, change, &mut packets);
            for s in &e.viewers {
                if let Some(p) = state.players.get_mut(s) {
                    for packet in &packets {
                        p.send_packet(packet.clone());
                    }
                }
            }
            packets.clear();
        }
        commit_movement(e, change);
    }
    state.rules.entities.cursor = start;
}

/// Puts in `packets` the packets an entity's change since it was last sent needs (PMMP
/// `updateMovement`): the flags (no client prediction while it rests, burning), or all its
/// metadata (and an NPC's held item) after an API change, the position. Its speed goes only in
/// its spawn packet: BDS 1.26.60 sends no `SetActorMotion` for entities but players (P6).
fn movement(e: &Entity, change: Change, packets: &mut Vec<Packet>) {
    if e.resend {
        packets.push(track::all_data(e, change.flags));
        if let Kind::Npc(c) = &e.kind {
            packets.push(track::held(e.runtime_id, &c.held, 0));
        }
    } else if change.flags != e.sent.flags || change.radius.is_some() {
        packets.push(track::data(e.runtime_id, change.flags, change.radius));
    }
    if change.moved {
        packets.push(track::movement(e));
    }
    if change.health
        && let Some(packet) = track::health_update(e)
    {
        packets.push(packet);
    }
}

/// Records what [`movement`] sent as the entity's last sent state.
fn commit_movement(e: &mut Entity, change: Change) {
    e.still = change.still;
    e.sent.flags = change.flags;
    e.resend = false;
    if let Some(r) = change.radius {
        e.sent.radius = r;
    }
    if change.health {
        e.sent.health = e.health;
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

/// The players that should see each spawned player, in session order. It is worked out before
/// anything is sent (sending changes no view), and each watcher's view is looked up once, not
/// once per pair.
fn player_viewers(state: &State) -> Vec<(SessionId, BTreeSet<SessionId>)> {
    let views = watchers(state);
    let hidden = &state.rules.entities.hidden;
    state
        .players
        .iter()
        .filter(|(_, p)| p.phase == Phase::Spawned)
        .map(|(&t, target)| {
            let (world, chunk) = (target.world, chunk_of(target.position));
            let want = views
                .iter()
                .filter(|&&(s, w, view)| {
                    s != t && w == world && view.sees(chunk) && !hidden.contains(&(s, t))
                })
                .map(|&(s, _, _)| s)
                .collect();
            (t, want)
        })
        .collect()
}

/// Removes the players no longer spawned from those who saw them.
fn untrack_gone_players(state: &mut State) {
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
            if state.settings.locator_bar {
                let bar = track::locator_removed(w.runtime_id);
                send_to(&mut state.players, &w.viewers, &bar);
            }
        }
    }
}

/// Shows players to each other: a player whose chunk another sees is added to it, moved, its
/// equipment changes sent, and removed when it leaves the view, the world or the server, or is
/// hidden from it. Adding and removing spend the viewer's `budget`: a viewer over it is shown
/// the player, or has it taken away, in a later tick.
fn track_players(state: &mut State, budget: &mut BTreeMap<SessionId, u32>) {
    untrack_gone_players(state);
    let mut packets: Vec<(SessionId, Packet)> = Vec::new();
    let mut changes = Vec::new();
    for (t, want) in player_viewers(state) {
        let Some(target) = state.players.get(&t) else {
            continue;
        };
        let before = state.rules.entities.watched.remove(&t);
        let old = before.as_ref().map(|w| &w.viewers);
        let mut viewers = BTreeSet::new();
        for &s in &want {
            if old.is_some_and(|v| v.contains(&s)) || spend(budget, s) {
                viewers.insert(s);
            }
        }
        for &s in old.into_iter().flat_map(|v| v.difference(&want)) {
            if !spend(budget, s) {
                viewers.insert(s);
            }
        }
        let inv = &target.inventory;
        let now = Watched {
            runtime_id: target.runtime_id,
            viewers,
            position: target.position,
            rotation: survival::rotation(target),
            held: inv.held().clone(),
            held_slot: inv.held_slot(),
            off_hand: inv.offhand().clone(),
            armour: inv.armor().clone(),
        };
        let game_mode = survival::game_mode(state, target);
        // Built only for a tick that has a new viewer.
        let mut add = None;
        for &s in &now.viewers {
            if old.is_some_and(|v| v.contains(&s)) {
                continue;
            }
            let add = add.get_or_insert_with(|| {
                track::add_player(track::Shown {
                    runtime_id: target.runtime_id,
                    uuid: target.identity.uuid,
                    name: &target.identity.name,
                    eye: target.position,
                    game_mode,
                    held: inv.held(),
                    data: survival::full_metadata(&state.settings, target),
                    device_id: &target.identity.client.device_id,
                    build_platform: target.identity.device_os,
                })
            });
            packets.push((s, add.clone()));
            packets.push((s, track::armour(now.runtime_id, &now.armour)));
            packets.push((s, track::off_hand(now.runtime_id, &now.off_hand)));
            if state.settings.locator_bar {
                for p in track::locator_shown(now.runtime_id, target.position) {
                    packets.push((s, p));
                }
            }
        }
        if let Some(before) = &before {
            for &s in before.viewers.difference(&now.viewers) {
                packets.push((s, track::remove(now.runtime_id)));
                if state.settings.locator_bar {
                    packets.push((s, track::locator_removed(now.runtime_id)));
                }
            }
            if before.position != now.position || before.rotation != now.rotation {
                let (yaw, pitch) = now.rotation;
                let ground = survival::is_on_ground(target);
                let moved = track::player_moved(now.runtime_id, now.position, yaw, pitch, ground);
                changes.push(moved);
            }
            // The copies share their data while nothing changed: `same` finds that at once.
            if !before.held.same(&now.held) || before.held_slot != now.held_slot {
                changes.push(track::held(now.runtime_id, &now.held, now.held_slot));
            }
            if !before.off_hand.same(&now.off_hand) {
                changes.push(track::off_hand(now.runtime_id, &now.off_hand));
            }
            if !now.same_armour(before) {
                changes.push(track::armour(now.runtime_id, &now.armour));
            }
            if !changes.is_empty() {
                for &s in before.viewers.intersection(&now.viewers) {
                    packets.extend(changes.iter().map(|p| (s, p.clone())));
                }
                changes.clear();
            }
        }
        for (s, packet) in packets.drain(..) {
            if let Some(p) = state.players.get_mut(&s) {
                p.send_packet(packet);
            }
        }
        state.rules.entities.watched.insert(t, now);
    }
}

/// Takes one spawn or removal from the bound of `session` ([`spawn_budget`]): false, taking
/// nothing, when none is left; true for a session that is gone, which is sent nothing.
fn spend(budget: &mut BTreeMap<SessionId, u32>, session: SessionId) -> bool {
    match budget.get_mut(&session) {
        Some(left) if *left > 0 => {
            *left -= 1;
            true
        }
        Some(_) => false,
        None => true,
    }
}

/// The player of `session` left: its hidden pairs ([`Entities::hidden`]) go, both ways (a scan
/// of every pair, once per leave).
pub(super) fn left(state: &mut State, session: SessionId) {
    state
        .rules
        .entities
        .hidden
        .retain(|(v, t)| *v != session && *t != session);
}

/// Throws the projectile of `item` from `session`'s eyes along its look (snowball, egg, ender
/// pearl, splash or lingering potion, experience bottle; PMMP `ProjectileItem::onClickAir`).
/// False if `item` throws nothing (a bottle o' enchanting with `experience-orbs = false`), the
/// player cannot (not spawned, dead, spectator) or the entity limit is reached (the caller keeps
/// the item; it takes one away on true, and keeps the ender pearl cooldown).
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

/// Throws the trident `session` holds, drawn for `used_ticks` (PMMP
/// `item\Trident::onReleaseUsing`; see [`projectile::release_trident_with`]). False if nothing
/// was thrown; on true the caller takes one trident from the hand (with finite items).
pub(super) fn release_trident(state: &mut State, session: SessionId, used_ticks: u32) -> bool {
    projectile::release_trident_with(state, session, used_ticks, HOOKS)
}

/// Hangs a painting on the `face` side of the block `clicked` (PMMP
/// `PaintingItem::onInteractBlock`; see [`object::place_painting`]).
pub(super) fn place_painting(
    state: &mut State,
    world: WorldIndex,
    clicked: BlockPos,
    face: block::Face,
    random: &mut Random,
) -> Placed {
    object::place_painting(state, world, clicked, face, random)
}

/// Places an end crystal on the block `clicked` (PMMP `item\EndCrystal::onInteractBlock`; see
/// [`object::place_end_crystal`]).
pub(super) fn place_end_crystal(state: &mut State, world: WorldIndex, clicked: BlockPos) -> Placed {
    object::place_end_crystal(state, world, clicked)
}

/// Launches the firework `item` that `session` used at `at` (PMMP `item\FireworkRocket::
/// onInteractBlock`; see [`object::launch_firework`]). False at the entity limit.
pub(super) fn launch_firework(
    state: &mut State,
    session: SessionId,
    world: WorldIndex,
    at: Vec3,
    item: &Stack,
    random: &mut Random,
) -> bool {
    object::launch_firework(state, session, world, at, item, random)
}

/// Turns the TNT block at `pos` into primed TNT with `fuse` ticks (PMMP `TNT::ignite`). False at
/// the entity limit (the block stays) or if the block is not TNT.
#[doc(alias = "TNT::ignite")]
pub(super) fn ignite(state: &mut State, world: WorldIndex, pos: BlockPos, fuse: u32) -> bool {
    explosion::light(state, world, pos, fuse, Raise::Events)
}

/// An explosion of `size` at `center` (PMMP `Explosion::explodeA/B`): breaks blocks through
/// `interact::destroy`, hurts and pushes entities and players, sets fire when `fire`, raising
/// the events of the hits and pushes with [`Raise::Events`]. See [`explosion`] for the bounds.
pub(super) fn explode(
    state: &mut State,
    world: WorldIndex,
    center: Vec3,
    size: f64,
    fire: bool,
    raise: Raise,
) {
    let blast = explosion::Blast::new(size, fire);
    let source = explosion::Source::None;
    explosion::explode_with(state, world, center, blast, source, HOOKS, raise);
}

/// Strikes a lightning bolt at `at` in `world` ([`lightning`]); false, with nothing done, where
/// it cannot strike.
pub(super) fn strike_lightning(
    state: &mut State,
    world: WorldIndex,
    at: Vec3,
    lightning: crate::world::Lightning,
) -> bool {
    lightning::strike(state, world, at, lightning)
}

/// The block `block` that was at `pos` explodes with `blast` from the centre of its place (PMMP
/// `Explosion` of a block: `RespawnAnchor::explode`), raising
/// [`BlockExplode`](crate::event::BlockExplode) and the events of the hits and pushes. The caller
/// has removed the block.
pub(super) fn explode_block(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    block: block::State,
    blast: Blast,
) {
    let center = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.5,
        f64::from(pos.z) + 0.5,
    );
    let source = explosion::Source::Block { pos, block };
    explosion::explode_with(state, world, center, blast, source, HOOKS, Raise::Events);
}

/// `session` hits the entity `target` in melee (PMMP `Player::attackEntity`); see [`npc`].
#[doc(alias = "Player::attackEntity")]
pub(super) fn attack(state: &mut State, session: SessionId, target: Id) {
    npc::attack_with(state, session, target, HOOKS);
}

/// `session` interacts with the entity `target` (PMMP `Player::interactEntity`): a villager NPC
/// within reach (8 blocks, in front) opens trading through `open_trade` (`trade::open`, or
/// `trade::open_from_client` for the player's client). False if nothing happened.
#[doc(alias = "Player::interactEntity")]
pub(super) fn interact(
    state: &mut State,
    session: SessionId,
    target: Id,
    open_trade: fn(&mut State, SessionId, Id) -> bool,
) -> bool {
    npc::interact_with(state, session, target, HOOKS, open_trade)
}

/// Whether `session` can act on the entity `id`: spawned, alive, not a spectator, in its world
/// and within reach of its eyes and look (PMMP `Player::canInteract`, the check of
/// [`interact`] and [`attack`]).
pub(super) fn reaches(state: &State, session: SessionId, id: Id) -> bool {
    npc::reaches(state, session, id, HOOKS).is_some()
}

/// Spawns `npc` at `pos` (feet); `None` where the API may not put an entity
/// ([`api::is_placeable`]: not finite, outside the world border or too far above or below the
/// world), at the entity limit, in an unloaded chunk, or for a human name over
/// [`MAX_NAME_TAG_BYTES`] bytes. NPCs
/// spawned here are invulnerable (only the void hurts them), except zombies (see [`npc`]).
pub(super) fn spawn_npc(state: &mut State, world: WorldIndex, pos: Vec3, npc: Npc) -> Option<Id> {
    if !api::is_placeable(pos) {
        return None;
    }
    npc::spawn(state, world, pos, npc)
}

/// Whether any entity or player whose box blocks placing intersects `area` in `world` (PMMP
/// `World::getCollidingEntities` for `canBePlaced`: alive entities that can be collided with,
/// players that are not spectators).
pub(super) fn occupied(state: &State, world: WorldIndex, area: Aabb) -> bool {
    let entities = &state.rules.entities;
    let mut entity = false;
    entities.each_nearby(world, area, |id| {
        entity = entity
            || entities
                .get(id)
                .is_some_and(|e| e.blocks_placing() && e.bounding_box().intersects(area));
    });
    entity
        || state.players.values().any(|p| {
            p.world == world
                && collects(state, p, HOOKS)
                && survival::bounding_box(p).intersects(area)
        })
}

/// A spawn egg used on a block (PMMP `SpawnEgg::onInteractBlock`): a zombie or a villager (with
/// no trades, and taking damage as PMMP's) stands at `at` facing `yaw`, named after the egg's
/// custom name if it has one. A squid's egg does nothing (squids swim: mob AI is out of scope),
/// and neither does any other item.
pub(super) fn hatch(
    state: &mut State,
    world: WorldIndex,
    at: Vec3,
    yaw: f64,
    egg: &Stack,
) -> Placed {
    let npc = match egg.item().and_then(crate::item::Type::kind) {
        Some(crate::item::Kind::ZombieSpawnEgg) => Npc::Zombie,
        Some(crate::item::Kind::VillagerSpawnEgg) => Npc::Villager {
            profession: 0,
            offers: super::trade::Offers::default(),
        },
        _ => return Placed::Nothing,
    };
    let Some(id) = spawn_npc(state, world, at, npc) else {
        return Placed::Full;
    };
    let name = egg.custom_name();
    with(state, id, |e| {
        e.yaw = yaw;
        if name.len() <= kind::MAX_NAME_TAG_BYTES {
            name.clone_into(&mut e.name_tag);
        }
        if let Kind::Npc(c) = &mut e.kind {
            c.invulnerable = false;
        }
    });
    Placed::Made
}

/// The boxes of the entities and spawned players in `area` of `world` (PMMP
/// `World::getNearbyEntities`: boxes that overlap it by more than [`EPSILON`]). A caller that
/// asks about many parts of an area gathers them once and tests each part against them alone.
pub(super) fn boxes_in(state: &State, world: WorldIndex, area: Aabb) -> Vec<Aabb> {
    let entities = &state.rules.entities;
    let mut boxes = Vec::new();
    entities.each_nearby(world, area, |id| {
        boxes.extend(entities.get(id).map(Entity::bounding_box));
    });
    boxes.extend(
        state
            .players
            .values()
            .filter(|p| p.world == world && p.phase == Phase::Spawned)
            .map(survival::bounding_box)
            .filter(|b| b.overlaps(area, EPSILON)),
    );
    boxes
}

/// Whether an entity or a spawned player is in `area` of `world` ([`boxes_in`]).
pub(super) fn anything_in(state: &State, world: WorldIndex, area: Aabb) -> bool {
    !boxes_in(state, world, area).is_empty()
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
            form: Form::Villager {
                profession, offers, ..
            },
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

/// The offers of the live villager NPC `id`, to change (a trade counted); its chunk is saved
/// again. `None`, marking nothing, for anything else.
pub(super) fn offers_mut(state: &mut State, id: Id) -> Option<&mut super::trade::Offers> {
    villager(state, id)?;
    let entities = &mut state.rules.entities;
    let e = entities.get(id)?;
    let key = (e.world, e.chunk);
    entities.changed.insert(key);
    match &mut entities.get_mut(id)?.kind {
        Kind::Npc(Character {
            form: Form::Villager { offers, .. },
            ..
        }) => Some(offers),
        _ => None,
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
        Shot::Trident(kind::Trident {
            item,
            creative,
            can_collide,
        }) => {
            h.u64(6);
            super::inventory::hash_stack(h, item);
            h.u64(u64::from(*creative));
            h.u64(u64::from(*can_collide));
        }
        Shot::IceBomb => h.u64(7),
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
        h.u64(u64::from(e.duration_ticks()));
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
        data,
        held,
    } = c;
    match form {
        Form::Villager {
            profession,
            offers,
            baby,
        } => {
            h.u64(0);
            h.u64(u64::from(*profession));
            offers.hash_into(h);
            h.u64(u64::from(*baby));
        }
        Form::Zombie { baby } => {
            h.u64(3);
            h.u64(u64::from(*baby));
        }
        Form::Human { uuid, skin } => {
            h.u64(1);
            h.bytes(uuid.as_bytes());
            match skin {
                Some(s) => {
                    h.bytes(s.id().as_bytes());
                    h.bytes(s.data());
                    h.bytes(s.cape());
                    h.bytes(s.geometry_name().as_bytes());
                    h.bytes(s.geometry_data());
                }
                None => h.u64(u64::MAX),
            }
        }
        Form::Custom(kind::Custom { spec }) => {
            h.u64(2);
            h.u64(u64::from(spec.index));
        }
    }
    super::extra::hash_nbt(h, data);
    super::inventory::hash_stack(h, held);
    h.u64(u64::from(*invulnerable));
    h.u64(u64::from(*attack_time));
    h.u64(last_damage.to_bits());
    h.u64(dead_ticks.map_or(0, |t| u64::from(t) + 1));
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

#[cfg(test)]
mod tests {
    mod events;

    use std::cell::Cell;

    use tokio::sync::mpsc;

    use super::physics::tests::named;
    use super::saved::tests::stack;
    use super::*;
    use crate::event::Listeners;
    use crate::protocol::Rng;
    use crate::protocol::consts::MoveFlag;
    use crate::save::Response;
    use crate::tick::queue::{Input, Message, Outbound};
    use crate::tick::tests::{
        answer, join, leave, moved_to, new_state, radius, settings, teleported,
    };
    use crate::tick::{InputLog, Output, boundary, deliver, step};

    thread_local! {
        /// Items the test inventories still take (one player per test takes items).
        static ROOM: Cell<u32> = const { Cell::new(0) };
        /// Items the test inventories took.
        static TAKEN: Cell<u64> = const { Cell::new(0) };
        /// Experience the test players got.
        static EXPERIENCE: Cell<u64> = const { Cell::new(0) };
        /// Entities the tracking pass compared with the players of their chunk.
        static TRACKED: Cell<usize> = const { Cell::new(0) };
        /// The calls the stand-ins saw.
        static CALLS: Cell<Calls> = Cell::new(Calls::default());
    }

    pub(super) const TEST_HOOKS: Hooks = Hooks {
        give,
        room: test_room,
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
            Self::with_listeners(threads, Listeners::default())
        }

        /// [`Harness::new`] with `listeners`.
        pub(super) fn with_listeners(threads: usize, listeners: Listeners) -> Self {
            Self::with_config(threads, listeners, |_| {})
        }

        /// [`Harness::with_listeners`] with the settings `edit` makes.
        pub(super) fn with_config(
            threads: usize,
            listeners: Listeners,
            edit: impl FnOnce(&mut crate::config::Config),
        ) -> Self {
            let mut state = new_state(settings(edit), listeners);
            // The preloaded chunks' extras reach the rules as a load's would.
            let loaded: Vec<ChunkPos> = state.worlds[0].loaded().collect();
            for pos in loaded {
                if let Some(extras) = state.chunks.extras.get((0, pos)).cloned() {
                    let world = &state.worlds[0];
                    let read =
                        state
                            .rules
                            .read((0, pos), &extras, world.chunk(pos).unwrap(), world);
                    state.rules.load((0, pos), read, &extras);
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
                            Message::SharedPacket(p) | Message::SpawnList(p) => {
                                Some(p.get().clone())
                            }
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
                ..crate::protocol::packet::PlayerAuthInput::default()
            };
            let input = crate::tick::tests::input(
                session,
                seq,
                crate::tick::tests::packet(Packet::PlayerAuthInput(Box::new(p))),
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

    pub(super) fn count_tracked() {
        TRACKED.with(|t| t.set(t.get() + 1));
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

    /// What the stand-in inventory takes of `stack`.
    fn test_room(_: &Player, stack: &Stack) -> u8 {
        let room = ROOM.with(Cell::get);
        u8::try_from(room.min(u32::from(stack.count()))).unwrap()
    }

    /// A stand-in pickup: counts the points, refused for players in `Calls::xp_cooldown`.
    fn pickup_experience(_: &mut State, session: SessionId, amount: u32) -> bool {
        if calls(|c| c.xp_cooldown.contains(&session.get())) {
            return false;
        }
        EXPERIENCE.with(|x| x.set(x.get() + u64::from(amount)));
        true
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

    fn damage(
        _: &mut State,
        session: SessionId,
        amount: f64,
        cause: survival::Damage,
        _: DamageSource,
    ) -> bool {
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

    fn set_on_fire(_: &mut State, session: SessionId, seconds: u32, _: CombustSource) {
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

    /// The eye position of a player standing on the flat ground at block (x, z).
    pub(super) fn standing(x: f32, z: f32) -> [f32; 3] {
        [x + 0.5, -58.38, z + 0.5]
    }

    pub(super) fn at(x: f64, y: f64, z: f64) -> Vec3 {
        Vec3::new(x, y, z)
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

    /// `item-despawn-ticks`: a dropped item lives that many ticks, and a cancelled despawn gives
    /// it that many again.
    #[test]
    fn dropped_items_live_for_item_despawn_ticks() {
        let mut h = Harness::with_config(1, Listeners::default(), |c| {
            c.general.item_despawn_ticks = 40;
        });
        h.spawn(1, standing(0.0, 0.0));
        assert!(drop_item(
            &mut h.state,
            0,
            at(6.5, -59.0, 6.5),
            stack("minecraft:stick", 1)
        ));
        let delay = |h: &Harness| {
            h.live()
                .iter()
                .find_map(|e| as_item(e).map(|i| i.despawn_delay))
        };
        assert_eq!(delay(&h), Some(40));
        for _ in 0..39 {
            h.entities();
        }
        assert_eq!(delay(&h), Some(1));
        h.entities();
        assert_eq!(delay(&h), None, "despawned");
    }

    /// `item-pickup-interval`: a player looks for items only every that many ticks, on the
    /// ticks its session number staggers it to.
    #[test]
    fn players_pick_up_items_only_every_item_pickup_interval_ticks() {
        let mut h = Harness::with_config(1, Listeners::default(), |c| {
            c.developer.item_pickup_interval = 5;
        });
        h.spawn(1, standing(0.0, 0.0));
        room(64);
        assert!(drop_item_with(
            &mut h.state,
            0,
            at(0.5, -60.0, 0.5),
            stack("minecraft:stick", 1),
            Some(Vec3::ZERO),
            0,
            String::new()
        ));
        // Session 1 looks on the ticks where tick + 1 is a multiple of 5: start just after one.
        h.state.tick = h.state.tick.next_multiple_of(5);
        let mut picked = Vec::new();
        for _ in 0..10 {
            let before = taken();
            h.entities();
            if taken() > before {
                picked.push(h.state.tick);
            }
            h.state.tick += 1;
        }
        let first = picked.first().copied().expect("picked up");
        assert_eq!(first % 5, 4, "only on the player's ticks: {picked:?}");
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

    /// Above the world height a chunk that is not loaded is a wall too: an item moving there
    /// stays in a loaded chunk, so a save keeps it.
    #[test]
    fn items_above_the_world_never_enter_an_unloaded_chunk() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let edge = (0..64)
            .find(|&x| !h.state.worlds[0].is_chunk_loaded(ChunkPos::new(x, 0)))
            .unwrap();
        let tick_radius = h.state.settings.view(0).tick_radius;
        assert!(edge - 1 <= i32::from(tick_radius), "the item starts ticked");
        let id = crate::tick::Context {
            state: &mut h.state,
        }
        .drop_item(
            world::Id::DEFAULT,
            at(f64::from(edge * 16) - 0.5, 330.0, 0.5),
            stack("minecraft:stick", 1),
            Some(Vec3::new(1.0, 0.0, 0.0)),
        )
        .unwrap();
        for _ in 0..5 {
            h.step(Vec::new());
        }
        let chunk = h.state.rules.entities.get(id).unwrap().chunk;
        assert!(h.state.worlds[0].is_chunk_loaded(chunk), "{chunk:?}");
        h.state.rules.flush(&mut h.state.chunks.extras);
        let item = crate::nbt::Value::String("minecraft:item".to_owned());
        let saved = h.state.worlds[0]
            .loaded()
            .filter_map(|pos| h.state.chunks.extras.get((0, pos)))
            .flat_map(|extras| extras.get(crate::storage::ChunkKind::Entities))
            .filter(|c| c.get("identifier") == Some(&item))
            .count();
        assert_eq!(saved, 1);
    }

    /// An item whose box already reaches into a chunk that is not loaded (dropped at the edge of
    /// the loaded ones) does not move into it, east or south, so a save keeps it.
    #[test]
    fn an_item_overlapping_an_unloaded_chunk_never_enters_it() {
        for (dx, dz) in [(1.0, 0.0), (0.0, 1.0)] {
            let mut h = Harness::new(1);
            h.spawn(1, standing(0.0, 0.0));
            let loaded = |h: &Harness, x: i32| {
                let pos = if dx > 0.0 {
                    ChunkPos::new(x, 0)
                } else {
                    ChunkPos::new(0, x)
                };
                h.state.worlds[0].is_chunk_loaded(pos)
            };
            let edge = (0..64).find(|&x| !loaded(&h, x)).unwrap();
            let border = f64::from(edge * 16) - 0.05;
            let start = if dx > 0.0 {
                at(border, -60.0, 0.5)
            } else {
                at(0.5, -60.0, border)
            };
            let id = crate::tick::Context {
                state: &mut h.state,
            }
            .drop_item(
                world::Id::DEFAULT,
                start,
                stack("minecraft:stick", 1),
                Some(Vec3::new(dx, 0.0, dz)),
            )
            .unwrap();
            for _ in 0..5 {
                h.step(Vec::new());
            }
            let chunk = h.state.rules.entities.get(id).unwrap().chunk;
            assert!(h.state.worlds[0].is_chunk_loaded(chunk), "{chunk:?}");
            h.state.rules.flush(&mut h.state.chunks.extras);
            let item = crate::nbt::Value::String("minecraft:item".to_owned());
            let saved = h.state.worlds[0]
                .loaded()
                .filter_map(|pos| h.state.chunks.extras.get((0, pos)))
                .flat_map(|extras| extras.get(crate::storage::ChunkKind::Entities))
                .filter(|c| c.get("identifier") == Some(&item))
                .count();
            assert_eq!(saved, 1);
        }
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

    /// A player's box is the one of its pose (PMMP `Living::recalculateSize`): sneaking 1.49
    /// high, swimming 0.6, so it picks up less high than standing (1.8).
    #[test]
    fn a_sneaking_or_swimming_player_picks_up_within_its_lower_box() {
        for (height_above_feet, pose) in [(2.1, "sneaking"), (1.2, "swimming")] {
            let mut h = Harness::with_config(1, Listeners::default(), |c| {
                c.developer.item_pickup_interval = 1;
            });
            h.spawn(1, standing(0.0, 0.0));
            room(64);
            assert!(drop_item_with(
                &mut h.state,
                0,
                at(0.5, -60.0 + height_above_feet, 0.5),
                stack("minecraft:stick", 1),
                Some(Vec3::ZERO),
                0,
                String::new()
            ));
            let id = h.state.rules.entities.ids().last().unwrap();
            with(&mut h.state, id, |e| e.gravity = false);
            let player = h.state.players.get_mut(&SessionId::new(1)).unwrap();
            if pose == "sneaking" {
                player.set_sneaking(true);
            } else {
                player.set_swimming(true);
            }
            for _ in 0..3 {
                h.entities();
            }
            assert_eq!(h.ground(), 1, "{pose}: above its box");
            let player = h.state.players.get_mut(&SessionId::new(1)).unwrap();
            player.set_sneaking(false);
            player.set_swimming(false);
            h.entities();
            assert_eq!(h.ground(), 0, "standing: within its box");
        }
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
            10,
            String::new()
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
                10,
                String::new()
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
            10,
            String::new()
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

    /// A burning item in a water cauldron is put out and the cauldron loses a level (PMMP
    /// `WaterCauldron::onEntityInside`).
    #[test]
    fn a_water_cauldron_puts_out_a_burning_item_and_drops_a_level() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(8.0, 8.0));
        let level = |n| {
            named("minecraft:cauldron")
                .with("cauldron_liquid", block::PropertyValue::String("water"))
                .and_then(|s| s.with("fill_level", block::PropertyValue::Int(n)))
                .unwrap()
        };
        let pos = BlockPos::new(0, -60, 0);
        h.state.worlds[0].set_block(pos, level(3)).unwrap();
        let stick = stack("minecraft:stick", 1);
        let spot = at(0.5, -59.9, 0.5);
        assert!(drop_item_with(
            &mut h.state,
            0,
            spot,
            stick,
            Some(Vec3::ZERO),
            10,
            String::new()
        ));
        let id = h.state.rules.entities.ids().next().unwrap();
        with(&mut h.state, id, |e| e.fire_ticks = 100);
        h.entities();
        assert_eq!(h.state.rules.entities.get(id).unwrap().fire_ticks, 0);
        assert_eq!(h.state.worlds[0].block(pos), Some(level(2)));
    }

    /// A burning item lights an unlit campfire it is in (PMMP `Campfire::onEntityInside`).
    #[test]
    fn a_burning_item_lights_an_unlit_campfire() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(8.0, 8.0));
        let lit = |on: bool| {
            named("minecraft:campfire")
                .with("extinguished", block::PropertyValue::Byte(u8::from(!on)))
                .unwrap()
        };
        let pos = BlockPos::new(0, -60, 0);
        h.state.worlds[0].set_block(pos, lit(false)).unwrap();
        let stick = stack("minecraft:stick", 1);
        let spot = at(0.5, -59.9, 0.5);
        assert!(drop_item_with(
            &mut h.state,
            0,
            spot,
            stick,
            Some(Vec3::ZERO),
            10,
            String::new()
        ));
        let id = h.state.rules.entities.ids().next().unwrap();
        with(&mut h.state, id, |e| e.fire_ticks = 100);
        h.entities();
        assert_eq!(h.state.worlds[0].block(pos), Some(lit(true)));
    }

    // pmmp: FallingBlock
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

    // pmmp: ExperienceOrb
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

    /// `experience = false` or `experience-orbs = false`: neither the rules nor the API make
    /// orbs.
    #[test]
    fn without_experience_or_orbs_no_orbs_are_made() {
        let off: [fn(&mut crate::config::Config); 2] = [
            |c| c.general.experience = false,
            |c| c.general.experience_orbs = false,
        ];
        for (i, edit) in off.into_iter().enumerate() {
            let mut h = Harness::with_config(1, Listeners::default(), edit);
            h.spawn(1, standing(0.0, 0.0));
            let next = h.state.next_runtime_id;
            drop_experience(&mut h.state, 0, at(2.5, -59.0, 0.5), 100);
            let mut cx = crate::tick::Context {
                state: &mut h.state,
            };
            let world = crate::world::Id::DEFAULT;
            assert_eq!(
                cx.drop_experience(world, at(2.5, -59.0, 0.5), 100),
                0,
                "{i}"
            );
            assert_eq!(
                cx.spawn_experience_orb(world, at(2.5, -59.0, 0.5), 5),
                None,
                "{i}"
            );
            assert!(h.live().is_empty(), "{i}");
            assert_eq!(h.state.next_runtime_id, next, "no runtime ID is used ({i})");
        }
    }

    pub(super) fn adds(packets: &[Packet]) -> Vec<u64> {
        packets
            .iter()
            .filter_map(|p| match p {
                Packet::AddItemActor(a) => Some(a.entity_runtime_id),
                Packet::AddActor(a) => Some(a.entity_runtime_id),
                Packet::AddPlayer(a) => Some(a.entity_runtime_id),
                Packet::AddPainting(a) => Some(a.entity_runtime_id),
                _ => None,
            })
            .collect()
    }

    /// `experience = false` or `experience-orbs = false`: the orbs of a saved chunk are dropped
    /// as it loads, out of the tick radius too (no runtime ID, never sent), and the chunk is
    /// saved without them; with both they stay.
    #[test]
    fn without_experience_or_orbs_saved_orbs_are_dropped_as_their_chunk_loads() {
        let key = (0, ChunkPos::new(5, 0));
        let list = {
            let mut h = Harness::new(1);
            h.spawn(1, standing(80.0, 0.0));
            drop_experience(&mut h.state, 0, at(80.5, -59.0, 0.5), 3);
            let item = stack("minecraft:stick", 3);
            assert!(drop_item(&mut h.state, 0, at(81.5, -59.0, 1.5), item));
            h.state.rules.entities.unload(key).unwrap()
        };
        let is_orb = |c: &Compound| {
            c.get("identifier") == Some(&crate::nbt::Value::String("minecraft:xp_orb".to_owned()))
        };
        // The stand-in save thread's markers come with them, kept as saved.
        let without: Vec<Compound> = list.iter().filter(|c| !is_orb(c)).cloned().collect();
        assert_eq!(list.len() - without.len(), 1, "one orb: {list:?}");
        for (experience, orbs) in [(false, true), (true, false), (true, true)] {
            let mut h = Harness::with_config(1, Listeners::default(), |c| {
                c.general.experience = experience;
                c.general.experience_orbs = orbs;
                c.developer.tick_radius = 2;
            });
            let (j, rx) = join(1, 0);
            h.receivers.push((1, rx));
            h.step(vec![j, radius(1, 1, 8)]);
            for _ in 0..40 {
                h.step(Vec::new());
            }
            assert!(h.state.players[&SessionId::new(1)].view.sees(key.1));
            h.take(1);
            let entities = &mut h.state.rules.entities;
            entities.unload(key);
            entities.load_list(key, &list, &crate::custom::Registry::default());
            for _ in 0..3 {
                h.step(Vec::new());
            }
            let both = experience && orbs;
            let kept = if both { 2 } else { 1 };
            assert_eq!(h.live().len(), kept, "experience {experience}, orbs {orbs}");
            assert_eq!(
                adds(&h.take(1)).len(),
                kept,
                "experience {experience}, orbs {orbs}"
            );
            let saved = h.state.rules.entities.unload(key);
            if both {
                assert_eq!(saved, None, "nothing changed");
            } else {
                assert_eq!(saved, Some(without.clone()), "saved without the orb");
            }
        }
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
        let name = h.state.players[&s].identity.name.clone();
        assert_eq!(e.thrower(), Some(name.as_str()), "the player threw it");
        assert!(drop_item(
            &mut h.state,
            0,
            at(0.5, -60.0, 0.5),
            stack("minecraft:stick", 1)
        ));
        let dropped: Vec<_> = h
            .live()
            .iter()
            .map(|e| e.thrower().map(str::to_owned))
            .collect();
        assert!(
            dropped.contains(&Some(String::new())),
            "nobody threw a dropped item"
        );
    }

    /// What a full inventory cannot take back as a window closes, or as the player dies, the
    /// player throws (PMMP `doCloseInventory`: a `DropItemAction`, `Player::dropItem`).
    #[test]
    fn items_a_full_inventory_cannot_take_back_are_thrown_by_the_player() {
        for dies in [false, true] {
            let mut h = Harness::new(1);
            h.spawn(1, standing(0.0, 0.0));
            h.look(1, 270.0, 0.0);
            let s = SessionId::new(1);
            let name = h.state.players[&s].identity.name.clone();
            let player = h.state.players.get_mut(&s).unwrap();
            for _ in 0..36 {
                let left = inventory::give(player, stack("minecraft:cobblestone", 64));
                assert!(left.is_empty());
            }
            let held = stack("minecraft:stick", 3);
            assert!(inventory::hold_outside(player, held.clone(), Stack::EMPTY));
            if dies {
                // Dying closes the window first.
                assert!(player.kill());
                h.step(Vec::new());
            } else {
                inventory::close_window(&mut h.state, s);
            }
            let thrown: Vec<_> = h
                .live()
                .iter()
                .filter(|e| matches!(&e.kind, Kind::Item(i) if i.stack == held))
                .map(|e| (e.thrower().map(str::to_owned), e.kind.clone()))
                .collect();
            let [(thrower, Kind::Item(item))] = thrown.as_slice() else {
                panic!("one stick item (dies {dies}): {thrown:?}");
            };
            assert_eq!(thrower.as_deref(), Some(name.as_str()), "dies {dies}");
            assert!(
                item.pickup_delay <= 40 && item.pickup_delay > 30,
                "dies {dies}"
            );
        }
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

    /// An orb measures its distance to a player from the player's feet rounded as PMMP keeps
    /// them (`survival::feet`): a hair under 8 blocks above them it finds the player, which the
    /// `f32` eye less the eye height puts a hair over 8.
    #[test]
    fn an_orb_finds_a_player_from_its_rounded_feet() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        drop_experience(&mut h.state, 0, at(0.5, -52.000_000_5, 0.5), 1);
        let id = h.state.rules.entities.ids().last().unwrap();
        with(&mut h.state, id, |e| {
            e.gravity = false;
            e.body.motion = Vec3::ZERO;
        });
        for _ in 0..=ORB_LOOK_PERIOD {
            h.entities();
        }
        let target = h.state.rules.entities.get(id).and_then(|e| match &e.kind {
            Kind::ExperienceOrb(o) => o.target,
            _ => None,
        });
        assert_eq!(target, Some(SessionId::new(1)));
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
        h.state
            .rules
            .entities
            .load_list(key, &list, &crate::custom::Registry::default());
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

    /// A finite point above the world border is no place for an entity: its height would be
    /// infinite as the `f32` of a packet.
    #[test]
    fn nothing_is_made_above_the_world_border() {
        let mut h = Harness::new(1);
        let next = h.state.next_runtime_id;
        let high = at(0.5, 1e39, 0.5);
        assert!(!drop_item(
            &mut h.state,
            0,
            high,
            stack("minecraft:stick", 1)
        ));
        assert!(h.live().is_empty());
        assert_eq!(h.state.next_runtime_id, next, "no runtime ID is used");
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
            hidden,
            doomed,
            cursor: _,
            restep,
            applying,
            spawned,
            left,
            despawn_heard: _,
            bolts,
            custom: _,
            experience: _,
        } = &h.state.rules.entities;
        assert!(slots.is_empty() && free.is_empty() && counts.is_empty() && bolts.is_empty());
        assert!(columns.is_empty() && runtime_ids.is_empty() && unassigned.is_empty());
        assert!(changed.is_empty() && departed.is_empty() && hidden.is_empty());
        assert!(doomed.is_empty() && restep.is_empty() && !applying);
        assert!(spawned.is_empty() && left.is_empty());
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
            10,
            String::new()
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
        let kept = saved::load(
            saved[0],
            ChunkPos::new(10, 0),
            &EntityTypes::default(),
            &crate::custom::Registry::default(),
        )
        .unwrap();
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
        entities.load_list(
            key,
            std::slice::from_ref(&pig),
            &crate::custom::Registry::default(),
        );
        assert_eq!(entities.unload(key), None, "nothing changed");
        entities.load_list(
            key,
            std::slice::from_ref(&pig),
            &crate::custom::Registry::default(),
        );
        assert!(drop_item(
            &mut h.state,
            0,
            at(1.5, -59.0, 1.5),
            stack("minecraft:stick", 3)
        ));
        let list = h.state.rules.entities.unload(key).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1], pig);
        h.state
            .rules
            .entities
            .load_list(key, &list, &crate::custom::Registry::default());
        assert_eq!(h.live().len(), 1);
        assert_eq!(h.state.rules.entities.take_changed(), Vec::new());
        h.state.rules.entities.changed.insert(key);
        let again = h.state.rules.entities.take_changed();
        assert_eq!(again, vec![(key, list)]);
    }

    /// Asking for the offers of an entity that is no villager marks its chunk for no save.
    #[test]
    fn asking_a_non_villager_for_offers_marks_nothing_to_save() {
        let mut h = Harness::new(1);
        let npc = Npc::Human {
            name: "Guide".to_owned(),
            skin: None,
        };
        let id = spawn_npc(&mut h.state, 0, at(3.5, -60.0, 0.5), npc).unwrap();
        h.state.rules.entities.take_changed();
        assert!(offers_mut(&mut h.state, id).is_none());
        assert_eq!(h.state.rules.entities.take_changed(), Vec::new());
    }

    fn moves(packets: &[Packet], runtime_id: u64) -> usize {
        packets
            .iter()
            .filter(|p| match p {
                Packet::MoveActorDelta(m) => m.entity_runtime_id == runtime_id,
                Packet::MoveActorAbsolute(m) => m.entity_runtime_id == runtime_id,
                Packet::MovePlayer(m) => m.entity_runtime_id == runtime_id,
                _ => false,
            })
            .count()
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
        let shown = |p: Vec3| [p.x, p.y + 0.125, p.z].map(to_f32);
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
                10,
                String::new()
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

    /// A viewer whose bound this tick's entity spawns used up is shown a new player in the next
    /// tick, so a crowd and a chunk full of items together cannot overflow its outbox.
    #[test]
    fn a_player_over_the_viewers_bound_is_shown_in_a_later_tick() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.spawn(2, standing(3.0, 3.0));
        let two = h.runtime_id(2);
        let pair = (SessionId::new(1), SessionId::new(2));
        h.step(Vec::new());
        assert!(adds(&h.take(1)).contains(&two));
        // Player 1 stops seeing player 2.
        h.state.rules.entities.hidden.insert(pair);
        h.entities();
        assert!(removes(&h.take(1)).contains(&two));
        for i in 0..MAX_SPAWNS_PER_PLAYER {
            let x = f64::from(i % 32) * 0.25 + 4.0;
            let z = f64::from(i / 32) * 0.25 + 4.0;
            let sword = stack("minecraft:diamond_sword", 1);
            assert!(drop_item_with(
                &mut h.state,
                0,
                at(x, -60.0, z),
                sword,
                Some(Vec3::ZERO),
                10,
                String::new()
            ));
        }
        h.state.rules.entities.hidden.remove(&pair);
        h.entities();
        let sent = adds(&h.take(1));
        assert_eq!(sent.len(), usize::try_from(MAX_SPAWNS_PER_PLAYER).unwrap());
        assert!(!sent.contains(&two), "the items used the bound up");
        h.entities();
        assert!(adds(&h.take(1)).contains(&two), "shown in the next tick");
    }

    /// An entity that arrives in a chunk is compared with the chunk's players alone: it is
    /// shown, and its resting neighbours are not looked at again.
    #[test]
    fn an_arrival_is_shown_and_its_resting_neighbours_are_not_looked_at_again() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(12.0, 12.0));
        h.spawn(2, standing(12.0, 13.0));
        let drop_sword = |h: &mut Harness, x: f64, z: f64| {
            let sword = stack("minecraft:diamond_sword", 1);
            let pos = at(x, -60.0, z);
            assert!(drop_item_with(
                &mut h.state,
                0,
                pos,
                sword,
                Some(Vec3::ZERO),
                10,
                String::new()
            ));
        };
        for i in 0..100u32 {
            drop_sword(
                &mut h,
                f64::from(i % 10) * 0.5 + 0.5,
                f64::from(i / 10) * 0.5 + 0.5,
            );
        }
        h.settle();
        let items = |packets: Vec<Packet>| {
            let added = packets
                .iter()
                .filter(|p| matches!(p, Packet::AddItemActor(_)));
            added.count()
        };
        for s in [1, 2] {
            assert_eq!(items(h.take(s)), 100);
        }
        TRACKED.with(Cell::take);
        drop_sword(&mut h, 6.5, 6.5);
        h.step(Vec::new());
        for s in [1, 2] {
            assert_eq!(items(h.take(s)), 1);
        }
        assert_eq!(TRACKED.with(Cell::take), 1);
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
            skin: None,
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
        assert_eq!((who, cause), (1, survival::Damage::FallingBlock));
        // It fell about 10 blocks onto the player's head: (round(fall) − 1) × 2.
        assert!(
            (14.0..=26.0).contains(&amount) && amount % 2.0 == 0.0,
            "{amount}"
        );
    }

    /// A falling anvil that lands on a zombie made after it knocks the zombie back, and the
    /// zombie moves on from the knockback, not by the step computed before the anvil landed.
    #[test]
    fn a_knockback_a_rule_gives_a_later_entity_is_not_undone_by_its_step() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(8.0, 8.0));
        let anvil = named("minecraft:anvil");
        let pos = BlockPos::new(0, -50, 0);
        h.state.worlds[0].set_block(pos, anvil).unwrap();
        assert!(fall(&mut h.state, 0, pos, anvil));
        // Beside the anvil's cell, its box within the anvil's.
        let beside = at(1.2, -60.0, 0.5);
        let zombie = spawn_npc(&mut h.state, 0, beside, Npc::Zombie).unwrap();
        let health = |h: &Harness| h.state.rules.entities.get(zombie).unwrap().health;
        let full = health(&h);
        for _ in 0..60 {
            h.entities();
            if health(&h) < full {
                break;
            }
        }
        assert!(health(&h) < full, "the anvil landed on the zombie");
        let motion = h.state.rules.entities.get(zombie).unwrap().body.motion;
        assert!(
            motion.x > 0.0,
            "knocked back away from the anvil: {motion:?}"
        );
    }

    // pmmp: Entity, ExperienceOrb, FallingBlock, AreaEffectCloud, Villager, Human
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

    /// A pushed player's motion reaches it and the players it is shown to, not one that hides it.
    #[test]
    fn a_pushed_players_motion_is_not_sent_to_a_player_who_hides_it() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.spawn(2, standing(3.0, 3.0));
        h.spawn(3, standing(-3.0, 3.0));
        let one = h.runtime_id(1);
        h.state
            .rules
            .entities
            .hidden
            .insert((SessionId::new(2), SessionId::new(1)));
        h.entities();
        let force = (KNOCKBACK, KNOCKBACK_VERTICAL_LIMIT);
        knock_player(
            &mut h.state,
            SessionId::new(1),
            (1.0, 0.0),
            force,
            Raise::Quiet,
        );
        let motions = |n: u64| {
            h.state.players[&SessionId::new(n)]
                .outbox
                .iter()
                .filter(|p| matches!(p, Packet::SetActorMotion(m) if m.entity_runtime_id == one))
                .count()
        };
        assert_eq!((motions(1), motions(2), motions(3)), (1, 0, 1));
    }

    /// Another player's walk and look reach those who see it as `MoveActorAbsolute` at its eyes
    /// with the ground flag and no teleport flag, which the client interpolates (PMMP
    /// `broadcastMovement`). A `MovePlayer` puts it there at once: 20 jumps a second.
    #[test]
    fn other_players_walk_and_turn_by_interpolated_moves() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.spawn(2, standing(3.0, 3.0));
        let one = h.runtime_id(1);
        h.step(Vec::new());
        h.take(2);
        let seen = |h: &mut Harness| {
            let sent = h.take(2);
            assert!(
                !sent
                    .iter()
                    .any(|p| matches!(p, Packet::MovePlayer(m) if m.entity_runtime_id == one)),
                "no MovePlayer for another player's move"
            );
            let moves: Vec<_> = sent
                .into_iter()
                .filter_map(|p| match p {
                    Packet::MoveActorAbsolute(m) if m.entity_runtime_id == one => Some(m),
                    _ => None,
                })
                .collect();
            let [m] = moves.as_slice() else {
                panic!("one move a tick: {moves:?}");
            };
            m.clone()
        };
        h.go(1, standing(1.0, 0.0));
        h.step(Vec::new());
        let walk = seen(&mut h);
        assert_eq!(walk.position, standing(1.0, 0.0), "at the eyes");
        assert_eq!(walk.flags, u8::from(MoveFlag::OnGround));
        h.look(1, 90.0, 30.0);
        h.step(Vec::new());
        let turn = seen(&mut h);
        assert_eq!(turn.position, walk.position);
        assert_eq!(turn.rotation, [30.0, 90.0, 90.0], "pitch, yaw, head yaw");
        assert_eq!(turn.flags, u8::from(MoveFlag::OnGround));
    }

    /// An entity made at rest is shown still from the start (`NoAi`), so that a client does not
    /// move it on its own (pushing a human NPC a player walks into) where the server never does.
    #[test]
    fn an_entity_made_at_rest_is_shown_still_from_the_start() {
        use crate::protocol::consts::{EntityDataFlag, EntityDataKey};
        use crate::protocol::{FlagSet, MetadataValue};
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let npc = Npc::Human {
            name: "Guide".to_owned(),
            skin: None,
        };
        let id = spawn_npc(&mut h.state, 0, at(3.5, -60.0, 0.5), npc).unwrap();
        h.step(Vec::new());
        let runtime_id = h.state.rules.entities.get(id).unwrap().runtime_id;
        let sent = h.take(1);
        let Some(add) = sent.iter().find_map(|p| match p {
            Packet::AddPlayer(a) if a.entity_runtime_id == runtime_id => Some(a),
            _ => None,
        }) else {
            panic!("no AddPlayer: {sent:?}");
        };
        let Some(MetadataValue::Int64(flags)) =
            add.entity_metadata.get(&(EntityDataKey::Flags as u32))
        else {
            panic!("no flags: {:?}", add.entity_metadata);
        };
        let shown = FlagSet::<EntityDataFlag>::from_words(&[flags.cast_unsigned()]).unwrap();
        assert!(shown.contains(EntityDataFlag::NoAi));
    }

    /// A human NPC, which clients know as a player, turns as a player does: `MoveActorAbsolute`
    /// at its eyes with its whole position, never a `MoveActorDelta` that leaves axes out.
    #[test]
    fn a_human_npc_turns_by_a_players_move() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let npc = Npc::Human {
            name: "Guide".to_owned(),
            skin: None,
        };
        let id = spawn_npc(&mut h.state, 0, at(3.5, -60.0, 0.5), npc).unwrap();
        for _ in 0..4 {
            h.step(Vec::new());
        }
        h.take(1);
        with(&mut h.state, id, |e| e.yaw = 90.0);
        h.step(Vec::new());
        let runtime_id = h.state.rules.entities.get(id).unwrap().runtime_id;
        let sent = h.take(1);
        assert!(
            !sent.iter().any(
                |p| matches!(p, Packet::MoveActorDelta(m) if m.entity_runtime_id == runtime_id)
            ),
            "no MoveActorDelta for a human NPC"
        );
        let moves: Vec<_> = sent
            .into_iter()
            .filter_map(|p| match p {
                Packet::MoveActorAbsolute(m) if m.entity_runtime_id == runtime_id => Some(m),
                _ => None,
            })
            .collect();
        let [turn] = moves.as_slice() else {
            panic!("one move: {moves:?}");
        };
        assert_eq!(
            turn.position,
            [3.5, to_f32(-60.0 + 1.621), 0.5],
            "at the eyes"
        );
        assert_eq!(turn.rotation, [0.0, 90.0, 90.0], "pitch, yaw, head yaw");
        assert_eq!(turn.flags, u8::from(MoveFlag::OnGround));
    }

    /// Another player is shown as BDS shows it (P6 `others`): its own whole entity data (its
    /// Breathing flag now) with BDS's seven additions, its device ID and build platform.
    #[test]
    fn players_see_each_others_bds_entity_data() {
        let mut h = Harness::new(1);
        // Without drowning a player breathes from the start.
        h.state.settings.drowning = false;
        h.spawn(1, standing(0.0, 0.0));
        if let Some(p) = h.state.players.get_mut(&SessionId::new(1)) {
            p.identity.device_os = 7;
            p.identity.client.device_id = "device-1".to_owned();
        }
        h.spawn(2, standing(3.0, 3.0));
        let one = h.runtime_id(1);
        h.step(Vec::new());
        let sent = h.take(2);
        let Some(add) = sent.iter().find_map(|p| match p {
            Packet::AddPlayer(a) if a.entity_runtime_id == one => Some(a),
            _ => None,
        }) else {
            panic!("no AddPlayer");
        };
        let mut keys: Vec<u32> = add.entity_metadata.keys().copied().collect();
        keys.sort_unstable();
        assert_eq!(keys.len(), 80, "{keys:?}");
        for (key, value) in [
            (10, crate::protocol::MetadataValue::Byte(0)),
            (38, crate::protocol::MetadataValue::Float32(1.0)),
            (53, crate::protocol::MetadataValue::Float32(0.6)),
            (54, crate::protocol::MetadataValue::Float32(1.8)),
            (56, crate::protocol::MetadataValue::Vec3([0.0; 3])),
            (74, crate::protocol::MetadataValue::Byte(0)),
            (139, crate::protocol::MetadataValue::Int64(0)),
        ] {
            assert_eq!(add.entity_metadata.get(&key), Some(&value), "key {key}");
        }
        let Some(crate::protocol::MetadataValue::Int64(flags)) = add.entity_metadata.get(&0) else {
            panic!("no flags");
        };
        assert_ne!(flags & (1 << 35), 0, "Breathing");
        assert_eq!(add.build_platform, 7);
        assert_eq!(add.device_id, "device-1");
    }

    /// A held item is sent again only when it changes: the copy of last tick shares its data,
    /// which is the same even when `==` finds it unequal to itself (a NaN float).
    #[test]
    fn an_unchanged_held_item_is_not_sent_again() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.spawn(2, standing(3.0, 3.0));
        let extra = crate::item::Extra {
            nbt: crate::nbt::Compound::from([("x".to_owned(), crate::nbt::Value::Float(f32::NAN))]),
            ..crate::item::Extra::default()
        };
        let stick = crate::item::Stack::new(crate::item::Kind::Stick.default_type(), 1)
            .with_extra(Some(extra))
            .unwrap();
        let p = h.state.players.get_mut(&SessionId::new(1)).unwrap();
        let slot = p.inventory.held_slot();
        inventory::set_main(p, slot, stick);
        h.step(Vec::new());
        h.take(2);
        h.step(Vec::new());
        let one = h.runtime_id(1);
        let again = h
            .take(2)
            .iter()
            .any(|p| matches!(p, Packet::MobEquipment(m) if m.entity_runtime_id == one));
        assert!(!again);
    }

    /// Players see each other on the locator bar as BDS shows them (P6 `others`): with the player,
    /// its location and a waypoint (added, then given to the client to follow); when it goes, the
    /// waypoint goes.
    #[test]
    fn players_see_each_other_on_the_locator_bar() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.spawn(2, standing(3.0, 3.0));
        let one = h.runtime_id(1);
        h.step(Vec::new());
        let sent = h.take(2);
        let location = sent.iter().find_map(|p| match p {
            Packet::PlayerLocation(l) => Some(l),
            _ => None,
        });
        assert_eq!(
            location.map(|l| (l.r#type, l.entity_unique_id)),
            Some((0, one.cast_signed()))
        );
        let bars: Vec<(u8, u32)> = sent
            .iter()
            .filter_map(|p| match p {
                Packet::LocatorBar(b) => Some(
                    b.waypoints
                        .iter()
                        .map(|w| (w.action, w.waypoint.update_flag)),
                ),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(bars, [(1, 63), (3, 44)]);
        h.step(vec![leave(1, 99)]);
        h.step(Vec::new());
        let gone: Vec<(u8, u32)> = h
            .take(2)
            .iter()
            .filter_map(|p| match p {
                Packet::LocatorBar(b) => Some(
                    b.waypoints
                        .iter()
                        .map(|w| (w.action, w.waypoint.update_flag)),
                ),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(gone, [(2, 0)]);
    }

    /// `locator-bar = false`: players are shown to each other and removed with no locator packet.
    #[test]
    fn without_the_locator_bar_players_see_each_other_without_it() {
        let mut h = Harness::with_config(1, Listeners::default(), |c| {
            c.general.locator_bar = false;
        });
        h.spawn(1, standing(0.0, 0.0));
        h.spawn(2, standing(3.0, 3.0));
        h.step(Vec::new());
        let mut sent = h.take(2);
        assert!(sent.iter().any(|p| matches!(p, Packet::AddPlayer(_))));
        h.step(vec![leave(1, 99)]);
        h.step(Vec::new());
        sent.extend(h.take(2));
        assert!(sent.iter().any(|p| matches!(p, Packet::RemoveActor(_))));
        assert!(
            !sent
                .iter()
                .any(|p| matches!(p, Packet::LocatorBar(_) | Packet::PlayerLocation(_))),
            "{sent:?}"
        );
    }

    /// Measures the entity tick with one player who sees every item: `cargo test --release --lib
    /// tick::entity::tests::tick_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement"]
    #[expect(
        clippy::print_stderr,
        clippy::disallowed_methods,
        reason = "consistency: a measurement prints its result and reads the wall clock"
    )]
    fn tick_cost() {
        let most = crate::runtime::logical_cpus().max(4);
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
                        10,
                        String::new()
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
                    // The phases of `tick_with`, timed one by one.
                    state.rules.entities.despawn_heard = dispatch::heard::<EntityDespawn>(state);
                    assign_runtime_ids(state);
                    announce(state);
                    let steps = compute(state, &h.pool);
                    mark(0, &mut lap);
                    apply_steps(state, steps, TEST_HOOKS);
                    mark(1, &mut lap);
                    pick_up(state, TEST_HOOKS);
                    announce(state);
                    remove_flagged(state);
                    lightning::remove_due(state);
                    mark(2, &mut lap);
                    let mut budget = spawn_budget(state);
                    track_entities(state, &mut budget);
                    mark(3, &mut lap);
                    track_players(state, &mut budget);
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
