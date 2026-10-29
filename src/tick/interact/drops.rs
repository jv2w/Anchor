//! What a broken block drops and the experience it gives (PMMP `Block::getDrops`,
//! `getDropsForCompatibleTool`, `getDropsForIncompatibleTool`, `getSilkTouchDrops`,
//! `getXpDropForTool` and every class override, `FortuneDropHelper`, `Explosion::explodeB`).
//!
//! PMMP picks a method by the class of the block, leaf first; [`resolve`] walks the PMMP class
//! chain of the state's kind the same way, so a subclass's rule wins over its parent's. Random
//! amounts come from the caller's engine stream; PMMP draws from PHP's global generator.

use crate::block::{Class, ItemForm, Kind, PropertyValue, State};
use crate::item::{self, Enchantment, Stack};
use crate::random::Random;

use super::props;

/// What a block drops: item stacks, and how many of the block's own item only its block entity
/// can name (a bed's colour, a banner's patterns: `ItemForm::BlockEntity`).
#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct Drops {
    /// The stacks, none empty.
    pub(super) items: Vec<Stack>,
    /// Copies of the block's own item that the block entity describes.
    pub(super) from_block_entity: u8,
}

impl Drops {
    /// Adds `count` of `item` (nothing for 0, as PMMP drops no empty stack).
    fn add(&mut self, item: item::Type, count: u32) {
        if count > 0 {
            let count = u8::try_from(count).unwrap_or(u8::MAX);
            self.items.push(Stack::new(item, count));
        }
    }

    /// Adds `count` of a vanilla item.
    fn add_kind(&mut self, kind: item::Kind, count: u32) {
        self.add(kind.default_type(), count);
    }

    /// Adds `count` of the item form of `state` (PMMP `asItem()->setCount`).
    fn add_own(&mut self, state: State, count: u32) {
        match state.item() {
            ItemForm::Item(item) => self.add(item, count),
            ItemForm::BlockEntity => {
                self.from_block_entity = self
                    .from_block_entity
                    .saturating_add(u8::try_from(count).unwrap_or(u8::MAX));
            }
            ItemForm::None => {}
        }
    }

    /// Adds `count` of the item form of a block kind's default state.
    fn add_block(&mut self, kind: Kind, count: u32) {
        self.add_own(kind.default_state(), count);
    }
}

/// The level of an enchantment on a stack, 0 if it has none.
pub(in crate::tick) fn level(stack: &Stack, enchantment: Enchantment) -> u16 {
    stack.enchantment_level(enchantment).unwrap_or(0)
}

/// Whether the item is shears (PMMP `getBlockToolType() & SHEARS`).
fn is_shears(tool: &Stack) -> bool {
    tool.item().is_some_and(|t| {
        t.block_tool_type()
            .intersects(crate::block::ToolType::SHEARS)
    })
}

/// Whether `tool` is a compatible tool for `state` (PMMP `BlockBreakInfo::isToolCompatible`).
pub(super) fn is_compatible(state: State, tool: &Stack) -> bool {
    let (tool_type, level) = tool.item().map_or((crate::block::ToolType::NONE, 0), |t| {
        (t.block_tool_type(), t.block_tool_harvest_level())
    });
    state.break_info().is_tool_compatible(tool_type, level)
}

/// The rule of the first class in the state's PMMP class chain (leaf first) that `pick` knows,
/// as PHP resolves an overridden method; `None` if no class has one (only for methods
/// [`Class::Block`] lacks).
fn resolve<T>(state: State, pick: impl Fn(Class) -> Option<T>) -> Option<T> {
    state.kind().classes().iter().find_map(|c| pick(*c))
}

