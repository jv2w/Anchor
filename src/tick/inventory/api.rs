//! The public inventory API: a player's inventories, its hotbar selection and window on [`Player`],
//! an offline player's inventories on [`Offline`], container blocks on [`Context`], and the writes
//! of [`Editor`].
//!
//! A write outside a request gives the slot a new stack network ID and sends it to the client
//! that shows it ([`set_own`] for a player's slots; [`request::notify`] for every viewer of
//! a container), so the next request of a client that still shows the old stack is refused. An
//! offline player's slots have no client: a write only changes the record. Writes raise no
//! events.

use super::{
    HOTBAR, Inventory as Slots, MAX_SAVED_PENDING, Slot, SlotRef, StackId, block_entity_halves,
    close_window_without_event, content_packet, held_count, public_window, request, select,
    set_own,
};
use crate::event::Window;
use crate::inventory::{BlockSlots, Editor, Error, Inventory, Kind, MAX_STACK, Result, Target};
use crate::item::Stack;
use crate::protocol::consts::WINDOW_ID_OFF_HAND;
use crate::save::WorldIndex;
use crate::tick::{Context, Offline, Player, State, block_entity};
use crate::world::{self, BlockPos};

/// What planned writes do ([`Editor::write`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Planned {
    /// Each write puts more of the item into its slot.
    Add,
    /// Each write takes some of the item out of its slot.
    Remove,
}

impl Player {
    /// The stack in the selected hotbar slot.
    #[doc(alias = "PlayerInventory::getItemInHand")]
    pub fn held_item(&self) -> &Stack {
        self.inventory.held()
    }

    /// The selected hotbar slot, 0 to 8.
    #[doc(alias = "PlayerInventory::getHeldItemIndex")]
    pub fn held_slot(&self) -> u8 {
        self.inventory.held
    }

    /// One of the player's inventories, to read.
    #[doc(alias = "Human::getInventory")]
    #[doc(alias = "Living::getArmorInventory")]
    #[doc(alias = "Human::getOffHandInventory")]
    #[doc(alias = "Player::getCursorInventory")]
    #[doc(alias = "Human::getEnderInventory")]
    pub fn inventory(&self, kind: Kind) -> Inventory<'_> {
        view_of(&self.inventory, kind)
    }

    /// The window the player has open, if any (its own inventory, a block's, a trade); a menu is
    /// [`Context::menu`]'s.
    #[doc(alias = "Player::getCurrentWindow")]
    pub fn window(&self) -> Option<Window> {
        self.inventory
            .window
            .as_ref()
            .and_then(|w| public_window(w.kind))
    }

    /// The stack in slot `slot` of the open window's own slots, which it gives back when it
    /// closes: a trade's two payments (0 and 1). `None` without an open window or such a slot.
    pub fn window_item(&self, slot: usize) -> Option<&Stack> {
        self.inventory
            .window
            .as_ref()
            .and_then(|w| w.ui.get(slot))
            .map(|s| &s.stack)
    }

    /// One of the player's inventories, to change; each write is sent to its client.
    pub fn inventory_mut(&mut self, kind: Kind) -> Editor<'_> {
        Editor {
            target: Target::Player(self, kind),
        }
    }

    /// Selects hotbar slot `slot` (0 to 8), stopping the item in use (a drawn bow, eating or
    /// drinking), and tells the client.
    ///
    /// # Errors
    ///
    /// [`Error::NoSlot`] for another slot, changing nothing.
    #[doc(alias = "PlayerInventory::setHeldItemIndex")]
    pub fn set_held_slot(&mut self, slot: u8) -> Result<()> {
        if slot >= HOTBAR {
            return Err(Error::NoSlot);
        }
        if slot != self.inventory.held {
            select(self, slot);
        }
        Ok(())
    }

    /// Shows this player alone the screen animation of a totem of undying saving it (the
    /// `TalismanActivate` actor event); the other players see nothing, and no stack changes. The
    /// client draws the animation only with a totem of undying in hand (`facts.md`), so its off
    /// hand shows one while it takes the event, then what it holds again.
    pub fn show_totem_animation(&mut self) {
        let totem = Stack::new(crate::item::Kind::Totem.default_type(), 1);
        let held: Vec<_> = self.inventory.offhand.instances().collect();
        self.send_packet(content_packet(
            WINDOW_ID_OFF_HAND,
            vec![totem.to_instance(0)],
        ));
        self.send_packet(crate::entity::Animation::TotemUse.packet(self.runtime_id));
        self.send_packet(content_packet(WINDOW_ID_OFF_HAND, held));
    }
}

