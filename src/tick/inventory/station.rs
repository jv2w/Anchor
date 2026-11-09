//! The request actions of the trade window.
//!
//! The executor hands the trade window everything it does not do itself: [`action`] begins a
//! `CraftRecipe` (a trade) or a `CraftRecipeAuto` (the quick trade) with the executor's slot
//! operations, and [`finish`] checks at the end of the request that exactly the offer's payments
//! were consumed, from the window's two payment slots (a quick trade's also from the main
//! inventory). Every other craft action is refused.
//!
//! A trade puts the offer's sale into the created output; what it does outside the slots (a
//! counted trade) is a [`Traded`] the commit applies ([`apply`]). What the request needs from
//! outside the inventory (the trade offers, the creative items) is read before it runs
//! ([`outside`]).

use std::collections::BTreeMap;

use super::request::{Created, Error, Exec, Made, Result};
use super::{SlotRef, TRADE_PAYMENT_A, TRADE_PAYMENT_B, WindowKind};
use crate::item::Stack;
use crate::protocol::{ItemStackRequest, StackRequestAction};
use crate::tick::{SessionId, State, trade};

/// What a request needs from outside the inventory: the trade window's offers and the creative
/// items.
#[derive(Debug, Default)]
pub(super) struct Outside {
    /// The trade offers the request's `CraftRecipe` and `CraftRecipeAuto` actions name.
    offers: BTreeMap<u32, trade::Offer>,
    /// The server's custom content: its creative items.
    pub(super) custom: std::sync::Arc<crate::custom::Registry>,
    /// The player's creative inventory, by its index in the custom content.
    pub(super) creative: u16,
}

/// A trade in progress in a request, with what it must consume.
#[derive(Debug, Clone)]
pub(super) struct Craft {
    net_id: u32,
    offer: trade::Offer,
    times: u8,
    payments: Payments,
}

/// Where a trade's payments come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Payments {
    /// `CraftRecipe`: the two payment slots, the first payment from the first.
    Slots,
    /// `CraftRecipeAuto`, the quick trade: the payment slots and the main inventory, in any mix.
    /// The trade button and a right click on the arrow send it.
    Anywhere,
}

/// What a committed trade does besides moving items: `times` trades of offer `net_id`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Traded {
    net_id: u32,
    times: u32,
}

/// What `request` of `session` needs from outside the inventory, read before the request runs.
/// The executor sees only the inventory.
pub(super) fn outside(state: &State, session: SessionId, request: &ItemStackRequest) -> Outside {
    let Some(player) = state.players.get(&session) else {
        return Outside::default();
    };
    let offers = if matches!(
        player.inventory.window.as_ref().map(|w| w.kind),
        Some(WindowKind::Trade { .. })
    ) {
        request
            .actions
            .iter()
            .filter_map(|a| match a {
                StackRequestAction::CraftRecipe(c) => Some(c.recipe_network_id),
                StackRequestAction::AutoCraftRecipe(c) => Some(c.recipe_network_id),
                _ => None,
            })
            .take(super::request::MAX_ACTIONS)
            .filter_map(|id| Some((id, trade::offer(state, session, id)?)))
            .collect()
    } else {
        BTreeMap::new()
    };
    Outside {
        offers,
        custom: std::sync::Arc::clone(&state.custom),
        creative: player.inventory.creative,
    }
}

/// Runs a craft request action: only a trade window takes one.
pub(super) fn action(exec: &mut Exec<'_>, action: &StackRequestAction) -> Result<()> {
    if exec.craft.is_some() {
        return Err(Error::Trade);
    }
    let craft = match action {
        StackRequestAction::CraftRecipe(a) if is_trading(exec) => begin_trade(
            exec,
            a.recipe_network_id,
            a.number_of_crafts,
            Payments::Slots,
        )?,
        // The client's ingredient list is not used: the consumes are checked against the offer.
        StackRequestAction::AutoCraftRecipe(a) if is_trading(exec) => begin_trade(
            exec,
            a.recipe_network_id,
            a.number_of_crafts,
            Payments::Anywhere,
        )?,
        _ => return Err(Error::Unsupported),
    };
    exec.craft = Some(craft);
    Ok(())
}

/// Begins `CraftRecipe` or `CraftRecipeAuto` in a trade window.
/// The offer with that network ID is traded `times` times; its payments come from where
/// `payments` says.
fn begin_trade(exec: &mut Exec<'_>, net_id: u32, times: u8, payments: Payments) -> Result<Craft> {
    let offer = exec
        .outside
        .offers
        .get(&net_id)
        .cloned()
        .ok_or(Error::Offer)?;
    // At most the offer's uses left, capped at `trade::MAX_REPEAT`; the action's count is a byte.
    if times == 0
        || u32::from(times) > offer.max_times
        || offer.sell.is_empty()
        || offer.buy_a.is_empty()
    {
        return Err(Error::Offer);
    }
    exec.set_created(Some(Created::of(&offer.sell, u32::from(times))))?;
    if exec.describe {
        exec.made = Some(Made::of(
            net_id,
            [offer.buy_a.clone(), offer.buy_b.clone()],
            offer.sell.clone(),
            u32::from(times),
        ));
    }
    Ok(Craft {
        net_id,
        offer,
        times,
        payments,
    })
}

