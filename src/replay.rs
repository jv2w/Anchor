//! Replaying a crash log: [`Server::replay`](crate::Server::replay)
//! runs the ticks a crashed server applied again, from a saved state, with the same build.
//!
//! A crash log (`data/crash/input-*.log`) holds segments: the inputs of every tick from a state
//! on, and a checkpoint of that state. The replay starts from the oldest segment whose save a
//! main data file still holds (`data/main.redb`, then its backups newest first, each opened
//! read-only and picked by the tick and commit identity of its last commit, so a file another run
//! committed at the same tick is not taken for it), preferring one after the log's last
//! truncation, and applies the logged ticks in order to the end of the log or to its first
//! truncation. Every other data file (players, each world) is the newest of
//! its copies (the file, then its backups) whose last commit is that save or an earlier one: a
//! save writes only the files that changed. Chunks load from those files and the generators as
//! they did, checked against the logged checksums; the server's own snapshots are committed to
//! memory on top of the files, so a later load sees them as the live one did. Every later
//! segment's checkpoint is compared with the replayed state.
//!
//! A data folder whose players file or a world file holds a later save than its main file (a
//! save stopped before the main file) is refused as [`Error::Storage`]: one start of the server
//! rolls such files back to the main file's save.
//!
//! A replay runs no network and no runtime, commits nothing and writes no file. Blocking jobs
//! do not run: their logged results are handed to their callbacks. The listeners, tasks and
//! behaviours are the replaying server's: state they keep in their closures starts over, and
//! what they do outside the server (files, network) happens again. A checkpoint that holds a
//! closure (a scheduled task, a waiting form or job, an open menu, a deferred callback) or a
//! player's extension cannot be replayed from ([`Error::Opaque`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tracing::{error, info};

use crate::config::Config;
use crate::pool::Pool;
use crate::runtime::{self, ThreadCounts};
use crate::save::{self, Loaded, Request, Response, WorldSpec};
use crate::storage::codec::Meta;
use crate::tick::checkpoint::Checkpoint;
use crate::tick::queue::{Input, InputKind, Outbound, SessionId, outbound_queue};
use crate::tick::{self, Answer, EntryKind, Fingerprints, InputLog, Origin, Segment, Setup, State};

/// What a replay did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    base_tick: u64,
    last_tick: Option<u64>,
    ticks: u64,
    end: End,
}

impl Replay {
    /// The engine tick of the state the replay started from: the first tick it applied.
    pub fn base_tick(&self) -> u64 {
        self.base_tick
    }

    /// The last tick it applied (the one that panicked, for [`End::Panicked`]); `None` if it
    /// applied none.
    pub fn last_tick(&self) -> Option<u64> {
        self.last_tick
    }

    /// How many ticks it applied, the one that panicked included.
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// How it ended.
    pub fn end(&self) -> &End {
        &self.end
    }
}

/// How a replay ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// Every logged tick applied.
    Completed,
    /// The log stopped recording, at its bound or at a save thread answer it could not log; the
    /// replay applied every tick before that.
    Truncated,
    /// A tick panicked again, as it may have in the run that wrote the log.
    Panicked {
        /// The tick that panicked.
        tick: u64,
        /// The panic's message.
        message: String,
    },
}

