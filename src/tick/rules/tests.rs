//! Chunk extras read on the save thread load the same state as when the tick read them.

use std::sync::Arc;

use super::reads::Reads;
use super::*;
use crate::block::{self, Class};
use crate::entity::Custom as Definition;
use crate::event::Listeners;
use crate::item::{Kind as ItemKind, Stack};
use crate::nbt::{Compound, List, Value};
use crate::random::Fnv;
use crate::save::{Loaded, Response, Snapshot};
use crate::tick::State;
use crate::tick::entity::Customs;
use crate::tick::tests::{new_state, settings};
use crate::tick::{Context, view};
use crate::world::{BlockPos, ChunkPos, Flat, SUB_CHUNKS};

/// Chests of the disk chunk, each with every slot full.
const CHESTS: i32 = 8;
/// The chunk the file holds terrain and extras for: it loads from the disk.
const DISK: ChunkPos = ChunkPos::new(40, 40);
/// The outcome of the fixture in each of the [`RUNS`] (state hash, rules hash, client bytes,
/// changed lists).
const EXPECTED_OUTCOME: [[u64; 4]; 2] = [
    [
        0x7cfa_45d3_fff2_dacc,
        0x3940_4dc6_a003_819d,
        0xc4d0_b7b4_ec98_80fa,
        0xb713_398d_58c8_d5e5,
    ],
    [
        0x052c_be7f_487e_e28f,
        0xce78_5002_959b_6c8a,
        0xc4d0_b7b4_ec98_80fa,
        0x8529_4a2b_6280_7bf4,
    ],
];
/// What reading the fixture's extras counts in each of the [`RUNS`]: 13 block entities (8 chests,
/// the shulker box, 1 + 3 strays), 8 entity compounds, 225 stacks (216 in the chests, 2 in the
/// shulker box, 4 in strays, 3 dropped items) and 17 or 15 saved compounds cloned (every block
/// entity, the 1 entry kept raw, and the compounds that are no entity: the pig, and the custom
/// entities on a server without their type).
const EXPECTED_READS: [Reads; 2] = [
    Reads {
        block_entities: 13,
        entities: 8,
        stacks: 225,
        clones: 17,
    },
    Reads {
        block_entities: 13,
        entities: 8,
        stacks: 225,
        clones: 15,
    },
];
/// The chunk the file holds only extras for: it is generated.
const GENERATED: ChunkPos = ChunkPos::new(41, 40);
/// The two runs (custom content, xp): vanilla with orbs, and with the custom item and
/// entity type without orbs.
const RUNS: [(bool, bool); 2] = [(false, true), (true, false)];
/// The height of the fixture's blocks: the first air above the flat ground.
const Y: i32 = -60;

fn at(pos: ChunkPos, x: i32, y: i32, z: i32) -> BlockPos {
    BlockPos::new(pos.x * 16 + x, y, pos.z * 16 + z)
}

fn block_entity(id: &str, pos: BlockPos, items: Vec<Compound>) -> Compound {
    let mut c = Compound::from([
        ("id".to_owned(), Value::String(id.to_owned())),
        ("x".to_owned(), Value::Int(pos.x)),
        ("y".to_owned(), Value::Int(pos.y)),
        ("z".to_owned(), Value::Int(pos.z)),
        ("isMovable".to_owned(), Value::Byte(1)),
    ]);
    if !items.is_empty() {
        c.insert("Items".to_owned(), Value::List(List::Compound(items)));
    }
    c
}

fn slotted(slot: u8, stack: &Stack) -> Compound {
    let mut c = stack.to_nbt();
    c.insert("Slot".to_owned(), Value::Byte(slot));
    c
}

fn stick(count: u8) -> Stack {
    Stack::new(ItemKind::Stick.default_type(), count)
}

