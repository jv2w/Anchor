//! Plants (PMMP `onRandomTick`, `onNearbyBlockChange` and bone meal `onInteract` of crops, stems,
//! sugar cane, cactus, cocoa, nether wart, bamboo, sweet berries, pitcher and torchflower crops,
//! cave and nether vines, chorus flowers, saplings, dripleaves, glow lichen, pink petals, grass,
//! azalea and fungi).
//!
//! A rule that writes its own block after a growth writes it only while the block is still the
//! one the rule read: a listener that changed it keeps its block.

use super::props;
use super::support::{self, side};
use super::tree::{self, Tx};
use super::{Rule, pick};
use crate::block::{self, Class, Face, ItemForm, Kind, TypeTag};
use crate::item::Stack;
use crate::random::Random;
use crate::world::{BlockPos, TreeType};

/// The crop age property of the classes that have one.
const GROWTH: &str = "growth";
/// The oldest sweet berry bush (PMMP `SweetBerryBush::STAGE_MATURE`).
const MAX_BERRY_GROWTH: i32 = 3;
/// The oldest chorus flower, which grows no more (PMMP `ChorusFlower::MAX_AGE`).
const MAX_CHORUS_AGE: i32 = 5;
/// The oldest crop and stem (PMMP `Crops::MAX_AGE`, `Stem::MAX_AGE`).
const MAX_CROP_GROWTH: i32 = 7;
/// The most age of cave and nether vines (PMMP `CaveVines::MAX_AGE`, `NetherVines::MAX_AGE`).
pub(in crate::tick) const MAX_VINE_AGE: i32 = 25;
/// The least full light bamboo and bamboo saplings grow in (PMMP `Bamboo::onRandomTick`,
/// `BambooSapling::onRandomTick`).
const MIN_BAMBOO_LIGHT: u8 = 9;
/// The least potential light a crop grows in (PMMP `CropGrowthHelper::MIN_LIGHT_LEVEL`).
const MIN_CROP_LIGHT: u8 = 9;

/// Runs a random tick of a plant.
pub(super) fn on_random(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    match block.class() {
        Class::Bamboo => bamboo(rule, pos, block),
        Class::BambooSapling => bamboo_sapling(rule, pos, block),
        Class::Beetroot | Class::Carrot | Class::Potato | Class::Wheat => {
            if age(block).is_some_and(|a| a < MAX_CROP_GROWTH) && can_grow(rule, pos, block) {
                grow_age(rule, pos, block, 1);
            }
        }
        Class::Cactus => cactus(rule, pos, block),
        Class::CaveVines => cave_vines(rule, pos, block),
        Class::ChorusFlower => chorus_flower(rule, pos, block),
        Class::CocoaBlock => {
            if rule.rng().range(1, 5) == 1 {
                cocoa_grow(rule, pos, block);
            }
        }
        Class::DoublePitcherCrop => {
            if props::flag(block, "upper_block_bit") == Some(false) && can_grow(rule, pos, block) {
                double_pitcher_grow(rule, pos, block);
            }
        }
        Class::MelonStem | Class::PumpkinStem => stem(rule, pos, block),
        Class::NetherVines => {
            let young = vine_age(block).is_some_and(|a| a < MAX_VINE_AGE);
            if young && rule.rng().range(1, 10) == 1 {
                let growth = vine_growth(block);
                if pos
                    .step(growth)
                    .and_then(|p| rule.get(p))
                    .is_some_and(block::State::can_be_replaced)
                {
                    nether_vines_grow(rule, pos, block, 1);
                }
            }
        }
        Class::NetherWartPlant => {
            if props::int(block, "age").is_some_and(|a| a < 3)
                && rule.rng().range(0, 10) == 0
                && let Some(s) = aged(block, "age", 1)
            {
                rule.grow(pos, s, pos);
            }
        }
        Class::PitcherCrop => {
            if can_grow(rule, pos, block) {
                pitcher_grow(rule, pos, block);
            }
        }
        Class::Sapling => sapling(rule, pos, block),
        Class::Sugarcane => sugarcane(rule, pos, block),
        Class::SweetBerryBush => {
            if age(block).is_some_and(|a| a < MAX_BERRY_GROWTH) && rule.rng().range(0, 2) == 1 {
                grow_age(rule, pos, block, 1);
            }
        }
        Class::TorchflowerCrop if can_grow(rule, pos, block) => {
            torchflower_grow(rule, pos, block);
        }
        _ => {}
    }
}

