//! Repository tasks. `cargo xtask gen [--check]` regenerates the protocol code (or checks it is
//! current); `cargo xtask verify` runs every check and ends with PASS or FAIL, or with the I/O
//! error that stopped it; `cargo xtask dump [--check]` rebuilds the committed block and item data
//! and the protocol field limits from the pinned PMMP, dragonfly, `CloudburstMC` and Mojang
//! protocol docs checkouts; `cargo xtask p6 ...` runs the reference server comparison. Neither
//! of the last two is part of `verify`: they need `.cache/` checkouts or BDS. `cargo xtask table [--check]` writes (or checks) the PMMP mapping table;
//! `verify` runs its check.

mod dump;
mod r#gen;
mod p6;
mod reasons;
mod table;
mod vendor;

use std::env;
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

/// The flag of the second documentation check. Its build writes the same files as the first, so
/// in one target directory each rebuilt the other's output on every run, even with nothing
/// changed; it gets a target directory of its own.
const PRIVATE_DOCS: &str = "--document-private-items";

/// Checks in run order. Each passes only if it exits successfully and prints no warning.
const CHECKS: &[&[&str]] = &[
    // No `--all`: it also formats path dependencies, the vendor copies.
    &["fmt", "--check"],
    &[
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ],
    &["test", "--workspace", "--quiet"],
    // The fixed copies are not workspace members; their own tests, the reproduction tests of
    // their fixes among them, run here (PLAN.md 9.1).
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
    // the published crate leaves out (`vendor-test str0m`, PLAN.md 9.1).
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
    // MSVC's linker: rust-lld warns about the debug info OpenSSL's build leaves out of its static
    // library (LNK4099), and a warning fails the check.
    &[
        "test",
        "--manifest-path",
        DIMPL,
        "--tests",
        "--config",
        "target.x86_64-pc-windows-msvc.linker=\"link.exe\"",
        "--quiet",
    ],
    &["doc", "--workspace", "--no-deps", "--quiet"],
    // The public docs above check links from public to private items; this one checks the links
    // in private docs.
    &["doc", "--workspace", "--no-deps", PRIVATE_DOCS, "--quiet"],
];

fn main() -> io::Result<ExitCode> {
    Ok(if run()? {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The repository root: the parent of this crate's directory.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn run() -> io::Result<bool> {
    let mut out = io::stdout().lock();
    let args: Vec<String> = env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["gen"] => gen_command(&mut out, false),
        ["gen", "--check"] => gen_command(&mut out, true),
        ["verify"] => verify(&mut out),
        ["dump"] => dump_command(&mut out, false),
        ["dump", "--check"] => dump_command(&mut out, true),
        ["table"] => table_command(&mut out, false),
        ["table", "--check"] => table_command(&mut out, true),
        ["vendor-test", "str0m"] => match vendor::str0m(&root()) {
            Ok(passed) => Ok(passed),
            Err(e) => {
                writeln!(out, "error: {e}")?;
                Ok(false)
            }
        },
        ["p6", rest @ ..] => match p6::command(&root(), rest, &mut out) {
            Ok(passed) => Ok(passed),
            Err(e) => {
                writeln!(out, "error: {e}")?;
                Ok(false)
            }
        },
        _ => {
            writeln!(
                out,
                "usage: cargo xtask gen [--check] | cargo xtask verify | cargo xtask dump [--check] \
                 | cargo xtask table [--check] | cargo xtask vendor-test str0m | {}",
                p6::USAGE
            )?;
            Ok(false)
        }
    }
}

fn gen_command(out: &mut impl Write, check: bool) -> io::Result<bool> {
    match r#gen::run(&root(), check) {
        Ok(differ) if differ.is_empty() => Ok(true),
        Ok(differ) => {
            for path in differ {
                writeln!(out, "out of date: {path} (run `cargo xtask gen`)")?;
            }
            Ok(false)
        }
        Err(e) => {
            writeln!(out, "error: {e}")?;
            Ok(false)
        }
    }
}

fn dump_command(out: &mut impl Write, check: bool) -> io::Result<bool> {
    match dump::run(&root(), check) {
        Ok(differ) if differ.is_empty() => Ok(true),
        Ok(differ) => {
            for path in differ {
                writeln!(out, "differs: {path} (run `cargo xtask dump`)")?;
            }
            Ok(false)
        }
        Err(e) => {
            writeln!(out, "error: {e}")?;
            Ok(false)
        }
    }
}

/// Writes (or with `check` compares) the PMMP mapping table and prints its counts. Passes
/// without errors; a check also needs the committed table current and no blank row.
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

fn verify(out: &mut impl Write) -> io::Result<bool> {
    // `CARGO` is the cargo that launched this task, so every check uses the same toolchain.
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut passed = true;
    let mut log = Vec::new();
    let ok = gen_command(&mut log, true)?;
    if !ok {
        out.write_all(&log)?;
    }
    writeln!(
        out,
        "{} cargo xtask gen --check",
        if ok { "ok  " } else { "FAIL" }
    )?;
    passed &= ok;
    let mut log = Vec::new();
    let ok = table_command(&mut log, true)?;
    out.write_all(&log)?;
    writeln!(
        out,
        "{} cargo xtask table --check",
        if ok { "ok  " } else { "FAIL" }
    )?;
    passed &= ok;
    let ok = match reasons::check(&root()) {
        Ok(errors) => {
            for error in &errors {
                writeln!(out, "error: {error}")?;
            }
            errors.is_empty()
        }
        Err(e) => {
            writeln!(out, "error: {e}")?;
            false
        }
    };
    writeln!(out, "{} lint reason tags", if ok { "ok  " } else { "FAIL" })?;
    passed &= ok;
    for args in CHECKS {
        let target = if args.contains(&PRIVATE_DOCS) {
            "verify-private-docs"
        } else {
            "verify"
        };
        let mut command = Command::new(&cargo);
        if cfg!(windows) && args.contains(&DIMPL) {
            command.env("OPENSSL_SRC_PERL", root().join(PERL));
        }
        let output = command
            .args(*args)
            .current_dir(root())
            // `cargo test --workspace` unifies features differently from `cargo xtask`, so in the
            // shared target directory it would rebuild the running xtask.exe, which Windows
            // cannot replace.
            .env("CARGO_TARGET_DIR", root().join("target").join(target))
            .env("RUSTDOCFLAGS", "-D warnings")
            .env("CARGO_TERM_COLOR", "never")
            .output()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Clippy reports an unresolved .clippy.toml path, and rustfmt an unstable option,
        // only as a warning with a successful exit code.
        let warned = stderr.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("warning") || line.starts_with("Warning")
        });
        let ok = output.status.success() && !warned;
        if !ok {
            out.write_all(&output.stdout)?;
            out.write_all(&output.stderr)?;
        }
        let status = if ok { "ok  " } else { "FAIL" };
        writeln!(out, "{status} cargo {}", args.join(" "))?;
        passed &= ok;
    }
    writeln!(out, "{}", if passed { "PASS" } else { "FAIL" })?;
    Ok(passed)
}
