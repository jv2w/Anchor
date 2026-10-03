//! The saved form of entities: chunk extras kind 2 (PLAN.md 10.7), one Bedrock-like NBT compound
//! per entity, with PMMP's keys (`Entity::saveNBT` and the kinds' overrides).
//!
//! Common keys: `identifier` (the Bedrock actor type; PMMP's save names `Item`, `XPOrb`,
//! `FallingSand`, `PrimedTNT`, `Arrow`, `Snowball`, `Egg`, `ThrownEnderpearl`, `ThrownPotion`,
//! `ThrownExpBottle`, `Villager` are read too), `Pos` and `Motion` (lists of three doubles; floats
//! are read too), `Rotation` (yaw and pitch floats), `FallDistance` (float), `Fire` (short),
//! `OnGround` (byte). Then per kind:
//! - items: `Item` (the stack, [`Stack::to_nbt`]), `Health`, `Age` and `PickupDelay` (shorts),
//!   `Owner` and `Thrower` (strings);
//! - orbs: `Age`, `Value` (short) and `experience value` (int);
//! - falling blocks: `FallingBlock` (the block state as Bedrock's disk palette writes it);
//! - primed TNT: `Fuse` (short), `AllowUnderwater` (byte, only when set);
//! - projectiles: `damage` (double), `StuckToBlockPos` (three ints, while stuck); arrows `pickup`,
//!   `crit` (bytes), `life` (short) and `punch` (byte, Anchor's: PMMP forgets Punch on a load);
//!   potions `PotionId` (short; a lingering one is saved as `minecraft:lingering_potion`, which
//!   PMMP loses);
//! - clouds: PMMP's `AreaEffectCloud` keys; `SpawnTick` is minus the cloud's age (the world clock
//!   is not at hand here; only the difference is read back);
//! - NPCs: `Health` (float), `Invulnerable` (byte); villagers `Profession` (int) and `Offers`
//!   (the offer list, [`Offers::to_nbt`]); human NPCs `CustomName` (the name).
//!
//! Runtime and unique IDs are not saved: they are numbers of one run; so are the owner of a
//! projectile and who a cloud affected (PMMP does not save them either).
//!
//! The file is outside input: a compound that does not describe an entity Anchor knows (an unknown
//! identifier, a missing or invalid key, a position outside its chunk) is not an entity to the
//! tick, and the store keeps it as saved.

use std::collections::BTreeMap;

use super::super::trade::Offers;
use super::Entity;
use super::kind::{
    ARROW_LIFE, Arrow, Character, Cloud, DEFAULT_DESPAWN_DELAY, Falling, Form, Item, Kind,
    MAX_CLOUD_EFFECTS, MAX_DESPAWN_DELAY, MAX_FUSE, NEVER, Orb, Pickup, Projectile, Shot, Tnt,
};
use super::physics::{self, Body};
use crate::effect::{self, Potion};
use crate::item::Stack;
use crate::math::Vec3;
use crate::nbt::{Compound, List, Value};
use crate::protocol::Uuid;
use crate::world::{BlockPos, ChunkPos, MAX_Y, MIN_Y};

/// `Age` that means "never despawns" (PMMP).
const AGE_NEVER: i16 = i16::MIN;
/// The farthest a saved position may be from the world's middle, as for players: the Bedrock
/// world border.
const MAX_COORDINATE: f64 = 30_000_000.0;
/// How far below and above the world height a saved position may be: entities fall out of the
/// world and die 16 blocks under it.
const HEIGHT_MARGIN: f64 = 64.0;
/// The largest projectile base damage read (a bow with Power V gives 5).
const MAX_PROJECTILE_DAMAGE: f64 = 1024.0;
/// The largest cloud radius and radius change read, in blocks (PMMP's default radius is 3).
const MAX_CLOUD_RADIUS: f64 = 64.0;
/// The longest a cloud lives, in ticks (one day of the world; PMMP's default is 600).
const MAX_CLOUD_AGE: i32 = 24_000;
/// The longest effect a cloud gives, in ticks (PMMP's longest potion is 9,600).
const MAX_EFFECT_TICKS: i32 = 1_000_000;
/// The longest NPC name read, in bytes (a player name is at most 16 characters).
const MAX_NAME_BYTES: usize = 256;

