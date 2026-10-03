//! Brewing stands (PMMP `tile\BrewingStand::onUpdate`): a blaze powder gives 20 brews of fuel; a
//! brew takes 400 ticks and turns every bottle the ingredient has a recipe for, then uses up one
//! ingredient.

use crate::item::Stack;
use crate::nbt::{Compound, Value};

/// The ingredient slot.
pub(in crate::tick) const INGREDIENT: usize = 0;
/// The three bottle slots.
pub(in crate::tick) const BOTTLES: [usize; 3] = [1, 2, 3];
/// The fuel slot.
pub(in crate::tick) const FUEL: usize = 4;
/// The slots of a brewing stand.
pub(in crate::tick) const SLOTS: usize = 5;
/// Ticks one brew takes (PMMP `BREW_TIME_TICKS`).
pub(in crate::tick) const BREW_TICKS: u16 = 400;
/// Brews one blaze powder fuels (PMMP `BrewingFuelUseEvent` default).
pub(in crate::tick) const FUEL_BREWS: u16 = 20;
/// `ContainerSetData` keys (PMMP `PROPERTY_BREWING_STAND_*`).
pub(in crate::tick) const DATA_BREW_TIME: i32 = 0;
pub(in crate::tick) const DATA_FUEL_AMOUNT: i32 = 1;
pub(in crate::tick) const DATA_FUEL_TOTAL: i32 = 2;

/// A brewing stand's timers.
#[derive(Debug, Clone, Default)]
pub(in crate::tick) struct Brewing {
    /// Ticks left of the brew in progress (PMMP `brewTime`), 0 when none.
    pub(in crate::tick) brew: u16,
    /// Brews of fuel left.
    pub(in crate::tick) fuel: u16,
    /// Brews the last blaze powder gave.
    pub(in crate::tick) fuel_total: u16,
    /// The results of the bottles for the ingredient last looked up: derived, so neither saved,
    /// hashed nor compared.
    recipes: Option<([Stack; 4], [Option<Stack>; 3])>,
}

impl PartialEq for Brewing {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            brew,
            fuel,
            fuel_total,
            recipes: _,
        } = self;
        (*brew, *fuel, *fuel_total) == (other.brew, other.fuel, other.fuel_total)
    }
}

/// What one brewing tick did.
#[derive(Debug, Clone, Default, PartialEq)]
pub(in crate::tick) struct Step {
    /// Bit `i`: slot `i` changed.
    pub(in crate::tick) slots: u8,
    /// Bit `i`: slot `i` changed only its count (the same item before and after), so its viewers
    /// keep its stack network ID (BDS, P6 F2: a furnace's output growing, its input shrinking).
    pub(in crate::tick) kept: u8,
    /// A brew finished with something brewed (PMMP `PotionFinishBrewingSound`).
    pub(in crate::tick) brewed: bool,
    /// The `ContainerSetData` values that changed.
    pub(in crate::tick) data: Vec<(i32, i32)>,
    /// Whether the stand needs another tick.
    pub(in crate::tick) active: bool,
}

impl Brewing {
    /// PMMP `BrewingStand::readSaveData` (Bedrock `CookTime`, `FuelAmount`, `FuelTotal`).
    pub(in crate::tick) fn read(nbt: &mut Compound) -> Self {
        let short = |nbt: &mut Compound, key: &str| match nbt.remove(key) {
            Some(Value::Short(v)) => u16::try_from(v).unwrap_or(0),
            _ => 0,
        };
        let brew = short(nbt, "CookTime").min(BREW_TICKS);
        let fuel = short(nbt, "FuelAmount");
        let mut fuel_total = short(nbt, "FuelTotal");
        if fuel_total == 0 {
            fuel_total = fuel;
        }
        if fuel == 0 {
            return Self::default();
        }
        Self {
            brew,
            fuel,
            fuel_total,
            recipes: None,
        }
    }

    /// The saved keys, which clients get too.
    pub(in crate::tick) fn write(&self, nbt: &mut Compound) {
        let short = |v: u16| Value::Short(i16::try_from(v).unwrap_or(i16::MAX));
        nbt.insert("CookTime".to_owned(), short(self.brew));
        nbt.insert("FuelTotal".to_owned(), short(self.fuel_total));
        nbt.insert("FuelAmount".to_owned(), short(self.fuel));
    }

    /// The `ContainerSetData` values a viewer gets when it opens the window.
    pub(in crate::tick) fn data(&self) -> [(i32, i32); 3] {
        [
            (DATA_BREW_TIME, i32::from(self.brew)),
            (DATA_FUEL_AMOUNT, i32::from(self.fuel)),
            (DATA_FUEL_TOTAL, i32::from(self.fuel_total)),
        ]
    }

