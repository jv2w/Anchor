//! Tests of the inventory, chest and trade events.
//!
//! The listener sees the right fields, cancelling keeps the game and resyncs the client, changed
//! fields change the outcome, monitors see the applied state, and a listener that changes the
//! game so the check fails cancels the action.

use std::cell::Cell;

use super::*;
use crate::event::{
    ChestPair, Event, Inventory as Where, InventoryClose, InventoryOpen, InventoryTransaction,
    PlayerBlockPick, PlayerDropItem, PlayerItemHeld, Priority, Trade, TradeOpen, Window as Shown,
};
use crate::inventory::Kind;
use crate::math::Vec3;
use crate::nbt::{List, Value};
use crate::protocol::packet::{BlockPickRequest, MobEquipment};
use crate::tick::tests::leave;
use crate::tick::{Context, block_entity, entity, survival, trade};
use crate::world;

/// The position of the blocks the tests place, next to the spawn.
const AT: BlockPos = BlockPos::new(3, -60, 3);
/// The request container of the main inventory and the hotbar together.
const COMBINED: C = C::CombinedHotBarAndInventory;

thread_local! {
    /// What the listeners of a test saw, in order.
    static LOG: Cell<Vec<String>> = const { Cell::new(Vec::new()) };
}

/// Records what a listener saw, for the test to compare.
fn log(line: String) {
    let mut lines = LOG.take();
    lines.push(line);
    LOG.set(lines);
}

/// Takes what the listeners logged.
fn logged() -> Vec<String> {
    LOG.take()
}

/// Listeners with one listener of `E` at normal priority.
fn on<E: Event>(listener: impl FnMut(&mut E, &mut Context<'_>) + Send + 'static) -> Listeners {
    let mut l = Listeners::default();
    l.push(Priority::Normal, listener);
    l
}

/// A survival world with one player holding `stacks` from main slot 0.
fn world_with(listeners: Listeners, stacks: &[Stack]) -> (World, SessionId) {
    LOG.set(Vec::new());
    let mut w = World::with_listeners(GameMode::Survival, listeners);
    let s = w.join(1);
    for stack in stacks {
        w.give(s, stack.clone());
    }
    survival::add_xp(w.player_mut(s), 100_000);
    w.sent(s);
    (w, s)
}

/// Moves `count` from main slot `from` into request slot `to`.
fn put(w: &mut World, s: SessionId, from: u8, count: u8, to: (C, u8)) {
    let id = w.id(s, COMBINED, from);
    let r = w.request(
        s,
        vec![place(count, at(COMBINED, from, id), at(to.0, to.1, 0))],
    );
    assert!(ok(&r), "{r:?}");
}

/// A stack of `count` sticks.
fn sticks(count: u8) -> Stack {
    Stack::new(item(it::Kind::Stick), count)
}

// api: InventoryTransactionEvent
#[test]
fn a_request_is_one_transaction_with_its_slot_changes_and_monitors_see_it_applied() {
    let mut l = on(|e: &mut InventoryTransaction, _: &mut Context<'_>| {
        let changes: Vec<_> = e
            .changes()
            .iter()
            .map(|c| {
                (
                    c.inventory(),
                    c.slot(),
                    c.before().count(),
                    c.after().count(),
                )
            })
            .collect();
        log(format!("{changes:?}"));
    });
    l.push_monitor(|e: &InventoryTransaction, cx: &Context<'_>| {
        let p = &cx.state.players[&e.player().session()];
        log(format!("applied {}", p.inventory.main.stack(0).count()));
    });
    let (mut w, s) = world_with(l, &[sticks(10)]);
    w.open_inventory(s);
    let id = w.id(s, COMBINED, 0);
    let r = w.request(s, vec![place(3, at(COMBINED, 0, id), at(COMBINED, 2, 0))]);
    assert!(ok(&r));
    assert_eq!(
        logged(),
        [
            format!(
                "{:?}",
                [
                    (Where::Player(Kind::Main), 0, 10, 7),
                    (Where::Player(Kind::Main), 2, 0, 3)
                ]
            ),
            "applied 7".to_owned()
        ]
    );
}

