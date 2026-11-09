//! Tests of inventories, windows and item stack requests: item conservation and refused craft requests.

mod api;
mod events;
mod menu;

use std::cell::Cell;
use std::collections::BTreeMap;

use tokio::sync::mpsc::Receiver;

use super::*;
use crate::block::{self as blk, ItemForm};
use crate::config::GameMode;
use crate::event::{InventoryOpen, Listeners, Priority};
use crate::item::{self as it, Enchantment, Extra, Type};
use crate::pool::Pool;
use crate::protocol::batch::{self, Decoder};
use crate::protocol::consts::{ContainerId as C, ItemStackResponseStatus};
use crate::protocol::packet::{ContainerClose, CraftingData, Interact, Packet};
use crate::protocol::{
    AutoCraftRecipeStackRequestAction, ConsumeStackRequestAction, CraftCreativeStackRequestAction,
    CraftRecipeStackRequestAction, DestroyStackRequestAction, DropStackRequestAction,
    FullContainerName, ItemStackRequest, ItemStackResponse, MineBlockStackRequestAction,
    PlaceStackRequestAction, StackRequestAction, StackRequestSlotInfo, SwapStackRequestAction,
    TakeStackRequestAction, TransferStackRequestAction,
};
use crate::random::Random;
use crate::save::Response;
use crate::tick::entity::Raise;
use crate::tick::interact::{self, Destroy};
use crate::tick::queue::{Input, Message, Outbound, SharedBatches};
use crate::tick::tests::{answer, initialised, join, new_state, radius, settings};
use crate::tick::{
    Context, InputLog, Output, Player, SessionId, State, block_entity, boundary, deliver, entity,
    step,
};

/// The block position the tests place their blocks at, next to the spawn.
const AT: BlockPos = BlockPos::new(3, -60, 3);

/// The container of the player's combined hotbar and inventory slots.
const COMBINED: u8 = C::CombinedHotBarAndInventory as u8;

thread_local! {
    /// The stacks the stand-in for item entities took.
    static DROPPED: Cell<Vec<Stack>> = const { Cell::new(Vec::new()) };
    /// The menu clicks the model test's handlers heard.
    static MENU_CLICKS: Cell<u32> = const { Cell::new(0) };
    /// The test stand-in for item entities: `Some(room)` while a test counts drops itself; each
    /// drop takes one place of `room` and is kept in [`DROPPED`].
    static ROOM: Cell<Option<usize>> = const { Cell::new(None) };
}

/// The stand-in's answer to a drop: `None` when no test stands in.
pub(super) fn stand_in_drop(stack: &Stack) -> Option<bool> {
    let room = ROOM.get()?;
    if room == 0 {
        return Some(false);
    }
    ROOM.set(Some(room - 1));
    let mut dropped = DROPPED.take();
    dropped.push(stack.clone());
    DROPPED.set(dropped);
    Some(true)
}

/// The stand-in's answer to whether a drop has room: `None` when no test stands in.
pub(super) fn stand_in_room() -> Option<bool> {
    ROOM.get().map(|room| room > 0)
}

pub(super) fn set_room(room: usize) {
    ROOM.set(Some(room));
}

pub(super) fn dropped() -> Vec<Stack> {
    let d = DROPPED.take();
    DROPPED.set(d.clone());
    d
}

/// A server with a flat world, driven tick by tick without the network.
pub(super) struct World {
    pub(super) state: State,
    pool: Pool,
    answers: Vec<Response>,
    /// Receivers of the joined sessions; dropping one would drop its session.
    receivers: Vec<Receiver<Outbound>>,
    /// The shared batches delivered to the joined sessions, in delivery order.
    shared: Vec<SharedBatches>,
    next_request: BTreeMap<SessionId, i32>,
}

impl World {
    pub(super) fn new(mode: GameMode) -> Self {
        Self::with_listeners(mode, Listeners::default())
    }

    /// A world whose events go to `listeners`.
    pub(super) fn with_listeners(mode: GameMode, listeners: Listeners) -> Self {
        set_room(usize::MAX);
        DROPPED.set(Vec::new());
        let mut state = new_state(settings(|c| c.general.gamemode = mode), listeners);
        // The test server loads its spawn chunks without the rule modules; hand them over.
        let loaded: Vec<_> = state.worlds[0].loaded_chunks().collect();
        for pos in loaded {
            let extras = state
                .chunks
                .extras
                .get((0, pos))
                .cloned()
                .unwrap_or_default();
            let read = crate::tick::rules::read_kept(
                (0, pos),
                &extras,
                state.worlds[0].chunk(pos).unwrap(),
                &state.rules.custom,
                state.rules.entities.types(),
            );
            state.rules.load((0, pos), read, &extras);
        }
        Self {
            state,
            pool: crate::runtime::cpu_pool(1).unwrap(),
            answers: Vec::new(),
            receivers: Vec::new(),
            shared: Vec::new(),
            next_request: BTreeMap::new(),
        }
    }

    /// The stack network ID the client knows a request slot by.
    pub(super) fn id(&self, s: SessionId, container: C, slot: u8) -> i32 {
        self.slot(s, container, slot).id.net
    }

    /// A request slot as it is now.
    pub(super) fn slot(&self, s: SessionId, container: C, slot: u8) -> Slot {
        self.try_slot(s, container, slot).expect("an open slot")
    }

    pub(super) fn try_slot(&self, s: SessionId, container: C, slot: u8) -> Option<Slot> {
        let inv = self.inv(s);
        if let Some(window) = inv.window.as_ref().filter(|w| w.id.is_some())
            && let (WindowKind::Menu { .. }, Some(menu), C::LevelEntity) =
                (window.kind, &inv.menu, container)
        {
            return Some(Slot {
                stack: menu.stacks.get(usize::from(slot))?.clone(),
                id: *window.ids.get(usize::from(slot))?,
            });
        }
        match inv.locate(container as u8, slot)? {
            SlotRef::Block(i) => {
                let window = inv.window.as_ref()?;
                let stacks = block_stacks(window.kind, &self.state.rules.block_entities);
                Some(Slot {
                    stack: stacks.get(usize::from(i))?.clone(),
                    id: *window.ids.get(usize::from(i))?,
                })
            }
            r => inv.get(r),
        }
    }

    pub(super) fn inv(&self, s: SessionId) -> &Inventory {
        &self.state.players[&s].inventory
    }

    pub(super) fn close(&mut self, s: SessionId) {
        // A window still waiting to open has no ID the client knows; it closes with 0.
        if let Some(id) = self
            .inv(s)
            .window
            .as_ref()
            .map(|w| w.id.unwrap_or_default())
        {
            self.client_close(s, id);
        }
    }

    /// A double chest at `at` and the neighbour it pairs with.
    pub(super) fn double_chest(&mut self, at: BlockPos) -> BlockPos {
        let chest = blk::Kind::Chest.default_state();
        self.place_block(at, chest);
        for (dx, dz) in [(1, 0), (0, 1), (-1, 0), (0, -1)] {
            let other = BlockPos::new(at.x + dx, at.y, at.z + dz);
            self.place_block(other, chest);
            if self.state.rules.block_entities.get(0, at).unwrap().pair() == Some(other) {
                return other;
            }
            self.state.worlds[0].set_block(other, blk::AIR).unwrap();
            block_entity::removed(&mut self.state, 0, other);
        }
        panic!("the chests pair");
    }

    pub(super) fn give(&mut self, s: SessionId, stack: Stack) {
        let left = give(self.player_mut(s), stack);
        assert!(left.is_empty());
    }

    pub(super) fn join(&mut self, n: u64) -> SessionId {
        let (j, rx) = join(n, 0);
        self.receivers.push(rx);
        self.tick(vec![j, radius(n, 1, 4)]);
        self.tick(Vec::new());
        let s = SessionId::new(n);
        assert!(self.state.players.contains_key(&s));
        s
    }

    pub(super) fn open_block(&mut self, s: SessionId, pos: BlockPos) {
        assert!(open_block(&mut self.state, s, 0, pos));
        self.ack(s);
    }

    pub(super) fn open_inventory(&mut self, s: SessionId) {
        let runtime = self.player_mut(s).runtime_id;
        on_interact(
            &mut self.state,
            s,
            &Interact {
                action_type: 6,
                target_entity_runtime_id: runtime,
                position: None,
            },
        );
        self.ack(s);
    }

    /// Acknowledges a close the server started, as the client does.
    pub(super) fn ack(&mut self, s: SessionId) {
        if let Some(id) = self.inv(s).pending_close {
            self.client_close(s, id);
        }
    }

    pub(super) fn client_close(&mut self, s: SessionId, window: u8) {
        on_container_close(
            &mut self.state,
            s,
            &ContainerClose {
                window_id: window,
                container_type: 0,
                server_side: false,
            },
        );
    }

    pub(super) fn place_block(&mut self, pos: BlockPos, block: blk::State) {
        self.state.worlds[0].set_block(pos, block).unwrap();
        block_entity::placed(&mut self.state, 0, pos, None, &Stack::EMPTY, blk::Face::Up);
    }

    /// Sends a request with the next request ID; its response.
    pub(super) fn request(
        &mut self,
        s: SessionId,
        actions: Vec<StackRequestAction>,
    ) -> ItemStackResponse {
        let id = self.next_id(s);
        self.request_as(s, id, actions)
    }

    pub(super) fn next_id(&mut self, s: SessionId) -> i32 {
        let next = self.next_request.entry(s).or_insert(-1);
        let id = *next;
        *next -= 2;
        id
    }

