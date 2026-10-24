//! The public entity API (PMMP `Entity`, `Living`, `object\*` (with `Painting`, `EndCrystal`,
//! `FireworkRocket`), `projectile\*` (with `Trident`, `IceBomb`), `Human`, `Villager`,
//! `World::dropItem`, `World::getNearbyEntities`...): what an [`Entity`] shows, what an [`Mut`]
//! changes, and the [`Context`] methods that find, lend and spawn entities.
//!
//! Setters change the entity at once, mark its chunk for the next save and, for what viewers
//! draw (name tag, scale, kind values), mark it for the next movement pass, which sends it with
//! the position, motion, rotation and flags it compares anyway. No setter raises an event
//! (direct calls do not re-enter).
//!
//! Lookups go through the store's slots and indexes: an [`Id`] or a runtime ID is O(1)
//! (O(log n)), "what is near" visits the chunk index, and the entities of a world are the store's
//! slots in `Id` order.

use std::collections::BTreeMap;
use std::ops::Deref;

use super::kind::{
    Arrow, Character, Cloud, Falling, Form, Item, Kind, MAX_CLOUD_EFFECTS, MAX_DESPAWN_DELAY,
    MAX_FUSE, MAX_NAME_TAG_BYTES, NEVER, Orb, Pickup, Projectile, Shot, TRIDENT_DAMAGE, Tnt,
    Trident,
};
use super::saved::{HEIGHT_MARGIN, MAX_CLOUD_AGE, MAX_CLOUD_RADIUS, MAX_PROJECTILE_DAMAGE};
use super::{
    Cause, EYE_HEIGHT, Entities, Entity, Hurt, Knock, MAX_ENTITIES_PER_WORLD,
    MAX_HIDDEN_PER_PLAYER, Npc, Raise, SessionId, State, actor_event, chunk_of, create, drop_stack,
    hurt_in, is_valid_position, is_valid_scale, object, physics, projectile, send_to, to_f32,
};
use crate::action;
use crate::block::{self, Face};
use crate::effect::{Instance, Potion};
use crate::entity::{Animation, ArrowPickup, Id, PaintingMotive, Skin, Type};
use crate::event::Actor;
use crate::item::Stack;
use crate::math::{self, Aabb, Vec3};
use crate::nbt::Compound;
use crate::player;
use crate::protocol::consts::ActorEvent;
use crate::tick::world_api::index;
use crate::tick::{Context, Phase, Player};
use crate::world::{self, BlockPos, ChunkPos, MAX_Y, MIN_Y, World};

impl Entity {
    /// The runtime ID clients know it by (PMMP `getId`); `None` for an entity whose chunk loaded
    /// this tick, which gets one at the next entity tick.
    #[doc(alias = "Entity::getId")]
    pub fn runtime_id(&self) -> Option<u64> {
        (self.runtime_id != 0).then_some(self.runtime_id)
    }

    /// What it is.
    pub fn entity_type(&self) -> Type {
        self.kind.entity_type()
    }

    /// The Bedrock actor type clients draw, such as `minecraft:item`.
    #[doc(alias = "Entity::getNetworkTypeId")]
    #[doc(alias = "Zombie::getNetworkTypeId")]
    pub fn network_type(&self) -> &str {
        self.kind.identifier()
    }

    /// The world it is in.
    #[doc(alias = "Entity::getWorld")]
    #[doc(alias = "Position::getWorld")]
    pub fn world(&self) -> world::Id {
        world::Id::of(self.world)
    }

    /// Where it is: the centre of the bottom of its box.
    #[doc(alias = "Entity::getPosition")]
    #[doc(alias = "Entity::getLocation")]
    pub fn position(&self) -> Vec3 {
        self.body.position
    }

    /// Its yaw in degrees.
    #[doc(alias = "Location::getYaw")]
    pub fn yaw(&self) -> f64 {
        self.yaw
    }

    /// Its pitch in degrees (negative is up).
    #[doc(alias = "Location::getPitch")]
    pub fn pitch(&self) -> f64 {
        self.pitch
    }

    /// Its motion, in blocks per tick.
    #[doc(alias = "Entity::getMotion")]
    pub fn motion(&self) -> Vec3 {
        self.body.motion
    }

    /// The height of its eyes above its position (PMMP `EntitySizeInfo`: a human NPC's is a
    /// player's, else half its height plus 0.1, at most its height), times its scale.
    #[doc(alias = "Entity::getEyeHeight")]
    pub fn eye_height(&self) -> f64 {
        if let Kind::Npc(Character {
            form: Form::Human { .. },
            ..
        }) = self.kind
        {
            return EYE_HEIGHT * self.scale;
        }
        let (_, height) = self.kind.size();
        math::min(height / 2.0 + 0.1, height) * self.scale
    }

    /// Where its eyes are.
    #[doc(alias = "Entity::getEyePos")]
    pub fn eye_position(&self) -> Vec3 {
        self.body.position + Vec3::new(0.0, self.eye_height(), 0.0)
    }

    /// The unit vector it looks along.
    #[doc(alias = "Entity::getDirectionVector")]
    pub fn direction(&self) -> Vec3 {
        super::direction(self.yaw, self.pitch)
    }

    /// Its health.
    #[doc(alias = "Entity::getHealth")]
    pub fn health(&self) -> f64 {
        self.health
    }

    /// The health it starts with (PMMP's per kind, `Kind::max_health`; a custom type's own).
    #[doc(alias = "Entity::getMaxHealth")]
    #[doc(alias = "Living::getMaxHealth")]
    pub fn max_health(&self) -> f64 {
        self.kind.max_health()
    }

    /// Whether it lives: health above 0 and, for an NPC, not dying.
    #[doc(alias = "Entity::isAlive")]
    pub fn is_alive(&self) -> bool {
        self.health > 0.0 && !matches!(&self.kind, Kind::Npc(c) if c.dead_ticks.is_some())
    }

    /// Ticks it keeps burning.
    #[doc(alias = "Entity::getFireTicks")]
    pub fn fire_ticks(&self) -> u16 {
        self.fire_ticks.cast_unsigned()
    }

    /// Whether it burns.
    #[doc(alias = "Entity::isOnFire")]
    pub fn is_on_fire(&self) -> bool {
        self.fire_ticks > 0
    }

    /// Whether its last move ended on a block.
    #[doc(alias = "Entity::isOnGround")]
    pub fn is_on_ground(&self) -> bool {
        self.body.on_ground
    }

    /// Blocks fallen since it last stood on something.
    #[doc(alias = "Entity::getFallDistance")]
    pub fn fall_distance(&self) -> f64 {
        self.body.fall_distance
    }

    /// Whether fire and lava leave it alone (netherite items, clouds).
    #[doc(alias = "Entity::isFireProof")]
    pub fn is_fire_proof(&self) -> bool {
        self.kind.is_fire_proof()
    }

    /// Its name tag; a human NPC's is its name. Empty without one.
    #[doc(alias = "Entity::getNameTag")]
    pub fn name_tag(&self) -> &str {
        &self.name_tag
    }

    /// Whether the name tag shows when a player looks at it.
    #[doc(alias = "Entity::isNameTagVisible")]
    pub fn is_name_tag_visible(&self) -> bool {
        self.name_tag_visible
    }

    /// Whether the name tag always shows.
    #[doc(alias = "Entity::isNameTagAlwaysVisible")]
    pub fn is_name_tag_always_visible(&self) -> bool {
        self.name_tag_always_visible
    }

    /// The size multiplier of its box and model (1 normally).
    #[doc(alias = "Entity::getScale")]
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// Whether clients do not draw it.
    #[doc(alias = "Entity::isInvisible")]
    pub fn is_invisible(&self) -> bool {
        self.invisible
    }

    /// Whether gravity pulls it.
    #[doc(alias = "Entity::hasGravity")]
    pub fn has_gravity(&self) -> bool {
        self.gravity
    }

    /// Whether it is saved with its chunk; one that is not goes when its chunk unloads.
    #[doc(alias = "Entity::canSaveWithChunk")]
    pub fn can_save_with_chunk(&self) -> bool {
        self.saved
    }

    /// Whether it blocks placing a block in its box (falling blocks, primed TNT, living NPCs, end
    /// crystals).
    #[doc(alias = "Entity::canBeCollidedWith")]
    pub fn can_be_collided_with(&self) -> bool {
        self.blocks_placing()
    }

    /// Whether only the void hurts it (NPCs spawn so, custom entities do not; other kinds are
    /// never).
    pub fn is_invulnerable(&self) -> bool {
        matches!(&self.kind, Kind::Npc(c) if c.invulnerable)
    }