/// Grows a plant with bone meal; whether the bone meal is used up. Ripe sweet berries and cave
/// vines with berries take none: the interaction rules pick them first.
pub(super) fn fertilize(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) -> bool {
    match block.class() {
        Class::Azalea => {
            if rule.rng().range(1, 100) <= 45 {
                tree::grow(rule, pos, TreeType::Azalea);
            }
            true
        }
        Class::Bamboo => {
            let Some(top) = seek(rule, pos, block, Face::Up) else {
                return false;
            };
            let Some(top_block) = rule.get(top) else {
                return false;
            };
            let amount = rule.rng().range(1, 2);
            bamboo_grow(
                rule,
                top,
                top_block,
                bamboo_max_height(top.x, top.z),
                amount,
            )
        }
        Class::BambooSapling => bamboo_sapling_grow(rule, pos),
        Class::Beetroot
        | Class::Carrot
        | Class::MelonStem
        | Class::Potato
        | Class::PumpkinStem
        | Class::Wheat => {
            let Some(a) = age(block).filter(|a| *a < MAX_CROP_GROWTH) else {
                return false;
            };
            let next = (a + rule.rng().range(2, 5)).min(MAX_CROP_GROWTH);
            props::with_int(block, GROWTH, next).is_some_and(|s| rule.grow(pos, s, pos))
        }
        Class::BigDripleafHead | Class::BigDripleafStem => big_dripleaf(rule, pos),
        Class::CaveVines => {
            let head = side(rule, pos, Face::Down).is_some_and(|d| d.kind() != block.kind());
            !berries(block)
                && cave_vine_state(block, vine_age(block).unwrap_or(0), true, head)
                    .is_some_and(|s| rule.grow(pos, s, pos))
        }
        Class::CocoaBlock => cocoa_grow(rule, pos, block),
        Class::DoublePitcherCrop => double_pitcher_grow(rule, pos, block),
        Class::GlowLichen => glow_lichen(rule, pos, block),
        Class::NetherFungus => fungus(rule, pos, block),
        Class::NetherVines => {
            let amount = rule.rng().range(1, 5);
            nether_vines_grow(rule, pos, block, amount)
        }
        Class::PinkPetals => {
            let Some(count) = props::int(block, GROWTH) else {
                return false;
            };
            if count < 3 {
                props::with_int(block, GROWTH, count + 1).is_some_and(|s| rule.grow(pos, s, pos))
            } else {
                drop_self(rule, pos, block)
            }
        }
        Class::PitcherCrop => pitcher_grow(rule, pos, block),
        Class::Sapling => tree::grow_sapling(rule, pos, block),
        Class::SmallDripleaf => small_dripleaf(rule, pos, block),
        Class::Sugarcane => {
            let Some(bottom) = seek(rule, pos, block, Face::Down) else {
                return false;
            };
            sugarcane_grow(rule, bottom)
        }
        Class::SweetBerryBush => {
            age(block).is_some_and(|a| a < MAX_BERRY_GROWTH) && grow_age(rule, pos, block, 1)
        }
        Class::TallGrass => tall_grass(rule, pos, block),
        Class::TorchflowerCrop => torchflower_grow(rule, pos, block),
        _ => false,
    }
}

/// Runs PMMP's `Sapling::onRandomTick`: in light of 8 or more, one in seven gets ready, then
/// grows.
fn sapling(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if rule.full_light(pos) < 8 || rule.rng().range(1, 7) != 1 {
        return;
    }
    if props::flag(block, "age_bit") == Some(true) {
        tree::grow_sapling(rule, pos, block);
    } else if let Some(ready) = props::with_flag(block, "age_bit", true) {
        rule.set(pos, ready);
    }
}

/// Runs PMMP's `NetherFungus::onInteract`: on its own nylium, 40 in 100 grow a huge fungus; the
/// bone meal is used either way.
fn fungus(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) -> bool {
    let (nylium, kind) = if block.kind() == Kind::CrimsonFungus {
        (Kind::CrimsonNylium, TreeType::Crimson)
    } else {
        (Kind::WarpedNylium, TreeType::Warped)
    };
    if side(rule, pos, Face::Down).is_some_and(|d| d.kind() == nylium)
        && rule.rng().range(1, 100) <= 40
    {
        tree::grow(rule, pos, kind);
    }
    true
}

