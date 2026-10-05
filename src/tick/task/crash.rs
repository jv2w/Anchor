//! Forced shutdown with user data (PLAN.md 10.8 판정식 "사용자 데이터"): a server writes the tick
//! into a server data value, a player data value and the terrain every tick, saving every third
//! tick, and is killed a random number of ticks after its first commit, again and again. Every
//! restart must find the three holding one and the same committed tick, newer than the last
//! restart's, with no tick counted twice or lost.
//!
//! The test runs its own test binary as the server process (`user_data_crash_child`) and kills
//! only that child, through its process handle, as the item crash test does
//! (`tick::tests::crash`): the server file, the save thread and the tick thread as `Server::run`
//! starts them, and one player who joins through the tick's input queue.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use super::super::queue::{Identity, InputKind, Outbound, SessionId, input_queue, outbound_queue};
use super::super::{Phase, SaveLink, Settings, Shared, State, Tasks, run};
use super::{Context, registered};
use crate::block::{self, Kind as BlockKind};
use crate::config::Config;
use crate::event::{Listeners, Priority, WorldSave};
use crate::protocol::Uuid;
use crate::save::WorldSpec;
use crate::world::{BlockPos, ChunkPos, Flat};

/// The child's environment variable naming its data folder.
const DATA_ENV: &str = "ANCHOR_USER_CRASH_DATA";
/// The child's environment variable: `run` keeps writing, `check` only reports.
const MODE_ENV: &str = "ANCHOR_USER_CRASH_MODE";
/// The prefix of the child's report lines.
const REPORT: &str = "ANCHOR-USER";
/// Kills, each followed by a restart that checks the file.
const KILLS: u32 = 20;
/// The child gives up after this many ticks, so a lost parent leaves no process behind.
const CHILD_TICKS: u64 = 1200;
const SESSION: SessionId = SessionId::new(1);
const NAME: &str = "keeper";

/// What the server and the player keep: the last tick that wrote it, and how many ticks have.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Progress {
    tick: u64,
    count: u64,
}

impl Progress {
    fn text(self) -> String {
        format!("{},{}", self.tick, self.count)
    }
}

/// Where the marker block is after tick `tick`, in the spawn chunks' dirt layer.
fn path(tick: u64) -> BlockPos {
    let t = i32::try_from(tick % 100_000).unwrap();
    BlockPos::new((t * 7) % 48 - 16, -62, (t * 11) % 48 - 16)
}

fn gold() -> block::State {
    BlockKind::Gold.default_state()
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
    #[expect(
        clippy::print_stdout,
        clippy::disallowed_methods,
        reason = "consistency: the child reports to the parent on standard output"
    )]
    fn report(line: &str) {
        println!("{REPORT} {line}");
        std::io::stdout().flush().unwrap();
    }

    fn stop_server(&self) {
        match self.stop.try_send(()) {
            Ok(()) | Err(_) => {}
        }
    }

    fn tick(&mut self, cx: &mut Context<'_>) {
        let tick = cx.tick();
        let start = *self.start.get_or_insert(tick);
        if tick > start + CHILD_TICKS {
            self.stop_server();
            return;
        }
        if !self.ready {
            self.ready = self.check(cx, start);
            if self.ready && !self.writes {
                self.stop_server();
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
        let id = cx.player_exact(NAME).expect("the player is in");
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
        Self::report(&format!(
            "tick={tick} server={} player={}",
            server.text(),
            mine.text()
        ));
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
        for x in -16..32 {
            for z in -16..32 {
                if world.block(BlockPos::new(x, -62, z)) == Some(gold()) {
                    golds.push(format!("{x},{z}"));
                }
            }
        }
        self.marker = golds.first().and_then(|g| {
            let (x, z) = g.split_once(',')?;
            Some(BlockPos::new(x.parse().ok()?, -62, z.parse().ok()?))
        });
        let server = cx.data::<Progress>().copied().expect("registered");
        let mine = player.data::<Progress>().copied().expect("registered");
        Self::report(&format!(
            "start={start} golds={} server={} player={}",
            golds.join(";"),
            server.text(),
            mine.text()
        ));
        true
    }
}

/// The server process the parent test starts and kills. Without the parent's environment it
/// does nothing.
#[test]
#[ignore = "the child process of forced_kills_keep_user_data_with_its_tick"]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the child drains its player's queue and waits for its threads"
)]
fn user_data_crash_child() {
    let (Ok(data), Ok(mode)) = (std::env::var(DATA_ENV), std::env::var(MODE_ENV)) else {
        return;
    };
    let specs = vec![WorldSpec {
        name: "world".to_owned(),
        kind: crate::world::Kind::Persistent,
        generator: Arc::new(Flat),
        seed: Some(0),
    }];
    let mut opened = crate::save::open(Path::new(&data), specs).unwrap();
    let mut user = registered::<Progress, Progress>("progress", "progress");
    user.load(&mut opened.thread).unwrap();
    let pool = Arc::new(crate::runtime::cpu_pool(2).unwrap());
    let (inputs, input_queue) = input_queue();
    let (stop_tx, stop) = mpsc::channel(1);
    let (done, _done_rx) = oneshot::channel();
    let (crash, _crash_rx) = oneshot::channel();
    let (requests, request_queue) = mpsc::channel(crate::save::REQUEST_CAPACITY);
    let (responder, responses) = mpsc::channel(crate::save::RESPONSE_CAPACITY);
    let save_pool = Arc::clone(&pool);
    let thread = opened.thread;
    let save_thread = crate::runtime::spawn_save(move || {
        crate::save::run(thread, &save_pool, request_queue, &responder)
    })
    .unwrap();
    let mut writer = Writer {
        writes: mode == "run",
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
            Writer::report(&format!("committed={}", cx.tick()));
        },
    );
    listeners.sort();
    let state = State::new(
        Settings::new(&Config::default()),
        Shared::new(
            crate::protocol::batch::Settings::default(),
            &crate::custom::Registry::default(),
            &crate::tick::entity::Customs::default(),
        )
        .unwrap(),
        crate::tick::Setup {
            listeners,
            tasks,
            data: user,
            ..crate::tick::Setup::default()
        },
        opened.worlds,
        opened.tick,
    );
    let (outbound, mut rx) = outbound_queue();
    let mut session = inputs.session(SESSION);
    session
        .try_send(InputKind::Join {
            identity: Identity {
                name: NAME.to_owned(),
                xuid: None,
                uuid: Uuid::from_bytes([4; 16]),
                skin: crate::tick::queue::Skin::default(),
                device_os: -1,
                address: None,
                client: crate::tick::queue::Client::default(),
            },
            outbound,
        })
        .unwrap();
    let link = SaveLink {
        requests,
        responses,
    };
    let tick_pool = Arc::clone(&pool);
    let tick_thread = crate::runtime::spawn_tick(move || {
        run(state, &tick_pool, input_queue, link, stop, crash, done)
    })
    .unwrap();
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

