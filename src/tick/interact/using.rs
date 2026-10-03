//! Using the held item on blocks, in the air and on entities (PMMP `World::useItemOn`,
//! `Block::onInteract` of every class and trait, `Item::onInteractBlock`, `Item::onClickAir`,
//! `Player::useHeldItem`, `releaseHeldItem`, `attackEntity`, `interactEntity`).
//!
//! Redstone is out of scope (CLAUDE.md 1): buttons, levers, repeaters and comparators only change
//! their own state. Block entity contents (signs, item frames, flower pots, jukeboxes, lecterns,
//! campfire cooking, cauldron dye) are [`super::decor`]'s; thrown items, bows, primed TNT and
//! entity attacks go to the entity rules.

use crate::block::{self, Class, Face, Kind, State as BlockState};
use crate::config::GameMode;
use crate::item::{self, Enchantment, Stack};
use crate::math::{Aabb, Vec3};
use crate::protocol::consts::{LevelEvent as Event, SoundEvent, UseItemOnEntityAction};
use crate::random::Random;
use crate::save::WorldIndex;
use crate::world::{BlockPos, MAX_Y, MIN_Y};

use super::super::{SessionId, State, block_entity, entity, inventory, survival, update};
use super::{
    Actor, EYE_HEIGHT, USE, adventure_allows, block_vec, breaking, center, drops, level_event,
    place, props, return_items, sign, sound, stream,
};

/// Water's and lava's scheduled update delays (PMMP `Water::tickRate`, `Lava::tickRate` outside
/// the nether).
const WATER_TICK_RATE: u32 = 5;
const LAVA_TICK_RATE: u32 = 30;
/// Ticks a pressed button stays pressed (PMMP `StoneButton`, `WoodenButton`).
const STONE_BUTTON_TICKS: u32 = 20;
const WOODEN_BUTTON_TICKS: u32 = 30;
/// PMMP `FillableCauldron::MAX_FILL_LEVEL` and the levels a bottle holds.
const CAULDRON_FULL: i32 = 6;
const BOTTLE_LEVELS: i32 = 2;

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
/// items; the caller applies both afterwards (PMMP `returnItemsFromAction`).
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
    pub(super) rng: Random,
}

impl Use {
    /// One item less (PMMP `Item::pop`).
    pub(super) fn pop(&mut self) {
        self.item = self.item.with_count(self.item.count().saturating_sub(1));
    }

    /// The item worn by `amount` (PMMP `Durable::applyDamage`).
    pub(super) fn wear(&mut self, amount: u32) {
        self.item = breaking::damage(&self.item, amount, &mut self.rng);
    }

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
        place::step(self.pos, self.face)
    }
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
        tracing::debug!(?pos, %e, "block not set");
    }
}

/// The block at `pos` (air where nothing is loaded).
fn get(state: &State, world: WorldIndex, pos: Option<BlockPos>) -> BlockState {
    state
        .worlds
        .get(world)
        .map_or(block::AIR, |w| place::block_at(w, pos))
}

/// PMMP `World::useItemOn` with `Player::interactBlock`'s item return: the clicked block's
/// interaction, the item's own use, then placement. False when nothing happened (the caller
/// resyncs).
pub(super) fn use_item_on(
    state: &mut State,
    actor: &Actor,
    pos: BlockPos,
    face: Face,
    click: Vec3,
) -> bool {
    let world = actor.world;
    let Some(w) = state.worlds.get(world) else {
        return false;
    };
    let Some(clicked) = w.block(pos) else {
        return false;
    };
    let Some(replace) = place::step(pos, face).filter(|p| (MIN_Y..=MAX_Y).contains(&p.y)) else {
        return false;
    };
    if w.block(replace).is_none() || clicked == block::AIR || actor.mode == GameMode::Spectator {
        return false;
    }
    let Some(held) = state
        .players
        .get(&actor.session)
        .map(|p| p.inventory.held().clone())
    else {
        return false;
    };
    let mut u = Use {
        actor: *actor,
        pos,
        face,
        click,
        block: clicked,
        item: held.clone(),
        returned: Vec::new(),
        rng: stream(state, world, USE, pos),
    };
    // PMMP: sneaking uses neither the block (unless the hand is empty) nor the item.
    let use_block = !actor.sneak_pressed || held.is_empty();
    let use_item = !actor.sneak_pressed;
    if use_block && interact(state, &mut u) {
        return_items(state, actor, &held, u.item, u.returned);
        return true;
    }
    if use_item {
        match item_use(state, &mut u) {
            Outcome::Success => {
                return_items(state, actor, &held, u.item, u.returned);
                return true;
            }
            Outcome::Fail => return false,
            Outcome::None => {}
        }
    }
    place_held(state, actor, &held, pos, face, click, clicked)
}

