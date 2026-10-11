//! Entity events (PMMP `pocketmine\event\entity`): damage, death, spawning, explosions and the
//! other things that happen to entities.
//!
//! PMMP's entities include players, so most of these events name an [`Actor`]. PMMP's subclass
//! trees are one event here with an enum for the subclass: [`EntityDamage`] has a
//! [`DamageSource`] (PMMP `EntityDamageByEntityEvent` and the others), [`EntityCombust`] a
//! [`CombustSource`].

use std::num::NonZeroU8;

use super::Actor;
use crate::block::State;
use crate::effect::Instance;
use crate::entity;
use crate::item::Stack;
use crate::math::{self, Vec3};
use crate::world::{self, BlockPos};

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

/// What sets something on fire (PMMP `EntityCombustByBlockEvent`, `EntityCombustByEntityEvent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CombustSource {
    /// Nothing in particular.
    None,
    /// A block: fire, lava or a lava cauldron at this position.
    Block(BlockPos),
    /// An entity: a burning arrow.
    Entity(Actor),
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
    /// Sets how long it burns, in seconds.
    #[doc(alias = "EntityCombustEvent::setDuration")]
    set_duration_secs => duration_secs: u32,
});

/// Why a player or an entity takes damage (PMMP `EntityDamageEvent::CAUSE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DamageCause {
    /// Touching a block that hurts: cactus, berry bush, magma.
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
    /// Standing in fire.
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
    /// An exploding block.
    #[doc(alias = "EntityDamageEvent::CAUSE_BLOCK_EXPLOSION")]
    BlockExplosion,
    /// An exploding entity (TNT).
    #[doc(alias = "EntityDamageEvent::CAUSE_ENTITY_EXPLOSION")]
    EntityExplosion,
    /// Below the world.
    #[doc(alias = "EntityDamageEvent::CAUSE_VOID")]
    Void,
    /// The player killed itself (`kill` on oneself).
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

/// A change to damage on top of the base damage (PMMP `EntityDamageEvent::MODIFIER_*`). The final
/// damage is the base damage plus every applicable modifier, so a reduction is negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DamageModifier {
    /// Armour points.
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
    /// Protection enchantments on the armour.
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
    pub const ALL: [Self; 11] = [
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
    ];
}

/// The modifiers of a hit, by [`DamageModifier`] order; `None` where one does not apply.
pub(crate) type Modifiers = [Option<f64>; DamageModifier::ALL.len()];

/// What deals damage (PMMP `EntityDamageByBlockEvent`, `EntityDamageByEntityEvent`,
/// `EntityDamageByChildEntityEvent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DamageSource {
    /// Nothing in particular: a fall, drowning, hunger, an effect.
    None,
    /// The block at this position: a cactus, magma, a campfire.
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

/// Raised when a player or an entity takes damage, before its health changes. Cancelling keeps
/// the health, and no knockback follows.
///
/// A hit within the attack cooldown that is no stronger than the last is raised too, already
/// cancelled, while the event has listeners (not only monitors), as PMMP's `Living::attack` does: a
/// listener that uncancels it lets it land as a stronger hit within the cooldown lands, with the
/// previous damage counted off ([`DamageModifier::PreviousDamageCooldown`]). Every other hit that
/// cannot land (a dead or creative target, fire against fire resistance) raises nothing.
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
    pub(crate) knockback_motion: Option<(Vec3, NonZeroU8)>,
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
    /// The motion that replaces the knockback and how many times it is sent, if a listener set
    /// one.
    knockback_motion: Option<(Vec3, NonZeroU8)>,
});

setters!(EntityDamage {
    /// Sets the ticks within which a weaker hit does no damage.
    #[doc(alias = "EntityDamageEvent::setAttackCooldown")]
    set_attack_cooldown_ticks => attack_cooldown_ticks: u32,
});

impl EntityDamage {
    /// Sets the damage before the modifiers, at least 0. A value that is not finite is ignored.
    #[doc(alias = "EntityDamageEvent::setBaseDamage")]
    pub fn set_base_damage(&mut self, damage: f64) {
        if damage.is_finite() {
            // The base damage is remembered for the attack cooldown, where a negative one would
            // add to the next hit; reduce the damage with a modifier instead.
            self.base_damage = math::max(damage, 0.0);
        }
    }

    /// The value of `modifier`, 0 when it does not apply.
    #[doc(alias = "EntityDamageEvent::getModifier")]
    pub fn modifier(&self, modifier: DamageModifier) -> f64 {
        value(&self.modifiers, modifier).unwrap_or(0.0)
    }

