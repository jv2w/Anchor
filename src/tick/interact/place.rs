//! Placing blocks: which states an item puts where.
//!
//! A placement does not check the place: the client's position is taken wherever it is. What the
//! rules decide is the state of the placed block (facing, half, axis, stacking, the form an item
//! takes) and the blocks that go together (doors, a bed, tall plants).
//!
//! [`placement`] only reads the world: it returns the blocks to set, which the caller checks
//! (entities in the way, adventure mode, events) and then applies.

use super::props::{self, Axis};
use crate::block::{self, Class, Face, Kind, State, SupportType, Trait, TypeTag};
use crate::item;
use crate::math::Vec3;
use crate::world::{BlockPos, Entry, MAX_Y, MIN_Y, World};

/// The distance in blocks that decides whether a barrel faces up or down: the player's feet closer
/// than this sideways, the eyes higher than this above it.
const BARREL_NEAR: f64 = 2.0;
/// The `candles` of a full set of four candles.
const CANDLES_FULL: i32 = 3;
/// The `cluster_count` of four sea pickles.
const CLUSTER_FULL: i32 = 3;
/// The most blocks one placement sets: two for a door, a bed or a tall plant.
const MAX_BLOCKS: usize = 2;
/// The `growth` of four pink petals.
const PETALS_FULL: i32 = 3;
/// The `height` of a full block of eight snow layers.
const SNOW_FULL: i32 = 7;

/// One placement attempt.
struct Attempt<'a> {
    world: &'a World,
    item: item::Type,
    /// Where the block goes, and what is there.
    replace: BlockPos,
    replace_state: State,
    clicked_state: State,
    /// The face the rules see: up for the attempt on the clicked block itself.
    face: Face,
    click: Vec3,
    placer: Option<&'a Placer>,
}

impl Attempt<'_> {
    /// The placer's horizontal facing; `None` without a placer.
    fn facing(&self) -> Option<Face> {
        self.placer.map(Placer::facing)
    }

    /// The block next to the replaced position toward `face`.
    fn side(&self, face: Face) -> State {
        block_at(self.world, self.replace.step(face))
    }

    /// Whether `state` stacks onto the replaced block, which the click is on: the block is of the
    /// same kind and has room, a slab half is free at the clicked height (a candle, sea pickle,
    /// pink petals or slab).
    fn stacks(&self, state: State) -> bool {
        let existing = self.replace_state;
        if existing.kind() != state.kind() {
            return false;
        }
        if state.is(Class::Candle) {
            props::int(existing, "candles").is_some_and(|c| c < CANDLES_FULL)
        } else if state.is(Class::PinkPetals) {
            props::int(existing, "growth").is_some_and(|c| c < PETALS_FULL)
        } else if state.is(Class::SeaPickle) {
            props::int(existing, "cluster_count").is_some_and(|c| c < CLUSTER_FULL)
        } else if state.is(Class::Slab) {
            !props::is_double_slab(existing)
                && if is_top_slab(existing) {
                    self.click.y <= 0.5
                } else {
                    self.click.y >= 0.5
                }
        } else {
            false
        }
    }

    /// The support the block toward `face` of the replaced position gives back.
    fn support(&self, face: Face) -> SupportType {
        adjacent_support(self.world, self.replace, face)
    }

    /// Whether the block goes in the upper half: against the bottom of the block above, or high
    /// up on a side.
    fn upper_half(&self) -> bool {
        (self.face != Face::Up && self.click.y > 0.5) || self.face == Face::Down
    }
}

/// The blocks a placement sets, in position order: one, or two for a door, a bed or a tall plant.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Blocks {
    slots: [Option<(BlockPos, State)>; MAX_BLOCKS],
}

impl Blocks {
    /// The first block in position order.
    pub(super) fn first(&self) -> Option<&(BlockPos, State)> {
        self.iter().next()
    }

    /// The blocks in position order.
    pub(super) fn iter(&self) -> impl Iterator<Item = &(BlockPos, State)> {
        self.into_iter()
    }

    /// The blocks as a list, to hand to an event.
    pub(super) fn to_vec(&self) -> Vec<(BlockPos, State)> {
        self.iter().copied().collect()
    }

