//! The kinds of entities: a closed set (PLAN.md 10.7), each with its data and the constants that
//! set it apart (size, physics, network type, saved identifier). A new kind is a new variant with
//! an arm in each `match` here and in the rules of [`super`]; nothing else changes shape.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::super::SessionId;
use super::super::trade::Offers;
use super::custom::Spec;
use crate::block::{self, Face};
use crate::effect::{self, Potion};
use crate::entity::{Id, PaintingMotive, Skin, Type};
use crate::item::Stack;
use crate::nbt::Compound;
use crate::protocol::Uuid;
use crate::world::BlockPos;

/// What an entity is, with the data only that kind has.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Kind {
    /// A dropped item stack (PMMP `ItemEntity`).
    Item(Item),
    /// An experience orb (PMMP `ExperienceOrb`).
    ExperienceOrb(Orb),
    /// A falling block: sand, gravel, an anvil… (PMMP `FallingBlock`).
    FallingBlock(Falling),
    /// Primed TNT (PMMP `PrimedTNT`).
    Tnt(Tnt),
    /// An arrow or a thrown item in flight or stuck (PMMP `Projectile`).
    Projectile(Projectile),
    /// A lingering potion's cloud (PMMP `AreaEffectCloud`).
    Cloud(Cloud),
    /// A non-player character without AI (PLAN.md 8.6: villager NPCs trade; human NPCs).
    Npc(Character),
    /// A painting on a wall (PMMP `Painting`).
    Painting(Painting),
    /// An end crystal (PMMP `EndCrystal`).
    EndCrystal(Crystal),
    /// A flying firework rocket (PMMP `FireworkRocket`).
    Firework(Firework),
}

/// A painting (PMMP `Painting`). It hangs in the block of its position, on the wall behind
/// `facing` (PMMP `blockIn` is that wall block: the position's block one step against `facing`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Painting {
    /// The picture.
    pub(super) motive: PaintingMotive,
    /// The side of the wall it hangs on: north, south, west or east.
    pub(super) facing: Face,
    /// Whether it drops its item when it dies (not when a player with infinite items broke it,
    /// PMMP `onDeath`). Not saved: it matters only between its death and its removal.
    pub(super) drops: bool,
}

/// An end crystal (PMMP `EndCrystal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Crystal {
    /// Its bedrock base shows (PMMP `showBase`).
    pub(super) show_base: bool,
    /// The block its beam points at (PMMP `beamTarget`).
    pub(super) beam_target: Option<BlockPos>,
    /// A hit other than the void's landed (PMMP `primed`): at its death it explodes. Not saved.
    pub(super) primed: bool,
}

/// A firework rocket (PMMP `FireworkRocket`; never saved with its chunk).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Firework {
    /// The age in ticks at which it bursts (PMMP `maxFlightTimeTicks`).
    pub(super) max_flight: u32,
    /// Its explosions: the item's `Explosions` compounds, each checked ([`MAX_EXPLOSIONS`] at
    /// most).
    pub(super) explosions: Vec<Compound>,
    /// The player who launched it (not saved).
    pub(super) owner: Option<SessionId>,
    /// The runtime ID of the owner, shown to clients (0: none).
    pub(super) owner_runtime_id: u64,
}

/// The most explosions a firework rocket holds. Crafting gives at most 7 (one gunpowder and up
/// to 7 stars); the bound keeps its metadata and its burst bounded whatever the API or an item's
/// NBT asks.
pub(super) const MAX_EXPLOSIONS: usize = 256;

/// A dropped item (PMMP `ItemEntity`).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Item {
    /// The items; never empty.
    pub(super) stack: Stack,
    /// Ticks until a player may pick it up; [`NEVER`] never.
    pub(super) pickup_delay: i16,
    /// Ticks until it disappears; [`NEVER`] never, otherwise 0 to [`MAX_DESPAWN_DELAY`].
    pub(super) despawn_delay: i32,
    /// PMMP's `Owner` and `Thrower` (player names), kept for the saved form.
    pub(super) owner: String,
    pub(super) thrower: String,
}

