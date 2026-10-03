//! Water and lava flow (PMMP `Liquid`, `Water`, `Lava`, `MinimumCostFlowCalculator`).
//!
//! A liquid block's state is its name (still `minecraft:water` or `minecraft:flowing_water`,
//! the same for lava) and `liquid_depth`: the decay (0 a source, 1 to 7 farther away) plus 8 when
//! falling. As in PMMP, liquid that flows keeps the still or flowing name of the block it came
//! from. A liquid that needs a block of a chunk that is not loaded waits a tick rate and tries
//! again (PMMP reads air there).

use super::Cx;
use super::props::HORIZONTAL;
use crate::block::{self, Class, Face, Kind, PropertyValue};
use crate::world::{BlockPos, MAX_Y, MIN_Y};

/// The highest decay (PMMP `Liquid::MAX_DECAY`).
const MAX_DECAY: i32 = 7;
/// How far from a liquid the rules read: the flow cost search goes four blocks out, one more to
/// look down.
const REACH: i32 = 5;

/// A liquid block as PMMP models it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Liquid {
    /// The liquid's state (its kind and name).
    state: block::State,
    decay: i32,
    falling: bool,
}

impl Liquid {
    /// The liquid of a state, if it is water or lava.
    fn of(s: block::State) -> Option<Self> {
        if !matches!(s.class(), Class::Water | Class::Lava) {
            return None;
        }
        let depth = match s.value("liquid_depth")? {
            PropertyValue::Int(d) => d,
            PropertyValue::Byte(d) => i32::from(d),
            PropertyValue::String(_) => return None,
        };
        Some(Self {
            state: s,
            decay: depth & 7,
            falling: depth & 8 != 0,
        })
    }

    fn is_water(self) -> bool {
        self.state.kind() == Kind::Water
    }

    /// PMMP `isSource`.
    fn is_source(self) -> bool {
        !self.falling && self.decay == 0
    }

    /// PMMP `tickRate`: water 5, lava 30.
    fn tick_rate(self) -> u32 {
        if self.is_water() { 5 } else { 30 }
    }

    /// PMMP `getFlowDecayPerBlock`: water 1, lava 2 (PMMP's overworld value).
    fn decay_per_block(self) -> i32 {
        if self.is_water() { 1 } else { 2 }
    }

    /// The state with this decay and falling, same name as `self`.
    fn with(self, decay: i32, falling: bool) -> Option<block::State> {
        let depth = if falling { 8 } else { decay };
        self.state.with("liquid_depth", PropertyValue::Int(depth))
    }

    /// PMMP `getEffectiveFlowDecay`: -1 for anything but this liquid, 0 when falling.
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

    /// PMMP `canFlowInto`: in the world, flowable, and not a source of any liquid.
    fn can_flow_into(pos: BlockPos, s: block::State) -> bool {
        (MIN_Y..=MAX_Y).contains(&pos.y)
            && s.can_be_flowed_into()
            && !Self::of(s).is_some_and(Self::is_source)
    }
}

/// Whether every chunk the rules of a liquid at `pos` may read is loaded.
fn reach_loaded(cx: &Cx<'_>, pos: BlockPos) -> bool {
    let y = pos.y.clamp(MIN_Y, MAX_Y);
    [
        (-REACH, -REACH),
        (-REACH, REACH),
        (REACH, -REACH),
        (REACH, REACH),
    ]
    .into_iter()
    .all(|(dx, dz)| {
        cx.is_loaded(BlockPos::new(
            pos.x.saturating_add(dx),
            y,
            pos.z.saturating_add(dz),
        ))
    })
}

/// The block at `pos + (dx, dy, dz)`, air outside the world height.
fn at(cx: &Cx<'_>, pos: BlockPos, dx: i32, dy: i32, dz: i32) -> (Option<BlockPos>, block::State) {
    let p = pos.offset(dx, dy, dz);
    (p, cx.block(p))
}

/// PMMP `Liquid::onNearbyBlockChange`: lava may harden; otherwise the liquid schedules a flow.
pub(super) fn on_nearby(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let Some(liquid) = Liquid::of(block) else {
        return;
    };
    if !reach_loaded(cx, pos) {
        cx.schedule(pos, liquid.tick_rate());
        return;
    }
    if !harden(cx, pos, liquid) {
        cx.schedule(pos, liquid.tick_rate());
    }
}

