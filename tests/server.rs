//! Public API of `Server`: building, the thread budget, and running until shutdown.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

#[path = "support/local_config.rs"]
mod local_config;
#[path = "support/overrun.rs"]
mod overrun;
#[path = "support/temp_path.rs"]
mod temp_path;

use std::num::{NonZeroU16, NonZeroU32};
use std::path::{Path, PathBuf};

use anchor::command::{Command, Output};
use anchor::config::Threads;
use anchor::event::{PlayerDuplicateLogin, Priority};
use anchor::pack::Pack;
use anchor::world::{Flat, Kind, Void, World};
use anchor::{Config, Error, Server, ShutdownHandle};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

use local_config::local_config;
use overrun::{Message, overrun_tick};
use temp_path::TempPath;

/// The tracing target of the tick pacer's warning that ticks overran.
const CLOCK_TARGET: &str = "anchor::clock";
/// The tick intervals `tick_intervals` measures.
const INTERVALS: usize = 200;
/// Ticks `tick_intervals` leaves out at the start, while the spawn area loads.
const WARM_UP: usize = 20;
/// The warnings and errors logged on threads named `anchor-*`, as `thread: message`, except the
/// tick pacer's. Only a server built `without_console_log` sends its events to the process's
/// global subscriber; the other tests' servers write theirs to their console log, and other test
/// threads are not named so.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: the test's subscriber collects events from every thread"
)]
static WARNINGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

#[test]
fn the_error_and_the_handle_cross_threads() {
    fn shareable<T: Send + Sync + 'static>() {}
    shareable::<Error>();
    shareable::<ShutdownHandle>();
}

#[test]
fn default_settings_build() {
    Server::builder().build().unwrap();
}

// api: Server, PluginManager::registerEvent, Event, EventPriority
#[test]
fn listeners_of_every_priority_build_and_run() {
    let mut seen = 0_u32;
    let data = TempPath::new("listeners");
    let server = Server::builder()
        .config(local_config())
        .data_dir(&data.0)
        .on(Priority::Highest, |e: &mut PlayerDuplicateLogin, _| {
            e.uncancel();
        })
        .on(Priority::Lowest, |e: &mut PlayerDuplicateLogin, _| {
            e.cancel();
        })
        .on(Priority::Normal, move |e: &mut PlayerDuplicateLogin, _| {
            seen += 1;
            if e.is_cancelled() && e.existing().xuid().is_none() {
                e.set_disconnect_screen_message(Some(format!(
                    "{} ({seen})",
                    e.connecting().name()
                )));
            }
        })
        .build()
        .unwrap();
    let handle = server.shutdown_handle();
    handle.shutdown();
    server.run().unwrap();
}

// api: EventPriority
#[test]
fn priorities_order_from_lowest_to_highest() {
    use Priority::{High, Highest, Low, Lowest, Normal};
    let mut all = [Highest, Normal, Lowest, High, Low];
    all.sort();
    assert_eq!(all, [Lowest, Low, Normal, High, Highest]);
}

// api: WorldManager::generateWorld
#[test]
fn extra_worlds_need_names_of_their_own() {
    let built = Server::builder()
        .world("arena", Kind::Temporary, Flat)
        .world("lobby", Kind::Persistent, Void)
        .build();
    assert!(built.is_ok());
    let twice = Server::builder()
        .world("arena", Kind::Temporary, Flat)
        .world("arena", Kind::Persistent, Void)
        .build();
    assert!(matches!(twice, Err(Error::DuplicateWorld { name }) if name == "arena"));
    // The default world is `level-name`.
    let default = Server::builder()
        .world(Config::default().general.level_name, Kind::Temporary, Flat)
        .build();
    assert!(matches!(default, Err(Error::DuplicateWorld { name }) if name == "world"));
}

// api: WorldManager::generateWorld
#[test]
fn an_extra_world_runs_beside_the_default_one() {
    let data = TempPath::new("worlds");
    let server = Server::builder()
        .config(local_config())
        .data_dir(&data.0)
        .world("arena", Kind::Temporary, Flat)
        .every(NonZeroU32::MIN, |cx| {
            let names: Vec<&str> = cx.worlds().map(World::name).collect();
            assert_eq!(names, ["world", "arena"]);
            let arena = cx.worlds().nth(1).map(World::id).expect("the arena");
            assert!(cx.world(arena).is_some_and(|w| w.kind() == Kind::Temporary));
            cx.shutdown();
        })
        .build()
        .unwrap();
    // A failed check panics the tick thread, and `run` returns the panic.
    server.run().unwrap();
}

