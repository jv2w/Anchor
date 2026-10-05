//! The import's run on chunks given directly: conversion, the report, steps, threads, failures,
//! the spawn height and the BDS fixture's blocks.

use std::sync::Arc;

use super::*;
use crate::import::chunk::Section;
use crate::import::level_dat::WorldData;
use crate::nbt::List;
use crate::storage::codec::ExtraRecord;
use crate::storage::seal::unseal;
use crate::storage::tests::{Disk, Fault, memory};
use crate::storage::{ChunkId, ChunkKey, ChunkPart, StartBackup};
use crate::world::{Flat, Generator, Void};

const NAME: &str = "imported";

/// An empty world whose generator marks one biome: biome 21 at (2, 50, 3) of every chunk.
#[derive(Debug)]
struct Marked;

impl Generator for Marked {
    fn generate(&self, _seed: i64, _pos: ChunkPos, chunk: &mut Chunk) {
        chunk.set_biome(2, 50, 3, 21).unwrap();
    }

    fn name(&self) -> &'static str {
        "marked"
    }
}

fn spec() -> WorldSpec {
    WorldSpec {
        name: NAME.to_owned(),
        kind: Kind::Persistent,
        generator: Arc::new(Marked),
        seed: None,
    }
}

fn world(spawn: [i32; 3]) -> WorldData {
    WorldData {
        spawn,
        seed: 1234,
        time: 5678,
    }
}

fn s(text: &str) -> Value {
    Value::String(text.to_owned())
}

fn state(name: &str, states: &[(&str, Value)]) -> Compound {
    Compound::from([
        ("name".to_owned(), s(name)),
        (
            "states".to_owned(),
            Value::Compound(
                states
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), v.clone()))
                    .collect(),
            ),
        ),
    ])
}

fn empty_raw() -> Raw {
    Raw {
        version: 42,
        sections: Vec::new(),
        block_entities: Vec::new(),
        legacy_blocks: Vec::new(),
        ignored: Vec::new(),
    }
}

/// A 2-bit paletted layer with palette index `index` at the slots `at`, 0 elsewhere.
fn paletted(palette: Vec<Compound>, at: &[(u16, u32)]) -> Layer {
    let mut words = vec![0u32; 256];
    for &(offset, index) in at {
        words[usize::from(offset / 16)] |= index << ((offset % 16) * 2);
    }
    Layer::Paletted(BlockLayer {
        bits: 2,
        words,
        palette,
    })
}

fn offset(x: u16, y: u16, z: u16) -> u16 {
    x << 8 | z << 4 | y
}

/// A legacy layer of air with (id, meta) at the (x, y, z) positions given.
fn legacy(blocks: &[([usize; 3], (u8, u8))]) -> Layer {
    let mut ids = Box::new([0u8; 4096]);
    let mut meta = Box::new([0u8; 2048]);
    for &([x, y, z], (id, m)) in blocks {
        let i = x << 8 | z << 4 | y;
        ids[i] = id;
        meta[i >> 1] |= m << ((i & 1) * 4);
    }
    Layer::Legacy(LegacyLayer { ids, meta })
}

fn item(name: &str) -> Compound {
    Compound::from([
        ("Name".to_owned(), s(name)),
        ("Count".to_owned(), Value::Byte(1)),
    ])
}

/// A chunk with every kind of data the import converts.
fn full_raw() -> Raw {
    Raw {
        sections: vec![
            Section {
                y: -4,
                version: 9,
                layers: vec![paletted(
                    vec![
                        state("minecraft:air", &[]),
                        state("minecraft:red_flower", &[("mapped_type", Value::Int(0))]),
                        state("minecraft:stone", &[]),
                        state("anchor:nope", &[]),
                    ],
                    &[
                        (offset(0, 0, 0), 1),
                        (offset(0, 1, 0), 2),
                        (offset(0, 2, 0), 3),
                    ],
                )],
            },
            Section {
                y: 0,
                version: 0,
                layers: vec![legacy(&[([1, 2, 3], (1, 0)), ([0, 0, 0], (1, 15))])],
            },
        ],
        block_entities: vec![Compound::from([
            ("id".to_owned(), s("Chest")),
            (
                "Items".to_owned(),
                Value::List(List::Compound(vec![item("minecraft:photo")])),
            ),
        ])],
        legacy_blocks: vec![LegacyBlock {
            x: 5,
            y: 3,
            z: 5,
            id: 9,
            meta: 0,
        }],
        ignored: vec![0x36],
        ..empty_raw()
    }
}

