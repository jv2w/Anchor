//! The file operations of the xtask tests; each fails the test on an error.

use std::path::Path;

/// Creates the directory `path` and every missing parent.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
pub(crate) fn create_dir(path: &Path) {
    std::fs::create_dir_all(path).expect("create a test directory");
}

/// The text of the file at `path`; empty while a child process has not created it.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
pub(crate) fn read(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => panic!("read {}: {e}", path.display()),
    }
}

/// Removes the directory tree at `path`; a directory that is already gone is fine.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
pub(crate) fn remove_dir(path: &Path) {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("remove {}: {e}", path.display()),
    }
}

/// Writes `bytes` to the file at `path`, replacing what it held.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
pub(crate) fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write a test file");
}
