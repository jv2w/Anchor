//! Trees that saplings, azaleas and fungi grow (PMMP `TreeFactory` and the `Tree` generators of
//! `world/generator/object`), and the block transaction that places them (PMMP
//! `BlockTransaction`).
//!
//! The random numbers come from the engine stream of the growing block ([`Cx::rng`]) instead of
//! a `Random` seeded from `mt_rand`, so a tree is the same in a replay. [`Context::grow_tree`]
//! grows one where the API asks, without [`StructureGrow`](crate::event::StructureGrow) (a direct
//! call raises no event).

use std::collections::BTreeMap;

use super::Cx;
use super::props::{self, HORIZONTAL};
use crate::block::{self, Class, Face, Kind, TypeTag};
use crate::random::Random;
use crate::tick::Context;
use crate::world::{self, BlockPos, MIN_Y, TreeType};

/// Blocks to set together, all or none (PMMP `BlockTransaction`).
#[derive(Debug, Default)]
pub(super) struct Tx {
    blocks: BTreeMap<BlockPos, block::State>,
}

impl Tx {
    /// Adds a block to set (replacing one added before at the same position).
    pub(super) fn add(&mut self, pos: BlockPos, s: block::State) {
        self.blocks.insert(pos, s);
    }

    /// Whether nothing was added.
    pub(super) fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// The block at `pos` once the transaction is applied (air where not loaded).
    fn fetch(&self, cx: &Cx<'_>, pos: Option<BlockPos>) -> block::State {
        pos.and_then(|p| self.blocks.get(&p).copied())
            .unwrap_or_else(|| cx.block(pos))
    }

    /// Sets the blocks the block at `origin` grows if every position is loaded (PMMP validates
    /// the world height and loads chunks; Anchor does not load chunks for a tree) and the
    /// listeners of [`StructureGrow`](crate::event::StructureGrow) let it. Whether a block
    /// changed.
    pub(super) fn apply(self, cx: &mut Cx<'_>, origin: BlockPos) -> bool {
        if !self.blocks.keys().all(|p| cx.is_loaded(*p)) {
            return false;
        }
        let Ok(event) = cx.ask_structure(origin, &self.blocks) else {
            return false;
        };
        let changed = self.set(cx);
        if changed {
            cx.tell(event);
        }
        changed
    }

    /// Sets the blocks if every position is loaded, without an event; whether a block changed.
    fn apply_quietly(self, cx: &mut Cx<'_>) -> bool {
        self.blocks.keys().all(|p| cx.is_loaded(*p)) && self.set(cx)
    }

    /// Sets the blocks; whether one changed.
    fn set(self, cx: &mut Cx<'_>) -> bool {
        let mut changed = false;
        for (pos, s) in self.blocks {
            if cx.get(pos) != Some(s) && cx.set(pos, s) {
                changed = true;
            }
        }
        changed
    }
}

/// A sapling grows its tree; whether it did.
pub(super) fn grow_sapling(cx: &mut Cx<'_>, pos: BlockPos, sapling: block::State) -> bool {
    let kind = match sapling.kind() {
        Kind::OakSapling => TreeType::Oak,
        Kind::SpruceSapling => TreeType::Spruce,
        Kind::BirchSapling => TreeType::Birch,
        Kind::JungleSapling => TreeType::Jungle,
        Kind::AcaciaSapling => TreeType::Acacia,
        _ => return false,
    };
    grow(cx, pos, kind)
}

/// Grows a tree of `kind` at `pos`; whether a block changed.
pub(super) fn grow(cx: &mut Cx<'_>, pos: BlockPos, kind: TreeType) -> bool {
    let mut rng = cx.rng();
    let tx = generate(cx, pos, kind, &mut rng);
    tx.is_some_and(|tx| tx.apply(cx, pos))
}

