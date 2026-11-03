//! Tick tests of interaction: packets in, state and packets out.

mod book;
mod custom;
mod events;

use std::cell::Cell;
use std::fmt::Debug;

use tokio::sync::mpsc;

use super::*;
use crate::block::Kind;
use crate::config::Config;
use crate::event::Listeners;
use crate::pool::Pool;
use crate::protocol::consts::{InputFlag, UseItemAction};
use crate::protocol::packet::{PlayerAuthInput, SetLocalPlayerAsInitialised};
use crate::protocol::{
    FlagSet, InputFlags, InventoryTransactionData, NormalTransactionData, PlayerBlockAction,
    ReleaseItemTransactionData, UseItemTransactionData,
};
use crate::tick::queue::{Input, InputKind, Message, Outbound, SessionId};
use crate::tick::survival::EYE_HEIGHT;
use crate::tick::tests::{answer, blocks_set, join, leave, new_state, radius, settings};
use crate::tick::{InputLog, Output, Phase, State, boundary, deliver, inventory, step};
use crate::world::Entry;

/// Window opening clicks squeezed into one tick by a client that changes each click a little
/// (so none is a repeat) and acknowledges every close at once.
const CLICKS: usize = 1000;
/// The network ID of dripstone in the 1.26.60 reference list (the value the official server
/// sends), a block without a dump type.
const DRIPSTONE_BLOCK: u32 = 1_289_380_550;
/// Looking east, level.
const EAST: f32 = 270.0;
/// The player's feet: on the flat world's grass at the spawn.
const FEET: [f64; 3] = [0.5, -60.0, 0.5];
/// The network ID of moss in the 1.26.60 reference list (the value the official server sends), a
/// block without a dump type.
const MOSS_BLOCK: u32 = 3_743_819_255;
/// The notes of listeners and hooks.
const SEEN: usize = 0;
/// Two blocks east of the feet, at foot level.
const TARGET: BlockPos = BlockPos::new(2, -60, 0);
/// The notes of monitors.
const WATCHED: usize = 1;

thread_local! {
    /// A CPU pool per test thread; tests do not shut it down.
    static POOL: Pool = crate::runtime::cpu_pool(2).unwrap();
    /// What listeners and hooks ([`SEEN`]) and monitors ([`WATCHED`]) write down for the test to
    /// read, as `Debug` text; they run on the test's thread, in its tick.
    static NOTES: [Cell<Vec<String>>; 2] = const { [Cell::new(Vec::new()), Cell::new(Vec::new())] };
}

/// A server with one spawned player (session 1) and its outbound queue.
struct Harness {
    state: State,
    output: Output,
    rx: mpsc::Receiver<Outbound>,
    seq: u64,
    /// Yaw the next auth inputs carry.
    yaw: f32,
}

impl Harness {
    fn new(edit: impl FnOnce(&mut Config)) -> Self {
        Self::with(edit, Listeners::default())
    }

    fn block(&self, pos: BlockPos) -> block::State {
        self.state.worlds[0].block(pos).unwrap()
    }

    fn held(&self) -> Stack {
        self.player().inventory.held().clone()
    }

    fn player(&self) -> &Player {
        self.state.players.get(&SessionId::new(1)).unwrap()
    }

    /// [`Harness::new`] with event listeners.
    fn with(edit: impl FnOnce(&mut Config), listeners: Listeners) -> Self {
        let state = new_state(settings(edit), listeners);
        let (j, rx) = join(1, 0);
        let mut h = Self {
            state,
            output: Output::default(),
            rx,
            seq: 2,
            yaw: EAST,
        };
        h.run(vec![j, radius(1, 1, 4)]);
        for _ in 0..8 {
            h.run(Vec::new());
        }
        let runtime_id = h.player().runtime_id;
        let init = h.input(Packet::SetLocalPlayerAsInitialised(
            SetLocalPlayerAsInitialised {
                entity_runtime_id: runtime_id,
            },
        ));
        h.run(vec![init]);
        assert_eq!(h.player().phase, Phase::Spawned);
        // Stand at the spawn looking east.
        let stand = h.auth(Vec::new(), None);
        h.run(vec![stand]);
        h.packets();
        h
    }

    /// Runs one tick with one auth input of `actions`.
    fn act(&mut self, actions: Vec<PlayerBlockAction>) {
        let input = self.auth(actions, None);
        self.run(vec![input]);
    }

    /// A `PlayerAuthInput` at the feet, with block actions and an item interaction.
    fn auth(
        &mut self,
        actions: Vec<PlayerBlockAction>,
        item: Option<UseItemTransactionData>,
    ) -> Input {
        let mut flags = FlagSet::<InputFlag>::new();
        if !actions.is_empty() {
            flags.insert(InputFlag::PerformBlockActions);
        }
        if item.is_some() {
            flags.insert(InputFlag::PerformItemInteraction);
        }
        let eye = [FEET[0], FEET[1] + EYE_HEIGHT, FEET[2]];
        let position = eye.map(crate::math::to_f32);
        let p = PlayerAuthInput {
            yaw: self.yaw,
            position,
            input_data: InputFlags::from_set(&flags),
            item_interaction_data: item,
            block_actions: (!actions.is_empty()).then_some(actions),
            ..PlayerAuthInput::default()
        };
        self.input(Packet::PlayerAuthInput(Box::new(p)))
    }

    fn hold(&mut self, stack: Stack) {
        inventory::set_held(self.player_mut(), stack);
    }

    /// The next input of session 1.
    fn input(&mut self, packet: Packet) -> Input {
        let mut bytes = Vec::new();
        packet.encode(&mut bytes).unwrap();
        self.seq += 1;
        Input {
            session: SessionId::new(1),
            seq: self.seq,
            kind: InputKind::Packet {
                packet,
                size: bytes.len(),
            },
        }
    }

    /// Every packet sent to the player since the last call: the delivered ones, then what the
    /// rules queued in its outbox since the last output phase.
    fn packets(&mut self) -> Vec<Packet> {
        let mut out = Vec::new();
        let queued = std::mem::take(&mut self.player_mut().outbox);
        while let Ok(o) = self.rx.try_recv() {
            if let Outbound::Send(messages) = o {
                for m in messages {
                    if let Message::Packet(p) = m {
                        out.push(p);
                    }
                }
            }
        }
        out.extend(queued);
        out
    }

    fn player_mut(&mut self) -> &mut Player {
        self.state.players.get_mut(&SessionId::new(1)).unwrap()
    }

    /// Runs and delivers one tick of `batch`.
    fn run(&mut self, mut batch: Vec<Input>) {
        let mut log = InputLog::default();
        let mut answers = std::mem::take(&mut self.output.answers);
        POOL.with(|pool| {
            step(
                &mut self.state,
                &mut batch,
                &mut answers,
                pool,
                &mut self.output,
                &mut log,
            );
        });
        deliver(&mut self.state, &mut self.output, &mut log);
        boundary(&mut self.state, &mut self.output, &mut log);
        let requests = std::mem::take(&mut self.output.requests);
        self.output.answers = answer(&self.state, requests);
    }

    fn set(&mut self, pos: BlockPos, s: block::State) {
        self.state.worlds[0].set_block(pos, s).unwrap();
    }
}

/// Writes down `v` in `notes`.
fn note(notes: usize, v: impl Debug) {
    NOTES.with(|n| {
        let mut list = n[notes].take();
        list.push(format!("{v:?}"));
        n[notes].set(list);
    });
}

/// Returns what was written down in `notes` since the last call.
fn notes(notes: usize) -> Vec<String> {
    NOTES.with(|n| n[notes].take())
}

/// The `Debug` text of `v`, as a note.
fn dbg(v: impl Debug) -> String {
    format!("{v:?}")
}

fn action(kind: PlayerAction, pos: BlockPos, face: Face) -> PlayerBlockAction {
    PlayerBlockAction {
        action: kind.into(),
        block_pos: [pos.x, pos.y, pos.z],
        face: i32::from(face.index()),
    }
}

/// A click on `face` of the block at `pos` with the held `stack`.
fn click(pos: BlockPos, face: Face, stack: &Stack) -> UseItemTransactionData {
    let mut data = UseItemTransactionData {
        action_type: UseItemAction::ClickBlock.into(),
        block_position: [pos.x, pos.y, pos.z],
        block_face: i32::from(face.index()),
        clicked_position: [0.5, 1.0, 0.5],
        ..UseItemTransactionData::default()
    };
    data.held_item.stack.item_type.network_id =
        stack.item().map_or(0, |t| i32::from(t.runtime_id()));
    data.held_item.stack.count = u16::from(stack.count());
    data
}

/// The positions [`blocks_set`] sets, in order.
fn updates(packets: &[Packet]) -> Vec<BlockPos> {
    blocks_set(packets)
        .into_iter()
        .map(|(pos, _)| pos)
        .collect()
}

fn stack(kind: crate::item::Kind) -> Stack {
    Stack::new(kind.default_type(), 1)
}

fn block_item(kind: Kind, count: u8) -> Stack {
    let block::ItemForm::Item(t) = kind.default_state().item() else {
        panic!("{kind:?} has no item");
    };
    Stack::new(t, count)
}

fn creative(c: &mut Config) {
    c.general.gamemode = GameMode::Creative;
}

