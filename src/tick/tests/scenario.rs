//! The automatic scenario: one survival player, driven only by the packets a client sends, goes
//! from a log broken by hand to a trade with a villager. The client learns its stack network IDs,
//! windows, enchanting options, villagers and offers from the packets the server sends, as a real
//! client does; it knows which item each of its slots holds as a real client does from the same
//! packets (the test reads that from the server instead of decoding item instances).
//!
//! The flat world has no trees, stone, ore, grass seeds or villagers, so a task (the public
//! `Context`) sets up what the stages need: it sets each oak log, stone block and the enchanting
//! table, gives coal ×1, raw iron ×2, wheat seeds ×1, bone meal ×8, lapis lazuli ×3, emeralds ×4
//! and 300 experience points, and spawns the villager. After every stage the player's items, as
//! a commit would save them, are exactly the expected ones and nothing lies on the ground, so no
//! item was duplicated or lost.

use std::collections::BTreeMap;

use tokio::sync::mpsc;

use super::super::queue::{Input, Outbound};
use super::super::{Output, State, save, survival, task};
use super::client::{self, Client, Seen};
use super::{initialised, input, join, new_state, packet, radius, received, settings, tick};
use crate::block::{self, Kind as BlockKind};
use crate::crafting::{self, Recipe};
use crate::event::Listeners;
use crate::item::{Kind as ItemKind, Stack, Type};
use crate::math::Vec3;
use crate::nbt::{List, Value};
use crate::protocol::consts::{
    ContainerId as C, InputFlag, PlayerAction as Action, UseItemAction, UseItemOnEntityAction,
};
use crate::protocol::packet::{
    ContainerClose, ItemStackRequest as RequestPacket, MobEquipment, Packet, PlayerAuthInput,
};
use crate::protocol::{
    ConsumeStackRequestAction, CraftRecipeStackRequestAction,
    CraftResultsDeprecatedStackRequestAction, DestroyStackRequestAction, FlagSet, InputFlags,
    InventoryTransactionData, ItemStackRequest, PlayerBlockAction, StackRequestAction,
    StackRequestSlotInfo, SwapStackRequestAction, UseItemOnEntityTransactionData,
    UseItemTransactionData,
};
use crate::storage::ChunkKind;
use crate::world::BlockPos;

/// The player's feet: on the grass at the spawn, looking east.
const FEET: Vec3 = Vec3::new(0.5, -60.0, 0.5);
/// The eye height.
const EYE: f64 = 1.62;
/// Looking east (+x): everything the scenario uses lies east of the player.
const YAW: f32 = 270.0;
/// Where logs and stone are set, one at a time, and where the wheat grows: next to the player.
const WORK: BlockPos = BlockPos::new(1, -60, 0);
/// The grass under [`WORK`], tilled into farmland.
const SOIL: BlockPos = BlockPos::new(1, -61, 0);
/// Where the crafting table, the furnace and the enchanting table stand: south-east, so drops
/// and orbs from the north-east (the villager) reach the player unhindered.
const TABLE: BlockPos = BlockPos::new(2, -60, 1);
const FURNACE: BlockPos = BlockPos::new(2, -60, 2);
const ENCHANTING: BlockPos = BlockPos::new(3, -60, 1);
/// Where the villager stands (feet).
const VILLAGER: Vec3 = Vec3::new(2.5, -60.0, -2.5);
/// Block faces as clients number them.
const FACE_UP: i32 = 1;
const FACE_WEST: i32 = 4;
/// The UI slots of the 2x2 grid, the 3x3 grid, the enchanting input and material and the first
/// trade payment.
const GRID_2X2: u8 = 28;
const GRID_3X3: u8 = 32;
const ENCHANT_INPUT: u8 = 14;
const ENCHANT_LAPIS: u8 = 15;
const PAYMENT: u8 = 4;
/// Furnace slots.
const FURNACE_INPUT: u8 = 0;
const FURNACE_FUEL: u8 = 1;
const FURNACE_OUTPUT: u8 = 2;

fn item(kind: ItemKind) -> Type {
    kind.default_type()
}

fn block_item(kind: BlockKind) -> Type {
    let block::ItemForm::Item(t) = kind.default_state().item() else {
        panic!("{kind:?} has an item");
    };
    t
}