/// Runs PMMP's `Stem::onRandomTick`.
fn stem(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if stem_facing(block) != Some(Face::Up) || !can_grow(rule, pos, block) {
        return;
    }
    if age(block).is_some_and(|a| a < MAX_CROP_GROWTH) {
        grow_age(rule, pos, block, 1);
        return;
    }
    let fruit = stem_fruit(block);
    for face in Face::HORIZONTAL {
        match side(rule, pos, face) {
            Some(s) if s.kind() == fruit => return,
            Some(_) => {}
            None => return,
        }
    }
    let face = pick(&mut rule.rng(), &Face::HORIZONTAL);
    let Some(at) = pos.step(face) else {
        return;
    };
    let empty = rule.get(at).is_some_and(|s| s.kind() == Kind::Air);
    let soil = at
        .step(Face::Down)
        .and_then(|p| rule.get(p))
        .is_some_and(|s| s.has_type_tag(TypeTag::Dirt));
    if empty
        && soil
        && rule.grow(at, fruit.default_state(), pos)
        && let Some(attached) = stem_with(block, face)
    {
        rule.set(pos, attached);
    }
}

/// Runs PMMP's `Stem::onNearbyBlockChange`: a stem whose fruit is gone points up again, then the
/// crop support check.
pub(super) fn stem_on_nearby(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    let mut block = block;
    if let Some(face) = stem_facing(block)
        && face != Face::Up
        && side(rule, pos, face).is_some_and(|s| s.kind() != stem_fruit(block))
        && let Some(up) = stem_with(block, Face::Up)
    {
        rule.set(pos, up);
        block = up;
    }
    if support::supported(rule, pos, block) == Some(false) {
        rule.destroy(pos);
    }
}

/// Runs PMMP's `Sugarcane::onNearbyBlockChange`.
pub(super) fn sugarcane_on_nearby(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    let Some(below) = pos.step(Face::Down) else {
        return;
    };
    let Some(down) = rule.loaded_block(below) else {
        return;
    };
    // Cane on cane always stays: it needs no water and is supported.
    if down.kind() == block.kind() {
        return;
    }
    let Some(water) = near_water(rule, below) else {
        return;
    };
    if !water || support::supported(rule, pos, block) == Some(false) {
        rule.destroy(pos);
    }
}

/// Runs PMMP's `Sugarcane::onRandomTick`.
fn sugarcane(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    let Some(below) = pos.step(Face::Down) else {
        return;
    };
    let Some(down) = rule.loaded_block(below) else {
        return;
    };
    if down.kind() == block.kind() {
        return;
    }
    match near_water(rule, below) {
        Some(false) => rule.destroy(pos),
        Some(true) => {
            if props::int(block, "age") == Some(15) {
                sugarcane_grow(rule, pos);
            } else if let Some(s) = aged(block, "age", 1) {
                rule.set(pos, s);
            }
        }
        None => {}
    }
}

/// Runs PMMP's `Sugarcane::grow`: up to two blocks of cane grow above `bottom`, then the cane at
/// `bottom` goes back to age 0. Whether a block grew.
fn sugarcane_grow(rule: &mut Rule<'_>, bottom: BlockPos) -> bool {
    let Some(cane) = rule.get(bottom) else {
        return false;
    };
    let mut grew = false;
    for y in 1..3 {
        let Some(at) = bottom.offset(0, y, 0) else {
            break;
        };
        let Some(b) = rule.get(at) else {
            break;
        };
        if b.kind() == Kind::Air {
            if rule.grow(at, Kind::Sugarcane.default_state(), bottom) {
                grew = true;
            } else {
                break;
            }
        } else if b.kind() != cane.kind() {
            break;
        }
    }
    if rule.get(bottom) == Some(cane)
        && let Some(young) = props::with_int(cane, "age", 0)
    {
        rule.set(bottom, young);
    }
    grew
}

/// Runs PMMP's `Cactus::onRandomTick`.
fn cactus(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    let Some(up_pos) = pos.step(Face::Up) else {
        return;
    };
    if rule.get(up_pos).is_none_or(|u| u.kind() != Kind::Air) {
        return;
    }
    let mut height = 1;
    while height < 3 {
        match pos.offset(0, -height, 0).and_then(|p| rule.get(p)) {
            Some(s) if s.kind() == block.kind() => height += 1,
            _ => break,
        }
    }
    let Some(age) = props::int(block, "age") else {
        return;
    };
    if age == 9 {
        let mut flower = true;
        for face in Face::HORIZONTAL {
            if up_pos
                .step(face)
                .and_then(|p| rule.get(p))
                .is_none_or(block::State::is_solid)
            {
                flower = false;
                break;
            }
        }
        if flower {
            let chance = if height >= 3 { 25 } else { 10 };
            if rule.rng().range(1, 100) <= chance {
                if rule.grow(up_pos, Kind::CactusFlower.default_state(), pos)
                    && let Some(young) = props::with_int(block, "age", 0)
                {
                    rule.set_quietly(pos, young);
                }
                return;
            }
        }
    }
    let next = if age == 15 {
        if height < 3 {
            rule.grow(up_pos, Kind::Cactus.default_state(), pos);
        }
        0
    } else {
        age + 1
    };
    if rule.get(pos) == Some(block)
        && let Some(s) = props::with_int(block, "age", next)
    {
        rule.set_quietly(pos, s);
    }
}

