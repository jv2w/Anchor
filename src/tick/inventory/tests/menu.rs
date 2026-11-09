//! Tests of fake blocks and menu windows.
//!
//! They cover who is sent what and when, the bounds, that a menu without editable slots never
//! moves an item, and that editable and lending menus move items like a chest and keep each one
//! once.

use std::cell::Cell;

use super::*;
use crate::menu::{self as m, Click, Kind as MenuKind, Menu, Response, SlotAction};
use crate::protocol::consts::ContainerType;
use crate::protocol::packet::SubChunkRequest;
use crate::tick::inventory::{load, restore, save};
use crate::tick::tests::{blocks_set, input, leave, packet};
use crate::tick::view::MAX_FAKE_BLOCKS;
use crate::tick::{Context, Phase, entity, survival};

/// The player's slot the handler of the store test writes with each change.
/// A box keeps a menu's items this way.
const STORE_SLOT: u8 = 0;
/// The main slot the request of the handler-writes test puts a stick into.
/// The request names it by its own ID, so no stack network ID goes stale.
const TO: u8 = 5;

thread_local! {
    /// What a lending menu's handler answers a change with.
    static ANSWER: Cell<Response> = const { Cell::new(Response::Keep) };
    /// How many closes the handler had heard as a quit's monitor ran.
    static AT_MONITOR: Cell<Option<usize>> = const { Cell::new(None) };
    /// The changes the handlers of a test heard.
    static CHANGES: Cell<Vec<m::Change>> = const { Cell::new(Vec::new()) };
    /// The clicks the handlers of a test heard.
    static CLICKS: Cell<Vec<Click>> = const { Cell::new(Vec::new()) };
    /// The closes the handlers of a test heard.
    static CLOSES: Cell<Vec<m::Close>> = const { Cell::new(Vec::new()) };
    /// How many quit events a test's listener and monitor heard.
    static QUITS: Cell<u32> = const { Cell::new(0) };
    /// The sticks a lending menu's handler keeps: its menu's slots and the lent ones.
    static STORE: Cell<i64> = const { Cell::new(0) };
    /// How many transactions a test's listener heard.
    static TRANSACTIONS: Cell<u32> = const { Cell::new(0) };
    /// The result of the cursor write a close handler tried.
    static WROTE: Cell<Option<crate::inventory::Result<()>>> = const { Cell::new(None) };
}

/// What an editable menu's handler answers a change with.
type Answer = fn(&mut Context<'_>, crate::player::Id, &m::Change) -> Response;

/// How a test closes a menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Way {
    Client,
    Far,
    OtherWindow,
    Death,
    Teleport,
    Leave,
    ChangeCloses,
    ClickCloses,
    CloseWindow,
    OtherMenu,
}

/// The clicks the handlers heard since the last call.
fn heard() -> Vec<Click> {
    CLICKS.take()
}

/// Keeps `click` for [`heard`].
fn record(click: &Click) {
    let mut c = CLICKS.take();
    c.push(click.clone());
    CLICKS.set(c);
}

/// A glass block.
fn glass() -> blk::State {
    blk::Kind::Glass.default_state()
}

/// The context over the world's state.
fn cx(w: &mut World) -> Context<'_> {
    Context {
        state: &mut w.state,
    }
}

/// The player ID of session `s`.
fn id(s: SessionId) -> crate::player::Id {
    crate::player::Id::new(s)
}

/// One tick; the packets each joined session got, in join order.
fn tick_each(w: &mut World, inputs: Vec<Input>) -> Vec<Vec<Packet>> {
    w.tick_each(inputs)
}

/// A block near the player, in a chunk its client was sent.
fn near(w: &mut World, s: SessionId) -> BlockPos {
    let eye = w.player_mut(s).position;
    let at = BlockPos::containing(eye);
    let pos = BlockPos::new(at.x + 2, -61, at.z + 2);
    assert!(w.player_mut(s).view.can_see(pos.chunk()));
    pos
}

fn real(w: &World, pos: BlockPos) -> u32 {
    w.state.worlds[0].block(pos).unwrap().network_id()
}

// api: World::createBlockUpdatePackets
#[test]
fn a_fake_block_reaches_its_player_only_and_clearing_shows_the_real_block() {
    let mut w = World::new(GameMode::Survival);
    let (a, b) = (w.join(1), w.join(2));
    tick_each(&mut w, Vec::new());
    let pos = near(&mut w, a);
    assert_eq!(cx(&mut w).send_fake_block(id(a), pos, glass()), Ok(()));
    assert_eq!(cx(&mut w).fake_block(id(a), pos), Some(glass()));
    assert_eq!(cx(&mut w).fake_block(id(b), pos), None);
    let sent = tick_each(&mut w, Vec::new());
    assert_eq!(blocks_set(&sent[0]), [(pos, glass().network_id())]);
    assert!(
        blocks_set(&sent[1]).is_empty(),
        "the other player sees nothing"
    );
    // The same block again sends nothing.
    assert_eq!(cx(&mut w).send_fake_block(id(a), pos, glass()), Ok(()));
    assert!(blocks_set(&tick_each(&mut w, Vec::new())[0]).is_empty());
    assert!(cx(&mut w).clear_fake_block(id(a), pos));
    assert!(!cx(&mut w).clear_fake_block(id(a), pos), "cleared once");
    let sent = tick_each(&mut w, Vec::new());
    assert_eq!(blocks_set(&sent[0]), [(pos, real(&w, pos))]);
    // Set then cleared in one tick: the last change wins.
    assert_eq!(cx(&mut w).send_fake_block(id(a), pos, glass()), Ok(()));
    assert!(cx(&mut w).clear_fake_block(id(a), pos));
    assert_eq!(
        blocks_set(&tick_each(&mut w, Vec::new())[0]),
        [(pos, real(&w, pos))]
    );
}

#[test]
fn a_fake_block_hides_real_changes_and_comes_back_with_its_sub_chunk() {
    let mut w = World::new(GameMode::Survival);
    let (a, _b) = (w.join(1), w.join(2));
    tick_each(&mut w, Vec::new());
    let pos = near(&mut w, a);
    let other = BlockPos::new(pos.x + 1, pos.y, pos.z);
    cx(&mut w).send_fake_block(id(a), pos, glass()).unwrap();
    tick_each(&mut w, Vec::new());
    // A real change at the fake block and one beside it in the same tick.
    let stone = blk::Kind::Stone.default_state();
    w.state.worlds[0].set_block(pos, stone).unwrap();
    w.state.worlds[0].set_block(other, stone).unwrap();
    let sent = tick_each(&mut w, Vec::new());
    assert_eq!(blocks_set(&sent[0]), [(other, stone.network_id())]);
    let seen = blocks_set(&sent[1]);
    assert!(
        seen.contains(&(pos, stone.network_id())),
        "the other player sees it"
    );
    // The client asks for the sub-chunk again: the fake block follows the answer.
    let chunk = pos.chunk();
    let request = SubChunkRequest {
        dimension: 0,
        position: [chunk.x, pos.y >> 4, chunk.z],
        offsets: vec![[0, 0, 0]],
    };
    let sent = tick_each(
        &mut w,
        vec![input(1, 50, packet(Packet::SubChunkRequest(request)))],
    );
    let at = sent[0]
        .iter()
        .position(|p| matches!(p, Packet::SubChunk(_)))
        .expect("the answer");
    assert_eq!(
        blocks_set(&sent[0][at..]),
        [(pos, glass().network_id())],
        "after the sub-chunk"
    );
    // Cleared: the real block, as it is now.
    assert!(cx(&mut w).clear_fake_blocks(id(a)));
    assert_eq!(
        blocks_set(&tick_each(&mut w, Vec::new())[0]),
        [(pos, stone.network_id())]
    );
}

#[test]
fn fake_blocks_are_bounded_and_go_with_their_player() {
    let mut w = World::new(GameMode::Survival);
    let a = w.join(1);
    let base = near(&mut w, a);
    let at = |i: usize| {
        let i = i32::try_from(i).unwrap();
        BlockPos::new(base.x + i % 64, -60 + i / 64 % 64, base.z)
    };
    for i in 0..MAX_FAKE_BLOCKS {
        assert_eq!(cx(&mut w).send_fake_block(id(a), at(i), glass()), Ok(()));
    }
    let extra = BlockPos::new(base.x, 100, base.z + 1);
    assert_eq!(
        cx(&mut w).send_fake_block(id(a), extra, glass()),
        Err(m::Error::Full)
    );
    // Changing one it has is not one more.
    assert_eq!(
        cx(&mut w).send_fake_block(id(a), at(0), blk::Kind::Stone.default_state()),
        Ok(())
    );
    assert_eq!(
        cx(&mut w).send_fake_block(id(a), BlockPos::new(0, 320, 0), glass()),
        Err(m::Error::OutsideWorld)
    );
    w.tick(Vec::new());
    w.sent(a);
    assert!(cx(&mut w).clear_fake_blocks(id(a)));
    // The clears wait to be sent: no room for more changes this tick.
    assert_eq!(
        cx(&mut w).send_fake_block(id(a), extra, glass()),
        Err(m::Error::Busy)
    );
    w.tick(Vec::new());
    assert_eq!(cx(&mut w).send_fake_block(id(a), extra, glass()), Ok(()));
    assert_eq!(w.player_mut(a).view.fake_count(), 1);
    w.tick(vec![leave(1, 90)]);
    assert!(!w.state.players.contains_key(&a));
    assert_eq!(
        cx(&mut w).send_fake_block(id(a), extra, glass()),
        Err(m::Error::Offline)
    );
    assert!(!cx(&mut w).clear_fake_blocks(id(a)));
}

/// A diamond sword.
fn sword() -> Stack {
    Stack::new(item(it::Kind::DiamondSword), 1)
}

