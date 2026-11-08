//! The kinds of trees and the shapes of the trees [`Context::grow_tree`](crate::Context::grow_tree)
//! grows: oaks, spruces, birches, jungle trees, acacias, azaleas and huge fungi.
//!
//! A shape is worked out from the world as it is and returned as the blocks to set; setting them
//! is the caller's.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;

use super::{BlockPos, MIN_Y, World};
use crate::block::{self, Class, Face, Kind, PropertyValue, TypeTag};
use crate::random::Random;

/// The diagonal reach of an acacia's main branch: how many of its steps also move sideways.
const ACACIA_DIAGONAL_RANGE: RangeInclusive<i32> = 1..=3;
/// How far below an acacia's height its straight trunk ends; also the main branch's length.
const ACACIA_FORK_DEPTH_RANGE: RangeInclusive<i32> = 1..=4;
/// One of the two additions to [`ACACIA_MIN_HEIGHT`].
const ACACIA_HEIGHT_STEP_RANGE: RangeInclusive<i32> = 0..=2;
/// The radius and taxicab reach of the leaf layer at the end of an acacia's main branch.
const ACACIA_LEAVES: (i32, i32) = (3, 5);
/// The radius and taxicab reach of the leaf layer one above the end of an acacia's main branch.
const ACACIA_LEAVES_TOP: (i32, i32) = (2, 2);
/// The lowest height of an acacia; two draws from [`ACACIA_HEIGHT_STEP_RANGE`] add to it.
const ACACIA_MIN_HEIGHT: i32 = 5;
/// The height an acacia's room check assumes; the acacia draws its real height after the check.
const ACACIA_ROOM_HEIGHT: i32 = 0;
/// How far below the top of an acacia's straight trunk its second branch starts.
const ACACIA_TWIG_DROP_RANGE: RangeInclusive<i32> = 0..=1;
/// The radius and taxicab reach of the leaf layer at the end of an acacia's second branch.
const ACACIA_TWIG_LEAVES: (i32, i32) = (2, 3);
/// The radius and taxicab reach of the leaf layer one above the end of an acacia's second branch.
const ACACIA_TWIG_LEAVES_TOP: (i32, i32) = (1, 2);
/// The length of an acacia's second branch, every step diagonal.
const ACACIA_TWIG_LENGTH_RANGE: RangeInclusive<i32> = 1..=3;
/// The span of the two draws whose difference moves an azalea leaf up or down from its log.
const AZALEA_LEAF_RISE: i32 = 2;
/// The span of the two draws whose difference moves an azalea leaf sideways from its log.
const AZALEA_LEAF_SPREAD: i32 = 3;
/// How many times leaves try to grow around each leafy log of an azalea.
const AZALEA_LEAF_TRIES: u32 = 50;
/// The height an azalea's room check assumes; the azalea's own shape decides its real height.
const AZALEA_ROOM_HEIGHT_RANGE: RangeInclusive<i32> = 4..=6;
/// An azalea grows short with odds one in this.
const AZALEA_SHORT_ODDS: i32 = 2;
/// The logs of a short azalea, bottom first, as (steps along its facing, height, whether leaves
/// grow around the log).
const AZALEA_SHORT_TRUNK: [(i32, i32, bool); 5] = [
    (0, 0, false),
    (0, 1, false),
    (1, 2, false),
    (2, 3, true),
    (3, 3, true),
];
/// The logs of a tall azalea, in the form of [`AZALEA_SHORT_TRUNK`].
const AZALEA_TALL_TRUNK: [(i32, i32, bool); 6] = [
    (0, 0, false),
    (0, 1, false),
    (0, 2, false),
    (1, 3, true),
    (2, 4, true),
    (3, 4, true),
];
/// The height of a birch.
const BIRCH_HEIGHT_RANGE: RangeInclusive<i32> = 5..=7;
/// An azalea leaf flowers with odds one in this.
const FLOWERING_AZALEA_ODDS: i32 = 4;
/// The (decoration, hat, vine) chances of a hat corner above the low layers, for a crimson fungus.
const FUNGUS_CORNER_HAT: (f64, f64, f64) = (0.01, 0.7, 0.083);
/// The chance that each cell of a huge fungus's corner stem columns grows.
const FUNGUS_CORNER_STEM_CHANCE: f64 = 0.1;
/// A fungus doubles its height with odds one in this.
const FUNGUS_DOUBLE_HEIGHT_ODDS: i32 = 12;
/// The fewest layers of a fungus's hat, unless the fungus is shorter.
const FUNGUS_HAT_MIN_HEIGHT: i32 = 5;
/// The radius of a hat layer below the taper.
const FUNGUS_HAT_RADIUS: i32 = 2;
/// How many of a hat's top layers, at most, narrow to [`FUNGUS_HAT_TOP_RADIUS`]; drawn per layer.
const FUNGUS_HAT_TAPER_RANGE: RangeInclusive<i32> = 0..=2;
/// The radius of a hat layer inside the taper.
const FUNGUS_HAT_TOP_RADIUS: i32 = 1;
/// A fungus's height before doubling.
const FUNGUS_HEIGHT_RANGE: RangeInclusive<i32> = 4..=12;
/// The chance that a fungus is huge, with a 3×3 stem.
const FUNGUS_HUGE_CHANCE: f64 = 0.06;
/// The (decoration, hat, vine) chances of a hat cell inside the rim above the low layers, for a
/// crimson fungus.
const FUNGUS_INNER_HAT: (f64, f64, f64) = (0.1, 0.2, 0.1);
/// The chance that a rim cell of a low hat layer grows a hat block when the block under it is no
/// hat block.
const FUNGUS_LOW_HAT_CHANCE: f64 = 0.15;
/// How many of a hat's bottom layers are low layers: a rim with an open middle.
const FUNGUS_LOW_HAT_LAYERS: i32 = 3;
/// A hat block a crimson fungus's low layer adds by chance grows vines with odds one in this.
const FUNGUS_LOW_VINE_ODDS: i32 = 11;
/// The (decoration, hat, vine) chances of the other hat cells above the low layers (rim cells
/// that are no corner, and the top layer's middle), for a crimson fungus.
const FUNGUS_RIM_HAT: (f64, f64, f64) = (0.0005, 0.98, 0.07);
/// A hat with more layers than this widens its bottom layers to [`FUNGUS_WIDE_HAT_RADIUS`].
const FUNGUS_TALL_HAT_HEIGHT: i32 = 8;
/// How many bottom layers of a tall hat widen.
const FUNGUS_WIDE_HAT_LAYERS: i32 = 4;
/// The radius of a tall hat's widened bottom layers.
const FUNGUS_WIDE_HAT_RADIUS: i32 = 3;
/// The height of a jungle tree.
const JUNGLE_HEIGHT: i32 = 8;
/// The most age of weeping and twisting vines.
const MAX_VINE_AGE: i32 = 25;
/// The height of an oak.
const OAK_HEIGHT_RANGE: RangeInclusive<i32> = 4..=6;
/// How many layers above a tree's height the room check covers.
const ROOM_ABOVE_TOP: i32 = 2;
/// The name of the rooted dirt under an azalea.
const ROOTED_DIRT: &str = "minecraft:dirt_with_roots";
/// A round crown drops a corner leaf of its lower layers with odds one in this.
const ROUND_CORNER_ODDS: i32 = 2;
/// How many layers below the tree's height a round crown starts.
const ROUND_CROWN_DEPTH: i32 = 3;
/// How many top layers of a round crown have [`ROUND_CROWN_NARROW_RADIUS`].
const ROUND_CROWN_NARROW_LAYERS: i32 = 2;
/// The radius of a round crown's top layers.
const ROUND_CROWN_NARROW_RADIUS: i32 = 1;
/// The radius of a round crown's lower layers.
const ROUND_CROWN_WIDE_RADIUS: i32 = 2;
/// How many logs short of its height a spruce's trunk stops.
const SPRUCE_BARE_RANGE: RangeInclusive<i32> = 0..=2;
/// The radius of a spruce's top leaf layer.
const SPRUCE_FIRST_RADIUS_RANGE: RangeInclusive<i32> = 0..=1;
/// The height of a spruce.
const SPRUCE_HEIGHT_RANGE: RangeInclusive<i32> = 6..=9;
/// How many layers fewer than its height a spruce's leaves have.
const SPRUCE_SKIPPED_LAYERS_RANGE: RangeInclusive<i32> = 0..=1;
/// The largest radius of a spruce's leaf layers.
const SPRUCE_WIDEST_RANGE: RangeInclusive<i32> = 2..=3;
/// The height of a tall birch.
const TALL_BIRCH_HEIGHT_RANGE: RangeInclusive<i32> = 10..=12;
/// A birch grows tall with odds one in this.
const TALL_BIRCH_ODDS: i32 = 39;
/// A vine strand doubles its length with odds one in this.
const VINE_DOUBLE_ODDS: i32 = 7;
/// The length of a vine strand before doubling.
const VINE_LENGTH_RANGE: RangeInclusive<i32> = 1..=5;
/// The age of a strand's top vine; each vine below is one older, up to [`MAX_VINE_AGE`].
const VINE_TOP_AGE: i32 = 23;

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

