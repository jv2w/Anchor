//! Builds a Go tool of `xtask/go` against gophertunnel with Anchor's corrections.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Builds `xtask/go/<tool>` into `dir` against the corrected gophertunnel and returns the
/// executable's path. Fails the test if Go is missing.
pub(super) fn build(dir: &Path, tool: &str) -> PathBuf {
    let exe = dir.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX));
    let go_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("go");
    let fixmod = dir.join("fixmod");
    let output = Command::new("go")
        .args(["run", "./fixmod", "-out"])
        .arg(&fixmod)
        .current_dir(&go_dir)
        .output()
        .unwrap_or_else(|e| panic!("cannot run `go` (the tests need Go on PATH): {e}"));
    assert!(
        output.status.success(),
        "go run ./fixmod failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new("go")
        .arg("build")
        .arg("-modfile")
        .arg(fixmod.join("go.mod"))
        .arg("-o")
        .arg(&exe)
        .arg(format!("./{tool}"))
        .current_dir(&go_dir)
        .output()
        .unwrap_or_else(|e| panic!("cannot run `go` (the tests need Go on PATH): {e}"));
    assert!(
        output.status.success(),
        "go build ./{tool} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    exe
}
