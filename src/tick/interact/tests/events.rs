//! Tick tests of the interaction events and actions: what listeners see, what cancelling and
//! changing fields do, what monitors see, and that listeners changing the game neither make nor
//! lose items.

use super::*;
use crate::event::{
    BlockBreak, BlockExplode, BlockPlace, BlockPreExplode, BlockTeleport, InteractAction,
    InventoryClose, Listeners, PlayerBucketEmpty, PlayerBucketFill, PlayerEntityInteract,
    PlayerInteract, PlayerItemUse, PlayerRespawnAnchorUse, Priority, RespawnAnchorAction,
    SignChange,
};
use crate::item::Kind as ItemKind;
use crate::nbt::{Compound, Value};
use crate::protocol::consts::{InventoryActionSource, WINDOW_ID_ARMOUR, WINDOW_ID_INVENTORY};
use crate::protocol::{InventoryAction, UseItemOnEntityTransactionData};
use crate::tick::Context;

/// The player of the harness.
fn me() -> crate::player::Id {
    crate::player::Id::new(SessionId::new(1))
}

/// Listeners with one listener for `E`.
fn listen<E: crate::event::Event>(
    f: impl FnMut(&mut E, &mut Context<'_>) + Send + 'static,
) -> Listeners {
    let mut l = Listeners::default();
    l.push(Priority::Normal, f);
    l
}

thread_local! {
    /// How many times a [`cancel`] listener ran on this test's thread (the tick runs there).
    static CANCELS: Cell<u32> = const { Cell::new(0) };
}

/// A listener that cancels `E`, counted in [`cancels`].
fn cancel<E: crate::event::Event>(cancel: fn(&mut E)) -> Listeners {
    listen(move |e: &mut E, _: &mut Context<'_>| {
        CANCELS.with(|c| c.set(c.get() + 1));
        cancel(e);
    })
}

/// How many times a [`cancel`] listener ran since the last call.
fn cancels() -> u32 {
    CANCELS.with(|c| c.replace(0))
}

/// The player's hand, changed from a listener.
fn set_hand(cx: &mut Context<'_>, stack: Stack) {
    let player = cx.state.players.get_mut(&SessionId::new(1)).unwrap();
    inventory::set_held(player, stack);
}

/// Takes every entity out of the chunks around the spawn: the count of `item` on the ground and
/// the experience of the orbs.
fn ground(h: &mut Harness, item: crate::item::Type) -> (u32, u32) {
    let (mut items, mut xp) = (0, 0);
    for x in -2..=2 {
        for z in -2..=2 {
            let key = (0, crate::world::ChunkPos::new(x, z));
            for c in h.state.rules.entities.unload(key).unwrap_or_default() {
                if let Some(Value::Compound(s)) = c.get("Item")
                    && let Some(stack) = Stack::from_nbt(s)
                    && stack.item() == Some(item)
                {
                    items += u32::from(stack.count());
                }
                if let Some(Value::Int(v)) = c.get("experience value") {
                    xp += v.cast_unsigned();
                }
            }
        }
    }
    (items, xp)
}

fn dandelion() -> block::State {
    Kind::Dandelion.default_state()
}

/// A right click on `face` of `pos` with the held item.
fn use_on(h: &mut Harness, pos: BlockPos, face: Face) {
    let held = h.held();
    let input = h.auth(Vec::new(), Some(click(pos, face, &held)));
    h.run(vec![input]);
}

/// The same right click, but claiming an item the player does not hold, so the rules refuse it
/// after the repeat filter noted it: the twin of a cancelled click.
fn refused_use_on(h: &mut Harness, pos: BlockPos, face: Face) {
    let wrong = click(pos, face, &block_item(Kind::Stone, 7));
    let input = h.auth(Vec::new(), Some(wrong));
    h.run(vec![input]);
}

/// Only the held press tells a cancelled right click from its [`refused_use_on`] twin: the
/// cancelled one raised its event, so its press has one. Checks that and forgets it.
fn forget_press(h: &mut Harness, refused: bool) {
    let press = &mut h.player_mut().breaking.press;
    assert_eq!(press.raised, !refused);
    press.raised = false;
}

/// A click in the air with the held item.
fn use_in_air(h: &mut Harness) {
    let held = h.held();
    let input = h.auth(Vec::new(), Some(air(&held)));
    h.run(vec![input]);
}

const GROUND: BlockPos = BlockPos::new(2, -61, 0);

/// A client's predicted change of `slot` in its own window `window` from `old` to `new`, as a
/// use item transaction carries it.
fn predicted(window: u32, slot: u8, old: &Stack, new: &Stack) -> InventoryAction {
    InventoryAction {
        source_type: InventoryActionSource::Container.into(),
        window_id: Some(i8::try_from(window).unwrap()),
        source_flags: None,
        inventory_slot: u32::from(slot),
        old_item: old.to_instance(0),
        new_item: new.to_instance(0),
    }
}

/// The counts `packets` give main slot `slot`, from whole inventory contents and single slots.
fn main_counts(packets: &[Packet], slot: u8) -> Vec<u16> {
    packets
        .iter()
        .filter_map(|p| match p {
            Packet::InventoryContent(c) if c.window_id == WINDOW_ID_INVENTORY => {
                c.content.get(usize::from(slot)).map(|i| i.stack.count)
            }
            Packet::InventorySlot(s)
                if s.window_id == WINDOW_ID_INVENTORY && s.slot == u32::from(slot) =>
            {
                Some(s.new_item.stack.count)
            }
            _ => None,
        })
        .collect()
}

// pmmp: BlockBreakEvent
#[test]
fn a_block_break_listener_sees_the_break_and_sets_its_drops_and_experience() {
    let mut l = listen(move |e: &mut BlockBreak, _: &mut Context<'_>| {
        note(
            SEEN,
            (
                e.player(),
                e.world(),
                e.pos(),
                e.block(),
                e.item().clone(),
                e.insta_break(),
                e.drops().to_vec(),
                e.xp_drop_amount(),
            ),
        );
        e.set_drops(vec![stack(ItemKind::Stick).with_count(3)]);
        e.set_xp_drop_amount(7);
    });
    l.push_monitor(move |e: &BlockBreak, cx: &Context<'_>| {
        note(WATCHED, cx.state.worlds[0].block(e.pos()).unwrap());
    });
    let mut h = Harness::with(|_| {}, l);
    h.set(TARGET, dandelion());
    let tool = stack(ItemKind::Stick);
    h.hold(tool.clone());
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(h.block(TARGET), block::AIR);
    let flower = block_item(Kind::Dandelion, 1);
    assert_eq!(
        notes(SEEN),
        [dbg((
            me(),
            crate::world::Id::DEFAULT,
            TARGET,
            dandelion(),
            tool,
            false,
            vec![flower.clone()],
            0
        ))]
    );
    assert_eq!(
        notes(WATCHED),
        [dbg(block::AIR)],
        "the monitor sees the block gone"
    );
    assert_eq!(ground(&mut h, flower.item().unwrap()), (0, 7));
    let mut h2 = Harness::new(|_| {});
    h2.set(TARGET, dandelion());
    h2.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(
        ground(&mut h2, flower.item().unwrap()),
        (1, 0),
        "without listeners"
    );
}

// pmmp: BlockBreakEvent
#[test]
fn a_cancelled_break_changes_nothing_and_resyncs_the_block() {
    let run = |listeners: Listeners, kind: PlayerAction| {
        let mut h = Harness::with(|_| {}, listeners);
        h.set(TARGET, dandelion());
        h.run(Vec::new());
        h.packets();
        h.act(vec![action(kind, TARGET, Face::West)]);
        (h.state.hash(), updates(&h.packets()), h.block(TARGET))
    };
    let (cancelled, sent, block) = run(
        cancel(BlockBreak::cancel),
        PlayerAction::PredictDestroyBlock,
    );
    assert_eq!(cancels(), 1);
    let (twin, _, _) = run(Listeners::default(), PlayerAction::AbortBreak);
    assert_eq!(block, dandelion());
    assert!(sent.contains(&TARGET), "{sent:?}");
    assert_eq!(
        cancelled, twin,
        "as if the client had only stopped breaking"
    );
}

/// A break listener sees an inert block (one only clients know) as `minecraft:info_update` and
/// can keep it, which the client gets back as itself; without a cancel, the listener's drops
/// fall.
#[test]
fn a_block_break_listener_sees_an_inert_block_as_info_update_and_can_keep_it() {
    let run = |cancelled: bool| {
        let listener = listen(move |e: &mut BlockBreak, _: &mut Context<'_>| {
            note(SEEN, e.block());
            if cancelled {
                e.cancel();
            } else {
                e.set_drops(vec![Stack::new(ItemKind::Apple.default_type(), 1)]);
            }
        });
        let mut h = Harness::with(creative, listener);
        let inert = h.state.worlds[0]
            .put_unknown(TARGET, super::stored("minecraft:moss_block"))
            .unwrap();
        h.act(vec![action(
            PlayerAction::PredictDestroyBlock,
            TARGET,
            Face::West,
        )]);
        assert_eq!(notes(SEEN), [dbg(block::INFO_UPDATE)]);
        (
            h.state.worlds[0].entry(TARGET),
            inert,
            h.packets(),
            ground(&mut h, ItemKind::Apple.default_type()),
        )
    };
    let (now, inert, sent, (apples, _)) = run(true);
    assert_eq!(now, Some(inert));
    assert!(
        super::sets(&sent, TARGET, super::MOSS_BLOCK),
        "resent as itself"
    );
    assert_eq!(apples, 0);
    let (now, _, _, (apples, _)) = run(false);
    assert_eq!(now, Some(Entry::AIR));
    assert_eq!(apples, 1);
}

// pmmp: BlockBreakEvent
#[test]
fn a_listener_that_changes_the_block_or_takes_the_tool_cancels_the_break() {
    let replace = listen(|e: &mut BlockBreak, cx: &mut Context<'_>| {
        cx.state.worlds[0]
            .set_block(e.pos(), Kind::Poppy.default_state())
            .unwrap();
    });
    let take = listen(|_: &mut BlockBreak, cx: &mut Context<'_>| {
        set_hand(cx, Stack::EMPTY);
    });
    for (listeners, left) in [(replace, Kind::Poppy.default_state()), (take, dandelion())] {
        let mut h = Harness::with(|_| {}, listeners);
        h.set(TARGET, dandelion());
        h.hold(stack(ItemKind::Stick));
        h.act(vec![action(
            PlayerAction::PredictDestroyBlock,
            TARGET,
            Face::West,
        )]);
        assert_eq!(h.block(TARGET), left);
        let flower = block_item(Kind::Dandelion, 1).item().unwrap();
        assert_eq!(ground(&mut h, flower), (0, 0), "nothing dropped");
    }
}