// api: Player::setCurrentWindow
#[test]
fn a_menu_opens_on_fake_blocks_with_its_title_then_its_window() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    tick_each(&mut w, Vec::new());
    let menu = Menu::new(MenuKind::Chest)
        .with_title("Shop")
        .item(13, sword());
    let opened = cx(&mut w).open_menu(id(s), menu, |_, _, _| Response::Keep);
    let menu_id = opened.unwrap();
    assert_eq!(cx(&mut w).menu(id(s)), Some(menu_id));
    assert_eq!(
        w.player_mut(s).window(),
        None,
        "events and windows do not name menus"
    );
    let [Some((pos, chest)), None] = w.player_mut(s).view.menu_blocks() else {
        panic!("one chest");
    };
    assert_eq!(chest, blk::Kind::Chest.default_state());
    let first = tick_each(&mut w, Vec::new()).swap_remove(0);
    assert_eq!(blocks_set(&first), [(pos, chest.network_id())]);
    let data: Vec<_> = first
        .iter()
        .filter_map(|p| match p {
            Packet::BlockActorData(d) => Some(d),
            _ => None,
        })
        .collect();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].position, [pos.x, pos.y, pos.z]);
    assert_eq!(
        data[0].nbt_data.get("CustomName"),
        Some(&Value::String("Shop".to_owned()))
    );
    assert_eq!(
        data[0].nbt_data.get("id"),
        Some(&Value::String("Chest".to_owned()))
    );
    let update = first
        .iter()
        .position(|p| matches!(p, Packet::UpdateBlock(_)));
    let actor = first
        .iter()
        .position(|p| matches!(p, Packet::BlockActorData(_)));
    assert!(update < actor, "the block before its data");
    assert!(!first.iter().any(|p| matches!(p, Packet::ContainerOpen(_))));
    let second = tick_each(&mut w, Vec::new()).swap_remove(0);
    assert!(!second.iter().any(|p| matches!(p, Packet::ContainerOpen(_))));
    let third = tick_each(&mut w, Vec::new()).swap_remove(0);
    let Some(Packet::ContainerOpen(o)) =
        third.iter().find(|p| matches!(p, Packet::ContainerOpen(_)))
    else {
        panic!("the window opens");
    };
    assert_eq!(o.container_type, ContainerType::Container as u8);
    assert_eq!(o.container_position, [pos.x, pos.y, pos.z]);
    let Some(Packet::InventoryContent(c)) = third.iter().find(
        |p| matches!(p, Packet::InventoryContent(c) if c.window_id == u32::from(o.window_id)),
    ) else {
        panic!("the menu's content");
    };
    assert_eq!(c.content.len(), 27);
    assert_eq!(c.content[13].stack.count, 1);
    assert_ne!(c.content[13].stack_network_id, 0);
    assert_eq!(c.content[0].stack.count, 0);
}

/// Opens `menu` for `s` with a handler that records each click and answers `response(click)`.
/// It runs the ticks until the window opens, and returns the packets `s` got meanwhile.
fn open(
    w: &mut World,
    s: SessionId,
    menu: Menu,
    response: fn(&mut Context<'_>, crate::player::Id, &Click) -> Response,
) -> (m::Id, Vec<Packet>) {
    heard();
    let menu = cx(w)
        .open_menu(id(s), menu, move |cx, player, input| match input {
            m::Input::Click(click) => {
                record(&click);
                response(cx, player, &click)
            }
            m::Input::Close(_) => Response::Keep,
            m::Input::Change(_) => panic!("no slot is editable: {input:?}"),
        })
        .unwrap();
    let mut got = Vec::new();
    for _ in 0..3 {
        // The client acknowledges a window the server closed for the menu.
        w.ack(s);
        got.extend(tick_each(w, Vec::new()).swap_remove(0));
    }
    assert_eq!(w.inv(s).window.as_ref().map(|w| w.id.is_some()), Some(true));
    (menu, got)
}

/// A click handler answer that keeps the click as it is.
fn keep(_: &mut Context<'_>, _: crate::player::Id, _: &Click) -> Response {
    Response::Keep
}

#[test]
fn double_chest_and_hopper_menus_have_their_blocks_slots_and_type() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let (_, got) = open(&mut w, s, Menu::new(MenuKind::DoubleChest), keep);
    let [Some((left, _)), Some((right, _))] = w.player_mut(s).view.menu_blocks() else {
        panic!("two chests");
    };
    assert_eq!(left.y, right.y);
    assert_eq!(left.x.abs_diff(right.x) + left.z.abs_diff(right.z), 1);
    let pairs: Vec<_> = got
        .iter()
        .filter_map(|p| match p {
            Packet::BlockActorData(d) => Some((d.position, d.nbt_data.get("pairx").cloned())),
            _ => None,
        })
        .collect();
    assert_eq!(
        pairs,
        [
            ([left.x, left.y, left.z], Some(Value::Int(right.x))),
            ([right.x, right.y, right.z], Some(Value::Int(left.x)))
        ]
    );
    let content = |got: &[Packet]| {
        got.iter()
            .find_map(|p| match p {
                Packet::InventoryContent(c) if c.window_id >= 2 && c.window_id < 100 => {
                    Some(c.content.len())
                }
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(content(&got), 54);
    let (_, got) = open(&mut w, s, Menu::new(MenuKind::Hopper), keep);
    assert_eq!(content(&got), 5);
    assert!(got.iter().any(|p| matches!(
        p,
        Packet::ContainerOpen(o) if o.container_type == ContainerType::Hopper as u8
    )));
    let [Some((_, hopper)), None] = w.player_mut(s).view.menu_blocks() else {
        panic!("one hopper");
    };
    assert_eq!(hopper, blk::Kind::Hopper.default_state());
    // The double chest's blocks showed the real ones again when the hopper replaced it.
    assert!(blocks_set(&got).contains(&(right, real(&w, right))));
}

#[test]
fn invalid_menus_and_players_open_nothing() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let bad = [
        (
            Menu::new(MenuKind::Hopper).item(5, sword()),
            m::Error::NoSlot,
        ),
        (
            Menu::new(MenuKind::Chest).with_title("x".repeat(1025)),
            m::Error::TitleTooLong,
        ),
    ];
    for (menu, error) in bad {
        assert_eq!(
            cx(&mut w).open_menu(id(s), menu, |_, _, _| Response::Keep),
            Err(error)
        );
    }
    let nobody = id(SessionId::new(9));
    assert_eq!(
        cx(&mut w).open_menu(nobody, Menu::new(MenuKind::Chest), |_, _, _| Response::Keep),
        Err(m::Error::Offline)
    );
    assert_eq!(
        cx(&mut w).set_menu_item(id(s), 0, sword()),
        Err(m::Error::NotOpen)
    );
    assert!(w.inv(s).window.is_none());
    assert_eq!(w.state.menus.len(), 0);
}

#[test]
fn menu_ids_are_refused_once_used_up_and_never_wrap() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    tick_each(&mut w, Vec::new());
    w.player_mut(s).inventory.next_menu = u64::from(u32::MAX);
    let last = cx(&mut w)
        .open_menu(id(s), Menu::new(MenuKind::Chest), |_, _, _| Response::Keep)
        .expect("the last ID");
    assert_eq!(last, m::Id::new(u32::MAX));
    assert_eq!(
        cx(&mut w).open_menu(id(s), Menu::new(MenuKind::Hopper), |_, _, _| Response::Keep),
        Err(m::Error::Exhausted)
    );
    assert_eq!(cx(&mut w).menu(id(s)), Some(last), "the open menu stays");
    assert_eq!(w.state.menus.len(), 1);
}

/// The menu window's ID and the stack network ID of menu slot `slot`.
fn shown(w: &World, s: SessionId, slot: usize) -> (u8, i32) {
    let window = w.inv(s).window.as_ref().unwrap();
    (window.id.unwrap(), window.ids[slot].net)
}

/// The request action taking one item from menu slot `slot` onto the cursor.
fn take_from_menu(slot: u8, net: i32) -> StackRequestAction {
    take(1, at(C::LevelEntity, slot, net), at(C::Cursor, 0, 0))
}

#[test]
fn a_click_calls_the_handler_once_and_moves_nothing() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.give(s, Stack::new(item(it::Kind::Stick), 10));
    let (menu, _) = open(
        &mut w,
        s,
        Menu::new(MenuKind::Chest).item(13, sword()),
        keep,
    );
    let before = totals(&w.state);
    let (window, net) = shown(&w, s, 13);
    let request = w.next_id(s);
    let r = w.request_as(s, request, vec![take_from_menu(13, net)]);
    assert!(!ok(&r), "refused");
    let clicks = heard();
    assert_eq!(clicks.len(), 1);
    assert_eq!(clicks[0].menu(), menu);
    assert_eq!(clicks[0].slot(), 13);
    assert_eq!(clicks[0].item(), &sword());
    assert_eq!(clicks[0].cursor(), &Stack::EMPTY);
    assert_eq!(clicks[0].action(), SlotAction::Take);
    assert_eq!(totals(&w.state), before);
    assert!(w.inv(s).cursor.stack(0).is_empty());
    assert_eq!(w.inv(s).menu.as_ref().unwrap().stacks[13], sword());
    // The same request again is a repeat: no second click.
    let r = w.request_as(s, request, vec![take_from_menu(13, net)]);
    assert!(!ok(&r));
    assert!(heard().is_empty());
    // A stale stack network ID is refused without a click.
    let r = w.request(s, vec![take_from_menu(13, net + 1)]);
    assert!(!ok(&r));
    assert!(heard().is_empty());
    // Placing the player's sticks into the menu: refused, the sticks stay.
    let stick = w.id(s, C::CombinedHotBarAndInventory, 0);
    let r = w.request(
        s,
        vec![place(
            4,
            at(C::CombinedHotBarAndInventory, 0, stick),
            at(C::LevelEntity, 0, 0),
        )],
    );
    assert!(!ok(&r));
    let clicks = heard();
    assert_eq!(clicks.len(), 1);
    assert_eq!(clicks[0].action(), SlotAction::Place);
    assert_eq!(clicks[0].slot(), 0);
    assert_eq!(totals(&w.state), before);
    assert_eq!(w.inv(s).main.stack(0).count(), 10);
    assert_eq!(shown(&w, s, 13), (window, net), "nothing renumbered");
}

#[test]
fn a_handler_changes_menu_items_and_closes_the_menu() {
    fn answer(cx: &mut Context<'_>, player: crate::player::Id, click: &Click) -> Response {
        if click.slot() == 0 {
            return Response::Close;
        }
        let next = Stack::new(item(it::Kind::Stick), u8::try_from(click.slot()).unwrap());
        cx.set_menu_item(player, click.slot(), next).unwrap();
        Response::Keep
    }
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let (_, _) = open(
        &mut w,
        s,
        Menu::new(MenuKind::Chest).item(5, sword()),
        answer,
    );
    let (window, net) = shown(&w, s, 5);
    w.state.tick += 1;
    let request_id = w.next_id(s);
    on_item_stack_request(
        &mut w.state,
        s,
        &ItemStackRequest {
            request_id,
            actions: vec![take_from_menu(5, net)],
            filter_strings: Vec::new(),
            filter_cause: 0,
        },
    );
    let sent = w.sent(s);
    // The refusal and its resend first, then the handler's change.
    let response = sent
        .iter()
        .position(|p| matches!(p, Packet::ItemStackResponse(_)))
        .unwrap();
    let changed = sent
        .iter()
        .rposition(|p| {
            matches!(p, Packet::InventorySlot(i) if i.window_id == u32::from(window) && i.slot == 5)
        })
        .expect("the new item is shown");
    assert!(response < changed);
    let Packet::InventorySlot(slot) = &sent[changed] else {
        panic!("an InventorySlot");
    };
    assert_eq!(slot.new_item.stack.count, 5);
    let (_, new_net) = shown(&w, s, 5);
    assert_ne!(new_net, net);
    assert_eq!(slot.new_item.stack_network_id, new_net);
    assert_eq!(
        w.inv(s).menu.as_ref().unwrap().stacks[5],
        Stack::new(item(it::Kind::Stick), 5)
    );
    // Slot 0 closes it: the blocks show again.
    let [Some((pos, _)), None] = w.player_mut(s).view.menu_blocks() else {
        panic!("one chest");
    };
    let (_, net0) = shown(&w, s, 0);
    let r = w.request(s, vec![take_from_menu(0, net0)]);
    assert!(!ok(&r));
    assert!(w.inv(s).menu.is_none());
    assert_eq!(w.state.menus.len(), 0);
    assert!(w.inv(s).pending_close.is_some(), "a server-side close");
    let got = tick_each(&mut w, Vec::new()).swap_remove(0);
    assert_eq!(blocks_set(&got), [(pos, real(&w, pos))]);
}

