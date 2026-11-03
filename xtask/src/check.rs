//! `cargo xtask check`: what every change passes before a task ends: the format, clippy, and the
//! tests of what changed. Those are the library tests of the changed modules (a file's own module
//! when it has submodules, else the module that owns its directory, so `foo/bar.rs` and
//! `foo/tests.rs` run `foo::`), every library test when `src/lib.rs` or a build file changed, and
//! each changed integration test file. A changed Rust source under `src/` or `xtask/src/` also
//! runs the two documentation builds of `verify`, and `xtask/data/dump/status.txt` or a changed
//! Rust file with an `// api:` or `// api-class:` comment runs `cargo xtask table --check`.
//!
//! A changed file that a Rust source of the repository ([`SOURCE_DIRS`]) pulls in counts as a
//! change of that source too, so it also runs that source's tests (in the source's own crate). A
//! source pulls in the file of a `#[path = "…"]` module and the file of an `include_bytes!` or
//! `include_str!` whose path is a string literal, both from the source's folder, or
//! `concat!(env!("CARGO_MANIFEST_DIR"), "/…")`, from the source's crate folder (`xtask/` for the
//! files under it, else the repository root). A deleted integration test file has no test to run.
//! Any other changed file that is neither a Rust source nor a build file nor the status file runs
//! nothing.
//! `cargo xtask verify` runs everything, when the user asks for it (`/verify`).
//!
//! The builds go to `target/check`, so a check neither waits on a running `verify` nor rebuilds
//! the running xtask.exe. A pass is remembered as a stamp of HEAD and the changed files the build
//! reads, so an unchanged tree passes at once; the stamp is kept only when the tree is the same
//! after the check as before it.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::process::Command;

use crate::dump::{create_dir, is_file, read_if_exists, write};
use crate::table::{STATUS_PATH, sources};
use crate::{
    CLIPPY, DOC, DOC_PRIVATE, FMT, Mode, SOURCE_DIRS, cargo_step, say, step, table_command,
};

/// The comments that mark a file the API table reads.
const API_COMMENTS: [&str; 2] = ["// api:", "// api-class:"];
/// Files other than Rust sources whose change can break the build or the lints.
const BUILD_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    ".clippy.toml",
    ".rustfmt.toml",
    "rust-toolchain.toml",
];
/// The tokens of an include path from the crate folder, before its `"/…"` literal.
const FROM_CRATE: [&str; 5] = ["concat!(", "env!(", "\"CARGO_MANIFEST_DIR\"", ")", ","];
/// The macros that include a file, each followed by its path.
const INCLUDE_MACROS: [&str; 2] = ["include_bytes!(", "include_str!("];
/// The stamp of the last pass.
const PASS: &str = "target/check/pass";
/// The attribute that names a module's file, followed by `= "<path>"`.
const PATH_ATTRIBUTE: &str = "#[path";
/// The build directory of the private documentation build under `target/`, apart from `TARGET`
/// because the two documentation builds write the same files.
const PRIVATE_DOCS_TARGET: &str = "check-private-docs";
/// The build directory under `target/`.
const TARGET: &str = "check";

/// The files the Rust sources pull in, each with the sources that pull it in, as repository
/// paths.
type Includes = BTreeMap<String, BTreeSet<String>>;

/// What the check depends on at one moment.
#[derive(Debug)]
struct Snapshot {
    /// The files that differ from HEAD.
    files: Vec<String>,
    /// The files the Rust sources pull in.
    includes: Includes,
    /// The stamp of HEAD and the changed files the build reads.
    stamp: String,
}

impl Snapshot {
    fn take(root: &Path) -> Result<Self, String> {
        let files = changed(root)?;
        let includes = includes(root)?;
        let stamp = stamp(root, &files, &includes)?;
        Ok(Self {
            files,
            includes,
            stamp,
        })
    }
}

