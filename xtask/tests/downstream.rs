//! A user project that depends on Anchor (Anchor is a library) gets the fixed
//! copies in `vendor/` with no configuration of its own: the project is created
//! outside the repository, so nothing of Anchor's workspace applies to it, and every fixed crate
//! and every crate that names one in its API must resolve to the copy, never to crates.io.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::path::Path;
use std::process::Command;

use serde_json::Value;

/// The repository root.
fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is in the repository")
}

/// The copies in `vendor/`, one folder each named after its crate: the fixed crates and the
/// unchanged crates between them and Anchor.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
fn copies() -> Vec<String> {
    let vendor = root().join("vendor");
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&vendor).expect("read vendor/") {
        let entry = entry.expect("a vendor/ entry");
        if entry.file_type().expect("a vendor/ entry type").is_dir() {
            names.push(
                entry
                    .file_name()
                    .into_string()
                    .expect("a UTF-8 folder name"),
            );
        }
    }
    names.sort();
    names
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
fn write(path: &Path, text: &str) {
    std::fs::write(path, text).expect("write a test file");
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
fn create_dir(path: &Path) {
    std::fs::create_dir_all(path).expect("create a test directory");
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
fn remove_dir(path: &Path) {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("remove {}: {e}", path.display()),
    }
}

/// Resolves a fresh user project that depends on Anchor by path and returns `cargo metadata`.
fn user_project_metadata() -> Value {
    // The OS temporary directory is outside the repository, so no Anchor manifest or
    // `.cargo/config.toml` above it takes part.
    let dir = std::env::temp_dir().join(format!("anchor-downstream-{}", std::process::id()));
    remove_dir(&dir);
    create_dir(&dir.join("src"));
    write(
        &dir.join("Cargo.toml"),
        &format!(
            "[package]\nname = \"user\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
             [dependencies]\nanchor = {{ path = '{}' }}\n",
            root().display()
        ),
    );
    write(&dir.join("src").join("main.rs"), "fn main() {}\n");
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--offline"])
        .current_dir(&dir)
        .output()
        .expect("cargo starts");
    remove_dir(&dir);
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata prints JSON")
}

#[test]
fn a_user_project_resolves_every_vendored_crate_to_its_copy() {
    let metadata = user_project_metadata();
    let packages = metadata["packages"].as_array().expect("packages");
    let vendor = root().join("vendor");
    for name in copies() {
        let found: Vec<&Value> = packages
            .iter()
            .filter(|p| p["name"] == name.as_str())
            .collect();
        let sources: Vec<String> = found
            .iter()
            .map(|p| match p["source"].as_str() {
                Some(source) => source.to_owned(),
                None => p["manifest_path"].as_str().unwrap_or_default().to_owned(),
            })
            .collect();
        let manifest = vendor.join(&name).join("Cargo.toml");
        assert!(
            found.len() == 1
                && found[0]["source"].is_null()
                && found[0]["manifest_path"].as_str().map(Path::new) == Some(manifest.as_path()),
            "{name} resolves to {sources:?}, not only to {}",
            manifest.display()
        );
    }
}
