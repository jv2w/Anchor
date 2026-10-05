//! Placing blocks: which states an item puts where (PMMP `World::useItemOn`'s placement
//! transactions, `Item::getPlacementTransaction`, `Block::canBePlacedAt` and `Block::place` of
//! every class and trait, the `canBeSupportedAt` rules of `StaticSupportTrait`).
//!
//! [`placement`] only reads the world: it returns the blocks to set, which the caller checks
//! (entities in the way, adventure mode, events in M8) and then applies.

use crate::block::{self, Class, Face, Kind, State, SupportType, Trait, TypeTag};
use crate::item;
use crate::math::Vec3;
use crate::random::Random;
use crate::world::{BlockPos, Entry, MAX_Y, MIN_Y, World};

use super::props::{self, Axis};

/// The blocks a placement sets, in position order (at most a few: doors, beds, tall plants).
pub(super) type Blocks = Vec<(BlockPos, State)>;

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
    /// PMMP `Entity::getHorizontalFacing`.
    fn facing(&self) -> Face {
        props::horizontal_facing(self.yaw)
    }
}

/// The block at `pos`, air where nothing is known (an unloaded chunk, outside the world): the
/// neighbour rules of PLAN.md 10.6. An inert block reads as its stand-in
/// ([`block::Inert::stand_in`]), which no placement replaces.
pub(super) fn block_at(world: &World, pos: Option<BlockPos>) -> State {
    pos.and_then(|p| placed_on(world, p)).unwrap_or(block::AIR)
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

/// PMMP `Block::getAdjacentSupportType`: the support the block toward `face` gives back.
fn adjacent_support(world: &World, pos: BlockPos, face: Face) -> SupportType {
    block_at(world, pos.step(face)).support(face.opposite())
}

/// PMMP `Block::isFullCube`: one collision box filling the cell.
fn is_full_cube(state: State) -> bool {
    match state.collision_boxes() {
        [b] => {
            let (dx, dy, dz) = (b.max.x - b.min.x, b.max.y - b.min.y, b.max.z - b.min.z);
            dx == dy && dy == dz && dx >= 1.0
        }
        _ => false,
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
    let ctx = |replace: BlockPos, face: Face| Context {
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
    if let Some(blocks) = transaction(&ctx(clicked, Face::Up), rng) {
        return Some(blocks);
    }
    let replace = clicked.step(face)?;
    placed_on(world, replace)?;
    transaction(&ctx(replace, face), rng)
}

/// One placement attempt (PMMP `Item::getPlacementTransaction`).
struct Context<'a> {
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

impl Context<'_> {
    /// The block next to the replaced position toward `face`.
    fn side(&self, face: Face) -> State {
        block_at(self.world, self.replace.step(face))
    }

    /// The support the block toward `face` of the replaced position gives back.
    fn support(&self, face: Face) -> SupportType {
        adjacent_support(self.world, self.replace, face)
    }

    /// The placer's horizontal facing; `None` without a placer.
    fn facing(&self) -> Option<Face> {
        self.placer.map(Placer::facing)
    }
}

/// The transaction of one attempt: the item's block for the face, checked and placed.
fn transaction(ctx: &Context<'_>, rng: &mut Random) -> Option<Blocks> {
    for state in candidates(ctx) {
        if !can_be_placed_at(ctx, state) {
            continue;
        }
        let blocks = place(ctx, state, rng)?;
        if blocks.iter().any(|(p, _)| !(MIN_Y..=MAX_Y).contains(&p.y)) {
            return None;
        }
        return Some(blocks);
    }
    None
}

/// The blocks the item tries for the face, in order: one for most items; a hanging sign tries
/// its ceiling forms (PMMP `HangingSign::getPlacementTransaction`).
fn candidates(ctx: &Context<'_>) -> Vec<State> {
    let Some(state) = ctx.item.placed_block(Some(ctx.face)) else {
        return Vec::new();
    };
    if !state.has_type_tag(TypeTag::HangingSign) {
        return vec![state];
    }
    let hanging = |hanging: bool, attached: bool| {
        props::with_flag(state, "hanging", hanging)
            .and_then(|s| props::with_flag(s, "attached_bit", attached))
    };
    if ctx.face != Face::Down {
        return hanging(false, false).into_iter().collect();
    }
    let edges = ctx
        .placer
        .is_none_or(|p| !p.sneaking)
        .then(|| hanging(true, false))
        .flatten();
    edges.into_iter().chain(hanging(true, true)).collect()
}

/// PMMP `Block::canBePlacedAt` of the state's class.
fn can_be_placed_at(ctx: &Context<'_>, state: State) -> bool {
    let replace = ctx.replace_state;
    let is_clicked = ctx.replace == ctx.clicked;
    for class in state.kind().classes() {
        match class {
            Class::Candle => {
                return if replace.kind() == state.kind() {
                    props::int(replace, "candles").is_some_and(|c| c < 3)
                } else {
                    replace.can_be_replaced()
                };
            }
            Class::PinkPetals => {
                return (replace.kind() == state.kind()
                    && props::int(replace, "growth").is_some_and(|c| c < 3))
                    || (supported_at(ctx, state) && flowable_placeable(state, replace));
            }
            Class::SeaPickle => {
                return (replace.kind() == state.kind()
                    && props::int(replace, "cluster_count").is_some_and(|c| c < 3))
                    || replace.can_be_replaced();
            }
            Class::Slab => {
                if replace.can_be_replaced() {
                    return true;
                }
                if replace.kind() == state.kind() && !is_double_slab(replace) {
                    return if is_top_slab(replace) {
                        ctx.click.y <= 0.5 || (!is_clicked && ctx.face == Face::Up)
                    } else {
                        ctx.click.y >= 0.5 || (!is_clicked && ctx.face == Face::Down)
                    };
                }
                return false;
            }
            Class::WaterLily => {
                return !replace.is(Class::Water)
                    && supported_at(ctx, state)
                    && flowable_placeable(state, replace);
            }
            Class::Light => {
                return replace.can_be_replaced() && replace.kind() != state.kind();
            }
            _ if uses_static_support(*class) => {
                return supported_at(ctx, state) && flowable_placeable(state, replace);
            }
            Class::Flowable => return flowable_placeable(state, replace),
            _ => {}
        }
    }
    replace.can_be_replaced()
}

/// PMMP `Flowable::canBePlacedAt` (a block liquids destroy does not go into a liquid), or
/// `Block::canBePlacedAt` for blocks that are not flowable.
fn flowable_placeable(state: State, replace: State) -> bool {
    if state.is(Class::Flowable) && state.can_be_flowed_into() && replace.is(Class::Liquid) {
        return false;
    }
    replace.can_be_replaced()
}

/// The PMMP classes that use `StaticSupportTrait` (their `canBePlacedAt` checks support).
fn uses_static_support(class: Class) -> bool {
    matches!(
        class,
        Class::Bamboo
            | Class::BambooSapling
            | Class::BaseCake
            | Class::Azalea
            | Class::Cactus
            | Class::CactusFlower
            | Class::Carpet
            | Class::CaveVines
            | Class::CeilingCenterHangingSign
            | Class::ChorusFlower
            | Class::ChorusPlant
            | Class::DeadBush
            | Class::Crops
            | Class::Coral
            | Class::FlowerPot
            | Class::Flower
            | Class::FloorCoralFan
            | Class::HangingRoots
            | Class::NetherFungus
            | Class::NetherVines
            | Class::PressurePlate
            | Class::NetherSprouts
            | Class::NetherWartPlant
            | Class::RedstoneRepeater
            | Class::RedstoneWire
            | Class::RedstoneComparator
            | Class::PitcherCrop
            | Class::NetherRoots
            | Class::Sapling
            | Class::SporeBlossom
            | Class::Sugarcane
            | Class::TorchflowerCrop
            | Class::TallGrass
            | Class::SweetBerryBush
            | Class::WitherRose
    )
}

/// PMMP `canBeSupportedAt` of the state's class, at the replaced position.
fn supported_at(ctx: &Context<'_>, state: State) -> bool {
    let below = ctx.side(Face::Down);
    let soil = |s: State| s.has_type_tag(TypeTag::Dirt) || s.has_type_tag(TypeTag::Mud);
    for class in state.kind().classes() {
        let result = match class {
            Class::Azalea => below.kind() == Kind::Clay || soil(below),
            Class::Bamboo => {
                below.kind() == state.kind()
                    || below.kind() == Kind::Gravel
                    || soil(below)
                    || below.has_type_tag(TypeTag::Sand)
            }
            Class::BambooSapling => {
                below.kind() == Kind::Gravel || soil(below) || below.has_type_tag(TypeTag::Sand)
            }
            Class::BaseCake | Class::Carpet => below != block::AIR,
            Class::Cactus => {
                (below.kind() == state.kind() || below.has_type_tag(TypeTag::Sand))
                    && Face::HORIZONTAL
                        .into_iter()
                        .all(|f| !ctx.side(f).is_solid())
            }
            Class::CactusFlower => {
                below.support(Face::Up).has_center_support() || below.kind() == Kind::Cactus
            }
            Class::CaveVines => {
                let up = ctx.side(Face::Up);
                up.support(Face::Down) == SupportType::Full || up.kind() == state.kind()
            }
            Class::CeilingCenterHangingSign => {
                let up = ctx.side(Face::Up);
                up.support(Face::Down).has_center_support() || up.has_type_tag(TypeTag::HangingSign)
            }
            Class::ChorusFlower => chorus_flower_supported(ctx, below),
            Class::ChorusPlant => chorus_plant_supported(ctx, state, below),
            Class::Coral | Class::FloorCoralFan | Class::FlowerPot | Class::RedstoneWire => {
                ctx.support(Face::Down).has_center_support()
            }
            Class::Crops | Class::PitcherCrop | Class::TorchflowerCrop => {
                below.kind() == Kind::Farmland
            }
            Class::DeadBush => {
                below.has_type_tag(TypeTag::Sand)
                    || below.has_type_tag(TypeTag::Mud)
                    || matches!(
                        below.kind(),
                        Kind::Podzol
                            | Kind::Mycelium
                            | Kind::Dirt
                            | Kind::Grass
                            | Kind::HardenedClay
                            | Kind::StainedClay
                    )
            }
            Class::Flower | Class::Sapling | Class::TallGrass | Class::PinkPetals => soil(below),
            Class::HangingRoots => ctx.support(Face::Up).has_center_support(),
            Class::NetherFungus | Class::NetherRoots | Class::NetherSprouts => {
                soil(below) || below.has_type_tag(TypeTag::Nylium) || below.kind() == Kind::SoulSoil
            }
            Class::NetherVines => {
                // Twisting vines grow up, weeping vines down.
                let growth = if state.kind() == Kind::WeepingVines {
                    Face::Down
                } else {
                    Face::Up
                };
                let support = ctx.side(growth.opposite());
                support.support(growth).has_center_support() || support.kind() == state.kind()
            }
            Class::NetherWartPlant => below.kind() == Kind::SoulSand,
            Class::PressurePlate | Class::RedstoneComparator | Class::RedstoneRepeater => {
                ctx.support(Face::Down) != SupportType::None
            }
            Class::SporeBlossom => ctx.support(Face::Up) == SupportType::Full,
            Class::Sugarcane => {
                below.kind() == state.kind()
                    || below.has_type_tag(TypeTag::Mud)
                    || below.has_type_tag(TypeTag::Dirt)
                    || below.has_type_tag(TypeTag::Sand)
            }
            Class::SweetBerryBush => below.kind() != Kind::Farmland && soil(below),
            Class::WaterLily => below.is(Class::Water),
            Class::WitherRose => {
                soil(below)
                    || matches!(
                        below.kind(),
                        Kind::Netherrack | Kind::SoulSand | Kind::SoulSoil
                    )
            }
            _ => continue,
        };
        return result;
    }
    true
}

/// PMMP `ChorusFlower::canBeSupportedAt`.
fn chorus_flower_supported(ctx: &Context<'_>, below: State) -> bool {
    if matches!(below.kind(), Kind::EndStone | Kind::ChorusPlant) {
        return true;
    }
    let mut plant = false;
    for f in Face::HORIZONTAL {
        let side = ctx.side(f);
        if side.kind() == Kind::ChorusPlant {
            if plant {
                return false;
            }
            plant = true;
        } else if side != block::AIR {
            return false;
        }
    }
    plant
}

/// PMMP `ChorusPlant::canBeSupportedAt`.
fn chorus_plant_supported(ctx: &Context<'_>, state: State, below: State) -> bool {
    let by = |s: State| s.kind() == state.kind() || s.kind() == Kind::EndStone;
    let vertical_air = below == block::AIR || ctx.side(Face::Up) == block::AIR;
    for f in Face::HORIZONTAL {
        let side_pos = ctx.replace.step(f);
        if block_at(ctx.world, side_pos).kind() == Kind::ChorusPlant {
            if !vertical_air {
                return false;
            }
            if by(block_at(
                ctx.world,
                side_pos.and_then(|p| p.step(Face::Down)),
            )) {
                return true;
            }
        }
    }
    by(below)
}

/// Whether a slab state is a double slab.
fn is_double_slab(state: State) -> bool {
    state.name().contains("double")
}

/// Whether a single slab is in the top half.
fn is_top_slab(state: State) -> bool {
    props::string(state, "minecraft:vertical_half") == Some("top")
}

/// The double form of a slab state (its `*_double_slab` name, same half).
fn double_slab(state: State) -> Option<State> {
    let name = state.name();
    let double = DOUBLE_SLABS
        .iter()
        .find(|(single, _)| *single == name)
        .map(|(_, d)| *d)?;
    props::renamed(state, double)
}

/// Every single slab name with its double form, where the double is not `<name>` with
/// `_slab` replaced by `_double_slab` (Bedrock's copper slabs put `double` in front).
const DOUBLE_SLABS: [(&str, &str); 8] = [
    (
        "minecraft:cut_copper_slab",
        "minecraft:double_cut_copper_slab",
    ),
    (
        "minecraft:exposed_cut_copper_slab",
        "minecraft:exposed_double_cut_copper_slab",
    ),
    (
        "minecraft:weathered_cut_copper_slab",
        "minecraft:weathered_double_cut_copper_slab",
    ),
    (
        "minecraft:oxidized_cut_copper_slab",
        "minecraft:oxidized_double_cut_copper_slab",
    ),
    (
        "minecraft:waxed_cut_copper_slab",
        "minecraft:waxed_double_cut_copper_slab",
    ),
    (
        "minecraft:waxed_exposed_cut_copper_slab",
        "minecraft:waxed_exposed_double_cut_copper_slab",
    ),
    (
        "minecraft:waxed_weathered_cut_copper_slab",
        "minecraft:waxed_weathered_double_cut_copper_slab",
    ),
    (
        "minecraft:waxed_oxidized_cut_copper_slab",
        "minecraft:waxed_oxidized_double_cut_copper_slab",
    ),
];

/// The double form of a single slab: the copper exceptions, else `*_double_slab`.
fn slab_doubled(state: State) -> Option<State> {
    if let Some(d) = double_slab(state) {
        return Some(d);
    }
    let base = state.name().strip_suffix("_slab")?;
    let mut name = String::with_capacity(base.len() + 12);
    name.push_str(base);
    name.push_str("_double_slab");
    props::renamed(state, &name)
}

/// PMMP `Block::place` of the state's class, with the traits that place: the blocks to set.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one arm per PMMP class keeps the port side by side with PMMP"
)]
fn place(ctx: &Context<'_>, state: State, rng: &mut Random) -> Option<Blocks> {
    let mut s = state;
    let mut extra: Blocks = Vec::new();
    let face = ctx.face;
    let set_facing = |s: State, f: Face| props::with_facing(s, f).unwrap_or(s);
    let opposite_player = |s: State| ctx.facing().map_or(s, |f| set_facing(s, f.opposite()));
    for class in state.kind().classes() {
        match class {
            Class::AmethystCluster => {
                if ctx.support(face.opposite()) != SupportType::Full {
                    return None;
                }
                s = set_facing(s, face);
            }
            Class::Anvil => {
                if let Some(f) = ctx.facing() {
                    s = set_facing(s, props::rotate_y(f, false));
                }
            }
            Class::Barrel => {
                if let Some(p) = ctx.placer {
                    s = set_facing(s, barrel_facing(ctx.replace, p));
                }
            }
            Class::FloorBanner | Class::OminousFloorBanner | Class::FloorSign => {
                if face != Face::Up {
                    return None;
                }
                if let Some(p) = ctx.placer {
                    s = props::with_int(s, props::ROTATION, props::rotation_from_yaw(p.yaw))
                        .unwrap_or(s);
                }
            }
            Class::WallBanner
            | Class::OminousWallBanner
            | Class::WallSign
            | Class::TripwireHook => {
                if Axis::of(face) == Axis::Y {
                    return None;
                }
                s = set_facing(s, face);
            }
            Class::BaseBanner | Class::BaseOminousBanner => {
                // The supporting face: down for a floor banner, behind a wall banner.
                let behind = props::facing(s).map_or(Face::Down, Face::opposite);
                let support = if s.is(Class::FloorBanner) || s.is(Class::OminousFloorBanner) {
                    Face::Down
                } else {
                    behind
                };
                if !ctx.side(support).is_solid() {
                    return None;
                }
            }
            Class::BaseBigDripleaf => {
                let below_pos = ctx.replace.step(Face::Down)?;
                let below = block_at(ctx.world, Some(below_pos));
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
                    extra.push((
                        below_pos,
                        set_facing(Kind::BigDripleafStem.default_state(), f),
                    ));
                }
            }
            Class::BaseRail => {
                if !ctx.support(Face::Down).has_edge_support() {
                    return None;
                }
            }
            Class::Bed => return bed(ctx, s),
            Class::Bell => {
                if ctx.support(face.opposite()) == SupportType::None {
                    return None;
                }
                let attachment = match face {
                    Face::Up => {
                        s = opposite_player(s);
                        "standing"
                    }
                    Face::Down => "hanging",
                    _ => {
                        s = set_facing(s, face);
                        if ctx.support(face) == SupportType::None {
                            "side"
                        } else {
                            "multiple"
                        }
                    }
                };
                s = props::with_string(s, "attachment", attachment)?;
            }
            Class::Button => {
                if !ctx.support(face.opposite()).has_center_support() {
                    return None;
                }
                s = set_facing(s, face);
            }
            Class::Campfire => {
                if ctx.side(Face::Down).is(Class::Campfire) {
                    return None;
                }
                if let Some(f) = ctx.facing() {
                    s = set_facing(s, f);
                }
                s = props::with_flag(s, "extinguished", false)?;
            }
            Class::Candle => {
                if !ctx.support(Face::Down).has_center_support() {
                    return None;
                }
                let existing = ctx.replace_state;
                if existing.kind() == s.kind() {
                    let count = props::int(existing, "candles")?;
                    if count >= 3 {
                        return None;
                    }
                    s = props::with_int(s, "candles", count + 1)?;
                    s = props::with_flag(s, "lit", props::flag(existing, "lit")?)?;
                }
            }
            Class::CaveVines => {
                // PMMP draws the age that limits how far the vine grows (MAX_AGE 25).
                s = props::with_int(s, "growing_plant_age", rng.range(0, 25))?;
            }
            Class::NetherVines => {
                let name = if s.kind() == Kind::WeepingVines {
                    "weeping_vines_age"
                } else {
                    "twisting_vines_age"
                };
                s = props::with_int(s, name, rng.range(0, 24))?;
            }
            Class::CeilingCenterHangingSign => {
                if face != Face::Down {
                    return None;
                }
                if let Some(p) = ctx.placer {
                    s = props::with_int(s, props::ROTATION, props::rotation_from_yaw(p.yaw))
                        .unwrap_or(s);
                }
            }
            Class::CeilingEdgesHangingSign => {
                if face != Face::Down {
                    return None;
                }
                s = opposite_player(s);
                let up = ctx.side(Face::Up);
                let along = |other: State| {
                    (other.is(Class::WallHangingSign) || other.is(Class::CeilingEdgesHangingSign))
                        && props::facing(other).map(Axis::of) == props::facing(s).map(Axis::of)
                };
                if up.support(Face::Down) != SupportType::Full && !along(up) {
                    return None;
                }
            }
            Class::CocoaBlock => {
                let jungle = ctx.clicked_state.is(Class::Wood)
                    && matches!(ctx.clicked_state.kind(), Kind::JungleLog | Kind::JungleWood);
                if Axis::of(face) == Axis::Y || !jungle {
                    return None;
                }
                s = set_facing(s, face);
            }
            Class::Door => return door(ctx, s),
            Class::DoublePlant => {
                let up = ctx.replace.step(Face::Up)?;
                if !ctx.side(Face::Down).has_type_tag(TypeTag::Dirt)
                    || !block_at(ctx.world, Some(up)).can_be_replaced()
                {
                    return None;
                }
                let top = props::with_flag(s, "upper_block_bit", true)?;
                return Some(sorted(vec![(ctx.replace, s), (up, top)]));
            }
            Class::EndRod => {
                let mut f = face;
                if ctx.clicked_state.is(Class::EndRod)
                    && props::facing(ctx.clicked_state) == Some(f)
                {
                    f = f.opposite();
                }
                s = set_facing(s, f);
            }
            Class::FenceGate => {
                if let Some(f) = ctx.facing() {
                    s = set_facing(s, f);
                }
            }
            Class::FloorCoralFan => {
                if let Some(p) = ctx.placer {
                    s = props::with_int(s, "coral_fan_direction", coral_fan_axis(ctx.replace, p))
                        .unwrap_or(s);
                }
                s = coral_dead(ctx, s);
            }
            Class::WallCoralFan => {
                if Axis::of(face) == Axis::Y || !ctx.support(face.opposite()).has_center_support() {
                    return None;
                }
                s = set_facing(s, face);
                s = coral_dead(ctx, s);
            }
            Class::Hopper => {
                let f = if face == Face::Down {
                    Face::Down
                } else {
                    face.opposite()
                };
                s = set_facing(s, f);
            }
            Class::ItemFrame => {
                if ctx.support(face.opposite()) == SupportType::None {
                    return None;
                }
                s = set_facing(s, face);
            }
            Class::Ladder => {
                if ctx.support(face.opposite()) != SupportType::Full || Axis::of(face) == Axis::Y {
                    return None;
                }
                s = set_facing(s, face);
            }
            Class::Lantern => {
                let down = ctx.support(Face::Down).has_center_support();
                if !down && !ctx.support(Face::Up).has_center_support() {
                    return None;
                }
                s = props::with_flag(s, "hanging", face == Face::Down || !down)?;
            }
            Class::Leaves => s = props::with_flag(s, "persistent_bit", true)?,
            Class::Lever => {
                if !ctx.support(face.opposite()).has_center_support() {
                    return None;
                }
                let x = ctx.facing().is_none_or(|f| Axis::of(f) == Axis::X);
                let direction = match face {
                    Face::Down if x => "down_east_west",
                    Face::Down => "down_north_south",
                    Face::Up if x => "up_east_west",
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
                let existing = ctx.replace_state;
                if existing.kind() == s.kind()
                    && let Some(count) = props::int(existing, "growth").filter(|c| *c < 3)
                {
                    s = props::with_int(s, "growth", count + 1)?;
                    s = set_facing(s, props::facing(existing)?);
                } else {
                    s = opposite_player(s);
                }
            }
            Class::RedMushroom => {
                let below = ctx.side(Face::Down);
                let light = ctx.world.full_light(ctx.replace, 0).unwrap_or(15);
                if !((light <= 12 && !below.is_transparent())
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
                let existing = ctx.replace_state;
                if existing.kind() == s.kind()
                    && let Some(count) = props::int(existing, "cluster_count").filter(|c| *c < 3)
                {
                    s = props::with_int(s, "cluster_count", count + 1)?;
                }
            }
            Class::Slab => s = slab(ctx, s)?,
            Class::SmallDripleaf => {
                let up = ctx.replace.step(Face::Up)?;
                if block_at(ctx.world, Some(up)) != block::AIR
                    || ctx.side(Face::Down).kind() != Kind::Clay
                {
                    return None;
                }
                s = opposite_player(s);
                let top = props::with_flag(s, "upper_block_bit", true)?;
                extra.push((up, top));
            }
            Class::SnowLayer => {
                let existing = ctx.replace_state;
                if existing.kind() == s.kind() {
                    let height = props::int(existing, "height")?;
                    if height >= 7 {
                        return None;
                    }
                    s = props::with_int(s, "height", height + 1)?;
                }
                if ctx.support(Face::Down) != SupportType::Full {
                    return None;
                }
            }
            Class::Stair => {
                if let Some(f) = ctx.facing() {
                    s = set_facing(s, f);
                }
                let upside_down = (ctx.click.y > 0.5 && face != Face::Up) || face == Face::Down;
                s = props::with_flag(s, "upside_down_bit", upside_down)?;
            }
            Class::Sugarcane => {
                let below_pos = ctx.replace.step(Face::Down)?;
                let below = block_at(ctx.world, Some(below_pos));
                if below.kind() != s.kind() {
                    let water = Face::HORIZONTAL
                        .into_iter()
                        .map(|f| block_at(ctx.world, below_pos.step(f)))
                        .any(|b| b.is(Class::Water) || b.is(Class::FrostedIce));
                    if !water {
                        return None;
                    }
                }
            }
            Class::Torch => s = torch(ctx, s)?,
            Class::Trapdoor => {
                s = opposite_player(s);
                if (ctx.click.y > 0.5 && face != Face::Up) || face == Face::Down {
                    s = props::with_flag(s, "upside_down_bit", true)?;
                }
            }
            Class::Vine => {
                let behind = block_at(ctx.world, ctx.replace.step(face.opposite()));
                if !is_full_cube(behind) || Axis::of(face) == Axis::Y {
                    return None;
                }
                let existing = ctx.replace_state;
                let bits = if existing.kind() == s.kind() {
                    props::int(existing, "vine_direction_bits")?
                } else {
                    0
                };
                s = props::with_int(s, "vine_direction_bits", bits | vine_bit(face.opposite()))?;
            }
            Class::WallHangingSign => s = wall_hanging_sign(ctx, s)?,
            Class::GlowLichen | Class::ResinClump => s = multi_support(ctx, s)?,
            Class::Block => {
                if s.has_trait(Trait::FacesOppositePlacingPlayer) {
                    s = opposite_player(s);
                }
                if s.has_trait(Trait::PillarRotation) {
                    s = props::with_axis(s, Axis::of(face)).unwrap_or(s);
                }
                extra.push((ctx.replace, s));
                return Some(sorted(extra));
            }
            _ => {}
        }
    }
    extra.push((ctx.replace, s));
    Some(sorted(extra))
}

/// The blocks in position order.
fn sorted(mut blocks: Blocks) -> Blocks {
    blocks.sort_by_key(|(p, _)| *p);
    blocks
}

/// PMMP `Barrel::place`: up or down when the player is close and above or below, else toward
/// the player.
fn barrel_facing(pos: BlockPos, p: &Placer) -> Face {
    let near =
        (p.feet.x - f64::from(pos.x)).abs() < 2.0 && (p.feet.z - f64::from(pos.z)).abs() < 2.0;
    if near {
        let y = p.eye.y;
        if y - f64::from(pos.y) > 2.0 {
            return Face::Up;
        }
        if f64::from(pos.y) - y > 0.0 {
            return Face::Down;
        }
    }
    p.facing().opposite()
}

/// PMMP `FloorCoralFan::place`'s axis (Bedrock `coral_fan_direction`, X 0 and Z 1): X when the
/// fan is more south of the player's block than east or west, else Z (PMMP's `atan2` test,
/// with its 45° edges on Z).
fn coral_fan_axis(pos: BlockPos, p: &Placer) -> i32 {
    let feet = p.feet.floor();
    let dx = f64::from(pos.x) - feet.x;
    let dz = f64::from(pos.z) - feet.z;
    i32::from(dz <= dx.abs())
}

/// PMMP `BaseCoral::isCoveredWithWater`: dead unless water is next to the replaced position.
fn coral_dead(ctx: &Context<'_>, state: State) -> State {
    let water = Face::ALL.into_iter().any(|f| ctx.side(f).is(Class::Water));
    if water {
        return state;
    }
    let name = state.name();
    if name.contains(":dead_") {
        return state;
    }
    name.strip_prefix("minecraft:")
        .and_then(|rest| {
            let mut dead = String::with_capacity(name.len() + 5);
            dead.push_str("minecraft:dead_");
            dead.push_str(rest);
            props::renamed(state, &dead)
        })
        .unwrap_or(state)
}

/// The `vine_direction_bits` bit of a face (PMMP `BlockLegacyMetadata::VINE_FLAG_*`).
fn vine_bit(face: Face) -> i32 {
    match face {
        Face::South => 1,
        Face::West => 2,
        Face::North => 4,
        Face::East => 8,
        Face::Up | Face::Down => 0,
    }
}

/// The `multi_face_direction_bits` bit of a face (PMMP `MULTI_FACE_DIRECTION_FLAG_*`).
fn multi_face_bit(face: Face) -> i32 {
    match face {
        Face::Down => 0x01,
        Face::Up => 0x02,
        Face::South => 0x04,
        Face::West => 0x08,
        Face::North => 0x10,
        Face::East => 0x20,
    }
}

/// PMMP `MultiAnySupportTrait::place`: the existing faces, plus the clicked side if it gives full
/// support, else the first face that does.
fn multi_support(ctx: &Context<'_>, state: State) -> Option<State> {
    let existing = ctx.replace_state;
    let mut bits = if existing.kind() == state.kind() {
        props::int(existing, "multi_face_direction_bits")?
    } else {
        0
    };
    let available: Vec<Face> = Face::ALL
        .into_iter()
        .filter(|f| bits & multi_face_bit(*f) == 0 && ctx.support(*f) == SupportType::Full)
        .collect();
    let placed = if available.contains(&ctx.face.opposite()) {
        ctx.face.opposite()
    } else {
        *available.first()?
    };
    bits |= multi_face_bit(placed);
    props::with_int(state, "multi_face_direction_bits", bits)
}

/// PMMP `Slab::place`: merges into the existing slab's empty half, else the top or bottom half.
fn slab(ctx: &Context<'_>, state: State) -> Option<State> {
    let existing = ctx.replace_state;
    let face = ctx.face;
    if existing.kind() == state.kind() && !is_double_slab(existing) {
        let top = is_top_slab(existing);
        if (top && (ctx.click.y <= 0.5 || face == Face::Up))
            || (!top && (ctx.click.y >= 0.5 || face == Face::Down))
        {
            return slab_doubled(state);
        }
    }
    let top = (face != Face::Up && ctx.click.y > 0.5) || face == Face::Down;
    props::with_string(
        state,
        "minecraft:vertical_half",
        if top { "top" } else { "bottom" },
    )
}

/// PMMP `Torch::place`: against the clicked side if it gives full support, else the first of
/// south, west, north, east and below that supports it.
fn torch(ctx: &Context<'_>, state: State) -> Option<State> {
    let supported = |f: Face| {
        if f == Face::Down {
            ctx.support(f).has_center_support()
        } else {
            ctx.support(f) == SupportType::Full
        }
    };
    let face = ctx.face;
    let facing = if face != Face::Down && supported(face.opposite()) {
        face
    } else {
        [Face::South, Face::West, Face::North, Face::East, Face::Down]
            .into_iter()
            .find(|f| supported(*f))?
            .opposite()
    };
    props::with_facing(state, facing)
}

/// PMMP `Bed::place`: the foot here and the head one block toward the player's facing, both
/// supported from below.
fn bed(ctx: &Context<'_>, state: State) -> Option<Blocks> {
    let supported =
        |pos: BlockPos| adjacent_support(ctx.world, pos, Face::Down) != SupportType::None;
    if !supported(ctx.replace) {
        return None;
    }
    let facing = ctx.facing().unwrap_or(Face::North);
    let foot = props::with_facing(state, facing)?;
    let next = ctx.replace.step(facing)?;
    if !block_at(ctx.world, Some(next)).can_be_replaced() || !supported(next) {
        return None;
    }
    let head = props::with_flag(foot, "head_piece_bit", true)?;
    Some(sorted(vec![(ctx.replace, foot), (next, head)]))
}

/// PMMP `Door::place`: on top of a block only, the top half above, hinged right next to another
/// door or against a wall.
fn door(ctx: &Context<'_>, state: State) -> Option<Blocks> {
    if ctx.face != Face::Up {
        return None;
    }
    let up = ctx.replace.step(Face::Up)?;
    if !block_at(ctx.world, Some(up)).can_be_replaced()
        || !ctx.support(Face::Down).has_edge_support()
    {
        return None;
    }
    let mut bottom = state;
    let facing = ctx.facing().or_else(|| props::facing(state))?;
    bottom = props::with_facing(bottom, facing)?;
    let next = ctx.side(props::rotate_y(facing, false));
    let next2 = ctx.side(props::rotate_y(facing, true));
    if next.kind() == state.kind() || (!next2.is_transparent() && next.is_transparent()) {
        bottom = props::with_flag(bottom, "door_hinge_bit", true)?;
    }
    let top = props::with_flag(bottom, "upper_block_bit", true)?;
    Some(sorted(vec![(ctx.replace, bottom), (up, top)]))
}

/// PMMP `WallHangingSign::place`: hung from the block beside it (or the one opposite), facing
/// the player where it can.
fn wall_hanging_sign(ctx: &Context<'_>, state: State) -> Option<State> {
    let player = ctx.facing()?;
    let attach = if Axis::of(ctx.face) == Axis::Y {
        props::rotate_y(player, true)
    } else {
        ctx.face
    };
    let supported = |f: Face| {
        let b = ctx.side(f);
        (b.is(Class::WallHangingSign)
            && props::facing(b).map(|bf| Axis::of(props::rotate_y(bf, true))) == Some(Axis::of(f)))
            || b.support(f.opposite()) == SupportType::Full
    };
    let direction = if supported(attach) {
        attach
    } else if supported(attach.opposite()) {
        attach.opposite()
    } else {
        return None;
    };
    let mut facing = props::rotate_y(direction.opposite(), true);
    if facing == player {
        facing = facing.opposite();
    }
    props::with_facing(state, facing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::PropertyValue;
    use crate::world::{Chunk, ChunkPos, Kind as WorldKind};
    use serde_json::Value;

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
                        got.len() == want.len()
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
