//! The item stack request executor (PMMP `ItemStackRequestExecutor`, with BDS's stack network
//! ID rules, P6).
//!
//! A request is one unit: its actions run in order on a scratch copy of the slots they touch
//! ([`Exec`]); if every action and the end-of-request checks pass, the changes are committed at
//! once, otherwise nothing changes and the client gets an error and a full resend.
//!
//! Stack network IDs in a request: a slot named with the request's own ID is the slot as this
//! request left it (no check); a negative ID must be the request that last changed the slot; any
//! other ID must be the slot's ID before the request. A whole stack that moves keeps its ID; part
//! of a stack moved into an empty slot gets a new ID while the rest keeps its own; a merge or a
//! count change keeps the ID; created items (crafting, creative) get new IDs.
//!
//! Crafting: `CraftRecipe` crafts from the crafting grid (2x2, or the crafting table's 3x3): the
//! grid as the action finds it must lay out the recipe (shaped: exact shape or its mirror;
//! shapeless: exactly the ingredients), and the request must consume exactly the crafted count from
//! every occupied cell and nothing else. `CraftRecipeAuto` (the recipe book) ignores the client's
//! ingredient list: what the request consumes, from the inventory and the grid, must be exactly the
//! recipe's ingredients for the crafted count; in a trade window it is the quick trade, which the
//! station checks against the offer. `CraftResultsDeprecated` is ignored (BDS does not check it
//! either).

mod events;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use tracing::{debug, warn};

use super::super::entity::Raise;
use super::super::{Phase, Player, SessionId, State, block_entity, entity, survival};
use super::{
    Container, GRID, Inventory, Notice, Slot, SlotRef, StackId, Station, UI_CREATED_OUTPUT, Window,
    WindowKind, block_slot, can_throw, close_now, give_pending, menu, resend_all, station,
    throw_stack, window_slot_of,
};
use crate::block::{self, Class};
use crate::crafting::{self, Recipe, ShapelessType};
use crate::inventory::{MAX_REQUEST_ACTIONS, RECENT_REQUESTS};
use crate::item::{ArmorSlot, Stack};
use crate::menu::{Change, Input, Response};
use crate::protocol::consts::{ContainerId, ItemStackResponseStatus};
use crate::protocol::packet::Packet;
use crate::protocol::{
    FullContainerName, ItemStackRequest, ItemStackResponse, StackRequestAction,
    StackRequestSlotInfo, StackResponseContainerInfo, StackResponseSlotInfo,
    TransferStackRequestAction,
};

/// The most requests a player has handled per tick (PMMP: 80 per packet); more are refused.
pub(super) const MAX_REQUESTS_PER_TICK: u16 = 80;
/// The most entries of a recipe book craft's ingredient list (a recipe has at most 9).
const MAX_INGREDIENT_ENTRIES: usize = crafting::GRID_CELLS;

/// Why a request is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    /// More requests this tick than [`MAX_REQUESTS_PER_TICK`].
    Busy,
    /// More actions than [`MAX_REQUEST_ACTIONS`] or ingredient entries than a recipe has.
    TooLarge,
    /// A request ID the player used recently.
    Repeat,
    /// A container and slot that is not open or does not exist.
    NoSlot,
    /// A stack network ID that does not match the slot.
    StackId,
    /// A count of 0, or more than the slot or the created output holds.
    Count,
    /// Items that do not stack together, or more than a stack holds.
    NoRoom,
    /// An item the slot does not take (armour in the wrong slot, a shulker box in a shulker box).
    NotAccepted,
    /// Destroying or creating items outside creative mode.
    NotCreative,
    /// Taking from the created output with nothing created.
    NothingCreated,
    /// Created items left behind.
    CreatedLeft,
    /// A consume or create action outside a craft, or a second craft.
    Crafting,
    /// An unknown recipe, or one that does not fit the open grid.
    Recipe,
    /// The grid does not lay out the recipe.
    Layout,
    /// What was consumed is not what the craft takes.
    Consumed,
    /// More than one drop in a request (a failed second drop could not undo the first).
    Drops,
    /// A drop the world has no room for (its entity limit, an unloaded position).
    Undroppable,
    /// The stacks held outside the slots afterwards could not all wait for room on a close (the
    /// player already waits with many stacks).
    Unreturnable,
    /// An action of a station that is not open or not handled.
    Unsupported,
    /// A listener cancelled the request, or changed the game so that it no longer does what
    /// the listeners saw.
    Cancelled,
    /// A slot of the open menu that is not editable, or a change of its editable slots that its
    /// handler refused ([`super::menu`]).
    Menu,
}

/// Request results.
pub(super) type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Busy => "too many requests this tick",
            Self::TooLarge => "request too large",
            Self::Repeat => "request id already used",
            Self::NoSlot => "no such open slot",
            Self::StackId => "stack network id mismatch",
            Self::Count => "bad count",
            Self::NoRoom => "items do not fit the slot",
            Self::NotAccepted => "item not accepted in the slot",
            Self::NotCreative => "not in creative mode",
            Self::NothingCreated => "no created item to take",
            Self::CreatedLeft => "created items were not all taken",
            Self::Crafting => "craft action out of place",
            Self::Recipe => "no such recipe for this grid",
            Self::Layout => "the grid does not lay out the recipe",
            Self::Consumed => "consumed items do not match the recipe",
            Self::Drops => "more than one drop",
            Self::Undroppable => "no room in the world for the drop",
            Self::Unreturnable => "held items could not go back",
            Self::Unsupported => "station action not available",
            Self::Cancelled => "cancelled by a listener",
            Self::Menu => "menu slots are the server's",
        })
    }
}