/// Sends a request; every packet the player got for it.
fn send(w: &mut World, s: SessionId, id: i32, actions: Vec<StackRequestAction>) -> Vec<Packet> {
    w.state.tick += 1;
    let request = ItemStackRequest {
        request_id: id,
        actions,
        filter_strings: Vec::new(),
        filter_cause: 0,
    };
    on_item_stack_request(&mut w.state, s, &request);
    w.sent(s)
}

/// Whether `packets` hold an ok response.
fn accepted(packets: &[Packet]) -> bool {
    packets.iter().any(|p| match p {
        Packet::ItemStackResponse(r) => r.responses.iter().all(ok),
        _ => false,
    })
}

// api: InventoryTransactionEvent
#[test]
fn a_cancelled_transaction_changes_nothing_and_resends_the_inventory() {
    let (mut w, s) = world_with(
        on(|e: &mut InventoryTransaction, _: &mut Context<'_>| e.cancel()),
        &[sticks(10)],
    );
    w.open_inventory(s);
    let snap = snapshot(&w.state);
    let id = w.id(s, COMBINED, 0);
    let packets = send(
        &mut w,
        s,
        -1,
        vec![place(3, at(COMBINED, 0, id), at(COMBINED, 2, 0))],
    );
    assert!(!accepted(&packets));
    assert!(
        packets
            .iter()
            .any(|p| matches!(p, Packet::InventoryContent(_))),
        "the client gets its slots again"
    );
    assert_eq!(snapshot(&w.state), snap);
}

// api: InventoryTransactionEvent
#[test]
fn a_listener_that_moves_a_slot_the_request_reads_refuses_it() {
    let (mut w, s) = world_with(
        on(|e: &mut InventoryTransaction, cx: &mut Context<'_>| {
            let p = cx.state.players.get_mut(&e.player().session()).unwrap();
            p.inventory_mut(Kind::Main).swap(0, 1).unwrap();
        }),
        &[sticks(10), Stack::new(item(it::Kind::Coal), 5)],
    );
    w.open_inventory(s);
    let before = totals(&w.state);
    let id = w.id(s, COMBINED, 0);
    let r = w.request(s, vec![place(3, at(COMBINED, 0, id), at(COMBINED, 3, 0))]);
    assert!(!ok(&r), "slot 0 holds coal now");
    assert_eq!(totals(&w.state), before);
    assert_eq!(w.inv(s).main.stack(0), &Stack::new(item(it::Kind::Coal), 5));
    // A change to slots the request does not read lets it through.
    let (mut w, s) = world_with(
        on(|e: &mut InventoryTransaction, cx: &mut Context<'_>| {
            let p = cx.state.players.get_mut(&e.player().session()).unwrap();
            p.inventory_mut(Kind::Main).swap(1, 2).unwrap();
        }),
        &[sticks(10), Stack::new(item(it::Kind::Coal), 5)],
    );
    w.open_inventory(s);
    let id = w.id(s, COMBINED, 0);
    let r = w.request(s, vec![place(3, at(COMBINED, 0, id), at(COMBINED, 3, 0))]);
    assert!(ok(&r));
    assert_eq!(w.inv(s).main.stack(3), &sticks(3));
}