#[test]
fn a_client_close_far_player_leave_or_other_window_closes_the_menu() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let chest = AT;
    w.place_block(chest, blk::Kind::Chest.default_state());
    for way in 0..4 {
        let (menu, _) = open(&mut w, s, Menu::new(MenuKind::Chest), keep);
        let [Some((pos, _)), None] = w.player_mut(s).view.menu_blocks() else {
            panic!("one chest");
        };
        match way {
            0 => w.close(s),
            1 => {
                let eye = w.player_mut(s).position;
                w.player_mut(s).position = Vec3::new(eye.x + 20.0, eye.y, eye.z);
            }
            2 => {
                w.open_block(s, chest);
                assert!(matches!(
                    w.inv(s).window.as_ref().map(|w| w.kind),
                    Some(WindowKind::Container { .. })
                ));
            }
            _ => assert!(cx(&mut w).close_window(id(s))),
        }
        let got = tick_each(&mut w, Vec::new()).swap_remove(0);
        w.ack(s);
        assert_ne!(cx(&mut w).menu(id(s)), Some(menu), "way {way}");
        assert!(w.inv(s).menu.is_none());
        assert_eq!(w.state.menus.len(), 0);
        assert!(
            blocks_set(&got).contains(&(pos, real(&w, pos))),
            "way {way}"
        );
        if way == 1 {
            let eye = w.player_mut(s).position;
            w.player_mut(s).position = Vec3::new(eye.x - 20.0, eye.y, eye.z);
        }
        w.close(s);
        w.ack(s);
    }
    let (_, _) = open(&mut w, s, Menu::new(MenuKind::Chest), keep);
    w.tick(vec![leave(1, 90)]);
    assert_eq!(w.state.menus.len(), 0);
}

#[test]
fn opening_and_closing_many_menus_leaves_every_table_at_its_start() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    tick_each(&mut w, Vec::new());
    let next_id = w.inv(s).next_id;
    for i in 0_u8..100 {
        let kind = [MenuKind::Chest, MenuKind::DoubleChest, MenuKind::Hopper][usize::from(i % 3)];
        let (_, _) = open(&mut w, s, Menu::new(kind).item(0, sword()), keep);
        let net = shown(&w, s, 0).1;
        w.request(s, vec![take_from_menu(0, net)]);
        if i % 2 == 0 {
            w.close(s);
        } else {
            assert!(cx(&mut w).close_window(id(s)));
            w.ack(s);
        }
        tick_each(&mut w, Vec::new());
    }
    assert_eq!(heard().len(), 1, "the last menu's click");
    assert!(w.inv(s).menu.is_none() && w.inv(s).window.is_none());
    assert_eq!(w.player_mut(s).view.menu_blocks(), [None, None]);
    assert_eq!(w.player_mut(s).view.fake_count(), 0);
    assert_eq!(w.state.menus.len(), 0);
    assert!(w.player_mut(s).view.out.is_empty());
    assert!(w.inv(s).next_id > next_id, "stack IDs only grow");
}

/// The changes the handlers heard since the last call.
fn changes() -> Vec<m::Change> {
    CHANGES.take()
}

/// Opens `menu` for `s` with a handler that records each click and each change.
/// It answers a click with [`Response::Keep`] and a change with `answer(change)`, and runs the
/// ticks until the window opens.
fn open_editable(w: &mut World, s: SessionId, menu: Menu, answer: Answer) -> m::Id {
    heard();
    changes();
    let menu = cx(w)
        .open_menu(id(s), menu, move |cx, player, input| match input {
            m::Input::Click(click) => {
                record(&click);
                Response::Keep
            }
            m::Input::Change(change) => {
                let mut c = CHANGES.take();
                c.push(change.clone());
                CHANGES.set(c);
                answer(cx, player, &change)
            }
            m::Input::Close(_) => Response::Keep,
        })
        .unwrap();
    for _ in 0..3 {
        w.ack(s);
        tick_each(w, Vec::new());
    }
    assert_eq!(w.inv(s).window.as_ref().map(|w| w.id.is_some()), Some(true));
    menu
}

/// A change handler answer that keeps the change.
fn keep_change(_: &mut Context<'_>, _: crate::player::Id, _: &m::Change) -> Response {
    Response::Keep
}

/// A change handler answer that refuses the change.
fn refuse_change(_: &mut Context<'_>, _: crate::player::Id, _: &m::Change) -> Response {
    Response::Refuse
}

/// The changes as (a menu slot, slot, before, after).
fn described(change: &m::Change) -> Vec<(bool, usize, Stack, Stack)> {
    change
        .changes()
        .iter()
        .map(|c| {
            (
                c.inventory() == crate::event::Inventory::Window,
                c.slot(),
                c.old_item().clone(),
                c.new_item().clone(),
            )
        })
        .collect()
}

/// The totals key of `kind` without damage or extra data.
fn plain(kind: it::Kind) -> TotalKey {
    (Some(item(kind)), 0, String::new())
}

#[test]
fn editable_slots_move_items_as_a_chest_and_the_handler_hears_each_change() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.give(s, sticks(10));
    let menu = Menu::new(MenuKind::Chest)
        .item(13, sword())
        .editable(13)
        .editable(0);
    let opened = open_editable(&mut w, s, menu, keep_change);
    let before = totals(&w.state);
    // The sword onto the cursor.
    let (_, net) = shown(&w, s, 13);
    let r = w.request(s, vec![take_from_menu(13, net)]);
    assert!(ok(&r), "{r:?}");
    assert_eq!(w.inv(s).cursor.stack(0), &sword());
    assert_eq!(cx(&mut w).menu_items(id(s)).unwrap()[13], Stack::EMPTY);
    let heard_changes = changes();
    assert_eq!(heard_changes.len(), 1);
    assert_eq!(heard_changes[0].menu(), opened);
    assert_eq!(
        described(&heard_changes[0]),
        [
            (false, 0, Stack::EMPTY, sword()),
            (true, 13, sword(), Stack::EMPTY)
        ],
        "the cursor and the menu slot"
    );
    // The response names the menu slot as the client knows it.
    let menu_slots: Vec<_> = r
        .container_info
        .iter()
        .filter(|c| c.container.container_id == C::LevelEntity as u8)
        .flat_map(|c| c.slot_info.iter().map(|i| (i.slot, i.count)))
        .collect();
    assert_eq!(menu_slots, [(13, 0)]);
    // Four sticks into slot 0: the menu holds them and the player has six.
    let stick = w.id(s, C::CombinedHotBarAndInventory, 0);
    let r = w.request(
        s,
        vec![place(
            4,
            at(C::CombinedHotBarAndInventory, 0, stick),
            at(C::LevelEntity, 0, 0),
        )],
    );
    assert!(ok(&r), "{r:?}");
    assert_eq!(cx(&mut w).menu_items(id(s)).unwrap()[0], sticks(4));
    assert_eq!(w.inv(s).main.stack(0).count(), 6);
    assert_eq!(
        described(&changes()[0]),
        [
            (false, 0, sticks(10), sticks(6)),
            (true, 0, Stack::EMPTY, sticks(4))
        ]
    );
    assert!(heard().is_empty(), "no clicks");
    // A slot that is not editable is a click and moves nothing, alone or with an editable one.
    let r = w.request(
        s,
        vec![place(
            1,
            at(C::CombinedHotBarAndInventory, 0, stick),
            at(C::LevelEntity, 5, 0),
        )],
    );
    assert!(!ok(&r));
    let (_, net0) = shown(&w, s, 0);
    let r = w.request(
        s,
        vec![take(
            1,
            at(C::LevelEntity, 0, net0),
            at(C::LevelEntity, 5, 0),
        )],
    );
    assert!(!ok(&r));
    let clicks = heard();
    assert_eq!(
        clicks
            .iter()
            .map(|c| (c.slot(), c.action()))
            .collect::<Vec<_>>(),
        [(5, SlotAction::Place), (5, SlotAction::Place)]
    );
    // Items only moved: the player has the sword and six sticks, the menu four sticks.
    let mut after = totals(&w.state);
    *after.entry(plain(it::Kind::Stick)).or_default() += 4;
    let mut expected = before;
    *expected.entry(plain(it::Kind::DiamondSword)).or_default() += 1;
    assert_eq!(after, expected);
}

