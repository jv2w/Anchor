//! The events of an item stack request: the whole request as one [`InventoryTransaction`], then
//! the trade it makes ([`Trade`]) and what it drops ([`PlayerDropItem`]), each built only when
//! heard.
//!
//! Anchor raises the transaction for every request that changes a slot, so a listener that guards
//! every slot change also sees trades.

use super::{Describe, Error, Made, Plan, Result};
use crate::event::{Inventory, InventoryTransaction, PlayerDropItem, SlotChange, Trade};
use crate::inventory::Kind;
use crate::item::Stack;
use crate::tick::inventory::{SlotRef, WindowKind};
use crate::tick::{SessionId, State, dispatch, trade};

/// The events of one request, those heard.
#[derive(Debug)]
pub(super) struct Raised {
    transaction: Option<InventoryTransaction>,
    trade: Option<Trade>,
    drop: Option<PlayerDropItem>,
}

impl Raised {
    /// Whether a listener (not only a monitor) of a raised event ran, so the game may have
    /// changed.
    pub(super) fn has_listened(&self, state: &State) -> bool {
        (self.transaction.is_some() && dispatch::listened::<InventoryTransaction>(state))
            || (self.trade.is_some() && dispatch::listened::<Trade>(state))
            || (self.drop.is_some() && dispatch::listened::<PlayerDropItem>(state))
    }

    /// The heard events of `session`'s request `plan`.
    pub(super) fn of(state: &State, session: SessionId, plan: &Plan) -> Self {
        let player = crate::player::Id::new(session);
        let window = state
            .players
            .get(&session)
            .and_then(|p| p.inventory.window.as_ref())
            .map(|w| w.kind);
        let changes = if dispatch::heard::<InventoryTransaction>(state) {
            changes(plan, window)
        } else {
            Vec::new()
        };
        let transaction = (!changes.is_empty()).then_some(InventoryTransaction {
            player,
            changes,
            cancelled: false,
        });
        let trade = made(state, session, plan);
        let drop = plan
            .drop
            .as_ref()
            .filter(|_| dispatch::heard::<PlayerDropItem>(state))
            .map(|item| PlayerDropItem {
                player,
                item: item.clone(),
                cancelled: false,
            });
        Self {
            transaction,
            trade,
            drop,
        }
    }

    /// Calls the monitors of each raised event, after the request applied.
    pub(super) fn watch(self, state: &mut State) {
        let Self {
            transaction,
            trade,
            drop,
        } = self;
        if let Some(e) = transaction {
            dispatch::watch(state, &e);
        }
        if let Some(e) = trade {
            dispatch::watch(state, &e);
        }
        if let Some(e) = drop {
            dispatch::watch(state, &e);
        }
    }

    /// Calls the listeners of each event in order; a cancelled one refuses the request and the
    /// later events are not raised.
    pub(super) fn call(&mut self, state: &mut State) -> Result<()> {
        fn one<E: crate::event::Event>(
            state: &mut State,
            event: Option<&mut E>,
            cancelled: fn(&E) -> bool,
        ) -> Result<()> {
            if let Some(e) = event {
                dispatch::call(state, e);
                if cancelled(e) {
                    return Err(Error::Cancelled);
                }
            }
            Ok(())
        }
        one(state, self.transaction.as_mut(), |e| e.cancelled)?;
        one(state, self.trade.as_mut(), |e| e.cancelled)?;
        one(state, self.drop.as_mut(), |e| e.cancelled)
    }
}

/// What the plan of a request describes of itself: the slots it changes before and after when a
/// handler of the open menu `edit` or a listener of the transaction hears them, what it makes or
/// drops when only another request event is heard, else nothing.
pub(super) fn describes(state: &State, edit: Option<crate::menu::Id>) -> Describe {
    if edit.is_some() || dispatch::heard::<InventoryTransaction>(state) {
        Describe::Slots
    } else if dispatch::heard::<Trade>(state) || dispatch::heard::<PlayerDropItem>(state) {
        Describe::Made
    } else {
        Describe::Nothing
    }
}

/// The slots the described `plan` changes, as events name them, in `window`.
pub(super) fn changes(plan: &Plan, window: Option<WindowKind>) -> Vec<SlotChange> {
    plan.writes
        .iter()
        .zip(&plan.before)
        .filter_map(|((r, after), before)| change(*r, before, &after.stack, window))
        .collect()
}

/// Every slot `plan` names, changed or not, as events name them in the window `inv` has open; the
/// created output is left out.
pub(super) fn named(plan: &Plan, inv: &crate::tick::inventory::Inventory) -> Vec<(Inventory, u8)> {
    let window = inv.window.as_ref().map(|w| w.kind);
    plan.touched
        .iter()
        .filter_map(|&(container, slot)| place(inv.locate(container, slot)?, window))
        .collect()
}

/// The heard event of the trade `plan` makes.
fn made(state: &State, session: SessionId, plan: &Plan) -> Option<Trade> {
    let Made {
        net_id,
        inputs,
        output,
        times,
    } = plan.made.as_ref()?;
    if !dispatch::heard::<Trade>(state) {
        return None;
    }
    let (villager, offer) = trade::trading(state, session, *net_id)?;
    Some(Trade {
        player: crate::player::Id::new(session),
        villager,
        offer,
        inputs: inputs.clone(),
        output: output.clone(),
        uses: *times,
        cancelled: false,
    })
}

/// The change of request slot `r` from `before` to `after`, as events name it; `None` if it
/// holds the same.
fn change(
    r: SlotRef,
    before: &Stack,
    after: &Stack,
    window: Option<WindowKind>,
) -> Option<SlotChange> {
    if before == after {
        return None;
    }
    let (inventory, slot) = place(r, window)?;
    Some(SlotChange {
        inventory,
        slot: usize::from(slot),
        old_item: before.clone(),
        new_item: after.clone(),
    })
}

/// Where a request slot lives, as events name it.
fn place(r: SlotRef, window: Option<WindowKind>) -> Option<(Inventory, u8)> {
    Some(match r {
        SlotRef::Main(i) => (Inventory::Player(Kind::Main), i),
        SlotRef::Armor(i) => (Inventory::Player(Kind::Armor), i),
        SlotRef::Offhand => (Inventory::Player(Kind::Offhand), 0),
        SlotRef::Cursor => (Inventory::Player(Kind::Cursor), 0),
        SlotRef::Ender(i) => (Inventory::Player(Kind::EnderChest), i),
        SlotRef::Block(i) => {
            let [Some((world, pos)), _] = window?.blocks() else {
                return None;
            };
            (
                Inventory::Block {
                    world: crate::world::Id::of(world),
                    pos,
                },
                i,
            )
        }
        SlotRef::Ui(i) | SlotRef::Menu(i) => (Inventory::Window, i),
    })
}