/// PMMP `Block::getDrops($tool)` for the state, with the class overrides of `getDrops`.
pub(super) fn drops(state: State, tool: &Stack, rng: &mut Random) -> Drops {
    /// The `getDrops` overrides.
    #[derive(Clone, Copy)]
    enum Rule {
        /// `Block::getDrops`.
        Base,
        /// Only one half drops (beds' heads, doors' bottoms, double plants' tops).
        Half(bool),
        /// Only shears get anything (glow lichen, vines).
        ShearsOnly,
        /// Nothing (nether portals, unknown blocks).
        Nothing,
    }
    let rule = resolve(state, |c| match c {
        Class::Bed => Some(Rule::Half(
            props::flag(state, "head_piece_bit") == Some(true),
        )),
        Class::Door => Some(Rule::Half(
            props::flag(state, "upper_block_bit") != Some(true),
        )),
        Class::DoublePlant => Some(Rule::Half(
            props::flag(state, "upper_block_bit") == Some(true),
        )),
        Class::GlowLichen | Class::Vine => Some(Rule::ShearsOnly),
        Class::NetherPortal => Some(Rule::Nothing),
        // Lectern adds its book, which its block entity holds (block_entity::drops adds it to a broken block's drops).
        Class::Block => Some(Rule::Base),
        _ => None,
    })
    .unwrap_or(Rule::Base);
    let mut out = Drops::default();
    match rule {
        Rule::Base | Rule::Half(true) => base(state, tool, rng, &mut out),
        Rule::ShearsOnly if is_shears(tool) => compatible(state, tool, rng, &mut out),
        Rule::Half(false) | Rule::ShearsOnly | Rule::Nothing => {}
    }
    out
}

/// PMMP `Block::getDrops`: the compatible tool's drops (Silk Touch's where it applies), else the
/// incompatible tool's.
fn base(state: State, tool: &Stack, rng: &mut Random, out: &mut Drops) {
    if is_compatible(state, tool) {
        if state.is_affected_by_silk_touch() && level(tool, Enchantment::SilkTouch) > 0 {
            silk_touch(state, out);
        } else {
            compatible(state, tool, rng, out);
        }
    } else {
        incompatible(state, tool, rng, out);
    }
}

/// The drops of an explosion of the state (PMMP `Explosion::explodeB`): the compatible tool's
/// drops for an empty hand where the block is explosion harvestable, else `getDrops` by hand.
pub(super) fn explosion(state: State, rng: &mut Random) -> Drops {
    if state.break_info().is_explosion_harvestable() {
        let mut out = Drops::default();
        compatible(state, &Stack::EMPTY, rng, &mut out);
        out
    } else {
        drops(state, &Stack::EMPTY, rng)
    }
}

/// PMMP `FortuneDropHelper::weighted`.
fn weighted(tool: &Stack, rng: &mut Random, min: u32, max_base: u32) -> u32 {
    let fortune = u32::from(level(tool, Enchantment::Fortune));
    let max = if fortune > 0 && rng.unit() > 2.0 / f64::from(fortune + 2) {
        max_base.saturating_mul(fortune + 1)
    } else {
        max_base
    };
    range(rng, min, max)
}

/// PMMP `FortuneDropHelper::binomial`: `min` plus a success per roll, `min_rolls + fortune`
/// rolls.
fn binomial(fortune: u16, rng: &mut Random, min: u32, min_rolls: u32, chance: f64) -> u32 {
    let rolls = min_rolls + u32::from(fortune);
    (0..rolls).fold(min, |n, _| n + u32::from(rng.unit() < chance))
}

/// PMMP `FortuneDropHelper::discrete`.
fn discrete(tool: &Stack, rng: &mut Random, min: u32, max_base: u32) -> u32 {
    let max = max_base + u32::from(level(tool, Enchantment::Fortune));
    range(rng, min, max)
}

/// PMMP `FortuneDropHelper::bonusChanceDivisor`: one in `base - fortune × per_level` (at least
/// one).
fn bonus_divisor(tool: &Stack, rng: &mut Random, base: u32, per_level: u32) -> bool {
    let fortune = u32::from(level(tool, Enchantment::Fortune));
    let divisor = base
        .saturating_sub(fortune.saturating_mul(per_level))
        .max(1);
    rng.below(divisor) == 0
}