// pmmp: BlockBreakEvent::getInstaBreak
#[test]
fn a_break_before_its_time_raises_no_block_break() {
    let l = listen(|e: &mut BlockBreak, _: &mut Context<'_>| note(SEEN, e.pos()));
    let mut h = Harness::with(|_| {}, l);
    // A full box: a break plan would copy its contents into the dropped box.
    set_with_entity(&mut h, Kind::ShulkerBox.default_state(), &Stack::EMPTY);
    h.state
        .rules
        .block_entities
        .get_mut(0, TARGET)
        .unwrap()
        .items[0] = Stack::new(ItemKind::Diamond.default_type(), 64);
    let full_box = h.block(TARGET);
    let early = action(PlayerAction::PredictDestroyBlock, TARGET, Face::West);
    h.act(vec![early; MAX_BLOCK_ACTIONS]);
    assert!(notes(SEEN).is_empty());
    assert_eq!(h.block(TARGET), full_box);
}

// pmmp: BlockPlaceEvent
#[test]
fn a_block_place_listener_sees_every_block_and_the_monitor_the_placed_door() {
    let mut l = listen(move |e: &mut BlockPlace, _: &mut Context<'_>| {
        let at: Vec<BlockPos> = e.blocks().iter().map(|(p, _)| *p).collect();
        note(
            SEEN,
            (
                e.player(),
                e.world(),
                at,
                e.block_against(),
                e.item().clone(),
            ),
        );
    });
    l.push_monitor(move |e: &BlockPlace, cx: &Context<'_>| {
        let world = &cx.state.worlds[0];
        let placed = e.blocks().iter().all(|(p, s)| world.block(*p) == Some(*s));
        let count = cx.state.players[&SessionId::new(1)]
            .inventory
            .held()
            .count();
        note(WATCHED, (placed, count));
    });
    let mut h = Harness::with(|_| {}, l);
    let door = block_item(Kind::OakDoor, 2);
    h.hold(door.clone());
    use_on(&mut h, GROUND, Face::Up);
    let up = BlockPos::new(2, -59, 0);
    assert_eq!(
        notes(SEEN),
        [dbg((
            me(),
            crate::world::Id::DEFAULT,
            vec![TARGET, up],
            GROUND,
            door
        ))]
    );
    assert_eq!(h.block(TARGET).kind(), Kind::OakDoor);
    assert_eq!(h.block(up).kind(), Kind::OakDoor);
    assert_eq!(notes(WATCHED), [dbg((true, 1))]);
}

// pmmp: BlockPlaceEvent
#[test]
fn a_cancelled_placement_changes_nothing_and_resyncs() {
    let run = |listeners: Listeners, refused: bool| {
        let mut h = Harness::with(|_| {}, listeners);
        h.hold(block_item(Kind::OakPlanks, 5));
        h.packets();
        if refused {
            refused_use_on(&mut h, GROUND, Face::Up);
        } else {
            use_on(&mut h, GROUND, Face::Up);
        }
        forget_press(&mut h, refused);
        (
            h.state.hash(),
            updates(&h.packets()),
            h.block(TARGET),
            h.held(),
        )
    };
    let (cancelled, sent, block, held) = run(cancel(BlockPlace::cancel), false);
    assert_eq!(cancels(), 1);
    let (twin, _, _, _) = run(Listeners::default(), true);
    assert_eq!((block, held), (block::AIR, block_item(Kind::OakPlanks, 5)));
    assert!(sent.contains(&TARGET), "{sent:?}");
    assert_eq!(cancelled, twin);
}

/// The client takes the item away as it sends a placement; a cancelled one sends the inventory
/// back, so the client's next click names the server's count and places, and a right prediction
/// is not sent again.
// pmmp: BlockPlaceEvent
#[test]
fn a_cancelled_placement_sends_back_the_predicted_slot_so_the_next_click_places() {
    let mut first = true;
    let once = listen(move |e: &mut BlockPlace, _: &mut Context<'_>| {
        if std::mem::take(&mut first) {
            e.cancel();
        }
    });
    let mut h = Harness::with(|_| {}, once);
    h.hold(block_item(Kind::OakPlanks, 5));
    let slot = h.player().inventory.held_slot();
    h.packets();
    let place = |h: &mut Harness| {
        let held = h.held();
        let mut data = click(GROUND, Face::Up, &held);
        let fewer = held.with_count(held.count() - 1);
        data.actions = vec![predicted(WINDOW_ID_INVENTORY, slot, &held, &fewer)];
        let input = h.auth(Vec::new(), Some(data));
        h.run(vec![input]);
    };
    place(&mut h);
    assert_eq!((h.block(TARGET), h.held().count()), (block::AIR, 5));
    assert_eq!(main_counts(&h.packets(), slot), [5]);
    for _ in 0..REPEAT_TICKS {
        h.run(Vec::new());
    }
    place(&mut h);
    assert_eq!(h.block(TARGET).kind(), Kind::OakPlanks);
    assert_eq!(main_counts(&h.packets(), slot), [4]);
}

/// Wearing armour by a click in the air: the client predicts the hand empty and the armour worn.
/// A cancelled use sends the inventory back when the click, from either packet, carried that
/// prediction, and nothing when it carried none.
// pmmp: PlayerItemUseEvent
#[test]
fn a_refused_use_in_the_air_resends_the_inventory_only_after_a_prediction() {
    let helmet = stack(ItemKind::IronHelmet);
    let run = |predicting: bool, transaction: bool| {
        let mut h = Harness::with(|_| {}, cancel(PlayerItemUse::cancel));
        h.hold(helmet.clone());
        let slot = h.player().inventory.held_slot();
        h.packets();
        let actions = if predicting {
            vec![
                predicted(WINDOW_ID_INVENTORY, slot, &helmet, &Stack::EMPTY),
                predicted(WINDOW_ID_ARMOUR, 0, &Stack::EMPTY, &helmet),
            ]
        } else {
            Vec::new()
        };
        let mut data = air(&helmet);
        let input = if transaction {
            h.input(Packet::InventoryTransaction(InventoryTransaction {
                actions,
                transaction_data: InventoryTransactionData::UseItem(Box::new(data)),
                ..Default::default()
            }))
        } else {
            data.actions = actions;
            h.auth(Vec::new(), Some(data))
        };
        h.run(vec![input]);
        assert_eq!(cancels(), 1);
        assert_eq!(h.held(), helmet);
        main_counts(&h.packets(), slot)
    };
    assert_eq!(run(false, false), [0_u16; 0]);
    assert_eq!(run(true, false), [1]);
    assert_eq!(run(true, true), [1]);
}

// pmmp: BlockPlaceEvent
#[test]
fn a_listener_that_fills_the_place_or_takes_the_item_cancels_the_placement() {
    let fill = listen(|e: &mut BlockPlace, cx: &mut Context<'_>| {
        cx.state.worlds[0]
            .set_block(e.blocks()[0].0, Kind::Stone.default_state())
            .unwrap();
    });
    let take = listen(|_: &mut BlockPlace, cx: &mut Context<'_>| {
        set_hand(cx, stack(ItemKind::Stick));
    });
    for (listeners, block, held) in [
        (
            fill,
            Kind::Stone.default_state(),
            block_item(Kind::OakPlanks, 5),
        ),
        (take, block::AIR, stack(ItemKind::Stick)),
    ] {
        let mut h = Harness::with(|_| {}, listeners);
        h.hold(block_item(Kind::OakPlanks, 5));
        use_on(&mut h, GROUND, Face::Up);
        assert_eq!((h.block(TARGET), h.held()), (block, held));
    }
}

/// A closed oak door at [`TARGET`], both halves.
fn door_at_target(h: &mut Harness) {
    let door = Kind::OakDoor.default_state();
    h.set(TARGET, door);
    h.set(
        BlockPos::new(2, -59, 0),
        props::with_flag(door, "upper_block_bit", true).unwrap(),
    );
}

fn door_open(h: &Harness) -> bool {
    props::flag(h.block(TARGET), "open_bit") == Some(true)
}

// pmmp: PlayerInteractEvent
#[test]
fn a_right_click_listener_sees_the_click_and_may_keep_the_block_from_reacting() {
    let l = listen(move |e: &mut PlayerInteract, _: &mut Context<'_>| {
        note(
            SEEN,
            (
                e.player(),
                e.pos(),
                e.block(),
                e.face(),
                e.item().clone(),
                e.touch_vector(),
                e.action(),
                e.uses_item(),
                e.uses_block(),
            ),
        );
        e.set_use_block(false);
    });
    let mut h = Harness::with(|_| {}, l);
    door_at_target(&mut h);
    let planks = block_item(Kind::OakPlanks, 5);
    h.hold(planks.clone());
    use_on(&mut h, TARGET, Face::West);
    assert_eq!(
        notes(SEEN),
        [dbg((
            me(),
            TARGET,
            Kind::OakDoor.default_state(),
            Face::West,
            planks,
            Vec3::new(0.5, 1.0, 0.5),
            InteractAction::RightClickBlock,
            true,
            true
        ))]
    );
    // The door stays shut; the planks go on its west face instead.
    assert!(!door_open(&h));
    assert_eq!(h.block(BlockPos::new(1, -60, 0)).kind(), Kind::OakPlanks);
    let mut h = Harness::new(|_| {});
    door_at_target(&mut h);
    h.hold(block_item(Kind::OakPlanks, 5));
    use_on(&mut h, TARGET, Face::West);
    assert!(door_open(&h), "without listeners the door opens");
}

// pmmp: PlayerInteractEvent
#[test]
fn a_cancelled_right_click_uses_neither_the_block_nor_the_item() {
    let run = |listeners: Listeners, refused: bool| {
        let mut h = Harness::with(|_| {}, listeners);
        door_at_target(&mut h);
        h.hold(block_item(Kind::OakPlanks, 5));
        if refused {
            refused_use_on(&mut h, TARGET, Face::West);
        } else {
            use_on(&mut h, TARGET, Face::West);
        }
        forget_press(&mut h, refused);
        assert!(!door_open(&h));
        assert_eq!(h.block(BlockPos::new(1, -60, 0)), block::AIR);
        assert_eq!(h.held().count(), 5);
        h.state.hash()
    };
    let cancelled = run(cancel(PlayerInteract::cancel), false);
    assert_eq!(cancels(), 1);
    assert_eq!(cancelled, run(Listeners::default(), true));
}

/// A right click on the top of the ground block at `x`, claiming to hold `held`, by `trigger`.
fn ground_click(h: &mut Harness, x: i32, trigger: TriggerType, held: &Stack) {
    let data = UseItemTransactionData {
        trigger_type: trigger.into(),
        ..click(BlockPos::new(x, -61, 0), Face::Up, held)
    };
    let input = h.auth(Vec::new(), Some(data));
    h.run(vec![input]);
}

