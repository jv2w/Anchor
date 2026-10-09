//! Tests of the inventory, crafting, station, furnace, brewing, campfire, chest and trade events
//!: the listener sees the right fields, cancelling keeps the game and
//! resyncs the client, changed fields change the outcome, monitors see the applied state, and a
//! listener that changes the game so the check fails cancels the action.

use std::cell::Cell;

use super::super::super::{Context, entity, trade};
use super::*;
use crate::block::PropertyValue;
use crate::event::{
    BrewItem, BrewingFuelUse, CampfireCook, ChestPair, CraftItem, EnchantOption, Event,
    FurnaceBurn, FurnaceSmelt, Inventory as Where, InventoryClose, InventoryOpen,
    InventoryTransaction, PlayerBlockPick, PlayerDropItem, PlayerEnchantingOptionsRequest,
    PlayerItemEnchant, PlayerItemHeld, Priority, Trade, TradeOpen, Window as Shown,
};
use crate::inventory::Kind;
use crate::math::Vec3;
use crate::nbt::{List, Value};
use crate::protocol::packet::{BlockPickRequest, MobEquipment};
use crate::world;

thread_local! {
    /// What the listeners of a test saw, in order.
    static LOG: Cell<Vec<String>> = const { Cell::new(Vec::new()) };
}

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

/// The position of the blocks the tests place, next to the spawn.
const AT: BlockPos = BlockPos::new(3, -60, 3);
const COMBINED: C = C::CombinedHotBarAndInventory;

/// A survival world with one player holding `stacks` from main slot 0.
fn world_with(listeners: Listeners, stacks: &[Stack]) -> (World, SessionId) {
    LOG.set(Vec::new());
    let mut w = World::with_listeners(GameMode::Survival, listeners);
    let s = w.join(1);
    for stack in stacks {
        w.give(s, stack.clone());
    }
    super::super::super::survival::add_experience(w.player(s), 100_000);
    w.sent(s);
    (w, s)
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

/// Moves `count` from main slot `from` into request slot `to`.
fn put(w: &mut World, s: SessionId, from: u8, count: u8, to: (C, u8)) {
    let id = w.id(s, COMBINED, from);
    let r = w.request(
        s,
        vec![place(count, at(COMBINED, from, id), at(to.0, to.1, 0))],
    );
    assert!(ok(&r), "{r:?}");
}

fn named(name: &str) -> Type {
    Type::from_name(name, 0, None, &crate::nbt::Compound::new()).unwrap()
}

fn sticks(count: u8) -> Stack {
    Stack::new(item(it::Kind::Stick), count)
}

// pmmp: InventoryTransactionEvent
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
    let r = w.request(
        s,
        vec![place(3, at(COMBINED, 0, id), at(C::CraftingInput, 28, 0))],
    );
    assert!(ok(&r));
    assert_eq!(
        logged(),
        [
            format!(
                "{:?}",
                [
                    (Where::Player(Kind::Main), 0, 10, 7),
                    (Where::Player(Kind::Crafting), 0, 0, 3)
                ]
            ),
            "applied 7".to_owned()
        ]
    );
}

// pmmp: InventoryTransactionEvent
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
        vec![place(3, at(COMBINED, 0, id), at(C::CraftingInput, 28, 0))],
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

// pmmp: InventoryTransactionEvent
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
    let r = w.request(
        s,
        vec![place(3, at(COMBINED, 0, id), at(C::CraftingInput, 28, 0))],
    );
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
    let r = w.request(
        s,
        vec![place(3, at(COMBINED, 0, id), at(C::CraftingInput, 28, 0))],
    );
    assert!(ok(&r));
    assert_eq!(w.inv(s).grid.stack(0), &sticks(3));
}