/// What the changed files need run.
#[derive(Debug, Default)]
struct Plan {
    /// Whether any file the build reads changed.
    code: bool,
    /// Whether the documentation builds run: a Rust source that is documented changed.
    doc: bool,
    /// Whether the API table check runs: the status file or a file with an API comment changed.
    table: bool,
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
    fn new(root: &Path, files: &[String], includes: &Includes) -> Result<Self, String> {
        let mut plan = Self {
            table: files.iter().any(|f| f == STATUS_PATH) || has_api_comment(root, files)?,
            ..Self::default()
        };
        // A changed included file stands for the sources that include it.
        let sources = files.iter().filter_map(|f| includes.get(f)).flatten();
        for file in files.iter().filter(|f| is_code(f)).chain(sources) {
            plan.code = true;
            if !is_rust(file) {
                if file.starts_with("xtask/") {
                    plan.xtask_unit = true;
                } else {
                    plan.all_lib = true;
                }
            } else if let Some(rel) = file.strip_prefix("src/") {
                plan.doc = true;
                match module(root, rel) {
                    Some(module) => {
                        plan.lib.insert(format!("{module}::"));
                    }
                    None => plan.all_lib = true,
                }
            } else if let Some(rel) = file.strip_prefix("tests/") {
                plan.anchor_tests.extend(test_target(root, file, rel));
            } else if file.starts_with("xtask/src/") {
                plan.doc = true;
                plan.xtask_unit = true;
            } else if let Some(rel) = file.strip_prefix("xtask/tests/") {
                plan.xtask_tests.extend(test_target(root, file, rel));
            }
            // Clippy builds `examples/` and the vendor copies Anchor uses; `verify` tests them.
        }
        Ok(plan)
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

/// Runs the check, writing its lines and `PASS` or `FAIL` to `out`, and returns whether it
/// passed.
pub(crate) fn command(root: &Path, out: &mut impl Write) -> Result<bool, String> {
    let before = Snapshot::take(root)?;
    if read_stamp(root)?.as_deref() == Some(before.stamp.as_str()) {
        say(out, "unchanged since the last pass\nPASS")?;
        return Ok(true);
    }
    let passed = check(root, &before.files, &before.includes, out)?;
    // A file that changed while the steps ran was seen in two states; the next check runs again.
    if passed && Snapshot::take(root)?.stamp == before.stamp {
        write_stamp(root, &before.stamp)?;
    }
    Ok(passed)
}

/// Runs the steps the changed `files` need, stopping at the first failure, and writes their
/// lines and `PASS` or `FAIL` to `out`.
fn check(
    root: &Path,
    files: &[String],
    includes: &Includes,
    out: &mut impl Write,
) -> Result<bool, String> {
    let plan = Plan::new(root, files, includes)?;
    if !plan.code && !plan.table {
        say(out, "no code changed\nPASS")?;
        return Ok(true);
    }
    if plan.table
        && !step(out, "cargo xtask table --check", |out| {
            table_command(out, Mode::Check)
        })?
    {
        say(out, "FAIL")?;
        return Ok(false);
    }
    if !plan.code {
        say(out, "PASS")?;
        return Ok(true);
    }
    let mut steps: Vec<(Vec<String>, &str)> = vec![(args(FMT), TARGET), (args(CLIPPY), TARGET)];
    steps.extend(plan.tests().into_iter().map(|test| (test, TARGET)));
    if plan.doc {
        steps.push((args(DOC), TARGET));
        steps.push((args(DOC_PRIVATE), PRIVATE_DOCS_TARGET));
    }
    for (cargo, target) in &steps {
        let cargo: Vec<&str> = cargo.iter().map(String::as_str).collect();
        if !cargo_step(out, &cargo, target)? {
            say(out, "FAIL")?;
            return Ok(false);
        }
    }
    say(out, "PASS")?;
    Ok(true)
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

/// Whether the check reads `file`: a Rust source, a build file, a file a source pulls in or the
/// status file.
fn is_read(file: &str, includes: &Includes) -> bool {
    is_code(file) || includes.contains_key(file) || file == STATUS_PATH
}

/// Whether one of the changed Rust `files` that still exists holds an API comment.
fn has_api_comment(root: &Path, files: &[String]) -> Result<bool, String> {
    for file in files.iter().filter(|f| is_rust(f)) {
        if let Some(bytes) = read_if_exists(&root.join(file))? {
            let text = String::from_utf8_lossy(&bytes);
            let marked = text.lines().any(|line| {
                let line = line.trim_start();
                API_COMMENTS.iter().any(|comment| line.starts_with(comment))
            });
            if marked {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Whether `file` is a Rust source or a build file.
fn is_code(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    is_rust(file) || file.starts_with(".cargo/") || BUILD_FILES.contains(&name)
}

/// The files the Rust sources under [`SOURCE_DIRS`] pull in.
fn includes(root: &Path) -> Result<Includes, String> {
    let mut includes = Includes::new();
    for (file, text) in sources(root, SOURCE_DIRS)? {
        for path in included(&file, &text) {
            includes.entry(path).or_default().insert(file.clone());
        }
    }
    Ok(includes)
}

/// The files the Rust source `file` (a repository path) pulls in, as the module documentation
/// says.
fn included(file: &str, text: &str) -> Vec<String> {
    let dir = file.rsplit_once('/').map_or("", |(dir, _)| dir);
    let crate_dir = if file.starts_with("xtask/") {
        "xtask"
    } else {
        ""
    };
    let mut paths = Vec::new();
    for name in INCLUDE_MACROS {
        for rest in after(text, name) {
            paths.extend(match literal(rest) {
                Some((path, _)) => resolve(dir, path),
                None => from_crate(rest).and_then(|path| resolve(crate_dir, path)),
            });
        }
    }
    for rest in after(text, PATH_ATTRIBUTE) {
        paths.extend(
            token(rest, "=")
                .and_then(literal)
                .and_then(|(path, _)| resolve(dir, path)),
        );
    }
    paths
}

/// The text after each `token` in `text`.
fn after<'a>(text: &'a str, token: &'a str) -> impl Iterator<Item = &'a str> {
    text.match_indices(token)
        .filter_map(move |(at, _)| text.get(at + token.len()..))
}

/// `text` after `token`, which may follow whitespace.
fn token<'a>(text: &'a str, token: &str) -> Option<&'a str> {
    text.trim_start().strip_prefix(token)
}

/// The string literal `text` starts with (after whitespace) and the text after it.
fn literal(text: &str) -> Option<(&str, &str)> {
    token(text, "\"")?.split_once('"')
}

/// The path, without its leading `/`, of an include path [`FROM_CRATE`] that `text` starts with.
fn from_crate(text: &str) -> Option<&str> {
    let rest = FROM_CRATE.iter().try_fold(text, |rest, t| token(rest, t))?;
    let (path, rest) = literal(rest)?;
    token(token(rest, ",").unwrap_or(rest), ")")?;
    path.strip_prefix('/')
}

/// `path` relative to the repository folder `dir`, as a repository path; `None` when it is
/// absolute or leaves the repository.
fn resolve(dir: &str, path: &str) -> Option<String> {
    if path.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
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

/// The test target a changed `file` under a `tests/` directory (`rel` is its path in it) belongs
/// to: its name for a top-level file, `*` (every test) for a shared support file, and none for a
/// top-level file that is deleted, which has nothing to run.
fn test_target(root: &Path, file: &str, rel: &str) -> Option<String> {
    match rel.strip_suffix(".rs") {
        Some(name) if !name.contains('/') => is_file(&root.join(file)).then(|| name.to_owned()),
        _ => Some("*".to_owned()),
    }
}

/// The files that differ from HEAD (changed, staged, deleted or untracked), as repository paths.
fn changed(root: &Path) -> Result<Vec<String>, String> {
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

/// A stamp of HEAD and the changed files the build reads with their contents (FNV-1a): equal
/// stamps mean the same code.
fn stamp(root: &Path, files: &[String], includes: &Includes) -> Result<String, String> {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut add = |bytes: &[u8]| {
        for b in bytes {
            hash = (hash ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    add(&git(root, &["rev-parse", "HEAD"])?);
    for file in files.iter().filter(|f| is_read(f, includes)) {
        add(file.as_bytes());
        match read_if_exists(&root.join(file))? {
            Some(bytes) => {
                add(&[1]);
                add(&bytes);
            }
            None => add(&[0]),
        }
    }
    Ok(format!("{hash:016x}"))
}

/// The output of `git <args>` at the repository root.
fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

/// The stamp of the last pass, if there is one.
fn read_stamp(root: &Path) -> Result<Option<String>, String> {
    let bytes = read_if_exists(&root.join(PASS))?;
    Ok(bytes.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
}

fn write_stamp(root: &Path, stamp: &str) -> Result<(), String> {
    let path = root.join(PASS);
    if let Some(dir) = path.parent() {
        create_dir(dir)?;
    }
    write(&path, stamp.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::root;

    fn plan(files: &[&str]) -> Plan {
        let files: Vec<String> = files.iter().map(|f| (*f).to_owned()).collect();
        Plan::new(&root(), &files, &includes(&root()).unwrap()).unwrap()
    }

    #[test]
    fn the_status_file_or_an_api_comment_runs_the_table_check_and_a_source_the_doc_builds() {
        let status = plan(&["xtask/data/dump/status.txt"]);
        assert!(status.table && !status.code && !status.doc);
        let api = plan(&["tests/block.rs"]);
        assert!(api.table && api.code && !api.doc);
        let source = plan(&["xtask/src/check.rs"]);
        assert!(source.doc && source.code && !source.table);
        assert!(!plan(&["README.md", "Cargo.toml"]).doc);
        assert!(!plan(&["README.md"]).table);
    }

    #[test]
    fn a_source_file_runs_the_tests_of_its_module() {
        let p = plan(&[
            "src/tick/block_entity/decor.rs",
            "src/tick/interact/tests.rs",
            "src/import/api/tests.rs",
            "src/save.rs",
            "src/math.rs",
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
    fn an_included_data_file_runs_the_tests_of_its_includers_and_enters_the_stamp() {
        let p = plan(&[
            "src/crypto/testdata/encryption.json",
            "xtask/data/reference/camera.json",
            "xtask/data/p6/intended.txt",
        ]);
        assert!(p.code && !p.all_lib);
        assert_eq!(
            p.tests(),
            [
                args(&[
                    "test",
                    "-p",
                    "anchor",
                    "--lib",
                    "--quiet",
                    "--",
                    "crypto::",
                    "tick::spawn::"
                ]),
                args(&["test", "-p", "xtask", "--bin", "xtask", "--quiet"]),
            ]
        );
        let includes = includes(&root()).unwrap();
        assert!(is_read("xtask/data/reference/camera.json", &includes));
        assert!(!is_read(".claude/docs/data.md", &includes));
    }

    #[test]
    fn a_file_another_crate_pulls_in_also_runs_that_crates_tests() {
        // `apply.rs` is a module of the library and, by `#[path]`, of xtask's `gen::upgrade`;
        // `continental.kv` is included from the crate folder by `src/import/tests.rs`.
        let p = plan(&[
            "src/import/upgrade/apply.rs",
            "xtask/data/import/continental.kv",
        ]);
        assert!(p.code && !p.all_lib && p.xtask_unit);
        let lib: Vec<&str> = p.lib.iter().map(String::as_str).collect();
        assert_eq!(lib, ["import::", "import::upgrade::"]);
    }

    #[test]
    fn pulled_in_paths_resolve_from_the_source_folder_or_the_crate_folder() {
        let text = r#"include_str!("../x/y.json"); include_bytes!( "z.bin");
            include_str!(concat!("q")); include_bytes!("../../../out");
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/data/k.kv"
            ));
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/a", "/b"));
            #[path = "../../shared.rs"]
            mod shared;"#;
        assert_eq!(
            included("src/a/b.rs", text),
            ["src/a/z.bin", "data/k.kv", "src/x/y.json", "shared.rs"]
        );
        assert_eq!(
            included("xtask/tests/t.rs", text),
            [
                "xtask/tests/z.bin",
                "xtask/data/k.kv",
                "xtask/x/y.json",
                "shared.rs"
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

    #[test]
    fn a_deleted_test_file_has_no_target_to_run() {
        let p = plan(&["tests/gone_for_good.rs", "xtask/tests/gone_too.rs"]);
        assert!(p.code);
        assert!(p.tests().is_empty());
    }
}
