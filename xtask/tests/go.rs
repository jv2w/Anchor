//! The Go tests of the bot, the oracle and the other tools in `xtask/go` (every check
//! runs under `cargo test`), built against gophertunnel with Anchor's corrections as every Go
//! tool is. Like the protocol tests, they need Go on PATH.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::path::Path;
use std::process::{Command, Output};

/// The output of a `go` command; failing to start fails the test.
fn run_go(command: &mut Command) -> Output {
    command
        .output()
        .unwrap_or_else(|e| panic!("cannot run `go` (the Go tests need Go on PATH): {e}"))
}

/// Fails the test with the command's output when it did not succeed.
fn assert_success(what: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{what} failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn the_go_tests_pass() {
    let go_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("go");
    let fixmod = Path::new(env!("CARGO_TARGET_TMPDIR")).join("go-tests-fixmod");
    let corrected = run_go(
        Command::new("go")
            .args(["run", "./fixmod", "-out"])
            .arg(&fixmod)
            .current_dir(&go_dir),
    );
    assert_success("go run ./fixmod", &corrected);
    let tests = run_go(
        Command::new("go")
            .arg("test")
            .arg("-modfile")
            .arg(fixmod.join("go.mod"))
            .arg("./...")
            .current_dir(&go_dir),
    );
    assert_success("go test ./...", &tests);
}
