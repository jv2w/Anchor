//! Blocks that need support and blocks that fall (PMMP `onNearbyBlockChange` of
//! `StaticSupportTrait`, `MultiAnySupportTrait`, `FallableTrait` and the attached blocks): when a
//! neighbour changes, a block that lost its support is destroyed with its drops, and a block
//! with nothing under it falls.

use super::props;
use super::{Rule, plant};
use crate::block::{self, Class, Face, Kind, SupportType, Trait, TypeTag};
use crate::tick::interact;
use crate::world::BlockPos;

/// Destroys the block with its drops, or makes it fall, if a neighbour change took its support.
pub(super) fn on_nearby(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if block.has_trait(Trait::Fallable) {
        fall_check(rule, pos, block);
        return;
    }
    match block.class() {
        Class::GlowLichen | Class::ResinClump => multi_face(rule, pos, block),
        Class::Vine => vine(rule, pos, block),
        _ => {
            if supported(rule, pos, block) == Some(false) {
                rule.destroy(pos);
            }
        }
    }
}

/// Tells whether the block still has what holds it, for the classes that break without it
/// ([`static_support`], then the attached and two-block ones); `None` for other classes or when
/// a block the check needs is not loaded.
pub(super) fn supported(rule: &Rule<'_>, pos: BlockPos, block: block::State) -> Option<bool> {
    if let Some(stands) = static_support(block, pos, |p| rule.loaded_block(p)) {
        return Some(stands);
    }
    let down = || side(rule, pos, Face::Down);
    let up = || side(rule, pos, Face::Up);
    let attached = |face: Face| attached_support(block, face, adjacent(rule, pos, face)?);
    Some(match block.class() {
        Class::ActivatorRail | Class::DetectorRail | Class::PoweredRail | Class::Rail => {
            rail(rule, pos, block)?
        }
        Class::AmethystCluster
        | Class::ItemFrame
        | Class::Ladder
        | Class::RedstoneTorch
        | Class::StoneButton
        | Class::Torch
        | Class::UnderwaterTorch
        | Class::WallCoralFan
        | Class::WoodenButton => attached(props::facing(block)?.opposite())?,
        Class::Bell => {
            let facing = props::facing(block)?;
            let faces: &[Face] = match props::string(block, "attachment")? {
                "hanging" => &[Face::Up],
                "standing" => &[Face::Down],
                "side" => &[facing.opposite()],
                _ => &[facing, facing.opposite()],
            };
            for &face in faces {
                if adjacent(rule, pos, face)? == SupportType::None {
                    return Some(false);
                }
            }
            true
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
        Class::BrownMushroom | Class::RedMushroom => !down()?.is_transparent(),
        Class::CeilingEdgesHangingSign => {
            let u = up()?;
            let facing = props::facing(block)?;
            u.support(Face::Down) == SupportType::Full
                || (matches!(
                    u.class(),
                    Class::CeilingEdgesHangingSign | Class::WallHangingSign
                ) && props::facing(u)
                    .is_some_and(|f| props::Axis::of(f) == props::Axis::of(facing)))
        }
        Class::CocoaBlock => plant::cocoa_attached(rule, pos, block)?,
        Class::CopperDoor | Class::Door | Class::WoodenDoor => {
            adjacent(rule, pos, Face::Down)?.has_edge_support() || down()?.is(Class::Door)
        }
        Class::CopperLantern | Class::Lantern => {
            let face = if props::flag(block, "hanging")? {
                Face::Up
            } else {
                Face::Down
            };
            attached(face)?
        }
        Class::DoublePitcherCrop | Class::DoublePlant | Class::DoubleTallGrass => {
            let top = props::flag(block, "upper_block_bit")?;
            let other = side(rule, pos, if top { Face::Down } else { Face::Up })?;
            let valid = other.is(Class::DoublePlant)
                && other.kind() == block.kind()
                && props::flag(other, "upper_block_bit")? != top;
            valid && (top || is_soil(down()?))
        }
        Class::FloorBanner | Class::OminousFloorBanner => down()?.is_solid(),
        Class::FloorSign => down()?.kind() != Kind::Air,
        Class::Lever => attached(props::lever_facing(block)?.opposite())?,
        Class::OminousWallBanner | Class::WallBanner => {
            let face = props::facing(block)?.opposite();
            side(rule, pos, face)?.is_solid()
        }
        Class::SmallDripleaf => {
            let top = props::flag(block, "upper_block_bit")?;
            if !top && down()?.kind() != Kind::Clay {
                return Some(false);
            }
            side(rule, pos, if top { Face::Down } else { Face::Up })?.kind() == block.kind()
        }
        Class::WallSign => {
            let face = props::facing(block)?.opposite();
            side(rule, pos, face)?.kind() != Kind::Air
        }
        _ => return None,
    })
}

/// Tells whether `block` stands at `pos` by PMMP's `canBeSupportedAt` of the classes with
/// `StaticSupportTrait` ([`Trait::StaticSupport`]), with `get` reading the blocks around it.
/// `None` for another class, or when `get` has no block the rule needs. Placement and the block
/// update rules share it, each with its own reader.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one table of PMMP's support rules, a short arm per class"
)]
pub(in crate::tick) fn static_support(
    block: block::State,
    pos: BlockPos,
    get: impl Fn(BlockPos) -> Option<block::State>,
) -> Option<bool> {
    let side = |face: Face| get(pos.step(face)?);
    let down = || side(Face::Down);
    let up = || side(Face::Up);
    let adjacent = |face: Face| Some(side(face)?.support(face.opposite()));
    Some(match block.class() {
        Class::Azalea => {
            let d = down()?;
            d.kind() == Kind::Clay || is_soil(d)
        }
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
        Class::Beetroot
        | Class::Carrot
        | Class::MelonStem
        | Class::PitcherCrop
        | Class::Potato
        | Class::PumpkinStem
        | Class::TorchflowerCrop
        | Class::Wheat => down()?.kind() == Kind::Farmland,
        Class::Cactus => {
            let d = down()?;
            if d.kind() != block.kind() && !d.has_type_tag(TypeTag::Sand) {
                return Some(false);
            }
            for face in Face::HORIZONTAL {
                if side(face)?.is_solid() {
                    return Some(false);
                }
            }
            true
        }
        Class::CactusFlower => {
            let d = down()?;
            d.support(Face::Up).has_center_support() || d.kind() == Kind::Cactus
        }
        Class::Cake | Class::CakeWithCandle | Class::CakeWithDyedCandle | Class::Carpet => {
            down()?.kind() != Kind::Air
        }
        Class::CaveVines => {
            let u = up()?;
            u.support(Face::Down) == SupportType::Full || u.kind() == block.kind()
        }
        Class::CeilingCenterHangingSign => {
            let u = up()?;
            u.support(Face::Down).has_center_support() || u.has_type_tag(TypeTag::HangingSign)
        }
        Class::ChorusFlower => chorus_flower(&side)?,
        Class::ChorusPlant => chorus_plant(pos, &get)?,
        Class::Coral | Class::FloorCoralFan | Class::FlowerPot | Class::RedstoneWire => {
            adjacent(Face::Down)?.has_center_support()
        }
        Class::DeadBush => {
            let d = down()?;
            d.has_type_tag(TypeTag::Sand)
                || d.has_type_tag(TypeTag::Mud)
                || matches!(
                    d.kind(),
                    Kind::Dirt
                        | Kind::Grass
                        | Kind::HardenedClay
                        | Kind::Mycelium
                        | Kind::Podzol
                        | Kind::StainedClay
                )
        }
        Class::Flower | Class::PinkPetals | Class::Sapling | Class::TallGrass => is_soil(down()?),
        Class::HangingRoots => adjacent(Face::Up)?.has_center_support(),
        Class::NetherFungus | Class::NetherRoots | Class::NetherSprouts => {
            let d = down()?;
            is_soil(d) || d.has_type_tag(TypeTag::Nylium) || d.kind() == Kind::SoulSoil
        }
        Class::NetherVines => {
            let growth = plant::vine_growth(block);
            let support = side(growth.opposite())?;
            support.support(growth).has_center_support() || support.kind() == block.kind()
        }
        Class::NetherWartPlant => down()?.kind() == Kind::SoulSand,
        Class::RedstoneComparator
        | Class::RedstoneRepeater
        | Class::StonePressurePlate
        | Class::WeightedPressurePlateHeavy
        | Class::WeightedPressurePlateLight
        | Class::WoodenPressurePlate => adjacent(Face::Down)? != SupportType::None,
        Class::SporeBlossom => adjacent(Face::Up)? == SupportType::Full,
        Class::Sugarcane => {
            let d = down()?;
            d.kind() == block.kind()
                || d.has_type_tag(TypeTag::Mud)
                || d.has_type_tag(TypeTag::Dirt)
                || d.has_type_tag(TypeTag::Sand)
        }
        Class::SweetBerryBush => {
            let d = down()?;
            d.kind() != Kind::Farmland && is_soil(d)
        }
        Class::WaterLily => down()?.kind() == Kind::Water,
        Class::WitherRose => {
            let d = down()?;
            is_soil(d) || matches!(d.kind(), Kind::Netherrack | Kind::SoulSand | Kind::SoulSoil)
        }
        _ => return None,
    })
}