/// Whether a trade window is open.
fn is_trading(exec: &Exec<'_>) -> bool {
    exec.window
        .is_some_and(|w| matches!(w.kind, WindowKind::Trade { .. }))
}

/// Ends a trade: checks that the request consumed exactly the offer's payments.
/// Its effect waits for the commit.
pub(super) fn finish(exec: &mut Exec<'_>) -> Result<()> {
    let Some(Craft {
        net_id,
        offer,
        times,
        payments,
    }) = exec.craft.take()
    else {
        return Ok(());
    };
    let times = u32::from(times);
    exec.traded = Some(Traded { net_id, times });
    let b = u32::from(offer.buy_b.count()) * times;
    let ok = match payments {
        Payments::Slots => {
            has_consumed_only_from(exec, &[TRADE_PAYMENT_A, TRADE_PAYMENT_B])
                && is_exact(
                    exec,
                    TRADE_PAYMENT_A,
                    u32::from(offer.buy_a.count()) * times,
                    pays(&offer.buy_a, offer.damage_a),
                )
                && (b == 0
                    || is_exact(exec, TRADE_PAYMENT_B, b, pays(&offer.buy_b, offer.damage_b)))
                && (b != 0 || taken_from(exec, TRADE_PAYMENT_B, |_| true).0 == 0)
        }
        Payments::Anywhere => is_paid_anywhere(exec, &offer, times),
    };
    if ok { Ok(()) } else { Err(Error::Consumed) }
}

/// Applies the effect of the committed request `request` of `session`: the trade's count.
pub(super) fn apply(state: &mut State, session: SessionId, request: i32, traded: Traded) {
    trade::traded(state, session, request, traded.net_id, traded.times);
}

/// Whether the consume actions took exactly `want` from the window's own slot `i`, all of it
/// `like`.
fn is_exact(exec: &Exec<'_>, i: u8, want: u32, like: impl Fn(&Stack) -> bool) -> bool {
    let (count, all) = taken_from(exec, i, like);
    count == want && all
}

/// What the consume actions took from the window's own slot `i`.
/// That is the count, and whether every stack was `like` (the same item, damage and data).
fn taken_from(exec: &Exec<'_>, i: u8, like: impl Fn(&Stack) -> bool) -> (u32, bool) {
    let mut count = 0_u32;
    let mut all = true;
    for (r, stack) in &exec.consumed {
        if *r == Some(SlotRef::Ui(i)) {
            count += u32::from(stack.count());
            all &= like(stack);
        }
    }
    (count, all)
}

/// Whether every consume action took from the window's own slots in `allowed`.
fn has_consumed_only_from(exec: &Exec<'_>, allowed: &[u8]) -> bool {
    exec.consumed
        .iter()
        .all(|(r, _)| matches!(r, Some(SlotRef::Ui(i)) if allowed.contains(i)))
}

/// Whether a stack is the offer's payment `want` ([`trade::is_payment`]).
fn pays(want: &Stack, damage: trade::Damage) -> impl Fn(&Stack) -> bool + '_ {
    move |s: &Stack| trade::is_payment(want, damage, s)
}

/// Whether the consume actions of a quick trade took exactly `times` of the offer's payments.
/// They come from the payment slots and the main inventory. A stack that pays both payments (the
/// same item twice) counts toward whichever still needs it.
fn is_paid_anywhere(exec: &Exec<'_>, offer: &trade::Offer, times: u32) -> bool {
    let want_a = u32::from(offer.buy_a.count()) * times;
    let want_b = u32::from(offer.buy_b.count()) * times;
    let (pays_a, pays_b) = (
        pays(&offer.buy_a, offer.damage_a),
        pays(&offer.buy_b, offer.damage_b),
    );
    let (mut only_a, mut only_b, mut either) = (0_u32, 0_u32, 0_u32);
    for (r, stack) in &exec.consumed {
        if !matches!(
            r,
            Some(SlotRef::Main(_) | SlotRef::Ui(TRADE_PAYMENT_A | TRADE_PAYMENT_B))
        ) {
            return false;
        }
        let count = u32::from(stack.count());
        let total = match (pays_a(stack), want_b > 0 && pays_b(stack)) {
            (true, true) => &mut either,
            (true, false) => &mut only_a,
            (false, true) => &mut only_b,
            (false, false) => return false,
        };
        *total = total.saturating_add(count);
    }
    only_a <= want_a
        && only_b <= want_b
        && only_a.saturating_add(only_b).saturating_add(either) == want_a + want_b
}
