//! xtask's file layer: every command reads and writes files through it, so every error names its
//! path.

use std::path::{Path, PathBuf};

/// The bytes of the file at `path`.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The bytes of the file at `path`, or none when there is no such file; any other failure is an
/// error.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn read_if_exists(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// The text of the file at `path`; a file that is not UTF-8 is an error.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn read_text(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The text of the file at `path`, or none when there is no such file; any other failure is an
/// error.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn read_text_if_exists(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Writes `bytes` to the file at `path`, replacing what it held.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Copies the directory `from` with everything in it to `to`.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
#[expect(
    clippy::expect_used,
    reason = "invariant: the path of a directory entry ends in its name"
)]
pub(crate) fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    create_dir(to)?;
    for source in read_dir(from)? {
        let target = to.join(source.file_name().expect("a directory entry has a name"));
        if is_dir(&source) {
            copy_dir(&source, &target)?;
        } else {
            std::fs::copy(&source, &target).map_err(|e| format!("{}: {e}", source.display()))?;
        }
    }
    Ok(())
}

/// The names of the entries directly in `dir`, sorted.
pub(crate) fn file_names(dir: &Path) -> Result<Vec<String>, String> {
    Ok(read_dir(dir)?
        .iter()
        .filter_map(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect())
}

/// The paths of the entries directly in `path`, sorted.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn read_dir(path: &Path) -> Result<Vec<PathBuf>, String> {
    let error = |e: std::io::Error| format!("{}: {e}", path.display());
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(path).map_err(error)? {
        paths.push(entry.map_err(error)?.path());
    }
    paths.sort();
    Ok(paths)
}

/// Whether `path` is a directory.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn is_dir(path: &Path) -> bool {
    path.is_dir()
}

/// Whether `path` is a file.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn is_file(path: &Path) -> bool {
    path.is_file()
}

/// Creates the directory `path` and every missing parent; one that exists is not an error.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn create_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Creates the file at `path` for writing, emptying one that exists.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn create_file(path: &Path) -> Result<std::fs::File, String> {
    std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Renames `from` to `to`, replacing a file at `to`.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn rename(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to).map_err(|e| format!("{}: {e}", from.display()))
}

/// Removes the file at `path`; a file that is already gone is not an error.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn remove_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Removes the directory tree at `path`; a directory that is already gone is not an error.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
pub(crate) fn remove_dir(path: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reading a missing file as text fails with an error that starts with its path, reading it
    /// only if it exists gives none, and removing a missing directory succeeds.
    #[test]
    fn missing_paths_read_as_errors_or_none_and_remove_as_done() {
        let path = std::env::temp_dir().join(format!("anchor-xtask-fs-{}", std::process::id()));
        assert!(
            read_text(&path)
                .unwrap_err()
                .starts_with(&path.display().to_string())
        );
        assert_eq!(read_text_if_exists(&path), Ok(None));
        assert_eq!(remove_dir(&path), Ok(()));
    }
}