/// The server, the player's connection and its client.
struct Scenario {
    state: State,
    output: Output,
    rx: mpsc::Receiver<Outbound>,
    client: Client,
    seen: Seen,
    seq: u64,
    runtime_id: u64,
    /// Where the player's feet are.
    feet: Vec3,
    /// Every packet received since the last [`Self::take`].
    inbox: Vec<Packet>,
}

impl Scenario {
    /// A survival player spawned at the spawn, standing still and looking east.
    fn new() -> Self {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        let (j, rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 4)], &mut output);
        let mut s = Self {
            state,
            output,
            rx,
            client: Client::default(),
            seen: Seen::new(),
            seq: 1,
            runtime_id: 0,
            feet: FEET,
            inbox: Vec::new(),
        };
        s.idle_server(4);
        s.runtime_id = s.state.players.values().next().unwrap().runtime_id;
        s.seq += 1;
        let init = initialised(1, s.seq, s.runtime_id);
        s.step(vec![init]);
        s.idle(2);
        s.take();
        assert_eq!(
            survival::game_mode(&s.state, s.player()),
            crate::config::GameMode::Survival
        );
        s
    }

    fn player(&self) -> &super::super::Player {
        self.state.players.values().next().unwrap()
    }

    /// One tick with `inputs`; the client learns from what it receives.
    fn step(&mut self, mut inputs: Vec<Input>) {
        tick(&mut self.state, &mut inputs, &mut self.output);
        let (packets, close, _) = received(&mut self.rx);
        assert!(close.is_none(), "the player was disconnected");
        for p in &packets {
            self.client.learn(p, &mut self.seen);
        }
        self.inbox.extend(packets);
    }

    /// Ticks without client input (the server is alone).
    fn idle_server(&mut self, n: u32) {
        for _ in 0..n {
            self.step(Vec::new());
        }
    }

    /// The packets received since the last call.
    fn take(&mut self) -> Vec<Packet> {
        std::mem::take(&mut self.inbox)
    }

    /// One tick in which the client sends `packets` after its movement input, as clients send a
    /// `PlayerAuthInput` every tick.
    fn send(&mut self, packets: Vec<Packet>) {
        self.send_with(None, None, packets);
    }

    fn send_with(
        &mut self,
        actions: Option<Vec<PlayerBlockAction>>,
        item: Option<UseItemTransactionData>,
        packets: Vec<Packet>,
    ) {
        let mut flags = FlagSet::<InputFlag>::new();
        if actions.is_some() {
            flags.insert(InputFlag::PerformBlockActions);
        }
        if item.is_some() {
            flags.insert(InputFlag::PerformItemInteraction);
        }
        let eye = self.feet + Vec3::new(0.0, EYE, 0.0);
        let auth = Packet::PlayerAuthInput(Box::new(PlayerAuthInput {
            yaw: YAW,
            position: eye.to_f32(),
            input_data: InputFlags::from_set(&flags),
            item_interaction_data: item,
            block_actions: actions,
            ..Default::default()
        }));
        let mut inputs = Vec::with_capacity(packets.len() + 1);
        for p in std::iter::once(auth).chain(packets) {
            self.seq += 1;
            inputs.push(input(1, self.seq, packet(p)));
        }
        self.step(inputs);
    }

    /// Walks to `feet` in steps of a quarter block.
    fn walk(&mut self, feet: Vec3) {
        while self.feet != feet {
            let d = feet - self.feet;
            let step = |v: f64| crate::math::clamp(v, -0.25, 0.25);
            self.feet += Vec3::new(step(d.x), step(d.y), step(d.z));
            self.idle(1);
        }
    }

    /// `n` ticks of movement input only.
    fn idle(&mut self, n: u32) {
        for _ in 0..n {
            self.send(Vec::new());
        }
    }

    /// A task's view of the server, for the setup.
    fn task(&mut self) -> task::Context<'_> {
        task::Context {
            state: &mut self.state,
        }
    }

    /// Gives the player `stack` through the task API.
    fn give(&mut self, stack: &Stack) {
        let mut ctx = self.task();
        let player = ctx.player_exact("player1").unwrap();
        let left = ctx
            .player_mut(player)
            .unwrap()
            .inventory_mut(crate::inventory::Kind::Main)
            .add(stack);
        assert!(left.is_empty());
    }

    /// Sets a block through the task API.
    fn set(&mut self, pos: BlockPos, kind: BlockKind) {
        self.task()
            .default_world_mut()
            .set_block(pos, kind.default_state())
            .unwrap();
    }

    fn block(&self, pos: BlockPos) -> block::State {
        self.state.worlds[0].block(pos).unwrap()
    }

    /// The main slot holding `item`, as the client knows its slots.
    fn slot_of(&self, item: Type) -> Option<u8> {
        (0u8..36).find(|&i| {
            self.player()
                .inventory
                .main_stack(i)
                .is_some_and(|s| s.item() == Some(item))
        })
    }

    fn stack_in(&self, slot: u8) -> Stack {
        self.player()
            .inventory
            .main_stack(slot)
            .cloned()
            .unwrap_or_default()
    }

    /// The first empty main slot.
    fn empty_slot(&self) -> u8 {
        (0u8..36)
            .find(|&i| {
                self.player()
                    .inventory
                    .main_stack(i)
                    .is_some_and(Stack::is_empty)
            })
            .expect("an empty slot")
    }

    /// A request slot of the inventory, named by the ID the client knows.
    fn main(&self, slot: u8) -> StackRequestSlotInfo {
        let c = C::CombinedHotBarAndInventory as u8;
        client::slot_info(c, slot, self.client.net(c, slot))
    }

    /// A request slot of another container, named by the ID the client knows.
    fn other(&self, container: C, slot: u8) -> StackRequestSlotInfo {
        let c = container as u8;
        client::slot_info(c, slot, self.client.net(c, slot))
    }

    /// Sends one item stack request (`build` gets its request ID) with this tick's movement
    /// input; whether the server did it.
    fn request(&mut self, build: impl FnOnce(&Self, i32) -> Vec<StackRequestAction>) -> bool {
        let id = self.client.next_request_id();
        let actions = build(self, id);
        let request = ItemStackRequest {
            request_id: id,
            actions,
            filter_strings: Vec::new(),
            filter_cause: 0,
        };
        let seen = self.inbox.len();
        self.send(vec![Packet::ItemStackRequest(RequestPacket {
            requests: vec![request],
        })]);
        self.inbox[seen..].iter().any(|p| {
            matches!(p, Packet::ItemStackResponse(r)
                if r.responses.iter().any(|r| r.request_id == id && r.status == 0))
        })
    }

    /// Moves `count` of `item` from the inventory to `to`.
    fn put(&mut self, item: Type, count: u8, to: StackRequestSlotInfo) {
        let from = self.slot_of(item).expect("the item is in the inventory");
        let what = format!(
            "putting {} into {}:{}",
            item.name(),
            to.container.container_id,
            to.slot
        );
        assert!(
            self.request(|s, _| vec![client::place(count, s.main(from), to)]),
            "{what}"
        );
    }

    /// Selects the hotbar slot holding `item`, swapping it into the hotbar first if needed.
    fn hold(&mut self, item: Type) {
        let mut slot = self.slot_of(item).expect("the item is in the inventory");
        if slot >= 9 {
            let hotbar = 8;
            assert!(self.request(
                |s, _| vec![StackRequestAction::Swap(SwapStackRequestAction {
                    source: s.main(slot),
                    destination: s.main(hotbar),
                })]
            ));
            slot = hotbar;
        }
        let stack = self.stack_in(slot);
        let c = C::CombinedHotBarAndInventory as u8;
        let equip = Packet::MobEquipment(Box::new(MobEquipment {
            entity_runtime_id: self.runtime_id,
            new_item: stack.to_instance(self.client.net(c, slot)),
            inventory_slot: slot,
            hot_bar_slot: slot,
            window_id: 0,
        }));
        self.send(vec![equip]);
        assert_eq!(self.player().inventory.held().item(), Some(item));
    }

    /// The held item as the client sends it with a click.
    fn held_click(&self, pos: BlockPos, face: i32) -> UseItemTransactionData {
        let slot = self.player().inventory.held_slot();
        let c = C::CombinedHotBarAndInventory as u8;
        let eye = self.feet + Vec3::new(0.0, EYE, 0.0);
        UseItemTransactionData {
            action_type: UseItemAction::ClickBlock.into(),
            block_position: [pos.x, pos.y, pos.z],
            block_face: face,
            hot_bar_slot: i32::from(slot),
            held_item: self.stack_in(slot).to_instance(self.client.net(c, slot)),
            position: eye.to_f32(),
            clicked_position: [0.5, 1.0, 0.5],
            ..Default::default()
        }
    }

    /// Clicks `face` of the block at `pos` with the held item (placing, using, opening), then
    /// waits the client's repeat interval.
    fn click(&mut self, pos: BlockPos, face: i32) {
        let data = self.held_click(pos, face);
        self.send(vec![client::transaction(
            InventoryTransactionData::UseItem(Box::new(data)),
        )]);
        self.idle(2);
    }

    /// Breaks the block at `pos` as a survival client does: start, the break time the client
    /// works out for its held tool, then the predicted destroy.
    fn break_block(&mut self, pos: BlockPos) {
        let tool = self.player().inventory.held().clone();
        let seconds = self.block(pos).break_time_secs(&tool).unwrap();
        let mut ticks = 0u32;
        while f64::from(ticks) < seconds * 20.0 {
            ticks += 1;
        }
        if ticks <= 1 {
            self.act(pos, &[Action::StartBreak, Action::PredictDestroyBlock]);
        } else {
            self.act(pos, &[Action::StartBreak]);
            self.idle(ticks - 1);
            self.act(pos, &[Action::PredictDestroyBlock]);
        }
        assert_eq!(self.block(pos), block::AIR, "the block broke");
    }

    /// One tick whose movement input carries the block actions `kinds` on the west face of
    /// `pos`.
    fn act(&mut self, pos: BlockPos, kinds: &[Action]) {
        let actions = kinds
            .iter()
            .map(|&k| client::action(k, pos, FACE_WEST))
            .collect();
        self.send_with(Some(actions), None, Vec::new());
    }

    /// Waits until every item and orb on the ground is picked up; when some bounced out of
    /// reach, the player steps onto the block east of it (where the drops came from) and back.
    fn pick_up(&mut self) {
        for n in 0..100 {
            self.idle(1);
            if loose(&mut self.state) == 0 {
                if self.feet != FEET {
                    self.walk(FEET);
                }
                return;
            }
            if n == 20 {
                self.walk(FEET + Vec3::new(1.0, 0.0, 0.0));
            }
        }
        panic!(
            "{} items or orbs left on the ground",
            loose(&mut self.state)
        );
    }

    /// Opens the block at `pos` by clicking it; the window ID.
    fn open(&mut self, pos: BlockPos) -> u8 {
        self.click(pos, FACE_UP);
        let window = self.client.window().expect("a window opened");
        assert!(self.player().inventory.has_window());
        window
    }

    /// Closes the window `window` as the client does.
    fn close(&mut self, window: u8) {
        self.send(vec![Packet::ContainerClose(ContainerClose {
            window_id: window,
            container_type: 0,
            server_side: false,
        })]);
        assert!(!self.player().inventory.has_window());
    }

    /// Crafts `result` once in the open grid of `side` (2: the inventory's, 3: a crafting
    /// table's): its ingredients laid out from the inventory, then the craft, as clients send them.
    fn craft(&mut self, result: &str, side: u8) {
        let (id, cells, made) = recipe(result, side, &self.inventory_types());
        let (result_type, made) = (made.item().expect("a result"), made.count());
        let base = if side == 2 { GRID_2X2 } else { GRID_3X3 };
        let grid = C::CraftingInput as u8;
        // Each cell from a slot that still has the item, as a client's drag spreads them.
        let mut left: Vec<u8> = (0u8..36).map(|i| self.stack_in(i).count()).collect();
        let lay: Vec<(u8, u8)> = cells
            .iter()
            .map(|&(cell, t)| {
                let from = (0u8..36)
                    .find(|&i| self.stack_in(i).item() == Some(t) && left[usize::from(i)] > 0)
                    .expect("an ingredient");
                left[usize::from(from)] -= 1;
                (base + cell, from)
            })
            .collect();
        assert!(self.request(|s, _| {
            lay.iter()
                .map(|&(at, from)| client::place(1, s.main(from), client::slot_info(grid, at, 0)))
                .collect()
        }));
        // The result goes onto a stack of its kind with room, else into an empty slot.
        let dest = (0u8..36)
            .find(|&i| {
                let have = self.stack_in(i);
                have.item() == Some(result_type)
                    && u16::from(have.count()) + u16::from(made) <= u16::from(have.max_count())
            })
            .unwrap_or_else(|| self.empty_slot());
        let dest_id = self.client.net(C::CombinedHotBarAndInventory as u8, dest);
        assert!(
            self.request(|s, req| {
                let mut actions = vec![
                    StackRequestAction::CraftRecipe(CraftRecipeStackRequestAction {
                        recipe_network_id: id,
                        number_of_crafts: 1,
                    }),
                    StackRequestAction::CraftResultsDeprecated(
                        CraftResultsDeprecatedStackRequestAction::default(),
                    ),
                ];
                for &(at, _) in &lay {
                    actions.push(consume(1, s.other(C::CraftingInput, at)));
                }
                actions.push(client::place(
                    made,
                    client::created(req),
                    client::slot_info(C::CombinedHotBarAndInventory as u8, dest, dest_id),
                ));
                actions
            }),
            "crafting {result}"
        );
    }

    /// The item types in the inventory.
    fn inventory_types(&self) -> Vec<Type> {
        (0u8..36).filter_map(|i| self.stack_in(i).item()).collect()
    }

    /// What a commit would save of the player, by item name, and that nothing lies around or
    /// waits in a block (a furnace, the tables' grids).
    fn contents(&mut self) -> BTreeMap<&'static str, u64> {
        assert_eq!(loose(&mut self.state), 0, "nothing lies on the ground");
        assert!(
            self.state
                .rules
                .block_entities
                .all_items()
                .all(Stack::is_empty),
            "nothing waits in a block"
        );
        let data = save::data_of(&self.state, self.player());
        let mut out = BTreeMap::new();
        for key in ["Inventory", "Armor", "Offhand", "PendingItems"] {
            if let Some(Value::List(List::Compound(list))) = data.extra.get(key) {
                for s in list.iter().filter_map(Stack::from_nbt) {
                    if let Some(t) = s.item() {
                        *out.entry(t.name()).or_default() += u64::from(s.count());
                    }
                }
            }
        }
        out
    }

    /// Checks [`Self::contents`] against `want` (name without the namespace, count).
    fn expect(&mut self, stage: &str, want: &[(&str, u64)]) {
        let want: BTreeMap<String, u64> = want
            .iter()
            .map(|(n, c)| (format!("minecraft:{n}"), *c))
            .collect();
        let have: BTreeMap<String, u64> = self
            .contents()
            .into_iter()
            .map(|(n, c)| (n.to_owned(), c))
            .collect();
        assert_eq!(have, want, "after {stage}");
    }

    /// The damage of the first stack of `item`.
    fn damage(&self, item: Type) -> u16 {
        self.stack_in(self.slot_of(item).unwrap()).damage()
    }
}

