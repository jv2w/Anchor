//! The kinds of entities: a closed set (PLAN.md 10.7), each with its data and the constants that
//! set it apart (size, physics, network type, saved identifier). A new kind is a new variant with
//! an arm in each `match` here and in the rules of [`super`]; nothing else changes shape.

use std::collections::BTreeMap;

use super::super::SessionId;
use super::super::trade::Offers;
use crate::block;
use crate::effect::{self, Potion};
use crate::entity::Id;
use crate::item::Stack;
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
}

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
#[derive(Debug, Clone, Copy, PartialEq)]
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
}

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pickup {
    /// Nobody.
    None,
    /// Every player (survival players get the arrow back).
    Any,
    /// Creative players only (shot with Infinity), who get nothing back.
    Creative,
}

impl Pickup {
    /// The saved byte (PMMP `pickup`).
    pub(super) fn to_byte(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Any => 1,
            Self::Creative => 2,
        }
    }

    /// The mode of a saved byte; PMMP's default (any) for an unknown one.
    pub(super) fn from_byte(b: u8) -> Self {
        match b {
            0 => Self::None,
            2 => Self::Creative,
            _ => Self::Any,
        }
    }
}

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
    /// A player-shaped NPC (PMMP `Human`), shown with `AddPlayer` and a default skin.
    Human {
        /// The name tag and player list name.
        name: String,
        /// The player list UUID, made from the runtime ID when it gets one.
        uuid: Uuid,
    },
}

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
}

/// PMMP `ItemEntity`: 0.25 × 0.25, gravity 0.04, drag 0.02 before gravity.
const ITEM: Physics = Physics {
    width: 0.25,
    height: 0.25,
    gravity: 0.04,
    drag: 0.02,
    drag_before_gravity: true,
    step_height: 0.0,
    unstuck: true,
    currents: true,
    ray: false,
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
}

impl Kind {
    /// The physics constants.
    pub(super) fn physics(&self) -> &'static Physics {
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
            },
            Self::Cloud(_) => &CLOUD,
            Self::Npc(n) => match n.form {
                Form::Villager { .. } => &VILLAGER,
                Form::Human { .. } => &HUMAN,
            },
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
    pub(super) fn identifier(&self) -> &'static str {
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
            },
            Self::Cloud(_) => "minecraft:area_effect_cloud",
            Self::Npc(n) => match n.form {
                Form::Villager { .. } => "minecraft:villager_v2",
                Form::Human { .. } => "minecraft:player",
            },
        }
    }

    /// The saved `identifier`: the actor type, except a lingering potion's (Bedrock's
    /// `minecraft:lingering_potion`, so it still lingers after a load: PMMP saves no flag and a
    /// loaded lingering potion splashes) and a human NPC's (PMMP's `Human`).
    pub(super) fn saved_identifier(&self) -> &'static str {
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
            Self::ExperienceOrb(_) | Self::Projectile(_) | Self::Cloud(_) | Self::Npc(_) => 0.0,
        }
    }

    /// The health of a new entity of this kind (PMMP: 5 for items, 1 for projectiles, the
    /// `Entity` default 20 otherwise).
    pub(super) fn max_health(&self) -> f64 {
        match self {
            Self::Item(_) => 5.0,
            Self::Projectile(_) => 1.0,
            Self::ExperienceOrb(_)
            | Self::FallingBlock(_)
            | Self::Tnt(_)
            | Self::Cloud(_)
            | Self::Npc(_) => 20.0,
        }
    }

    /// Whether fire and lava leave it alone (PMMP `isFireProof`: netherite items, clouds).
    pub(super) fn is_fire_proof(&self) -> bool {
        match self {
            Self::Item(item) => item
                .stack
                .item()
                .is_some_and(crate::item::Type::is_fire_proof),
            Self::Cloud(_) => true,
            Self::ExperienceOrb(_)
            | Self::FallingBlock(_)
            | Self::Tnt(_)
            | Self::Projectile(_)
            | Self::Npc(_) => false,
        }
    }

    /// Whether damage of `cause` hurts it (PMMP `attack` overrides: falling blocks, primed TNT
    /// and projectiles take only void damage; fire-proof entities ignore fire and lava; an
    /// invulnerable NPC takes only void damage).
    pub(super) fn takes(&self, cause: Cause) -> bool {
        match self {
            Self::FallingBlock(_) | Self::Tnt(_) | Self::Projectile(_) => cause == Cause::Void,
            Self::Npc(n) if n.invulnerable => cause == Cause::Void,
            Self::Item(_) | Self::ExperienceOrb(_) | Self::Cloud(_) | Self::Npc(_) => {
                !(self.is_fire_proof()
                    && matches!(cause, Cause::Fire | Cause::FireTick | Cause::Lava))
            }
        }
    }

    /// Whether placing a block in its box is refused (PMMP `canBeCollidedWith`: alive and not an
    /// item, orb, projectile or cloud).
    pub(super) fn blocks_placing(&self) -> bool {
        match self {
            Self::FallingBlock(_) | Self::Tnt(_) => true,
            Self::Npc(n) => n.dead_ticks.is_none(),
            Self::Item(_) | Self::ExperienceOrb(_) | Self::Projectile(_) | Self::Cloud(_) => false,
        }
    }

    /// Whether it is a living NPC that projectiles, potions and clouds can hit (PMMP `Living`).
    pub(super) fn is_living(&self) -> bool {
        matches!(self, Self::Npc(n) if n.dead_ticks.is_none())
    }
}