/// The placement part of PMMP `World::useItemOn`: the blocks, the entity and adventure checks,
/// the change, block entities, the sign editor, the sound, one item used.
fn place_held(
    state: &mut State,
    actor: &Actor,
    held: &Stack,
    pos: BlockPos,
    face: Face,
    click: Vec3,
    clicked: BlockState,
) -> bool {
    let world = actor.world;
    let Some(item) = held.item() else {
        return false;
    };
    let Some(w) = state.worlds.get(world) else {
        return false;
    };
    let mut rng = stream(state, world, USE, pos);
    let Some(blocks) = place::placement(w, item, pos, face, click, Some(&actor.placer()), &mut rng)
    else {
        return false;
    };
    if blocks.iter().any(|(p, s)| obstructed(state, world, *p, *s)) {
        return false;
    }
    if actor.mode == GameMode::Adventure && !adventure_allows(held, "CanPlaceOn", clicked) {
        return false;
    }
    // PMMP `BlockTransaction::apply`: nothing placed if no block would change.
    let changed = blocks
        .iter()
        .filter(|(p, s)| state.worlds.get(world).and_then(|w| w.block(*p)) != Some(*s))
        .count();
    if changed == 0 {
        return false;
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
    if let Some((first, s)) = blocks.first() {
        sound(
            state,
            world,
            center(*first),
            SoundEvent::Place,
            s.network_id().cast_signed(),
        );
    }
    let used = held.with_count(held.count().saturating_sub(1));
    return_items(state, actor, held, used, Vec::new());
    true
}

/// Whether an entity is in the way of `block` at `pos` (PMMP `World::getCollidingEntities` for
/// each collision box): players, except spectators, and the entities the entity rules say block
/// placing.
fn obstructed(state: &State, world: WorldIndex, pos: BlockPos, block: BlockState) -> bool {
    let at = block_vec(pos);
    block.collision_boxes().iter().any(|b| {
        let placed = b.offset(at);
        entity::occupied(state, world, placed)
            || state.players.values().any(|p| {
                if p.world != world || survival::game_mode(state, p) == GameMode::Spectator {
                    return false;
                }
                let feet = p.position - Vec3::new(0.0, EYE_HEIGHT, 0.0);
                let bb = Aabb::new(
                    Vec3::new(feet.x - 0.3, feet.y, feet.z - 0.3),
                    Vec3::new(feet.x + 0.3, feet.y + 1.8, feet.z + 0.3),
                );
                bb.intersects(placed)
            })
    })
}

/// PMMP `Block::onInteract` of the clicked block's class (the first class in its chain that has
/// one): true when the block did something.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one arm per PMMP class keeps the port side by side with PMMP"
)]
fn interact(state: &mut State, u: &mut Use) -> bool {
    let block = u.block;
    // Bone meal on anything that grows: the block update rules decide (PMMP `Fertilizer` in
    // each class's onInteract).
    if u.item_is(item::Class::Fertilizer) && fertilizable(block) {
        if update::fertilize(state, u.actor.world, u.pos) {
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
                let _ = inventory::open_block(state, u.actor.session, u.actor.world, u.pos);
                return true;
            }
            Class::Chest => {
                // A chest under a solid block stays shut (its pair's check is the window's).
                let above = get(state, u.actor.world, place::step(u.pos, Face::Up));
                if above.is_transparent() {
                    let _ = inventory::open_block(state, u.actor.session, u.actor.world, u.pos);
                }
                return true;
            }
            Class::Bed => return bed(state, u),
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
                let sound_event = if on {
                    SoundEvent::PowerOn
                } else {
                    SoundEvent::PowerOff
                };
                sound(state, u.actor.world, center(u.pos), sound_event, 0);
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
                    u.wear(1);
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
                if !block.name().contains(":lit_") {
                    let lit = block
                        .name()
                        .strip_prefix("minecraft:")
                        .map(|rest| {
                            let mut name = String::with_capacity(block.name().len() + 4);
                            name.push_str("minecraft:lit_");
                            name.push_str(rest);
                            name
                        })
                        .and_then(|name| props::renamed(block, &name));
                    if let Some(lit) = lit {
                        set(state, u.actor.world, u.pos, lit);
                    }
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
                    sound(
                        state,
                        u.actor.world,
                        center(u.pos),
                        SoundEvent::RespawnAnchorCharge,
                        0,
                    );
                    u.pop();
                    return true;
                }
                // A charged anchor explodes outside the nether: the explosion is an entity rule
                // (wave C).
                return false;
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
                    sound(
                        state,
                        u.actor.world,
                        center(u.pos),
                        SoundEvent::SweetBerryBushPick,
                        0,
                    );
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
                        u.wear(1);
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
                level_event(
                    state,
                    u.actor.world,
                    block_vec(u.pos),
                    Event::SoundOpenDoor,
                    0,
                );
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
                        u.wear(1);
                        set(state, u.actor.world, u.pos, stripped);
                        sound(
                            state,
                            u.actor.world,
                            center(u.pos),
                            SoundEvent::ItemUseOn,
                            stripped.network_id().cast_signed(),
                        );
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

/// PMMP `Bell::onInteract`: the bell rings when hit on a side it can swing from (the ring itself
/// is an effect only).
fn bell_rings(u: &Use) -> bool {
    let hit = props::horizontal_facing(u.actor.yaw).opposite();
    let facing = props::facing(u.block).unwrap_or(Face::North);
    match props::string(u.block, "attachment") {
        Some("hanging") => true,
        Some("standing") => props::Axis::of(hit) == props::Axis::of(facing),
        _ => hit == props::rotate_y(facing, false) || hit == props::rotate_y(facing, true),
    }
}

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

/// The PMMP classes whose `onInteract` grows the block with bone meal (each class's
/// `Fertilizer` branch; the block update rules grow it).
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

/// The item form of a block as a single stack.
fn drops_item(block: BlockState) -> Option<Stack> {
    match block.item() {
        block::ItemForm::Item(t) => Some(Stack::new(t, 1)),
        _ => None,
    }
}

/// The item type of a block's item form, if it has one.
fn item_type(block: BlockState) -> Option<item::Type> {
    match block.item() {
        block::ItemForm::Item(t) => Some(t),
        _ => None,
    }
}

/// PMMP `Bed::onInteract`: lie down in the head half (the survival rules check night, distance
/// and whether it is taken).
fn bed(state: &mut State, u: &Use) -> bool {
    let block = u.block;
    let head = props::flag(block, "head_piece_bit") == Some(true);
    let head_pos = if head {
        Some(u.pos)
    } else {
        props::facing(block).and_then(|f| place::step(u.pos, f))
    };
    let Some(head_pos) = head_pos else {
        return true;
    };
    let other = get(state, u.actor.world, Some(head_pos));
    if head_pos != u.pos
        && !(other.is(Class::Bed) && props::flag(other, "head_piece_bit") == Some(true))
    {
        // An incomplete bed (PMMP tells the player).
        return true;
    }
    let _ = survival::sleep(state, u.actor.session, u.actor.world, head_pos);
    true
}

/// PMMP `Button::onInteract`: pressed until a scheduled update releases it.
fn button(state: &mut State, u: &Use) -> bool {
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
        sound(state, u.actor.world, center(u.pos), SoundEvent::PowerOn, 0);
    }
    true
}

/// The cake with the candle the held item places (PMMP `Cake::onInteract`), if it is one.
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

/// The bites a cake has before it is gone (PMMP `Cake::MAX_BITES`).
const CAKE_MAX_BITES: i32 = 6;

/// PMMP `BaseCake::onInteract`: the player eats a slice (`Player::consumeObject`, the survival
/// rules); false, and the block stays, if it cannot eat now. Then `onConsume`: a cake loses a bite
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

/// PMMP `Campfire::onInteract`: lighting with a fire charge, flint and steel or Fire Aspect, and
/// putting out with a shovel. Cooking items is the block entity's (wave C).
fn campfire(state: &mut State, u: &mut Use) -> bool {
    let lit = props::flag(u.block, "extinguished") != Some(true);
    let world = u.actor.world;
    if !lit {
        if u.item_kind(item::Kind::FireCharge) {
            u.pop();
            if let Some(s) = props::with_flag(u.block, "extinguished", false) {
                set(state, world, u.pos, s);
            }
            level_event(state, world, block_vec(u.pos), Event::SoundBlazeFireball, 0);
            update::schedule(state, world, u.pos, 40);
            return true;
        }
        if u.lights() {
            u.wear(1);
            if let Some(s) = props::with_flag(u.block, "extinguished", false) {
                set(state, world, u.pos, s);
            }
            sound(state, world, center(u.pos), SoundEvent::Ignite, 0);
            update::schedule(state, world, u.pos, 40);
            return true;
        }
    } else if u.item_is(item::Class::Shovel) {
        u.wear(1);
        if let Some(s) = props::with_flag(u.block, "extinguished", true) {
            set(state, world, u.pos, s);
        }
        sound(state, world, center(u.pos), SoundEvent::ExtinguishFire, 0);
        return true;
    }
    false
}

/// PMMP `CandleTrait::onInteract`: lit by fire, put out by an empty hand.
fn candle(state: &mut State, u: &mut Use) -> bool {
    let lit = props::flag(u.block, "lit") == Some(true);
    let world = u.actor.world;
    if u.item_kind(item::Kind::FireCharge) || u.lights() {
        if lit {
            return true;
        }
        if u.item.item().and_then(item::Type::max_durability).is_some() {
            u.wear(1);
        } else if u.item_kind(item::Kind::FireCharge) {
            u.pop();
            level_event(state, world, block_vec(u.pos), Event::SoundBlazeFireball, 0);
        }
        sound(state, world, center(u.pos), SoundEvent::Ignite, 0);
        if let Some(s) = props::with_flag(u.block, "lit", true) {
            set(state, world, u.pos, s);
        }
        return true;
    }
    if u.item.is_empty() {
        if !lit {
            return true;
        }
        sound(state, world, center(u.pos), SoundEvent::ExtinguishFire, 0);
        if let Some(s) = props::with_flag(u.block, "lit", false) {
            set(state, world, u.pos, s);
        }
        return true;
    }
    // Not handled: a candle in hand is placed (PMMP returns false on purpose).
    false
}

/// PMMP `Cauldron`, `WaterCauldron` and `LavaCauldron` `onInteract`: filling with buckets and
/// water bottles, emptying with buckets and bottles, mixing liquids. Dyed water, potions and
/// washing items need the cauldron's block entity (the block entity rules).
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
    let (result, returned, fill_sound) = if level == 0 {
        if u.item_kind(item::Kind::WaterBucket) {
            (
                with("water", CAULDRON_FULL),
                bucket,
                SoundEvent::BucketEmptyWater,
            )
        } else if u.item_kind(item::Kind::LavaBucket) {
            (
                with("lava", CAULDRON_FULL),
                bucket,
                SoundEvent::BucketEmptyLava,
            )
        } else if water_potion {
            (
                with("water", BOTTLE_LEVELS),
                bottle,
                SoundEvent::BucketEmptyWater,
            )
        } else {
            return true;
        }
    } else if lava {
        if u.item_kind(item::Kind::Bucket) && level >= CAULDRON_FULL {
            let lava_bucket = Stack::new(item::Kind::LavaBucket.default_type(), 1);
            (with("water", 0), lava_bucket, SoundEvent::BucketFillLava)
        } else if u.item_kind(item::Kind::WaterBucket) || any_potion {
            // Mixing empties the cauldron.
            let back = if any_potion { bottle } else { bucket };
            (with("water", 0), back, SoundEvent::Fizz)
        } else {
            return true;
        }
    } else if u.item_kind(item::Kind::WaterBucket) && level < CAULDRON_FULL {
        (
            with("water", CAULDRON_FULL),
            bucket,
            SoundEvent::BucketEmptyWater,
        )
    } else if u.item_kind(item::Kind::Bucket) && level >= CAULDRON_FULL {
        let water_bucket = Stack::new(item::Kind::WaterBucket.default_type(), 1);
        (with("water", 0), water_bucket, SoundEvent::BucketFillWater)
    } else if u.item_kind(item::Kind::GlassBottle) && level >= BOTTLE_LEVELS {
        let potion = Stack::new(item::Kind::Potion.default_type(), 1);
        (
            with("water", level - BOTTLE_LEVELS),
            potion,
            SoundEvent::BucketFillWater,
        )
    } else if water_potion && level < CAULDRON_FULL {
        let next = (level + BOTTLE_LEVELS).min(CAULDRON_FULL);
        (with("water", next), bottle, SoundEvent::BucketEmptyWater)
    } else if u.item_kind(item::Kind::LavaBucket) {
        (with("water", 0), bucket, SoundEvent::Fizz)
    } else if any_potion && !water_potion {
        // Another potion mixes with the water (PMMP `WaterCauldron::onInteract`).
        (with("water", 0), bottle, SoundEvent::Fizz)
    } else {
        return true;
    };
    if let Some(result) = result {
        set(state, world, u.pos, result);
        sound(state, world, center(u.pos), fill_sound, 0);
        u.pop();
        u.returned.push(returned);
    }
    true
}

