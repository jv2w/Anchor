//! Forced shutdown with user data: a server writes the tick into a server data value, a player data
//! value and the terrain every tick, saving every third tick, and is killed a random number of
//! ticks after its first commit, again and again. Every restart must find the three holding one and
//! the same committed tick, newer than the last restart's, with no tick counted twice or lost.
//!
//! The test runs its own test binary as the server process (`user_data_crash_child`) and kills
//! only that child, through its process handle, as the item crash test does
//! (`tick::tests::crash`). The child opens the data files and starts the save thread and the tick
//! thread as [`serve`] does, and one player joins through the tick's input queue. The child
//! process, its server and its report lines are shared with the item crash test.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::num::NonZeroU32;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use super::{Context, registered};
use crate::block::{self, Kind as BlockKind};
use crate::config::Config;
use crate::event::{Listeners, Priority, WorldSave};
use crate::protocol::Uuid;
use crate::save::{Loaded, Response, WorldSpec};
use crate::tick::queue::{
    Client, Identity, InputKind, Outbound, SessionId, Skin, input_queue, outbound_queue,
};
use crate::tick::tests::{join, settings, shared, tick};
use crate::tick::{Output, Phase, SaveLink, Settings, Setup, State, Tasks, run};
use crate::world::{BlockPos, ChunkPos, Flat};

/// The child gives up after this many ticks, so a lost parent leaves no process behind.
pub(in crate::tick) const CHILD_TICKS: u64 = 1200;
/// The child's environment variable naming its data folder.
const DATA_ENV: &str = "ANCHOR_USER_CRASH_DATA";
/// Kills, each followed by a restart that checks the file.
const KILLS: u32 = 20;
/// The height of the marker block, in the spawn chunks' dirt layer.
const MARKER_HEIGHT: i32 = -62;
/// The x and z coordinates the marker block can have.
const MARKER_RANGE: Range<i32> = -16..32;
/// The child's mode that only reports what it started with.
const MODE_CHECK: &str = "check";
/// The child's environment variable naming its mode: [`MODE_RUN`] or [`MODE_CHECK`].
const MODE_ENV: &str = "ANCHOR_USER_CRASH_MODE";
/// The child's mode that keeps writing.
const MODE_RUN: &str = "run";
/// The name of the player the child joins as.
const NAME: &str = "keeper";
/// The prefix of the child's report lines.
const REPORT: &str = "ANCHOR-USER";
/// How long the parent waits for a line of the child's report.
const REPORT_TIMEOUT: Duration = Duration::from_secs(120);
/// The session of the child's player.
pub(in crate::tick) const SESSION: SessionId = SessionId::new(1);

/// What the server and the player keep: the last tick that wrote it, and how many ticks have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
struct Progress {
    tick: u64,
    count: u64,
}

impl Progress {
    fn text(self) -> String {
        format!("{},{}", self.tick, self.count)
    }
}

/// The child's task state.
struct Writer {
    writes: bool,
    stop: mpsc::Sender<()>,
    start: Option<u64>,
    ready: bool,
    marker: Option<BlockPos>,
}

impl Writer {
    fn tick(&mut self, cx: &mut Context<'_>) {
        let tick = cx.tick();
        let start = *self.start.get_or_insert(tick);
        if tick > start + CHILD_TICKS {
            stop_server(&self.stop);
            return;
        }
        if !self.ready {
            self.ready = self.check(cx, start);
            if self.ready && !self.writes {
                stop_server(&self.stop);
            }
            return;
        }
        if !self.writes {
            return;
        }
        let server = cx.data_mut::<Progress>().expect("registered");
        *server = Progress {
            tick,
            count: server.count + 1,
        };
        let server = *server;
        let id = cx.player_by_name(NAME).expect("the player is in");
        let player = cx.player_mut(id).expect("the player is in");
        let mine = player.data_mut::<Progress>().expect("registered");
        *mine = Progress {
            tick,
            count: mine.count + 1,
        };
        let mine = *mine;
        let world = cx.default_world_mut();
        if let Some(old) = self.marker {
            world
                .set_block(old, BlockKind::Dirt.default_state())
                .unwrap();
        }
        world.set_block(path(tick), gold()).unwrap();
        self.marker = Some(path(tick));
        report(
            REPORT,
            &format!(
                "tick={tick} server={} player={}",
                server.text(),
                mine.text()
            ),
        );
        if tick.is_multiple_of(3) {
            cx.save();
        }
    }

