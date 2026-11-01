//! Menu windows and fake blocks: container screens whose items stay the server's, and blocks one
//! player sees in place of the world's.
//!
//! A [`Menu`] looks like a chest, a double chest or a hopper with a title. Open it with
//! [`Context::open_menu`](crate::Context::open_menu); the player cannot take, place or drop the
//! items of its slots: every click is refused, the player's screen is put back as it was, and the
//! handler hears of it with a [`Click`]. The handler answers whether the menu stays open
//! ([`Response`]) and may change the menu's items with
//! [`Context::set_menu_item`](crate::Context::set_menu_item) or give the player items through
//! its inventory.
//!
//! Slots made [`editable`](Menu::editable) work as a chest's instead: the player takes, places and
//! swaps their items with its own slots and cursor, drops them and, in creative mode, destroys
//! them, the requests checked as in a chest window. Before a request that changes a menu slot is
//! made, the handler hears of its changes with a [`Change`] and answers whether it is made
//! ([`Response`]); a refused one changes nothing. The menu's items are a copy of what the handler
//! keeps: no change reaches them unless the handler kept it, so the handler holds every item the
//! menu holds, and closing the menu drops the copy. The cursor and the player's slots are the
//! player's: closing gives the cursor back as any window does, once the handler has heard of the
//! close with a [`Close`] and has not taken the cursor's stack into what it keeps.
//!
//! A menu that [lends](Menu::lend) leaves the items the player takes from its editable slots onto
//! the cursor in what the handler keeps until they are put down: they are lent, the handler's and
//! not the player's, so the player's saved data leaves them out and closing gives back only the
//! cursor's other items. Each change tells how many of the cursor's items are lent before and
//! after it ([`Change::lent_before`]), and so the handler keeps what it holds as it is when they
//! are taken up and moves them out when they leave the cursor elsewhere than into the menu.
//!
//! ```
//! use anchor::Context;
//! use anchor::item::Stack;
//! use anchor::menu::{Action, Input, Kind, Menu, Response};
//! use anchor::player;
//!
//! fn open_shop(ctx: &mut Context<'_>, player: player::Id, sword: Stack) {
//!     let menu = Menu::new(Kind::Chest).with_title("Shop").item(13, sword);
//!     let opened = ctx.open_menu(player, menu, |_, _, input| match input {
//!         Input::Click(click) if click.slot() == 13 && click.action() == Action::Take => {
//!             // Give the sword through the player's inventory here.
//!             Response::Close
//!         }
//!         _ => Response::Keep,
//!     });
//!     let _ = opened;
//! }
//! ```
//!
//! A menu stands on fake container blocks near the player, as the client needs a container block
//! to show the screen: [`Context::send_fake_block`](crate::Context::send_fake_block) shows any
//! block to one player the same way. The server keeps playing by the real blocks.

use std::fmt;

use crate::event::{Inventory, SlotChange};
use crate::inventory::{CHEST_SLOTS, HOPPER_SLOTS};
use crate::item::Stack;

/// The longest menu title, in bytes.
pub(crate) const MAX_TITLE_BYTES: usize = 1024;

// Every slot of the largest menu has its bit.
const _: () = assert!(Kind::DoubleChest.size() <= 64);

/// What a menu looks like and how many slots it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A chest: 27 slots.
    Chest,
    /// A double chest: 54 slots.
    DoubleChest,
    /// A hopper: 5 slots.
    Hopper,
}

impl Kind {
    /// How many slots the menu has.
    pub const fn size(self) -> usize {
        match self {
            Self::Chest => CHEST_SLOTS,
            Self::DoubleChest => 2 * CHEST_SLOTS,
            Self::Hopper => HOPPER_SLOTS,
        }
    }
}

/// A menu to open for a player: its kind, title, items and editable slots.
#[derive(Debug, Clone, PartialEq)]
pub struct Menu {
    kind: Kind,
    title: String,
    items: Vec<Stack>,
    /// The editable slots, bit `i` for slot `i`.
    editable: u64,
    /// Whether it [lends](Self::lend) the items taken onto the cursor.
    lends: bool,
    /// The first slot an [`item`](Self::item) or [`editable`](Self::editable) call named past
    /// the end, reported on open.
    bad_slot: Option<usize>,
}

