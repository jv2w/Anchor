//! The kinds of entities, a closed set with the data and the constants of each.
//!
//! A kind has its data, its size and physics, its network type and its saved identifier. A new
//! kind is a new variant with an arm in each `match` here and in the rules of [`super`]; nothing
//! else changes shape.

pub(super) use crate::entity::ArrowPickup as Pickup;

use std::collections::BTreeMap;
use std::sync::Arc;

use super::custom::Spec;
use crate::block::{self, Face};
use crate::effect::{self, Potion};
use crate::entity::{Id, PaintingMotive, Skin, Type};
use crate::event::DamageCause;
use crate::item::Stack;
use crate::nbt::Compound;
use crate::protocol::Uuid;
use crate::tick::SessionId;
use crate::tick::checkpoint::{self, Codec, Reader, Writer, fields, variants};
use crate::tick::survival::{self, PLAYER_HEIGHT, PLAYER_WIDTH};
use crate::tick::trade::Offers;
use crate::world::{BlockPos, PLAYER_ENTITY_TYPE};

/// An arrow: 0.25 × 0.25, gravity 0.05, drag 0.01 before gravity.
const ARROW: Physics = Physics {
    width: 0.25,
    height: 0.25,
    gravity: 0.05,
    drag: 0.01,
    drag_before_gravity: true,
    unstuck: false,
    currents: true,
    ray: true,
    water: false,
};
/// An xp bottle: gravity 0.07.
const BOTTLE: Physics = Physics {
    gravity: 0.07,
    ..THROWN
};
/// A cloud: no gravity, no drag; [`Kind::size`] gives its box.
const CLOUD: Physics = Physics {
    width: 0.5,
    height: 0.5,
    gravity: 0.0,
    drag: 0.0,
    drag_before_gravity: false,
    unstuck: false,
    currents: false,
    ray: false,
    water: false,
};
/// The ticks a new orb lives, which the saved `Age` counts from.
/// It is 5 minutes; a dropped item lives `item-despawn-ticks`.
pub(super) const DEFAULT_DESPAWN_DELAY_TICKS: i32 = 6000;
/// An end crystal: 2 × 2, no gravity, a drag of 1 that stops every motion before it moves.
const END_CRYSTAL: Physics = Physics {
    width: 2.0,
    height: 2.0,
    ..PAINTING
};
/// A falling block: 0.98 × 0.98, gravity 0.04, drag 0.02, not moved by liquids.
const FALLING: Physics = Physics {
    width: 0.98,
    height: 0.98,
    gravity: 0.04,
    drag: 0.02,
    drag_before_gravity: false,
    unstuck: false,
    currents: false,
    ray: false,
    water: false,
};
/// A firework rocket: 0.25 × 0.25, no gravity, no drag; liquids push it.
const FIREWORK: Physics = Physics {
    width: 0.25,
    height: 0.25,
    drag: 0.0,
    currents: true,
    ..PAINTING
};
/// A human NPC: a player's box.
const HUMAN: Physics = Physics {
    width: PLAYER_WIDTH,
    height: PLAYER_HEIGHT,
    ..VILLAGER
};
/// An ice bomb (a thrown item): its ray stops at water.
const ICE_BOMB: Physics = Physics {
    water: true,
    ..THROWN
};
/// A dropped item: 0.25 × 0.25, gravity 0.04, drag 0.02 before gravity.
pub(super) const ITEM: Physics = Physics {
    width: 0.25,
    height: 0.25,
    gravity: 0.04,
    drag: 0.02,
    drag_before_gravity: true,
    unstuck: true,
    currents: true,
    ray: false,
    water: false,
};
/// The longest despawn delay the saved `Age` short can express.
pub(super) const MAX_DESPAWN_DELAY_TICKS: i32 = i16::MAX as i32 + DEFAULT_DESPAWN_DELAY_TICKS;
/// The longest name tag, in bytes.
/// It bounds an entity's ([`Mut::set_name_tag`](super::Mut::set_name_tag)), a human NPC's name and
/// a dropped item's owner or thrower (a player name is at most 16 characters).
pub const MAX_NAME_TAG_BYTES: usize = 1024;
/// The value of a pickup or despawn delay that means never.
pub(super) const NEVER: i16 = -1;
/// An xp orb: as an item, with drag after gravity.
const ORB: Physics = Physics {
    drag_before_gravity: false,
    ..ITEM
};
/// A painting: a 0.5 box, no gravity, a drag of 1 that stops every motion.
/// The box is not used, and [`Kind::size`] is not its box either (`Entity::bounding_box` is).
/// Liquids do not move it.
const PAINTING: Physics = Physics {
    width: 0.5,
    height: 0.5,
    gravity: 0.0,
    drag: 1.0,
    drag_before_gravity: false,
    unstuck: false,
    currents: false,
    ray: false,
    water: false,
};
/// A thrown potion: gravity 0.05.
const POTION: Physics = Physics {
    gravity: 0.05,
    ..THROWN
};
/// A thrown item: as an arrow with gravity 0.03.
const THROWN: Physics = Physics {
    gravity: 0.03,
    ..ARROW
};
/// Primed TNT: as a falling block, moved by liquids.
const TNT: Physics = Physics {
    currents: true,
    ..FALLING
};
/// A thrown trident: 0.25 wide, 0.35 high, gravity 0.1, drag 0.01 before gravity.
const TRIDENT: Physics = Physics {
    width: 0.25,
    height: 0.35,
    gravity: 0.1,
    ..ARROW
};
/// A villager (gravity 0.08, drag 0.02): 0.6 × 1.9.
const VILLAGER: Physics = Physics {
    width: 0.6,
    height: 1.9,
    gravity: 0.08,
    drag: 0.02,
    drag_before_gravity: false,
    unstuck: false,
    currents: true,
    ray: false,
    water: false,
};
/// A zombie: as a villager, 0.6 × 1.9.
const ZOMBIE: Physics = VILLAGER;

