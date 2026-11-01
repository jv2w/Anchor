//! Forced shutdown: a server that keeps moving one block and saving is
//! killed a random number of ticks after its first commit (counted from its reports, never in
//! wall-clock time), during a later commit or between two, again and again; every restart must
//! find undamaged data files whose main file's last save is newer than the last restart's, and the
//! block exactly once (it lives in one world file, whose commits are whole) where the tick of the
//! world file's last save put it: that save's or a later one's, since a kill between the world
//! file's commit and the main file's leaves the world file one save ahead. A restart on a world
//! file a save ahead logs the error naming it, and no other restart logs one.
//!
//! The test runs its own test binary as the server process (`crash_child`) and kills only that
//! child, through its process handle (SIGKILL on Unix).

#![cfg(test)]

use std::io::{BufRead, BufReader, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anchor::block::State;
use anchor::event::WorldSave;
use anchor::world::{BlockPos, ChunkPos, World};
use anchor::{Config, Context, Server};

/// The child's environment variable naming its data folder.
const DATA_ENV: &str = "ANCHOR_CRASH_DATA";
/// The child's environment variable: `run` keeps moving the block, `check` only reports.
const MODE_ENV: &str = "ANCHOR_CRASH_MODE";
/// The prefix of the child's report lines.
const REPORT: &str = "ANCHOR-CRASH";
/// Kills, each followed by a restart that checks the file.
const KILLS: u32 = 50;
/// The block's path stays in the spawn chunks (3 × 3 around chunk 0, 0), always loaded.
const SPAN: i32 = 48;
/// The child gives up after this many ticks, so a lost parent leaves no process behind.
const CHILD_TICKS: u64 = 1200;
/// The start of the error a start logs for a data file holding a later save than the main file.
const LATER_SAVE: &str = "data files disagree";
/// The console log line that follows the start's file checks.
const STARTED: &str = "server started";

/// Where the block is after the task of tick `tick` moved it: a walk over the spawn chunks that
/// changes chunk often, so commits hold different chunks together.
fn path(tick: u64) -> BlockPos {
    let t = i32::try_from(tick % 100_000).unwrap();
    BlockPos::new((t * 7) % SPAN - 16, -60, (t * 11) % SPAN - 16)
}

fn gold() -> State {
    State::find("minecraft:gold_block", &[]).unwrap()
}

/// Settings that run without the network beyond loopback. Port 0 lets the system pick every port
/// at bind time: nothing connects, and a port picked earlier and released could be taken by
/// another process first.
fn local_config() -> Config {
    let mut config = Config::default();
    config.general.xbox_auth = false;
    config.general.enable_ipv6 = false;
    config.general.server_port = 0;
    config.developer.console_input = false;
    config
}

/// Every gold block on the block's path, by scanning the spawn chunks at y -60.
fn golds(world: &World) -> Option<Vec<BlockPos>> {
    let mut found = Vec::new();
    for x in -16..SPAN - 16 {
        for z in -16..SPAN - 16 {
            let at = BlockPos::new(x, -60, z);
            if world.block(at)? == gold() {
                found.push(at);
            }
        }
    }
    Some(found)
}

/// The child's task state.
struct Mover {
    /// The tick the server started at: its last commit's.
    start: Option<u64>,
    /// Whether the world was checked and reported.
    checked: bool,
    /// Where the block is.
    at: Option<BlockPos>,
    moves: bool,
}

impl Mover {
    #[expect(
        clippy::print_stdout,
        clippy::disallowed_methods,
        reason = "consistency: the child reports to the parent on standard output"
    )]
    fn report(line: &str) {
        println!("{REPORT} {line}");
        std::io::stdout().flush().unwrap();
    }

    fn tick(&mut self, ctx: &mut Context<'_>, stop: &anchor::ShutdownHandle) {
        let start = *self.start.get_or_insert(ctx.tick());
        if ctx.tick() > start + CHILD_TICKS {
            stop.shutdown();
            return;
        }
        if !self.checked {
            // Wait until every spawn chunk is loaded, then report what the commit holds.
            let world = ctx.default_world();
            let loaded =
                (-1..=1).all(|x| (-1..=1).all(|z| world.is_chunk_loaded(ChunkPos::new(x, z))));
            if !loaded {
                return;
            }
            let Some(found) = golds(world) else {
                return;
            };
            let list: Vec<String> = found.iter().map(|p| format!("{},{}", p.x, p.z)).collect();
            Self::report(&format!("start={start} golds={}", list.join(";")));
            self.at = found.first().copied();
            self.checked = true;
            if !self.moves {
                stop.shutdown();
            }
            return;
        }
        if !self.moves {
            return;
        }
        // One tick moves the block: both writes land in the same commit or neither does.
        let next = path(ctx.tick());
        let world = ctx.default_world_mut();
        if let Some(old) = self.at {
            world
                .set_block(old, State::find("minecraft:air", &[]).unwrap())
                .unwrap();
        }
        world.set_block(next, gold()).unwrap();
        self.at = Some(next);
        Self::report(&format!("tick={}", ctx.tick()));
        if ctx.tick().is_multiple_of(3) {
            ctx.save();
        }
    }
}