/// PMMP `FortuneDropHelper::bonusChanceFixed`.
fn bonus_fixed(tool: &Stack, rng: &mut Random, base: f64, per_level: f64) -> bool {
    let fortune = f64::from(level(tool, Enchantment::Fortune));
    rng.unit() < crate::math::min(1.0, base + fortune * per_level)
}

/// A number in `lo..=hi` (PHP `mt_rand`).
fn range(rng: &mut Random, lo: u32, hi: u32) -> u32 {
    if hi <= lo {
        return lo;
    }
    lo + rng.below(hi - lo + 1)
}

/// An integer property as a count (0 when missing or negative).
fn count_of(state: State, name: &str) -> u32 {
    props::int(state, name).map_or(0, |v| u32::try_from(v).unwrap_or(0))
}

/// A crop's age (Bedrock `growth`).
fn growth(state: State) -> u32 {
    count_of(state, "growth")
}

/// PMMP `getDropsForCompatibleTool` with its class overrides.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one match arm per PMMP class keeps the port side by side with PMMP"
)]
fn compatible(state: State, tool: &Stack, rng: &mut Random, out: &mut Drops) {
    use item::Kind as I;
    // Anonymous classes of VanillaBlocks (stone, deepslate, reinforced deepslate) by kind.
    match state.kind() {
        Kind::Stone => return out.add_block(Kind::Cobblestone, 1),
        Kind::Deepslate => return out.add_block(Kind::CobbledDeepslate, 1),
        Kind::ReinforcedDeepslate => return,
        _ => {}
    }
    let fortune = level(tool, Enchantment::Fortune);
    let Some(class) = resolve(state, |c| {
        matches!(
            c,
            Class::AmethystCluster
                | Class::BaseCoral
                | Class::BaseFire
                | Class::Beetroot
                | Class::Block
                | Class::BlueIce
                | Class::Bookshelf
                | Class::BrownMushroomBlock
                | Class::BuddingAmethyst
                | Class::Cake
                | Class::CakeWithCandle
                | Class::Campfire
                | Class::Candle
                | Class::Carrot
                | Class::CaveVines
                | Class::ChiseledBookshelf
                | Class::ChorusPlant
                | Class::Clay
                | Class::CoalOre
                | Class::Cobweb
                | Class::CocoaBlock
                | Class::CopperOre
                | Class::CoralBlock
                | Class::DiamondOre
                | Class::DoublePitcherCrop
                | Class::EmeraldOre
                | Class::EnderChest
                | Class::Farmland
                | Class::GildedBlackstone
                | Class::Glass
                | Class::GlassPane
                | Class::Glowstone
                | Class::GoldOre
                | Class::Grass
                | Class::GrassPath
                | Class::Gravel
                | Class::Ice
                | Class::InfestedStone
                | Class::IronOre
                | Class::LapisOre
                | Class::Leaves
                | Class::Liquid
                | Class::Melon
                | Class::MonsterSpawner
                | Class::MushroomStem
                | Class::Mycelium
                | Class::NetherGoldOre
                | Class::NetherQuartzOre
                | Class::NetherReactor
                | Class::NetherVines
                | Class::NetherWartPlant
                | Class::Nylium
                | Class::PackedIce
                | Class::PinkPetals
                | Class::Podzol
                | Class::Potato
                | Class::RedMushroomBlock
                | Class::RedstoneOre
                | Class::Sculk
                | Class::SeaLantern
                | Class::SeaPickle
                | Class::Slab
                | Class::SmallDripleaf
                | Class::Snow
                | Class::SnowLayer
                | Class::SoulCampfire
                | Class::Stem
                | Class::SweetBerryBush
                | Class::Wheat
        )
        .then_some(c)
    }) else {
        return out.add_own(state, 1);
    };
    match class {
        Class::AmethystCluster => {
            if state.name() == "minecraft:amethyst_cluster" {
                out.add_kind(I::AmethystShard, weighted(tool, rng, 4, 4));
            }
        }
        Class::BaseCoral
        | Class::BaseFire
        | Class::BlueIce
        | Class::BuddingAmethyst
        | Class::Cake
        | Class::ChiseledBookshelf
        | Class::Glass
        | Class::GlassPane
        | Class::Ice
        | Class::InfestedStone
        | Class::Liquid
        | Class::MonsterSpawner
        | Class::MushroomStem
        | Class::PackedIce
        | Class::Sculk => {}
        Class::Beetroot => {
            if growth(state) >= 7 {
                out.add_kind(I::Beetroot, 1);
                out.add_kind(I::BeetrootSeeds, binomial(fortune, rng, 0, 3, 4.0 / 7.0));
            } else {
                out.add_kind(I::BeetrootSeeds, 1);
            }
        }
        Class::Bookshelf => out.add_kind(I::Book, 3),
        Class::BrownMushroomBlock => out.add_block(Kind::BrownMushroom, range(rng, 0, 2)),
        Class::RedMushroomBlock => out.add_block(Kind::RedMushroom, range(rng, 0, 2)),
        Class::CakeWithCandle => {
            // The candle of the cake's colour: `minecraft:<colour>_candle_cake` holds
            // `minecraft:<colour>_candle`.
            let candle = state.name().strip_suffix("_cake").and_then(|name| {
                State::find(
                    name,
                    &[
                        ("candles", PropertyValue::Int(0)),
                        ("lit", PropertyValue::Byte(0)),
                    ],
                )
            });
            if let Some(candle) = candle {
                out.add_own(candle, 1);
            }
        }
        Class::SoulCampfire => out.add_block(Kind::SoulSoil, 1),
        Class::Campfire => out.add_kind(I::Charcoal, 2),
        Class::Candle => out.add_own(state, count_of(state, "candles") + 1),
        Class::Carrot => {
            let n = if growth(state) >= 7 {
                binomial(fortune, rng, 1, 3, 4.0 / 7.0)
            } else {
                1
            };
            out.add_kind(I::Carrot, n);
        }
        Class::Potato => {
            let ripe = growth(state) >= 7;
            let n = if ripe {
                binomial(fortune, rng, 1, 3, 4.0 / 7.0)
            } else {
                1
            };
            out.add_kind(I::Potato, n);
            if ripe && rng.below(50) == 0 {
                out.add_kind(I::PoisonousPotato, 1);
            }
        }
        Class::Wheat => {
            if growth(state) >= 7 {
                out.add_kind(I::Wheat, 1);
                out.add_kind(I::WheatSeeds, binomial(fortune, rng, 0, 3, 4.0 / 7.0));
            } else {
                out.add_kind(I::WheatSeeds, 1);
            }
        }
        Class::CaveVines => {
            if state.name().ends_with("_with_berries") {
                out.add_own(state, 1);
            }
        }
        Class::ChorusPlant => {
            if rng.below(2) == 1 {
                out.add_kind(I::ChorusFruit, 1);
            }
        }
        Class::Clay => out.add_kind(I::Clay, 4),
        Class::CoalOre => out.add_kind(I::Coal, weighted(tool, rng, 1, 1)),
        Class::DiamondOre => out.add_kind(I::Diamond, weighted(tool, rng, 1, 1)),
        Class::EmeraldOre => out.add_kind(I::Emerald, weighted(tool, rng, 1, 1)),
        Class::GoldOre => out.add_kind(I::RawGold, weighted(tool, rng, 1, 1)),
        Class::IronOre => out.add_kind(I::RawIron, weighted(tool, rng, 1, 1)),
        Class::CopperOre => out.add_kind(I::RawCopper, weighted(tool, rng, 2, 5)),
        Class::LapisOre => out.add_kind(I::LapisLazuli, weighted(tool, rng, 4, 9)),
        Class::NetherGoldOre => out.add_kind(I::GoldNugget, weighted(tool, rng, 2, 6)),
        Class::NetherQuartzOre => out.add_kind(I::NetherQuartz, weighted(tool, rng, 1, 1)),
        Class::RedstoneOre => out.add_kind(I::RedstoneDust, discrete(tool, rng, 4, 5)),
        Class::Cobweb => {
            if is_shears(tool) {
                out.add_own(state, 1);
            } else {
                out.add_kind(I::String, 1);
            }
        }
        Class::CocoaBlock => {
            let n = if props::int(state, "age") == Some(2) {
                range(rng, 2, 3)
            } else {
                1
            };
            out.add_kind(I::CocoaBeans, n);
        }
        Class::CoralBlock => out.add_own(props::dead_coral(state).unwrap_or(state), 1),
        Class::DoublePitcherCrop => {
            // PMMP age 1 (the last) is Bedrock growth 4.
            if growth(state) >= 4 {
                out.add_block(Kind::PitcherPlant, 1);
            } else {
                out.add_kind(I::PitcherPod, 1);
            }
        }
        Class::EnderChest => out.add_block(Kind::Obsidian, 8),
        Class::Farmland | Class::Grass | Class::GrassPath | Class::Mycelium | Class::Podzol => {
            out.add_block(Kind::Dirt, 1);
        }
        Class::Nylium => out.add_block(Kind::Netherrack, 1),
        Class::GildedBlackstone => {
            if bonus_divisor(tool, rng, 10, 3) {
                out.add_kind(I::GoldNugget, range(rng, 2, 5));
            } else {
                out.add_own(state, 1);
            }
        }
        Class::Gravel => {
            if bonus_divisor(tool, rng, 10, 3) {
                out.add_kind(I::Flint, 1);
            } else {
                out.add_own(state, 1);
            }
        }
        Class::Glowstone => {
            out.add_kind(I::GlowstoneDust, discrete(tool, rng, 2, 4).min(4));
        }
        Class::Leaves => leaves(state, tool, rng, out),
        Class::Melon => out.add_kind(I::Melon, discrete(tool, rng, 3, 7).min(9)),
        Class::NetherReactor => {
            out.add_kind(I::IronIngot, 6);
            out.add_kind(I::Diamond, 3);
        }
        Class::NetherVines => {
            if is_shears(tool) || bonus_fixed(tool, rng, 1.0 / 3.0, 2.0 / 9.0) {
                out.add_own(state, 1);
            }
        }
        Class::NetherWartPlant => {
            let n = if props::int(state, "age") == Some(3) {
                discrete(tool, rng, 2, 4)
            } else {
                1
            };
            out.add_own(state, n);
        }
        Class::PinkPetals => out.add_own(state, growth(state) + 1),
        Class::SeaPickle => out.add_own(state, count_of(state, "cluster_count") + 1),
        Class::SeaLantern => {
            out.add_kind(I::PrismarineCrystals, discrete(tool, rng, 2, 3).min(5));
        }
        Class::Slab => {
            let double = props::is_double_slab(state);
            out.add_own(state, if double { 2 } else { 1 });
        }
        Class::SmallDripleaf => {
            if props::flag(state, "upper_block_bit") != Some(true) {
                out.add_own(state, 1);
            }
        }
        Class::Snow => out.add_kind(I::Snowball, 4),
        Class::SnowLayer => {
            // PMMP layers 1 to 8 are Bedrock height 0 to 7.
            let layers = count_of(state, "height") + 1;
            out.add_kind(I::Snowball, (layers / 2).max(1));
        }
        Class::Stem => {
            // Not changed by Fortune (PMMP passes an empty hand).
            let chance = f64::from(growth(state) + 1) / 15.0;
            out.add_own(state, binomial(0, rng, 0, 3, chance));
        }
        Class::SweetBerryBush => {
            let n = match growth(state) {
                3 => discrete(tool, rng, 2, 3),
                2 => discrete(tool, rng, 1, 2),
                _ => 0,
            };
            out.add_own(state, n);
        }
        _ => out.add_own(state, 1),
    }
}

