//! Block events (PMMP `pocketmine\event\block`): blocks broken, placed, grown, burnt and changed by
//! the world.
//!
//! A block event names its block by world, position and state. Other blocks it mentions are still
//! in the world while the listeners run, so the event gives their positions only.

use super::{MAX_SAVED_FUEL_TIME, fire_chance_after, set_clamped, set_radius};
use crate::block::State;
use crate::item::Stack;
use crate::math::Vec3;
use crate::player;
use crate::tile::Side;
use crate::world::{self, BlockPos};

/// Raised when a player breaks a block, before it goes. Cancelling keeps the block.
#[doc(alias = "BlockBreakEvent")]
#[derive(Debug)]
pub struct BlockBreak {
    pub(crate) player: player::Id,
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) item: Stack,
    pub(crate) insta_break: bool,
    pub(crate) drops: Vec<Stack>,
    pub(crate) xp_drop_amount: u32,
    pub(crate) cancelled: bool,
}

getters!(BlockBreak {
    /// The player breaking the block.
    #[doc(alias = "BlockBreakEvent::getPlayer")]
    player: player::Id,
    /// The world of the block.
    world: world::Id,
    /// Where the block is.
    pos: BlockPos,
    /// The block being broken, or `minecraft:info_update` for an inert block (one Anchor keeps as
    /// it was saved but gives no behaviour).
    #[doc(alias = "BlockEvent::getBlock")]
    block: State,
    /// What the player breaks it with (empty for a bare hand).
    #[doc(alias = "BlockBreakEvent::getItem")]
    item: &Stack,
    /// Whether the break skipped the break time check: true for creative players, breaks the
    /// API asks for and servers with `break-time-check = false`. A player's break that has not
    /// lasted its time is refused before the event, so no listener can let it through.
    #[doc(alias = "BlockBreakEvent::getInstaBreak")]
    insta_break: bool,
    /// What the block drops. Starts as its drops for the item.
    #[doc(alias = "BlockBreakEvent::getDrops")]
    drops: &[Stack],
    /// The experience the block drops.
    #[doc(alias = "BlockBreakEvent::getXpDropAmount")]
    xp_drop_amount: u32,
});

setters!(BlockBreak {
    /// Replaces what the block drops.
    #[doc(alias = "BlockBreakEvent::setDrops")]
    #[doc(alias = "BlockBreakEvent::setDropsVariadic")]
    set_drops => drops: Vec<Stack>,
    /// Replaces the experience the block drops.
    #[doc(alias = "BlockBreakEvent::setXpDropAmount")]
    set_xp_drop_amount => xp_drop_amount: u32,
});

cancellable! {
    BlockBreak
        #[doc(alias = "BlockBreakEvent::cancel")]
        #[doc(alias = "BlockBreakEvent::uncancel")]
        #[doc(alias = "BlockBreakEvent::isCancelled")];
}

/// Raised when fire burns a block away; TNT is primed instead. Cancelling keeps the block.
#[doc(alias = "BlockBurnEvent")]
#[derive(Debug)]
pub struct BlockBurn {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) causing_block: BlockPos,
    pub(crate) cancelled: bool,
}

getters!(BlockBurn {
    /// The world of the block.
    world: world::Id,
    /// Where the block is.
    pos: BlockPos,
    /// The block that burns.
    block: State,
    /// Where the fire that burns it is.
    #[doc(alias = "BlockBurnEvent::getCausingBlock")]
    causing_block: BlockPos,
});

cancellable! {
    BlockBurn
        #[doc(alias = "BlockBurnEvent::cancel")]
        #[doc(alias = "BlockBurnEvent::uncancel")]
        #[doc(alias = "BlockBurnEvent::isCancelled")];
}

/// Raised when a block dies: coral without water becomes dead coral. Cancelling keeps it alive.
#[doc(alias = "BlockDeathEvent")]
#[derive(Debug)]
pub struct BlockDeath {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) new_state: State,
    pub(crate) cancelled: bool,
}

getters!(BlockDeath {
    /// The world of the block.
    world: world::Id,
    /// Where the block is.
    pos: BlockPos,
    /// The living block.
    block: State,
    /// The dead block it becomes.
    new_state: State,
});

cancellable! {
    BlockDeath;
}

/// Raised when a block explodes (a respawn anchor), after the blocks it breaks are known and
/// before they break. Cancelling keeps the blocks and hurts nobody; the exploding block is gone
/// either way.
#[doc(alias = "BlockExplodeEvent")]
#[derive(Debug)]
pub struct BlockExplode {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) position: Vec3,
    pub(crate) blocks: Vec<BlockPos>,
    pub(crate) yield_percent: f64,
    pub(crate) ignitions: Vec<BlockPos>,
    pub(crate) cancelled: bool,
}

getters!(BlockExplode {
    /// The world of the block.
    world: world::Id,
    /// Where the exploding block was.
    pos: BlockPos,
    /// The block that explodes.
    block: State,
    /// The centre of the explosion.
    #[doc(alias = "BlockExplodeEvent::getPosition")]
    position: Vec3,
    /// The blocks that break.
    #[doc(alias = "BlockExplodeEvent::getAffectedBlocks")]
    blocks: &[BlockPos],
    /// The chance in percent that a broken block drops its items.
    #[doc(alias = "BlockExplodeEvent::getYield")]
    yield_percent: f64,
    /// The blocks that catch fire.
    #[doc(alias = "BlockExplodeEvent::getIgnitions")]
    ignitions: &[BlockPos],
});

