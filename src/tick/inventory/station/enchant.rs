//! Enchanting table options (PMMP `EnchantingHelper`): the bookshelves around the table, three
//! options from the player's enchanting seed, and the enchanted item an option makes.
//!
//! The draws come from the engine's random streams keyed by the seed alone, so the same seed and
//! input always give the same options (PMMP draws them from its `Random` seeded the same way).

use crate::block;
use crate::item::{ArmorSlot, Class, Enchantment, Kind, Stack, Type};
use crate::random::{self, Random};
use crate::world::{BlockPos, World};

/// The most bookshelves that count (PMMP `MAX_BOOKSHELF_COUNT`).
const MAX_BOOKSHELVES: u8 = 15;
/// The random stream purpose of options.
const OPTIONS: u64 = random::tag("enchant-options");

/// One enchanting option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tick) struct Choice {
    /// The level the player needs (PMMP `getRequiredXpLevel`).
    pub(in crate::tick) level: u8,
    /// A name the client shows in the enchanting alphabet.
    pub(in crate::tick) name: String,
    /// What the option enchants, in the order chosen.
    pub(in crate::tick) enchantments: Vec<(Enchantment, u8)>,
}

/// The bookshelves that power a table at `pos` (PMMP `countBookshelves`): the two-high ring two
/// blocks out, each column only with air between it and the table, at most 15.
pub(in crate::tick) fn bookshelves(world: &World, pos: BlockPos) -> u8 {
    let mut count = 0;
    for x in -2i32..=2 {
        for z in -2i32..=2 {
            if x.abs() != 2 && z.abs() != 2 {
                continue;
            }
            let (sx, sz) = (x.clamp(-1, 1), z.clamp(-1, 1));
            let open = (0..=1).all(|y| {
                world.block(BlockPos::new(pos.x + sx, pos.y + y, pos.z + sz)) == Some(block::AIR)
            });
            if !open {
                continue;
            }
            for y in 0..=1 {
                let at = BlockPos::new(pos.x + x, pos.y + y, pos.z + z);
                if world
                    .block(at)
                    .is_some_and(|b| b.kind() == block::Kind::Bookshelf)
                {
                    count += 1;
                    if count == MAX_BOOKSHELVES {
                        return count;
                    }
                }
            }
        }
    }
    count
}

/// The three options for `input` with `bookshelves` and the player's `seed` (PMMP
/// `generateOptions`); none for an empty or already enchanted item, or one that takes no
/// enchantments.
pub(in crate::tick) fn options(input: &Stack, bookshelves: u8, seed: i32) -> Vec<Choice> {
    let Some(item) = input.item() else {
        return Vec::new();
    };
    let enchanted = input.extra().is_some_and(|e| !e.enchantments.is_empty());
    if enchanted || item.primary_enchantments().is_empty() {
        return Vec::new();
    }
    let mut rng = Random::new(i64::from(seed), 0, random::key(OPTIONS, &[]));
    let shelves = i32::from(bookshelves);
    let base = rng.range(1, 8) + (shelves >> 1) + rng.range(0, shelves);
    let top = (base / 3).max(1);
    let middle = base * 2 / 3 + 1;
    let bottom = base.max(shelves * 2);
    [top, middle, bottom]
        .into_iter()
        .map(|level| choice(&mut rng, input, level))
        .collect()
}

/// PMMP `createOption`.
fn choice(rng: &mut Random, input: &Stack, level: i32) -> Choice {
    let item = input.item();
    let enchantability = i32::from(item.map_or(0, crate::item::Type::enchantability));
    let mut power =
        level + rng.range(0, enchantability >> 2) + rng.range(0, enchantability >> 2) + 1;
    // A bonus between 0.85 and 1.15.
    let bonus = 1.0 + (rng.unit() + rng.unit() - 1.0) * 0.15;
    power = round(f64::from(power) * bonus);
    let mut available = available(power, input);
    let mut chosen = Vec::new();
    if let Some(last) = weighted(rng, &available) {
        chosen.push(last);
        let mut last = last;
        while rng.unit() <= f64::from(power + 1) / 50.0 {
            available.retain(|(e, _)| *e != last.0 && e.is_compatible_with(last.0));
            let Some(next) = weighted(rng, &available) else {
                break;
            };
            chosen.push(next);
            last = next;
            power >>= 1;
        }
    }
    Choice {
        level: u8::try_from(level.clamp(0, 255)).unwrap_or(u8::MAX),
        name: name(rng),
        enchantments: chosen,
    }
}