#[test]
fn only_the_first_right_click_of_a_held_press_is_first() {
    let l = listen(|e: &mut PlayerInteract, _: &mut Context<'_>| note(SEEN, e.is_first()));
    let mut h = Harness::with(|_| {}, l);
    let hand = h.held();
    let (input, held) = (TriggerType::PlayerInput, TriggerType::SimulationTick);
    // Pressed on a block and held over others.
    ground_click(&mut h, 1, input, &hand);
    ground_click(&mut h, 2, held, &hand);
    ground_click(&mut h, 3, held, &hand);
    assert_eq!(notes(SEEN), [dbg(true), dbg(false), dbg(false)]);
    // Let go; pressed in the air and held onto blocks: no `PlayerInput` click comes.
    for _ in 0..PRESS_GAP_TICKS {
        h.run(Vec::new());
    }
    ground_click(&mut h, 1, held, &hand);
    ground_click(&mut h, 2, held, &hand);
    // Pressed again at once, or with an unknown trigger: a new press each.
    ground_click(&mut h, 3, input, &hand);
    ground_click(&mut h, 1, TriggerType::Unknown, &hand);
    // An action is a click of its own and leaves the held press as it was.
    perform(
        &mut h,
        action::UseItemOn::new(me(), GROUND, Face::Up, Vec3::ZERO),
    );
    ground_click(&mut h, 3, held, &hand);
    // A refused first click raises nothing, so the next of its press is first.
    ground_click(&mut h, 1, input, &block_item(Kind::Stone, 7));
    ground_click(&mut h, 2, held, &hand);
    ground_click(&mut h, 3, held, &hand);
    assert_eq!(
        notes(SEEN),
        [true, false, true, true, true, false, true, false].map(dbg)
    );
}

// pmmp: PlayerInteractEvent
#[test]
fn a_cancelled_left_click_starts_no_breaking_and_keeps_the_dragon_egg() {
    let l = listen(move |e: &mut PlayerInteract, _: &mut Context<'_>| {
        note(SEEN, (e.action(), e.block(), e.touch_vector()));
        e.cancel();
    });
    let mut h = Harness::with(|_| {}, l);
    let glass = Kind::Glass.default_state();
    h.set(TARGET, glass);
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    assert!(h.player().breaking.target.is_none());
    assert_eq!(
        notes(SEEN),
        [dbg((InteractAction::LeftClickBlock, glass, Vec3::ZERO))]
    );
    let egg = Kind::DragonEgg.default_state();
    let at = BlockPos::new(3, -60, 0);
    h.set(at, egg);
    h.act(vec![action(PlayerAction::StartBreak, at, Face::West)]);
    assert_eq!(h.block(at), egg);
    // The twin: the client only says which face it cracks.
    let run = |listeners: Listeners, kind: PlayerAction| {
        let mut h = Harness::with(|_| {}, listeners);
        h.set(TARGET, glass);
        h.act(vec![action(kind, TARGET, Face::West)]);
        h.state.hash()
    };
    let cancelled = run(cancel(PlayerInteract::cancel), PlayerAction::StartBreak);
    assert_eq!(cancels(), 1);
    assert_eq!(
        cancelled,
        run(Listeners::default(), PlayerAction::CrackBreak)
    );
}

/// Starting to eat asks the item use listeners, and a cancel keeps the player from eating
/// (PMMP `Player::useHeldItem`); finishing does not ask again.
#[test]
fn starting_to_eat_asks_the_item_use_listeners() {
    let apple = stack(ItemKind::GoldenApple);
    let mut h = Harness::with(|_| {}, cancel(PlayerItemUse::cancel));
    h.hold(apple.clone());
    use_in_air(&mut h);
    assert_eq!(cancels(), 1);
    assert!(!h.player().is_using_item());
    let mut h = Harness::with(
        |_| {},
        listen(|_: &mut PlayerItemUse, _: &mut Context<'_>| {
            note(SEEN, ());
        }),
    );
    h.hold(apple);
    use_in_air(&mut h);
    assert!(h.player().is_using_item());
    for _ in 0..40 {
        h.run(Vec::new());
    }
    use_in_air(&mut h);
    assert!(h.held().is_empty(), "eaten");
    assert_eq!(notes(SEEN).len(), 1, "asked at the start only");
}

// pmmp: PlayerItemUseEvent
#[test]
fn an_item_use_listener_sees_the_item_and_look_and_cancelling_keeps_the_armour_in_hand() {
    let helmet = stack(ItemKind::IronHelmet);
    let mut l = listen(move |e: &mut PlayerItemUse, _: &mut Context<'_>| {
        let look = e.direction_vector();
        assert!((look.x - 1.0).abs() < 1e-6 && look.y.abs() < 1e-6 && look.z.abs() < 1e-6);
        note(SEEN, (e.player(), e.item().clone()));
    });
    l.push_monitor(move |_: &PlayerItemUse, cx: &Context<'_>| {
        let worn = cx.state.players[&SessionId::new(1)].inventory.armor()[0].clone();
        note(WATCHED, worn);
    });
    let mut h = Harness::with(|_| {}, l);
    h.hold(helmet.clone());
    use_in_air(&mut h);
    assert_eq!(notes(SEEN), [dbg((me(), &helmet))]);
    assert_eq!(notes(WATCHED), [dbg(&helmet)]);
    assert_eq!(h.player().inventory.armor()[0], helmet);
    let run = |listeners: Listeners, refused: bool| {
        let mut h = Harness::with(|_| {}, listeners);
        h.hold(helmet.clone());
        if refused {
            let input = h.auth(Vec::new(), Some(air(&block_item(Kind::Stone, 3))));
            h.run(vec![input]);
        } else {
            use_in_air(&mut h);
        }
        assert_eq!(h.held(), helmet);
        assert!(h.player().inventory.armor()[0].is_empty());
        h.state.hash()
    };
    let cancelled = run(cancel(PlayerItemUse::cancel), false);
    assert_eq!(cancels(), 1);
    assert_eq!(cancelled, run(Listeners::default(), true));
    // A listener that takes the helmet cancels the use.
    let take = listen(|_: &mut PlayerItemUse, cx: &mut Context<'_>| {
        set_hand(cx, stack(ItemKind::Stick));
    });
    let mut h = Harness::with(|_| {}, take);
    h.hold(helmet);
    use_in_air(&mut h);
    assert!(h.player().inventory.armor()[0].is_empty());
    assert_eq!(h.held(), stack(ItemKind::Stick));
}

fn water_source() -> block::State {
    block::State::find(
        "minecraft:water",
        &[("liquid_depth", block::PropertyValue::Int(0))],
    )
    .unwrap()
}

// pmmp: PlayerBucketFillEvent, PlayerBucketEvent::setItem, PlayerBucketEmptyEvent
#[test]
fn bucket_listeners_see_the_bucket_and_may_cancel_or_change_what_the_player_gets() {
    let lava = stack(ItemKind::LavaBucket);
    let l = listen(move |e: &mut PlayerBucketFill, _: &mut Context<'_>| {
        note(
            SEEN,
            (
                e.player(),
                e.block_clicked(),
                e.block_face(),
                e.bucket().clone(),
                e.item().clone(),
            ),
        );
        e.set_item(stack(ItemKind::LavaBucket));
    });
    let mut h = Harness::with(|_| {}, l);
    h.set(TARGET, water_source());
    let bucket = stack(ItemKind::Bucket);
    h.hold(bucket.clone());
    use_on(&mut h, TARGET, Face::West);
    assert_eq!(
        notes(SEEN),
        [dbg((
            me(),
            TARGET,
            Face::West,
            bucket.clone(),
            stack(ItemKind::WaterBucket)
        ))]
    );
    assert_eq!((h.block(TARGET), h.held()), (block::AIR, lava));
    // Cancelled: the water and the bucket stay.
    let mut h = Harness::with(|_| {}, cancel(PlayerBucketFill::cancel));
    h.set(TARGET, water_source());
    h.hold(bucket.clone());
    use_on(&mut h, TARGET, Face::West);
    assert_eq!(
        (h.block(TARGET), h.held()),
        (water_source(), bucket.clone())
    );
    assert_eq!(cancels(), 1);
    // Pouring: watched once the hand holds the empty bucket; cancelled, no water flows.
    let mut l = Listeners::default();
    l.push_monitor(move |e: &PlayerBucketEmpty, cx: &Context<'_>| {
        let hand = cx.state.players[&SessionId::new(1)]
            .inventory
            .held()
            .clone();
        note(WATCHED, (e.block_clicked(), e.block_face(), hand));
    });
    let mut h = Harness::with(|_| {}, l);
    h.hold(stack(ItemKind::WaterBucket));
    use_on(&mut h, GROUND, Face::Up);
    assert_eq!(h.block(TARGET).kind(), Kind::Water);
    assert_eq!(notes(WATCHED), [dbg((GROUND, Face::Up, bucket))]);
    let mut h = Harness::with(|_| {}, cancel(PlayerBucketEmpty::cancel));
    h.hold(stack(ItemKind::WaterBucket));
    use_on(&mut h, GROUND, Face::Up);
    assert_eq!(h.block(TARGET), block::AIR);
    assert_eq!(h.held(), stack(ItemKind::WaterBucket));
    assert_eq!(cancels(), 1);
}

