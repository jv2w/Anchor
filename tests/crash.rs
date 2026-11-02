//! Forced shutdown: a server that keeps moving one block and saving is
//! killed a random number of ticks after its first commit (counted from its reports, never in
//! wall-clock time), during a later commit or between two, again and again; every restart must
//! find undamaged data files whose main file's last save is newer than the last restart's, and the
//! block exactly once where the tick of the main file's last save put it: a kill between the world
//! file's commit and the main file's leaves the world file a save ahead, which the restart rolls
//! back to the main file's save and logs, naming the world file. The server also makes, unloads
//! and deletes a second world over and over, moving its spawn every tick it is loaded, so kills
//! land while a new world is saved and while a deleted one goes: every restart finds that world
//! exactly as the main file's last save had it, or not at all if that save had none. Every other
//! kill is followed by a start killed after a random number of its first log lines, during the
//! roll back of the files the kill left, before the restart that checks.
//!
//! The test runs its own test binary as the server process (`crash_child`) and kills only that
//! child, through its process handle (SIGKILL on Unix).

#![cfg(test)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anchor::block::State;
use anchor::event::WorldSave;
use anchor::world::{self, BlockPos, ChunkPos, Options, World};
use anchor::{Config, Context, Server};

/// The child gives up after this many ticks, so a lost parent leaves no process behind.
const CHILD_TICKS: u64 = 1200;
/// Ticks of one cycle of the second world: made at 0, unloaded at 8, deleted at 10.
const CYCLE: u64 = 12;
/// The child's environment variable naming its data folder.
const DATA_ENV: &str = "ANCHOR_CRASH_DATA";
/// The second world, which the server makes, unloads and deletes over and over.
const EXTRA: &str = "extra";
/// Kills, each followed by a restart that checks the file.
const KILLS: u32 = 50;
/// The child's environment variable: `run` keeps moving the block and cycling the second world,
/// `check` only reports, `idle` only starts and waits to be killed.
const MODE_ENV: &str = "ANCHOR_CRASH_MODE";
/// The prefix of the child's report lines.
const REPORT: &str = "ANCHOR-CRASH";
/// The start of the warning a start logs for a data file it rolled back to the main file's save.
const ROLLED_BACK: &str = "data file rolled back";
/// The block's path stays in the spawn chunks (3 × 3 around chunk 0, 0), always loaded.
const SPAN: i32 = 48;
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
    /// Whether the child only starts, to be killed during its start.
    idle: bool,
    /// The second world while it exists, loaded or not.
    extra: Option<world::Id>,
    /// The x of the second world's spawn while it exists: the last tick that moved it, modulo 16.
    extra_x: Option<i32>,
}