setters!(BlockExplode {
    /// Replaces the blocks that break. Only the first 8,192 are used, and blocks outside loaded
    /// chunks are left alone.
    #[doc(alias = "BlockExplodeEvent::setAffectedBlocks")]
    set_blocks => blocks: Vec<BlockPos>,
    /// Replaces the blocks that catch fire. Only the first 8,192 are used, and a block catches
    /// fire only if it is air once the blocks have broken and the block under it has a full top.
    #[doc(alias = "BlockExplodeEvent::setIgnitions")]
    set_ignitions => ignitions: Vec<BlockPos>,
});

impl BlockExplode {
    /// Sets the chance in percent that a broken block drops its items, limited to 0 to 100. A
    /// value that is not finite is ignored.
    #[doc(alias = "BlockExplodeEvent::setYield")]
    pub fn set_yield_percent(&mut self, percent: f64) {
        set_clamped(&mut self.yield_percent, percent, 0.0, 100.0);
    }
}

cancellable! {
    BlockExplode
        #[doc(alias = "BlockExplodeEvent::cancel")]
        #[doc(alias = "BlockExplodeEvent::uncancel")]
        #[doc(alias = "BlockExplodeEvent::isCancelled")];
}

/// Raised when a block forms: water hardens concrete powder, lava and water make stone. Cancelling
/// keeps the old block.
#[doc(alias = "BlockFormEvent")]
#[derive(Debug)]
pub struct BlockForm {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) new_state: State,
    pub(crate) causing_block: BlockPos,
    pub(crate) cancelled: bool,
}

getters!(BlockForm {
    /// The world of the block.
    world: world::Id,
    /// Where the block is.
    pos: BlockPos,
    /// The block before it forms.
    block: State,
    /// The block it becomes.
    new_state: State,
    /// Where the block that makes it form is: the water or lava, or the blue ice that makes
    /// basalt.
    #[doc(alias = "BlockFormEvent::getCausingBlock")]
    causing_block: BlockPos,
});

cancellable! {
    BlockForm;
}

/// Raised when a block grows by itself or by bone meal (crops, stems, cane, cactus, berries…).
/// Cancelling keeps the old block.
#[doc(alias = "BlockGrowEvent")]
#[derive(Debug)]
pub struct BlockGrow {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) new_state: State,
    pub(crate) player: Option<player::Id>,
    pub(crate) cancelled: bool,
}

getters!(BlockGrow {
    /// The world of the block.
    world: world::Id,
    /// Where the block is.
    pos: BlockPos,
    /// The block before it grows.
    block: State,
    /// The block it becomes.
    new_state: State,
    /// The player who used bone meal, if one did.
    #[doc(alias = "BlockGrowEvent::getPlayer")]
    player: Option<player::Id>,
});

cancellable! {
    BlockGrow;
}

/// Raised when a block melts: ice, frosted ice and snow in light. Cancelling keeps the block.
#[doc(alias = "BlockMeltEvent")]
#[derive(Debug)]
pub struct BlockMelt {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) new_state: State,
    pub(crate) cancelled: bool,
}

getters!(BlockMelt {
    /// The world of the block.
    world: world::Id,
    /// Where the block is.
    pos: BlockPos,
    /// The block that melts.
    block: State,
    /// The block it becomes.
    new_state: State,
});

cancellable! {
    BlockMelt;
}

/// Raised when a player places a block, before it is set. Cancelling keeps the world as it was
/// and the item in the player's hand.
#[doc(alias = "BlockPlaceEvent")]
#[derive(Debug)]
pub struct BlockPlace {
    pub(crate) player: player::Id,
    pub(crate) world: world::Id,
    pub(crate) blocks: Vec<(BlockPos, State)>,
    pub(crate) block_against: BlockPos,
    pub(crate) item: Stack,
    pub(crate) cancelled: bool,
}

getters!(BlockPlace {
    /// The player placing the block.
    #[doc(alias = "BlockPlaceEvent::getPlayer")]
    player: player::Id,
    /// The world of the blocks.
    world: world::Id,
    /// Every block the placement sets: one, or more for doors, beds and tall plants.
    #[doc(alias = "BlockPlaceEvent::getTransaction")]
    blocks: &[(BlockPos, State)],
    /// The block the player clicked to place against.
    #[doc(alias = "BlockPlaceEvent::getBlockAgainst")]
    block_against: BlockPos,
    /// The item the player places.
    #[doc(alias = "BlockPlaceEvent::getItem")]
    item: &Stack,
});

cancellable! {
    BlockPlace
        #[doc(alias = "BlockPlaceEvent::cancel")]
        #[doc(alias = "BlockPlaceEvent::uncancel")]
        #[doc(alias = "BlockPlaceEvent::isCancelled")];
}

/// Raised when a block is about to explode (a charged respawn anchor a player uses), before it
/// goes and the blocks it breaks are found. Cancelling keeps the block.
#[doc(alias = "BlockPreExplodeEvent")]
#[derive(Debug)]
pub struct BlockPreExplode {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) player: Option<player::Id>,
    pub(crate) radius: f64,
    pub(crate) fire_chance: f64,
    pub(crate) block_breaking: bool,
    pub(crate) cancelled: bool,
}

getters!(BlockPreExplode {
    /// The world of the block.
    world: world::Id,
    /// Where the block is.
    pos: BlockPos,
    /// The block that explodes.
    block: State,
    /// The player who made it explode, if one did.
    #[doc(alias = "BlockPreExplodeEvent::getPlayer")]
    player: Option<player::Id>,
    /// The radius of the explosion, in blocks.
    #[doc(alias = "BlockPreExplodeEvent::getRadius")]
    radius: f64,
    /// The chance, 0 to 1, that a block the explosion reaches catches fire.
    #[doc(alias = "BlockPreExplodeEvent::getFireChance")]
    fire_chance: f64,
});

impl BlockPreExplode {
    /// Whether the explosion breaks blocks.
    #[doc(alias = "BlockPreExplodeEvent::isBlockBreaking")]
    pub fn is_block_breaking(&self) -> bool {
        self.block_breaking
    }

