//! Entity events: damage, death, spawning, explosions and the other things that happen to
//! entities.
//!
//! Players count as entities, so most of these events name an [`Actor`]. A family of related
//! events is one event with an enum for the variant: [`EntityDamage`] has a [`DamageSource`],
//! [`EntityCombust`] a [`CombustSource`].

use super::{Actor, fire_chance_after, set_clamped, set_radius};
use crate::block::State;
use crate::effect::Instance;
use crate::entity;
use crate::item::Stack;
use crate::math::{self, Vec3};
use crate::world::{self, BlockPos};

/// The most damage, in half hearts, a hit may start with and a modifier may add or take away:
/// 2^24. [`crate::action::Damage`] and [`crate::entity::Mut::attack`] refuse a larger amount, and
/// [`EntityDamage`]'s setters cut a value to it. It is far above any hit the game deals, and it
/// keeps every sum of a hit (Strength up to level 256, armor, eleven modifiers) far below 2^53,
/// where `f64` would round a totem's health − 1 away and the totem would not save its holder.
pub const MAX_DAMAGE: f64 = 16_777_216.0;
/// The most a listener may set a hit's knockback force or its upward limit to, either way, in
/// blocks per tick: the client applies the motion as sent, and a packet carries `f32`.
const MAX_KNOCKBACK: f64 = 16.0;

/// Raised when a lingering potion's cloud is about to give its effects to the players and
/// entities in it. Cancelling skips this application.
#[doc(alias = "AreaEffectCloudApplyEvent")]
#[derive(Debug)]
pub struct AreaEffectCloudApply {
    pub(crate) entity: entity::Id,
    pub(crate) affected: Vec<Actor>,
    pub(crate) cancelled: bool,
}

getters!(AreaEffectCloudApply {
    /// The cloud.
    #[doc(alias = "AreaEffectCloudApplyEvent::getEntity")]
    entity: entity::Id,
    /// Who gets the effects.
    #[doc(alias = "AreaEffectCloudApplyEvent::getAffectedEntities")]
    affected: &[Actor],
});

cancellable! {
    AreaEffectCloudApply
        #[doc(alias = "AreaEffectCloudApplyEvent::cancel")]
        #[doc(alias = "AreaEffectCloudApplyEvent::uncancel")]
        #[doc(alias = "AreaEffectCloudApplyEvent::isCancelled")];
}

/// Raised when an entity changes a block: a falling block lands and becomes a block. Cancelling
/// keeps the old block.
#[doc(alias = "EntityBlockChangeEvent")]
#[derive(Debug)]
pub struct EntityBlockChange {
    pub(crate) entity: entity::Id,
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) to: State,
    pub(crate) cancelled: bool,
}

getters!(EntityBlockChange {
    /// The entity.
    entity: entity::Id,
    /// The world of the block.
    world: world::Id,
    /// Where the block is.
    pos: BlockPos,
    /// The block before.
    #[doc(alias = "EntityBlockChangeEvent::getBlock")]
    block: State,
    /// The block after.
    #[doc(alias = "EntityBlockChangeEvent::getTo")]
    to: State,
});

cancellable! {
    EntityBlockChange
        #[doc(alias = "EntityBlockChangeEvent::cancel")]
        #[doc(alias = "EntityBlockChangeEvent::uncancel")]
        #[doc(alias = "EntityBlockChangeEvent::isCancelled")];
}

/// Raised when a player or an entity catches fire. Cancelling keeps it from burning.
#[doc(alias = "EntityCombustEvent")]
#[doc(alias = "EntityCombustByBlockEvent")]
#[doc(alias = "EntityCombustByEntityEvent")]
#[derive(Debug)]
pub struct EntityCombust {
    pub(crate) entity: Actor,
    pub(crate) duration_secs: u32,
    pub(crate) source: CombustSource,
    pub(crate) cancelled: bool,
}

getters!(EntityCombust {
    /// Who catches fire.
    entity: Actor,
    /// How long it burns, in seconds.
    #[doc(alias = "EntityCombustEvent::getDuration")]
    duration_secs: u32,
    /// What sets it on fire.
    #[doc(alias = "EntityCombustByBlockEvent::getCombuster")]
    #[doc(alias = "EntityCombustByEntityEvent::getCombuster")]
    source: CombustSource,
});

setters!(EntityCombust {
    /// Sets how long it burns, in seconds. 0 burns nothing, as a cancel does.
    #[doc(alias = "EntityCombustEvent::setDuration")]
    set_duration_secs => duration_secs: u32,
});

cancellable! {
    EntityCombust
        #[doc(alias = "EntityCombustEvent::cancel")]
        #[doc(alias = "EntityCombustEvent::uncancel")]
        #[doc(alias = "EntityCombustEvent::isCancelled")];
}

/// What sets something on fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CombustSource {
    /// A block: fire, lava or a lava cauldron at this position.
    Block(BlockPos),
    /// An entity or a player: a burning arrow, or a player hitting with a Fire Aspect weapon.
    Entity(Actor),
}

/// Raised when a player or an entity takes damage, before its health changes. Cancelling keeps
/// the health, and no knockback follows. A hit by a player or an entity (a projectile included)
/// on an invulnerable NPC is raised too, already cancelled, so a listener learns who hit it:
/// uncancelling it does not hurt the NPC.
#[doc(alias = "EntityDamageEvent")]
#[doc(alias = "EntityDamageByBlockEvent")]
#[doc(alias = "EntityDamageByEntityEvent")]
#[doc(alias = "EntityDamageByChildEntityEvent")]
#[derive(Debug)]
pub struct EntityDamage {
    pub(crate) entity: Actor,
    pub(crate) cause: DamageCause,
    pub(crate) source: DamageSource,
    pub(crate) base_damage: f64,
    pub(crate) original_base_damage: f64,
    pub(crate) modifiers: Modifiers,
    pub(crate) original_modifiers: Modifiers,
    pub(crate) attack_cooldown_ticks: u32,
    pub(crate) knockback: f64,
    pub(crate) vertical_knockback_limit: f64,
    pub(crate) cancelled: bool,
}

getters!(EntityDamage {
    /// Who takes the damage.
    #[doc(alias = "EntityEvent::getEntity")]
    entity: Actor,
    /// Why.
    #[doc(alias = "EntityDamageEvent::getCause")]
    cause: DamageCause,
    /// What deals it.
    #[doc(alias = "EntityDamageByEntityEvent::getDamager")]
    #[doc(alias = "EntityDamageByBlockEvent::getDamager")]
    #[doc(alias = "EntityDamageByChildEntityEvent::getChild")]
    source: DamageSource,
    /// The damage before the modifiers.
    #[doc(alias = "EntityDamageEvent::getBaseDamage")]
    base_damage: f64,
    /// The base damage the event started with.
    #[doc(alias = "EntityDamageEvent::getOriginalBaseDamage")]
    original_base_damage: f64,
    /// The ticks within which a weaker hit does no damage. Starts at 10.
    #[doc(alias = "EntityDamageEvent::getAttackCooldown")]
    attack_cooldown_ticks: u32,
    /// The knockback force of a hit. Starts at 0.4.
    #[doc(alias = "EntityDamageByEntityEvent::getKnockBack")]
    knockback: f64,
    /// The most a hit knocks upward. Starts at 0.4.
    #[doc(alias = "EntityDamageByEntityEvent::getVerticalKnockBackLimit")]
    vertical_knockback_limit: f64,
});

setters!(EntityDamage {
    /// Sets the ticks within which a weaker hit does no damage.
    #[doc(alias = "EntityDamageEvent::setAttackCooldown")]
    set_attack_cooldown_ticks => attack_cooldown_ticks: u32,
});

impl EntityDamage {
    /// Whether armor reduces damage of this cause.
    #[doc(alias = "EntityDamageEvent::canBeReducedByArmor")]
    pub fn can_be_reduced_by_armor(&self) -> bool {
        self.cause.is_reduced_by_armor()
    }

    /// The damage dealt: the base damage plus the modifiers, at least 0.
    #[doc(alias = "EntityDamageEvent::getFinalDamage")]
    pub fn final_damage(&self) -> f64 {
        let total = applicable(&self.modifiers).fold(self.base_damage, |sum, (_, v)| sum + v);
        math::max(total, 0.0)
    }

    /// Whether `modifier` applies to this damage.
    #[doc(alias = "EntityDamageEvent::isApplicable")]
    pub fn is_applicable(&self, modifier: DamageModifier) -> bool {
        modifier_value(&self.modifiers, modifier).is_some()
    }