/// Listeners that swap the hand for a stick.
fn take_hand<E: crate::event::Event>() -> Listeners {
    listen(|_: &mut E, cx: &mut Context<'_>| set_hand(cx, stack(ItemKind::Stick)))
}

// pmmp: PlayerBucketFillEvent, PlayerBucketEmptyEvent, SignChangeEvent
#[test]
fn a_listener_that_takes_the_bucket_or_the_dye_cancels_the_use() {
    let stick = stack(ItemKind::Stick);
    let mut h = Harness::with(|_| {}, take_hand::<PlayerBucketFill>());
    h.set(TARGET, water_source());
    h.hold(stack(ItemKind::Bucket));
    use_on(&mut h, TARGET, Face::West);
    assert_eq!((h.block(TARGET), h.held()), (water_source(), stick.clone()));
    let mut h = Harness::with(|_| {}, take_hand::<PlayerBucketEmpty>());
    h.hold(stack(ItemKind::WaterBucket));
    use_on(&mut h, GROUND, Face::Up);
    assert_eq!((h.block(TARGET), h.held()), (block::AIR, stick.clone()));
    let mut h = Harness::with(|_| {}, take_hand::<SignChange>());
    let sign = stack(ItemKind::OakSign);
    h.hold(sign.clone());
    place_on_grass(&mut h, &sign);
    h.run(Vec::new());
    h.hold(named("minecraft:red_dye"));
    use_on_target(&mut h);
    assert_eq!(h.held(), stick);
    assert_eq!(sign_at(&h).1, crate::tick::block_entity::BLACK_TEXT);
}

/// The front lines of the sign at [`TARGET`].
fn front_lines(h: &Harness) -> Vec<String> {
    decor::sign_text(&h.state, 0, TARGET)
        .unwrap()
        .front
        .to_vec()
}

// pmmp: SignChangeEvent
#[test]
fn a_sign_change_listener_sees_the_edit_and_may_rewrite_or_cancel_it() {
    let l = listen(move |e: &mut SignChange, _: &mut Context<'_>| {
        note(
            SEEN,
            (
                e.player(),
                e.pos(),
                e.old_text().clone(),
                e.new_text().clone(),
                e.side(),
            ),
        );
        e.set_new_text(["a", "b", "§cc", "d"].map(String::from));
    });
    let mut h = Harness::with(|_| {}, l);
    let sign = stack(ItemKind::OakSign);
    h.hold(sign.clone());
    place_on_grass(&mut h, &sign);
    sign_edit(&mut h, "hello\nworld");
    let empty: [String; 4] = Default::default();
    let hello = ["hello", "world", "", ""].map(String::from);
    assert_eq!(
        notes(SEEN),
        [dbg((
            me(),
            TARGET,
            empty.clone(),
            hello,
            crate::tile::Side::Front
        ))]
    );
    assert_eq!(front_lines(&h), ["a", "b", "§cc", "d"]);
    assert_eq!(h.player().breaking.sign_editor, None);
    // Cancelled, or text that fails the checks: the old text stays and the client gets it back,
    // the sign's data after the block, as the client foresaw its new text.
    let newline = listen(|e: &mut SignChange, _: &mut Context<'_>| {
        e.set_new_text(["a\nb", "", "", ""].map(String::from));
    });
    let long = listen(|e: &mut SignChange, _: &mut Context<'_>| {
        e.set_new_text([
            "x".repeat(1001),
            String::new(),
            String::new(),
            String::new(),
        ]);
    });
    for listeners in [cancel(SignChange::cancel), newline, long] {
        let mut h = Harness::with(|_| {}, listeners);
        h.hold(sign.clone());
        place_on_grass(&mut h, &sign);
        h.packets();
        sign_edit(&mut h, "hello");
        assert_eq!(front_lines(&h), empty);
        let packets = h.packets();
        assert!(updates(&packets).contains(&TARGET));
        let at = [TARGET.x, TARGET.y, TARGET.z];
        let block = packets
            .iter()
            .position(|p| matches!(p, Packet::UpdateBlock(u) if u.position == at));
        let data = packets
            .iter()
            .position(|p| matches!(p, Packet::BlockActorData(d) if d.position == at));
        assert!(data.is_some() && block < data, "{packets:?}");
    }
    assert_eq!(cancels(), 1);
}

// pmmp: SignChangeEvent
#[test]
fn a_cancelled_sign_dye_keeps_the_dye_and_opens_the_editor() {
    let mut h = Harness::with(|_| {}, cancel(SignChange::cancel));
    let sign = stack(ItemKind::OakSign);
    h.hold(sign.clone());
    place_on_grass(&mut h, &sign);
    h.run(Vec::new());
    h.packets();
    let dye = named("minecraft:red_dye").with_count(2);
    h.hold(dye.clone());
    use_on_target(&mut h);
    assert_eq!(h.held(), dye);
    assert_eq!(cancels(), 1);
    assert_eq!(sign_at(&h).1, crate::tick::block_entity::BLACK_TEXT);
    assert!(h.packets().iter().any(|p| matches!(p, Packet::OpenSign(_))));
    // A listener that rewrites the text with the dye writes both.
    let l = listen(|e: &mut SignChange, _: &mut Context<'_>| {
        e.set_new_text(["dyed", "", "", ""].map(String::from));
    });
    let mut h = Harness::with(|_| {}, l);
    h.hold(sign.clone());
    place_on_grass(&mut h, &sign);
    h.run(Vec::new());
    h.hold(dye);
    use_on_target(&mut h);
    assert_eq!(h.held().count(), 1);
    assert_eq!(sign_at(&h).0[0], "dyed");
    assert_ne!(sign_at(&h).1, crate::tick::block_entity::BLACK_TEXT);
}

/// A villager NPC two blocks south of the player: its entity ID and runtime ID.
fn villager(h: &mut Harness) -> (crate::entity::Id, u64) {
    let apple = Stack::new(ItemKind::Apple.default_type(), 2);
    let bread = Stack::new(ItemKind::Bread.default_type(), 1);
    let offer = Compound::from([
        ("buyA".to_owned(), Value::Compound(apple.to_nbt())),
        ("sell".to_owned(), Value::Compound(bread.to_nbt())),
        ("maxUses".to_owned(), Value::Int(12)),
    ]);
    let offers = crate::tick::trade::Offers::from_nbt(
        &Compound::from([(
            "Recipes".to_owned(),
            Value::List(crate::nbt::List::Compound(vec![offer])),
        )]),
        &crate::custom::Registry::default(),
    );
    let npc = entity::Npc::Villager {
        profession: 1,
        offers,
    };
    let id = entity::spawn_npc(&mut h.state, 0, Vec3::new(0.5, -60.0, 2.5), npc).unwrap();
    let runtime_id = entity::villager(&h.state, id).unwrap().2;
    (id, runtime_id)
}

/// The player's right click on the entity `runtime_id` at `at`.
fn entity_click(h: &mut Harness, runtime_id: u64, at: [f32; 3]) -> Input {
    h.input(Packet::InventoryTransaction(InventoryTransaction {
        transaction_data: InventoryTransactionData::UseItemOnEntity(Box::new(
            UseItemOnEntityTransactionData {
                target_entity_runtime_id: runtime_id,
                action_type: UseItemOnEntityAction::Interact.into(),
                clicked_position: at,
                ..Default::default()
            },
        )),
        ..Default::default()
    }))
}

/// The player right-clicks the entity `runtime_id` at `at`.
fn use_on_entity(h: &mut Harness, runtime_id: u64, at: [f32; 3]) {
    let input = entity_click(h, runtime_id, at);
    h.run(vec![input]);
}

/// The trade screens sent since the last call.
fn trades_opened(h: &mut Harness) -> usize {
    h.packets()
        .iter()
        .filter(|p| matches!(p, Packet::UpdateTrade(_)))
        .count()
}

fn trade_opened(h: &mut Harness) -> bool {
    trades_opened(h) > 0
}

#[test]
fn a_client_opens_trading_at_most_once_a_tick() {
    let mut h = Harness::new(|_| {});
    let (id, runtime_id) = villager(&mut h);
    h.run(Vec::new());
    h.packets();
    let clicks = (0..CLICKS)
        .map(|_| entity_click(&mut h, runtime_id, [0.0; 3]))
        .collect();
    h.run(clicks);
    assert_eq!(trades_opened(&mut h), 1);
    use_on_entity(&mut h, runtime_id, [0.0; 3]);
    assert_eq!(trades_opened(&mut h), 1, "the next tick opens it again");
    for _ in 0..3 {
        perform(&mut h, action::InteractEntity::new(me(), id, Vec3::ZERO));
    }
    assert_eq!(trades_opened(&mut h), 3, "the API is not limited");
}

// pmmp: PlayerEntityInteractEvent
#[test]
fn an_entity_interact_listener_sees_the_villager_and_cancelling_opens_no_trade() {
    let mut l = listen(move |e: &mut PlayerEntityInteract, _: &mut Context<'_>| {
        note(SEEN, (e.player(), e.entity(), e.click_position()));
    });
    l.push_monitor(move |e: &PlayerEntityInteract, _: &Context<'_>| {
        note(WATCHED, e.entity());
    });
    let mut h = Harness::with(|_| {}, l);
    let (id, runtime_id) = villager(&mut h);
    h.run(Vec::new());
    h.packets();
    use_on_entity(&mut h, runtime_id, [0.25, 1.5, f32::NAN]);
    assert_eq!(notes(SEEN), [dbg((me(), id, Vec3::new(0.25, 1.5, 0.0)))]);
    assert_eq!(notes(WATCHED), [dbg(id)]);
    assert!(trade_opened(&mut h));
    let mut h = Harness::with(|_| {}, cancel(PlayerEntityInteract::cancel));
    let (_, runtime_id) = villager(&mut h);
    h.run(Vec::new());
    h.packets();
    use_on_entity(&mut h, runtime_id, [0.0; 3]);
    assert!(!trade_opened(&mut h));
    assert_eq!(cancels(), 1);
}

// pmmp: BlockTeleportEvent, BlockTeleportEvent::setTo
#[test]
fn a_dragon_egg_teleport_listener_may_send_it_elsewhere_or_keep_it() {
    let egg = Kind::DragonEgg.default_state();
    let to = BlockPos::new(3, -60, 2);
    let send = listen(move |e: &mut BlockTeleport, _: &mut Context<'_>| {
        note(SEEN, (e.world(), e.pos(), e.block()));
        e.set_to(to);
    });
    let mut h = Harness::with(|_| {}, send);
    h.set(TARGET, egg);
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    assert_eq!((h.block(TARGET), h.block(to)), (block::AIR, egg));
    assert_eq!(notes(SEEN), [dbg((crate::world::Id::DEFAULT, TARGET, egg))]);
    // Cancelled, or sent into the ground: the egg stays.
    let ground_target = listen(|e: &mut BlockTeleport, _: &mut Context<'_>| {
        e.set_to(BlockPos::new(3, -61, 2));
    });
    for listeners in [cancel(BlockTeleport::cancel), ground_target] {
        let mut h = Harness::with(|_| {}, listeners);
        h.set(TARGET, egg);
        h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
        assert_eq!(h.block(TARGET), egg);
    }
    assert_eq!(cancels(), 1);
}

/// Does `action` through [`Context::perform`] and runs it.
fn perform(h: &mut Harness, a: impl crate::action::Action) {
    Context {
        state: &mut h.state,
    }
    .perform(a)
    .unwrap();
    let mut runs = 0;
    crate::tick::dispatch::run_deferred(&mut h.state, &mut runs);
}

// pmmp: Player::breakBlock, Player::interactBlock, Player::useHeldItem, Player::attackBlock, Player::interactEntity
#[test]
fn actions_do_what_the_player_would_with_events() {
    let l = listen(move |e: &mut BlockBreak, _: &mut Context<'_>| {
        note(SEEN, e.insta_break());
    });
    let mut h = Harness::with(|_| {}, l);
    // Breaking dirt by hand takes time; the action breaks it now.
    h.set(TARGET, Kind::Dirt.default_state());
    perform(&mut h, action::BreakBlock::new(me(), TARGET));
    assert_eq!(h.block(TARGET), block::AIR);
    assert_eq!(notes(SEEN), [dbg(true)]);
    h.hold(block_item(Kind::OakPlanks, 3));
    perform(
        &mut h,
        action::UseItemOn::new(me(), GROUND, Face::Up, Vec3::new(0.5, f64::NAN, 7.0)),
    );
    assert_eq!(h.block(TARGET).kind(), Kind::OakPlanks);
    assert_eq!(h.held().count(), 2);
    let helmet = stack(ItemKind::IronHelmet);
    h.hold(helmet.clone());
    perform(&mut h, action::UseItem::new(me()));
    assert_eq!(h.player().inventory.armor()[0], helmet);
    let egg = Kind::DragonEgg.default_state();
    let at = BlockPos::new(3, -60, 0);
    h.set(at, egg);
    perform(&mut h, action::AttackBlock::new(me(), at, Face::West));
    assert_ne!(h.block(at), egg, "the egg jumped away");
    let (id, _) = villager(&mut h);
    h.run(Vec::new());
    h.packets();
    perform(&mut h, action::InteractEntity::new(me(), id, Vec3::ZERO));
    h.run(Vec::new());
    assert!(trade_opened(&mut h));
}

