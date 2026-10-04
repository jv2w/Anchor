//! The examples (PLAN.md 10.8 "예제 4개"): each example binary runs in a fresh folder with its own
//! `anchor.toml`, as a user would run it, and the bot (`xtask/go/bot`) plays the example's script
//! `xtask/data/examples/<name>.txt` (`-scenario script`), checking what a player sees.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

/// RakNet's offline message magic.
const MAGIC: [u8; 16] = [
    0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56, 0x78,
];
/// Tries at starting an example on fresh ports when another process took them first.
const START_TRIES: usize = 5;

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

/// Builds the bot once per test process. Fails the test if Go is missing (PLAN.md 4).
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

/// A port no socket uses right now for UDP or TCP. Another process may still take it before the
/// example binds it; [`Example::start`] retries then.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: test tooling outside the server"
)]
fn free_port() -> u16 {
    loop {
        let tcp = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind a port");
        let port = tcp.local_addr().expect("local address").port();
        if std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).is_ok() {
            return port;
        }
    }
}

/// An example binary running as a child process in its own folder. Dropping it kills that
/// child, and only that child.
struct Example {
    name: String,
    dir: PathBuf,
    port: u16,
    child: Child,
}

impl Example {
    /// Builds and starts the example `name` in a fresh folder, and waits until it answers pings,
    /// so the bot never races the start.
    fn start(name: &str) -> Self {
        let exe = build_example(name);
        let dir = work_dir().join(name);
        remove_dir(&dir);
        create_dir(&dir);
        for _ in 0..START_TRIES {
            let port = free_port();
            let mut nethernet_port = free_port();
            while nethernet_port == port {
                nethernet_port = free_port();
            }
            write(&dir.join("anchor.toml"), &settings(port, nethernet_port));
            let child = Command::new(&exe)
                .current_dir(&dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(create_file(&dir.join("stderr.txt")))
                .spawn()
                .expect("the example starts");
            let mut example = Self {
                name: name.to_owned(),
                dir: dir.clone(),
                port,
                child,
            };
            if example.answers_pings() {
                return example;
            }
        }
        panic!("the example {name} found no free ports after {START_TRIES} tries")
    }

    /// Pings until the example answers (pings 300 ms apart stay under the rate limit of 5 per
    /// second): true when it does, false when it exited first (its ports were taken). Fails
    /// the test if it neither answers nor exits.
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: test tooling outside the server"
    )]
    fn answers_pings(&mut self) -> bool {
        let socket =
            std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind the ping socket");
        socket
            .set_read_timeout(Some(Duration::from_millis(300)))
            .expect("set a read timeout");
        let mut ping = vec![0x01];
        ping.extend_from_slice(&0_i64.to_be_bytes());
        ping.extend_from_slice(&MAGIC);
        ping.extend_from_slice(&1_i64.to_be_bytes());
        let mut buf = [0; 1500];
        for _ in 0..200 {
            socket
                .send_to(&ping, (Ipv4Addr::LOCALHOST, self.port))
                .expect("send a ping");
            if let Ok((n, _)) = socket.recv_from(&mut buf)
                && n > 35
                && buf[0] == 0x1c
            {
                return true;
            }
            if !self.is_running() {
                return false;
            }
            // An ICMP "port unreachable" makes `recv_from` fail at once on Windows.
            pause();
        }
        panic!(
            "the example {} did not answer pings: {}",
            self.name,
            read(&self.dir.join("stderr.txt"))
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

/// The example's settings: loopback ports, offline, a small view, no console.
fn settings(port: u16, nethernet_port: u16) -> String {
    format!(
        "[general]\n\
         server-ip = \"127.0.0.1\"\n\
         enable-ipv6 = false\n\
         server-port = {port}\n\
         nethernet-port = {nethernet_port}\n\
         xbox-auth = false\n\
         view-distance = 4\n\
         \n\
         [developer]\n\
         console-input = false\n"
    )
}

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