    /// The value of `modifier`, 0 when it does not apply.
    #[doc(alias = "EntityDamageEvent::getModifier")]
    pub fn modifier(&self, modifier: DamageModifier) -> f64 {
        modifier_value(&self.modifiers, modifier).unwrap_or(0.0)
    }

    /// The modifiers that apply, with their values, in [`DamageModifier`] order.
    #[doc(alias = "EntityDamageEvent::getModifiers")]
    pub fn modifiers(&self) -> impl Iterator<Item = (DamageModifier, f64)> + '_ {
        applicable(&self.modifiers)
    }

    /// The value `modifier` started with, 0 when it did not apply.
    #[doc(alias = "EntityDamageEvent::getOriginalModifier")]
    pub fn original_modifier(&self, modifier: DamageModifier) -> f64 {
        modifier_value(&self.original_modifiers, modifier).unwrap_or(0.0)
    }

    /// The modifiers the event started with, in [`DamageModifier`] order.
    #[doc(alias = "EntityDamageEvent::getOriginalModifiers")]
    pub fn original_modifiers(&self) -> impl Iterator<Item = (DamageModifier, f64)> + '_ {
        applicable(&self.original_modifiers)
    }

    /// Sets the damage before the modifiers, cut to 0 through [`MAX_DAMAGE`]. A value that is not
    /// finite is ignored.
    #[doc(alias = "EntityDamageEvent::setBaseDamage")]
    pub fn set_base_damage(&mut self, damage: f64) {
        // The base damage is remembered for the attack cooldown, where a negative one would add to
        // the next hit; reduce the damage with a modifier instead.
        set_clamped(&mut self.base_damage, damage, 0.0, MAX_DAMAGE);
    }

    /// Sets the knockback force of a hit, cut to ±16. A value that is not finite is ignored.
    #[doc(alias = "EntityDamageByEntityEvent::setKnockBack")]
    pub fn set_knockback(&mut self, force: f64) {
        set_clamped(&mut self.knockback, force, -MAX_KNOCKBACK, MAX_KNOCKBACK);
    }

    /// Sets `modifier` to `damage` (negative to reduce), cut to ±[`MAX_DAMAGE`], so that it
    /// applies. A value that is not finite is ignored.
    #[doc(alias = "EntityDamageEvent::setModifier")]
    pub fn set_modifier(&mut self, modifier: DamageModifier, damage: f64) {
        if damage.is_finite() {
            #[expect(
                clippy::indexing_slicing,
                reason = "invariant: a modifier's number is its place in a modifier array"
            )]
            let slot = &mut self.modifiers[modifier as usize];
            *slot = Some(math::clamp(damage, -MAX_DAMAGE, MAX_DAMAGE));
        }
    }

    /// Sets the most a hit knocks upward, cut to ±16. A value that is not finite is ignored.
    #[doc(alias = "EntityDamageByEntityEvent::setVerticalKnockBackLimit")]
    pub fn set_vertical_knockback_limit(&mut self, limit: f64) {
        set_clamped(
            &mut self.vertical_knockback_limit,
            limit,
            -MAX_KNOCKBACK,
            MAX_KNOCKBACK,
        );
    }
}

cancellable! {
    EntityDamage
        #[doc(alias = "EntityDamageEvent::cancel")]
        #[doc(alias = "EntityDamageEvent::uncancel")]
        #[doc(alias = "EntityDamageEvent::isCancelled")];
}

/// Why a player or an entity takes damage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DamageCause {
    /// Touching a block that hurts: cactus, a sweet berry bush past its first stage (with or
    /// without berries).
    #[doc(alias = "EntityDamageEvent::CAUSE_CONTACT")]
    Contact,
    /// A melee hit.
    #[doc(alias = "EntityDamageEvent::CAUSE_ENTITY_ATTACK")]
    EntityAttack,
    /// A projectile's hit.
    #[doc(alias = "EntityDamageEvent::CAUSE_PROJECTILE")]
    Projectile,
    /// Inside a solid block.
    #[doc(alias = "EntityDamageEvent::CAUSE_SUFFOCATION")]
    Suffocation,
    /// A fall.
    #[doc(alias = "EntityDamageEvent::CAUSE_FALL")]
    Fall,
    /// Standing in fire, on magma or in a lit campfire.
    #[doc(alias = "EntityDamageEvent::CAUSE_FIRE")]
    Fire,
    /// Burning after leaving fire.
    #[doc(alias = "EntityDamageEvent::CAUSE_FIRE_TICK")]
    FireTick,
    /// Touching lava.
    #[doc(alias = "EntityDamageEvent::CAUSE_LAVA")]
    Lava,
    /// Under water without air.
    #[doc(alias = "EntityDamageEvent::CAUSE_DROWNING")]
    Drowning,
    /// An explosion no entity caused, as [`Context::explode`](crate::Context::explode) makes.
    #[doc(alias = "EntityDamageEvent::CAUSE_BLOCK_EXPLOSION")]
    BlockExplosion,
    /// An exploding entity (TNT).
    #[doc(alias = "EntityDamageEvent::CAUSE_ENTITY_EXPLOSION")]
    EntityExplosion,
    /// Below the world.
    #[doc(alias = "EntityDamageEvent::CAUSE_VOID")]
    Void,
    /// Damage an [`action::Damage`](crate::action::Damage) gives as the player's own; `/kill`
    /// raises none (it calls [`Player::kill`](crate::player::Player::kill)).
    #[doc(alias = "EntityDamageEvent::CAUSE_SUICIDE")]
    Suicide,
    /// An effect or a potion: poison, harming.
    #[doc(alias = "EntityDamageEvent::CAUSE_MAGIC")]
    Magic,
    /// Damage code gives without a game cause.
    #[doc(alias = "EntityDamageEvent::CAUSE_CUSTOM")]
    Custom,
    /// An empty food bar.
    #[doc(alias = "EntityDamageEvent::CAUSE_STARVATION")]
    Starvation,
    /// A falling block landing on it.
    #[doc(alias = "EntityDamageEvent::CAUSE_FALLING_BLOCK")]
    FallingBlock,
}

impl DamageCause {
    /// Whether armor reduces damage of this cause: the one rule for listeners and the tick.
    pub(crate) fn is_reduced_by_armor(self) -> bool {
        !matches!(
            self,
            Self::Suffocation
                | Self::Fall
                | Self::FireTick
                | Self::Drowning
                | Self::Void
                | Self::Suicide
                | Self::Magic
                | Self::Starvation
        )
    }
}

/// What deals damage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DamageSource {
    /// Nothing in particular: a fall, drowning, food, an effect.
    None,
    /// The block at this position: a cactus, magma, a lit campfire.
    Block(BlockPos),
    /// A player's or an entity's hit, or an exploding entity.
    Entity(Actor),
    /// A projectile, and who shot it while that one is still there.
    Child {
        /// The projectile.
        child: entity::Id,
        /// Who shot or threw it.
        owner: Option<Actor>,
    },
}

/// A change to damage on top of the base damage. The final damage is the base damage plus every
/// applicable modifier, so a reduction is negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum DamageModifier {
    /// Armor points.
    #[doc(alias = "EntityDamageEvent::MODIFIER_ARMOR")]
    Armor,
    /// The attacker's Strength effect.
    #[doc(alias = "EntityDamageEvent::MODIFIER_STRENGTH")]
    Strength,
    /// The attacker's Weakness effect.
    #[doc(alias = "EntityDamageEvent::MODIFIER_WEAKNESS")]
    Weakness,
    /// The Resistance effect.
    #[doc(alias = "EntityDamageEvent::MODIFIER_RESISTANCE")]
    Resistance,
    /// Absorption hearts taking the damage.
    #[doc(alias = "EntityDamageEvent::MODIFIER_ABSORPTION")]
    Absorption,
    /// Protection enchantments on the armor.
    #[doc(alias = "EntityDamageEvent::MODIFIER_ARMOR_ENCHANTMENTS")]
    ArmorEnchantments,
    /// A critical hit.
    #[doc(alias = "EntityDamageEvent::MODIFIER_CRITICAL")]
    Critical,
    /// A totem that saves from death.
    #[doc(alias = "EntityDamageEvent::MODIFIER_TOTEM")]
    Totem,
    /// Damage enchantments on the weapon (Sharpness and the like).
    #[doc(alias = "EntityDamageEvent::MODIFIER_WEAPON_ENCHANTMENTS")]
    WeaponEnchantments,
    /// The part of a hit within the cooldown of an earlier, weaker hit.
    #[doc(alias = "EntityDamageEvent::MODIFIER_PREVIOUS_DAMAGE_COOLDOWN")]
    PreviousDamageCooldown,
    /// A helmet against a falling block.
    #[doc(alias = "EntityDamageEvent::MODIFIER_ARMOR_HELMET")]
    ArmorHelmet,
}