    /// Reports what the server started with once the player's data and the spawn chunks, where
    /// the marker [`path`] runs, are in (the save thread may answer them in different ticks);
    /// false while waiting.
    fn check(&mut self, cx: &mut Context<'_>, start: u64) -> bool {
        let state: &State = cx.state;
        let Some(player) = state.players.get(&SESSION) else {
            return false;
        };
        let world = &state.worlds[0];
        let mut spawn_chunks = (-1..=1).flat_map(|x| (-1..=1).map(move |z| ChunkPos::new(x, z)));
        if player.phase == Phase::Loading || !spawn_chunks.all(|pos| world.is_chunk_loaded(pos)) {
            return false;
        }
        let mut golds = Vec::new();
        for x in MARKER_RANGE {
            for z in MARKER_RANGE {
                if world.block(BlockPos::new(x, MARKER_HEIGHT, z)) == Some(gold()) {
                    golds.push(format!("{x},{z}"));
                }
            }
        }
        self.marker = golds.first().and_then(|g| {
            let (x, z) = g.split_once(',')?;
            Some(BlockPos::new(
                x.parse().ok()?,
                MARKER_HEIGHT,
                z.parse().ok()?,
            ))
        });
        let server = cx.data::<Progress>().copied().expect("registered");
        let mine = player.data::<Progress>().copied().expect("registered");
        report(
            REPORT,
            &format!(
                "start={start} golds={} server={} player={}",
                golds.join(";"),
                server.text(),
                mine.text()
            ),
        );
        true
    }
}

/// A started child and its report lines.
pub(in crate::tick) struct Running {
    pub(in crate::tick) child: Child,
    pub(in crate::tick) lines: std::sync::mpsc::Receiver<String>,
}

/// The server process the parent test starts and kills. Without the parent's environment it
/// does nothing.
#[test]
#[ignore = "the child process of forced_kills_keep_user_data_with_its_tick"]
fn user_data_crash_child() {
    let (Ok(data), Ok(mode)) = (std::env::var(DATA_ENV), std::env::var(MODE_ENV)) else {
        return;
    };
    let (stop_tx, stop) = mpsc::channel(1);
    let mut writer = Writer {
        writes: mode == MODE_RUN,
        stop: stop_tx,
        start: None,
        ready: false,
        marker: None,
    };
    let mut tasks = Tasks::default();
    tasks.push(NonZeroU32::MIN, move |cx| writer.tick(cx));
    // The parent times its kill from the commits.
    let mut listeners = Listeners::default();
    listeners.push(
        Priority::Normal,
        |_: &mut WorldSave, cx: &mut Context<'_>| {
            report(REPORT, &format!("committed={}", cx.tick()));
        },
    );
    let setup = Setup {
        listeners,
        tasks,
        data: registered::<Progress, Progress>("progress", "progress"),
        ..Setup::default()
    };
    serve(Path::new(&data), NAME, 4, setup, stop, Vec::new());
}

/// The check of a restart reads the marker block only once the spawn chunks are in, even when the
/// save thread answers them a tick after the player data (a busy save thread on a loaded machine).
#[test]
fn the_check_waits_for_spawn_chunks_answered_after_the_player() {
    let world = crate::world::World::new(
        "world",
        crate::world::Kind::Persistent,
        0,
        crate::world::Generator::spawn(&Flat, 0),
    );
    let mut state = State::new(
        settings(|c| c.general.save_player_data = true),
        shared(),
        Setup {
            data: registered::<Progress, Progress>("progress", "progress"),
            ..Setup::default()
        },
        vec![world],
        0,
    );
    let (stop, _stop_rx) = mpsc::channel(1);
    let mut writer = Writer {
        writes: false,
        stop,
        start: None,
        ready: false,
        marker: None,
    };
    let marker = path(5);
    let mut output = Output::default();
    let (j, _rx) = join(SESSION.get(), 0);
    let mut batch = vec![j];
    let mut late = Vec::new();
    for _ in 0..10 {
        tick(&mut state, &mut batch, &mut output);
        // The chunks answer a tick after the rest; the file holds the marker block.
        let (mut chunks, rest): (Vec<_>, Vec<_>) = output
            .answers
            .drain(..)
            .partition(|a| matches!(a, Response::Chunk { .. }));
        for a in &mut chunks {
            if let Response::Chunk {
                pos,
                loaded: Loaded::Generated(chunk, _),
                ..
            } = a
                && *pos == marker.chunk()
            {
                let local = |c: i32| u8::try_from(c.rem_euclid(16)).unwrap();
                chunk
                    .set_block(local(marker.x), marker.y, local(marker.z), gold())
                    .unwrap();
            }
        }
        output.answers = rest;
        output.answers.append(&mut late);
        late = chunks;
        if writer.check(&mut Context { state: &mut state }, 0) {
            break;
        }
    }
    assert_eq!(
        writer.marker,
        Some(marker),
        "the check saw the file's marker"
    );
}

fn start(data: &Path, mode: &str) -> Running {
    spawn_child(
        "tick::task::crash::user_data_crash_child",
        REPORT,
        &[(DATA_ENV, data.as_os_str()), (MODE_ENV, OsStr::new(mode))],
    )
}