/// The tree a generator would place at `pos` (PMMP `TreeFactory::get` then
/// `getBlockTransaction`), or `None` where it does not fit or PMMP has no generator (dark oak).
pub(super) fn generate(cx: &Cx<'_>, pos: BlockPos, kind: TreeType, rng: &mut Random) -> Option<Tx> {
    let plain = |k: Kind| k.default_state();
    let mut tree = match kind {
        TreeType::Oak => Tree::new(Kind::OakLog, Kind::OakLeaves, 4 + below(rng, 3)),
        TreeType::Spruce => Tree::new(Kind::SpruceLog, Kind::SpruceLeaves, 10),
        TreeType::Birch => {
            let tall = rng.below(39) == 0;
            Tree::new(Kind::BirchLog, Kind::BirchLeaves, 7).with_birch(tall)
        }
        TreeType::Jungle => Tree::new(Kind::JungleLog, Kind::JungleLeaves, 8),
        TreeType::Acacia => Tree::new(Kind::AcaciaLog, Kind::AcaciaLeaves, 0),
        TreeType::Azalea => Tree::new(Kind::OakLog, Kind::AzaleaLeaves, 0),
        TreeType::DarkOak => return None,
        TreeType::Crimson | TreeType::Warped => {
            let base = below(rng, 9) + 4;
            let height = base * if rng.below(12) == 0 { 2 } else { 1 };
            let huge = rng.unit() < 0.06;
            let (stem, hat) = if kind == TreeType::Crimson {
                (Kind::CrimsonStem, Kind::NetherWartBlock)
            } else {
                (Kind::WarpedStem, Kind::WarpedWartBlock)
            };
            return Some(nether(
                cx,
                pos,
                rng,
                plain(stem),
                plain(hat),
                height,
                kind == TreeType::Crimson,
                huge,
            ));
        }
    };
    // Birch picks its height in `getBlockTransaction`, after the factory's draws.
    match kind {
        TreeType::Birch => tree.height = below(rng, 3) + 5 + if tree.tall { 5 } else { 0 },
        TreeType::Spruce => tree.height = below(rng, 4) + 6,
        TreeType::Azalea => tree.height = below(rng, 3) + 4,
        _ => {}
    }
    tree.place(cx, pos, kind, rng)
}

impl Context<'_> {
    /// Grows a tree of `kind` from `pos` in `world`, as a sapling (or an azalea, a fungus) there
    /// would (PMMP `TreeFactory::get`, then `Tree::getBlockTransaction` and its `apply`): the
    /// same generators, with the engine's random numbers for that position and tick. Nothing
    /// changes, and the result is false, where the tree does not fit, `pos` is outside the world
    /// height or a block it needs is in a chunk that is not loaded, for [`TreeType::DarkOak`] (PMMP has no generator) or a world the
    /// server does not have. Like every direct call it raises no event (a sapling's growth raises
    /// [`StructureGrow`](crate::event::StructureGrow)).
    #[doc(alias = "TreeFactory")]
    #[doc(alias = "TreeFactory::get")]
    pub fn grow_tree(&mut self, world: world::Id, pos: BlockPos, kind: TreeType) -> bool {
        let Some(w) = crate::tick::world_api::index(self.state, world) else {
            return false;
        };
        let mut cx = Cx::new(self.state, w, pos);
        // The generators add to the origin without checks: a loaded origin is inside the world
        // border and height, where a tree's extent cannot overflow.
        if !cx.is_loaded(pos) {
            return false;
        }
        let mut rng = cx.rng();
        generate(&cx, pos, kind, &mut rng).is_some_and(|tx| tx.apply_quietly(&mut cx))
    }
}

/// `nextBoundedInt(n)` as an `i32`.
fn below(rng: &mut Random, n: u32) -> i32 {
    i32::try_from(rng.below(n)).unwrap_or(0)
}

