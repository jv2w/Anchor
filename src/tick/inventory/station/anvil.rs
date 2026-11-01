//! Anvil rules: renaming, repairing with an item's material or with a second item of its type,
//! joining the enchantments of a second item or an enchanted book, and the level cost, which
//! grows with the items' prior work (`RepairCost`). PMMP has no anvil; the levels each step costs
//! are those of dragonfly's anvil.

use crate::item::{self, Enchantment, Extra, Kind, Stack, ToolTier};
use crate::nbt::Value;

/// The cost at which the anvil refuses outside creative mode ("too expensive").
pub(in crate::tick) const TOO_EXPENSIVE: u32 = 40;
/// The tag key of an item's prior work.
const REPAIR_COST: &str = "RepairCost";
/// The levels renaming costs.
const RENAME_LEVELS: u32 = 1;
/// The levels repairing with a second item of the same type costs.
const JOIN_REPAIR_LEVELS: u32 = 2;
/// The share of the durability, in percent, that repairing with a second item of the same type
/// adds to the durability the second item has left.
const JOIN_REPAIR_BONUS_PERCENT: u32 = 12;
/// How many units of material repair a whole durability, a level each.
const MATERIAL_UNITS: u16 = 4;

/// What an anvil makes of its input and material.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::tick) struct Outcome {
    /// The result, as many as the input.
    pub(in crate::tick) result: Stack,
    /// Material items used up: the repair count, or the whole material.
    pub(in crate::tick) material_used: u8,
    /// The level cost.
    pub(in crate::tick) cost: u32,
}

/// Why the anvil does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tick) enum Refused {
    /// No input.
    NoInput,
    /// The material does not repair or combine with the input.
    Mismatch,
    /// The input is not damaged.
    Repaired,
    /// Nothing would change.
    Nothing,
    /// The cost reached [`TOO_EXPENSIVE`] outside creative mode.
    TooExpensive,
    /// The result would be over a stack limit (its saved size, a name over 1,024 bytes).
    TooLarge(item::Error),
}

/// The result taking shape: its damage, its extra data and the levels the work so far costs.
struct Work {
    damage: u16,
    extra: Extra,
    levels: u32,
}

impl Work {
    /// The work on `input` before any step.
    fn new(input: &Stack) -> Self {
        Self {
            damage: input.damage(),
            extra: input.extra().cloned().unwrap_or_default(),
            levels: 0,
        }
    }

    /// Repairs an item of `item` with up to `units` of its material, each restoring a quarter of
    /// the durability for a level, until it is whole; the units used.
    fn repair(&mut self, item: item::Type, units: u8) -> Result<u8, Refused> {
        let quarter = item.max_durability().map_or(0, |max| max / MATERIAL_UNITS);
        if self.damage == 0 || quarter == 0 {
            return Err(Refused::Repaired);
        }
        let mut used = 0;
        while self.damage > 0 && used < units {
            self.damage -= self.damage.min(quarter);
            used += 1;
            self.levels += 1;
        }
        Ok(used)
    }

    /// Joins `material` to `input`, an item of `item`: a second item of the same type that wears
    /// adds the durability it has left and [`JOIN_REPAIR_BONUS_PERCENT`] of the whole, for
    /// [`JOIN_REPAIR_LEVELS`]; an enchanted book or a second item gives its enchantments
    /// ([`Work::join_enchantments`]). Anything else, or enchantments of which none joins, does
    /// nothing.
    fn join(&mut self, input: &Stack, item: item::Type, material: &Stack) -> Result<(), Refused> {
        let book =
            is_book(material) && material.extra().is_some_and(|e| !e.enchantments.is_empty());
        if !book {
            let Some(max) = item
                .max_durability()
                .filter(|_| material.item() == Some(item))
            else {
                return Err(Refused::Mismatch);
            };
            let restored = u32::from(max - material.damage())
                + u32::from(max) * JOIN_REPAIR_BONUS_PERCENT / 100;
            // Taking `u16::MAX` or more takes all the damage there is.
            let damage = self
                .damage
                .saturating_sub(u16::try_from(restored).unwrap_or(u16::MAX));
            if damage < self.damage {
                self.damage = damage;
                self.levels += JOIN_REPAIR_LEVELS;
            }
        }
        let (joined, refused) = self.join_enchantments(input, item, material, book);
        // dragonfly refuses this only for items that do not wear, and so charges a level for
        // nothing on others; vanilla (Java `AnvilMenu`) refuses it for every item.
        if refused && !joined {
            return Err(Refused::Mismatch);
        }
        Ok(())
    }