/// Runs PMMP's `CocoaBlock::grow`.
fn cocoa_grow(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) -> bool {
    props::int(block, "age").is_some_and(|a| a < 2)
        && aged(block, "age", 1).is_some_and(|s| rule.grow(pos, s, pos))
}

/// Tells whether a cocoa pod's log is still there (PMMP `CocoaBlock::canAttachTo`: jungle wood).
pub(super) fn cocoa_attached(rule: &Rule<'_>, pos: BlockPos, block: block::State) -> Option<bool> {
    let face = props::facing(block)?.opposite();
    let log = side(rule, pos, face)?;
    Some(matches!(log.kind(), Kind::JungleLog | Kind::JungleWood))
}

/// Runs PMMP's `Bamboo::onRandomTick`.
fn bamboo(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if props::flag(block, "age_bit") == Some(true) {
        let Some(unready) = props::with_flag(block, "age_bit", false) else {
            return;
        };
        if (rule.full_light(pos) < MIN_BAMBOO_LIGHT
            || !bamboo_grow(rule, pos, unready, bamboo_max_height(pos.x, pos.z), 1))
            && rule.get(pos) == Some(block)
        {
            rule.set(pos, unready);
        }
    } else if pos
        .step(Face::Up)
        .and_then(|p| rule.get(p))
        .is_some_and(block::State::can_be_replaced)
        && let Some(r) = props::with_flag(block, "age_bit", true)
    {
        rule.set(pos, r);
    }
}

/// Runs PMMP's `Bamboo::grow`: the top of a stalk at `pos` grows `amount` blocks up to `max`
/// high.
fn bamboo_grow(
    rule: &mut Rule<'_>,
    pos: BlockPos,
    block: block::State,
    max: i32,
    amount: i32,
) -> bool {
    let Some(up) = pos.step(Face::Up) else {
        return false;
    };
    if !rule.get(up).is_some_and(block::State::can_be_replaced) {
        return false;
    }
    let mut height = 1;
    while pos
        .offset(0, -height, 0)
        .and_then(|p| rule.get(p))
        .is_some_and(|s| s.kind() == block.kind())
    {
        height += 1;
        if height >= max {
            return false;
        }
    }
    let new_height = height + amount;
    let leaves = |s: block::State, size: &str| props::with_string(s, "bamboo_leaf_size", size);
    let Some(mut stem) =
        props::with_flag(block, "age_bit", false).and_then(|s| leaves(s, "no_leaves"))
    else {
        return false;
    };
    if new_height >= 4
        && props::string(stem, "bamboo_stalk_thickness") == Some("thin")
        && let Some(thick) = props::with_string(stem, "bamboo_stalk_thickness", "thick")
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
    // The blocks go from the new top down.
    let mut tx = Tx::default();
    let mut dy = amount;
    for s in blocks {
        if let Some(at) = pos.offset(0, dy, 0) {
            tx.add(at, s);
        }
        dy -= 1;
    }
    tx.apply(rule, pos)
}

/// Works out PMMP's `Bamboo::getMaxHeight`: 12 to 16 by a hash of the column.
fn bamboo_max_height(x: i32, z: i32) -> i32 {
    let p1 = i64::from(z).wrapping_mul(0x6eb_fff5);
    let p2 = i64::from(x).wrapping_mul(0x2f_c20f);
    let xord = p1 ^ p2;
    let full = xord
        .wrapping_mul(0x285_b825)
        .wrapping_add(0xb)
        .wrapping_mul(xord);
    let low = full & 0xffff_ffff;
    #[expect(
        clippy::expect_used,
        reason = "invariant: the low half is not negative, so its remainder of 5 is 0 to 4"
    )]
    let offset = i32::try_from(low % 5).expect("a remainder of 5 fits i32");
    12 + offset
}

