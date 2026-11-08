//! The typed trade API: [`Context`] methods that read and change a villager NPC's offers as
//! [`Offer`] values, and the conversion between them and the stored offers.

use super::{Damage, Error, HOOKS, Offers, Result, StoredOffer, change_offers};
use crate::custom::{self, Offer};
use crate::entity::Id;
use crate::nbt::Compound;
use crate::tick::{Context, State};

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
        Some(v.offers.list().iter().map(to_offer).collect())
    }

    /// The tier (0 to [`custom::MAX_TRADE_TIER`]) of the villager NPC `villager`; `None` if it is
    /// not one or is dying.
    pub fn trade_tier(&self, villager: Id) -> Option<u8> {
        (HOOKS.villager)(self.state, villager).map(|v| v.offers.tier())
    }

    /// The trade xp each tier of the villager NPC `villager` needs; `None` if it is not
    /// one or is dying.
    pub fn trade_tier_xp(&self, villager: Id) -> Option<[u32; custom::TRADE_TIERS]> {
        (HOOKS.villager)(self.state, villager).map(|v| v.offers.tier_xp())
    }

    /// The trade xp of the villager NPC `villager`; `None` if it is not one or is dying.
    pub fn trade_xp(&self, villager: Id) -> Option<u32> {
        (HOOKS.villager)(self.state, villager).map(|v| v.offers.xp())
    }

    /// Adds `offer` at the end of the villager's offers.
    ///
    /// # Errors
    ///
    /// [`custom::Error::NotVillager`] as [`Self::set_trade_offers`] and [`custom::Error::Offer`]
    /// for an offer [`check_offers`] refuses or a list it would make too long or too large.
    /// Nothing is changed then.
    pub fn add_trade_offer(&mut self, villager: Id, offer: &Offer) -> custom::Result<()> {
        let stored = to_stored(offer);
        change(self.state, villager, |o| o.add(stored))
    }

    /// Removes and returns the villager's offer at `index`.
    ///
    /// # Errors
    ///
    /// [`custom::Error::NotVillager`] as [`Self::set_trade_offers`] and [`custom::Error::Offer`]
    /// when there is no offer at `index`.
    pub fn remove_trade_offer(&mut self, villager: Id, index: usize) -> custom::Result<Offer> {
        change(self.state, villager, |o| o.remove(index)).map(|stored| to_offer(&stored))
    }

    /// Replaces the villager's offer at `index`.
    ///
    /// # Errors
    ///
    /// [`custom::Error::NotVillager`] as [`Self::set_trade_offers`] and [`custom::Error::Offer`]
    /// for an offer [`check_offers`] refuses, a list it would make too large, or no offer at
    /// `index`. Nothing is changed then.
    pub fn replace_trade_offer(
        &mut self,
        villager: Id,
        index: usize,
        offer: &Offer,
    ) -> custom::Result<()> {
        let stored = to_stored(offer);
        change(self.state, villager, |o| o.replace(index, stored))
    }

    /// Sets the uses of every offer of the villager back to 0 (a restock).
    ///
    /// # Errors
    ///
    /// [`custom::Error::NotVillager`] as [`Self::set_trade_offers`].
    pub fn reset_trade_uses(&mut self, villager: Id) -> custom::Result<()> {
        change(self.state, villager, |o| {
            o.reset_uses();
            Ok(())
        })
    }

    /// Replaces the offers of the villager NPC `villager`; players trading with it see the new list
    /// at once.
    ///
    /// The saved offers the server cannot read ([`Self::trade_offers`]) are not replaced: they
    /// stay after the new list, count towards the 2 MiB, and are read again when the villager is
    /// next loaded with their custom items registered.
    ///
    /// # Errors
    ///
    /// [`custom::Error::NotVillager`] if it is not one or is dying and [`custom::Error::Offer`]
    /// for offers [`check_offers`] refuses. Nothing is changed then.
    pub fn set_trade_offers(&mut self, villager: Id, offers: &[Offer]) -> custom::Result<()> {
        let stored = offers.iter().map(to_stored).collect();
        change(self.state, villager, |o| o.set(stored))
    }

    /// Sets the villager's tier (0 to [`custom::MAX_TRADE_TIER`]); its trade xp moves to
    /// at least the tier's need and below the next higher need, so only xp a trade gives
    /// raises the tier again (a later tier that needs no more than this one is reached by the
    /// next trade).
    ///
    /// # Errors
    ///
    /// [`custom::Error::Offer`] for a higher tier and [`custom::Error::NotVillager`] as
    /// [`Self::set_trade_offers`].
    pub fn set_trade_tier(&mut self, villager: Id, tier: u8) -> custom::Result<()> {
        change(self.state, villager, |o| o.set_tier(tier))
    }

    /// Sets the trade xp each tier needs ([`custom::is_tier_xp`]; the defaults are 0,
    /// 10, 70, 150 and 250) and raises the tier to the highest one the xp reaches; higher
    /// needs keep the tier.
    ///
    /// # Errors
    ///
    /// [`custom::Error::Offer`] for needs [`custom::is_tier_xp`] refuses and
    /// [`custom::Error::NotVillager`] as [`Self::set_trade_offers`].
    pub fn set_trade_tier_xp(
        &mut self,
        villager: Id,
        tiers: [u32; custom::TRADE_TIERS],
    ) -> custom::Result<()> {
        change(self.state, villager, |o| o.set_tier_xp(tiers))
    }

    /// Sets the villager's trade xp (at most [`custom::MAX_TRADE_COUNTER`]) and raises
    /// its tier to the highest one the xp reaches; a lower xp keeps the tier.
    /// Players who see it are shown the new values.
    ///
    /// # Errors
    ///
    /// [`custom::Error::Offer`] for a higher xp and [`custom::Error::NotVillager`] as
    /// [`Self::set_trade_offers`].
    pub fn set_trade_xp(&mut self, villager: Id, xp: u32) -> custom::Result<()> {
        change(self.state, villager, |o| o.set_xp(xp))
    }
}

