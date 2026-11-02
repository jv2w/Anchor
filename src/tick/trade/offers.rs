//! The offer list of one villager NPC and its two NBT forms (P6).
//!
//! An offer list holds the offers ([`Recipe`], key `Recipes`) in the order the trade screen
//! shows them, the villager's tier and trade xp, and the xp each tier needs
//! (`TierExpRequirements`). Code sets and changes it ([`Offers::set`], [`Offers::add`],
//! [`Offers::remove`], [`Offers::replace`], [`Offers::set_tier`], [`Offers::set_xp`],
//! [`Offers::set_tier_xp`], [`Offers::reset_uses`]); trades change only the counters
//! ([`Offers::apply`]). There is no villager economy: prices are the counts of the payment stacks,
//! and demand and price multipliers are data the client is sent as they are.
//!
//! Both NBT forms come from one writer:
//!
//! - saved ([`Offers::to_nbt`]): the entity's `Offers` compound, `{Recipes, TierExpRequirements,
//!   TradeTier, TradeExperience}` plus every unknown key. The official server keeps the
//!   tier and xp on the entity; here they travel with the offers so the entity module stores one
//!   value.
//! - wire ([`Offers::wire_nbt`]): `UpdateTrade.SerialisedOffers`, the shape `{Recipes,
//!   TierExpRequirements}` (the packet carries the tier), each recipe with its `netId`.
//!
//! A recipe's keys are `buyA`, `buyB` (only with a second payment), `buyCountA`,
//! `buyCountB`, `demand`, `maxUses`, `netId` (wire only), `priceMultiplierA`, `priceMultiplierB`,
//! `rewardExp`, `sell`, `tier`, `traderExp`, `uses`; items are disk item compounds with
//! `WasPickedUp`. Unknown recipe and root keys are kept and written back. A saved recipe Anchor
//! cannot use (unknown item, a value out of range, a wrong tag type) is skipped with a warning,
//! never shown, and written back as it was at the end of `Recipes` (saved data is not
//! destroyed).

use std::fmt;

use tracing::warn;

use crate::custom::{
    MAX_OFFERS, MAX_TRADE_COUNTER, MAX_TRADE_TIER, Registry, TRADE_TIERS, is_price_multiplier,
    is_tier_xp,
};
use crate::item::Stack;
use crate::nbt::{Compound, Encoding, List, Value};
use crate::protocol::Writer;

/// A payment item's `Damage` that takes any damage (the Bedrock any-meta value, P6).
const ANY_DAMAGE: i16 = i16::MAX;
/// A recipe's first payment.
const BUY_A: &str = "buyA";
/// A recipe's second payment, only when it has one.
const BUY_B: &str = "buyB";
/// The first payment's count: the price charged.
const BUY_COUNT_A: &str = "buyCountA";
/// The second payment's count (0 without one).
const BUY_COUNT_B: &str = "buyCountB";
/// An item compound's damage (meta).
const DAMAGE: &str = "Damage";
/// The xp each tier needs, as the official server sends it (P6).
const DEFAULT_TIER_XP: [u32; TRADE_TIERS] = [0, 10, 70, 150, 250];
/// A recipe's demand.
const DEMAND: &str = "demand";
/// The first network ID of an offer; `CraftingData` is empty, so no crafting recipe takes one.
pub(super) const FIRST_NET_ID: u32 = 1;
/// The most bytes a villager's saved offers take ([`Offers::to_nbt`]), and four times that of a
/// reader's budget to read back: half of one NBT root ([`crate::nbt::MAX_ROOT_BYTES`]), which
/// the villager's entity and its `UpdateTrade` must each fit, though [`MAX_OFFERS`] offers of
/// three stacks could take 36 MiB (`item::MAX_NBT_BYTES` each).
const MAX_SAVED_BYTES: usize = crate::nbt::MAX_ROOT_BYTES / 2;
/// A recipe's maximum uses.
const MAX_USES: &str = "maxUses";
/// A recipe's network ID (wire form only).
const NET_ID: &str = "netId";
/// A recipe's first price multiplier.
const PRICE_MULTIPLIER_A: &str = "priceMultiplierA";
/// A recipe's second price multiplier.
const PRICE_MULTIPLIER_B: &str = "priceMultiplierB";
/// Every key a recipe compound reads; the others are kept as unknown.
const RECIPE_KEYS: [&str; 14] = [
    BUY_A,
    BUY_B,
    BUY_COUNT_A,
    BUY_COUNT_B,
    DEMAND,
    MAX_USES,
    NET_ID,
    PRICE_MULTIPLIER_A,
    PRICE_MULTIPLIER_B,
    REWARD_EXP,
    SELL,
    TIER,
    TRADER_EXP,
    USES,
];
/// The offers (both forms).
const RECIPES: &str = "Recipes";
/// Whether a recipe's trades drop orbs (a byte flag).
const REWARD_EXP: &str = "rewardExp";
/// Every key the root compound reads; the others are kept as unknown.
const ROOT_KEYS: [&str; 4] = [RECIPES, TIER_XP, TRADE_TIER, TRADE_XP];
/// What a recipe sells.
const SELL: &str = "sell";
/// The tier that unlocks a recipe.
const TIER: &str = "tier";
/// The xp each tier needs (both forms).
const TIER_XP: &str = "TierExpRequirements";
/// The villager's tier (saved form only; `UpdateTrade` carries it).
const TRADE_TIER: &str = "TradeTier";
/// The villager's trade xp (saved form only).
const TRADE_XP: &str = "TradeExperience";
/// The villager xp per trade of a recipe.
const TRADER_EXP: &str = "traderExp";
/// A recipe's uses so far.
const USES: &str = "uses";

/// The offer list of one villager NPC, kept in its entity.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::tick) struct Offers {
    /// The offers, at most [`MAX_OFFERS`], each passing [`Recipe::check`].
    recipes: Vec<Recipe>,
    /// The villager's tier (0 to [`MAX_TRADE_TIER`]): offers of higher tiers are locked.
    tier: u8,
    /// The villager's trade xp, at most [`MAX_TRADE_COUNTER`].
    xp: u32,
    /// The xp each tier needs, never going down.
    tier_xp: [u32; TRADE_TIERS],
    /// Unknown root keys, written back as they came.
    unknown: Compound,
    /// Saved recipes Anchor cannot use, written back after the others.
    skipped: Vec<Compound>,
    /// The network ID of the first offer while the list is shown to players: allocated when a
    /// player first opens the list, kept while it does not change (reopening shows the same
    /// IDs, as the official server does), dropped by every change of the list so a stale
    /// screen's IDs fail. Not saved.
    first_net_id: Option<u32>,
}

