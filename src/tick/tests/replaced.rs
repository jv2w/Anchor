//! Block entities of blocks that change without being broken: fire burning, liquids flowing,
//! explosions and a task's `World::set_block` leave no block entity behind, and everything the
//! block entity held drops once.

use tokio::sync::mpsc;

use super::super::queue::Outbound;
use super::super::{Output, State, block_entity, entity};
use super::crash::ground;
use super::{join, new_state, radius, settings, tick};
use crate::block::{self, Face, Kind as BlockKind};
use crate::event::Listeners;
use crate::item::{Kind as ItemKind, Stack, Type};
use crate::nbt::Compound;
use crate::world::BlockPos;

/// Where the blocks stand: on the flat world's grass, away from the player at the spawn.
const AT: BlockPos = BlockPos::new(8, -60, 8);

/// A server with a player whose view is open (entities tick near it), its output and the
/// player's receiver (dropping it would drop the player).
fn server() -> (State, Output, mpsc::Receiver<Outbound>) {
    let mut state = new_state(settings(|_| {}), Listeners::default());
    let mut output = Output::default();
    let (j, rx) = join(1, 0);
    tick(&mut state, &mut vec![j, radius(1, 1, 4)], &mut output);
    tick(&mut state, &mut Vec::new(), &mut output);
    (state, output, rx)
}

fn block_item(kind: BlockKind) -> Type {
    let block::ItemForm::Item(t) = kind.default_state().item() else {
        panic!("{kind:?} has an item");
    };
    t
}

/// Sets `block` at `pos` with its block entity, as placing it would.
fn place(state: &mut State, pos: BlockPos, block: block::State) {
    state.worlds[0].set_block(pos, block).unwrap();
    block_entity::placed(state, 0, pos, None, &Stack::EMPTY, Face::Up);
    assert!(state.rules.block_entities.get(0, pos).is_some());
}

/// How many items of `item` lie on the ground.
fn on_ground(state: &mut State, item: Type) -> u64 {
    ground(state)
        .iter()
        .filter(|s| s.item() == Some(item))
        .map(|s| u64::from(s.count()))
        .sum()
}

/// Runs ticks until the block at `pos` is no longer of `kind` (at most `limit`); the ticks run.
fn until_replaced(
    state: &mut State,
    output: &mut Output,
    pos: BlockPos,
    kind: BlockKind,
    limit: u32,
) -> u32 {
    for n in 1..=limit {
        tick(state, &mut Vec::new(), output);
        if state.worlds[0].block(pos).is_some_and(|b| b.kind() != kind) {
            return n;
        }
    }
    panic!("{kind:?} at {pos:?} was not replaced in {limit} ticks");
}

#[test]
fn a_burnt_lectern_leaves_no_block_entity_and_drops_its_book() {
    let (mut state, mut output, _rx) = server();
    let lectern = BlockKind::Lectern.default_state();
    assert!(lectern.flammability() > 0, "lecterns burn");
    place(&mut state, AT, lectern);
    let book = ItemKind::WritableBook.default_type();
    match &mut state.rules.block_entities.get_mut(0, AT).unwrap().kind {
        block_entity::Kind::Lectern { book: b, .. } => *b = Stack::new(book, 1),
        other => panic!("{other:?}"),
    }
    // Fire on the grass on every side.
    let fire = BlockKind::Fire.default_state();
    for face in [Face::North, Face::South, Face::East, Face::West] {
        let at = AT.step(face).unwrap();
        state.worlds[0].set_block(at, fire).unwrap();
    }
    until_replaced(&mut state, &mut output, AT, BlockKind::Lectern, 4000);
    assert!(state.rules.block_entities.get(0, AT).is_none());
    assert_eq!(on_ground(&mut state, book), 1, "the book dropped once");
}

#[test]
fn water_flowing_into_a_flower_pot_drops_the_pot_and_its_plant_once() {
    let (mut state, mut output, _rx) = server();
    place(&mut state, AT, BlockKind::FlowerPot.default_state());
    let dandelion = BlockKind::Dandelion.default_state();
    match &mut state.rules.block_entities.get_mut(0, AT).unwrap().kind {
        block_entity::Kind::FlowerPot { plant } => {
            *plant = Some(block_entity::Plant::Known(dandelion));
        }
        other => panic!("{other:?}"),
    }
    let source = AT.step(Face::West).unwrap();
    state.worlds[0]
        .set_block(source, BlockKind::Water.default_state())
        .unwrap();
    until_replaced(&mut state, &mut output, AT, BlockKind::FlowerPot, 100);
    // The drops and the change feed's check both ran.
    tick(&mut state, &mut Vec::new(), &mut output);
    assert!(state.rules.block_entities.get(0, AT).is_none());
    assert_eq!(on_ground(&mut state, block_item(BlockKind::FlowerPot)), 1);
    assert_eq!(on_ground(&mut state, block_item(BlockKind::Dandelion)), 1);
}

