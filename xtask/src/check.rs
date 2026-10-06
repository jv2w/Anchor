//! `cargo xtask check`: what every change passes before a task ends: the format, clippy, and the
//! tests of what changed. Those are the library tests of the changed modules (a file's own module
//! when it has submodules, else the module that owns its directory, so `foo/bar.rs` and
//! `foo/tests.rs` run `foo::`), every library test when `src/lib.rs` or a build file changed, and
//! each changed integration test file. `cargo xtask verify` runs everything,
//! when the user asks for it (`/verify`).
//!
//! The builds go to `target/check`, so a check neither waits on a running `verify` nor rebuilds
//! the running xtask.exe. A pass is remembered as a stamp of HEAD and the changed files, so an
//! unchanged tree passes at once.
//!
//! `--hook` is the Claude Code Stop hook: it reads the hook input on standard input and reports a
//! failure on standard error with exit code 2, which keeps the session going. On the hook's own
//! retry (`stop_hook_active`) with nothing changed since that failure it lets the session end, so
//! a failure the session cannot fix does not loop.

use std::collections::BTreeSet;
use std::env;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, ExitCode};

use crate::{CLIPPY, FMT, cargo_step};

/// Files other than Rust sources whose change can break the build or the lints.
const BUILD_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    ".clippy.toml",
    ".rustfmt.toml",
    "rust-toolchain.toml",
];
/// The build directory under `target/`.
const TARGET: &str = "check";
/// The stamps of the last pass and the last failure.
const PASS: &str = "target/check/pass";
const FAIL: &str = "target/check/fail";

