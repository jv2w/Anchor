//! A scripted client for the determinism runs: it learns what a real client learns from the packets
//! it receives (stack network IDs, the open window, the villagers it sees) and sends the game
//! inputs the run's fixed-seed generator picks: item stack requests (moves, crafts, drops, creative
//! items, broken ones), block actions, item use on blocks, in the air and on entities (attacks),
//! windows, the hotbar and player actions. Most inputs are valid by the server's rules; some are
//! not.

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::block;
use crate::item::Stack;
use crate::protocol::consts::{
    ContainerId as C, InputFlag, PlayerAction as Action, UseItemAction, UseItemOnEntityAction,
    WINDOW_ID_ARMOUR, WINDOW_ID_INVENTORY, WINDOW_ID_OFF_HAND, WINDOW_ID_UI,
};
use crate::protocol::packet::{
    ContainerClose, Interact, InventoryTransaction, ItemStackRequest as RequestPacket,
    MobEquipment, PlayerAction, PlayerAuthInput,
};
use crate::protocol::{
    ConsumeStackRequestAction, CraftCreativeStackRequestAction, CraftRecipeStackRequestAction,
    CraftResultsDeprecatedStackRequestAction, DestroyStackRequestAction, DropStackRequestAction,
    FlagSet, FullContainerName, InputFlags, InventoryAction, InventoryTransactionData,
    ItemInstance, ItemStackRequest, NormalTransactionData, PlaceStackRequestAction,
    PlayerBlockAction, ReleaseItemTransactionData, Rng, StackRequestAction, StackRequestSlotInfo,
    SwapStackRequestAction, TakeStackRequestAction, TransferStackRequestAction,
    UseItemOnEntityTransactionData, UseItemTransactionData,
};
use crate::world::BlockPos;

/// The UI slot of the created output.
const CREATED_SLOT: u8 = 50;
/// The UI slot of the 2x2 crafting grid's first cell; the 3x3 grid's cells follow its four.
pub(super) const GRID_2X2: u8 = 28;
/// The upper bound of the creative item network IDs the generator draws; the creative inventory
/// has fewer items, so some draws are refused.
const MAX_CREATIVE_ID: usize = 1700;
/// How many items the refused craft claims to make.
const REFUSED_CRAFT_COUNT: u8 = 4;
/// The recipe network ID of the craft the server must refuse: the recipe list is empty.
const REFUSED_RECIPE: u32 = 1;

/// What the clients of a run received, by kind, over every session: the run must reach each.
pub(super) type Seen = BTreeMap<&'static str, usize>;

/// One session's client.
#[derive(Debug)]
pub(super) struct Client {
    /// The stack network ID and count of each request slot the client was told of, by
    /// (container, slot); the hotbar and inventory are under the combined container.
    slots: BTreeMap<(u8, u8), (i32, u8)>,
    /// The open window's ID.
    window: Option<u8>,
    /// The next request ID: -1, -3, -5… as clients number them.
    next_request: i32,
}

impl Client {
    /// The stack network ID the client knows a slot by (0 for an empty or unknown slot).
    pub(super) fn net(&self, container: u8, slot: u8) -> i32 {
        self.slots
            .get(&(container, slot))
            .filter(|(_, count)| *count > 0)
            .map_or(0, |(net, _)| *net)
    }

