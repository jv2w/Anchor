//! The kinds of trees and the shapes of the trees [`Context::grow_tree`](crate::Context::grow_tree)
//! grows: oaks, spruces, birches, jungle trees, acacias, azaleas and huge fungi.
//!
//! A shape is worked out from the world as it is and returned as the blocks to set; setting them
//! is the caller's.

use std::collections::{BTreeMap, BTreeSet};

use super::{BlockPos, MIN_Y, World};
use crate::block::{self, Class, Face, Kind, PropertyValue, TypeTag};
use crate::random::Random;

/// The most age of weeping and twisting vines.
pub(crate) const MAX_VINE_AGE: i32 = 25;

/// A kind of tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
// api-class: TreeType
pub enum TreeType {
    /// Oak.
    #[doc(alias = "TreeType::OAK")]
    Oak,
    /// Spruce.
    #[doc(alias = "TreeType::SPRUCE")]
    Spruce,
    /// Birch, now and then a tall one.
    #[doc(alias = "TreeType::BIRCH")]
    Birch,
    /// Jungle, as one sapling grows it.
    #[doc(alias = "TreeType::JUNGLE")]
    Jungle,
    /// Acacia.
    #[doc(alias = "TreeType::ACACIA")]
    Acacia,
    /// A huge crimson fungus, with weeping vines.
    #[doc(alias = "TreeType::CRIMSON")]
    Crimson,
    /// A huge warped fungus.
    #[doc(alias = "TreeType::WARPED")]
    Warped,
    /// Azalea: an oak trunk under azalea leaves.
    #[doc(alias = "TreeType::AZALEA")]
    Azalea,
}

impl TreeType {
    /// The kind's name as players read it, such as `Oak`.
    #[doc(alias = "TreeType::getDisplayName")]
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Oak => "Oak",
            Self::Spruce => "Spruce",
            Self::Birch => "Birch",
            Self::Jungle => "Jungle",
            Self::Acacia => "Acacia",
            Self::Crimson => "Crimson",
            Self::Warped => "Warped",
            Self::Azalea => "Azalea",
        }
    }
}

/// Blocks to set together, all or none.
#[derive(Debug, Default)]
struct Tx {
    /// The blocks to set, by position.
    blocks: BTreeMap<BlockPos, block::State>,
}

impl Tx {
    /// The block at `pos` once the transaction is applied (air where not loaded).
    fn fetch(&self, world: &World, pos: Option<BlockPos>) -> block::State {
        pos.and_then(|p| self.blocks.get(&p).copied())
            .unwrap_or_else(|| world.block_for_rules(pos))
    }

    /// Adds a block to set (replacing one added before at the same position).
    fn add(&mut self, pos: BlockPos, s: block::State) {
        self.blocks.insert(pos, s);
    }
}

/// A tree of logs and leaves.
struct Tree {
    /// The trunk's log.
    trunk: block::State,
    /// The canopy's leaves.
    leaf: block::State,
    /// The tree's height.
    height: i32,
    /// Whether a birch is the tall one of the one in 39 chance.
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

    /// Works out the blocks of the tree at `pos`; `None` where it does not fit.
    fn place(&self, world: &World, pos: BlockPos, kind: TreeType, rng: &mut Random) -> Option<Tx> {
        // The whole room of the tree must be free to grow through.
        let mut radius = 0;
        for yy in 0..self.height + 3 {
            if yy == 1 || yy == self.height {
                radius += 1;
            }
            for xx in -radius..=radius {
                for zz in -radius..=radius {
                    if !Self::can_override(world.block_for_rules(pos.offset(xx, yy, zz)), kind) {
                        return None;
                    }
                }
            }
        }
        let mut tx = Tx::default();
        match kind {
            TreeType::Acacia => self.acacia(world, &mut tx, pos, rng),
            TreeType::Azalea => self.azalea(world, &mut tx, pos, rng),
            _ => {
                let trunk = match kind {
                    TreeType::Spruce => self.height - below(rng, 3),
                    _ => self.height - 1,
                };
                self.trunk(world, &mut tx, pos, trunk, kind);
                if kind == TreeType::Spruce {
                    self.spruce_canopy(world, &mut tx, pos, rng);
                } else {
                    self.canopy(world, &mut tx, pos, rng);
                }
            }
        }
        Some(tx)
    }