impl DamageModifier {
    /// Every modifier, in order.
    pub const fn all() -> &'static [Self] {
        &[
            Self::Armor,
            Self::Strength,
            Self::Weakness,
            Self::Resistance,
            Self::Absorption,
            Self::ArmorEnchantments,
            Self::Critical,
            Self::Totem,
            Self::WeaponEnchantments,
            Self::PreviousDamageCooldown,
            Self::ArmorHelmet,
        ]
    }
}

/// The modifiers of a hit, by [`DamageModifier`] order; `None` where one does not apply.
pub(crate) type Modifiers = [Option<f64>; DamageModifier::all().len()];

/// Raised when an NPC (a human, a villager, a zombie or a custom entity) dies, before it drops what
/// it carries.
#[doc(alias = "EntityDeathEvent")]
#[derive(Debug)]
pub struct EntityDeath {
    pub(crate) entity: entity::Id,
    pub(crate) drops: Vec<Stack>,
    pub(crate) xp_drop_amount: u32,
}

getters!(EntityDeath {
    /// The entity that dies.
    #[doc(alias = "EntityDeathEvent::getEntity")]
    entity: entity::Id,
    /// What it drops.
    #[doc(alias = "EntityDeathEvent::getDrops")]
    drops: &[Stack],
    /// The XP it drops.
    #[doc(alias = "EntityDeathEvent::getXpDropAmount")]
    xp_drop_amount: u32,
});

setters!(EntityDeath {
    /// Replaces what it drops.
    #[doc(alias = "EntityDeathEvent::setDrops")]
    set_drops => drops: Vec<Stack>,
    /// Replaces the XP it drops.
    #[doc(alias = "EntityDeathEvent::setXpDropAmount")]
    set_xp_drop_amount => xp_drop_amount: u32,
});

/// Raised when an entity that is not a player has left its world: it was removed, or its chunk
/// unloaded. It is already out of the game, so [`Context::entity`](crate::Context::entity) no
/// longer finds it and a listener has only its ID. An entity that leaves with its chunk raises it
/// only if its spawn was announced and it had a runtime ID.
#[doc(alias = "EntityDespawnEvent")]
#[derive(Debug)]
pub struct EntityDespawn {
    pub(crate) entity: entity::Id,
}

getters!(EntityDespawn {
    /// The entity.
    entity: entity::Id,
});

/// Raised when a player gets an effect, or a stronger or longer one replaces the one it has.
/// Cancelling keeps the effects as they were.
#[doc(alias = "EntityEffectAddEvent")]
#[doc(alias = "EntityEffectEvent")]
#[derive(Debug)]
pub struct EntityEffectAdd {
    pub(crate) entity: Actor,
    pub(crate) effect: Instance,
    pub(crate) old_effect: Option<Instance>,
    pub(crate) cancelled: bool,
}

getters!(EntityEffectAdd {
    /// Who gets the effect.
    entity: Actor,
    /// The effect added.
    #[doc(alias = "EntityEffectEvent::getEffect")]
    effect: Instance,
    /// The effect of the same type it replaces, if any.
    #[doc(alias = "EntityEffectAddEvent::getOldEffect")]
    old_effect: Option<Instance>,
});

impl EntityEffectAdd {
    /// Whether the effect replaces one of the same type.
    #[doc(alias = "EntityEffectAddEvent::hasOldEffect")]
    #[doc(alias = "EntityEffectAddEvent::willModify")]
    pub fn has_old_effect(&self) -> bool {
        self.old_effect.is_some()
    }
}

cancellable! {
    EntityEffectAdd
        #[doc(alias = "EntityEffectEvent::cancel")]
        #[doc(alias = "EntityEffectEvent::uncancel")]
        #[doc(alias = "EntityEffectEvent::isCancelled")];
}

/// Raised when an effect ends on a player: it ran out, or milk or code removed it. Cancelling
/// keeps the effect, unless it ran out.
#[doc(alias = "EntityEffectRemoveEvent")]
#[derive(Debug)]
pub struct EntityEffectRemove {
    pub(crate) entity: Actor,
    pub(crate) effect: Instance,
    pub(crate) cancelled: bool,
}

getters!(EntityEffectRemove {
    /// Who loses the effect.
    entity: Actor,
    /// The effect removed.
    effect: Instance,
});

impl EntityEffectRemove {
    /// Whether a listener cancelled the removal.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Keeps the effect. An effect that ran out cannot stay (an infinite one never runs out), so
    /// then nothing happens.
    #[doc(alias = "EntityEffectRemoveEvent::cancel")]
    pub fn cancel(&mut self) {
        if !self.effect.is_expired() || self.effect.is_infinite() {
            self.cancelled = true;
        }
    }

    /// Undoes [`cancel`](Self::cancel).
    pub fn uncancel(&mut self) {
        self.cancelled = false;
    }
}

/// Raised when an entity explodes (primed TNT or an end crystal), after the blocks it breaks are
/// known and before they break. Cancelling keeps the blocks, hurts nobody and plays no particles
/// or sound; the entity is gone either way.
#[doc(alias = "EntityExplodeEvent")]
#[derive(Debug)]
pub struct EntityExplode {
    pub(crate) entity: entity::Id,
    pub(crate) world: world::Id,
    pub(crate) position: Vec3,
    pub(crate) blocks: Vec<BlockPos>,
    pub(crate) yield_percent: f64,
    pub(crate) ignitions: Vec<BlockPos>,
    pub(crate) cancelled: bool,
}

getters!(EntityExplode {
    /// The entity that explodes.
    entity: entity::Id,
    /// The world of the explosion.
    world: world::Id,
    /// The center of the explosion.
    #[doc(alias = "EntityExplodeEvent::getPosition")]
    position: Vec3,
    /// The blocks that break.
    #[doc(alias = "EntityExplodeEvent::getBlockList")]
    blocks: &[BlockPos],
    /// The chance in percent that a broken block drops its items.
    #[doc(alias = "EntityExplodeEvent::getYield")]
    yield_percent: f64,
    /// The blocks that catch fire.
    #[doc(alias = "EntityExplodeEvent::getIgnitions")]
    ignitions: &[BlockPos],
});

setters!(EntityExplode {
    /// Replaces the blocks that break. Only the first 8,192 are used, and blocks outside loaded
    /// chunks are left alone.
    #[doc(alias = "EntityExplodeEvent::setBlockList")]
    set_blocks => blocks: Vec<BlockPos>,
    /// Replaces the blocks that catch fire. Only the first 8,192 are used, and a block catches
    /// fire only if it is air once the blocks have broken and the block under it has a full top.
    #[doc(alias = "EntityExplodeEvent::setIgnitions")]
    set_ignitions => ignitions: Vec<BlockPos>,
});

impl EntityExplode {
    /// Sets the chance in percent that a broken block drops its items, limited to 0 to 100. A
    /// value that is not finite is ignored.
    #[doc(alias = "EntityExplodeEvent::setYield")]
    pub fn set_yield_percent(&mut self, percent: f64) {
        set_clamped(&mut self.yield_percent, percent, 0.0, 100.0);
    }
}

cancellable! {
    EntityExplode
        #[doc(alias = "EntityExplodeEvent::cancel")]
        #[doc(alias = "EntityExplodeEvent::uncancel")]
        #[doc(alias = "EntityExplodeEvent::isCancelled")];
}

/// Raised when a player or an entity stops burning.
#[doc(alias = "EntityExtinguishEvent")]
#[derive(Debug)]
pub struct EntityExtinguish {
    pub(crate) entity: Actor,
    pub(crate) cause: ExtinguishCause,
}

getters!(EntityExtinguish {
    /// Who stops burning.
    entity: Actor,
    /// Why.
    #[doc(alias = "EntityExtinguishEvent::getCause")]
    cause: ExtinguishCause,
});

/// Why a fire went out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExtinguishCause {
    /// Water.
    #[doc(alias = "EntityExtinguishEvent::CAUSE_WATER")]
    Water,
    /// A water cauldron.
    #[doc(alias = "EntityExtinguishEvent::CAUSE_WATER_CAULDRON")]
    WaterCauldron,
    /// Respawning.
    #[doc(alias = "EntityExtinguishEvent::CAUSE_RESPAWN")]
    Respawn,
    /// Fire resistance.
    #[doc(alias = "EntityExtinguishEvent::CAUSE_FIRE_PROOF")]
    FireProof,
    /// The burning time ran out.
    #[doc(alias = "EntityExtinguishEvent::CAUSE_TICKING")]
    Ticking,
}