// api: PlayerDropItemEvent
#[test]
fn drops_raise_player_drop_item_and_cancelling_keeps_the_item() {
    let (mut w, s) = world_with(
        on(|e: &mut PlayerDropItem, _: &mut Context<'_>| log(format!("{:?}", e.item()))),
        &[sticks(10)],
    );
    let id = w.id(s, C::HotBar, 0);
    assert!(ok(&w.request(s, vec![drop_action(4, at(C::HotBar, 0, id))])));
    assert!(drop_from_main(&mut w.state, s, 0, 1));
    assert_eq!(
        logged(),
        [format!("{:?}", sticks(4)), format!("{:?}", sticks(1))]
    );
    assert_eq!(dropped(), [sticks(4), sticks(1)]);

    let (mut w, s) = world_with(
        on(|e: &mut PlayerDropItem, _: &mut Context<'_>| e.cancel()),
        &[sticks(10)],
    );
    let id = w.id(s, C::HotBar, 0);
    assert!(!ok(
        &w.request(s, vec![drop_action(4, at(C::HotBar, 0, id))])
    ));
    assert!(!drop_from_main(&mut w.state, s, 0, 1));
    assert!(
        w.sent(s)
            .iter()
            .any(|p| matches!(p, Packet::InventorySlot(_))),
        "the legacy drop's slot is sent again"
    );
    assert_eq!(w.inv(s).main.stack(0), &sticks(10));
    assert!(dropped().is_empty());
    // A listener that takes the item away cancels the drop.
    let (mut w, s) = world_with(
        on(|e: &mut PlayerDropItem, cx: &mut Context<'_>| {
            let p = cx.state.players.get_mut(&e.player().session()).unwrap();
            p.inventory_mut(Kind::Main).swap(0, 5).unwrap();
        }),
        &[sticks(10)],
    );
    assert!(!drop_from_main(&mut w.state, s, 0, 1));
    assert_eq!(w.inv(s).main.stack(5), &sticks(10));
    assert!(dropped().is_empty());
}

/// A villager selling one bread for two apples, beside the player; its ID.
fn villager(w: &mut World) -> crate::entity::Id {
    let apple = Stack::new(item(it::Kind::Apple), 2);
    let bread = Stack::new(item(it::Kind::Bread), 1);
    let offer = crate::nbt::Compound::from([
        ("buyA".to_owned(), Value::Compound(apple.to_nbt())),
        ("sell".to_owned(), Value::Compound(bread.to_nbt())),
        ("maxUses".to_owned(), Value::Int(12)),
    ]);
    let offers = trade::Offers::from_nbt(
        &crate::nbt::Compound::from([(
            "Recipes".to_owned(),
            Value::List(List::Compound(vec![offer])),
        )]),
        &crate::custom::Registry::default(),
    );
    let npc = entity::Npc::Villager {
        profession: 1,
        offers,
    };
    let id = entity::spawn_npc(&mut w.state, 0, Vec3::new(0.5, -60.0, 2.5), npc).unwrap();
    w.tick(Vec::new());
    id
}

#[test]
fn trading_raises_trade_open_and_trade_and_cancelling_keeps_the_payment() {
    let mut l =
        on(|e: &mut TradeOpen, _: &mut Context<'_>| log(format!("open {:?}", e.villager())));
    l.push(Priority::Normal, |e: &mut Trade, _: &mut Context<'_>| {
        log(format!(
            "trade {} {} {:?} {:?}",
            e.offer(),
            e.uses(),
            e.inputs()[0],
            e.output()
        ));
        if e.uses() > 1 {
            e.cancel();
        }
    });
    let apples = Stack::new(item(it::Kind::Apple), 10);
    let (mut w, s) = world_with(l, &[apples]);
    let v = villager(&mut w);
    assert!(trade::open(&mut w.state, s, v));
    put(&mut w, s, 0, 10, (C::TradeTwoIngredientOne, 4));
    let net = (1..)
        .take(1 << 21)
        .find(|id| trade::offer(&w.state, s, *id).is_some())
        .unwrap();
    let trade_times = |w: &mut World, times: u8| {
        let pay = w.id(s, C::TradeTwoIngredientOne, 4);
        let id = w.next_id(s);
        let actions = vec![
            craft(net, times),
            consume(2 * times, at(C::TradeTwoIngredientOne, 4, pay)),
            place(times, created(id), at(COMBINED, 5, w.id(s, COMBINED, 5))),
        ];
        w.request_as(s, id, actions)
    };
    assert!(ok(&trade_times(&mut w, 1)));
    let before = totals(&w.state);
    assert!(
        !ok(&trade_times(&mut w, 2)),
        "the listener cancels two trades"
    );
    assert_eq!(totals(&w.state), before);
    let price = Stack::new(item(it::Kind::Apple), 2);
    let bread = Stack::new(item(it::Kind::Bread), 1);
    assert_eq!(
        logged(),
        [
            format!("open {v:?}"),
            format!("trade 0 1 {price:?} {bread:?}"),
            format!("trade 0 2 {price:?} {bread:?}"),
        ]
    );
    assert_eq!(w.inv(s).main.stack(5), &bread);

    let (mut w, s) = world_with(on(|e: &mut TradeOpen, _: &mut Context<'_>| e.cancel()), &[]);
    let v = villager(&mut w);
    assert!(!trade::open(&mut w.state, s, v));
    assert!(w.inv(s).window.is_none());
}

/// A quick trade pays from the main inventory, the payment slots or both, exactly the price.
/// It is `CraftRecipeAuto`: the trade button, a right click on the arrow.
#[test]
fn a_quick_trade_pays_from_the_inventory_and_the_payment_slots() {
    let (mut w, s) = world_with(
        Listeners::default(),
        &[Stack::new(item(it::Kind::Apple), 10)],
    );
    let v = villager(&mut w);
    assert!(trade::open(&mut w.state, s, v));
    let net = (1..)
        .take(1 << 21)
        .find(|id| trade::offer(&w.state, s, *id).is_some())
        .unwrap();
    let bread = |n| Stack::new(item(it::Kind::Bread), n);
    let to_bread =
        |w: &World, n: u8, id: i32| place(n, created(id), at(COMBINED, 5, w.id(s, COMBINED, 5)));

    let id = w.next_id(s);
    let apples = w.id(s, COMBINED, 0);
    let paid = w.request_as(
        s,
        id,
        vec![
            auto(net, 2),
            consume(4, at(COMBINED, 0, apples)),
            to_bread(&w, 2, id),
        ],
    );
    assert!(ok(&paid));
    assert_eq!(w.inv(s).main.stack(0).count(), 6);
    assert_eq!(w.inv(s).main.stack(5), &bread(2));

    put(&mut w, s, 0, 1, (C::TradeTwoIngredientOne, 4));
    let id = w.next_id(s);
    let (apples, slot) = (w.id(s, COMBINED, 0), w.id(s, C::TradeTwoIngredientOne, 4));
    let mixed = vec![
        auto(net, 1),
        consume(1, at(C::TradeTwoIngredientOne, 4, slot)),
        consume(1, at(COMBINED, 0, apples)),
        to_bread(&w, 1, id),
    ];
    assert!(ok(&w.request_as(s, id, mixed)));
    assert_eq!(w.inv(s).main.stack(0).count(), 4);
    assert_eq!(w.inv(s).main.stack(5), &bread(3));

    let before = totals(&w.state);
    let id = w.next_id(s);
    let apples = w.id(s, COMBINED, 0);
    let overpaid = vec![
        auto(net, 1),
        consume(3, at(COMBINED, 0, apples)),
        to_bread(&w, 1, id),
    ];
    assert!(
        !ok(&w.request_as(s, id, overpaid)),
        "one trade takes two apples"
    );
    assert_eq!(totals(&w.state), before);
}

/// Unpairing or pairing chests through the API closes the windows on them without an event.
/// A direct call raises none.
#[test]
fn the_chest_pair_api_closes_windows_without_close_events() {
    let (mut w, s) = world_with(
        on(|e: &mut InventoryClose, _: &mut Context<'_>| {
            log(format!("close {:?}", e.window()));
        }),
        &[],
    );
    let other = w.double_chest(AT);
    w.open_block(s, AT);
    let mut cx = Context {
        state: &mut w.state,
    };
    let mut chest = cx.block_entity_mut(world::Id::DEFAULT, AT).unwrap();
    chest.unpair().unwrap();
    assert!(w.inv(s).window.is_none(), "unpairing closes the window");
    w.open_block(s, AT);
    let mut cx = Context {
        state: &mut w.state,
    };
    let mut chest = cx.block_entity_mut(world::Id::DEFAULT, AT).unwrap();
    chest.pair_with(other).unwrap();
    assert!(w.inv(s).window.is_none(), "pairing closes the window");
    assert_eq!(logged(), Vec::<String>::new());
}

/// A chest the world removes (a task, a command) closes the windows on it without an event.
#[test]
fn a_chest_removed_by_the_world_closes_windows_without_close_events() {
    let (mut w, s) = world_with(
        on(|e: &mut InventoryClose, _: &mut Context<'_>| {
            log(format!("close {:?}", e.window()));
        }),
        &[],
    );
    w.double_chest(AT);
    w.open_block(s, AT);
    w.state.worlds[0].set_block(AT, blk::AIR).unwrap();
    block_entity::removed(&mut w.state, 0, AT);
    assert!(w.inv(s).window.is_none());
    assert_eq!(logged(), Vec::<String>::new());
}

// api: InventoryOpenEvent, InventoryCloseEvent
#[test]
fn windows_raise_open_and_close_and_a_cancelled_or_broken_open_stays_closed() {
    let mut l =
        on(|e: &mut InventoryOpen, _: &mut Context<'_>| log(format!("open {:?}", e.window())));
    l.push(
        Priority::Normal,
        |e: &mut InventoryClose, _: &mut Context<'_>| {
            log(format!("close {:?}", e.window()));
        },
    );
    let (mut w, s) = world_with(l, &[]);
    w.place_block(AT, blk::Kind::Barrel.default_state());
    w.open_block(s, AT);
    w.close(s);
    w.open_inventory(s);
    let barrel = Shown::Block {
        world: world::Id::DEFAULT,
        pos: AT,
    };
    assert_eq!(
        logged(),
        [
            format!("open {barrel:?}"),
            format!("close {barrel:?}"),
            format!("open {:?}", Shown::Own)
        ]
    );
    assert_eq!(w.player_mut(s).window(), Some(Shown::Own));

    let (mut w, s) = world_with(
        on(|e: &mut InventoryOpen, _: &mut Context<'_>| e.cancel()),
        &[],
    );
    w.place_block(AT, blk::Kind::Barrel.default_state());
    w.open_block(s, AT);
    assert!(w.inv(s).window.is_none());
    w.open_inventory(s);
    assert!(w.inv(s).window.is_none());

    // A listener that breaks the block: the window cannot open any more.
    let (mut w, s) = world_with(
        on(|_: &mut InventoryOpen, cx: &mut Context<'_>| {
            cx.state.worlds[0].set_block(AT, blk::AIR).unwrap();
        }),
        &[],
    );
    w.place_block(AT, blk::Kind::Barrel.default_state());
    w.open_block(s, AT);
    assert!(w.inv(s).window.is_none());

    // A listener that changes the hand: the interaction must not go on with another item.
    let (mut w, s) = world_with(
        on(|e: &mut InventoryOpen, cx: &mut Context<'_>| {
            let p = cx.state.players.get_mut(&e.player().session()).unwrap();
            p.inventory_mut(Kind::Main).set(0, Stack::EMPTY).unwrap();
        }),
        &[sticks(3)],
    );
    w.place_block(AT, blk::Kind::Barrel.default_state());
    w.open_block(s, AT);
    assert!(w.inv(s).window.is_none());
}

