//! The typed trade API: [`Context`] methods that read and change a villager NPC's offers as
//! [`Offer`] values, and the conversion between them and the stored offers.

use super::super::{Context, State};
use super::{Damage, Error, HOOKS, Offers, Recipe, Result, change_offers};
use crate::custom::{self, Offer};
use crate::entity::Id;

impl Error {
    /// The public error of a refused offer change.
    fn public(self) -> custom::Error {
        match self {
            Self::NotVillager => custom::Error::NotVillager,
            Self::NoPayment => offer("the offer has no payment"),
            Self::NoSale => offer("the offer sells nothing"),
            Self::Uses => offer("uses above the maximum or a counter above i32::MAX"),
            Self::Tier => offer("a tier above 4"),
            Self::Multiplier => offer("a price multiplier that is not finite"),
            Self::TierExperience => offer("tier experience that goes down or is above i32::MAX"),
            Self::Experience => offer("trade experience above i32::MAX"),
            Self::Full => offer("more than 128 offers"),
            Self::TooLarge => offer("offers whose saved form is over 2 MiB"),

            Self::NoOffer => offer("no offer at that index"),
        }
    }
}

/// [`custom::Error::Offer`] with `reason`.
fn offer(reason: &'static str) -> custom::Error {
    custom::Error::Offer { reason }
}

/// The stored form of an offer.
fn to_recipe(o: &Offer) -> Recipe {
    let damage = |any: bool| if any { Damage::Any } else { Damage::Exact };
    let mut r = Recipe::new(o.buy_a.clone(), o.buy_b.clone(), o.sell.clone(), o.max_uses);
    r.damage_a = damage(o.any_damage);
    r.damage_b = damage(o.any_damage && !o.buy_b.is_empty());
    r.uses = o.uses;
    r.tier = o.tier;
    r.trader_exp = o.trader_exp;
    r.reward_exp = o.reward_exp;
    (r.price_multiplier_a, r.price_multiplier_b) = o.price_multipliers;
    r.demand = o.demand;
    r
}

/// The typed form of a stored offer (its unknown saved keys stay with the villager only).
fn to_offer(r: &Recipe) -> Offer {
    Offer {
        buy_a: r.buy_a.clone(),
        buy_b: r.buy_b.clone(),
        any_damage: r.damage_a == Damage::Any,
        sell: r.sell.clone(),
        max_uses: r.max_uses,
        uses: r.uses,
        tier: r.tier,
        trader_exp: r.trader_exp,
        reward_exp: r.reward_exp,
        price_multipliers: (r.price_multiplier_a, r.price_multiplier_b),
        demand: r.demand,
    }
}

/// The stored offers of a new villager trading `offers`, checked as
/// [`Context::set_trade_offers`] checks them.
pub(in crate::tick) fn new_offers(offers: &[Offer]) -> std::result::Result<Offers, custom::Error> {
    let mut stored = Offers::default();
    stored
        .set(offers.iter().map(to_recipe).collect())
        .map_err(Error::public)?;
    Ok(stored)
}

/// [`change_offers`] with the public error.
fn change<T>(
    state: &mut State,
    villager: Id,
    f: impl FnOnce(&mut Offers) -> Result<T>,
) -> std::result::Result<T, custom::Error> {
    change_offers(state, villager, f).map_err(Error::public)
}

