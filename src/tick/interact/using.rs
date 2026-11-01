//! Using the held item on blocks, in the air and on entities (PMMP `World::useItemOn`,
//! `Block::onInteract` of every class and trait, `Item::onInteractBlock`, `Item::onClickAir`,
//! `Player::useHeldItem`, `releaseHeldItem`, `attackEntity`, `interactEntity`).
//!
//! Redstone is out of scope: buttons, levers, repeaters and comparators only change their own
//! state. Block entity contents (signs, item frames, flower pots, jukeboxes, lecterns, campfire
//! cooking, cauldron dye) are [`super::decor`]'s; thrown items, bows, tridents, paintings, end
//! crystals, firework rockets, primed TNT and entity attacks go to the entity rules.

use tracing::debug;

use super::{
    Actor, Source, USE, adventure_allows, block_vec, breaking, center, drops, level_event, place,
    props, return_items, stream,
};
use crate::block::{self, Class, Face, Kind, State as BlockState};
use crate::config::GameMode;
use crate::event::{
    BlockPlace, BlockPreExplode, BlockTeleport, InteractAction, PlayerBucketEmpty,
    PlayerBucketFill, PlayerEntityInteract, PlayerInteract, PlayerItemUse, PlayerRespawnAnchorUse,
    RespawnAnchorAction, SignChange,
};
use crate::item::{self, Enchantment, Stack};
use crate::math::Vec3;
use crate::player;
use crate::protocol::consts::{LevelEvent as Event, UseItemOnEntityAction};
use crate::random::Random;
use crate::save::WorldIndex;
use crate::tick::block_entity::{self, BlockEntity};
use crate::tick::update::{CAULDRON_FULL, LAVA_TICK_RATE, WATER_TICK_RATE};
use crate::tick::{SessionId, State, dispatch, emit, entity, inventory, survival, trade, update};
use crate::world::{BlockPos, Entry, MAX_Y, MIN_Y, Particle, Sound};

/// Ticks a pressed button stays pressed (PMMP `StoneButton`, `WoodenButton`).
const STONE_BUTTON_TICKS: u32 = 20;
const WOODEN_BUTTON_TICKS: u32 = 30;
/// The radius of a respawn anchor's explosion (PMMP `RespawnAnchor::explode`).
const ANCHOR_RADIUS: f64 = 5.0;
/// The grey chat line of a respawn anchor that becomes the respawn point (PMMP
/// `KnownTranslationFactory::tile_respawn_anchor_respawnSet()->prefix(TextFormat::GRAY)`).
const ANCHOR_SPAWN_SET: &str = "§7%tile.respawn_anchor.respawnSet";
/// The cauldron levels a bottle holds.
const BOTTLE_LEVELS: i32 = 2;
/// The bites a cake has before it is gone (PMMP `Cake::MAX_BITES`).
const CAKE_MAX_BITES: i32 = 6;
/// The most sounds one use plays (a candle lit by a fire charge plays two).
const MAX_USE_SOUNDS: usize = 2;
/// The sixteen light block names by level (PMMP `Light`).
const LIGHT_BLOCKS: [&str; 16] = [
    "minecraft:light_block_0",
    "minecraft:light_block_1",
    "minecraft:light_block_2",
    "minecraft:light_block_3",
    "minecraft:light_block_4",
    "minecraft:light_block_5",
    "minecraft:light_block_6",
    "minecraft:light_block_7",
    "minecraft:light_block_8",
    "minecraft:light_block_9",
    "minecraft:light_block_10",
    "minecraft:light_block_11",
    "minecraft:light_block_12",
    "minecraft:light_block_13",
    "minecraft:light_block_14",
    "minecraft:light_block_15",
];

/// The result of an item's own use (PMMP `ItemUseResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Not the item's kind of use: placement comes next.
    None,
    /// Used.
    Success,
    /// Refused.
    Fail,
}

/// A use of the held item on a block. PMMP passes the item by reference and collects returned
/// items; the caller applies both afterwards (PMMP `returnItemsFromAction`), then plays the use's
/// sounds.
pub(super) struct Use {
    pub(super) actor: Actor,
    pub(super) pos: BlockPos,
    pub(super) face: Face,
    /// Where on the face the click was, 0 to 1 on each axis.
    pub(super) click: Vec3,
    /// The clicked block.
    pub(super) block: BlockState,
    /// The held item as the rules change it.
    pub(super) item: Stack,
    /// Items to give back (a filled bucket, an emptied bottle).
    pub(super) returned: Vec<Stack>,
    /// The sounds the use plays once it is applied with the held item and returned items, so that
    /// a [`WorldSound`](crate::event::WorldSound) listener never sees it half done.
    sounds: [Option<(Vec3, Sound)>; MAX_USE_SOUNDS],
    pub(super) rng: Random,
    /// The event the use raised, for its monitors once the held item is applied too.
    pub(super) watched: Option<Watched>,
    /// Whether user code (a custom block's hook, a listener) ran in the block's own use: a use
    /// that goes on to the item and placement checks the click again.
    ran_user_code: bool,
    /// Where the click comes from.
    pub(super) source: Source,
}

impl Use {
    /// Whether the item is of a PMMP item class.
    fn item_is(&self, class: item::Class) -> bool {
        self.item.item().is_some_and(|t| t.is(class))
    }

    /// Whether the item is a vanilla item kind.
    pub(super) fn item_kind(&self, kind: item::Kind) -> bool {
        self.item.item().is_some_and(|t| t.kind() == Some(kind))
    }

    /// Whether the item lights things (PMMP: flint and steel or Fire Aspect).
    fn lights(&self) -> bool {
        self.item_is(item::Class::FlintSteel)
            || drops::level(&self.item, Enchantment::FireAspect) > 0
    }

    /// The block next to the clicked face.
    fn replace(&self) -> Option<BlockPos> {
        self.pos.step(self.face)
    }

    /// Takes one item (PMMP `Item::pop`).
    pub(super) fn pop(&mut self) {
        self.item = self.item.with_count(self.item.count().saturating_sub(1));
    }

    /// Wears the item by `amount` (PMMP `Durable::applyDamage`).
    pub(super) fn wear(&mut self, settings: &crate::tick::Settings, amount: u32) {
        self.item = breaking::damage(settings, &self.item, amount, &mut self.rng);
    }

    /// Plays `sound` at `at` once the use is applied ([`finish`]).
    #[expect(
        clippy::expect_used,
        reason = "invariant: no use plays more than MAX_USE_SOUNDS sounds"
    )]
    pub(super) fn play(&mut self, at: Vec3, sound: Sound) {
        let free = self
            .sounds
            .iter_mut()
            .find(|s| s.is_none())
            .expect("a use plays at most MAX_USE_SOUNDS sounds");
        *free = Some((at, sound));
    }
}

/// An event an item use raised, kept until the use is applied with the held item and returned
/// items, so that its monitors see the outcome.
#[derive(Debug)]
pub(super) enum Watched {
    /// A bucket poured.
    BucketEmpty(PlayerBucketEmpty),
    /// A bucket filled.
    BucketFill(PlayerBucketFill),
    /// A sign dyed, lit or dimmed.
    Sign(SignChange),
}

impl Watched {
    /// Calls the event's monitors.
    fn watch(self, state: &mut State) {
        match self {
            Self::BucketEmpty(e) => dispatch::watch(state, &e),
            Self::BucketFill(e) => dispatch::watch(state, &e),
            Self::Sign(e) => dispatch::watch(state, &e),
        }
    }
}

/// Returns, after listeners ran, whether the player is still in the world of `actor`, not a
/// spectator, and holds `held`, so that a use planned before them can go on (a listener that took
/// the item or moved the player away cancels it).
pub(super) fn still_holds(state: &State, actor: &Actor, held: &Stack) -> bool {
    super::actor(state, actor.session).is_some_and(|a| {
        a.world == actor.world
            && a.mode != GameMode::Spectator
            && state
                .players
                .get(&actor.session)
                .is_some_and(|p| p.inventory.held().same(held))
    })
}

/// Drops `item` at the centre of the used block (PMMP `World::dropItem`); at the world's entity
/// limit it goes to the player instead, so nothing is lost.
fn drop_or_return(state: &mut State, u: &mut Use, item: Stack) {
    if !entity::drop_item(state, u.actor.world, center(u.pos), item.clone()) {
        u.returned.push(item);
    }
}

/// Sets a block in a world (a missing world or chunk changes nothing).
pub(super) fn set(state: &mut State, world: WorldIndex, pos: BlockPos, block: BlockState) {
    if let Some(w) = state.worlds.get_mut(world)
        && let Err(e) = w.set_block(pos, block)
    {
        debug!(?pos, error = %e, "block not set");
    }
}

/// Returns the block at `pos` (air where nothing is loaded).
fn get(state: &State, world: WorldIndex, pos: Option<BlockPos>) -> BlockState {
    state
        .worlds
        .get(world)
        .map_or(block::AIR, |w| place::block_at(w, pos))
}