    /// Sets `modifier` to `damage` (negative to reduce), so that it applies. A value that is not
    /// finite is ignored.
    #[doc(alias = "EntityDamageEvent::setModifier")]
    pub fn set_modifier(&mut self, modifier: DamageModifier, damage: f64) {
        if let Some(slot) = self.modifiers.get_mut(modifier as usize)
            && damage.is_finite()
        {
            *slot = Some(damage);
        }
    }

    /// Whether `modifier` applies to this damage.
    #[doc(alias = "EntityDamageEvent::isApplicable")]
    pub fn is_applicable(&self, modifier: DamageModifier) -> bool {
        value(&self.modifiers, modifier).is_some()
    }

    /// The modifiers that apply, with their values, in [`DamageModifier`] order.
    #[doc(alias = "EntityDamageEvent::getModifiers")]
    pub fn modifiers(&self) -> impl Iterator<Item = (DamageModifier, f64)> + '_ {
        applicable(&self.modifiers)
    }

    /// The value `modifier` started with, 0 when it did not apply.
    #[doc(alias = "EntityDamageEvent::getOriginalModifier")]
    pub fn original_modifier(&self, modifier: DamageModifier) -> f64 {
        value(&self.original_modifiers, modifier).unwrap_or(0.0)
    }

    /// The modifiers the event started with, in [`DamageModifier`] order.
    #[doc(alias = "EntityDamageEvent::getOriginalModifiers")]
    pub fn original_modifiers(&self) -> impl Iterator<Item = (DamageModifier, f64)> + '_ {
        applicable(&self.original_modifiers)
    }

    /// The damage dealt: the base damage plus the modifiers, at least 0.
    #[doc(alias = "EntityDamageEvent::getFinalDamage")]
    pub fn final_damage(&self) -> f64 {
        let total = applicable(&self.modifiers).fold(self.base_damage, |sum, (_, v)| sum + v);
        // Finite parts add up to at most an infinity of one sign, never NaN.
        math::clamp(total, 0.0, f64::MAX)
    }

    /// Whether armour reduces damage of this cause.
    #[doc(alias = "EntityDamageEvent::canBeReducedByArmor")]
    pub fn can_be_reduced_by_armor(&self) -> bool {
        !matches!(
            self.cause,
            DamageCause::FireTick
                | DamageCause::Suffocation
                | DamageCause::Drowning
                | DamageCause::Starvation
                | DamageCause::Fall
                | DamageCause::Void
                | DamageCause::Magic
                | DamageCause::Suicide
        )
    }

    /// Sets the knockback force of a hit. A value that is not finite is ignored.
    #[doc(alias = "EntityDamageByEntityEvent::setKnockBack")]
    pub fn set_knockback(&mut self, force: f64) {
        if force.is_finite() {
            self.knockback = force;
        }
    }

    /// Sets the most a hit knocks upward. A value that is not finite is ignored.
    #[doc(alias = "EntityDamageByEntityEvent::setVerticalKnockBackLimit")]
    pub fn set_vertical_knockback_limit(&mut self, limit: f64) {
        if limit.is_finite() {
            self.vertical_knockback_limit = limit;
        }
    }

    /// Replaces the knockback with `motion`, in blocks a tick: a hit that lands and leaves the
    /// target alive pushes it by exactly that, within the attack cooldown too, and the weapon's
    /// Knockback pushes no more. A knockback that keeps part of the target's own motion, as BDS's
    /// does, is built from it. The target and the players that see it are sent the motion
    /// `sends` times in a row (BDS sends a knockback with extra force twice). A motion that is not
    /// finite is ignored.
    pub fn set_knockback_motion(&mut self, motion: Vec3, sends: NonZeroU8) {
        if motion.is_finite() {
            self.knockback_motion = Some((motion, sends));
        }
    }
}

/// The value of `modifier` in `modifiers`, if it applies.
fn value(modifiers: &Modifiers, modifier: DamageModifier) -> Option<f64> {
    modifiers.get(modifier as usize).copied().flatten()
}

/// The modifiers that apply, with their values.
fn applicable(modifiers: &Modifiers) -> impl Iterator<Item = (DamageModifier, f64)> + '_ {
    DamageModifier::ALL
        .into_iter()
        .zip(modifiers)
        .filter_map(|(m, v)| v.map(|v| (m, v)))
}

/// Raised when an entity that is not a player dies, before it drops what it carries.
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
    /// The experience it drops.
    #[doc(alias = "EntityDeathEvent::getXpDropAmount")]
    xp_drop_amount: u32,
});

setters!(EntityDeath {
    /// Replaces what it drops.
    #[doc(alias = "EntityDeathEvent::setDrops")]
    set_drops => drops: Vec<Stack>,
    /// Replaces the experience it drops.
    #[doc(alias = "EntityDeathEvent::setXpDropAmount")]
    set_xp_drop_amount => xp_drop_amount: u32,
});