    /// The block `first` and the block `second` with it, if there is one, in position order.
    fn new(first: (BlockPos, State), second: Option<(BlockPos, State)>) -> Self {
        let slots = match second {
            Some(second) if second.0 < first.0 => [Some(second), Some(first)],
            _ => [Some(first), second],
        };
        Self { slots }
    }
}

impl<'a> IntoIterator for &'a Blocks {
    type Item = &'a (BlockPos, State);
    type IntoIter = std::iter::Flatten<std::slice::Iter<'a, Option<(BlockPos, State)>>>;

    fn into_iter(self) -> Self::IntoIter {
        self.slots.iter().flatten()
    }
}

/// What the placing player shows the placement rules.
#[derive(Debug, Clone, Copy)]
pub(super) struct Placer {
    /// The feet position.
    pub(super) feet: Vec3,
    /// The eye position.
    pub(super) eye: Vec3,
    /// The yaw in degrees.
    pub(super) yaw: f64,
    /// Whether the player is sneaking.
    pub(super) sneaking: bool,
}

impl Placer {
    /// The horizontal facing.
    fn facing(&self) -> Face {
        props::horizontal_facing(self.yaw)
    }
}

/// The placement of `item` clicked on `face` of the block at `clicked`, at `click` inside it
/// (each component 0 to 1): the blocks to set, or `None` if the item places nothing there.
///
/// The clicked block itself is tried first, as if its top was clicked, when it is replaceable or
/// the item stacks onto it (grass, a slab half); otherwise the block goes next to the clicked
/// face.
pub(super) fn placement(
    world: &World,
    item: item::Type,
    clicked: BlockPos,
    face: Face,
    click: Vec3,
    placer: Option<&Placer>,
) -> Option<Blocks> {
    let clicked_state = world.block(clicked)?;
    if clicked_state == block::AIR {
        return None;
    }
    let attempt = |replace: BlockPos, face: Face| Attempt {
        world,
        item,
        replace,
        replace_state: block_at(world, Some(replace)),
        clicked_state,
        face,
        click,
        placer,
    };
    let onto = attempt(clicked, Face::Up);
    if takes(&onto)
        && let Some(blocks) = transaction(&onto)
    {
        return Some(blocks);
    }
    let replace = clicked.step(face)?;
    world.block(replace)?;
    let next = attempt(replace, face);
    if !takes(&next) {
        return None;
    }
    transaction(&next)
}

/// Whether the item's block goes into the replaced position as it is: the block there is
/// replaceable or the item stacks onto it.
fn takes(attempt: &Attempt<'_>) -> bool {
    attempt.replace_state.can_be_replaced()
        || candidates(attempt)
            .into_iter()
            .flatten()
            .any(|state| attempt.stacks(state))
}

/// The transaction of one attempt: the first form of the item whose rules give a state, placed.
fn transaction(attempt: &Attempt<'_>) -> Option<Blocks> {
    for state in candidates(attempt).into_iter().flatten() {
        // A form whose rules refuse leaves the next to try (a hanging sign's edges, then its
        // centre).
        let Some(blocks) = place(attempt, state) else {
            continue;
        };
        if blocks.iter().any(|(p, _)| !(MIN_Y..=MAX_Y).contains(&p.y)) {
            return None;
        }
        // The other cell of a two-block block goes into a replaceable block only.
        let blocked = blocks.iter().any(|(p, _)| {
            *p != attempt.replace && !block_at(attempt.world, Some(*p)).can_be_replaced()
        });
        if blocked {
            return None;
        }
        return Some(blocks);
    }
    None
}

/// The blocks the item tries for the face, in order: one for most items; a hanging sign tries
/// its ceiling forms.
fn candidates(attempt: &Attempt<'_>) -> [Option<State>; 2] {
    let Some(state) = attempt.item.placed_block(Some(attempt.face)) else {
        return [None, None];
    };
    if !state.has_type_tag(TypeTag::HangingSign) {
        return [Some(state), None];
    }
    let hanging = |hanging: bool, attached: bool| {
        props::with_flag(state, "hanging", hanging)
            .and_then(|s| props::with_flag(s, "attached_bit", attached))
    };
    if attempt.face != Face::Down {
        return [hanging(false, false), None];
    }
    let edges = attempt
        .placer
        .is_none_or(|p| !p.sneaking)
        .then(|| hanging(true, false))
        .flatten();
    [edges, hanging(true, true)]
}

