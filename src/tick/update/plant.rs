//! Plants (PMMP `onRandomTick`, `onNearbyBlockChange` and bone meal `onInteract` of crops, stems,
//! sugar cane, cactus, cocoa, nether wart, bamboo, sweet berries, pitcher and torchflower crops,
//! cave and nether vines, chorus flowers, saplings, dripleaves, glow lichen, pink petals, grass,
//! azalea and fungi).

use super::Cx;
use super::props::{self, HORIZONTAL};
use super::support::{self, side};
use super::tree::{self, Tx};
use crate::block::{self, Class, Face, ItemForm, Kind, TypeTag};
use crate::item::Stack;
use crate::random::Random;
use crate::tick::interact;
use crate::world::BlockPos;
use crate::world::TreeType;

/// The crop age property of the classes that have one.
const GROWTH: &str = "growth";
/// The least potential light a crop grows in (PMMP `CropGrowthHelper::MIN_LIGHT_LEVEL`).
const CROP_LIGHT: u8 = 9;

/// A random tick reached a plant.
pub(super) fn on_random(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    match block.class() {
        Class::Wheat | Class::Carrot | Class::Potato | Class::Beetroot => {
            if age(block).is_some_and(|a| a < 7) && can_grow(cx, pos, block) {
                grow_age(cx, pos, block, 1);
            }
        }
        Class::MelonStem | Class::PumpkinStem => stem(cx, pos, block),
        Class::Sugarcane => sugarcane(cx, pos, block),
        Class::Cactus => cactus(cx, pos, block),
        Class::CocoaBlock => {
            if cx.rng().range(1, 5) == 1 {
                cocoa_grow(cx, pos, block);
            }
        }
        Class::NetherWartPlant => {
            if props::int(block, "age").is_some_and(|a| a < 3)
                && cx.rng().range(0, 10) == 0
                && let Some(s) = aged(block, "age", 1)
            {
                cx.grow(pos, s);
            }
        }
        Class::SweetBerryBush => {
            if age(block).is_some_and(|a| a < 3) && cx.rng().range(0, 2) == 1 {
                grow_age(cx, pos, block, 1);
            }
        }
        Class::Bamboo => bamboo(cx, pos, block),
        Class::BambooSapling => bamboo_sapling(cx, pos, block),
        Class::PitcherCrop => {
            if can_grow(cx, pos, block) {
                pitcher_grow(cx, pos, block);
            }
        }
        Class::DoublePitcherCrop => {
            if props::flag(block, "upper_block_bit") == Some(false) && can_grow(cx, pos, block) {
                double_pitcher_grow(cx, pos, block);
            }
        }
        Class::TorchflowerCrop => {
            if can_grow(cx, pos, block) {
                torchflower_grow(cx, pos, block);
            }
        }
        Class::CaveVines => cave_vines(cx, pos, block),
        Class::NetherVines => {
            let max = vine_age(block).is_some_and(|a| a < MAX_VINE_AGE);
            if max && cx.rng().range(1, 10) == 1 {
                let growth = vine_growth(block);
                if pos
                    .step(growth)
                    .and_then(|p| cx.get(p))
                    .is_some_and(block::State::can_be_replaced)
                {
                    nether_vines_grow(cx, pos, block, 1);
                }
            }
        }
        Class::ChorusFlower => chorus_flower(cx, pos, block),
        Class::Sapling => sapling(cx, pos, block),
        _ => {}
    }
}

/// PMMP `Sapling::onRandomTick`: in light of 8 or more, one in seven gets ready, then grows.
fn sapling(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if cx.full_light(pos) < 8 || cx.rng().range(1, 7) != 1 {
        return;
    }
    if props::flag(block, "age_bit") == Some(true) {
        tree::grow_sapling(cx, pos, block);
    } else if let Some(ready) = props::with_flag(block, "age_bit", true) {
        cx.set(pos, ready);
    }
}