/// What a saved compound describes, before the store gives it a slot.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Loaded {
    pub(super) body: Body,
    pub(super) yaw: f64,
    pub(super) pitch: f64,
    pub(super) health: f64,
    pub(super) fire_ticks: i16,
    pub(super) kind: Kind,
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
        y_size: _,
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
    match &entity.kind {
        Kind::Item(item) => {
            put("Item", Value::Compound(item.stack.to_nbt()));
            put("Health", Value::Short(health_short(entity.health)));
            put("Age", Value::Short(age(item.despawn_delay)));
            put("PickupDelay", Value::Short(item.pickup_delay));
            put("Owner", Value::String(item.owner.clone()));
            put("Thrower", Value::String(item.thrower.clone()));
        }
        Kind::ExperienceOrb(orb) => {
            put("Age", Value::Short(age(orb.despawn_delay)));
            let value = i16::try_from(orb.value).unwrap_or(i16::MAX);
            put("Value", Value::Short(value));
            put("experience value", Value::Int(i32::from(value)));
        }
        Kind::FallingBlock(falling) => {
            put(
                "FallingBlock",
                Value::Compound(crate::save::state_nbt(falling.block)),
            );
        }
        Kind::Tnt(tnt) => {
            put(
                "Fuse",
                Value::Short(i16::try_from(tnt.fuse).unwrap_or(i16::MAX)),
            );
            if tnt.underwater {
                put("AllowUnderwater", Value::Byte(1));
            }
        }
        Kind::Projectile(p) => save_projectile(p, &mut put),
        Kind::Cloud(cloud) => save_cloud(cloud, &mut put),
        Kind::Npc(npc) => {
            put("Health", Value::Float(to_f32(entity.health)));
            put("Invulnerable", Value::Byte(u8::from(npc.invulnerable)));
            match &npc.form {
                Form::Villager { profession, offers } => {
                    put("Profession", Value::Int(i32::from(*profession)));
                    put("Offers", Value::Compound(offers.to_nbt()));
                }
                Form::Human { name, uuid: _ } => {
                    put("CustomName", Value::String(name.clone()));
                }
            }
        }
    }
    c
}

/// A projectile's keys (PMMP `Projectile::saveNBT`, `Arrow::saveNBT`, `SplashPotion::saveNBT`).
fn save_projectile(p: &Projectile, put: &mut impl FnMut(&str, Value)) {
    put("damage", Value::Double(p.damage));
    if let Some(pos) = p.stuck {
        put(
            "StuckToBlockPos",
            Value::List(List::Int(vec![pos.x, pos.y, pos.z])),
        );
    }
    match p.shot {
        Shot::Arrow(a) => {
            put("pickup", Value::Byte(a.pickup.to_byte()));
            put("crit", Value::Byte(u8::from(a.critical)));
            put("life", Value::Short(a.life));
            if a.punch > 0 {
                put("punch", Value::Byte(a.punch));
            }
        }
        Shot::Potion { potion, .. } => {
            put("PotionId", Value::Short(i16::from(potion.id())));
        }
        Shot::Snowball | Shot::Egg | Shot::EnderPearl | Shot::ExperienceBottle => {}
    }
}

/// A cloud's keys (PMMP `AreaEffectCloud::saveNBT`).
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
        let effects = cloud
            .effects
            .iter()
            .map(|e| {
                Compound::from([
                    ("Id".to_owned(), Value::Byte(e.effect().id())),
                    ("Amplifier".to_owned(), Value::Byte(e.amplifier())),
                    (
                        "Duration".to_owned(),
                        Value::Int(i32::try_from(e.duration()).unwrap_or(i32::MAX)),
                    ),
                    ("Ambient".to_owned(), Value::Byte(u8::from(e.is_ambient()))),
                    (
                        "ShowParticles".to_owned(),
                        Value::Byte(u8::from(e.is_visible())),
                    ),
                ])
            })
            .collect();
        put("mobEffects", Value::List(List::Compound(effects)));
    }
}