/// The blocks to set to place the state: its state rules and the blocks that go with it. `None`
/// for a form the click cannot give (a wall form on the top face).
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one arm per block class keeps the rules side by side"
)]
fn place(attempt: &Attempt<'_>, state: State) -> Option<Blocks> {
    let mut s = state;
    let mut second: Option<(BlockPos, State)> = None;
    let face = attempt.face;
    let set_facing = |s: State, f: Face| props::with_facing(s, f).unwrap_or(s);
    let opposite_player = |s: State| attempt.facing().map_or(s, |f| set_facing(s, f.opposite()));
    for class in state.kind().classes() {
        match class {
            Class::AmethystCluster | Class::Button | Class::ItemFrame => {
                s = set_facing(s, face);
            }
            Class::Anvil => {
                if let Some(f) = attempt.facing() {
                    s = set_facing(s, props::rotate_counterclockwise(f));
                }
            }
            Class::Barrel => {
                if let Some(p) = attempt.placer {
                    s = set_facing(s, barrel_facing(attempt.replace, p));
                }
            }
            Class::BaseBigDripleaf => {
                let below_pos = attempt.replace.step(Face::Down)?;
                let below = block_at(attempt.world, Some(below_pos));
                s = opposite_player(s);
                // On a head the plant grows: the head below becomes its stem.
                if below.is(Class::BaseBigDripleaf)
                    && props::flag(below, "big_dripleaf_head") == Some(true)
                {
                    let f = props::facing(below)?;
                    s = set_facing(s, f);
                    second = Some((
                        below_pos,
                        set_facing(Kind::BigDripleafStem.default_state(), f),
                    ));
                }
            }
            Class::Bed => return bed(attempt, s),
            Class::Bell => {
                let attachment = match face {
                    Face::Down => "hanging",
                    Face::Up => {
                        s = opposite_player(s);
                        "standing"
                    }
                    _ => {
                        s = set_facing(s, face);
                        if attempt.support(face) == SupportType::None {
                            "side"
                        } else {
                            "multiple"
                        }
                    }
                };
                s = props::with_string(s, "attachment", attachment)?;
            }
            Class::Block => {
                if s.has_trait(Trait::FacesOppositePlacingPlayer) {
                    s = opposite_player(s);
                }
                if s.has_trait(Trait::PillarRotation) {
                    s = props::with_axis(s, Axis::of(face)).unwrap_or(s);
                }
                return Some(Blocks::new((attempt.replace, s), second));
            }
            Class::Campfire => {
                if let Some(f) = attempt.facing() {
                    s = set_facing(s, f);
                }
                s = props::with_flag(s, "extinguished", false)?;
            }
            Class::Candle => {
                let existing = attempt.replace_state;
                if existing.kind() == s.kind()
                    && let Some(count) =
                        props::int(existing, "candles").filter(|c| *c < CANDLES_FULL)
                {
                    s = props::with_int(s, "candles", count + 1)?;
                    s = props::with_flag(s, "lit", props::flag(existing, "lit")?)?;
                }
            }
            Class::CeilingCenterHangingSign => {
                if face != Face::Down {
                    return None;
                }
                if let Some(p) = attempt.placer {
                    s = props::with_int(s, props::ROTATION, props::rotation_from_yaw(p.yaw))
                        .unwrap_or(s);
                }
            }
            Class::CeilingEdgesHangingSign => {
                if face != Face::Down {
                    return None;
                }
                s = opposite_player(s);
                // A sign hangs by its edges from a full block or along another hanging sign; a
                // thinner block above gives the form that hangs from its centre.
                let up = attempt.side(Face::Up);
                let along = |other: State| {
                    (other.is(Class::WallHangingSign) || other.is(Class::CeilingEdgesHangingSign))
                        && props::facing(other).map(Axis::of) == props::facing(s).map(Axis::of)
                };
                if up.support(Face::Down) != SupportType::Full && !along(up) {
                    return None;
                }
            }
            Class::CocoaBlock
            | Class::Ladder
            | Class::OminousWallBanner
            | Class::TripwireHook
            | Class::WallBanner
            | Class::WallSign => {
                if Axis::of(face) == Axis::Y {
                    return None;
                }
                s = set_facing(s, face);
            }
            Class::Door => return door(attempt, s),
            Class::DoublePlant => {
                let up = attempt.replace.step(Face::Up)?;
                let top = props::with_flag(s, "upper_block_bit", true)?;
                return Some(Blocks::new((attempt.replace, s), Some((up, top))));
            }
            Class::EndRod => {
                let mut f = face;
                if attempt.clicked_state.is(Class::EndRod)
                    && props::facing(attempt.clicked_state) == Some(f)
                {
                    f = f.opposite();
                }
                s = set_facing(s, f);
            }
            Class::FenceGate => {
                if let Some(f) = attempt.facing() {
                    s = set_facing(s, f);
                }
            }
            Class::FloorBanner | Class::FloorSign | Class::OminousFloorBanner => {
                if face != Face::Up {
                    return None;
                }
                if let Some(p) = attempt.placer {
                    s = props::with_int(s, props::ROTATION, props::rotation_from_yaw(p.yaw))
                        .unwrap_or(s);
                }
            }
            Class::FloorCoralFan => {
                if let Some(p) = attempt.placer {
                    s = props::with_int(
                        s,
                        "coral_fan_direction",
                        coral_fan_axis(attempt.replace, p),
                    )
                    .unwrap_or(s);
                }
                s = coral_dead(attempt, s);
            }
            Class::GlowLichen | Class::ResinClump => s = multi_support(attempt, s)?,
            Class::Hopper => {
                let f = if face == Face::Down {
                    Face::Down
                } else {
                    face.opposite()
                };
                s = set_facing(s, f);
            }
            Class::Lantern => {
                // Hangs from above when clicked from below, or when the block below holds
                // nothing to stand on.
                let hangs = face == Face::Down || !attempt.support(Face::Down).has_center_support();
                s = props::with_flag(s, "hanging", hangs)?;
            }
            Class::Leaves => s = props::with_flag(s, "persistent_bit", true)?,
            Class::Lever => {
                let east_west = attempt.facing().is_none_or(|f| Axis::of(f) == Axis::X);
                let direction = match face {
                    Face::Down if east_west => "down_east_west",
                    Face::Down => "down_north_south",
                    Face::Up if east_west => "up_east_west",
                    Face::Up => "up_north_south",
                    other => props::face_name(other),
                };
                s = props::with_string(s, "lever_direction", direction)?;
            }
            Class::LightningRod => s = set_facing(s, face),
            Class::MobHead => {
                if face == Face::Down {
                    return None;
                }
                // The rotation on the floor is the block entity's.
                s = set_facing(s, face);
            }
            Class::PinkPetals => {
                let existing = attempt.replace_state;
                if existing.kind() == s.kind()
                    && let Some(count) = props::int(existing, "growth").filter(|c| *c < PETALS_FULL)
                {
                    s = props::with_int(s, "growth", count + 1)?;
                    s = set_facing(s, props::facing(existing)?);
                } else {
                    s = opposite_player(s);
                }
            }
            Class::RedstoneComparator | Class::RedstoneRepeater => s = opposite_player(s),
            Class::SeaPickle => {
                // Sea pickles are placed out of water, so dead.
                s = props::with_flag(s, "dead_bit", true)?;
                let existing = attempt.replace_state;
                if existing.kind() == s.kind()
                    && let Some(count) =
                        props::int(existing, "cluster_count").filter(|c| *c < CLUSTER_FULL)
                {
                    s = props::with_int(s, "cluster_count", count + 1)?;
                }
            }
            Class::Slab => s = slab(attempt, s)?,
            Class::SmallDripleaf => {
                let up = attempt.replace.step(Face::Up)?;
                s = opposite_player(s);
                let top = props::with_flag(s, "upper_block_bit", true)?;
                second = Some((up, top));
            }
            Class::SnowLayer => {
                let existing = attempt.replace_state;
                if existing.kind() == s.kind()
                    && let Some(height) = props::int(existing, "height").filter(|h| *h < SNOW_FULL)
                {
                    s = props::with_int(s, "height", height + 1)?;
                }
            }
            Class::Stair => {
                if let Some(f) = attempt.facing() {
                    s = set_facing(s, f);
                }
                s = props::with_flag(s, "upside_down_bit", attempt.upper_half())?;
            }
            Class::Torch => s = torch(attempt, s)?,
            Class::Trapdoor => {
                s = opposite_player(s);
                if attempt.upper_half() {
                    s = props::with_flag(s, "upside_down_bit", true)?;
                }
            }
            Class::Vine => {
                if Axis::of(face) == Axis::Y {
                    return None;
                }
                let existing = attempt.replace_state;
                let bits = if existing.kind() == s.kind() {
                    props::int(existing, "vine_direction_bits")?
                } else {
                    0
                };
                let bit = props::vine_bit(face.opposite()).unwrap_or(0);
                s = props::with_int(s, "vine_direction_bits", bits | bit)?;
            }
            Class::WallCoralFan => {
                if Axis::of(face) == Axis::Y {
                    return None;
                }
                s = set_facing(s, face);
                s = coral_dead(attempt, s);
            }
            Class::WallHangingSign => s = wall_hanging_sign(attempt, s)?,
            _ => {}
        }
    }
    Some(Blocks::new((attempt.replace, s), second))
}

