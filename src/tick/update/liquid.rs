//! Water and lava flow (PMMP `Liquid`, `Water`, `Lava`, `MinimumCostFlowCalculator`).
//!
//! A liquid block's state is its name (still `minecraft:water` or `minecraft:flowing_water`,
//! the same for lava) and `liquid_depth`: the decay (0 a source, 1 to 7 farther away) plus 8 when
//! falling. As in PMMP, liquid that flows keeps the still or flowing name of the block it came
//! from. A liquid that needs a block of a chunk that is not loaded waits a tick rate and tries
//! again (PMMP reads air there).

use super::Rule;
use crate::block::{self, Class, Face, Kind, PropertyValue};
use crate::event::BlockSpread;
use crate::world::{BlockPos, MAX_Y, MIN_Y};

/// The side of the [`Visited`] grid.
const GRID: usize = 2 * REACH as usize + 1;
/// Lava's scheduled update delay (PMMP `Lava::tickRate` outside the nether).
pub(in crate::tick) const LAVA_TICK_RATE: u32 = 30;
/// The highest decay (PMMP `Liquid::MAX_DECAY`).
const MAX_DECAY: i32 = 7;
/// The flow cost of a side without a way down (PMMP `MinimumCostFlowCalculator`).
const NO_WAY_DOWN: i32 = 1000;
/// How far from a liquid the rules read: the flow cost search goes four blocks out, one more to
/// look down.
const REACH: i32 = 5;
/// Water's scheduled update delay (PMMP `Water::tickRate`).
pub(in crate::tick) const WATER_TICK_RATE: u32 = 5;

/// A liquid block as PMMP models it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Liquid {
    /// The liquid's state (its kind and name).
    state: block::State,
    /// The decay: 0 a source, 1 to 7 farther away.
    decay: i32,
    /// Whether the liquid falls.
    falling: bool,
}

impl Liquid {
    /// Reads the liquid of a state, if it is water or lava.
    fn of(s: block::State) -> Option<Self> {
        if !matches!(s.class(), Class::Lava | Class::Water) {
            return None;
        }
        let (decay, falling) = crate::tick::entity::liquid_depth(s)?;
        Some(Self {
            state: s,
            decay,
            falling,
        })
    }

    /// Whether the liquid is water.
    fn is_water(self) -> bool {
        self.state.kind() == Kind::Water
    }

    /// Whether the liquid is a source (PMMP `isSource`).
    fn is_source(self) -> bool {
        !self.falling && self.decay == 0
    }

    /// The liquid's scheduled update delay (PMMP `tickRate`).
    fn tick_rate(self) -> u32 {
        if self.is_water() {
            WATER_TICK_RATE
        } else {
            LAVA_TICK_RATE
        }
    }

    /// The decay a flow step adds (PMMP `getFlowDecayPerBlock`): water 1, lava 2 (PMMP's
    /// overworld value).
    fn decay_per_block(self) -> i32 {
        if self.is_water() { 1 } else { 2 }
    }

    /// The decay of `other` as this liquid sees it (PMMP `getEffectiveFlowDecay`): -1 for
    /// anything but this liquid, 0 when falling.
    fn effective(self, other: block::State) -> i32 {
        match Self::of(other) {
            Some(o) if o.state.kind() == self.state.kind() => {
                if o.falling {
                    0
                } else {
                    o.decay
                }
            }
            _ => -1,
        }
    }

    /// Tells whether a liquid can flow into `s` at `pos` (PMMP `canFlowInto`): in the world,
    /// flowable, and not a source of any liquid.
    fn can_flow_into(pos: BlockPos, s: block::State) -> bool {
        (MIN_Y..=MAX_Y).contains(&pos.y)
            && s.can_be_flowed_into()
            && !Self::of(s).is_some_and(Self::is_source)
    }

    /// The state with this decay and falling, with the name of `self`.
    fn with(self, decay: i32, falling: bool) -> Option<block::State> {
        let depth = if falling { 8 } else { decay };
        self.state.with("liquid_depth", PropertyValue::Int(depth))
    }
}

/// What the flow cost search knows of a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    /// Liquid cannot go there.
    Blocked,
    /// Liquid can go there but not down from there.
    Flow,
    /// Liquid can go there and down from there.
    FlowDown,
}

/// The positions the flow cost search has looked at: all on the liquid's level within
/// [`REACH`] blocks, so a fixed grid around it (PMMP's hash map, without allocating).
struct Visited {
    /// The liquid's position, the grid's centre.
    origin: BlockPos,
    /// The status of each position looked at, by [`Self::index`].
    cells: [Option<Status>; GRID * GRID],
}

impl Visited {
    fn new(origin: BlockPos) -> Self {
        Self {
            origin,
            cells: [None; GRID * GRID],
        }
    }

    /// The status of `pos`, if it was looked at.
    fn get(&self, pos: BlockPos) -> Option<Status> {
        *self.cells.get(self.index(pos)?)?
    }

    /// Records the status of `pos`.
    fn insert(&mut self, pos: BlockPos, status: Status) {
        if let Some(i) = self.index(pos)
            && let Some(cell) = self.cells.get_mut(i)
        {
            *cell = Some(status);
        }
    }

