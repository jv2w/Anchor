//! Leaks: a player joins, opens and closes every window, is sent a form, places and breaks blocks
//! with block entities, makes and loses entities, schedules block updates, trades and leaves, 100
//! times; every table and queue [`Tables`] lists is back at its start.

use tokio::sync::mpsc;

use super::*;
use crate::block::{self, Face, Kind};
use crate::form::Modal;
use crate::item::Stack;
use crate::nbt::{Compound, List, Value};
use crate::protocol::consts::{UseItemAction, UseItemOnEntityAction};
use crate::protocol::packet::{ContainerClose, Interact, InventoryTransaction};
use crate::protocol::{
    InventoryTransactionData, UseItemOnEntityTransactionData, UseItemTransactionData,
};
use crate::world::BlockPos;

/// Where the window blocks go as (x, z): in front of the player at the spawn (looking south),
/// apart so no two chests pair, and within survival reach.
const SPOTS: [(i32, i32); 16] = [
    (-5, 1),
    (-3, 1),
    (-1, 1),
    (1, 1),
    (3, 1),
    (5, 1),
    (-5, 3),
    (-3, 3),
    (-1, 3),
    (1, 3),
    (3, 3),
    (5, 3),
    (-3, 5),
    (-1, 5),
    (1, 5),
    (3, 5),
];

/// The blocks with a window, each opened once a cycle.
const WINDOWS: [Kind; 16] = [
    Kind::Chest,
    Kind::Barrel,
    Kind::ShulkerBox,
    Kind::Hopper,
    Kind::EnderChest,
    Kind::Furnace,
    Kind::BlastFurnace,
    Kind::Smoker,
    Kind::BrewingStand,
    Kind::CraftingTable,
    Kind::EnchantingTable,
    Kind::Anvil,
    Kind::SmithingTable,
    Kind::Stonecutter,
    Kind::Loom,
    Kind::Beacon,
];

/// Every table and queue the leak test watches, by a size or a form that holds no counter.
#[derive(Debug, PartialEq)]
struct Tables {
    players: usize,
    index: usize,
    runtime_ids: usize,
    /// Whether the indexes by key, name, UUID, runtime ID and viewed chunk hold exactly the
    /// players.
    indexes_match: bool,
    /// The open forms and displays of the players, without a player's form ID counter.
    forms: String,
    menus: usize,
    deferred: usize,
    /// The sessions with a command grant and the sessions waiting for a command list resend.
    commands: (usize, usize),
    loading: usize,
    held_chunks: usize,
    /// The chunk positions the index of who sees each chunk holds.
    viewed_chunks: usize,
    chunks_idle: bool,
    extras: usize,
    saving_idle: bool,
    /// Loaded chunks, viewer entries, spills, active block entities.
    block_entities: (usize, usize, usize, usize),
    /// Scheduled updates, their index, queued neighbour updates.
    updates: (usize, usize, usize),
    /// Every entity table, without its ID counters.
    entities: String,
    /// The trading sessions and their index, without the offer ID counter.
    trades: String,
    clocks: usize,
    loaded: usize,
    quarantined: usize,
    changed_blocks: usize,
    light_idle: bool,
}

/// One session's ticks: its inputs numbered in order.
struct Session {
    id: u64,
    seq: u64,
    rx: mpsc::Receiver<Outbound>,
    /// Windows the session was shown.
    opened: usize,
}

impl Session {
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn packet(&mut self, packet: Packet) -> Input {
        let seq = self.next_seq();
        input(self.id, seq, round_trip(packet))
    }

    /// Counts the windows opened since the last call (a trade screen opens with `UpdateTrade`).
    fn read(&mut self) {
        let (packets, _, _) = received(&mut self.rx);
        self.opened += packets
            .iter()
            .filter(|p| matches!(p, Packet::ContainerOpen(_) | Packet::UpdateTrade(_)))
            .count();
    }
}

/// `text` with the number after each of `fields` (`name: 12`) replaced by `_`.
fn masked(text: &str, fields: &[&str]) -> String {
    let mut out = text.to_owned();
    for field in fields {
        let key = format!("{field}: ");
        let mut from = 0;
        while let Some(at) = out.get(from..).and_then(|rest| rest.find(&key)) {
            let start = from + at + key.len();
            let end = out
                .get(start..)
                .and_then(|rest| {
                    rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '(' && c != ')')
                })
                .map_or(out.len(), |e| start + e);
            out.replace_range(start..end, "_");
            from = start + 1;
        }
    }
    out
}