/// PMMP `Leaves::getDropsForCompatibleTool`: itself with shears, else saplings, apples and
/// sticks by chance.
fn leaves(state: State, tool: &Stack, rng: &mut Random, out: &mut Drops) {
    if is_shears(tool) {
        return out.add_own(state, 1);
    }
    let kind = state.kind();
    if bonus_divisor(tool, rng, 20, 4) {
        let sapling = match kind {
            Kind::AcaciaLeaves => Some(Kind::AcaciaSapling),
            Kind::BirchLeaves => Some(Kind::BirchSapling),
            Kind::DarkOakLeaves => Some(Kind::DarkOakSapling),
            Kind::JungleLeaves => Some(Kind::JungleSapling),
            Kind::OakLeaves => Some(Kind::OakSapling),
            Kind::SpruceLeaves => Some(Kind::SpruceSapling),
            Kind::MangroveLeaves | Kind::AzaleaLeaves => Some(Kind::Azalea),
            Kind::FloweringAzaleaLeaves => Some(Kind::FloweringAzalea),
            _ => None,
        };
        if let Some(sapling) = sapling {
            out.add_block(sapling, 1);
        }
    }
    if matches!(kind, Kind::OakLeaves | Kind::DarkOakLeaves) && bonus_divisor(tool, rng, 200, 20) {
        out.add_kind(item::Kind::Apple, 1);
    }
    if bonus_divisor(tool, rng, 50, 5) {
        out.add_kind(item::Kind::Stick, range(rng, 1, 2));
    }
}

