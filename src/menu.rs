//! Menu windows and fake blocks: container screens whose items stay the server's, and blocks one
//! player sees in place of the world's.
//!
//! A [`Menu`] looks like a chest, a double chest or a hopper with a title. Open it with
//! [`Context::open_menu`](crate::Context::open_menu); the player cannot take, place or drop its
//! items: every click is refused, the player's screen is put back as it was, and the click
//! handler hears of it with a [`Click`]. The handler answers whether the menu stays open
//! ([`Response`]) and may change the menu's items with
//! [`Context::set_menu_item`](crate::Context::set_menu_item) or give the player items through
//! its inventory. The player's own items never move into or out of a menu.
//!
//! ```
//! use anchor::Context;
//! use anchor::item::Stack;
//! use anchor::menu::{Action, Kind, Menu, Response};
//! use anchor::player;
//!
//! fn open_shop(cx: &mut Context<'_>, player: player::Id, sword: Stack) {
//!     let menu = Menu::new(Kind::Chest).title("Shop").item(13, sword);
//!     let opened = cx.open_menu(player, menu, |_, _, click| {
//!         if click.slot() == 13 && click.action() == Action::Take {
//!             // Give the sword through the player's inventory here.
//!             Response::Close
//!         } else {
//!             Response::Keep
//!         }
//!     });
//!     let _ = opened;
//! }
//! ```
//!
//! A menu stands on fake container blocks near the player, as the client needs a container block
//! to show the screen: [`Context::send_fake_block`](crate::Context::send_fake_block) shows any
//! block to one player the same way. The server keeps playing by the real blocks.

use std::fmt;

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

/// A menu to open for a player: its kind, title and items.
#[derive(Debug, Clone, PartialEq)]
pub struct Menu {
    kind: Kind,
    title: String,
    items: Vec<Stack>,
    /// The first slot an [`item`](Self::item) call named past the end, reported on open.
    bad_slot: Option<usize>,
}

impl Menu {
    /// An empty menu of `kind` without a title (the client shows the container's name).
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            title: String::new(),
            items: vec![Stack::EMPTY; kind.size()],
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

    /// What the menu looks like.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The menu's items, one per slot.
    pub fn items(&self) -> &[Stack] {
        &self.items
    }

    /// Checks the menu before it opens and gives its parts: the kind, the title and the items.
    pub(crate) fn prepare(self) -> Result<(Kind, String, Vec<Stack>)> {
        if self.bad_slot.is_some() {
            return Err(Error::NoSlot);
        }
        if self.title.len() > MAX_TITLE_BYTES {
            return Err(Error::TitleTooLong);
        }
        Ok((self.kind, self.title, self.items))
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

/// A click on a menu slot. Nothing moved: the menu and the player's slots are as they were.
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

/// What a click handler decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Response {
    /// The menu stays open, its items where they are.
    #[default]
    Keep,
    /// The menu closes.
    Close,
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