    /// The players it was sent to, in `Id` order.
    #[doc(alias = "Entity::getViewers")]
    pub fn viewers(&self) -> impl Iterator<Item = player::Id> + '_ {
        self.viewers.iter().map(|&s| player::Id::new(s))
    }

    /// A dropped item's stack, or the trident item a thrown trident holds (what a player who
    /// picks it up gets).
    #[doc(alias = "ItemEntity::getItem")]
    #[doc(alias = "entity\\projectile\\Trident::getItem")]
    pub fn item(&self) -> Option<&Stack> {
        match &self.kind {
            Kind::Item(item) => Some(&item.stack),
            Kind::Projectile(Projectile {
                shot: Shot::Trident(t),
                ..
            }) => Some(&t.item),
            _ => None,
        }
    }

    /// A dropped item's ticks until a player may pick it up; -1 never.
    #[doc(alias = "ItemEntity::getPickupDelay")]
    pub fn pickup_delay_ticks(&self) -> Option<i16> {
        match &self.kind {
            Kind::Item(item) => Some(item.pickup_delay),
            _ => None,
        }
    }

    /// A dropped item's or orb's ticks until it disappears; -1 never.
    #[doc(alias = "ItemEntity::getDespawnDelay")]
    #[doc(alias = "ExperienceOrb::getDespawnDelay")]
    pub fn despawn_delay_ticks(&self) -> Option<i32> {
        match &self.kind {
            Kind::Item(item) => Some(item.despawn_delay),
            Kind::ExperienceOrb(orb) => Some(orb.despawn_delay),
            _ => None,
        }
    }

    /// A dropped item's owner (a player name, saved; empty without one).
    #[doc(alias = "ItemEntity::getOwner")]
    pub fn owner(&self) -> Option<&str> {
        match &self.kind {
            Kind::Item(item) => Some(&item.owner),
            _ => None,
        }
    }

    /// Who threw a dropped item (a player name, saved; empty without one).
    #[doc(alias = "ItemEntity::getThrower")]
    pub fn thrower(&self) -> Option<&str> {
        match &self.kind {
            Kind::Item(item) => Some(&item.thrower),
            _ => None,
        }
    }

    /// An orb's experience points.
    #[doc(alias = "ExperienceOrb::getXpValue")]
    pub fn xp_value(&self) -> Option<u16> {
        match &self.kind {
            Kind::ExperienceOrb(orb) => Some(orb.value),
            _ => None,
        }
    }

    /// The player an orb follows.
    #[doc(alias = "ExperienceOrb::getTargetPlayer")]
    #[doc(alias = "ExperienceOrb::hasTargetPlayer")]
    pub fn target_player(&self) -> Option<player::Id> {
        match &self.kind {
            Kind::ExperienceOrb(orb) => orb.target.map(player::Id::new),
            _ => None,
        }
    }

    /// The item a player who picks it (middle click) gets: an end crystal, the block of a falling
    /// block or primed TNT, a painting, or the spawn egg of a villager or a zombie; `None` for
    /// the other kinds.
    #[doc(alias = "Entity::getPickedItem")]
    #[doc(alias = "entity\\object\\EndCrystal::getPickedItem")]
    #[doc(alias = "FallingBlock::getPickedItem")]
    #[doc(alias = "Painting::getPickedItem")]
    #[doc(alias = "PrimedTNT::getPickedItem")]
    #[doc(alias = "Villager::getPickedItem")]
    #[doc(alias = "Zombie::getPickedItem")]
    pub fn picked_item(&self) -> Option<Stack> {
        super::pick::picked_item(&self.kind)
    }

    /// The block a falling block is.
    #[doc(alias = "FallingBlock::getBlock")]
    pub fn falling_block(&self) -> Option<block::State> {
        match &self.kind {
            Kind::FallingBlock(f) => Some(f.block),
            _ => None,
        }
    }

    /// Primed TNT's ticks until it explodes.
    #[doc(alias = "PrimedTNT::getFuse")]
    pub fn fuse_ticks(&self) -> Option<u16> {
        match &self.kind {
            Kind::Tnt(t) => Some(t.fuse),
            _ => None,
        }
    }

    /// Whether it is underwater TNT.
    #[doc(alias = "PrimedTNT::worksUnderwater")]
    pub fn works_underwater(&self) -> bool {
        matches!(&self.kind, Kind::Tnt(t) if t.underwater)
    }

    /// A projectile's base damage.
    #[doc(alias = "Projectile::getBaseDamage")]
    pub fn base_damage(&self) -> Option<f64> {
        match &self.kind {
            Kind::Projectile(p) => Some(p.damage),
            _ => None,
        }
    }

    /// The player who shot or threw a projectile or launched a firework rocket, while it is in
    /// the server.
    #[doc(alias = "Entity::getOwningEntity")]
    #[doc(alias = "Entity::getOwningEntityId")]
    pub fn owning_player(&self) -> Option<player::Id> {
        match &self.kind {
            Kind::Projectile(p) => p.owner.map(player::Id::new),
            Kind::Firework(f) => f.owner.map(player::Id::new),
            _ => None,
        }
    }

    /// Whether it is a critical arrow (more damage, a trail).
    #[doc(alias = "entity\\projectile\\Arrow::isCritical")]
    pub fn is_critical(&self) -> bool {
        matches!(arrow(&self.kind), Some(a) if a.critical)
    }

    /// Who may pick up an arrow once stuck.
    #[doc(alias = "entity\\projectile\\Arrow::getPickupMode")]
    pub fn pickup_mode(&self) -> Option<ArrowPickup> {
        arrow(&self.kind).map(|a| a.pickup)
    }

    /// An arrow's Punch level.
    #[doc(alias = "entity\\projectile\\Arrow::getPunchKnockback")]
    pub fn punch_knockback(&self) -> Option<u8> {
        arrow(&self.kind).map(|a| a.punch)
    }

    /// A thrown potion's type.
    #[doc(alias = "entity\\projectile\\SplashPotion::getPotionType")]
    pub fn potion(&self) -> Option<Potion> {
        match &self.kind {
            Kind::Projectile(Projectile {
                shot: Shot::Potion { potion, .. },
                ..
            }) => Some(*potion),
            _ => None,
        }
    }

    /// Whether a thrown potion leaves a cloud.
    #[doc(alias = "entity\\projectile\\SplashPotion::willLinger")]
    pub fn will_linger(&self) -> bool {
        matches!(
            &self.kind,
            Kind::Projectile(Projectile {
                shot: Shot::Potion { linger: true, .. },
                ..
            })
        )
    }

    /// A cloud's radius, in blocks.
    #[doc(alias = "AreaEffectCloud::getRadius")]
    pub fn cloud_radius(&self) -> Option<f64> {
        match &self.kind {
            Kind::Cloud(c) => Some(c.radius),
            _ => None,
        }
    }

    /// A villager NPC's profession (the actor variant, such as 1 for a farmer).
    #[doc(alias = "Villager::getProfession")]
    pub fn profession(&self) -> Option<u8> {
        match &self.kind {
            Kind::Npc(Character {
                form: Form::Villager { profession, .. },
                ..
            }) => Some(*profession),
            _ => None,
        }
    }

    /// A human NPC's skin; `None` for the default skin and for other kinds.
    pub fn skin(&self) -> Option<&Skin> {
        match &self.kind {
            Kind::Npc(Character {
                form: Form::Human { skin, .. },
                ..
            }) => skin.as_deref(),
            _ => None,
        }
    }

    /// The name of an NPC (PMMP `Living::getName`): a human NPC's name tag, `Villager` for a
    /// villager, `Zombie` for a zombie, a custom entity's type. `None` for kinds that do not
    /// live.
    #[doc(alias = "Villager::getName")]
    #[doc(alias = "Zombie::getName")]
    pub fn name(&self) -> Option<&str> {
        match &self.kind {
            Kind::Npc(Character { form, .. }) => Some(match form {
                Form::Human { .. } => &self.name_tag,
                Form::Villager { .. } => "Villager",
                Form::Zombie { .. } => "Zombie",
                Form::Custom(c) => &c.spec.identifier,
            }),
            _ => None,
        }
    }

    /// Whether it is a baby: a villager or a zombie made one with [`Mut::set_baby`]; false for
    /// every other kind.
    #[doc(alias = "entity\\Ageable")]
    #[doc(alias = "entity\\Ageable::isBaby")]
    #[doc(alias = "Villager::isBaby")]
    pub fn is_baby(&self) -> bool {
        self.kind.is_baby()
    }

    /// The downward acceleration its kind falls with, in blocks per tick² (PMMP's per kind:
    /// 0.04 for items, 0.05 for arrows, 0.03 for thrown items...), whether or not gravity pulls
    /// it now ([`Self::has_gravity`]).
    #[doc(alias = "Entity::getGravity")]
    pub fn gravity(&self) -> f64 {
        self.kind.physics().gravity
    }

    /// How high it steps up blocks while moving (0 for every kind Anchor has, as in PMMP).
    #[doc(alias = "Entity::getStepHeight")]
    #[expect(
        clippy::unused_self,
        reason = "consistency: an entity's value, read like its other physics getters"
    )]
    pub fn step_height(&self) -> f64 {
        STEP_HEIGHT
    }

    /// A thrown potion's effects (PMMP: the potion type's, none for water).
    #[doc(alias = "entity\\projectile\\SplashPotion::getPotionEffects")]
    pub fn potion_effects(&self) -> Option<&'static [Instance]> {
        self.potion().map(Potion::effects)
    }

    /// A painting's picture.
    #[doc(alias = "Painting::getMotive")]
    pub fn painting_motive(&self) -> Option<PaintingMotive> {
        match &self.kind {
            Kind::Painting(p) => Some(p.motive),
            _ => None,
        }
    }

    /// The side of its wall a painting hangs on: north, south, west or east (PMMP `getFacing`:
    /// the face of the wall block it covers).
    #[doc(alias = "Painting::getFacing")]
    pub fn painting_facing(&self) -> Option<Face> {
        match &self.kind {
            Kind::Painting(p) => Some(p.facing),
            _ => None,
        }
    }

    /// Whether an end crystal shows its bedrock base (a placed one does not).
    #[doc(alias = "entity\\object\\EndCrystal::showBase")]
    pub fn shows_base(&self) -> bool {
        matches!(&self.kind, Kind::EndCrystal(c) if c.show_base)
    }

    /// The block an end crystal's beam points at, if any.
    #[doc(alias = "entity\\object\\EndCrystal::getBeamTarget")]
    pub fn beam_target(&self) -> Option<BlockPos> {
        match &self.kind {
            Kind::EndCrystal(c) => c.beam_target,
            _ => None,
        }
    }

    /// A firework rocket's explosions, as a firework item's `Explosions` holds them (compounds of
    /// `FireworkType`, `FireworkColor`, `FireworkFade`, `FireworkFlicker`, `FireworkTrail`).
    #[doc(alias = "entity\\object\\FireworkRocket::getExplosions")]
    pub fn firework_explosions(&self) -> Option<&[Compound]> {
        match &self.kind {
            Kind::Firework(f) => Some(&f.explosions),
            _ => None,
        }
    }

    /// The age in ticks at which a firework rocket bursts.
    #[doc(alias = "entity\\object\\FireworkRocket::getMaxFlightTimeTicks")]
    pub fn max_flight_time_ticks(&self) -> Option<u32> {
        match &self.kind {
            Kind::Firework(f) => Some(f.max_flight),
            _ => None,
        }
    }

    /// A cloud's own data.
    fn cloud(&self) -> Option<&Cloud> {
        match &self.kind {
            Kind::Cloud(c) => Some(c),
            _ => None,
        }
    }

    /// A cloud's effects, which each entity in it gets for a quarter of their duration (instant
    /// ones at half strength).
    #[doc(alias = "AreaEffectCloud::getEffects")]
    pub fn cloud_effects(&self) -> Option<&[Instance]> {
        self.cloud().map(|c| c.effects.as_slice())
    }

    /// A cloud's ticks since it appeared.
    #[doc(alias = "AreaEffectCloud::getAge")]
    pub fn cloud_age_ticks(&self) -> Option<u32> {
        self.cloud().map(|c| c.age)
    }

    /// The age at which a cloud goes.
    #[doc(alias = "AreaEffectCloud::getMaxAge")]
    pub fn cloud_max_age_ticks(&self) -> Option<i32> {
        self.cloud().map(|c| c.max_age)
    }

    /// The change of a cloud's maximum age for each entity it affects.
    #[doc(alias = "AreaEffectCloud::getMaxAgeChangeOnUse")]
    pub fn cloud_max_age_change_on_use_ticks(&self) -> Option<i32> {
        self.cloud().map(|c| c.max_age_on_use)
    }

    /// The ticks before a cloud affects the same entity again.
    #[doc(alias = "AreaEffectCloud::getReapplicationDelay")]
    pub fn cloud_reapplication_delay_ticks(&self) -> Option<i32> {
        self.cloud().map(|c| c.reapplication_delay)
    }

    /// The radius a cloud started with, in blocks.
    #[doc(alias = "AreaEffectCloud::getInitialRadius")]
    pub fn cloud_initial_radius(&self) -> Option<f64> {
        self.cloud().map(|c| c.initial_radius)
    }

    /// The change of a cloud's radius when a bottle takes from it (kept for the saved form:
    /// Anchor has no dragon's breath).
    #[doc(alias = "AreaEffectCloud::getRadiusChangeOnPickup")]
    pub fn cloud_radius_change_on_pickup(&self) -> Option<f64> {
        self.cloud().map(|c| c.radius_on_pickup)
    }

    /// The change of a cloud's radius for each entity it affects.
    #[doc(alias = "AreaEffectCloud::getRadiusChangeOnUse")]
    pub fn cloud_radius_change_on_use(&self) -> Option<f64> {
        self.cloud().map(|c| c.radius_on_use)
    }

    /// The change of a cloud's radius per tick.
    #[doc(alias = "AreaEffectCloud::getRadiusChangePerTick")]
    pub fn cloud_radius_change_per_tick(&self) -> Option<f64> {
        self.cloud().map(|c| c.radius_per_tick)
    }
}