/// The copper names of a state: waxed, the oxidation prefix and the rest (PMMP `CopperTrait`).
fn copper_name(name: &str) -> Option<(bool, &'static str, &str)> {
    let rest = name.strip_prefix("minecraft:")?;
    let (waxed, rest) = match rest.strip_prefix("waxed_") {
        Some(r) => (true, r),
        None => (false, rest),
    };
    for ox in ["exposed_", "weathered_", "oxidized_"] {
        if let Some(r) = rest.strip_prefix(ox) {
            return Some((waxed, ox, r));
        }
    }
    // The plain full block is `copper_block`, its other forms `<prefix>copper`.
    let base = if rest == "copper_block" {
        "copper"
    } else {
        rest
    };
    Some((waxed, "", base))
}

/// The copper state with the same properties for `waxed` and `oxidation`.
fn copper_state(block: BlockState, waxed: bool, oxidation: &str, base: &str) -> Option<BlockState> {
    let mut name = String::with_capacity(48);
    name.push_str("minecraft:");
    if waxed {
        name.push_str("waxed_");
    }
    name.push_str(oxidation);
    if !waxed && oxidation.is_empty() && base == "copper" {
        name.push_str("copper_block");
    } else {
        name.push_str(base);
    }
    props::renamed(block, &name)
}