impl Context<'_> {
    /// The offers of the villager NPC `villager`, in screen order; `None` if it is not one.
    pub fn trade_offers(&self, villager: Id) -> Option<Vec<Offer>> {
        let v = (HOOKS.villager)(self.state, villager)?;
        Some(v.offers.recipes().iter().map(to_offer).collect())
    }

    /// The tier (0 to [`custom::MAX_TRADE_TIER`]) of the villager NPC `villager`; `None` if it is not one.
    pub fn trade_tier(&self, villager: Id) -> Option<u8> {
        (HOOKS.villager)(self.state, villager).map(|v| v.offers.tier())
    }

    /// The trade experience of the villager NPC `villager`; `None` if it is not one.
    #[cfg(test)]
    pub(in crate::tick) fn trade_experience(&self, villager: Id) -> Option<u32> {
        (HOOKS.villager)(self.state, villager).map(|v| v.offers.experience())
    }

    /// The trade experience each tier of the villager NPC `villager` needs; `None` if it is not
    /// one.
    #[cfg(test)]
    pub(in crate::tick) fn trade_tier_experience(
        &self,
        villager: Id,
    ) -> Option<[u32; custom::TRADE_TIERS]> {
        (HOOKS.villager)(self.state, villager).map(|v| v.offers.tier_experience())
    }

    /// Replaces the offers of the villager NPC `villager` (at most 128; each needs a payment,
    /// a sale, uses at most its maximum, a tier up to [`custom::MAX_TRADE_TIER`] and finite price
    /// multipliers). Players trading with it see the new list at once. Nothing changes on failure.
    pub fn set_trade_offers(
        &mut self,
        villager: Id,
        offers: &[Offer],
    ) -> std::result::Result<(), custom::Error> {
        let recipes = offers.iter().map(to_recipe).collect();
        change(self.state, villager, |o| o.set(recipes))
    }

    /// Adds `offer` at the end of the villager's offers.
    pub fn add_trade_offer(
        &mut self,
        villager: Id,
        offer: &Offer,
    ) -> std::result::Result<(), custom::Error> {
        let recipe = to_recipe(offer);
        change(self.state, villager, |o| o.add(recipe))
    }

    /// Replaces the villager's offer at `index`.
    pub fn replace_trade_offer(
        &mut self,
        villager: Id,
        index: usize,
        offer: &Offer,
    ) -> std::result::Result<(), custom::Error> {
        let recipe = to_recipe(offer);
        change(self.state, villager, |o| o.replace(index, recipe))
    }

    /// Removes and returns the villager's offer at `index`.
    pub fn remove_trade_offer(
        &mut self,
        villager: Id,
        index: usize,
    ) -> std::result::Result<Offer, custom::Error> {
        change(self.state, villager, |o| o.remove(index)).map(|r| to_offer(&r))
    }

    /// Sets the villager's tier (0 to [`custom::MAX_TRADE_TIER`]); its trade experience rises to
    /// the tier's requirement.
    pub fn set_trade_tier(
        &mut self,
        villager: Id,
        tier: u8,
    ) -> std::result::Result<(), custom::Error> {
        change(self.state, villager, |o| o.set_tier(tier))
    }

    /// Sets the villager's trade experience (at most [`custom::MAX_TRADE_COUNTER`]); its tier
    /// stays until a trade reaches the next one. Players who see it are shown the new value.
    pub fn set_trade_experience(
        &mut self,
        villager: Id,
        experience: u32,
    ) -> std::result::Result<(), custom::Error> {
        change(self.state, villager, |o| o.set_experience(experience))
    }

    /// Sets the trade experience each tier needs (never going down, at most
    /// [`custom::MAX_TRADE_COUNTER`]; BDS's are 0, 10, 70, 150 and 250).
    pub fn set_trade_tier_experience(
        &mut self,
        villager: Id,
        tiers: [u32; custom::TRADE_TIERS],
    ) -> std::result::Result<(), custom::Error> {
        change(self.state, villager, |o| o.set_tier_experience(tiers))
    }

    /// Sets the uses of every offer of the villager back to 0 (a restock).
    pub fn reset_trade_uses(&mut self, villager: Id) -> std::result::Result<(), custom::Error> {
        change(self.state, villager, |o| {
            o.reset_uses();
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::offers::{MAX_OFFERS, MAX_SAVED_BYTES};
    use super::*;

    /// The reason of `error`'s public form.
    fn reason(error: Error) -> &'static str {
        match error.public() {
            custom::Error::Offer { reason } => reason,
            other => panic!("not an offer error: {other:?}"),
        }
    }

    /// The reasons restate the limits as text: a changed limit fails here until its text follows.
    #[test]
    fn offer_error_reasons_name_the_current_limits() {
        assert!(reason(Error::Full).contains(&format!(" {MAX_OFFERS} ")));
        assert_eq!(MAX_SAVED_BYTES % (1 << 20), 0, "a whole number of MiB");
        assert!(reason(Error::TooLarge).contains(&format!(" {} MiB", MAX_SAVED_BYTES >> 20)));
        assert!(reason(Error::Tier).ends_with(&format!(" {}", custom::MAX_TRADE_TIER)));
    }
}
