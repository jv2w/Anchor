//! Tests of inventories, windows and item stack requests: item conservation and crafting.

mod api;
mod events;
mod menu;

use std::cell::Cell;
use std::collections::BTreeMap;

use tokio::sync::mpsc::Receiver;

use super::*;
use crate::block::{self as blk, ItemForm};
use crate::config::GameMode;
use crate::crafting::{self, Ingredient, Recipe, ShapelessType};
use crate::event::{InventoryOpen, Listeners, Priority};
use crate::item::{self as it, Enchantment, Extra, Type};
use crate::pool::Pool;
use crate::protocol::consts::{ContainerId as C, ItemStackResponseStatus};
use crate::protocol::packet::{ContainerClose, Interact, Packet};
use crate::protocol::{
    AutoCraftRecipeStackRequestAction, ConsumeStackRequestAction, CraftCreativeStackRequestAction,
    CraftRecipeStackRequestAction, CraftResultsDeprecatedStackRequestAction,
    DestroyStackRequestAction, DropStackRequestAction, FullContainerName, ItemStackRequest,
    ItemStackResponse, MineBlockStackRequestAction, PlaceStackRequestAction, StackRequestAction,
    StackRequestSlotInfo, SwapStackRequestAction, TakeStackRequestAction,
    TransferStackRequestAction,
};
use crate::random::Random;
use crate::save::Response;
use crate::tick::queue::{Input, Message, Outbound};
use crate::tick::tests::{answer, join, new_state, radius, settings};
use crate::tick::{
    Context, InputLog, Output, Player, SessionId, State, block_entity, boundary, deliver, step,
};