/// Tells whether `block` holds on its `face` side, where the block gives back `support`, by
/// PMMP's `canBeSupportedAt` of the blocks attached to one face (torches, ladders, levers,
/// buttons, item frames, amethyst clusters, wall coral fans, lanterns). `None` for another
/// class. Placement and the block update rules share it.
pub(in crate::tick) fn attached_support(
    block: block::State,
    face: Face,
    support: SupportType,
) -> Option<bool> {
    Some(match block.class() {
        Class::AmethystCluster | Class::Ladder => support == SupportType::Full,
        Class::CopperLantern
        | Class::Lantern
        | Class::Lever
        | Class::StoneButton
        | Class::WallCoralFan
        | Class::WoodenButton => support.has_center_support(),
        Class::ItemFrame => support != SupportType::None,
        Class::RedstoneTorch | Class::Torch | Class::UnderwaterTorch => {
            if face == Face::Down {
                support.has_center_support()
            } else {
                support == SupportType::Full
            }
        }
        _ => return None,
    })
}

/// Tells whether a rail holds: it needs edge support below, and the block it climbs onto must
/// have edge support on top (PMMP `BaseRail::onNearbyBlockChange`).
fn rail(rule: &Rule<'_>, pos: BlockPos, block: block::State) -> Option<bool> {
    if !adjacent(rule, pos, Face::Down)?.has_edge_support() {
        return Some(false);
    }
    let Some(ascend) = interact::rail_ascent(props::int(block, "rail_direction")?) else {
        return Some(true);
    };
    Some(
        side(rule, pos, ascend)?
            .support(Face::Up)
            .has_edge_support(),
    )
}

