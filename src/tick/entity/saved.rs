//! The saved form of entities: chunk extras kind 2, one Bedrock-like NBT compound per entity.
//!
//! Common keys: `identifier` (the Bedrock actor type; the older Bedrock types
//! `minecraft:villager` and `minecraft:trident` are read too; `Human` for a human NPC), `Pos` and
//! `Motion` (lists of three doubles; floats are read too), `Rotation` (yaw and pitch floats),
//! `FallDistance` (float), `Fire` (short), `OnGround` (byte); with a name tag `CustomName`
//! (string); `CustomNameVisible` (byte, "shown when looked at") with a name tag or when it is off.
//! Anchor's own keys, written only when they differ from the kind's default:
//! `CustomNameAlwaysVisible` (byte), `Scale` (float), `Invisible` and `NoGravity` (bytes). An
//! entity that is not saved with its chunk (the API's choice) writes nothing. Then per kind:
//! - items: `Item` (the stack, [`Stack::to_nbt`]), `Health` (short, read up to the maximum
//!   health), `Age` and `PickupDelay` (shorts), `Owner` and `Thrower` (strings of at most
//!   [`MAX_NAME_TAG_BYTES`]); Anchor's `DespawnDelay` (int) for a despawn delay past what `Age`
//!   holds (the longest `item-despawn-ticks`), which a load takes over `Age`;
//! - orbs: `Age` and `Value` (short);
//! - falling blocks: `FallingBlock` (the block state as Bedrock's disk palette writes it);
//! - primed TNT: `Fuse` (short), `AllowUnderwater` (byte, only when set);
//! - projectiles: `damage` (double), `StuckToBlockPos` (three ints, while stuck); arrows `pickup`,
//!   `crit` (bytes), `life` (short) and `punch` (byte); potions `PotionId` (short; a lingering
//!   one is saved as `minecraft:lingering_potion`); tridents `Trident` (the item,
//!   [`Stack::to_nbt`]) and `isCreative` (byte);
//! - paintings: `TileX`, `TileY`, `TileZ` (ints: the wall block, which is the position's block one
//!   step against the facing), `Facing` and `Direction` (bytes: south 0, west 1, north 2, east 3)
//!   and `Motive` (string);
//! - end crystals: `ShowBottom` (byte) and, with a beam target, `BlockTargetX`, `BlockTargetY`,
//!   `BlockTargetZ` (ints);
//! - firework rockets are never saved;
//! - clouds: `SpawnTick` (minus the cloud's age: the world clock is not at hand here, and only
//!   the difference is read back), `PotionId` (always water's 0: a cloud keeps no potion, so one
//!   saved with another stays as saved), `Duration`, `DurationOnUse`, `PickupCount`,
//!   `ReapplicationDelay`, `InitialRadius`, `Radius`, `RadiusChangeOnPickup`, `RadiusOnUse`,
//!   `RadiusPerTick` and, with effects, `mobEffects`;
//! - NPCs: `Health` (float), `Invulnerable` (byte) and, when not empty, `AnchorData` (the user's
//!   compound, at most [`custom::MAX_DATA_BYTES`]) and Bedrock's `Mainhand` (the held item, a
//!   list of one [`Stack::to_nbt`]); villagers `Profession` (int), `Offers` (the offer list,
//!   [`Offers::to_nbt`]) and Bedrock's `IsBaby` (byte); zombies `IsBaby`; human NPCs their name as
//!   the name tag and, with a skin, a `Skin` compound (`Name`, `Data`, `CapeData`,
//!   `GeometryName`, `GeometryData`; one with a skin that does not pass [`Skin`]'s checks is no
//!   entity and stays as saved);
//! - custom entities: the type's identifier and the NPC keys. One whose type the server does not
//!   register is not an entity here (like an unknown block) and stays as saved.
//!
//! Runtime and unique IDs are not saved: they are numbers of one run; so are the owner of a
//! projectile and who a cloud affected.
//!
//! The file is outside input: a compound that does not describe an entity Anchor knows (an unknown
//! identifier, a missing or invalid key, a position outside its chunk) is not an entity to the
//! tick, and the store keeps it as saved.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::Entity;
use super::custom::{self, EntityTypes};
use super::kind::{
    Arrow, Character, Cloud, Crystal, Custom, DEFAULT_DESPAWN_DELAY_TICKS, Falling, Form, Item,
    Kind, MAX_DESPAWN_DELAY_TICKS, MAX_NAME_TAG_BYTES, NEVER, Orb, Painting, Projectile, Shot, Tnt,
    Trident,
};
use super::object::{data_facing, facing_data};
use super::physics::{self, Body};
use crate::config::ITEM_DESPAWN_TICKS_RANGE;
use crate::custom::Registry;
use crate::effect::{self, Potion};
use crate::entity::{ArrowPickup, PaintingMotive, Skin};
use crate::item::Stack;
use crate::math::{Vec3, to_f32};
use crate::nbt::{Compound, List, Value};
use crate::protocol::Uuid;
use crate::tick::trade::Offers;
use crate::world::{BlockPos, ChunkPos};

/// The `Age` that means "never despawns".
const AGE_NEVER: i16 = i16::MIN;
/// Bedrock's baby flag of a mob (`IsBaby`).
const BABY: &str = "IsBaby";
/// The key of an NPC's data.
pub(super) const DATA_KEY: &str = "AnchorData";
/// The key of an NPC's held item: Bedrock's mob key, a list of one item compound.
const MAINHAND: &str = "Mainhand";

/// What a saved compound describes, before the store gives it a slot.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Loaded {
    pub(super) body: Body,
    pub(super) yaw: f64,
    pub(super) pitch: f64,
    pub(super) health: f64,
    pub(super) fire_ticks: i16,
    pub(super) kind: Kind,
    pub(super) looks: Looks,
}

/// The saved name tag and looks of an entity.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Looks {
    pub(super) name_tag: String,
    pub(super) name_tag_visible: bool,
    /// `None`: the kind's default.
    pub(super) name_tag_always_visible: Option<bool>,
    pub(super) scale: f64,
    pub(super) invisible: bool,
    pub(super) gravity: bool,
}