/// Raised when a player picks up a dropped item, an arrow or a thrown trident (not an XP
/// orb). Cancelling leaves it where it is.
#[doc(alias = "EntityItemPickupEvent")]
#[derive(Debug)]
pub struct EntityItemPickup {
    pub(crate) entity: Actor,
    pub(crate) origin: entity::Id,
    pub(crate) item: Stack,
    pub(crate) cancelled: bool,
}

getters!(EntityItemPickup {
    /// Who picks it up.
    entity: Actor,
    /// The entity picked up.
    #[doc(alias = "EntityItemPickupEvent::getOrigin")]
    origin: entity::Id,
    /// What the picker gets.
    #[doc(alias = "EntityItemPickupEvent::getItem")]
    item: &Stack,
});

setters!(EntityItemPickup {
    /// Replaces what the picker gets; the entity goes as it would.
    #[doc(alias = "EntityItemPickupEvent::setItem")]
    set_item => item: Stack,
});

cancellable! {
    EntityItemPickup
        #[doc(alias = "EntityItemPickupEvent::cancel")]
        #[doc(alias = "EntityItemPickupEvent::uncancel")]
        #[doc(alias = "EntityItemPickupEvent::isCancelled")];
}

/// Raised when a player or an entity is pushed: knockback, an explosion. Cancelling keeps its
/// motion; [`set_motion`](Self::set_motion) pushes it otherwise.
#[doc(alias = "EntityMotionEvent")]
#[derive(Debug)]
pub struct EntityMotion {
    pub(crate) entity: Actor,
    pub(crate) motion: Vec3,
    pub(crate) cancelled: bool,
}

getters!(EntityMotion {
    /// Who is pushed.
    entity: Actor,
    /// The new motion, in blocks per tick.
    #[doc(alias = "EntityMotionEvent::getVector")]
    motion: Vec3,
});

impl EntityMotion {
    /// Replaces the new motion, in blocks per tick, cut to the speed bound of every push (10 a
    /// component) as it is set. A motion that is not finite is ignored.
    pub fn set_motion(&mut self, motion: Vec3) {
        if motion.is_finite() {
            self.motion = crate::tick::bounded_motion(motion);
        }
    }
}

cancellable! {
    EntityMotion
        #[doc(alias = "EntityMotionEvent::cancel")]
        #[doc(alias = "EntityMotionEvent::uncancel")]
        #[doc(alias = "EntityMotionEvent::isCancelled")];
}

/// Raised when an entity is about to explode (primed TNT or an end crystal), before the blocks it
/// breaks are found. Cancelling stops the explosion; the entity is gone either way.
#[doc(alias = "EntityPreExplodeEvent")]
#[derive(Debug)]
pub struct EntityPreExplode {
    pub(crate) entity: entity::Id,
    pub(crate) radius: f64,
    pub(crate) fire_chance: f64,
    pub(crate) block_breaking: bool,
    pub(crate) cancelled: bool,
}

getters!(EntityPreExplode {
    /// The entity that explodes.
    entity: entity::Id,
    /// The radius of the explosion, in blocks.
    #[doc(alias = "EntityPreExplodeEvent::getRadius")]
    radius: f64,
    /// The chance, 0 to 1, that a block the explosion reaches catches fire.
    #[doc(alias = "EntityPreExplodeEvent::getFireChance")]
    fire_chance: f64,
});

setters!(EntityPreExplode {
    /// Sets whether the explosion breaks blocks.
    #[doc(alias = "EntityPreExplodeEvent::setBlockBreaking")]
    set_block_breaking => block_breaking: bool,
});

impl EntityPreExplode {
    /// Whether the explosion breaks blocks.
    #[doc(alias = "EntityPreExplodeEvent::isBlockBreaking")]
    pub fn is_block_breaking(&self) -> bool {
        self.block_breaking
    }

    /// Whether the explosion sets fires.
    #[doc(alias = "EntityPreExplodeEvent::isIncendiary")]
    pub fn is_incendiary(&self) -> bool {
        self.fire_chance > 0.0
    }

    /// Sets the chance that a block catches fire, limited to 0 to 1. A value that is not finite
    /// is ignored.
    #[doc(alias = "EntityPreExplodeEvent::setFireChance")]
    pub fn set_fire_chance(&mut self, chance: f64) {
        set_clamped(&mut self.fire_chance, chance, 0.0, 1.0);
    }

    /// Makes the explosion set fires (with a chance of 1/3 unless one is set) or none.
    #[doc(alias = "EntityPreExplodeEvent::setIncendiary")]
    pub fn set_incendiary(&mut self, incendiary: bool) {
        self.fire_chance = fire_chance_after(self.fire_chance, incendiary);
    }

    /// Sets the radius of the explosion, limited to 8 blocks. A value that is not finite or not
    /// above 0 is ignored.
    #[doc(alias = "EntityPreExplodeEvent::setRadius")]
    pub fn set_radius(&mut self, radius: f64) {
        set_radius(&mut self.radius, radius);
    }
}

cancellable! {
    EntityPreExplode
        #[doc(alias = "EntityPreExplodeEvent::cancel")]
        #[doc(alias = "EntityPreExplodeEvent::uncancel")]
        #[doc(alias = "EntityPreExplodeEvent::isCancelled")];
}

/// Raised when a player or an entity heals: only while it is alive and below its maximum health,
/// for a positive amount. Cancelling keeps its health, and so does an amount the listeners leave
/// that is not positive.
#[doc(alias = "EntityRegainHealthEvent")]
#[derive(Debug)]
pub struct EntityRegainHealth {
    pub(crate) entity: Actor,
    pub(crate) amount: f64,
    pub(crate) regain_reason: RegainReason,
    pub(crate) cancelled: bool,
}

getters!(EntityRegainHealth {
    /// Who heals.
    entity: Actor,
    /// The health gained, in half hearts.
    #[doc(alias = "EntityRegainHealthEvent::getAmount")]
    amount: f64,
    /// Why.
    #[doc(alias = "EntityRegainHealthEvent::getRegainReason")]
    regain_reason: RegainReason,
});

impl EntityRegainHealth {
    /// Sets the health gained. A value that is not finite is ignored.
    #[doc(alias = "EntityRegainHealthEvent::setAmount")]
    pub fn set_amount(&mut self, amount: f64) {
        if amount.is_finite() {
            self.amount = amount;
        }
    }
}

cancellable! {
    EntityRegainHealth
        #[doc(alias = "EntityRegainHealthEvent::cancel")]
        #[doc(alias = "EntityRegainHealthEvent::uncancel")]
        #[doc(alias = "EntityRegainHealthEvent::isCancelled")];
}

/// Why a player or an entity heals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RegainReason {
    /// The Regeneration effect.
    #[doc(alias = "EntityRegainHealthEvent::CAUSE_REGEN")]
    Regen,
    /// An effect or a potion: instant health.
    #[doc(alias = "EntityRegainHealthEvent::CAUSE_MAGIC")]
    Magic,
    /// A full food bar.
    #[doc(alias = "EntityRegainHealthEvent::CAUSE_SATURATION")]
    Saturation,
}

/// Raised when a player shoots a bow, after the arrow is made and before it flies. Cancelling
/// removes the arrow and keeps the bow and the arrows in the inventory.
#[doc(alias = "EntityShootBowEvent")]
#[derive(Debug)]
pub struct EntityShootBow {
    pub(crate) entity: Actor,
    pub(crate) bow: Stack,
    pub(crate) projectile: entity::Id,
    pub(crate) force: f64,
    pub(crate) cancelled: bool,
}

getters!(EntityShootBow {
    /// Who shoots.
    #[doc(alias = "EntityShootBowEvent::getEntity")]
    entity: Actor,
    /// The bow.
    #[doc(alias = "EntityShootBowEvent::getBow")]
    bow: &Stack,
    /// The arrow.
    #[doc(alias = "EntityShootBowEvent::getProjectile")]
    projectile: entity::Id,
    /// The arrow's speed in blocks per tick: 3 for a full draw, less for a shorter one.
    #[doc(alias = "EntityShootBowEvent::getForce")]
    force: f64,
});

impl EntityShootBow {
    /// Sets the arrow's speed in blocks per tick; each component of its motion is cut to 10, which
    /// bends the flight of a faster arrow. A value that is not finite is ignored.
    #[doc(alias = "EntityShootBowEvent::setForce")]
    pub fn set_force(&mut self, force: f64) {
        if force.is_finite() {
            self.force = force;
        }
    }
}

cancellable! {
    EntityShootBow
        #[doc(alias = "EntityShootBowEvent::cancel")]
        #[doc(alias = "EntityShootBowEvent::uncancel")]
        #[doc(alias = "EntityShootBowEvent::isCancelled")];
}

