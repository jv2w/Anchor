//! The server and its builder.

use std::any::Any;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::args::{self, Task};
use crate::clock;
use crate::config::{Config, LevelType};
use crate::console;
use crate::error::{Error, Result};
use crate::event::{self, Priority};
use crate::pack::Pack;
use crate::protocol::packet::PROTOCOL_VERSION;
use crate::runtime::{self, Runtime, ThreadCounts, ThreadTotals};
use crate::save::{self, WorldSpec};
use crate::session;
use crate::tick::queue::{self, InputKind, InputSender, Measure, SessionId, SessionInputs};
use crate::tick::{self, Context, Setup};
use crate::world::{Flat, Generator, Kind, Void};

/// A Minecraft: Bedrock Edition server. Make one with [`Server::builder`], then call
/// [`Server::run`].
#[derive(Debug)]
// pmmp-class: Server
pub struct Server {
    config: Arc<Config>,
    data_dir: PathBuf,
    threads: ThreadCounts,
    setup: Setup,
    worlds: Vec<WorldSpec>,
    /// The resource packs given in code, offered after those of the settings.
    packs: Vec<Pack>,
    shutdown: ShutdownHandle,
    shutdown_requests: mpsc::Receiver<()>,
    /// Whether Anchor's console log is on ([`ServerBuilder::without_console_log`]).
    console_log: bool,
}

/// Collects what a [`Server`] needs; [`build`](Self::build) checks it.
#[derive(Debug)]
pub struct ServerBuilder {
    config: Config,
    data_dir: PathBuf,
    /// Whether Anchor's console log is on ([`Self::without_console_log`]).
    console_log: bool,
    /// What the user registers; modules add their builder methods in their own files.
    pub(crate) setup: Setup,
    /// Worlds besides the default one, in registration order.
    worlds: Vec<WorldSpec>,
    /// The resource packs given in code ([`ServerBuilder::resource_pack`]), in order.
    pub(crate) packs: Vec<Pack>,
}

/// Asks a [`Server`] to stop. Cheap to clone and usable from any thread.
#[derive(Debug, Clone)]
pub struct ShutdownHandle(mpsc::Sender<()>);

impl Server {
    /// A builder with [`Config::default`] and the data folder `data`.
    pub fn builder() -> ServerBuilder {
        ServerBuilder {
            config: Config::default(),
            data_dir: PathBuf::from("data"),
            console_log: true,
            setup: Setup::default(),
            worlds: Vec::new(),
            packs: Vec::new(),
        }
    }

