//! Inventory events: windows, transactions and villager trades.

use crate::entity;
use crate::inventory::Kind;
use crate::item::Stack;
use crate::player;
use crate::world::{self, BlockPos};

/// Raised when a player's window other than a menu closes, a trade window included (the pair of
/// [`TradeOpen`]): the player closed it, left, died, was moved away or opened another window, or
/// a game rule closed it (its block went away, the villager went away).
#[doc(alias = "InventoryCloseEvent")]
#[derive(Debug)]
pub struct InventoryClose {
    pub(crate) player: player::Id,
    pub(crate) window: Window,
}

getters!(InventoryClose {
    /// The player.
    #[doc(alias = "InventoryCloseEvent::getPlayer")]
    player: player::Id,
    /// The window that closes.
    window: Window,
});

/// Raised when a player opens a window, other than a trade (that is [`TradeOpen`]). Cancelling
/// keeps it closed, and so does a listener that changes the player's open window (by opening a
/// menu, say) or the block so that the window no longer opens.
#[doc(alias = "InventoryOpenEvent")]
#[derive(Debug)]
pub struct InventoryOpen {
    pub(crate) player: player::Id,
    pub(crate) window: Window,
    pub(crate) cancelled: bool,
}

getters!(InventoryOpen {
    /// The player.
    #[doc(alias = "InventoryOpenEvent::getPlayer")]
    player: player::Id,
    /// The window that opens.
    #[doc(alias = "InventoryEvent::getInventory")]
    window: Window,
});

cancellable! {
    InventoryOpen
        #[doc(alias = "InventoryOpenEvent::cancel")]
        #[doc(alias = "InventoryOpenEvent::uncancel")]
        #[doc(alias = "InventoryOpenEvent::isCancelled")];
}

/// A window a player opens or closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Window {
    /// The player's own inventory.
    Own,
    /// The window of a block: a container or an ender chest.
    Block {
        /// The world of the block.
        world: world::Id,
        /// Where the block is.
        pos: BlockPos,
    },
    /// Trading with a villager NPC.
    Trade(entity::Id),
}

/// Raised when a player's item request has passed every check, before its slots change. A request
/// changes its slots all together or not at all; cancelling keeps them all.
#[doc(alias = "InventoryTransactionEvent")]
#[derive(Debug)]
// api-class: InventoryTransaction
pub struct InventoryTransaction {
    pub(crate) player: player::Id,
    pub(crate) changes: Vec<SlotChange>,
    pub(crate) cancelled: bool,
}

getters!(InventoryTransaction {
    /// The player.
    #[doc(alias = "InventoryTransaction::getSource")]
    player: player::Id,
    /// Every slot the request changes.
    #[doc(alias = "InventoryTransactionEvent::getTransaction")]
    #[doc(alias = "InventoryTransaction::getActions")]
    changes: &[SlotChange],
});

cancellable! {
    InventoryTransaction
        #[doc(alias = "InventoryTransactionEvent::cancel")]
        #[doc(alias = "InventoryTransactionEvent::uncancel")]
        #[doc(alias = "InventoryTransactionEvent::isCancelled")];
}

/// One slot a transaction changes.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotChange {
    pub(crate) inventory: Inventory,
    pub(crate) slot: u8,
    pub(crate) before: Stack,
    pub(crate) after: Stack,
}

getters!(SlotChange {
    /// Where the slot lives.
    #[doc(alias = "SlotChangeAction::getInventory")]
    inventory: Inventory,
    /// The slot in its inventory.
    #[doc(alias = "SlotChangeAction::getSlot")]
    slot: u8,
    /// What the slot held before.
    #[doc(alias = "InventoryAction::getSourceItem")]
    before: &Stack,
    /// What the slot holds after.
    #[doc(alias = "InventoryAction::getTargetItem")]
    after: &Stack,
});

/// Where a slot of a player's transaction lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Inventory {
    /// One of the player's own inventories (the inventory API's names).
    Player(Kind),
    /// The open container block (a double chest's right half from slot 27).
    Block {
        /// The world of the block.
        world: world::Id,
        /// Where the block is (a double chest's left half).
        pos: BlockPos,
    },
    /// The open window's own slots: the payment slots of a trade, a menu's editable slots
    /// ([`crate::menu`]).
    Window,
}

/// Raised when a player trades with a villager NPC, after the trade is checked and before the
/// items move. Cancelling keeps the items and the offer's uses.
#[derive(Debug)]
pub struct Trade {
    pub(crate) player: player::Id,
    pub(crate) villager: entity::Id,
    pub(crate) offer: usize,
    pub(crate) inputs: [Stack; 2],
    pub(crate) output: Stack,
    pub(crate) uses: u32,
    pub(crate) cancelled: bool,
}