fn catalog(chunks: Vec<(i32, i32)>) -> Catalog {
    Catalog {
        chunks,
        nether_chunks: 2,
        end_chunks: 3,
        other_dimension_chunks: 4,
        entities: 7,
        players: 5,
        other_keys: 6,
    }
}

/// Imports `raws` (by position) into `storage` and commits; also whether the pool's threads all
/// ended.
fn import(
    storage: &mut Storage,
    threads: usize,
    raws: &BTreeMap<(i32, i32), Raw>,
    spawn: [i32; 3],
) -> (crate::Result<Report>, bool) {
    let pool = crate::runtime::cpu_pool(threads).unwrap();
    let custom = Registry::default();
    let result = prepare(
        storage,
        &pool,
        &spec(),
        &custom,
        &catalog(raws.keys().copied().collect()),
        &world(spawn),
        |x, z| Ok(raws[&(x, z)].clone()),
        |raw: &Raw| Ok(raw.clone()),
        |_: &Raw| 0,
    )
    .and_then(|(import, report)| {
        import.commit()?;
        Ok(report)
    });
    (result, pool.shutdown())
}

/// [`import`] that must succeed.
fn imported(
    storage: &mut Storage,
    threads: usize,
    raws: &BTreeMap<(i32, i32), Raw>,
    spawn: [i32; 3],
) -> Report {
    let (result, joined) = import(storage, threads, raws, spawn);
    assert!(joined);
    result.unwrap()
}

/// The stored world record and every value of `chunk`, unsealed.
fn read_chunk(storage: &mut Storage, number: u32, x: i32, z: i32) -> (Chunk, [Vec<Compound>; 3]) {
    let id = ChunkId {
        world: number,
        x,
        z,
    };
    let values = storage.read_chunks(&[id]).unwrap().pop().unwrap().unwrap();
    let mut sub_chunks = Vec::new();
    let mut extras: [Vec<Compound>; 3] = Default::default();
    for (part, sealed) in values {
        let key = ChunkKey { chunk: id, part }.to_bytes();
        match part {
            ChunkPart::SubChunk(_) => {
                sub_chunks.push(unseal::<SubChunkRecord>(&key, &sealed).unwrap());
            }
            ChunkPart::Extras(kind, _) => {
                let list = unseal::<ExtraRecord<'_>>(&key, &sealed)
                    .unwrap()
                    .0
                    .into_owned();
                let slot = match kind {
                    ChunkKind::BlockEntities => 0,
                    ChunkKind::Entities => 1,
                    ChunkKind::ScheduledUpdates => 2,
                };
                extras[slot].extend(list);
            }
            ChunkPart::Column => panic!("an import writes sub-chunks"),
        }
    }
    let record = ChunkRecord {
        sub_chunks: sub_chunks.into_boxed_slice().try_into().unwrap(),
    };
    (
        save::to_chunk(record, &Registry::default()).unwrap(),
        extras,
    )
}

fn world_record(storage: &mut Storage) -> Option<WorldRecord> {
    save::stored_worlds(storage)
        .unwrap()
        .into_iter()
        .find(|(n, _)| n == NAME)
        .map(|(_, r)| r)
}

fn known(nbt: &Compound) -> crate::block::State {
    save::find_state(nbt).unwrap()
}