    pub(super) fn request_as(
        &mut self,
        s: SessionId,
        id: i32,
        actions: Vec<StackRequestAction>,
    ) -> ItemStackResponse {
        self.state.tick += 1;
        let request = ItemStackRequest {
            request_id: id,
            actions,
            filter_strings: Vec::new(),
            filter_cause: 0,
        };
        on_item_stack_request(&mut self.state, s, &request);
        let responses: Vec<ItemStackResponse> = self
            .sent(s)
            .into_iter()
            .filter_map(|p| match p {
                Packet::ItemStackResponse(r) => Some(r.responses),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(responses.len(), 1, "one response per request");
        let r = responses.into_iter().next().unwrap();
        assert_eq!(r.request_id, id);
        r
    }

    /// Takes the packets queued for `s`.
    pub(super) fn sent(&mut self, s: SessionId) -> Vec<Packet> {
        std::mem::take(&mut self.player_mut(s).outbox)
    }

    pub(super) fn player_mut(&mut self, s: SessionId) -> &mut Player {
        self.state.players.get_mut(&s).unwrap()
    }

    pub(super) fn tick(&mut self, inputs: Vec<Input>) {
        self.tick_delivered(inputs);
    }

    /// One tick; the packets it delivered to the joined sessions.
    pub(super) fn tick_delivered(&mut self, inputs: Vec<Input>) -> Vec<Packet> {
        self.tick_each(inputs).into_iter().flatten().collect()
    }

    /// One tick; the packets it delivered to each joined session, in join order.
    pub(super) fn tick_each(&mut self, mut inputs: Vec<Input>) -> Vec<Vec<Packet>> {
        let mut output = Output::default();
        let mut log = InputLog::default();
        let mut answers = std::mem::take(&mut self.answers);
        let state = &mut self.state;
        step(
            state,
            &mut inputs,
            &mut answers,
            &self.pool,
            &mut output,
            &mut log,
        );
        deliver(state, &mut output, &mut log);
        boundary(state, &mut output, &mut log);
        let requests = std::mem::take(&mut output.requests);
        self.answers = answer(state, requests);
        let mut delivered = Vec::with_capacity(self.receivers.len());
        for rx in &mut self.receivers {
            let mut packets = Vec::new();
            while let Ok(o) = rx.try_recv() {
                if let Outbound::Send(messages) = o {
                    packets.extend(messages.into_iter().filter_map(|m| match m {
                        Message::Packet(p) => Some(p),
                        Message::SharedPacket(p) | Message::SpawnList(p) => Some(p.get().clone()),
                        Message::Shared(batches) => {
                            self.shared.push(batches);
                            None
                        }
                    }));
                }
            }
            delivered.push(packets);
        }
        delivered
    }
}

pub(super) fn at(container: C, slot: u8, id: i32) -> StackRequestSlotInfo {
    StackRequestSlotInfo {
        container: FullContainerName {
            container_id: container as u8,
            dynamic_container_id: None,
        },
        slot,
        stack_network_id: id,
    }
}

pub(super) fn created(request: i32) -> StackRequestSlotInfo {
    at(C::CreatedOutput, UI_CREATED_OUTPUT, request)
}

fn transfer(
    count: u8,
    source: StackRequestSlotInfo,
    destination: StackRequestSlotInfo,
) -> TransferStackRequestAction {
    TransferStackRequestAction {
        count,
        source,
        destination,
    }
}

pub(super) fn place(
    count: u8,
    src: StackRequestSlotInfo,
    dst: StackRequestSlotInfo,
) -> StackRequestAction {
    StackRequestAction::Place(PlaceStackRequestAction {
        transfer_stack_request_action: transfer(count, src, dst),
    })
}

pub(super) fn take(
    count: u8,
    src: StackRequestSlotInfo,
    dst: StackRequestSlotInfo,
) -> StackRequestAction {
    StackRequestAction::Take(TakeStackRequestAction {
        transfer_stack_request_action: transfer(count, src, dst),
    })
}

pub(super) fn consume(count: u8, source: StackRequestSlotInfo) -> StackRequestAction {
    StackRequestAction::Consume(ConsumeStackRequestAction {
        destroy_stack_request_action: DestroyStackRequestAction { count, source },
    })
}

pub(super) fn destroy(count: u8, source: StackRequestSlotInfo) -> StackRequestAction {
    StackRequestAction::Destroy(DestroyStackRequestAction { count, source })
}

fn drop_action(count: u8, source: StackRequestSlotInfo) -> StackRequestAction {
    StackRequestAction::Drop(DropStackRequestAction {
        count,
        source,
        randomly: false,
    })
}

pub(super) fn swap(
    source: StackRequestSlotInfo,
    destination: StackRequestSlotInfo,
) -> StackRequestAction {
    StackRequestAction::Swap(SwapStackRequestAction {
        source,
        destination,
    })
}

pub(super) fn craft(recipe: u32, times: u8) -> StackRequestAction {
    StackRequestAction::CraftRecipe(CraftRecipeStackRequestAction {
        recipe_network_id: recipe,
        number_of_crafts: times,
    })
}

fn auto(recipe: u32, times: u8) -> StackRequestAction {
    StackRequestAction::AutoCraftRecipe(AutoCraftRecipeStackRequestAction {
        recipe_network_id: recipe,
        number_of_crafts: times,
        ingredients: Vec::new(),
    })
}

fn creative(id: u32) -> StackRequestAction {
    StackRequestAction::CraftCreative(CraftCreativeStackRequestAction {
        creative_item_network_id: id,
        number_of_crafts: 1,
    })
}

pub(super) fn item(kind: it::Kind) -> Type {
    kind.default_type()
}

pub(super) fn block_item(kind: blk::Kind) -> Type {
    match kind.default_state().item() {
        ItemForm::Item(t) => t,
        other => panic!("{kind:?} has no item: {other:?}"),
    }
}

/// A stack of `count` sticks.
pub(super) fn sticks(count: u8) -> Stack {
    Stack::new(item(it::Kind::Stick), count)
}

pub(super) fn ok(r: &ItemStackResponse) -> bool {
    r.status == ItemStackResponseStatus::Ok as u8
}

/// The stack network IDs in a response, slot by slot: (container, slot, count, ID).
fn shape(r: &ItemStackResponse) -> Vec<(u8, u8, u8, i32)> {
    assert!(ok(r), "status {}", r.status);
    let mut out = Vec::new();
    for c in &r.container_info {
        assert_eq!(c.container.dynamic_container_id, None);
        for s in &c.slot_info {
            assert_eq!(s.slot, s.hotbar_slot);
            assert_eq!(s.filtered_custom_name, None);
            assert_eq!(s.durability_correction, 0);
            out.push((
                c.container.container_id,
                s.slot,
                s.count,
                s.stack_network_id,
            ));
        }
    }
    out
}

/// The `CraftingData` packets among the shared batches the joined sessions got.
fn crafting_lists(w: &World) -> Vec<CraftingData> {
    let mut decoder = Decoder::new(batch::Settings::default());
    let mut payload = Vec::new();
    let mut packets = Vec::new();
    let mut lists = Vec::new();
    for batches in &w.shared {
        for body in batches.iter() {
            decoder.decode(body, &mut payload).unwrap();
            // The start-game registries are too large for the client-side decode limits.
            if batch::decode_packets(&payload, decoder.settings(), &mut packets).is_err() {
                continue;
            }
            lists.extend(packets.drain(..).filter_map(|(_, p)| match p {
                Packet::CraftingData(data) => Some(*data),
                _ => None,
            }));
        }
    }
    lists
}

/// Every slot and stack network ID of a player.
type PlayerSnapshot = (
    Slots<MAIN>,
    Slots<ARMOR>,
    Slots<1>,
    Slots<1>,
    Slots<ENDER>,
    Option<Window>,
    Vec<Stack>,
    i32,
);

/// Every slot and stack network ID of every player and container, to see that nothing changed.
fn snapshot(state: &State) -> (Vec<PlayerSnapshot>, Vec<Stack>, usize) {
    let players = state
        .players
        .values()
        .map(|p| {
            let inv = &p.inventory;
            (
                inv.main.clone(),
                inv.armor.clone(),
                inv.offhand.clone(),
                inv.cursor.clone(),
                inv.ender.clone(),
                inv.window.clone(),
                inv.pending.clone(),
                inv.next_id,
            )
        })
        .collect();
    let blocks = state.rules.block_entities.all_items().cloned().collect();
    (players, blocks, dropped().len())
}

#[test]
fn crafting_is_refused_with_the_screen_reverted_and_the_recipe_list_is_empty() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let empty = CraftingData {
        clear_recipes: true,
        ..CraftingData::default()
    };
    assert_eq!(crafting_lists(&w), [empty]);
    w.give(s, Stack::new(block_item(blk::Kind::OakLog), 4));
    w.open_inventory(s);
    w.sent(s);
    let before = snapshot(&w.state);
    let logs = w.id(s, C::CombinedHotBarAndInventory, 0);
    for first in [craft(1, 1), auto(1, 1)] {
        let id = w.next_id(s);
        w.state.tick += 1;
        let request = ItemStackRequest {
            request_id: id,
            actions: vec![
                first,
                consume(1, at(C::CombinedHotBarAndInventory, 0, logs)),
                place(4, created(id), at(C::CombinedHotBarAndInventory, 1, 0)),
            ],
            filter_strings: Vec::new(),
            filter_cause: 0,
        };
        on_item_stack_request(&mut w.state, s, &request);
        let sent = w.sent(s);
        assert!(
            sent.iter().any(|p| matches!(
                p,
                Packet::ItemStackResponse(r)
                    if r.responses.iter().all(|r| !ok(r) && r.request_id == id)
            )),
            "{sent:?}"
        );
        assert!(
            sent.iter()
                .any(|p| matches!(p, Packet::InventoryContent(_))),
            "the client gets its slots again"
        );
        assert_eq!(snapshot(&w.state), before);
    }
}

#[test]
fn stack_network_ids_and_responses_follow_the_official_server() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.give(s, Stack::new(block_item(blk::Kind::OakLog), 4));
    w.sent(s);
    w.open_inventory(s);
    let open = w.sent(s);
    let eye = BlockPos::containing(w.player_mut(s).position);
    assert_eq!(
        open,
        vec![Packet::ContainerOpen(ContainerOpen {
            window_id: 2,
            container_type: 255,
            container_position: [eye.x, eye.y, eye.z],
            container_entity_unique_id: -1,
        })]
    );
    // The whole stack into an empty slot keeps its ID.
    let logs = w.id(s, C::CombinedHotBarAndInventory, 0);
    let r = w.request(
        s,
        vec![place(
            4,
            at(C::CombinedHotBarAndInventory, 0, logs),
            at(C::CombinedHotBarAndInventory, 1, 0),
        )],
    );
    assert_eq!(shape(&r), [(COMBINED, 0, 0, 0), (COMBINED, 1, 4, logs)]);
    // Part of a stack into an empty slot gets a new ID.
    let r = w.request(
        s,
        vec![place(
            1,
            at(C::CombinedHotBarAndInventory, 1, logs),
            at(C::CombinedHotBarAndInventory, 0, 0),
        )],
    );
    let moved = shape(&r);
    assert_eq!(moved[0], (COMBINED, 1, 3, logs));
    assert_eq!((moved[1].0, moved[1].1, moved[1].2), (COMBINED, 0, 1));
    assert!(moved[1].3 > logs);
    // Closing is answered with type 247.
    w.close(s);
    assert_eq!(
        w.sent(s),
        vec![Packet::ContainerClose(ContainerClose {
            window_id: 2,
            container_type: 247,
            server_side: false,
        })]
    );
    // A chest: window 3, at the block.
    let pos = BlockPos::new(2, -60, 0);
    w.place_block(pos, blk::Kind::Chest.default_state());
    w.open_block(s, pos);
    let open = w.sent(s);
    assert!(
        open.iter().any(|p| matches!(
            p,
            Packet::ContainerOpen(o) if o.window_id == 3 && o.container_position == [2, -60, 0]
        )),
        "{open:?}"
    );
}

