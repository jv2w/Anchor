//! `cargo xtask p6`: the reference server comparison (PLAN.md 4 P6, 10.2, 10.3). It builds the Go
//! bot (`xtask/go/bot`), runs its scenarios against BDS or any running server, and compares
//! captures. It needs Go and, for `bds`, the BDS copy in `.cache/bds/`, so `verify` does not run it.
//!
//! - `p6 bds [zlib|snappy]...`: for each compression (default both), writes the test settings into
//!   the copy's `server.properties`, deletes its `worlds` folder, starts `bedrock_server.exe` in the
//!   copy and stops it with `stop` after each run. With `transport=raknet` it runs the scenarios
//!   into `target/p6/bds-<compression>/`; with `transport=nethernet` (signaling on `server-port`)
//!   into `target/p6/bds-nethernet-<compression>/`, plus big-skin with the bot sending 65,535-byte
//!   segments (`big-skin-65535.jsonl`, recorded). NetherNet runs print the segment facts of each
//!   capture (what the server sent per data channel message). Once, it also runs the `auto` bot
//!   against the RakNet BDS (`bds-fallback/`, it must fall back to RakNet and spawn) and records, against the
//!   NetherNet BDS, what a mismatched offer identity (`bds-bind-mismatch/`) and a second login
//!   with the same name (`bds-duplicate/`) do. The duplicate login is a check only if BDS kicks
//!   the first player as PMMP does. Each RakNet run also runs the `blocks` scenario
//!   (`bds-<compression>/blocks.jsonl`): while the bot stays spawned, BDS console commands place
//!   fences, panes, bars, stairs and tripwire next to each other near the player, and the block
//!   updates BDS sent for each step are reported with their states (`blocks.txt`).
//! - `p6 anchor [zlib|snappy]...`: starts Anchor in this process with the same settings and runs
//!   the same RakNet and NetherNet scenarios into `target/p6/anchor-<compression>/` and
//!   `anchor-nethernet-<compression>/`, and the `auto` bot against an unreachable NetherNet
//!   address into `anchor-fallback/`; each capture is compared with its BDS capture.
//! - `p6 run --addr HOST:PORT --out DIR [--transport T] [--token-key PEM --kid KID --iss ISS]`:
//!   runs the scenarios against a running server; the token flags add the online scenario.
//! - `p6 compare A B`: compares the spawn windows of two captures, their answer SDP structures
//!   when they have them, and their full item packets (`<capture>.items.jsonl`) when both have
//!   them (see `compare`).

mod compare;

use std::io::Write;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use std::collections::BTreeMap;

use compare::field;
use serde_json::Value;