/// PMMP `CopperTrait::onInteract`: honeycomb waxes, an axe takes the wax off or scrapes one
/// stage of oxidation.
fn copper(state: &mut State, u: &mut Use) -> bool {
    let Some((waxed, oxidation, base)) = copper_name(u.block.name()) else {
        return false;
    };
    let world = u.actor.world;
    if !waxed && u.item_kind(item::Kind::Honeycomb) {
        if let Some(s) = copper_state(u.block, true, oxidation, base) {
            set(state, world, u.pos, s);
            level_event(state, world, block_vec(u.pos), Event::WaxOn, 0);
            u.pop();
            return true;
        }
        return false;
    }
    if u.item_is(item::Class::Axe) {
        let (next, event) = if waxed {
            (copper_state(u.block, false, oxidation, base), Event::WaxOff)
        } else {
            let previous = match oxidation {
                "oxidized_" => "weathered_",
                "weathered_" => "exposed_",
                "exposed_" => "",
                _ => return false,
            };
            (copper_state(u.block, false, previous, base), Event::Scrape)
        };
        if let Some(next) = next {
            set(state, world, u.pos, next);
            level_event(state, world, block_vec(u.pos), event, 0);
            u.wear(1);
            return true;
        }
    }
    false
}

/// PMMP `CopperDoor::onInteract`: the other half takes the new copper state.
fn copy_copper_to_other_half(state: &mut State, u: &Use) {
    let world = u.actor.world;
    let Some(now) = state.worlds.get(world).and_then(|w| w.block(u.pos)) else {
        return;
    };
    let top = props::flag(now, "upper_block_bit") == Some(true);
    let other_pos = place::step(u.pos, if top { Face::Down } else { Face::Up });
    let other = get(state, world, other_pos);
    if let (Some(p), true) = (other_pos, other.is(Class::CopperDoor)) {
        let copied = props::renamed(other, now.name()).unwrap_or(other);
        set(state, world, p, copied);
    }
}

