//! The survival keys of a player's saved extra, with PMMP's names and types (PMMP
//! `Player::getSaveData`, `Human::saveNBT`, `Living::saveNBT`, `Entity::saveNBT`), plus
//! `AbsorptionAmount`, `SleepingPos` and `PendingDeathDrops` (the held-back death drops, as
//! Bedrock item compounds), which PMMP does not keep.
//!
//! The extra comes from the server file, so every value is checked on load: a wrong type or a
//! value out of range keeps the default, except that the health is cut to the maximum health, the
//! air to [`MIN_AIR`]..=[`MAX_AIR`], the experience level to [`MAX_LEVEL`](crate::player::MAX_LEVEL), and
//! the food timer wraps at [`FOOD_CYCLE`]; nothing here panics.

use tracing::warn;

use super::effects::Active;
use super::food::FOOD_CYCLE;
use super::{
    MAX_AIR, MAX_EXHAUSTION, MAX_FOOD, MAX_HELD_BACK, MAX_SATURATION, MIN_AIR, Point, Survival,
};
use crate::config::GameMode;
use crate::effect::Instance;
use crate::item::Stack;
use crate::math::to_f32;
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
const FIRST_PLAYED: &str = "firstPlayed";
const LAST_PLAYED: &str = "lastPlayed";

/// The most falling distance kept (a longer fall kills anyway).
const MAX_FALL: f64 = 4096.0;
/// The most effects read: one per effect type.
const MAX_EFFECTS: usize = crate::effect::TYPE_COUNT;

/// The protocol's game type of a game mode as the packets carry it (`UpdatePlayerGameType`,
/// `StartGame`).
pub(in crate::tick) fn game_mode_id(mode: GameMode) -> i32 {
    i32::from(mode.game_type())
}

/// The saved IDs of the game modes (PMMP `GameModeIdMap`), for saving and loading. Spectator is 3
/// here, unlike the protocol's game type.
const SAVED_MODES: [(GameMode, i32); 4] = [
    (GameMode::Survival, 0),
    (GameMode::Creative, 1),
    (GameMode::Adventure, 2),
    (GameMode::Spectator, 3),
];