#[test]
fn an_exploded_chest_drops_its_contents_once() {
    let (mut state, mut output, _rx) = server();
    place(&mut state, AT, BlockKind::Chest.default_state());
    let (stick, diamond) = (
        ItemKind::Stick.default_type(),
        ItemKind::Diamond.default_type(),
    );
    let items = &mut state.rules.block_entities.get_mut(0, AT).unwrap().items;
    items[0] = Stack::new(stick, 10);
    items[5] = Stack::new(diamond, 3);
    let tnt = AT.step(Face::East).unwrap();
    state.worlds[0]
        .set_block(tnt, BlockKind::Tnt.default_state())
        .unwrap();
    assert!(entity::ignite(&mut state, 0, tnt, 2));
    until_replaced(&mut state, &mut output, AT, BlockKind::Chest, 20);
    tick(&mut state, &mut Vec::new(), &mut output);
    assert!(state.rules.block_entities.get(0, AT).is_none());
    assert_eq!(on_ground(&mut state, stick), 10);
    assert_eq!(on_ground(&mut state, diamond), 3);
}

#[test]
fn a_task_replacing_containers_and_decorations_drops_what_they_held_once() {
    let (mut state, mut output, _rx) = server();
    let stick = ItemKind::Stick.default_type();
    // A chest replaced by stone, a shulker box by air, a jukebox by water, a furnace (its lit
    // form keeps the block entity) by a chest.
    let chest = AT;
    let shulker = BlockPos::new(10, -60, 8);
    let jukebox = BlockPos::new(12, -60, 8);
    let furnace = BlockPos::new(14, -60, 8);
    place(&mut state, chest, BlockKind::Chest.default_state());
    place(&mut state, shulker, BlockKind::ShulkerBox.default_state());
    place(&mut state, jukebox, BlockKind::Jukebox.default_state());
    place(&mut state, furnace, BlockKind::Furnace.default_state());
    for pos in [chest, shulker, furnace] {
        state.rules.block_entities.get_mut(0, pos).unwrap().items[0] = Stack::new(stick, 4);
    }
    let disc = Type::from_name("minecraft:music_disc_13", 0, None, &Compound::new()).unwrap();
    match &mut state.rules.block_entities.get_mut(0, jukebox).unwrap().kind {
        block_entity::Kind::Jukebox { record } => *record = Stack::new(disc, 1),
        other => panic!("{other:?}"),
    }
    let lit = block::State::all()
        .find(|s| s.name() == "minecraft:lit_furnace")
        .unwrap();
    let world = &mut state.worlds[0];
    world.set_block(furnace, lit).unwrap();
    tick(&mut state, &mut Vec::new(), &mut output);
    assert!(
        state.rules.block_entities.get(0, furnace).is_some(),
        "a lit furnace keeps its block entity"
    );
    let world = &mut state.worlds[0];
    world
        .set_block(chest, BlockKind::Stone.default_state())
        .unwrap();
    world.set_block(shulker, block::AIR).unwrap();
    world
        .set_block(jukebox, BlockKind::Water.default_state())
        .unwrap();
    world
        .set_block(furnace, BlockKind::Chest.default_state())
        .unwrap();
    tick(&mut state, &mut Vec::new(), &mut output);
    for pos in [chest, shulker, jukebox, furnace] {
        assert!(
            state.rules.block_entities.get(0, pos).is_none(),
            "{pos:?} keeps a block entity"
        );
    }
    assert_eq!(on_ground(&mut state, stick), 3 * 4);
    assert_eq!(on_ground(&mut state, disc), 1);
    // Nothing more drops later.
    for _ in 0..5 {
        tick(&mut state, &mut Vec::new(), &mut output);
    }
    assert_eq!(on_ground(&mut state, stick), 3 * 4);
    assert_eq!(on_ground(&mut state, disc), 1);
}