#[test]
fn a_chunk_imports_with_its_blocks_upgraded_unknown_blocks_kept_and_its_block_entities() {
    let disk = Disk::default();
    let mut storage = memory(&disk);
    // An existing file with another world numbered 3.
    let mut batch = Batch::with_capacity(1);
    let other = Key::World("other".to_owned());
    let record = WorldRecord {
        number: 3,
        seed: 0,
        spawn: [0, 0, 0],
        generator: "flat".to_owned(),
        time: 0,
        time_stopped: false,
        difficulty: None,
        weather: crate::world::Weather::Clear,
    };
    batch.put(other.clone(), seal(&other.to_bytes(), &record).unwrap());
    storage.commit(&batch).unwrap();

    let raws = BTreeMap::from([((7, -2), full_raw())]);
    let report = imported(&mut storage, 2, &raws, [10, 70, -20]);

    let record = world_record(&mut storage).unwrap();
    assert_eq!(
        (record.number, record.seed, record.spawn, record.time),
        (4, 1234, [10, 70, -20], 5678)
    );
    assert_eq!(record.generator, Marked.name());
    let (chunk, [block_entities, entities, updates]) = read_chunk(&mut storage, 4, 7, -2);
    let stone = known(&state("minecraft:stone", &[]));
    assert_eq!(
        chunk.block(0, -64, 0),
        Some(known(&state("minecraft:poppy", &[])))
    );
    assert_eq!(chunk.block(0, -63, 0), Some(stone));
    assert_eq!(chunk.block(0, -62, 0), None);
    assert_eq!(chunk.block(1, 2, 3), Some(stone));
    assert_eq!(chunk.block(0, 0, 0), None);
    assert_eq!(chunk.block(0, 100, 0), Some(crate::block::AIR));
    let unknown: Vec<&Compound> = chunk.unknown().iter().collect();
    assert_eq!(unknown.len(), 2);
    assert!(
        unknown
            .iter()
            .any(|c| c.get("name") == Some(&s("anchor:nope")))
    );
    assert!(
        unknown
            .iter()
            .any(|c| c.get("id") == Some(&Value::Short(1))
                && c.get("val") == Some(&Value::Short(15)))
    );
    // Legacy block extra data is the second layer of its legacy sub-chunk.
    let water = known(&upgrade::legacy(9, 0).unwrap());
    let layers = chunk.sub_chunk(4).unwrap().layers();
    assert_eq!(layers[1].get(offset(5, 3, 5)), Entry::Known(water));
    assert_eq!(
        layers[1].get(offset(5, 4, 5)),
        Entry::Known(crate::block::AIR)
    );
    // The generator's biomes.
    assert_eq!(chunk.biome(2, 50, 3), Some(21));
    assert_eq!(chunk.biome(0, -64, 0), Some(crate::world::PLAINS));

    assert_eq!(
        block_entities[0].get("Items"),
        Some(&Value::List(List::Compound(vec![item(
            "minecraft:photo_item"
        )])))
    );
    assert!(entities.is_empty() && updates.is_empty());

    let expected = Report {
        chunks: 1,
        block_entities: 1,
        upgraded_nbt: 1,
        entities: 7,
        ignored_tags: BTreeMap::from([(0x36, 1)]),
        nether_chunks: 2,
        end_chunks: 3,
        other_dimension_chunks: 4,
        players: 5,
        other_keys: 6,
        unknown_blocks: BTreeMap::from([
            ("anchor:nope".to_owned(), 1),
            ("legacy:1:15".to_owned(), 1),
        ]),
        unlisted_unknown_blocks: 0,
    };
    assert_eq!(report, expected);
}

#[test]
fn an_all_air_chunk_is_written_air_with_the_generators_biomes() {
    let disk = Disk::default();
    let mut storage = memory(&disk);
    let raws = BTreeMap::from([((0, 0), empty_raw())]);
    imported(&mut storage, 1, &raws, [0, 64, 0]);
    let (chunk, extras) = read_chunk(&mut storage, 0, 0, 0);
    assert_eq!(chunk.block(3, 3, 3), Some(crate::block::AIR));
    assert_eq!(chunk.biome(3, 3, 3), Some(crate::world::PLAINS));
    assert_eq!(chunk.biome(2, 50, 3), Some(21));
    assert!(extras.iter().all(Vec::is_empty));
}

/// `chunks` chunks whose lowest sub-chunk has 255 unknown blocks of distinct names, each
/// `anchor:` and `padding` bytes before a number unique across the chunks.
fn distinct_unknown_raws(chunks: i32, padding: usize) -> BTreeMap<(i32, i32), Raw> {
    let pad = "u".repeat(padding);
    (0..chunks)
        .map(|x| {
            let mut palette = vec![state("minecraft:air", &[])];
            let mut words = vec![0u32; 1024];
            for i in 1..256u32 {
                let n = u32::try_from(x).unwrap() * 256 + i;
                palette.push(state(&format!("anchor:{pad}{n}"), &[]));
                words[usize::try_from(i / 4).unwrap()] |= i << ((i % 4) * 8);
            }
            let layer = Layer::Paletted(BlockLayer {
                bits: 8,
                words,
                palette,
            });
            let raw = Raw {
                sections: vec![Section {
                    y: -4,
                    version: 9,
                    layers: vec![layer],
                }],
                ..empty_raw()
            };
            ((x, 0), raw)
        })
        .collect()
}