/// The saved compound of an entity.
pub(super) fn save(entity: &Entity) -> Compound {
    let mut c = Compound::new();
    let mut put = |k: &str, v: Value| {
        c.insert(k.to_owned(), v);
    };
    let Body {
        position,
        motion,
        on_ground,
        fall_distance,
    } = entity.body;
    put(
        "identifier",
        Value::String(entity.kind.saved_identifier().to_owned()),
    );
    put("Pos", doubles(position));
    put("Motion", doubles(motion));
    put(
        "Rotation",
        Value::List(List::Float(vec![to_f32(entity.yaw), to_f32(entity.pitch)])),
    );
    put("FallDistance", Value::Float(to_f32(fall_distance)));
    put("Fire", Value::Short(entity.fire_ticks));
    put("OnGround", Value::Byte(u8::from(on_ground)));
    if !entity.name_tag.is_empty() {
        put("CustomName", Value::String(entity.name_tag.clone()));
    }
    // A load without the key shows the name tag, so an entity whose tag is off keeps it off
    // until it gets a tag.
    if !entity.name_tag.is_empty() || !entity.name_tag_visible {
        put(
            "CustomNameVisible",
            Value::Byte(u8::from(entity.name_tag_visible)),
        );
    }
    if entity.name_tag_always_visible != entity.kind.is_name_always_shown() {
        put(
            "CustomNameAlwaysVisible",
            Value::Byte(u8::from(entity.name_tag_always_visible)),
        );
    }
    // The scale is a value a float holds exactly (the setter checks it).
    if entity.scale != 1.0 {
        put("Scale", Value::Float(to_f32(entity.scale)));
    }
    if entity.invisible {
        put("Invisible", Value::Byte(1));
    }
    if !entity.gravity {
        put("NoGravity", Value::Byte(1));
    }
    match &entity.kind {
        Kind::Item(item) => {
            put("Item", Value::Compound(item.stack.to_nbt()));
            put("Health", Value::Short(health_short(entity.health)));
            put("Age", Value::Short(age_ticks(item.despawn_delay_ticks)));
            if item.despawn_delay_ticks > MAX_DESPAWN_DELAY_TICKS {
                put("DespawnDelay", Value::Int(item.despawn_delay_ticks));
            }
            put("PickupDelay", Value::Short(item.pickup_delay_ticks));
            put("Owner", Value::String(item.owner.clone()));
            put("Thrower", Value::String(item.thrower.clone()));
        }
        Kind::XpOrb(orb) => {
            put("Age", Value::Short(age_ticks(orb.despawn_delay_ticks)));
            put("Value", Value::Short(orb.value.cast_signed()));
        }
        Kind::FallingBlock(falling) => {
            put(
                "FallingBlock",
                Value::Compound(crate::save::state_nbt(falling.block)),
            );
        }
        Kind::Tnt(tnt) => {
            put("Fuse", Value::Short(tnt.fuse.cast_signed()));
            if tnt.underwater {
                put("AllowUnderwater", Value::Byte(1));
            }
        }
        Kind::Projectile(p) => save_projectile(p, &mut put),
        Kind::Cloud(cloud) => save_cloud(cloud, &mut put),
        Kind::Npc(npc) => save_npc(npc, entity.health, &mut put),
        Kind::Painting(painting) => save_painting(*painting, position, &mut put),
        Kind::EndCrystal(crystal) => save_crystal(*crystal, &mut put),
        // Never saved: `Entities::list` leaves it out (`can_be_saved`); the compound is only
        // the common keys.
        Kind::Firework(_) => {}
    }
    c
}

/// An NPC's keys: its health, `Invulnerable`, its data, its held item and its form's own.
fn save_npc(npc: &Character, health: f64, put: &mut impl FnMut(&str, Value)) {
    put("Health", Value::Float(to_f32(health)));
    put("Invulnerable", Value::Byte(u8::from(npc.invulnerable)));
    if !npc.data.is_empty() {
        put(DATA_KEY, Value::Compound(npc.data.clone()));
    }
    if !npc.held.is_empty() {
        put(
            MAINHAND,
            Value::List(List::Compound(vec![npc.held.to_nbt()])),
        );
    }
    match &npc.form {
        Form::Villager {
            profession,
            offers,
            baby,
        } => {
            put("Profession", Value::Int(i32::from(*profession)));
            put("Offers", Value::Compound(offers.to_nbt()));
            put(BABY, Value::Byte(u8::from(*baby)));
        }
        Form::Zombie { baby } => put(BABY, Value::Byte(u8::from(*baby))),
        Form::Human { uuid: _, skin } => {
            if let Some(skin) = skin {
                put("Skin", Value::Compound(save_skin(skin)));
            }
        }
        Form::Custom(_) => {}
    }
}

/// A painting's keys: the wall block, the facing twice (`Facing` and `Direction`) and the motive.
fn save_painting(painting: Painting, position: Vec3, put: &mut impl FnMut(&str, Value)) {
    let block = BlockPos::containing(position);
    let (dx, dy, dz) = painting.facing.opposite().offset();
    let wall = block.offset(dx, dy, dz).unwrap_or(block);
    put("TileX", Value::Int(wall.x));
    put("TileY", Value::Int(wall.y));
    put("TileZ", Value::Int(wall.z));
    let data = facing_data(painting.facing);
    put("Facing", Value::Byte(data));
    put("Direction", Value::Byte(data));
    put("Motive", Value::String(painting.motive.name().to_owned()));
}

/// An end crystal's keys: the base shown and the beam target.
fn save_crystal(crystal: Crystal, put: &mut impl FnMut(&str, Value)) {
    put("ShowBottom", Value::Byte(u8::from(crystal.show_base)));
    if let Some(t) = crystal.beam_target {
        put("BlockTargetX", Value::Int(t.x));
        put("BlockTargetY", Value::Int(t.y));
        put("BlockTargetZ", Value::Int(t.z));
    }
}

/// A projectile's keys: the damage, the block it is stuck in and its shot's own.
fn save_projectile(p: &Projectile, put: &mut impl FnMut(&str, Value)) {
    put("damage", Value::Double(p.damage));
    if let Some(pos) = p.stuck {
        put(
            "StuckToBlockPos",
            Value::List(List::Int(vec![pos.x, pos.y, pos.z])),
        );
    }
    match &p.shot {
        Shot::Arrow(a) => {
            put("pickup", Value::Byte(a.pickup.to_byte()));
            put("crit", Value::Byte(u8::from(a.critical)));
            put("life", Value::Short(a.life));
            if a.punch > 0 {
                put("punch", Value::Byte(a.punch));
            }
        }
        Shot::Snowball | Shot::Egg | Shot::EnderPearl | Shot::XpBottle | Shot::IceBomb => {}
        Shot::Potion { potion, .. } => {
            put("PotionId", Value::Short(i16::from(potion.id())));
        }
        Shot::Trident(t) => {
            put("Trident", Value::Compound(t.item.to_nbt()));
            put("isCreative", Value::Byte(u8::from(t.creative)));
        }
    }
}

/// A cloud's keys.
fn save_cloud(cloud: &Cloud, put: &mut impl FnMut(&str, Value)) {
    put("SpawnTick", Value::Long(-i64::from(cloud.age)));
    put("PotionId", Value::Short(0));
    put("Duration", Value::Int(cloud.max_age));
    put("DurationOnUse", Value::Int(cloud.max_age_on_use));
    put("PickupCount", Value::Int(cloud.pickup_count));
    put("ReapplicationDelay", Value::Int(cloud.reapplication_delay));
    put("InitialRadius", Value::Float(to_f32(cloud.initial_radius)));
    put("Radius", Value::Float(to_f32(cloud.radius)));
    put(
        "RadiusChangeOnPickup",
        Value::Float(to_f32(cloud.radius_on_pickup)),
    );
    put("RadiusOnUse", Value::Float(to_f32(cloud.radius_on_use)));
    put("RadiusPerTick", Value::Float(to_f32(cloud.radius_per_tick)));
    if !cloud.effects.is_empty() {
        let effects = cloud.effects.iter().map(|e| e.to_nbt()).collect();
        put("mobEffects", Value::List(List::Compound(effects)));
    }
}

