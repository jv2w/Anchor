//! The survival keys of a player's saved extra, with PMMP's names and types (PMMP
//! `Player::getSaveData`, `Human::saveNBT`, `Living::saveNBT`, `Entity::saveNBT`), plus
//! `AbsorptionAmount`, `SleepingPos` and `PendingDeathDrops` (the held-back death drops, as
//! Bedrock item compounds), which PMMP does not keep.
//!
//! The extra comes from the world file, so every value is checked on load: a wrong type or a
//! value out of range keeps the default (PLAN.md 2.1); nothing here panics.

use tracing::warn;

use super::effects::{Active, MAX_DURATION};
use super::{MAX_AIR, MAX_FOOD, MAX_HELD_BACK, MAX_SATURATION, Point, Survival};
use crate::config::GameMode;
use crate::effect::Type as Effect;
use crate::item::Stack;
use crate::nbt::{Compound, List, Value};
use crate::world::BlockPos;

const GAME_MODE: &str = "playerGameType";
const HEALTH: &str = "Health";
const ABSORPTION: &str = "AbsorptionAmount";
const FOOD: &str = "foodLevel";
const SATURATION: &str = "foodSaturationLevel";
const EXHAUSTION: &str = "foodExhaustionLevel";
const FOOD_TIMER: &str = "foodTickTimer";
const XP_LEVEL: &str = "XpLevel";
const XP_PROGRESS: &str = "XpP";
const XP_TOTAL: &str = "XpTotal";
const XP_SEED: &str = "XpSeed";
const EFFECTS: &str = "ActiveEffects";
const FIRE: &str = "Fire";
const AIR: &str = "Air";
const FALL: &str = "FallDistance";
const SPAWN_WORLD: &str = "SpawnLevel";
const SPAWN: [&str; 3] = ["SpawnX", "SpawnY", "SpawnZ"];
const DEATH_WORLD: &str = "DeathLevel";
const DEATH: [&str; 3] = ["DeathPositionX", "DeathPositionY", "DeathPositionZ"];
const SLEEPING: &str = "SleepingPos";
const HELD_BACK: &str = "PendingDeathDrops";

const EFFECT_ID: &str = "Id";
const EFFECT_AMPLIFIER: &str = "Amplifier";
const EFFECT_DURATION: &str = "Duration";
const EFFECT_AMBIENT: &str = "Ambient";
const EFFECT_PARTICLES: &str = "ShowParticles";

/// The most falling distance kept (a longer fall kills anyway).
const MAX_FALL: f64 = 4096.0;
/// The most exhaustion kept (PMMP `Attribute::EXHAUSTION` maximum).
const MAX_EXHAUSTION: f64 = 5.0;
/// The most effects read: one per effect type.
const MAX_EFFECTS: usize = 27;

/// The protocol's game type of a game mode (`SetPlayerGameType`, `StartGame`).
pub(in crate::tick) fn game_mode_id(mode: GameMode) -> i32 {
    match mode {
        GameMode::Survival => 0,
        GameMode::Creative => 1,
        GameMode::Adventure => 2,
        GameMode::Spectator => 6,
    }
}

/// The saved ID of a game mode (PMMP `GameModeIdMap`).
fn saved_mode(mode: GameMode) -> i32 {
    match mode {
        GameMode::Survival => 0,
        GameMode::Creative => 1,
        GameMode::Adventure => 2,
        GameMode::Spectator => 3,
    }
}

fn int(extra: &Compound, key: &str) -> Option<i32> {
    match extra.get(key) {
        Some(Value::Int(v)) => Some(*v),
        _ => None,
    }
}

fn short(extra: &Compound, key: &str) -> Option<i16> {
    match extra.get(key) {
        Some(Value::Short(v)) => Some(*v),
        _ => None,
    }
}

fn byte(extra: &Compound, key: &str) -> Option<u8> {
    match extra.get(key) {
        Some(Value::Byte(v)) => Some(*v),
        _ => None,
    }
}

/// A float key that is finite and within `lo..=hi`.
fn float(extra: &Compound, key: &str, lo: f64, hi: f64) -> Option<f64> {
    match extra.get(key) {
        Some(Value::Float(v)) => {
            let v = f64::from(*v);
            (v.is_finite() && v >= lo && v <= hi).then_some(v)
        }
        _ => None,
    }
}

fn string<'a>(extra: &'a Compound, key: &str) -> Option<&'a str> {
    match extra.get(key) {
        Some(Value::String(v)) => Some(v),
        _ => None,
    }
}

/// A point saved as a world name and three int keys.
fn point(extra: &Compound, world: &str, keys: [&str; 3]) -> Option<Point> {
    let world = string(extra, world)?;
    let [x, y, z] = keys.map(|k| int(extra, k));
    Some(Point {
        world: world.to_owned(),
        pos: BlockPos::new(x?, y?, z?),
    })
}