/// A tree about to grow: its blocks, the height its room check uses, and its crown's form.
struct Tree {
    /// The log of the trunk and the branches.
    trunk: block::State,
    /// The leaves.
    leaves: block::State,
    /// The height the room check uses; for a round or spruce crown also the height of the top
    /// leaf layer above the origin.
    height: i32,
    /// The form of the crown.
    crown: Crown,
}

impl Tree {
    /// A tree of `trunk` logs and `leaves` with the default states.
    fn new(trunk: Kind, leaves: Kind, height: i32, crown: Crown) -> Self {
        Self {
            trunk: trunk.default_state(),
            leaves: leaves.default_state(),
            height,
            crown,
        }
    }

    /// Whether the world leaves room for the tree at `pos`: every block of a column that widens
    /// with height is one the tree can grow through.
    fn has_room(&self, world: &World, pos: BlockPos, kind: TreeType) -> bool {
        (0..=self.height + ROOM_ABOVE_TOP).all(|dy| {
            // One wider from the first layer above the origin, one more from the tree's height.
            let reach = i32::from(dy >= 1) + i32::from(dy >= self.height);
            (-reach..=reach).all(|dx| {
                (-reach..=reach).all(|dz| {
                    Self::can_override(world.block_for_rules(pos.offset(dx, dy, dz)), kind)
                })
            })
        })
    }