    /// Tells whether a tree may grow through `s`, with azalea's additions.
    fn can_override(s: block::State, kind: TreeType) -> bool {
        s.can_be_replaced()
            || s.is(Class::Sapling)
            || s.is(Class::Leaves)
            || (kind == TreeType::Azalea
                && matches!(s.kind(), Kind::Azalea | Kind::FloweringAzalea))
    }

    /// Places the trunk: dirt below, logs up through replaceable blocks.
    fn trunk(&self, world: &World, tx: &mut Tx, pos: BlockPos, height: i32, kind: TreeType) {
        if let Some(below) = pos.offset(0, -1, 0) {
            tx.add(below, block::DIRT);
        }
        for yy in 0..height {
            let at = pos.offset(0, yy, 0);
            if Self::can_override(tx.fetch(world, at), kind)
                && let Some(at) = at
            {
                tx.add(at, self.trunk);
            }
        }
    }

    /// Places the canopy.
    fn canopy(&self, world: &World, tx: &mut Tx, pos: BlockPos, rng: &mut Random) {
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
                    if !tx.fetch(world, at).is_solid()
                        && let Some(at) = at
                    {
                        tx.add(at, self.leaf);
                    }
                }
            }
        }
    }

    /// Places a spruce's canopy.
    fn spruce_canopy(&self, world: &World, tx: &mut Tx, pos: BlockPos, rng: &mut Random) {
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
                    if !tx.fetch(world, at).is_solid()
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

    /// Places an acacia: a trunk with a main and a second branch, a flat canopy on each.
    fn acacia(&self, world: &World, tx: &mut Tx, pos: BlockPos, rng: &mut Random) {
        let height = 5 + below(rng, 3) + below(rng, 3);
        if let Some(below) = pos.offset(0, -1, 0) {
            tx.add(below, block::DIRT);
        }
        let first = height - 1 - below(rng, 4);
        for yy in 0..=first {
            if let Some(at) = pos.offset(0, yy, 0) {
                tx.add(at, self.trunk);
            }
        }
        let main_facing = pick(rng, &Face::HORIZONTAL);
        let start = BlockPos::new(pos.x, pos.y + first, pos.z);
        let diagonal = 1 + below(rng, 3);
        let main_tip = self.branch(tx, start, main_facing, diagonal, height - first);
        let second_facing = pick(rng, &Face::HORIZONTAL);
        let mut second_tip = None;
        if second_facing != main_facing {
            let length = 1 + below(rng, 3);
            let start = BlockPos::new(pos.x, pos.y + (first - below(rng, 2)), pos.z);
            second_tip = Some(self.branch(tx, start, second_facing, length, length));
        }
        self.acacia_layer(world, tx, Some(main_tip), 3, 5);
        self.acacia_layer(world, tx, main_tip.offset(0, 1, 0), 2, 2);
        if let Some(tip) = second_tip {
            self.acacia_layer(world, tx, Some(tip), 2, 3);
            self.acacia_layer(world, tx, tip.offset(0, 1, 0), 1, 2);
        }
    }

    /// Places an acacia branch: up `length` blocks, sideways for the first `diagonal`. Returns
    /// the branch's tip.
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

    /// Places an acacia canopy layer.
    fn acacia_layer(
        &self,
        world: &World,
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
                if dx.abs() + dz.abs() <= taxicab && tx.fetch(world, Some(at)).can_be_replaced() {
                    tx.add(at, self.leaf);
                }
            }
        }
    }

    /// Places an azalea tree: a bent trunk on rooted dirt, leaves scattered around its top.
    fn azalea(&self, world: &World, tx: &mut Tx, pos: BlockPos, rng: &mut Random) {
        let height = (4 + below(rng, 2)).min(5);
        if let (Some(below_pos), Some(rooted)) = (
            pos.offset(0, -1, 0),
            block::State::find("minecraft:dirt_with_roots", &[]),
        ) {
            tx.add(below_pos, rooted);
        }
        let direction = pick(rng, &Face::HORIZONTAL);
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
            if Self::can_override(tx.fetch(world, Some(at)), TreeType::Azalea) {
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
        let mut visited = BTreeSet::new();
        for a in attachments {
            for _ in 0..50 {
                let ddx = below(rng, 3) - below(rng, 3);
                let ddy = below(rng, 2) - below(rng, 2);
                let ddz = below(rng, 3) - below(rng, 3);
                let at = BlockPos::new(a.x + ddx, a.y + ddy, a.z + ddz);
                if !visited.insert(at) {
                    continue;
                }
                if tx.fetch(world, Some(at)).is_transparent() {
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

    fn with_tall(mut self, tall: bool) -> Self {
        self.tall = tall;
        self
    }
}

/// A huge fungus to grow.
struct Fungus {
    /// The stem's block.
    stem: block::State,
    /// The hat's block.
    hat: block::State,
    /// The stem's height.
    height: i32,
    /// Whether it is crimson (with weeping vines) and not warped.
    crimson: bool,
    /// Whether the stem is 3 × 3.
    huge: bool,
}

/// Works out the blocks of a tree of `kind` that grows at `pos`, or `None` where it does not
/// fit or the kind has no shape (dark oak).
///
/// The shapes add to the origin without checks: an origin the world holds is inside the world
/// border and height, where a tree's extent cannot overflow.
pub(crate) fn shape(
    world: &World,
    pos: BlockPos,
    kind: TreeType,
    rng: &mut Random,
) -> Option<BTreeMap<BlockPos, block::State>> {
    let plain = |k: Kind| k.default_state();
    let mut tree = match kind {
        TreeType::Oak => Tree::new(Kind::OakLog, Kind::OakLeaves, 4 + below(rng, 3)),
        TreeType::Spruce => Tree::new(Kind::SpruceLog, Kind::SpruceLeaves, 10),
        TreeType::Birch => {
            let tall = rng.below(39) == 0;
            Tree::new(Kind::BirchLog, Kind::BirchLeaves, 7).with_tall(tall)
        }
        TreeType::Jungle => Tree::new(Kind::JungleLog, Kind::JungleLeaves, 8),
        TreeType::Acacia => Tree::new(Kind::AcaciaLog, Kind::AcaciaLeaves, 0),
        TreeType::Crimson | TreeType::Warped => {
            let base = below(rng, 9) + 4;
            let height = base * if rng.below(12) == 0 { 2 } else { 1 };
            let huge = rng.unit() < 0.06;
            let (stem, hat) = if kind == TreeType::Crimson {
                (Kind::CrimsonStem, Kind::NetherWartBlock)
            } else {
                (Kind::WarpedStem, Kind::WarpedWartBlock)
            };
            let fungus = Fungus {
                stem: plain(stem),
                hat: plain(hat),
                height,
                crimson: kind == TreeType::Crimson,
                huge,
            };
            return Some(nether(world, pos, rng, &fungus).blocks);
        }
        TreeType::Azalea => Tree::new(Kind::OakLog, Kind::AzaleaLeaves, 0),
    };
    // The heights of these kinds are drawn after the draws above, in this order.
    match kind {
        TreeType::Spruce => tree.height = below(rng, 4) + 6,
        TreeType::Birch => tree.height = below(rng, 3) + 5 + if tree.tall { 5 } else { 0 },
        TreeType::Azalea => tree.height = below(rng, 3) + 4,
        _ => {}
    }
    tree.place(world, pos, kind, rng).map(|tx| tx.blocks)
}

/// Works out the blocks of a huge fungus.
fn nether(world: &World, pos: BlockPos, rng: &mut Random, fungus: &Fungus) -> Tx {
    let Fungus {
        stem,
        hat,
        height,
        crimson,
        huge,
    } = *fungus;
    let can_override =
        |s: block::State| s.can_be_replaced() || s.has_type_tag(TypeTag::HugeFungusReplaceable);
    let decor = Kind::Shroomlight.default_state();
    let mut tx = Tx::default();
    // The stem.
    let i = i32::from(huge);
    for j in -i..=i {
        for k in -i..=i {
            let corner = huge && j.abs() == i && k.abs() == i;
            for l in 0..height {
                let at = BlockPos::new(pos.x + j, pos.y + l, pos.z + k);
                if (!corner || rng.unit() < 0.1) && can_override(tx.fetch(world, Some(at))) {
                    tx.add(at, stem);
                }
            }
        }
    }
    // The hat.
    let hat_height = (below(rng, 1 + height / 3) + 5).min(height);
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
                if !can_override(tx.fetch(world, Some(at))) {
                    continue;
                }
                let vine = |c: f64| if crimson { c } else { 0.0 };
                if lower {
                    if !inner {
                        let below_at = Some(BlockPos::new(at.x, at.y - 1, at.z));
                        if tx.fetch(world, below_at).kind() == hat.kind() {
                            tx.add(at, hat);
                        } else if rng.unit() < 0.15 {
                            tx.add(at, hat);
                            if crimson && rng.below(11) == 0 {
                                weeping_vines(world, &mut tx, at, rng, &can_override);
                            }
                        }
                    }
                } else if inner {
                    hat_block(
                        world,
                        &mut tx,
                        at,
                        rng,
                        (0.1, 0.2, vine(0.1)),
                        (decor, hat),
                        &can_override,
                    );
                } else if corner {
                    hat_block(
                        world,
                        &mut tx,
                        at,
                        rng,
                        (0.01, 0.7, vine(0.083)),
                        (decor, hat),
                        &can_override,
                    );
                } else {
                    hat_block(
                        world,
                        &mut tx,
                        at,
                        rng,
                        (0.0005, 0.98, vine(0.07)),
                        (decor, hat),
                        &can_override,
                    );
                }
            }
        }
    }
    tx
}

/// Places a hat block with the (decoration, hat, vine) chances and the (decoration, hat) blocks.
fn hat_block(
    world: &World,
    tx: &mut Tx,
    at: BlockPos,
    rng: &mut Random,
    (decor_chance, hat_chance, vine_chance): (f64, f64, f64),
    (decor, hat): (block::State, block::State),
    can_override: &dyn Fn(block::State) -> bool,
) {
    if rng.unit() < decor_chance {
        tx.add(at, decor);
    } else if rng.unit() < hat_chance {
        tx.add(at, hat);
        if rng.unit() < vine_chance {
            weeping_vines(world, tx, at, rng, can_override);
        }
    }
}

/// Places weeping vines under `at`, down to the world bottom.
fn weeping_vines(
    world: &World,
    tx: &mut Tx,
    at: BlockPos,
    rng: &mut Random,
    can_override: &dyn Fn(block::State) -> bool,
) {
    let y = at.y - 1;
    if !can_override(tx.fetch(world, Some(BlockPos::new(at.x, y, at.z)))) {
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
        if !can_override(tx.fetch(world, Some(p))) {
            break;
        }
        let age = (23 + v).min(MAX_VINE_AGE);
        if let Some(s) = vine.with(nether_vines_age(vine), PropertyValue::Int(age)) {
            tx.add(p, s);
        }
    }
}

/// The name of the age property of nether vines: weeping or twisting.
pub(crate) fn nether_vines_age(block: block::State) -> &'static str {
    if block.kind() == Kind::WeepingVines {
        "weeping_vines_age"
    } else {
        "twisting_vines_age"
    }
}

/// Draws a number in `0..n`.
fn below(rng: &mut Random, n: i32) -> i32 {
    rng.range(0, n - 1)
}

/// Picks an element of `items` with `rng`.
#[expect(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "invariant: the trees pick from short lists that are not empty, and `below(n)` is \
              below n"
)]
fn pick<T: Copy>(rng: &mut Random, items: &[T]) -> T {
    let len = u32::try_from(items.len()).expect("a tree's list length fits u32");
    items[rng.below(len) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    // api: TreeType, TreeType::getDisplayName
    #[test]
    fn tree_types_have_display_names() {
        assert_eq!(TreeType::Oak.display_name(), "Oak");
        assert_eq!(TreeType::Azalea.display_name(), "Azalea");
    }

    #[test]
    fn nether_vines_name_their_age_property() {
        assert_eq!(
            nether_vines_age(Kind::WeepingVines.default_state()),
            "weeping_vines_age"
        );
        assert_eq!(
            nether_vines_age(Kind::TwistingVines.default_state()),
            "twisting_vines_age"
        );
    }
}
