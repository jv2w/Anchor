//! The Go tests of the bot, the oracle and the other tools in `xtask/go` (every check
//! runs under `cargo test`). Like the protocol tests, they need Go on PATH.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::path::Path;
use std::process::Command;

#[test]
fn the_go_tests_pass() {
    let output = Command::new("go")
        .args(["test", "./..."])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("go"))
        .output()
        .unwrap_or_else(|e| panic!("cannot run `go` (the Go tests need Go on PATH): {e}"));
    assert!(
        output.status.success(),
        "go test ./... failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
