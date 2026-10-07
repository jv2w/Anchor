//! The gophertunnel-based bot (`xtask/go/bot`): built once per test process.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Builds the bot into `dir`, the test binary's folder (the same on every call), on first use and
/// returns its path. Fails the test if Go is missing.
pub(crate) fn binary(dir: &Path) -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        create_dir(dir);
        let exe = dir.join(format!("bot{}", std::env::consts::EXE_SUFFIX));
        let output = Command::new("go")
            .args(["build", "-o"])
            .arg(&exe)
            .arg("./bot")
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("go"))
            .output()
            .unwrap_or_else(|e| panic!("cannot run `go` (the bot tests need Go on PATH): {e}"));
        assert!(
            output.status.success(),
            "go build ./bot failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        exe
    })
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
fn create_dir(path: &Path) {
    std::fs::create_dir_all(path).expect("create a test directory");
}
