//! The examples: each example binary runs in a fresh folder with its own `anchor.toml`, as a user
//! would run it, and the bot (`xtask/go/bot`) plays the example's script
//! `xtask/data/examples/<name>.txt` (`-scenario script`), checking what a player sees.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

/// The directory for this test binary's files.
fn work_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("examples")
}

/// The repository root.
fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is in the repository")
}

/// Builds the bot once per test process. Fails the test if Go is missing.
fn bot() -> &'static Path {
    static BOT: OnceLock<PathBuf> = OnceLock::new();
    BOT.get_or_init(|| {
        let dir = work_dir();
        create_dir(&dir);
        let exe = dir.join(format!("bot{}", std::env::consts::EXE_SUFFIX));
        let output = Command::new("go")
            .args(["build", "-o"])
            .arg(&exe)
            .arg("./bot")
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("go"))
            .output()
            .unwrap_or_else(|e| panic!("cannot run `go` (the example tests need Go on PATH): {e}"));
        assert!(
            output.status.success(),
            "go build ./bot failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        exe
    })
}

/// Builds the example `name` with the cargo running these tests (same target directory and
/// profile) and returns its executable.
fn build_example(name: &str) -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args(["build", "--package", "anchor", "--example", name])
        .arg("--message-format=json-render-diagnostics")
        .current_dir(root())
        .stderr(Stdio::inherit())
        .output()
        .expect("cargo starts");
    assert!(
        output.status.success(),
        "cargo build --example {name} failed"
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == name)
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
        .unwrap_or_else(|| panic!("cargo built no executable for the example {name}"))
}

/// An example binary running as a child process in its own folder. Dropping it kills that
/// child, and only that child.
struct Example {
    name: String,
    dir: PathBuf,
    /// The RakNet port the example logged.
    port: u16,
    child: Child,
}

impl Example {
    /// Builds and starts the example `name` in a fresh folder on ports the system picks when the
    /// example binds them (port 0), so no other process can take them first, and waits until it
    /// has logged its RakNet address, so the bot never races the start.
    fn start(name: &str) -> Self {
        let exe = build_example(name);
        let dir = work_dir().join(name);
        remove_dir(&dir);
        create_dir(&dir);
        write(&dir.join("anchor.toml"), SETTINGS);
        let child = Command::new(&exe)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(create_file(&dir.join("stderr.txt")))
            .spawn()
            .expect("the example starts");
        // Owned before the wait, so a failed wait still kills the child.
        let mut example = Self {
            name: name.to_owned(),
            dir,
            port: 0,
            child,
        };
        example.port = example.raknet_port();
        example
    }

    /// The port of the example's start log line for RakNet (one line per opened address, RakNet
    /// last). Fails the test if the example exits first; the limit only catches an example that
    /// neither starts nor exits.
    fn raknet_port(&mut self) -> u16 {
        let stderr = self.dir.join("stderr.txt");
        for _ in 0..300 {
            let log = read(&stderr);
            let addr = log
                .lines()
                .filter(|line| line.contains("listening for RakNet"))
                .find_map(|line| line.split_once("addr=")?.1.split([' ', ')']).next());
            if let Some(addr) = addr {
                return addr
                    .parse::<SocketAddr>()
                    .unwrap_or_else(|e| panic!("the logged address {addr}: {e}"))
                    .port();
            }
            assert!(
                self.is_running(),
                "the example {} stopped before opening its addresses: {log}",
                self.name
            );
            pause();
        }
        panic!(
            "the example {} did not open its addresses: {}",
            self.name,
            read(&stderr)
        );
    }

    /// Plays `xtask/data/examples/<name>.txt` with the bot and fails the test with the bot's
    /// reason if it does not pass. The example must still be running at the end.
    fn play(&mut self) {
        let script = root()
            .join("xtask")
            .join("data")
            .join("examples")
            .join(format!("{}.txt", self.name));
        let out = self.dir.join("bot.jsonl");
        let output = Command::new(bot())
            .args(["-addr", &format!("127.0.0.1:{}", self.port)])
            .args(["-scenario", "script", "-script"])
            .arg(&script)
            .args(["-timeout", "50s", "-out"])
            .arg(&out)
            .output()
            .expect("the bot starts");
        let lines = read(&out);
        assert!(
            output.status.success(),
            "the {} script failed: {}\n{}",
            self.name,
            String::from_utf8_lossy(&output.stderr),
            lines.lines().last().unwrap_or_default()
        );
        assert!(
            self.is_running(),
            "the example {} stopped: {}",
            self.name,
            read(&self.dir.join("stderr.txt"))
        );
    }

    /// Whether the example has not exited.
    fn is_running(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("the example's status")
            .is_none()
    }
}

impl Drop for Example {
    fn drop(&mut self) {
        // Also on a failed assertion, so the test process can end.
        if self.child.kill().is_ok() {
            let _status = self.child.wait();
        }
    }
}

/// The example's settings: loopback ports the system picks, offline, a small view, no console.
const SETTINGS: &str = "[general]\n\
    server-ip = \"127.0.0.1\"\n\
    enable-ipv6 = false\n\
    server-port = 0\n\
    nethernet-port = 0\n\
    xbox-auth = false\n\
    view-distance = 4\n\
    \n\
    [developer]\n\
    console-input = false\n";

#[test]
fn the_pvp_example_gives_the_kit_and_shows_the_scoreboard() {
    Example::start("pvp").play();
}

#[test]
fn the_minigame_example_counts_down_plays_a_round_and_sums_it_up() {
    Example::start("minigame").play();
}

#[test]
fn the_lobby_example_opens_the_menu_shop_and_arena() {
    Example::start("lobby").play();
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

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
fn write(path: &Path, text: &str) {
    std::fs::write(path, text).expect("write a test file");
}

#[expect(
    clippy::disallowed_types,
    reason = "off-tick: test tooling outside the server"
)]
fn create_file(path: &Path) -> std::fs::File {
    std::fs::File::create(path).expect("create a test file")
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test waits between pings"
)]
fn pause() {
    std::thread::sleep(Duration::from_millis(100));
}

#[test]
fn the_custom_example_serves_its_pack_items_blocks_and_entities() {
    Example::start("custom").play();
}