fn progress(text: &str) -> Progress {
    let (tick, count) = text.split_once(',').unwrap();
    Progress {
        tick: tick.parse().unwrap(),
        count: count.parse().unwrap(),
    }
}

fn data_dir() -> PathBuf {
    std::env::temp_dir().join(format!("anchor-user-crash-{}", std::process::id()))
}

#[test]
fn forced_kills_keep_user_data_with_its_tick() {
    let data = data_dir();
    fresh(&data);
    let mut seed = 0x05e7_da7a_u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    // The values each tick wrote; a later run's line for a tick replaces an earlier one's.
    let mut written: BTreeMap<u64, (Progress, Progress)> = BTreeMap::new();
    // Whether a start found a commit in the files since they were made.
    let mut committed = false;
    // The starts that found a commit and checked it, over all the files ever made.
    let mut checked = 0;
    let mut advanced = 0;
    let mut last_start = 0;
    for kill in 0..=KILLS {
        let mode = if kill == KILLS { MODE_CHECK } else { MODE_RUN };
        let mut server = start(&data, mode);
        let line = server
            .lines
            .recv_timeout(REPORT_TIMEOUT)
            .unwrap_or_else(|_| {
                let status = server.child.try_wait().unwrap();
                panic!("kill {kill}: no report (exit {status:?}): a damaged file is refused")
            });
        let f = fields(&line);
        let start: u64 = f["start"].parse().unwrap();
        let (on_server, on_player) = (progress(&f["server"]), progress(&f["player"]));
        // A kill between the files of one save is rolled back by the start.
        if on_server.count == 0 {
            assert!(!committed, "kill {kill}: the commit was lost: {line}");
            assert_eq!(on_player, Progress::default(), "kill {kill}: {line}");
            assert_eq!(f["golds"], "", "kill {kill}: {line}");
        } else {
            committed = true;
            checked += 1;
            // The commit was taken before tick `start`: everything holds tick `start - 1`.
            let tick = start - 1;
            assert_eq!(on_server.tick, tick, "kill {kill}: {line}");
            assert_eq!(on_player, on_server, "kill {kill}: server and player split");
            let p = path(tick);
            assert_eq!(
                f["golds"],
                format!("{},{}", p.x, p.z),
                "kill {kill}: {line}"
            );
            assert_eq!(
                written.get(&tick),
                Some(&(on_server, on_player)),
                "kill {kill}: the commit of tick {tick} holds another tick's values: {line}"
            );
        }
        if start > last_start {
            advanced += 1;
            last_start = start;
        }
        // Within a run every tick writes once: ticks and counts go up by one together, from
        // what the restart found.
        let mut last = (start.checked_sub(1), on_server.count);
        let mut record = |line: &str| {
            let f = fields(line);
            if let Some(t) = f.get("tick") {
                let values = (progress(&f["server"]), progress(&f["player"]));
                let tick: u64 = t.parse().unwrap();
                assert_eq!(values.0.count - last.1, 1, "kill {kill}: tick {tick}");
                assert!(last.0.is_none_or(|l| tick > l), "kill {kill}: tick {tick}");
                assert_eq!(values.0, values.1, "kill {kill}: tick {tick}");
                last = (Some(tick), values.0.count);
                written.insert(tick, values);
            }
        };
        if mode == MODE_RUN {
            // The kill lands a random number of ticks after the run's first commit, counted by
            // the child's reports, so every restart finds a newer commit however slow the
            // machine is.
            let after = next() % 30;
            let mut since_commit = None;
            while since_commit.is_none_or(|ticks| ticks < after) {
                let line = server
                    .lines
                    .recv_timeout(REPORT_TIMEOUT)
                    .unwrap_or_else(|_| panic!("kill {kill}: the child stopped reporting"));
                record(&line);
                if line.starts_with("committed=") {
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
            record(&line);
        }
    }
    assert!(checked > 0, "no start found a commit to check");
    assert_eq!(advanced, KILLS, "every restart finds a newer commit");
    remove(&data);
}

/// The place of the marker block after tick `tick`, in the spawn chunks' dirt layer.
pub(in crate::tick) fn path(tick: u64) -> BlockPos {
    let t = i32::try_from(tick % 100_000).unwrap();
    let span = MARKER_RANGE.end - MARKER_RANGE.start;
    BlockPos::new(
        (t * 7) % span + MARKER_RANGE.start,
        MARKER_HEIGHT,
        (t * 11) % span + MARKER_RANGE.start,
    )
}

pub(in crate::tick) fn gold() -> block::State {
    BlockKind::Gold.default_state()
}

/// Runs the server of a crash test inside its child process on the data folder `data`: the save
/// thread and the tick thread as `Server::run` starts them, without its fingerprints, access
/// lists, world catalog, identity, network controls and custom content. The player `player`
/// joins through the tick's input queue with `after_join` behind its join, and the call returns
/// once the server closes the player's connection (the tasks in `setup` stop the server through
/// `stop`).
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the child drains its player's queue and waits for its threads"
)]
pub(in crate::tick) fn serve(
    data: &Path,
    player: &str,
    uuid: u8,
    mut setup: Setup,
    stop: mpsc::Receiver<()>,
    after_join: Vec<InputKind>,
) {
    let specs = vec![WorldSpec {
        name: "world".to_owned(),
        kind: crate::world::Kind::Persistent,
        generator: Arc::new(Flat),
        seed: Some(0),
        spawn: None,
    }];
    let mut opened = crate::save::open(data, specs).unwrap();
    setup.data.load(&mut opened.thread).unwrap();
    let pool = Arc::new(crate::runtime::cpu_pool(2).unwrap());
    let (inputs, input_queue) = input_queue(1);
    let (done, _done_rx) = oneshot::channel();
    let (crash, _crash_rx) = oneshot::channel();
    let (requests, request_queue) = mpsc::channel(crate::save::MAX_REQUESTS);
    let (responder, responses) = mpsc::channel(crate::save::MAX_RESPONSES);
    let save_pool = Arc::clone(&pool);
    let thread = opened.thread;
    let save_thread = crate::runtime::spawn_save(move || {
        crate::save::run(thread, &save_pool, request_queue, &responder)
    })
    .unwrap();
    let state = State::new(
        Settings::new(&Config::default()),
        shared(),
        setup,
        opened.worlds,
        opened.tick,
    );
    let (outbound, mut rx) = outbound_queue();
    let mut session = inputs.session(SESSION);
    session
        .try_send(InputKind::Join {
            identity: Identity {
                name: player.to_owned(),
                xuid: None,
                uuid: Uuid::from_bytes([uuid; 16]),
                skin: Skin::default(),
                device_os: -1,
                address: None,
                client: Client::default(),
            },
            outbound,
        })
        .unwrap();
    for kind in after_join {
        session.try_send(kind).unwrap();
    }
    let link = SaveLink {
        requests,
        responses,
    };
    let tick_pool = Arc::clone(&pool);
    let tick_thread = crate::runtime::spawn_tick(move || {
        run(state, &tick_pool, input_queue, link, stop, crash, done)
    })
    .unwrap();
    // The player's end of the connection: read until the server closes it.
    while let Some(out) = rx.blocking_recv() {
        if matches!(out, Outbound::Close(_)) {
            break;
        }
    }
    drop(rx);
    drop(session);
    drop(inputs);
    tick_thread.join().unwrap();
    save_thread.join().unwrap().unwrap();
}