/// What tells the items of the totals apart: the type, the damage and the extra data.
type TotalKey = (Option<Type>, u16, String);

/// The key of `s` in the totals.
fn total_key(s: &Stack) -> TotalKey {
    let extra = s.extra().map(|e| format!("{e:?}")).unwrap_or_default();
    (s.item_type(), s.damage(), extra)
}

/// Everything players, containers, waiting lists and dropped items hold, by item.
pub(super) fn totals(state: &State) -> BTreeMap<TotalKey, u64> {
    let mut out: BTreeMap<_, u64> = BTreeMap::new();
    let mut add = |s: &Stack| {
        if s.is_empty() {
            return;
        }
        *out.entry(total_key(s)).or_default() += u64::from(s.count());
    };
    for p in state.players.values() {
        let inv = &p.inventory;
        for s in inv
            .main
            .stacks
            .iter()
            .chain(&inv.armor.stacks)
            .chain(&inv.offhand.stacks)
            .chain(&inv.cursor.stacks)
            .chain(&inv.ender.stacks)
            .chain(&inv.pending)
        {
            add(s);
        }
        if let Some(w) = &inv.window {
            for s in &w.ui {
                add(&s.stack);
            }
        }
    }
    for s in state.rules.block_entities.all_items() {
        add(s);
    }
    for s in dropped() {
        add(&s);
    }
    out
}

/// No slot holds more than its stack size, and a slot is empty exactly when its ID is 0.
pub(super) fn check_slots(state: &State) {
    let check = |s: &Stack, id: Option<StackId>| {
        assert!(s.count() <= s.max_stack_size().unwrap_or(0), "{s:?}");
        assert_eq!(s.is_empty(), s.count() == 0);
        if let Some(id) = id {
            assert_eq!(s.is_empty(), id.net == 0, "{s:?} {id:?}");
        }
    };
    for p in state.players.values() {
        let inv = &p.inventory;
        for (stacks, ids) in [
            (&inv.main.stacks[..], &inv.main.ids[..]),
            (&inv.armor.stacks[..], &inv.armor.ids[..]),
            (&inv.offhand.stacks[..], &inv.offhand.ids[..]),
            (&inv.cursor.stacks[..], &inv.cursor.ids[..]),
            (&inv.ender.stacks[..], &inv.ender.ids[..]),
        ] {
            for (s, id) in stacks.iter().zip(ids) {
                check(s, Some(*id));
            }
        }
        for (i, s) in inv.armor.stacks.iter().enumerate() {
            if let Some(a) = s.item_type().and_then(Type::armor) {
                assert_eq!(a.slot() as usize, i);
            }
        }
        if let Some(w) = &inv.window {
            for s in &w.ui {
                check(&s.stack, Some(s.id));
            }
            if w.id.is_some() && matches!(w.kind, WindowKind::Container { .. }) {
                let stacks = block_stacks(w.kind, &state.rules.block_entities);
                assert_eq!(stacks.len(), w.ids.len());
                for (s, id) in stacks.iter().zip(&w.ids) {
                    check(s, Some(*id));
                }
            }
            if let WindowKind::Menu { kind, .. } = w.kind {
                let stacks = &inv
                    .menu
                    .as_ref()
                    .expect("a menu window has its menu")
                    .stacks;
                assert_eq!(stacks.len(), kind.size());
                if w.id.is_some() {
                    assert_eq!(stacks.len(), w.ids.len());
                    for (s, id) in stacks.iter().zip(&w.ids) {
                        check(s, Some(*id));
                    }
                }
            }
        }
        let window = inv.window.as_ref().map_or(&[][..], |w| w.ui.as_slice());
        let held = inv
            .cursor
            .stacks
            .iter()
            .chain(window.iter().map(|s| &s.stack))
            .filter(|s| !s.is_empty())
            .count();
        assert!(
            inv.pending.len() + held <= MAX_SAVED_PENDING,
            "the waiting and held stacks keep their bound"
        );
        assert!(usize::from(inv.held) < usize::from(HOTBAR));
    }
    for s in state.rules.block_entities.all_items() {
        check(s, None);
    }
}

/// The request slots a player can name now.
fn addressable(inv: &Inventory, rng: &mut Random) -> Vec<(C, u8)> {
    let window = inv.window.as_ref().filter(|w| w.id.is_some());
    let mut out = Vec::new();
    for i in 0..36 {
        let c = match rng.below(3) {
            0 => C::CombinedHotBarAndInventory,
            1 => C::HotBar,
            _ => C::Inventory,
        };
        out.push((c, i));
    }
    out.extend((0..4).map(|i| (C::Armor, i)));
    out.push((C::Offhand, 0));
    out.push((C::Cursor, 0));
    if let Some(WindowKind::Menu { kind, .. }) = window.map(|w| w.kind) {
        let size = u8::try_from(kind.size()).unwrap();
        out.extend((0..size).map(|i| (C::LevelEntity, i)));
    }
    if let Some(w) = window {
        for p in w.kind.parts() {
            let c = C::all()
                .iter()
                .copied()
                .find(|c| *c as u8 == p.container)
                .unwrap();
            out.extend((p.first..p.first + p.len).map(|i| (c, i)));
        }
    }
    out
}

/// A random request slot naming: mostly the right stack network ID, sometimes a wrong one,
/// the request's own, or a slot that does not exist.
fn name(
    w: &World,
    s: SessionId,
    (c, slot): (C, u8),
    rng: &mut Random,
    request: i32,
) -> StackRequestSlotInfo {
    let id = w.id(s, c, slot);
    match rng.below(40) {
        0 => at(c, slot, id.wrapping_add(1)),
        1 => at(c, slot, request),
        2 => at(C::LevelEntity, 200, id),
        3 => at(c, slot, -999),
        _ => at(c, slot, id),
    }
}

/// What a successful request adds to and takes from the item totals: creative items placed or
/// dropped from the created output, minus what consume and destroy actions take.
fn expected_change(
    w: &World,
    s: SessionId,
    actions: &[StackRequestAction],
) -> BTreeMap<TotalKey, i64> {
    let mut out: BTreeMap<_, i64> = BTreeMap::new();
    let mut made: Option<Stack> = None;
    let is_created = |i: &StackRequestSlotInfo| {
        i.container.container_id == C::CreatedOutput as u8 && i.slot == UI_CREATED_OUTPUT
    };
    for a in actions {
        match a {
            StackRequestAction::CraftCreative(c) => {
                made = w.state.custom.creative_stack(0, c.creative_item_network_id);
            }
            StackRequestAction::Place(PlaceStackRequestAction {
                transfer_stack_request_action: t,
            })
            | StackRequestAction::Take(TakeStackRequestAction {
                transfer_stack_request_action: t,
            }) if is_created(&t.source) => {
                if let Some(m) = &made {
                    *out.entry(total_key(m)).or_default() += i64::from(t.count);
                }
            }
            StackRequestAction::Drop(d) if is_created(&d.source) => {
                if let Some(m) = &made {
                    *out.entry(total_key(m)).or_default() += i64::from(d.count);
                }
            }
            StackRequestAction::Consume(ConsumeStackRequestAction {
                destroy_stack_request_action: d,
            })
            | StackRequestAction::Destroy(d)
                if !is_created(&d.source) =>
            {
                let c = C::all()
                    .iter()
                    .copied()
                    .find(|c| *c as u8 == d.source.container.container_id);
                if let Some(slot) = c.and_then(|c| w.try_slot(s, c, d.source.slot)) {
                    *out.entry(total_key(&slot.stack)).or_default() -= i64::from(d.count);
                }
            }
            _ => {}
        }
    }
    out
}

/// A small random number as a count or slot.
fn small(v: u32) -> u8 {
    u8::try_from(v).unwrap()
}