/// PMMP `Dirt::onInteract`: a hoe tills (coarse dirt becomes dirt, rooted dirt drops its roots),
/// a water bottle makes mud.
fn dirt(state: &mut State, u: &mut Use) -> bool {
    let world = u.actor.world;
    let name = u.block.name();
    if u.face != Face::Down && u.item_is(item::Class::Hoe) {
        let above = get(state, world, place::step(u.pos, Face::Up));
        if above != block::AIR {
            return true;
        }
        u.wear(1);
        let farmland = name == "minecraft:dirt";
        let next = if farmland {
            Kind::Farmland.default_state()
        } else {
            Kind::Dirt.default_state()
        };
        sound(
            state,
            world,
            center(u.pos),
            SoundEvent::ItemUseOn,
            next.network_id().cast_signed(),
        );
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
        sound(state, world, center(u.pos), SoundEvent::Splash, 0);
        return true;
    }
    false
}

/// PMMP `Door::onInteract`: both halves open or close.
fn door(state: &mut State, u: &Use) -> bool {
    let world = u.actor.world;
    let open = props::flag(u.block, "open_bit") != Some(true);
    let top = props::flag(u.block, "upper_block_bit") == Some(true);
    let other_pos = place::step(u.pos, if top { Face::Down } else { Face::Up });
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
    level_event(state, world, block_vec(u.pos), Event::SoundOpenDoor, 0);
    true
}