    /// A handle that stops this server from any thread, before or during [`run`](Self::run).
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.shutdown.clone()
    }

    /// Starts Anchor's console log and makes it the calling thread's default until the guard
    /// drops, which writes the lines still queued; `None` if it is off (`log-level = "off"` or
    /// [`ServerBuilder::without_console_log`]). Fails if its thread cannot start.
    fn console_log(&self) -> std::io::Result<Option<console::log::Installed>> {
        if !self.console_log {
            return Ok(None);
        }
        let log = console::log::start(self.config.general.log_level)?;
        Ok(log.map(console::log::Log::install))
    }

    /// Imports the Bedrock world folder `dir` (a `level.dat` and its `db` database folder, as BDS
    /// and the game write it) as the persistent world `world`, before [`run`](Self::run).
    ///
    /// The world becomes the new file `data/worlds/<world>.redb`, and nothing else is
    /// written: several Bedrock worlds import side by side under their own names. A declared
    /// world ([`ServerBuilder::world`], or `level-name`) must be [`Kind::Persistent`]; a name not
    /// declared takes the default world's generator for its biomes, and the tick loads it with
    /// [`Context::load_world`]. The name must be a file name
    /// ([`world::Error::InvalidName`](crate::world::Error::InvalidName) tells the rule;
    /// [`Error::InvalidWorldName`] otherwise). An import only creates a world: a world file of the
    /// name (in any letter case) refuses it with
    /// [`import::Error::WorldExists`](crate::import::Error) and changes nothing. Blocks and items of older versions are upgraded as PMMP upgrades them;
    /// blocks Anchor does not know are kept as unknown blocks. The overworld's blocks and block
    /// entities (with their whole NBT: container items, sign text, ...) are imported, with the
    /// time; a chunk's biomes are those the world's generator gives it. Biomes,
    /// entities and pending block updates of the source are left out and counted, as are the
    /// Nether, the End and the players (Bedrock files players by device, not by Xbox user ID).
    /// The [`import::Report`](crate::import::Report) counts what was imported and left out. The
    /// world keeps the spawn of `level.dat`
    /// ([`import::Report::spawn`](crate::import::Report::spawn)) as its own, but no setting of
    /// `level.dat` is imported (seed, game mode, difficulty, game rules): the world runs with the
    /// server's settings.
    ///
    /// A program started as `<program> import <world folder> [<world name>]` runs this from
    /// [`run`](Self::run), the default world without a name.
    ///
    /// It blocks the calling thread, uses a CPU pool of `cpu-threads` that it joins before
    /// returning, and writes the whole world in one transaction: on any failure, a chunk that
    /// cannot be read included, no world file is left; the error names the source file at fault,
    /// and the chunk and the key ([`import::Error::Key`](crate::import::Error::Key)) when a
    /// chunk's value is. The source folder is only read, and the
    /// import fails if it changes meanwhile.
    #[doc(alias = "FormatConverter::execute")]
    pub fn import_world(
        &self,
        dir: impl AsRef<Path>,
        world: &str,
    ) -> Result<crate::import::Report> {
        let _log = self.console_log().map_err(Error::Start)?;
        crate::import::run(
            &self.data_dir,
            crate::storage::Start::of(&self.config),
            &self.worlds,
            self.threads.cpu,
            &self.setup.custom,
            dir.as_ref(),
            world,
        )
    }

    /// Exports the persistent world `world` as the Bedrock world folder `dir` (a `level.dat` and
    /// its `db` database folder, which BDS and the game open), the reverse of
    /// [`import_world`](Self::import_world).
    ///
    /// It reads the world's file `data/worlds/<world>.redb` (the name in any letter case)
    /// and the main file read-only, at the last save, and writes nothing in the data folder. A
    /// world file later than the main file is refused; one start of the server rolls it back. A
    /// running server holds the main file, so it refuses the export ([`Error::Storage`]). `dir`
    /// must not be there yet
    /// ([`export::Error::FolderExists`](crate::export::Error::FolderExists)): the world is
    /// written into `<dir>.tmp` and renamed to `dir` once whole, so on any failure no folder of
    /// its own is left, and the error names the chunk at fault
    /// ([`export::Error::Chunk`](crate::export::Error::Chunk)).
    ///
    /// Every chunk the file holds is written with its blocks, biomes and block entities. A
    /// chunk whose terrain was never saved (only its entities or block entities were) gets what
    /// the world's generator makes, as when it loads: a declared world's generator and seed, or
    /// for another name the default world's generator with the seed of the name. Entities,
    /// scheduled block updates and spills are left out and counted in the
    /// [`export::Report`](crate::export::Report): they are saved in Anchor's own form, and so
    /// are players, who are not in the world's file. Chunks the file does not hold (generated and never changed) are not written: the
    /// Bedrock world's flat generator makes them, as BDS's default flat world for a world of
    /// [`Flat`] (the same terrain) and as air for any other generator. `level.dat` holds the
    /// world's name, spawn and time and no setting (game mode, difficulty, game rules): a world
    /// runs with the settings of the server that opens it.
    ///
    /// A program started as `<program> export <world folder> [<world name>]` runs this from
    /// [`run`](Self::run), the default world without a name.
    ///
    /// It blocks the calling thread and uses a CPU pool of `cpu-threads` that it joins before
    /// returning. The same world file gives the same files whatever the thread count.
    pub fn export_world(
        &self,
        world: &str,
        dir: impl AsRef<Path>,
    ) -> Result<crate::export::Report> {
        let _log = self.console_log().map_err(Error::Start)?;
        crate::export::run(
            &self.data_dir,
            &self.worlds,
            self.threads.cpu,
            world,
            dir.as_ref(),
        )
    }

    /// Runs the server until a [`ShutdownHandle`] or a stop signal stops it, blocking the calling
    /// thread.
    ///
    /// The stop signals are Ctrl+C, SIGTERM and SIGHUP on Unix (SIGHUP only when it is not
    /// ignored as the server starts, as `nohup` ignores it), and Ctrl+C, Ctrl+Break and the
    /// console's close and shutdown events on Windows (after those two Windows ends the process
    /// about 5 seconds later, whether the stop finished or not). A second stop signal
    /// ends the process with status 130, as after a crash: the last commit stays. It first lets
    /// the console log write its lines, for half a second at most.
    ///
    /// While it runs, Anchor's console log ([`ServerBuilder::without_console_log`]) is the calling
    /// thread's default and that of every thread Anchor starts.
    ///
    /// It first opens the data files: `data/main.redb` (the save's tick, operators, bans,
    /// whitelist and the server data), `data/players.redb` and the file
    /// `data/worlds/<name>.redb` of each persistent world it starts
    /// with; the other world files there are worlds [`Context::load_world`] may load. They are
    /// checked together as `integrity-check` says, without being written, before any is opened:
    /// a damaged one, or one of a newer format, refuses the start, and every file is left as it
    /// was. With `backups`, each file is then backed up into its own folder of `data/backups/`
    /// while the server runs. A players or world file holding a later save than the main file (a
    /// crash in the middle of a save) is first rolled back to the main file's save. Then the packs load from `data/packs`. On shutdown every player sees
    /// `shutdown-message`, the last state is saved to every data file it changed (one tick's
    /// state, the main file last), networking stops, and the threads are joined. Fails if a data
    /// file is damaged or cannot be opened, if a pack cannot be offered, if the runtime, the
    /// threads or networking cannot start, if a save fails after its retries, or if the tick
    /// thread panics.
    ///
    /// The program's process arguments can ask for a command instead, which the program answers
    /// with no code of its own, writing the result to standard output: `<program> update-config`
    /// reports what [`Config::load`] added to the settings file and removed from it,
    /// `<program> import <world folder> [<world name>]` runs [`import_world`](Self::import_world)
    /// and `<program> export <world folder> [<world name>]` runs
    /// [`export_world`](Self::export_world) (the default world, `level-name`, without a name).
    /// Each then returns without starting the server, so it can run beside a running server.
    /// Those commands given other arguments fail with [`Error::Usage`]; any other arguments are
    /// the program's own.
    pub fn run(self) -> Result<()> {
        let task = args::current()?;
        self.run_task(task)
    }

    /// [`run`](Self::run) with the command `task` of the process arguments.
    fn run_task(self, task: Task) -> Result<()> {
        let answer = match task {
            Task::Run => return self.serve(),
            Task::UpdateConfig => args::updated(self.config.updated()),
            Task::Import { dir, world } => {
                let world = world.unwrap_or_else(|| self.config.general.level_name.clone());
                let file = crate::storage::Place::world(&self.data_dir, &world).file;
                let report = self.import_world(&dir, &world)?;
                args::imported(&world, &file, &report)
            }
            Task::Export { dir, world } => {
                let world = world.unwrap_or_else(|| self.config.general.level_name.clone());
                let report = self.export_world(&world, &dir)?;
                args::exported(&world, &dir, &report)
            }
        };
        args::say(&answer).map_err(Error::Output)
    }

    /// Runs the server ([`run`](Self::run) without a command of the process arguments).
    #[expect(
        clippy::too_many_lines,
        reason = "consistency: one function starts every part of the server in its order"
    )]
    fn serve(self) -> Result<()> {
        let started = clock::now();
        let log = self.console_log().map_err(Error::Start)?;
        info!(
            version = Context::ANCHOR_VERSION,
            minecraft = Context::VERSION,
            protocol = PROTOCOL_VERSION,
            "server starting"
        );
        let Self {
            config,
            data_dir,
            threads,
            setup,
            worlds,
            packs,
            shutdown,
            shutdown_requests,
            console_log: _,
        } = self;
        // Built before any thread starts: encoding the shared spawn batches takes a while once.
        let shared = tick::Shared::new(
            config.batch_settings()?,
            &setup.custom,
            &setup.custom_entities,
        )
        .map_err(|e| Error::Start(std::io::Error::other(e)))?;
        let mut setup = setup;
        setup.fingerprints = tick::Fingerprints::of(&tick::Settings::new(&config), &worlds, &setup);
        // Made first: the start checks the data files on it.
        let pool = runtime::cpu_pool(threads.cpu)?;
        let ready = (|| {
            let mut opened =
                save::open_with(&data_dir, worlds, crate::storage::Start::of(&config), &pool)?;
            setup.data.load(&mut opened.thread)?;
            setup.commands.start(
                std::mem::take(&mut opened.access),
                config.general.white_list,
            );
            opened
                .thread
                .set_lighting(config.developer.lighting.is_calculated());
            Ok::<_, Error>((opened, Runtime::new(threads)?))
        })();
        let (mut opened, runtime) = match ready {
            Ok(ready) => ready,
            Err(e) => {
                // Dropping the pool would not wait for its threads (`Pool::shutdown`).
                pool.shutdown();
                return Err(e);
            }
        };
        // Before the tick starts, so that a stop signal from the first tick on stops normally.
        let signals = stop_signals(&runtime.handle());
        let pool = Arc::new(pool);
        let (inputs, input_queue) = tick::queue::input_queue(config.general.max_players);
        setup.tasks.start(runtime.handle(), &inputs);
        if config.developer.console_input {
            crate::console::start(&inputs).map_err(Error::Start)?;
        }
        let (stop_tick, stop) = mpsc::channel(1);
        let (done, tick_done) = oneshot::channel();
        let (crash, mut crash_log) = oneshot::channel();
        let (requests, request_queue) = mpsc::channel(save::REQUEST_CAPACITY);
        let (responder, responses) = mpsc::channel(save::RESPONSE_CAPACITY);
        let mut state = tick::State::new(
            tick::Settings::new(&config),
            shared,
            setup,
            opened.worlds,
            opened.tick,
        );
        state.open_worlds(opened.catalog);
        state.set_identity(opened.identity);
        let (controls, control_queue) = tick::queue::control_queue();
        state.connect_network(controls);
        state.set_threads(ThreadTotals::new(
            threads,
            config.developer.console_input,
            log.is_some(),
        ));
        opened
            .thread
            .set_custom(Arc::clone(state.custom()), Arc::clone(state.entity_types()));
        let save_pool = Arc::clone(&pool);
        let save_thread = runtime::spawn_save(move || {
            save::run(opened.thread, &save_pool, request_queue, &responder)
        })?;
        let tick_pool = Arc::clone(&pool);
        let link = tick::SaveLink {
            requests,
            responses,
        };
        let tick_thread = runtime::spawn_tick(move || {
            tick::run(state, &tick_pool, input_queue, link, stop, crash, done)
        })?;
        debug!(
            network_threads = threads.network,
            cpu_threads = threads.cpu,
            "threads started"
        );
        let running = Running {
            shutdown,
            shutdown_requests,
            stop_tick,
            tick_done,
            started,
            signals,
            log: log.as_ref().map(console::log::Installed::drained),
        };
        let served =
            runtime.block_on(running.serve(config, packs, &data_dir, inputs, control_queue));
        // `serve` returns only after the tick thread has ended, so this does not wait long.
        let joined = tick_thread.join();
        // The tick has ended: after its `Finish` the save thread commits what it holds and ends;
        // after a tick panic it ends without committing.
        let saved = save_thread.join();
        runtime.shutdown();
        // `Pool::shutdown` warns about threads that did not end.
        if let Ok(pool) = Arc::try_unwrap(pool) {
            pool.shutdown();
        } else {
            warn!("the CPU pool is still shared; its threads are not joined");
        }
        let saved = match saved {
            Ok(result) => result.map_err(Error::from),
            Err(payload) => {
                let message = panic_message(payload.as_ref());
                error!(%message, "save thread panicked");
                Err(Error::Storage(message.into()))
            }
        };
        match joined {
            Ok(tick::End::Stopped) => {
                info!("server stopped");
                served.and(saved)
            }
            Ok(tick::End::SaveFailed(log)) => {
                write_crash(&data_dir, &log);
                Err(saved
                    .err()
                    .unwrap_or_else(|| Error::Storage("the save thread stopped answering".into())))
            }
            Err(payload) => {
                let message = panic_message(payload.as_ref());
                error!(%message, "tick thread panicked; stopped without saving");
                if let Ok(log) = crash_log.try_recv() {
                    write_crash(&data_dir, &log);
                }
                Err(Error::TickPanicked { message })
            }
        }
    }
}