/// A replay that could not run.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The log or a thread could not be read or started.
    Io(std::io::Error),
    /// The file is not a crash log Anchor can read, or one of its checkpoints is damaged.
    Format(Box<dyn std::error::Error + Send + Sync>),
    /// The server's settings or worlds differ from those of the run that wrote the log.
    SettingsDiffer,
    /// What the server registers (commands, custom content, listeners, tasks, data types,
    /// hooks) differs from what the run that wrote the log registered.
    RegistrationsDiffer,
    /// No main data file (the file or a backup) holds the save of a segment of the log, or
    /// another data file has no copy of that save or an earlier one.
    NoMatchingWorld,
    /// The segments the data files hold have checkpoints that hold what cannot be replayed.
    Opaque {
        /// The tick of the segment's state.
        tick: u64,
        /// What the checkpoint holds.
        what: String,
    },
    /// The replay left the logged run: a chunk load read other bytes, a snapshot came at
    /// another tick, or the state at a later segment differs from its checkpoint.
    Diverged {
        /// The tick where it was seen.
        tick: u64,
    },
    /// A data file failed.
    Storage(Box<dyn std::error::Error + Send + Sync>),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => f.write_str("the crash log or a thread could not be read or started"),
            Self::Format(_) => f.write_str("not a crash log this server can read"),
            Self::SettingsDiffer => f.write_str("the settings differ from the logged run's"),
            Self::RegistrationsDiffer => {
                f.write_str("the registrations differ from the logged run's")
            }
            Self::NoMatchingWorld => {
                f.write_str("no data files hold the save of a segment of the log")
            }
            Self::Opaque { tick, what } => write!(
                f,
                "the checkpoint of tick {tick} cannot be replayed from: it holds {what}"
            ),
            Self::Diverged { tick } => write!(f, "the replay left the logged run at tick {tick}"),
            Self::Storage(_) => f.write_str("a data file failed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Format(e) | Self::Storage(e) => Some(e.as_ref()),
            Self::SettingsDiffer
            | Self::RegistrationsDiffer
            | Self::NoMatchingWorld
            | Self::Opaque { .. }
            | Self::Diverged { .. } => None,
        }
    }
}

/// A result with the module's [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// How far the replay got, sent to the main thread if a tick panics.
struct Progress {
    tick: Option<u64>,
    ticks: u64,
    done: Option<oneshot::Sender<(Option<u64>, u64)>>,
}

impl Drop for Progress {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(done) = self.done.take()
            && done.send((self.tick, self.ticks)).is_err()
        {
            error!("the replay's progress could not be handed over");
        }
    }
}

/// The replay on its tick thread.
struct Replayer {
    state: State,
    /// The data files, read-only, with the replay's commits in memory.
    disk: save::Thread,
    pool: Pool,
    /// The replay's own log: its checkpoints are compared with the replayed log's.
    own: InputLog,
    /// The outbound queues of the players, drained after each tick.
    receivers: BTreeMap<SessionId, mpsc::Receiver<Outbound>>,
    progress: Progress,
}

impl Replayer {
    /// Applies the ticks of `log` from segment `from` on; shuts the pool down at the end.
    fn run(mut self, log: &InputLog, from: usize) -> Result<(Option<u64>, u64, End)> {
        let result = self.ticks(log, from);
        let Self { pool, progress, .. } = self;
        // `Pool::shutdown` warns about threads that did not end.
        pool.shutdown();
        let end = result?;
        Ok((progress.tick, progress.ticks, end))
    }

    fn ticks(&mut self, log: &InputLog, from: usize) -> Result<End> {
        for (i, segment) in log.segments().enumerate().skip(from) {
            if i > from {
                self.compare(segment)?;
            }
            for t in segment.ticks() {
                self.progress.tick = Some(t.number);
                if t.number != tick::tick_of(&self.state) {
                    return Err(Error::Diverged { tick: t.number });
                }
                let mut inputs = self.inputs(t);
                let mut responses = self.responses(t)?;
                self.progress.ticks += 1;
                let requests = tick::replay_tick(
                    &mut self.state,
                    &mut inputs,
                    &mut responses,
                    &t.dropped,
                    &self.pool,
                    &mut self.own,
                );
                for rx in self.receivers.values_mut() {
                    while rx.try_recv().is_ok() {}
                }
                for request in requests {
                    match request {
                        Request::Commit(snapshot) => {
                            self.disk.commit(&self.pool, &snapshot).map_err(storage)?;
                        }
                        // The logged answers say which worlds did not open.
                        Request::Worlds(worlds) => drop(self.disk.add_worlds(&worlds)),
                        Request::DeleteWorlds(worlds) => self.disk.delete_worlds(&worlds),
                        Request::Chunks(_)
                        | Request::Player(_)
                        | Request::Discard(_)
                        | Request::Finish => {}
                    }
                }
                drop(self.own.take_discarded());
            }
            if segment.is_truncated() {
                return Ok(End::Truncated);
            }
        }
        Ok(End::Completed)
    }