#[test]
fn the_same_menu_change_twice_leaves_the_state_of_once() {
    let run = |twice: bool| {
        let mut w = World::new(GameMode::Survival);
        let s = w.join(1);
        let menu = Menu::new(MenuKind::Chest).item(3, sword()).editable(3);
        open_editable(&mut w, s, menu, keep_change);
        let (_, net) = shown(&w, s, 3);
        assert!(ok(&w.request_as(s, -1, vec![take_from_menu(3, net)])));
        if twice {
            assert!(!ok(&w.request_as(s, -1, vec![take_from_menu(3, net)])));
        }
        assert_eq!(changes().len(), 1, "the handler hears it once");
        w.state.tick = 1000;
        w.state.hash()
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn a_refused_change_puts_every_slot_back() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.give(s, sticks(10));
    let menu = Menu::new(MenuKind::Hopper).item(1, sword()).editable(1);
    open_editable(&mut w, s, menu, refuse_change);
    let main = w.inv(s).main.clone();
    let (cursor, window) = (w.inv(s).cursor.clone(), w.inv(s).window.clone());
    let items = cx(&mut w).menu_items(id(s)).unwrap().to_vec();
    let stick = w.id(s, C::CombinedHotBarAndInventory, 0);
    let (_, net) = shown(&w, s, 1);
    // The sticks swapped with the sword.
    let r = w.request(
        s,
        vec![swap(
            at(C::CombinedHotBarAndInventory, 0, stick),
            at(C::LevelEntity, 1, net),
        )],
    );
    assert!(!ok(&r));
    assert_eq!(changes().len(), 1, "the handler heard it");
    assert_eq!(w.inv(s).main, main);
    assert_eq!(w.inv(s).cursor, cursor);
    assert_eq!(w.inv(s).window, window, "the menu slots' IDs too");
    assert_eq!(cx(&mut w).menu_items(id(s)).unwrap(), items);
}

// api: PlayerDropItemEvent
#[test]
fn a_drop_from_an_editable_slot_is_a_change_thrown_once_kept() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let menu = Menu::new(MenuKind::Chest).item(0, sticks(4)).editable(0);
    open_editable(&mut w, s, menu, keep_change);
    let (_, net) = shown(&w, s, 0);
    let r = w.request(s, vec![drop_action(1, at(C::LevelEntity, 0, net))]);
    assert!(ok(&r), "{r:?}");
    let heard_changes = changes();
    assert_eq!(heard_changes.len(), 1, "a change, not a click");
    assert!(heard().is_empty());
    assert_eq!(
        described(&heard_changes[0]),
        [(true, 0, sticks(4), sticks(3))]
    );
    assert_eq!(cx(&mut w).menu_items(id(s)).unwrap()[0], sticks(3));
    assert_eq!(dropped(), [sticks(1)], "thrown as from a chest");
    // The whole stack: the slot empties.
    let (_, net) = shown(&w, s, 0);
    let r = w.request(s, vec![drop_action(3, at(C::LevelEntity, 0, net))]);
    assert!(ok(&r), "{r:?}");
    assert_eq!(
        described(&changes()[0]),
        [(true, 0, sticks(3), Stack::EMPTY)]
    );
    assert_eq!(cx(&mut w).menu_items(id(s)).unwrap()[0], Stack::EMPTY);
    assert_eq!(dropped(), [sticks(1), sticks(3)]);
}

#[test]
fn a_refused_drop_from_an_editable_slot_throws_nothing() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let menu = Menu::new(MenuKind::Chest).item(0, sticks(4)).editable(0);
    open_editable(&mut w, s, menu, refuse_change);
    let (window, net) = shown(&w, s, 0);
    assert!(!ok(
        &w.request(s, vec![drop_action(1, at(C::LevelEntity, 0, net))])
    ));
    assert_eq!(changes().len(), 1, "the handler heard it");
    assert!(dropped().is_empty());
    assert_eq!(cx(&mut w).menu_items(id(s)).unwrap()[0], sticks(4));
    assert_eq!(shown(&w, s, 0), (window, net));
    // A world without room for the item: refused before the handler hears of it.
    open_editable(
        &mut w,
        s,
        Menu::new(MenuKind::Chest).item(0, sticks(4)).editable(0),
        keep_change,
    );
    let (_, net) = shown(&w, s, 0);
    set_room(0);
    assert!(!ok(
        &w.request(s, vec![drop_action(1, at(C::LevelEntity, 0, net))])
    ));
    assert!(changes().is_empty(), "no change the world cannot take");
    assert_eq!(cx(&mut w).menu_items(id(s)).unwrap()[0], sticks(4));
}