/// The entity a saved compound of chunk `chunk` describes, or `None` (the store keeps such a
/// compound as it is).
pub(super) fn load(c: &Compound, chunk: ChunkPos) -> Option<Loaded> {
    let identifier = match c.get("identifier").or_else(|| c.get("id")) {
        Some(Value::String(s)) => s.as_str(),
        _ => return None,
    };
    let position = vector(c.get("Pos"))?;
    let border = |v: f64| v.abs() <= MAX_COORDINATE;
    let height = f64::from(MIN_Y) - HEIGHT_MARGIN..=f64::from(MAX_Y) + HEIGHT_MARGIN;
    if !border(position.x) || !border(position.z) || !height.contains(&position.y) {
        return None;
    }
    if super::chunk_of(position) != chunk {
        return None;
    }
    let motion = match c.get("Motion") {
        None => Vec3::ZERO,
        some => physics::bounded(vector(some)?),
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
    let kind = load_kind(c, identifier)?;
    let health = match (&kind, c.get("Health")) {
        (Kind::Item(_), Some(Value::Short(h))) => f64::from(*h),
        (Kind::Npc(_), Some(Value::Float(h))) if h.is_finite() => {
            crate::math::clamp(f64::from(*h), 0.0, kind.max_health())
        }
        _ => kind.max_health(),
    };
    Some(Loaded {
        body: Body {
            position,
            motion,
            on_ground,
            fall_distance,
            y_size: 0.0,
        },
        yaw,
        pitch,
        health,
        fire_ticks,
        kind,
    })
}

/// The kind a saved compound with `identifier` describes, with its own keys.
fn load_kind(c: &Compound, identifier: &str) -> Option<Kind> {
    Some(match identifier {
        "minecraft:item" | "Item" => Kind::Item(load_item(c)?),
        "minecraft:xp_orb" | "XPOrb" => Kind::ExperienceOrb(load_orb(c)),
        "minecraft:falling_block" | "FallingSand" => {
            let Some(Value::Compound(block)) = c.get("FallingBlock") else {
                return None;
            };
            Kind::FallingBlock(Falling {
                block: crate::save::find_state(block)?,
            })
        }
        "minecraft:tnt" | "PrimedTNT" | "PrimedTnt" => Kind::Tnt(Tnt {
            fuse: match c.get("Fuse") {
                Some(Value::Short(f)) => u16::try_from(*f).ok().filter(|f| *f <= MAX_FUSE)?,
                _ => 80,
            },
            underwater: matches!(c.get("AllowUnderwater"), Some(Value::Byte(b)) if *b != 0),
        }),
        "minecraft:arrow" | "Arrow" => load_projectile(c, Shot::Arrow(load_arrow(c)))?,
        "minecraft:snowball" | "Snowball" => load_projectile(c, Shot::Snowball)?,
        "minecraft:egg" | "Egg" => load_projectile(c, Shot::Egg)?,
        "minecraft:ender_pearl" | "ThrownEnderpearl" => load_projectile(c, Shot::EnderPearl)?,
        "minecraft:xp_bottle" | "ThrownExpBottle" => load_projectile(c, Shot::ExperienceBottle)?,
        "minecraft:splash_potion" | "ThrownPotion" => load_projectile(c, potion(c, false)?)?,
        "minecraft:lingering_potion" => load_projectile(c, potion(c, true)?)?,
        "minecraft:area_effect_cloud" => Kind::Cloud(load_cloud(c)?),
        "minecraft:villager_v2" | "minecraft:villager" | "Villager" => Kind::Npc(load_npc(
            c,
            Form::Villager {
                profession: match c.get("Profession") {
                    Some(Value::Int(p)) => u8::try_from(*p).ok()?,
                    _ => 0,
                },
                // The trade module bounds and checks the offers it reads (PLAN.md 8.6).
                offers: match c.get("Offers") {
                    Some(Value::Compound(offers)) => Offers::from_nbt(offers),
                    None => Offers::default(),
                    Some(_) => return None,
                },
            },
        )),
        "Human" => Kind::Npc(load_npc(
            c,
            Form::Human {
                name: match c.get("CustomName") {
                    Some(Value::String(s)) if s.len() <= MAX_NAME_BYTES => s.clone(),
                    Some(_) => return None,
                    None => String::new(),
                },
                uuid: Uuid::from_bytes([0; 16]),
            },
        )),
        _ => return None,
    })
}

/// A dropped item's keys (PMMP `ItemEntity::initEntity`); `None` without a valid stack.
fn load_item(c: &Compound) -> Option<Item> {
    let Some(Value::Compound(stack)) = c.get("Item") else {
        return None;
    };
    let stack = Stack::from_nbt(stack).filter(|s| !s.is_empty())?;
    let pickup_delay = match c.get("PickupDelay") {
        Some(Value::Short(d)) => (*d).max(NEVER),
        _ => 0,
    };
    let text = |k: &str| match c.get(k) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    Some(Item {
        stack,
        pickup_delay,
        despawn_delay: despawn_delay(c),
        owner: text("Owner"),
        thrower: text("Thrower"),
    })
}

/// An orb's keys (PMMP's `XPOrb` factory and `initEntity`): `Value`, else `experience value`,
/// else 1; values over the `Value` short are cut to it.
fn load_orb(c: &Compound) -> Orb {
    let value = match (c.get("Value"), c.get("experience value")) {
        (Some(Value::Short(v)), _) => i32::from(*v),
        (_, Some(Value::Int(v))) => *v,
        _ => 1,
    };
    let value = u16::try_from(value.clamp(1, i32::from(i16::MAX))).unwrap_or(1);
    Orb {
        value,
        despawn_delay: despawn_delay(c),
        look_for_target: 0,
        target: None,
    }
}

/// A projectile's keys (PMMP `Projectile::initEntity`): the damage (finite, 0 to
/// [`MAX_PROJECTILE_DAMAGE`]) and the block it is stuck in.
fn load_projectile(c: &Compound, shot: Shot) -> Option<Kind> {
    let damage = match c.get("damage") {
        Some(Value::Double(d)) if d.is_finite() && (0.0..=MAX_PROJECTILE_DAMAGE).contains(d) => *d,
        Some(_) => return None,
        None => match shot {
            Shot::Arrow(_) => super::projectile::ARROW_DAMAGE,
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

/// An arrow's keys (PMMP `Arrow::initEntity`).
fn load_arrow(c: &Compound) -> Arrow {
    let byte = |k: &str| match c.get(k) {
        Some(Value::Byte(b)) => Some(*b),
        _ => None,
    };
    Arrow {
        critical: byte("crit") == Some(1),
        pickup: byte("pickup").map_or(Pickup::Any, Pickup::from_byte),
        punch: byte("punch").unwrap_or(0),
        life: match c.get("life") {
            Some(Value::Short(l)) => (*l).clamp(0, ARROW_LIFE),
            _ => 0,
        },
    }
}

/// A potion's `PotionId` (PMMP `SplashPotion`); water when missing.
fn potion(c: &Compound, linger: bool) -> Option<Shot> {
    let potion = match c.get("PotionId") {
        Some(Value::Short(id)) => Potion::from_id(i32::from(*id))?,
        _ => Potion::from_id(0)?,
    };
    Some(Shot::Potion { potion, linger })
}

/// A cloud's keys (PMMP `AreaEffectCloud::initEntity`), with every number bounded.
fn load_cloud(c: &Compound) -> Option<Cloud> {
    let int = |k: &str, default: i32| match c.get(k) {
        Some(Value::Int(v)) => *v,
        _ => default,
    };
    let float = |k: &str, default: f64| match c.get(k) {
        Some(Value::Float(v)) if v.is_finite() => {
            crate::math::clamp(f64::from(*v), -MAX_CLOUD_RADIUS, MAX_CLOUD_RADIUS)
        }
        _ => default,
    };
    let age = match c.get("SpawnTick") {
        Some(Value::Long(t)) => {
            u32::try_from(t.saturating_neg().clamp(0, i64::from(MAX_CLOUD_AGE))).unwrap_or(0)
        }
        _ => 0,
    };
    let mut effects = Vec::new();
    match c.get("mobEffects") {
        Some(Value::List(List::Compound(list))) => {
            for e in list.iter().take(MAX_CLOUD_EFFECTS) {
                let (Some(Value::Byte(id)), Some(Value::Byte(amp)), Some(Value::Int(d))) =
                    (e.get("Id"), e.get("Amplifier"), e.get("Duration"))
                else {
                    continue;
                };
                let Some(t) = effect::Type::from_id(i32::from(*id)) else {
                    continue;
                };
                let flag = |k: &str, default: bool| match e.get(k) {
                    Some(Value::Byte(b)) => *b != 0,
                    _ => default,
                };
                let duration = u32::try_from((*d).clamp(0, MAX_EFFECT_TICKS)).unwrap_or(0);
                effects.push(effect::Instance::new(
                    t,
                    duration,
                    *amp,
                    flag("ShowParticles", true),
                    flag("Ambient", false),
                ));
            }
        }
        Some(Value::List(List::End)) | None => {}
        Some(_) => return None,
    }
    let initial_radius = float("InitialRadius", super::projectile::CLOUD_RADIUS);
    Some(Cloud {
        effects,
        age,
        max_age: int("Duration", super::projectile::CLOUD_DURATION).clamp(0, MAX_CLOUD_AGE),
        max_age_on_use: int("DurationOnUse", 0).clamp(-MAX_CLOUD_AGE, MAX_CLOUD_AGE),
        reapplication_delay: int("ReapplicationDelay", super::projectile::CLOUD_REAPPLICATION)
            .clamp(0, MAX_CLOUD_AGE),
        pickup_count: int("PickupCount", 0),
        initial_radius,
        radius: float("Radius", initial_radius),
        radius_on_pickup: float(
            "RadiusChangeOnPickup",
            super::projectile::CLOUD_RADIUS_ON_USE,
        ),
        radius_on_use: float("RadiusOnUse", super::projectile::CLOUD_RADIUS_ON_USE),
        radius_per_tick: float("RadiusPerTick", super::projectile::CLOUD_RADIUS_PER_TICK),
        victims: BTreeMap::new(),
    })
}

/// An NPC's keys.
fn load_npc(c: &Compound, form: Form) -> Character {
    Character {
        form,
        invulnerable: matches!(c.get("Invulnerable"), Some(Value::Byte(b)) if *b != 0),
        attack_time: 0,
        last_damage: 0.0,
        dead_ticks: None,
    }
}

/// The despawn delay a saved `Age` gives (PMMP: `-32768` never, otherwise 6000 minus the age, at
/// least 0).
fn despawn_delay(c: &Compound) -> i32 {
    match c.get("Age") {
        Some(Value::Short(AGE_NEVER)) => i32::from(NEVER),
        Some(Value::Short(a)) => {
            (DEFAULT_DESPAWN_DELAY - i32::from(*a)).clamp(0, MAX_DESPAWN_DELAY)
        }
        _ => DEFAULT_DESPAWN_DELAY,
    }
}

/// The `Age` short of a despawn delay.
fn age(despawn_delay: i32) -> i16 {
    if despawn_delay == i32::from(NEVER) {
        AGE_NEVER
    } else {
        let a = DEFAULT_DESPAWN_DELAY - despawn_delay;
        i16::try_from(a.clamp(i32::from(i16::MIN) + 1, i32::from(i16::MAX))).unwrap_or(0)
    }
}

/// The `Health` short of a health value.
#[expect(
    clippy::cast_possible_truncation,
    reason = "invariant: entity health is finite and at most 20; `as` saturates otherwise"
)]
fn health_short(health: f64) -> i16 {
    health.round() as i16
}

/// An `f32` of a tick value for the saved form (rounded to nearest).
#[expect(
    clippy::cast_possible_truncation,
    reason = "consistency: the saved form keeps rotations, radii and fall distances as floats"
)]
fn to_f32(v: f64) -> f32 {
    v as f32
}

