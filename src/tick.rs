//! The tick loop, the tick step and the game state.
//!
//! One thread owns [`State`] and changes it only in [`step`], once per tick, from the inputs the
//! sessions queued and the answers of the save thread (PLAN.md 5, 10.2, 10.6). Outputs leave
//! through each session's bounded queue and the save thread's request queue.

mod log;
pub(crate) mod queue;
mod save;
mod spawn;
mod task;
mod view;

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use tokio::sync::mpsc::error::{TryRecvError, TrySendError};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, trace, warn};

use crate::clock;
use crate::config::{Config, Difficulty, GameMode};
use crate::event::Listeners;
use crate::pool::Pool;
use crate::protocol::packet::{Disconnect, Packet};
use crate::save::{Request, Response, WorldIndex};
use crate::storage::PlayerKey as StoredPlayer;
use crate::world::World;

use self::log::EntryKind;
pub(crate) use self::log::{InputLog, write_crash_log};
use queue::{INPUT_CAPACITY, Identity, Input, InputKind, Message, Outbound, SessionId};
pub(crate) use spawn::Shared;
pub use task::Context;
pub(crate) use task::Tasks;

/// The length of a tick: 20 ticks per second.
pub(crate) const TICK: Duration = Duration::from_millis(50);
/// The most inputs one tick applies; the rest wait for the next tick. A full input queue drains in
/// 8 ticks, and the batch buffer is allocated once at this size.
pub(crate) const MAX_INPUTS_PER_TICK: usize = INPUT_CAPACITY / 8;
/// Save thread answers one tick applies; the rest wait for the next tick.
pub(crate) const MAX_RESPONSES_PER_TICK: usize = 1024;
/// `Disconnect` reason for a server shutdown (gophertunnel `DisconnectReasonShutdown`).
const DISCONNECT_REASON_SHUTDOWN: i32 = 68;
/// `Disconnect` reason for a duplicate login (gophertunnel `DisconnectReasonLoggedInOtherLocation`).
const DISCONNECT_REASON_LOGGED_IN_OTHER_LOCATION: i32 = 43;
/// The largest coordinate a client may report: the Bedrock world border (30,000,000 blocks).
const MAX_COORDINATE: f32 = 30_000_000.0;

/// The settings the tick reads, taken from [`Config`] once at start.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    motd: String,
    game_mode: GameMode,
    difficulty: Difficulty,
    hardcore: bool,
    view_distance: u8,
    spawn_radius: u8,
    tick_radius: u8,
    chunks_per_tick: u32,
    max_players: u32,
    shutdown_message: String,
    /// Ticks between save snapshots.
    save_interval: u64,
    /// Whether player data is loaded and saved (`save-player-data`).
    save_player_data: bool,
}

impl Settings {
    /// The tick settings of `config`.
    pub(crate) fn new(config: &Config) -> Self {
        let (general, developer) = (&config.general, &config.developer);
        Self {
            motd: general.motd.clone(),
            game_mode: general.gamemode,
            difficulty: general.difficulty,
            hardcore: general.hardcore,
            view_distance: general.view_distance,
            spawn_radius: developer.spawn_radius.min(general.view_distance),
            tick_radius: developer.tick_radius.min(general.view_distance),
            chunks_per_tick: developer.chunks_per_tick,
            max_players: general.max_players,
            shutdown_message: general.shutdown_message.clone(),
            // 20 ticks a second, 60 seconds a minute.
            save_interval: u64::from(developer.save_interval_minutes) * 20 * 60,
            save_player_data: general.save_player_data,
        }
    }
}

/// The game state.
#[derive(Debug)]
pub(crate) struct State {
    settings: Settings,
    /// The start-game batches every player gets the same.
    shared: Shared,
    /// Engine time: ticks applied since the world was created.
    tick: u64,
    /// The next entity runtime ID; IDs are never reused.
    next_runtime_id: u64,
    players: BTreeMap<SessionId, Player>,
    /// The session of each joined player by [`PlayerKey`]: `index[p.key] == s` exactly when
    /// `players[s] == p`. Derived from `players`, so it changes only where `players` does.
    index: BTreeMap<PlayerKey, SessionId>,
    /// The user's event listeners, called only in [`step`].
    listeners: Listeners,
    /// The user's scheduled tasks, called only in [`step`].
    tasks: Tasks,
    /// The worlds; the default world is the first.
    worlds: Vec<World>,
    /// Which chunks are held, in flight or waiting for a commit.
    chunks: view::Chunks,
    /// Save snapshots and what waits for their commit.
    saving: save::Saving,
    /// The session of each player waiting for its saved data, by storage key.
    loading: BTreeMap<StoredPlayer, SessionId>,
}

/// What makes two logins the same player (PLAN.md 10.3): the XUID with Xbox sign-in, otherwise
/// the name in ASCII lowercase (PMMP compares with `strcasecmp`, which folds ASCII only).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PlayerKey {
    Xuid(u64),
    Name(String),
}

impl PlayerKey {
    fn of(identity: &Identity) -> Self {
        match identity.xuid {
            Some(xuid) => Self::Xuid(xuid),
            None => Self::Name(identity.name.to_ascii_lowercase()),
        }
    }
}

/// A joined player.
#[derive(Debug)]
struct Player {
    runtime_id: u64,
    identity: Identity,
    /// [`PlayerKey::of`] the identity, kept to leave the index without rebuilding it.
    key: PlayerKey,
    /// The world the player is in.
    world: WorldIndex,
    /// The eye position, finite and inside the world border.
    position: [f32; 3],
    /// The position changed since the last save snapshot.
    data_dirty: bool,
    /// The granted chunk radius, once the client asked.
    chunk_radius: Option<u8>,
    phase: Phase,
    /// The chunks the player sees.
    view: view::View,
    outbound: mpsc::Sender<Outbound>,
    /// What the output phase sends this player at the end of the tick; empty between ticks.
    pending: spawn::Pending,
}

/// How far a player is through spawning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the saved player data.
    Loading,
    /// `StartGame` sent.
    Joined,
    /// The spawn chunks and `PlayStatus(PlayerSpawn)` sent.
    Spawning,
    /// The client confirmed with `SetLocalPlayerAsInitialised`.
    Spawned,
}

impl State {
    /// A server at engine time `tick` with `worlds` (the default world first, none loaded) whose
    /// events go to `listeners` (already sorted). The spawn chunks of every world are held from
    /// the start.
    pub(crate) fn new(
        settings: Settings,
        shared: Shared,
        listeners: Listeners,
        tasks: Tasks,
        worlds: Vec<World>,
        tick: u64,
    ) -> Self {
        let saving = save::Saving::new(tick, settings.save_interval, settings.save_player_data);
        let mut state = Self {
            settings,
            shared,
            tick,
            next_runtime_id: 1,
            players: BTreeMap::new(),
            index: BTreeMap::new(),
            listeners,
            tasks,
            worlds,
            chunks: view::Chunks::default(),
            saving,
            loading: BTreeMap::new(),
        };
        view::hold_spawns(&mut state);
        state
    }

    /// A hash of everything a replay must reproduce, independent of thread count, OS and the
    /// order of the map in memory (FNV-1a 64 over fields in a fixed order and width).
    ///
    /// The index and each player's key are left out: both are functions of the hashed sessions
    /// and identities ([`Self::index_matches_players`] checks this in tests). Listeners and
    /// tasks are code, fixed at build.
    #[cfg(test)]
    pub(crate) fn hash(&self) -> u64 {
        let mut h = Fnv::new();
        h.u64(self.tick);
        h.u64(self.next_runtime_id);
        h.usize(self.players.len());
        for (session, p) in &self.players {
            h.u64(session.get());
            h.u64(p.runtime_id);
            h.bytes(p.identity.name.as_bytes());
            h.u64(p.identity.xuid.map_or(0, |x| x.wrapping_add(1)));
            h.bytes(p.identity.uuid.as_bytes());
            h.usize(p.world);
            for c in p.position {
                h.u64(u64::from(c.to_bits()));
            }
            h.u64(u64::from(p.data_dirty));
            h.u64(p.chunk_radius.map_or(0, |r| u64::from(r) + 1));
            h.u64(match p.phase {
                Phase::Loading => 3,
                Phase::Joined => 0,
                Phase::Spawning => 1,
                Phase::Spawned => 2,
            });
            p.view.hash_into(&mut h);
        }
        for world in &self.worlds {
            h.bytes(world.name().as_bytes());
            let spawn = world.spawn();
            for c in [spawn.x, spawn.y, spawn.z] {
                h.u64(u64::from(c.cast_unsigned()));
            }
            h.usize(world.loaded().len());
            for pos in world.loaded() {
                h.u64(u64::from(pos.x.cast_unsigned()));
                h.u64(u64::from(pos.z.cast_unsigned()));
                // Chunks change only through set_block, which marks them dirty until a
                // snapshot; the content of a clean chunk is the generator's or the file's.
                if let Some(chunk) = world.chunk(pos) {
                    h.u64(u64::from(chunk.is_dirty()));
                    if chunk.is_dirty() {
                        for i in 0..crate::world::SUB_CHUNKS {
                            h.bytes(&chunk.sub_chunk_payload(i).unwrap_or_default());
                        }
                    }
                }
            }
            h.usize(world.quarantined().len());
        }
        self.chunks.hash_into(&mut h);
        self.saving.hash_into(&mut h);
        h.0
    }

    /// Whether the index holds exactly the key and session of every player.
    #[cfg(test)]
    pub(crate) fn index_matches_players(&self) -> bool {
        self.index.len() == self.players.len()
            && self.players.iter().all(|(session, p)| {
                p.key == PlayerKey::of(&p.identity) && self.index.get(&p.key) == Some(session)
            })
    }

    /// Adds a player, which must have a new session and a key no player has.
    fn insert(&mut self, session: SessionId, player: Player) {
        self.index.insert(player.key.clone(), session);
        self.players.insert(session, player);
    }

    /// Removes the player of `session`, if any, with its index entry and the chunks it held;
    /// unsaved data stays in memory for the next commit. Returns its outbound queue.
    fn remove(&mut self, session: SessionId) -> Option<Player> {
        let mut player = self.players.remove(&session)?;
        self.index.remove(&player.key);
        let stored = save::storage_key(&player.key);
        if self.loading.get(&stored) == Some(&session) {
            self.loading.remove(&stored);
        }
        view::close(self, &mut player);
        save::left(self, &player);
        Some(player)
    }

    /// Forgets the player of `session`, if any; its outbound queue closes.
    fn leave(&mut self, session: SessionId) {
        if let Some(p) = self.remove(session) {
            info!(session = session.get(), name = %p.identity.name, "player left");
        }
    }