/// PHP `round`: half away from zero, as an `i32`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "safety: the power is at most a few hundred"
)]
fn round(v: f64) -> i32 {
    v.round() as i32
}

/// PMMP `getAvailableEnchantments`: for each enchantment the table can give the item, its
/// highest level whose power range holds `power`.
fn available(power: i32, input: &Stack) -> Vec<(Enchantment, u8)> {
    let Some(item) = input.item() else {
        return Vec::new();
    };
    let mut list = Vec::new();
    for e in item.primary_enchantments().iter() {
        for level in (1..=e.max_level()).rev() {
            let (Some(min), Some(max)) =
                (e.min_enchanting_power(level), e.max_enchanting_power(level))
            else {
                continue;
            };
            if (i32::from(min)..=i32::from(max)).contains(&power) {
                list.push((e, level));
                break;
            }
        }
    }
    list
}

/// PMMP `getRandomWeightedEnchantment`: by rarity weight.
fn weighted(rng: &mut Random, list: &[(Enchantment, u8)]) -> Option<(Enchantment, u8)> {
    let total: i32 = list.iter().map(|(e, _)| i32::from(e.rarity())).sum();
    if list.is_empty() || total <= 0 {
        return None;
    }
    let mut left = rng.range(1, total);
    for &(e, level) in list {
        left -= i32::from(e.rarity());
        if left <= 0 {
            return Some((e, level));
        }
    }
    None
}

/// PMMP `getRandomOptionName`: 5 to 15 letters.
fn name(rng: &mut Random) -> String {
    let len = rng.range(5, 15);
    (0..len)
        .map(|_| {
            char::from(u8::try_from(rng.range(i32::from(b'a'), i32::from(b'z'))).unwrap_or(b'a'))
        })
        .collect()
}

/// The item an option makes of `input` (PMMP `enchantItem`): a book becomes an enchanted book;
/// each enchantment replaces one of its type.
pub(in crate::tick) fn enchant(input: &Stack, enchantments: &[(Enchantment, u8)]) -> Stack {
    let base = if input.item().and_then(crate::item::Type::kind) == Some(Kind::Book) {
        Stack::new(Kind::EnchantedBook.default_type(), input.count())
    } else {
        input.clone()
    };
    let mut extra = base.extra().cloned().unwrap_or_default();
    for &(e, level) in enchantments {
        match extra.enchantments.iter_mut().find(|(x, _)| *x == e) {
            Some(slot) => slot.1 = level,
            None => extra.enchantments.push((e, level)),
        }
    }
    base.with_extra(Some(extra))
}

/// The `Slot` of the `PlayerEnchantOptions` for `input`: the item's enchantment slot flag (PMMP
/// `ItemFlags`, which BDS sends: a pickaxe is 1024, P6 `enchant`); 0 for other items.
///
/// Verify (M9): the armour flags and a book's value were not seen from BDS.
pub(in crate::tick) fn slot(input: Option<Type>) -> i32 {
    let Some(item) = input else {
        return 0;
    };
    if let Some(armor) = item.armor() {
        return match armor.slot() {
            ArmorSlot::Head => 0x1,
            ArmorSlot::Chest => 0x2,
            ArmorSlot::Legs => 0x4,
            ArmorSlot::Feet => 0x8,
        };
    }
    [
        (Class::Sword, 0x10),
        (Class::Bow, 0x20),
        (Class::Hoe, 0x40),
        (Class::Shears, 0x80),
        (Class::FlintSteel, 0x100),
        (Class::Axe, 0x200),
        (Class::Pickaxe, 0x400),
        (Class::Shovel, 0x800),
        (Class::FishingRod, 0x1000),
        (Class::Trident, 0x8000),
    ]
    .into_iter()
    .find_map(|(class, flag)| item.is(class).then_some(flag))
    .unwrap_or(0)
}

