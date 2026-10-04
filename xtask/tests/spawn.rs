//! P5 (PLAN.md 4; 10.2 "M2 판정식", 10.3 "M3 판정식"): the gophertunnel-based bot
//! (`xtask/go/bot`) joins an Anchor server running in this process.
//!
//! RakNet: spawns with zlib and Snappy, offline and online (a test issuer), and with a 6 MiB
//! Login; idle connections close at the login deadline and one IP gets four connections before
//! login. NetherNet: the same spawns over WebRTC, both transports at once, the fallback to RakNet
//! when NetherNet fails, the pre-login peer limit, the identity binding and PMMP's duplicate
//! login in both outcomes.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::Duration;

use anchor::config::GameMode;
use anchor::event::{PlayerDuplicateLogin, Priority};
use anchor::protocol::compression::Compression;
use anchor::{Config, Server, ServerBuilder, ShutdownHandle};
use serde_json::Value;

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

/// A port no socket uses right now for UDP or TCP. Another test process may still take it before
/// the server binds it; [`Running::start_with`] retries then.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: test tooling outside the server"
)]
fn free_port() -> u16 {
    loop {
        let tcp = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("bind a port");
        let port = tcp.local_addr().expect("local address").port();
        if std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok() {
            return port;
        }
    }
}

/// Tries at starting a server on fresh ports when another process took them first.
const START_TRIES: usize = 5;

/// How waiting for a server's first pong ended.
enum Pong {
    Answered(String),
    /// The server thread ended first, with this result.
    Ended(std::thread::Result<anchor::Result<()>>),
}

/// An Anchor server on a thread of this process.
struct Running {
    port: u16,
    shutdown: ShutdownHandle,
    thread: Option<JoinHandle<anchor::Result<()>>>,
}

impl Running {
    fn start(name: &str, config: Config, data_dir: &Path) -> Self {
        Self::start_with(name, config, data_dir, |builder| builder)
    }

    /// Starts with `edit` applied to the builder (event listeners) and waits until it answers
    /// pings, so the bot never races the start.
    fn start_with(
        name: &str,
        mut config: Config,
        data_dir: &Path,
        edit: impl Fn(ServerBuilder) -> ServerBuilder,
    ) -> Self {
        config.general.enable_ipv6 = false;
        config.general.level_name = name.to_owned();
        for attempt in 1..=START_TRIES {
            let port = free_port();
            let mut nethernet_port = free_port();
            while nethernet_port == port {
                nethernet_port = free_port();
            }
            config.general.server_port = port;
            config.general.nethernet_port = nethernet_port;
            let server = edit(Server::builder().config(config.clone()).data_dir(data_dir))
                .build()
                .expect("valid settings");
            let shutdown = server.shutdown_handle();
            let thread = spawn(move || server.run());
            let mut running = Self {
                port,
                shutdown,
                thread: Some(thread),
            };
            match running.try_pong() {
                Pong::Answered(_) => return running,
                Pong::Ended(Ok(Err(anchor::Error::Network(e))))
                    if e.kind() == std::io::ErrorKind::AddrInUse && attempt < START_TRIES => {}
                Pong::Ended(run) => panic!("the server stopped before answering pings: {run:?}"),
            }
        }
        panic!("no free ports after {START_TRIES} tries")
    }

    /// Pings until the server answers and returns the pong text.
    fn pong(&mut self) -> String {
        match self.try_pong() {
            Pong::Answered(text) => text,
            Pong::Ended(run) => panic!("the server stopped before answering pings: {run:?}"),
        }
    }

    /// Pings until the server answers (pings 300 ms apart stay under the rate limit of 5 per
    /// second), or until the server thread ends.
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: test tooling outside the server"
    )]
    fn try_pong(&mut self) -> Pong {
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
                return Pong::Answered(String::from_utf8_lossy(&buf[35..n]).into_owned());
            }
            if self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
                return Pong::Ended(self.thread.take().expect("checked").join());
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

/// The bot command for `scenario` against `server`, writing `<name>.jsonl`. Later flags in
/// `extra` override the defaults.
fn bot_command(server: &Running, scenario: &str, name: &str, extra: &[&str]) -> Command {
    let mut command = Command::new(bot());
    command
        .args(["-addr", &server.addr(), "-scenario", scenario, "-out"])
        .arg(out_file(name))
        .args(["-timeout", "60s", "-stay", "2s"])
        .args(extra);
    command
}

fn out_file(name: &str) -> PathBuf {
    work_dir().join(format!("{name}.jsonl"))
}