    /// Joins each enchantment of `material` that `item` takes (any, for a book) and that
    /// conflicts with none of the input's: at the material's level, or one above an equal level
    /// of the input's that is not the highest. Each input entry an enchantment conflicts with
    /// costs a level, and each join its rarity's levels (halved, at least one, from a `book`) for
    /// each level gained; with more than one input item a join makes the work too expensive.
    /// Returns whether any joined and whether any did not.
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: a tally's place is an index of the result's enchantments"
    )]
    fn join_enchantments(
        &mut self,
        input: &Stack,
        item: item::Type,
        material: &Stack,
        book: bool,
    ) -> (bool, bool) {
        let Some(offered) = material.extra() else {
            return (false, false);
        };
        let takes_any = is_book(input);
        let mut tallies = Tallies::new(&self.extra.enchantments);
        let (mut joined, mut refused) = (false, false);
        for &(e, level) in &offered.enchantments {
            let conflicts: u32 = e
                .incompatible()
                .iter()
                .map(|other| tallies.get(other).entries)
                .sum();
            self.levels = self.levels.saturating_add(conflicts);
            if conflicts > 0 || !(takes_any || item.enchantments().contains(e)) {
                refused = true;
                continue;
            }
            // A stack holds levels up to `MAX_ENCHANTMENT_LEVEL`, past the highest when a server
            // gave it.
            let can_rise =
                level != u16::from(e.max_level()) && level != item::MAX_ENCHANTMENT_LEVEL;
            let tally = tallies.get(e);
            let (level, gained) = match tally.first_level {
                None => (level, level),
                Some(had) if had < level => (level, level - had),
                Some(had) if had == level && can_rise => (level + 1, 1),
                Some(_) => {
                    refused = true;
                    continue;
                }
            };
            joined = true;
            if let Some(at) = tally.at {
                self.extra.enchantments[at].1 = level;
            } else {
                tallies.place(e, self.extra.enchantments.len());
                self.extra.enchantments.push((e, level));
            }
            let rarity = if book {
                (rarity_levels(e) / 2).max(1)
            } else {
                rarity_levels(e)
            };
            self.levels = self.levels.saturating_add(rarity * u32::from(gained));
            if input.count() > 1 {
                self.levels = TOO_EXPENSIVE;
            }
        }
        (joined, refused)
    }

    /// The result: `input` with the work's damage and extra data.
    #[expect(
        clippy::expect_used,
        reason = "invariant: the work keeps the input's damage or one from 0 to its durability"
    )]
    fn into_stack(self, input: &Stack) -> Result<Stack, Refused> {
        input
            .with_damage(self.damage)
            .expect("the damage is one the input keeps")
            .with_extra(Some(self.extra))
            .map_err(Refused::TooLarge)
    }
}

/// The input's enchantment entries by enchantment.
struct Tallies(Vec<Tally>);

impl Tallies {
    /// The tallies of `enchantments`.
    fn new(enchantments: &[(Enchantment, u16)]) -> Self {
        let mut tallies = Self(vec![Tally::default(); Enchantment::all().len()]);
        for (at, &(e, level)) in enchantments.iter().enumerate() {
            let tally = tallies.slot(e);
            tally.entries += 1;
            if tally.at.is_none() {
                tally.first_level = Some(level);
                tally.at = Some(at);
            }
        }
        tallies
    }

