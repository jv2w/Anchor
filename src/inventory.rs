//! Inventories: the slots of a player and of container blocks, read through [`Inventory`] and
//! changed through [`Editor`].
//!
//! A player has five inventories ([`Kind`]); [`Player::inventory`](crate::player::Player) and
//! `inventory_mut` lend them, and [`Offline`](crate::player::Offline) those of a player who is
//! not online. A container block's slots (a chest, a double chest as one inventory of 54, a
//! barrel, a shulker box) come from [`Context::block_inventory`](crate::Context) and
//! `block_inventory_mut`.
//!
//! Every write tells the clients that show the slot: the player's own client, or every player
//! with the container's window open. A written slot gets a new stack network ID, so a request a
//! client built on the old contents is refused instead of applied to the new ones.
//! Direct writes raise no events.
//!
//! Slot numbers are `u8` where the protocol numbers them (the hotbar, the held slot, events) and
//! `usize` where they index an [`Inventory`], as std indexes slices.

use std::fmt;

use crate::item::Stack;

/// The slots of a chest (each half of a double chest), a barrel, a shulker box and an ender
/// chest.
pub(crate) const CHEST_SLOTS: usize = 27;
/// The slots of a hopper.
pub(crate) const HOPPER_SLOTS: usize = 5;
/// The hotbar slots: the main slots from 0 to `HOTBAR_SIZE - 1`.
#[doc(alias = "PlayerInventory::getHotbarSize")]
pub const HOTBAR_SIZE: u8 = 9;
/// The most items of one slot of most inventories.
pub(crate) const MAX_STACK: u8 = 64;

/// One of a player's inventories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// The 36 main slots: the hotbar from 0 to 8, then the 27 above it.
    #[doc(alias = "PlayerInventory")]
    Main,
    /// The four armour slots: helmet, chestplate, leggings, boots.
    #[doc(alias = "ArmorInventory")]
    Armor,
    /// The off-hand slot.
    #[doc(alias = "PlayerOffHandInventory")]
    Offhand,
    /// The item on the cursor while a window is open.
    #[doc(alias = "PlayerCursorInventory")]
    Cursor,
    /// The player's ender chest contents.
    #[doc(alias = "PlayerEnderInventory")]
    EnderChest,
}

impl Kind {
    /// How many slots the inventory has.
    pub const fn size(self) -> usize {
        match self {
            Self::Main => 36,
            Self::Armor => 4,
            Self::Offhand | Self::Cursor => 1,
            Self::EnderChest => CHEST_SLOTS,
        }
    }
}

/// Why a write to an inventory did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The inventory has no such slot.
    NoSlot,
    /// The stack holds more than the slot takes.
    TooMany,
    /// The player holds as many stacks outside its slots (cursor, window, a closing menu's while
    /// its handler hears the close) as can be given back when its window closes, so the cursor
    /// takes no new stack.
    Full,
    /// The contents are longer than the inventory.
    TooLong,
    /// The slot does not take the stack: a shulker box, or a stack over 3 KiB saved, in a
    /// shulker box (so that the box with its items is one stack within
    /// [`MAX_NBT_BYTES`](crate::item::MAX_NBT_BYTES)).
    NotAccepted,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSlot => f.write_str("no such slot"),
            Self::TooMany => f.write_str("more items than the slot takes"),
            Self::Full => f.write_str("too many stacks held outside the slots"),
            Self::TooLong => f.write_str("more stacks than slots"),
            Self::NotAccepted => f.write_str("the slot does not take the stack"),
        }
    }
}

impl std::error::Error for Error {}

/// A result with the module's [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// An inventory to read: its slots in order (a double chest's left half first).
#[doc(alias = "BaseInventory")]
#[doc(alias = "SimpleInventory")]
#[derive(Clone, Copy)]
// api-class: Inventory
pub struct Inventory<'a> {
    halves: [&'a [Stack]; 2],
    max: u8,
    /// A shulker box's slots: they take only what [`Stack::can_enter_shulker_box`].
    shulker_box: bool,
}

