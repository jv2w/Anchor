//! The world folder reader: the official server fixture, the catalog, chunk reads, the source check, and its
//! file errors and limits.

use std::collections::BTreeMap;

use super::chunk::{CHUNK_TAG_OLD_VERSION, CHUNK_TAG_VERSION, Layer, MAX_CHUNK_VERSION, prefix};
use super::*;
use crate::block::Kind;
use crate::export::chunk::{DATA_3D, SAME_BIOMES, values};
use crate::import::chunk::tests::key;
use crate::import::leveldb::tests::database_files;
use crate::nbt::{Compound, Value};
use crate::save::tests::TempDir;
use crate::storage::codec::tests::block;
use crate::world::{Chunk, ChunkPos};

/// The world the official server 1.26.52 made: a flat world, seed 7, with blocks filled, water placed, a chest, a
/// sign, a pig, an armor stand and pending ticks over five runs, so it has a table and a live log.
pub(super) const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/xtask/data/import/flat");

/// Every file under `dir` with its bytes.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: reads test fixtures on disk"
)]
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(snapshot(&path));
        } else {
            files.insert(path.clone(), std::fs::read(&path).unwrap());
        }
    }
    files
}

/// The string `name` of `compound`.
fn text(compound: &crate::nbt::Compound, name: &str) -> String {
    match compound.get(name) {
        Some(crate::nbt::Value::String(text)) => text.clone(),
        other => panic!("{name}: {other:?}"),
    }
}

#[test]
fn the_official_fixture_reads_as_raw_chunks_without_writing() {
    let before = snapshot(Path::new(FIXTURE));
    let mut source = Source::open(Path::new(FIXTURE)).unwrap();
    assert_eq!(source.world().spawn, [0, 32_767, 0]);
    let catalog = source.catalog().unwrap();
    assert_eq!(catalog.chunks.len(), 36);
    assert!(catalog.chunks.is_sorted());
    assert_eq!(
        (catalog.nether_chunks, catalog.end_chunks, catalog.players),
        (0, 0, 0)
    );
    assert!(catalog.other_keys > 0);
    // A pig and an armor stand, not imported.
    assert_eq!(catalog.entities, 2);
    let (mut block_entities, mut ticks) = (Vec::new(), 0);
    for &(x, z) in &catalog.chunks {
        let raw = chunk::decode(&source.read(x, z).unwrap()).unwrap();
        assert!(!raw.sections.is_empty());
        for section in &raw.sections {
            assert_eq!(section.version, 9);
            assert!(matches!(section.layers[0], Layer::Paletted(_)));
        }
        // Biomes are not read.
        assert!(raw.ignored.contains(&0x2b));
        block_entities.extend(raw.block_entities.iter().map(|e| text(e, "id")));
        ticks += u32::from(raw.ignored.contains(&0x33));
    }
    block_entities.sort();
    assert_eq!(block_entities, ["Chest", "Sign"]);
    // Chunks with pending ticks, not imported.
    assert_eq!(ticks, 3);
    source.unchanged().unwrap();
    assert_eq!(snapshot(Path::new(FIXTURE)), before, "the source changed");
}

/// Writes a world folder whose database holds `writes`.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: writes a test world folder"
)]
pub(super) fn write_world(dir: &Path, writes: &[(Vec<u8>, Option<Vec<u8>>)]) {
    std::fs::create_dir_all(dir.join("db")).unwrap();
    for (name, bytes) in database_files(writes) {
        std::fs::write(dir.join("db").join(name), bytes).unwrap();
    }
    let level_dat = level_dat::tests::encode(&level_dat::tests::root());
    std::fs::write(dir.join("level.dat"), level_dat).unwrap();
}

fn put(key: Vec<u8>, value: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    (key, Some(value.to_vec()))
}

