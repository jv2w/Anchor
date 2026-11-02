//! What a broken block drops: its own item, the one picking the block gives.

use crate::block::{self, ItemForm};
use crate::item::{Enchantment, Stack};
use crate::save::WorldIndex;
use crate::tick::{State, block_entity};
use crate::world::BlockPos;

/// The item of `block` at `pos`: one of its item form, or the bed or banner its block entity
/// describes; `None` for a block without an item form.
pub(in crate::tick) fn own_item(
    state: &State,
    world: WorldIndex,
    pos: BlockPos,
    block: block::State,
) -> Option<Stack> {
    match block.item() {
        ItemForm::Item(item) => Some(Stack::new(item, 1)),
        ItemForm::BlockEntity => block_entity::own_item(state, world, pos),
        ItemForm::None => None,
    }
}

/// The level of an enchantment on a stack, 0 if it has none.
pub(in crate::tick) fn level(stack: &Stack, enchantment: Enchantment) -> u16 {
    stack.enchantment_level(enchantment).unwrap_or(0)
}