/// Builds a random request for `s`: moves, swaps, drops, destroys, creative items; valid and
/// invalid.
fn random_request(
    w: &World,
    s: SessionId,
    rng: &mut Random,
    request: i32,
) -> Vec<StackRequestAction> {
    let slots = addressable(w.inv(s), rng);
    let full: Vec<(C, u8)> = slots
        .iter()
        .copied()
        .filter(|(c, i)| !w.slot(s, *c, *i).stack.is_empty())
        .collect();
    let pick = |rng: &mut Random, from: &[(C, u8)]| {
        from[rng.below(u32::try_from(from.len()).unwrap()) as usize]
    };
    let mut actions = Vec::new();
    // Slots a destroy or consume must not take from (changed earlier in this request).
    let mut changed: Vec<(C, u8)> = Vec::new();
    if rng.below(20) == 0 {
        let id = 1 + rng.below(1700);
        actions.push(creative(id));
        let count = 1 + small(rng.below(70));
        actions.push(place(
            count,
            created(request),
            name(w, s, pick(rng, &slots), rng, request),
        ));
        return actions;
    }
    for _ in 0..=rng.below(3) {
        let src = if full.is_empty() || rng.below(10) == 0 {
            pick(rng, &slots)
        } else {
            pick(rng, &full)
        };
        let have = w.slot(s, src.0, src.1).stack.count();
        let count = match rng.below(10) {
            0 => have.saturating_add(1),
            1 => 0,
            _ => 1 + small(rng.below(u32::from(have.max(1)))),
        };
        let dst = pick(rng, &slots);
        match rng.below(12) {
            0 if !changed.contains(&src) => {
                actions.push(drop_action(count, name(w, s, src, rng, request)));
            }
            1 if !changed.contains(&src) => {
                actions.push(destroy(count, name(w, s, src, rng, request)));
            }
            2 if !changed.contains(&src) => {
                actions.push(consume(count, name(w, s, src, rng, request)));
            }
            3 => {
                actions.push(swap(
                    name(w, s, src, rng, request),
                    name(w, s, dst, rng, request),
                ));
                changed.extend([src, dst]);
            }
            4 => actions.push(StackRequestAction::MineBlock(MineBlockStackRequestAction {
                hotbar_slot: rng.below(10).cast_signed(),
                predicted_durability: 5,
                stack_network_id: 0,
            })),
            5 => {
                actions.push(take(
                    count,
                    name(w, s, src, rng, request),
                    name(w, s, dst, rng, request),
                ));
                changed.extend([src, dst]);
            }
            _ => {
                actions.push(place(
                    count,
                    name(w, s, src, rng, request),
                    name(w, s, dst, rng, request),
                ));
                changed.extend([src, dst]);
            }
        }
    }
    actions
}

fn stock(w: &mut World, s: SessionId) {
    let mut sword = Stack::new(item(it::Kind::DiamondSword), 1)
        .with_damage(7)
        .unwrap();
    sword = sword
        .with_extra(Some(Extra {
            custom_name: Some("Blade".to_owned()),
            enchantments: vec![(Enchantment::from_id(9).unwrap(), 2)],
            ..Extra::default()
        }))
        .unwrap();
    for stack in [
        Stack::new(block_item(blk::Kind::OakLog), 64),
        Stack::new(block_item(blk::Kind::OakLog), 30),
        Stack::new(block_item(blk::Kind::OakPlanks), 64),
        Stack::new(item(it::Kind::Stick), 40),
        Stack::new(block_item(blk::Kind::Cobblestone), 64),
        Stack::new(item(it::Kind::IronHelmet), 1),
        Stack::new(item(it::Kind::IronBoots), 1),
        Stack::new(block_item(blk::Kind::CarvedPumpkin), 1),
        sword,
    ] {
        w.give(s, stack);
    }
}

/// Opens a menu of kind `k` for `s` in the model test: its handler counts the click, puts a copy
/// of the player's cursor into the clicked slot (the menu's items are the server's: the copy
/// makes no item), and closes the menu on every seventh slot.
fn open_model_menu(w: &mut World, s: SessionId, k: u32) {
    let kind = [
        crate::menu::Kind::Chest,
        crate::menu::Kind::DoubleChest,
        crate::menu::Kind::Hopper,
    ][k as usize];
    let menu = crate::menu::Menu::new(kind).item(1, Stack::new(item(it::Kind::Stick), 3));
    let opened = Context {
        state: &mut w.state,
    }
    .open_menu(crate::player::Id::new(s), menu, |cx, player, input| {
        let click = match input {
            crate::menu::Input::Click(click) => click,
            crate::menu::Input::Close(_) => return crate::menu::Response::Keep,
            crate::menu::Input::Change(_) => panic!("no slot is editable: {input:?}"),
        };
        MENU_CLICKS.set(MENU_CLICKS.get() + 1);
        cx.set_menu_item(player, click.slot(), click.cursor().clone())
            .expect("the cursor's stack fits a slot");
        if click.slot() % 7 == 0 {
            crate::menu::Response::Close
        } else {
            crate::menu::Response::Keep
        }
    });
    assert!(
        matches!(opened, Ok(_) | Err(crate::menu::Error::Busy)),
        "{opened:?}"
    );
}

/// One run of the model test: `count` random requests of two players around shared containers.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: the model test checks every request kind in one loop"
)]
fn run_model(mode: GameMode, seed: i64, count: u32, listeners: Option<Listeners>) {
    // Listeners may move items themselves (keeping their totals), so a refused request keeps
    // the totals but not every slot.
    let listening = listeners.is_some();
    let mut w = World::with_listeners(mode, listeners.unwrap_or_default());
    let players = [w.join(1), w.join(2)];
    let chest = AT;
    w.double_chest(chest);
    let barrel = BlockPos::new(-3, -60, 3);
    w.place_block(barrel, blk::Kind::Barrel.default_state());
    let ender = BlockPos::new(-3, -60, -3);
    w.place_block(ender, blk::Kind::EnderChest.default_state());
    for p in players {
        stock(&mut w, p);
    }
    let mut rng = Random::new(seed, 0, 7);
    let (mut oks, mut errors, mut drops) = (0_u32, 0_u32, 0_u32);
    let mut last: Option<(SessionId, i32, Vec<StackRequestAction>)> = None;
    for n in 0..count {
        let s = players[rng.below(2) as usize];
        set_room(if rng.below(10) == 0 { 0 } else { usize::MAX });
        let before = totals(&w.state);
        let snap = snapshot(&w.state);
        match rng.below(100) {
            0..=2 => {
                match rng.below(7) {
                    0 => w.open_inventory(s),
                    1 => w.open_block(s, chest),
                    2 => w.open_block(s, barrel),
                    3 => w.open_block(s, ender),
                    4 | 5 => open_model_menu(&mut w, s, rng.below(3)),
                    _ => w.close(s),
                }
                w.sent(s);
                assert_eq!(totals(&w.state), before, "windows keep every item");
            }
            3 => {
                // Pick dropped items up again.
                let picked = DROPPED.take();
                for stack in picked {
                    let left = give(w.player_mut(s), stack);
                    if !left.is_empty() {
                        let mut d = DROPPED.take();
                        d.push(left);
                        DROPPED.set(d);
                    }
                }
                w.sent(s);
                assert_eq!(totals(&w.state), before, "pickups keep every item");
            }
            4 if last.is_some() => {
                let (ls, id, actions) = last.clone().unwrap();
                let r = w.request_as(ls, id, actions);
                assert!(!ok(&r), "a repeat is refused");
                assert_eq!(snapshot(&w.state), snap, "a repeat changes nothing");
            }
            _ => {
                let id = w.next_id(s);
                let actions = random_request(&w, s, &mut rng, id);
                let expected = expected_change(&w, s, &actions);
                let dropping = actions
                    .iter()
                    .any(|a| matches!(a, StackRequestAction::Drop(_)));
                let touch = super::menu::touch(
                    &w.state,
                    s,
                    &ItemStackRequest {
                        request_id: id,
                        actions: actions.clone(),
                        filter_strings: Vec::new(),
                        filter_cause: 0,
                    },
                );
                // A menu handler changes the menu's slots and may close it, never an item.
                let menu_request = !matches!(touch, super::menu::Touch::Nothing);
                let r = w.request_as(s, id, actions.clone());
                let after = totals(&w.state);
                if ok(&r) {
                    oks += 1;
                    drops += u32::from(dropping);
                    let mut want: BTreeMap<_, i64> = before
                        .iter()
                        .map(|(k, v)| (k.clone(), i64::try_from(*v).unwrap()))
                        .collect();
                    for (k, d) in expected {
                        *want.entry(k).or_default() += d;
                    }
                    want.retain(|_, v| *v != 0);
                    let got: BTreeMap<_, i64> = after
                        .iter()
                        .map(|(k, v)| (k.clone(), i64::try_from(*v).unwrap()))
                        .collect();
                    assert_eq!(got, want, "request {n}: {actions:?}");
                } else {
                    errors += 1;
                    assert_eq!(after, before, "request {n}");
                    if !listening && !menu_request {
                        assert_eq!(
                            snapshot(&w.state),
                            snap,
                            "a refused request changes nothing"
                        );
                    }
                }
                last = Some((s, id, actions));
            }
        }
        tick_menus(&mut w.state);
        check_slots(&w.state);
        for p in players {
            w.sent(p);
        }
    }
    // Both kinds were exercised in numbers.
    assert!(
        oks > count / 10 && errors > count / 10,
        "{oks} ok, {errors} refused"
    );
    assert!(drops > 100, "{drops} drops");
    let clicks = MENU_CLICKS.take();
    assert!(clicks > 20, "{clicks} menu clicks");
}

#[test]
fn random_requests_keep_every_item_accounted_for() {
    run_model(GameMode::Survival, 1, 50_000, None);
    run_model(GameMode::Creative, 2, 50_000, None);
}

// api: InventoryTransactionEvent, PlayerDropItemEvent
#[test]
fn random_requests_with_random_listeners_keep_every_item_accounted_for() {
    // Listeners from a fixed seed cancel, change fields or move items themselves through the
    // Context; no item is made or lost but by the requests' own creative items and drops.
    run_model(
        GameMode::Survival,
        3,
        50_000,
        Some(events::random_listeners(3)),
    );
    run_model(
        GameMode::Creative,
        4,
        50_000,
        Some(events::random_listeners(4)),
    );
}