/// An experience orb (PMMP `ExperienceOrb`).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Orb {
    /// Experience points it gives, 1 to `i16::MAX`.
    pub(super) value: u16,
    /// Ticks until it disappears; [`NEVER`] never, otherwise 0 to [`MAX_DESPAWN_DELAY`].
    pub(super) despawn_delay: i32,
    /// Ticks since it last looked for a player to follow (PMMP `lookForTargetTime`), 0 to 20.
    pub(super) look_for_target: u8,
    /// The player it follows.
    pub(super) target: Option<SessionId>,
}

/// A falling block (PMMP `FallingBlock`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Falling {
    /// The block it is, placed as it is when it lands (an anvil keeps its damage and facing).
    pub(super) block: block::State,
}

/// Primed TNT (PMMP `PrimedTNT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Tnt {
    /// Ticks until it explodes, 0 to [`MAX_FUSE`]; 0 waits for the next tick's explosion budget.
    pub(super) fuse: u16,
    /// Underwater TNT (shown as such; PMMP does not change the explosion either).
    pub(super) underwater: bool,
}

/// The longest fuse (PMMP `PrimedTNT::setFuse`: the saved short).
pub(super) const MAX_FUSE: u16 = 32_767;

/// An arrow or thrown item (PMMP `Projectile`).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Projectile {
    /// What it is.
    pub(super) shot: Shot,
    /// The base damage (PMMP `Projectile::$damage`), finite and not negative.
    pub(super) damage: f64,
    /// The block it is stuck in (PMMP `blockHit`); it does not move while stuck.
    pub(super) stuck: Option<BlockPos>,
    /// The player who shot or threw it (not saved: PMMP's owner is an entity of one run).
    pub(super) owner: Option<SessionId>,
    /// The runtime ID of the owner, shown to clients (0: none).
    pub(super) owner_runtime_id: u64,
}

/// What a projectile is.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Shot {
    /// PMMP `Arrow`.
    Arrow(Arrow),
    /// PMMP `Snowball`.
    Snowball,
    /// PMMP `Egg` (no chickens: mobs are out of scope, CLAUDE.md 1).
    Egg,
    /// PMMP `EnderPearl`.
    EnderPearl,
    /// PMMP `SplashPotion`; a lingering one leaves a cloud.
    Potion {
        /// The potion.
        potion: Potion,
        /// Whether it leaves a cloud (PMMP `willLinger`).
        linger: bool,
    },
    /// PMMP `ExperienceBottle`.
    ExperienceBottle,
    /// PMMP `Trident`.
    Trident(Trident),
    /// PMMP `IceBomb`.
    IceBomb,
}

/// A thrown trident's own data (PMMP `Trident`).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Trident {
    /// The trident item it holds (never empty); a player picks it up whole.
    pub(super) item: Stack,
    /// Thrown by a creative player (PMMP `spawnedInCreative`): a pickup takes it away without
    /// giving the item.
    pub(super) creative: bool,
    /// It may hit players and NPCs (PMMP `canCollide`): not between an entity hit and the next
    /// block. Not saved.
    pub(super) can_collide: bool,
}

/// A thrown trident's base damage (PMMP `Trident::$damage`).
pub(super) const TRIDENT_DAMAGE: f64 = 8.0;

/// An arrow's own data (PMMP `Arrow`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Arrow {
    /// Fully drawn: more damage, and a trail (PMMP `isCritical`).
    pub(super) critical: bool,
    /// Who may pick it up once stuck.
    pub(super) pickup: Pickup,
    /// Punch level: extra knockback (PMMP `punchKnockback`), 0 to 255.
    pub(super) punch: u8,
    /// Ticks it has been stuck (PMMP `collideTicks`); it goes after [`ARROW_LIFE`].
    pub(super) life: i16,
}

/// Ticks a stuck arrow stays (PMMP `Arrow::entityBaseTick`: more than 1,200).
pub(super) const ARROW_LIFE: i16 = 1200;

/// Who may pick up a stuck arrow (PMMP `Arrow::PICKUP_*`).
pub(super) use crate::entity::ArrowPickup as Pickup;