/// A close listener that opens a menu for player 1, and monitors that log what opened and closed.
fn menu_on_close() -> Listeners {
    let mut l = on(|_: &mut InventoryClose, cx: &mut Context<'_>| {
        let player = crate::player::Id::new(SessionId::new(1));
        let menu = crate::menu::Menu::new(crate::menu::Kind::Chest);
        cx.open_menu(player, menu, |_, _, _| crate::menu::Response::Keep)
            .expect("the menu opens");
    });
    l.push_monitor(|e: &InventoryOpen, _: &Context<'_>| log(format!("opened {:?}", e.window())));
    l.push_monitor(|e: &InventoryClose, _: &Context<'_>| log(format!("closed {:?}", e.window())));
    l
}

// api: InventoryCloseEvent
#[test]
fn a_menu_a_close_listener_opens_stays_open_in_place_of_the_closed_window() {
    let barrel = Shown::Block {
        world: world::Id::DEFAULT,
        pos: AT,
    };
    for way in ["client close", "another window"] {
        let (mut w, s) = world_with(menu_on_close(), &[]);
        w.place_block(AT, blk::Kind::Barrel.default_state());
        w.open_block(s, AT);
        if way == "client close" {
            w.close(s);
        } else {
            w.open_inventory(s);
        }
        assert!(
            matches!(
                w.inv(s).window.as_ref().map(|w| w.kind),
                Some(WindowKind::Menu { .. })
            ),
            "{way}: the listener's menu is the window"
        );
        assert!(w.inv(s).menu.is_some(), "{way}");
        assert_eq!(w.state.menus.len(), 1, "{way}");
        assert!(
            w.state.rules.block_entities.viewers(0, AT).is_empty(),
            "{way}: the barrel closed"
        );
        assert_eq!(
            logged(),
            [format!("opened {barrel:?}"), format!("closed {barrel:?}")],
            "{way}: the own inventory did not open over the menu"
        );
    }

    // A player who leaves with the barrel open gets the menu as it closes; the menu phase drops
    // the menu's handler with the player.
    let (mut w, s) = world_with(menu_on_close(), &[]);
    w.place_block(AT, blk::Kind::Barrel.default_state());
    w.open_block(s, AT);
    w.tick(vec![leave(1, 90)]);
    assert!(w.state.players.is_empty());
    assert_eq!(w.state.menus.len(), 0);
}

// api: PlayerItemHeldEvent
#[test]
fn hotbar_selection_raises_item_held_and_cancelling_sends_the_old_slot() {
    let equip = |slot: u8| MobEquipment {
        entity_runtime_id: 1,
        new_item: crate::protocol::ItemInstance::default(),
        inventory_slot: slot,
        hot_bar_slot: slot,
        window_id: 0,
    };
    let (mut w, s) = world_with(
        on(|e: &mut PlayerItemHeld, _: &mut Context<'_>| {
            log(format!("{} {:?}", e.slot(), e.item()));
            if e.slot() == 2 {
                e.cancel();
            }
        }),
        &[
            sticks(1),
            Stack::new(item(it::Kind::Coal), 2),
            Stack::new(item(it::Kind::Apple), 3),
        ],
    );
    on_mob_equipment(&mut w.state, s, &equip(1));
    assert_eq!(w.player_mut(s).held_slot(), 1);
    w.sent(s);
    on_mob_equipment(&mut w.state, s, &equip(2));
    assert_eq!(w.player_mut(s).held_slot(), 1);
    assert!(
        w.tick_delivered(Vec::new())
            .iter()
            .any(|p| matches!(p, Packet::MobEquipment(m) if m.hot_bar_slot == 1))
    );
    assert_eq!(
        logged(),
        [
            format!("1 {:?}", Stack::new(item(it::Kind::Coal), 2)),
            format!("2 {:?}", Stack::new(item(it::Kind::Apple), 3))
        ]
    );
    // Cancelled selections around a taken one get one answer: the hand the server kept last.
    for slot in [2, 0, 2] {
        on_mob_equipment(&mut w.state, s, &equip(slot));
    }
    let answers: Vec<u8> = w
        .tick_delivered(Vec::new())
        .iter()
        .filter_map(|p| match p {
            Packet::MobEquipment(m) => Some(m.hot_bar_slot),
            _ => None,
        })
        .collect();
    assert_eq!(answers, [0]);
}

