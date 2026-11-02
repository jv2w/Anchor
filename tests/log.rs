//! The console log: a server process writes Anchor's lines on standard
//! error at `log-level`, and with the log off (`log-level = "off"` or
//! `ServerBuilder::without_console_log`) Anchor's events reach the program's own subscriber
//! instead.
//!
//! The test runs its own test binary as the server process (`log_child`), which stops itself once
//! the spawn chunk is loaded.

#![cfg(test)]

#[path = "support/local_config.rs"]
mod local_config;
#[path = "support/overrun.rs"]
mod overrun;
#[path = "support/temp_path.rs"]
mod temp_path;

use std::io::Write as _;
use std::process::{Command, Stdio};

use anchor::Server;
use anchor::config::LogLevel;
use anchor::world::ChunkPos;
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

use local_config::local_config;
use overrun::{Message, overrun_tick};
use temp_path::TempPath;

/// The child gives up after this many ticks, so a lost parent leaves no process behind.
const CHILD_TICKS: u64 = 1200;
/// The child's environment variable naming its data folder.
const DATA_ENV: &str = "ANCHOR_LOG_DATA";
/// The child's environment variable: `info`, `debug`, `off` (`log-level = "off"` and a program
/// subscriber) or `own` (`without_console_log` and a program subscriber).
const MODE_ENV: &str = "ANCHOR_LOG_MODE";
/// The prefix of the lines the program's own subscriber writes on standard output.
const OWN: &str = "ANCHOR-OWN";

/// The program's own subscriber: every event's message on standard output.
struct Own;

impl Subscriber for Own {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::INFO
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    #[expect(
        clippy::print_stdout,
        reason = "consistency: the program's subscriber reports to the parent on standard output"
    )]
    fn event(&self, event: &Event<'_>) {
        let mut message = Message::default();
        event.record(&mut message);
        println!("{OWN} {}", message.0);
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

/// What a child wrote: its standard output and the log lines of its standard error.
struct Written {
    stdout: String,
    log: Vec<String>,
}

/// The server process the parent test starts. Without the parent's environment it does nothing.
#[test]
#[ignore = "the child process of the console log tests"]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the child flushes its subscriber's output after the server stopped"
)]
fn log_child() {
    let (Ok(data), Ok(mode)) = (std::env::var(DATA_ENV), std::env::var(MODE_ENV)) else {
        return;
    };
    let mut config = local_config();
    config.developer.console_input = false;
    config.general.log_level = match mode.as_str() {
        "debug" => LogLevel::Debug,
        "off" => LogLevel::Off,
        _ => LogLevel::Info,
    };
    let mut builder = Server::builder().config(config).data_dir(&data);
    if mode == "own" {
        builder = builder.without_console_log();
    }
    if mode == "off" || mode == "own" {
        tracing::subscriber::set_global_default(Own).unwrap();
    }
    let mut start = None;
    builder
        .every(std::num::NonZeroU32::MIN, move |cx| {
            let start = *start.get_or_insert_with(|| {
                overrun_tick();
                cx.tick()
            });
            if cx.tick() > start + CHILD_TICKS
                || cx.default_world().is_chunk_loaded(ChunkPos::new(0, 0))
            {
                cx.shutdown();
            }
        })
        .build()
        .unwrap()
        .run()
        .unwrap();
    std::io::stdout().flush().unwrap();
}

fn run_child(mode: &str) -> Written {
    let data = TempPath::new(&format!("log-{mode}"));
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "log_child",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(DATA_ENV, &data.0)
        .env(MODE_ENV, mode)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "the {mode} child failed: {stderr}");
    // Lines that start like a date are log lines; libtest and panics write others.
    let log = stderr
        .lines()
        .filter(|l| l.starts_with("20"))
        .map(str::to_owned)
        .collect();
    Written {
        stdout: String::from_utf8(out.stdout).unwrap(),
        log,
    }
}

/// The line's text after its time and level, if it has Anchor's form at `level`.
fn after<'a>(line: &'a str, level: &str) -> Option<&'a str> {
    let (time, rest) = line.split_at_checked(24)?;
    let shape = time.char_indices().all(|(i, c)| match i {
        4 | 7 => c == '-',
        10 => c == ' ',
        13 | 16 => c == ':',
        19 => c == '.',
        23 => c == 'Z',
        _ => c.is_ascii_digit(),
    });
    let rest = rest.strip_prefix(' ')?.strip_prefix(level)?;
    shape.then_some(rest.trim_start())
}

#[test]
fn the_console_log_writes_start_listening_world_and_stop_lines_at_its_level() {
    let written = run_child("info");
    // The child's first tick overran, so a warning is among the lines: at info, every line is at
    // info or above.
    assert!(
        written.log.iter().all(|l| {
            ["INFO", "WARN", "ERROR"]
                .iter()
                .any(|level| after(l, level).is_some())
        }),
        "only lines at info or above at info: {:?}",
        written.log
    );
    let info: Vec<&str> = written
        .log
        .iter()
        .filter_map(|l| after(l, "INFO"))
        .collect();
    let version = format!(
        "server starting (version={} minecraft={} protocol=",
        anchor::Context::ANCHOR_VERSION,
        anchor::Context::VERSION
    );
    for wanted in [
        version.as_str(),
        "listening for NetherNet signaling on TCP (addr=127.0.0.1:",
        "listening for RakNet, NetherNet game data and server list pings on UDP (addr=127.0.0.1:",
        "world loaded (world=world kind=Persistent",
        "server started",
        "stop asked for by a command or callback",
        "server stopped",
    ] {
        assert!(
            info.iter().any(|l| l.starts_with(wanted)),
            "no line {wanted:?} in {info:?}"
        );
    }
    assert!(!written.stdout.contains(OWN));
    // Debug lines appear only at debug.
    let debug = run_child("debug");
    assert!(
        debug
            .log
            .iter()
            .any(|l| after(l, "DEBUG").is_some_and(|t| t.starts_with("threads started"))),
        "{:?}",
        debug.log
    );
}

#[test]
fn with_the_console_log_off_events_reach_the_programs_subscriber() {
    for mode in ["off", "own"] {
        let written = run_child(mode);
        assert!(written.log.is_empty(), "{mode}: {:?}", written.log);
        for wanted in ["server starting", "server started", "server stopped"] {
            assert!(
                written.stdout.contains(&format!("{OWN} {wanted}")),
                "{mode}: no {wanted:?} in {}",
                written.stdout
            );
        }
    }
}
