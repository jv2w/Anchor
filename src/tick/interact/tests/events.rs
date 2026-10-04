//! Tick tests of the interaction events and actions (PLAN.md 10.8): what listeners see, what
//! cancelling and changing fields do, what monitors see, and that listeners changing the game
//! neither make nor lose items.

use std::cell::Cell;
use std::fmt::Debug;

use super::*;
use crate::event::{
    BlockBreak, BlockPlace, BlockTeleport, InteractAction, Listeners, PlayerBucketEmpty,
    PlayerBucketFill, PlayerEntityInteract, PlayerInteract, PlayerItemUse, Priority, SignChange,
};
use crate::item::Kind as ItemKind;
use crate::nbt::{Compound, Value};
use crate::protocol::UseItemOnEntityTransactionData;
use crate::tick::Context;

thread_local! {
    /// What listeners ([`SEEN`]) and monitors ([`WATCHED`]) write down for the test to read, as
    /// `Debug` text; they run on the test's thread, in its tick.
    static NOTES: [Cell<Vec<String>>; 2] = const { [Cell::new(Vec::new()), Cell::new(Vec::new())] };
}

/// The notes of listeners.
const SEEN: usize = 0;
/// The notes of monitors.
const WATCHED: usize = 1;

/// Writes down `v` in `notes`.
fn note(notes: usize, v: impl Debug) {
    NOTES.with(|n| {
        let mut list = n[notes].take();
        list.push(format!("{v:?}"));
        n[notes].set(list);
    });
}

/// What was written down in `notes` since the last call.
fn notes(notes: usize) -> Vec<String> {
    NOTES.with(|n| n[notes].take())
}

/// `v` as a note.
fn dbg(v: impl Debug) -> String {
    format!("{v:?}")
}

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
    static CANCELS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
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

/// A click in the air with the held item.
fn use_in_air(h: &mut Harness) {
    let held = h.held();
    let input = h.auth(Vec::new(), Some(air(&held)));
    h.run(vec![input]);
}

const GROUND: BlockPos = BlockPos::new(2, -61, 0);

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

// pmmp: BlockBreakEvent::setInstaBreak
#[test]
fn insta_break_skips_the_break_time() {
    let dirt = Kind::Dirt.default_state();
    for (listeners, broken) in [
        (Listeners::default(), false),
        (
            listen(|e: &mut BlockBreak, _: &mut Context<'_>| {
                assert!(!e.insta_break());
                e.set_insta_break(true);
            }),
            true,
        ),
    ] {
        let mut h = Harness::with(|_| {}, listeners);
        h.set(TARGET, dirt);
        h.act(vec![action(
            PlayerAction::PredictDestroyBlock,
            TARGET,
            Face::West,
        )]);
        assert_eq!(h.block(TARGET) == block::AIR, broken);
    }
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
        assert!(!door_open(&h));
        assert_eq!(h.block(BlockPos::new(1, -60, 0)), block::AIR);
        assert_eq!(h.held().count(), 5);
        h.state.hash()
    };
    let cancelled = run(cancel(PlayerInteract::cancel), false);
    assert_eq!(cancels(), 1);
    assert_eq!(cancelled, run(Listeners::default(), true));
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
    assert_eq!(sign_at(&h).1, super::super::super::block_entity::BLACK_TEXT);
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
                e.is_front_face(),
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
        [dbg((me(), TARGET, empty.clone(), hello, true))]
    );
    assert_eq!(front_lines(&h), ["a", "b", "§cc", "d"]);
    assert_eq!(h.player().breaking.sign_editor, None);
    // Cancelled, or text that fails the checks: the old text stays and the client gets it back.
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
        assert!(updates(&h.packets()).contains(&TARGET));
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
    assert_eq!(sign_at(&h).1, super::super::super::block_entity::BLACK_TEXT);
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
    assert_ne!(sign_at(&h).1, super::super::super::block_entity::BLACK_TEXT);
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
    let offers = super::super::super::trade::Offers::from_nbt(
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

/// The player right-clicks the entity `runtime_id` at `at`.
fn use_on_entity(h: &mut Harness, runtime_id: u64, at: [f32; 3]) {
    let input = h.input(Packet::InventoryTransaction(InventoryTransaction {
        transaction_data: InventoryTransactionData::UseItemOnEntity(Box::new(
            UseItemOnEntityTransactionData {
                target_entity_runtime_id: runtime_id,
                action_type: UseItemOnEntityAction::Interact.into(),
                clicked_position: at,
                ..Default::default()
            },
        )),
        ..Default::default()
    }));
    h.run(vec![input]);
}

fn trade_opened(h: &mut Harness) -> bool {
    h.packets()
        .iter()
        .any(|p| matches!(p, Packet::UpdateTrade(_)))
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
    super::super::super::dispatch::run_deferred(&mut h.state, &mut runs);
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
                // The break time would refuse a break of a block a listener took: skip it.
                e.set_insta_break(true);
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
                if meddle(cx, place::step(e.pos(), e.face())) {
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

#[test]
fn listener_text_is_checked_like_the_players() {
    assert_eq!(
        sign::check(&["§ca", "b\u{E000}", "", ""].map(String::from)),
        Some(["§ca", "b", "", ""].map(String::from))
    );
    assert_eq!(sign::check(&["a\nb", "", "", ""].map(String::from)), None);
    let long = [
        "x".repeat(500),
        "x".repeat(501),
        String::new(),
        String::new(),
    ];
    assert_eq!(sign::check(&long), None);
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