#[test]
fn a_survival_break_waits_for_the_break_time() {
    // Glass breaks by hand in 0.45 s (9 ticks) and drops nothing, so no entity is needed.
    let glass = Kind::Glass.default_state();
    let mut h = Harness::new(|_| {});
    h.set(TARGET, glass);
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    assert!(h.player().breaking.target.is_some());
    let started = h.packets();
    assert!(started.iter().any(|p| matches!(p, Packet::LevelEvent(e)
        if e.event_type == i32::from(Event::StartBlockCracking))));
    // Too early: refused, the block stays and the client gets the real blocks back.
    for _ in 0..3 {
        h.act(Vec::new());
    }
    h.packets();
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(h.block(TARGET), glass);
    let sent = h.packets();
    let resynced = updates(&sent);
    assert!(resynced.contains(&TARGET), "{resynced:?}");
    assert!(resynced.contains(&BlockPos::new(2, -59, 0)), "{resynced:?}");
    // Breaking again for the whole time succeeds.
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    for _ in 0..8 {
        h.act(Vec::new());
    }
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(h.block(TARGET), block::AIR);
    assert!(h.player().breaking.target.is_none());
    let sent = h.packets();
    assert!(sent.iter().any(|p| matches!(p, Packet::LevelEvent(e)
        if e.event_type == i32::from(Event::ParticlesDestroyBlock)
            && e.event_data == glass.network_id().cast_signed())));
}

/// `break-time-check = false`: the client's break is taken at once, no progress is counted, and
/// reach still counts.
#[test]
fn without_the_break_time_check_a_break_is_taken_at_once_and_reach_still_counts() {
    let obsidian = Kind::Obsidian.default_state();
    let mut h = Harness::new(|c| c.general.break_time_check = false);
    h.set(TARGET, obsidian);
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    h.act(Vec::new());
    let target = h.player().breaking.target.clone().expect("breaking");
    assert_eq!((target.inputs, target.progress), (0, 0.0), "no progress");
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(h.block(TARGET), block::AIR);
    let far = BlockPos::new(20, -60, 0);
    h.set(far, obsidian);
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        far,
        Face::West,
    )]);
    assert_eq!(h.block(far), obsidian, "out of reach");
}

#[test]
fn creative_breaks_at_once_without_drops() {
    let mut h = Harness::new(creative);
    h.set(TARGET, Kind::Stone.default_state());
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(h.block(TARGET), block::AIR);
}

#[test]
fn adventure_mode_cannot_break_and_far_blocks_are_out_of_reach() {
    let glass = Kind::Glass.default_state();
    let mut h = Harness::new(|c| c.general.gamemode = GameMode::Adventure);
    h.set(TARGET, glass);
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    for _ in 0..12 {
        h.act(Vec::new());
    }
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(h.block(TARGET), glass);
    let mut h = Harness::new(creative);
    let far = BlockPos::new(20, -60, 0);
    h.set(far, glass);
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        far,
        Face::West,
    )]);
    assert_eq!(h.block(far), glass);
}

#[test]
fn a_repeated_break_changes_nothing_twice() {
    let run = |twice: bool| {
        let mut h = Harness::new(creative);
        h.set(TARGET, Kind::Stone.default_state());
        let mut actions = vec![action(
            PlayerAction::PredictDestroyBlock,
            TARGET,
            Face::West,
        )];
        if twice {
            actions.push(action(
                PlayerAction::PredictDestroyBlock,
                TARGET,
                Face::West,
            ));
        }
        h.act(actions);
        h.act(Vec::new());
        h.state.hash()
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn an_action_the_server_does_not_accept_drops_the_whole_packet() {
    let mut h = Harness::new(|_| {});
    h.set(TARGET, Kind::Glass.default_state());
    let bad = PlayerBlockAction {
        action: PlayerAction::StartItemUseOn.into(),
        block_pos: [TARGET.x, TARGET.y, TARGET.z],
        face: 4,
    };
    h.act(vec![
        action(PlayerAction::StartBreak, TARGET, Face::West),
        bad,
    ]);
    assert!(h.player().breaking.target.is_none());
    let invalid_face = PlayerBlockAction {
        face: 9,
        ..action(PlayerAction::StartBreak, TARGET, Face::West)
    };
    h.act(vec![invalid_face]);
    assert!(h.player().breaking.target.is_none());
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    assert!(h.player().breaking.target.is_some());
}

#[test]
fn leaving_drops_the_breaking_state() {
    let run = |breaking: bool| {
        let mut h = Harness::new(|_| {});
        h.set(TARGET, Kind::Glass.default_state());
        if breaking {
            h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
        } else {
            h.act(Vec::new());
        }
        let seq = h.seq + 1;
        h.run(vec![leave(1, seq)]);
        assert!(h.state.players.is_empty());
        h.state.hash()
    };
    assert_eq!(run(true), run(false));
}

#[test]
fn breaking_stops_when_the_player_walks_away() {
    let mut h = Harness::new(|_| {});
    h.set(TARGET, Kind::Glass.default_state());
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    h.player_mut().position = Vec3::new(40.5, -58.38, 0.5);
    h.run(Vec::new());
    assert!(h.player().breaking.target.is_none());
}

/// A placement on top of the grass east of the player.
fn place_on_grass(h: &mut Harness, item: &Stack) -> BlockPos {
    let ground = BlockPos::new(2, -61, 0);
    let data = click(ground, Face::Up, item);
    let input = h.auth(Vec::new(), Some(data));
    h.run(vec![input]);
    BlockPos::new(2, -60, 0)
}

#[test]
fn placing_uses_one_item_in_survival_and_none_in_creative() {
    let mut h = Harness::new(|_| {});
    let planks = block_item(Kind::OakPlanks, 5);
    h.hold(planks.clone());
    let at = place_on_grass(&mut h, &planks);
    assert_eq!(h.block(at).kind(), Kind::OakPlanks);
    assert_eq!(h.held().count(), 4);
    let mut h = Harness::new(creative);
    h.hold(planks.clone());
    let at = place_on_grass(&mut h, &planks);
    assert_eq!(h.block(at).kind(), Kind::OakPlanks);
    assert_eq!(h.held().count(), 5);
}

#[test]
fn a_repeated_click_places_once() {
    let run = |twice: bool| {
        let mut h = Harness::new(|_| {});
        let slab = block_item(Kind::OakSlab, 5);
        h.hold(slab.clone());
        let ground = BlockPos::new(2, -61, 0);
        let data = click(ground, Face::Up, &slab);
        let mut batch = vec![h.auth(Vec::new(), Some(data.clone()))];
        if twice {
            // The client repeats the click; on a bottom slab a second placement would merge.
            batch.push(h.auth(Vec::new(), Some(data)));
        }
        h.run(batch);
        h.run(Vec::new());
        (h.state.hash(), h.held().count())
    };
    let once = run(false);
    assert_eq!(once.1, 4);
    assert_eq!(once, run(true));
}

#[test]
fn a_refused_placement_changes_nothing_and_resyncs_the_clicked_block() {
    let mut h = Harness::new(|_| {});
    let planks = block_item(Kind::OakPlanks, 5);
    h.hold(planks.clone());
    // The block that would be placed is already there: nothing changes.
    let ground = BlockPos::new(0, -61, 0);
    h.set(BlockPos::new(0, -60, 0), Kind::OakPlanks.default_state());
    h.packets();
    let data = click(ground, Face::Up, &planks);
    let input = h.auth(Vec::new(), Some(data));
    h.run(vec![input]);
    assert_eq!(h.held().count(), 5);
    let sent = updates(&h.packets());
    assert!(sent.contains(&ground), "{sent:?}");
    // A held item the client does not have is refused too.
    let wrong = click(
        BlockPos::new(2, -61, 0),
        Face::Up,
        &block_item(Kind::Stone, 1),
    );
    let input = h.auth(Vec::new(), Some(wrong));
    h.run(vec![input]);
    assert_eq!(h.block(BlockPos::new(2, -60, 0)), block::AIR);
    assert_eq!(h.held().count(), 5);
}

/// A refused click sends the real clicked block once with its block.
#[test]
fn a_refused_click_sends_the_real_clicked_block_once() {
    let mut h = Harness::new(|_| {});
    let planks = block_item(Kind::OakPlanks, 5);
    h.hold(planks.clone());
    // The block that would be placed is already there: nothing changes.
    let ground = BlockPos::new(0, -61, 0);
    h.set(BlockPos::new(0, -60, 0), Kind::OakPlanks.default_state());
    h.packets();
    let input = h.auth(Vec::new(), Some(click(ground, Face::Up, &planks)));
    h.run(vec![input]);
    let sent = blocks_set(&h.packets());
    assert_eq!(
        sent.iter()
            .filter(|(p, _)| *p == ground)
            .collect::<Vec<_>>(),
        [&(ground, h.block(ground).network_id())],
        "{sent:?}"
    );
}

/// A refused click sends the blocks around it as the tick leaves them: a block a later input of
/// the same tick places among them is not undone on the client.
#[test]
fn a_refused_click_sends_the_blocks_around_it_as_the_tick_leaves_them() {
    let mut h = Harness::new(|_| {});
    let planks = block_item(Kind::OakPlanks, 5);
    h.hold(planks.clone());
    h.set(BlockPos::new(0, -60, 0), Kind::OakPlanks.default_state());
    h.packets();
    // Refused: the block is already there; (1, -60, 0) is among the blocks sent.
    let refused = click(BlockPos::new(0, -61, 0), Face::Up, &planks);
    let refused = h.auth(Vec::new(), Some(refused));
    let target = BlockPos::new(1, -60, 0);
    let placed = click(BlockPos::new(1, -61, 0), Face::Up, &planks);
    let placed = h.auth(Vec::new(), Some(placed));
    h.run(vec![refused, placed]);
    let block = Kind::OakPlanks.default_state();
    assert_eq!(h.block(target), block);
    let sent = blocks_set(&h.packets());
    let last = sent.iter().rev().find(|(p, _)| *p == target);
    assert_eq!(last, Some(&(target, block.network_id())), "{sent:?}");
}

#[test]
fn a_refused_click_on_a_fake_block_shows_the_fake_block_again() {
    let mut h = Harness::new(|_| {});
    let planks = block_item(Kind::OakPlanks, 5);
    h.hold(planks.clone());
    let ground = BlockPos::new(0, -61, 0);
    let fake = Kind::Diamond.default_state();
    let player = crate::player::Id::new(SessionId::new(1));
    Context {
        state: &mut h.state,
    }
    .send_fake_block(player, ground, fake)
    .unwrap();
    h.run(Vec::new());
    h.packets();
    // The block that would be placed is already there: refused, and the clicked block is resent.
    h.set(BlockPos::new(0, -60, 0), Kind::OakPlanks.default_state());
    let data = click(ground, Face::Up, &planks);
    let input = h.auth(Vec::new(), Some(data));
    h.run(vec![input]);
    assert_eq!(h.held().count(), 5);
    let resent = |h: &mut Harness, pos: BlockPos| {
        blocks_set(&h.packets())
            .into_iter()
            .filter_map(|(at, id)| (at == pos).then_some(id))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        resent(&mut h, ground),
        [fake.network_id()],
        "the fake block"
    );
    // Without the fake block, the real one.
    let cleared = Context {
        state: &mut h.state,
    }
    .clear_fake_block(player, ground);
    assert!(cleared);
    h.run(Vec::new());
    h.packets();
    let data = click(ground, Face::Up, &planks);
    let input = h.auth(Vec::new(), Some(data));
    h.run(vec![input]);
    let real = h.block(ground).network_id();
    assert_eq!(resent(&mut h, ground), [real]);
}

#[test]
fn doors_place_and_break_as_two_blocks() {
    let mut h = Harness::new(creative);
    let door = block_item(Kind::OakDoor, 1);
    h.hold(door.clone());
    let at = place_on_grass(&mut h, &door);
    let top = BlockPos::new(at.x, at.y + 1, at.z);
    assert!(h.block(at).is(Class::Door));
    assert_eq!(props::flag(h.block(top), "upper_block_bit"), Some(true));
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        top,
        Face::West,
    )]);
    assert_eq!(h.block(at), block::AIR);
    assert_eq!(h.block(top), block::AIR);
}