impl Server {
    /// Replays the crash log at `log` (a `data/crash/input-*.log` the server wrote when it
    /// stopped on a panic or a failed save) with this server's settings, worlds and
    /// registrations, and reports how it ended ([`crate::replay`]): it starts from the saved
    /// state of the log's oldest segment whose save `data/main.redb` or one of its backups still
    /// holds, with each other data file's newest copy (the file or a backup) of that save or an
    /// earlier one, and applies the logged ticks. A tick that panics again ends it with
    /// [`End::Panicked`](crate::replay::End::Panicked).
    ///
    /// Build the server as the run that wrote the log was built: the same settings, worlds and
    /// registrations, or the replay is refused. It opens the data files read-only, runs no
    /// network, commits nothing and writes no file. State that listeners, tasks and behaviours
    /// keep in their closures starts over, and their callbacks run again, so what they do outside
    /// the server (files, network) happens again.
    pub fn replay(self, log: impl AsRef<Path>) -> crate::replay::Result<crate::replay::Replay> {
        let _log = self.console_log().map_err(crate::replay::Error::Io)?;
        let Self {
            config,
            data_dir,
            threads,
            setup,
            worlds,
            packs: _,
            shutdown: _,
            shutdown_requests: _,
            console_log: _,
        } = self;
        crate::replay::run(&config, &data_dir, threads, setup, &worlds, log.as_ref())
    }
}