/// How high an entity steps up blocks: PMMP's `Entity::$stepHeight` default, which none of its
/// kinds that Anchor has changes.
const STEP_HEIGHT: f64 = 0.0;

/// Whether the API may put an entity at `pos`: finite, inside the world border, and at most 64
/// blocks above or below the world height (as the saved form reads it back).
pub(super) fn is_placeable(pos: Vec3) -> bool {
    let height = f64::from(MIN_Y) - HEIGHT_MARGIN..=f64::from(MAX_Y) + HEIGHT_MARGIN;
    is_valid_position(pos) && height.contains(&pos.y)
}

/// An arrow's own data.
fn arrow(kind: &Kind) -> Option<&Arrow> {
    match kind {
        Kind::Projectile(Projectile {
            shot: Shot::Arrow(a),
            ..
        }) => Some(a),
        _ => None,
    }
}

/// An entity lent to change ([`Context::entity_mut`]); it reads as the [`Entity`] it changes.
/// Each setter changes the entity at once and returns false, changing nothing, for a value it
/// does not take or a kind the value does not apply to.
pub struct Mut<'a> {
    id: Id,
    entities: &'a mut Entities,
    worlds: &'a [World],
    players: &'a mut BTreeMap<SessionId, Player>,
}

impl Deref for Mut<'_> {
    type Target = Entity;

    fn deref(&self) -> &Entity {
        #[expect(
            clippy::expect_used,
            reason = "invariant: a `Mut` is made for a live entity and holds the store's only \
                      mutable borrow, so nothing removes it while the `Mut` lives"
        )]
        self.entities
            .get(self.id)
            .expect("the lent entity is in the store")
    }
}

impl std::fmt::Debug for Mut<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mut")
            .field("id", &self.id)
            .field("entity", &**self)
            .finish_non_exhaustive()
    }
}

