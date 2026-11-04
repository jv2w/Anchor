//! The gophertunnel-based bot (`xtask/go/bot`): built once per test process.

#[path = "go_build.rs"]
mod go_build;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The bot executable, built on first use.
static BINARY: OnceLock<PathBuf> = OnceLock::new();

/// Builds the bot into `dir`, the test binary's folder (the same on every call), on first use and
/// returns its path. Fails the test if Go is missing.
pub(crate) fn binary(dir: &Path) -> &'static Path {
    BINARY.get_or_init(|| {
        create_dir(dir);
        go_build::build(dir, "bot")
    })
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
fn create_dir(path: &Path) {
    std::fs::create_dir_all(path).expect("create a test directory");
}
