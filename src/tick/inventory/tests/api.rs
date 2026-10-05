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

/// A stick whose saved form takes exactly `bytes` as the stack limits count it (with a `Slot`),
/// padded with a byte array.
fn padded(bytes: usize) -> it::Result<Stack> {
    let with_pad = |len: usize| {
        sticks(1).with_extra(Some(Extra {
            nbt: crate::nbt::Compound::from([(
                "pad".to_owned(),
                crate::nbt::Value::ByteArray(vec![1; len]),
            )]),
            ..Extra::default()
        }))
    };
    let mut nbt = with_pad(0)?.to_nbt();
    nbt.insert("Slot".to_owned(), crate::nbt::Value::Byte(0));
    let mut w = crate::protocol::Writer::new(Vec::new(), 0);
    w.nbt(&nbt, crate::nbt::Encoding::LittleEndian).unwrap();
    with_pad(bytes - w.as_bytes().len())
}

/// The shulker box state and item the tests place and carry.
fn shulker_box() -> (blk::State, Type) {
    let state = blk::State::all()
        .find(|b| b.is(blk::Class::ShulkerBox))
        .unwrap();
    let ItemForm::Item(item) = state.item() else {
        panic!("a shulker box item");
    };
    (state, item)
}

/// A plugin cannot make a stack over the limits: the call fails and the stack it started from
/// stays as it was.
#[test]
fn a_stack_over_the_limits_is_refused_and_changes_nothing() {
    let largest = padded(it::MAX_NBT_BYTES).expect("a stack at the limit is made");
    assert_eq!(padded(it::MAX_NBT_BYTES + 1), Err(it::Error::TooLarge));
    let named = largest.with_extra(Some(Extra {
        custom_name: Some("n".repeat(crate::tile::MAX_NAME_BYTES + 1)),
        ..Extra::default()
    }));
    assert_eq!(named, Err(it::Error::NameTooLong));
    assert_eq!(padded(it::MAX_NBT_BYTES), Ok(largest));
}

/// A player holding the largest stack in each of its 146 places (main, armour, offhand, ender
/// chest, cursor, grid and the stacks waiting for room) and 16 user data values of the largest
/// size commits and reads back: its extra compound is over one NBT root (4 MiB) but within its
/// record.
#[test]
fn a_player_holding_the_largest_stack_everywhere_commits_and_reads_back() {
    use crate::storage::codec::PlayerRecord;
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let largest = padded(it::MAX_NBT_BYTES).unwrap();
    let values = (0..crate::data::MAX_PLAYER_TYPES)
        .map(|i| {
            let name = format!("{i:02}{}", "n".repeat(crate::data::MAX_NAME_BYTES - 2));
            let json = vec![b'7'; crate::data::MAX_PLAYER_VALUE_BYTES];
            (name, crate::nbt::Value::ByteArray(json))
        })
        .collect();
    let p = w.player(s);
    p.extra = super::super::super::extra::player_loaded(crate::nbt::Compound::from([(
        "anchor:data".to_owned(),
        crate::nbt::Value::Compound(values),
    )]));
    for kind in [
        Kind::Main,
        Kind::Armor,
        Kind::Offhand,
        Kind::EnderChest,
        Kind::Cursor,
        Kind::Crafting,
    ] {
        let mut inv = p.inventory_mut(kind);
        for i in 0..kind.size() {
            inv.set(i, largest.clone()).unwrap();
        }
    }
    p.inventory.pending = vec![largest; MAX_SAVED_PENDING - 5];
    let data = super::super::super::save::data_of(&w.state, &w.state.players[&s]);
    let record = PlayerRecord {
        world: data.world,
        position: data.position,
        extra: data.extra,
    };
    let mut w = crate::protocol::Writer::new(Vec::new(), 0);
    assert!(
        w.nbt(&record.extra, crate::nbt::Encoding::LittleEndian)
            .is_err(),
        "over one NBT root"
    );
    let sealed = crate::storage::seal::seal(b"key", &record).expect("the player encodes");
    let back: PlayerRecord = crate::storage::seal::unseal(b"key", &sealed).unwrap();
    assert_eq!(back, record);
}

/// A shulker box takes no shulker box and no stack over 3 KiB through the API either (PMMP's
/// `setItem` does not check), so the box with its items is always one stack.
#[test]
fn a_shulker_box_takes_through_the_api_only_what_its_item_can_carry() {
    let mut w = World::new(GameMode::Survival);
    let pos = BlockPos::new(3, -60, 3);
    let (shulker, box_item) = shulker_box();
    w.place_block(pos, shulker);
    let mut cx = Context {
        state: &mut w.state,
    };
    let mut inv = cx.block_inventory_mut(world::Id::DEFAULT, pos).unwrap();
    assert_eq!(inv.set(0, Stack::new(box_item, 1)), Err(Error::NotAccepted));
    let over = padded(it::MAX_STORED_BYTES + 1).unwrap();
    assert_eq!(inv.set(0, over.clone()), Err(Error::NotAccepted));
    assert_eq!(inv.add(&over), over, "nothing goes in");
    assert!(inv.view().is_slot_empty(0));
    let most = padded(it::MAX_STORED_BYTES).unwrap();
    assert_eq!(inv.set(0, most.clone()), Ok(()));
    assert_eq!(inv.view().item(0), Some(&most));
}