/// What an entity is, with the data only that kind has.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Kind {
    /// A dropped item stack.
    Item(Item),
    /// An xp orb.
    XpOrb(Orb),
    /// A falling block that the API spawned.
    FallingBlock(Falling),
    /// Primed TNT.
    Tnt(Tnt),
    /// An arrow or a thrown item in flight or stuck.
    Projectile(Projectile),
    /// A lingering potion's cloud.
    Cloud(Cloud),
    /// A non-player character without AI.
    /// It is a villager that trades, a human, a zombie or a custom entity.
    Npc(Character),
    /// A painting on a wall.
    Painting(Painting),
    /// An end crystal.
    EndCrystal(Crystal),
    /// A flying firework rocket.
    Firework(Firework),
}

impl Kind {
    /// Whether an entity of this kind is saved with its chunk by default and may be.
    /// All are but a temporary custom type's and a firework rocket.
    pub(super) fn is_saved(&self) -> bool {
        self.can_be_saved() && self.custom().is_none_or(|c| c.spec.saved)
    }

    /// Whether an entity of this kind may be saved with its chunk at all.
    /// Firework rockets never are.
    pub(super) fn can_be_saved(&self) -> bool {
        !matches!(self, Self::Firework(_))
    }

    /// The name tag a new entity of this kind starts with.
    /// It is a custom type's, and empty for every other kind.
    pub(super) fn default_name_tag(&self) -> &str {
        self.custom().map_or("", |c| &c.spec.name_tag)
    }

    /// The custom entity's type, or `None` for another kind.
    pub(super) fn custom(&self) -> Option<&Custom> {
        match self {
            Self::Npc(Character {
                form: Form::Custom(c),
                ..
            }) => Some(c),
            _ => None,
        }
    }

    /// What it is, as the API names it.
    pub(super) fn entity_type(&self) -> Type {
        match self {
            Self::Item(_) => Type::Item,
            Self::XpOrb(_) => Type::XpOrb,
            Self::FallingBlock(_) => Type::FallingBlock,
            Self::Tnt(_) => Type::PrimedTnt,
            Self::Projectile(p) => match p.shot {
                Shot::Arrow(_) => Type::Arrow,
                Shot::Snowball => Type::Snowball,
                Shot::Egg => Type::Egg,
                Shot::EnderPearl => Type::EnderPearl,
                Shot::Potion { .. } => Type::SplashPotion,
                Shot::XpBottle => Type::XpBottle,
                Shot::Trident(_) => Type::Trident,
                Shot::IceBomb => Type::IceBomb,
            },
            Self::Cloud(_) => Type::AreaEffectCloud,
            Self::Npc(n) => match n.form {
                Form::Villager { .. } => Type::Villager,
                Form::Zombie { .. } => Type::Zombie,
                Form::Human { .. } => Type::Human,
                Form::Custom(_) => Type::Custom,
            },
            Self::Painting(_) => Type::Painting,
            Self::EndCrystal(_) => Type::EndCrystal,
            Self::Firework(_) => Type::FireworkRocket,
        }
    }

