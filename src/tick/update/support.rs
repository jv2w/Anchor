//! Blocks that need support and blocks that fall (PMMP `onNearbyBlockChange` of
//! `StaticSupportTrait`, `MultiAnySupportTrait`, `FallableTrait` and the attached blocks): when a
//! neighbour changes, a block that lost its support is destroyed with its drops, and a block
//! with nothing under it falls.

use super::props::{self, HORIZONTAL};
use super::{Cx, plant};
use crate::block::{self, Class, Face, Kind, SupportType, Trait, TypeTag};
use crate::world::BlockPos;

/// The support the block on the `face` side of `pos` gives toward `pos` (PMMP
/// `getAdjacentSupportType`), or `None` if it is not loaded.
pub(super) fn adjacent(cx: &Cx<'_>, pos: BlockPos, face: Face) -> Option<SupportType> {
    Some(cx.get(pos.step(face)?)?.support(face.opposite()))
}

/// The block on the `face` side of `pos`, or `None` if it is not loaded.
pub(super) fn side(cx: &Cx<'_>, pos: BlockPos, face: Face) -> Option<block::State> {
    cx.get(pos.step(face)?)
}

/// Whether the block has the dirt or mud tag (PMMP `BlockTypeTags::DIRT`, `MUD`).
pub(super) fn is_soil(s: block::State) -> bool {
    s.has_type_tag(TypeTag::Dirt) || s.has_type_tag(TypeTag::Mud)
}

/// A neighbour of the block changed: destroys or drops it if it lost its support.
pub(super) fn on_nearby(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if block.has_trait(Trait::Fallable) {
        fall_check(cx, pos, block);
        return;
    }
    match block.class() {
        Class::Vine => vine(cx, pos, block),
        Class::GlowLichen | Class::ResinClump => multi_face(cx, pos, block),
        _ => {
            if supported(cx, pos, block) == Some(false) {
                cx.destroy(pos);
            }
        }
    }
}