/// Writes the input log since the last commit to `data/crash/`.
fn write_crash(data_dir: &Path, log: &tick::InputLog) {
    match tick::write_crash_log(data_dir, log) {
        Ok(path) => error!(
            path = %path.display(),
            truncated = log.is_truncated(),
            "input log written"
        ),
        Err(e) => error!(error = %e, "input log not written"),
    }
}

/// The parts of a running server that `serve` waits on.
struct Running {
    /// Kept so that `shutdown_requests` stays open while the server runs.
    shutdown: ShutdownHandle,
    shutdown_requests: mpsc::Receiver<()>,
    stop_tick: mpsc::Sender<()>,
    /// Resolves when the tick thread ends, normally or by panic. Polled once at most.
    tick_done: oneshot::Receiver<()>,
    /// When [`Server::run`] started: the uptime counts from here.
    started: Instant,
    /// The stop signals received, by name ([`stop_signals`]).
    signals: mpsc::Receiver<&'static str>,
    /// Anchor's console log, if it runs: a second stop signal waits for its lines.
    log: Option<console::log::Drained>,
}

impl Running {
    /// Starts networking, waits for a shutdown request or the end of the tick thread, then stops
    /// the tick (which disconnects every player) and networking.
    async fn serve(
        self,
        config: Arc<Config>,
        packs: Vec<Pack>,
        data_dir: &Path,
        inputs: InputSender,
        controls: mpsc::Receiver<tick::queue::Control>,
    ) -> Result<()> {
        let Self {
            shutdown,
            mut shutdown_requests,
            stop_tick,
            mut tick_done,
            started,
            signals,
            log,
        } = self;
        tokio::spawn(stop_on_signals(shutdown.clone(), signals, log));
        tokio::spawn(measure_process(inputs.session(SessionId::PROCESS), started));
        let network = match session::start(config, packs, data_dir, inputs, controls).await {
            Ok(network) => network,
            Err(e) => {
                stop(&stop_tick, tick_done).await;
                return Err(e);
            }
        };
        info!("server started");
        let tick_ended = tokio::select! {
            // `None` cannot happen while `shutdown` is alive; both mean stop.
            _ = shutdown_requests.recv() => false,
            _ = &mut tick_done => true,
        };
        if !tick_ended {
            info!("shutting down");
            stop(&stop_tick, tick_done).await;
        }
        network.stop().await;
        drop(shutdown);
        Ok(())
    }
}

/// How often [`measure_process`] measures.
const PROCESS_PERIOD: Duration = Duration::from_secs(1);

/// Measures the process every [`PROCESS_PERIOD`] for `/status` ([`Measure::Process`]) until the
/// tick stops. A measure the full queue refuses is dropped: the next one follows a second later.
async fn measure_process(mut inputs: SessionInputs, started: Instant) {
    let mut every = tokio::time::interval(PROCESS_PERIOD);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        let memory = match tokio::task::spawn_blocking(resident_memory).await {
            Ok(memory) => memory,
            Err(e) => {
                debug!(error = %e, "process measures stopped");
                return;
            }
        };
        let uptime = clock::now().saturating_duration_since(started).as_millis();
        let measure = Measure::Process {
            memory,
            uptime_ms: u64::try_from(uptime).unwrap_or(u64::MAX),
        };
        match inputs.try_send(InputKind::Measure(measure)) {
            Ok(()) => {}
            Err(queue::Error::Full) => {
                debug!("the tick's input queue is full; process measure dropped");
            }
            Err(queue::Error::Closed) => return,
        }
    }
}