    /// The cell of `pos`, if it is in the grid.
    fn index(&self, pos: BlockPos) -> Option<usize> {
        let dx = usize::try_from(pos.x.checked_sub(self.origin.x)?.checked_add(REACH)?).ok()?;
        let dz = usize::try_from(pos.z.checked_sub(self.origin.z)?.checked_add(REACH)?).ok()?;
        (dx < GRID && dz < GRID).then_some(dx * GRID + dz)
    }
}

/// Runs PMMP's `Liquid::onNearbyBlockChange`: lava may harden; otherwise the liquid schedules a
/// flow.
pub(super) fn on_nearby(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    let Some(liquid) = Liquid::of(block) else {
        return;
    };
    if !rule.is_area_loaded(pos, REACH) || !harden(rule, pos, liquid) {
        rule.schedule(pos, liquid.tick_rate());
    }
}

/// Runs PMMP's `Liquid::onScheduledUpdate`: a liquid that is no source settles ([`settle`]),
/// then the liquid flows down and, when it is a source or cannot go down, to the sides with the
/// cheapest way down.
pub(super) fn on_scheduled(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    let Some(mut this) = Liquid::of(block) else {
        return;
    };
    if !rule.is_area_loaded(pos, REACH) {
        rule.schedule(pos, this.tick_rate());
        return;
    }
    let per_block = this.decay_per_block();
    if !this.is_source() {
        let Some((decay, falling)) = settle(rule, pos, this, per_block) else {
            rule.set(pos, block::AIR);
            return;
        };
        let changed = falling != this.falling || (!falling && decay != this.decay);
        if changed {
            let Some(state) = this.with(decay, falling) else {
                return;
            };
            this = Liquid {
                state,
                decay,
                falling,
            };
            rule.set(pos, state);
        }
    }
    let (below, bottom) = at(rule, pos, 0, -1, 0);
    if let Some(b) = below {
        flow_into(rule, (pos, this), b, bottom, 0, true);
    }
    if this.is_source() || !bottom.can_be_flowed_into() {
        // Falling liquid spreads to the sides like a source does.
        let side_decay = if this.falling {
            1
        } else {
            this.decay + per_block
        };
        if side_decay <= MAX_DECAY {
            for face in optimal_directions(rule, pos, per_block) {
                let (dx, dy, dz) = face.offset();
                if let (Some(p), s) = at(rule, pos, dx, dy, dz) {
                    flow_into(rule, (pos, this), p, s, side_decay, false);
                }
            }
        }
    }
    harden(rule, pos, this);
}

/// Works out what a liquid that is no source becomes from the blocks around it: its decay (0
/// when falling) and whether it falls, or `None` when it dries up. Water between two water
/// sources beside it, standing on a solid block or a water source, becomes a source. Otherwise
/// it falls under the same liquid, or else takes one flow step past the lowest decay of the same
/// liquid beside it (falling liquid counts as 0) and dries up past [`MAX_DECAY`] or with none
/// beside it.
fn settle(rule: &Rule<'_>, pos: BlockPos, this: Liquid, per_block: i32) -> Option<(i32, bool)> {
    let beside = [(0, -1), (0, 1), (-1, 0), (1, 0)].map(|(dx, dz)| at(rule, pos, dx, 0, dz).1);
    let same_source = |s: block::State| {
        Liquid::of(s).is_some_and(|o| o.state.kind() == this.state.kind() && o.is_source())
    };
    if this.is_water() && beside.into_iter().filter(|&s| same_source(s)).count() >= 2 {
        let (_, bottom) = at(rule, pos, 0, -1, 0);
        if bottom.is_solid() || same_source(bottom) {
            return Some((0, false));
        }
    }
    if this.effective(at(rule, pos, 0, 1, 0).1) >= 0 {
        return Some((0, true));
    }
    let lowest = beside
        .into_iter()
        .map(|s| this.effective(s))
        .filter(|&d| d >= 0)
        .min()?;
    let decay = lowest + per_block;
    (decay <= MAX_DECAY).then_some((decay, false))
}

/// Runs PMMP's `flowIntoBlock`: the liquid at `from` spreads into `pos` ([`BlockSpread`]),
/// breaking what was there; with lava's override, lava flowing into water forms stone of the
/// water.
fn flow_into(
    rule: &mut Rule<'_>,
    (from, this): (BlockPos, Liquid),
    pos: BlockPos,
    s: block::State,
    decay: i32,
    falling: bool,
) {
    if !this.is_water() && s.class() == Class::Water {
        rule.form(pos, Kind::Stone.default_state(), from);
        return;
    }
    if !Liquid::can_flow_into(pos, s) || Liquid::of(s).is_some() {
        return;
    }
    let Some(new) = this.with(if falling { 0 } else { decay }, falling) else {
        return;
    };
    let Ok(event) = rule.ask(
        [pos, from],
        |rule| BlockSpread {
            world: rule.world_id(),
            pos,
            block: s,
            new_state: new,
            source: from,
            cancelled: false,
        },
        BlockSpread::is_cancelled,
    ) else {
        return;
    };
    if s.kind() != Kind::Air {
        rule.destroy(pos);
    }
    if rule.set(pos, new) {
        rule.tell(event);
    }
}