/// The facing of a barrel placed at `pos` by the placer `p`: up or down when the player is close
/// and above or below, else toward the player.
fn barrel_facing(pos: BlockPos, p: &Placer) -> Face {
    let near = (p.feet.x - f64::from(pos.x)).abs() < BARREL_NEAR
        && (p.feet.z - f64::from(pos.z)).abs() < BARREL_NEAR;
    if near {
        let y = p.eye.y;
        if y - f64::from(pos.y) > BARREL_NEAR {
            return Face::Up;
        }
        if f64::from(pos.y) - y > 0.0 {
            return Face::Down;
        }
    }
    p.facing().opposite()
}

/// The blocks of a bed: the foot here and the head one block toward the player's facing.
fn bed(attempt: &Attempt<'_>, state: State) -> Option<Blocks> {
    let facing = attempt.facing().unwrap_or(Face::North);
    let foot = props::with_facing(state, facing)?;
    let next = attempt.replace.step(facing)?;
    let head = props::with_flag(foot, "head_piece_bit", true)?;
    Some(Blocks::new((attempt.replace, foot), Some((next, head))))
}

/// The blocks of a door: the top half above, hinged right next to another door or against a
/// wall.
fn door(attempt: &Attempt<'_>, state: State) -> Option<Blocks> {
    let up = attempt.replace.step(Face::Up)?;
    let facing = attempt.facing().or_else(|| props::facing(state))?;
    let mut bottom = props::with_facing(state, facing)?;
    let next = attempt.side(props::rotate_counterclockwise(facing));
    let next2 = attempt.side(props::rotate_clockwise(facing));
    if next.kind() == state.kind() || (!next2.is_transparent() && next.is_transparent()) {
        bottom = props::with_flag(bottom, "door_hinge_bit", true)?;
    }
    let top = props::with_flag(bottom, "upper_block_bit", true)?;
    Some(Blocks::new((attempt.replace, bottom), Some((up, top))))
}