#[test]
fn the_report_lists_unknown_names_only_up_to_its_count_and_byte_caps_and_counts_the_rest() {
    // 17 chunks of 255 short names pass the count cap; 5 chunks of 255 names of 1,000 bytes pass
    // the byte cap.
    for (chunks, padding) in [(17, 0), (5, 1_000)] {
        let disk = Disk::default();
        let mut storage = memory(&disk);
        let report = imported(
            &mut storage,
            2,
            &distinct_unknown_raws(chunks, padding),
            [0, 64, 0],
        );
        let listed = report.unknown_blocks();
        assert!(
            listed.len() <= MAX_LISTED_UNKNOWN,
            "{} names listed",
            listed.len()
        );
        let bytes: usize = listed.keys().map(String::len).sum();
        assert!(
            bytes <= MAX_LISTED_UNKNOWN_BYTES,
            "{bytes} name bytes listed"
        );
        // Every unknown block is counted once, listed or not.
        assert!(report.unlisted_unknown_blocks() > 0);
        assert_eq!(
            listed.values().sum::<u64>() + report.unlisted_unknown_blocks(),
            u64::try_from(chunks).unwrap() * 255
        );
    }
}

/// PMMP ignores a pumpkin's facing (a dummy property) and reads every facing as its pumpkin, which
/// faces south; a state PMMP rejects stays an unknown block.
#[test]
fn a_pumpkin_of_any_facing_imports_as_pmmp_pumpkin_and_a_state_pmmp_rejects_stays_unknown() {
    let disk = Disk::default();
    let mut storage = memory(&disk);
    let saved = |mut c: Compound| {
        c.insert(
            "version".to_owned(),
            Value::Int(crate::import::BLOCK_STATE_VERSION),
        );
        c
    };
    let pumpkin = |facing: &str| {
        saved(state(
            "minecraft:pumpkin",
            &[("minecraft:cardinal_direction", s(facing))],
        ))
    };
    let column = saved(state(
        "minecraft:bubble_column",
        &[("drag_down", Value::Byte(0))],
    ));
    let facings = ["north", "east", "south", "west"];
    let raw = Raw {
        sections: vec![
            Section {
                y: -4,
                version: 9,
                layers: vec![paletted(
                    facings.map(pumpkin).to_vec(),
                    &[
                        (offset(0, 0, 0), 0),
                        (offset(1, 0, 0), 1),
                        (offset(2, 0, 0), 2),
                        (offset(3, 0, 0), 3),
                    ],
                )],
            },
            Section {
                y: -3,
                version: 9,
                layers: vec![paletted(
                    vec![state("minecraft:air", &[]), column.clone()],
                    &[(offset(0, 0, 0), 1)],
                )],
            },
        ],
        ..empty_raw()
    };
    let report = imported(
        &mut storage,
        1,
        &BTreeMap::from([((0, 0), raw)]),
        [0, 64, 0],
    );
    let (chunk, _) = read_chunk(&mut storage, 0, 0, 0);
    let south = crate::block::State::find(
        "minecraft:pumpkin",
        &[(
            "minecraft:cardinal_direction",
            crate::block::PropertyValue::String("south"),
        )],
    );
    for x in 0..4 {
        assert_eq!(chunk.block(x, -64, 0), south, "{}", facings[x as usize]);
    }
    assert_eq!(chunk.block(0, -48, 0), None);
    assert_eq!(
        chunk
            .unknown()
            .iter()
            .map(|c| c.get("name"))
            .collect::<Vec<_>>(),
        [column.get("name")]
    );
    assert_eq!(
        report.unknown_blocks(),
        &BTreeMap::from([("minecraft:bubble_column".to_owned(), 1)])
    );
}

