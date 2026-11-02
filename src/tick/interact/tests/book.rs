//! Tick tests of book editing (PMMP `InGamePacketHandler::handleBookEdit`) and
//! [`PlayerEditBook`](crate::event::PlayerEditBook): packets in, the book in the slot out.

use super::*;
use crate::event::{BookAction as Edit, Listeners, PlayerEditBook, Priority};
use crate::item::Kind as ItemKind;
use crate::nbt::{List, Value};
use crate::protocol::consts::BookAction;
use crate::protocol::packet::BookEdit;
use crate::tick::Context;

/// A book and quill in the player's hand; its slot.
fn quill(h: &mut Harness) -> i32 {
    h.hold(stack(ItemKind::WritableBook));
    i32::from(h.player().inventory.held_slot())
}

/// Sends the edit `action` of slot `slot` with `page`, `second` and `text` (or the title and
/// author when signing) and runs a tick.
fn edit(h: &mut Harness, slot: i32, action: BookAction, page: i32, second: i32, text: &str) {
    let mut packet = BookEdit {
        inventory_slot: slot,
        action_type: u32::from(action),
        page_number: page,
        secondary_page_number: second,
        ..BookEdit::default()
    };
    if action == BookAction::Sign {
        packet.title = text.to_owned();
        packet.author = "Steve".to_owned();
    } else {
        packet.text = text.to_owned();
    }
    let input = h.input(Packet::BookEdit(Box::new(packet)));
    h.run(vec![input]);
}

/// The page texts of the held book.
fn pages(h: &Harness) -> Vec<String> {
    let held = h.held();
    match held.extra().and_then(|e| e.nbt.get("pages")) {
        Some(Value::List(List::Compound(list))) => list
            .iter()
            .map(|c| match c.get("text") {
                Some(Value::String(s)) => s.clone(),
                _ => panic!("a page without text"),
            })
            .collect(),
        None => Vec::new(),
        other => panic!("pages: {other:?}"),
    }
}

#[test]
fn book_edits_replace_insert_delete_and_swap_pages_as_pmmp() {
    let mut h = Harness::new(|_| {});
    let slot = quill(&mut h);
    edit(&mut h, slot, BookAction::ReplacePage, 0, 0, "a");
    assert_eq!(pages(&h), ["a"]);
    // A page past the end grows the book with empty pages.
    edit(&mut h, slot, BookAction::ReplacePage, 2, 0, "c");
    assert_eq!(pages(&h), ["a", "", "c"]);
    edit(&mut h, slot, BookAction::AddPage, 1, 0, "b");
    assert_eq!(pages(&h), ["a", "b", "", "c"]);
    edit(&mut h, slot, BookAction::DeletePage, 2, 0, "");
    assert_eq!(pages(&h), ["a", "b", "c"]);
    edit(&mut h, slot, BookAction::SwapPages, 0, 2, "");
    assert_eq!(pages(&h), ["c", "b", "a"]);
    // The client makes pages without telling: a swap with a missing page adds it first.
    edit(&mut h, slot, BookAction::SwapPages, 0, 4, "");
    assert_eq!(pages(&h), ["", "b", "a", "", "c"]);
    // Inserting before or deleting a page that does not exist does nothing.
    edit(&mut h, slot, BookAction::AddPage, 5, 0, "x");
    edit(&mut h, slot, BookAction::DeletePage, 5, 0, "");
    edit(&mut h, slot, BookAction::DeletePage, -1, 0, "");
    assert_eq!(pages(&h).len(), 5);
    // Colours stay; private-use characters and terminal escapes go.
    edit(
        &mut h,
        slot,
        BookAction::ReplacePage,
        0,
        0,
        "§cred\u{E000}\u{1b}[31m",
    );
    assert_eq!(pages(&h)[0], "§cred");
    assert_eq!(h.held().count(), 1);
}