    /// Learns from a packet the server sent and counts what the run reached.
    pub(super) fn learn(&mut self, packet: &Packet, seen: &mut Seen) {
        let mut count = |what: &'static str| *seen.entry(what).or_default() += 1;
        match packet {
            Packet::InventoryContent(p) => {
                for (slot, item) in (0_u8..).zip(&p.content) {
                    self.set(p.window_id, slot, item);
                }
            }
            Packet::InventorySlot(p) => {
                count("inventory slot");
                if let Ok(slot) = u8::try_from(p.slot) {
                    self.set(p.window_id, slot, &p.new_item);
                }
            }
            Packet::ItemStackResponse(p) => {
                for r in &p.responses {
                    if r.status != 0 {
                        count("stack refused");
                        continue;
                    }
                    count("stack done");
                    for c in &r.container_info {
                        let container = match c.container.container_id {
                            id if id == C::HotBar as u8 || id == C::Inventory as u8 => {
                                C::CombinedHotBarAndInventory as u8
                            }
                            id => id,
                        };
                        for s in &c.slot_info {
                            self.slots
                                .insert((container, s.slot), (s.stack_network_id, s.count));
                        }
                    }
                }
            }
            Packet::ContainerOpen(p) => {
                count("window opened");
                self.close_block();
                self.window = Some(p.window_id);
            }
            Packet::ContainerClose(_) => {
                self.close_block();
                self.window = None;
            }
            Packet::AddItemActor(_) => count("item entity"),
            Packet::AddActor(p) if p.entity_type == "minecraft:villager_v2" => count("villager"),
            Packet::AddActor(_) => count("entity"),
            Packet::UpdateTrade(_) => count("trade window"),
            Packet::UpdateBlock(_) | Packet::UpdateSubChunkBlocks(_) => count("block update"),
            Packet::LevelEvent(_) => count("level event"),
            Packet::ActorEvent(_) => count("actor event"),
            Packet::MobEquipment(_) => count("equipment"),
            _ => {}
        }
    }

    /// Takes the next request ID: -1, -3, -5…
    pub(super) fn next_request_id(&mut self) -> i32 {
        let id = self.next_request;
        self.next_request = self.next_request.wrapping_sub(2);
        id
    }