/// A lingering potion's cloud (PMMP `AreaEffectCloud`).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Cloud {
    /// The effects it gives (at most [`MAX_CLOUD_EFFECTS`]).
    pub(super) effects: Vec<effect::Instance>,
    /// Ticks since it appeared.
    pub(super) age: u32,
    /// The age at which it goes (PMMP `maxAge`, `Duration`).
    pub(super) max_age: i32,
    /// Change of `max_age` per entity it affects (PMMP `DurationOnUse`).
    pub(super) max_age_on_use: i32,
    /// Ticks before an affected entity is affected again (PMMP `ReapplicationDelay`).
    pub(super) reapplication_delay: i32,
    /// PMMP `PickupCount` (bottles of dragon breath; kept for the saved form).
    pub(super) pickup_count: i32,
    /// The radius it started with, in blocks.
    pub(super) initial_radius: f64,
    /// The radius now, in blocks, at least 0.5 while it exists.
    pub(super) radius: f64,
    /// PMMP `RadiusChangeOnPickup`, kept for the saved form.
    pub(super) radius_on_pickup: f64,
    /// Change of the radius per entity it affects (PMMP `RadiusOnUse`).
    pub(super) radius_on_use: f64,
    /// Change of the radius per tick (PMMP `RadiusPerTick`).
    pub(super) radius_per_tick: f64,
    /// Who it affected and the age at which they may be affected again (at most one entry per
    /// living entity of the world: players and NPCs; expired entries go at each application).
    pub(super) victims: BTreeMap<Victim, u32>,
}

/// The most effects a cloud keeps from a saved compound (a potion has at most 3).
pub(super) const MAX_CLOUD_EFFECTS: usize = 32;

/// What a cloud affected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Victim {
    /// A player.
    Player(SessionId),
    /// An NPC.
    Entity(Id),
}

/// A non-player character (PMMP `Living` without AI).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Character {
    /// What it looks like and does.
    pub(super) form: Form,
    /// Takes no damage but the void's (the default for NPCs Anchor spawns; see `spawn_npc`).
    pub(super) invulnerable: bool,
    /// Ticks within which a weaker hit does nothing (PMMP `attackTime`).
    pub(super) attack_time: u8,
    /// The base damage of the last hit (PMMP `lastDamageCause`), while `attack_time` runs.
    pub(super) last_damage: f64,
    /// Ticks since it died (PMMP `deadTicks`); `None` while alive.
    pub(super) dead_ticks: Option<u8>,
}

/// Ticks a dead NPC shows its death before it goes (PMMP `Living::$maxDeadTicks`).
pub(super) const MAX_DEAD_TICKS: u8 = 25;

/// What an NPC is.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Form {
    /// A villager (network actor `minecraft:villager_v2`, PLAN.md 8.6) with its trades.
    Villager {
        /// The profession shown (`Variant`).
        profession: u8,
        /// Its trades.
        offers: Offers,
    },
    /// A player-shaped NPC (PMMP `Human`), shown with `AddPlayer`; its name tag is its player
    /// list name.
    Human {
        /// The player list UUID, made from the runtime ID when it gets one.
        uuid: Uuid,
        /// Its skin; `None` shows the default skin.
        skin: Option<Arc<Skin>>,
    },
    /// An entity of a custom type ([`super::custom`]).
    Custom(Custom),
}

/// A custom entity's own data.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Custom {
    /// Its type.
    pub(super) spec: Arc<Spec>,
    /// The user's payload, saved (at most [`super::custom::MAX_DATA_BYTES`] encoded).
    pub(super) data: Compound,
}

/// The longest name tag, in bytes (a player name is at most 16 characters).
pub(super) const MAX_NAME_TAG_BYTES: usize = 1024;

/// "Never" for a pickup or despawn delay (PMMP `NEVER_DESPAWN`).
pub(super) const NEVER: i16 = -1;
/// Ticks a new dropped item or orb lives: 5 minutes (PMMP `DEFAULT_DESPAWN_DELAY`).
pub(super) const DEFAULT_DESPAWN_DELAY: i32 = 6000;
/// The longest despawn delay the saved `Age` short can express (PMMP `MAX_DESPAWN_DELAY`).
pub(super) const MAX_DESPAWN_DELAY: i32 = i16::MAX as i32 + DEFAULT_DESPAWN_DELAY;