/// A break elsewhere, done or refused, leaves the block the player breaks alone (PMMP
/// `Player::stopBreakBlock` stops only the block it names).
#[test]
fn a_break_elsewhere_keeps_the_players_breaking() {
    let beside = BlockPos::new(2, -60, 1);
    let glass = Kind::Glass.default_state();
    for cancel in [false, true] {
        let l = if cancel {
            listen(move |e: &mut BlockBreak, _: &mut Context<'_>| {
                if e.pos() == beside {
                    e.cancel();
                }
            })
        } else {
            Listeners::default()
        };
        let mut h = Harness::with(|_| {}, l);
        h.set(TARGET, glass);
        h.set(beside, glass);
        h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
        h.packets();
        // Nothing to break: refused.
        perform(
            &mut h,
            action::BreakBlock::new(me(), BlockPos::new(3, -60, 1)),
        );
        perform(&mut h, action::BreakBlock::new(me(), beside));
        assert_eq!(h.block(beside) == glass, cancel);
        let target = h.player().breaking.target.as_ref().map(|t| t.pos);
        assert_eq!(target, Some(TARGET), "cancelled: {cancel}");
        assert!(
            !h.packets().iter().any(|p| matches!(p, Packet::LevelEvent(e)
            if e.event_type == i32::from(Event::StopBlockCracking)))
        );
        for _ in 0..8 {
            h.act(Vec::new());
        }
        h.act(vec![action(
            PlayerAction::PredictDestroyBlock,
            TARGET,
            Face::West,
        )]);
        assert_eq!(h.block(TARGET), block::AIR, "cancelled: {cancel}");
    }
}

/// One of each interaction, the same every time: the state hash after it.
fn every_interaction(listeners: Listeners) -> u64 {
    let mut h = Harness::with(|_| {}, listeners);
    // Place a flower and break it.
    h.hold(block_item(Kind::Dandelion, 4));
    use_on(&mut h, GROUND, Face::Up);
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    // Fill a bucket and pour it back.
    h.set(TARGET, water_source());
    h.hold(stack(ItemKind::Bucket));
    use_on(&mut h, TARGET, Face::West);
    use_on(&mut h, GROUND, Face::Up);
    // A sign: written, then dyed.
    h.set(TARGET, block::AIR);
    let sign = stack(ItemKind::OakSign);
    h.hold(sign.clone());
    place_on_grass(&mut h, &sign);
    sign_edit(&mut h, "hi");
    h.hold(named("minecraft:red_dye"));
    use_on_target(&mut h);
    // A dragon egg hit, a helmet put on, a villager used.
    let at = BlockPos::new(3, -60, 0);
    h.set(at, Kind::DragonEgg.default_state());
    h.act(vec![action(PlayerAction::StartBreak, at, Face::West)]);
    h.hold(stack(ItemKind::IronHelmet));
    use_in_air(&mut h);
    let (_, runtime_id) = villager(&mut h);
    h.run(Vec::new());
    use_on_entity(&mut h, runtime_id, [0.0; 3]);
    h.run(Vec::new());
    h.state.hash()
}

/// Listeners that do nothing or monitors on every interaction event.
fn idle(monitors: bool) -> Listeners {
    fn on<E: crate::event::Event>(l: &mut Listeners, monitors: bool) {
        if monitors {
            l.push_monitor(|_: &E, _: &Context<'_>| {});
        } else {
            l.push(Priority::Normal, |_: &mut E, _: &mut Context<'_>| {});
        }
    }
    let mut l = Listeners::default();
    on::<BlockBreak>(&mut l, monitors);
    on::<BlockPlace>(&mut l, monitors);
    on::<BlockTeleport>(&mut l, monitors);
    on::<PlayerInteract>(&mut l, monitors);
    on::<PlayerItemUse>(&mut l, monitors);
    on::<PlayerBucketEmpty>(&mut l, monitors);
    on::<PlayerBucketFill>(&mut l, monitors);
    on::<SignChange>(&mut l, monitors);
    on::<PlayerEntityInteract>(&mut l, monitors);
    l
}

#[test]
fn idle_listeners_and_monitors_leave_the_game_as_without_them() {
    let without = every_interaction(Listeners::default());
    assert_eq!(every_interaction(idle(true)), without, "monitors");
    assert_eq!(every_interaction(idle(false)), without, "idle listeners");
}

/// A listener's random meddling, from a fixed seed: cancelling, taking the hand's flowers into a
/// vault, giving them back, picking up a flower or putting one down where the action happens.
#[derive(Debug, Clone, Copy)]
struct Chaos {
    seed: u64,
    vault: u32,
    /// Listener calls.
    calls: u32,
}

impl Chaos {
    fn next(&mut self) -> u64 {
        // xorshift64.
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed
    }

    /// Meddles at `at`; true to cancel.
    fn meddle(&mut self, cx: &mut Context<'_>, at: Option<BlockPos>) -> bool {
        self.calls += 1;
        let flower = block_item(Kind::Dandelion, 1);
        let player = cx.state.players.get_mut(&SessionId::new(1)).unwrap();
        let hand = player.inventory.held().clone();
        match self.next() % 6 {
            0 => return true,
            1 if hand.item() == flower.item() => {
                self.vault += u32::from(hand.count());
                inventory::set_held(player, Stack::EMPTY);
            }
            2 if hand.is_empty() && self.vault > 0 => {
                let n = self.vault.min(64);
                self.vault -= n;
                inventory::set_held(player, flower.with_count(u8::try_from(n).unwrap()));
            }
            3 => {
                if let Some(at) = at
                    && cx.state.worlds[0].block(at) == Some(dandelion())
                {
                    cx.state.worlds[0].set_block(at, block::AIR).unwrap();
                    self.vault += 1;
                }
            }
            4 => {
                if let Some(at) = at
                    && self.vault > 0
                    && cx.state.worlds[0].block(at) == Some(block::AIR)
                {
                    cx.state.worlds[0].set_block(at, dandelion()).unwrap();
                    self.vault -= 1;
                }
            }
            _ => {}
        }
        false
    }
}

thread_local! {
    /// The meddling of this test thread's listeners.
    static CHAOS: Cell<Chaos> = const {
        Cell::new(Chaos {
            seed: 0,
            vault: 0,
            calls: 0,
        })
    };
}

/// Meddles at `at` with this thread's [`CHAOS`]; true to cancel.
fn meddle(cx: &mut Context<'_>, at: Option<BlockPos>) -> bool {
    let mut chaos = CHAOS.get();
    let cancelled = chaos.meddle(cx, at);
    CHAOS.set(chaos);
    cancelled
}

/// Every flower: in the inventory, the vault, the world around the targets and on the ground.
fn flowers(h: &mut Harness, chaos: Chaos) -> u32 {
    let flower = block_item(Kind::Dandelion, 1).item().unwrap();
    let carried: u32 = (0..36)
        .filter_map(|slot| h.player().inventory.main_stack(slot))
        .filter(|s| s.item() == Some(flower))
        .map(|s| u32::from(s.count()))
        .sum();
    let mut placed = 0;
    for x in -3..=6 {
        for z in -3..=3 {
            if h.block(BlockPos::new(x, -60, z)) == dandelion() {
                placed += 1;
            }
        }
    }
    carried + chaos.vault + placed + ground(h, flower).0
}

// pmmp: BlockBreakEvent, BlockPlaceEvent, PlayerInteractEvent
#[test]
fn listeners_that_meddle_at_random_neither_make_nor_lose_items() {
    for seed in [1, 0x9e37_79b9, 0xdead_beef] {
        CHAOS.set(Chaos {
            seed,
            vault: 0,
            calls: 0,
        });
        let mut meddlers = Listeners::default();
        meddlers.push(
            Priority::Normal,
            |e: &mut BlockBreak, cx: &mut Context<'_>| {
                if meddle(cx, Some(e.pos())) {
                    e.cancel();
                }
            },
        );
        meddlers.push(
            Priority::Normal,
            |e: &mut BlockPlace, cx: &mut Context<'_>| {
                if meddle(cx, e.blocks().first().map(|(p, _)| *p)) {
                    e.cancel();
                }
            },
        );
        meddlers.push(
            Priority::Normal,
            |e: &mut PlayerInteract, cx: &mut Context<'_>| {
                if meddle(cx, e.pos().step(e.face())) {
                    e.cancel();
                }
            },
        );
        let mut harness = Harness::with(|_| {}, meddlers);
        harness.hold(block_item(Kind::Dandelion, 32));
        let mut pick = Chaos {
            seed: !seed,
            vault: 0,
            calls: 0,
        };
        for _ in 0..300 {
            let roll = pick.next();
            let x = 2 + i32::try_from(roll % 3).unwrap();
            let z = i32::try_from(roll / 3 % 3).unwrap() - 1;
            if (roll / 9).is_multiple_of(2) {
                use_on(&mut harness, BlockPos::new(x, -61, z), Face::Up);
            } else {
                harness.act(vec![action(
                    PlayerAction::PredictDestroyBlock,
                    BlockPos::new(x, -60, z),
                    Face::West,
                )]);
            }
        }
        let chaos = CHAOS.get();
        assert!(chaos.calls > 30, "seed {seed}: {} calls", chaos.calls);
        assert_eq!(flowers(&mut harness, chaos), 32, "seed {seed}");
    }
}

// pmmp: SignChangeEvent::setNewText
/// A listener's text goes on the sign as it is (PMMP cleans the player's text only) and the
/// monitors see what went on; text over a sign's 1,000 bytes in all is refused.
#[test]
fn a_listeners_sign_text_goes_on_as_it_is_and_monitors_see_it() {
    let glyph = ["§ca", "b\u{E000}", "", ""].map(String::from);
    let mut l = listen(|e: &mut SignChange, _: &mut Context<'_>| {
        e.set_new_text(["§ca", "b\u{E000}", "", ""].map(String::from));
    });
    l.push_monitor(|e: &SignChange, _: &Context<'_>| {
        note(WATCHED, e.new_text());
    });
    let mut h = Harness::with(|_| {}, l);
    let sign = stack(ItemKind::OakSign);
    h.hold(sign.clone());
    place_on_grass(&mut h, &sign);
    sign_edit(&mut h, "hello");
    assert_eq!(front_lines(&h), glyph);
    assert_eq!(notes(WATCHED), [dbg(&glyph)]);
    let long = listen(|e: &mut SignChange, _: &mut Context<'_>| {
        e.set_new_text([
            "x".repeat(500),
            "x".repeat(501),
            String::new(),
            String::new(),
        ]);
    });
    let mut h = Harness::with(|_| {}, long);
    h.hold(sign.clone());
    place_on_grass(&mut h, &sign);
    sign_edit(&mut h, "hello");
    let empty: [String; 4] = Default::default();
    assert_eq!(front_lines(&h), empty);
}

