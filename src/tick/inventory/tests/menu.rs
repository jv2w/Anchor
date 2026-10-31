//! Tests of fake blocks and menu windows: who is sent what and when, the bounds, and that a menu
//! never moves an item.

use std::cell::Cell;

use super::super::super::Context;
use super::super::super::tests::{blocks_set, input, packet};
use super::super::super::view::MAX_FAKE_BLOCKS;
use super::*;
use crate::menu::{self as m, Action, Click, Kind as MenuKind, Menu, Response};
use crate::protocol::consts::ContainerType;
use crate::protocol::packet::SubChunkRequest;

thread_local! {
    /// The clicks the handlers of a test heard.
    static CLICKS: Cell<Vec<Click>> = const { Cell::new(Vec::new()) };
}

fn heard() -> Vec<Click> {
    CLICKS.take()
}

fn record(click: &Click) {
    let mut c = CLICKS.take();
    c.push(click.clone());
    CLICKS.set(c);
}

fn glass() -> blk::State {
    blk::Kind::Glass.default_state()
}

fn cx(w: &mut World) -> Context<'_> {
    Context {
        state: &mut w.state,
    }
}

fn id(s: SessionId) -> crate::player::Id {
    crate::player::Id::new(s)
}

/// One tick; the packets each joined session got, in join order.
fn tick_each(w: &mut World, inputs: Vec<Input>) -> Vec<Vec<Packet>> {
    w.tick_each(inputs)
}

/// A block near the player, in a chunk its client was sent.
fn near(w: &mut World, s: SessionId) -> BlockPos {
    let eye = w.player(s).position;
    let at = BlockPos::containing(eye);
    let pos = BlockPos::new(at.x + 2, -61, at.z + 2);
    assert!(w.player(s).view.sees(pos.chunk()));
    pos
}

fn real(w: &World, pos: BlockPos) -> u32 {
    w.state.worlds[0].block(pos).unwrap().network_id()
}

// pmmp: World::createBlockUpdatePackets
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
    assert_eq!(w.player(a).view.fake_count(), 1);
    w.tick(vec![super::super::super::tests::leave(1, 90)]);
    assert!(!w.state.players.contains_key(&a));
    assert_eq!(
        cx(&mut w).send_fake_block(id(a), extra, glass()),
        Err(m::Error::Offline)
    );
    assert!(!cx(&mut w).clear_fake_blocks(id(a)));
}

/// Opens `menu` for `s` with a handler that records each click and answers `response(click)`,
/// and runs the ticks until its window opens; the packets `s` got meanwhile.
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
    assert_eq!(w.inv(s).window.as_ref().map(|w| w.id != 0), Some(true));
    (menu, got)
}

fn keep(_: &mut Context<'_>, _: crate::player::Id, _: &Click) -> Response {
    Response::Keep
}

fn sword() -> Stack {
    Stack::new(item(it::Kind::DiamondSword), 1)
}

/// The menu window's ID and the stack network ID of menu slot `slot`.
fn shown(w: &World, s: SessionId, slot: usize) -> (u8, i32) {
    let window = w.inv(s).window.as_ref().unwrap();
    (window.id, window.ids[slot].net)
}