/// PMMP `FenceGate::onInteract`: opens away from the player.
fn fence_gate(state: &mut State, u: &Use) -> bool {
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
    level_event(state, world, block_vec(u.pos), Event::SoundOpenDoor, 0);
    true
}

/// PMMP `Grass::onInteract`: a hoe makes farmland, a shovel a path, with air above.
fn grass(state: &mut State, u: &mut Use) -> bool {
    let world = u.actor.world;
    if get(state, world, place::step(u.pos, Face::Up)) != block::AIR || u.face == Face::Down {
        return false;
    }
    let next = if u.item_is(item::Class::Hoe) {
        Kind::Farmland.default_state()
    } else if u.item_is(item::Class::Shovel) {
        Kind::GrassPath.default_state()
    } else {
        return false;
    };
    u.wear(1);
    sound(
        state,
        world,
        center(u.pos),
        SoundEvent::ItemUseOn,
        next.network_id().cast_signed(),
    );
    set(state, world, u.pos, next);
    true
}

/// Which side of a sign the player at `feet` faces (PMMP `BaseSign::interactsFront`, by the
/// sign's facing direction instead of `atan2`).
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

/// PMMP `Item::onInteractBlock` of the held item's class (when the block did nothing).
fn item_use(state: &mut State, u: &mut Use) -> Outcome {
    let world = u.actor.world;
    let Some(item) = u.item.item() else {
        return Outcome::None;
    };
    let replace_pos = u.replace();
    let replace = get(state, world, replace_pos);
    if item.is(item::Class::LiquidBucket) {
        let (liquid, rate, empty_sound) = if u.item_kind(item::Kind::LavaBucket) {
            (Kind::Lava, LAVA_TICK_RATE, SoundEvent::BucketEmptyLava)
        } else if u.item_kind(item::Kind::WaterBucket) {
            (Kind::Water, WATER_TICK_RATE, SoundEvent::BucketEmptyWater)
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
        set(state, world, pos, flowing);
        update::schedule(state, world, pos, rate);
        sound(state, world, center(pos), empty_sound, 0);
        u.pop();
        u.returned
            .push(Stack::new(item::Kind::Bucket.default_type(), 1));
        return Outcome::Success;
    }
    if u.item_kind(item::Kind::Bucket) {
        let block = u.block;
        let source = block.is(Class::Liquid) && props::int(block, "liquid_depth") == Some(0);
        if !source {
            return Outcome::None;
        }
        let (filled, fill_sound) = match block.kind() {
            Kind::Water => (item::Kind::WaterBucket, SoundEvent::BucketFillWater),
            Kind::Lava => (item::Kind::LavaBucket, SoundEvent::BucketFillLava),
            _ => return Outcome::Fail,
        };
        set(state, world, u.pos, block::AIR);
        sound(state, world, center(u.pos), fill_sound, 0);
        u.pop();
        u.returned.push(Stack::new(filled.default_type(), 1));
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
            level_event(state, world, center(pos), Event::SoundBlazeFireball, 0);
            u.pop();
        } else {
            sound(state, world, center(pos), SoundEvent::Ignite, 0);
            u.wear(1);
        }
        return Outcome::Success;
    }
    // End crystals, fireworks, paintings and spawn eggs make entities: the entity rules' (wave
    // C); none of their kinds exist yet.
    Outcome::None
}

