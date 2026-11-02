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
mod dump;
mod r#gen;
mod p6;
mod reasons;
mod table;
mod vendor;

use std::env;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// The dimpl copy, whose tests build OpenSSL from source (its `openssl` dev-dependency is
/// vendored).
const DIMPL: &str = "vendor/dimpl/Cargo.toml";

/// The Perl that configures that OpenSSL build on Windows: an MSVC build needs a Windows Perl, and
/// Strawberry Perl portable is a development tool kept in the git-ignored `.cache/tools`.
const PERL: &str = ".cache/tools/strawberry-perl-5.42.3.1/perl/bin/perl.exe";

/// The str0m copy, and the patches that point its registry dev-dependencies' dimpl and
/// str0m-proto at the copies, as in Anchor.
const STR0M: &str = "vendor/str0m/Cargo.toml";
const STR0M_DIMPL: &str = "patch.crates-io.dimpl.path=\"vendor/dimpl\"";
const STR0M_PROTO: &str = "patch.crates-io.str0m-proto.path=\"vendor/str0m-proto\"";

/// The folders of the repository's Rust sources (the vendor copies aside).
const SOURCE_DIRS: &[&str] = &["src", "tests", "examples", "xtask/src", "xtask/tests"];

/// The build directory of `verify` under `target/`; `vendor-test` builds there too.
const VERIFY_TARGET: &str = "verify";

/// The flag of the second documentation check. Its build writes the same files as the first, so
/// in one target directory each rebuilt the other's output on every run, even with nothing
/// changed; it gets a target directory of its own.
const PRIVATE_DOCS: &str = "--document-private-items";

/// The build directory of the second documentation check under `target/`.
const PRIVATE_DOCS_TARGET: &str = "verify-private-docs";

/// The format check. No `--all`: it also formats path dependencies, the vendor copies.
const FMT: &[&str] = &["fmt", "--check"];
/// The lint check.
const CLIPPY: &[&str] = &[
    "clippy",
    "--workspace",
    "--all-targets",
    "--",
    "-D",
    "warnings",
];

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
    &["doc", "--workspace", "--no-deps", "--quiet"],
    // The public docs above check links from public to private items; this one checks the links
    // in private docs.
    &["doc", "--workspace", "--no-deps", PRIVATE_DOCS, "--quiet"],
];

fn main() -> io::Result<ExitCode> {
    run()
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn run() -> io::Result<ExitCode> {
    let mut out = io::stdout().lock();
    let args: Vec<String> = env::args().skip(1).collect();
    let passed = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["check"] => {
            let result = check::command(&root(), &mut out);
            report_passed(&mut out, result)
        }
        ["gen"] => gen_command(&mut out, false),
        ["gen", "--check"] => gen_command(&mut out, true),
        ["verify"] => verify(&mut out),
        ["dump"] => dump_command(&mut out, false),
        ["dump", "--check"] => dump_command(&mut out, true),
        ["table"] => table_command(&mut out, false),
        ["table", "--check"] => table_command(&mut out, true),
        ["vendor-test", "str0m"] => report_passed(&mut out, vendor::str0m(&root())),
        ["p6", rest @ ..] => {
            let result = p6::command(&root(), rest, &mut out);
            report_passed(&mut out, result)
        }
        _ => {
            writeln!(
                out,
                "usage: cargo xtask gen [--check] | cargo xtask check | cargo xtask \
                 verify | cargo xtask dump [--check] | cargo xtask table [--check] | cargo xtask \
                 vendor-test str0m | {}",
                p6::USAGE
            )?;
            Ok(false)
        }
    }?;
    Ok(exit(passed))
}