    /// Disconnects every player with the shutdown message.
    fn close_all(&mut self, output: &mut Output) {
        let sessions: Vec<SessionId> = self.players.keys().copied().collect();
        for session in sessions {
            let Some(p) = self.remove(session) else {
                continue;
            };
            debug!(session = session.get(), "closing for shutdown");
            let packet = Packet::Disconnect(Disconnect {
                reason: DISCONNECT_REASON_SHUTDOWN,
                hide_disconnection_screen: false,
                message: self.settings.shutdown_message.clone(),
                filtered_message: String::new(),
            });
            output.close(p.outbound, packet);
        }
    }
}

/// What one tick sends, delivered after the step.
#[derive(Debug, Default)]
pub(crate) struct Output {
    /// Messages per session from the output phase, in session order.
    packets: Vec<(SessionId, Vec<Message>)>,
    /// Final packets for sessions the tick has already forgotten.
    closes: Vec<(mpsc::Sender<Outbound>, Packet)>,
    /// Requests for the save thread, in order.
    requests: Vec<Request>,
    /// A commit failed: the server stops without saving (PLAN.md 6).
    save_failed: bool,
    /// Tests: the answers of a stand-in save thread, applied by the next tick.
    #[cfg(test)]
    answers: Vec<Response>,
}

impl Output {
    fn close(&mut self, outbound: mpsc::Sender<Outbound>, packet: Packet) {
        self.closes.push((outbound, packet));
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.packets.is_empty() && self.closes.is_empty()
    }
}

/// Applies one tick of `inputs` and save thread `responses`, leaving both empty, and records them
/// in `log` (PLAN.md 5, 10.5, 10.6).
///
/// 1. Input (serial): the save thread's answers in arrival order (loaded chunks and player data,
///    commits), then the session inputs in (session, seq) order, so the result does not depend
///    on how sessions interleaved in the queue. Then the scheduled tasks.
/// 2. Compute and 3. apply (serial for now): each view follows its player, picks the chunks to
///    send and asks for the loads it needs.
/// 4. Output (parallel, read-only): the sub-chunks the answers need, encoded once each, then
///    every marked player's packets, built on the CPU pool in player order, so the output does
///    not depend on the thread count.
///
/// After the output is delivered, [`boundary`] takes a save snapshot if one is due.
pub(crate) fn step(
    state: &mut State,
    inputs: &mut Vec<Input>,
    responses: &mut Vec<Response>,
    pool: &Pool,
    output: &mut Output,
    log: &mut InputLog,
) {
    log.begin(state.tick);
    for response in responses.drain(..) {
        respond(state, response, output, log);
    }
    // Keys are unique per input, so the unstable sort is deterministic and allocates nothing.
    inputs.sort_unstable_by_key(|i| (i.session, i.seq));
    for input in inputs.drain(..) {
        let (session, seq) = (input.session, input.seq);
        match input.kind {
            InputKind::Join { identity, outbound } => {
                log.record(session, seq, EntryKind::Join(identity.clone()), 0);
                if let Some(request) = spawn::join(state, session, identity, outbound, output) {
                    output.requests.push(request);
                }
            }
            InputKind::Packet { packet, size } => {
                apply_packet(state, session, &packet);
                log.record(session, seq, EntryKind::Packet(packet), size);
            }
            InputKind::Leave => {
                log.record(session, seq, EntryKind::Leave, 0);
                state.leave(session);
            }
        }
    }
    let mut save_requested = false;
    state
        .tasks
        .run(state.tick, &mut state.worlds, &mut save_requested);
    state.saving.requested |= save_requested;
    if let Some(request) = view::update(state) {
        output.requests.push(request);
    }
    let changes = view::take_changes(&mut state.worlds);
    output.packets = spawn::output(state, pool, &changes);
    state.tick = state.tick.saturating_add(1);
}

/// The tick boundary after a step and its delivery: a save snapshot for the save thread if one is
/// due (PLAN.md 10.6). It comes after the delivery because sessions dropped there change the
/// state of the tick.
pub(crate) fn boundary(state: &mut State, output: &mut Output, log: &mut InputLog) {
    if let Some(request) = save::snapshot(state, log, false) {
        output.requests.push(request);
    }
}

/// Applies one answer of the save thread and records it.
fn respond(state: &mut State, response: Response, output: &mut Output, log: &mut InputLog) {
    log.response(&response);
    match response {
        Response::Chunk { world, pos, loaded } => view::loaded(state, world, pos, loaded),
        Response::Player { key, data } => spawn::player_loaded(state, &key, data, output),
        Response::Committed { start } => save::committed(state, log, start),
        Response::CommitFailed { start } => {
            save::failed(start);
            output.save_failed = true;
        }
    }
}

/// Applies an in-game packet of `session`.
fn apply_packet(state: &mut State, session: SessionId, packet: &Packet) {
    match packet {
        Packet::RequestChunkRadius(p) => spawn::request_chunk_radius(state, session, p),
        Packet::SetLocalPlayerAsInitialised(p) => spawn::initialised(state, session, p),
        Packet::PlayerAuthInput(p) => {
            if let Some(player) = state.players.get_mut(&session) {
                moved(player, p.position);
            }
        }
        Packet::SubChunkRequest(p) => {
            if let Some(player) = state.players.get_mut(&session) {
                view::queue_request(player, p.clone());
            }
        }
        p => trace!(
            session = session.get(),
            id = p.id(),
            "packet not handled yet"
        ),
    }
}

/// Takes the client's position as it is (movement checks come with M7). Positions that are not
/// finite or outside the world border are ignored: tick state holds no NaN or infinity (PLAN.md
/// 2.1).
fn moved(player: &mut Player, position: [f32; 3]) {
    if player.phase == Phase::Loading
        || !position
            .iter()
            .all(|c| c.is_finite() && c.abs() <= MAX_COORDINATE)
    {
        return;
    }
    if position != player.position {
        player.position = position;
        player.data_dirty = true;
    }
}

/// Sends `output` to the sessions. A session whose queue is full or closed is forgotten as if it
/// had left, and `log` records it (a replay must forget it too); the tick never waits.
fn deliver(state: &mut State, output: &mut Output, log: &mut InputLog) {
    for (session, packets) in output.packets.drain(..) {
        let Some(player) = state.players.get(&session) else {
            continue;
        };
        match player.outbound.try_send(Outbound::Send(packets)) {
            Ok(()) => continue,
            Err(TrySendError::Full(_)) => {
                warn!(
                    session = session.get(),
                    "outbound queue full; dropping the session"
                );
            }
            Err(TrySendError::Closed(_)) => {}
        }
        log.dropped(session);
        state.leave(session);
    }
    for (outbound, packet) in output.closes.drain(..) {
        // The session closes when the queue does; a full or closed queue only loses the reason.
        if outbound.try_send(Outbound::Close(packet)).is_err() {
            debug!("could not queue the final packet of a closed session");
        }
    }
}

/// Hands the input log to the main thread if the tick thread panics, so it can be written to
/// `data/crash/` without the tick thread touching files (PLAN.md 4.6, 10.5).
struct CrashGuard {
    log: InputLog,
    crash: Option<oneshot::Sender<InputLog>>,
}

impl Drop for CrashGuard {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(crash) = self.crash.take()
            && crash.send(std::mem::take(&mut self.log)).is_err()
        {
            error!("the input log of the panicked tick could not be handed over");
        }
    }
}

/// The tick's link to the save thread.
#[derive(Debug)]
pub(crate) struct SaveLink {
    pub(crate) requests: mpsc::Sender<Request>,
    pub(crate) responses: mpsc::Receiver<Response>,
}

/// How the tick thread ended.
#[derive(Debug)]
pub(crate) enum End {
    /// Stopped on request; the final snapshot went to the save thread.
    Stopped,
    /// A commit failed: the players were disconnected and nothing more was saved. The input log
    /// since the last commit goes to `data/crash/`.
    SaveFailed(InputLog),
}

/// The tick thread: runs ticks every [`TICK`] until `stop` receives or closes or a commit fails,
/// then disconnects every player, hands the final snapshot to the save thread (unless a commit
/// failed) and returns. `_done` drops when the thread ends, normally or by
/// panic; on a panic the input log goes to `crash` first.
pub(crate) fn run(
    mut state: State,
    pool: &Pool,
    mut inputs: mpsc::Receiver<Input>,
    mut save: SaveLink,
    mut stop: mpsc::Receiver<()>,
    crash: oneshot::Sender<InputLog>,
    _done: oneshot::Sender<()>,
) -> End {
    let mut guard = CrashGuard {
        log: InputLog::starting_at(state.tick),
        crash: Some(crash),
    };
    let mut batch = Vec::with_capacity(MAX_INPUTS_PER_TICK);
    let mut answers = Vec::with_capacity(MAX_RESPONSES_PER_TICK);
    let mut waiting = VecDeque::new();
    let mut output = Output::default();
    let mut pacer = clock::Pacer::new(TICK, clock::now());
    info!(tick = state.tick, "tick started");
    let mut failed = false;
    loop {
        if !matches!(stop.try_recv(), Err(TryRecvError::Empty)) {
            break;
        }
        drain(&mut inputs, &mut batch, MAX_INPUTS_PER_TICK);
        drain(&mut save.responses, &mut answers, MAX_RESPONSES_PER_TICK);
        if answers.is_empty() && save.responses.is_closed() {
            // The save thread ended (it panicked): nothing could be saved any more.
            error!("the save thread ended; stopping without saving");
            failed = true;
            break;
        }
        step(
            &mut state,
            &mut batch,
            &mut answers,
            pool,
            &mut output,
            &mut guard.log,
        );
        deliver(&mut state, &mut output, &mut guard.log);
        boundary(&mut state, &mut output, &mut guard.log);
        waiting.extend(output.requests.drain(..));
        send_requests(&save.requests, &mut waiting);
        if output.save_failed {
            failed = true;
            break;
        }
        clock::sleep_until(pacer.schedule(clock::now()));
    }
    state.close_all(&mut output);
    if !failed {
        if let Some(request) = save::snapshot(&mut state, &mut guard.log, true) {
            waiting.push_back(request);
        }
        finish_requests(&save.requests, waiting);
    }
    deliver(&mut state, &mut output, &mut guard.log);
    drop(save);
    info!(ticks = state.tick, "tick stopped");
    if failed {
        End::SaveFailed(std::mem::take(&mut guard.log))
    } else {
        End::Stopped
    }
}

/// Sends the waiting requests in order without waiting; what does not fit waits for the next
/// tick. The queue only fills if the save thread falls behind.
fn send_requests(requests: &mpsc::Sender<Request>, waiting: &mut VecDeque<Request>) {
    while let Some(request) = waiting.pop_front() {
        match requests.try_send(request) {
            Ok(()) => {}
            Err(TrySendError::Full(request)) => {
                waiting.push_front(request);
                return;
            }
            Err(TrySendError::Closed(_)) => {
                error!("the save thread stopped; requests are lost");
                waiting.clear();
                return;
            }
        }
    }
}