    /// Whether a `kind` tree grows through `s`.
    fn can_override(s: block::State, kind: TreeType) -> bool {
        s.can_be_replaced()
            || s.is(Class::Sapling)
            || s.is(Class::Leaves)
            || (matches!(kind, TreeType::Azalea)
                && matches!(s.kind(), Kind::Azalea | Kind::FloweringAzalea))
    }

    /// Draws the tree's blocks at `pos`, or `None` when the world leaves no room for it.
    fn place(&self, world: &World, pos: BlockPos, kind: TreeType, rng: &mut Random) -> Option<Tx> {
        if !self.has_room(world, pos, kind) {
            return None;
        }
        let mut tx = Tx::default();
        let soil = match self.crown {
            Crown::Round | Crown::Spruce | Crown::Acacia => Some(block::DIRT),
            Crown::Azalea => block::State::find(ROOTED_DIRT, &[]),
        };
        if let Some(soil) = soil {
            tx.insert(pos.offset(0, -1, 0), soil);
        }
        match self.crown {
            Crown::Round => {
                // The top log stands under the second highest leaf layer.
                self.trunk(world, &mut tx, pos, self.height - 1, kind);
                self.canopy(world, &mut tx, pos, rng);
            }
            Crown::Spruce => {
                let bare = draw(rng, SPRUCE_BARE_RANGE);
                self.trunk(world, &mut tx, pos, self.height - bare, kind);
                self.spruce_canopy(world, &mut tx, pos, rng);
            }
            Crown::Acacia => self.acacia(world, &mut tx, pos, rng),
            Crown::Azalea => self.azalea(world, &mut tx, pos, rng),
        }
        Some(tx)
    }

    /// Adds a column of `height` logs from `pos` up, each where the tree grows through the block.
    fn trunk(&self, world: &World, tx: &mut Tx, pos: BlockPos, height: i32, kind: TreeType) {
        for dy in 0..height {
            let at = pos.offset(0, dy, 0);
            if Self::can_override(tx.get(world, at), kind) {
                tx.insert(at, self.trunk);
            }
        }
    }