    /// The saved `identifier`, which is the actor type except for two kinds.
    /// A lingering potion saves Bedrock's `minecraft:lingering_potion`, so it still lingers after
    /// a load. A human NPC saves `Human`.
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

    /// The Bedrock actor type sent to clients.
    pub(super) fn identifier(&self) -> &str {
        match self {
            Self::Item(_) => "minecraft:item",
            Self::XpOrb(_) => "minecraft:xp_orb",
            Self::FallingBlock(_) => "minecraft:falling_block",
            Self::Tnt(_) => "minecraft:tnt",
            Self::Projectile(p) => match p.shot {
                Shot::Arrow(_) => "minecraft:arrow",
                Shot::Snowball => "minecraft:snowball",
                Shot::Egg => "minecraft:egg",
                Shot::EnderPearl => "minecraft:ender_pearl",
                Shot::Potion { .. } => "minecraft:splash_potion",
                Shot::XpBottle => "minecraft:xp_bottle",
                Shot::Trident(_) => "minecraft:thrown_trident",
                Shot::IceBomb => "minecraft:ice_bomb",
            },
            Self::Cloud(_) => "minecraft:area_effect_cloud",
            Self::Npc(n) => match &n.form {
                Form::Villager { .. } => "minecraft:villager_v2",
                Form::Zombie { .. } => "minecraft:zombie",
                Form::Human { .. } => PLAYER_ENTITY_TYPE,
                Form::Custom(c) => &c.spec.identifier,
            },
            Self::Painting(_) => "minecraft:painting",
            Self::EndCrystal(_) => "minecraft:ender_crystal",
            Self::Firework(_) => "minecraft:fireworks_rocket",
        }
    }

    /// Whether it is a baby: a villager or a zombie made one.
    pub(super) fn is_baby(&self) -> bool {
        matches!(
            self,
            Self::Npc(Character {
                form: Form::Villager { baby: true, .. } | Form::Zombie { baby: true },
                ..
            })
        )
    }

    /// Whether damage of `cause` hurts it.
    /// Falling blocks, primed TNT and projectiles take only void damage, fire-proof entities
    /// ignore fire and lava, and an invulnerable NPC takes only void damage.
    pub(super) fn is_hurt_by(&self, cause: Cause) -> bool {
        let only_void = match self {
            Self::Item(_)
            | Self::XpOrb(_)
            | Self::Cloud(_)
            | Self::Painting(_)
            | Self::EndCrystal(_)
            | Self::Firework(_) => false,
            Self::FallingBlock(_) | Self::Tnt(_) | Self::Projectile(_) => true,
            Self::Npc(n) => n.invulnerable,
        };
        if only_void {
            cause == Cause::Void
        } else {
            !(self.is_fire_proof() && matches!(cause, Cause::Fire | Cause::FireTick | Cause::Lava))
        }
    }

    /// Whether fire and lava leave it alone: netherite items, clouds and end crystals.
    pub(super) fn is_fire_proof(&self) -> bool {
        match self {
            Self::Item(item) => item
                .stack
                .item()
                .is_some_and(crate::item::Type::is_fire_proof),
            Self::XpOrb(_)
            | Self::FallingBlock(_)
            | Self::Tnt(_)
            | Self::Projectile(_)
            | Self::Npc(_)
            | Self::Painting(_)
            | Self::Firework(_) => false,
            Self::Cloud(_) | Self::EndCrystal(_) => true,
        }
    }

    /// Whether it is a living NPC that projectiles, potions and clouds can hit.
    pub(super) fn is_living(&self) -> bool {
        matches!(self, Self::Npc(n) if n.dead_ticks.is_none())
    }

    /// Whether a new entity of this kind shows its name tag always.
    /// Human NPCs do, with their name, as a player's shows.
    pub(super) fn is_name_always_shown(&self) -> bool {
        matches!(
            self,
            Self::Npc(Character {
                form: Form::Human { .. },
                ..
            })
        )
    }

