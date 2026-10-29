//! How long blocks take to break and what breaking costs a tool (PMMP
//! `BlockBreakInfo::getBreakTime`, `Tool::getMiningEfficiency`, `Sword::getMiningEfficiency`,
//! `SurvivalBlockBreakHandler::calculateBreakProgressPerTick`, `Durable::applyDamage`).

use crate::block::{Inert, State};
use crate::item::{self, Enchantment, Stack};
use crate::random::Random;
use crate::tick::Settings;

use super::drops::level;

/// What changes how fast a player breaks blocks besides the tool (PMMP reads these from the
/// player each tick).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Conditions {
    /// Standing on a block.
    pub(super) on_ground: bool,
    /// Flying (creative abilities).
    pub(super) flying: bool,
    /// The eyes are in water.
    pub(super) underwater: bool,
    /// The helmet has Aqua Affinity.
    pub(super) aqua_affinity: bool,
    /// Haste level (0: none).
    pub(super) haste: u8,
    /// Mining Fatigue level (0: none).
    pub(super) fatigue: u8,
}

impl Conditions {
    /// On the ground, out of water, no effects.
    #[cfg(test)]
    pub(super) const PLAIN: Self = Self {
        on_ground: true,
        flying: false,
        underwater: false,
        aqua_affinity: false,
        haste: 0,
        fatigue: 0,
    };
}

/// The seconds `state` takes to break with `tool` ([`State::break_time_secs`]: PMMP
/// `BlockBreakInfo::getBreakTime` with the leaf, bamboo and wool exceptions); `None` for a block
/// that cannot be broken.
fn break_time(state: State, tool: &Stack) -> Option<f64> {
    if !state.break_info().is_breakable() {
        return None;
    }
    state.break_time_secs(tool)
}

/// `base` to the power `n` by repeated multiplication (std's `powi` is not deterministic).
fn power(base: f64, n: u8) -> f64 {
    (0..n).fold(1.0, |acc, _| acc * base)
}

/// PMMP `SurvivalBlockBreakHandler::calculateBreakProgressPerTick`: the share of the block one
/// tick breaks (1 or more breaks it at once; 0 for a block that cannot be broken).
pub(super) fn progress_per_tick(state: State, tool: &Stack, conditions: Conditions) -> f64 {
    progress(break_time(state, tool), conditions)
}

/// [`progress_per_tick`] for an inert block, which breaks as its [`Inert::break_info`] says.
pub(super) fn inert_progress_per_tick(state: Inert, tool: &Stack, conditions: Conditions) -> f64 {
    let info = state.break_info();
    let seconds = info
        .is_breakable()
        .then(|| info.break_time_secs(tool))
        .flatten();
    progress(seconds, conditions)
}

/// The share of a block one tick breaks when it takes `seconds` with the held tool (`None`: it
/// cannot be broken).
fn progress(seconds: Option<f64>, conditions: Conditions) -> f64 {
    let Some(seconds) = seconds else {
        return 0.0;
    };
    let mut ticks = seconds * 20.0;
    if !conditions.on_ground && !conditions.flying {
        ticks *= 5.0;
    }
    if conditions.underwater && !conditions.aqua_affinity {
        ticks *= 5.0;
    }
    if ticks <= 0.0 {
        return 1.0;
    }
    let mut progress = 1.0 / ticks;
    if conditions.haste > 0 {
        let level = f64::from(conditions.haste);
        progress *= (1.0 + 0.2 * level) * power(1.2, conditions.haste);
    }
    if conditions.fatigue > 0 {
        progress *= power(0.21, conditions.fatigue);
    }
    progress
}

