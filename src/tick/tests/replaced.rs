//! Block entities of blocks that change without being broken: explosions and a task's
//! `World::set_block` leave no block entity behind, and everything the block entity held drops
//! once.

use tokio::sync::mpsc;

use super::crash::ground;
use super::*;
use crate::block::{self, Face, Kind as BlockKind};
use crate::item::{Kind as ItemKind, Stack, Type};
use crate::math::Vec3;
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
    let center = Vec3::new(
        f64::from(AT.x) + 1.5,
        f64::from(AT.y) + 0.5,
        f64::from(AT.z) + 0.5,
    );
    crate::tick::Context { state: &mut state }
        .explode(crate::action::Explode::new(
            crate::world::Id::DEFAULT,
            center,
            4.0,
        ))
        .unwrap();
    tick(&mut state, &mut Vec::new(), &mut output);
    assert!(
        state.worlds[0]
            .block(AT)
            .is_some_and(|b| b.kind() != BlockKind::Chest)
    );
    assert!(state.rules.block_entities.get(0, AT).is_none());
    assert_eq!(on_ground(&mut state, stick), 10);
    assert_eq!(on_ground(&mut state, diamond), 3);
}

#[test]
fn a_task_replacing_containers_drops_what_they_held_once() {
    let (mut state, mut output, _rx) = server();
    let stick = ItemKind::Stick.default_type();
    // A chest replaced by stone, a shulker box by air, a barrel by a chest, which takes a new
    // block entity.
    let chest = AT;
    let shulker = BlockPos::new(10, -60, 8);
    let barrel = BlockPos::new(14, -60, 8);
    place(&mut state, chest, BlockKind::Chest.default_state());
    place(&mut state, shulker, BlockKind::ShulkerBox.default_state());
    place(&mut state, barrel, BlockKind::Barrel.default_state());
    for pos in [chest, shulker, barrel] {
        state.rules.block_entities.get_mut(0, pos).unwrap().items[0] = Stack::new(stick, 4);
    }
    let world = &mut state.worlds[0];
    world
        .set_block(chest, BlockKind::Stone.default_state())
        .unwrap();
    world.set_block(shulker, block::AIR).unwrap();
    world
        .set_block(barrel, BlockKind::Chest.default_state())
        .unwrap();
    tick(&mut state, &mut Vec::new(), &mut output);
    for pos in [chest, shulker] {
        assert!(
            state.rules.block_entities.get(0, pos).is_none(),
            "{pos:?} keeps a block entity"
        );
    }
    // The chest in the barrel's place gets an empty block entity of its own.
    let new = state.rules.block_entities.get(0, barrel).unwrap();
    assert!(matches!(new.kind, block_entity::Kind::Chest { .. }));
    assert!(new.items.iter().all(Stack::is_empty));
    assert_eq!(on_ground(&mut state, stick), 3 * 4);
    // Nothing more drops later.
    for _ in 0..5 {
        tick(&mut state, &mut Vec::new(), &mut output);
    }
    assert_eq!(on_ground(&mut state, stick), 3 * 4);
}