impl Mut<'_> {
    /// The entity, to change; its chunk is marked for the next save. While the entity tick
    /// applies its steps (a listener's change), it moves again from its new state rather than by
    /// the step computed before the change.
    pub(super) fn entity(&mut self) -> &mut Entity {
        let key = {
            let e = &**self;
            (e.world, e.chunk)
        };
        self.entities.changed.insert(key);
        if self.entities.applying {
            self.entities.restep.insert(self.id);
        }
        #[expect(
            clippy::expect_used,
            reason = "invariant: a `Mut` is made for a live entity and holds the store's only \
                      mutable borrow, so nothing removes it while the `Mut` lives"
        )]
        self.entities
            .get_mut(self.id)
            .expect("the lent entity is in the store")
    }

    /// The entity, to change what its viewers see too.
    fn shown(&mut self) -> &mut Entity {
        let e = self.entity();
        e.resend = true;
        e
    }

    /// Its ID.
    pub fn id(&self) -> Id {
        self.id
    }

    /// Sets its motion in blocks per tick (cut to the physics' speed bound). False for a motion
    /// that is not finite.
    #[doc(alias = "Entity::setMotion")]
    #[doc(alias = "Entity::addMotion")]
    pub fn set_motion(&mut self, motion: Vec3) -> bool {
        if !motion.is_finite() {
            return false;
        }
        let e = self.entity();
        e.body.motion = physics::bounded(motion);
        e.forced = true;
        true
    }

    /// Sets its yaw and pitch in degrees (kept to float precision, as saved). False for an angle
    /// that is not finite.
    #[doc(alias = "Entity::setRotation")]
    pub fn set_rotation(&mut self, yaw: f64, pitch: f64) -> bool {
        if !yaw.is_finite() || !pitch.is_finite() {
            return false;
        }
        let e = self.entity();
        e.yaw = f64::from(to_f32(yaw));
        e.pitch = f64::from(to_f32(pitch));
        true
    }

    /// Turns its eyes to `target` (PMMP `Living::lookAt`). False for a target that is not finite.
    #[doc(alias = "Living::lookAt")]
    pub fn look_at(&mut self, target: Vec3) -> bool {
        if !target.is_finite() {
            return false;
        }
        let (at, eye) = (self.position(), self.eye_height());
        let (dx, dz) = (target.x - at.x, target.z - at.z);
        let horizontal = (dx * dx + dz * dz).sqrt();
        let vertical = target.y - (at.y + eye);
        let pitch = -math::degrees(math::atan2(vertical, horizontal));
        let mut yaw = math::degrees(math::atan2(dz, dx)) - 90.0;
        if yaw < 0.0 {
            yaw += 360.0;
        }
        self.set_rotation(yaw, pitch)
    }

    /// Moves it to `pos` in `world`, at rest (PMMP `Entity::teleport`: no motion, no fall
    /// distance; the rotation stays). False, moving nothing, if the world does not exist, `pos` is
    /// not finite, outside the world border or more than 64 blocks above or below the world
    /// height, its chunk is not loaded, another world is at its entity limit, or it is a painting
    /// (it hangs on its wall: PMMP never moves a painting on clients either).
    #[doc(alias = "Entity::teleport")]
    pub fn teleport(&mut self, world: world::Id, pos: Vec3) -> bool {
        if matches!(self.kind, Kind::Painting(_)) {
            return false;
        }
        let to = world.index();
        let loaded = self
            .worlds
            .get(to)
            .is_some_and(|w| is_placeable(pos) && w.is_chunk_loaded(chunk_of(pos)));
        let from = self.world;
        if !loaded || (to != from && self.entities.count(to) >= MAX_ENTITIES_PER_WORLD) {
            return false;
        }
        let e = self.entity();
        e.body.position = pos;
        e.body.motion = Vec3::ZERO;
        e.body.fall_distance = 0.0;
        e.forced = true;
        if let Kind::Projectile(p) = &mut e.kind {
            p.stuck = None;
        }
        if to != from {
            self.entities.move_world(self.id, to);
        }
        self.entities.reindex(self.id);
        true
    }

    /// Sets its name tag (a human NPC's name). False over 1,024 bytes.
    #[doc(alias = "Entity::setNameTag")]
    pub fn set_name_tag(&mut self, name: &str) -> bool {
        if name.len() > MAX_NAME_TAG_BYTES {
            return false;
        }
        name.clone_into(&mut self.shown().name_tag);
        true
    }

    /// Sets whether the name tag shows when a player looks at it.
    #[doc(alias = "Entity::setNameTagVisible")]
    pub fn set_name_tag_visible(&mut self, visible: bool) {
        self.shown().name_tag_visible = visible;
    }

    /// Sets whether the name tag always shows.
    #[doc(alias = "Entity::setNameTagAlwaysVisible")]
    pub fn set_name_tag_always_visible(&mut self, visible: bool) {
        self.shown().name_tag_always_visible = visible;
    }

    /// Sets the size multiplier of its box and model (kept to float precision, as saved). False
    /// unless it is above 0 and the scaled box is at most 16 blocks wide and high.
    #[doc(alias = "Entity::setScale")]
    pub fn set_scale(&mut self, scale: f64) -> bool {
        let scale = f64::from(to_f32(scale));
        if !is_valid_scale(scale, &self.kind) {
            return false;
        }
        let e = self.shown();
        e.scale = scale;
        e.forced = true;
        true
    }

    /// Sets whether clients draw it.
    #[doc(alias = "Entity::setInvisible")]
    pub fn set_invisible(&mut self, invisible: bool) {
        self.entity().invisible = invisible;
    }

    /// Sets whether gravity pulls it.
    #[doc(alias = "Entity::setHasGravity")]
    pub fn set_has_gravity(&mut self, gravity: bool) {
        let e = self.entity();
        e.gravity = gravity;
        e.forced = true;
    }

    /// Sets whether it is saved with its chunk (a minigame's entities need not be; a firework
    /// rocket never is).
    #[doc(alias = "Entity::setCanSaveWithChunk")]
    pub fn set_can_save_with_chunk(&mut self, saved: bool) {
        let e = self.entity();
        e.saved = saved && e.kind.can_be_saved();
    }

    /// Sets it on fire for `seconds` unless it burns longer already or is fire-proof (PMMP
    /// `setOnFire`).
    #[doc(alias = "Entity::setOnFire")]
    #[doc(alias = "Living::setOnFire")]
    pub fn set_on_fire(&mut self, seconds: u16) {
        let seconds = i16::try_from(seconds).unwrap_or(i16::MAX);
        self.entity().set_on_fire(seconds);
    }

    /// Sets the ticks it burns, at most 32,767 (none for a fire-proof entity); 0 puts its fire
    /// out (PMMP `extinguish`, merged: a direct call raises no event).
    #[doc(alias = "Entity::setFireTicks")]
    #[doc(alias = "Entity::extinguish")]
    pub fn set_fire_ticks(&mut self, ticks: u16) {
        let e = self.entity();
        if !e.kind.is_fire_proof() {
            e.fire_ticks = i16::try_from(ticks).unwrap_or(i16::MAX);
        }
    }

    /// Sets its health, at most its maximum (PMMP `setHealth`); 0 or less kills it. False for a
    /// value that is not a number, and for a dying NPC.
    #[doc(alias = "Entity::setHealth")]
    #[doc(alias = "Living::setHealth")]
    pub fn set_health(&mut self, health: f64) -> bool {
        if health.is_nan() || !self.is_alive() {
            return false;
        }
        if health <= 0.0 {
            self.kill();
            return true;
        }
        let e = self.entity();
        e.health = math::min(health, e.kind.max_health());
        true
    }

    /// Adds `amount` to its health, at most its maximum (PMMP `heal`). False for an amount that is
    /// not finite or below 0, and for an entity that does not live.
    #[doc(alias = "Entity::heal")]
    pub fn heal(&mut self, amount: f64) -> bool {
        amount.is_finite() && amount >= 0.0 && self.set_health(self.health + amount)
    }

    /// Hurts it by `amount` (PMMP `attack` with `CAUSE_CUSTOM`, without the event): an NPC
    /// follows its hit cooldown and shows the hit, and dies at 0 health; anything else that
    /// reaches 0 is removed. Falling blocks, primed TNT, projectiles and invulnerable NPCs take
    /// no damage but the void's. False if nothing was taken, as for an amount that is below 0 or
    /// above [`MAX_DAMAGE`](crate::event::MAX_DAMAGE), or not finite.
    #[doc(alias = "Entity::attack")]
    #[doc(alias = "Living::attack")]
    pub fn attack(&mut self, amount: f64) -> bool {
        let id = self.id;
        let key = (self.world, self.chunk);
        let result = hurt_in(self.entities, self.players, id, Cause::Custom, amount, None);
        self.entities.changed.insert(key);
        result != Hurt::Ignored
    }

    /// Kills it (PMMP `kill`): health 0; an NPC shows its death and goes 25 ticks later, a hurt
    /// end crystal explodes at its next tick, anything else goes at the end of the tick (a
    /// painting drops its item). Nothing for an entity that does not live.
    #[doc(alias = "Entity::kill")]
    pub fn kill(&mut self) {
        if !self.is_alive() {
            return;
        }
        let id = self.id;
        let e = self.entity();
        e.health = 0.0;
        let (runtime_id, viewers) = (e.runtime_id, e.viewers.clone());
        if let Kind::Npc(c) = &mut e.kind {
            c.dead_ticks = Some(0);
            send_to(
                self.players,
                &viewers,
                &actor_event(runtime_id, ActorEvent::Death, 0),
            );
        } else if !matches!(&e.kind, Kind::EndCrystal(c) if c.primed) {
            self.entities.doom(id);
        }
    }

    /// Knocks a living NPC back away along (`x`, `z`) with `force`, up at most `vertical_limit`
    /// (PMMP `Living::knockBack`, whose defaults are 0.4 and 0.4). False for another kind, values
    /// that are not finite, or no horizontal direction.
    #[doc(alias = "Living::knockBack")]
    pub fn knock_back(&mut self, x: f64, z: f64, force: f64, vertical_limit: f64) -> bool {
        if !self.kind.is_living() || ![x, z, force, vertical_limit].iter().all(|v| v.is_finite()) {
            return false;
        }
        let knock = Knock {
            x,
            z,
            force,
            vertical: vertical_limit,
        };
        let Some(motion) = knock.applied_to(self.motion()) else {
            return false;
        };
        self.set_motion(motion)
    }

    /// Removes it at the end of the tick (with its removal sent to its viewers). Nothing if it is
    /// gone already.
    #[doc(alias = "Entity::flagForDespawn")]
    #[doc(alias = "Entity::close")]
    pub fn flag_for_despawn(&mut self) {
        let id = self.id;
        self.entities.doom(id);
    }

    /// Sets whether only the void hurts an NPC or a custom entity. False for another kind.
    pub fn set_invulnerable(&mut self, invulnerable: bool) -> bool {
        match &mut self.entity().kind {
            Kind::Npc(c) => {
                c.invulnerable = invulnerable;
                true
            }
            _ => false,
        }
    }

    /// Sets a dropped item's ticks until a player may pick it up; -1 never.
    #[doc(alias = "ItemEntity::setPickupDelay")]
    pub fn set_pickup_delay_ticks(&mut self, ticks: i16) -> bool {
        match &mut self.entity().kind {
            Kind::Item(item) => {
                item.pickup_delay = ticks.max(NEVER);
                true
            }
            _ => false,
        }
    }

    /// Sets a dropped item's or orb's ticks until it disappears: -1 never, otherwise 0 to 38,767
    /// (what the saved `Age` holds).
    #[doc(alias = "ItemEntity::setDespawnDelay")]
    #[doc(alias = "ExperienceOrb::setDespawnDelay")]
    pub fn set_despawn_delay_ticks(&mut self, ticks: i32) -> bool {
        if ticks != i32::from(NEVER) && !(0..=MAX_DESPAWN_DELAY).contains(&ticks) {
            return false;
        }
        match &mut self.entity().kind {
            Kind::Item(Item { despawn_delay, .. })
            | Kind::ExperienceOrb(Orb { despawn_delay, .. }) => {
                *despawn_delay = ticks;
                true
            }
            _ => false,
        }
    }

    /// Sets a dropped item's owner (a player name, at most 1,024 bytes).
    #[doc(alias = "ItemEntity::setOwner")]
    pub fn set_owner(&mut self, owner: &str) -> bool {
        if owner.len() > MAX_NAME_TAG_BYTES {
            return false;
        }
        match &mut self.entity().kind {
            Kind::Item(item) => {
                owner.clone_into(&mut item.owner);
                true
            }
            _ => false,
        }
    }

    /// Sets who threw a dropped item (a player name, at most 1,024 bytes).
    #[doc(alias = "ItemEntity::setThrower")]
    pub fn set_thrower(&mut self, thrower: &str) -> bool {
        if thrower.len() > MAX_NAME_TAG_BYTES {
            return false;
        }
        match &mut self.entity().kind {
            Kind::Item(item) => {
                thrower.clone_into(&mut item.thrower);
                true
            }
            _ => false,
        }
    }

    /// Sets an orb's experience points, 1 to 32,767 (the saved short).
    #[doc(alias = "ExperienceOrb::setXpValue")]
    pub fn set_xp_value(&mut self, value: u16) -> bool {
        if value == 0 || value > i16::MAX.cast_unsigned() {
            return false;
        }
        match &mut self.shown().kind {
            Kind::ExperienceOrb(orb) => {
                orb.value = value;
                true
            }
            _ => false,
        }
    }

    /// Sets primed TNT's ticks until it explodes, at most 32,767.
    #[doc(alias = "PrimedTNT::setFuse")]
    pub fn set_fuse_ticks(&mut self, ticks: u16) -> bool {
        if ticks > MAX_FUSE {
            return false;
        }
        match &mut self.shown().kind {
            Kind::Tnt(t) => {
                t.fuse = ticks;
                true
            }
            _ => false,
        }
    }

    /// Sets whether primed TNT is shown as underwater TNT.
    #[doc(alias = "PrimedTNT::setWorksUnderwater")]
    pub fn set_works_underwater(&mut self, underwater: bool) -> bool {
        match &mut self.shown().kind {
            Kind::Tnt(t) => {
                t.underwater = underwater;
                true
            }
            _ => false,
        }
    }

    /// Sets a projectile's base damage, 0 to 1,024.
    #[doc(alias = "Projectile::setBaseDamage")]
    pub fn set_base_damage(&mut self, damage: f64) -> bool {
        if !(0.0..=MAX_PROJECTILE_DAMAGE).contains(&damage) {
            return false;
        }
        match &mut self.entity().kind {
            Kind::Projectile(p) => {
                p.damage = damage;
                true
            }
            _ => false,
        }
    }

    /// Sets whether an arrow is critical.
    #[doc(alias = "entity\\projectile\\Arrow::setCritical")]
    pub fn set_critical(&mut self, critical: bool) -> bool {
        self.with_arrow(|a| a.critical = critical)
    }

    /// Sets who may pick up an arrow once stuck.
    #[doc(alias = "entity\\projectile\\Arrow::setPickupMode")]
    pub fn set_pickup_mode(&mut self, pickup: ArrowPickup) -> bool {
        self.with_arrow(|a| a.pickup = pickup)
    }

    /// Sets an arrow's Punch level.
    #[doc(alias = "entity\\projectile\\Arrow::setPunchKnockback")]
    pub fn set_punch_knockback(&mut self, level: u8) -> bool {
        self.with_arrow(|a| a.punch = level)
    }

    /// Runs `f` on an arrow's own data; false for another kind.
    fn with_arrow(&mut self, f: impl FnOnce(&mut Arrow)) -> bool {
        match &mut self.entity().kind {
            Kind::Projectile(Projectile {
                shot: Shot::Arrow(a),
                ..
            }) => {
                f(a);
                true
            }
            _ => false,
        }
    }

    /// Sets whether a thrown potion leaves a cloud.
    #[doc(alias = "entity\\projectile\\SplashPotion::setLinger")]
    pub fn set_linger(&mut self, linger: bool) -> bool {
        match &mut self.entity().kind {
            Kind::Projectile(Projectile {
                shot: Shot::Potion { linger: l, .. },
                ..
            }) => {
                *l = linger;
                true
            }
            _ => false,
        }
    }

    /// Makes a villager or a zombie a baby or a grown-up; viewers see the change (the `Baby`
    /// flag). False for another kind (PMMP has no setter: its villagers are never babies).
    pub fn set_baby(&mut self, baby: bool) -> bool {
        match &mut self.shown().kind {
            Kind::Npc(Character {
                form: Form::Villager { baby: b, .. } | Form::Zombie { baby: b },
                ..
            }) => {
                *b = baby;
                true
            }
            _ => false,
        }
    }

    /// Sets a villager NPC's profession (the actor variant).
    #[doc(alias = "Villager::setProfession")]
    pub fn set_profession(&mut self, profession: u8) -> bool {
        match &mut self.shown().kind {
            Kind::Npc(Character {
                form: Form::Villager { profession: p, .. },
                ..
            }) => {
                *p = profession;
                true
            }
            _ => false,
        }
    }

    /// Sets the player a projectile counts as shot or thrown by (who its hit is credited to and,
    /// for an ender pearl, who it teleports) or a firework rocket as launched by, or none. False
    /// for another kind, or a player who is not online.
    #[doc(alias = "Entity::setOwningEntity")]
    pub fn set_owning_player(&mut self, owner: Option<player::Id>) -> bool {
        let runtime_id = match owner {
            Some(id) => match self.players.get(&id.session()) {
                Some(p) if p.phase != Phase::Loading => p.runtime_id,
                _ => return false,
            },
            None => 0,
        };
        match &mut self.shown().kind {
            Kind::Projectile(p) => {
                p.owner = owner.map(player::Id::session);
                p.owner_runtime_id = runtime_id;
                true
            }
            Kind::Firework(f) => {
                f.owner = owner.map(player::Id::session);
                f.owner_runtime_id = runtime_id;
                true
            }
            _ => false,
        }
    }

    /// Sets the player an orb flies to, or none (it looks for the nearest again within a second).
    /// False for another kind, or a player who is not online.
    #[doc(alias = "ExperienceOrb::setTargetPlayer")]
    pub fn set_target_player(&mut self, target: Option<player::Id>) -> bool {
        if target.is_some_and(|id| {
            self.players
                .get(&id.session())
                .is_none_or(|p| p.phase == Phase::Loading)
        }) {
            return false;
        }
        match &mut self.entity().kind {
            Kind::ExperienceOrb(orb) => {
                orb.target = target.map(player::Id::session);
                true
            }
            _ => false,
        }
    }

    /// Sets how many items a dropped item holds, 1 to the item's stack size, and shows the new
    /// count (PMMP `setStackSize` with its `ItemEntityStackSizeChangeAnimation`). False for
    /// another count or kind.
    #[doc(alias = "ItemEntity::setStackSize")]
    pub fn set_stack_size(&mut self, count: u8) -> bool {
        let max = match &self.kind {
            Kind::Item(item) => item
                .stack
                .item()
                .map_or(0, crate::item::Type::max_stack_size),
            _ => 0,
        };
        if count == 0 || count > max {
            return false;
        }
        let e = self.entity();
        if let Kind::Item(item) = &mut e.kind {
            item.stack = item.stack.with_count(count);
        }
        let packet = Animation::ItemStackSize { count }.packet(e.runtime_id);
        let viewers = e.viewers.clone();
        send_to(self.players, &viewers, &packet);
        true
    }

    /// Sets a thrown potion's type.
    #[doc(alias = "entity\\projectile\\SplashPotion::setPotionType")]
    pub fn set_potion(&mut self, potion: Potion) -> bool {
        match &mut self.shown().kind {
            Kind::Projectile(Projectile {
                shot: Shot::Potion { potion: p, .. },
                ..
            }) => {
                *p = potion;
                true
            }
            _ => false,
        }
    }

    /// Sets whether an end crystal shows its bedrock base. False for another kind.
    #[doc(alias = "entity\\object\\EndCrystal::setShowBase")]
    pub fn set_show_base(&mut self, show: bool) -> bool {
        match &mut self.entity().kind {
            Kind::EndCrystal(c) => {
                c.show_base = show;
                true
            }
            _ => false,
        }
    }

    /// Sets the block an end crystal's beam points at, or none. False for another kind.
    #[doc(alias = "entity\\object\\EndCrystal::setBeamTarget")]
    pub fn set_beam_target(&mut self, target: Option<BlockPos>) -> bool {
        match &mut self.shown().kind {
            Kind::EndCrystal(c) => {
                c.beam_target = target;
                true
            }
            _ => false,
        }
    }

    /// Sets a firework rocket's explosions (see [`Entity::firework_explosions`]). False, changing
    /// nothing, for another kind, more than 256 explosions, or a compound PMMP does not read as
    /// one (a shape byte 0 to 4, at least one colour, colours 0 to 15).
    #[doc(alias = "entity\\object\\FireworkRocket::setExplosions")]
    pub fn set_firework_explosions(&mut self, explosions: Vec<Compound>) -> bool {
        let Some(explosions) = object::checked_explosions(explosions) else {
            return false;
        };
        if !matches!(self.kind, Kind::Firework(_)) {
            return false;
        }
        if let Kind::Firework(f) = &mut self.shown().kind {
            f.explosions = explosions;
        }
        true
    }

    /// Sets the age in ticks at which a firework rocket bursts (an age it has reached bursts it at
    /// its next tick). False for another kind.
    #[doc(alias = "entity\\object\\FireworkRocket::setMaxFlightTimeTicks")]
    pub fn set_max_flight_time_ticks(&mut self, ticks: u32) -> bool {
        match &mut self.entity().kind {
            Kind::Firework(f) => {
                f.max_flight = ticks;
                true
            }
            _ => false,
        }
    }

    /// Sets the item a thrown trident holds (what a player who picks it up gets). False for
    /// another kind or an empty stack.
    #[doc(alias = "entity\\projectile\\Trident::setItem")]
    pub fn set_item(&mut self, item: Stack) -> bool {
        let trident = matches!(
            self.kind,
            Kind::Projectile(Projectile {
                shot: Shot::Trident(_),
                ..
            })
        );
        if item.is_empty() || !trident {
            return false;
        }
        if let Kind::Projectile(Projectile {
            shot: Shot::Trident(t),
            ..
        }) = &mut self.entity().kind
        {
            t.item = item;
        }
        true
    }

    /// Runs `f` on a cloud's own data; false for another kind.
    fn with_cloud(&mut self, f: impl FnOnce(&mut Cloud)) -> bool {
        match &mut self.entity().kind {
            Kind::Cloud(c) => {
                f(c);
                true
            }
            _ => false,
        }
    }

    /// Sets the age at which a cloud goes, 0 to 24,000 ticks (as the saved form reads it).
    #[doc(alias = "AreaEffectCloud::setMaxAge")]
    pub fn set_cloud_max_age_ticks(&mut self, ticks: i32) -> bool {
        (0..=MAX_CLOUD_AGE).contains(&ticks) && self.with_cloud(|c| c.max_age = ticks)
    }

    /// Sets the change of a cloud's maximum age for each entity it affects, -24,000 to 24,000.
    #[doc(alias = "AreaEffectCloud::setMaxAgeChangeOnUse")]
    pub fn set_cloud_max_age_change_on_use_ticks(&mut self, ticks: i32) -> bool {
        (-MAX_CLOUD_AGE..=MAX_CLOUD_AGE).contains(&ticks)
            && self.with_cloud(|c| c.max_age_on_use = ticks)
    }

    /// Sets the ticks before a cloud affects the same entity again, 0 to 24,000.
    #[doc(alias = "AreaEffectCloud::setReapplicationDelay")]
    pub fn set_cloud_reapplication_delay_ticks(&mut self, ticks: i32) -> bool {
        (0..=MAX_CLOUD_AGE).contains(&ticks) && self.with_cloud(|c| c.reapplication_delay = ticks)
    }

    /// Sets the change of a cloud's radius when a bottle takes from it, -64 to 64 blocks.
    #[doc(alias = "AreaEffectCloud::setRadiusChangeOnPickup")]
    pub fn set_cloud_radius_change_on_pickup(&mut self, blocks: f64) -> bool {
        let blocks = f64::from(to_f32(blocks));
        is_radius_change(blocks) && self.with_cloud(|c| c.radius_on_pickup = blocks)
    }

    /// Sets the change of a cloud's radius for each entity it affects, -64 to 64 blocks.
    #[doc(alias = "AreaEffectCloud::setRadiusChangeOnUse")]
    pub fn set_cloud_radius_change_on_use(&mut self, blocks: f64) -> bool {
        let blocks = f64::from(to_f32(blocks));
        is_radius_change(blocks) && self.with_cloud(|c| c.radius_on_use = blocks)
    }

    /// Sets the change of a cloud's radius per tick, -64 to 64 blocks.
    #[doc(alias = "AreaEffectCloud::setRadiusChangePerTick")]
    pub fn set_cloud_radius_change_per_tick(&mut self, blocks: f64) -> bool {
        let blocks = f64::from(to_f32(blocks));
        is_radius_change(blocks) && self.with_cloud(|c| c.radius_per_tick = blocks)
    }
}

