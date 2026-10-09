//! Campfires and soul campfires (PMMP `block\Campfire::onScheduledUpdate`, `tile\Campfire`): up to
//! four items cook while the fire is lit, ten ticks per update; a cooked item drops as its
//! campfire recipe's result above the fire.

use crate::crafting::FurnaceType;
use crate::custom::Registry;
use crate::item::Stack;
use crate::nbt::{Compound, Value};

/// The slots of a campfire.
pub(in crate::tick) const SLOTS: usize = 4;
/// Ticks between cooking updates (PMMP `UPDATE_INTERVAL_TICKS`).
pub(in crate::tick) const INTERVAL: u64 = 10;
/// The saved item and time keys of each slot (PMMP `TAG_*_INPUT_ITEM`, `TAG_*_COOKING_TIME`).
const KEYS: [(&str, &str); SLOTS] = [
    ("Item1", "ItemTime1"),
    ("Item2", "ItemTime2"),
    ("Item3", "ItemTime3"),
    ("Item4", "ItemTime4"),
];

/// A campfire's cooking times. Whether it is a soul campfire (with its own recipes) is the
/// block's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::tick) struct Campfire {
    /// Ticks each slot's item has cooked.
    pub(in crate::tick) times: [u16; SLOTS],
}

/// The recipe kind of a campfire block.
pub(in crate::tick) fn kind_of(soul: bool) -> FurnaceType {
    if soul {
        FurnaceType::SoulCampfire
    } else {
        FurnaceType::Campfire
    }
}

/// Whether an item can go onto a campfire of `kind`: it has a recipe there, PMMP's or one of
/// the server's custom ones (PMMP `Campfire::onInteract`).
pub(in crate::tick) fn cooks(kind: FurnaceType, item: &Stack, custom: &Registry) -> bool {
    custom.furnace_recipe(kind, &item.with_count(1)).is_some()
}

impl Campfire {
    /// PMMP `Campfire::readSaveData` into `items` (one item a slot); times are kept up to the
    /// longer of the two campfire cook times.
    pub(in crate::tick) fn read(
        nbt: &mut Compound,
        items: &mut [Stack],
        custom: &Registry,
    ) -> Self {
        let mut out = Self { times: [0; SLOTS] };
        let max = FurnaceType::Campfire
            .cook_ticks()
            .max(FurnaceType::SoulCampfire.cook_ticks());
        for ((item_key, time_key), (slot, time)) in
            KEYS.iter().zip(items.iter_mut().zip(out.times.iter_mut()))
        {
            if let Some(Value::Compound(item)) = nbt.remove(*item_key) {
                *slot = super::decor::read_stack(&item, "campfire", custom).with_count(1);
            }
            // A slot whose item was left out keeps no time, as one emptied in play.
            if let Some(Value::Int(t)) = nbt.remove(*time_key)
                && !slot.is_empty()
            {
                *time = u16::try_from(t.max(0)).unwrap_or(u16::MAX).min(max);
            }
        }
        out
    }

    /// The saved keys of the non-empty slots (PMMP `writeSaveData`).
    pub(in crate::tick) fn write(&self, items: &[Stack], nbt: &mut Compound) {
        for ((item_key, time_key), (item, time)) in
            KEYS.iter().zip(items.iter().zip(self.times.iter()))
        {
            if !item.is_empty() {
                nbt.insert((*item_key).to_owned(), Value::Compound(item.to_nbt()));
                nbt.insert((*time_key).to_owned(), Value::Int(i32::from(*time)));
            }
        }
    }

    /// The client keys: the items (PMMP `addAdditionalSpawnData`).
    pub(in crate::tick) fn spawn(items: &[Stack], nbt: &mut Compound) {
        for ((item_key, _), item) in KEYS.iter().zip(items) {
            if !item.is_empty() {
                nbt.insert((*item_key).to_owned(), Value::Compound(item.to_nbt()));
            }
        }
    }

    /// One cooking update of a lit campfire of `kind`: every item cooks [`INTERVAL`] ticks more;
    /// returns the slots whose item is done with what each turns into by PMMP's or the server's
    /// custom recipes (`custom`; an item without a recipe turns into nothing). The caller
    /// empties those slots once the result has dropped.
    pub(in crate::tick) fn step(
        &mut self,
        kind: FurnaceType,
        items: &[Stack],
        custom: &Registry,
    ) -> Vec<(usize, Stack)> {
        let max = kind.cook_ticks();
        let interval = u16::try_from(INTERVAL).unwrap_or(u16::MAX);
        let mut done = Vec::new();
        for (i, (item, time)) in items.iter().zip(self.times.iter_mut()).enumerate() {
            if item.is_empty() {
                continue;
            }
            *time = time.saturating_add(interval).min(max);
            if *time >= max {
                let result = custom
                    .furnace_recipe(kind, item)
                    .and_then(|i| custom.furnace_at(i))
                    .and_then(|r| r.result.to_item())
                    .unwrap_or_default();
                done.push((i, result));
            }
        }
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::Kind;

    fn named(name: &str) -> crate::item::Type {
        crate::item::Type::from_name(name, 0, None, &crate::nbt::Compound::new()).unwrap()
    }

    #[test]
    fn food_cooks_in_30_seconds_and_is_kept_with_its_time() {
        let mut c = Campfire::default();
        let kind = kind_of(false);
        let none = Registry::default();
        let beef = Stack::new(named("minecraft:beef"), 1);
        assert!(cooks(kind, &beef, &none));
        assert!(!cooks(
            kind,
            &Stack::new(Kind::Stick.default_type(), 1),
            &none
        ));
        let items = vec![beef.clone(), Stack::EMPTY, Stack::EMPTY, Stack::EMPTY];
        for _ in 0..59 {
            assert!(c.step(kind, &items, &none).is_empty());
        }
        let done = c.step(kind, &items, &none);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].0, 0);
        assert_eq!(done[0].1.item(), Some(named("minecraft:cooked_beef")));
        let mut nbt = Compound::new();
        c.write(&items, &mut nbt);
        let mut back = vec![Stack::EMPTY; SLOTS];
        assert_eq!(Campfire::read(&mut nbt, &mut back, &none), c);
        assert_eq!(back, items);
    }

    #[test]
    fn a_saved_time_without_its_item_is_not_kept() {
        let mut nbt = Compound::from([
            (
                "Item1".to_owned(),
                Value::Compound(Compound::from([(
                    "Name".to_owned(),
                    Value::String("minecraft:no_such_item".to_owned()),
                )])),
            ),
            ("ItemTime1".to_owned(), Value::Int(590)),
            ("ItemTime2".to_owned(), Value::Int(300)),
        ]);
        let mut items = vec![Stack::EMPTY; SLOTS];
        let c = Campfire::read(&mut nbt, &mut items, &Registry::default());
        assert!(items.iter().all(Stack::is_empty));
        assert_eq!(c.times, [0; SLOTS], "a new item cooks from 0");
    }
}