#[test]
fn the_same_menu_drop_twice_leaves_the_state_of_once() {
    let run = |twice: bool| {
        let mut w = World::new(GameMode::Survival);
        let s = w.join(1);
        let menu = Menu::new(MenuKind::Chest).item(3, sticks(4)).editable(3);
        open_editable(&mut w, s, menu, keep_change);
        let (_, net) = shown(&w, s, 3);
        let drop = vec![drop_action(1, at(C::LevelEntity, 3, net))];
        assert!(ok(&w.request_as(s, -1, drop.clone())));
        if twice {
            assert!(!ok(&w.request_as(s, -1, drop)));
        }
        assert_eq!(changes().len(), 1, "the handler hears it once");
        assert_eq!(dropped(), [sticks(1)], "thrown once");
        w.state.tick = 1000;
        w.state.hash()
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn a_creative_destroy_of_an_editable_slot_is_a_change_made_only_once_kept() {
    let menu = || Menu::new(MenuKind::Chest).item(0, sticks(4)).editable(0);
    let mut w = World::new(GameMode::Creative);
    let s = w.join(1);
    open_editable(&mut w, s, menu(), refuse_change);
    let (window, net) = shown(&w, s, 0);
    assert!(!ok(
        &w.request(s, vec![destroy(4, at(C::LevelEntity, 0, net))])
    ));
    let heard_changes = changes();
    assert_eq!(heard_changes.len(), 1, "a change, not a click");
    assert!(heard().is_empty());
    assert_eq!(
        described(&heard_changes[0]),
        [(true, 0, sticks(4), Stack::EMPTY)]
    );
    assert_eq!(cx(&mut w).menu_items(id(s)).unwrap()[0], sticks(4));
    assert_eq!(shown(&w, s, 0), (window, net), "refused: nothing changed");
    open_editable(&mut w, s, menu(), keep_change);
    let (_, net) = shown(&w, s, 0);
    let r = w.request(s, vec![destroy(4, at(C::LevelEntity, 0, net))]);
    assert!(ok(&r), "{r:?}");
    assert_eq!(changes().len(), 1);
    assert_eq!(cx(&mut w).menu_items(id(s)).unwrap()[0], Stack::EMPTY);
    // Outside creative mode it is refused before the handler hears of it.
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    open_editable(&mut w, s, menu(), keep_change);
    let (_, net) = shown(&w, s, 0);
    assert!(!ok(
        &w.request(s, vec![destroy(4, at(C::LevelEntity, 0, net))])
    ));
    assert!(changes().is_empty());
    assert_eq!(cx(&mut w).menu_items(id(s)).unwrap()[0], sticks(4));
}

#[test]
fn the_handler_hears_a_change_before_it_is_made() {
    fn look(cx: &mut Context<'_>, player: crate::player::Id, _: &m::Change) -> Response {
        let p = cx.state.players.get(&player.session()).unwrap();
        assert_eq!(cx.menu_items(player).unwrap()[2], sword(), "the menu slot");
        assert!(p.inventory.cursor.stack(0).is_empty(), "the cursor");
        Response::Close
    }
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let menu = Menu::new(MenuKind::Chest).item(2, sword()).editable(2);
    open_editable(&mut w, s, menu, look);
    let (_, net) = shown(&w, s, 2);
    assert!(ok(&w.request(s, vec![take_from_menu(2, net)])));
    // Made, then the menu closed: the taken sword goes back to the inventory.
    assert!(w.inv(s).menu.is_none());
    assert_eq!(w.inv(s).main.stack(0), &sword());
    assert!(w.inv(s).cursor.stack(0).is_empty());
}

#[test]
fn a_handler_that_closes_the_menu_and_refuses_leaves_no_copy() {
    fn close(cx: &mut Context<'_>, player: crate::player::Id, _: &m::Change) -> Response {
        assert!(cx.close_window(player));
        Response::Refuse
    }
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let menu = Menu::new(MenuKind::Chest).item(0, sword()).editable(0);
    open_editable(&mut w, s, menu, close);
    let before = totals(&w.state);
    let (_, net) = shown(&w, s, 0);
    assert!(!ok(&w.request(s, vec![take_from_menu(0, net)])));
    assert!(w.inv(s).menu.is_none());
    assert_eq!(
        totals(&w.state),
        before,
        "the handler keeps the sword it refused to give"
    );
}

#[test]
fn a_change_whose_handler_changes_a_slot_it_names_is_refused() {
    fn refill(cx: &mut Context<'_>, player: crate::player::Id, change: &m::Change) -> Response {
        for c in change.changes() {
            if c.inventory() == crate::event::Inventory::Window {
                cx.set_menu_item(player, c.slot(), sticks(1)).unwrap();
            }
        }
        Response::Keep
    }
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let menu = Menu::new(MenuKind::Chest).item(2, sword()).editable(2);
    open_editable(&mut w, s, menu, refill);
    let before = totals(&w.state);
    let (_, net) = shown(&w, s, 2);
    assert!(!ok(&w.request(s, vec![take_from_menu(2, net)])));
    assert_eq!(totals(&w.state), before, "the sword was not taken");
    assert_eq!(cx(&mut w).menu_items(id(s)).unwrap()[2], sticks(1));
}

#[test]
fn a_kept_change_the_handler_made_do_otherwise_is_logged_as_the_handlers() {
    fn fill(cx: &mut Context<'_>, player: crate::player::Id, _: &m::Change) -> Response {
        cx.player_mut(player)
            .unwrap()
            .inventory_mut(crate::inventory::Kind::Main)
            .set(usize::from(TO), sticks(1))
            .unwrap();
        Response::Keep
    }
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    open_editable(
        &mut w,
        s,
        Menu::new(MenuKind::Chest).item(0, sticks(4)).editable(0),
        fill,
    );
    let (_, net) = shown(&w, s, 0);
    let request = -5;
    let actions = vec![take(
        1,
        at(C::LevelEntity, 0, net),
        at(C::CombinedHotBarAndInventory, TO, request),
    )];
    let logged = crate::console::log::tests::captured(|| {
        assert!(!ok(&w.request_as(s, request, actions)));
    });
    let warned: Vec<_> = logged.iter().filter(|l| l.contains("WARN")).collect();
    let [line] = warned.as_slice() else {
        panic!("one warning: {logged:?}");
    };
    assert!(
        line.contains("error=changed by the menu handler"),
        "the handler's, not a listener's: {line}"
    );
    assert_eq!(cx(&mut w).menu_items(id(s)).unwrap()[0], sticks(4));
    assert_eq!(w.inv(s).main.stack(TO), &sticks(1));
}

#[test]
fn a_handler_knows_each_player_slot_a_change_names_though_it_stays() {
    fn write_store(
        cx: &mut Context<'_>,
        player: crate::player::Id,
        change: &m::Change,
    ) -> Response {
        let main = crate::inventory::Kind::Main;
        if change.is_slot_named(
            crate::event::Inventory::Player(main),
            usize::from(STORE_SLOT),
        ) {
            return Response::Refuse;
        }
        cx.player_mut(player)
            .unwrap()
            .inventory_mut(main)
            .set(usize::from(STORE_SLOT), sticks(1))
            .unwrap();
        Response::Keep
    }
    let run = |twice: bool| {
        let mut w = World::new(GameMode::Survival);
        let s = w.join(1);
        w.give(s, sticks(3));
        let menu = Menu::new(MenuKind::Chest).item(0, sword()).editable(0);
        open_editable(&mut w, s, menu, write_store);
        let stick = w.id(s, C::CombinedHotBarAndInventory, STORE_SLOT);
        let (_, net) = shown(&w, s, 0);
        // A crafted request: a stick out of the store's slot and back, then the sword taken.
        let actions = vec![
            take(
                1,
                at(C::CombinedHotBarAndInventory, STORE_SLOT, stick),
                at(C::CombinedHotBarAndInventory, 1, 0),
            ),
            place(
                1,
                at(C::CombinedHotBarAndInventory, 1, -1),
                at(C::CombinedHotBarAndInventory, STORE_SLOT, -1),
            ),
            take_from_menu(0, net),
        ];
        assert!(!ok(&w.request_as(s, -1, actions.clone())));
        if twice {
            assert!(!ok(&w.request_as(s, -1, actions)));
        }
        let heard_changes = changes();
        assert_eq!(heard_changes.len(), 1, "the handler hears it once");
        let main = crate::event::Inventory::Player(crate::inventory::Kind::Main);
        assert!(
            heard_changes[0]
                .changes()
                .iter()
                .all(|c| c.inventory() != main),
            "no main slot changes"
        );
        assert_eq!(
            w.inv(s).main.stack(STORE_SLOT),
            &sticks(3),
            "the store is as it was"
        );
        assert_eq!(cx(&mut w).menu_items(id(s)).unwrap()[0], sword());
        // A change that does not name the store's slot is written and made.
        let (_, net) = shown(&w, s, 0);
        assert!(ok(&w.request_as(s, -3, vec![take_from_menu(0, net)])));
        assert_eq!(w.inv(s).main.stack(STORE_SLOT), &sticks(1));
        assert_eq!(w.inv(s).cursor.stack(0), &sword());
        w.close(s);
        assert_eq!(w.state.menus.len(), 0);
        w.state.tick = 1000;
        w.state.hash()
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn a_handler_refills_a_slot_once_the_change_is_made() {
    fn refill(cx: &mut Context<'_>, player: crate::player::Id, change: &m::Change) -> Response {
        for c in change.changes() {
            if c.inventory() == crate::event::Inventory::Window {
                let (slot, stack, left) = (c.slot(), c.old_item().clone(), c.new_item().clone());
                // As `Change` says: only if no later request changed the slot again.
                cx.defer(move |cx| {
                    if cx.menu_items(player).and_then(|items| items.get(slot)) == Some(&left) {
                        cx.set_menu_item(player, slot, stack).unwrap();
                    }
                })
                .unwrap();
            }
        }
        Response::Keep
    }
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let menu = Menu::new(MenuKind::Chest).item(2, sword()).editable(2);
    open_editable(&mut w, s, menu, refill);
    let (window, net) = shown(&w, s, 2);
    assert!(ok(&w.request(s, vec![take_from_menu(2, net)])));
    crate::tick::dispatch::run_deferred(&mut w.state, &mut 0);
    assert_eq!(w.inv(s).cursor.stack(0), &sword());
    assert_eq!(cx(&mut w).menu_items(id(s)).unwrap()[2], sword());
    let refilled = w.sent(s).into_iter().any(|p| {
        matches!(p, Packet::InventorySlot(p)
            if p.window_id == u32::from(window) && p.slot == 2 && p.new_item.stack.count == 1)
    });
    assert!(refilled, "the client sees the refilled slot");
}

#[test]
fn closing_an_editable_menu_gives_the_cursor_back_and_leaves_the_items_to_the_handler() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.give(s, sticks(10));
    let menu = Menu::new(MenuKind::Chest)
        .item(0, sword())
        .editable(0)
        .editable(1);
    open_editable(&mut w, s, menu, keep_change);
    let (_, net) = shown(&w, s, 0);
    assert!(ok(&w.request(s, vec![take_from_menu(0, net)])));
    let stick = w.id(s, C::CombinedHotBarAndInventory, 0);
    assert!(ok(&w.request(
        s,
        vec![place(
            3,
            at(C::CombinedHotBarAndInventory, 0, stick),
            at(C::LevelEntity, 1, 0),
        )],
    )));
    let heard_changes = changes();
    assert_eq!(
        described(&heard_changes[1])[1],
        (true, 1, Stack::EMPTY, sticks(3))
    );
    w.close(s);
    // The sword on the cursor is the player's again; the sticks are the menu's, which closed
    // with them once its handler had heard of them.
    assert!(w.inv(s).menu.is_none() && w.inv(s).window.is_none());
    assert!(w.inv(s).cursor.stack(0).is_empty());
    let totals = totals(&w.state);
    assert_eq!(totals.get(&plain(it::Kind::DiamondSword)), Some(&1));
    assert_eq!(totals.get(&plain(it::Kind::Stick)), Some(&7));
    assert_eq!(w.state.menus.len(), 0);
}

/// Opens for `s` a menu with a sword in editable slot 0 and a stick in slot 1, a button.
/// A click on it closes the menu, so does a change if `way` says so, and the handler records
/// each close, answering `answer`. The sword is then taken onto the cursor.
fn open_closing(w: &mut World, s: SessionId, way: Way, answer: Response) -> m::Id {
    CLOSES.take();
    let menu = Menu::new(MenuKind::Chest)
        .item(0, sword())
        .editable(0)
        .item(1, sticks(1));
    let menu = cx(w)
        .open_menu(id(s), menu, move |cx, player, input| match input {
            m::Input::Click(_) => Response::Close,
            m::Input::Change(_) if way == Way::ChangeCloses => Response::Close,
            m::Input::Change(_) => Response::Keep,
            m::Input::Close(close) => {
                assert_eq!(cx.menu(player), None, "the menu has closed");
                let mut c = CLOSES.take();
                c.push(close);
                CLOSES.set(c);
                answer
            }
        })
        .unwrap();
    for _ in 0..3 {
        w.ack(s);
        tick_each(w, Vec::new());
    }
    let (_, net) = shown(w, s, 0);
    assert!(ok(&w.request(s, vec![take_from_menu(0, net)])));
    menu
}

/// Closes the menu of `s` as `way` says; `chest` is a chest beside it.
fn close_by(w: &mut World, s: SessionId, way: Way, chest: BlockPos) {
    match way {
        Way::Client => w.close(s),
        Way::Far => {
            let eye = w.player_mut(s).position;
            w.player_mut(s).position = Vec3::new(eye.x + 20.0, eye.y, eye.z);
            tick_each(w, Vec::new());
        }
        Way::OtherWindow => w.open_block(s, chest),
        Way::Death => {
            // Only a spawned player dies; the test client never says it spawned.
            w.player_mut(s).phase = Phase::Spawned;
            assert!(w.player_mut(s).kill());
            w.tick(Vec::new());
        }
        Way::Teleport => {
            let eye = w.player_mut(s).position;
            let feet = Vec3::new(eye.x, eye.y - survival::EYE_HEIGHT, eye.z);
            let id = crate::player::Id::new(s);
            Context {
                state: &mut w.state,
            }
            .teleport(id, crate::world::Id::of(0), feet)
            .unwrap();
            w.tick(Vec::new());
        }
        Way::Leave => {
            // Back in, to count its items.
            let player = w.state.remove(s).unwrap();
            w.state.players.insert(s, player);
        }
        // The change that took the sword closed it.
        Way::ChangeCloses => {}
        Way::ClickCloses => {
            let (_, net) = shown(w, s, 1);
            assert!(!ok(&w.request(s, vec![take_from_menu(1, net)])));
        }
        Way::CloseWindow => assert!(cx(w).close_window(id(s))),
        Way::OtherMenu => {
            let other = Menu::new(MenuKind::Hopper);
            cx(w)
                .open_menu(id(s), other, |_, _, _| Response::Keep)
                .unwrap();
        }
    }
}

#[test]
fn a_closing_menu_hands_its_handler_the_cursor_once_on_every_close_but_a_direct_call() {
    let ways = [
        Way::Client,
        Way::Far,
        Way::OtherWindow,
        Way::Death,
        Way::Teleport,
        Way::Leave,
        Way::ChangeCloses,
        Way::ClickCloses,
        Way::CloseWindow,
        Way::OtherMenu,
    ];
    for way in ways {
        for take in [false, true] {
            let mut listeners = crate::event::Listeners::default();
            listeners.push(
                crate::event::Priority::Normal,
                |e: &mut crate::event::PlayerDeath, _: &mut Context<'_>| {
                    e.set_keep_inventory(true);
                },
            );
            let mut w = World::with_listeners(GameMode::Survival, listeners);
            let s = w.join(1);
            let chest = AT;
            w.place_block(chest, blk::Kind::Chest.default_state());
            let answer = if take {
                Response::Refuse
            } else {
                Response::Keep
            };
            let menu = open_closing(&mut w, s, way, answer);
            close_by(&mut w, s, way, chest);
            let heard = CLOSES.take();
            let direct = matches!(way, Way::CloseWindow | Way::OtherMenu);
            if direct {
                assert!(heard.is_empty(), "{way:?}: a direct call calls no handler");
            } else {
                let [close] = heard.as_slice() else {
                    panic!("{way:?}: one close: {heard:?}");
                };
                assert_eq!(close.menu(), menu);
                assert_eq!(close.cursor(), &sword(), "{way:?}");
                assert_eq!(close.is_by_player(), way == Way::Client, "{way:?}");
            }
            assert_ne!(cx(&mut w).menu(id(s)), Some(menu), "{way:?}");
            assert!(w.inv(s).cursor.stack(0).is_empty(), "{way:?}");
            let swords = totals(&w.state)
                .get(&plain(it::Kind::DiamondSword))
                .copied()
                .unwrap_or(0);
            let taken = u64::from(take && !direct);
            assert_eq!(
                swords + taken,
                1,
                "{way:?}, taken {take}: the sword is the player's or the handler's, once"
            );
        }
    }
}

#[test]
fn a_close_s_handler_cannot_hold_more_than_the_waiting_bound_allows() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let menu = Menu::new(MenuKind::Chest).item(0, sword()).editable(0);
    let fills_cursor = |cx: &mut Context<'_>, player: crate::player::Id, input: m::Input| {
        if let m::Input::Close(_) = input {
            let p = cx.player_mut(player).unwrap();
            let wrote = p
                .inventory_mut(crate::inventory::Kind::Cursor)
                .set(0, sticks(1));
            WROTE.set(Some(wrote));
        }
        Response::Keep
    };
    cx(&mut w).open_menu(id(s), menu, fills_cursor).unwrap();
    for _ in 0..3 {
        w.ack(s);
        tick_each(&mut w, Vec::new());
    }
    let (_, net) = shown(&w, s, 0);
    assert!(ok(&w.request(s, vec![take_from_menu(0, net)])));
    // The sword on the cursor and the waiting stacks fill the bound.
    let waiting = MAX_SAVED_PENDING - 1;
    w.player_mut(s).inventory.pending = vec![sticks(1); waiting];
    w.close(s);
    assert_eq!(
        WROTE.take(),
        Some(Err(crate::inventory::Error::Full)),
        "the sword the close took out still counts"
    );
    let inv = w.inv(s);
    assert!(inv.cursor.stack(0).is_empty());
    assert_eq!(inv.pending.len(), waiting);
}