    /// Adds a round crown: two wide layers and two narrow ones, its lower corners each dropped by
    /// chance and its top corners always.
    fn canopy(&self, world: &World, tx: &mut Tx, pos: BlockPos, rng: &mut Random) {
        let top = self.height;
        for dy in top - ROUND_CROWN_DEPTH..=top {
            let radius = if dy > top - ROUND_CROWN_NARROW_LAYERS {
                ROUND_CROWN_NARROW_RADIUS
            } else {
                ROUND_CROWN_WIDE_RADIUS
            };
            for dx in -radius..=radius {
                for dz in -radius..=radius {
                    // The top layer drops its corners without a draw; a draw is made even for a
                    // corner on a solid block.
                    let corner = dx.abs() == radius && dz.abs() == radius;
                    if corner && (dy == top || below(rng, ROUND_CORNER_ODDS) == 0) {
                        continue;
                    }
                    let at = pos.offset(dx, dy, dz);
                    if !tx.get(world, at).is_solid() {
                        tx.insert(at, self.leaves);
                    }
                }
            }
        }
    }

    /// Adds a spruce's leaves: square layers without corners from the top down, whose radius
    /// climbs and falls back with a ceiling that rises toward the drawn widest radius.
    fn spruce_canopy(&self, world: &World, tx: &mut Tx, pos: BlockPos, rng: &mut Random) {
        let skipped = draw(rng, SPRUCE_SKIPPED_LAYERS_RANGE);
        let widest = draw(rng, SPRUCE_WIDEST_RANGE);
        let mut radius = draw(rng, SPRUCE_FIRST_RADIUS_RANGE);
        // A climb ends at `ceiling`; the next one starts at `floor`, which is 0 only the first time.
        let mut floor = 0;
        let mut ceiling = 1;
        for layer in 0..self.height - skipped {
            let dy = self.height - layer;
            for dx in -radius..=radius {
                for dz in -radius..=radius {
                    if radius > 0 && dx.abs() == radius && dz.abs() == radius {
                        continue;
                    }
                    let at = pos.offset(dx, dy, dz);
                    if !tx.get(world, at).is_solid() {
                        tx.insert(at, self.leaves);
                    }
                }
            }
            if radius >= ceiling {
                radius = floor;
                floor = 1;
                ceiling = (ceiling + 1).min(widest);
            } else {
                radius += 1;
            }
        }
    }

    /// Adds an acacia: a straight trunk, a main branch that bends to one side, maybe a second
    /// branch to another side, and a flat leaf crown on each branch end.
    fn acacia(&self, world: &World, tx: &mut Tx, pos: BlockPos, rng: &mut Random) {
        let height = ACACIA_MIN_HEIGHT
            + draw(rng, ACACIA_HEIGHT_STEP_RANGE)
            + draw(rng, ACACIA_HEIGHT_STEP_RANGE);
        let depth = draw(rng, ACACIA_FORK_DEPTH_RANGE);
        let fork = height - depth;
        for dy in 0..=fork {
            tx.insert(pos.offset(0, dy, 0), self.trunk);
        }
        // `branch` takes and returns a position, so its starts saturate; from an origin inside
        // the world no branch comes near the `i32` bounds.
        let fork_top = BlockPos::new(pos.x, pos.y.saturating_add(fork), pos.z);
        let facing = pick(rng, &Face::HORIZONTAL);
        let diagonal = draw(rng, ACACIA_DIAGONAL_RANGE);
        let end = self.branch(tx, fork_top, facing, diagonal, depth);
        let twig_facing = pick(rng, &Face::HORIZONTAL);
        let twig_end = if twig_facing == facing {
            None
        } else {
            let length = draw(rng, ACACIA_TWIG_LENGTH_RANGE);
            let drop = draw(rng, ACACIA_TWIG_DROP_RANGE);
            let start = BlockPos::new(pos.x, pos.y.saturating_add(fork - drop), pos.z);
            Some(self.branch(tx, start, twig_facing, length, length))
        };
        // Leaves come after every log, so they never cover a branch.
        let layers = [
            (Some(end), ACACIA_LEAVES),
            (end.offset(0, 1, 0), ACACIA_LEAVES_TOP),
            (twig_end, ACACIA_TWIG_LEAVES),
            (
                twig_end.and_then(|p| p.offset(0, 1, 0)),
                ACACIA_TWIG_LEAVES_TOP,
            ),
        ];
        for (center, (radius, taxicab)) in layers {
            self.acacia_layer(world, tx, center, radius, taxicab);
        }
    }

