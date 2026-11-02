//! `cargo xtask p6`: the reference server comparison. It builds the Go
//! bot (`xtask/go/bot`), runs its scenarios against BDS or any running server, and compares
//! captures. It needs Go and, for `bds`, the BDS copy in `.cache/bds/`, so `verify` does not run it.
//!
//! - `p6 bds [zlib|snappy]...`: for each compression (default both), writes the test settings into
//!   the copy's `server.properties`, deletes its `worlds` folder, starts `bedrock_server` in the
//!   copy and stops it with `stop` after each run. With `transport=raknet` it runs the scenarios
//!   into `target/p6/bds-<compression>/`; with `transport=nethernet` (signaling on `server-port`)
//!   into `target/p6/bds-nethernet-<compression>/`, plus big-skin with the bot sending 65,535-byte
//!   segments (`big-skin-65535.jsonl`, recorded). NetherNet runs print the segment facts of each
//!   capture (what the server sent per data channel message). In the run of
//!   `FALLBACK_COMPRESSION` it also runs the `auto` bot against the RakNet BDS (`bds-fallback/`,
//!   it must fall back to RakNet and spawn). Once, in the first compression's runs, it records,
//!   against the NetherNet BDS, what a mismatched offer identity (`bds-bind-mismatch/`) and a
//!   second login with the same name (`bds-duplicate/`) do. The duplicate login is a check only if BDS kicks
//!   the first player as PMMP does. Each RakNet run also runs the `blocks` scenario
//!   (`bds-<compression>/blocks.jsonl`): while the bot stays spawned, BDS console commands place
//!   fences, panes, bars, walls, a gate, stairs and tripwire next to each other near the player
//!   and remove some again; a marker block set after each step delimits the steps (`MARKER`), and
//!   the block updates BDS sent for each step are reported with their states (`blocks.txt`). The
//!   biome definitions and jigsaw data of each spawn capture are checked against `CloudburstMC`'s
//!   data (`cloudburst`). Last, it runs the game session once (`game`, every game scenario into
//!   `target/p6/bds-game/`) and the API session once (`api`, every API scenario into
//!   `target/p6/bds-api/`).
//! - `p6 api bds|anchor [scenario]...`, `p6 api compare A B`: the API session alone (see `api`).
//! - `p6 import`: a world BDS made, imported into Anchor, sends the same blocks (see `import`).
//! - `p6 game bds [scenario]...`: the game session alone; `p6 game anchor [scenario]...`: the
//!   same session against Anchor into `target/p6/anchor-game/`, compared with `bds-game/`;
//!   `p6 game compare A B` compares the game traces of two capture folders (see `game`).
//! - `p6 anchor [zlib|snappy]...`: starts Anchor in this process with the same settings and runs
//!   the same RakNet and NetherNet scenarios into `target/p6/anchor-<compression>/` and
//!   `anchor-nethernet-<compression>/`, and the `auto` bot, with `FALLBACK_COMPRESSION`, against an
//!   unreachable NetherNet address into `anchor-fallback/`; each capture is compared with its BDS
//!   capture. Last, it runs
//!   the game session against Anchor (`p6 game anchor`, into `anchor-game/`, compared with
//!   `bds-game/`) and the API session (`p6 api anchor`, into `anchor-api/`, compared with
//!   `bds-api/`).
//! - `p6 run --addr HOST:PORT --out DIR [--transport T] [--token-key PEM --kid KID --iss ISS]`:
//!   runs the scenarios against a running server; the token flags add the online scenario.
//! - `p6 compare A B`: compares the spawn windows of two captures, their answer SDP structures
//!   when they have them, their full item packets (`<capture>.items.jsonl`) when both have them,
//!   their chunk packets, and the `blocks.jsonl` captures next to them when both exist (see
//!   `compare`).

mod api;
mod compare;
mod game;
mod import;

use std::collections::BTreeMap;
use std::io::Write;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::dump::{CLOUDBURST, REFERENCE_BLOCKS, REFERENCE_DIR, create_dir, is_file, write};
use compare::field;