/// Reads the survival keys; each missing or bad key keeps its default.
pub(super) fn load(survival: &mut Survival, extra: &Compound) {
    let s = survival;
    s.game_mode = int(extra, GAME_MODE).and_then(|mode| match mode {
        0 => Some(GameMode::Survival),
        1 => Some(GameMode::Creative),
        2 => Some(GameMode::Adventure),
        3 => Some(GameMode::Spectator),
        _ => None,
    });
    if let Some(Value::List(List::Compound(list))) = extra.get(EFFECTS) {
        for e in list.iter().take(MAX_EFFECTS) {
            if let Some((effect, active)) = load_effect(e) {
                s.effects.insert(effect, active);
            }
        }
    }
    if let Some(health) = float(extra, HEALTH, 0.0, f64::MAX) {
        let max = s.max_health();
        s.health = if health > max { max } else { health };
    }
    if let Some(absorption) = float(extra, ABSORPTION, 0.0, f64::from(f32::MAX)) {
        s.absorption = absorption;
    }
    if let Some(food) = int(extra, FOOD).filter(|food| (0..=20).contains(food)) {
        s.food = f64::from(food);
    }
    if let Some(saturation) = float(extra, SATURATION, 0.0, MAX_SATURATION) {
        s.saturation = saturation;
    }
    if let Some(exhaustion) = float(extra, EXHAUSTION, 0.0, MAX_EXHAUSTION) {
        s.exhaustion = exhaustion;
    }
    if let Some(timer) = int(extra, FOOD_TIMER).and_then(|timer| u32::try_from(timer).ok()) {
        s.food_timer = timer % 80;
    }
    if let Some(level) = int(extra, XP_LEVEL).and_then(|level| u32::try_from(level).ok()) {
        s.xp_level = level.min(super::food::MAX_LEVEL);
    }
    if let Some(progress) = float(extra, XP_PROGRESS, 0.0, 1.0) {
        s.xp_progress = if progress >= 1.0 { 0.0 } else { progress };
    }
    if let Some(total) = int(extra, XP_TOTAL).and_then(|total| u32::try_from(total).ok()) {
        s.xp_lifetime = total;
    }
    s.xp_seed = int(extra, XP_SEED);
    if let Some(fire) = short(extra, FIRE).and_then(|fire| u16::try_from(fire).ok()) {
        s.fire_ticks = fire;
    }
    if let Some(air) = short(extra, AIR) {
        s.air = air.clamp(-20, MAX_AIR);
        s.breathing = s.air >= MAX_AIR;
    }
    if let Some(fall) = float(extra, FALL, 0.0, MAX_FALL) {
        s.fall_distance = fall;
    }
    s.spawn = point(extra, SPAWN_WORLD, SPAWN);
    s.death = point(extra, DEATH_WORLD, DEATH);
    s.sleeping = match extra.get(SLEEPING) {
        Some(Value::IntArray(pos)) => match pos.as_slice() {
            &[x, y, z] => Some(BlockPos::new(x, y, z)),
            _ => None,
        },
        _ => None,
    };
    if s.food > MAX_FOOD {
        s.food = MAX_FOOD;
    }
    s.held_back = match extra.get(HELD_BACK) {
        Some(Value::List(List::Compound(list))) => list
            .iter()
            .take(MAX_HELD_BACK)
            .filter_map(|nbt| {
                let stack = Stack::from_nbt(nbt);
                if stack.is_none() {
                    // PMMP `Item::safeNbtDeserialize`: an item it does not know is left out.
                    warn!(?nbt, "a held-back death drop is unknown; it is left out");
                }
                stack.filter(|s| !s.is_empty())
            })
            .collect(),
        _ => Vec::new(),
    };
}

/// One saved effect (PMMP `Living::initEntity`): an unknown ID, or a negative duration other
/// than -1 (infinite), is skipped.
fn load_effect(e: &Compound) -> Option<(Effect, Active)> {
    let effect = Effect::from_id(i32::from(byte(e, EFFECT_ID)?))?;
    let duration = int(e, EFFECT_DURATION)?;
    let infinite = duration == -1;
    let duration = if infinite {
        MAX_DURATION
    } else {
        u32::try_from(duration).ok()?
    };
    Some((
        effect,
        Active {
            duration,
            amplifier: byte(e, EFFECT_AMPLIFIER)?,
            visible: byte(e, EFFECT_PARTICLES).unwrap_or(1) != 0,
            ambient: byte(e, EFFECT_AMBIENT).unwrap_or(0) != 0,
            infinite,
        },
    ))
}

/// Narrows a tick value to the `f32` the file keeps.
fn f32_of(v: f64) -> f32 {
    super::f32_of(v)
}

