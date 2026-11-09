//! Player inventories and windows: the player's own slots, the cursor, open containers, and the
//! server-authoritative item stack requests that move items between them, with the official
//! server's window and stack network ID rules (P6, `xtask/data/p6/facts.md`).
//!
//! A player holds 36 main slots (the hotbar is 0–8), 4 armor slots, the off hand, the cursor and
//! its ender chest contents. Every slot carries the stack network ID the client knows its stack
//! by and the request that last changed it. IDs come from one counter per player (the official
//! server uses one per server, which the client cannot tell apart: it checks IDs per slot) and
//! follow the official server within a request ([`request`]). A change outside a request gives
//! the slot a new ID and is sent to the client.
//!
//! One window is open at a time: the player's own inventory, a container block (chest, double
//! chest, barrel, shulker box), the ender chest or a villager's trade window. Window IDs count
//! per player from 2, as on the official server. Opening while another window is open closes it
//! from the server side and sends the new one once the client acknowledges the close. Closing
//! gives the cursor and the window's own slots back to the inventory; what does not fit the
//! player throws, and what cannot be thrown (the world's entity limit) waits in a bounded list
//! given back as slots free up: no item is ever lost.
//!
//! A menu ([`menu`]) is a window of server-owned items on fake container blocks; requests move
//! items into and out of its editable slots only.
//!
//! The trade window keeps its payment slots in the window and trades through [`station`]. There
//! is no craft: no window has a craft grid, and a craft request is refused.
//!
//! Events: opening a window raises `InventoryOpen` and closing it `InventoryClose`; selecting a
//! hotbar slot `PlayerItemHeld`; picking a block `PlayerBlockPick`; the legacy drop
//! `PlayerDropItem`; a request its own ([`request`]). After a listener the rule checks again and
//! does nothing if the check fails, which includes a listener that changed the player's window
//! (it opened a menu); the public API ([`api`]) raises none. A direct call that closes a window
//! uses [`close_window_without_event`], a rule of the game [`close_window`].

mod api;
mod menu;
mod request;
mod station;

pub(super) use menu::{Handlers as Menus, tick as tick_menus};
pub(super) use request::on_item_stack_request;

use std::collections::{BTreeSet, VecDeque};
use std::fmt;

use tracing::warn;

use super::checkpoint::{self, Codec, Reader, Writer};
use super::entity::Raise;
use super::{Phase, Player, SessionId, State, block_entity, dispatch, interact, survival, trade};
use crate::block::{self, Class, ItemForm};
use crate::data::{MAX_PLAYER_TYPES, MAX_PLAYER_VALUE_BYTES};
use crate::event::{
    InventoryClose, InventoryOpen, PlayerBlockPick, PlayerDropItem, PlayerItemHeld,
    Window as PublicWindow,
};
use crate::inventory::CHEST_SLOTS;
use crate::item::{MAX_NBT_BYTES, Stack};
use crate::math::Vec3;
use crate::nbt::{Compound, List, Value};
use crate::protocol::consts::{
    ContainerId, ContainerType, InteractAction, InventoryActionSource, WINDOW_ID_ARMOUR,
    WINDOW_ID_INVENTORY, WINDOW_ID_OFF_HAND, WINDOW_ID_UI,
};
use crate::protocol::packet::{
    BlockPickRequest, ContainerClose, ContainerOpen, Interact, InventoryContent, InventorySlot,
    MobEquipment, Packet, PlayerHotBar,
};
use crate::protocol::{FullContainerName, InventoryAction, ItemInstance};
use crate::save::WorldIndex;
use crate::storage::codec::MAX_PLAYER_EXTRA_BYTES;
use crate::world::BlockPos;

/// The packets that announce a window: its `ContainerOpen` and its content.
const ANNOUNCE_PACKETS: usize = 2;
/// Armor slots: helmet, chestplate, leggings, boots.
const ARMOR: usize = crate::inventory::Kind::Armor.size();
/// Slots of the armor window the client takes (the fifth is a mount's body slot).
const ARMOR_WINDOW: usize = 5;
/// A barrel.
const BARREL: [Part; 1] = [block_part(ContainerId::Barrel, 0, CHEST_SLOTS)];
/// The slots a player carries items in: the main slots, armor and the off hand.
pub(super) const CARRIED: usize = MAIN + ARMOR + 1;
/// A single chest.
const CHEST: [Part; 1] = [block_part(ContainerId::LevelEntity, 0, CHEST_SLOTS)];
/// The type a server answers a window close with (-9 as a byte; P6).
const CONTAINER_TYPE_NONE: u8 = 247;
/// A double chest: both halves as one run.
const DOUBLE_CHEST: [Part; 1] = [block_part(ContainerId::LevelEntity, 0, 2 * CHEST_SLOTS)];
/// Where a player drops items, below its eyes: the tick keeps the eye position, so the drop
/// point is [`DROP_HEIGHT`] above the feet, this far below the eyes.
const DROP_BELOW_EYE: f64 = 0.32;
/// How high above the feet a player drops an item.
const DROP_HEIGHT: f64 = 1.3;
const _: () = assert!((survival::EYE_HEIGHT - DROP_HEIGHT - DROP_BELOW_EYE).abs() < 1e-9);
/// Ender chest slots.
const ENDER: usize = crate::inventory::Kind::EnderChest.size();
/// An ender chest.
const ENDER_CHEST: [Part; 1] = [part(ContainerId::LevelEntity, 0, byte(ENDER), Store::Ender)];
/// Hotbar slots.
const HOTBAR: u8 = crate::inventory::HOTBAR_SIZE;
/// The inventory window ID as the byte `MobEquipment` and `PlayerHotBar` carry.
pub(super) const INVENTORY_WINDOW_BYTE: u8 = window_byte(WINDOW_ID_INVENTORY);
/// The key of the saved armor list in the saved player data (Bedrock player NBT: armor in
/// order).
const KEY_ARMOR: &str = "Armor";
/// The key of the saved ender chest list (slots by `Slot` 0–26).
const KEY_ENDER_CHEST: &str = "EnderChestInventory";
/// The key of the saved main slots list (slots by `Slot` 0–35).
const KEY_INVENTORY: &str = "Inventory";
/// The key of a saved stack's item name.
const KEY_NAME: &str = "Name";
/// The key of the saved off hand list.
const KEY_OFFHAND: &str = "Offhand";
/// The key of Anchor's saved waiting stacks ([`save`]).
const KEY_PENDING: &str = "PendingItems";
/// The key of the selected hotbar slot in the saved player data.
const KEY_SELECTED_SLOT: &str = "SelectedInventorySlot";
/// The key of a saved stack's slot in a list by `Slot`.
const KEY_SLOT: &str = "Slot";
/// Main inventory slots: the hotbar (0–8) and the 27 above it.
const MAIN: usize = crate::inventory::Kind::Main.size();
/// The most stacks closing a window gives back: the cursor and the trade window's payments.
const MAX_GIVE_BACK: usize = 1 + TRADE_PAYMENTS;
/// Windows do not open while more than this many stacks wait for room less [`MAX_GIVE_BACK`]
/// (the waiting list leaves room for what a window may hold).
const MAX_PENDING: usize = 64;
/// Block picks a player may make in one tick; more do nothing. A pick is a key press, far below
/// one a tick, but one costs up to a shulker box's NBT built, checked and compared, whatever the
/// few bytes of its packet.
const MAX_PICKS_PER_TICK: u8 = 4;
/// The most request IDs remembered per player to refuse a repeat; clients number requests -1, -3,
/// -5…, so a repeat within this many is a replay, not a new request.
const MAX_RECENT_REQUESTS: usize = 256;
/// The most stacks a player waits with for room, and the most entries of a saved
/// `PendingItems` list (the waiting stacks and everything held outside the slots, [`save`]).
///
/// Invariant: the waiting stacks plus the non-empty stacks held outside the slots (the cursor,
/// the open window's own slots, and those a close took out while its menu's handler hears it,
/// `Inventory::closing`) are at most this many. It holds after a load (the
/// saved list is cut to this and nothing is held), a request or API write that would break it is
/// refused (`request::Error::Unreturnable`, `Error::Full`), and only a close turns a held stack
/// into at most one waiting stack (what [`give`] leaves of it). So the waiting list never passes
/// this bound and no item a player holds is ever lost for want of room.
pub(super) const MAX_SAVED_PENDING: usize = MAX_PENDING + MAX_GIVE_BACK;
/// The most stacks a saved player record holds: what the player carries, its ender chest and the
/// waiting stacks with what is held outside the slots.
const MAX_SAVED_STACKS: usize = CARRIED + ENDER + MAX_SAVED_PENDING;
// The record has room for that many stacks of the largest saved size and the user's data.
const _: () = assert!(
    MAX_SAVED_STACKS * MAX_NBT_BYTES + MAX_PLAYER_TYPES * MAX_PLAYER_VALUE_BYTES
        <= MAX_PLAYER_EXTRA_BYTES
);
/// The off hand window ID as the byte `MobEquipment` and `PlayerHotBar` carry.
pub(super) const OFFHAND_WINDOW_BYTE: u8 = window_byte(WINDOW_ID_OFF_HAND);
/// The `InventoryContent` packets of the player's own windows ([`own_windows`]).
const OWN_WINDOW_PACKETS: usize = 4;
/// A shulker box.
const SHULKER_BOX: [Part; 1] = [block_part(ContainerId::ShulkerBox, 0, CHEST_SLOTS)];
/// Ticks before anyone picks up a thrown item.
const THROW_PICKUP_DELAY_TICKS: i16 = 40;
/// The speed of a thrown item along the look.
const THROW_SPEED: f64 = 0.4;
/// A trade: payments 4 and 5 (containers 47 and 48; P6).
const TRADE: [Part; TRADE_PAYMENTS] = [
    part(
        ContainerId::TradeTwoIngredientOne,
        UI_TRADE,
        1,
        Store::Ui(TRADE_PAYMENT_A),
    ),
    part(
        ContainerId::TradeTwoIngredientTwo,
        UI_TRADE + 1,
        1,
        Store::Ui(TRADE_PAYMENT_B),
    ),
];
/// The window's own slot of a trade's first payment.
pub(super) const TRADE_PAYMENT_A: u8 = 0;
/// The window's own slot of a trade's second payment.
pub(super) const TRADE_PAYMENT_B: u8 = 1;
/// The payment slots of a trade window.
const TRADE_PAYMENTS: usize = 2;
/// The UI slot of the created output (container 60).
const UI_CREATED_OUTPUT: u8 = 50;
/// Slots of the UI window (124): the cursor at 0, the trade payments at 4 and 5, the created
/// output at 50.
const UI_SLOTS: usize = 54;
/// The first UI slot of the trade window.
const UI_TRADE: u8 = 4;
/// The first window ID; window IDs run from 2 to 99, then 1.
const WINDOW_FIRST: u8 = 1;
/// The window ID past the last one; window IDs run from 2 to 99, then 1.
const WINDOW_LAST: u8 = 100;
/// The window ID a client closes "the last window" with (-1).
const WINDOW_NONE: u8 = 255;