/// Runs PMMP's `BambooSapling::onRandomTick`.
fn bamboo_sapling(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if props::flag(block, "age_bit") == Some(true) {
        let Some(unready) = props::with_flag(block, "age_bit", false) else {
            return;
        };
        if (rule.full_light(pos) < MIN_BAMBOO_LIGHT || !bamboo_sapling_grow(rule, pos))
            && rule.get(pos) == Some(block)
        {
            rule.set(pos, unready);
        }
    } else if pos
        .step(Face::Up)
        .and_then(|p| rule.get(p))
        .is_some_and(block::State::can_be_replaced)
        && let Some(r) = props::with_flag(block, "age_bit", true)
    {
        rule.set(pos, r);
    }
}

/// Runs PMMP's `BambooSapling::grow`: bamboo with a small-leaved bamboo above.
fn bamboo_sapling_grow(rule: &mut Rule<'_>, pos: BlockPos) -> bool {
    let Some(up) = pos.step(Face::Up) else {
        return false;
    };
    if !rule.get(up).is_some_and(block::State::can_be_replaced) {
        return false;
    }
    let bamboo = Kind::Bamboo.default_state();
    let Some(small) = props::with_string(bamboo, "bamboo_leaf_size", "small_leaves") else {
        return false;
    };
    let mut tx = Tx::default();
    tx.add(pos, bamboo);
    tx.add(up, small);
    tx.apply(rule, pos)
}

/// Runs PMMP's `PitcherCrop::grow`: stages 0 to 2, then the two-block plant.
fn pitcher_grow(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) -> bool {
    match age(block) {
        Some(a) if a < 2 => grow_age(rule, pos, block, 1),
        Some(2) => {
            let Some(up) = pos.step(Face::Up) else {
                return false;
            };
            if rule.get(up).is_none_or(|u| u.kind() != Kind::Air) {
                return false;
            }
            let (Some(bottom), Some(top)) = (pitcher_half(3, false), pitcher_half(3, true)) else {
                return false;
            };
            let mut tx = Tx::default();
            tx.add(pos, bottom);
            tx.add(up, top);
            tx.apply(rule, pos)
        }
        _ => false,
    }
}

/// Runs PMMP's `DoublePitcherCrop::grow`.
fn double_pitcher_grow(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) -> bool {
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
    if rule
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
    tx.apply(rule, bottom)
}

/// Runs PMMP's `TorchflowerCrop::getNextState`: the crop gets ready, then turns into the
/// torchflower.
fn torchflower_grow(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) -> bool {
    let next = if age(block) == Some(1) {
        Some(Kind::Torchflower.default_state())
    } else {
        props::with_int(block, GROWTH, 1)
    };
    next.is_some_and(|s| rule.grow(pos, s, pos))
}

/// Runs PMMP's `CaveVines::onRandomTick`.
fn cave_vines(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    let Some(below) = pos.step(Face::Down) else {
        return;
    };
    let Some(down) = rule.get(below) else {
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
        rule.set(pos, s);
        block = s;
    }
    let mut rng = rule.rng();
    if age < MAX_VINE_AGE && rng.range(1, 10) == 1 && down.kind() == Kind::Air {
        let fruit = rng.range(1, 9) == 1;
        if let Some(s) = cave_vine_state(block, age + 1, fruit, false) {
            rule.grow(below, s, pos);
        }
    }
}

/// Runs PMMP's `NetherVines::grow`: `amount` blocks grow past the tip, each one older.
fn nether_vines_grow(rule: &mut Rule<'_>, pos: BlockPos, block: block::State, amount: i32) -> bool {
    let growth = vine_growth(block);
    let Some(tip) = seek(rule, pos, block, growth) else {
        return false;
    };
    let Some(tip_block) = rule.get(tip) else {
        return false;
    };
    let mut age = vine_age(tip_block).unwrap_or(0);
    let mut tx = Tx::default();
    let mut at = tip;
    for _ in 0..amount {
        let Some(next) = at.step(growth) else {
            break;
        };
        if !rule.get(next).is_some_and(block::State::can_be_replaced) {
            break;
        }
        age = (age + 1).min(MAX_VINE_AGE);
        if let Some(s) = props::with_int(tip_block, nether_vines_age(block), age) {
            tx.add(next, s);
        }
        at = next;
    }
    !tx.is_empty() && tx.apply(rule, tip)
}

