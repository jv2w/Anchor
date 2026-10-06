//! P5: the gophertunnel-based bot (`xtask/go/bot`) joins an Anchor server running in this process.
//!
//! RakNet: spawns with zlib and Snappy, offline and online (a test issuer), and with a 6 MiB
//! Login; idle connections close at the login deadline and one IP gets four connections before
//! login. NetherNet: the same spawns over WebRTC, both transports at once, the fallback to RakNet
//! when NetherNet fails, the pre-login peer limit, the identity binding and PMMP's duplicate
//! login in both outcomes. The jigsaw and biome data on the wire equal `CloudburstMC`'s. Sessions
//! that logged in but have not joined are capped at `max-players`, and the server's stop reaches
//! them with the shutdown message.
//!
//! The servers' events down to debug go to one log of this process ([`Recorder`]): a failed test
//! shows its server's lines, and the session cycle tests read in it that each connection ended
//! by the bot's close, not at the idle timeout.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::{MutexGuard, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use anchor::config::GameMode;
use anchor::event::{PlayerDuplicateLogin, PlayerQuit, Priority};
use anchor::protocol::compression::Compression;
use anchor::{Config, Server, ServerBuilder, ShutdownHandle};
use serde_json::Value;
use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::{Event, Level, Metadata, Subscriber, span};

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
            .unwrap_or_else(|e| panic!("cannot run `go` (P5 needs Go on PATH): {e}"));
        assert!(
            output.status.success(),
            "go build ./bot failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        exe
    })
}

/// The addresses the starting server logs, one line per opened address (`listening …
/// addr=…`). The servers of this process run without Anchor's console log, so their events reach
/// [`Recorder`], the global subscriber; [`START`] lets one server start at a time, so the lines
/// logged while it is held are that server's.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: the subscriber hears the server's threads"
)]
static LISTENED: std::sync::Mutex<Vec<(String, SocketAddr)>> = std::sync::Mutex::new(Vec::new());
/// Held while one server starts, until it has logged its addresses.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: the tests of this process start servers on their own threads"
)]
static START: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// The server log: Anchor's events down to debug of every server of this process, in order.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: the subscriber hears the server's threads"
)]
static LOG: std::sync::Mutex<Log> = std::sync::Mutex::new(Log {
    next: 0,
    lines: VecDeque::new(),
});
/// The lines [`LOG`] keeps; older ones are dropped.
const LOG_LINES: usize = 50_000;
/// The last lines of the log a failed test shows.
const SHOWN_LINES: usize = 300;

/// The last [`LOG_LINES`] lines of the server log and the number of the next one.
struct Log {
    next: u64,
    lines: VecDeque<String>,
}

impl Log {
    /// The lines from line number `from` on.
    fn since(&self, from: u64) -> impl Iterator<Item = &String> {
        let first = self.next.saturating_sub(self.lines.len() as u64);
        let skip = usize::try_from(from.saturating_sub(first)).unwrap_or(usize::MAX);
        self.lines.iter().skip(skip)
    }
}

/// Keeps Anchor's events down to debug in [`LOG`], and the `listening` info events of its
/// networking in [`LISTENED`].
struct Recorder;

impl Subscriber for Recorder {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.is_event()
            && *metadata.level() <= Level::DEBUG
            && metadata.target().starts_with("anchor")
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::DEBUG)
    }

    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}

    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut line = Line::default();
        event.record(&mut line);
        let metadata = event.metadata();
        if let Some(addr) = line.addr
            && line.message.starts_with("listening")
        {
            lock(&LISTENED).push((line.message.clone(), addr));
        }
        let text = format!(
            "{} {}: {}{}",
            metadata.level(),
            metadata.target(),
            line.message,
            line.fields
        );
        let mut log = lock(&LOG);
        if log.lines.len() == LOG_LINES {
            log.lines.pop_front();
        }
        log.lines.push_back(text);
        log.next += 1;
    }

    fn enter(&self, _: &span::Id) {}

    fn exit(&self, _: &span::Id) {}
}

/// One event: its message, its `addr` field and all its fields as ` name=value`.
#[derive(Default)]
struct Line {
    message: String,
    addr: Option<SocketAddr>,
    fields: String,
}

impl Visit for Line {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => self.message = format!("{value:?}"),
            name => {
                if name == "addr" {
                    self.addr = format!("{value:?}").parse().ok();
                }
                std::fmt::Write::write_fmt(&mut self.fields, format_args!(" {name}={value:?}"))
                    .expect("writing to a String");
            }
        }
    }
}