/// What one player carries and has open.
#[derive(Debug)]
pub(super) struct Inventory {
    main: Slots<MAIN>,
    armor: Slots<ARMOR>,
    offhand: Slots<1>,
    cursor: Slots<1>,
    ender: Slots<ENDER>,
    /// The selected hotbar slot, 0 to 8.
    held: u8,
    /// The next stack network ID.
    next_id: i32,
    /// The last window ID given.
    last_window: u8,
    window: Option<Window>,
    /// A window the server closed whose close the client has not acknowledged yet; a new window
    /// waits for it.
    pending_close: Option<u8>,
    /// The latest request IDs, oldest first, at most [`MAX_RECENT_REQUESTS`].
    recent: VecDeque<i32>,
    /// The same IDs, to look them up.
    recent_set: BTreeSet<i32>,
    /// Requests handled in a tick: (tick, count), at most the per-tick bound of [`request`].
    budget: (u64, u16),
    /// The tick of the last full resend after a refused request (once a tick at most).
    resent: Option<u64>,
    /// A hotbar selection the server did not take this tick waits for [`answer_held`].
    held_unanswered: bool,
    /// Block picks in a tick: (tick, count), at most [`MAX_PICKS_PER_TICK`].
    picks: (u64, u8),
    /// The tick the client last tried to open a window ([`first_client_open_this_tick`]).
    client_opened: Option<u64>,
    /// Stacks waiting for room in the inventory, at most [`MAX_SAVED_PENDING`] with what is held
    /// outside the slots (its invariant).
    pending: Vec<Stack>,
    /// The open menu, exactly while the window is a [`WindowKind::Menu`].
    menu: Option<menu::Open>,
    /// The number of the next menu ([`crate::menu::Id`]); numbers at or past 2^32 are refused.
    next_menu: u64,
    /// The creative inventory the player sees: its index in `custom::Registry::creatives`.
    pub(super) creative: u16,
    /// The stacks closes took out while their menu's handlers hear them ([`close_now`]): held
    /// outside the slots ([`MAX_SAVED_PENDING`]). 0 but while such a handler runs.
    closing: usize,
    /// How many of the cursor's items the open menu lends ([`crate::menu::Menu::lending`]): its
    /// handler's, which the player's saved data leaves out. 0 but while a menu that lends is open
    /// (its close takes them), and at most the cursor's count, all of its item.
    lent: u8,
}

impl Inventory {
    /// Helmet, chestplate, leggings, boots.
    pub(super) fn armor(&self) -> &[Stack; ARMOR] {
        &self.armor.stacks
    }

    /// The stack on the cursor.
    pub(super) fn cursor(&self) -> &Stack {
        self.cursor.stack(0)
    }

    /// Hashes the state kept between ticks (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            main,
            armor,
            offhand,
            cursor,
            ender,
            held,
            next_id,
            last_window,
            window,
            pending_close,
            recent,
            // The set holds the same IDs as `recent`.
            recent_set: _,
            // Per-tick rate limits: they reset every tick and never change items or windows, so
            // a refused repeat leaves the state it would have had (idempotence).
            budget: _,
            resent: _,
            // Cleared by the end of every tick.
            held_unanswered: _,
            picks: _,
            client_opened: _,
            pending,
            menu,
            next_menu,
            creative,
            // 0 but while a menu's handler hears a close.
            closing: _,
            lent,
        } = self;
        let mut slots = |stacks: &[Stack], ids: &[StackId]| {
            for (s, id) in stacks.iter().zip(ids) {
                hash_stack(h, s);
                hash_id(h, *id);
            }
        };
        slots(&main.stacks, &main.ids);
        slots(&armor.stacks, &armor.ids);
        slots(&offhand.stacks, &offhand.ids);
        slots(&cursor.stacks, &cursor.ids);
        slots(&ender.stacks, &ender.ids);
        h.u64(u64::from(*held));
        h.u64(u64::from(next_id.cast_unsigned()));
        h.u64(u64::from(*last_window));
        match window {
            Some(Window { id, kind, ui, ids }) => {
                h.u64(1 + id.map_or(0, u64::from));
                h.bytes(format!("{kind:?}").as_bytes());
                h.usize(ui.len());
                for s in ui {
                    hash_stack(h, &s.stack);
                    hash_id(h, s.id);
                }
                h.usize(ids.len());
                for id in ids {
                    hash_id(h, *id);
                }
            }
            None => h.u64(0),
        }
        h.u64(pending_close.map_or(0, |w| 1 + u64::from(w)));
        h.usize(recent.len());
        for r in recent {
            h.u64(u64::from(r.cast_unsigned()));
        }
        h.usize(pending.len());
        for s in pending {
            hash_stack(h, s);
        }
        match menu {
            Some(m) => m.hash_into(h),
            None => h.u64(0),
        }
        h.u64(*next_menu);
        h.u64(u64::from(*creative));
        h.u64(u64::from(*lent));
    }

    /// The stack in the selected hotbar slot.
    pub(super) fn held(&self) -> &Stack {
        self.main.stack(self.held)
    }

    /// The selected hotbar slot, 0 to 8.
    pub(super) fn held_slot(&self) -> u8 {
        self.held
    }

    /// The stack in main slot `slot` (0 to 35).
    pub(super) fn main_stack(&self, slot: u8) -> Option<&Stack> {
        self.main.stacks.get(usize::from(slot))
    }

    /// The stack in the off hand.
    pub(super) fn offhand(&self) -> &Stack {
        self.offhand.stack(0)
    }

    /// The first main slot holding the item of `stack` (any count; the extra data counts only if
    /// `stack` has some: [`crate::inventory::Inventory::first`]).
    fn first(&self, stack: &Stack) -> Option<u8> {
        self.main_view()
            .first(stack)
            .and_then(|i| u8::try_from(i).ok())
    }

    /// The first empty main slot.
    fn first_empty(&self) -> Option<u8> {
        self.main_view()
            .first_empty()
            .and_then(|i| u8::try_from(i).ok())
    }

    /// The player's slot `r` (not a block container slot).
    fn get(&self, r: SlotRef) -> Option<Slot> {
        match r {
            SlotRef::Main(i) => self.main.get(i),
            SlotRef::Armor(i) => self.armor.get(i),
            SlotRef::Offhand => self.offhand.get(0),
            SlotRef::Cursor => self.cursor.get(0),
            SlotRef::Ender(i) => self.ender.get(i),
            SlotRef::Block(_) => None,
            SlotRef::Ui(i) => self.window.as_ref()?.ui.get(usize::from(i)).cloned(),
            SlotRef::Menu(i) => Some(Slot {
                stack: self.menu.as_ref()?.stacks.get(usize::from(i))?.clone(),
                id: *self.window.as_ref()?.ids.get(usize::from(i))?,
            }),
        }
    }

    /// Whether a window may open: few enough stacks wait for room that what it holds can go back
    /// (at most [`MAX_PENDING`] less [`MAX_GIVE_BACK`] wait).
    fn has_room_to_open(&self) -> bool {
        self.pending.len() + MAX_GIVE_BACK <= MAX_PENDING
    }

    /// The slot a request's container and slot name, if it exists now: the player's own
    /// containers always, the window's while it is open.
    fn locate(&self, container: u8, slot: u8) -> Option<SlotRef> {
        let main = |s: u8| (usize::from(s) < MAIN).then_some(SlotRef::Main(s));
        match container {
            c if c == ContainerId::CombinedHotBarAndInventory as u8
                || c == ContainerId::HotBar as u8
                || c == ContainerId::Inventory as u8 =>
            {
                main(slot)
            }
            c if c == ContainerId::Armor as u8 => {
                (usize::from(slot) < ARMOR).then_some(SlotRef::Armor(slot))
            }
            // The client sends a wrong slot for the off hand; it has one slot anyway.
            c if c == ContainerId::Offhand as u8 => Some(SlotRef::Offhand),
            c if c == ContainerId::Cursor as u8 => (slot == 0).then_some(SlotRef::Cursor),
            _ => {
                let window = self.window.as_ref().filter(|w| w.id.is_some())?;
                if let WindowKind::Menu { .. } = window.kind {
                    let editable = self.menu.as_ref()?.is_editable(usize::from(slot));
                    return (container == ContainerId::LevelEntity as u8 && editable)
                        .then_some(SlotRef::Menu(slot));
                }
                let part = window.kind.parts().iter().find(|p| {
                    p.container == container
                        && slot >= p.first
                        && u16::from(slot) < u16::from(p.first) + u16::from(p.len)
                })?;
                let at = slot - part.first;
                Some(match part.store {
                    Store::Block(first) => SlotRef::Block(first.checked_add(at)?),
                    Store::Ui(first) => SlotRef::Ui(first.checked_add(at)?),
                    Store::Ender => SlotRef::Ender(at),
                })
            }
        }
    }

    /// The main slots as an inventory to read.
    fn main_view(&self) -> crate::inventory::Inventory<'_> {
        crate::inventory::Inventory::new(&self.main.stacks, &[], crate::inventory::MAX_STACK)
    }

    /// Puts `stack` where it fits: first onto stacks of the same item with room, in slot order,
    /// then into empty slots; changed slots get new IDs. Returns what did not fit and the main
    /// slots changed.
    fn add(&mut self, stack: Stack) -> (Stack, Vec<u8>) {
        let (writes, left) = self.main_view().plan_add(&stack);
        if writes.is_empty() {
            return (stack, Vec::new());
        }
        let mut changed = Vec::with_capacity(writes.len());
        for (i, s) in writes {
            #[expect(
                clippy::expect_used,
                reason = "invariant: plan_add names only the 36 main slots"
            )]
            let i = u8::try_from(i).expect("a main slot number is a byte");
            let net = self.new_id();
            let put = self.main.put(
                i,
                Slot {
                    stack: s,
                    id: StackId { net, request: None },
                },
            );
            #[expect(
                clippy::expect_used,
                reason = "invariant: plan_add names only the 36 main slots"
            )]
            put.then_some(()).expect("plan_add names a main slot");
            changed.push(i);
        }
        (left, changed)
    }

    /// A new stack network ID ([`take_stack_id`]).
    fn new_id(&mut self) -> i32 {
        take_stack_id(&mut self.next_id)
    }

    /// The ID of a window opening now ([`WINDOW_FIRST`]); every window, a trade window too,
    /// counts on the same `last_window`.
    fn next_window_id(&mut self) -> u8 {
        self.last_window = (self.last_window.wrapping_add(1) % WINDOW_LAST).max(WINDOW_FIRST);
        self.last_window
    }

    /// Sets the player's slot `r`; false if there is no such slot.
    fn put(&mut self, r: SlotRef, slot: Slot) -> bool {
        match r {
            SlotRef::Main(i) => self.main.put(i, slot),
            SlotRef::Armor(i) => self.armor.put(i, slot),
            SlotRef::Offhand => self.offhand.put(0, slot),
            SlotRef::Cursor => self.cursor.put(0, slot),
            SlotRef::Ender(i) => self.ender.put(i, slot),
            SlotRef::Block(_) => false,
            SlotRef::Ui(i) => match self
                .window
                .as_mut()
                .and_then(|w| w.ui.get_mut(usize::from(i)))
            {
                Some(s) => {
                    *s = slot;
                    true
                }
                None => false,
            },
            SlotRef::Menu(i) => {
                let i = usize::from(i);
                match (
                    self.menu.as_mut().and_then(|m| m.stacks.get_mut(i)),
                    self.window.as_mut().and_then(|w| w.ids.get_mut(i)),
                ) {
                    (Some(stack), Some(id)) => {
                        *stack = slot.stack;
                        *id = slot.id;
                        true
                    }
                    _ => false,
                }
            }
        }
    }
}

impl Default for Inventory {
    fn default() -> Self {
        Self {
            main: Slots::default(),
            armor: Slots::default(),
            offhand: Slots::default(),
            cursor: Slots::default(),
            ender: Slots::default(),
            held: 0,
            next_id: 1,
            last_window: WINDOW_FIRST,
            window: None,
            pending_close: None,
            recent: VecDeque::with_capacity(MAX_RECENT_REQUESTS),
            recent_set: BTreeSet::new(),
            budget: (0, 0),
            resent: None,
            held_unanswered: false,
            picks: (0, 0),
            client_opened: None,
            pending: Vec::new(),
            menu: None,
            next_menu: 0,
            creative: 0,
            closing: 0,
            lent: 0,
        }
    }
}