/// A tree of logs and leaves (PMMP `Tree` and its overriding generators).
struct Tree {
    trunk: block::State,
    leaf: block::State,
    height: i32,
    /// A birch from the one in 39 chance of a tall one.
    tall: bool,
}

impl Tree {
    fn new(trunk: Kind, leaf: Kind, height: i32) -> Self {
        Self {
            trunk: trunk.default_state(),
            leaf: leaf.default_state(),
            height,
            tall: false,
        }
    }

    fn with_birch(mut self, tall: bool) -> Self {
        self.tall = tall;
        self
    }

    /// PMMP `Tree::canOverride` (with azalea's additions).
    fn can_override(s: block::State, kind: TreeType) -> bool {
        s.can_be_replaced()
            || s.is(Class::Sapling)
            || s.is(Class::Leaves)
            || (kind == TreeType::Azalea
                && matches!(s.kind(), Kind::Azalea | Kind::FloweringAzalea))
    }

    /// PMMP `Tree::getBlockTransaction`.
    fn place(&self, cx: &Cx<'_>, pos: BlockPos, kind: TreeType, rng: &mut Random) -> Option<Tx> {
        // canPlaceObject
        let mut radius = 0;
        for yy in 0..self.height + 3 {
            if yy == 1 || yy == self.height {
                radius += 1;
            }
            for xx in -radius..=radius {
                for zz in -radius..=radius {
                    if !Self::can_override(cx.block(pos.offset(xx, yy, zz)), kind) {
                        return None;
                    }
                }
            }
        }
        let mut tx = Tx::default();
        match kind {
            TreeType::Acacia => self.acacia(cx, &mut tx, pos, rng),
            TreeType::Azalea => self.azalea(cx, &mut tx, pos, rng),
            _ => {
                let trunk = match kind {
                    TreeType::Spruce => self.height - below(rng, 3),
                    _ => self.height - 1,
                };
                self.trunk(cx, &mut tx, pos, trunk, kind);
                if kind == TreeType::Spruce {
                    self.spruce_canopy(cx, &mut tx, pos, rng);
                } else {
                    self.canopy(cx, &mut tx, pos, rng);
                }
            }
        }
        Some(tx)
    }

    /// PMMP `Tree::placeTrunk`: dirt below, logs up through replaceable blocks.
    fn trunk(&self, cx: &Cx<'_>, tx: &mut Tx, pos: BlockPos, height: i32, kind: TreeType) {
        if let Some(below) = pos.offset(0, -1, 0) {
            tx.add(below, block::DIRT);
        }
        for yy in 0..height {
            let at = pos.offset(0, yy, 0);
            if Self::can_override(tx.fetch(cx, at), kind)
                && let Some(at) = at
            {
                tx.add(at, self.trunk);
            }
        }
    }

    /// PMMP `Tree::placeCanopy`.
    fn canopy(&self, cx: &Cx<'_>, tx: &mut Tx, pos: BlockPos, rng: &mut Random) {
        let top = pos.y + self.height;
        for yy in (top - 3)..=top {
            let y_off = yy - top;
            let mid = 1 + (-y_off) / 2;
            for dx in -mid..=mid {
                for dz in -mid..=mid {
                    if dx.abs() == mid && dz.abs() == mid && (y_off == 0 || rng.below(2) == 0) {
                        continue;
                    }
                    let at = Some(BlockPos::new(pos.x + dx, yy, pos.z + dz));
                    if !tx.fetch(cx, at).is_solid()
                        && let Some(at) = at
                    {
                        tx.add(at, self.leaf);
                    }
                }
            }
        }
    }