/// PMMP `Liquid::onScheduledUpdate`: decay or form a source, fall, then spread to the sides
/// with the cheapest way down.
pub(super) fn on_scheduled(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let Some(mut this) = Liquid::of(block) else {
        return;
    };
    if !reach_loaded(cx, pos) {
        cx.schedule(pos, this.tick_rate());
        return;
    }
    let per_block = this.decay_per_block();
    if !this.is_source() {
        let mut smallest = -100;
        let mut sources = 0;
        for (dx, dz) in [(0, -1), (0, 1), (-1, 0), (1, 0)] {
            let (_, s) = at(cx, pos, dx, 0, dz);
            if let Some(other) = Liquid::of(s).filter(|o| o.state.kind() == this.state.kind()) {
                let mut decay = other.decay;
                if other.is_source() {
                    sources += 1;
                } else if other.falling {
                    decay = 0;
                }
                if !(smallest >= 0 && decay >= smallest) {
                    smallest = decay;
                }
            }
        }
        let mut new_decay = smallest + per_block;
        let mut falling = false;
        if new_decay > MAX_DECAY || smallest < 0 {
            new_decay = -1;
        }
        if this.effective(at(cx, pos, 0, 1, 0).1) >= 0 {
            falling = true;
        }
        if this.is_water() && sources >= 2 {
            let (_, bottom) = at(cx, pos, 0, -1, 0);
            if bottom.is_solid()
                || Liquid::of(bottom)
                    .is_some_and(|b| b.state.kind() == this.state.kind() && b.is_source())
            {
                new_decay = 0;
                falling = false;
            }
        }
        if falling != this.falling || (!falling && new_decay != this.decay) {
            if !falling && new_decay < 0 {
                cx.set(pos, block::AIR);
                return;
            }
            let decay = if falling { 0 } else { new_decay };
            let Some(s) = this.with(decay, falling) else {
                return;
            };
            this = Liquid {
                state: s,
                decay,
                falling,
            };
            cx.set(pos, s);
        }
    }
    let (bottom_pos, bottom) = at(cx, pos, 0, -1, 0);
    if let Some(b) = bottom_pos {
        flow_into(cx, this, b, bottom, 0, true);
    }
    if this.is_source() || !bottom.can_be_flowed_into() {
        let adjacent = if this.falling {
            1
        } else {
            this.decay + per_block
        };
        if adjacent <= MAX_DECAY {
            for face in optimal_directions(cx, pos, per_block) {
                let (dx, dy, dz) = face.offset();
                let (p, s) = at(cx, pos, dx, dy, dz);
                if let Some(p) = p {
                    flow_into(cx, this, p, s, adjacent, false);
                }
            }
        }
    }
    harden(cx, pos, this);
}

/// PMMP `flowIntoBlock` (with lava's override: lava flowing into water turns it to stone).
fn flow_into(
    cx: &mut Cx<'_>,
    this: Liquid,
    pos: BlockPos,
    s: block::State,
    decay: i32,
    falling: bool,
) {
    if !this.is_water() && s.class() == Class::Water {
        cx.set(pos, Kind::Stone.default_state());
        return;
    }
    if !Liquid::can_flow_into(pos, s) || Liquid::of(s).is_some() {
        return;
    }
    let Some(new) = this.with(if falling { 0 } else { decay }, falling) else {
        return;
    };
    if s.kind() != Kind::Air {
        cx.destroy(pos);
    }
    cx.set(pos, new);
}

/// Lava next to water hardens (PMMP `Lava::checkForHarden`): a source to obsidian, decay up to 4
/// to cobblestone; on soul soil next to blue ice, to basalt. Water never hardens.
fn harden(cx: &mut Cx<'_>, pos: BlockPos, this: Liquid) -> bool {
    if this.is_water() || this.falling {
        return false;
    }
    let around = [Face::Up, Face::North, Face::South, Face::West, Face::East];
    for face in around {
        let (dx, dy, dz) = face.offset();
        if at(cx, pos, dx, dy, dz).1.class() == Class::Water {
            if this.decay == 0 {
                cx.set(pos, Kind::Obsidian.default_state());
                return true;
            } else if this.decay <= 4 {
                cx.set(pos, Kind::Cobblestone.default_state());
                return true;
            }
        }
    }
    if at(cx, pos, 0, -1, 0).1.kind() == Kind::SoulSoil {
        for face in around {
            let (dx, dy, dz) = face.offset();
            if at(cx, pos, dx, dy, dz).1.kind() == Kind::BlueIce {
                cx.set(pos, Kind::Basalt.default_state());
                return true;
            }
        }
    }
    false
}

