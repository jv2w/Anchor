//! The public inventory API: a player's inventories, its hotbar selection and window on [`Player`],
//! container blocks on [`Context`], and the writes of [`Editor`].
//!
//! A write outside a request gives the slot a new stack network ID and sends it to the client
//! that shows it ([`super::set_own`] for a player's slots; [`request::notify`] for every viewer of
//! a container), so the next request of a client that still shows the old stack is refused.
//! Writes raise no events.

use super::super::{Context, Player, State, block_entity};
use super::{
    MAX_SAVED_PENDING, Notice, PublicWindow as Window, SlotRef, block_entity_halves, close_now,
    hotbar, own_slot_packet, own_window, public_window, request, select,
};
use crate::inventory::{BlockSlots, Editor, Error, Inventory, Kind, MAX_STACK, Result, Target};
use crate::item::Stack;
use crate::protocol::packet::{Packet, PlayerHotBar};
use crate::save::WorldIndex;
use crate::world::{self, BlockPos};

impl Player {
    /// One of the player's inventories, to read.
    #[doc(alias = "Human::getInventory")]
    #[doc(alias = "Living::getArmorInventory")]
    #[doc(alias = "Human::getOffHandInventory")]
    #[doc(alias = "Player::getCursorInventory")]
    #[doc(alias = "Player::getCraftingGrid")]
    #[doc(alias = "Human::getEnderInventory")]
    pub fn inventory(&self, kind: Kind) -> Inventory<'_> {
        let inv = &self.inventory;
        let stacks: &[Stack] = match kind {
            Kind::Main => &inv.main.stacks,
            Kind::Armor => &inv.armor.stacks,
            Kind::Offhand => &inv.offhand.stacks,
            Kind::Cursor => &inv.cursor.stacks,
            Kind::Crafting => &inv.grid.stacks,
            Kind::EnderChest => &inv.ender.stacks,
        };
        Inventory::new(stacks, &[], MAX_STACK)
    }

    /// One of the player's inventories, to change; each write is sent to its client.
    pub fn inventory_mut(&mut self, kind: Kind) -> Editor<'_> {
        Editor {
            target: Target::Player(self, kind),
        }
    }

    /// The selected hotbar slot, 0 to 8.
    #[doc(alias = "PlayerInventory::getHeldItemIndex")]
    pub fn held_slot(&self) -> u8 {
        self.inventory.held
    }

    /// Selects hotbar slot `slot` (0 to 8) and tells the client; false, changing nothing, for
    /// another slot.
    #[doc(alias = "PlayerInventory::setHeldItemIndex")]
    pub fn set_held_slot(&mut self, slot: u8) -> bool {
        if slot >= super::HOTBAR {
            return false;
        }
        if slot != self.inventory.held {
            select(self, slot);
        }
        true
    }

    /// The stack in the selected hotbar slot.
    #[doc(alias = "PlayerInventory::getItemInHand")]
    pub fn held_item(&self) -> &Stack {
        self.inventory.held()
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
    /// closes: a crafting table's grid (0 to 8), an enchanting table's input (0) and lapis (1),
    /// an anvil's input and material, a smithing table's input, material and template, a
    /// stonecutter's input, a loom's banner, dye and pattern, a beacon's payment, a trade's two
    /// payments. `None` without an open window or such a slot.
    #[doc(alias = "EnchantInventory::getInput")]
    #[doc(alias = "EnchantInventory::getLapis")]
    pub fn window_item(&self, slot: usize) -> Option<&Stack> {
        self.inventory
            .window
            .as_ref()
            .and_then(|w| w.ui.get(slot))
            .map(|s| &s.stack)
    }

    /// Sends the client slot `slot` of the player's inventory `kind` as the server holds it, with
    /// its stack network ID, undoing what the client predicted there (`InventorySlot`; the off
    /// hand as `InventoryContent`, as the client may ignore its slot). False, sending nothing,
    /// for a slot past the inventory's size, or the ender chest contents while no ender chest is
    /// open.
    #[doc(alias = "InventoryManager::syncSlot")]
    pub fn send_inventory_slot(&mut self, kind: Kind, slot: usize) -> bool {
        let packet = slot_ref(kind, slot).and_then(|r| {
            let s = self.inventory.get(r)?;
            own_slot_packet(&self.inventory, r, s.stack.to_instance(s.id.net))
        });
        let Some(packet) = packet else {
            return false;
        };
        self.send_packet(packet);
        true
    }

    /// Sends the client the whole window that shows the player's inventory `kind` as the server
    /// holds it (`InventoryContent`): the inventory, armour or off hand window; the cursor and
    /// the crafting grid both send the UI window, which holds them and the open window's own
    /// slots. False, sending nothing, for the ender chest contents while no ender chest is open.
    #[doc(alias = "InventoryManager::syncContents")]
    pub fn send_inventory(&mut self, kind: Kind) -> bool {
        let Some(packet) = own_window(&self.inventory, kind) else {
            return false;
        };
        self.send_packet(packet);
        true
    }

    /// Tells the client the held hotbar slot (`PlayerHotBar`) without asking it to select the
    /// slot, as BDS ends its answer to an attack.
    #[doc(alias = "InventoryManager::syncSelectedHotbarSlot")]
    pub fn send_hotbar(&mut self) {
        let packet = PlayerHotBar {
            select_hot_bar_slot: false,
            ..hotbar(self.inventory.held)
        };
        self.send_packet(Packet::PlayerHotBar(packet));
    }

    /// Makes the client select the held hotbar slot again (`PlayerHotBar` that selects it), as
    /// BDS answers an attack made from another slot.
    pub fn reselect_held_slot(&mut self) {
        self.send_packet(Packet::PlayerHotBar(hotbar(self.inventory.held)));
    }
}

