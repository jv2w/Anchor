//! Normal shutdown: a server process that reads `stop` from its console, or on Unix gets SIGINT,
//! SIGTERM or SIGHUP, stops cleanly. It exits with success, writes no crash log, leaves the mark
//! of a normal stop on every data file, and its final commit holds what the last tick changed,
//! which a restart reads back. A process started with SIGHUP ignored (`nohup`) does not stop at
//! SIGHUP.
//!
//! The test runs its own test binary as the server process (`shutdown_child`), writes to that
//! child's standard input and, on Unix, sends it signals with `kill`.

#![cfg(test)]

#[path = "support/local_config.rs"]
mod local_config;
#[path = "support/temp_path.rs"]
mod temp_path;

use std::io::{BufRead, BufReader, Write};
use std::num::NonZeroU32;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use anchor::block::State;
use anchor::config::{IntegrityCheck, LogLevel};
use anchor::world::{BlockPos, ChunkPos};
use anchor::{Config, Server};

use local_config::local_config;
use temp_path::TempPath;

/// The child gives up after this many ticks, so a lost parent leaves no process behind.
const CHILD_TICKS: u64 = 1200;
/// The child's environment variable naming its data folder.
const DATA_ENV: &str = "ANCHOR_SHUTDOWN_DATA";
/// The data files a run writes.
const FILES: [&str; 3] = ["main.redb", "players.redb", "world.redb"];
/// The debug line of a start that found a file's normal-stop mark and so skipped its check.
const MARKED: &str = "data file closed normally; its integrity is not checked";
/// Where the marker goes: in the spawn chunk, always loaded.
const MARKER: BlockPos = BlockPos::new(3, -60, 3);
/// The child's environment variable: `place` puts the marker and waits for `stop`, `check`
/// reports whether the marker is there and stops itself.
const MODE_ENV: &str = "ANCHOR_SHUTDOWN_MODE";
/// The prefix of the child's report line.
const REPORT: &str = "ANCHOR-SHUTDOWN";

fn gold() -> State {
    State::find("minecraft:gold_block", &[]).unwrap()
}

/// Settings that run without the network beyond loopback, reading commands from the console.
/// A start checks only the files without the normal-stop mark and logs the others at debug level.
fn console_config() -> Config {
    let mut config = local_config();
    config.general.log_level = LogLevel::Debug;
    config.developer.console_input = true;
    config.developer.integrity_check = IntegrityCheck::AfterUncleanShutdown;
    config
}

/// The server process the parent test starts. Without the parent's environment it does nothing.
#[test]
#[ignore = "the child process of the tests that stop a server"]
#[expect(
    clippy::print_stdout,
    clippy::disallowed_methods,
    reason = "consistency: the child reports to the parent on standard output"
)]
fn shutdown_child() {
    let (Ok(data), Ok(mode)) = (std::env::var(DATA_ENV), std::env::var(MODE_ENV)) else {
        return;
    };
    let mut start = None;
    let mut reported = false;
    Server::builder()
        .config(console_config())
        .data_dir(&data)
        .every(NonZeroU32::MIN, move |cx| {
            let start = *start.get_or_insert(cx.tick());
            if cx.tick() > start + CHILD_TICKS {
                cx.shutdown();
                return;
            }
            let world = cx.default_world_mut();
            if reported || !world.is_chunk_loaded(ChunkPos::new(0, 0)) {
                return;
            }
            reported = true;
            if mode == "place" {
                world.set_block(MARKER, gold()).unwrap();
                println!("{REPORT} placed");
            } else {
                println!("{REPORT} gold={}", world.block(MARKER) == Some(gold()));
                cx.shutdown();
            }
            std::io::stdout().flush().unwrap();
        })
        .build()
        .unwrap()
        .run()
        .unwrap();
}

/// A started child, its report lines and the lines of its console log.
struct Running {
    child: Child,
    lines: mpsc::Receiver<String>,
    /// Ends with the child's standard error, giving every line of it.
    log: JoinHandle<Vec<String>>,
}