/// The flowing form of a liquid at full level (Bedrock `flowing_*` with depth 0).
fn flowing(liquid: Kind) -> Option<BlockState> {
    let name = if liquid == Kind::Lava {
        "minecraft:flowing_lava"
    } else {
        "minecraft:flowing_water"
    };
    BlockState::find(name, &[("liquid_depth", block::PropertyValue::Int(0))])
}

/// PMMP `Player::useHeldItem` for items that are not consumables (those are the survival
/// rules'): refused while the item's cooldown runs; a use that works starts the cooldown again
/// (ender pearls, goat horns).
pub(super) fn use_held_item(state: &mut State, actor: &Actor) -> bool {
    if actor.mode == GameMode::Spectator {
        return false;
    }
    let Some(player) = state.players.get(&actor.session) else {
        return false;
    };
    let held = player.inventory.held().clone();
    let Some(item) = held.item() else {
        return false;
    };
    if survival::has_item_cooldown(state, player, item) {
        return false;
    }
    if !click_air(state, actor, &held, item) {
        return false;
    }
    survival::reset_item_cooldown(state, actor.session, item);
    true
}

/// PMMP `Item::onClickAir`: armour goes on, a goat horn sounds, a projectile is thrown (one used
/// up with finite resources), a bow starts drawing.
fn click_air(state: &mut State, actor: &Actor, held: &Stack, item: item::Type) -> bool {
    let Some(player) = state.players.get(&actor.session) else {
        return false;
    };
    if let Some(armor) = item.armor() {
        // PMMP `Armor::onClickAir`: swap with what is worn there.
        let slot = match armor.slot() {
            item::ArmorSlot::Head => 0,
            item::ArmorSlot::Chest => 1,
            item::ArmorSlot::Legs => 2,
            item::ArmorSlot::Feet => 3,
        };
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
        sound(state, actor.world, actor.eye, SoundEvent::EquipGeneric, 0);
        if !rest.is_empty() {
            return_items(state, actor, held, held.clone(), vec![rest]);
        }
        return true;
    }
    if item.kind() == Some(item::Kind::GoatHorn) {
        // PMMP `GoatHornSound`: the call of the horn's type (its meta).
        let calls = [
            SoundEvent::GoatCall0,
            SoundEvent::GoatCall1,
            SoundEvent::GoatCall2,
            SoundEvent::GoatCall3,
            SoundEvent::GoatCall4,
            SoundEvent::GoatCall5,
            SoundEvent::GoatCall6,
            SoundEvent::GoatCall7,
        ];
        let call = calls
            .get(usize::from(held.damage()))
            .copied()
            .unwrap_or(SoundEvent::GoatCall0);
        sound(state, actor.world, actor.eye, call, 0);
        return true;
    }
    // PMMP `ProjectileItem::onClickAir`: the entity rules throw it; one is used up.
    if item.throw_force().is_some() {
        if !entity::throw(state, actor.session, held) {
            return false;
        }
        let used = held.with_count(held.count().saturating_sub(1));
        return_items(state, actor, held, used, Vec::new());
        return true;
    }
    // PMMP `Bow::canStartUsingItem`: drawing starts with an arrow to shoot (or infinite
    // resources); the release shoots.
    if item.is(item::Class::Bow) {
        let tick = state.tick;
        if let Some(player) = state.players.get_mut(&actor.session)
            && (!actor.finite() || inventory::has_arrow(player))
        {
            player.breaking.using_since = Some(tick);
        }
        return true;
    }
    false
}

