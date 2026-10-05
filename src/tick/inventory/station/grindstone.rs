//! Grindstone rules (user exception, PLAN.md 8.4; PMMP has no grindstone): one enchanted item
//! loses every enchantment but the curses, two items of the same kind that wear join into one
//! with both durabilities and 5% of the maximum and only the curses of both, and the enchantments
//! taken off give experience (BDS P6, `xtask/data/p6/m9-facts.md` STATIONS; dragonfly
//! `handleGrindstoneCraft` where BDS cannot show it).

use crate::item::{self, Enchantment, Kind, Stack};
use crate::random::Random;

/// The prior work key the grindstone clears (dragonfly `WithAnvilCost(0)`).
const REPAIR_COST: &str = "RepairCost";

/// What a grindstone makes of its two slots.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::tick) struct Outcome {
    /// The result, one item.
    pub(in crate::tick) result: Stack,
    /// The enchanting power of the enchantments taken off, which sets the experience it gives
    /// ([`experience`]).
    pub(in crate::tick) power: u32,
}

/// Why the grindstone makes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tick) enum Refused {
    /// Both slots are empty.
    NoInput,
    /// A slot holds more than one item, or the two items are not the same kind of item that
    /// wears.
    Mismatch,
    /// One item alone without an enchantment to take off.
    Nothing,
    /// The result would be over a stack limit.
    TooLarge(item::Error),
}

/// Whether an enchantment stays on a ground item.
fn is_curse(e: Enchantment) -> bool {
    e == Enchantment::Vanishing
}

/// The grindstone's outcome for its input and additional slot.
pub(in crate::tick) fn craft(input: &Stack, additional: &Stack) -> Result<Outcome, Refused> {
    let (base, other) = match (input.is_empty(), additional.is_empty()) {
        (true, true) => return Err(Refused::NoInput),
        (false, true) => (input, None),
        (true, false) => (additional, None),
        (false, false) => (input, Some(additional)),
    };
    if input.count() > 1 || additional.count() > 1 {
        return Err(Refused::Mismatch);
    }
    let max = base.item().and_then(item::Type::max_durability);
    if let Some(other) = other
        && (other.item() != base.item() || max.is_none())
    {
        return Err(Refused::Mismatch);
    }
    let mut extra = base.extra().cloned().unwrap_or_default();
    let mut power = 0u32;
    let mut curses = Vec::new();
    for stack in std::iter::once(base).chain(other) {
        for &(e, level) in stack.extra().map_or(&[][..], |x| x.enchantments.as_slice()) {
            if is_curse(e) {
                if !curses.iter().any(|(c, _)| *c == e) {
                    curses.push((e, level));
                }
            } else {
                let level = level.min(e.max_level());
                power = power.saturating_add(e.min_enchanting_power(level).map_or(0, u32::from));
            }
        }
    }
    if other.is_none() && extra.enchantments.len() == curses.len() {
        return Err(Refused::Nothing);
    }
    extra.enchantments = curses;
    extra.nbt.remove(REPAIR_COST);
    let mut result = base.with_extra(Some(extra)).map_err(Refused::TooLarge)?;
    if let (Some(other), Some(max)) = (other, max) {
        let left = |s: &Stack| u32::from(max).saturating_sub(u32::from(s.damage()));
        let total = left(base) + left(other) + u32::from(max) * 5 / 100;
        let damage = u32::from(max).saturating_sub(total);
        result = result.with_damage(u16::try_from(damage).unwrap_or(0));
    }
    // An enchanted book with nothing left on it becomes a book.
    if result
        .item()
        .is_some_and(|t| t.kind() == Some(Kind::EnchantedBook))
        && result.extra().is_none_or(|e| e.enchantments.is_empty())
    {
        result = Stack::new(Kind::Book.default_type(), 1)
            .with_extra(result.extra().cloned())
            .map_err(Refused::TooLarge)?;
    }
    Ok(Outcome { result, power })
}

