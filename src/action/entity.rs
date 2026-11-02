//! Actions on entities: a player's melee hit and an explosion.
//!
//! They go through the game rules with their events (the entity rules do them,
//! `tick::entity`).

use super::Action;
use crate::math::Vec3;
use crate::{entity, player, world};

/// A player hits an entity in melee as its client's hit would.
///
/// The hit has reach, the damage of the held item with Strength, Weakness, Sharpness and a
/// critical hit, knockback, Knockback and Fire Aspect, wear and food, with the events of each
/// ([`EntityDamage`](crate::event::EntityDamage), [`EntityMotion`](crate::event::EntityMotion),
/// [`EntityCombust`](crate::event::EntityCombust), [`EntityDeath`](crate::event::EntityDeath)).
/// A painting or an end crystal is hit too; nothing happens for any other target that is not a
/// living NPC, nor for an attacker who is not in the game or not in the target's world. A player
/// who cannot reach the target only swings, with the sound of a hit that did no damage.
#[doc(alias = "Player::attackEntity")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attack {
    pub(crate) attacker: player::Id,
    pub(crate) target: entity::Id,
}

impl Attack {
    /// `attacker` hits `target`.
    pub fn new(attacker: player::Id, target: entity::Id) -> Self {
        Self { attacker, target }
    }
}

impl Action for Attack {}

/// An explosion as the game makes one.
///
/// It does what [`Context::explode`](crate::Context::explode) does, with the events of the
/// entities it hurts and pushes ([`EntityDamage`](crate::event::EntityDamage),
/// [`EntityMotion`](crate::event::EntityMotion)). It has no source entity, so no
/// [`EntityExplode`](crate::event::EntityExplode). Nothing happens for a world that is not
/// loaded, a centre that is not finite or outside the world border, or a size that is not finite
/// or not above 0.
#[doc(alias = "Explosion::explodeA")]
#[doc(alias = "Explosion::explodeB")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Explode {
    pub(crate) world: world::Id,
    pub(crate) center: Vec3,
    pub(crate) size: f64,
    pub(crate) fire: bool,
}

impl Explode {
    /// An explosion of `size` at `center` in `world`, without fire; a size above 8 explodes as 8.
    pub fn new(world: world::Id, center: Vec3, size: f64) -> Self {
        Self {
            world,
            center,
            size,
            fire: false,
        }
    }

    /// The same explosion, with fire.
    ///
    /// A third of the broken blocks are set on fire where the block under them can hold fire.
    #[doc(alias = "Explosion::setFireChance")]
    #[must_use]
    pub fn fire(self) -> Self {
        Self { fire: true, ..self }
    }
}

impl Action for Explode {}