/// The physics constants of a kind (PMMP `getInitialDragMultiplier`, `getInitialGravity` and the
/// overrides that change `Entity::move`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Physics {
    /// Width and depth of the bounding box, in blocks.
    pub(super) width: f64,
    /// Height of the bounding box.
    pub(super) height: f64,
    /// Downward acceleration in blocks per tick².
    pub(super) gravity: f64,
    /// Share of the speed lost per tick in the air.
    pub(super) drag: f64,
    /// Whether drag applies before gravity (PMMP `applyDragBeforeGravity`: items, projectiles).
    pub(super) drag_before_gravity: bool,
    /// How high it steps up blocks (0 for every kind so far).
    pub(super) step_height: f64,
    /// Whether it pushes itself out of solid blocks (PMMP `checkObstruction`: items, orbs).
    pub(super) unstuck: bool,
    /// Whether flowing liquids push it (PMMP `canBeMovedByCurrents`).
    pub(super) currents: bool,
    /// Whether it moves along a ray that stops at the first block (PMMP `Projectile::move`)
    /// instead of sliding along blocks.
    pub(super) ray: bool,
    /// Whether its ray stops at water as at a full block (PMMP `IceBomb::
    /// calculateInterceptWithBlock`).
    pub(super) water: bool,
}

/// PMMP `ItemEntity`: 0.25 × 0.25, gravity 0.04, drag 0.02 before gravity.
pub(super) const ITEM: Physics = Physics {
    width: 0.25,
    height: 0.25,
    gravity: 0.04,
    drag: 0.02,
    drag_before_gravity: true,
    step_height: 0.0,
    unstuck: true,
    currents: true,
    ray: false,
    water: false,
};

/// PMMP `ExperienceOrb`: as an item, with drag after gravity.
const ORB: Physics = Physics {
    drag_before_gravity: false,
    ..ITEM
};

/// PMMP `FallingBlock`: 0.98 × 0.98, gravity 0.04, drag 0.02, not moved by liquids.
const FALLING: Physics = Physics {
    width: 0.98,
    height: 0.98,
    gravity: 0.04,
    drag: 0.02,
    drag_before_gravity: false,
    step_height: 0.0,
    unstuck: false,
    currents: false,
    ray: false,
    water: false,
};

/// PMMP `PrimedTNT`: as a falling block, moved by liquids.
const TNT: Physics = Physics {
    currents: true,
    ..FALLING
};

/// PMMP `Arrow`: 0.25 × 0.25, gravity 0.05, drag 0.01 before gravity.
const ARROW: Physics = Physics {
    width: 0.25,
    height: 0.25,
    gravity: 0.05,
    drag: 0.01,
    drag_before_gravity: true,
    step_height: 0.0,
    unstuck: false,
    currents: true,
    ray: true,
    water: false,
};

/// PMMP `Throwable`: as an arrow with gravity 0.03.
const THROWN: Physics = Physics {
    gravity: 0.03,
    ..ARROW
};

/// PMMP `SplashPotion`: gravity 0.05.
const POTION: Physics = Physics {
    gravity: 0.05,
    ..THROWN
};

/// PMMP `ExperienceBottle`: gravity 0.07.
const BOTTLE: Physics = Physics {
    gravity: 0.07,
    ..THROWN
};

/// PMMP `IceBomb` (a `Throwable`): its ray stops at water.
const ICE_BOMB: Physics = Physics {
    water: true,
    ..THROWN
};

/// PMMP `Trident`: 0.25 wide, 0.35 high, gravity 0.1, drag 0.01 before gravity.
const TRIDENT: Physics = Physics {
    width: 0.25,
    height: 0.35,
    gravity: 0.1,
    ..ARROW
};

/// PMMP `Painting`: a 0.5 box (PMMP: "not accurate, but not used"; [`Kind::size`] is not its box
/// either, `Entity::bounding_box` is), no gravity, a drag of 1 that stops every motion. Liquids do
/// not move it (PMMP: it has no movement update).
const PAINTING: Physics = Physics {
    width: 0.5,
    height: 0.5,
    gravity: 0.0,
    drag: 1.0,
    drag_before_gravity: false,
    step_height: 0.0,
    unstuck: false,
    currents: false,
    ray: false,
    water: false,
};