/// Every field but the set of recent request IDs, which is `recent` again, and the held answer,
/// the block picks, the client's window open and the stacks of a close a handler hears, which
/// only count in their own tick: a checkpoint is taken after its tick.
impl Codec for Inventory {
    fn encode(&self, w: &mut Writer) {
        let Self {
            main,
            armor,
            offhand,
            cursor,
            ender,
            held,
            next_id,
            last_window,
            window,
            pending_close,
            recent,
            recent_set: _,
            budget,
            resent,
            held_unanswered: _,
            picks: _,
            client_opened: _,
            pending,
            menu,
            next_menu,
            creative,
            closing: _,
            lent,
        } = self;
        main.encode(w);
        armor.encode(w);
        offhand.encode(w);
        cursor.encode(w);
        ender.encode(w);
        held.encode(w);
        next_id.encode(w);
        last_window.encode(w);
        window.encode(w);
        pending_close.encode(w);
        recent.encode(w);
        budget.encode(w);
        resent.encode(w);
        pending.encode(w);
        menu.encode(w);
        next_menu.encode(w);
        creative.encode(w);
        lent.encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> checkpoint::Result<Self> {
        let main = Codec::decode(r)?;
        let armor = Codec::decode(r)?;
        let offhand = Codec::decode(r)?;
        let cursor = Codec::decode(r)?;
        let ender = Codec::decode(r)?;
        let held = Codec::decode(r)?;
        let next_id = Codec::decode(r)?;
        let last_window = Codec::decode(r)?;
        let window = Codec::decode(r)?;
        let pending_close = Codec::decode(r)?;
        let recent: VecDeque<i32> = Codec::decode(r)?;
        let recent_set = recent.iter().copied().collect();
        Ok(Self {
            main,
            armor,
            offhand,
            cursor,
            ender,
            held,
            next_id,
            last_window,
            window,
            pending_close,
            recent,
            recent_set,
            budget: Codec::decode(r)?,
            resent: Codec::decode(r)?,
            held_unanswered: false,
            picks: (0, 0),
            client_opened: None,
            pending: Codec::decode(r)?,
            menu: Codec::decode(r)?,
            next_menu: Codec::decode(r)?,
            creative: Codec::decode(r)?,
            closing: 0,
            lent: Codec::decode(r)?,
        })
    }
}

/// A fixed run of a player's slots: the stacks, and their IDs beside them.
#[derive(Debug, Clone, PartialEq)]
struct Slots<const N: usize> {
    stacks: [Stack; N],
    ids: [StackId; N],
}

impl<const N: usize> Slots<N> {
    fn get(&self, i: u8) -> Option<Slot> {
        let i = usize::from(i);
        Some(Slot {
            stack: self.stacks.get(i)?.clone(),
            id: *self.ids.get(i)?,
        })
    }

    /// The network form of every slot.
    fn instances(&self) -> impl Iterator<Item = ItemInstance> + '_ {
        self.stacks
            .iter()
            .zip(&self.ids)
            .map(|(s, id)| s.to_instance(id.net))
    }

    fn stack(&self, i: u8) -> &Stack {
        self.stacks.get(usize::from(i)).unwrap_or(&Stack::EMPTY)
    }

    fn put(&mut self, i: u8, slot: Slot) -> bool {
        let i = usize::from(i);
        match (self.stacks.get_mut(i), self.ids.get_mut(i)) {
            (Some(stack), Some(id)) => {
                *stack = slot.stack;
                *id = slot.id;
                true
            }
            _ => false,
        }
    }

    /// Empties every slot, putting the stacks that were not empty into `out`.
    fn take_all(&mut self, out: &mut Vec<Stack>) {
        for (stack, id) in self.stacks.iter_mut().zip(&mut self.ids) {
            if !stack.is_empty() {
                out.push(std::mem::take(stack));
            }
            *id = StackId::default();
        }
    }
}

impl<const N: usize> Default for Slots<N> {
    fn default() -> Self {
        Self {
            stacks: std::array::from_fn(|_| Stack::EMPTY),
            ids: [StackId::default(); N],
        }
    }
}

impl<const N: usize> Codec for Slots<N> {
    fn encode(&self, w: &mut Writer) {
        let Self { stacks, ids } = self;
        stacks.encode(w);
        ids.encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> checkpoint::Result<Self> {
        Ok(Self {
            stacks: Codec::decode(r)?,
            ids: Codec::decode(r)?,
        })
    }
}

/// A slot: its stack and stack network ID.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Slot {
    stack: Stack,
    id: StackId,
}

checkpoint::fields!(Slot { stack, id });

/// The stack network ID of a slot as one player's client knows it, and the request that last
/// changed the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct StackId {
    /// 0 for an empty slot.
    net: i32,
    request: Option<i32>,
}

checkpoint::fields!(StackId { net, request });

/// A slot a request names, resolved to where it lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SlotRef {
    Main(u8),
    Armor(u8),
    Offhand,
    Cursor,
    /// The player's ender chest contents (through an ender chest window).
    Ender(u8),
    /// A slot of the open block container (a double chest's right half from 27).
    Block(u8),
    /// A slot the open window keeps itself (the trade payments).
    Ui(u8),
    /// An editable slot of the open menu.
    Menu(u8),
}

/// An open window.
#[derive(Debug, Clone, PartialEq)]
struct Window {
    /// The window ID; `None` while it waits for the close of the previous window to be
    /// acknowledged.
    id: Option<u8>,
    kind: WindowKind,
    /// The window's own slots (the trade payments), given back on close.
    ui: Vec<Slot>,
    /// The client's stack IDs of the block container slots, by window slot.
    ids: Vec<StackId>,
}

impl Window {
    /// A window of `kind` with ID `id` (`None` while it waits to open) and nothing in its own
    /// slots.
    fn new(kind: WindowKind, id: Option<u8>) -> Self {
        Self {
            id,
            kind,
            ui: vec![Slot::default(); kind.ui_slots()],
            ids: Vec::new(),
        }
    }
}

checkpoint::fields!(Window { id, kind, ui, ids });

/// What a window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowKind {
    /// The player's own inventory (opened with `Interact`).
    Own,
    /// A container block; a double chest from its left half (`pos`) and its right half.
    Container {
        world: WorldIndex,
        pos: BlockPos,
        pair: Option<BlockPos>,
        kind: Container,
    },
    /// An ender chest: the player's own ender contents.
    Ender { world: WorldIndex, pos: BlockPos },
    /// Trading with a villager NPC: two payment slots kept by the window (P6).
    Trade { villager: crate::entity::Id },
    /// A menu on fake container blocks from `pos` (a double chest's other half beside it); its
    /// items are [`Inventory::menu`]'s, its slots' IDs the window's `ids`.
    Menu {
        pos: BlockPos,
        kind: crate::menu::Kind,
    },
}

impl WindowKind {
    /// Block container slots the window shows.
    fn block_slots(self) -> usize {
        self.parts()
            .iter()
            .filter(|p| matches!(p.store, Store::Block(_)))
            .map(|p| usize::from(p.len))
            .sum()
    }

    /// The blocks the window shows: its block, and a double chest's other half.
    fn blocks(self) -> [Option<(WorldIndex, BlockPos)>; 2] {
        match self {
            Self::Own | Self::Trade { .. } | Self::Menu { .. } => [None, None],
            Self::Container {
                world, pos, pair, ..
            } => [Some((world, pos)), pair.map(|p| (world, p))],
            Self::Ender { world, pos } => [Some((world, pos)), None],
        }
    }

    /// The `ContainerOpen` type.
    fn container_type(self) -> u8 {
        let t = match self {
            Self::Own => ContainerType::Inventory,
            Self::Container { .. } | Self::Ender { .. } => ContainerType::Container,
            Self::Trade { .. } => ContainerType::Trade,
            Self::Menu { kind, .. } => match kind {
                crate::menu::Kind::Hopper => ContainerType::Hopper,
                crate::menu::Kind::Chest | crate::menu::Kind::DoubleChest => {
                    ContainerType::Container
                }
            },
        };
        t as u8
    }

    /// Whether the block shows viewers (a lid, a barrel's `open_bit`).
    fn is_animated(self) -> bool {
        matches!(self, Self::Container { .. } | Self::Ender { .. })
    }

    /// The UI slot (window 124) the window's own slot `i` is shown in.
    fn ui_slot(self, i: u8) -> Option<u8> {
        self.parts().iter().find_map(|p| match p.store {
            Store::Ui(first) if i >= first && i - first < p.len => Some(p.first + (i - first)),
            _ => None,
        })
    }

    /// Slots the window keeps itself.
    fn ui_slots(self) -> usize {
        self.parts()
            .iter()
            .filter(|p| matches!(p.store, Store::Ui(_)))
            .map(|p| usize::from(p.len))
            .sum()
    }

    /// The request slots of the window; a menu has none here, as [`Inventory::locate`] finds its
    /// editable slots by the menu.
    fn parts(self) -> &'static [Part] {
        match self {
            Self::Own | Self::Menu { .. } => &[],
            Self::Container { kind, pair, .. } => match kind {
                Container::Chest if pair.is_some() => &DOUBLE_CHEST,
                Container::Chest => &CHEST,
                Container::Barrel => &BARREL,
                Container::ShulkerBox => &SHULKER_BOX,
            },
            Self::Ender { .. } => &ENDER_CHEST,
            Self::Trade { .. } => &TRADE,
        }
    }
}

checkpoint::variants!(WindowKind {
    0 => Own,
    1 => Container { world, pos, pair, kind },
    2 => Ender { world, pos },
    3 => Trade { villager },
    4 => Menu { pos, kind },
});

/// A container block kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    Chest,
    Barrel,
    ShulkerBox,
}

checkpoint::variants!(Container {
    0 => Chest,
    1 => Barrel,
    2 => ShulkerBox,
});

/// What tells the window a player has open apart from a window opened in its place: its ID, its
/// kind and its menu (a new menu has a new [`crate::menu::Id`], since menu IDs never repeat).
type WindowKey = (Option<u8>, WindowKind, Option<crate::menu::Id>);

/// A run of request slots of a window and where they live.
#[derive(Debug, Clone, Copy)]
struct Part {
    /// The request container (`ContainerId`).
    container: u8,
    /// The first request slot.
    first: u8,
    len: u8,
    store: Store,
}

/// Where the slots of a [`Part`] live.
#[derive(Debug, Clone, Copy)]
enum Store {
    /// The block container, from this window slot.
    Block(u8),
    /// The window's own slots, from this one.
    Ui(u8),
    /// The player's ender contents.
    Ender,
}

/// Whether closing a window tells the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Notice {
    /// The server closes it: the client gets a server-side `ContainerClose` and the close waits
    /// for its acknowledgement.
    Send,
    /// The client closed it itself, or a new screen replaces it on the client.
    Skip,
}

/// What a picked block's item carries besides its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickData {
    /// Only the item.
    Plain,
    /// The block entity's data too.
    WithBlockEntity,
}

/// A window ID as a byte; only the constants use it, so an ID that does not fit fails the build.
const fn window_byte(id: u32) -> u8 {
    assert!(id < 256, "the window ID fits a byte");
    #[expect(
        clippy::cast_possible_truncation,
        reason = "invariant: the assertion above keeps the ID within a byte"
    )]
    let byte = id as u8;
    byte
}