    /// Checks that the replay started `segment` too, at its tick and with its state.
    fn compare(&self, segment: &Segment) -> Result<()> {
        let tick = segment.start();
        let own = self.own.last_segment();
        if own.start() != tick {
            return Err(Error::Diverged { tick });
        }
        match (segment.checkpoint(), own.taken()) {
            (Some(logged), Some(replayed)) if logged != replayed.to_bytes().as_slice() => {
                Err(Error::Diverged { tick })
            }
            _ => Ok(()),
        }
    }

    /// The logged inputs of tick `t` as the sessions gave them; a join gets a new outbound queue.
    fn inputs(&mut self, t: &tick::LoggedTick) -> Vec<Input> {
        t.inputs
            .iter()
            .map(|e| {
                let kind = match &e.kind {
                    EntryKind::Join(identity) => {
                        let (tx, rx) = outbound_queue();
                        self.receivers.insert(e.session, rx);
                        InputKind::Join {
                            identity: identity.clone(),
                            outbound: tx,
                        }
                    }
                    // The size only counts against the replay's own log's bound.
                    EntryKind::Packet(p) => InputKind::Packet {
                        packet: p.clone(),
                        size: 0,
                    },
                    EntryKind::Leave(reason) => InputKind::Leave(*reason),
                    EntryKind::Console(line) => InputKind::Console(line.clone()),
                    EntryKind::Job(outcome) => InputKind::Job(outcome.clone()),
                    EntryKind::Measure(measure) => InputKind::Measure(*measure),
                };
                Input {
                    session: e.session,
                    seq: e.seq,
                    kind,
                }
            })
            .collect()
    }

    /// The save thread's logged answers of tick `t`: chunks loaded again from the data files and
    /// the generators, each checked against its logged origin and checksum.
    fn responses(&mut self, t: &tick::LoggedTick) -> Result<Vec<Response>> {
        let keys: Vec<_> = t
            .responses
            .iter()
            .filter_map(|a| match a {
                Answer::Chunk {
                    world, pos, origin, ..
                } if *origin != Origin::Damaged => Some((*world, *pos)),
                _ => None,
            })
            .collect();
        let mut loaded: BTreeMap<_, Loaded> = BTreeMap::new();
        if !keys.is_empty() {
            for answer in self.disk.load(&self.pool, &keys).map_err(storage)? {
                if let Response::Chunk {
                    world,
                    pos,
                    loaded: l,
                } = answer
                {
                    loaded.insert((world, pos), l);
                }
            }
        }
        let diverged = Error::Diverged { tick: t.number };
        let mut out = Vec::with_capacity(t.responses.len());
        for answer in &t.responses {
            out.push(match answer {
                Answer::Chunk {
                    world,
                    pos,
                    origin,
                    checksum,
                } => {
                    let loaded = match origin {
                        Origin::Damaged => Loaded::Damaged,
                        Origin::Disk | Origin::Generated => {
                            let l = loaded
                                .remove(&(*world, *pos))
                                .ok_or(Error::Diverged { tick: t.number })?;
                            let matches = match &l {
                                Loaded::Disk(_, s) => {
                                    *origin == Origin::Disk && s.checksum == *checksum
                                }
                                Loaded::Generated(_, s) => {
                                    *origin == Origin::Generated && s.checksum == *checksum
                                }
                                Loaded::Damaged => false,
                            };
                            if !matches {
                                return Err(diverged);
                            }
                            l
                        }
                    };
                    Response::Chunk {
                        world: *world,
                        pos: *pos,
                        loaded,
                    }
                }
                Answer::Player { key, data } => Response::Player {
                    key: crate::storage::PlayerKey::from_text(key),
                    data: data.to_load().map_err(format)?,
                },
                Answer::Committed(start) => Response::Committed { start: *start },
                Answer::CommitFailed(start) => Response::CommitFailed { start: *start },
                Answer::LoadFailed => Response::LoadFailed,
                Answer::WorldOpenFailed(world) => Response::WorldOpenFailed { world: *world },
                Answer::Checked(found) => Response::Checked(found.clone()),
            });
        }
        Ok(out)
    }
}