thread_local! {
    /// Whether the re-entry test's break listener is running.
    static INSIDE: Cell<bool> = const { Cell::new(false) };
    /// What the re-entry test's listeners and monitors heard, in order.
    static HEARD: Cell<Vec<String>> = const { Cell::new(Vec::new()) };
}

/// Writes down `what` in [`HEARD`], marked when the break listener is running.
fn hear(what: &str) {
    let what = if INSIDE.get() {
        format!("inside {what}")
    } else {
        what.to_owned()
    };
    HEARD.with(|h| {
        let mut list = h.take();
        list.push(what);
        h.set(list);
    });
}

// pmmp: BlockBreakEvent, Player::breakBlock
#[test]
fn direct_calls_in_a_listener_raise_no_events_and_performed_actions_raise_theirs_after_it() {
    const BESIDE: BlockPos = BlockPos::new(2, -60, 1);
    const PLACED: BlockPos = BlockPos::new(3, -60, 1);
    let mut l = Listeners::default();
    l.listen_to_every_event(hear);
    l.monitor_every_event(|name| hear(&format!("monitor {name}")));
    l.push(
        Priority::Lowest,
        |e: &mut BlockBreak, cx: &mut Context<'_>| {
            if e.pos() != TARGET {
                hear("break beside");
                return;
            }
            INSIDE.set(true);
            // Direct calls: a block set, an explosion that hurts and pushes the player, an item
            // given. None raises an event.
            let world = crate::world::Id::DEFAULT;
            let stone = Kind::Stone.default_state();
            cx.default_world_mut().set_block(PLACED, stone).unwrap();
            assert!(cx.explode(world, Vec3::new(-0.5, -60.0, 0.5), 1.0));
            let p = cx.player_mut(me()).unwrap();
            let left = p
                .inventory_mut(crate::inventory::Kind::Main)
                .add(&stack(ItemKind::Stick));
            assert!(left.is_empty());
            // Deferred: they run once this listener has returned.
            cx.perform(action::BreakBlock::new(me(), BESIDE)).unwrap();
            cx.defer(|_| hear("deferred")).unwrap();
            INSIDE.set(false);
            hear("returned");
        },
    );
    let mut h = Harness::with(|_| {}, l);
    h.set(TARGET, dandelion());
    h.set(BESIDE, dandelion());
    h.hold(stack(ItemKind::DiamondSword));
    h.run(Vec::new());
    HEARD.with(Cell::take);
    let health = h.player().health();
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    let heard = HEARD.with(Cell::take);
    assert!(!heard.iter().any(|w| w.starts_with("inside")), "{heard:?}");
    for quiet in [
        "block_place",
        "entity_damage",
        "entity_motion",
        "entity_pre_explode",
        "entity_explode",
        "inventory_transaction",
    ] {
        assert!(
            !heard.iter().any(|w| w.ends_with(quiet)),
            "{quiet}: {heard:?}"
        );
    }
    let at = |what: &str| heard.iter().position(|w| w == what).expect(what);
    assert!(at("returned") < at("break beside"), "{heard:?}");
    assert!(at("break beside") < at("deferred"), "{heard:?}");
    let breaks: Vec<usize> = (0..heard.len())
        .filter(|&i| heard[i] == "block_break")
        .collect();
    assert_eq!(breaks.len(), 2, "{heard:?}");
    assert!(
        breaks[1] > at("break beside"),
        "the performed break raised its event"
    );
    assert_eq!(h.block(TARGET), block::AIR);
    assert_eq!(h.block(BESIDE), block::AIR);
    assert_eq!(h.block(PLACED), Kind::Stone.default_state());
    assert!(h.player().health() < health, "the explosion hurt");
}

// pmmp: Explosion::explodeB, PlayerDeathEvent
#[test]
fn a_direct_explosion_kills_without_events_and_the_death_follows_in_the_tick() {
    let mut l = Listeners::default();
    l.listen_to_every_event(hear);
    let mut h = Harness::with(|_| {}, l);
    h.player_mut().set_health(1.0).unwrap();
    HEARD.with(Cell::take);
    let mut cx = Context {
        state: &mut h.state,
    };
    assert!(cx.explode(crate::world::Id::DEFAULT, Vec3::new(0.5, -60.0, 0.5), 2.0));
    assert!(HEARD.with(Cell::take).is_empty(), "the call raised nothing");
    assert!(!h.player().is_alive());
    h.run(Vec::new());
    let heard = HEARD.with(Cell::take);
    assert!(heard.iter().any(|w| w == "player_death"), "{heard:?}");
    assert!(!heard.iter().any(|w| w == "entity_damage"), "{heard:?}");
}

/// The diamonds in `stack`, those in a shulker box's items included.
fn diamonds(stack: &Stack) -> u32 {
    let own = if stack.item() == Some(ItemKind::Diamond.default_type()) {
        u32::from(stack.count())
    } else {
        0
    };
    let inside: u32 = match stack.tag().as_ref().and_then(|t| t.get("Items")) {
        Some(Value::List(crate::nbt::List::Compound(items))) => items
            .iter()
            .filter_map(Stack::from_nbt)
            .map(|s| diamonds(&s))
            .sum(),
        _ => 0,
    };
    own + inside
}

/// Every diamond of the game: the player's, on the ground (taken off it) and in the block
/// entity at [`TARGET`].
fn every_diamond(h: &mut Harness) -> u32 {
    let held: u32 = (0..36)
        .filter_map(|i| h.player().inventory.main_stack(i))
        .map(diamonds)
        .sum();
    let mut ground = 0;
    for x in -2..=2 {
        for z in -2..=2 {
            let key = (0, crate::world::ChunkPos::new(x, z));
            for c in h.state.rules.entities.unload(key).unwrap_or_default() {
                if let Some(Value::Compound(s)) = c.get("Item")
                    && let Some(stack) = Stack::from_nbt(s)
                {
                    ground += diamonds(&stack);
                }
            }
        }
    }
    let kept: u32 = h
        .state
        .rules
        .block_entities
        .get(0, TARGET)
        .map_or(0, |e| e.items.iter().map(diamonds).sum());
    held + ground + kept
}

// pmmp: InventoryCloseEvent
#[test]
fn breaking_an_open_shulker_with_a_close_listener_keeps_the_item_total() {
    for cause in [
        Destroy::Player {
            session: SessionId::new(1),
            tool: Stack::EMPTY,
        },
        Destroy::Explosion {
            size: 1.0,
            raise: Raise::Events,
        },
    ] {
        // The listener moves the box's items to the player when its window closes.
        let l = listen(|_: &mut InventoryClose, cx: &mut Context<'_>| {
            let Some(mut inv) = cx.block_inventory_mut(crate::world::Id::DEFAULT, TARGET) else {
                return;
            };
            inv.clear_all();
            let diamonds = Stack::new(ItemKind::Diamond.default_type(), 64);
            let player = cx.state.players.get_mut(&SessionId::new(1)).unwrap();
            let left = player
                .inventory_mut(crate::inventory::Kind::Main)
                .add(&diamonds);
            assert!(left.is_empty());
        });
        let mut h = Harness::with(|_| {}, l);
        set_with_entity(&mut h, Kind::ShulkerBox.default_state(), &Stack::EMPTY);
        h.state
            .rules
            .block_entities
            .get_mut(0, TARGET)
            .unwrap()
            .items[0] = Stack::new(ItemKind::Diamond.default_type(), 64);
        h.run(Vec::new());
        use_on(&mut h, TARGET, Face::West);
        assert!(h.player().window().is_some(), "the box is open");
        let _destroyed = destroy(&mut h.state, 0, TARGET, cause.clone());
        h.run(Vec::new());
        assert_eq!(every_diamond(&mut h), 64, "{cause:?}");
    }
}

/// An explosion of a direct call (`Context::explode`) closes the windows on the blocks it breaks
/// without [`InventoryClose`]; one of the game raises it.
#[test]
fn a_quiet_explosion_closes_windows_without_inventory_close() {
    for (raise, heard) in [(Raise::Quiet, 0), (Raise::Events, 1)] {
        let l = listen(|_: &mut InventoryClose, _: &mut Context<'_>| {
            note(SEEN, "closed");
        });
        let mut h = Harness::with(|_| {}, l);
        set_with_entity(&mut h, Kind::Chest.default_state(), &Stack::EMPTY);
        h.run(Vec::new());
        use_on(&mut h, TARGET, Face::West);
        assert!(h.player().window().is_some(), "the chest is open");
        let cause = Destroy::Explosion { size: 1.0, raise };
        assert!(destroy(&mut h.state, 0, TARGET, cause));
        assert!(h.player().window().is_none(), "{raise:?}");
        assert_eq!(notes(SEEN).len(), heard, "{raise:?}");
    }
}

// pmmp: PlayerExhaustEvent, Player::breakBlock
#[test]
fn mining_raises_player_exhaust_with_the_mining_cause() {
    let l = listen(|e: &mut crate::event::PlayerExhaust, _: &mut Context<'_>| {
        note(SEEN, (e.player(), e.cause()));
    });
    let mut h = Harness::with(|_| {}, l);
    h.set(TARGET, dandelion());
    notes(SEEN);
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(h.block(TARGET), block::AIR);
    let seen = notes(SEEN);
    let mining = dbg((me(), crate::event::ExhaustCause::Mining));
    assert!(seen.contains(&mining), "{seen:?}");
}

/// A respawn anchor with `charges`.
fn anchor(charges: i32) -> block::State {
    props::with_int(
        Kind::RespawnAnchor.default_state(),
        "respawn_anchor_charge",
        charges,
    )
    .unwrap()
}

/// Runs ticks until a click like the last one is not taken for the client repeating itself.
fn past_repeat(h: &mut Harness) {
    for _ in 0..3 {
        h.run(Vec::new());
    }
}

/// A charged anchor east of the player and a stick in its hand; the anchor is used.
fn use_charged_anchor(h: &mut Harness) {
    h.set(TARGET, anchor(1));
    h.hold(stack(ItemKind::Stick));
    use_on(h, TARGET, Face::West);
}