/// The server process the parent test starts and kills. Without the parent's environment it
/// does nothing.
#[test]
#[ignore = "the child process of forced_kills_never_damage_duplicate_or_lose_the_block"]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the task gets the server's shutdown handle once, after the build"
)]
fn crash_child() {
    let (Ok(data), Ok(mode)) = (std::env::var(DATA_ENV), std::env::var(MODE_ENV)) else {
        return;
    };
    let server = Server::builder().config(local_config()).data_dir(&data);
    let (handle_tx, handle_rx) = std::sync::mpsc::sync_channel(1);
    let mut mover = Mover {
        start: None,
        checked: false,
        at: None,
        moves: mode == "run",
    };
    let mut stop: Option<anchor::ShutdownHandle> = None;
    let server = server
        .every(NonZeroU32::MIN, move |ctx| {
            let stop = stop.get_or_insert_with(|| handle_rx.recv().unwrap());
            mover.tick(ctx, stop);
        })
        // The parent times its kill from the commits.
        .monitor(|_: &WorldSave, _| Mover::report("committed"))
        .build()
        .unwrap();
    handle_tx.send(server.shutdown_handle()).unwrap();
    server.run().unwrap();
}

/// A started child, its report lines (the text after the prefix) and its console log lines.
struct Running {
    child: Child,
    lines: mpsc::Receiver<String>,
    log: mpsc::Receiver<String>,
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test starts and reads its child process"
)]
fn start(data: &Path, mode: &str) -> Running {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "crash_child",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(DATA_ENV, data)
        .env(MODE_ENV, mode)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, log) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            // libtest writes the test's name first on the same line.
            if let Some(rest) = line
                .find(REPORT)
                .and_then(|at| line.get(at + REPORT.len()..))
                && tx.send(rest.trim().to_owned()).is_err()
            {
                break;
            }
        }
    });
    Running { child, lines, log }
}

/// The errors the child's start logged about data files holding a later save than the main
/// file. The log thread writes lines in order, so they are all out once the line that follows the
/// start's checks is.
fn later_saves(server: &mut Running) -> Vec<String> {
    let mut found = Vec::new();
    loop {
        let line = server
            .log
            .recv_timeout(Duration::from_secs(120))
            .unwrap_or_else(|_| {
                let status = server.child.try_wait().unwrap();
                panic!("the child did not log its start (exit {status:?})")
            });
        if line.contains(STARTED) {
            return found;
        }
        if line.contains(LATER_SAVE) {
            found.push(line);
        }
    }
}

