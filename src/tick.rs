//! The tick loop, the tick step and the game state.
//!
//! One thread owns [`State`] and changes it only in [`step`], once per tick, from the inputs the
//! sessions queued (PLAN.md 5 and 10.2). Outputs leave through each session's bounded queue.

pub(crate) mod queue;
mod spawn;

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::sync::mpsc::error::{TryRecvError, TrySendError};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, trace, warn};

use crate::clock;
use crate::config::{Config, Difficulty, GameMode};
use crate::event::Listeners;
use crate::protocol::packet::{Disconnect, Packet};

use queue::{INPUT_CAPACITY, Identity, Input, InputKind, Outbound, SessionId};

/// The length of a tick: 20 ticks per second.
pub(crate) const TICK: Duration = Duration::from_millis(50);
/// The most inputs one tick applies; the rest wait for the next tick. A full input queue drains in
/// 8 ticks, and the batch buffer is allocated once at this size.
pub(crate) const MAX_INPUTS_PER_TICK: usize = INPUT_CAPACITY / 8;
/// `Disconnect` reason for a server shutdown (gophertunnel `DisconnectReasonShutdown`).
const DISCONNECT_REASON_SHUTDOWN: i32 = 68;
/// `Disconnect` reason for a duplicate login (gophertunnel `DisconnectReasonLoggedInOtherLocation`).
const DISCONNECT_REASON_LOGGED_IN_OTHER_LOCATION: i32 = 43;

/// The settings the tick reads, taken from [`Config`] once at start.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    motd: String,
    game_mode: GameMode,
    difficulty: Difficulty,
    hardcore: bool,
    seed: i64,
    view_distance: u8,
    spawn_radius: u8,
    tick_radius: u8,
    max_players: u32,
    shutdown_message: String,
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
            seed: general.level_seed.unwrap_or(0),
            view_distance: general.view_distance,
            spawn_radius: developer.spawn_radius.min(general.view_distance),
            tick_radius: developer.tick_radius.min(general.view_distance),
            max_players: general.max_players,
            shutdown_message: general.shutdown_message.clone(),
        }
    }
}

/// The game state.
#[derive(Debug)]
pub(crate) struct State {
    settings: Settings,
    /// Engine time: ticks applied so far.
    tick: u64,
    /// The next entity runtime ID; IDs are never reused.
    next_runtime_id: u64,
    players: BTreeMap<SessionId, Player>,
    /// The session of each joined player by [`PlayerKey`]: `index[p.key] == s` exactly when
    /// `players[s] == p`. Derived from `players`, so it changes only where `players` does.
    index: BTreeMap<PlayerKey, SessionId>,
    /// The user's event listeners, called only in [`step`].
    listeners: Listeners,
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
    position: [f32; 3],
    /// The granted chunk radius, once the client asked.
    chunk_radius: Option<u8>,
    phase: Phase,
    outbound: mpsc::Sender<Outbound>,
}

/// How far a player is through spawning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// `StartGame` sent.
    Joined,
    /// Spawn chunks and `PlayStatus(PlayerSpawn)` sent.
    Spawning,
    /// The client confirmed with `SetLocalPlayerAsInitialised`.
    Spawned,
}

impl State {
    /// An empty world whose events go to `listeners` (already sorted).
    pub(crate) fn new(settings: Settings, listeners: Listeners) -> Self {
        Self {
            settings,
            tick: 0,
            next_runtime_id: 1,
            players: BTreeMap::new(),
            index: BTreeMap::new(),
            listeners,
        }
    }