/// PMMP `getDropsForIncompatibleTool` with its class overrides.
fn incompatible(state: State, tool: &Stack, rng: &mut Random, out: &mut Drops) {
    let tall_grass = |rng: &mut Random, out: &mut Drops| {
        if bonus_divisor(tool, rng, 8, 2) {
            out.add_kind(item::Kind::WheatSeeds, 1);
        }
    };
    let class = resolve(state, |c| {
        matches!(
            c,
            Class::AmethystCluster
                | Class::Bell
                | Class::Block
                | Class::DeadBush
                | Class::DoubleTallGrass
                | Class::HangingRoots
                | Class::TallGrass
        )
        .then_some(c)
    });
    match class {
        Some(Class::AmethystCluster) => {
            if state.name() == "minecraft:amethyst_cluster" {
                out.add_kind(item::Kind::AmethystShard, weighted(tool, rng, 2, 2));
            }
        }
        Some(Class::Bell) => out.add_own(state, 1),
        Some(Class::DeadBush) => out.add_kind(item::Kind::Stick, range(rng, 0, 2)),
        Some(Class::DoubleTallGrass) => {
            if props::flag(state, "upper_block_bit") == Some(true) {
                tall_grass(rng, out);
            }
        }
        Some(Class::TallGrass) => tall_grass(rng, out),
        Some(Class::HangingRoots) if level(tool, Enchantment::SilkTouch) > 0 => {
            compatible(state, tool, rng, out);
        }
        _ => {}
    }
}