    /// Adds `length` logs rising one per step from `start` toward `facing`, the first `diagonal`
    /// steps also moving one sideways, and returns the last log's position.
    fn branch(
        &self,
        tx: &mut Tx,
        start: BlockPos,
        facing: Face,
        diagonal: i32,
        length: i32,
    ) -> BlockPos {
        let (step_x, _, step_z) = facing.offset();
        let mut end = start;
        for step in 1..=length {
            let across = step.min(diagonal);
            end = BlockPos::new(
                start.x.saturating_add(step_x * across),
                start.y.saturating_add(step),
                start.z.saturating_add(step_z * across),
            );
            tx.insert(Some(end), self.trunk);
        }
        end
    }

    /// Adds leaves on the replaceable blocks of the layer around `center` within `radius` on
    /// each axis and `taxicab` in sum; no layer without a center.
    fn acacia_layer(
        &self,
        world: &World,
        tx: &mut Tx,
        center: Option<BlockPos>,
        radius: i32,
        taxicab: i32,
    ) {
        let Some(center) = center else {
            return;
        };
        for dx in -radius..=radius {
            for dz in -radius..=radius {
                if dx.abs() + dz.abs() > taxicab {
                    continue;
                }
                let at = center.offset(dx, 0, dz);
                if tx.get(world, at).can_be_replaced() {
                    tx.insert(at, self.leaves);
                }
            }
        }
    }

    /// Adds an azalea: a trunk that bends toward a drawn side and leaves scattered around its
    /// top logs, some of them flowering.
    fn azalea(&self, world: &World, tx: &mut Tx, pos: BlockPos, rng: &mut Random) {
        let trunk: &[(i32, i32, bool)] = if below(rng, AZALEA_SHORT_ODDS) == 0 {
            &AZALEA_SHORT_TRUNK
        } else {
            &AZALEA_TALL_TRUNK
        };
        let (step_x, _, step_z) = pick(rng, &Face::HORIZONTAL).offset();
        let log_at = |along: i32, up: i32| pos.offset(step_x * along, up, step_z * along);
        for &(along, up, _) in trunk {
            let at = log_at(along, up);
            if Self::can_override(tx.get(world, at), TreeType::Azalea) {
                tx.insert(at, self.trunk);
            }
        }
        let flowering = Kind::FloweringAzaleaLeaves.default_state();
        // A cell tried once is never tried again, from any log: the later try draws nothing.
        let mut tried = BTreeSet::new();
        // The leafy logs count whether or not a log went in.
        for &(along, up, _) in trunk.iter().filter(|log| log.2) {
            let Some(log) = log_at(along, up) else {
                continue;
            };
            for _ in 0..AZALEA_LEAF_TRIES {
                let dx = spread(rng, AZALEA_LEAF_SPREAD);
                let dy = spread(rng, AZALEA_LEAF_RISE);
                let dz = spread(rng, AZALEA_LEAF_SPREAD);
                let Some(at) = log.offset(dx, dy, dz) else {
                    continue;
                };
                if !tried.insert(at) || !tx.get(world, Some(at)).is_transparent() {
                    continue;
                }
                let leaves = if below(rng, FLOWERING_AZALEA_ODDS) == 0 {
                    flowering
                } else {
                    self.leaves
                };
                tx.insert(Some(at), leaves);
            }
        }
    }
}

/// The form of a tree's crown, which decides the rest of its shape.
enum Crown {
    /// Four square layers around the trunk's top (oak, birch, jungle).
    Round,
    /// Layers that widen and narrow by turns down the trunk.
    Spruce,
    /// Flat crowns on bent branches.
    Acacia,
    /// Leaves scattered around a bent trunk.
    Azalea,
}

/// A huge fungus about to grow.
struct Fungus {
    /// The stem block.
    stem: block::State,
    /// The hat block.
    hat: block::State,
    /// The stem's height; the hat's top layer is one above it.
    height: i32,
    /// Whether it is crimson, whose hat grows weeping vines.
    crimson: bool,
    /// Whether the stem is 3×3 and the hat one wider.
    huge: bool,
}

/// The blocks a shape places, by position, over the world's blocks.
///
/// A later block at a position replaces the earlier one, and the shape's later checks see it.
#[derive(Debug, Default)]
struct Tx {
    /// The placed blocks.
    blocks: BTreeMap<BlockPos, block::State>,
}

impl Tx {
    /// The block at `pos` as the shape sees it: the placed one, else the world's (air where no
    /// position or no loaded chunk is).
    fn get(&self, world: &World, pos: Option<BlockPos>) -> block::State {
        match pos.and_then(|p| self.blocks.get(&p)) {
            Some(&s) => s,
            None => world.block_for_rules(pos),
        }
    }