    /// Whether the explosion sets fires.
    #[doc(alias = "BlockPreExplodeEvent::isIncendiary")]
    pub fn is_incendiary(&self) -> bool {
        self.fire_chance > 0.0
    }

    /// Sets the chance that a block catches fire, limited to 0 to 1. A value that is not finite
    /// is ignored.
    #[doc(alias = "BlockPreExplodeEvent::setFireChance")]
    pub fn set_fire_chance(&mut self, chance: f64) {
        set_clamped(&mut self.fire_chance, chance, 0.0, 1.0);
    }

    /// Makes the explosion set fires (with a chance of 1/3 unless one is set) or none.
    #[doc(alias = "BlockPreExplodeEvent::setIncendiary")]
    pub fn set_incendiary(&mut self, incendiary: bool) {
        self.fire_chance = fire_chance_after(self.fire_chance, incendiary);
    }

    /// Sets the radius of the explosion, limited to 8 blocks. A value that is not finite or not
    /// above 0 is ignored.
    #[doc(alias = "BlockPreExplodeEvent::setRadius")]
    pub fn set_radius(&mut self, radius: f64) {
        set_radius(&mut self.radius, radius);
    }
}

setters!(BlockPreExplode {
    /// Sets whether the explosion breaks blocks.
    #[doc(alias = "BlockPreExplodeEvent::setBlockBreaking")]
    set_block_breaking => block_breaking: bool,
});

cancellable! {
    BlockPreExplode
        #[doc(alias = "BlockPreExplodeEvent::cancel")]
        #[doc(alias = "BlockPreExplodeEvent::uncancel")]
        #[doc(alias = "BlockPreExplodeEvent::isCancelled")];
}

/// Raised when a block spreads: grass and mycelium onto dirt, fire to a neighbour, a liquid into a
/// block; also when a block turns in place into what spreads (grass in the dark to dirt, covered
/// nylium to netherrack), then [`source`](Self::source) is [`pos`](Self::pos). Cancelling keeps
/// the old block, but a fire that spreads into a block it burnt leaves air there: the block is
/// already burnt.
#[doc(alias = "BlockSpreadEvent")]
#[derive(Debug)]
pub struct BlockSpread {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) new_state: State,
    pub(crate) source: BlockPos,
    pub(crate) cancelled: bool,
}

getters!(BlockSpread {
    /// The world of the block.
    world: world::Id,
    /// Where the block is.
    pos: BlockPos,
    /// The block spread to.
    block: State,
    /// The block it becomes.
    new_state: State,
    /// Where the block that spreads is.
    #[doc(alias = "BlockSpreadEvent::getSource")]
    source: BlockPos,
});

cancellable! {
    BlockSpread;
}

/// Raised when a dragon egg teleports. Cancelling keeps it in place.
#[doc(alias = "BlockTeleportEvent")]
#[derive(Debug)]
pub struct BlockTeleport {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) to: BlockPos,
    pub(crate) cancelled: bool,
}

getters!(BlockTeleport {
    /// The world of the block.
    world: world::Id,
    /// Where the block is.
    pos: BlockPos,
    /// The block that teleports.
    block: State,
    /// Where it goes.
    #[doc(alias = "BlockTeleportEvent::getTo")]
    to: BlockPos,
});

setters!(BlockTeleport {
    /// Sends the block elsewhere. The target is checked again (an air block in a loaded chunk);
    /// a target that fails cancels the teleport.
    #[doc(alias = "BlockTeleportEvent::setTo")]
    set_to => to: BlockPos,
});

cancellable! {
    BlockTeleport
        #[doc(alias = "BlockTeleportEvent::cancel")]
        #[doc(alias = "BlockTeleportEvent::uncancel")]
        #[doc(alias = "BlockTeleportEvent::isCancelled")];
}

/// Raised when a block is told that a neighbour changed, before it reacts: the changed block itself
/// and its six neighbours. Cancelling skips the reaction of the block and of the entities in it.
/// Scheduled updates raise no event.
#[doc(alias = "BlockUpdateEvent")]
#[derive(Debug)]
pub struct BlockUpdate {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) cancelled: bool,
}

getters!(BlockUpdate {
    /// The world of the block.
    world: world::Id,
    /// Where the block is.
    pos: BlockPos,
    /// The block that is updated.
    block: State,
});

cancellable! {
    BlockUpdate
        #[doc(alias = "BlockUpdateEvent::cancel")]
        #[doc(alias = "BlockUpdateEvent::uncancel")]
        #[doc(alias = "BlockUpdateEvent::isCancelled")];
}

/// Raised when a brewing stand finishes brewing one bottle. Cancelling keeps the bottle as it was;
/// the brew still uses up one ingredient.
#[doc(alias = "BrewItemEvent")]
#[derive(Debug)]
pub struct BrewItem {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) slot: u8,
    pub(crate) input: Stack,
    pub(crate) result: Stack,
    pub(crate) cancelled: bool,
}

getters!(BrewItem {
    /// The world of the brewing stand.
    world: world::Id,
    /// Where the brewing stand is.
    #[doc(alias = "BrewItemEvent::getBrewingStand")]
    pos: BlockPos,
    /// The brewing stand.
    block: State,
    /// The bottle slot, 1 to 3.
    #[doc(alias = "BrewItemEvent::getSlot")]
    slot: u8,
    /// The bottle before brewing.
    #[doc(alias = "BrewItemEvent::getInput")]
    input: &Stack,
    /// The bottle after brewing.
    #[doc(alias = "BrewItemEvent::getResult")]
    result: &Stack,
});