/// Puts one log into the 2x2 grid; the planks recipe and the log's ID there.
fn log_in_grid(w: &mut World, s: SessionId) -> (u32, i32) {
    let (log_item, plank) = (
        block_item(blk::Kind::OakLog),
        block_item(blk::Kind::OakPlanks),
    );
    let (planks, _) = recipe_for(plank, &[log_item]);
    w.open_inventory(s);
    put(w, s, 0, 1, (C::CraftingInput, 28));
    (planks, w.id(s, C::CraftingInput, 28))
}

// pmmp: CraftItemEvent
#[test]
fn crafting_raises_craft_item_and_cancelling_keeps_the_grid() {
    let l = on(|e: &mut CraftItem, _: &mut Context<'_>| {
        log(format!(
            "{:?} {:?} {}",
            e.inputs(),
            e.outputs(),
            e.repetitions()
        ));
    });
    let (mut w, s) = world_with(l, &[Stack::new(block_item(blk::Kind::OakLog), 4)]);
    let (planks, l_id) = log_in_grid(&mut w, s);
    let id = w.next_id(s);
    let actions = vec![
        craft(planks, 1),
        consume(1, at(C::CraftingInput, 28, l_id)),
        place(4, created(id), at(COMBINED, 1, 0)),
    ];
    assert!(ok(&w.request_as(s, id, actions)));
    let log_one = Stack::new(block_item(blk::Kind::OakLog), 1);
    let made_planks = Stack::new(block_item(blk::Kind::OakPlanks), 4);
    assert_eq!(
        logged(),
        [format!(
            "{:?} {:?} 1",
            [log_one],
            std::slice::from_ref(&made_planks)
        )]
    );
    assert_eq!(w.inv(s).main.stack(1), &made_planks);

    let (mut w, s) = world_with(
        on(|e: &mut CraftItem, _: &mut Context<'_>| e.cancel()),
        &[Stack::new(block_item(blk::Kind::OakLog), 4)],
    );
    let (planks, l_id) = log_in_grid(&mut w, s);
    let before = totals(&w.state);
    let id = w.next_id(s);
    let actions = vec![
        craft(planks, 1),
        consume(1, at(C::CraftingInput, 28, l_id)),
        place(4, created(id), at(COMBINED, 1, 0)),
    ];
    assert!(!ok(&w.request_as(s, id, actions)));
    assert_eq!(totals(&w.state), before);
    assert_eq!(w.inv(s).grid.stack(0).count(), 1);
}

// pmmp: PlayerDropItemEvent
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

/// An enchanting table with a pickaxe and three lapis in it.
fn enchanting(listeners: Listeners) -> (World, SessionId) {
    let (mut w, s) = world_with(
        listeners,
        &[
            Stack::new(item(it::Kind::IronPickaxe), 1),
            Stack::new(item(it::Kind::LapisLazuli), 3),
        ],
    );
    w.place_block(AT, blk::Kind::EnchantingTable.default_state());
    w.open_block(s, AT);
    put(&mut w, s, 0, 1, (C::EnchantingInput, 14));
    put(&mut w, s, 1, 3, (C::EnchantingMaterial, 15));
    (w, s)
}

/// The enchant request of the shown option `index`.
fn enchant_with(w: &mut World, s: SessionId, index: usize) -> ItemStackResponse {
    let option = w.inv(s).window.as_ref().unwrap().station.options()[index].0;
    let input = w.id(s, C::EnchantingInput, 14);
    let lapis = w.id(s, C::EnchantingMaterial, 15);
    let id = w.next_id(s);
    let cost = u8::try_from(index + 1).unwrap();
    let actions = vec![
        craft(option, 1),
        consume(1, at(C::EnchantingInput, 14, input)),
        consume(cost, at(C::EnchantingMaterial, 15, lapis)),
        place(1, created(id), at(C::EnchantingInput, 14, id)),
    ];
    w.request_as(s, id, actions)
}

