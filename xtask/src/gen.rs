//! `cargo xtask gen`: runs the Go extractor (`xtask/go/extract`) into the protocol IR
//! (`xtask/data/protocol.json`) and generates `src/protocol/types.rs` and
//! `src/protocol/packet.rs` from it, and the block, item and recipe tables (see
//! `data`; with the reference states Anchor shows but does not implement, see `inert`), the biome
//! table and the import's upgrade tables (see `upgrade`) from the committed data in
//! `xtask/data/`. With `--check` it does the same in `target/` and fails if the committed IR or a
//! generated file differs. The protocol decoders also apply the per-field limits in
//! `xtask/data/reference/protocol_limits.json`.

/// Appends formatted text to a `String`, which cannot fail (unlike `write!`).
macro_rules! put {
    ($out:expr, $($arg:tt)*) => {
        $out.push_str(&::std::fmt::format(format_args!($($arg)*)))
    };
}

pub(crate) mod biomes;
mod blocks;
mod body;
mod consts;
mod creative;
pub(crate) mod data;
mod drops;
mod effects;
mod emit;
mod inert;
mod info;
mod ir;
mod items;
mod model;
mod names;
mod recipes;
mod registry;
mod upgrade;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The IR, relative to the repository root.
const IR_PATH: &str = "xtask/data/protocol.json";
/// The per-field decode limits `dump` writes, relative to the repository root.
const LIMITS_PATH: &str = "xtask/data/reference/protocol_limits.json";
/// The generated files, relative to the repository root.
const TYPES_PATH: &str = "src/protocol/types.rs";
const PACKET_PATH: &str = "src/protocol/packet.rs";
/// Functions longer than this many lines need a `clippy::too_many_lines` expectation.
const TOO_MANY_LINES: usize = 100;

/// Runs `gen`; with `check`, compares instead of writing. Returns the differing files on a
/// failed check.
pub(crate) fn run(root: &Path, check: bool) -> Result<Vec<String>, String> {
    let ir_path = root.join(IR_PATH);
    let extracted = if check {
        let dir = root.join("target").join("xtask-gen");
        create_dir(&dir)?;
        dir.join("protocol.json")
    } else {
        ir_path.clone()
    };
    extract(root, &extracted)?;
    let ir_bytes = read(&extracted)?;
    let ir: ir::Ir =
        serde_json::from_slice(&ir_bytes).map_err(|e| format!("{}: {e}", extracted.display()))?;
    let limits = serde_json::from_slice(&read(&root.join(LIMITS_PATH))?)
        .map_err(|e| format!("{LIMITS_PATH}: {e}"))?;
    let model = model::Model::new(&ir, limits)?;
    let output = emit::generate(&model)?;
    let data = data::Data::load(root)?;
    let files = [
        (TYPES_PATH, finish(root, &output.types)?),
        (PACKET_PATH, finish(root, &output.packet)?),
        (consts::PATH, finish(root, &consts::generate(&ir)?)?),
        (blocks::PATH, finish(root, &blocks::generate(&data)?)?),
        (items::PATH, finish(root, &items::generate(&data)?)?),
        (registry::PATH, finish(root, &registry::generate(root)?)?),
        (
            creative::PATH,
            finish(root, &creative::generate(root, &data)?)?,
        ),
        (
            recipes::PATH,
            finish(root, &recipes::generate(root, &data)?)?,
        ),
        (biomes::PATH, finish(root, &biomes::generate(root)?)?),
        (effects::PATH, finish(root, &effects::generate(&data)?)?),
        (drops::PATH, finish(root, &drops::generate(&data)?)?),
        (upgrade::PATH, finish(root, &upgrade::generate(root)?)?),
        (inert::PATH, finish(root, &inert::generate(root, &data)?)?),
    ];
    let mut differ = Vec::new();
    if check {
        if read(&ir_path).ok().as_deref() != Some(ir_bytes.as_slice()) {
            differ.push(IR_PATH.to_owned());
        }
        for (path, text) in &files {
            if read(&root.join(path)).ok().as_deref() != Some(text.as_bytes()) {
                differ.push((*path).to_owned());
            }
        }
    } else {
        for (path, text) in &files {
            write(&root.join(path), text.as_bytes())?;
        }
    }
    Ok(differ)
}

