//! Block events: blocks broken and placed by players, chests paired and signs written.
//!
//! A block event names its block by world, position and state. Other blocks it mentions are still
//! in the world while the listeners run, so the event gives their positions only.

use crate::block::State;
use crate::block_entity::Side;
use crate::item::Stack;
use crate::player;
use crate::world::{self, BlockPos};

/// Raised when a player breaks a block, before it goes. Cancelling keeps the block.
#[doc(alias = "BlockBreakEvent")]
#[derive(Debug)]
pub struct BlockBreak {
    pub(crate) player: player::Id,
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) item: Stack,
    pub(crate) insta_break: bool,
    pub(crate) drops: Vec<Stack>,
    pub(crate) cancelled: bool,
}

getters!(BlockBreak {
    /// The player breaking the block.
    #[doc(alias = "BlockBreakEvent::getPlayer")]
    player: player::Id,
    /// The world of the block.
    world: world::Id,
    /// Where the block is.
    pos: BlockPos,
    /// The block being broken. A block state Anchor does not know raises no event, neither this
    /// nor a left click's [`PlayerInteract`](crate::event::PlayerInteract); a known state
    /// without a block type of its own comes as itself, under its own name.
    #[doc(alias = "BlockEvent::getBlock")]
    block: State,
    /// What the player breaks it with (empty for a bare hand).
    #[doc(alias = "BlockBreakEvent::getItem")]
    item: &Stack,
    /// Whether the break skipped the break time check: true for creative players, breaks the
    /// API asks for and servers with `break-time-check = false`. A player's break that has not
    /// lasted its time is refused before the event, so no listener can let it through.
    #[doc(alias = "BlockBreakEvent::getInstaBreak")]
    insta_break: bool,
    /// What the block drops. Starts as the block's own item: nothing for a block without an item
    /// form. A custom block starts with what its definition drops for the tool, and a player with
    /// unlimited resources (creative) starts with nothing.
    #[doc(alias = "BlockBreakEvent::getDrops")]
    drops: &[Stack],
});

setters!(BlockBreak {
    /// Replaces what the block drops.
    #[doc(alias = "BlockBreakEvent::setDrops")]
    #[doc(alias = "BlockBreakEvent::setDropsVariadic")]
    set_drops => drops: Vec<Stack>,
});

cancellable! {
    BlockBreak
        #[doc(alias = "BlockBreakEvent::cancel")]
        #[doc(alias = "BlockBreakEvent::uncancel")]
        #[doc(alias = "BlockBreakEvent::isCancelled")];
}

/// Raised when a player places a block, before it is set. Cancelling keeps the world as it was
/// and the item in the player's hand.
#[doc(alias = "BlockPlaceEvent")]
#[derive(Debug)]
pub struct BlockPlace {
    pub(crate) player: player::Id,
    pub(crate) world: world::Id,
    pub(crate) blocks: Vec<(BlockPos, State)>,
    pub(crate) block_against: BlockPos,
    pub(crate) item: Stack,
    pub(crate) cancelled: bool,
}

getters!(BlockPlace {
    /// The player placing the block.
    #[doc(alias = "BlockPlaceEvent::getPlayer")]
    player: player::Id,
    /// The world of the blocks.
    world: world::Id,
    /// Every block the placement sets: one, or more for doors, beds and tall plants.
    #[doc(alias = "BlockPlaceEvent::getTransaction")]
    blocks: &[(BlockPos, State)],
    /// The block the player clicked to place against.
    #[doc(alias = "BlockPlaceEvent::getBlockAgainst")]
    block_against: BlockPos,
    /// The item the player places.
    #[doc(alias = "BlockPlaceEvent::getItem")]
    item: &Stack,
});

cancellable! {
    BlockPlace
        #[doc(alias = "BlockPlaceEvent::cancel")]
        #[doc(alias = "BlockPlaceEvent::uncancel")]
        #[doc(alias = "BlockPlaceEvent::isCancelled")];
}

/// Raised when a chest that gets its block entity (placed, set by code, or opened without one)
/// joins the chest next to it into a double chest. Cancelling keeps them single.
#[doc(alias = "ChestPairEvent")]
#[derive(Debug)]
pub struct ChestPair {
    pub(crate) world: world::Id,
    pub(crate) left: BlockPos,
    pub(crate) right: BlockPos,
    pub(crate) cancelled: bool,
}

getters!(ChestPair {
    /// The world of the chests.
    world: world::Id,
    /// The left half, as seen facing the chest.
    #[doc(alias = "ChestPairEvent::getLeft")]
    left: BlockPos,
    /// The right half.
    #[doc(alias = "ChestPairEvent::getRight")]
    right: BlockPos,
});

cancellable! {
    ChestPair
        #[doc(alias = "ChestPairEvent::cancel")]
        #[doc(alias = "ChestPairEvent::uncancel")]
        #[doc(alias = "ChestPairEvent::isCancelled")];
}

/// Raised when a player writes on a sign. Cancelling keeps the old text.
#[doc(alias = "SignChangeEvent")]
#[derive(Debug)]
pub struct SignChange {
    pub(crate) player: player::Id,
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) old_text: [String; 4],
    pub(crate) new_text: [String; 4],
    pub(crate) side: Side,
    pub(crate) cancelled: bool,
}

