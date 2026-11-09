//! The item stack request executor, with the official server's stack network ID rules (P6).
//!
//! A request is one unit: its actions run in order on a scratch copy of the slots they touch
//! ([`Exec`]); if every action and the end-of-request checks pass, the changes are committed at
//! once, otherwise nothing changes and the client gets an error and a full resend.
//!
//! Stack network IDs in a request: a slot named with the request's own ID is the slot as this
//! request left it (no check); a negative ID must be the request that last changed the slot; any
//! other ID must be the slot's ID before the request. A whole stack that moves keeps its ID; part
//! of a stack moved into an empty slot gets a new ID while the rest keeps its own; a merge or a
//! count change keeps the ID; created items (a trade, creative) get new IDs.
//!
//! Crafting is not offered: a craft action outside a trade window is refused. In a trade
//! window `CraftRecipe` is a trade and `CraftRecipeAuto` the quick trade, which the trade
//! window's [`station`] checks against the offer. `CraftResultsDeprecated` is ignored (the
//! official server does not check it either).

mod events;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use tracing::{debug, warn};

use super::{
    Container, HOTBAR, Inventory, MAX_RECENT_REQUESTS, MAX_SAVED_PENDING, Notice, Slot, SlotRef,
    StackId, UI_CREATED_OUTPUT, Window, WindowKind, block_slot, can_throw, close_now, give_pending,
    held_count, menu, resend_all, slot_packet, station, take_stack_id, throw_stack, window_slot_of,
};
use crate::block::{self, Class};
use crate::item::{ArmorSlot, Stack};
use crate::menu::{Change, Input, Response};
use crate::protocol::consts::{ContainerId, ItemStackResponseStatus};
use crate::protocol::packet::Packet;
use crate::protocol::{
    FullContainerName, ItemStackRequest, ItemStackResponse, StackRequestAction,
    StackRequestSlotInfo, StackResponseContainerInfo, StackResponseSlotInfo,
    TransferStackRequestAction,
};
use crate::tick::entity::Raise;
use crate::tick::{Phase, Player, SessionId, State, block_entity, survival};

/// The most actions in one request: a quick trade consumes from the 36 main slots and the two
/// payment slots, besides its craft and take actions.
pub(super) const MAX_ACTIONS: usize = 60;
/// The most requests a player has handled per tick; more are refused.
const MAX_REQUESTS_PER_TICK: u16 = 80;

/// One request applied on a scratch copy of the slots it touches.
pub(super) struct Exec<'a> {
    inv: &'a Inventory,
    pub(super) window: Option<&'a Window>,
    /// The block container slots of the window, by half (a double chest has two).
    halves: [&'a [Stack]; 2],
    request: i32,
    creative: bool,
    /// What the trade window reads from outside the inventory.
    pub(super) outside: &'a station::Outside,
    /// The slots changed so far, in their new state.
    overlay: BTreeMap<SlotRef, Slot>,
    /// The request containers and slots named, in first-seen order (the response's order).
    touched: Vec<(u8, u8)>,
    seen: BTreeSet<(u8, u8)>,
    next_id: i32,
    created: Option<Created>,
    /// The trade in progress.
    pub(super) craft: Option<station::Craft>,
    /// The trade the request makes once committed.
    pub(super) traded: Option<station::Traded>,
    /// What consume actions took, and from where (`None`: the created output).
    pub(super) consumed: Vec<(Option<SlotRef>, Stack)>,
    drop: Option<Stack>,
    /// How many of the cursor's items are lent so far ([`Inventory::lent`]).
    lent: u8,
    /// Whether the plan describes itself for the events (what is made).
    pub(super) describe: bool,
    /// Whether the plan also keeps the slots before the request ([`Plan::before`]).
    records_before: bool,
    /// What the request makes, when it describes itself.
    pub(super) made: Option<Made>,
}

impl<'a> Exec<'a> {
    /// Puts items into the created output; the previous ones must all have been taken, unless
    /// they came from the creative inventory.
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

