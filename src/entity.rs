//! Entities: what lives in a world besides blocks and players.
//!
//! The tick owns every entity; code outside a tick keeps only an [`Id`], which stays valid until
//! the entity is gone and is never reused for another entity. A
//! [`Context`](crate::Context) lends an [`Entity`] to read (`entity`) and a [`Mut`] to
//! change (`entity_mut`), finds entities near a place through the chunk index (`entities_in`,
//! `nearest_entity`) and spawns them (`drop_item`, `spawn_tnt`, `spawn_human`, `spawn_zombie`,
//! `spawn_custom_entity`, `spawn_painting`, ...) and plays [`Animation`]s on them and on
//! players (`play_animation`). The server adds entity types of its own as [`Custom`]s.

mod animation;
mod custom;
mod painting;
mod skin;

pub use crate::tick::{Entity, MAX_ENTITIES_PER_WORLD, MAX_NAME_TAG_BYTES, Mut};
pub use animation::Animation;
pub use custom::{Behaviour, Custom, Error, Result};
pub use painting::PaintingMotive;
pub use skin::{MAX_SKIN_BYTES, SKIN_SIZES, Skin};
pub(crate) use skin::{MAX_SKIN_JSON_DEPTH, NO_CUSTOM_GEOMETRY};

/// An entity: its slot in the world's entity store and the slot's generation, so a stale `Id`
/// of a removed entity never finds the entity that took the slot later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id {
    index: u32,
    // A u64 so it never wraps: one removal adds 1, so 2^64 removals never happen, while 2^32
    // could on a long-running server and let a stale `Id` find a new entity.
    generation: u64,
}

impl Id {
    /// The ID of slot `index` in its `generation`.
    pub(crate) const fn new(index: u32, generation: u64) -> Self {
        Self { index, generation }
    }

    /// The generation of the slot this entity holds.
    pub(crate) const fn generation(self) -> u64 {
        self.generation
    }

    /// The slot in the entity store.
    pub(crate) const fn index(self) -> u32 {
        self.index
    }
}

/// What an entity is: the types Anchor knows (mob AI is out of scope).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Type {
    /// A dropped item stack.
    #[doc(alias = "ItemEntity")]
    Item,
    /// An XP orb.
    #[doc(alias = "ExperienceOrb")]
    XpOrb,
    /// A block that falls and lands as a block again.
    // api-class: FallingBlock
    FallingBlock,
    /// Primed TNT.
    #[doc(alias = "PrimedTNT")]
    PrimedTnt,
    /// An arrow.
    #[doc(alias = "entity\\projectile\\Arrow")]
    Arrow,
    /// A thrown snowball.
    #[doc(alias = "entity\\projectile\\Snowball")]
    Snowball,
    /// A thrown egg (no chickens hatch: mobs are out of scope).
    #[doc(alias = "entity\\projectile\\Egg")]
    Egg,
    /// A thrown ender pearl.
    #[doc(alias = "entity\\projectile\\EnderPearl")]
    EnderPearl,
    /// A thrown splash or lingering potion.
    #[doc(alias = "entity\\projectile\\SplashPotion")]
    SplashPotion,
    /// A thrown XP bottle.
    #[doc(alias = "entity\\projectile\\ExperienceBottle")]
    XpBottle,
    /// A lingering potion's cloud.
    // api-class: AreaEffectCloud
    AreaEffectCloud,
    /// A villager-type NPC that trades (no AI).
    // api-class: Villager
    Villager,
    /// A player-shaped NPC (no AI).
    // api-class: Human
    Human,
    /// A zombie without AI: it stands, falls, burns, takes hits and drops rotten flesh.
    // api-class: Zombie
    Zombie,
    /// An entity of a [`Custom`] type ([`Entity::custom_type`] names it).
    Custom,
    /// A painting on a wall.
    // api-class: Painting
    Painting,
    /// An end crystal: it explodes when hurt.
    #[doc(alias = "entity\\object\\EndCrystal")]
    EndCrystal,
    /// A flying firework rocket: it bursts when its flight time is up.
    #[doc(alias = "entity\\object\\FireworkRocket")]
    FireworkRocket,
    /// A thrown trident: it holds the trident item until a player picks it up.
    #[doc(alias = "entity\\projectile\\Trident")]
    Trident,
    /// A thrown ice bomb (an Education item): it freezes the water around where it lands.
    #[doc(alias = "entity\\projectile\\IceBomb")]
    IceBomb,
}

/// Who may pick up an arrow stuck in a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArrowPickup {
    /// Nobody.
    None,
    /// Every player (survival players get the arrow back).
    Any,
    /// Creative players only (shot with Infinity), who get nothing back.
    Creative,
}

impl ArrowPickup {
    /// The mode of a saved byte; any for an unknown one.
    pub(crate) fn from_byte(b: u8) -> Self {
        match b {
            0 => Self::None,
            2 => Self::Creative,
            _ => Self::Any,
        }
    }

    /// The saved byte.
    pub(crate) fn to_byte(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Any => 1,
            Self::Creative => 2,
        }
    }
}

/// A projectile to launch with [`Context::spawn_projectile`](crate::Context::spawn_projectile):
/// it flies and hits as one a player threw or shot, without an owner (an ender pearl without an
/// owner teleports nobody).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Projectile {
    /// An arrow with the base damage 2 that anyone may pick up once stuck; `critical` as a fully
    /// drawn bow's (more damage, a trail).
    Arrow {
        /// Shot with full force.
        critical: bool,
    },
    /// A snowball.
    Snowball,
    /// An egg.
    Egg,
    /// An ender pearl.
    EnderPearl,
    /// A splash potion of the type.
    SplashPotion(crate::effect::Potion),
    /// A lingering potion of the type: it leaves a cloud.
    LingeringPotion(crate::effect::Potion),
    /// An XP bottle.
    XpBottle,
    /// An ice bomb.
    IceBomb,
}
