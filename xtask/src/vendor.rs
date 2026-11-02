//! `cargo xtask vendor-test str0m`: the str0m copy's full test suite, run when `vendor/str0m`
//! changes; `verify` runs only its library tests. The integration tests need seven
//! capture files the published crate leaves out and the repository does not commit: they are
//! downloaded from the upstream commit of the copy into `.cache/`, checked against the SHA-256
//! list in `xtask/data/vendor/`, copied into the copy's `tests/data/` for the run and removed
//! after it.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::dump::upgrade::{hex, sha256};
use crate::dump::{create_dir, is_file, read, remove_file, rename, write};
use crate::{STR0M, STR0M_DIMPL, STR0M_PROTO, VERIFY_TARGET, cargo, target_dir};

/// The download cache, git-ignored.
const CACHE: &str = ".cache/str0m-test-data";
/// Where the tests read the captures, with `include_bytes!` and a path relative to their file.
const DATA: &str = "vendor/str0m/tests/data";
/// `<sha256>  <file name>` per line, the `sha256sum` format.
const LIST: &str = "xtask/data/vendor/str0m-test-data.sha256";
/// The upstream commit of the copy, which cargo recorded when it packaged the crate.
const VCS_INFO: &str = "vendor/str0m/.cargo_vcs_info.json";

#[derive(Deserialize)]
struct VcsInfo {
    git: Git,
}

#[derive(Deserialize)]
struct Git {
    sha1: String,
}

/// Downloads (when not cached) and checks the captures, then runs every str0m test with
/// Anchor's features. Returns whether the tests passed.
pub(crate) fn str0m(root: &Path) -> Result<bool, String> {
    let vcs: VcsInfo = serde_json::from_slice(&read(&root.join(VCS_INFO))?)
        .map_err(|e| format!("{VCS_INFO}: {e}"))?;
    let list = String::from_utf8(read(&root.join(LIST))?).map_err(|e| format!("{LIST}: {e}"))?;
    let cache = root.join(CACHE);
    create_dir(&cache)?;
    let mut files = Vec::new();
    for line in list.lines() {
        let (hash, name) = line
            .split_once("  ")
            .ok_or_else(|| format!("{LIST}: `{line}` is not `<sha256>  <name>`"))?;
        let cached = cache.join(name);
        if !is_file(&cached) {
            download(
                &format!(
                    "https://raw.githubusercontent.com/algesten/str0m/{}/tests/data/{name}",
                    vcs.git.sha1
                ),
                &cached,
            )?;
        }
        let bytes = read(&cached)?;
        if hex(&sha256(&bytes)) != hash {
            return Err(format!(
                "{}: the SHA-256 differs from {LIST}; delete the file to download it again",
                cached.display()
            ));
        }
        files.push((root.join(DATA).join(name), bytes));
    }
    let mut copied = Vec::with_capacity(files.len());
    let passed = copy_and_test(root, &files, &mut copied);
    let removed = remove_files(&copied);
    let passed = passed?;
    removed?;
    Ok(passed)
}

/// Copies the captures `files` into the copy and runs its tests, listing each path in `copied`
/// before its write, so a write that fails halfway is removed too.
fn copy_and_test<'a>(
    root: &Path,
    files: &'a [(PathBuf, Vec<u8>)],
    copied: &mut Vec<&'a Path>,
) -> Result<bool, String> {
    for (path, bytes) in files {
        copied.push(path);
        write(path, bytes)?;
    }
    test(root)
}

/// Removes the files `paths`, stopping at the first failure.
fn remove_files(paths: &[&Path]) -> Result<(), String> {
    for path in paths {
        remove_file(path)?;
    }
    Ok(())
}

/// Runs the library and integration tests of the copy. The test-export feature opens the
/// internals the integration tests use; the patches keep str0m's registry dev-dependencies on
/// the copies, as in `verify`.
fn test(root: &Path) -> Result<bool, String> {
    Command::new(cargo())
        .args([
            "test",
            "--manifest-path",
            STR0M,
            "--lib",
            "--tests",
            "--no-default-features",
            "--features",
            "aws-lc-rs,_internal_test_exports",
            "--config",
            STR0M_DIMPL,
            "--config",
            STR0M_PROTO,
        ])
        .current_dir(root)
        // `verify`'s target directory, which already holds the copy's dependencies.
        .env("CARGO_TARGET_DIR", target_dir(VERIFY_TARGET))
        .status()
        .map(|status| status.success())
        .map_err(|e| format!("cannot run cargo: {e}"))
}

/// Downloads `url` to `path` with curl (in Windows 10 and later, and in Git for Windows). The
/// file gets its name only when the download is whole, so a cut one is never taken for a cached
/// file.
fn download(url: &str, path: &Path) -> Result<(), String> {
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let status = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--output",
        ])
        .arg(&part)
        .arg(url)
        .status()
        .map_err(|e| format!("cannot run curl ({e}); install curl and put it on PATH"))?;
    if !status.success() {
        return Err(format!("downloading {url} failed ({status})"));
    }
    rename(&part, path)
}