#[test]
fn a_second_import_of_the_same_world_is_refused_and_changes_nothing() {
    let disk = Disk::default();
    let mut storage = memory(&disk);
    let raws = BTreeMap::from([((0, 0), full_raw())]);
    imported(&mut storage, 1, &raws, [0, 64, 0]);
    let before = storage
        .read_chunks(&[ChunkId {
            world: 0,
            x: 0,
            z: 0,
        }])
        .unwrap();
    let other = BTreeMap::from([((0, 0), empty_raw()), ((1, 0), empty_raw())]);
    let err = import(&mut storage, 1, &other, [0, 64, 0]).0.unwrap_err();
    assert!(
        matches!(&err, crate::Error::Import(Error::WorldExists { name }) if name == NAME),
        "{err:?}"
    );
    let after = storage
        .read_chunks(&[ChunkId {
            world: 0,
            x: 0,
            z: 0,
        }])
        .unwrap();
    assert_eq!(format!("{before:?}"), format!("{after:?}"));
    assert!(
        storage
            .read_chunks(&[ChunkId {
                world: 0,
                x: 1,
                z: 0
            }])
            .unwrap()
            .pop()
            .unwrap()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_chunk_that_fails_ends_the_import_with_its_position_and_writes_nothing() {
    let raws: BTreeMap<(i32, i32), Raw> = (0..300).map(|i| ((i, 0), full_raw())).collect();
    let mut outside = full_raw();
    outside.sections[0].y = 20;
    for (bad, x) in [(outside, 280)] {
        let disk = Disk::default();
        let mut storage = memory(&disk);
        let mut raws = raws.clone();
        raws.insert((x, 0), bad);
        let err = import(&mut storage, 3, &raws, [0, 64, 0]).0.unwrap_err();
        match err {
            crate::Error::Import(Error::Chunk {
                x: ex,
                z: 0,
                source,
            }) => {
                assert_eq!(ex, x);
                assert!(matches!(*source, Error::Malformed(_)), "{source:?}");
            }
            e => panic!("{e:?}"),
        }
        assert_eq!(world_record(&mut storage), None);
        for x in [0, 279] {
            let values = storage
                .read_chunks(&[ChunkId { world: 0, x, z: 0 }])
                .unwrap();
            assert!(values[0].as_ref().unwrap().is_empty(), "{x}");
        }
    }
    // A read or decode failure is the reader's, already naming its chunk.
    let disk = Disk::default();
    let mut storage = memory(&disk);
    let pool = crate::runtime::cpu_pool(2).unwrap();
    let custom = Registry::default();
    let err = prepare(
        &mut storage,
        &pool,
        &spec(),
        &custom,
        &catalog(vec![(0, 0), (1, 0)]),
        &world([0, 64, 0]),
        |x, z| Ok((x, z)),
        |&(x, z): &(i32, i32)| {
            if x == 1 {
                Err(Error::Chunk {
                    x,
                    z,
                    source: Box::new(Error::Malformed("broken")),
                })
            } else {
                Ok(empty_raw())
            }
        },
        |_: &(i32, i32)| 0,
    )
    .unwrap_err();
    assert!(pool.shutdown());
    assert!(
        matches!(err, crate::Error::Import(Error::Chunk { x: 1, z: 0, .. })),
        "{err:?}"
    );
    assert_eq!(world_record(&mut storage), None);
}

#[test]
fn a_failed_commit_leaves_the_world_file_as_it_was() {
    let mut rng = crate::protocol::Rng(0x1a9_0e77);
    let raws: BTreeMap<(i32, i32), Raw> = (0..3).map(|i| ((i, i), full_raw())).collect();
    for at in 1.. {
        let disk = Disk::default();
        let mut storage = memory(&disk);
        let pool = crate::runtime::cpu_pool(1).unwrap();
        let custom = Registry::default();
        let (import, _) = prepare(
            &mut storage,
            &pool,
            &spec(),
            &custom,
            &catalog(raws.keys().copied().collect()),
            &world([0, 64, 0]),
            |x, z| Ok(raws[&(x, z)].clone()),
            |raw: &Raw| Ok(raw.clone()),
            |_: &Raw| 0,
        )
        .unwrap();
        assert!(pool.shutdown());
        disk.arm(Fault::From { at });
        let committed = import.commit().is_ok();
        drop(storage);
        disk.power_loss(&mut rng, true);
        let mut storage = memory(&disk);
        assert_eq!(world_record(&mut storage).is_some(), committed, "{at}");
        let values = storage
            .read_chunks(&[ChunkId {
                world: 0,
                x: 1,
                z: 1,
            }])
            .unwrap();
        assert_eq!(values[0].as_ref().unwrap().is_empty(), !committed, "{at}");
        if committed {
            break;
        }
    }
}

#[test]
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: a test counts reads across the pool's threads"
)]
fn a_step_reads_chunks_only_up_to_its_byte_cap() {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    // Each chunk read holds half the cap, so a step decodes every two.
    let reads = AtomicUsize::new(0);
    // The reads done when the first decode ran (0 before it).
    let first = AtomicUsize::new(0);
    let disk = Disk::default();
    let mut storage = memory(&disk);
    let pool = crate::runtime::cpu_pool(2).unwrap();
    let custom = Registry::default();
    let (_, report) = prepare(
        &mut storage,
        &pool,
        &spec(),
        &custom,
        &catalog((0..10).map(|x| (x, 0)).collect()),
        &world([0, 64, 0]),
        |_, _| {
            reads.fetch_add(1, Relaxed);
            Ok(STEP_BYTES / 2)
        },
        |_: &usize| {
            first
                .compare_exchange(0, reads.load(Relaxed), Relaxed, Relaxed)
                .unwrap_or_default();
            Ok(empty_raw())
        },
        |&size: &usize| size,
    )
    .unwrap();
    assert!(pool.shutdown());
    assert_eq!(report.chunks(), 10);
    assert_eq!(first.load(Relaxed), 2);
}