    /// A hash of everything a replay must reproduce, independent of thread count, OS and the
    /// order of the map in memory (FNV-1a 64 over fields in a fixed order and width).
    ///
    /// The index and each player's key are left out: both are functions of the hashed sessions
    /// and identities ([`Self::index_matches_players`] checks this in tests). Listeners are code,
    /// fixed at build.
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
            for c in p.position {
                h.u64(u64::from(c.to_bits()));
            }
            h.u64(p.chunk_radius.map_or(0, |r| u64::from(r) + 1));
            h.u64(match p.phase {
                Phase::Joined => 0,
                Phase::Spawning => 1,
                Phase::Spawned => 2,
            });
        }
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

    /// Removes and returns the player of `session`, if any, with its index entry.
    fn remove(&mut self, session: SessionId) -> Option<Player> {
        let player = self.players.remove(&session)?;
        self.index.remove(&player.key);
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
        self.index.clear();
        for (session, p) in std::mem::take(&mut self.players) {
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
    /// Packets per session, in order; one entry per run of the same session.
    packets: Vec<(SessionId, Vec<Packet>)>,
    /// Final packets for sessions the tick has already forgotten.
    closes: Vec<(mpsc::Sender<Outbound>, Packet)>,
}

impl Output {
    fn send(&mut self, session: SessionId, packet: Packet) {
        match self.packets.last_mut() {
            Some((last, packets)) if *last == session => packets.push(packet),
            _ => self.packets.push((session, vec![packet])),
        }
    }

    fn close(&mut self, outbound: mpsc::Sender<Outbound>, packet: Packet) {
        self.closes.push((outbound, packet));
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.packets.is_empty() && self.closes.is_empty()
    }
}

/// Applies one tick of `inputs`, leaving `inputs` empty. Inputs are applied in (session, seq)
/// order, so the result does not depend on how sessions interleaved in the queue.
pub(crate) fn step(state: &mut State, inputs: &mut Vec<Input>, output: &mut Output) {
    // Keys are unique per input, so the unstable sort is deterministic and allocates nothing.
    inputs.sort_unstable_by_key(|i| (i.session, i.seq));
    for input in inputs.drain(..) {
        let session = input.session;
        match input.kind {
            InputKind::Join { identity, outbound } => {
                spawn::join(state, session, identity, outbound, output);
            }
            InputKind::Packet(Packet::RequestChunkRadius(p)) => {
                spawn::request_chunk_radius(state, session, &p, output);
            }
            InputKind::Packet(Packet::SetLocalPlayerAsInitialised(p)) => {
                spawn::initialised(state, session, &p);
            }
            InputKind::Packet(p) => {
                trace!(
                    session = session.get(),
                    id = p.id(),
                    "packet not handled yet"
                );
            }
            InputKind::Leave => state.leave(session),
        }
    }
    state.tick = state.tick.saturating_add(1);
}

/// Sends `output` to the sessions. A session whose queue is full or closed is forgotten as if it
/// had left; the tick never waits.
fn deliver(state: &mut State, output: &mut Output) {
    for (session, packets) in output.packets.drain(..) {
        let Some(player) = state.players.get(&session) else {
            continue;
        };
        match player.outbound.try_send(Outbound::Packets(packets)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                warn!(
                    session = session.get(),
                    "outbound queue full; dropping the session"
                );
                state.leave(session);
            }
            Err(TrySendError::Closed(_)) => state.leave(session),
        }
    }
    for (outbound, packet) in output.closes.drain(..) {
        // The session closes when the queue does; a full or closed queue only loses the reason.
        if outbound.try_send(Outbound::Close(packet)).is_err() {
            debug!("could not queue the final packet of a closed session");
        }
    }
}

/// The tick thread: runs ticks every [`TICK`] until `stop` receives or closes, then disconnects
/// every player and returns. `_done` drops when the thread ends, normally or by panic.
pub(crate) fn run(
    mut state: State,
    mut inputs: mpsc::Receiver<Input>,
    mut stop: mpsc::Receiver<()>,
    _done: oneshot::Sender<()>,
) {
    let mut batch = Vec::with_capacity(MAX_INPUTS_PER_TICK);
    let mut output = Output::default();
    let mut pacer = clock::Pacer::new(TICK, clock::now());
    info!("tick started");
    loop {
        if !matches!(stop.try_recv(), Err(TryRecvError::Empty)) {
            break;
        }
        drain(&mut inputs, &mut batch);
        step(&mut state, &mut batch, &mut output);
        deliver(&mut state, &mut output);
        clock::sleep_until(pacer.schedule(clock::now()));
    }
    state.close_all(&mut output);
    deliver(&mut state, &mut output);
    info!(ticks = state.tick, "tick stopped");
}