/// Replays the crash log at `path` with the server's settings, worlds and registrations
/// ([`Server::replay`](crate::Server::replay)).
pub(crate) fn run(
    config: &Config,
    data_dir: &Path,
    threads: ThreadCounts,
    mut setup: Setup,
    worlds: &[WorldSpec],
    path: &Path,
) -> Result<Replay> {
    let settings = tick::Settings::new(config);
    let fingerprints = Fingerprints::of(&settings, worlds, &setup);
    let log = read(path)?;
    let logged = log.fingerprints();
    if logged.settings != fingerprints.settings {
        return Err(Error::SettingsDiffer);
    }
    if logged.registrations != fingerprints.registrations {
        return Err(Error::RegistrationsDiffer);
    }
    check_settled(data_dir)?;
    let (from, file, checkpoint) = choose(&log, data_dir, &setup)?;
    info!(
        path = %file.main.display(),
        tick = checkpoint.tick(),
        "replaying the crash log"
    );
    let shared = tick::Shared::new(
        config.batch_settings().map_err(format)?,
        &setup.custom,
        &setup.custom_entities,
    )
    .map_err(format)?;
    let seeds: Vec<i64> = checkpoint.seeds().collect();
    let specs = checkpoint_worlds(&checkpoint, &setup)?;
    let mut opened = save::open_replica(&file, specs, &seeds).map_err(storage)?;
    let identity = opened.identity;
    setup.data.load(&mut opened.thread).map_err(storage)?;
    setup.commands.start(
        std::mem::take(&mut opened.access),
        config.general.white_list,
    );
    opened
        .thread
        .set_lighting(config.developer.lighting.is_calculated());
    let pool = runtime::cpu_pool(threads.cpu).map_err(start)?;
    let mut state = State::new(settings, shared, setup, opened.worlds, opened.tick);
    let mut disk = opened.thread;
    disk.set_custom(Arc::clone(state.custom()), Arc::clone(state.entity_types()));
    let base_tick = checkpoint.tick();
    let extras = extras(&mut disk, &pool, &checkpoint)?;
    let mut receivers = BTreeMap::new();
    state
        .restore(&checkpoint, extras, |session| {
            let (tx, rx) = outbound_queue();
            receivers.insert(session, rx);
            tx
        })
        .map_err(format)?;
    let planned: BTreeSet<u64> = log.segments().skip(from + 1).map(Segment::start).collect();
    let own = InputLog::replaying(checkpoint, identity, planned);
    let (done, mut progress) = oneshot::channel();
    let replayer = Replayer {
        state,
        disk,
        pool,
        own,
        receivers,
        progress: Progress {
            tick: None,
            ticks: 0,
            done: Some(done),
        },
    };
    let thread = runtime::spawn_tick(move || replayer.run(&log, from)).map_err(start)?;
    let (last_tick, ticks, end) = match thread.join() {
        Ok(result) => result?,
        Err(payload) => {
            let message = crate::server::panic_message(payload.as_ref());
            let (tick, ticks) = progress.try_recv().unwrap_or((None, 0));
            error!(%message, ?tick, "the replayed tick panicked");
            let end = End::Panicked {
                tick: tick.unwrap_or(base_tick),
                message,
            };
            (tick, ticks, end)
        }
    };
    Ok(Replay {
        base_tick,
        last_tick,
        ticks,
        end,
    })
}