#[test]
fn one_and_many_threads_write_identical_bytes() {
    let raws: BTreeMap<(i32, i32), Raw> = (-12i32..13)
        .flat_map(|x| (-12i32..13).map(move |z| (x, z)))
        .map(|(x, z)| {
            let mut raw = full_raw();
            raw.legacy_blocks[0].x = u8::try_from(x.rem_euclid(16)).unwrap();
            raw.sections[0].y = i8::try_from(z.rem_euclid(24) - 4).unwrap();
            ((x, z), raw)
        })
        .collect();
    assert!(raws.len() > STEP);
    let mut contents = Vec::new();
    for threads in [1, 4] {
        let disk = Disk::default();
        let mut storage = memory(&disk);
        imported(&mut storage, threads, &raws, [0, 64, 0]);
        let ids: Vec<ChunkId> = raws
            .keys()
            .map(|&(x, z)| ChunkId { world: 0, x, z })
            .collect();
        let values = storage.read_chunks(&ids).unwrap();
        contents.push(format!("{values:?}{:?}", world_record(&mut storage)));
    }
    assert_eq!(contents[0], contents[1]);
}

#[test]
fn a_spawn_without_a_height_lands_above_the_highest_block_or_at_the_generator_spawn() {
    let mut raw = empty_raw();
    raw.sections.push(Section {
        y: 0,
        version: 9,
        layers: vec![paletted(
            vec![state("minecraft:air", &[]), state("minecraft:stone", &[])],
            &[
                (offset(3, 10, 4), 1),
                (offset(3, 2, 4), 1),
                (offset(4, 12, 4), 1),
            ],
        )],
    });
    let raws = BTreeMap::from([((-1, 2), raw)]);
    let disk = Disk::default();
    let mut storage = memory(&disk);
    imported(&mut storage, 1, &raws, [-13, SPAWN_Y_UNSET, 36]);
    assert_eq!(world_record(&mut storage).unwrap().spawn, [-13, 11, 36]);
    // No chunk at the spawn: the configured generator's spawn height.
    let disk = Disk::default();
    let mut storage = memory(&disk);
    imported(&mut storage, 1, &raws, [100, SPAWN_Y_UNSET, 100]);
    assert_eq!(
        world_record(&mut storage).unwrap().spawn,
        [100, Void.spawn(1234).y, 100]
    );
    assert_ne!(Void.spawn(1234).y, Flat.spawn(1234).y);
}

#[test]
fn an_undeclared_or_temporary_world_is_refused_before_anything_is_opened() {
    let dir = crate::save::tests::TempDir::new("import-undeclared");
    let data = dir.0.join("data");
    let mut temporary = spec();
    temporary.kind = Kind::Temporary;
    for (worlds, name) in [(vec![spec()], "other"), (vec![temporary], NAME)] {
        let err = run(
            &data,
            Start::checked(StartBackup::Make),
            &worlds,
            1,
            &Registry::default(),
            &dir.0,
            name,
        )
        .unwrap_err();
        assert!(
            matches!(&err, crate::Error::Import(Error::UnknownWorld { name: n }) if n == name),
            "{err:?}"
        );
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: a test checks the folder"
    )]
    let created = data.exists();
    assert!(!created);
}

/// The process's peak working set in MiB, from the OS (`None` if it cannot be read).
#[cfg(windows)]
fn peak_memory_mib() -> Option<u64> {
    let script = format!("(Get-Process -Id {}).PeakWorkingSet64", std::process::id());
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .ok()?;
    let bytes: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(bytes >> 20)
}

/// The process's peak resident set in MiB, from the OS (`None` if it cannot be read).
#[cfg(unix)]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: a measurement reads /proc"
)]
fn peak_memory_mib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib >> 10)
}

