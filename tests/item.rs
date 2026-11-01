//! Public item type lookups: known names resolve, unknown input is `None` without panicking, and
//! a vanilla stack reads back from its disk form.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use anchor::block::ItemForm;
use anchor::item::{Kind, Stack, Type};
use anchor::nbt::{Compound, Value};

/// Item names of the PMMP dump, as the lookup knows them, with a meta each type has.
const NAMES: &[(&str, u16)] = &[
    ("minecraft:stick", 0),
    ("minecraft:diamond_sword", 0),
    ("minecraft:potion", 0),
    ("minecraft:potion", 5),
    ("minecraft:banner", 0),
    ("minecraft:banner", 15),
    ("minecraft:bed", 14),
    ("minecraft:firework_star", 0),
];

/// splitmix64: a fixed-seed generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number below `n`.
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).expect("n fits u64")).expect("below n")
    }
}

#[test]
fn known_names_resolve() {
    let empty = Compound::new();
    for &(name, meta) in NAMES {
        let t = Type::from_name(name, meta, None, &empty).expect(name);
        assert_eq!(t.name(), name);
        assert_eq!(t.meta(), meta);
        assert!(t.block().is_none());
    }
    // The ominous banner is white (meta 15) as BDS sends it, told apart from the white banner by
    // its type NBT.
    let ominous: Compound = [("Type".to_owned(), Value::Int(1))].into();
    let banner = Type::from_name("minecraft:banner", 15, None, &ominous).expect("ominous banner");
    assert_ne!(
        Some(banner),
        Type::from_name("minecraft:banner", 15, None, &empty)
    );
}

#[test]
fn unknown_names_are_none() {
    let mut rng = Rng(0x5eed_0002);
    let empty = Compound::new();
    let ominous: Compound = [("Type".to_owned(), Value::Int(1))].into();
    let alphabet = b"abcdefghijklmnopqrstuvwxyz_:.0123456789\xc3\xa9";
    for _ in 0..100_000 {
        let (base, base_meta) = NAMES[rng.below(NAMES.len())];
        let mut bytes = base.as_bytes().to_vec();
        let mutated = rng.below(2) == 0;
        if mutated {
            for _ in 0..=rng.below(3) {
                let at = rng.below(bytes.len() + 1);
                match rng.below(3) {
                    0 => bytes.insert(at, alphabet[rng.below(alphabet.len())]),
                    1 if at < bytes.len() => {
                        bytes.remove(at);
                    }
                    _ => bytes.truncate(at),
                }
            }
        }
        let name = String::from_utf8_lossy(&bytes);
        let meta = if rng.below(2) == 0 {
            base_meta
        } else {
            u16::try_from(rng.next() >> 48).unwrap()
        };
        let nbt = if rng.below(2) == 0 { &empty } else { &ominous };
        let found = Type::from_name(&name, meta, None, nbt);
        if let Some(t) = found {
            assert_eq!(t.name(), name);
        } else {
            assert!(mutated || meta != base_meta, "{name} {meta}");
        }
        if !NAMES.iter().any(|(n, _)| *n == name) && !name.starts_with("minecraft:") {
            assert_eq!(found, None);
        }
    }
}

/// The disk form reads back without a server: vanilla items and block items, and the empty
/// stack; an unknown item or a compound that is no item is `None`.
#[test]
fn a_vanilla_stack_reads_back_from_its_disk_form() {
    let worn = Stack::new(Kind::DiamondSword.default_type(), 1)
        .with_damage(42)
        .unwrap();
    let ItemForm::Item(stone) = anchor::block::Kind::Stone.default_state().item() else {
        panic!("stone has an item");
    };
    for stack in [
        Stack::new(Kind::Stick.default_type(), 5),
        worn,
        Stack::new(stone, 64),
        Stack::EMPTY,
    ] {
        assert_eq!(Stack::from_nbt(&stack.to_nbt()), Some(stack));
    }
    let mut unknown = Stack::new(Kind::Stick.default_type(), 1).to_nbt();
    unknown.insert(
        "Name".to_owned(),
        Value::String("minecraft:no_such_item".to_owned()),
    );
    assert_eq!(Stack::from_nbt(&unknown), None);
    assert_eq!(Stack::from_nbt(&Compound::new()), None);
}