/// The sizes of every table and queue of `state` that [`Tables`] lists.
fn tables(state: &State) -> Tables {
    let world = &state.worlds[0];
    Tables {
        players: state.players.len(),
        index: state.index.len(),
        runtime_ids: state.runtime_ids.len(),
        indexes_match: state.index_matches_players(),
        forms: format!("{:?}", state.forms),
        menus: state.menus.len(),
        deferred: state.deferred.len(),
        commands: state.commands.session_entries(),
        loading: state.loading.len(),
        held_chunks: state.chunks.held(),
        viewed_chunks: state.chunks.viewed(),
        chunks_idle: state.chunks.is_idle(),
        extras: state.chunks.extras.len(),
        saving_idle: state.saving.is_idle(),
        block_entities: state.rules.block_entities.sizes(),
        updates: state.rules.updates.len(),
        entities: masked(
            &format!("{:?}", state.rules.entities),
            &["retired", "cursor"],
        ),
        trades: masked(&format!("{:?}", state.trades), &["next_net_id"]),
        clocks: state.clock.worlds_len(),
        loaded: world.loaded_chunks().len(),
        quarantined: world.quarantined().len(),
        changed_blocks: world.changed_len(),
        light_idle: world.is_light_idle(),
    }
}

/// Where the window block `i` goes.
fn spot(i: usize) -> BlockPos {
    let (x, z) = SPOTS[i];
    BlockPos::new(x, -60, z)
}

/// Runs a tick of `inputs` and reads what the session got.
fn run(state: &mut State, output: &mut Output, s: &mut Session, mut inputs: Vec<Input>) {
    tick(state, &mut inputs, output);
    s.read();
}

/// A click with the empty hand on the top of the block at `pos`.
fn click_packet(pos: BlockPos) -> Packet {
    Packet::InventoryTransaction(InventoryTransaction {
        legacy_request_id: 0,
        legacy_set_item_slots: Vec::new(),
        actions: Vec::new(),
        transaction_data: InventoryTransactionData::UseItem(Box::new(UseItemTransactionData {
            action_type: UseItemAction::ClickBlock.into(),
            block_position: [pos.x, pos.y, pos.z],
            block_face: 1,
            position: [0.5, -58.38, 0.5],
            clicked_position: [0.5, 1.0, 0.5],
            ..UseItemTransactionData::default()
        })),
    })
}

/// The client closes its latest window.
fn close_packet() -> Packet {
    Packet::ContainerClose(ContainerClose {
        window_id: 255,
        container_type: 0,
        server_side: false,
    })
}

/// Removes every entity with a runtime ID from `first` on.
fn remove_entities(state: &mut State, first: u64) {
    for r in first..state.next_runtime_id {
        if let Some(id) = entity::find(state, r) {
            assert!(entity::remove(state, id));
        }
    }
}

/// A villager trading two apples for a bread.
fn offers() -> trade::Offers {
    let apple = Stack::new(crate::item::Kind::Apple.default_type(), 2);
    let bread = Stack::new(crate::item::Kind::Bread.default_type(), 1);
    let offer = Compound::from([
        ("buyA".to_owned(), Value::Compound(apple.to_nbt())),
        ("sell".to_owned(), Value::Compound(bread.to_nbt())),
        ("maxUses".to_owned(), Value::Int(12)),
    ]);
    trade::Offers::from_nbt(
        &Compound::from([(
            "Recipes".to_owned(),
            Value::List(List::Compound(vec![offer])),
        )]),
        &crate::custom::Registry::default(),
    )
}