/// The process's resident memory in bytes (the working set on Windows), or 0 if the OS does not
/// tell. It reads `/proc/self/statm` on Linux, so it runs on a blocking thread.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: measure_process runs it on a blocking thread"
)]
fn resident_memory() -> u64 {
    memory_stats::memory_stats().map_or(0, |m| u64::try_from(m.physical_mem).unwrap_or(u64::MAX))
}

/// The exit status after a second stop signal: 128 + SIGINT, what shells report for Ctrl+C.
const INTERRUPTED_EXIT: i32 = 130;

/// Listens for the stop signals ([`listen`]) from now on, on `runtime`, and gives them by name.
fn stop_signals(runtime: &tokio::runtime::Handle) -> mpsc::Receiver<&'static str> {
    let (received, signals) = mpsc::channel(1);
    let _runtime = runtime.enter();
    listen(&received);
    signals
}

/// Turns the first stop signal of `signals` into a shutdown request and ends the process at the
/// second, as PMMP does (it drops its handlers after the first): an operator whose stop hangs can
/// still end it, with the result of a crash (the last commit stays).
async fn stop_on_signals(
    shutdown: ShutdownHandle,
    mut signals: mpsc::Receiver<&'static str>,
    log: Option<console::log::Drained>,
) {
    let Some(signal) = signals.recv().await else {
        return;
    };
    info!(signal, "stop signal received; shutting down");
    shutdown.shutdown();
    if let Some(signal) = signals.recv().await {
        exit_interrupted(signal, log.as_ref()).await;
    }
}

/// How long the exit after a second stop signal waits for the console log to write its lines.
const EXIT_LOG_WAIT: Duration = Duration::from_millis(500);

/// Ends the process after the second stop signal `signal`, once `log` (Anchor's console log, if
/// it runs) has written the lines queued until then, or after [`EXIT_LOG_WAIT`]: `exit` ends the
/// log thread with the others, and a console that takes no writes must not hold up the exit.
#[expect(
    clippy::exit,
    reason = "safety: a second stop signal ends the process, as the OS would without a handler"
)]
async fn exit_interrupted(signal: &str, log: Option<&console::log::Drained>) -> ! {
    error!(signal, "second stop signal; exiting without the final save");
    if let Some(log) = log {
        match tokio::time::timeout(EXIT_LOG_WAIT, log.wait()).await {
            // Past the wait the lines still queued are lost, as after a crash.
            Ok(()) | Err(tokio::time::error::Elapsed { .. }) => {}
        }
    }
    std::process::exit(INTERRUPTED_EXIT);
}

/// Starts a task per stop signal that forwards each one to `received` by name.
macro_rules! forward {
    ($received:expr, $name:literal, $listener:expr) => {
        match $listener {
            Ok(mut listener) => {
                let received = $received.clone();
                tokio::spawn(async move {
                    while listener.recv().await.is_some() {
                        if received.send($name).await.is_err() {
                            return;
                        }
                    }
                });
            }
            Err(e) => warn!(signal = $name, error = %e, "cannot listen for the signal"),
        }
    };
}

/// The stop signals on Unix: Ctrl+C, SIGTERM (`kill`, service managers) and SIGHUP (the
/// terminal closed), as PMMP handles them. SIGHUP is not listened for when it is ignored as the
/// server starts (`nohup`): the operator asked a closed terminal to leave the process running,
/// and a handler would replace that disposition. If that cannot be read, SIGHUP is listened for,
/// since with the default disposition a closed terminal would end the process without the final
/// save.
#[cfg(unix)]
fn listen(received: &mpsc::Sender<&'static str>) {
    use tokio::signal::unix::{SignalKind, signal};
    let hangup_ignored = hangup_ignored().unwrap_or_else(|e| {
        warn!(error = %e, "cannot tell whether SIGHUP is ignored; SIGHUP stops the server");
        false
    });
    forward!(received, "SIGINT", signal(SignalKind::interrupt()));
    forward!(received, "SIGTERM", signal(SignalKind::terminate()));
    if !hangup_ignored {
        forward!(received, "SIGHUP", signal(SignalKind::hangup()));
    }
}

/// The most bytes `/proc/self/status` is read up to: it holds a few dozen short lines.
#[cfg(unix)]
const PROCESS_STATUS_LIMIT_BYTES: u64 = 64 * 1024;

/// Whether SIGHUP is ignored now, from the `SigIgn` mask of `/proc/self/status` (Linux).
#[cfg(unix)]
fn hangup_ignored() -> std::io::Result<bool> {
    use std::io::{Error, ErrorKind};
    let status =
        crate::fs::read_bounded(Path::new("/proc/self/status"), PROCESS_STATUS_LIMIT_BYTES)?;
    let mask = status
        .split(|&byte| byte == b'\n')
        .find_map(|line| line.strip_prefix(b"SigIgn:"))
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "no SigIgn line"))?;
    // The mask is hexadecimal with signal 1 (SIGHUP) as the lowest bit of the last digit.
    let last_digit = mask
        .trim_ascii()
        .last()
        .and_then(|&digit| char::from(digit).to_digit(16))
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "the SigIgn mask is not hexadecimal"))?;
    Ok(last_digit & 1 == 1)
}