setters!(BrewItem {
    /// Replaces the bottle after brewing. An empty result keeps the bottle as it was, as a cancel
    /// does.
    #[doc(alias = "BrewItemEvent::setResult")]
    set_result => result: Stack,
});

cancellable! {
    BrewItem
        #[doc(alias = "BrewItemEvent::cancel")]
        #[doc(alias = "BrewItemEvent::uncancel")]
        #[doc(alias = "BrewItemEvent::isCancelled")];
}

/// Raised when a brewing stand uses up a blaze powder as fuel. Cancelling keeps the powder and
/// gives no fuel.
#[doc(alias = "BrewingFuelUseEvent")]
#[derive(Debug)]
pub struct BrewingFuelUse {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) fuel_time: u32,
    pub(crate) cancelled: bool,
}

getters!(BrewingFuelUse {
    /// The world of the brewing stand.
    world: world::Id,
    /// Where the brewing stand is.
    #[doc(alias = "BrewingFuelUseEvent::getBrewingStand")]
    pos: BlockPos,
    /// The brewing stand.
    block: State,
    /// How many brews the fuel lasts. Starts at 20.
    #[doc(alias = "BrewingFuelUseEvent::getFuelTime")]
    fuel_time: u32,
});

impl BrewingFuelUse {
    /// Sets how many brews the fuel lasts, 1 to 32,767 (0 counts as 1, more as 32,767).
    #[doc(alias = "BrewingFuelUseEvent::setFuelTime")]
    pub fn set_fuel_time(&mut self, fuel_time: u32) {
        self.fuel_time = fuel_time.clamp(1, MAX_SAVED_FUEL_TIME);
    }
}

cancellable! {
    BrewingFuelUse
        #[doc(alias = "BrewingFuelUseEvent::cancel")]
        #[doc(alias = "BrewingFuelUseEvent::uncancel")]
        #[doc(alias = "BrewingFuelUseEvent::isCancelled")];
}

/// Raised when a campfire finishes cooking an item. Cancelling keeps the item uncooked.
#[doc(alias = "CampfireCookEvent")]
#[derive(Debug)]
pub struct CampfireCook {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) slot: u8,
    pub(crate) input: Stack,
    pub(crate) result: Stack,
    pub(crate) cancelled: bool,
}

getters!(CampfireCook {
    /// The world of the campfire.
    world: world::Id,
    /// Where the campfire is.
    #[doc(alias = "CampfireCookEvent::getCampfire")]
    pos: BlockPos,
    /// The campfire.
    block: State,
    /// The campfire slot, 0 to 3.
    #[doc(alias = "CampfireCookEvent::getSlot")]
    slot: u8,
    /// The raw item.
    #[doc(alias = "CampfireCookEvent::getInput")]
    input: &Stack,
    /// The cooked item that drops.
    #[doc(alias = "CampfireCookEvent::getResult")]
    result: &Stack,
});

setters!(CampfireCook {
    /// Replaces the cooked item that drops. An empty result drops nothing, and the raw item is
    /// gone all the same.
    #[doc(alias = "CampfireCookEvent::setResult")]
    set_result => result: Stack,
});

cancellable! {
    CampfireCook
        #[doc(alias = "CampfireCookEvent::cancel")]
        #[doc(alias = "CampfireCookEvent::uncancel")]
        #[doc(alias = "CampfireCookEvent::isCancelled")];
}

/// Raised when a chest that gets its block entity (placed, set by code, or opened without one)
/// joins the chest next to it into a double chest. Cancelling keeps them single.
#[doc(alias = "ChestPairEvent")]
#[derive(Debug)]
pub struct ChestPair {
    pub(crate) world: world::Id,
    pub(crate) left: BlockPos,
    pub(crate) right: BlockPos,
    pub(crate) cancelled: bool,
}

getters!(ChestPair {
    /// The world of the chests.
    world: world::Id,
    /// The left half, as seen facing the chest.
    #[doc(alias = "ChestPairEvent::getLeft")]
    left: BlockPos,
    /// The right half.
    #[doc(alias = "ChestPairEvent::getRight")]
    right: BlockPos,
});

cancellable! {
    ChestPair
        #[doc(alias = "ChestPairEvent::cancel")]
        #[doc(alias = "ChestPairEvent::uncancel")]
        #[doc(alias = "ChestPairEvent::isCancelled")];
}

/// Raised when farmland gets wetter or drier. Cancelling keeps its wetness.
#[doc(alias = "FarmlandHydrationChangeEvent")]
#[derive(Debug)]
pub struct FarmlandHydrationChange {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) old_hydration: u8,
    pub(crate) new_hydration: u8,
    pub(crate) cancelled: bool,
}

getters!(FarmlandHydrationChange {
    /// The world of the farmland.
    world: world::Id,
    /// Where the farmland is.
    pos: BlockPos,
    /// The farmland.
    block: State,
    /// The wetness before, 0 to 7.
    #[doc(alias = "FarmlandHydrationChangeEvent::getOldHydration")]
    old_hydration: u8,
    /// The wetness after, 0 to 7.
    #[doc(alias = "FarmlandHydrationChangeEvent::getNewHydration")]
    new_hydration: u8,
});

impl FarmlandHydrationChange {
    /// The wettest farmland (PMMP `Farmland::MAX_WETNESS`).
    pub(crate) const MAX_HYDRATION: u8 = 7;

    /// Sets the wetness after, at most 7 (more counts as 7).
    #[doc(alias = "FarmlandHydrationChangeEvent::setNewHydration")]
    pub fn set_new_hydration(&mut self, hydration: u8) {
        self.new_hydration = hydration.min(Self::MAX_HYDRATION);
    }
}

cancellable! {
    FarmlandHydrationChange
        #[doc(alias = "FarmlandHydrationChangeEvent::cancel")]
        #[doc(alias = "FarmlandHydrationChangeEvent::uncancel")]
        #[doc(alias = "FarmlandHydrationChangeEvent::isCancelled")];
}