/// Bone meal on a plant: whether it is used up.
pub(super) fn fertilize(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) -> bool {
    match block.class() {
        Class::Wheat
        | Class::Carrot
        | Class::Potato
        | Class::Beetroot
        | Class::MelonStem
        | Class::PumpkinStem => {
            let Some(a) = age(block).filter(|a| *a < 7) else {
                return false;
            };
            let next = (a + cx.rng().range(2, 5)).min(7);
            props::with_int(block, GROWTH, next).is_some_and(|s| cx.grow(pos, s))
        }
        Class::Sugarcane => {
            let Some(bottom) = seek(cx, pos, block, Face::Down) else {
                return false;
            };
            sugarcane_grow(cx, bottom, block)
        }
        Class::CocoaBlock => cocoa_grow(cx, pos, block),
        Class::SweetBerryBush => sweet_berries(cx, pos, block),
        Class::Bamboo => {
            let Some(top) = seek(cx, pos, block, Face::Up) else {
                return false;
            };
            let Some(top_block) = cx.get(top) else {
                return false;
            };
            let amount = cx.rng().range(1, 2);
            bamboo_grow(cx, top, top_block, bamboo_max_height(top.x, top.z), amount)
        }
        Class::BambooSapling => bamboo_sapling_grow(cx, pos),
        Class::NetherVines => {
            let amount = cx.rng().range(1, 5);
            nether_vines_grow(cx, pos, block, amount)
        }
        Class::CaveVines => {
            if berries(block) {
                // PMMP picks the berries first, bone meal or not.
                pick_berries(cx, pos, block);
                return false;
            }
            let head = side(cx, pos, Face::Down).is_some_and(|d| d.kind() != block.kind());
            cave_vine_state(block, vine_age(block).unwrap_or(0), true, head)
                .is_some_and(|s| cx.grow(pos, s))
        }
        Class::PitcherCrop => pitcher_grow(cx, pos, block),
        Class::DoublePitcherCrop => double_pitcher_grow(cx, pos, block),
        Class::TorchflowerCrop => torchflower_grow(cx, pos, block),
        Class::PinkPetals => {
            let Some(count) = props::int(block, GROWTH) else {
                return false;
            };
            if count < 3 {
                props::with_int(block, GROWTH, count + 1).is_some_and(|s| cx.grow(pos, s))
            } else {
                drop_self(cx, pos, block, 1);
                true
            }
        }
        Class::TallGrass => tall_grass(cx, pos, block),
        Class::GlowLichen => glow_lichen(cx, pos, block),
        Class::SmallDripleaf => small_dripleaf(cx, pos, block),
        Class::BigDripleafHead | Class::BigDripleafStem => big_dripleaf(cx, pos),
        Class::Sapling => tree::grow_sapling(cx, pos, block),
        Class::Azalea => {
            if cx.rng().range(1, 100) <= 45 {
                tree::grow(cx, pos, TreeType::Azalea);
            }
            true
        }
        Class::NetherFungus => fungus(cx, pos, block),
        _ => false,
    }
}

/// PMMP `SweetBerryBush::onInteract`: bone meal grows a bush that is not ripe; a ripe one is
/// picked instead (berries dropped, bone meal kept).
fn sweet_berries(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) -> bool {
    match age(block) {
        Some(a) if a < 3 => grow_age(cx, pos, block, 1),
        Some(a) => {
            let mut rng = cx.rng();
            let count = if a == 3 {
                rng.range(2, 3)
            } else {
                rng.range(1, 2)
            };
            if let Some(picked) = props::with_int(block, GROWTH, 1)
                && cx.set(pos, picked)
            {
                drop_self(cx, pos, block, count);
            }
            false
        }
        None => false,
    }
}

/// PMMP `NetherFungus::onInteract`: on its own nylium, 40 in 100 grow a huge fungus; the bone
/// meal is used either way.
fn fungus(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) -> bool {
    let (nylium, kind) = if block.kind() == Kind::CrimsonFungus {
        (Kind::CrimsonNylium, TreeType::Crimson)
    } else {
        (Kind::WarpedNylium, TreeType::Warped)
    };
    if side(cx, pos, Face::Down).is_some_and(|d| d.kind() == nylium) && cx.rng().range(1, 100) <= 40
    {
        tree::grow(cx, pos, kind);
    }
    true
}

/// The crop age (`growth`).
fn age(block: block::State) -> Option<i32> {
    props::int(block, GROWTH)
}

/// Grows the block `by` stages older (PMMP `BlockEventHelper::grow`).
fn grow_age(cx: &mut Cx<'_>, pos: BlockPos, block: block::State, by: i32) -> bool {
    aged(block, GROWTH, by).is_some_and(|s| cx.grow(pos, s))
}

/// The block with the integer property `name` `by` more.
fn aged(block: block::State, name: &str, by: i32) -> Option<block::State> {
    props::int(block, name).and_then(|v| props::with_int(block, name, v + by))
}

/// Drops `count` of the block's item at its position.
fn drop_self(cx: &mut Cx<'_>, pos: BlockPos, block: block::State, count: i32) {
    if let ItemForm::Item(item) = block.item()
        && let Ok(count) = u8::try_from(count)
    {
        cx.drop_item(pos, Stack::new(item, count));
    }
}

/// PMMP `CropGrowthHelper::canGrow`: one chance in `25 / multiplier + 1` (the multiplier from
/// the farmland under the crop and around it, halved for crops of the same kind not in spaced
/// rows) and potential light of at least 9.
pub(super) fn can_grow(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) -> bool {
    let Some(bound) = growth_bound(cx, pos, block) else {
        return false;
    };
    cx.rng().range(0, bound) == 0 && cx.potential_light(pos) >= CROP_LIGHT
}

