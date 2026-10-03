//! `cargo xtask dump [--check]`: rebuilds the committed block and item data (PLAN.md 10.4) from
//! the pinned external checkouts in `.cache/`. It runs PMMP's own registries through
//! `xtask/php/dump.php` with the PMMP PHP binary into `xtask/data/pmmp/`, and converts the
//! reference lists of dragonfly and `CloudburstMC` with `xtask/go/reference` into
//! `xtask/data/reference/`. With `--check` it writes into `target/` and fails if a committed file
//! differs. `verify` does not run it: it needs the checkouts, the PHP binary and Go, and its inputs
//! never change (`gen --check` checks the code made from the committed data instead).

use std::path::Path;
use std::process::Command;

/// The pinned checkouts, relative to the repository root, and their commits.
const PMMP: (&str, &str) = (".cache/pmmp", "6a7cc02e9dff59b69241aa0bcffdb9903ce86beb");
const DRAGONFLY: (&str, &str) = (
    ".cache/dragonfly",
    "4c7b5074be94fa83a1cd98e9c752083ad04a6e21",
);
const CLOUDBURST: (&str, &str) = (
    ".cache/cloudburst-data",
    "659ce1e2eee3a67045693f4fd5515c6ccf571953",
);
/// The PMMP PHP binary (pmmp/PHP-Binaries php-build-524, PHP 8.4 PM5 for Windows): plain PHP
/// lacks the extensions PMMP's registries load.
const PHP: &str = ".cache/php-pm5/bin/php/php.exe";
/// Output directories and their files, relative to the repository root.
const PMMP_OUT: &str = "xtask/data/pmmp";
const PMMP_FILES: &[&str] = &["blocks.json", "items.json", "creative.json", "recipes.json"];
const REFERENCE_OUT: &str = "xtask/data/reference";
const REFERENCE_FILES: &[&str] = &["blocks.json", "items.json", "item_blocks.json"];

/// Runs `dump`; with `check`, compares instead of writing. Returns the differing files on a
/// failed check.
pub(crate) fn run(root: &Path, check: bool) -> Result<Vec<String>, String> {
    for (dir, commit) in [PMMP, DRAGONFLY, CLOUDBURST] {
        pinned(root, dir, commit)?;
    }
    if !is_dir(&root.join(PMMP.0).join("vendor")) {
        return Err(format!(
            "{}/vendor is missing; run `composer install --no-dev --ignore-platform-reqs` there",
            PMMP.0
        ));
    }
    let (pmmp_out, reference_out) = if check {
        let dir = root.join("target").join("xtask-dump");
        (dir.join("pmmp"), dir.join("reference"))
    } else {
        (root.join(PMMP_OUT), root.join(REFERENCE_OUT))
    };
    create_dir(&pmmp_out)?;
    create_dir(&reference_out)?;

    let status = Command::new(root.join(PHP))
        .args(["-d", "memory_limit=4G"])
        .arg(root.join("xtask").join("php").join("dump.php"))
        .arg(&pmmp_out)
        .current_dir(root.join(PMMP.0))
        .status()
        .map_err(|e| format!("cannot run {PHP} ({e}); see PLAN.md 10.4"))?;
    if !status.success() {
        return Err(format!("the PMMP dump failed ({status})"));
    }
    let status = Command::new("go")
        .args(["run", "./reference", "-dragonfly"])
        .arg(root.join(DRAGONFLY.0))
        .arg("-cloudburst")
        .arg(root.join(CLOUDBURST.0))
        .arg("-out")
        .arg(&reference_out)
        .current_dir(root.join("xtask").join("go"))
        .status()
        .map_err(|e| format!("cannot run `go` ({e}); install Go and put it on PATH"))?;
    if !status.success() {
        return Err(format!("the reference conversion failed ({status})"));
    }

    let mut differ = Vec::new();
    if check {
        for (committed, made, files) in [
            (PMMP_OUT, &pmmp_out, PMMP_FILES),
            (REFERENCE_OUT, &reference_out, REFERENCE_FILES),
        ] {
            for file in files {
                if read(&root.join(committed).join(file)).ok() != Some(read(&made.join(file))?) {
                    differ.push(format!("{committed}/{file}"));
                }
            }
        }
    }
    Ok(differ)
}

/// Fails unless the git checkout `dir` is at `commit`.
fn pinned(root: &Path, dir: &str, commit: &str) -> Result<(), String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root.join(dir))
        .output()
        .map_err(|e| format!("cannot run git in {dir}: {e}"))?;
    let head = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || head.trim() != commit {
        return Err(format!(
            "{dir} must be a checkout of commit {commit} (PLAN.md 10.4), found {}",
            head.trim()
        ));
    }
    Ok(())
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn is_dir(path: &Path) -> bool {
    path.is_dir()
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn create_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))
}