impl Offers {
    /// Reads the offers of a saved entity (its `Offers` compound). Recipes Anchor cannot use are
    /// skipped with a warning and kept to be written back; a bad tier, xp or tier
    /// requirement list falls back to its default with a warning. Custom items are read
    /// through `custom`, the server's custom content.
    pub(in crate::tick) fn from_nbt(saved: &Compound, custom: &Registry) -> Self {
        let mut offers = Self {
            unknown: unknown_keys(saved, &ROOT_KEYS),
            ..Self::default()
        };
        match saved.get(RECIPES) {
            Some(Value::List(List::Compound(list))) => {
                for c in list {
                    match Recipe::from_nbt(c, custom) {
                        Ok(r) if offers.recipes.len() < MAX_OFFERS => offers.recipes.push(r),
                        Ok(_) => {
                            warn!(max = MAX_OFFERS, "villager offer past the limit skipped");
                            offers.skipped.push(c.clone());
                        }
                        Err(reason) => {
                            warn!(reason, "saved villager offer skipped");
                            offers.skipped.push(c.clone());
                        }
                    }
                }
            }
            Some(Value::List(List::End)) | None => {}
            Some(_) => warn!("saved villager offers are not a compound list; none read"),
        }
        match saved.get(TIER_XP).map(tier_xp) {
            Some(Some(t)) => offers.tier_xp = t,
            Some(None) => warn!("saved villager tier requirements are invalid; defaults used"),
            None => {}
        }
        let tier = saved.get(TRADE_TIER).map(|v| match v {
            Value::Int(t) => u8::try_from(*t).ok().filter(|t| *t <= MAX_TRADE_TIER),
            _ => None,
        });
        match tier {
            Some(Some(t)) => offers.tier = t,
            Some(None) => warn!("saved villager tier is invalid; tier 0 used"),
            None => {}
        }
        let xp = saved.get(TRADE_XP).map(|v| match v {
            Value::Int(x) => u32::try_from(*x).ok(),
            _ => None,
        });
        match xp {
            Some(Some(x)) => offers.xp = x,
            Some(None) => warn!("saved villager xp is invalid; 0 used"),
            None => {}
        }
        offers
    }

    /// The offers, in screen order.
    pub(in crate::tick) fn recipes(&self) -> &[Recipe] {
        &self.recipes
    }

    /// Whether the list has no offer.
    pub(in crate::tick) fn is_empty(&self) -> bool {
        self.recipes.is_empty()
    }

    /// The villager's tier (0 to [`MAX_TRADE_TIER`]).
    pub(in crate::tick) fn tier(&self) -> u8 {
        self.tier
    }

    /// The villager's trade xp.
    pub(in crate::tick) fn xp(&self) -> u32 {
        self.xp
    }

    /// The xp each tier needs.
    pub(in crate::tick) fn tier_xp(&self) -> [u32; TRADE_TIERS] {
        self.tier_xp
    }

    /// The index of the offer the client knows as `net_id`, if the list still has that ID.
    pub(in crate::tick) fn index_of(&self, net_id: u32) -> Option<usize> {
        let i = usize::try_from(net_id.checked_sub(self.first_net_id?)?).ok()?;
        (i < self.recipes.len()).then_some(i)
    }

    /// The offer at `index` if it can be traded now: its tier is unlocked and it has uses left.
    pub(in crate::tick) fn tradable(&self, index: usize) -> Option<&Recipe> {
        self.recipes
            .get(index)
            .filter(|r| r.tier <= self.tier && r.uses_left() > 0)
    }

    /// Replaces the whole list. Every offer must pass [`Recipe::check`] and the saved list fit
    /// [`MAX_SAVED_BYTES`]; nothing changes otherwise. The saved offers Anchor cannot use stay
    /// after the new list and count towards that size.
    pub(in crate::tick) fn set(&mut self, recipes: Vec<Recipe>) -> Result<()> {
        if recipes.len() > MAX_OFFERS {
            return Err(Error::Full);
        }
        for recipe in &recipes {
            recipe.check()?;
        }
        let old = std::mem::replace(&mut self.recipes, recipes);
        self.kept_if_saved(|r| *r = old)
    }

    /// Adds an offer at the end.
    pub(in crate::tick) fn add(&mut self, recipe: Recipe) -> Result<()> {
        recipe.check()?;
        if self.recipes.len() >= MAX_OFFERS {
            return Err(Error::Full);
        }
        self.recipes.push(recipe);
        self.kept_if_saved(|r| {
            r.pop();
        })
    }

    /// Removes and returns the offer at `index`.
    pub(in crate::tick) fn remove(&mut self, index: usize) -> Result<Recipe> {
        if index >= self.recipes.len() {
            return Err(Error::NoOffer);
        }
        self.first_net_id = None;
        Ok(self.recipes.remove(index))
    }

