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
//! swaps their items with its own slots and cursor and drops them, the requests checked as in a
//! chest window.
//! Before a request that changes a menu slot is made, the handler hears of its changes with a
//! [`Change`] and answers whether it is made ([`Response`]); a refused one changes nothing. The
//! menu's items are a copy of what the handler keeps: no change reaches them unless the handler
//! kept it, so the handler holds every item the menu holds, and closing the menu drops the copy.
//! The cursor and the player's slots are the player's: closing gives the cursor back as any window
//! does, once the handler has heard of the close with a [`Close`] and has not taken the cursor's
//! stack into what it keeps.
//!
//! ```
//! use anchor::Context;
//! use anchor::item::Stack;
//! use anchor::menu::{Action, Input, Kind, Menu, Response};
//! use anchor::player;
//!
//! fn open_shop(cx: &mut Context<'_>, player: player::Id, sword: Stack) {
//!     let menu = Menu::new(Kind::Chest).title("Shop").item(13, sword);
//!     let opened = cx.open_menu(player, menu, |_, _, input| match input {
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
    /// The first slot an [`item`](Self::item) or [`editable`](Self::editable) call named past
    /// the end, reported on open.
    bad_slot: Option<usize>,
}

// Every slot of the largest menu has its bit.
const _: () = assert!(Kind::DoubleChest.size() <= 64);

/// Whether slot `slot` is editable in `editable` (bit `i` for slot `i`).
pub(crate) fn is_editable(editable: u64, slot: usize) -> bool {
    u32::try_from(slot)
        .ok()
        .and_then(|s| 1u64.checked_shl(s))
        .is_some_and(|bit| editable & bit != 0)
}

impl Menu {
    /// An empty menu of `kind` without a title (the client shows the container's name).
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            title: String::new(),
            items: vec![Stack::EMPTY; kind.size()],
            editable: 0,
            bad_slot: None,
        }
    }

    /// The menu with `title` shown above its slots (at most 1,024 bytes, checked on open).
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
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

    /// The menu with `slot` editable: the player moves items into and out of it as in a chest,
    /// and the handler hears of each change before it is made ([`Change`]). A slot past the end
    /// makes the open fail ([`Error::NoSlot`]).
    #[must_use]
    pub fn editable(mut self, slot: usize) -> Self {
        let bit = u32::try_from(slot)
            .ok()
            .and_then(|s| 1u64.checked_shl(s))
            .filter(|_| slot < self.kind.size());
        match bit {
            Some(bit) => self.editable |= bit,
            None => {
                self.bad_slot.get_or_insert(slot);
            }
        }
        self
    }

    /// What the menu looks like.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The menu's items, one per slot.
    pub fn items(&self) -> &[Stack] {
        &self.items
    }

    /// Checks the menu before it opens and gives its parts: the kind, the title, the items and
    /// the editable slots.
    pub(crate) fn prepare(self) -> Result<(Kind, String, Vec<Stack>, u64)> {
        if self.bad_slot.is_some() {
            return Err(Error::NoSlot);
        }
        if self.title.len() > MAX_TITLE_BYTES {
            return Err(Error::TitleTooLong);
        }
        Ok((self.kind, self.title, self.items, self.editable))
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
    /// [`Context::open_menu`](crate::Context::open_menu) opening another menu in its place).
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
    /// The menu clicked.
    pub fn menu(&self) -> Id {
        self.menu
    }

    /// The menu slot clicked.
    pub fn slot(&self) -> usize {
        self.slot
    }

    /// The stack in the slot.
    pub fn item(&self) -> &Stack {
        &self.item
    }

    /// The stack on the player's cursor.
    pub fn cursor(&self) -> &Stack {
        &self.cursor
    }

    /// What the player tried to do.
    pub fn action(&self) -> Action {
        self.action
    }
}

/// One item request's changes of a menu's editable slots and the player's slots, not made yet:
/// the menu's slots, the player's and its cursor hold what they held before while the handler
/// runs, and the request is made once it returns, unless it refuses. A drop from a menu slot
/// leaves fewer items in it; once made, they are thrown as from a chest, by the player.
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
}

impl Change {
    /// The menu changed.
    pub fn menu(&self) -> Id {
        self.menu
    }

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
}

/// A menu that closed, as its handler hears it once its window is gone: the stack the player
/// held on its cursor is out of the player's slots, and goes back to the player as from any
/// window when the handler returns, unless it answers [`Response::Refuse`], taking the stack
/// into what it keeps. Nothing the handler does meanwhile reaches that stack, so whatever it
/// answers, the stack is the player's or the handler's, once.
///
/// A direct call that closes the menu calls no handler: its caller takes the cursor's stack first
/// if it keeps it ([`Player::inventory_mut`] of [`Kind::Cursor`]).
///
/// [`Player::inventory_mut`]: crate::player::Player::inventory_mut
/// [`Kind::Cursor`]: crate::inventory::Kind::Cursor
#[derive(Debug, Clone, PartialEq)]
pub struct Close {
    pub(crate) menu: Id,
    pub(crate) cursor: Stack,
    pub(crate) by_player: bool,
}

impl Close {
    /// The menu closed.
    pub fn menu(&self) -> Id {
        self.menu
    }

    /// The stack on the player's cursor as the menu closed (empty if none).
    pub fn cursor(&self) -> &Stack {
        &self.cursor
    }

    /// Whether the player closed the menu itself; false for a close of the server's: its death,
    /// leaving, a teleport, going more than 8 blocks away, another window, or the handler's own
    /// [`Response::Close`].
    pub fn is_by_player(&self) -> bool {
        self.by_player
    }
}

/// What a handler decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Response {
    /// The menu stays open (a change is made). For a close, the cursor's stack goes back to the
    /// player.
    #[default]
    Keep,
    /// The menu closes (a change is made first). For a close, the same as [`Keep`](Self::Keep).
    Close,
    /// A change is not made: every slot stays as it was and the player's screen is put back. For
    /// a click, the same as [`Keep`](Self::Keep). For a close, the cursor's stack does not go back
    /// to the player: the handler took it.
    Refuse,
}

/// Why a menu or fake block call did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The player is not in the game, or its client has no view of the world yet.
    Offline,
    /// The player cannot take this now: it waits with stacks for room in its inventory, or
    /// 4,096 fake block changes wait to be sent to it this tick.
    Busy,
    /// The player has 4,096 fake blocks.
    Full,
    /// The position is outside the world height.
    OutsideWorld,
    /// The menu has no such slot.
    NoSlot,
    /// The title is longer than 1,024 bytes.
    TitleTooLong,
    /// The player has no menu open.
    NotOpen,
    /// The player was given `u32::MAX` menus already; menu IDs do not wrap.
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

/// Menu and fake block results.
pub type Result<T> = std::result::Result<T, Error>;