/// PMMP `getSilkTouchDrops` with its class overrides.
fn silk_touch(state: State, out: &mut Drops) {
    match resolve(state, |c| {
        matches!(
            c,
            Class::InfestedStone | Class::RedMushroomBlock | Class::Block
        )
        .then_some(c)
    }) {
        Some(Class::InfestedStone) => {
            let imitated = match state.kind() {
                Kind::InfestedStone => Kind::Stone,
                Kind::InfestedCobblestone => Kind::Cobblestone,
                Kind::InfestedStoneBrick => Kind::StoneBricks,
                Kind::InfestedMossyStoneBrick => Kind::MossyStoneBricks,
                Kind::InfestedCrackedStoneBrick => Kind::CrackedStoneBricks,
                Kind::InfestedChiseledStoneBrick => Kind::ChiseledStoneBricks,
                Kind::InfestedDeepslate => Kind::Deepslate,
                other => other,
            };
            out.add_block(imitated, 1);
        }
        Some(Class::RedMushroomBlock) => {
            // The all-cap form (Bedrock `huge_mushroom_bits` 14).
            let cap = props::with_int(state, "huge_mushroom_bits", 14).unwrap_or(state);
            out.add_own(cap, 1);
        }
        _ => out.add_own(state, 1),
    }
}