/// Raised when an entity that is not a player leaves its world, just before it goes.
#[doc(alias = "EntityDespawnEvent")]
#[derive(Debug)]
pub struct EntityDespawn {
    pub(crate) entity: entity::Id,
}

getters!(EntityDespawn {
    /// The entity.
    entity: entity::Id,
});

/// Raised when a player or an entity gets an effect, or a stronger or longer one replaces the one
/// it has. Cancelling keeps the effects as they were.
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

/// Raised when an effect ends on a player or an entity: it ran out, or milk or code removed it.
/// Cancelling keeps the effect, unless it ran out.
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
    /// Keeps the effect. An effect that ran out cannot stay, so then nothing happens.
    #[doc(alias = "EntityEffectRemoveEvent::cancel")]
    pub fn cancel(&mut self) {
        if self.effect.duration_ticks() > 0 {
            self.cancelled = true;
        }
    }

    /// Undoes [`cancel`](Self::cancel).
    pub fn uncancel(&mut self) {
        self.cancelled = false;
    }

    /// Whether a listener cancelled the removal.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

/// Raised when an entity explodes (primed TNT), after the blocks it breaks are known and before
/// they break. Cancelling keeps the blocks; the entity is gone either way.
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
    /// The centre of the explosion.
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
    /// Replaces the blocks that break. Blocks outside loaded chunks are left alone.
    #[doc(alias = "EntityExplodeEvent::setBlockList")]
    set_blocks => blocks: Vec<BlockPos>,
    /// Replaces the blocks that catch fire.
    #[doc(alias = "EntityExplodeEvent::setIgnitions")]
    set_ignitions => ignitions: Vec<BlockPos>,
});

impl EntityExplode {
    /// Sets the chance in percent that a broken block drops its items, limited to 0 to 100. A
    /// value that is not a number is ignored.
    #[doc(alias = "EntityExplodeEvent::setYield")]
    pub fn set_yield_percent(&mut self, percent: f64) {
        if !percent.is_nan() {
            self.yield_percent = math::clamp(percent, 0.0, 100.0);
        }
    }
}

/// The largest radius [`EntityFrostWalker::set_radius`] takes: a square of 33 × 33 blocks (PMMP
/// has no limit; Frost Walker II freezes a radius of 4).
pub const MAX_FROST_WALKER_RADIUS: u32 = 16;

/// Raised when a player wearing Frost Walker boots moves, before the water around its feet
/// freezes. Cancelling freezes nothing.
#[doc(alias = "EntityFrostWalkerEvent")]
#[derive(Debug)]
pub struct EntityFrostWalker {
    pub(crate) entity: Actor,
    pub(crate) world: world::Id,
    pub(crate) radius: u32,
    pub(crate) liquid: State,
    pub(crate) target_block: State,
    pub(crate) cancelled: bool,
}

getters!(EntityFrostWalker {
    /// Who walks.
    entity: Actor,
    /// The world it walks in.
    world: world::Id,
    /// How far from the block under its feet the water freezes, in blocks along each axis.
    #[doc(alias = "EntityFrostWalkerEvent::getRadius")]
    radius: u32,
    /// The liquid that freezes: a water source.
    #[doc(alias = "EntityFrostWalkerEvent::getLiquid")]
    liquid: State,
    /// What the liquid becomes: frosted ice.
    #[doc(alias = "EntityFrostWalkerEvent::getTargetBlock")]
    target_block: State,
});

impl EntityFrostWalker {
    /// Sets the radius. One above [`MAX_FROST_WALKER_RADIUS`] is ignored.
    #[doc(alias = "EntityFrostWalkerEvent::setRadius")]
    pub fn set_radius(&mut self, radius: u32) {
        if radius <= MAX_FROST_WALKER_RADIUS {
            self.radius = radius;
        }
    }

    /// Sets the liquid that freezes: any state of water or lava. Another block is ignored.
    #[doc(alias = "EntityFrostWalkerEvent::setLiquid")]
    pub fn set_liquid(&mut self, liquid: State) {
        if liquid.is(crate::block::Class::Liquid) {
            self.liquid = liquid;
        }
    }
}

setters!(EntityFrostWalker {
    /// Sets what the liquid becomes.
    #[doc(alias = "EntityFrostWalkerEvent::setTargetBlock")]
    set_target_block => target_block: State,
});

/// Why a fire went out (PMMP `EntityExtinguishEvent::CAUSE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExtinguishCause {
    /// Code put it out.
    #[doc(alias = "EntityExtinguishEvent::CAUSE_CUSTOM")]
    Custom,
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
    /// Rain.
    #[doc(alias = "EntityExtinguishEvent::CAUSE_RAIN")]
    Rain,
    /// Powder snow.
    #[doc(alias = "EntityExtinguishEvent::CAUSE_POWDER_SNOW")]
    PowderSnow,
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