    fn new(
        inv: &'a Inventory,
        blocks: &'a block_entity::BlockEntities,
        request: i32,
        creative: bool,
        outside: &'a station::Outside,
    ) -> Self {
        let window = inv.window.as_ref().filter(|w| w.id.is_some());
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
            outside,
            overlay: BTreeMap::new(),
            touched: Vec::new(),
            seen: BTreeSet::new(),
            next_id: inv.next_id,
            created: None,
            craft: None,
            traded: None,
            consumed: Vec::new(),
            drop: None,
            lent: inv.lent,
            describe: false,
            records_before: false,
            made: None,
        }
    }

    /// Whether slot `r` takes `stack`: armor only in its slot (a carved pumpkin or a mob head on
    /// the head), only what [`Stack::can_enter_shulker_box`] in a shulker box.
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
                ) || stack.can_enter_shulker_box()
            }
            _ => true,
        };
        if ok { Ok(()) } else { Err(Error::NotAccepted) }
    }

    /// Whether the stacks held outside the slots afterwards ([`held_count`]) and the waiting
    /// stacks keep the invariant of [`MAX_SAVED_PENDING`]: a close can turn each held stack into
    /// a waiting one.
    fn check_returnable(&self) -> Result<()> {
        let held = held_count(self.inv, |r| {
            self.slot(r).is_ok_and(|s| !s.stack.is_empty())
        });
        if self.inv.pending.len() + held > MAX_SAVED_PENDING {
            return Err(Error::Unreturnable);
        }
        Ok(())
    }

    /// Whether items moving from `r` onto the cursor are lent: `r` is a slot of a menu that lends.
    fn is_lent_from(&self, r: SlotRef) -> bool {
        matches!(r, SlotRef::Menu(_)) && self.inv.menu.as_ref().is_some_and(menu::Open::is_lending)
    }

    /// Runs every action, then the end-of-request checks.
    fn run(mut self, actions: &[StackRequestAction]) -> Result<Plan> {
        for action in actions {
            self.action(action)?;
        }
        self.finish()?;
        let before = if self.records_before {
            self.overlay
                .keys()
                .map(|r| self.base(*r).map(|s| s.stack).unwrap_or_default())
                .collect()
        } else {
            Vec::new()
        };
        Ok(Plan {
            writes: self.overlay.into_iter().collect(),
            before,
            touched: self.touched,
            next_id: self.next_id,
            drop: self.drop,
            traded: self.traded,
            made: self.made,
            lent: self.lent,
        })
    }

    /// The slot as the request has left it so far.
    fn slot(&self, r: SlotRef) -> Result<Slot> {
        match self.overlay.get(&r) {
            Some(s) => Ok(s.clone()),
            None => self.base(r).ok_or(Error::NoSlot),
        }
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
            StackRequestAction::Destroy(a) => {
                if !self.creative {
                    return Err(Error::NotCreative);
                }
                self.remove(&a.source, a.count).map(|_| ())
            }
            StackRequestAction::Consume(a) => {
                let a = &a.destroy_stack_request_action;
                if self.craft.is_none() {
                    return Err(Error::Trade);
                }
                let consumed = self.remove(&a.source, a.count)?;
                self.consumed.push(consumed);
                Ok(())
            }
            StackRequestAction::MineBlock(a) => {
                // Durability is the server's; the slot is only named in the response.
                let slot = u8::try_from(a.hotbar_slot)
                    .ok()
                    .filter(|s| *s < HOTBAR)
                    .ok_or(Error::NoSlot)?;
                self.touch(ContainerId::HotBar as u8, slot);
                Ok(())
            }
            StackRequestAction::CraftCreative(a) => {
                if !self.creative {
                    return Err(Error::NotCreative);
                }
                let stack = self
                    .outside
                    .custom
                    .creative_stack(self.outside.creative, a.creative_item_network_id)
                    .ok_or(Error::NoItem)?;
                self.set_created(Some(Created {
                    stack,
                    total: 0,
                    taken: 0,
                    creative: true,
                }))
            }
            StackRequestAction::CraftResultsDeprecated(_) => Ok(()),
            other => station::action(self, other),
        }
    }

    /// The end-of-request checks: every created item taken, a trade consumed exactly its
    /// payments, and the stacks held outside the slots can go back.
    fn finish(&mut self) -> Result<()> {
        self.set_created(None)?;
        self.check_returnable()?;
        station::finish(self)
    }

    /// Takes `count` items from a request slot (the created output included): the slot keeps
    /// its ID unless it empties. Returns where they came from and what was taken.
    fn remove(
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
        self.follow_lent(r, have, count, None);
        Ok((Some(r), taken))
    }

    /// Handles `Swap`: exchanges two slots; their stacks keep their IDs.
    fn swap(&mut self, a: &StackRequestSlotInfo, b: &StackRequestSlotInfo) -> Result<()> {
        let ra = self.resolve(a)?;
        let rb = self.resolve(b)?;
        let (sa, sb) = (self.slot(ra)?, self.slot(rb)?);
        self.accepts(ra, &sb.stack)?;
        self.accepts(rb, &sa.stack)?;
        // The cursor's stack goes whole: what it gets is lent if a lending menu's slot held it,
        // unless the cursor gave only the player's own items for it, as in a container (an empty
        // cursor takes it lent, as a take would). A swap of a slot with itself (a crafted
        // request) changes nothing.
        let onto_cursor = match (ra, rb) {
            _ if ra == rb => None,
            (SlotRef::Cursor, _) => Some((rb, &sa.stack, sb.stack.count())),
            (_, SlotRef::Cursor) => Some((ra, &sb.stack, sa.stack.count())),
            _ => None,
        };
        if let Some((other, given, count)) = onto_cursor {
            let bought = self.lent == 0 && !given.is_empty();
            self.lent = if self.is_lent_from(other) && !bought {
                count
            } else {
                0
            };
        }
        self.set(ra, sb.stack, sb.id.net);
        self.set(rb, sa.stack, sa.id.net);
        Ok(())
    }

    /// Handles `Take` and `Place`: moves `count` items.
    fn transfer(&mut self, a: &TransferStackRequestAction) -> Result<()> {
        let count = a.count;
        if count == 0 {
            return Err(Error::Count);
        }
        let (moved, keep, from) = if is_created_output(&a.source) {
            (self.take_created(count)?, None, None)
        } else {
            let r = self.resolve(&a.source)?;
            let slot = self.slot(r)?;
            let have = slot.stack.count();
            if have < count {
                return Err(Error::Count);
            }
            self.set(r, slot.stack.with_count(have - count), slot.id.net);
            // A whole stack keeps its ID where it goes.
            (
                slot.stack,
                (have == count).then_some(slot.id.net),
                Some((r, have)),
            )
        };
        let r = self.resolve(&a.destination)?;
        if let Some((from, have)) = from {
            self.follow_lent(from, have, count, Some(r));
        }
        self.accepts(r, &moved)?;
        let there = self.slot(r)?;
        let total = u16::from(there.stack.count()) + u16::from(count);
        if total > u16::from(moved.max_stack_size()) {
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

    /// Follows the lent items ([`Inventory::lent`]) through `count` items moving from `from`,
    /// which held `had` before, to `to` (`None`: out of the slots, dropped, destroyed or used up).
    /// Of the cursor's items, those going into a menu slot are the lent ones first, those going
    /// anywhere else the player's own first, as the handler keeps the lent ones until they leave
    /// for elsewhere. Items put back where they were (a crafted request) stay as they are.
    fn follow_lent(&mut self, from: SlotRef, had: u8, count: u8, to: Option<SlotRef>) {
        if to == Some(from) {
            return;
        }
        if from == SlotRef::Cursor {
            let leaving = match to {
                Some(SlotRef::Menu(_)) => count,
                _ => count.saturating_sub(had.saturating_sub(self.lent)),
            };
            self.lent = self.lent.saturating_sub(leaving);
        }
        if to == Some(SlotRef::Cursor) && self.is_lent_from(from) {
            self.lent = self.lent.saturating_add(count);
        }
    }

    /// A new stack network ID.
    fn new_id(&mut self) -> i32 {
        take_stack_id(&mut self.next_id)
    }

    /// The slot a request slot names, after checking its stack network ID.
    fn resolve(&mut self, info: &StackRequestSlotInfo) -> Result<SlotRef> {
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

    /// Sets a slot in the scratch copy, as changed by this request.
    fn set(&mut self, r: SlotRef, stack: Stack, net: i32) {
        let id = StackId {
            net: if stack.is_empty() { 0 } else { net },
            request: Some(self.request),
        };
        self.overlay.insert(r, Slot { stack, id });
    }

    /// Takes `count` created items; a stack holds at most its maximum.
    fn take_created(&mut self, count: u8) -> Result<Stack> {
        let created = self.created.as_mut().ok_or(Error::NothingCreated)?;
        if count == 0 || count > created.stack.max_stack_size() {
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

    /// Names a request slot for the response.
    fn touch(&mut self, container: u8, slot: u8) {
        if self.seen.insert((container, slot)) {
            self.touched.push((container, slot));
        }
    }
}

impl fmt::Debug for Exec<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Exec")
            .field("request", &self.request)
            .field("overlay", &self.overlay)
            .finish_non_exhaustive()
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

impl Created {
    /// `times` makes of `result`.
    pub(super) fn of(result: &Stack, times: u32) -> Self {
        Self {
            stack: result.with_count(1),
            total: u32::from(result.count()).saturating_mul(times),
            taken: 0,
            creative: false,
        }
    }
}

/// The trade a request makes, for its events: `times` trades of the offer with network ID
/// `net_id`.
#[derive(Debug, Clone)]
pub(super) struct Made {
    net_id: u32,
    inputs: [Stack; 2],
    output: Stack,
    times: u32,
}

impl Made {
    /// `times` trades of the offer with network ID `net_id`.
    pub(super) fn of(net_id: u32, inputs: [Stack; 2], output: Stack, times: u32) -> Self {
        Self {
            net_id,
            inputs,
            output,
            times,
        }
    }
}

/// What a request does once every check passed.
#[derive(Debug)]
struct Plan {
    writes: Vec<(SlotRef, Slot)>,
    /// The stacks of `writes`' slots before the request, when the plan describes its slots.
    before: Vec<Stack>,
    touched: Vec<(u8, u8)>,
    next_id: i32,
    drop: Option<Stack>,
    traded: Option<station::Traded>,
    made: Option<Made>,
    /// How many of the cursor's items are lent afterwards ([`Inventory::lent`]).
    lent: u8,
}

impl Plan {
    /// Whether `other` does the same: the same slots to the same stacks, the same drop, the same
    /// trade and the same lent items (stack network IDs aside).
    fn is_same_as(&self, other: &Self) -> bool {
        self.writes.len() == other.writes.len()
            && self
                .writes
                .iter()
                .zip(&other.writes)
                .all(|((ra, a), (rb, b))| ra == rb && a.stack == b.stack)
            && self.drop == other.drop
            && self.traded == other.traded
            && self.lent == other.lent
    }
}

/// Why a request is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    /// More requests this tick than [`MAX_REQUESTS_PER_TICK`].
    Busy,
    /// More actions than [`MAX_ACTIONS`].
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
    /// An item the slot does not take (armor in the wrong slot, a shulker box in a shulker box).
    NotAccepted,
    /// Destroying or creating items outside creative mode.
    NotCreative,
    /// Taking from the created output with nothing created.
    NothingCreated,
    /// Created items left behind.
    CreatedLeft,
    /// A consume action outside a trade, or a second trade.
    Trade,
    /// An unknown trade offer, or one the trade window cannot make now.
    Offer,
    /// A creative item number the player's creative inventory lacks.
    NoItem,
    /// What was consumed is not what the trade takes.
    Consumed,
    /// More than one drop in a request (a failed second drop could not undo the first).
    Drops,
    /// A drop the world has no room for (its entity limit, an unloaded position).
    Undroppable,
    /// The stacks held outside the slots afterwards could not all wait for room on a close (the
    /// player already waits with many stacks).
    Unreturnable,
    /// An action the open window does not take (a craft outside a trade window).
    Unsupported,
    /// A listener cancelled the request, or changed the game so that it no longer does what
    /// the listeners saw.
    Cancelled,
    /// The open menu's handler changed the game so that a change it kept no longer does what it
    /// heard.
    MenuChanged,
    /// A slot of the open menu that is not editable, or a change of its editable slots that its
    /// handler refused ([`super::menu`]).
    Menu,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("too many requests this tick"),
            Self::TooLarge => f.write_str("request too large"),
            Self::Repeat => f.write_str("request id already used"),
            Self::NoSlot => f.write_str("no such open slot"),
            Self::StackId => f.write_str("stack network id mismatch"),
            Self::Count => f.write_str("bad count"),
            Self::NoRoom => f.write_str("items do not fit the slot"),
            Self::NotAccepted => f.write_str("item not accepted in the slot"),
            Self::NotCreative => f.write_str("not in creative mode"),
            Self::NothingCreated => f.write_str("no created item to take"),
            Self::CreatedLeft => f.write_str("created items were not all taken"),
            Self::Trade => f.write_str("trade action out of place"),
            Self::Offer => f.write_str("no such trade offer"),
            Self::NoItem => f.write_str("no such creative item"),
            Self::Consumed => f.write_str("consumed items do not match the trade"),
            Self::Drops => f.write_str("more than one drop"),
            Self::Undroppable => f.write_str("no room in the world for the drop"),
            Self::Unreturnable => f.write_str("held items could not go back"),
            Self::Unsupported => f.write_str("action not available in this window"),
            Self::Cancelled => f.write_str("cancelled by a listener"),
            Self::MenuChanged => f.write_str("changed by the menu handler"),
            Self::Menu => f.write_str("menu slot not editable or change refused by its handler"),
        }
    }
}

impl std::error::Error for Error {}

/// A result with the module's [`enum@Error`].
pub(super) type Result<T, E = Error> = std::result::Result<T, E>;

/// What a checked plan tells of itself, for the events and the open menu's handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Describe {
    /// Nothing: no one hears the request.
    Nothing,
    /// What the request makes and drops.
    Made,
    /// What it makes and drops, and the slots it changes before and after.
    Slots,
}

