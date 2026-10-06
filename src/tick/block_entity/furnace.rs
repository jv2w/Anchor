//! Furnaces, blast furnaces and smokers (PMMP `tile\Furnace::onUpdate`): fuel burns down one tick
//! at a time, the input cooks while there is fuel and room for the result, and the block is lit
//! while fuel burns. Each smelted item adds its experience to the furnace (dragonfly `smelter`:
//! the whole part, and the fraction as a chance); taking from the output drops it as orbs.

use crate::crafting::{self, FurnaceType};
use crate::custom::Registry;
use crate::item::Stack;
use crate::nbt::{Compound, Value};
use crate::random::Random;

/// The input slot.
pub(in crate::tick) const INPUT: usize = 0;
/// The fuel slot.
pub(in crate::tick) const FUEL: usize = 1;
/// The output slot.
pub(in crate::tick) const OUTPUT: usize = 2;
/// The slots of a furnace.
pub(in crate::tick) const SLOTS: usize = 3;
/// `ContainerSetData` keys (PMMP `ContainerSetDataPacket::PROPERTY_FURNACE_*`).
pub(in crate::tick) const DATA_COOK: i32 = 0;
pub(in crate::tick) const DATA_BURN: i32 = 1;
pub(in crate::tick) const DATA_MAX_BURN: i32 = 2;
pub(in crate::tick) const DATA_XP: i32 = 3;

/// A furnace-like block entity's timers and stored experience.
#[derive(Debug, Clone)]
pub(in crate::tick) struct Furnace {
    /// Furnace, blast furnace or smoker.
    pub(in crate::tick) kind: FurnaceType,
    /// Ticks of fuel left (PMMP `BurnTime`).
    pub(in crate::tick) burn: u16,
    /// The burn time of the fuel burning now (PMMP `MaxTime`).
    pub(in crate::tick) max_burn: u16,
    /// Ticks the input has cooked (PMMP `CookTime`).
    pub(in crate::tick) cook: u16,
    /// Experience of the smelted items not yet taken (BDS `StoredXPInt`).
    pub(in crate::tick) xp: u16,
    /// The recipe of the input last looked up: derived, so neither saved, hashed nor compared.
    recipe: Option<(Stack, Option<u16>)>,
}

impl PartialEq for Furnace {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            kind,
            burn,
            max_burn,
            cook,
            xp,
            recipe: _,
        } = self;
        (*kind, *burn, *max_burn, *cook, *xp)
            == (other.kind, other.burn, other.max_burn, other.cook, other.xp)
    }
}

/// What one furnace tick did, for the caller to apply outside.
#[derive(Debug, Clone, Default, PartialEq)]
pub(in crate::tick) struct Step {
    /// Bit `i`: slot `i` changed.
    pub(in crate::tick) slots: u8,
    /// Bit `i`: slot `i` changed only its count (the same item before and after), so its viewers
    /// keep its stack network ID (BDS, P6: a furnace's output growing, its input shrinking).
    pub(in crate::tick) kept: u8,
    /// The block must become lit (`Some(true)`) or unlit (`Some(false)`).
    pub(in crate::tick) lit: Option<bool>,
    /// The fuel item that started burning (BDS `LastFuel`).
    pub(in crate::tick) fuel: Option<Stack>,
    /// The `ContainerSetData` values that changed: key and value.
    pub(in crate::tick) data: super::DataChanges,
    /// Whether the furnace needs another tick (PMMP `onUpdate`'s return value).
    pub(in crate::tick) active: bool,
}

impl Furnace {
    /// A new, cold furnace of `kind`.
    pub(in crate::tick) fn new(kind: FurnaceType) -> Self {
        Self {
            kind,
            burn: 0,
            max_burn: 0,
            cook: 0,
            xp: 0,
            recipe: None,
        }
    }

    /// PMMP `Furnace::readSaveData`: no cooking without fuel, the burn time of the fuel at least
    /// what is left.
    pub(in crate::tick) fn read(nbt: &mut Compound, kind: FurnaceType) -> Self {
        let short = |nbt: &mut Compound, key: &str| match nbt.remove(key) {
            Some(Value::Short(v)) => u16::try_from(v).unwrap_or(0),
            _ => 0,
        };
        let burn = short(nbt, "BurnTime");
        let mut cook = short(nbt, "CookTime");
        let mut max_burn = short(nbt, "MaxTime");
        let xp = short(nbt, "StoredXPInt");
        if burn == 0 {
            cook = 0;
        }
        if max_burn == 0 {
            max_burn = burn;
        }
        Self {
            kind,
            burn,
            max_burn,
            cook: cook.min(kind.cook_ticks()),
            xp,
            recipe: None,
        }
    }