/// A placed shulker box item made by a plugin with a stack its slots do not take drops that
/// stack beside the box and keeps the rest.
#[test]
fn a_placed_shulker_box_drops_what_its_slots_do_not_take() {
    let mut w = World::new(GameMode::Survival);
    let pos = BlockPos::new(3, -60, 3);
    let (shulker, box_item) = shulker_box();
    let over = padded(it::MAX_STORED_BYTES + 100).unwrap();
    let entry = |stack: &Stack, slot: u8| {
        let mut nbt = stack.to_nbt();
        nbt.insert("Slot".to_owned(), crate::nbt::Value::Byte(slot));
        nbt
    };
    let item = Stack::new(box_item, 1)
        .with_extra(Some(Extra {
            nbt: crate::nbt::Compound::from([(
                "Items".to_owned(),
                crate::nbt::Value::List(crate::nbt::List::Compound(vec![
                    entry(&sticks(5), 0),
                    entry(&over, 1),
                ])),
            )]),
            ..Extra::default()
        }))
        .unwrap();
    w.state.worlds[0].set_block(pos, shulker).unwrap();
    block_entity::placed(&mut w.state, 0, pos, None, &item, blk::Face::Up);
    let e = w.state.rules.block_entities.get(0, pos).unwrap();
    assert_eq!(e.items[0], sticks(5));
    assert!(e.items[1].is_empty());
    assert_eq!(dropped(), vec![over]);
}

/// A chest of 27 shulker boxes, each full of the largest stacks a shulker box takes and named
/// with the longest name, commits and reads back: the box's item is within the stack limit and
/// the chest within one NBT root. A plugin's box item holding more is refused.
#[test]
fn a_chest_of_full_shulker_boxes_of_the_largest_items_commits_and_reads_back() {
    let mut w = World::new(GameMode::Survival);
    let (shulker, box_item) = shulker_box();
    let (box_pos, chest_pos) = (BlockPos::new(3, -60, 3), BlockPos::new(5, -60, 3));
    w.place_block(box_pos, shulker);
    w.place_block(chest_pos, blk::Kind::Chest.default_state());
    let most = padded(it::MAX_STORED_BYTES).unwrap();
    let mut cx = Context {
        state: &mut w.state,
    };
    let here = world::Id::DEFAULT;
    let mut inv = cx.block_inventory_mut(here, box_pos).unwrap();
    for i in 0..27 {
        inv.set(i, most.clone()).unwrap();
    }
    let name = "n".repeat(crate::tile::MAX_NAME_BYTES);
    cx.block_entity_mut(here, box_pos)
        .unwrap()
        .set_name(Some(&name))
        .unwrap();
    let full = block_entity::with_block_data(&w.state, 0, box_pos, Stack::new(box_item, 1));
    assert_eq!(full.custom_name(), name);
    let mut cx = Context {
        state: &mut w.state,
    };
    let mut chest = cx.block_inventory_mut(here, chest_pos).unwrap();
    for i in 0..27 {
        chest.set(i, full.clone()).unwrap();
    }
    let changed = w.state.rules.block_entities.take_changed();
    let (_, list) = changed
        .iter()
        .find(|(key, _)| key.1 == chest_pos.chunk())
        .unwrap();
    assert_eq!(block_entity::tests::committed(list), *list);
    let saved = list
        .iter()
        .find(|c| c.get("id") == Some(&crate::nbt::Value::String("Chest".to_owned())))
        .unwrap();
    let Some(crate::nbt::Value::List(crate::nbt::List::Compound(items))) = saved.get("Items")
    else {
        panic!("the chest's items");
    };
    assert_eq!(items.len(), 27);
    assert_eq!(
        Stack::from_nbt_with(&items[26], &w.state.custom),
        Some(full.clone())
    );
    // A box item holding 27 stacks of 8 KiB is not made.
    let big = padded(8 << 10).unwrap();
    let entries = (0u8..27)
        .map(|slot| {
            let mut nbt = big.to_nbt();
            nbt.insert("Slot".to_owned(), crate::nbt::Value::Byte(slot));
            nbt
        })
        .collect();
    let refused = Stack::new(box_item, 1).with_extra(Some(Extra {
        nbt: crate::nbt::Compound::from([(
            "Items".to_owned(),
            crate::nbt::Value::List(crate::nbt::List::Compound(entries)),
        )]),
        ..Extra::default()
    }));
    assert_eq!(refused, Err(it::Error::TooLarge));
}