/// The command that runs the child test binary directly.
fn child() -> Command {
    Command::new(std::env::current_exe().unwrap())
}

/// The command that runs the child test binary with SIGHUP ignored, as `nohup` starts a program.
/// The shell's `exec` keeps the ignored disposition and the process ID.
#[cfg(unix)]
fn child_ignoring_hangup() -> Command {
    let mut command = Command::new("sh");
    command
        .args(["-c", "trap '' HUP; exec \"$0\" \"$@\""])
        .arg(std::env::current_exe().unwrap());
    command
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test starts and reads its child process"
)]
fn start(mut command: Command, data: &Path, mode: &str) -> Running {
    let mut child = command
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
        .stderr(Stdio::piped())
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
    let stderr = child.stderr.take().unwrap();
    let log = std::thread::spawn(move || {
        BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
            .collect()
    });
    Running { child, lines, log }
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
    reason = "off-tick: the test inspects its data folder"
)]
fn exists(path: &Path) -> bool {
    path.exists()
}

/// Starts a child with `command` that places the marker, stops it with `stop` once it has, and
/// checks that it stopped normally: it exits with success, writes no crash log, leaves the
/// normal-stop mark on every data file, and a restart finds the marker.
fn stop_saves_the_last_tick(name: &str, command: Command, stop: impl FnOnce(&mut Running)) {
    // The tests of this file run at once, each on a folder of its own.
    let data = TempPath::new(&format!("shutdown-{name}"));
    let mut server = start(command, &data.0, "place");
    assert_eq!(report(&mut server), "placed");
    stop(&mut server);
    let status = server.child.wait().unwrap();
    assert!(status.success(), "the child exited with {status}");
    assert!(
        !exists(&data.0.join("crash")),
        "a normal stop wrote a crash log"
    );

    // Nothing saved the marker before the stop: only the final commit holds it.
    let mut server = start(child(), &data.0, "check");
    assert_eq!(report(&mut server), "gold=true");
    let status = server.child.wait().unwrap();
    assert!(status.success(), "the check child exited with {status}");
    let log = server.log.join().unwrap();
    for file in FILES {
        assert!(
            log.iter()
                .any(|line| line.contains(MARKED) && line.contains(file)),
            "{file} has no normal-stop mark:\n{}",
            log.join("\n")
        );
    }
}

#[test]
fn a_console_stop_saves_the_last_tick_and_exits_cleanly() {
    stop_saves_the_last_tick("console", child(), |server| {
        let mut stdin = server.child.stdin.take().unwrap();
        stdin.write_all(b"stop\n").unwrap();
        stdin.flush().unwrap();
    });
}

/// Sends the signal `name` (as `kill` spells it) to the child.
#[cfg(unix)]
fn signal(server: &Running, name: &str) {
    let status = Command::new("kill")
        .args([format!("-{name}"), server.child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill -{name} exited with {status}");
}

#[test]
#[cfg(unix)]
fn sigint_saves_the_last_tick_and_exits_cleanly() {
    stop_saves_the_last_tick("sigint", child(), |server| signal(server, "INT"));
}

#[test]
#[cfg(unix)]
fn sigterm_saves_the_last_tick_and_exits_cleanly() {
    stop_saves_the_last_tick("sigterm", child(), |server| signal(server, "TERM"));
}

#[test]
#[cfg(unix)]
fn sighup_saves_the_last_tick_and_exits_cleanly() {
    stop_saves_the_last_tick("sighup", child(), |server| signal(server, "HUP"));
}

/// A server started with SIGHUP ignored, as `nohup` starts it, keeps running through a SIGHUP
/// (a closed terminal): the SIGTERM after it is the first stop signal, which saves and exits
/// cleanly, not a second one, which would end the process with status 130.
#[test]
#[cfg(unix)]
fn sighup_ignored_at_start_keeps_the_server_running() {
    stop_saves_the_last_tick("nohup", child_ignoring_hangup(), |server| {
        signal(server, "HUP");
        signal(server, "TERM");
    });
}