/// The stop signals on Windows: Ctrl+C, Ctrl+Break, and the console's close and shutdown events.
/// After those two Windows ends the process about 5 seconds later, whether the stop has finished
/// or not. The logoff event is not a stop: only a process run as a service receives it, at any
/// user's logoff, and the default handler that runs without a listener keeps a service running.
#[cfg(windows)]
fn listen(received: &mpsc::Sender<&'static str>) {
    use tokio::signal::windows;
    forward!(received, "Ctrl+C", windows::ctrl_c());
    forward!(received, "Ctrl+Break", windows::ctrl_break());
    forward!(received, "close", windows::ctrl_close());
    forward!(received, "shutdown", windows::ctrl_shutdown());
}

/// Asks the tick thread to stop and waits until it has ended.
async fn stop(stop_tick: &mpsc::Sender<()>, tick_done: oneshot::Receiver<()>) {
    match stop_tick.try_send(()) {
        // Full: a stop is already queued. Closed: the tick thread has already ended.
        Ok(()) | Err(TrySendError::Full(()) | TrySendError::Closed(())) => {}
    }
    match tick_done.await {
        // The thread never sends; the sender dropping when it ends is the signal.
        Ok(()) | Err(oneshot::error::RecvError { .. }) => {}
    }
}

/// The text of a panic payload.
pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a payload that is not text".to_owned())
}