#[test]
fn a_listener_selecting_another_hotbar_slot_keeps_its_selection() {
    let equip = |slot: u8| MobEquipment {
        entity_runtime_id: 1,
        new_item: crate::protocol::ItemInstance::default(),
        inventory_slot: slot,
        hot_bar_slot: slot,
        window_id: 0,
    };
    let (mut w, s) = world_with(
        on(|e: &mut PlayerItemHeld, cx: &mut Context<'_>| {
            if e.slot() == 3 {
                cx.player_mut(e.player()).unwrap().set_held_slot(0);
            }
        }),
        &[],
    );
    on_mob_equipment(&mut w.state, s, &equip(1));
    assert_eq!(w.player_mut(s).held_slot(), 1);
    on_mob_equipment(&mut w.state, s, &equip(3));
    assert_eq!(
        w.player_mut(s).held_slot(),
        0,
        "the slot the listener selected and told the client"
    );
}

// api: PlayerBlockPickEvent
#[test]
fn pick_block_raises_block_pick_and_cancelling_gives_nothing() {
    let pick = |cancel: bool| {
        let l = on(move |e: &mut PlayerBlockPick, _: &mut Context<'_>| {
            log(format!("{:?} {:?}", e.block(), e.result_item()));
            if cancel {
                e.cancel();
            }
        });
        LOG.set(Vec::new());
        let mut w = World::with_listeners(GameMode::Creative, l);
        let s = w.join(1);
        let stone = blk::Kind::Cobblestone.default_state();
        w.state.worlds[0].set_block(AT, stone).unwrap();
        on_block_pick_request(
            &mut w.state,
            s,
            &BlockPickRequest {
                position: [AT.x, AT.y, AT.z],
                add_block_nbt: false,
                hot_bar_slot: 0,
            },
        );
        let cobble = Stack::new(block_item(blk::Kind::Cobblestone), 1);
        assert_eq!(logged(), [format!("{stone:?} {cobble:?}")]);
        w.inv(s).main.stack(0).clone()
    };
    assert_eq!(
        pick(false),
        Stack::new(block_item(blk::Kind::Cobblestone), 1)
    );
    assert!(pick(true).is_empty());
}