    /// Draws the packets of one game input of the spawned `player`: one packet, or a burst for
    /// actions a client sends over several of its ticks (breaking, crafting from the grid, choosing
    /// an item before using it). Empty when the player lacks what the drawn input needs (a log to
    /// craft planks from).
    #[expect(
        clippy::too_many_lines,
        reason = "consistency: one generator keeps the input mix in one place"
    )]
    pub(super) fn inputs(
        &mut self,
        rng: &mut Rng,
        state: &State,
        player: &Player,
        villagers: &BTreeSet<u64>,
    ) -> Vec<Packet> {
        let feet = player.position - Vec3::new(0.0, survival::EYE_HEIGHT, 0.0);
        let at = BlockPos::containing(feet);
        match rng.below(18) {
            0..=2 => {
                let request = self.stack_request(rng);
                vec![Packet::ItemStackRequest(RequestPacket {
                    requests: vec![request],
                })]
            }
            3 => {
                // A request inside the movement packet, as clients send them.
                let request = self.stack_request(rng);
                let mut flags = FlagSet::<InputFlag>::new();
                flags.insert(InputFlag::PerformItemStackRequest);
                vec![auth_packet(player, &flags, None, None, Some(request))]
            }
            4 => self.craft_refused(rng, player),
            5 | 6 => {
                // Breaking: start, some client ticks, then the predicted destroy.
                let Some(target) = solid_near(rng, state, at) else {
                    return vec![single_action(player, rng, at)];
                };
                let face = if target.y < at.y { 1 } else { random_face(rng) };
                let flags = block_actions_flags();
                let mut out = vec![auth_packet(
                    player,
                    &flags,
                    Some(vec![block_action(Action::StartBreak, target, face)]),
                    None,
                    None,
                )];
                for _ in 0..rng.below(32) {
                    out.push(auth_packet(player, &FlagSet::new(), None, None, None));
                }
                let last = if rng.below(6) == 0 {
                    Action::AbortBreak
                } else {
                    Action::PredictDestroyBlock
                };
                out.push(auth_packet(
                    player,
                    &flags,
                    Some(vec![block_action(last, target, face)]),
                    None,
                    None,
                ));
                out
            }
            7 => vec![single_action(player, rng, at)],
            8 => {
                // Using the held item on a block from the movement packet: placing it.
                let target = ground_near(rng, at);
                let data = self.use_on_block(rng, player, target, 1);
                let mut flags = FlagSet::<InputFlag>::new();
                flags.insert(InputFlag::PerformItemInteraction);
                vec![auth_packet(player, &flags, None, Some(data), None)]
            }
            9 => {
                // Clicking a block: a container opens, the held block is placed.
                let target = match container_near(state, at) {
                    Some(c) if rng.below(2) == 0 => c,
                    _ => ground_near(rng, at),
                };
                let side = if rng.below(4) == 0 {
                    random_face(rng)
                } else {
                    1
                };
                let data = self.use_on_block(rng, player, target, side);
                vec![transaction_packet(InventoryTransactionData::UseItem(
                    Box::new(data),
                ))]
            }
            10 => {
                if rng.below(4) == 0 {
                    let data = ReleaseItemTransactionData {
                        action_type: 0,
                        hot_bar_slot: i32::from(player.inventory.held_slot()),
                        held_item: self.held(player),
                        head_position: player.position.to_f32(),
                        hand: 0,
                    };
                    return vec![transaction_packet(InventoryTransactionData::ReleaseItem(
                        Box::new(data),
                    ))];
                }
                let mut out = Vec::new();
                let mut data = self.use_on_block(rng, player, at, 1);
                // Mostly a hotbar item used in the air first: snowballs, eggs, bows, food.
                let usable = (0_u8..9).filter(|&i| {
                    player.inventory.main_stack(i).is_some_and(|s| {
                        s.item().is_some_and(|t| {
                            t.throw_force().is_some()
                                || t.is(crate::item::Class::Bow)
                                || t.food().is_some()
                        })
                    })
                });
                let usable: Vec<u8> = usable.collect();
                if !usable.is_empty() && rng.below(3) != 0 {
                    let slot = usable[rng.below(usable.len())];
                    let stack = player
                        .inventory
                        .main_stack(slot)
                        .cloned()
                        .unwrap_or_default();
                    let net = self.net(C::CombinedHotBarAndInventory as u8, slot);
                    out.push(Packet::MobEquipment(Box::new(MobEquipment {
                        entity_runtime_id: player.runtime_id,
                        new_item: stack.to_instance(net),
                        inventory_slot: slot,
                        hot_bar_slot: slot,
                        window_id: 0,
                    })));
                    data.hot_bar_slot = i32::from(slot);
                    data.held_item = stack.to_instance(net);
                }
                data.action_type = UseItemAction::ClickAir.into();
                data.block_position = [0, 0, 0];
                data.block_face = 255;
                out.push(transaction_packet(InventoryTransactionData::UseItem(
                    Box::new(data),
                )));
                out
            }
            11 => {
                // An attack on a player or a villager, or trading with a villager.
                let players: Vec<u64> = state.players.values().map(|p| p.runtime_id).collect();
                let villagers: Vec<u64> = villagers.iter().copied().collect();
                let (target, action) = if !villagers.is_empty() && rng.below(2) == 0 {
                    let v = villagers[rng.below(villagers.len())];
                    let a = if rng.below(3) == 0 {
                        UseItemOnEntityAction::Attack
                    } else {
                        UseItemOnEntityAction::Interact
                    };
                    (v, a)
                } else {
                    (
                        players[rng.below(players.len())],
                        UseItemOnEntityAction::Attack,
                    )
                };
                let data = UseItemOnEntityTransactionData {
                    target_entity_runtime_id: target,
                    action_type: action.into(),
                    hot_bar_slot: i32::from(player.inventory.held_slot()),
                    hand: 0,
                    held_item: self.held(player),
                    position: player.position.to_f32(),
                    clicked_position: [0.0, 0.0, 0.0],
                };
                vec![transaction_packet(
                    InventoryTransactionData::UseItemOnEntity(Box::new(data)),
                )]
            }
            12 => {
                let target = if rng.below(8) == 0 {
                    player.runtime_id + 1
                } else {
                    player.runtime_id
                };
                vec![Packet::Interact(Interact {
                    action_type: 6,
                    target_entity_runtime_id: target,
                    position: None,
                })]
            }
            13 => {
                let window_id = match (self.window, rng.below(6)) {
                    (_, 0) => 255,
                    (_, 1) => u8::try_from(rng.below(8)).unwrap(),
                    (Some(id), _) => id,
                    (None, _) => 0,
                };
                vec![Packet::ContainerClose(ContainerClose {
                    window_id,
                    container_type: 0,
                    server_side: false,
                })]
            }
            14 => {
                let slot = u8::try_from(rng.below(10)).unwrap();
                let stack = player
                    .inventory
                    .main_stack(slot)
                    .cloned()
                    .unwrap_or_default();
                let net = self.net(C::CombinedHotBarAndInventory as u8, slot);
                vec![Packet::MobEquipment(Box::new(MobEquipment {
                    entity_runtime_id: player.runtime_id,
                    new_item: stack.to_instance(net),
                    inventory_slot: slot,
                    hot_bar_slot: slot,
                    window_id: if rng.below(10) == 0 { 119 } else { 0 },
                }))]
            }
            15 => {
                let actions = [
                    Action::Jump,
                    Action::StartSprint,
                    Action::StopSprint,
                    Action::StartSneak,
                    Action::StopSneak,
                    Action::Respawn,
                    Action::StartGlide,
                    Action::StopGlide,
                    Action::StartSwimming,
                    Action::StopSwimming,
                    Action::StopSleeping,
                ];
                let action_type = match rng.below(actions.len() + 1) {
                    i if i < actions.len() => actions[i].into(),
                    _ => 99,
                };
                vec![Packet::PlayerAction(PlayerAction {
                    entity_runtime_id: player.runtime_id,
                    action_type,
                    block_position: [0, 0, 0],
                    result_position: [0, 0, 0],
                    block_face: 0,
                })]
            }
            16 => {
                // A step, sometimes jumping or sneaking.
                let mut step = || f64::from(i32::try_from(rng.below(31)).unwrap() - 15) / 10.0;
                let to = player.position + Vec3::new(step(), 0.0, step());
                let mut flags = FlagSet::<InputFlag>::new();
                match rng.below(4) {
                    0 => flags.insert(InputFlag::Jumping),
                    1 => flags.insert(InputFlag::StartSneaking),
                    2 => flags.insert(InputFlag::StopSneaking),
                    _ => {}
                }
                let mut p = auth_packet(player, &flags, None, None, None);
                if let Packet::PlayerAuthInput(a) = &mut p {
                    a.position = to.to_f32();
                }
                vec![p]
            }
            _ => {
                // The Q key: a legacy drop of the held slot.
                let slot = player.inventory.held_slot();
                let stack = player.inventory.held().clone();
                let count = 1 + rng.below(usize::from(stack.count().max(1)));
                let world = InventoryAction {
                    source_type: 2,
                    window_id: None,
                    source_flags: Some(0),
                    inventory_slot: 0,
                    old_item: ItemInstance::default(),
                    new_item: stack
                        .with_count(u8::try_from(count).unwrap_or(1))
                        .to_instance(0),
                };
                let container = InventoryAction {
                    source_type: 0,
                    window_id: Some(0),
                    source_flags: None,
                    inventory_slot: u32::from(slot),
                    old_item: stack.to_instance(0),
                    new_item: ItemInstance::default(),
                };
                vec![Packet::InventoryTransaction(InventoryTransaction {
                    legacy_request_id: 0,
                    legacy_set_item_slots: Vec::new(),
                    actions: vec![world, container],
                    transaction_data: InventoryTransactionData::Normal(NormalTransactionData {}),
                })]
            }
        }
    }

    /// The held item as the client sends it.
    fn held(&self, player: &Player) -> ItemInstance {
        let slot = player.inventory.held_slot();
        player
            .inventory
            .held()
            .to_instance(self.net(C::CombinedHotBarAndInventory as u8, slot))
    }

    /// A click on `face` of the block at `target` with the held item; sometimes a stale one.
    fn use_on_block(
        &self,
        rng: &mut Rng,
        player: &Player,
        target: BlockPos,
        face: i32,
    ) -> UseItemTransactionData {
        let mut held = self.held(player);
        if rng.below(10) == 0 {
            held.stack.count = held.stack.count.wrapping_add(1);
        }
        UseItemTransactionData {
            action_type: UseItemAction::ClickBlock.into(),
            block_position: [target.x, target.y, target.z],
            block_face: face,
            hot_bar_slot: i32::from(player.inventory.held_slot()),
            held_item: held,
            position: player.position.to_f32(),
            clicked_position: [0.5, 1.0, 0.5],
            ..UseItemTransactionData::default()
        }
    }

    /// A request slot naming the client's ID for it; sometimes a wrong one.
    fn slot_with_known_id(
        &self,
        rng: &mut Rng,
        (container, slot): (u8, u8),
    ) -> StackRequestSlotInfo {
        let known = self.net(container, slot);
        let stack_network_id = match rng.below(24) {
            0 => known.wrapping_add(1),
            1 => -999,
            _ => known,
        };
        slot_info(container, slot, stack_network_id)
    }

    /// Forgets the slots of the block window.
    fn close_block(&mut self) {
        self.slots.retain(|(c, _), _| {
            ![C::LevelEntity as u8, C::Barrel as u8, C::ShulkerBox as u8].contains(c)
        });
    }

    /// Learns a slot of a window.
    fn set(&mut self, window: u32, slot: u8, item: &ItemInstance) {
        let key = match window {
            WINDOW_ID_INVENTORY if slot < 36 => (C::CombinedHotBarAndInventory as u8, slot),
            WINDOW_ID_ARMOUR if slot < 4 => (C::Armor as u8, slot),
            WINDOW_ID_OFF_HAND => (C::Offhand as u8, 0),
            WINDOW_ID_UI if slot == 0 => (C::Cursor as u8, 0),
            WINDOW_ID_UI if (GRID_2X2..GRID_2X2 + 13).contains(&slot) => {
                (C::CraftingInput as u8, slot)
            }
            w => match self.window {
                // Chests, hoppers and ender chests; a barrel's or shulker box's own container
                // comes with the responses.
                Some(id) if u32::from(id) == w => (C::LevelEntity as u8, slot),
                _ => return,
            },
        };
        let count = u8::try_from(item.stack.count).unwrap_or(u8::MAX);
        self.slots.insert(key, (item.stack_network_id, count));
    }

    /// Takes a new request ID; sometimes an old one again (a repeat the server must refuse).
    fn request_id(&mut self, rng: &mut Rng) -> i32 {
        if rng.below(16) == 0 && self.next_request < -1 {
            return self.next_request + 2;
        }
        self.next_request_id()
    }

    /// Draws a request moving, swapping, dropping, destroying or creating items, from what the
    /// client knows of its slots; sometimes one that is broken from the start.
    fn stack_request(&mut self, rng: &mut Rng) -> ItemStackRequest {
        let request_id = self.request_id(rng);
        let full: Vec<(u8, u8)> = self
            .slots
            .iter()
            .filter(|(_, (net, count))| *net != 0 && *count > 0)
            .map(|(k, _)| *k)
            .collect();
        let mut any: Vec<(u8, u8)> = self.slots.keys().copied().collect();
        any.extend((0..36).map(|i| (C::CombinedHotBarAndInventory as u8, i)));
        any.extend([(C::Cursor as u8, 0), (C::CraftingInput as u8, GRID_2X2)]);
        let mut actions = Vec::new();
        if full.is_empty() || rng.below(12) == 0 {
            // Broken: unknown containers, slots and IDs.
            for _ in 0..=rng.below(3) {
                let src = slot_info(
                    u8::try_from(rng.below(64)).unwrap(),
                    u8::try_from(rng.below(60)).unwrap(),
                    i32::try_from(rng.below(40)).unwrap() - 20,
                );
                let dst = slot_info(
                    u8::try_from(rng.below(64)).unwrap(),
                    u8::try_from(rng.below(60)).unwrap(),
                    0,
                );
                actions.push(place(u8::try_from(rng.below(70)).unwrap(), src, dst));
            }
        } else {
            for _ in 0..=rng.below(2) {
                let src = full[rng.below(full.len())];
                let dst = any[rng.below(any.len())];
                let have = self.slots.get(&src).map_or(1, |(_, c)| *c);
                let count = if rng.below(10) == 0 {
                    have.saturating_add(1)
                } else {
                    1 + u8::try_from(rng.below(usize::from(have.max(1)))).unwrap()
                };
                let (s, d) = (
                    self.slot_with_known_id(rng, src),
                    self.slot_with_known_id(rng, dst),
                );
                let action = match rng.below(10) {
                    0..=3 => place(count, s, d),
                    4 => StackRequestAction::Take(TakeStackRequestAction {
                        transfer_stack_request_action: TransferStackRequestAction {
                            count,
                            source: s,
                            destination: d,
                        },
                    }),
                    5 => StackRequestAction::Swap(SwapStackRequestAction {
                        source: s,
                        destination: d,
                    }),
                    6 | 7 => StackRequestAction::Drop(DropStackRequestAction {
                        count,
                        source: s,
                        randomly: false,
                    }),
                    8 => {
                        StackRequestAction::Destroy(DestroyStackRequestAction { count, source: s })
                    }
                    _ => {
                        actions.push(StackRequestAction::CraftCreative(
                            CraftCreativeStackRequestAction {
                                creative_item_network_id: 1 + u32::try_from(
                                    rng.below(MAX_CREATIVE_ID),
                                )
                                .unwrap(),
                                number_of_crafts: 1,
                            },
                        ));
                        place(count, created_slot(request_id), d)
                    }
                };
                actions.push(action);
            }
        }
        ItemStackRequest {
            request_id,
            actions,
            filter_strings: Vec::new(),
            filter_cause: 0,
        }
    }

    /// Draws a craft the server refuses: a log from the inventory into the 2x2 grid, then the craft
    /// naming the grid cell by the first request's ID, as a client does before the answer arrives.
    fn craft_refused(&mut self, rng: &mut Rng, player: &Player) -> Vec<Packet> {
        let (recipe, made) = (REFUSED_RECIPE, REFUSED_CRAFT_COUNT);
        let log = block::Kind::OakLog.default_state().item();
        let block::ItemForm::Item(log) = log else {
            return Vec::new();
        };
        let Some(slot) = (0_u8..36).find(|&i| {
            player
                .inventory
                .main_stack(i)
                .is_some_and(|s| s.item() == Some(log))
        }) else {
            return Vec::new();
        };
        let lay = self.request_id(rng);
        let cell = (C::CraftingInput as u8, GRID_2X2);
        let first = ItemStackRequest {
            request_id: lay,
            actions: vec![place(
                1,
                self.slot_with_known_id(rng, (C::CombinedHotBarAndInventory as u8, slot)),
                slot_info(cell.0, cell.1, self.net(cell.0, cell.1)),
            )],
            filter_strings: Vec::new(),
            filter_cause: 0,
        };
        let craft = self.request_id(rng);
        let empty = (0_u8..36)
            .find(|&i| player.inventory.main_stack(i).is_some_and(Stack::is_empty))
            .unwrap_or(35);
        let second = ItemStackRequest {
            request_id: craft,
            actions: vec![
                StackRequestAction::CraftRecipe(CraftRecipeStackRequestAction {
                    recipe_network_id: recipe,
                    number_of_crafts: 1,
                }),
                StackRequestAction::CraftResultsDeprecated(
                    CraftResultsDeprecatedStackRequestAction::default(),
                ),
                StackRequestAction::Consume(ConsumeStackRequestAction {
                    destroy_stack_request_action: DestroyStackRequestAction {
                        count: 1,
                        source: slot_info(cell.0, cell.1, lay),
                    },
                }),
                place(
                    made,
                    created_slot(craft),
                    slot_info(C::CombinedHotBarAndInventory as u8, empty, 0),
                ),
            ],
            filter_strings: Vec::new(),
            filter_cause: 0,
        };
        vec![
            Packet::ItemStackRequest(RequestPacket {
                requests: vec![first],
            }),
            Packet::ItemStackRequest(RequestPacket {
                requests: vec![second],
            }),
        ]
    }
}