/// Runs PMMP's `ChorusFlower::onRandomTick`.
fn chorus_flower(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    let Some(age) = props::int(block, "age").filter(|a| *a < MAX_CHORUS_AGE) else {
        return;
    };
    let air = |rule: &Rule<'_>, p: Option<BlockPos>| {
        p.and_then(|p| rule.get(p))
            .is_some_and(|s| s.kind() == Kind::Air)
    };
    let all_empty = |rule: &Rule<'_>, p: BlockPos, except: Option<Face>| {
        Face::HORIZONTAL
            .into_iter()
            .filter(|f| Some(*f) != except)
            .all(|f| air(rule, p.step(f)))
    };
    // Scan the stem.
    let mut stem = 0;
    let mut end_stone = false;
    for y in 1..=5 {
        match pos.offset(0, -y, 0).and_then(|p| rule.get(p)) {
            Some(s) if s.kind() == Kind::ChorusPlant => stem += 1,
            Some(s) => {
                end_stone = s.kind() == Kind::EndStone;
                break;
            }
            None => break,
        }
    }
    let mut rng = rule.rng();
    let up = pos.step(Face::Up);
    let mut grows_up = air(rule, up)
        && up.is_some_and(|u| {
            u.step(Face::Up)
                .is_none_or(|uu| rule.get(uu).is_none_or(|s| s.kind() == Kind::Air))
        });
    if grows_up
        && !air(rule, pos.step(Face::Down))
        && (stem >= 5 || (stem > 1 && stem > rng.range(0, if end_stone { 4 } else { 3 })))
    {
        grows_up = false;
    }
    grows_up = grows_up && up.is_some_and(|u| all_empty(rule, u, None));
    let mut tx = Tx::default();
    let older = |by: i32| props::with_int(block, "age", (age + by).min(MAX_CHORUS_AGE));
    if grows_up {
        if let (Some(u), Some(s)) = (up, older(0)) {
            tx.add(u, s);
        }
    } else {
        let attempts = rng.range(0, if end_stone { 4 } else { 3 });
        // One bit per face, so a side is tried once.
        let mut visited = 0_u8;
        for _ in 0..attempts {
            let face = pick(&mut rng, &Face::HORIZONTAL);
            let bit = 1 << face as u8;
            if visited & bit != 0 {
                continue;
            }
            visited |= bit;
            let Some(side_pos) = pos.step(face) else {
                continue;
            };
            if air(rule, Some(side_pos))
                && air(rule, side_pos.step(Face::Down))
                && all_empty(rule, side_pos, Some(face.opposite()))
                && let Some(s) = older(1)
            {
                tx.add(side_pos, s);
            }
        }
    }
    if tx.is_empty() {
        if let Some(dead) = props::with_int(block, "age", MAX_CHORUS_AGE) {
            rule.set(pos, dead);
        }
    } else {
        tx.add(pos, Kind::ChorusPlant.default_state());
        tx.apply(rule, pos);
    }
}

/// Runs PMMP's `TallGrass::onInteract`: grass and ferns become their two-block forms.
fn tall_grass(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) -> bool {
    let Some(up) = pos.step(Face::Up) else {
        return false;
    };
    if rule.get(up).is_none_or(|u| u.kind() != Kind::Air) {
        return false;
    }
    let double = match block.kind() {
        Kind::Fern => Kind::LargeFern,
        Kind::TallGrass => Kind::DoubleTallgrass,
        _ => return false,
    };
    let half = |top: bool| props::with_flag(double.default_state(), "upper_block_bit", top);
    let (Some(bottom), Some(top)) = (half(false), half(true)) else {
        return false;
    };
    rule.set(pos, bottom);
    rule.set(up, top);
    true
}

/// Runs PMMP's `GlowLichen::onInteract` with bone meal: the lichen spreads one face somewhere
/// near. Whether it spread: only then is the bone meal used up.
fn glow_lichen(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) -> bool {
    let Some(mut faces) = props::faces(block) else {
        return false;
    };
    if faces.is_empty() {
        return false;
    }
    let mut rng = rule.rng();
    shuffle(&mut rng, &mut faces);
    let mut methods = [0_u8, 1, 2];
    shuffle(&mut rng, &mut methods);
    for source in faces {
        for &method in &methods {
            let mut spread_faces: Vec<Face> = Face::ALL
                .into_iter()
                .filter(|f| props::Axis::of(*f) != props::Axis::of(source))
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
                replace.is_some_and(|r| lichen_spread(rule, pos, r, toward))
            });
            if done {
                return true;
            }
        }
    }
    false
}

