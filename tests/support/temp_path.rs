//! A temporary folder for the tests that run a server on a data folder.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// A folder in Cargo's test temporary folder, unique to this process and test, removed when
/// dropped.
pub(crate) struct TempPath(pub(crate) PathBuf);

impl TempPath {
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test's own temporary folder"
    )]
    pub(crate) fn new(name: &str) -> Self {
        let path =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{}-{name}", std::process::id()));
        if let Err(e) = std::fs::remove_dir_all(&path) {
            assert_eq!(e.kind(), ErrorKind::NotFound, "clearing {path:?}");
        }
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
