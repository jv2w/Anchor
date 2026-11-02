//! Inventory events (PMMP `pocketmine\event\inventory`): windows, transactions, crafting and the
//! furnace, and the two trade events PMMP does not have.

use super::MAX_SAVED_FUEL_TIME;
use crate::block::State;
use crate::entity;
use crate::inventory::Kind;
use crate::item::Stack;
use crate::player;
use crate::world::{self, BlockPos};

/// Raised when a player crafts, before the inputs are used up. Cancelling keeps the inputs and
/// makes nothing.
#[doc(alias = "CraftItemEvent")]
#[derive(Debug)]
pub struct CraftItem {
    pub(crate) player: player::Id,
    pub(crate) inputs: Vec<Stack>,
    pub(crate) outputs: Vec<Stack>,
    pub(crate) repetitions: u32,
    pub(crate) cancelled: bool,
}

getters!(CraftItem {
    /// The player crafting.
    #[doc(alias = "CraftItemEvent::getPlayer")]
    player: player::Id,
    /// What one craft uses up.
    #[doc(alias = "CraftItemEvent::getInputs")]
    inputs: &[Stack],
    /// What one craft makes.
    #[doc(alias = "CraftItemEvent::getOutputs")]
    outputs: &[Stack],
    /// How many times the recipe is made at once.
    #[doc(alias = "CraftItemEvent::getRepetitions")]
    repetitions: u32,
});

cancellable! {
    CraftItem
        #[doc(alias = "CraftItemEvent::cancel")]
        #[doc(alias = "CraftItemEvent::uncancel")]
        #[doc(alias = "CraftItemEvent::isCancelled")];
}

/// Raised when a furnace, blast furnace or smoker burns a new fuel item. Cancelling keeps the fuel
/// and the furnace stays out.
#[doc(alias = "FurnaceBurnEvent")]
#[derive(Debug)]
pub struct FurnaceBurn {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) fuel: Stack,
    pub(crate) burn_time_ticks: u32,
    pub(crate) burning: bool,
    pub(crate) cancelled: bool,
}

getters!(FurnaceBurn {
    /// The world of the furnace.
    world: world::Id,
    /// Where the furnace is.
    #[doc(alias = "FurnaceBurnEvent::getFurnace")]
    pos: BlockPos,
    /// The furnace.
    block: State,
    /// The fuel.
    #[doc(alias = "FurnaceBurnEvent::getFuel")]
    fuel: &Stack,
    /// How long the fuel burns, in ticks.
    #[doc(alias = "FurnaceBurnEvent::getBurnTime")]
    burn_time_ticks: u32,
});

impl FurnaceBurn {
    /// Whether the fuel item is used up. Starts true.
    #[doc(alias = "FurnaceBurnEvent::isBurning")]
    pub fn is_burning(&self) -> bool {
        self.burning
    }

    /// Sets how long the fuel burns, in ticks, at most 32,767 (more counts as 32,767); 0 burns
    /// nothing and keeps the fuel, as if cancelled.
    #[doc(alias = "FurnaceBurnEvent::setBurnTime")]
    pub fn set_burn_time_ticks(&mut self, burn_time_ticks: u32) {
        self.burn_time_ticks = burn_time_ticks.min(MAX_SAVED_FUEL_TIME);
    }
}

setters!(FurnaceBurn {
    /// Sets whether the fuel item is used up (false: the furnace burns for free).
    #[doc(alias = "FurnaceBurnEvent::setBurning")]
    set_burning => burning: bool,
});

cancellable! {
    FurnaceBurn
        #[doc(alias = "FurnaceBurnEvent::cancel")]
        #[doc(alias = "FurnaceBurnEvent::uncancel")]
        #[doc(alias = "FurnaceBurnEvent::isCancelled")];
}

/// Raised when a furnace, blast furnace or smoker finishes smelting an item. Cancelling keeps the
/// item unsmelted.
#[doc(alias = "FurnaceSmeltEvent")]
#[derive(Debug)]
pub struct FurnaceSmelt {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) source: Stack,
    pub(crate) result: Stack,
    pub(crate) cancelled: bool,
}

getters!(FurnaceSmelt {
    /// The world of the furnace.
    world: world::Id,
    /// Where the furnace is.
    #[doc(alias = "FurnaceSmeltEvent::getFurnace")]
    pos: BlockPos,
    /// The furnace.
    block: State,
    /// The item smelted (one of the input stack).
    #[doc(alias = "FurnaceSmeltEvent::getSource")]
    source: &Stack,
    /// What goes to the output slot.
    #[doc(alias = "FurnaceSmeltEvent::getResult")]
    result: &Stack,
});

setters!(FurnaceSmelt {
    /// Replaces what goes to the output slot. A result that is empty or does not fit there
    /// cancels the smelt.
    #[doc(alias = "FurnaceSmeltEvent::setResult")]
    set_result => result: Stack,
});