/// PMMP `EndCrystal`: 2 × 2, no gravity, a drag of 1 that stops every motion before it moves.
const END_CRYSTAL: Physics = Physics {
    width: 2.0,
    height: 2.0,
    ..PAINTING
};

/// PMMP `FireworkRocket`: 0.25 × 0.25, no gravity, no drag; liquids push it.
const FIREWORK: Physics = Physics {
    width: 0.25,
    height: 0.25,
    drag: 0.0,
    currents: true,
    ..PAINTING
};

/// PMMP `AreaEffectCloud`: no gravity, no drag; [`Kind::size`] gives its box.
const CLOUD: Physics = Physics {
    width: 0.5,
    height: 0.5,
    gravity: 0.0,
    drag: 0.0,
    drag_before_gravity: false,
    step_height: 0.0,
    unstuck: false,
    currents: false,
    ray: false,
    water: false,
};

/// PMMP `Villager` (`Living`: gravity 0.08, drag 0.02): 0.6 × 1.9.
const VILLAGER: Physics = Physics {
    width: 0.6,
    height: 1.9,
    gravity: 0.08,
    drag: 0.02,
    drag_before_gravity: false,
    step_height: 0.0,
    unstuck: false,
    currents: true,
    ray: false,
    water: false,
};

/// PMMP `Human`: 0.6 × 1.8.
const HUMAN: Physics = Physics {
    height: 1.8,
    ..VILLAGER
};

/// Why an entity takes damage, as far as entities other than players care (PMMP
/// `EntityDamageEvent::CAUSE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cause {
    /// Inside fire.
    Fire,
    /// Burning.
    FireTick,
    /// Inside lava.
    Lava,
    /// Touching a cactus.
    Contact,
    /// Below the world.
    Void,
    /// An explosion.
    Explosion,
    /// A player's hit.
    Attack,
    /// A projectile's hit.
    Projectile,
    /// Instant damage.
    Magic,
    /// A fall.
    Fall,
    /// Inside a solid block.
    Suffocation,
    /// A falling anvil.
    FallingBlock,
    /// Damage dealt through the API (PMMP `CAUSE_CUSTOM`).
    Custom,
}

impl Kind {
    /// What it is, as the API names it.
    pub(super) fn entity_type(&self) -> Type {
        match self {
            Self::Item(_) => Type::Item,
            Self::ExperienceOrb(_) => Type::ExperienceOrb,
            Self::FallingBlock(_) => Type::FallingBlock,
            Self::Tnt(_) => Type::PrimedTnt,
            Self::Projectile(p) => match p.shot {
                Shot::Arrow(_) => Type::Arrow,
                Shot::Snowball => Type::Snowball,
                Shot::Egg => Type::Egg,
                Shot::EnderPearl => Type::EnderPearl,
                Shot::Potion { .. } => Type::SplashPotion,
                Shot::ExperienceBottle => Type::ExperienceBottle,
                Shot::Trident(_) => Type::Trident,
                Shot::IceBomb => Type::IceBomb,
            },
            Self::Cloud(_) => Type::AreaEffectCloud,
            Self::Npc(n) => match n.form {
                Form::Villager { .. } => Type::Villager,
                Form::Human { .. } => Type::Human,
                Form::Custom(_) => Type::Custom,
            },
            Self::Painting(_) => Type::Painting,
            Self::EndCrystal(_) => Type::EndCrystal,
            Self::Firework(_) => Type::FireworkRocket,
        }
    }

    /// Whether a new entity of this kind shows its name tag always (human NPCs: their name, as
    /// a player's shows).
    pub(super) fn shows_name_always(&self) -> bool {
        matches!(
            self,
            Self::Npc(Character {
                form: Form::Human { .. },
                ..
            })
        )
    }