/// Checks `offers` for [`Context::spawn_villager`], failing with the
/// [`custom::Error::Offer`] it would: at most [`custom::MAX_OFFERS`] offers, each with a payment,
/// a sale, uses at most its maximum, counters at most [`custom::MAX_TRADE_COUNTER`], a tier at most
/// [`custom::MAX_TRADE_TIER`] and price multipliers passing [`custom::is_price_multiplier`],
/// whose saved form takes at most 2 MiB. [`Context::set_trade_offers`] checks the same, but counts
/// towards the 2 MiB the offer data a villager loaded from a saved world keeps unread.
///
/// # Errors
///
/// [`custom::Error::Offer`] for the first rule `offers` breaks.
pub fn check_offers(offers: &[Offer]) -> custom::Result<()> {
    new_offers(offers).map(drop)
}

/// The stored offers of a new villager trading `offers` ([`check_offers`]).
pub(in crate::tick) fn new_offers(offers: &[Offer]) -> custom::Result<Offers> {
    let mut stored = Offers::default();
    stored
        .set(offers.iter().map(to_stored).collect())
        .map_err(to_public)?;
    Ok(stored)
}

/// Runs [`change_offers`] with the public error.
fn change<T>(
    state: &mut State,
    villager: Id,
    f: impl FnOnce(&mut Offers) -> Result<T>,
) -> custom::Result<T> {
    change_offers(state, villager, f).map_err(to_public)
}

/// Converts an offer into its stored form: the second payment takes any damage when the first
/// does, and there are no unknown saved keys.
fn to_stored(offer: &Offer) -> StoredOffer {
    let Offer {
        buy_a,
        buy_b,
        any_damage,
        sell,
        max_uses,
        uses,
        tier,
        trader_xp,
        reward_xp,
        price_multipliers: (price_multiplier_a, price_multiplier_b),
        demand,
    } = offer;
    let damage = |any: bool| if any { Damage::Any } else { Damage::Exact };
    StoredOffer {
        buy_a: buy_a.clone(),
        buy_b: buy_b.clone(),
        damage_a: damage(*any_damage),
        damage_b: damage(*any_damage && !buy_b.is_empty()),
        sell: sell.clone(),
        max_uses: *max_uses,
        uses: *uses,
        tier: *tier,
        trader_xp: *trader_xp,
        reward_xp: *reward_xp,
        price_multiplier_a: *price_multiplier_a,
        price_multiplier_b: *price_multiplier_b,
        demand: *demand,
        unknown: Compound::new(),
    }
}

/// Converts a stored offer into its typed form.
fn to_offer(stored: &StoredOffer) -> Offer {
    let StoredOffer {
        buy_a,
        buy_b,
        damage_a,
        // An `Offer` has one damage rule, the first payment's.
        damage_b: _,
        sell,
        max_uses,
        uses,
        tier,
        trader_xp,
        reward_xp,
        price_multiplier_a,
        price_multiplier_b,
        demand,
        // Unknown saved keys stay with the villager only.
        unknown: _,
    } = stored;
    Offer {
        buy_a: buy_a.clone(),
        buy_b: buy_b.clone(),
        any_damage: *damage_a == Damage::Any,
        sell: sell.clone(),
        max_uses: *max_uses,
        uses: *uses,
        tier: *tier,
        trader_xp: *trader_xp,
        reward_xp: *reward_xp,
        price_multipliers: (*price_multiplier_a, *price_multiplier_b),
        demand: *demand,
    }
}

/// Converts a refused offer change into the public error.
fn to_public(error: Error) -> custom::Error {
    match error {
        Error::NotVillager => custom::Error::NotVillager,
        _ => custom::Error::Offer {
            reason: error.reason(),
        },
    }
}