/// A part of a block container whose request slots `first..first + len` are the block slots of
/// the same numbers.
const fn block_part(container: ContainerId, first: usize, len: usize) -> Part {
    part(container, byte(first), byte(len), Store::Block(byte(first)))
}

const fn part(container: ContainerId, first: u8, len: u8, store: Store) -> Part {
    Part {
        container: container as u8,
        first,
        len,
        store,
    }
}

/// A block container slot number or count as a window slot number: the tables above are
/// constants, so a number past a byte fails the build.
const fn byte(n: usize) -> u8 {
    assert!(n < 256, "a window slot number is a byte");
    #[expect(
        clippy::cast_possible_truncation,
        reason = "invariant: checked just above"
    )]
    let b = n as u8;
    b
}

/// Reads the player's inventory keys from its saved extra (empty for a new player).
///
/// Keys (Bedrock player NBT): [`KEY_INVENTORY`] (main slots by `Slot` 0–35), [`KEY_ARMOR`] (4
/// items), [`KEY_OFFHAND`] (1 item), [`KEY_ENDER_CHEST`] (by `Slot` 0–26),
/// `SelectedInventorySlot`; and Anchor's [`KEY_PENDING`] (stacks waiting for room, and what was
/// held outside the slots: see [`save`]), at most [`MAX_SAVED_PENDING`]. Unknown items are left
/// out with a warning that names the player `player`, the key, the slot and the item; the next
/// save does not write them back, so an item whose custom definition was renamed or removed is
/// lost for good. Custom items are read through `custom`, the server's custom content.
pub(super) fn load(
    inventory: &mut Inventory,
    extra: &Compound,
    custom: &crate::custom::Registry,
    player: &str,
) {
    let list = |key: &str| match extra.get(key) {
        Some(Value::List(List::Compound(list))) => list.as_slice(),
        _ => &[],
    };
    let read = |key: &str, slot: usize, nbt: &Compound| {
        read_saved_stack(
            nbt,
            custom,
            format_args!("player {player}, {key} slot {slot}"),
        )
        .unwrap_or(Stack::EMPTY)
    };
    let by_slot = |key: &str, stacks: &mut [Stack]| {
        for nbt in list(key) {
            if let Some(Value::Byte(i)) = nbt.get(KEY_SLOT)
                && let Some(stack) = stacks.get_mut(usize::from(*i))
            {
                *stack = read(key, usize::from(*i), nbt);
            }
        }
    };
    let in_order = |key: &str, stacks: &mut [Stack]| {
        for (slot, (stack, nbt)) in stacks.iter_mut().zip(list(key)).enumerate() {
            *stack = read(key, slot, nbt);
        }
    };
    by_slot(KEY_INVENTORY, &mut inventory.main.stacks);
    by_slot(KEY_ENDER_CHEST, &mut inventory.ender.stacks);
    in_order(KEY_ARMOR, &mut inventory.armor.stacks);
    in_order(KEY_OFFHAND, &mut inventory.offhand.stacks);
    inventory.pending = list(KEY_PENDING)
        .iter()
        .take(MAX_SAVED_PENDING)
        .enumerate()
        .map(|(slot, nbt)| read(KEY_PENDING, slot, nbt))
        .filter(|s| !s.is_empty())
        .collect();
    if let Some(Value::Int(held)) = extra.get(KEY_SELECTED_SLOT)
        && let Ok(held) = u8::try_from(*held)
        && held < HOTBAR
    {
        inventory.held = held;
    }
    let Inventory {
        main,
        armor,
        offhand,
        ender,
        next_id,
        ..
    } = inventory;
    let mut assign = |stacks: &[Stack], ids: &mut [StackId]| {
        for (stack, id) in stacks.iter().zip(ids) {
            *id = StackId {
                net: id_for(next_id, stack),
                request: None,
            };
        }
    };
    assign(&main.stacks, &mut main.ids);
    assign(&armor.stacks, &mut armor.ids);
    assign(&offhand.stacks, &mut offhand.ids);
    assign(&ender.stacks, &mut ender.ids);
}

/// The stack a saved item holds, read through `custom`, or `None` for an unknown item. The warning
/// names whose item it was (`owner`, with its place) and says the next save does not write the
/// item back, so an item whose custom definition was renamed or removed is lost for good.
pub(super) fn read_saved_stack(
    nbt: &Compound,
    custom: &crate::custom::Registry,
    owner: impl fmt::Display,
) -> Option<Stack> {
    let stack = Stack::from_nbt_with(nbt, custom);
    if stack.is_none() {
        let item = match nbt.get(KEY_NAME) {
            Some(Value::String(name)) => name.as_str(),
            _ => "",
        };
        warn!(
            %owner,
            item,
            "a saved item is unknown; the next save drops it for good (register it if it is a custom item)"
        );
    }
    stack
}

/// Writes the player's inventory keys into the extra to save (the keys of [`load`]).
///
/// `PendingItems` holds the stacks waiting for room and then everything the player holds outside
/// its slots: the cursor and the open window's own slots (the trade payments). A commit thus
/// holds every item the player carries exactly once, and a crash before the window closes loses
/// none. A block container's slots are its block entity's, saved with its chunk, so they are not
/// here, nor are the cursor's lent items, which the menu's handler keeps
/// ([`crate::menu::Menu::lending`]). A loaded player has no window open: the list comes back as
/// waiting stacks ([`restore`]).
pub(super) fn save(inventory: &Inventory, extra: &mut Compound) {
    let by_slot = |stacks: &[Stack]| {
        let list = (0_u8..)
            .zip(stacks)
            .filter(|(_, s)| !s.is_empty())
            .map(|(i, s)| {
                let mut nbt = s.to_nbt();
                nbt.insert(KEY_SLOT.to_owned(), Value::Byte(i));
                nbt
            })
            .collect();
        Value::List(List::Compound(list))
    };
    let all =
        |stacks: &[Stack]| Value::List(List::Compound(stacks.iter().map(Stack::to_nbt).collect()));
    extra.insert(KEY_INVENTORY.to_owned(), by_slot(&inventory.main.stacks));
    extra.insert(KEY_ARMOR.to_owned(), all(&inventory.armor.stacks));
    extra.insert(KEY_OFFHAND.to_owned(), all(&inventory.offhand.stacks));
    extra.insert(KEY_ENDER_CHEST.to_owned(), by_slot(&inventory.ender.stacks));
    extra.insert(
        KEY_SELECTED_SLOT.to_owned(),
        Value::Int(i32::from(inventory.held)),
    );
    let window = inventory
        .window
        .as_ref()
        .map_or(&[][..], |w| w.ui.as_slice());
    let cursor = own_part(inventory.cursor(), inventory.lent);
    let held: Vec<Compound> = inventory
        .pending
        .iter()
        .chain([&cursor])
        .chain(window.iter().map(|s| &s.stack))
        .filter(|s| !s.is_empty())
        .map(Stack::to_nbt)
        .collect();
    extra.insert(KEY_PENDING.to_owned(), Value::List(List::Compound(held)));
}

/// Puts the waiting stacks of a player just loaded where they fit, without packets (the
/// start-game window sends the whole inventory): what a commit held on the cursor or in a window
/// is back in the slots at once.
pub(super) fn restore(inventory: &mut Inventory) {
    let pending = std::mem::take(&mut inventory.pending);
    for stack in pending {
        let (left, _) = inventory.add(stack);
        if !left.is_empty() {
            inventory.pending.push(left);
        }
    }
}

/// Appends the inventory packets a spawning player gets in the start-game window (P6:
/// `InventoryContent` ×4, `PlayerHotBar`).
pub(super) fn spawn_packets(player: &Player, out: &mut Vec<Packet>) {
    own_windows(&player.inventory, out);
    out.push(hotbar_packet(player.inventory.held));
}

/// Handles `MobEquipment`, the client selecting a hotbar slot: the off hand window is ignored; a
/// slot outside the hotbar is answered with the selected one at the end of the tick
/// ([`answer_held`]).
pub(super) fn on_mob_equipment(state: &mut State, session: SessionId, packet: &MobEquipment) {
    let heard = dispatch::heard::<PlayerItemHeld>(state);
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if player.phase == Phase::Loading || u32::from(packet.window_id) != WINDOW_ID_INVENTORY {
        return;
    }
    let slot = packet.hot_bar_slot;
    if slot >= HOTBAR {
        player.inventory.held_unanswered = true;
        return;
    }
    if player.inventory.held == slot {
        return;
    }
    if !heard {
        hold(player, slot);
        return;
    }
    let before = player.inventory.held;
    let mut event = PlayerItemHeld {
        player: crate::player::Id::new(session),
        item: player.inventory.main.stack(slot).clone(),
        slot: usize::from(slot),
        cancelled: false,
    };
    dispatch::call(state, &mut event);
    let listened = dispatch::listened::<PlayerItemHeld>(state);
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    // A listener that selected a slot itself already told the client: its selection stands.
    if event.cancelled || (listened && player.inventory.held != before) {
        player.inventory.held_unanswered = true;
        return;
    }
    hold(player, slot);
    dispatch::watch(state, &event);
}

/// Tells the client which hotbar slot is selected and what it holds if the server did not take a
/// selection this tick; the end of the tick calls it. One answer with the selection the tick
/// ended with says what an answer to each refusal would, as the tick's packets reach the client
/// together, and each answer copies the stack's NBT.
pub(super) fn answer_held(player: &mut Player) {
    if !std::mem::take(&mut player.inventory.held_unanswered) {
        return;
    }
    let held = player.inventory.held;
    let slot = player.inventory.main.get(held).unwrap_or_default();
    let runtime_id = player.runtime_id;
    player.send_packet(Packet::MobEquipment(MobEquipment {
        entity_runtime_id: runtime_id,
        new_item: slot.stack.to_instance(slot.id.net),
        inventory_slot: held,
        hot_bar_slot: held,
        window_id: INVENTORY_WINDOW_BYTE,
    }));
}

/// Whether a hotbar selection the server did not take this tick waits for [`answer_held`].
pub(super) fn has_unanswered_hold(player: &Player) -> bool {
    player.inventory.held_unanswered
}

/// Handles `ContainerClose`, the client closing a window: the window's items go back and the close
/// is acknowledged with type 247 (P6). Window -1 means the latest window. If it acknowledges a
/// close the server started, the window waiting for it opens now.
pub(super) fn on_container_close(state: &mut State, session: SessionId, packet: &ContainerClose) {
    let Some(player) = state.players.get(&session) else {
        return;
    };
    if player.phase == Phase::Loading {
        return;
    }
    let inv = &player.inventory;
    let id = if packet.window_id == WINDOW_NONE {
        inv.last_window
    } else {
        packet.window_id
    };
    let current = inv.window.as_ref().is_some_and(|w| w.id == Some(id));
    if current && inv.pending_close != Some(id) {
        close(state, session, Notice::Skip);
    }
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    player.send_packet(Packet::ContainerClose(ContainerClose {
        window_id: id,
        container_type: CONTAINER_TYPE_NONE,
        server_side: false,
    }));
    if player.inventory.pending_close == Some(id) {
        player.inventory.pending_close = None;
        announce(state, session);
    }
}

/// Handles `Interact`: the client opened its own inventory (`OpenInventory` on itself). Other
/// actions belong to other modules or are ignored.
pub(super) fn on_interact(state: &mut State, session: SessionId, packet: &Interact) {
    let Some(player) = state.players.get(&session) else {
        return;
    };
    if packet.action_type != InteractAction::OpenInventory as u8
        || packet.target_entity_runtime_id != player.runtime_id
    {
        return;
    }
    open_checked(state, session, |state| {
        can_open(state, session).then_some(WindowKind::Own)
    });
}