// api: Command
#[test]
fn a_server_builds_with_a_command_of_the_users() {
    let kit = Command::new("Kit", "Gives a kit")
        .alias("k")
        .overload(&[], |_, _, _| Ok(Output::from("given")));
    assert_eq!((kit.name(), kit.description()), ("kit", "Gives a kit"));
    assert_eq!(kit.aliases(), ["k"]);
    Server::builder().command(kit).build().unwrap();
}

#[test]
fn pack_errors_convert_for_the_question_mark_in_main() {
    fn main() -> Result<(), Error> {
        let _pack = Pack::from_bytes(b"not a zip archive".to_vec())?;
        Ok(())
    }
    let err = main().unwrap_err();
    assert!(matches!(err, Error::Pack(_)));
    assert_eq!(err.to_string(), "a resource pack is invalid");
    let source = std::error::Error::source(&err).expect("the pack error");
    assert!(
        source.to_string().starts_with("invalid zip archive"),
        "{source}"
    );
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test inspects the file system"
)]
fn exists(path: &Path) -> bool {
    path.try_exists().expect("inspect the path")
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test writes its broken pack file"
)]
fn a_broken_pack_file_fails_run_with_error_pack() {
    let data = TempPath::new("bad-pack");
    std::fs::create_dir_all(&data.0).expect("create the data folder");
    let pack = data.0.join("bad.zip");
    std::fs::write(&pack, b"not a zip archive").expect("write the pack file");
    let mut config = local_config();
    config.general.resource_packs = vec![pack];
    let server = Server::builder()
        .config(config)
        .data_dir(&data.0)
        .build()
        .unwrap();
    let err = server.run().unwrap_err();
    assert!(matches!(err, Error::Pack(_)), "{err:?}");
    // The pack is checked before any data file is opened.
    assert!(
        !exists(&data.0.join("main.redb")),
        "the start opened the data files before the pack failed"
    );
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test writes its settings and pack files"
)]
fn run_reads_relative_packs_from_the_data_pack_folder() {
    let root = TempPath::new("data-packs");
    let data = root.0.join("data");
    let packs = data.join("packs");
    std::fs::create_dir_all(&packs).expect("create the pack folder");
    std::fs::write(packs.join("bad.zip"), b"not a zip archive").expect("write the pack file");
    let file = root.0.join("anchor.toml");
    let settings = "[general]\nresource-packs = [\"bad.zip\"]\n";
    std::fs::write(&file, settings).expect("write the settings file");
    let config = local_config().load(&file).expect("valid settings");
    let server = Server::builder()
        .config(config)
        .data_dir(&data)
        .build()
        .unwrap();
    let err = server.run().unwrap_err();
    // The broken pack stops the start, read from the data folder.
    let Error::Pack(anchor::pack::Error::File { path, .. }) = &err else {
        panic!("{err:?}");
    };
    assert_eq!(*path, packs.join("bad.zip"));
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test inspects the file system"
)]
fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("read the folder")
        .map(|e| e.expect("read a folder entry").path())
        .collect();
    entries.sort();
    entries
}

#[test]
fn building_without_config_touches_no_file() {
    let data = TempPath::new("untouched");
    let cwd = std::env::current_dir().unwrap();
    let before = entries(&cwd);
    drop(Server::builder().data_dir(&data.0).build().unwrap());
    assert!(!exists(&data.0), "build() created the data folder");
    assert_eq!(entries(&cwd), before, "build() changed the working folder");
}

#[test]
fn thread_counts_over_the_budget_are_rejected_naming_the_keys() {
    let mut config = Config::default();
    config.developer.network_threads = Threads::Count(NonZeroU16::MAX);
    config.developer.cpu_threads = Threads::Count(NonZeroU16::MAX);
    let err = Server::builder().config(config).build().unwrap_err();
    let Error::Config(anchor::config::Error::Invalid { key, expected }) = &err else {
        panic!("{err:?}");
    };
    assert_eq!(key, "developer.network-threads");
    assert!(expected.contains("cpu-threads"), "{expected}");
}

