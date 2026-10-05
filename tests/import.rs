//! The Bedrock world import as a user runs it (PLAN.md 10.9): `Server::import_world` on the
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
/// What the server task saw of the imported world: its seed and the spawn column's lowest
/// block names.
type Seen = (i64, Vec<String>);

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
    config.general.nethernet_port = 0;
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
    assert!(report.scheduled_updates() > 0);
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

    // A second import is refused (opening the file still backs it up and compacts it, as every
    // start does; the values stay, which the crate's own test checks).
    let err = server(&data.0, 2)
        .import_world(FIXTURE, NAME)
        .expect_err("a second import");
    assert!(
        matches!(&err, Error::Import(import::Error::WorldExists { name }) if name == NAME),
        "{err:?}"
    );

    // The server runs the world: its seed, its spawn chunks loaded with BDS's flat
    // layers at the bottom.
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
            *out.lock().expect("not poisoned") = Some((World::seed(world), column));
            cx.shutdown();
        })
        .build()
        .expect("the server builds");
    server.run().expect("the server runs");
    let (world_seed, column) = seen
        .lock()
        .expect("not poisoned")
        .take()
        .expect("the spawn chunk loaded");
    assert_eq!(world_seed, 7);
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
fn an_import_keeps_a_backup_only_with_backups_on() {
    for (backups, kept) in [(false, 0), (true, 1)] {
        let data = TempPath::new(&format!("backups-{backups}"));
        let server = || {
            let mut config = config(1);
            config.general.backups = backups;
            Server::builder()
                .config(config)
                .data_dir(&data.0)
                .world(NAME, Kind::Persistent, Void)
                .world("second", Kind::Persistent, Void)
                .build()
                .expect("the server builds")
        };
        // The first import creates the world file, the second opens (and copies) it.
        for world in [NAME, "second"] {
            server().import_world(FIXTURE, world).expect("the import");
        }
        let files = std::fs::read_dir(data.0.join("backups"))
            .map(Iterator::count)
            .unwrap_or_default();
        assert_eq!(files, kept, "backups = {backups}");
    }
}

#[test]
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: the server task fills it on the tick thread for the test thread"
)]
fn an_imported_pending_tick_runs_at_its_world_position() {
    let data = TempPath::new("pending");
    server(&data.0, 1)
        .import_world(FIXTURE, NAME)
        .expect("the import");
    // BDS left water at (3, -58, -1), over air, with an update due 2 ticks later (chunk (0, -1)
    // stores it at those world coordinates): when it runs, the water falls.
    let below = BlockPos::new(3, -59, -1);
    let fell: Arc<std::sync::Mutex<Option<bool>>> = Arc::default();
    let out = Arc::clone(&fell);
    let mut ticks = 0;
    let server = Server::builder()
        .config(config(1))
        .data_dir(&data.0)
        .world(NAME, Kind::Persistent, Void)
        .every(NonZeroU32::MIN, move |cx| {
            let world = cx
                .worlds()
                .find(|w| w.name() == NAME)
                .expect("the imported world");
            if !world.is_chunk_loaded(below.chunk()) {
                return;
            }
            ticks += 1;
            let water = world
                .block(below)
                .is_some_and(|s| s.name() == "minecraft:water");
            if water || ticks == 100 {
                *out.lock().expect("not poisoned") = Some(water);
                cx.shutdown();
            }
        })
        .build()
        .expect("the server builds");
    server.run().expect("the server runs");
    assert_eq!(*fell.lock().expect("not poisoned"), Some(true));
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: reads the world files on disk"
)]
fn one_and_many_threads_write_the_same_world_file() {
    let mut files = Vec::new();
    for cpu in [1, 4] {
        let data = TempPath::new(&format!("threads-{cpu}"));
        server(&data.0, cpu)
            .import_world(FIXTURE, NAME)
            .expect("the import");
        files.push(std::fs::read(data.0.join("world.redb")).expect("the world file"));
    }
    assert!(files[0] == files[1], "the world files differ");
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: checks the data folder on disk"
)]
fn only_a_declared_persistent_world_can_be_imported_into() {
    let data = TempPath::new("undeclared");
    for name in ["other", "arena"] {
        let err = Server::builder()
            .config(config(1))
            .data_dir(&data.0)
            .world("arena", Kind::Temporary, Void)
            .build()
            .expect("the server builds")
            .import_world(FIXTURE, name)
            .expect_err("no such persistent world");
        assert!(
            matches!(&err, Error::Import(import::Error::UnknownWorld { name: n }) if n == name),
            "{err:?}"
        );
    }
    assert!(!data.0.exists(), "nothing was written");
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
    assert!(!data.0.join("world.redb").exists());
}