#[test]
fn the_same_request_twice_leaves_the_state_of_once() {
    let run = |twice: bool| {
        let mut w = World::new(GameMode::Survival);
        let s = w.join(1);
        w.give(s, Stack::new(block_item(blk::Kind::OakLog), 10));
        w.open_inventory(s);
        let id = w.id(s, C::CombinedHotBarAndInventory, 0);
        let actions = vec![place(
            3,
            at(C::CombinedHotBarAndInventory, 0, id),
            at(C::CombinedHotBarAndInventory, 1, 0),
        )];
        assert!(ok(&w.request_as(s, -1, actions.clone())));
        if twice {
            assert!(!ok(&w.request_as(s, -1, actions)));
        }
        w.state.tick = 1000;
        w.state.hash()
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn creative_actions_are_refused_in_survival() {
    for (mode, expect) in [(GameMode::Survival, false), (GameMode::Creative, true)] {
        let mut w = World::new(mode);
        let s = w.join(1);
        let id = w.next_id(s);
        let r = w.request_as(
            s,
            id,
            vec![creative(1), place(1, created(id), at(C::HotBar, 0, 0))],
        );
        assert_eq!(ok(&r), expect);
        let r = w.request(s, vec![destroy(1, at(C::HotBar, 0, w.id(s, C::HotBar, 0)))]);
        assert_eq!(ok(&r), expect);
    }
}

#[test]
fn the_legacy_drop_takes_from_a_main_slot_only_with_room() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.give(s, Stack::new(item(it::Kind::Stick), 10));
    let id = w.id(s, C::HotBar, 0);
    // A refused drop sends the slot back: the client took the items away as it sent the drop.
    let resent = |w: &mut World| {
        let sent = w.sent(s);
        assert!(
            sent.iter().any(|p| matches!(
                p,
                Packet::InventorySlot(i)
                    if i.slot == 0 && i.new_item.stack.count == 10 && i.new_item.stack_network_id == id
            )),
            "{sent:?}"
        );
    };
    w.sent(s);
    set_room(0);
    assert!(!drop_from_main(&mut w.state, s, 0, 3));
    assert_eq!(w.slot(s, C::HotBar, 0).stack.count(), 10);
    resent(&mut w);
    set_room(1);
    assert!(
        !drop_from_main(&mut w.state, s, 0, 11),
        "more than the slot holds"
    );
    resent(&mut w);
    assert!(drop_from_main(&mut w.state, s, 0, 3));
    let after = w.slot(s, C::HotBar, 0);
    assert_eq!(after.stack.count(), 7);
    assert_ne!(after.id.net, id, "a change outside a request gets a new ID");
    assert_eq!(dropped(), vec![Stack::new(item(it::Kind::Stick), 3)]);
}

#[test]
fn a_drop_without_room_in_the_world_fails_and_moves_nothing() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.give(s, Stack::new(item(it::Kind::Stick), 10));
    let id = w.id(s, C::HotBar, 0);
    set_room(0);
    assert!(!ok(
        &w.request(s, vec![drop_action(4, at(C::HotBar, 0, id))])
    ));
    assert_eq!(w.inv(s).main.stack(0).count(), 10);
    set_room(1);
    assert!(ok(&w.request(s, vec![drop_action(4, at(C::HotBar, 0, id))])));
    assert_eq!(w.inv(s).main.stack(0).count(), 6);
    assert_eq!(dropped(), vec![Stack::new(item(it::Kind::Stick), 4)]);
    // A second drop in one request is refused: the first could not be taken back.
    set_room(5);
    let r = w.request(
        s,
        vec![
            drop_action(1, at(C::HotBar, 0, id)),
            drop_action(1, at(C::HotBar, 0, id)),
        ],
    );
    assert!(!ok(&r));
    assert_eq!(w.inv(s).main.stack(0).count(), 6);
}

/// Puts 5 of 64 planks on the cursor with the inventory open.
fn hold_on_cursor(w: &mut World, s: SessionId) {
    w.give(s, Stack::new(block_item(blk::Kind::OakPlanks), 64));
    w.open_inventory(s);
    let planks = w.id(s, C::CombinedHotBarAndInventory, 0);
    let r = w.request(
        s,
        vec![place(
            5,
            at(C::CombinedHotBarAndInventory, 0, planks),
            at(C::Cursor, 0, 0),
        )],
    );
    assert!(ok(&r));
    assert_eq!(w.inv(s).main.stack(0).count(), 64 - 5);
}

#[test]
fn closing_leaving_or_dying_with_the_cursor_held_returns_every_item() {
    // Close, with room: everything comes back.
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    hold_on_cursor(&mut w, s);
    let before = totals(&w.state);
    w.close(s);
    assert_eq!(totals(&w.state), before);
    assert_eq!(w.inv(s).main.stack(0).count(), 64);
    assert!(dropped().is_empty());

    // A full inventory: with no room in the world the cursor's stack waits, and comes back as
    // soon as a slot frees.
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    hold_on_cursor(&mut w, s);
    for _ in 1..36 {
        w.give(s, Stack::new(block_item(blk::Kind::Cobblestone), 64));
    }
    w.give(s, Stack::new(block_item(blk::Kind::OakPlanks), 5));
    assert!(w.inv(s).first_empty().is_none(), "the inventory is full");
    let before = totals(&w.state);
    set_room(0);
    close_window(&mut w.state, s);
    w.ack(s);
    assert_eq!(totals(&w.state), before, "nothing duplicated or lost");
    assert!(dropped().is_empty());
    let waiting = w.inv(s).pending.len();
    assert_eq!(waiting, 1, "the cursor's stack waits");
    let mut saved = Compound::new();
    save(w.inv(s), &mut saved);
    let mut back = Inventory::default();
    load(
        &mut back,
        &saved,
        &crate::custom::Registry::default(),
        "player",
    );
    assert_eq!(back.pending, w.inv(s).pending, "waiting stacks are saved");
    let id = w.id(s, C::HotBar, 1);
    set_room(usize::MAX);
    assert!(ok(
        &w.request(s, vec![drop_action(64, at(C::HotBar, 1, id))])
    ));
    assert!(
        w.inv(s).pending.is_empty(),
        "a freed slot takes the waiting stacks"
    );
    assert_eq!(totals(&w.state), before);

    // Dying: the window closes, then everything is taken for the death drops.
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    hold_on_cursor(&mut w, s);
    close_window(&mut w.state, s);
    let all = take_all(w.player_mut(s));
    assert_eq!(all.iter().map(|s| u64::from(s.count())).sum::<u64>(), 64);
    assert!(totals(&w.state).is_empty());
}

#[test]
fn the_most_waiting_stacks_lose_no_item_and_holding_more_is_refused() {
    // A full inventory, a world with no room for drops, the cursor held, and as many waiting
    // stacks as the bound leaves (a load brings up to the bound).
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    for _ in 0..36 {
        w.give(s, Stack::new(block_item(blk::Kind::Cobblestone), 64));
    }
    let stick = Stack::new(item(it::Kind::Stick), 1);
    let inv = &mut w.player_mut(s).inventory;
    inv.pending = vec![stick.clone(); MAX_SAVED_PENDING - 1];
    let net = inv.new_id();
    let id = StackId { net, request: None };
    assert!(inv.put(SlotRef::Cursor, Slot { stack: stick, id }));
    check_slots(&w.state);
    set_room(0);
    let before = totals(&w.state);
    close_window(&mut w.state, s);
    assert_eq!(totals(&w.state), before, "no item is lost");
    assert_eq!(w.inv(s).pending.len(), MAX_SAVED_PENDING);
    assert!(dropped().is_empty());
    check_slots(&w.state);

    // Holding one more stack outside the slots is refused; nothing changes.
    let id = w.id(s, C::HotBar, 0);
    let to_cursor = |id| vec![take(1, at(C::HotBar, 0, id), at(C::Cursor, 0, 0))];
    assert!(!ok(&w.request(s, to_cursor(id))));
    assert_eq!(totals(&w.state), before);
    assert!(w.inv(s).cursor.stack(0).is_empty());
    // With one waiting stack fewer it is allowed, and a close keeps the bound.
    w.player_mut(s).inventory.pending.pop();
    let before = totals(&w.state);
    let id = w.id(s, C::HotBar, 0);
    assert!(ok(&w.request(s, to_cursor(id))));
    check_slots(&w.state);
    close_window(&mut w.state, s);
    assert_eq!(totals(&w.state), before);
    assert!(w.inv(s).pending.len() <= MAX_SAVED_PENDING);
    check_slots(&w.state);
}

#[test]
fn a_leaving_player_keeps_its_items_in_its_saved_data() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    hold_on_cursor(&mut w, s);
    close_window(&mut w.state, s);
    let mut extra = Compound::new();
    save(w.inv(s), &mut extra);
    let mut back = Inventory::default();
    load(
        &mut back,
        &extra,
        &crate::custom::Registry::default(),
        "player",
    );
    let count: u64 = back.main.stacks.iter().map(|s| u64::from(s.count())).sum();
    assert_eq!(count, 64);
}

/// Items in the saved inventory keys, in `Inventory` and `PendingItems`.
fn saved_count(extra: &Compound) -> u64 {
    ["Inventory", "PendingItems"]
        .iter()
        .filter_map(|k| match extra.get(*k) {
            Some(Value::List(List::Compound(l))) => Some(l),
            _ => None,
        })
        .flatten()
        .filter_map(Stack::from_nbt)
        .map(|s| u64::from(s.count()))
        .sum()
}

#[test]
fn a_save_holds_the_cursor_once_and_a_load_gives_it_back() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    // 64 planks: 59 in the main slots and 5 on the cursor.
    hold_on_cursor(&mut w, s);
    let mut saved = Compound::new();
    save(w.inv(s), &mut saved);
    assert_eq!(saved_count(&saved), 64, "every plank once");
    let mut back = Inventory::default();
    load(
        &mut back,
        &saved,
        &crate::custom::Registry::default(),
        "player",
    );
    assert!(back.window.is_none() && back.cursor.stack(0).is_empty());
    assert_eq!(back.pending.len(), 1);
    restore(&mut back);
    assert!(back.pending.is_empty(), "the slots have room");
    let main: u64 = back.main.stacks.iter().map(|s| u64::from(s.count())).sum();
    assert_eq!(main, 64);
    // The live player closes the window: the same 64, saved once each again.
    let before = totals(&w.state);
    w.close(s);
    assert_eq!(totals(&w.state), before);
    let mut again = Compound::new();
    save(w.inv(s), &mut again);
    assert_eq!(saved_count(&again), 64);
    // A loaded player reopening and closing a window changes nothing.
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.player_mut(s).inventory = back;
    let chest = BlockPos::new(2, -60, 0);
    w.place_block(chest, blk::Kind::Chest.default_state());
    w.open_block(s, chest);
    w.close(s);
    let mut third = Compound::new();
    save(w.inv(s), &mut third);
    assert_eq!(saved_count(&third), 64);
}