/// Runs PMMP's `GlowLichen::spread`: the lichen at `source` spreads a face to `replace` on the
/// side `toward`, if a full face is there.
fn lichen_spread(rule: &mut Rule<'_>, source: BlockPos, replace: BlockPos, toward: Face) -> bool {
    let Some(support) = replace.step(toward).and_then(|p| rule.get(p)) else {
        return false;
    };
    if support.support(toward.opposite()) != block::SupportType::Full {
        return false;
    }
    let Some(current) = rule.get(replace) else {
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
        .is_some_and(|s| rule.spread(replace, s, source))
}

/// Runs PMMP's `SmallDripleaf::grow`: a big dripleaf two to five blocks high.
fn small_dripleaf(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) -> bool {
    let top = props::flag(block, "upper_block_bit") == Some(true);
    let Some(bottom) = (if top { pos.step(Face::Down) } else { Some(pos) }) else {
        return false;
    };
    if rule.get(bottom).is_none_or(|b| b.kind() != block.kind()) {
        return false;
    }
    let Some(facing) = props::facing(block) else {
        return false;
    };
    let can_grow_to = |rule: &Rule<'_>, p: Option<BlockPos>| {
        p.and_then(|p| rule.get(p))
            .is_some_and(|s| s.kind() == block.kind() || s.kind() == Kind::Air)
    };
    let height = rule.rng().range(2, 5);
    let mut grown = 0;
    let mut tx = Tx::default();
    for i in 0..height {
        let at = bottom.offset(0, i, 0);
        if !can_grow_to(rule, at) {
            break;
        }
        grown += 1;
        let kind = if grown < height && can_grow_to(rule, at.and_then(|a| a.step(Face::Up))) {
            Kind::BigDripleafStem
        } else {
            Kind::BigDripleafHead
        };
        if let (Some(at), Some(s)) = (at, dripleaf(kind, facing)) {
            tx.add(at, s);
        }
    }
    grown > 1 && tx.apply(rule, bottom)
}