    /// Replaces the offer at `index`.
    pub(in crate::tick) fn replace(&mut self, index: usize, recipe: Recipe) -> Result<()> {
        recipe.check()?;
        let slot = self.recipes.get_mut(index).ok_or(Error::NoOffer)?;
        let old = std::mem::replace(slot, recipe);
        self.kept_if_saved(|r| {
            #[expect(
                clippy::expect_used,
                reason = "invariant: the offer at `index` was found just before, and the list keeps its length"
            )]
            let slot = r.get_mut(index).expect("the replaced offer is in the list");
            *slot = old;
        })
    }

    /// Sets the villager's tier; its xp moves to at least the tier's need and below the
    /// next higher need, so only xp a trade gives raises the tier again (a later tier
    /// that needs no more than this one is reached by the next trade). Offers keep their network
    /// IDs.
    pub(in crate::tick) fn set_tier(&mut self, tier: u8) -> Result<()> {
        let need = *self.tier_xp.get(usize::from(tier)).ok_or(Error::Tier)?;
        let higher = self
            .tier_xp
            .iter()
            .skip(usize::from(tier) + 1)
            .find(|&&next| next > need);
        self.tier = tier;
        self.xp = match higher {
            Some(&next) => self.xp.clamp(need, next - 1),
            None => self.xp.max(need),
        };
        Ok(())
    }

    /// Sets the villager's trade xp (at most [`MAX_TRADE_COUNTER`]) and raises its tier
    /// to the highest one the xp reaches; a lower xp keeps the tier.
    pub(in crate::tick) fn set_xp(&mut self, xp: u32) -> Result<()> {
        if xp > MAX_TRADE_COUNTER {
            return Err(Error::Xp);
        }
        self.xp = xp;
        self.raise_tier();
        Ok(())
    }

    /// Sets the xp each tier needs ([`is_tier_xp`]) and raises the tier to the
    /// highest one the xp reaches; higher needs keep the tier.
    pub(in crate::tick) fn set_tier_xp(&mut self, tiers: [u32; TRADE_TIERS]) -> Result<()> {
        if !is_tier_xp(tiers) {
            return Err(Error::TierXp);
        }
        self.tier_xp = tiers;
        self.raise_tier();
        Ok(())
    }

    /// Sets every offer's uses back to 0 (a restock). Offers keep their network IDs.
    pub(in crate::tick) fn reset_uses(&mut self) {
        for r in &mut self.recipes {
            r.uses = 0;
        }
    }

    /// Gives the offers network IDs from `next` (the server's counter, which moves past them)
    /// unless they have them. IDs stay at most [`MAX_TRADE_COUNTER`]; past it the counter starts
    /// over at [`FIRST_NET_ID`].
    pub(in crate::tick) fn allocate_net_ids(&mut self, next: &mut u32) {
        if self.first_net_id.is_some() {
            return;
        }
        #[expect(
            clippy::expect_used,
            reason = "invariant: the list holds at most MAX_OFFERS (128) offers"
        )]
        let len = u32::try_from(self.recipes.len()).expect("the offers count fits a u32");
        let start = (*next).max(FIRST_NET_ID);
        let start = match start.checked_add(len) {
            Some(end) if end <= MAX_TRADE_COUNTER => start,
            _ => FIRST_NET_ID,
        };
        *next = start.saturating_add(len);
        self.first_net_id = Some(start);
    }

    /// Counts `times` trades of the offer at `index`: its uses, the villager's xp and
    /// tier (raised to the highest tier the xp reaches, several at once if enough). At
    /// most the uses left are counted. `None` if there is no such offer.
    pub(in crate::tick) fn apply(&mut self, index: usize, times: u32) -> Option<Applied> {
        let recipe = self.recipes.get_mut(index)?;
        let times = times.min(recipe.uses_left());
        recipe.uses = recipe.uses.saturating_add(times);
        let gained = recipe.trader_exp.saturating_mul(times);
        let reward_exp = recipe.reward_exp;
        self.xp = self.xp.saturating_add(gained).min(MAX_TRADE_COUNTER);
        self.raise_tier();
        Some(Applied { times, reward_exp })
    }

    /// The offers to save with the entity.
    pub(in crate::tick) fn to_nbt(&self) -> Compound {
        let mut c = self.nbt(Form::Saved);
        c.insert(TRADE_TIER.to_owned(), Value::Int(i32::from(self.tier)));
        c.insert(TRADE_XP.to_owned(), int(self.xp));
        c
    }

    /// The offers as `UpdateTrade` sends them: every recipe with its network ID. `None` until
    /// network IDs are allocated ([`Self::allocate_net_ids`]).
    pub(in crate::tick) fn wire_nbt(&self) -> Option<Compound> {
        self.first_net_id?;
        Some(self.nbt(Form::Wire))
    }

    /// The common part of both forms: the wire form has network IDs and no skipped recipes.
    fn nbt(&self, form: Form) -> Compound {
        let mut c = self.unknown.clone();
        let skipped = match form {
            Form::Saved => self.skipped.as_slice(),
            Form::Wire => &[],
        };
        let mut recipes: Vec<Compound> = Vec::with_capacity(self.recipes.len() + skipped.len());
        for (r, i) in self.recipes.iter().zip(0_u32..) {
            recipes.push(r.to_nbt(match form {
                Form::Saved => None,
                Form::Wire => self.net_id(i),
            }));
        }
        recipes.extend(skipped.iter().cloned());
        c.insert(RECIPES.to_owned(), Value::List(List::Compound(recipes)));
        let tiers = (0..TRADE_TIERS)
            .zip(self.tier_xp)
            .map(|(i, x)| Compound::from([(i.to_string(), int(x))]))
            .collect();
        c.insert(TIER_XP.to_owned(), Value::List(List::Compound(tiers)));
        c
    }

    /// Keeps a change of the offers if their saved form still fits [`MAX_SAVED_BYTES`] (and
    /// reads back within four times that); otherwise `undo` puts the old offers back.
    fn kept_if_saved(&mut self, undo: impl FnOnce(&mut Vec<Recipe>)) -> Result<()> {
        let mut w = Writer::new(Vec::new(), 0);
        let fits = w
            .nbt_within(&self.to_nbt(), Encoding::LittleEndian, MAX_SAVED_BYTES)
            .is_ok_and(|c| c.alloc <= 4 * MAX_SAVED_BYTES);
        if !fits {
            undo(&mut self.recipes);
            return Err(Error::TooLarge);
        }
        self.first_net_id = None;
        Ok(())
    }

    /// Raises the tier to the highest one the xp reaches; xp never lowers it.
    fn raise_tier(&mut self) {
        while self.tier < MAX_TRADE_TIER
            && self
                .tier_xp
                .get(usize::from(self.tier) + 1)
                .is_some_and(|&need| self.xp >= need)
        {
            self.tier += 1;
        }
    }

    /// The network ID of the offer at `index`, while the list has IDs.
    fn net_id(&self, index: u32) -> Option<u32> {
        self.first_net_id?.checked_add(index)
    }

    #[cfg(test)]
    pub(in crate::tick) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            recipes,
            tier,
            xp,
            tier_xp,
            unknown,
            skipped,
            first_net_id,
        } = self;
        h.usize(recipes.len());
        for r in recipes {
            r.hash_into(h);
        }
        h.u64(u64::from(*tier));
        h.u64(u64::from(*xp));
        for x in tier_xp {
            h.u64(u64::from(*x));
        }
        crate::tick::extra::hash_nbt(h, unknown);
        h.usize(skipped.len());
        for c in skipped {
            crate::tick::extra::hash_nbt(h, c);
        }
        h.u64(first_net_id.map_or(0, |id| u64::from(id) + 1));
    }
}

impl Default for Offers {
    fn default() -> Self {
        Self {
            recipes: Vec::new(),
            tier: 0,
            xp: 0,
            tier_xp: DEFAULT_TIER_XP,
            unknown: Compound::new(),
            skipped: Vec::new(),
            first_net_id: None,
        }
    }
}

crate::tick::checkpoint::fields!(Offers {
    recipes,
    tier,
    xp,
    tier_xp,
    unknown,
    skipped,
    first_net_id,
});

/// One offer as the villager keeps it.
///
/// The fields are open for building one; an offer list takes it only through
/// [`Recipe::check`], so a stored offer is always valid.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::tick) struct Recipe {
    /// The first payment; its count is the price (sent as `buyCountA`).
    pub(in crate::tick) buy_a: Stack,
    /// The second payment, or empty.
    pub(in crate::tick) buy_b: Stack,
    /// How the first payment's damage matches.
    pub(in crate::tick) damage_a: Damage,
    /// How the second payment's damage matches.
    pub(in crate::tick) damage_b: Damage,
    /// What one trade gives.
    pub(in crate::tick) sell: Stack,
    /// How many trades the offer allows until its uses are reset.
    pub(in crate::tick) max_uses: u32,
    /// Trades made since the last reset.
    pub(in crate::tick) uses: u32,
    /// The villager tier that unlocks it (0 to [`MAX_TRADE_TIER`]).
    pub(in crate::tick) tier: u8,
    /// Xp the villager gets per trade.
    pub(in crate::tick) trader_exp: u32,
    /// Whether each trade drops xp orbs for the player.
    pub(in crate::tick) reward_exp: bool,
    /// Sent as `priceMultiplierA`; Anchor's price does not depend on it.
    pub(in crate::tick) price_multiplier_a: f32,
    /// Sent as `priceMultiplierB`.
    pub(in crate::tick) price_multiplier_b: f32,
    /// Sent as `demand`; Anchor's price does not depend on it.
    pub(in crate::tick) demand: i32,
    /// Keys Anchor does not read, written back as they came.
    pub(in crate::tick) unknown: Compound,
}