/// The container of the player's combined hotbar and inventory slots.
const COMBINED: u8 = C::CombinedHotBarAndInventory as u8;
/// The container of the 2x2 crafting grid.
const CRAFT: u8 = C::CraftingInput as u8;

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
            let world = &state.worlds[0];
            let read = state
                .rules
                .read((0, pos), &extras, world.chunk(pos).unwrap(), world);
            state.rules.load((0, pos), read, &extras);
        }
        Self {
            state,
            pool: crate::runtime::cpu_pool(1).unwrap(),
            answers: Vec::new(),
            receivers: Vec::new(),
            next_request: BTreeMap::new(),
        }
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
                        Message::Shared(_) => None,
                    }));
                }
            }
            delivered.push(packets);
        }
        delivered
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

    pub(super) fn player_mut(&mut self, s: SessionId) -> &mut Player {
        self.state.players.get_mut(&s).unwrap()
    }

    pub(super) fn inv(&self, s: SessionId) -> &Inventory {
        &self.state.players[&s].inventory
    }

    /// Takes the packets queued for `s`.
    pub(super) fn sent(&mut self, s: SessionId) -> Vec<Packet> {
        std::mem::take(&mut self.player_mut(s).outbox)
    }

    pub(super) fn next_id(&mut self, s: SessionId) -> i32 {
        let next = self.next_request.entry(s).or_insert(-1);
        let id = *next;
        *next -= 2;
        id
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

    pub(super) fn give(&mut self, s: SessionId, stack: Stack) {
        let left = give(self.player_mut(s), stack);
        assert!(left.is_empty());
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
        if let Some(window) = inv.window.as_ref().filter(|w| w.id != 0)
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

    pub(super) fn open_block(&mut self, s: SessionId, pos: BlockPos) {
        assert!(open_block(&mut self.state, s, 0, pos));
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

    pub(super) fn close(&mut self, s: SessionId) {
        if let Some(id) = self.inv(s).window.as_ref().map(|w| w.id) {
            self.client_close(s, id);
        }
    }

    pub(super) fn place_block(&mut self, pos: BlockPos, block: blk::State) {
        self.state.worlds[0].set_block(pos, block).unwrap();
        block_entity::placed(&mut self.state, 0, pos, None, &Stack::EMPTY, blk::Face::Up);
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

fn deprecated() -> StackRequestAction {
    StackRequestAction::CraftResultsDeprecated(CraftResultsDeprecatedStackRequestAction::default())
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

pub(super) fn ok(r: &ItemStackResponse) -> bool {
    r.status == ItemStackResponseStatus::Ok as u8
}

/// The crafting recipes in network ID order with their ingredients and their one result.
fn crafting_recipes() -> Vec<(u32, Recipe<'static>)> {
    (1..)
        .map_while(|id| crafting::recipe(id).map(|r| (id, r)))
        .filter(|(_, r)| match r {
            Recipe::Shaped(_) => true,
            Recipe::Shapeless(s) => s.kind == ShapelessType::Crafting,
            _ => false,
        })
        .collect()
}

fn ingredients(r: Recipe<'static>) -> Vec<Ingredient> {
    match r {
        Recipe::Shaped(s) => s.ingredients().copied().collect(),
        Recipe::Shapeless(s) => s.ingredients.to_vec(),
        _ => Vec::new(),
    }
}

fn results(r: Recipe<'static>) -> Vec<Stack> {
    let list = match r {
        Recipe::Shaped(s) => s.results,
        Recipe::Shapeless(s) => s.results,
        _ => &[],
    };
    list.iter().map(|s| s.to_item().unwrap()).collect()
}

/// The lowest recipe ID making `result` from only `allowed` items (the bot's `recipeFor`).
fn recipe_for(result: Type, allowed: &[Type]) -> (u32, Recipe<'static>) {
    let allowed: Vec<Stack> = allowed.iter().map(|t| Stack::new(*t, 1)).collect();
    crafting_recipes()
        .into_iter()
        .find(|(_, r)| {
            let res = results(*r);
            res.len() == 1
                && res[0].item() == Some(result)
                && ingredients(*r)
                    .iter()
                    .all(|i| allowed.iter().any(|a| i.can_accept(a)))
        })
        .expect("a recipe")
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

/// `recipe-book = false`: the recipe book's quick crafting is refused and changes nothing; the
/// grid still crafts.
#[test]
fn a_hidden_recipe_book_refuses_its_quick_crafting() {
    let mut w = World::new(GameMode::Survival);
    let mut custom = crate::custom::Registry::default();
    custom.hide_recipe_book();
    custom.finish().unwrap();
    w.state.custom = std::sync::Arc::new(custom);
    let s = w.join(1);
    let (log, plank) = (
        block_item(blk::Kind::OakLog),
        block_item(blk::Kind::OakPlanks),
    );
    let (planks, _) = recipe_for(plank, &[log]);
    w.give(s, Stack::new(log, 4));
    w.open_inventory(s);
    w.sent(s);
    let l = w.id(s, C::CombinedHotBarAndInventory, 0);
    let id = w.next_id(s);
    let r = w.request_as(
        s,
        id,
        vec![
            auto(planks, 1),
            deprecated(),
            consume(1, at(C::CombinedHotBarAndInventory, 0, l)),
            place(4, created(id), at(C::CombinedHotBarAndInventory, 1, 0)),
        ],
    );
    assert!(!ok(&r), "the recipe book is refused");
    assert_eq!(w.slot(s, C::CombinedHotBarAndInventory, 0).stack.count(), 4);
    let r = w.request(
        s,
        vec![place(
            4,
            at(C::CombinedHotBarAndInventory, 0, l),
            at(C::CraftingInput, 28, 0),
        )],
    );
    assert!(ok(&r));
    let id = w.next_id(s);
    let r = w.request_as(
        s,
        id,
        vec![
            craft(planks, 1),
            deprecated(),
            consume(1, at(C::CraftingInput, 28, l)),
            place(4, created(id), at(C::CombinedHotBarAndInventory, 0, 0)),
        ],
    );
    assert!(ok(&r), "the grid crafts");
}

#[test]
#[expect(
    clippy::many_single_char_names,
    reason = "consistency: the IDs keep the short names of the P6 craft record (l logs, p planks)"
)]
#[expect(
    clippy::too_many_lines,
    reason = "consistency: the fixture follows the BDS scenario step by step"
)]
fn stack_network_ids_and_responses_follow_the_bds_craft_scenario() {
    // P6 `craft`, as the bot plays it against BDS (`xtask/data/p6/facts.md`).
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let (log, plank, stick) = (
        block_item(blk::Kind::OakLog),
        block_item(blk::Kind::OakPlanks),
        item(it::Kind::Stick),
    );
    let table_item = block_item(blk::Kind::CraftingTable);
    let (planks, _) = recipe_for(plank, &[log]);
    let (table, _) = recipe_for(table_item, &[plank]);
    let (sticks, _) = recipe_for(stick, &[plank]);
    let (pickaxe, _) = recipe_for(item(it::Kind::WoodenPickaxe), &[plank, stick]);
    w.give(s, Stack::new(log, 4));
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
    // The logs into the grid: the whole stack keeps its ID.
    let l = w.id(s, C::CombinedHotBarAndInventory, 0);
    let r = w.request(
        s,
        vec![place(
            4,
            at(C::CombinedHotBarAndInventory, 0, l),
            at(C::CraftingInput, 28, 0),
        )],
    );
    assert_eq!(shape(&r), [(COMBINED, 0, 0, 0), (CRAFT, 28, 4, l)]);
    // Planks four times: the grid keeps its ID; the result gets a new one, then merges.
    let mut p = 0;
    for n in 1..=4_u8 {
        let id = w.next_id(s);
        let r = w.request_as(
            s,
            id,
            vec![
                craft(planks, 1),
                deprecated(),
                consume(1, at(C::CraftingInput, 28, l)),
                place(4, created(id), at(C::CombinedHotBarAndInventory, 0, p)),
            ],
        );
        let left = if n < 4 { l } else { 0 };
        if n == 1 {
            p = shape(&r)[1].3;
            assert!(p > l);
        }
        assert_eq!(
            shape(&r),
            [(CRAFT, 28, 4 - n, left), (COMBINED, 0, 4 * n, p)]
        );
    }
    // One plank into each grid slot: part of a stack into an empty slot gets a new ID.
    let fill = |w: &mut World, cells: &[u8], from: u8, item_id: i32| {
        let mut ids = Vec::new();
        for &c in cells {
            let have = w.slot(s, C::CombinedHotBarAndInventory, from).stack.count();
            let r = w.request(
                s,
                vec![place(
                    1,
                    at(C::CombinedHotBarAndInventory, from, item_id),
                    at(C::CraftingInput, c, 0),
                )],
            );
            let shape = shape(&r);
            assert_eq!(shape[0], (COMBINED, from, have - 1, item_id));
            assert_eq!((shape[1].0, shape[1].1, shape[1].2), (CRAFT, c, 1));
            assert!(shape[1].3 > item_id && !ids.contains(&shape[1].3));
            ids.push(shape[1].3);
        }
        ids
    };
    let mut tables = Vec::new();
    for _ in 0..2 {
        let cells = fill(&mut w, &[28, 29, 30, 31], 0, p);
        let id = w.next_id(s);
        let dst = w.inv(s).first_empty().unwrap();
        let mut actions = vec![craft(table, 1), deprecated()];
        actions.extend(
            (0..4).map(|i| consume(1, at(C::CraftingInput, 28 + i, cells[usize::from(i)]))),
        );
        actions.push(place(
            1,
            created(id),
            at(C::CombinedHotBarAndInventory, dst, 0),
        ));
        let r = w.request_as(s, id, actions);
        let shape = shape(&r);
        assert_eq!(
            shape[..4],
            [
                (CRAFT, 28, 0, 0),
                (CRAFT, 29, 0, 0),
                (CRAFT, 30, 0, 0),
                (CRAFT, 31, 0, 0)
            ]
        );
        assert_eq!((shape[4].0, shape[4].1, shape[4].2), (COMBINED, dst, 1));
        tables.push(shape[4].3);
    }
    assert_ne!(tables[0], tables[1]);
    // Sticks from the recipe book: two consumes of the plank slot under its unchanged ID.
    let id = w.next_id(s);
    let dst = w.inv(s).first_empty().unwrap();
    let r = w.request_as(
        s,
        id,
        vec![
            auto(sticks, 1),
            deprecated(),
            consume(1, at(C::CombinedHotBarAndInventory, 0, p)),
            consume(1, at(C::CombinedHotBarAndInventory, 0, p)),
            place(4, created(id), at(C::CombinedHotBarAndInventory, dst, 0)),
        ],
    );
    let shape_sticks = shape(&r);
    assert_eq!(shape_sticks[0], (COMBINED, 0, 6, p));
    assert_eq!(shape_sticks[1].2, 4);
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
    // A crafting table: window 3, type 1, at the block.
    let pos = BlockPos::new(2, -60, 0);
    w.place_block(pos, blk::Kind::CraftingTable.default_state());
    w.open_block(s, pos);
    let open = w.sent(s);
    assert!(open.contains(&Packet::ContainerOpen(ContainerOpen {
        window_id: 3,
        container_type: 1,
        container_position: [2, -60, 0],
        container_entity_unique_id: -1,
    })));
    let plank_cells = fill(&mut w, &[32, 33, 34], 0, p);
    let stick_slot = shape_sticks[1].1;
    let stick_id = shape_sticks[1].3;
    let stick_cells = {
        let mut ids = Vec::new();
        for c in [36, 39] {
            let r = w.request(
                s,
                vec![place(
                    1,
                    at(C::CombinedHotBarAndInventory, stick_slot, stick_id),
                    at(C::CraftingInput, c, 0),
                )],
            );
            ids.push(shape(&r)[1].3);
        }
        ids
    };
    let id = w.next_id(s);
    let dst = w.inv(s).first_empty().unwrap();
    let mut actions = vec![craft(pickaxe, 1), deprecated()];
    for (c, i) in [32, 33, 34].into_iter().zip(&plank_cells) {
        actions.push(consume(1, at(C::CraftingInput, c, *i)));
    }
    for (c, i) in [36, 39].into_iter().zip(&stick_cells) {
        actions.push(consume(1, at(C::CraftingInput, c, *i)));
    }
    actions.push(place(
        1,
        created(id),
        at(C::CombinedHotBarAndInventory, dst, 0),
    ));
    let r = w.request_as(s, id, actions);
    let shape = shape(&r);
    assert_eq!(shape.len(), 6, "five grid slots and the destination");
    assert!(
        shape[..5]
            .iter()
            .all(|(c, _, n, i)| *c == CRAFT && *n == 0 && *i == 0)
    );
    w.close(s);
    assert_eq!(
        w.inv(s).main.stack(dst).item(),
        Some(item(it::Kind::WoodenPickaxe))
    );
}

/// What tells the items of the totals apart: the type, the damage and the extra data.
type TotalKey = (Option<Type>, u16, String);

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
            .chain(&inv.grid.stacks)
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

/// The key of `s` in the totals.
fn total_key(s: &Stack) -> TotalKey {
    let extra = s.extra().map(|e| format!("{e:?}")).unwrap_or_default();
    (s.item(), s.damage(), extra)
}

/// Every slot and stack network ID of a player.
type PlayerSnapshot = (
    Slots<MAIN>,
    Slots<ARMOR>,
    Slots<1>,
    Slots<1>,
    Slots<GRID>,
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
                inv.grid.clone(),
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

/// No slot holds more than its stack size, and a slot is empty exactly when its ID is 0.
pub(super) fn check_slots(state: &State) {
    let check = |s: &Stack, id: Option<StackId>| {
        assert!(s.count() <= s.max_count(), "{s:?}");
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
            (&inv.grid.stacks[..], &inv.grid.ids[..]),
            (&inv.ender.stacks[..], &inv.ender.ids[..]),
        ] {
            for (s, id) in stacks.iter().zip(ids) {
                check(s, Some(*id));
            }
        }
        for (i, s) in inv.armor.stacks.iter().enumerate() {
            if let Some(a) = s.item().and_then(Type::armor) {
                assert_eq!(a.slot() as usize, i);
            }
        }
        if let Some(w) = &inv.window {
            for s in &w.ui {
                check(&s.stack, Some(s.id));
            }
            if w.id != 0 && matches!(w.kind, WindowKind::Container { .. }) {
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
                if w.id != 0 {
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
            .chain(&inv.grid.stacks)
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
    let window = inv.window.as_ref().filter(|w| w.id != 0);
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
    out.extend((28..32).map(|i| (C::CraftingInput, i)));
    if let Some(WindowKind::Menu { kind, .. }) = window.map(|w| w.kind) {
        let size = u8::try_from(kind.size()).unwrap();
        out.extend((0..size).map(|i| (C::LevelEntity, i)));
    }
    if let Some(w) = window {
        for p in w.kind.parts() {
            let c = C::ALL
                .into_iter()
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

/// What a successful request adds to and takes from the item totals: created items placed or
/// dropped from the created output, minus what consume and destroy actions take.
fn expected_change(
    w: &World,
    s: SessionId,
    actions: &[StackRequestAction],
) -> BTreeMap<TotalKey, i64> {
    let mut out: BTreeMap<_, i64> = BTreeMap::new();
    let mut made: Option<Stack> = None;
    let mut outputs: Vec<Stack> = Vec::new();
    let is_created = |i: &StackRequestSlotInfo| {
        i.container.container_id == C::CreatedOutput as u8 && i.slot == UI_CREATED_OUTPUT
    };
    for a in actions {
        match a {
            StackRequestAction::CraftRecipe(CraftRecipeStackRequestAction {
                recipe_network_id,
                ..
            })
            | StackRequestAction::AutoCraftRecipe(AutoCraftRecipeStackRequestAction {
                recipe_network_id,
                ..
            }) => {
                outputs = crafting::recipe(*recipe_network_id)
                    .map(results)
                    .unwrap_or_default();
                made = outputs.first().cloned().filter(|_| outputs.len() == 1);
            }
            StackRequestAction::Create(c) => {
                made = outputs.get(usize::from(c.results_slot)).cloned();
            }
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
                let c = C::ALL
                    .into_iter()
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

/// Builds a random request for `s`: moves, swaps, drops, destroys, crafts (by hand from a grid
/// laid out by earlier requests, or from the recipe book), creative items; valid and invalid.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one generator keeps the request mix in one place"
)]
fn random_request(
    w: &World,
    s: SessionId,
    rng: &mut Random,
    request: i32,
    recipes: &[(u32, Recipe<'static>)],
) -> Vec<StackRequestAction> {
    let inv = w.inv(s);
    let slots = addressable(inv, rng);
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
    match rng.below(20) {
        0..=2 => {
            // Craft by hand from whatever the grid holds: the recipe of the grid, if any.
            let workbench = inv
                .window
                .as_ref()
                .is_some_and(|w| w.id != 0 && w.kind.is_workbench());
            let (cells, side): (Vec<(C, u8)>, usize) = if workbench {
                ((32..41).map(|i| (C::CraftingInput, i)).collect(), 3)
            } else {
                ((28..32).map(|i| (C::CraftingInput, i)).collect(), 2)
            };
            let grid: Vec<Stack> = cells.iter().map(|(c, i)| w.slot(s, *c, *i).stack).collect();
            let found = recipes.iter().find(|(_, r)| match r {
                Recipe::Shaped(r) => r.matches(&grid, side),
                Recipe::Shapeless(r) => r.matches(&grid),
                _ => false,
            });
            let (id, recipe) = match found {
                Some(f) if rng.below(8) != 0 => *f,
                _ => recipes[rng.below(u32::try_from(recipes.len()).unwrap()) as usize],
            };
            let lowest = grid
                .iter()
                .filter(|g| !g.is_empty())
                .map(Stack::count)
                .min()
                .unwrap_or(1);
            let times = if rng.below(4) == 0 {
                lowest.saturating_add(1)
            } else {
                1 + small(rng.below(u32::from(lowest)))
            };
            actions.push(craft(id, times));
            actions.push(deprecated());
            for ((c, i), g) in cells.iter().zip(&grid) {
                if !g.is_empty() {
                    actions.push(consume(times, name(w, s, (*c, *i), rng, request)));
                }
            }
            let result = &results(recipe)[0];
            let total = u32::from(result.count()) * u32::from(times);
            let mut left = total;
            let mut empty = inv
                .main
                .stacks
                .iter()
                .enumerate()
                .filter(|(_, s)| s.is_empty())
                .map(|(i, _)| u8::try_from(i).unwrap());
            while left > 0 {
                let n = small(left.min(u32::from(result.max_count())));
                match empty.next() {
                    Some(e) if rng.below(4) != 0 => actions.push(place(
                        n,
                        created(request),
                        at(C::CombinedHotBarAndInventory, e, 0),
                    )),
                    _ => {
                        let dst = pick(rng, &slots);
                        actions.push(place(n, created(request), name(w, s, dst, rng, request)));
                    }
                }
                left -= u32::from(n);
            }
        }
        3 => {
            // The recipe book: consumes from inventory slots holding something the recipe takes.
            let (id, recipe) = recipes[rng.below(u32::try_from(recipes.len()).unwrap()) as usize];
            actions.push(auto(id, 1));
            for ing in ingredients(recipe) {
                if let Some(&(c, i)) = full
                    .iter()
                    .find(|(c, i)| ing.can_accept(&w.slot(s, *c, *i).stack))
                {
                    actions.push(consume(1, name(w, s, (c, i), rng, request)));
                }
            }
            let result = &results(recipe)[0];
            actions.push(place(
                result.count(),
                created(request),
                name(w, s, pick(rng, &slots), rng, request),
            ));
        }
        4..=6 => actions = lay_out(w, s, rng, request, recipes),
        7 => {
            let id = 1 + rng.below(1700);
            actions.push(creative(id));
            let count = 1 + small(rng.below(70));
            actions.push(place(
                count,
                created(request),
                name(w, s, pick(rng, &slots), rng, request),
            ));
        }
        _ => {
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
        }
    }
    actions
}

/// A request laying a random recipe out in the open crafting grid from the inventory: what the
/// grid holds goes to empty slots first, then one accepted item into each recipe cell.
fn lay_out(
    w: &World,
    s: SessionId,
    rng: &mut Random,
    request: i32,
    recipes: &[(u32, Recipe<'static>)],
) -> Vec<StackRequestAction> {
    let inv = w.inv(s);
    let workbench = inv
        .window
        .as_ref()
        .is_some_and(|w| w.id != 0 && w.kind.is_workbench());
    let (first, side) = if workbench {
        (32_u8, 3_usize)
    } else {
        (28_u8, 2_usize)
    };
    let fitting: Vec<&(u32, Recipe<'static>)> = recipes
        .iter()
        .filter(|(_, r)| match r {
            Recipe::Shaped(r) => usize::from(r.width) <= side && usize::from(r.height) <= side,
            Recipe::Shapeless(r) => r.ingredients.len() <= side * side,
            _ => false,
        })
        .collect();
    let (_, recipe) = *fitting[rng.below(u32::try_from(fitting.len()).unwrap()) as usize];
    let wanted: Vec<(u8, Ingredient)> = match recipe {
        Recipe::Shaped(r) => (0..usize::from(r.height))
            .flat_map(|y| (0..usize::from(r.width)).map(move |x| (x, y)))
            .filter_map(|(x, y)| {
                let i = r.grid[y * usize::from(r.width) + x]?;
                Some((first + u8::try_from(y * side + x).unwrap(), i))
            })
            .collect(),
        Recipe::Shapeless(r) => r
            .ingredients
            .iter()
            .enumerate()
            .map(|(k, i)| (first + u8::try_from(k).unwrap(), *i))
            .collect(),
        _ => Vec::new(),
    };
    let main = |i: u8| {
        at(
            C::CombinedHotBarAndInventory,
            i,
            inv.main.ids[usize::from(i)].net,
        )
    };
    let mut empties: Vec<u8> = (0..36_u8)
        .filter(|i| inv.main.stack(*i).is_empty())
        .rev()
        .collect();
    let mut left: Vec<u8> = inv.main.stacks.iter().map(Stack::count).collect();
    let mut actions = Vec::new();
    let mut cleared = Vec::new();
    for cell in first..first + u8::try_from(side * side).unwrap() {
        let there = w.slot(s, C::CraftingInput, cell);
        if there.stack.is_empty() {
            continue;
        }
        let Some(e) = empties.pop() else {
            return actions;
        };
        actions.push(place(
            there.stack.count(),
            at(C::CraftingInput, cell, there.id.net),
            at(C::CombinedHotBarAndInventory, e, 0),
        ));
        cleared.push(cell);
    }
    for (cell, ingredient) in wanted {
        let Some(k) = (0..36_u8)
            .find(|k| left[usize::from(*k)] > 0 && ingredient.can_accept(inv.main.stack(*k)))
        else {
            return actions;
        };
        left[usize::from(k)] -= 1;
        let id = if cleared.contains(&cell) { request } else { 0 };
        actions.push(place(1, main(k), at(C::CraftingInput, cell, id)));
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
    let chest = BlockPos::new(3, -60, 3);
    w.double_chest(chest);
    let barrel = BlockPos::new(-3, -60, 3);
    w.place_block(barrel, blk::Kind::Barrel.default_state());
    let bench = BlockPos::new(3, -60, -3);
    w.place_block(bench, blk::Kind::CraftingTable.default_state());
    let ender = BlockPos::new(-3, -60, -3);
    w.place_block(ender, blk::Kind::EnderChest.default_state());
    for p in players {
        stock(&mut w, p);
    }
    let supplies = [
        block_item(blk::Kind::OakLog),
        block_item(blk::Kind::OakPlanks),
        item(it::Kind::Stick),
        block_item(blk::Kind::Cobblestone),
    ];
    let recipes: Vec<(u32, Recipe<'static>)> = crafting_recipes()
        .into_iter()
        .filter(|(_, r)| {
            results(*r).len() == 1
                && ingredients(*r)
                    .iter()
                    .all(|i| supplies.iter().any(|t| i.can_accept(&Stack::new(*t, 1))))
        })
        .collect();
    assert!(recipes.len() >= 8, "{} recipes", recipes.len());
    let mut rng = Random::new(seed, 0, 7);
    let (mut oks, mut errors, mut crafts, mut drops) = (0_u32, 0_u32, 0_u32, 0_u32);
    let mut last: Option<(SessionId, i32, Vec<StackRequestAction>)> = None;
    for n in 0..count {
        let s = players[rng.below(2) as usize];
        set_room(if rng.below(10) == 0 { 0 } else { usize::MAX });
        let before = totals(&w.state);
        let snap = snapshot(&w.state);
        match rng.below(100) {
            0..=2 => {
                match rng.below(8) {
                    0 => w.open_inventory(s),
                    1 => w.open_block(s, chest),
                    2 => w.open_block(s, barrel),
                    3 => w.open_block(s, bench),
                    4 => w.open_block(s, ender),
                    5 | 6 => open_model_menu(&mut w, s, rng.below(3)),
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
                let actions = random_request(&w, s, &mut rng, id, &recipes);
                let expected = expected_change(&w, s, &actions);
                let crafting = actions.iter().any(|a| {
                    matches!(
                        a,
                        StackRequestAction::CraftRecipe(_) | StackRequestAction::AutoCraftRecipe(_)
                    )
                });
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
                    crafts += u32::from(crafting);
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
    assert!(crafts > 100, "{crafts} crafts");
    assert!(drops > 100, "{drops} drops");
    let clicks = MENU_CLICKS.take();
    assert!(clicks > 20, "{clicks} menu clicks");
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

#[test]
fn random_requests_keep_every_item_accounted_for() {
    run_model(GameMode::Survival, 1, 50_000, None);
    run_model(GameMode::Creative, 2, 50_000, None);
}

// pmmp: InventoryTransactionEvent, CraftItemEvent, PlayerDropItemEvent
#[test]
fn random_requests_with_random_listeners_keep_every_item_accounted_for() {
    // Listeners from a fixed seed cancel, change fields or move items themselves through the
    // Context; no item is made or lost but by the requests' own crafts and drops.
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
            at(C::CraftingInput, 28, 0),
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

/// Fills the grid, the crafting table grid and the cursor, then `leave` happens.
fn mid_craft(w: &mut World, s: SessionId) {
    let pos = BlockPos::new(2, -60, 0);
    w.place_block(pos, blk::Kind::CraftingTable.default_state());
    w.give(s, Stack::new(block_item(blk::Kind::OakPlanks), 64));
    w.open_block(s, pos);
    let p = w.id(s, C::CombinedHotBarAndInventory, 0);
    let mut moves = vec![place(
        5,
        at(C::CombinedHotBarAndInventory, 0, p),
        at(C::Cursor, 0, 0),
    )];
    moves.extend((32..41).map(|c| {
        place(
            2,
            at(C::CombinedHotBarAndInventory, 0, p),
            at(C::CraftingInput, c, 0),
        )
    }));
    moves.extend((28..32).map(|c| {
        place(
            1,
            at(C::CombinedHotBarAndInventory, 0, p),
            at(C::CraftingInput, c, 0),
        )
    }));
    assert!(ok(&w.request(s, moves)));
    assert_eq!(w.inv(s).main.stack(0).count(), 64 - 5 - 18 - 4);
}

#[test]
fn closing_leaving_or_dying_mid_craft_returns_every_item() {
    // Close, with room: everything comes back.
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    mid_craft(&mut w, s);
    let before = totals(&w.state);
    w.close(s);
    assert_eq!(totals(&w.state), before);
    assert_eq!(w.inv(s).main.stack(0).count(), 64);
    assert!(dropped().is_empty());

    // A full inventory: the rest drops; with no room in the world it waits, and comes back as
    // soon as a slot frees.
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    mid_craft(&mut w, s);
    for _ in 1..36 {
        w.give(s, Stack::new(block_item(blk::Kind::Cobblestone), 64));
    }
    w.give(s, Stack::new(block_item(blk::Kind::OakPlanks), 27));
    assert!(w.inv(s).first_empty().is_none(), "the inventory is full");
    let before = totals(&w.state);
    set_room(1);
    close_window(&mut w.state, s);
    w.ack(s);
    assert_eq!(totals(&w.state), before, "nothing duplicated or lost");
    assert_eq!(dropped().len(), 1);
    let waiting = w.inv(s).pending.len();
    assert_eq!(waiting, 13, "the cursor dropped; the 13 grid stacks wait");
    let mut saved = Compound::new();
    save(w.inv(s), &mut saved);
    let mut back = Inventory::default();
    load(&mut back, &saved, &crate::custom::Registry::default());
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
    mid_craft(&mut w, s);
    close_window(&mut w.state, s);
    let all = take_all(w.player_mut(s));
    assert_eq!(all.iter().map(|s| u64::from(s.count())).sum::<u64>(), 64);
    assert!(totals(&w.state).is_empty());
}

#[test]
fn the_most_waiting_stacks_lose_no_item_and_holding_more_is_refused() {
    // A full inventory, a world with no room for drops, the cursor and the 2x2 grid held, and as
    // many waiting stacks as the bound leaves (a load brings up to the bound).
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    for _ in 0..36 {
        w.give(s, Stack::new(block_item(blk::Kind::Cobblestone), 64));
    }
    let stick = Stack::new(item(it::Kind::Stick), 1);
    let held = 1 + GRID;
    let inv = &mut w.player_mut(s).inventory;
    inv.pending = vec![stick.clone(); MAX_SAVED_PENDING - held];
    for r in std::iter::once(SlotRef::Cursor).chain((0..4).map(SlotRef::Grid)) {
        let net = inv.new_id();
        let id = StackId { net, request: None };
        assert!(inv.put(
            r,
            Slot {
                stack: stick.clone(),
                id
            }
        ));
    }
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
    mid_craft(&mut w, s);
    close_window(&mut w.state, s);
    let mut extra = Compound::new();
    save(w.inv(s), &mut extra);
    let mut back = Inventory::default();
    load(&mut back, &extra, &crate::custom::Registry::default());
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
fn a_save_holds_the_cursor_grids_and_window_slots_once_and_a_load_gives_them_back() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    // 64 planks: 37 in the main slots, 5 on the cursor, 4 in the 2x2 grid, 18 in the crafting
    // table's grid.
    mid_craft(&mut w, s);
    let mut saved = Compound::new();
    save(w.inv(s), &mut saved);
    assert_eq!(saved_count(&saved), 64, "every plank once");
    let mut back = Inventory::default();
    load(&mut back, &saved, &crate::custom::Registry::default());
    assert!(back.window.is_none() && back.cursor.stack(0).is_empty());
    assert_eq!(back.pending.len(), 1 + 4 + 9);
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
    let table = BlockPos::new(2, -60, 0);
    w.place_block(table, blk::Kind::CraftingTable.default_state());
    w.open_block(s, table);
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
    load(&mut back, &first, &crate::custom::Registry::default());
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
    );
    assert!(empty.main.stacks.iter().all(Stack::is_empty));
}

#[test]
fn join_open_close_leave_cycles_leave_every_table_at_its_start() {
    let mut w = World::new(GameMode::Survival);
    let chest = BlockPos::new(3, -60, 3);
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
        w.state.leave(s, "client disconnect");
        w.tick(Vec::new());
        let now = w.state.rules.block_entities.sizes();
        assert_eq!(
            (now.1, now.2),
            (start.1, start.2),
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
    let chest = BlockPos::new(3, -60, 3);
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
        let pos = BlockPos::new(3, -60, 3);
        w.place_block(pos, kind.default_state());
        let a = w.join(1);
        let b = w.join(2);
        w.sent(a);
        w.sent(b);
        // The opener and the players near hear it (PMMP `AnimatedBlockInventoryTrait`).
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
    let chest = BlockPos::new(3, -60, 3);
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
    let centre = crate::math::Vec3::new(
        f64::from(left.x) + 0.5,
        f64::from(left.y) + 0.5,
        f64::from(left.z) + 0.5,
    );
    assert_eq!(opened, [("chest.open".to_owned(), centre.to_f32())]);
}

#[test]
fn a_broken_chest_spills_its_items_and_waits_while_the_world_is_full() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let chest = BlockPos::new(3, -60, 3);
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
    assert_eq!(w.state.rules.block_entities.sizes().2, 1, "one spill waits");
    block_entity::tick(&mut w.state);
    assert_eq!(dropped().len(), 1, "the world is still full");
    set_room(usize::MAX);
    block_entity::tick(&mut w.state);
    assert_eq!(dropped().len(), 2);
    assert_eq!(w.state.rules.block_entities.sizes().2, 0);
    assert_eq!(totals(&w.state), before);
}

#[test]
fn a_shulker_box_keeps_its_items_in_its_item() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let pos = BlockPos::new(3, -60, 3);
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
    let bench = BlockPos::new(3, -60, 3);
    w.place_block(bench, blk::Kind::CraftingTable.default_state());
    w.open_inventory(s);
    assert_eq!(w.inv(s).window.as_ref().unwrap().id, 2);
    w.sent(s);
    // Opening the table while the inventory is open: the server closes it first.
    assert!(open_block(&mut w.state, s, 0, bench));
    let sent = w.sent(s);
    assert_eq!(
        sent,
        vec![Packet::ContainerClose(ContainerClose {
            window_id: 2,
            container_type: 255,
            server_side: true,
        })]
    );
    assert_eq!(w.inv(s).window.as_ref().unwrap().id, 0, "waiting");
    w.client_close(s, 2);
    let sent = w.sent(s);
    assert_eq!(sent.len(), 2);
    assert_eq!(w.inv(s).window.as_ref().unwrap().id, 3);
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
fn hotbar_selection_and_pick_block_follow_pmmp() {
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
    let pos = BlockPos::new(3, -60, 3);
    w.place_block(pos, blk::Kind::Cobblestone.default_state());
    let pick = BlockPickRequest {
        position: [3, -60, 3],
        add_block_nbt: false,
        hot_bar_slot: 4,
    };
    on_block_pick_request(&mut w.state, s, &pick);
    assert_eq!(
        w.inv(s).main.stack(0).item(),
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
    let pos = BlockPos::new(3, -60, 3);
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

#[test]
fn every_crafting_recipe_crafts_from_its_grid_and_shifted_or_short_grids_are_refused() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let bench = BlockPos::new(3, -60, 3);
    w.place_block(bench, blk::Kind::CraftingTable.default_state());
    w.open_block(s, bench);
    let samples = crafting_samples();
    let (mut crafted, mut mirrored, mut shifted, mut short, mut unknown) = (0, 0, 0, 0, Vec::new());
    let all = crafting_recipes();
    for &(id, recipe) in &all {
        // PMMP keeps recipes naming items it does not have (a lodestone compass): never craftable.
        let Some(cells) = layout(recipe, &samples) else {
            unknown.push(id);
            continue;
        };
        let attempt = |w: &mut World, cells: &[Stack]| -> Result<(), request::Error> {
            set_grid(w, s, cells);
            let rid = w.next_id(s);
            let mut actions = vec![craft(id, 1)];
            for (i, c) in cells.iter().enumerate() {
                if !c.is_empty() {
                    let cell = 32 + u8::try_from(i).unwrap();
                    actions.push(consume(1, at(C::CraftingInput, cell, rid)));
                }
            }
            let res = results(recipe);
            for (k, r) in res.iter().enumerate() {
                if res.len() > 1 {
                    actions.push(StackRequestAction::Create(
                        crate::protocol::CreateStackRequestAction {
                            results_slot: u8::try_from(k).unwrap(),
                        },
                    ));
                }
                actions.push(place(
                    r.count(),
                    created(rid),
                    at(C::HotBar, u8::try_from(k).unwrap(), 0),
                ));
            }
            let request = ItemStackRequest {
                request_id: rid,
                actions: actions.clone(),
                filter_strings: Vec::new(),
                filter_cause: 0,
            };
            let dry = request::dry_run(&w.state, s, &request);
            let r = w.request_as(s, rid, actions);
            assert_eq!(ok(&r), dry.is_ok());
            clear(w, s);
            dry
        };
        let made = attempt(&mut w, &cells);
        assert!(made.is_ok(), "recipe {id} crafts from {cells:?}: {made:?}");
        crafted += 1;
        if matches!(recipe, Recipe::Shaped(_)) {
            let mirror = mirror_cells(&cells);
            if mirror != cells {
                assert!(
                    attempt(&mut w, &mirror).is_ok(),
                    "recipe {id} crafts mirrored"
                );
                mirrored += 1;
            }
            if let Some(moved) = shift_one(&cells) {
                assert!(
                    attempt(&mut w, &moved).is_err(),
                    "recipe {id} refuses {moved:?}"
                );
                shifted += 1;
            }
        }
        let mut less = cells.clone();
        let last = less.iter().rposition(|c| !c.is_empty()).unwrap();
        less[last] = Stack::EMPTY;
        if less.iter().any(|c| !c.is_empty()) {
            assert!(
                attempt(&mut w, &less).is_err(),
                "recipe {id} refuses a short grid"
            );
            short += 1;
        }
    }
    assert_eq!(crafted + unknown.len(), all.len());
    // Ender eye, eyeblossoms, golden dandelion, honey block, lodestone compass, moss block,
    // wildflowers: PMMP has no such items, so neither does Anchor.
    assert_eq!(
        unknown,
        [696, 706, 828, 1008, 1041, 1042, 1156, 1260, 1370, 1404]
    );
    assert!(all.len() > 1000, "{} crafting recipes", all.len());
    assert!(
        mirrored > 100 && shifted > 300 && short > 1000,
        "{mirrored} {shifted} {short}"
    );
}

/// An item of every name: every item kind's default type and every block's item.
fn crafting_samples() -> BTreeMap<String, Stack> {
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

fn sample(i: &Ingredient, by_name: &BTreeMap<String, Stack>) -> Option<Stack> {
    match i {
        Ingredient::Exact(e) => {
            Some(Stack::from_parts(e.item, 1, e.meta, e.nbt.map(crate::nbt::compound)).unwrap())
        }
        Ingredient::AnyMeta(n) => by_name.get(n.name).cloned().or_else(|| {
            Type::from_name(n.name, 0, None, &crate::nbt::Compound::new()).map(|t| Stack::new(t, 1))
        }),
        Ingredient::Tag(_) => by_name.values().find(|s| i.can_accept(s)).cloned(),
    }
}

/// The recipe laid out in the top left of a 3x3 grid; `None` if an ingredient has no item.
fn layout(recipe: Recipe<'static>, by_name: &BTreeMap<String, Stack>) -> Option<Vec<Stack>> {
    let mut cells = vec![Stack::EMPTY; 9];
    match recipe {
        Recipe::Shaped(r) => {
            for y in 0..usize::from(r.height) {
                for x in 0..usize::from(r.width) {
                    if let Some(i) = &r.grid[y * usize::from(r.width) + x] {
                        let s = sample(i, by_name)?;
                        assert!(i.can_accept(&s));
                        cells[y * 3 + x] = s;
                    }
                }
            }
        }
        Recipe::Shapeless(r) => {
            for (k, i) in r.ingredients.iter().enumerate() {
                cells[k] = sample(i, by_name)?;
            }
        }
        _ => return None,
    }
    Some(cells)
}

fn mirror_cells(cells: &[Stack]) -> Vec<Stack> {
    let mut out = cells.to_vec();
    for y in 0..3 {
        out[y * 3..y * 3 + 3].reverse();
    }
    out
}

/// The occupied cells, trimmed to their bounds, as a pattern.
fn pattern(cells: &[Stack]) -> Vec<Vec<bool>> {
    let rows: Vec<Vec<bool>> = (0..3)
        .map(|y| (0..3).map(|x| !cells[y * 3 + x].is_empty()).collect())
        .collect();
    let used_rows: Vec<usize> = (0..3).filter(|y| rows[*y].iter().any(|c| *c)).collect();
    let used_cols: Vec<usize> = (0..3).filter(|x| rows.iter().any(|r| r[*x])).collect();
    let (Some(&y0), Some(&y1), Some(&x0), Some(&x1)) = (
        used_rows.first(),
        used_rows.last(),
        used_cols.first(),
        used_cols.last(),
    ) else {
        return Vec::new();
    };
    (y0..=y1)
        .map(|y| (x0..=x1).map(|x| rows[y][x]).collect())
        .collect()
}

/// One occupied cell moved to a neighbouring empty cell so that the occupied cells no longer
/// form the recipe's shape or its mirror image: such a grid can never lay out the recipe.
fn shift_one(cells: &[Stack]) -> Option<Vec<Stack>> {
    let original = pattern(cells);
    let mirrored: Vec<Vec<bool>> = original
        .iter()
        .map(|r| r.iter().rev().copied().collect())
        .collect();
    for from in 0..9 {
        if cells[from].is_empty() {
            continue;
        }
        let (x, y) = (from % 3, from / 3);
        for (dx, dy) in [(1, 0), (0, 1), (-1, 0), (0, -1)] {
            let (Some(nx), Some(ny)) = (x.checked_add_signed(dx), y.checked_add_signed(dy)) else {
                continue;
            };
            if nx >= 3 || ny >= 3 {
                continue;
            }
            let to = ny * 3 + nx;
            if !cells[to].is_empty() {
                continue;
            }
            let mut moved = cells.to_vec();
            moved.swap(from, to);
            let p = pattern(&moved);
            if p != original && p != mirrored {
                return Some(moved);
            }
        }
    }
    None
}

/// Puts `cells` into the crafting table grid (test setup, outside requests).
fn set_grid(w: &mut World, s: SessionId, cells: &[Stack]) {
    let window = w.player_mut(s).inventory.window.as_mut().unwrap();
    for (slot, c) in window.ui.iter_mut().zip(cells) {
        slot.stack = c.clone();
        slot.id = StackId {
            net: i32::from(!c.is_empty()),
            request: None,
        };
    }
}

/// Empties the grid and the hotbar (test setup).
fn clear(w: &mut World, s: SessionId) {
    let inv = &mut w.player_mut(s).inventory;
    if let Some(window) = inv.window.as_mut() {
        for slot in &mut window.ui {
            *slot = Slot::default();
        }
    }
    inv.main = Slots::default();
    w.sent(s);
}

#[test]
fn crafting_refuses_ingredients_swapped_after_the_craft_began() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let bench = BlockPos::new(3, -60, 3);
    w.place_block(bench, blk::Kind::CraftingTable.default_state());
    w.open_block(s, bench);
    let log = block_item(blk::Kind::OakLog);
    let (id, recipe) = recipe_for(block_item(blk::Kind::OakPlanks), &[log]);
    let planks = results(recipe)[0].clone();
    set_grid(&mut w, s, &[Stack::new(log, 1)]);
    w.give(s, Stack::new(block_item(blk::Kind::Dirt), 1));
    let rid = w.next_id(s);
    // The log laid out at the craft, then swapped for dirt, which is consumed instead.
    let actions = vec![
        craft(id, 1),
        swap(at(C::CraftingInput, 32, rid), at(C::HotBar, 0, rid)),
        consume(1, at(C::CraftingInput, 32, rid)),
        place(planks.count(), created(rid), at(C::HotBar, 1, 0)),
    ];
    let request = ItemStackRequest {
        request_id: rid,
        actions: actions.clone(),
        filter_strings: Vec::new(),
        filter_cause: 0,
    };
    assert_eq!(
        request::dry_run(&w.state, s, &request),
        Err(request::Error::Consumed)
    );
    let before = snapshot(&w.state);
    assert!(!ok(&w.request_as(s, rid, actions)));
    assert_eq!(snapshot(&w.state), before);
}

#[test]
fn death_drops_waiting_at_the_entity_limit_survive_a_restart() {
    use crate::tick::entity;
    use crate::tick::tests::initialised;
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let runtime_id = w.player_mut(s).runtime_id;
    w.tick(vec![initialised(1, 2, runtime_id)]);
    // Distinct items, so none merge: the main slots full and the most waiting stacks.
    let mut items = crafting_samples()
        .into_values()
        .filter(|st| st.max_count() > 1)
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
    let pos = BlockPos::new(3, -60, 3);
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
    let barrel = BlockPos::new(3, -60, 3);
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

/// A crafting table or station window closes once its block is replaced, and stays while it is
/// there.
#[test]
fn a_station_window_closes_once_its_block_is_replaced() {
    let mut w = World::new(GameMode::Survival);
    let s = w.join(1);
    let anvil = BlockPos::new(3, -60, 3);
    w.place_block(anvil, blk::Kind::Anvil.default_state());
    w.open_block(s, anvil);
    assert!(w.inv(s).window.is_some());
    close_replaced(&mut w.state, 0, anvil);
    assert!(w.inv(s).window.is_some(), "the anvil is still there");
    w.state.worlds[0]
        .set_block(anvil, blk::Kind::Stone.default_state())
        .unwrap();
    close_replaced(&mut w.state, 0, anvil);
    assert!(w.inv(s).window.is_none());
    assert!(w.state.rules.block_entities.viewers(0, anvil).is_empty());
}