/// Raised when leaves without a log nearby decay. Cancelling keeps them.
#[doc(alias = "LeavesDecayEvent")]
#[derive(Debug)]
pub struct LeavesDecay {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) cancelled: bool,
}

getters!(LeavesDecay {
    /// The world of the leaves.
    world: world::Id,
    /// Where the leaves are.
    pos: BlockPos,
    /// The leaves.
    block: State,
});

cancellable! {
    LeavesDecay
        #[doc(alias = "LeavesDecayEvent::cancel")]
        #[doc(alias = "LeavesDecayEvent::uncancel")]
        #[doc(alias = "LeavesDecayEvent::isCancelled")];
}

/// Raised when a player writes on a sign, or dyes it or lights it or dims it with a dye, an ink
/// sac or a glow ink sac. Cancelling keeps the old text and the old colour and light; a use with
/// a dye or an ink sac then opens the sign's editor.
///
/// A dye or an ink sac starts the new text equal to the old; the event does not name the colour or
/// the light.
#[doc(alias = "SignChangeEvent")]
#[derive(Debug)]
pub struct SignChange {
    pub(crate) player: player::Id,
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) old_text: [String; 4],
    pub(crate) new_text: [String; 4],
    pub(crate) side: Side,
    pub(crate) cancelled: bool,
}

getters!(SignChange {
    /// The player writing.
    #[doc(alias = "SignChangeEvent::getPlayer")]
    player: player::Id,
    /// The world of the sign.
    world: world::Id,
    /// Where the sign is.
    #[doc(alias = "SignChangeEvent::getSign")]
    pos: BlockPos,
    /// The sign.
    block: State,
    /// The four lines of the side before.
    #[doc(alias = "SignChangeEvent::getOldText")]
    old_text: &[String; 4],
    /// The four lines of the side after. The player's lines come without private-use characters
    /// and terminal escapes (colour codes stay); a listener's go on as it set them.
    #[doc(alias = "SignChangeEvent::getNewText")]
    new_text: &[String; 4],
    /// The side written on.
    #[doc(alias = "SignChangeEvent::isFrontFace")]
    side: Side,
});

setters!(SignChange {
    /// Replaces the lines written, which go on as they are (PMMP cleans only the player's text).
    /// They are checked again (no newline, at most 1,000 bytes in all); text that fails cancels
    /// the change.
    #[doc(alias = "SignChangeEvent::setNewText")]
    set_new_text => new_text: [String; 4],
});

cancellable! {
    SignChange
        #[doc(alias = "SignChangeEvent::cancel")]
        #[doc(alias = "SignChangeEvent::uncancel")]
        #[doc(alias = "SignChangeEvent::isCancelled")];
}

/// Raised when a structure grows from a block: a sapling into a tree, a fungus into a huge
/// fungus, bamboo, chorus and other plants that set many blocks at once. Cancelling keeps the
/// world as it was.
#[doc(alias = "StructureGrowEvent")]
#[derive(Debug)]
pub struct StructureGrow {
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) blocks: Vec<(BlockPos, State)>,
    pub(crate) player: Option<player::Id>,
    pub(crate) cancelled: bool,
}

getters!(StructureGrow {
    /// The world of the block.
    world: world::Id,
    /// Where the block that grows is.
    pos: BlockPos,
    /// The block that grows.
    block: State,
    /// Every block the structure sets.
    #[doc(alias = "StructureGrowEvent::getTransaction")]
    blocks: &[(BlockPos, State)],
    /// The player who used bone meal, if one did.
    #[doc(alias = "StructureGrowEvent::getPlayer")]
    player: Option<player::Id>,
});