/// A terrain-like chunk: nine stored sub-chunks of four states each and a block entity.
fn terrain_raw(rng: &mut crate::protocol::Rng) -> Raw {
    let palette = vec![
        state("minecraft:stone", &[]),
        state("minecraft:dirt", &[]),
        state("minecraft:deepslate", &[("pillar_axis", s("y"))]),
        state("minecraft:water", &[("liquid_depth", Value::Int(0))]),
    ];
    let sections = (-4..=4)
        .map(|y| {
            let words = (0..256)
                .map(|_| u32::try_from(rng.below(1 << 31)).unwrap())
                .collect();
            Section {
                y,
                version: 9,
                layers: vec![Layer::Paletted(BlockLayer {
                    bits: 2,
                    words,
                    palette: palette.clone(),
                })],
            }
        })
        .collect();
    let mut raw = full_raw();
    raw.sections = sections;
    raw.legacy_blocks.clear();
    raw
}

fn ms(start: std::time::Instant) -> f64 {
    crate::clock::now().duration_since(start).as_secs_f64() * 1e3
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: a measurement reads the file size"
)]
fn file_mib(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len() >> 20
}

/// Measurements for PLAN.md: `cargo test --release --lib
/// import::api::tests::measure_100k_chunk_import -- --ignored --nocapture`.
#[test]
#[ignore = "measurement"]
#[expect(
    clippy::print_stdout,
    reason = "consistency: a measurement prints its results"
)]
fn measure_100k_chunk_import() {
    let dir = crate::save::tests::TempDir::new("import-measure");
    let mut storage = Storage::open(
        &dir.0,
        crate::storage::Start::checked(crate::storage::StartBackup::Make),
    )
    .unwrap();
    let mut rng = crate::protocol::Rng(0x0010_0000);
    // A few distinct chunks, read again and again: generating each would dominate the time.
    let templates: Vec<Raw> = (0..64).map(|_| terrain_raw(&mut rng)).collect();
    // In (x, z) order, as the reader lists them.
    let positions: Vec<(i32, i32)> = (0..100_000).map(|i| (i / 316, i % 316)).collect();
    let threads = crate::runtime::logical_cpus().saturating_sub(2).max(1);
    let pool = crate::runtime::cpu_pool(threads).unwrap();
    let custom = Registry::default();
    let before = peak_memory_mib();
    let start = crate::clock::now();
    let (import, report) = prepare(
        &mut storage,
        &pool,
        &spec(),
        &custom,
        &catalog(positions),
        &world([0, 64, 0]),
        |x, z| Ok(templates[usize::try_from((x ^ z) & 63).unwrap()].clone()),
        |raw: &Raw| Ok(raw.clone()),
        |_: &Raw| 0,
    )
    .unwrap();
    let written = ms(start);
    let start = crate::clock::now();
    import.commit().unwrap();
    let committed = ms(start);
    let peak = peak_memory_mib();
    assert!(pool.shutdown());
    let file = dir.0.join(crate::storage::WORLD_FILE);
    let size = file_mib(&file);
    let start = crate::clock::now();
    storage.close().unwrap();
    let closed = ms(start);
    println!(
        "{} chunks on {threads} threads: converted and written {written:.0} ms, commit \
         {committed:.0} ms, close (compact) {closed:.0} ms; file {size} MiB, {} MiB compacted; \
         peak memory {before:?} MiB before, {peak:?} MiB after",
        report.chunks,
        file_mib(&file)
    );
}

/// The world BDS 1.26.52 made (IMPORT-R's fixture): flat, seed 7, filled blocks, water, a chest,
/// a sign, a pig, an armor stand and pending ticks; the pig, the armor stand and the ticks are not
/// imported.
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/xtask/data/import/flat");