/// Handles `BlockPickRequest`, the creative pick block: puts the block's item (with its block
/// entity data when asked) in the player's hand ([`equip_or_add`]). Players with finite resources
/// only select or swap items they have. At most [`MAX_PICKS_PER_TICK`] picks a tick are handled.
pub(super) fn on_block_pick_request(
    state: &mut State,
    session: SessionId,
    packet: &BlockPickRequest,
) {
    let tick = state.tick;
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let picks = &mut player.inventory.picks;
    if picks.0 != tick {
        *picks = (tick, 0);
    }
    if picks.1 >= MAX_PICKS_PER_TICK {
        return;
    }
    picks.1 = picks.1.saturating_add(1);
    let [x, y, z] = packet.position;
    let pos = BlockPos::new(x, y, z);
    let data = if packet.add_block_nbt {
        PickData::WithBlockEntity
    } else {
        PickData::Plain
    };
    let Some((world, block, picked)) = pick(state, session, pos, data) else {
        return;
    };
    let mut event = dispatch::heard::<PlayerBlockPick>(state).then(|| PlayerBlockPick {
        player: crate::player::Id::new(session),
        world: crate::world::Id::of(world),
        pos,
        block,
        result_item: picked.clone(),
        cancelled: false,
    });
    if let Some(e) = &mut event {
        let held = held_of(state, session);
        dispatch::call(state, e);
        if e.cancelled
            || (dispatch::listened::<PlayerBlockPick>(state)
                && (pick(state, session, pos, data).map(|(_, _, p)| p) != Some(picked.clone())
                    || held_of(state, session) != held))
        {
            return;
        }
    }
    if let Some(player) = state.players.get_mut(&session) {
        equip_or_add(player, picked);
    }
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
}

/// Puts a picked block's or entity's item in the player's hand: selected if the inventory has it
/// in the hotbar, swapped with the held slot if elsewhere, else put into the first empty slot
/// (selected in the hotbar, else swapped with the held slot), else into the held slot.
pub(super) fn equip_or_add(player: &mut Player, picked: Stack) {
    let held = player.inventory.held;
    match player.inventory.first(&picked) {
        Some(slot) if slot < HOTBAR => select(player, slot),
        Some(slot) => swap_main(player, held, slot),
        None => match player.inventory.first_empty() {
            Some(slot) if slot < HOTBAR => {
                set_own(player, SlotRef::Main(slot), picked);
                select(player, slot);
            }
            Some(slot) => {
                swap_main(player, held, slot);
                set_own(player, SlotRef::Main(held), picked);
            }
            None => set_own(player, SlotRef::Main(held), picked),
        },
    }
}

/// What picking the block at `pos` gives `session` now: its world, the block and the item;
/// `None` if nothing would happen (no block item, or a player with finite resources who does not
/// carry it; no event is raised then).
fn pick(
    state: &State,
    session: SessionId,
    pos: BlockPos,
    data: PickData,
) -> Option<(WorldIndex, block::State, Stack)> {
    let player = state.players.get(&session)?;
    if player.phase == Phase::Loading {
        return None;
    }
    let world = player.world;
    let finite = survival::has_finite_resources(player.game_mode());
    let block = state.worlds.get(world).and_then(|w| w.block(pos))?;
    let own = interact::drops::own_item(state, world, pos, block)?;
    let picked = match (block.item(), data) {
        (ItemForm::Item(_), PickData::WithBlockEntity) => {
            block_entity::with_block_data(state, world, pos, own)
        }
        _ => own,
    };
    if finite && player.inventory.first(&picked).is_none() {
        return None;
    }
    Some((world, block, picked))
}

/// Whether the player's main inventory holds `stack` ([`Inventory::first`] finds it).
pub(super) fn has(player: &Player, stack: &Stack) -> bool {
    player.inventory.first(stack).is_some()
}

/// Selects hotbar slot `slot`, stopping the item in use, and tells the client.
fn select(player: &mut Player, slot: u8) {
    hold(player, slot);
    player.send_packet(hotbar_packet(slot));
}

/// Selects hotbar slot `slot`, stopping the item in use, without telling the client: the client
/// selected it, or [`select`] tells it.
fn hold(player: &mut Player, slot: u8) {
    player.inventory.held = slot;
    hand_changed(player);
}

/// Swaps two main slots outside a request.
fn swap_main(player: &mut Player, a: u8, b: u8) {
    let sa = player.inventory.main.stack(a).clone();
    let sb = player.inventory.main.stack(b).clone();
    set_own(player, SlotRef::Main(a), sb);
    set_own(player, SlotRef::Main(b), sa);
}

/// Replaces the held stack (an item used up, a tool damaged) and tells the client.
pub(super) fn set_held(player: &mut Player, stack: Stack) {
    let held = player.inventory.held;
    set_own(player, SlotRef::Main(held), stack);
    give_pending(player);
    hand_changed(player);
}

/// Replaces an armor piece (0 helmet … 3 boots: damage, breaking) and tells the client.
pub(super) fn set_armor(player: &mut Player, slot: usize, stack: Stack) {
    if let Ok(slot) = u8::try_from(slot)
        && usize::from(slot) < ARMOR
    {
        set_own(player, SlotRef::Armor(slot), stack);
    }
}

/// Replaces the off-hand stack (a totem used up, mending) and tells the client.
pub(super) fn set_offhand(player: &mut Player, stack: Stack) {
    set_own(player, SlotRef::Offhand, stack);
}

/// Replaces main slot `slot` (0 to 35; others are ignored) and tells the client.
pub(super) fn set_main(player: &mut Player, slot: u8, stack: Stack) {
    if usize::from(slot) < MAIN {
        set_own(player, SlotRef::Main(slot), stack);
    }
}

/// Replaces the stack on the cursor and tells the client.
pub(super) fn set_cursor(player: &mut Player, stack: Stack) {
    set_own(player, SlotRef::Cursor, stack);
}

/// Takes one arrow from the player, off hand first, and tells the client; false if it has none.
pub(super) fn take_arrow(player: &mut Player) -> bool {
    let Some(slot) = arrow_slot(player) else {
        return false;
    };
    let Some(stack) = player.inventory.get(slot).map(|s| s.stack) else {
        return false;
    };
    set_own(
        player,
        slot,
        stack.with_count(stack.count().saturating_sub(1)),
    );
    true
}

/// Whether the player carries an arrow, in the off hand or the inventory.
pub(super) fn has_arrow(player: &Player) -> bool {
    arrow_slot(player).is_some()
}

/// The slot a bow takes its arrow from: the off hand if it holds arrows, else the first main slot
/// that does.
fn arrow_slot(player: &Player) -> Option<SlotRef> {
    let inv = &player.inventory;
    if is_arrow(inv.offhand()) {
        return Some(SlotRef::Offhand);
    }
    (0..MAIN)
        .filter_map(|i| u8::try_from(i).ok())
        .find(|&i| is_arrow(inv.main.stack(i)))
        .map(SlotRef::Main)
}

/// Whether a stack is arrows.
fn is_arrow(stack: &Stack) -> bool {
    stack.item_type().and_then(crate::item::Type::kind) == Some(crate::item::Kind::Arrow)
}

/// Drops `count` items of main slot `slot` in front of the player (the legacy
/// `InventoryTransaction` drop of the Q key): false, and nothing changes, if the slot holds fewer
/// or the world has no room for the item entity. A refused drop sends the slot back, as the
/// client took the items away as it sent the drop.
pub(super) fn drop_from_main(state: &mut State, session: SessionId, slot: u8, count: u8) -> bool {
    let Some(player) = state.players.get(&session) else {
        return false;
    };
    let have = player.inventory.main.stack(slot).clone();
    if count == 0 || player.phase == Phase::Loading {
        return false;
    }
    if have.count() < count {
        resend_main(state, session, slot);
        return false;
    }
    let mut event = dispatch::heard::<PlayerDropItem>(state).then(|| PlayerDropItem {
        player: crate::player::Id::new(session),
        item: have.with_count(count),
        cancelled: false,
    });
    if let Some(e) = &mut event {
        dispatch::call(state, e);
        let changed = dispatch::listened::<PlayerDropItem>(state)
            && state
                .players
                .get(&session)
                .is_none_or(|p| *p.inventory.main.stack(slot) != have);
        if e.cancelled || changed {
            resend_main(state, session, slot);
            return false;
        }
    }
    if !throw_stack(state, session, have.with_count(count)) {
        resend_main(state, session, slot);
        return false;
    }
    let Some(player) = state.players.get_mut(&session) else {
        return false;
    };
    set_own(
        player,
        SlotRef::Main(slot),
        have.with_count(have.count() - count),
    );
    give_pending(player);
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
    true
}

/// Gives waiting stacks back where they fit now.
fn give_pending(player: &mut Player) {
    if player.inventory.pending.is_empty() {
        return;
    }
    let pending = std::mem::take(&mut player.inventory.pending);
    for stack in pending {
        let left = give(player, stack);
        if !left.is_empty() {
            player.inventory.pending.push(left);
        }
    }
}

/// Empties everything the player carries (death without `keepInventory`) and returns it: the
/// main slots, armor, off hand, cursor, the open window's own slots and the stacks waiting for
/// room. None of the cursor's items is lent: a death closes the window first, and only a request
/// lends.
pub(super) fn take_all(player: &mut Player) -> Vec<Stack> {
    let inv = &mut player.inventory;
    let mut out = Vec::new();
    inv.main.take_all(&mut out);
    inv.armor.take_all(&mut out);
    inv.offhand.take_all(&mut out);
    inv.cursor.take_all(&mut out);
    if let Some(window) = inv.window.as_mut() {
        for s in &mut window.ui {
            if !s.stack.is_empty() {
                out.push(std::mem::take(s).stack);
            }
        }
    }
    out.append(&mut inv.pending);
    let mut packets = Vec::with_capacity(OWN_WINDOW_PACKETS);
    own_windows(&player.inventory, &mut packets);
    for p in packets {
        player.send_packet(p);
    }
    player.data_dirty = true;
    out
}

/// Opens the window of the block at `pos` (chest, barrel, shulker box, ender chest) for
/// `session`: false when the block has no window.
///
/// True without opening when the container cannot open now: a chest or ender chest under a block
/// that is not transparent, a shulker box whose lid is blocked, a lock the held item's name does
/// not open, or a player who waits with more than `MAX_PENDING` less `MAX_GIVE_BACK` stacks for
/// room. A container without its block entity gets an empty one.
pub(super) fn open_block(
    state: &mut State,
    session: SessionId,
    world: WorldIndex,
    pos: BlockPos,
) -> bool {
    let Some(block) = state.worlds.get(world).and_then(|w| w.block(pos)) else {
        return false;
    };
    let Some(kind) = window_of(block, world, pos) else {
        return false;
    };
    if !can_open(state, session) {
        return true;
    }
    if state.rules.block_entities.get(world, pos).is_none() {
        block_entity::missing(state, world, pos);
    }
    // The interaction that opens the window goes on with the held item: a listener that
    // changes the hand cancels the open, so the rest of the interaction sees the hand it began
    // with.
    let held = held_of(state, session);
    open_checked(state, session, |state| {
        let block = state.worlds.get(world).and_then(|w| w.block(pos))?;
        if window_of(block, world, pos) != Some(kind)
            || !can_open(state, session)
            || held_of(state, session) != held
        {
            return None;
        }
        let held_name = state.players.get(&session)?.inventory.held().custom_name();
        openable(state, kind, held_name)
    });
    true
}