    /// PMMP `SpruceTree::placeCanopy`.
    fn spruce_canopy(&self, cx: &Cx<'_>, tx: &mut Tx, pos: BlockPos, rng: &mut Random) {
        let top_size = self.height - (1 + below(rng, 2));
        let l_radius = 2 + below(rng, 2);
        let mut radius = below(rng, 2);
        let (mut max_r, mut min_r) = (1, 0);
        for yy in 0..=top_size {
            let y = pos.y + self.height - yy;
            for dx in -radius..=radius {
                for dz in -radius..=radius {
                    if dx.abs() == radius && dz.abs() == radius && radius > 0 {
                        continue;
                    }
                    let at = Some(BlockPos::new(pos.x + dx, y, pos.z + dz));
                    if !tx.fetch(cx, at).is_solid()
                        && let Some(at) = at
                    {
                        tx.add(at, self.leaf);
                    }
                }
            }
            if radius >= max_r {
                radius = min_r;
                min_r = 1;
                max_r += 1;
                if max_r > l_radius {
                    max_r = l_radius;
                }
            } else {
                radius += 1;
            }
        }
    }

    /// PMMP `AcaciaTree`: a trunk with a main and a second branch, a flat canopy on each.
    fn acacia(&self, cx: &Cx<'_>, tx: &mut Tx, pos: BlockPos, rng: &mut Random) {
        let height = 5 + rng.range(0, 2) + rng.range(0, 2);
        if let Some(below) = pos.offset(0, -1, 0) {
            tx.add(below, block::DIRT);
        }
        let first = height - 1 - rng.range(0, 3);
        for yy in 0..=first {
            if let Some(at) = pos.offset(0, yy, 0) {
                tx.add(at, self.trunk);
            }
        }
        let main_facing = pick(rng);
        let start = BlockPos::new(pos.x, pos.y + first, pos.z);
        let diagonal = rng.range(1, 3);
        let main_tip = self.branch(tx, start, main_facing, diagonal, height - first);
        let second_facing = pick(rng);
        let mut second_tip = None;
        if second_facing != main_facing {
            let length = rng.range(1, 3);
            let start = BlockPos::new(pos.x, pos.y + (first - rng.range(0, 1)), pos.z);
            second_tip = Some(self.branch(tx, start, second_facing, length, length));
        }
        self.acacia_layer(cx, tx, Some(main_tip), 3, 5);
        self.acacia_layer(cx, tx, main_tip.offset(0, 1, 0), 2, 2);
        if let Some(tip) = second_tip {
            self.acacia_layer(cx, tx, Some(tip), 2, 3);
            self.acacia_layer(cx, tx, tip.offset(0, 1, 0), 1, 2);
        }
    }

    /// PMMP `AcaciaTree::placeBranch`: up `length` blocks, sideways for the first `diagonal`.
    fn branch(
        &self,
        tx: &mut Tx,
        start: BlockPos,
        facing: Face,
        diagonal: i32,
        length: i32,
    ) -> BlockPos {
        let mut at = start;
        let mut placed = 0;
        for _ in 0..length {
            at = BlockPos::new(at.x, at.y + 1, at.z);
            if placed < diagonal {
                let (dx, _, dz) = facing.offset();
                at = BlockPos::new(at.x + dx, at.y, at.z + dz);
                placed += 1;
            }
            tx.add(at, self.trunk);
        }
        at
    }

    /// PMMP `AcaciaTree::placeCanopyLayer`.
    fn acacia_layer(
        &self,
        cx: &Cx<'_>,
        tx: &mut Tx,
        center: Option<BlockPos>,
        radius: i32,
        taxicab: i32,
    ) {
        let Some(c) = center else {
            return;
        };
        for dx in -radius..=radius {
            for dz in -radius..=radius {
                let at = BlockPos::new(c.x + dx, c.y, c.z + dz);
                if dx.abs() + dz.abs() <= taxicab && tx.fetch(cx, Some(at)).can_be_replaced() {
                    tx.add(at, self.leaf);
                }
            }
        }
    }