#[test]
fn a_menu_a_far_close_s_handler_opens_for_another_far_player_stays_open() {
    let mut w = World::new(GameMode::Survival);
    let (a, b) = (w.join(1), w.join(2));
    let opens_for_b = move |cx: &mut Context<'_>, _: crate::player::Id, input: m::Input| {
        if let m::Input::Close(_) = input {
            cx.open_menu(id(b), Menu::new(MenuKind::Hopper), |_, _, _| Response::Keep)
                .unwrap();
        }
        Response::Keep
    };
    cx(&mut w)
        .open_menu(id(a), Menu::new(MenuKind::Chest), opens_for_b)
        .unwrap();
    let old = cx(&mut w)
        .open_menu(id(b), Menu::new(MenuKind::Chest), |_, _, _| Response::Keep)
        .unwrap();
    for _ in 0..3 {
        w.ack(a);
        w.ack(b);
        tick_each(&mut w, Vec::new());
    }
    for s in [a, b] {
        let eye = w.player_mut(s).position;
        w.player_mut(s).position = Vec3::new(eye.x + 20.0, eye.y, eye.z);
    }
    tick_each(&mut w, Vec::new());
    assert_eq!(cx(&mut w).menu(id(a)), None, "far away, A's menu closed");
    let new = cx(&mut w).menu(id(b));
    assert!(
        new.is_some_and(|m| m != old),
        "player B's new menu, opened beside it, stays open: {new:?}"
    );
}

#[test]
fn a_leaving_player_s_menu_closes_after_the_quit_listeners_and_monitors_and_before_the_save() {
    fn quit_heard() {
        assert!(CLOSES.take().is_empty(), "the menu is still open");
        QUITS.set(QUITS.get() + 1);
    }
    let mut listeners = crate::event::Listeners::default();
    listeners.push(
        crate::event::Priority::Normal,
        |_: &mut crate::event::PlayerQuit, _: &mut Context<'_>| quit_heard(),
    );
    listeners.push_monitor(|_: &crate::event::PlayerQuit, _: &Context<'_>| quit_heard());
    let mut w = World::with_listeners(GameMode::Survival, listeners);
    let s = w.join(1);
    w.state.saving.players = true;
    let menu = open_closing(&mut w, s, Way::Leave, Response::Keep);
    w.state.leave(s, crate::event::QuitReason::Left);
    assert_eq!(
        QUITS.get(),
        2,
        "the listener and the monitor heard the quit"
    );
    let heard = CLOSES.take();
    assert!(
        matches!(heard.as_slice(), [close] if close.menu() == menu),
        "{heard:?}"
    );
    let saved = &w.state.saving.offline.values().next().unwrap().data;
    let Some(Value::List(List::Compound(stacks))) = saved.extra.get("Inventory") else {
        panic!("{:?}", saved.extra.get("Inventory"));
    };
    assert!(
        stacks
            .iter()
            .filter_map(Stack::from_nbt)
            .any(|s| s == sword()),
        "the sword the handler kept is saved with the player"
    );
}

#[test]
fn a_quit_listener_s_teleport_closes_the_menu_before_the_quit_monitors() {
    let mut listeners = crate::event::Listeners::default();
    listeners.push(
        crate::event::Priority::Normal,
        |e: &mut crate::event::PlayerQuit, cx: &mut Context<'_>| {
            let p = cx.player(e.player()).unwrap();
            let (world, feet) = (p.world(), p.position());
            cx.teleport(e.player(), world, feet).unwrap();
        },
    );
    listeners.push_monitor(|_: &crate::event::PlayerQuit, _: &Context<'_>| {
        let closes = CLOSES.take();
        AT_MONITOR.set(Some(closes.len()));
        CLOSES.set(closes);
    });
    let mut w = World::with_listeners(GameMode::Survival, listeners);
    let s = w.join(1);
    let menu = open_closing(&mut w, s, Way::Leave, Response::Keep);
    w.state.leave(s, crate::event::QuitReason::Left);
    assert_eq!(AT_MONITOR.get(), Some(1), "closed before the monitors");
    let heard = CLOSES.take();
    assert!(
        matches!(heard.as_slice(), [close] if close.menu() == menu),
        "closed once: {heard:?}"
    );
}

#[test]
fn an_inventory_transaction_names_menu_slots_as_the_window_and_may_cancel() {
    let mut listeners = crate::event::Listeners::default();
    listeners.push(
        crate::event::Priority::Normal,
        |e: &mut crate::event::InventoryTransaction, _: &mut Context<'_>| {
            assert!(
                e.changes()
                    .iter()
                    .any(|c| c.inventory() == crate::event::Inventory::Window && c.slot() == 4)
            );
            TRANSACTIONS.set(TRANSACTIONS.get() + 1);
            e.cancel();
        },
    );
    let mut w = World::with_listeners(GameMode::Survival, listeners);
    let s = w.join(1);
    let menu = Menu::new(MenuKind::Chest).item(4, sword()).editable(4);
    open_editable(&mut w, s, menu, keep_change);
    let (_, net) = shown(&w, s, 4);
    assert!(!ok(&w.request(s, vec![take_from_menu(4, net)])));
    assert_eq!(TRANSACTIONS.get(), 1, "the listener heard the request");
    assert!(changes().is_empty(), "a cancelled request changes nothing");
    assert_eq!(cx(&mut w).menu_items(id(s)).unwrap()[4], sword());
    assert!(w.inv(s).cursor.stack(0).is_empty());
}

#[test]
fn editable_menus_opened_changed_and_closed_leave_every_table_at_its_start() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    tick_each(&mut w, Vec::new());
    for i in 0_u8..60 {
        let kind = [MenuKind::Chest, MenuKind::DoubleChest, MenuKind::Hopper][usize::from(i % 3)];
        let menu = Menu::new(kind).item(0, sticks(1)).editable(0);
        open_editable(&mut w, s, menu, keep_change);
        let net = shown(&w, s, 0).1;
        assert!(ok(&w.request(s, vec![take_from_menu(0, net)])));
        if i % 2 == 0 {
            w.close(s);
        } else {
            assert!(cx(&mut w).close_window(id(s)));
            w.ack(s);
        }
        tick_each(&mut w, Vec::new());
    }
    assert_eq!(changes().len(), 1, "the last menu's change");
    assert!(w.inv(s).menu.is_none() && w.inv(s).window.is_none());
    assert_eq!(w.player_mut(s).view.menu_blocks(), [None, None]);
    assert_eq!(w.player_mut(s).view.fake_count(), 0);
    assert_eq!(w.state.menus.len(), 0);
    assert!(w.player_mut(s).view.out.is_empty());
    assert!(w.inv(s).pending.is_empty());
    assert_eq!(
        w.inv(s).main.stack(0),
        &sticks(60),
        "every stick taken is kept"
    );
}

#[test]
fn an_editable_slot_past_the_end_opens_nothing() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let menu = Menu::new(MenuKind::Hopper).editable(5);
    assert_eq!(
        cx(&mut w).open_menu(id(s), menu, |_, _, _| Response::Keep),
        Err(m::Error::NoSlot)
    );
    assert_eq!(w.state.menus.len(), 0);
}

/// Opens for `s` a lending menu holding `stacks` in editable slots and a sword button in slot 26.
/// It runs the ticks until the window opens. Its handler keeps its sticks in [`STORE`] as the
/// lent counts say ([`m::Change::lent_before`]), records each change and close, answers a change
/// with [`ANSWER`] (or closes the menu if `closes`) and a close with `on_close`, taking the
/// cursor's own sticks on [`Response::Refuse`]; a click closes the menu.
fn open_lending(
    w: &mut World,
    s: SessionId,
    stacks: &[Stack],
    closes: bool,
    on_close: Response,
) -> m::Id {
    changes();
    CLOSES.take();
    ANSWER.set(Response::Keep);
    let held: u8 = stacks.iter().map(Stack::count).sum();
    STORE.set(i64::from(held));
    let menu = stacks
        .iter()
        .enumerate()
        .fold(Menu::new(MenuKind::Chest).lending(), |menu, (i, stack)| {
            menu.item(i, stack.clone()).editable(i)
        })
        .item(26, sword());
    let menu = cx(w)
        .open_menu(id(s), menu, move |_, _, input| match input {
            m::Input::Click(_) => Response::Close,
            m::Input::Change(change) => {
                let answer = ANSWER.get();
                if answer != Response::Refuse {
                    let window: i64 = change
                        .changes()
                        .iter()
                        .filter(|c| c.inventory() == crate::event::Inventory::Window)
                        .map(|c| i64::from(c.new_item().count()) - i64::from(c.old_item().count()))
                        .sum();
                    let lent = i64::from(change.lent_after()) - i64::from(change.lent_before());
                    STORE.set(STORE.get() + window + lent);
                }
                let mut c = CHANGES.take();
                c.push(change);
                CHANGES.set(c);
                if closes { Response::Close } else { answer }
            }
            m::Input::Close(close) => {
                if on_close == Response::Refuse {
                    let own = close.cursor().count() - close.lent();
                    STORE.set(STORE.get() + i64::from(own));
                }
                let mut c = CLOSES.take();
                c.push(close);
                CLOSES.set(c);
                on_close
            }
        })
        .unwrap();
    for _ in 0..3 {
        w.ack(s);
        tick_each(w, Vec::new());
    }
    menu
}

/// How many sticks `stacks` hold.
fn count_sticks<'a>(stacks: impl IntoIterator<Item = &'a Stack>) -> u64 {
    stacks
        .into_iter()
        .filter(|st| st.can_stack_with(&sticks(1)))
        .map(|st| u64::from(st.count()))
        .sum()
}

/// The sticks the player's saved data holds, and those a player loaded from it holds.
/// That is what a commit keeps and a restart finds.
fn saved_sticks(w: &World, s: SessionId) -> (u64, u64) {
    let mut extra = Compound::new();
    save(w.inv(s), &mut extra);
    let saved: Vec<Stack> = extra
        .values()
        .filter_map(|v| match v {
            Value::List(List::Compound(list)) => Some(list),
            _ => None,
        })
        .flatten()
        .filter_map(Stack::from_nbt)
        .collect();
    let mut loaded = Inventory::default();
    load(&mut loaded, &extra, &w.state.custom, "player");
    restore(&mut loaded);
    let restored = count_sticks(loaded.main.stacks.iter().chain(&loaded.pending));
    (count_sticks(&saved), restored)
}

/// Asserts that the sticks are `total`, each once.
/// They are those the player's saved data holds, those it threw and the handler's; the handler
/// keeps the menu's sticks and the lent ones.
fn check_once(w: &mut World, s: SessionId, total: i64) {
    let (saved, restored) = saved_sticks(w, s);
    assert_eq!(saved, restored, "a restart finds what was saved");
    let thrown = count_sticks(&dropped());
    let player = i64::try_from(saved + thrown).unwrap();
    assert_eq!(player + STORE.get(), total, "every stick once");
    let lent = w.inv(s).lent;
    if let Some(items) = cx(w).menu_items(id(s)) {
        let shown = i64::try_from(count_sticks(items)).unwrap();
        assert_eq!(
            shown + i64::from(lent),
            STORE.get(),
            "the handler keeps the menu's sticks and the lent ones"
        );
    }
}