#[test]
fn doors_trapdoors_and_fence_gates_open_and_close() {
    let mut h = Harness::new(|_| {});
    let door = Kind::OakDoor.default_state();
    let top = props::with_flag(door, "upper_block_bit", true).unwrap();
    h.set(TARGET, door);
    h.set(BlockPos::new(2, -59, 0), top);
    h.hold(Stack::EMPTY);
    use_on_target(&mut h);
    assert_eq!(props::flag(h.block(TARGET), "open_bit"), Some(true));
    assert_eq!(
        props::flag(h.block(BlockPos::new(2, -59, 0)), "open_bit"),
        Some(true)
    );
    h.set(BlockPos::new(2, -59, 0), block::AIR);
    for kind in [Kind::OakTrapdoor, Kind::OakFenceGate] {
        h.set(TARGET, kind.default_state());
        // A tick apart, so the same click is not taken for the client repeating itself.
        h.run(Vec::new());
        use_on_target(&mut h);
        assert_eq!(
            props::flag(h.block(TARGET), "open_bit"),
            Some(true),
            "{kind:?}"
        );
        h.run(Vec::new());
        use_on_target(&mut h);
        assert_eq!(
            props::flag(h.block(TARGET), "open_bit"),
            Some(false),
            "{kind:?}"
        );
    }
}

/// A click on a block of a removed kind changes nothing, opens no window and sends the clicked
/// block again; the click event still fires (see the events tests).
#[test]
fn a_click_on_a_removed_block_changes_nothing_and_resends_it() {
    let bed = props::with_flag(Kind::Bed.default_state(), "head_piece_bit", false).unwrap();
    let removed = [
        Kind::Lever,
        Kind::StoneButton,
        Kind::Cake,
        Kind::CraftingTable,
        Kind::Anvil,
        Kind::Furnace,
        Kind::Jukebox,
        Kind::NoteBlock,
        Kind::Bell,
    ];
    let states = removed.into_iter().map(Kind::default_state).chain([bed]);
    for state in states {
        let mut h = Harness::new(|_| {});
        h.state.worlds[0].set_time(13_000);
        h.set(TARGET, state);
        h.hold(Stack::EMPTY);
        h.packets();
        use_on_target(&mut h);
        assert_eq!(h.block(TARGET), state, "{state:?}");
        let sent = h.packets();
        assert!(updates(&sent).contains(&TARGET), "{state:?}");
        assert!(
            !sent.iter().any(|p| matches!(p, Packet::ContainerOpen(_))),
            "{state:?}"
        );
    }
}

/// The use of an item that changed a block or hung an entity changes nothing now: the held item,
/// the block and the entities stay, and the clicked block is sent again.
#[test]
fn a_removed_item_use_changes_nothing_and_resends_the_clicked_block() {
    use crate::item::Kind as ItemKind;
    let ground = BlockPos::new(2, -61, 0);
    let source = block::State::find(
        "minecraft:water",
        &[("liquid_depth", block::PropertyValue::Int(0))],
    )
    .unwrap();
    // The item, the block set at the clicked place (the ground is grass), the place, the face.
    let cases = [
        (ItemKind::WoodenHoe, None, ground, Face::Up),
        (ItemKind::WoodenShovel, None, ground, Face::Up),
        (
            ItemKind::WoodenAxe,
            Some(Kind::OakLog.default_state()),
            TARGET,
            Face::West,
        ),
        (ItemKind::FlintAndSteel, None, ground, Face::Up),
        (ItemKind::FireCharge, None, ground, Face::Up),
        (ItemKind::BoneMeal, None, ground, Face::Up),
        (ItemKind::WaterBucket, None, ground, Face::Up),
        (ItemKind::Bucket, Some(source), TARGET, Face::West),
        (
            ItemKind::Painting,
            Some(Kind::Stone.default_state()),
            TARGET,
            Face::West,
        ),
        (
            ItemKind::EndCrystal,
            Some(Kind::Obsidian.default_state()),
            TARGET,
            Face::Up,
        ),
    ];
    for (kind, block, at, face) in cases {
        let mut h = Harness::new(|_| {});
        if let Some(block) = block {
            h.set(at, block);
        }
        let held = stack(kind);
        h.hold(held.clone());
        let around = |h: &Harness| {
            let above = at.step(Face::Up).unwrap();
            [at, above].map(|pos| h.block(pos))
        };
        let blocks = around(&h);
        let entities = h.state.rules.entities.count(0);
        h.packets();
        let input = h.auth(Vec::new(), Some(click(at, face, &held)));
        h.run(vec![input]);
        h.run(Vec::new());
        assert_eq!(h.held(), held, "{kind:?}");
        assert_eq!(around(&h), blocks, "{kind:?}");
        assert_eq!(h.state.rules.entities.count(0), entities, "{kind:?}");
        assert!(updates(&h.packets()).contains(&at), "{kind:?}");
    }
}

/// `durability = false`: breaking does not wear the tool.
#[test]
fn durability_off_keeps_tools_as_they_are() {
    let worn = |durability: bool| {
        let mut h = Harness::new(|c| {
            c.general.break_time_check = false;
            c.general.durability = durability;
        });
        let pickaxe = stack(crate::item::Kind::WoodenPickaxe);
        h.hold(pickaxe);
        h.set(TARGET, Kind::Stone.default_state());
        h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
        h.act(vec![action(
            PlayerAction::PredictDestroyBlock,
            TARGET,
            Face::West,
        )]);
        assert_eq!(h.block(TARGET), block::AIR);
        h.held().damage()
    };
    assert_eq!(worn(true), 1);
    assert_eq!(worn(false), 0);
}

#[test]
fn placing_a_sign_opens_its_editor_and_only_the_editor_may_write() {
    let mut h = Harness::new(|_| {});
    let sign = stack(crate::item::Kind::OakSign);
    h.hold(sign.clone());
    let at = place_on_grass(&mut h, &sign);
    assert!(h.block(at).is(Class::BaseSign));
    assert!(h.packets().iter().any(|p| matches!(p, Packet::OpenSign(o)
        if o.position == [at.x, at.y, at.z] && o.front_side)));
    assert_eq!(h.player().breaking.sign_editor, Some((0, at)));
    // Another sign is not the player's to write: refused with the real block.
    let other = BlockPos::new(2, -60, 2);
    h.set(other, h.block(at));
    h.run(Vec::new());
    h.packets();
    let mut nbt = crate::nbt::Compound::new();
    for side in ["FrontText", "BackText"] {
        let mut c = crate::nbt::Compound::new();
        c.insert(
            "Text".to_owned(),
            crate::nbt::Value::String("hi".to_owned()),
        );
        nbt.insert(side.to_owned(), crate::nbt::Value::Compound(c));
    }
    let edit = h.input(Packet::BlockActorData(BlockActorData {
        position: [other.x, other.y, other.z],
        nbt_data: nbt,
    }));
    h.run(vec![edit]);
    assert_eq!(updates(&h.packets()), vec![other]);
}

/// A normal transaction that is no Q key drop (here one without actions) changes nothing: the
/// held items stay and nothing drops.
#[test]
fn a_normal_transaction_that_is_no_q_key_drop_changes_nothing() {
    let mut h = Harness::new(|_| {});
    let sticks = stack(crate::item::Kind::Stick).with_count(5);
    h.hold(sticks.clone());
    let entities = h.state.rules.entities.count(0);
    let tx = InventoryTransaction {
        transaction_data: InventoryTransactionData::Normal(NormalTransactionData::default()),
        ..InventoryTransaction::default()
    };
    let input = h.input(Packet::InventoryTransaction(Box::new(tx)));
    h.run(vec![input]);
    assert_eq!(h.held(), sticks);
    assert_eq!(h.state.rules.entities.count(0), entities);
}

