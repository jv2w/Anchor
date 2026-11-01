//! Fire (PMMP `Fire`, `SoulFire`): ageing, burning out, burning the blocks around and spreading,
//! with the flammability and flame encouragement of the block data. Rain is not simulated (PMMP
//! does not check it either). Every fire block looks at a fixed box around it (the air it may set
//! alight, 3 × 6 × 3 blocks, and the neighbours of that air), so one update is bounded; a fire
//! whose box reaches into a chunk that is not loaded waits and tries again.

use super::Cx;
use super::props;
use crate::block::{self, Face, Kind, SupportType};
use crate::event::BlockBurn;
use crate::random::Random;
use crate::tick::entity;
use crate::world::{BlockPos, MAX_Y, MIN_Y};

/// The oldest a fire gets (PMMP `Fire::MAX_AGE`).
const MAX_AGE: i32 = 15;
/// How far sideways a fire reads: the air around it that may catch fire, and that air's
/// neighbours.
const REACH: i32 = 2;
/// The bounds of a fire's delay to its next update (PMMP `mt_rand(30, 40)`).
const MIN_DELAY_TICKS: u32 = 30;
const MAX_DELAY_TICKS: u32 = 40;
/// The burn chance bound of the blocks beside a fire, and of those above and below it (PMMP
/// `Fire::onRandomTick`).
const SIDE_BURN_BOUND: i32 = 300;
const VERTICAL_BURN_BOUND: i32 = 350;

/// Runs PMMP's `Fire::onNearbyBlockChange`: fire turns to soul fire on soul sand and soil, goes
/// out without a full block under it or a block around that burns, and otherwise updates again.
pub(super) fn on_nearby(cx: &mut Cx<'_>, pos: BlockPos) {
    if !cx.is_area_loaded(pos, REACH) {
        schedule_next(cx, pos);
        return;
    }
    let down = cx.block(pos.step(Face::Down));
    if holds_soul_fire(down) {
        cx.set(pos, Kind::SoulFire.default_state());
    } else if down.support(Face::Up) != SupportType::Full && !flammable_around(cx, pos) {
        cx.set(pos, block::AIR);
    } else {
        schedule_next(cx, pos);
    }
}

/// Runs PMMP's `SoulFire::onNearbyBlockChange`: soul fire goes out off soul sand and soil.
pub(super) fn soul_on_nearby(cx: &mut Cx<'_>, pos: BlockPos) {
    if !holds_soul_fire(cx.block(pos.step(Face::Down))) {
        cx.set(pos, block::AIR);
    }
}

/// Runs PMMP's `Fire::onRandomTick` (and `onScheduledUpdate`, which runs it). The fire stops
/// spreading once a listener of [`BlockBurn`] changes it.
pub(super) fn on_random(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let Some(mut age) = props::int(block, "age") else {
        return;
    };
    let mut rng = cx.rng();
    if !cx.is_area_loaded(pos, REACH) {
        cx.schedule_random(&mut rng, pos, MIN_DELAY_TICKS, MAX_DELAY_TICKS);
        return;
    }
    let down = cx.block(pos.step(Face::Down));
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
    cx.schedule_random(&mut rng, pos, MIN_DELAY_TICKS, MAX_DELAY_TICKS);
    if !spread {
        return;
    }
    let Some(fire) = props::with_int(block, "age", age) else {
        return;
    };
    let source = (pos, fire, age);
    let targets = Face::HORIZONTAL
        .map(|face| (face, SIDE_BURN_BOUND))
        .into_iter()
        .chain([
            (Face::Up, VERTICAL_BURN_BOUND),
            (Face::Down, VERTICAL_BURN_BOUND),
        ]);
    for (face, bound) in targets {
        burn(cx, &mut rng, source, pos.step(face), bound);
        // The rest of the run spreads from this fire: a listener that changed it ends the run.
        if cx.get(pos) != Some(fire) {
            return;
        }
    }
    spread_fire(cx, &mut rng, pos, fire, age);
}

/// Runs PMMP's `Fire::burnBlock`: the block burns with a chance of its flammability in `bound`
/// ([`BlockBurn`]), then the fire at `source` (its state `fire` and `age`) spreads into it or it
/// is gone. Burning TNT is primed instead (PMMP `TNT::onIncinerate`); at the world's entity
/// limit it stays TNT and the burn is not applied.
fn burn(
    cx: &mut Cx<'_>,
    rng: &mut Random,
    (source, fire, age): (BlockPos, block::State, i32),
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
    let Ok(event) = cx.ask(
        [pos, source],
        |cx| BlockBurn {
            world: cx.world_id(),
            pos,
            block: target,
            causing_block: source,
            cancelled: false,
        },
        BlockBurn::is_cancelled,
    ) else {
        return;
    };
    if target.kind() == Kind::Tnt {
        if !entity::ignite(cx.state, cx.world, pos, u32::from(entity::TNT_FUSE)) {
            return;
        }
    } else {
        let mut spread = false;
        if rng.range(0, age + 9) < 5
            && let Some(new) = older(rng, fire, age)
        {
            spread = cx.spread(pos, new, source);
        }
        if !spread {
            cx.set(pos, block::AIR);
        }
    }
    cx.tell(event);
}

/// Runs PMMP's `Fire::spreadFire`: air around the fire, up to four blocks above, catches fire by
/// the flame encouragement of its neighbours, the difficulty and the fire's age.
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
                    .fold(0, u8::max);
                if encouragement == 0 {
                    continue;
                }
                let chance = (i32::from(encouragement) + 40 + difficulty) / divisor;
                if chance > 0
                    && rng.range(0, bound - 1) <= chance
                    && let Some(new) = older(rng, fire, age)
                {
                    cx.spread(at, new, pos);
                }
            }
        }
    }
}

/// Schedules the next update of the fire at `pos` with a random stream of its own.
fn schedule_next(cx: &mut Cx<'_>, pos: BlockPos) {
    let mut rng = cx.rng();
    cx.schedule_random(&mut rng, pos, MIN_DELAY_TICKS, MAX_DELAY_TICKS);
}

/// Makes a fire one stage older by PMMP's chance (`min(15, age + (mt_rand(0, 4) >> 2))`).
fn older(rng: &mut Random, fire: block::State, age: i32) -> Option<block::State> {
    let next = (age + (rng.range(0, 4) >> 2)).min(MAX_AGE);
    props::with_int(fire, "age", next)
}

/// Tells whether soul fire stays on this block (PMMP `SoulFire::canBeSupportedBy`).
fn holds_soul_fire(s: block::State) -> bool {
    matches!(s.kind(), Kind::SoulSand | Kind::SoulSoil)
}

/// Tells whether a block next to `pos` burns (PMMP `hasAdjacentFlammableBlocks`).
fn flammable_around(cx: &Cx<'_>, pos: BlockPos) -> bool {
    Face::ALL
        .into_iter()
        .any(|f| cx.block(pos.step(f)).is_flammable())
}