/// Raised when a player picks up a dropped item, an arrow or a thrown trident (not an experience
/// orb, as in PMMP). Cancelling leaves it where it is.
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

/// Raised when a player or an entity is pushed: knockback, an explosion. Cancelling keeps its
/// motion.
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

/// Raised when an entity is about to explode (primed TNT), before the blocks it breaks are found.
/// Cancelling stops the explosion; the entity is gone either way.
#[doc(alias = "EntityPreExplodeEvent")]
#[derive(Debug)]
pub struct EntityPreExplode {
    pub(crate) entity: entity::Id,
    pub(crate) radius: f64,
    pub(crate) fire_chance: f64,
    pub(crate) block_breaking: bool,
    pub(crate) cancelled: bool,
}

/// PMMP `Explosion::DEFAULT_FIRE_CHANCE`: the share of destroyed blocks an incendiary explosion
/// sets on fire, which [`EntityPreExplode::set_incendiary`],
/// [`BlockPreExplode::set_incendiary`](super::BlockPreExplode::set_incendiary) and the
/// explosion rules use.
pub(crate) const DEFAULT_FIRE_CHANCE: f64 = 1.0 / 3.0;

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

impl EntityPreExplode {
    /// Sets the radius of the explosion. A value that is not above 0 is ignored; Anchor's
    /// explosions reach at most 8 blocks, and a larger radius stops there.
    #[doc(alias = "EntityPreExplodeEvent::setRadius")]
    pub fn set_radius(&mut self, radius: f64) {
        if radius.is_finite() && radius > 0.0 {
            self.radius = radius;
        }
    }

    /// Sets the chance that a block catches fire, limited to 0 to 1. A value that is not a number
    /// is ignored.
    #[doc(alias = "EntityPreExplodeEvent::setFireChance")]
    pub fn set_fire_chance(&mut self, chance: f64) {
        if !chance.is_nan() {
            self.fire_chance = math::clamp(chance, 0.0, 1.0);
        }
    }

    /// Whether the explosion sets fires.
    #[doc(alias = "EntityPreExplodeEvent::isIncendiary")]
    pub fn is_incendiary(&self) -> bool {
        self.fire_chance > 0.0
    }

    /// Makes the explosion set fires (with a chance of 1/3 unless one is set) or none.
    #[doc(alias = "EntityPreExplodeEvent::setIncendiary")]
    pub fn set_incendiary(&mut self, incendiary: bool) {
        if !incendiary {
            self.fire_chance = 0.0;
        } else if self.fire_chance <= 0.0 {
            self.fire_chance = DEFAULT_FIRE_CHANCE;
        }
    }

    /// Whether the explosion breaks blocks.
    #[doc(alias = "EntityPreExplodeEvent::isBlockBreaking")]
    pub fn is_block_breaking(&self) -> bool {
        self.block_breaking
    }
}

setters!(EntityPreExplode {
    /// Sets whether the explosion breaks blocks.
    #[doc(alias = "EntityPreExplodeEvent::setBlockBreaking")]
    set_block_breaking => block_breaking: bool,
});

/// Why a player or an entity heals (PMMP `EntityRegainHealthEvent::CAUSE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegainReason {
    /// The Regeneration effect.
    #[doc(alias = "EntityRegainHealthEvent::CAUSE_REGEN")]
    Regen,
    /// Eating (a cake slice).
    #[doc(alias = "EntityRegainHealthEvent::CAUSE_EATING")]
    Eating,
    /// An effect or a potion: instant health.
    #[doc(alias = "EntityRegainHealthEvent::CAUSE_MAGIC")]
    Magic,
    /// Code healed it.
    #[doc(alias = "EntityRegainHealthEvent::CAUSE_CUSTOM")]
    Custom,
    /// A full food bar.
    #[doc(alias = "EntityRegainHealthEvent::CAUSE_SATURATION")]
    Saturation,
}

/// Raised when a player or an entity heals. Cancelling keeps its health.
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
    /// Sets the arrow's speed in blocks per tick. A value that is not finite is ignored.
    #[doc(alias = "EntityShootBowEvent::setForce")]
    pub fn set_force(&mut self, force: f64) {
        if force.is_finite() {
            self.force = force;
        }
    }
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
    pub(crate) origin_world: world::Id,
    pub(crate) origin: Vec3,
    pub(crate) target_world: world::Id,
    pub(crate) target: Vec3,
    pub(crate) cancelled: bool,
}