impl Context<'_> {
    /// The slots of the container block at `pos` in `world` (a chest, both halves of a double
    /// chest from either half, a barrel, a shulker box, a hopper, a furnace, a brewing stand, a
    /// campfire); `None` if no loaded container is there.
    #[doc(alias = "Container::getRealInventory")]
    #[doc(alias = "InventoryHolder::getInventory")]
    pub fn block_inventory(&self, world: world::Id, pos: BlockPos) -> Option<Inventory<'_>> {
        let (pos, pair, max) = container(self.state, world.index(), pos)?;
        Some(block_view(self.state, world.index(), pos, pair, max))
    }

    /// The slots of the container block at `pos` in `world`, to change; each write is sent to
    /// every player with its window open.
    pub fn block_inventory_mut(&mut self, world: world::Id, pos: BlockPos) -> Option<Editor<'_>> {
        let (pos, pair, max) = container(self.state, world.index(), pos)?;
        Some(Editor {
            target: Target::Block(BlockSlots {
                state: self.state,
                world: world.index(),
                pos,
                pair,
                max,
            }),
        })
    }

    /// The players with a window open on the block at `pos` in `world` (a container, both halves
    /// of a double chest from either half, or a station such as an anvil), in the order they
    /// opened it; how many there are is the iterator's length.
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

    /// Switches the creative inventory `player` sees (PMMP `Player::setCreativeInventory`): the
    /// alternative registered as `inventory`
    /// ([`ServerBuilder::alternative_creative_inventory`](crate::ServerBuilder::alternative_creative_inventory)),
    /// or the default one for `None`. A spawned player's client gets the new list. False,
    /// changing nothing, if the player has left or no alternative has that name.
    #[doc(alias = "Player::setCreativeInventory")]
    pub fn set_creative_inventory(
        &mut self,
        player: crate::player::Id,
        inventory: Option<&str>,
    ) -> bool {
        let Some(index) = self.state.custom.creative_index(inventory) else {
            return false;
        };
        let Some(p) = self.state.players.get_mut(&player.session()) else {
            return false;
        };
        if p.inventory.creative != index {
            p.inventory.creative = index;
            // Before its chunk radius request the client gets the inventory in its spawn window.
            p.pending.creative |= p.chunk_radius.is_some();
        }
        true
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

    /// Closes the player's window, a menu too: the cursor, the crafting grid and the window's own
    /// slots go back to its inventory (what does not fit drops), and a menu's blocks show again.
    /// False if the player has left or has no window open. Raises no event.
    #[doc(alias = "Player::removeCurrentWindow")]
    pub fn close_window(&mut self, player: crate::player::Id) -> bool {
        let session = player.session();
        let open = self
            .state
            .players
            .get(&session)
            .is_some_and(|p| p.inventory.window.is_some());
        if open {
            close_now(self.state, session, Notice::Send);
        }
        open
    }
}

