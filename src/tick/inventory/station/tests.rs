//! Tests of the station windows: each station's rules, conservation (items in = items out +
//! consumed), the same request twice, closing mid-use, and open/close cycles leaving no state
//! behind; the furnace and brewing stand windows and their block entity ticks; breaking containers.

use std::collections::BTreeMap;

use super::super::super::interact::{self, Destroy};
use super::super::super::{State, block_entity, survival};
use super::super::tests::{
    World, at, check_slots, consume, craft, created, dropped, item, ok, place, swap, take, totals,
};
use super::super::{Slot, SlotRef, StackId, close_window, on_item_stack_request, open_trade};
use super::*;
use crate::block::{self as blk, Face};
use crate::config::GameMode;
use crate::crafting::ShapelessType;
use crate::item::{Extra, Kind, Type};
use crate::nbt::Compound;
use crate::protocol::consts::ContainerId as C;
use crate::protocol::packet::Packet;
use crate::protocol::{
    BeaconPaymentStackRequestAction, CraftLoomRecipeStackRequestAction,
    CraftRecipeOptionalStackRequestAction, ItemStackResponse,
};
use crate::world::BlockPos;

/// A station block's position, next to the spawn.
const AT: BlockPos = BlockPos::new(3, -60, 3);
const COMBINED: C = C::CombinedHotBarAndInventory;

fn named(name: &str) -> Type {
    Type::from_name(name, 0, None, &Compound::new()).unwrap()
}

fn named_meta(name: &str, meta: u16) -> Type {
    Type::from_name(name, meta, None, &Compound::new()).unwrap()
}

/// A world with one survival player holding `stacks` (one per main slot from 0) and many levels.
fn world(stacks: &[Stack]) -> (World, super::super::super::SessionId) {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    for stack in stacks {
        w.give(s, stack.clone());
    }
    survival::add_experience(w.player(s), 100_000);
    w.sent(s);
    (w, s)
}

/// Sends a request with filter strings; its response.
fn request_filter(
    w: &mut World,
    s: super::super::super::SessionId,
    id: i32,
    actions: Vec<StackRequestAction>,
    filter: &str,
) -> ItemStackResponse {
    w.state.tick += 1;
    let request = ItemStackRequest {
        request_id: id,
        actions,
        filter_strings: vec![filter.to_owned()],
        filter_cause: 0,
    };
    on_item_stack_request(&mut w.state, s, &request);
    w.sent(s)
        .into_iter()
        .find_map(|p| match p {
            Packet::ItemStackResponse(r) => r.responses.into_iter().next(),
            _ => None,
        })
        .unwrap()
}

/// Moves `count` from main slot `from` into the open window's request slot.
fn put(w: &mut World, s: super::super::super::SessionId, from: u8, count: u8, to: (C, u8)) {
    let id = w.id(s, COMBINED, from);
    let r = w.request(
        s,
        vec![place(count, at(COMBINED, from, id), at(to.0, to.1, 0))],
    );
    assert!(ok(&r), "{r:?}");
}

/// The window's own slot `i`.
fn ui_stack(w: &World, s: super::super::super::SessionId, i: u8) -> Stack {
    w.inv(s).get(SlotRef::Ui(i)).unwrap().stack
}

/// Sets the window's own slot `i` directly (outside a request) with stack network ID `net`.
fn set_ui(w: &mut World, s: super::super::super::SessionId, i: u8, stack: Stack, net: i32) {
    let slot = Slot {
        stack,
        id: StackId { net, request: None },
    };
    assert!(w.player(s).inventory.put(SlotRef::Ui(i), slot));
}

fn total_count(state: &State) -> u64 {
    totals(state).values().sum()
}

#[test]
fn a_furnace_window_cooks_with_bds_data_and_the_output_gives_its_experience() {
    let (mut w, s) = world(&[
        Stack::new(item(Kind::Coal), 1),
        Stack::new(named("minecraft:raw_iron"), 2),
    ]);
    w.place_block(AT, blk::Kind::Furnace.default_state());
    w.open_block(s, AT);
    let keys: Vec<i32> = w
        .sent(s)
        .iter()
        .filter_map(|p| match p {
            Packet::ContainerSetData(d) => Some(d.key),
            _ => None,
        })
        .collect();
    assert_eq!(keys, vec![0, 1, 2, 3], "BDS sends keys 0 to 3 on open");
    put(&mut w, s, 0, 1, (C::FurnaceFuel, 1));
    put(&mut w, s, 1, 2, (C::FurnaceIngredient, 0));
    w.tick(Vec::new());
    let lit = w.state.worlds[0].block(AT).unwrap();
    assert!(lit.name().contains("lit_"), "{lit:?}");
    for _ in 0..400 {
        w.tick(Vec::new());
    }
    let out = w.slot(s, C::FurnaceResult, 2);
    assert_eq!(out.stack, Stack::new(named("minecraft:iron_ingot"), 2));
    assert!(w.slot(s, C::FurnaceIngredient, 0).stack.is_empty());
    let r = w.request(
        s,
        vec![take(
            2,
            at(C::FurnaceResult, 2, out.id.net),
            at(COMBINED, 5, 0),
        )],
    );
    assert!(ok(&r));
    assert_eq!(
        w.inv(s).main.stack(5),
        &Stack::new(named("minecraft:iron_ingot"), 2)
    );
    let furnace = w.state.rules.block_entities.get(0, AT).unwrap();
    assert!(
        matches!(&furnace.kind, block_entity::Kind::Furnace(f) if f.xp == 0),
        "taking the output takes the stored experience"
    );
    check_slots(&w.state);
}

/// `experience = false`: a furnace stores no experience for what it smelts.
#[test]
fn without_experience_a_furnace_stores_no_experience() {
    let (mut w, s) = world(&[
        Stack::new(item(Kind::Coal), 1),
        Stack::new(named("minecraft:raw_iron"), 8),
    ]);
    w.state.settings.experience = false;
    w.place_block(AT, blk::Kind::Furnace.default_state());
    w.open_block(s, AT);
    put(&mut w, s, 0, 1, (C::FurnaceFuel, 1));
    put(&mut w, s, 1, 8, (C::FurnaceIngredient, 0));
    for _ in 0..1700 {
        w.tick(Vec::new());
    }
    assert_eq!(
        w.slot(s, C::FurnaceResult, 2).stack,
        Stack::new(named("minecraft:iron_ingot"), 8)
    );
    let furnace = w.state.rules.block_entities.get(0, AT).unwrap();
    assert!(matches!(&furnace.kind, block_entity::Kind::Furnace(f) if f.xp == 0));
}