/// A list of three doubles.
fn doubles(v: Vec3) -> Value {
    Value::List(List::Double(vec![v.x, v.y, v.z]))
}

/// A finite vector from a list of three doubles or floats (PMMP `EntityDataHelper::parseVec3`).
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
    use super::super::physics::tests::named;
    use super::*;
    use crate::block;
    use crate::item::{Enchantment, Extra, Type};

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
            y_size: 0.0,
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
            .with_extra(Some(Extra {
                custom_name: Some("Edge".to_owned()),
                lore: vec!["one".to_owned(), "two".to_owned()],
                enchantments: vec![(Enchantment::all()[0], 3)],
                nbt: Compound::from([("RepairCost".to_owned(), Value::Int(4))]),
            }));
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
                pickup_delay: 7,
                despawn_delay: 5000,
                owner: "owner".to_owned(),
                thrower: String::new(),
            }),
            Kind::Item(Item {
                stack: enchanted,
                pickup_delay: NEVER,
                despawn_delay: i32::from(NEVER),
                owner: String::new(),
                thrower: "thrower".to_owned(),
            }),
            Kind::Item(Item {
                stack: stack("minecraft:oak_planks", 64),
                pickup_delay: 0,
                despawn_delay: MAX_DESPAWN_DELAY,
                owner: String::new(),
                thrower: String::new(),
            }),
            Kind::ExperienceOrb(Orb {
                value: 2477,
                despawn_delay: 1,
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
                    pickup: Pickup::Creative,
                    punch: 2,
                    life: 0,
                }),
                3.5,
                None,
            ),
            projectile(
                Shot::Arrow(Arrow {
                    critical: false,
                    pickup: Pickup::Any,
                    punch: 0,
                    life: 300,
                }),
                2.0,
                Some(BlockPos::new(5, -61, -4)),
            ),
            projectile(Shot::Snowball, 0.0, None),
            projectile(Shot::Egg, 0.0, None),
            projectile(Shot::EnderPearl, 0.0, None),
            projectile(Shot::ExperienceBottle, 0.0, None),
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
                    offers: Offers::from_nbt(&crate::nbt::Compound::from([(
                        "Recipes".to_owned(),
                        Value::List(crate::nbt::List::Compound(vec![offer(0), offer(1)])),
                    )])),
                },
                invulnerable: true,
                attack_time: 0,
                last_damage: 0.0,
                dead_ticks: None,
            }),
            Kind::Npc(Character {
                form: Form::Human {
                    name: "Guide".to_owned(),
                    uuid: Uuid::from_bytes([0; 16]),
                },
                invulnerable: false,
                attack_time: 0,
                last_damage: 0.0,
                dead_ticks: None,
            }),
        ]
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
        for kind in kinds() {
            let mut e = entity(kind.clone(), at);
            if matches!(kind, Kind::Npc(_)) {
                e.health = 13.0;
            }
            let saved = save(&e);
            let loaded = load(&saved, ChunkPos::new(0, -1)).expect("loads");
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

    #[test]
    fn saved_compounds_that_are_not_entities_here_do_not_load() {
        let e = entity(kinds().remove(0), Vec3::new(5.25, -59.5, -3.75));
        let good = save(&e);
        assert!(load(&good, ChunkPos::new(0, -1)).is_some());
        assert!(
            load(&good, ChunkPos::new(1, -1)).is_none(),
            "outside its chunk"
        );
        let broken = |edit: &dyn Fn(&mut Compound)| {
            let mut c = good.clone();
            edit(&mut c);
            load(&c, ChunkPos::new(0, -1))
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
        assert_eq!(
            broken(&|c| {
                c.insert("identifier".to_owned(), Value::String("Item".to_owned()));
            })
            .unwrap()
            .kind,
            e.kind,
            "PMMP's save name"
        );
        // Out-of-range numbers of the new kinds.
        let tnt = save(&entity(kinds().remove(6), Vec3::new(5.25, -59.5, -3.75)));
        let mut bad = tnt.clone();
        bad.insert("Fuse".to_owned(), Value::Short(-3));
        assert!(load(&bad, ChunkPos::new(0, -1)).is_none());
        let arrow = save(&entity(kinds().remove(8), Vec3::new(5.25, -59.5, -3.75)));
        let mut bad = arrow.clone();
        bad.insert("damage".to_owned(), Value::Double(f64::INFINITY));
        assert!(load(&bad, ChunkPos::new(0, -1)).is_none());
        let mut bad = arrow;
        bad.insert(
            "StuckToBlockPos".to_owned(),
            Value::List(List::Int(vec![1, 2])),
        );
        assert!(load(&bad, ChunkPos::new(0, -1)).is_none());
        let npc = save(&entity(kinds().remove(17), Vec3::new(5.25, -59.5, -3.75)));
        let mut bad = npc;
        bad.insert("Profession".to_owned(), Value::Int(1000));
        assert!(load(&bad, ChunkPos::new(0, -1)).is_none());
    }
}