/// The container block at `pos`: its first slots' block (a double chest's left half), the other
/// half, and the most items a slot holds; `None` for a block without container slots.
fn container(
    state: &State,
    world: WorldIndex,
    pos: BlockPos,
) -> Option<(BlockPos, Option<BlockPos>, u8)> {
    let entity = state.rules.block_entities.get(world, pos)?;
    match entity.kind {
        block_entity::Kind::Chest { .. } => {
            let (left, right) = block_entity_halves(&state.rules.block_entities, world, pos);
            Some((left, right, MAX_STACK))
        }
        block_entity::Kind::Barrel
        | block_entity::Kind::ShulkerBox { .. }
        | block_entity::Kind::Hopper
        | block_entity::Kind::Furnace(_)
        | block_entity::Kind::BrewingStand(_) => Some((pos, None, MAX_STACK)),
        // PMMP `CampfireInventory`: one item a slot.
        block_entity::Kind::Campfire(_) => Some((pos, None, 1)),
        _ => None,
    }
}

/// The slots of the container at `pos` and its other half.
fn block_view(
    state: &State,
    world: WorldIndex,
    pos: BlockPos,
    pair: Option<BlockPos>,
    max: u8,
) -> Inventory<'_> {
    let blocks = &state.rules.block_entities;
    let items = |p: Option<BlockPos>| {
        p.and_then(|p| blocks.get(world, p))
            .map_or(&[][..], |e| e.items.as_slice())
    };
    let view = Inventory::new(items(Some(pos)), items(pair), max);
    match blocks.get(world, pos).map(|e| &e.kind) {
        Some(block_entity::Kind::ShulkerBox { .. }) => view.of_shulker_box(),
        _ => view,
    }
}

/// The stacks the player holds outside its slots: the cursor, the 2x2 grid and the window's own
/// slots (the invariant of [`MAX_SAVED_PENDING`]).
fn held_outside(inv: &super::Inventory) -> usize {
    super::held_outside(inv)
        .filter(|r| inv.get(*r).is_some_and(|s| !s.stack.is_empty()))
        .count()
}

impl Editor<'_> {
    /// The inventory as it is now, to read.
    pub fn view(&self) -> Inventory<'_> {
        match &self.target {
            Target::Player(p, kind) => p.inventory(*kind),
            Target::Block(b) => block_view(b.state, b.world, b.pos, b.pair, b.max),
        }
    }

    /// Puts `stack` into `slot` (an empty stack clears it) and tells the clients that show it.
    #[doc(alias = "Inventory::setItem")]
    #[doc(alias = "BaseInventory::setItem")]
    #[doc(alias = "CraftingGrid::setItem")]
    #[doc(alias = "Inventory::clear")]
    #[doc(alias = "BaseInventory::clear")]
    #[doc(alias = "ArmorInventory::setHelmet")]
    #[doc(alias = "ArmorInventory::setChestplate")]
    #[doc(alias = "ArmorInventory::setLeggings")]
    #[doc(alias = "ArmorInventory::setBoots")]
    pub fn set(&mut self, slot: usize, stack: Stack) -> Result<()> {
        match &mut self.target {
            Target::Player(p, kind) => put_player(p, *kind, slot, stack),
            Target::Block(b) => put_block(b, slot, stack),
        }
    }

    /// Puts `stacks` into the first slots in order and empties the rest. Fails, changing
    /// nothing, with more stacks than slots or a stack a slot cannot take.
    #[doc(alias = "Inventory::setContents")]
    #[doc(alias = "BaseInventory::setContents")]
    pub fn set_contents(&mut self, stacks: &[Stack]) -> Result<()> {
        let view = self.view();
        if stacks.len() > view.size() {
            return Err(Error::TooLong);
        }
        if stacks.iter().any(|s| s.count() > view.max_stack_size()) {
            return Err(Error::TooMany);
        }
        if stacks.iter().any(|s| view.refuses(s)) {
            return Err(Error::NotAccepted);
        }
        let size = view.size();
        for i in 0..size {
            let stack = stacks.get(i).cloned().unwrap_or_default();
            self.set(i, stack)?;
        }
        Ok(())
    }

    /// Empties every slot.
    #[doc(alias = "Inventory::clearAll")]
    #[doc(alias = "BaseInventory::clearAll")]
    pub fn clear_all(&mut self) {
        for i in 0..self.view().size() {
            // Clearing a slot cannot fail.
            let _cleared = self.set(i, Stack::EMPTY);
        }
    }

    /// Puts `stack` where it fits (PMMP `addItem`): onto the stacks it stacks with, in slot
    /// order, then into empty slots. Returns what did not fit.
    #[doc(alias = "Inventory::addItem")]
    #[doc(alias = "BaseInventory::addItem")]
    pub fn add(&mut self, stack: &Stack) -> Stack {
        let (writes, left) = self.view().plan_add(stack);
        self.write(writes, left, Planned::Add)
    }

    /// Takes `stack.count()` of `stack`'s item (its damage, and its extra data when it has some)
    /// from the slots that hold it, in slot order (PMMP `removeItem`). Returns what could not be
    /// taken.
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
            let _cleared = self.set(i, Stack::EMPTY);
        }
    }

    /// Exchanges the stacks of slots `a` and `b`.
    #[doc(alias = "Inventory::swap")]
    #[doc(alias = "BaseInventory::swap")]
    pub fn swap(&mut self, a: usize, b: usize) -> Result<()> {
        let view = self.view();
        let (sa, sb) = match (view.item(a), view.item(b)) {
            (Some(sa), Some(sb)) => (sa.clone(), sb.clone()),
            _ => return Err(Error::NoSlot),
        };
        // Both slots hold a stack of the inventory already, so both writes fit. The slot that
        // empties is written first, so the cursor and grid never hold one stack more than before.
        if sa.is_empty() {
            self.set(b, sa)?;
            self.set(a, sb)
        } else {
            self.set(a, sb)?;
            self.set(b, sa)
        }
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

/// What planned writes do ([`Editor::write`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Planned {
    /// Each write puts more of the item into its slot.
    Add,
    /// Each write takes some of the item out of its slot.
    Remove,
}