#[test]
fn the_inventory_round_trips_through_the_player_extra_byte_for_byte() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    stock(&mut w, s);
    let p = w.player_mut(s);
    set_armor(
        p,
        0,
        Stack::new(item(it::Kind::IronHelmet), 1)
            .with_damage(3)
            .unwrap(),
    );
    p.inventory.held = 4;
    set_held(p, Stack::new(block_item(blk::Kind::Cobblestone), 2));
    assert_eq!(p.inventory.held().count(), 2);
    assert_eq!(p.inventory.armor()[0].damage(), 3);
    p.inventory.offhand.stacks[0] = Stack::new(item(it::Kind::Stick), 3);
    assert_eq!(p.inventory.offhand().count(), 3);
    p.inventory.ender.stacks[26] = Stack::new(block_item(blk::Kind::OakLog), 9);
    p.inventory
        .pending
        .push(Stack::new(item(it::Kind::Stick), 1));
    let mut first = Compound::new();
    save(w.inv(s), &mut first);
    let mut back = Inventory::default();
    load(
        &mut back,
        &first,
        &crate::custom::Registry::default(),
        "player",
    );
    let mut second = Compound::new();
    save(&back, &mut second);
    let bytes = |c: &Compound| {
        let mut w = crate::protocol::Writer::new(Vec::new(), 0);
        w.nbt(c, crate::nbt::Encoding::LittleEndian).unwrap();
        w.as_bytes().to_vec()
    };
    assert_eq!(bytes(&first), bytes(&second));
    assert_eq!(back.held, 4);
    assert_eq!(back.main.stacks, w.inv(s).main.stacks);
    assert_eq!(back.armor.stacks, w.inv(s).armor.stacks);
    // A new player has nothing, and unknown keys are no inventory.
    let mut empty = Inventory::default();
    load(
        &mut empty,
        &Compound::new(),
        &crate::custom::Registry::default(),
        "player",
    );
    assert!(empty.main.stacks.iter().all(Stack::is_empty));
}

#[test]
fn join_open_close_leave_cycles_leave_every_table_at_its_start() {
    let mut w = World::new(GameMode::Survival);
    let chest = AT;
    w.double_chest(chest);
    let start = w.state.rules.block_entities.sizes();
    for n in 0..100_u64 {
        let s = w.join(10 + n);
        w.give(s, Stack::new(item(it::Kind::Stick), 5));
        w.open_block(s, chest);
        let window = w.inv(s).window.clone().unwrap();
        assert_eq!(window.ids.len(), 54);
        let id = w.id(s, C::HotBar, 0);
        let slot = u8::try_from(n % 54).unwrap();
        let there = w.id(s, C::LevelEntity, slot);
        let r = w.request(
            s,
            vec![place(
                1,
                at(C::HotBar, 0, id),
                at(C::LevelEntity, slot, there),
            )],
        );
        assert!(ok(&r));
        if n % 2 == 0 {
            w.close(s);
        }
        w.state.leave(s, crate::event::QuitReason::Left);
        w.tick(Vec::new());
        let now = w.state.rules.block_entities.sizes();
        assert_eq!(
            (now.viewers, now.spills),
            (start.viewers, start.spills),
            "no viewer or spill is left"
        );
        assert!(w.state.players.is_empty());
    }
    let in_chest: u64 = w
        .state
        .rules
        .block_entities
        .all_items()
        .map(|s| u64::from(s.count()))
        .sum();
    assert_eq!(in_chest, 100);
}

#[test]
fn every_viewer_of_a_chest_sees_its_changes() {
    let mut w = World::new(GameMode::Survival);
    let chest = AT;
    w.double_chest(chest);
    let a = w.join(1);
    let b = w.join(2);
    w.give(a, Stack::new(item(it::Kind::Stick), 5));
    w.open_block(a, chest);
    w.open_block(b, chest);
    w.sent(b);
    let id = w.id(a, C::HotBar, 0);
    assert!(ok(&w.request(
        a,
        vec![place(5, at(C::HotBar, 0, id), at(C::LevelEntity, 40, 0))]
    )));
    let seen = w.sent(b);
    let Some(Packet::InventorySlot(slot)) = seen.first() else {
        panic!("the other viewer gets the slot: {seen:?}");
    };
    assert_eq!((slot.slot, slot.new_item.stack.count), (40, 5));
    let b_id = w.id(b, C::LevelEntity, 40);
    assert_eq!(slot.new_item.stack_network_id, b_id);
    // B takes them with its own ID for the slot.
    let r = w.request(
        b,
        vec![take(5, at(C::LevelEntity, 40, b_id), at(C::HotBar, 3, 0))],
    );
    assert!(ok(&r));
    assert_eq!(w.inv(b).main.stack(3).count(), 5);
    // The chest breaking closes both windows and spills nothing (it is empty again).
    block_entity::removed(&mut w.state, 0, chest);
    assert!(w.inv(a).window.is_none() && w.inv(b).window.is_none());
}