/// The axis of a coral fan placed at `pos` by the placer `p` (Bedrock `coral_fan_direction`, X 0
/// and Z 1): X when the fan is more south of the player's block than east or west, else Z (the
/// 45° edges are Z).
fn coral_fan_axis(pos: BlockPos, p: &Placer) -> i32 {
    let feet = p.feet.floor();
    let dx = f64::from(pos.x) - feet.x;
    let dz = f64::from(pos.z) - feet.z;
    i32::from(dz <= dx.abs())
}

/// The coral state: dead unless water is next to the replaced position.
fn coral_dead(attempt: &Attempt<'_>, state: State) -> State {
    let water = Face::all()
        .iter()
        .any(|&f| attempt.side(f).is(Class::Water));
    if water {
        return state;
    }
    props::dead_coral(state).unwrap_or(state)
}

/// The state with the existing faces plus the face opposite the clicked one, or the first face
/// that gives full support when that one is taken.
fn multi_support(attempt: &Attempt<'_>, state: State) -> Option<State> {
    let existing = attempt.replace_state;
    let mut bits = if existing.kind() == state.kind() {
        props::int(existing, "multi_face_direction_bits")?
    } else {
        0
    };
    let free = |face: Face| {
        bits & props::multi_face_bit(face) == 0 && attempt.support(face) == SupportType::Full
    };
    let opposite = attempt.face.opposite();
    let placed = if free(opposite) {
        opposite
    } else {
        Face::all()
            .iter()
            .copied()
            .find(|face| free(*face))
            .unwrap_or(opposite)
    };
    bits |= props::multi_face_bit(placed);
    props::with_int(state, "multi_face_direction_bits", bits)
}