    /// The saved keys (PMMP `writeSaveData` and BDS `StoredXPInt`).
    pub(in crate::tick) fn write(&self, nbt: &mut Compound) {
        let short = |v: u16| Value::Short(i16::try_from(v).unwrap_or(i16::MAX));
        nbt.insert("BurnTime".to_owned(), short(self.burn));
        nbt.insert("CookTime".to_owned(), short(self.cook));
        nbt.insert("MaxTime".to_owned(), short(self.max_burn));
        nbt.insert("StoredXPInt".to_owned(), short(self.xp));
    }

    /// The `ContainerSetData` values a viewer gets when it opens the window (BDS sends keys 0
    /// to 3 on open, P6).
    pub(in crate::tick) fn data(&self) -> [(i32, i32); 4] {
        [
            (DATA_COOK, i32::from(self.cook)),
            (DATA_BURN, i32::from(self.burn)),
            (DATA_MAX_BURN, i32::from(self.max_burn)),
            (DATA_XP, i32::from(self.xp)),
        ]
    }

    /// The recipe for `input` among PMMP's and the server's custom ones (`custom`, fixed for
    /// the server's life), looked up only when the input differs from the last one.
    fn recipe(&mut self, input: &Stack, custom: &Registry) -> Option<crafting::Furnace> {
        let one = input.with_count(1);
        let index = match &self.recipe {
            Some((last, index)) if *last == one => *index,
            _ => {
                let index = custom.furnace_recipe(self.kind, &one);
                self.recipe = Some((one, index));
                index
            }
        };
        index.and_then(|i| custom.furnace_at(i))
    }

    /// One tick (PMMP `Furnace::onUpdate`) without events: [`begin`](Self::begin), the fuel
    /// that is due burns, the cooking, the smelt that is due, [`finish`](Self::finish). `items`
    /// are the three slots; `lit` whether the block is lit now. The tick raises its events
    /// between these steps (`tick_furnace`).
    #[cfg(test)]
    pub(in crate::tick) fn step(
        &mut self,
        items: &mut [Stack],
        lit: bool,
        rng: &mut Random,
        custom: &Registry,
    ) -> Step {
        let mut t = self.begin(lit);
        if let Some((_, time)) = self.fuel_due(items, custom) {
            self.burn(items, time, true, &mut t);
        }
        if let Some((_, result)) = self.cook(items, custom, &mut t) {
            self.smelt(items, &result, Some(rng), &mut t);
        }
        self.finish(t)
    }

    /// Starts a tick: what [`finish`](Self::finish) compares with.
    pub(in crate::tick) fn begin(&self, lit: bool) -> Tick {
        Tick {
            prev: (self.cook, self.burn, self.max_burn),
            lit,
            out: Step::default(),
        }
    }

    /// The input and what one smelt of it makes, if it can smelt now: it has a recipe and the
    /// output has room for the result (PMMP `canSmelt`).
    pub(in crate::tick) fn ready(
        &mut self,
        items: &[Stack],
        custom: &Registry,
    ) -> Option<(Stack, Stack)> {
        let raw = items.get(INPUT).cloned().unwrap_or_default();
        let result = self.recipe(&raw, custom).and_then(|r| r.result.to_item())?;
        let product = items.get(OUTPUT).cloned().unwrap_or_default();
        let room = product.is_empty()
            || (result.can_stack_with(&product) && product.count() < product.max_count());
        (raw.count() > 0 && room).then(|| (raw.with_count(1), result))
    }

    /// The fuel that starts burning now (PMMP `checkFuel`'s condition: nothing burns, the input
    /// can smelt, the fuel slot holds fuel): one of it and its burn time.
    pub(in crate::tick) fn fuel_due(
        &mut self,
        items: &[Stack],
        custom: &Registry,
    ) -> Option<(Stack, u16)> {
        let fuel = items.get(FUEL).cloned().unwrap_or_default();
        let time = fuel.item().map_or(0, crate::item::Type::fuel_time_ticks);
        (self.burn == 0 && time > 0 && fuel.count() > 0 && self.ready(items, custom).is_some())
            .then(|| (fuel.with_count(1), time))
    }