/// The disk chunk's terrain and block entities of [`fixture`].
fn disk_block_entities(custom: Option<&Registry>) -> (Chunk, Vec<Compound>) {
    let chest = block::Kind::Chest.default_state();
    let shulker = block::State::all()
        .find(|b| b.is(Class::ShulkerBox))
        .unwrap();
    let block::ItemForm::Item(shulker_item) = shulker.item() else {
        panic!("a shulker box has an item");
    };
    let mut terrain = Chunk::generate(&Flat, 5, DISK);
    let mut block_entities = Vec::new();
    for i in 0..CHESTS {
        let local = u8::try_from(2 * i).unwrap();
        terrain.set_block(local, Y, 1, chest).unwrap();
        let items = (0_u8..27)
            .map(|slot| {
                let stack = match (custom, i, slot) {
                    (Some(r), 0, 0) => {
                        let ruby = r
                            .resolve(crate::custom::ItemRef::Custom(&crate::custom::tests::RUBY))
                            .unwrap();
                        Stack::new(ruby, 3)
                    }
                    _ if slot % 2 == 0 => stick(slot + 1),
                    _ => Stack::new(ItemKind::Diamond.default_type(), slot),
                };
                slotted(slot, &stack)
            })
            .collect();
        let mut c = block_entity("Chest", at(DISK, 2 * i, Y, 1), items);
        if i == 3 {
            c.insert("CustomName".to_owned(), Value::String("Loot".to_owned()));
        }
        block_entities.push(c);
    }
    terrain.set_block(0, Y, 8, shulker).unwrap();
    block_entities.push(block_entity(
        "ShulkerBox",
        at(DISK, 0, Y, 8),
        vec![
            slotted(0, &stick(4)),
            slotted(1, &Stack::new(shulker_item, 1)),
        ],
    ));
    block_entities.push(block_entity(
        "Chest",
        at(DISK, 1, Y, 12),
        vec![slotted(0, &stick(7)), slotted(5, &stick(9))],
    ));
    block_entities.push(block_entity("Chest", at(DISK, 0, Y, 1), Vec::new()));
    block_entities.push(block_entity("Chest", at(GENERATED, 0, Y, 1), Vec::new()));
    (terrain, block_entities)
}

/// A saved entity compound of `identifier` at `pos`, with `rest`.
fn entity(identifier: &str, pos: [f64; 3], rest: Vec<(&str, Value)>) -> Compound {
    let mut c = Compound::from([
        (
            "identifier".to_owned(),
            Value::String(identifier.to_owned()),
        ),
        ("Pos".to_owned(), Value::List(List::Double(pos.to_vec()))),
        (
            "Rotation".to_owned(),
            Value::List(List::Float(vec![90.0, 0.0])),
        ),
    ]);
    for (k, v) in rest {
        c.insert(k.to_owned(), v);
    }
    c
}

fn item_entity(pos: ChunkPos, x: f64, stack: &Stack) -> Compound {
    let base = |c: i32| f64::from(c * 16);
    entity(
        "minecraft:item",
        [base(pos.x) + x, -60.0, base(pos.z) + 4.5],
        vec![
            ("Item", Value::Compound(stack.to_nbt())),
            ("Health", Value::Short(5)),
            ("Age", Value::Short(10)),
            ("PickupDelay", Value::Short(10)),
        ],
    )
}