/// The entity a saved compound of chunk `chunk` describes, or `None` (the store keeps such a
/// compound as it is); its items are read through `content`, the server's custom content.
pub(super) fn load(
    c: &Compound,
    chunk: ChunkPos,
    types: &EntityTypes,
    content: &Registry,
) -> Option<Loaded> {
    #[cfg(test)]
    crate::tick::rules::reads::add(|r| r.entities += 1);
    let identifier = match c.get("identifier") {
        Some(Value::String(s)) => s.as_str(),
        _ => return None,
    };
    let position = vector(c.get("Pos"))?;
    if !super::is_valid_position(position) || super::chunk_of(position) != chunk {
        return None;
    }
    let motion = match c.get("Motion") {
        some @ Some(_) => physics::bounded(vector(some)?),
        None => Vec3::ZERO,
    };
    let (yaw, pitch) = match c.get("Rotation") {
        Some(Value::List(List::Float(r))) if r.len() == 2 => {
            let (yaw, pitch) = (r.first()?, r.get(1)?);
            if !yaw.is_finite() || !pitch.is_finite() {
                return None;
            }
            (f64::from(*yaw), f64::from(*pitch))
        }
        _ => return None,
    };
    let fall_distance = match c.get("FallDistance") {
        Some(Value::Float(f)) => physics::fall_distance(f64::from(*f)),
        _ => 0.0,
    };
    let fire_ticks = match c.get("Fire") {
        Some(Value::Short(f)) => (*f).max(0),
        _ => 0,
    };
    let on_ground = matches!(c.get("OnGround"), Some(Value::Byte(b)) if *b != 0);
    let mut kind = load_kind(c, identifier, types, content)?;
    if let Kind::Painting(p) = kind
        && !hangs_at(c, p, position)
    {
        return None;
    }
    let looks = load_looks(c, &kind)?;
    let health = match (&kind, c.get("Health")) {
        // The health stays within 0 and the maximum, as `Mut::set_health` keeps it.
        (Kind::Item(_), Some(Value::Short(h))) => {
            crate::math::clamp(f64::from(*h), 0.0, kind.max_health())
        }
        (Kind::Npc(_), Some(Value::Float(h))) if h.is_finite() => {
            crate::math::clamp(f64::from(*h), 0.0, kind.max_health())
        }
        _ => kind.max_health(),
    };
    // An NPC saved while it died loads dead, past the tick of its drops, and finishes dying.
    if let Kind::Npc(n) = &mut kind
        && health <= 0.0
    {
        n.dead_ticks = Some(1);
    }
    Some(Loaded {
        body: Body {
            position,
            motion,
            on_ground,
            fall_distance,
        },
        yaw,
        pitch,
        health,
        fire_ticks,
        kind,
        looks,
    })
}

/// The name tag and looks of an entity of `kind`; `None` for a name tag over
/// [`MAX_NAME_TAG_BYTES`] or a scale `is_valid_scale` refuses.
fn load_looks(c: &Compound, kind: &Kind) -> Option<Looks> {
    let flag = |k: &str| match c.get(k) {
        Some(Value::Byte(b)) => Some(*b != 0),
        _ => None,
    };
    let name_tag = match c.get("CustomName") {
        Some(Value::String(s)) if s.len() <= MAX_NAME_TAG_BYTES => s.clone(),
        Some(_) => return None,
        None => String::new(),
    };
    let name_tag_visible = flag("CustomNameVisible").unwrap_or(true);
    let scale = match c.get("Scale") {
        Some(Value::Float(s)) => Some(f64::from(*s)).filter(|s| super::is_valid_scale(*s, kind))?,
        _ => 1.0,
    };
    Some(Looks {
        name_tag,
        name_tag_visible,
        name_tag_always_visible: flag("CustomNameAlwaysVisible"),
        scale,
        invisible: flag("Invisible").unwrap_or(false),
        gravity: !flag("NoGravity").unwrap_or(false),
    })
}

/// The kind a saved compound with `identifier` describes, with its own keys.
fn load_kind(
    c: &Compound,
    identifier: &str,
    types: &EntityTypes,
    content: &Registry,
) -> Option<Kind> {
    Some(match identifier {
        "minecraft:item" => Kind::Item(load_item(c, content)?),
        "minecraft:xp_orb" => Kind::XpOrb(load_orb(c)),
        "minecraft:falling_block" => {
            let Some(Value::Compound(block)) = c.get("FallingBlock") else {
                return None;
            };
            Kind::FallingBlock(Falling {
                block: crate::save::find_state(block)?,
            })
        }
        "minecraft:tnt" => Kind::Tnt(Tnt {
            fuse: match c.get("Fuse") {
                Some(Value::Short(f)) => u16::try_from(*f).ok().filter(|f| *f <= Tnt::MAX_FUSE)?,
                _ => super::TNT_FUSE_TICKS,
            },
            underwater: matches!(c.get("AllowUnderwater"), Some(Value::Byte(b)) if *b != 0),
        }),
        "minecraft:arrow" => load_projectile(c, Shot::Arrow(load_arrow(c)?))?,
        "minecraft:snowball" => load_projectile(c, Shot::Snowball)?,
        "minecraft:egg" => load_projectile(c, Shot::Egg)?,
        "minecraft:ender_pearl" => load_projectile(c, Shot::EnderPearl)?,
        "minecraft:xp_bottle" => load_projectile(c, Shot::XpBottle)?,
        "minecraft:ice_bomb" => load_projectile(c, Shot::IceBomb)?,
        "minecraft:thrown_trident" | "minecraft:trident" => {
            load_projectile(c, Shot::Trident(load_trident(c, content)?))?
        }
        "minecraft:painting" => Kind::Painting(load_painting(c)?),
        "minecraft:ender_crystal" => Kind::EndCrystal(load_crystal(c)),
        "minecraft:splash_potion" => load_projectile(c, potion(c, false)?)?,
        "minecraft:lingering_potion" => load_projectile(c, potion(c, true)?)?,
        "minecraft:area_effect_cloud" => Kind::Cloud(load_cloud(c)?),
        "minecraft:villager_v2" | "minecraft:villager" => Kind::Npc(load_npc(
            c,
            Form::Villager {
                profession: match c.get("Profession") {
                    Some(Value::Int(p)) => u8::try_from(*p).ok()?,
                    _ => 0,
                },
                // The trade module bounds and checks the offers it reads.
                offers: match c.get("Offers") {
                    Some(Value::Compound(offers)) => Offers::from_nbt(offers, content)?,
                    Some(_) => return None,
                    None => Offers::default(),
                },
                baby: is_baby(c),
            },
            content,
        )?),
        "minecraft:zombie" => Kind::Npc(load_npc(c, Form::Zombie { baby: is_baby(c) }, content)?),
        "Human" => Kind::Npc(load_npc(
            c,
            Form::Human {
                uuid: Uuid::from_bytes([0; 16]),
                skin: match c.get("Skin") {
                    Some(Value::Compound(skin)) => Some(Arc::new(load_skin(skin)?)),
                    Some(_) => return None,
                    None => None,
                },
            },
            content,
        )?),
        _ => load_custom(c, identifier, types, content)?,
    })
}

/// A human NPC's `Skin` compound.
fn save_skin(skin: &Skin) -> Compound {
    Compound::from([
        ("Name".to_owned(), Value::String(skin.id().to_owned())),
        ("Data".to_owned(), Value::ByteArray(skin.data().to_vec())),
        (
            "CapeData".to_owned(),
            Value::ByteArray(skin.cape().to_vec()),
        ),
        (
            "GeometryName".to_owned(),
            Value::String(skin.geometry_name().to_owned()),
        ),
        (
            "GeometryData".to_owned(),
            Value::ByteArray(skin.geometry_data().to_vec()),
        ),
    ])
}