/// Uses the held item on a block (PMMP `World::useItemOn` with `Player::interactBlock`'s item
/// return): the clicked block's interaction, the item's own use, then placement. False when
/// nothing happened (the caller resyncs).
pub(super) fn use_item_on(
    state: &mut State,
    actor: &Actor,
    pos: BlockPos,
    face: Face,
    click: Vec3,
    source: Source,
) -> bool {
    let Some((clicked, held)) = right_click(state, actor, pos, face) else {
        return false;
    };
    // Marked whether or not a listener hears the event, so that listeners change no state.
    let first = source == Source::Action
        || state
            .players
            .get_mut(&actor.session)
            .is_some_and(|p| p.breaking.first_of_press());
    // An inert block has no use of its own; events name it as `minecraft:info_update`.
    let inert = state
        .worlds
        .get(actor.world)
        .and_then(|w| w.entry(pos))
        .is_some_and(|e| matches!(e, Entry::Inert(..)));
    // PMMP: sneaking uses neither the block (unless the hand is empty) nor the item.
    let mut use_block = (!actor.sneak_pressed || held.is_empty()) && !inert;
    let mut use_item = !actor.sneak_pressed;
    let mut actor = *actor;
    let mut event = None;
    if dispatch::heard::<PlayerInteract>(state) {
        let world = crate::world::Id::of(actor.world);
        let mut e = PlayerInteract {
            player: player::Id::new(actor.session),
            world,
            pos,
            block: if inert { block::INFO_UPDATE } else { clicked },
            face,
            item: held.clone(),
            touch_vector: click,
            action: InteractAction::RightClickBlock,
            first,
            use_item,
            use_block,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled {
            return false;
        }
        if dispatch::listened::<PlayerInteract>(state) {
            // A listener may have moved the player, changed the block or the held item.
            let Some(again) = click_again(state, &actor, pos, face, clicked, &held) else {
                return false;
            };
            actor = again;
        }
        use_block = e.use_block;
        use_item = e.use_item;
        event = Some(e);
    }
    let mut u = Use {
        actor,
        pos,
        face,
        click,
        block: clicked,
        item: held.clone(),
        returned: Vec::new(),
        sounds: [None; MAX_USE_SOUNDS],
        rng: stream(state, actor.world, USE, pos),
        watched: None,
        ran_user_code: false,
        source,
    };
    let done = if use_block && interact(state, &mut u) {
        finish(state, u, &held);
        true
    } else {
        if u.ran_user_code {
            // A custom block's hook or a listener may have moved the player, changed the block
            // or the held item: the item's use and placement go on only from the same click.
            let Some(again) = click_again(state, &u.actor, pos, face, clicked, &held) else {
                return false;
            };
            u.actor = again;
        }
        match use_item.then(|| item_use(state, &mut u)) {
            Some(Outcome::Success) => {
                finish(state, u, &held);
                true
            }
            Some(Outcome::Fail) => false,
            Some(Outcome::None) | None => place_held(state, &u.actor, &held, pos, face, click),
        }
    };
    if let Some(e) = event.filter(|_| done) {
        dispatch::watch(state, &e);
    }
    done
}

/// Checks a right click (PMMP `World::useItemOn`'s checks): the clicked block (not air) and the
/// one next to its face are loaded and in the world, and the player is not a spectator. The
/// clicked block (an inert one as its stand-in, [`place::placed_on`]) and the held item if so.
fn right_click(
    state: &State,
    actor: &Actor,
    pos: BlockPos,
    face: Face,
) -> Option<(BlockState, Stack)> {
    let w = state.worlds.get(actor.world)?;
    let clicked = place::placed_on(w, pos)?;
    let replace = pos.step(face).filter(|p| (MIN_Y..=MAX_Y).contains(&p.y))?;
    if place::placed_on(w, replace).is_none()
        || clicked == block::AIR
        || actor.mode == GameMode::Spectator
    {
        return None;
    }
    let held = state.players.get(&actor.session)?.inventory.held().clone();
    Some((clicked, held))
}

/// Finds the player of `actor` again after user code ran in its click at `pos`: in the same
/// world, within reach, with the block it clicked (`clicked`) and the item it held (`held`) as
/// they were.
fn click_again(
    state: &State,
    actor: &Actor,
    pos: BlockPos,
    face: Face,
    clicked: BlockState,
    held: &Stack,
) -> Option<Actor> {
    super::actor(state, actor.session).filter(|a| {
        a.world == actor.world
            && a.can_interact(center(pos), a.reach())
            && right_click(state, a, pos, face).is_some_and(|(c, h)| c == clicked && h.same(held))
    })
}

/// Applies a use that worked: the held item and returned items (PMMP `returnItemsFromAction`),
/// then its sounds, then the monitors of the event it raised.
fn finish(state: &mut State, u: Use, held: &Stack) {
    let Use {
        actor,
        item,
        returned,
        sounds,
        watched,
        ..
    } = u;
    return_items(state, actor.session, held, item, returned);
    for (at, sound) in sounds.into_iter().flatten() {
        emit::sound(state, actor.world, at, sound);
    }
    if let Some(watched) = watched {
        watched.watch(state);
    }
}

/// Places the held block (the placement part of PMMP `World::useItemOn`): the blocks, the entity
/// and adventure checks ([`placement`]), [`BlockPlace`], the change, block entities, the sign
/// editor, one item used, then the sound and the custom blocks' `on_place`.
fn place_held(
    state: &mut State,
    actor: &Actor,
    held: &Stack,
    pos: BlockPos,
    face: Face,
    click: Vec3,
) -> bool {
    let world = actor.world;
    let Some(blocks) = placement(state, actor, held, pos, face, click) else {
        return false;
    };
    let mut event = None;
    if dispatch::heard::<BlockPlace>(state) {
        let mut e = BlockPlace {
            player: player::Id::new(actor.session),
            world: crate::world::Id::of(world),
            blocks: blocks.clone(),
            block_against: pos,
            item: held.clone(),
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled {
            return false;
        }
        // A listener may have changed the world, moved the player or taken the item: the
        // placement must come out the same.
        if dispatch::listened::<BlockPlace>(state)
            && !super::actor(state, actor.session).is_some_and(|a| {
                still_holds(state, actor, held)
                    && a.can_interact(center(pos), a.reach())
                    && placement(state, &a, held, pos, face, click).as_ref() == Some(&blocks)
            })
        {
            return false;
        }
        event = Some(e);
    }
    for (p, s) in &blocks {
        set(state, world, *p, *s);
    }
    for (p, s) in &blocks {
        block_entity::placed(state, world, *p, Some(actor.session), held, face);
        // PMMP `BaseSign::onPostPlace`: the placer writes the new sign.
        if s.is(Class::BaseSign) {
            super::open_sign_editor(state, actor.session, world, *p, true);
        }
        // PMMP `BaseRail::onPostPlace`: a rail joins the rails next to it.
        if s.is(Class::BaseRail) {
            let joined = state
                .worlds
                .get(world)
                .map(|w| super::rail::reconnect(w, *p))
                .unwrap_or_default();
            for (q, r) in joined {
                set(state, world, q, r);
            }
        }
    }
    let used = held.with_count(held.count().saturating_sub(1));
    return_items(state, actor.session, held, used, Vec::new());
    if let Some((first, s)) = blocks.first() {
        emit::sound(state, world, center(*first), Sound::BlockPlace(*s));
    }
    for (p, s) in &blocks {
        if s.custom_block().is_some() {
            super::custom::placed(state, Some(actor.session), world, *p, *s);
        }
    }
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
    true
}

/// Works out the blocks placing `held` against the face `face` of the block at `pos` sets (PMMP
/// `Item::getPlacementTransaction`), if adventure mode allows it, at least one block changes
/// (PMMP `BlockTransaction::apply`) and nothing is in the way. The cheap checks come first.
fn placement(
    state: &State,
    actor: &Actor,
    held: &Stack,
    pos: BlockPos,
    face: Face,
    click: Vec3,
) -> Option<place::Blocks> {
    let world = actor.world;
    let item = held.item()?;
    let w = state.worlds.get(world)?;
    // `CanPlaceOn` names the clicked block, an inert one by its own name.
    let clicked = match w.entry(pos)? {
        Entry::Known(s) => s.name(),
        Entry::Inert(s, _) => s.name(),
        Entry::Unknown(_) => return None,
    };
    if actor.mode == GameMode::Adventure && !adventure_allows(held, "CanPlaceOn", clicked) {
        return None;
    }
    let mut rng = stream(state, world, USE, pos);
    let blocks = place::placement(w, item, pos, face, click, Some(&actor.placer()), &mut rng)?;
    let changes = blocks.iter().any(|(p, s)| w.block(*p) != Some(*s));
    if !changes || blocks.iter().any(|(p, s)| obstructed(state, world, *p, *s)) {
        return None;
    }
    Some(blocks)
}

/// Returns whether an entity or player is in the way of `block` at `pos` (PMMP
/// `World::getCollidingEntities` for each collision box, [`entity::occupied`]).
fn obstructed(state: &State, world: WorldIndex, pos: BlockPos, block: BlockState) -> bool {
    let at = block_vec(pos);
    block
        .collision_boxes()
        .iter()
        .any(|b| entity::occupied(state, world, b.offset(at)))
}

/// Opens the window of the clicked block ([`inventory::open_block`]); a client tries once a tick
/// at most ([`inventory::first_client_open_this_tick`]).
fn open_window(state: &mut State, u: &Use) {
    if u.source == Source::Client && !inventory::first_client_open_this_tick(state, u.actor.session)
    {
        return;
    }
    let _ = inventory::open_block(state, u.actor.session, u.actor.world, u.pos);
}

/// Uses the clicked block (PMMP `Block::onInteract` of the first class in its chain that has
/// one): true when the block did something.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one arm per PMMP class keeps the port side by side with PMMP"
)]
fn interact(state: &mut State, u: &mut Use) -> bool {
    let block = u.block;
    // A custom block's own use; no vanilla class applies to it.
    if block.custom_block().is_some() {
        u.ran_user_code = true;
        return super::custom::used(state, u.actor.session, u.actor.world, u.pos, block);
    }
    // Bone meal on anything that grows: the block update rules decide (PMMP `Fertilizer` in
    // each class's onInteract).
    if u.item_is(item::Class::Fertilizer) && fertilizable(block) {
        if update::fertilize(
            state,
            u.actor.world,
            u.pos,
            Some(crate::player::Id::new(u.actor.session)),
        ) {
            u.pop();
            level_event(
                state,
                u.actor.world,
                block_vec(u.pos),
                Event::ParticleCropGrowth,
                0,
            );
            return true;
        }
        return false;
    }
    for class in block.kind().classes() {
        match class {
            Class::Anvil
            | Class::Barrel
            | Class::Beacon
            | Class::BrewingStand
            | Class::CartographyTable
            | Class::CraftingTable
            | Class::EnchantingTable
            | Class::EnderChest
            | Class::Furnace
            | Class::Hopper
            | Class::Loom
            | Class::ShulkerBox
            | Class::SmithingTable
            | Class::Stonecutter => {
                open_window(state, u);
                return true;
            }
            Class::Chest => {
                // A chest under a solid block stays shut (its pair's check is the window's).
                let above = get(state, u.actor.world, u.pos.step(Face::Up));
                if above.is_transparent() {
                    open_window(state, u);
                }
                return true;
            }
            // With `beds = false` a bed is a plain block.
            Class::Bed if state.settings.beds => return bed(state, u),
            Class::Button => return button(state, u),
            Class::CakeWithCandle => {
                if props::flag(block, "lit") == Some(true) && u.face != Face::Up {
                    return true;
                }
                if candle(state, u) {
                    return true;
                }
                return cake(state, u);
            }
            Class::Cake => {
                let bites = props::int(block, "bite_counter").unwrap_or(0);
                if bites == 0
                    && let Some(with) = candle_cake(&u.item)
                {
                    set(state, u.actor.world, u.pos, with);
                    u.pop();
                    return true;
                }
                return cake(state, u);
            }
            Class::BaseCake => return cake(state, u),
            Class::Campfire => {
                return campfire(state, u) || super::decor::campfire_cook(state, u);
            }
            Class::Candle => return candle(state, u),
            Class::Cauldron | Class::FillableCauldron => {
                return super::decor::cauldron_dye(state, u) || cauldron(state, u);
            }
            Class::CaveVines => {
                if block.name().ends_with("_with_berries") {
                    if let Some(item) = drops_item(block) {
                        drop_or_return(state, u, item);
                    }
                    u.play(center(u.pos), Sound::GlowBerriesPick);
                    if let Some(bare) = props::renamed(block, "minecraft:cave_vines") {
                        set(state, u.actor.world, u.pos, bare);
                    }
                    return true;
                }
                return false;
            }
            Class::CopperDoor | Class::CopperTrapdoor => {
                if u.actor.sneaking && copper(state, u) {
                    if block.is(Class::CopperDoor) {
                        copy_copper_to_other_half(state, u);
                    }
                    return true;
                }
            }
            Class::DaylightSensor => {
                let name = if block.name().ends_with("_inverted") {
                    "minecraft:daylight_detector"
                } else {
                    "minecraft:daylight_detector_inverted"
                };
                if let Some(next) = props::renamed(block, name) {
                    set(state, u.actor.world, u.pos, next);
                }
                return true;
            }
            Class::Dirt => return dirt(state, u),
            Class::Door => return door(state, u),
            Class::DragonEgg => {
                teleport_dragon_egg(state, u.actor.world, u.pos, block);
                return true;
            }
            Class::FenceGate => return fence_gate(state, u),
            Class::Grass => return grass(state, u),
            Class::Lever => {
                let on = props::flag(block, "open_bit") != Some(true);
                if let Some(next) = props::with_flag(block, "open_bit", on) {
                    set(state, u.actor.world, u.pos, next);
                }
                let switched = if on {
                    Sound::RedstonePowerOn
                } else {
                    Sound::RedstonePowerOff
                };
                u.play(center(u.pos), switched);
                return true;
            }
            Class::Light => {
                let level = block
                    .name()
                    .strip_prefix("minecraft:light_block_")
                    .and_then(|n| n.parse::<u8>().ok())
                    .unwrap_or(0);
                let next = if level >= 15 { 0 } else { level + 1 };
                if let Some(s) = LIGHT_BLOCKS
                    .get(usize::from(next))
                    .and_then(|n| BlockState::find(n, &[]))
                {
                    set(state, u.actor.world, u.pos, s);
                }
                return true;
            }
            Class::Pumpkin => {
                if u.item_is(item::Class::Shears)
                    && matches!(u.face, Face::North | Face::South | Face::West | Face::East)
                {
                    u.wear(&state.settings, 1);
                    let carved = props::with_facing(Kind::CarvedPumpkin.default_state(), u.face);
                    if let Some(carved) = carved {
                        set(state, u.actor.world, u.pos, carved);
                    }
                    let seeds = Stack::new(item::Kind::PumpkinSeeds.default_type(), 1);
                    drop_or_return(state, u, seeds);
                    return true;
                }
                return false;
            }
            Class::RedstoneComparator => {
                let subtract = props::flag(block, "output_subtract_bit") != Some(true);
                if let Some(next) = props::with_flag(block, "output_subtract_bit", subtract) {
                    set(state, u.actor.world, u.pos, next);
                }
                return true;
            }
            Class::RedstoneOre => {
                if !block.name().contains(":lit_")
                    && let Some(lit) = props::with_lit(block, true)
                {
                    set(state, u.actor.world, u.pos, lit);
                }
                // Lighting it does not stop a placement (PMMP returns false).
                return false;
            }
            Class::RedstoneRepeater => {
                // PMMP delay 1 to 4 is Bedrock `repeater_delay` 0 to 3.
                let delay = props::int(block, "repeater_delay").unwrap_or(0);
                let next = if delay >= 3 { 0 } else { delay + 1 };
                if let Some(s) = props::with_int(block, "repeater_delay", next) {
                    set(state, u.actor.world, u.pos, s);
                }
                return true;
            }
            Class::RespawnAnchor => {
                let charges = props::int(block, "respawn_anchor_charge").unwrap_or(0);
                let glowstone = item_type(Kind::Glowstone.default_state());
                if glowstone.is_some() && u.item.item() == glowstone && charges < 4 {
                    if let Some(s) = props::with_int(block, "respawn_anchor_charge", charges + 1) {
                        set(state, u.actor.world, u.pos, s);
                    }
                    u.play(center(u.pos), Sound::RespawnAnchorCharge);
                    u.pop();
                    return true;
                }
                return charges > 0 && respawn_anchor(state, u);
            }
            Class::BaseSign => return super::decor::sign(state, u),
            Class::SweetBerryBush => {
                let age = props::int(block, "growth").unwrap_or(0);
                let amount = match age {
                    3 => u.rng.range(2, 3),
                    2 => u.rng.range(1, 2),
                    _ => 0,
                };
                if amount > 0 {
                    if let Some(s) = props::with_int(block, "growth", 1) {
                        set(state, u.actor.world, u.pos, s);
                    }
                    if let Some(berries) = drops_item(block) {
                        let count = u8::try_from(amount).unwrap_or(1);
                        drop_or_return(state, u, berries.with_count(count));
                    }
                    u.play(center(u.pos), Sound::SweetBerriesPick);
                }
                return true;
            }
            Class::Tnt => {
                if u.item_kind(item::Kind::FireCharge) || u.lights() {
                    if !ignite_tnt(state, u.actor.world, u.pos) {
                        return false;
                    }
                    if u.item_kind(item::Kind::FireCharge) {
                        u.pop();
                    } else {
                        u.wear(&state.settings, 1);
                    }
                    return true;
                }
                return false;
            }
            Class::Trapdoor => {
                let open = props::flag(block, "open_bit") != Some(true);
                if let Some(s) = props::with_flag(block, "open_bit", open) {
                    set(state, u.actor.world, u.pos, s);
                }
                u.play(block_vec(u.pos), Sound::Door { pitch: 0.0 });
                return true;
            }
            Class::Wood => {
                if u.item_is(item::Class::Axe) && !block.name().contains("stripped_") {
                    let stripped = block.name().strip_prefix("minecraft:").and_then(|rest| {
                        let mut name = String::with_capacity(block.name().len() + 9);
                        name.push_str("minecraft:stripped_");
                        name.push_str(rest);
                        props::renamed(block, &name)
                    });
                    if let Some(stripped) = stripped {
                        u.wear(&state.settings, 1);
                        set(state, u.actor.world, u.pos, stripped);
                        u.play(center(u.pos), Sound::ItemUseOnBlock(stripped));
                        return true;
                    }
                }
                return false;
            }
            Class::FlowerPot => return super::decor::flower_pot(state, u),
            Class::ItemFrame => return super::decor::item_frame(state, u),
            Class::ChiseledBookshelf => return super::decor::bookshelf(state, u),
            Class::ChemistryTable => return false,
            Class::Bell => return bell_rings(u),
            Class::Jukebox => return super::decor::jukebox(state, u),
            Class::Lectern => return super::decor::lectern(state, u),
            Class::Block => {
                if block.has_trait(block::Trait::Copper) {
                    return copper(state, u);
                }
                return false;
            }
            _ => {}
        }
    }
    false
}