impl ServerBuilder {
    /// Uses `config` instead of [`Config::default`]. Without this call Anchor reads and writes no
    /// settings file.
    #[must_use]
    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// Uses `path` for the world, the authorization cache, crash reports and the resource pack
    /// folder `packs` instead of `data`.
    #[must_use]
    pub fn data_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.data_dir = path.into();
        self
    }

    /// Turns Anchor's console log off, whatever `log-level` says, so that the program's own
    /// `tracing` subscriber gets Anchor's events.
    ///
    /// Anchor's console log is no global subscriber: while [`Server::run`],
    /// [`Server::import_world`] or [`Server::replay`] runs, it is the default of the calling
    /// thread and of every thread Anchor starts, and writes one line per event on standard error
    /// (UTC time, level, message and fields; control characters escaped, Minecraft format codes
    /// left out). A thread of its own writes the lines, so a console that stops taking them never
    /// holds up the server: lines it cannot keep up with are left out and counted in the log.
    /// Threads of the program keep their own default. Without it, Anchor's events go to the
    /// program's global subscriber (`tracing::subscriber::set_global_default`), if any.
    #[must_use]
    pub fn without_console_log(mut self) -> Self {
        self.console_log = false;
        self
    }

    /// Calls `listener` on every `E` event at `priority`, in the tick, after the listeners of
    /// lower priority and those added earlier at the same priority. The listener may change the
    /// event and, through the [`Context`], the game; its calls raise no events
    /// ([`event`] explains the order). A [replay](Server::replay) calls it again
    /// as the replaying server built it: state it captures starts over, and what it does
    /// outside the server happens again.
    #[doc(alias = "PluginManager::registerEvent")]
    #[must_use]
    pub fn on<E: event::Event>(
        mut self,
        priority: Priority,
        listener: impl FnMut(&mut E, &mut Context<'_>) + Send + 'static,
    ) -> Self {
        self.setup.listeners.push(priority, listener);
        self
    }

    /// Calls `monitor` on every `E` event after its outcome is applied, with shared access only,
    /// in registration order (PMMP's `MONITOR` priority, enforced by the types).
    #[doc(alias = "EventPriority::MONITOR")]
    #[must_use]
    pub fn monitor<E: event::Event>(
        mut self,
        monitor: impl FnMut(&E, &Context<'_>) + Send + 'static,
    ) -> Self {
        self.setup.listeners.push_monitor(monitor);
        self
    }

    /// Runs `task` in the tick every `period_ticks` ticks (on ticks that are multiples of it),
    /// after the tick's inputs and in registration order, with a [`Context`] that reaches the
    /// worlds. Like listeners, tasks run only in the tick, so a [replay](Server::replay) runs
    /// them the same way, the state they capture starting over. This is the build-time way,
    /// fixed for the server's life; the runtime
    /// one, counted from its call and cancellable, is
    /// [`Context::schedule_repeating`] (PMMP `TaskScheduler::scheduleRepeatingTask`).
    #[must_use]
    pub fn every(
        mut self,
        period_ticks: NonZeroU32,
        task: impl FnMut(&mut Context<'_>) + Send + 'static,
    ) -> Self {
        self.setup.tasks.push(period_ticks, task);
        self
    }

    /// Adds a world besides the default one (`level-name`), loaded from the start. A
    /// [`Kind::Persistent`] world is saved in its file `data/worlds/<name>.redb`; a
    /// [`Kind::Temporary`] one is generated fresh at every start and never saved. Its seed is
    /// made from its name at every start. When the world is new, its spawn is the generator's,
    /// or `spawn` of its `[worlds.<name>]` table; a persistent world's file then keeps its spawn.
    /// Players join the default world. Its generator is registered as
    /// [`generator`](Self::generator) registers one.
    #[doc(alias = "WorldManager::generateWorld")]
    #[must_use]
    pub fn world(mut self, name: impl Into<String>, kind: Kind, generator: impl Generator) -> Self {
        let generator = self.setup.generators.add_typed(generator);
        self.worlds.push(WorldSpec {
            name: name.into(),
            kind,
            generator,
            seed: None,
            spawn: None,
        });
        self
    }

    /// Sets the view values of the world `name` as a `[worlds.<name>]` table of the settings
    /// does (each value set replaces the global one in that world), into the settings this
    /// builder holds: a later [`config`](Self::config) replaces them. The build refuses a value
    /// out of its key's range and a name that is not one of the server's worlds.
    #[must_use]
    pub fn world_settings(
        mut self,
        name: impl Into<String>,
        settings: crate::config::WorldSettings,
    ) -> Self {
        self.config.worlds.insert(name.into(), settings);
        self
    }

    /// Registers `generator` under its [`name`](Generator::name), for the worlds
    /// [`Context::create_world`] makes and the stored worlds [`Context::load_world`] loads (PMMP
    /// `GeneratorManager::addGenerator`). The built-in `flat` and `void` are always registered.
    /// Two generators of one type with one name count as one; two types with one name fail the
    /// build with [`Error::DuplicateGenerator`].
    #[doc(alias = "GeneratorManager::addGenerator")]
    #[must_use]
    pub fn generator(mut self, generator: impl Generator) -> Self {
        self.setup.generators.add_typed(generator);
        self
    }

    /// Checks the settings, the thread budget and the world names (those of `[worlds.<name>]`
    /// included), and puts the listeners in calling order. Touches no file.
    pub fn build(mut self) -> Result<Server> {
        self.config.validate()?;
        let developer = &self.config.developer;
        let threads = ThreadCounts::resolve(
            developer.network_threads,
            developer.cpu_threads,
            runtime::logical_cpus(),
        )?;
        self.setup.commands.finish().map_err(Error::Command)?;
        if !self.config.general.vanilla_recipes {
            self.setup.custom.drop_vanilla_recipes();
        }
        if !self.config.general.recipe_book {
            self.setup.custom.hide_recipe_book();
        }
        self.setup.custom.finish().map_err(Error::Custom)?;
        self.setup
            .custom_entities
            .finish()
            .map_err(Error::CustomEntity)?;
        self.setup.data.finish().map_err(Error::Data)?;
        let general = &self.config.general;
        if let Some(name) = self.setup.generators.clash() {
            return Err(Error::DuplicateGenerator {
                name: name.to_owned(),
            });
        }
        let generator: Arc<dyn Generator> = match general.level_type {
            LevelType::Flat => Arc::new(Flat),
            LevelType::Void => Arc::new(Void),
        };
        let mut worlds = Vec::with_capacity(self.worlds.len() + 1);
        worlds.push(WorldSpec {
            name: general.level_name.clone(),
            kind: Kind::Persistent,
            generator,
            seed: general.level_seed,
            spawn: None,
        });
        for world in self.worlds {
            // Names that differ only in letter case would be one file.
            let key = crate::storage::world_name_key(&world.name);
            if worlds
                .iter()
                .any(|w| crate::storage::world_name_key(&w.name) == key)
            {
                return Err(Error::DuplicateWorld { name: world.name });
            }
            worlds.push(world);
        }
        if let Some(world) = worlds
            .iter()
            .find(|w| !crate::storage::is_world_file_name(&w.name))
        {
            return Err(Error::InvalidWorldName {
                name: world.name.clone(),
            });
        }
        self.config
            .check_world_names(|name| worlds.iter().any(|w| w.name == name))?;
        for world in &mut worlds {
            world.spawn = self.config.worlds.get(&world.name).and_then(|w| w.spawn);
        }
        let (requests, shutdown_requests) = mpsc::channel(1);
        Ok(Server {
            config: Arc::new(self.config),
            data_dir: self.data_dir,
            threads,
            setup: self.setup,
            worlds,
            packs: self.packs,
            shutdown: ShutdownHandle(requests),
            shutdown_requests,
            console_log: self.console_log,
        })
    }
}

impl ShutdownHandle {
    /// Asks the server to stop: every player sees `shutdown-message`, then [`Server::run`]
    /// returns. Returns at once; repeated calls, and calls after the server stopped, do nothing.
    #[doc(alias = "Server::shutdown")]
    pub fn shutdown(&self) {
        match self.0.try_send(()) {
            // Full: a request is already pending. Closed: the server has already stopped.
            Ok(()) | Err(TrySendError::Full(()) | TrySendError::Closed(())) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn argument_commands_answer_without_starting_the_server() {
        let data = save::tests::TempDir::new("argument-commands");
        let server = || {
            Server::builder()
                .data_dir(&data.0)
                .without_console_log()
                .build()
                .unwrap()
        };
        // `Config::load` did the update; the server only reports it, and does not start.
        server().run_task(Task::UpdateConfig).unwrap();
        assert!(!data.0.join("main.redb").exists());

        let dir = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/xtask/data/import/flat"
        ));
        let import = |world: Option<&str>| {
            server().run_task(Task::Import {
                dir: dir.clone(),
                world: world.map(str::to_owned),
            })
        };
        // Without a name, as the default world.
        import(None).unwrap();
        assert!(data.0.join("worlds").join("world.redb").exists());
        import(Some("lobby")).unwrap();
        assert!(data.0.join("worlds").join("lobby.redb").exists());
        let again = import(Some("LOBBY")).unwrap_err();
        assert!(
            matches!(
                again,
                Error::Import(crate::import::Error::WorldExists { .. })
            ),
            "{again:?}"
        );

        let out = data.0.join("exported");
        let export = |world: Option<&str>, dir: PathBuf| {
            server().run_task(Task::Export {
                dir,
                world: world.map(str::to_owned),
            })
        };
        // Without a name, the default world.
        export(None, out.join("world")).unwrap();
        assert!(out.join("world").join("level.dat").exists());
        export(Some("LOBBY"), out.join("lobby")).unwrap();
        assert!(out.join("lobby").join("db").join("CURRENT").exists());
        let again = export(Some("lobby"), out.join("lobby")).unwrap_err();
        assert!(
            matches!(
                again,
                Error::Export(crate::export::Error::FolderExists { .. })
            ),
            "{again:?}"
        );
        assert!(!data.0.join("main.redb").exists());
    }

    /// Set for the child process of
    /// `a_second_stop_signal_is_logged_before_the_process_ends`.
    const SIGNAL_CHILD_ENV: &str = "ANCHOR_SECOND_SIGNAL_CHILD";

    /// The process a second stop signal ends, under Anchor's console log on standard error.
    /// Without the parent's environment it does nothing.
    #[test]
    #[ignore = "the child process of a_second_stop_signal_is_logged_before_the_process_ends"]
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the child owns a small runtime"
    )]
    fn second_signal_child() {
        if std::env::var_os(SIGNAL_CHILD_ENV).is_none() {
            return;
        }
        let log = console::log::start(crate::config::LogLevel::Info)
            .unwrap()
            .unwrap()
            .install();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(exit_interrupted("test", Some(&log.drained())));
    }

    /// The line about the skipped final save reaches standard error before the process ends.
    #[test]
    fn a_second_stop_signal_is_logged_before_the_process_ends() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "server::tests::second_signal_child",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(SIGNAL_CHILD_ENV, "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(INTERRUPTED_EXIT), "{stderr}");
        assert!(
            stderr.contains("ERROR second stop signal; exiting without the final save"),
            "{stderr}"
        );
    }

    /// [`ServerBuilder::world_settings`] sets a world's `[worlds.<name>]` values, checked by the
    /// build as the file's are.
    #[test]
    fn world_settings_are_the_worlds_table_of_the_settings() {
        let spawn = crate::world::BlockPos::new(3, -50, -7);
        let own = crate::config::WorldSettings {
            view_distance: Some(6),
            spawn: Some(spawn),
            ..crate::config::WorldSettings::default()
        };
        let server = Server::builder()
            .world("arena", Kind::Temporary, Flat)
            .world_settings("arena", own)
            .build()
            .unwrap();
        assert_eq!(server.config.worlds.get("arena"), Some(&own));
        // The spawn reaches the world's spec, which every start makes the world from.
        let spawns: Vec<_> = server
            .worlds
            .iter()
            .map(|w| (w.name.as_str(), w.spawn))
            .collect();
        assert_eq!(spawns, [("world", None), ("arena", Some(spawn))]);
        let bad = crate::config::WorldSettings {
            view_distance: Some(1),
            ..crate::config::WorldSettings::default()
        };
        let refused = Server::builder()
            .world("arena", Kind::Temporary, Flat)
            .world_settings("arena", bad)
            .build();
        assert!(matches!(refused, Err(Error::Config(_))));
        let undeclared = Server::builder().world_settings("nowhere", own).build();
        assert!(matches!(undeclared, Err(Error::Config(_))));
    }

    /// `vanilla-recipes = false` and `recipe-book = false` reach the recipes the server builds.
    #[test]
    fn the_recipe_switches_reach_the_built_recipes() {
        let built = |vanilla: bool, book: bool| {
            let mut config = Config::default();
            config.general.vanilla_recipes = vanilla;
            config.general.recipe_book = book;
            Server::builder()
                .config(config)
                .build()
                .unwrap()
                .setup
                .custom
        };
        let on = built(true, true);
        assert!(on.first_special_id() > 1 && on.has_recipe_book());
        let off = built(false, false);
        assert_eq!(off.first_special_id(), 1);
        assert!(!off.has_recipe_book());
    }

    #[test]
    fn a_panicking_tick_thread_is_joined_with_its_message() {
        let thread = runtime::spawn_tick(|| panic!("tick {} failed", 7)).unwrap();
        let payload = thread.join().unwrap_err();
        assert_eq!(panic_message(payload.as_ref()), "tick 7 failed");
        let thread = runtime::spawn_tick(|| std::panic::panic_any(5_u8)).unwrap();
        let payload = thread.join().unwrap_err();
        assert_eq!(
            panic_message(payload.as_ref()),
            "a payload that is not text"
        );
        assert_eq!(panic_message(&"static"), "static");
    }

    /// The process measure comes on its own session with the memory the OS reports and the
    /// uptime since the start, and stops once the tick's queue is gone.
    #[test]
    fn the_process_is_measured_every_second_until_the_tick_stops() {
        let runtime = Runtime::new(ThreadCounts { network: 1, cpu: 1 }).unwrap();
        let (inputs, mut queue) = queue::input_queue(1);
        let started = clock::now().checked_sub(Duration::from_secs(5)).unwrap();
        runtime.block_on(async move {
            let task = tokio::spawn(measure_process(inputs.session(SessionId::PROCESS), started));
            for seq in 0..2 {
                let input = queue.recv().await.unwrap();
                assert_eq!((input.session, input.seq), (SessionId::PROCESS, seq));
                let InputKind::Measure(Measure::Process { memory, uptime_ms }) = input.kind else {
                    panic!("{:?}", input.kind);
                };
                // A test process holds more than a megabyte.
                assert!(memory > 1 << 20, "{memory}");
                assert!((5_000..60_000).contains(&uptime_ms), "{uptime_ms}");
            }
            drop(queue);
            task.await.unwrap();
        });
        runtime.shutdown();
    }
}