/// The slab state: the double slab when it merges into the existing slab's empty half, else the
/// top or bottom half.
fn slab(attempt: &Attempt<'_>, state: State) -> Option<State> {
    let existing = attempt.replace_state;
    let face = attempt.face;
    if existing.kind() == state.kind() && !props::is_double_slab(existing) {
        let top = is_top_slab(existing);
        if (top && (attempt.click.y <= 0.5 || face == Face::Up))
            || (!top && (attempt.click.y >= 0.5 || face == Face::Down))
        {
            return props::double_slab(state);
        }
    }
    let half = if attempt.upper_half() {
        "top"
    } else {
        "bottom"
    };
    props::with_string(state, "minecraft:vertical_half", half)
}

/// The torch state: on the clicked side, standing when clicked from below.
fn torch(attempt: &Attempt<'_>, state: State) -> Option<State> {
    let facing = if attempt.face == Face::Down {
        Face::Up
    } else {
        attempt.face
    };
    props::with_facing(state, facing)
}

/// The wall hanging sign state: hung from the block beside it (or the one opposite when only
/// that holds it), facing the player where it can.
fn wall_hanging_sign(attempt: &Attempt<'_>, state: State) -> Option<State> {
    let player = attempt.facing()?;
    let attach = if Axis::of(attempt.face) == Axis::Y {
        props::rotate_clockwise(player)
    } else {
        attempt.face
    };
    let supported = |f: Face| {
        let b = attempt.side(f);
        (b.is(Class::WallHangingSign)
            && props::facing(b).map(|bf| Axis::of(props::rotate_clockwise(bf)))
                == Some(Axis::of(f)))
            || b.support(f.opposite()) == SupportType::Full
    };
    let direction = if supported(attach) || !supported(attach.opposite()) {
        attach
    } else {
        attach.opposite()
    };
    let mut facing = props::rotate_clockwise(direction.opposite());
    if facing == player {
        facing = facing.opposite();
    }
    props::with_facing(state, facing)
}

/// Whether a single slab is in the top half.
fn is_top_slab(state: State) -> bool {
    props::string(state, "minecraft:vertical_half") == Some("top")
}

/// The support the block next to `pos` toward `face` gives back.
fn adjacent_support(world: &World, pos: BlockPos, face: Face) -> SupportType {
    block_at(world, pos.step(face)).support(face.opposite())
}