/// The snapshot that puts the fixture in the file: the disk chunk with 8 chests full of items
/// (one with a custom item when `custom` has one), a chest in the air (a stray), a shulker box
/// holding a shulker box (unfit), a repeated and an outside entry; items, an orb, a compound that
/// is no entity and a custom entity. The generated chunk holds strays and entities only.
fn fixture(custom: Option<&Registry>) -> Snapshot {
    let (terrain, block_entities) = disk_block_entities(custom);
    let ghost = |pos: ChunkPos| {
        entity(
            "test:ghost",
            [
                f64::from(pos.x * 16) + 9.5,
                -60.0,
                f64::from(pos.z * 16) + 9.5,
            ],
            vec![("Health", Value::Float(7.0))],
        )
    };
    let entities = vec![
        item_entity(DISK, 1.5, &stick(12)),
        item_entity(DISK, 2.5, &Stack::new(ItemKind::Diamond.default_type(), 2)),
        entity(
            "minecraft:xp_orb",
            [
                f64::from(DISK.x * 16) + 3.5,
                -60.0,
                f64::from(DISK.z * 16) + 3.5,
            ],
            vec![("Value", Value::Short(3)), ("Age", Value::Short(0))],
        ),
        entity(
            "minecraft:pig",
            [
                f64::from(DISK.x * 16) + 5.5,
                -60.0,
                f64::from(DISK.z * 16) + 5.5,
            ],
            Vec::new(),
        ),
        ghost(DISK),
    ];
    let generated_block_entities = vec![
        block_entity("Chest", at(GENERATED, 2, Y, 3), vec![slotted(2, &stick(2))]),
        block_entity("Chest", at(GENERATED, 4, Y, 3), vec![slotted(0, &stick(1))]),
        block_entity("Sign", at(GENERATED, 6, Y, 3), Vec::new()),
    ];
    let generated_entities = vec![
        item_entity(GENERATED, 6.5, &stick(1)),
        entity(
            "minecraft:xp_orb",
            [
                f64::from(GENERATED.x * 16) + 8.5,
                -60.0,
                f64::from(GENERATED.z * 16) + 8.5,
            ],
            vec![("Value", Value::Short(1))],
        ),
        ghost(GENERATED),
    ];
    Snapshot {
        start: 1,
        chunks: vec![(0, DISK, terrain)],
        extras: vec![
            (0, DISK, ChunkKind::BlockEntities, block_entities),
            (0, DISK, ChunkKind::Entities, entities),
            (
                0,
                GENERATED,
                ChunkKind::BlockEntities,
                generated_block_entities,
            ),
            (0, GENERATED, ChunkKind::Entities, generated_entities),
        ],
        ..Snapshot::default()
    }
}

/// A server holding the two fixture chunks, with the custom item and entity type when
/// `custom`, and orbs when `xp`.
fn server(custom: bool, xp: bool) -> (State, Arc<Registry>) {
    let mut state = new_state(settings(|_| {}), Listeners::default());
    let registry = if custom {
        let r = Arc::new(crate::custom::tests::registry());
        state.custom = Arc::clone(&r);
        state.rules.custom = Arc::clone(&r);
        let mut types = Customs::default();
        types.register(Definition::new("test:ghost").with_max_health(20.0));
        types.finish().unwrap();
        state.rules.entities.set_customs(types);
        r
    } else {
        Arc::clone(state.custom())
    };
    state.rules.entities.set_xp(xp);
    let id = state.worlds[0].id();
    let mut cx = Context { state: &mut state };
    for pos in [DISK, GENERATED] {
        let _ticket = cx.hold_chunks(id, pos, 0).unwrap();
    }
    (state, registry)
}

/// What the identity test compares: the state hash, the rule modules' hash, the client form of
/// every sub-chunk's block entities and the changed lists, each hashed.
fn outcome(state: &mut State) -> [u64; 4] {
    let whole = state.hash();
    let mut rules = Fnv::new();
    state.rules.hash_into(&mut rules);
    let mut client = Fnv::new();
    for pos in [DISK, GENERATED] {
        for index in 0..SUB_CHUNKS {
            let mut bytes = Vec::new();
            state
                .rules
                .block_entities
                .write_client_bytes(0, pos, index, &mut bytes);
            client.bytes(&bytes);
        }
    }
    let mut changed = Fnv::new();
    let lists = [
        state.rules.block_entities.take_changed(),
        state.rules.entities.take_changed(),
    ];
    for list in lists {
        changed.usize(list.len());
        for ((world, pos), compounds) in list {
            changed.usize(world);
            changed.u64(u64::from(pos.x.cast_unsigned()));
            changed.u64(u64::from(pos.z.cast_unsigned()));
            changed.usize(compounds.len());
            for c in &compounds {
                crate::tick::extra::hash_nbt(&mut changed, c);
            }
        }
    }
    [whole, rules.0, client.0, changed.0]
}