    /// Places `s` at `pos`; no position places nothing.
    fn insert(&mut self, pos: Option<BlockPos>, s: block::State) {
        if let Some(pos) = pos {
            self.blocks.insert(pos, s);
        }
    }
}

/// Draws the shape of a `kind` tree growing at `pos`: every block it places, or `None` when the
/// world leaves no room for it. A fungus always has room.
///
/// The caller decides which blocks are loaded and actually change.
pub(crate) fn shape(
    world: &World,
    pos: BlockPos,
    kind: TreeType,
    rng: &mut Random,
) -> Option<BTreeMap<BlockPos, block::State>> {
    let tree = match kind {
        TreeType::Oak => Tree::new(
            Kind::OakLog,
            Kind::OakLeaves,
            draw(rng, OAK_HEIGHT_RANGE),
            Crown::Round,
        ),
        TreeType::Spruce => Tree::new(
            Kind::SpruceLog,
            Kind::SpruceLeaves,
            draw(rng, SPRUCE_HEIGHT_RANGE),
            Crown::Spruce,
        ),
        TreeType::Birch => {
            let heights = if below(rng, TALL_BIRCH_ODDS) == 0 {
                TALL_BIRCH_HEIGHT_RANGE
            } else {
                BIRCH_HEIGHT_RANGE
            };
            Tree::new(
                Kind::BirchLog,
                Kind::BirchLeaves,
                draw(rng, heights),
                Crown::Round,
            )
        }
        TreeType::Jungle => Tree::new(
            Kind::JungleLog,
            Kind::JungleLeaves,
            JUNGLE_HEIGHT,
            Crown::Round,
        ),
        TreeType::Acacia => Tree::new(
            Kind::AcaciaLog,
            Kind::AcaciaLeaves,
            ACACIA_ROOM_HEIGHT,
            Crown::Acacia,
        ),
        TreeType::Crimson | TreeType::Warped => {
            let crimson = matches!(kind, TreeType::Crimson);
            let (stem, hat) = if crimson {
                (Kind::CrimsonStem, Kind::NetherWartBlock)
            } else {
                (Kind::WarpedStem, Kind::WarpedWartBlock)
            };
            let base = draw(rng, FUNGUS_HEIGHT_RANGE);
            let height = if below(rng, FUNGUS_DOUBLE_HEIGHT_ODDS) == 0 {
                base * 2
            } else {
                base
            };
            let fungus = Fungus {
                stem: stem.default_state(),
                hat: hat.default_state(),
                height,
                crimson,
                huge: rng.unit() < FUNGUS_HUGE_CHANCE,
            };
            return Some(nether(world, pos, rng, &fungus).blocks);
        }
        TreeType::Azalea => Tree::new(
            Kind::OakLog,
            Kind::AzaleaLeaves,
            draw(rng, AZALEA_ROOM_HEIGHT_RANGE),
            Crown::Azalea,
        ),
    };
    tree.place(world, pos, kind, rng).map(|tx| tx.blocks)
}