/// Locks `mutex`; a test that failed while holding it left nothing half done.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: test tooling outside the server"
)]
fn lock<T>(mutex: &std::sync::Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An Anchor server on a thread of this process.
struct Running {
    /// The RakNet (and server list ping) port.
    raknet_port: u16,
    /// The NetherNet signaling port.
    signaling_port: u16,
    shutdown: ShutdownHandle,
    thread: Option<JoinHandle<anchor::Result<()>>>,
    /// The number of the first server log line logged after it started.
    log_from: u64,
}

impl Running {
    fn start(name: &str, config: Config, data_dir: &Path) -> Self {
        Self::start_with(name, config, data_dir, |builder| builder)
    }

    /// Starts with `edit` applied to the builder (event listeners) on ports the system picks
    /// when the server binds them (port 0), so no other process can take them first, and waits
    /// until the server has logged the addresses it opened, so the bot never races the start.
    fn start_with(
        name: &str,
        mut config: Config,
        data_dir: &Path,
        edit: impl Fn(ServerBuilder) -> ServerBuilder,
    ) -> Self {
        static SUBSCRIBER: OnceLock<()> = OnceLock::new();
        SUBSCRIBER.get_or_init(|| {
            tracing::subscriber::set_global_default(Recorder).expect("the only global subscriber");
        });
        config.general.enable_ipv6 = false;
        config.general.level_name = name.to_owned();
        config.general.server_port = 0;
        config.general.nethernet_port = 0;
        let server = edit(Server::builder().config(config).data_dir(data_dir))
            .without_console_log()
            .build()
            .expect("valid settings");
        let shutdown = server.shutdown_handle();
        let _one = lock(&START);
        lock(&LISTENED).clear();
        let log_from = lock(&LOG).next;
        let mut thread = Some(spawn(move || server.run()));
        // RakNet opens last; the limit only catches a server that neither starts nor stops.
        for _ in 0..300 {
            let listened = lock(&LISTENED);
            let find = |what: &str| {
                listened
                    .iter()
                    .find(|(message, _)| message.contains(what))
                    .map(|(_, addr)| addr.port())
            };
            if let (Some(raknet_port), Some(signaling_port)) = (find("pings"), find("signaling")) {
                return Self {
                    raknet_port,
                    signaling_port,
                    shutdown,
                    thread,
                    log_from,
                };
            }
            drop(listened);
            if thread.as_ref().is_some_and(JoinHandle::is_finished) {
                let run = thread.take().expect("checked").join();
                panic!("the server stopped before opening its addresses: {run:?}");
            }
            pause();
        }
        // Stop it while `START` is still held: a server that ends logs before the next test
        // clears `LISTENED`, so only a server hung for good, which never logs, is left.
        shutdown.shutdown();
        let ended = (0..300).any(|_| {
            let ended = thread.as_ref().is_some_and(JoinHandle::is_finished);
            if !ended {
                pause();
            }
            ended
        });
        let state = if ended { "ended" } else { "still runs" };
        panic!("the server did not open its addresses; after a shutdown its thread {state}");
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
                .send_to(&ping, (Ipv4Addr::LOCALHOST, self.raknet_port))
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
        format!("127.0.0.1:{}", self.raknet_port)
    }

    fn signaling_addr(&self) -> String {
        format!("127.0.0.1:{}", self.signaling_port)
    }

    /// Whether a line of the server log since this server started has all of `parts`.
    fn logged(&self, parts: &[&str]) -> bool {
        lock(&LOG)
            .since(self.log_from)
            .any(|line| parts.iter().all(|part| line.contains(part)))
    }

    /// Fails unless the server ended the RakNet connection of the bot run that wrote `lines`
    /// because the bot disconnected, not at the idle timeout.
    fn assert_raknet_closed_by_bot(&self, lines: &str) {
        let result = result(lines);
        let port = result["local_addr"]
            .as_str()
            .and_then(|addr| addr.rsplit(':').next())
            .unwrap_or_else(|| panic!("no local address in {result}"));
        let addr = format!("addr=127.0.0.1:{port} ");
        assert!(
            self.logged(&[
                "raknet connection closed",
                &addr,
                "reason=peer disconnected"
            ]),
            "the bot's disconnect did not end its connection ({addr})"
        );
    }

    /// Fails unless the server ended the NetherNet peer of the bot run that wrote `lines` because
    /// the bot closed it, not at the idle timeout. The peer is the one of the answer's ICE ufrag.
    fn assert_nethernet_closed_by_bot(&self, lines: &str) {
        let ufrag = lines
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|v| v["type"] == "sdp" && v["role"] == "answer")
            .find_map(|v| {
                v["text"].as_str().and_then(|sdp| {
                    sdp.lines()
                        .find_map(|l| l.strip_prefix("a=ice-ufrag:"))
                        .map(|u| u.trim().to_owned())
                })
            })
            .expect("the capture has the answer's ICE ufrag");
        let ufrag = format!("ufrag={ufrag} ");
        assert!(
            self.logged(&["nethernet peer closed", &ufrag, "closed by the remote"]),
            "the bot's close did not end its peer ({ufrag})"
        );
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
    #[expect(
        clippy::print_stderr,
        reason = "consistency: the test harness shows a failed test's standard error"
    )]
    fn drop(&mut self) {
        // A failed assertion still stops the server so the test process can end, and shows the
        // server log with the failure.
        self.shutdown.shutdown();
        if std::thread::panicking() {
            let log = lock(&LOG);
            let lines: Vec<&String> = log.since(self.log_from).collect();
            let first = lines.len().saturating_sub(SHOWN_LINES);
            let mut text = format!(
                "server log since this server started, the last {} of {} lines (the servers of \
                 tests running at the same time log here too; run one test to see only its own):",
                lines.len() - first,
                lines.len()
            );
            for line in lines.iter().skip(first) {
                text.push_str("\n  ");
                text.push_str(line);
            }
            eprintln!("{text}");
        }
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
        .args([
            "-addr",
            &server.addr(),
            "-signaling-addr",
            &server.signaling_addr(),
        ])
        .args(["-scenario", scenario, "-out"])
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

/// The jigsaw structure data and biome definitions Anchor sends equal `CloudburstMC`'s data, as
/// BDS's do, checked on the wire by `bot cloudburst` when the pinned checkout is in
/// `.cache` (`cargo xtask dump` needs it too).
#[test]
fn the_jigsaw_and_biome_data_sent_equal_the_cloudburst_data() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../.cache/cloudburst-data");
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: test tooling outside the server"
    )]
    let present = data.join("jigsaw_structure_data.nbt").exists();
    if !present {
        return;
    }
    let name = "cloudburst";
    let server = Running::start(name, config(Compression::Snappy), &data_dir(name));
    play(&server, "spawn", name, &[]);
    server.stop();
    let output = Command::new(bot())
        .arg("cloudburst")
        .arg("-world")
        .arg(work_dir().join(format!("{name}.world.jsonl")))
        .arg("-data")
        .arg(&data)
        .output()
        .expect("the bot starts");
    assert!(
        output.status.success(),
        "bot cloudburst: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn sessions_end_cleanly_and_the_pong_counts_no_one_after_they_leave() {
    let mut server = Running::start("cycles", config(Compression::Snappy), &data_dir("cycles"));
    let online = |server: &mut Running| server.pong().split(';').nth(4).map(str::to_owned);
    assert_eq!(online(&mut server).as_deref(), Some("0"));
    let runs: Vec<String> = (0..5)
        .map(|cycle| {
            play(
                &server,
                "spawn",
                &format!("cycle-{cycle}"),
                &["-stay", "0s"],
            )
        })
        .collect();
    // Sessions report their end to the accept task a moment after the bot closes.
    let counts: Vec<_> = (0..20).map(|_| online(&mut server)).collect();
    assert!(
        counts.iter().any(|n| n.as_deref() == Some("0")),
        "players still counted: {counts:?}"
    );
    for lines in &runs {
        server.assert_raknet_closed_by_bot(lines);
    }
    server.stop();
}