/// Raised when an entity that is not a player appears in its world.
#[doc(alias = "EntitySpawnEvent")]
#[derive(Debug)]
pub struct EntitySpawn {
    pub(crate) entity: entity::Id,
}

getters!(EntitySpawn {
    /// The entity.
    entity: entity::Id,
});

/// Raised when a player teleports by an ender pearl, `/tp` or
/// [`action::Teleport`](crate::action::Teleport). Cancelling keeps it where it is.
#[doc(alias = "EntityTeleportEvent")]
#[derive(Debug)]
pub struct EntityTeleport {
    pub(crate) entity: Actor,
    pub(crate) cause: TeleportCause,
    pub(crate) origin_world: world::Id,
    pub(crate) origin: Vec3,
    pub(crate) target_world: world::Id,
    pub(crate) target: Vec3,
    pub(crate) cancelled: bool,
}

getters!(EntityTeleport {
    /// Who teleports.
    entity: Actor,
    /// Why.
    cause: TeleportCause,
    /// The world it leaves.
    origin_world: world::Id,
    /// Where it was.
    #[doc(alias = "EntityTeleportEvent::getFrom")]
    origin: Vec3,
    /// The world it goes to.
    target_world: world::Id,
    /// Where it goes.
    #[doc(alias = "EntityTeleportEvent::getTo")]
    target: Vec3,
});

impl EntityTeleport {
    /// Sends it elsewhere. A position that is not finite is ignored. A world that is not loaded,
    /// or a place whose eyes lie outside the world border, keeps the player where it is, as a
    /// cancel does ([`Context::teleport`](crate::Context::teleport) fails there).
    #[doc(alias = "EntityTeleportEvent::setTo")]
    pub fn set_target(&mut self, world: world::Id, target: Vec3) {
        if target.is_finite() {
            self.target_world = world;
            self.target = target;
        }
    }
}

cancellable! {
    EntityTeleport
        #[doc(alias = "EntityTeleportEvent::cancel")]
        #[doc(alias = "EntityTeleportEvent::uncancel")]
        #[doc(alias = "EntityTeleportEvent::isCancelled")];
}

/// Why a player teleports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TeleportCause {
    /// Its ender pearl landed.
    EnderPearl,
    /// `/tp`.
    Command,
    /// An [`action::Teleport`](crate::action::Teleport).
    Action,
}

/// Raised when a dropped item is about to vanish after its time on the ground. Cancelling gives
/// it the time again.
#[doc(alias = "ItemDespawnEvent")]
#[derive(Debug)]
pub struct ItemDespawn {
    pub(crate) entity: entity::Id,
    pub(crate) cancelled: bool,
}

getters!(ItemDespawn {
    /// The dropped item.
    #[doc(alias = "ItemDespawnEvent::getEntity")]
    entity: entity::Id,
});

cancellable! {
    ItemDespawn
        #[doc(alias = "ItemDespawnEvent::cancel")]
        #[doc(alias = "ItemDespawnEvent::uncancel")]
        #[doc(alias = "ItemDespawnEvent::isCancelled")];
}

/// Raised when a dropped item is about to merge into another one nearby. Cancelling keeps both.
#[doc(alias = "ItemMergeEvent")]
#[derive(Debug)]
pub struct ItemMerge {
    pub(crate) entity: entity::Id,
    pub(crate) target: entity::Id,
    pub(crate) cancelled: bool,
}

getters!(ItemMerge {
    /// The dropped item that goes.
    entity: entity::Id,
    /// The dropped item that takes its items.
    #[doc(alias = "ItemMergeEvent::getTarget")]
    target: entity::Id,
});

cancellable! {
    ItemMerge
        #[doc(alias = "ItemMergeEvent::cancel")]
        #[doc(alias = "ItemMergeEvent::uncancel")]
        #[doc(alias = "ItemMergeEvent::isCancelled")];
}

/// Raised when a dropped item appears in its world.
#[doc(alias = "ItemSpawnEvent")]
#[derive(Debug)]
pub struct ItemSpawn {
    pub(crate) entity: entity::Id,
}

getters!(ItemSpawn {
    /// The dropped item.
    #[doc(alias = "ItemSpawnEvent::getEntity")]
    entity: entity::Id,
});

/// Raised when a projectile hits a block.
#[doc(alias = "ProjectileHitBlockEvent")]
#[doc(alias = "ProjectileHitEvent")]
#[derive(Debug)]
pub struct ProjectileHitBlock {
    pub(crate) entity: entity::Id,
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) hit_position: Vec3,
}

getters!(ProjectileHitBlock {
    /// The projectile.
    #[doc(alias = "ProjectileHitEvent::getEntity")]
    entity: entity::Id,
    /// The world of the block.
    world: world::Id,
    /// Where the block is.
    pos: BlockPos,
    /// The block hit; air where the chunk is not loaded (a projectile sticks at the border of the
    /// loaded chunks).
    #[doc(alias = "ProjectileHitBlockEvent::getBlockHit")]
    block: State,
    /// Where the projectile hit.
    #[doc(alias = "ProjectileHitEvent::getRayTraceResult")]
    hit_position: Vec3,
});

/// Raised when a projectile hits a player or an entity, before the damage.
#[doc(alias = "ProjectileHitEntityEvent")]
#[derive(Debug)]
pub struct ProjectileHitEntity {
    pub(crate) entity: entity::Id,
    pub(crate) entity_hit: Actor,
    pub(crate) hit_position: Vec3,
}

getters!(ProjectileHitEntity {
    /// The projectile.
    entity: entity::Id,
    /// Who is hit.
    #[doc(alias = "ProjectileHitEntityEvent::getEntityHit")]
    entity_hit: Actor,
    /// Where the projectile hit.
    hit_position: Vec3,
});

/// Raised when a projectile is thrown or shot, before it flies. Cancelling removes it.
#[doc(alias = "ProjectileLaunchEvent")]
#[derive(Debug)]
pub struct ProjectileLaunch {
    pub(crate) entity: entity::Id,
    pub(crate) cancelled: bool,
}

getters!(ProjectileLaunch {
    /// The projectile.
    #[doc(alias = "ProjectileLaunchEvent::getEntity")]
    entity: entity::Id,
});

cancellable! {
    ProjectileLaunch
        #[doc(alias = "ProjectileLaunchEvent::cancel")]
        #[doc(alias = "ProjectileLaunchEvent::uncancel")]
        #[doc(alias = "ProjectileLaunchEvent::isCancelled")];
}

/// The modifiers that apply, with their values.
fn applicable(modifiers: &Modifiers) -> impl Iterator<Item = (DamageModifier, f64)> + '_ {
    DamageModifier::all()
        .iter()
        .copied()
        .zip(modifiers)
        .filter_map(|(m, v)| v.map(|v| (m, v)))
}