/// The names of the sounds in `packets`.
fn sounds(packets: &[Packet]) -> Vec<String> {
    packets
        .iter()
        .filter_map(|p| match p {
            Packet::LevelSoundEvent(s) => Some(s.sound_type.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn chest_like_blocks_sound_when_the_first_viewer_opens_and_the_last_closes() {
    let cases = [
        (blk::Kind::Chest, "chest.open", "chest.closed"),
        (
            blk::Kind::EnderChest,
            "enderchest.open",
            "enderchest.closed",
        ),
        (blk::Kind::Barrel, "block.barrel.open", "block.barrel.close"),
        (
            blk::Kind::ShulkerBox,
            "shulkerbox.open",
            "shulkerbox.closed",
        ),
    ];
    for (kind, open, close) in cases {
        let mut w = World::new(GameMode::Survival);
        let pos = AT;
        w.place_block(pos, kind.default_state());
        let a = w.join(1);
        let b = w.join(2);
        w.sent(a);
        w.sent(b);
        // The opener and the players near hear it.
        w.open_block(a, pos);
        assert_eq!(sounds(&w.sent(a)), [open], "{kind:?}");
        assert_eq!(sounds(&w.sent(b)), [open], "{kind:?}");
        w.open_block(b, pos);
        w.close(a);
        assert!(sounds(&w.sent(b)).is_empty(), "{kind:?}");
        w.close(b);
        assert_eq!(sounds(&w.sent(a)), [close], "{kind:?}");
    }
}

#[test]
fn a_double_chest_sounds_once_at_its_left_half() {
    let mut w = World::new(GameMode::Survival);
    let chest = AT;
    w.double_chest(chest);
    let a = w.join(1);
    w.sent(a);
    w.open_block(a, chest);
    let opened: Vec<_> = w
        .sent(a)
        .into_iter()
        .filter_map(|p| match p {
            Packet::LevelSoundEvent(s) => Some((s.sound_type, s.position)),
            _ => None,
        })
        .collect();
    let left = w
        .inv(a)
        .window
        .as_ref()
        .and_then(|win| win.kind.blocks()[0])
        .map(|(_, p)| p)
        .unwrap();
    assert_eq!(
        opened,
        [("chest.open".to_owned(), left.to_center().to_f32())]
    );
}

#[test]
fn a_broken_chest_spills_its_items_and_waits_while_the_world_is_full() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let chest = AT;
    w.place_block(chest, blk::Kind::Chest.default_state());
    w.give(s, Stack::new(item(it::Kind::Stick), 64));
    w.give(s, Stack::new(block_item(blk::Kind::Cobblestone), 64));
    w.open_block(s, chest);
    for (from, to) in [(0, 0), (1, 5)] {
        let id = w.id(s, C::HotBar, from);
        assert!(ok(&w.request(
            s,
            vec![place(
                64,
                at(C::HotBar, from, id),
                at(C::LevelEntity, to, 0)
            )]
        )));
    }
    let before = totals(&w.state);
    set_room(1);
    w.state.worlds[0].set_block(chest, blk::AIR).unwrap();
    block_entity::removed(&mut w.state, 0, chest);
    assert!(w.inv(s).window.is_none());
    assert_eq!(totals(&w.state), before);
    assert_eq!(
        w.state.rules.block_entities.sizes().spills,
        1,
        "one spill waits"
    );
    block_entity::tick(&mut w.state);
    assert_eq!(dropped().len(), 1, "the world is still full");
    set_room(usize::MAX);
    block_entity::tick(&mut w.state);
    assert_eq!(dropped().len(), 2);
    assert_eq!(w.state.rules.block_entities.sizes().spills, 0);
    assert_eq!(totals(&w.state), before);
}

#[test]
fn a_shulker_box_keeps_its_items_in_its_item() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let pos = AT;
    let shulker = blk::State::all()
        .find(|b| b.is(blk::Class::ShulkerBox))
        .unwrap();
    w.place_block(pos, shulker);
    w.give(s, Stack::new(item(it::Kind::Stick), 7));
    w.open_block(s, pos);
    let id = w.id(s, C::HotBar, 0);
    assert!(ok(&w.request(
        s,
        vec![place(7, at(C::HotBar, 0, id), at(C::ShulkerBox, 2, 0))]
    )));
    // A shulker box does not go into a shulker box.
    let ItemForm::Item(box_item) = shulker.item() else {
        panic!("a shulker box item");
    };
    w.give(s, Stack::new(box_item, 1));
    let id = w.id(s, C::HotBar, 0);
    assert!(!ok(&w.request(
        s,
        vec![place(1, at(C::HotBar, 0, id), at(C::ShulkerBox, 3, 0))]
    )));
    let dropped_box = block_entity::with_block_data(&w.state, 0, pos, Stack::new(box_item, 1));
    w.state.worlds[0].set_block(pos, blk::AIR).unwrap();
    block_entity::removed(&mut w.state, 0, pos);
    assert!(dropped().is_empty(), "the box keeps its items");
    // Placed again from that item, the box holds them.
    w.state.worlds[0].set_block(pos, shulker).unwrap();
    block_entity::placed(&mut w.state, 0, pos, Some(s), &dropped_box, blk::Face::Up);
    let e = w.state.rules.block_entities.get(0, pos).unwrap();
    assert_eq!(e.items[2], Stack::new(item(it::Kind::Stick), 7));
    // Its network form shows the stored items without their hidden data.
    assert!(dropped_box.to_network().nbt_data.contains_key("Items"));
}

#[test]
fn window_ids_count_from_two_and_a_new_window_waits_for_the_close() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let barrel = AT;
    w.place_block(barrel, blk::Kind::Barrel.default_state());
    w.open_inventory(s);
    assert_eq!(w.inv(s).window.as_ref().unwrap().id, Some(2));
    w.sent(s);
    // Opening the barrel while the inventory is open: the server closes it first.
    assert!(open_block(&mut w.state, s, 0, barrel));
    let sent = w.sent(s);
    // The barrel's open sound goes with its first viewer, which the waiting window already is.
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(
        sent[0],
        Packet::ContainerClose(ContainerClose {
            window_id: 2,
            container_type: 255,
            server_side: true,
        })
    );
    assert!(matches!(&sent[1], Packet::LevelSoundEvent(e) if e.sound_type == "block.barrel.open"));
    assert_eq!(w.inv(s).window.as_ref().unwrap().id, None, "waiting");
    w.client_close(s, 2);
    let sent = w.sent(s);
    assert!(
        sent.iter()
            .any(|p| matches!(p, Packet::ContainerOpen(o) if o.window_id == 3)),
        "{sent:?}"
    );
    assert_eq!(w.inv(s).window.as_ref().unwrap().id, Some(3));
    // The client closing "window -1" closes the latest.
    w.client_close(s, 255);
    assert!(w.inv(s).window.is_none());
    for _ in 0..200 {
        w.open_inventory(s);
        w.close(s);
    }
    let id = w.inv(s).last_window;
    assert!((1..100).contains(&id));
}

#[test]
fn hotbar_selection_and_pick_block_select_or_take_the_picked_item() {
    let mut w = World::new(GameMode::Creative);
    let s = w.join(1);
    let select = |w: &mut World, slot: u8| {
        on_mob_equipment(
            &mut w.state,
            s,
            &MobEquipment {
                entity_runtime_id: 1,
                new_item: ItemInstance::default(),
                inventory_slot: slot,
                hot_bar_slot: slot,
                window_id: 0,
            },
        );
    };
    select(&mut w, 4);
    assert_eq!(w.inv(s).held_slot(), 4);
    select(&mut w, 12);
    assert_eq!(w.inv(s).held_slot(), 4);
    assert!(
        w.tick_delivered(Vec::new())
            .iter()
            .any(|p| matches!(p, Packet::MobEquipment(m) if m.hot_bar_slot == 4))
    );
    let pos = AT;
    w.place_block(pos, blk::Kind::Cobblestone.default_state());
    let pick = BlockPickRequest {
        position: [3, -60, 3],
        add_block_nbt: false,
        hot_bar_slot: 4,
    };
    on_block_pick_request(&mut w.state, s, &pick);
    assert_eq!(
        w.inv(s).main.stack(0).item_type(),
        Some(block_item(blk::Kind::Cobblestone))
    );
    assert_eq!(w.inv(s).held_slot(), 0, "an empty hotbar slot is selected");
    select(&mut w, 6);
    on_block_pick_request(&mut w.state, s, &pick);
    assert_eq!(w.inv(s).held_slot(), 0, "an item already held is selected");
    // Players with finite resources, spectators too, only select what they have.
    for mode in [GameMode::Survival, GameMode::Spectator] {
        let mut w = World::new(mode);
        let s = w.join(1);
        w.place_block(pos, blk::Kind::Cobblestone.default_state());
        on_block_pick_request(&mut w.state, s, &pick);
        assert!(w.inv(s).main.stacks.iter().all(Stack::is_empty), "{mode:?}");
    }
}

#[test]
fn a_slot_outside_the_hotbar_is_answered_once_a_tick_with_the_last_selection() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.give(s, Stack::new(item(it::Kind::Stick), 1));
    let equip = |w: &mut World, slot: u8| {
        on_mob_equipment(
            &mut w.state,
            s,
            &MobEquipment {
                entity_runtime_id: 1,
                new_item: ItemInstance::default(),
                inventory_slot: slot,
                hot_bar_slot: slot,
                window_id: 0,
            },
        );
    };
    // The selected slots the end of the tick answered.
    let answers = |w: &mut World| -> Vec<u8> {
        w.tick_delivered(Vec::new())
            .iter()
            .filter_map(|p| match p {
                Packet::MobEquipment(m) => Some(m.hot_bar_slot),
                _ => None,
            })
            .collect()
    };
    // Valid selections in between change the hand but not the answer count.
    for i in 0..50 {
        equip(&mut w, i % 2);
        equip(&mut w, 9);
    }
    assert_eq!(answers(&mut w), [1], "one answer with the last hand");
    assert!(answers(&mut w).is_empty(), "no answer without a request");
    equip(&mut w, 9);
    assert_eq!(answers(&mut w), [1], "the next tick is answered again");
}

#[test]
fn block_picks_past_the_tick_limit_do_nothing_until_the_next_tick() {
    let mut w = World::new(GameMode::Creative);
    let s = w.join(1);
    let pos = AT;
    let shulker = blk::State::all()
        .find(|b| b.is(blk::Class::ShulkerBox))
        .unwrap();
    w.place_block(pos, shulker);
    let pick = BlockPickRequest {
        position: [3, -60, 3],
        add_block_nbt: true,
        hot_bar_slot: 0,
    };
    let picks = |w: &mut World, n: u8| {
        for _ in 0..n {
            on_block_pick_request(&mut w.state, s, &pick);
        }
        w.sent(s)
            .iter()
            .filter(|p| matches!(p, Packet::PlayerHotBar(_)))
            .count()
    };
    assert_eq!(
        picks(&mut w, MAX_PICKS_PER_TICK + 5),
        usize::from(MAX_PICKS_PER_TICK)
    );
    w.state.tick += 1;
    assert_eq!(picks(&mut w, 1), 1);
}

#[test]
fn spawn_packets_are_the_four_windows_and_the_hotbar() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let mut out = Vec::new();
    spawn_packets(w.player_mut(s), &mut out);
    let sizes: Vec<(u32, usize)> = out
        .iter()
        .filter_map(|p| match p {
            Packet::InventoryContent(c) => Some((c.window_id, c.content.len())),
            _ => None,
        })
        .collect();
    assert_eq!(sizes, [(0, 36), (120, 5), (124, 54), (119, 1)]);
    assert_eq!(
        out.last(),
        Some(&Packet::PlayerHotBar(PlayerHotBar {
            selected_hot_bar_slot: 0,
            window_id: 0,
            select_hot_bar_slot: true,
        }))
    );
}

/// An item of every name: every item kind's default type and every block's item.
fn item_samples() -> BTreeMap<String, Stack> {
    let mut by_name = BTreeMap::new();
    let blocks = blk::State::all().filter_map(|b| match b.item() {
        ItemForm::Item(t) => Some(t),
        _ => None,
    });
    for t in it::Kind::all()
        .iter()
        .map(|k| k.default_type())
        .chain(blocks)
    {
        by_name
            .entry(t.name().to_owned())
            .or_insert_with(|| Stack::new(t, 1));
    }
    by_name
}

#[test]
fn death_drops_waiting_at_the_entity_limit_survive_a_restart() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let runtime_id = w.player_mut(s).runtime_id;
    w.tick(vec![initialised(1, 2, runtime_id)]);
    // Distinct items, so none merge: the main slots full and the most waiting stacks.
    let mut items = item_samples()
        .into_values()
        .filter(|st| st.max_stack_size() > Some(1))
        .map(|st| st.with_count(1));
    for st in items.by_ref().take(MAIN) {
        w.give(s, st);
    }
    let pending: Vec<Stack> = items.by_ref().take(MAX_SAVED_PENDING).collect();
    assert_eq!(pending.len(), MAX_SAVED_PENDING);
    w.player_mut(s).inventory.pending = pending;
    let before: u64 = totals(&w.state).values().sum();
    // The world's entity limit: no death drop drops.
    let at = w.player_mut(s).position;
    while entity::drop_item(&mut w.state, 0, at, Stack::new(item(it::Kind::Stick), 1)) {}
    assert!(survival::damage(
        &mut w.state,
        s,
        1000.0,
        survival::Damage::Suicide
    ));
    let held = |extra: &Compound| -> u64 {
        match extra.get("PendingDeathDrops") {
            Some(Value::List(List::Compound(list))) => list
                .iter()
                .map(|nbt| u64::from(Stack::from_nbt(nbt).unwrap().count()))
                .sum(),
            _ => 0,
        }
    };
    let mut saved = Compound::new();
    survival::save(&w.player_mut(s).survival, &mut saved);
    let kept: u64 = totals(&w.state).values().sum();
    assert_eq!(kept + held(&saved), before, "no item is lost on death");
    // A restart loads every held-back stack.
    let mut loaded = survival::Survival::default();
    survival::load(&mut loaded, &saved, &crate::custom::Registry::default());
    let mut again = Compound::new();
    survival::save(&loaded, &mut again);
    assert_eq!(held(&again), held(&saved));
    assert_eq!(again, saved);
}

#[test]
fn placing_over_a_stale_shulker_entity_drops_its_items() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let pos = AT;
    let shulker = blk::State::all()
        .find(|b| b.is(blk::Class::ShulkerBox))
        .unwrap();
    w.place_block(pos, shulker);
    w.give(s, Stack::new(item(it::Kind::Stick), 7));
    w.open_block(s, pos);
    let id = w.id(s, C::HotBar, 0);
    assert!(ok(&w.request(
        s,
        vec![place(7, at(C::HotBar, 0, id), at(C::ShulkerBox, 2, 0))]
    )));
    w.close(s);
    let before = totals(&w.state);
    // The box is set to air outside breaking: its entity stays until the change feed runs, and a
    // box placed there meanwhile replaces it.
    w.state.worlds[0].set_block(pos, blk::AIR).unwrap();
    w.place_block(pos, shulker);
    assert_eq!(dropped(), vec![Stack::new(item(it::Kind::Stick), 7)]);
    assert_eq!(totals(&w.state), before);
}