/// `enable-encryption`: over RakNet the bot gets the handshake and encrypts with it on, and
/// spawns without either with it off.
#[test]
fn the_bot_spawns_over_raknet_with_and_without_encryption() {
    for (encryption, name) in [(true, "encrypted"), (false, "plain")] {
        let mut config = config(Compression::Snappy);
        config.developer.enable_encryption = encryption;
        let server = Running::start(name, config, &data_dir(name));
        let outcome = result(&play(&server, "spawn", name, &[]));
        assert_eq!(outcome["spawned"], true, "{name}: {outcome}");
        assert_eq!(outcome["handshake"], encryption, "{name}: {outcome}");
        assert_eq!(outcome["encrypted"], encryption, "{name}: {outcome}");
        server.stop();
    }
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
        let (status, body) = post_offer(server.signaling_port);
        assert_eq!(status, 200, "offer {n}: {body}");
        assert!(body.starts_with("v=0"), "offer {n}: {body}");
    }
    assert_eq!(post_offer(server.signaling_port).0, 503);
    server.stop();
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test hears the quits of the server's tick and times its status poll"
)]
fn nethernet_sessions_end_cleanly_and_the_join_status_counts_no_one_after_they_leave() {
    const CYCLES: usize = 5;
    let (quit, quits) = std::sync::mpsc::sync_channel(CYCLES);
    let server = Running::start_with(
        "nn-cycles",
        config(Compression::Snappy),
        &data_dir("nn-cycles"),
        |builder| {
            let quit = quit.clone();
            builder.on(Priority::Normal, move |_: &mut PlayerQuit, _| {
                quit.try_send(()).expect("one quit per cycle");
            })
        },
    );
    let players = |server: &Running| -> Value {
        let (status, body) = http(
            server.signaling_port,
            "GET /v1/join HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200, "{body}");
        serde_json::from_str::<Value>(&body).expect("JSON status")["players"].clone()
    };
    assert_eq!(players(&server), 0);
    let runs: Vec<String> = (0..CYCLES)
        .map(|cycle| {
            play(
                &server,
                "spawn",
                &format!("nn-cycle-{cycle}"),
                &[NETHERNET, &["-stay", "0s"]].concat(),
            )
        })
        .collect();
    // A session queues its leave for the tick, then reports its end to the accept task; the
    // tick's quit means the session has ended. The limit only keeps a broken server from
    // hanging the test: a session the bot's close does not end ends at the idle timeout, which
    // the server log tells apart.
    for cycle in 0..CYCLES {
        quits
            .recv_timeout(Duration::from_secs(60))
            .unwrap_or_else(|e| panic!("quit {cycle} of {CYCLES}: {e}"));
    }
    for lines in &runs {
        server.assert_nethernet_closed_by_bot(lines);
    }
    // What is left is the accept task taking the end report the session sent after its leave;
    // no event follows it, so the status is polled. The limit only catches a broken server.
    let end = std::time::Instant::now() + Duration::from_secs(60);
    let mut counted = players(&server);
    while counted != 0 && std::time::Instant::now() < end {
        pause();
        counted = players(&server);
    }
    assert_eq!(counted, 0, "players still counted");
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
        first["disconnect_message"], "disconnectionScreen.loggedinOtherLocation",
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
        "disconnectionScreen.loggedinOtherLocation"
    );
    // The first player stays connected for its whole stay without a disconnect.
    finish(first, "spawn", "dupc-first");
    server.stop();
}

