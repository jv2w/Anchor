//! Tick tests of interaction: packets in, state and packets out (PLAN.md 10.7 judgement).

use tokio::sync::mpsc;

use super::super::queue::{Input, InputKind, Message, Outbound, SessionId};
use super::super::tests::{answer, join, leave, new_state, radius, settings};
use super::super::{InputLog, Output, Phase, State, boundary, deliver, inventory, step};
use super::*;
use crate::block::Kind;
use crate::config::Config;
use crate::event::Listeners;
use crate::pool::Pool;
use crate::protocol::consts::{InputFlag, UseItemAction};
use crate::protocol::packet::{PlayerAuthInput, SetLocalPlayerAsInitialised};
use crate::protocol::{
    FlagSet, InputFlags, InventoryTransactionData, NormalTransactionData, PlayerBlockAction,
    UseItemTransactionData,
};

thread_local! {
    /// A CPU pool per test thread; tests do not shut it down.
    static POOL: Pool = crate::runtime::cpu_pool(2).unwrap();
}

/// The player's feet: on the flat world's grass at the spawn.
const FEET: [f64; 3] = [0.5, -60.0, 0.5];
/// Looking east, level.
const EAST: f32 = 270.0;

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

    /// [`Harness::new`] with event listeners (sorted here, as `build()` does).
    fn with(edit: impl FnOnce(&mut Config), mut listeners: Listeners) -> Self {
        listeners.sort();
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

    fn player(&self) -> &Player {
        self.state.players.get(&SessionId::new(1)).unwrap()
    }

    fn player_mut(&mut self) -> &mut Player {
        self.state.players.get_mut(&SessionId::new(1)).unwrap()
    }

    /// The next input of session 1.
    fn input(&mut self, packet: Packet) -> Input {
        let mut bytes = Vec::new();
        packet.encode(&mut bytes, 0).unwrap();
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
        #[expect(
            clippy::cast_possible_truncation,
            reason = "invariant: test coordinates are exact in f32"
        )]
        let position = eye.map(|c| c as f32);
        let p = PlayerAuthInput {
            yaw: self.yaw,
            position,
            input_data: InputFlags::from_set(&flags),
            item_interaction_data: item,
            block_actions: (!actions.is_empty()).then_some(actions),
            ..Default::default()
        };
        self.input(Packet::PlayerAuthInput(Box::new(p)))
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

    /// Runs one tick with one auth input of `actions`.
    fn act(&mut self, actions: Vec<PlayerBlockAction>) {
        let input = self.auth(actions, None);
        self.run(vec![input]);
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

    fn block(&self, pos: BlockPos) -> block::State {
        self.state.worlds[0].block(pos).unwrap()
    }

    fn set(&mut self, pos: BlockPos, s: block::State) {
        self.state.worlds[0].set_block(pos, s).unwrap();
    }

    fn hold(&mut self, stack: Stack) {
        inventory::set_held(self.player_mut(), stack);
    }

    fn held(&self) -> Stack {
        self.player().inventory.held().clone()
    }
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
        ..Default::default()
    };
    data.held_item.stack.item_type.network_id =
        stack.item().map_or(0, |t| i32::from(t.runtime_id()));
    data.held_item.stack.count = u16::from(stack.count());
    data
}