/// Whether the block still has what holds it, for the classes that break without it; `None`
/// for other classes or when a block the check needs is not loaded.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one table of PMMP's support rules, a short arm per class"
)]
pub(super) fn supported(cx: &Cx<'_>, pos: BlockPos, block: block::State) -> Option<bool> {
    let down = || side(cx, pos, Face::Down);
    let up = || side(cx, pos, Face::Up);
    Some(match block.class() {
        // Plants on soil (PMMP `canBeSupportedAt` of each class).
        Class::Flower | Class::TallGrass | Class::Sapling | Class::PinkPetals => is_soil(down()?),
        Class::DeadBush => {
            let d = down()?;
            d.has_type_tag(TypeTag::Sand)
                || d.has_type_tag(TypeTag::Mud)
                || matches!(
                    d.kind(),
                    Kind::Podzol
                        | Kind::Mycelium
                        | Kind::Dirt
                        | Kind::Grass
                        | Kind::HardenedClay
                        | Kind::StainedClay
                )
        }
        Class::WitherRose => {
            let d = down()?;
            is_soil(d) || matches!(d.kind(), Kind::Netherrack | Kind::SoulSand | Kind::SoulSoil)
        }
        Class::Azalea => {
            let d = down()?;
            d.kind() == Kind::Clay || is_soil(d)
        }
        Class::NetherFungus | Class::NetherRoots | Class::NetherSprouts => {
            let d = down()?;
            is_soil(d) || d.has_type_tag(TypeTag::Nylium) || d.kind() == Kind::SoulSoil
        }
        Class::SweetBerryBush => {
            let d = down()?;
            d.kind() != Kind::Farmland && is_soil(d)
        }
        Class::Wheat
        | Class::Carrot
        | Class::Potato
        | Class::Beetroot
        | Class::MelonStem
        | Class::PumpkinStem
        | Class::PitcherCrop
        | Class::TorchflowerCrop => down()?.kind() == Kind::Farmland,
        Class::NetherWartPlant => down()?.kind() == Kind::SoulSand,
        Class::Bamboo => {
            let d = down()?;
            d.kind() == block.kind()
                || d.kind() == Kind::Gravel
                || is_soil(d)
                || d.has_type_tag(TypeTag::Sand)
        }
        Class::BambooSapling => {
            let d = down()?;
            d.kind() == Kind::Gravel || is_soil(d) || d.has_type_tag(TypeTag::Sand)
        }
        Class::Cactus => {
            let d = down()?;
            if d.kind() != block.kind() && !d.has_type_tag(TypeTag::Sand) {
                return Some(false);
            }
            for face in HORIZONTAL {
                if side(cx, pos, face)?.is_solid() {
                    return Some(false);
                }
            }
            true
        }
        Class::CactusFlower => {
            let d = down()?;
            d.support(Face::Up).has_center_support() || d.kind() == Kind::Cactus
        }
        Class::CaveVines => {
            let u = up()?;
            u.support(Face::Down) == SupportType::Full || u.kind() == block.kind()
        }
        Class::NetherVines => {
            let growth = plant::vine_growth(block);
            let support = side(cx, pos, growth.opposite())?;
            support.support(growth).has_center_support() || support.kind() == block.kind()
        }
        Class::ChorusFlower => chorus_flower(cx, pos)?,
        Class::ChorusPlant => chorus_plant(cx, pos)?,
        Class::WaterLily => down()?.kind() == Kind::Water,
        Class::RedMushroom | Class::BrownMushroom => !down()?.is_transparent(),
        Class::Sugarcane => plant::sugarcane_supported(cx, pos, block)?,
        Class::CocoaBlock => plant::cocoa_attached(cx, pos, block)?,
        // Blocks on a face below or above.
        Class::Coral | Class::FloorCoralFan | Class::FlowerPot | Class::RedstoneWire => {
            adjacent(cx, pos, Face::Down)?.has_center_support()
        }
        Class::HangingRoots => adjacent(cx, pos, Face::Up)?.has_center_support(),
        Class::SporeBlossom => adjacent(cx, pos, Face::Up)? == SupportType::Full,
        Class::Cake | Class::CakeWithCandle | Class::CakeWithDyedCandle | Class::Carpet => {
            down()?.kind() != Kind::Air
        }
        Class::StonePressurePlate
        | Class::WoodenPressurePlate
        | Class::WeightedPressurePlateLight
        | Class::WeightedPressurePlateHeavy
        | Class::RedstoneComparator
        | Class::RedstoneRepeater => adjacent(cx, pos, Face::Down)? != SupportType::None,
        Class::CeilingCenterHangingSign => {
            let u = up()?;
            u.support(Face::Down).has_center_support() || u.has_type_tag(TypeTag::HangingSign)
        }
        Class::CeilingEdgesHangingSign => {
            let u = up()?;
            let facing = props::facing_direction(block)?;
            u.support(Face::Down) == SupportType::Full
                || (matches!(
                    u.class(),
                    Class::WallHangingSign | Class::CeilingEdgesHangingSign
                ) && props::facing_direction(u).is_some_and(|f| props::same_axis(f, facing)))
        }
        Class::FloorSign => down()?.kind() != Kind::Air,
        Class::WallSign => {
            let face = props::facing_direction(block)?.opposite();
            side(cx, pos, face)?.kind() != Kind::Air
        }
        Class::FloorBanner | Class::OminousFloorBanner => down()?.is_solid(),
        Class::WallBanner | Class::OminousWallBanner => {
            let face = props::facing_direction(block)?.opposite();
            side(cx, pos, face)?.is_solid()
        }
        // Attached blocks.
        Class::Torch | Class::RedstoneTorch | Class::UnderwaterTorch => {
            let face = props::torch_facing(block)?.opposite();
            let support = adjacent(cx, pos, face)?;
            if face == Face::Down {
                support.has_center_support()
            } else {
                support == SupportType::Full
            }
        }
        Class::Ladder => {
            let face = props::facing_direction(block)?.opposite();
            adjacent(cx, pos, face)? == SupportType::Full
        }
        Class::Lever => {
            let face = props::lever_facing(block)?.opposite();
            adjacent(cx, pos, face)?.has_center_support()
        }
        Class::StoneButton | Class::WoodenButton => {
            let face = props::facing_direction(block)?.opposite();
            adjacent(cx, pos, face)?.has_center_support()
        }
        Class::ItemFrame => {
            let face = props::facing_direction(block)?.opposite();
            adjacent(cx, pos, face)? != SupportType::None
        }
        Class::AmethystCluster => {
            let face = props::face_name(props::text(block, "minecraft:block_face")?)?.opposite();
            adjacent(cx, pos, face)? == SupportType::Full
        }
        Class::WallCoralFan => {
            let face = props::coral_facing(block)?.opposite();
            adjacent(cx, pos, face)?.has_center_support()
        }
        Class::Lantern | Class::CopperLantern => {
            let face = if props::flag(block, "hanging")? {
                Face::Up
            } else {
                Face::Down
            };
            adjacent(cx, pos, face)?.has_center_support()
        }
        // A grindstone breaks when the block it stands on, hangs from or leans on goes (BDS P6,
        // m9-facts STATIONS); a block without collision does not hold it (dragonfly
        // `Grindstone.NeighbourUpdateTick`).
        Class::Grindstone => {
            let face = match props::text(block, "attachment")? {
                "standing" => Face::Down,
                "hanging" => Face::Up,
                _ => props::direction_swne(block)?.opposite(),
            };
            !side(cx, pos, face)?.collision_boxes().is_empty()
        }
        Class::Bell => {
            let facing = props::direction_swne(block)?;
            let faces: &[Face] = match props::text(block, "attachment")? {
                "hanging" => &[Face::Up],
                "standing" => &[Face::Down],
                "side" => &[facing.opposite()],
                _ => &[facing, facing.opposite()],
            };
            for &face in faces {
                if adjacent(cx, pos, face)? == SupportType::None {
                    return Some(false);
                }
            }
            true
        }
        Class::Door | Class::WoodenDoor | Class::CopperDoor => {
            adjacent(cx, pos, Face::Down)?.has_edge_support() || down()?.is(Class::Door)
        }
        Class::DoublePlant | Class::DoubleTallGrass | Class::DoublePitcherCrop => {
            let top = props::flag(block, "upper_block_bit")?;
            let other = side(cx, pos, if top { Face::Down } else { Face::Up })?;
            let valid = other.is(Class::DoublePlant)
                && other.kind() == block.kind()
                && props::flag(other, "upper_block_bit")? != top;
            valid && (top || is_soil(down()?))
        }
        Class::SmallDripleaf => {
            let top = props::flag(block, "upper_block_bit")?;
            if !top && down()?.kind() != Kind::Clay {
                return Some(false);
            }
            side(cx, pos, if top { Face::Down } else { Face::Up })?.kind() == block.kind()
        }
        Class::BigDripleafHead | Class::BigDripleafStem => {
            let head = block.class() == Class::BigDripleafHead;
            if !head && !up()?.is(Class::BaseBigDripleaf) {
                return Some(false);
            }
            let d = down()?;
            (d.is(Class::BaseBigDripleaf) && d.class() == Class::BigDripleafStem)
                || d.kind() == Kind::Clay
                || is_soil(d)
        }
        Class::Rail | Class::PoweredRail | Class::DetectorRail | Class::ActivatorRail => {
            rail(cx, pos, block)?
        }
        _ => return None,
    })
}