// pmmp: Player::setCurrentWindow
#[test]
fn a_menu_opens_on_fake_blocks_with_its_title_then_its_window() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    tick_each(&mut w, Vec::new());
    let menu = Menu::new(MenuKind::Chest).title("Shop").item(13, sword());
    let opened = cx(&mut w).open_menu(id(s), menu, |_, _, _| Response::Keep);
    let menu_id = opened.unwrap();
    assert_eq!(cx(&mut w).menu(id(s)), Some(menu_id));
    assert_eq!(
        w.player(s).window(),
        None,
        "events and windows do not name menus"
    );
    let [Some((pos, chest)), None] = w.player(s).view.menu_blocks() else {
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
        Some(&Value::String("Shop".into()))
    );
    assert_eq!(
        data[0].nbt_data.get("id"),
        Some(&Value::String("Chest".into()))
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

#[test]
fn double_chest_and_hopper_menus_have_their_blocks_slots_and_type() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let (_, got) = open(&mut w, s, Menu::new(MenuKind::DoubleChest), keep);
    let [Some((left, _)), Some((right, _))] = w.player(s).view.menu_blocks() else {
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
    let [Some((_, hopper)), None] = w.player(s).view.menu_blocks() else {
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
            Menu::new(MenuKind::Chest).title("x".repeat(1025)),
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
    w.player(s).inventory.next_menu = u64::from(u32::MAX);
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
    assert_eq!(clicks[0].action(), Action::Take);
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
    assert_eq!(clicks[0].action(), Action::Place);
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
    let [Some((pos, _)), None] = w.player(s).view.menu_blocks() else {
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
    let chest = BlockPos::new(3, -60, 3);
    w.place_block(chest, blk::Kind::Chest.default_state());
    for way in 0..4 {
        let (menu, _) = open(&mut w, s, Menu::new(MenuKind::Chest), keep);
        let [Some((pos, _)), None] = w.player(s).view.menu_blocks() else {
            panic!("one chest");
        };
        match way {
            0 => w.close(s),
            1 => {
                let eye = w.player(s).position;
                w.player(s).position = Vec3::new(eye.x + 20.0, eye.y, eye.z);
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
            let eye = w.player(s).position;
            w.player(s).position = Vec3::new(eye.x - 20.0, eye.y, eye.z);
        }
        w.close(s);
        w.ack(s);
    }
    let (_, _) = open(&mut w, s, Menu::new(MenuKind::Chest), keep);
    w.tick(vec![super::super::super::tests::leave(1, 90)]);
    assert_eq!(w.state.menus.len(), 0);
}

#[test]
fn opening_and_closing_many_menus_leaves_every_table_at_its_start() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    tick_each(&mut w, Vec::new());
    let next_id = w.inv(s).next_id;
    for i in 0..100u8 {
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
    assert_eq!(w.player(s).view.menu_blocks(), [None, None]);
    assert_eq!(w.player(s).view.fake_count(), 0);
    assert_eq!(w.state.menus.len(), 0);
    assert!(w.player(s).view.out.is_empty());
    assert!(w.inv(s).next_id > next_id, "stack IDs only grow");
}

thread_local! {
    /// The changes the handlers of a test heard.
    static CHANGES: Cell<Vec<m::Change>> = const { Cell::new(Vec::new()) };
}

fn changes() -> Vec<m::Change> {
    CHANGES.take()
}

/// What an editable menu's handler answers a change with.
type Answer = fn(&mut Context<'_>, crate::player::Id, &m::Change) -> Response;

/// Opens `menu` for `s` with a handler that records each click (answering [`Response::Keep`]) and
/// each change (answering `answer(change)`), and runs the ticks until its window opens.
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
    assert_eq!(w.inv(s).window.as_ref().map(|w| w.id != 0), Some(true));
    menu
}

fn keep_change(_: &mut Context<'_>, _: crate::player::Id, _: &m::Change) -> Response {
    Response::Keep
}

fn refuse_change(_: &mut Context<'_>, _: crate::player::Id, _: &m::Change) -> Response {
    Response::Refuse
}

/// The changes as (a menu slot, slot, before, after).
fn described(change: &m::Change) -> Vec<(bool, u8, Stack, Stack)> {
    change
        .changes()
        .iter()
        .map(|c| {
            (
                c.inventory() == crate::event::Inventory::Window,
                c.slot(),
                c.before().clone(),
                c.after().clone(),
            )
        })
        .collect()
}

fn sticks(n: u8) -> Stack {
    Stack::new(item(it::Kind::Stick), n)
}

/// The totals key of `kind` without damage or extra data.
fn plain(kind: it::Kind) -> (Option<Type>, u16, String) {
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
        [(5, Action::Place), (5, Action::Place)]
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

// pmmp: PlayerDropItemEvent
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
                cx.set_menu_item(player, usize::from(c.slot()), sticks(1))
                    .unwrap();
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
    /// The main slot the request puts a stick into, named by the request's own ID (no stack
    /// network ID to go stale).
    const TO: u8 = 5;
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
    /// The player's slot the handler writes with each change, as a box keeps a menu's items.
    const STORE: u8 = 0;
    fn write_store(
        cx: &mut Context<'_>,
        player: crate::player::Id,
        change: &m::Change,
    ) -> Response {
        let main = crate::inventory::Kind::Main;
        if change.is_slot_named(crate::event::Inventory::Player(main), STORE) {
            return Response::Refuse;
        }
        cx.player_mut(player)
            .unwrap()
            .inventory_mut(main)
            .set(usize::from(STORE), sticks(1))
            .unwrap();
        Response::Keep
    }
    let run = |twice: bool| {
        let mut w = World::new(GameMode::Survival);
        let s = w.join(1);
        w.give(s, sticks(3));
        let menu = Menu::new(MenuKind::Chest).item(0, sword()).editable(0);
        open_editable(&mut w, s, menu, write_store);
        let stick = w.id(s, C::CombinedHotBarAndInventory, STORE);
        let (_, net) = shown(&w, s, 0);
        // A crafted request: a stick out of the store's slot and back, then the sword taken.
        let actions = vec![
            take(
                1,
                at(C::CombinedHotBarAndInventory, STORE, stick),
                at(C::CombinedHotBarAndInventory, 1, 0),
            ),
            place(
                1,
                at(C::CombinedHotBarAndInventory, 1, -1),
                at(C::CombinedHotBarAndInventory, STORE, -1),
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
            w.inv(s).main.stack(STORE),
            &sticks(3),
            "the store is as it was"
        );
        assert_eq!(cx(&mut w).menu_items(id(s)).unwrap()[0], sword());
        // A change that does not name the store's slot is written and made.
        let (_, net) = shown(&w, s, 0);
        assert!(ok(&w.request_as(s, -3, vec![take_from_menu(0, net)])));
        assert_eq!(w.inv(s).main.stack(STORE), &sticks(1));
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
                let (slot, stack, left) =
                    (usize::from(c.slot()), c.before().clone(), c.after().clone());
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

thread_local! {
    /// The closes the handlers of a test heard.
    static CLOSES: Cell<Vec<m::Close>> = const { Cell::new(Vec::new()) };
}

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

/// Opens for `s` a menu with a sword in editable slot 0 and a stick in slot 1, a button: a click
/// on it closes the menu, so does a change if `way` says so, and the handler records each close,
/// answering `answer`. The sword is then taken onto the cursor.
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
            let eye = w.player(s).position;
            w.player(s).position = Vec3::new(eye.x + 20.0, eye.y, eye.z);
            tick_each(w, Vec::new());
        }
        Way::OtherWindow => w.open_block(s, chest),
        Way::Death => {
            // Only a spawned player dies; the test client never says it spawned.
            w.player(s).phase = super::super::super::Phase::Spawned;
            assert!(w.player(s).kill());
            w.tick(Vec::new());
        }
        Way::Teleport => {
            let eye = w.player(s).position;
            let feet = Vec3::new(
                eye.x,
                eye.y - super::super::super::survival::EYE_HEIGHT,
                eye.z,
            );
            w.player(s).teleport(crate::world::Id::of(0), feet).unwrap();
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
            let chest = BlockPos::new(3, -60, 3);
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
fn an_inventory_transaction_names_menu_slots_as_the_window_and_may_cancel() {
    thread_local! {
        static HEARD: Cell<u32> = const { Cell::new(0) };
    }
    let mut listeners = crate::event::Listeners::default();
    listeners.push(
        crate::event::Priority::Normal,
        |e: &mut crate::event::InventoryTransaction, _: &mut Context<'_>| {
            assert!(
                e.changes()
                    .iter()
                    .any(|c| c.inventory() == crate::event::Inventory::Window && c.slot() == 4)
            );
            HEARD.set(HEARD.get() + 1);
            e.cancel();
        },
    );
    let mut w = World::with_listeners(GameMode::Survival, listeners);
    let s = w.join(1);
    let menu = Menu::new(MenuKind::Chest).item(4, sword()).editable(4);
    open_editable(&mut w, s, menu, keep_change);
    let (_, net) = shown(&w, s, 4);
    assert!(!ok(&w.request(s, vec![take_from_menu(4, net)])));
    assert_eq!(HEARD.get(), 1, "the listener heard the request");
    assert!(changes().is_empty(), "a cancelled request changes nothing");
    assert_eq!(cx(&mut w).menu_items(id(s)).unwrap()[4], sword());
    assert!(w.inv(s).cursor.stack(0).is_empty());
}

#[test]
fn editable_menus_opened_changed_and_closed_leave_every_table_at_its_start() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    tick_each(&mut w, Vec::new());
    for i in 0..60u8 {
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
    assert_eq!(w.player(s).view.menu_blocks(), [None, None]);
    assert_eq!(w.player(s).view.fake_count(), 0);
    assert_eq!(w.state.menus.len(), 0);
    assert!(w.player(s).view.out.is_empty());
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