/// An `InventoryOpen` listener that opens a menu and does not cancel keeps the menu: the window it
/// was asked about does not open in place of it.
#[test]
fn an_open_listener_that_opens_a_menu_keeps_the_menu_in_place_of_the_window() {
    let mut listeners = Listeners::default();
    listeners.push(
        Priority::Normal,
        |e: &mut InventoryOpen, cx: &mut Context<'_>| {
            let menu = crate::menu::Menu::new(crate::menu::Kind::Chest);
            cx.open_menu(e.player(), menu, |_, _, _| crate::menu::Response::Keep)
                .expect("the menu opens");
        },
    );
    let mut w = World::with_listeners(GameMode::Survival, listeners);
    let s = w.join(1);
    let barrel = AT;
    w.place_block(barrel, blk::Kind::Barrel.default_state());
    w.open_block(s, barrel);
    assert!(matches!(
        w.inv(s).window.as_ref().map(|window| window.kind),
        Some(WindowKind::Menu { .. })
    ));
    assert!(w.inv(s).menu.is_some());
    assert!(w.state.rules.block_entities.viewers(0, barrel).is_empty());
}

/// A menu whose blocks cannot stand within reach of the player's eyes (it is far above the world)
/// opens nothing.
#[test]
fn a_menu_opens_nothing_for_a_player_out_of_reach_of_its_blocks() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.player_mut(s).position = Vec3::new(0.5, 400.0, 0.5);
    let menu = crate::menu::Menu::new(crate::menu::Kind::Chest);
    let opened = Context {
        state: &mut w.state,
    }
    .open_menu(crate::player::Id::new(s), menu, |_, _, _| {
        crate::menu::Response::Keep
    });
    assert_eq!(opened, Err(crate::menu::Error::OutsideWorld));
    assert!(w.inv(s).window.is_none());
    assert!(w.inv(s).menu.is_none());
    assert_eq!(w.state.menus.len(), 0);
}

/// A container window closes once its block is replaced, and stays while it is there.
#[test]
fn a_container_window_closes_once_its_block_is_replaced() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let barrel = AT;
    w.place_block(barrel, blk::Kind::Barrel.default_state());
    w.open_block(s, barrel);
    assert!(w.inv(s).window.is_some());
    block_entity::block_changed(&mut w.state, 0, barrel);
    assert!(w.inv(s).window.is_some(), "the barrel is still there");
    w.state.worlds[0]
        .set_block(barrel, blk::Kind::Stone.default_state())
        .unwrap();
    block_entity::block_changed(&mut w.state, 0, barrel);
    assert!(w.inv(s).window.is_none());
    assert!(w.state.rules.block_entities.viewers(0, barrel).is_empty());
}

#[test]
fn the_trade_window_takes_payments_and_gives_them_back_on_close() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.give(s, Stack::new(item(it::Kind::Emerald), 10));
    w.sent(s);
    let window = open_trade(&mut w.state, s, crate::entity::Id::new(1, 0)).unwrap();
    assert_eq!(w.inv(s).window.as_ref().unwrap().id, Some(window));
    assert!(w.sent(s).is_empty(), "the trade module sends UpdateTrade");
    let emeralds = w.id(s, C::CombinedHotBarAndInventory, 0);
    let r = w.request(
        s,
        vec![place(
            4,
            at(C::CombinedHotBarAndInventory, 0, emeralds),
            at(C::TradeTwoIngredientOne, 4, 0),
        )],
    );
    assert!(ok(&r));
    let before = totals(&w.state);
    // No such offer (the trade module offers none here): refused.
    let id = w.next_id(s);
    let pay = w.id(s, C::TradeTwoIngredientOne, 4);
    let actions = vec![
        craft(5000, 1),
        consume(4, at(C::TradeTwoIngredientOne, 4, pay)),
        place(1, created(id), at(C::CombinedHotBarAndInventory, 3, 0)),
    ];
    assert!(!ok(&w.request_as(s, id, actions)));
    w.client_close(s, window);
    assert_eq!(totals(&w.state), before);
    assert_eq!(w.inv(s).main.stack(0).count(), 10);
    assert!(w.inv(s).window.is_none());
}

#[test]
fn closing_leaving_or_dying_with_a_trade_payment_returns_every_item() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    w.give(s, Stack::new(item(it::Kind::Emerald), 1));
    open_trade(&mut w.state, s, crate::entity::Id::new(1, 0)).unwrap();
    let emeralds = w.id(s, C::CombinedHotBarAndInventory, 0);
    let r = w.request(
        s,
        vec![place(
            1,
            at(C::CombinedHotBarAndInventory, 0, emeralds),
            at(C::TradeTwoIngredientOne, 4, 0),
        )],
    );
    assert!(ok(&r));
    let before = totals(&w.state);
    // Leaving and dying close the window this way first (`State::remove`, the death rules).
    close_window(&mut w.state, s);
    w.ack(s);
    assert_eq!(totals(&w.state), before);
    assert_eq!(w.inv(s).main.stack(0).count(), 1);
    assert!(dropped().is_empty());
}

#[test]
fn opening_and_closing_every_window_a_hundred_times_leaves_nothing_behind() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let containers = [
        blk::Kind::Chest,
        blk::Kind::Barrel,
        blk::Kind::EnderChest,
        blk::Kind::ShulkerBox,
    ];
    let positions: Vec<BlockPos> = (0..4).map(|i| BlockPos::new(-6 + i * 2, -60, 6)).collect();
    for (kind, pos) in containers.into_iter().zip(&positions) {
        w.place_block(*pos, kind.default_state());
    }
    let start = w.state.rules.block_entities.sizes();
    for _ in 0..100 {
        for pos in &positions {
            w.open_block(s, *pos);
            w.close(s);
            w.ack(s);
        }
        w.open_inventory(s);
        w.close(s);
        w.ack(s);
    }
    assert!(w.inv(s).window.is_none() && w.inv(s).pending_close.is_none());
    assert_eq!(w.state.rules.block_entities.sizes(), start);
    w.tick(Vec::new());
    assert_eq!(
        w.state.rules.block_entities.sizes().viewers,
        0,
        "no viewers"
    );
}

#[test]
fn breaking_a_container_drops_its_contents_once() {
    let stick = Stack::new(item(it::Kind::Stick), 1);
    for kind in [blk::Kind::Chest, blk::Kind::Barrel] {
        let mut w = World::new(GameMode::Survival);
        let pos = AT;
        w.place_block(pos, kind.default_state());
        let slots = {
            let e = w.state.rules.block_entities.get_mut(0, pos).unwrap();
            for s in &mut e.items {
                *s = stick.clone();
            }
            e.items.len()
        };
        assert!(slots > 0, "{kind:?}");
        // The block's own item may drop too: only the sticks are counted.
        let sticks = || dropped().into_iter().filter(|s| *s == stick).count();
        let cause = || Destroy::Explosion {
            size: 4.0,
            raise: Raise::Quiet,
        };
        assert!(interact::destroy(&mut w.state, 0, pos, &cause()));
        assert_eq!(sticks(), slots, "{kind:?}");
        assert!(!interact::destroy(&mut w.state, 0, pos, &cause()));
        assert_eq!(sticks(), slots, "{kind:?}: once");
        assert!(w.state.rules.block_entities.get(0, pos).is_none());
    }
}

#[test]
fn container_block_entities_survive_chunk_unload_and_reload_unchanged() {
    let mut w = World::new(GameMode::Survival);
    let chest = AT;
    let barrel = BlockPos::new(4, -60, 3);
    w.place_block(chest, blk::Kind::Chest.default_state());
    w.place_block(barrel, blk::Kind::Barrel.default_state());
    w.state
        .rules
        .block_entities
        .get_mut(0, chest)
        .unwrap()
        .items[0] = Stack::new(item(it::Kind::Stick), 3);
    let key = (0, chest.chunk());
    let before: BTreeMap<_, _> = [chest, barrel]
        .into_iter()
        .map(|p| (p, w.state.rules.block_entities.get(0, p).cloned()))
        .collect();
    let list = w.state.rules.block_entities.unload(key).unwrap();
    assert!(w.state.rules.block_entities.get(0, chest).is_none());
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
fn beds_and_banners_keep_their_item_data() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let place_with =
        |w: &mut World, pos: BlockPos, block: blk::State, stack: &Stack, face: blk::Face| {
            w.state.worlds[0].set_block(pos, block).unwrap();
            block_entity::placed(&mut w.state, 0, pos, Some(s), stack, face);
        };
    let named_meta =
        |name: &str, meta: u16| Type::from_name(name, meta, None, &Compound::new()).unwrap();
    let blue_bed = Stack::new(named_meta("minecraft:bed", 11), 1);
    let bed = AT;
    place_with(
        &mut w,
        bed,
        blk::Kind::Bed.default_state(),
        &blue_bed,
        blk::Face::Up,
    );
    assert_eq!(block_entity::own_item(&w.state, 0, bed), Some(blue_bed));
    let mut tag = Compound::new();
    tag.insert(
        "Patterns".to_owned(),
        Value::List(List::Compound(vec![Compound::from([
            ("Pattern".to_owned(), Value::String("bo".to_owned())),
            ("Color".to_owned(), Value::Int(3)),
        ])])),
    );
    let banner_type = named_meta("minecraft:banner", 4);
    let banner = Stack::from_parts(banner_type, 1, 4, Some(tag)).unwrap();
    let standing_banner = BlockPos::new(5, -60, 3);
    let standing = blk::State::all()
        .find(|b| b.name() == "minecraft:standing_banner")
        .unwrap();
    place_with(&mut w, standing_banner, standing, &banner, blk::Face::Up);
    assert_eq!(
        block_entity::own_item(&w.state, 0, standing_banner),
        Some(banner)
    );
}