/// Runs every check in order, each one even after a failure, and writes `ok` or `FAIL` for each
/// and `PASS` or `FAIL` for all.
fn verify(out: &mut impl Write) -> io::Result<bool> {
    let mut passed = true;
    passed &= step(out, "cargo xtask gen --check", |out| gen_command(out, true))?;
    passed &= step(out, "cargo xtask table --check", |out| {
        table_command(out, true)
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
    writeln!(out, "{}", if passed { "PASS" } else { "FAIL" })?;
    Ok(passed)
}

/// Runs `check`, which writes its findings to `out`, and then writes its `ok` or `FAIL` line.
fn step<W: Write>(
    out: &mut W,
    name: &str,
    check: impl FnOnce(&mut W) -> io::Result<bool>,
) -> io::Result<bool> {
    let ok = check(out)?;
    writeln!(out, "{} {name}", status(ok))?;
    Ok(ok)
}

/// Runs `cargo <args>` at the repository root with its builds in `target/<target>` and writes
/// `ok` or `FAIL` with the command, after the output of a failure. It passes on a successful exit
/// that printed no warning.
fn cargo_step(out: &mut impl Write, args: &[&str], target: &str) -> io::Result<bool> {
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
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Clippy reports an unresolved .clippy.toml path, and rustfmt an unstable option, only as a
    // warning with a successful exit code.
    let warned = stderr.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("warning") || line.starts_with("Warning")
    });
    let ok = output.status.success() && !warned;
    if !ok {
        out.write_all(&output.stdout)?;
        out.write_all(&output.stderr)?;
    }
    writeln!(out, "{} cargo {}", status(ok), args.join(" "))?;
    Ok(ok)
}

fn gen_command(out: &mut impl Write, check: bool) -> io::Result<bool> {
    report_files(out, r#gen::run(&root(), check), "out of date", "gen")
}

fn dump_command(out: &mut impl Write, check: bool) -> io::Result<bool> {
    report_files(out, dump::run(&root(), check), "differs", "dump")
}

/// Writes (or with `check` compares) the API table and prints its counts. Passes without errors;
/// a check also needs the committed table current and no blank row.
fn table_command(out: &mut impl Write, check: bool) -> io::Result<bool> {
    let report = match table::run(&root(), check) {
        Ok(report) => report,
        Err(e) => {
            writeln!(out, "error: {e}")?;
            return Ok(false);
        }
    };
    for error in &report.errors {
        writeln!(out, "error: {error}")?;
    }
    if report.stale {
        writeln!(
            out,
            "out of date: xtask/data/pmmp/table.md (run `cargo xtask table`)"
        )?;
    }
    writeln!(out, "pmmp table: {}", report.counts)?;
    Ok(report.errors.is_empty() && !(check && (report.stale || report.counts.blank > 0)))
}

fn reasons_command(out: &mut impl Write) -> io::Result<bool> {
    match reasons::check(&root()) {
        Ok(errors) => {
            for error in &errors {
                writeln!(out, "error: {error}")?;
            }
            Ok(errors.is_empty())
        }
        Err(e) => {
            writeln!(out, "error: {e}")?;
            Ok(false)
        }
    }
}

/// Writes one `<verb>: <file> (run `cargo xtask <command>`)` line for each file that `result`
/// lists as differing, or its error. Passes when the list is empty.
fn report_files(
    out: &mut impl Write,
    result: Result<Vec<String>, String>,
    verb: &str,
    command: &str,
) -> io::Result<bool> {
    match result {
        Ok(differ) => {
            for path in &differ {
                writeln!(out, "{verb}: {path} (run `cargo xtask {command}`)")?;
            }
            Ok(differ.is_empty())
        }
        Err(e) => {
            writeln!(out, "error: {e}")?;
            Ok(false)
        }
    }
}

/// `passed` of a command that reports through `out` itself, or its error written to `out`.
fn report_passed(out: &mut impl Write, result: Result<bool, String>) -> io::Result<bool> {
    match result {
        Ok(passed) => Ok(passed),
        Err(e) => {
            writeln!(out, "error: {e}")?;
            Ok(false)
        }
    }
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
    env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
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