/// What the flow cost search knows of a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Blocked,
    Flow,
    FlowDown,
}

/// The positions the flow cost search has looked at: all on the liquid's level within
/// [`REACH`] blocks, so a fixed grid around it (PMMP's hash map, without allocating).
struct Visited {
    origin: BlockPos,
    cells: [Option<Status>; GRID * GRID],
}

/// The side of the [`Visited`] grid.
const GRID: usize = 2 * REACH as usize + 1;

impl Visited {
    fn new(origin: BlockPos) -> Self {
        Self {
            origin,
            cells: [None; GRID * GRID],
        }
    }

    /// The cell of `pos`, if it is in the grid.
    fn index(&self, pos: BlockPos) -> Option<usize> {
        let dx = usize::try_from(pos.x.checked_sub(self.origin.x)?.checked_add(REACH)?).ok()?;
        let dz = usize::try_from(pos.z.checked_sub(self.origin.z)?.checked_add(REACH)?).ok()?;
        (dx < GRID && dz < GRID).then_some(dx * GRID + dz)
    }

    fn get(&self, pos: BlockPos) -> Option<Status> {
        *self.cells.get(self.index(pos)?)?
    }

    fn insert(&mut self, pos: BlockPos, status: Status) {
        if let Some(i) = self.index(pos)
            && let Some(cell) = self.cells.get_mut(i)
        {
            *cell = Some(status);
        }
    }
}

/// PMMP `MinimumCostFlowCalculator::getOptimalFlowDirections`: the horizontal sides with the
/// shortest way to a drop, within `4 / per_block` blocks.
fn optimal_directions(cx: &Cx<'_>, pos: BlockPos, per_block: i32) -> Vec<Face> {
    let mut visited = Visited::new(pos);
    let mut costs = [1000; 4];
    let mut max_cost = 4 / per_block;
    for (i, face) in HORIZONTAL.into_iter().enumerate() {
        let (dx, _, dz) = face.offset();
        let side = BlockPos::new(pos.x + dx, pos.y, pos.z + dz);
        let status = status_of(cx, side);
        visited.insert(side, status);
        match status {
            Status::Blocked => {}
            Status::FlowDown => {
                max_cost = 0;
                if let Some(c) = costs.get_mut(i) {
                    *c = 0;
                }
            }
            Status::Flow => {
                if max_cost > 0 {
                    let opposite = face.opposite();
                    let cost = flow_cost(cx, &mut visited, side, 1, max_cost, opposite, opposite);
                    if let Some(c) = costs.get_mut(i) {
                        *c = cost;
                    }
                    max_cost = max_cost.min(cost);
                }
            }
        }
    }
    let min = costs.iter().copied().min().unwrap_or(1000);
    HORIZONTAL
        .into_iter()
        .zip(costs)
        .filter(|&(_, c)| c == min)
        .map(|(f, _)| f)
        .collect()
}

/// Whether liquid can go to `pos`, and down from there.
fn status_of(cx: &Cx<'_>, pos: BlockPos) -> Status {
    if !Liquid::can_flow_into(pos, cx.block(Some(pos))) {
        Status::Blocked
    } else if cx.block(pos.offset(0, -1, 0)).can_be_flowed_into() {
        Status::FlowDown
    } else {
        Status::Flow
    }
}

/// PMMP `calculateFlowCost`: the fewest blocks from `pos` to a drop, not going back.
fn flow_cost(
    cx: &Cx<'_>,
    visited: &mut Visited,
    pos: BlockPos,
    cost: i32,
    max_cost: i32,
    origin_opposite: Face,
    last_opposite: Face,
) -> i32 {
    let mut best = 1000;
    for face in HORIZONTAL {
        if face == origin_opposite || face == last_opposite {
            continue;
        }
        let (dx, _, dz) = face.offset();
        let next = BlockPos::new(pos.x + dx, pos.y, pos.z + dz);
        let status = visited.get(next).unwrap_or_else(|| {
            let s = status_of(cx, next);
            visited.insert(next, s);
            s
        });
        match status {
            Status::Blocked => continue,
            Status::FlowDown => return cost,
            Status::Flow => {}
        }
        if cost >= max_cost {
            continue;
        }
        let real = flow_cost(
            cx,
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
