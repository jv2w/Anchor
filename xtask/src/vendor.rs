//! `cargo xtask vendor-test str0m`: the str0m copy's full test suite, run when `vendor/str0m`
//! changes (PLAN.md 9.1); `verify` runs only its library tests. The integration tests need seven
//! capture files the published crate leaves out and the repository does not commit: they are
//! downloaded from the upstream commit of the copy into `.cache/`, checked against the SHA-256
//! list in `xtask/data/vendor/`, copied into the copy's `tests/data/` for the run and removed
//! after it.

use std::path::Path;
use std::process::Command;

use serde::Deserialize;

use crate::dump::upgrade::{hex, sha256};
use crate::dump::{create_dir, read, write};
use crate::{STR0M, STR0M_DIMPL, STR0M_PROTO};

/// `<sha256>  <file name>` per line, the `sha256sum` format.
const LIST: &str = "xtask/data/vendor/str0m-test-data.sha256";
/// The download cache, git-ignored.
const CACHE: &str = ".cache/str0m-test-data";
/// Where the tests read the captures (`include_bytes!("data/<name>")`).
const DATA: &str = "vendor/str0m/tests/data";
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
    let passed = files
        .iter()
        .try_for_each(|(path, bytes)| {
            write(path, bytes)?;
            copied.push(path);
            Ok(())
        })
        .and_then(|()| test(root));
    let removed = copied.into_iter().try_for_each(|path| remove_file(path));
    let passed = passed?;
    removed?;
    Ok(passed)
}

/// Runs the library and integration tests of the copy. The test-export feature opens the
/// internals the integration tests use; the patches keep str0m's registry dev-dependencies on
/// the copies, as in `verify`.
fn test(root: &Path) -> Result<bool, String> {
    // `CARGO` is the cargo that launched this task, so the tests use the same toolchain.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    Command::new(cargo)
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
        .env("CARGO_TARGET_DIR", root.join("target").join("verify"))
        .status()
        .map(|status| status.success())
        .map_err(|e| format!("cannot run cargo: {e}"))
}

/// Downloads `url` to `path` with curl (in Windows 10 and later, and in Git for Windows).
fn download(url: &str, path: &Path) -> Result<(), String> {
    let status = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--output",
        ])
        .arg(path)
        .arg(url)
        .status()
        .map_err(|e| format!("cannot run curl ({e}); install curl and put it on PATH"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("downloading {url} failed ({status})"))
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn is_file(path: &Path) -> bool {
    path.is_file()
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn remove_file(path: &Path) -> Result<(), String> {
    std::fs::remove_file(path).map_err(|e| format!("{}: {e}", path.display()))
}
