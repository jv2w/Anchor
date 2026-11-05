//! Repository tasks, run as `cargo xtask <command>`:
//! - `gen [--check]` regenerates the protocol codecs and the data tables, or checks they are
//!   current (`gen`).
//! - `check` runs the format, clippy and the tests of what changed, the check every task ends
//!   with (`check`).
//! - `verify` runs every check and ends with `PASS` or `FAIL`.
//! - `dump [--check]` rebuilds the committed block, item, upgrade and field limit data from the
//!   pinned checkouts in `.cache/` (`dump`). It is not part of `verify`: it needs those
//!   checkouts.
//! - `table [--check]` writes or checks the API table (`table`); `verify` runs its check.
//! - `vendor-test str0m` runs the str0m copy's full test suite (`vendor`).
//! - `p6 ...` compares Anchor with the official server (`p6`). It is not part of `verify`: it
//!   needs the official server.

mod check;
pub mod dump;
mod fs;
mod r#gen;
mod p6;
mod process;
mod reasons;
mod table;
mod vendor;

use std::env;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Checks in run order. Each passes only if it exits successfully and prints no warning.
const CHECKS: &[&[&str]] = &[
    FMT,
    CLIPPY,
    &["test", "--workspace", "--quiet"],
    // The fixed copies are not workspace members; their own tests, the reproduction tests of
    // their fixes among them, run here.
    &[
        "test",
        "--manifest-path",
        "vendor/sctp-proto/Cargo.toml",
        "--lib",
        "--quiet",
    ],
    &[
        "test",
        "--manifest-path",
        "vendor/redb/Cargo.toml",
        "--tests",
        "--quiet",
    ],
    // With Anchor's features, the library tests only: the integration tests need capture files
    // the published crate leaves out (`vendor-test str0m`).
    &[
        "test",
        "--manifest-path",
        STR0M,
        "--lib",
        "--no-default-features",
        "--features",
        "aws-lc-rs",
        "--config",
        STR0M_DIMPL,
        "--config",
        STR0M_PROTO,
        "--quiet",
    ],
    // The tests link OpenSSL's static library, which names debug info its build leaves out
    // (LNK4099), and a warning fails the check. The workspace links with rust-lld, which is told
    // to ignore just that one warning.
    &[
        "test",
        "--manifest-path",
        DIMPL,
        "--tests",
        "--config",
        "target.x86_64-pc-windows-msvc.rustflags=[\"-Clink-arg=/ignore:4099\"]",
        "--quiet",
    ],
    DOC,
    DOC_PRIVATE,
];
/// The lint check.
const CLIPPY: &[&str] = &[
    "clippy",
    "--workspace",
    "--all-targets",
    "--",
    "-D",
    "warnings",
];
/// The dimpl copy, whose tests build OpenSSL from source (its `openssl` dev-dependency is
/// vendored).
const DIMPL: &str = "vendor/dimpl/Cargo.toml";
/// The public documentation check: links from public to private items.
const DOC: &[&str] = &["doc", "--workspace", "--no-deps", "--quiet"];
/// The private documentation check: the links in private docs.
const DOC_PRIVATE: &[&str] = &["doc", "--workspace", "--no-deps", PRIVATE_DOCS, "--quiet"];
/// The format check. No `--all`: it also formats path dependencies, the vendor copies.
const FMT: &[&str] = &["fmt", "--check"];
/// The Perl that configures that OpenSSL build on Windows: an MSVC build needs a Windows Perl, and
/// Strawberry Perl portable is a development tool kept in the git-ignored `.cache/tools`.
const PERL: &str = ".cache/tools/strawberry-perl-5.42.3.1/perl/bin/perl.exe";
/// The flag of the second documentation check. Its build writes the same files as the first, so
/// in one target directory each rebuilt the other's output on every run, even with nothing
/// changed; it gets a target directory of its own.
const PRIVATE_DOCS: &str = "--document-private-items";
/// The build directory of the second documentation check under `target/`.
const PRIVATE_DOCS_TARGET: &str = "verify-private-docs";
/// The folders of the repository's Rust sources (the vendor copies aside).
const SOURCE_DIRS: &[&str] = &["src", "tests", "examples", "xtask/src", "xtask/tests"];
/// The str0m copy's manifest.
const STR0M: &str = "vendor/str0m/Cargo.toml";
/// The patch that points the str0m copy's registry dev-dependency dimpl at the copy, as in Anchor.
const STR0M_DIMPL: &str = "patch.crates-io.dimpl.path=\"vendor/dimpl\"";
/// The patch that points the str0m copy's registry dev-dependency str0m-proto at the copy, as in
/// Anchor.
const STR0M_PROTO: &str = "patch.crates-io.str0m-proto.path=\"vendor/str0m-proto\"";
/// The build directory of `verify` under `target/`; `vendor-test` builds there too.
const VERIFY_TARGET: &str = "verify";