#[test]
fn the_smallest_explicit_thread_counts_always_fit() {
    let one = Threads::Count(NonZeroU16::MIN);
    let mut config = Config::default();
    config.developer.network_threads = one;
    config.developer.cpu_threads = one;
    // 1 + 1 + 2 never exceeds max(logical CPUs, 4).
    Server::builder().config(config).build().unwrap();
}

/// The number of files in `dir` and in the folders under it.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test inspects the file system"
)]
fn files_in(dir: &Path) -> usize {
    entries(dir)
        .iter()
        .map(|entry| if entry.is_dir() { files_in(entry) } else { 1 })
        .sum()
}

/// `backups = false`: a start keeps no copy of the data files; with it on, the second start
/// does.
#[test]
fn without_backups_a_start_keeps_no_copy() {
    for backups in [true, false] {
        let data = TempPath::new(if backups { "backups" } else { "no-backups" });
        for _ in 0..2 {
            let mut config = local_config();
            config.general.backups = backups;
            let server = Server::builder()
                .config(config)
                .data_dir(&data.0)
                .build()
                .unwrap();
            server.shutdown_handle().shutdown();
            server.run().unwrap();
        }
        // Each data file has its own backup folder, a world's under `worlds/<world>/`.
        let dir = data.0.join("backups");
        let copies = if exists(&dir) { files_in(&dir) } else { 0 };
        assert_eq!(copies > 0, backups, "backups: {backups}");
    }
}

#[test]
fn a_shutdown_before_run_stops_it_at_once() {
    let data = TempPath::new("early");
    let server = Server::builder()
        .config(local_config())
        .data_dir(&data.0)
        .build()
        .unwrap();
    let handle = server.shutdown_handle();
    handle.shutdown();
    handle.shutdown();
    server.run().unwrap();
    // After the server stopped, a request does nothing.
    handle.shutdown();
}

/// `run` makes its own runtime on a thread of its own, so a program may call it from inside a
/// tokio runtime, which refuses a runtime started or stopped on its threads.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test macro starts a runtime to run inside"
)]
#[tokio::test]
async fn run_inside_a_tokio_runtime_ends_ok() {
    let data = TempPath::new("inside-runtime");
    let server = Server::builder()
        .config(local_config())
        .data_dir(&data.0)
        .build()
        .unwrap();
    server.shutdown_handle().shutdown();
    server.run().unwrap();
}

/// A key that is neither an XUID in decimal nor `offline:` and a name is refused where it enters.
#[test]
fn deleting_a_player_record_refuses_a_key_that_names_no_player() {
    let data = TempPath::new("delete-player-key");
    let server = Server::builder()
        .data_dir(&data.0)
        .without_console_log()
        .build()
        .unwrap();
    for key in ["", "steve", "007", "-1", "offline:", "offline:\n"] {
        let refused = server.delete_player_data(key);
        assert!(
            matches!(&refused, Err(Error::InvalidPlayerKey { key: named }) if named == key),
            "{key:?}: {refused:?}"
        );
    }
}

/// A record that is not there answers false and makes no file.
#[test]
fn deleting_a_player_record_that_is_not_there_answers_false() {
    let data = TempPath::new("delete-player-none");
    let server = Server::builder()
        .data_dir(&data.0)
        .without_console_log()
        .build()
        .unwrap();
    for key in ["2535428917000000", "offline:Steve"] {
        assert!(!server.delete_player_data(key).unwrap(), "{key}");
    }
    assert!(!exists(&data.0.join("players.redb")));
}

/// The global subscriber that fills [`WARNINGS`]. It skips the tick pacer's warning that ticks
/// overran: a loaded machine makes it in a normal run, and it is no stage of the stop.
struct Warnings;