#[test]
fn the_catalog_lists_overworld_chunks_in_numeric_order_and_counts_the_rest() {
    let dir = TempDir::new("import-catalog");
    write_world(
        &dir.0,
        &[
            put(key(0, 0, &[0x2c]), &[42]),
            put(key(0, 0, &[0x2f, 0]), &[9]),
            put(key(-1, 5, &[0x76]), &[7]),
            put(key(2, 2, &[0x2c]), &[42]),
            put(key(2, 2, &[0x76]), &[42]),
            put(key(0, 0, &[1, 0, 0, 0, 0x2c]), &[42]),
            put(key(3, 0, &[2, 0, 0, 0, 0x2c]), &[42]),
            put(key(3, 0, &[7, 0, 0, 0, 0x76]), &[42]),
            put(b"~local_player".to_vec(), b""),
            put(b"player_server_00000000".to_vec(), b""),
            put(b"scoreboard".to_vec(), b""),
            put([&b"digp"[..], &chunk::prefix(0, 0)].concat(), &[1; 16]),
            put([&b"actorprefix"[..], &[1; 8]].concat(), b""),
            put([&b"actorprefix"[..], &[2; 8]].concat(), b""),
            (key(5, 5, &[0x2c]), None),
        ],
    );
    let mut source = Source::open(&dir.0).unwrap();
    let catalog = source.catalog().unwrap();
    assert_eq!(
        catalog,
        Catalog {
            chunks: vec![(-1, 5), (0, 0), (2, 2)],
            nether_chunks: 1,
            end_chunks: 1,
            other_dimension_chunks: 1,
            entities: 2,
            players: 2,
            other_keys: 1,
        }
    );
}

#[test]
fn a_read_takes_the_chunks_keys_only() {
    let dir = TempDir::new("import-read");
    let block_entity = |id: &str| {
        let compound = crate::nbt::Compound::from([(
            "id".to_owned(),
            crate::nbt::Value::String(id.to_owned()),
        )]);
        chunk::tests::nbt(&compound)
    };
    write_world(
        &dir.0,
        &[
            put(key(0, 0, &[0x2c]), &[42]),
            put(key(0, 0, &[0x31]), &block_entity("own")),
            put(key(0, 0, &[1, 0, 0, 0, 0x31]), &block_entity("nether")),
            put([&b"digp"[..], &chunk::prefix(0, 0)].concat(), &[1; 8]),
            put([&b"actorprefix"[..], &[1; 8]].concat(), &[0xff]),
            put(key(0, 1, &[0x2c]), &[42]),
            put(key(0, 1, &[0x31]), &block_entity("neighbour")),
            put(key(-1, -1, &[0x2c]), &[42]),
            put(key(-1, -1, &[0x31]), &block_entity("corner")),
        ],
    );
    let mut source = Source::open(&dir.0).unwrap();
    let ids_of = |raw: chunk::Raw| -> Vec<String> {
        raw.block_entities.iter().map(|e| text(e, "id")).collect()
    };
    let raw = chunk::decode(&source.read(0, 0).unwrap()).unwrap();
    assert!(raw.ignored.is_empty());
    assert_eq!(ids_of(raw), vec!["own"]);
    let neighbour = chunk::decode(&source.read(0, 1).unwrap()).unwrap();
    assert_eq!(ids_of(neighbour), vec!["neighbour"]);
    // The chunk whose keys end the key space.
    let corner = chunk::decode(&source.read(-1, -1).unwrap()).unwrap();
    assert_eq!(ids_of(corner), vec!["corner"]);
}

/// An entity's little-endian NBT: a compound whose string `StorageKey` holds the bytes of `id`,
/// which are not UTF-8, as the game writes an entity's storage key.
fn entity_nbt(id: [u8; 8]) -> Vec<u8> {
    let name = b"StorageKey";
    // A compound with an empty name, then a string tag.
    let mut out = vec![10, 0, 0, 8];
    out.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&u16::try_from(id.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&id);
    // The compound's end tag.
    out.push(0);
    out
}

