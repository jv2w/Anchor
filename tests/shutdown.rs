//! Normal shutdown (PLAN.md 10.9 "정상 종료"): a server process that reads `stop` from its console
//! stops cleanly. It exits with success, writes no crash log, and its final commit holds what the
//! last tick changed, which a restart reads back.
//!
//! The test runs its own test binary as the server process (`shutdown_child`) and writes to that
//! child's standard input.

#![cfg(test)]

use std::io::{BufRead, BufReader, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anchor::block::State;
use anchor::world::{BlockPos, ChunkPos};
use anchor::{Config, Server};

/// The child's environment variable naming its data folder.
const DATA_ENV: &str = "ANCHOR_SHUTDOWN_DATA";
/// The child's environment variable: `place` puts the marker and waits for `stop`, `check`
/// reports whether the marker is there and stops itself.
const MODE_ENV: &str = "ANCHOR_SHUTDOWN_MODE";
/// The prefix of the child's report line.
const REPORT: &str = "ANCHOR-SHUTDOWN";
/// Where the marker goes: in the spawn chunk, always loaded.
const MARKER: BlockPos = BlockPos::new(3, -60, 3);
/// The child gives up after this many ticks, so a lost parent leaves no process behind.
const CHILD_TICKS: u64 = 1200;

fn gold() -> State {
    State::find("minecraft:gold_block", &[]).unwrap()
}

/// Settings that run without the network beyond loopback, reading commands from the console.
/// Nothing connects to the child, so port 0 lets the system pick every port at bind time: a port
/// picked earlier and released could be taken by another process before the child binds it.
fn local_config() -> Config {
    let mut config = Config::default();
    config.general.xbox_auth = false;
    config.general.enable_ipv6 = false;
    config.general.server_port = 0;
    config.general.nethernet_port = 0;
    config.developer.console_input = true;
    config
}

/// The server process the parent test starts. Without the parent's environment it does nothing.
#[test]
#[ignore = "the child process of a_console_stop_saves_the_last_tick_and_exits_cleanly"]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the task gets the server's shutdown handle once, after the build"
)]
#[expect(
    clippy::print_stdout,
    reason = "consistency: the child reports to the parent on standard output"
)]
fn shutdown_child() {
    let (Ok(data), Ok(mode)) = (std::env::var(DATA_ENV), std::env::var(MODE_ENV)) else {
        return;
    };
    let (handle_tx, handle_rx) = std::sync::mpsc::sync_channel(1);
    let mut stop: Option<anchor::ShutdownHandle> = None;
    let mut start = None;
    let mut reported = false;
    let server = Server::builder()
        .config(local_config())
        .data_dir(&data)
        .every(NonZeroU32::MIN, move |ctx| {
            let stop = stop.get_or_insert_with(|| handle_rx.recv().unwrap());
            let start = *start.get_or_insert(ctx.tick());
            if ctx.tick() > start + CHILD_TICKS {
                stop.shutdown();
                return;
            }
            let world = ctx.default_world_mut();
            if reported || !world.is_chunk_loaded(ChunkPos::new(0, 0)) {
                return;
            }
            reported = true;
            if mode == "place" {
                world.set_block(MARKER, gold()).unwrap();
                println!("{REPORT} placed");
            } else {
                println!("{REPORT} gold={}", world.block(MARKER) == Some(gold()));
                stop.shutdown();
            }
            std::io::stdout().flush().unwrap();
        })
        .build()
        .unwrap();
    handle_tx.send(server.shutdown_handle()).unwrap();
    server.run().unwrap();
}

/// A started child and its report lines.
struct Running {
    child: Child,
    lines: mpsc::Receiver<String>,
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test starts and reads its child process"
)]
fn start(data: &Path, mode: &str) -> Running {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "shutdown_child",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(DATA_ENV, data)
        .env(MODE_ENV, mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    Running { child, lines }
}

/// The text after the child's report prefix.
fn report(server: &mut Running) -> String {
    loop {
        let line = server
            .lines
            .recv_timeout(Duration::from_secs(120))
            .unwrap_or_else(|_| {
                let status = server.child.try_wait().unwrap();
                panic!("no report from the child (exit {status:?})")
            });
        // libtest writes the test's name first on the same line.
        if let Some(rest) = line
            .find(REPORT)
            .and_then(|at| line.get(at + REPORT.len()..))
        {
            return rest.trim().to_owned();
        }
    }
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
fn exists(path: &Path) -> bool {
    path.exists()
}

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("shutdown-{}", std::process::id()))
}

#[test]
fn a_console_stop_saves_the_last_tick_and_exits_cleanly() {
    let data = data_dir();
    fresh(&data);
    let mut server = start(&data, "place");
    assert_eq!(report(&mut server), "placed");
    let mut stdin = server.child.stdin.take().unwrap();
    stdin.write_all(b"stop\n").unwrap();
    stdin.flush().unwrap();
    let status = server.child.wait().unwrap();
    assert!(status.success(), "the child exited with {status}");
    assert!(
        !exists(&data.join("crash")),
        "a normal stop wrote a crash log"
    );

    // Nothing saved the marker before `stop`: only the final commit holds it.
    let mut server = start(&data, "check");
    assert_eq!(report(&mut server), "gold=true");
    let status = server.child.wait().unwrap();
    assert!(status.success(), "the check child exited with {status}");
    fresh(&data);
}
