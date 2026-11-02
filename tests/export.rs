//! The world export as a user runs it: the world BDS 1.26.52 made (`xtask/data/import/flat`)
//! imported with `Server::import_world`, exported with `Server::export_world`, and the export
//! imported again.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

#[path = "support/temp_path.rs"]
mod temp_path;

use std::num::NonZeroU16;
use std::path::Path;

use anchor::config::Threads;
use anchor::world::{BlockPos, Kind, Void};
use anchor::{Config, Error, Server, export};

use temp_path::TempPath;

/// The Bedrock world folder the tests import and export.
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/xtask/data/import/flat");
/// The name of the world the tests export.
const NAME: &str = "exported";

/// Returns a server of the data folder `data` with two CPU threads and the world [`NAME`].
fn server(data: &Path) -> Server {
    let mut config = Config::default();
    config.developer.cpu_threads = Threads::Count(NonZeroU16::new(2).expect("not zero"));
    config.developer.network_threads = Threads::Count(NonZeroU16::MIN);
    Server::builder()
        .config(config)
        .data_dir(data)
        .world(NAME, Kind::Persistent, Void)
        .build()
        .expect("the server builds")
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: reads the world files on disk"
)]
fn an_exported_world_imports_back_into_the_same_world_file() {
    let data = TempPath::new("round-trip");
    let (first, second) = (data.0.join("first"), data.0.join("second"));
    let folder = data.0.join("bedrock").join(NAME);
    let imported = server(&first)
        .import_world(NAME, FIXTURE)
        .expect("the import");
    let report = server(&first)
        .export_world(NAME, &folder)
        .expect("the export");
    assert_eq!(report.chunks(), imported.chunks());
    assert_eq!(report.block_entities(), imported.block_entities());
    // The import left the entities out, and an import makes no spills.
    assert_eq!(
        (
            report.entities(),
            report.scheduled_updates(),
            report.spills()
        ),
        (0, 0, 0)
    );
    assert!(folder.join("level.dat").exists());
    assert!(!data.0.join("bedrock").join(format!("{NAME}.tmp")).exists());

    let again = server(&second)
        .import_world(NAME, &folder)
        .expect("the import of the export");
    assert_eq!(again.chunks(), imported.chunks());
    assert_eq!(
        again.upgraded_nbt(),
        0,
        "the export is of the current version"
    );
    assert!(again.unknown_blocks().is_empty(), "{again:?}");
    // The spawn now has its height.
    assert_eq!(again.spawn(), BlockPos::new(0, -57, 0));
    let file = |data: &Path| {
        std::fs::read(data.join("worlds").join(format!("{NAME}.redb"))).expect("the world file")
    };
    assert_eq!(file(&first), file(&second), "the world files differ");

    // The folder is not written twice.
    let err = server(&first)
        .export_world(NAME, &folder)
        .expect_err("the folder is there");
    assert!(
        matches!(&err, Error::Export(export::Error::FolderExists { path }) if *path == folder),
        "{err:?}"
    );
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: checks the folders on disk"
)]
fn a_world_never_saved_has_nothing_to_export() {
    let data = TempPath::new("unsaved");
    let folder = data.0.join("bedrock");
    let err = server(&data.0)
        .export_world(NAME, &folder)
        .expect_err("no world file");
    assert!(
        matches!(&err, Error::Export(export::Error::NoWorld { name }) if name == NAME),
        "{err:?}"
    );
    assert!(!folder.exists());
}