#[test]
fn a_furnace_opens_without_block_actor_data_and_its_tick_keeps_the_ids_of_counts() {
    let (mut w, s) = world(&[
        Stack::new(item(Kind::Coal), 1),
        Stack::new(named("minecraft:raw_iron"), 3),
    ]);
    // A block without its block entity (a task or the console set it): opening creates one.
    w.state.worlds[0]
        .set_block(AT, blk::Kind::Furnace.default_state())
        .unwrap();
    w.sent(s);
    w.open_block(s, AT);
    assert!(w.state.rules.block_entities.get(0, AT).is_some());
    assert!(
        !w.sent(s)
            .iter()
            .any(|p| matches!(p, Packet::BlockActorData(_))),
        "BDS sends no BlockActorData when a window opens"
    );
    put(&mut w, s, 0, 1, (C::FurnaceFuel, 1));
    put(&mut w, s, 1, 3, (C::FurnaceIngredient, 0));
    let input = w.slot(s, C::FurnaceIngredient, 0).id.net;
    let mut slots: BTreeMap<u32, Vec<(u16, i32)>> = BTreeMap::new();
    let mut last_fuel = Vec::new();
    for _ in 0..700 {
        for p in w.tick_delivered(Vec::new()) {
            match p {
                Packet::InventorySlot(p) => slots
                    .entry(p.slot)
                    .or_default()
                    .push((p.new_item.stack.count, p.new_item.stack_network_id)),
                Packet::BlockActorData(d) => last_fuel.push(d.nbt_data),
                _ => {}
            }
        }
    }
    assert_eq!(
        slots[&0],
        vec![(2, input), (1, input), (0, 0)],
        "the input keeps its ID while it shrinks"
    );
    assert_eq!(slots[&1], vec![(0, 0)], "the fuel burnt");
    let output = slots[&2][0].1;
    assert_ne!(output, 0);
    assert_eq!(
        slots[&2],
        vec![(1, output), (2, output), (3, output)],
        "the output gets a new ID once, then keeps it as it grows"
    );
    let coal = Stack::new(item(Kind::Coal), 1).to_nbt();
    let mut fuel = coal.clone();
    fuel.insert("WasPickedUp".to_owned(), crate::nbt::Value::Byte(0));
    assert_eq!(
        last_fuel,
        vec![Compound::from([(
            "LastFuel".to_owned(),
            crate::nbt::Value::Compound(fuel)
        )])],
        "BDS: the last fuel with WasPickedUp, once"
    );
    check_slots(&w.state);
}

#[test]
fn a_brewing_stand_window_brews_every_bottle_with_one_ingredient_and_blaze_powder() {
    let water = item(Kind::Potion);
    let (mut w, s) = world(&[
        Stack::new(named("minecraft:nether_wart"), 1),
        Stack::new(water, 1),
        Stack::new(water, 1),
        Stack::new(item(Kind::BlazePowder), 1),
    ]);
    w.place_block(AT, blk::Kind::BrewingStand.default_state());
    w.open_block(s, AT);
    put(&mut w, s, 0, 1, (C::BrewingStandInput, 0));
    put(&mut w, s, 1, 1, (C::BrewingStandResult, 1));
    put(&mut w, s, 2, 1, (C::BrewingStandResult, 3));
    put(&mut w, s, 3, 1, (C::BrewingStandFuel, 4));
    for _ in 0..401 {
        w.tick(Vec::new());
    }
    let awkward = crate::crafting::brew_potion_type(
        &Stack::new(water, 1),
        &Stack::new(named("minecraft:nether_wart"), 1),
    )
    .unwrap();
    assert_eq!(w.slot(s, C::BrewingStandResult, 1).stack, awkward);
    assert_eq!(w.slot(s, C::BrewingStandResult, 3).stack, awkward);
    assert!(w.slot(s, C::BrewingStandInput, 0).stack.is_empty());
    assert!(w.slot(s, C::BrewingStandFuel, 4).stack.is_empty());
}

/// Opens an enchanting table with a pickaxe in main slot 0 and lapis in slot 1, puts the pickaxe
/// in: the options shown.
fn enchanting(
    lapis: u8,
) -> (
    World,
    super::super::super::SessionId,
    Vec<(u32, enchant::Choice)>,
) {
    let (mut w, s) = world(&[
        Stack::new(item(Kind::IronPickaxe), 1),
        Stack::new(item(Kind::LapisLazuli), lapis),
    ]);
    w.place_block(AT, blk::Kind::EnchantingTable.default_state());
    w.open_block(s, AT);
    let opened = w.sent(s);
    assert!(
        opened
            .iter()
            .any(|p| matches!(p, Packet::PlayerEnchantOptions(o) if o.options.is_empty()))
    );
    put(&mut w, s, 0, 1, (C::EnchantingInput, 14));
    put(&mut w, s, 1, lapis, (C::EnchantingMaterial, 15));
    let options = w.inv(s).window.as_ref().unwrap().station.options.clone();
    assert_eq!(options.len(), 3);
    assert!(options[0].0 >= crate::crafting::first_special_id());
    (w, s, options)
}

// pmmp: EnchantInventory::getInput, EnchantInventory::getLapis
#[test]
fn the_window_slots_read_the_enchanting_input_and_lapis() {
    let (w, s, _) = enchanting(3);
    let player = &w.state.players[&s];
    assert_eq!(
        player.window_item(0),
        Some(&Stack::new(item(Kind::IronPickaxe), 1))
    );
    assert_eq!(
        player.window_item(1),
        Some(&Stack::new(item(Kind::LapisLazuli), 3))
    );
    assert_eq!(player.window_item(2), None);
}

