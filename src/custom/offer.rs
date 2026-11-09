//! Villager trade offers as typed values: what the API takes and gives instead of offer NBT. The
//! villager keeps them in its entity; [`Context::set_trade_offers`] and the other trade methods
//! read and change them.
//!
//! [`Context::set_trade_offers`]: crate::Context::set_trade_offers

use crate::item::Stack;

/// The price multiplier the official server gives most offers.
pub(crate) const DEFAULT_PRICE_MULTIPLIER: f32 = 0.05;

/// The most offers one villager holds ([`check_offers`](super::check_offers)): well above the
/// largest vanilla list (about 20); the saved offers take at most 2 MiB apart from it.
pub const MAX_OFFERS: usize = 128;

/// The largest trade counter, as an NBT `TAG_Int` saves it: an offer's
/// [`max_uses`](Offer::max_uses), [`uses`](Offer::uses) and
/// [`trader_xp`](Offer::trader_xp), a villager's trade xp
/// ([`Context::set_trade_xp`]), each tier's need ([`Context::set_trade_tier_xp`])
/// and the network IDs the server gives offers (sent as `netId`).
///
/// [`Context::set_trade_xp`]: crate::Context::set_trade_xp
/// [`Context::set_trade_tier_xp`]: crate::Context::set_trade_tier_xp
pub const MAX_TRADE_COUNTER: u32 = i32::MAX.cast_unsigned();

/// The highest villager tier, master; tiers start at 0, novice.
pub const MAX_TRADE_TIER: u8 = 4;

/// The number of villager tiers, 0 to [`MAX_TRADE_TIER`].
pub const TRADE_TIERS: usize = MAX_TRADE_TIER as usize + 1;

/// One trade a villager offers: what the player pays (one or two stacks, their counts are the
/// price) and what one trade gives, with its limits and rewards.
///
/// Prices are the stacks' real counts; the price multipliers and the demand are only sent to
/// the client as they are (Anchor runs no villager economy).
#[derive(Debug, Clone, PartialEq)]
pub struct Offer {
    pub(crate) buy_a: Stack,
    pub(crate) buy_b: Stack,
    pub(crate) any_damage: bool,
    pub(crate) sell: Stack,
    pub(crate) max_uses: u32,
    pub(crate) uses: u32,
    pub(crate) tier: u8,
    pub(crate) trader_xp: u32,
    pub(crate) reward_xp: bool,
    pub(crate) price_multipliers: (f32, f32),
    pub(crate) demand: i32,
}

impl Offer {
    /// An offer of `sell` for `buy` at most `max_uses` times, with the official server's
    /// defaults: no second payment, tier 0 (novice), no villager xp, reward orbs for the player,
    /// price multipliers 0.05, demand 0.
    pub fn new(buy: Stack, sell: Stack, max_uses: u32) -> Self {
        Self {
            buy_a: buy,
            buy_b: Stack::EMPTY,
            any_damage: false,
            sell,
            max_uses,
            uses: 0,
            tier: 0,
            trader_xp: 0,
            reward_xp: true,
            price_multipliers: (DEFAULT_PRICE_MULTIPLIER, DEFAULT_PRICE_MULTIPLIER),
            demand: 0,
        }
    }

    /// The first payment; its count is the price.
    pub fn buy(&self) -> &Stack {
        &self.buy_a
    }

    /// The demand sent with the offer.
    pub fn demand(&self) -> i32 {
        self.demand
    }

    /// The trades the offer allows until its uses are reset.
    pub fn max_uses(&self) -> u32 {
        self.max_uses
    }

    /// The price multipliers sent with the offer.
    pub fn price_multipliers(&self) -> (f32, f32) {
        self.price_multipliers
    }

    /// Whether each trade drops xp orbs for the player.
    pub fn rewards_xp(&self) -> bool {
        self.reward_xp
    }

    /// The second payment, or the empty stack.
    pub fn second_payment(&self) -> &Stack {
        &self.buy_b
    }

    /// What one trade gives.
    pub fn sell(&self) -> &Stack {
        &self.sell
    }

    /// Whether the payments are taken of any damage.
    pub fn takes_any_damage(&self) -> bool {
        self.any_damage
    }

    /// The villager tier that unlocks the offer.
    pub fn tier(&self) -> u8 {
        self.tier
    }

    /// The villager xp per trade.
    pub fn trader_xp(&self) -> u32 {
        self.trader_xp
    }

    /// The trades made since the last reset.
    pub fn uses(&self) -> u32 {
        self.uses
    }

    /// The same offer taking its payments of any damage (worn tools, any meta), as the official
    /// server's offers carry `Damage` 32767.
    #[must_use]
    pub fn any_damage(mut self) -> Self {
        self.any_damage = true;
        self
    }

    /// The same offer sent with this demand (it does not change the price).
    #[must_use]
    pub fn with_demand(mut self, demand: i32) -> Self {
        self.demand = demand;
        self
    }

    /// The same offer sent with these price multipliers (they do not change the price; each must
    /// pass [`is_price_multiplier`]).
    #[must_use]
    pub fn with_price_multipliers(mut self, first: f32, second: f32) -> Self {
        self.price_multipliers = (first, second);
        self
    }

    /// The same offer with a second payment.
    #[must_use]
    pub fn with_second_payment(mut self, buy: Stack) -> Self {
        self.buy_b = buy;
        self
    }

    /// The same offer unlocked at villager tier `tier` (at most [`MAX_TRADE_TIER`]).
    #[must_use]
    pub fn with_tier(mut self, tier: u8) -> Self {
        self.tier = tier;
        self
    }

    /// The same offer giving the villager `xp` per trade (toward its next tier).
    #[must_use]
    pub fn with_trader_xp(mut self, xp: u32) -> Self {
        self.trader_xp = xp;
        self
    }

    /// The same offer with `uses` trades already made.
    #[must_use]
    pub fn with_uses(mut self, uses: u32) -> Self {
        self.uses = uses;
        self
    }

    /// The same offer dropping no xp orbs for the player.
    #[must_use]
    pub fn without_reward_xp(mut self) -> Self {
        self.reward_xp = false;
        self
    }
}

/// Whether an offer list takes `multiplier` as one of an offer's price multipliers
/// ([`Offer::with_price_multipliers`]): it is finite.
pub fn is_price_multiplier(multiplier: f32) -> bool {
    multiplier.is_finite()
}

/// Whether `tiers` is the trade xp each tier needs as
/// [`Context::set_trade_tier_xp`] takes it: never going down, each at most
/// [`MAX_TRADE_COUNTER`].
///
/// [`Context::set_trade_tier_xp`]: crate::Context::set_trade_tier_xp
pub fn is_tier_xp(tiers: [u32; TRADE_TIERS]) -> bool {
    tiers.windows(2).all(|w| w.first() <= w.get(1)) && tiers.iter().all(|&x| x <= MAX_TRADE_COUNTER)
}
