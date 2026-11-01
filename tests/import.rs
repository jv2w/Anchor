//! The Bedrock world import as a user runs it: `Server::import_world` on the
//! world BDS 1.26.52 made (`xtask/data/import/flat`, flat, seed 7), then the server loading it.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::collections::BTreeMap;
use std::num::{NonZeroU16, NonZeroU32};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anchor::config::Threads;
use anchor::world::{BlockPos, Kind, Void, World};
use anchor::{Config, Error, Server, import};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/xtask/data/import/flat");
const NAME: &str = "imported";
/// What the server task saw of the imported world: its seed, its spawn and the spawn column's
/// lowest block names.
type Seen = (i64, BlockPos, Vec<String>);

/// A folder in Cargo's test temporary folder, unique to this process and test, removed when
/// dropped.
struct TempPath(PathBuf);

impl TempPath {
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test's own temporary folder"
    )]
    fn new(name: &str) -> Self {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("import-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&path).unwrap_or(());
        Self(path)
    }
}

impl Drop for TempPath {
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test's own temporary folder"
    )]
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap_or(());
    }
}

/// Settings that run without the network beyond loopback, with `cpu` CPU threads. Port 0 lets
/// the system pick every port at bind time: nothing connects, and a port picked earlier and
/// released could be taken by another process first.
fn config(cpu: u16) -> Config {
    let mut config = Config::default();
    config.general.xbox_auth = false;
    config.general.enable_ipv6 = false;
    config.general.server_port = 0;
    config.developer.cpu_threads = Threads::Count(NonZeroU16::new(cpu).expect("not zero"));
    config.developer.network_threads = Threads::Count(NonZeroU16::MIN);
    config
}

fn server(data: &Path, cpu: u16) -> Server {
    Server::builder()
        .config(config(cpu))
        .data_dir(data)
        .world(NAME, Kind::Persistent, Void)
        .build()
        .expect("the server builds")
}

/// Every file under `dir` with its bytes.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: reads the fixture on disk"
)]
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(dir).expect("read the folder") {
        let path = entry.expect("a folder entry").path();
        if path.is_dir() {
            files.extend(snapshot(&path));
        } else {
            files.insert(path.clone(), std::fs::read(&path).expect("read the file"));
        }
    }
    files
}