cancellable! {
    StructureGrow
        #[doc(alias = "StructureGrowEvent::cancel")]
        #[doc(alias = "StructureGrowEvent::uncancel")]
        #[doc(alias = "StructureGrowEvent::isCancelled")];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{AIR, DIRT, GRASS_BLOCK};
    use crate::event::fixtures::{POS, player, sticks};
    use crate::event::{DEFAULT_FIRE_CHANCE, MAX_EXPLOSION_RADIUS};

    const WORLD: world::Id = world::Id::DEFAULT;
    const OTHER: BlockPos = BlockPos::new(5, 6, 7);

    // pmmp: BlockBreakEvent, BlockBreakEvent::getPlayer, BlockEvent::getBlock,
    // pmmp: BlockBreakEvent::getItem, BlockBreakEvent::getInstaBreak, BlockBreakEvent::getDrops,
    // pmmp: BlockBreakEvent::getXpDropAmount, BlockBreakEvent::setDrops,
    // pmmp: BlockBreakEvent::setDropsVariadic, BlockBreakEvent::setXpDropAmount,
    // pmmp: BlockBreakEvent::cancel, BlockBreakEvent::uncancel, BlockBreakEvent::isCancelled
    #[test]
    fn block_break_reads_and_changes_its_fields() {
        let mut e = BlockBreak {
            player: player(1),
            world: WORLD,
            pos: POS,
            block: DIRT,
            item: sticks(1),
            insta_break: false,
            drops: vec![sticks(2)],
            xp_drop_amount: 3,
            cancelled: false,
        };
        assert_eq!(
            (e.player(), e.world(), e.pos(), e.block()),
            (player(1), WORLD, POS, DIRT)
        );
        assert_eq!(e.item(), &sticks(1));
        assert!(!e.insta_break());
        assert_eq!(e.drops(), &[sticks(2)]);
        assert_eq!(e.xp_drop_amount(), 3);
        e.set_drops(Vec::new());
        e.set_xp_drop_amount(0);
        assert!(e.drops().is_empty());
        assert_eq!(e.xp_drop_amount(), 0);
        assert!(!e.is_cancelled());
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: BlockBurnEvent, BlockBurnEvent::getCausingBlock, BlockBurnEvent::cancel,
    // pmmp: BlockBurnEvent::uncancel, BlockBurnEvent::isCancelled
    #[test]
    fn block_burn_names_the_fire() {
        let mut e = BlockBurn {
            world: WORLD,
            pos: POS,
            block: DIRT,
            causing_block: OTHER,
            cancelled: false,
        };
        assert_eq!((e.world(), e.pos(), e.block()), (WORLD, POS, DIRT));
        assert_eq!(e.causing_block(), OTHER);
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: BlockDeathEvent, BlockMeltEvent
    #[test]
    fn block_changes_name_the_old_and_new_state() {
        let mut death = BlockDeath {
            world: WORLD,
            pos: POS,
            block: DIRT,
            new_state: AIR,
            cancelled: false,
        };
        assert_eq!(
            (death.world(), death.pos(), death.block(), death.new_state()),
            (WORLD, POS, DIRT, AIR)
        );
        death.cancel();
        assert!(death.is_cancelled());
        death.uncancel();
        assert!(!death.is_cancelled());
        let mut melt = BlockMelt {
            world: WORLD,
            pos: POS,
            block: DIRT,
            new_state: AIR,
            cancelled: true,
        };
        assert_eq!(
            (melt.world(), melt.pos(), melt.block(), melt.new_state()),
            (WORLD, POS, DIRT, AIR)
        );
        melt.uncancel();
        assert!(!melt.is_cancelled());
        melt.cancel();
        assert!(melt.is_cancelled());
    }

    // pmmp: BlockFormEvent, BlockGrowEvent, BlockSpreadEvent, BlockFormEvent::getCausingBlock,
    // pmmp: BlockGrowEvent::getPlayer, BlockSpreadEvent::getSource
    #[test]
    fn block_form_grow_and_spread_name_their_cause() {
        let mut form = BlockForm {
            world: WORLD,
            pos: POS,
            block: DIRT,
            new_state: GRASS_BLOCK,
            causing_block: OTHER,
            cancelled: false,
        };
        assert_eq!(
            (form.world(), form.pos(), form.block(), form.new_state()),
            (WORLD, POS, DIRT, GRASS_BLOCK)
        );
        assert_eq!(form.causing_block(), OTHER);
        form.cancel();
        assert!(form.is_cancelled());
        form.uncancel();
        assert!(!form.is_cancelled());
        let mut grow = BlockGrow {
            world: WORLD,
            pos: POS,
            block: DIRT,
            new_state: GRASS_BLOCK,
            player: Some(player(2)),
            cancelled: false,
        };
        assert_eq!(
            (grow.world(), grow.pos(), grow.block(), grow.new_state()),
            (WORLD, POS, DIRT, GRASS_BLOCK)
        );
        assert_eq!(grow.player(), Some(player(2)));
        grow.cancel();
        assert!(grow.is_cancelled());
        grow.uncancel();
        assert!(!grow.is_cancelled());
        let mut spread = BlockSpread {
            world: WORLD,
            pos: POS,
            block: DIRT,
            new_state: GRASS_BLOCK,
            source: OTHER,
            cancelled: false,
        };
        assert_eq!(
            (
                spread.world(),
                spread.pos(),
                spread.block(),
                spread.new_state()
            ),
            (WORLD, POS, DIRT, GRASS_BLOCK)
        );
        assert_eq!(spread.source(), OTHER);
        spread.cancel();
        assert!(spread.is_cancelled());
        spread.uncancel();
        assert!(!spread.is_cancelled());
    }

    // pmmp: BlockPlaceEvent, BlockPlaceEvent::getPlayer, BlockPlaceEvent::getTransaction,
    // pmmp: BlockPlaceEvent::getBlockAgainst, BlockPlaceEvent::getItem, BlockPlaceEvent::cancel,
    // pmmp: BlockPlaceEvent::uncancel, BlockPlaceEvent::isCancelled
    #[test]
    fn block_place_lists_every_block_it_sets() {
        let mut e = BlockPlace {
            player: player(1),
            world: WORLD,
            blocks: vec![(POS, DIRT), (OTHER, GRASS_BLOCK)],
            block_against: OTHER,
            item: sticks(1),
            cancelled: false,
        };
        assert_eq!((e.player(), e.world()), (player(1), WORLD));
        assert_eq!(e.blocks(), &[(POS, DIRT), (OTHER, GRASS_BLOCK)]);
        assert_eq!(e.block_against(), OTHER);
        assert_eq!(e.item(), &sticks(1));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: BlockTeleportEvent, BlockTeleportEvent::getTo, BlockTeleportEvent::setTo,
    // pmmp: BlockTeleportEvent::cancel, BlockTeleportEvent::uncancel,
    // pmmp: BlockTeleportEvent::isCancelled
    #[test]
    fn block_teleport_target_can_change() {
        let mut e = BlockTeleport {
            world: WORLD,
            pos: POS,
            block: DIRT,
            to: OTHER,
            cancelled: false,
        };
        assert_eq!(
            (e.world(), e.pos(), e.block(), e.to()),
            (WORLD, POS, DIRT, OTHER)
        );
        e.set_to(POS);
        assert_eq!(e.to(), POS);
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: BlockUpdateEvent, LeavesDecayEvent, BlockUpdateEvent::cancel,
    // pmmp: BlockUpdateEvent::uncancel, BlockUpdateEvent::isCancelled, LeavesDecayEvent::cancel,
    // pmmp: LeavesDecayEvent::uncancel, LeavesDecayEvent::isCancelled
    #[test]
    fn block_update_and_leaves_decay_can_be_cancelled() {
        let mut update = BlockUpdate {
            world: WORLD,
            pos: POS,
            block: DIRT,
            cancelled: false,
        };
        assert_eq!(
            (update.world(), update.pos(), update.block()),
            (WORLD, POS, DIRT)
        );
        update.cancel();
        assert!(update.is_cancelled());
        update.uncancel();
        assert!(!update.is_cancelled());
        let mut decay = LeavesDecay {
            world: WORLD,
            pos: POS,
            block: DIRT,
            cancelled: false,
        };
        assert_eq!(
            (decay.world(), decay.pos(), decay.block()),
            (WORLD, POS, DIRT)
        );
        decay.cancel();
        assert!(decay.is_cancelled());
        decay.uncancel();
        assert!(!decay.is_cancelled());
    }

    // pmmp: BrewItemEvent, BrewItemEvent::getBrewingStand, BrewItemEvent::getSlot,
    // pmmp: BrewItemEvent::getInput, BrewItemEvent::getResult, BrewItemEvent::setResult,
    // pmmp: BrewItemEvent::cancel, BrewItemEvent::uncancel, BrewItemEvent::isCancelled
    #[test]
    fn brew_item_result_can_change() {
        let mut e = BrewItem {
            world: WORLD,
            pos: POS,
            block: DIRT,
            slot: 2,
            input: sticks(1),
            result: sticks(2),
            cancelled: false,
        };
        assert_eq!(
            (e.world(), e.pos(), e.block(), e.slot()),
            (WORLD, POS, DIRT, 2)
        );
        assert_eq!((e.input(), e.result()), (&sticks(1), &sticks(2)));
        e.set_result(sticks(3));
        assert_eq!(e.result(), &sticks(3));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: BrewingFuelUseEvent, BrewingFuelUseEvent::getBrewingStand,
    // pmmp: BrewingFuelUseEvent::getFuelTime, BrewingFuelUseEvent::setFuelTime,
    // pmmp: BrewingFuelUseEvent::cancel, BrewingFuelUseEvent::uncancel,
    // pmmp: BrewingFuelUseEvent::isCancelled
    #[test]
    fn brewing_fuel_lasts_at_least_one_brew() {
        let mut e = BrewingFuelUse {
            world: WORLD,
            pos: POS,
            block: DIRT,
            fuel_time: 20,
            cancelled: false,
        };
        assert_eq!(
            (e.world(), e.pos(), e.block(), e.fuel_time()),
            (WORLD, POS, DIRT, 20)
        );
        e.set_fuel_time(5);
        assert_eq!(e.fuel_time(), 5);
        e.set_fuel_time(0);
        assert_eq!(e.fuel_time(), 1);
        e.set_fuel_time(100_000);
        assert_eq!(
            e.fuel_time(),
            32_767,
            "a brewing stand saves at most a short"
        );
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: CampfireCookEvent, CampfireCookEvent::getCampfire, CampfireCookEvent::getSlot,
    // pmmp: CampfireCookEvent::getInput, CampfireCookEvent::getResult,
    // pmmp: CampfireCookEvent::setResult, CampfireCookEvent::cancel,
    // pmmp: CampfireCookEvent::uncancel, CampfireCookEvent::isCancelled
    #[test]
    fn campfire_cook_result_can_change() {
        let mut e = CampfireCook {
            world: WORLD,
            pos: POS,
            block: DIRT,
            slot: 3,
            input: sticks(1),
            result: sticks(2),
            cancelled: false,
        };
        assert_eq!(
            (e.world(), e.pos(), e.block(), e.slot()),
            (WORLD, POS, DIRT, 3)
        );
        assert_eq!((e.input(), e.result()), (&sticks(1), &sticks(2)));
        e.set_result(Stack::EMPTY);
        assert_eq!(e.result(), &Stack::EMPTY);
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: ChestPairEvent, ChestPairEvent::getLeft, ChestPairEvent::getRight,
    // pmmp: ChestPairEvent::cancel, ChestPairEvent::uncancel, ChestPairEvent::isCancelled
    #[test]
    fn chest_pair_names_both_halves() {
        let mut e = ChestPair {
            world: WORLD,
            left: POS,
            right: OTHER,
            cancelled: false,
        };
        assert_eq!((e.world(), e.left(), e.right()), (WORLD, POS, OTHER));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: FarmlandHydrationChangeEvent, FarmlandHydrationChangeEvent::getOldHydration,
    // pmmp: FarmlandHydrationChangeEvent::getNewHydration,
    // pmmp: FarmlandHydrationChangeEvent::setNewHydration,
    // pmmp: FarmlandHydrationChangeEvent::cancel, FarmlandHydrationChangeEvent::uncancel,
    // pmmp: FarmlandHydrationChangeEvent::isCancelled
    #[test]
    fn farmland_hydration_stays_in_range() {
        let mut e = FarmlandHydrationChange {
            world: WORLD,
            pos: POS,
            block: DIRT,
            old_hydration: 7,
            new_hydration: 6,
            cancelled: false,
        };
        assert_eq!((e.world(), e.pos(), e.block()), (WORLD, POS, DIRT));
        assert_eq!((e.old_hydration(), e.new_hydration()), (7, 6));
        e.set_new_hydration(3);
        assert_eq!(e.new_hydration(), 3);
        e.set_new_hydration(200);
        assert_eq!(e.new_hydration(), 7);
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: SignChangeEvent, SignChangeEvent::getPlayer, SignChangeEvent::getSign,
    // pmmp: SignChangeEvent::getOldText, SignChangeEvent::getNewText,
    // pmmp: SignChangeEvent::isFrontFace, SignChangeEvent::setNewText, SignChangeEvent::cancel,
    // pmmp: SignChangeEvent::uncancel, SignChangeEvent::isCancelled
    #[test]
    fn sign_change_names_its_side_and_its_text_can_change() {
        let lines = |s: &str| [s.to_owned(), String::new(), String::new(), String::new()];
        let mut e = SignChange {
            player: player(1),
            world: WORLD,
            pos: POS,
            block: DIRT,
            old_text: lines("old"),
            new_text: lines("new"),
            side: Side::Back,
            cancelled: false,
        };
        assert_eq!(
            (e.player(), e.world(), e.pos(), e.block()),
            (player(1), WORLD, POS, DIRT)
        );
        assert_eq!((e.old_text(), e.new_text()), (&lines("old"), &lines("new")));
        assert_eq!(e.side(), Side::Back);
        e.set_new_text(lines("other"));
        assert_eq!(e.new_text(), &lines("other"));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: StructureGrowEvent, StructureGrowEvent::getTransaction,
    // pmmp: StructureGrowEvent::getPlayer, StructureGrowEvent::cancel,
    // pmmp: StructureGrowEvent::uncancel, StructureGrowEvent::isCancelled
    #[test]
    fn structure_grow_lists_every_block_it_sets() {
        let mut e = StructureGrow {
            world: WORLD,
            pos: POS,
            block: DIRT,
            blocks: vec![(POS, GRASS_BLOCK)],
            player: None,
            cancelled: false,
        };
        assert_eq!((e.world(), e.pos(), e.block()), (WORLD, POS, DIRT));
        assert_eq!(e.blocks(), &[(POS, GRASS_BLOCK)]);
        assert_eq!(e.player(), None);
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: BlockPreExplodeEvent, BlockPreExplodeEvent::getRadius,
    // pmmp: BlockPreExplodeEvent::setRadius, BlockPreExplodeEvent::getFireChance,
    // pmmp: BlockPreExplodeEvent::setFireChance, BlockPreExplodeEvent::isIncendiary,
    // pmmp: BlockPreExplodeEvent::setIncendiary, BlockPreExplodeEvent::isBlockBreaking,
    // pmmp: BlockPreExplodeEvent::setBlockBreaking, BlockPreExplodeEvent::getPlayer,
    // pmmp: BlockPreExplodeEvent::cancel, BlockPreExplodeEvent::uncancel,
    // pmmp: BlockPreExplodeEvent::isCancelled
    #[test]
    fn block_pre_explode_keeps_its_values_in_range() {
        let mut e = BlockPreExplode {
            world: WORLD,
            pos: POS,
            block: DIRT,
            player: Some(player(1)),
            radius: 5.0,
            fire_chance: 0.0,
            block_breaking: true,
            cancelled: false,
        };
        assert_eq!((e.world(), e.pos(), e.block()), (WORLD, POS, DIRT));
        assert_eq!((e.player(), e.radius()), (Some(player(1)), 5.0));
        assert!(!e.is_incendiary() && e.is_block_breaking());
        e.set_radius(2.5);
        e.set_radius(-1.0);
        e.set_radius(f64::NAN);
        assert_eq!(e.radius(), 2.5);
        e.set_radius(100.0);
        assert_eq!(e.radius(), MAX_EXPLOSION_RADIUS);
        e.set_incendiary(true);
        assert_eq!(e.fire_chance(), DEFAULT_FIRE_CHANCE);
        e.set_fire_chance(2.0);
        assert_eq!(e.fire_chance(), 1.0);
        e.set_fire_chance(f64::NAN);
        assert_eq!(e.fire_chance(), 1.0);
        e.set_fire_chance(0.5);
        e.set_fire_chance(f64::INFINITY);
        e.set_fire_chance(f64::NEG_INFINITY);
        assert_eq!(e.fire_chance(), 0.5);
        e.set_incendiary(false);
        assert!(!e.is_incendiary());
        e.set_block_breaking(false);
        assert!(!e.is_block_breaking());
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: BlockExplodeEvent, BlockExplodeEvent::getPosition,
    // pmmp: BlockExplodeEvent::getAffectedBlocks, BlockExplodeEvent::setAffectedBlocks,
    // pmmp: BlockExplodeEvent::getIgnitions, BlockExplodeEvent::setIgnitions,
    // pmmp: BlockExplodeEvent::getYield, BlockExplodeEvent::setYield,
    // pmmp: BlockExplodeEvent::cancel, BlockExplodeEvent::uncancel,
    // pmmp: BlockExplodeEvent::isCancelled
    #[test]
    fn block_explode_blocks_and_yield_can_change() {
        let centre = Vec3::new(1.5, 64.5, -1.5);
        let mut e = BlockExplode {
            world: WORLD,
            pos: POS,
            block: DIRT,
            position: centre,
            blocks: vec![POS, OTHER],
            yield_percent: 20.0,
            ignitions: vec![OTHER],
            cancelled: false,
        };
        assert_eq!((e.world(), e.pos(), e.block()), (WORLD, POS, DIRT));
        assert_eq!(e.position(), centre);
        assert_eq!(
            (e.blocks(), e.ignitions()),
            ([POS, OTHER].as_slice(), [OTHER].as_slice())
        );
        e.set_blocks(vec![OTHER]);
        e.set_ignitions(Vec::new());
        assert_eq!(
            (e.blocks(), e.ignitions()),
            ([OTHER].as_slice(), [].as_slice())
        );
        e.set_yield_percent(150.0);
        assert_eq!(e.yield_percent(), 100.0);
        e.set_yield_percent(f64::NAN);
        assert_eq!(e.yield_percent(), 100.0);
        e.set_yield_percent(50.0);
        e.set_yield_percent(f64::INFINITY);
        e.set_yield_percent(f64::NEG_INFINITY);
        assert_eq!(e.yield_percent(), 50.0);
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }
}