/// Items waiting in the created output (container 60, slot 50).
#[derive(Debug, Clone)]
pub(super) struct Created {
    /// The item, one of it.
    stack: Stack,
    /// How many were made; unlimited from the creative inventory.
    total: u32,
    taken: u32,
    creative: bool,
}

/// A craft in progress in a request.
#[derive(Debug)]
struct Craft<'a> {
    recipe: Recipe<'a>,
    times: u8,
    /// The recipe's results, one craft's worth each.
    results: Vec<Stack>,
    /// Bit `i`: result `i` was created.
    created: u32,
    /// The crafting grid when the craft began, with its slots; `None` for the recipe book.
    grid: Option<(Vec<Stack>, Vec<SlotRef>, usize)>,
}

/// One request applied on a scratch copy of the slots it touches.
pub(super) struct Exec<'a> {
    inv: &'a Inventory,
    pub(super) window: Option<&'a Window>,
    /// The block container slots of the window, by half (a double chest has two).
    halves: [&'a [Stack]; 2],
    request: i32,
    pub(super) creative: bool,
    /// What the stations read from outside the inventory.
    pub(super) ctx: &'a station::Context,
    /// The slots changed so far, in their new state.
    overlay: BTreeMap<SlotRef, Slot>,
    /// The request containers and slots named, in first-seen order (the response's order).
    touched: Vec<(u8, u8)>,
    seen: BTreeSet<(u8, u8)>,
    next_id: i32,
    created: Option<Created>,
    craft: Option<Craft<'a>>,
    /// A station's craft in progress.
    pub(super) station: Option<station::Craft>,
    /// What the request's station crafts do once committed.
    pub(super) effects: Vec<station::Effect>,
    /// What consume actions took, and from where (`None`: the created output).
    pub(super) consumed: Vec<(Option<SlotRef>, Stack)>,
    drop: Option<Stack>,
    /// Whether the plan describes itself for the events (the slots before, what is made).
    pub(super) describe: bool,
    /// What the request makes, when it describes itself.
    pub(super) made: Option<Made>,
}

/// What a request makes, for its events.
#[derive(Debug, Clone)]
pub(super) enum Made {
    /// A recipe (crafting grid, recipe book, stonecutter, smithing table, loom) `times` times;
    /// `inputs` is filled at the end from what the request consumed.
    Craft {
        inputs: Vec<Stack>,
        outputs: Vec<Stack>,
        times: u32,
    },
    /// An enchanting option.
    Enchant {
        choice: station::Choice,
        input: Stack,
        output: Stack,
        cost: u8,
    },
    /// `times` trades of the offer with network ID `net_id`.
    Trade {
        net_id: u32,
        inputs: [Stack; 2],
        output: Stack,
        times: u32,
    },
}

impl fmt::Debug for Exec<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Exec")
            .field("request", &self.request)
            .field("overlay", &self.overlay)
            .finish_non_exhaustive()
    }
}

/// What a request does once every check passed.
#[derive(Debug)]
pub(super) struct Plan {
    writes: Vec<(SlotRef, Slot)>,
    /// The stacks of `writes`' slots before the request, when the plan describes itself.
    before: Vec<Stack>,
    touched: Vec<(u8, u8)>,
    next_id: i32,
    drop: Option<Stack>,
    effects: Vec<station::Effect>,
    made: Option<Made>,
}

impl Plan {
    /// Whether `other` does the same: the same slots to the same stacks, the same drop and the
    /// same station effects (stack network IDs aside).
    fn same(&self, other: &Self) -> bool {
        self.writes.len() == other.writes.len()
            && self
                .writes
                .iter()
                .zip(&other.writes)
                .all(|((ra, a), (rb, b))| ra == rb && a.stack == b.stack)
            && self.drop == other.drop
            && self.effects == other.effects
    }
}

/// An item stack request, sent alone or inside `PlayerAuthInput`.
pub(in crate::tick) fn on_item_stack_request(
    state: &mut State,
    session: SessionId,
    request: &ItemStackRequest,
) {
    let tick = state.tick;
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if player.phase == Phase::Loading {
        return;
    }
    // PMMP `executeInventoryTransaction`: moving items stops using the held item.
    survival::stop_using_item(player);
    let inv = &mut player.inventory;
    if inv.budget.0 != tick {
        inv.budget = (tick, 0);
    }
    let refusal = if inv.budget.1 >= MAX_REQUESTS_PER_TICK {
        Some(Error::Busy)
    } else if request.actions.len() > MAX_REQUEST_ACTIONS {
        Some(Error::TooLarge)
    } else if inv.recent_set.contains(&request.request_id) {
        Some(Error::Repeat)
    } else {
        None
    };
    inv.budget.1 = inv.budget.1.saturating_add(1);
    if refusal.is_none() {
        inv.remember(request.request_id);
    }
    let touch = match refusal {
        Some(_) => menu::Touch::Nothing,
        None => menu::touch(state, session, request),
    };
    let result = match (refusal, &touch) {
        (Some(e), _) => Err(e),
        (None, menu::Touch::Nothing) => run(state, session, request, None),
        (None, menu::Touch::Edit(menu)) => run(state, session, request, Some(*menu)),
        (None, menu::Touch::Refused | menu::Touch::Click(_)) => Err(Error::Menu),
    };
    if let Err(e) = result {
        debug!(
            session = session.get(),
            request = request.request_id,
            error = %e,
            "item stack request refused"
        );
        refuse(state, session, request.request_id);
    }
    // After the refusal and its resend, so what the handler changes in the menu is sent last.
    if let menu::Touch::Click(click) = touch {
        menu::clicked(state, session, click);
    }
}

