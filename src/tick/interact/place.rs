//! Placing blocks: which states an item puts where (PMMP `World::useItemOn`'s placement
//! transactions, `Item::getPlacementTransaction`, `Block::canBePlacedAt` and `Block::place` of
//! every class and trait, the `canBeSupportedAt` rules of `StaticSupportTrait`).
//!
//! [`placement`] only reads the world: it returns the blocks to set, which the caller checks
//! (entities in the way, adventure mode, events) and then applies.

use super::props::{self, Axis};
use crate::block::{self, Class, Face, Kind, State, SupportType, Trait, TypeTag};
use crate::item;
use crate::math::Vec3;
use crate::random::Random;
use crate::tick::update;
use crate::world::{BlockPos, Entry, MAX_Y, MIN_Y, World};

/// The distance in blocks that decides whether a barrel faces up or down: the player's feet closer
/// than this sideways, the eyes higher than this above it (PMMP `Barrel::place`).
const BARREL_NEAR: f64 = 2.0;
/// The `candles` of a full set of four candles.
const CANDLES_FULL: i32 = 3;
/// The `cluster_count` of four sea pickles.
const CLUSTER_FULL: i32 = 3;
/// The brightest light. A red mushroom counts a chunk that is not lit yet as this, so the
/// placement is refused until the light is there.
const FULL_LIGHT: u8 = 15;
/// The most blocks one placement sets: two for doors, beds and tall plants.
const MAX_BLOCKS: usize = 2;
/// The `growth` of four pink petals.
const PETALS_FULL: i32 = 3;
/// The brightest light a red mushroom may be placed in on ground that is not mycelium, podzol or
/// nylium (PMMP `RedMushroom::canBeSupportedAt`).
const RED_MUSHROOM_MAX_LIGHT: u8 = 12;
/// The `height` of a full block of eight snow layers.
const SNOW_FULL: i32 = 7;

/// One placement attempt (PMMP `Item::getPlacementTransaction`).
struct Attempt<'a> {
    world: &'a World,
    item: item::Type,
    /// Where the block goes, and what is there.
    replace: BlockPos,
    replace_state: State,
    clicked: BlockPos,
    clicked_state: State,
    /// The face PMMP passes: up for the first attempt.
    face: Face,
    click: Vec3,
    placer: Option<&'a Placer>,
}

impl Attempt<'_> {
    /// Whether the attached block `state` holds on the `face` side of the replaced position
    /// ([`update::attached_support`]); `None` for a class without that rule.
    fn attached(&self, state: State, face: Face) -> Option<bool> {
        update::attached_support(state, face, self.support(face))
    }

    /// The placer's horizontal facing; `None` without a placer.
    fn facing(&self) -> Option<Face> {
        self.placer.map(Placer::facing)
    }

    /// The block next to the replaced position toward `face`.
    fn side(&self, face: Face) -> State {
        block_at(self.world, self.replace.step(face))
    }

    /// The support the block toward `face` of the replaced position gives back.
    fn support(&self, face: Face) -> SupportType {
        adjacent_support(self.world, self.replace, face)
    }

    /// Whether the block goes in the upper half: against the bottom of the block above, or high
    /// up on a side (PMMP `Slab::place`, `Stair::place`, `Trapdoor::place`).
    fn upper_half(&self) -> bool {
        (self.face != Face::Up && self.click.y > 0.5) || self.face == Face::Down
    }
}

/// The blocks a placement sets, in position order: one, or two for doors, beds and tall plants.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Blocks {
    slots: [Option<(BlockPos, State)>; MAX_BLOCKS],
}

impl Blocks {
    /// The block `first` and the block `second` with it, if there is one, in position order.
    fn new(first: (BlockPos, State), second: Option<(BlockPos, State)>) -> Self {
        let slots = match second {
            Some(second) if second.0 < first.0 => [Some(second), Some(first)],
            _ => [Some(first), second],
        };
        Self { slots }
    }

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
}

impl<'a> IntoIterator for &'a Blocks {
    type Item = &'a (BlockPos, State);
    type IntoIter = std::iter::Flatten<std::slice::Iter<'a, Option<(BlockPos, State)>>>;

    fn into_iter(self) -> Self::IntoIter {
        self.slots.iter().flatten()
    }
}