/// `(int) (25 / multiplier)` of `CropGrowthHelper`, in eighths so the arithmetic is exact: the
/// multiplier is 1, plus 3 (wet) or 1 (dry) for the farmland below, plus 3/4 or 1/4 for each
/// farmland around, halved for an improper arrangement.
pub(super) fn growth_bound(cx: &Cx<'_>, pos: BlockPos, block: block::State) -> Option<i32> {
    let below = pos.offset(0, -1, 0)?;
    let wet = |s: block::State| props::int(s, "moisturized_amount").unwrap_or(0) > 0;
    let mut eighths = 8;
    let farmland = cx.get(below)?;
    if farmland.kind() == Kind::Farmland {
        eighths += if wet(farmland) { 24 } else { 8 };
    }
    let (mut x_row, mut z_row, mut improper) = (false, false, false);
    for x in -1..=1 {
        for z in -1..=1 {
            if x == 0 && z == 0 {
                continue;
            }
            let next = cx.get(below.offset(x, 0, z)?)?;
            if next.kind() != Kind::Farmland {
                continue;
            }
            eighths += if wet(next) { 6 } else { 2 };
            if !improper && cx.get(pos.offset(x, 0, z)?)?.kind() == block.kind() {
                if x == 0 {
                    if z_row {
                        improper = true;
                    } else {
                        x_row = true;
                    }
                } else if z == 0 {
                    if x_row {
                        improper = true;
                    } else {
                        z_row = true;
                    }
                } else {
                    improper = true;
                }
            }
        }
    }
    if improper {
        eighths /= 2;
    }
    Some(200 / eighths)
}

/// The kind a stem grows.
fn stem_fruit(block: block::State) -> Kind {
    if block.class() == Class::MelonStem {
        Kind::Melon
    } else {
        Kind::Pumpkin
    }
}

/// The side a stem is attached to (up: not attached).
fn stem_facing(block: block::State) -> Option<Face> {
    interact::props::facing(block)
}

/// A stem with the facing `face` (up: not attached).
fn stem_with(block: block::State, face: Face) -> Option<block::State> {
    interact::props::with_facing(block, face)
}

/// PMMP `Stem::onRandomTick`.
fn stem(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if stem_facing(block) != Some(Face::Up) || !can_grow(cx, pos, block) {
        return;
    }
    if age(block).is_some_and(|a| a < 7) {
        grow_age(cx, pos, block, 1);
        return;
    }
    let fruit = stem_fruit(block);
    for face in HORIZONTAL {
        match side(cx, pos, face) {
            Some(s) if s.kind() == fruit => return,
            Some(_) => {}
            None => return,
        }
    }
    let Some(face) = HORIZONTAL.get(cx.rng().below(4) as usize).copied() else {
        return;
    };
    let Some(at) = pos.step(face) else {
        return;
    };
    let empty = cx.get(at).is_some_and(|s| s.kind() == Kind::Air);
    let soil = at
        .step(Face::Down)
        .and_then(|p| cx.get(p))
        .is_some_and(|s| s.has_type_tag(TypeTag::Dirt));
    if empty
        && soil
        && cx.grow(at, fruit.default_state())
        && let Some(attached) = stem_with(block, face)
    {
        cx.set(pos, attached);
    }
}

/// PMMP `Stem::onNearbyBlockChange`: a stem whose fruit is gone points up again, then the crop
/// support check.
pub(super) fn stem_on_nearby(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let mut block = block;
    if let Some(face) = stem_facing(block)
        && face != Face::Up
        && side(cx, pos, face).is_some_and(|s| s.kind() != stem_fruit(block))
        && let Some(up) = stem_with(block, Face::Up)
    {
        cx.set(pos, up);
        block = up;
    }
    if support::supported(cx, pos, block) == Some(false) {
        cx.destroy(pos);
    }
}

/// The last block of the plant's kind from `pos` toward `face`.
fn seek(cx: &Cx<'_>, pos: BlockPos, block: block::State, face: Face) -> Option<BlockPos> {
    let mut at = pos;
    loop {
        let next = at.step(face)?;
        match cx.get(next) {
            Some(s) if s.kind() == block.kind() => at = next,
            _ => return Some(at),
        }
    }
}

/// Whether water or frosted ice is beside the block under sugar cane.
fn near_water(cx: &Cx<'_>, below: BlockPos) -> Option<bool> {
    for face in HORIZONTAL {
        let s = cx.loaded_block(below.step(face)?)?;
        if matches!(s.kind(), Kind::Water | Kind::FrostedIce) {
            return Some(true);
        }
    }
    Some(false)
}

/// PMMP `Sugarcane::onNearbyBlockChange`.
pub(super) fn sugarcane_on_nearby(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let Some(below) = pos.step(Face::Down) else {
        return;
    };
    let Some(down) = cx.loaded_block(below) else {
        return;
    };
    // Cane on cane always stays: it needs no water and is supported.
    if down.kind() == block.kind() {
        return;
    }
    let Some(water) = near_water(cx, below) else {
        return;
    };
    if !water || support::supported(cx, pos, block) == Some(false) {
        cx.destroy(pos);
    }
}