/// Runs the Go extractor, writing the IR to `out`.
fn extract(root: &Path, out: &Path) -> Result<(), String> {
    let status = Command::new("go")
        .args(["run", "./extract", "-out"])
        .arg(out)
        .current_dir(root.join("xtask").join("go"))
        .status()
        .map_err(|e| format!("cannot run `go` ({e}); install Go and put it on PATH"))?;
    if !status.success() {
        return Err(format!("the Go extractor failed ({status})"));
    }
    Ok(())
}

/// Formats generated text and adds the `too_many_lines` expectations rustfmt's line count
/// calls for.
fn finish(root: &Path, text: &str) -> Result<String, String> {
    let formatted = rustfmt(root, text)?;
    let marked = mark_long_functions(&formatted);
    let formatted = rustfmt(root, &marked)?;
    if formatted != rustfmt(root, &formatted)? {
        return Err("rustfmt output is not stable".to_owned());
    }
    Ok(formatted)
}

/// Runs rustfmt (the repository's `.rustfmt.toml`) on text.
fn rustfmt(root: &Path, text: &str) -> Result<String, String> {
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run rustfmt: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("rustfmt has no stdin")?;
    let input = text.to_owned();
    // rustfmt reads all input before writing, so writing first cannot deadlock on its output.
    stdin
        .write_all(input.as_bytes())
        .map_err(|e| format!("rustfmt input: {e}"))?;
    drop(stdin);
    let output = child
        .wait_with_output()
        .map_err(|e| format!("rustfmt: {e}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let mut dump = String::new();
        for (i, line) in text.lines().enumerate() {
            put!(dump, "{:5} {line}\n", i + 1);
        }
        return Err(format!("rustfmt failed:\n{err}\n--- input ---\n{dump}"));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("rustfmt output: {e}"))
}

/// Adds `#[expect(clippy::too_many_lines)]` to functions whose body has more than
/// [`TOO_MANY_LINES`] lines of code, counted like clippy: non-blank lines from the opening to
/// the closing brace.
fn mark_long_functions(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut marks: Vec<(usize, String)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let is_fn = [
            "fn ",
            "pub fn ",
            "pub(crate) fn ",
            "pub const fn ",
            "const fn ",
        ]
        .iter()
        .any(|p| trimmed.starts_with(p));
        if !is_fn {
            continue;
        }
        let indent = line.strip_suffix(trimmed).unwrap_or_default();
        let Some(open) =
            (i..lines.len()).find(|&j| lines.get(j).is_some_and(|l| l.trim_end().ends_with('{')))
        else {
            continue;
        };
        let close_pat = format!("{indent}}}");
        let Some(close) =
            (open..lines.len()).find(|&j| lines.get(j).is_some_and(|l| *l == close_pat))
        else {
            continue;
        };
        let count = lines
            .get(open..=close)
            .unwrap_or_default()
            .iter()
            .filter(|l| {
                let t = l.trim();
                !t.is_empty() && !t.starts_with("//")
            })
            .count();
        if count > TOO_MANY_LINES {
            marks.push((
                i,
                format!(
                    "{indent}#[expect(clippy::too_many_lines, reason = \"consistency: generated from one gophertunnel body or table\")]"
                ),
            ));
        }
    }
    let mut out = String::with_capacity(text.len() + marks.len() * 100);
    let mut next = marks.iter().peekable();
    for (i, line) in lines.iter().enumerate() {
        if let Some((_, mark)) = next.next_if(|(at, _)| *at == i) {
            out.push_str(mark);
            out.push('\n');
        }
        out.push_str(line);
        out.push('\n');
    }
    out
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
fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn create_dir(path: &PathBuf) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))
}