cancellable! {
    FurnaceSmelt
        #[doc(alias = "FurnaceSmeltEvent::cancel")]
        #[doc(alias = "FurnaceSmeltEvent::uncancel")]
        #[doc(alias = "FurnaceSmeltEvent::isCancelled")];
}

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
    /// The window of a block: a container, a crafting table, a station, an ender chest.
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
// pmmp-class: InventoryTransaction
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
    /// The open window's own slots: a crafting table's grid, a station's inputs, the payment
    /// slots of a trade, a menu's editable slots ([`crate::menu`]).
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

    use crate::block::DIRT;
    use crate::event::fixtures::{POS, entity, player, sticks};

    const WORLD: world::Id = world::Id::DEFAULT;

    // pmmp: CraftItemEvent, CraftItemEvent::getPlayer, CraftItemEvent::getInputs,
    // pmmp: CraftItemEvent::getOutputs, CraftItemEvent::getRepetitions, CraftItemEvent::cancel,
    // pmmp: CraftItemEvent::uncancel, CraftItemEvent::isCancelled
    #[test]
    fn craft_item_lists_inputs_and_outputs() {
        let mut e = CraftItem {
            player: player(1),
            inputs: vec![sticks(2)],
            outputs: vec![sticks(4)],
            repetitions: 3,
            cancelled: false,
        };
        assert_eq!((e.player(), e.repetitions()), (player(1), 3));
        assert_eq!(
            (e.inputs(), e.outputs()),
            ([sticks(2)].as_slice(), [sticks(4)].as_slice())
        );
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: FurnaceBurnEvent, FurnaceBurnEvent::getFurnace, FurnaceBurnEvent::getFuel,
    // pmmp: FurnaceBurnEvent::getBurnTime, FurnaceBurnEvent::setBurnTime,
    // pmmp: FurnaceBurnEvent::isBurning, FurnaceBurnEvent::setBurning, FurnaceBurnEvent::cancel,
    // pmmp: FurnaceBurnEvent::uncancel, FurnaceBurnEvent::isCancelled
    #[test]
    fn furnace_burn_time_and_use_can_change() {
        let mut e = FurnaceBurn {
            world: WORLD,
            pos: POS,
            block: DIRT,
            fuel: sticks(1),
            burn_time_ticks: 100,
            burning: true,
            cancelled: false,
        };
        assert_eq!((e.world(), e.pos(), e.block()), (WORLD, POS, DIRT));
        assert_eq!((e.fuel(), e.burn_time_ticks()), (&sticks(1), 100));
        assert!(e.is_burning());
        e.set_burn_time_ticks(40);
        e.set_burning(false);
        assert_eq!(e.burn_time_ticks(), 40);
        assert!(!e.is_burning());
        e.set_burn_time_ticks(100_000);
        assert_eq!(
            e.burn_time_ticks(),
            32_767,
            "a furnace saves at most a short"
        );
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: FurnaceSmeltEvent, FurnaceSmeltEvent::getFurnace, FurnaceSmeltEvent::getSource,
    // pmmp: FurnaceSmeltEvent::getResult, FurnaceSmeltEvent::setResult,
    // pmmp: FurnaceSmeltEvent::cancel, FurnaceSmeltEvent::uncancel,
    // pmmp: FurnaceSmeltEvent::isCancelled
    #[test]
    fn furnace_smelt_result_can_change() {
        let mut e = FurnaceSmelt {
            world: WORLD,
            pos: POS,
            block: DIRT,
            source: sticks(1),
            result: sticks(2),
            cancelled: false,
        };
        assert_eq!((e.world(), e.pos(), e.block()), (WORLD, POS, DIRT));
        assert_eq!((e.source(), e.result()), (&sticks(1), &sticks(2)));
        e.set_result(sticks(3));
        assert_eq!(e.result(), &sticks(3));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: InventoryOpenEvent, InventoryCloseEvent, InventoryOpenEvent::getPlayer,
    // pmmp: InventoryEvent::getInventory, InventoryOpenEvent::cancel,
    // pmmp: InventoryOpenEvent::uncancel, InventoryOpenEvent::isCancelled,
    // pmmp: InventoryCloseEvent::getPlayer
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

    // pmmp: InventoryTransactionEvent, InventoryTransaction::getSource,
    // pmmp: InventoryTransactionEvent::getTransaction, InventoryTransaction::getActions,
    // pmmp: SlotChangeAction::getInventory, SlotChangeAction::getSlot,
    // pmmp: InventoryAction::getSourceItem, InventoryAction::getTargetItem,
    // pmmp: InventoryTransactionEvent::cancel, InventoryTransactionEvent::uncancel,
    // pmmp: InventoryTransactionEvent::isCancelled, InventoryTransaction
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