getters!(SignChange {
    /// The player writing.
    #[doc(alias = "SignChangeEvent::getPlayer")]
    player: player::Id,
    /// The world of the sign.
    world: world::Id,
    /// Where the sign is.
    #[doc(alias = "SignChangeEvent::getSign")]
    pos: BlockPos,
    /// The sign.
    block: State,
    /// The four lines of the side before.
    #[doc(alias = "SignChangeEvent::getOldText")]
    old_text: &[String; 4],
    /// The four lines of the side after. The player's lines come without private-use characters
    /// and terminal escapes (colour codes stay); a listener's go on as it set them.
    #[doc(alias = "SignChangeEvent::getNewText")]
    new_text: &[String; 4],
    /// The side written on.
    #[doc(alias = "SignChangeEvent::isFrontFace")]
    side: Side,
});

setters!(SignChange {
    /// Replaces the lines written, which go on as they are (only the player's text is cleaned).
    /// They are checked again (no newline, at most
    /// [`MAX_SIGN_TEXT_BYTES`](crate::block_entity::MAX_SIGN_TEXT_BYTES) bytes in all); text that
    /// fails cancels the change.
    #[doc(alias = "SignChangeEvent::setNewText")]
    set_new_text => new_text: [String; 4],
});

cancellable! {
    SignChange
        #[doc(alias = "SignChangeEvent::cancel")]
        #[doc(alias = "SignChangeEvent::uncancel")]
        #[doc(alias = "SignChangeEvent::isCancelled")];
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::block::{DIRT, GRASS_BLOCK};
    use crate::event::fixtures::{POS, player, sticks};

    const WORLD: world::Id = world::Id::DEFAULT;
    const OTHER: BlockPos = BlockPos::new(5, 6, 7);

    // api: BlockBreakEvent, BlockBreakEvent::getPlayer, BlockEvent::getBlock,
    // api: BlockBreakEvent::getItem, BlockBreakEvent::getInstaBreak, BlockBreakEvent::getDrops,
    // api: BlockBreakEvent::setDrops, BlockBreakEvent::setDropsVariadic,
    // api: BlockBreakEvent::cancel, BlockBreakEvent::uncancel, BlockBreakEvent::isCancelled
    #[test]
    fn block_break_reads_and_changes_its_fields() {
        let mut e = BlockBreak {
            player: player(1),
            world: WORLD,
            pos: POS,
            block: DIRT,
            item: sticks(1),
            insta_break: false,
            drops: vec![sticks(2)],
            cancelled: false,
        };
        assert_eq!(
            (e.player(), e.world(), e.pos(), e.block()),
            (player(1), WORLD, POS, DIRT)
        );
        assert_eq!(e.item(), &sticks(1));
        assert!(!e.insta_break());
        assert_eq!(e.drops(), &[sticks(2)]);
        e.set_drops(Vec::new());
        assert!(e.drops().is_empty());
        assert!(!e.is_cancelled());
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: BlockPlaceEvent, BlockPlaceEvent::getPlayer, BlockPlaceEvent::getTransaction,
    // api: BlockPlaceEvent::getBlockAgainst, BlockPlaceEvent::getItem, BlockPlaceEvent::cancel,
    // api: BlockPlaceEvent::uncancel, BlockPlaceEvent::isCancelled
    #[test]
    fn block_place_lists_every_block_it_sets() {
        let mut e = BlockPlace {
            player: player(1),
            world: WORLD,
            blocks: vec![(POS, DIRT), (OTHER, GRASS_BLOCK)],
            block_against: OTHER,
            item: sticks(1),
            cancelled: false,
        };
        assert_eq!((e.player(), e.world()), (player(1), WORLD));
        assert_eq!(e.blocks(), &[(POS, DIRT), (OTHER, GRASS_BLOCK)]);
        assert_eq!(e.block_against(), OTHER);
        assert_eq!(e.item(), &sticks(1));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: ChestPairEvent, ChestPairEvent::getLeft, ChestPairEvent::getRight,
    // api: ChestPairEvent::cancel, ChestPairEvent::uncancel, ChestPairEvent::isCancelled
    #[test]
    fn chest_pair_names_both_halves() {
        let mut e = ChestPair {
            world: WORLD,
            left: POS,
            right: OTHER,
            cancelled: false,
        };
        assert_eq!((e.world(), e.left(), e.right()), (WORLD, POS, OTHER));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: SignChangeEvent, SignChangeEvent::getPlayer, SignChangeEvent::getSign,
    // api: SignChangeEvent::getOldText, SignChangeEvent::getNewText,
    // api: SignChangeEvent::isFrontFace, SignChangeEvent::setNewText, SignChangeEvent::cancel,
    // api: SignChangeEvent::uncancel, SignChangeEvent::isCancelled
    #[test]
    fn sign_change_names_its_side_and_its_text_can_change() {
        let lines = |s: &str| [s.to_owned(), String::new(), String::new(), String::new()];
        let mut e = SignChange {
            player: player(1),
            world: WORLD,
            pos: POS,
            block: DIRT,
            old_text: lines("old"),
            new_text: lines("new"),
            side: Side::Back,
            cancelled: false,
        };
        assert_eq!(
            (e.player(), e.world(), e.pos(), e.block()),
            (player(1), WORLD, POS, DIRT)
        );
        assert_eq!((e.old_text(), e.new_text()), (&lines("old"), &lines("new")));
        assert_eq!(e.side(), Side::Back);
        e.set_new_text(lines("other"));
        assert_eq!(e.new_text(), &lines("other"));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }
}