impl Recipe {
    /// Builds an offer of `sell` for `buy_a` (and `buy_b`, or empty) at most `max_uses` times,
    /// with the defaults of [`Offer::new`](crate::custom::Offer::new) and exact payment damage.
    #[cfg(test)]
    pub(in crate::tick) fn new(buy_a: Stack, buy_b: Stack, sell: Stack, max_uses: u32) -> Self {
        let crate::custom::Offer {
            buy_a,
            buy_b: _,
            any_damage: _,
            sell,
            max_uses,
            uses,
            tier,
            trader_exp,
            reward_exp,
            price_multipliers: (price_multiplier_a, price_multiplier_b),
            demand,
        } = crate::custom::Offer::new(buy_a, sell, max_uses);
        Self {
            buy_a,
            buy_b,
            damage_a: Damage::Exact,
            damage_b: Damage::Exact,
            sell,
            max_uses,
            uses,
            tier,
            trader_exp,
            reward_exp,
            price_multiplier_a,
            price_multiplier_b,
            demand,
            unknown: Compound::new(),
        }
    }

    /// Reads the offer a recipe compound holds, or why it cannot be used. The price is
    /// `buyCountA` (the actual count) when present, otherwise the item's `Count`. Custom items
    /// are read through `custom`.
    fn from_nbt(c: &Compound, custom: &Registry) -> Result<Self, &'static str> {
        let (buy_a, damage_a) =
            payment(c, BUY_A, BUY_COUNT_A, custom)?.ok_or("no first payment")?;
        let (buy_b, damage_b) = payment(c, BUY_B, BUY_COUNT_B, custom)?.unwrap_or_default();
        let sell = item(c, SELL, custom)?.ok_or("nothing sold")?;
        let max_uses = counter(c, MAX_USES)?.ok_or("no maximum uses")?;
        let recipe = Self {
            buy_a,
            buy_b,
            damage_a,
            damage_b,
            sell,
            max_uses,
            uses: counter(c, USES)?.unwrap_or(0),
            tier: counter(c, TIER)?
                .map_or(Ok(0), u8::try_from)
                .map_err(|_| "tier out of range")?,
            trader_exp: counter(c, TRADER_EXP)?.unwrap_or(0),
            reward_exp: match c.get(REWARD_EXP) {
                Some(Value::Byte(1)) | None => true,
                Some(Value::Byte(0)) => false,
                Some(_) => return Err("rewardExp is not a byte flag"),
            },
            price_multiplier_a: float(c, PRICE_MULTIPLIER_A)?,
            price_multiplier_b: float(c, PRICE_MULTIPLIER_B)?,
            demand: match c.get(DEMAND) {
                Some(Value::Int(d)) => *d,
                Some(_) => return Err("demand is not an int"),
                None => 0,
            },
            unknown: unknown_keys(c, &RECIPE_KEYS),
        };
        recipe.check().map_err(Error::reason)?;
        Ok(recipe)
    }

    /// Whether an offer list can hold this offer.
    pub(in crate::tick) fn check(&self) -> Result<()> {
        if self.buy_a.is_empty() {
            return Err(Error::NoPayment);
        }
        if self.sell.is_empty() {
            return Err(Error::NoSale);
        }
        if self.max_uses > MAX_TRADE_COUNTER
            || self.uses > self.max_uses
            || self.trader_exp > MAX_TRADE_COUNTER
        {
            return Err(Error::Uses);
        }
        if self.tier > MAX_TRADE_TIER {
            return Err(Error::Tier);
        }
        if !is_price_multiplier(self.price_multiplier_a)
            || !is_price_multiplier(self.price_multiplier_b)
        {
            return Err(Error::Multiplier);
        }
        Ok(())
    }

    /// Trades the offer allows before its uses run out.
    pub(in crate::tick) fn uses_left(&self) -> u32 {
        self.max_uses.saturating_sub(self.uses)
    }

    /// The recipe compound, with `net_id` on the wire.
    fn to_nbt(&self, net_id: Option<u32>) -> Compound {
        let Self {
            buy_a,
            buy_b,
            damage_a,
            damage_b,
            sell,
            max_uses,
            uses,
            tier,
            trader_exp,
            reward_exp,
            price_multiplier_a,
            price_multiplier_b,
            demand,
            unknown,
        } = self;
        let mut c = unknown.clone();
        let mut put = |key: &str, value: Value| {
            c.insert(key.to_owned(), value);
        };
        put(BUY_A, item_nbt(buy_a, *damage_a));
        if !buy_b.is_empty() {
            put(BUY_B, item_nbt(buy_b, *damage_b));
        }
        put(BUY_COUNT_A, Value::Int(i32::from(buy_a.count())));
        put(BUY_COUNT_B, Value::Int(i32::from(buy_b.count())));
        put(DEMAND, Value::Int(*demand));
        put(MAX_USES, int(*max_uses));
        if let Some(id) = net_id {
            put(NET_ID, int(id));
        }
        put(PRICE_MULTIPLIER_A, Value::Float(*price_multiplier_a));
        put(PRICE_MULTIPLIER_B, Value::Float(*price_multiplier_b));
        put(REWARD_EXP, Value::Byte(u8::from(*reward_exp)));
        put(SELL, item_nbt(sell, Damage::Exact));
        put(TIER, Value::Int(i32::from(*tier)));
        put(TRADER_EXP, int(*trader_exp));
        put(USES, int(*uses));
        c
    }

    #[cfg(test)]
    fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            buy_a,
            buy_b,
            damage_a,
            damage_b,
            sell,
            max_uses,
            uses,
            tier,
            trader_exp,
            reward_exp,
            price_multiplier_a,
            price_multiplier_b,
            demand,
            unknown,
        } = self;
        for stack in [buy_a, buy_b, sell] {
            crate::tick::inventory::hash_stack(h, stack);
        }
        for d in [damage_a, damage_b] {
            h.u64(u64::from(*d == Damage::Any));
        }
        for v in [*max_uses, *uses, u32::from(*tier), *trader_exp] {
            h.u64(u64::from(v));
        }
        h.u64(u64::from(*reward_exp));
        h.u64(u64::from(price_multiplier_a.to_bits()));
        h.u64(u64::from(price_multiplier_b.to_bits()));
        h.u64(u64::from(demand.cast_unsigned()));
        crate::tick::extra::hash_nbt(h, unknown);
    }
}

crate::tick::checkpoint::fields!(Recipe {
    buy_a,
    buy_b,
    damage_a,
    damage_b,
    sell,
    max_uses,
    uses,
    tier,
    trader_exp,
    reward_exp,
    price_multiplier_a,
    price_multiplier_b,
    demand,
    unknown,
});

/// How a payment item matches what the player gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::tick) enum Damage {
    /// The offer item's damage (meta) exactly.
    #[default]
    Exact,
    /// Any damage: the official server's offers carry `Damage` 32767 on their payments (P6),
    /// written back and sent as it came.
    Any,
}

crate::tick::checkpoint::variants!(Damage {
    0 => Exact,
    1 => Any,
});