/// Sends the last requests at shutdown, waiting for room: the tick loop has ended, and the final
/// snapshot must reach the save thread.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the tick loop has ended; shutdown waits for the save thread to take the final snapshot"
)]
fn finish_requests(requests: &mpsc::Sender<Request>, waiting: VecDeque<Request>) {
    for request in waiting {
        if requests.blocking_send(request).is_err() {
            error!("the save thread stopped before the final save");
            return;
        }
    }
}

/// Moves up to `max` queued items into `batch` without waiting.
fn drain<T>(queue: &mut mpsc::Receiver<T>, batch: &mut Vec<T>, max: usize) {
    while batch.len() < max {
        match queue.try_recv() {
            Ok(item) => batch.push(item),
            // Disconnected: every sender is gone (networking or the save thread stopped).
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
}

/// FNV-1a 64: a hash whose output is fixed by its definition, unlike `DefaultHasher`.
#[cfg(test)]
struct Fnv(u64);

#[cfg(test)]
impl Fnv {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;

    fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn raw(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(Self::PRIME);
        }
    }

    fn u64(&mut self, v: u64) {
        self.raw(&v.to_le_bytes());
    }

    fn usize(&mut self, v: usize) {
        self.u64(u64::try_from(v).unwrap_or(u64::MAX));
    }

    /// Length-prefixed, so adjacent fields cannot run together.
    fn bytes(&mut self, bytes: &[u8]) {
        self.usize(bytes.len());
        self.raw(bytes);
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use super::queue::{input_queue, outbound_queue};
    use super::*;
    use crate::event::{PlayerDuplicateLogin, Priority};
    use crate::protocol::Uuid;
    use crate::protocol::packet::{
        ChunkRadiusUpdated, PlayStatus, RequestChunkRadius, SetLocalPlayerAsInitialised,
    };
    use crate::save::{Loaded, PlayerLoad};
    use crate::world::{Chunk, ChunkPos, Flat, Generator, Kind};

    /// Test settings: no player data (a join needs no load), and every loaded chunk of a view
    /// sent in one tick.
    fn settings(edit: impl FnOnce(&mut Config)) -> Settings {
        let mut config = Config::default();
        config.general.save_player_data = false;
        config.developer.chunks_per_tick = 256;
        edit(&mut config);
        Settings::new(&config)
    }

    /// Chunks within this many chunks of the spawn (a square) are loaded before the first tick,
    /// so a spawn needs no load; the others load through the stand-in save thread.
    const PRELOADED: i32 = 4;

    /// A server with a flat persistent world, seed 0, its spawn area loaded.
    fn new_state(settings: Settings, listeners: Listeners) -> State {
        let world = World::new("world", Kind::Persistent, 0, Flat.spawn(0));
        let mut state = State::new(
            settings,
            shared(),
            listeners,
            Tasks::default(),
            vec![world],
            0,
        );
        let center = state.worlds[0].spawn().chunk();
        for x in -PRELOADED..=PRELOADED {
            for z in -PRELOADED..=PRELOADED {
                let pos = ChunkPos::new(center.x + x, center.z + z);
                state.worlds[0].insert_chunk(pos, Chunk::generate(&Flat, 0, pos));
            }
        }
        state
    }

    /// The stand-in save thread: chunks are generated, players are new, commits succeed.
    fn answer(state: &State, requests: Vec<Request>) -> Vec<Response> {
        let mut out = Vec::new();
        for request in requests {
            match request {
                Request::Chunks(keys) => {
                    for (world, pos) in keys {
                        let seed = state.worlds[world].seed();
                        let chunk = Box::new(Chunk::generate(&Flat, seed, pos));
                        out.push(Response::Chunk {
                            world,
                            pos,
                            loaded: Loaded::Generated(chunk),
                        });
                    }
                }
                Request::Player(key) => out.push(Response::Player {
                    key,
                    data: PlayerLoad::Absent,
                }),
                Request::Commit(snapshot) => out.push(Response::Committed {
                    start: snapshot.start,
                }),
            }
        }
        out
    }

    /// The shared start-game batches, built once for all tests.
    fn shared() -> Shared {
        static SHARED: std::sync::OnceLock<Shared> = std::sync::OnceLock::new();
        SHARED
            .get_or_init(|| Shared::new(crate::protocol::batch::Settings::default()).unwrap())
            .clone()
    }

    thread_local! {
        /// A CPU pool per test thread; tests do not shut it down.
        static POOL: Pool = crate::runtime::cpu_pool(2).unwrap();
    }

    /// Runs and delivers one tick of `batch`; the stand-in save thread answers its requests for
    /// the next tick.
    fn tick(state: &mut State, batch: &mut Vec<Input>, output: &mut Output) {
        let mut log = InputLog::default();
        let mut answers = std::mem::take(&mut output.answers);
        POOL.with(|pool| step(state, batch, &mut answers, pool, output, &mut log));
        deliver(state, output, &mut log);
        boundary(state, output, &mut log);
        let requests = std::mem::take(&mut output.requests);
        output.answers = answer(state, requests);
    }

    /// An in-game packet input of its encoded size.
    fn packet(packet: Packet) -> InputKind {
        let mut bytes = Vec::new();
        packet.encode(&mut bytes, 0).unwrap();
        InputKind::Packet {
            packet,
            size: bytes.len(),
        }
    }

    fn identity(n: u64) -> Identity {
        Identity {
            name: format!("player{n}"),
            xuid: Some(2_535_000_000 + n),
            uuid: Uuid::from_bytes([u8::try_from(n % 256).unwrap(); 16]),
        }
    }

    fn input(session: u64, seq: u64, kind: InputKind) -> Input {
        Input {
            session: SessionId::new(session),
            seq,
            kind,
        }
    }

    fn join(session: u64, seq: u64) -> (Input, mpsc::Receiver<Outbound>) {
        let (tx, rx) = outbound_queue();
        let kind = InputKind::Join {
            identity: identity(session),
            outbound: tx,
        };
        (input(session, seq, kind), rx)
    }

    fn radius(session: u64, seq: u64, chunk_radius: i32) -> Input {
        let p = RequestChunkRadius {
            chunk_radius,
            max_chunk_radius: 0,
        };
        input(session, seq, packet(Packet::RequestChunkRadius(p)))
    }

    fn initialised(session: u64, seq: u64, entity_runtime_id: u64) -> Input {
        let p = SetLocalPlayerAsInitialised { entity_runtime_id };
        let kind = packet(Packet::SetLocalPlayerAsInitialised(p));
        input(session, seq, kind)
    }

    fn leave(session: u64, seq: u64) -> Input {
        input(session, seq, InputKind::Leave)
    }

    /// Runs and delivers one tick per batch.
    fn run_ticks(settings: Settings, ticks: Vec<Vec<Input>>) -> State {
        run_ticks_with(settings, Listeners::default(), ticks)
    }

    /// [`run_ticks`] with event listeners (sorted here, as `build()` does).
    fn run_ticks_with(
        settings: Settings,
        mut listeners: Listeners,
        ticks: Vec<Vec<Input>>,
    ) -> State {
        listeners.sort();
        let mut state = new_state(settings, listeners);
        let mut output = Output::default();
        for mut batch in ticks {
            tick(&mut state, &mut batch, &mut output);
            assert!(batch.is_empty());
            assert!(output.is_empty());
            assert!(state.index_matches_players());
        }
        state
    }

    /// Every packet queued on `rx`, the final packet if the session was closed, and the IDs of
    /// everything sent in order, the packets of shared batches included.
    fn received(rx: &mut mpsc::Receiver<Outbound>) -> (Vec<Packet>, Option<Packet>, Vec<u32>) {
        let mut packets = Vec::new();
        let mut close = None;
        let mut sent = Vec::new();
        let mut decoder =
            crate::protocol::batch::Decoder::new(crate::protocol::batch::Settings::default());
        while let Ok(out) = rx.try_recv() {
            match out {
                Outbound::Send(messages) => {
                    for m in messages {
                        match m {
                            Message::Packet(p) => {
                                sent.push(p.id());
                                packets.push(p);
                            }
                            Message::Shared(batches) => {
                                for body in batches.iter() {
                                    let mut payload = Vec::new();
                                    decoder.decode(body, &mut payload).unwrap();
                                    for packet in crate::protocol::batch::split(&payload).unwrap() {
                                        sent.push(packet.0.id);
                                    }
                                }
                            }
                        }
                    }
                }
                Outbound::Close(p) => close = Some(p),
            }
        }
        (packets, close, sent)
    }

    fn spawn_status_count(packets: &[Packet]) -> usize {
        packets
            .iter()
            .filter(|p| matches!(p, Packet::PlayStatus(s) if s.status == 3))
            .count()
    }

    /// A full spawn of sessions 1..=n over two ticks (the client confirms after the spawn
    /// status), one input list per session and tick; session k gets runtime ID k.
    fn spawn_inputs(n: u64) -> ([Vec<Vec<Input>>; 2], Vec<mpsc::Receiver<Outbound>>) {
        let mut ticks = [Vec::new(), Vec::new()];
        let mut receivers = Vec::new();
        for s in 1..=n {
            let (j, rx) = join(s, 0);
            receivers.push(rx);
            ticks[0].push(vec![j, radius(s, 1, 8)]);
            ticks[1].push(vec![initialised(s, 2, s)]);
        }
        (ticks, receivers)
    }

    fn round_robin(sessions: Vec<Vec<Input>>) -> Vec<Input> {
        let total = sessions.iter().map(Vec::len).sum();
        let mut iters: Vec<_> = sessions.into_iter().map(Vec::into_iter).collect();
        let mut out = Vec::with_capacity(total);
        while out.len() < total {
            for it in &mut iters {
                out.extend(it.next());
            }
        }
        out
    }

    #[test]
    fn interleaving_between_sessions_does_not_change_the_state() {
        type Order = fn(Vec<Vec<Input>>) -> Vec<Input>;
        let orders: [Order; 3] = [
            |s| s.concat(),
            round_robin,
            |s| {
                s.into_iter()
                    .rev()
                    .flat_map(|v| v.into_iter().rev())
                    .collect()
            },
        ];
        let mut results = Vec::new();
        for order in orders {
            let ([first, second], mut receivers) = spawn_inputs(3);
            let state = run_ticks(settings(|_| {}), vec![order(first), order(second)]);
            assert!(state.players.values().all(|p| p.phase == Phase::Spawned));
            let sent: Vec<_> = receivers.iter_mut().map(|rx| received(rx).0).collect();
            results.push((state.hash(), sent));
        }
        assert_eq!(results[0], results[1]);
        assert_eq!(results[0], results[2]);
    }

    #[test]
    fn a_duplicate_join_changes_nothing() {
        let (j, mut rx) = join(1, 0);
        let once = run_ticks(settings(|_| {}), vec![vec![j.clone()], vec![]]);
        let (j2, mut rx2) = join(1, 1);
        let same_tick = run_ticks(settings(|_| {}), vec![vec![j.clone(), j2], vec![]]);
        let (j3, mut rx3) = join(1, 0);
        let next_tick = run_ticks(settings(|_| {}), vec![vec![j], vec![j3]]);
        assert_eq!(once.hash(), same_tick.hash());
        assert_eq!(once.hash(), next_tick.hash());
        // The first join got the start-game packets (each run above sent them once).
        assert_eq!(received(&mut rx).2, [11, 162, 11, 162, 11, 162]);
        // The duplicates got nothing and their queues were dropped.
        for rx in [&mut rx2, &mut rx3] {
            assert_eq!(received(rx), (vec![], None, vec![]));
            assert!(rx.is_closed());
        }
    }

    #[test]
    fn a_duplicate_leave_changes_nothing() {
        let (j, _rx) = join(1, 0);
        let (k, _rx2) = join(2, 0);
        let first = vec![j, k];
        let once = run_ticks(settings(|_| {}), vec![first.clone(), vec![leave(1, 1)]]);
        let twice = run_ticks(
            settings(|_| {}),
            vec![first.clone(), vec![leave(1, 1), leave(1, 2)]],
        );
        let later = run_ticks(
            settings(|_| {}),
            vec![first.clone(), vec![leave(1, 1)], vec![leave(1, 2)]],
        );
        let unknown = run_ticks(
            settings(|_| {}),
            vec![first.clone(), vec![leave(1, 1), leave(9, 0)]],
        );
        assert_eq!(once.hash(), twice.hash());
        assert_eq!(once.hash(), unknown.hash());
        let once_later = run_ticks(settings(|_| {}), vec![first, vec![leave(1, 1)], vec![]]);
        assert_eq!(once_later.hash(), later.hash());
        assert_eq!(once.players.len(), 1);
    }

    #[test]
    fn a_duplicate_chunk_radius_request_changes_nothing_and_spawns_once() {
        let (j, mut rx) = join(1, 0);
        let once = run_ticks(settings(|_| {}), vec![vec![j, radius(1, 1, 8)], vec![]]);
        let packets_once = received(&mut rx).0;
        let (j, mut rx) = join(1, 0);
        let twice = run_ticks(
            settings(|_| {}),
            vec![vec![j, radius(1, 1, 8)], vec![radius(1, 2, 8)]],
        );
        let packets_twice = received(&mut rx).0;
        let (j, _rx) = join(1, 0);
        let same_tick = run_ticks(
            settings(|_| {}),
            vec![vec![j, radius(1, 1, 8), radius(1, 2, 8)], vec![]],
        );
        assert_eq!(once.hash(), twice.hash());
        assert_eq!(once.hash(), same_tick.hash());
        assert_eq!(spawn_status_count(&packets_once), 1);
        assert_eq!(spawn_status_count(&packets_twice), 1);
    }

    #[test]
    fn a_duplicate_initialised_changes_nothing() {
        let spawned = |extra: Vec<Input>, later: Vec<Input>| {
            let (j, _rx) = join(1, 0);
            let mut second = vec![initialised(1, 2, 1)];
            second.extend(extra);
            run_ticks(
                settings(|_| {}),
                vec![vec![j, radius(1, 1, 4)], second, later],
            )
        };
        let once = spawned(vec![], vec![]);
        let twice = spawned(vec![initialised(1, 3, 1)], vec![]);
        let later = spawned(vec![], vec![initialised(1, 3, 1)]);
        assert_eq!(once.hash(), twice.hash());
        assert_eq!(once.hash(), later.hash());
        assert_eq!(once.players[&SessionId::new(1)].phase, Phase::Spawned);
    }

    #[test]
    fn initialised_with_a_wrong_runtime_id_or_before_spawning_is_ignored() {
        let (j, _rx) = join(1, 0);
        let early = run_ticks(settings(|_| {}), vec![vec![j, initialised(1, 1, 1)]]);
        assert_eq!(early.players[&SessionId::new(1)].phase, Phase::Joined);
        let (j, _rx) = join(1, 0);
        let wrong = run_ticks(
            settings(|_| {}),
            vec![vec![j, radius(1, 1, 4), initialised(1, 2, 99)]],
        );
        assert_eq!(wrong.players[&SessionId::new(1)].phase, Phase::Spawning);
    }

    #[test]
    fn join_and_leave_cycles_return_every_table_and_queue_to_its_start() {
        let (sender, mut queue) = input_queue();
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut batch = Vec::with_capacity(MAX_INPUTS_PER_TICK);
        let mut output = Output::default();
        let start = (
            state.players.len(),
            queue.len(),
            batch.capacity(),
            state.chunks.held(),
        );
        let mut tick = |queue: &mut mpsc::Receiver<Input>, state: &mut State| {
            drain(queue, &mut batch, MAX_INPUTS_PER_TICK);
            tick(state, &mut batch, &mut output);
        };
        for n in 0..100 {
            let mut inputs = sender.session(SessionId::new(n));
            let (tx, mut rx) = outbound_queue();
            let join = InputKind::Join {
                identity: identity(n),
                outbound: tx,
            };
            inputs.try_send(join).unwrap();
            let request = RequestChunkRadius {
                chunk_radius: 4,
                max_chunk_radius: 0,
            };
            let request = packet(Packet::RequestChunkRadius(request));
            inputs.try_send(request).unwrap();
            // The view's chunks unloaded when the last player left, so they load again first.
            for _ in 0..3 {
                tick(&mut queue, &mut state);
            }
            assert_eq!(state.players.len(), 1);
            let mut leaving = pin!(inputs.leave());
            let polled = leaving
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()));
            assert_eq!(polled, Poll::Ready(()));
            tick(&mut queue, &mut state);
            assert_eq!(spawn_status_count(&received(&mut rx).0), 1);
            assert!(rx.is_closed(), "the tick dropped the session's sender");
        }
        assert_eq!(
            (
                state.players.len(),
                queue.len(),
                batch.capacity(),
                state.chunks.held()
            ),
            start
        );
        assert!(output.is_empty());
        assert!(state.chunks.is_idle() && state.saving.is_idle() && state.loading.is_empty());
        assert_eq!(state.next_runtime_id, 101, "runtime IDs are never reused");
    }

    #[test]
    fn a_full_server_rejects_the_join_without_changing_state() {
        let full = settings(|c| c.general.max_players = 1);
        let (a, _rx) = join(1, 0);
        let one = run_ticks(full.clone(), vec![vec![a]]);
        let (a, _rx) = join(1, 0);
        let (b, mut rx2) = join(2, 0);
        let both = run_ticks(full, vec![vec![a, b]]);
        assert_eq!(one.hash(), both.hash());
        let (packets, close, _) = received(&mut rx2);
        assert!(packets.is_empty());
        assert_eq!(close, Some(Packet::PlayStatus(PlayStatus { status: 7 })));
        assert!(rx2.is_closed());
    }

    #[test]
    fn a_session_with_a_full_outbound_queue_is_dropped() {
        let (tx, mut rx) = mpsc::channel(1);
        let kind = InputKind::Join {
            identity: identity(1),
            outbound: tx,
        };
        // The join fills the queue; the radius answer does not fit.
        let state = run_ticks(
            settings(|_| {}),
            vec![vec![input(1, 0, kind)], vec![radius(1, 1, 4)]],
        );
        assert!(state.players.is_empty());
        assert_eq!(received(&mut rx).2, [11, 162]);
        assert!(rx.is_closed());
    }

    #[test]
    fn the_chunk_radius_is_capped_and_chunks_cover_the_spawn_radius() {
        let config = settings(|c| {
            c.general.view_distance = 6;
            c.developer.spawn_radius = 2;
        });
        for (requested, granted) in [(1, 1), (3, 3), (6, 6), (7, 6), (1_000, 6)] {
            let (j, mut rx) = join(1, 0);
            run_ticks(config.clone(), vec![vec![j, radius(1, 1, requested)]]);
            let packets = received(&mut rx).0;
            let chunks: Vec<[i32; 2]> = packets
                .iter()
                .filter_map(|p| match p {
                    Packet::LevelChunk(c) => Some(c.position),
                    _ => None,
                })
                .collect();
            let expected = ChunkRadiusUpdated {
                chunk_radius: granted,
            };
            assert_eq!(packets.get(2), Some(&Packet::ChunkRadiusUpdated(expected)));
            // Every loaded chunk of the view went out in one tick, nearest first; they cover
            // the spawn radius, so the spawn status follows them.
            let r = granted.min(2) * granted.min(2);
            let spawn_disc = chunks.iter().filter(|[x, z]| x * x + z * z <= r).count();
            assert_eq!(
                spawn_disc,
                if granted == 1 { 5 } else { 13 },
                "r = {requested}"
            );
            assert!(
                chunks
                    .iter()
                    .all(|[x, z]| x * x + z * z <= granted * granted)
            );
            assert_eq!(spawn_status_count(&packets), 1);
        }
        for bad in [0, -1, i32::MIN] {
            let (j, mut rx) = join(1, 0);
            let state = run_ticks(config.clone(), vec![vec![j, radius(1, 1, bad)]]);
            assert_eq!(
                received(&mut rx).2,
                [11, 162],
                "only the start-game packets"
            );
            assert_eq!(state.players[&SessionId::new(1)].chunk_radius, None);
        }
    }

    #[test]
    fn spawn_packets_follow_the_config_and_survive_a_codec_round_trip() {
        let config = settings(|c| {
            c.general.motd = "Lobby".to_owned();
            c.general.gamemode = GameMode::Creative;
            c.general.difficulty = Difficulty::Hard;
            c.general.hardcore = true;
        });
        let (j, mut rx) = join(1, 0);
        run_ticks(config, vec![vec![j, radius(1, 1, 2)]]);
        let (packets, _, sent) = received(&mut rx);
        let Some(Packet::StartGame(start)) = packets.first() else {
            panic!("StartGame comes first");
        };
        assert_eq!(start.entity_runtime_id, 1);
        assert_eq!((start.world_game_mode, start.difficulty), (1, 3));
        assert_eq!(
            start.player_game_mode, 5,
            "the player follows the world game mode"
        );
        assert!(start.hardcore && start.use_block_network_id_hashes);
        assert_eq!((start.world_seed, start.world_name.as_str()), (0, "Lobby"));
        assert_eq!(start.world_spawn, [0, -60, 0], "the flat world's spawn");
        assert_eq!(start.player_position, [0.5, -58.38, 0.5]);
        assert_eq!(start.game_version, crate::protocol::packet::GAME_VERSION);
        // StartGame, ItemRegistry, SetSpawnPosition, BiomeDefinitionList, CreativeContent,
        // CraftingData, ChunkRadiusUpdated, NetworkChunkPublisherUpdate, a chunk.
        assert_eq!(sent[..9], [11, 162, 43, 122, 145, 52, 70, 121, 58]);
        let last = Packet::PlayStatus(PlayStatus { status: 3 });
        assert_eq!(packets.last(), Some(&last));
        for p in &packets {
            let mut bytes = Vec::new();
            p.encode(&mut bytes, 0).unwrap();
            let mut budget = usize::MAX;
            assert_eq!(&Packet::decode(p.id(), &bytes, &mut budget, 0).unwrap(), p);
        }
    }

    #[test]
    fn shutdown_closes_every_player_with_the_message() {
        let config = settings(|c| c.general.shutdown_message = "Bye".to_owned());
        let (a, mut rx1) = join(1, 0);
        let (b, mut rx2) = join(2, 0);
        let mut state = run_ticks(config, vec![vec![a, b]]);
        let mut output = Output::default();
        state.close_all(&mut output);
        deliver(&mut state, &mut output, &mut InputLog::default());
        assert!(state.players.is_empty());
        for rx in [&mut rx1, &mut rx2] {
            let Some(Packet::Disconnect(d)) = received(rx).1 else {
                panic!("a Disconnect closes the session");
            };
            assert_eq!((d.reason, d.message.as_str()), (68, "Bye"));
            assert!(rx.is_closed());
        }
    }

    #[test]
    fn the_hash_changes_with_the_state() {
        let (j, _rx) = join(1, 0);
        let (k, _rx2) = join(2, 0);
        let hashes = [
            run_ticks(settings(|_| {}), vec![]).hash(),
            run_ticks(settings(|_| {}), vec![vec![j.clone()]]).hash(),
            run_ticks(settings(|_| {}), vec![vec![k]]).hash(),
            run_ticks(settings(|_| {}), vec![vec![j.clone(), radius(1, 1, 4)]]).hash(),
            run_ticks(settings(|_| {}), vec![vec![j], vec![]]).hash(),
        ];
        for (i, x) in hashes.iter().enumerate() {
            for y in &hashes[i + 1..] {
                assert_ne!(x, y);
            }
        }
    }

    fn person(name: &str, xuid: Option<u64>) -> Identity {
        Identity {
            name: name.to_owned(),
            xuid,
            uuid: Uuid::from_bytes([7; 16]),
        }
    }

    fn join_as(session: u64, seq: u64, identity: Identity) -> (Input, mpsc::Receiver<Outbound>) {
        let (tx, rx) = outbound_queue();
        let kind = InputKind::Join {
            identity,
            outbound: tx,
        };
        (input(session, seq, kind), rx)
    }

    type Hook = fn(&mut PlayerDuplicateLogin);

    fn with(listeners: &[(Priority, Hook)]) -> Listeners {
        let mut all = Listeners::default();
        for &(priority, listener) in listeners {
            all.push(priority, listener);
        }
        all
    }

    /// The `Disconnect` that closed the session of `rx`, as (reason, message).
    fn kicked(rx: &mut mpsc::Receiver<Outbound>) -> (i32, String) {
        let Some(Packet::Disconnect(d)) = received(rx).1 else {
            panic!("a Disconnect closes the session");
        };
        assert!(rx.is_closed());
        (d.reason, d.message)
    }

    fn sessions(state: &State) -> Vec<u64> {
        state.players.keys().map(|s| s.get()).collect()
    }

    const LOGGED_IN_OTHER_LOCATION: (i32, &str) = (43, "Logged in from other location");

    #[test]
    fn a_duplicate_login_replaces_the_existing_player() {
        let noop: Hook = |_| {};
        for listeners in [with(&[]), with(&[(Priority::Normal, noop)])] {
            let (a, mut rx1) = join_as(1, 0, person("Alex", Some(5)));
            let (b, mut rx2) = join_as(2, 0, person("Renamed", Some(5)));
            let state = run_ticks_with(settings(|_| {}), listeners, vec![vec![a], vec![b]]);
            assert_eq!(sessions(&state), [2]);
            let (reason, message) = kicked(&mut rx1);
            assert_eq!((reason, message.as_str()), LOGGED_IN_OTHER_LOCATION);
            assert_eq!(received(&mut rx2).2, [11, 162], "the new player joined");
            assert_eq!(state.players[&SessionId::new(2)].runtime_id, 2);
        }
    }

    #[test]
    fn a_cancelled_duplicate_login_rejects_the_connecting_player() {
        let cancel: Hook = |e| {
            let reason = format!("{} over {}", e.connecting().name(), e.existing().name());
            e.set_disconnect_reason(reason);
            e.cancel();
        };
        let (a, _rx1) = join_as(1, 0, person("Alex", Some(5)));
        let (b, mut rx2) = join_as(2, 0, person("Alex2", Some(5)));
        let state = run_ticks_with(
            settings(|_| {}),
            with(&[(Priority::Normal, cancel)]),
            vec![vec![a.clone()], vec![b]],
        );
        assert_eq!(kicked(&mut rx2), (43, "Alex2 over Alex".to_owned()));
        let alone = run_ticks(settings(|_| {}), vec![vec![a], vec![]]);
        assert_eq!(state.hash(), alone.hash(), "the rejection changed nothing");
        assert_eq!(sessions(&state), [1]);
    }

    #[test]
    fn the_screen_message_replaces_the_reason_on_the_disconnect() {
        let screen: Hook = |e| {
            e.set_disconnect_reason("logged reason");
            e.set_disconnect_screen_message(Some("Seen on screen".to_owned()));
        };
        let (a, mut rx1) = join_as(1, 0, person("Alex", Some(5)));
        let (b, _rx2) = join_as(2, 0, person("Alex", Some(5)));
        run_ticks_with(
            settings(|_| {}),
            with(&[(Priority::Normal, screen)]),
            vec![vec![a], vec![b]],
        );
        assert_eq!(kicked(&mut rx1), (43, "Seen on screen".to_owned()));
    }

    #[test]
    fn the_highest_priority_decides_and_equal_priorities_run_in_registration_order() {
        let cancel: Hook = PlayerDuplicateLogin::cancel;
        let uncancel: Hook = PlayerDuplicateLogin::uncancel;
        let cases = [
            // Registered out of order: the sort puts Lowest first, so Highest decides.
            (
                with(&[(Priority::Highest, uncancel), (Priority::Lowest, cancel)]),
                [2],
            ),
            (
                with(&[(Priority::Highest, cancel), (Priority::Lowest, uncancel)]),
                [1],
            ),
            (
                with(&[(Priority::Normal, cancel), (Priority::Normal, uncancel)]),
                [2],
            ),
            (
                with(&[(Priority::Normal, uncancel), (Priority::Normal, cancel)]),
                [1],
            ),
            (
                with(&[(Priority::High, cancel), (Priority::Low, uncancel)]),
                [1],
            ),
        ];
        for (listeners, remaining) in cases {
            let (a, _rx1) = join_as(1, 0, person("Alex", Some(5)));
            let (b, _rx2) = join_as(2, 0, person("Alex", Some(5)));
            let state = run_ticks_with(settings(|_| {}), listeners, vec![vec![a], vec![b]]);
            assert_eq!(sessions(&state), remaining);
        }
    }

    #[test]
    fn offline_names_match_ignoring_ascii_case_and_online_players_match_by_xuid() {
        let cases = [
            (person("Steve", None), person("sTEVE", None), vec![2]),
            (person("Steve", None), person("Steve2", None), vec![1, 2]),
            (person("Steve", None), person("Ste ve", None), vec![1, 2]),
            (
                person("Steve", Some(1)),
                person("Steve", Some(2)),
                vec![1, 2],
            ),
            (person("Steve", Some(1)), person("Alex", Some(1)), vec![2]),
        ];
        for (first, second, remaining) in cases {
            let (a, _rx1) = join_as(1, 0, first);
            let (b, _rx2) = join_as(2, 0, second);
            let state = run_ticks(settings(|_| {}), vec![vec![a], vec![b]]);
            assert_eq!(sessions(&state), remaining);
        }
    }

    #[test]
    fn a_full_server_still_lets_a_player_replace_itself() {
        let full = settings(|c| c.general.max_players = 1);
        let (a, mut rx1) = join_as(1, 0, person("Steve", None));
        let (b, _rx2) = join_as(2, 0, person("STEVE", None));
        let state = run_ticks(full, vec![vec![a], vec![b]]);
        assert_eq!(sessions(&state), [2]);
        assert_eq!(kicked(&mut rx1).0, 43);
    }

    #[test]
    fn a_repeated_join_after_a_duplicate_login_changes_nothing() {
        let cancel: Hook = PlayerDuplicateLogin::cancel;
        let cancels: [&[(Priority, Hook)]; 2] = [&[], &[(Priority::Normal, cancel)]];
        for (listeners, remaining) in cancels.into_iter().zip([2, 1]) {
            // The receivers stay alive, so that no session leaves by a closed queue.
            let run = |repeat_at: Option<usize>| {
                let (a, _rx1) = join_as(1, 0, person("Alex", Some(5)));
                let (b, _rx2) = join_as(2, 0, person("Alex", Some(5)));
                let mut again = b.clone();
                again.seq = 1;
                let mut ticks = vec![vec![a], vec![b], vec![]];
                if let Some(tick) = repeat_at {
                    ticks[tick].push(again);
                }
                let state = run_ticks_with(settings(|_| {}), with(listeners), ticks);
                (state.hash(), sessions(&state))
            };
            let once = run(None);
            assert_eq!(once.1, [remaining]);
            assert_eq!(run(Some(1)), once, "repeated in the same tick");
            assert_eq!(run(Some(2)), once, "repeated in the next tick");
        }
    }

    #[test]
    fn duplicate_login_cycles_leave_no_player_and_no_index_entry() {
        let mut calls = 0_u32;
        let mut listeners = Listeners::default();
        // Every other duplicate login is cancelled, so both outcomes repeat.
        listeners.push(Priority::Normal, move |e: &mut PlayerDuplicateLogin| {
            calls += 1;
            if calls.is_multiple_of(2) {
                e.cancel();
            }
        });
        listeners.sort();
        let mut state = new_state(settings(|_| {}), listeners);
        let mut output = Output::default();
        for n in 0..100 {
            let (old, new) = (2 * n, 2 * n + 1);
            let (a, mut rx1) = join_as(old, 0, person("Steve", None));
            let (b, mut rx2) = join_as(new, 0, person("steve", None));
            for mut batch in [vec![a], vec![b], vec![leave(old, 1), leave(new, 1)]] {
                tick(&mut state, &mut batch, &mut output);
                assert!(state.index_matches_players());
            }
            let loser = if n % 2 == 0 { &mut rx1 } else { &mut rx2 };
            assert_eq!(kicked(loser).0, 43);
            assert!(rx1.is_closed() && rx2.is_closed());
        }
        assert!(state.players.is_empty() && state.index.is_empty());
        assert!(output.is_empty());
    }

    /// One tick of a determinism run: the state hash after it and what every session received.
    type Observed = (u64, Vec<(u64, Vec<u32>, Option<Packet>)>);

    /// A fixed-seed script of inputs for `sessions` sessions (PLAN.md 10.5, 10.6): joins (some
    /// under one name, so duplicate logins happen), chunk radius requests (some invalid), right
    /// and wrong initialisations, moves (some to positions that are not finite), sub-chunk
    /// requests, packets the tick does not handle, leaves, and sessions whose outbound queue
    /// closes, which the tick must forget. The state also runs [`script_task`].
    struct Script {
        rng: crate::protocol::Rng,
        sessions: usize,
        seq: Vec<u64>,
        joined: Vec<bool>,
        /// Receivers of joined sessions; `None` once the session dropped its end.
        receivers: BTreeMap<u64, Option<mpsc::Receiver<Outbound>>>,
    }

    impl Script {
        fn new(seed: u64, sessions: usize) -> Self {
            Self {
                rng: crate::protocol::Rng(seed),
                sessions,
                seq: vec![0; sessions],
                joined: vec![false; sessions],
                receivers: BTreeMap::new(),
            }
        }

        /// The inputs of one tick, in an arrival order unlike the applied (session, seq) order.
        fn tick(&mut self) -> Vec<Input> {
            let mut batch = Vec::new();
            for _ in 0..self.rng.below(12) {
                let i = self.rng.below(self.sessions);
                let s = u64::try_from(i).unwrap();
                self.seq[i] += 1;
                let seq = self.seq[i];
                if !self.joined[i] {
                    self.joined[i] = true;
                    // Every eighth session shares one name, so duplicate logins happen.
                    let name = if s % 8 == 0 {
                        "Twin".to_owned()
                    } else {
                        format!("p{s}")
                    };
                    let (tx, rx) = outbound_queue();
                    self.receivers.insert(s, Some(rx));
                    let kind = InputKind::Join {
                        identity: person(&name, None),
                        outbound: tx,
                    };
                    batch.push(input(s, seq, kind));
                    continue;
                }
                let kind = match self.rng.below(14) {
                    10 | 11 => {
                        let mut c =
                            || f32::from(u16::try_from(self.rng.below(400)).unwrap()) - 200.0;
                        let position = [c(), -58.0, c()];
                        moved_to(s, seq, position).kind
                    }
                    12 => moved_to(s, seq, [f32::NAN, 0.0, 0.0]).kind,
                    13 => {
                        let count = self.rng.below(6);
                        let mut c = || i8::try_from(self.rng.below(9)).unwrap() - 4;
                        let offsets = (0..count).map(|_| [c(), c(), c()]).collect();
                        sub_chunks(s, seq, [0, 0, 0], offsets).kind
                    }
                    0..=3 => {
                        let r = i32::try_from(self.rng.below(40)).unwrap() - 4;
                        packet(Packet::RequestChunkRadius(RequestChunkRadius {
                            chunk_radius: r,
                            max_chunk_radius: 0,
                        }))
                    }
                    4..=6 => packet(Packet::SetLocalPlayerAsInitialised(
                        SetLocalPlayerAsInitialised {
                            entity_runtime_id: u64::try_from(self.rng.below(80)).unwrap(),
                        },
                    )),
                    7 => packet(Packet::ClientCacheStatus(
                        crate::protocol::packet::ClientCacheStatus { enabled: true },
                    )),
                    8 => {
                        // The session goes away without telling the tick.
                        if let Some(rx) = self.receivers.get_mut(&s) {
                            *rx = None;
                        }
                        continue;
                    }
                    _ => {
                        self.joined[i] = false;
                        InputKind::Leave
                    }
                };
                batch.push(input(s, seq, kind));
            }
            batch.reverse();
            batch
        }

        /// What every session received since the last call, by session.
        fn received(&mut self) -> Vec<(u64, Vec<u32>, Option<Packet>)> {
            self.receivers
                .iter_mut()
                .filter_map(|(s, rx)| {
                    let (_, close, ids) = received(rx.as_mut()?);
                    (!ids.is_empty() || close.is_some()).then_some((*s, ids, close))
                })
                .collect()
        }
    }

    /// The task of the determinism runs: every other tick it sets a block of the spawn chunks
    /// (always loaded) to stone or air, and every 50 ticks it saves.
    fn script_task() -> Tasks {
        let stone = crate::block::State::find("minecraft:stone", &[]).unwrap();
        let mut tasks = Tasks::default();
        tasks.push(
            std::num::NonZeroU32::new(2).unwrap(),
            move |ctx: &mut task::Context<'_>| {
                let t = i32::try_from(ctx.tick() % 1000).unwrap();
                let at = BlockPos::new((t * 7) % 48 - 16, -60 + t % 3, (t * 13) % 48 - 16);
                let state = if t % 3 == 0 { crate::block::AIR } else { stone };
                assert!(ctx.world_mut().set_block(at, state).is_ok());
                if ctx.tick().is_multiple_of(50) {
                    ctx.save();
                }
            },
        );
        tasks
    }

    /// Runs the script of `seed` live with a pool of `threads` threads: the observed ticks and the
    /// log.
    fn live(threads: usize, seed: u64) -> (Vec<Observed>, InputLog) {
        let pool = crate::runtime::cpu_pool(threads).unwrap();
        let mut script = Script::new(seed, 64);
        let mut state = new_state(settings(|_| {}), Listeners::default());
        state.tasks = script_task();
        let mut output = Output::default();
        let mut log = InputLog::default();
        let mut observed = Vec::new();
        for _ in 0..400 {
            let mut batch = script.tick();
            let mut answers = std::mem::take(&mut output.answers);
            step(
                &mut state,
                &mut batch,
                &mut answers,
                &pool,
                &mut output,
                &mut log,
            );
            deliver(&mut state, &mut output, &mut log);
            boundary(&mut state, &mut output, &mut log);
            let requests = std::mem::take(&mut output.requests);
            // Commits stay unanswered: an answered commit drops the log before it, and these
            // runs replay from the start (rotation has tests of its own).
            output.answers = answer(&state, requests);
            output
                .answers
                .retain(|a| !matches!(a, Response::Committed { .. }));
            assert!(state.index_matches_players());
            observed.push((state.hash(), script.received()));
        }
        assert!(pool.shutdown());
        (observed, log)
    }

    /// Replays `ticks` of a log from the start state: the state hash after each tick and the log
    /// the replay records itself. Sessions the live run forgot after its output are forgotten at
    /// the same point.
    fn replay(ticks: &[log::Tick], listeners: Listeners, pool: &Pool) -> (Vec<u64>, InputLog) {
        let mut state = new_state(settings(|_| {}), listeners);
        state.tasks = script_task();
        let mut output = Output::default();
        let mut log = InputLog::default();
        let mut receivers = BTreeMap::new();
        let mut hashes = Vec::new();
        for t in ticks {
            assert_eq!(t.number, state.tick);
            let mut batch: Vec<Input> = t
                .inputs
                .iter()
                .map(|e| {
                    let kind = match &e.kind {
                        EntryKind::Join(identity) => {
                            let (tx, rx) = outbound_queue();
                            receivers.insert(e.session, rx);
                            InputKind::Join {
                                identity: identity.clone(),
                                outbound: tx,
                            }
                        }
                        EntryKind::Packet(p) => packet(p.clone()),
                        EntryKind::Leave => InputKind::Leave,
                    };
                    Input {
                        session: e.session,
                        seq: e.seq,
                        kind,
                    }
                })
                .collect();
            let mut answers: Vec<Response> = t
                .responses
                .iter()
                .map(|a| match a {
                    log::Answer::Chunk { world, pos, origin } => Response::Chunk {
                        world: *world,
                        pos: *pos,
                        loaded: match origin {
                            log::Origin::Damaged => Loaded::Damaged,
                            log::Origin::Disk | log::Origin::Generated => {
                                let seed = state.worlds[*world].seed();
                                Loaded::Generated(Box::new(Chunk::generate(&Flat, seed, *pos)))
                            }
                        },
                    },
                    log::Answer::Player { key, data } => Response::Player {
                        key: crate::storage::PlayerKey::from_text(key),
                        data: data.clone(),
                    },
                    log::Answer::Committed(start) => Response::Committed { start: *start },
                    log::Answer::CommitFailed(start) => Response::CommitFailed { start: *start },
                })
                .collect();
            step(
                &mut state,
                &mut batch,
                &mut answers,
                pool,
                &mut output,
                &mut log,
            );
            deliver(&mut state, &mut output, &mut log);
            for rx in receivers.values_mut() {
                received(rx);
            }
            for &session in &t.dropped {
                log.dropped(session);
                state.leave(session);
            }
            boundary(&mut state, &mut output, &mut log);
            output.requests.clear();
            hashes.push(state.hash());
        }
        (hashes, log)
    }

    /// Seeds of the determinism runs: different inputs, sessions colliding differently.
    const SEEDS: [u64; 8] = [
        0x5eed_0005,
        1,
        2,
        42,
        0xdead_beef,
        0x0123_4567_89ab_cdef,
        u64::MAX,
        0x8000_0000_0000_0000,
    ];

    #[test]
    fn one_and_many_pool_threads_give_the_same_ticks_and_the_log_replays_them() {
        let many = crate::runtime::logical_cpus().max(4);
        let one_pool = crate::runtime::cpu_pool(1).unwrap();
        let many_pool = crate::runtime::cpu_pool(many).unwrap();
        for seed in SEEDS {
            let (one, log_one) = live(1, seed);
            let (more, log_more) = live(many, seed);
            assert_eq!(one, more, "seed {seed:#x}: 1 and {many} threads");
            // Logs compare by their bytes: some moves carry NaN, which is never equal to itself.
            let bytes = log_one.encode().unwrap();
            assert_eq!(bytes, log_more.encode().unwrap(), "seed {seed:#x}");
            // The script reached what it is for: spawns, closed joins and forgotten sessions.
            let status = Packet::PlayStatus(PlayStatus { status: 0 }).id();
            let sessions = || one.iter().flat_map(|(_, sessions)| sessions);
            let spawned = sessions()
                .filter(|(_, ids, _)| ids.contains(&status))
                .count();
            let closed = sessions().filter(|(_, _, close)| close.is_some()).count();
            let dropped: usize = log_one.ticks().iter().map(|t| t.dropped.len()).sum();
            assert!(
                spawned > 20 && closed > 0 && dropped > 0,
                "seed {seed:#x}: {spawned} {closed} {dropped}"
            );

            // The log replays to the same states with 1 and many threads, records itself the
            // same, and reads back the same after it is written.
            let live_hashes: Vec<u64> = one.iter().map(|(h, _)| *h).collect();
            let read = InputLog::decode(&log_one.encode().unwrap()).unwrap();
            assert_eq!(read.encode().unwrap(), bytes, "seed {seed:#x}");
            assert!(!read.is_truncated());
            for pool in [&one_pool, &many_pool] {
                let (hashes, again) = replay(&read.ticks(), Listeners::default(), pool);
                assert_eq!(hashes, live_hashes, "seed {seed:#x}: replay");
                assert_eq!(
                    again.encode().unwrap(),
                    bytes,
                    "seed {seed:#x}: re-recorded"
                );
            }
        }
        assert!(one_pool.shutdown());
        assert!(many_pool.shutdown());
    }

    #[test]
    fn corrupt_logs_are_errors_without_panics() {
        let (_, log) = live(2, SEEDS[0]);
        let bytes = log.encode().unwrap();
        // A shorter seed keeps the run fast; the mutations still reach every field kind.
        let seed = &bytes[..bytes.len().min(4096)];
        crate::protocol::fuzz(seed, 10_000, |b| {
            InputLog::decode(b)
                .map(|_| ())
                .map_err(|_| crate::protocol::Error::Size)
        });
        assert!(InputLog::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(InputLog::decode(&extra).is_err());
    }

    #[test]
    fn the_log_stops_at_its_bound_without_the_partial_tick() {
        let mut log = InputLog::default();
        log.begin(0);
        log.record(SessionId::new(1), 1, EntryKind::Leave, log::MAX_BYTES / 2);
        log.begin(1);
        log.record(SessionId::new(1), 2, EntryKind::Leave, 0);
        log.record(SessionId::new(2), 1, EntryKind::Leave, log::MAX_BYTES / 2);
        assert!(log.is_truncated());
        assert_eq!(log.ticks().len(), 1, "tick 1 is incomplete and left out");
        log.begin(2);
        log.record(SessionId::new(3), 1, EntryKind::Leave, 0);
        log.dropped(SessionId::new(3));
        assert_eq!(log.ticks().len(), 1);
        let read = InputLog::decode(&log.encode().unwrap()).unwrap();
        assert!(read.is_truncated());
        assert_eq!(read.ticks(), log.ticks());
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test waits for ticks and reads the written log"
    )]
    fn a_panicking_tick_leaves_a_log_that_reproduces_the_panic() {
        let panicking = || {
            let mut listeners = Listeners::default();
            listeners.push(Priority::Normal, |_: &mut PlayerDuplicateLogin| {
                panic!("listener failed");
            });
            listeners.sort();
            listeners
        };
        let (sender, queue) = input_queue();
        let (_stop_tx, stop) = mpsc::channel(1);
        let (crash, mut crash_log) = oneshot::channel();
        let (done, _done_rx) = oneshot::channel();
        let state = new_state(settings(|_| {}), panicking());
        let pool = crate::runtime::cpu_pool(2).unwrap();
        let (requests, _request_queue) = mpsc::channel(crate::save::REQUEST_CAPACITY);
        let (_responder, responses) = mpsc::channel(crate::save::RESPONSE_CAPACITY);
        let link = SaveLink {
            requests,
            responses,
        };
        let thread = crate::runtime::spawn_tick(move || {
            run(state, &pool, queue, link, stop, crash, done);
        })
        .unwrap();
        let join = |session: u64| {
            let (tx, rx) = outbound_queue();
            let mut inputs = sender.session(SessionId::new(session));
            inputs
                .try_send(InputKind::Join {
                    identity: person("Same", None),
                    outbound: tx,
                })
                .unwrap();
            rx
        };
        let _first = join(1);
        // The first join applies in a tick of its own before the duplicate arrives.
        std::thread::sleep(TICK * 3);
        let _second = join(2);
        assert!(
            thread.join().is_err(),
            "the listener panic ends the tick thread"
        );
        let log = crash_log.try_recv().expect("the log is handed over");

        let dir = std::env::temp_dir().join(format!("anchor-crash-{}", std::process::id()));
        let path = write_crash_log(&dir, &log).unwrap();
        assert!(path.starts_with(dir.join("crash")));
        let read = InputLog::decode(&std::fs::read(&path).unwrap()).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(read.ticks(), log.ticks());

        // The last tick holds the join that panicked. The ticks before it replay to a server with
        // the first player; the whole log panics again where the server did.
        let ticks = read.ticks();
        let (last, before) = ticks.split_last().unwrap();
        assert!(
            last.inputs
                .iter()
                .any(|e| e.session == SessionId::new(2) && matches!(e.kind, EntryKind::Join(_)))
        );
        let pool = crate::runtime::cpu_pool(2).unwrap();
        let (hashes, _) = replay(before, panicking(), &pool);
        assert_eq!(hashes.len(), before.len());
        let replayed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            replay(&ticks, panicking(), &pool)
        }));
        assert!(replayed.is_err(), "the replay reproduces the panic");
        assert!(pool.shutdown());
    }

    // M6 (PLAN.md 10.6): chunk views, sub-chunk answers, block changes, saving.

    fn moved_to(session: u64, seq: u64, position: [f32; 3]) -> Input {
        let p = crate::protocol::packet::PlayerAuthInput {
            position,
            ..Default::default()
        };
        input(session, seq, packet(Packet::PlayerAuthInput(Box::new(p))))
    }

    fn sub_chunks(session: u64, seq: u64, position: [i32; 3], offsets: Vec<[i8; 3]>) -> Input {
        let p = crate::protocol::packet::SubChunkRequest {
            dimension: 0,
            offsets,
            position,
        };
        input(session, seq, packet(Packet::SubChunkRequest(p)))
    }

    /// The chunk positions of the `LevelChunk`s among `packets`.
    fn level_chunks(packets: &[Packet]) -> Vec<[i32; 2]> {
        packets
            .iter()
            .filter_map(|p| match p {
                Packet::LevelChunk(c) => Some(c.position),
                _ => None,
            })
            .collect()
    }

    /// Runs empty ticks until every load is answered and every view was sent.
    fn settle(state: &mut State, output: &mut Output) {
        for _ in 0..128 {
            tick(state, &mut Vec::new(), output);
            let complete = state.players.values().all(|p| p.view.is_complete());
            if output.answers.is_empty() && state.chunks.is_idle() && complete {
                return;
            }
        }
        panic!("loads did not settle");
    }

    #[test]
    fn a_view_loads_and_sends_every_chunk_of_its_disc_once_and_releases_them_on_leave() {
        let mut state = new_state(
            settings(|c| c.developer.chunks_per_tick = 7),
            Listeners::default(),
        );
        let mut output = Output::default();
        let held = state.chunks.held();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 8)], &mut output);
        settle(&mut state, &mut output);
        let mut sent = level_chunks(&received(&mut rx).0);
        assert_eq!(sent.len(), view::disc(ChunkPos::new(0, 0), 8).len());
        // Nearest first within each tick's batch, and no chunk twice.
        let count = sent.len();
        sent.sort_unstable();
        sent.dedup();
        assert_eq!(sent.len(), count);
        tick(&mut state, &mut vec![leave(1, 2)], &mut output);
        assert_eq!(state.chunks.held(), held, "only the spawn chunks stay held");
        assert!(state.chunks.is_idle());
        let world = &state.worlds[0];
        assert!(
            world.is_chunk_loaded(ChunkPos::new(1, 1)),
            "spawn chunks stay loaded"
        );
        assert!(
            !world.is_chunk_loaded(ChunkPos::new(8, 0)),
            "the view's chunks unload"
        );
    }

    #[test]
    fn moving_sends_the_new_chunks_and_ignores_positions_outside_the_world() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 2)], &mut output);
        settle(&mut state, &mut output);
        received(&mut rx);
        for bad in [f32::NAN, f32::INFINITY, 4.0e7] {
            tick(
                &mut state,
                &mut vec![moved_to(1, 2, [bad, 0.0, 0.0])],
                &mut output,
            );
            assert_eq!(
                state.players[&SessionId::new(1)].position,
                [0.5, -58.38, 0.5]
            );
        }
        tick(
            &mut state,
            &mut vec![moved_to(1, 3, [160.5, -58.38, 0.5])],
            &mut output,
        );
        settle(&mut state, &mut output);
        let packets = received(&mut rx).0;
        assert!(matches!(
            packets.first(),
            Some(Packet::NetworkChunkPublisherUpdate(p)) if p.position == [160, -59, 0] && p.radius == 32
        ));
        let mut chunks = level_chunks(&packets);
        chunks.sort_unstable();
        let mut expected: Vec<[i32; 2]> = view::disc(ChunkPos::new(10, 0), 2)
            .into_iter()
            .map(|p| [p.x, p.z])
            .collect();
        expected.sort_unstable();
        assert_eq!(
            chunks, expected,
            "the old view shares no chunk with the new one"
        );
        assert!(state.players[&SessionId::new(1)].data_dirty);
    }

    #[test]
    fn sub_chunk_requests_get_bds_answers() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 2)], &mut output);
        let offsets: Vec<[i8; 3]> = (-4..=19).map(|y| [0, y, 0]).collect();
        tick(
            &mut state,
            &mut vec![
                sub_chunks(1, 2, [0, 0, 0], offsets),
                sub_chunks(1, 3, [0, 0, 0], vec![[0, -5, 0], [0, 20, 0], [100, 0, 0]]),
            ],
            &mut output,
        );
        let answers: Vec<_> = received(&mut rx)
            .0
            .into_iter()
            .filter_map(|p| match p {
                Packet::SubChunk(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(answers.len(), 2, "one answer per request");
        let all = &answers[0].sub_chunk_entries;
        assert_eq!(all.len(), 24);
        // The terrain: a payload equal to the flat chunk's, its height map 4 everywhere.
        let ground = &all[0];
        assert_eq!(
            (
                ground.result,
                ground.height_map_type,
                ground.render_height_map_type
            ),
            (1, 1, 4)
        );
        assert_eq!(ground.height_map_data, Some([[4; 16]; 16]));
        let flat = Chunk::generate(&Flat, 0, ChunkPos::new(0, 0));
        assert_eq!(ground.raw_payload, flat.sub_chunk_payload(0));
        // Above it, air with every column lower.
        for e in &all[1..] {
            assert_eq!(
                (e.result, e.height_map_type, e.render_height_map_type),
                (6, 3, 3)
            );
            assert_eq!((e.raw_payload.as_ref(), e.height_map_data), (None, None));
        }
        let results: Vec<_> = answers[1]
            .sub_chunk_entries
            .iter()
            .map(|e| (e.result, e.height_map_type, e.render_height_map_type))
            .collect();
        assert_eq!(results, [(5, 0, 0), (5, 0, 0), (2, 0, 0)]);
    }

    /// A task that sets the block of `script` for its tick, if any, and saves on `save_at`.
    fn block_task(script: Vec<(u64, BlockPos, crate::block::State)>, save_at: Vec<u64>) -> Tasks {
        let mut tasks = Tasks::default();
        tasks.push(
            std::num::NonZeroU32::MIN,
            move |ctx: &mut task::Context<'_>| {
                for &(at, pos, state) in &script {
                    if at == ctx.tick() {
                        ctx.world_mut().set_block(pos, state).unwrap();
                    }
                }
                if save_at.contains(&ctx.tick()) {
                    ctx.save();
                }
            },
        );
        tasks
    }

    fn state_with_tasks(tasks: Tasks) -> State {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        state.tasks = tasks;
        state
    }

    use crate::world::BlockPos;

    #[test]
    fn block_changes_reach_the_players_who_were_sent_the_chunk() {
        let stone = crate::block::State::find("minecraft:stone", &[]).unwrap();
        let script = vec![
            (2, BlockPos::new(1, -60, 1), stone),
            (3, BlockPos::new(2, -60, 1), stone),
            (3, BlockPos::new(3, -60, 1), stone),
            // Setting the same state again changes nothing.
            (4, BlockPos::new(1, -60, 1), stone),
        ];
        let mut state = state_with_tasks(block_task(script, vec![]));
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 2)], &mut output);
        received(&mut rx);
        let mut updates = Vec::new();
        for _ in 0..4 {
            tick(&mut state, &mut Vec::new(), &mut output);
            updates.push(
                received(&mut rx)
                    .0
                    .into_iter()
                    .filter(|p| {
                        matches!(p, Packet::UpdateBlock(_) | Packet::UpdateSubChunkBlocks(_))
                    })
                    .collect::<Vec<_>>(),
            );
        }
        let id = stone.network_id();
        assert_eq!(updates[0], []);
        assert_eq!(
            updates[1],
            [Packet::UpdateBlock(crate::protocol::packet::UpdateBlock {
                position: [1, -60, 1],
                new_block_runtime_id: id,
                flags: 3,
                layer: 0,
            })]
        );
        let Packet::UpdateSubChunkBlocks(both) = &updates[2][0] else {
            panic!("two blocks of one sub-chunk go in one packet");
        };
        assert_eq!(both.position, [0, -64, 0]);
        assert_eq!(both.blocks.len(), 2);
        assert_eq!(updates[3], [], "an unchanged block sends nothing");
    }

    #[test]
    fn a_save_holds_changed_chunks_and_players_and_memory_waits_for_its_commit() {
        let stone = crate::block::State::find("minecraft:stone", &[]).unwrap();
        let far = BlockPos::new(150, -60, 0);
        let script = vec![(5, far, stone)];
        let mut state = state_with_tasks(block_task(script, vec![6]));
        state.saving.players = true;
        let mut output = Output::default();
        let (j, _rx) = join(1, 0);
        // The player data loads, then the view around the far block opens.
        tick(&mut state, &mut vec![j], &mut output);
        tick(&mut state, &mut vec![radius(1, 1, 1)], &mut output);
        tick(
            &mut state,
            &mut vec![moved_to(1, 2, [150.5, -58.38, 0.5])],
            &mut output,
        );
        while state.tick < 9 {
            tick(&mut state, &mut Vec::new(), &mut output);
        }
        settle(&mut state, &mut output);
        // The snapshot of tick 7 held the changed chunk and the moved player; the stand-in
        // committed it, so nothing waits in memory.
        assert!(state.saving.is_idle() && state.chunks.unsaved.is_empty());
        // A change, then the view leaves before any save: the chunk waits in memory.
        let pos = far.chunk();
        state.worlds[0]
            .set_block(BlockPos::new(151, -60, 0), stone)
            .unwrap();
        tick(
            &mut state,
            &mut vec![moved_to(1, 3, [0.5, -58.38, 0.5])],
            &mut output,
        );
        assert!(!state.worlds[0].is_chunk_loaded(pos));
        assert!(
            state
                .chunks
                .unsaved
                .get(&(0, pos))
                .is_some_and(|u| u.snapshot.is_none())
        );
        // Coming back takes it from memory, with its change and still dirty.
        tick(
            &mut state,
            &mut vec![moved_to(1, 4, [150.5, -58.38, 0.5])],
            &mut output,
        );
        assert!(state.worlds[0].is_chunk_loaded(pos));
        assert_eq!(
            state.worlds[0].block(BlockPos::new(151, -60, 0)),
            Some(stone)
        );
        assert!(state.worlds[0].chunk(pos).unwrap().is_dirty());
        assert!(
            !state.chunks.requested.contains(&(0, pos)),
            "no load from the file"
        );
    }

    #[test]
    fn a_player_who_left_before_a_commit_rejoins_with_the_data_in_memory() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        state.saving.players = true;
        let mut output = Output::default();
        let (j, _rx) = join(1, 0);
        tick(&mut state, &mut vec![j], &mut output);
        assert_eq!(state.players[&SessionId::new(1)].phase, Phase::Loading);
        tick(&mut state, &mut Vec::new(), &mut output);
        assert_eq!(state.players[&SessionId::new(1)].phase, Phase::Joined);
        tick(
            &mut state,
            &mut vec![radius(1, 1, 2), moved_to(1, 2, [3.5, -58.38, 3.5])],
            &mut output,
        );
        tick(&mut state, &mut vec![leave(1, 3)], &mut output);
        assert_eq!(state.saving.offline.len(), 1);
        let (j, _rx) = join_as(2, 0, identity(1));
        tick(&mut state, &mut vec![j], &mut output);
        let player = &state.players[&SessionId::new(2)];
        assert_eq!(
            (player.phase, player.position),
            (Phase::Joined, [3.5, -58.38, 3.5])
        );
        assert!(player.data_dirty && state.saving.offline.is_empty());
        assert!(output.answers.is_empty(), "no load from the file");
    }

    #[test]
    fn damaged_player_data_refuses_the_join() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        state.saving.players = true;
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j], &mut output);
        for a in &mut output.answers {
            if let Response::Player { data, .. } = a {
                *data = PlayerLoad::Damaged;
            }
        }
        tick(&mut state, &mut Vec::new(), &mut output);
        assert!(state.players.is_empty() && state.loading.is_empty());
        assert_eq!(kicked(&mut rx).0, 0);
    }

    #[test]
    fn repeated_requests_and_block_changes_change_nothing_twice() {
        let stone = crate::block::State::find("minecraft:stone", &[]).unwrap();
        let at = BlockPos::new(1, -60, 1);
        let run = |extra: bool| {
            let script = if extra {
                vec![(3, at, stone), (3, at, stone), (4, at, stone)]
            } else {
                vec![(3, at, stone)]
            };
            let mut state = state_with_tasks(block_task(script, vec![]));
            let mut output = Output::default();
            let (j, _rx) = join(1, 0);
            let mut first = vec![j, radius(1, 1, 2)];
            let request = |seq| sub_chunks(1, seq, [0, 0, 0], vec![[0, -4, 0]]);
            first.push(request(2));
            if extra {
                first.push(request(3));
                first.push(radius(1, 4, 2));
            }
            tick(&mut state, &mut first, &mut output);
            for _ in 0..3 {
                tick(&mut state, &mut Vec::new(), &mut output);
            }
            state.hash()
        };
        assert_eq!(run(false), run(true));
    }

    #[test]
    fn a_damaged_chunk_is_shown_empty_and_cannot_change() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 8)], &mut output);
        let damaged = ChunkPos::new(7, 0);
        for a in &mut output.answers {
            if let Response::Chunk { pos, loaded, .. } = a
                && *pos == damaged
            {
                *loaded = Loaded::Damaged;
            }
        }
        settle(&mut state, &mut output);
        assert!(state.worlds[0].is_quarantined(damaged));
        let stone = crate::block::State::find("minecraft:stone", &[]).unwrap();
        assert!(
            state.worlds[0]
                .set_block(BlockPos::new(112, -60, 0), stone)
                .is_err()
        );
        let sent = received(&mut rx).0;
        let Some(Packet::LevelChunk(empty)) = sent
            .iter()
            .find(|p| matches!(p, Packet::LevelChunk(c) if c.position == [7, 0]))
        else {
            panic!("the damaged chunk is sent");
        };
        assert_eq!(empty.sub_chunk_limit, Some(0));
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test plays the save thread on its own thread"
    )]
    fn a_failed_commit_stops_the_tick_without_saving_and_hands_over_the_log() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        state.tasks = block_task(vec![], vec![1]);
        let (_sender, queue) = input_queue();
        let (_stop_tx, stop) = mpsc::channel(1);
        let (crash, _crash_log) = oneshot::channel();
        let (done, _done_rx) = oneshot::channel();
        let (requests, mut request_queue) = mpsc::channel(crate::save::REQUEST_CAPACITY);
        let (responder, responses) = mpsc::channel(crate::save::RESPONSE_CAPACITY);
        let pool = crate::runtime::cpu_pool(2).unwrap();
        let link = SaveLink {
            requests,
            responses,
        };
        let thread =
            crate::runtime::spawn_tick(move || run(state, &pool, queue, link, stop, crash, done))
                .unwrap();
        let start = loop {
            match request_queue.blocking_recv() {
                Some(Request::Commit(snapshot)) => break snapshot.start,
                Some(_) => {}
                None => panic!("the tick ended before its snapshot"),
            }
        };
        assert_eq!(
            start, 2,
            "the task asked at tick 1, so the snapshot is before tick 2"
        );
        responder
            .blocking_send(Response::CommitFailed { start })
            .unwrap();
        let End::SaveFailed(log) = thread.join().unwrap() else {
            panic!("a failed commit ends the tick");
        };
        assert_eq!(
            log.start(),
            0,
            "the log since the last commit, which never came"
        );
        // No final snapshot follows a failed commit.
        while let Ok(request) = request_queue.try_recv() {
            assert!(!matches!(request, Request::Commit(_)));
        }
    }
}