impl Menu {
    /// An empty menu of `kind` without a title (the client shows the container's name).
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            title: String::new(),
            items: vec![Stack::EMPTY; kind.size()],
            editable: 0,
            lends: false,
            bad_slot: None,
        }
    }

    /// The menu with `title` shown above its slots (at most 1,024 bytes, checked on open).
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// The menu with `slot` editable: the player moves items into and out of it as in a chest,
    /// and the handler hears of each change before it is made ([`Change`]). A slot past the end
    /// makes the open fail ([`Error::NoSlot`]).
    #[must_use]
    pub fn editable(mut self, slot: usize) -> Self {
        let bit = slot_bit(slot).filter(|_| slot < self.kind.size());
        match bit {
            Some(bit) => self.editable |= bit,
            None => {
                self.bad_slot.get_or_insert(slot);
            }
        }
        self
    }

    /// The menu with `stack` in `slot`. A slot past the end makes the open fail
    /// ([`Error::NoSlot`]).
    #[must_use]
    pub fn item(mut self, slot: usize, stack: Stack) -> Self {
        match self.items.get_mut(slot) {
            Some(s) => *s = stack,
            None => {
                self.bad_slot.get_or_insert(slot);
            }
        }
        self
    }

    /// The menu lending the items the player takes from its editable slots onto its cursor: they
    /// stay in what the handler keeps, which holds the menu's items and the lent ones, until they
    /// leave the cursor. Items going from the cursor into a menu slot are the lent ones first, and
    /// items going anywhere else (the player's slots, a drop, a creative destroy) are the player's
    /// own first. A swap of the cursor with a menu slot lends the slot's stack, unless the cursor
    /// held only the player's own items: then, as in a container, they go into the slot and the
    /// slot's stack is the player's. A swap with another slot lends none. A write of the cursor
    /// through the inventory API keeps lent as many of them as the new stack holds of their
    /// item, the player's own going first.
    ///
    /// The cursor's stack, as the inventory API reads it, holds the lent items
    /// ([`Context::menu_lent`] says how many), but the player's saved data does not: a commit
    /// holds them in what the handler keeps only. A handler that keeps them in the players' file
    /// (another player's data) commits with the player, so a crash neither loses nor doubles
    /// them; in another file (the server's data, a block's items), a crash between the two
    /// files' commits is a mismatch the next start reports (`.claude/docs/storage.md`). On a
    /// close, they stay the handler's ([`Close::lent`]), whether it hears of the close or not.
    ///
    /// [`Context::menu_lent`]: crate::Context::menu_lent
    #[must_use]
    pub fn lend(mut self) -> Self {
        self.lends = true;
        self
    }

    /// The menu's items, one per slot.
    pub fn items(&self) -> &[Stack] {
        &self.items
    }

    /// What the menu looks like.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Checks the menu before it opens and gives its parts: the kind, the title, the items, the
    /// editable slots and whether it lends.
    pub(crate) fn prepare(self) -> Result<(Kind, String, Vec<Stack>, u64, bool)> {
        if self.bad_slot.is_some() {
            return Err(Error::NoSlot);
        }
        if self.title.len() > MAX_TITLE_BYTES {
            return Err(Error::TitleTooLong);
        }
        Ok((self.kind, self.title, self.items, self.editable, self.lends))
    }
}

/// An open menu, as [`Context::open_menu`](crate::Context::open_menu) gives it. IDs count per
/// player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(u32);

impl Id {
    pub(crate) const fn new(n: u32) -> Self {
        Self(n)
    }

    pub(crate) const fn get(self) -> u32 {
        self.0
    }
}

/// What the player tried to do with a menu slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Take items from the slot (onto the cursor or into the inventory).
    Take,
    /// Place items into the slot.
    Place,
    /// Swap the slot with another.
    Swap,
    /// Drop items from the slot.
    Drop,
}

/// What a menu's handler hears of.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// A click on a slot that is not editable.
    Click(Click),
    /// A change of editable slots, not made yet.
    Change(Change),
    /// The menu closed, for any reason but a direct call
    /// ([`Context::close_window`](crate::Context::close_window), or
    /// [`Context::open_menu`](crate::Context::open_menu) opening another menu in its place). A
    /// menu that closes while its handler runs, through the handler's own calls (an action that
    /// chats or breaks a block), is not heard either.
    Close(Close),
}