// pmmp: PlayerRespawnAnchorUseEvent, BlockPreExplodeEvent, BlockExplodeEvent
#[test]
fn glowstone_charges_a_respawn_anchor_and_a_charged_one_explodes_in_any_world() {
    let mut h = Harness::new(|_| {});
    h.set(TARGET, anchor(0));
    h.hold(block_item(Kind::Glowstone, 2));
    use_on(&mut h, TARGET, Face::West);
    assert_eq!(h.block(TARGET), anchor(1));
    assert_eq!(h.held().count(), 1, "one glowstone used");
    // Anything but glowstone: it explodes (PMMP: in every world, not only the nether).
    past_repeat(&mut h);
    h.hold(stack(ItemKind::Stick));
    use_on(&mut h, TARGET, Face::West);
    assert_eq!(h.block(TARGET), block::AIR, "the anchor is gone");
    assert_ne!(
        h.block(GROUND),
        block::GRASS_BLOCK,
        "a radius 5 explosion breaks the ground under it"
    );
    assert!(!h.player().is_alive(), "two blocks from a size 5 explosion");
}

// pmmp: PlayerRespawnAnchorUseEvent, BlockPreExplodeEvent, BlockExplodeEvent
#[test]
fn an_anchor_explosion_is_the_same_on_every_run() {
    let run = || {
        let mut h = Harness::new(|_| {});
        use_charged_anchor(&mut h);
        h.run(Vec::new());
        h.state.hash()
    };
    assert_eq!(run(), run());
}

// pmmp: PlayerRespawnAnchorUseEvent, PlayerRespawnAnchorUseEvent::getBlock, PlayerRespawnAnchorUseEvent::getAction, BlockPreExplodeEvent, BlockPreExplodeEvent::getRadius, BlockPreExplodeEvent::getFireChance, BlockPreExplodeEvent::getPlayer, BlockPreExplodeEvent::isBlockBreaking, BlockExplodeEvent, BlockExplodeEvent::getPosition, BlockExplodeEvent::getAffectedBlocks, BlockExplodeEvent::getYield
#[test]
fn anchor_listeners_see_the_use_the_coming_explosion_and_its_blocks_and_monitors_the_outcome() {
    let mut l = Listeners::default();
    l.push(
        Priority::Normal,
        |e: &mut PlayerRespawnAnchorUse, _: &mut Context<'_>| {
            note(SEEN, ("use", e.player(), e.pos(), e.block(), e.action()));
        },
    );
    l.push(
        Priority::Normal,
        |e: &mut BlockPreExplode, _: &mut Context<'_>| {
            note(
                SEEN,
                (
                    "pre",
                    e.pos(),
                    e.block(),
                    e.player(),
                    e.radius(),
                    e.fire_chance(),
                    e.is_block_breaking(),
                ),
            );
        },
    );
    l.push(
        Priority::Normal,
        |e: &mut BlockExplode, _: &mut Context<'_>| {
            note(
                SEEN,
                (
                    "explode",
                    e.pos(),
                    e.position(),
                    e.blocks().contains(&GROUND),
                    e.yield_percent(),
                ),
            );
        },
    );
    l.push_monitor(|_: &PlayerRespawnAnchorUse, cx: &Context<'_>| {
        note(WATCHED, ("use", cx.state.worlds[0].block(TARGET)));
    });
    l.push_monitor(|_: &BlockPreExplode, cx: &Context<'_>| {
        note(WATCHED, ("pre", cx.state.worlds[0].block(TARGET)));
    });
    l.push_monitor(|e: &BlockExplode, cx: &Context<'_>| {
        let ground = cx.state.worlds[0].block(GROUND);
        note(WATCHED, ("explode", e.blocks().contains(&GROUND), ground));
    });
    let mut h = Harness::with(|_| {}, l);
    use_charged_anchor(&mut h);
    let center = Vec3::new(2.5, -59.5, 0.5);
    assert_eq!(
        notes(SEEN),
        [
            dbg(("use", me(), TARGET, anchor(1), RespawnAnchorAction::Explode)),
            dbg(("pre", TARGET, anchor(1), Some(me()), 5.0, 1.0 / 3.0, true)),
            dbg(("explode", TARGET, center, true, 20.0)),
        ]
    );
    let ground = h.block(GROUND);
    assert_ne!(ground, block::GRASS_BLOCK);
    assert_eq!(
        notes(WATCHED),
        [
            dbg(("explode", true, Some(ground))),
            dbg(("pre", Some(block::AIR))),
            dbg(("use", Some(block::AIR))),
        ]
    );
}

// pmmp: PlayerRespawnAnchorUseEvent::cancel, BlockPreExplodeEvent::cancel, BlockExplodeEvent::cancel
#[test]
fn cancelling_an_anchor_event_stops_what_it_announces() {
    let grass = block::GRASS_BLOCK;
    // The use: the anchor stays charged; a stick is not placeable, so nothing happens.
    let mut h = Harness::with(|_| {}, cancel(PlayerRespawnAnchorUse::cancel));
    use_charged_anchor(&mut h);
    assert_eq!(h.block(TARGET), anchor(1));
    assert!(h.player().is_alive());
    // The coming explosion: the anchor stays.
    let mut h = Harness::with(|_| {}, cancel(BlockPreExplode::cancel));
    use_charged_anchor(&mut h);
    assert_eq!(h.block(TARGET), anchor(1));
    assert!(h.player().is_alive());
    // The explosion: the anchor is gone, but nothing breaks and nobody is hurt.
    let mut h = Harness::with(|_| {}, cancel(BlockExplode::cancel));
    use_charged_anchor(&mut h);
    assert_eq!((h.block(TARGET), h.block(GROUND)), (block::AIR, grass));
    assert_eq!(h.player().health(), 20.0);
    assert_eq!(cancels(), 3);
}

// pmmp: PlayerRespawnAnchorUseEvent::setAction, Player::setSpawn
#[test]
fn a_listener_may_make_the_anchor_set_the_spawn_once() {
    let set = listen(|e: &mut PlayerRespawnAnchorUse, _: &mut Context<'_>| {
        e.set_action(RespawnAnchorAction::SetSpawn);
    });
    let mut h = Harness::with(|_| {}, set);
    use_charged_anchor(&mut h);
    let name = h.state.worlds[0].name().to_owned();
    assert_eq!(h.player().spawn(), Some((name.as_str(), TARGET)));
    assert_eq!(h.block(TARGET), anchor(1), "nothing explodes");
    assert!(h.player().is_alive());
    let told = |packets: &[Packet]| {
        packets
            .iter()
            .filter(|p| {
                matches!(p, Packet::Text(t) if t.message == "§7%tile.respawn_anchor.respawnSet")
            })
            .count()
    };
    assert_eq!(told(&h.packets()), 1);
    past_repeat(&mut h);
    use_on(&mut h, TARGET, Face::West);
    assert_eq!(h.player().spawn(), Some((name.as_str(), TARGET)));
    assert_eq!(
        told(&h.packets()),
        0,
        "already its spawn: no second message"
    );
}

// pmmp: BlockPreExplodeEvent::setBlockBreaking, BlockPreExplodeEvent::setRadius, BlockPreExplodeEvent::setIncendiary
#[test]
fn a_pre_explode_listener_may_keep_the_blocks_and_shrink_the_blast() {
    let calm = listen(|e: &mut BlockPreExplode, _: &mut Context<'_>| {
        e.set_block_breaking(false);
        e.set_radius(0.5);
        e.set_incendiary(false);
    });
    let mut h = Harness::with(|_| {}, calm);
    use_charged_anchor(&mut h);
    assert_eq!(h.block(TARGET), block::AIR, "the anchor itself goes");
    assert_eq!(h.block(GROUND), block::GRASS_BLOCK);
    assert!(h.player().is_alive(), "a radius 0.5 blast does not reach");
}

// pmmp: BlockExplodeEvent::setAffectedBlocks, BlockExplodeEvent::setIgnitions, BlockExplodeEvent::setYield
#[test]
fn an_explode_listener_chooses_the_blocks() {
    let only_ground = listen(|e: &mut BlockExplode, _: &mut Context<'_>| {
        e.set_blocks(vec![GROUND]);
        e.set_ignitions(Vec::new());
        e.set_yield_percent(0.0);
    });
    let mut h = Harness::with(|_| {}, only_ground);
    use_charged_anchor(&mut h);
    assert_eq!(h.block(GROUND), block::AIR);
    let below = BlockPos::new(2, -62, 0);
    assert_eq!(h.block(below), Kind::Dirt.default_state(), "not listed");
    let dirt = block_item(Kind::Dirt, 1).item().unwrap();
    assert_eq!(ground(&mut h, dirt), (0, 0), "yield 0: no drops");
}

#[test]
fn a_listener_that_changes_the_anchor_cancels_its_use_and_its_explosion() {
    fn take(cx: &mut Context<'_>) {
        cx.state.worlds[0].set_block(TARGET, anchor(2)).unwrap();
    }
    let on_use = listen(|_: &mut PlayerRespawnAnchorUse, cx: &mut Context<'_>| take(cx));
    let on_pre = listen(|_: &mut BlockPreExplode, cx: &mut Context<'_>| take(cx));
    for listeners in [on_use, on_pre] {
        let mut h = Harness::with(|_| {}, listeners);
        use_charged_anchor(&mut h);
        assert_eq!(h.block(TARGET), anchor(2));
        assert!(h.player().is_alive());
    }
}

/// The trapdoor east of the player.
fn trapdoor(h: &mut Harness) {
    h.set(TARGET, Kind::OakTrapdoor.default_state());
}

/// Whether `packets` hold a door sound (PMMP `DoorSound`, level event `SoundOpenDoor`).
fn door_sounds(packets: &[Packet]) -> usize {
    packets
        .iter()
        .filter(|p| {
            matches!(p, Packet::LevelEvent(e) if e.event_type == i32::from(Event::SoundOpenDoor))
        })
        .count()
}

// pmmp: WorldSoundEvent, WorldSoundEvent::getSound, WorldSoundEvent::getPosition, WorldSoundEvent::getRecipients, World::addSound
#[test]
fn a_world_sound_listener_hears_what_the_game_plays_and_monitors_see_it_played() {
    let mut l = listen(|e: &mut crate::event::WorldSound, _: &mut Context<'_>| {
        note(
            SEEN,
            (e.world(), e.position(), e.sound(), e.recipients().to_vec()),
        );
    });
    l.push_monitor(|e: &crate::event::WorldSound, cx: &Context<'_>| {
        let queued = door_sounds(&cx.state.players[&SessionId::new(1)].outbox);
        note(WATCHED, (e.sound(), queued));
    });
    let mut h = Harness::with(|_| {}, l);
    trapdoor(&mut h);
    h.hold(Stack::EMPTY);
    use_on(&mut h, TARGET, Face::West);
    let door = crate::world::Sound::Door { pitch: 0.0 };
    let at = block_vec(TARGET);
    assert_eq!(
        notes(SEEN),
        [dbg((crate::world::Id::DEFAULT, at, door, vec![me()]))]
    );
    assert_eq!(notes(WATCHED), [dbg((door, 1))]);
    assert_eq!(door_sounds(&h.packets()), 1);
}

