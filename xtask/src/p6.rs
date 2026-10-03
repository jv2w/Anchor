//! `cargo xtask p6`: the reference server comparison (PLAN.md 4 P6, 10.2). It builds the Go bot
//! (`xtask/go/bot`), runs its scenarios against BDS or any running server, and compares captures.
//! It needs Go and, for `bds`, the BDS copy in `.cache/bds/`, so `verify` does not run it.
//!
//! - `p6 bds [zlib|snappy]...`: for each compression (default both), writes the test settings into
//!   the copy's `server.properties`, deletes its `worlds` folder, starts `bedrock_server.exe` in the
//!   copy, runs the scenarios into `target/p6/bds-<compression>/` and stops BDS with `stop`.
//! - `p6 anchor [zlib|snappy]...`: starts Anchor in this process with the same settings, runs the
//!   scenarios into `target/p6/anchor-<compression>/` and compares them with the BDS captures.
//! - `p6 run --addr HOST:PORT --out DIR [--token-key PEM --kid KID --iss ISS]`: runs the scenarios
//!   against a running server; the token flags add the online scenario.
//! - `p6 compare A B`: compares the spawn windows of two captures (see `compare`).

mod compare;

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

/// The BDS copy, relative to the repository root. The original download is never run.
const BDS_DIR: &str = ".cache/bds/1.26.52.3";
/// Captures and logs, relative to the repository root.
const OUT_DIR: &str = "target/p6";
/// Scenarios every server must pass.
const SCENARIOS: &[&str] = &["spawn", "big-skin"];
/// Test settings written into the copy's `server.properties`. The world and gameplay values are
/// Anchor's defaults (PLAN.md 8.5), so config-dependent fields compare equal. LAN visibility is on
/// because BDS answers RakNet pings with empty data without it; it also binds UDP 19132 and 19133.
const BDS_SETTINGS: &[(&str, &str)] = &[
    ("online-mode", "false"),
    ("allow-list", "false"),
    ("transport", "raknet"),
    ("enable-lan-visibility", "true"),
    ("level-name", "p6"),
    ("level-type", "FLAT"),
    ("level-seed", "1"),
    ("gamemode", "survival"),
    ("difficulty", "normal"),
    ("max-players", "20"),
    ("view-distance", "16"),
    ("compression-threshold", "256"),
];
const START_TIMEOUT: Duration = Duration::from_mins(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(200);
/// The command forms, for the usage text.
pub(crate) const USAGE: &str = "cargo xtask p6 bds [zlib|snappy]... | cargo xtask p6 anchor \
                                [zlib|snappy]... | cargo xtask p6 run \
                                --addr HOST:PORT --out DIR [--token-key PEM --kid KID --iss ISS] \
                                | cargo xtask p6 compare A B";

/// Runs `cargo xtask p6 <args>`.
pub(crate) fn command(root: &Path, args: &[&str], out: &mut impl Write) -> Result<bool, String> {
    match args {
        ["bds", rest @ ..] => bds(root, rest, out),
        ["anchor", rest @ ..] => anchor(root, rest, out),
        ["run", rest @ ..] => run(root, rest, out),
        ["compare", a, b] => compare_files(root, Path::new(a), Path::new(b), out),
        _ => Err(format!("usage: {USAGE}")),
    }
}

fn say(out: &mut impl Write, text: &str) -> Result<(), String> {
    writeln!(out, "{text}").map_err(|e| e.to_string())
}

/// `p6 bds`: runs the scenarios against BDS once per compression.
fn bds(root: &Path, args: &[&str], out: &mut impl Write) -> Result<bool, String> {
    let compressions = compressions(args)?;
    let dir = root.join(BDS_DIR);
    let exe = dir.join("bedrock_server.exe");
    if !is_file(&exe) {
        return Err(format!(
            "{} is missing: copy BDS 1.26.52.3 into {BDS_DIR} (never run the original)",
            exe.display()
        ));
    }
    let bot = build_bot(root)?;
    let deferred = load_deferred(root)?;
    let mut passed = true;
    for compression in &compressions {
        let out_dir = root.join(OUT_DIR).join(format!("bds-{compression}"));
        create_dir(&out_dir)?;
        let (port, port_v6) = free_ports()?;
        let mut settings = BDS_SETTINGS.to_vec();
        let (port_text, port_v6_text) = (port.to_string(), port_v6.to_string());
        settings.extend([
            ("server-port", port_text.as_str()),
            ("server-portv6", port_v6_text.as_str()),
            ("compression-algorithm", compression),
        ]);
        set_properties(&dir.join("server.properties"), &settings)?;
        remove_dir(&dir.join("worlds"))?;
        say(
            out,
            &format!("bds-{compression}: starting BDS on UDP {port}"),
        )?;
        let mut server = Bds::start(&exe, &dir, &out_dir.join("bds.log"))?;
        let result = server.wait_started().and_then(|()| {
            let addr = format!("127.0.0.1:{port}");
            run_scenarios(&bot, &addr, &out_dir, None, out)
        });
        let stopped = server.stop();
        passed &= result?;
        stopped?;
        report_window(&out_dir.join("spawn.jsonl"), &deferred, out)?;
    }
    say(out, if passed { "PASS" } else { "FAIL" })?;
    Ok(passed)
}

/// `p6 anchor`: runs the scenarios against Anchor, started in this process with the settings
/// `p6 bds` gives BDS, and compares each capture with the BDS capture of the same compression.
fn anchor(root: &Path, args: &[&str], out: &mut impl Write) -> Result<bool, String> {
    let compressions = compressions(args)?;
    let bot = build_bot(root)?;
    let deferred = load_deferred(root)?;
    let mut passed = true;
    for compression in compressions {
        let out_dir = root.join(OUT_DIR).join(format!("anchor-{compression}"));
        let data_dir = out_dir.join("data");
        remove_dir(&data_dir)?;
        create_dir(&data_dir)?;
        let (port, _) = free_ports()?;
        let mut config = anchor::Config::default();
        config.general.xbox_auth = false;
        config.general.enable_ipv6 = false;
        config.general.server_port = port;
        "p6".clone_into(&mut config.general.level_name);
        config.general.level_seed = Some(1);
        config.developer.compression = match compression {
            "zlib" => anchor::protocol::compression::Compression::Zlib,
            _ => anchor::protocol::compression::Compression::Snappy,
        };
        say(
            out,
            &format!("anchor-{compression}: starting Anchor on UDP {port}"),
        )?;
        let server = anchor::Server::builder()
            .config(config)
            .data_dir(&data_dir)
            .build()
            .map_err(|e| format!("build Anchor: {e}"))?;
        let shutdown = server.shutdown_handle();
        let thread = spawn(move || server.run());
        let addr = format!("127.0.0.1:{port}");
        let result =
            wait_for_pong(port).and_then(|()| run_scenarios(&bot, &addr, &out_dir, None, out));
        shutdown.shutdown();
        let stopped = thread
            .join()
            .map_err(|_| "the Anchor thread panicked".to_owned())
            .and_then(|run| run.map_err(|e| format!("Anchor stopped with an error: {e}")));
        passed &= result?;
        stopped?;
        report_window(&out_dir.join("spawn.jsonl"), &deferred, out)?;
        let bds = root
            .join(OUT_DIR)
            .join(format!("bds-{compression}"))
            .join("spawn.jsonl");
        if is_file(&bds) {
            say(
                out,
                &format!("compare bds-{compression} anchor-{compression}:"),
            )?;
            passed &= compare_files(root, &bds, &out_dir.join("spawn.jsonl"), out)?;
        } else {
            say(
                out,
                &format!("no BDS capture at {}; run `p6 bds` first", bds.display()),
            )?;
            passed = false;
        }
    }
    say(out, if passed { "PASS" } else { "FAIL" })?;
    Ok(passed)
}

/// The compressions named on the command line, or both.
fn compressions<'a>(args: &[&'a str]) -> Result<Vec<&'a str>, String> {
    if let Some(bad) = args.iter().find(|c| !matches!(**c, "zlib" | "snappy")) {
        return Err(format!("unknown compression `{bad}` (want zlib or snappy)"));
    }
    Ok(if args.is_empty() {
        vec!["zlib", "snappy"]
    } else {
        args.to_vec()
    })
}