/// One cycle of session `n`: join, a form, every window, block entities placed and broken,
/// entities, block updates, a trade, leave; then the world settles.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one cycle reads in the order a player plays it"
)]
fn cycle(state: &mut State, output: &mut Output, n: u64) {
    let (j, rx) = join(n, 0);
    let mut s = Session {
        id: n,
        seq: 1,
        rx,
        opened: 0,
    };
    let first_entity = state.next_runtime_id;
    run(state, output, &mut s, vec![j, radius(n, 1, 4)]);
    // The view's chunks unloaded when the last player left; they load again first.
    for _ in 0..8 {
        if state.players[&SessionId::new(n)].phase == Phase::Spawning {
            break;
        }
        run(state, output, &mut s, Vec::new());
    }
    let runtime_id = state.players[&SessionId::new(n)].runtime_id;
    let init = initialised(n, s.next_seq(), runtime_id);
    run(state, output, &mut s, vec![init]);
    assert_eq!(state.players[&SessionId::new(n)].phase, Phase::Spawned);

    // A form the client never answers.
    let mut cx = task::Context { state: &mut *state };
    let player = cx.player_exact(&format!("player{n}")).unwrap();
    let modal = Modal::new("Kit", "Take a kit?", "Yes", "No");
    assert!(cx.send_form(player, modal, |_, _, _| {}).is_ok());

    // Every block window: placed, opened by a click, closed by the client.
    for (i, kind) in WINDOWS.into_iter().enumerate() {
        let pos = spot(i);
        state.worlds[0]
            .set_block(pos, kind.default_state())
            .unwrap();
        block_entity::placed(state, 0, pos, None, &Stack::EMPTY, Face::Up);
    }
    for (i, kind) in WINDOWS.into_iter().enumerate() {
        let before = s.opened;
        let open = s.packet(click_packet(spot(i)));
        run(state, output, &mut s, vec![open]);
        assert_eq!(
            s.opened,
            before + 1,
            "cycle {n}: the {kind:?} window opened"
        );
        let shut = s.packet(close_packet());
        run(state, output, &mut s, vec![shut]);
    }
    // The own inventory.
    let own = s.packet(Packet::Interact(Interact {
        action_type: 6,
        target_entity_runtime_id: runtime_id,
        position: None,
    }));
    run(state, output, &mut s, vec![own]);
    let shut = s.packet(close_packet());
    run(state, output, &mut s, vec![shut]);

    // A trade with a villager NPC, then the villager goes.
    let npc = entity::Npc::Villager {
        profession: 1,
        offers: offers(),
    };
    let villager = entity::spawn_npc(state, 0, Vec3::new(0.5, -60.0, 3.5), npc).unwrap();
    let target = entity::villager(state, villager).unwrap().2;
    let use_on = s.packet(Packet::InventoryTransaction(InventoryTransaction {
        legacy_request_id: 0,
        legacy_set_item_slots: Vec::new(),
        actions: Vec::new(),
        transaction_data: InventoryTransactionData::UseItemOnEntity(Box::new(
            UseItemOnEntityTransactionData {
                target_entity_runtime_id: target,
                action_type: UseItemOnEntityAction::Interact.into(),
                position: [0.5, -58.38, 0.5],
                ..UseItemOnEntityTransactionData::default()
            },
        )),
    }));
    run(state, output, &mut s, vec![use_on]);
    let shut = s.packet(close_packet());
    run(state, output, &mut s, vec![shut]);
    assert!(entity::remove(state, villager));

    // Entities: items on the ground, a thrown snowball, xp orbs.
    let stick = Stack::new(crate::item::Kind::Stick.default_type(), 3);
    assert!(entity::drop_item(
        state,
        0,
        Vec3::new(0.5, -60.0, 9.5),
        stick.clone()
    ));
    entity::drop_xp(state, 0, Vec3::new(0.5, -60.0, 2.5), 30);
    let snowball = Stack::new(crate::item::Kind::Snowball.default_type(), 1);
    assert!(entity::throw(state, SessionId::new(n), &snowball));
    // Block updates: sand over air falls as an entity and lands; a scheduled update of dirt.
    let sand = BlockPos::new(-2, -57, 7);
    state.worlds[0]
        .set_block(sand, Kind::Sand.default_state())
        .unwrap();
    update::schedule(state, 0, sand, 2);
    update::schedule(state, 0, BlockPos::new(2, -61, 7), 3);
    for _ in 0..30 {
        run(state, output, &mut s, Vec::new());
    }

    // The block entities break: one chest with items spills them.
    state
        .rules
        .block_entities
        .get_mut(0, spot(0))
        .unwrap()
        .items[0] = stick;
    for (i, kind) in WINDOWS.into_iter().enumerate() {
        let cause = interact::Destroy::Rules;
        assert!(interact::destroy(state, 0, spot(i), cause), "{kind:?}");
    }
    for _ in 0..10 {
        run(state, output, &mut s, Vec::new());
    }
    let landed = BlockPos::new(-2, -60, 7);
    assert_eq!(
        state.worlds[0].block(landed),
        Some(Kind::Sand.default_state()),
        "the sand fell"
    );
    state.worlds[0].set_block(landed, block::AIR).unwrap();
    remove_entities(state, first_entity);
    let bye = leave(n, s.next_seq());
    run(state, output, &mut s, vec![bye]);
    for _ in 0..5 {
        run(state, output, &mut s, Vec::new());
    }
    assert_eq!(
        s.opened,
        WINDOWS.len() + 2,
        "cycle {n}: every window, the inventory and the trade opened"
    );
    assert!(s.rx.is_closed(), "the session's queue was dropped");
}

#[test]
fn rule_cycles_return_every_table_and_queue_to_their_start() {
    let mut state = new_state(settings(|_| {}), Listeners::default());
    let mut output = Output::default();
    // A first cycle lets the chunks preloaded around the spawn go (only the spawn chunks stay
    // held) and starts the clocks; a save takes every changed chunk and its commit lets them go.
    let settle = |state: &mut State, output: &mut Output| {
        state.saving.requested = true;
        for _ in 0..20 {
            tick(state, &mut Vec::new(), output);
        }
        assert!(output.answers.is_empty());
    };
    cycle(&mut state, &mut output, 1);
    settle(&mut state, &mut output);
    let start = tables(&state);
    for n in 2..=101 {
        cycle(&mut state, &mut output, n);
    }
    settle(&mut state, &mut output);
    assert_eq!(tables(&state), start);
}