/// The worlds the run had at `checkpoint`, the ones it made or loaded included, each with the
/// generator `setup` gives it for the name it was made with ([`crate::world::Generators::of_world`]), as the
/// run picked it.
fn checkpoint_worlds(checkpoint: &Checkpoint, setup: &Setup) -> Result<Vec<WorldSpec>> {
    checkpoint
        .worlds()
        .iter()
        .map(|w| {
            Ok(WorldSpec {
                name: w.name.clone(),
                kind: w.kind,
                generator: Arc::clone(
                    setup
                        .generators
                        .of_world(&w.name, &w.generator)
                        .ok_or(Error::RegistrationsDiffer)?,
                ),
                seed: None,
                spawn: None,
            })
        })
        .collect()
}

/// Reads the crash log at `path`.
fn read(path: &Path) -> Result<InputLog> {
    let bytes = crate::fs::read_bounded(path, tick::MAX_LOG_FILE_BYTES as u64).map_err(|e| {
        if e.kind() == io::ErrorKind::FileTooLarge {
            Error::Format("the file is larger than a crash log can be".into())
        } else {
            Error::Io(e)
        }
    })?;
    InputLog::decode(&bytes).map_err(format)
}

/// Refuses a data folder whose players file or a world file holds a later save than its main
/// file ([`crate::storage::check_settled`]): a save stopped before the main file, which only a
/// start of the server rolls back. A file that does not open read-only is left to the choice of
/// copies, which skips it.
fn check_settled(data_dir: &Path) -> Result<()> {
    use crate::storage::{MAIN_FILE, PLAYERS_FILE, Place};
    let main = Place::data(data_dir, MAIN_FILE).file;
    let worlds = save::world_files(data_dir).map_err(storage)?;
    let files = std::iter::once(Place::data(data_dir, PLAYERS_FILE).file)
        .chain(worlds.iter().map(|name| Place::world(data_dir, name).file));
    for file in files {
        if let Err(e @ crate::storage::Error::Ahead) = crate::storage::check_settled(&file, &main) {
            return Err(storage(e));
        }
    }
    Ok(())
}

/// The segment to start from (its place among the log's), its data files and its checkpoint:
/// the oldest segment with a checkpoint whose save a main data file holds, after the log's last
/// truncation if one is; refuses an opaque checkpoint. A file holds a segment's state if its last
/// commit is of the segment's tick and has the segment's commit identity, unless either identity
/// is unknown (a file no save wrote).
fn choose(
    log: &InputLog,
    data_dir: &Path,
    setup: &Setup,
) -> Result<(usize, save::ReplayFiles, Checkpoint)> {
    let main = crate::storage::Place::data(data_dir, crate::storage::MAIN_FILE);
    let files = copies(&main)?;
    let segments: Vec<&Segment> = log.segments().collect();
    let after = segments
        .iter()
        .rposition(|s| s.is_truncated())
        .map_or(0, |i| i + 1);
    let mut usable = Vec::new();
    let mut opaque = None;
    for (i, segment) in segments.iter().enumerate() {
        let Some(bytes) = segment.checkpoint() else {
            continue;
        };
        let holds = |meta: &Meta| {
            meta.tick == segment.start()
                && meta
                    .identity
                    .zip(segment.identity())
                    .is_none_or(|(file, logged)| file == logged)
        };
        let Some((meta, file)) = files.iter().find(|(meta, _)| holds(meta)) else {
            continue;
        };
        let checkpoint =
            Checkpoint::read(bytes, &setup.custom, &setup.custom_entities).map_err(format)?;
        if let Some(what) = checkpoint.opaque() {
            opaque.get_or_insert(Error::Opaque {
                tick: segment.start(),
                what: what.to_owned(),
            });
            continue;
        }
        usable.push((i, (*meta, file.clone()), checkpoint));
    }
    let pick = usable.iter().position(|(i, _, _)| *i >= after).unwrap_or(0);
    if usable.is_empty() {
        return Err(opaque.unwrap_or(Error::NoMatchingWorld));
    }
    let (i, (meta, file), checkpoint) = usable.swap_remove(pick);
    Ok((i, files_of(data_dir, &meta, file)?, checkpoint))
}