/// What the placing player shows the placement rules (PMMP reads these from the `Player`).
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
    /// The horizontal facing (PMMP `Entity::getHorizontalFacing`).
    fn facing(&self) -> Face {
        props::horizontal_facing(self.yaw)
    }
}

/// The placement of `item` clicked on `face` of the block at `clicked`, at `click` inside it
/// (each component 0 to 1): the blocks to set, or `None` if the item places nothing there.
///
/// As PMMP `World::useItemOn`, the clicked block itself is tried first, as if its top was
/// clicked (to replace grass, merge slabs), then the block next to the clicked face.
pub(super) fn placement(
    world: &World,
    item: item::Type,
    clicked: BlockPos,
    face: Face,
    click: Vec3,
    placer: Option<&Placer>,
    rng: &mut Random,
) -> Option<Blocks> {
    let clicked_state = placed_on(world, clicked)?;
    if clicked_state == block::AIR {
        return None;
    }
    let attempt = |replace: BlockPos, face: Face| Attempt {
        world,
        item,
        replace,
        replace_state: block_at(world, Some(replace)),
        clicked,
        clicked_state,
        face,
        click,
        placer,
    };
    if let Some(blocks) = transaction(&attempt(clicked, Face::Up), rng) {
        return Some(blocks);
    }
    let replace = clicked.step(face)?;
    placed_on(world, replace)?;
    transaction(&attempt(replace, face), rng)
}

/// The transaction of one attempt: the item's block for the face, checked and placed.
fn transaction(attempt: &Attempt<'_>, rng: &mut Random) -> Option<Blocks> {
    for state in candidates(attempt).into_iter().flatten() {
        if !can_be_placed_at(attempt, state) {
            continue;
        }
        // A form whose rules refuse leaves the next to try (PMMP
        // `HangingSign::getPlacementTransaction`).
        let Some(blocks) = place(attempt, state, rng) else {
            continue;
        };
        if blocks.iter().any(|(p, _)| !(MIN_Y..=MAX_Y).contains(&p.y)) {
            return None;
        }
        return Some(blocks);
    }
    None
}

/// The blocks the item tries for the face, in order: one for most items; a hanging sign tries
/// its ceiling forms (PMMP `HangingSign::getPlacementTransaction`).
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

/// Whether the state can be placed at the replaced position (PMMP `Block::canBePlacedAt` of its
/// class).
fn can_be_placed_at(attempt: &Attempt<'_>, state: State) -> bool {
    let replace = attempt.replace_state;
    let is_clicked = attempt.replace == attempt.clicked;
    let static_support = state.has_trait(Trait::StaticSupport);
    for class in state.kind().classes() {
        match class {
            Class::Candle => {
                return if replace.kind() == state.kind() {
                    props::int(replace, "candles").is_some_and(|c| c < CANDLES_FULL)
                } else {
                    replace.can_be_replaced()
                };
            }
            Class::Flowable if !static_support => return flowable_placeable(state, replace),
            Class::Light => {
                return replace.can_be_replaced() && replace.kind() != state.kind();
            }
            Class::PinkPetals => {
                return (replace.kind() == state.kind()
                    && props::int(replace, "growth").is_some_and(|c| c < PETALS_FULL))
                    || (supported_at(attempt, state) && flowable_placeable(state, replace));
            }
            Class::SeaPickle => {
                return (replace.kind() == state.kind()
                    && props::int(replace, "cluster_count").is_some_and(|c| c < CLUSTER_FULL))
                    || replace.can_be_replaced();
            }
            Class::Slab => {
                if replace.can_be_replaced() {
                    return true;
                }
                if replace.kind() == state.kind() && !props::is_double_slab(replace) {
                    return if is_top_slab(replace) {
                        attempt.click.y <= 0.5 || (!is_clicked && attempt.face == Face::Up)
                    } else {
                        attempt.click.y >= 0.5 || (!is_clicked && attempt.face == Face::Down)
                    };
                }
                return false;
            }
            Class::WaterLily => {
                return !replace.is(Class::Water)
                    && supported_at(attempt, state)
                    && flowable_placeable(state, replace);
            }
            _ if static_support => {
                return supported_at(attempt, state) && flowable_placeable(state, replace);
            }
            _ => {}
        }
    }
    replace.can_be_replaced()
}