/// The skin of a saved `Skin` compound; `None` when it does not pass [`Skin`]'s checks.
fn load_skin(c: &Compound) -> Option<Skin> {
    let bytes = |k: &str| match c.get(k) {
        Some(Value::ByteArray(b)) => Some(b.clone()),
        Some(_) => None,
        None => Some(Vec::new()),
    };
    let Some(Value::String(name)) = c.get("Name") else {
        return None;
    };
    let geometry = match c.get("GeometryName") {
        Some(Value::String(g)) => g.as_str(),
        _ => return None,
    };
    Skin::new(name.clone(), bytes("Data")?)?
        .with_cape(bytes("CapeData")?)?
        .with_geometry(geometry, bytes("GeometryData")?)
}

/// A dropped item's keys; `None` without a valid stack, or with an owner or thrower that is not a
/// string of at most [`MAX_NAME_TAG_BYTES`].
fn load_item(c: &Compound, content: &Registry) -> Option<Item> {
    let Some(Value::Compound(stack)) = c.get("Item") else {
        return None;
    };
    #[cfg(test)]
    crate::tick::rules::reads::add(|r| r.stacks += 1);
    let stack = crate::tick::inventory::read_saved_stack(stack, content, "a dropped item")
        .filter(|s| !s.is_empty())?;
    let pickup_delay_ticks = match c.get("PickupDelay") {
        Some(Value::Short(d)) => (*d).max(NEVER),
        _ => 0,
    };
    // At most what the API takes (`Mut::set_owner`, `Mut::set_thrower`).
    let text = |k: &str| match c.get(k) {
        Some(Value::String(s)) if s.len() <= MAX_NAME_TAG_BYTES => Some(s.clone()),
        Some(_) => None,
        None => Some(String::new()),
    };
    // Anchor's key for a delay `Age` cannot hold; a value outside that range is not Anchor's.
    let longest = ITEM_DESPAWN_TICKS_RANGE.end().cast_signed();
    let despawn_delay_ticks = match c.get("DespawnDelay") {
        Some(Value::Int(d)) if (MAX_DESPAWN_DELAY_TICKS + 1..=longest).contains(d) => *d,
        _ => despawn_delay_ticks(c),
    };
    Some(Item {
        stack,
        pickup_delay_ticks,
        despawn_delay_ticks,
        owner: text("Owner")?,
        thrower: text("Thrower")?,
    })
}

/// An orb's keys: `Value` (at least 1; its short never passes [`Orb::MAX_VALUE`]), else 1.
fn load_orb(c: &Compound) -> Orb {
    let value = match c.get("Value") {
        Some(Value::Short(v)) => (*v).max(1).cast_unsigned(),
        _ => 1,
    };
    Orb {
        value,
        despawn_delay_ticks: despawn_delay_ticks(c),
        look_for_target: 0,
        target: None,
    }
}

/// A projectile's keys: the damage (finite, 0 to [`Projectile::MAX_DAMAGE`]) and the block it is
/// stuck in.
fn load_projectile(c: &Compound, shot: Shot) -> Option<Kind> {
    let damage = match c.get("damage") {
        Some(Value::Double(d)) if d.is_finite() && (0.0..=Projectile::MAX_DAMAGE).contains(d) => *d,
        Some(_) => return None,
        None => match shot {
            Shot::Arrow(_) => Arrow::DAMAGE,
            Shot::Trident(_) => Trident::DAMAGE,
            _ => 0.0,
        },
    };
    let stuck = match c.get("StuckToBlockPos") {
        Some(Value::List(List::Int(p))) if p.len() == 3 => {
            Some(BlockPos::new(*p.first()?, *p.get(1)?, *p.get(2)?))
        }
        Some(_) => return None,
        None => None,
    };
    Some(Kind::Projectile(Projectile {
        shot,
        damage,
        stuck,
        owner: None,
        owner_runtime_id: 0,
    }))
}

/// A thrown trident's keys; `None` without a valid item.
fn load_trident(c: &Compound, content: &Registry) -> Option<Trident> {
    let Some(Value::Compound(item)) = c.get("Trident") else {
        return None;
    };
    #[cfg(test)]
    crate::tick::rules::reads::add(|r| r.stacks += 1);
    let item = crate::tick::inventory::read_saved_stack(item, content, "a thrown trident")
        .filter(|s| !s.is_empty())?;
    Some(Trident {
        item,
        creative: matches!(c.get("isCreative"), Some(Value::Byte(1))),
        can_collide: true,
    })
}

/// A painting's keys: a known motive and a facing (`Direction`, else `Facing`).
fn load_painting(c: &Compound) -> Option<Painting> {
    let Some(Value::String(motive)) = c.get("Motive") else {
        return None;
    };
    let facing = match (c.get("Direction"), c.get("Facing")) {
        (Some(Value::Byte(d)), _) | (None, Some(Value::Byte(d))) => data_facing(*d),
        _ => return None,
    };
    Some(Painting {
        motive: PaintingMotive::from_name(motive)?,
        facing,
        drops: true,
    })
}

/// Whether a saved painting's wall block (`TileX`, `TileY`, `TileZ`) is the block behind its
/// position, as every painting Anchor saves has it.
fn hangs_at(c: &Compound, painting: Painting, position: Vec3) -> bool {
    let (Some(Value::Int(x)), Some(Value::Int(y)), Some(Value::Int(z))) =
        (c.get("TileX"), c.get("TileY"), c.get("TileZ"))
    else {
        return false;
    };
    BlockPos::new(*x, *y, *z).step(painting.facing) == Some(BlockPos::containing(position))
}

/// An end crystal's keys: the beam target only with all three ints.
fn load_crystal(c: &Compound) -> Crystal {
    let target = match (
        c.get("BlockTargetX"),
        c.get("BlockTargetY"),
        c.get("BlockTargetZ"),
    ) {
        (Some(Value::Int(x)), Some(Value::Int(y)), Some(Value::Int(z))) => {
            Some(BlockPos::new(*x, *y, *z))
        }
        _ => None,
    };
    Crystal {
        show_base: matches!(c.get("ShowBottom"), Some(Value::Byte(1))),
        beam_target: target,
        primed: false,
    }
}

/// An arrow's keys; `None` for a `life` outside 0 to [`Arrow::MAX_LIFE`].
fn load_arrow(c: &Compound) -> Option<Arrow> {
    let byte = |k: &str| match c.get(k) {
        Some(Value::Byte(b)) => Some(*b),
        _ => None,
    };
    Some(Arrow {
        critical: byte("crit") == Some(1),
        pickup: byte("pickup").map_or(ArrowPickup::Any, ArrowPickup::from_byte),
        punch: byte("punch").unwrap_or(0),
        life: match c.get("life") {
            Some(Value::Short(l)) if (0..=Arrow::MAX_LIFE).contains(l) => *l,
            Some(_) => return None,
            None => 0,
        },
    })
}

/// A potion's `PotionId`; water when missing.
fn potion(c: &Compound, linger: bool) -> Option<Shot> {
    let potion = match c.get("PotionId") {
        Some(Value::Short(id)) => Potion::from_id(u8::try_from(*id).ok()?)?,
        _ => Potion::from_id(0)?,
    };
    Some(Shot::Potion { potion, linger })
}

