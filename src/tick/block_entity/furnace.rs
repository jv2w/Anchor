//! Furnaces, blast furnaces and smokers (PMMP `tile\Furnace::onUpdate`): fuel burns down one tick
//! at a time, the input cooks while there is fuel and room for the result, and the block is lit
//! while fuel burns. Each smelted item adds its experience to the furnace (dragonfly `smelter`:
//! the whole part, and the fraction as a chance); taking from the output drops it as orbs.

use crate::crafting::{self, FurnaceType};
use crate::custom::Registry;
use crate::item::Stack;
use crate::nbt::Compound;
use crate::random::Random;

/// The `ContainerSetData` key of the cook time (PMMP `ContainerSetDataPacket::PROPERTY_FURNACE_*`).
pub(in crate::tick) const DATA_COOK: i32 = 0;
/// The `ContainerSetData` key of the burn time left.
pub(in crate::tick) const DATA_BURN: i32 = 1;
/// The `ContainerSetData` key of the burn time of the fuel burning.
pub(in crate::tick) const DATA_MAX_BURN: i32 = 2;
/// The `ContainerSetData` key of the stored experience.
pub(in crate::tick) const DATA_XP: i32 = 3;
/// The fuel slot.
pub(in crate::tick) const FUEL: usize = 1;
/// The input slot.
pub(in crate::tick) const INPUT: usize = 0;
/// The most experience a furnace stores: what its saved `Short` holds, so the save and the
/// checkpoint read back what the furnace has.
const MAX_XP: u16 = i16::MAX.cast_unsigned();
/// The output slot.
pub(in crate::tick) const OUTPUT: usize = 2;
/// The slots of a furnace.
pub(in crate::tick) const SLOTS: usize = 3;

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
    /// Experience of the smelted items not yet taken (BDS `StoredXPInt`), at most [`MAX_XP`].
    pub(in crate::tick) xp: u16,
    /// The input last looked up and what one smelt of it makes: derived, so neither saved,
    /// hashed nor compared.
    recipe: Option<(Stack, Option<Stack>)>,
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

    /// PMMP `Furnace::readSaveData`: a missing or zero burn time of the fuel is the burn time
    /// left. Unlike PMMP a cook time without fuel stays: the tick that burns the last fuel
    /// mid-smelt leaves one for the next fuel, and a checkpoint reads back as it was.
    pub(in crate::tick) fn read(nbt: &mut Compound, kind: FurnaceType) -> Self {
        let burn = super::take_timer(nbt, "BurnTime");
        let cook = super::take_timer(nbt, "CookTime");
        let mut max_burn = super::take_timer(nbt, "MaxTime");
        let xp = super::take_timer(nbt, "StoredXPInt");
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

    /// Whether `result` goes onto the output slot: it is not empty and stacks onto the output
    /// within the stack size.
    pub(in crate::tick) fn fits(items: &[Stack], result: &Stack) -> bool {
        let product = items.get(OUTPUT).cloned().unwrap_or_default();
        !result.is_empty()
            && (product.is_empty() || product.can_stack_with(result))
            && u16::from(product.count()) + u16::from(result.count())
                <= u16::from(result.max_count())
    }

    /// The input and what one smelt of it makes, if it can smelt now: it has a recipe and the
    /// output has room for the whole result (PMMP `canSmelt`, which counts room for one only: a
    /// custom result of more would burn fuel without ever smelting).
    pub(in crate::tick) fn ready(
        &mut self,
        items: &[Stack],
        custom: &Registry,
    ) -> Option<(Stack, Stack)> {
        let raw = items.get(INPUT).cloned().unwrap_or_default();
        let result = self.result(&raw, custom)?;
        (raw.count() > 0 && Self::fits(items, &result)).then(|| (raw.with_count(1), result))
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

    /// One tick (PMMP `Furnace::onUpdate`) without events: [`begin`](Self::begin), the fuel
    /// that is due burns and is used up, the cooking, the smelt that is due,
    /// [`finish`](Self::finish). `items` are the three slots; `lit` whether the block is lit now.
    /// The tick raises its events between these steps (`tick_furnace`).
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
            self.burn(items, time, &mut t);
            use_fuel(items, &mut t);
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

    /// Starts burning the fuel of [`fuel_due`](Self::fuel_due) for `time` ticks (at least 1):
    /// the block lights and the fuel is the last fuel; [`use_fuel`] uses it up.
    pub(in crate::tick) fn burn(&mut self, items: &[Stack], time: u16, t: &mut Tick) {
        self.burn = time.max(1);
        self.max_burn = self.burn;
        if !t.lit {
            t.out.lit = Some(true);
        }
        let fuel = items.get(FUEL).cloned().unwrap_or_default();
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

    /// Smelts one input into `result` (which [`fits`](Self::fits)) and stores its experience,
    /// drawn from `experience` (`None` with `experience = false`: nothing is stored), up to
    /// [`MAX_XP`].
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
            self.xp = self.xp.saturating_add(earned(result, rng)).min(MAX_XP);
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

    /// The saved keys (PMMP `writeSaveData` and BDS `StoredXPInt`).
    pub(in crate::tick) fn write(&self, nbt: &mut Compound) {
        nbt.insert("BurnTime".to_owned(), super::timer_value(self.burn));
        nbt.insert("CookTime".to_owned(), super::timer_value(self.cook));
        nbt.insert("MaxTime".to_owned(), super::timer_value(self.max_burn));
        nbt.insert("StoredXPInt".to_owned(), super::timer_value(self.xp));
    }

    /// What one smelt of `input` makes by PMMP's and the server's custom recipes (`custom`,
    /// fixed for the server's life), looked up only when the input differs from the last one.
    fn result(&mut self, input: &Stack, custom: &Registry) -> Option<Stack> {
        let one = input.with_count(1);
        if let Some((last, result)) = &self.recipe
            && last.same(&one)
        {
            return result.clone();
        }
        let result = custom
            .furnace_recipe(self.kind, &one)
            .and_then(|i| custom.furnace_at(i))
            .and_then(|r| r.result.to_item());
        self.recipe = Some((one, result.clone()));
        result
    }
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
#[derive(Debug, Clone, PartialEq, Default)]
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

/// A furnace tick in progress.
#[derive(Debug)]
pub(in crate::tick) struct Tick {
    /// The cook, burn and max burn times when it began.
    prev: (u16, u16, u16),
    /// Whether the block was lit when it began.
    lit: bool,
    out: Step,
}

/// Uses up one fuel item of [`Furnace::burn`]; its residue (a lava bucket's bucket) stays.
pub(in crate::tick) fn use_fuel(items: &mut [Stack], t: &mut Tick) {
    let fuel = items.get(FUEL).cloned().unwrap_or_default();
    let residue = match fuel.item().and_then(crate::item::Type::fuel_residue) {
        Some(residue) => Stack::new(residue, 1),
        None => fuel.with_count(fuel.count().saturating_sub(1)),
    };
    set(items, FUEL, residue, &mut t.out);
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

/// Sets slot `i` ([`super::tick_set_slot`]).
fn set(items: &mut [Stack], i: usize, stack: Stack, out: &mut Step) {
    super::tick_set_slot(items, i, stack, &mut out.slots, &mut out.kept);
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

    /// A custom recipe that makes more than one: no fuel burns while the output cannot take the
    /// whole result.
    #[test]
    fn no_fuel_burns_while_the_output_cannot_take_a_whole_result() {
        use crate::custom::{Ingredient, ItemRef, Recipe};
        let mut custom = crate::Server::builder()
            .recipe(Recipe::furnace(
                FurnaceType::Furnace,
                Ingredient::Item(ItemRef::Item(Kind::Stick.default_type())),
                (ItemRef::Item(Kind::Coal.default_type()), 4),
            ))
            .setup
            .custom;
        custom.finish().unwrap();
        let mut f = Furnace::new(FurnaceType::Furnace);
        let mut items = vec![
            stack(Kind::Stick, 1),
            stack(Kind::Coal, 2),
            stack(Kind::Coal, 61),
        ];
        let mut rng = Random::new(1, 0, 1);
        for _ in 0..1600 {
            assert_eq!(f.step(&mut items, false, &mut rng, &custom).lit, None);
        }
        assert_eq!(items[FUEL], stack(Kind::Coal, 2));
        assert_eq!(items[INPUT], stack(Kind::Stick, 1));
        items[OUTPUT] = stack(Kind::Coal, 60);
        f.step(&mut items, false, &mut rng, &custom);
        assert_eq!(items[FUEL], stack(Kind::Coal, 1), "room for 4: it burns");
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
    fn saved_timers_round_trip_also_with_the_fuel_burnt_out() {
        let mut f = Furnace::new(FurnaceType::Smoker);
        (f.burn, f.max_burn, f.cook, f.xp) = (30, 1600, 40, 7);
        let mut nbt = Compound::new();
        f.write(&mut nbt);
        let mut read = nbt.clone();
        assert_eq!(Furnace::read(&mut read, FurnaceType::Smoker), f);
        assert!(read.is_empty());
        // The tick that burns the last fuel mid-smelt leaves the cook time for the next fuel.
        (f.burn, f.max_burn, f.cook) = (0, 300, 40);
        let mut nbt = Compound::new();
        f.write(&mut nbt);
        assert_eq!(Furnace::read(&mut nbt, FurnaceType::Smoker), f);
    }

    /// Stored experience stops at what its saved `Short` holds, so the save reads back the
    /// experience the furnace has.
    #[test]
    fn stored_experience_stops_at_what_a_save_holds() {
        let mut f = Furnace::new(FurnaceType::Furnace);
        f.xp = MAX_XP;
        let mut items = vec![stack(Kind::RawGold, 1), Stack::EMPTY, Stack::EMPTY];
        let mut t = f.begin(false);
        let mut rng = Random::new(1, 0, 1);
        f.smelt(
            &mut items,
            &stack(Kind::GoldIngot, 1),
            Some(&mut rng),
            &mut t,
        );
        assert_eq!(f.xp, MAX_XP);
        let mut nbt = Compound::new();
        f.write(&mut nbt);
        assert_eq!(Furnace::read(&mut nbt, FurnaceType::Furnace), f);
    }
}