/// Hardens lava next to water (PMMP `Lava::checkForHarden`): a source to obsidian, decay up to
/// 4 to cobblestone; on soul soil next to blue ice, to basalt ([`BlockForm`] caused by the water
/// or the ice). Water never hardens. True when the lava meets what hardens it, formed or not
/// (PMMP `liquidCollide`).
///
/// [`BlockForm`]: crate::event::BlockForm
fn harden(rule: &mut Rule<'_>, pos: BlockPos, this: Liquid) -> bool {
    if this.is_water() || this.falling {
        return false;
    }
    let around = [Face::Up, Face::North, Face::South, Face::West, Face::East];
    for face in around {
        let (dx, dy, dz) = face.offset();
        let (cause, s) = at(rule, pos, dx, dy, dz);
        if s.class() == Class::Water
            && let Some(cause) = cause
        {
            if this.decay == 0 {
                rule.form(pos, Kind::Obsidian.default_state(), cause);
                return true;
            } else if this.decay <= 4 {
                rule.form(pos, Kind::Cobblestone.default_state(), cause);
                return true;
            }
        }
    }
    if at(rule, pos, 0, -1, 0).1.kind() == Kind::SoulSoil {
        for face in around {
            let (dx, dy, dz) = face.offset();
            if let (Some(cause), s) = at(rule, pos, dx, dy, dz)
                && s.kind() == Kind::BlueIce
            {
                rule.form(pos, Kind::Basalt.default_state(), cause);
                return true;
            }
        }
    }
    false
}

/// Reads the block at `pos + (dx, dy, dz)`, air outside the world height.
fn at(
    rule: &Rule<'_>,
    pos: BlockPos,
    dx: i32,
    dy: i32,
    dz: i32,
) -> (Option<BlockPos>, block::State) {
    let p = pos.offset(dx, dy, dz);
    (p, rule.block(p))
}

/// Finds the horizontal sides with the shortest way to a drop, within `4 / per_block` blocks
/// (PMMP `MinimumCostFlowCalculator::getOptimalFlowDirections`).
fn optimal_directions(
    rule: &Rule<'_>,
    pos: BlockPos,
    per_block: i32,
) -> impl Iterator<Item = Face> + use<> {
    let mut visited = Visited::new(pos);
    let mut costs = [NO_WAY_DOWN; 4];
    let mut max_cost = 4 / per_block;
    for (face, cost) in Face::HORIZONTAL.into_iter().zip(&mut costs) {
        let (dx, _, dz) = face.offset();
        let side = BlockPos::new(pos.x + dx, pos.y, pos.z + dz);
        let status = status_of(rule, side);
        visited.insert(side, status);
        match status {
            Status::Blocked => {}
            Status::Flow => {
                if max_cost > 0 {
                    let opposite = face.opposite();
                    *cost = flow_cost(rule, &mut visited, side, 1, max_cost, opposite, opposite);
                    max_cost = max_cost.min(*cost);
                }
            }
            Status::FlowDown => {
                max_cost = 0;
                *cost = 0;
            }
        }
    }
    let min = costs.into_iter().fold(NO_WAY_DOWN, i32::min);
    Face::HORIZONTAL
        .into_iter()
        .zip(costs)
        .filter(move |&(_, c)| c == min)
        .map(|(f, _)| f)
}

/// Works out the fewest blocks from `pos` to a drop, not going back (PMMP
/// `calculateFlowCost`).
fn flow_cost(
    rule: &Rule<'_>,
    visited: &mut Visited,
    pos: BlockPos,
    cost: i32,
    max_cost: i32,
    origin_opposite: Face,
    last_opposite: Face,
) -> i32 {
    let mut best = NO_WAY_DOWN;
    for face in Face::HORIZONTAL {
        if face == origin_opposite || face == last_opposite {
            continue;
        }
        let (dx, _, dz) = face.offset();
        let next = BlockPos::new(pos.x + dx, pos.y, pos.z + dz);
        let status = visited.get(next).unwrap_or_else(|| {
            let s = status_of(rule, next);
            visited.insert(next, s);
            s
        });
        match status {
            Status::Blocked => continue,
            Status::Flow => {}
            Status::FlowDown => return cost,
        }
        if cost >= max_cost {
            continue;
        }
        let real = flow_cost(
            rule,
            visited,
            next,
            cost + 1,
            max_cost,
            origin_opposite,
            face.opposite(),
        );
        best = best.min(real);
    }
    best
}

/// Works out whether liquid can go to `pos`, and down from there.
fn status_of(rule: &Rule<'_>, pos: BlockPos) -> Status {
    if !Liquid::can_flow_into(pos, rule.block(Some(pos))) {
        Status::Blocked
    } else if rule.block(pos.offset(0, -1, 0)).can_be_flowed_into() {
        Status::FlowDown
    } else {
        Status::Flow
    }
}