    /// The tally of `e`.
    fn get(&self, e: Enchantment) -> Tally {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: there is a tally per enchantment"
        )]
        self.0[e as usize]
    }

    /// Records that the result's enchantments hold `e` at `at`.
    fn place(&mut self, e: Enchantment, at: usize) {
        self.slot(e).at = Some(at);
    }

    /// The tally of `e`, to change.
    fn slot(&mut self, e: Enchantment) -> &mut Tally {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: there is a tally per enchantment"
        )]
        &mut self.0[e as usize]
    }
}

/// What the input says of one enchantment.
#[derive(Clone, Copy, Default)]
struct Tally {
    /// How many entries it has.
    entries: u32,
    /// The level of its first entry.
    first_level: Option<u16>,
    /// Where the result's enchantments hold it.
    at: Option<usize>,
}

/// The anvil's outcome for `input` and `material` with the optional new name `rename`;
/// `creative` lifts the cost limit.
pub(in crate::tick) fn craft(
    input: &Stack,
    material: &Stack,
    rename: Option<&str>,
    creative: bool,
) -> Result<Outcome, Refused> {
    let Some(item) = input.item() else {
        return Err(Refused::NoInput);
    };
    let mut work = Work::new(input);
    let material_used = match material.item() {
        Some(repairer) if repairs(item, repairer) => work.repair(item, material.count())?,
        Some(_) => {
            work.join(input, item, material)?;
            material.count()
        }
        None => 0,
    };
    let renamed_only = work.levels == 0 && rename.is_some();
    if let Some(name) = rename {
        work.extra.custom_name = Some(name.to_owned()).filter(|n| !n.is_empty());
        work.levels = work.levels.saturating_add(RENAME_LEVELS);
    }
    let mut cost = work
        .levels
        .saturating_add(prior_work(input))
        .saturating_add(prior_work(material));
    if cost == 0 {
        return Err(Refused::Nothing);
    }
    // Renaming alone is never too expensive.
    if renamed_only {
        cost = cost.min(TOO_EXPENSIVE - 1);
    }
    if cost >= TOO_EXPENSIVE && !creative {
        return Err(Refused::TooExpensive);
    }
    let prior = prior_work(input).max(prior_work(material));
    let prior = if renamed_only {
        prior
    } else {
        prior.saturating_mul(2).saturating_add(1)
    };
    // The prior work stops at the largest `TAG_Int`, the form it is saved in.
    work.extra.nbt.insert(
        REPAIR_COST.to_owned(),
        Value::Int(i32::try_from(prior).unwrap_or(i32::MAX)),
    );
    Ok(Outcome {
        result: work.into_stack(input)?,
        material_used,
        cost,
    })
}

/// An item's prior work (`RepairCost`, dragonfly `AnvilCost`); a saved value below 0 is none.
fn prior_work(stack: &Stack) -> u32 {
    match stack.extra().and_then(|e| e.nbt.get(REPAIR_COST)) {
        Some(Value::Int(c)) => u32::try_from(*c).unwrap_or(0),
        _ => 0,
    }
}