getters!(EntityTeleport {
    /// Who teleports.
    entity: Actor,
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
    /// Sends it elsewhere. A position that is not finite is ignored.
    #[doc(alias = "EntityTeleportEvent::setTo")]
    pub fn set_target(&mut self, world: world::Id, target: Vec3) {
        if target.is_finite() {
            self.target_world = world;
            self.target = target;
        }
    }
}

/// Raised when a player or an entity lands on farmland hard enough to trample it to dirt.
/// Cancelling keeps the farmland.
#[doc(alias = "EntityTrampleFarmlandEvent")]
#[derive(Debug)]
pub struct EntityTrampleFarmland {
    pub(crate) entity: Actor,
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) cancelled: bool,
}

getters!(EntityTrampleFarmland {
    /// Who lands.
    entity: Actor,
    /// The world of the farmland.
    world: world::Id,
    /// Where the farmland is.
    pos: BlockPos,
    /// The farmland.
    #[doc(alias = "EntityTrampleFarmlandEvent::getBlock")]
    block: State,
});

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
    /// The block hit.
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
    AreaEffectCloudApply
        #[doc(alias = "AreaEffectCloudApplyEvent::cancel")]
        #[doc(alias = "AreaEffectCloudApplyEvent::uncancel")]
        #[doc(alias = "AreaEffectCloudApplyEvent::isCancelled")];
    EntityBlockChange
        #[doc(alias = "EntityBlockChangeEvent::cancel")]
        #[doc(alias = "EntityBlockChangeEvent::uncancel")]
        #[doc(alias = "EntityBlockChangeEvent::isCancelled")];
    EntityCombust
        #[doc(alias = "EntityCombustEvent::cancel")]
        #[doc(alias = "EntityCombustEvent::uncancel")]
        #[doc(alias = "EntityCombustEvent::isCancelled")];
    EntityDamage
        #[doc(alias = "EntityDamageEvent::cancel")]
        #[doc(alias = "EntityDamageEvent::uncancel")]
        #[doc(alias = "EntityDamageEvent::isCancelled")];
    EntityEffectAdd
        #[doc(alias = "EntityEffectEvent::cancel")]
        #[doc(alias = "EntityEffectEvent::uncancel")]
        #[doc(alias = "EntityEffectEvent::isCancelled")];
    EntityExplode
        #[doc(alias = "EntityExplodeEvent::cancel")]
        #[doc(alias = "EntityExplodeEvent::uncancel")]
        #[doc(alias = "EntityExplodeEvent::isCancelled")];
    EntityFrostWalker
        #[doc(alias = "EntityFrostWalkerEvent::cancel")]
        #[doc(alias = "EntityFrostWalkerEvent::uncancel")]
        #[doc(alias = "EntityFrostWalkerEvent::isCancelled")];
    EntityItemPickup
        #[doc(alias = "EntityItemPickupEvent::cancel")]
        #[doc(alias = "EntityItemPickupEvent::uncancel")]
        #[doc(alias = "EntityItemPickupEvent::isCancelled")];
    EntityMotion
        #[doc(alias = "EntityMotionEvent::cancel")]
        #[doc(alias = "EntityMotionEvent::uncancel")]
        #[doc(alias = "EntityMotionEvent::isCancelled")];
    EntityPreExplode
        #[doc(alias = "EntityPreExplodeEvent::cancel")]
        #[doc(alias = "EntityPreExplodeEvent::uncancel")]
        #[doc(alias = "EntityPreExplodeEvent::isCancelled")];
    EntityRegainHealth
        #[doc(alias = "EntityRegainHealthEvent::cancel")]
        #[doc(alias = "EntityRegainHealthEvent::uncancel")]
        #[doc(alias = "EntityRegainHealthEvent::isCancelled")];
    EntityShootBow
        #[doc(alias = "EntityShootBowEvent::cancel")]
        #[doc(alias = "EntityShootBowEvent::uncancel")]
        #[doc(alias = "EntityShootBowEvent::isCancelled")];
    EntityTeleport
        #[doc(alias = "EntityTeleportEvent::cancel")]
        #[doc(alias = "EntityTeleportEvent::uncancel")]
        #[doc(alias = "EntityTeleportEvent::isCancelled")];
    EntityTrampleFarmland
        #[doc(alias = "EntityTrampleFarmlandEvent::cancel")]
        #[doc(alias = "EntityTrampleFarmlandEvent::uncancel")]
        #[doc(alias = "EntityTrampleFarmlandEvent::isCancelled")];
    ItemDespawn
        #[doc(alias = "ItemDespawnEvent::cancel")]
        #[doc(alias = "ItemDespawnEvent::uncancel")]
        #[doc(alias = "ItemDespawnEvent::isCancelled")];
    ItemMerge
        #[doc(alias = "ItemMergeEvent::cancel")]
        #[doc(alias = "ItemMergeEvent::uncancel")]
        #[doc(alias = "ItemMergeEvent::isCancelled")];
    ProjectileLaunch
        #[doc(alias = "ProjectileLaunchEvent::cancel")]
        #[doc(alias = "ProjectileLaunchEvent::uncancel")]
        #[doc(alias = "ProjectileLaunchEvent::isCancelled")];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{AIR, DIRT};
    use crate::effect::Type;
    use crate::event::fixtures::{POS, entity, player, sticks};

    const WORLD: world::Id = world::Id::DEFAULT;
    const AT: Vec3 = Vec3::new(1.5, 2.0, -3.25);

    fn none() -> Modifiers {
        [None; DamageModifier::ALL.len()]
    }

    // pmmp: AreaEffectCloudApplyEvent, AreaEffectCloudApplyEvent::getEntity, AreaEffectCloudApplyEvent::getAffectedEntities, AreaEffectCloudApplyEvent::cancel, AreaEffectCloudApplyEvent::uncancel, AreaEffectCloudApplyEvent::isCancelled
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

    // pmmp: EntityBlockChangeEvent, EntityBlockChangeEvent::getBlock, EntityBlockChangeEvent::getTo, EntityBlockChangeEvent::cancel, EntityBlockChangeEvent::uncancel, EntityBlockChangeEvent::isCancelled
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

    // pmmp: EntityCombustEvent, EntityCombustByBlockEvent, EntityCombustByEntityEvent, EntityCombustEvent::getDuration, EntityCombustEvent::setDuration, EntityCombustByBlockEvent::getCombuster, EntityCombustByEntityEvent::getCombuster, EntityCombustEvent::cancel, EntityCombustEvent::uncancel, EntityCombustEvent::isCancelled
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

    // pmmp: EntityDamageEvent, EntityDamageByEntityEvent, EntityDamageByBlockEvent, EntityDamageByChildEntityEvent, EntityEvent::getEntity, EntityDamageEvent::getCause, EntityDamageByEntityEvent::getDamager, EntityDamageByBlockEvent::getDamager, EntityDamageByChildEntityEvent::getChild, EntityDamageEvent::getBaseDamage, EntityDamageEvent::setBaseDamage, EntityDamageEvent::getOriginalBaseDamage, EntityDamageEvent::getModifier, EntityDamageEvent::setModifier, EntityDamageEvent::isApplicable, EntityDamageEvent::getModifiers, EntityDamageEvent::getOriginalModifier, EntityDamageEvent::getOriginalModifiers, EntityDamageEvent::getFinalDamage, EntityDamageEvent::canBeReducedByArmor, EntityDamageEvent::getAttackCooldown, EntityDamageEvent::setAttackCooldown, EntityDamageByEntityEvent::getKnockBack, EntityDamageByEntityEvent::setKnockBack, EntityDamageByEntityEvent::getVerticalKnockBackLimit, EntityDamageByEntityEvent::setVerticalKnockBackLimit, EntityDamageEvent::cancel, EntityDamageEvent::uncancel, EntityDamageEvent::isCancelled
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
            knockback_motion: None,
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
        e.set_modifier(DamageModifier::Resistance, f64::MAX);
        assert_eq!(e.final_damage(), f64::MAX);
        e.set_attack_cooldown_ticks(4);
        e.set_knockback(0.8);
        e.set_vertical_knockback_limit(0.1);
        e.set_knockback(f64::NAN);
        assert_eq!(e.attack_cooldown_ticks(), 4);
        assert_eq!((e.knockback(), e.vertical_knockback_limit()), (0.8, 0.1));
        assert_eq!(e.knockback_motion(), None);
        let twice = NonZeroU8::new(2).unwrap();
        e.set_knockback_motion(AT, twice);
        e.set_knockback_motion(Vec3::new(f64::NAN, 0.0, 0.0), NonZeroU8::MIN);
        assert_eq!(e.knockback_motion(), Some((AT, twice)));
        e.cause = DamageCause::Fall;
        assert!(!e.can_be_reduced_by_armor());
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: EntityDamageEvent, EntityDamageEvent::CAUSE_CONTACT, EntityDamageEvent::CAUSE_ENTITY_ATTACK, EntityDamageEvent::CAUSE_PROJECTILE, EntityDamageEvent::CAUSE_SUFFOCATION, EntityDamageEvent::CAUSE_FALL, EntityDamageEvent::CAUSE_FIRE, EntityDamageEvent::CAUSE_FIRE_TICK, EntityDamageEvent::CAUSE_LAVA, EntityDamageEvent::CAUSE_DROWNING, EntityDamageEvent::CAUSE_BLOCK_EXPLOSION, EntityDamageEvent::CAUSE_ENTITY_EXPLOSION, EntityDamageEvent::CAUSE_VOID, EntityDamageEvent::CAUSE_SUICIDE, EntityDamageEvent::CAUSE_MAGIC, EntityDamageEvent::CAUSE_CUSTOM, EntityDamageEvent::CAUSE_STARVATION, EntityDamageEvent::CAUSE_FALLING_BLOCK
    #[test]
    fn armor_reduces_the_causes_pmmp_lets_it_reduce() {
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
                knockback_motion: None,
                cancelled: false,
            };
            assert_eq!(e.can_be_reduced_by_armor(), expected, "{cause:?}");
        }
    }

    // pmmp: EntityDamageEvent, EntityDamageEvent::MODIFIER_ARMOR, EntityDamageEvent::MODIFIER_STRENGTH, EntityDamageEvent::MODIFIER_WEAKNESS, EntityDamageEvent::MODIFIER_RESISTANCE, EntityDamageEvent::MODIFIER_ABSORPTION, EntityDamageEvent::MODIFIER_ARMOR_ENCHANTMENTS, EntityDamageEvent::MODIFIER_CRITICAL, EntityDamageEvent::MODIFIER_TOTEM, EntityDamageEvent::MODIFIER_WEAPON_ENCHANTMENTS, EntityDamageEvent::MODIFIER_PREVIOUS_DAMAGE_COOLDOWN, EntityDamageEvent::MODIFIER_ARMOR_HELMET
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
            knockback_motion: None,
            cancelled: false,
        };
        for (i, m) in DamageModifier::ALL.into_iter().enumerate() {
            assert_eq!(m as usize, i);
            e.set_modifier(m, 1.0);
        }
        assert_eq!(e.modifiers().count(), DamageModifier::ALL.len());
        assert_eq!(e.final_damage(), 11.0);
    }

    // pmmp: EntityDeathEvent, EntityDeathEvent::getEntity, EntityDeathEvent::getDrops, EntityDeathEvent::setDrops, EntityDeathEvent::getXpDropAmount, EntityDeathEvent::setXpDropAmount
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

    // pmmp: EntitySpawnEvent, EntityDespawnEvent
    #[test]
    fn entities_spawn_and_despawn() {
        assert_eq!(EntitySpawn { entity: entity(4) }.entity(), entity(4));
        assert_eq!(EntityDespawn { entity: entity(5) }.entity(), entity(5));
    }

    // pmmp: EntityEffectEvent, EntityEffectAddEvent, EntityEffectRemoveEvent, EntityEffectEvent::getEffect, EntityEffectAddEvent::getOldEffect, EntityEffectAddEvent::hasOldEffect, EntityEffectAddEvent::willModify, EntityEffectEvent::cancel, EntityEffectEvent::uncancel, EntityEffectEvent::isCancelled, EntityEffectRemoveEvent::cancel
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
    }

    // pmmp: EntityExplodeEvent, EntityExplodeEvent::getPosition, EntityExplodeEvent::getBlockList, EntityExplodeEvent::setBlockList, EntityExplodeEvent::getYield, EntityExplodeEvent::setYield, EntityExplodeEvent::getIgnitions, EntityExplodeEvent::setIgnitions, EntityExplodeEvent::cancel, EntityExplodeEvent::uncancel, EntityExplodeEvent::isCancelled
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
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: EntityExtinguishEvent, EntityExtinguishEvent::getCause, EntityExtinguishEvent::CAUSE_CUSTOM, EntityExtinguishEvent::CAUSE_WATER, EntityExtinguishEvent::CAUSE_WATER_CAULDRON, EntityExtinguishEvent::CAUSE_RESPAWN, EntityExtinguishEvent::CAUSE_FIRE_PROOF, EntityExtinguishEvent::CAUSE_TICKING, EntityExtinguishEvent::CAUSE_RAIN, EntityExtinguishEvent::CAUSE_POWDER_SNOW
    #[test]
    fn entity_extinguish_names_the_cause() {
        use ExtinguishCause::*;
        for cause in [
            Custom,
            Water,
            WaterCauldron,
            Respawn,
            FireProof,
            Ticking,
            Rain,
            PowderSnow,
        ] {
            let e = EntityExtinguish {
                entity: Actor::Player(player(1)),
                cause,
            };
            assert_eq!((e.entity(), e.cause()), (Actor::Player(player(1)), cause));
        }
    }

    // pmmp: EntityItemPickupEvent, EntityItemPickupEvent::getOrigin, EntityItemPickupEvent::getItem, EntityItemPickupEvent::setItem, EntityItemPickupEvent::cancel, EntityItemPickupEvent::uncancel, EntityItemPickupEvent::isCancelled
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

    // pmmp: EntityMotionEvent, EntityMotionEvent::getVector, EntityMotionEvent::cancel, EntityMotionEvent::uncancel, EntityMotionEvent::isCancelled
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

    // pmmp: EntityPreExplodeEvent, EntityPreExplodeEvent::getRadius, EntityPreExplodeEvent::setRadius, EntityPreExplodeEvent::getFireChance, EntityPreExplodeEvent::setFireChance, EntityPreExplodeEvent::isIncendiary, EntityPreExplodeEvent::setIncendiary, EntityPreExplodeEvent::isBlockBreaking, EntityPreExplodeEvent::setBlockBreaking, EntityPreExplodeEvent::cancel, EntityPreExplodeEvent::uncancel, EntityPreExplodeEvent::isCancelled
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
        e.set_incendiary(true);
        assert_eq!(e.fire_chance(), DEFAULT_FIRE_CHANCE);
        e.set_fire_chance(0.75);
        e.set_incendiary(true);
        assert_eq!(e.fire_chance(), 0.75);
        e.set_fire_chance(3.0);
        assert_eq!(e.fire_chance(), 1.0);
        e.set_fire_chance(f64::NAN);
        assert_eq!(e.fire_chance(), 1.0);
        e.set_incendiary(false);
        assert!(!e.is_incendiary());
        e.set_block_breaking(false);
        assert!(!e.is_block_breaking());
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: EntityRegainHealthEvent, EntityRegainHealthEvent::getAmount, EntityRegainHealthEvent::setAmount, EntityRegainHealthEvent::getRegainReason, EntityRegainHealthEvent::CAUSE_REGEN, EntityRegainHealthEvent::CAUSE_EATING, EntityRegainHealthEvent::CAUSE_MAGIC, EntityRegainHealthEvent::CAUSE_CUSTOM, EntityRegainHealthEvent::CAUSE_SATURATION, EntityRegainHealthEvent::cancel, EntityRegainHealthEvent::uncancel, EntityRegainHealthEvent::isCancelled
    #[test]
    fn entity_regain_health_amount_can_change() {
        use RegainReason::*;
        for reason in [Regen, Eating, Magic, Custom, Saturation] {
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

    // pmmp: EntityShootBowEvent, EntityShootBowEvent::getEntity, EntityShootBowEvent::getBow, EntityShootBowEvent::getProjectile, EntityShootBowEvent::getForce, EntityShootBowEvent::setForce, EntityShootBowEvent::cancel, EntityShootBowEvent::uncancel, EntityShootBowEvent::isCancelled
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

    // pmmp: EntityTeleportEvent, EntityTeleportEvent::getFrom, EntityTeleportEvent::getTo, EntityTeleportEvent::setTo, EntityTeleportEvent::cancel, EntityTeleportEvent::uncancel, EntityTeleportEvent::isCancelled
    #[test]
    fn entity_teleport_target_can_change() {
        let mut e = EntityTeleport {
            entity: Actor::Player(player(1)),
            origin_world: WORLD,
            origin: Vec3::ZERO,
            target_world: WORLD,
            target: AT,
            cancelled: false,
        };
        assert_eq!(e.entity(), Actor::Player(player(1)));
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

    // pmmp: EntityTrampleFarmlandEvent, EntityTrampleFarmlandEvent::getBlock, EntityTrampleFarmlandEvent::cancel, EntityTrampleFarmlandEvent::uncancel, EntityTrampleFarmlandEvent::isCancelled
    #[test]
    fn entity_trample_farmland_names_the_farmland() {
        let mut e = EntityTrampleFarmland {
            entity: Actor::Player(player(1)),
            world: WORLD,
            pos: POS,
            block: DIRT,
            cancelled: false,
        };
        assert_eq!(e.entity(), Actor::Player(player(1)));
        assert_eq!((e.world(), e.pos(), e.block()), (WORLD, POS, DIRT));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: ItemDespawnEvent, ItemMergeEvent, ItemSpawnEvent, ItemDespawnEvent::getEntity, ItemDespawnEvent::cancel, ItemDespawnEvent::uncancel, ItemDespawnEvent::isCancelled, ItemMergeEvent::getTarget, ItemMergeEvent::cancel, ItemMergeEvent::uncancel, ItemMergeEvent::isCancelled, ItemSpawnEvent::getEntity
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

    // pmmp: ProjectileHitEvent, ProjectileHitBlockEvent, ProjectileHitEntityEvent, ProjectileLaunchEvent, ProjectileHitEvent::getEntity, ProjectileHitBlockEvent::getBlockHit, ProjectileHitEvent::getRayTraceResult, ProjectileHitEntityEvent::getEntityHit, ProjectileLaunchEvent::getEntity, ProjectileLaunchEvent::cancel, ProjectileLaunchEvent::uncancel, ProjectileLaunchEvent::isCancelled
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
}