impl<'a> Inventory<'a> {
    /// The slots `first` then `second` (a double chest's two halves), each holding at most `max`
    /// items. Over slots of its own, such as a copy of a player's, a program plans changes of
    /// several steps with [`plan_add`](Self::plan_add) before it writes them.
    pub fn new(first: &'a [Stack], second: &'a [Stack], max: u8) -> Self {
        Self {
            halves: [first, second],
            max,
            shulker_box: false,
        }
    }

    /// The same slots as a shulker box's, which take only what [`Stack::can_enter_shulker_box`].
    pub(crate) fn of_shulker_box(self) -> Self {
        Self {
            shulker_box: true,
            ..self
        }
    }

    /// Whether all of `stack` would fit.
    #[doc(alias = "Inventory::canAddItem")]
    #[doc(alias = "BaseInventory::canAddItem")]
    #[doc(alias = "ShulkerBoxInventory::canAddItem")]
    pub fn can_add(&self, stack: &Stack) -> bool {
        self.addable_quantity(stack) == stack.count()
    }

    /// How many of `stack` would fit: onto stacks it stacks with and into empty slots, at most
    /// its count; none of a shulker box into a shulker box.
    #[doc(alias = "Inventory::getAddableItemQuantity")]
    #[doc(alias = "BaseInventory::getAddableItemQuantity")]
    pub fn addable_quantity(&self, stack: &Stack) -> u8 {
        if self.refuses(stack) {
            return 0;
        }
        let max = u32::from(self.slot_max(stack));
        let mut room = 0_u32;
        let want = u32::from(stack.count());
        for s in self.iter() {
            if room >= want {
                break;
            }
            if s.is_empty() {
                room += max;
            } else if s.can_stack_with(stack) {
                room += max.saturating_sub(u32::from(s.count()));
            }
        }
        stack.count().min(u8::try_from(room).unwrap_or(u8::MAX))
    }