/// Tells whether a chorus flower stands (PMMP `ChorusFlower::canBeSupportedAt`), with `side`
/// reading the block toward a face.
fn chorus_flower(side: &impl Fn(Face) -> Option<block::State>) -> Option<bool> {
    let down = side(Face::Down)?;
    if matches!(down.kind(), Kind::ChorusPlant | Kind::EndStone) {
        return Some(true);
    }
    let mut plant_adjacent = false;
    for face in Face::HORIZONTAL {
        let s = side(face)?;
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

/// Tells whether a chorus plant stands at `pos` (PMMP `ChorusPlant::canBeSupportedAt`), with
/// `get` reading the blocks.
fn chorus_plant(pos: BlockPos, get: &impl Fn(BlockPos) -> Option<block::State>) -> Option<bool> {
    let holds = |s: block::State| matches!(s.kind(), Kind::ChorusPlant | Kind::EndStone);
    let side = |face: Face| get(pos.step(face)?);
    let down = side(Face::Down)?;
    let vertical_air = down.kind() == Kind::Air || side(Face::Up)?.kind() == Kind::Air;
    for face in Face::HORIZONTAL {
        let s = side(face)?;
        if s.kind() == Kind::ChorusPlant {
            if !vertical_air {
                return Some(false);
            }
            let below = get(pos.step(face)?.step(Face::Down)?)?;
            if holds(below) {
                return Some(true);
            }
        }
    }
    Some(holds(down))
}

/// Makes a block with a replaceable block under it fall (PMMP `FallableTrait`).
pub(super) fn fall_check(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if side(rule, pos, Face::Down).is_some_and(block::State::can_be_replaced) {
        rule.fall(pos, block);
    }
}

/// Keeps the faces of vines with a solid block behind them or a vine on that face above (PMMP
/// `Vine::onNearbyBlockChange`).
fn vine(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    let Some(faces) = props::faces(block) else {
        return;
    };
    let Some(up) = side(rule, pos, Face::Up) else {
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
        let Some(behind) = side(rule, pos, face) else {
            return;
        };
        if !behind.is_solid() {
            bits &= !props::vine_bit(face).unwrap_or(0);
        }
    }
    finish_faces(rule, pos, block, "vine_direction_bits", bits);
}

/// Keeps the faces of glow lichen and resin with full support (PMMP `MultiAnySupportTrait`).
fn multi_face(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    let Some(faces) = props::faces(block) else {
        return;
    };
    let mut bits = props::int(block, "multi_face_direction_bits").unwrap_or(0);
    for face in faces {
        let Some(support) = adjacent(rule, pos, face) else {
            return;
        };
        if support != SupportType::Full {
            bits &= !props::multi_face_bit(face);
        }
    }
    finish_faces(rule, pos, block, "multi_face_direction_bits", bits);
}

/// Sets the remaining faces, or destroys the block without any.
fn finish_faces(rule: &mut Rule<'_>, pos: BlockPos, block: block::State, name: &str, bits: i32) {
    if Some(bits) == props::int(block, name) {
        return;
    }
    if bits == 0 {
        rule.destroy(pos);
    } else if let Some(next) = props::with_int(block, name, bits) {
        rule.set(pos, next);
    }
}

/// Releases a pressed button on its scheduled update (PMMP `Button::onScheduledUpdate`).
pub(super) fn release_button(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if props::flag(block, "button_pressed_bit") == Some(true)
        && let Some(released) = props::with_flag(block, "button_pressed_bit", false)
    {
        rule.set(pos, released);
    }
}

/// Makes a bed's foot follow its head's occupied bit (PMMP `Bed::onNearbyBlockChange`).
pub(super) fn bed_on_nearby(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    let (Some(head), Some(facing), Some(occupied)) = (
        props::flag(block, "head_piece_bit"),
        props::facing(block),
        props::flag(block, "occupied_bit"),
    ) else {
        return;
    };
    if head {
        return;
    }
    let Some(other) = side(rule, pos, facing) else {
        return;
    };
    if other.class() == Class::Bed
        && props::flag(other, "head_piece_bit") == Some(true)
        && props::facing(other) == Some(facing)
        && let Some(theirs) = props::flag(other, "occupied_bit")
        && theirs != occupied
        && let Some(next) = props::with_flag(block, "occupied_bit", theirs)
    {
        rule.set(pos, next);
    }
}

/// Reads the support the block on the `face` side of `pos` gives toward `pos` (PMMP
/// `getAdjacentSupportType`), or `None` if it is not loaded ([`Rule::loaded_block`]).
pub(super) fn adjacent(rule: &Rule<'_>, pos: BlockPos, face: Face) -> Option<SupportType> {
    Some(side(rule, pos, face)?.support(face.opposite()))
}

/// Reads the block on the `face` side of `pos`, or `None` if it is not loaded
/// ([`Rule::loaded_block`]).
pub(super) fn side(rule: &Rule<'_>, pos: BlockPos, face: Face) -> Option<block::State> {
    rule.loaded_block(pos.step(face)?)
}

/// Tells whether the block has the dirt or mud tag (PMMP `BlockTypeTags::DIRT`, `MUD`).
pub(super) fn is_soil(s: block::State) -> bool {
    s.has_type_tag(TypeTag::Dirt) || s.has_type_tag(TypeTag::Mud)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rules of [`static_support`] and the classes PMMP gives `StaticSupportTrait` are the
    /// same set: placement picks the rule by the trait.
    #[test]
    fn static_support_rules_cover_exactly_the_static_support_trait() {
        let pos = BlockPos::new(0, 10, 0);
        for s in block::State::all() {
            assert_eq!(
                s.has_trait(Trait::StaticSupport),
                static_support(s, pos, |_| Some(block::AIR)).is_some(),
                "{s:?}"
            );
        }
    }
}
