//! Repository tasks. `cargo xtask gen [--check]` regenerates the protocol code (or checks it is
//! current); `cargo xtask verify` runs every check and ends with PASS or FAIL, or with the I/O
//! error that stopped it; `cargo xtask p6 ...` runs the reference server comparison (not part of
//! `verify`: it needs BDS).

mod r#gen;
mod p6;

use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Checks in run order. Each passes only if it exits successfully and prints no warning.
const CHECKS: &[&[&str]] = &[
    &["fmt", "--all", "--check"],
    &[
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ],
    &["test", "--workspace", "--quiet"],
    // The fixed sctp-proto copy is not a workspace member; its own tests, the reproduction tests
    // of its fixes among them, run here (PLAN.md 9.1).
    &[
        "test",
        "--manifest-path",
        "vendor/sctp-proto/Cargo.toml",
        "--lib",
        "--quiet",
    ],
    &["doc", "--workspace", "--no-deps", "--quiet"],
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
                "usage: cargo xtask gen [--check] | cargo xtask verify | {}",
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
    for args in CHECKS {
        let output = Command::new(&cargo)
            .args(*args)
            .current_dir(root())
            // `cargo test --workspace` unifies features differently from `cargo xtask`, so in the
            // shared target directory it would rebuild the running xtask.exe, which Windows
            // cannot replace.
            .env("CARGO_TARGET_DIR", root().join("target").join("verify"))
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