/// Whether a command writes its files or only compares them with the committed ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Write,
    Check,
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn main() -> ExitCode {
    let passed = run().unwrap_or_else(|e| {
        // Only a failed write to standard output gets here, so the error goes to standard error;
        // the exit code reports the failure even when that write fails too.
        let _unreported = say_error(&mut io::stderr(), &e);
        false
    });
    exit(passed)
}

/// Runs the command the arguments name, writing its lines to standard output, and returns whether
/// it passed.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn run() -> Result<bool, String> {
    let mut out = io::stdout().lock();
    let args: Vec<String> = env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["check"] => check::command(&root(), &mut out),
        ["gen"] => gen_command(&mut out, Mode::Write),
        ["gen", "--check"] => gen_command(&mut out, Mode::Check),
        ["verify"] => verify(&mut out),
        ["dump"] => dump_command(&mut out, Mode::Write),
        ["dump", "--check"] => dump_command(&mut out, Mode::Check),
        ["table"] => table_command(&mut out, Mode::Write),
        ["table", "--check"] => table_command(&mut out, Mode::Check),
        ["vendor-test", "str0m"] => vendor::str0m(&root()).or_else(|e| report_error(&mut out, &e)),
        ["p6", rest @ ..] => {
            p6::command(&root(), rest, &mut out).or_else(|e| report_error(&mut out, &e))
        }
        _ => {
            say(
                &mut out,
                &format!(
                    "usage: cargo xtask gen [--check] | cargo xtask check | cargo xtask \
                     verify | cargo xtask dump [--check] | cargo xtask table [--check] | cargo \
                     xtask vendor-test str0m | {}",
                    p6::USAGE
                ),
            )?;
            Ok(false)
        }
    }
}

/// Writes `text` and a newline to `out`.
pub(crate) fn say(out: &mut impl Write, text: &str) -> Result<(), String> {
    writeln!(out, "{text}").map_err(|e| e.to_string())
}

/// Runs every check in order, each one even after a failure, and writes `ok` or `FAIL` for each
/// and `PASS` or `FAIL` for all.
fn verify(out: &mut impl Write) -> Result<bool, String> {
    let mut passed = true;
    passed &= step(out, "cargo xtask gen --check", |out| {
        gen_command(out, Mode::Check)
    })?;
    passed &= step(out, "cargo xtask table --check", |out| {
        table_command(out, Mode::Check)
    })?;
    passed &= step(out, "lint reason tags", reasons_command)?;
    for args in CHECKS {
        let target = if args.contains(&PRIVATE_DOCS) {
            PRIVATE_DOCS_TARGET
        } else {
            VERIFY_TARGET
        };
        passed &= cargo_step(out, args, target)?;
    }
    say(out, if passed { "PASS" } else { "FAIL" })?;
    Ok(passed)
}

/// Runs `check`, which writes its findings to `out`, and then writes its `ok` or `FAIL` line.
fn step<W: Write>(
    out: &mut W,
    name: &str,
    check: impl FnOnce(&mut W) -> Result<bool, String>,
) -> Result<bool, String> {
    let ok = check(out)?;
    say(out, &format!("{} {name}", status(ok)))?;
    Ok(ok)
}

/// Runs `cargo <args>` ([`run_cargo`]) and writes `ok` or `FAIL` with the command, after the
/// output of a failure or the error of a cargo that cannot start.
fn cargo_step(out: &mut impl Write, args: &[&str], target: &str) -> Result<bool, String> {
    step(out, &format!("cargo {}", args.join(" ")), |out| {
        run_cargo(out, args, target).or_else(|e| report_error(out, &e))
    })
}

