//! Running external programs: the output of a command, and the Go tools of `xtask/go` built
//! against gophertunnel with Anchor's corrections. The xtask tests include this file by path, so
//! it uses nothing else of xtask.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Builds the Go tool `xtask/go/<tool>` into `dir` against gophertunnel with Anchor's corrections
/// (`xtask/go/fix`, written to `<dir>/fixmod`) and returns the executable's path.
pub(crate) fn build_go(dir: &Path, tool: &str) -> Result<PathBuf, String> {
    let go_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("go");
    let fixmod = dir.join("fixmod");
    let exe = dir.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX));
    let error = |e: String| format!("cannot build {tool} (it needs Go on PATH): {e}");
    stdout(
        Command::new("go")
            .args(["run", "./fixmod", "-out"])
            .arg(&fixmod)
            .current_dir(&go_dir),
    )
    .map_err(error)?;
    stdout(
        Command::new("go")
            .arg("build")
            .arg("-modfile")
            .arg(fixmod.join("go.mod"))
            .arg("-o")
            .arg(&exe)
            .arg(format!("./{tool}"))
            .current_dir(&go_dir),
    )
    .map_err(error)?;
    Ok(exe)
}

/// The standard output of `command`; a run that cannot start or fails is an error naming the
/// command line, with the program's standard error when it ran.
pub(crate) fn stdout(command: &mut Command) -> Result<Vec<u8>, String> {
    let line = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let output = command
        .output()
        .map_err(|e| format!("cannot run {line}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{line} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}