    /// Whether placing a block in its box is refused.
    /// It is so for what is alive and not an item, orb, projectile, cloud, painting or firework
    /// rocket. An end crystal is alive while its health is above 0, which
    /// `Entity::can_be_collided_with` adds.
    pub(super) fn is_obstacle(&self) -> bool {
        match self {
            Self::Item(_)
            | Self::XpOrb(_)
            | Self::Projectile(_)
            | Self::Cloud(_)
            | Self::Painting(_)
            | Self::Firework(_) => false,
            Self::FallingBlock(_) | Self::Tnt(_) | Self::EndCrystal(_) => true,
            Self::Npc(n) => n.dead_ticks.is_none(),
        }
    }

    /// The health of a new entity of this kind.
    /// It is 5 for items, 1 for projectiles, paintings and end crystals, and 20 for the rest; a
    /// custom type has its own.
    pub(super) fn max_health(&self) -> f64 {
        match self {
            Self::Item(_) => 5.0,
            Self::XpOrb(_)
            | Self::FallingBlock(_)
            | Self::Tnt(_)
            | Self::Cloud(_)
            | Self::Firework(_) => 20.0,
            Self::Projectile(_) | Self::Painting(_) | Self::EndCrystal(_) => 1.0,
            Self::Npc(n) => match &n.form {
                Form::Villager { .. } | Form::Zombie { .. } | Form::Human { .. } => 20.0,
                Form::Custom(c) => c.spec.max_health,
            },
        }
    }

    /// How far above its position the client draws it.
    pub(super) fn network_offset(&self) -> f64 {
        match self {
            Self::Item(_) => 0.125,
            Self::XpOrb(_)
            | Self::Projectile(_)
            | Self::Cloud(_)
            | Self::Painting(_)
            | Self::EndCrystal(_)
            | Self::Firework(_) => 0.0,
            Self::FallingBlock(_) | Self::Tnt(_) => 0.49,
            Self::Npc(n) => match n.form {
                Form::Human { .. } => 1.621,
                Form::Villager { .. } | Form::Zombie { .. } | Form::Custom(_) => 0.0,
            },
        }
    }

    /// Width and height of the box.
    /// It is the physics size, except a cloud's, which is as wide as its radius × 2.
    pub(super) fn size(&self) -> (f64, f64) {
        if let Self::Cloud(c) = self {
            (c.radius * 2.0, CLOUD.height)
        } else {
            let p = self.physics();
            (p.width, p.height)
        }
    }

    /// The physics constants.
    pub(super) fn physics(&self) -> &Physics {
        match self {
            Self::Item(_) => &ITEM,
            Self::XpOrb(_) => &ORB,
            Self::FallingBlock(_) => &FALLING,
            Self::Tnt(_) => &TNT,
            Self::Projectile(p) => match p.shot {
                Shot::Arrow(_) => &ARROW,
                Shot::Snowball | Shot::Egg | Shot::EnderPearl => &THROWN,
                Shot::Potion { .. } => &POTION,
                Shot::XpBottle => &BOTTLE,
                Shot::Trident(_) => &TRIDENT,
                Shot::IceBomb => &ICE_BOMB,
            },
            Self::Cloud(_) => &CLOUD,
            Self::Npc(n) => match &n.form {
                Form::Villager { .. } => &VILLAGER,
                Form::Zombie { .. } => &ZOMBIE,
                Form::Human { .. } => &HUMAN,
                Form::Custom(c) => &c.spec.physics,
            },
            Self::Painting(_) => &PAINTING,
            Self::EndCrystal(_) => &END_CRYSTAL,
            Self::Firework(_) => &FIREWORK,
        }
    }
}

variants!(Kind {
    0 => Item(item),
    1 => XpOrb(orb),
    2 => FallingBlock(falling),
    3 => Tnt(tnt),
    4 => Projectile(projectile),
    5 => Cloud(cloud),
    6 => Npc(character),
    7 => Painting(painting),
    8 => EndCrystal(crystal),
    9 => Firework(firework),
});

/// A dropped item.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Item {
    /// The items; never empty.
    pub(super) stack: Stack,
    /// The ticks until a player may pick it up; [`NEVER`] never.
    pub(super) pickup_delay: i16,
    /// The ticks until it disappears; [`NEVER`] never.
    /// Otherwise it is 0 to the larger of [`MAX_DESPAWN_DELAY_TICKS`] and `item-despawn-ticks`.
    pub(super) despawn_delay: i32,
    /// The saved `Owner` (a player name).
    pub(super) owner: String,
    /// The saved `Thrower` (a player name).
    pub(super) thrower: String,
}

fields!(Item {
    stack,
    pickup_delay,
    despawn_delay,
    owner,
    thrower,
});