impl Subscriber for Warnings {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::WARN && metadata.target() != CLOCK_TARGET
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let thread = std::thread::current();
        let Some(name) = thread.name().filter(|n| n.starts_with("anchor-")) else {
            return;
        };
        let mut message = Message::default();
        event.record(&mut message);
        WARNINGS
            .lock()
            .expect("the warnings")
            .push(format!("{name}: {}", message.0));
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

/// Stops a running server from another thread. The verdict does not depend on the machine's
/// load. The stop's slow stages are the final commit and the data files' close, durable
/// writes that take what the disk takes (under load up to 1.6 s, after a start of up to 4 s), so
/// a wall-clock bound measured the disk, not the stop. What matters is checked instead: `run`
/// returns `Ok` only after the tick has stopped, networking has stopped and the final commit and
/// the file's close have succeeded; and no stage waited out a timer. The stop path's waits that
/// can end by time end by their work here: the sessions' grace and the CPU pool's wait log a
/// warning when they run out, the transports' close deadlines apply to connections only (there
/// are none), and the runtime's grace waits only for blocking work still running (here at most
/// a memory reading). The tick pacer's warning that ticks overran is no such wait: a loaded
/// machine makes it in a normal run, so the first tick is held to make it here, and it is not
/// collected.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test runs the server on a thread of its own and waits for its tick"
)]
fn a_shutdown_from_another_thread_ends_run() {
    tracing::subscriber::set_global_default(Warnings).expect("the only global subscriber");
    let data = TempPath::new("running");
    let mut config = local_config();
    config.developer.console_input = false;
    let (ticked, tick) = std::sync::mpsc::sync_channel(1);
    let mut first = true;
    let server = Server::builder()
        .config(config)
        .data_dir(&data.0)
        .without_console_log()
        .every(NonZeroU32::MIN, move |_| {
            if std::mem::take(&mut first) {
                overrun_tick();
            }
            // Full after the first tick; the test waits for one only.
            ticked.try_send(()).unwrap_or(());
        })
        .build()
        .unwrap();
    let handle = server.shutdown_handle();
    let running = std::thread::Builder::new()
        .name("anchor-test-run".to_owned())
        .spawn(move || server.run())
        .expect("start the server thread");
    tick.recv().expect("the tick runs");
    handle.shutdown();
    running.join().expect("run does not panic").unwrap();
    let warnings = WARNINGS.lock().expect("the warnings");
    assert!(warnings.is_empty(), "{warnings:#?}");
}

/// Measures the spread of the real tick loop's intervals over 200 ticks,
/// as a task sees them. A wait on Windows' default 15.625 ms timer makes 50 ms ticks 46.9 or
/// 62.5 ms, which other players' movement shows as stutter. `cargo test --release --test server
/// tick_intervals -- --ignored --nocapture`.
#[test]
#[ignore = "measurement"]
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test times the ticks by the wall clock and runs the server on a thread of its own"
)]
#[expect(
    clippy::print_stdout,
    reason = "consistency: a measurement prints its results"
)]
fn tick_intervals() {
    let data = TempPath::new("intervals");
    let mut config = local_config();
    config.developer.console_input = false;
    let (ticked, ticks) = std::sync::mpsc::sync_channel(WARM_UP + INTERVALS + 1);
    let server = Server::builder()
        .config(config)
        .data_dir(&data.0)
        .without_console_log()
        .every(NonZeroU32::MIN, move |_| {
            ticked.try_send(std::time::Instant::now()).unwrap_or(());
        })
        .build()
        .unwrap();
    let handle = server.shutdown_handle();
    let running = std::thread::Builder::new()
        .name("anchor-test-run".to_owned())
        .spawn(move || server.run())
        .expect("start the server thread");
    let times: Vec<_> = (0..=WARM_UP + INTERVALS)
        .map(|_| ticks.recv().expect("a tick"))
        .collect();
    handle.shutdown();
    running.join().expect("run does not panic").unwrap();
    let mut ms: Vec<f64> = times[WARM_UP..]
        .windows(2)
        .map(|w| w[1].duration_since(w[0]).as_secs_f64() * 1e3)
        .collect();
    ms.sort_by(f64::total_cmp);
    let at = |percent: usize| ms[(ms.len() - 1) * percent / 100];
    println!(
        "{INTERVALS} tick intervals: min {:.2} ms, p1 {:.2}, p50 {:.2}, p99 {:.2}, max {:.2}",
        at(0),
        at(1),
        at(50),
        at(99),
        at(100)
    );
    assert!(at(1) > 47.0 && at(99) < 53.0, "ticks within 3 ms of 50");
}