/// Moves up to [`MAX_INPUTS_PER_TICK`] queued inputs into `batch` without waiting.
fn drain(inputs: &mut mpsc::Receiver<Input>, batch: &mut Vec<Input>) {
    while batch.len() < MAX_INPUTS_PER_TICK {
        match inputs.try_recv() {
            Ok(input) => batch.push(input),
            // Disconnected: every session sender is gone (networking stopped).
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

    fn settings(edit: impl FnOnce(&mut Config)) -> Settings {
        let mut config = Config::default();
        edit(&mut config);
        Settings::new(&config)
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
        input(
            session,
            seq,
            InputKind::Packet(Packet::RequestChunkRadius(p)),
        )
    }

    fn initialised(session: u64, seq: u64, entity_runtime_id: u64) -> Input {
        let p = SetLocalPlayerAsInitialised { entity_runtime_id };
        let kind = InputKind::Packet(Packet::SetLocalPlayerAsInitialised(p));
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
        let mut state = State::new(settings, listeners);
        let mut output = Output::default();
        for mut batch in ticks {
            step(&mut state, &mut batch, &mut output);
            assert!(batch.is_empty());
            deliver(&mut state, &mut output);
            assert!(output.is_empty());
            assert!(state.index_matches_players());
        }
        state
    }

    /// Every packet queued on `rx`, and the final packet if the session was closed.
    fn received(rx: &mut mpsc::Receiver<Outbound>) -> (Vec<Packet>, Option<Packet>) {
        let mut packets = Vec::new();
        let mut close = None;
        while let Ok(out) = rx.try_recv() {
            match out {
                Outbound::Packets(p) => packets.extend(p),
                Outbound::Close(p) => close = Some(p),
            }
        }
        (packets, close)
    }

    fn ids(packets: &[Packet]) -> Vec<u32> {
        packets.iter().map(Packet::id).collect()
    }

    fn spawn_status_count(packets: &[Packet]) -> usize {
        packets
            .iter()
            .filter(|p| matches!(p, Packet::PlayStatus(s) if s.status == 3))
            .count()
    }

    /// A full spawn of sessions 1..=n, one input list per session; session k gets runtime ID k.
    fn spawn_inputs(n: u64) -> (Vec<Vec<Input>>, Vec<mpsc::Receiver<Outbound>>) {
        let mut per_session = Vec::new();
        let mut receivers = Vec::new();
        for s in 1..=n {
            let (j, rx) = join(s, 0);
            receivers.push(rx);
            per_session.push(vec![j, radius(s, 1, 8), initialised(s, 2, s)]);
        }
        (per_session, receivers)
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
            let (per_session, mut receivers) = spawn_inputs(3);
            let state = run_ticks(settings(|_| {}), vec![order(per_session)]);
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
        assert_eq!(ids(&received(&mut rx).0), [11, 162, 11, 162, 11, 162]);
        // The duplicates got nothing and their queues were dropped.
        for rx in [&mut rx2, &mut rx3] {
            assert_eq!(received(rx), (vec![], None));
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
            let mut first = vec![j, radius(1, 1, 4), initialised(1, 2, 1)];
            first.extend(extra);
            run_ticks(settings(|_| {}), vec![first, later])
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
        let mut state = State::new(settings(|_| {}), Listeners::default());
        let mut batch = Vec::with_capacity(MAX_INPUTS_PER_TICK);
        let mut output = Output::default();
        let start = (state.players.len(), queue.len(), batch.capacity());
        let mut tick = |queue: &mut mpsc::Receiver<Input>, state: &mut State| {
            drain(queue, &mut batch);
            step(state, &mut batch, &mut output);
            deliver(state, &mut output);
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
            let request = InputKind::Packet(Packet::RequestChunkRadius(request));
            inputs.try_send(request).unwrap();
            tick(&mut queue, &mut state);
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
        assert_eq!((state.players.len(), queue.len(), batch.capacity()), start);
        assert!(output.is_empty());
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
        let (packets, close) = received(&mut rx2);
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
        assert_eq!(ids(&received(&mut rx).0), [11, 162]);
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
            let chunks = packets
                .iter()
                .filter(|p| matches!(p, Packet::LevelChunk(_)))
                .count();
            let expected = ChunkRadiusUpdated {
                chunk_radius: granted,
            };
            assert_eq!(packets.get(2), Some(&Packet::ChunkRadiusUpdated(expected)));
            // A disc of radius min(spawn radius, granted): 5 chunks for r = 1, 13 for r = 2.
            assert_eq!(chunks, if granted == 1 { 5 } else { 13 }, "r = {requested}");
        }
        for bad in [0, -1, i32::MIN] {
            let (j, mut rx) = join(1, 0);
            let state = run_ticks(config.clone(), vec![vec![j, radius(1, 1, bad)]]);
            assert_eq!(received(&mut rx).0.len(), 2, "only the start-game packets");
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
            c.general.level_seed = Some(-42);
        });
        let (j, mut rx) = join(1, 0);
        run_ticks(config, vec![vec![j, radius(1, 1, 2)]]);
        let packets = received(&mut rx).0;
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
        assert_eq!(
            (start.world_seed, start.world_name.as_str()),
            (-42, "Lobby")
        );
        assert_eq!(start.game_version, crate::protocol::packet::GAME_VERSION);
        // StartGame, ItemRegistry, ChunkRadiusUpdated, NetworkChunkPublisherUpdate, a chunk.
        assert_eq!(ids(&packets)[..5], [11, 162, 70, 121, 58]);
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
        deliver(&mut state, &mut output);
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
            assert_eq!(
                ids(&received(&mut rx2).0),
                [11, 162],
                "the new player joined"
            );
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
        let mut state = State::new(settings(|_| {}), listeners);
        let mut output = Output::default();
        for n in 0..100 {
            let (old, new) = (2 * n, 2 * n + 1);
            let (a, mut rx1) = join_as(old, 0, person("Steve", None));
            let (b, mut rx2) = join_as(new, 0, person("steve", None));
            for mut batch in [vec![a], vec![b], vec![leave(old, 1), leave(new, 1)]] {
                step(&mut state, &mut batch, &mut output);
                deliver(&mut state, &mut output);
                assert!(state.index_matches_players());
            }
            let loser = if n % 2 == 0 { &mut rx1 } else { &mut rx2 };
            assert_eq!(kicked(loser).0, 43);
            assert!(rx1.is_closed() && rx2.is_closed());
        }
        assert!(state.players.is_empty() && state.index.is_empty());
        assert!(output.is_empty());
    }
}