/// The BDS copy, relative to the repository root. The original download is never run.
const BDS_DIR: &str = ".cache/bds/1.26.60.6";
/// Test settings written into the copy's `server.properties`, besides the transport, ports,
/// compression and seed of each run and the lines of [`default_settings`]. The login (offline),
/// the world name ([`LEVEL_NAME`]) and the flat generator are fixed for the comparison, and
/// [`anchor_config`] and [`anchor_session`] give Anchor the same; the lines of
/// [`default_settings`] are Anchor's defaults, so config-dependent fields compare equal. LAN
/// visibility is on because BDS answers RakNet pings with empty data without it; it also binds
/// UDP 19132 and 19133.
const BDS_SETTINGS: &[(&str, &str)] = &[
    ("online-mode", "false"),
    ("allow-list", "false"),
    ("allow-cheats", "false"),
    ("enable-lan-visibility", "true"),
    ("level-name", LEVEL_NAME),
    ("level-type", "FLAT"),
];
/// The BDS log line of a `setblock` that ran.
const BLOCK_PLACED: &str = "Block placed";
/// The steps of the blocks scenario: one block placed at (`dx`, `dy`, `dz`) from the base position
/// with `setblock ... keep`, or removed with `setblock ... air replace` when the block is `air`. The
/// groups are 3 blocks apart on `z`, so they do not touch each other.
const BLOCK_STEPS: &[(i32, i32, i32, &str)] = &[
    // Two fences side by side.
    (0, 0, 0, "oak_fence"),
    (1, 0, 0, "oak_fence"),
    // A fence placed next to a solid block, and a solid block placed next to a fence.
    (0, 0, 3, "stone"),
    (1, 0, 3, "oak_fence"),
    (4, 0, 3, "oak_fence"),
    (5, 0, 3, "stone"),
    // Glass panes in an L.
    (0, 0, 6, "glass_pane"),
    (1, 0, 6, "glass_pane"),
    (0, 0, 7, "glass_pane"),
    // Iron bars in a line, then copper bars at its end.
    (0, 0, 10, "iron_bars"),
    (1, 0, 10, "iron_bars"),
    (2, 0, 10, "iron_bars"),
    (3, 0, 10, "copper_bars"),
    // Stairs facing east (weirdo_direction 0), then one facing south (2) on its east side and on
    // its west side: one pair makes an outer corner and the other an inner corner.
    (0, 0, 13, r#"oak_stairs ["weirdo_direction"=0]"#),
    (1, 0, 13, r#"oak_stairs ["weirdo_direction"=2]"#),
    (1, 0, 16, r#"oak_stairs ["weirdo_direction"=0]"#),
    (0, 0, 16, r#"oak_stairs ["weirdo_direction"=2]"#),
    // A lone stair.
    (0, 0, 19, r#"oak_stairs ["weirdo_direction"=0]"#),
    // Tripwire hooks on stone, facing each other, then the tripwire between them; then two
    // tripwires without hooks.
    (0, 0, 22, "stone"),
    (4, 0, 22, "stone"),
    (1, 0, 22, r#"tripwire_hook ["direction"=3]"#),
    (3, 0, 22, r#"tripwire_hook ["direction"=1]"#),
    (2, 0, 22, "trip_wire"),
    (6, 0, 22, "trip_wire"),
    (7, 0, 22, "trip_wire"),
    // Cobblestone walls: alone, in a line, in an L, then between a fence and a pane.
    (0, 0, 25, "cobblestone_wall"),
    (0, 0, 28, "cobblestone_wall"),
    (1, 0, 28, "cobblestone_wall"),
    (2, 0, 28, "cobblestone_wall"),
    (0, 0, 31, "cobblestone_wall"),
    (1, 0, 31, "cobblestone_wall"),
    (0, 0, 32, "cobblestone_wall"),
    (1, 0, 35, "cobblestone_wall"),
    (0, 0, 35, "oak_fence"),
    (2, 0, 35, "glass_pane"),
    // A nether brick fence next to an oak fence.
    (0, 0, 38, "oak_fence"),
    (1, 0, 38, "nether_brick_fence"),
    // A fence gate between two fences; facing south it spans the x axis, the fence line.
    (0, 0, 41, "oak_fence"),
    (2, 0, 41, "oak_fence"),
    (
        1,
        0,
        41,
        r#"fence_gate ["minecraft:cardinal_direction"="south"]"#,
    ),
    // A pane next to a wall.
    (0, 0, 44, "cobblestone_wall"),
    (1, 0, 44, "glass_pane"),
    // A stair next to a full block, then a fence on the stair's other side.
    (0, 0, 47, "stone"),
    (1, 0, 47, r#"oak_stairs ["weirdo_direction"=0]"#),
    (2, 0, 47, "oak_fence"),
    // Removing blocks with connected neighbours: the middle wall of the line, then the second
    // fence of the first pair.
    (1, 0, 28, "air"),
    (1, 0, 0, "air"),
    // A block high above the ground in the spawn chunk, for the sub-chunk height maps the bot
    // requests at the end (columns of a sub-chunk below and above their highest block).
    (0, 40, -2, "stone"),
];
/// How far the blocks scenario's base position is from the player's spawn column, on `x` and `z`.
const BLOCKS_OFFSET: i32 = 2;
/// How long the blocks bot may stay spawned; it leaves on the end marker.
const BLOCKS_STAY: &str = "90s";
/// The `y` of the blocks scenario: the first air layer of BDS's default flat world (bedrock at
/// -64, two dirt layers, grass at -61). Every step uses `keep`, so a wrong height places nothing
/// and fails the run.
const BLOCKS_Y: i32 = -60;
/// How long BDS may take to run one console command of the blocks scenario.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// The marker block that ends the scenario; the bot leaves when it sees it (`xtask/go/bot`).
const END_MARKER: &str = "diamond_block";
/// The compression of the `auto` fallback runs, which `p6 bds` records and `p6 anchor` compares
/// whatever compressions their arguments name.
const FALLBACK_COMPRESSION: &str = "zlib";
/// How long a NetherNet bot waits after the data channels open before its first packet, so the
/// capture shows whether the server sends anything unprompted.
const FIRST_SEND_DELAY: &str = "1s";
/// The name of the world both servers run, fixed for the comparison.
const LEVEL_NAME: &str = "p6";
/// The seed of the world both servers run, fixed for the comparison.
const LEVEL_SEED: i64 = 1;
/// The addresses the Anchor server of this process logged while starting (`listening … addr=…`),
/// kept by [`AnchorLog`].
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: the log of the server's threads hands the harness its addresses"
)]
static LISTENED: std::sync::Mutex<Vec<(String, SocketAddr)>> = std::sync::Mutex::new(Vec::new());
/// The result of making [`AnchorLog`] the global log of this process.
static LOG: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
/// The marker position of the blocks scenario, (`dx`, `dz`) from the base position: no step block
/// is next to it. After each step the marker is set to the step's marker block, so every step ends
/// with a single-block update there; both servers can make it with block changes only.
const MARKER: (i32, i32) = (-4, 0);
/// Captures and logs, relative to the repository root.
const OUT_DIR: &str = "target/p6";
const POLL: Duration = Duration::from_millis(200);
/// Where the text of a RakNet pong starts: ID, time (8), server GUID (8), magic (16), text length
/// (2).
const PONG_TEXT_OFFSET: usize = 35;
/// The magic bytes of RakNet's unconnected messages.
const RAKNET_MAGIC: [u8; 16] = [
    0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56, 0x78,
];
/// Scenarios every server must pass, over each transport.
const SCENARIOS: &[&str] = &["spawn", "big-skin"];
/// A segment payload smaller than go-nethernet's 262,143 (the bot's default): what a sender
/// limited to 64 KiB SCTP messages uses. `p6 bds` sends big-skin in it to record whether BDS
/// accepts it; Anchor accepts both.
const SMALL_SEGMENT: &str = "65535";
const START_TIMEOUT: Duration = Duration::from_mins(2);
/// The marker blocks of the steps, alternating: step `n` ends with `STEP_MARKERS[n % 2]`.
const STEP_MARKERS: [&str; 2] = ["gold_block", "iron_block"];
/// Ticks between two block changes of Anchor's steps: 200 ms, the pause between BDS commands.
const STEP_TICKS: std::num::NonZeroU32 = std::num::NonZeroU32::new(4).unwrap();
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// The player name of both bots of the duplicate-login run.
const TWIN: &str = "P6Twin";
/// How long the first bot of the duplicate-login run waits for its Disconnect.
const TWIN_STAY: &str = "20s";
/// The address Anchor advertises in the fallback run: an IPv6 documentation address (RFC 3849)
/// that nothing answers, so the NetherNet negotiation fails in the client's ICE start. An
/// unreachable IPv4 address does not fail: Anchor's own ICE checks give the client a
/// peer-reflexive candidate. An IPv6 address needs `enable-ipv6`.
const UNREACHABLE: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
/// The command forms, for the usage text.
pub(crate) const USAGE: &str = "cargo xtask p6 bds [zlib|snappy]... | cargo xtask p6 anchor \
                                [zlib|snappy]... | cargo xtask p6 run \
                                --addr HOST:PORT --out DIR [--transport raknet|nethernet|auto] \
                                [--token-key PEM --kid KID --iss ISS] | cargo xtask p6 compare A B \
                                | cargo xtask p6 game bds [scenario]... \
                                | cargo xtask p6 game anchor [scenario]... \
                                | cargo xtask p6 game compare DIR_A DIR_B \
                                | cargo xtask p6 api bds [scenario]... \
                                | cargo xtask p6 api anchor [scenario]... \
                                | cargo xtask p6 api compare DIR_A DIR_B \
                                | cargo xtask p6 import";

/// Runs `cargo xtask p6 <args>`.
pub(crate) fn command(root: &Path, args: &[&str], out: &mut impl Write) -> Result<bool, String> {
    match args {
        ["bds", rest @ ..] => bds(root, rest, out),
        ["import"] => import::command(root, out),
        ["anchor", rest @ ..] => anchor(root, rest, out),
        ["run", rest @ ..] => run(root, rest, out),
        ["compare", a, b] => compare_files(root, Path::new(a), Path::new(b), out),
        ["game", "bds", rest @ ..] => game::bds(root, rest, out),
        ["game", "anchor", rest @ ..] => game::anchor(root, rest, out),
        ["game", "compare", a, b] => game::compare(root, Path::new(a), Path::new(b), out),
        ["api", "bds", rest @ ..] => api::bds(root, rest, out),
        ["api", "anchor", rest @ ..] => api::anchor(root, rest, out),
        ["api", "compare", a, b] => api::compare(root, Path::new(a), Path::new(b), out),
        _ => Err(format!("usage: {USAGE}")),
    }
}

fn say(out: &mut impl Write, text: &str) -> Result<(), String> {
    writeln!(out, "{text}").map_err(|e| e.to_string())
}

/// The scenarios named in `args` out of `all` (all when `args` is empty); `session` names the
/// session in the error.
fn pick_scenarios<'a>(
    args: &[&'a str],
    all: &[&'a str],
    session: &str,
) -> Result<Vec<&'a str>, String> {
    if args.is_empty() {
        return Ok(all.to_vec());
    }
    args.iter()
        .map(|a| {
            all.iter()
                .find(|s| *s == a)
                .map(|_| *a)
                .ok_or_else(|| format!("unknown {session} scenario `{a}` (want {all:?})"))
        })
        .collect()
}

/// Says so and returns true when `scenario` has no capture in the folder `a` or `b`.
fn say_missing_capture(
    a: &Path,
    b: &Path,
    scenario: &str,
    out: &mut impl Write,
) -> Result<bool, String> {
    let capture = |dir: &Path| dir.join(format!("{scenario}.jsonl"));
    let Some(dir) = [a, b].into_iter().find(|d| !is_file(&capture(d))) else {
        return Ok(false);
    };
    say(
        out,
        &format!("DIFFERENT {scenario}: no capture in {}", dir.display()),
    )?;
    Ok(true)
}

/// Writes the report `compare` makes under `title` to `<dir>/compare.txt` and `out`, and returns
/// whether the captures are the same. An error of the comparison is a difference in the report.
fn report_comparison<W: Write>(
    dir: &Path,
    title: &str,
    out: &mut W,
    compare: impl FnOnce(&mut Vec<u8>) -> Result<bool, String>,
) -> Result<bool, String> {
    let mut report = Vec::new();
    say(&mut report, title)?;
    let same = compare(&mut report).or_else(|e| {
        say(&mut report, &format!("DIFFERENT: {e}"))?;
        Ok::<_, String>(false)
    })?;
    write(&dir.join("compare.txt"), &report)?;
    out.write_all(&report).map_err(|e| e.to_string())?;
    Ok(same)
}

/// Whether the report of `scenario` was written; a failure is said as a failed scenario, so one
/// broken capture does not end the session.
fn report_written(
    scenario: &str,
    written: Result<(), String>,
    out: &mut impl Write,
) -> Result<bool, String> {
    match written {
        Ok(()) => Ok(true),
        Err(e) => {
            say(out, &format!("FAIL {scenario} report: {e}"))?;
            Ok(false)
        }
    }
}

/// The BDS executable of the copy for this OS (`bedrock_server.exe` on Windows, `bedrock_server`
/// on Linux), or an error saying where to copy BDS when it is missing.
fn bds_exe(root: &Path) -> Result<PathBuf, String> {
    let exe = root
        .join(BDS_DIR)
        .join(format!("bedrock_server{}", std::env::consts::EXE_SUFFIX));
    if is_file(&exe) {
        return Ok(exe);
    }
    let version = BDS_DIR.rsplit('/').next().unwrap_or(BDS_DIR);
    Err(format!(
        "{} is missing: copy BDS {version} for this OS into {BDS_DIR} (never run the original)",
        exe.display()
    ))
}

/// The `server.properties` lines whose values are Anchor's defaults, taken from them.
fn default_settings() -> [(&'static str, String); 5] {
    let config = anchor::Config::default();
    let gamemode = match config.general.gamemode {
        anchor::config::GameMode::Survival => "survival",
        anchor::config::GameMode::Creative => "creative",
        anchor::config::GameMode::Adventure => "adventure",
        anchor::config::GameMode::Spectator => "spectator",
    };
    let difficulty = match config.general.difficulty {
        anchor::config::Difficulty::Peaceful => "peaceful",
        anchor::config::Difficulty::Easy => "easy",
        anchor::config::Difficulty::Normal => "normal",
        anchor::config::Difficulty::Hard => "hard",
    };
    [
        ("max-players", config.general.max_players.to_string()),
        ("view-distance", config.general.view_distance.to_string()),
        (
            "compression-threshold",
            config.developer.compression_threshold.to_string(),
        ),
        ("gamemode", gamemode.to_owned()),
        ("difficulty", difficulty.to_owned()),
    ]
}

/// `p6 bds`: runs the scenarios against BDS once per compression and transport.
fn bds(root: &Path, args: &[&str], out: &mut impl Write) -> Result<bool, String> {
    let compressions = compressions(args)?;
    let exe = bds_exe(root)?;
    let bot = build_bot(root)?;
    let deferred = load_deferred(root)?;
    let states = block_states(root)?;
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
                let mut ok = run_scenarios(&bot, addr, addr, &raknet, "raknet", None, out)?;
                report_window(&raknet.join("spawn.jsonl"), &deferred, out)?;
                ok &= cloudburst(root, &bot, &raknet, out)?;
                // The fallback capture is compared with Anchor's, so it runs before the blocks
                // scenario changes the world.
                if *compression == FALLBACK_COMPRESSION {
                    wait_until_empty(addr)?;
                    ok &= fallback(&bot, addr, addr, &dir("bds-fallback"), out)?;
                }
                ok &= blocks(&bot, addr, Placer::Bds(server), &raknet, &states, out)?;
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
                let mut ok = run_scenarios(&bot, addr, addr, &nethernet, "nethernet", None, out)?;
                report_window(&nethernet.join("spawn.jsonl"), &deferred, out)?;
                ok &= cloudburst(root, &bot, &nethernet, out)?;
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
    passed &= game::session(root, &exe, &bot, &game::SCENARIOS, out)?;
    passed &= api::session(root, &exe, &bot, &api::SCENARIOS, out)?;
    say(out, if passed { "PASS" } else { "FAIL" })?;
    Ok(passed)
}

/// Checks the biome definitions and jigsaw structure data of `<dir>/spawn.world.jsonl` against
/// `CloudburstMC`'s data with the bot (`bot cloudburst`) and prints the result. Any
/// difference, or either packet missing, fails, on BDS's captures and on Anchor's alike.
fn cloudburst(root: &Path, bot: &Path, dir: &Path, out: &mut impl Write) -> Result<bool, String> {
    let output = Command::new(bot)
        .arg("cloudburst")
        .arg("-world")
        .arg(dir.join("spawn.world.jsonl"))
        .arg("-data")
        .arg(root.join(CLOUDBURST.0))
        .output()
        .map_err(|e| format!("cannot run the bot: {e}"))?;
    let same = match output.status.code() {
        Some(0) => true,
        Some(1) => false,
        _ => {
            return Err(format!(
                "bot cloudburst failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        say(out, &format!("CloudburstMC check: {line}"))?;
    }
    Ok(same)
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
    let session = BdsSession {
        transport,
        compression,
        settings: &[],
    };
    bds_session_with(exe, out_dir, &session, |_| Ok(()), out, run)
}

/// What a BDS session runs with besides the test settings.
struct BdsSession<'a> {
    transport: &'a str,
    compression: &'a str,
    /// Settings that override the test settings.
    settings: &'a [(&'a str, &'a str)],
}

/// [`bds_session`] with more settings, and `prepare` called with the copy's folder once its
/// `worlds` folder is gone, before BDS starts (to install a pack into the new world).
fn bds_session_with<W: Write>(
    exe: &Path,
    out_dir: &Path,
    session: &BdsSession<'_>,
    prepare: impl FnOnce(&Path) -> Result<(), String>,
    out: &mut W,
    run: impl FnOnce(&str, &mut Bds, &mut W) -> Result<bool, String>,
) -> Result<bool, String> {
    let BdsSession {
        transport,
        compression,
        settings: extra,
    } = *session;
    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no parent", exe.display()))?;
    create_dir(out_dir)?;
    let (port, port_v6) = free_ports()?;
    let (port_text, port_v6_text) = (port.to_string(), port_v6.to_string());
    let seed_text = LEVEL_SEED.to_string();
    let defaults = default_settings();
    let mut settings = BDS_SETTINGS.to_vec();
    settings.extend(defaults.iter().map(|(key, value)| (*key, value.as_str())));
    settings.extend([
        ("level-seed", seed_text.as_str()),
        ("transport", transport),
        ("server-port", port_text.as_str()),
        ("server-portv6", port_v6_text.as_str()),
        ("compression-algorithm", compression),
    ]);
    settings.extend_from_slice(extra);
    set_properties(&dir.join("server.properties"), &settings)?;
    remove_dir(&dir.join("worlds"))?;
    prepare(dir)?;
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
    let states = block_states(root)?;
    let dir = |name: &str| root.join(OUT_DIR).join(name);
    let mut passed = true;
    for compression in &compressions {
        for (transport, name) in [
            ("raknet", (*compression).to_owned()),
            ("nethernet", format!("nethernet-{compression}")),
        ] {
            let out_dir = dir(&format!("anchor-{name}"));
            let (bases, base) = channel();
            let steps = anchor_steps(base)?;
            let with_steps = |b: anchor::ServerBuilder| b.every(STEP_TICKS, steps);
            passed &= anchor_session(
                &out_dir,
                compression,
                |_| {},
                with_steps,
                out,
                |addr, signaling, out| {
                    let mut ok =
                        run_scenarios(&bot, addr, signaling, &out_dir, transport, None, out)?;
                    report_window(&out_dir.join("spawn.jsonl"), &deferred, out)?;
                    ok &= cloudburst(root, &bot, &out_dir, out)?;
                    if transport == "raknet" {
                        wait_until_empty(addr)?;
                        ok &= blocks(&bot, addr, Placer::Anchor(bases), &out_dir, &states, out)?;
                    }
                    let same = compare_with_bds(root, &format!("bds-{name}"), &out_dir, out)?;
                    Ok(ok && same)
                },
            )?;
        }
    }
    let out_dir = dir("anchor-fallback");
    passed &= anchor_session(
        &out_dir,
        FALLBACK_COMPRESSION,
        |c| {
            c.general.enable_ipv6 = true;
            c.general.public_ip = Some(UNREACHABLE);
        },
        |b| b,
        out,
        |addr, signaling, out| {
            let ok = fallback(&bot, addr, signaling, &out_dir, out)?;
            let same = compare_with_bds(root, "bds-fallback", &out_dir, out)?;
            Ok(ok && same)
        },
    )?;
    passed &= game::anchor_session(root, &bot, &game::SCENARIOS, out)?;
    passed &= api::anchor_session(root, &bot, &api::SCENARIOS, out)?;
    say(out, if passed { "PASS" } else { "FAIL" })?;
    Ok(passed)
}

/// Starts Anchor on a thread of this process with the P6 settings as `settings` changes them and
/// what `setup` adds to the builder, calls `run` with its RakNet and NetherNet signaling addresses
/// and shuts it down.
fn anchor_session<W: Write>(
    out_dir: &Path,
    compression: &str,
    settings: impl FnOnce(&mut anchor::Config),
    setup: impl FnOnce(anchor::ServerBuilder) -> anchor::ServerBuilder,
    out: &mut W,
    run: impl FnOnce(&str, &str, &mut W) -> Result<bool, String>,
) -> Result<bool, String> {
    let data_dir = out_dir.join("data");
    remove_dir(&data_dir)?;
    create_dir(&data_dir)?;
    let mut config = anchor_config();
    LEVEL_NAME.clone_into(&mut config.general.level_name);
    config.general.level_seed = Some(LEVEL_SEED);
    config.developer.compression = match compression {
        "zlib" => anchor::protocol::compression::Compression::Zlib,
        _ => anchor::protocol::compression::Compression::Snappy,
    };
    settings(&mut config);
    say(
        out,
        &format!("{}: starting Anchor ({compression})", name(out_dir)),
    )?;
    let server = setup(anchor::Server::builder().config(config).data_dir(&data_dir))
        .without_console_log()
        .build()
        .map_err(|e| format!("build Anchor: {e}"))?;
    run_anchor(server, |ports| {
        say(
            out,
            &format!(
                "{}: Anchor on port {}, NetherNet signaling TCP {}",
                name(out_dir),
                ports.raknet,
                ports.signaling
            ),
        )?;
        let (addr, signaling) = (
            format!("127.0.0.1:{}", ports.raknet),
            format!("127.0.0.1:{}", ports.signaling),
        );
        run(&addr, &signaling, out)
    })
}

/// The settings every Anchor of the harness starts with: offline, IPv4 only, and every port
/// picked by the system when the server binds it (port 0), so no other process can take one
/// first; [`run_anchor`] reads the ports the server opened from its log.
fn anchor_config() -> anchor::Config {
    let mut config = anchor::Config::default();
    config.general.xbox_auth = false;
    config.general.enable_ipv6 = false;
    config.general.level_type = anchor::config::LevelType::Flat;
    config.general.server_port = 0;
    config
}

/// The ports an Anchor server of this process opened.
#[derive(Debug, Clone, Copy)]
struct Ports {
    /// The RakNet and server list ping port (UDP).
    raknet: u16,
    /// The NetherNet signaling port (TCP).
    signaling: u16,
}

/// Runs `server` (built from [`anchor_config`] with `without_console_log`) on a thread of this
/// process, calls `run` with the ports it opened once it answers pings, and shuts it down.
fn run_anchor<T>(
    server: anchor::Server,
    run: impl FnOnce(Ports) -> Result<T, String>,
) -> Result<T, String> {
    install_log()?;
    let shutdown = server.shutdown_handle();
    // The harness starts one server at a time: the lines logged from here on are this one's.
    lock(&LISTENED).clear();
    let thread = spawn(move || server.run());
    let result = opened_ports(&thread).and_then(|ports| {
        wait_for_pong(ports.raknet)?;
        run(ports)
    });
    shutdown.shutdown();
    let stopped = thread
        .join()
        .map_err(|_| "the Anchor thread panicked".to_owned())
        .and_then(|run| run.map_err(|e| format!("Anchor stopped with an error: {e}")));
    let value = result?;
    stopped?;
    Ok(value)
}

/// Makes [`AnchorLog`] the global log of this process, once.
fn install_log() -> Result<(), String> {
    LOG.get_or_init(|| {
        tracing::subscriber::set_global_default(AnchorLog)
            .map_err(|e| format!("install the log of Anchor: {e}"))
    })
    .clone()
}

/// Waits until the starting server has logged its RakNet and signaling addresses (one line per
/// opened address, [`AnchorLog`]).
fn opened_ports(thread: &std::thread::JoinHandle<anchor::Result<()>>) -> Result<Ports, String> {
    let deadline = now() + START_TIMEOUT;
    while now() < deadline {
        let listened = lock(&LISTENED);
        let find = |what: &str| {
            listened
                .iter()
                .find(|(message, _)| message.contains(what))
                .map(|(_, addr)| addr.port())
        };
        if let (Some(raknet), Some(signaling)) = (find("pings"), find("signaling")) {
            return Ok(Ports { raknet, signaling });
        }
        drop(listened);
        if thread.is_finished() {
            return Err("Anchor stopped before opening its addresses".to_owned());
        }
        pause();
    }
    Err(format!(
        "Anchor did not open its addresses within {START_TIMEOUT:?}"
    ))
}

/// Locks `mutex`; a thread that panicked while holding it left nothing half done.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: xtask is a development tool"
)]
fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The global log of this process: Anchor runs here without its console log, so its events come
/// here. Each event of level info and above is written on standard error, as the console log
/// would, and the addresses of the `listening` lines go to [`LISTENED`].
struct AnchorLog;

impl tracing::Subscriber for AnchorLog {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.is_event() && *metadata.level() <= tracing::Level::INFO
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::INFO)
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: xtask is a development tool"
    )]
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = LogLine::default();
        event.record(&mut line);
        let metadata = event.metadata();
        let text = format!(
            "{} {}: {}{}\n",
            metadata.level(),
            metadata.target(),
            line.message,
            line.fields
        );
        // Standard error is the log's only way out: a failed write has nowhere to be reported.
        match std::io::stderr().write_all(text.as_bytes()) {
            Ok(()) | Err(_) => {}
        }
        if let Some(addr) = line.addr
            && line.message.starts_with("listening")
        {
            lock(&LISTENED).push((line.message, addr));
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// The message, the `addr` field and every field as ` name=value` of one event.
#[derive(Default)]
struct LogLine {
    message: String,
    addr: Option<SocketAddr>,
    fields: String,
}

impl tracing::field::Visit for LogLine {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
            return;
        }
        let value = format!("{value:?}");
        if field.name() == "addr" {
            self.addr = value.parse().ok();
        }
        for part in [" ", field.name(), "=", &value] {
            self.fields.push_str(part);
        }
    }
}

/// What changes the blocks of the blocks scenario on a server.
enum Placer<'a> {
    /// BDS console commands.
    Bds(&'a mut Bds),
    /// Anchor's scheduled task from [`anchor_steps`], started by sending it the base position.
    Anchor(std::sync::mpsc::SyncSender<[i32; 3]>),
}

/// The channel that hands Anchor's steps task the base position.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the harness hands the task the base position through a channel"
)]
fn channel() -> (
    std::sync::mpsc::SyncSender<[i32; 3]>,
    std::sync::mpsc::Receiver<[i32; 3]>,
) {
    std::sync::mpsc::sync_channel(1)
}

/// The block state a step names: `name` or `name ["key"=value,...]` (BDS `setblock` syntax), with
/// the properties it leaves out at their first value in Anchor's table, as `setblock` takes the
/// default state (connection properties are worked out by `set_block` anyway).
fn step_state(block: &str) -> Result<anchor::block::State, String> {
    use anchor::block::PropertyValue;
    let (name, properties) = match block.split_once(' ') {
        Some((name, rest)) => (name, rest.trim()),
        None => (block, ""),
    };
    let name = format!("minecraft:{name}");
    let mut wanted = Vec::new();
    let inner = properties.trim_start_matches('[').trim_end_matches(']');
    for pair in inner.split(',').filter(|p| !p.trim().is_empty()) {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("bad property {pair:?} in {block:?}"))?;
        let key = key.trim().trim_matches('"').to_owned();
        let value = value.trim();
        wanted.push((
            key,
            value.trim_matches('"').to_owned(),
            value.starts_with('"'),
        ));
    }
    anchor::block::State::all()
        .find(|s| {
            s.name() == name
                && wanted.iter().all(|(key, value, text)| {
                    s.properties().any(|p| {
                        p.name() == key
                            && match p.value() {
                                PropertyValue::String(v) => *text && v == value,
                                PropertyValue::Int(v) => !*text && v.to_string() == *value,
                                PropertyValue::Byte(v) => !*text && v.to_string() == *value,
                            }
                    })
                })
        })
        .ok_or_else(|| format!("no state of Anchor's table matches {block:?}"))
}

/// Anchor's side of the blocks scenario: a task for `ServerBuilder::every(STEP_TICKS, ..)` that
/// waits for the base position, turns random ticks off, then on each call sets the next step's
/// block or the step's marker (as `blocks` does with BDS commands), and finally the end marker.
/// The harness sends the base position once the bot has spawned; the channel is the harness's,
/// outside the engine.
fn anchor_steps(
    bases: std::sync::mpsc::Receiver<[i32; 3]>,
) -> Result<impl FnMut(&mut anchor::Context<'_>) + Send + 'static, String> {
    let air = anchor::block::State::find("minecraft:air", &[])
        .ok_or_else(|| "no air in Anchor's table".to_owned())?;
    let mut steps = Vec::with_capacity(BLOCK_STEPS.len());
    for &(dx, dy, dz, block) in BLOCK_STEPS {
        let state = if block == "air" {
            air
        } else {
            step_state(block)?
        };
        steps.push(([dx, dy, dz], state));
    }
    let [even, odd] = STEP_MARKERS.map(step_state);
    let (even_marker, odd_marker) = (even?, odd?);
    let end = step_state(END_MARKER)?;
    let mut base: Option<[i32; 3]> = None;
    // Even actions place a step's block, odd ones its marker; past the steps, the end marker.
    let mut action = 0_usize;
    let task = move |ctx: &mut anchor::Context<'_>| {
        if base.is_none() {
            // Random ticks go off before the first step, as on BDS (`blocks`). Speed 0 is within
            // the limit; were it refused, no step would run and the bot would fail.
            base = bases
                .try_recv()
                .ok()
                .filter(|_| ctx.set_random_tick_speed(0));
        }
        let Some([bx, by, bz]) = base else {
            return;
        };
        let at = |[dx, dy, dz]: [i32; 3]| anchor::world::BlockPos::new(bx + dx, by + dy, bz + dz);
        let (step, marker) = (action / 2, action % 2 == 1);
        let change = match steps.get(step) {
            Some(&(offset, state)) if !marker => Some((at(offset), state)),
            Some(_) => {
                let state = if step % 2 == 0 {
                    even_marker
                } else {
                    odd_marker
                };
                Some((at([MARKER.0, 0, MARKER.1]), state))
            }
            None if step == steps.len() && !marker => Some((at([MARKER.0, 0, MARKER.1]), end)),
            None => None,
        };
        // A change that fails (its chunk is not loaded yet) is tried again on the next call.
        if let Some((pos, state)) = change
            && ctx.default_world_mut().set_block(pos, state).is_ok()
        {
            action += 1;
        }
    };
    Ok(task)
}

/// Runs the blocks scenario into `<dir>/blocks.jsonl`: once the bot spawned, turns random ticks
/// off (with them on, a placed block can change on its own between steps, as copper bars oxidize,
/// so a step's states would differ between runs and servers: `xtask/data/p6/facts.md`), changes
/// each block of `BLOCK_STEPS` and ends the step by setting the marker (`MARKER`, `STEP_MARKERS`), then places the end marker: on BDS with console commands,
/// each sent once BDS ran the one before, on Anchor with the task of [`anchor_steps`]. Reports
/// the block changes of each step with their states (also into `<dir>/blocks.txt`). It passes if
/// the bot passed, every step ended and every step changed a block; the comparison with BDS is
/// `compare_with_bds`.
fn blocks(
    bot: &Path,
    addr: &str,
    changer: Placer<'_>,
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
        let base = blocks_base(&read_text(&capture)?)?;
        // Give the server a moment to finish the join before the first change.
        for _ in 0..5 {
            pause();
        }
        let server = match changer {
            Placer::Bds(server) => server,
            Placer::Anchor(bases) => {
                return bases
                    .send(base)
                    .map_err(|_| "the steps task is gone".to_owned());
            }
        };
        server.set_game_rule("randomtickspeed", "0")?;
        let [mx, my, mz] = [base[0] + MARKER.0, base[1], base[2] + MARKER.1];
        for (n, (dx, dy, dz, block)) in BLOCK_STEPS.iter().enumerate() {
            let (bx, by, bz) = (base[0] + dx, base[1] + dy, base[2] + dz);
            let mode = if *block == "air" { "replace" } else { "keep" };
            server.command_done(
                &format!("setblock {bx} {by} {bz} {block} {mode}"),
                BLOCK_PLACED,
            )?;
            let [even, odd] = STEP_MARKERS;
            let marker = if n % 2 == 0 { even } else { odd };
            server.command_done(
                &format!("setblock {mx} {my} {mz} {marker} replace"),
                BLOCK_PLACED,
            )?;
        }
        server.command_done(
            &format!("setblock {mx} {my} {mz} {END_MARKER} replace"),
            BLOCK_PLACED,
        )?;
        Ok(())
    });
    if placed.is_err() {
        child.kill().map_err(|e| format!("kill the bot: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("wait for the bot: {e}"))?;
    placed?;
    let passed = output.status.success();
    let failure = if passed {
        String::new()
    } else {
        format!(": {}", String::from_utf8_lossy(&output.stderr).trim())
    };
    let (report, complete) = blocks_report(&read_text(&capture)?, states)?;
    let text = format!(
        "{} raknet blocks{failure}\n{report}",
        if passed { "ok  " } else { "FAIL" }
    );
    write(&dir.join("blocks.txt"), format!("{text}\n").as_bytes())?;
    say(out, &text)?;
    Ok(passed && complete)
}

/// The base position of the blocks scenario in a capture: `BLOCKS_OFFSET` from the spawn column
/// at `BLOCKS_Y`.
fn blocks_base(capture: &str) -> Result<[i32; 3], String> {
    let [x, z] = spawn_column(capture)?;
    Ok([x + BLOCKS_OFFSET, BLOCKS_Y, z + BLOCKS_OFFSET])
}

/// The marker of the blocks scenario with the reference list's state names.
fn markers() -> compare::Markers {
    let state = |name: &str| state_text(&format!("minecraft:{name}"), "");
    let mut near = std::collections::BTreeSet::new();
    for (dx, dy, dz, _) in BLOCK_STEPS {
        for (x, z) in [-1, 0, 1]
            .into_iter()
            .flat_map(|x| [-1, 0, 1].map(|z| (x, z)))
        {
            near.insert([dx + x, *dy, dz + z].map(i64::from));
        }
    }
    compare::Markers {
        position: [i64::from(MARKER.0), 0, i64::from(MARKER.1)],
        steps: STEP_MARKERS.map(state),
        end: state(END_MARKER),
        near,
    }
}

/// The label of each step: `setblock (dx,dy,dz) <block>`.
fn step_labels() -> Vec<String> {
    BLOCK_STEPS
        .iter()
        .map(|(dx, dy, dz, block)| format!("setblock ({dx},{dy},{dz}) {block}"))
        .collect()
}

/// The steps of a blocks capture, relative to its base position.
fn capture_steps(
    capture: &str,
    states: &BTreeMap<u32, String>,
) -> Result<(Vec<compare::Step>, bool), String> {
    let base = blocks_base(capture)?.map(i64::from);
    compare::block_steps(capture, base, &markers(), states)
}

/// The block changes of each step of a blocks capture as text, and whether the capture is complete:
/// every step ended with its marker, the end marker came and every step changed a block.
fn blocks_report(capture: &str, states: &BTreeMap<u32, String>) -> Result<(String, bool), String> {
    let (steps, ended) = capture_steps(capture, states)?;
    let mut report = Vec::new();
    let mut complete = ended && steps.len() == BLOCK_STEPS.len();
    for (n, label) in step_labels().iter().enumerate() {
        report.push(format!("  step {n}: {label}"));
        let Some(step) = steps.get(n) else {
            report.push("    (not reached)".to_owned());
            continue;
        };
        complete &= !step.changes.is_empty();
        if step.changes.is_empty() {
            report.push("    (nothing)".to_owned());
        }
        for c in &step.changes {
            let [dx, dy, dz] = c.position;
            report.push(format!(
                "    #{} {} ({dx},{dy},{dz}) layer {} flags {} {}",
                c.packet, c.kind, c.layer, c.flags, c.state
            ));
        }
        report.extend(step.others.iter().map(|o| format!("    {o}")));
        for c in &step.ignored {
            let [dx, dy, dz] = c.position;
            report.push(format!(
                "    (away from the steps, not compared) #{} {} ({dx},{dy},{dz}) {}",
                c.packet, c.kind, c.state
            ));
        }
    }
    if !complete {
        report.push(format!(
            "FAIL blocks: {} of {} steps ended, end marker {}, or a step changed nothing",
            steps.len(),
            BLOCK_STEPS.len(),
            if ended { "seen" } else { "missing" }
        ));
    }
    Ok((report.join("\n"), complete))
}

/// Compares two blocks captures step by step (`compare::compare_steps`); prints the differences.
fn compare_blocks(
    a: &Path,
    b: &Path,
    states: &BTreeMap<u32, String>,
    out: &mut impl Write,
) -> Result<bool, String> {
    let steps = |path: &Path| {
        capture_steps(&read_text(path)?, states).map_err(|e| format!("{}: {e}", path.display()))
    };
    let ((steps_a, ended_a), (steps_b, ended_b)) = (steps(a)?, steps(b)?);
    let (mut differences, notes) = compare::compare_steps(&steps_a, &steps_b, &step_labels());
    for (name, ended) in [("A", ended_a), ("B", ended_b)] {
        if !ended {
            differences.push(format!("blocks: {name} has no end marker"));
        }
    }
    for note in &notes {
        say(out, &format!("note: {note}"))?;
    }
    for (name, steps) in [("A", &steps_a), ("B", &steps_b)] {
        let ignored: usize = steps.iter().map(|s| s.ignored.len()).sum();
        if ignored > 0 {
            say(
                out,
                &format!("note: blocks: {name} changed {ignored} blocks away from the steps"),
            )?;
        }
    }
    for difference in &differences {
        say(out, difference)?;
    }
    let same = differences.is_empty();
    say(
        out,
        if same {
            "blocks SAME"
        } else {
            "blocks DIFFERENT"
        },
    )?;
    Ok(same)
}

/// The block column (`x`, `z`) of the player's position in the capture's `StartGame`.
fn spawn_column(capture: &str) -> Result<[i32; 2], String> {
    for line in capture.lines() {
        let value: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        if field(&value, "name").as_str() == Some("StartGame") {
            return compare::block_column(field(&value, "fields"));
        }
    }
    Err("the capture has no StartGame".to_owned())
}

/// Reads the reference block list into network ID -> `name[property=value,...]`.
fn block_states(root: &Path) -> Result<BTreeMap<u32, String>, String> {
    let path = root.join(REFERENCE_DIR).join(REFERENCE_BLOCKS);
    let list: Vec<Value> =
        serde_json::from_str(&read_text(&path)?).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut map = BTreeMap::new();
    for entry in &list {
        let id = field(entry, "network_id")
            .as_u64()
            .and_then(|id| u32::try_from(id).ok())
            .ok_or_else(|| format!("{}: bad network_id in {entry}", path.display()))?;
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
        map.insert(id, state_text(name, &properties.join(",")));
    }
    Ok(map)
}

/// The text of a block state: `name[key=value,...]`, the form of [`block_states`].
fn state_text(name: &str, properties: &str) -> String {
    format!("{name}[{properties}]")
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
fn fallback(
    bot: &Path,
    addr: &str,
    signaling: &str,
    dir: &Path,
    out: &mut impl Write,
) -> Result<bool, String> {
    create_dir(dir)?;
    let capture = dir.join("spawn.jsonl");
    let args = [
        "-addr",
        addr,
        "-signaling-addr",
        signaling,
        "-transport",
        "auto",
        "-scenario",
        "spawn",
    ];
    let outcome = run_bot(bot, &args, &capture)?;
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
            return Err(format!("the bot exited ({status}) before spawning"));
        }
        pause();
    }
    Err(format!("the bot did not spawn within {START_TIMEOUT:?}"))
}

/// Prints the signaling facts of a NetherNet capture: the /v1/join answer, the answer SDP line
/// structure and whether the server sent a handshake.
fn report_nethernet(capture: &Path, out: &mut impl Write) -> Result<(), String> {
    let Some(text) = readable_capture(capture, out)? else {
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
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    socket
        .set_read_timeout(Some(POLL))
        .map_err(|e| e.to_string())?;
    let mut ping = vec![0x01];
    ping.extend_from_slice(&0_i64.to_be_bytes());
    ping.extend_from_slice(&RAKNET_MAGIC);
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
                buf.get(PONG_TEXT_OFFSET..n).unwrap_or_default(),
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
    let passed = run_scenarios(&bot, addr, addr, &out_dir, transport, online, out)?;
    report_window(&out_dir.join("spawn.jsonl"), &load_deferred(root)?, out)?;
    say(out, if passed { "PASS" } else { "FAIL" })?;
    Ok(passed)
}

/// `p6 compare`: compares the spawn windows of two captures, their answer SDP structures when
/// either has one, their full item packets when both have them, their chunk packets (only shown
/// while the deferral table holds one of `compare::CHUNK_PACKETS`) and the blocks captures next
/// to them when both exist; prints the notes.
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
    // The chunk packets of the whole captures; while the deferral table holds them, their
    // differences are only shown.
    let chunks = |path: &Path, text: &str| {
        compare::chunks(text).map_err(|e| format!("{}: {e}", path.display()))
    };
    let (chunk_differences, chunk_notes) =
        compare::compare_chunks(&chunks(a, &text_a)?, &chunks(b, &text_b)?);
    for note in &chunk_notes {
        say(out, &format!("note: {note}"))?;
    }
    if compare::CHUNK_PACKETS.iter().any(|p| deferred.contains(p)) {
        for difference in &chunk_differences {
            say(out, &format!("deferred chunk difference: {difference}"))?;
        }
    } else {
        differences.extend(chunk_differences);
    }
    for difference in &differences {
        say(out, difference)?;
    }
    let mut same = differences.is_empty();
    say(out, if same { "SAME" } else { "DIFFERENT" })?;
    // The blocks captures next to both spawn captures (`p6 bds` writes them for RakNet runs).
    let (blocks_a, blocks_b) = (
        a.with_file_name("blocks.jsonl"),
        b.with_file_name("blocks.jsonl"),
    );
    if is_file(&blocks_a) && is_file(&blocks_b) {
        let states = block_states(root)?;
        same &= compare_blocks(&blocks_a, &blocks_b, &states, out)?;
    }
    Ok(same)
}

/// The bot's full item packet file of a capture: `x.jsonl` -> `x.items.jsonl`.
fn items_path(capture: &Path) -> PathBuf {
    capture.with_extension("items.jsonl")
}

fn load_deferred(root: &Path) -> Result<compare::Deferred, String> {
    compare::parse_deferred(&read_text(&root.join(compare::DEFERRED_PATH))?)
}

/// The text of a capture to report, or `None` after a note saying why it cannot be read: a bot
/// that failed early leaves no capture, and its failure is reported by the run.
fn readable_capture(capture: &Path, out: &mut impl Write) -> Result<Option<String>, String> {
    match read_text(capture) {
        Ok(text) => Ok(Some(text)),
        Err(e) => {
            say(out, &format!("no capture to report: {e}"))?;
            Ok(None)
        }
    }
}

/// Prints the spawn window of a capture without deferred packets.
fn report_window(
    capture: &Path,
    deferred: &compare::Deferred,
    out: &mut impl Write,
) -> Result<(), String> {
    let Some(text) = readable_capture(capture, out)? else {
        return Ok(());
    };
    let window = match compare::window(&text) {
        Ok(window) => window,
        Err(e) => {
            return say(
                out,
                &format!("no spawn window in {}: {e}", capture.display()),
            );
        }
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
    let go_dir = root.join("xtask").join("go");
    // The bot reads packets with Anchor's gophertunnel corrections (`xtask/go/fix`).
    let fixmod = dir.join("fixmod");
    let output = Command::new("go")
        .args(["run", "./fixmod", "-out"])
        .arg(&fixmod)
        .current_dir(&go_dir)
        .output()
        .map_err(|e| format!("cannot run `go` ({e}); install Go and put it on PATH"))?;
    if !output.status.success() {
        return Err(format!(
            "go run ./fixmod failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let output = Command::new("go")
        .arg("build")
        .arg("-modfile")
        .arg(fixmod.join("go.mod"))
        .arg("-o")
        .arg(&exe)
        .arg("./bot")
        .current_dir(&go_dir)
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

/// Runs each scenario over the transport into `<out_dir>/<scenario>.jsonl` against the RakNet
/// address `addr` and the NetherNet signaling address `signaling`; returns whether all passed.
fn run_scenarios(
    bot: &Path,
    addr: &str,
    signaling: &str,
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
            "-signaling-addr",
            signaling,
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
    let Some(text) = readable_capture(capture, out)? else {
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
            // The Linux BDS loads the libraries in its folder, as its how-to starts it
            // (`LD_LIBRARY_PATH=. ./bedrock_server`); Windows ignores the variable.
            .env("LD_LIBRARY_PATH", dir)
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

    /// Sends one console command and waits until BDS logs `done` once more, so the next command
    /// runs in a later tick. A pause between commands does not ensure that: BDS sometimes runs
    /// commands that came a few hundred milliseconds apart in one tick, and then sends their block
    /// changes grouped by sub-chunk, not in command order.
    fn command_done(&mut self, line: &str, done: &str) -> Result<(), String> {
        let log = self.log.clone();
        let logged = || read_text(&log).map(|text| text.matches(done).count());
        let before = logged()?;
        self.command(line)?;
        let deadline = now() + COMMAND_TIMEOUT;
        while logged()? == before {
            if now() > deadline {
                return Err(format!(
                    "BDS did not log {done:?} for `{line}` within {COMMAND_TIMEOUT:?}; see {}",
                    self.log.display()
                ));
            }
            pause();
        }
        Ok(())
    }

    /// Sets the game rule and waits until BDS confirms it.
    fn set_game_rule(&mut self, rule: &str, value: &str) -> Result<(), String> {
        self.command_done(
            &format!("gamerule {rule} {value}"),
            &format!("Game rule {rule} has been updated to {value}"),
        )
    }

    /// Sends `stop` and waits for the exit; kills BDS, and says why, if `stop` could not be sent
    /// or did not end it in time.
    fn stop(mut self) -> Result<(), String> {
        let sent = self
            .stdin
            .take()
            .map(|mut stdin| stdin.write_all(b"stop\n").and_then(|()| stdin.flush()));
        let reason = match sent {
            Some(Ok(())) => {
                let deadline = now() + STOP_TIMEOUT;
                loop {
                    if self.child.try_wait().map_err(|e| e.to_string())?.is_some() {
                        return Ok(());
                    }
                    if now() >= deadline {
                        break format!("BDS did not stop on `stop` within {STOP_TIMEOUT:?}");
                    }
                    pause();
                }
            }
            Some(Err(e)) => format!("BDS console: {e}"),
            None => "BDS has no console input".to_owned(),
        };
        if let Some(status) = self.child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!(
                "{reason}; BDS had exited ({status}); see {}",
                self.log.display()
            ));
        }
        self.child.kill().map_err(|e| format!("kill BDS: {e}"))?;
        self.child
            .wait()
            .map_err(|e| format!("wait for BDS: {e}"))?;
        Err(format!("{reason}; killed it"))
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

/// Two distinct UDP ports that are free on all IPv4 interfaces right now, for BDS, which takes
/// its ports from `server.properties` (Anchor binds port 0 instead, [`anchor_config`]). Another
/// process may take one before BDS binds it; BDS then cannot bind it, says so in its log and never
/// logs "Server started.", so [`Bds::wait_started`] fails the run instead of a bot meeting another
/// server.
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
fn read_text(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The text of a file the bot is still writing: empty while the file does not exist yet.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn read_growing_text(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        text => text.map_err(|e| format!("{}: {e}", path.display())),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_anchor_session_runs_on_the_ports_the_server_logged() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(OUT_DIR)
            .join("test-session");
        let mut out = Vec::new();
        let passed = anchor_session(
            &dir,
            "zlib",
            |_| {},
            |b| b,
            &mut out,
            |addr, _, _| {
                wait_until_empty(addr)?;
                Ok(true)
            },
        )
        .expect("the session runs");
        assert!(passed);
    }
}