/// An explosion that raises no event.
fn quiet_explosion() -> Destroy {
    Destroy::Explosion {
        size: 4.0,
        raise: Raise::Quiet,
    }
}

#[test]
fn destroying_for_each_cause_leaves_air_and_respects_the_entity_cap() {
    let mut h = Harness::new(|_| {});
    // Both halves of a door go, whichever is destroyed.
    let door = Kind::OakDoor.default_state();
    let top = props::with_flag(door, "upper_block_bit", true).unwrap();
    let up = BlockPos::new(2, -59, 0);
    h.set(TARGET, door);
    h.set(up, top);
    assert!(destroy(&mut h.state, 0, TARGET, quiet_explosion()));
    assert_eq!(h.block(TARGET), block::AIR);
    assert_eq!(h.block(up), block::AIR);
    // Ice leaves air, not water; so does an explosion.
    h.set(TARGET, Kind::Ice.default_state());
    assert!(destroy(&mut h.state, 0, TARGET, quiet_explosion()));
    assert_eq!(h.block(TARGET), block::AIR);
    h.set(TARGET, Kind::Stone.default_state());
    assert!(destroy(
        &mut h.state,
        0,
        TARGET,
        Destroy::Explosion {
            size: 4.0,
            raise: Raise::Events
        }
    ));
    assert_eq!(h.block(TARGET), block::AIR);
    // Air is not destroyed again.
    assert!(!destroy(&mut h.state, 0, TARGET, quiet_explosion()));
    // A player's break whose drop finds no room changes nothing: the world is at its entity cap.
    // (The door's drop is one of the entities already.)
    let mut i = 0_u32;
    while entity::has_room(&h.state, 0, 1) {
        let at = Vec3::new(
            f64::from(i % 64) - 32.0,
            -59.0,
            f64::from(i / 64 % 64) - 32.0,
        );
        assert!(entity::drop_item(
            &mut h.state,
            0,
            at,
            stack(crate::item::Kind::Stick)
        ));
        i += 1;
    }
    let tool = Stack::new(crate::item::Kind::WoodenPickaxe.default_type(), 1);
    h.set(TARGET, Kind::Stone.default_state());
    let session = SessionId::new(1);
    assert!(!destroy(
        &mut h.state,
        0,
        TARGET,
        Destroy::Player { session, tool }
    ));
    assert_eq!(h.block(TARGET), Kind::Stone.default_state());
}

/// Clicks the west face of the block at [`TARGET`] with the held stack.
fn use_on_target(h: &mut Harness) {
    let held = h.held();
    let input = h.auth(Vec::new(), Some(click(TARGET, Face::West, &held)));
    h.run(vec![input]);
}

/// Makes a stack of one item of the item name `name`.
fn named(name: &str) -> Stack {
    let t = crate::item::Type::from_name(name, 0, None, &crate::nbt::Compound::new()).unwrap();
    Stack::new(t, 1)
}

/// Sets `block` at [`TARGET`] with its block entity, as placing it would.
fn set_with_entity(h: &mut Harness, block: block::State, item: &Stack) {
    h.set(TARGET, block);
    crate::tick::block_entity::placed(&mut h.state, 0, TARGET, None, item, Face::West);
}

#[test]
fn sneaking_places_instead_of_opening() {
    let mut h = Harness::new(|_| {});
    let door = Kind::OakDoor.default_state();
    h.set(TARGET, door);
    h.set(
        BlockPos::new(2, -59, 0),
        props::with_flag(door, "upper_block_bit", true).unwrap(),
    );
    let planks = block_item(Kind::OakPlanks, 3);
    h.hold(planks.clone());
    let input = {
        let data = click(TARGET, Face::West, &planks);
        // Sneaking stays pressed for this packet.
        let mut flags = FlagSet::<InputFlag>::new();
        flags.insert(InputFlag::Sneaking);
        flags.insert(InputFlag::PerformItemInteraction);
        let p = PlayerAuthInput {
            yaw: EAST,
            position: [0.5, -58.38, 0.5],
            input_data: InputFlags::from_set(&flags),
            item_interaction_data: Some(data),
            ..PlayerAuthInput::default()
        };
        h.input(Packet::PlayerAuthInput(Box::new(p)))
    };
    h.run(vec![input]);
    assert_eq!(props::flag(h.block(TARGET), "open_bit"), Some(false));
    assert_eq!(h.block(BlockPos::new(1, -60, 0)).kind(), Kind::OakPlanks);
}

/// Breaks the glass at [`TARGET`] with `ticks` client ticks between start and predicted break;
/// whether it broke.
fn break_glass_after(ticks: usize) -> bool {
    let glass = Kind::Glass.default_state();
    let mut h = Harness::new(|_| {});
    h.set(TARGET, glass);
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    for _ in 0..ticks {
        h.act(Vec::new());
    }
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    h.block(TARGET) == block::AIR
}

#[test]
fn the_break_time_has_one_tick_of_tolerance() {
    // Glass by hand: 0.45 s, 9 ticks. The predicting packet counts, plus one tick of tolerance.
    assert!(!break_glass_after(6));
    assert!(break_glass_after(7));
}

/// Progress made on one block does not break another put in its place: glass broken long
/// enough, then obsidian there, stays and is sent back.
#[test]
fn break_progress_does_not_carry_over_to_a_new_block() {
    let mut h = Harness::new(|_| {});
    h.set(TARGET, Kind::Glass.default_state());
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    for _ in 0..10 {
        h.act(Vec::new());
    }
    let obsidian = Kind::Obsidian.default_state();
    h.set(TARGET, obsidian);
    h.packets();
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(h.block(TARGET), obsidian);
    assert!(updates(&h.packets()).contains(&TARGET), "resynced");
}

#[test]
fn a_client_opens_a_block_window_at_most_once_a_tick() {
    use crate::protocol::packet::ContainerClose;
    for (source, opens) in [(Source::Client, 1), (Source::Action, CLICKS)] {
        let mut h = Harness::new(|_| {});
        set_with_entity(&mut h, Kind::Chest.default_state(), &Stack::EMPTY);
        h.packets();
        let mut opened = 0;
        for _ in 0..CLICKS {
            let click = Vec3::new(0.0, 0.5, 0.5);
            let _ = interact_block(
                &mut h.state,
                SessionId::new(1),
                TARGET,
                Face::West,
                click,
                source,
            );
            for p in h.packets() {
                match p {
                    Packet::ContainerOpen(_) => opened += 1,
                    Packet::ContainerClose(c) if c.server_side => {
                        let ack = ContainerClose {
                            server_side: false,
                            ..c
                        };
                        inventory::on_container_close(&mut h.state, SessionId::new(1), &ack);
                    }
                    _ => {}
                }
            }
        }
        let last = h.packets();
        opened += last
            .iter()
            .filter(|p| matches!(p, Packet::ContainerOpen(_)))
            .count();
        assert_eq!(opened, opens, "{source:?}");
    }
}

fn entity_at(h: &Harness) -> crate::tick::block_entity::Kind {
    h.state
        .rules
        .block_entities
        .get(0, TARGET)
        .unwrap()
        .kind
        .clone()
}

/// The sign side texts at [`TARGET`]: (front lines, colour, glowing, waxed).
fn sign_at(h: &Harness) -> (Vec<String>, i32, bool, bool) {
    match entity_at(h) {
        crate::tick::block_entity::Kind::Sign(s) => {
            let side = if s.front.lines.iter().any(|l| !l.is_empty())
                || s.back.lines.iter().all(String::is_empty)
            {
                &s.front
            } else {
                &s.back
            };
            (side.lines.to_vec(), side.colour, side.glowing, s.waxed)
        }
        other => panic!("{other:?}"),
    }
}

fn sign_edit(h: &mut Harness, text: &str) {
    let mut nbt = crate::nbt::Compound::new();
    for side in ["FrontText", "BackText"] {
        let mut c = crate::nbt::Compound::new();
        c.insert(
            "Text".to_owned(),
            crate::nbt::Value::String(text.to_owned()),
        );
        nbt.insert(side.to_owned(), crate::nbt::Value::Compound(c));
    }
    let edit = h.input(Packet::BlockActorData(BlockActorData {
        position: [TARGET.x, TARGET.y, TARGET.z],
        nbt_data: nbt,
    }));
    h.run(vec![edit]);
}

#[test]
fn signs_keep_their_text() {
    let mut h = Harness::new(|_| {});
    let sign = stack(crate::item::Kind::OakSign);
    h.hold(sign.clone());
    let at = place_on_grass(&mut h, &sign);
    assert_eq!(at, TARGET);
    sign_edit(&mut h, "hello\nworld");
    let (lines, colour, glowing, waxed) = sign_at(&h);
    assert_eq!(&lines[..2], ["hello", "world"]);
    assert_eq!(
        (colour, glowing, waxed),
        (crate::block_entity::BLACK_TEXT, false, false)
    );
    let sent = h.packets();
    assert!(sent.iter().any(|p| matches!(p, Packet::BlockActorData(d)
        if d.position == [at.x, at.y, at.z] && d.nbt_data.contains_key("FrontText"))));
    // A click with a dye on the sign opens the editor and dyes nothing.
    let red_dye = named("minecraft:red_dye").with_count(2);
    h.hold(red_dye.clone());
    use_on_target(&mut h);
    assert_eq!(h.held(), red_dye);
    assert_eq!(sign_at(&h).1, crate::block_entity::BLACK_TEXT);
    assert_eq!(h.player().breaking.sign_editor, Some((0, TARGET)));
}