/// An xp orb.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Orb {
    /// The xp points it gives, 1 to [`Self::MAX_VALUE`].
    pub(super) value: u16,
    /// The ticks until it disappears; [`NEVER`] never, otherwise 0 to [`MAX_DESPAWN_DELAY_TICKS`].
    pub(super) despawn_delay: i32,
    /// The ticks since it last looked for a player to follow, 0 to 20.
    pub(super) look_for_target: u8,
    /// The player it follows.
    pub(super) target: Option<SessionId>,
}

impl Orb {
    /// The most points one orb gives: what the saved `Value` short holds.
    pub(super) const MAX_VALUE: u16 = i16::MAX.cast_unsigned();
}

fields!(Orb {
    value,
    despawn_delay,
    look_for_target,
    target,
});

/// A falling block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Falling {
    /// The block it is, placed as it is when it lands (it keeps its block state).
    pub(super) block: block::State,
}

fields!(Falling { block });

/// Primed TNT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Tnt {
    /// The ticks until it explodes, 0 to [`Self::MAX_FUSE`].
    /// A fuse of 0 waits for the next tick's explosion budget.
    pub(super) fuse: u16,
    /// Whether it is shown as underwater TNT; the explosion does not change.
    pub(super) underwater: bool,
}

impl Tnt {
    /// The longest fuse: what the saved `Fuse` short holds.
    pub(super) const MAX_FUSE: u16 = 32_767;
}

fields!(Tnt { fuse, underwater });

/// An arrow or thrown item.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Projectile {
    /// What it is.
    pub(super) shot: Shot,
    /// The base damage, finite and not negative.
    pub(super) damage: f64,
    /// The block it is stuck in; it does not move while stuck.
    pub(super) stuck: Option<BlockPos>,
    /// The player who shot or threw it (not saved: a session belongs to one run).
    pub(super) owner: Option<SessionId>,
    /// The runtime ID of the owner, shown to clients (0: none).
    pub(super) owner_runtime_id: u64,
}

fields!(Projectile {
    shot,
    damage,
    stuck,
    owner,
    owner_runtime_id,
});

/// What a projectile is.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Shot {
    /// An arrow.
    Arrow(Arrow),
    /// A snowball.
    Snowball,
    /// An egg (no chickens: mobs are out of scope).
    Egg,
    /// An ender pearl.
    EnderPearl,
    /// A thrown potion; a lingering one leaves a cloud.
    Potion {
        /// The potion.
        potion: Potion,
        /// Whether it leaves a cloud.
        linger: bool,
    },
    /// An xp bottle.
    XpBottle,
    /// A thrown trident.
    Trident(Trident),
    /// An ice bomb.
    IceBomb,
}

variants!(Shot {
    0 => Arrow(arrow),
    1 => Snowball,
    2 => Egg,
    3 => EnderPearl,
    4 => Potion { potion, linger },
    5 => XpBottle,
    6 => Trident(trident),
    7 => IceBomb,
});

/// An arrow's own data.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Arrow {
    /// Whether it is fully drawn: more damage, and a trail.
    pub(super) critical: bool,
    /// Who may pick it up once stuck.
    pub(super) pickup: Pickup,
    /// The Punch level: extra knockback, 0 to 255.
    pub(super) punch: u8,
    /// The ticks it has been stuck; it goes after [`Self::MAX_LIFE`].
    pub(super) life: i16,
}

impl Arrow {
    /// The ticks a stuck arrow stays: it goes once it has been stuck for more.
    pub(super) const MAX_LIFE: i16 = 1200;
}

fields!(Arrow {
    critical,
    pickup,
    punch,
    life,
});

/// A thrown trident's own data.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Trident {
    /// The trident item it holds (never empty); a player picks it up whole.
    pub(super) item: Stack,
    /// Whether a creative player threw it.
    /// A pickup takes it away without giving the item.
    pub(super) creative: bool,
    /// Whether it may hit players and NPCs.
    /// It may not between an entity hit and the next block. Not saved.
    pub(super) can_collide: bool,
}

impl Trident {
    /// A thrown trident's base damage.
    pub(super) const DAMAGE: f64 = 8.0;
}

fields!(Trident {
    item,
    creative,
    can_collide,
});