/// Writes a report line of the child to the parent on standard output.
#[expect(
    clippy::print_stdout,
    clippy::disallowed_methods,
    reason = "consistency: the child reports to the parent on standard output"
)]
pub(in crate::tick) fn report(prefix: &str, line: &str) {
    println!("{prefix} {line}");
    std::io::stdout().flush().unwrap();
}

/// Asks the tick thread to stop (a full queue holds a stop already).
pub(in crate::tick) fn stop_server(stop: &mpsc::Sender<()>) {
    match stop.try_send(()) {
        Ok(()) | Err(_) => {}
    }
}

/// Starts this test binary as the child that runs the ignored test `test` with the environment
/// `envs`, and reads the lines of its output that carry the prefix `report`.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test starts and reads its child process"
)]
pub(in crate::tick) fn spawn_child(test: &str, report: &str, envs: &[(&str, &OsStr)]) -> Running {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        test,
        "--exact",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    for &(key, value) in envs {
        command.env(key, value);
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = std::sync::mpsc::channel();
    let prefix = report.to_owned();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            // libtest writes the test's name first on the same line.
            if let Some(rest) = line
                .find(&prefix)
                .and_then(|at| line.get(at + prefix.len()..))
                && tx.send(rest.trim().to_owned()).is_err()
            {
                break;
            }
        }
    });
    Running { child, lines }
}

/// The `key=value` fields of a report line.
pub(in crate::tick) fn fields(line: &str) -> BTreeMap<String, String> {
    line.split_whitespace()
        .filter_map(|f| f.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// Makes `dir` an empty data folder.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test makes its data folder"
)]
pub(in crate::tick) fn fresh(dir: &Path) {
    remove(dir);
    std::fs::create_dir_all(dir).unwrap();
}

/// Removes the data folder `dir`, if it exists.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test removes its data folder"
)]
pub(in crate::tick) fn remove(dir: &Path) {
    std::fs::remove_dir_all(dir).unwrap_or(());
}