// pmmp: FormatConverter::execute
#[test]
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: the server task fills it on the tick thread for the test thread"
)]
fn a_bds_world_imports_once_and_the_server_loads_it() {
    let data = TempPath::new("load");
    let before = snapshot(Path::new(FIXTURE));
    let report = server(&data.0, 2)
        .import_world(FIXTURE, NAME)
        .expect("the import");
    assert_eq!(report.chunks(), 36);
    assert_eq!((report.block_entities(), report.entities()), (2, 2));
    // Pending ticks and biomes are left out, counted by the chunks that had them.
    assert!(report.ignored_tags().get(&0x33).is_some_and(|&n| n > 0));
    assert_eq!(report.ignored_tags().get(&0x2b), Some(&36));
    assert!(report.unknown_blocks().is_empty(), "{report:?}");
    assert_eq!(
        (
            report.nether_chunks(),
            report.end_chunks(),
            report.players()
        ),
        (0, 0, 0)
    );
    assert!(snapshot(Path::new(FIXTURE)) == before, "the source changed");
    // level.dat's spawn (no height: above the highest block), which the world keeps.
    assert_eq!(report.spawn(), BlockPos::new(0, -57, 0));

    // A second import is refused (opening the file still backs it up, as every start does; the
    // values stay, which the crate's own test checks).
    let err = server(&data.0, 2)
        .import_world(FIXTURE, NAME)
        .expect_err("a second import");
    assert!(
        matches!(&err, Error::Import(import::Error::WorldExists { name }) if name == NAME),
        "{err:?}"
    );

    // The server runs the world with its own settings (not level.dat's seed 7) and the imported
    // spawn: its spawn chunks loaded with BDS's flat layers at the bottom.
    let seen: Arc<std::sync::Mutex<Option<Seen>>> = Arc::default();
    let out = Arc::clone(&seen);
    let server = Server::builder()
        .config(config(2))
        .data_dir(&data.0)
        .world(NAME, Kind::Persistent, Void)
        .every(NonZeroU32::MIN, move |cx| {
            let world = cx
                .worlds()
                .find(|w| w.name() == NAME)
                .expect("the imported world");
            let spawn = world.spawn();
            if !world.is_chunk_loaded(spawn.chunk()) {
                return;
            }
            let column = (-64..=-62)
                .map(|y| {
                    let pos = BlockPos::new(spawn.x, y, spawn.z);
                    world
                        .block(pos)
                        .map_or("?".to_owned(), |s| s.name().to_owned())
                })
                .collect();
            *out.lock().expect("not poisoned") = Some((World::seed(world), spawn, column));
            cx.shutdown();
        })
        .build()
        .expect("the server builds");
    server.run().expect("the server runs");
    let (world_seed, spawn, column) = seen
        .lock()
        .expect("not poisoned")
        .take()
        .expect("the spawn chunk loaded");
    assert_ne!(world_seed, 7, "level.dat's seed is a setting, not imported");
    assert_eq!(spawn, report.spawn(), "the imported spawn");
    assert_eq!(
        column,
        ["minecraft:bedrock", "minecraft:dirt", "minecraft:dirt",]
    );
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: counts the backups on disk"
)]
fn each_import_writes_its_own_world_file_and_nothing_else() {
    let data = TempPath::new("two-worlds");
    let server = || {
        Server::builder()
            .config(config(1))
            .data_dir(&data.0)
            .world(NAME, Kind::Persistent, Void)
            .build()
            .expect("the server builds")
    };
    // A declared world and one that is not: both become a world file of their own.
    for world in [NAME, "second"] {
        server().import_world(FIXTURE, world).expect("the import");
    }
    let mut files: Vec<String> = std::fs::read_dir(data.0.join("worlds"))
        .expect("the world folder")
        .map(|e| {
            e.expect("an entry")
                .file_name()
                .into_string()
                .expect("a name")
        })
        .collect();
    files.sort();
    assert_eq!(files, [format!("{NAME}.redb"), "second.redb".to_owned()]);
    let mut top: Vec<String> = std::fs::read_dir(&data.0)
        .expect("the data folder")
        .map(|e| {
            e.expect("an entry")
                .file_name()
                .into_string()
                .expect("a name")
        })
        .collect();
    top.sort();
    assert_eq!(
        top,
        ["worlds"],
        "an import writes no other data file and no backup"
    );
    // A second import of a name is refused and changes nothing.
    let err = server()
        .import_world(FIXTURE, "SECOND")
        .expect_err("the world exists");
    assert!(
        matches!(&err, Error::Import(import::Error::WorldExists { .. })),
        "{err:?}"
    );
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: reads the server files on disk"
)]
fn one_and_many_threads_write_the_same_server_file() {
    let mut files = Vec::new();
    for cpu in [1, 4] {
        let data = TempPath::new(&format!("threads-{cpu}"));
        server(&data.0, cpu)
            .import_world(FIXTURE, NAME)
            .expect("the import");
        let file = data.0.join("worlds").join(format!("{NAME}.redb"));
        files.push(std::fs::read(file).expect("the world file"));
    }
    assert!(files[0] == files[1], "the server files differ");
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: checks the data folder on disk"
)]
fn a_temporary_world_cannot_be_imported_into_in_any_letter_case() {
    let data = TempPath::new("undeclared");
    let server = Server::builder()
        .config(config(1))
        .data_dir(&data.0)
        .world("arena", Kind::Temporary, Void)
        .build()
        .expect("the server builds");
    for world in ["arena", "ARENA"] {
        let err = server
            .import_world(FIXTURE, world)
            .expect_err("a temporary world");
        assert!(
            matches!(&err, Error::Import(import::Error::UnknownWorld { name }) if name == world),
            "{err:?}"
        );
        assert!(!data.0.exists(), "nothing was written");
    }
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: checks the data folder on disk"
)]
fn a_folder_that_is_not_a_bedrock_world_fails_and_writes_no_world() {
    let data = TempPath::new("not-a-world");
    let err = server(&data.0, 1)
        .import_world(data.0.join("missing"), NAME)
        .expect_err("not a world");
    assert!(matches!(err, Error::Import(_)), "{err:?}");
    assert!(!data.0.join("worlds").join(format!("{NAME}.redb")).exists());
}

#[test]
fn the_world_database_reads_the_keys_the_import_leaves_out() {
    let before = snapshot(Path::new(FIXTURE));
    let mut db = import::Db::open(FIXTURE).expect("the database opens");
    // The import counts 2 entities: one `actorprefix` key each.
    let mut entities = Vec::new();
    db.scan_prefix(b"actorprefix", |key, value| {
        entities.push((key.to_vec(), value.to_vec()));
    })
    .expect("the scan");
    assert_eq!(entities.len(), 2);
    for (key, value) in &entities {
        assert_eq!(db.get(key).expect("the read").as_ref(), Some(value));
    }
    let missing = [b"actorprefix".as_slice(), &[0xff; 8]].concat();
    assert_eq!(db.get(&missing).expect("the read"), None);
    assert!(snapshot(Path::new(FIXTURE)) == before, "the source changed");

    let err = import::Db::open(Path::new(FIXTURE).join("missing")).expect_err("not a world");
    assert!(matches!(err, import::Error::File { .. }), "{err:?}");
}