    /// The physics constants.
    pub(super) fn physics(&self) -> &Physics {
        match self {
            Self::Item(_) => &ITEM,
            Self::ExperienceOrb(_) => &ORB,
            Self::FallingBlock(_) => &FALLING,
            Self::Tnt(_) => &TNT,
            Self::Projectile(p) => match p.shot {
                Shot::Arrow(_) => &ARROW,
                Shot::Snowball | Shot::Egg | Shot::EnderPearl => &THROWN,
                Shot::Potion { .. } => &POTION,
                Shot::ExperienceBottle => &BOTTLE,
                Shot::Trident(_) => &TRIDENT,
                Shot::IceBomb => &ICE_BOMB,
            },
            Self::Cloud(_) => &CLOUD,
            Self::Npc(n) => match &n.form {
                Form::Villager { .. } => &VILLAGER,
                Form::Human { .. } => &HUMAN,
                Form::Custom(c) => &c.spec.physics,
            },
            Self::Painting(_) => &PAINTING,
            Self::EndCrystal(_) => &END_CRYSTAL,
            Self::Firework(_) => &FIREWORK,
        }
    }

    /// Width and height of the box: the physics size, except a cloud's, as wide as its
    /// radius × 2 (PMMP `AreaEffectCloud::getInitialSizeInfo`).
    pub(super) fn size(&self) -> (f64, f64) {
        if let Self::Cloud(c) = self {
            (c.radius * 2.0, CLOUD.height)
        } else {
            let p = self.physics();
            (p.width, p.height)
        }
    }

    /// The Bedrock actor type sent to clients (PMMP `getNetworkTypeId`).
    pub(super) fn identifier(&self) -> &str {
        match self {
            Self::Item(_) => "minecraft:item",
            Self::ExperienceOrb(_) => "minecraft:xp_orb",
            Self::FallingBlock(_) => "minecraft:falling_block",
            Self::Tnt(_) => "minecraft:tnt",
            Self::Projectile(p) => match p.shot {
                Shot::Arrow(_) => "minecraft:arrow",
                Shot::Snowball => "minecraft:snowball",
                Shot::Egg => "minecraft:egg",
                Shot::EnderPearl => "minecraft:ender_pearl",
                Shot::Potion { .. } => "minecraft:splash_potion",
                Shot::ExperienceBottle => "minecraft:xp_bottle",
                Shot::Trident(_) => "minecraft:thrown_trident",
                Shot::IceBomb => "minecraft:ice_bomb",
            },
            Self::Cloud(_) => "minecraft:area_effect_cloud",
            Self::Npc(n) => match &n.form {
                Form::Villager { .. } => "minecraft:villager_v2",
                Form::Human { .. } => "minecraft:player",
                Form::Custom(c) => &c.spec.identifier,
            },
            Self::Painting(_) => "minecraft:painting",
            Self::EndCrystal(_) => "minecraft:ender_crystal",
            Self::Firework(_) => "minecraft:fireworks_rocket",
        }
    }

    /// The saved `identifier`: the actor type, except a lingering potion's (Bedrock's
    /// `minecraft:lingering_potion`, so it still lingers after a load: PMMP saves no flag and a
    /// loaded lingering potion splashes) and a human NPC's (PMMP's `Human`).
    pub(super) fn saved_identifier(&self) -> &str {
        match self {
            Self::Projectile(Projectile {
                shot: Shot::Potion { linger: true, .. },
                ..
            }) => "minecraft:lingering_potion",
            Self::Npc(Character {
                form: Form::Human { .. },
                ..
            }) => "Human",
            _ => self.identifier(),
        }
    }

    /// How far above its position the client draws it (PMMP `getOffsetPosition`).
    pub(super) fn network_offset(&self) -> f64 {
        match self {
            Self::Item(_) => 0.125,
            Self::FallingBlock(_) | Self::Tnt(_) => 0.49,
            Self::Npc(Character {
                form: Form::Human { .. },
                ..
            }) => 1.621,
            Self::ExperienceOrb(_)
            | Self::Projectile(_)
            | Self::Cloud(_)
            | Self::Npc(_)
            | Self::Painting(_)
            | Self::EndCrystal(_)
            | Self::Firework(_) => 0.0,
        }
    }