/// The list of a `PlayerEnchantOptions` option an enchantment goes in: 0 for armour
/// enchantments worn (equipped), 1 for those that act through the held item, 2 for the item's
/// own (self). The lists are the ones gophertunnel documents for the vanilla server, and BDS sends
/// a pickaxe's unbreaking in list 1 and efficiency in list 2 (P6 `enchant`). Swift sneak, missing
/// from that documentation, is worn armour (verify (M9); no enchanting table gives it).
pub(in crate::tick) fn activation(e: Enchantment) -> usize {
    match e {
        Enchantment::Protection
        | Enchantment::FireProtection
        | Enchantment::FeatherFalling
        | Enchantment::BlastProtection
        | Enchantment::ProjectileProtection
        | Enchantment::Thorns
        | Enchantment::Respiration
        | Enchantment::AquaAffinity
        | Enchantment::FrostWalker
        | Enchantment::SwiftSneak => 0,
        Enchantment::Sharpness
        | Enchantment::FireAspect
        | Enchantment::SilkTouch
        | Enchantment::Unbreaking
        | Enchantment::Fortune
        | Enchantment::Flame => 1,
        Enchantment::Knockback
        | Enchantment::Efficiency
        | Enchantment::Power
        | Enchantment::Punch
        | Enchantment::Infinity
        | Enchantment::Mending
        | Enchantment::Vanishing => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_are_deterministic_from_the_seed_and_within_pmmp_power_ranges() {
        let pickaxe = Stack::new(Kind::IronPickaxe.default_type(), 1);
        for shelves in [0, 7, 15] {
            for seed in [-5, 0, 1, 77, i32::MAX] {
                let a = options(&pickaxe, shelves, seed);
                assert_eq!(a, options(&pickaxe, shelves, seed));
                assert_eq!(a.len(), 3);
                // PMMP's levels: top max(base / 3, 1), middle base * 2 / 3 + 1, bottom
                // max(base, shelves * 2), with base in 1 + shelves / 2 ..= 8 + shelves / 2 +
                // shelves.
                let lo = 1 + i32::from(shelves >> 1);
                let hi = 8 + i32::from(shelves >> 1) + i32::from(shelves);
                let bottom = i32::from(a[2].level);
                assert!(
                    bottom >= lo.max(i32::from(shelves) * 2)
                        && bottom <= hi.max(i32::from(shelves) * 2)
                );
                assert!(a[0].level <= a[1].level && a[1].level <= a[2].level);
                for c in &a {
                    assert!((5..=15).contains(&c.name.len()));
                    assert!(c.name.bytes().all(|b| b.is_ascii_lowercase()));
                    for (e, level) in &c.enchantments {
                        assert!(pickaxe.item().unwrap().primary_enchantments().contains(*e));
                        assert!(*level >= 1 && *level <= e.max_level());
                    }
                    for (i, (x, _)) in c.enchantments.iter().enumerate() {
                        for (y, _) in &c.enchantments[i + 1..] {
                            assert!(x != y && x.is_compatible_with(*y));
                        }
                    }
                }
            }
        }
        assert!(options(&Stack::new(Kind::Stick.default_type(), 1), 15, 1).is_empty());
        let enchanted = enchant(&pickaxe, &[(Enchantment::Efficiency, 1)]);
        assert!(options(&enchanted, 15, 1).is_empty());
    }

    #[test]
    fn a_book_becomes_an_enchanted_book() {
        let book = Stack::new(Kind::Book.default_type(), 1);
        let out = enchant(&book, &[(Enchantment::Unbreaking, 2)]);
        assert_eq!(out.item(), Some(Kind::EnchantedBook.default_type()));
        assert_eq!(
            out.extra().unwrap().enchantments,
            vec![(Enchantment::Unbreaking, 2)]
        );
    }
}