    /// Starts burning the fuel of [`fuel_due`](Self::fuel_due) for `time` ticks (at least 1):
    /// the block lights; when `consume`, the fuel item is used up (its residue stays).
    pub(in crate::tick) fn burn(
        &mut self,
        items: &mut [Stack],
        time: u16,
        consume: bool,
        t: &mut Tick,
    ) {
        let fuel = items.get(FUEL).cloned().unwrap_or_default();
        self.burn = time.max(1);
        self.max_burn = self.burn;
        if !t.lit {
            t.out.lit = Some(true);
        }
        if consume {
            let residue = match fuel.item().and_then(crate::item::Type::fuel_residue) {
                Some(residue) => Stack::new(residue, 1),
                None => fuel.with_count(fuel.count().saturating_sub(1)),
            };
            set(items, FUEL, residue, &mut t.out);
        }
        t.out.fuel = Some(fuel.with_count(1));
    }

    /// Burns one tick and cooks the input: the input and result of a smelt that is due now.
    pub(in crate::tick) fn cook(
        &mut self,
        items: &[Stack],
        custom: &Registry,
        t: &mut Tick,
    ) -> Option<(Stack, Stack)> {
        let mut due = None;
        if self.burn > 0 {
            self.burn -= 1;
            if let Some(ready) = self.ready(items, custom) {
                self.cook = self.cook.saturating_add(1);
                let need = self.kind.cook_ticks();
                if self.cook >= need {
                    self.cook -= need;
                    due = Some(ready);
                }
            } else if self.burn == 0 {
                (self.burn, self.cook, self.max_burn) = (0, 0, 0);
            } else {
                self.cook = 0;
            }
            t.out.active = true;
        } else {
            if t.lit {
                t.out.lit = Some(false);
            }
            (self.burn, self.cook, self.max_burn) = (0, 0, 0);
        }
        due
    }

    /// Whether `result` goes onto the output slot: it is not empty and stacks onto the output
    /// within the stack size.
    pub(in crate::tick) fn fits(items: &[Stack], result: &Stack) -> bool {
        let product = items.get(OUTPUT).cloned().unwrap_or_default();
        !result.is_empty()
            && (product.is_empty() || product.can_stack_with(result))
            && u16::from(product.count()) + u16::from(result.count())
                <= u16::from(result.max_count())
    }

    /// Smelts one input into `result` (which [`fits`](Self::fits)) and stores its experience,
    /// drawn from `experience` (`None` with `experience = false`: nothing is stored).
    pub(in crate::tick) fn smelt(
        &mut self,
        items: &mut [Stack],
        result: &Stack,
        experience: Option<&mut Random>,
        t: &mut Tick,
    ) {
        let raw = items.get(INPUT).cloned().unwrap_or_default();
        let product = items.get(OUTPUT).cloned().unwrap_or_default();
        let count = product.count().saturating_add(result.count());
        set(items, OUTPUT, result.with_count(count), &mut t.out);
        set(
            items,
            INPUT,
            raw.with_count(raw.count().saturating_sub(1)),
            &mut t.out,
        );
        if let Some(rng) = experience {
            self.xp = self.xp.saturating_add(earned(result, rng));
        }
    }

    /// Ends a tick: what it did, with the window data that changed.
    pub(in crate::tick) fn finish(&self, t: Tick) -> Step {
        let Tick {
            prev: (prev_cook, prev_burn, prev_max),
            mut out,
            ..
        } = t;
        for (key, before, now) in [
            (DATA_COOK, prev_cook, self.cook),
            (DATA_BURN, prev_burn, self.burn),
            (DATA_MAX_BURN, prev_max, self.max_burn),
        ] {
            if before != now {
                out.data.push(key, i32::from(now));
            }
        }
        out
    }
}

