//! Fire (PMMP `Fire`, `SoulFire`): ageing, burning out, burning the blocks around and spreading,
//! with the flammability and flame encouragement of the block data. Rain is not simulated (PMMP
//! does not check it either). Fire spreads only into loaded chunks, and every fire block looks at
//! a fixed box of 3 × 6 × 3 blocks, so one update is bounded.

use super::Cx;
use super::props::{self, HORIZONTAL};
use crate::block::{self, Face, Kind, SupportType};
use crate::random::Random;
use crate::world::{BlockPos, MAX_Y, MIN_Y};

/// The oldest a fire gets (PMMP `Fire::MAX_AGE`).
const MAX_AGE: i32 = 15;
/// The fuse of TNT that fire primes (PMMP `TNT::ignite` default, 4 seconds).
const TNT_FUSE: u32 = 80;

/// Whether soul fire stays on this block (PMMP `SoulFire::canBeSupportedBy`).
fn holds_soul_fire(s: block::State) -> bool {
    matches!(s.kind(), Kind::SoulSand | Kind::SoulSoil)
}

/// Whether a block next to `pos` burns (PMMP `hasAdjacentFlammableBlocks`).
fn flammable_around(cx: &Cx<'_>, pos: BlockPos) -> bool {
    Face::ALL
        .into_iter()
        .any(|f| cx.block(pos.step(f)).is_flammable())
}

/// PMMP `Fire::onNearbyBlockChange`.
pub(super) fn on_nearby(cx: &mut Cx<'_>, pos: BlockPos, _block: block::State) {
    let Some(down) = pos.step(Face::Down).and_then(|p| cx.get(p)) else {
        return;
    };
    if holds_soul_fire(down) {
        cx.set(pos, Kind::SoulFire.default_state());
    } else if down.support(Face::Up) != SupportType::Full && !flammable_around(cx, pos) {
        cx.set(pos, block::AIR);
    } else {
        cx.schedule_random(pos, 30, 40);
    }
}

/// PMMP `SoulFire::onNearbyBlockChange`: soul fire goes out off soul sand and soil.
pub(super) fn soul_on_nearby(cx: &mut Cx<'_>, pos: BlockPos) {
    if pos
        .step(Face::Down)
        .and_then(|p| cx.get(p))
        .is_some_and(|d| !holds_soul_fire(d))
    {
        cx.set(pos, block::AIR);
    }
}

/// PMMP `Fire::onRandomTick` (and `onScheduledUpdate`, which runs it).
pub(super) fn on_random(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let Some(mut age) = props::int(block, "age") else {
        return;
    };
    let Some(down) = pos.step(Face::Down).and_then(|p| cx.get(p)) else {
        return;
    };
    let mut rng = cx.rng();
    let mut result = None;
    if age < MAX_AGE && rng.range(0, 2) == 0 {
        age += 1;
        result = props::with_int(block, "age", age);
    }
    let mut spread = true;
    if !down.burns_forever() {
        if age == MAX_AGE {
            if !down.is_flammable() && rng.range(0, 3) == 3 {
                spread = false;
                result = Some(block::AIR);
            }
        } else if !flammable_around(cx, pos) {
            spread = false;
            if down.is_transparent() || age > 3 {
                result = Some(block::AIR);
            }
        }
    }
    if let Some(r) = result {
        cx.set(pos, r);
    }
    let delay = rng.range(30, 40);
    cx.schedule(pos, u32::try_from(delay).unwrap_or(30));
    if spread {
        let Some(fire) = props::with_int(block, "age", age) else {
            return;
        };
        for face in HORIZONTAL {
            burn(cx, &mut rng, fire, age, pos.step(face), 300);
        }
        burn(cx, &mut rng, fire, age, pos.step(Face::Up), 350);
        burn(cx, &mut rng, fire, age, pos.step(Face::Down), 350);
        spread_fire(cx, &mut rng, pos, fire, age);
    }
}

/// A fire one stage older by PMMP's chance (`min(15, age + (mt_rand(0, 4) >> 2))`).
fn older(rng: &mut Random, fire: block::State, age: i32) -> Option<block::State> {
    let next = (age + (rng.range(0, 4) >> 2)).min(MAX_AGE);
    props::with_int(fire, "age", next)
}

/// PMMP `Fire::burnBlock`: the block burns with a chance of its flammability in `bound`, then
/// catches fire or is gone. Burning TNT is primed instead (PMMP `TNT::onIncinerate`); at the
/// world's entity limit it stays TNT.
fn burn(
    cx: &mut Cx<'_>,
    rng: &mut Random,
    fire: block::State,
    age: i32,
    pos: Option<BlockPos>,
    bound: i32,
) {
    let Some(pos) = pos else {
        return;
    };
    let Some(target) = cx.get(pos) else {
        return;
    };
    if rng.range(0, bound) >= i32::from(target.flammability()) {
        return;
    }
    if target.kind() == Kind::Tnt {
        crate::tick::entity::ignite(cx.state, cx.world, pos, TNT_FUSE);
        return;
    }
    let mut spread = false;
    if rng.range(0, age + 9) < 5
        && let Some(new) = older(rng, fire, age)
    {
        spread = cx.set(pos, new);
    }
    if !spread {
        cx.set(pos, block::AIR);
    }
}

/// PMMP `Fire::spreadFire`: air around the fire, up to four blocks above, catches fire by the
/// flame encouragement of its neighbours, the difficulty and the fire's age.
fn spread_fire(cx: &mut Cx<'_>, rng: &mut Random, pos: BlockPos, fire: block::State, age: i32) {
    let difficulty = i32::from(cx.difficulty()) * 7;
    let divisor = age + 30;
    for y in -1..=4 {
        let target_y = pos.y + y;
        if !(MIN_Y..=MAX_Y).contains(&target_y) {
            continue;
        }
        let bound = 100 + if y > 1 { (y - 1) * 100 } else { 0 };
        for z in -1..=1 {
            for x in -1..=1 {
                if x == 0 && y == 0 && z == 0 {
                    continue;
                }
                let Some(at) = pos.offset(x, y, z) else {
                    continue;
                };
                if cx.get(at).is_none_or(|b| b.kind() != Kind::Air) {
                    continue;
                }
                let encouragement = Face::ALL
                    .into_iter()
                    .map(|f| cx.block(at.step(f)).flame_encouragement())
                    .max()
                    .unwrap_or(0);
                if encouragement == 0 {
                    continue;
                }
                let chance = (i32::from(encouragement) + 40 + difficulty) / divisor;
                if chance > 0
                    && rng.range(0, bound - 1) <= chance
                    && let Some(new) = older(rng, fire, age)
                {
                    cx.set(at, new);
                }
            }
        }
    }
}
