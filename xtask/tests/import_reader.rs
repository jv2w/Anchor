//! The Go cross-check of the world import reader: goleveldb, a LevelDB
//! reader, must read the official server's fixture world (`xtask/data/import/flat`) as
//! `xtask/data/import/flat.dump` says, and Anchor's reader is checked against that same file
//! (`import::leveldb::tests::the_fixture_reads_like_goleveldb`). Like the other Go tests,
//! it needs Go on PATH.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::path::Path;
use std::process::Command;

#[test]
fn goleveldb_reads_the_fixture_as_the_committed_dump() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new("go")
        .args(["run", "./leveldbdump", "../data/import/flat/db"])
        .current_dir(root.join("go"))
        .output()
        .unwrap_or_else(|e| panic!("cannot run `go` (the Go tests need Go on PATH): {e}"));
    assert!(
        output.status.success(),
        "leveldbdump failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    let want = std::fs::read(root.join("data/import/flat.dump")).unwrap();
    assert!(
        output.stdout == want,
        "goleveldb reads the fixture differently from xtask/data/import/flat.dump"
    );
}