/// A rail needs edge support below, and the block it climbs onto must have edge support on top
/// (PMMP `BaseRail::onNearbyBlockChange`).
fn rail(cx: &Cx<'_>, pos: BlockPos, block: block::State) -> Option<bool> {
    if !adjacent(cx, pos, Face::Down)?.has_edge_support() {
        return Some(false);
    }
    // PMMP `RailConnectionInfo`: shapes 2 to 5 ascend east, west, north, south.
    let ascend = match props::int(block, "rail_direction")? {
        2 => Face::East,
        3 => Face::West,
        4 => Face::North,
        5 => Face::South,
        _ => return Some(true),
    };
    Some(side(cx, pos, ascend)?.support(Face::Up).has_edge_support())
}

/// PMMP `ChorusFlower::canBeSupportedAt`.
fn chorus_flower(cx: &Cx<'_>, pos: BlockPos) -> Option<bool> {
    let down = side(cx, pos, Face::Down)?;
    if matches!(down.kind(), Kind::EndStone | Kind::ChorusPlant) {
        return Some(true);
    }
    let mut plant_adjacent = false;
    for face in HORIZONTAL {
        let s = side(cx, pos, face)?;
        if s.kind() == Kind::ChorusPlant {
            if plant_adjacent {
                return Some(false);
            }
            plant_adjacent = true;
        } else if s.kind() != Kind::Air {
            return Some(false);
        }
    }
    Some(plant_adjacent)
}