#[test]
fn sessions_past_login_but_not_joined_are_capped() {
    let mut config = config(Compression::Snappy);
    config.general.max_players = 2;
    let server = Running::start("waiting-cap", config, &data_dir("waiting-cap"));
    let stalled: Vec<_> = (0..2)
        .map(|n| {
            let name = format!("waiting-{n}");
            let bot = start_bot(&server, "stall", &name, &["-name", &format!("Waiting{n}")]);
            wait_for_packs_info(&name);
            (bot, name)
        })
        .collect();
    // Both places are taken by players still in the resource pack stage.
    let refused = play(&server, "stall", "waiting-refused", &["-name", "Refused"]);
    let refused = result(&refused);
    assert_eq!(refused["play_status"], 7, "{refused}");
    assert_eq!(refused["resource_packs_info_index"], -1, "{refused}");
    server.stop();
    for (bot, name) in stalled {
        let lines = finish(bot, "stall", &name);
        assert_eq!(
            result(&lines)["disconnect_message"],
            "The server has stopped",
            "{name}"
        );
    }
}

#[test]
fn a_player_downloading_packs_gets_the_shutdown_message() {
    let mut config = config(Compression::Snappy);
    "Back soon".clone_into(&mut config.general.shutdown_message);
    let server = Running::start("packs-stop", config, &data_dir("packs-stop"));
    let bot = start_bot(&server, "stall", "packs-stop", &[]);
    wait_for_packs_info("packs-stop");
    server.stop();
    let lines = finish(bot, "stall", "packs-stop");
    let result = result(&lines);
    assert_eq!(result["disconnect_message"], "Back soon", "{result}");
}

/// Waits until the bot writing `<name>.jsonl` has sent `SetLocalPlayerAsInitialised` (the bot
/// flushes its output right after).
fn wait_for_spawn(name: &str) {
    wait_for(name, "\"name\":\"SetLocalPlayerAsInitialised\"");
}

/// Waits until the stalling bot writing `<name>.jsonl` has received `ResourcePacksInfo` (it
/// flushes its output then).
fn wait_for_packs_info(name: &str) {
    wait_for(name, "\"name\":\"ResourcePacksInfo\"");
}

fn wait_for(name: &str, text: &str) {
    for _ in 0..300 {
        if read(&out_file(name)).contains(text) {
            return;
        }
        pause();
    }
    panic!("{name} never wrote {text}");
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
    clippy::disallowed_methods,
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