/// The keys and values of two chunks as the export writes them ([`values`]), changed to hold
/// values a strict reader rejects, and three entities. Chunk (-198, 8) has one sub-chunk, the
/// chunk version before the newest and the version key of old worlds besides, and a `Data3D` of
/// 25 biome storages where a reader reads 24. Chunk (-5, -44) has its first sub-chunk at y 4 and
/// 8 block entities, and the NBT of the three entities holds strings of bytes that are not UTF-8
/// ([`entity_nbt`]).
pub(super) fn strict_reader_chunks() -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    let mut writes = Vec::new();
    let mut terrain = Chunk::new();
    for x in 0..16 {
        for z in 0..16 {
            terrain
                .set_block(x, 64, z, Kind::Stone.default_state())
                .unwrap();
        }
    }
    let old_version = MAX_CHUNK_VERSION - 1;
    let first = ChunkPos::new(-198, 8);
    for (key, mut value) in values(first, &crate::save::to_record(&terrain), &[]).unwrap() {
        match key[8..] {
            [CHUNK_TAG_VERSION] => value = vec![old_version],
            // One storage more than the sub-chunks.
            [DATA_3D] => value.push(SAME_BIOMES),
            _ => {}
        }
        writes.push((key, Some(value)));
    }
    writes.push((
        key(first.x, first.z, &[CHUNK_TAG_OLD_VERSION]),
        Some(vec![old_version]),
    ));
    let second = ChunkPos::new(-5, -44);
    let mut terrain = Chunk::new();
    let mut block_entities = Vec::new();
    for (n, (kind, id)) in (1_u8..).zip([
        (Kind::SporeBlossom, "SporeBlossom"),
        (Kind::BlastFurnace, "BlastFurnace"),
        (Kind::FlowerPot, "FlowerPot"),
        (Kind::Cauldron, "Cauldron"),
        (Kind::OakSign, "Sign"),
        (Kind::OakSign, "Sign"),
        (Kind::OakSign, "Sign"),
        (Kind::SporeBlossom, "SporeBlossom"),
    ]) {
        terrain.set_block(n, 64, n, kind.default_state()).unwrap();
        let mut compound = Compound::from([
            ("id".to_owned(), Value::String(id.to_owned())),
            ("x".to_owned(), Value::Int(second.x * 16 + i32::from(n))),
            ("y".to_owned(), Value::Int(64)),
            ("z".to_owned(), Value::Int(second.z * 16 + i32::from(n))),
        ]);
        match id {
            "FlowerPot" => {
                let plant = block("minecraft:poppy", &[]);
                compound.insert("PlantBlock".to_owned(), Value::Compound(plant));
            }
            "Sign" => {
                let text =
                    Compound::from([("Text".to_owned(), Value::String(format!("sign {n}")))]);
                compound.insert("FrontText".to_owned(), Value::Compound(text));
            }
            _ => {}
        }
        block_entities.push(compound);
    }
    let listed: Vec<&Compound> = block_entities.iter().collect();
    for (key, value) in values(second, &crate::save::to_record(&terrain), &listed).unwrap() {
        writes.push((key, Some(value)));
    }
    let mut ids = Vec::new();
    for n in 1..=3 {
        let id = [0xff, 0xfe, 0x80, 0xc0, 0, 0, 0, n];
        writes.push(([&b"actorprefix"[..], &id].concat(), Some(entity_nbt(id))));
        ids.extend_from_slice(&id);
    }
    writes.push((
        [&b"digp"[..], &prefix(second.x, second.z)].concat(),
        Some(ids),
    ));
    writes
}

#[test]
fn chunks_a_strict_reader_rejects_read_with_their_blocks_and_block_entities() {
    let dir = TempDir::new("import-strict");
    write_world(&dir.0, &strict_reader_chunks());
    let mut source = Source::open(&dir.0).unwrap();
    let catalog = source.catalog().unwrap();
    assert_eq!(catalog.chunks, vec![(-198, 8), (-5, -44)]);
    assert_eq!(catalog.entities, 3);
    let raw = chunk::decode(&source.read(-198, 8).unwrap()).unwrap();
    assert_eq!(raw.sections.len(), 1);
    assert!(raw.block_entities.is_empty());
    assert_eq!(raw.ignored, vec![0x2b, 0x36, 0x76]);
    let raw = chunk::decode(&source.read(-5, -44).unwrap()).unwrap();
    assert_eq!(raw.sections[0].y, 4);
    let ids: Vec<String> = raw.block_entities.iter().map(|e| text(e, "id")).collect();
    assert_eq!(
        ids,
        [
            "SporeBlossom",
            "BlastFurnace",
            "FlowerPot",
            "Cauldron",
            "Sign",
            "Sign",
            "Sign",
            "SporeBlossom"
        ]
    );
    assert_eq!(raw.ignored, vec![0x2b, 0x36]);
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: writes a test world folder"
)]
fn copy_fixture(to: &Path) {
    std::fs::create_dir_all(to.join("db")).unwrap();
    for (path, bytes) in snapshot(Path::new(FIXTURE)) {
        let relative = path.strip_prefix(FIXTURE).unwrap();
        std::fs::write(to.join(relative), bytes).unwrap();
    }
}

