//! The world folder reader: the BDS fixture, the catalog, chunk reads, the source check, and its
//! file errors and limits.

use std::collections::BTreeMap;

use super::chunk::{Biomes, Layer};
use super::*;
use crate::import::leveldb::tests::database_files;
use crate::save::tests::TempDir;

/// The world BDS 1.26.52 made (PLAN.md 10.9): a flat world, seed 7, with blocks filled, entities
/// summoned and water placed over five runs, so it has a table and a live log.
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/xtask/data/import/flat");

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

/// Writes a world folder whose database holds `writes`.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: writes a test world folder"
)]
fn write_world(dir: &Path, writes: &[(Vec<u8>, Option<Vec<u8>>)]) {
    std::fs::create_dir_all(dir.join("db")).unwrap();
    for (name, bytes) in database_files(writes) {
        std::fs::write(dir.join("db").join(name), bytes).unwrap();
    }
    let level_dat = level_dat::tests::encode(&level_dat::tests::root());
    std::fs::write(dir.join("level.dat"), level_dat).unwrap();
}

fn key(x: i32, z: i32, suffix: &[u8]) -> Vec<u8> {
    let mut key = chunk::prefix(x, z).to_vec();
    key.extend_from_slice(suffix);
    key
}

/// The string `name` of `compound`.
fn text(compound: &crate::nbt::Compound, name: &str) -> String {
    match compound.get(name) {
        Some(crate::nbt::Value::String(text)) => text.clone(),
        other => panic!("{name}: {other:?}"),
    }
}

fn put(key: Vec<u8>, value: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    (key, Some(value.to_vec()))
}

#[test]
fn the_bds_fixture_reads_as_raw_chunks_without_writing() {
    let before = snapshot(Path::new(FIXTURE));
    let mut source = Source::open(Path::new(FIXTURE)).unwrap();
    assert_eq!(source.world().seed, 7);
    let catalog = source.catalog().unwrap();
    assert_eq!(catalog.chunks.len(), 36);
    assert!(catalog.chunks.is_sorted());
    assert_eq!(
        (catalog.nether_chunks, catalog.end_chunks, catalog.players),
        (0, 0, 0)
    );
    assert!(catalog.other_keys > 0);
    let (mut entities, mut block_entities, mut ticks) = (Vec::new(), Vec::new(), 0);
    for &(x, z) in &catalog.chunks {
        let raw = chunk::decode(&source.read(x, z).unwrap()).unwrap();
        assert_eq!(raw.version, 42);
        assert!(!raw.sections.is_empty());
        for section in &raw.sections {
            assert_eq!(section.version, 9);
            assert!(matches!(section.layers[0], Layer::Paletted(_)));
        }
        assert!(matches!(&raw.biomes, Biomes::Paletted(b) if b.len() == 24));
        entities.extend(raw.entities.iter().map(|e| text(e, "identifier")));
        block_entities.extend(raw.block_entities.iter().map(|e| text(e, "id")));
        if let Some(pending) = &raw.pending_ticks {
            assert!(matches!(
                pending.get("currentTick"),
                Some(crate::nbt::Value::Int(_))
            ));
            assert!(matches!(
                pending.get("tickList"),
                Some(crate::nbt::Value::List(_))
            ));
            ticks += 1;
        }
    }
    entities.sort();
    block_entities.sort();
    assert_eq!(entities, ["minecraft:armor_stand", "minecraft:pig"]);
    assert_eq!(block_entities, ["Chest", "Sign"]);
    assert_eq!(ticks, 3);
    source.unchanged().unwrap();
    assert!(snapshot(Path::new(FIXTURE)) == before, "the source changed");
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
            put(chunk::digest_key(0, 0), b""),
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
            players: 2,
            other_keys: 1,
        }
    );
}

#[test]
fn a_read_takes_the_chunks_keys_and_its_digp_actors() {
    let dir = TempDir::new("import-read");
    let actor = |id: &str| {
        let compound = crate::nbt::Compound::from([(
            "id".to_owned(),
            crate::nbt::Value::String(id.to_owned()),
        )]);
        chunk::tests::nbt(&compound)
    };
    let mut ids = [1_u8; 8].to_vec();
    ids.extend_from_slice(&[2; 8]);
    write_world(
        &dir.0,
        &[
            put(key(0, 0, &[0x2c]), &[42]),
            put(key(0, 0, &[0x32]), &actor("old")),
            put(key(0, 0, &[1, 0, 0, 0, 0x32]), &actor("nether")),
            put(key(0, 1, &[0x2c]), &[42]),
            put(key(0, 1, &[0x32]), &actor("neighbour")),
            put(key(-1, -1, &[0x2c]), &[42]),
            put(key(-1, -1, &[0x32]), &actor("corner")),
            put(chunk::digest_key(0, 0), &ids),
            put(chunk::actor_key([1; 8]), &actor("a")),
            put(chunk::actor_key([2; 8]), &actor("b")),
            put(key(5, 5, &[0x2c]), &[42]),
            put(chunk::digest_key(5, 5), &[3; 8]),
            put(key(6, 6, &[0x2c]), &[42]),
            put(chunk::digest_key(6, 6), &[3; 9]),
        ],
    );
    let mut source = Source::open(&dir.0).unwrap();
    let ids_of = |raw: chunk::Raw| -> Vec<String> {
        raw.entities
            .iter()
            .map(|e| match e.get("id") {
                Some(crate::nbt::Value::String(id)) => id.clone(),
                _ => panic!("no id"),
            })
            .collect()
    };
    let raw = chunk::decode(&source.read(0, 0).unwrap()).unwrap();
    assert_eq!(raw.ignored, vec![0x32]);
    assert_eq!(ids_of(raw), vec!["a", "b"]);
    let neighbour = chunk::decode(&source.read(0, 1).unwrap()).unwrap();
    assert_eq!(ids_of(neighbour), vec!["neighbour"]);
    // The chunk whose keys end the key space.
    let corner = chunk::decode(&source.read(-1, -1).unwrap()).unwrap();
    assert_eq!(ids_of(corner), vec!["corner"]);
    let missing = source.read(5, 5).unwrap_err();
    assert!(
        matches!(&missing, Error::Chunk { x: 5, z: 5, source } if matches!(**source, Error::Malformed(_))),
        "{missing:?}"
    );
    assert!(matches!(
        source.read(6, 6),
        Err(Error::Chunk { x: 6, z: 6, .. })
    ));
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
    assert_eq!(error.to_string(), "chunk 1 -2");
    assert_eq!(
        std::error::Error::source(&error).unwrap().to_string(),
        "malformed data: empty sub-chunk"
    );
    let file = Error::File {
        path: PathBuf::from("db/000005.ldb"),
        source: Box::new(Error::Checksum),
    };
    assert_eq!(file.to_string(), "world file db/000005.ldb");
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
fn more_than_4_million_overworld_chunks_are_refused() {
    let mut catalog = Catalog {
        chunks: vec![(0, 0); MAX_CHUNKS - 1],
        ..Catalog::default()
    };
    catalog.add(&key(1, 1, &[0x2c])).unwrap();
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
        vec![0; usize::try_from(MAX_LEVEL_DAT).unwrap() + 1],
    )
    .unwrap();
    let error = Source::open(&dir.0).unwrap_err();
    assert!(
        matches!(&error, Error::File { source, .. } if matches!(**source, Error::TooLarge { .. })),
        "{error:?}"
    );
}