/// A cloud's keys; `None` for a key outside what a cloud holds: a number out of its range, an
/// effect [`effect::Instance::from_nbt`] refuses, more than [`Cloud::MAX_EFFECTS`] effects, or a
/// `PotionId` other than water's 0, which the cloud would not keep.
fn load_cloud(c: &Compound) -> Option<Cloud> {
    let int = |k: &str, default: i32, low: i32| match c.get(k) {
        Some(Value::Int(v)) => Some(*v).filter(|v| (low..=Cloud::MAX_AGE_TICKS).contains(v)),
        Some(_) => None,
        None => Some(default),
    };
    let float = |k: &str, default: f64| match c.get(k) {
        Some(Value::Float(v)) => Some(f64::from(*v))
            .filter(|v| v.is_finite() && (-Cloud::MAX_RADIUS..=Cloud::MAX_RADIUS).contains(v)),
        Some(_) => None,
        None => Some(default),
    };
    match c.get("PotionId") {
        Some(Value::Short(0)) | None => {}
        Some(_) => return None,
    }
    let age = match c.get("SpawnTick") {
        Some(Value::Long(t)) => u32::try_from(t.checked_neg()?).ok()?,
        Some(_) => return None,
        None => 0,
    };
    let effects = match c.get("mobEffects") {
        Some(Value::List(List::Compound(list))) if list.len() <= Cloud::MAX_EFFECTS => list
            .iter()
            .map(effect::Instance::from_nbt)
            .collect::<Option<Vec<_>>>()?,
        Some(Value::List(List::End)) | None => Vec::new(),
        Some(_) => return None,
    };
    let initial_radius = float("InitialRadius", Cloud::DEFAULT_RADIUS)?;
    Some(Cloud {
        effects,
        age,
        max_age: int("Duration", Cloud::DEFAULT_MAX_AGE_TICKS, 0)?,
        max_age_on_use: int("DurationOnUse", 0, -Cloud::MAX_AGE_TICKS)?,
        reapplication_delay: int(
            "ReapplicationDelay",
            Cloud::DEFAULT_REAPPLICATION_DELAY_TICKS,
            0,
        )?,
        pickup_count: match c.get("PickupCount") {
            Some(Value::Int(n)) => *n,
            Some(_) => return None,
            None => 0,
        },
        initial_radius,
        radius: float("Radius", initial_radius)?,
        radius_on_pickup: float("RadiusChangeOnPickup", Cloud::DEFAULT_RADIUS_ON_USE)?,
        radius_on_use: float("RadiusOnUse", Cloud::DEFAULT_RADIUS_ON_USE)?,
        radius_per_tick: float("RadiusPerTick", Cloud::DEFAULT_RADIUS_PER_TICK)?,
        victims: BTreeMap::new(),
    })
}

/// A custom entity of the registered type `identifier`: an NPC's keys; `None` for a type the
/// server does not have or keys [`load_npc`] refuses.
fn load_custom(
    c: &Compound,
    identifier: &str,
    types: &EntityTypes,
    content: &Registry,
) -> Option<Kind> {
    let spec = Arc::clone(types.find(identifier)?);
    load_npc(c, Form::Custom(Custom { spec }), content).map(Kind::Npc)
}

/// Whether a saved mob is a baby.
fn is_baby(c: &Compound) -> bool {
    matches!(c.get(BABY), Some(Value::Byte(b)) if *b != 0)
}

/// An NPC's keys; `None` for data over [`custom::MAX_DATA_BYTES`] or a held item that is not one
/// stack the server knows.
fn load_npc(c: &Compound, form: Form, content: &Registry) -> Option<Character> {
    let data = match c.get(DATA_KEY) {
        Some(Value::Compound(data)) if custom::fits(data) => data.clone(),
        Some(_) => return None,
        None => Compound::new(),
    };
    let held = match c.get(MAINHAND) {
        Some(Value::List(List::Compound(list))) => match list.as_slice() {
            [item] => crate::tick::inventory::read_saved_stack(item, content, "an npc")?,
            _ => return None,
        },
        Some(_) => return None,
        None => Stack::EMPTY,
    };
    Some(Character {
        form,
        invulnerable: matches!(c.get("Invulnerable"), Some(Value::Byte(b)) if *b != 0),
        attack_time: 0,
        last_damage: 0.0,
        dead_ticks: None,
        data,
        held,
    })
}

/// The despawn delay a saved `Age` gives ([`AGE_NEVER`] never, otherwise
/// [`DEFAULT_DESPAWN_DELAY_TICKS`] minus the age, at least 0).
fn despawn_delay_ticks(c: &Compound) -> i32 {
    match c.get("Age") {
        Some(Value::Short(AGE_NEVER)) => i32::from(NEVER),
        Some(Value::Short(a)) => {
            (DEFAULT_DESPAWN_DELAY_TICKS - i32::from(*a)).clamp(0, MAX_DESPAWN_DELAY_TICKS)
        }
        _ => DEFAULT_DESPAWN_DELAY_TICKS,
    }
}

/// The `Age` short of a despawn delay: the age a delay past [`MAX_DESPAWN_DELAY_TICKS`] would have
/// stops above [`AGE_NEVER`].
#[expect(
    clippy::expect_used,
    reason = "invariant: a despawn delay other than `NEVER` is not negative (`Item`, `Orb`), so \
              the age is at most `DEFAULT_DESPAWN_DELAY_TICKS`"
)]
fn age_ticks(delay_ticks: i32) -> i16 {
    if delay_ticks == i32::from(NEVER) {
        return AGE_NEVER;
    }
    let a = DEFAULT_DESPAWN_DELAY_TICKS - delay_ticks;
    i16::try_from(a.max(i32::from(AGE_NEVER) + 1)).expect("an age fits the saved short")
}

/// The `Health` short of an item's health.
#[expect(
    clippy::cast_possible_truncation,
    reason = "invariant: an item's health is finite and 0 to its maximum health (`Mut::set_health` \
              and the load keep it there); `as` saturates otherwise"
)]
fn health_short(health: f64) -> i16 {
    // Up, so an item with a fraction of a point left loads alive.
    health.ceil() as i16
}

/// A list of three doubles.
fn doubles(v: Vec3) -> Value {
    Value::List(List::Double(vec![v.x, v.y, v.z]))
}

