//! Save snapshots (PLAN.md 10.6): when the tick takes one, and what it keeps in memory until the
//! save thread reports the commit.
//!
//! A snapshot holds everything changed since the last one. Until the tick learns that a
//! snapshot is committed, the data in it stays in memory even when its chunk unloads or its
//! player leaves, and a later load takes it from memory. So a load from the file always happens
//! after the commit that wrote the newest data, and never reads an older one.

use std::collections::{BTreeMap, BTreeSet};

use tracing::{debug, error};

use super::log::InputLog;
use super::view::Key;
use super::{Phase, Player, PlayerKey, State};
use crate::save::{PlayerData, Request, Snapshot};
use crate::storage::PlayerKey as StoredPlayer;
use crate::world::{Chunk, Kind};

/// Unloaded chunks waiting for a commit beyond which the tick asks for one at once.
const MAX_UNSAVED_CHUNKS: usize = 4096;
/// Players who left with data waiting for a commit beyond which the tick asks for one at once.
const MAX_OFFLINE_PLAYERS: usize = 4096;

/// A chunk unloaded before a commit held its last changes.
#[derive(Debug)]
pub(super) struct Unsaved {
    pub(super) chunk: Chunk,
    /// The snapshot that holds its changes, or `None` if none does yet.
    pub(super) snapshot: Option<u64>,
}

/// The data of a player who left before a commit held its last changes.
#[derive(Debug)]
pub(super) struct Offline {
    pub(super) data: PlayerData,
    /// The snapshot that holds the data, or `None` if none does yet.
    pub(super) snapshot: Option<u64>,
}

/// When the next snapshot is due and what the snapshot in flight holds.
#[derive(Debug)]
pub(super) struct Saving {
    /// Ticks between snapshots (`save-interval-minutes`).
    interval: u64,
    /// The tick the next periodic snapshot is due.
    next_due: u64,
    /// A task or command asked for a save at the next tick boundary.
    pub(super) requested: bool,
    /// The snapshot sent to the save thread and not answered yet.
    in_flight: Option<InFlight>,
    /// Players who left, by storage key.
    pub(super) offline: BTreeMap<StoredPlayer, Offline>,
    /// Whether player data is kept (`save-player-data`).
    pub(super) players: bool,
}

/// What the snapshot in flight holds.
#[derive(Debug)]
struct InFlight {
    start: u64,
    chunks: BTreeSet<Key>,
    players: BTreeSet<StoredPlayer>,
}

impl Saving {
    /// Snapshots every `interval` ticks from `start`.
    pub(super) fn new(start: u64, interval: u64, players: bool) -> Self {
        Self {
            interval: interval.max(1),
            next_due: start.saturating_add(interval.max(1)),
            requested: false,
            in_flight: None,
            offline: BTreeMap::new(),
            players,
        }
    }

    /// Whether `key` holds a chunk the snapshot in flight holds.
    pub(super) fn holds_chunk(&self, key: &Key) -> Option<u64> {
        self.in_flight
            .as_ref()
            .filter(|f| f.chunks.contains(key))
            .map(|f| f.start)
    }

    /// The snapshot in flight that holds a player's data.
    fn holds_player(&self, key: &StoredPlayer) -> Option<u64> {
        self.in_flight
            .as_ref()
            .filter(|f| f.players.contains(key))
            .map(|f| f.start)
    }

    /// Whether nothing is in flight or waiting in memory (for leak tests).
    #[cfg(test)]
    pub(super) fn is_idle(&self) -> bool {
        self.in_flight.is_none() && self.offline.is_empty()
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        h.u64(self.next_due);
        h.u64(u64::from(self.requested));
        h.u64(
            self.in_flight
                .as_ref()
                .map_or(0, |f| f.start.wrapping_add(1)),
        );
        h.usize(self.offline.len());
    }
}

/// The storage key of a player (PLAN.md 6): the XUID in decimal, or `offline:` and the name in
/// ASCII lowercase.
pub(super) fn storage_key(key: &PlayerKey) -> StoredPlayer {
    match key {
        PlayerKey::Xuid(xuid) => StoredPlayer::xuid(*xuid),
        PlayerKey::Name(name) => StoredPlayer::offline(name),
    }
}

/// The data kept of a player.
pub(super) fn data_of(state: &State, player: &Player) -> PlayerData {
    PlayerData {
        world: state
            .worlds
            .get(player.world)
            .map(|w| w.name().to_owned())
            .unwrap_or_default(),
        position: player.position,
    }
}