/// Checks a request, raises its events and applies it: the events see the plan; a cancelled one
/// refuses the request. If a listener ran, the request is checked again on the game as the
/// listeners left it, and refused unless it still does exactly what they saw. A request on the
/// editable slots of the open menu `edit` is then offered to the menu's handler ([`offer`]) and,
/// unless it refuses, checked again the same way, applied, and the menu closed if it said so.
fn run(
    state: &mut State,
    session: SessionId,
    request: &ItemStackRequest,
    edit: Option<crate::menu::Id>,
) -> Result<()> {
    // The handler hears the slots before and after, as the events do.
    let describe = edit.is_some() || events::heard(state);
    let plan = check(state, session, request, describe)?;
    if !describe {
        return commit(state, session, request.request_id, plan);
    }
    let mut raised = events::Raised::of(state, session, &plan);
    raised.call(state)?;
    let plan = if raised.listened(state) {
        check_again(state, session, request, &plan, edit.is_some())?
    } else {
        plan
    };
    let answer = match edit {
        Some(menu) => offer(state, session, menu, &plan)?,
        None => None,
    };
    let plan = match answer {
        Some(_) => check_again(state, session, request, &plan, false).inspect_err(|e| {
            warn!(
                session = session.get(),
                request = request.request_id,
                error = %e,
                "menu handler changed what the change it kept reads; change refused"
            );
        })?,
        None => plan,
    };
    commit(state, session, request.request_id, plan)?;
    raised.watch(state);
    if let (Some(Response::Close), Some(menu)) = (answer, edit)
        && menu::is_open(state, session, menu)
    {
        close_now(state, session, Notice::Send, Raise::Quiet);
    }
    Ok(())
}

/// Checks a request again on the game as the callbacks left it: refused unless it still does
/// exactly what `plan` does.
fn check_again(
    state: &State,
    session: SessionId,
    request: &ItemStackRequest,
    plan: &Plan,
    describe: bool,
) -> Result<Plan> {
    let again = check(state, session, request, describe)?;
    if again.same(plan) {
        Ok(again)
    } else {
        Err(Error::Cancelled)
    }
}

/// Checks a request on the game as it is now; describes the plan for the events if asked.
fn check(
    state: &State,
    session: SessionId,
    request: &ItemStackRequest,
    describe: bool,
) -> Result<Plan> {
    let creative = player_creative(state, session);
    let ctx = station::context(state, session, request);
    let player = state.players.get(&session).ok_or(Error::NoSlot)?;
    let mut exec = Exec::new(
        &player.inventory,
        &state.rules.block_entities,
        request.request_id,
        creative,
        &ctx,
    );
    exec.describe = describe;
    let plan = exec.run(&request.actions)?;
    // Here, not as the drop is made: a menu handler never keeps a change the world then refuses.
    if plan.drop.is_some() && !can_throw(state, session) {
        return Err(Error::Undroppable);
    }
    Ok(plan)
}

/// Checks a request without applying it.
#[cfg(test)]
pub(super) fn dry_run(state: &State, session: SessionId, request: &ItemStackRequest) -> Result<()> {
    check(state, session, request, false).map(|_| ())
}

/// Whether the player's items are not used up, as in creative mode (destroying and creating
/// items).
fn player_creative(state: &State, session: SessionId) -> bool {
    state
        .players
        .get(&session)
        .is_some_and(|p| !survival::has_finite_resources(survival::game_mode(state, p)))
}

/// Answers a refused request: an error response, then the whole inventory again (PMMP
/// `requestSyncAll`) so the client undoes what it predicted.
fn refuse(state: &mut State, session: SessionId, request: i32) {
    let tick = state.tick;
    let blocks = &state.rules.block_entities;
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    player.send_packet(Packet::ItemStackResponse(
        crate::protocol::packet::ItemStackResponse {
            responses: vec![ItemStackResponse {
                status: ItemStackResponseStatus::Error as u8,
                request_id: request,
                container_info: Vec::new(),
            }],
        },
    ));
    resend_all(player, tick, blocks);
}