// pmmp: WorldSoundEvent::cancel, WorldSoundEvent::setSound, WorldSoundEvent::setRecipients
#[test]
fn a_world_sound_listener_may_silence_replace_or_keep_the_sound_from_players() {
    // Cancelled: the trapdoor opens without a sound.
    let mut h = Harness::with(|_| {}, cancel(crate::event::WorldSound::cancel));
    trapdoor(&mut h);
    use_on(&mut h, TARGET, Face::West);
    let opened = h.block(TARGET);
    assert_ne!(
        opened,
        Kind::OakTrapdoor.default_state(),
        "the trapdoor still opens"
    );
    assert_eq!(door_sounds(&h.packets()), 0);
    assert_eq!(cancels(), 1);
    // Another sound; then nobody hears it.
    let swap = listen(|e: &mut crate::event::WorldSound, _: &mut Context<'_>| {
        e.set_sound(crate::world::Sound::Burp);
    });
    let mut h = Harness::with(|_| {}, swap);
    trapdoor(&mut h);
    use_on(&mut h, TARGET, Face::West);
    let packets = h.packets();
    assert_eq!(door_sounds(&packets), 0);
    let burps = packets
        .iter()
        .filter(|p| matches!(p, Packet::LevelSoundEvent(e) if e.sound_type == "burp"))
        .count();
    assert_eq!(burps, 1);
    let nobody = listen(|e: &mut crate::event::WorldSound, _: &mut Context<'_>| {
        e.set_recipients(Vec::new());
    });
    let mut h = Harness::with(|_| {}, nobody);
    trapdoor(&mut h);
    use_on(&mut h, TARGET, Face::West);
    assert_eq!(door_sounds(&h.packets()), 0);
    // A recipient who is not online (or does not see the place) is left out.
    let stranger = listen(|e: &mut crate::event::WorldSound, _: &mut Context<'_>| {
        let mut all = e.recipients().to_vec();
        all.extend([crate::player::Id::new(SessionId::new(9)), me()]);
        e.set_recipients(all);
    });
    let mut h = Harness::with(|_| {}, stranger);
    trapdoor(&mut h);
    use_on(&mut h, TARGET, Face::West);
    assert_eq!(door_sounds(&h.packets()), 1, "once, though named twice");
}

// pmmp: WorldParticleEvent, WorldParticleEvent::getParticle, WorldParticleEvent::getPosition, WorldParticleEvent::getRecipients, WorldParticleEvent::cancel, World::addParticle
#[test]
fn a_world_particle_listener_sees_a_broken_block_and_may_hide_it() {
    let pieces = |packets: &[Packet]| {
        packets
            .iter()
            .filter(|p| {
                matches!(p, Packet::LevelEvent(e)
                    if e.event_type == i32::from(Event::ParticlesDestroyBlock))
            })
            .count()
    };
    let l = listen(|e: &mut crate::event::WorldParticle, _: &mut Context<'_>| {
        note(SEEN, (e.position(), e.particle(), e.recipients().to_vec()));
    });
    let mut h = Harness::with(creative, l);
    h.set(TARGET, dandelion());
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(h.block(TARGET), block::AIR);
    let broken = crate::world::Particle::BlockBreak(dandelion());
    assert_eq!(notes(SEEN), [dbg((center(TARGET), broken, vec![me()]))]);
    assert_eq!(pieces(&h.packets()), 1);
    let mut h = Harness::with(creative, cancel(crate::event::WorldParticle::cancel));
    h.set(TARGET, dandelion());
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(h.block(TARGET), block::AIR, "the block still breaks");
    assert_eq!(pieces(&h.packets()), 0);
    assert_eq!(cancels(), 1);
}

#[test]
fn direct_sounds_particles_and_explosions_raise_no_world_sound_or_particle() {
    let mut l = Listeners::default();
    l.listen_to_every_event(hear);
    let mut h = Harness::with(|_| {}, l);
    HEARD.with(Cell::take);
    let mut cx = Context {
        state: &mut h.state,
    };
    let at = Vec3::new(20.5, -60.0, 20.5);
    assert!(cx.add_sound(crate::world::Id::DEFAULT, at, crate::world::Sound::Explode));
    assert!(cx.add_particle(crate::world::Id::DEFAULT, at, crate::world::Particle::Flame));
    assert!(cx.explode(crate::world::Id::DEFAULT, at, 1.0));
    let heard = HEARD.with(Cell::take);
    assert!(
        !heard
            .iter()
            .any(|w| w == "world_sound" || w == "world_particle"),
        "{heard:?}"
    );
}

/// Returns a tool of `kind` with one use left.
fn worn_out(kind: ItemKind) -> Stack {
    let tool = stack(kind);
    let max = tool
        .item()
        .and_then(crate::item::Type::max_durability)
        .unwrap();
    tool.with_damage(max - 1).unwrap()
}

// pmmp: WorldSoundEvent, WorldParticleEvent, Player::breakBlock
/// The listeners of a break's sound and particle hear them once the break is applied: the worn
/// out tool is gone from the hand and the block from the world.
#[test]
fn a_breaks_sound_and_particle_listeners_see_it_applied() {
    let mut l = listen(|e: &mut crate::event::WorldSound, cx: &mut Context<'_>| {
        if e.sound() == crate::world::Sound::ItemBreak {
            let empty = cx.state.players[&SessionId::new(1)]
                .inventory
                .held()
                .is_empty();
            note(SEEN, ("item break", empty));
        }
    });
    l.push(
        Priority::Normal,
        |_: &mut crate::event::WorldParticle, cx: &mut Context<'_>| {
            note(SEEN, ("block break", cx.state.worlds[0].block(TARGET)));
        },
    );
    let mut h = Harness::with(|c| c.general.break_time_check = false, l);
    h.set(TARGET, Kind::Stone.default_state());
    h.hold(worn_out(ItemKind::IronPickaxe));
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(h.block(TARGET), block::AIR);
    assert!(h.held().is_empty(), "worn out");
    assert_eq!(
        notes(SEEN),
        [
            dbg(("item break", true)),
            dbg(("block break", Some(block::AIR))),
        ]
    );
}

// pmmp: WorldSoundEvent, Player::interactBlock
/// The listeners of a use's sound hear it once the held item is applied: a placed block and a
/// framed item are already gone from the hand.
#[test]
fn a_uses_sound_listeners_hear_it_once_the_held_item_is_applied() {
    let l = listen(|e: &mut crate::event::WorldSound, cx: &mut Context<'_>| {
        let held = cx.state.players[&SessionId::new(1)]
            .inventory
            .held()
            .count();
        note(SEEN, (e.sound(), held));
    });
    let mut h = Harness::with(|_| {}, l);
    let planks = block_item(Kind::OakPlanks, 5);
    h.hold(planks.clone());
    place_on_grass(&mut h, &planks);
    let placed = crate::world::Sound::BlockPlace(Kind::OakPlanks.default_state());
    assert_eq!(notes(SEEN), [dbg((placed, 4))]);
    // Hung on the west face of a stone east of it.
    h.set(BlockPos::new(3, -60, 0), Kind::Stone.default_state());
    let frame = block::State::all()
        .find(|b| b.name() == "minecraft:frame")
        .and_then(|f| props::with_facing(f, Face::West))
        .unwrap();
    set_with_entity(&mut h, frame, &Stack::EMPTY);
    h.run(Vec::new());
    h.hold(stack(ItemKind::Stick).with_count(3));
    use_on_target(&mut h);
    let framed = crate::world::Sound::ItemFrameAddItem;
    assert_eq!(notes(SEEN), [dbg((framed, 2))]);
}

// pmmp: BlockPlaceEvent, Player::returnItemsFromAction
/// A place listener that ends creative mode has the block paid for: the item is taken by the mode
/// the player has once the listeners ran.
#[test]
fn a_place_listener_that_ends_creative_mode_has_the_block_paid_for() {
    let survival = listen(|_: &mut BlockPlace, cx: &mut Context<'_>| {
        let player = cx.state.players.get_mut(&SessionId::new(1)).unwrap();
        assert!(player.set_game_mode(GameMode::Survival));
    });
    let mut h = Harness::with(creative, survival);
    let planks = block_item(Kind::OakPlanks, 5);
    h.hold(planks.clone());
    let at = place_on_grass(&mut h, &planks);
    assert_eq!(h.block(at).kind(), Kind::OakPlanks);
    assert_eq!(h.held().count(), 4);
}

// pmmp: BlockPlaceEvent
/// A place listener that moves the player out of reach cancels the placement, as a right click
/// listener's does.
#[test]
fn a_place_listener_that_moves_the_player_out_of_reach_cancels_the_placement() {
    let away = listen(|_: &mut BlockPlace, cx: &mut Context<'_>| {
        let eye = Vec3::new(20.5, -60.0 + EYE_HEIGHT, 0.5);
        survival::teleport(cx.state, SessionId::new(1), 0, eye);
    });
    let mut h = Harness::with(|_| {}, away);
    let planks = block_item(Kind::OakPlanks, 5);
    h.hold(planks.clone());
    let at = place_on_grass(&mut h, &planks);
    assert_eq!(h.block(at), block::AIR);
    assert_eq!(h.held().count(), 5);
}

// pmmp: PlayerRespawnAnchorUseEvent, BlockPreExplodeEvent::cancel
/// The monitors of an anchor's use see only a use that was applied: a cancelled explosion keeps
/// the anchor and tells them nothing.
#[test]
fn an_anchor_use_monitor_sees_only_an_applied_use() {
    let mut l = cancel(BlockPreExplode::cancel);
    l.push_monitor(|_: &PlayerRespawnAnchorUse, _: &Context<'_>| {
        note(WATCHED, "used");
    });
    let mut h = Harness::with(|_| {}, l);
    use_charged_anchor(&mut h);
    assert_eq!(h.block(TARGET), anchor(1));
    assert_eq!(cancels(), 1);
    assert!(notes(WATCHED).is_empty());
}

// pmmp: PlayerRespawnAnchorUseEvent::cancel
/// An anchor listener that changes the held item and cancels the use keeps the block once held
/// from being placed: the click goes on only as it was.
#[test]
fn an_anchor_listener_that_changes_the_hand_keeps_the_old_block_from_being_placed() {
    let swap = listen(|e: &mut PlayerRespawnAnchorUse, cx: &mut Context<'_>| {
        set_hand(cx, stack(ItemKind::Stick));
        e.cancel();
    });
    let mut h = Harness::with(|_| {}, swap);
    h.set(TARGET, anchor(1));
    h.hold(block_item(Kind::OakPlanks, 5));
    use_on(&mut h, TARGET, Face::West);
    assert_eq!(h.block(BlockPos::new(1, -60, 0)), block::AIR);
    assert_eq!(h.held(), stack(ItemKind::Stick));
}