/// The experience a craft of enchanting power `power` gives: half of it rounded up, plus up to
/// as much again at random (dragonfly `experienceFromEnchantments`).
pub(in crate::tick) fn experience(power: u32, rng: &mut Random) -> u32 {
    let half = power.div_ceil(2);
    if half == 0 {
        return 0;
    }
    half + rng.below(half)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::Enchantment::{Efficiency, Sharpness, Unbreaking, Vanishing};
    use crate::item::Extra;
    use crate::nbt::Value;

    fn stack(kind: Kind, count: u8, damage: u16, list: &[(Enchantment, u8)]) -> Stack {
        Stack::new(kind.default_type(), count)
            .with_damage(damage)
            .with_extra(Some(Extra {
                enchantments: list.to_vec(),
                ..Extra::default()
            }))
            .unwrap()
    }

    #[test]
    fn one_item_keeps_only_its_curses_its_name_and_no_prior_work() {
        let mut sword = stack(Kind::IronSword, 1, 30, &[(Sharpness, 5), (Vanishing, 1)]);
        let mut extra = sword.extra().cloned().unwrap();
        extra.custom_name = Some("Edge".to_owned());
        extra.nbt.insert(REPAIR_COST.to_owned(), Value::Int(7));
        sword = sword.with_extra(Some(extra)).unwrap();
        let out = craft(&sword, &Stack::EMPTY).unwrap();
        assert_eq!(out.result.damage(), 30);
        assert_eq!(out.result.custom_name(), "Edge");
        let extra = out.result.extra().unwrap();
        assert_eq!(extra.enchantments, [(Vanishing, 1)]);
        assert!(!extra.nbt.contains_key(REPAIR_COST));
        let power = Sharpness.min_enchanting_power(5).unwrap();
        assert_eq!(out.power, u32::from(power));
        // The additional slot alone works the same.
        assert_eq!(craft(&Stack::EMPTY, &sword), Ok(out));
    }

    #[test]
    fn two_items_join_their_durability_and_their_curses_and_stop_at_full() {
        let a = stack(Kind::DiamondPickaxe, 1, 1000, &[(Efficiency, 4)]);
        let b = stack(
            Kind::DiamondPickaxe,
            1,
            1500,
            &[(Unbreaking, 3), (Vanishing, 1)],
        );
        let max = u32::from(
            Kind::DiamondPickaxe
                .default_type()
                .max_durability()
                .unwrap(),
        );
        let out = craft(&a, &b).unwrap();
        let left = (max - 1000) + (max - 1500) + max * 5 / 100;
        assert_eq!(u32::from(out.result.damage()), max - left);
        assert_eq!(out.result.extra().unwrap().enchantments, [(Vanishing, 1)]);
        let power = Efficiency.min_enchanting_power(4).unwrap()
            + Unbreaking.min_enchanting_power(3).unwrap();
        assert_eq!(out.power, u32::from(power));
        let fresh = craft(
            &stack(Kind::DiamondPickaxe, 1, 1, &[]),
            &stack(Kind::DiamondPickaxe, 1, 2, &[]),
        )
        .unwrap();
        assert_eq!(fresh.result.damage(), 0, "never past full");
        assert_eq!(fresh.power, 0);
    }

    #[test]
    fn mismatched_stacked_plain_and_empty_inputs_make_nothing() {
        let sword = stack(Kind::IronSword, 1, 0, &[(Sharpness, 1)]);
        assert_eq!(craft(&Stack::EMPTY, &Stack::EMPTY), Err(Refused::NoInput));
        assert_eq!(
            craft(&sword, &stack(Kind::GoldenSword, 1, 0, &[])),
            Err(Refused::Mismatch)
        );
        let book = stack(Kind::EnchantedBook, 1, 0, &[(Sharpness, 1)]);
        assert_eq!(
            craft(&book, &book),
            Err(Refused::Mismatch),
            "books do not wear"
        );
        let sticks = stack(Kind::Stick, 2, 0, &[(Sharpness, 1)]);
        assert_eq!(craft(&sticks, &Stack::EMPTY), Err(Refused::Mismatch));
        assert_eq!(
            craft(
                &stack(Kind::IronSword, 1, 5, &[(Vanishing, 1)]),
                &Stack::EMPTY
            ),
            Err(Refused::Nothing)
        );
    }

    #[test]
    fn an_enchanted_book_without_curses_becomes_a_book() {
        let book = stack(Kind::EnchantedBook, 1, 0, &[(Sharpness, 2)]);
        let out = craft(&book, &Stack::EMPTY).unwrap();
        assert_eq!(out.result.item(), Some(Kind::Book.default_type()));
        assert!(out.result.extra().is_none_or(|e| e.enchantments.is_empty()));
        let cursed = stack(Kind::EnchantedBook, 1, 0, &[(Sharpness, 2), (Vanishing, 1)]);
        let out = craft(&cursed, &Stack::EMPTY).unwrap();
        assert_eq!(out.result.item(), Some(Kind::EnchantedBook.default_type()));
    }

    #[test]
    fn experience_is_half_the_power_rounded_up_plus_up_to_as_much_again() {
        let mut rng = Random::new(1, 2, 3);
        assert_eq!(experience(0, &mut rng), 0);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..2000 {
            let x = experience(23, &mut rng);
            assert!((12..24).contains(&x), "{x}");
            seen.insert(x);
        }
        assert_eq!(seen.len(), 12, "every amount in the range");
    }
}
