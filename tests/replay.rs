//! The replay tool through the public API: a server process whose task
//! saves and then panics writes a crash log; the same build replays it to the same panic without
//! changing the data files, and again after a restart moved the data files on, from the backups
//! the restart made. Data files another run committed at a segment's tick are not replayed on.
//!
//! The test runs its own test binary as the server process (`replay_child`).

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

#[path = "support/local_config.rs"]
mod local_config;
#[path = "support/temp_path.rs"]
mod temp_path;

use std::io::{BufRead, BufReader};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anchor::replay::End;
use anchor::world::BlockPos;
use anchor::{Config, Context, Server, ServerBuilder};

use local_config::local_config;
use temp_path::TempPath;

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
/// The mode of a child without backups that runs a while and stops.
const MODE_BASE: &str = "base";
/// The mode of a child that saves and panics.
const MODE_CRASH: &str = "crash";
/// The child's environment variable naming its mode: [`MODE_CRASH`], [`MODE_RESTART`],
/// [`MODE_BASE`] or [`MODE_FORK`].
const MODE_ENV: &str = "ANCHOR_REPLAY_MODE";
/// The mode of a child without backups that saves and panics later.
const MODE_FORK: &str = "fork";
/// The mode of a child that runs a while and stops.
const MODE_RESTART: &str = "restart";
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

/// Settings that run without the network beyond loopback, with start backups or without.
fn backup_config(backups: bool) -> Config {
    let mut config = local_config();
    config.general.backups = backups;
    config
}

/// Does what the task does at every tick, from the game state alone: sets a block a tick, saves
/// and panics as `plan` says; a run without a panic stops at [`STOP_AT`].
fn step(cx: &mut Context<'_>, plan: Plan) {
    let tick = cx.tick();
    let t = i32::try_from(tick % 32).unwrap();
    let stone = anchor::block::State::find("minecraft:stone", &[]).unwrap();
    // The spawn chunks load in the first ticks; until then the block is not set.
    match cx
        .default_world_mut()
        .set_block(BlockPos::new(t - 16, -60, 3), stone)
    {
        Ok(()) | Err(anchor::world::Error::NotLoaded) => {}
        Err(e) => panic!("{e}"),
    }
    if tick == plan.save_at {
        cx.save();
    }
    match plan.panic_at {
        Some(at) => assert!(at != tick, "child panic at {tick}"),
        None if tick >= STOP_AT => cx.shutdown(),
        None => {}
    }
}

/// The server of a run of `plan` on `data`, with start backups or without: the logged run and its
/// replays register the same.
fn builder(data: &Path, plan: Plan, backups: bool) -> ServerBuilder {
    Server::builder()
        .config(backup_config(backups))
        .data_dir(data)
        .every(NonZeroU32::MIN, move |cx| step(cx, plan))
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
        MODE_CRASH => (CRASH, true),
        MODE_FORK => (FORK, false),
        MODE_BASE => (stops, false),
        MODE_RESTART => (stops, true),
        _ => panic!("unknown mode {mode}"),
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
    let data = TempPath::new("replay");
    child(&data.0, MODE_CRASH);
    let log = crash_log(&data.0);
    let before = read_all(&data.0);

    let panicked = End::Panicked {
        tick: PANIC_AT,
        message: format!("child panic at {PANIC_AT}"),
    };

    let replay = build(&data.0, CRASH, true).replay(&log).unwrap();
    assert_eq!(replay.end(), &panicked);
    assert_eq!(replay.base_tick(), SAVE_AT + 1, "from the save's commit");
    assert_eq!(read_all(&data.0), before, "a replay writes nothing");

    // A restart backs the data files up and commits on: the replay starts from the backups.
    child(&data.0, MODE_RESTART);
    assert_ne!(
        read_all(&data.0),
        before,
        "the restart moved the data files on"
    );
    let replay = build(&data.0, CRASH, true).replay(&log).unwrap();
    assert_eq!(replay.end(), &panicked);
    assert_eq!(replay.base_tick(), SAVE_AT + 1);
}

#[test]
fn a_crash_log_is_not_replayed_on_a_server_file_another_run_committed_at_its_tick() {
    let data = TempPath::new("replay-fork");
    // Data files stopped at some tick, without backups.
    child(&data.0, MODE_BASE);
    let base = read_all(&data.0);
    // Run A from there commits its save and panics; its commit never reached the disk.
    child(&data.0, MODE_FORK);
    let log = data.0.join("a.log");
    rename(&crash_log(&data.0), &log);
    for (file, bytes) in data_files(&data.0).iter().zip(&base) {
        write(file, bytes);
    }
    // Run B from the same state commits a save of its own at the same tick, from other inputs
    // (its measured tick times at least).
    child(&data.0, MODE_FORK);
    let result = build(&data.0, FORK, false).replay(&log);
    assert!(
        matches!(result, Err(anchor::replay::Error::NoMatchingWorld)),
        "{result:?}"
    );
}