    /// PMMP `AzaleaTree`: a bent trunk on rooted dirt, leaves scattered around its top.
    fn azalea(&self, cx: &Cx<'_>, tx: &mut Tx, pos: BlockPos, rng: &mut Random) {
        let height = (4 + below(rng, 2)).min(5);
        if let (Some(below_pos), Some(rooted)) = (
            pos.offset(0, -1, 0),
            props::plain("minecraft:dirt_with_roots"),
        ) {
            tx.add(below_pos, rooted);
        }
        let direction = HORIZONTAL
            .get(usize::try_from(rng.range(0, 3)).unwrap_or(0))
            .copied()
            .unwrap_or(Face::North);
        let (dx, _, dz) = direction.offset();
        let side_up = (height - 2).clamp(0, 2);
        let mut lead_up = (height - side_up).min(3);
        let total = lead_up + side_up + 1;
        if total < height {
            lead_up += height - total;
        }
        let (mut x, mut y, mut z) = (pos.x, pos.y, pos.z);
        let mut attachments = Vec::new();
        for i in 0..total {
            let is_lead_up = i < lead_up;
            let is_side_up = i < lead_up + side_up - 1;
            if !is_lead_up {
                x += dx;
                z += dz;
            }
            let at = BlockPos::new(x, y, z);
            if Self::can_override(tx.fetch(cx, Some(at)), TreeType::Azalea) {
                tx.add(at, self.trunk);
            }
            if i >= 3 {
                attachments.push(at);
            }
            if is_lead_up || is_side_up {
                y += 1;
            }
        }
        let flowering = Kind::FloweringAzaleaLeaves.default_state();
        let mut visited = std::collections::BTreeSet::new();
        for a in attachments {
            for _ in 0..50 {
                let ddx = below(rng, 3) - below(rng, 3);
                let ddy = below(rng, 2) - below(rng, 2);
                let ddz = below(rng, 3) - below(rng, 3);
                let at = BlockPos::new(a.x + ddx, a.y + ddy, a.z + ddz);
                if !visited.insert(at) {
                    continue;
                }
                if tx.fetch(cx, Some(at)).is_transparent() {
                    let leaf = if rng.below(4) == 0 {
                        flowering
                    } else {
                        self.leaf
                    };
                    tx.add(at, leaf);
                }
            }
        }
    }
}

/// A random horizontal facing (PMMP `Facing::HORIZONTAL[array_rand(...)]`).
fn pick(rng: &mut Random) -> Face {
    HORIZONTAL
        .get(rng.below(4) as usize)
        .copied()
        .unwrap_or(Face::North)
}