/// Writes the survival keys, replacing the ones read; keys of absent values are removed.
pub(super) fn save(s: &Survival, extra: &mut Compound) {
    let mut put = |key: &str, value: Option<Value>| match value {
        Some(v) => {
            extra.insert(key.to_owned(), v);
        }
        None => {
            extra.remove(key);
        }
    };
    put(GAME_MODE, s.game_mode.map(|m| Value::Int(saved_mode(m))));
    put(HEALTH, Some(Value::Float(f32_of(s.health))));
    put(ABSORPTION, Some(Value::Float(f32_of(s.absorption))));
    #[expect(
        clippy::cast_possible_truncation,
        reason = "invariant: the food level is a whole number from 0 to 20"
    )]
    let food = s.food as i32;
    put(FOOD, Some(Value::Int(food)));
    put(SATURATION, Some(Value::Float(f32_of(s.saturation))));
    put(EXHAUSTION, Some(Value::Float(f32_of(s.exhaustion))));
    put(
        FOOD_TIMER,
        Some(Value::Int(i32::try_from(s.food_timer).unwrap_or(0))),
    );
    put(
        XP_LEVEL,
        Some(Value::Int(i32::try_from(s.xp_level).unwrap_or(0))),
    );
    put(XP_PROGRESS, Some(Value::Float(f32_of(s.xp_progress))));
    put(
        XP_TOTAL,
        Some(Value::Int(i32::try_from(s.xp_lifetime).unwrap_or(i32::MAX))),
    );
    put(XP_SEED, s.xp_seed.map(Value::Int));
    let effects: Vec<Compound> = s
        .effects
        .iter()
        .map(|(effect, a)| {
            let mut c = Compound::new();
            c.insert(EFFECT_ID.to_owned(), Value::Byte(effect.id()));
            c.insert(EFFECT_AMPLIFIER.to_owned(), Value::Byte(a.amplifier));
            let duration = if a.infinite {
                -1
            } else {
                i32::try_from(a.duration).unwrap_or(i32::MAX)
            };
            c.insert(EFFECT_DURATION.to_owned(), Value::Int(duration));
            c.insert(EFFECT_AMBIENT.to_owned(), Value::Byte(u8::from(a.ambient)));
            c.insert(
                EFFECT_PARTICLES.to_owned(),
                Value::Byte(u8::from(a.visible)),
            );
            c
        })
        .collect();
    put(
        EFFECTS,
        (!effects.is_empty()).then_some(Value::List(List::Compound(effects))),
    );
    put(
        FIRE,
        Some(Value::Short(
            i16::try_from(s.fire_ticks).unwrap_or(i16::MAX),
        )),
    );
    put(AIR, Some(Value::Short(s.air)));
    put(FALL, Some(Value::Float(f32_of(s.fall_distance))));
    for (world, keys, point) in [
        (SPAWN_WORLD, SPAWN, &s.spawn),
        (DEATH_WORLD, DEATH, &s.death),
    ] {
        put(
            world,
            point.as_ref().map(|p| Value::String(p.world.clone())),
        );
        let coords = point.as_ref().map(|p| [p.pos.x, p.pos.y, p.pos.z]);
        for (i, key) in keys.into_iter().enumerate() {
            put(key, coords.and_then(|c| c.get(i).copied()).map(Value::Int));
        }
    }
    put(
        SLEEPING,
        s.sleeping.map(|b| Value::IntArray(vec![b.x, b.y, b.z])),
    );
    put(
        HELD_BACK,
        (!s.held_back.is_empty()).then(|| {
            Value::List(List::Compound(
                s.held_back.iter().map(Stack::to_nbt).collect(),
            ))
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::extra::hash_nbt;

    /// A state with a value in every saved field.
    fn full() -> Survival {
        let mut s = Survival {
            game_mode: Some(GameMode::Adventure),
            health: 13.5,
            absorption: 4.0,
            food: 17.0,
            saturation: 2.5,
            exhaustion: 1.25,
            food_timer: 33,
            xp_level: 21,
            xp_progress: 0.5,
            xp_lifetime: 1234,
            xp_seed: Some(-77),
            fire_ticks: 120,
            air: 150,
            breathing: false,
            fall_distance: 2.75,
            spawn: Some(Point {
                world: "world".to_owned(),
                pos: BlockPos::new(1, -60, 3),
            }),
            death: Some(Point {
                world: "nether".to_owned(),
                pos: BlockPos::new(-5, 70, 9),
            }),
            sleeping: Some(BlockPos::new(4, 5, 6)),
            ..Survival::default()
        };
        s.effects.insert(Effect::Speed, Active::new(200, 1));
        let mut infinite = Active::new(MAX_DURATION, 0);
        infinite.infinite = true;
        infinite.ambient = true;
        infinite.visible = false;
        s.effects.insert(Effect::NightVision, infinite);
        s
    }

    #[test]
    fn every_key_survives_a_save_and_load() {
        let s = full();
        let mut extra = Compound::new();
        extra.insert("Other".to_owned(), Value::String("kept".to_owned()));
        save(&s, &mut extra);
        let mut loaded = Survival::default();
        load(&mut loaded, &extra);
        let mut again = extra.clone();
        save(&loaded, &mut again);
        assert_eq!(extra, again, "the bytes of unchanged data stay the same");
        assert_eq!(loaded.game_mode, s.game_mode);
        assert_eq!(loaded.health, s.health);
        assert_eq!(loaded.absorption, s.absorption);
        assert_eq!(loaded.food, s.food);
        assert_eq!(loaded.saturation, s.saturation);
        assert_eq!(loaded.exhaustion, s.exhaustion);
        assert_eq!(loaded.food_timer, s.food_timer);
        assert_eq!(loaded.xp_level, s.xp_level);
        assert_eq!(loaded.xp_progress, s.xp_progress);
        assert_eq!(loaded.xp_lifetime, s.xp_lifetime);
        assert_eq!(loaded.xp_seed, s.xp_seed);
        assert_eq!(loaded.effects, s.effects);
        assert_eq!(loaded.fire_ticks, s.fire_ticks);
        assert_eq!(loaded.air, s.air);
        assert_eq!(loaded.fall_distance, s.fall_distance);
        assert_eq!(loaded.spawn, s.spawn);
        assert_eq!(loaded.death, s.death);
        assert_eq!(loaded.sleeping, s.sleeping);
        assert_eq!(extra.get("Other"), Some(&Value::String("kept".to_owned())));
        let mut h1 = crate::tick::Fnv::new();
        hash_nbt(&mut h1, &extra);
        let mut h2 = crate::tick::Fnv::new();
        hash_nbt(&mut h2, &again);
        assert_eq!(h1.0, h2.0);
    }

    #[test]
    fn a_new_player_saves_defaults_and_absent_keys_are_removed() {
        let mut extra = Compound::new();
        save(&full(), &mut extra);
        save(&Survival::default(), &mut extra);
        for key in [
            GAME_MODE,
            XP_SEED,
            EFFECTS,
            SPAWN_WORLD,
            DEATH_WORLD,
            SLEEPING,
        ] {
            assert!(!extra.contains_key(key), "{key}");
        }
        assert_eq!(extra.get(HEALTH), Some(&Value::Float(20.0)));
        assert_eq!(extra.get(SATURATION), Some(&Value::Float(5.0)));
    }

    #[test]
    fn bad_saved_values_keep_the_defaults() {
        let mut extra = Compound::new();
        extra.insert(HEALTH.to_owned(), Value::Float(f32::NAN));
        extra.insert(FOOD.to_owned(), Value::Int(99));
        extra.insert(SATURATION.to_owned(), Value::Float(-1.0));
        extra.insert(XP_LEVEL.to_owned(), Value::Int(-3));
        extra.insert(XP_PROGRESS.to_owned(), Value::Float(f32::INFINITY));
        extra.insert(FIRE.to_owned(), Value::Short(-5));
        extra.insert(AIR.to_owned(), Value::Short(i16::MIN));
        extra.insert(GAME_MODE.to_owned(), Value::Int(7));
        extra.insert(SPAWN_WORLD.to_owned(), Value::String("w".to_owned()));
        extra.insert(SPAWN[0].to_owned(), Value::Int(1));
        let mut list = Vec::new();
        let mut bad = Compound::new();
        bad.insert(EFFECT_ID.to_owned(), Value::Byte(250));
        bad.insert(EFFECT_DURATION.to_owned(), Value::Int(10));
        bad.insert(EFFECT_AMPLIFIER.to_owned(), Value::Byte(0));
        list.push(bad);
        let mut negative = Compound::new();
        negative.insert(EFFECT_ID.to_owned(), Value::Byte(Effect::Speed.id()));
        negative.insert(EFFECT_DURATION.to_owned(), Value::Int(-7));
        negative.insert(EFFECT_AMPLIFIER.to_owned(), Value::Byte(0));
        list.push(negative);
        list.extend(std::iter::repeat_n(Compound::new(), 1000));
        extra.insert(EFFECTS.to_owned(), Value::List(List::Compound(list)));
        let mut s = Survival::default();
        load(&mut s, &extra);
        assert_eq!(s.health, 20.0);
        assert_eq!(s.food, 20.0);
        assert_eq!(s.saturation, 5.0);
        assert_eq!(s.xp_level, 0);
        assert_eq!(s.xp_progress, 0.0);
        assert_eq!(s.fire_ticks, 0);
        assert_eq!(s.air, -20);
        assert_eq!(s.game_mode, None);
        assert_eq!(s.spawn, None);
        assert!(s.effects.is_empty());
    }
}