fn consume(count: u8, source: StackRequestSlotInfo) -> StackRequestAction {
    StackRequestAction::Consume(ConsumeStackRequestAction {
        destroy_stack_request_action: DestroyStackRequestAction { count, source },
    })
}

/// The crafting recipe making `result` that fits a grid of `side` and takes only items of
/// `have`: its network ID, its cells (grid index, item) laid out from the top left, and what one
/// craft makes.
fn recipe(result: &str, side: u8, have: &[Type]) -> (u32, Vec<(u8, Type)>, Stack) {
    let pick =
        |i: &crafting::Ingredient| have.iter().copied().find(|t| i.accepts(&Stack::new(*t, 1)));
    (1..)
        .map_while(|id| crafting::recipe(id).map(|r| (id, r)))
        .find_map(|(id, r)| {
            let (cells, results): (Vec<(u8, Type)>, _) = match r {
                Recipe::Shaped(s) if s.width <= side && s.height <= side => {
                    let mut cells = Vec::new();
                    for (i, cell) in (0u8..).zip(s.grid) {
                        if let Some(ing) = cell {
                            let (row, col) = (i / s.width, i % s.width);
                            cells.push((row * side + col, pick(ing)?));
                        }
                    }
                    (cells, s.results)
                }
                Recipe::Shapeless(s)
                    if s.kind == crafting::ShapelessType::Crafting
                        && s.ingredients.len() <= usize::from(side * side) =>
                {
                    let cells = (0u8..)
                        .zip(s.ingredients)
                        .map(|(i, ing)| Some((i, pick(ing)?)))
                        .collect::<Option<_>>()?;
                    (cells, s.results)
                }
                _ => return None,
            };
            let made = results.first()?.to_item()?;
            (made.item()?.name() == format!("minecraft:{result}")).then_some((id, cells, made))
        })
        .unwrap_or_else(|| panic!("a recipe for {result}"))
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "consistency: the scenario's stages read in order, each with its check"
)]
fn a_survival_player_goes_from_a_log_to_a_trade_with_client_packets_only() {
    let mut s = Scenario::new();
    s.expect("joining", &[]);

    // 1. Three oak logs broken by hand (3 s each), their drops picked up. A destroy predicted
    // too early is refused.
    s.set(WORK, BlockKind::OakLog);
    s.idle(1);
    s.act(WORK, &[Action::StartBreak]);
    s.idle(20);
    s.act(WORK, &[Action::PredictDestroyBlock]);
    assert_eq!(s.block(WORK).kind(), BlockKind::OakLog, "1 s is not 3 s");
    for _ in 0..3 {
        s.set(WORK, BlockKind::OakLog);
        s.idle(1);
        s.break_block(WORK);
        s.pick_up();
    }
    s.expect("breaking logs", &[("oak_log", 3)]);

    // 2. Planks in the inventory's 2x2 grid, then a crafting table.
    for _ in 0..3 {
        s.craft("oak_planks", 2);
    }
    s.expect("crafting planks", &[("oak_planks", 12)]);
    s.craft("crafting_table", 2);
    s.expect(
        "crafting a table",
        &[("oak_planks", 8), ("crafting_table", 1)],
    );

    // 3. The table placed on the grass and opened; sticks, a pickaxe and a hoe in its 3x3 grid.
    s.hold(block_item(BlockKind::CraftingTable));
    s.click(TABLE.offset(0, -1, 0).unwrap(), FACE_UP);
    assert_eq!(s.block(TABLE).kind(), BlockKind::CraftingTable);
    let window = s.open(TABLE);
    s.craft("stick", 3);
    s.craft("wooden_pickaxe", 3);
    s.craft("wooden_hoe", 3);
    s.close(window);
    s.expect(
        "crafting tools",
        &[("oak_planks", 1), ("wooden_pickaxe", 1), ("wooden_hoe", 1)],
    );

    // 4. Eight stone blocks mined with the pickaxe: cobblestone, and the pickaxe wears.
    let pickaxe = item(ItemKind::WoodenPickaxe);
    s.hold(pickaxe);
    for _ in 0..8 {
        s.set(WORK, BlockKind::Stone);
        s.idle(1);
        s.break_block(WORK);
        s.pick_up();
    }
    assert_eq!(s.damage(pickaxe), 8, "one use a block");
    s.expect(
        "mining stone",
        &[
            ("oak_planks", 1),
            ("wooden_pickaxe", 1),
            ("wooden_hoe", 1),
            ("cobblestone", 8),
        ],
    );

    // 5. A furnace crafted at the table, placed, smelting two raw iron with coal.
    let window = s.open(TABLE);
    s.craft("furnace", 3);
    s.close(window);
    s.give(&Stack::new(item(ItemKind::Coal), 1));
    s.give(&Stack::new(item(ItemKind::RawIron), 2));
    s.hold(block_item(BlockKind::Furnace));
    s.click(FURNACE.offset(0, -1, 0).unwrap(), FACE_UP);
    assert!(s.block(FURNACE).is(block::Class::Furnace));
    let window = s.open(FURNACE);
    s.put(
        item(ItemKind::Coal),
        1,
        client::slot_info(C::FurnaceFuel as u8, FURNACE_FUEL, 0),
    );
    s.put(
        item(ItemKind::RawIron),
        2,
        client::slot_info(C::FurnaceIngredient as u8, FURNACE_INPUT, 0),
    );
    s.idle(2 * 200 + 5);
    let xp_before = survival::lifetime_experience(s.player());
    let dest = s.empty_slot();
    // The output slot, as the window's slot updates told the client.
    let output = client::slot_info(
        C::FurnaceResult as u8,
        FURNACE_OUTPUT,
        s.client.net(C::LevelEntity as u8, FURNACE_OUTPUT),
    );
    assert!(s.request(|_, _| vec![client::place(2, output, empty_main(dest))]));
    s.close(window);
    s.pick_up();
    assert!(
        survival::lifetime_experience(s.player()) > xp_before,
        "the smelting experience"
    );
    s.expect(
        "smelting",
        &[
            ("oak_planks", 1),
            ("wooden_pickaxe", 1),
            ("wooden_hoe", 1),
            ("iron_ingot", 2),
        ],
    );

    // 6. The grass tilled with the hoe, seeds planted, bone meal until ripe, the wheat harvested.
    s.give(&Stack::new(item(ItemKind::WheatSeeds), 1));
    s.give(&Stack::new(item(ItemKind::BoneMeal), 8));
    let hoe = item(ItemKind::WoodenHoe);
    s.hold(hoe);
    s.click(SOIL, FACE_UP);
    assert_eq!(s.block(SOIL).kind(), BlockKind::Farmland);
    assert_eq!(s.damage(hoe), 1);
    s.hold(item(ItemKind::WheatSeeds));
    s.click(SOIL, FACE_UP);
    assert_eq!(s.block(WORK).kind(), BlockKind::Wheat);
    s.hold(item(ItemKind::BoneMeal));
    let mut meal = 0;
    while props_growth(s.block(WORK)) < 7 {
        s.click(WORK, FACE_UP);
        meal += 1;
        assert!(meal <= 8, "bone meal ripens wheat");
    }
    s.break_block(WORK);
    s.pick_up();
    let contents = s.contents();
    let seeds = contents.get("minecraft:wheat_seeds").copied().unwrap_or(0);
    assert!(seeds <= 3, "a ripe crop drops 0 to 3 seeds");
    let mut farmed = vec![
        ("oak_planks", 1),
        ("wooden_pickaxe", 1),
        ("wooden_hoe", 1),
        ("iron_ingot", 2),
        ("wheat", 1),
        ("bone_meal", 8 - meal),
    ];
    if seeds > 0 {
        farmed.push(("wheat_seeds", seeds));
    }
    farmed.retain(|(_, n)| *n > 0);
    s.expect("farming", &farmed);

    // 7. The pickaxe enchanted at an enchanting table with lapis and levels.
    s.set(ENCHANTING, BlockKind::EnchantingTable);
    s.give(&Stack::new(item(ItemKind::LapisLazuli), 3));
    {
        let mut ctx = s.task();
        let player = ctx.player_exact("player1").unwrap();
        ctx.player_mut(player).unwrap().add_xp(300);
    }
    let levels = survival::xp_level(s.player());
    let window = s.open(ENCHANTING);
    s.take();
    s.put(
        pickaxe,
        1,
        client::slot_info(C::EnchantingInput as u8, ENCHANT_INPUT, 0),
    );
    s.put(
        item(ItemKind::LapisLazuli),
        3,
        client::slot_info(C::EnchantingMaterial as u8, ENCHANT_LAPIS, 0),
    );
    let options = s
        .take()
        .into_iter()
        .rev()
        .find_map(|p| match p {
            Packet::PlayerEnchantOptions(o) if !o.options.is_empty() => Some(o.options),
            _ => None,
        })
        .expect("the options for the pickaxe");
    let option = options[0].recipe_network_id;
    let cost = options[0].cost;
    assert!(u32::from(cost) <= levels);
    assert!(s.request(|s, req| vec![
        StackRequestAction::CraftRecipe(CraftRecipeStackRequestAction {
            recipe_network_id: option,
            number_of_crafts: 1,
        }),
        consume(1, s.other(C::EnchantingInput, ENCHANT_INPUT)),
        consume(1, s.other(C::EnchantingMaterial, ENCHANT_LAPIS)),
        client::place(
            1,
            client::created(req),
            client::slot_info(C::EnchantingInput as u8, ENCHANT_INPUT, req),
        ),
    ]));
    let dest = s.empty_slot();
    assert!(s.request(|s, _| vec![client::place(
        1,
        s.other(C::EnchantingInput, ENCHANT_INPUT),
        empty_main(dest),
    )]));
    s.close(window);
    let enchanted = s.stack_in(s.slot_of(pickaxe).unwrap());
    assert!(
        enchanted
            .extra()
            .is_some_and(|e| !e.enchantments.is_empty()),
        "the pickaxe is enchanted"
    );
    assert_eq!(enchanted.damage(), 8, "enchanting keeps the wear");
    assert_eq!(
        survival::xp_level(s.player()),
        levels - 1,
        "the first option costs a level (and a lapis)"
    );
    let mut enchanted_stage = farmed.clone();
    enchanted_stage.push(("lapis_lazuli", 2));
    s.expect("enchanting", &enchanted_stage);

    // 8. A trade with a villager NPC: two emeralds for bread.
    s.give(&Stack::new(item(ItemKind::Emerald), 4));
    let offer = crate::custom::Offer::new(
        Stack::new(item(ItemKind::Emerald), 2),
        Stack::new(item(ItemKind::Bread), 3),
        4,
    );
    assert!(
        s.task()
            .spawn_villager(crate::world::Id::DEFAULT, VILLAGER, 1, &[offer])
            .is_ok()
    );
    s.idle(3);
    let villager = s
        .take()
        .into_iter()
        .find_map(|p| match p {
            Packet::AddActor(a) if a.entity_type == "minecraft:villager_v2" => {
                Some(a.entity_runtime_id)
            }
            _ => None,
        })
        .expect("the villager is shown");
    let held = s.player().inventory.held_slot();
    let c = C::CombinedHotBarAndInventory as u8;
    let data = UseItemOnEntityTransactionData {
        target_entity_runtime_id: villager,
        action_type: UseItemOnEntityAction::Interact.into(),
        hot_bar_slot: i32::from(held),
        hand: 0,
        held_item: s.stack_in(held).to_instance(s.client.net(c, held)),
        position: (s.feet + Vec3::new(0.0, EYE, 0.0)).to_f32(),
        clicked_position: [0.0, 0.0, 0.0],
    };
    s.send(vec![client::transaction(
        InventoryTransactionData::UseItemOnEntity(Box::new(data)),
    )]);
    let screen = s
        .take()
        .into_iter()
        .find_map(|p| match p {
            Packet::UpdateTrade(u) => Some(u),
            _ => None,
        })
        .expect("the trade screen");
    let net_id = offer_ids(&screen.serialised_offers)[0];
    s.put(
        item(ItemKind::Emerald),
        2,
        client::slot_info(C::TradeTwoIngredientOne as u8, PAYMENT, 0),
    );
    let dest = s.empty_slot();
    assert!(s.request(|s, req| vec![
        StackRequestAction::CraftRecipe(CraftRecipeStackRequestAction {
            recipe_network_id: net_id,
            number_of_crafts: 1,
        }),
        consume(2, s.other(C::TradeTwoIngredientOne, PAYMENT)),
        client::place(3, client::created(req), empty_main(dest)),
    ]));
    s.close(screen.window_id);
    s.pick_up();
    let mut traded = enchanted_stage;
    traded.push(("emerald", 2));
    traded.push(("bread", 3));
    s.expect("trading", &traded);
    assert!(!s.player().inventory.has_window());
}