#[test]
fn book_edits_keep_pmmps_page_and_text_limits() {
    let mut h = Harness::new(|_| {});
    let slot = quill(&mut h);
    // 512 characters fit on a page (768 bytes, the wire limit), 513 do not.
    let page = format!("{}{}", "é".repeat(256), "x".repeat(256));
    edit(&mut h, slot, BookAction::ReplacePage, 0, 0, &page);
    assert_eq!(pages(&h)[0].chars().count(), 512);
    edit(
        &mut h,
        slot,
        BookAction::ReplacePage,
        0,
        0,
        &"x".repeat(513),
    );
    assert_eq!(pages(&h)[0].chars().count(), 512, "too long: refused");
    // Page 50 would be the 51st: refused before any page is made; page 49 is the 50th.
    edit(&mut h, slot, BookAction::ReplacePage, 50, 0, "");
    edit(&mut h, slot, BookAction::ReplacePage, i32::MAX, 0, "");
    edit(&mut h, slot, BookAction::SwapPages, 0, 50, "");
    assert_eq!(pages(&h).len(), 1);
    edit(&mut h, slot, BookAction::ReplacePage, 49, 0, "last");
    assert_eq!(pages(&h).len(), 50);
    edit(&mut h, slot, BookAction::AddPage, 0, 0, "one more");
    assert_eq!(pages(&h).len(), 50, "a full book takes no more pages");
    edit(&mut h, slot, BookAction::SwapPages, 0, 49, "");
    assert_eq!(pages(&h)[0], "last", "swapping within a full book works");
    // A title of 17 characters is refused; 16 sign the book.
    edit(&mut h, slot, BookAction::Sign, 0, 0, &"t".repeat(17));
    assert_eq!(h.held().item(), Some(ItemKind::WritableBook.default_type()));
    edit(&mut h, slot, BookAction::Sign, 0, 0, &"t".repeat(16));
    assert_eq!(h.held().item(), Some(ItemKind::WrittenBook.default_type()));
}

/// The edits are checked against the page count before any page is copied; pages in a list of
/// another type are no pages, for the count as for the edit.
#[test]
fn a_book_with_pages_of_another_type_counts_none() {
    let mut h = Harness::new(|_| {});
    let extra = crate::item::Extra {
        nbt: crate::nbt::Compound::from([("pages".to_owned(), Value::List(List::Int(vec![1, 2])))]),
        ..crate::item::Extra::default()
    };
    h.hold(
        stack(ItemKind::WritableBook)
            .with_extra(Some(extra))
            .unwrap(),
    );
    let slot = i32::from(h.player().inventory.held_slot());
    edit(&mut h, slot, BookAction::AddPage, 0, 0, "x");
    edit(&mut h, slot, BookAction::DeletePage, 0, 0, "");
    assert!(matches!(
        h.held().extra().and_then(|e| e.nbt.get("pages")),
        Some(Value::List(List::Int(_)))
    ));
    edit(&mut h, slot, BookAction::ReplacePage, 0, 0, "a");
    assert_eq!(pages(&h), ["a"]);
}

/// A lectern counts a book's pages as the book edits do (PMMP `Lectern::onPageTurn`): pages in a
/// list of another type are none, so no page turn is taken.
#[test]
fn a_lectern_turns_no_page_of_a_book_with_pages_of_another_type() {
    let mut h = Harness::new(|_| {});
    let extra = crate::item::Extra {
        nbt: crate::nbt::Compound::from([("pages".to_owned(), Value::List(List::Int(vec![1, 2])))]),
        ..crate::item::Extra::default()
    };
    let book = stack(ItemKind::WritableBook)
        .with_extra(Some(extra))
        .unwrap();
    set_with_entity(&mut h, Kind::Lectern.default_state(), &Stack::EMPTY);
    h.run(Vec::new());
    h.hold(book);
    use_on_target(&mut h);
    let turn = h.input(Packet::LecternUpdate(
        crate::protocol::packet::LecternUpdate {
            page: 1,
            page_count: 2,
            position: [TARGET.x, TARGET.y, TARGET.z],
        },
    ));
    h.run(vec![turn]);
    assert!(matches!(entity_at(&h),
        crate::tick::block_entity::Kind::Lectern { book, page: 0 } if !book.is_empty()));
}