/// The lent counts of the changes the handler heard since the last call.
fn lent_heard() -> Vec<(u8, u8)> {
    changes()
        .iter()
        .map(|c| (c.lent_before(), c.lent_after()))
        .collect()
}

#[test]
fn a_lending_menu_keeps_taken_items_the_handler_s_until_they_leave_the_cursor() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.give(s, sticks(4));
    open_lending(&mut w, s, &[sticks(8), Stack::EMPTY], false, Response::Keep);
    check_once(&mut w, s, 12);
    let cursor = |w: &World| (w.inv(s).cursor.stack(0).count(), w.inv(s).lent);
    // Five taken up are lent: the handler keeps them, the player's data does not.
    let (_, net) = shown(&w, s, 0);
    let r = w.request(
        s,
        vec![take(5, at(C::LevelEntity, 0, net), at(C::Cursor, 0, 0))],
    );
    assert!(ok(&r), "{r:?}");
    assert_eq!(lent_heard(), [(0, 5)], "a pickup is heard");
    assert_eq!(cursor(&w), (5, 5));
    check_once(&mut w, s, 12);
    // Two put into the menu are lent ones.
    let net = w.id(s, C::Cursor, 0);
    let r = w.request(
        s,
        vec![place(2, at(C::Cursor, 0, net), at(C::LevelEntity, 1, 0))],
    );
    assert!(ok(&r), "{r:?}");
    assert_eq!(lent_heard(), [(5, 3)]);
    assert_eq!(cursor(&w), (3, 3));
    check_once(&mut w, s, 12);
    // The player's own four join them, heard as the cursor holds lent items.
    let (own, net) = (
        w.id(s, C::CombinedHotBarAndInventory, 0),
        w.id(s, C::Cursor, 0),
    );
    let r = w.request(
        s,
        vec![take(
            4,
            at(C::CombinedHotBarAndInventory, 0, own),
            at(C::Cursor, 0, net),
        )],
    );
    assert!(ok(&r), "{r:?}");
    assert_eq!(lent_heard(), [(3, 3)], "nothing lent moved");
    assert_eq!(cursor(&w), (7, 3));
    check_once(&mut w, s, 12);
    // Five put into the inventory are the player's own four, then one lent.
    let net = w.id(s, C::Cursor, 0);
    let r = w.request(
        s,
        vec![place(
            5,
            at(C::Cursor, 0, net),
            at(C::CombinedHotBarAndInventory, 1, 0),
        )],
    );
    assert!(ok(&r), "{r:?}");
    assert_eq!(lent_heard(), [(3, 2)], "a request naming no menu slot");
    assert_eq!(cursor(&w), (2, 2));
    check_once(&mut w, s, 12);
    // A refused drop of a lent one throws nothing.
    ANSWER.set(Response::Refuse);
    let net = w.id(s, C::Cursor, 0);
    assert!(!ok(
        &w.request(s, vec![drop_action(1, at(C::Cursor, 0, net))])
    ));
    assert_eq!(lent_heard(), [(2, 1)]);
    assert_eq!(cursor(&w), (2, 2));
    assert!(dropped().is_empty());
    check_once(&mut w, s, 12);
    ANSWER.set(Response::Keep);
    let net = w.id(s, C::Cursor, 0);
    assert!(ok(
        &w.request(s, vec![drop_action(1, at(C::Cursor, 0, net))])
    ));
    assert_eq!(lent_heard(), [(2, 1)]);
    assert_eq!(cursor(&w), (1, 1));
    check_once(&mut w, s, 12);
    // The lent one stays the handler's as the player closes the menu.
    w.close(s);
    let heard = CLOSES.take();
    assert!(
        matches!(heard.as_slice(), [close] if close.lent() == 1 && close.cursor() == &sticks(1)),
        "{heard:?}"
    );
    assert_eq!(cursor(&w), (0, 0));
    check_once(&mut w, s, 12);
}

#[test]
fn a_swap_of_the_cursor_lends_a_menu_slot_s_stack_only_for_lent_items() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.give(s, sticks(5));
    open_lending(&mut w, s, &[sticks(3)], false, Response::Keep);
    let cursor = |w: &World| (w.inv(s).cursor.stack(0).count(), w.inv(s).lent);
    let own = w.id(s, C::CombinedHotBarAndInventory, 0);
    let r = w.request(
        s,
        vec![take(
            5,
            at(C::CombinedHotBarAndInventory, 0, own),
            at(C::Cursor, 0, 0),
        )],
    );
    assert!(ok(&r), "{r:?}");
    let swap_with_menu = |w: &mut World| {
        let (net, menu_net) = (w.id(s, C::Cursor, 0), shown(w, s, 0).1);
        let r = w.request(
            s,
            vec![swap(at(C::Cursor, 0, net), at(C::LevelEntity, 0, menu_net))],
        );
        assert!(ok(&r), "{r:?}");
    };
    // The player's own five for the menu's three, as in a container: nothing lent.
    swap_with_menu(&mut w);
    assert_eq!(lent_heard(), [(0, 0)]);
    assert_eq!(cursor(&w), (3, 0));
    check_once(&mut w, s, 8);
    // With lent ones on the cursor, the stack it gets is lent.
    let (_, net) = shown(&w, s, 0);
    let r = w.request(
        s,
        vec![take(
            2,
            at(C::LevelEntity, 0, net),
            at(C::Cursor, 0, w.id(s, C::Cursor, 0)),
        )],
    );
    assert!(ok(&r), "{r:?}");
    assert_eq!(lent_heard(), [(0, 2)]);
    swap_with_menu(&mut w);
    assert_eq!(lent_heard(), [(2, 3)]);
    assert_eq!(cursor(&w), (3, 3));
    check_once(&mut w, s, 8);
    // A checkpoint keeps the lent count.
    let mut writer = checkpoint::Writer::default();
    w.inv(s).encode(&mut writer);
    let (bytes, opaque) = writer.into_parts();
    assert!(opaque.is_none());
    let entities = entity::Customs::default();
    let mut reader = checkpoint::Reader::new(&bytes, &w.state.custom, &entities);
    assert_eq!(Inventory::decode(&mut reader).unwrap().lent, 3);
    let net = w.id(s, C::Cursor, 0);
    let r = w.request(
        s,
        vec![swap(
            at(C::Cursor, 0, net),
            at(C::CombinedHotBarAndInventory, 0, 0),
        )],
    );
    assert!(ok(&r), "{r:?}");
    assert_eq!(
        lent_heard(),
        [(3, 0)],
        "the three left for the player's slot"
    );
    assert_eq!(cursor(&w), (0, 0));
    check_once(&mut w, s, 8);
}

#[test]
fn a_move_or_swap_of_the_cursor_with_itself_keeps_its_lent_items() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.give(s, sticks(2));
    open_lending(&mut w, s, &[sticks(3)], false, Response::Keep);
    let own = w.id(s, C::CombinedHotBarAndInventory, 0);
    assert!(ok(&w.request(
        s,
        vec![take(
            2,
            at(C::CombinedHotBarAndInventory, 0, own),
            at(C::Cursor, 0, 0),
        )],
    )));
    let (cursor, (_, net)) = (w.id(s, C::Cursor, 0), shown(&w, s, 0));
    assert!(ok(&w.request(
        s,
        vec![take(
            3,
            at(C::LevelEntity, 0, net),
            at(C::Cursor, 0, cursor)
        )],
    )));
    assert_eq!(lent_heard(), [(0, 3)]);
    let net = w.id(s, C::Cursor, 0);
    for action in [
        take(5, at(C::Cursor, 0, net), at(C::Cursor, 0, net)),
        swap(at(C::Cursor, 0, net), at(C::Cursor, 0, net)),
    ] {
        let r = w.request(s, vec![action]);
        assert!(ok(&r), "{r:?}");
        assert!(lent_heard().is_empty(), "nothing lent moved");
        assert_eq!((w.inv(s).cursor.stack(0), w.inv(s).lent), (&sticks(5), 3));
        check_once(&mut w, s, 5);
    }
}

/// Opens a lending menu of three sticks for a player with four of its own in main slot 0.
/// The three are then taken onto its cursor.
fn lend_three(w: &mut World) -> SessionId {
    let s = w.join(1);
    w.give(s, sticks(4));
    open_lending(w, s, &[sticks(3)], false, Response::Keep);
    let (_, net) = shown(w, s, 0);
    assert!(ok(&w.request(
        s,
        vec![take(3, at(C::LevelEntity, 0, net), at(C::Cursor, 0, 0))],
    )));
    assert_eq!(lent_heard(), [(0, 3)]);
    s
}

/// The player's four sticks onto a cursor of three lent ones.
fn take_own_onto_lent(w: &World, s: SessionId) -> StackRequestAction {
    take(
        4,
        at(
            C::CombinedHotBarAndInventory,
            0,
            w.id(s, C::CombinedHotBarAndInventory, 0),
        ),
        at(C::Cursor, 0, w.id(s, C::Cursor, 0)),
    )
}

#[test]
fn a_request_changing_a_cursor_with_lent_items_is_heard_and_may_be_refused() {
    let mut w = World::new(GameMode::Survival);
    let s = lend_three(&mut w);
    let cursor = |w: &World| (w.inv(s).cursor.stack(0).count(), w.inv(s).lent);
    for (answer, after) in [(Response::Refuse, (3, 3)), (Response::Keep, (7, 3))] {
        ANSWER.set(answer);
        let r = w.request(s, vec![take_own_onto_lent(&w, s)]);
        assert_eq!(ok(&r), answer == Response::Keep, "{answer:?}");
        assert_eq!(lent_heard(), [(3, 3)], "{answer:?}: the player's own join");
        assert_eq!(cursor(&w), after, "{answer:?}");
        check_once(&mut w, s, 7);
    }
    // A swap with a player's slot gives the lent ones to the player.
    for (answer, after) in [(Response::Refuse, (7, 3)), (Response::Keep, (0, 0))] {
        ANSWER.set(answer);
        let net = w.id(s, C::Cursor, 0);
        let r = w.request(
            s,
            vec![swap(
                at(C::Cursor, 0, net),
                at(C::CombinedHotBarAndInventory, 1, 0),
            )],
        );
        assert_eq!(ok(&r), answer == Response::Keep, "{answer:?}");
        assert_eq!(lent_heard(), [(3, 0)], "{answer:?}");
        assert_eq!(cursor(&w), after, "{answer:?}");
        check_once(&mut w, s, 7);
    }
}

