//! Tests of the public inventory API (PLAN.md 10.8): a player's inventories, the hotbar, the
//! window, container blocks; every write reaches the clients with a new stack network ID and
//! raises no event.

use std::cell::Cell;

use super::super::super::Context;
use super::*;
use crate::event::{InventoryClose, InventoryTransaction, Priority, Window as Shown};
use crate::inventory::{Error, Kind};
use crate::world;

fn sticks(count: u8) -> Stack {
    Stack::new(item(it::Kind::Stick), count)
}

fn coal(count: u8) -> Stack {
    Stack::new(item(it::Kind::Coal), count)
}

/// The `InventorySlot` packets in `packets`: (window, slot, count, stack network ID).
fn slots(packets: &[Packet]) -> Vec<(u32, u32, u16, i32)> {
    packets
        .iter()
        .filter_map(|p| match p {
            Packet::InventorySlot(s) => Some((
                s.window_id,
                s.slot,
                s.new_item.stack.count,
                s.new_item.stack_network_id,
            )),
            _ => None,
        })
        .collect()
}

// pmmp: Human::getInventory, Living::getArmorInventory, Human::getOffHandInventory, Player::getCursorInventory, Player::getCraftingGrid, Human::getEnderInventory, PlayerInventory, ArmorInventory, PlayerOffHandInventory, PlayerCursorInventory, PlayerCraftingInventory, PlayerEnderInventory, Inventory::setItem, BaseInventory::setItem, CraftingGrid::setItem, Inventory::clear, BaseInventory::clear, ArmorInventory::setHelmet, ArmorInventory::setChestplate, ArmorInventory::setLeggings, ArmorInventory::setBoots, ArmorInventory::getHelmet, ArmorInventory::getChestplate, ArmorInventory::getLeggings, ArmorInventory::getBoots, PlayerInventory::getHotbarSlotItem, Inventory::addItem, BaseInventory::addItem, Inventory::removeItem, BaseInventory::removeItem, Inventory::remove, BaseInventory::remove, Inventory::swap, BaseInventory::swap, Inventory::clearAll, BaseInventory::clearAll, Inventory::setContents, BaseInventory::setContents
#[test]
fn player_writes_reach_the_client_with_new_stack_ids() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.sent(s);
    let p = w.player(s);
    for (kind, size) in [
        (Kind::Main, 36),
        (Kind::Armor, 4),
        (Kind::Offhand, 1),
        (Kind::Cursor, 1),
        (Kind::Crafting, 4),
        (Kind::EnderChest, 27),
    ] {
        assert_eq!(p.inventory(kind).size(), size);
        assert_eq!(kind.size(), size);
    }
    let mut inv = p.inventory_mut(Kind::Main);
    assert!(inv.add(&sticks(64)).is_empty());
    assert!(inv.add(&sticks(6)).is_empty(), "the first slot is full");
    assert_eq!(inv.add(&coal(5)), Stack::EMPTY);
    assert_eq!(inv.view().item(0), Some(&sticks(64)));
    assert_eq!(inv.view().item(2), Some(&coal(5)));
    assert_eq!(inv.remove(&sticks(64)), Stack::EMPTY);
    assert_eq!(inv.remove(&sticks(2)), Stack::EMPTY);
    assert_eq!(
        inv.remove(&Stack::new(item(it::Kind::Apple), 3)),
        Stack::new(item(it::Kind::Apple), 3)
    );
    assert_eq!(inv.view().item(1), Some(&sticks(4)));
    assert!(inv.view().is_slot_empty(0));
    inv.swap(1, 2).unwrap();
    assert_eq!(inv.view().item(1), Some(&coal(5)));
    assert_eq!(inv.set(36, sticks(1)), Err(Error::NoSlot));
    assert_eq!(inv.swap(0, 40), Err(Error::NoSlot));
    let packets = w.sent(s);
    let sent = slots(&packets);
    assert!(sent.iter().all(|&(window, ..)| window == 0));
    assert_eq!(sent.last().map(|s| (s.1, s.2)), Some((2, 4)));
    let ids: Vec<i32> = sent.iter().filter(|s| s.2 > 0).map(|s| s.3).collect();
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        ids.len(),
        "each write is a new stack network ID"
    );
    assert_eq!(w.id(s, C::HotBar, 2), *ids.last().unwrap());

    let p = w.player(s);
    let helmet = Stack::new(item(it::Kind::IronHelmet), 1);
    let mut armor = p.inventory_mut(Kind::Armor);
    armor.set(0, helmet.clone()).unwrap();
    assert_eq!(armor.view().item(0), Some(&helmet));
    armor.set(0, Stack::EMPTY).unwrap();
    let mut ender = p.inventory_mut(Kind::EnderChest);
    ender.set_contents(&[sticks(3), coal(2)]).unwrap();
    assert_eq!(ender.view().iter().filter(|s| !s.is_empty()).count(), 2);
    assert_eq!(
        ender.set_contents(&vec![sticks(1); 28]),
        Err(Error::TooLong)
    );
    ender.remove_all(&sticks(1));
    assert_eq!(ender.view().first(&coal(1)), Some(1));
    ender.clear_all();
    assert_eq!(ender.view().first_empty(), Some(0));
    assert!(ender.view().iter().all(Stack::is_empty));
    assert!(format!("{ender:?}").contains("EnderChest"));
    let mut grid = p.inventory_mut(Kind::Crafting);
    grid.set(3, sticks(2)).unwrap();
    assert_eq!(w.inv(s).grid.stack(3), &sticks(2));
    check_slots(&w.state);
}