/// Whether `material` repairs `input` (vanilla repair materials: a tool's tier material, an
/// armour piece's material, leather, phantom membrane for elytra, planks for shields).
fn repairs(input: item::Type, material: item::Type) -> bool {
    let kind = material.kind();
    let tag = |name: &str| item::Tag::find(name).is_some_and(|t| t.contains(material));
    let tier = |t: ToolTier| match t {
        ToolTier::Wood => tag("minecraft:planks"),
        ToolTier::Stone => tag("minecraft:stone_tool_materials"),
        ToolTier::Copper => kind == Some(Kind::CopperIngot),
        ToolTier::Iron => kind == Some(Kind::IronIngot),
        ToolTier::Gold => kind == Some(Kind::GoldIngot),
        ToolTier::Diamond => kind == Some(Kind::Diamond),
        ToolTier::Netherite => kind == Some(Kind::NetheriteIngot),
    };
    if input.armor().is_none()
        && let Some(t) = input.tool_tier()
    {
        return tier(t);
    }
    let name = input.name().strip_prefix("minecraft:").unwrap_or_default();
    if name == "turtle_helmet" {
        return kind == Some(Kind::Scute);
    }
    if name == "elytra" {
        return kind == Some(Kind::PhantomMembrane);
    }
    if name == "shield" {
        return tag("minecraft:planks");
    }
    if input.armor().is_none() {
        return false;
    }
    let by_prefix = [
        ("leather_", Some(Kind::Leather)),
        ("chainmail_", Some(Kind::IronIngot)),
        ("iron_", Some(Kind::IronIngot)),
        ("golden_", Some(Kind::GoldIngot)),
        ("diamond_", Some(Kind::Diamond)),
        ("netherite_", Some(Kind::NetheriteIngot)),
        ("copper_", Some(Kind::CopperIngot)),
    ];
    by_prefix
        .iter()
        .any(|(prefix, want)| name.starts_with(prefix) && kind == *want)
}

/// The levels an enchantment's rarity costs per level (dragonfly `Rarity.Cost`: common 1,
/// uncommon 2, rare 4, very rare 8), from its weight.
fn rarity_levels(e: Enchantment) -> u32 {
    match e.rarity() {
        10.. => 1,
        5..=9 => 2,
        2..=4 => 4,
        _ => 8,
    }
}