/// PMMP `Durable::applyDamage($amount)` on a copy of `tool`: Unbreaking may spare each point (by
/// `1 / (level + 1)` chance it counts; armour only 40% of the time at best); a tool worn to its
/// durability breaks (the stack loses one item). The stack unchanged for items without
/// durability, unbreakable items and broken ones, and for every item with `durability = false`:
/// every rule that wears an item (tools, weapons, armour, bows, tridents) comes here.
#[expect(
    clippy::expect_used,
    reason = "invariant: an item that wears keeps 0 and a damage below its durability"
)]
pub(in crate::tick) fn damage(
    settings: &Settings,
    tool: &Stack,
    amount: u32,
    rng: &mut Random,
) -> Stack {
    if !settings.durability {
        return tool.clone();
    }
    let Some(item) = tool.item() else {
        return tool.clone();
    };
    let Some(max) = item.max_durability() else {
        return tool.clone();
    };
    let unbreakable = tool.extra().is_some_and(
        |e| matches!(e.nbt.get(item::TAG_UNBREAKABLE), Some(crate::nbt::Value::Byte(b)) if *b != 0),
    );
    if unbreakable || tool.damage() >= max {
        return tool.clone();
    }
    let unbreaking = level(tool, Enchantment::Unbreaking);
    let armor = item.is(item::Class::Armor);
    // More than the whole durability breaks it all the same: no more draws.
    let amount = amount.min(u32::from(max));
    let mut taken = amount;
    if unbreaking > 0 {
        let chance = 1.0 / (f64::from(unbreaking) + 1.0);
        for _ in 0..amount {
            let spared = if armor {
                rng.range(1, 100) > 60 && rng.unit() > chance
            } else {
                rng.unit() > chance
            };
            taken -= u32::from(spared);
        }
    }
    let total = u32::from(tool.damage()).saturating_add(taken);
    let damage = u16::try_from(total.min(u32::from(max))).unwrap_or(max);
    if damage >= max {
        // PMMP `Durable::onBroken`: one item less, the rest undamaged.
        return tool
            .with_count(tool.count() - 1)
            .with_damage(0)
            .expect("invariant: every item keeps no damage");
    }
    tool.with_damage(damage)
        .expect("invariant: a damage below the durability is kept")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::Kind;
    use crate::item::{Extra, Kind as I};

    fn tool(kind: I) -> Stack {
        Stack::new(kind.default_type(), 1)
    }

    fn enchanted(kind: I, e: Enchantment, level: u8) -> Stack {
        let extra = Extra {
            enchantments: vec![(e, level)],
            ..Extra::default()
        };
        tool(kind).with_extra(Some(extra)).unwrap()
    }

    fn seconds(block: Kind, with: &Stack) -> f64 {
        break_time(block.default_state(), with).unwrap()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// Break times per tool and tier are PMMP's (`BlockBreakInfo::getBreakTime` run on the
    /// pinned checkout).
    #[test]
    fn break_times_per_tool_and_tier_are_pmmp_values() {
        let cases: [(Kind, Stack, f64); 16] = [
            (Kind::Stone, Stack::EMPTY, 7.5),
            (Kind::Stone, tool(I::WoodenPickaxe), 1.125),
            (Kind::Stone, tool(I::StonePickaxe), 0.5625),
            (Kind::Stone, tool(I::IronPickaxe), 0.375),
            (Kind::Stone, tool(I::DiamondPickaxe), 0.281_25),
            (Kind::Stone, tool(I::NetheritePickaxe), 0.25),
            (Kind::Stone, tool(I::GoldenPickaxe), 0.1875),
            (
                Kind::Stone,
                enchanted(I::DiamondPickaxe, Enchantment::Efficiency, 5),
                2.25 / 34.0,
            ),
            (Kind::Obsidian, tool(I::DiamondPickaxe), 6.5625),
            (Kind::Obsidian, tool(I::IronPickaxe), 175.0 / 6.0),
            (Kind::OakLog, Stack::EMPTY, 3.0),
            (Kind::OakLog, tool(I::WoodenAxe), 1.5),
            (Kind::Dirt, Stack::EMPTY, 0.75),
            (Kind::Dirt, tool(I::WoodenSword), 0.5),
            (Kind::Cobweb, tool(I::WoodenSword), 0.4),
            (Kind::Cobweb, tool(I::Shears), 0.4),
        ];
        for (block, with, want) in cases {
            let got = seconds(block, &with);
            assert!(
                close(got, want),
                "{block:?} {:?}: {got} != {want}",
                with.item()
            );
        }
        assert_eq!(
            break_time(Kind::Bedrock.default_state(), &Stack::EMPTY),
            None
        );
        assert_eq!(seconds(Kind::Wheat, &Stack::EMPTY), 0.0);
    }

    /// Breaking uses the block's own break time: PMMP's exceptions for leaves with shears,
    /// bamboo with a sword and wool with shears count.
    #[test]
    fn breaking_follows_pmmp_break_time_exceptions() {
        let leaves = Kind::OakLeaves.default_state();
        let shears = tool(I::Shears);
        assert_eq!(seconds(Kind::OakLeaves, &shears), 0.0);
        assert_eq!(progress_per_tick(leaves, &shears, Conditions::PLAIN), 1.0);
        assert_eq!(seconds(Kind::Bamboo, &tool(I::WoodenSword)), 0.0);
        assert!(close(seconds(Kind::Wool, &shears), 0.24));
        assert!(close(
            progress_per_tick(Kind::Wool.default_state(), &shears, Conditions::PLAIN),
            1.0 / (0.24 * 20.0)
        ));
    }

    #[test]
    fn progress_follows_ground_water_and_effects() {
        let stone = Kind::Stone.default_state();
        let pick = tool(I::WoodenPickaxe);
        let plain = progress_per_tick(stone, &pick, Conditions::PLAIN);
        assert!(close(plain, 1.0 / 22.5));
        let air = Conditions {
            on_ground: false,
            ..Conditions::PLAIN
        };
        assert!(close(progress_per_tick(stone, &pick, air), plain / 5.0));
        let water = Conditions {
            underwater: true,
            ..Conditions::PLAIN
        };
        assert!(close(progress_per_tick(stone, &pick, water), plain / 5.0));
        let aqua = Conditions {
            aqua_affinity: true,
            ..water
        };
        assert!(close(progress_per_tick(stone, &pick, aqua), plain));
        let haste = Conditions {
            haste: 2,
            ..Conditions::PLAIN
        };
        assert!(close(
            progress_per_tick(stone, &pick, haste),
            plain * 1.4 * 1.44
        ));
        let fatigue = Conditions {
            fatigue: 1,
            ..Conditions::PLAIN
        };
        assert!(close(
            progress_per_tick(stone, &pick, fatigue),
            plain * 0.21
        ));
        assert_eq!(
            progress_per_tick(
                Kind::Wheat.default_state(),
                &Stack::EMPTY,
                Conditions::PLAIN
            ),
            1.0
        );
        assert_eq!(
            progress_per_tick(Kind::Bedrock.default_state(), &pick, Conditions::PLAIN),
            0.0
        );
    }

    /// The tick settings with `durability` as given.
    fn settings(durability: bool) -> Settings {
        let mut config = crate::config::Config::default();
        config.general.durability = durability;
        Settings::new(&config)
    }

    #[test]
    fn tools_wear_and_break_at_their_durability() {
        let on = settings(true);
        let mut rng = Random::new(0, 0, 0);
        let pick = tool(I::WoodenPickaxe);
        let max = pick.item().unwrap().max_durability().unwrap();
        let worn = damage(&on, &pick, 1, &mut rng);
        assert_eq!(worn.damage(), 1);
        let almost = pick.with_damage(max - 1).unwrap();
        assert!(damage(&on, &almost, 1, &mut rng).is_empty());
        assert_eq!(damage(&on, &Stack::EMPTY, 1, &mut rng), Stack::EMPTY);
        let stick = tool(I::Stick);
        assert_eq!(damage(&on, &stick, 1, &mut rng), stick);
        // Unbreaking III spares about three of four points.
        let tough = enchanted(I::DiamondPickaxe, Enchantment::Unbreaking, 3);
        let mut taken = 0;
        for seed in 0..4_000 {
            let mut rng = Random::new(seed, 0, 0);
            taken += u32::from(damage(&on, &tough, 1, &mut rng).damage());
        }
        assert!((800..1_200).contains(&taken), "{taken}");
    }

    #[test]
    fn the_highest_unbreaking_level_wears_without_overflow() {
        let on = settings(true);
        let tough = enchanted(I::DiamondPickaxe, Enchantment::Unbreaking, u8::MAX);
        let mut rng = Random::new(0, 0, 0);
        assert!(damage(&on, &tough, 1, &mut rng).damage() <= 1);
    }

    /// `durability = false`: nothing wears, not even an item one point from breaking, and no
    /// random draw is taken.
    #[test]
    fn without_durability_nothing_wears() {
        let off = settings(false);
        let mut rng = Random::new(0, 0, 0);
        let pick = tool(I::WoodenPickaxe);
        let max = pick.item().unwrap().max_durability().unwrap();
        let almost = pick.with_damage(max - 1).unwrap();
        assert_eq!(damage(&off, &almost, 5, &mut rng), almost);
        let tough = enchanted(I::DiamondPickaxe, Enchantment::Unbreaking, 3);
        assert_eq!(damage(&off, &tough, 1, &mut rng), tough);
        let fresh = Random::new(0, 0, 0).unit();
        assert_eq!(rng.unit().to_bits(), fresh.to_bits(), "no draw taken");
    }
}
