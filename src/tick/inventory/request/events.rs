//! The events of an item stack request: the whole request as one [`InventoryTransaction`], then
//! what it makes ([`CraftItem`], [`PlayerItemEnchant`], [`Trade`]) and what it drops
//! ([`PlayerDropItem`]), each built only when heard.
//!
//! PMMP raises only one of the first four (a crafting transaction raises `CraftItemEvent`
//! instead of `InventoryTransactionEvent`); Anchor raises the transaction for every request, so a
//! listener that guards every slot change also sees crafts and trades.

use super::super::super::{SessionId, State, dispatch, trade};
use super::super::{SlotRef, WindowKind};
use super::{Error, Made, Plan, Result};
use crate::event::{
    CraftItem, EnchantOption, Inventory, InventoryTransaction, PlayerDropItem, PlayerItemEnchant,
    SlotChange, Trade,
};
use crate::inventory::Kind;
use crate::item::Stack;

/// Whether anyone listens to or monitors a request event, so that the plan describes itself.
pub(super) fn heard(state: &State) -> bool {
    dispatch::heard::<InventoryTransaction>(state)
        || dispatch::heard::<CraftItem>(state)
        || dispatch::heard::<PlayerItemEnchant>(state)
        || dispatch::heard::<Trade>(state)
        || dispatch::heard::<PlayerDropItem>(state)
}

/// The events of one request, those heard.
#[derive(Debug)]
pub(super) struct Raised {
    transaction: Option<InventoryTransaction>,
    craft: Option<CraftItem>,
    enchant: Option<PlayerItemEnchant>,
    trade: Option<Trade>,
    drop: Option<PlayerDropItem>,
}

impl Raised {
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
        let (craft, enchant, trade) = made(state, session, plan, window);
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
            craft,
            enchant,
            trade,
            drop,
        }
    }
}

/// The heard events of what `plan` makes.
fn made(
    state: &State,
    session: SessionId,
    plan: &Plan,
    window: Option<WindowKind>,
) -> (Option<CraftItem>, Option<PlayerItemEnchant>, Option<Trade>) {
    let player = crate::player::Id::new(session);
    let (mut craft, mut enchant, mut traded) = (None, None, None);
    match &plan.made {
        Some(Made::Craft {
            inputs,
            outputs,
            times,
        }) if dispatch::heard::<CraftItem>(state) => {
            craft = Some(CraftItem {
                player,
                inputs: inputs.clone(),
                outputs: outputs.clone(),
                repetitions: *times,
                cancelled: false,
            });
        }
        Some(Made::Enchant {
            choice,
            input,
            output,
            cost,
        }) if dispatch::heard::<PlayerItemEnchant>(state) => {
            if let Some(WindowKind::Station { world, pos, .. }) = window {
                enchant = Some(PlayerItemEnchant {
                    player,
                    world: crate::world::Id::of(world),
                    pos,
                    option: EnchantOption {
                        required_level: choice.level,
                        name: choice.name.clone(),
                        enchantments: choice.enchantments.clone(),
                    },
                    input_item: input.clone(),
                    output_item: output.clone(),
                    cost: *cost,
                    cancelled: false,
                });
            }
        }
        Some(Made::Trade {
            net_id,
            inputs,
            output,
            times,
        }) if dispatch::heard::<Trade>(state) => {
            if let Some((villager, offer)) = trade::trading(state, session, *net_id) {
                traded = Some(Trade {
                    player,
                    villager,
                    offer,
                    inputs: inputs.clone(),
                    output: output.clone(),
                    uses: *times,
                    cancelled: false,
                });
            }
        }
        _ => {}
    }
    (craft, enchant, traded)
}

impl Raised {
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
        one(state, self.craft.as_mut(), |e| e.cancelled)?;
        one(state, self.enchant.as_mut(), |e| e.cancelled)?;
        one(state, self.trade.as_mut(), |e| e.cancelled)?;
        one(state, self.drop.as_mut(), |e| e.cancelled)
    }

    /// Whether a listener (not only a monitor) of a raised event ran, so the game may have
    /// changed.
    pub(super) fn listened(&self, state: &State) -> bool {
        (self.transaction.is_some() && dispatch::listened::<InventoryTransaction>(state))
            || (self.craft.is_some() && dispatch::listened::<CraftItem>(state))
            || (self.enchant.is_some() && dispatch::listened::<PlayerItemEnchant>(state))
            || (self.trade.is_some() && dispatch::listened::<Trade>(state))
            || (self.drop.is_some() && dispatch::listened::<PlayerDropItem>(state))
    }

    /// Calls the monitors of each raised event, after the request applied.
    pub(super) fn watch(self, state: &mut State) {
        let Self {
            transaction,
            craft,
            enchant,
            trade,
            drop,
        } = self;
        if let Some(e) = transaction {
            dispatch::watch(state, &e);
        }
        if let Some(e) = craft {
            dispatch::watch(state, &e);
        }
        if let Some(e) = enchant {
            dispatch::watch(state, &e);
        }
        if let Some(e) = trade {
            dispatch::watch(state, &e);
        }
        if let Some(e) = drop {
            dispatch::watch(state, &e);
        }
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
pub(super) fn named(plan: &Plan, inv: &super::super::Inventory) -> Vec<(Inventory, u8)> {
    let window = inv.window.as_ref().map(|w| w.kind);
    plan.touched
        .iter()
        .filter_map(|&(container, slot)| place(inv.locate(container, slot)?, window))
        .collect()
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
        slot,
        before: before.clone(),
        after: after.clone(),
    })
}

/// Where a request slot lives, as events name it.
fn place(r: SlotRef, window: Option<WindowKind>) -> Option<(Inventory, u8)> {
    Some(match r {
        SlotRef::Main(i) => (Inventory::Player(Kind::Main), i),
        SlotRef::Armor(i) => (Inventory::Player(Kind::Armor), i),
        SlotRef::Offhand => (Inventory::Player(Kind::Offhand), 0),
        SlotRef::Cursor => (Inventory::Player(Kind::Cursor), 0),
        SlotRef::Grid(i) => (Inventory::Player(Kind::Crafting), i),
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
