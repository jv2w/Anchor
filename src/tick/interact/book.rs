//! Book editing (PMMP `InGamePacketHandler::handleBookEdit`, `WritableBookBase`, `WrittenBook`):
//! a player rewrites, inserts, deletes or swaps the pages of a book and quill in its inventory,
//! or signs it into a written book, through [`PlayerEditBook`].
//!
//! PMMP grows a book to whatever page a packet names before it checks the page count, so one
//! packet could make millions of pages; Anchor refuses an edit that would take the book past
//! [`MAX_PAGES`] before it builds anything. PMMP raises its event already cancelled for a text
//! over its limits or too many pages; Anchor refuses those without an event, as it does other
//! actions that cannot happen. The packet's strings are at most 768 bytes, below PMMP's hard
//! limits, so only the character limits remain.

use super::super::{Phase, SessionId, State, dispatch, inventory};
use super::sign::clean;
use crate::event::{BookAction, PlayerEditBook};
use crate::item::{self, Extra, Stack};
use crate::nbt::{Compound, List, Value};
use crate::protocol::consts::BookAction as Action;
use crate::protocol::packet::BookEdit;

/// The most pages a player may give a book (PMMP `handleBookEdit`; a book that has more, made by
/// code, may still be edited without growing).
const MAX_PAGES: usize = 50;
/// The most characters on a page (PMMP `PAGE_LENGTH_SOFT_LIMIT_CHARS`).
const MAX_PAGE_CHARS: usize = 512;
/// The most characters in a title, and in an author (PMMP `handleBookEdit`; vanilla has no
/// author limit, PMMP's is 256).
const MAX_TITLE_CHARS: usize = 16;
const MAX_AUTHOR_CHARS: usize = 256;
/// The NBT keys of a book (PMMP `WritableBookBase::TAG_*`, `WrittenBook::TAG_*`).
const PAGES: &str = "pages";
const TEXT: &str = "text";
const PHOTO: &str = "photoname";
const GENERATION: &str = "generation";
const AUTHOR: &str = "author";
const TITLE: &str = "title";

/// A page (PMMP `WritableBookPage`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Page {
    text: String,
    photo: String,
}

impl Page {
    /// A page of `text` without a photo.
    fn of(text: String) -> Self {
        Self {
            text,
            photo: String::new(),
        }
    }
}