/// The window `kind` of a block after the checks of opening it, with a chest's pair; `None` if
/// it cannot open now.
fn openable(state: &State, kind: WindowKind, held_name: &str) -> Option<WindowKind> {
    let [Some((world, pos)), _] = kind.blocks() else {
        return Some(kind);
    };
    let blocks = &state.rules.block_entities;
    let entity = blocks.get(world, pos);
    if entity
        .and_then(block_entity::BlockEntity::lock)
        .is_some_and(|lock| lock != held_name)
    {
        return None;
    }
    // The block beside `at` reads as the placement rules read it: an inert block as its stand-in.
    let beside = |at: BlockPos, face: block::Face| {
        state
            .worlds
            .get(world)
            .map_or(block::AIR, |w| w.block_for_rules(at.step(face)))
    };
    let solid_beside = |at: BlockPos, face: block::Face| beside(at, face).is_solid();
    let opaque_beside = |at: BlockPos, face: block::Face| !beside(at, face).is_transparent();
    match kind {
        WindowKind::Container {
            kind: Container::Chest,
            ..
        } => {
            let (left, right) = block_entity_halves(blocks, world, pos);
            if opaque_beside(left, block::Face::Up)
                || right.is_some_and(|p| opaque_beside(p, block::Face::Up))
            {
                return None;
            }
            Some(WindowKind::Container {
                world,
                pos: left,
                pair: right,
                kind: Container::Chest,
            })
        }
        WindowKind::Container {
            kind: Container::ShulkerBox,
            ..
        } => {
            let facing = match entity.map(|e| &e.kind) {
                Some(block_entity::Kind::ShulkerBox { facing }) => *facing,
                _ => 1,
            };
            let face = block::Face::all()
                .get(usize::from(facing))
                .copied()
                .unwrap_or(block::Face::Up);
            (!solid_beside(pos, face)).then_some(kind)
        }
        WindowKind::Ender { .. } if opaque_beside(pos, block::Face::Up) => None,
        _ => Some(kind),
    }
}

/// The halves of the chest at `pos`: the left one (the half with the larger `x + (z << 15)`) and
/// the right one, if the chest pairs with a chest that pairs back.
fn block_entity_halves(
    blocks: &block_entity::BlockEntities,
    world: WorldIndex,
    pos: BlockPos,
) -> (BlockPos, Option<BlockPos>) {
    let pair = blocks
        .get(world, pos)
        .and_then(block_entity::BlockEntity::pair)
        .filter(|p| {
            blocks
                .get(world, *p)
                .and_then(block_entity::BlockEntity::pair)
                == Some(pos)
        });
    let key = |p: BlockPos| i64::from(p.x) + (i64::from(p.z) << 15);
    match pair {
        Some(p) if key(p) > key(pos) => (p, Some(pos)),
        pair => (pos, pair),
    }
}

/// The window a block opens, if any.
fn window_of(block: block::State, world: WorldIndex, pos: BlockPos) -> Option<WindowKind> {
    let container = |kind| WindowKind::Container {
        world,
        pos,
        pair: None,
        kind,
    };
    let is = |c| block.is(c);
    Some(if is(Class::EnderChest) {
        WindowKind::Ender { world, pos }
    } else if is(Class::Chest) {
        container(Container::Chest)
    } else if is(Class::Barrel) {
        container(Container::Barrel)
    } else if is(Class::ShulkerBox) {
        container(Container::ShulkerBox)
    } else {
        return None;
    })
}

/// The selected hotbar slot of `session` and its stack (with its stack network ID), to see after
/// listeners that the hand is the one a rule began with.
pub(super) fn held_of(state: &State, session: SessionId) -> Option<(u8, Slot)> {
    let inv = &state.players.get(&session)?.inventory;
    Some((inv.held, inv.main.get(inv.held)?))
}

/// Notes that the client of `session` tries to open a window (a block's or a villager's): false,
/// and the caller opens nothing, if it already tried in this tick. A person opens far fewer than
/// one window a tick, but each open builds the window's whole contents or a villager's every
/// offer, whatever the few bytes of the click. Windows the API opens are not limited.
pub(super) fn first_client_open_this_tick(state: &mut State, session: SessionId) -> bool {
    let tick = state.tick;
    let Some(player) = state.players.get_mut(&session) else {
        return false;
    };
    let first = player.inventory.client_opened != Some(tick);
    player.inventory.client_opened = Some(tick);
    first
}

/// Opens the trade window with the villager NPC `villager` for `session` (window type 15; input
/// slots 4 and 5, output 50; request containers 47 and 48, the sale taken from the created output
/// 60:50; P6) and returns its window ID; `None` if a window cannot open now (another window
/// is open or closing, or [`can_open`] refuses). The official server sends no `ContainerOpen`
/// for it: the caller's `UpdateTrade` opens it on the client. An open trade window is closed
/// first (its payments go back and the trade module hears of it); the new screen replaces it on
/// the client.
pub(super) fn open_trade(
    state: &mut State,
    session: SessionId,
    villager: crate::entity::Id,
) -> Option<u8> {
    if !can_open_trade(state, session) {
        return None;
    }
    // The window open, if any, is a trade window.
    if state
        .players
        .get(&session)
        .is_some_and(|p| p.inventory.window.is_some())
    {
        close(state, session, Notice::Skip);
    }
    // After the close: the payments it gives back may wait for room, and a close listener may
    // have opened a window.
    if !can_open(state, session) {
        return None;
    }
    let inv = &mut state.players.get_mut(&session)?.inventory;
    if inv.window.is_some() || inv.pending_close.is_some() {
        return None;
    }
    let id = inv.next_window_id();
    inv.window = Some(Window::new(WindowKind::Trade { villager }, Some(id)));
    Some(id)
}

/// Whether a trade window may open for `session` but for the room its stacks need: it is in the
/// game and has no window open but a trade window, and none closing. It reads only, so a caller
/// builds what the window shows ([`open_trade`]) after it says yes.
pub(super) fn can_open_trade(state: &State, session: SessionId) -> bool {
    state.players.get(&session).is_some_and(|p| {
        p.phase != Phase::Loading
            && p.inventory.pending_close.is_none()
            && p.inventory
                .window
                .as_ref()
                .is_none_or(|w| matches!(w.kind, WindowKind::Trade { .. }))
    })
}

/// Opens the window `check` finds for `session` now (none: nothing opens), with [`InventoryOpen`]:
/// a cancelled event opens nothing, and after a listener the check runs again and must find the
/// same window, and the player's window must be the one it was (a listener that opened a menu
/// cancels the open).
fn open_checked(
    state: &mut State,
    session: SessionId,
    check: impl Fn(&State) -> Option<WindowKind>,
) {
    let Some(kind) = check(state) else {
        return;
    };
    let Some(window) = public_window(kind).filter(|_| dispatch::heard::<InventoryOpen>(state))
    else {
        let _opened = open(state, session, kind, &check);
        return;
    };
    let mut event = InventoryOpen {
        player: crate::player::Id::new(session),
        window,
        cancelled: false,
    };
    let before = window_key(state, session);
    dispatch::call(state, &mut event);
    let changed = dispatch::listened::<InventoryOpen>(state)
        && (check(state) != Some(kind) || window_key(state, session) != before);
    if event.cancelled || changed {
        return;
    }
    if open(state, session, kind, &check) {
        dispatch::watch(state, &event);
    }
}

/// Whether `session` may open a window now: it is joined and loaded, and it waits with few
/// enough stacks for room that a window's items can go back.
fn can_open(state: &State, session: SessionId) -> bool {
    state
        .players
        .get(&session)
        .is_some_and(|p| p.phase != Phase::Loading && p.inventory.has_room_to_open())
}

/// Opens `kind` for `session`: the current window closes from the server side first, and the
/// new one is announced once that close is acknowledged. False, opening nothing, if the player
/// has left, an [`InventoryClose`] listener opened a window in place of the closed one, or after
/// such a listener `check` no longer finds `kind`.
fn open(
    state: &mut State,
    session: SessionId,
    kind: WindowKind,
    check: &impl Fn(&State) -> Option<WindowKind>,
) -> bool {
    let Some(player) = state.players.get(&session) else {
        return false;
    };
    if player.inventory.window.is_some() {
        close(state, session, Notice::Send);
        if state
            .players
            .get(&session)
            .is_none_or(|p| p.inventory.window.is_some())
            || (dispatch::listened::<InventoryClose>(state) && check(state) != Some(kind))
        {
            return false;
        }
    }
    let mut opened = None;
    for (world, pos) in kind.blocks().into_iter().flatten() {
        let viewers = state.rules.block_entities.add_viewer(world, pos, session);
        if viewers == 1 && kind.is_animated() {
            block_entity::animate(state, world, pos, viewers);
            opened.get_or_insert((world, pos));
        }
    }
    // A double chest sounds once, at its left half.
    if let Some((world, pos)) = opened {
        block_entity::lid_sound(state, world, pos, 1, Raise::Events);
    }
    let Some(player) = state.players.get_mut(&session) else {
        return false;
    };
    player.inventory.window = Some(Window::new(kind, None));
    if player.inventory.pending_close.is_none() {
        announce(state, session);
    }
    true
}

/// Sends a window that waits to be opened: `ContainerOpen` with a new window ID, then its
/// content. No `BlockActorData`: the official server sends none on open, clients have a block
/// entity's data from its chunk and its changes. The own inventory opens at the eye block with
/// entity -1 (P6).
fn announce(state: &mut State, session: SessionId) {
    let blocks = &state.rules.block_entities;
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if !menu::is_ready(player, state.tick) {
        return;
    }
    let eye = player.position;
    let inv = &mut player.inventory;
    let Some(kind) = inv
        .window
        .as_ref()
        .filter(|w| w.id.is_none())
        .map(|w| w.kind)
    else {
        return;
    };
    let id = inv.next_window_id();
    let stacks = match (kind, &inv.menu) {
        (WindowKind::Menu { .. }, Some(m)) => m.stacks.clone(),
        _ => block_stacks(kind, blocks),
    };
    let ids: Vec<StackId> = stacks
        .iter()
        .map(|s| StackId {
            net: id_for(&mut inv.next_id, s),
            request: None,
        })
        .collect();
    let mut packets = Vec::with_capacity(ANNOUNCE_PACKETS);
    let at = match (kind, kind.blocks()) {
        (WindowKind::Menu { pos, .. }, _) | (_, [Some((_, pos)), _]) => [pos.x, pos.y, pos.z],
        _ => {
            let b = BlockPos::containing(eye);
            [b.x, b.y, b.z]
        }
    };
    packets.push(Packet::ContainerOpen(ContainerOpen {
        window_id: id,
        container_type: kind.container_type(),
        container_position: at,
        container_entity_unique_id: -1,
    }));
    if let Some(window) = inv.window.as_mut() {
        window.id = Some(id);
        window.ids = ids;
    }
    if let Some(window) = &inv.window
        && let Some(content) = window_content(inv, window, blocks)
    {
        packets.push(content_packet(u32::from(id), content));
    }
    for p in packets {
        player.send_packet(p);
    }
}

/// Closes whatever window `session` has open, with [`InventoryClose`], for a rule of the game:
/// the cursor and the window's own slots go back to its inventory (the player throws what does
/// not fit). A direct API call uses [`close_window_without_event`].
pub(super) fn close_window(state: &mut State, session: SessionId) {
    close(state, session, Notice::Send);
}

/// Closes the window as [`close_window`] does, for a direct API call, which raises no
/// `InventoryClose`.
pub(super) fn close_window_without_event(state: &mut State, session: SessionId) {
    close_now(state, session, Notice::Send, Raise::Quiet);
}