impl Offline {
    /// One of the player's inventories as saved, to read. What it held on the cursor or in a
    /// window is saved as stacks waiting for room, which a join puts back, so the cursor reads
    /// empty.
    pub fn inventory(&self, kind: Kind) -> Inventory<'_> {
        view_of(&self.inventory, kind)
    }

    /// One of the player's inventories, to change.
    pub fn inventory_mut(&mut self, kind: Kind) -> Editor<'_> {
        Editor {
            target: Target::Offline(self, kind),
        }
    }
}

impl Context<'_> {
    /// The slots of the container block at `pos` in `world` (a chest, both halves of a double
    /// chest from either half, a barrel, a shulker box); `None` if no loaded container with a
    /// block entity is there.
    #[doc(alias = "Container::getRealInventory")]
    #[doc(alias = "InventoryHolder::getInventory")]
    pub fn block_inventory(&self, world: world::Id, pos: BlockPos) -> Option<Inventory<'_>> {
        let (pos, pair) = container(self.state, world.index(), pos)?;
        Some(block_view(self.state, world.index(), pos, pair))
    }

    /// The players with a window open on the block at `pos` in `world` (a container, both halves
    /// of a double chest from either half), in the order they opened it; how many there are is
    /// the iterator's length.
    #[doc(alias = "Inventory::getViewers")]
    #[doc(alias = "BaseInventory::getViewers")]
    pub fn block_inventory_viewers(
        &self,
        world: world::Id,
        pos: BlockPos,
    ) -> impl ExactSizeIterator<Item = crate::player::Id> + '_ {
        self.state
            .rules
            .block_entities
            .viewers(world.index(), pos)
            .iter()
            .map(|s| crate::player::Id::new(*s))
    }

    /// The name of the alternative creative inventory `player` sees; `None` for the default one
    /// or a player who has left.
    #[doc(alias = "Player::getCreativeInventory")]
    pub fn creative_inventory(&self, player: crate::player::Id) -> Option<&'static str> {
        let p = self.state.players.get(&player.session())?;
        self.state.custom.creative_name(p.inventory.creative)
    }

    /// The items of a creative inventory, one of each entry in order (a player's request names
    /// the entry at index `i` as creative item `i + 1`): the alternative registered as
    /// `inventory`, or the default one for `None`; `None` for a name not registered.
    #[doc(alias = "CreativeInventory::getAll")]
    pub fn creative_items(&self, inventory: Option<&str>) -> Option<&[Stack]> {
        let custom = &self.state.custom;
        let index = custom.creative_index(inventory)?;
        Some(custom.creatives().get(usize::from(index))?.stacks())
    }

    /// The slots of the container block at `pos` in `world`, to change; each write is sent to
    /// every player with its window open.
    pub fn block_inventory_mut(&mut self, world: world::Id, pos: BlockPos) -> Option<Editor<'_>> {
        let (pos, pair) = container(self.state, world.index(), pos)?;
        Some(Editor {
            target: Target::Block(BlockSlots {
                state: self.state,
                world: world.index(),
                pos,
                pair,
            }),
        })
    }

    /// Closes the player's window, a menu too: the cursor (but a lending menu's lent items, which
    /// stay its handler's) and the window's own slots go back to its inventory (the player
    /// throws what does not fit), and a menu's blocks show again. False if the player
    /// has left or has no window open. Raises no event and calls no menu handler
    /// ([`menu::Close`](crate::menu::Close) says how to keep the cursor's own items); the caller
    /// is how its own close is known.
    #[doc(alias = "Player::removeCurrentWindow")]
    pub fn close_window(&mut self, player: crate::player::Id) -> bool {
        let session = player.session();
        let open = self
            .state
            .players
            .get(&session)
            .is_some_and(|p| p.inventory.window.is_some());
        if open {
            close_window_without_event(self.state, session);
        }
        open
    }

    /// Switches the creative inventory `player` sees: the alternative registered as `inventory`
    /// ([`ServerBuilder::alternative_creative_inventory`](crate::ServerBuilder::alternative_creative_inventory)),
    /// or the default one for `None`. A spawned player's client gets the new list.
    ///
    /// # Errors
    ///
    /// Changes nothing with:
    /// [`Error::NoPlayer`] for a player who has left.
    /// [`Error::NoCreativeInventory`] for a name no alternative has.
    #[doc(alias = "Player::setCreativeInventory")]
    pub fn set_creative_inventory(
        &mut self,
        player: crate::player::Id,
        inventory: Option<&str>,
    ) -> Result<()> {
        let Some(index) = self.state.custom.creative_index(inventory) else {
            return Err(Error::NoCreativeInventory);
        };
        let Some(p) = self.state.players.get_mut(&player.session()) else {
            return Err(Error::NoPlayer);
        };
        if p.inventory.creative != index {
            p.inventory.creative = index;
            // Before its chunk radius request the client gets the inventory in its spawn window.
            p.pending.creative |= p.chunk_radius.is_some();
        }
        Ok(())
    }
}