#[test]
fn the_bds_fixture_imports_with_every_block_known_and_in_place() {
    let dir = crate::save::tests::TempDir::new("import-fixture");
    let data = dir.0.join("data");
    let report = run(
        &data,
        Start::checked(StartBackup::Make),
        &[spec()],
        2,
        &Registry::default(),
        Path::new(FIXTURE),
        NAME,
    )
    .unwrap();
    let mut source = Source::open(Path::new(FIXTURE)).unwrap();
    let catalog = source.catalog().unwrap();
    let mut storage = Storage::open(&data, Start::checked(StartBackup::Make)).unwrap();
    let record = world_record(&mut storage).unwrap();
    assert_eq!((record.seed, record.time), (7, source.world().time));
    for &(x, z) in &catalog.chunks {
        let raw = chunk::decode(&source.read(x, z).unwrap()).unwrap();
        let (chunk, [block_entities, entities, updates]) =
            read_chunk(&mut storage, record.number, x, z);
        assert!(
            chunk.unknown().is_empty(),
            "({x}, {z}): {:?}",
            chunk.unknown()
        );
        assert_eq!(block_entities, raw.block_entities);
        assert!(entities.is_empty() && updates.is_empty());
        for section in &raw.sections {
            let sub = chunk
                .sub_chunk(usize::try_from(i32::from(section.y) + 4).unwrap())
                .unwrap();
            for (layer, stored) in section.layers.iter().zip(sub.layers()) {
                let Layer::Paletted(layer) = layer else {
                    panic!("BDS writes paletted layers");
                };
                let indices: Vec<u16> = (0..u16::try_from(layer.palette.len()).unwrap()).collect();
                let cube = Paletted::from_parts(layer.bits, &layer.words, &indices).unwrap();
                for offset in 0..SLOTS {
                    let entry = &layer.palette[usize::from(cube.get(offset))];
                    let expected = save::find_state(&upgrade::state(entry).unwrap()).unwrap();
                    assert_eq!(
                        stored.get(offset),
                        Entry::Known(expected),
                        "({x}, {z}) {offset}"
                    );
                }
            }
        }
    }
    assert_eq!(report.chunks(), 36);
    assert_eq!((report.block_entities(), report.entities()), (2, 2));
    assert_eq!(report.ignored_tags().get(&0x33), Some(&3));
    assert_eq!(report.ignored_tags().get(&0x2b), Some(&36));
    assert!(
        report.unknown_blocks().is_empty(),
        "{:?}",
        report.unknown_blocks()
    );
    assert!(report.other_keys() > 0);
}

#[test]
fn chunks_a_game_saved_import_with_every_block_entity_whole() {
    let dir = crate::save::tests::TempDir::new("import-continental-run");
    let world = dir.0.join("world");
    crate::import::tests::write_world(&world, &crate::import::tests::continental());
    let data = dir.0.join("data");
    let report = run(
        &data,
        Start::checked(StartBackup::Make),
        &[spec()],
        2,
        &Registry::default(),
        &world,
        NAME,
    )
    .unwrap();
    assert_eq!(report.chunks(), 2);
    assert_eq!((report.block_entities(), report.entities()), (8, 3));
    assert_eq!(report.ignored_tags().get(&0x2b), Some(&2));
    let mut source = Source::open(&world).unwrap();
    let mut storage = Storage::open(&data, Start::checked(StartBackup::Make)).unwrap();
    let record = world_record(&mut storage).unwrap();
    for (x, z) in [(-198, 8), (-5, -44)] {
        let raw = chunk::decode(&source.read(x, z).unwrap()).unwrap();
        let (chunk, [block_entities, entities, updates]) =
            read_chunk(&mut storage, record.number, x, z);
        // Every compound whole (the signs' text, the pot's plant, the furnace's state), upgraded.
        let mut expected = raw.block_entities.clone();
        for c in &mut expected {
            upgrade::nbt(c);
        }
        assert_eq!(block_entities, expected);
        assert!(entities.is_empty() && updates.is_empty());
        assert_eq!(chunk.biome(2, 50, 3), Some(21));
        for section in &raw.sections {
            let sub = chunk
                .sub_chunk(usize::try_from(i32::from(section.y) + 4).unwrap())
                .unwrap();
            let Layer::Paletted(layer) = &section.layers[0] else {
                panic!("the game writes paletted layers");
            };
            let indices: Vec<u16> = (0..u16::try_from(layer.palette.len()).unwrap()).collect();
            let cube = Paletted::from_parts(layer.bits, &layer.words, &indices).unwrap();
            for offset in 0..SLOTS {
                let entry = &layer.palette[usize::from(cube.get(offset))];
                let state = upgrade::state(entry).unwrap();
                let stored = sub.layers()[0].get(offset);
                match save::find_state(&state) {
                    Some(known) => assert_eq!(stored, Entry::Known(known), "({x}, {z}) {offset}"),
                    None => assert!(matches!(stored, Entry::Unknown(_)), "({x}, {z}) {offset}"),
                }
            }
        }
    }
}