/// A click on a menu slot that is not editable. Nothing moved: the menu and the player's slots
/// are as they were.
#[derive(Debug, Clone, PartialEq)]
pub struct Click {
    pub(crate) menu: Id,
    pub(crate) slot: usize,
    pub(crate) item: Stack,
    pub(crate) cursor: Stack,
    pub(crate) action: Action,
}

impl Click {
    /// What the player tried to do.
    pub fn action(&self) -> Action {
        self.action
    }

    /// The stack on the player's cursor, its lent items included ([`Context::menu_lent`]).
    ///
    /// [`Context::menu_lent`]: crate::Context::menu_lent
    pub fn cursor(&self) -> &Stack {
        &self.cursor
    }

    /// The stack in the slot.
    pub fn item(&self) -> &Stack {
        &self.item
    }

    /// The menu clicked.
    pub fn menu(&self) -> Id {
        self.menu
    }

    /// The menu slot clicked.
    pub fn slot(&self) -> usize {
        self.slot
    }
}

/// One item request's changes of a menu's editable slots and the player's slots, not made yet:
/// the menu's slots, the player's and its cursor hold what they held before while the handler
/// runs, and the request is made once it returns, unless it refuses. A drop from a menu slot
/// leaves fewer items in it; once made, they are thrown as from a chest, by the player. A
/// creative destroy leaves fewer items too, deleted once made.
///
/// Once the handler returns, the request is checked again and made only if it still does what
/// the handler heard. The handler's own calls can fail that check when they change what the
/// request reads: a slot it names, changed or not ([`is_slot_named`](Self::is_slot_named)), through
/// [`Context::set_menu_item`] or the player's inventory; the player's cursor, crafting grid or the
/// stacks it waits with for room; the menu itself, closed or replaced by another window, a
/// teleport or another world; or the world's room for the item a drop throws (new entities). The
/// change is then refused although the handler kept it, a warning is logged, and the handler is
/// not told. So a handler that changes none of these while it answers never has a kept change
/// refused: it does such work with [`Context::defer`] or answers [`Response::Close`].
///
/// A deferred call runs once the whole packet the request came in is handled, after the packet's
/// later requests, which may have changed the same slot (the handler hears of those too). So a
/// deferred call that writes a menu slot first checks that the slot still holds what the kept
/// change left there ([`Context::menu_items`]).
///
/// [`Context::set_menu_item`]: crate::Context::set_menu_item
/// [`Context::defer`]: crate::Context::defer
/// [`Context::menu_items`]: crate::Context::menu_items
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub(crate) menu: Id,
    pub(crate) changes: Vec<SlotChange>,
    pub(crate) named: Vec<(Inventory, u8)>,
    pub(crate) lent: [u8; 2],
}

impl Change {
    /// Every slot the request changed, as [`InventoryTransaction`] lists them: the menu's slots
    /// are [`Inventory::Window`].
    ///
    /// [`InventoryTransaction`]: crate::event::InventoryTransaction
    pub fn changes(&self) -> &[SlotChange] {
        &self.changes
    }

    /// Whether the request names slot `slot` of `inventory` (named as in
    /// [`changes`](Self::changes)): a slot the handler must leave as it is. A request may name a
    /// slot it leaves as it was (items taken out and put back), which `changes` does not list.
    pub fn is_slot_named(&self, inventory: Inventory, slot: u8) -> bool {
        self.named.contains(&(inventory, slot))
    }

    /// How many of the items on the player's cursor after the change are lent
    /// ([`lent_before`](Self::lent_before)).
    pub fn lent_after(&self) -> u8 {
        self.lent[1]
    }

    /// How many of the items on the player's cursor before the change are lent ([`Menu::lend`]);
    /// 0 for a menu that does not lend. What the handler keeps changes as the menu's slots do,
    /// plus the lent items after the change, less those before: none for items taken up onto the
    /// cursor or put back into the menu, and the items that leave the cursor for the player's
    /// slots or the world. The handler hears every request that changes the lent items, or the
    /// cursor while it holds lent items (the player's own items joining them), whether it names
    /// a menu slot or not, and may refuse it.
    pub fn lent_before(&self) -> u8 {
        self.lent[0]
    }

    /// The menu changed.
    pub fn menu(&self) -> Id {
        self.menu
    }
}

