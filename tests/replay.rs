//! The replay tool through the public API: a server process whose task
//! saves and then panics writes a crash log; the same build replays it to the same panic without
//! changing the data files, and again after a restart moved the data files on, from the backups
//! the restart made. Data files another run committed at a segment's tick are not replayed on.
//!
//! The test runs its own test binary as the server process (`replay_child`).

#![cfg(test)]

use std::io::{BufRead, BufReader};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anchor::replay::End;
use anchor::world::BlockPos;
use anchor::{Config, Context, Server, ServerBuilder};

/// The plan of the run that writes the crash log.
const CRASH: Plan = Plan {
    save_at: SAVE_AT,
    panic_at: Some(PANIC_AT),
};
/// The child's environment variable naming its data folder.
const DATA_ENV: &str = "ANCHOR_REPLAY_DATA";
/// The plan of the fork of a stopped world.
const FORK: Plan = Plan {
    save_at: FORK_SAVE_AT,
    panic_at: Some(FORK_PANIC_AT),
};
/// The tick that panics in a fork of a stopped world.
const FORK_PANIC_AT: u64 = 200;
/// The tick whose save a fork of a stopped world commits.
const FORK_SAVE_AT: u64 = 160;
/// The child's environment variable: `crash` saves and panics, `restart` runs a while and stops;
/// without backups, `base` runs a while and stops and `fork` saves and panics later.
const MODE_ENV: &str = "ANCHOR_REPLAY_MODE";
/// The tick that panics.
const PANIC_AT: u64 = 100;
/// The tick whose save the crash log starts from.
const SAVE_AT: u64 = 20;
/// The tick a restart stops at.
const STOP_AT: u64 = 120;

/// What the task of a run does: the tick it saves at and the tick it panics at, if any.
#[derive(Debug, Clone, Copy)]
struct Plan {
    save_at: u64,
    panic_at: Option<u64>,
}

/// Settings that run without the network beyond loopback, with start backups or without. Port 0
/// lets the system pick every port at bind time: nothing connects, and a port picked earlier and
/// released could be taken by another process first.
fn local_config(backups: bool) -> Config {
    let mut config = Config::default();
    config.general.xbox_auth = false;
    config.general.enable_ipv6 = false;
    config.general.server_port = 0;
    config.general.backups = backups;
    config
}

/// What the task does at every tick, from the game state alone: a block a tick, a save and a
/// panic as `plan` says; a run without a panic stops at [`STOP_AT`].
fn step(ctx: &mut Context<'_>, plan: Plan) {
    let tick = ctx.tick();
    let t = i32::try_from(tick % 32).unwrap();
    let stone = anchor::block::State::find("minecraft:stone", &[]).unwrap();
    // The spawn chunks load in the first ticks; until then the block is not set.
    match ctx
        .default_world_mut()
        .set_block(BlockPos::new(t - 16, -60, 3), stone)
    {
        Ok(()) | Err(anchor::world::Error::NotLoaded) => {}
        Err(e) => panic!("{e}"),
    }
    if tick == plan.save_at {
        ctx.save();
    }
    match plan.panic_at {
        Some(at) => assert!(at != tick, "child panic at {tick}"),
        None if tick >= STOP_AT => ctx.shutdown(),
        None => {}
    }
}

/// The server of a run of `plan` on `data`, with start backups or without: the logged run and its
/// replays register the same.
fn builder(data: &Path, plan: Plan, backups: bool) -> ServerBuilder {
    Server::builder()
        .config(local_config(backups))
        .data_dir(data)
        .every(NonZeroU32::MIN, move |ctx| step(ctx, plan))
}

