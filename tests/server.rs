//! Public API of `Server`: building, the thread budget, and running until shutdown.

#![cfg(test)]

use std::fmt;
use std::num::{NonZeroU16, NonZeroU32};
use std::path::{Path, PathBuf};

use anchor::command::{Command, Output};
use anchor::config::Threads;
use anchor::event::{PlayerDuplicateLogin, Priority};
use anchor::pack::Pack;
use anchor::world::{Flat, Kind, Void, World};
use anchor::{Config, Error, Server, ShutdownHandle};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// A path in Cargo's test temporary folder, unique to this process and test; not created, and
/// removed with its contents when dropped.
struct TempPath(PathBuf);

impl TempPath {
    fn new(name: &str) -> Self {
        Self(
            Path::new(env!("CARGO_TARGET_TMPDIR"))
                .join(format!("server-{}-{name}", std::process::id())),
        )
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test removes its data folder"
)]
impl Drop for TempPath {
    fn drop(&mut self) {
        // A test that never ran its server has no folder to remove.
        std::fs::remove_dir_all(&self.0).unwrap_or(());
    }
}

/// Settings that run without the network beyond loopback: offline login (no authorization
/// fetch) and port 0, so the system picks every port at bind time (no test connects to them; a
/// port picked earlier and released could be taken by another process first).
fn local_config() -> Config {
    let mut config = Config::default();
    config.general.xbox_auth = false;
    config.general.enable_ipv6 = false;
    config.general.server_port = 0;
    config.general.nethernet_port = 0;
    config
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the test inspects the file system"
)]
fn exists(path: &Path) -> bool {
    path.exists()
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
fn the_error_and_the_handle_cross_threads() {
    fn shareable<T: Send + Sync + 'static>() {}
    shareable::<Error>();
    shareable::<ShutdownHandle>();
}

#[test]
fn default_settings_build() {
    Server::builder().build().unwrap();
}

// pmmp: Server, PluginManager::registerEvent, Event, EventPriority
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

// pmmp: EventPriority
#[test]
fn priorities_order_from_lowest_to_highest() {
    use Priority::{High, Highest, Low, Lowest, Normal};
    let mut all = [Highest, Normal, Lowest, High, Low];
    all.sort();
    assert_eq!(all, [Lowest, Low, Normal, High, Highest]);
}

// pmmp: WorldManager::generateWorld
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

// pmmp: WorldManager::generateWorld
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

// pmmp: Command
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
    assert!(matches!(
        err,
        Error::Threads {
            network: 65_535,
            cpu: 65_535,
            ..
        }
    ));
    let message = err.to_string();
    assert!(message.contains("network-threads") && message.contains("cpu-threads"));
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

/// `backups = false`: a start keeps no copy of the world file; with it on, the second start
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
        let dir = data.0.join("backups");
        let copies = if exists(&dir) { entries(&dir).len() } else { 0 };
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

/// The warnings and errors logged on threads named `anchor-*`, as `thread: message`. Only a
/// server built `without_console_log` sends its events to the process's global subscriber; the
/// other tests' servers write theirs to their console log, and other test threads are not named
/// so.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: the test's subscriber collects events from every thread"
)]
static WARNINGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The global subscriber that fills [`WARNINGS`].
struct Warnings;

/// The message of an event.
#[derive(Default)]
struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl Subscriber for Warnings {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::WARN
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
/// load. The stop's slow stages are the final commit and the world file's compaction, durable
/// writes that take what the disk takes (under load up to 1.6 s, after a start of up to 4 s), so
/// a wall-clock bound measured the disk, not the stop. What matters is checked instead: `run`
/// returns `Ok` only after the tick has stopped, networking has stopped and the final commit and
/// the file's close have succeeded; and no stage waited out a timer. The stop path's waits that
/// can end by time end by their work here: the sessions' grace and the CPU pool's wait log a
/// warning when they run out, the transports' close deadlines apply to connections only (there
/// are none), and the runtime's grace waits only for blocking work still running (here at most
/// a memory reading).
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
    let server = Server::builder()
        .config(config)
        .data_dir(&data.0)
        .without_console_log()
        .every(NonZeroU32::MIN, move |_| {
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