// api: Player::openSignEditor
#[test]
fn the_api_opens_a_sign_editor_only_on_a_sign_in_the_game() {
    let mut h = Harness::new(|_| {});
    let sign = stack(crate::item::Kind::OakSign);
    h.hold(sign.clone());
    let at = place_on_grass(&mut h, &sign);
    assert_eq!(at, TARGET);
    sign_edit(&mut h, "first");
    assert_eq!(
        h.player().breaking.sign_editor,
        None,
        "one edit per opening"
    );
    h.packets();
    let id = crate::player::Id::new(SessionId::new(1));
    let mut cx = Context {
        state: &mut h.state,
    };
    let grass = BlockPos::new(5, -61, 5);
    assert!(
        !cx.open_sign_editor(id, grass, crate::block_entity::Side::Front),
        "not a sign"
    );
    let stranger = crate::player::Id::new(SessionId::new(9));
    assert!(!cx.open_sign_editor(stranger, at, crate::block_entity::Side::Front));
    assert!(cx.open_sign_editor(id, at, crate::block_entity::Side::Back));
    assert_eq!(h.player().breaking.sign_editor, Some((0, at)));
    h.run(Vec::new());
    assert!(h.packets().iter().any(|p| matches!(p, Packet::OpenSign(o)
        if o.position == [at.x, at.y, at.z] && !o.front_side)));
    sign_edit(&mut h, "second");
    assert_eq!(sign_at(&h).0[0], "second");
}

// api: Player::openSignEditor
/// A sign set by code, which the block updates gave its block entity, takes the edit of an
/// editor the API opens; a dead player, who could send no edit, gets no editor.
#[test]
fn a_sign_set_by_code_takes_the_edit_of_an_editor_the_api_opens() {
    let mut h = Harness::new(|_| {});
    h.set(TARGET, Kind::OakSign.default_state());
    h.run(Vec::new());
    assert!(h.state.rules.block_entities.get(0, TARGET).is_some());
    let id = crate::player::Id::new(SessionId::new(1));
    let mut cx = Context {
        state: &mut h.state,
    };
    assert!(cx.open_sign_editor(id, TARGET, crate::block_entity::Side::Front));
    sign_edit(&mut h, "by code");
    assert_eq!(sign_at(&h).0[0], "by code");
    assert!(h.player_mut().kill());
    h.run(Vec::new());
    let mut cx = Context {
        state: &mut h.state,
    };
    assert!(!cx.open_sign_editor(id, TARGET, crate::block_entity::Side::Front));
}

#[test]
fn the_q_key_drops_from_the_held_slot_when_the_client_stack_matches() {
    let mut h = Harness::new(|_| {});
    let sticks = stack(crate::item::Kind::Stick).with_count(5);
    h.hold(sticks.clone());
    let slot = h.player().inventory.held_slot();
    let old = crate::protocol::ItemInstance {
        stack: sticks.to_network(),
        ..crate::protocol::ItemInstance::default()
    };
    let new = crate::protocol::ItemInstance {
        stack: sticks.with_count(2).to_network(),
        ..crate::protocol::ItemInstance::default()
    };
    let tx = InventoryTransaction {
        actions: vec![
            crate::protocol::InventoryAction {
                source_type: 2,
                window_id: None,
                source_flags: Some(0),
                inventory_slot: 0,
                old_item: crate::protocol::ItemInstance::default(),
                new_item: new,
            },
            crate::protocol::InventoryAction {
                source_type: 0,
                window_id: Some(0),
                source_flags: None,
                inventory_slot: u32::from(slot),
                old_item: old,
                new_item: crate::protocol::ItemInstance::default(),
            },
        ],
        transaction_data: InventoryTransactionData::Normal(NormalTransactionData::default()),
        ..InventoryTransaction::default()
    };
    let input = h.input(Packet::InventoryTransaction(Box::new(tx)));
    h.run(vec![input]);
    assert_eq!(h.held().count(), 3);
}

/// The client took the items away as it sent the drop: one that names another stack than the
/// server's drops nothing and sends the slot back.
#[test]
fn the_q_key_with_another_client_stack_sends_the_slot_back() {
    let mut h = Harness::new(|_| {});
    let sticks = stack(crate::item::Kind::Stick).with_count(5);
    h.hold(sticks.clone());
    let slot = h.player().inventory.held_slot();
    h.packets();
    let drop = crate::protocol::InventoryAction {
        source_type: 2,
        window_id: None,
        source_flags: Some(0),
        inventory_slot: 0,
        old_item: crate::protocol::ItemInstance::default(),
        new_item: sticks.with_count(1).to_instance(0),
    };
    let held = crate::protocol::InventoryAction {
        source_type: 0,
        window_id: Some(0),
        source_flags: None,
        inventory_slot: u32::from(slot),
        old_item: sticks.with_count(4).to_instance(0),
        new_item: sticks.with_count(3).to_instance(0),
    };
    let tx = InventoryTransaction {
        actions: vec![drop, held],
        transaction_data: InventoryTransactionData::Normal(NormalTransactionData::default()),
        ..InventoryTransaction::default()
    };
    let input = h.input(Packet::InventoryTransaction(Box::new(tx)));
    h.run(vec![input]);
    assert_eq!(h.held().count(), 5);
    let sent: Vec<_> = h
        .packets()
        .into_iter()
        .filter_map(|p| match p {
            Packet::InventorySlot(s) => Some((s.window_id, s.slot, s.new_item.stack.count)),
            _ => None,
        })
        .collect();
    assert_eq!(sent, [(0, u32::from(slot), 5)]);
}

#[test]
fn breaking_a_bed_or_banner_drops_its_block_entity_item() {
    let mut h = Harness::new(|_| {});
    let blue =
        crate::item::Type::from_name("minecraft:bed", 11, None, &crate::nbt::Compound::new())
            .unwrap();
    h.hold(Stack::new(blue, 1));
    let at = place_on_grass(&mut h, &Stack::new(blue, 1));
    assert!(h.block(at).is(Class::Bed));
    assert_eq!(
        crate::tick::block_entity::own_item(&h.state, 0, at),
        Some(Stack::new(blue, 1))
    );
}

/// Breaking reads the survival rules' effects: with Haste II glass breaks after 3 client ticks
/// instead of 7; and the block's own break time counts (shears break leaves at once).
#[test]
fn breaking_counts_haste_and_the_blocks_own_break_time() {
    let glass = Kind::Glass.default_state();
    let mut h = Harness::new(|_| {});
    let haste = crate::effect::Instance::new(crate::effect::Type::Haste, 600, 1, true, false);
    assert!(survival::add_effect(&mut h.state, SessionId::new(1), haste));
    h.set(TARGET, glass);
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    for _ in 0..3 {
        h.act(Vec::new());
    }
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(h.block(TARGET), block::AIR, "Haste II breaks it sooner");
    assert!(!break_glass_after(3), "without Haste it takes longer");
    let mut h = Harness::new(|_| {});
    h.set(TARGET, Kind::OakLeaves.default_state());
    h.hold(stack(crate::item::Kind::Shears));
    h.act(vec![
        action(PlayerAction::StartBreak, TARGET, Face::West),
        action(PlayerAction::PredictDestroyBlock, TARGET, Face::West),
    ]);
    assert_eq!(h.block(TARGET), block::AIR, "shears break leaves at once");
}

/// A teleport ends the breaking in progress and the viewers see the cracking stop.
#[test]
fn a_teleport_stops_breaking() {
    let mut h = Harness::new(|_| {});
    h.set(TARGET, Kind::Glass.default_state());
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    assert!(h.player().breaking.target.is_some());
    h.packets();
    survival::teleport(
        &mut h.state,
        SessionId::new(1),
        0,
        Vec3::new(0.5, -58.38, 3.5),
    );
    assert!(h.player().breaking.target.is_none());
    let stopped = h.packets().iter().any(|p| {
        matches!(p, Packet::LevelEvent(e)
        if e.event_type == i32::from(Event::StopBlockCracking))
    });
    assert!(stopped);
}

/// A use in the air with the held `stack`.
fn air(stack: &Stack) -> UseItemTransactionData {
    let mut data = UseItemTransactionData {
        action_type: UseItemAction::ClickAir.into(),
        ..UseItemTransactionData::default()
    };
    data.held_item.stack.item_type.network_id =
        stack.item().map_or(0, |t| i32::from(t.runtime_id()));
    data.held_item.stack.count = u16::from(stack.count());
    data
}

/// Ender pearls wait for their cooldown: a throw starts it and tells the client, a throw before
/// it ends does nothing.
#[test]
fn ender_pearls_wait_for_their_cooldown() {
    let mut h = Harness::new(|_| {});
    h.hold(stack(crate::item::Kind::EnderPearl).with_count(4));
    let throw = |h: &mut Harness| {
        let held = h.held();
        let input = h.auth(Vec::new(), Some(air(&held)));
        h.run(vec![input]);
    };
    throw(&mut h);
    assert_eq!(h.held().count(), 3);
    let told = h.packets().iter().any(|p| {
        matches!(p,
        Packet::ClientStartItemCooldown(c)
            if c.category == "minecraft:ender_pearl" && c.duration == 20)
    });
    assert!(told, "the client hears the cooldown");
    h.run(Vec::new());
    throw(&mut h);
    assert_eq!(h.held().count(), 3, "within the cooldown");
    for _ in 0..20 {
        h.run(Vec::new());
    }
    throw(&mut h);
    assert_eq!(h.held().count(), 2);
}