// pmmp: PlayerEnchantingOptionsRequestEvent, PlayerItemEnchantEvent
#[test]
fn enchanting_options_can_be_replaced_and_the_enchant_event_shows_the_result() {
    let mut l = on(
        |e: &mut PlayerEnchantingOptionsRequest, _: &mut Context<'_>| {
            log(format!("options {}", e.options().len()));
            e.set_options(vec![EnchantOption::new(
                1,
                "x".repeat(100),
                vec![(Enchantment::Efficiency, 5), (Enchantment::Unbreaking, 0)],
            )]);
        },
    );
    l.push(
        Priority::Normal,
        |e: &mut PlayerItemEnchant, _: &mut Context<'_>| {
            log(format!(
                "cost {} {:?}",
                e.cost(),
                e.output_item().extra().unwrap().enchantments
            ));
        },
    );
    let (mut w, s) = enchanting(l);
    let options = w.inv(s).window.as_ref().unwrap().station.options().to_vec();
    assert_eq!(options.len(), 1);
    assert_eq!(options[0].1.name.len(), 64, "a listener's name is cut");
    assert_eq!(options[0].1.enchantments, [(Enchantment::Efficiency, 5)]);
    assert!(ok(&enchant_with(&mut w, s, 0)));
    let pick = w.inv(s).get(SlotRef::Ui(0)).unwrap().stack;
    assert_eq!(
        pick.extra().unwrap().enchantments,
        [(Enchantment::Efficiency, 5)]
    );
    assert_eq!(
        logged(),
        [
            "options 3".to_owned(),
            format!("cost 1 {:?}", [(Enchantment::Efficiency, 5)])
        ]
    );

    let mut l = on(|e: &mut PlayerItemEnchant, _: &mut Context<'_>| e.cancel());
    l.push(
        Priority::Normal,
        |e: &mut PlayerEnchantingOptionsRequest, _: &mut Context<'_>| {
            log(format!(
                "{}",
                e.player() == crate::player::Id::new(SessionId::new(1))
            ));
        },
    );
    let (mut w, s) = enchanting(l);
    let before = totals(&w.state);
    assert!(!ok(&enchant_with(&mut w, s, 0)));
    assert_eq!(totals(&w.state), before, "the pickaxe and the lapis stay");
    assert!(
        w.inv(s)
            .get(SlotRef::Ui(0))
            .unwrap()
            .stack
            .extra()
            .is_none()
    );

    let (w, s) = enchanting(on(
        |e: &mut PlayerEnchantingOptionsRequest, _: &mut Context<'_>| e.cancel(),
    ));
    assert!(
        w.inv(s)
            .window
            .as_ref()
            .unwrap()
            .station
            .options()
            .is_empty()
    );
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

// pmmp: InventoryOpenEvent, InventoryCloseEvent
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
    assert_eq!(w.player(s).window(), Some(Shown::Own));

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

// pmmp: InventoryCloseEvent
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
    w.tick(vec![super::super::super::tests::leave(1, 90)]);
    assert!(w.state.players.is_empty());
    assert_eq!(w.state.menus.len(), 0);
}

// pmmp: PlayerItemHeldEvent
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
    assert_eq!(w.player(s).held_slot(), 1);
    w.sent(s);
    on_mob_equipment(&mut w.state, s, &equip(2));
    assert_eq!(w.player(s).held_slot(), 1);
    assert!(
        w.sent(s)
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
    assert_eq!(w.player(s).held_slot(), 1);
    on_mob_equipment(&mut w.state, s, &equip(3));
    assert_eq!(
        w.player(s).held_slot(),
        0,
        "the slot the listener selected and told the client"
    );
}

// pmmp: PlayerBlockPickEvent
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

/// The furnace's slots after `ticks` ticks of a furnace with two raw iron and one coal.
fn furnace(listeners: Listeners, ticks: u32) -> Vec<Stack> {
    let (mut w, _) = world_with(listeners, &[]);
    w.place_block(AT, blk::Kind::Furnace.default_state());
    let mut cx = Context {
        state: &mut w.state,
    };
    let mut inv = cx.block_inventory_mut(world::Id::DEFAULT, AT).unwrap();
    inv.set(0, Stack::new(named("minecraft:raw_iron"), 2))
        .unwrap();
    inv.set(1, Stack::new(item(it::Kind::Coal), 1)).unwrap();
    for _ in 0..ticks {
        w.tick(Vec::new());
    }
    w.state
        .rules
        .block_entities
        .get(0, AT)
        .unwrap()
        .items
        .clone()
}

