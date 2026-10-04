//! Tests of fake blocks and menu windows (PLAN.md 10.8 "메뉴 창", "가짜 블록"): who is sent
//! what and when, the bounds, and that a menu never moves an item.

use std::cell::Cell;

use super::super::super::Context;
use super::super::super::tests::{input, packet};
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

/// The blocks `packets` set: (position, network ID), from `UpdateBlock` and
/// `UpdateSubChunkBlocks`.
fn blocks_set(packets: &[Packet]) -> Vec<(BlockPos, u32)> {
    let mut out = Vec::new();
    for p in packets {
        match p {
            Packet::UpdateBlock(u) => {
                let [x, y, z] = u.position;
                out.push((BlockPos::new(x, y, z), u.new_block_runtime_id));
            }
            Packet::UpdateSubChunkBlocks(u) => {
                for b in &u.blocks {
                    let [x, y, z] = b.block_pos;
                    out.push((BlockPos::new(x, y, z), b.block_runtime_id));
                }
            }
            _ => {}
        }
    }
    out
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
        .open_menu(id(s), menu, move |cx, player, click| {
            record(&click);
            response(cx, player, &click)
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