/// PMMP `Player::releaseHeldItem`: a drawn bow shoots (the entity rules) with the ticks it was
/// drawn.
pub(super) fn release(state: &mut State, session: SessionId) {
    let tick = state.tick;
    let Some(since) = state
        .players
        .get_mut(&session)
        .and_then(|p| p.breaking.using_since.take())
    else {
        return;
    };
    let used = u32::try_from(tick.saturating_sub(since)).unwrap_or(u32::MAX);
    let _shot = entity::release_bow(state, session, used);
}

/// `UseItemOnEntity`: attacking another player is the survival rules' melee (PMMP
/// `attackEntity`); attacking or using another entity (trading with a villager) is the entity
/// rules'.
pub(super) fn on_entity(
    state: &mut State,
    session: SessionId,
    runtime_id: u64,
    action: UseItemOnEntityAction,
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
            let _used = entity::interact(state, session, target);
        }
    }
}

/// Stores a sign edit in the sign's block entity ([`super::decor::write_sign`]).
pub(super) fn write_sign(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    edit: &sign::Edit,
) -> bool {
    super::decor::write_sign(state, world, pos, edit)
}

/// PMMP `DragonEgg::teleport`: up to 16 tries at a random air block within 16 blocks across and 8
/// up or down.
pub(super) fn teleport_dragon_egg(
    state: &mut State,
    world: WorldIndex,
    pos: BlockPos,
    egg: BlockState,
) {
    let mut rng = stream(state, world, USE, pos);
    for _ in 0..16 {
        let to = BlockPos::new(
            pos.x.saturating_add(rng.range(-16, 16)),
            pos.y.saturating_add(rng.range(-8, 8)).clamp(MIN_Y, MAX_Y),
            pos.z.saturating_add(rng.range(-16, 16)),
        );
        if state.worlds.get(world).and_then(|w| w.block(to)) != Some(block::AIR) {
            continue;
        }
        let (dx, dy, dz) = (
            pos.x.saturating_sub(to.x),
            pos.y.saturating_sub(to.y),
            pos.z.saturating_sub(to.z),
        );
        // PMMP `DragonEggTeleportParticle`: signs in bits 24-26, distances in bytes.
        let data = (i32::from(dz < 0) << 24)
            | (i32::from(dy < 0) << 25)
            | (i32::from(dx < 0) << 26)
            | ((dx.unsigned_abs().min(255).cast_signed()) << 16)
            | ((dy.unsigned_abs().min(255).cast_signed()) << 8)
            | dz.unsigned_abs().min(255).cast_signed();
        level_event(
            state,
            world,
            block_vec(pos),
            Event::ParticlesDragonEgg,
            data,
        );
        set(state, world, pos, block::AIR);
        set(state, world, to, egg);
        return;
    }
}

/// The fuse of lit TNT (PMMP `TNT::ignite` default, 80 ticks).
const TNT_FUSE: u32 = 80;

/// PMMP `TNT::ignite`: the block becomes a primed TNT entity (the entity rules); false at the
/// entity limit, and the block stays.
pub(super) fn ignite_tnt(state: &mut State, world: WorldIndex, pos: BlockPos) -> bool {
    entity::ignite(state, world, pos, TNT_FUSE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copper_names_split_and_join_back() {
        for state in BlockState::all().filter(|s| s.has_trait(block::Trait::Copper)) {
            let (waxed, ox, base) = copper_name(state.name()).unwrap();
            assert_eq!(
                copper_state(state, waxed, ox, base),
                Some(state),
                "{state:?}"
            );
            if !waxed {
                assert!(copper_state(state, true, ox, base).is_some(), "{state:?}");
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