/// Returns whether the bell rings, hit on a side it can swing from (PMMP `Bell::onInteract`; the
/// ring itself is an effect only).
fn bell_rings(u: &Use) -> bool {
    let hit = props::horizontal_facing(u.actor.yaw).opposite();
    let facing = props::facing(u.block).unwrap_or(Face::North);
    match props::string(u.block, "attachment") {
        Some("hanging") => true,
        Some("standing") => props::Axis::of(hit) == props::Axis::of(facing),
        _ => hit == props::rotate_y(facing, false) || hit == props::rotate_y(facing, true),
    }
}

/// Returns whether bone meal grows the block: the PMMP classes whose `onInteract` has a
/// `Fertilizer` branch (the block update rules grow it).
fn fertilizable(block: BlockState) -> bool {
    // Berries come off before bone meal counts (PMMP `CaveVines`, `SweetBerryBush`).
    if (block.is(Class::CaveVines) && block.name().ends_with("_with_berries"))
        || (block.is(Class::SweetBerryBush) && props::int(block, "growth").is_some_and(|g| g >= 3))
    {
        return false;
    }
    [
        Class::Azalea,
        Class::Bamboo,
        Class::BambooSapling,
        Class::BaseBigDripleaf,
        Class::CaveVines,
        Class::CocoaBlock,
        Class::Crops,
        Class::Dirt,
        Class::DoublePitcherCrop,
        Class::GlowLichen,
        Class::Grass,
        Class::NetherFungus,
        Class::NetherVines,
        Class::Netherrack,
        Class::Nylium,
        Class::PinkPetals,
        Class::PitcherCrop,
        Class::Sapling,
        Class::SmallDripleaf,
        Class::Sugarcane,
        Class::SweetBerryBush,
        Class::TallGrass,
        Class::TorchflowerCrop,
    ]
    .into_iter()
    .any(|c| block.is(c))
}