/// Runs the check (`hook`: as the Stop hook).
pub(crate) fn command(root: &Path, hook: bool) -> io::Result<ExitCode> {
    let retry = hook && stop_hook_active()?;
    let files = changed(root)?;
    let stamp = stamp(root, &files)?;
    if read_stamp(root, PASS).as_deref() == Some(stamp.as_str()) {
        return Ok(ExitCode::SUCCESS);
    }
    if retry && read_stamp(root, FAIL).as_deref() == Some(stamp.as_str()) {
        return Ok(ExitCode::SUCCESS);
    }
    let mut report = Vec::new();
    let passed = check(root, &files, &mut report)?;
    write_stamp(root, if passed { PASS } else { FAIL }, &stamp)?;
    if hook && !passed {
        let mut err = stderr();
        err.write_all(&report)?;
        writeln!(
            err,
            "cargo xtask check failed: fix it before ending the task, or tell the user what fails \
             and why"
        )?;
        return Ok(ExitCode::from(2));
    }
    stdout().write_all(&report)?;
    Ok(if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Runs the steps the changed `files` need, stopping at the first failure, and writes their
/// lines and `PASS` or `FAIL` to `out`.
fn check(root: &Path, files: &[String], out: &mut Vec<u8>) -> io::Result<bool> {
    let plan = Plan::new(root, files);
    if !plan.code {
        writeln!(out, "no code changed\nPASS")?;
        return Ok(true);
    }
    // `CARGO` is the cargo that launched this task, so every step uses the same toolchain.
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut steps: Vec<Vec<String>> = vec![args(FMT), args(CLIPPY)];
    steps.extend(plan.tests());
    for step in &steps {
        let step: Vec<&str> = step.iter().map(String::as_str).collect();
        if !cargo_step(out, &cargo, &step, TARGET)? {
            writeln!(out, "FAIL")?;
            return Ok(false);
        }
    }
    writeln!(out, "PASS")?;
    Ok(true)
}

/// What the changed files need run.
#[derive(Debug, Default)]
struct Plan {
    /// Whether any file the build reads changed.
    code: bool,
    /// Every library test instead of the filters.
    all_lib: bool,
    /// Library test name filters (`module::`).
    lib: BTreeSet<String>,
    /// Changed `tests/` files by test name; `*` for a file that is no test of its own.
    anchor_tests: BTreeSet<String>,
    /// Whether xtask's own sources or manifest changed: its unit tests.
    xtask_unit: bool,
    /// Changed `xtask/tests/` files, as `anchor_tests`.
    xtask_tests: BTreeSet<String>,
}

impl Plan {
    fn new(root: &Path, files: &[String]) -> Self {
        let mut plan = Self::default();
        for file in files.iter().filter(|f| is_code(f)) {
            plan.code = true;
            if !is_rust(file) {
                if file.starts_with("xtask/") {
                    plan.xtask_unit = true;
                } else {
                    plan.all_lib = true;
                }
            } else if let Some(rel) = file.strip_prefix("src/") {
                match module(root, rel) {
                    Some(module) => {
                        plan.lib.insert(format!("{module}::"));
                    }
                    None => plan.all_lib = true,
                }
            } else if let Some(rel) = file.strip_prefix("tests/") {
                plan.anchor_tests.insert(test_name(rel));
            } else if file.starts_with("xtask/src/") {
                plan.xtask_unit = true;
            } else if let Some(rel) = file.strip_prefix("xtask/tests/") {
                plan.xtask_tests.insert(test_name(rel));
            }
            // Clippy builds `examples/` and the vendor copies Anchor uses; `verify` tests them.
        }
        plan
    }

    /// The test commands, after the format and clippy.
    fn tests(&self) -> Vec<Vec<String>> {
        let mut steps = Vec::new();
        if self.all_lib || !self.lib.is_empty() {
            let mut step = args(&["test", "-p", "anchor", "--lib", "--quiet"]);
            if !self.all_lib {
                step.push("--".to_owned());
                step.extend(self.lib.iter().cloned());
            }
            steps.push(step);
        }
        steps.extend(targets("anchor", &self.anchor_tests));
        if self.xtask_unit {
            steps.push(args(&["test", "-p", "xtask", "--bin", "xtask", "--quiet"]));
        }
        steps.extend(targets("xtask", &self.xtask_tests));
        steps
    }
}

/// `cargo test` of the changed integration test files of `package`, if any.
fn targets(package: &str, tests: &BTreeSet<String>) -> Option<Vec<String>> {
    if tests.is_empty() {
        return None;
    }
    let mut step = args(&["test", "-p", package, "--quiet"]);
    let names: Vec<&str> = if tests.contains("*") {
        vec!["*"]
    } else {
        tests.iter().map(String::as_str).collect()
    };
    for name in names {
        step.push("--test".to_owned());
        step.push(name.to_owned());
    }
    Some(step)
}

/// Whether the build reads `file`: a Rust source or a build file.
fn is_code(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    is_rust(file) || file.starts_with(".cargo/") || BUILD_FILES.contains(&name)
}

fn is_rust(file: &str) -> bool {
    Path::new(file).extension().is_some_and(|e| e == "rs")
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|a| (*a).to_owned()).collect()
}

/// The library module a source file (`rel`, without `src/`) is tested with: its own module when
/// it has submodules, else the module that owns its directory, else a top-level module's own;
/// `None` for the crate root, whose change runs every test.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn module(root: &Path, rel: &str) -> Option<String> {
    let src = root.join("src");
    let stem = rel.strip_suffix(".rs").unwrap_or(rel);
    if stem == "lib" {
        return None;
    }
    if src.join(stem).is_dir() {
        return Some(stem.replace('/', "::"));
    }
    let mut dir = Path::new(stem).parent();
    while let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
        let name = d.to_string_lossy().replace('\\', "/");
        if src.join(format!("{name}.rs")).is_file() {
            return Some(name.replace('/', "::"));
        }
        dir = d.parent();
    }
    Some(stem.replace('/', "::"))
}

/// The test target a changed file under a `tests/` directory belongs to: its name for a
/// top-level file, `*` (every test) for a shared support file.
fn test_name(rel: &str) -> String {
    match rel.strip_suffix(".rs") {
        Some(name) if !name.contains('/') => name.to_owned(),
        _ => "*".to_owned(),
    }
}