/// PMMP `ChorusPlant::canBeSupportedAt`.
fn chorus_plant(cx: &Cx<'_>, pos: BlockPos) -> Option<bool> {
    let holds = |s: block::State| matches!(s.kind(), Kind::ChorusPlant | Kind::EndStone);
    let down = side(cx, pos, Face::Down)?;
    let vertical_air = down.kind() == Kind::Air || side(cx, pos, Face::Up)?.kind() == Kind::Air;
    for face in HORIZONTAL {
        let s = side(cx, pos, face)?;
        if s.kind() == Kind::ChorusPlant {
            if !vertical_air {
                return Some(false);
            }
            let below = s_down(cx, pos, face)?;
            if holds(below) {
                return Some(true);
            }
        }
    }
    Some(holds(down))
}

/// The block below the `face` neighbour of `pos`.
fn s_down(cx: &Cx<'_>, pos: BlockPos, face: Face) -> Option<block::State> {
    cx.get(pos.step(face)?.step(Face::Down)?)
}

/// PMMP `FallableTrait`: a block with a replaceable block under it falls.
fn fall_check(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if block.class() == Class::ConcretePowder {
        return;
    }
    if side(cx, pos, Face::Down).is_some_and(block::State::can_be_replaced) {
        cx.fall(pos, block);
    }
}

/// A concrete powder that did not harden falls.
pub(super) fn fall_unless_hardened(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if side(cx, pos, Face::Down).is_some_and(block::State::can_be_replaced) {
        cx.fall(pos, block);
    }
}

/// Vines keep the faces with a solid block behind them or a vine on that face above (PMMP
/// `Vine::onNearbyBlockChange`).
fn vine(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let Some(faces) = props::faces(block) else {
        return;
    };
    let Some(up) = side(cx, pos, Face::Up) else {
        return;
    };
    let above = if up.class() == Class::Vine {
        props::faces(up).unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut bits = props::int(block, "vine_direction_bits").unwrap_or(0);
    for face in faces {
        if above.contains(&face) {
            continue;
        }
        let Some(behind) = side(cx, pos, face) else {
            return;
        };
        if !behind.is_solid() {
            bits &= !props::vine_bit(face).unwrap_or(0);
        }
    }
    finish_faces(cx, pos, block, "vine_direction_bits", bits);
}

/// Glow lichen and resin keep the faces with full support (PMMP `MultiAnySupportTrait`).
fn multi_face(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let Some(faces) = props::faces(block) else {
        return;
    };
    let mut bits = props::int(block, "multi_face_direction_bits").unwrap_or(0);
    for face in faces {
        let Some(support) = adjacent(cx, pos, face) else {
            return;
        };
        if support != SupportType::Full {
            bits &= !props::multi_face_bit(face);
        }
    }
    finish_faces(cx, pos, block, "multi_face_direction_bits", bits);
}

/// Sets the remaining faces, or destroys the block without any.
fn finish_faces(cx: &mut Cx<'_>, pos: BlockPos, block: block::State, name: &str, bits: i32) {
    if Some(bits) == props::int(block, name) {
        return;
    }
    if bits == 0 {
        cx.destroy(pos);
    } else if let Some(next) = props::with_int(block, name, bits) {
        cx.set(pos, next);
    }
}

/// A pressed button's scheduled update releases it (PMMP `Button::onScheduledUpdate`).
pub(super) fn release_button(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if props::flag(block, "button_pressed_bit") == Some(true)
        && let Some(released) = props::with_flag(block, "button_pressed_bit", false)
    {
        cx.set(pos, released);
    }
}

/// A bed's foot follows its head's occupied bit (PMMP `Bed::onNearbyBlockChange`).
pub(super) fn bed_on_nearby(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let (Some(head), Some(facing), Some(occupied)) = (
        props::flag(block, "head_piece_bit"),
        props::direction_swne(block),
        props::flag(block, "occupied_bit"),
    ) else {
        return;
    };
    if head {
        return;
    }
    let Some(other) = side(cx, pos, facing) else {
        return;
    };
    if other.class() == Class::Bed
        && props::flag(other, "head_piece_bit") == Some(true)
        && props::direction_swne(other) == Some(facing)
        && let Some(theirs) = props::flag(other, "occupied_bit")
        && theirs != occupied
        && let Some(next) = props::with_flag(block, "occupied_bit", theirs)
    {
        cx.set(pos, next);
    }
}