/// A finite vector from a list of three doubles or floats.
fn vector(v: Option<&Value>) -> Option<Vec3> {
    let v = match v? {
        Value::List(List::Double(d)) if d.len() == 3 => {
            Vec3::new(*d.first()?, *d.get(1)?, *d.get(2)?)
        }
        Value::List(List::Float(f)) if f.len() == 3 => Vec3::new(
            f64::from(*f.first()?),
            f64::from(*f.get(1)?),
            f64::from(*f.get(2)?),
        ),
        _ => return None,
    };
    v.is_finite().then_some(v)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    use crate::block::{self, Face};
    use crate::item::{Enchantment, Extra, Type};
    use crate::tick::entity::physics::tests::named;
    use crate::tick::entity::tests::{Harness, standing};

    /// `count` of the item `name` (a block's item for a block name).
    pub(in crate::tick) fn stack(name: &str, count: u8) -> Stack {
        let t = Type::from_name(name, 0, None, &Compound::new()).unwrap_or_else(|| {
            match named(name).item() {
                block::ItemForm::Item(t) => t,
                other => panic!("{name}: {other:?}"),
            }
        });
        Stack::new(t, count)
    }

    fn entity(kind: Kind, at: Vec3) -> Entity {
        let body = Body {
            position: at,
            motion: Vec3::new(0.125, -0.25, 0.0625),
            on_ground: true,
            fall_distance: 1.5,
        };
        let mut e = Entity::new(0, body, 90.5, -12.25, kind);
        e.fire_ticks = 37;
        e
    }

    /// A saved offer compound (the trading module owns the shape; the entity keeps it as it is).
    fn offer(n: i32) -> Compound {
        Compound::from([
            (
                "buyA".to_owned(),
                Value::Compound(stack("minecraft:emerald", 3).to_nbt()),
            ),
            (
                "sell".to_owned(),
                Value::Compound(stack("minecraft:bread", 6).to_nbt()),
            ),
            ("maxUses".to_owned(), Value::Int(16)),
            ("netId".to_owned(), Value::Int(n)),
            ("tier".to_owned(), Value::Int(0)),
            ("uses".to_owned(), Value::Int(n)),
        ])
    }

    /// One entity of every kind and shape.
    #[expect(
        clippy::too_many_lines,
        reason = "consistency: one literal entity of every kind and shape"
    )]
    pub(in crate::tick) fn kinds() -> Vec<Kind> {
        let enchanted = stack("minecraft:diamond_sword", 1)
            .with_damage(17)
            .unwrap()
            .with_extra(Some(Extra {
                custom_name: Some("Edge".to_owned()),
                lore: vec!["one".to_owned(), "two".to_owned()],
                enchantments: vec![(Enchantment::all()[0], 3)],
                nbt: Compound::from([("RepairCost".to_owned(), Value::Int(4))]),
            }))
            .unwrap();
        let anvil = block::State::all()
            .filter(|s| s.name() == "minecraft:chipped_anvil")
            .nth(2)
            .unwrap();
        let projectile = |shot: Shot, damage: f64, stuck: Option<BlockPos>| {
            Kind::Projectile(Projectile {
                shot,
                damage,
                stuck,
                owner: None,
                owner_runtime_id: 0,
            })
        };
        let potion = |name: &str| {
            *Potion::all()
                .iter()
                .find(|p| format!("{p:?}") == name)
                .unwrap()
        };
        let regeneration = potion("Regeneration").effects()[0];
        vec![
            Kind::Item(Item {
                stack: stack("minecraft:stick", 13),
                pickup_delay_ticks: 7,
                despawn_delay_ticks: 5000,
                owner: "owner".to_owned(),
                thrower: String::new(),
            }),
            Kind::Item(Item {
                stack: enchanted,
                pickup_delay_ticks: NEVER,
                despawn_delay_ticks: i32::from(NEVER),
                owner: String::new(),
                thrower: "thrower".to_owned(),
            }),
            Kind::Item(Item {
                stack: stack("minecraft:oak_planks", 64),
                pickup_delay_ticks: 0,
                despawn_delay_ticks: MAX_DESPAWN_DELAY_TICKS,
                owner: String::new(),
                thrower: String::new(),
            }),
            Kind::XpOrb(Orb {
                value: 2477,
                despawn_delay_ticks: 1,
                look_for_target: 0,
                target: None,
            }),
            Kind::FallingBlock(Falling { block: anvil }),
            Kind::FallingBlock(Falling {
                block: named("minecraft:sand"),
            }),
            Kind::Tnt(Tnt {
                fuse: 37,
                underwater: false,
            }),
            Kind::Tnt(Tnt {
                fuse: 0,
                underwater: true,
            }),
            projectile(
                Shot::Arrow(Arrow {
                    critical: true,
                    pickup: ArrowPickup::Creative,
                    punch: 2,
                    life: 0,
                }),
                3.5,
                None,
            ),
            projectile(
                Shot::Arrow(Arrow {
                    critical: false,
                    pickup: ArrowPickup::Any,
                    punch: 0,
                    life: 300,
                }),
                2.0,
                Some(BlockPos::new(5, -61, -4)),
            ),
            projectile(Shot::Snowball, 0.0, None),
            projectile(Shot::Egg, 0.0, None),
            projectile(Shot::EnderPearl, 0.0, None),
            projectile(Shot::XpBottle, 0.0, None),
            projectile(
                Shot::Potion {
                    potion: potion("Harming"),
                    linger: false,
                },
                0.0,
                None,
            ),
            projectile(
                Shot::Potion {
                    potion: potion("LongPoison"),
                    linger: true,
                },
                0.0,
                None,
            ),
            Kind::Cloud(Cloud {
                effects: vec![regeneration],
                age: 70,
                max_age: 600,
                max_age_on_use: 0,
                reapplication_delay: 40,
                pickup_count: 0,
                initial_radius: 3.0,
                radius: 2.5,
                radius_on_pickup: -0.5,
                radius_on_use: -0.5,
                radius_per_tick: f64::from(-0.005_f32),
                victims: BTreeMap::new(),
            }),
            Kind::Npc(Character {
                form: Form::Villager {
                    profession: 3,
                    offers: Offers::from_nbt(
                        &crate::nbt::Compound::from([(
                            "Recipes".to_owned(),
                            Value::List(crate::nbt::List::Compound(vec![offer(0), offer(1)])),
                        )]),
                        &crate::custom::Registry::default(),
                    )
                    .expect("the offers read"),
                    baby: true,
                },
                invulnerable: true,
                attack_time: 0,
                last_damage: 0.0,
                dead_ticks: None,
                data: Compound::from([("npc".to_owned(), Value::String("shop".to_owned()))]),
                held: stack("minecraft:diamond_sword", 1),
            }),
            Kind::Npc(Character {
                form: Form::Human {
                    skin: None,
                    uuid: Uuid::from_bytes([0; 16]),
                },
                invulnerable: false,
                attack_time: 0,
                last_damage: 0.0,
                dead_ticks: None,
                data: Compound::new(),
                held: Stack::EMPTY,
            }),
            Kind::Npc(Character {
                form: Form::Zombie { baby: true },
                invulnerable: false,
                attack_time: 0,
                last_damage: 0.0,
                dead_ticks: None,
                data: Compound::new(),
                held: Stack::EMPTY,
            }),
            Kind::Npc(Character {
                form: Form::Zombie { baby: false },
                invulnerable: true,
                attack_time: 0,
                last_damage: 0.0,
                dead_ticks: None,
                data: Compound::new(),
                held: Stack::EMPTY,
            }),
            Kind::Painting(Painting {
                motive: PaintingMotive::Kebab,
                facing: Face::West,
                drops: true,
            }),
            Kind::Painting(Painting {
                motive: PaintingMotive::Skeleton,
                facing: Face::South,
                drops: true,
            }),
            Kind::EndCrystal(Crystal {
                show_base: true,
                beam_target: Some(BlockPos::new(3, -50, -9)),
                primed: false,
            }),
            Kind::EndCrystal(Crystal {
                show_base: false,
                beam_target: None,
                primed: false,
            }),
            projectile(
                Shot::Trident(Trident {
                    item: stack("minecraft:trident", 1).with_damage(3).unwrap(),
                    creative: false,
                    can_collide: true,
                }),
                Trident::DAMAGE,
                Some(BlockPos::new(5, -61, -4)),
            ),
            projectile(
                Shot::Trident(Trident {
                    item: stack("minecraft:trident", 1),
                    creative: true,
                    can_collide: true,
                }),
                Trident::DAMAGE,
                None,
            ),
            projectile(Shot::IceBomb, 0.0, None),
            Kind::Firework(crate::tick::entity::kind::Firework {
                max_flight: 60,
                explosions: vec![Compound::from([
                    ("FireworkType".to_owned(), Value::Byte(1)),
                    ("FireworkColor".to_owned(), Value::ByteArray(vec![4, 11])),
                    ("FireworkFlicker".to_owned(), Value::Byte(1)),
                ])],
                owner: None,
                flight: 1,
            }),
            // The longest `item-despawn-ticks`, past what the saved `Age` holds.
            Kind::Item(Item {
                stack: stack("minecraft:oak_planks", 1),
                pickup_delay_ticks: 0,
                despawn_delay_ticks: i32::try_from(*ITEM_DESPAWN_TICKS_RANGE.end()).unwrap(),
                owner: String::new(),
                thrower: String::new(),
            }),
            // Shot from a bow of the highest Power.
            projectile(
                Shot::Arrow(Arrow {
                    critical: true,
                    pickup: ArrowPickup::Any,
                    punch: u8::MAX,
                    life: 0,
                }),
                crate::tick::entity::projectile::bow_arrow_damage(
                    crate::item::MAX_ENCHANTMENT_LEVEL,
                ),
                None,
            ),
        ]
    }

    /// [`load`] on a server without custom entities or custom items.
    fn load_without_custom(c: &Compound, chunk: ChunkPos) -> Option<Loaded> {
        load(c, chunk, &EntityTypes::default(), &Registry::default())
    }

    /// The disk bytes of a compound (little-endian NBT, as chunk extras are written).
    pub(in crate::tick) fn bytes(c: &Compound) -> Vec<u8> {
        let mut w = crate::protocol::Writer::new(Vec::new(), 0);
        w.nbt(c, crate::nbt::Encoding::LittleEndian).unwrap();
        w.into_inner()
    }

    #[test]
    fn every_kind_round_trips_through_its_saved_form() {
        let at = Vec3::new(5.25, -59.5, -3.75);
        for kind in kinds().into_iter().filter(Kind::can_be_saved) {
            let mut e = entity(kind.clone(), at);
            if matches!(kind, Kind::Npc(_)) {
                e.health = 13.0;
            }
            let saved = save(&e);
            let loaded = load_without_custom(&saved, ChunkPos::new(0, -1)).expect("loads");
            assert_eq!(loaded.kind, kind);
            assert_eq!(loaded.body, e.body);
            assert_eq!((loaded.yaw, loaded.pitch), (e.yaw, e.pitch));
            assert_eq!(loaded.fire_ticks, e.fire_ticks);
            assert_eq!(loaded.health, e.health, "{kind:?}");
            let mut again = Entity::new(0, loaded.body, loaded.yaw, loaded.pitch, loaded.kind);
            again.fire_ticks = loaded.fire_ticks;
            again.health = loaded.health;
            let resaved = save(&again);
            assert_eq!(resaved, saved, "the saved form is stable");
            assert_eq!(bytes(&resaved), bytes(&saved), "byte for byte");
        }
    }

    /// A cloud's effects keep their saved form through a save and load as a player's do: an
    /// infinite one stays infinite (saved as -1), a long one keeps its duration.
    #[test]
    fn a_clouds_infinite_and_long_effects_round_trip() {
        let Some(Kind::Cloud(mut cloud)) =
            kinds().into_iter().find(|k| matches!(k, Kind::Cloud(_)))
        else {
            panic!("a cloud kind");
        };
        let speed = effect::Instance::new(effect::Type::Speed, 0, 1, true, false);
        cloud.effects = vec![
            speed
                .infinite()
                .with_duration_ticks(effect::MAX_DURATION_TICKS),
            speed.with_duration_ticks(2_000_000),
        ];
        let e = entity(Kind::Cloud(cloud.clone()), Vec3::new(5.25, -59.5, -3.75));
        let saved = save(&e);
        let loaded = load_without_custom(&saved, ChunkPos::new(0, -1)).expect("loads");
        assert_eq!(loaded.kind, Kind::Cloud(cloud));
        let Some(Value::List(List::Compound(effects))) = saved.get("mobEffects") else {
            panic!("the effects are saved");
        };
        assert_eq!(effects[0].get("Duration"), Some(&Value::Int(-1)));
    }

    /// An item with less than half a point of health saves with health left, so it loads alive.
    #[test]
    fn an_item_with_a_fraction_of_health_loads_alive() {
        let kind = kinds()
            .into_iter()
            .find(|k| matches!(k, Kind::Item(_)))
            .expect("an item kind");
        let mut e = entity(kind, Vec3::new(5.25, -59.5, -3.75));
        e.health = 0.3;
        let loaded = load_without_custom(&save(&e), ChunkPos::new(0, -1)).expect("loads");
        assert!(loaded.health > 0.0, "health {}", loaded.health);
    }

    /// An entity the tick holds far above or below the world height (an item a flying player
    /// dropped, one shot upwards) loads back with its chunk: the saved form reads every place
    /// an entity may be.
    #[test]
    fn an_entity_far_above_or_below_the_world_loads_back() {
        let kind = kinds()
            .into_iter()
            .find(|k| matches!(k, Kind::Item(_)))
            .expect("an item kind");
        for y in [f64::from(crate::world::MAX_Y) + 100.0, -1000.0] {
            let e = entity(kind.clone(), Vec3::new(5.25, y, -3.75));
            let loaded = load_without_custom(&save(&e), ChunkPos::new(0, -1)).expect("loads");
            assert_eq!(loaded.body, e.body);
        }
        let past = entity(
            kind,
            Vec3::new(5.25, f64::from(crate::world::BORDER) + 1.0, -3.75),
        );
        assert!(load_without_custom(&save(&past), ChunkPos::new(0, -1)).is_none());
    }

    // api: Entity::setHealth
    #[test]
    fn an_npc_saved_without_health_loads_dying() {
        let mut dying = entity(
            Kind::Npc(Character {
                form: Form::Zombie { baby: false },
                invulnerable: false,
                attack_time: 0,
                last_damage: 0.0,
                dead_ticks: Some(0),
                data: Compound::new(),
                held: Stack::EMPTY,
            }),
            Vec3::new(5.25, -59.5, -3.75),
        );
        dying.health = 0.0;
        let loaded = load_without_custom(&save(&dying), ChunkPos::new(0, -1)).expect("loads");
        assert_eq!(loaded.health, 0.0);
        assert!(!loaded.kind.is_living());
        let Kind::Npc(c) = loaded.kind else {
            panic!("{:?}", loaded.kind)
        };
        assert_eq!(c.dead_ticks, Some(1), "past the tick that drops");
    }

    /// The items by count and the xp on the ground.
    fn drops(h: &Harness) -> (u32, u32) {
        let (mut items, mut xp) = (0, 0);
        for e in h.live() {
            match &e.kind {
                Kind::Item(i) => items += u32::from(i.stack.count()),
                Kind::XpOrb(o) => xp += u32::from(o.value),
                _ => {}
            }
        }
        (items, xp)
    }

    #[test]
    fn a_zombie_killed_then_unloaded_and_loaded_drops_once() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        // Out of the player's pickup reach, in its view.
        let z = crate::tick::Context {
            state: &mut h.state,
        }
        .spawn_zombie(crate::world::Id::DEFAULT, Vec3::new(40.5, -60.0, 0.5))
        .expect("spawns");
        crate::tick::Context {
            state: &mut h.state,
        }
        .entity_mut(z)
        .expect("lives")
        .kill();
        h.entities();
        let first = drops(&h);
        assert_eq!(first.1, 5, "the death dropped");
        let key = (0, ChunkPos::new(2, 0));
        let list = h.state.rules.entities.unload(key).expect("changed");
        h.state
            .rules
            .entities
            .load_list(key, &list, &Registry::default());
        h.step(Vec::new());
        let npc = |h: &Harness| {
            let entities = &h.state.rules.entities;
            entities.ids().find(|&id| {
                entities
                    .get(id)
                    .is_some_and(|e| matches!(e.kind, Kind::Npc(_)))
            })
        };
        let back = npc(&h).expect("the zombie came back");
        crate::tick::Context {
            state: &mut h.state,
        }
        .entity_mut(back)
        .expect("not flagged")
        .attack(1.0);
        for _ in 0..crate::tick::entity::kind::Character::MAX_DEAD_TICKS {
            h.step(Vec::new());
        }
        assert_eq!(drops(&h), first, "no second drop");
        assert_eq!(npc(&h), None, "it finished dying");
    }

    #[test]
    fn saved_compounds_that_are_not_entities_here_do_not_load() {
        let e = entity(kinds().remove(0), Vec3::new(5.25, -59.5, -3.75));
        let good = save(&e);
        assert!(load_without_custom(&good, ChunkPos::new(0, -1)).is_some());
        assert!(
            load_without_custom(&good, ChunkPos::new(1, -1)).is_none(),
            "outside its chunk"
        );
        let broken = |edit: &dyn Fn(&mut Compound)| {
            let mut c = good.clone();
            edit(&mut c);
            load_without_custom(&c, ChunkPos::new(0, -1))
        };
        assert!(
            broken(&|c| {
                c.insert(
                    "identifier".to_owned(),
                    Value::String("minecraft:pig".to_owned()),
                );
            })
            .is_none()
        );
        assert!(
            broken(&|c| {
                c.insert("Pos".to_owned(), doubles(Vec3::new(f64::NAN, 0.0, 0.0)));
            })
            .is_none()
        );
        assert!(
            broken(&|c| {
                c.remove("Rotation");
            })
            .is_none()
        );
        assert!(
            broken(&|c| {
                c.insert("Item".to_owned(), Value::Compound(Compound::new()));
            })
            .is_none()
        );
        let fast = broken(&|c| {
            c.insert("Motion".to_owned(), doubles(Vec3::new(1e9, -1e9, 0.5)));
        })
        .unwrap();
        assert_eq!(
            fast.body.motion,
            Vec3::new(10.0, -10.0, 0.5),
            "speeds are cut"
        );
        assert!(
            broken(&|c| {
                c.insert("identifier".to_owned(), Value::String("Item".to_owned()));
            })
            .is_none(),
            "a name Anchor does not write"
        );
        // Out-of-range numbers of a fuse, a projectile and a villager.
        let tnt = save(&entity(kinds().remove(6), Vec3::new(5.25, -59.5, -3.75)));
        let mut bad = tnt.clone();
        bad.insert("Fuse".to_owned(), Value::Short(-3));
        assert!(load_without_custom(&bad, ChunkPos::new(0, -1)).is_none());
        let arrow = save(&entity(kinds().remove(8), Vec3::new(5.25, -59.5, -3.75)));
        let mut bad = arrow.clone();
        bad.insert("damage".to_owned(), Value::Double(f64::INFINITY));
        assert!(load_without_custom(&bad, ChunkPos::new(0, -1)).is_none());
        let mut bad = arrow;
        bad.insert(
            "StuckToBlockPos".to_owned(),
            Value::List(List::Int(vec![1, 2])),
        );
        assert!(load_without_custom(&bad, ChunkPos::new(0, -1)).is_none());
        let npc = save(&entity(kinds().remove(17), Vec3::new(5.25, -59.5, -3.75)));
        let mut bad = npc;
        bad.insert("Profession".to_owned(), Value::Int(1000));
        assert!(load_without_custom(&bad, ChunkPos::new(0, -1)).is_none());
    }

    /// A saved cloud, arrow or human NPC with a value it cannot hold is no entity here and stays
    /// as saved, rather than loading with the value cut or dropped.
    #[test]
    fn saved_values_an_entity_cannot_hold_keep_the_compound_as_saved() {
        let at = Vec3::new(5.25, -59.5, -3.75);
        let chunk = ChunkPos::new(0, -1);
        let saved_of = |pick: fn(&Kind) -> bool| {
            save(&entity(
                kinds().into_iter().find(pick).expect("the kind"),
                at,
            ))
        };
        let refused = |good: &Compound, key: &str, value: Value| {
            let mut bad = good.clone();
            bad.insert(key.to_owned(), value);
            load_without_custom(&bad, chunk).is_none()
        };
        let cloud = saved_of(|k| matches!(k, Kind::Cloud(_)));
        assert!(load_without_custom(&cloud, chunk).is_some());
        let speed = effect::Instance::new(effect::Type::Speed, 0, 1, true, false).to_nbt();
        for (key, value) in [
            ("Radius", Value::Float(100.0)),
            ("RadiusPerTick", Value::Float(f32::NAN)),
            ("Duration", Value::Int(-1)),
            ("DurationOnUse", Value::Int(30_000)),
            ("ReapplicationDelay", Value::Int(30_000)),
            ("SpawnTick", Value::Long(5)),
            ("PotionId", Value::Short(5)),
            (
                "mobEffects",
                Value::List(List::Compound(vec![Compound::new()])),
            ),
            (
                "mobEffects",
                Value::List(List::Compound(vec![speed; Cloud::MAX_EFFECTS + 1])),
            ),
        ] {
            assert!(refused(&cloud, key, value), "{key}");
        }
        let arrow = saved_of(|k| {
            matches!(
                k,
                Kind::Projectile(Projectile {
                    shot: Shot::Arrow(_),
                    ..
                })
            )
        });
        assert!(load_without_custom(&arrow, chunk).is_some());
        assert!(refused(&arrow, "life", Value::Short(-1)));
        let human = saved_of(|k| {
            matches!(
                k,
                Kind::Npc(Character {
                    form: Form::Human { .. },
                    ..
                })
            )
        });
        assert!(load_without_custom(&human, chunk).is_some());
        assert!(refused(&human, "Skin", Value::Compound(Compound::new())));
    }

    /// An entity whose name tag is off loads with it off, with or without a name tag.
    #[test]
    fn a_name_tag_turned_off_stays_off_through_a_save() {
        let mut e = entity(kinds().remove(0), Vec3::new(5.25, -59.5, -3.75));
        e.name_tag_visible = false;
        let loaded = load_without_custom(&save(&e), ChunkPos::new(0, -1)).expect("loads");
        assert!(!loaded.looks.name_tag_visible);
    }

    /// A saved item's health is read within 0 and its maximum, and its owner and thrower within
    /// what the API takes, as an item the API made has them.
    #[test]
    fn a_saved_item_loads_with_the_values_the_api_allows() {
        let good = save(&entity(kinds().remove(0), Vec3::new(5.25, -59.5, -3.75)));
        let load_with = |key: &str, value: Value| {
            let mut c = good.clone();
            c.insert(key.to_owned(), value);
            load_without_custom(&c, ChunkPos::new(0, -1))
        };
        let health = |h: i16| load_with("Health", Value::Short(h)).expect("loads").health;
        let max = kinds().remove(0).max_health();
        assert_eq!(health(30_000), max);
        assert_eq!(health(-5), 0.0);
        let long = Value::String("o".repeat(MAX_NAME_TAG_BYTES + 1));
        assert!(load_with("Owner", long.clone()).is_none());
        assert!(load_with("Thrower", long).is_none());
    }
}