impl Editor<'_> {
    /// The inventory as it is now, to read.
    pub fn view(&self) -> Inventory<'_> {
        match &self.target {
            Target::Player(p, kind) => p.inventory(*kind),
            Target::Offline(o, kind) => o.inventory(*kind),
            Target::Block(b) => block_view(b.state, b.world, b.pos, b.pair),
        }
    }

    /// Puts `stack` where it fits: onto the stacks it stacks with, in slot order, then into empty
    /// slots. Returns what did not fit.
    #[doc(alias = "Inventory::addItem")]
    #[doc(alias = "BaseInventory::addItem")]
    pub fn add(&mut self, stack: &Stack) -> Stack {
        let (writes, left) = self.view().plan_add(stack);
        self.write(writes, left, Planned::Add)
    }

    /// Empties every slot.
    #[doc(alias = "Inventory::clearAll")]
    #[doc(alias = "BaseInventory::clearAll")]
    pub fn clear_all(&mut self) {
        for i in 0..self.view().size() {
            self.clear(i);
        }
    }

    /// Takes `stack.count()` of `stack`'s item (its damage, and its extra data when it has some)
    /// from the slots that hold it, in slot order. Returns what could not be taken.
    #[doc(alias = "Inventory::removeItem")]
    #[doc(alias = "BaseInventory::removeItem")]
    pub fn remove(&mut self, stack: &Stack) -> Stack {
        let (writes, left) = self.view().plan_remove(stack);
        self.write(writes, left, Planned::Remove)
    }

    /// Empties every slot holding `stack`'s item, as [`remove`](Self::remove) matches it.
    #[doc(alias = "Inventory::remove")]
    #[doc(alias = "BaseInventory::remove")]
    pub fn remove_all(&mut self, stack: &Stack) {
        let slots: Vec<usize> = self.view().all(stack).map(|(i, _)| i).collect();
        for i in slots {
            self.clear(i);
        }
    }

    /// Puts `stacks` into the first slots in order and empties the rest.
    ///
    /// # Errors
    ///
    /// Changes nothing with:
    /// [`Error::Full`] for more stacks than the cursor may hold.
    /// [`Error::TooLong`] for more stacks than slots.
    /// [`Error::NotAccepted`] for a stack a slot does not take.
    #[doc(alias = "Inventory::setContents")]
    #[doc(alias = "BaseInventory::setContents")]
    pub fn set_contents(&mut self, stacks: &[Stack]) -> Result<()> {
        let view = self.view();
        if stacks.len() > view.size() {
            return Err(Error::TooLong);
        }
        if stacks.iter().any(|s| view.refuses(s)) {
            return Err(Error::NotAccepted);
        }
        let held = match &self.target {
            Target::Player(p, Kind::Cursor) => Some(&p.inventory),
            Target::Offline(o, Kind::Cursor) => Some(&o.inventory),
            Target::Player(..) | Target::Offline(..) | Target::Block(_) => None,
        };
        if let Some(inv) = held {
            let now = view.iter().filter(|s| !s.is_empty()).count();
            let after = stacks.iter().filter(|s| !s.is_empty()).count();
            if inv.pending.len() + held_now(inv) - now + after > MAX_SAVED_PENDING {
                return Err(Error::Full);
            }
        }
        // The slots that empty are written first, so the cursor never holds more stacks than at
        // the end, which the check above allows.
        let size = view.size();
        for empties in [true, false] {
            for i in 0..size {
                let stack = stacks.get(i).cloned().unwrap_or_default();
                if stack.is_empty() == empties {
                    self.set(i, stack)?;
                }
            }
        }
        Ok(())
    }

    /// Exchanges the stacks of slots `a` and `b`.
    ///
    /// # Errors
    ///
    /// [`Error::NoSlot`] if the inventory lacks slot `a` or `b`.
    #[doc(alias = "Inventory::swap")]
    #[doc(alias = "BaseInventory::swap")]
    pub fn swap(&mut self, a: usize, b: usize) -> Result<()> {
        let view = self.view();
        let (sa, sb) = match (view.item(a), view.item(b)) {
            (Some(sa), Some(sb)) => (sa.clone(), sb.clone()),
            _ => return Err(Error::NoSlot),
        };
        // Both slots hold a stack of the inventory already, so both writes fit. The slot that
        // empties is written first, so the cursor never holds one stack more than before.
        if sa.is_empty() {
            self.set(b, sa)?;
            self.set(a, sb)
        } else {
            self.set(a, sb)?;
            self.set(b, sa)
        }
    }

    /// Puts `stack` into `slot` (an empty stack clears it) and tells the clients that show it. A
    /// changed stack in a player's held slot stops the item in use.
    ///
    /// # Errors
    ///
    /// [`Error::NoSlot`] for a slot the inventory does not have.
    /// [`Error::Full`] for a new stack on the cursor when it holds as many stacks as can be given
    /// back.
    /// [`Error::NotAccepted`] for a stack a shulker box does not take.
    #[doc(alias = "Inventory::setItem")]
    #[doc(alias = "BaseInventory::setItem")]
    #[doc(alias = "Inventory::clear")]
    #[doc(alias = "BaseInventory::clear")]
    #[doc(alias = "ArmorInventory::setHelmet")]
    #[doc(alias = "ArmorInventory::setChestplate")]
    #[doc(alias = "ArmorInventory::setLeggings")]
    #[doc(alias = "ArmorInventory::setBoots")]
    pub fn set(&mut self, slot: usize, stack: Stack) -> Result<()> {
        match &mut self.target {
            Target::Player(p, kind) => put_player(p, *kind, slot, stack),
            Target::Offline(o, kind) => put_offline(o, *kind, slot, stack),
            Target::Block(b) => put_block(b, slot, stack),
        }
    }

    /// Empties slot `slot` of the inventory.
    fn clear(&mut self, slot: usize) {
        #[expect(
            clippy::expect_used,
            reason = "invariant: an empty stack goes into every slot the inventory has"
        )]
        self.set(slot, Stack::EMPTY)
            .expect("an empty stack goes into a slot of the inventory");
    }

    /// Writes planned slots; a write that fails gives its items back to `left` (added items
    /// that did not go in, or taken items that stay).
    fn write(&mut self, writes: Vec<(usize, Stack)>, mut left: Stack, plan: Planned) -> Stack {
        for (i, stack) in writes {
            let before = self.view().item(i).cloned().unwrap_or_default();
            let (moved, template) = match plan {
                Planned::Add => (stack.count().saturating_sub(before.count()), stack.clone()),
                Planned::Remove => (before.count().saturating_sub(stack.count()), before),
            };
            if self.set(i, stack).is_err() {
                let count = left.count().saturating_add(moved);
                left = if left.is_empty() {
                    template.with_count(count)
                } else {
                    left.with_count(count)
                };
            }
        }
        left
    }
}