/// Keeps the data of a leaving player in memory while no commit holds it.
pub(super) fn left(state: &mut State, player: &Player) {
    if !state.saving.players || player.phase == Phase::Loading {
        return;
    }
    let key = storage_key(&player.key);
    let snapshot = if player.data_dirty {
        None
    } else if let Some(start) = state.saving.holds_player(&key) {
        Some(start)
    } else {
        return;
    };
    let data = data_of(state, player);
    state.saving.offline.insert(key, Offline { data, snapshot });
}

/// The memory a chunk a view releases keeps until a commit holds it: its changes no snapshot
/// holds yet, or the snapshot in flight that holds them. `None` if the file has it.
pub(super) fn keep_unloaded(saving: &Saving, key: &Key, chunk: Chunk) -> Option<Unsaved> {
    let snapshot = if chunk.is_dirty() {
        None
    } else {
        Some(saving.holds_chunk(key)?)
    };
    Some(Unsaved { chunk, snapshot })
}

/// Takes a snapshot at this tick boundary if one is due (periodic, asked for, or too many unloaded
/// chunks or departed players waiting) and none is in flight, and starts a new log segment there. `last` takes the
/// final snapshot of a shutdown whatever is in flight: the save thread commits in order.
pub(super) fn snapshot(state: &mut State, log: &mut InputLog, last: bool) -> Option<Request> {
    let due = state.tick >= state.saving.next_due
        || state.saving.requested
        || state.chunks.unsaved.len() > MAX_UNSAVED_CHUNKS
        || state.saving.offline.len() > MAX_OFFLINE_PLAYERS;
    if !last && (!due || state.saving.in_flight.is_some()) {
        return None;
    }
    let start = state.tick;
    let mut snapshot = Snapshot {
        start,
        ..Snapshot::default()
    };
    let mut held = InFlight {
        start,
        chunks: BTreeSet::new(),
        players: BTreeSet::new(),
    };
    for (index, world) in state.worlds.iter_mut().enumerate() {
        if world.kind() != Kind::Persistent {
            continue;
        }
        snapshot.spawns.push((index, world.spawn()));
        for (pos, chunk) in world.take_dirty() {
            held.chunks.insert((index, pos));
            snapshot.chunks.push((index, pos, chunk));
        }
    }
    for (&(index, pos), unsaved) in &mut state.chunks.unsaved {
        if unsaved.snapshot.is_none() {
            unsaved.snapshot = Some(start);
            held.chunks.insert((index, pos));
            snapshot.chunks.push((index, pos, unsaved.chunk.clone()));
        }
    }
    if state.saving.players {
        let sessions: Vec<_> = state
            .players
            .iter()
            .filter(|(_, p)| p.data_dirty)
            .map(|(&s, _)| s)
            .collect();
        for session in sessions {
            let Some(player) = state.players.get(&session) else {
                continue;
            };
            let key = storage_key(&player.key);
            snapshot.players.push((key.clone(), data_of(state, player)));
            held.players.insert(key);
            if let Some(player) = state.players.get_mut(&session) {
                player.data_dirty = false;
            }
        }
        for (key, offline) in &mut state.saving.offline {
            if offline.snapshot.is_none() {
                offline.snapshot = Some(start);
                snapshot.players.push((key.clone(), offline.data.clone()));
                held.players.insert(key.clone());
            }
        }
    }
    debug!(
        start,
        chunks = snapshot.chunks.len(),
        players = snapshot.players.len(),
        "save snapshot taken"
    );
    log.snapshot(start);
    let saving = &mut state.saving;
    saving.requested = false;
    saving.next_due = start.saturating_add(saving.interval);
    saving.in_flight = Some(held);
    Some(Request::Commit(Box::new(snapshot)))
}

/// The snapshot taken before tick `start` is committed: the memory that held it for the commit
/// is let go, and the log now starts there.
pub(super) fn committed(state: &mut State, log: &mut InputLog, start: u64) {
    if state.saving.in_flight.as_ref().map(|f| f.start) == Some(start) {
        state.saving.in_flight = None;
    }
    state
        .chunks
        .unsaved
        .retain(|_, u| u.snapshot != Some(start));
    state
        .saving
        .offline
        .retain(|_, o| o.snapshot != Some(start));
    log.committed(start);
    debug!(start, "save committed");
}

/// The snapshot taken before tick `start` could not be committed: the tick stops without saving
/// (PLAN.md 6).
pub(super) fn failed(start: u64) {
    error!(
        start,
        "the world could not be saved; stopping without saving (the last save is kept)"
    );
}
