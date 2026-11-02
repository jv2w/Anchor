//! Exports of world files written here, read back with the import's reader.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::*;
use crate::import::Source;
use crate::import::chunk::{Layer, decode};
use crate::import::level_dat::WorldData;
use crate::nbt::{Compound, List, Value};
use crate::protocol::Rng;
use crate::save::tests::TempDir;
use crate::storage::codec::ChunkRecord;
use crate::storage::codec::tests::{empty_chunk, terrain_chunk};
use crate::storage::seal::seal;
use crate::storage::{Batch, Start, StartBackup, Storage};
use crate::world::{BlockPos, Flat, Kind};

/// The name of the world the tests export.
const NAME: &str = "w";
/// The seed of the world the tests export.
const SEED: i64 = 5;

/// A chunk to store: its terrain (`None` for none) and its block entities, entities and
/// scheduled updates.
type StoredChunk = (ChunkPos, Option<ChunkRecord>, [Vec<Compound>; 3]);

/// Returns the world record the tests store.
fn record() -> WorldRecord {
    WorldRecord {
        number: 0,
        time: 1234,
        spawn: BlockPos::new(1, 70, -3),
    }
}

/// Returns the declared worlds: [`NAME`], flat, of the seed [`SEED`].
fn worlds() -> Vec<WorldSpec> {
    vec![WorldSpec {
        name: NAME.to_owned(),
        kind: Kind::Persistent,
        generator: Arc::new(Flat),
        seed: Some(SEED),
        spawn: None,
    }]
}

/// Returns the compound of `entries`.
fn compound(entries: &[(&str, Value)]) -> Compound {
    entries
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

/// Returns a block entity in Bedrock form at (`x`, `y`, `z`).
fn chest(x: i32, y: i32, z: i32) -> Compound {
    compound(&[
        ("id", Value::String("Chest".to_owned())),
        ("x", Value::Int(x)),
        ("y", Value::Int(y)),
        ("z", Value::Int(z)),
        ("Items", Value::List(List::Compound(Vec::new()))),
    ])
}

/// Returns three chunks: one with terrain and a chest, one with a chest but no terrain
/// (generated), one with empty terrain, an entity and two scheduled updates.
fn chunks() -> Vec<StoredChunk> {
    let entity = compound(&[("identifier", Value::String("minecraft:item".to_owned()))]);
    let update = compound(&[("x", Value::Int(48))]);
    vec![
        (
            ChunkPos::new(0, 0),
            Some(terrain_chunk(&mut Rng(0xe790_0001))),
            [vec![chest(3, 70, 5)], Vec::new(), Vec::new()],
        ),
        (
            ChunkPos::new(-1, 5),
            None,
            [vec![chest(-14, -60, 81)], Vec::new(), Vec::new()],
        ),
        (
            ChunkPos::new(3, -2),
            Some(empty_chunk()),
            [Vec::new(), vec![entity], vec![update.clone(), update]],
        ),
    ]
}

/// Writes the world file of [`NAME`] in `data` with `record` and `chunks`.
fn write_world(data: &Path, record: WorldRecord, chunks: &[StoredChunk]) {
    let mut storage =
        Storage::open(&Place::world(data, NAME), Start::checked(StartBackup::Skip)).unwrap();
    let mut batch = Batch::default();
    batch.put(Key::World, seal(&Key::World.to_bytes(), &record).unwrap());
    for (pos, terrain, extras) in chunks {
        let id = save::chunk_id(record.number, *pos);
        for (sub, index) in terrain.iter().flat_map(|t| t.sub_chunks.iter()).zip(0_u8..) {
            let key = id.key(ChunkPart::SubChunk(index));
            batch.put(key.clone(), seal(&key.to_bytes(), sub).unwrap());
        }
        for (kind, list) in ChunkKind::EXTRAS.into_iter().zip(extras) {
            for (key, value) in save::seal_pages(id, kind, list).unwrap() {
                batch.put(key, value);
            }
        }
    }
    storage.commit(&batch).unwrap();
    storage.close().unwrap();
}

/// Reads every file under `dir` with its bytes, by path relative to `dir`.
#[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut folders = vec![dir.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                folders.push(path);
            } else {
                let name = path.strip_prefix(dir).unwrap().display().to_string();
                out.insert(name, std::fs::read(&path).unwrap());
            }
        }
    }
    out
}

#[test]
#[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
fn exported_chunks_read_back_as_stored() {
    let data = TempDir::new("export-chunks");
    let chunks = chunks();
    write_world(&data.0, record(), &chunks);
    let out = data.0.join("bedrock").join(NAME);
    let report = run(&data.0, &worlds(), 2, NAME, &out).unwrap();
    assert_eq!(
        report,
        Report {
            chunks: 3,
            block_entities: 2,
            entities: 1,
            scheduled_updates: 2,
            spills: 0,
        }
    );

    let mut source = Source::open(&out).unwrap();
    assert_eq!(
        *source.world(),
        WorldData {
            spawn: [1, 70, -3],
            time: 1234,
        }
    );
    let catalog = source.catalog().unwrap();
    assert_eq!(catalog.chunks, [(-1, 5), (0, 0), (3, -2)]);
    for (pos, terrain, [block_entities, _, _]) in &chunks {
        let raw = decode(&source.read(pos.x, pos.z).unwrap()).unwrap();
        assert_eq!(raw.version, 42);
        // A chunk stored without terrain has its generator's.
        let terrain = terrain
            .clone()
            .unwrap_or_else(|| save::to_record(&Chunk::generate(&Flat, SEED, *pos)));
        let expected: Vec<(i8, Vec<Layer>)> = terrain
            .sub_chunks
            .iter()
            .zip(-4_i8..)
            .filter(|(sub, _)| !sub.layers.is_empty())
            .map(|(sub, y)| (y, sub.layers.iter().cloned().map(Layer::Paletted).collect()))
            .collect();
        let read: Vec<(i8, Vec<Layer>)> = raw
            .sections
            .into_iter()
            .map(|s| {
                assert_eq!(s.version, 9);
                (s.y, s.layers)
            })
            .collect();
        assert!(!expected.is_empty() || terrain == empty_chunk());
        assert_eq!(read, expected, "{pos:?}");
        assert_eq!(&raw.block_entities, block_entities, "{pos:?}");
        // Biomes and finalization are written, not read by the import.
        assert_eq!(raw.ignored, [0x2b, 0x36], "{pos:?}");
    }
    assert!(!data.0.join("bedrock").join("w.tmp").exists());
}