/// Applies a request checked on the game as it is: the drop, every slot, the response, the
/// other viewers of a changed container, and the stacks waiting for room.
fn commit(state: &mut State, session: SessionId, request: i32, plan: Plan) -> Result<()> {
    if let Some(stack) = plan.drop.clone() {
        #[expect(
            clippy::expect_used,
            reason = "invariant: run's last check, no callback after it, found room for the drop"
        )]
        throw_stack(state, session, stack)
            .then_some(())
            .expect("invariant: a checked drop has room in the world");
    }
    let State { players, rules, .. } = state;
    let player = players.get_mut(&session).ok_or(Error::NoSlot)?;
    let kind = player.inventory.window.as_ref().map(|w| w.kind);
    let mut changed_blocks = Vec::new();
    let mut enchant_input = false;
    let mut furnace_taken = None;
    for (r, slot) in plan.writes {
        if r == SlotRef::Ui(0)
            && matches!(
                kind,
                Some(WindowKind::Station {
                    kind: Station::Enchant,
                    ..
                })
            )
        {
            enchant_input = true;
        }
        if let SlotRef::Block(i) = r {
            let Some((world, pos, j)) = kind.and_then(|k| block_slot(k, i)) else {
                continue;
            };
            if let Some(item) = rules
                .block_entities
                .get_mut(world, pos)
                .and_then(|e| e.items.get_mut(j))
            {
                // dragonfly `collectRewards`: taking from a furnace's output gives its stored
                // experience.
                let furnace = matches!(
                    kind,
                    Some(WindowKind::Container {
                        kind: Container::Furnace(_),
                        ..
                    })
                );
                if furnace && j == block_entity::FURNACE_OUTPUT && slot.stack.count() < item.count()
                {
                    furnace_taken = Some((world, pos));
                }
                *item = slot.stack;
            }
            if let Some(id) = player
                .inventory
                .window
                .as_mut()
                .and_then(|w| w.ids.get_mut(usize::from(i)))
            {
                *id = slot.id;
            }
            changed_blocks.push((world, pos, j));
        } else {
            player.inventory.put(r, slot);
        }
    }
    player.inventory.next_id = plan.next_id;
    player.data_dirty = true;
    let response = response(
        &player.inventory,
        &rules.block_entities,
        request,
        &plan.touched,
    );
    player.send_packet(Packet::ItemStackResponse(
        crate::protocol::packet::ItemStackResponse {
            responses: vec![response],
        },
    ));
    give_pending(player);
    for (world, pos, j) in changed_blocks {
        rules.block_entities.wake(world, pos);
        for viewer in rules.block_entities.viewers(world, pos) {
            if *viewer != session
                && let Some(other) = players.get_mut(viewer)
            {
                notify(other, &rules.block_entities, world, pos, j, Renumber::New);
            }
        }
    }
    if let Some((world, pos)) = furnace_taken {
        let xp = block_entity::take_experience(state, world, pos);
        if let Some(player) = state.players.get(&session)
            && xp > 0
        {
            let (world, at) = (player.world, player.position);
            entity::drop_experience(state, world, at, xp);
        }
    }
    station::apply(state, session, request, plan.effects, enchant_input);
    Ok(())
}

/// Hands the handler of the open menu `menu` the changes the described `plan` would make, before
/// they are made, and gives its answer; `None` if the plan changes no menu slot, so no handler
/// ran. Refused if the handler refuses, or if the menu has no handler (a listener closed it).
fn offer(
    state: &mut State,
    session: SessionId,
    menu: crate::menu::Id,
    plan: &Plan,
) -> Result<Option<Response>> {
    let inv = &state.players.get(&session).ok_or(Error::NoSlot)?.inventory;
    let changes = events::changes(plan, inv.window.as_ref().map(|w| w.kind));
    if !changes
        .iter()
        .any(|c| c.inventory == crate::event::Inventory::Window)
    {
        return Ok(None);
    }
    let named = events::named(plan, inv);
    let change = Input::Change(Change {
        menu,
        changes,
        named,
    });
    match menu::call(state, session, menu, change) {
        Some(Response::Refuse) | None => Err(Error::Menu),
        answer => Ok(answer),
    }
}

/// The stack network ID a viewer's slot gets when a container changes outside its requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Renumber {
    /// A new ID (another player's request, an item that changed).
    New,
    /// The slot's ID stays: the container's own tick changed only its count (BDS keeps it, P6). An
    /// empty slot gets ID 0 and a slot without an ID a new one.
    Keep,
}

/// Tells a viewer of a container that a slot changed outside its own requests, with the slot's
/// ID as `renumber` says.
pub(super) fn notify(
    player: &mut Player,
    blocks: &block_entity::BlockEntities,
    world: crate::save::WorldIndex,
    pos: crate::world::BlockPos,
    index: usize,
    renumber: Renumber,
) {
    let Some(stack) = blocks
        .get(world, pos)
        .and_then(|e| e.items.get(index))
        .cloned()
    else {
        return;
    };
    let Some((id, kind)) = player
        .inventory
        .window
        .as_ref()
        .filter(|w| w.id != 0)
        .map(|w| (w.id, w.kind))
    else {
        return;
    };
    let Some(slot) = window_slot_of(kind, pos, index) else {
        return;
    };
    let kept = player
        .inventory
        .window
        .as_ref()
        .and_then(|w| w.ids.get(usize::from(slot)).copied())
        .filter(|id| renumber == Renumber::Keep && id.net != 0 && !stack.is_empty());
    let net = match kept {
        Some(id) => id.net,
        None if stack.is_empty() => 0,
        None => player.inventory.new_id(),
    };
    if let Some(w) = player.inventory.window.as_mut()
        && let Some(slot_id) = w.ids.get_mut(usize::from(slot))
    {
        *slot_id = kept.unwrap_or(StackId { net, request: None });
    }
    player.send_packet(super::slot_packet(
        u32::from(id),
        u32::from(slot),
        stack.to_instance(net),
    ));
}