/// Runs a bot scenario and returns its output lines; fails the test with the bot's reason.
fn play(server: &Running, scenario: &str, name: &str, extra: &[&str]) -> String {
    let output = bot_command(server, scenario, name, extra)
        .output()
        .expect("the bot starts");
    passed(scenario, name, &output)
}

/// Starts a bot scenario without waiting for it; [`finish`] collects it. The output of an earlier
/// run is removed first, so [`wait_for_spawn`] sees only this one.
fn start_bot(server: &Running, scenario: &str, name: &str, extra: &[&str]) -> Child {
    remove_file(&out_file(name));
    bot_command(server, scenario, name, extra)
        .spawn()
        .expect("the bot starts")
}

fn finish(bot: Child, scenario: &str, name: &str) -> String {
    let output = bot.wait_with_output().expect("the bot ends");
    passed(scenario, name, &output)
}

fn passed(scenario: &str, name: &str, output: &Output) -> String {
    let lines = read(&out_file(name));
    assert!(
        output.status.success(),
        "bot {scenario} ({name}) failed: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        lines.lines().last().unwrap_or_default()
    );
    lines
}

/// The bot's final `result` line.
fn result(lines: &str) -> Value {
    let last = lines.lines().last().expect("the bot wrote a result");
    serde_json::from_str(last).expect("the result is JSON")
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
    let online = ["-token-key", key, "-kid", KID, "-iss", ISSUER];
    play(&server, "online", "online", &online);
    // Over NetherNet the offer's identity is checked against the same keys, then bound to the
    // Login key.
    let lines = play(
        &server,
        "online",
        "online-nethernet",
        &[&online[..], &["-transport", "nethernet"]].concat(),
    );
    assert_eq!(result(&lines)["transport"], "nethernet");
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

const NETHERNET: &[&str] = &["-transport", "nethernet"];

#[test]
fn the_bot_spawns_over_nethernet_with_snappy_and_with_zlib() {
    for (compression, name) in [
        (Compression::Snappy, "nn-snappy"),
        (Compression::Zlib, "nn-zlib"),
    ] {
        let server = Running::start(name, config(compression), &data_dir(name));
        let lines = play(&server, "spawn", name, NETHERNET);
        assert_eq!(result(&lines)["transport"], "nethernet");
        let id = compression.network_id();
        assert!(
            lines.contains(&format!("\"CompressionAlgorithm\":{id}")),
            "NetworkSettings announces {name}"
        );
        // The answer carries the server identity at session level, as BDS's does.
        let identity = lines
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("JSON line"))
            .find(|line| line["type"] == "identity")
            .expect("an identity line");
        assert_eq!(identity["session_level"], true, "{identity}");
        server.stop();
    }
}

#[test]
fn a_six_mib_login_spawns_over_nethernet() {
    let server = Running::start(
        "nn-big-skin",
        config(Compression::Snappy),
        &data_dir("nn-big-skin"),
    );
    play(&server, "big-skin", "nn-big-skin", NETHERNET);
    server.stop();
}

#[test]
fn raknet_and_nethernet_players_join_at_once() {
    let server = Running::start("both", config(Compression::Snappy), &data_dir("both"));
    let stay = ["-stay", "4s"];
    let raknet = start_bot(
        &server,
        "spawn",
        "both-raknet",
        &[&stay[..], &["-name", "ByRaknet"]].concat(),
    );
    let nethernet = start_bot(
        &server,
        "spawn",
        "both-nethernet",
        &[&stay[..], NETHERNET, &["-name", "ByNethernet"]].concat(),
    );
    let raknet = finish(raknet, "spawn", "both-raknet");
    let nethernet = finish(nethernet, "spawn", "both-nethernet");
    assert_eq!(result(&raknet)["transport"], "raknet");
    assert_eq!(result(&nethernet)["transport"], "nethernet");
    server.stop();
}

#[test]
fn a_failed_nethernet_negotiation_falls_back_to_raknet() {
    let mut config = config(Compression::Snappy);
    // No IPv6 socket serves this advertised address, so every offer is answered with code 11.
    config.general.public_ip = Some(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)));
    let server = Running::start("fallback", config, &data_dir("fallback"));
    let lines = play(&server, "spawn", "fallback", &["-transport", "auto"]);
    let result = result(&lines);
    assert_eq!(result["transport"], "raknet", "{result}");
    assert!(
        result["nethernet_error"]
            .as_str()
            .is_some_and(|e| e.ends_with("error code: 11")),
        "{result}"
    );
    server.stop();
}