/// Draws a huge fungus at `pos`: its stem, then its hat layer by layer from the bottom up.
fn nether(world: &World, pos: BlockPos, rng: &mut Random, fungus: &Fungus) -> Tx {
    let can_override =
        |s: block::State| s.can_be_replaced() || s.has_type_tag(TypeTag::HugeFungusReplaceable);
    let mut tx = Tx::default();
    let stem_reach = i32::from(fungus.huge);
    for dx in -stem_reach..=stem_reach {
        for dz in -stem_reach..=stem_reach {
            for dy in 0..fungus.height {
                // Each cell of a corner column draws whether it grows, whatever block is there.
                if dx != 0 && dz != 0 && rng.unit() >= FUNGUS_CORNER_STEM_CHANCE {
                    continue;
                }
                let at = pos.offset(dx, dy, dz);
                if can_override(tx.get(world, at)) {
                    tx.insert(at, fungus.stem);
                }
            }
        }
    }
    let hat_height = (draw(rng, 0..=fungus.height / 3) + FUNGUS_HAT_MIN_HEIGHT).min(fungus.height);
    let bottom = fungus.height - hat_height;
    let top = fungus.height;
    let decor = Kind::Shroomlight.default_state();
    for layer in bottom..=top {
        let taper = draw(rng, FUNGUS_HAT_TAPER_RANGE);
        let mut radius = if layer < top - taper {
            FUNGUS_HAT_RADIUS
        } else {
            FUNGUS_HAT_TOP_RADIUS
        };
        if hat_height > FUNGUS_TALL_HAT_HEIGHT && layer < bottom + FUNGUS_WIDE_HAT_LAYERS {
            radius = FUNGUS_WIDE_HAT_RADIUS;
        }
        if fungus.huge {
            radius += 1;
        }
        let is_low = layer < bottom + FUNGUS_LOW_HAT_LAYERS;
        for dx in -radius..=radius {
            for dz in -radius..=radius {
                let Some(at) = pos.offset(dx, layer, dz) else {
                    continue;
                };
                if !can_override(tx.get(world, Some(at))) {
                    continue;
                }
                let on_x_rim = dx.abs() == radius;
                let on_z_rim = dz.abs() == radius;
                let is_inner = !on_x_rim && !on_z_rim && layer != top;
                if is_low {
                    if is_inner {
                        continue;
                    }
                    // A rim cell over a hat block always closes the hat; others close it by chance.
                    if tx.get(world, at.offset(0, -1, 0)).kind() == fungus.hat.kind() {
                        tx.insert(Some(at), fungus.hat);
                    } else if rng.unit() < FUNGUS_LOW_HAT_CHANCE {
                        tx.insert(Some(at), fungus.hat);
                        if fungus.crimson && below(rng, FUNGUS_LOW_VINE_ODDS) == 0 {
                            weeping_vines(world, &mut tx, at, rng, &can_override);
                        }
                    }
                    continue;
                }
                let (decor_chance, hat_chance, crimson_vine_chance) = if is_inner {
                    FUNGUS_INNER_HAT
                } else if on_x_rim && on_z_rim {
                    FUNGUS_CORNER_HAT
                } else {
                    FUNGUS_RIM_HAT
                };
                let vine_chance = if fungus.crimson {
                    crimson_vine_chance
                } else {
                    0.0
                };
                hat_block(
                    world,
                    &mut tx,
                    at,
                    rng,
                    (decor_chance, hat_chance, vine_chance),
                    (decor, fungus.hat),
                    &can_override,
                );
            }
        }
    }
    tx
}

/// Draws one hat cell at `at`: a decoration, or else a hat block that may grow vines, or nothing.
///
/// The vine chance is drawn after every hat block, even when it is 0.
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
        tx.insert(Some(at), decor);
        return;
    }
    if rng.unit() >= hat_chance {
        return;
    }
    tx.insert(Some(at), hat);
    if rng.unit() < vine_chance {
        weeping_vines(world, tx, at, rng, can_override);
    }
}

/// Draws a strand of weeping vines hanging from the hat block at `at`, older toward its end; it
/// stops at the first block it cannot grow through or below the world.
fn weeping_vines(
    world: &World,
    tx: &mut Tx,
    at: BlockPos,
    rng: &mut Random,
    can_override: &dyn Fn(block::State) -> bool,
) {
    if !can_override(tx.get(world, at.offset(0, -1, 0))) {
        return;
    }
    let mut length = draw(rng, VINE_LENGTH_RANGE);
    if below(rng, VINE_DOUBLE_ODDS) == 0 {
        length *= 2;
    }
    let vine = Kind::WeepingVines.default_state();
    for hang in 0..length {
        let Some(cell) = at.offset(0, -1 - hang, 0) else {
            break;
        };
        if cell.y < MIN_Y || !can_override(tx.get(world, Some(cell))) {
            break;
        }
        let age = PropertyValue::Int((VINE_TOP_AGE + hang).min(MAX_VINE_AGE));
        if let Some(aged) = vine.with(nether_vines_age(vine), age) {
            tx.insert(Some(cell), aged);
        }
    }
}

/// Draws a value of `range` with one draw.
fn draw(rng: &mut Random, range: RangeInclusive<i32>) -> i32 {
    let (lowest, highest) = range.into_inner();
    lowest + below(rng, highest - lowest + 1)
}

/// Draws the difference of two draws below `span`: a value within `span - 1` of 0, near 0 most
/// often.
fn spread(rng: &mut Random, span: i32) -> i32 {
    below(rng, span) - below(rng, span)
}

/// The name of the age property of nether vines: weeping or twisting.
fn nether_vines_age(block: block::State) -> &'static str {
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