/// The data files of the save `main` names, whose main file is `file`: each other file is its
/// newest copy committed at that save or before (see the module's documentation).
fn files_of(data_dir: &Path, main: &Meta, file: PathBuf) -> Result<save::ReplayFiles> {
    use crate::storage::{PLAYERS_FILE, Place};
    let pick = |place: &Place| -> Result<Option<PathBuf>> {
        Ok(copies(place)?
            .into_iter()
            .filter(|(meta, _)| !crate::storage::is_later(meta, main))
            .max_by_key(|(meta, _)| meta.tick)
            .map(|(_, path)| path))
    };
    let mut worlds = BTreeMap::new();
    for name in save::world_files(data_dir).map_err(storage)? {
        if let Some(path) = pick(&Place::world(data_dir, &name))? {
            worlds.insert(name, path);
        }
    }
    Ok(save::ReplayFiles {
        main: file,
        players: pick(&Place::data(data_dir, PLAYERS_FILE))?.ok_or(Error::NoMatchingWorld)?,
        worlds,
    })
}

/// The copies of the data file at `place` with the meta value of each one's last commit, as
/// [`crate::storage::copies`] lists them; copies that do not open are left out.
fn copies(place: &crate::storage::Place) -> Result<Vec<(Meta, PathBuf)>> {
    let mut files = Vec::new();
    for file in crate::storage::copies(place).map_err(storage)? {
        match save::committed_meta(&file) {
            Ok(meta) => files.push((meta, file)),
            Err(e) => {
                info!(
                    path = %file.display(),
                    error = &e as &(dyn std::error::Error + 'static),
                    "data file left out of the replay"
                );
            }
        }
    }
    Ok(files)
}

/// The raw extras the data files (or the generator) have for the chunks `checkpoint` names: the
/// checkpoint keeps their terrain and typed state, and the extras as they are stored.
fn extras(
    disk: &mut save::Thread,
    pool: &Pool,
    checkpoint: &Checkpoint,
) -> Result<BTreeMap<(usize, crate::world::ChunkPos), save::ChunkExtras>> {
    let keys: Vec<_> = checkpoint.loaded().chain(checkpoint.unsaved()).collect();
    let mut extras = BTreeMap::new();
    for answer in disk.load_stored(pool, &keys).map_err(storage)? {
        let Response::Chunk { world, pos, loaded } = answer else {
            continue;
        };
        match loaded {
            Loaded::Disk(_, stored) | Loaded::Generated(_, stored) => {
                extras.insert((world, pos), stored.extras);
            }
            Loaded::Damaged => {
                return Err(Error::Diverged {
                    tick: checkpoint.tick(),
                });
            }
        }
    }
    Ok(extras)
}

fn format(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::Format(Box::new(e))
}

fn storage(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::Storage(Box::new(e))
}

/// A started thread or pool's failure.
fn start(e: crate::Error) -> Error {
    match e {
        crate::Error::Start(e) => Error::Io(e),
        e => storage(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(
        clippy::disallowed_types,
        clippy::disallowed_methods,
        reason = "off-tick: test fixtures on disk"
    )]
    fn a_file_larger_than_a_crash_log_can_be_is_a_format_error() {
        let path = std::env::temp_dir().join(format!("anchor-replay-{}.log", std::process::id()));
        // Only the length is set: nothing is written, and the read refuses it before reading.
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(tick::MAX_LOG_FILE_BYTES as u64 + 1).unwrap();
        drop(file);
        let read = read(&path).err();
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(read, Some(Error::Format(_))), "{read:?}");
    }
}