#[test]
#[expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "off-tick: changes a test world folder"
)]
fn a_changed_source_fails_the_check() {
    let dir = TempDir::new("import-changed");
    copy_fixture(&dir.0);
    let source = Source::open(&dir.0).unwrap();
    source.unchanged().unwrap();
    let mut level_dat = std::fs::read(dir.0.join("level.dat")).unwrap();
    level_dat.push(0);
    std::fs::write(dir.0.join("level.dat"), &level_dat).unwrap();
    assert!(
        matches!(source.unchanged(), Err(Error::SourceChanged { path }) if path.ends_with("level.dat"))
    );

    let dir = TempDir::new("import-touched");
    copy_fixture(&dir.0);
    let source = Source::open(&dir.0).unwrap();
    let log = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.0.join("db/000021.log"))
        .unwrap();
    let modified = log.metadata().unwrap().modified().unwrap();
    log.set_modified(modified + std::time::Duration::from_secs(5))
        .unwrap();
    drop(log);
    assert!(
        matches!(source.unchanged(), Err(Error::SourceChanged { path }) if path.ends_with("000021.log"))
    );

    let dir = TempDir::new("import-added");
    copy_fixture(&dir.0);
    let source = Source::open(&dir.0).unwrap();
    std::fs::write(dir.0.join("db/LOCK"), b"").unwrap();
    assert!(
        matches!(source.unchanged(), Err(Error::SourceChanged { path }) if path.ends_with("LOCK"))
    );
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: writes a test world folder"
)]
fn a_folder_without_a_world_is_a_file_error() {
    let dir = TempDir::new("import-missing");
    let error = Source::open(&dir.0).unwrap_err();
    assert!(
        matches!(&error, Error::File { path, source } if path.ends_with("level.dat") && matches!(**source, Error::Read(_))),
        "{error:?}"
    );
    std::fs::write(
        dir.0.join("level.dat"),
        level_dat::tests::encode(&level_dat::tests::root()),
    )
    .unwrap();
    assert!(matches!(Source::open(&dir.0), Err(Error::File { .. })));
}

#[test]
fn error_messages_name_the_place() {
    let error = Error::Chunk {
        x: 1,
        z: -2,
        source: Box::new(Error::Malformed("empty sub-chunk")),
    };
    assert_eq!(error.to_string(), "overworld chunk 1 -2");
    assert_eq!(
        std::error::Error::source(&error).unwrap().to_string(),
        "malformed data: empty sub-chunk"
    );
    let key = Error::Key {
        kind: "SubChunkPrefix",
        key: "01000000feffffff2f03".to_owned(),
        source: Box::new(Error::Malformed("empty sub-chunk")),
    };
    assert_eq!(key.to_string(), "SubChunkPrefix key 01000000feffffff2f03");
    assert_eq!(
        std::error::Error::source(&key).unwrap().to_string(),
        "malformed data: empty sub-chunk"
    );
    let file = Error::File {
        path: PathBuf::from("db/000005.ldb"),
        source: Box::new(Error::Checksum),
    };
    assert_eq!(file.to_string(), "source file db/000005.ldb");
    let too_large = Error::TooLarge {
        what: "bytes in a block",
        limit: 16,
    };
    assert_eq!(too_large.to_string(), "more than 16 bytes in a block");
    let newer = Error::NewerFormat {
        what: "chunk version",
        version: 43,
        supported: 42,
    };
    assert_eq!(
        newer.to_string(),
        "chunk version 43 is newer than the supported 42"
    );
}

#[test]
fn more_than_4_million_overworld_chunks_are_refused_counting_each_chunk_once() {
    let mut catalog = Catalog {
        chunks: vec![(0, 0); MAX_CHUNKS - 1],
        ..Catalog::default()
    };
    catalog.add(&key(1, 1, &[0x2c])).unwrap();
    // The chunk's second version key, after one of its sub-chunks: the same chunk again.
    catalog.add(&key(1, 1, &[0x2f, 0])).unwrap();
    catalog.add(&key(1, 1, &[0x76])).unwrap();
    assert_eq!(catalog.chunks.len(), MAX_CHUNKS);
    let error = catalog.add(&key(1, 2, &[0x2c])).unwrap_err();
    assert!(matches!(
        error,
        Error::TooLarge {
            what: "overworld chunks",
            ..
        }
    ));
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: writes a test world folder"
)]
fn a_level_dat_over_its_limit_is_refused_before_reading() {
    let dir = TempDir::new("import-big-level-dat");
    std::fs::write(
        dir.0.join("level.dat"),
        vec![0; usize::try_from(MAX_LEVEL_DAT_BYTES).unwrap() + 1],
    )
    .unwrap();
    let error = Source::open(&dir.0).unwrap_err();
    assert!(
        matches!(&error, Error::File { source, .. } if matches!(**source, Error::TooLarge { .. })),
        "{error:?}"
    );
}