/// Pings the server until it answers with a RakNet pong, so the bot does not race its start.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: xtask is a development tool"
)]
fn wait_for_pong(port: u16) -> Result<(), String> {
    const MAGIC: [u8; 16] = [
        0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56,
        0x78,
    ];
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    socket
        .set_read_timeout(Some(POLL))
        .map_err(|e| e.to_string())?;
    let mut ping = vec![0x01];
    ping.extend_from_slice(&0_i64.to_be_bytes());
    ping.extend_from_slice(&MAGIC);
    ping.extend_from_slice(&1_i64.to_be_bytes());
    let deadline = now() + START_TIMEOUT;
    let mut buf = [0; 1500];
    while now() < deadline {
        socket
            .send_to(&ping, ("127.0.0.1", port))
            .map_err(|e| e.to_string())?;
        if let Ok((n, _)) = socket.recv_from(&mut buf)
            && n > 0
            && buf.first() == Some(&0x1c)
        {
            return Ok(());
        }
        // An ICMP "port unreachable" makes `recv_from` fail at once on Windows.
        pause();
    }
    Err(format!(
        "Anchor did not answer pings within {START_TIMEOUT:?}"
    ))
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn spawn<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> std::thread::JoinHandle<T> {
    std::thread::spawn(f)
}

/// `p6 run`: runs the scenarios against a running server.
fn run(root: &Path, args: &[&str], out: &mut impl Write) -> Result<bool, String> {
    let mut flags = std::collections::BTreeMap::new();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let key = flag
            .strip_prefix("--")
            .filter(|k| matches!(*k, "addr" | "out" | "token-key" | "kid" | "iss"))
            .ok_or_else(|| format!("unknown argument `{flag}`; usage: {USAGE}"))?;
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        if flags.insert(key, *value).is_some() {
            return Err(format!("{flag} given twice"));
        }
    }
    let (Some(addr), Some(out_dir)) = (flags.get("addr"), flags.get("out")) else {
        return Err(format!("--addr and --out are required; usage: {USAGE}"));
    };
    let online = match (flags.get("token-key"), flags.get("kid"), flags.get("iss")) {
        (Some(key), Some(kid), Some(iss)) => Some([*key, *kid, *iss]),
        (None, None, None) => None,
        _ => return Err("--token-key, --kid and --iss go together".to_owned()),
    };
    addr.parse::<SocketAddr>()
        .map_err(|e| format!("--addr {addr}: {e}"))?;
    let out_dir = PathBuf::from(out_dir);
    create_dir(&out_dir)?;
    let bot = build_bot(root)?;
    let passed = run_scenarios(&bot, addr, &out_dir, online, out)?;
    report_window(&out_dir.join("spawn.jsonl"), &load_deferred(root)?, out)?;
    say(out, if passed { "PASS" } else { "FAIL" })?;
    Ok(passed)
}