/// An explosion breaks both halves of a door at once and drops it at most once, without the
/// per-block break particles.
#[test]
fn explosions_break_both_door_halves_once_without_break_particles() {
    let mut h = Harness::new(|_| {});
    let door = Kind::OakDoor.default_state();
    let top = props::with_flag(door, "upper_block_bit", true).unwrap();
    let up = BlockPos::new(2, -59, 0);
    h.set(TARGET, door);
    h.set(up, top);
    h.run(Vec::new());
    h.packets();
    let explosion = Destroy::Explosion {
        size: 1.0,
        raise: Raise::Events,
    };
    assert!(destroy(&mut h.state, 0, up, explosion.clone()));
    assert_eq!((h.block(TARGET), h.block(up)), (block::AIR, block::AIR));
    assert!(!destroy(&mut h.state, 0, TARGET, explosion));
    h.run(Vec::new());
    let sent = h.packets();
    let particles = sent.iter().any(|p| {
        matches!(p, Packet::LevelEvent(e)
        if e.event_type == i32::from(Event::ParticlesDestroyBlock))
    });
    assert!(!particles, "no break particles");
    let drops = sent
        .iter()
        .filter(|p| matches!(p, Packet::AddItemActor(_)))
        .count();
    assert_eq!(drops, 1, "one door");
}

/// Selecting another hotbar slot stops drawing: a bow or trident drawn, then put away and taken
/// back, is not still drawn.
#[test]
fn switching_the_held_slot_cancels_a_drawn_bow() {
    for weapon in [crate::item::Kind::Bow, crate::item::Kind::Trident] {
        let mut h = Harness::new(|_| {});
        let held = stack(weapon);
        h.hold(held.clone());
        assert!(inventory::give(h.player_mut(), stack(crate::item::Kind::Arrow)).is_empty());
        let draw = h.auth(Vec::new(), Some(air(&held)));
        h.run(vec![draw]);
        assert!(
            h.player().breaking.using_since.is_some(),
            "{weapon:?} drawn"
        );
        let equip = |slot: u8| {
            Packet::MobEquipment(crate::protocol::packet::MobEquipment {
                entity_runtime_id: 1,
                new_item: crate::protocol::ItemInstance::default(),
                inventory_slot: slot,
                hot_bar_slot: slot,
                window_id: 0,
            })
        };
        let away = h.input(equip(1));
        h.run(vec![away]);
        let back = h.input(equip(0));
        h.run(vec![back]);
        assert_eq!(h.player().inventory.held_slot(), 0);
        assert!(h.player().breaking.using_since.is_none(), "{weapon:?}");
    }
}

/// Selecting another hotbar slot, moving items or a change of the held stack stops eating, even
/// when the held slot then holds the same food.
#[test]
fn switching_the_held_slot_or_moving_items_stops_eating() {
    let apple = stack(crate::item::Kind::GoldenApple);
    let mut h = Harness::new(|_| {});
    h.hold(apple.clone());
    inventory::set_main(h.player_mut(), 1, apple.clone());
    let eat = h.auth(Vec::new(), Some(air(&apple)));
    h.run(vec![eat]);
    assert!(h.player().is_using_item());
    let away = h.input(Packet::MobEquipment(
        crate::protocol::packet::MobEquipment {
            entity_runtime_id: 1,
            new_item: crate::protocol::ItemInstance::default(),
            inventory_slot: 1,
            hot_bar_slot: 1,
            window_id: 0,
        },
    ));
    h.run(vec![away]);
    assert_eq!(h.player().inventory.held_slot(), 1);
    assert!(!h.player().is_using_item(), "a hotbar switch");
    let mut click = air(&apple);
    click.hot_bar_slot = 1;
    let eat = h.auth(Vec::new(), Some(click.clone()));
    h.run(vec![eat]);
    assert!(h.player().is_using_item());
    let request = h.input(Packet::ItemStackRequest(
        crate::protocol::packet::ItemStackRequest {
            requests: vec![crate::protocol::ItemStackRequest::default()],
        },
    ));
    h.run(vec![request]);
    assert!(!h.player().is_using_item(), "an item stack request");
    let eat = h.auth(Vec::new(), Some(click.clone()));
    h.run(vec![eat]);
    assert!(h.player().is_using_item());
    h.hold(apple);
    assert!(!h.player().is_using_item(), "a held stack change");
    let eat = h.auth(Vec::new(), Some(click));
    h.run(vec![eat]);
    assert!(h.player().is_using_item());
    h.player_mut()
        .inventory_mut(crate::inventory::Kind::Main)
        .set(1, Stack::EMPTY)
        .unwrap();
    assert!(
        !h.player().is_using_item(),
        "an inventory API write of the held slot"
    );
}

#[test]
fn dying_stops_a_drawn_bow() {
    let mut h = Harness::new(|_| {});
    let held = stack(crate::item::Kind::Bow);
    h.hold(held.clone());
    assert!(inventory::give(h.player_mut(), stack(crate::item::Kind::Arrow)).is_empty());
    let draw = h.auth(Vec::new(), Some(air(&held)));
    h.run(vec![draw]);
    assert!(h.player().is_using_item());
    assert!(h.player_mut().kill());
    h.run(Vec::new());
    assert!(!h.player().is_using_item());
}

#[test]
fn dying_stops_the_breaking_viewers_see() {
    let mut h = Harness::new(|_| {});
    h.set(TARGET, Kind::Glass.default_state());
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    h.packets();
    assert!(h.player_mut().kill());
    h.run(Vec::new());
    assert!(h.player().breaking.target.is_none());
    assert!(h.packets().iter().any(|p| matches!(p, Packet::LevelEvent(e)
        if e.event_type == i32::from(Event::StopBlockCracking))));
}

/// Every block action ends the use of the held item, an ignored repeat start too.
#[test]
fn a_block_action_stops_a_drawn_bow() {
    let mut h = Harness::new(|_| {});
    h.set(TARGET, Kind::Glass.default_state());
    let held = stack(crate::item::Kind::Bow);
    h.hold(held.clone());
    assert!(inventory::give(h.player_mut(), stack(crate::item::Kind::Arrow)).is_empty());
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    let draw = h.auth(Vec::new(), Some(air(&held)));
    h.run(vec![draw]);
    assert!(h.player().is_using_item());
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    assert!(!h.player().is_using_item(), "a repeat start");
    let draw = h.auth(Vec::new(), Some(air(&held)));
    h.run(vec![draw]);
    assert!(h.player().is_using_item());
    h.act(vec![action(PlayerAction::AbortBreak, TARGET, Face::West)]);
    assert!(!h.player().is_using_item(), "an abort");
}

/// After a death the client starts breaking anew: the block last attacked is forgotten.
#[test]
fn dying_forgets_the_block_last_attacked() {
    let mut h = Harness::new(|_| {});
    h.set(TARGET, Kind::Glass.default_state());
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    assert_eq!(h.player().breaking.last_attacked, Some(TARGET));
    assert!(h.player_mut().kill());
    h.run(Vec::new());
    assert_eq!(h.player().breaking.last_attacked, None);
}

#[test]
fn a_dead_player_neither_breaks_nor_places() {
    let mut h = Harness::new(|_| {});
    h.set(TARGET, Kind::Glass.default_state());
    assert!(h.player_mut().kill());
    h.run(Vec::new());
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    assert!(h.player().breaking.target.is_none());
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(h.block(TARGET), Kind::Glass.default_state());
    h.set(TARGET, block::AIR);
    let planks = block_item(Kind::OakPlanks, 5);
    h.hold(planks.clone());
    let at = place_on_grass(&mut h, &planks);
    assert_eq!(h.block(at), block::AIR);
}

/// The client starts the use of a bow, trident, goat horn or spyglass on its own and stops it
/// unless the server sends the `UsingItem` entity flag (P6 `itemuse`: the official server sends
/// it): the flag is on while in use and off once released.
#[test]
fn a_used_bow_trident_horn_or_spyglass_shows_the_using_item_flag() {
    let flag = |packets: &[Packet]| {
        packets.iter().rev().find_map(|p| match p {
            Packet::SetActorData(d) if d.entity_runtime_id == 1 => d
                .entity_metadata
                .get(&crate::protocol::consts::EntityDataKey::Flags.into())
                .map(|v| {
                    let bit = 1 << u32::from(crate::protocol::consts::EntityDataFlag::UsingItem);
                    matches!(v, crate::protocol::MetadataValue::Int64(f) if f & bit != 0)
                }),
            _ => None,
        })
    };
    let used = [
        crate::item::Kind::Bow,
        crate::item::Kind::Trident,
        crate::item::Kind::GoatHorn,
        crate::item::Kind::Spyglass,
    ];
    for weapon in used {
        let mut h = Harness::new(|_| {});
        let held = stack(weapon);
        h.hold(held.clone());
        assert!(inventory::give(h.player_mut(), stack(crate::item::Kind::Arrow)).is_empty());
        h.run(Vec::new());
        h.packets();
        let draw = h.input(Packet::InventoryTransaction(Box::new(
            InventoryTransaction {
                transaction_data: InventoryTransactionData::UseItem(Box::new(air(&held))),
                ..InventoryTransaction::default()
            },
        )));
        h.run(vec![draw]);
        assert_eq!(flag(&h.packets()), Some(true), "{weapon:?} drawn");
        assert!(h.player().is_using_item());
        for _ in 0..20 {
            h.run(Vec::new());
        }
        let release = h.input(Packet::InventoryTransaction(Box::new(
            InventoryTransaction {
                transaction_data: InventoryTransactionData::ReleaseItem(
                    ReleaseItemTransactionData::default(),
                ),
                ..InventoryTransaction::default()
            },
        )));
        h.run(vec![release]);
        assert_eq!(flag(&h.packets()), Some(false), "{weapon:?} released");
        assert!(!h.player().is_using_item());
    }
}