/// Runs PMMP's `BaseBigDripleaf::grow`: the head moves one block up.
fn big_dripleaf(rule: &mut Rule<'_>, pos: BlockPos) -> bool {
    let mut at = pos;
    let head = loop {
        match rule.get(at) {
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
    if rule.get(up).is_none_or(|u| u.kind() != Kind::Air) {
        return false;
    }
    let Some(facing) = props::facing(head_block) else {
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
    tx.apply(rule, head_pos)
}

/// Runs PMMP's `BigDripleafHead::onScheduledUpdate`: a tilted leaf tilts further, then comes
/// back.
pub(super) fn dripleaf_tilt(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    let next = match props::string(block, "big_dripleaf_tilt") {
        Some("full_tilt") => ("none", None),
        Some("unstable") => ("partial_tilt", Some(10)),
        Some("partial_tilt") => ("full_tilt", Some(100)),
        _ => return,
    };
    if let Some(s) = props::with_string(block, "big_dripleaf_tilt", next.0)
        && rule.set(pos, s)
        && let Some(delay) = next.1
    {
        rule.schedule(pos, delay);
    }
}

/// Runs PMMP's `CropGrowthHelper::canGrow`: one chance in `25 / multiplier + 1` (the multiplier
/// from the farmland under the crop and around it, halved for crops of the same kind not in
/// spaced rows) and potential light of at least 9.
pub(super) fn can_grow(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) -> bool {
    let Some(bound) = growth_bound(rule, pos, block) else {
        return false;
    };
    rule.rng().range(0, bound) == 0 && rule.potential_light(pos) >= MIN_CROP_LIGHT
}

/// Works out `(int) (25 / multiplier)` of `CropGrowthHelper`, in eighths so the arithmetic is
/// exact: the multiplier is 1, plus 3 (wet) or 1 (dry) for the farmland below, plus 3/4 or 1/4
/// for each farmland around, halved for an improper arrangement.
pub(super) fn growth_bound(rule: &Rule<'_>, pos: BlockPos, block: block::State) -> Option<i32> {
    let below = pos.offset(0, -1, 0)?;
    let wet = |s: block::State| props::int(s, "moisturized_amount").unwrap_or(0) > 0;
    let mut eighths = 8;
    let farmland = rule.get(below)?;
    if farmland.kind() == Kind::Farmland {
        eighths += if wet(farmland) { 24 } else { 8 };
    }
    let (mut x_row, mut z_row, mut improper) = (false, false, false);
    for x in -1..=1 {
        for z in -1..=1 {
            if x == 0 && z == 0 {
                continue;
            }
            let next = rule.get(below.offset(x, 0, z)?)?;
            if next.kind() != Kind::Farmland {
                continue;
            }
            eighths += if wet(next) { 6 } else { 2 };
            if !improper && rule.get(pos.offset(x, 0, z)?)?.kind() == block.kind() {
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

/// Grows the block `by` stages older (PMMP `BlockEventHelper::grow`).
fn grow_age(rule: &mut Rule<'_>, pos: BlockPos, block: block::State, by: i32) -> bool {
    aged(block, GROWTH, by).is_some_and(|s| rule.grow(pos, s, pos))
}

/// Adds `by` to the block's integer property `name`.
fn aged(block: block::State, name: &str, by: i32) -> Option<block::State> {
    props::int(block, name).and_then(|v| props::with_int(block, name, v + by))
}

/// Reads the crop age (`growth`).
fn age(block: block::State) -> Option<i32> {
    props::int(block, GROWTH)
}

/// Drops one of the block's item at its position; false (nothing dropped) for a block without
/// an item or at the world's entity limit.
fn drop_self(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) -> bool {
    let ItemForm::Item(item) = block.item() else {
        return false;
    };
    rule.drop_item(pos, Stack::new(item, 1))
}

/// Finds the last block of the plant's kind from `pos` toward `face`.
fn seek(rule: &Rule<'_>, pos: BlockPos, block: block::State, face: Face) -> Option<BlockPos> {
    let mut at = pos;
    loop {
        let next = at.step(face)?;
        match rule.get(next) {
            Some(s) if s.kind() == block.kind() => at = next,
            _ => return Some(at),
        }
    }
}

/// Tells whether water or frosted ice is beside the block under sugar cane.
fn near_water(rule: &Rule<'_>, below: BlockPos) -> Option<bool> {
    for face in Face::HORIZONTAL {
        let s = rule.loaded_block(below.step(face)?)?;
        if matches!(s.kind(), Kind::FrostedIce | Kind::Water) {
            return Some(true);
        }
    }
    Some(false)
}

/// Tells the kind a stem grows.
fn stem_fruit(block: block::State) -> Kind {
    if block.class() == Class::MelonStem {
        Kind::Melon
    } else {
        Kind::Pumpkin
    }
}

/// Reads the side a stem is attached to (up: not attached).
fn stem_facing(block: block::State) -> Option<Face> {
    props::facing(block)
}

/// Makes a stem with the facing `face` (up: not attached).
fn stem_with(block: block::State, face: Face) -> Option<block::State> {
    props::with_facing(block, face)
}

/// Makes the two-block pitcher crop with `growth` (3 or 4) and half.
fn pitcher_half(growth: i32, top: bool) -> Option<block::State> {
    props::with_int(Kind::DoublePitcherCrop.default_state(), GROWTH, growth)
        .and_then(|s| props::with_flag(s, "upper_block_bit", top))
}

/// Reads the age of cave or nether vines.
fn vine_age(block: block::State) -> Option<i32> {
    props::int(block, "growing_plant_age")
        .or_else(|| props::int(block, "weeping_vines_age"))
        .or_else(|| props::int(block, "twisting_vines_age"))
}

/// Tells the age property of nether vines: weeping or twisting.
pub(in crate::tick) fn nether_vines_age(block: block::State) -> &'static str {
    if vine_growth(block) == Face::Down {
        "weeping_vines_age"
    } else {
        "twisting_vines_age"
    }
}

/// Tells the direction nether vines grow: weeping vines down, twisting vines up.
pub(super) fn vine_growth(block: block::State) -> Face {
    if block.kind() == Kind::WeepingVines {
        Face::Down
    } else {
        Face::Up
    }
}

/// Tells whether cave vines have berries.
fn berries(block: block::State) -> bool {
    block.name() != "minecraft:cave_vines"
}

/// Makes the cave vines state with `age`, berries and head (Bedrock has no head without
/// berries: PMMP `FlattenedCaveVinesVariant`).
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
    props::with_int(props::renamed(block, name)?, "growing_plant_age", age)
}

/// Shuffles `items` into a random order (PHP `shuffle`).
fn shuffle<T>(rng: &mut Random, items: &mut [T]) {
    for i in (1..items.len()).rev() {
        #[expect(
            clippy::expect_used,
            reason = "invariant: the shuffled lists hold at most six items"
        )]
        let j = rng.below(u32::try_from(i + 1).expect("a list length fits u32")) as usize;
        items.swap(i, j);
    }
}

/// Makes a big dripleaf part facing `facing`, stable.
fn dripleaf(kind: Kind, facing: Face) -> Option<block::State> {
    props::with_facing(kind.default_state(), facing)
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