/// The child's first report: the tick of the commit it started from and the gold blocks found.
fn report(server: &mut Running) -> (u64, Vec<(i32, i32)>) {
    let line = server
        .lines
        .recv_timeout(Duration::from_secs(120))
        .unwrap_or_else(|_| {
            let status = server.child.try_wait().unwrap();
            panic!("no report from the child (exit {status:?}): a damaged file is refused at start")
        });
    let mut start = None;
    let mut golds = Vec::new();
    for field in line.split_whitespace() {
        if let Some(t) = field.strip_prefix("start=") {
            start = Some(t.parse().unwrap());
        } else if let Some(list) = field.strip_prefix("golds=") {
            for p in list.split(';').filter(|p| !p.is_empty()) {
                let (x, z) = p.split_once(',').unwrap();
                golds.push((x.parse().unwrap(), z.parse().unwrap()));
            }
        }
    }
    (start.unwrap(), golds)
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test prepares its data folder"
)]
fn fresh(dir: &Path) {
    std::fs::remove_dir_all(dir).unwrap_or(());
    std::fs::create_dir_all(dir).unwrap();
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test removes its data folder"
)]
fn remove(dir: &Path) {
    std::fs::remove_dir_all(dir).unwrap();
}

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("crash-{}", std::process::id()))
}

#[test]
#[expect(
    clippy::print_stdout,
    reason = "consistency: the test tells how often a kill split a save"
)]
fn forced_kills_never_damage_duplicate_or_lose_the_block() {
    let data = data_dir();
    fresh(&data);
    // A fixed-seed generator for the kill moments (xorshift).
    let mut seed = 0x5eed_c0de_u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut placed = false;
    let mut checked = 0;
    // Runs whose kill came after at least one commit: the start tick moved on.
    let (mut last_start, mut advanced) = (0, 0);
    // Restarts whose world file held a later save than the main file.
    let mut ahead = 0;
    for kill in 0..=KILLS {
        let mode = if kill == KILLS { "check" } else { "run" };
        let mut server = start(&data, mode);
        let (start, golds) = report(&mut server);
        let later_saves = later_saves(&mut server);
        // Exactly one block once one was committed (no duplicate, no loss), at the place the
        // tick of the world file's last save put it: the main file's save, or one after it.
        let world_ahead = match golds[..] {
            [] => {
                assert!(!placed, "kill {kill}: the block was lost (start {start})");
                false
            }
            [(x, z)] => {
                let at = |tick: u64| {
                    let p = path(tick - 1);
                    (p.x, p.z)
                };
                let world_ahead = at(start) != (x, z);
                if world_ahead {
                    assert!(
                        (start..start + CHILD_TICKS).any(|t| at(t) == (x, z)),
                        "kill {kill}: start {start}, block at {x} {z}"
                    );
                }
                placed = true;
                world_ahead
            }
            _ => panic!("kill {kill}: the block was duplicated: {golds:?} (start {start})"),
        };
        // Only the world file changes, so only it can hold a later save, and the start says so.
        if world_ahead {
            ahead += 1;
            let main_tick = format!("main_save_tick={start}");
            assert!(
                matches!(&later_saves[..], [line] if line.contains("world.redb") && line.contains(&main_tick)),
                "kill {kill}: the world file a save ahead was not reported (start {start}): \
                 {later_saves:?}"
            );
        } else {
            assert!(
                later_saves.is_empty(),
                "kill {kill}: files of one save reported as later: {later_saves:?}"
            );
        }
        checked += 1;
        if start > last_start {
            advanced += 1;
            last_start = start;
        }
        if mode == "run" {
            // The kill lands a random number of ticks after the run's first commit, counted by
            // the child's reports, however slow the machine is.
            let after = next() % 30;
            let mut since_commit = None;
            while since_commit.is_none_or(|ticks| ticks < after) {
                let line = server
                    .lines
                    .recv_timeout(Duration::from_secs(120))
                    .unwrap_or_else(|_| panic!("kill {kill}: the child stopped reporting"));
                if line == "committed" {
                    since_commit.get_or_insert(0);
                } else if let Some(ticks) = &mut since_commit
                    && line.starts_with("tick=")
                {
                    *ticks += 1;
                }
            }
            server.child.kill().unwrap();
        }
        server.child.wait().unwrap();
    }
    assert_eq!(checked, KILLS + 1);
    assert_eq!(advanced, KILLS, "every restart finds a newer commit");
    println!("restarts with the world file a save ahead: {ahead} of {KILLS}");
    remove(&data);
}