/// The server process the parent tests start. Without the parent's environment it does nothing.
#[test]
#[ignore = "the child process of the replay tests"]
fn replay_child() {
    let (Ok(data), Ok(mode)) = (std::env::var(DATA_ENV), std::env::var(MODE_ENV)) else {
        return;
    };
    let stops = Plan {
        panic_at: None,
        ..CRASH
    };
    let (plan, backups) = match mode.as_str() {
        "crash" => (CRASH, true),
        "fork" => (FORK, false),
        "base" => (stops, false),
        _ => (stops, true),
    };
    let result = builder(Path::new(&data), plan, backups)
        .build()
        .unwrap()
        .run();
    assert_eq!(result.is_err(), plan.panic_at.is_some(), "{result:?}");
}

/// Runs the child in `mode` on `data` to its end.
fn child(data: &Path, mode: &str) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "replay_child",
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
    let lines: Vec<String> = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .collect();
    let status = child.wait().unwrap();
    assert!(status.success(), "the {mode} child failed: {lines:?}");
}

/// The server the logged run of `plan` was built as.
fn build(data: &Path, plan: Plan, backups: bool) -> Server {
    builder(data, plan, backups).build().unwrap()
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test prepares and inspects its data folder"
)]
fn fresh(dir: &Path) {
    std::fs::remove_dir_all(dir).unwrap_or(());
    std::fs::create_dir_all(dir).unwrap();
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test inspects its data folder"
)]
fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test changes its data folder between runs"
)]
fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test changes its data folder between runs"
)]
fn rename(from: &Path, to: &Path) {
    std::fs::rename(from, to).unwrap();
}

/// The data files of the runs: the server's and the default world's.
fn data_files(data: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = ["main", "players"]
        .iter()
        .map(|name| data.join(format!("{name}.redb")))
        .collect();
    files.push(data.join("worlds").join("world.redb"));
    files
}

/// The bytes of every data file.
fn read_all(data: &Path) -> Vec<Vec<u8>> {
    data_files(data).iter().map(|f| read(f)).collect()
}

/// The one crash log in `data`.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test inspects its data folder"
)]
fn crash_log(data: &Path) -> PathBuf {
    let logs: Vec<PathBuf> = std::fs::read_dir(data.join("crash"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?}");
    logs.into_iter().next().unwrap()
}

#[test]
fn a_crash_log_replays_to_its_panic_before_and_after_a_restart() {
    let data =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("replay-{}", std::process::id()));
    fresh(&data);
    child(&data, "crash");
    let log = crash_log(&data);
    let before = read_all(&data);

    let panicked = End::Panicked {
        tick: PANIC_AT,
        message: format!("child panic at {PANIC_AT}"),
    };

    let replay = build(&data, CRASH, true).replay(&log).unwrap();
    assert_eq!(replay.end(), &panicked);
    assert_eq!(replay.base_tick(), SAVE_AT + 1, "from the save's commit");
    assert_eq!(read_all(&data), before, "a replay writes nothing");

    // A restart backs the data files up and commits on: the replay starts from the backups.
    child(&data, "restart");
    assert_ne!(
        read_all(&data),
        before,
        "the restart moved the data files on"
    );
    let replay = build(&data, CRASH, true).replay(&log).unwrap();
    assert_eq!(replay.end(), &panicked);
    assert_eq!(replay.base_tick(), SAVE_AT + 1);
    fresh(&data);
}

#[test]
fn a_crash_log_is_not_replayed_on_a_server_file_another_run_committed_at_its_tick() {
    let data =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("replay-fork-{}", std::process::id()));
    fresh(&data);
    // Data files stopped at some tick, without backups.
    child(&data, "base");
    let base = read_all(&data);
    // Run A from there commits its save and panics; its commit never reached the disk.
    child(&data, "fork");
    let log = data.join("a.log");
    rename(&crash_log(&data), &log);
    for (file, bytes) in data_files(&data).iter().zip(&base) {
        write(file, bytes);
    }
    // Run B from the same state commits a save of its own at the same tick, from other inputs
    // (its measured tick times at least).
    child(&data, "fork");
    let result = build(&data, FORK, false).replay(&log);
    assert!(
        matches!(result, Err(anchor::replay::Error::NoMatchingWorld)),
        "{result:?}"
    );
    fresh(&data);
}