/// `p6 compare`: compares the spawn windows of two captures.
fn compare_files(root: &Path, a: &Path, b: &Path, out: &mut impl Write) -> Result<bool, String> {
    let deferred = load_deferred(root)?;
    let fields = compare::parse_fields(&read_text(&root.join(compare::FIELDS_PATH))?, &deferred)?;
    let window = |path: &Path| {
        compare::window(&read_text(path)?).map_err(|e| format!("{}: {e}", path.display()))
    };
    let differences = compare::compare(&window(a)?, &window(b)?, &deferred, &fields);
    for difference in &differences {
        say(out, difference)?;
    }
    let same = differences.is_empty();
    say(out, if same { "SAME" } else { "DIFFERENT" })?;
    Ok(same)
}

fn load_deferred(root: &Path) -> Result<compare::Deferred, String> {
    compare::parse_deferred(&read_text(&root.join(compare::DEFERRED_PATH))?)
}

/// Prints the spawn window of a capture without deferred packets.
fn report_window(
    capture: &Path,
    deferred: &compare::Deferred,
    out: &mut impl Write,
) -> Result<(), String> {
    let Ok(window) = read_text(capture).and_then(|text| compare::window(&text)) else {
        return Ok(());
    };
    say(
        out,
        &format!(
            "spawn window without deferred packets: {}",
            compare::sequence(&window, deferred).join(", ")
        ),
    )
}