getters!(Trade {
    /// The player.
    player: player::Id,
    /// The villager.
    villager: entity::Id,
    /// The offer's place in the villager's list, from 0.
    offer: usize,
    /// What one use of the offer costs: the first payment and the second (empty when the offer
    /// has no second payment).
    inputs: &[Stack; 2],
    /// What one use of the offer gives.
    output: &Stack,
    /// How many times the trade uses the offer at once, 1 to 256.
    uses: u32,
});

cancellable! {
    Trade;
}

/// Raised when a player opens a villager NPC's trade window. Cancelling keeps it closed.
#[derive(Debug)]
pub struct TradeOpen {
    pub(crate) player: player::Id,
    pub(crate) villager: entity::Id,
    pub(crate) cancelled: bool,
}

getters!(TradeOpen {
    /// The player.
    player: player::Id,
    /// The villager.
    villager: entity::Id,
});

cancellable! {
    TradeOpen;
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::event::fixtures::{POS, entity, player, sticks};

    const WORLD: world::Id = world::Id::DEFAULT;

    // api: InventoryOpenEvent, InventoryCloseEvent, InventoryOpenEvent::getPlayer,
    // api: InventoryEvent::getInventory, InventoryOpenEvent::cancel,
    // api: InventoryOpenEvent::uncancel, InventoryOpenEvent::isCancelled,
    // api: InventoryCloseEvent::getPlayer
    #[test]
    fn inventory_open_and_close_name_the_window() {
        let window = Window::Block {
            world: WORLD,
            pos: POS,
        };
        let mut open = InventoryOpen {
            player: player(1),
            window,
            cancelled: false,
        };
        assert_eq!((open.player(), open.window()), (player(1), window));
        open.cancel();
        assert!(open.is_cancelled());
        open.uncancel();
        assert!(!open.is_cancelled());
        let close = InventoryClose {
            player: player(1),
            window: Window::Trade(entity(1)),
        };
        assert_eq!(
            (close.player(), close.window()),
            (player(1), Window::Trade(entity(1)))
        );
    }

    // api: InventoryTransactionEvent, InventoryTransaction::getSource,
    // api: InventoryTransactionEvent::getTransaction, InventoryTransaction::getActions,
    // api: SlotChangeAction::getInventory, SlotChangeAction::getSlot,
    // api: InventoryAction::getSourceItem, InventoryAction::getTargetItem,
    // api: InventoryTransactionEvent::cancel, InventoryTransactionEvent::uncancel,
    // api: InventoryTransactionEvent::isCancelled, InventoryTransaction
    #[test]
    fn inventory_transaction_lists_every_slot_change() {
        let change = SlotChange {
            inventory: Inventory::Player(Kind::Main),
            slot: 4,
            before: sticks(2),
            after: Stack::EMPTY,
        };
        let mut e = InventoryTransaction {
            player: player(1),
            changes: vec![change.clone()],
            cancelled: false,
        };
        assert_eq!((e.player(), e.changes()), (player(1), [change].as_slice()));
        let change = e.changes().first().expect("one change");
        assert_eq!(
            (change.inventory(), change.slot()),
            (Inventory::Player(Kind::Main), 4)
        );
        assert_eq!(
            (change.before(), change.after()),
            (&sticks(2), &Stack::EMPTY)
        );
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    #[test]
    fn trade_events_name_the_villager_and_the_offer() {
        let mut open = TradeOpen {
            player: player(1),
            villager: entity(2),
            cancelled: false,
        };
        assert_eq!((open.player(), open.villager()), (player(1), entity(2)));
        open.cancel();
        assert!(open.is_cancelled());
        open.uncancel();
        assert!(!open.is_cancelled());
        let mut trade = Trade {
            player: player(1),
            villager: entity(2),
            offer: 1,
            inputs: [sticks(3), Stack::EMPTY],
            output: sticks(1),
            uses: 2,
            cancelled: false,
        };
        assert_eq!(
            (
                trade.player(),
                trade.villager(),
                trade.offer(),
                trade.uses()
            ),
            (player(1), entity(2), 1, 2)
        );
        assert_eq!(trade.inputs(), &[sticks(3), Stack::EMPTY]);
        assert_eq!(trade.output(), &sticks(1));
        trade.cancel();
        assert!(trade.is_cancelled());
        trade.uncancel();
        assert!(!trade.is_cancelled());
    }
}