/// Writes slot `slot` of a player's inventory `kind`.
fn put_player(player: &mut Player, kind: Kind, slot: usize, stack: Stack) -> Result<()> {
    if let Some(r) = own_slot(&player.inventory, kind, slot, &stack)? {
        set_own(player, r, stack);
    }
    Ok(())
}

/// Writes slot `slot` of an offline player's inventory `kind`: its record only, with no stack
/// network ID, which a join gives every stack.
fn put_offline(record: &mut Offline, kind: Kind, slot: usize, stack: Stack) -> Result<()> {
    let Some(r) = own_slot(&record.inventory, kind, slot, &stack)? else {
        return Ok(());
    };
    let slot = Slot {
        stack,
        id: StackId::default(),
    };
    #[expect(
        clippy::expect_used,
        reason = "invariant: own_slot gives only slots the player's own inventories have"
    )]
    record
        .inventory
        .put(r, slot)
        .then_some(())
        .expect("own_slot gives an own slot");
    record.change_inventory();
    Ok(())
}

/// The slot of a player's slots `inv` that a write of `stack` into slot `slot` of inventory
/// `kind` changes; `None` if it holds `stack` already. Fails for a slot the inventory does not
/// have, and for a new stack on the cursor past [`MAX_SAVED_PENDING`].
fn own_slot(inv: &Slots, kind: Kind, slot: usize, stack: &Stack) -> Result<Option<SlotRef>> {
    let i = u8::try_from(slot)
        .ok()
        .filter(|&i| usize::from(i) < kind.size())
        .ok_or(Error::NoSlot)?;
    let r = match kind {
        Kind::Main => SlotRef::Main(i),
        Kind::Armor => SlotRef::Armor(i),
        Kind::Offhand => SlotRef::Offhand,
        Kind::Cursor => SlotRef::Cursor,
        Kind::EnderChest => SlotRef::Ender(i),
    };
    #[expect(
        clippy::expect_used,
        reason = "invariant: a slot below kind's size is one of the player's own slots"
    )]
    let before = inv.get(r).expect("own_slot's slot is an own slot").stack;
    if before == *stack {
        return Ok(None);
    }
    let held = kind == Kind::Cursor;
    if held && before.is_empty() && inv.pending.len() + held_now(inv) >= MAX_SAVED_PENDING {
        return Err(Error::Full);
    }
    Ok(Some(r))
}