/// Page turns of one tick reach the players that see the lectern once, with the last page: the
/// client keeps only the last of them anyway.
#[test]
fn a_ticks_page_turns_send_the_lectern_once_with_the_last_page() {
    let mut h = Harness::new(|_| {});
    let page = |text: &str| {
        crate::nbt::Compound::from([("text".to_owned(), Value::String(text.to_owned()))])
    };
    let extra = crate::item::Extra {
        nbt: crate::nbt::Compound::from([(
            "pages".to_owned(),
            Value::List(List::Compound(vec![page("a"), page("b")])),
        )]),
        ..crate::item::Extra::default()
    };
    let book = stack(ItemKind::WritableBook)
        .with_extra(Some(extra))
        .unwrap();
    set_with_entity(&mut h, Kind::Lectern.default_state(), &Stack::EMPTY);
    h.run(Vec::new());
    h.hold(book);
    use_on_target(&mut h);
    h.packets();
    let turns: Vec<_> = (1..=1001_u16)
        .map(|i| {
            h.input(Packet::LecternUpdate(
                crate::protocol::packet::LecternUpdate {
                    page: u8::try_from(i % 2).unwrap(),
                    page_count: 2,
                    position: [TARGET.x, TARGET.y, TARGET.z],
                },
            ))
        })
        .collect();
    h.run(turns);
    let sent: Vec<_> = h
        .packets()
        .into_iter()
        .filter_map(|p| match p {
            Packet::BlockActorData(d) => Some(d.nbt_data),
            _ => None,
        })
        .collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].get("page"), Some(&Value::Int(1)));
}

#[test]
fn signing_makes_an_original_written_book_and_only_books_and_quills_are_edited() {
    let mut h = Harness::new(|_| {});
    let slot = quill(&mut h);
    edit(&mut h, slot, BookAction::ReplacePage, 0, 0, "story");
    edit(&mut h, slot, BookAction::Sign, 0, 0, "Title");
    let book = h.held();
    assert_eq!(
        (book.item(), book.count()),
        (Some(ItemKind::WrittenBook.default_type()), 1)
    );
    let nbt = &book.extra().unwrap().nbt;
    assert_eq!(nbt.get("title"), Some(&Value::String("Title".to_owned())));
    assert_eq!(nbt.get("author"), Some(&Value::String("Steve".to_owned())));
    // An original's generation is the type's own NBT, which the extra data does not repeat.
    assert_eq!(book.tag().unwrap().get("generation"), Some(&Value::Int(0)));
    assert_eq!(pages(&h), ["story"]);
    // A written book is not edited; neither is a slot outside the inventory.
    edit(&mut h, slot, BookAction::ReplacePage, 0, 0, "changed");
    assert_eq!(h.held(), book);
    for wrong in [-1, 36, 1000] {
        edit(&mut h, wrong, BookAction::ReplacePage, 0, 0, "x");
    }
    assert_eq!(h.held(), book);
}

#[test]
fn the_same_page_replaced_twice_is_the_same_book() {
    let run = |times: usize| {
        let mut h = Harness::new(|_| {});
        let slot = quill(&mut h);
        for _ in 0..times {
            edit(&mut h, slot, BookAction::ReplacePage, 3, 0, "same");
        }
        h.held()
    };
    assert_eq!(run(1), run(2));
}

/// Listeners that write down what they see and do `then` to the event.
fn listener(then: fn(&mut PlayerEditBook, &mut Context<'_>)) -> Listeners {
    let mut l = Listeners::default();
    l.push(
        Priority::Normal,
        move |e: &mut PlayerEditBook, cx: &mut Context<'_>| {
            note(
                SEEN,
                (
                    e.player(),
                    e.action(),
                    e.old_book().item(),
                    e.new_book().item(),
                    e.modified_pages(),
                ),
            );
            then(e, cx);
        },
    );
    l.push_monitor(move |e: &PlayerEditBook, cx: &Context<'_>| {
        let held = cx.state.players[&SessionId::new(1)]
            .inventory
            .held()
            .clone();
        note(SEEN, &held == e.new_book());
    });
    l
}