    /// The result of each bottle (PMMP `getBrewableRecipes`), looked up again only when the
    /// ingredient or a bottle changed.
    fn results(&mut self, items: &[Stack]) -> [Option<Stack>; 3] {
        let key: [Stack; 4] = std::array::from_fn(|i| {
            items
                .get(if i == 0 { INGREDIENT } else { i })
                .cloned()
                .unwrap_or_default()
        });
        if let Some((last, results)) = &self.recipes
            && *last == key
        {
            return results.clone();
        }
        let [ingredient, b0, b1, b2] = &key;
        let results = [b0, b1, b2].map(|bottle| {
            if ingredient.is_empty() || bottle.is_empty() {
                None
            } else {
                crate::crafting::brew(bottle, ingredient)
            }
        });
        self.recipes = Some((key, results.clone()));
        results
    }

    /// One tick (PMMP `BrewingStand::onUpdate`).
    pub(in crate::tick) fn step(&mut self, items: &mut [Stack]) -> Step {
        let mut out = Step::default();
        let (prev_brew, prev_fuel, prev_total) = (self.brew, self.fuel, self.fuel_total);
        let results = self.results(items);
        let can_brew = results.iter().any(Option::is_some);
        if self.fuel == 0 && can_brew {
            // PMMP `checkFuel`: only blaze powder.
            let powder = items.get(FUEL).cloned().unwrap_or_default();
            if powder.item().and_then(crate::item::Type::kind)
                == Some(crate::item::Kind::BlazePowder)
            {
                set(items, FUEL, powder.with_count(powder.count() - 1), &mut out);
                self.fuel = FUEL_BREWS;
                self.fuel_total = FUEL_BREWS;
            }
        }
        if self.fuel > 0 {
            if can_brew {
                if self.brew == 0 {
                    self.brew = BREW_TICKS;
                    self.fuel -= 1;
                }
                self.brew -= 1;
                if self.brew == 0 {
                    for (slot, result) in BOTTLES.into_iter().zip(results) {
                        if let Some(result) = result {
                            set(items, slot, result, &mut out);
                            out.brewed = true;
                        }
                    }
                    let ingredient = items.get(INGREDIENT).cloned().unwrap_or_default();
                    set(
                        items,
                        INGREDIENT,
                        ingredient.with_count(ingredient.count().saturating_sub(1)),
                        &mut out,
                    );
                } else {
                    out.active = true;
                }
            } else {
                self.brew = 0;
            }
        } else {
            (self.brew, self.fuel, self.fuel_total) = (0, 0, 0);
        }
        for (key, before, now) in [
            (DATA_BREW_TIME, prev_brew, self.brew),
            (DATA_FUEL_AMOUNT, prev_fuel, self.fuel),
            (DATA_FUEL_TOTAL, prev_total, self.fuel_total),
        ] {
            if before != now {
                out.data.push((key, i32::from(now)));
            }
        }
        // A finished brew changed the slots: the next tick looks again (PMMP schedules an update
        // on every inventory change).
        out.active |= out.slots != 0;
        out
    }
}

/// Sets slot `i`, marking it changed (and kept if only its count changed).
fn set(items: &mut [Stack], i: usize, stack: Stack, out: &mut Step) {
    if let Some(slot) = items.get_mut(i) {
        let bit = 1 << i;
        if out.slots & bit == 0 && slot.can_stack_with(&stack) {
            out.kept |= bit;
        } else {
            out.kept &= !bit;
        }
        *slot = stack;
        out.slots |= bit;
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
    fn a_brew_takes_400_ticks_one_fuel_and_one_ingredient() {
        let mut b = Brewing::default();
        let water = Kind::Potion.default_type();
        let mut items = vec![
            Stack::new(named("minecraft:nether_wart"), 2),
            Stack::new(water, 1),
            Stack::new(water, 1),
            Stack::EMPTY,
            Stack::new(Kind::BlazePowder.default_type(), 1),
        ];
        let first = b.step(&mut items);
        assert!(items[FUEL].is_empty());
        assert_eq!((b.fuel, b.fuel_total, b.brew), (19, 20, 399));
        assert!(first.active);
        for _ in 1..BREW_TICKS {
            b.step(&mut items);
        }
        assert_eq!(items[INGREDIENT].count(), 1);
        let awkward = items[1].clone();
        assert_ne!(awkward, Stack::new(water, 1));
        assert_eq!(items[1], items[2]);
        assert!(items[3].is_empty());
        assert_eq!(b.fuel, 19);
    }

    #[test]
    fn no_brew_without_blaze_powder_or_without_a_recipe() {
        let mut b = Brewing::default();
        let water = Kind::Potion.default_type();
        let mut items = vec![
            Stack::new(Kind::Stick.default_type(), 1),
            Stack::new(water, 1),
            Stack::EMPTY,
            Stack::EMPTY,
            Stack::new(Kind::BlazePowder.default_type(), 1),
        ];
        assert!(!b.step(&mut items).active);
        assert_eq!(items[FUEL].count(), 1, "no recipe, no fuel used");
        items[INGREDIENT] = Stack::new(named("minecraft:nether_wart"), 1);
        items[FUEL] = Stack::new(Kind::Coal.default_type(), 1);
        assert!(!b.step(&mut items).active);
        assert_eq!(b.fuel, 0);
    }
}