/// An explosion at the world's entity limit breaks a chest: its items wait as a spill in the
/// chunk instead of being lost.
#[test]
fn explosion_at_the_entity_cap_keeps_chest_items_as_spill() {
    let mut h = Harness::new(|_| {});
    set_with_entity(&mut h, Kind::Chest.default_state(), &Stack::EMPTY);
    let sword = stack(crate::item::Kind::DiamondSword);
    h.state
        .rules
        .block_entities
        .get_mut(0, TARGET)
        .unwrap()
        .items[0] = sword.clone();
    for i in 0..u32::try_from(entity::MAX_ENTITIES_PER_WORLD).unwrap() {
        let at = Vec3::new(
            f64::from(i % 64) - 32.0,
            -59.0,
            f64::from(i / 64 % 64) - 32.0,
        );
        assert!(entity::drop_item(
            &mut h.state,
            0,
            at,
            stack(crate::item::Kind::Stick)
        ));
    }
    assert!(destroy(
        &mut h.state,
        0,
        TARGET,
        Destroy::Explosion {
            size: 1.0,
            raise: Raise::Events
        }
    ));
    assert_eq!(h.block(TARGET), block::AIR);
    let kept: Vec<Stack> = h.state.rules.block_entities.all_items().cloned().collect();
    assert!(kept.contains(&sword), "{kept:?}");
}

/// With the world at its entity limit and the chunk's spills full, a break, an explosion and a
/// replaced block leave the chest and its items as they were.
#[test]
fn a_chest_stays_when_its_items_have_nowhere_to_wait() {
    let mut h = Harness::new(|_| {});
    set_with_entity(&mut h, Kind::Chest.default_state(), &Stack::EMPTY);
    let sword = stack(crate::item::Kind::DiamondSword);
    let stick = stack(crate::item::Kind::Stick);
    h.state
        .rules
        .block_entities
        .get_mut(0, TARGET)
        .unwrap()
        .items[0] = sword.clone();
    h.state.rules.block_entities.fill_spills(0, TARGET, &stick);
    let mut i = 0_u32;
    while entity::has_room(&h.state, 0, 1) {
        let at = Vec3::new(
            f64::from(i % 64) - 32.0,
            -59.0,
            f64::from(i / 64 % 64) - 32.0,
        );
        assert!(entity::drop_item(&mut h.state, 0, at, stick.clone()));
        i += 1;
    }
    let total = |h: &Harness| {
        h.state
            .rules
            .block_entities
            .all_items()
            .map(|s| usize::from(s.count()))
            .sum::<usize>()
    };
    let before = total(&h);
    let session = SessionId::new(1);
    let tool = Stack::new(crate::item::Kind::WoodenPickaxe.default_type(), 1);
    let causes = [
        Destroy::Player { session, tool },
        Destroy::Explosion {
            size: 1.0,
            raise: Raise::Events,
        },
    ];
    for cause in causes {
        assert!(!destroy(&mut h.state, 0, TARGET, cause));
        assert_eq!(h.block(TARGET), Kind::Chest.default_state());
        assert_eq!(total(&h), before);
    }
    h.set(TARGET, Kind::Stone.default_state());
    crate::tick::block_entity::block_changed(&mut h.state, 0, TARGET);
    assert!(h.state.rules.block_entities.get(0, TARGET).is_some());
    assert_eq!(total(&h), before);
}

/// A block goes where the game sent it, even into a player standing there.
#[test]
fn a_block_is_placed_into_a_standing_player() {
    let mut h = Harness::new(|_| {});
    let planks = block_item(Kind::OakPlanks, 5);
    h.hold(planks.clone());
    // The player stands at the feet position (0.5, -60, 0.5).
    let ground = BlockPos::new(0, -61, 0);
    let input = h.auth(Vec::new(), Some(click(ground, Face::Up, &planks)));
    h.run(vec![input]);
    assert_eq!(h.block(BlockPos::new(0, -60, 0)).kind(), Kind::OakPlanks);
    assert_eq!(h.held().count(), 4);
}

#[test]
fn spawn_eggs_hatch_a_zombie_or_a_villager_and_are_used_up() {
    let mut h = Harness::new(|_| {});
    let named = |kind: crate::item::Kind, name: &str| {
        let extra = crate::item::Extra {
            custom_name: (!name.is_empty()).then(|| name.to_owned()),
            ..crate::item::Extra::default()
        };
        Stack::new(kind.default_type(), 2)
            .with_extra(Some(extra))
            .unwrap()
    };
    let ground = BlockPos::new(2, -61, 0);
    let mut hatched = Vec::new();
    for (egg, name) in [
        (crate::item::Kind::ZombieSpawnEgg, "Bob"),
        (crate::item::Kind::VillagerSpawnEgg, ""),
        (crate::item::Kind::SquidSpawnEgg, ""),
    ] {
        h.hold(named(egg, name));
        let held = h.held();
        let input = h.auth(Vec::new(), Some(click(ground, Face::Up, &held)));
        h.run(vec![input]);
        for _ in 0..3 {
            h.run(Vec::new());
        }
        hatched.push(h.held().count());
    }
    assert_eq!(hatched, [1, 1, 2], "a squid's egg does nothing");
    let cx = Context {
        state: &mut h.state,
    };
    let mut found: Vec<_> = cx
        .entities(crate::world::Id::DEFAULT)
        .map(|(_, e)| {
            (
                e.entity_type(),
                e.position(),
                e.name_tag().to_owned(),
                e.is_invulnerable(),
                e.profession(),
            )
        })
        .collect();
    found.sort_by_key(|f| f.2.clone());
    let middle = Vec3::new(2.5, -60.0, 0.5);
    assert_eq!(
        found,
        [
            (
                crate::entity::Type::Villager,
                middle,
                String::new(),
                false,
                Some(0)
            ),
            (
                crate::entity::Type::Zombie,
                middle,
                "Bob".to_owned(),
                false,
                None
            ),
        ]
    );
}

/// A villager from a spawn egg or from the API without offers has an empty trade list (the vanilla
/// villager economy is out of scope, so there are no trade templates) and opens no trade window;
/// offers set through the trade API then trade as usual.
#[test]
fn villagers_hatched_or_spawned_without_offers_have_an_empty_trade_list() {
    let mut h = Harness::new(|_| {});
    let egg = Stack::new(crate::item::Kind::VillagerSpawnEgg.default_type(), 1);
    h.hold(egg.clone());
    let input = h.auth(
        Vec::new(),
        Some(click(BlockPos::new(2, -61, 0), Face::Up, &egg)),
    );
    h.run(vec![input]);
    h.run(Vec::new());
    let mut cx = Context {
        state: &mut h.state,
    };
    let spawned = cx
        .spawn_villager(
            crate::world::Id::DEFAULT,
            Vec3::new(4.5, -60.0, 4.5),
            0,
            &[],
        )
        .unwrap();
    let villagers: Vec<_> = cx
        .entities(crate::world::Id::DEFAULT)
        .filter(|(_, e)| e.entity_type() == crate::entity::Type::Villager)
        .map(|(id, _)| id)
        .collect();
    assert_eq!(villagers.len(), 2, "the hatched one and the spawned one");
    assert!(villagers.contains(&spawned));
    for v in &villagers {
        assert_eq!(cx.trade_offers(*v), Some(Vec::new()));
    }
    let session = SessionId::new(1);
    for v in &villagers {
        assert!(!crate::tick::trade::open(&mut h.state, session, *v));
    }
    let offer = crate::custom::Offer::new(
        Stack::new(crate::item::Kind::Emerald.default_type(), 1),
        Stack::new(crate::item::Kind::Apple.default_type(), 1),
        4,
    );
    let mut cx = Context {
        state: &mut h.state,
    };
    assert!(cx.set_trade_offers(spawned, &[offer]).is_ok());
    assert_eq!(cx.trade_offers(spawned).map(|o| o.len()), Some(1));
}

/// A stored block state of `name` with no properties, as a server file holds it.
fn stored(name: &str) -> crate::nbt::Compound {
    use crate::nbt::{Compound, Value};
    Compound::from([
        ("name".to_owned(), Value::String(name.to_owned())),
        ("states".to_owned(), Value::Compound(Compound::new())),
        (
            "version".to_owned(),
            Value::Int(crate::import::BLOCK_STATE_VERSION),
        ),
    ])
}

/// Whether `packets` set `pos` to the block of network ID `id`.
fn sets(packets: &[Packet], pos: BlockPos, id: u32) -> bool {
    blocks_set(packets).contains(&(pos, id))
}

/// The block of `name` with its default properties, one without a dump type.
fn without_dump_type(name: &str) -> block::State {
    let state = block::State::find(name, &[]).unwrap();
    assert_eq!(state.kind(), Kind::InfoUpdate, "{name}");
    state
}

/// A block without a dump type breaks like a block of its hardness: dripstone by hand in
/// 1.5 × 1.5 s, 45 ticks. Too early, the client gets it back as itself; in time it becomes air,
/// the viewers see it break, and it drops nothing.
#[test]
fn a_block_without_a_dump_type_breaks_in_its_time_into_air_with_no_drops() {
    let mut h = Harness::new(|_| {});
    let dripstone = without_dump_type("minecraft:dripstone_block");
    h.set(TARGET, dripstone);
    assert_eq!(dripstone.network_id(), DRIPSTONE_BLOCK);
    let entities = h.state.rules.entities.count(0);
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    assert!(h.player().breaking.target.is_some());
    for _ in 0..30 {
        h.act(Vec::new());
    }
    h.packets();
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(h.block(TARGET), dripstone);
    assert!(
        sets(&h.packets(), TARGET, DRIPSTONE_BLOCK),
        "resent as itself"
    );
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    for _ in 0..44 {
        h.act(Vec::new());
    }
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(h.block(TARGET), block::AIR);
    let sent = h.packets();
    assert!(sent.iter().any(|p| matches!(p, Packet::LevelEvent(e)
        if e.event_type == i32::from(Event::ParticlesDestroyBlock)
            && e.event_data == DRIPSTONE_BLOCK.cast_signed())));
    assert_eq!(h.state.rules.entities.count(0), entities, "no drops");
}