/// What trades did to an offer list ([`Offers::apply`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tick) struct Applied {
    /// Trades counted.
    pub(in crate::tick) times: u32,
    /// Whether the offer drops xp orbs per trade.
    pub(in crate::tick) reward_exp: bool,
}

/// The NBT form an offer list is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    /// With the entity ([`Offers::to_nbt`]).
    Saved,
    /// In `UpdateTrade` ([`Offers::wire_nbt`]).
    Wire,
}

/// Why an offer or an offer list change is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tick) enum Error {
    /// The first payment is empty.
    NoPayment,
    /// The offer sells nothing.
    NoSale,
    /// Uses above the maximum, or a counter above `TAG_Int`.
    Uses,
    /// A tier above [`MAX_TRADE_TIER`].
    Tier,
    /// A price multiplier that is NaN or infinite.
    Multiplier,
    /// Tier xp requirements that go down or exceed `TAG_Int`.
    TierXp,
    /// Trade xp above `TAG_Int`.
    Xp,
    /// The list would hold more than [`MAX_OFFERS`].
    Full,
    /// The saved offers would take more than [`MAX_SAVED_BYTES`].
    TooLarge,
    /// No offer at that index.
    NoOffer,
    /// The entity is not a villager NPC.
    NotVillager,
}

impl Error {
    /// The reason for the refusal, as the public error, the skipped saved offer's warning and the
    /// display give it.
    pub(in crate::tick) fn reason(self) -> &'static str {
        match self {
            Self::NoPayment => "the offer has no payment",
            Self::NoSale => "the offer sells nothing",
            Self::Uses => "uses above the maximum or a counter above i32::MAX",
            Self::Tier => "a tier above 4",
            Self::Multiplier => "a price multiplier that is not finite",
            Self::TierXp => "tier xp that goes down or is above i32::MAX",
            Self::Xp => "trade xp above i32::MAX",
            Self::Full => "more than 128 offers",
            Self::TooLarge => "offers whose saved form is over 2 MiB",
            Self::NoOffer => "no offer at that index",
            Self::NotVillager => "not a villager NPC",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl std::error::Error for Error {}

/// A result with the module's [`enum@Error`].
pub(in crate::tick) type Result<T, E = Error> = std::result::Result<T, E>;

/// Converts a counter into a `TAG_Int`.
fn int(v: u32) -> Value {
    #[expect(
        clippy::expect_used,
        reason = "invariant: counters and network IDs are kept at most MAX_TRADE_COUNTER (i32::MAX)"
    )]
    let v = i32::try_from(v).expect("a counter fits a TAG_Int");
    Value::Int(v)
}

/// Writes a stack as a trade item compound: the disk item compound with `WasPickedUp`, and
/// `Damage` 32767 for a payment of any damage.
fn item_nbt(stack: &Stack, damage: Damage) -> Value {
    let mut c = stack.to_sent_nbt();
    if damage == Damage::Any {
        c.insert(DAMAGE.to_owned(), Value::Short(ANY_DAMAGE));
    }
    Value::Compound(c)
}

/// Reads a payment item with its price and damage rule: the count under `count_key` if present
/// (1 to the stack size), otherwise the item's own count.
fn payment(
    c: &Compound,
    key: &str,
    count_key: &str,
    custom: &Registry,
) -> Result<Option<(Stack, Damage)>, &'static str> {
    // `Damage` 32767 is any damage; the item is the one of damage 0.
    let (stack, damage) = match c.get(key) {
        Some(Value::Compound(i)) if i.get(DAMAGE) == Some(&Value::Short(ANY_DAMAGE)) => {
            let mut i = i.clone();
            i.insert(DAMAGE.to_owned(), Value::Short(0));
            let stack = Stack::from_nbt_with(&i, custom).ok_or("unknown item")?;
            ((!stack.is_empty()).then_some(stack), Damage::Any)
        }
        _ => (item(c, key, custom)?, Damage::Exact),
    };
    let Some(stack) = stack else {
        return Ok(None);
    };
    let Some(count) = counter(c, count_key)? else {
        return Ok(Some((stack, damage)));
    };
    match u8::try_from(count) {
        Ok(n) if n >= 1 && n <= stack.max_count() => Ok(Some((stack.with_count(n), damage))),
        _ => Err("price out of range"),
    }
}

/// Reads the item under `key`: `None` if absent or empty air.
fn item(c: &Compound, key: &str, custom: &Registry) -> Result<Option<Stack>, &'static str> {
    match c.get(key) {
        Some(Value::Compound(item)) => {
            let stack = Stack::from_nbt_with(item, custom).ok_or("unknown item")?;
            Ok((!stack.is_empty()).then_some(stack))
        }
        Some(_) => Err("item is not a compound"),
        None => Ok(None),
    }
}

/// Reads a non-negative `TAG_Int` under `key`, if present.
fn counter(c: &Compound, key: &str) -> Result<Option<u32>, &'static str> {
    match c.get(key) {
        Some(Value::Int(v)) => u32::try_from(*v).map(Some).map_err(|_| "negative counter"),
        Some(_) => Err("counter is not an int"),
        None => Ok(None),
    }
}

/// Reads a price multiplier under `key` (the default if absent).
fn float(c: &Compound, key: &str) -> Result<f32, &'static str> {
    match c.get(key) {
        Some(Value::Float(v)) => Ok(*v),
        Some(_) => Err("price multiplier is not a float"),
        None => Ok(crate::custom::DEFAULT_PRICE_MULTIPLIER),
    }
}

/// Reads the tier requirements of a `TierExpRequirements` list: exactly one compound per tier,
/// the `i`-th holding only the int key `"i"`, as [`is_tier_xp`] takes them; `None`
/// otherwise.
fn tier_xp(value: &Value) -> Option<[u32; TRADE_TIERS]> {
    let Value::List(List::Compound(list)) = value else {
        return None;
    };
    if list.len() != TRADE_TIERS {
        return None;
    }
    let mut out = [0; TRADE_TIERS];
    for ((i, c), slot) in list.iter().enumerate().zip(&mut out) {
        let (key, value) = c.iter().next().filter(|_| c.len() == 1)?;
        let Value::Int(x) = value else {
            return None;
        };
        if *key != i.to_string() {
            return None;
        }
        *slot = u32::try_from(*x).ok()?;
    }
    is_tier_xp(out).then_some(out)
}