impl Default for Client {
    fn default() -> Self {
        Self {
            slots: BTreeMap::new(),
            window: None,
            next_request: -1,
        }
    }
}

/// A random face, sometimes one that does not exist.
fn random_face(rng: &mut Rng) -> i32 {
    i32::try_from(rng.below(7)).unwrap()
}

/// A random block of the ground near the feet at `at`.
fn ground_near(rng: &mut Rng, at: BlockPos) -> BlockPos {
    let mut d = || i32::try_from(rng.below(7)).unwrap() - 3;
    BlockPos::new(at.x + d(), at.y - 1, at.z + d())
}

/// A random block that is not air near the feet at `at`, at foot level or below.
fn solid_near(rng: &mut Rng, state: &State, at: BlockPos) -> Option<BlockPos> {
    let world = state.worlds.first()?;
    let mut found = Vec::new();
    for dy in [-1, 0, 1] {
        for dx in -2..=2 {
            for dz in -2..=2 {
                let p = BlockPos::new(at.x + dx, at.y + dy, at.z + dz);
                if world.block(p).is_some_and(|b| b != block::AIR) {
                    found.push(p);
                }
            }
        }
    }
    (!found.is_empty()).then(|| found[rng.below(found.len())])
}

/// The nearest container block within reach of the feet at `at`, if any.
fn container_near(state: &State, at: BlockPos) -> Option<BlockPos> {
    let world = state.worlds.first()?;
    for dy in [0, -1, 1] {
        for dx in -3..=3 {
            for dz in -3..=3 {
                let p = BlockPos::new(at.x + dx, at.y + dy, at.z + dz);
                if state.rules.block_entities.get(0, p).is_some()
                    && world.block(p).is_some_and(|b| b != block::AIR)
                {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// One block action alone (stop, abort, crack, continue, creative destroy, interact).
fn single_action(player: &Player, rng: &mut Rng, at: BlockPos) -> Packet {
    let kinds = [
        Action::AbortBreak,
        Action::StopBreak,
        Action::CrackBreak,
        Action::ContinueDestroyBlock,
        Action::CreativePlayerDestroyBlock,
        Action::InteractWithBlock,
        Action::Jump,
    ];
    let kind = kinds[rng.below(kinds.len())];
    let target = ground_near(rng, at);
    auth_packet(
        player,
        &block_actions_flags(),
        Some(vec![block_action(kind, target, random_face(rng))]),
        None,
        None,
    )
}

/// The input flags of a movement packet that carries block actions.
fn block_actions_flags() -> FlagSet<InputFlag> {
    let mut flags = FlagSet::new();
    flags.insert(InputFlag::PerformBlockActions);
    flags
}

/// A block action of `kind` on `face` of the block at `pos`.
pub(super) fn block_action(kind: Action, pos: BlockPos, face: i32) -> PlayerBlockAction {
    PlayerBlockAction {
        action: kind.into(),
        block_pos: [pos.x, pos.y, pos.z],
        face,
    }
}

/// A `PlayerAuthInput` standing where the player is.
fn auth_packet(
    player: &Player,
    flags: &FlagSet<InputFlag>,
    actions: Option<Vec<PlayerBlockAction>>,
    item: Option<UseItemTransactionData>,
    request: Option<ItemStackRequest>,
) -> Packet {
    let yaw = crate::math::to_f32(survival::rotation(player).0);
    Packet::PlayerAuthInput(Box::new(PlayerAuthInput {
        yaw,
        position: player.position.to_f32(),
        input_data: InputFlags::from_set(flags),
        item_interaction_data: item,
        item_stack_request: request,
        block_actions: actions,
        ..PlayerAuthInput::default()
    }))
}

/// An inventory transaction packet of `data`.
pub(super) fn transaction_packet(data: InventoryTransactionData) -> Packet {
    Packet::InventoryTransaction(InventoryTransaction {
        legacy_request_id: 0,
        legacy_set_item_slots: Vec::new(),
        actions: Vec::new(),
        transaction_data: data,
    })
}

/// The request slot `slot` of `container`, named by `stack_network_id`.
pub(super) fn slot_info(container: u8, slot: u8, stack_network_id: i32) -> StackRequestSlotInfo {
    StackRequestSlotInfo {
        container: FullContainerName {
            container_id: container,
            dynamic_container_id: None,
        },
        slot,
        stack_network_id,
    }
}

/// The created output slot of request `request`.
pub(super) fn created_slot(request: i32) -> StackRequestSlotInfo {
    slot_info(C::CreatedOutput as u8, CREATED_SLOT, request)
}

/// A request action placing `count` items from `source` into `destination`.
pub(super) fn place(
    count: u8,
    source: StackRequestSlotInfo,
    destination: StackRequestSlotInfo,
) -> StackRequestAction {
    StackRequestAction::Place(PlaceStackRequestAction {
        transfer_stack_request_action: TransferStackRequestAction {
            count,
            source,
            destination,
        },
    })
}