    /// Every slot holding `stack`'s item, as [`first`](Self::first) matches, with its stack.
    #[doc(alias = "Inventory::all")]
    #[doc(alias = "BaseInventory::all")]
    pub fn all(&self, stack: &'a Stack) -> impl Iterator<Item = (usize, &'a Stack)> + 'a {
        self.iter()
            .enumerate()
            .filter(move |(_, s)| matches(s, stack))
    }

    /// Whether the inventory holds at least `stack.count()` of `stack`'s item (its damage, and
    /// its extra data when it has some), over all slots.
    #[doc(alias = "Inventory::contains")]
    #[doc(alias = "BaseInventory::contains")]
    pub fn contains(&self, stack: &Stack) -> bool {
        let have: u32 = self
            .iter()
            .filter(|s| matches(s, stack))
            .map(|s| u32::from(s.count()))
            .sum();
        have >= u32::from(stack.count())
    }

    /// The first slot holding `stack`'s item (any count; the extra data counts only when
    /// `stack` has some). The first slot holding exactly `stack` is
    /// `iter().position(|s| s == stack)`.
    #[doc(alias = "Inventory::first")]
    #[doc(alias = "BaseInventory::first")]
    pub fn first(&self, stack: &Stack) -> Option<usize> {
        self.iter().position(|s| matches(s, stack))
    }

    /// The first empty slot.
    #[doc(alias = "Inventory::firstEmpty")]
    #[doc(alias = "BaseInventory::firstEmpty")]
    pub fn first_empty(&self) -> Option<usize> {
        self.iter().position(Stack::is_empty)
    }

    /// Whether `slot` holds nothing (a slot the inventory does not have holds nothing).
    #[doc(alias = "Inventory::isSlotEmpty")]
    #[doc(alias = "BaseInventory::isSlotEmpty")]
    #[doc(alias = "SimpleInventory::isSlotEmpty")]
    #[doc(alias = "DoubleChestInventory::isSlotEmpty")]
    pub fn is_slot_empty(&self, slot: usize) -> bool {
        self.item(slot).is_none_or(Stack::is_empty)
    }

    /// The stack in `slot`; `None` if the inventory has no such slot.
    #[doc(alias = "Inventory::getItem")]
    #[doc(alias = "SimpleInventory::getItem")]
    #[doc(alias = "DoubleChestInventory::getItem")]
    #[doc(alias = "Inventory::slotExists")]
    #[doc(alias = "BaseInventory::slotExists")]
    #[doc(alias = "ArmorInventory::getHelmet")]
    #[doc(alias = "ArmorInventory::getChestplate")]
    #[doc(alias = "ArmorInventory::getLeggings")]
    #[doc(alias = "ArmorInventory::getBoots")]
    #[doc(alias = "PlayerInventory::getHotbarSlotItem")]
    pub fn item(&self, slot: usize) -> Option<&'a Stack> {
        let [first, second] = self.halves;
        match slot.checked_sub(first.len()) {
            Some(i) => second.get(i),
            None => first.get(slot),
        }
    }

    /// Every slot in order, empty ones included.
    #[doc(alias = "Inventory::getContents")]
    #[doc(alias = "SimpleInventory::getContents")]
    #[doc(alias = "DoubleChestInventory::getContents")]
    pub fn iter(&self) -> impl Iterator<Item = &'a Stack> + 'a {
        let [first, second] = self.halves;
        first.iter().chain(second)
    }

    /// The most items one slot holds (a stack holds no more than its item's own maximum).
    #[doc(alias = "Inventory::getMaxStackSize")]
    #[doc(alias = "BaseInventory::getMaxStackSize")]
    pub fn max_stack_size(&self) -> u8 {
        self.max
    }

    /// Where `stack` goes: onto the stacks it stacks with that have room, in slot order, then
    /// into empty slots. The slots to write, each with the stack it then holds, and what does not
    /// fit: what [`Editor::add`] writes and returns.
    pub fn plan_add(&self, stack: &Stack) -> (Vec<(usize, Stack)>, Stack) {
        if self.refuses(stack) {
            return (Vec::new(), stack.clone());
        }
        let max = self.slot_max(stack);
        let mut left = stack.count();
        let mut writes = Vec::new();
        for pass in [false, true] {
            for (i, s) in self.iter().enumerate() {
                if left == 0 {
                    break;
                }
                let moved = if pass {
                    if !s.is_empty() {
                        continue;
                    }
                    max.min(left)
                } else {
                    if s.count() >= max || !s.can_stack_with(stack) {
                        continue;
                    }
                    (max - s.count()).min(left)
                };
                writes.push((i, stack.with_count(s.count() + moved)));
                left -= moved;
            }
        }
        (writes, stack.with_count(left))
    }

    /// What taking `stack.count()` of `stack`'s item writes: from the matching slots in order.
    /// The slots to write and what could not be taken.
    pub(crate) fn plan_remove(&self, stack: &Stack) -> (Vec<(usize, Stack)>, Stack) {
        let mut left = stack.count();
        let mut writes = Vec::new();
        for (i, s) in self.iter().enumerate() {
            if left == 0 {
                break;
            }
            if matches(s, stack) {
                let taken = s.count().min(left);
                writes.push((i, s.with_count(s.count() - taken)));
                left -= taken;
            }
        }
        (writes, stack.with_count(left))
    }

    /// Whether the slots refuse `stack` whatever room they have: a shulker box or a large stack
    /// in a shulker box.
    pub(crate) fn refuses(&self, stack: &Stack) -> bool {
        self.shulker_box && !stack.can_enter_shulker_box()
    }

    /// How many slots the inventory has.
    #[doc(alias = "Inventory::getSize")]
    #[doc(alias = "SimpleInventory::getSize")]
    #[doc(alias = "DoubleChestInventory::getSize")]
    pub fn size(&self) -> usize {
        self.halves[0].len() + self.halves[1].len()
    }

    /// The most items of `stack` one slot of this inventory holds.
    fn slot_max(&self, stack: &Stack) -> u8 {
        stack.max_count().min(self.max)
    }
}