/// PMMP `NetherTree` (huge fungi).
#[expect(
    clippy::too_many_arguments,
    reason = "consistency: PMMP's `NetherTree` constructor arguments, passed once"
)]
fn nether(
    cx: &Cx<'_>,
    pos: BlockPos,
    rng: &mut Random,
    stem: block::State,
    hat: block::State,
    height: i32,
    crimson: bool,
    huge: bool,
) -> Tx {
    let can_override =
        |s: block::State| s.can_be_replaced() || s.has_type_tag(TypeTag::HugeFungusReplaceable);
    let decor = Kind::Shroomlight.default_state();
    let mut tx = Tx::default();
    // placeTrunk
    let i = i32::from(huge);
    for j in -i..=i {
        for k in -i..=i {
            let corner = huge && j.abs() == i && k.abs() == i;
            for l in 0..height {
                let at = BlockPos::new(pos.x + j, pos.y + l, pos.z + k);
                if (!corner || rng.unit() < 0.1) && can_override(tx.fetch(cx, Some(at))) {
                    tx.add(at, stem);
                }
            }
        }
    }
    // placeCanopy
    let hat_height = (below(rng, u32::try_from(1 + height / 3).unwrap_or(1)) + 5).min(height);
    let start = height - hat_height;
    for k in start..=height {
        let mut l = if k < height - below(rng, 3) { 2 } else { 1 };
        if hat_height > 8 && k < start + 4 {
            l = 3;
        }
        if huge {
            l += 1;
        }
        for i1 in -l..=l {
            for j1 in -l..=l {
                let edge_x = i1 == -l || i1 == l;
                let edge_z = j1 == -l || j1 == l;
                let inner = !edge_x && !edge_z && k != height;
                let corner = edge_x && edge_z;
                let lower = k < start + 3;
                let at = BlockPos::new(pos.x + i1, pos.y + k, pos.z + j1);
                if !can_override(tx.fetch(cx, Some(at))) {
                    continue;
                }
                let vine = |c: f64| if crimson { c } else { 0.0 };
                if lower {
                    if !inner {
                        let below_at = Some(BlockPos::new(at.x, at.y - 1, at.z));
                        if tx.fetch(cx, below_at).kind() == hat.kind() {
                            tx.add(at, hat);
                        } else if rng.unit() < 0.15 {
                            tx.add(at, hat);
                            if crimson && rng.below(11) == 0 {
                                weeping_vines(cx, &mut tx, at, rng, &can_override);
                            }
                        }
                    }
                } else if inner {
                    hat_block(
                        cx,
                        &mut tx,
                        at,
                        rng,
                        (0.1, 0.2, vine(0.1)),
                        decor,
                        hat,
                        &can_override,
                    );
                } else if corner {
                    hat_block(
                        cx,
                        &mut tx,
                        at,
                        rng,
                        (0.01, 0.7, vine(0.083)),
                        decor,
                        hat,
                        &can_override,
                    );
                } else {
                    hat_block(
                        cx,
                        &mut tx,
                        at,
                        rng,
                        (0.0005, 0.98, vine(0.07)),
                        decor,
                        hat,
                        &can_override,
                    );
                }
            }
        }
    }
    tx
}

/// PMMP `NetherTree::placeHatBlock`: (decoration, hat, vine) chances.
#[expect(
    clippy::too_many_arguments,
    reason = "consistency: PMMP's `placeHatBlock` arguments plus the tree's blocks"
)]
fn hat_block(
    cx: &Cx<'_>,
    tx: &mut Tx,
    at: BlockPos,
    rng: &mut Random,
    (decor_chance, hat_chance, vine_chance): (f64, f64, f64),
    decor: block::State,
    hat: block::State,
    can_override: &dyn Fn(block::State) -> bool,
) {
    if rng.unit() < decor_chance {
        tx.add(at, decor);
    } else if rng.unit() < hat_chance {
        tx.add(at, hat);
        if rng.unit() < vine_chance {
            weeping_vines(cx, tx, at, rng, can_override);
        }
    }
}