/// Whether a cloud takes `blocks` as a radius change (finite, within the saved form's bound).
fn is_radius_change(blocks: f64) -> bool {
    (-MAX_CLOUD_RADIUS..=MAX_CLOUD_RADIUS).contains(&blocks)
}

impl Entities {
    /// Moves a live entity's count to world `to`; the caller checked the cap and reindexes it.
    fn move_world(&mut self, id: Id, to: usize) {
        let Some(e) = self.get_mut(id) else {
            return;
        };
        let (from, chunk) = (e.world, e.chunk);
        e.world = to;
        if let Some(n) = self.counts.get_mut(&from) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.counts.remove(&from);
            }
        }
        *self.counts.entry(to).or_insert(0) += 1;
        // The index entry moves as a chunk change of the old world to a chunk of the new one.
        if let Some(column) = self.columns.get_mut(&(from, chunk)) {
            column.ids.remove(&id);
            if column.ids.is_empty() {
                self.columns.remove(&(from, chunk));
            }
        }
        let column = self.columns.entry((to, chunk)).or_default();
        column.ids.insert(id);
        column.arrived.insert(id);
        self.changed.insert((from, chunk));
        self.changed.insert((to, chunk));
    }

    /// The live, unflagged entities of `world` in `Id` order: the store's slots, filtered.
    fn of_world(&self, world: usize) -> impl Iterator<Item = (Id, &Entity)> + '_ {
        (0u32..).zip(&self.slots).filter_map(move |(i, s)| {
            let e = s.entity.as_ref()?;
            (e.world == world && !e.flagged).then_some((Id::new(i, s.generation), e))
        })
    }
}