#[test]
fn the_cursor_and_grid_take_no_stack_beyond_what_a_close_can_give_back() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.player(s).inventory.pending = vec![sticks(1); MAX_SAVED_PENDING];
    let p = w.player(s);
    assert_eq!(
        p.inventory_mut(Kind::Cursor).set(0, sticks(1)),
        Err(Error::Full)
    );
    assert_eq!(p.inventory_mut(Kind::Crafting).add(&coal(3)), coal(3));
    assert!(p.inventory_mut(Kind::Main).add(&coal(3)).is_empty());
    check_slots(&w.state);
}

// pmmp: PlayerInventory::getHeldItemIndex, PlayerInventory::setHeldItemIndex, PlayerInventory::getItemInHand, Player::getCurrentWindow, Player::removeCurrentWindow
#[test]
fn the_hotbar_selection_and_the_window_are_the_players() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let p = w.player(s);
    p.inventory_mut(Kind::Main).set(4, sticks(9)).unwrap();
    assert_eq!(p.held_slot(), 0);
    assert!(p.set_held_slot(4));
    assert!(!p.set_held_slot(9));
    assert_eq!((p.held_slot(), p.held_item()), (4, &sticks(9)));
    assert!(
        w.sent(s)
            .iter()
            .any(|pk| matches!(pk, Packet::PlayerHotBar(h) if h.selected_hot_bar_slot == 4))
    );
    assert_eq!(w.player(s).window(), None);
    w.open_inventory(s);
    assert_eq!(w.player(s).window(), Some(Shown::Own));
    w.player(s)
        .inventory_mut(Kind::Cursor)
        .set(0, coal(2))
        .unwrap();
    let mut cx = Context {
        state: &mut w.state,
    };
    let id = crate::player::Id::new(s);
    assert!(cx.close_window(id));
    assert!(!cx.close_window(id), "nothing is open any more");
    assert_eq!(w.player(s).window(), None);
    assert_eq!(w.inv(s).main.stack(0), &coal(2), "the cursor went back");
}

// pmmp: Container::getRealInventory, InventoryHolder::getInventory
#[test]
fn container_blocks_read_both_chest_halves_and_tell_every_viewer() {
    let mut w = World::new(GameMode::Survival);
    let viewer = w.join(1);
    let chest = BlockPos::new(3, -60, 3);
    let other = w.double_chest(chest);
    w.open_block(viewer, chest);
    w.sent(viewer);
    let mut cx = Context {
        state: &mut w.state,
    };
    let here = world::Id::DEFAULT;
    let left = cx.block_inventory(here, chest).unwrap();
    assert_eq!(left.size(), 54);
    let mut inv = cx.block_inventory_mut(here, other).unwrap();
    assert_eq!(inv.view().size(), 54, "either half opens the double chest");
    assert!(inv.add(&sticks(64)).is_empty());
    inv.set(30, coal(7)).unwrap();
    assert_eq!(inv.set(54, coal(1)), Err(Error::NoSlot));
    let view = cx.block_inventory(here, chest).unwrap();
    assert_eq!(view.item(30), Some(&coal(7)));
    assert!(view.contains(&coal(7)) && !view.contains(&coal(8)));
    assert!(
        cx.block_inventory(here, BlockPos::new(0, -61, 0)).is_none(),
        "no container"
    );
    let window = w.inv(viewer).window.as_ref().unwrap().id;
    let sent = slots(&w.sent(viewer));
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|s| s.0 == u32::from(window)));
    let mut sent_slots: Vec<u32> = sent.iter().map(|s| s.1).collect();
    sent_slots.sort_unstable();
    assert_eq!(sent_slots, [0, 30]);
    // The viewer's next request uses the new IDs.
    let id = w.id(viewer, C::LevelEntity, 30);
    assert_eq!(id, sent.iter().find(|s| s.1 == 30).unwrap().3);
    check_slots(&w.state);
}

thread_local! {
    static HEARD: Cell<u32> = const { Cell::new(0) };
}

#[test]
fn direct_writes_and_closes_raise_no_events() {
    let mut l = Listeners::default();
    l.push(
        Priority::Normal,
        |_: &mut InventoryTransaction, _: &mut Context<'_>| {
            HEARD.set(HEARD.get() + 1);
        },
    );
    l.push(
        Priority::Normal,
        |_: &mut InventoryClose, _: &mut Context<'_>| {
            HEARD.set(HEARD.get() + 1);
        },
    );
    HEARD.set(0);
    let mut w = World::with_listeners(GameMode::Survival, l);
    let s = w.join(1);
    let chest = BlockPos::new(3, -60, 3);
    w.place_block(chest, blk::Kind::Chest.default_state());
    w.open_block(s, chest);
    let mut cx = Context {
        state: &mut w.state,
    };
    cx.block_inventory_mut(world::Id::DEFAULT, chest)
        .unwrap()
        .set(0, sticks(3))
        .unwrap();
    cx.state
        .players
        .get_mut(&s)
        .unwrap()
        .inventory_mut(Kind::Main)
        .set(0, coal(1))
        .unwrap();
    assert!(cx.close_window(crate::player::Id::new(s)));
    assert_eq!(HEARD.get(), 0);
    // The game's own close raises it.
    w.open_block(s, chest);
    w.close(s);
    assert_eq!(HEARD.get(), 1);
}