/// Writes window slot `slot` of a container block.
fn put_block(b: &mut BlockSlots<'_>, slot: usize, stack: Stack) -> Result<()> {
    let blocks = &b.state.rules.block_entities;
    let first = blocks.get(b.world, b.pos).map_or(0, |e| e.items.len());
    let (pos, index) = match slot.checked_sub(first) {
        Some(i) => (b.pair.ok_or(Error::NoSlot)?, i),
        None => (b.pos, slot),
    };
    if !block_entity::set_slot(b.state, b.world, pos, index, stack)? {
        return Ok(());
    }
    let State { players, rules, .. } = &mut *b.state;
    for viewer in rules.block_entities.viewers(b.world, pos) {
        if let Some(p) = players.get_mut(viewer) {
            request::notify(p, &rules.block_entities, b.world, pos, index);
        }
    }
    Ok(())
}

/// The container block at `pos`: its first slots' block (a double chest's left half) and the
/// other half; `None` for a block without container slots.
fn container(
    state: &State,
    world: WorldIndex,
    pos: BlockPos,
) -> Option<(BlockPos, Option<BlockPos>)> {
    let entity = state.rules.block_entities.get(world, pos)?;
    match entity.kind {
        block_entity::Kind::Chest { .. } => {
            Some(block_entity_halves(&state.rules.block_entities, world, pos))
        }
        block_entity::Kind::Barrel | block_entity::Kind::ShulkerBox { .. } => Some((pos, None)),
        _ => None,
    }
}

/// The slots of the container at `pos` and its other half.
fn block_view(
    state: &State,
    world: WorldIndex,
    pos: BlockPos,
    pair: Option<BlockPos>,
) -> Inventory<'_> {
    let blocks = &state.rules.block_entities;
    let items = |p: Option<BlockPos>| {
        p.and_then(|p| blocks.get(world, p))
            .map_or(&[][..], |e| e.items.as_slice())
    };
    let view = Inventory::new(items(Some(pos)), items(pair), MAX_STACK);
    match blocks.get(world, pos).map(|e| &e.kind) {
        Some(block_entity::Kind::ShulkerBox { .. }) => view.of_shulker_box(),
        _ => view,
    }
}

/// How many stacks the player holds outside its slots now ([`held_count`]).
fn held_now(inv: &Slots) -> usize {
    held_count(inv, |r| inv.get(r).is_some_and(|s| !s.stack.is_empty()))
}

/// Inventory `kind` of a player's slots `inv`, to read.
fn view_of(inv: &Slots, kind: Kind) -> Inventory<'_> {
    let stacks: &[Stack] = match kind {
        Kind::Main => &inv.main.stacks,
        Kind::Armor => &inv.armor.stacks,
        Kind::Offhand => &inv.offhand.stacks,
        Kind::Cursor => &inv.cursor.stacks,
        Kind::EnderChest => &inv.ender.stacks,
    };
    Inventory::new(stacks, &[], MAX_STACK)
}