/// The files that differ from HEAD (changed, staged, deleted or untracked), as repository paths.
fn changed(root: &Path) -> io::Result<Vec<String>> {
    let mut files = Vec::new();
    for list in [
        git(root, &["diff", "--name-only", "-z", "HEAD"])?,
        git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?,
    ] {
        files.extend(
            list.split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .map(|p| String::from_utf8_lossy(p).into_owned()),
        );
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// A stamp of HEAD and the changed code files with their contents (FNV-1a): equal stamps mean
/// the same code.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn stamp(root: &Path, files: &[String]) -> io::Result<String> {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut add = |bytes: &[u8]| {
        for b in bytes {
            hash = (hash ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    add(&git(root, &["rev-parse", "HEAD"])?);
    for file in files.iter().filter(|f| is_code(f)) {
        add(file.as_bytes());
        match std::fs::read(root.join(file)) {
            Ok(bytes) => {
                add(&[1]);
                add(&bytes);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => add(&[0]),
            Err(e) => return Err(e),
        }
    }
    Ok(format!("{hash:016x}"))
}

/// The output of `git <args>` at the repository root.
fn git(root: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let output = Command::new("git").args(args).current_dir(root).output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn read_stamp(root: &Path, path: &str) -> Option<String> {
    std::fs::read_to_string(root.join(path)).ok()
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn write_stamp(root: &Path, path: &str, stamp: &str) -> io::Result<()> {
    let path = root.join(path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, stamp)
}

/// Whether the Stop hook input on standard input says the hook already stopped the session once.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn stop_hook_active() -> io::Result<bool> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let value: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
    Ok(value
        .get("stop_hook_active")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false))
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn stdout() -> io::StdoutLock<'static> {
    io::stdout().lock()
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn stderr() -> io::StderrLock<'static> {
    io::stderr().lock()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(files: &[&str]) -> Plan {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let files: Vec<String> = files.iter().map(|f| (*f).to_owned()).collect();
        Plan::new(root, &files)
    }

    #[test]
    fn a_source_file_runs_the_tests_of_its_module() {
        let p = plan(&[
            "src/tick/block_entity/decor.rs",
            "src/tick/interact/tests.rs",
            "src/import/api/tests.rs",
            "src/save.rs",
            "src/math.rs",
            "src/crypto/testdata/encryption.json",
        ]);
        assert!(p.code && !p.all_lib);
        let lib: Vec<&str> = p.lib.iter().map(String::as_str).collect();
        assert_eq!(
            lib,
            [
                "import::api::",
                "math::",
                "save::",
                "tick::block_entity::",
                "tick::interact::tests::"
            ]
        );
    }

    #[test]
    fn the_crate_root_or_a_build_file_runs_every_library_test() {
        assert!(plan(&["src/lib.rs"]).all_lib);
        assert!(plan(&["Cargo.toml"]).all_lib);
        let docs = plan(&[".claude/docs/data.md", "README.md"]);
        assert!(!docs.code && docs.tests().is_empty());
    }

    #[test]
    fn a_changed_test_file_runs_its_target_and_a_support_file_every_target() {
        let p = plan(&[
            "tests/config.rs",
            "xtask/tests/spawn.rs",
            "xtask/src/check.rs",
        ]);
        assert_eq!(
            p.tests(),
            [
                args(&["test", "-p", "anchor", "--quiet", "--test", "config"]),
                args(&["test", "-p", "xtask", "--bin", "xtask", "--quiet"]),
                args(&["test", "-p", "xtask", "--quiet", "--test", "spawn"]),
            ]
        );
        let support = plan(&["tests/support/world.rs", "tests/world.rs"]);
        assert_eq!(
            support.tests(),
            [args(&["test", "-p", "anchor", "--quiet", "--test", "*"])]
        );
    }
}