/// Closes the window of `session` as [`close_now`] does, with [`InventoryClose`] when a window
/// is open (its listeners run before the close, its monitors after). The close applies only to
/// the window that was open before the listeners: one a listener closed is not closed twice, and
/// one a listener opened in its place (a menu) stays open.
fn close(state: &mut State, session: SessionId, notice: Notice) {
    let Some(player) = state.players.get(&session) else {
        return;
    };
    let window = player
        .inventory
        .window
        .as_ref()
        .and_then(|w| public_window(w.kind));
    let before = window_key(state, session);
    match window.filter(|_| dispatch::heard::<InventoryClose>(state)) {
        Some(window) => {
            let mut event = InventoryClose {
                player: crate::player::Id::new(session),
                window,
            };
            dispatch::call(state, &mut event);
            if window_key(state, session) == before {
                close_now(state, session, notice, Raise::Events);
            }
            dispatch::watch(state, &event);
        }
        None => close_now(state, session, notice, Raise::Events),
    }
}

/// What tells the window `session` has open apart from a window opened in its place
/// ([`WindowKey`]); `None` without a player or a window.
fn window_key(state: &State, session: SessionId) -> Option<WindowKey> {
    let inv = &state.players.get(&session)?.inventory;
    let menu = inv.menu.as_ref().map(menu::Open::id);
    inv.window.as_ref().map(|w| (w.id, w.kind, menu))
}

/// Closes the window of `session` (telling the client as `notice` says) and gives the cursor and
/// the window's own slots back, raising no inventory event; if `raise` says so, a chest-like
/// block's close sound raises `WorldSound` and a menu's handler hears of the close
/// ([`menu::closed`]).
fn close_now(state: &mut State, session: SessionId, notice: Notice, raise: Raise) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let window = player.inventory.window.take();
    // Out of the slots before a menu's handler runs, so nothing it calls reaches the stack.
    let [cursor] = std::mem::take(&mut player.inventory.cursor).stacks;
    let menu_lent = std::mem::take(&mut player.inventory.lent);
    let mut back: Vec<Stack> = Vec::with_capacity(MAX_GIVE_BACK);
    if let Some(w) = &window {
        back.extend(
            w.ui.iter()
                .filter(|s| !s.stack.is_empty())
                .map(|s| s.stack.clone()),
        );
        if let Some(id) = w.id
            && notice == Notice::Send
        {
            player.send_packet(Packet::ContainerClose(ContainerClose {
                window_id: id,
                container_type: w.kind.container_type(),
                server_side: true,
            }));
            player.inventory.pending_close = Some(id);
        }
    }
    if !cursor.is_empty() || !back.is_empty() {
        // The client clears these slots itself when a window closes; say so anyway.
        let ui = ui_content(&player.inventory);
        player.send_packet(content_packet(WINDOW_ID_UI, ui));
        player.data_dirty = true;
    }
    if let Some(w) = &window {
        let mut closed = None;
        for (world, pos) in w.kind.blocks().into_iter().flatten() {
            let left = state
                .rules
                .block_entities
                .remove_viewer(world, pos, session);
            if left == 0 && w.kind.is_animated() {
                block_entity::animate(state, world, pos, 0);
                closed.get_or_insert((world, pos));
            }
        }
        if let Some((world, pos)) = closed {
            block_entity::lid_sound(state, world, pos, 0, raise);
        }
    }
    let kind = window.map(|w| w.kind);
    let cursor = match kind {
        Some(WindowKind::Menu { .. }) => {
            // Still held while the handler runs, out of the slots: what it writes there counts
            // them (MAX_SAVED_PENDING).
            let out = usize::from(!cursor.is_empty()) + back.len();
            #[expect(
                clippy::expect_used,
                reason = "invariant: found above, and a menu window has no blocks whose close runs a callback"
            )]
            let p = state
                .players
                .get_mut(&session)
                .expect("a closing menu's player is in the game");
            p.inventory.closing = p.inventory.closing.saturating_add(out);
            let cursor = menu::closed(state, session, cursor, menu_lent, notice, raise);
            #[expect(
                clippy::expect_used,
                reason = "invariant: players leave only at the tick's own places, never in a handler"
            )]
            let p = state
                .players
                .get_mut(&session)
                .expect("a closed menu's player is in the game");
            p.inventory.closing = p.inventory.closing.saturating_sub(out);
            cursor
        }
        // Only a menu lends.
        _ => cursor,
    };
    if !cursor.is_empty() {
        // First, so that the cursor's stack takes the first room, then the window's.
        back.insert(0, cursor);
    }
    give_back(state, session, back);
    if let Some(WindowKind::Trade { .. }) = kind {
        trade::closed(state, session);
    }
}

/// Puts stacks back into the inventory; what does not fit the player throws ([`throw_stack`]),
/// and what cannot be thrown waits in the player's pending list.
fn give_back(state: &mut State, session: SessionId, stacks: Vec<Stack>) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let mut drops = Vec::new();
    for stack in stacks {
        let left = give(player, stack);
        if !left.is_empty() {
            drops.push(left);
        }
    }
    for stack in drops {
        if throw_stack(state, session, stack.clone()) {
            continue;
        }
        let Some(player) = state.players.get_mut(&session) else {
            continue;
        };
        // The invariant of MAX_SAVED_PENDING: each given back stack was held, so the list has room.
        let has_room = player.inventory.pending.len() < MAX_SAVED_PENDING;
        #[expect(
            clippy::expect_used,
            reason = "invariant: each given back stack was held, which MAX_SAVED_PENDING counts"
        )]
        has_room
            .then_some(())
            .expect("the waiting list has room for a given back stack");
        player.inventory.pending.push(stack);
        player.data_dirty = true;
    }
}

/// Puts `stack` into the player's inventory where it fits and tells the client; returns what did
/// not fit.
pub(super) fn give(player: &mut Player, stack: Stack) -> Stack {
    let (left, changed) = player.inventory.add(stack);
    if !changed.is_empty() {
        player.data_dirty = true;
    }
    for i in changed {
        if let Some(slot) = player.inventory.main.get(i) {
            let item = slot.stack.to_instance(slot.id.net);
            player.send_packet(slot_packet(WINDOW_ID_INVENTORY, u32::from(i), item));
        }
    }
    left
}

/// Throws `stack` for the player of `session` (the Q key, a request's drop, an item returned to a
/// full inventory): it leaves [`DROP_HEIGHT`] above the feet along the look at [`THROW_SPEED`],
/// can be picked up after [`THROW_PICKUP_DELAY_TICKS`] and has the player as its thrower, and the
/// viewers see the arm swing. False if the world has no room. Tests stand in for the entity
/// module.
pub(super) fn throw_stack(state: &mut State, session: SessionId, stack: Stack) -> bool {
    let Some(player) = state.players.get(&session) else {
        return false;
    };
    let (world, at) = throw_origin(player);
    let motion = survival::look_direction(player) * THROW_SPEED;
    #[cfg(test)]
    if let Some(dropped) = tests::stand_in_drop(&stack) {
        return dropped;
    }
    let thrower = player.identity.name.clone();
    let delay = THROW_PICKUP_DELAY_TICKS;
    if !super::entity::drop_item_with(state, world, at, stack, Some(motion), delay, thrower) {
        return false;
    }
    survival::swing(state, session);
    true
}

/// Whether [`throw_stack`] would throw a stack for `session` now (the world has room for the
/// item entity).
pub(super) fn can_throw(state: &State, session: SessionId) -> bool {
    let Some(player) = state.players.get(&session) else {
        return false;
    };
    #[cfg(test)]
    if let Some(room) = tests::stand_in_room() {
        return room;
    }
    let (world, at) = throw_origin(player);
    super::entity::can_create(state, world, at)
}

/// Where the player throws from: its world, [`DROP_BELOW_EYE`] below its eyes.
fn throw_origin(player: &Player) -> (WorldIndex, Vec3) {
    let eye = player.position;
    (
        player.world,
        Vec3::new(eye.x, eye.y - DROP_BELOW_EYE, eye.z),
    )
}

/// Drops `stack` as an item entity ([`super::entity::drop_item`]); false if the world has no
/// room. Tests stand in for the entity module.
pub(super) fn drop_stack(state: &mut State, world: WorldIndex, at: Vec3, stack: Stack) -> bool {
    #[cfg(test)]
    if let Some(dropped) = tests::stand_in_drop(&stack) {
        return dropped;
    }
    super::entity::drop_item(state, world, at, stack)
}

/// Sends main slot `slot` as it is, undoing what the client predicted.
pub(super) fn resend_main(state: &mut State, session: SessionId, slot: u8) {
    if let Some(player) = state.players.get_mut(&session)
        && let Some(s) = player.inventory.main.get(slot)
    {
        let item = s.stack.to_instance(s.id.net);
        send_own(player, SlotRef::Main(slot), item);
    }
}

/// Sends everything again ([`resend_all`]) after a client's item click if any of the player's own
/// slots (main, armor, off hand) the client predicted holds another stack than the prediction.
/// The client changed those slots as it sent the click and keeps them until told otherwise, so a
/// refused click would leave it a count that fails the held item check of every later click. A
/// right prediction sends nothing.
pub(super) fn resend_mispredicted(
    state: &mut State,
    session: SessionId,
    predicted: &[InventoryAction],
) {
    let tick = state.tick;
    let blocks = &state.rules.block_entities;
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if player.phase == Phase::Loading {
        return;
    }
    if predicted
        .iter()
        .any(|a| is_mispredicted(&player.inventory, a))
    {
        resend_all(player, tick, blocks);
    }
}

/// Whether `action` predicts one of the player's own slots as other than it is: another item,
/// meta, count or tag. The block runtime ID is not compared: the official server matches a
/// client's stacks without it (stack requests and the held item, P6), so a form differing only
/// there is no misprediction.
fn is_mispredicted(inv: &Inventory, action: &InventoryAction) -> bool {
    if action.source_type != u32::from(InventoryActionSource::Container) {
        return false;
    }
    let Some(window) = action.window_id.and_then(|w| u32::try_from(w).ok()) else {
        return false;
    };
    let Ok(i) = u8::try_from(action.inventory_slot) else {
        return false;
    };
    let r = match window {
        WINDOW_ID_INVENTORY => SlotRef::Main(i),
        WINDOW_ID_ARMOUR => SlotRef::Armor(i),
        WINDOW_ID_OFF_HAND if i == 0 => SlotRef::Offhand,
        _ => return false,
    };
    let Some(slot) = inv.get(r) else {
        return false;
    };
    let (server, client) = (slot.stack.to_network(), &action.new_item.stack);
    server.item_type != client.item_type
        || server.count != client.count
        || server.nbt_data != client.nbt_data
}

/// Sends every slot of the player's own windows and of the open window, at most once a tick:
/// after a refused request or a mispredicted item click ([`resend_mispredicted`]) the client's
/// predictions are undone this way.
fn resend_all(player: &mut Player, tick: u64, blocks: &block_entity::BlockEntities) {
    if player.inventory.resent == Some(tick) {
        return;
    }
    player.inventory.resent = Some(tick);
    let mut packets = Vec::with_capacity(OWN_WINDOW_PACKETS + 1);
    own_windows(&player.inventory, &mut packets);
    if let Some(window) = &player.inventory.window
        && let Some(id) = window.id
        && let Some(content) = window_content(&player.inventory, window, blocks)
    {
        packets.push(content_packet(u32::from(id), content));
    }
    for p in packets {
        player.send_packet(p);
    }
}