/// The BDS copy, relative to the repository root. The original download is never run.
const BDS_DIR: &str = ".cache/bds/1.26.52.3";
/// Captures and logs, relative to the repository root.
const OUT_DIR: &str = "target/p6";
/// Scenarios every server must pass, over each transport.
const SCENARIOS: &[&str] = &["spawn", "big-skin"];
/// Test settings written into the copy's `server.properties`, besides the transport, ports and
/// compression of each run. The world and gameplay values are Anchor's defaults (PLAN.md 8.5), so
/// config-dependent fields compare equal. LAN visibility is on because BDS answers RakNet pings
/// with empty data without it; it also binds UDP 19132 and 19133.
const BDS_SETTINGS: &[(&str, &str)] = &[
    ("online-mode", "false"),
    ("allow-list", "false"),
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
/// The block states of the reference list (PLAN.md 7), relative to the repository root.
const REFERENCE_BLOCKS: &str = "xtask/data/reference/blocks.json";
/// The `y` of the blocks scenario: the first air layer of BDS's default flat world (bedrock at
/// -64, two dirt layers, grass at -61). Every step uses `keep`, so a wrong height places nothing
/// and fails the run.
const BLOCKS_Y: i32 = -60;
/// How far the blocks scenario's base position is from the player's spawn column, on `x` and `z`.
const BLOCKS_OFFSET: i32 = 2;
/// The steps of the blocks scenario: one block placed at (`dx`, `dz`) from the base position. The
/// groups are 3 blocks apart on `z`, so they do not touch each other.
const BLOCK_STEPS: &[(i32, i32, &str)] = &[
    // Two fences side by side.
    (0, 0, "oak_fence"),
    (1, 0, "oak_fence"),
    // A fence placed next to a solid block, and a solid block placed next to a fence.
    (0, 3, "stone"),
    (1, 3, "oak_fence"),
    (4, 3, "oak_fence"),
    (5, 3, "stone"),
    // Glass panes in an L.
    (0, 6, "glass_pane"),
    (1, 6, "glass_pane"),
    (0, 7, "glass_pane"),
    // Iron bars in a line, then copper bars at its end.
    (0, 10, "iron_bars"),
    (1, 10, "iron_bars"),
    (2, 10, "iron_bars"),
    (3, 10, "copper_bars"),
    // Stairs facing east (weirdo_direction 0), then one facing south (2) on its east side and on
    // its west side: one pair makes an outer corner and the other an inner corner.
    (0, 13, r#"oak_stairs ["weirdo_direction"=0]"#),
    (1, 13, r#"oak_stairs ["weirdo_direction"=2]"#),
    (1, 16, r#"oak_stairs ["weirdo_direction"=0]"#),
    (0, 16, r#"oak_stairs ["weirdo_direction"=2]"#),
    // A lone stair.
    (0, 19, r#"oak_stairs ["weirdo_direction"=0]"#),
    // Tripwire hooks on stone, facing each other, then the tripwire between them; then two
    // tripwires without hooks.
    (0, 22, "stone"),
    (4, 22, "stone"),
    (1, 22, r#"tripwire_hook ["direction"=3]"#),
    (3, 22, r#"tripwire_hook ["direction"=1]"#),
    (2, 22, "trip_wire"),
    (6, 22, "trip_wire"),
    (7, 22, "trip_wire"),
];
/// The `say` text that closes each step of the blocks scenario; the step number follows it. The
/// bot ends the scenario on `p6-end` (`xtask/go/bot`).
const STEP_MARKER: &str = "p6-step ";
/// How long the blocks bot may stay spawned; it leaves on the `p6-end` message.
const BLOCKS_STAY: &str = "90s";
/// The address Anchor advertises in the fallback run: an IPv6 documentation address (RFC 3849)
/// while Anchor has no IPv6 socket, so the NetherNet negotiation fails with code 11. An
/// unreachable IPv4 address does not fail: Anchor's own ICE checks give the client a
/// peer-reflexive candidate.
const UNREACHABLE: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
/// The player name of both bots of the duplicate-login run.
const TWIN: &str = "P6Twin";
/// How long the first bot of the duplicate-login run waits for its Disconnect.
const TWIN_STAY: &str = "20s";
/// How long a NetherNet bot waits after the data channels open before its first packet, so the
/// capture shows whether the server sends anything unprompted.
const FIRST_SEND_DELAY: &str = "1s";
/// A segment payload smaller than go-nethernet's 262,143 (the bot's default): what a sender
/// limited to 64 KiB SCTP messages uses. `p6 bds` sends big-skin in it to record whether BDS
/// accepts it; Anchor accepts both (PLAN.md 10.3).
const SMALL_SEGMENT: &str = "65535";
const START_TIMEOUT: Duration = Duration::from_mins(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(200);
/// The command forms, for the usage text.
pub(crate) const USAGE: &str = "cargo xtask p6 bds [zlib|snappy]... | cargo xtask p6 anchor \
                                [zlib|snappy]... | cargo xtask p6 run \
                                --addr HOST:PORT --out DIR [--transport raknet|nethernet|auto] \
                                [--token-key PEM --kid KID --iss ISS] | cargo xtask p6 compare A B";

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

/// `p6 bds`: runs the scenarios against BDS once per compression and transport.
fn bds(root: &Path, args: &[&str], out: &mut impl Write) -> Result<bool, String> {
    let compressions = compressions(args)?;
    let exe = root.join(BDS_DIR).join("bedrock_server.exe");
    if !is_file(&exe) {
        return Err(format!(
            "{} is missing: copy BDS 1.26.52.3 into {BDS_DIR} (never run the original)",
            exe.display()
        ));
    }
    let bot = build_bot(root)?;
    let deferred = load_deferred(root)?;
    let states = block_states(&read_text(&root.join(REFERENCE_BLOCKS))?)?;
    let dir = |name: &str| root.join(OUT_DIR).join(name);
    let mut passed = true;
    for (i, compression) in compressions.iter().enumerate() {
        let once = i == 0;
        let raknet = dir(&format!("bds-{compression}"));
        passed &= bds_session(
            &exe,
            &raknet,
            "raknet",
            compression,
            out,
            |addr, server, out| {
                let mut ok = run_scenarios(&bot, addr, &raknet, "raknet", None, out)?;
                report_window(&raknet.join("spawn.jsonl"), &deferred, out)?;
                ok &= blocks(&bot, addr, server, &raknet, &states, out)?;
                if once {
                    wait_until_empty(addr)?;
                    ok &= fallback(&bot, addr, &dir("bds-fallback"), out)?;
                }
                Ok(ok)
            },
        )?;
        let nethernet = dir(&format!("bds-nethernet-{compression}"));
        passed &= bds_session(
            &exe,
            &nethernet,
            "nethernet",
            compression,
            out,
            |addr, _, out| {
                let mut ok = run_scenarios(&bot, addr, &nethernet, "nethernet", None, out)?;
                report_window(&nethernet.join("spawn.jsonl"), &deferred, out)?;
                report_nethernet(&nethernet.join("spawn.jsonl"), out)?;
                record_segment_size(&bot, addr, &nethernet, SMALL_SEGMENT, out)?;
                if once {
                    record_bind_mismatch(&bot, addr, &dir("bds-bind-mismatch"), out)?;
                    ok &= duplicate_login(&bot, addr, &dir("bds-duplicate"), out)?;
                }
                Ok(ok)
            },
        )?;
    }
    say(out, if passed { "PASS" } else { "FAIL" })?;
    Ok(passed)
}

/// Starts BDS with the test settings, the transport and the compression, calls `run` with its
/// address and the process (for console commands) and stops it. The BDS log goes to
/// `<out_dir>/bds.log`.
fn bds_session<W: Write>(
    exe: &Path,
    out_dir: &Path,
    transport: &str,
    compression: &str,
    out: &mut W,
    run: impl FnOnce(&str, &mut Bds, &mut W) -> Result<bool, String>,
) -> Result<bool, String> {
    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no parent", exe.display()))?;
    create_dir(out_dir)?;
    let (port, port_v6) = free_ports()?;
    let (port_text, port_v6_text) = (port.to_string(), port_v6.to_string());
    let mut settings = BDS_SETTINGS.to_vec();
    settings.extend([
        ("transport", transport),
        ("server-port", port_text.as_str()),
        ("server-portv6", port_v6_text.as_str()),
        ("compression-algorithm", compression),
    ]);
    set_properties(&dir.join("server.properties"), &settings)?;
    remove_dir(&dir.join("worlds"))?;
    say(
        out,
        &format!(
            "{}: starting BDS ({transport}, {compression}) on port {port}",
            name(out_dir)
        ),
    )?;
    let mut server = Bds::start(exe, dir, &out_dir.join("bds.log"))?;
    let result = server
        .wait_started()
        .and_then(|()| run(&format!("127.0.0.1:{port}"), &mut server, out));
    let stopped = server.stop();
    let passed = result?;
    stopped?;
    Ok(passed)
}

/// `p6 anchor`: runs the scenarios against Anchor, started in this process with the settings
/// `p6 bds` gives BDS, and compares each capture with the BDS capture of the same run.
fn anchor(root: &Path, args: &[&str], out: &mut impl Write) -> Result<bool, String> {
    let compressions = compressions(args)?;
    let bot = build_bot(root)?;
    let deferred = load_deferred(root)?;
    let dir = |name: &str| root.join(OUT_DIR).join(name);
    let mut passed = true;
    for compression in &compressions {
        for (transport, name) in [
            ("raknet", (*compression).to_owned()),
            ("nethernet", format!("nethernet-{compression}")),
        ] {
            let out_dir = dir(&format!("anchor-{name}"));
            passed &= anchor_session(&out_dir, compression, None, out, |addr, out| {
                let ok = run_scenarios(&bot, addr, &out_dir, transport, None, out)?;
                report_window(&out_dir.join("spawn.jsonl"), &deferred, out)?;
                let same = compare_with_bds(root, &format!("bds-{name}"), &out_dir, out)?;
                Ok(ok && same)
            })?;
        }
    }
    let first = compressions.first().copied().unwrap_or("snappy");
    let out_dir = dir("anchor-fallback");
    passed &= anchor_session(&out_dir, first, Some(UNREACHABLE), out, |addr, out| {
        let ok = fallback(&bot, addr, &out_dir, out)?;
        let same = compare_with_bds(root, "bds-fallback", &out_dir, out)?;
        Ok(ok && same)
    })?;
    say(out, if passed { "PASS" } else { "FAIL" })?;
    Ok(passed)
}

/// Starts Anchor on a thread of this process with the P6 settings, calls `run` with its address
/// and shuts it down. `public_ip` is the NetherNet address Anchor advertises.
fn anchor_session<W: Write>(
    out_dir: &Path,
    compression: &str,
    public_ip: Option<IpAddr>,
    out: &mut W,
    run: impl FnOnce(&str, &mut W) -> Result<bool, String>,
) -> Result<bool, String> {
    let data_dir = out_dir.join("data");
    remove_dir(&data_dir)?;
    create_dir(&data_dir)?;
    let (port, nethernet_port) = free_ports()?;
    let mut config = anchor::Config::default();
    config.general.xbox_auth = false;
    config.general.enable_ipv6 = false;
    config.general.server_port = port;
    config.general.nethernet_port = nethernet_port;
    config.general.public_ip = public_ip;
    "p6".clone_into(&mut config.general.level_name);
    config.general.level_seed = Some(1);
    config.developer.compression = match compression {
        "zlib" => anchor::protocol::compression::Compression::Zlib,
        _ => anchor::protocol::compression::Compression::Snappy,
    };
    say(
        out,
        &format!(
            "{}: starting Anchor ({compression}) on port {port}, NetherNet UDP {nethernet_port}",
            name(out_dir)
        ),
    )?;
    let server = anchor::Server::builder()
        .config(config)
        .data_dir(&data_dir)
        .build()
        .map_err(|e| format!("build Anchor: {e}"))?;
    let shutdown = server.shutdown_handle();
    let thread = spawn(move || server.run());
    let result = wait_for_pong(port).and_then(|()| run(&format!("127.0.0.1:{port}"), out));
    shutdown.shutdown();
    let stopped = thread
        .join()
        .map_err(|_| "the Anchor thread panicked".to_owned())
        .and_then(|run| run.map_err(|e| format!("Anchor stopped with an error: {e}")));
    let passed = result?;
    stopped?;
    Ok(passed)
}

/// Runs the blocks scenario into `<dir>/blocks.jsonl`: once the bot spawned, places each block of
/// `BLOCK_STEPS` with a BDS console command and closes the step with `say p6-step <n>`, then says
/// `p6-end`. Reports the block updates of each step with their states (also into
/// `<dir>/blocks.txt`). It passes if the bot passed and every step got a block update; what BDS
/// sent is recorded, not checked.
fn blocks(
    bot: &Path,
    addr: &str,
    server: &mut Bds,
    dir: &Path,
    states: &BTreeMap<u32, String>,
    out: &mut impl Write,
) -> Result<bool, String> {
    let capture = dir.join("blocks.jsonl");
    remove_file(&capture)?;
    let mut child = Command::new(bot)
        .args(["-addr", addr, "-transport", "raknet", "-scenario", "blocks"])
        .args(["-stay", BLOCKS_STAY, "-timeout", "120s", "-out"])
        .arg(&capture)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run the bot: {e}"))?;
    let placed = wait_for_spawn(&mut child, &capture).and_then(|()| {
        let (x, z) = spawn_column(&read_text(&capture)?)?;
        let base = (x + BLOCKS_OFFSET, BLOCKS_Y, z + BLOCKS_OFFSET);
        // Give BDS a moment to finish the join before the first command.
        for _ in 0..5 {
            pause();
        }
        for (n, (dx, dz, block)) in BLOCK_STEPS.iter().enumerate() {
            let (bx, by, bz) = (base.0 + dx, base.1, base.2 + dz);
            server.command(&format!("setblock {bx} {by} {bz} {block} keep"))?;
            pause();
            server.command(&format!("say {STEP_MARKER}{n}"))?;
            pause();
        }
        server.command("say p6-end")?;
        Ok(base)
    });
    if placed.is_err() {
        child.kill().map_err(|e| format!("kill the bot: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("wait for the bot: {e}"))?;
    let base = placed?;
    let passed = output.status.success();
    let failure = if passed {
        String::new()
    } else {
        format!(": {}", String::from_utf8_lossy(&output.stderr).trim())
    };
    let mut report = vec![format!(
        "{} raknet blocks: base {base:?}{failure}",
        if passed { "ok  " } else { "FAIL" }
    )];
    let steps = block_updates(&read_text(&capture)?, base, states)?;
    let mut all_updated = true;
    for (n, (dx, dz, block)) in BLOCK_STEPS.iter().enumerate() {
        let updates = steps.get(n).map_or(&[][..], Vec::as_slice);
        all_updated &= updates.iter().any(|u| u.starts_with("Update"));
        report.push(format!("  step {n}: setblock ({dx},0,{dz}) {block}"));
        if updates.is_empty() {
            report.push("    (nothing)".to_owned());
        }
        report.extend(updates.iter().map(|u| format!("    {u}")));
    }
    if !all_updated {
        report.push("FAIL blocks: a step got no block update".to_owned());
    }
    let text = report.join("\n");
    write(&dir.join("blocks.txt"), format!("{text}\n").as_bytes())?;
    say(out, &text)?;
    Ok(passed && all_updated)
}

/// The block column (`x`, `z`) of the player's position in the capture's `StartGame`.
fn spawn_column(capture: &str) -> Result<(i32, i32), String> {
    for line in capture.lines() {
        let value: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        if field(&value, "name").as_str() != Some("StartGame") {
            continue;
        }
        let position = field(field(&value, "fields"), "PlayerPosition");
        let coordinate = |i: usize| {
            position
                .get(i)
                .and_then(Value::as_f64)
                .map(f64::floor)
                .filter(|v| v.abs() < 1e6)
                .ok_or_else(|| format!("StartGame PlayerPosition {position} is not a position"))
        };
        let (x, z) = (coordinate(0)?, coordinate(2)?);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "safety: whole numbers below 1e6 in size"
        )]
        return Ok((x as i32, z as i32));
    }
    Err("the capture has no StartGame".to_owned())
}

/// Reads the reference block list (PLAN.md 7) into network ID -> `name[property=value,...]`.
fn block_states(text: &str) -> Result<BTreeMap<u32, String>, String> {
    let list: Vec<Value> =
        serde_json::from_str(text).map_err(|e| format!("{REFERENCE_BLOCKS}: {e}"))?;
    let mut map = BTreeMap::new();
    for entry in &list {
        let id = field(entry, "network_id")
            .as_u64()
            .and_then(|id| u32::try_from(id).ok())
            .ok_or_else(|| format!("{REFERENCE_BLOCKS}: bad network_id in {entry}"))?;
        let name = field(entry, "name").as_str().unwrap_or_default();
        let properties: Vec<String> = field(entry, "states")
            .as_object()
            .into_iter()
            .flatten()
            .map(|(key, typed)| {
                // Typed values: {"byte":1}, {"int":2}, {"string":"x"}.
                let value = typed.as_object().and_then(|t| t.values().next());
                let value = value.map_or_else(String::new, |v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                });
                format!("{}={value}", key.trim_start_matches("minecraft:"))
            })
            .collect();
        map.insert(id, format!("{name}[{}]", properties.join(",")));
    }
    Ok(map)
}

/// The block updates of a blocks capture per step: each `UpdateBlock` or `UpdateSubChunkBlocks`
/// entry as `<packet> (dx,dy,dz) layer <l> flags <f> <state>` relative to `base`, and the names of
/// `BlockActorData` and `UpdateBlockSynced` packets. Chunks are left out: they are the spawn's
/// chunk stream and the answers to the bot's sub-chunk requests. A step ends at the Text that
/// carries its marker.
fn block_updates(
    capture: &str,
    base: (i32, i32, i32),
    states: &BTreeMap<u32, String>,
) -> Result<Vec<Vec<String>>, String> {
    let mut steps = Vec::new();
    let mut current = Vec::new();
    let state = |id: &Value| {
        id.as_u64()
            .and_then(|id| u32::try_from(id).ok())
            .and_then(|id| states.get(&id))
            .map_or_else(|| format!("unknown {id}"), String::clone)
    };
    let relative = |position: &Value| {
        let at = |i: usize, b: i32| {
            position
                .get(i)
                .and_then(Value::as_i64)
                .map_or_else(|| "?".to_owned(), |v| (v - i64::from(b)).to_string())
        };
        format!("({},{},{})", at(0, base.0), at(1, base.1), at(2, base.2))
    };
    for line in capture.lines() {
        let value: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        if field(&value, "type").as_str() != Some("packet") {
            continue;
        }
        let fields = field(&value, "fields");
        match field(&value, "name").as_str().unwrap_or_default() {
            "Text" => {
                let message = field(fields, "Message").as_str().unwrap_or_default();
                if let Some((_, n)) = message.split_once(STEP_MARKER) {
                    if n.trim() != steps.len().to_string() {
                        return Err(format!("step marker {n} out of order"));
                    }
                    steps.push(std::mem::take(&mut current));
                }
            }
            "UpdateBlock" => current.push(format!(
                "UpdateBlock {} layer {} flags {} {}",
                relative(field(fields, "Position")),
                field(fields, "Layer"),
                field(fields, "Flags"),
                state(field(fields, "NewBlockRuntimeID"))
            )),
            "UpdateSubChunkBlocks" => {
                for (layer, key) in [(0, "Blocks"), (1, "Extra")] {
                    for entry in field(fields, key).as_array().into_iter().flatten() {
                        current.push(format!(
                            "UpdateSubChunkBlocks {} layer {layer} flags {} {}",
                            relative(field(entry, "BlockPos")),
                            field(entry, "Flags"),
                            state(field(entry, "BlockRuntimeID"))
                        ));
                    }
                }
            }
            name @ ("BlockActorData" | "UpdateBlockSynced") => {
                current.push(name.to_owned());
            }
            _ => {}
        }
    }
    Ok(steps)
}

/// Compares `<out_dir>/spawn.jsonl` with the spawn capture of the BDS run `bds_name`.
fn compare_with_bds(
    root: &Path,
    bds_name: &str,
    out_dir: &Path,
    out: &mut impl Write,
) -> Result<bool, String> {
    let bds = root.join(OUT_DIR).join(bds_name).join("spawn.jsonl");
    if !is_file(&bds) {
        say(
            out,
            &format!("no BDS capture at {}; run `p6 bds` first", bds.display()),
        )?;
        return Ok(false);
    }
    say(out, &format!("compare {bds_name} {}:", name(out_dir)))?;
    // A capture without a spawn window fails the comparison; the other runs still go on.
    compare_files(root, &bds, &out_dir.join("spawn.jsonl"), out).or_else(|e| {
        say(out, &format!("DIFFERENT: {e}"))?;
        Ok(false)
    })
}

/// The last component of a path, for messages.
fn name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
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

/// Runs the `auto` bot into `<dir>/spawn.jsonl`; it passes if the bot spawned over RakNet.
fn fallback(bot: &Path, addr: &str, dir: &Path, out: &mut impl Write) -> Result<bool, String> {
    create_dir(dir)?;
    let capture = dir.join("spawn.jsonl");
    let outcome = run_bot(
        bot,
        &["-addr", addr, "-transport", "auto", "-scenario", "spawn"],
        &capture,
    )?;
    let result = compare::result(&read_text(&capture)?)?;
    let transport = field(&result, "transport").as_str().unwrap_or_default();
    let ok = outcome.passed && transport == "raknet";
    say(
        out,
        &format!(
            "{} auto fallback: transport {transport:?}, NetherNet error {}{}",
            if ok { "ok  " } else { "FAIL" },
            field(&result, "nethernet_error"),
            outcome.failure()
        ),
    )?;
    Ok(ok)
}

/// Runs the bind-mismatch scenario into `<dir>/bind-mismatch.jsonl` and reports what the server
/// did; the outcome is recorded, not checked.
fn record_bind_mismatch(
    bot: &Path,
    addr: &str,
    dir: &Path,
    out: &mut impl Write,
) -> Result<(), String> {
    create_dir(dir)?;
    let capture = dir.join("bind-mismatch.jsonl");
    let args = [
        "-addr",
        addr,
        "-transport",
        "nethernet",
        "-scenario",
        "bind-mismatch",
    ];
    let outcome = run_bot(bot, &args, &capture)?;
    let result = compare::result(&read_text(&capture)?)?;
    say(
        out,
        &format!(
            "note bind-mismatch (recorded): {}: {}",
            if outcome.passed {
                "refused"
            } else {
                "not refused"
            },
            field(&result, "reason")
        ),
    )
}

/// Logs in twice with the same name: the first bot (`kicked`, into `<dir>/first.jsonl`) spawns
/// and waits for its Disconnect while the second (`spawn`, `<dir>/second.jsonl`) joins. When the
/// first is kicked with `LoggedInOtherLocation`, as in PMMP, both must pass; otherwise the server's
/// behaviour is recorded and the run passes.
fn duplicate_login(
    bot: &Path,
    addr: &str,
    dir: &Path,
    out: &mut impl Write,
) -> Result<bool, String> {
    create_dir(dir)?;
    let (first_capture, second_capture) = (dir.join("first.jsonl"), dir.join("second.jsonl"));
    remove_file(&first_capture)?;
    let mut first = Command::new(bot)
        .args([
            "-addr",
            addr,
            "-transport",
            "nethernet",
            "-scenario",
            "kicked",
        ])
        .args(["-name", TWIN, "-stay", TWIN_STAY, "-timeout", "60s", "-out"])
        .arg(&first_capture)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run the bot: {e}"))?;
    let second = wait_for_spawn(&mut first, &first_capture).and_then(|()| {
        let args = [
            "-addr",
            addr,
            "-transport",
            "nethernet",
            "-scenario",
            "spawn",
            "-name",
            TWIN,
        ];
        run_bot(bot, &args, &second_capture)
    });
    let first_status = match &second {
        Ok(_) => first.wait().map_err(|e| e.to_string()),
        Err(_) => first
            .kill()
            .and_then(|()| first.wait())
            .map_err(|e| e.to_string()),
    };
    let second = second?;
    let first_status = first_status?;
    let first_text = read_text(&first_capture)?;
    let first_result = compare::result(&first_text)?;
    let players = compare::added_players(&first_text)?;
    if first_status.success() {
        say(
            out,
            &format!(
                "{} duplicate login: the first player was kicked ({}, {}), like PMMP{}",
                if second.passed { "ok  " } else { "FAIL" },
                field(&first_result, "disconnect_reason"),
                field(&first_result, "disconnect_message"),
                second.failure()
            ),
        )?;
        return Ok(second.passed);
    }
    say(
        out,
        &format!(
            "note duplicate login (recorded, not PMMP's behaviour): first player: {}; second \
             spawned: {}; players the first saw added: {}",
            field(&first_result, "reason"),
            second.passed,
            players.join(", ")
        ),
    )?;
    Ok(true)
}

/// Waits until the bot writing `capture` sent `SetLocalPlayerAsInitialised` (the bot flushes its
/// output then).
fn wait_for_spawn(child: &mut Child, capture: &Path) -> Result<(), String> {
    let deadline = now() + START_TIMEOUT;
    while now() < deadline {
        if read_text(capture).is_ok_and(|t| t.contains("\"name\":\"SetLocalPlayerAsInitialised\""))
        {
            return Ok(());
        }
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("the first bot exited ({status}) before spawning"));
        }
        pause();
    }
    Err(format!(
        "the first bot did not spawn within {START_TIMEOUT:?}"
    ))
}

/// Prints the signaling facts of a NetherNet capture: the /v1/join answer, the answer SDP line
/// structure and whether the server sent a handshake.
fn report_nethernet(capture: &Path, out: &mut impl Write) -> Result<(), String> {
    let Ok(text) = read_text(capture) else {
        return Ok(());
    };
    let result = compare::result(&text)?;
    say(
        out,
        &format!(
            "/v1/join ({}): {}",
            field(&result, "join_scheme"),
            field(&result, "join")
        ),
    )?;
    if let Some(structure) = compare::answer_structure(&text)? {
        say(out, &format!("answer SDP lines: {}", structure.join(" ")))?;
    }
    say(
        out,
        &format!(
            "ServerToClientHandshake: {}, encrypted: {}",
            field(&result, "handshake"),
            field(&result, "encrypted")
        ),
    )
}

/// Pings the server until it answers with a RakNet pong, so the bot does not race its start.
fn wait_for_pong(port: u16) -> Result<(), String> {
    wait_for_pong_where(port, |_| true, "answer pings")
}

/// Pings the server until its pong reports no player online, so a scenario does not meet the
/// player of the one before it (BDS keeps a left player for a while).
fn wait_until_empty(addr: &str) -> Result<(), String> {
    let port = addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| format!("no port in {addr}"))?;
    // The pong text is `MCPE;motd;protocol;version;online;max;...`.
    wait_for_pong_where(
        port,
        |data| data.split(';').nth(4) == Some("0"),
        "become empty",
    )
}

/// Pings the server until a pong's text satisfies `accept`.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: xtask is a development tool"
)]
fn wait_for_pong_where(port: u16, accept: impl Fn(&str) -> bool, what: &str) -> Result<(), String> {
    const MAGIC: [u8; 16] = [
        0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56,
        0x78,
    ];
    // Pong: ID, time (8), server GUID (8), magic (16), text length (2), text.
    const TEXT: usize = 35;
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
            && buf.get(..n).and_then(|b| b.first()) == Some(&0x1c)
            && accept(&String::from_utf8_lossy(
                buf.get(TEXT..n).unwrap_or_default(),
            ))
        {
            return Ok(());
        }
        // An ICMP "port unreachable" makes `recv_from` fail at once on Windows.
        pause();
    }
    Err(format!(
        "the server on port {port} did not {what} within {START_TIMEOUT:?}"
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
            .filter(|k| {
                matches!(
                    *k,
                    "addr" | "out" | "transport" | "token-key" | "kid" | "iss"
                )
            })
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
    let transport = flags.get("transport").copied().unwrap_or("raknet");
    addr.parse::<SocketAddr>()
        .map_err(|e| format!("--addr {addr}: {e}"))?;
    let out_dir = PathBuf::from(out_dir);
    create_dir(&out_dir)?;
    let bot = build_bot(root)?;
    let passed = run_scenarios(&bot, addr, &out_dir, transport, online, out)?;
    report_window(&out_dir.join("spawn.jsonl"), &load_deferred(root)?, out)?;
    say(out, if passed { "PASS" } else { "FAIL" })?;
    Ok(passed)
}

/// `p6 compare`: compares the spawn windows of two captures, their answer SDP structures when
/// either has one and their full item packets when both have them; prints the item notes.
fn compare_files(root: &Path, a: &Path, b: &Path, out: &mut impl Write) -> Result<bool, String> {
    let deferred = load_deferred(root)?;
    let fields = compare::parse_fields(&read_text(&root.join(compare::FIELDS_PATH))?, &deferred)?;
    let (text_a, text_b) = (read_text(a)?, read_text(b)?);
    let window = |path: &Path, text: &str| {
        compare::window(text).map_err(|e| format!("{}: {e}", path.display()))
    };
    let mut differences = compare::compare(
        &window(a, &text_a)?,
        &window(b, &text_b)?,
        &deferred,
        &fields,
    );
    let structure = |path: &Path, text: &str| {
        compare::answer_structure(text).map_err(|e| format!("{}: {e}", path.display()))
    };
    match (structure(a, &text_a)?, structure(b, &text_b)?) {
        (Some(x), Some(y)) => differences.extend(compare::compare_structure(&x, &y)),
        (None, None) => {}
        _ => differences.push("only one capture has an answer SDP".to_owned()),
    }
    // The full item packets; a capture without them (an older bot) skips these checks.
    let (items_a, items_b) = (items_path(a), items_path(b));
    if is_file(&items_a) && is_file(&items_b) {
        let items = |path: &Path| {
            compare::parse_items(&read_text(path)?).map_err(|e| format!("{}: {e}", path.display()))
        };
        let (item_differences, notes) =
            compare::compare_items(&items(&items_a)?, &items(&items_b)?);
        differences.extend(item_differences);
        for note in &notes {
            say(out, &format!("note: {note}"))?;
        }
    }
    for difference in &differences {
        say(out, difference)?;
    }
    let same = differences.is_empty();
    say(out, if same { "SAME" } else { "DIFFERENT" })?;
    Ok(same)
}

/// The bot's full item packet file of a capture: `x.jsonl` -> `x.items.jsonl`.
fn items_path(capture: &Path) -> PathBuf {
    capture.with_extension("items.jsonl")
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

/// How a bot run ended (exit code 0 or 1; 2 is an error).
#[derive(Debug)]
struct Outcome {
    passed: bool,
    stderr: String,
}

impl Outcome {
    /// The bot's failure message, or nothing when it passed.
    fn failure(&self) -> String {
        if self.passed {
            String::new()
        } else {
            format!(": {}", self.stderr.trim())
        }
    }
}

/// Runs the bot with `args` and `-out capture`.
fn run_bot(bot: &Path, args: &[&str], capture: &Path) -> Result<Outcome, String> {
    let output = Command::new(bot)
        .args(args)
        .args(["-timeout", "60s", "-out"])
        .arg(capture)
        .output()
        .map_err(|e| format!("cannot run the bot: {e}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    match output.status.code() {
        Some(0 | 1) => Ok(Outcome {
            passed: output.status.success(),
            stderr,
        }),
        _ => Err(format!("the bot failed ({}): {stderr}", output.status)),
    }
}

/// Runs each scenario over the transport into `<out_dir>/<scenario>.jsonl`; returns whether all
/// passed.
fn run_scenarios(
    bot: &Path,
    addr: &str,
    out_dir: &Path,
    transport: &str,
    online: Option<[&str; 3]>,
    out: &mut impl Write,
) -> Result<bool, String> {
    let mut passed = true;
    let online_scenario = online.map(|_| "online");
    for scenario in SCENARIOS.iter().copied().chain(online_scenario) {
        let capture = out_dir.join(format!("{scenario}.jsonl"));
        let mut args = vec![
            "-addr",
            addr,
            "-transport",
            transport,
            "-scenario",
            scenario,
        ];
        if let (Some([key, kid, iss]), "online") = (online, scenario) {
            args.extend(["-token-key", key, "-kid", kid, "-iss", iss]);
        }
        if transport == "nethernet" {
            args.extend(["-first-send-delay", FIRST_SEND_DELAY]);
        }
        let outcome = run_bot(bot, &args, &capture)?;
        passed &= outcome.passed;
        let status = if outcome.passed { "ok  " } else { "FAIL" };
        say(
            out,
            &format!("{status} {transport} {scenario}{}", outcome.failure()),
        )?;
        if transport == "nethernet" {
            report_segments(&capture, out)?;
        }
    }
    Ok(passed)
}

/// Runs big-skin over NetherNet with the bot sending segments of `size` payload bytes into
/// `<out_dir>/big-skin-<size>.jsonl` and reports whether the server accepted them; the outcome is
/// recorded, not checked.
fn record_segment_size(
    bot: &Path,
    addr: &str,
    out_dir: &Path,
    size: &str,
    out: &mut impl Write,
) -> Result<(), String> {
    let capture = out_dir.join(format!("big-skin-{size}.jsonl"));
    let args = [
        "-addr",
        addr,
        "-transport",
        "nethernet",
        "-scenario",
        "big-skin",
        "-segment-size",
        size,
        "-first-send-delay",
        FIRST_SEND_DELAY,
    ];
    let outcome = run_bot(bot, &args, &capture)?;
    say(
        out,
        &format!(
            "note big-skin with sent segments of {size} (recorded): {}{}",
            if outcome.passed {
                "accepted"
            } else {
                "not accepted"
            },
            outcome.failure()
        ),
    )?;
    report_segments(&capture, out)
}

/// Prints the NetherNet segment facts of a capture's result: what the server sent (the largest
/// segment payload, segmented messages and their non-final segment sizes, messages on the
/// unreliable channel, anything sent before the bot's first packet) and what the bot sent.
fn report_segments(capture: &Path, out: &mut impl Write) -> Result<(), String> {
    let Ok(text) = read_text(capture) else {
        return Ok(());
    };
    let result = compare::result(&text)?;
    let f = |key| field(&result, key);
    say(
        out,
        &format!(
            "  segments received: answer max-message-size {}, max payload {}, segmented messages \
             {} (non-final payloads {}), unreliable messages {}, before the first client packet \
             {}, unhooked {}; sent: max payload {}, segmented messages {}",
            f("answer_max_message_size"),
            f("max_segment_payload"),
            f("segmented_messages"),
            f("non_last_segment_payloads"),
            f("unreliable_messages"),
            f("first_server_message_before_client"),
            f("unhooked_messages"),
            f("max_sent_segment_payload"),
            f("sent_segmented_messages"),
        ),
    )
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

    /// Sends one console command.
    fn command(&mut self, line: &str) -> Result<(), String> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "BDS has no console input".to_owned())?;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("BDS console: {e}"))
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

/// Removes a file; a missing file is fine.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn remove_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
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
