//! Campfires and soul campfires (PMMP `block\Campfire::onScheduledUpdate`, `tile\Campfire`): up to
//! four items cook while the fire is lit, ten ticks per update; a cooked item drops as its
//! campfire recipe's result above the fire, and an item without a recipe stays on it.

use crate::block::{self, Class};
use crate::crafting::FurnaceType;
use crate::custom::Registry;
use crate::item::Stack;
use crate::nbt::{Compound, Value};
use crate::world::BlockPos;

/// The slots of a campfire.
pub(in crate::tick) const SLOTS: usize = 4;
/// Ticks between cooking updates (PMMP `UPDATE_INTERVAL_TICKS`).
pub(in crate::tick) const INTERVAL_TICKS: u16 = 10;
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

impl Campfire {
    /// PMMP `Campfire::readSaveData` of the campfire at `pos` into `items` (one item a slot);
    /// times are kept up to the longer of the two campfire cook times.
    pub(in crate::tick) fn read(
        nbt: &mut Compound,
        pos: BlockPos,
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
                *slot = super::decor::read_stack(&item, pos, "campfire", custom).with_count(1);
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

    /// One cooking update of a lit campfire of `kind`: every item cooks [`INTERVAL_TICKS`] more;
    /// returns the slots whose item is done with what each turns into by PMMP's or the server's
    /// custom recipes (`custom`). An item without a recipe is never done: it stays on the fire
    /// (PMMP turns it into nothing; no item is lost). The caller empties those slots once the
    /// result has dropped.
    pub(in crate::tick) fn step(
        &mut self,
        kind: FurnaceType,
        items: &[Stack],
        custom: &Registry,
    ) -> Vec<(usize, Stack)> {
        let max = kind.cook_ticks();
        let mut done = Vec::new();
        for (i, (item, time)) in items.iter().zip(self.times.iter_mut()).enumerate() {
            if item.is_empty() {
                continue;
            }
            *time = time.saturating_add(INTERVAL_TICKS).min(max);
            if *time < max {
                continue;
            }
            if let Some(result) = custom
                .furnace_recipe(kind, item)
                .and_then(|i| custom.furnace_at(i))
                .and_then(|r| r.result.to_item())
            {
                done.push((i, result));
            }
        }
        done
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
}

/// The recipe kind of a campfire block: a soul campfire cooks its own recipes.
pub(in crate::tick) fn kind_of(block: block::State) -> FurnaceType {
    if block.is(Class::SoulCampfire) {
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
        let kind = kind_of(block::Kind::Campfire.default_state());
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
        let pos = BlockPos::new(1, 2, 3);
        assert_eq!(Campfire::read(&mut nbt, pos, &mut back, &none), c);
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
        let pos = BlockPos::new(1, 2, 3);
        let c = Campfire::read(&mut nbt, pos, &mut items, &Registry::default());
        assert!(items.iter().all(Stack::is_empty));
        assert_eq!(c.times, [0; SLOTS], "a new item cooks from 0");
    }

    /// An item without a campfire recipe stays on the fire however long it cooks.
    #[test]
    fn an_item_without_a_recipe_stays_on_the_fire() {
        let mut c = Campfire::default();
        let stick = Stack::new(Kind::Stick.default_type(), 1);
        let items = vec![stick, Stack::EMPTY, Stack::EMPTY, Stack::EMPTY];
        for _ in 0..100 {
            let done = c.step(FurnaceType::Campfire, &items, &Registry::default());
            assert!(done.is_empty(), "{done:?}");
        }
    }
}
