//! Book editing: a player rewrites, inserts, deletes or swaps the pages of a book and quill in
//! its inventory, or signs it into a written book, through [`PlayerEditBook`].
//!
//! An edit that would take the book past [`MAX_PAGES`] is refused before anything is built, so
//! one packet cannot make millions of pages. A text over its limits or too many pages is refused
//! without an event, as other actions that cannot happen are. The packet's strings are at most
//! 768 bytes, so only the character limits remain.

use tracing::debug;

use super::can_act;
use super::sign::clean;
use crate::event::{BookAction, PlayerEditBook};
use crate::item::{self, Extra, Stack};
use crate::nbt::{Compound, List, Value};
use crate::protocol::consts::BookAction as Action;
use crate::protocol::packet::BookEdit;
use crate::tick::{SessionId, State, dispatch, inventory};

/// The NBT key of a written book's author.
const AUTHOR: &str = "author";
/// The NBT key of a written book's generation.
const GENERATION: &str = "generation";
/// The most characters in an author.
const MAX_AUTHOR_CHARS: usize = 256;
/// The most characters on a page.
const MAX_PAGE_CHARS: usize = 512;
/// The most pages a player may give a book (a book that has more, made by code, may still be
/// edited without growing).
const MAX_PAGES: usize = 50;
/// The most characters in a title.
const MAX_TITLE_CHARS: usize = 16;
/// The NBT key of a book's pages.
const PAGES: &str = "pages";
/// The NBT key of a page's photo name.
const PHOTO: &str = "photoname";
/// The NBT key of a page's text.
const TEXT: &str = "text";
/// The NBT key of a written book's title.
const TITLE: &str = "title";

/// A page of a book.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Page {
    text: String,
    photo: String,
}

impl Page {
    /// Makes a page of `text` without a photo.
    fn of(text: String) -> Self {
        Self {
            text,
            photo: String::new(),
        }
    }
}