#[test]
fn enchanting_options_have_the_bds_shape() {
    let (_, _, options) = enchanting(3);
    let Packet::PlayerEnchantOptions(packet) = options_packet(1024, &options) else {
        panic!("not an options packet");
    };
    for (sent, (id, choice)) in packet.options.iter().zip(&options) {
        assert_eq!(sent.enchantments.slot, 1024, "a pickaxe's slot flag (BDS)");
        assert_eq!(sent.recipe_network_id, *id);
        let mut all = Vec::new();
        for (list, entries) in sent.enchantments.enchantments.iter().enumerate() {
            for e in entries {
                let (kind, level) = choice
                    .enchantments
                    .iter()
                    .copied()
                    .find(|(k, _)| k.id() == e.r#type)
                    .unwrap();
                assert_eq!(enchant::activation(kind), list, "{kind:?}");
                all.push((kind, level));
            }
        }
        assert_eq!(all.len(), choice.enchantments.len());
    }
    // BDS: unbreaking in the held list, efficiency in the self list (P6 `enchant`).
    assert_eq!(enchant::activation(crate::item::Enchantment::Unbreaking), 1);
    assert_eq!(enchant::activation(crate::item::Enchantment::Efficiency), 2);
    assert_eq!(enchant::slot(Some(item(Kind::IronPickaxe))), 1024);
    assert_eq!(enchant::slot(None), 0);
}

/// The BDS enchant request for option `index` (P6).
fn enchant_request(
    w: &World,
    s: super::super::super::SessionId,
    option: u32,
    index: u8,
    id: i32,
) -> Vec<StackRequestAction> {
    let input = w.id(s, C::EnchantingInput, 14);
    let lapis = w.id(s, C::EnchantingMaterial, 15);
    vec![
        craft(option, 1),
        consume(1, at(C::EnchantingInput, 14, input)),
        consume(index + 1, at(C::EnchantingMaterial, 15, lapis)),
        place(1, created(id), at(C::EnchantingInput, 14, id)),
    ]
}

#[test]
fn enchanting_follows_the_option_spends_lapis_and_levels_and_reseeds() {
    let (mut w, s, options) = enchanting(3);
    let level = survival::xp_level(w.player(s));
    let before = total_count(&w.state);
    let id = w.next_id(s);
    let actions = enchant_request(&w, s, options[1].0, 1, id);
    assert!(ok(&w.request_as(s, id, actions)));
    let out = ui_stack(&w, s, 0);
    assert_eq!(out.item(), Some(item(Kind::IronPickaxe)));
    assert_eq!(out.extra().unwrap().enchantments, options[1].1.enchantments);
    assert_eq!(
        ui_stack(&w, s, 1).count(),
        1,
        "the second option costs two lapis"
    );
    assert_eq!(survival::xp_level(w.player(s)), level - 2);
    assert_eq!(
        survival::enchantment_seed(w.player(s)),
        0,
        "a new seed is drawn"
    );
    assert_eq!(total_count(&w.state), before - 2);
    // The enchanted input has no options: the empty list went out with the response.
    assert!(w.inv(s).window.as_ref().unwrap().station.options.is_empty());
    check_slots(&w.state);
}

// PMMP `EnchantingTransaction::execute`: levels go with finite resources, every mode but creative.
#[test]
fn a_spectator_enchanting_spends_the_levels() {
    let (mut w, s, options) = enchanting(3);
    assert!(w.player(s).set_game_mode(GameMode::Spectator));
    let level = survival::xp_level(w.player(s));
    let id = w.next_id(s);
    let actions = enchant_request(&w, s, options[1].0, 1, id);
    assert!(ok(&w.request_as(s, id, actions)));
    assert_eq!(survival::xp_level(w.player(s)), level - 2);
}

#[test]
fn enchanting_refuses_stale_options_short_lapis_and_a_repeat() {
    let (mut w, s, options) = enchanting(1);
    // Two lapis for the second option, but only one there.
    let id = w.next_id(s);
    let actions = enchant_request(&w, s, options[1].0, 1, id);
    assert!(!ok(&w.request_as(s, id, actions)));
    let id = w.next_id(s);
    let actions = enchant_request(&w, s, options[0].0 + 100, 0, id);
    assert!(!ok(&w.request_as(s, id, actions)), "an option not shown");
    // The same request twice changes the state once.
    let run = |twice: bool| {
        let (mut w, s, options) = enchanting(3);
        let actions = enchant_request(&w, s, options[0].0, 0, -101);
        assert!(ok(&w.request_as(s, -101, actions.clone())));
        if twice {
            assert!(!ok(&w.request_as(s, -101, actions)));
        }
        w.state.tick = 1000;
        w.state.hash()
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn enchanting_refuses_an_option_after_the_input_is_swapped() {
    let (mut w, s, options) = enchanting(3);
    w.give(s, Stack::new(item(Kind::IronSword), 1));
    let before = total_count(&w.state);
    // The pickaxe's option, taken for a sword swapped into the input in the same request.
    let id = w.next_id(s);
    let actions = vec![
        swap(at(C::EnchantingInput, 14, id), at(COMBINED, 0, id)),
        craft(options[0].0, 1),
        consume(1, at(C::EnchantingInput, 14, id)),
        consume(1, at(C::EnchantingMaterial, 15, id)),
        place(1, created(id), at(C::EnchantingInput, 14, id)),
    ];
    assert!(!ok(&w.request_as(s, id, actions)));
    assert_eq!(ui_stack(&w, s, 0), Stack::new(item(Kind::IronPickaxe), 1));
    assert_eq!(total_count(&w.state), before);
    check_slots(&w.state);
}

#[test]
fn enchanting_option_ids_follow_custom_recipes() {
    use crate::custom::{Ingredient, ItemRef, Recipe, Registry};
    let mut custom = Registry::default();
    for result in [Kind::Diamond, Kind::Emerald] {
        custom.add_recipe(Recipe::shapeless(
            &[Ingredient::Item(ItemRef::Item(item(Kind::Stick)))],
            &[(ItemRef::Item(item(result)), 1)],
        ));
    }
    custom.finish().unwrap();
    let first = custom.first_special_id();
    assert_eq!(first, crate::crafting::first_special_id() + 2);
    let (mut w, s) = world(&[
        Stack::new(item(Kind::IronPickaxe), 1),
        Stack::new(item(Kind::LapisLazuli), 3),
    ]);
    w.state.custom = std::sync::Arc::new(custom);
    w.place_block(AT, blk::Kind::EnchantingTable.default_state());
    w.open_block(s, AT);
    put(&mut w, s, 0, 1, (C::EnchantingInput, 14));
    let options = w.inv(s).window.as_ref().unwrap().station.options.clone();
    assert_eq!(options.len(), 3);
    // No option shares a network ID with a custom recipe.
    assert_eq!(options[0].0, first);
    assert!(options.iter().all(|(id, _)| *id >= first));
}

#[test]
fn the_anvil_renames_and_repairs_for_levels() {
    let pick = Stack::new(item(Kind::IronPickaxe), 1)
        .with_damage(200)
        .unwrap();
    let (mut w, s) = world(&[pick.clone(), Stack::new(item(Kind::IronIngot), 5)]);
    w.place_block(AT, blk::Kind::Anvil.default_state());
    w.open_block(s, AT);
    put(&mut w, s, 0, 1, (C::AnvilInput, 1));
    put(&mut w, s, 1, 5, (C::AnvilMaterial, 2));
    let level = survival::xp_level(w.player(s));
    let expect = anvil::craft(
        &pick,
        &Stack::new(item(Kind::IronIngot), 5),
        Some("Pick"),
        false,
    )
    .unwrap();
    let input = w.id(s, C::AnvilInput, 1);
    let material = w.id(s, C::AnvilMaterial, 2);
    let id = w.next_id(s);
    let optional = StackRequestAction::CraftRecipeOptional(CraftRecipeOptionalStackRequestAction {
        recipe_network_id: 0,
        filter_string_index: 0,
    });
    let actions = vec![
        optional.clone(),
        consume(1, at(C::AnvilInput, 1, input)),
        consume(expect.material_used, at(C::AnvilMaterial, 2, material)),
        place(1, created(id), at(COMBINED, 7, 0)),
    ];
    // Consuming the wrong amount of material is refused.
    let mut wrong = actions.clone();
    wrong[2] = consume(5, at(C::AnvilMaterial, 2, material));
    assert!(!ok(&request_filter(&mut w, s, -501, wrong, "Pick")));
    assert!(ok(&request_filter(&mut w, s, id, actions, "Pick")));
    let got = w.inv(s).main.stack(7).clone();
    assert_eq!(got, expect.result);
    assert_eq!(got.custom_name(), "Pick");
    assert_eq!(survival::xp_level(w.player(s)), level - expect.cost);
    assert_eq!(ui_stack(&w, s, 1).count(), 5 - expect.material_used);
    check_slots(&w.state);
}

/// A survival player with 64 sticks at an open anvil.
fn anvil_world() -> (World, super::super::super::SessionId) {
    let (mut w, s) = world(&[Stack::new(item(Kind::Stick), 64)]);
    w.place_block(AT, blk::Kind::Anvil.default_state());
    w.open_block(s, AT);
    (w, s)
}

/// Sends a request in the current tick, which the server takes.
fn send_now(
    w: &mut World,
    s: super::super::super::SessionId,
    id: i32,
    actions: Vec<StackRequestAction>,
) {
    let request = ItemStackRequest {
        request_id: id,
        actions,
        filter_strings: vec!["x".to_owned()],
        filter_cause: 0,
    };
    on_item_stack_request(&mut w.state, s, &request);
    let sent = w.sent(s);
    assert!(sent.iter().any(|p| matches!(
        p,
        Packet::ItemStackResponse(r) if r.responses.iter().all(ok)
    )));
}

/// Renames one stick into main slot 1 in the current tick; whether the anvil took damage.
fn rename_stick(w: &mut World, s: super::super::super::SessionId) -> bool {
    let before = w.state.worlds[0].block(AT);
    let id = w.next_id(s);
    let from = w.id(s, COMBINED, 0);
    send_now(
        w,
        s,
        id,
        vec![place(1, at(COMBINED, 0, from), at(C::AnvilInput, 1, 0))],
    );
    let id = w.next_id(s);
    let (input, to) = (w.id(s, C::AnvilInput, 1), w.id(s, COMBINED, 1));
    let optional = StackRequestAction::CraftRecipeOptional(CraftRecipeOptionalStackRequestAction {
        recipe_network_id: 0,
        filter_string_index: 0,
    });
    send_now(
        w,
        s,
        id,
        vec![
            optional,
            consume(1, at(C::AnvilInput, 1, input)),
            place(1, created(id), at(COMBINED, 1, to)),
        ],
    );
    w.state.worlds[0].block(AT) != before
}

/// Starts the next tick with an undamaged anvil.
fn next_tick_with_new_anvil(w: &mut World) {
    w.state.tick += 1;
    w.state.worlds[0]
        .set_block(AT, blk::Kind::Anvil.default_state())
        .unwrap();
}

/// Each use of an anvil draws its own damage chance, also several uses of one player in one tick.
#[test]
fn anvil_uses_in_one_tick_draw_their_damage_apart() {
    let (mut w, s) = anvil_world();
    let mut apart = false;
    for _ in 0..32 {
        next_tick_with_new_anvil(&mut w);
        apart |= rename_stick(&mut w, s) != rename_stick(&mut w, s);
    }
    assert_eq!(
        w.inv(s).main.stack(1).count(),
        64,
        "every rename went through"
    );
    assert!(
        apart,
        "two uses in one tick never differed: they share one draw"
    );
}

/// A refused request earlier in the tick does not change how an anvil use comes out
/// (idempotence: a repeated request leaves the game as one).
#[test]
fn a_refused_repeat_does_not_change_a_later_anvil_use() {
    let (mut once, s) = anvil_world();
    let (mut twice, _) = anvil_world();
    for i in 0..32 {
        next_tick_with_new_anvil(&mut once);
        next_tick_with_new_anvil(&mut twice);
        if i > 0 {
            // The first request ID of the first tick again: refused as a repeat.
            let repeat = ItemStackRequest {
                request_id: -1,
                actions: Vec::new(),
                filter_strings: Vec::new(),
                filter_cause: 0,
            };
            on_item_stack_request(&mut twice.state, s, &repeat);
            twice.sent(s);
        }
        assert_eq!(
            rename_stick(&mut once, s),
            rename_stick(&mut twice, s),
            "tick {i}"
        );
    }
}

/// Stacks a server gives with the highest enchantment level, or the highest prior work, go into
/// the anvil: the level cannot climb past the maximum and the prior-work cost does not overflow.
/// With no rename the work is nothing or too expensive, so each is refused and nothing changes.
#[test]
fn the_anvil_handles_levels_and_prior_work_at_their_limits() {
    let sword = |damage: u16, extra: Extra| {
        Stack::new(item(Kind::DiamondSword), 1)
            .with_damage(damage)
            .unwrap()
            .with_extra(Some(extra))
            .unwrap()
    };
    let highest = Extra {
        enchantments: vec![(crate::item::Enchantment::Sharpness, u8::MAX)],
        ..Extra::default()
    };
    let worked = Extra {
        nbt: Compound::from([("RepairCost".to_owned(), crate::nbt::Value::Int(i32::MAX))]),
        ..Extra::default()
    };
    for (input, material) in [
        (sword(0, highest.clone()), sword(0, highest)),
        (sword(100, worked.clone()), sword(0, worked)),
    ] {
        // `craft` must not overflow, whatever the levels or prior work.
        assert!(anvil::craft(&input, &material, None, false).is_err());
        let (mut w, s) = world(&[input, material]);
        w.place_block(AT, blk::Kind::Anvil.default_state());
        w.open_block(s, AT);
        put(&mut w, s, 0, 1, (C::AnvilInput, 1));
        put(&mut w, s, 1, 1, (C::AnvilMaterial, 2));
        let before = totals(&w.state);
        let level = survival::xp_level(w.player(s));
        let input_id = w.id(s, C::AnvilInput, 1);
        let material_id = w.id(s, C::AnvilMaterial, 2);
        let id = w.next_id(s);
        let actions = vec![
            StackRequestAction::CraftRecipeOptional(CraftRecipeOptionalStackRequestAction {
                recipe_network_id: 0,
                filter_string_index: 0,
            }),
            consume(1, at(C::AnvilInput, 1, input_id)),
            consume(1, at(C::AnvilMaterial, 2, material_id)),
            place(1, created(id), at(COMBINED, 7, 0)),
        ];
        let request = ItemStackRequest {
            request_id: id,
            actions,
            filter_strings: Vec::new(),
            filter_cause: 0,
        };
        w.state.tick += 1;
        on_item_stack_request(&mut w.state, s, &request);
        assert_eq!(totals(&w.state), before);
        assert_eq!(survival::xp_level(w.player(s)), level);
        check_slots(&w.state);
    }
}

/// `experience = false`: the levels a player kept count as none, so a survival player takes no
/// enchanting option and no anvil work, and keeps its levels.
#[test]
fn without_experience_enchanting_and_the_anvil_take_no_levels_and_refuse() {
    let (mut w, s, options) = enchanting(3);
    w.state.settings.experience = false;
    let level = survival::xp_level(w.player(s));
    let id = w.next_id(s);
    let actions = enchant_request(&w, s, options[0].0, 0, id);
    assert!(!ok(&w.request_as(s, id, actions)));
    assert_eq!(ui_stack(&w, s, 0), Stack::new(item(Kind::IronPickaxe), 1));
    assert_eq!(survival::xp_level(w.player(s)), level);
    let pick = Stack::new(item(Kind::IronPickaxe), 1)
        .with_damage(200)
        .unwrap();
    let (mut w, s) = world(&[pick.clone(), Stack::new(item(Kind::IronIngot), 5)]);
    w.state.settings.experience = false;
    w.place_block(AT, blk::Kind::Anvil.default_state());
    w.open_block(s, AT);
    put(&mut w, s, 0, 1, (C::AnvilInput, 1));
    put(&mut w, s, 1, 5, (C::AnvilMaterial, 2));
    let used = anvil::craft(
        &pick,
        &Stack::new(item(Kind::IronIngot), 5),
        Some("Pick"),
        false,
    )
    .unwrap()
    .material_used;
    let input = w.id(s, C::AnvilInput, 1);
    let material = w.id(s, C::AnvilMaterial, 2);
    let id = w.next_id(s);
    let actions = vec![
        StackRequestAction::CraftRecipeOptional(CraftRecipeOptionalStackRequestAction {
            recipe_network_id: 0,
            filter_string_index: 0,
        }),
        consume(1, at(C::AnvilInput, 1, input)),
        consume(used, at(C::AnvilMaterial, 2, material)),
        place(1, created(id), at(COMBINED, 7, 0)),
    ];
    assert!(!ok(&request_filter(&mut w, s, id, actions, "Pick")));
    assert!(w.inv(s).main.stack(7).is_empty());
    check_slots(&w.state);
}

/// A stack an ingredient takes (exact items as they are, wildcards by name, tags by their
/// first item).
fn sample(i: &Ingredient) -> Option<Stack> {
    match i {
        Ingredient::Exact(e) => {
            Some(Stack::from_parts(e.item, 1, e.meta, e.nbt.map(crate::nbt::compound)).unwrap())
        }
        Ingredient::AnyMeta(n) => {
            Type::from_network_any_block(i32::from(n.runtime_id), 0, 0, &Compound::new())
                .map(|t| Stack::new(t, 1))
        }
        Ingredient::Tag(_) => None,
    }
}

#[test]
fn every_stonecutter_recipe_cuts_and_a_wrong_input_is_refused() {
    let (mut w, s) = world(&[]);
    w.place_block(AT, blk::Kind::Stonecutter.default_state());
    w.open_block(s, AT);
    let mut cut = 0;
    for rid in 1.. {
        let Some(recipe) = crate::crafting::recipe(rid) else {
            break;
        };
        let Recipe::Shapeless(r) = recipe else {
            continue;
        };
        if r.kind != ShapelessType::Stonecutter {
            continue;
        }
        let input =
            sample(&r.ingredients[0]).unwrap_or_else(|| panic!("{rid}: {:?}", r.ingredients[0]));
        let result = r.results[0].to_item().unwrap();
        set_ui(&mut w, s, 0, input.with_count(2), 900);
        let id = w.next_id(s);
        let actions = vec![
            craft(rid, 2),
            consume(2, at(C::StonecutterInput, 3, 900)),
            place(result.count() * 2, created(id), at(COMBINED, 0, 0)),
        ];
        let r = w.request_as(s, id, actions);
        assert!(ok(&r), "recipe {rid}");
        assert_eq!(
            w.inv(s).main.stack(0),
            &result.with_count(result.count() * 2)
        );
        assert!(ui_stack(&w, s, 0).is_empty());
        w.player(s).inventory.main.stacks[0] = Stack::EMPTY;
        w.player(s).inventory.main.ids[0] = StackId::default();
        cut += 1;
        if cut == 1 {
            // A stick is no stonecutter input.
            set_ui(&mut w, s, 0, Stack::new(item(Kind::Stick), 1), 901);
            let id = w.next_id(s);
            let actions = vec![
                craft(rid, 1),
                consume(1, at(C::StonecutterInput, 3, 901)),
                place(result.count(), created(id), at(COMBINED, 0, 0)),
            ];
            assert!(!ok(&w.request_as(s, id, actions)));
            set_ui(&mut w, s, 0, Stack::EMPTY, 0);
        }
    }
    assert!(cut > 100, "{cut} stonecutter recipes");
}

/// The smithing request for recipe `rid` making `result` (dragonfly; BDS consumes each slot).
fn smith(w: &mut World, s: super::super::super::SessionId, rid: u32) -> ItemStackResponse {
    let ids: Vec<i32> = (0..3)
        .map(|i| w.inv(s).get(SlotRef::Ui(i)).unwrap().id.net)
        .collect();
    let id = w.next_id(s);
    let actions = vec![
        craft(rid, 1),
        consume(1, at(C::SmithingTableInput, 51, ids[0])),
        consume(1, at(C::SmithingTableMaterial, 52, ids[1])),
        consume(1, at(C::SmithingTableTemplate, 53, ids[2])),
        place(1, created(id), at(COMBINED, 8, 0)),
    ];
    w.request_as(s, id, actions)
}

#[test]
fn smithing_upgrades_keeping_enchantments_and_trims_armour() {
    let sword = Stack::new(item(Kind::DiamondSword), 1)
        .with_damage(30)
        .unwrap()
        .with_extra(Some(Extra {
            enchantments: vec![(crate::item::Enchantment::Sharpness, 3)],
            ..Extra::default()
        }))
        .unwrap();
    let (mut w, s) = world(&[
        sword.clone(),
        Stack::new(item(Kind::NetheriteIngot), 1),
        Stack::new(named("minecraft:netherite_upgrade_smithing_template"), 1),
        Stack::new(item(Kind::IronChestplate), 1),
        Stack::new(item(Kind::GoldIngot), 1),
        Stack::new(named("minecraft:ward_armor_trim_smithing_template"), 1),
    ]);
    w.place_block(AT, blk::Kind::SmithingTable.default_state());
    w.open_block(s, AT);
    put(&mut w, s, 0, 1, (C::SmithingTableInput, 51));
    put(&mut w, s, 1, 1, (C::SmithingTableMaterial, 52));
    put(&mut w, s, 2, 1, (C::SmithingTableTemplate, 53));
    let recipes: Vec<(u32, Recipe<'static>)> = (1..)
        .map_while(|id| crate::crafting::recipe(id).map(|r| (id, r)))
        .collect();
    let upgrade = recipes
        .iter()
        .find(|(_, r)| {
            matches!(r, Recipe::SmithingTransform(t)
            if t.result.item == item(Kind::NetheriteSword))
        })
        .unwrap()
        .0;
    let trim = recipes
        .iter()
        .find(|(_, r)| matches!(r, Recipe::SmithingTrim(_)))
        .unwrap()
        .0;
    assert!(!ok(&smith(&mut w, s, trim)), "a sword takes no trim");
    assert!(ok(&smith(&mut w, s, upgrade)));
    let got = w.inv(s).main.stack(8).clone();
    assert_eq!(got.item(), Some(item(Kind::NetheriteSword)));
    assert_eq!(got.damage(), 30);
    assert_eq!(got.extra(), sword.extra());
    put(&mut w, s, 3, 1, (C::SmithingTableInput, 51));
    put(&mut w, s, 4, 1, (C::SmithingTableMaterial, 52));
    put(&mut w, s, 5, 1, (C::SmithingTableTemplate, 53));
    w.player(s).inventory.main.stacks[8] = Stack::EMPTY;
    w.player(s).inventory.main.ids[8] = StackId::default();
    assert!(ok(&smith(&mut w, s, trim)));
    let trimmed = w.inv(s).main.stack(8).clone();
    let trim_tag = trimmed.extra().unwrap().nbt.get("Trim").cloned();
    assert_eq!(
        trim_tag,
        Some(crate::nbt::Value::Compound(Compound::from([
            (
                "Material".to_owned(),
                crate::nbt::Value::String("gold".to_owned())
            ),
            (
                "Pattern".to_owned(),
                crate::nbt::Value::String("ward".to_owned())
            ),
        ])))
    );
}

fn loom(pattern: &str, times: u8) -> StackRequestAction {
    StackRequestAction::CraftLoomRecipe(CraftLoomRecipeStackRequestAction {
        pattern: pattern.to_owned(),
        times_crafted: times,
    })
}

#[test]
fn the_loom_adds_a_pattern_in_the_dye_colour_and_refuses_item_patterns() {
    let white = named_meta("minecraft:banner", 15);
    let (mut w, s) = world(&[
        Stack::new(white, 2),
        Stack::new(named("minecraft:red_dye"), 2),
    ]);
    w.place_block(AT, blk::Kind::Loom.default_state());
    w.open_block(s, AT);
    put(&mut w, s, 0, 2, (C::LoomInput, 9));
    put(&mut w, s, 1, 2, (C::LoomDye, 10));
    let loom_request = |w: &mut World, pattern: &str| {
        let (b, d) = (w.id(s, C::LoomInput, 9), w.id(s, C::LoomDye, 10));
        let id = w.next_id(s);
        let actions = vec![
            loom(pattern, 2),
            consume(2, at(C::LoomInput, 9, b)),
            consume(2, at(C::LoomDye, 10, d)),
            place(2, created(id), at(COMBINED, 4, 0)),
        ];
        w.request_as(s, id, actions)
    };
    assert!(
        !ok(&loom_request(&mut w, "cre")),
        "the creeper needs its pattern item"
    );
    assert!(!ok(&loom_request(&mut w, "nope")));
    assert!(ok(&loom_request(&mut w, "bo")));
    let got = w.inv(s).main.stack(4).clone();
    assert_eq!(got.count(), 2);
    assert_eq!(got.item(), Some(white));
    let patterns = got.extra().unwrap().nbt.get("Patterns").cloned().unwrap();
    // Red is dye 14: inverted 1.
    assert_eq!(
        patterns,
        crate::nbt::Value::List(crate::nbt::List::Compound(vec![Compound::from([
            (
                "Pattern".to_owned(),
                crate::nbt::Value::String("bo".to_owned())
            ),
            ("Color".to_owned(), crate::nbt::Value::Int(1)),
        ])]))
    );
}

fn payment(primary: i32, secondary: i32) -> StackRequestAction {
    StackRequestAction::BeaconPayment(BeaconPaymentStackRequestAction {
        primary_effect: primary,
        secondary_effect: secondary,
    })
}

#[test]
fn a_beacon_takes_one_payment_for_effects_its_pyramid_allows() {
    let (mut w, s) = world(&[Stack::new(item(Kind::IronIngot), 3)]);
    let iron = blk::State::find("minecraft:iron_block", &[]).unwrap();
    for x in -1..=1 {
        for z in -1..=1 {
            w.state.worlds[0]
                .set_block(BlockPos::new(AT.x + x, AT.y - 1, AT.z + z), iron)
                .unwrap();
        }
    }
    w.place_block(AT, blk::Kind::Beacon.default_state());
    assert_eq!(block_entity::beacon_level(&w.state, 0, AT), 1);
    w.open_block(s, AT);
    put(&mut w, s, 0, 1, (C::BeaconPayment, 27));
    let pay = |w: &mut World, primary: i32| {
        let id = w.id(s, C::BeaconPayment, 27);
        w.request(
            s,
            vec![
                payment(primary, 0),
                consume(1, at(C::BeaconPayment, 27, id)),
            ],
        )
    };
    assert!(!ok(&pay(&mut w, 5)), "strength needs three levels");
    assert!(ok(&pay(&mut w, 1)));
    assert!(ui_stack(&w, s, 0).is_empty());
    assert!(matches!(
        w.state.rules.block_entities.get(0, AT).unwrap().kind,
        block_entity::Kind::Beacon {
            primary: 1,
            secondary: 0
        }
    ));
    // A payment swapped out for a stick within the request pays nothing.
    put(&mut w, s, 0, 1, (C::BeaconPayment, 27));
    w.give(s, Stack::new(item(Kind::Stick), 1));
    let (iron_id, stick_id) = (w.id(s, C::BeaconPayment, 27), w.id(s, COMBINED, 1));
    let r = w.request(
        s,
        vec![
            payment(3, 0),
            swap(at(C::BeaconPayment, 27, iron_id), at(COMBINED, 1, stick_id)),
            consume(1, at(C::BeaconPayment, 27, iron_id)),
        ],
    );
    assert!(!ok(&r), "a stick is no payment");
    assert_eq!(ui_stack(&w, s, 0), Stack::new(item(Kind::IronIngot), 1));
    assert!(matches!(
        w.state.rules.block_entities.get(0, AT).unwrap().kind,
        block_entity::Kind::Beacon { primary: 1, .. }
    ));
}

#[test]
fn the_trade_window_takes_payments_and_gives_them_back_on_close() {
    let (mut w, s) = world(&[Stack::new(item(Kind::Emerald), 10)]);
    let window = open_trade(&mut w.state, s, crate::entity::Id::new(1, 0)).unwrap();
    assert_eq!(w.inv(s).window.as_ref().unwrap().id, window);
    assert!(w.sent(s).is_empty(), "the trade module sends UpdateTrade");
    put(&mut w, s, 0, 4, (C::TradeTwoIngredientOne, 4));
    let before = totals(&w.state);
    // No such offer (the trade module offers none here): refused.
    let id = w.next_id(s);
    let pay = w.id(s, C::TradeTwoIngredientOne, 4);
    let actions = vec![
        craft(5000, 1),
        consume(4, at(C::TradeTwoIngredientOne, 4, pay)),
        place(1, created(id), at(COMBINED, 3, 0)),
    ];
    assert!(!ok(&w.request_as(s, id, actions)));
    w.client_close(s, window);
    assert_eq!(totals(&w.state), before);
    assert_eq!(w.inv(s).main.stack(0).count(), 10);
    assert!(w.inv(s).window.is_none());
}

/// Each station block and the request slot of its first input.
fn stations() -> Vec<(blk::State, (C, u8))> {
    vec![
        (
            blk::Kind::EnchantingTable.default_state(),
            (C::EnchantingInput, 14),
        ),
        (blk::Kind::Anvil.default_state(), (C::AnvilInput, 1)),
        (
            blk::Kind::SmithingTable.default_state(),
            (C::SmithingTableInput, 51),
        ),
        (
            blk::Kind::Stonecutter.default_state(),
            (C::StonecutterInput, 3),
        ),
        (blk::Kind::Loom.default_state(), (C::LoomInput, 9)),
        (blk::Kind::Beacon.default_state(), (C::BeaconPayment, 27)),
        (
            blk::Kind::Furnace.default_state(),
            (C::FurnaceIngredient, 0),
        ),
        (
            blk::Kind::BrewingStand.default_state(),
            (C::BrewingStandInput, 0),
        ),
    ]
}

#[test]
fn closing_leaving_or_dying_mid_use_returns_every_input() {
    for (block, slot) in stations() {
        let (mut w, s) = world(&[Stack::new(item(Kind::Stick), 1)]);
        w.place_block(AT, block);
        w.open_block(s, AT);
        put(&mut w, s, 0, 1, slot);
        let before = totals(&w.state);
        // Leaving and dying close the window this way first (`State::remove`, the death rules).
        close_window(&mut w.state, s);
        w.ack(s);
        assert_eq!(totals(&w.state), before, "{block:?}");
        let in_inventory = w.inv(s).main.stack(0).count();
        let in_block: u64 = w
            .state
            .rules
            .block_entities
            .all_items()
            .map(|x| u64::from(x.count()))
            .sum();
        assert_eq!(u64::from(in_inventory) + in_block, 1, "{block:?}");
        assert!(dropped().is_empty());
    }
}

#[test]
fn opening_and_closing_every_station_a_hundred_times_leaves_nothing_behind() {
    let (mut w, s) = world(&[]);
    let positions: Vec<BlockPos> = (0..8).map(|i| BlockPos::new(-6 + i * 2, -60, 6)).collect();
    for ((block, _), pos) in stations().into_iter().zip(&positions) {
        w.place_block(*pos, block);
    }
    let start = w.state.rules.block_entities.sizes();
    for _ in 0..100 {
        for pos in &positions {
            w.open_block(s, *pos);
            w.close(s);
            w.ack(s);
        }
    }
    assert!(w.inv(s).window.is_none() && w.inv(s).pending_close.is_none());
    assert_eq!(w.state.rules.block_entities.sizes(), start);
    w.tick(Vec::new());
    assert_eq!(w.state.rules.block_entities.sizes().1, 0, "no viewers");
}

#[test]
fn breaking_a_container_like_block_drops_its_contents_once() {
    let stick = Stack::new(item(Kind::Stick), 1);
    let blocks = [
        blk::Kind::Furnace.default_state(),
        blk::Kind::BrewingStand.default_state(),
        blk::Kind::Campfire.default_state(),
        blk::Kind::ChiseledBookshelf.default_state(),
        blk::Kind::Chest.default_state(),
        blk::Kind::Barrel.default_state(),
        blk::Kind::Hopper.default_state(),
    ];
    for block in blocks {
        let (mut w, _s) = world(&[]);
        w.place_block(AT, block);
        let slots = {
            let e = w.state.rules.block_entities.get_mut(0, AT).unwrap();
            for s in &mut e.items {
                *s = stick.clone();
            }
            e.items.len()
        };
        assert!(slots > 0, "{block:?}");
        assert!(interact::destroy(&mut w.state, 0, AT, Destroy::Rules));
        let got = dropped();
        assert_eq!(got.len(), slots, "{block:?}");
        assert!(got.iter().all(|s| *s == stick));
        assert!(!interact::destroy(&mut w.state, 0, AT, Destroy::Rules));
        assert_eq!(dropped().len(), slots, "{block:?}: once");
        assert!(w.state.rules.block_entities.get(0, AT).is_none());
    }
}

#[test]
fn block_entities_survive_chunk_unload_and_reload_unchanged() {
    let (mut w, s) = world(&[]);
    w.place_block(AT, blk::Kind::Furnace.default_state());
    w.place_block(
        BlockPos::new(4, -60, 3),
        blk::Kind::Campfire.default_state(),
    );
    w.open_block(s, AT);
    close_window(&mut w.state, s);
    w.ack(s);
    if let Some(e) = w.state.rules.block_entities.get_mut(0, AT) {
        e.items[0] = Stack::new(item(Kind::Stick), 3);
    }
    let key = (0, AT.chunk());
    let before: BTreeMap<_, _> = [AT, BlockPos::new(4, -60, 3)]
        .into_iter()
        .map(|p| (p, w.state.rules.block_entities.get(0, p).cloned()))
        .collect();
    let list = w.state.rules.block_entities.unload(key).unwrap();
    assert!(w.state.rules.block_entities.get(0, AT).is_none());
    w.state
        .rules
        .block_entities
        .load_list(key, &list, &w.state.worlds[0], &w.state.custom);
    for (p, e) in before {
        assert_eq!(w.state.rules.block_entities.get(0, p).cloned(), e);
    }
    assert_eq!(w.state.rules.block_entities.unload(key), None, "unchanged");
}

#[test]
fn beds_banners_and_heads_keep_their_item_data() {
    let (mut w, s) = world(&[]);
    let place_with =
        |w: &mut World, pos: BlockPos, block: blk::State, stack: &Stack, face: Face| {
            w.state.worlds[0].set_block(pos, block).unwrap();
            block_entity::placed(&mut w.state, 0, pos, Some(s), stack, face);
        };
    let blue_bed = Stack::new(named_meta("minecraft:bed", 11), 1);
    place_with(
        &mut w,
        AT,
        blk::Kind::Bed.default_state(),
        &blue_bed,
        Face::Up,
    );
    assert_eq!(block_entity::own_item(&w.state, 0, AT), Some(blue_bed));
    let mut tag = Compound::new();
    tag.insert(
        "Patterns".to_owned(),
        crate::nbt::Value::List(crate::nbt::List::Compound(vec![Compound::from([
            (
                "Pattern".to_owned(),
                crate::nbt::Value::String("bo".to_owned()),
            ),
            ("Color".to_owned(), crate::nbt::Value::Int(3)),
        ])])),
    );
    let banner_type = named_meta("minecraft:banner", 4);
    let banner = Stack::from_parts(banner_type, 1, 4, Some(tag)).unwrap();
    let at2 = BlockPos::new(5, -60, 3);
    let standing = blk::State::all()
        .find(|b| b.name() == "minecraft:standing_banner")
        .unwrap();
    place_with(&mut w, at2, standing, &banner, Face::Up);
    assert_eq!(block_entity::own_item(&w.state, 0, at2), Some(banner));
}

/// [`world`] on a server with the recipes of `builder`.
fn custom_world(
    builder: crate::ServerBuilder,
    stacks: &[Stack],
) -> (World, super::super::super::SessionId) {
    let (mut w, s) = world(stacks);
    let mut custom = builder.setup.custom;
    custom.finish().unwrap();
    w.state.custom = std::sync::Arc::new(custom);
    (w, s)
}

fn own(kind: Kind) -> crate::custom::ItemRef {
    crate::custom::ItemRef::Item(item(kind))
}

/// A server whose stonecutter cuts a diamond into four sticks and whose smithing table turns a
/// golden sword with an emerald and a netherite upgrade template into a diamond sword.
fn station_recipes() -> crate::ServerBuilder {
    use crate::custom::{Ingredient as I, Recipe as R};
    crate::Server::builder()
        .recipe(R::stonecutter(
            I::Item(own(Kind::Diamond)),
            (own(Kind::Stick), 4),
        ))
        .recipe(R::smithing_transform(
            I::Item(crate::custom::ItemRef::Item(named(
                "minecraft:netherite_upgrade_smithing_template",
            ))),
            // Any damage: an exact ingredient takes an undamaged item only (PMMP).
            I::AnyMeta(own(Kind::GoldenSword)),
            I::AnyMeta(own(Kind::Emerald)),
            own(Kind::DiamondSword),
        ))
}

/// The stonecutter request cutting `times` diamonds with server recipe `rid`.
fn cut(w: &mut World, s: super::super::super::SessionId, rid: u32, times: u8, id: i32) -> bool {
    let net = w.inv(s).get(SlotRef::Ui(0)).unwrap().id.net;
    let actions = vec![
        craft(rid, times),
        consume(times, at(C::StonecutterInput, 3, net)),
        place(4 * times, created(id), at(COMBINED, 1, 0)),
    ];
    ok(&w.request_as(s, id, actions))
}

#[test]
fn a_servers_stonecutter_recipe_cuts_once_per_request_and_keeps_the_count() {
    let run = |twice: bool| {
        let (mut w, s) = custom_world(station_recipes(), &[Stack::new(item(Kind::Diamond), 3)]);
        let rid = crate::crafting::first_special_id();
        w.place_block(AT, blk::Kind::Stonecutter.default_state());
        w.open_block(s, AT);
        put(&mut w, s, 0, 3, (C::StonecutterInput, 3));
        let before = totals(&w.state);
        assert!(cut(&mut w, s, rid, 2, -101));
        if twice {
            assert!(!cut(&mut w, s, rid, 2, -101), "the same request again");
        }
        assert_eq!(w.inv(s).main.stack(1), &Stack::new(item(Kind::Stick), 8));
        assert_eq!(ui_stack(&w, s, 0), Stack::new(item(Kind::Diamond), 1));
        // Two diamonds in, eight sticks out, nothing else made or lost.
        let after = totals(&w.state);
        let count = |t: &BTreeMap<(Option<Type>, u16, String), u64>, kind| {
            t.iter()
                .filter(|((i, _, _), _)| *i == Some(item(kind)))
                .map(|(_, n)| *n)
                .sum::<u64>()
        };
        assert_eq!(
            count(&before, Kind::Diamond) - count(&after, Kind::Diamond),
            2
        );
        assert_eq!(count(&after, Kind::Stick) - count(&before, Kind::Stick), 8);
        assert_eq!(total_count(&w.state), 3 - 2 + 8);
        // Another recipe's ID is refused at the stonecutter.
        assert!(!cut(&mut w, s, rid + 1, 1, -103));
        check_slots(&w.state);
        w.state.tick = 1000;
        w.state.hash()
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn a_servers_smithing_recipe_transforms_keeping_the_inputs_data_once() {
    let sword = Stack::new(item(Kind::GoldenSword), 1)
        .with_damage(7)
        .unwrap()
        .with_extra(Some(Extra {
            enchantments: vec![(crate::item::Enchantment::Sharpness, 1)],
            ..Extra::default()
        }))
        .unwrap();
    let run = |twice: bool| {
        let (mut w, s) = custom_world(
            station_recipes(),
            &[
                sword.clone(),
                Stack::new(item(Kind::Emerald), 2),
                Stack::new(named("minecraft:netherite_upgrade_smithing_template"), 1),
            ],
        );
        let rid = crate::crafting::first_special_id() + 1;
        w.place_block(AT, blk::Kind::SmithingTable.default_state());
        w.open_block(s, AT);
        put(&mut w, s, 0, 1, (C::SmithingTableInput, 51));
        put(&mut w, s, 1, 2, (C::SmithingTableMaterial, 52));
        put(&mut w, s, 2, 1, (C::SmithingTableTemplate, 53));
        assert!(
            !ok(&smith(&mut w, s, rid - 1)),
            "the stonecutter recipe's ID"
        );
        let ids: Vec<i32> = (0..3)
            .map(|i| w.inv(s).get(SlotRef::Ui(i)).unwrap().id.net)
            .collect();
        let actions = vec![
            craft(rid, 1),
            consume(1, at(C::SmithingTableInput, 51, ids[0])),
            consume(1, at(C::SmithingTableMaterial, 52, ids[1])),
            consume(1, at(C::SmithingTableTemplate, 53, ids[2])),
            place(1, created(-101), at(COMBINED, 8, 0)),
        ];
        assert!(ok(&w.request_as(s, -101, actions.clone())));
        if twice {
            assert!(!ok(&w.request_as(s, -101, actions)));
        }
        let got = w.inv(s).main.stack(8).clone();
        assert_eq!(got.item(), Some(item(Kind::DiamondSword)));
        assert_eq!(got.damage(), 7);
        assert_eq!(got.extra(), sword.extra());
        assert_eq!(ui_stack(&w, s, 1), Stack::new(item(Kind::Emerald), 1));
        assert!(ui_stack(&w, s, 0).is_empty() && ui_stack(&w, s, 2).is_empty());
        assert_eq!(total_count(&w.state), 1 + 1);
        check_slots(&w.state);
        w.state.tick = 1000;
        w.state.hash()
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn a_brewing_stand_brews_a_servers_recipes_after_pmmps_container_changes() {
    use crate::custom::{Ingredient as I, Recipe as R};
    let water = item(Kind::Potion);
    let splash = named("minecraft:splash_potion");
    let builder = crate::Server::builder()
        .recipe(R::brewing(
            I::Item(crate::custom::ItemRef::Item(water)),
            I::Item(own(Kind::Emerald)),
            crate::custom::ItemRef::Item(named_meta("minecraft:potion", 7)),
        ))
        .recipe(R::brewing_container(
            crate::custom::ItemRef::Item(splash),
            I::Item(own(Kind::Emerald)),
            crate::custom::ItemRef::Item(named("minecraft:lingering_potion")),
        ));
    let (mut w, s) = custom_world(
        builder,
        &[
            Stack::new(item(Kind::Emerald), 1),
            Stack::new(water, 1),
            Stack::new(splash, 1),
            Stack::new(item(Kind::BlazePowder), 1),
        ],
    );
    w.place_block(AT, blk::Kind::BrewingStand.default_state());
    w.open_block(s, AT);
    put(&mut w, s, 0, 1, (C::BrewingStandInput, 0));
    put(&mut w, s, 1, 1, (C::BrewingStandResult, 1));
    put(&mut w, s, 2, 1, (C::BrewingStandResult, 2));
    put(&mut w, s, 3, 1, (C::BrewingStandFuel, 4));
    for _ in 0..401 {
        w.tick(Vec::new());
    }
    assert_eq!(
        w.slot(s, C::BrewingStandResult, 1).stack,
        Stack::new(named_meta("minecraft:potion", 7), 1)
    );
    assert_eq!(
        w.slot(s, C::BrewingStandResult, 2).stack,
        Stack::new(named("minecraft:lingering_potion"), 1),
        "the splash water bottle keeps its meta"
    );
    assert!(
        w.slot(s, C::BrewingStandInput, 0).stack.is_empty(),
        "one emerald for both"
    );
    check_slots(&w.state);
}