/// The response of a request that succeeded: every container and slot it named, grouped by
/// container in first-seen order, with the slots' contents now (PMMP `ItemStackResponseBuilder`;
/// BDS: `Slot` = `HotbarSlot`, no filtered name). The created output is left out.
fn response(
    inv: &Inventory,
    blocks: &block_entity::BlockEntities,
    request: i32,
    touched: &[(u8, u8)],
) -> ItemStackResponse {
    let mut containers: Vec<StackResponseContainerInfo> = Vec::new();
    for &(container, slot) in touched {
        if container == ContainerId::CreatedOutput as u8 {
            continue;
        }
        let Some(r) = inv.locate(container, slot) else {
            continue;
        };
        let current = match r {
            SlotRef::Block(i) => {
                let Some(window) = &inv.window else {
                    continue;
                };
                let stack = block_slot(window.kind, i)
                    .and_then(|(w, p, j)| blocks.get(w, p)?.items.get(j).cloned())
                    .unwrap_or_default();
                let id = window.ids.get(usize::from(i)).copied().unwrap_or_default();
                Slot { stack, id }
            }
            r => inv.get(r).unwrap_or_default(),
        };
        let durability = match current.stack.item() {
            Some(t) if t.max_durability().is_some() => i32::from(current.stack.damage()),
            _ => 0,
        };
        let info = StackResponseSlotInfo {
            slot,
            hotbar_slot: slot,
            count: current.stack.count(),
            stack_network_id: current.id.net,
            custom_name: current.stack.custom_name().to_owned(),
            filtered_custom_name: None,
            durability_correction: durability,
        };
        match containers
            .iter_mut()
            .find(|c| c.container.container_id == container)
        {
            Some(c) => c.slot_info.push(info),
            None => containers.push(StackResponseContainerInfo {
                container: FullContainerName {
                    container_id: container,
                    dynamic_container_id: None,
                },
                slot_info: vec![info],
            }),
        }
    }
    ItemStackResponse {
        status: ItemStackResponseStatus::Ok as u8,
        request_id: request,
        container_info: containers,
    }
}

impl<'a> Exec<'a> {
    fn new(
        inv: &'a Inventory,
        blocks: &'a block_entity::BlockEntities,
        request: i32,
        creative: bool,
        ctx: &'a station::Context,
    ) -> Self {
        let window = inv.window.as_ref().filter(|w| w.id != 0);
        let mut halves: [&[Stack]; 2] = [&[], &[]];
        if let Some(w) = window {
            for (half, block) in halves.iter_mut().zip(w.kind.blocks()) {
                if let Some((world, pos)) = block
                    && let Some(e) = blocks.get(world, pos)
                {
                    *half = &e.items;
                }
            }
        }
        Self {
            inv,
            window,
            halves,
            request,
            creative,
            ctx,
            overlay: BTreeMap::new(),
            touched: Vec::new(),
            seen: BTreeSet::new(),
            next_id: inv.next_id,
            created: None,
            craft: None,
            station: None,
            effects: Vec::new(),
            consumed: Vec::new(),
            drop: None,
            describe: false,
            made: None,
        }
    }

    /// Runs every action, then the end-of-request checks.
    fn run(mut self, actions: &[StackRequestAction]) -> Result<Plan> {
        for action in actions {
            self.action(action)?;
        }
        self.finish()?;
        let before = if self.describe {
            self.overlay
                .keys()
                .map(|r| self.base(*r).map(|s| s.stack).unwrap_or_default())
                .collect()
        } else {
            Vec::new()
        };
        if let Some(Made::Craft { inputs, times, .. }) = &mut self.made {
            *inputs = per_craft(&self.consumed, *times);
        }
        Ok(Plan {
            writes: self.overlay.into_iter().collect(),
            before,
            touched: self.touched,
            next_id: self.next_id,
            drop: self.drop,
            effects: self.effects,
            made: self.made,
        })
    }

    fn action(&mut self, action: &StackRequestAction) -> Result<()> {
        match action {
            StackRequestAction::Take(a) => self.transfer(&a.transfer_stack_request_action),
            StackRequestAction::Place(a) => self.transfer(&a.transfer_stack_request_action),
            StackRequestAction::Swap(a) => self.swap(&a.source, &a.destination),
            StackRequestAction::Drop(a) => {
                if self.drop.is_some() {
                    return Err(Error::Drops);
                }
                let (_, dropped) = self.remove(&a.source, a.count)?;
                self.drop = Some(dropped);
                Ok(())
            }
            StackRequestAction::Destroy(a) if super::station::takes_destroy(self) => {
                let consumed = self.remove(&a.source, a.count)?;
                self.consumed.push(consumed);
                Ok(())
            }
            StackRequestAction::Destroy(a) => {
                if !self.creative {
                    return Err(Error::NotCreative);
                }
                self.remove(&a.source, a.count).map(|_| ())
            }
            StackRequestAction::Consume(a) => {
                let a = &a.destroy_stack_request_action;
                if self.craft.is_none() && !super::station::crafting(self) {
                    return Err(Error::Crafting);
                }
                let consumed = self.remove(&a.source, a.count)?;
                self.consumed.push(consumed);
                Ok(())
            }
            StackRequestAction::Create(a) => {
                let craft = self.craft.as_mut().ok_or(Error::Crafting)?;
                let index = usize::from(a.results_slot);
                let result = craft.results.get(index).cloned().ok_or(Error::Recipe)?;
                let bit = 1u32.checked_shl(u32::from(a.results_slot)).unwrap_or(0);
                // Each result is made once per craft; a second create would duplicate it.
                if craft.created & bit != 0 {
                    return Err(Error::Crafting);
                }
                craft.created |= bit;
                let times = u32::from(craft.times);
                self.set_created(Some(Created::of(&result, times)))
            }
            StackRequestAction::CraftCreative(a) => {
                if !self.creative {
                    return Err(Error::NotCreative);
                }
                let stack = self
                    .ctx
                    .custom
                    .creative_stack(self.ctx.creative, a.creative_item_network_id)
                    .ok_or(Error::Recipe)?;
                self.set_created(Some(Created {
                    stack,
                    total: 0,
                    taken: 0,
                    creative: true,
                }))
            }
            StackRequestAction::CraftRecipe(a) if !super::station::takes_craft_recipe(self) => {
                self.begin_craft(a.recipe_network_id, a.number_of_crafts, false)
            }
            StackRequestAction::AutoCraftRecipe(_) if super::station::takes_auto_craft(self) => {
                super::station::action(self, action)
            }
            StackRequestAction::AutoCraftRecipe(_) if !self.ctx.custom.has_recipe_book() => {
                Err(Error::Recipe)
            }
            StackRequestAction::AutoCraftRecipe(a) => {
                // The client's ingredient list is not used, only bounded (P6).
                if a.ingredients.len() > MAX_INGREDIENT_ENTRIES {
                    return Err(Error::TooLarge);
                }
                self.begin_craft(a.recipe_network_id, a.number_of_crafts, true)
            }
            StackRequestAction::CraftResultsDeprecated(_) => Ok(()),
            StackRequestAction::MineBlock(a) => {
                // Durability is the server's; the slot is only named in the response (PMMP).
                let slot = u8::try_from(a.hotbar_slot)
                    .ok()
                    .filter(|s| *s < super::HOTBAR)
                    .ok_or(Error::NoSlot)?;
                self.touch(ContainerId::HotBar as u8, slot);
                Ok(())
            }
            other => super::station::action(self, other),
        }
    }