/// The value of `modifier` in `modifiers`, if it applies.
fn modifier_value(modifiers: &Modifiers, modifier: DamageModifier) -> Option<f64> {
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: a modifier's number is its place in a modifier array"
    )]
    modifiers[modifier as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::block::{AIR, DIRT};
    use crate::effect::Type;
    use crate::event::fixtures::{POS, entity, player, sticks};
    use crate::event::{DEFAULT_FIRE_CHANCE, MAX_EXPLOSION_RADIUS};

    /// A position away from the block grid.
    const AT: Vec3 = Vec3::new(1.5, 2.0, -3.25);
    /// The world the events happen in.
    const WORLD: world::Id = world::Id::DEFAULT;

    // api: AreaEffectCloudApplyEvent, AreaEffectCloudApplyEvent::getEntity,
    // api: AreaEffectCloudApplyEvent::getAffectedEntities,
    // api: AreaEffectCloudApplyEvent::cancel, AreaEffectCloudApplyEvent::uncancel,
    // api: AreaEffectCloudApplyEvent::isCancelled
    #[test]
    fn area_effect_cloud_apply_lists_who_is_affected() {
        let affected = vec![Actor::Player(player(1)), Actor::Entity(entity(2))];
        let mut e = AreaEffectCloudApply {
            entity: entity(1),
            affected: affected.clone(),
            cancelled: false,
        };
        assert_eq!(e.entity(), entity(1));
        assert_eq!(e.affected(), affected.as_slice());
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: EntityBlockChangeEvent, EntityBlockChangeEvent::getBlock,
    // api: EntityBlockChangeEvent::getTo, EntityBlockChangeEvent::cancel,
    // api: EntityBlockChangeEvent::uncancel, EntityBlockChangeEvent::isCancelled
    #[test]
    fn entity_block_change_names_both_states() {
        let mut e = EntityBlockChange {
            entity: entity(1),
            world: WORLD,
            pos: POS,
            block: AIR,
            to: DIRT,
            cancelled: false,
        };
        assert_eq!(
            (e.entity(), e.world(), e.pos(), e.block(), e.to()),
            (entity(1), WORLD, POS, AIR, DIRT)
        );
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: EntityCombustEvent, EntityCombustByBlockEvent, EntityCombustByEntityEvent,
    // api: EntityCombustEvent::getDuration, EntityCombustEvent::setDuration,
    // api: EntityCombustByBlockEvent::getCombuster,
    // api: EntityCombustByEntityEvent::getCombuster, EntityCombustEvent::cancel,
    // api: EntityCombustEvent::uncancel, EntityCombustEvent::isCancelled
    #[test]
    fn entity_combust_duration_can_change() {
        let mut e = EntityCombust {
            entity: Actor::Player(player(1)),
            duration_secs: 8,
            source: CombustSource::Block(POS),
            cancelled: false,
        };
        assert_eq!(e.entity(), Actor::Player(player(1)));
        assert_eq!(e.duration_secs(), 8);
        assert_eq!(e.source(), CombustSource::Block(POS));
        e.set_duration_secs(2);
        assert_eq!(e.duration_secs(), 2);
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    fn none() -> Modifiers {
        [None; DamageModifier::all().len()]
    }

    // api: EntityDamageEvent, EntityDamageByEntityEvent, EntityDamageByBlockEvent,
    // api: EntityDamageByChildEntityEvent, EntityEvent::getEntity, EntityDamageEvent::getCause,
    // api: EntityDamageByEntityEvent::getDamager, EntityDamageByBlockEvent::getDamager,
    // api: EntityDamageByChildEntityEvent::getChild, EntityDamageEvent::getBaseDamage,
    // api: EntityDamageEvent::setBaseDamage, EntityDamageEvent::getOriginalBaseDamage,
    // api: EntityDamageEvent::getModifier, EntityDamageEvent::setModifier,
    // api: EntityDamageEvent::isApplicable, EntityDamageEvent::getModifiers,
    // api: EntityDamageEvent::getOriginalModifier, EntityDamageEvent::getOriginalModifiers,
    // api: EntityDamageEvent::getFinalDamage, EntityDamageEvent::canBeReducedByArmor,
    // api: EntityDamageEvent::getAttackCooldown, EntityDamageEvent::setAttackCooldown,
    // api: EntityDamageByEntityEvent::getKnockBack, EntityDamageByEntityEvent::setKnockBack,
    // api: EntityDamageByEntityEvent::getVerticalKnockBackLimit,
    // api: EntityDamageByEntityEvent::setVerticalKnockBackLimit, EntityDamageEvent::cancel,
    // api: EntityDamageEvent::uncancel, EntityDamageEvent::isCancelled
    #[test]
    fn entity_damage_sums_base_and_modifiers() {
        let mut modifiers = none();
        modifiers[DamageModifier::Armor as usize] = Some(-2.0);
        let source = DamageSource::Child {
            child: entity(3),
            owner: Some(Actor::Player(player(2))),
        };
        let mut e = EntityDamage {
            entity: Actor::Player(player(1)),
            cause: DamageCause::Projectile,
            source,
            base_damage: 6.0,
            original_base_damage: 6.0,
            modifiers,
            original_modifiers: modifiers,
            attack_cooldown_ticks: 10,
            knockback: 0.4,
            vertical_knockback_limit: 0.4,
            cancelled: false,
        };
        assert_eq!(e.entity(), Actor::Player(player(1)));
        assert_eq!(e.cause(), DamageCause::Projectile);
        assert_eq!(e.source(), source);
        assert_eq!(e.final_damage(), 4.0);
        assert!(e.can_be_reduced_by_armor());
        e.set_base_damage(10.0);
        e.set_base_damage(f64::NAN);
        e.set_modifier(DamageModifier::Critical, 1.5);
        e.set_modifier(DamageModifier::Totem, f64::INFINITY);
        assert_eq!((e.base_damage(), e.original_base_damage()), (10.0, 6.0));
        assert_eq!(e.modifier(DamageModifier::Critical), 1.5);
        assert_eq!(e.modifier(DamageModifier::Totem), 0.0);
        assert!(e.is_applicable(DamageModifier::Armor));
        assert!(!e.is_applicable(DamageModifier::Totem));
        assert_eq!(
            e.modifiers().collect::<Vec<_>>(),
            [
                (DamageModifier::Armor, -2.0),
                (DamageModifier::Critical, 1.5)
            ]
        );
        assert_eq!(e.original_modifier(DamageModifier::Critical), 0.0);
        assert_eq!(
            e.original_modifiers().collect::<Vec<_>>(),
            [(DamageModifier::Armor, -2.0)]
        );
        assert_eq!(e.final_damage(), 9.5);
        e.set_modifier(DamageModifier::Resistance, -20.0);
        assert_eq!(e.final_damage(), 0.0);
        e.set_base_damage(-5.0);
        assert_eq!(e.base_damage(), 0.0);
        e.set_base_damage(f64::MAX);
        e.set_modifier(DamageModifier::Resistance, -f64::MAX);
        assert_eq!(e.base_damage(), MAX_DAMAGE);
        assert_eq!(e.modifier(DamageModifier::Resistance), -MAX_DAMAGE);
        e.set_attack_cooldown_ticks(4);
        e.set_knockback(0.8);
        e.set_vertical_knockback_limit(0.1);
        e.set_knockback(f64::NAN);
        assert_eq!(e.attack_cooldown_ticks(), 4);
        assert_eq!((e.knockback(), e.vertical_knockback_limit()), (0.8, 0.1));
        e.set_knockback(f64::MAX);
        e.set_vertical_knockback_limit(-f64::MAX);
        assert_eq!(
            (e.knockback(), e.vertical_knockback_limit()),
            (MAX_KNOCKBACK, -MAX_KNOCKBACK)
        );
        e.cause = DamageCause::Fall;
        assert!(!e.can_be_reduced_by_armor());
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: EntityDamageEvent, EntityDamageEvent::CAUSE_CONTACT,
    // api: EntityDamageEvent::CAUSE_ENTITY_ATTACK, EntityDamageEvent::CAUSE_PROJECTILE,
    // api: EntityDamageEvent::CAUSE_SUFFOCATION, EntityDamageEvent::CAUSE_FALL,
    // api: EntityDamageEvent::CAUSE_FIRE, EntityDamageEvent::CAUSE_FIRE_TICK,
    // api: EntityDamageEvent::CAUSE_LAVA, EntityDamageEvent::CAUSE_DROWNING,
    // api: EntityDamageEvent::CAUSE_BLOCK_EXPLOSION, EntityDamageEvent::CAUSE_ENTITY_EXPLOSION,
    // api: EntityDamageEvent::CAUSE_VOID, EntityDamageEvent::CAUSE_SUICIDE,
    // api: EntityDamageEvent::CAUSE_MAGIC, EntityDamageEvent::CAUSE_CUSTOM,
    // api: EntityDamageEvent::CAUSE_STARVATION, EntityDamageEvent::CAUSE_FALLING_BLOCK
    #[test]
    fn armor_reduces_only_the_causes_it_can() {
        use DamageCause::*;
        let reduced = [
            Contact,
            EntityAttack,
            Projectile,
            Fire,
            Lava,
            BlockExplosion,
            EntityExplosion,
            Custom,
            FallingBlock,
        ];
        let not = [
            Suffocation,
            Fall,
            FireTick,
            Drowning,
            Void,
            Suicide,
            Magic,
            Starvation,
        ];
        for (cause, expected) in reduced
            .map(|c| (c, true))
            .into_iter()
            .chain(not.map(|c| (c, false)))
        {
            let e = EntityDamage {
                entity: Actor::Entity(entity(1)),
                cause,
                source: DamageSource::None,
                base_damage: 1.0,
                original_base_damage: 1.0,
                modifiers: none(),
                original_modifiers: none(),
                attack_cooldown_ticks: 10,
                knockback: 0.4,
                vertical_knockback_limit: 0.4,
                cancelled: false,
            };
            assert_eq!(e.can_be_reduced_by_armor(), expected, "{cause:?}");
        }
    }

    // api: EntityDamageEvent, EntityDamageEvent::MODIFIER_ARMOR,
    // api: EntityDamageEvent::MODIFIER_STRENGTH, EntityDamageEvent::MODIFIER_WEAKNESS,
    // api: EntityDamageEvent::MODIFIER_RESISTANCE, EntityDamageEvent::MODIFIER_ABSORPTION,
    // api: EntityDamageEvent::MODIFIER_ARMOR_ENCHANTMENTS, EntityDamageEvent::MODIFIER_CRITICAL,
    // api: EntityDamageEvent::MODIFIER_TOTEM, EntityDamageEvent::MODIFIER_WEAPON_ENCHANTMENTS,
    // api: EntityDamageEvent::MODIFIER_PREVIOUS_DAMAGE_COOLDOWN,
    // api: EntityDamageEvent::MODIFIER_ARMOR_HELMET
    #[test]
    fn every_modifier_has_its_own_slot() {
        let mut e = EntityDamage {
            entity: Actor::Entity(entity(1)),
            cause: DamageCause::Custom,
            source: DamageSource::Block(POS),
            base_damage: 0.0,
            original_base_damage: 0.0,
            modifiers: none(),
            original_modifiers: none(),
            attack_cooldown_ticks: 10,
            knockback: 0.4,
            vertical_knockback_limit: 0.4,
            cancelled: false,
        };
        for (i, &m) in DamageModifier::all().iter().enumerate() {
            assert_eq!(m as usize, i);
            e.set_modifier(m, 1.0);
        }
        assert_eq!(e.modifiers().count(), DamageModifier::all().len());
        assert_eq!(e.final_damage(), 11.0);
    }

    // api: EntityDeathEvent, EntityDeathEvent::getEntity, EntityDeathEvent::getDrops,
    // api: EntityDeathEvent::setDrops, EntityDeathEvent::getXpDropAmount,
    // api: EntityDeathEvent::setXpDropAmount
    #[test]
    fn entity_death_drops_can_change() {
        let mut e = EntityDeath {
            entity: entity(1),
            drops: vec![sticks(1)],
            xp_drop_amount: 5,
        };
        assert_eq!(e.entity(), entity(1));
        assert_eq!((e.drops(), e.xp_drop_amount()), ([sticks(1)].as_slice(), 5));
        e.set_drops(vec![sticks(2), sticks(3)]);
        e.set_xp_drop_amount(0);
        assert_eq!(
            (e.drops(), e.xp_drop_amount()),
            ([sticks(2), sticks(3)].as_slice(), 0)
        );
    }

    // api: EntitySpawnEvent, EntityDespawnEvent
    #[test]
    fn entities_spawn_and_despawn() {
        assert_eq!(EntitySpawn { entity: entity(4) }.entity(), entity(4));
        assert_eq!(EntityDespawn { entity: entity(5) }.entity(), entity(5));
    }

    // api: EntityEffectEvent, EntityEffectAddEvent, EntityEffectRemoveEvent,
    // api: EntityEffectEvent::getEffect, EntityEffectAddEvent::getOldEffect,
    // api: EntityEffectAddEvent::hasOldEffect, EntityEffectAddEvent::willModify,
    // api: EntityEffectEvent::cancel, EntityEffectEvent::uncancel,
    // api: EntityEffectEvent::isCancelled, EntityEffectRemoveEvent::cancel
    #[test]
    fn effect_events_name_the_effects_and_keep_expired_ones_removed() {
        let new = Instance::new(Type::Speed, 200, 1, true, false);
        let old = Instance::new(Type::Speed, 100, 0, true, false);
        let mut add = EntityEffectAdd {
            entity: Actor::Player(player(1)),
            effect: new,
            old_effect: Some(old),
            cancelled: false,
        };
        assert_eq!(add.entity(), Actor::Player(player(1)));
        assert_eq!((add.effect(), add.old_effect()), (new, Some(old)));
        assert!(add.has_old_effect());
        add.cancel();
        assert!(add.is_cancelled());
        add.uncancel();
        assert!(!add.is_cancelled());
        let mut remove = EntityEffectRemove {
            entity: Actor::Entity(entity(1)),
            effect: old,
            cancelled: false,
        };
        assert_eq!(
            (remove.entity(), remove.effect()),
            (Actor::Entity(entity(1)), old)
        );
        remove.cancel();
        assert!(remove.is_cancelled());
        remove.uncancel();
        assert!(!remove.is_cancelled());
        remove.effect = Instance::new(Type::Speed, 0, 0, true, false);
        remove.cancel();
        assert!(!remove.is_cancelled());
        remove.effect = remove.effect.with_infinite(true);
        remove.cancel();
        assert!(remove.is_cancelled(), "an infinite effect never runs out");
    }

    // api: EntityExplodeEvent, EntityExplodeEvent::getPosition,
    // api: EntityExplodeEvent::getBlockList, EntityExplodeEvent::setBlockList,
    // api: EntityExplodeEvent::getYield, EntityExplodeEvent::setYield,
    // api: EntityExplodeEvent::getIgnitions, EntityExplodeEvent::setIgnitions,
    // api: EntityExplodeEvent::cancel, EntityExplodeEvent::uncancel,
    // api: EntityExplodeEvent::isCancelled
    #[test]
    fn entity_explode_blocks_and_yield_can_change() {
        let mut e = EntityExplode {
            entity: entity(1),
            world: WORLD,
            position: AT,
            blocks: vec![POS],
            yield_percent: 25.0,
            ignitions: Vec::new(),
            cancelled: false,
        };
        assert_eq!(
            (e.entity(), e.world(), e.position()),
            (entity(1), WORLD, AT)
        );
        assert_eq!((e.blocks(), e.yield_percent()), ([POS].as_slice(), 25.0));
        assert!(e.ignitions().is_empty());
        e.set_blocks(Vec::new());
        e.set_ignitions(vec![POS]);
        e.set_yield_percent(250.0);
        assert!(e.blocks().is_empty());
        assert_eq!(e.ignitions(), &[POS]);
        assert_eq!(e.yield_percent(), 100.0);
        e.set_yield_percent(f64::NAN);
        assert_eq!(e.yield_percent(), 100.0);
        e.set_yield_percent(-1.0);
        assert_eq!(e.yield_percent(), 0.0);
        e.set_yield_percent(50.0);
        e.set_yield_percent(f64::INFINITY);
        e.set_yield_percent(f64::NEG_INFINITY);
        assert_eq!(e.yield_percent(), 50.0);
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: EntityExtinguishEvent, EntityExtinguishEvent::getCause,
    // api: EntityExtinguishEvent::CAUSE_WATER, EntityExtinguishEvent::CAUSE_WATER_CAULDRON,
    // api: EntityExtinguishEvent::CAUSE_RESPAWN, EntityExtinguishEvent::CAUSE_FIRE_PROOF,
    // api: EntityExtinguishEvent::CAUSE_TICKING
    #[test]
    fn entity_extinguish_names_the_cause() {
        use ExtinguishCause::*;
        for cause in [Water, WaterCauldron, Respawn, FireProof, Ticking] {
            let e = EntityExtinguish {
                entity: Actor::Player(player(1)),
                cause,
            };
            assert_eq!((e.entity(), e.cause()), (Actor::Player(player(1)), cause));
        }
    }

    // api: EntityItemPickupEvent, EntityItemPickupEvent::getOrigin,
    // api: EntityItemPickupEvent::getItem, EntityItemPickupEvent::setItem,
    // api: EntityItemPickupEvent::cancel, EntityItemPickupEvent::uncancel,
    // api: EntityItemPickupEvent::isCancelled
    #[test]
    fn entity_item_pickup_item_can_change() {
        let mut e = EntityItemPickup {
            entity: Actor::Player(player(1)),
            origin: entity(2),
            item: sticks(4),
            cancelled: false,
        };
        assert_eq!(
            (e.entity(), e.origin()),
            (Actor::Player(player(1)), entity(2))
        );
        assert_eq!(e.item(), &sticks(4));
        e.set_item(sticks(1));
        assert_eq!(e.item(), &sticks(1));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: EntityMotionEvent, EntityMotionEvent::getVector, EntityMotionEvent::cancel,
    // api: EntityMotionEvent::uncancel, EntityMotionEvent::isCancelled
    #[test]
    fn entity_motion_names_the_motion() {
        let mut e = EntityMotion {
            entity: Actor::Entity(entity(1)),
            motion: AT,
            cancelled: false,
        };
        assert_eq!((e.entity(), e.motion()), (Actor::Entity(entity(1)), AT));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: EntityPreExplodeEvent, EntityPreExplodeEvent::getRadius,
    // api: EntityPreExplodeEvent::setRadius, EntityPreExplodeEvent::getFireChance,
    // api: EntityPreExplodeEvent::setFireChance, EntityPreExplodeEvent::isIncendiary,
    // api: EntityPreExplodeEvent::setIncendiary, EntityPreExplodeEvent::isBlockBreaking,
    // api: EntityPreExplodeEvent::setBlockBreaking, EntityPreExplodeEvent::cancel,
    // api: EntityPreExplodeEvent::uncancel, EntityPreExplodeEvent::isCancelled
    #[test]
    fn entity_pre_explode_keeps_its_values_in_range() {
        let mut e = EntityPreExplode {
            entity: entity(1),
            radius: 4.0,
            fire_chance: 0.0,
            block_breaking: true,
            cancelled: false,
        };
        assert_eq!(
            (e.entity(), e.radius(), e.fire_chance()),
            (entity(1), 4.0, 0.0)
        );
        assert!(!e.is_incendiary());
        assert!(e.is_block_breaking());
        e.set_radius(2.5);
        e.set_radius(0.0);
        e.set_radius(f64::INFINITY);
        assert_eq!(e.radius(), 2.5);
        e.set_radius(100.0);
        assert_eq!(e.radius(), MAX_EXPLOSION_RADIUS);
        e.set_incendiary(true);
        assert_eq!(e.fire_chance(), DEFAULT_FIRE_CHANCE);
        e.set_fire_chance(0.75);
        e.set_incendiary(true);
        assert_eq!(e.fire_chance(), 0.75);
        e.set_fire_chance(3.0);
        assert_eq!(e.fire_chance(), 1.0);
        e.set_fire_chance(f64::NAN);
        assert_eq!(e.fire_chance(), 1.0);
        e.set_fire_chance(0.5);
        e.set_fire_chance(f64::INFINITY);
        e.set_fire_chance(f64::NEG_INFINITY);
        assert_eq!(e.fire_chance(), 0.5);
        e.set_incendiary(false);
        assert!(!e.is_incendiary());
        e.set_block_breaking(false);
        assert!(!e.is_block_breaking());
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: EntityRegainHealthEvent, EntityRegainHealthEvent::getAmount,
    // api: EntityRegainHealthEvent::setAmount, EntityRegainHealthEvent::getRegainReason,
    // api: EntityRegainHealthEvent::CAUSE_REGEN, EntityRegainHealthEvent::CAUSE_MAGIC,
    // api: EntityRegainHealthEvent::CAUSE_SATURATION, EntityRegainHealthEvent::cancel,
    // api: EntityRegainHealthEvent::uncancel, EntityRegainHealthEvent::isCancelled
    #[test]
    fn entity_regain_health_amount_can_change() {
        use RegainReason::*;
        for reason in [Regen, Magic, Saturation] {
            let mut e = EntityRegainHealth {
                entity: Actor::Player(player(1)),
                amount: 1.0,
                regain_reason: reason,
                cancelled: false,
            };
            assert_eq!(e.entity(), Actor::Player(player(1)));
            assert_eq!((e.amount(), e.regain_reason()), (1.0, reason));
            e.set_amount(4.0);
            e.set_amount(f64::NAN);
            assert_eq!(e.amount(), 4.0);
            e.cancel();
            assert!(e.is_cancelled());
            e.uncancel();
            assert!(!e.is_cancelled());
        }
    }

    // api: EntityShootBowEvent, EntityShootBowEvent::getEntity, EntityShootBowEvent::getBow,
    // api: EntityShootBowEvent::getProjectile, EntityShootBowEvent::getForce,
    // api: EntityShootBowEvent::setForce, EntityShootBowEvent::cancel,
    // api: EntityShootBowEvent::uncancel, EntityShootBowEvent::isCancelled
    #[test]
    fn entity_shoot_bow_force_can_change() {
        let mut e = EntityShootBow {
            entity: Actor::Player(player(1)),
            bow: sticks(1),
            projectile: entity(2),
            force: 1.0,
            cancelled: false,
        };
        assert_eq!(
            (e.entity(), e.bow()),
            (Actor::Player(player(1)), &sticks(1))
        );
        assert_eq!((e.projectile(), e.force()), (entity(2), 1.0));
        e.set_force(0.5);
        e.set_force(f64::NEG_INFINITY);
        assert_eq!(e.force(), 0.5);
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: EntityTeleportEvent, EntityTeleportEvent::getFrom, EntityTeleportEvent::getTo,
    // api: EntityTeleportEvent::setTo, EntityTeleportEvent::cancel,
    // api: EntityTeleportEvent::uncancel, EntityTeleportEvent::isCancelled
    #[test]
    fn entity_teleport_target_can_change() {
        let mut e = EntityTeleport {
            entity: Actor::Player(player(1)),
            cause: TeleportCause::Command,
            origin_world: WORLD,
            origin: Vec3::ZERO,
            target_world: WORLD,
            target: AT,
            cancelled: false,
        };
        assert_eq!(e.entity(), Actor::Player(player(1)));
        assert_eq!(e.cause(), TeleportCause::Command);
        assert_eq!((e.origin_world(), e.origin()), (WORLD, Vec3::ZERO));
        assert_eq!((e.target_world(), e.target()), (WORLD, AT));
        e.set_target(WORLD, Vec3::new(9.0, 9.0, 9.0));
        e.set_target(WORLD, Vec3::new(f64::NAN, 0.0, 0.0));
        assert_eq!(e.target(), Vec3::new(9.0, 9.0, 9.0));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: ItemDespawnEvent, ItemMergeEvent, ItemSpawnEvent, ItemDespawnEvent::getEntity,
    // api: ItemDespawnEvent::cancel, ItemDespawnEvent::uncancel,
    // api: ItemDespawnEvent::isCancelled, ItemMergeEvent::getTarget, ItemMergeEvent::cancel,
    // api: ItemMergeEvent::uncancel, ItemMergeEvent::isCancelled, ItemSpawnEvent::getEntity
    #[test]
    fn dropped_item_events_name_the_items() {
        let mut despawn = ItemDespawn {
            entity: entity(1),
            cancelled: false,
        };
        assert_eq!(despawn.entity(), entity(1));
        despawn.cancel();
        assert!(despawn.is_cancelled());
        despawn.uncancel();
        assert!(!despawn.is_cancelled());
        let mut merge = ItemMerge {
            entity: entity(1),
            target: entity(2),
            cancelled: false,
        };
        assert_eq!((merge.entity(), merge.target()), (entity(1), entity(2)));
        merge.cancel();
        assert!(merge.is_cancelled());
        merge.uncancel();
        assert!(!merge.is_cancelled());
        assert_eq!(ItemSpawn { entity: entity(3) }.entity(), entity(3));
    }

    // api: ProjectileHitEvent, ProjectileHitBlockEvent, ProjectileHitEntityEvent,
    // api: ProjectileLaunchEvent, ProjectileHitEvent::getEntity,
    // api: ProjectileHitBlockEvent::getBlockHit, ProjectileHitEvent::getRayTraceResult,
    // api: ProjectileHitEntityEvent::getEntityHit, ProjectileLaunchEvent::getEntity,
    // api: ProjectileLaunchEvent::cancel, ProjectileLaunchEvent::uncancel,
    // api: ProjectileLaunchEvent::isCancelled
    #[test]
    fn projectile_events_name_what_was_hit() {
        let block = ProjectileHitBlock {
            entity: entity(1),
            world: WORLD,
            pos: POS,
            block: DIRT,
            hit_position: AT,
        };
        assert_eq!(
            (
                block.entity(),
                block.world(),
                block.pos(),
                block.block(),
                block.hit_position()
            ),
            (entity(1), WORLD, POS, DIRT, AT)
        );
        let hit = ProjectileHitEntity {
            entity: entity(1),
            entity_hit: Actor::Player(player(2)),
            hit_position: AT,
        };
        assert_eq!(
            (hit.entity(), hit.entity_hit(), hit.hit_position()),
            (entity(1), Actor::Player(player(2)), AT)
        );
        let mut launch = ProjectileLaunch {
            entity: entity(1),
            cancelled: false,
        };
        assert_eq!(launch.entity(), entity(1));
        launch.cancel();
        assert!(launch.is_cancelled());
        launch.uncancel();
        assert!(!launch.is_cancelled());
    }

    #[test]
    fn a_set_motion_is_cut_to_the_speed_bound_as_it_is_set() {
        let mut e = EntityMotion {
            entity: Actor::Entity(entity(1)),
            motion: Vec3::ZERO,
            cancelled: false,
        };
        e.set_motion(Vec3::new(30.0, 0.5, -30.0));
        assert_eq!(e.motion(), Vec3::new(10.0, 0.5, -10.0));
        e.set_motion(Vec3::new(f64::NAN, 1.0, 1.0));
        assert_eq!(e.motion(), Vec3::new(10.0, 0.5, -10.0), "ignored");
    }
}
