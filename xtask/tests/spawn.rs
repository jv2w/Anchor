//! P5 over RakNet (PLAN.md 4, 10.2 "M2 판정식"): the gophertunnel-based bot (`xtask/go/bot`)
//! joins an Anchor server running in this process and spawns, with zlib and Snappy, offline and
//! online (a test issuer), and with a 6 MiB Login; idle connections close at the login deadline
//! and one IP gets four connections before login.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::Duration;

use anchor::config::GameMode;
use anchor::protocol::compression::Compression;
use anchor::{Config, Server, ShutdownHandle};

/// RakNet's offline message magic.
const MAGIC: [u8; 16] = [
    0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56, 0x78,
];
/// The issuer the test issuer signs with; the bot puts it in its tokens.
const ISSUER: &str = "https://issuer.invalid/";
const KID: &str = "p5-key";

/// The directory for this test binary's files.
fn work_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("p5")
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
            .unwrap_or_else(|e| panic!("cannot run `go` (P5 needs Go on PATH): {e}"));
        assert!(
            output.status.success(),
            "go build ./bot failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        exe
    })
}

/// A port no socket uses right now, for both UDP families.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: test tooling outside the server"
)]
fn free_port() -> u16 {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind a free port");
    socket.local_addr().expect("local address").port()
}

/// An Anchor server on a thread of this process.
struct Running {
    port: u16,
    shutdown: ShutdownHandle,
    thread: Option<JoinHandle<anchor::Result<()>>>,
}

impl Running {
    fn start(name: &str, mut config: Config, data_dir: &Path) -> Self {
        let port = free_port();
        config.general.server_port = port;
        config.general.enable_ipv6 = false;
        config.general.level_name = name.to_owned();
        let server = Server::builder()
            .config(config)
            .data_dir(data_dir)
            .build()
            .expect("valid settings");
        let shutdown = server.shutdown_handle();
        let thread = spawn(move || server.run());
        let mut running = Self {
            port,
            shutdown,
            thread: Some(thread),
        };
        running.wait_for_pong();
        running
    }

    /// Pings until the server answers, so the bot never races the start.
    fn wait_for_pong(&mut self) {
        self.pong();
    }

    /// Pings until the server answers (pings 300 ms apart stay under the rate limit of 5 per
    /// second) and returns the pong text.
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: test tooling outside the server"
    )]
    fn pong(&mut self) -> String {
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
        for _ in 0..100 {
            socket
                .send_to(&ping, (Ipv4Addr::LOCALHOST, self.port))
                .expect("send a ping");
            // 0x1c, time, server GUID, magic, then the length-prefixed pong text.
            if let Ok((n, _)) = socket.recv_from(&mut buf)
                && n > 35
                && buf[0] == 0x1c
            {
                return String::from_utf8_lossy(&buf[35..n]).into_owned();
            }
            if self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
                let run = self.thread.take().expect("checked").join();
                panic!("the server stopped before answering pings: {run:?}");
            }
            // An ICMP "port unreachable" makes `recv_from` fail at once on Windows.
            pause();
        }
        panic!("the server did not answer pings");
    }

    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn stop(mut self) {
        self.shutdown.shutdown();
        let result = self.thread.take().expect("running").join();
        result
            .expect("the server thread")
            .expect("run ends cleanly");
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // A failed assertion still stops the server so the test process can end.
        self.shutdown.shutdown();
    }
}

/// Settings for P5: offline unless a test says otherwise, small view distance.
fn config(compression: Compression) -> Config {
    let mut config = Config::default();
    config.general.xbox_auth = false;
    config.general.view_distance = 4;
    config.general.gamemode = GameMode::Creative;
    config.developer.compression = compression;
    config
}

/// Runs a bot scenario and returns its output lines; fails the test with the bot's reason.
fn play(server: &Running, scenario: &str, name: &str, extra: &[&str]) -> String {
    let out = work_dir().join(format!("{name}.jsonl"));
    let output = Command::new(bot())
        .args(["-addr", &server.addr(), "-scenario", scenario, "-out"])
        .arg(&out)
        .args(["-timeout", "60s", "-stay", "2s"])
        .args(extra)
        .output()
        .expect("the bot starts");
    let lines = read(&out);
    assert!(
        output.status.success(),
        "bot {scenario} failed: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        lines.lines().last().unwrap_or_default()
    );
    lines
}

fn data_dir(name: &str) -> PathBuf {
    let dir = work_dir().join(name);
    remove_dir(&dir);
    create_dir(&dir);
    dir
}

#[test]
fn the_bot_spawns_with_snappy_and_with_zlib() {
    for (compression, name) in [(Compression::Snappy, "snappy"), (Compression::Zlib, "zlib")] {
        let server = Running::start(name, config(compression), &data_dir(name));
        let lines = play(&server, "spawn", name, &[]);
        let id = compression.network_id();
        assert!(
            lines.contains(&format!("\"CompressionAlgorithm\":{id}")),
            "NetworkSettings announces {name}"
        );
        server.stop();
    }
}

#[test]
fn sessions_end_cleanly_and_the_pong_counts_no_one_after_they_leave() {
    let mut server = Running::start("cycles", config(Compression::Snappy), &data_dir("cycles"));
    let online = |server: &mut Running| server.pong().split(';').nth(4).map(str::to_owned);
    assert_eq!(online(&mut server).as_deref(), Some("0"));
    for cycle in 0..5 {
        play(
            &server,
            "spawn",
            &format!("cycle-{cycle}"),
            &["-stay", "0s"],
        );
    }
    // Sessions report their end to the accept task a moment after the bot closes.
    let counts: Vec<_> = (0..20).map(|_| online(&mut server)).collect();
    assert!(
        counts.iter().any(|n| n.as_deref() == Some("0")),
        "players still counted: {counts:?}"
    );
    server.stop();
}

#[test]
fn a_six_mib_login_spawns() {
    let server = Running::start(
        "big-skin",
        config(Compression::Snappy),
        &data_dir("big-skin"),
    );
    play(&server, "big-skin", "big-skin", &[]);
    server.stop();
}

#[test]
fn a_test_issuer_token_logs_in_online() {
    let dir = data_dir("online");
    let output = Command::new(bot())
        .args(["issuer", "-dir"])
        .arg(&dir)
        .args(["-kid", KID, "-iss", ISSUER])
        .output()
        .expect("the bot starts");
    assert!(
        output.status.success(),
        "bot issuer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut config = config(Compression::Snappy);
    config.general.xbox_auth = true;
    let server = Running::start("online", config, &dir);
    let key = dir.join("issuer.pem");
    let key = key.to_str().expect("utf-8 path");
    play(
        &server,
        "online",
        "online",
        &["-token-key", key, "-kid", KID, "-iss", ISSUER],
    );
    server.stop();
}

#[test]
fn an_idle_connection_is_closed_at_the_login_deadline() {
    let server = Running::start("idle", config(Compression::Snappy), &data_dir("idle"));
    play(&server, "idle", "idle", &[]);
    server.stop();
}

#[test]
fn one_ip_gets_four_connections_before_login() {
    let server = Running::start(
        "prelogin-limit",
        config(Compression::Snappy),
        &data_dir("prelogin-limit"),
    );
    play(&server, "prelogin-limit", "prelogin-limit", &[]);
    server.stop();
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test runs the server on its own thread"
)]
fn spawn<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> JoinHandle<T> {
    std::thread::spawn(f)
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