    /// Names a request slot for the response.
    fn touch(&mut self, container: u8, slot: u8) {
        if self.seen.insert((container, slot)) {
            self.touched.push((container, slot));
        }
    }

    /// The slot a request slot names, after checking its stack network ID.
    pub(super) fn resolve(&mut self, info: &StackRequestSlotInfo) -> Result<SlotRef> {
        let (container, slot) = (info.container.container_id, info.slot);
        if info.container.dynamic_container_id.is_some() {
            return Err(Error::NoSlot);
        }
        let r = self.inv.locate(container, slot).ok_or(Error::NoSlot)?;
        self.touch(container, slot);
        let client = info.stack_network_id;
        if client != self.request {
            let before = self.base(r).ok_or(Error::NoSlot)?;
            let matches = if client < 0 {
                before.id.request == Some(client)
            } else {
                before.id.net == client
            };
            if !matches {
                return Err(Error::StackId);
            }
        }
        Ok(r)
    }

    /// The slot before the request.
    fn base(&self, r: SlotRef) -> Option<Slot> {
        match r {
            SlotRef::Block(i) => {
                let window = self.window?;
                let (_, pos, j) = block_slot(window.kind, i)?;
                let half = match window.kind.blocks() {
                    [Some((_, p)), _] if p == pos => 0,
                    _ => 1,
                };
                Some(Slot {
                    stack: self.halves.get(half)?.get(j)?.clone(),
                    id: window.ids.get(usize::from(i)).copied().unwrap_or_default(),
                })
            }
            r => self.inv.get(r),
        }
    }

    /// The slot as the request has left it so far.
    pub(super) fn slot(&self, r: SlotRef) -> Result<Slot> {
        match self.overlay.get(&r) {
            Some(s) => Ok(s.clone()),
            None => self.base(r).ok_or(Error::NoSlot),
        }
    }

    /// Sets a slot in the scratch copy, as changed by this request.
    fn set(&mut self, r: SlotRef, stack: Stack, net: i32) {
        let id = StackId {
            net: if stack.is_empty() { 0 } else { net },
            request: Some(self.request),
        };
        self.overlay.insert(r, Slot { stack, id });
    }

    /// A new stack network ID.
    fn new_id(&mut self) -> i32 {
        super::take_stack_id(&mut self.next_id)
    }

    /// Whether slot `r` takes `stack` (PMMP slot validators): armour only in its slot (a carved
    /// pumpkin or a mob head on the head), only what [`Stack::fits_shulker_box`] in a shulker box.
    fn accepts(&self, r: SlotRef, stack: &Stack) -> Result<()> {
        let Some(item) = stack.item() else {
            return Ok(());
        };
        let ok = match r {
            SlotRef::Armor(i) => match item.armor() {
                Some(armor) => armor.slot().index() == usize::from(i),
                None => {
                    usize::from(i) == ArmorSlot::Head.index()
                        && item.block().is_some_and(|b| {
                            b.kind() == block::Kind::CarvedPumpkin || b.is(Class::MobHead)
                        })
                }
            },
            SlotRef::Block(_) => {
                !matches!(
                    self.window.map(|w| w.kind),
                    Some(WindowKind::Container {
                        kind: Container::ShulkerBox,
                        ..
                    })
                ) || stack.fits_shulker_box()
            }
            _ => true,
        };
        if ok { Ok(()) } else { Err(Error::NotAccepted) }
    }

    /// Takes `count` items from a request slot (the created output included): the slot keeps
    /// its ID unless it empties. Returns where they came from and what was taken.
    pub(super) fn remove(
        &mut self,
        info: &StackRequestSlotInfo,
        count: u8,
    ) -> Result<(Option<SlotRef>, Stack)> {
        if count == 0 {
            return Err(Error::Count);
        }
        if is_created_output(info) {
            return self.take_created(count).map(|s| (None, s));
        }
        let r = self.resolve(info)?;
        let slot = self.slot(r)?;
        let have = slot.stack.count();
        if have < count {
            return Err(Error::Count);
        }
        let taken = slot.stack.with_count(count);
        self.set(r, slot.stack.with_count(have - count), slot.id.net);
        Ok((Some(r), taken))
    }