/// Appends `InventoryContent` of the four windows a player always has, in the official server's
/// order: inventory, armor, UI and off hand (P6).
fn own_windows(inv: &Inventory, out: &mut Vec<Packet>) {
    out.push(content_packet(
        WINDOW_ID_INVENTORY,
        inv.main.instances().collect(),
    ));
    let mut armor: Vec<ItemInstance> = inv.armor.instances().collect();
    armor.resize(ARMOR_WINDOW, ItemInstance::default());
    out.push(content_packet(WINDOW_ID_ARMOUR, armor));
    out.push(content_packet(WINDOW_ID_UI, ui_content(inv)));
    out.push(content_packet(
        WINDOW_ID_OFF_HAND,
        inv.offhand.instances().collect(),
    ));
}

/// The UI window's slots: the cursor and the open window's own slots.
fn ui_content(inv: &Inventory) -> Vec<ItemInstance> {
    let mut ui = vec![ItemInstance::default(); UI_SLOTS];
    let mut put = |at: usize, item: ItemInstance| {
        if let Some(u) = ui.get_mut(at) {
            *u = item;
        }
    };
    for (item, at) in inv.cursor.instances().zip(0..) {
        put(at, item);
    }
    if let Some(window) = &inv.window {
        for p in window.kind.parts() {
            if let Store::Ui(first) = p.store {
                let slots = window
                    .ui
                    .iter()
                    .skip(usize::from(first))
                    .take(usize::from(p.len));
                for (s, at) in slots.zip(usize::from(p.first)..) {
                    put(at, s.stack.to_instance(s.id.net));
                }
            }
        }
    }
    ui
}

/// The content of a block window as the client shows it, with this player's IDs; `None` for a
/// window without such slots.
fn window_content(
    inv: &Inventory,
    window: &Window,
    blocks: &block_entity::BlockEntities,
) -> Option<Vec<ItemInstance>> {
    match window.kind {
        WindowKind::Own | WindowKind::Trade { .. } => None,
        kind @ WindowKind::Container { .. } => Some(
            block_stacks(kind, blocks)
                .iter()
                .zip(&window.ids)
                .map(|(s, id)| s.to_instance(id.net))
                .collect(),
        ),
        WindowKind::Ender { .. } => Some(inv.ender.instances().collect()),
        WindowKind::Menu { .. } => Some(
            inv.menu
                .as_ref()
                .map_or(&[][..], |m| m.stacks.as_slice())
                .iter()
                .zip(&window.ids)
                .map(|(s, id)| s.to_instance(id.net))
                .collect(),
        ),
    }
}

/// The block container slots a window shows, in window order (left half first).
fn block_stacks(kind: WindowKind, blocks: &block_entity::BlockEntities) -> Vec<Stack> {
    let mut out = Vec::with_capacity(kind.block_slots());
    for (world, pos) in kind.blocks().into_iter().flatten() {
        if let Some(e) = blocks.get(world, pos) {
            out.extend(e.items.iter().cloned());
        }
    }
    out.resize(kind.block_slots(), Stack::EMPTY);
    out
}

/// Gives the player's slot `r` a new stack changed outside a request: a new stack network ID,
/// sent to the client, and the player's data is to be saved. A new stack on the cursor keeps lent
/// as many of the lent items as it holds of their item, the player's own going first. A changed
/// stack in the held slot stops the item in use ([`hand_changed`]).
fn set_own(player: &mut Player, r: SlotRef, stack: Stack) {
    let inv = &mut player.inventory;
    if r == SlotRef::Cursor {
        inv.lent = if inv.cursor().can_stack_with(&stack) {
            inv.lent.min(stack.count())
        } else {
            0
        };
    }
    let hand = r == SlotRef::Main(inv.held) && *inv.main.stack(inv.held) != stack;
    let net = id_for(&mut inv.next_id, &stack);
    let item = stack.to_instance(net);
    let slot = Slot {
        stack,
        id: StackId { net, request: None },
    };
    if player.inventory.put(r, slot) {
        if hand {
            hand_changed(player);
        } else {
            player.data_dirty = true;
        }
        send_own(player, r, item);
    }
}

/// Notes that what the player holds in its hand changed (the selection or the stack): the
/// player's data is to be saved and the item in use stops.
fn hand_changed(player: &mut Player) {
    player.data_dirty = true;
    survival::stop_using_item(player);
}

/// Sends a player slot as it is now.
fn send_own(player: &mut Player, r: SlotRef, item: ItemInstance) {
    match r {
        // The client keeps its own stack network ID for an armor slot told the item it already
        // shows (armor worn by a click it predicted), and a request naming that ID fails; the slot
        // emptied first makes it take the new one.
        SlotRef::Armor(i) => {
            let slot = u32::from(i);
            if item.stack_network_id != 0 {
                let empty = ItemInstance::default();
                player.send_packet(slot_packet(WINDOW_ID_ARMOUR, slot, empty));
            }
            player.send_packet(slot_packet(WINDOW_ID_ARMOUR, slot, item));
        }
        // The client may ignore `InventorySlot` for the off hand.
        SlotRef::Offhand => player.send_packet(content_packet(WINDOW_ID_OFF_HAND, vec![item])),
        SlotRef::Ender(i) => {
            if let Some(window) = &player.inventory.window
                && let Some(id) = window.id
                && matches!(window.kind, WindowKind::Ender { .. })
            {
                player.send_packet(slot_packet(u32::from(id), u32::from(i), item));
            }
        }
        r => {
            if let Some((window, slot)) = client_slot(&player.inventory, r) {
                player.send_packet(slot_packet(window, slot, item));
            }
        }
    }
}

/// The window and slot the client shows a player slot in, outside block windows; the open
/// window's own slots are in the UI window where its parts put them.
fn client_slot(inv: &Inventory, r: SlotRef) -> Option<(u32, u32)> {
    Some(match r {
        SlotRef::Main(i) => (WINDOW_ID_INVENTORY, u32::from(i)),
        SlotRef::Armor(i) => (WINDOW_ID_ARMOUR, u32::from(i)),
        SlotRef::Offhand => (WINDOW_ID_OFF_HAND, 0),
        SlotRef::Cursor => (WINDOW_ID_UI, 0),
        SlotRef::Ender(_) | SlotRef::Block(_) | SlotRef::Menu(_) => return None,
        SlotRef::Ui(i) => (
            WINDOW_ID_UI,
            u32::from(inv.window.as_ref()?.kind.ui_slot(i)?),
        ),
    })
}

/// `InventorySlot` of one slot.
fn slot_packet(window: u32, slot: u32, item: ItemInstance) -> Packet {
    Packet::InventorySlot(Box::new(InventorySlot {
        window_id: window,
        slot,
        container: Some(no_container()),
        storage_item: None,
        new_item: item,
    }))
}

/// `InventoryContent` of a whole window.
fn content_packet(window: u32, content: Vec<ItemInstance>) -> Packet {
    Packet::InventoryContent(Box::new(InventoryContent {
        window_id: window,
        content,
        container: no_container(),
        storage_item: ItemInstance::default(),
    }))
}

/// The container `FullContainerName` of the inventory packets (container 0).
fn no_container() -> FullContainerName {
    FullContainerName {
        container_id: 0,
        dynamic_container_id: None,
    }
}

/// `PlayerHotBar` selecting `slot`.
fn hotbar_packet(slot: u8) -> Packet {
    Packet::PlayerHotBar(PlayerHotBar {
        selected_hot_bar_slot: u32::from(slot),
        window_id: INVENTORY_WINDOW_BYTE,
        select_hot_bar_slot: true,
    })
}

/// A window as events and the API name it; `None` for a menu, which they do not name.
fn public_window(kind: WindowKind) -> Option<PublicWindow> {
    Some(match kind {
        WindowKind::Own => PublicWindow::Own,
        WindowKind::Container { world, pos, .. } | WindowKind::Ender { world, pos } => {
            PublicWindow::Block {
                world: crate::world::Id::of(world),
                pos,
            }
        }
        WindowKind::Trade { villager } => PublicWindow::Trade(villager),
        WindowKind::Menu { .. } => return None,
    })
}

/// The block and its slot behind window slot `i` of a block container window.
fn block_slot(kind: WindowKind, i: u8) -> Option<(WorldIndex, BlockPos, usize)> {
    let [first, second] = kind.blocks();
    let (world, pos) = first?;
    let i = usize::from(i);
    let half = match kind {
        WindowKind::Container {
            kind: Container::Chest,
            pair: Some(_),
            ..
        } => CHEST_SLOTS,
        _ => usize::MAX,
    };
    if i < half {
        Some((world, pos, i))
    } else {
        let (world, pos) = second?;
        Some((world, pos, i - half))
    }
}

/// The window slot of block slot `index` of the block at `pos` in a block window, if it shows
/// that block.
fn window_slot_of(kind: WindowKind, pos: BlockPos, index: usize) -> Option<u8> {
    let [first, second] = kind.blocks();
    let at = if first.is_some_and(|(_, p)| p == pos) {
        index
    } else if second.is_some_and(|(_, p)| p == pos) {
        index.checked_add(CHEST_SLOTS)?
    } else {
        return None;
    };
    u8::try_from(at)
        .ok()
        .filter(|i| usize::from(*i) < kind.block_slots())
}

/// The stack network ID a slot holding `stack` gets: 0 for an empty slot, else a new one
/// ([`take_stack_id`]).
fn id_for(next: &mut i32, stack: &Stack) -> i32 {
    if stack.is_empty() {
        0
    } else {
        take_stack_id(next)
    }
}

/// Takes the stack network ID `next` holds and moves it on: IDs stay positive and wrap after
/// `i32::MAX - 1`.
fn take_stack_id(next: &mut i32) -> i32 {
    let id = *next;
    *next = if id >= i32::MAX - 1 { 1 } else { id + 1 };
    id
}

/// How many stacks the player holds outside its slots, the count the invariant of
/// [`MAX_SAVED_PENDING`] bounds: those of [`held_outside`] that `holds` says hold a stack, and
/// those a close took out while its menu's handler hears it (`Inventory::closing`). A cursor of
/// lent items only ([`Inventory::lent`]) counts too, though a close gives none of them back: the
/// bound is only stricter, and the API's writes compare stacks, not their lent parts.
fn held_count(inv: &Inventory, holds: impl Fn(SlotRef) -> bool) -> usize {
    held_outside(inv).filter(|r| holds(*r)).count() + inv.closing
}

/// The slots a player holds stacks in outside its own: the cursor and the open window's own
/// slots. A close gives them back, so they bound the waiting stacks ([`MAX_SAVED_PENDING`]);
/// [`save`], [`close_now`] and [`take_all`] list the same slots.
fn held_outside(inv: &Inventory) -> impl Iterator<Item = SlotRef> {
    let ui = inv.window.as_ref().map_or(0, |w| w.ui.len());
    std::iter::once(SlotRef::Cursor)
        .chain((0..ui).filter_map(|i| u8::try_from(i).ok().map(SlotRef::Ui)))
}

/// What of `cursor`, a cursor's stack of which `lent` items are lent ([`Inventory::lent`]), is
/// the player's own.
fn own_part(cursor: &Stack, lent: u8) -> Stack {
    cursor.with_count(cursor.count().saturating_sub(lent))
}

#[cfg(test)]
fn hash_id(h: &mut super::Fnv, id: StackId) {
    h.u64(u64::from(id.net.cast_unsigned()));
    h.u64(id.request.map_or(0, |r| u64::from(r.cast_unsigned()) + 1));
}

/// Hashes a stack by its disk form. Every empty stack has the same disk form, so it hashes as a
/// 0 (no disk form's length): most slots are empty, and building their NBT was most of the
/// hashing of a debug determinism run.
#[cfg(test)]
pub(super) fn hash_stack(h: &mut super::Fnv, stack: &Stack) {
    if stack.is_empty() {
        h.u64(0);
    } else {
        super::extra::hash_nbt(h, &stack.to_nbt());
    }
}

#[cfg(test)]
mod tests;