/// Whether the state can go into `replace` (PMMP `Flowable::canBePlacedAt`: a block liquids
/// destroy does not go into a liquid; `Block::canBePlacedAt` for blocks that are not flowable).
fn flowable_placeable(state: State, replace: State) -> bool {
    if state.is(Class::Flowable) && state.can_be_flowed_into() && replace.is(Class::Liquid) {
        return false;
    }
    replace.can_be_replaced()
}

/// Whether the state stands at the replaced position (PMMP `canBeSupportedAt` of its class,
/// [`update::static_support`] with the placement reads; a class without a rule stands).
fn supported_at(attempt: &Attempt<'_>, state: State) -> bool {
    update::static_support(state, attempt.replace, |p| {
        Some(block_at(attempt.world, Some(p)))
    })
    .unwrap_or(true)
}

/// The blocks to set to place the state (PMMP `Block::place` of its class, with the traits that
/// place).
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one arm per PMMP class keeps the port side by side with PMMP"
)]
fn place(attempt: &Attempt<'_>, state: State, rng: &mut Random) -> Option<Blocks> {
    let mut s = state;
    let mut second: Option<(BlockPos, State)> = None;
    let face = attempt.face;
    let set_facing = |s: State, f: Face| props::with_facing(s, f).unwrap_or(s);
    let opposite_player = |s: State| attempt.facing().map_or(s, |f| set_facing(s, f.opposite()));
    for class in state.kind().classes() {
        match class {
            Class::AmethystCluster | Class::Button | Class::ItemFrame => {
                if !attempt.attached(state, face.opposite())? {
                    return None;
                }
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
            Class::BaseBanner | Class::BaseOminousBanner => {
                // The supporting face: down for a floor banner, behind a wall banner.
                let behind = props::facing(s).map_or(Face::Down, Face::opposite);
                let support = if s.is(Class::FloorBanner) || s.is(Class::OminousFloorBanner) {
                    Face::Down
                } else {
                    behind
                };
                if !attempt.side(support).is_solid() {
                    return None;
                }
            }
            Class::BaseBigDripleaf => {
                let below_pos = attempt.replace.step(Face::Down)?;
                let below = block_at(attempt.world, Some(below_pos));
                let head = below.is(Class::BaseBigDripleaf)
                    && props::flag(below, "big_dripleaf_head") == Some(true);
                if !(head
                    || below.kind() == Kind::Clay
                    || below.has_type_tag(TypeTag::Dirt)
                    || below.has_type_tag(TypeTag::Mud))
                {
                    return None;
                }
                s = opposite_player(s);
                if below.is(Class::BaseBigDripleaf) {
                    let f = props::facing(below)?;
                    s = set_facing(s, f);
                    second = Some((
                        below_pos,
                        set_facing(Kind::BigDripleafStem.default_state(), f),
                    ));
                }
            }
            Class::BaseRail => {
                if !attempt.support(Face::Down).has_edge_support() {
                    return None;
                }
            }
            Class::Bed => return bed(attempt, s),
            Class::Bell => {
                if attempt.support(face.opposite()) == SupportType::None {
                    return None;
                }
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
                if attempt.side(Face::Down).is(Class::Campfire) {
                    return None;
                }
                if let Some(f) = attempt.facing() {
                    s = set_facing(s, f);
                }
                s = props::with_flag(s, "extinguished", false)?;
            }
            Class::Candle => {
                if !attempt.support(Face::Down).has_center_support() {
                    return None;
                }
                let existing = attempt.replace_state;
                if existing.kind() == s.kind() {
                    let count = props::int(existing, "candles")?;
                    if count >= CANDLES_FULL {
                        return None;
                    }
                    s = props::with_int(s, "candles", count + 1)?;
                    s = props::with_flag(s, "lit", props::flag(existing, "lit")?)?;
                }
            }
            Class::CaveVines => {
                // PMMP draws the age that limits how far the vine grows.
                s = props::with_int(s, "growing_plant_age", rng.range(0, update::MAX_VINE_AGE))?;
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
                let up = attempt.side(Face::Up);
                let along = |other: State| {
                    (other.is(Class::WallHangingSign) || other.is(Class::CeilingEdgesHangingSign))
                        && props::facing(other).map(Axis::of) == props::facing(s).map(Axis::of)
                };
                if up.support(Face::Down) != SupportType::Full && !along(up) {
                    return None;
                }
            }
            Class::CocoaBlock => {
                let jungle = attempt.clicked_state.is(Class::Wood)
                    && matches!(
                        attempt.clicked_state.kind(),
                        Kind::JungleLog | Kind::JungleWood
                    );
                if Axis::of(face) == Axis::Y || !jungle {
                    return None;
                }
                s = set_facing(s, face);
            }
            Class::Door => return door(attempt, s),
            Class::DoublePlant => {
                let up = attempt.replace.step(Face::Up)?;
                if !attempt.side(Face::Down).has_type_tag(TypeTag::Dirt)
                    || !block_at(attempt.world, Some(up)).can_be_replaced()
                {
                    return None;
                }
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
            Class::Ladder => {
                if !attempt.attached(state, face.opposite())? || Axis::of(face) == Axis::Y {
                    return None;
                }
                s = set_facing(s, face);
            }
            Class::Lantern => {
                let down = attempt.attached(state, Face::Down)?;
                if !down && !attempt.attached(state, Face::Up)? {
                    return None;
                }
                s = props::with_flag(s, "hanging", face == Face::Down || !down)?;
            }
            Class::Leaves => s = props::with_flag(s, "persistent_bit", true)?,
            Class::Lever => {
                if !attempt.attached(state, face.opposite())? {
                    return None;
                }
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
            Class::NetherVines => {
                let age = rng.range(0, update::MAX_VINE_AGE - 1);
                s = props::with_int(s, update::nether_vines_age(s), age)?;
            }
            Class::OminousWallBanner
            | Class::TripwireHook
            | Class::WallBanner
            | Class::WallSign => {
                if Axis::of(face) == Axis::Y {
                    return None;
                }
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
            Class::RedMushroom => {
                let below = attempt.side(Face::Down);
                let light = attempt
                    .world
                    .full_light(attempt.replace, attempt.world.time_ticks())
                    .unwrap_or(FULL_LIGHT);
                if !((light <= RED_MUSHROOM_MAX_LIGHT && !below.is_transparent())
                    || matches!(below.kind(), Kind::Mycelium | Kind::Podzol)
                    || below.has_type_tag(TypeTag::Nylium))
                {
                    return None;
                }
            }
            Class::RedstoneComparator | Class::RedstoneRepeater => s = opposite_player(s),
            Class::SeaPickle => {
                // PMMP places sea pickles out of water (dead) until it has water logic.
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
                if block_at(attempt.world, Some(up)) != block::AIR
                    || attempt.side(Face::Down).kind() != Kind::Clay
                {
                    return None;
                }
                s = opposite_player(s);
                let top = props::with_flag(s, "upper_block_bit", true)?;
                second = Some((up, top));
            }
            Class::SnowLayer => {
                let existing = attempt.replace_state;
                if existing.kind() == s.kind() {
                    let height = props::int(existing, "height")?;
                    if height >= SNOW_FULL {
                        return None;
                    }
                    s = props::with_int(s, "height", height + 1)?;
                }
                if attempt.support(Face::Down) != SupportType::Full {
                    return None;
                }
            }
            Class::Stair => {
                if let Some(f) = attempt.facing() {
                    s = set_facing(s, f);
                }
                s = props::with_flag(s, "upside_down_bit", attempt.upper_half())?;
            }
            Class::Sugarcane => {
                let below_pos = attempt.replace.step(Face::Down)?;
                let below = block_at(attempt.world, Some(below_pos));
                if below.kind() != s.kind() {
                    let water = Face::HORIZONTAL
                        .into_iter()
                        .map(|f| block_at(attempt.world, below_pos.step(f)))
                        .any(|b| b.is(Class::Water) || b.is(Class::FrostedIce));
                    if !water {
                        return None;
                    }
                }
            }
            Class::Torch => s = torch(attempt, s)?,
            Class::Trapdoor => {
                s = opposite_player(s);
                if attempt.upper_half() {
                    s = props::with_flag(s, "upside_down_bit", true)?;
                }
            }
            Class::Vine => {
                let behind = block_at(attempt.world, attempt.replace.step(face.opposite()));
                if !is_full_cube(behind) || Axis::of(face) == Axis::Y {
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
                if Axis::of(face) == Axis::Y || !attempt.attached(state, face.opposite())? {
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

/// The facing of a barrel placed at `pos` by the placer `p` (PMMP `Barrel::place`): up or down
/// when the player is close and above or below, else toward the player.
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

/// The blocks of a bed (PMMP `Bed::place`): the foot here and the head one block toward the
/// player's facing, both supported from below.
fn bed(attempt: &Attempt<'_>, state: State) -> Option<Blocks> {
    let supported =
        |pos: BlockPos| adjacent_support(attempt.world, pos, Face::Down) != SupportType::None;
    if !supported(attempt.replace) {
        return None;
    }
    let facing = attempt.facing().unwrap_or(Face::North);
    let foot = props::with_facing(state, facing)?;
    let next = attempt.replace.step(facing)?;
    if !block_at(attempt.world, Some(next)).can_be_replaced() || !supported(next) {
        return None;
    }
    let head = props::with_flag(foot, "head_piece_bit", true)?;
    Some(Blocks::new((attempt.replace, foot), Some((next, head))))
}

/// The blocks of a door (PMMP `Door::place`): on top of a block only, the top half above, hinged
/// right next to another door or against a wall.
fn door(attempt: &Attempt<'_>, state: State) -> Option<Blocks> {
    if attempt.face != Face::Up {
        return None;
    }
    let up = attempt.replace.step(Face::Up)?;
    if !block_at(attempt.world, Some(up)).can_be_replaced()
        || !attempt.support(Face::Down).has_edge_support()
    {
        return None;
    }
    let mut bottom = state;
    let facing = attempt.facing().or_else(|| props::facing(state))?;
    bottom = props::with_facing(bottom, facing)?;
    let next = attempt.side(props::rotate_counterclockwise(facing));
    let next2 = attempt.side(props::rotate_clockwise(facing));
    if next.kind() == state.kind() || (!next2.is_transparent() && next.is_transparent()) {
        bottom = props::with_flag(bottom, "door_hinge_bit", true)?;
    }
    let top = props::with_flag(bottom, "upper_block_bit", true)?;
    Some(Blocks::new((attempt.replace, bottom), Some((up, top))))
}

/// The axis of a coral fan placed at `pos` by the placer `p` (PMMP `FloorCoralFan::place`;
/// Bedrock `coral_fan_direction`, X 0 and Z 1): X when the fan is more south of the player's
/// block than east or west, else Z (PMMP's `atan2` test, with its 45° edges on Z).
fn coral_fan_axis(pos: BlockPos, p: &Placer) -> i32 {
    let feet = p.feet.floor();
    let dx = f64::from(pos.x) - feet.x;
    let dz = f64::from(pos.z) - feet.z;
    i32::from(dz <= dx.abs())
}

/// The coral state: dead unless water is next to the replaced position (PMMP
/// `BaseCoral::isCoveredWithWater`).
fn coral_dead(attempt: &Attempt<'_>, state: State) -> State {
    let water = Face::ALL
        .into_iter()
        .any(|f| attempt.side(f).is(Class::Water));
    if water {
        return state;
    }
    props::dead_coral(state).unwrap_or(state)
}

/// The state with the existing faces plus the clicked side if it gives full support, else the
/// first face that does (PMMP `MultiAnySupportTrait::place`).
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
        Face::ALL.into_iter().find(|face| free(*face))?
    };
    bits |= props::multi_face_bit(placed);
    props::with_int(state, "multi_face_direction_bits", bits)
}

/// The slab state (PMMP `Slab::place`): the double slab when it merges into the existing slab's
/// empty half, else the top or bottom half.
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

/// The torch state (PMMP `Torch::place`): against the clicked side if it gives full support, else
/// the first of south, west, north, east and below that supports it.
fn torch(attempt: &Attempt<'_>, state: State) -> Option<State> {
    let face = attempt.face;
    let facing = if face != Face::Down && attempt.attached(state, face.opposite())? {
        face
    } else {
        [Face::South, Face::West, Face::North, Face::East, Face::Down]
            .into_iter()
            .find(|f| attempt.attached(state, *f) == Some(true))?
            .opposite()
    };
    props::with_facing(state, facing)
}

/// Whether the state is one collision box filling the cell (PMMP `Block::isFullCube`).
fn is_full_cube(state: State) -> bool {
    match state.collision_boxes() {
        [b] => {
            let (dx, dy, dz) = (b.max.x - b.min.x, b.max.y - b.min.y, b.max.z - b.min.z);
            dx == dy && dy == dz && dx >= 1.0
        }
        _ => false,
    }
}

/// The wall hanging sign state (PMMP `WallHangingSign::place`): hung from the block beside it
/// (or the one opposite), facing the player where it can.
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
    let direction = if supported(attach) {
        attach
    } else if supported(attach.opposite()) {
        attach.opposite()
    } else {
        return None;
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

/// The support the block next to `pos` toward `face` gives back (PMMP
/// `Block::getAdjacentSupportType`).
fn adjacent_support(world: &World, pos: BlockPos, face: Face) -> SupportType {
    block_at(world, pos.step(face)).support(face.opposite())
}

/// The block at `pos`, air where there is none (an unloaded chunk, outside the world), so the
/// neighbour rules never load a chunk. An inert block reads as its stand-in
/// ([`block::Inert::stand_in`]) and an unknown state as bedrock (solid, full and unchanged, as
/// the neighbour updates read it), neither of which a placement replaces.
pub(crate) fn block_at(world: &World, pos: Option<BlockPos>) -> State {
    match pos.and_then(|p| world.entry(p)) {
        None => block::AIR,
        Some(Entry::Known(s)) => s,
        Some(Entry::Inert(s, _)) => s.stand_in(),
        Some(Entry::Unknown(_)) => block::BEDROCK,
    }
}

/// The block at `pos` as placements see it: an inert block as its stand-in; `None` if the chunk
/// is not loaded, `pos` is outside the world height or the state is unknown.
pub(super) fn placed_on(world: &World, pos: BlockPos) -> Option<State> {
    match world.entry(pos)? {
        Entry::Known(s) => Some(s),
        Entry::Inert(s, _) => Some(s.stand_in()),
        Entry::Unknown(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::Value;

    use crate::block::PropertyValue;
    use crate::world::{Chunk, ChunkPos, Kind as WorldKind};

    /// The PMMP placement golden table (`xtask/php/place.php`).
    const GOLDEN: &str = include_str!("../../../xtask/data/pmmp/place.json");

    /// A property value of the golden table.
    fn value(v: &Value) -> PropertyValue<'_> {
        let (tag, v) = v.as_object().unwrap().iter().next().unwrap();
        match tag.as_str() {
            "byte" => PropertyValue::Byte(u8::try_from(v.as_u64().unwrap()).unwrap()),
            "int" => PropertyValue::Int(i32::try_from(v.as_i64().unwrap()).unwrap()),
            _ => PropertyValue::String(v.as_str().unwrap()),
        }
    }

    /// The golden Bedrock state's name and properties.
    fn golden_state(v: &Value) -> (&str, Vec<(&str, PropertyValue<'_>)>) {
        let name = v["name"].as_str().unwrap();
        let props = v["states"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.as_str(), value(v)))
            .collect();
        (name, props)
    }

    /// An Anchor state with the golden name and properties; properties the client version
    /// added (connections) take the state's first value.
    fn state_of(v: &Value) -> State {
        let (name, props) = golden_state(v);
        State::find(name, &props)
            .or_else(|| {
                State::all()
                    .find(|s| s.name() == name && props.iter().all(|(k, v)| s.value(k) == Some(*v)))
            })
            .unwrap_or_else(|| panic!("no state {name} {props:?}"))
    }

    /// Whether `state` has the golden name and every golden property; the random ages PMMP
    /// draws when placing vines are not compared.
    fn matches(state: State, v: &Value) -> bool {
        let (name, props) = golden_state(v);
        state.name() == name
            && props.iter().all(|(k, v)| {
                matches!(
                    *k,
                    "growing_plant_age" | "twisting_vines_age" | "weeping_vines_age"
                ) || state.value(k) == Some(*v)
            })
    }

    /// An empty world with the chunks around the origin loaded, without light (full light 15
    /// as PMMP's fake world).
    fn world() -> World {
        let mut world = World::new("place", WorldKind::Temporary, 0, BlockPos::new(0, 0, 0));
        world.set_lighting(crate::world::LightMode::Fixed(15));
        for x in -1..=1 {
            for z in -1..=1 {
                world.insert_chunk(ChunkPos::new(x, z), Chunk::new());
            }
        }
        world
    }

    fn set(world: &mut World, x: i32, y: i32, z: i32, state: State) {
        world.set_block(BlockPos::new(x, y, z), state).unwrap();
    }

    fn layer(world: &mut World, y: i32, state: State) {
        for x in -2..=2 {
            for z in -2..=2 {
                set(world, x, y, z, state);
            }
        }
    }

    /// The item type of a golden item entry.
    fn item_of(v: &Value) -> item::Type {
        let name = v["name"].as_str().unwrap();
        let meta = u16::try_from(v["meta"].as_u64().unwrap()).unwrap();
        let nbt = crate::nbt::Compound::new();
        let Some(block) = v.get("block") else {
            return item::Type::from_name(name, meta, None, &nbt)
                .unwrap_or_else(|| panic!("no item {v}"));
        };
        // The item's block is one of the states with the golden properties (the client version
        // added connection properties PMMP does not write).
        let (block_name, props) = golden_state(block);
        State::all()
            .filter(|s| s.name() == block_name && props.iter().all(|(k, v)| s.value(k) == Some(*v)))
            .find_map(|s| item::Type::from_name(name, meta, Some(s), &nbt))
            .unwrap_or_else(|| panic!("no item {v}"))
    }

    fn click(face: Face, y: f64) -> Vec3 {
        match face {
            Face::North => Vec3::new(0.5, y, 0.0),
            Face::South => Vec3::new(0.5, y, 1.0),
            Face::West => Vec3::new(0.0, y, 0.5),
            Face::East => Vec3::new(1.0, y, 0.5),
            _ => Vec3::new(0.5, y, 0.5),
        }
    }

    /// A hanging sign clicked under a block too thin for the edge form hangs from its centre
    /// (PMMP `HangingSign::getPlacementTransaction` tries the next form when one refuses).
    #[test]
    fn a_hanging_sign_under_a_fence_hangs_from_its_centre() {
        let mut w = world();
        set(&mut w, 0, 1, 0, block::Kind::OakFence.default_state());
        let feet = Vec3::new(0.5, 0.0, 2.5);
        let placer = Placer {
            feet,
            eye: feet + Vec3::new(0.0, 1.62, 0.0),
            yaw: 0.0,
            sneaking: false,
        };
        let blocks = placement(
            &w,
            item::Kind::OakHangingSign.default_type(),
            BlockPos::new(0, 1, 0),
            Face::Down,
            Vec3::new(0.5, 0.0, 0.5),
            Some(&placer),
            &mut Random::new(0, 0, 0),
        )
        .expect("the centre form places");
        let list = blocks.to_vec();
        let [(pos, state)] = list.as_slice() else {
            panic!("one block: {blocks:?}");
        };
        assert_eq!(*pos, BlockPos::new(0, 0, 0));
        assert_eq!(props::flag(*state, "hanging"), Some(true));
        assert_eq!(props::flag(*state, "attached_bit"), Some(true));
    }

    /// A red mushroom goes down where the world's light at its time of day is dim: the open sky at
    /// night, not at noon (PMMP `RedMushroom::canBeSupportedAt` reads `getFullLightAt`).
    #[test]
    fn a_red_mushroom_is_placed_under_the_night_sky_only() {
        let block::ItemForm::Item(mushroom) = Kind::RedMushroom.default_state().item() else {
            panic!("a red mushroom has an item");
        };
        let placed_at = |time_ticks: i64| {
            let mut w = world();
            w.set_time(time_ticks);
            set(&mut w, 0, 0, 0, Kind::Stone.default_state());
            placement(
                &w,
                mushroom,
                BlockPos::new(0, 0, 0),
                Face::Up,
                click(Face::Up, 1.0),
                None,
                &mut Random::new(0, 0, 0),
            )
        };
        assert!(placed_at(18_000).is_some());
        assert!(placed_at(0).is_none());
    }

    /// Every item PMMP places, in every scenario of the golden table (floor, wall, ceiling and
    /// merging, five yaws, two click heights, nine grounds), places what PMMP places.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "consistency: the scenario worlds are built next to the comparison they feed"
    )]
    fn placement_equals_pmmp_for_every_item_and_scenario() {
        let golden: Value = serde_json::from_str(GOLDEN).unwrap();
        let grounds: Vec<(String, State)> = golden["grounds"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), state_of(v)))
            .collect();
        let ground = |name: &str| grounds.iter().find(|(k, _)| k == name).unwrap().1;
        let stone = ground("stone");
        let states = golden["states"].as_array().unwrap();
        let results = golden["results"].as_array().unwrap();
        let scenarios = golden["scenarios"].as_array().unwrap();
        let mut wrong = Vec::new();
        let mut compared = 0;
        for entry in golden["items"].as_array().unwrap() {
            let item = item_of(&entry["item"]);
            let mut first: Option<Blocks> = None;
            for (i, (scenario, result)) in scenarios
                .iter()
                .zip(entry["results"].as_array().unwrap())
                .enumerate()
            {
                let layout = scenario["layout"].as_str().unwrap();
                let face =
                    Face::from_index(i32::try_from(scenario["face"].as_i64().unwrap()).unwrap())
                        .unwrap();
                let yaw = scenario["yaw"].as_f64().unwrap();
                let click_y = scenario["click_y"].as_f64().unwrap();
                let feet: Vec<f64> = scenario["feet"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_f64().unwrap())
                    .collect();
                let g = ground(scenario["ground"].as_str().unwrap());
                if layout == "merge" && first.is_none() {
                    continue;
                }
                let mut w = world();
                let clicked = match layout {
                    "floor" => {
                        layer(&mut w, 0, g);
                        BlockPos::new(0, 0, 0)
                    }
                    "wall" => {
                        layer(&mut w, -1, stone);
                        set(&mut w, 0, 0, 0, g);
                        BlockPos::new(0, 0, 0)
                    }
                    "ceiling" => {
                        layer(&mut w, -3, stone);
                        set(&mut w, 0, 0, 0, g);
                        BlockPos::new(0, 0, 0)
                    }
                    _ => {
                        layer(&mut w, 0, g);
                        for (p, s) in first.as_ref().unwrap() {
                            w.set_block(*p, *s).unwrap();
                        }
                        BlockPos::new(0, 1, 0)
                    }
                };
                let feet = Vec3::new(feet[0], feet[1], feet[2]);
                let placer = Placer {
                    feet,
                    eye: feet + Vec3::new(0.0, 1.62, 0.0),
                    yaw,
                    sneaking: false,
                };
                let mut rng = Random::new(0, 0, 0);
                let got = placement(
                    &w,
                    item,
                    clicked,
                    face,
                    click(face, click_y),
                    Some(&placer),
                    &mut rng,
                );
                if i == 0 {
                    first.clone_from(&got);
                }
                let want = &results[usize::try_from(result.as_u64().unwrap()).unwrap()];
                compared += 1;
                let same = match (&got, want.as_array()) {
                    (None, None) => true,
                    (Some(got), Some(want)) => {
                        got.iter().count() == want.len()
                            && got.iter().zip(want).all(|((p, s), w)| {
                                let w = w.as_array().unwrap();
                                let at = |k: usize| i32::try_from(w[k].as_i64().unwrap()).unwrap();
                                *p == BlockPos::new(
                                    clicked.x + at(0),
                                    clicked.y + at(1),
                                    clicked.z + at(2),
                                ) && matches(
                                    *s,
                                    &states[usize::try_from(w[3].as_u64().unwrap()).unwrap()],
                                )
                            })
                    }
                    _ => false,
                };
                if !same {
                    let want: Option<Vec<String>> = want.as_array().map(|list| {
                        list.iter()
                            .map(|w| {
                                let s = &states[usize::try_from(w[3].as_u64().unwrap()).unwrap()];
                                format!("{} {} {} {}", w[0], w[1], w[2], s)
                            })
                            .collect()
                    });
                    let got: Option<Vec<String>> = got.as_ref().map(|list| {
                        list.iter()
                            .map(|(p, s)| format!("{} {} {} {s:?}", p.x, p.y, p.z))
                            .collect()
                    });
                    wrong.push(format!(
                        "{} #{i} {layout} {:?} {face:?} yaw {yaw} y {click_y}:\n   got  {got:?}\n   want {want:?}",
                        item.name(),
                        scenario["ground"]
                    ));
                }
            }
        }
        assert!(compared > 150_000, "{compared}");
        // One failure per item, so a broken rule shows every item it breaks.
        let mut shown: Vec<&str> = Vec::new();
        let mut items = std::collections::BTreeSet::new();
        for w in &wrong {
            if items.insert(w.split(' ').next().unwrap_or_default()) {
                shown.push(w);
            }
        }
        assert!(
            wrong.is_empty(),
            "{} wrong of {compared} ({} items):\n{}",
            wrong.len(),
            shown.len(),
            shown[..shown.len().min(80)].join("\n")
        );
    }
}