    /// `Take` and `Place`: moves `count` items (PMMP `transferItems`).
    fn transfer(&mut self, a: &TransferStackRequestAction) -> Result<()> {
        let count = a.count;
        if count == 0 {
            return Err(Error::Count);
        }
        let (moved, keep) = if is_created_output(&a.source) {
            (self.take_created(count)?, None)
        } else {
            let r = self.resolve(&a.source)?;
            let slot = self.slot(r)?;
            let have = slot.stack.count();
            if have < count {
                return Err(Error::Count);
            }
            self.set(r, slot.stack.with_count(have - count), slot.id.net);
            // A whole stack keeps its ID where it goes.
            (slot.stack, (have == count).then_some(slot.id.net))
        };
        let r = self.resolve(&a.destination)?;
        self.accepts(r, &moved)?;
        let there = self.slot(r)?;
        let total = u16::from(there.stack.count()) + u16::from(count);
        if total > u16::from(moved.max_count()) {
            return Err(Error::NoRoom);
        }
        let total = u8::try_from(total).map_err(|_| Error::NoRoom)?;
        if there.stack.is_empty() {
            let net = match keep {
                Some(net) => net,
                None => self.new_id(),
            };
            self.set(r, moved.with_count(total), net);
        } else if there.stack.can_stack_with(&moved) {
            self.set(r, there.stack.with_count(total), there.id.net);
        } else {
            return Err(Error::NoRoom);
        }
        Ok(())
    }

    /// `Swap`: exchanges two slots; their stacks keep their IDs.
    fn swap(&mut self, a: &StackRequestSlotInfo, b: &StackRequestSlotInfo) -> Result<()> {
        let ra = self.resolve(a)?;
        let rb = self.resolve(b)?;
        let (sa, sb) = (self.slot(ra)?, self.slot(rb)?);
        self.accepts(ra, &sb.stack)?;
        self.accepts(rb, &sa.stack)?;
        self.set(ra, sb.stack, sb.id.net);
        self.set(rb, sa.stack, sa.id.net);
        Ok(())
    }

    /// Puts items into the created output; the previous ones must all have been taken, unless
    /// they came from the creative inventory (PMMP `setNextCreatedItem`).
    pub(super) fn set_created(&mut self, next: Option<Created>) -> Result<()> {
        if let Some(c) = &self.created
            && !c.creative
            && c.taken < c.total
        {
            return Err(Error::CreatedLeft);
        }
        self.created = next;
        Ok(())
    }

    /// Takes `count` created items (PMMP `takeCreatedItem`); a stack holds at most its maximum.
    fn take_created(&mut self, count: u8) -> Result<Stack> {
        let created = self.created.as_mut().ok_or(Error::NothingCreated)?;
        if count == 0 || count > created.stack.max_count() {
            return Err(Error::Count);
        }
        if !created.creative && u32::from(count) > created.total - created.taken {
            return Err(Error::Count);
        }
        created.taken += u32::from(count);
        let taken = created.stack.with_count(count);
        if !created.creative && created.taken >= created.total {
            self.created = None;
        }
        Ok(taken)
    }

    /// `CraftRecipe` and `CraftRecipeAuto`: begins crafting a crafting recipe `times` times. It
    /// must fit the open grid (the crafting table's 3x3, else the 2x2). A recipe with one result
    /// puts it into the created output now; others wait for `Create` actions.
    fn begin_craft(&mut self, id: u32, times: u8, auto: bool) -> Result<()> {
        if self.craft.is_some() || times == 0 {
            return Err(Error::Crafting);
        }
        let recipe = self.ctx.custom.recipe(id).ok_or(Error::Recipe)?;
        let workbench = self.window.is_some_and(|w| w.kind.is_workbench());
        let side = if workbench { 3 } else { 2 };
        let results = match recipe {
            Recipe::Shaped(r) if r.size().0 <= side && r.size().1 <= side => r.results,
            Recipe::Shapeless(r)
                if r.kind == ShapelessType::Crafting && r.ingredients.len() <= side * side =>
            {
                r.results
            }
            _ => return Err(Error::Recipe),
        };
        let results: Vec<Stack> = results
            .iter()
            .map(|s| s.to_item())
            .collect::<Option<_>>()
            .ok_or(Error::Recipe)?;
        let grid = if auto {
            None
        } else {
            let refs: Vec<SlotRef> = if workbench {
                (0..9).map(SlotRef::Ui).collect()
            } else {
                (0..4).map(SlotRef::Grid).collect()
            };
            let cells = refs
                .iter()
                .map(|r| self.slot(*r).map(|s| s.stack))
                .collect::<Result<Vec<_>>>()?;
            Some((cells, refs, side))
        };
        let single = match results.as_slice() {
            [one] => Some(Created::of(one, u32::from(times))),
            _ => None,
        };
        if self.describe {
            self.made = Some(Made::Craft {
                inputs: Vec::new(),
                outputs: results.clone(),
                times: u32::from(times),
            });
        }
        self.craft = Some(Craft {
            recipe,
            times,
            created: u32::from(single.is_some()),
            results,
            grid,
        });
        match single {
            Some(created) => self.set_created(Some(created)),
            None => Ok(()),
        }
    }