/// PMMP `getXpDropForTool`: nothing with Silk Touch or an incompatible tool, else the class's
/// `getXpDropAmount`.
pub(super) fn experience(state: State, tool: &Stack, rng: &mut Random) -> u32 {
    if level(tool, Enchantment::SilkTouch) > 0 || !is_compatible(state, tool) {
        return 0;
    }
    match resolve(state, |c| {
        matches!(
            c,
            Class::CoalOre
                | Class::DiamondOre
                | Class::EmeraldOre
                | Class::LapisOre
                | Class::MonsterSpawner
                | Class::NetherQuartzOre
                | Class::RedstoneOre
                | Class::Sculk
                | Class::Block
        )
        .then_some(c)
    }) {
        Some(Class::CoalOre) => range(rng, 0, 2),
        Some(Class::DiamondOre | Class::EmeraldOre) => range(rng, 3, 7),
        Some(Class::LapisOre | Class::NetherQuartzOre) => range(rng, 2, 5),
        Some(Class::MonsterSpawner) => range(rng, 15, 43),
        Some(Class::RedstoneOre) => range(rng, 1, 5),
        Some(Class::Sculk) => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{GoldenDrop, GoldenItems};
    use crate::item::Extra;

    /// `tool` with Silk Touch.
    fn silk(tool: crate::item::Type) -> Stack {
        let extra = Extra {
            enchantments: vec![(Enchantment::SilkTouch, 1)],
            ..Extra::default()
        };
        Stack::new(tool, 1).with_extra(Some(extra)).unwrap()
    }

    /// `tool` with Fortune of `level`.
    fn fortune(tool: crate::item::Type, level: u16) -> Stack {
        let extra = Extra {
            enchantments: vec![(Enchantment::Fortune, level)],
            ..Extra::default()
        };
        Stack::new(tool, 1).with_extra(Some(extra)).unwrap()
    }

    /// The drops as (item, count) sorted, for comparing with the golden stacks.
    fn sorted(drops: &Drops) -> Vec<(crate::item::Type, u8)> {
        let mut out: Vec<_> = drops
            .items
            .iter()
            .map(|s| (s.item().unwrap(), s.count()))
            .collect();
        out.sort();
        out
    }

    /// Checks one golden case: exact stacks are equal over many seeds and XP stays in range.
    fn check(state: State, tool: &Stack, golden: GoldenDrop) -> bool {
        let mut exact = true;
        for seed in 0..64 {
            let mut rng = Random::new(seed, 7, 11);
            let got = drops(state, tool, &mut rng);
            let xp = experience(state, tool, &mut rng);
            assert!(
                (u32::from(golden.xp.0)..=u32::from(golden.xp.1)).contains(&xp),
                "{state:?} {tool:?} xp {xp} not in {:?}",
                golden.xp
            );
            match golden.items {
                GoldenItems::Exact(..) => {
                    let mut want: Vec<_> = golden
                        .items
                        .stacks()
                        .unwrap()
                        .iter()
                        .map(|s| (s.item, s.count))
                        .collect();
                    want.sort();
                    if sorted(&got) != want || got.from_block_entity != 0 {
                        exact = false;
                    }
                }
                GoldenItems::World | GoldenItems::BlockEntity => {
                    assert!(got.from_block_entity <= 1);
                }
                GoldenItems::Random => {}
            }
        }
        exact
    }

    /// Every state's drops by hand, with the lowest compatible tool and with Silk Touch are the
    /// PMMP dump's deterministic drops, and XP is in the dump's range.
    #[test]
    fn deterministic_drops_equal_the_pmmp_dump_for_every_state() {
        let mut wrong = Vec::new();
        let mut cases = 0;
        for state in State::all() {
            let golden = state.golden_drops();
            let mut list = vec![(Stack::EMPTY, golden.hand)];
            if let Some((tool, drop)) = golden.tool {
                list.push((Stack::new(tool, 1), drop));
            }
            if let Some((tool, drop)) = golden.silk_touch {
                list.push((silk(tool), drop));
            }
            for (tool, drop) in list {
                cases += 1;
                if !check(state, &tool, drop) {
                    wrong.push(format!("{state:?} with {:?}", tool.item()));
                }
            }
        }
        assert!(cases > 30_000, "{cases}");
        assert!(
            wrong.is_empty(),
            "{} wrong, first: {:?}",
            wrong.len(),
            &wrong[..wrong.len().min(20)]
        );
    }

    /// The drop counts PMMP draws at random stay within PMMP's ranges.
    #[test]
    fn random_drops_stay_in_pmmp_ranges() {
        let pickaxe = item::Kind::IronPickaxe.default_type();
        let state = |k: Kind| k.default_state();
        let ripe = |k: Kind| props::with_int(k.default_state(), "growth", 7).unwrap();
        let count = |d: &Drops, k: item::Kind| -> u32 {
            d.items
                .iter()
                .filter(|s| s.item() == Some(k.default_type()))
                .map(|s| u32::from(s.count()))
                .sum()
        };
        for seed in 0..2_000 {
            let mut rng = Random::new(seed, 1, 2);
            let d = drops(ripe(Kind::Wheat), &Stack::EMPTY, &mut rng);
            assert_eq!(count(&d, item::Kind::Wheat), 1);
            assert!(count(&d, item::Kind::WheatSeeds) <= 3);
            let d = drops(
                state(Kind::LapisLazuliOre),
                &Stack::new(pickaxe, 1),
                &mut rng,
            );
            assert!((4..=9).contains(&count(&d, item::Kind::LapisLazuli)));
            let d = drops(state(Kind::LapisLazuliOre), &fortune(pickaxe, 3), &mut rng);
            assert!((4..=36).contains(&count(&d, item::Kind::LapisLazuli)));
            let d = drops(state(Kind::Glowstone), &fortune(pickaxe, 3), &mut rng);
            assert!((2..=4).contains(&count(&d, item::Kind::GlowstoneDust)));
            let d = drops(state(Kind::Melon), &Stack::EMPTY, &mut rng);
            assert!((3..=7).contains(&count(&d, item::Kind::Melon)));
            let d = drops(ripe(Kind::Carrots), &fortune(pickaxe, 2), &mut rng);
            assert!((1..=6).contains(&count(&d, item::Kind::Carrot)));
            let d = drops(state(Kind::CopperOre), &Stack::new(pickaxe, 1), &mut rng);
            assert!((2..=5).contains(&count(&d, item::Kind::RawCopper)));
            let xp = experience(state(Kind::DiamondOre), &Stack::new(pickaxe, 1), &mut rng);
            assert!((3..=7).contains(&xp));
        }
    }

    #[test]
    fn fortune_raises_ore_drops_on_average() {
        let pickaxe = item::Kind::DiamondPickaxe.default_type();
        let ore = Kind::DiamondOre.default_state();
        let mut plain = 0_u32;
        let mut lucky = 0_u32;
        for seed in 0..1_000 {
            let mut rng = Random::new(seed, 0, 0);
            plain += u32::from(drops(ore, &Stack::new(pickaxe, 1), &mut rng).items[0].count());
            let mut rng = Random::new(seed, 0, 0);
            lucky += u32::from(drops(ore, &fortune(pickaxe, 3), &mut rng).items[0].count());
        }
        assert_eq!(plain, 1_000);
        // PMMP: 60% of breaks pick from 1..=4, so the mean is 1.9.
        assert!((1_700..2_100).contains(&lucky), "{lucky}");
    }
}