impl Inventory {
    /// Remembers a request ID, forgetting the oldest beyond [`MAX_RECENT_REQUESTS`].
    fn remember(&mut self, request: i32) {
        if self.recent.len() >= MAX_RECENT_REQUESTS
            && let Some(old) = self.recent.pop_front()
        {
            self.recent_set.remove(&old);
        }
        self.recent.push_back(request);
        self.recent_set.insert(request);
    }
}

/// Handles an item stack request, sent alone or inside `PlayerAuthInput`.
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
    // Moving items stops using the held item.
    survival::stop_using_item(player);
    let inv = &mut player.inventory;
    if inv.budget.0 != tick {
        inv.budget = (tick, 0);
    }
    let refusal = if inv.budget.1 >= MAX_REQUESTS_PER_TICK {
        Some(Error::Busy)
    } else if request.actions.len() > MAX_ACTIONS {
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
            error = &e as &(dyn std::error::Error + 'static),
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
    let describe = events::describes(state, edit);
    let plan = check(state, session, request, describe)?;
    if describe == Describe::Nothing {
        commit(state, session, request.request_id, plan);
        return Ok(());
    }
    let mut raised = events::Raised::of(state, session, &plan);
    raised.call(state)?;
    let plan = if raised.has_listened(state) {
        let again = if edit.is_some() {
            Describe::Slots
        } else {
            Describe::Nothing
        };
        check_again(state, session, request, &plan, again, Error::Cancelled)?
    } else {
        plan
    };
    let answer = match edit {
        Some(menu) => offer(state, session, menu, &plan)?.map(|r| (menu, r)),
        None => None,
    };
    let plan = match answer {
        Some(_) => check_again(
            state,
            session,
            request,
            &plan,
            Describe::Nothing,
            Error::MenuChanged,
        )
        .inspect_err(|e| {
            warn!(
                session = session.get(),
                request = request.request_id,
                error = e as &(dyn std::error::Error + 'static),
                "menu handler changed what the change it kept reads; change refused"
            );
        })?,
        None => plan,
    };
    commit(state, session, request.request_id, plan);
    raised.watch(state);
    if let Some((menu, Response::Close)) = answer
        && menu::is_open(state, session, menu)
    {
        close_now(state, session, Notice::Send, Raise::Events);
    }
    Ok(())
}

/// The plan of a request checked again on the game as the callbacks left it: refused unless it
/// still does exactly what `plan` does, with `changed` if it checks but does otherwise.
fn check_again(
    state: &State,
    session: SessionId,
    request: &ItemStackRequest,
    plan: &Plan,
    describe: Describe,
    changed: Error,
) -> Result<Plan> {
    let again = check(state, session, request, describe)?;
    if again.is_same_as(plan) {
        Ok(again)
    } else {
        Err(changed)
    }
}

/// The plan of a request checked on the game as it is now, described as `describe` says.
fn check(
    state: &State,
    session: SessionId,
    request: &ItemStackRequest,
    describe: Describe,
) -> Result<Plan> {
    let creative = is_creative(state, session);
    let outside = station::outside(state, session, request);
    let player = state.players.get(&session).ok_or(Error::NoSlot)?;
    let mut exec = Exec::new(
        &player.inventory,
        &state.rules.block_entities,
        request.request_id,
        creative,
        &outside,
    );
    exec.describe = describe != Describe::Nothing;
    exec.records_before = describe == Describe::Slots;
    let plan = exec.run(&request.actions)?;
    // Here, not as the drop is made: a menu handler never keeps a change the world then refuses.
    if plan.drop.is_some() && !can_throw(state, session) {
        return Err(Error::Undroppable);
    }
    Ok(plan)
}

/// Whether the player's items are not used up, as in creative mode (destroying and creating
/// items).
fn is_creative(state: &State, session: SessionId) -> bool {
    state
        .players
        .get(&session)
        .is_some_and(|p| !survival::has_finite_resources(p.game_mode()))
}

/// Answers a refused request: an error response, then the whole inventory again so the client
/// undoes what it predicted.
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
fn commit(state: &mut State, session: SessionId, request: i32, plan: Plan) {
    if let Some(stack) = plan.drop.clone() {
        #[expect(
            clippy::expect_used,
            reason = "invariant: run's last check, no callback after it, found room for the drop"
        )]
        throw_stack(state, session, stack)
            .then_some(())
            .expect("a checked drop has room in the world");
    }
    let State { players, rules, .. } = state;
    #[expect(clippy::expect_used, reason = "invariant: run's last check found it")]
    let player = players
        .get_mut(&session)
        .expect("a checked player is in the game");
    let kind = player.inventory.window.as_ref().map(|w| w.kind);
    let mut changed_blocks = Vec::new();
    for (r, slot) in plan.writes {
        if let SlotRef::Block(i) = r {
            let Some((world, pos, j)) = kind.and_then(|k| block_slot(k, i)) else {
                continue;
            };
            if let Some(item) = rules
                .block_entities
                .get_mut(world, pos)
                .and_then(|e| e.items.get_mut(j))
            {
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
    player.inventory.lent = plan.lent;
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
        for viewer in rules.block_entities.viewers(world, pos) {
            if *viewer != session
                && let Some(other) = players.get_mut(viewer)
            {
                notify(other, &rules.block_entities, world, pos, j);
            }
        }
    }
    if let Some(traded) = plan.traded {
        station::apply(state, session, request, traded);
    }
}

/// Hands the handler of the open menu `menu` the changes the described `plan` would make, before
/// they are made, and gives its answer; `None` if the plan changes no menu slot, nor the lent
/// items, nor a cursor that holds lent items, so no handler ran. Refused if the handler refuses.
/// A listener that closed the menu never gets here: the check after the listeners refuses the
/// request first (its menu slots are gone or renumbered).
fn offer(
    state: &mut State,
    session: SessionId,
    menu: crate::menu::Id,
    plan: &Plan,
) -> Result<Option<Response>> {
    #[expect(clippy::expect_used, reason = "invariant: run's checks found it")]
    let inv = &state
        .players
        .get(&session)
        .expect("a checked player is in the game")
        .inventory;
    let changes = events::changes(plan, inv.window.as_ref().map(|w| w.kind));
    let lent = [inv.lent, plan.lent];
    let heard = |c: &crate::event::SlotChange| match c.inventory {
        crate::event::Inventory::Window => true,
        crate::event::Inventory::Player(crate::inventory::Kind::Cursor) => lent[0] > 0,
        _ => false,
    };
    if lent[0] == lent[1] && !changes.iter().any(heard) {
        return Ok(None);
    }
    let named = events::named(plan, inv);
    let change = Input::Change(Change {
        menu,
        changes,
        named,
        lent,
    });
    match menu::call(state, session, menu, change) {
        Some(Response::Refuse) | None => Err(Error::Menu),
        answer => Ok(answer),
    }
}

/// Tells a viewer of a container that a slot changed outside its own requests, with a new stack
/// network ID.
pub(super) fn notify(
    player: &mut Player,
    blocks: &block_entity::BlockEntities,
    world: crate::save::WorldIndex,
    pos: crate::world::BlockPos,
    index: usize,
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
        .and_then(|w| Some((w.id?, w.kind)))
    else {
        return;
    };
    let Some(slot) = window_slot_of(kind, pos, index) else {
        return;
    };
    let net = if stack.is_empty() {
        0
    } else {
        player.inventory.new_id()
    };
    if let Some(w) = player.inventory.window.as_mut()
        && let Some(slot_id) = w.ids.get_mut(usize::from(slot))
    {
        *slot_id = StackId { net, request: None };
    }
    player.send_packet(slot_packet(
        u32::from(id),
        u32::from(slot),
        stack.to_instance(net),
    ));
}

/// The response of a request that succeeded: every container and slot it named, grouped by
/// container in first-seen order, with the slots' contents now (as the official server: `Slot` =
/// `HotbarSlot`, no filtered name). The created output is left out.
fn response(
    inv: &Inventory,
    blocks: &block_entity::BlockEntities,
    request: i32,
    touched: &[(u8, u8)],
) -> ItemStackResponse {
    let mut containers: Vec<StackResponseContainerInfo> = Vec::new();
    // Where each container ID is in `containers`.
    let mut index_of: BTreeMap<u8, usize> = BTreeMap::new();
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
        if let Some(c) = index_of
            .get(&container)
            .and_then(|&at| containers.get_mut(at))
        {
            c.slot_info.push(info);
        } else {
            index_of.insert(container, containers.len());
            containers.push(StackResponseContainerInfo {
                container: FullContainerName {
                    container_id: container,
                    dynamic_container_id: None,
                },
                slot_info: vec![info],
            });
        }
    }
    ItemStackResponse {
        status: ItemStackResponseStatus::Ok as u8,
        request_id: request,
        container_info: containers,
    }
}

/// Whether a request slot is the created output (container 60, slot 50).
fn is_created_output(info: &StackRequestSlotInfo) -> bool {
    info.container.container_id == ContainerId::CreatedOutput as u8
        && info.slot == UI_CREATED_OUTPUT
}
