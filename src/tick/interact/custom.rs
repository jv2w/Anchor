//! Custom blocks in the interaction rules: the server's custom item types and block states for the
//! API, the drops of a broken custom block, and the calls into a custom block's
//! [`Behaviour`](crate::custom::Behaviour).

use super::super::{Context, SessionId, State};
use crate::block;
use crate::custom::{self, Drops};
use crate::item::{self, Enchantment, Stack};
use crate::nbt::Compound;
use crate::random::Random;
use crate::save::WorldIndex;
use crate::world::BlockPos;

use super::drops;

impl Context<'_> {
    /// The item type of the registered custom item `item`; `None` if the server was not given
    /// it ([`ServerBuilder::custom_item`](crate::ServerBuilder::custom_item)).
    pub fn custom_item(&self, item: &custom::Item) -> Option<item::Type> {
        self.state.custom.item_type(item)
    }

    /// The default state of the registered custom block `block` (the first value of each
    /// property; [`block::State::with`] changes one); `None` if the server was not given it.
    pub fn custom_block(&self, block: &custom::Block) -> Option<block::State> {
        self.state.custom.block_state(block)
    }

    /// The item type an [`ItemRef`](custom::ItemRef) names: a vanilla one, or a registered
    /// custom item or block item.
    pub fn item_of(&self, item: custom::ItemRef) -> Option<item::Type> {
        self.state.custom.resolve(item)
    }

    /// The stack a disk item compound holds, read as Anchor reads its saves, the server's custom
    /// items and blocks included: empty for air or a count of 0; `None` for an unknown item, a
    /// compound that is no item or a stack over a limit. A count above the item's stack size is
    /// cut to it. [`Stack::to_nbt`] writes the form.
    #[doc(alias = "Item::nbtDeserialize")]
    pub fn stack_from_nbt(&self, nbt: &Compound) -> Option<Stack> {
        Stack::from_nbt_with(nbt, &self.state.custom)
    }
}

/// What a broken custom block drops (PMMP `Block::getDrops` with the definition's drops): with a
/// compatible tool its drops, or itself with Silk Touch where its drops are not itself; nothing
/// with an incompatible tool. The experience is the definition's range, with a compatible tool
/// and without Silk Touch.
pub(super) fn drops_of(
    state: &State,
    block: block::State,
    tool: &Stack,
    rng: &mut Random,
) -> (Vec<Stack>, u32) {
    let Some(def) = block.custom_block() else {
        return (Vec::new(), 0);
    };
    if !drops::is_compatible(block, tool) {
        return (Vec::new(), 0);
    }
    let own = || match block.item() {
        block::ItemForm::Item(t) => vec![Stack::new(t, 1)],
        block::ItemForm::None | block::ItemForm::BlockEntity => Vec::new(),
    };
    let silk = drops::level(tool, Enchantment::SilkTouch) > 0;
    let items = match def.dropped() {
        Drops::Nothing if !silk => Vec::new(),
        Drops::Items(list) if !silk => list
            .iter()
            .filter_map(|&(r, count)| Some(Stack::new(state.custom.resolve(r)?, count)))
            .collect(),
        Drops::Itself | Drops::Nothing | Drops::Items(_) => own(),
    };
    let (min, max) = def.experience_range();
    let xp = if silk || max == 0 {
        0
    } else {
        u32::from(min) + rng.below(u32::from(max - min) + 1)
    };
    (items, xp)
}

/// `on_use` of the custom block `block` at `pos`, used by `session`: whether it did something.
pub(super) fn used(
    state: &mut State,
    session: SessionId,
    world: WorldIndex,
    pos: BlockPos,
    block: block::State,
) -> bool {
    let Some(hooks) = block
        .custom_block()
        .and_then(custom::Block::behaviour_hooks)
    else {
        return false;
    };
    hooks.on_use(
        &mut Context { state },
        crate::player::Id::new(session),
        crate::world::Id::of(world),
        pos,
        block,
    )
}

/// `on_place` of the custom block `block` just placed at `pos`.
pub(super) fn placed(
    state: &mut State,
    session: Option<SessionId>,
    world: WorldIndex,
    pos: BlockPos,
    block: block::State,
) {
    if let Some(hooks) = block
        .custom_block()
        .and_then(custom::Block::behaviour_hooks)
    {
        let world = crate::world::Id::of(world);
        hooks.on_place(
            &mut Context { state },
            session.map(crate::player::Id::new),
            world,
            pos,
            block,
        );
    }
}

/// `on_random_tick` of the custom block `block` at `pos`, which a random tick reached (the block
/// update rules call it only for a definition that takes random ticks).
pub(in crate::tick) fn random_tick(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    block: block::State,
) {
    if let Some(hooks) = block
        .custom_block()
        .and_then(custom::Block::behaviour_hooks)
    {
        let world = crate::world::Id::of(world);
        hooks.on_random_tick(&mut Context { state }, world, pos, block);
    }
}

/// `on_break` of the custom block `block` just broken at `pos`.
pub(super) fn broken(
    state: &mut State,
    session: Option<SessionId>,
    world: WorldIndex,
    pos: BlockPos,
    block: block::State,
) {
    if let Some(hooks) = block
        .custom_block()
        .and_then(custom::Block::behaviour_hooks)
    {
        let world = crate::world::Id::of(world);
        hooks.on_break(
            &mut Context { state },
            session.map(crate::player::Id::new),
            world,
            pos,
            block,
        );
    }
}