/// A report's text for the second world: its spawn's x, or `-` while it does not exist.
fn extra_text(x: Option<i32>) -> String {
    x.map_or("-".to_owned(), |x| x.to_string())
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

    fn tick(&mut self, cx: &mut Context<'_>) {
        let start = *self.start.get_or_insert(cx.tick());
        if cx.tick() > start + CHILD_TICKS {
            cx.shutdown();
            return;
        }
        if self.idle {
            // Only the start counts; the parent kills the child during it or soon after.
            return;
        }
        if !self.checked {
            // Wait until every spawn chunk is loaded, then report what the commit holds.
            let world = cx.default_world();
            let loaded =
                (-1..=1).all(|x| (-1..=1).all(|z| world.is_chunk_loaded(ChunkPos::new(x, z))));
            if !loaded {
                return;
            }
            let Some(found) = golds(world) else {
                return;
            };
            let list: Vec<String> = found.iter().map(|p| format!("{},{}", p.x, p.z)).collect();
            if cx.is_world_generated(EXTRA) {
                let id = cx.load_world(EXTRA, &Options::new()).unwrap();
                self.extra = Some(id);
                self.extra_x = cx.world(id).map(|w| w.spawn().x);
            }
            Self::report(&format!(
                "start={start} golds={} extra={}",
                list.join(";"),
                extra_text(self.extra_x)
            ));
            self.at = found.first().copied();
            self.checked = true;
            if !self.moves {
                cx.shutdown();
            }
            return;
        }
        if !self.moves {
            return;
        }
        self.cycle(cx);
        // One tick moves the block: both writes land in the same commit or neither does.
        let next = path(cx.tick());
        let world = cx.default_world_mut();
        if let Some(old) = self.at {
            world
                .set_block(old, State::find("minecraft:air", &[]).unwrap())
                .unwrap();
        }
        world.set_block(next, gold()).unwrap();
        self.at = Some(next);
        Self::report(&format!(
            "tick={} extra={}",
            cx.tick(),
            extra_text(self.extra_x)
        ));
        if cx.tick().is_multiple_of(3) {
            cx.save();
        }
    }

    /// Makes the second world at the start of a cycle, unloads it and deletes it later in the
    /// cycle, and moves its spawn every tick it is loaded (inside one chunk, so no load follows).
    fn cycle(&mut self, cx: &mut Context<'_>) {
        let tick = cx.tick();
        match (tick % CYCLE, self.extra) {
            (0, None) => self.extra = Some(cx.create_world(EXTRA, &Options::new()).unwrap()),
            (8, Some(id)) if cx.is_world_loaded(EXTRA) => cx.unload_world(id).unwrap(),
            (10, Some(_)) if !cx.is_world_loaded(EXTRA) => {
                cx.delete_world(EXTRA).unwrap();
                self.extra = None;
                self.extra_x = None;
            }
            _ => {}
        }
        if let Some(id) = self.extra
            && let Some(world) = cx.world_mut(id)
        {
            let x = i32::try_from(tick % 16).unwrap();
            world.set_spawn(BlockPos::new(x, -60, 0)).unwrap();
            self.extra_x = Some(x);
        }
    }
}

/// The server process the parent test starts and kills. Without the parent's environment it
/// does nothing.
#[test]
#[ignore = "the child process of forced_kills_never_damage_duplicate_or_lose_the_block"]
fn crash_child() {
    let (Ok(data), Ok(mode)) = (std::env::var(DATA_ENV), std::env::var(MODE_ENV)) else {
        return;
    };
    let mut mover = Mover {
        start: None,
        checked: false,
        at: None,
        moves: mode == "run",
        idle: mode == "idle",
        extra: None,
        extra_x: None,
    };
    Server::builder()
        .config(local_config())
        .data_dir(&data)
        .every(NonZeroU32::MIN, move |cx| mover.tick(cx))
        // The parent times its kill from the commits.
        .monitor(|_: &WorldSave, _| Mover::report("committed"))
        .build()
        .unwrap()
        .run()
        .unwrap();
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

/// The warnings the child's start logged about data files it rolled back. The log thread writes
/// lines in order, so they are all out once the line that follows the start's checks is.
fn roll_backs(server: &mut Running) -> Vec<String> {
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
        if line.contains(ROLLED_BACK) {
            found.push(line);
        }
    }
}

/// Kills a child started `idle` after `lines` lines of its console log, or once it logs that it
/// started: during its start's roll back of the files a kill left, or soon after.
fn kill_during_start(server: &mut Running, lines: u64) {
    for _ in 0..lines {
        let line = server
            .log
            .recv_timeout(Duration::from_secs(120))
            .unwrap_or_else(|_| {
                let status = server.child.try_wait().unwrap();
                panic!("the idle child did not log its start (exit {status:?})")
            });
        if line.contains(STARTED) {
            break;
        }
    }
    server.child.kill().unwrap();
    server.child.wait().unwrap();
}

/// The value of the field `name` of a report line.
fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|f| f.strip_prefix(name)?.strip_prefix('='))
}