/// An empty inventory slot as a request destination.
fn empty_main(slot: u8) -> StackRequestSlotInfo {
    client::slot_info(C::CombinedHotBarAndInventory as u8, slot, 0)
}

/// The item entities and experience orbs of the loaded chunks, as a save writes them.
fn loose(state: &mut State) -> usize {
    state.rules.flush(&mut state.chunks.extras);
    let mut n = 0;
    for pos in state.worlds[0].loaded() {
        let Some(extras) = state.chunks.extras.get((0, pos)) else {
            continue;
        };
        n += extras
            .get(ChunkKind::Entities)
            .iter()
            .filter(|c| {
                matches!(c.get("identifier"), Some(Value::String(id))
                    if id == "minecraft:item" || id == "minecraft:xp_orb")
            })
            .count();
    }
    n
}

/// A crop's growth stage.
fn props_growth(b: block::State) -> i32 {
    match b.value("growth") {
        Some(block::PropertyValue::Int(g)) => g,
        _ => 0,
    }
}

/// The `netId` of every offer of a trade screen.
fn offer_ids(serialised: &[u8]) -> Vec<u32> {
    let offers = crate::protocol::Reader::new(serialised, serialised.len() * 64, 0)
        .nbt(crate::nbt::Encoding::NetworkLittleEndian)
        .unwrap();
    let Some(Value::List(List::Compound(recipes))) = offers.get("Recipes") else {
        panic!("no recipes");
    };
    recipes
        .iter()
        .map(|r| match r.get("netId") {
            Some(Value::Int(id)) => u32::try_from(*id).unwrap(),
            other => panic!("no netId: {other:?}"),
        })
        .collect()
}