// pmmp: FurnaceBurnEvent, FurnaceSmeltEvent
#[test]
fn furnace_events_change_the_burn_and_the_result_and_cancelling_keeps_the_items() {
    let coal = Stack::new(item(it::Kind::Coal), 1);
    let gold = Stack::new(named("minecraft:gold_ingot"), 3);
    let mut l = on(|e: &mut FurnaceBurn, _: &mut Context<'_>| {
        log(format!(
            "burn {:?} {} {}",
            e.fuel(),
            e.burn_time_ticks(),
            e.is_burning()
        ));
        e.set_burning(false);
        e.set_burn_time_ticks(250);
    });
    l.push(
        Priority::Normal,
        |e: &mut FurnaceSmelt, _: &mut Context<'_>| {
            log(format!("smelt {:?}", e.source()));
            e.set_result(Stack::new(named("minecraft:gold_ingot"), 3));
        },
    );
    l.push_monitor(|e: &FurnaceSmelt, cx: &Context<'_>| {
        let out = &cx.state.rules.block_entities.get(0, e.pos()).unwrap().items[2];
        log(format!("applied {}", out.count()));
    });
    let items = furnace(l, 201);
    assert_eq!(items[1], coal, "the fuel is not used up");
    assert_eq!(items[2], gold, "the listener's result");
    assert_eq!(items[0].count(), 1);
    let raw = Stack::new(named("minecraft:raw_iron"), 1);
    assert_eq!(
        logged(),
        [
            format!("burn {coal:?} 1600 true"),
            format!("smelt {raw:?}"),
            "applied 3".to_owned()
        ]
    );
    // A cancelled burn: nothing burns, so nothing smelts.
    let items = furnace(
        on(|e: &mut FurnaceBurn, _: &mut Context<'_>| e.cancel()),
        300,
    );
    assert_eq!(items[0].count(), 2, "nothing burns, nothing smelts");
    assert_eq!(items[1], coal);
    let items = furnace(
        on(|e: &mut FurnaceSmelt, _: &mut Context<'_>| e.cancel()),
        201,
    );
    assert_eq!(items[0].count(), 2, "the smelt is cancelled");
    assert!(items[2].is_empty());
    // A result that does not fit the output cancels the smelt.
    let items = furnace(
        on(|e: &mut FurnaceSmelt, _: &mut Context<'_>| e.set_result(Stack::EMPTY)),
        201,
    );
    assert_eq!(items[0].count(), 2);
}

// pmmp: BrewingFuelUseEvent, BrewItemEvent
#[test]
fn brewing_events_change_the_fuel_and_each_bottle() {
    let water = Stack::new(item(it::Kind::Potion), 1);
    let mut l = on(|e: &mut BrewingFuelUse, _: &mut Context<'_>| {
        log(format!("fuel {}", e.fuel_time()));
        e.set_fuel_time(0);
    });
    l.push(Priority::Normal, |e: &mut BrewItem, _: &mut Context<'_>| {
        log(format!("brew {}", e.slot()));
        if e.slot() == 1 {
            e.set_result(sticks(1));
        } else {
            e.cancel();
        }
    });
    let (mut w, _) = world_with(l, &[]);
    w.place_block(AT, blk::Kind::BrewingStand.default_state());
    let mut cx = Context {
        state: &mut w.state,
    };
    let mut inv = cx.block_inventory_mut(world::Id::DEFAULT, AT).unwrap();
    inv.set(0, Stack::new(named("minecraft:nether_wart"), 1))
        .unwrap();
    inv.set(1, water.clone()).unwrap();
    inv.set(3, water.clone()).unwrap();
    inv.set(4, Stack::new(item(it::Kind::BlazePowder), 2))
        .unwrap();
    for _ in 0..401 {
        w.tick(Vec::new());
    }
    let e = w.state.rules.block_entities.get(0, AT).unwrap();
    assert_eq!(e.items[1], sticks(1));
    assert_eq!(e.items[3], water, "the cancelled bottle stays");
    assert!(e.items[0].is_empty(), "the ingredient is used up");
    // One brew of fuel per powder: the first brew used it up as it began, so the second
    // powder went in on the next tick.
    assert!(e.items[4].is_empty());
    assert!(matches!(&e.kind, block_entity::Kind::BrewingStand(b) if b.fuel_total == 1));
    assert_eq!(logged(), ["fuel 20", "fuel 20", "brew 1", "brew 3"]);
}