/// PMMP `Sugarcane::onRandomTick`.
fn sugarcane(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let Some(below) = pos.step(Face::Down) else {
        return;
    };
    let Some(down) = cx.loaded_block(below) else {
        return;
    };
    if down.kind() == block.kind() {
        return;
    }
    match near_water(cx, below) {
        Some(false) => cx.destroy(pos),
        Some(true) => {
            if props::int(block, "age") == Some(15) {
                sugarcane_grow(cx, pos, block);
            } else if let Some(s) = aged(block, "age", 1) {
                cx.set(pos, s);
            }
        }
        None => {}
    }
}

/// PMMP `Sugarcane::grow`: up to two blocks of cane above `bottom`, then `block` at age 0 at
/// `bottom`. Whether a block grew.
fn sugarcane_grow(cx: &mut Cx<'_>, bottom: BlockPos, block: block::State) -> bool {
    let mut grew = false;
    for y in 1..3 {
        let Some(at) = bottom.offset(0, y, 0) else {
            break;
        };
        let Some(b) = cx.get(at) else {
            break;
        };
        if b.kind() == Kind::Air {
            if cx.grow(at, Kind::Sugarcane.default_state()) {
                grew = true;
            } else {
                break;
            }
        } else if b.kind() != block.kind() {
            break;
        }
    }
    if let Some(young) = props::with_int(block, "age", 0) {
        cx.set(bottom, young);
    }
    grew
}

/// PMMP `Cactus::onRandomTick`.
fn cactus(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let Some(up_pos) = pos.step(Face::Up) else {
        return;
    };
    if cx.get(up_pos).is_none_or(|u| u.kind() != Kind::Air) {
        return;
    }
    let mut height = 1;
    while height < 3 {
        match pos.offset(0, -height, 0).and_then(|p| cx.get(p)) {
            Some(s) if s.kind() == block.kind() => height += 1,
            _ => break,
        }
    }
    let Some(age) = props::int(block, "age") else {
        return;
    };
    if age == 9 {
        let mut flower = true;
        for face in HORIZONTAL {
            if up_pos
                .step(face)
                .and_then(|p| cx.get(p))
                .is_none_or(block::State::is_solid)
            {
                flower = false;
                break;
            }
        }
        if flower {
            let chance = if height >= 3 { 25 } else { 10 };
            if cx.rng().range(1, 100) <= chance {
                if cx.grow(up_pos, Kind::CactusFlower.default_state())
                    && let Some(young) = props::with_int(block, "age", 0)
                {
                    cx.set_quietly(pos, young);
                }
                return;
            }
        }
    }
    let next = if age == 15 {
        if height < 3 {
            cx.grow(up_pos, Kind::Cactus.default_state());
        }
        0
    } else {
        age + 1
    };
    if let Some(s) = props::with_int(block, "age", next) {
        cx.set_quietly(pos, s);
    }
}

/// PMMP `CocoaBlock::grow`.
fn cocoa_grow(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) -> bool {
    props::int(block, "age").is_some_and(|a| a < 2)
        && aged(block, "age", 1).is_some_and(|s| cx.grow(pos, s))
}

/// Whether a cocoa pod's log is still there (PMMP `CocoaBlock::canAttachTo`: jungle wood).
pub(super) fn cocoa_attached(cx: &Cx<'_>, pos: BlockPos, block: block::State) -> Option<bool> {
    let face = props::direction_nesw(block)?.opposite();
    let log = side(cx, pos, face)?;
    Some(matches!(log.kind(), Kind::JungleLog | Kind::JungleWood))
}

/// PMMP `Bamboo::getMaxHeight`: 12 to 16 by a hash of the column.
pub(super) fn bamboo_max_height(x: i32, z: i32) -> i32 {
    let p1 = i64::from(z).wrapping_mul(0x6eb_fff5);
    let p2 = i64::from(x).wrapping_mul(0x2f_c20f);
    let xord = p1 ^ p2;
    let full = xord
        .wrapping_mul(0x285_b825)
        .wrapping_add(0xb)
        .wrapping_mul(xord);
    let low = full & 0xffff_ffff;
    12 + i32::try_from(low % 5).unwrap_or(0)
}

/// PMMP `Bamboo::onRandomTick`.
fn bamboo(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let ready = props::flag(block, "age_bit") == Some(true);
    if ready {
        let Some(unready) = props::with_flag(block, "age_bit", false) else {
            return;
        };
        if cx.full_light(pos) < 9
            || !bamboo_grow(cx, pos, unready, bamboo_max_height(pos.x, pos.z), 1)
        {
            cx.set(pos, unready);
        }
    } else if pos
        .step(Face::Up)
        .and_then(|p| cx.get(p))
        .is_some_and(block::State::can_be_replaced)
        && let Some(r) = props::with_flag(block, "age_bit", true)
    {
        cx.set(pos, r);
    }
}