/// The block at `pos`, air where there is none (an unloaded chunk, outside the world), so the
/// rules never load a chunk. An unknown state reads as bedrock (solid and full), which a
/// placement does not replace.
pub(crate) fn block_at(world: &World, pos: Option<BlockPos>) -> State {
    match pos.and_then(|p| world.entry(p)) {
        Some(Entry::Known(s)) => s,
        Some(Entry::Unknown(_)) => block::BEDROCK,
        None => block::AIR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::world::{Chunk, ChunkPos, Kind as WorldKind};

    /// An empty world with the chunks around the origin loaded and stone at the origin.
    fn world() -> World {
        let mut world = World::new("place", WorldKind::Temporary, 0, BlockPos::new(0, 0, 0));
        for x in -1..=1 {
            for z in -1..=1 {
                world.insert_chunk(ChunkPos::new(x, z), Chunk::new());
            }
        }
        world
            .set_block(BlockPos::new(0, 0, 0), Kind::Stone.default_state())
            .unwrap();
        world
    }

    /// A player south of the origin looking north.
    fn placer() -> Placer {
        let feet = Vec3::new(0.5, 1.0, 3.5);
        Placer {
            feet,
            eye: feet + Vec3::new(0.0, 1.62, 0.0),
            yaw: 180.0,
            sneaking: false,
        }
    }

    /// The item of the block `state`; a block whose item the block entity decides (a bed) has
    /// the item of its name.
    fn item_of(state: State) -> item::Type {
        match state.item() {
            block::ItemForm::Item(item) => item,
            block::ItemForm::BlockEntity => {
                item::Type::from_name(state.name(), 0, None, &crate::nbt::Compound::new())
                    .unwrap_or_else(|| panic!("{state:?} has an item"))
            }
            block::ItemForm::None => panic!("{state:?} has an item"),
        }
    }

    /// The blocks the item of `state` sets when its block at the origin is clicked on `face`
    /// at the height `click_y`.
    fn placed(world: &World, state: State, face: Face, click_y: f64) -> Option<Blocks> {
        placement(
            world,
            item_of(state),
            BlockPos::new(0, 0, 0),
            face,
            Vec3::new(0.5, click_y, 0.5),
            Some(&placer()),
        )
    }

    /// A placement checks nothing about the place: blocks that need a floor, a wall or light go
    /// down in the air, beside the clicked face.
    #[test]
    fn a_block_is_placed_where_nothing_holds_it() {
        let w = world();
        for state in [
            Kind::Rail.default_state(),
            Kind::RedMushroom.default_state(),
            Kind::Stone.default_state(),
        ] {
            let blocks = placed(&w, state, Face::East, 0.5).unwrap();
            assert_eq!(blocks.first().map(|b| b.0), Some(BlockPos::new(1, 0, 0)));
        }
    }

    /// A door sets its top half above, hinged by the rules, and both halves are in the list.
    #[test]
    fn a_door_is_placed_as_two_blocks() {
        let w = world();
        let door = State::all().find(|s| s.is(Class::Door)).unwrap();
        let list = placed(&w, door, Face::East, 0.5).unwrap().to_vec();
        let [(bottom_pos, bottom), (top_pos, top)] = list.as_slice() else {
            panic!("two blocks: {list:?}");
        };
        assert_eq!(*bottom_pos, BlockPos::new(1, 0, 0));
        assert_eq!(*top_pos, BlockPos::new(1, 1, 0));
        assert_eq!(bottom.kind(), top.kind());
        assert_eq!(props::flag(*top, "upper_block_bit"), Some(true));
    }

    /// A bed sets its head one block toward the player's facing.
    #[test]
    fn a_bed_is_placed_as_foot_and_head() {
        let w = world();
        let bed = State::all().find(|s| s.is(Class::Bed)).unwrap();
        let list = placed(&w, bed, Face::East, 0.5).unwrap().to_vec();
        let heads: Vec<_> = list
            .iter()
            .map(|(_, s)| props::flag(*s, "head_piece_bit"))
            .collect();
        assert_eq!(list.len(), 2);
        assert!(heads.contains(&Some(true)) && heads.contains(&Some(false)));
    }

    /// A slab placed on a slab's free half makes a double slab in the clicked block; on its other
    /// half it goes next to it.
    #[test]
    fn slabs_stack_into_a_double_slab() {
        let mut w = world();
        let bottom = State::all()
            .find(|s| {
                s.is(Class::Slab)
                    && !props::is_double_slab(*s)
                    && props::string(*s, "minecraft:vertical_half") == Some("bottom")
            })
            .unwrap();
        w.set_block(BlockPos::new(0, 0, 0), bottom).unwrap();
        let merged = placed(&w, bottom, Face::Up, 0.75).unwrap().to_vec();
        let [(pos, state)] = merged.as_slice() else {
            panic!("one block: {merged:?}");
        };
        assert_eq!(*pos, BlockPos::new(0, 0, 0));
        assert!(props::is_double_slab(*state));
        let beside = placed(&w, bottom, Face::Up, 0.25).unwrap();
        assert_eq!(beside.first().map(|b| b.0), Some(BlockPos::new(0, 1, 0)));
    }

    /// A hanging sign clicked under a block too thin for the edge form hangs from its centre.
    #[test]
    fn a_hanging_sign_under_a_fence_hangs_from_its_centre() {
        let mut w = world();
        w.set_block(BlockPos::new(0, 2, 0), Kind::OakFence.default_state())
            .unwrap();
        let blocks = placement(
            &w,
            item::Kind::OakHangingSign.default_type(),
            BlockPos::new(0, 2, 0),
            Face::Down,
            Vec3::new(0.5, 0.0, 0.5),
            Some(&placer()),
        )
        .expect("the centre form places");
        let list = blocks.to_vec();
        let [(pos, state)] = list.as_slice() else {
            panic!("one block: {blocks:?}");
        };
        assert_eq!(*pos, BlockPos::new(0, 1, 0));
        assert_eq!(props::flag(*state, "hanging"), Some(true));
        assert_eq!(props::flag(*state, "attached_bit"), Some(true));
    }
}