/// The child's first report: the tick of the commit it started from, the gold blocks found and
/// the second world's text ([`extra_text`]).
fn report(server: &mut Running) -> (u64, Vec<(i32, i32)>, String) {
    let line = server
        .lines
        .recv_timeout(Duration::from_secs(120))
        .unwrap_or_else(|_| {
            let status = server.child.try_wait().unwrap();
            panic!("no report from the child (exit {status:?}): a damaged file is refused at start")
        });
    let start = field(&line, "start").unwrap().parse().unwrap();
    let golds = field(&line, "golds")
        .unwrap()
        .split(';')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (x, z) = p.split_once(',').unwrap();
            (x.parse().unwrap(), z.parse().unwrap())
        })
        .collect();
    let extra = field(&line, "extra").unwrap().to_owned();
    (start, golds, extra)
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

/// Records the second world's text of a tick line in `extras`.
fn record(line: &str, extras: &mut BTreeMap<u64, String>) {
    if let (Some(tick), Some(extra)) = (field(line, "tick"), field(line, "extra")) {
        extras.insert(tick.parse().unwrap(), extra.to_owned());
    }
}

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("crash-{}", std::process::id()))
}

#[test]
#[expect(
    clippy::print_stdout,
    reason = "consistency: the test tells how often a restart rolled a file back"
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
    // Restarts that rolled a world file back to the main file's save.
    let mut rolled_back = 0;
    // The second world's text after each tick a run reported; a later run's line for a tick
    // replaces an earlier one's, as a restart goes on from the commit.
    let mut extras: BTreeMap<u64, String> = BTreeMap::new();
    for kill in 0..=KILLS {
        let mode = if kill == KILLS { "check" } else { "run" };
        let mut server = start(&data, mode);
        let (start_tick, golds, extra) = report(&mut server);
        let roll_backs = roll_backs(&mut server);
        // The second world as the main file's last save had it: as the task of its tick left it.
        let expected = start_tick
            .checked_sub(1)
            .and_then(|tick| extras.get(&tick).cloned())
            .unwrap_or_else(|| extra_text(None));
        assert_eq!(
            extra, expected,
            "kill {kill}: the second world is not as the main file's save had it (start {start_tick}, \
             roll backs {roll_backs:?})"
        );
        // Exactly one block once one was committed (no duplicate, no loss), at the place the
        // tick of the main file's last save put it.
        match golds[..] {
            [] => assert!(
                !placed,
                "kill {kill}: the block was lost (start {start_tick})"
            ),
            [(x, z)] => {
                let at = path(start_tick - 1);
                assert_eq!(
                    (x, z),
                    (at.x, at.z),
                    "kill {kill}: the block is not where the main file's save put it \
                     (start {start_tick}, roll backs {roll_backs:?})"
                );
                placed = true;
            }
            _ => panic!("kill {kill}: the block was duplicated: {golds:?} (start {start_tick})"),
        }
        // Only the world files change besides the main file, so only they can be rolled back.
        let main_tick = format!("main_save_tick={start_tick}");
        assert!(
            roll_backs.iter().all(|line| {
                (line.contains("world.redb") || line.contains("extra.redb"))
                    && line.contains(&main_tick)
            }),
            "kill {kill}: another file was rolled back (start {start_tick}): {roll_backs:?}"
        );
        rolled_back += roll_backs.len();
        checked += 1;
        if start_tick > last_start {
            advanced += 1;
            last_start = start_tick;
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
                record(&line, &mut extras);
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
        for line in &server.lines {
            record(&line, &mut extras);
        }
        if mode == "run" && kill % 2 == 1 {
            let mut interrupted = start(&data, "idle");
            kill_during_start(&mut interrupted, next() % 8);
        }
    }
    assert_eq!(checked, KILLS + 1);
    assert_eq!(advanced, KILLS, "every restart finds a newer commit");
    println!("restarts that rolled the world file back: {rolled_back} of {KILLS}");
    remove(&data);
}