    /// The end-of-request checks: every created item taken, and a craft consumed exactly what
    /// it takes.
    fn finish(&mut self) -> Result<()> {
        self.set_created(None)?;
        if let Some(craft) = self.craft.take() {
            let all = 1u32
                .checked_shl(u32::try_from(craft.results.len()).unwrap_or(32))
                .map_or(u32::MAX, |b| b - 1);
            if craft.created != all {
                return Err(Error::CreatedLeft);
            }
            self.check_consumed(&craft)?;
        }
        self.check_returnable()?;
        super::station::finish(self)
    }

    /// Whether the stacks held outside the slots afterwards (the cursor, the 2x2 grid, the
    /// window's own slots) and the waiting stacks keep the invariant of
    /// [`super::MAX_SAVED_PENDING`]: a close can turn each held stack into a waiting one.
    fn check_returnable(&self) -> Result<()> {
        let held = super::held_outside(self.inv)
            .filter(|r| self.slot(*r).is_ok_and(|s| !s.stack.is_empty()))
            .count();
        if self.inv.pending.len() + held > super::MAX_SAVED_PENDING {
            return Err(Error::Unreturnable);
        }
        Ok(())
    }

    /// Whether the consume actions took exactly what `craft` takes, each cell's from the item it
    /// held when the craft began.
    fn check_consumed(&self, craft: &Craft<'_>) -> Result<()> {
        let times = u32::from(craft.times);
        let Some((cells, refs, side)) = &craft.grid else {
            return self.check_auto(craft, times);
        };
        let laid_out = match craft.recipe {
            Recipe::Shaped(r) => r.matches(cells, *side),
            Recipe::Shapeless(r) => r.matches(cells),
            _ => false,
        };
        if !laid_out {
            return Err(Error::Layout);
        }
        let mut taken = [0u32; crafting::GRID_CELLS];
        for (r, stack) in &self.consumed {
            let k = refs
                .iter()
                .position(|x| Some(*x) == *r)
                .ok_or(Error::Consumed)?;
            if !cells.get(k).is_some_and(|cell| cell.can_stack_with(stack)) {
                return Err(Error::Consumed);
            }
            let t = taken.get_mut(k).ok_or(Error::Consumed)?;
            *t = t.saturating_add(u32::from(stack.count()));
        }
        let exact = cells
            .iter()
            .zip(taken)
            .all(|(cell, t)| t == if cell.is_empty() { 0 } else { times });
        if exact { Ok(()) } else { Err(Error::Consumed) }
    }

    /// The recipe book: what was consumed, from the inventory and the grids, is exactly `times`
    /// of every ingredient.
    fn check_auto(&self, craft: &Craft<'_>, times: u32) -> Result<()> {
        let workbench = self.window.is_some_and(|w| w.kind.is_workbench());
        let mut provided = Vec::with_capacity(self.consumed.len());
        for (r, stack) in &self.consumed {
            let allowed = match r {
                Some(SlotRef::Main(_) | SlotRef::Grid(_)) => true,
                Some(SlotRef::Ui(i)) => workbench && *i < 9,
                _ => false,
            };
            if !allowed {
                return Err(Error::Consumed);
            }
            provided.push((stack.clone(), u32::from(stack.count())));
        }
        let matched = match craft.recipe {
            Recipe::Shaped(r) => crafting::consumed_matches(r.ingredients(), &provided, times),
            Recipe::Shapeless(r) => {
                crafting::consumed_matches(r.ingredients.iter(), &provided, times)
            }
            _ => false,
        };
        if matched {
            Ok(())
        } else {
            Err(Error::Consumed)
        }
    }
}

impl Created {
    /// `times` crafts of `result`.
    pub(super) fn of(result: &Stack, times: u32) -> Self {
        Self {
            stack: result.with_count(1),
            total: u32::from(result.count()).saturating_mul(times),
            taken: 0,
            creative: false,
        }
    }
}

/// What one of `times` crafts used up: the consumed stacks, those that stack together summed,
/// divided by `times` (the checks made the consumption exactly `times` crafts' worth).
fn per_craft(consumed: &[(Option<SlotRef>, Stack)], times: u32) -> Vec<Stack> {
    let mut sums: Vec<(Stack, u32)> = Vec::with_capacity(consumed.len());
    for (_, stack) in consumed {
        match sums.iter_mut().find(|(s, _)| s.can_stack_with(stack)) {
            Some((_, n)) => *n = n.saturating_add(u32::from(stack.count())),
            None => sums.push((stack.clone(), u32::from(stack.count()))),
        }
    }
    sums.into_iter()
        .map(|(s, n)| s.with_count(u8::try_from(n / times.max(1)).unwrap_or(u8::MAX)))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Whether a request slot is the created output (container 60, slot 50).
fn is_created_output(info: &StackRequestSlotInfo) -> bool {
    info.container.container_id == ContainerId::CreatedOutput as u8
        && info.slot == UI_CREATED_OUTPUT
}

impl Inventory {
    /// Remembers a request ID, forgetting the oldest beyond [`RECENT_REQUESTS`].
    fn remember(&mut self, request: i32) {
        if self.recent.len() >= RECENT_REQUESTS
            && let Some(old) = self.recent.pop_front()
        {
            self.recent_set.remove(&old);
        }
        self.recent.push_back(request);
        self.recent_set.insert(request);
    }
}

const _: () = assert!(GRID <= crafting::GRID_CELLS);
const _: () = assert!(MAX_REQUEST_ACTIONS <= crafting::MAX_PROVIDED);