/// Applies `BookEdit` from the client: the edit of the book and quill in main slot
/// `inventory_slot`, which [`PlayerEditBook`] may change or cancel. A refused or cancelled edit
/// sends the client the slot again. Anything that does not fit is ignored.
pub(in crate::tick) fn on_book_edit(state: &mut State, session: SessionId, packet: &BookEdit) {
    let Ok(slot) = u8::try_from(packet.inventory_slot) else {
        return;
    };
    let Some(old) = writable(state, session, slot).cloned() else {
        return;
    };
    let Some((action, new, modified)) = edited(&old, packet) else {
        debug!(session = session.get(), "book edit refused");
        inventory::resend_main(state, session, slot);
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
            inventory::resend_main(state, session, slot);
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
    state
        .players
        .get(&session)
        .filter(|p| can_act(p))?
        .inventory
        .main_stack(slot)
        .filter(|s| s.item().is_some_and(|t| t.is(item::Class::WritableBook)))
}

/// What `packet` makes of the book `old`: the action, the new book and the pages it changed;
/// `None` for an edit that is refused, or a book over the stack limits. Every check comes before
/// the pages are copied, so a refused edit costs nothing like the book's size.
fn edited(old: &Stack, packet: &BookEdit) -> Option<(BookAction, Stack, Vec<u32>)> {
    let count = page_count(old);
    // The page count the edit may reach: it may shrink or stay, and grow up to `MAX_PAGES`.
    let room = count.max(MAX_PAGES);
    let page = usize::try_from(packet.page_number).ok();
    let number = |p: usize| u32::try_from(p).ok();
    let (action, modified, pages) = match Action::try_from(packet.action_type).ok()? {
        Action::ReplacePage => {
            let p = page.filter(|&p| p < room)?;
            let text = page_text(&packet.text)?;
            let mut pages = pages(old);
            if p >= count {
                pages.resize(p + 1, Page::of(String::new()));
            }
            *pages.get_mut(p)? = Page::of(text);
            (BookAction::ReplacePage, vec![number(p)?], pages)
        }
        Action::AddPage => {
            // Only before a page that exists.
            let p = page.filter(|&p| p < count && count < MAX_PAGES)?;
            let text = page_text(&packet.text)?;
            let mut pages = pages(old);
            pages.insert(p, Page::of(text));
            (BookAction::AddPage, vec![number(p)?], pages)
        }
        Action::DeletePage => {
            let p = page.filter(|&p| p < count)?;
            let mut pages = pages(old);
            pages.remove(p);
            (BookAction::DeletePage, vec![number(p)?], pages)
        }
        Action::SwapPages => {
            let a = page?;
            let b = usize::try_from(packet.secondary_page_number).ok()?;
            let last = a.max(b);
            if last >= room {
                return None;
            }
            let mut pages = pages(old);
            // The client makes pages without telling; they are added up to the last one.
            if last >= count {
                pages.resize(last + 1, Page::of(String::new()));
            }
            // A swap moves the texts only: the photos go.
            let (ta, tb) = (pages.get(a)?.text.clone(), pages.get(b)?.text.clone());
            *pages.get_mut(a)? = Page::of(tb);
            *pages.get_mut(b)? = Page::of(ta);
            (BookAction::SwapPages, vec![number(a)?, number(b)?], pages)
        }
        Action::Sign => {
            let title = limited(&packet.title, MAX_TITLE_CHARS)?;
            let author = limited(&packet.author, MAX_AUTHOR_CHARS)?;
            let book = written(pages(old), author, title)?;
            return Some((BookAction::SignBook, book, Vec::new()));
        }
    };
    Some((action, with_pages(old, pages)?, modified))
}

/// A page's text, cleaned keeping colours; `None` over [`MAX_PAGE_CHARS`] characters.
fn page_text(text: &str) -> Option<String> {
    limited(text, MAX_PAGE_CHARS)
}

/// The `text` cleaned, if it has at most `max` characters.
fn limited(text: &str, max: usize) -> Option<String> {
    let cleaned = clean(text);
    (cleaned.chars().count() <= max).then_some(cleaned)
}

/// The pages of a book: compounds with `text` and `photoname`, or the Java form of strings;
/// anything else is no pages.
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

/// The number of [`pages`] of a book, counted without copying them.
fn page_count(book: &Stack) -> usize {
    match book.extra().and_then(|e| e.nbt.get(PAGES)) {
        Some(Value::List(List::Compound(list))) => list.len(),
        Some(Value::List(List::String(list))) => list.len(),
        _ => 0,
    }
}

/// The `book` with `pages` and its other data as it was; `None` over the stack limits. The
/// old pages are left out of the copy, as the new ones replace them.
fn with_pages(book: &Stack, pages: Vec<Page>) -> Option<Stack> {
    let mut extra = book.extra().map_or_else(Extra::default, |old| Extra {
        custom_name: old.custom_name.clone(),
        lore: old.lore.clone(),
        enchantments: old.enchantments.clone(),
        nbt: old
            .nbt
            .iter()
            .filter(|(key, _)| key.as_str() != PAGES)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    });
    if let Some(list) = pages_nbt(pages) {
        extra.nbt.insert(PAGES.to_owned(), list);
    }
    book.with_extra(Some(extra)).ok()
}

/// Makes a new written book of `pages`, by `author`, named `title`, an original; `None` over the
/// stack limits.
fn written(pages: Vec<Page>, author: String, title: String) -> Option<Stack> {
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

/// The `pages` list of `pages`; `None` for none.
fn pages_nbt(pages: Vec<Page>) -> Option<Value> {
    (!pages.is_empty()).then(|| {
        let list = pages
            .into_iter()
            .map(|p| {
                Compound::from([
                    (TEXT.to_owned(), Value::String(p.text)),
                    (PHOTO.to_owned(), Value::String(p.photo)),
                ])
            })
            .collect();
        Value::List(List::Compound(list))
    })
}