/// A started child and its report lines.
struct Running {
    child: Child,
    lines: std::sync::mpsc::Receiver<String>,
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test starts and reads its child process"
)]
fn start(data: &Path, mode: &str) -> Running {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "tick::task::crash::user_data_crash_child",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(DATA_ENV, data)
        .env(MODE_ENV, mode)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(rest) = line
                .find(REPORT)
                .and_then(|at| line.get(at + REPORT.len()..))
                && tx.send(rest.trim().to_owned()).is_err()
            {
                break;
            }
        }
    });
    Running { child, lines }
}

fn fields(line: &str) -> BTreeMap<String, String> {
    line.split_whitespace()
        .filter_map(|f| f.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn progress(text: &str) -> Progress {
    let (tick, count) = text.split_once(',').unwrap();
    Progress {
        tick: tick.parse().unwrap(),
        count: count.parse().unwrap(),
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test prepares and removes its data folder"
)]
fn fresh(dir: &Path, create: bool) {
    std::fs::remove_dir_all(dir).unwrap_or(());
    if create {
        std::fs::create_dir_all(dir).unwrap();
    }
}

fn data_dir() -> PathBuf {
    std::env::temp_dir().join(format!("anchor-user-crash-{}", std::process::id()))
}

/// The check of a restart reads the marker block only once the spawn chunks are in, even when the
/// save thread answers them a tick after the player data (a busy save thread on a loaded machine).
#[test]
fn the_check_waits_for_spawn_chunks_answered_after_the_player() {
    use crate::save::{Loaded, Response};
    use crate::tick::Output;
    use crate::tick::tests::{join, settings, shared, tick};

    let world = crate::world::World::new(
        "world",
        crate::world::Kind::Persistent,
        0,
        crate::world::Generator::spawn(&Flat, 0),
    );
    let mut state = State::new(
        settings(|c| c.general.save_player_data = true),
        shared(),
        crate::tick::Setup {
            data: registered::<Progress, Progress>("progress", "progress"),
            ..crate::tick::Setup::default()
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

#[test]
fn forced_kills_keep_user_data_with_its_tick() {
    let data = data_dir();
    fresh(&data, true);
    let mut seed = 0x05e7_da7a_u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    // The values each tick wrote; a later run's line for a tick replaces an earlier one's.
    let mut written: BTreeMap<u64, (Progress, Progress)> = BTreeMap::new();
    let mut committed = false;
    let mut advanced = 0;
    let mut last_start = 0;
    for kill in 0..=KILLS {
        let mode = if kill == KILLS { "check" } else { "run" };
        let mut server = start(&data, mode);
        let line = server
            .lines
            .recv_timeout(Duration::from_secs(120))
            .unwrap_or_else(|_| {
                let status = server.child.try_wait().unwrap();
                panic!("kill {kill}: no report (exit {status:?}): a damaged file is refused")
            });
        let f = fields(&line);
        let start: u64 = f["start"].parse().unwrap();
        let (on_server, on_player) = (progress(&f["server"]), progress(&f["player"]));
        if on_server.count == 0 {
            assert!(!committed, "kill {kill}: the commit was lost: {line}");
            assert_eq!(on_player, Progress::default(), "kill {kill}: {line}");
            assert_eq!(f["golds"], "", "kill {kill}: {line}");
        } else {
            committed = true;
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
        if mode == "run" {
            // The kill lands a random number of ticks after the run's first commit, counted by
            // the child's reports, so every restart finds a newer commit however slow the
            // machine is.
            let after = next() % 30;
            let mut since_commit = None;
            while since_commit.is_none_or(|ticks| ticks < after) {
                let line = server
                    .lines
                    .recv_timeout(Duration::from_secs(120))
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
    assert!(committed, "nothing was ever committed");
    assert_eq!(advanced, KILLS, "every restart finds a newer commit");
    fresh(&data, false);
}