impl fmt::Debug for Inventory<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// An inventory to change: a player's, an offline player's or a container block's, borrowed from
/// the tick. Each write tells the clients that show the slot.
pub struct Editor<'a> {
    pub(crate) target: Target<'a>,
}

impl fmt::Debug for Editor<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("Editor");
        match &self.target {
            Target::Player(_, kind) => s.field("player", kind),
            Target::Offline(_, kind) => s.field("offline", kind),
            Target::Block(b) => s
                .field("world", &b.world)
                .field("pos", &b.pos)
                .field("pair", &b.pair),
        };
        s.finish_non_exhaustive()
    }
}

/// What an [`Editor`] writes to.
pub(crate) enum Target<'a> {
    /// One of a player's inventories.
    Player(&'a mut crate::tick::Player, Kind),
    /// One of an offline player's inventories, which no client shows.
    Offline(&'a mut crate::tick::Offline, Kind),
    /// A container block (and a double chest's other half).
    Block(BlockSlots<'a>),
}

/// The slots of a container block: the block at `pos` (a double chest's left half) and the
/// other half `pair`, in world `world` of the tick state.
pub(crate) struct BlockSlots<'a> {
    pub(crate) state: &'a mut crate::tick::State,
    pub(crate) world: crate::save::WorldIndex,
    pub(crate) pos: crate::world::BlockPos,
    pub(crate) pair: Option<crate::world::BlockPos>,
}

/// Whether `have` is `want`'s item for finding and taking: the same type and damage, and the
/// same extra data when `want` has some.
pub(crate) fn matches(have: &Stack, want: &Stack) -> bool {
    have.item().is_some()
        && have.item() == want.item()
        && have.damage() == want.damage()
        && (want.extra().is_none() || have.extra() == want.extra())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::item::{Extra, Kind as ItemKind};

    fn stack(kind: ItemKind, count: u8) -> Stack {
        Stack::new(kind.default_type(), count)
    }

    // api: BaseInventory, SimpleInventory, Inventory::getSize, SimpleInventory::getSize,
    // api: DoubleChestInventory::getSize, Inventory::getItem, SimpleInventory::getItem,
    // api: DoubleChestInventory::getItem, Inventory::slotExists, BaseInventory::slotExists,
    // api: Inventory::isSlotEmpty, BaseInventory::isSlotEmpty, SimpleInventory::isSlotEmpty,
    // api: DoubleChestInventory::isSlotEmpty, Inventory::getContents,
    // api: SimpleInventory::getContents, DoubleChestInventory::getContents,
    // api: Inventory::getMaxStackSize, BaseInventory::getMaxStackSize, Inventory
    #[test]
    fn a_view_reads_both_halves_in_order() {
        let left = [stack(ItemKind::Stick, 3), Stack::EMPTY];
        let right = [stack(ItemKind::Coal, 1)];
        let inv = Inventory::new(&left, &right, 1);
        assert_eq!(inv.size(), 3);
        assert_eq!(inv.item(2), Some(&stack(ItemKind::Coal, 1)));
        assert_eq!(inv.item(3), None);
        assert!(inv.is_slot_empty(1) && inv.is_slot_empty(9) && !inv.is_slot_empty(0));
        assert_eq!(inv.iter().count(), 3);
        assert_eq!(inv.max_stack_size(), 1);
        assert_eq!(format!("{inv:?}").matches("Stack").count(), 3);
    }

    // api: Inventory::contains, BaseInventory::contains, Inventory::first,
    // api: BaseInventory::first, Inventory::firstEmpty, BaseInventory::firstEmpty,
    // api: Inventory::all, BaseInventory::all, Inventory::getAddableItemQuantity,
    // api: BaseInventory::getAddableItemQuantity, Inventory::canAddItem,
    // api: BaseInventory::canAddItem
    #[test]
    fn finding_and_room_count_stacks_with_equal_data() {
        let named = stack(ItemKind::Stick, 1)
            .with_extra(Some(Extra {
                custom_name: Some("a".to_owned()),
                ..Extra::default()
            }))
            .unwrap();
        let slots = [
            stack(ItemKind::Coal, 60),
            Stack::EMPTY,
            named.with_count(5),
            stack(ItemKind::Stick, 10),
        ];
        let inv = Inventory::new(&slots, &[], MAX_STACK);
        assert!(
            inv.contains(&stack(ItemKind::Stick, 15)),
            "any extra data counts"
        );
        assert!(!inv.contains(&named.with_count(6)));
        assert_eq!(inv.first(&stack(ItemKind::Stick, 1)), Some(2));
        assert_eq!(inv.first(&named), Some(2));
        assert_eq!(inv.first_empty(), Some(1));
        let want = stack(ItemKind::Stick, 1);
        assert_eq!(inv.all(&want).map(|(i, _)| i).collect::<Vec<_>>(), [2, 3]);
        // 4 onto the coal, 64 into the empty slot.
        assert_eq!(inv.addable_quantity(&stack(ItemKind::Coal, 64)), 64);
        assert!(inv.can_add(&stack(ItemKind::Coal, 64)));
        let full = [stack(ItemKind::Coal, 62)];
        let inv = Inventory::new(&full, &[], MAX_STACK);
        assert_eq!(inv.addable_quantity(&stack(ItemKind::Coal, 5)), 2);
        assert!(!inv.can_add(&stack(ItemKind::Coal, 5)));
    }

    #[test]
    fn adding_fills_partial_stacks_first_and_removing_takes_in_order() {
        let slots = [
            Stack::EMPTY,
            stack(ItemKind::Coal, 60),
            stack(ItemKind::Coal, 60),
        ];
        let inv = Inventory::new(&slots, &[], MAX_STACK);
        let (writes, left) = inv.plan_add(&stack(ItemKind::Coal, 64));
        assert_eq!(
            writes,
            [
                (1, stack(ItemKind::Coal, 64)),
                (2, stack(ItemKind::Coal, 64)),
                (0, stack(ItemKind::Coal, 56)),
            ]
        );
        assert!(left.is_empty());
        let (writes, left) = inv.plan_remove(&stack(ItemKind::Coal, 62));
        assert_eq!(writes, [(1, Stack::EMPTY), (2, stack(ItemKind::Coal, 58))]);
        assert!(left.is_empty());
        let few = [stack(ItemKind::Coal, 3)];
        let (_, left) = Inventory::new(&few, &[], MAX_STACK).plan_remove(&stack(ItemKind::Coal, 5));
        assert_eq!(left, stack(ItemKind::Coal, 2));
        assert_eq!(Error::TooLong.to_string(), "more stacks than slots");
    }

    // api: ShulkerBoxInventory::canAddItem
    #[test]
    fn a_shulker_box_takes_no_shulker_box() {
        let crate::block::ItemForm::Item(shulker) =
            crate::block::Kind::ShulkerBox.default_state().item()
        else {
            panic!("a shulker box has an item");
        };
        let shulker = Stack::new(shulker, 1);
        let slots = [Stack::EMPTY, Stack::EMPTY];
        let chest = Inventory::new(&slots, &[], MAX_STACK);
        assert!(chest.can_add(&shulker));
        let box_slots = chest.of_shulker_box();
        assert!(!box_slots.can_add(&shulker));
        assert_eq!(box_slots.addable_quantity(&shulker), 0);
        assert_eq!(box_slots.plan_add(&shulker), (Vec::new(), shulker.clone()));
        assert!(box_slots.can_add(&stack(ItemKind::Coal, 64)));
    }

    // api: PlayerInventory::getHotbarSize
    #[test]
    fn the_hotbar_is_the_first_nine_main_slots() {
        assert_eq!(HOTBAR_SIZE, 9);
        assert!(usize::from(HOTBAR_SIZE) < Kind::Main.size());
    }
}