/// Copies the entries of `c` whose keys are not in `known`, to be written back as they came.
fn unknown_keys(c: &Compound, known: &[&str]) -> Compound {
    c.iter()
        .filter(|(key, _)| !known.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
pub(in crate::tick) mod tests {
    use super::*;
    use crate::item::{Extra, Kind};
    use crate::protocol::Reader;

    pub(in crate::tick) fn stack(kind: Kind, count: u8) -> Stack {
        Stack::new(kind.default_type(), count)
    }

    /// Makes `count` items of the block `name`.
    fn block_item(name: &str, count: u8) -> Stack {
        let state = crate::block::State::all()
            .find(|s| s.name() == name)
            .unwrap();
        match state.item() {
            crate::block::ItemForm::Item(t) => Stack::new(t, count),
            other => panic!("{name}: {other:?}"),
        }
    }

    /// Builds offers of the shapes the official server sends: a plain offer, one with a second
    /// payment and a block item, an enchanted sale, a locked tier.
    pub(in crate::tick) fn sample() -> Offers {
        let mut offers = Offers::default();
        let mut wheat = Recipe::new(
            stack(Kind::Wheat, 20),
            Stack::EMPTY,
            stack(Kind::Emerald, 1),
            16,
        );
        wheat.trader_exp = 2;
        offers.add(wheat).unwrap();
        let mut book = Recipe::new(
            stack(Kind::Emerald, 9),
            stack(Kind::Book, 1),
            stack(Kind::EnchantedBook, 1)
                .with_extra(Some(Extra {
                    enchantments: vec![(crate::item::Enchantment::Efficiency, 2)],
                    ..Extra::default()
                }))
                .unwrap(),
            12,
        );
        book.tier = 1;
        book.trader_exp = 10;
        book.price_multiplier_a = 0.2;
        offers.add(book).unwrap();
        let mut glass = Recipe::new(
            stack(Kind::Emerald, 1),
            Stack::EMPTY,
            block_item("minecraft:glass", 4),
            12,
        );
        glass.tier = 2;
        glass.reward_exp = false;
        glass.demand = -3;
        offers.add(glass).unwrap();
        offers
    }

    /// Collects the key and tag type pairs of a compound, nested compounds as `outer.inner`.
    pub(in crate::tick) fn shape(c: &Compound, prefix: &str, out: &mut Vec<String>) {
        for (k, v) in c {
            let ty = match v {
                Value::Byte(_) => "byte",
                Value::Short(_) => "short",
                Value::Int(_) => "int",
                Value::Float(_) => "float",
                Value::String(_) => "string",
                Value::List(_) => "list",
                Value::Compound(inner) => {
                    shape(inner, &format!("{prefix}{k}."), out);
                    "compound"
                }
                _ => "other",
            };
            out.push(format!("{prefix}{k}:{ty}"));
        }
    }

    fn recipes(c: &Compound) -> &Vec<Compound> {
        match c.get(RECIPES) {
            Some(Value::List(List::Compound(l))) => l,
            other => panic!("no recipes: {other:?}"),
        }
    }

    #[test]
    fn the_wire_form_has_the_official_keys_and_types() {
        let mut offers = sample();
        assert_eq!(
            offers.wire_nbt(),
            None,
            "no network IDs before the first opening"
        );
        let mut next = 0;
        offers.allocate_net_ids(&mut next);
        let wire = offers.wire_nbt().unwrap();
        assert_eq!(
            wire.keys().collect::<Vec<_>>(),
            [RECIPES, TIER_XP],
            "the root keys"
        );
        assert_eq!(
            wire.get(TIER_XP),
            Some(&Value::List(List::Compound(
                [0, 10, 70, 150, 250]
                    .iter()
                    .enumerate()
                    .map(|(i, x)| Compound::from([(i.to_string(), Value::Int(*x))]))
                    .collect()
            )))
        );
        let list = recipes(&wire);
        let mut first = Vec::new();
        shape(&list[0], "", &mut first);
        first.sort();
        let expected = [
            "buyA.Count:byte",
            "buyA.Damage:short",
            "buyA.Name:string",
            "buyA.WasPickedUp:byte",
            "buyA:compound",
            "buyCountA:int",
            "buyCountB:int",
            "demand:int",
            "maxUses:int",
            "netId:int",
            "priceMultiplierA:float",
            "priceMultiplierB:float",
            "rewardExp:byte",
            "sell.Count:byte",
            "sell.Damage:short",
            "sell.Name:string",
            "sell.WasPickedUp:byte",
            "sell:compound",
            "tier:int",
            "traderExp:int",
            "uses:int",
        ];
        assert_eq!(first, expected);
        // A second payment adds `buyB`; an enchanted sale its `tag.ench`; a block item `Block`.
        let mut second = Vec::new();
        shape(&list[1], "", &mut second);
        assert!(second.contains(&"buyB:compound".to_owned()));
        assert!(second.contains(&"sell.tag:compound".to_owned()));
        let mut third = Vec::new();
        shape(&list[2], "", &mut third);
        assert!(third.contains(&"sell.Block:compound".to_owned()));
        // Prices are the actual counts; network IDs follow the counter.
        let first = i32::try_from(FIRST_NET_ID).unwrap();
        for (i, r) in list.iter().enumerate() {
            assert_eq!(
                r.get(NET_ID),
                Some(&Value::Int(first + i32::try_from(i).unwrap()))
            );
            let Some(Value::Compound(buy)) = r.get(BUY_A) else {
                panic!()
            };
            assert_eq!(
                buy.get("Count").map(|v| match v {
                    Value::Byte(b) => i32::from(*b),
                    _ => -1,
                }),
                r.get(BUY_COUNT_A).map(|v| match v {
                    Value::Int(n) => *n,
                    _ => -2,
                })
            );
        }
        assert_eq!(next, FIRST_NET_ID + 3);
    }

    pub(in crate::tick) fn network_bytes(c: &Compound) -> Vec<u8> {
        let mut w = Writer::new(Vec::new(), 0);
        w.nbt(c, Encoding::NetworkLittleEndian).unwrap();
        w.into_inner()
    }

    pub(in crate::tick) fn from_network_bytes(bytes: &[u8]) -> Compound {
        Reader::new(bytes, bytes.len() * 64, 0)
            .nbt(Encoding::NetworkLittleEndian)
            .unwrap()
    }

    fn little_endian_bytes(c: &Compound) -> Vec<u8> {
        let mut w = Writer::new(Vec::new(), 0);
        w.nbt(c, Encoding::LittleEndian).unwrap();
        w.into_inner()
    }

    #[test]
    fn both_forms_round_trip_byte_for_byte() {
        let mut offers = sample();
        offers.apply(0, 3).unwrap();
        offers
            .unknown
            .insert("Future".to_owned(), Value::String("kept".to_owned()));
        offers.recipes[1]
            .unknown
            .insert("someNewKey".to_owned(), Value::Int(7));
        let mut next = 0;
        offers.allocate_net_ids(&mut next);

        let wire = network_bytes(&offers.wire_nbt().unwrap());
        let mut back = Offers::from_nbt(
            &from_network_bytes(&wire),
            &crate::custom::Registry::default(),
        );
        assert_eq!(
            back.recipes, offers.recipes,
            "the wire form holds every offer"
        );
        back.allocate_net_ids(&mut 0);
        assert_eq!(network_bytes(&back.wire_nbt().unwrap()), wire);

        let saved = little_endian_bytes(&offers.to_nbt());
        let back = Offers::from_nbt(&offers.to_nbt(), &crate::custom::Registry::default());
        assert_eq!(little_endian_bytes(&back.to_nbt()), saved);
        assert_eq!(back.recipes, offers.recipes);
        assert_eq!((back.tier, back.xp), (offers.tier, offers.xp));
        assert_eq!(back.first_net_id, None, "network IDs are not saved");
    }

    #[test]
    fn counters_survive_saving() {
        let mut offers = sample();
        offers.set_tier_xp([0, 5, 20, 40, 80]).unwrap();
        offers.apply(0, 16).unwrap();
        let back = Offers::from_nbt(&offers.to_nbt(), &crate::custom::Registry::default());
        assert_eq!(back.recipes[0].uses, 16);
        assert_eq!(back.xp, 32);
        assert_eq!(back.tier, 2);
        assert_eq!(back.tier_xp, [0, 5, 20, 40, 80]);
        assert_eq!(
            back,
            Offers {
                first_net_id: None,
                ..offers
            }
        );
    }

    #[test]
    fn payments_of_any_damage_are_kept_and_written_as_they_came() {
        // A farmer offer of the official server (P6): `Damage` 32767 on both payments.
        let mut wheat = sample().recipes[0].to_nbt(None);
        let any = |c: &mut Compound, key: &str| {
            if let Some(Value::Compound(item)) = c.get_mut(key) {
                item.insert(DAMAGE.to_owned(), Value::Short(ANY_DAMAGE));
            }
        };
        any(&mut wheat, BUY_A);
        let mut book = sample().recipes[1].to_nbt(None);
        any(&mut book, BUY_A);
        any(&mut book, BUY_B);
        let saved = Compound::from([(
            RECIPES.to_owned(),
            Value::List(List::Compound(vec![wheat.clone(), book.clone()])),
        )]);
        let offers = Offers::from_nbt(&saved, &crate::custom::Registry::default());
        assert!(offers.skipped.is_empty(), "no offer is skipped");
        let [w, b] = offers.recipes.as_slice() else {
            panic!("{offers:?}");
        };
        assert_eq!((w.damage_a, w.damage_b), (Damage::Any, Damage::Exact));
        assert_eq!((b.damage_a, b.damage_b), (Damage::Any, Damage::Any));
        assert_eq!(w.buy_a, stack(Kind::Wheat, 20));
        assert_eq!(
            recipes(&offers.to_nbt()).as_slice(),
            [wheat, book],
            "written back with Damage 32767"
        );
        let mut shown = offers.clone();
        shown.allocate_net_ids(&mut 0);
        let wire = shown.wire_nbt().unwrap();
        let sent = recipes(&wire)[0].get(BUY_A);
        assert!(
            matches!(sent, Some(Value::Compound(c)) if c.get(DAMAGE) == Some(&Value::Short(ANY_DAMAGE))),
            "sent with Damage 32767: {sent:?}"
        );
    }

    #[test]
    fn unusable_saved_offers_are_skipped_and_written_back() {
        let good = sample().recipes[0].to_nbt(None);
        let mut unknown_item = good.clone();
        unknown_item.insert(
            SELL.to_owned(),
            Value::Compound(Compound::from([
                (
                    "Name".to_owned(),
                    Value::String("minecraft:nothing".to_owned()),
                ),
                ("Count".to_owned(), Value::Byte(1)),
            ])),
        );
        let mut overused = good.clone();
        overused.insert(USES.to_owned(), Value::Int(17));
        let mut wrong_type = good.clone();
        wrong_type.insert(TIER.to_owned(), Value::Byte(1));
        let mut high_tier = good.clone();
        high_tier.insert(TIER.to_owned(), Value::Int(5));
        let mut nan = good.clone();
        nan.insert(PRICE_MULTIPLIER_A.to_owned(), Value::Float(f32::NAN));
        let mut too_dear = good.clone();
        too_dear.insert(BUY_COUNT_A.to_owned(), Value::Int(65));
        let mut no_sale = good.clone();
        no_sale.remove(SELL);
        let bad = [
            unknown_item,
            overused,
            wrong_type,
            high_tier,
            nan,
            too_dear,
            no_sale,
        ];
        let mut list = vec![good.clone()];
        list.extend(bad.iter().cloned());
        let saved = Compound::from([
            (RECIPES.to_owned(), Value::List(List::Compound(list))),
            (TRADE_TIER.to_owned(), Value::Int(9)),
            (TIER_XP.to_owned(), Value::Int(3)),
        ]);
        let offers = Offers::from_nbt(&saved, &crate::custom::Registry::default());
        assert_eq!(offers.recipes.len(), 1);
        assert_eq!(offers.skipped.len(), bad.len());
        assert_eq!(offers.tier, 0, "a bad tier falls back to 0");
        assert_eq!(offers.tier_xp, DEFAULT_TIER_XP);
        let written = offers.to_nbt();
        // Compared as bytes: one of them holds a NaN.
        let list = |l: &[Compound]| {
            little_endian_bytes(&Compound::from([(
                RECIPES.to_owned(),
                Value::List(List::Compound(l.to_vec())),
            )]))
        };
        assert_eq!(
            list(&recipes(&written)[1..]),
            list(&bad),
            "skipped offers are written back unchanged"
        );
        // The wire form never shows them.
        let mut shown = offers.clone();
        shown.allocate_net_ids(&mut 0);
        assert_eq!(recipes(&shown.wire_nbt().unwrap()).len(), 1);
    }

    #[test]
    fn changes_are_checked_and_drop_the_network_ids() {
        let mut offers = sample();
        let mut next = 10;
        offers.allocate_net_ids(&mut next);
        assert_eq!(offers.index_of(12), Some(2));
        offers.allocate_net_ids(&mut next);
        assert_eq!(offers.index_of(12), Some(2), "reopening keeps the IDs");

        let mut bad = Recipe::new(Stack::EMPTY, Stack::EMPTY, stack(Kind::Emerald, 1), 1);
        assert_eq!(offers.add(bad.clone()), Err(Error::NoPayment));
        bad.buy_a = stack(Kind::Wheat, 1);
        bad.sell = Stack::EMPTY;
        assert_eq!(offers.replace(0, bad.clone()), Err(Error::NoSale));
        bad.sell = stack(Kind::Emerald, 1);
        bad.uses = 2;
        assert_eq!(offers.add(bad.clone()), Err(Error::Uses));
        bad.uses = 0;
        bad.tier = 5;
        assert_eq!(offers.add(bad.clone()), Err(Error::Tier));
        bad.tier = 0;
        bad.price_multiplier_b = f32::INFINITY;
        assert_eq!(offers.add(bad.clone()), Err(Error::Multiplier));
        assert_eq!(offers.remove(3), Err(Error::NoOffer));
        assert_eq!(offers.set_tier(5), Err(Error::Tier));
        assert_eq!(offers.set_tier_xp([0, 10, 5, 20, 30]), Err(Error::TierXp));
        assert_eq!(
            offers.index_of(12),
            Some(2),
            "refused changes change nothing"
        );

        offers.reset_uses();
        offers.set_tier(3).unwrap();
        assert_eq!(offers.xp, 150, "the xp meets the set tier");
        assert_eq!(offers.index_of(12), Some(2), "tier and uses keep the IDs");

        offers.remove(0).unwrap();
        assert_eq!(offers.index_of(12), None, "a changed list has no IDs");
        offers.allocate_net_ids(&mut next);
        assert_eq!(offers.index_of(13), Some(0), "new IDs follow the old ones");
        assert_eq!(offers.index_of(12), None);

        let full: Vec<Recipe> = (0..=MAX_OFFERS)
            .map(|_| {
                Recipe::new(
                    stack(Kind::Wheat, 1),
                    Stack::EMPTY,
                    stack(Kind::Emerald, 1),
                    1,
                )
            })
            .collect();
        assert_eq!(offers.set(full.clone()), Err(Error::Full));
        offers.set(full[1..].to_vec()).unwrap();
        assert_eq!(offers.add(full[0].clone()), Err(Error::Full));
    }

    #[test]
    fn network_ids_stay_in_int_range() {
        let mut offers = sample();
        let mut next = MAX_TRADE_COUNTER - 1;
        offers.allocate_net_ids(&mut next);
        assert_eq!(
            offers.first_net_id,
            Some(FIRST_NET_ID),
            "the counter starts over"
        );
        assert_eq!(next, FIRST_NET_ID + 3);
        let mut next = 0;
        offers.first_net_id = None;
        offers.allocate_net_ids(&mut next);
        assert_eq!(
            offers.first_net_id,
            Some(FIRST_NET_ID),
            "the IDs start at the first one"
        );
        assert_eq!(offers.index_of(FIRST_NET_ID - 1), None);
        assert_eq!(offers.index_of(FIRST_NET_ID + 3), None);
        assert_eq!(offers.index_of(u32::MAX), None);
    }

    #[test]
    fn trades_count_uses_and_raise_the_tier() {
        let mut offers = sample();
        assert!(offers.tradable(0).is_some());
        assert!(offers.tradable(1).is_none(), "tier 1 is locked at tier 0");
        let applied = offers.apply(0, 4).unwrap();
        assert_eq!(
            applied,
            Applied {
                times: 4,
                reward_exp: true,
            }
        );
        assert_eq!((offers.xp, offers.tier), (8, 0));
        offers.apply(0, 1).unwrap();
        assert_eq!(offers.tier, 1, "10 xp reaches tier 1");
        assert!(offers.tradable(1).is_some());
        // Uses run out; more than the uses left are never counted.
        let applied = offers.apply(0, 100).unwrap();
        assert_eq!(applied.times, 11);
        assert_eq!(offers.recipes[0].uses, 16);
        assert!(offers.tradable(0).is_none());
        assert_eq!(offers.apply(0, 1).unwrap().times, 0);
        // Enough xp at once passes several tiers, up to 4.
        offers.recipes[1].trader_exp = MAX_TRADE_COUNTER;
        offers.apply(1, 2).unwrap();
        assert_eq!((offers.tier, offers.xp), (4, MAX_TRADE_COUNTER));
        assert_eq!(offers.apply(9, 1), None);
    }

    /// A tier set lower stays through a trade that gives no xp: its xp moves
    /// below the next higher need.
    #[test]
    fn a_lowered_tier_stays_through_the_next_trade() {
        let mut offers = sample();
        offers.recipes[0].trader_exp = 0;
        offers.set_tier(2).unwrap();
        assert_eq!(offers.xp, 70);
        offers.set_tier(0).unwrap();
        assert_eq!(offers.xp, 9, "below tier 1's need");
        offers.apply(0, 1).unwrap();
        assert_eq!((offers.tier, offers.xp), (0, 9));
        offers.set_tier(MAX_TRADE_TIER).unwrap();
        assert_eq!(offers.xp, 250, "the top tier has no upper bound");
        // Tier 2 needs no more than tier 1: a trade reaches it.
        let mut same = sample();
        same.recipes[0].trader_exp = 0;
        same.set_tier_xp([0, 10, 10, 70, 250]).unwrap();
        same.set_xp(80).unwrap();
        same.set_tier(1).unwrap();
        assert_eq!(same.xp, 69, "below the next higher need");
        same.apply(0, 1).unwrap();
        assert_eq!(same.tier, 2);
    }

    /// New tier needs or a new xp raise the tier at once to the highest one the
    /// xp reaches, so its offers unlock without a trade; higher needs or a lower
    /// xp keep the tier.
    #[test]
    fn changed_needs_or_xp_raise_the_tier_at_once() {
        let mut offers = sample();
        offers.set_tier_xp([0; TRADE_TIERS]).unwrap();
        assert_eq!(offers.tier, MAX_TRADE_TIER);
        let mut offers = sample();
        offers.set_xp(70).unwrap();
        assert_eq!(offers.tier, 2);
        assert!(offers.tradable(2).is_some(), "the tier 2 offer unlocks");
        offers.set_xp(0).unwrap();
        offers.set_tier_xp([0, 100, 200, 300, 400]).unwrap();
        assert_eq!(offers.tier, 2, "tiers never go down");
    }

    /// Offers whose saved form would pass 2 MiB are refused and change nothing: [`MAX_OFFERS`]
    /// offers each selling a stack of 60 KB would make the villager too large for one NBT root, so
    /// its chunk could not be committed. What is accepted saves.
    #[test]
    fn offers_too_large_to_save_are_refused_and_change_nothing() {
        let big = stack(Kind::Stick, 1)
            .with_extra(Some(Extra {
                nbt: Compound::from([("pad".to_owned(), Value::ByteArray(vec![3; 60_000]))]),
                ..Extra::default()
            }))
            .unwrap();
        let recipe = Recipe::new(stack(Kind::Emerald, 1), Stack::EMPTY, big, 1);
        let mut offers = sample();
        let before = offers.to_nbt();
        assert_eq!(
            offers.set(vec![recipe.clone(); MAX_OFFERS]),
            Err(Error::TooLarge)
        );
        assert_eq!(offers.to_nbt(), before);
        let mut added = 0;
        while offers.add(recipe.clone()).is_ok() {
            added += 1;
        }
        assert!((1..MAX_OFFERS).contains(&added), "{added}");
        let full = offers.to_nbt();
        assert_eq!(offers.add(recipe.clone()), Err(Error::TooLarge));
        assert_eq!(offers.replace(0, recipe), Err(Error::TooLarge));
        assert_eq!(offers.to_nbt(), full);
        let mut w = Writer::new(Vec::new(), 0);
        assert!(w.nbt(&full, Encoding::LittleEndian).is_ok());
    }

    /// The reasons restate the limits as text: a changed limit fails here until its text follows.
    #[test]
    fn offer_error_reasons_name_the_current_limits() {
        assert!(Error::Full.reason().contains(&format!(" {MAX_OFFERS} ")));
        assert_eq!(MAX_SAVED_BYTES % (1 << 20), 0, "a whole number of MiB");
        assert!(
            Error::TooLarge
                .reason()
                .contains(&format!(" {} MiB", MAX_SAVED_BYTES >> 20))
        );
        assert!(
            Error::Tier
                .reason()
                .ends_with(&format!(" {MAX_TRADE_TIER}"))
        );
    }
}