impl Context<'_> {
    /// The entity `id`, while it exists (removed, despawned and unloaded entities are gone).
    pub fn entity(&self, id: Id) -> Option<&Entity> {
        self.state.rules.entities.get(id).filter(|e| !e.flagged)
    }

    /// The entity `id`, to change it.
    pub fn entity_mut(&mut self, id: Id) -> Option<Mut<'_>> {
        let State {
            rules,
            worlds,
            players,
            ..
        } = &mut *self.state;
        rules.entities.get(id).filter(|e| !e.flagged)?;
        Some(Mut {
            id,
            entities: &mut rules.entities,
            worlds,
            players,
        })
    }

    /// The entity clients know by `runtime_id` (an index lookup; players are not entities here).
    #[doc(alias = "WorldManager::findEntity")]
    #[doc(alias = "World::getEntity")]
    pub fn find_entity(&self, runtime_id: u64) -> Option<Id> {
        super::find(self.state, runtime_id).filter(|&id| self.entity(id).is_some())
    }

    /// The entities of `world` in `Id` order (none for a world the server does not have). It
    /// visits every slot of the store, so a rule that needs the entities of a place asks
    /// [`Self::entities_in`] instead.
    #[doc(alias = "World::getEntities")]
    pub fn entities(&self, world: world::Id) -> impl Iterator<Item = (Id, &Entity)> + '_ {
        let world = index(self.state, world).unwrap_or(usize::MAX);
        self.state.rules.entities.of_world(world)
    }

    /// The entities in chunk `chunk` of `world`, in `Id` order.
    #[doc(alias = "World::getChunkEntities")]
    pub fn chunk_entities(
        &self,
        world: world::Id,
        chunk: ChunkPos,
    ) -> impl Iterator<Item = (Id, &Entity)> + '_ {
        let entities = &self.state.rules.entities;
        let world = index(self.state, world).unwrap_or(usize::MAX);
        entities
            .columns
            .get(&(world, chunk))
            .into_iter()
            .flat_map(|c| c.ids.iter())
            .filter_map(|&id| entities.get(id).filter(|e| !e.flagged).map(|e| (id, e)))
    }

    /// The entities of `world` whose box overlaps `area` (PMMP `getNearbyEntities` and
    /// `getCollidingEntities`), in chunk then `Id` order, through the chunk index. Empty for a
    /// world the server does not have or an area that is not finite.
    #[doc(alias = "World::getNearbyEntities")]
    #[doc(alias = "World::getCollidingEntities")]
    pub fn entities_in(&self, world: world::Id, area: Aabb) -> Vec<Id> {
        match index(self.state, world) {
            Some(w) if area.min.is_finite() && area.max.is_finite() => {
                self.state.rules.entities.nearby(w, area)
            }
            _ => Vec::new(),
        }
    }

    /// The nearest entity of `world` within `max_distance` of `pos` that `filter` accepts, by
    /// the distance of its position (PMMP `getNearestEntity`; ties go to the lower `Id`). `None`
    /// for a world the server does not have, or a `pos` or distance that is not finite.
    #[doc(alias = "World::getNearestEntity")]
    pub fn nearest_entity(
        &self,
        world: world::Id,
        pos: Vec3,
        max_distance: f64,
        mut filter: impl FnMut(&Entity) -> bool,
    ) -> Option<Id> {
        if !pos.is_finite() || !max_distance.is_finite() || max_distance < 0.0 {
            return None;
        }
        let reach = Vec3::new(max_distance, max_distance, max_distance);
        let area = Aabb::new(pos - reach, pos + reach);
        let entities = &self.state.rules.entities;
        let mut best: Option<(f64, Id)> = None;
        for id in self.entities_in(world, area) {
            let Some(e) = entities.get(id) else {
                continue;
            };
            let d = e.body.position.distance_squared(pos);
            if d > max_distance * max_distance || !filter(e) {
                continue;
            }
            if best.is_none_or(|(bd, bid)| d < bd || (d == bd && id < bid)) {
                best = Some((d, id));
            }
        }
        best.map(|(_, id)| id)
    }

    /// Drops `stack` as an item entity at `pos` in `world` (PMMP `World::dropItem`): with
    /// `motion`, or PMMP's random one for `None`, and a pickup delay of 10 ticks. `Err` gives the
    /// stack back when it is empty, the world does not exist, `pos` is not finite, outside the
    /// world border, more than 64 blocks above or below the world height or not loaded, or the
    /// world is at its entity limit.
    #[doc(alias = "World::dropItem")]
    pub fn drop_item(
        &mut self,
        world: world::Id,
        pos: Vec3,
        stack: Stack,
        motion: Option<Vec3>,
    ) -> Result<Id, Stack> {
        let Some(w) = index(self.state, world) else {
            return Err(stack);
        };
        if stack.is_empty() || !is_placeable(pos) || motion.is_some_and(|m| !m.is_finite()) {
            return Err(stack);
        }
        let back = stack.clone();
        drop_stack(self.state, w, pos, stack, motion, super::DROP_PICKUP_DELAY).ok_or(back)
    }

    /// Drops `amount` experience points as orbs at `pos` in `world` (PMMP
    /// `World::dropExperience`: split into the orb sizes, each with a random motion) and returns
    /// the points dropped: fewer at the world's entity limit, none for a world the server does
    /// not have, a `pos` that is not loaded, or with `experience = false` or
    /// `experience-orbs = false`.
    #[doc(alias = "World::dropExperience")]
    pub fn drop_experience(&mut self, world: world::Id, pos: Vec3, amount: u32) -> u32 {
        match index(self.state, world) {
            Some(w) if is_placeable(pos) => super::drop_orbs(self.state, w, pos, amount),
            _ => 0,
        }
    }

    /// Spawns one experience orb of `value` points (1 to 32,767) at `pos` in `world`, at rest.
    /// `None` for another value, where [`Self::drop_item`] fails, or with `experience = false` or
    /// `experience-orbs = false`.
    #[doc(alias = "ExperienceOrb::__construct")]
    pub fn spawn_experience_orb(&mut self, world: world::Id, pos: Vec3, value: u16) -> Option<Id> {
        if !self.state.settings.has_orbs() || value == 0 || value > i16::MAX.cast_unsigned() {
            return None;
        }
        let orb = Orb {
            value,
            despawn_delay: super::DEFAULT_DESPAWN_DELAY,
            look_for_target: 0,
            target: None,
        };
        self.spawn(world, pos, Kind::ExperienceOrb(orb), Vec3::ZERO)
    }

    /// Spawns a falling block of `block` at `pos` in `world` (the world's block there stays as it
    /// is); it lands as a block, or drops its item where it cannot. `None` where
    /// [`Self::drop_item`] fails.
    #[doc(alias = "FallingBlock::__construct")]
    pub fn spawn_falling_block(
        &mut self,
        world: world::Id,
        pos: Vec3,
        block: block::State,
    ) -> Option<Id> {
        let kind = Kind::FallingBlock(Falling { block });
        self.spawn(world, pos, kind, Vec3::ZERO)
    }

    /// Spawns primed TNT at `pos` in `world` that explodes after `fuse_ticks` ticks (at most
    /// 32,767). `None` for a longer fuse, or where [`Self::drop_item`] fails.
    pub fn spawn_tnt(&mut self, world: world::Id, pos: Vec3, fuse_ticks: u16) -> Option<Id> {
        if fuse_ticks > MAX_FUSE {
            return None;
        }
        let kind = Kind::Tnt(Tnt {
            fuse: fuse_ticks,
            underwater: false,
        });
        self.spawn(world, pos, kind, Vec3::ZERO)
    }

    /// Launches `projectile` from `pos` in `world` with `motion` in blocks per tick (cut to the
    /// physics' speed bound); it has no owner. `None` for a motion that is not finite, or where
    /// [`Self::drop_item`] fails.
    #[doc(alias = "Projectile::__construct")]
    pub fn spawn_projectile(
        &mut self,
        world: world::Id,
        pos: Vec3,
        projectile: crate::entity::Projectile,
        motion: Vec3,
    ) -> Option<Id> {
        use crate::entity::Projectile as P;
        if !motion.is_finite() {
            return None;
        }
        let (shot, damage) = match projectile {
            P::Arrow { critical } => {
                let arrow = Arrow {
                    critical,
                    pickup: Pickup::Any,
                    punch: 0,
                    life: 0,
                };
                (Shot::Arrow(arrow), projectile::ARROW_DAMAGE)
            }
            P::Snowball => (Shot::Snowball, 0.0),
            P::Egg => (Shot::Egg, 0.0),
            P::EnderPearl => (Shot::EnderPearl, 0.0),
            P::SplashPotion(potion) => (
                Shot::Potion {
                    potion,
                    linger: false,
                },
                0.0,
            ),
            P::LingeringPotion(potion) => (
                Shot::Potion {
                    potion,
                    linger: true,
                },
                0.0,
            ),
            P::ExperienceBottle => (Shot::ExperienceBottle, 0.0),
            P::IceBomb => (Shot::IceBomb, 0.0),
        };
        let kind = Kind::Projectile(Projectile {
            shot,
            damage,
            stuck: None,
            owner: None,
            owner_runtime_id: 0,
        });
        self.spawn(world, pos, kind, motion)
    }

    /// Spawns a human NPC named `name` (at most 1,024 bytes) at `pos` (its feet) in `world`, with
    /// `skin` or the default skin, and its name always showing. Like every NPC Anchor spawns it is
    /// invulnerable ([`Mut::set_invulnerable`]) and has no AI. `None` for a longer name, or
    /// where [`Self::drop_item`] fails.
    #[doc(alias = "Human::__construct")]
    pub fn spawn_human(
        &mut self,
        world: world::Id,
        pos: Vec3,
        name: &str,
        skin: Option<Skin>,
    ) -> Option<Id> {
        let w = index(self.state, world)?;
        let npc = Npc::Human {
            name: name.to_owned(),
            skin,
        };
        super::spawn_npc(self.state, w, pos, npc)
    }

    /// Spawns a zombie at `pos` (its feet) in `world` (PMMP `Zombie`, which has no AI either): it
    /// stands where it is put, falls, burns, takes damage and at its death drops rotten flesh
    /// and sometimes an iron ingot, a carrot or a potato, and 5 experience. `None` where
    /// [`Self::drop_item`] fails.
    pub fn spawn_zombie(&mut self, world: world::Id, pos: Vec3) -> Option<Id> {
        let w = index(self.state, world)?;
        super::spawn_npc(self.state, w, pos, Npc::Zombie)
    }

    /// Spawns a cloud at `pos` in `world` that gives `effects` (at most 32) with PMMP's defaults
    /// for a lingering potion's: radius 3, gone after 600 ticks, shrinking as it is used. `None`
    /// for more effects, or where [`Self::drop_item`] fails.
    #[doc(alias = "AreaEffectCloud::__construct")]
    pub fn spawn_area_effect_cloud(
        &mut self,
        world: world::Id,
        pos: Vec3,
        effects: &[Instance],
    ) -> Option<Id> {
        if effects.len() > MAX_CLOUD_EFFECTS {
            return None;
        }
        let kind = Kind::Cloud(projectile::new_cloud(effects));
        self.spawn(world, pos, kind, Vec3::ZERO)
    }

    /// Hangs a painting of `motive` in the block `pos` of `world`, on the wall behind `facing`
    /// (north, south, west or east), whether or not it fits there (see [`Self::can_hang_painting`];
    /// a neighbour update that finds it does not fit makes it fall). `None` for an up or down
    /// facing, or where [`Self::drop_item`] fails.
    #[doc(alias = "Painting::__construct")]
    pub fn spawn_painting(
        &mut self,
        world: world::Id,
        pos: BlockPos,
        facing: Face,
        motive: PaintingMotive,
    ) -> Option<Id> {
        if !object::is_horizontal(facing) {
            return None;
        }
        let corner = Vec3::new(f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
        self.spawn(world, corner, object::painting(facing, motive), Vec3::ZERO)
    }

    /// Whether a painting of `motive` can hang in the block `pos` of `world` on the wall behind
    /// `facing` (PMMP `Painting::canFit` with the overlap check): every block it covers is loaded
    /// and not solid with a solid block behind it, and no other painting is there. False for a
    /// world the server does not have or an up or down facing.
    #[doc(alias = "Painting::canFit")]
    pub fn can_hang_painting(
        &self,
        world: world::Id,
        pos: BlockPos,
        facing: Face,
        motive: PaintingMotive,
    ) -> bool {
        index(self.state, world)
            .is_some_and(|w| object::can_hang(self.state, w, pos, facing, motive))
    }

    /// Spawns an end crystal at `pos` (the middle of its bottom) in `world`, without its base or
    /// a beam. `None` where [`Self::drop_item`] fails.
    pub fn spawn_end_crystal(&mut self, world: world::Id, pos: Vec3) -> Option<Id> {
        self.spawn(world, pos, object::crystal(), Vec3::ZERO)
    }

    /// Launches a firework rocket from `pos` in `world` with `motion` in blocks per tick that
    /// bursts with `explosions` (see [`Entity::firework_explosions`]) at the age of
    /// `max_flight_ticks`; it has no owner and is never saved. `None` for explosions
    /// [`Mut::set_firework_explosions`] refuses, a motion that is not finite, or where
    /// [`Self::drop_item`] fails.
    #[doc(alias = "entity\\object\\FireworkRocket::__construct")]
    pub fn spawn_firework_rocket(
        &mut self,
        world: world::Id,
        pos: Vec3,
        max_flight_ticks: u32,
        explosions: Vec<Compound>,
        motion: Vec3,
    ) -> Option<Id> {
        let explosions = object::checked_explosions(explosions)?;
        if !motion.is_finite() {
            return None;
        }
        let flight = object::flight_multiplier(max_flight_ticks);
        let kind = object::firework(max_flight_ticks, flight, explosions, None);
        self.spawn(world, pos, kind, motion)
    }

    /// Throws a trident holding `item` from `pos` in `world` with `motion` in blocks per tick
    /// (cut to the physics' speed bound); it has no owner, deals PMMP's 8 damage and is picked
    /// up as `item`. `Err` gives `item` back when it is empty, the motion is not finite, or where
    /// [`Self::drop_item`] fails.
    #[doc(alias = "entity\\projectile\\Trident::__construct")]
    pub fn spawn_trident(
        &mut self,
        world: world::Id,
        pos: Vec3,
        item: Stack,
        motion: Vec3,
    ) -> Result<Id, Stack> {
        if item.is_empty() || !motion.is_finite() {
            return Err(item);
        }
        let back = item.clone();
        let kind = Kind::Projectile(Projectile {
            shot: Shot::Trident(Trident {
                item,
                creative: false,
                can_collide: true,
            }),
            damage: TRIDENT_DAMAGE,
            stuck: None,
            owner: None,
            owner_runtime_id: 0,
        });
        self.spawn(world, pos, kind, motion).ok_or(back)
    }

    /// Plays `animation` on `actor` for the players who see it (PMMP `broadcastAnimation`: a
    /// player sees its own too). A cosmetic packet like a sound: a player whose outbox is nearly
    /// full misses it. False if the actor is gone or not yet shown (an entity loaded this tick).
    #[doc(alias = "Entity::broadcastAnimation")]
    #[doc(alias = "Player::broadcastAnimation")]
    pub fn play_animation(&mut self, actor: Actor, animation: Animation) -> bool {
        let shown = match actor {
            Actor::Player(id) => self
                .player(id)
                .map(|p| (world::Id::of(p.world), p.position, p.runtime_id)),
            Actor::Entity(id) => self
                .entity(id)
                .map(|e| (e.world(), e.body.position, e.runtime_id)),
        };
        match shown {
            Some((world, at, runtime_id)) if runtime_id != 0 => {
                self.broadcast_packet(world, at, &animation.packet(runtime_id))
            }
            _ => false,
        }
    }

    /// Stops showing the player `target` to the player `viewer` until [`Self::show_player`] or
    /// either leaves: `viewer`'s client removes it at the end of the tick and is not sent it
    /// again (PMMP `hidePlayer`). True if `target` is hidden from `viewer` now; false if either is
    /// not online, they are the same player, or `viewer` hides 1,024 players already.
    #[doc(alias = "Player::hidePlayer")]
    pub fn hide_player(&mut self, viewer: player::Id, target: player::Id) -> bool {
        if viewer == target || self.player(viewer).is_none() || self.player(target).is_none() {
            return false;
        }
        let (v, t) = (viewer.session(), target.session());
        let hidden = &mut self.state.rules.entities.hidden;
        if hidden.contains(&(v, t)) {
            return true;
        }
        if hidden
            .range((v, SessionId::new(0))..=(v, SessionId::CONSOLE))
            .count()
            >= MAX_HIDDEN_PER_PLAYER
        {
            return false;
        }
        hidden.insert((v, t))
    }

    /// Shows the player `target` to the player `viewer` again (PMMP `showPlayer`): it is sent at
    /// the end of the tick if `viewer` sees its chunk. False if it was not hidden.
    #[doc(alias = "Player::showPlayer")]
    pub fn show_player(&mut self, viewer: player::Id, target: player::Id) -> bool {
        self.state
            .rules
            .entities
            .hidden
            .remove(&(viewer.session(), target.session()))
    }

    /// Whether the player `viewer` may see the player `target`: it is not hidden from it (PMMP
    /// `canSee`).
    #[doc(alias = "Player::canSee")]
    pub fn can_see(&self, viewer: player::Id, target: player::Id) -> bool {
        !self
            .state
            .rules
            .entities
            .hidden
            .contains(&(viewer.session(), target.session()))
    }

    /// Creates an entity of `kind` at `pos` in `world` with `motion` and no rotation.
    fn spawn(&mut self, world: world::Id, pos: Vec3, kind: Kind, motion: Vec3) -> Option<Id> {
        let w = index(self.state, world).filter(|_| is_placeable(pos))?;
        create(self.state, w, pos, kind, |_| (motion, 0.0, 0.0))
    }
}

impl action::Sealed for action::Attack {
    fn perform(self, cx: &mut Context<'_>) {
        super::attack(cx.state, self.attacker.session(), self.target);
    }
}

impl action::Sealed for action::Explode {
    fn perform(self, cx: &mut Context<'_>) {
        let Self {
            world,
            center,
            size,
            fire,
        } = self;
        let Some(w) = index(cx.state, world) else {
            return;
        };
        super::explode(cx.state, w, center, size, fire, Raise::Events);
    }
}

#[cfg(test)]
mod tests;