/// Builds the bot into `target/p6/` and returns its path.
fn build_bot(root: &Path) -> Result<PathBuf, String> {
    let dir = root.join(OUT_DIR);
    create_dir(&dir)?;
    let exe = dir.join(format!("bot{}", std::env::consts::EXE_SUFFIX));
    let output = Command::new("go")
        .arg("build")
        .arg("-o")
        .arg(&exe)
        .arg("./bot")
        .current_dir(root.join("xtask").join("go"))
        .output()
        .map_err(|e| format!("cannot run `go` ({e}); install Go and put it on PATH"))?;
    if !output.status.success() {
        return Err(format!(
            "go build ./bot failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(exe)
}

/// Runs each scenario into `<out_dir>/<scenario>.jsonl`; returns whether all passed.
fn run_scenarios(
    bot: &Path,
    addr: &str,
    out_dir: &Path,
    online: Option<[&str; 3]>,
    out: &mut impl Write,
) -> Result<bool, String> {
    let mut passed = true;
    let online_scenario = online.map(|_| "online");
    for scenario in SCENARIOS.iter().copied().chain(online_scenario) {
        let capture = out_dir.join(format!("{scenario}.jsonl"));
        let mut command = Command::new(bot);
        command
            .args([
                "-addr",
                addr,
                "-scenario",
                scenario,
                "-timeout",
                "60s",
                "-out",
            ])
            .arg(&capture);
        if let (Some([key, kid, iss]), "online") = (online, scenario) {
            command.args(["-token-key", key, "-kid", kid, "-iss", iss]);
        }
        let output = command
            .output()
            .map_err(|e| format!("cannot run the bot: {e}"))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        match output.status.code() {
            Some(0) => say(out, &format!("ok   {scenario}"))?,
            Some(1) => {
                passed = false;
                say(out, &format!("FAIL {scenario}: {}", stderr.trim()))?;
            }
            _ => return Err(format!("the bot failed ({}): {stderr}", output.status)),
        }
    }
    Ok(passed)
}

/// A running BDS process. Its output goes to a log file, which is polled for the start line.
#[derive(Debug)]
struct Bds {
    child: Child,
    stdin: Option<ChildStdin>,
    log: PathBuf,
}

impl Bds {
    fn start(exe: &Path, dir: &Path, log: &Path) -> Result<Self, String> {
        let file = create_file(log)?;
        let stderr = file.try_clone().map_err(|e| e.to_string())?;
        let mut child = Command::new(exe)
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(file)
            .stderr(stderr)
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
        let stdin = child.stdin.take();
        Ok(Self {
            child,
            stdin,
            log: log.to_path_buf(),
        })
    }

    /// Waits for BDS's "Server started." line.
    fn wait_started(&mut self) -> Result<(), String> {
        let deadline = now() + START_TIMEOUT;
        loop {
            if read_text(&self.log)?.contains("Server started.") {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait().map_err(|e| e.to_string())? {
                return Err(format!(
                    "BDS exited ({status}) before starting; see {}",
                    self.log.display()
                ));
            }
            if now() > deadline {
                return Err(format!(
                    "BDS did not start within {START_TIMEOUT:?}; see {}",
                    self.log.display()
                ));
            }
            pause();
        }
    }

    /// Sends `stop` and waits for the exit; kills BDS if it does not stop in time.
    fn stop(mut self) -> Result<(), String> {
        let sent = self
            .stdin
            .take()
            .map(|mut stdin| stdin.write_all(b"stop\n").and_then(|()| stdin.flush()));
        let deadline = now() + STOP_TIMEOUT;
        while matches!(sent, Some(Ok(()))) && now() < deadline {
            if self.child.try_wait().map_err(|e| e.to_string())?.is_some() {
                return Ok(());
            }
            pause();
        }
        self.child.kill().map_err(|e| format!("kill BDS: {e}"))?;
        self.child
            .wait()
            .map_err(|e| format!("wait for BDS: {e}"))?;
        Err(format!(
            "BDS did not stop on `stop` within {STOP_TIMEOUT:?}; killed it"
        ))
    }
}

/// Sets `key=value` lines in a properties file, replacing existing keys and appending new ones.
fn set_properties(path: &Path, settings: &[(&str, &str)]) -> Result<(), String> {
    let text = read_text(path)?;
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    for (key, value) in settings {
        let prefix = format!("{key}=");
        let line = format!("{key}={value}");
        match lines.iter_mut().find(|l| l.starts_with(&prefix)) {
            Some(existing) => *existing = line,
            None => lines.push(line),
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    write(path, out.as_bytes())
}

/// Two distinct UDP ports that are free on all IPv4 interfaces right now.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: xtask is a development tool"
)]
fn free_ports() -> Result<(u16, u16), String> {
    let bind = || {
        std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| Ok((s.local_addr()?.port(), s)))
            .map_err(|e| format!("find a free UDP port: {e}"))
    };
    // Both sockets stay bound until both ports are known, so the ports differ.
    let (a, _first) = bind()?;
    let (b, _second) = bind()?;
    Ok((a, b))
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn now() -> Instant {
    Instant::now()
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn pause() {
    std::thread::sleep(POLL);
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn is_file(path: &Path) -> bool {
    path.is_file()
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn read_text(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
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
fn create_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Removes a directory tree; a missing directory is fine.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn remove_dir(path: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("{}: {e}", path.display()))
        }
        _ => Ok(()),
    }
}

#[expect(
    clippy::disallowed_types,
    reason = "off-tick: xtask is a development tool"
)]
fn create_file(path: &Path) -> Result<std::fs::File, String> {
    std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))
}