/// The saved ID of a game mode.
#[expect(
    clippy::expect_used,
    reason = "invariant: `SAVED_MODES` lists every game mode"
)]
fn saved_mode(mode: GameMode) -> i32 {
    SAVED_MODES
        .iter()
        .find(|&&(m, _)| m == mode)
        .map(|&(_, id)| id)
        .expect("every game mode has a saved ID")
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

/// Reads the survival keys; each missing or bad key keeps its default, but for the few the module
/// documentation names, which are cut into range. With `force-gamemode` the saved game mode is
/// not loaded: the player joins in the server's mode (PMMP). Held-back items
/// read with the server's `custom` items and blocks, so a custom item is not lost.
pub(super) fn load(survival: &mut Survival, extra: &Compound, custom: &crate::custom::Registry) {
    let s = survival;
    s.game_mode = int(extra, GAME_MODE)
        .filter(|_| !s.forced_mode)
        .and_then(|id| SAVED_MODES.iter().find(|&&(_, i)| i == id))
        .map(|&(mode, _)| mode);
    if let Some(Value::List(List::Compound(list))) = extra.get(EFFECTS) {
        for e in list.iter().take(MAX_EFFECTS) {
            if let Some(instance) = Instance::from_nbt(e) {
                s.effects.insert(instance.effect(), Active::of(instance));
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
    if let Some(food) = int(extra, FOOD)
        .map(f64::from)
        .filter(|food| (0.0..=MAX_FOOD).contains(food))
    {
        s.food = food;
    }
    if let Some(saturation) = float(extra, SATURATION, 0.0, MAX_SATURATION) {
        s.saturation = saturation;
    }
    if let Some(exhaustion) = float(extra, EXHAUSTION, 0.0, MAX_EXHAUSTION) {
        s.exhaustion = exhaustion;
    }
    if let Some(timer) = int(extra, FOOD_TIMER).and_then(|timer| u32::try_from(timer).ok()) {
        s.food_timer = timer % FOOD_CYCLE;
    }
    if let Some(level) = int(extra, XP_LEVEL).and_then(|level| u32::try_from(level).ok()) {
        s.xp_level = level.min(crate::player::MAX_LEVEL);
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
    // With `drowning = false` there is no air: the player keeps the full supply.
    if s.switches.drowning
        && let Some(air) = short(extra, AIR)
    {
        s.air = air.clamp(MIN_AIR, MAX_AIR);
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
    s.held_back = match extra.get(HELD_BACK) {
        Some(Value::List(List::Compound(list))) => list
            .iter()
            .take(MAX_HELD_BACK)
            .filter_map(|nbt| {
                let stack = Stack::from_nbt_with(nbt, custom);
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

/// The saved play history (PMMP `Player::TAG_FIRST_PLAYED`, `Player::TAG_LAST_PLAYED`: longs of
/// milliseconds since the Unix epoch): the first join and the last save, each `None` when
/// missing or not a long.
pub(in crate::tick) fn load_played(extra: &Compound) -> (Option<i64>, Option<i64>) {
    let long = |key| match extra.get(key) {
        Some(Value::Long(v)) => Some(*v),
        _ => None,
    };
    (long(FIRST_PLAYED), long(LAST_PLAYED))
}

/// Writes the play history: the first join and the time of this save.
pub(in crate::tick) fn save_played((first, last): (i64, i64), extra: &mut Compound) {
    extra.insert(FIRST_PLAYED.to_owned(), Value::Long(first));
    extra.insert(LAST_PLAYED.to_owned(), Value::Long(last));
}

/// Writes the survival keys, replacing the ones read; keys of absent values are removed.
pub(super) fn save(s: &Survival, extra: &mut Compound) {
    // A returning player's compound holds the keys already: no key string is made for them.
    let mut put = |key: &str, value: Option<Value>| match value {
        Some(v) => match extra.get_mut(key) {
            Some(slot) => *slot = v,
            None => {
                extra.insert(key.to_owned(), v);
            }
        },
        None => {
            extra.remove(key);
        }
    };
    put(GAME_MODE, s.game_mode.map(|m| Value::Int(saved_mode(m))));
    put(HEALTH, Some(Value::Float(to_f32(s.health))));
    put(ABSORPTION, Some(Value::Float(to_f32(s.absorption))));
    #[expect(
        clippy::cast_possible_truncation,
        reason = "consistency: the food level, 0 to 20 and maybe fractional, is saved truncated as PMMP saves it"
    )]
    let food = s.food as i32;
    put(FOOD, Some(Value::Int(food)));
    put(SATURATION, Some(Value::Float(to_f32(s.saturation))));
    put(EXHAUSTION, Some(Value::Float(to_f32(s.exhaustion))));
    #[expect(
        clippy::expect_used,
        reason = "invariant: the food timer stays below `FOOD_CYCLE`"
    )]
    let food_timer = i32::try_from(s.food_timer).expect("the food timer fits an int");
    put(FOOD_TIMER, Some(Value::Int(food_timer)));
    #[expect(
        clippy::expect_used,
        reason = "invariant: the experience level is at most `player::MAX_LEVEL`"
    )]
    let xp_level = i32::try_from(s.xp_level).expect("the experience level fits an int");
    put(XP_LEVEL, Some(Value::Int(xp_level)));
    put(XP_PROGRESS, Some(Value::Float(to_f32(s.xp_progress))));
    put(
        XP_TOTAL,
        Some(Value::Int(i32::try_from(s.xp_lifetime).unwrap_or(i32::MAX))),
    );
    put(XP_SEED, s.xp_seed.map(Value::Int));
    let effects: Vec<Compound> = s
        .effects
        .iter()
        .map(|(&effect, a)| a.instance(effect).to_nbt())
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
    put(FALL, Some(Value::Float(to_f32(s.fall_distance))));
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
    use crate::effect::{MAX_DURATION, Type as Effect};
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
        load(&mut loaded, &extra, &crate::custom::Registry::default());
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
    fn bad_saved_values_keep_the_defaults_or_are_cut_into_range() {
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
        bad.insert("Id".to_owned(), Value::Byte(250));
        bad.insert("Duration".to_owned(), Value::Int(10));
        bad.insert("Amplifier".to_owned(), Value::Byte(0));
        list.push(bad);
        let mut negative = Compound::new();
        negative.insert("Id".to_owned(), Value::Byte(Effect::Speed.id()));
        negative.insert("Duration".to_owned(), Value::Int(-7));
        negative.insert("Amplifier".to_owned(), Value::Byte(0));
        list.push(negative);
        list.extend(std::iter::repeat_n(Compound::new(), 1000));
        extra.insert(EFFECTS.to_owned(), Value::List(List::Compound(list)));
        let mut s = Survival::default();
        load(&mut s, &extra, &crate::custom::Registry::default());
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

    /// `drowning = false`: the air a save kept is not read, so the player joins with full air
    /// that never changes.
    #[test]
    fn without_drowning_the_saved_air_is_not_read() {
        let mut extra = Compound::new();
        extra.insert(AIR.to_owned(), Value::Short(100));
        let mut s = Survival::default();
        s.switches.drowning = false;
        load(&mut s, &extra, &crate::custom::Registry::default());
        assert_eq!(s.air, MAX_AIR);
    }

    #[test]
    fn a_held_back_custom_item_survives_a_save_and_reload() {
        let custom = crate::custom::tests::registry();
        let ruby = custom.item_type(&crate::custom::tests::RUBY).unwrap();
        let s = Survival {
            held_back: vec![Stack::new(ruby, 3)],
            ..Survival::default()
        };
        let mut extra = Compound::new();
        save(&s, &mut extra);
        let mut loaded = Survival::default();
        load(&mut loaded, &extra, &custom);
        assert_eq!(loaded.held_back, s.held_back);
        let mut again = Compound::new();
        save(&loaded, &mut again);
        assert_eq!(again, extra, "saved again byte for byte");
    }
}