/// A lingering potion's cloud.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Cloud {
    /// The effects it gives (at most [`Self::MAX_EFFECTS`]).
    pub(super) effects: Vec<effect::Instance>,
    /// The ticks since it appeared.
    pub(super) age: u32,
    /// The age at which it goes (the saved `Duration`).
    pub(super) max_age: i32,
    /// The change of `max_age` per entity it affects (the saved `DurationOnUse`).
    pub(super) max_age_on_use: i32,
    /// The ticks before an affected entity is affected again.
    pub(super) reapplication_delay: i32,
    /// The bottles of dragon breath taken (the saved `PickupCount`), kept for the saved form.
    pub(super) pickup_count: i32,
    /// The radius it started with, in blocks.
    pub(super) initial_radius: f64,
    /// The radius now, in blocks, at least 0.5 while it exists.
    pub(super) radius: f64,
    /// The change of the radius when a bottle takes from it, kept for the saved form.
    pub(super) radius_on_pickup: f64,
    /// The change of the radius per entity it affects.
    pub(super) radius_on_use: f64,
    /// The change of the radius per tick.
    pub(super) radius_per_tick: f64,
    /// Who it affected and the age at which they may be affected again.
    /// It holds at most one entry per living entity of the world, players and NPCs; the expired
    /// entries go at each application.
    pub(super) victims: BTreeMap<Victim, u32>,
}

impl Cloud {
    /// The most effects a cloud keeps from a saved compound (a potion has at most 3).
    pub(super) const MAX_EFFECTS: usize = 32;
}

fields!(Cloud {
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
});

/// What a cloud affected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Victim {
    /// A player.
    Player(SessionId),
    /// An NPC.
    Entity(Id),
}

variants!(Victim {
    0 => Player(session),
    1 => Entity(id),
});

/// A non-player character without AI.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Character {
    /// What it looks like and does.
    pub(super) form: Form,
    /// Whether it takes no damage but the void's.
    /// It is the default of the villager and human NPCs that `spawn_npc` makes, not of a zombie or
    /// a custom entity.
    pub(super) invulnerable: bool,
    /// The ticks within which a weaker hit does nothing.
    pub(super) attack_time: u32,
    /// The base damage of the last hit, while `attack_time` runs.
    pub(super) last_damage: f64,
    /// The ticks since it died; `None` while alive.
    pub(super) dead_ticks: Option<u8>,
    /// The user's payload, saved (at most [`super::custom::MAX_DATA_BYTES`] encoded).
    pub(super) data: Compound,
    /// What it holds in its hand, shown to its viewers and saved; empty for nothing.
    pub(super) held: Stack,
}

impl Character {
    /// The ticks a dead NPC shows its death before it goes.
    pub(super) const MAX_DEAD_TICKS: u8 = 25;
}

fields!(Character {
    form,
    invulnerable,
    attack_time,
    last_damage,
    dead_ticks,
    data,
    held,
});

/// What an NPC is.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Form {
    /// A villager (network actor `minecraft:villager_v2`) with its trades.
    Villager {
        /// The profession shown (`Variant`).
        profession: u8,
        /// Its trades.
        offers: Offers,
        /// Whether it is a baby (shown by the `Baby` flag).
        baby: bool,
    },
    /// A zombie without AI.
    Zombie {
        /// Whether it is a baby (shown by the `Baby` flag).
        baby: bool,
    },
    /// A player-shaped NPC, shown with `AddPlayer`.
    /// Its name tag is its player list name.
    Human {
        /// The player list UUID, made from the runtime ID when it gets one.
        uuid: Uuid,
        /// Its skin; `None` shows the default skin.
        skin: Option<Arc<Skin>>,
    },
    /// An entity of a custom type ([`super::custom`]).
    Custom(Custom),
}

variants!(Form {
    0 => Villager {
        profession,
        offers,
        baby
    },
    1 => Zombie { baby },
    2 => Human { uuid, skin },
    3 => Custom(custom),
});

/// What a custom entity is.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Custom {
    /// Its type.
    pub(super) spec: Arc<Spec>,
}

/// A custom entity's type by its identifier, found again among the server's custom types.
impl Codec for Custom {
    fn encode(&self, w: &mut Writer) {
        let Self { spec } = self;
        spec.identifier.encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> checkpoint::Result<Self> {
        let identifier = String::decode(r)?;
        let spec = r
            .entities
            .find(&identifier)
            .cloned()
            .ok_or(checkpoint::Error::Format("custom entity type"))?;
        Ok(Self { spec })
    }
}

/// A painting.
/// It hangs in the block of its position, on the wall behind `facing`: the wall block is the
/// position's block one step against `facing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Painting {
    /// The picture.
    pub(super) motive: PaintingMotive,
    /// The side of the wall it hangs on: north, south, west or east.
    pub(super) facing: Face,
    /// Whether it drops its item when it dies.
    /// It does not when a player with infinite items broke it. Not saved: it matters only
    /// between its death and its removal.
    pub(super) drops: bool,
}