// pmmp: Inventory::getViewers, BaseInventory::getViewers
#[test]
fn block_inventory_viewers_are_the_players_with_its_window_open() {
    let mut w = World::new(GameMode::Survival);
    let (a, b) = (w.join(1), w.join(2));
    let chest = BlockPos::new(3, -60, 3);
    let other = w.double_chest(chest);
    let anvil = BlockPos::new(6, -60, 3);
    w.place_block(anvil, blk::Kind::Anvil.default_state());
    let here = world::Id::DEFAULT;
    let viewers = |w: &mut World, pos| {
        let cx = Context {
            state: &mut w.state,
        };
        cx.block_inventory_viewers(here, pos).collect::<Vec<_>>()
    };
    let id = crate::player::Id::new;
    assert!(viewers(&mut w, chest).is_empty());
    w.open_block(b, other);
    w.open_block(a, chest);
    // Either half of the double chest, in the order they opened it.
    assert_eq!(viewers(&mut w, chest), [id(b), id(a)]);
    assert_eq!(viewers(&mut w, other), [id(b), id(a)]);
    let mut cx = Context {
        state: &mut w.state,
    };
    assert_eq!(cx.block_inventory_viewers(here, chest).len(), 2);
    assert!(cx.close_window(id(b)));
    assert_eq!(viewers(&mut w, other), [id(a)]);
    // A station's window counts too; opening it closes the chest.
    w.open_block(a, anvil);
    assert!(viewers(&mut w, chest).is_empty());
    assert_eq!(viewers(&mut w, anvil), [id(a)]);
    assert!(
        viewers(&mut w, BlockPos::new(0, -61, 0)).is_empty(),
        "no block entity"
    );
}

// pmmp: Player::setCreativeInventory, Player::getCreativeInventory, CreativeInventory::getAll
#[test]
fn a_player_switched_to_another_creative_inventory_is_sent_it_and_takes_from_it() {
    use crate::custom::{Category, CreativeInventory, ItemRef};
    let stick = item(it::Kind::Stick);
    let builder = crate::Server::builder().alternative_creative_inventory(
        "kits",
        CreativeInventory::empty().add(ItemRef::Item(stick), Category::Items, None),
    );
    let mut custom = builder.setup.custom;
    custom.finish().unwrap();
    let mut w = World::new(GameMode::Creative);
    w.state.shared = super::super::super::Shared::new(
        crate::protocol::batch::Settings::default(),
        &custom,
        &super::super::super::entity::Customs::default(),
    )
    .unwrap();
    w.state.custom = std::sync::Arc::new(custom);
    let s = w.join(1);
    let p = crate::player::Id::new(s);
    // The creative batches the output phase sends `s` now, by inventory.
    let sent = |w: &mut World| {
        let out = super::super::super::spawn::output(&mut w.state, &w.pool, &BTreeMap::default());
        let batches = w.state.shared.creative_batches();
        out.into_iter()
            .filter(|(to, _)| *to == s)
            .flat_map(|(_, messages)| messages)
            .filter_map(|m| match m {
                Message::Shared(b) => batches.iter().position(|c| std::sync::Arc::ptr_eq(c, &b)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let mut cx = Context {
        state: &mut w.state,
    };
    assert_eq!(cx.creative_inventory(p), None, "the default");
    assert!(!cx.set_creative_inventory(p, Some("other")));
    assert!(!cx.set_creative_inventory(crate::player::Id::new(SessionId::new(9)), None));
    assert!(cx.set_creative_inventory(p, Some("kits")));
    assert_eq!(cx.creative_inventory(p), Some("kits"));
    assert_eq!(
        cx.creative_items(Some("kits")),
        Some(&[Stack::new(stick, 1)][..])
    );
    assert_eq!(cx.creative_items(None).map(<[Stack]>::len), Some(1634));
    assert_eq!(cx.creative_items(Some("other")), None);
    assert_eq!(sent(&mut w), [1], "the client gets the kits");
    assert!(sent(&mut w).is_empty(), "once");
    // Creative item 1 is the kit's stick now, not PMMP's first entry.
    let id = w.next_id(s);
    let r = w.request_as(
        s,
        id,
        vec![creative(1), place(1, created(id), at(C::HotBar, 0, 0))],
    );
    assert!(ok(&r));
    assert_eq!(w.slot(s, C::HotBar, 0).stack, Stack::new(stick, 1));
    let id = w.next_id(s);
    let r = w.request_as(
        s,
        id,
        vec![creative(2), place(1, created(id), at(C::HotBar, 1, 0))],
    );
    assert!(!ok(&r), "the kits have one entry");
    let mut cx = Context {
        state: &mut w.state,
    };
    assert!(cx.set_creative_inventory(p, None));
    assert!(cx.set_creative_inventory(p, None), "already the default");
    assert_eq!(cx.creative_inventory(p), None);
    assert_eq!(sent(&mut w), [0]);
    check_slots(&w.state);
}