/// Writes slot `slot` of a player's inventory `kind`.
fn put_player(player: &mut Player, kind: Kind, slot: usize, stack: Stack) -> Result<()> {
    let r = slot_ref(kind, slot).ok_or(Error::NoSlot)?;
    let inv = &player.inventory;
    let before = inv.get(r).map(|s| s.stack).unwrap_or_default();
    if before == stack {
        return Ok(());
    }
    let held = matches!(kind, Kind::Cursor | Kind::Crafting);
    if held && before.is_empty() && inv.pending.len() + held_outside(inv) >= MAX_SAVED_PENDING {
        return Err(Error::Full);
    }
    super::set_own(player, r, stack);
    Ok(())
}

/// Slot `slot` of a player's inventory `kind`; `None` past its size.
fn slot_ref(kind: Kind, slot: usize) -> Option<SlotRef> {
    let i = u8::try_from(slot)
        .ok()
        .filter(|&i| usize::from(i) < kind.size())?;
    Some(match kind {
        Kind::Main => SlotRef::Main(i),
        Kind::Armor => SlotRef::Armor(i),
        Kind::Offhand => SlotRef::Offhand,
        Kind::Cursor => SlotRef::Cursor,
        Kind::Crafting => SlotRef::Grid(i),
        Kind::EnderChest => SlotRef::Ender(i),
    })
}

/// Writes window slot `slot` of a container block.
fn put_block(b: &mut BlockSlots<'_>, slot: usize, stack: Stack) -> Result<()> {
    if stack.count() > b.max {
        return Err(Error::TooMany);
    }
    let blocks = &b.state.rules.block_entities;
    let first = blocks.get(b.world, b.pos).map_or(0, |e| e.items.len());
    let (pos, index) = match slot.checked_sub(first) {
        None => (b.pos, slot),
        Some(i) => (b.pair.ok_or(Error::NoSlot)?, i),
    };
    if !block_entity::set_slot(b.state, b.world, pos, index, stack)? {
        return Ok(());
    }
    let State { players, rules, .. } = &mut *b.state;
    for viewer in rules.block_entities.viewers(b.world, pos) {
        if let Some(p) = players.get_mut(viewer) {
            request::notify(
                p,
                &rules.block_entities,
                b.world,
                pos,
                index,
                request::Renumber::New,
            );
        }
    }
    Ok(())
}
