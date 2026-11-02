//! The typed trade API: [`Context`] methods that read and change a villager NPC's offers as
//! [`Offer`] values, and the conversion between them and the stored offers.

use super::{Damage, Error, HOOKS, Offers, Recipe, Result, change_offers};
use crate::custom::{self, Offer};
use crate::entity::Id;
use crate::nbt::Compound;
use crate::tick::{Context, State};

/// Converts a refused offer change into the public error.
fn to_public(error: Error) -> custom::Error {
    match error {
        Error::NotVillager => custom::Error::NotVillager,
        _ => custom::Error::Offer {
            reason: error.reason(),
        },
    }
}

/// Converts an offer into its stored form: the second payment takes any damage when the first
/// does, and there are no unknown saved keys.
fn to_recipe(offer: &Offer) -> Recipe {
    let Offer {
        buy_a,
        buy_b,
        any_damage,
        sell,
        max_uses,
        uses,
        tier,
        trader_exp,
        reward_exp,
        price_multipliers: (price_multiplier_a, price_multiplier_b),
        demand,
    } = offer;
    let damage = |any: bool| if any { Damage::Any } else { Damage::Exact };
    Recipe {
        buy_a: buy_a.clone(),
        buy_b: buy_b.clone(),
        damage_a: damage(*any_damage),
        damage_b: damage(*any_damage && !buy_b.is_empty()),
        sell: sell.clone(),
        max_uses: *max_uses,
        uses: *uses,
        tier: *tier,
        trader_exp: *trader_exp,
        reward_exp: *reward_exp,
        price_multiplier_a: *price_multiplier_a,
        price_multiplier_b: *price_multiplier_b,
        demand: *demand,
        unknown: Compound::new(),
    }
}

/// Converts a stored offer into its typed form.
fn to_offer(recipe: &Recipe) -> Offer {
    let Recipe {
        buy_a,
        buy_b,
        damage_a,
        // An `Offer` has one damage rule, the first payment's.
        damage_b: _,
        sell,
        max_uses,
        uses,
        tier,
        trader_exp,
        reward_exp,
        price_multiplier_a,
        price_multiplier_b,
        demand,
        // Unknown saved keys stay with the villager only.
        unknown: _,
    } = recipe;
    Offer {
        buy_a: buy_a.clone(),
        buy_b: buy_b.clone(),
        any_damage: *damage_a == Damage::Any,
        sell: sell.clone(),
        max_uses: *max_uses,
        uses: *uses,
        tier: *tier,
        trader_exp: *trader_exp,
        reward_exp: *reward_exp,
        price_multipliers: (*price_multiplier_a, *price_multiplier_b),
        demand: *demand,
    }
}

/// Checks `offers` for [`Context::spawn_villager`], failing with the
/// [`custom::Error::Offer`] it would: at most [`custom::MAX_OFFERS`] offers, each with a payment,
/// a sale, uses at most its maximum, counters at most [`custom::MAX_TRADE_COUNTER`], a tier at most
/// [`custom::MAX_TRADE_TIER`] and price multipliers passing [`custom::is_price_multiplier`],
/// whose saved form takes at most 2 MiB. [`Context::set_trade_offers`] checks the same, but counts
/// towards the 2 MiB the offer data a villager loaded from a saved world keeps unread.
pub fn check_offers(offers: &[Offer]) -> Result<(), custom::Error> {
    new_offers(offers).map(drop)
}

/// Builds the stored offers of a new villager trading `offers` ([`check_offers`]).
pub(in crate::tick) fn new_offers(offers: &[Offer]) -> Result<Offers, custom::Error> {
    let mut stored = Offers::default();
    stored
        .set(offers.iter().map(to_recipe).collect())
        .map_err(to_public)?;
    Ok(stored)
}

/// Runs [`change_offers`] with the public error.
fn change<T>(
    state: &mut State,
    villager: Id,
    f: impl FnOnce(&mut Offers) -> Result<T>,
) -> Result<T, custom::Error> {
    change_offers(state, villager, f).map_err(to_public)
}

