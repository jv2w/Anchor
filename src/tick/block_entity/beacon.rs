//! Beacons (PMMP keeps only the chosen effects; the pyramid, the payment and the effects follow
//! dragonfly `block.Beacon`, PLAN.md 8.4): every 80 ticks the pyramid of mineral blocks below is
//! counted (0 to 4 levels) and, under an open column, the players within range get the effects.

use super::super::{State, survival};
use crate::block;
use crate::effect::{Instance, Type};
use crate::item;
use crate::save::WorldIndex;
use crate::world::{BlockPos, MAX_Y, World};

/// Ticks between beacon updates (dragonfly: 80).
pub(in crate::tick) const INTERVAL: u64 = 80;
/// The most pyramid levels.
const MAX_LEVEL: u8 = 4;

/// Whether a block counts in a beacon pyramid (vanilla's `beacon_base_blocks`).
fn powers(state: block::State) -> bool {
    matches!(
        state.name(),
        "minecraft:iron_block"
            | "minecraft:gold_block"
            | "minecraft:diamond_block"
            | "minecraft:emerald_block"
            | "minecraft:netherite_block"
    )
}

/// Whether an item pays for beacon effects (vanilla's `beacon_payment_items`).
pub(in crate::tick) fn is_payment(item: item::Type) -> bool {
    matches!(
        item.name(),
        "minecraft:iron_ingot"
            | "minecraft:gold_ingot"
            | "minecraft:diamond"
            | "minecraft:emerald"
            | "minecraft:netherite_ingot"
    )
}

/// The pyramid levels under the beacon at `pos` (dragonfly `recalculateLevel`): each level is a
/// full square of mineral blocks one wider on every side than the one above.
pub(in crate::tick) fn level(world: &World, pos: BlockPos) -> u8 {
    let mut level = 0;
    for step in 1..=i32::from(MAX_LEVEL) {
        let y = pos.y - step;
        for x in pos.x - step..=pos.x + step {
            for z in pos.z - step..=pos.z + step {
                if !world.block(BlockPos::new(x, y, z)).is_some_and(powers) {
                    return level;
                }
            }
        }
        level += 1;
    }
    level
}

/// Whether a block above the beacon stops its beam (dragonfly `obstructed`: a block that blocks
/// direct sky light).
fn obstructed(world: &World, pos: BlockPos) -> bool {
    let Some(top) = world.highest_block(pos.x, pos.z) else {
        return false;
    };
    ((pos.y + 1)..=top.min(MAX_Y)).any(|y| {
        world
            .block(BlockPos::new(pos.x, y, pos.z))
            .is_some_and(block::State::blocks_direct_sky_light)
    })
}

/// Whether an effect ID may be chosen at `level` (dragonfly `validBeaconEffect`): speed and haste
/// from 1, resistance and jump boost from 2, strength from 3, regeneration (secondary) at 4; 0
/// is none.
pub(in crate::tick) fn allowed(id: i32, level: u8) -> bool {
    let need = match id {
        0 => 0,
        1 | 3 => 1,
        8 | 11 => 2,
        5 => 3,
        10 => 4,
        _ => return false,
    };
    level >= need
}

/// The effects a beacon of `level` gives (dragonfly `broadcastBeaconEffects`): the primary from
/// level 1 if it is allowed there; at level 4 the secondary, or the primary one level stronger
/// when both are the same. They last 9 + 2 × level seconds (one less at level 4).
pub(in crate::tick) fn effects(level: u8, primary: i32, secondary: i32) -> Vec<Instance> {
    if level == 0 || primary == 0 || !allowed(primary, level) {
        return Vec::new();
    }
    let Some(primary_type) = Type::from_id(primary) else {
        return Vec::new();
    };
    let mut seconds = 9 + 2 * u32::from(level);
    if level == MAX_LEVEL {
        seconds -= 1;
    }
    let ticks = seconds * 20;
    let effect = |t: Type, amplifier: u8| Instance::new(t, ticks, amplifier, true, true);
    if level < MAX_LEVEL || secondary == 0 {
        return vec![effect(primary_type, 0)];
    }
    if secondary == primary {
        return vec![effect(primary_type, 1)];
    }
    match Type::from_id(secondary) {
        Some(second) if allowed(secondary, level) => {
            vec![effect(primary_type, 0), effect(second, 0)]
        }
        _ => vec![effect(primary_type, 0)],
    }
}

/// One beacon update at `pos`: the level is counted again, and the effects go to the players
/// within 10 + 10 × level blocks across (any height) when the beam is open. Returns the level.
pub(in crate::tick) fn update(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    primary: i32,
    secondary: i32,
) -> u8 {
    let Some(w) = state.worlds.get(world) else {
        return 0;
    };
    let level = level(w, pos);
    if level == 0 || obstructed(w, pos) {
        return level;
    }
    let given = effects(level, primary, secondary);
    if given.is_empty() {
        return level;
    }
    let range = f64::from(10 + 10 * i32::from(level));
    let (cx, cz) = (f64::from(pos.x), f64::from(pos.z));
    let targets: Vec<_> = state
        .players
        .iter()
        .filter(|(_, p)| {
            p.world == world
                && survival::is_alive(p)
                && (p.position.x - cx).abs() <= range
                && (p.position.z - cz).abs() <= range
        })
        .map(|(s, _)| *s)
        .collect();
    for session in targets {
        for e in &given {
            let _ = survival::add_effect(state, session, *e);
        }
    }
    level
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effects_follow_the_level_rules() {
        assert!(effects(0, 1, 0).is_empty());
        assert_eq!(effects(1, 1, 0).len(), 1);
        assert!(effects(1, 5, 0).is_empty(), "strength needs level 3");
        let four = effects(4, 1, 10);
        assert_eq!(four.len(), 2);
        assert_eq!(four[0].duration_ticks(), 16 * 20);
        let double = effects(4, 3, 3);
        assert_eq!(double.len(), 1);
        assert_eq!(double[0].amplifier(), 1);
        assert_eq!(
            effects(3, 3, 3)[0].amplifier(),
            0,
            "no secondary below level 4"
        );
        assert!(effects(2, 1, 0)[0].is_ambient());
        assert_eq!(effects(2, 1, 0)[0].duration_ticks(), 13 * 20);
        assert!(!allowed(10, 3) && allowed(10, 4) && !allowed(2, 4));
    }
}