/// Whether the item is an enchanted book.
fn is_book(stack: &Stack) -> bool {
    stack
        .item()
        .is_some_and(|t| t.is(item::Class::EnchantedBook))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The durability `stack` has left.
    fn left(stack: &Stack) -> u16 {
        stack.item().and_then(item::Type::max_durability).unwrap() - stack.damage()
    }

    /// `stack` with `durability` left.
    fn worn(stack: &Stack, durability: u16) -> Stack {
        let max = stack.item().and_then(item::Type::max_durability).unwrap();
        stack.with_damage(max - durability).unwrap()
    }

    #[test]
    fn renaming_repairing_and_prior_work_cost_their_levels() {
        let pick = Stack::new(Kind::IronPickaxe.default_type(), 1);
        let max = left(&pick);
        let worn_pick = worn(&pick, max / 10);
        // Rename only: 1 level, prior work stays 0.
        let renamed = craft(&pick, &Stack::EMPTY, Some("Pick"), false).unwrap();
        assert_eq!(renamed.cost, 1);
        assert_eq!(renamed.result.custom_name(), "Pick");
        assert_eq!(prior_work(&renamed.result), 0);
        // Repair with ingots: each restores a quarter of the maximum, one level each.
        let ingots = Stack::new(Kind::IronIngot.default_type(), 5);
        let repaired = craft(&worn_pick, &ingots, None, false).unwrap();
        assert_eq!(repaired.material_used, 4, "a quarter at a time until full");
        assert_eq!(left(&repaired.result), max);
        assert_eq!(repaired.cost, 4);
        assert_eq!(prior_work(&repaired.result), 1);
        // A second repair costs the prior work too.
        let again = craft(&worn(&repaired.result, max - 100), &ingots, None, false).unwrap();
        assert_eq!(again.material_used, 2);
        assert_eq!(again.cost, 2 + 1);
        assert_eq!(prior_work(&again.result), 3);
        // A full item cannot be repaired; a wrong material does nothing.
        assert_eq!(craft(&pick, &ingots, None, false), Err(Refused::Repaired));
        let gold = Stack::new(Kind::GoldIngot.default_type(), 1);
        assert_eq!(
            craft(&worn_pick, &gold, None, false),
            Err(Refused::Mismatch)
        );
        assert_eq!(
            craft(&pick, &Stack::EMPTY, None, false),
            Err(Refused::Nothing)
        );
    }

    fn enchanted(kind: Kind, list: &[(Enchantment, u16)]) -> Stack {
        Stack::new(kind.default_type(), 1)
            .with_extra(Some(Extra {
                enchantments: list.to_vec(),
                ..Extra::default()
            }))
            .unwrap()
    }

    /// `stack` with prior work `levels`.
    fn with_prior_work(stack: &Stack, levels: i32) -> Stack {
        let mut extra = stack.extra().cloned().unwrap_or_default();
        extra.nbt.insert(REPAIR_COST.to_owned(), Value::Int(levels));
        stack.with_extra(Some(extra)).unwrap()
    }

    #[test]
    fn enchantments_combine_by_level_and_rarity() {
        let a = enchanted(Kind::DiamondSword, &[(Enchantment::Sharpness, 2)]);
        let book = enchanted(Kind::EnchantedBook, &[(Enchantment::Sharpness, 2)]);
        let out = craft(&a, &book, None, false).unwrap();
        assert_eq!(
            out.result.extra().unwrap().enchantments,
            vec![(Enchantment::Sharpness, 3)]
        );
        // Common (1), halved for a book (at least 1), times one level up.
        assert_eq!(out.cost, 1);
        // An enchantment that conflicts with the input's joins nothing: refused.
        let helmet = enchanted(Kind::DiamondHelmet, &[(Enchantment::Protection, 1)]);
        let blast = enchanted(Kind::EnchantedBook, &[(Enchantment::BlastProtection, 1)]);
        assert_eq!(craft(&helmet, &blast, None, false), Err(Refused::Mismatch));
        // Two of a kind repair by 12% and merge.
        let sword = Stack::new(Kind::DiamondSword.default_type(), 1);
        let worn_sword = worn(&sword, 100);
        let other = worn(
            &enchanted(Kind::DiamondSword, &[(Enchantment::Unbreaking, 1)]),
            100,
        );
        let both = craft(&worn_sword, &other, None, false).unwrap();
        let max = u32::from(left(&sword));
        assert_eq!(u32::from(left(&both.result)), 200 + max * 12 / 100);
        assert_eq!(both.cost, 2 + 2);
        // Too expensive outside creative.
        let costly = with_prior_work(&worn_sword, 39);
        assert_eq!(
            craft(&costly, &Stack::EMPTY, Some("x"), false)
                .unwrap()
                .cost,
            39
        );
        assert_eq!(
            craft(&costly, &other, None, false),
            Err(Refused::TooExpensive)
        );
        assert!(craft(&costly, &other, None, true).is_ok());
    }

    /// Repeated entries count each: each input entry an enchantment conflicts with costs a
    /// level, and a repeated enchantment of the material joins against the input's level again.
    #[test]
    fn repeated_enchantment_entries_count_each() {
        let helmet = enchanted(
            Kind::DiamondHelmet,
            &[(Enchantment::Protection, 1), (Enchantment::Protection, 1)],
        );
        let book = enchanted(
            Kind::EnchantedBook,
            &[
                (Enchantment::BlastProtection, 1),
                (Enchantment::Unbreaking, 1),
            ],
        );
        let out = craft(&helmet, &book, None, false).unwrap();
        // Two conflicts, then uncommon (2), halved for a book, times one level.
        assert_eq!(out.cost, 2 + 1);
        assert_eq!(
            out.result.extra().unwrap().enchantments,
            [
                (Enchantment::Protection, 1),
                (Enchantment::Protection, 1),
                (Enchantment::Unbreaking, 1)
            ]
        );
        let sword = enchanted(Kind::DiamondSword, &[(Enchantment::Sharpness, 2)]);
        let twice = enchanted(
            Kind::EnchantedBook,
            &[(Enchantment::Sharpness, 2), (Enchantment::Sharpness, 2)],
        );
        let out = craft(&sword, &twice, None, false).unwrap();
        assert_eq!(out.cost, 1 + 1);
        assert_eq!(
            out.result.extra().unwrap().enchantments,
            [(Enchantment::Sharpness, 3)]
        );
    }
}
