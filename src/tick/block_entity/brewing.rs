//! Brewing stands (PMMP `tile\BrewingStand::onUpdate`): a blaze powder gives 20 brews of fuel; a
//! brew takes 400 ticks and turns every bottle the ingredient has a recipe for, then uses up one
//! ingredient.

use crate::block::BrewingStandSlot;
use crate::custom::Registry;
use crate::item::Stack;
use crate::nbt::{Compound, Value};

/// The ingredient slot.
pub(in crate::tick) const INGREDIENT: usize = 0;
/// The three bottle slots, left to right.
pub(in crate::tick) const BOTTLES: [usize; 3] = [
    BrewingStandSlot::East.slot(),
    BrewingStandSlot::Northwest.slot(),
    BrewingStandSlot::Southwest.slot(),
];
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
    /// keep its stack network ID (BDS, P6: an ingredient shrinking).
    pub(in crate::tick) kept: u8,
    /// A brew finished with something brewed (PMMP `PotionFinishBrewingSound`).
    pub(in crate::tick) brewed: bool,
    /// The `ContainerSetData` values that changed.
    pub(in crate::tick) data: super::DataChanges,
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
    fn results(&mut self, items: &[Stack], custom: &Registry) -> [Option<Stack>; 3] {
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
                custom.brew(bottle, ingredient)
            }
        });
        self.recipes = Some((key, results.clone()));
        results
    }

    /// One tick (PMMP `BrewingStand::onUpdate`) without events: [`begin`](Self::begin), the
    /// fuel that is due, the brewing, every bottle of a finished brew, the ingredient used up,
    /// [`finish`](Self::finish). The tick raises its events between these steps
    /// (`tick_brewing`).
    #[cfg(test)]
    pub(in crate::tick) fn step(&mut self, items: &mut [Stack], custom: &Registry) -> Step {
        let mut t = self.begin();
        if self.fuel_due(items, custom) {
            self.refuel(items, FUEL_BREWS, &mut t);
        }
        if let Some(results) = self.brew_tick(items, custom, &mut t) {
            for (slot, result) in BOTTLES.into_iter().zip(results) {
                if let Some(result) = result {
                    bottle(items, slot, result, &mut t);
                }
            }
            use_ingredient(items, &mut t);
        }
        self.finish(t)
    }

    /// Starts a tick: what [`finish`](Self::finish) compares with.
    pub(in crate::tick) fn begin(&self) -> Tick {
        Tick {
            prev: (self.brew, self.fuel, self.fuel_total),
            out: Step::default(),
        }
    }

    /// The result of each bottle slot for the ingredient now.
    pub(in crate::tick) fn bottle_results(
        &mut self,
        items: &[Stack],
        custom: &Registry,
    ) -> [Option<Stack>; 3] {
        self.results(items, custom)
    }

    /// Whether a blaze powder is to be used now (PMMP `checkFuel`'s condition: no fuel left,
    /// something can brew, the fuel slot holds blaze powder; PMMP raises its event cancelled
    /// for another item, Anchor raises none).
    pub(in crate::tick) fn fuel_due(&mut self, items: &[Stack], custom: &Registry) -> bool {
        let powder = items.get(FUEL).cloned().unwrap_or_default();
        self.fuel == 0
            && self.results(items, custom).iter().any(Option::is_some)
            && powder.item().and_then(crate::item::Type::kind)
                == Some(crate::item::Kind::BlazePowder)
    }

    /// Uses up one blaze powder for `brews` brews of fuel (at least 1).
    pub(in crate::tick) fn refuel(&mut self, items: &mut [Stack], brews: u16, t: &mut Tick) {
        let powder = items.get(FUEL).cloned().unwrap_or_default();
        set(
            items,
            FUEL,
            powder.with_count(powder.count().saturating_sub(1)),
            &mut t.out,
        );
        self.fuel = brews.max(1);
        self.fuel_total = self.fuel;
    }

    /// Brews one tick; the result of each bottle when the brew finishes now.
    pub(in crate::tick) fn brew_tick(
        &mut self,
        items: &[Stack],
        custom: &Registry,
        t: &mut Tick,
    ) -> Option<[Option<Stack>; 3]> {
        let results = self.results(items, custom);
        let can_brew = results.iter().any(Option::is_some);
        if self.fuel > 0 {
            if can_brew {
                if self.brew == 0 {
                    self.brew = BREW_TICKS;
                    self.fuel -= 1;
                }
                self.brew -= 1;
                if self.brew == 0 {
                    return Some(results);
                }
                t.out.active = true;
            } else {
                self.brew = 0;
            }
        } else {
            (self.brew, self.fuel, self.fuel_total) = (0, 0, 0);
        }
        None
    }

    /// Ends a tick: what it did, with the window data that changed.
    pub(in crate::tick) fn finish(&self, t: Tick) -> Step {
        let Tick {
            prev: (prev_brew, prev_fuel, prev_total),
            mut out,
        } = t;
        for (key, before, now) in [
            (DATA_BREW_TIME, prev_brew, self.brew),
            (DATA_FUEL_AMOUNT, prev_fuel, self.fuel),
            (DATA_FUEL_TOTAL, prev_total, self.fuel_total),
        ] {
            if before != now {
                out.data.push(key, i32::from(now));
            }
        }
        // A finished brew changed the slots: the next tick looks again (PMMP schedules an update
        // on every inventory change).
        out.active |= out.slots != 0;
        out
    }
}

/// A brewing tick in progress.
#[derive(Debug)]
pub(in crate::tick) struct Tick {
    /// The brew, fuel and fuel total when it began.
    prev: (u16, u16, u16),
    out: Step,
}

/// Puts the brewed `result` into bottle slot `slot`.
pub(in crate::tick) fn bottle(items: &mut [Stack], slot: usize, result: Stack, t: &mut Tick) {
    set(items, slot, result, &mut t.out);
    t.out.brewed = true;
}

/// Uses up one ingredient at the end of a brew (PMMP: whatever the bottles became).
pub(in crate::tick) fn use_ingredient(items: &mut [Stack], t: &mut Tick) {
    let ingredient = items.get(INGREDIENT).cloned().unwrap_or_default();
    set(
        items,
        INGREDIENT,
        ingredient.with_count(ingredient.count().saturating_sub(1)),
        &mut t.out,
    );
}

/// Sets slot `i` ([`super::tick_set_slot`]).
fn set(items: &mut [Stack], i: usize, stack: Stack, out: &mut Step) {
    super::tick_set_slot(items, i, stack, &mut out.slots, &mut out.kept);
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
        let first = b.step(&mut items, &Registry::default());
        assert!(items[FUEL].is_empty());
        assert_eq!((b.fuel, b.fuel_total, b.brew), (19, 20, 399));
        assert!(first.active);
        for _ in 1..BREW_TICKS {
            b.step(&mut items, &Registry::default());
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
        assert!(!b.step(&mut items, &Registry::default()).active);
        assert_eq!(items[FUEL].count(), 1, "no recipe, no fuel used");
        items[INGREDIENT] = Stack::new(named("minecraft:nether_wart"), 1);
        items[FUEL] = Stack::new(Kind::Coal.default_type(), 1);
        assert!(!b.step(&mut items, &Registry::default()).active);
        assert_eq!(b.fuel, 0);
    }
}