/// The `UpdateBlock` positions among `packets`.
fn updates(packets: &[Packet]) -> Vec<BlockPos> {
    packets
        .iter()
        .filter_map(|p| match p {
            Packet::UpdateBlock(u) => Some(block_pos(u.position)),
            _ => None,
        })
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

/// Two blocks east of the feet, at foot level.
const TARGET: BlockPos = BlockPos::new(2, -60, 0);

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
fn a_refused_placement_changes_nothing_and_resyncs_the_clicked_and_target_blocks() {
    let mut h = Harness::new(|_| {});
    let planks = block_item(Kind::OakPlanks, 5);
    h.hold(planks.clone());
    // The player stands where the block would go.
    let ground = BlockPos::new(0, -61, 0);
    h.packets();
    let data = click(ground, Face::Up, &planks);
    let input = h.auth(Vec::new(), Some(data));
    h.run(vec![input]);
    assert_eq!(h.block(BlockPos::new(0, -60, 0)), block::AIR);
    assert_eq!(h.held().count(), 5);
    let sent = updates(&h.packets());
    assert!(sent.contains(&BlockPos::new(0, -60, 0)), "{sent:?}");
    assert!(sent.contains(&BlockPos::new(0, -62, 0)), "{sent:?}");
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

#[test]
fn a_refused_click_on_a_fake_block_shows_the_fake_block_again() {
    let mut h = Harness::new(|_| {});
    let planks = block_item(Kind::OakPlanks, 5);
    h.hold(planks.clone());
    let ground = BlockPos::new(0, -61, 0);
    let fake = Kind::Diamond.default_state();
    let player = crate::player::Id::new(SessionId::new(1));
    crate::Context {
        state: &mut h.state,
    }
    .send_fake_block(player, ground, fake)
    .unwrap();
    h.run(Vec::new());
    h.packets();
    // The player stands where the block would go: refused, and the clicked block is resent.
    let data = click(ground, Face::Up, &planks);
    let input = h.auth(Vec::new(), Some(data));
    h.run(vec![input]);
    assert_eq!(h.held().count(), 5);
    let resent = |h: &mut Harness, pos: BlockPos| {
        h.packets()
            .iter()
            .filter_map(|p| match p {
                Packet::UpdateBlock(u) if block_pos(u.position) == pos => {
                    Some(u.new_block_runtime_id)
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        resent(&mut h, ground),
        [fake.network_id()],
        "the fake block"
    );
    // Without the fake block, the real one.
    let cleared = crate::Context {
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
fn doors_and_trapdoors_open_and_close() {
    let mut h = Harness::new(|_| {});
    let door = Kind::OakDoor.default_state();
    let top = props::with_flag(door, "upper_block_bit", true).unwrap();
    h.set(TARGET, door);
    h.set(BlockPos::new(2, -59, 0), top);
    h.hold(Stack::EMPTY);
    let data = click(TARGET, Face::West, &Stack::EMPTY);
    let input = h.auth(Vec::new(), Some(data));
    h.run(vec![input]);
    assert_eq!(props::flag(h.block(TARGET), "open_bit"), Some(true));
    assert_eq!(
        props::flag(h.block(BlockPos::new(2, -59, 0)), "open_bit"),
        Some(true)
    );
}

#[test]
fn hoes_till_and_axes_strip_with_wear() {
    let mut h = Harness::new(|_| {});
    let hoe = stack(crate::item::Kind::WoodenHoe);
    h.hold(hoe.clone());
    let grass = BlockPos::new(2, -61, 0);
    let input = h.auth(Vec::new(), Some(click(grass, Face::Up, &hoe)));
    h.run(vec![input]);
    assert_eq!(h.block(grass).kind(), Kind::Farmland);
    assert_eq!(h.held().damage(), 1);
    let axe = stack(crate::item::Kind::WoodenAxe);
    h.hold(axe.clone());
    h.set(TARGET, Kind::OakLog.default_state());
    let input = h.auth(Vec::new(), Some(click(TARGET, Face::West, &axe)));
    h.run(vec![input]);
    assert!(h.block(TARGET).name().contains("stripped_oak_log"));
}

/// `durability = false`: tilling and breaking do not wear the tool.
#[test]
fn durability_off_keeps_tools_as_they_are() {
    let mut h = Harness::new(|c| c.general.durability = false);
    let hoe = stack(crate::item::Kind::WoodenHoe);
    h.hold(hoe.clone());
    let grass = BlockPos::new(2, -61, 0);
    let input = h.auth(Vec::new(), Some(click(grass, Face::Up, &hoe)));
    h.run(vec![input]);
    assert_eq!(h.block(grass).kind(), Kind::Farmland);
    assert_eq!(h.held(), hoe);
}

#[test]
fn buckets_take_and_pour_water() {
    let mut h = Harness::new(|_| {});
    let water = props::with_int(
        block::State::find(
            "minecraft:water",
            &[("liquid_depth", block::PropertyValue::Int(0))],
        )
        .unwrap(),
        "liquid_depth",
        0,
    )
    .unwrap();
    h.set(TARGET, water);
    let bucket = stack(crate::item::Kind::Bucket);
    h.hold(bucket.clone());
    let input = h.auth(Vec::new(), Some(click(TARGET, Face::West, &bucket)));
    h.run(vec![input]);
    assert_eq!(h.block(TARGET), block::AIR);
    assert_eq!(
        h.held().item(),
        Some(crate::item::Kind::WaterBucket.default_type())
    );
    let full = h.held();
    let ground = BlockPos::new(2, -61, 0);
    let input = h.auth(Vec::new(), Some(click(ground, Face::Up, &full)));
    h.run(vec![input]);
    assert_eq!(h.block(BlockPos::new(2, -60, 0)).kind(), Kind::Water);
    assert_eq!(
        h.held().item(),
        Some(crate::item::Kind::Bucket.default_type())
    );
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

#[test]
fn normal_transactions_do_nothing() {
    let mut h = Harness::new(|_| {});
    let tx = InventoryTransaction {
        transaction_data: InventoryTransactionData::Normal(NormalTransactionData::default()),
        ..Default::default()
    };
    let input = h.input(Packet::InventoryTransaction(tx));
    h.run(vec![input]);
    assert_eq!(h.block(TARGET), block::AIR);
}

#[test]
fn destroying_for_each_cause_follows_pmmp() {
    let mut h = Harness::new(|_| {});
    // Both halves of a door go, whichever is destroyed.
    let door = Kind::OakDoor.default_state();
    let top = props::with_flag(door, "upper_block_bit", true).unwrap();
    let up = BlockPos::new(2, -59, 0);
    h.set(TARGET, door);
    h.set(up, top);
    assert!(destroy(&mut h.state, 0, TARGET, Destroy::Rules));
    assert_eq!(h.block(TARGET), block::AIR);
    assert_eq!(h.block(up), block::AIR);
    // Ice the world removes melts; an explosion leaves air.
    h.set(TARGET, Kind::Ice.default_state());
    assert!(destroy(&mut h.state, 0, TARGET, Destroy::Rules));
    assert_eq!(h.block(TARGET).kind(), Kind::Water);
    h.set(TARGET, Kind::Stone.default_state());
    assert!(destroy(
        &mut h.state,
        0,
        TARGET,
        Destroy::Explosion { size: 4.0 }
    ));
    assert_eq!(h.block(TARGET), block::AIR);
    // Air is not destroyed again.
    assert!(!destroy(&mut h.state, 0, TARGET, Destroy::Rules));
    // A player's break whose drop finds no room changes nothing: the world is at its entity cap.
    // (The door's drop is one of the entities already.)
    let mut i = 0u32;
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

#[test]
fn honeycomb_waxes_copper_and_an_axe_scrapes_it() {
    let mut h = Harness::new(|_| {});
    let exposed = block::State::find("minecraft:exposed_copper", &[]).unwrap();
    h.set(TARGET, exposed);
    h.hold(stack(crate::item::Kind::Honeycomb).with_count(2));
    use_on_target(&mut h);
    assert_eq!(h.block(TARGET).name(), "minecraft:waxed_exposed_copper");
    assert_eq!(h.held().count(), 1);
    h.hold(stack(crate::item::Kind::WoodenAxe));
    // A tick apart, so the same click is not taken for the client repeating itself.
    h.run(Vec::new());
    use_on_target(&mut h);
    assert_eq!(h.block(TARGET).name(), "minecraft:exposed_copper");
    h.run(Vec::new());
    use_on_target(&mut h);
    assert_eq!(h.block(TARGET).name(), "minecraft:copper_block");
    assert_eq!(h.held().damage(), 2);
}

#[test]
fn candles_light_with_flint_and_go_out_by_hand() {
    let mut h = Harness::new(|_| {});
    h.set(TARGET, Kind::Candle.default_state());
    h.hold(stack(crate::item::Kind::FlintAndSteel));
    use_on_target(&mut h);
    assert_eq!(props::flag(h.block(TARGET), "lit"), Some(true));
    assert_eq!(h.held().damage(), 1);
    h.hold(Stack::EMPTY);
    h.run(Vec::new());
    use_on_target(&mut h);
    assert_eq!(props::flag(h.block(TARGET), "lit"), Some(false));
}

#[test]
fn cauldrons_fill_from_buckets_and_empty_into_bottles() {
    let mut h = Harness::new(|_| {});
    h.set(TARGET, Kind::Cauldron.default_state());
    h.hold(stack(crate::item::Kind::WaterBucket));
    use_on_target(&mut h);
    assert_eq!(props::int(h.block(TARGET), "fill_level"), Some(6));
    assert_eq!(
        h.held().item(),
        Some(crate::item::Kind::Bucket.default_type())
    );
    h.hold(stack(crate::item::Kind::GlassBottle));
    h.run(Vec::new());
    use_on_target(&mut h);
    assert_eq!(props::int(h.block(TARGET), "fill_level"), Some(4));
    assert_eq!(
        h.held().item(),
        Some(crate::item::Kind::Potion.default_type())
    );
}

#[test]
fn levers_toggle_and_buttons_stay_pressed() {
    let mut h = Harness::new(|_| {});
    // On the west face of a stone east of it; the button hangs under a stone above it. Toggling
    // updates the block itself, and an unsupported one breaks (PMMP).
    let stone = Kind::Stone.default_state();
    h.set(BlockPos::new(3, -60, 0), stone);
    h.set(BlockPos::new(2, -59, 0), stone);
    let lever = props::with_string(Kind::Lever.default_state(), "lever_direction", "west").unwrap();
    h.set(TARGET, lever);
    h.hold(Stack::EMPTY);
    use_on_target(&mut h);
    assert_eq!(props::flag(h.block(TARGET), "open_bit"), Some(true));
    h.run(Vec::new());
    use_on_target(&mut h);
    assert_eq!(props::flag(h.block(TARGET), "open_bit"), Some(false));
    h.set(TARGET, Kind::StoneButton.default_state());
    h.run(Vec::new());
    use_on_target(&mut h);
    assert_eq!(
        props::flag(h.block(TARGET), "button_pressed_bit"),
        Some(true)
    );
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
            ..Default::default()
        };
        h.input(Packet::PlayerAuthInput(Box::new(p)))
    };
    h.run(vec![input]);
    assert_eq!(props::flag(h.block(TARGET), "open_bit"), Some(false));
    assert_eq!(h.block(BlockPos::new(1, -60, 0)).kind(), Kind::OakPlanks);
}

#[test]
fn ripe_berries_come_off_before_bone_meal_counts() {
    let mut h = Harness::new(|_| {});
    let ripe = props::with_int(Kind::SweetBerryBush.default_state(), "growth", 3).unwrap();
    h.set(TARGET, ripe);
    h.hold(stack(crate::item::Kind::BoneMeal).with_count(4));
    use_on_target(&mut h);
    assert_eq!(props::int(h.block(TARGET), "growth"), Some(1));
    assert_eq!(h.held().count(), 4);
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

fn named(name: &str) -> Stack {
    let t = crate::item::Type::from_name(name, 0, None, &crate::nbt::Compound::new()).unwrap();
    Stack::new(t, 1)
}

/// Sets `block` at [`TARGET`] with its block entity, as placing it would.
fn set_with_entity(h: &mut Harness, block: block::State, item: &Stack) {
    h.set(TARGET, block);
    super::super::block_entity::placed(&mut h.state, 0, TARGET, None, item, Face::West);
}

fn entity_at(h: &Harness) -> super::super::block_entity::Kind {
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
        super::super::block_entity::Kind::Sign(s) => {
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
fn signs_keep_their_text_and_take_dye_glow_and_wax() {
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
        (super::super::block_entity::BLACK_TEXT, false, false)
    );
    let sent = h.packets();
    assert!(sent.iter().any(|p| matches!(p, Packet::BlockActorData(d)
        if d.position == [at.x, at.y, at.z] && d.nbt_data.contains_key("FrontText"))));
    h.hold(named("minecraft:red_dye").with_count(2));
    use_on_target(&mut h);
    assert_eq!(sign_at(&h).1, (0xff00_0000_u32 | 0xb0_2e26).cast_signed());
    assert_eq!(h.held().count(), 1);
    h.run(Vec::new());
    h.hold(stack(crate::item::Kind::GlowInkSac));
    use_on_target(&mut h);
    assert!(sign_at(&h).2);
    h.run(Vec::new());
    h.hold(stack(crate::item::Kind::Honeycomb));
    use_on_target(&mut h);
    assert!(sign_at(&h).3);
    // A waxed sign takes no edit.
    h.player_mut().breaking.sign_editor = Some((0, TARGET));
    sign_edit(&mut h, "changed");
    assert_eq!(sign_at(&h).0[0], "hello");
}

// pmmp: Player::openSignEditor
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
        !cx.open_sign_editor(id, grass, crate::tile::Side::Front),
        "not a sign"
    );
    let stranger = crate::player::Id::new(SessionId::new(9));
    assert!(!cx.open_sign_editor(stranger, at, crate::tile::Side::Front));
    assert!(cx.open_sign_editor(id, at, crate::tile::Side::Back));
    assert_eq!(h.player().breaking.sign_editor, Some((0, at)));
    h.run(Vec::new());
    assert!(h.packets().iter().any(|p| matches!(p, Packet::OpenSign(o)
        if o.position == [at.x, at.y, at.z] && !o.front_side)));
    sign_edit(&mut h, "second");
    assert_eq!(sign_at(&h).0[0], "second");
}

#[test]
fn flower_pots_frames_jukeboxes_and_lecterns_hold_items() {
    let mut h = Harness::new(|_| {});
    // A flower pot takes a pottable plant and gives it back.
    set_with_entity(&mut h, Kind::FlowerPot.default_state(), &Stack::EMPTY);
    let flower = block_item(Kind::Dandelion, 1);
    h.hold(flower.clone());
    use_on_target(&mut h);
    assert!(matches!(
        entity_at(&h),
        super::super::block_entity::Kind::FlowerPot { plant: Some(_) }
    ));
    assert!(h.held().is_empty());
    h.run(Vec::new());
    use_on_target(&mut h);
    assert!(matches!(
        entity_at(&h),
        super::super::block_entity::Kind::FlowerPot { plant: None }
    ));
    assert_eq!(h.held(), flower);

    // A frame takes one item, turns it, and drops it when hit.
    // Hung on the west face of a stone east of it.
    h.set(BlockPos::new(3, -60, 0), Kind::Stone.default_state());
    let frame = block::State::all()
        .find(|b| b.name() == "minecraft:frame")
        .and_then(|f| props::with_facing(f, Face::West))
        .unwrap();
    set_with_entity(&mut h, frame, &Stack::EMPTY);
    h.run(Vec::new());
    h.hold(stack(crate::item::Kind::Stick).with_count(3));
    use_on_target(&mut h);
    h.run(Vec::new());
    use_on_target(&mut h);
    match entity_at(&h) {
        super::super::block_entity::Kind::ItemFrame(f) => {
            assert_eq!(f.item, stack(crate::item::Kind::Stick));
            assert_eq!(f.rotation, 1);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(h.held().count(), 2);
    h.act(vec![action(PlayerAction::StartBreak, TARGET, Face::West)]);
    match entity_at(&h) {
        super::super::block_entity::Kind::ItemFrame(f) => assert!(f.item.is_empty()),
        other => panic!("{other:?}"),
    }

    // A jukebox takes a record and ejects it.
    let disc = named("minecraft:music_disc_13");
    set_with_entity(&mut h, Kind::Jukebox.default_state(), &Stack::EMPTY);
    h.run(Vec::new());
    h.hold(disc.clone());
    use_on_target(&mut h);
    assert!(
        matches!(entity_at(&h), super::super::block_entity::Kind::Jukebox { record } if record == disc)
    );
    h.run(Vec::new());
    use_on_target(&mut h);
    assert!(
        matches!(entity_at(&h), super::super::block_entity::Kind::Jukebox { record } if record.is_empty())
    );

    // A lectern takes a book; a page beyond it is refused.
    set_with_entity(&mut h, Kind::Lectern.default_state(), &Stack::EMPTY);
    h.run(Vec::new());
    h.hold(stack(crate::item::Kind::WritableBook));
    use_on_target(&mut h);
    let turn = h.input(Packet::LecternUpdate(
        crate::protocol::packet::LecternUpdate {
            page: 3,
            page_count: 1,
            position: [TARGET.x, TARGET.y, TARGET.z],
        },
    ));
    h.run(vec![turn]);
    assert!(matches!(entity_at(&h),
        super::super::block_entity::Kind::Lectern { book, page: 0 } if !book.is_empty()));
}

#[test]
fn a_campfire_cooks_what_it_is_given_and_drops_the_result() {
    let mut h = Harness::new(|_| {});
    let campfire = Kind::Campfire.default_state();
    let lit = props::with_flag(campfire, "extinguished", false).unwrap();
    set_with_entity(&mut h, lit, &Stack::EMPTY);
    h.hold(named("minecraft:beef").with_count(2));
    use_on_target(&mut h);
    assert_eq!(h.held().count(), 1);
    let holds = |h: &Harness| {
        h.state
            .rules
            .block_entities
            .get(0, TARGET)
            .unwrap()
            .items
            .iter()
            .filter(|s| !s.is_empty())
            .count()
    };
    assert_eq!(holds(&h), 1);
    for _ in 0..620 {
        h.run(Vec::new());
    }
    assert_eq!(holds(&h), 0, "cooked and dropped");
}

#[test]
fn the_q_key_drops_from_the_held_slot_when_the_client_stack_matches() {
    let mut h = Harness::new(|_| {});
    let sticks = stack(crate::item::Kind::Stick).with_count(5);
    h.hold(sticks.clone());
    let slot = h.player().inventory.held_slot();
    let old = crate::protocol::ItemInstance {
        stack: sticks.to_network(),
        ..Default::default()
    };
    let new = crate::protocol::ItemInstance {
        stack: sticks.with_count(2).to_network(),
        ..Default::default()
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
        ..Default::default()
    };
    let input = h.input(Packet::InventoryTransaction(tx));
    h.run(vec![input]);
    assert_eq!(h.held().count(), 3);
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
        super::super::block_entity::own_item(&h.state, 0, at),
        Some(Stack::new(blue, 1))
    );
}

/// `experience = false`: a broken ore gives no experience (the break event names none).
#[test]
fn without_experience_breaking_an_ore_gives_none() {
    let xp = |experience: bool| {
        let mut h = Harness::new(|c| c.general.experience = experience);
        h.set(TARGET, Kind::DiamondOre.default_state());
        let cause = Destroy::Player {
            session: SessionId::new(1),
            tool: stack(crate::item::Kind::IronPickaxe),
        };
        plan_destruction(&h.state, 0, TARGET, &cause).unwrap().xp
    };
    assert!((3..=7).contains(&xp(true)));
    assert_eq!(xp(false), 0);
}

/// `beds = false`: a bed is a block like any other: a click at night neither lays the player
/// down nor sets its spawn point, and the held block goes on top of it.
#[test]
fn with_beds_off_a_bed_is_a_plain_block() {
    let click_bed_at_night = |beds: bool| {
        let mut h = Harness::new(|c| c.general.beds = beds);
        let bed =
            crate::item::Type::from_name("minecraft:bed", 0, None, &crate::nbt::Compound::new())
                .unwrap();
        h.hold(Stack::new(bed, 1));
        let at = place_on_grass(&mut h, &Stack::new(bed, 1));
        assert!(h.block(at).is(Class::Bed));
        h.state.worlds[0].set_time(13_000);
        let planks = block_item(Kind::OakPlanks, 1);
        h.hold(planks.clone());
        let input = h.auth(Vec::new(), Some(click(at, Face::Up, &planks)));
        h.run(vec![input]);
        let above = h.block(at.step(Face::Up).unwrap()).kind();
        (
            h.player().is_sleeping(),
            h.player().spawn().is_some(),
            above,
        )
    };
    assert_eq!(click_bed_at_night(true), (true, true, Kind::Air));
    assert_eq!(click_bed_at_night(false), (false, false, Kind::OakPlanks));
}

/// Breaking reads the survival rules' effects (PMMP `calculateBreakProgressPerTick`): with
/// Haste II glass breaks after 3 client ticks instead of 7; and the block's own break time
/// counts (PMMP: shears break leaves at once).
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

/// A teleport ends the breaking in progress (PMMP `Player::teleport` drops the break handler)
/// and the viewers see the cracking stop.
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

/// A hungry player eats a slice of cake (PMMP `BaseCake::onInteract`): the cake loses a bite and
/// is gone after the last; a player with a full food bar leaves it alone.
#[test]
fn a_hungry_player_eats_cake_one_bite_at_a_time() {
    let mut h = Harness::new(|_| {});
    h.set(TARGET, Kind::Cake.default_state());
    h.hold(Stack::EMPTY);
    use_on_target(&mut h);
    assert_eq!(props::int(h.block(TARGET), "bite_counter"), Some(0), "full");
    survival::exhaust(h.player_mut(), 28.0);
    h.run(Vec::new());
    h.run(Vec::new());
    use_on_target(&mut h);
    assert_eq!(props::int(h.block(TARGET), "bite_counter"), Some(1));
    let last = props::with_int(Kind::Cake.default_state(), "bite_counter", 6).unwrap();
    h.set(TARGET, last);
    survival::exhaust(h.player_mut(), 28.0);
    h.run(Vec::new());
    h.run(Vec::new());
    use_on_target(&mut h);
    assert_eq!(h.block(TARGET), block::AIR, "the last bite");
}

/// A use in the air with the held `stack`.
fn air(stack: &Stack) -> UseItemTransactionData {
    let mut data = UseItemTransactionData {
        action_type: UseItemAction::ClickAir.into(),
        ..Default::default()
    };
    data.held_item.stack.item_type.network_id =
        stack.item().map_or(0, |t| i32::from(t.runtime_id()));
    data.held_item.stack.count = u16::from(stack.count());
    data
}

/// Ender pearls wait for their cooldown (PMMP `Player::useHeldItem`): a throw starts it and
/// tells the client, a throw before it ends does nothing.
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
/// per-block break particles (PMMP `Explosion::explodeB`).
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
    assert!(destroy(
        &mut h.state,
        0,
        up,
        Destroy::Explosion { size: 1.0 }
    ));
    assert_eq!((h.block(TARGET), h.block(up)), (block::AIR, block::AIR));
    assert!(!destroy(
        &mut h.state,
        0,
        TARGET,
        Destroy::Explosion { size: 1.0 }
    ));
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

/// Selecting another hotbar slot stops drawing (PMMP `Player::selectHotbarSlot`): a bow or
/// trident drawn, then put away and taken back, is not still drawn.
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
            Packet::MobEquipment(Box::new(crate::protocol::packet::MobEquipment {
                entity_runtime_id: 1,
                new_item: crate::protocol::ItemInstance::default(),
                inventory_slot: slot,
                hot_bar_slot: slot,
                window_id: 0,
            }))
        };
        let away = h.input(equip(1));
        h.run(vec![away]);
        let back = h.input(equip(0));
        h.run(vec![back]);
        assert_eq!(h.player().inventory.held_slot(), 0);
        assert!(h.player().breaking.using_since.is_none(), "{weapon:?}");
    }
}

/// The client starts the use of a bow, trident, goat horn or spyglass on its own and stops it
/// unless the server sends the `UsingItem` entity flag (P6 `itemuse`: BDS sends it, as PMMP's
/// `ACTION` flag for its `Releasable` items): the flag is on while in use and off once released.
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
        let draw = h.input(Packet::InventoryTransaction(InventoryTransaction {
            transaction_data: InventoryTransactionData::UseItem(Box::new(air(&held))),
            ..InventoryTransaction::default()
        }));
        h.run(vec![draw]);
        assert_eq!(flag(&h.packets()), Some(true), "{weapon:?} drawn");
        assert!(h.player().is_using_item());
        for _ in 0..20 {
            h.run(Vec::new());
        }
        let release = h.input(Packet::InventoryTransaction(InventoryTransaction {
            transaction_data: InventoryTransactionData::ReleaseItem(Box::default()),
            ..InventoryTransaction::default()
        }));
        h.run(vec![release]);
        assert_eq!(flag(&h.packets()), Some(false), "{weapon:?} released");
        assert!(!h.player().is_using_item());
    }
}

/// An explosion at the world's entity limit breaks an item frame: the frame and its item wait as
/// a spill in the chunk instead of being lost (PLAN.md 10.7).
#[test]
fn explosion_at_the_entity_cap_keeps_framed_items_as_spill() {
    let mut h = Harness::new(|_| {});
    h.set(BlockPos::new(3, -60, 0), Kind::Stone.default_state());
    let frame = block::State::all()
        .find(|b| b.name() == "minecraft:frame")
        .and_then(|f| props::with_facing(f, Face::West))
        .unwrap();
    set_with_entity(&mut h, frame, &Stack::EMPTY);
    let sword = stack(crate::item::Kind::DiamondSword);
    match h
        .state
        .rules
        .block_entities
        .get_mut(0, TARGET)
        .map(|e| &mut e.kind)
    {
        Some(super::super::block_entity::Kind::ItemFrame(f)) => f.item = sword.clone(),
        other => panic!("{other:?}"),
    }
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
        Destroy::Explosion { size: 1.0 }
    ));
    assert_eq!(h.block(TARGET), block::AIR);
    let kept: Vec<Stack> = h.state.rules.block_entities.all_items().cloned().collect();
    assert!(kept.contains(&sword), "{kept:?}");
}

/// A dead player standing where a block goes does not keep it from being placed (PMMP
/// `canBeCollidedWith`: alive and not a spectator); a living one does.
#[test]
fn dead_players_do_not_block_placing() {
    use super::super::tests::initialised;
    let mut h = Harness::new(|_| {});
    let (j, _rx) = join(2, 1);
    h.run(vec![j, radius(2, 2, 4)]);
    for _ in 0..8 {
        h.run(Vec::new());
    }
    let two = SessionId::new(2);
    let runtime_id = h.state.players[&two].runtime_id;
    h.run(vec![initialised(2, 3, runtime_id)]);
    assert_eq!(h.state.players[&two].phase, Phase::Spawned);
    let planks = block_item(Kind::OakPlanks, 5);
    h.hold(planks.clone());
    let stand = |h: &mut Harness| {
        h.state.players.get_mut(&two).unwrap().position = Vec3::new(2.5, -60.0 + EYE_HEIGHT, 0.5);
    };
    stand(&mut h);
    let at = place_on_grass(&mut h, &planks);
    assert_eq!(h.block(at), block::AIR, "a living player is in the way");
    for _ in 0..3 {
        h.run(Vec::new());
    }
    stand(&mut h);
    assert!(h.state.players.get_mut(&two).unwrap().kill());
    let at = place_on_grass(&mut h, &planks);
    assert_eq!(h.block(at).kind(), Kind::OakPlanks);
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
    let cx = super::super::Context {
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

/// User decision (PLAN.md 8.4, 2026-10-04): a villager from a spawn egg or from the API without
/// offers has an empty trade list (no vanilla trade templates) and opens no trade window; offers
/// set through the trade API then trade as usual.
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
    let mut cx = super::super::Context {
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
        assert!(!super::super::trade::open(&mut h.state, session, *v));
    }
    let offer = crate::custom::Offer::new(
        Stack::new(crate::item::Kind::Emerald.default_type(), 1),
        Stack::new(crate::item::Kind::Apple.default_type(), 1),
        4,
    );
    let mut cx = super::super::Context {
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
        ("version".to_owned(), Value::Int(18_168_865)),
    ])
}

/// The network IDs of the 1.26.50 reference list (the values BDS sends) of two blocks only
/// clients know.
const DRIPSTONE_BLOCK: u32 = 1_289_380_550;
const MOSS_BLOCK: u32 = 3_743_819_255;

/// Whether `packets` set `pos` to the block of network ID `id`.
fn sets(packets: &[Packet], pos: BlockPos, id: u32) -> bool {
    packets.iter().any(|p| {
        matches!(p, Packet::UpdateBlock(u)
            if block_pos(u.position) == pos && u.new_block_runtime_id == id)
    })
}

/// An inert block (one only clients know) breaks like a block of its hardness: dripstone by
/// hand in 1.5 × 1.5 s, 45 ticks. Too early, the client gets it back as itself; in time it
/// becomes air, the viewers see it break, and it drops nothing.
#[test]
fn an_inert_block_breaks_in_its_time_into_air_with_no_drops() {
    let mut h = Harness::new(|_| {});
    let inert = h.state.worlds[0]
        .put_unknown(TARGET, stored("minecraft:dripstone_block"))
        .unwrap();
    assert!(matches!(inert, Entry::Inert(..)), "{inert:?}");
    assert_eq!(inert.network_id(), DRIPSTONE_BLOCK);
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
    assert_eq!(h.state.worlds[0].entry(TARGET), Some(inert));
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

/// A creative player breaks an inert block at once; a spectator cannot.
#[test]
fn creative_breaks_an_inert_block_at_once_and_a_spectator_cannot() {
    for (mode, broken) in [(GameMode::Creative, true), (GameMode::Spectator, false)] {
        let mut h = Harness::new(|c| c.general.gamemode = mode);
        let inert = h.state.worlds[0]
            .put_unknown(TARGET, stored("minecraft:moss_block"))
            .unwrap();
        h.act(vec![action(
            PlayerAction::PredictDestroyBlock,
            TARGET,
            Face::West,
        )]);
        let now = h.state.worlds[0].entry(TARGET);
        if broken {
            assert_eq!(now, Some(Entry::AIR), "{mode:?}");
        } else {
            assert_eq!(now, Some(inert), "{mode:?}");
            assert!(sets(&h.packets(), TARGET, MOSS_BLOCK), "resent as itself");
        }
    }
}

/// Blocks go on inert blocks (ones only clients know) as on their stand-ins: a solid one (moss)
/// gives full support, so stone and a torch go on top; one that is not (moss carpet) gives none,
/// so a torch cannot stand on it but stone goes into the cell above it; seagrass is not replaced
/// (BDS's data does not say vanilla replaces it), the stone goes above. The inert block stays.
#[test]
fn blocks_go_on_inert_blocks_beside_them_and_torches_need_a_solid_one() {
    let above = BlockPos::new(2, -59, 0);
    let cases = [
        ("minecraft:moss_block", Kind::Stone, true),
        ("minecraft:moss_block", Kind::Torch, true),
        ("minecraft:moss_carpet", Kind::Torch, false),
        ("minecraft:moss_carpet", Kind::Stone, true),
        ("minecraft:seagrass", Kind::Stone, true),
    ];
    for (name, kind, placed) in cases {
        let mut h = Harness::new(|_| {});
        let mut nbt = stored(name);
        if name == "minecraft:seagrass" {
            let states = crate::nbt::Compound::from([(
                "sea_grass_type".to_owned(),
                crate::nbt::Value::String("default".to_owned()),
            )]);
            nbt.insert("states".to_owned(), crate::nbt::Value::Compound(states));
        }
        let inert = h.state.worlds[0].put_unknown(TARGET, nbt).unwrap();
        assert!(matches!(inert, Entry::Inert(..)), "{name}");
        let item = block_item(kind, 4);
        h.hold(item.clone());
        let input = h.auth(Vec::new(), Some(click(TARGET, Face::Up, &item)));
        h.run(vec![input]);
        assert_eq!(h.state.worlds[0].entry(TARGET), Some(inert), "{name} stays");
        let now = h.block(above);
        if placed {
            assert_eq!(now.kind(), kind, "{name} {kind:?}");
            assert_eq!(h.held().count(), 3);
        } else {
            assert_eq!(now, block::AIR, "{name} {kind:?}");
            assert_eq!(h.held().count(), 4);
        }
    }
}

mod book;
mod custom;
mod events;