/// Runs `cargo <args>` at the repository root with its builds in `target/<target>`, writes its
/// output to `out` when it fails, and returns whether it passed: a successful exit that printed no
/// warning.
fn run_cargo(out: &mut impl Write, args: &[&str], target: &str) -> Result<bool, String> {
    let mut command = Command::new(cargo());
    if cfg!(windows) && args.contains(&DIMPL) {
        command.env("OPENSSL_SRC_PERL", root().join(PERL));
    }
    let output = command
        .args(args)
        .current_dir(root())
        // `cargo test --workspace` unifies features differently from `cargo xtask`, so in the
        // shared target directory it would rebuild the running xtask.exe, which Windows cannot
        // replace.
        .env("CARGO_TARGET_DIR", target_dir(target))
        .env("RUSTDOCFLAGS", "-D warnings")
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Clippy reports an unresolved .clippy.toml path, and rustfmt an unstable option, only as a
    // warning with a successful exit code.
    let warned = stderr.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("warning") || line.starts_with("Warning")
    });
    let ok = output.status.success() && !warned;
    if !ok {
        out.write_all(&output.stdout).map_err(|e| e.to_string())?;
        out.write_all(&output.stderr).map_err(|e| e.to_string())?;
    }
    Ok(ok)
}

fn gen_command(out: &mut impl Write, mode: Mode) -> Result<bool, String> {
    report_files(out, r#gen::run(&root(), mode), "out of date", "gen")
}

fn dump_command(out: &mut impl Write, mode: Mode) -> Result<bool, String> {
    report_files(out, dump::run(&root(), mode), "differs", "dump")
}

/// Writes (or in check mode compares) the API table and prints its counts. Passes without errors;
/// a check also needs the committed table current and no blank row.
fn table_command(out: &mut impl Write, mode: Mode) -> Result<bool, String> {
    let report = match table::run(&root(), mode) {
        Ok(report) => report,
        Err(e) => return report_error(out, &e),
    };
    for error in &report.errors {
        say_error(out, error)?;
    }
    if report.stale {
        say(
            out,
            "out of date: xtask/data/dump/table.md (run `cargo xtask table`)",
        )?;
    }
    say(out, &format!("api table:{}", report.counts))?;
    let failed_check = mode == Mode::Check && (report.stale || report.counts.blank > 0);
    Ok(report.errors.is_empty() && !failed_check)
}

fn reasons_command(out: &mut impl Write) -> Result<bool, String> {
    match reasons::check(&root()) {
        Ok(errors) => {
            for error in &errors {
                say_error(out, error)?;
            }
            Ok(errors.is_empty())
        }
        Err(e) => report_error(out, &e),
    }
}

/// Writes one `verb: file (run `cargo xtask command`)` line for each file that `result`
/// lists as differing, or its error. Passes when the list is empty.
fn report_files(
    out: &mut impl Write,
    result: Result<Vec<String>, String>,
    verb: &str,
    command: &str,
) -> Result<bool, String> {
    match result {
        Ok(differ) => {
            for path in &differ {
                say(
                    out,
                    &format!("{verb}: {path} (run `cargo xtask {command}`)"),
                )?;
            }
            Ok(differ.is_empty())
        }
        Err(e) => report_error(out, &e),
    }
}

/// Writes `error` as an error line and reports a failed command.
pub(crate) fn report_error(out: &mut impl Write, error: &str) -> Result<bool, String> {
    say_error(out, error)?;
    Ok(false)
}

/// Writes one `error: ...` line.
fn say_error(out: &mut impl Write, error: &str) -> Result<(), String> {
    say(out, &format!("error: {error}"))
}

fn exit(passed: bool) -> ExitCode {
    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The status column of a step line.
fn status(ok: bool) -> &'static str {
    if ok { "ok  " } else { "FAIL" }
}

/// The cargo that launched this task, so every step uses the same toolchain.
fn cargo() -> OsString {
    env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}

/// The standard output of `git <args>` in `dir`; a failed run is an error with git's standard
/// error.
pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    process::stdout(Command::new("git").args(args).current_dir(dir))
}

/// The build directory `target/<name>` under the repository root.
fn target_dir(name: &str) -> PathBuf {
    root().join("target").join(name)
}

/// The repository root: the parent of this crate's directory.
#[expect(
    clippy::expect_used,
    reason = "invariant: this crate's directory is inside the repository"
)]
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is in the repository")
        .to_path_buf()
}