#[test]
fn a_request_not_touching_a_cursor_with_lent_items_is_not_heard() {
    let mut w = World::new(GameMode::Survival);
    let s = lend_three(&mut w);
    // A handler that heard it would refuse it.
    ANSWER.set(Response::Refuse);
    let own = w.id(s, C::CombinedHotBarAndInventory, 0);
    let r = w.request(
        s,
        vec![place(
            4,
            at(C::CombinedHotBarAndInventory, 0, own),
            at(C::CombinedHotBarAndInventory, 1, 0),
        )],
    );
    assert!(ok(&r), "{r:?}");
    assert!(lent_heard().is_empty());
    assert_eq!(w.inv(s).lent, 3);
    check_once(&mut w, s, 7);
}

#[test]
fn the_same_request_onto_a_lent_cursor_twice_leaves_the_state_of_once() {
    let run = |twice: bool| {
        let mut w = World::new(GameMode::Survival);
        let s = lend_three(&mut w);
        let action = take_own_onto_lent(&w, s);
        assert!(ok(&w.request_as(s, -9, vec![action.clone()])));
        if twice {
            assert!(!ok(&w.request_as(s, -9, vec![action])));
        }
        assert_eq!(lent_heard(), [(3, 3)], "the handler hears it once");
        w.state.tick = 1000;
        w.state.hash()
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn a_creative_destroy_off_a_lent_cursor_deletes_the_player_s_own_first() {
    let mut w = World::new(GameMode::Creative);
    let s = w.join(1);
    w.give(s, sticks(2));
    open_lending(&mut w, s, &[sticks(3)], false, Response::Keep);
    let own = w.id(s, C::CombinedHotBarAndInventory, 0);
    assert!(ok(&w.request(
        s,
        vec![take(
            2,
            at(C::CombinedHotBarAndInventory, 0, own),
            at(C::Cursor, 0, 0),
        )],
    )));
    let (cursor, (_, net)) = (w.id(s, C::Cursor, 0), shown(&w, s, 0));
    assert!(ok(&w.request(
        s,
        vec![take(
            3,
            at(C::LevelEntity, 0, net),
            at(C::Cursor, 0, cursor)
        )],
    )));
    assert_eq!(lent_heard(), [(0, 3)]);
    // Four destroyed are the player's own two, then two lent.
    for (answer, after, store) in [(Response::Refuse, (5, 3), 3), (Response::Keep, (1, 1), 1)] {
        ANSWER.set(answer);
        let net = w.id(s, C::Cursor, 0);
        let r = w.request(s, vec![destroy(4, at(C::Cursor, 0, net))]);
        assert_eq!(ok(&r), answer == Response::Keep, "{answer:?}");
        assert_eq!(lent_heard(), [(3, 1)], "{answer:?}");
        let inv = w.inv(s);
        assert_eq!((inv.cursor.stack(0).count(), inv.lent), after, "{answer:?}");
        assert_eq!(STORE.get(), store, "{answer:?}");
    }
}

#[test]
fn a_lending_menu_s_lent_items_stay_the_handler_s_on_every_close() {
    let ways = [
        Way::Client,
        Way::Far,
        Way::OtherWindow,
        Way::Death,
        Way::Teleport,
        Way::Leave,
        Way::ChangeCloses,
        Way::ClickCloses,
        Way::CloseWindow,
        Way::OtherMenu,
    ];
    for way in ways {
        for on_close in [Response::Keep, Response::Refuse] {
            let mut listeners = crate::event::Listeners::default();
            listeners.push(
                crate::event::Priority::Normal,
                |e: &mut crate::event::PlayerDeath, _: &mut Context<'_>| {
                    e.set_keep_inventory(true);
                },
            );
            let mut w = World::with_listeners(GameMode::Survival, listeners);
            let s = w.join(1);
            let chest = AT;
            w.place_block(chest, blk::Kind::Chest.default_state());
            w.give(s, sticks(2));
            open_lending(&mut w, s, &[sticks(3)], way == Way::ChangeCloses, on_close);
            // The player's own two onto the cursor, then the menu's three, lent.
            let own = w.id(s, C::CombinedHotBarAndInventory, 0);
            assert!(ok(&w.request(
                s,
                vec![take(
                    2,
                    at(C::CombinedHotBarAndInventory, 0, own),
                    at(C::Cursor, 0, 0),
                )],
            )));
            let (cursor, (_, net)) = (w.id(s, C::Cursor, 0), shown(&w, s, 0));
            assert!(ok(&w.request(
                s,
                vec![take(
                    3,
                    at(C::LevelEntity, 0, net),
                    at(C::Cursor, 0, cursor)
                )],
            )));
            if way == Way::ClickCloses {
                let (_, button) = shown(&w, s, 26);
                assert!(!ok(&w.request(s, vec![take_from_menu(26, button)])));
            } else {
                close_by(&mut w, s, way, chest);
            }
            let heard = CLOSES.take();
            let direct = matches!(way, Way::CloseWindow | Way::OtherMenu);
            if direct {
                assert!(heard.is_empty(), "{way:?}: a direct call calls no handler");
            } else {
                let [close] = heard.as_slice() else {
                    panic!("{way:?}: one close: {heard:?}");
                };
                assert_eq!((close.cursor(), close.lent()), (&sticks(5), 3), "{way:?}");
            }
            assert_eq!(
                (w.inv(s).cursor.stack(0), w.inv(s).lent),
                (&Stack::EMPTY, 0),
                "{way:?}"
            );
            let player = totals(&w.state)
                .get(&plain(it::Kind::Stick))
                .copied()
                .unwrap_or(0);
            let own = if on_close == Response::Refuse && !direct {
                0
            } else {
                2
            };
            assert_eq!(
                player, own,
                "{way:?}, {on_close:?}: the player's own sticks only"
            );
            assert_eq!(
                i64::try_from(player).unwrap() + STORE.get(),
                5,
                "{way:?}, {on_close:?}: every stick once"
            );
        }
    }
}

#[test]
fn a_direct_close_after_taking_the_cursor_s_own_items_doubles_none() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    assert_eq!(cx(&mut w).menu_lent(id(s)), None, "no menu open");
    w.give(s, sticks(2));
    open_lending(&mut w, s, &[sticks(3)], false, Response::Keep);
    assert_eq!(cx(&mut w).menu_lent(id(s)), Some(0));
    // The player's own two onto the cursor, then the menu's three, lent.
    let own = w.id(s, C::CombinedHotBarAndInventory, 0);
    assert!(ok(&w.request(
        s,
        vec![take(
            2,
            at(C::CombinedHotBarAndInventory, 0, own),
            at(C::Cursor, 0, 0),
        )],
    )));
    let (cursor, (_, net)) = (w.id(s, C::Cursor, 0), shown(&w, s, 0));
    assert!(ok(&w.request(
        s,
        vec![take(
            3,
            at(C::LevelEntity, 0, net),
            at(C::Cursor, 0, cursor)
        )],
    )));
    // The caller keeps the cursor's own items, leaving it the lent ones, and closes the menu.
    let lent = cx(&mut w).menu_lent(id(s)).unwrap();
    assert_eq!(lent, 3);
    let stack = w.inv(s).cursor.stack(0).clone();
    let kept = stack.count() - lent;
    w.player_mut(s)
        .inventory_mut(crate::inventory::Kind::Cursor)
        .set(0, stack.with_count(lent))
        .unwrap();
    assert!(cx(&mut w).close_window(id(s)));
    assert_eq!(cx(&mut w).menu_lent(id(s)), None);
    let player = totals(&w.state)
        .get(&plain(it::Kind::Stick))
        .copied()
        .unwrap_or(0);
    assert_eq!(player, 0, "the caller took the player's own sticks");
    assert_eq!(i64::from(kept) + STORE.get(), 5, "every stick once");
}

#[test]
fn the_same_lending_take_twice_leaves_the_state_of_once() {
    let run = |twice: bool| {
        let mut w = World::new(GameMode::Survival);
        let s = w.join(1);
        open_lending(&mut w, s, &[sticks(3)], false, Response::Keep);
        let (_, net) = shown(&w, s, 0);
        assert!(ok(&w.request_as(s, -1, vec![take_from_menu(0, net)])));
        if twice {
            assert!(!ok(&w.request_as(s, -1, vec![take_from_menu(0, net)])));
        }
        assert_eq!(lent_heard(), [(0, 1)], "the handler hears it once");
        assert_eq!(w.inv(s).lent, 1);
        w.state.tick = 1000;
        w.state.hash()
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn a_cursor_write_keeps_lent_only_what_it_still_holds_of_their_item() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    open_lending(&mut w, s, &[sticks(5)], false, Response::Keep);
    let (_, net) = shown(&w, s, 0);
    assert!(ok(&w.request(
        s,
        vec![take(5, at(C::LevelEntity, 0, net), at(C::Cursor, 0, 0))],
    )));
    let write = |w: &mut World, stack: Stack| {
        w.player_mut(s)
            .inventory_mut(crate::inventory::Kind::Cursor)
            .set(0, stack)
            .unwrap();
        (w.inv(s).lent, saved_sticks(w, s).0)
    };
    assert_eq!(
        write(&mut w, sticks(7)),
        (5, 2),
        "two more are the player's"
    );
    assert_eq!(
        write(&mut w, sticks(3)),
        (3, 0),
        "the player's own go first"
    );
    assert_eq!(write(&mut w, sword()), (0, 0), "another item lends none");
    assert_eq!(write(&mut w, sticks(3)), (0, 3));
}

#[test]
fn lending_menus_opened_taken_and_closed_leave_every_table_at_its_start() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    tick_each(&mut w, Vec::new());
    for i in 0_u8..60 {
        open_lending(&mut w, s, &[sticks(1)], false, Response::Keep);
        let net = shown(&w, s, 0).1;
        assert!(ok(&w.request(s, vec![take_from_menu(0, net)])));
        assert_eq!(w.inv(s).lent, 1);
        if i % 2 == 0 {
            w.close(s);
        } else {
            assert!(cx(&mut w).close_window(id(s)));
            w.ack(s);
        }
        tick_each(&mut w, Vec::new());
        assert_eq!(STORE.get(), 1, "the handler kept its stick");
    }
    assert!(w.inv(s).menu.is_none() && w.inv(s).window.is_none());
    assert_eq!(w.inv(s).lent, 0);
    assert_eq!(w.player_mut(s).view.menu_blocks(), [None, None]);
    assert_eq!(w.player_mut(s).view.fake_count(), 0);
    assert_eq!(w.state.menus.len(), 0);
    assert!(w.player_mut(s).view.out.is_empty());
    assert!(w.inv(s).pending.is_empty());
    assert_eq!(
        totals(&w.state).get(&plain(it::Kind::Stick)),
        None,
        "a lent stick never became the player's"
    );
}