/// PMMP `Bamboo::grow`: the top of a stalk at `pos` grows `amount` blocks up to `max` high.
fn bamboo_grow(cx: &mut Cx<'_>, pos: BlockPos, block: block::State, max: i32, amount: i32) -> bool {
    let Some(up) = pos.step(Face::Up) else {
        return false;
    };
    if !cx.get(up).is_some_and(block::State::can_be_replaced) {
        return false;
    }
    let mut height = 1;
    while pos
        .offset(0, -height, 0)
        .and_then(|p| cx.get(p))
        .is_some_and(|s| s.kind() == block.kind())
    {
        height += 1;
        if height >= max {
            return false;
        }
    }
    let new_height = height + amount;
    let leaves = |s: block::State, size: &str| props::with_text(s, "bamboo_leaf_size", size);
    let Some(mut stem) =
        props::with_flag(block, "age_bit", false).and_then(|s| leaves(s, "no_leaves"))
    else {
        return false;
    };
    if new_height >= 4
        && props::text(stem, "bamboo_stalk_thickness") == Some("thin")
        && let Some(thick) = props::with_text(stem, "bamboo_stalk_thickness", "thick")
    {
        stem = thick;
    }
    let (Some(small), Some(big)) = (leaves(stem, "small_leaves"), leaves(stem, "large_leaves"))
    else {
        return false;
    };
    let mut blocks = Vec::with_capacity(8);
    match new_height {
        2 => blocks.push(small),
        3 => blocks.extend([small, small]),
        4 => blocks.extend([big, small, stem, stem]),
        h if h > 4 => {
            blocks.extend([big, big, small]);
            let more = amount.min(h - 3);
            for _ in 0..more {
                blocks.push(stem);
            }
        }
        _ => {}
    }
    let mut tx = Tx::default();
    for (index, s) in blocks.into_iter().enumerate() {
        let dy = amount - i32::try_from(index).unwrap_or(0);
        if let Some(at) = pos.offset(0, dy, 0) {
            tx.add(at, s);
        }
    }
    tx.apply(cx, pos)
}

/// PMMP `BambooSapling::onRandomTick`.
fn bamboo_sapling(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if props::flag(block, "age_bit") == Some(true) {
        let Some(unready) = props::with_flag(block, "age_bit", false) else {
            return;
        };
        if cx.full_light(pos) < 9 || !bamboo_sapling_grow(cx, pos) {
            cx.set(pos, unready);
        }
    } else if pos
        .step(Face::Up)
        .and_then(|p| cx.get(p))
        .is_some_and(block::State::can_be_replaced)
        && let Some(r) = props::with_flag(block, "age_bit", true)
    {
        cx.set(pos, r);
    }
}

/// PMMP `BambooSapling::grow`: bamboo with a small-leaved bamboo above.
fn bamboo_sapling_grow(cx: &mut Cx<'_>, pos: BlockPos) -> bool {
    let Some(up) = pos.step(Face::Up) else {
        return false;
    };
    if !cx.get(up).is_some_and(block::State::can_be_replaced) {
        return false;
    }
    let bamboo = Kind::Bamboo.default_state();
    let Some(small) = props::with_text(bamboo, "bamboo_leaf_size", "small_leaves") else {
        return false;
    };
    let mut tx = Tx::default();
    tx.add(pos, bamboo);
    tx.add(up, small);
    tx.apply(cx, pos)
}

/// PMMP `PitcherCrop::grow`: stages 0 to 2, then the two-block plant.
fn pitcher_grow(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) -> bool {
    match age(block) {
        Some(a) if a < 2 => grow_age(cx, pos, block, 1),
        Some(2) => {
            let Some(up) = pos.step(Face::Up) else {
                return false;
            };
            if cx.get(up).is_none_or(|u| u.kind() != Kind::Air) {
                return false;
            }
            let (Some(bottom), Some(top)) = (pitcher_half(3, false), pitcher_half(3, true)) else {
                return false;
            };
            let mut tx = Tx::default();
            tx.add(pos, bottom);
            tx.add(up, top);
            tx.apply(cx, pos)
        }
        _ => false,
    }
}

/// The two-block pitcher crop with `growth` (3 or 4) and half.
fn pitcher_half(growth: i32, top: bool) -> Option<block::State> {
    props::with_int(Kind::DoublePitcherCrop.default_state(), GROWTH, growth)
        .and_then(|s| props::with_flag(s, "upper_block_bit", top))
}

/// PMMP `DoublePitcherCrop::grow`.
fn double_pitcher_grow(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) -> bool {
    if age(block) != Some(3) {
        return false;
    }
    let Some(top_half) = props::flag(block, "upper_block_bit") else {
        return false;
    };
    let (Some(bottom), Some(top)) = (
        if top_half {
            pos.step(Face::Down)
        } else {
            Some(pos)
        },
        if top_half {
            Some(pos)
        } else {
            pos.step(Face::Up)
        },
    ) else {
        return false;
    };
    if cx
        .get(top)
        .is_none_or(|t| t.kind() != Kind::Air && t.kind() != block.kind())
    {
        return false;
    }
    let (Some(b), Some(t)) = (pitcher_half(4, false), pitcher_half(4, true)) else {
        return false;
    };
    let mut tx = Tx::default();
    tx.add(bottom, b);
    tx.add(top, t);
    tx.apply(cx, bottom)
}