/// `BookEdit` from the client (PMMP `handleBookEdit`): the edit of the book and quill in main
/// slot `inventory_slot`, which [`PlayerEditBook`] may change or cancel. Anything that does not
/// fit is ignored.
pub(in crate::tick) fn on_book_edit(state: &mut State, session: SessionId, packet: &BookEdit) {
    let Ok(slot) = u8::try_from(packet.inventory_slot) else {
        return;
    };
    let Some(old) = writable(state, session, slot).cloned() else {
        return;
    };
    let Some((action, new, modified)) = edited(&old, packet) else {
        tracing::debug!(session = session.get(), "book edit refused");
        return;
    };
    let mut new = new;
    let mut event = None;
    if dispatch::heard::<PlayerEditBook>(state) {
        let mut e = PlayerEditBook {
            player: crate::player::Id::new(session),
            old_book: old.clone(),
            new_book: new,
            action,
            modified_pages: modified,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        // A listener that took or changed the book cancels the edit.
        if e.cancelled
            || (dispatch::listened::<PlayerEditBook>(state)
                && writable(state, session, slot) != Some(&old))
        {
            return;
        }
        new = e.new_book.clone();
        event = Some(e);
    }
    if let Some(player) = state.players.get_mut(&session) {
        inventory::set_main(player, slot, new);
    }
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
}

/// The book and quill in main slot `slot` of a spawned, living `session`.
fn writable(state: &State, session: SessionId, slot: u8) -> Option<&Stack> {
    let player = state.players.get(&session)?;
    if player.phase != Phase::Spawned || !player.is_alive() {
        return None;
    }
    player
        .inventory
        .main_stack(slot)
        .filter(|s| s.item().is_some_and(|t| t.is(item::Class::WritableBook)))
}

/// What `packet` makes of the book `old`: the action, the new book and the pages it changed;
/// `None` for an edit PMMP refuses or cancels, or a book over the stack limits.
fn edited(old: &Stack, packet: &BookEdit) -> Option<(BookAction, Stack, Vec<u32>)> {
    let mut pages = pages(old);
    let count = pages.len();
    // The page count the edit may reach: PMMP lets it shrink or stay, and grow up to 50.
    let room = count.max(MAX_PAGES);
    let page = usize::try_from(packet.page_number).ok();
    let number = |p: usize| u32::try_from(p).ok();
    let (action, modified) = match Action::try_from(packet.action_type).ok()? {
        Action::ReplacePage => {
            let p = page.filter(|&p| p < room)?;
            let text = page_text(&packet.text)?;
            if p >= count {
                pages.resize(p + 1, Page::of(String::new()));
            }
            *pages.get_mut(p)? = Page::of(text);
            (BookAction::ReplacePage, vec![number(p)?])
        }
        Action::AddPage => {
            // PMMP: only before a page that exists.
            let p = page.filter(|&p| p < count && count < MAX_PAGES)?;
            let text = page_text(&packet.text)?;
            pages.insert(p, Page::of(text));
            (BookAction::AddPage, vec![number(p)?])
        }
        Action::DeletePage => {
            let p = page.filter(|&p| p < count)?;
            pages.remove(p);
            (BookAction::DeletePage, vec![number(p)?])
        }
        Action::SwapPages => {
            let a = page?;
            let b = usize::try_from(packet.secondary_page_number).ok()?;
            let last = a.max(b);
            if last >= room {
                return None;
            }
            // The client makes pages without telling; PMMP adds them up to the last one.
            if last >= count {
                pages.resize(last + 1, Page::of(String::new()));
            }
            // PMMP swaps through `setPageText`: the photos go.
            let (ta, tb) = (pages.get(a)?.text.clone(), pages.get(b)?.text.clone());
            *pages.get_mut(a)? = Page::of(tb);
            *pages.get_mut(b)? = Page::of(ta);
            (BookAction::SwapPages, vec![number(a)?, number(b)?])
        }
        Action::Sign => {
            let title = limited(&packet.title, MAX_TITLE_CHARS)?;
            let author = limited(&packet.author, MAX_AUTHOR_CHARS)?;
            let book = written(&self::pages(old), author, title)?;
            return Some((BookAction::SignBook, book, Vec::new()));
        }
    };
    Some((action, with_pages(old, &pages)?, modified))
}

/// A page's text, cleaned (PMMP `TextFormat::clean` keeping colours); `None` over 512 characters.
fn page_text(text: &str) -> Option<String> {
    limited(text, MAX_PAGE_CHARS)
}

/// `text` cleaned, if it has at most `max` characters.
fn limited(text: &str, max: usize) -> Option<String> {
    let cleaned = clean(text);
    (cleaned.chars().count() <= max).then_some(cleaned)
}

/// The pages of a book (PMMP `WritableBookBase::deserializeCompoundTag`: compounds with `text`
/// and `photoname`, or the Java form of strings; anything else is no pages).
fn pages(book: &Stack) -> Vec<Page> {
    let text = |c: &Compound, key: &str| match c.get(key) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    match book.extra().and_then(|e| e.nbt.get(PAGES)) {
        Some(Value::List(List::Compound(list))) => list
            .iter()
            .map(|c| Page {
                text: text(c, TEXT),
                photo: text(c, PHOTO),
            })
            .collect(),
        Some(Value::List(List::String(list))) => list.iter().cloned().map(Page::of).collect(),
        _ => Vec::new(),
    }
}

/// The `pages` list of `pages` (PMMP `WritableBookBase::serializeCompoundTag`); `None` for none.
fn pages_nbt(pages: &[Page]) -> Option<Value> {
    (!pages.is_empty()).then(|| {
        let list = pages
            .iter()
            .map(|p| {
                Compound::from([
                    (TEXT.to_owned(), Value::String(p.text.clone())),
                    (PHOTO.to_owned(), Value::String(p.photo.clone())),
                ])
            })
            .collect();
        Value::List(List::Compound(list))
    })
}

/// `book` with `pages` and its other data as it was; `None` over the stack limits.
fn with_pages(book: &Stack, pages: &[Page]) -> Option<Stack> {
    let mut extra = book.extra().cloned().unwrap_or_default();
    match pages_nbt(pages) {
        Some(list) => extra.nbt.insert(PAGES.to_owned(), list),
        None => extra.nbt.remove(PAGES),
    };
    book.with_extra(Some(extra)).ok()
}

/// A new written book of `pages`, by `author`, named `title`, an original (PMMP
/// `VanillaItems::WRITTEN_BOOK()->setPages()->setAuthor()->setTitle()->setGeneration(0)`); `None`
/// over the stack limits.
fn written(pages: &[Page], author: String, title: String) -> Option<Stack> {
    let mut nbt = Compound::from([
        (GENERATION.to_owned(), Value::Int(0)),
        (AUTHOR.to_owned(), Value::String(author)),
        (TITLE.to_owned(), Value::String(title)),
    ]);
    if let Some(list) = pages_nbt(pages) {
        nbt.insert(PAGES.to_owned(), list);
    }
    let extra = Extra {
        nbt,
        ..Extra::default()
    };
    Stack::new(item::Kind::WrittenBook.default_type(), 1)
        .with_extra(Some(extra))
        .ok()
}