impl Context<'_> {
    /// The offers of the villager NPC `villager`, in screen order; `None` if it is not one or is
    /// dying.
    ///
    /// Saved offers the server cannot read (an unknown item, a value out of range) are not
    /// shown, and keys it does not read are not part of an [`Offer`]. A saved offer whose second
    /// payment matches damage unlike the first shows the first's rule
    /// ([`Offer::takes_any_damage`]). Writing offers back through [`Self::set_trade_offers`] or
    /// [`Self::replace_trade_offer`] gives both payments that rule and drops those keys.
    pub fn trade_offers(&self, villager: Id) -> Option<Vec<Offer>> {
        let v = (HOOKS.villager)(self.state, villager)?;
        Some(v.offers.recipes().iter().map(to_offer).collect())
    }

    /// The tier (0 to [`custom::MAX_TRADE_TIER`]) of the villager NPC `villager`; `None` if it is
    /// not one or is dying.
    pub fn trade_tier(&self, villager: Id) -> Option<u8> {
        (HOOKS.villager)(self.state, villager).map(|v| v.offers.tier())
    }

    /// The trade experience of the villager NPC `villager`; `None` if it is not one or is dying.
    pub fn trade_experience(&self, villager: Id) -> Option<u32> {
        (HOOKS.villager)(self.state, villager).map(|v| v.offers.experience())
    }

    /// The trade experience each tier of the villager NPC `villager` needs; `None` if it is not
    /// one or is dying.
    pub fn trade_tier_experience(&self, villager: Id) -> Option<[u32; custom::TRADE_TIERS]> {
        (HOOKS.villager)(self.state, villager).map(|v| v.offers.tier_experience())
    }

    /// Replaces the offers of the villager NPC `villager`; players trading with it see the new list
    /// at once.
    ///
    /// Fails, changing nothing, with [`custom::Error::NotVillager`] if it is not one or is dying,
    /// and with [`custom::Error::Offer`] for offers [`check_offers`] refuses. The 2 MiB count the
    /// saved offers the server cannot read too ([`Self::trade_offers`]): they stay after the new
    /// list and are shown again once they can be read (their custom items registered).
    pub fn set_trade_offers(
        &mut self,
        villager: Id,
        offers: &[Offer],
    ) -> Result<(), custom::Error> {
        let recipes = offers.iter().map(to_recipe).collect();
        change(self.state, villager, |o| o.set(recipes))
    }

    /// Adds `offer` at the end of the villager's offers. Fails, changing nothing, as
    /// [`Self::set_trade_offers`] does for the list with `offer` added.
    pub fn add_trade_offer(&mut self, villager: Id, offer: &Offer) -> Result<(), custom::Error> {
        let recipe = to_recipe(offer);
        change(self.state, villager, |o| o.add(recipe))
    }

    /// Replaces the villager's offer at `index`. Fails, changing nothing, as
    /// [`Self::set_trade_offers`] does for the list with the offer replaced, or with
    /// [`custom::Error::Offer`] when there is no offer at `index`.
    pub fn replace_trade_offer(
        &mut self,
        villager: Id,
        index: usize,
        offer: &Offer,
    ) -> Result<(), custom::Error> {
        let recipe = to_recipe(offer);
        change(self.state, villager, |o| o.replace(index, recipe))
    }

    /// Removes and returns the villager's offer at `index`. Fails, changing nothing, with
    /// [`custom::Error::NotVillager`] as [`Self::set_trade_offers`] does, or with
    /// [`custom::Error::Offer`] when there is no offer at `index`.
    pub fn remove_trade_offer(
        &mut self,
        villager: Id,
        index: usize,
    ) -> Result<Offer, custom::Error> {
        change(self.state, villager, |o| o.remove(index)).map(|r| to_offer(&r))
    }

    /// Sets the villager's tier (0 to [`custom::MAX_TRADE_TIER`]); its trade experience moves to
    /// at least the tier's need and below the next higher need, so only experience a trade gives
    /// raises the tier again (a later tier that needs no more than this one is reached by the
    /// next trade). Fails, changing nothing, with [`custom::Error::Offer`] for a higher tier, or
    /// with [`custom::Error::NotVillager`] as [`Self::set_trade_offers`] does.
    pub fn set_trade_tier(&mut self, villager: Id, tier: u8) -> Result<(), custom::Error> {
        change(self.state, villager, |o| o.set_tier(tier))
    }

    /// Sets the villager's trade experience (at most [`custom::MAX_TRADE_COUNTER`]) and raises
    /// its tier to the highest one the experience reaches; a lower experience keeps the tier.
    /// Players who see it are shown the new values. Fails, changing nothing, with
    /// [`custom::Error::Offer`] for a higher experience, or with [`custom::Error::NotVillager`] as
    /// [`Self::set_trade_offers`] does.
    pub fn set_trade_experience(
        &mut self,
        villager: Id,
        experience: u32,
    ) -> Result<(), custom::Error> {
        change(self.state, villager, |o| o.set_experience(experience))
    }

    /// Sets the trade experience each tier needs ([`custom::is_tier_experience`]; BDS's are 0,
    /// 10, 70, 150 and 250) and raises the tier to the highest one the experience reaches; higher
    /// needs keep the tier. Fails, changing nothing, with [`custom::Error::Offer`] for needs
    /// [`custom::is_tier_experience`] refuses, or with [`custom::Error::NotVillager`] as
    /// [`Self::set_trade_offers`] does.
    pub fn set_trade_tier_experience(
        &mut self,
        villager: Id,
        tiers: [u32; custom::TRADE_TIERS],
    ) -> Result<(), custom::Error> {
        change(self.state, villager, |o| o.set_tier_experience(tiers))
    }

    /// Sets the uses of every offer of the villager back to 0 (a restock). Fails, changing
    /// nothing, with [`custom::Error::NotVillager`] as [`Self::set_trade_offers`] does.
    pub fn reset_trade_uses(&mut self, villager: Id) -> Result<(), custom::Error> {
        change(self.state, villager, |o| {
            o.reset_uses();
            Ok(())
        })
    }
}