// pmmp: PlayerEditBookEvent, PlayerEditBookEvent::getAction, PlayerEditBookEvent::getOldBook, PlayerEditBookEvent::getNewBook, PlayerEditBookEvent::getModifiedPages, PlayerEditBookEvent::setNewBook
#[test]
fn an_edit_book_listener_sees_the_edit_and_may_replace_the_new_book() {
    let l = listener(|e, _| {
        if e.action() == Edit::SignBook {
            let unsigned = e.old_book().clone();
            e.set_new_book(unsigned);
        }
        // Not a book: ignored.
        e.set_new_book(Stack::new(ItemKind::Stick.default_type(), 1));
    });
    let mut h = Harness::with(|_| {}, l);
    let slot = quill(&mut h);
    edit(&mut h, slot, BookAction::SwapPages, 0, 1, "");
    edit(&mut h, slot, BookAction::Sign, 0, 0, "Title");
    let quill_type = Some(ItemKind::WritableBook.default_type());
    let written = Some(ItemKind::WrittenBook.default_type());
    assert_eq!(
        notes(SEEN),
        [
            dbg((
                crate::player::Id::new(SessionId::new(1)),
                Edit::SwapPages,
                quill_type,
                quill_type,
                [0u32, 1].as_slice()
            )),
            dbg(true),
            dbg((
                crate::player::Id::new(SessionId::new(1)),
                Edit::SignBook,
                quill_type,
                written,
                [0u32; 0].as_slice()
            )),
            dbg(true),
        ]
    );
    assert_eq!(
        h.held().item(),
        quill_type,
        "the listener kept the book unsigned"
    );
    assert_eq!(pages(&h), ["", ""]);
}

// pmmp: PlayerEditBookEvent::cancel, PlayerEditBookEvent::uncancel, PlayerEditBookEvent::isCancelled
#[test]
fn a_cancelled_edit_keeps_the_book_and_a_listener_that_takes_it_cancels_the_edit() {
    let l = listener(|e, _| {
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        e.cancel();
    });
    let mut h = Harness::with(|_| {}, l);
    let slot = quill(&mut h);
    edit(&mut h, slot, BookAction::ReplacePage, 0, 0, "a");
    assert!(pages(&h).is_empty());
    assert_eq!(notes(SEEN).len(), 1, "no monitor for a cancelled edit");
    // A listener that takes the book: the edit does not put it back (nothing is made).
    let l = listener(|_, cx| {
        let player = cx.state.players.get_mut(&SessionId::new(1)).unwrap();
        inventory::set_held(player, Stack::EMPTY);
    });
    let mut h = Harness::with(|_| {}, l);
    let slot = quill(&mut h);
    edit(&mut h, slot, BookAction::ReplacePage, 0, 0, "a");
    assert!(h.held().is_empty());
}

/// Whether the packets the tick sent hold the slot `slot` of the inventory again.
fn slot_resent(h: &mut Harness, slot: i32) -> bool {
    let slot = u32::try_from(slot).unwrap();
    h.packets()
        .iter()
        .any(|p| matches!(p, Packet::InventorySlot(s) if s.slot == slot))
}

#[test]
fn a_refused_or_cancelled_edit_sends_the_slot_again() {
    let mut h = Harness::with(|_| {}, listener(|e, _| e.cancel()));
    let slot = quill(&mut h);
    h.packets();
    edit(&mut h, slot, BookAction::ReplacePage, 0, 0, "a");
    assert!(slot_resent(&mut h, slot), "cancelled");
    let mut h = Harness::new(|_| {});
    let slot = quill(&mut h);
    h.packets();
    edit(
        &mut h,
        slot,
        BookAction::ReplacePage,
        0,
        0,
        &"x".repeat(513),
    );
    assert!(slot_resent(&mut h, slot), "refused");
}