    /// The health of a new entity of this kind (PMMP: 5 for items, 1 for projectiles,
    /// paintings and end crystals, the `Entity` default 20 otherwise; a custom type's own).
    pub(super) fn max_health(&self) -> f64 {
        match self {
            Self::Item(_) => 5.0,
            Self::Projectile(_) | Self::Painting(_) | Self::EndCrystal(_) => 1.0,
            Self::Npc(Character {
                form: Form::Custom(c),
                ..
            }) => c.spec.max_health,
            Self::ExperienceOrb(_)
            | Self::FallingBlock(_)
            | Self::Tnt(_)
            | Self::Cloud(_)
            | Self::Npc(_)
            | Self::Firework(_) => 20.0,
        }
    }

    /// Whether fire and lava leave it alone (PMMP `isFireProof`: netherite items, clouds, end
    /// crystals).
    pub(super) fn is_fire_proof(&self) -> bool {
        match self {
            Self::Item(item) => item
                .stack
                .item()
                .is_some_and(crate::item::Type::is_fire_proof),
            Self::Cloud(_) | Self::EndCrystal(_) => true,
            Self::ExperienceOrb(_)
            | Self::FallingBlock(_)
            | Self::Tnt(_)
            | Self::Projectile(_)
            | Self::Npc(_)
            | Self::Painting(_)
            | Self::Firework(_) => false,
        }
    }

    /// Whether damage of `cause` hurts it (PMMP `attack` overrides: falling blocks, primed TNT
    /// and projectiles take only void damage; fire-proof entities ignore fire and lava; an
    /// invulnerable NPC takes only void damage).
    pub(super) fn takes(&self, cause: Cause) -> bool {
        match self {
            Self::FallingBlock(_) | Self::Tnt(_) | Self::Projectile(_) => cause == Cause::Void,
            Self::Npc(n) if n.invulnerable => cause == Cause::Void,
            Self::Item(_)
            | Self::ExperienceOrb(_)
            | Self::Cloud(_)
            | Self::Npc(_)
            | Self::Painting(_)
            | Self::EndCrystal(_)
            | Self::Firework(_) => {
                !(self.is_fire_proof()
                    && matches!(cause, Cause::Fire | Cause::FireTick | Cause::Lava))
            }
        }
    }

    /// Whether placing a block in its box is refused (PMMP `canBeCollidedWith`: alive and not an
    /// item, orb, projectile, cloud, painting or firework rocket; an end crystal is alive while
    /// its health is above 0, which `Entity::can_be_collided_with` adds).
    pub(super) fn blocks_placing(&self) -> bool {
        match self {
            Self::FallingBlock(_) | Self::Tnt(_) | Self::EndCrystal(_) => true,
            Self::Npc(n) => n.dead_ticks.is_none(),
            Self::Item(_)
            | Self::ExperienceOrb(_)
            | Self::Projectile(_)
            | Self::Cloud(_)
            | Self::Painting(_)
            | Self::Firework(_) => false,
        }
    }

    /// Whether it is a living NPC that projectiles, potions and clouds can hit (PMMP `Living`).
    pub(super) fn is_living(&self) -> bool {
        matches!(self, Self::Npc(n) if n.dead_ticks.is_none())
    }

    /// A custom entity's own data.
    pub(super) fn custom(&self) -> Option<&Custom> {
        match self {
            Self::Npc(Character {
                form: Form::Custom(c),
                ..
            }) => Some(c),
            _ => None,
        }
    }

    /// The name tag a new entity of this kind starts with (a custom type's; empty otherwise).
    pub(super) fn default_name_tag(&self) -> &str {
        self.custom().map_or("", |c| &c.spec.name_tag)
    }

    /// Whether a new entity of this kind is saved with its chunk (all but a temporary custom
    /// type's and a firework rocket).
    pub(super) fn is_saved(&self) -> bool {
        self.can_be_saved() && self.custom().is_none_or(|c| c.spec.saved)
    }

    /// Whether an entity of this kind may be saved with its chunk at all (PMMP
    /// `NeverSavedWithChunkEntity`: firework rockets never are).
    pub(super) fn can_be_saved(&self) -> bool {
        !matches!(self, Self::Firework(_))
    }
}