/// PMMP `TorchflowerCrop::getNextState`: ready, then the torchflower.
fn torchflower_grow(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) -> bool {
    let next = if age(block) == Some(1) {
        Some(Kind::Torchflower.default_state())
    } else {
        props::with_int(block, GROWTH, 1)
    };
    next.is_some_and(|s| cx.grow(pos, s))
}

/// The most age of cave and nether vines (PMMP `CaveVines::MAX_AGE`, `NetherVines::MAX_AGE`).
pub(in crate::tick) const MAX_VINE_AGE: i32 = 25;

/// The age of cave or nether vines.
fn vine_age(block: block::State) -> Option<i32> {
    props::int(block, "growing_plant_age")
        .or_else(|| props::int(block, "weeping_vines_age"))
        .or_else(|| props::int(block, "twisting_vines_age"))
}

/// The direction nether vines grow: weeping vines down, twisting vines up.
pub(super) fn vine_growth(block: block::State) -> Face {
    if block.kind() == Kind::WeepingVines {
        Face::Down
    } else {
        Face::Up
    }
}

/// The age property of nether vines: weeping or twisting.
pub(in crate::tick) fn nether_vines_age(block: block::State) -> &'static str {
    if vine_growth(block) == Face::Down {
        "weeping_vines_age"
    } else {
        "twisting_vines_age"
    }
}

/// Whether cave vines have berries.
fn berries(block: block::State) -> bool {
    block.name() != "minecraft:cave_vines"
}

/// The cave vines state with `age`, berries and head (Bedrock has no head without berries:
/// PMMP `FlattenedCaveVinesVariant`).
fn cave_vine_state(
    block: block::State,
    age: i32,
    berries: bool,
    head: bool,
) -> Option<block::State> {
    let name = match (berries, head) {
        (false, _) => "minecraft:cave_vines",
        (true, true) => "minecraft:cave_vines_head_with_berries",
        (true, false) => "minecraft:cave_vines_body_with_berries",
    };
    props::with_int(
        interact::props::renamed(block, name)?,
        "growing_plant_age",
        age,
    )
}

/// Picks the berries of cave vines (PMMP `CaveVines::onInteract`).
fn pick_berries(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if let Some(bare) = cave_vine_state(block, vine_age(block).unwrap_or(0), false, false)
        && cx.set(pos, bare)
    {
        drop_self(cx, pos, block, 1);
    }
}

/// PMMP `CaveVines::onRandomTick`.
fn cave_vines(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let Some(below) = pos.step(Face::Down) else {
        return;
    };
    let Some(down) = cx.get(below) else {
        return;
    };
    let head = down.kind() != block.kind();
    let age = vine_age(block).unwrap_or(0);
    let has_berries = berries(block);
    let mut block = block;
    if has_berries
        && head != (block.name() == "minecraft:cave_vines_head_with_berries")
        && let Some(s) = cave_vine_state(block, age, true, head)
    {
        cx.set(pos, s);
        block = s;
    }
    let mut rng = cx.rng();
    if age < MAX_VINE_AGE && rng.range(1, 10) == 1 && down.kind() == Kind::Air {
        let fruit = rng.range(1, 9) == 1;
        if let Some(s) = cave_vine_state(block, age + 1, fruit, false) {
            cx.grow(below, s);
        }
    }
}

/// PMMP `NetherVines::grow`: `amount` blocks past the tip, each one older.
fn nether_vines_grow(cx: &mut Cx<'_>, pos: BlockPos, block: block::State, amount: i32) -> bool {
    let growth = vine_growth(block);
    let Some(tip) = seek(cx, pos, block, growth) else {
        return false;
    };
    let Some(tip_block) = cx.get(tip) else {
        return false;
    };
    let mut age = vine_age(tip_block).unwrap_or(0);
    let mut tx = Tx::default();
    let mut at = tip;
    for _ in 0..amount {
        let Some(next) = at.step(growth) else {
            break;
        };
        if !cx.get(next).is_some_and(block::State::can_be_replaced) {
            break;
        }
        age = (age + 1).min(MAX_VINE_AGE);
        if let Some(s) = props::with_int(tip_block, nether_vines_age(block), age) {
            tx.add(next, s);
        }
        at = next;
    }
    !tx.is_empty() && tx.apply(cx, tip)
}