/// Loads the fixture through the save thread's load and the tick's answer handling; returns
/// what reading the answers' extras again on this thread counts (what the load did off the
/// tick), what the tick read while it took the answers, and the outcome.
fn load(custom: bool, xp: bool) -> (Reads, Reads, [u64; 4]) {
    let (mut state, registry) = server(custom, xp);
    let snapshot = fixture(custom.then_some(&*registry));
    let types = Arc::clone(state.entity_types());
    let keys = [(0, DISK), (0, GENERATED)];
    let answers = crate::save::tests::commit_and_load(&snapshot, &keys, registry, types);
    reads::take();
    let mut origins = Vec::new();
    for answer in &answers {
        let Response::Chunk {
            world,
            pos,
            loaded: Loaded::Disk(chunk, stored) | Loaded::Generated(chunk, stored),
        } = answer
        else {
            panic!("a loaded chunk");
        };
        origins.push(matches!(
            answer,
            Response::Chunk {
                loaded: Loaded::Disk(..),
                ..
            }
        ));
        let again = ChunkRules::read(
            (*world, *pos),
            &stored.extras,
            chunk,
            state.custom(),
            state.entity_types(),
        )
        .unwrap();
        assert_eq!(again, stored.rules, "the load read the extras");
    }
    assert_eq!(
        origins,
        [true, false],
        "one chunk from the disk, one generated"
    );
    let off_tick = reads::take();
    for answer in answers {
        let Response::Chunk { world, pos, loaded } = answer else {
            panic!("a chunk answer");
        };
        view::loaded(&mut state, world, pos, loaded);
    }
    let on_tick = reads::take();
    (off_tick, on_tick, outcome(&mut state))
}

#[test]
fn extras_read_on_the_save_thread_load_the_recorded_state() {
    for (i, (custom, xp)) in RUNS.into_iter().enumerate() {
        let (_, _, outcome) = load(custom, xp);
        assert_eq!(outcome, EXPECTED_OUTCOME[i], "custom {custom}");
    }
}

#[test]
fn a_chunk_load_reads_no_extras_on_the_tick() {
    for (i, (custom, xp)) in RUNS.into_iter().enumerate() {
        let (off_tick, on_tick, _) = load(custom, xp);
        assert_eq!(
            off_tick, EXPECTED_READS[i],
            "the load reads the extras, custom {custom}"
        );
        // A saved orb of a server without orbs stays as saved: the tick keeps its compound.
        let kept_orbs = Reads {
            clones: if xp { 0 } else { 2 },
            ..Reads::default()
        };
        assert_eq!(on_tick, kept_orbs, "custom {custom}");
    }
}

/// An answer for a chunk nobody holds is dropped without a read.
#[test]
fn an_unwanted_answer_is_dropped_without_a_read() {
    let mut state = new_state(settings(|_| {}), Listeners::default());
    let registry = Arc::clone(state.custom());
    let types = Arc::clone(state.entity_types());
    let keys = [(0, DISK), (0, GENERATED)];
    let answers = crate::save::tests::commit_and_load(&fixture(None), &keys, registry, types);
    reads::take();
    for answer in answers {
        let Response::Chunk { world, pos, loaded } = answer else {
            panic!("a chunk answer");
        };
        view::loaded(&mut state, world, pos, loaded);
        assert!(!state.worlds[0].is_chunk_loaded(pos));
    }
    assert_eq!(reads::take(), Reads::default());
}

/// A saved block entity list keeping more entries as read than a chunk may loads its chunk as
/// damaged: the save keeps it as it is.
#[test]
fn a_block_entity_list_of_too_many_raw_entries_loads_damaged() {
    let state = new_state(settings(|_| {}), Listeners::default());
    let raw = Compound::from([("id".to_owned(), Value::String("Chest".to_owned()))]);
    let list = vec![raw; crate::storage::codec::MAX_RAW_BLOCK_ENTITIES + 1];
    let snapshot = Snapshot {
        start: 1,
        extras: vec![(0, GENERATED, ChunkKind::BlockEntities, list)],
        ..Snapshot::default()
    };
    let registry = Arc::clone(state.custom());
    let types = Arc::clone(state.entity_types());
    let answers =
        crate::save::tests::commit_and_load(&snapshot, &[(0, GENERATED)], registry, types);
    assert!(matches!(
        answers.as_slice(),
        [Response::Chunk {
            loaded: Loaded::Damaged,
            ..
        }]
    ));
}