// api: ChestPairEvent
#[test]
fn chest_pair_names_the_halves_and_cancelling_keeps_them_single() {
    let pair = |cancel: bool| {
        let l = on(move |e: &mut ChestPair, _: &mut Context<'_>| {
            log(format!("{:?} {:?}", e.left(), e.right()));
            if cancel {
                e.cancel();
            }
        });
        let (mut w, _) = world_with(l, &[]);
        let chest = blk::Kind::Chest.default_state();
        w.place_block(AT, chest);
        let next = BlockPos::new(AT.x + 1, AT.y, AT.z);
        let other = BlockPos::new(AT.x, AT.y, AT.z + 1);
        w.place_block(next, chest);
        w.place_block(other, chest);
        let pairs =
            [AT, next, other].map(|p| w.state.rules.block_entities.get(0, p).unwrap().pair());
        (pairs, logged())
    };
    let (pairs, seen) = pair(false);
    assert_eq!(seen.len(), 1, "{seen:?}");
    let paired = pairs.iter().filter(|p| p.is_some()).count();
    assert_eq!(paired, 2);
    let (pairs, seen) = pair(true);
    assert!(pairs.iter().all(Option::is_none));
    assert!(!seen.is_empty());
}

/// Listeners for the conservation model (`run_model`), drawn from `seed`.
/// On every transaction and drop event they cancel, cancel and uncancel, or move items themselves
/// through the Context (swap two main slots, move a main stack into the model's chest or a chest
/// stack into the inventory), keeping the item totals.
pub(super) fn random_listeners(seed: i64) -> Listeners {
    fn act(
        rng: &mut Random,
        session: SessionId,
        cx: &mut Context<'_>,
        cancel: &mut dyn FnMut(bool),
    ) {
        let chest = AT;
        let from = usize::try_from(rng.below(36)).unwrap();
        let to = usize::try_from(rng.below(36)).unwrap();
        match rng.below(8) {
            0 => cancel(true),
            1 => {
                cancel(true);
                cancel(false);
            }
            2 => {
                let p = cx.state.players.get_mut(&session).unwrap();
                p.inventory_mut(Kind::Main).swap(from, to).unwrap();
            }
            3 => {
                let p = cx.state.players.get_mut(&session).unwrap();
                let stack = p.inventory(Kind::Main).item(from).unwrap().clone();
                p.inventory_mut(Kind::Main).set(from, Stack::EMPTY).unwrap();
                let left = match cx.block_inventory_mut(world::Id::DEFAULT, chest) {
                    Some(mut inv) => inv.add(&stack),
                    None => stack,
                };
                let p = cx.state.players.get_mut(&session).unwrap();
                p.inventory_mut(Kind::Main).set(from, left).unwrap();
            }
            4 => {
                let Some(mut inv) = cx.block_inventory_mut(world::Id::DEFAULT, chest) else {
                    return;
                };
                let Some(slot) = inv.view().iter().position(|s| !s.is_empty()) else {
                    return;
                };
                let stack = inv.view().item(slot).unwrap().clone();
                inv.set(slot, Stack::EMPTY).unwrap();
                let p = cx.state.players.get_mut(&session).unwrap();
                let left = p.inventory_mut(Kind::Main).add(&stack);
                if !left.is_empty() {
                    let mut inv = cx.block_inventory_mut(world::Id::DEFAULT, chest).unwrap();
                    inv.set(slot, left).unwrap();
                }
            }
            _ => {}
        }
    }
    let mut l = Listeners::default();
    let mut n = 0_u64;
    l.push(
        Priority::Normal,
        move |e: &mut InventoryTransaction, cx: &mut Context<'_>| {
            n += 1;
            let mut rng = Random::new(seed, n, 1);
            let mut cancelled = e.is_cancelled();
            act(&mut rng, e.player().session(), cx, &mut |c| cancelled = c);
            if cancelled {
                e.cancel();
            } else {
                e.uncancel();
            }
        },
    );
    let mut n = 0_u64;
    l.push(
        Priority::Low,
        move |e: &mut PlayerDropItem, cx: &mut Context<'_>| {
            n += 1;
            let mut rng = Random::new(seed, n, 2);
            let mut cancelled = false;
            act(&mut rng, e.player().session(), cx, &mut |c| cancelled = c);
            if cancelled {
                e.cancel();
            }
        },
    );
    l
}