/// Returns the item form of a block as a single stack.
fn drops_item(block: BlockState) -> Option<Stack> {
    match block.item() {
        block::ItemForm::Item(t) => Some(Stack::new(t, 1)),
        _ => None,
    }
}

/// Returns the item type of a block's item form, if it has one.
fn item_type(block: BlockState) -> Option<item::Type> {
    match block.item() {
        block::ItemForm::Item(t) => Some(t),
        _ => None,
    }
}

/// Lays the player in the bed (PMMP `Bed::onInteract`): the survival rules find the clicked bed's
/// other half (none for an incomplete bed), pick its head and check night, distance and whether
/// it is taken.
fn bed(state: &mut State, u: &Use) -> bool {
    let _ = survival::sleep(state, u.actor.session, u.actor.world, u.pos);
    true
}

/// Uses a charged respawn anchor (PMMP `RespawnAnchor::onInteract`): [`PlayerRespawnAnchorUse`],
/// then the anchor explodes (in any world, as in PMMP) or, if a listener chose so, becomes the
/// player's respawn point. False if cancelled, or if a listener took the item, moved the player
/// away or changed the anchor. Its monitors see only what was applied.
fn respawn_anchor(state: &mut State, u: &mut Use) -> bool {
    let mut action = RespawnAnchorAction::Explode;
    let mut event = None;
    if dispatch::heard::<PlayerRespawnAnchorUse>(state) {
        let mut e = PlayerRespawnAnchorUse {
            player: player::Id::new(u.actor.session),
            world: crate::world::Id::of(u.actor.world),
            pos: u.pos,
            block: u.block,
            action,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        let listened = dispatch::listened::<PlayerRespawnAnchorUse>(state);
        u.ran_user_code = listened;
        if e.cancelled
            || (listened
                && !(still_holds(state, &u.actor, &u.item)
                    && get(state, u.actor.world, Some(u.pos)) == u.block))
        {
            return false;
        }
        action = e.action;
        event = Some(e);
    }
    let applied = match action {
        RespawnAnchorAction::Explode => explode_anchor(state, u),
        RespawnAnchorAction::SetSpawn => anchor_spawn(state, u),
    };
    if let Some(e) = event.filter(|_| applied) {
        dispatch::watch(state, &e);
    }
    true
}

/// Explodes a respawn anchor (PMMP `RespawnAnchor::explode`): [`BlockPreExplode`] (radius 5,
/// fire), then the anchor goes and explodes from the centre of its block. A cancelled event, or a
/// listener that changed the anchor, keeps it: false then.
fn explode_anchor(state: &mut State, u: &Use) -> bool {
    let mut blast = entity::Blast::new(ANCHOR_RADIUS, true);
    let mut event = None;
    if dispatch::heard::<BlockPreExplode>(state) {
        let mut e = BlockPreExplode {
            world: crate::world::Id::of(u.actor.world),
            pos: u.pos,
            block: u.block,
            player: Some(player::Id::new(u.actor.session)),
            radius: blast.size,
            fire_chance: blast.fire_chance,
            block_breaking: blast.breaks,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled
            || (dispatch::listened::<BlockPreExplode>(state)
                && get(state, u.actor.world, Some(u.pos)) != u.block)
        {
            return false;
        }
        blast = entity::Blast {
            size: e.radius,
            fire_chance: e.fire_chance,
            breaks: e.block_breaking,
        };
        event = Some(e);
    }
    set(state, u.actor.world, u.pos, block::AIR);
    entity::explode_block(state, u.actor.world, u.pos, u.block, blast);
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
    true
}

/// Makes the anchor the player's respawn point with a sound and a message, unless it already is
/// (the `SetSpawn` action of PMMP `RespawnAnchor::onInteract`). False when nothing changed.
fn anchor_spawn(state: &mut State, u: &mut Use) -> bool {
    let Some(name) = state.worlds.get(u.actor.world).map(|w| w.name().to_owned()) else {
        return false;
    };
    let Some(player) = state.players.get_mut(&u.actor.session) else {
        return false;
    };
    if player.spawn() == Some((name.as_str(), u.pos)) {
        return false;
    }
    if let Err(e) = player.set_spawn(Some((&name, u.pos))) {
        debug!(error = %e, "respawn anchor spawn not set");
        return false;
    }
    let message = player::Message::Translation {
        key: ANCHOR_SPAWN_SET.to_owned(),
        parameters: Vec::new(),
    };
    if let Err(e) = player.send(message) {
        debug!(error = %e, "respawn anchor message not sent");
    }
    u.play(center(u.pos), Sound::RespawnAnchorSetSpawn);
    true
}

/// Presses a button until a scheduled update releases it (PMMP `Button::onInteract`).
fn button(state: &mut State, u: &mut Use) -> bool {
    if props::flag(u.block, "button_pressed_bit") == Some(true) {
        return true;
    }
    if let Some(pressed) = props::with_flag(u.block, "button_pressed_bit", true) {
        set(state, u.actor.world, u.pos, pressed);
        let ticks = if u.block.is(Class::WoodenButton) {
            WOODEN_BUTTON_TICKS
        } else {
            STONE_BUTTON_TICKS
        };
        update::schedule(state, u.actor.world, u.pos, ticks);
        u.play(center(u.pos), Sound::RedstonePowerOn);
    }
    true
}

/// Returns the cake with the candle the held item places (PMMP `Cake::onInteract`), if it is one.
fn candle_cake(item: &Stack) -> Option<BlockState> {
    let candle = item.item()?.block()?;
    if !candle.is(Class::Candle) {
        return None;
    }
    // `minecraft:<colour>_candle` goes on `minecraft:<colour>_candle_cake`.
    let mut name = String::with_capacity(candle.name().len() + 5);
    name.push_str(candle.name());
    name.push_str("_cake");
    BlockState::find(&name, &[("lit", block::PropertyValue::Byte(0))])
}

/// Eats a slice of cake (PMMP `BaseCake::onInteract`): the player eats it
/// (`Player::consumeObject`, the survival rules); false, and the block stays, if it cannot eat
/// now. Then `onConsume`: a cake loses a bite
/// and is gone after the last (`Cake::getResidue`); a cake with a candle becomes a cake with one
/// bite and drops its candle (`CakeWithCandle`).
fn cake(state: &mut State, u: &mut Use) -> bool {
    if !survival::eat_cake(state, u.actor.session) {
        return false;
    }
    let world = u.actor.world;
    if u.block.is(Class::CakeWithCandle) {
        if let Some(cake) = props::with_int(Kind::Cake.default_state(), "bite_counter", 1) {
            set(state, world, u.pos, cake);
        }
        for candle in drops::drops(u.block, &Stack::EMPTY, &mut u.rng).items {
            drop_or_return(state, u, candle);
        }
        return true;
    }
    let bites = props::int(u.block, "bite_counter").unwrap_or(0) + 1;
    let residue = if bites > CAKE_MAX_BITES {
        Some(block::AIR)
    } else {
        props::with_int(u.block, "bite_counter", bites)
    };
    if let Some(residue) = residue {
        set(state, world, u.pos, residue);
    }
    true
}

/// Lights a campfire with a fire charge, flint and steel or Fire Aspect, or puts it out with a
/// shovel (PMMP `Campfire::onInteract`). Cooking items is the block entity's.
fn campfire(state: &mut State, u: &mut Use) -> bool {
    let lit = props::flag(u.block, "extinguished") != Some(true);
    let world = u.actor.world;
    if !lit {
        let (at, lights) = if u.item_kind(item::Kind::FireCharge) {
            u.pop();
            (block_vec(u.pos), Sound::BlazeShoot)
        } else if u.lights() {
            u.wear(&state.settings, 1);
            (center(u.pos), Sound::FlintSteel)
        } else {
            return false;
        };
        if let Some(s) = props::with_flag(u.block, "extinguished", false) {
            set(state, world, u.pos, s);
        }
        u.play(at, lights);
        return true;
    }
    if u.item_is(item::Class::Shovel) {
        u.wear(&state.settings, 1);
        if let Some(s) = props::with_flag(u.block, "extinguished", true) {
            set(state, world, u.pos, s);
        }
        u.play(center(u.pos), Sound::FireExtinguish);
        return true;
    }
    false
}

/// Lights the unlit campfire `block` at `pos` (PMMP `Campfire::ignite`) for a burning entity
/// inside it (PMMP `Campfire::onEntityInside`).
pub(in crate::tick) fn light_campfire(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    block: BlockState,
) {
    if let Some(s) = props::with_flag(block, "extinguished", false) {
        set(state, world, pos, s);
    }
    emit::sound(state, world, center(pos), Sound::FlintSteel);
}

/// Lowers the water cauldron `block` at `pos` by a level for putting out a burning entity (PMMP
/// `WaterCauldron::onEntityInside`; level 0 is the empty cauldron).
pub(in crate::tick) fn lower_water_cauldron(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    block: BlockState,
) {
    if let Some(lower) =
        props::int(block, "fill_level").and_then(|l| props::with_int(block, "fill_level", l - 1))
    {
        set(state, world, pos, lower);
    }
}

/// Clears the dye and potion of the cauldron's block entity at `pos`, and shows it to the
/// players who see it when it had either (PMMP `Cauldron::writeStateToWorld`, and clear water
/// poured into a dyed one in `WaterCauldron::onInteract`).
pub(in crate::tick) fn clear_cauldron(state: &mut State, world: WorldIndex, pos: BlockPos) {
    // Read first: `get_mut` marks the block entities of the chunk changed.
    let tinted = matches!(
        state.rules.block_entities.get(world, pos),
        Some(BlockEntity {
            kind: block_entity::Kind::Cauldron(c),
            ..
        }) if c.colour.is_some() || c.potion != -1
    );
    if tinted
        && let Some(BlockEntity {
            kind: block_entity::Kind::Cauldron(c),
            ..
        }) = state.rules.block_entities.get_mut(world, pos)
    {
        std::mem::take(c);
        block_entity::mark_spawn(state, world, pos);
    }
}

/// Lights a candle by fire or puts it out by an empty hand (PMMP `CandleTrait::onInteract`).
fn candle(state: &mut State, u: &mut Use) -> bool {
    let lit = props::flag(u.block, "lit") == Some(true);
    let world = u.actor.world;
    if u.item_kind(item::Kind::FireCharge) || u.lights() {
        if lit {
            return true;
        }
        if u.item.item().and_then(item::Type::max_durability).is_some() {
            u.wear(&state.settings, 1);
        } else if u.item_kind(item::Kind::FireCharge) {
            u.pop();
            u.play(block_vec(u.pos), Sound::BlazeShoot);
        }
        u.play(center(u.pos), Sound::FlintSteel);
        if let Some(s) = props::with_flag(u.block, "lit", true) {
            set(state, world, u.pos, s);
        }
        return true;
    }
    if u.item.is_empty() {
        if !lit {
            return true;
        }
        u.play(center(u.pos), Sound::FireExtinguish);
        if let Some(s) = props::with_flag(u.block, "lit", false) {
            set(state, world, u.pos, s);
        }
        return true;
    }
    // Not handled: a candle in hand is placed (PMMP returns false on purpose).
    false
}

/// Fills a cauldron with buckets and water bottles, empties it with buckets and bottles, or mixes
/// liquids (PMMP `Cauldron`, `WaterCauldron` and `LavaCauldron` `onInteract`). Clear water
/// poured in and an emptied cauldron lose their dye and potion ([`clear_cauldron`]). A dye is
/// [`super::decor::cauldron_dye`]'s; putting a potion in or taking it out (PMMP
/// `PotionCauldron::onInteract`) and washing items are not handled.
fn cauldron(state: &mut State, u: &mut Use) -> bool {
    let level = props::int(u.block, "fill_level").unwrap_or(0);
    let lava = props::string(u.block, "cauldron_liquid") == Some("lava");
    let any_potion = u.item.item().is_some_and(|t| t.potion().is_some());
    let water_potion = u.item.item().is_some_and(|t| {
        (t.is(item::Class::Potion) || t.is(item::Class::SplashPotion))
            && t.potion() == item::Kind::Potion.default_type().potion()
    });
    let world = u.actor.world;
    let with = |liquid: &str, level: i32| {
        props::with_string(Kind::Cauldron.default_state(), "cauldron_liquid", liquid)
            .and_then(|s| props::with_int(s, "fill_level", level))
    };
    let bucket = Stack::new(item::Kind::Bucket.default_type(), 1);
    let bottle = Stack::new(item::Kind::GlassBottle.default_type(), 1);
    // PMMP `FillableCauldron::addFillLevels`, `removeFillLevels`: the liquid's fill or empty
    // sound; `mix` has none in PMMP (a fizz here).
    let mixed = Sound::Fizz { pitch: 0.0 };
    let (level_after, result, returned, played) = if level == 0 {
        if u.item_kind(item::Kind::WaterBucket) {
            let result = with("water", CAULDRON_FULL);
            (CAULDRON_FULL, result, bucket, Sound::CauldronFillWater)
        } else if u.item_kind(item::Kind::LavaBucket) {
            let result = with("lava", CAULDRON_FULL);
            (CAULDRON_FULL, result, bucket, Sound::CauldronFillLava)
        } else if water_potion {
            let result = with("water", BOTTLE_LEVELS);
            (BOTTLE_LEVELS, result, bottle, Sound::CauldronFillWater)
        } else {
            return true;
        }
    } else if lava {
        if u.item_kind(item::Kind::Bucket) && level >= CAULDRON_FULL {
            let lava_bucket = Stack::new(item::Kind::LavaBucket.default_type(), 1);
            (0, with("water", 0), lava_bucket, Sound::CauldronEmptyLava)
        } else if u.item_kind(item::Kind::WaterBucket) || any_potion {
            // Mixing empties the cauldron.
            let back = if any_potion { bottle } else { bucket };
            (0, with("water", 0), back, mixed)
        } else {
            return true;
        }
    } else if u.item_kind(item::Kind::WaterBucket) && level < CAULDRON_FULL {
        let result = with("water", CAULDRON_FULL);
        (CAULDRON_FULL, result, bucket, Sound::CauldronFillWater)
    } else if u.item_kind(item::Kind::Bucket) && level >= CAULDRON_FULL {
        let water_bucket = Stack::new(item::Kind::WaterBucket.default_type(), 1);
        (0, with("water", 0), water_bucket, Sound::CauldronEmptyWater)
    } else if u.item_kind(item::Kind::GlassBottle) && level >= BOTTLE_LEVELS {
        let potion = Stack::new(item::Kind::Potion.default_type(), 1);
        let next = level - BOTTLE_LEVELS;
        (next, with("water", next), potion, Sound::CauldronEmptyWater)
    } else if water_potion && level < CAULDRON_FULL {
        let next = (level + BOTTLE_LEVELS).min(CAULDRON_FULL);
        (next, with("water", next), bottle, Sound::CauldronFillWater)
    } else if u.item_kind(item::Kind::LavaBucket) {
        (0, with("water", 0), bucket, mixed)
    } else if any_potion && !water_potion {
        // Another potion mixes with the water (PMMP `WaterCauldron::onInteract`).
        (0, with("water", 0), bottle, mixed)
    } else {
        return true;
    };
    if let Some(result) = result {
        set(state, world, u.pos, result);
        if level_after == 0 || u.item_kind(item::Kind::WaterBucket) || water_potion {
            clear_cauldron(state, world, u.pos);
        }
        u.play(center(u.pos), played);
        u.pop();
        u.returned.push(returned);
    }
    true
}

/// Waxes copper with honeycomb, or takes the wax off or scrapes one stage of oxidation with an
/// axe (PMMP `CopperTrait::onInteract`).
fn copper(state: &mut State, u: &mut Use) -> bool {
    let block = u.block;
    let Some((oxidation, waxed, base)) = block::copper_parts(block) else {
        return false;
    };
    let renamed =
        |oxidation, waxed| props::renamed(block, &block::copper_name(oxidation, waxed, base));
    let world = u.actor.world;
    if !waxed && u.item_kind(item::Kind::Honeycomb) {
        if let Some(s) = renamed(oxidation, true) {
            set(state, world, u.pos, s);
            u.play(block_vec(u.pos), Sound::CopperWaxApply);
            u.pop();
            return true;
        }
        return false;
    }
    if u.item_is(item::Class::Axe) {
        let (next, played) = if waxed {
            (renamed(oxidation, false), Sound::CopperWaxRemove)
        } else {
            let Some(previous) = oxidation.previous() else {
                return false;
            };
            (renamed(previous, false), Sound::Scrape)
        };
        if let Some(next) = next {
            set(state, world, u.pos, next);
            u.play(block_vec(u.pos), played);
            u.wear(&state.settings, 1);
            return true;
        }
    }
    false
}

/// Gives the other half of a copper door its new copper state (PMMP `CopperDoor::onInteract`).
fn copy_copper_to_other_half(state: &mut State, u: &Use) {
    let world = u.actor.world;
    let Some(now) = state.worlds.get(world).and_then(|w| w.block(u.pos)) else {
        return;
    };
    let top = props::flag(now, "upper_block_bit") == Some(true);
    let other_pos = u.pos.step(if top { Face::Down } else { Face::Up });
    let other = get(state, world, other_pos);
    if let (Some(p), true) = (other_pos, other.is(Class::CopperDoor)) {
        let copied = props::renamed(other, now.name()).unwrap_or(other);
        set(state, world, p, copied);
    }
}

/// Tills dirt with a hoe (coarse dirt becomes dirt, rooted dirt drops its roots) or makes mud
/// with a water bottle (PMMP `Dirt::onInteract`).
fn dirt(state: &mut State, u: &mut Use) -> bool {
    let world = u.actor.world;
    let name = u.block.name();
    if u.face != Face::Down && u.item_is(item::Class::Hoe) {
        let above = get(state, world, u.pos.step(Face::Up));
        if above != block::AIR {
            return true;
        }
        u.wear(&state.settings, 1);
        let farmland = name == "minecraft:dirt";
        let next = if farmland {
            Kind::Farmland.default_state()
        } else {
            Kind::Dirt.default_state()
        };
        u.play(center(u.pos), Sound::ItemUseOnBlock(next));
        set(state, world, u.pos, next);
        if name == "minecraft:dirt_with_roots"
            && let Some(roots) = drops_item(Kind::HangingRoots.default_state())
        {
            drop_or_return(state, u, roots);
        }
        return true;
    }
    let water = u.item.item().is_some_and(|t| {
        (t.is(item::Class::Potion) || t.is(item::Class::SplashPotion))
            && t.potion() == item::Kind::Potion.default_type().potion()
    });
    if water {
        u.pop();
        set(state, world, u.pos, Kind::Mud.default_state());
        u.play(center(u.pos), Sound::WaterSplash { volume: 0.5 });
        return true;
    }
    false
}

/// Opens or closes both halves of a door (PMMP `Door::onInteract`).
fn door(state: &mut State, u: &mut Use) -> bool {
    let world = u.actor.world;
    let open = props::flag(u.block, "open_bit") != Some(true);
    let top = props::flag(u.block, "upper_block_bit") == Some(true);
    let other_pos = u.pos.step(if top { Face::Down } else { Face::Up });
    let other = get(state, world, other_pos);
    if let Some(p) = other_pos
        && other.kind() == u.block.kind()
        && let Some(s) = props::with_flag(other, "open_bit", open)
    {
        set(state, world, p, s);
    }
    if let Some(s) = props::with_flag(u.block, "open_bit", open) {
        set(state, world, u.pos, s);
    }
    u.play(block_vec(u.pos), Sound::Door { pitch: 0.0 });
    true
}

/// Opens a fence gate away from the player, or closes it (PMMP `FenceGate::onInteract`).
fn fence_gate(state: &mut State, u: &mut Use) -> bool {
    let world = u.actor.world;
    let open = props::flag(u.block, "open_bit") != Some(true);
    let mut next = props::with_flag(u.block, "open_bit", open).unwrap_or(u.block);
    if open {
        let player = props::horizontal_facing(u.actor.yaw);
        if props::facing(u.block) == Some(player.opposite()) {
            next = props::with_facing(next, player).unwrap_or(next);
        }
    }
    set(state, world, u.pos, next);
    u.play(block_vec(u.pos), Sound::Door { pitch: 0.0 });
    true
}

/// Makes farmland of grass with a hoe or a path with a shovel, with air above (PMMP
/// `Grass::onInteract`).
fn grass(state: &mut State, u: &mut Use) -> bool {
    let world = u.actor.world;
    if get(state, world, u.pos.step(Face::Up)) != block::AIR || u.face == Face::Down {
        return false;
    }
    let next = if u.item_is(item::Class::Hoe) {
        Kind::Farmland.default_state()
    } else if u.item_is(item::Class::Shovel) {
        Kind::GrassPath.default_state()
    } else {
        return false;
    };
    u.wear(&state.settings, 1);
    u.play(center(u.pos), Sound::ItemUseOnBlock(next));
    set(state, world, u.pos, next);
    true
}

/// Returns which side of a sign the player at `feet` faces (PMMP `BaseSign::interactsFront`, by
/// the sign's facing direction instead of `atan2`): true for the front.
pub(super) fn sign_front(block: BlockState, feet: Vec3, pos: BlockPos) -> bool {
    let facing = if let Some(rotation) = props::int(block, props::ROTATION)
        .filter(|_| props::facing(block).is_none() || block.is(Class::CeilingCenterHangingSign))
    {
        // Sixteenths of a turn from south, clockwise seen from above.
        let angle = crate::math::radians(f64::from(rotation) * 22.5);
        Vec3::new(-crate::math::sin(angle), 0.0, crate::math::cos(angle))
    } else {
        let (dx, _, dz) = props::facing(block).unwrap_or(Face::South).offset();
        Vec3::new(f64::from(dx), 0.0, f64::from(dz))
    };
    let to_player = feet - center(pos);
    facing.x * to_player.x + facing.z * to_player.z >= 0.0
}

/// Uses the held item on the block when the block did nothing (PMMP `Item::onInteractBlock` of
/// the item's class).
fn item_use(state: &mut State, u: &mut Use) -> Outcome {
    let world = u.actor.world;
    let Some(item) = u.item.item() else {
        return Outcome::None;
    };
    let replace_pos = u.replace();
    let replace = get(state, world, replace_pos);
    if item.is(item::Class::LiquidBucket) {
        let (liquid, rate, empty_sound) = if u.item_kind(item::Kind::LavaBucket) {
            (Kind::Lava, LAVA_TICK_RATE, Sound::BucketEmptyLava)
        } else if u.item_kind(item::Kind::WaterBucket) {
            (Kind::Water, WATER_TICK_RATE, Sound::BucketEmptyWater)
        } else {
            // Fish and powder snow buckets are not PMMP liquids.
            return Outcome::None;
        };
        let Some(pos) = replace_pos else {
            return Outcome::None;
        };
        if !replace.can_be_replaced() {
            return Outcome::None;
        }
        // PMMP `Liquid::getFlowingForm`: the flowing form at full level.
        let Some(flowing) = flowing(liquid) else {
            return Outcome::Fail;
        };
        let Some(returned) = bucket_empty(state, u, pos, replace) else {
            return Outcome::Fail;
        };
        set(state, world, pos, flowing);
        update::schedule(state, world, pos, rate);
        u.play(center(pos), empty_sound);
        u.pop();
        if !returned.is_empty() {
            u.returned.push(returned);
        }
        return Outcome::Success;
    }
    if u.item_kind(item::Kind::Bucket) {
        let block = u.block;
        let source = block.is(Class::Liquid) && entity::liquid_depth(block) == Some((0, false));
        if !source {
            return Outcome::None;
        }
        let (filled, fill_sound) = match block.kind() {
            Kind::Water => (item::Kind::WaterBucket, Sound::BucketFillWater),
            Kind::Lava => (item::Kind::LavaBucket, Sound::BucketFillLava),
            _ => return Outcome::Fail,
        };
        let Some(returned) = bucket_fill(state, u, Stack::new(filled.default_type(), 1)) else {
            return Outcome::Fail;
        };
        set(state, world, u.pos, block::AIR);
        u.play(center(u.pos), fill_sound);
        u.pop();
        if !returned.is_empty() {
            u.returned.push(returned);
        }
        return Outcome::Success;
    }
    if u.item_kind(item::Kind::GlassBottle) {
        if u.block.kind() == Kind::Water {
            u.pop();
            u.returned
                .push(Stack::new(item::Kind::Potion.default_type(), 1));
            return Outcome::Success;
        }
        return Outcome::None;
    }
    if item.is(item::Class::FlintSteel) || u.item_kind(item::Kind::FireCharge) {
        let Some(pos) = replace_pos else {
            return Outcome::None;
        };
        if replace != block::AIR {
            return Outcome::None;
        }
        set(state, world, pos, Kind::Fire.default_state());
        if u.item_kind(item::Kind::FireCharge) {
            u.play(center(pos), Sound::BlazeShoot);
            u.pop();
        } else {
            u.play(center(pos), Sound::FlintSteel);
            u.wear(&state.settings, 1);
        }
        return Outcome::Success;
    }
    entity_use(state, u, item)
}

/// Makes the entity of an item that makes one, and uses one up (PMMP `PaintingItem`,
/// `item\EndCrystal`, `item\FireworkRocket` and `SpawnEgg::onInteractBlock`; the entity rules).
/// A spawn egg's entity stands in the middle of the block next to the clicked face, turned at
/// random.
fn entity_use(state: &mut State, u: &mut Use, item: item::Type) -> Outcome {
    let world = u.actor.world;
    let placed = if item.is(item::Class::PaintingItem) {
        entity::place_painting(state, world, u.pos, u.face, &mut u.rng)
    } else if item.is(item::Class::EndCrystal) {
        entity::place_end_crystal(state, world, u.pos)
    } else if item.is(item::Class::SpawnEgg) {
        let Some(at) = u.replace().map(|p| block_vec(p) + Vec3::new(0.5, 0.0, 0.5)) else {
            return Outcome::None;
        };
        let yaw = u.rng.unit() * 360.0;
        entity::hatch(state, world, at, yaw, &u.item)
    } else if item.is(item::Class::FireworkRocket) {
        // PMMP: the clicked point, 0.15 out of the clicked face.
        let (dx, dy, dz) = u.face.offset();
        let out = Vec3::new(f64::from(dx), f64::from(dy), f64::from(dz)) * 0.15;
        let at = block_vec(u.pos) + u.click + out;
        if entity::launch_firework(state, u.actor.session, world, at, &u.item, &mut u.rng) {
            entity::Placed::Made
        } else {
            entity::Placed::Full
        }
    } else {
        return Outcome::None;
    };
    match placed {
        entity::Placed::Nothing => Outcome::None,
        entity::Placed::Made => {
            u.pop();
            Outcome::Success
        }
        entity::Placed::Full => Outcome::Fail,
    }
}

/// Raises [`PlayerBucketEmpty`] for pouring the held bucket at `pos` (now `replace`): what the
/// player gets back (an empty bucket unless a listener changed it), or `None` if cancelled or a
/// listener changed the place, the clicked block or the held bucket.
fn bucket_empty(
    state: &mut State,
    u: &mut Use,
    pos: BlockPos,
    replace: BlockState,
) -> Option<Stack> {
    let returned = Stack::new(item::Kind::Bucket.default_type(), 1);
    if !dispatch::heard::<PlayerBucketEmpty>(state) {
        return Some(returned);
    }
    let world = u.actor.world;
    let mut e = PlayerBucketEmpty {
        player: player::Id::new(u.actor.session),
        world: crate::world::Id::of(world),
        block_clicked: u.pos,
        block_face: u.face,
        bucket: u.item.clone(),
        item: returned,
        cancelled: false,
    };
    dispatch::call(state, &mut e);
    let changed = || {
        !still_holds(state, &u.actor, &u.item)
            || get(state, world, Some(pos)) != replace
            || get(state, world, Some(u.pos)) != u.block
    };
    if e.cancelled || (dispatch::listened::<PlayerBucketEmpty>(state) && changed()) {
        return None;
    }
    let returned = e.item.clone();
    u.watched = Some(Watched::BucketEmpty(e));
    Some(returned)
}

/// Raises [`PlayerBucketFill`] for filling the held bucket from the clicked source: what the
/// player gets back (`filled` unless a listener changed it), or `None` if cancelled or a listener
/// changed the source or the held bucket.
fn bucket_fill(state: &mut State, u: &mut Use, filled: Stack) -> Option<Stack> {
    if !dispatch::heard::<PlayerBucketFill>(state) {
        return Some(filled);
    }
    let world = u.actor.world;
    let mut e = PlayerBucketFill {
        player: player::Id::new(u.actor.session),
        world: crate::world::Id::of(world),
        block_clicked: u.pos,
        block_face: u.face,
        bucket: u.item.clone(),
        item: filled,
        cancelled: false,
    };
    dispatch::call(state, &mut e);
    let changed =
        || !still_holds(state, &u.actor, &u.item) || get(state, world, Some(u.pos)) != u.block;
    if e.cancelled || (dispatch::listened::<PlayerBucketFill>(state) && changed()) {
        return None;
    }
    let returned = e.item.clone();
    u.watched = Some(Watched::BucketFill(e));
    Some(returned)
}

/// Returns the flowing form of a liquid at full level (Bedrock `flowing_*` with depth 0).
fn flowing(liquid: Kind) -> Option<BlockState> {
    let name = if liquid == Kind::Lava {
        "minecraft:flowing_lava"
    } else {
        "minecraft:flowing_water"
    };
    BlockState::find(name, &[("liquid_depth", block::PropertyValue::Int(0))])
}

/// Uses the held item in the air (PMMP `Player::useHeldItem`): refused while its cooldown runs;
/// a consumable starts being eaten or drunk (the survival rules'); any other use that works
/// starts the cooldown again (ender pearls, goat horns).
pub(super) fn use_held_item(state: &mut State, actor: &Actor) -> bool {
    let Some(held) = usable(state, actor) else {
        return false;
    };
    let mut actor = *actor;
    let mut event = None;
    if dispatch::heard::<PlayerItemUse>(state) {
        let mut e = PlayerItemUse {
            player: player::Id::new(actor.session),
            item: held.clone(),
            direction_vector: actor.look,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled {
            return false;
        }
        if dispatch::listened::<PlayerItemUse>(state) {
            // A listener may have taken the item, moved the player or started its cooldown.
            let again = super::actor(state, actor.session)
                .filter(|a| a.world == actor.world && usable(state, a).as_ref() == Some(&held));
            let Some(again) = again else {
                return false;
            };
            actor = again;
        }
        event = Some(e);
    }
    let Some(item) = held.item() else {
        return false;
    };
    // A consumable has no air use or cooldown of its own (PMMP `setUsingItem` starts it).
    if !survival::start_eating(state, actor.session) {
        if !click_air(state, &actor, &held, item) {
            return false;
        }
        survival::reset_item_cooldown(state, actor.session, item);
    }
    if let Some(e) = event {
        dispatch::watch(state, &e);
    }
    true
}

/// Returns the held item, if the player may use it in the air now (PMMP `Player::useHeldItem`:
/// not a spectator, the item's cooldown over).
fn usable(state: &State, actor: &Actor) -> Option<Stack> {
    if actor.mode == GameMode::Spectator {
        return None;
    }
    let player = state.players.get(&actor.session)?;
    let held = player.inventory.held();
    let item = held.item()?;
    (!survival::has_item_cooldown(state, player, item)).then(|| held.clone())
}

/// Applies the held item's own use in the air (PMMP `Item::onClickAir`): armour goes on, a goat
/// horn sounds, a projectile is thrown (one used up with finite resources), a bow starts drawing.
fn click_air(state: &mut State, actor: &Actor, held: &Stack, item: item::Type) -> bool {
    let Some(player) = state.players.get(&actor.session) else {
        return false;
    };
    if let Some(armor) = item.armor() {
        // PMMP `Armor::onClickAir`: swap with what is worn there.
        let slot = armor.slot().index();
        let worn = player
            .inventory
            .armor()
            .get(slot)
            .cloned()
            .unwrap_or_default();
        let one = held.with_count(1);
        let rest = held.with_count(held.count().saturating_sub(1));
        if let Some(player) = state.players.get_mut(&actor.session) {
            inventory::set_armor(player, slot, one);
            inventory::set_held(player, worn);
        }
        if !rest.is_empty() {
            return_items(state, actor.session, held, held.clone(), vec![rest]);
        }
        emit::sound(state, actor.world, actor.eye, Sound::ArmorEquipGeneric);
        return true;
    }
    if item.kind() == Some(item::Kind::GoatHorn) {
        // PMMP `GoatHorn::onClickAir`: the call of the horn's type; the horn stays raised
        // (`canStartUsingItem`).
        start_using(state, actor.session);
        emit::sound(state, actor.world, actor.eye, Sound::GoatHorn(item));
        return true;
    }
    if item.kind() == Some(item::Kind::Spyglass) {
        // PMMP `Spyglass::canStartUsingItem`: looking through it is a use.
        start_using(state, actor.session);
        return true;
    }
    // PMMP `ProjectileItem::onClickAir`: the entity rules throw it; one is used up.
    if item.throw_force().is_some() {
        if !entity::throw(state, actor.session, held) {
            return false;
        }
        let used = held.with_count(held.count().saturating_sub(1));
        return_items(state, actor.session, held, used, Vec::new());
        return true;
    }
    // PMMP `item\Trident::canStartUsingItem`: drawing starts while one more throw does not break
    // it; the release throws.
    if item.is(item::Class::Trident) {
        let whole = item
            .max_durability()
            .is_none_or(|max| held.damage() < max.saturating_sub(1));
        if whole {
            start_using(state, actor.session);
        }
        return true;
    }
    // PMMP `Bow::canStartUsingItem`: drawing starts with an arrow to shoot (or infinite
    // resources); the release shoots.
    if item.is(item::Class::Bow) {
        if !actor.finite() || inventory::has_arrow(player) {
            start_using(state, actor.session);
        }
        return true;
    }
    false
}

/// Starts the use of the held item now (PMMP `Player::setUsingItem(true)`).
fn start_using(state: &mut State, session: SessionId) {
    let tick = state.tick;
    if let Some(player) = state.players.get_mut(&session) {
        player.breaking.using_since = Some(tick);
    }
}

/// Releases the held item (PMMP `Player::releaseHeldItem`): a drawn bow shoots and a drawn
/// trident is thrown (the entity
/// rules) with the ticks it was drawn; a thrown trident leaves the hand of a player with finite
/// items (PMMP `returnItemsFromAction`).
pub(super) fn release(state: &mut State, session: SessionId) {
    let tick = state.tick;
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let Some(since) = player.breaking.using_since.take() else {
        return;
    };
    let held = player.inventory.held().clone();
    let used = u32::try_from(tick.saturating_sub(since)).unwrap_or(u32::MAX);
    if !held.item().is_some_and(|t| t.is(item::Class::Trident)) {
        let _shot = entity::release_bow(state, session, used);
        return;
    }
    if entity::release_trident(state, session, used)
        && super::actor(state, session).is_some_and(|a| a.finite())
        && let Some(player) = state.players.get_mut(&session)
        && player.inventory.held().same(&held)
    {
        inventory::set_held(player, held.with_count(held.count().saturating_sub(1)));
    }
}

/// Applies `UseItemOnEntity`: attacking another player is the survival rules' melee (PMMP
/// `attackEntity`); attacking or using another entity (trading with a villager) is the entity
/// rules'.
pub(super) fn on_entity(
    state: &mut State,
    session: SessionId,
    runtime_id: u64,
    action: UseItemOnEntityAction,
    click: Vec3,
) {
    if let Some(&target) = state.runtime_ids.get(&runtime_id) {
        if target != session && action == UseItemOnEntityAction::Attack {
            survival::attack_player(state, session, target);
        }
        return;
    }
    let Some(target) = entity::find(state, runtime_id) else {
        return;
    };
    match action {
        UseItemOnEntityAction::Attack => entity::attack(state, session, target),
        UseItemOnEntityAction::Interact => {
            let _used = interact_entity(state, session, target, click, Source::Client);
        }
    }
}

/// Uses an entity (PMMP `Player::interactEntity`): when it is within reach,
/// [`PlayerEntityInteract`], then the entity rules' interaction (which check the reach again; a
/// client tries to open trading once a tick at most, [`trade::open_from_client`]). False if
/// nothing happened. Each part of
/// `click` that is not a finite number counts as 0.
pub(super) fn interact_entity(
    state: &mut State,
    session: SessionId,
    target: crate::entity::Id,
    click: Vec3,
    source: Source,
) -> bool {
    let click = super::finite_vec(click);
    let mut event = None;
    if dispatch::heard::<PlayerEntityInteract>(state) {
        // Out of reach or gone: nothing happens, so the event is not raised.
        if !entity::reaches(state, session, target) {
            return false;
        }
        let mut e = PlayerEntityInteract {
            player: player::Id::new(session),
            entity: target,
            click_position: click,
            cancelled: false,
        };
        dispatch::call(state, &mut e);
        if e.cancelled {
            return false;
        }
        event = Some(e);
    }
    // The entity rules check again whether the entity is there and within reach.
    let open_trade = match source {
        Source::Client => trade::open_from_client,
        Source::Action => trade::open,
    };
    let used = entity::interact(state, session, target, open_trade);
    if let Some(e) = event.filter(|_| used) {
        dispatch::watch(state, &e);
    }
    used
}

/// Teleports a dragon egg (PMMP `DragonEgg::teleport`): up to 16 tries at a random air block
/// within 16 blocks across and 8 up or down, then [`BlockTeleport`] (cancelled, the egg stays).
/// The trail shows once the egg moved.
pub(super) fn teleport_dragon_egg(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    egg: BlockState,
) {
    let mut rng = stream(state, world, USE, pos);
    let air = |state: &State, to: BlockPos| {
        (MIN_Y..=MAX_Y).contains(&to.y)
            && state.worlds.get(world).and_then(|w| w.block(to)) == Some(block::AIR)
    };
    for _ in 0..16 {
        let mut to = BlockPos::new(
            pos.x.saturating_add(rng.range(-16, 16)),
            pos.y.saturating_add(rng.range(-8, 8)).clamp(MIN_Y, MAX_Y),
            pos.z.saturating_add(rng.range(-16, 16)),
        );
        if !air(state, to) {
            continue;
        }
        let mut event = None;
        if dispatch::heard::<BlockTeleport>(state) {
            let mut e = BlockTeleport {
                world: crate::world::Id::of(world),
                pos,
                block: egg,
                to,
                cancelled: false,
            };
            dispatch::call(state, &mut e);
            // A listener may have moved the egg, filled the target or set another one.
            let changed = dispatch::listened::<BlockTeleport>(state)
                && (state.worlds.get(world).and_then(|w| w.block(pos)) != Some(egg)
                    || !air(state, e.to));
            if e.cancelled || changed {
                return;
            }
            to = e.to;
            event = Some(e);
        }
        set(state, world, pos, block::AIR);
        set(state, world, to, egg);
        // PMMP `DragonEggTeleportParticle` of the distance from the new place to the old.
        let axis = |from: i32, to: i32| {
            let d = from.saturating_sub(to).clamp(-255, 255);
            i16::try_from(d).unwrap_or(0)
        };
        let trail = Particle::DragonEggTeleport {
            x: axis(pos.x, to.x),
            y: axis(pos.y, to.y),
            z: axis(pos.z, to.z),
        };
        emit::particle(state, world, block_vec(pos), trail);
        if let Some(e) = event {
            dispatch::watch(state, &e);
        }
        return;
    }
}

/// Ignites TNT (PMMP `TNT::ignite`): the block becomes a primed TNT entity (the entity rules);
/// false at the entity limit, and the block stays.
pub(super) fn ignite_tnt(state: &mut State, world: WorldIndex, pos: BlockPos) -> bool {
    entity::ignite(state, world, pos, u32::from(entity::TNT_FUSE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copper_names_split_and_join_back() {
        for state in BlockState::all().filter(|s| s.has_trait(block::Trait::Copper)) {
            let (oxidation, waxed, base) = block::copper_parts(state).unwrap();
            let copper = |waxed| props::renamed(state, &block::copper_name(oxidation, waxed, base));
            assert_eq!(copper(waxed), Some(state), "{state:?}");
            if !waxed {
                assert!(copper(true).is_some(), "{state:?}");
            }
        }
    }

    #[test]
    fn the_default_potion_is_water_and_flowing_liquids_exist() {
        assert_eq!(
            item::Kind::Potion.default_type().potion(),
            Some(crate::effect::Potion::Water)
        );
        assert!(flowing(Kind::Water).is_some());
        assert!(flowing(Kind::Lava).is_some());
        assert!(
            LIGHT_BLOCKS
                .iter()
                .all(|n| BlockState::find(n, &[]).is_some())
        );
    }
}