#[test]
fn spills_are_left_out_and_counted() {
    let data = TempDir::new("export-spills");
    let spill = compound(&[
        ("id", Value::String(SPILL_ID.to_owned())),
        ("x", Value::Int(2)),
        ("y", Value::Int(-60)),
        ("z", Value::Int(3)),
        ("Items", Value::List(List::Compound(Vec::new()))),
    ]);
    let pos = ChunkPos::new(0, 0);
    let stored = (
        pos,
        Some(empty_chunk()),
        [vec![chest(1, -60, 1), spill], Vec::new(), Vec::new()],
    );
    write_world(&data.0, record(), &[stored]);
    let out = data.0.join("out");
    let report = run(&data.0, &worlds(), 2, NAME, &out).unwrap();
    assert_eq!(
        report,
        Report {
            chunks: 1,
            block_entities: 1,
            entities: 0,
            scheduled_updates: 0,
            spills: 1,
        }
    );
    let mut source = Source::open(&out).unwrap();
    assert_eq!(source.catalog().unwrap().chunks, [(0, 0)]);
    let raw = decode(&source.read(pos.x, pos.z).unwrap()).unwrap();
    assert_eq!(raw.block_entities, [chest(1, -60, 1)]);
}

#[test]
fn messages_say_what_the_export_needs() {
    let exists = Error::FolderExists {
        path: PathBuf::from("out"),
    };
    assert_eq!(
        exists.to_string(),
        "out is already there; the export writes a new folder and takes the world by its name"
    );
    let missing = Error::NoWorld {
        name: NAME.to_owned(),
    };
    assert_eq!(missing.to_string(), "the world \"w\" has no saved state");
}

#[test]
fn one_and_many_threads_write_the_same_files() {
    let data = TempDir::new("export-threads");
    write_world(&data.0, record(), &chunks());
    let one = data.0.join("one");
    let many = data.0.join("many");
    run(&data.0, &worlds(), 1, NAME, &one).unwrap();
    run(&data.0, &worlds(), 4, NAME, &many).unwrap();
    let written = files(&one);
    assert!(written.contains_key(&format!("db{}000001.ldb", std::path::MAIN_SEPARATOR)));
    assert_eq!(written, files(&many));
}

#[test]
#[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
fn a_folder_already_there_is_left_as_it_is() {
    let data = TempDir::new("export-exists");
    write_world(&data.0, record(), &chunks());
    let out = data.0.join("out");
    std::fs::create_dir(&out).unwrap();
    std::fs::write(out.join("keep"), b"mine").unwrap();
    let e = run(&data.0, &worlds(), 2, NAME, &out).unwrap_err();
    assert!(
        matches!(e, crate::Error::Export(Error::FolderExists { ref path }) if *path == out),
        "{e:?}"
    );
    assert_eq!(std::fs::read(out.join("keep")).unwrap(), b"mine");
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 1);

    // A temporary folder an interrupted export left is not taken over either.
    let other = data.0.join("other");
    std::fs::create_dir(data.0.join("other.tmp")).unwrap();
    let e = run(&data.0, &worlds(), 2, NAME, &other).unwrap_err();
    assert!(
        matches!(e, crate::Error::Export(Error::FolderExists { .. })),
        "{e:?}"
    );
    assert!(!other.exists());
}

#[test]
#[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
fn a_world_without_a_file_or_declared_temporary_is_not_there() {
    let data = TempDir::new("export-missing");
    let out = data.0.join("out");
    let e = run(&data.0, &worlds(), 2, NAME, &out).unwrap_err();
    assert!(
        matches!(e, crate::Error::Export(Error::NoWorld { .. })),
        "{e:?}"
    );
    write_world(&data.0, record(), &chunks());
    let mut temporary = worlds();
    temporary[0].kind = Kind::Temporary;
    let e = run(&data.0, &temporary, 2, NAME, &out).unwrap_err();
    assert!(
        matches!(e, crate::Error::Export(Error::NoWorld { .. })),
        "{e:?}"
    );
    assert!(!out.exists());
}

#[test]
#[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
fn a_damaged_chunk_fails_the_export_and_leaves_no_folder() {
    let data = TempDir::new("export-damaged");
    write_world(&data.0, record(), &chunks());
    let mut storage = Storage::open(
        &Place::world(&data.0, NAME),
        Start::checked(StartBackup::Skip),
    )
    .unwrap();
    let mut batch = Batch::default();
    let id = save::chunk_id(0, ChunkPos::new(3, -2));
    batch.put(id.key(ChunkPart::SubChunk(7)), vec![1, 2, 3]);
    storage.commit(&batch).unwrap();
    storage.close().unwrap();

    let out = data.0.join("out");
    let e = run(&data.0, &worlds(), 2, NAME, &out).unwrap_err();
    assert!(
        matches!(e, crate::Error::Export(Error::Chunk { x: 3, z: -2, .. })),
        "{e:?}"
    );
    assert!(!out.exists());
    assert!(!data.0.join("out.tmp").exists());
}