#[test]
fn a_login_with_another_key_than_the_offer_is_refused() {
    let server = Running::start("bind", config(Compression::Snappy), &data_dir("bind"));
    play(&server, "bind-mismatch", "bind-mismatch", NETHERNET);
    server.stop();
}

#[test]
fn one_ip_gets_four_nethernet_peers_before_login() {
    let server = Running::start(
        "nn-limit",
        config(Compression::Snappy),
        &data_dir("nn-limit"),
    );
    // Offline offers need no identity; these peers never connect, so they stay counted until
    // their establishment deadline.
    for n in 0..4 {
        let (status, body) = post_offer(server.port);
        assert_eq!(status, 200, "offer {n}: {body}");
        assert!(body.starts_with("v=0"), "offer {n}: {body}");
    }
    assert_eq!(post_offer(server.port).0, 503);
    server.stop();
}

#[test]
fn nethernet_sessions_end_cleanly_and_the_join_status_counts_no_one_after_they_leave() {
    let server = Running::start(
        "nn-cycles",
        config(Compression::Snappy),
        &data_dir("nn-cycles"),
    );
    let players = |server: &Running| -> Value {
        let (status, body) = http(server.port, "GET /v1/join HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 200, "{body}");
        serde_json::from_str::<Value>(&body).expect("JSON status")["players"].clone()
    };
    assert_eq!(players(&server), 0);
    for cycle in 0..5 {
        let name = format!("nn-cycle-{cycle}");
        play(
            &server,
            "spawn",
            &name,
            &[NETHERNET, &["-stay", "0s"]].concat(),
        );
    }
    // Sessions report their end to the accept task a moment after the bot closes.
    let mut counts = Vec::new();
    for _ in 0..20 {
        counts.push(players(&server));
        if counts.last() == Some(&Value::from(0)) {
            break;
        }
        pause();
    }
    assert_eq!(
        counts.last(),
        Some(&Value::from(0)),
        "players still counted: {counts:?}"
    );
    server.stop();
}

/// The name both bots of a duplicate-login test use.
const TWIN: &str = "Twin";

#[test]
fn a_duplicate_login_replaces_the_player_already_online() {
    let server = Running::start(
        "duplicate",
        config(Compression::Snappy),
        &data_dir("duplicate"),
    );
    // The first player joins over RakNet and waits to be replaced by the second over NetherNet.
    let first = start_bot(
        &server,
        "kicked",
        "dup-first",
        &["-name", TWIN, "-stay", "30s"],
    );
    wait_for_spawn("dup-first");
    let second = play(
        &server,
        "spawn",
        "dup-second",
        &[NETHERNET, &["-name", TWIN]].concat(),
    );
    assert_eq!(result(&second)["transport"], "nethernet");
    let first = finish(first, "kicked", "dup-first");
    let first = result(&first);
    assert_eq!(
        first["disconnect_message"], "Logged in from other location",
        "{first}"
    );
    server.stop();
}

#[test]
fn a_cancelled_duplicate_login_turns_the_new_player_away() {
    let server = Running::start_with(
        "duplicate-cancelled",
        config(Compression::Snappy),
        &data_dir("duplicate-cancelled"),
        |builder| {
            builder.on(Priority::Normal, |e: &mut PlayerDuplicateLogin, _| {
                e.cancel();
            })
        },
    );
    let first = start_bot(
        &server,
        "spawn",
        "dupc-first",
        &["-name", TWIN, "-stay", "8s"],
    );
    wait_for_spawn("dupc-first");
    let second = play(
        &server,
        "rejected",
        "dupc-second",
        &[NETHERNET, &["-name", TWIN]].concat(),
    );
    assert_eq!(
        result(&second)["disconnect_message"],
        "Logged in from other location"
    );
    // The first player stays connected for its whole stay without a disconnect.
    finish(first, "spawn", "dupc-first");
    server.stop();
}

/// Waits until the bot writing `<name>.jsonl` has sent `SetLocalPlayerAsInitialised` (the bot
/// flushes its output right after).
fn wait_for_spawn(name: &str) {
    for _ in 0..300 {
        if read(&out_file(name)).contains("\"name\":\"SetLocalPlayerAsInitialised\"") {
            return;
        }
        pause();
    }
    panic!("{name} did not spawn");
}

/// A NetherNet offer without an identity, which offline servers accept.
const OFFER: &str = "v=0\r\n\
    o=- 1 2 IN IP4 127.0.0.1\r\n\
    s=-\r\n\
    t=0 0\r\n\
    a=group:BUNDLE 0\r\n\
    m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
    c=IN IP4 0.0.0.0\r\n\
    a=candidate:1 1 udp 2130706431 127.0.0.1 9 typ host\r\n\
    a=ice-ufrag:abcd\r\n\
    a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
    a=fingerprint:sha-256 8A:74:FB:69:94:62:1E:0C:53:90:18:B6:DC:D4:3C:FC:75:64:24:E1:42:F3:95:55:FB:F9:52:2D:E5:EE:63:F2\r\n\
    a=setup:actpass\r\n\
    a=mid:0\r\n\
    a=sctp-port:5000\r\n\
    a=max-message-size:262144\r\n";

fn post_offer(port: u16) -> (u16, String) {
    let request = format!(
        "POST /v1/join/1234 HTTP/1.1\r\nHost: x\r\nContent-Type: application/sdp\r\n\
         Content-Length: {}\r\n\r\n{OFFER}",
        OFFER.len()
    );
    http(port, &request)
}

/// Sends one HTTP request to the signaling port and returns the status and body.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: test tooling outside the server"
)]
fn http(port: u16, request: &str) -> (u16, String) {
    let mut stream =
        std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect to signaling");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("set a read timeout");
    stream
        .write_all(request.as_bytes())
        .expect("send the request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read the response");
    let status = response
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("a status line");
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    (status, body)
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
fn remove_file(path: &Path) {
    match std::fs::remove_file(path) {
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

/// A stored (uncompressed) zip archive of `files`, the smallest a pack can be.
fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    let (mut out, mut directory) = (Vec::new(), Vec::new());
    for (name, data) in files {
        let offset = u32::try_from(out.len()).unwrap();
        let mut common = Vec::new();
        common.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // version, flags, stored, time, date
        common.extend_from_slice(&crc32(data).to_le_bytes());
        let len = u32::try_from(data.len()).unwrap().to_le_bytes();
        common.extend_from_slice(&len);
        common.extend_from_slice(&len);
        common.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        common.extend_from_slice(&[0, 0]); // extra
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&common);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        directory.extend_from_slice(b"PK\x01\x02\x14\x00");
        directory.extend_from_slice(&common);
        directory.extend_from_slice(&[0; 10]); // comment, disk, attributes
        directory.extend_from_slice(&offset.to_le_bytes());
        directory.extend_from_slice(name.as_bytes());
    }
    let offset = u32::try_from(out.len()).unwrap();
    let count = u16::try_from(files.len()).unwrap().to_le_bytes();
    out.extend_from_slice(&directory);
    out.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    out.extend_from_slice(&count);
    out.extend_from_slice(&count);
    out.extend_from_slice(&u32::try_from(directory.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

/// A resource pack with `uuid` and `size` bytes of texture that do not compress.
fn pack_bytes(uuid: &str, size: usize) -> Vec<u8> {
    let manifest = format!(
        r#"{{"format_version": 2, "header": {{"name": "P5", "uuid": "{uuid}", "version": [1, 0, 0]}},
        "modules": [{{"type": "resources", "uuid": "{uuid}", "version": [1, 0, 0]}}]}}"#
    );
    let mut seed = 0x9e37_79b9_u32;
    let texture: Vec<u8> = (0..size)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed.to_le_bytes()[0]
        })
        .collect();
    zip(&[
        ("manifest.json", manifest.as_bytes()),
        ("textures/noise.png", &texture),
    ])
}

#[test]
fn the_bot_downloads_the_resource_packs_over_both_transports() {
    let dir = data_dir("packs");
    let file = dir.join("a.zip");
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    std::fs::write(
        &file,
        pack_bytes("6f1d9c2a-3b4e-4f50-8a61-7c2d3e4f5a6b", 400_000),
    )
    .unwrap();
    let mut config = config(Compression::Snappy);
    config.general.resource_packs = vec![file];
    config.general.force_resources = true;
    // A pack of exactly one 128 KiB chunk, given in code.
    let code = pack_bytes("9e8d7c6b-5a49-4382-a170-6f5e4d3c2b1a", 131_072);
    let server = Running::start_with("packs", config, &dir, |builder| {
        builder.resource_pack(anchor::pack::Pack::from_bytes(code.clone()).expect("a pack"))
    });
    for (transport, name) in [("raknet", "packs-raknet"), ("nethernet", "packs-nethernet")] {
        let lines = play(&server, "spawn", name, &["-transport", transport]);
        let result = result(&lines);
        assert_eq!(result["packs_offered"], 2, "{transport}");
        assert_eq!(result["packs_downloaded"], 2, "{transport}");
        assert_eq!(result["spawned"], true, "{transport}");
    }
    server.stop();
}