/// PMMP `ChorusFlower::onRandomTick`.
fn chorus_flower(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let Some(age) = props::int(block, "age").filter(|a| *a < 5) else {
        return;
    };
    let air = |cx: &Cx<'_>, p: Option<BlockPos>| {
        p.and_then(|p| cx.get(p))
            .is_some_and(|s| s.kind() == Kind::Air)
    };
    let all_empty = |cx: &Cx<'_>, p: BlockPos, except: Option<Face>| {
        HORIZONTAL
            .into_iter()
            .filter(|f| Some(*f) != except)
            .all(|f| air(cx, p.step(f)))
    };
    // Scan the stem.
    let mut stem = 0;
    let mut end_stone = false;
    for y in 1..=5 {
        match pos.offset(0, -y, 0).and_then(|p| cx.get(p)) {
            Some(s) if s.kind() == Kind::ChorusPlant => stem += 1,
            Some(s) => {
                end_stone = s.kind() == Kind::EndStone;
                break;
            }
            None => break,
        }
    }
    let mut rng = cx.rng();
    let up = pos.step(Face::Up);
    let mut grows_up = air(cx, up)
        && up.is_some_and(|u| {
            u.step(Face::Up)
                .is_none_or(|uu| cx.get(uu).is_none_or(|s| s.kind() == Kind::Air))
        });
    if grows_up
        && !air(cx, pos.step(Face::Down))
        && (stem >= 5 || (stem > 1 && stem > rng.range(0, if end_stone { 4 } else { 3 })))
    {
        grows_up = false;
    }
    grows_up = grows_up && up.is_some_and(|u| all_empty(cx, u, None));
    let mut tx = Tx::default();
    let older = |by: i32| props::with_int(block, "age", (age + by).min(5));
    if grows_up {
        if let (Some(u), Some(s)) = (up, older(0)) {
            tx.add(u, s);
        }
    } else {
        let attempts = rng.range(0, if end_stone { 4 } else { 3 });
        // One bit per side of `HORIZONTAL`.
        let mut visited = 0u8;
        for _ in 0..attempts {
            let index = rng.below(4);
            let Some(face) = HORIZONTAL.get(index as usize).copied() else {
                continue;
            };
            let bit = 1 << index;
            if visited & bit != 0 {
                continue;
            }
            visited |= bit;
            let Some(side_pos) = pos.step(face) else {
                continue;
            };
            if air(cx, Some(side_pos))
                && air(cx, side_pos.step(Face::Down))
                && all_empty(cx, side_pos, Some(face.opposite()))
                && let Some(s) = older(1)
            {
                tx.add(side_pos, s);
            }
        }
    }
    if tx.is_empty() {
        if let Some(dead) = props::with_int(block, "age", 5) {
            cx.set(pos, dead);
        }
    } else {
        tx.add(pos, Kind::ChorusPlant.default_state());
        tx.apply(cx, pos);
    }
}

/// PMMP `TallGrass::onInteract`: grass and ferns become their two-block forms.
fn tall_grass(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) -> bool {
    let Some(up) = pos.step(Face::Up) else {
        return false;
    };
    if cx.get(up).is_none_or(|u| u.kind() != Kind::Air) {
        return false;
    }
    let double = match block.kind() {
        Kind::TallGrass => Kind::DoubleTallgrass,
        Kind::Fern => Kind::LargeFern,
        _ => return false,
    };
    let half = |top: bool| props::with_flag(double.default_state(), "upper_block_bit", top);
    let (Some(bottom), Some(top)) = (half(false), half(true)) else {
        return false;
    };
    cx.set(pos, bottom);
    cx.set(up, top);
    true
}

/// A random permutation of `items` (PHP `shuffle`).
fn shuffle<T>(rng: &mut Random, items: &mut [T]) {
    for i in (1..items.len()).rev() {
        let j = rng.below(u32::try_from(i + 1).unwrap_or(u32::MAX)) as usize;
        items.swap(i, j);
    }
}

/// PMMP `GlowLichen::onInteract` with bone meal: spreads one face somewhere near.
fn glow_lichen(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) -> bool {
    let Some(mut faces) = props::faces(block) else {
        return false;
    };
    if faces.is_empty() {
        return false;
    }
    let mut rng = cx.rng();
    shuffle(&mut rng, &mut faces);
    let mut methods = [0u8, 1, 2];
    shuffle(&mut rng, &mut methods);
    for source in faces {
        for &method in &methods {
            let mut spread_faces: Vec<Face> = Face::ALL
                .into_iter()
                .filter(|f| !props::same_axis(*f, source))
                .collect();
            shuffle(&mut rng, &mut spread_faces);
            let done = spread_faces.into_iter().any(|spread| {
                let (replace, toward) = match method {
                    0 => (
                        pos.step(source).and_then(|p| p.step(spread)),
                        spread.opposite(),
                    ),
                    1 => (pos.step(spread), source),
                    _ => {
                        if props::faces(block).is_some_and(|f| f.contains(&spread)) {
                            return false;
                        }
                        (Some(pos), spread)
                    }
                };
                replace.is_some_and(|r| lichen_spread(cx, pos, r, toward))
            });
            if done {
                return true;
            }
        }
    }
    true
}