/// A furnace tick in progress.
#[derive(Debug)]
pub(in crate::tick) struct Tick {
    /// The cook, burn and max burn times when it began.
    prev: (u16, u16, u16),
    /// Whether the block was lit when it began.
    lit: bool,
    out: Step,
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

/// The experience one smelted `result` earns: the whole part, plus one with the chance of the
/// fraction (dragonfly `smelter.tickSmelting`).
fn earned(result: &Stack, rng: &mut Random) -> u16 {
    let Some(item) = result.item() else {
        return 0;
    };
    let xp = crafting::smelt_experience(item) * f64::from(result.count());
    let whole = xp.floor();
    let extra = u16::from(xp - whole > 0.0 && rng.unit() < xp - whole);
    // The table's values are at most 2 per item and a result is one stack.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "safety: the experience is finite, positive and at most 2 * 64"
    )]
    let whole = whole as u16;
    whole.saturating_add(extra)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::Kind;

    fn stack(kind: Kind, count: u8) -> Stack {
        Stack::new(kind.default_type(), count)
    }

    /// Runs `ticks` furnace ticks; the block's lit state follows the steps.
    fn run(f: &mut Furnace, items: &mut [Stack], ticks: u32, lit: &mut bool) -> Vec<Step> {
        let mut rng = Random::new(1, 0, 1);
        (0..ticks)
            .map(|_| {
                let step = f.step(items, *lit, &mut rng, &Registry::default());
                if let Some(l) = step.lit {
                    *lit = l;
                }
                step
            })
            .collect()
    }

    #[test]
    fn a_coal_smelts_eight_iron_and_the_furnace_goes_out_after_its_fuel() {
        let mut f = Furnace::new(FurnaceType::Furnace);
        let mut items = vec![stack(Kind::RawIron, 9), stack(Kind::Coal, 1), Stack::EMPTY];
        let mut lit = false;
        let steps = run(&mut f, &mut items, 1600, &mut lit);
        assert_eq!(steps[0].lit, Some(true));
        assert_eq!(steps[0].fuel, Some(stack(Kind::Coal, 1)));
        assert!(items[FUEL].is_empty());
        assert_eq!(items[OUTPUT], stack(Kind::IronIngot, 8));
        assert_eq!(items[INPUT], stack(Kind::RawIron, 1));
        assert_eq!(f.burn, 0);
        // The next tick finds no fuel: the block goes out and the progress resets (PMMP).
        let steps = run(&mut f, &mut items, 1, &mut lit);
        assert_eq!(steps[0].lit, Some(false));
        assert!(!steps[0].active);
        assert_eq!((f.burn, f.cook, f.max_burn), (0, 0, 0));
        // Iron is 0.7 each: 8 smelts store between 0 and 8 experience.
        assert!(f.xp <= 8);
    }

    #[test]
    fn several_fuels_burn_one_after_another_and_cooking_stops_when_the_output_is_full() {
        let mut f = Furnace::new(FurnaceType::BlastFurnace);
        let mut items = vec![
            stack(Kind::RawGold, 20),
            stack(Kind::Coal, 3),
            stack(Kind::GoldIngot, 60),
        ];
        let mut lit = false;
        run(&mut f, &mut items, 4 * 100, &mut lit);
        assert_eq!(items[OUTPUT].count(), 64);
        assert_eq!(items[INPUT].count(), 16);
        // One coal burnt; the blast furnace keeps burning the rest of it with a full output.
        assert_eq!(items[FUEL].count(), 2);
        run(&mut f, &mut items, 1600, &mut lit);
        assert_eq!(
            items[FUEL].count(),
            2,
            "no new fuel without room for the result"
        );
        assert!(!lit);
    }

    #[test]
    fn a_lava_bucket_leaves_a_bucket_and_unknown_input_does_not_burn_fuel() {
        let mut f = Furnace::new(FurnaceType::Furnace);
        let mut items = vec![
            stack(Kind::Stick, 1),
            stack(Kind::LavaBucket, 1),
            Stack::EMPTY,
        ];
        let mut lit = false;
        let steps = run(&mut f, &mut items, 5, &mut lit);
        assert!(steps.iter().all(|s| s.fuel.is_none() && !s.active));
        items[INPUT] = stack(Kind::RawCopper, 1);
        run(&mut f, &mut items, 1, &mut lit);
        assert_eq!(items[FUEL], stack(Kind::Bucket, 1));
        assert_eq!(f.max_burn, 20_000);
    }

    #[test]
    fn saved_timers_round_trip_and_follow_pmmp_on_load() {
        let mut f = Furnace::new(FurnaceType::Smoker);
        (f.burn, f.max_burn, f.cook, f.xp) = (30, 1600, 40, 7);
        let mut nbt = Compound::new();
        f.write(&mut nbt);
        let mut read = nbt.clone();
        assert_eq!(Furnace::read(&mut read, FurnaceType::Smoker), f);
        assert!(read.is_empty());
        nbt.insert("BurnTime".to_owned(), Value::Short(0));
        assert_eq!(Furnace::read(&mut nbt, FurnaceType::Smoker).cook, 0);
    }
}