fields!(Painting {
    motive,
    facing,
    drops,
});

/// An end crystal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Crystal {
    /// Whether its bedrock base shows.
    pub(super) show_base: bool,
    /// The block its beam points at.
    pub(super) beam_target: Option<BlockPos>,
    /// Whether a hit other than the void's landed.
    /// At its death it explodes. Not saved.
    pub(super) primed: bool,
}

fields!(Crystal {
    show_base,
    beam_target,
    primed,
});

/// A firework rocket, never saved with its chunk.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Firework {
    /// The age in ticks at which it bursts.
    pub(super) max_flight: u32,
    /// Its explosions: the item's `Explosions` compounds, each checked.
    /// It holds at most [`Self::MAX_EXPLOSIONS`].
    pub(super) explosions: Vec<Compound>,
    /// The player who launched it (not saved).
    pub(super) owner: Option<SessionId>,
    /// The flight multiplier its shown item carries.
    /// It is `Flight`, as the official server shows a launched rocket's item. Shown only: not in
    /// checkpoints or the state hash (a restored rocket shows 0).
    pub(super) flight: u8,
}

impl Firework {
    /// The most explosions a firework rocket holds.
    /// Crafting gives at most 7 (one gunpowder and up to 7 stars); the bound keeps its metadata
    /// and its burst bounded whatever the API or an item's NBT asks.
    pub(super) const MAX_EXPLOSIONS: usize = 256;
}

fields!(Firework {
    max_flight,
    explosions,
    owner;
    flight
});

/// The physics constants of a kind.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Physics {
    /// The width and depth of the bounding box, in blocks.
    pub(super) width: f64,
    /// The height of the bounding box.
    pub(super) height: f64,
    /// The downward acceleration in blocks per tick².
    pub(super) gravity: f64,
    /// The share of the speed lost per tick in the air.
    pub(super) drag: f64,
    /// Whether drag applies before gravity (items, projectiles).
    pub(super) drag_before_gravity: bool,
    /// Whether it pushes itself out of solid blocks (items, orbs).
    pub(super) unstuck: bool,
    /// Whether flowing liquids push it.
    pub(super) currents: bool,
    /// Whether it moves along a ray that stops at the first block, not sliding along blocks.
    pub(super) ray: bool,
    /// Whether its ray stops at water as at a full block (an ice bomb).
    pub(super) water: bool,
}

/// Why an entity takes damage, as far as entities other than players care.
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
    /// An explosion of a block or of nothing in particular.
    BlockExplosion,
    /// An explosion of an entity.
    EntityExplosion,
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
    /// A lightning bolt.
    /// Its event cause is custom, and the hurt event shows the lightning number.
    Lightning,
    /// Damage dealt through the API.
    Custom,
}

impl Cause {
    /// The official server's damage cause number, which the hurt event carries.
    /// A lightning bolt has the players' number; every other cause follows
    /// [`survival::actor_cause`] (P6: a trident's hit on a villager 3).
    pub(super) fn actor_cause(self) -> i32 {
        match self {
            Self::Lightning => survival::Damage::Lightning.actor_cause(),
            _ => survival::actor_cause(self.event_cause()),
        }
    }

    /// The event's cause of this damage.
    pub(super) fn event_cause(self) -> DamageCause {
        match self {
            Self::Fire => DamageCause::Fire,
            Self::FireTick => DamageCause::FireTick,
            Self::Lava => DamageCause::Lava,
            Self::Contact => DamageCause::Contact,
            Self::Void => DamageCause::Void,
            Self::BlockExplosion => DamageCause::BlockExplosion,
            Self::EntityExplosion => DamageCause::EntityExplosion,
            Self::Attack => DamageCause::EntityAttack,
            Self::Projectile => DamageCause::Projectile,
            Self::Magic => DamageCause::Magic,
            Self::Fall => DamageCause::Fall,
            Self::Suffocation => DamageCause::Suffocation,
            Self::Lightning | Self::Custom => DamageCause::Custom,
        }
    }
}