/// PMMP `GlowLichen::spread`: the lichen at `source` spreads a face to `replace` on the side
/// `toward`, if a full face is there.
fn lichen_spread(cx: &mut Cx<'_>, source: BlockPos, replace: BlockPos, toward: Face) -> bool {
    let Some(support) = replace.step(toward).and_then(|p| cx.get(p)) else {
        return false;
    };
    if support.support(toward.opposite()) != block::SupportType::Full {
        return false;
    }
    let Some(current) = cx.get(replace) else {
        return false;
    };
    let base = if current.kind() == Kind::GlowLichen {
        if props::faces(current).is_some_and(|f| f.contains(&toward)) {
            return false;
        }
        current
    } else if current.kind() == Kind::Air {
        Kind::GlowLichen.default_state()
    } else {
        return false;
    };
    let bits =
        props::int(base, "multi_face_direction_bits").unwrap_or(0) | props::multi_face_bit(toward);
    props::with_int(base, "multi_face_direction_bits", bits)
        .is_some_and(|s| cx.spread(replace, s, source))
}

/// PMMP `SmallDripleaf::grow`: a big dripleaf two to five blocks high.
fn small_dripleaf(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) -> bool {
    let top = props::flag(block, "upper_block_bit") == Some(true);
    let Some(bottom) = (if top { pos.step(Face::Down) } else { Some(pos) }) else {
        return false;
    };
    if cx.get(bottom).is_none_or(|b| b.kind() != block.kind()) {
        return false;
    }
    let Some(facing) = props::cardinal(block) else {
        return false;
    };
    let can_grow_to = |cx: &Cx<'_>, p: Option<BlockPos>| {
        p.and_then(|p| cx.get(p))
            .is_some_and(|s| s.kind() == block.kind() || s.kind() == Kind::Air)
    };
    let height = cx.rng().range(2, 5);
    let mut grown = 0;
    let mut tx = Tx::default();
    for i in 0..height {
        let at = bottom.offset(0, i, 0);
        if !can_grow_to(cx, at) {
            break;
        }
        grown += 1;
        let kind = if grown < height && can_grow_to(cx, at.and_then(|a| a.step(Face::Up))) {
            Kind::BigDripleafStem
        } else {
            Kind::BigDripleafHead
        };
        if let (Some(at), Some(s)) = (at, dripleaf(kind, facing)) {
            tx.add(at, s);
        }
    }
    grown > 1 && tx.apply(cx, bottom)
}

/// A big dripleaf part facing `facing`, stable.
fn dripleaf(kind: Kind, facing: Face) -> Option<block::State> {
    props::with_text(
        kind.default_state(),
        "minecraft:cardinal_direction",
        props::face_name(facing),
    )
}

/// PMMP `BaseBigDripleaf::grow`: the head moves one block up.
fn big_dripleaf(cx: &mut Cx<'_>, pos: BlockPos) -> bool {
    let mut at = pos;
    let head = loop {
        match cx.get(at) {
            Some(s) if s.class() == Class::BigDripleafHead => break (at, s),
            Some(s) if s.class() == Class::BigDripleafStem => {
                let Some(next) = at.step(Face::Up) else {
                    return false;
                };
                at = next;
            }
            _ => return false,
        }
    };
    let (head_pos, head_block) = head;
    let Some(up) = head_pos.step(Face::Up) else {
        return false;
    };
    if cx.get(up).is_none_or(|u| u.kind() != Kind::Air) {
        return false;
    }
    let Some(facing) = props::cardinal(head_block) else {
        return false;
    };
    let (Some(stem), Some(new_head)) = (
        dripleaf(Kind::BigDripleafStem, facing),
        dripleaf(Kind::BigDripleafHead, facing),
    ) else {
        return false;
    };
    let mut tx = Tx::default();
    tx.add(head_pos, stem);
    tx.add(up, new_head);
    tx.apply(cx, head_pos)
}

/// PMMP `BigDripleafHead::onScheduledUpdate`: a tilted leaf tilts further, then comes back.
pub(super) fn dripleaf_tilt(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let next = match props::text(block, "big_dripleaf_tilt") {
        Some("full_tilt") => ("none", None),
        Some("unstable") => ("partial_tilt", Some(10)),
        Some("partial_tilt") => ("full_tilt", Some(100)),
        _ => return,
    };
    if let Some(s) = props::with_text(block, "big_dripleaf_tilt", next.0)
        && cx.set(pos, s)
        && let Some(delay) = next.1
    {
        cx.schedule(pos, delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bamboo_heights_follow_pmmp_offset_hash() {
        // PMMP `Bamboo::getMaxHeight` for a few columns (gmp arithmetic, computed by hand).
        for (x, z) in [(0, 0), (1, 0), (0, 1), (-7, 13), (1000, -1000)] {
            let h = bamboo_max_height(x, z);
            assert!((12..=16).contains(&h), "{x} {z} {h}");
        }
        assert_eq!(bamboo_max_height(0, 0), 12);
    }

    #[test]
    fn pitcher_halves_are_the_double_crop_states() {
        for growth in [3, 4] {
            for top in [false, true] {
                let scanned = block::State::all().find(|s| {
                    s.kind() == Kind::DoublePitcherCrop
                        && age(*s) == Some(growth)
                        && props::flag(*s, "upper_block_bit") == Some(top)
                });
                assert!(scanned.is_some());
                assert_eq!(pitcher_half(growth, top), scanned, "{growth} {top}");
            }
        }
    }
}