/// A menu that closed, as its handler hears it once its window is gone: the stack the player
/// held on its cursor is out of the player's slots, and goes back to the player as from any
/// window when the handler returns, unless it answers [`Response::Refuse`], taking the stack
/// into what it keeps. The stack's lent items ([`lent`](Self::lent)) are the handler's already
/// and never go back. Nothing the handler does meanwhile reaches that stack, so whatever it
/// answers, the stack is the player's or the handler's, once.
///
/// A leaving player's menu closes after the [`PlayerQuit`] listeners, and after its monitors
/// unless a listener's call closed it first (a teleport): the player is still in the game, and
/// what goes back to it is saved with it.
///
/// A direct call that closes the menu calls no handler: the cursor's lent items
/// ([`Context::menu_lent`]) stay the handler's, and its other items go back to the player. A caller
/// that keeps those takes them first, writing the cursor down to the lent items
/// ([`Player::inventory_mut`] of [`Kind::Cursor`]; a write keeps lent what the stack still holds
/// of their item, [`Menu::lend`]): taking the whole stack would take the lent items too, which
/// the handler keeps already.
///
/// [`Context::menu_lent`]: crate::Context::menu_lent
/// [`PlayerQuit`]: crate::event::PlayerQuit
/// [`Player::inventory_mut`]: crate::player::Player::inventory_mut
/// [`Kind::Cursor`]: crate::inventory::Kind::Cursor
#[derive(Debug, Clone, PartialEq)]
pub struct Close {
    pub(crate) menu: Id,
    pub(crate) cursor: Stack,
    pub(crate) lent: u8,
    pub(crate) by_player: bool,
}

impl Close {
    /// The stack on the player's cursor as the menu closed (empty if none).
    pub fn cursor(&self) -> &Stack {
        &self.cursor
    }

    /// Whether the player closed the menu itself; false for a close of the server's: its death,
    /// leaving, a teleport, going more than 8 blocks away, another window, its chat, its breaking
    /// a block, or the handler's own [`Response::Close`].
    pub fn is_by_player(&self) -> bool {
        self.by_player
    }

    /// How many of the cursor's items are lent ([`Menu::lend`]): still in what the handler keeps,
    /// they do not go back to the player. 0 for a menu that does not lend.
    pub fn lent(&self) -> u8 {
        self.lent
    }

    /// The menu closed.
    pub fn menu(&self) -> Id {
        self.menu
    }
}

/// What a handler decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Response {
    /// The menu stays open (a change is made). For a close, the cursor's stack but its lent items
    /// goes back to the player.
    #[default]
    Keep,
    /// The menu closes (a change is made first). For a close, the same as [`Keep`](Self::Keep).
    Close,
    /// A change is not made: every slot stays as it was and the player's screen is put back. For
    /// a click, the same as [`Keep`](Self::Keep). For a close, nothing of the cursor's stack goes
    /// back to the player: the handler took it.
    Refuse,
}

/// Why a menu or fake block call did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The player is not in the game, or (to open a menu) its client has no view of the world
    /// yet.
    Offline,
    /// The player cannot take this now: it waits with so many stacks for room that a window's
    /// items could not go back, or 4,096 fake block changes wait to be sent to it this tick.
    Busy,
    /// The player has 4,096 fake blocks.
    Full,
    /// The position is outside the world height, or a menu's blocks cannot stand within 8 blocks
    /// of the player's eyes.
    OutsideWorld,
    /// The menu has no such slot.
    NoSlot,
    /// The title is longer than 1,024 bytes.
    TitleTooLong,
    /// The player has no menu open.
    NotOpen,
    /// The player was given 2^32 menus already; menu IDs do not wrap.
    Exhausted,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Offline => "player not in the game",
            Self::Busy => "player busy",
            Self::Full => "too many fake blocks",
            Self::OutsideWorld => "position outside the world",
            Self::NoSlot => "no such menu slot",
            Self::TitleTooLong => "menu title too long",
            Self::NotOpen => "no menu open",
            Self::Exhausted => "menu IDs used up",
        })
    }
}

impl std::error::Error for Error {}

/// A result with the menu [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The bit of slot `slot` in a set of slots (bit `i` for slot `i`); `None` past bit 63.
pub(crate) fn slot_bit(slot: usize) -> Option<u64> {
    u32::try_from(slot).ok().and_then(|s| 1_u64.checked_shl(s))
}