// pmmp: CampfireCookEvent
#[test]
fn campfire_cook_changes_what_drops_and_cancelling_keeps_the_item() {
    let cook = |cancel: bool| {
        let l = on(move |e: &mut CampfireCook, _: &mut Context<'_>| {
            log(format!("{} {:?}", e.slot(), e.input().item()));
            if cancel {
                e.cancel();
            } else {
                e.set_result(sticks(2));
            }
        });
        let (mut w, _) = world_with(l, &[]);
        let lit = blk::Kind::Campfire
            .default_state()
            .with("extinguished", PropertyValue::Byte(0))
            .unwrap();
        w.place_block(AT, lit);
        let mut cx = Context {
            state: &mut w.state,
        };
        let mut inv = cx.block_inventory_mut(world::Id::DEFAULT, AT).unwrap();
        assert_eq!(
            inv.set(0, Stack::new(named("minecraft:beef"), 2)),
            Err(crate::inventory::Error::TooMany),
            "a campfire slot takes one"
        );
        inv.set(0, Stack::new(named("minecraft:beef"), 1)).unwrap();
        for _ in 0..620 {
            w.tick(Vec::new());
        }
        let e = w.state.rules.block_entities.get(0, AT).unwrap();
        (e.items[0].clone(), dropped())
    };
    let (left, drops) = cook(false);
    assert!(left.is_empty());
    assert_eq!(drops, [sticks(2)]);
    assert_eq!(
        logged()[0],
        format!("0 {:?}", Some(named("minecraft:beef")))
    );
    let (left, drops) = cook(true);
    assert_eq!(left, Stack::new(named("minecraft:beef"), 1));
    assert!(drops.is_empty());
}

// pmmp: ChestPairEvent
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

/// Listeners for the conservation model (`run_model`) from `seed`: on every request event they
/// cancel, cancel and uncancel, or move items themselves through the Context (swap two main
/// slots, move a main stack into the model's chest or a chest stack into the inventory), keeping
/// the item totals.
pub(super) fn random_listeners(seed: i64) -> Listeners {
    fn act(
        rng: &mut Random,
        session: SessionId,
        cx: &mut Context<'_>,
        cancel: &mut dyn FnMut(bool),
    ) {
        let chest = BlockPos::new(3, -60, 3);
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
    let mut n = 0u64;
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
    let mut n = 0u64;
    l.push(
        Priority::High,
        move |e: &mut CraftItem, cx: &mut Context<'_>| {
            n += 1;
            let mut rng = Random::new(seed, n, 2);
            let mut cancelled = false;
            act(&mut rng, e.player().session(), cx, &mut |c| cancelled = c);
            if cancelled {
                e.cancel();
            }
        },
    );
    let mut n = 0u64;
    l.push(
        Priority::Low,
        move |e: &mut PlayerDropItem, cx: &mut Context<'_>| {
            n += 1;
            let mut rng = Random::new(seed, n, 3);
            let mut cancelled = false;
            act(&mut rng, e.player().session(), cx, &mut |c| cancelled = c);
            if cancelled {
                e.cancel();
            }
        },
    );
    l
}