/// A creative player breaks a block without a dump type at once; a spectator cannot.
#[test]
fn creative_breaks_a_block_without_a_dump_type_at_once_and_a_spectator_cannot() {
    for (mode, broken) in [(GameMode::Creative, true), (GameMode::Spectator, false)] {
        let mut h = Harness::new(|c| c.general.gamemode = mode);
        let moss = without_dump_type("minecraft:moss_block");
        h.set(TARGET, moss);
        h.act(vec![action(
            PlayerAction::PredictDestroyBlock,
            TARGET,
            Face::West,
        )]);
        if broken {
            assert_eq!(h.block(TARGET), block::AIR, "{mode:?}");
        } else {
            assert_eq!(h.block(TARGET), moss, "{mode:?}");
            assert!(sets(&h.packets(), TARGET, MOSS_BLOCK), "resent as itself");
        }
    }
}

/// Breaking goes on only through a state change of the same block: moss swapped for dripstone by
/// code in the middle of a break resets the break.
#[test]
fn swapping_a_block_without_a_dump_type_for_another_resets_the_break() {
    let mut h = Harness::new(|_| {});
    h.set(TARGET, without_dump_type("minecraft:moss_block"));
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    for _ in 0..30 {
        h.act(Vec::new());
    }
    let dripstone = without_dump_type("minecraft:dripstone_block");
    h.set(TARGET, dripstone);
    h.packets();
    h.act(vec![action(
        PlayerAction::PredictDestroyBlock,
        TARGET,
        Face::West,
    )]);
    assert_eq!(h.block(TARGET), dripstone);
    assert!(updates(&h.packets()).contains(&TARGET), "resynced");
}

/// A block goes next to a block without a dump type, which no placement replaces, and a torch
/// needs no support: the block goes into the cell above and the clicked block stays.
#[test]
fn blocks_go_beside_blocks_without_a_dump_type() {
    let above = BlockPos::new(2, -59, 0);
    let cases = [
        ("minecraft:moss_block", Kind::Stone),
        ("minecraft:moss_block", Kind::Torch),
        ("minecraft:moss_carpet", Kind::Torch),
        ("minecraft:moss_carpet", Kind::Stone),
    ];
    for (name, kind) in cases {
        let mut h = Harness::new(|_| {});
        let clicked = without_dump_type(name);
        h.set(TARGET, clicked);
        let item = block_item(kind, 4);
        h.hold(item.clone());
        let input = h.auth(Vec::new(), Some(click(TARGET, Face::Up, &item)));
        h.run(vec![input]);
        assert_eq!(h.block(TARGET), clicked, "{name} stays");
        assert_eq!(h.block(above).kind(), kind, "{name} {kind:?}");
        assert_eq!(h.held().count(), 3);
    }
}

/// Seagrass is not replaced by a placement either.
#[test]
fn seagrass_is_not_replaced_by_a_placement() {
    let mut h = Harness::new(|_| {});
    let seagrass = block::State::find(
        "minecraft:seagrass",
        &[("sea_grass_type", block::PropertyValue::String("default"))],
    )
    .unwrap();
    h.set(TARGET, seagrass);
    let item = block_item(Kind::Stone, 4);
    h.hold(item.clone());
    let input = h.auth(Vec::new(), Some(click(TARGET, Face::Up, &item)));
    h.run(vec![input]);
    assert_eq!(h.block(TARGET), seagrass);
    assert_eq!(h.block(BlockPos::new(2, -59, 0)).kind(), Kind::Stone);
    assert_eq!(h.held().count(), 3);
}

/// A block whose second cell holds a state nobody knows (kept as stored) is not placed: the
/// unknown block stays and the item is kept. A door's top, a bed's head (toward the player's
/// east facing), a tall plant's top and a small dripleaf's top.
#[test]
fn a_second_cell_does_not_replace_an_unknown_block() {
    let ground = BlockPos::new(2, -61, 0);
    let above = BlockPos::new(2, -59, 0);
    let bed = crate::item::Type::from_name("minecraft:bed", 0, None, &crate::nbt::Compound::new())
        .unwrap();
    let cases = [
        (block_item(Kind::OakDoor, 2), above, Kind::Grass),
        (Stack::new(bed, 1), BlockPos::new(3, -60, 0), Kind::Grass),
        (block_item(Kind::Sunflower, 2), above, Kind::Grass),
        (block_item(Kind::SmallDripleaf, 2), above, Kind::Clay),
    ];
    for (item, second, below) in cases {
        let mut h = Harness::new(|_| {});
        h.set(ground, below.default_state());
        let unknown = h.state.worlds[0]
            .put_unknown(second, stored("minecraft:not_a_block"))
            .unwrap();
        assert!(matches!(unknown, Entry::Unknown(_)), "{unknown:?}");
        h.hold(item.clone());
        let input = h.auth(Vec::new(), Some(click(ground, Face::Up, &item)));
        h.run(vec![input]);
        assert_eq!(h.state.worlds[0].entry(second), Some(unknown), "{item:?}");
        assert_eq!(h.block(TARGET), block::AIR, "{item:?}");
        assert_eq!(h.held(), item);
    }
}

/// The feet the interaction rules use are the survival rules' rounded feet: a player standing on
/// the ground (its `f32` eyes a hair below 1.62 above it) has its feet in the block above the
/// ground, not in the ground.
#[test]
fn a_player_on_the_ground_has_its_feet_above_the_ground() {
    let h = Harness::new(|_| {});
    let actor = actor(&h.state, SessionId::new(1)).unwrap();
    assert_eq!(BlockPos::containing(actor.feet), BlockPos::new(0, -60, 0));
}

/// A hotbar slot selected through the API or by a pick stops eating like the client's selection,
/// even with the same food in the new slot.
#[test]
fn selecting_a_slot_through_the_api_or_a_pick_stops_eating() {
    let apple = stack(crate::item::Kind::GoldenApple);
    let mut h = Harness::new(|_| {});
    h.hold(apple.clone());
    inventory::set_main(h.player_mut(), 1, apple.clone());
    let eat = h.auth(Vec::new(), Some(air(&apple)));
    h.run(vec![eat]);
    assert!(h.player().is_using_item());
    h.player_mut().set_held_slot(1).unwrap();
    assert!(!h.player().is_using_item(), "an API hotbar switch");
    let mut click = air(&apple);
    click.hot_bar_slot = 1;
    let eat = h.auth(Vec::new(), Some(click));
    h.run(vec![eat]);
    assert!(h.player().is_using_item());
    inventory::equip_or_add(h.player_mut(), apple);
    assert_eq!(h.player().inventory.held_slot(), 0);
    assert!(!h.player().is_using_item(), "a pick");
}

/// Hitting the block under a fire sends the fire again: the client puts it out at once and the
/// server keeps it.
#[test]
fn hitting_the_block_under_a_fire_sends_the_fire_again() {
    let fire = Kind::Fire.default_state();
    let mut h = Harness::new(|_| {});
    h.set(TARGET, fire);
    h.run(Vec::new());
    h.packets();
    let ground = BlockPos::new(2, -61, 0);
    h.act(vec![action(PlayerAction::StartBreak, ground, Face::Up)]);
    assert_eq!(h.block(TARGET), fire);
    let sent = blocks_set(&h.packets());
    assert!(sent.contains(&(TARGET, fire.network_id())), "{sent:?}");
}

/// A sign edit whose text cleaning turns into the stored text is answered with the sign, so the
/// writer's raw text goes.
#[test]
fn a_sign_edit_that_cleaning_leaves_unchanged_is_answered_with_the_sign() {
    let mut h = Harness::new(|_| {});
    let sign = stack(crate::item::Kind::OakSign);
    h.hold(sign.clone());
    place_on_grass(&mut h, &sign);
    h.packets();
    sign_edit(&mut h, "\u{E123}");
    let sent = h.packets();
    assert!(
        sent.iter().any(|p| matches!(p, Packet::BlockActorData(d)
        if d.position == [TARGET.x, TARGET.y, TARGET.z])),
        "{sent:?}"
    );
}

/// A block placed into water clears the layer 1 water the client moves under it, also in a chunk
/// that was sent without water.
#[test]
fn placing_into_water_clears_the_layer_one_water_of_the_cell() {
    let mut h = Harness::new(|_| {});
    h.set(TARGET, Kind::Water.default_state());
    h.run(Vec::new());
    let fence = block_item(Kind::OakFence, 2);
    h.hold(fence.clone());
    h.packets();
    place_on_grass(&mut h, &fence);
    assert_eq!(h.block(TARGET).kind(), Kind::OakFence);
    let sent = h.packets();
    assert!(
        sent.iter().any(|p| matches!(p, Packet::UpdateBlock(u)
            if u.position == [TARGET.x, TARGET.y, TARGET.z]
                && u.layer == 1
                && u.new_block_runtime_id == Entry::AIR.network_id())),
        "{sent:?}"
    );
}

/// A big dripleaf placed on a big dripleaf head grows: the head below becomes its stem.
#[test]
fn a_big_dripleaf_is_placed_on_a_big_dripleaf_head() {
    let mut h = Harness::new(|_| {});
    let head = Kind::BigDripleafHead.default_state();
    h.set(TARGET, head);
    let leaf = block_item(Kind::BigDripleafHead, 2);
    h.hold(leaf.clone());
    let input = h.auth(Vec::new(), Some(click(TARGET, Face::Up, &leaf)));
    h.run(vec![input]);
    assert_eq!(h.block(TARGET).kind(), Kind::BigDripleafStem);
    assert_eq!(
        h.block(BlockPos::new(2, -59, 0)).kind(),
        Kind::BigDripleafHead
    );
    assert_eq!(h.held().count(), 1);
}
