//! The gophertunnel-based bot (`xtask/go/bot`): built once per test process.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::fs::create_dir;
use super::process::build_go;

/// The bot executable, built on first use.
static BINARY: OnceLock<PathBuf> = OnceLock::new();

/// Builds the bot into `dir`, the test binary's folder (the same on every call), on first use and
/// returns its path; a failed build fails the test.
pub(crate) fn binary(dir: &Path) -> &'static Path {
    BINARY.get_or_init(|| {
        create_dir(dir);
        build_go(dir, "bot").unwrap_or_else(|e| panic!("{e}"))
    })
}
