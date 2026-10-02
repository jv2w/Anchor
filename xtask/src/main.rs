//! Repository tasks. `cargo xtask verify` runs every check and ends with PASS or FAIL,
//! or with the I/O error that stopped it.

use std::env;
use std::io::{self, Write};
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
    &["doc", "--workspace", "--no-deps", "--quiet"],
];

fn main() -> io::Result<ExitCode> {
    Ok(if run()? {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn run() -> io::Result<bool> {
    let mut out = io::stdout().lock();
    if env::args_os().nth(1).is_none_or(|arg| arg != "verify") {
        writeln!(out, "usage: cargo xtask verify")?;
        return Ok(false);
    }
    // `CARGO` is the cargo that launched this task, so every check uses the same toolchain.
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut passed = true;
    for args in CHECKS {
        let output = Command::new(&cargo)
            .args(*args)
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