/// PMMP `NetherTree::tryPlaceWeepingVines`. PMMP stops at y 0, the old world bottom; Anchor
/// stops at the world bottom.
fn weeping_vines(
    cx: &Cx<'_>,
    tx: &mut Tx,
    at: BlockPos,
    rng: &mut Random,
    can_override: &dyn Fn(block::State) -> bool,
) {
    let y = at.y - 1;
    if !can_override(tx.fetch(cx, Some(BlockPos::new(at.x, y, at.z)))) {
        return;
    }
    let mut count = below(rng, 5) + 1;
    if rng.below(7) == 0 {
        count *= 2;
    }
    let vine = Kind::WeepingVines.default_state();
    for v in 0..count {
        let vy = y - v;
        if vy < MIN_Y {
            break;
        }
        let p = BlockPos::new(at.x, vy, at.z);
        if !can_override(tx.fetch(cx, Some(p))) {
            break;
        }
        if let Some(s) =
            props::with_int(vine, "weeping_vines_age", (23 + v).min(super::MAX_VINE_AGE))
        {
            tx.add(p, s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Listeners, Priority, StructureGrow};
    use crate::tick::State;
    use crate::tick::tests::{new_state, settings};

    /// A flat world whose listeners count [`StructureGrow`] events in `count`.
    fn server(count: fn()) -> State {
        let mut l = Listeners::default();
        l.push(
            Priority::Normal,
            move |_: &mut StructureGrow, _: &mut Context<'_>| count(),
        );
        new_state(
            settings(|c| c.developer.lighting = crate::config::Lighting::Fixed),
            l,
        )
    }

    thread_local! {
        static HEARD: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    fn heard() {
        HEARD.with(|h| h.set(h.get() + 1));
    }

    /// The oak logs in the column of `pos` from its height up.
    fn logs(state: &State, pos: BlockPos) -> usize {
        (0..20)
            .filter(|dy| {
                state.worlds[0]
                    .block(BlockPos::new(pos.x, pos.y + dy, pos.z))
                    .is_some_and(|b| b.kind() == Kind::OakLog)
            })
            .count()
    }

    // pmmp: TreeFactory, TreeFactory::get
    #[test]
    fn the_api_grows_pmmps_trees_where_they_fit_without_an_event() {
        let world = world::Id::DEFAULT;
        let at = BlockPos::new(20, -60, 20);
        let mut state = server(heard);
        let mut cx = Context { state: &mut state };
        assert!(cx.grow_tree(world, at, TreeType::Oak));
        assert!(
            (4..=6).contains(&logs(&state, at)),
            "PMMP's oak: 4 to 6 logs"
        );
        assert_eq!(
            state.worlds[0].block(at.step(Face::Down).unwrap()),
            Some(block::DIRT)
        );
        let mut cx = Context { state: &mut state };
        assert!(
            !cx.grow_tree(world, at, TreeType::Oak),
            "a log is in the way"
        );
        let dark = BlockPos::new(-20, -60, 20);
        assert!(!cx.grow_tree(world, dark, TreeType::DarkOak));
        let missing = world::Id::new(7).unwrap();
        assert!(!cx.grow_tree(missing, at, TreeType::Oak));
        assert!(!cx.grow_tree(world, BlockPos::new(10_000, -60, 0), TreeType::Oak));
        assert_eq!(state.worlds[0].block(dark), Some(block::AIR));
        assert_eq!(
            logs(&state, at),
            logs(&state, at.step(Face::Up).unwrap()) + 1
        );
        assert_eq!(
            HEARD.with(std::cell::Cell::get),
            0,
            "a direct call raises no event"
        );
        for kind in [
            TreeType::Spruce,
            TreeType::Birch,
            TreeType::Jungle,
            TreeType::Acacia,
            TreeType::Azalea,
            TreeType::Crimson,
            TreeType::Warped,
        ] {
            let mut state = server(heard);
            let at = BlockPos::new(-20, -60, -20);
            assert!(
                Context { state: &mut state }.grow_tree(world, at, kind),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn the_same_tree_grows_from_the_same_state() {
        let grow = || {
            let mut state = server(heard);
            let at = BlockPos::new(-20, -60, 20);
            assert!(Context { state: &mut state }.grow_tree(
                world::Id::DEFAULT,
                at,
                TreeType::Birch
            ));
            state.hash()
        };
        assert_eq!(grow(), grow());
    }

    #[test]
    fn the_api_grows_nothing_far_outside_the_world() {
        let mut state = server(heard);
        let mut cx = Context { state: &mut state };
        for pos in [
            BlockPos::new(0, i32::MAX, 0),
            BlockPos::new(i32::MAX, -60, 0),
            BlockPos::new(0, -60, i32::MAX),
        ] {
            for kind in [
                TreeType::Oak,
                TreeType::Spruce,
                TreeType::Birch,
                TreeType::Jungle,
                TreeType::Acacia,
                TreeType::Azalea,
                TreeType::Crimson,
                TreeType::Warped,
            ] {
                assert!(
                    !cx.grow_tree(world::Id::DEFAULT, pos, kind),
                    "{kind:?} at {pos:?}"
                );
            }
        }
    }
}
