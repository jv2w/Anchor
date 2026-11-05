//! Save snapshots: when the tick takes one, and what it keeps in memory until the save thread
//! reports the commit.
//!
//! A snapshot holds everything changed since the last one. Until the tick learns that a
//! snapshot is committed, the data in it stays in memory even when its chunk unloads or its
//! player leaves, and a later load takes it from memory. So a load from the file always happens
//! after the commit that wrote the newest data, and never reads an older one.
//!
//! A chunk's terrain and extras (`extra`) are tracked apart: each part goes into a snapshot only
//! when it changed, and an unloaded chunk waits in memory, both parts together, while either has
//! changes no commit holds.

use std::collections::{BTreeMap, BTreeSet};

use tracing::{debug, error, warn};

use super::extra::{self, Held};
use super::log::InputLog;
use super::view::Key;
use super::{Context, Phase, Player, PlayerKey, SessionId, State, dispatch};
use crate::command::Sender;
use crate::event::{PlayerDataSave, WorldSave};
use crate::player;
use crate::save::{PlayerData, Request, Snapshot, WorldIndex, WorldValues};
use crate::storage::{Checked, PlayerKey as StoredPlayer};
use crate::world::{Chunk, Kind};

/// Players who left with data waiting for a commit beyond which the tick asks for one at once.
const MAX_OFFLINE_PLAYERS: usize = 4096;
/// Unloaded chunks waiting for a commit beyond which the tick asks for one at once.
const MAX_UNSAVED_CHUNKS: usize = 4096;

/// A chunk unloaded before a commit held its last changes.
#[derive(Debug)]
pub(super) struct Unsaved {
    /// The terrain; dirty while a change of it is in no snapshot.
    pub(super) chunk: Chunk,
    /// The extras; dirty kinds are in no snapshot yet.
    pub(super) extras: Held,
    /// The snapshot that holds its last changes, or `None` if a part changed since.
    pub(super) snapshot: Option<u64>,
}

impl Unsaved {
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            chunk,
            extras,
            snapshot,
        } = self;
        h.u64(snapshot.map_or(0, |s| s.wrapping_add(1)));
        // As for loaded chunks (`State::hash`): the content of a clean chunk is the file's, the
        // generator's or the snapshot's in flight, all fixed by earlier hashed state.
        h.u64(u64::from(chunk.is_dirty()));
        if chunk.is_dirty() {
            for i in 0..crate::world::SUB_CHUNKS {
                h.bytes(&chunk.sub_chunk_payload(i).unwrap_or_default());
            }
        }
        extras.hash_into(h);
    }
}

impl Unsaved {
    /// Writes the snapshot that holds it and its extras ([`Held::checkpoint`]); the checkpoint
    /// keeps the terrain apart, shared until a crash log is written.
    pub(super) fn checkpoint(&self, w: &mut super::checkpoint::Writer) {
        use super::checkpoint::Codec;
        let Self {
            chunk: _,
            extras,
            snapshot,
        } = self;
        snapshot.encode(w);
        extras.checkpoint(w);
    }

    /// Reads what [`Self::checkpoint`] wrote, with the terrain the checkpoint kept and the lists
    /// the server file or the generator has for the chunk.
    pub(super) fn restore(
        r: &mut super::checkpoint::Reader<'_>,
        chunk: Chunk,
        lists: Option<crate::save::ChunkExtras>,
    ) -> super::checkpoint::Result<Self> {
        use super::checkpoint::Codec;
        Ok(Self {
            snapshot: Codec::decode(r)?,
            extras: Held::restore(r, lists)?,
            chunk,
        })
    }
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
    /// A task or command asked for an online backup after the next save.
    pub(super) backup: bool,
    /// `/check` asked for a check of the data files after the next save.
    check: bool,
    /// Who asked for the check asked for or being made, until its result comes: one at a time.
    checker: Option<Sender>,
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
    /// The persistent worlds, by index, whose state it holds.
    worlds: BTreeSet<usize>,
}

impl Saving {
    /// Snapshots every `interval` ticks from `start`.
    pub(super) fn new(start: u64, interval: u64, players: bool) -> Self {
        Self {
            interval: interval.max(1),
            next_due: start.saturating_add(interval.max(1)),
            requested: false,
            backup: false,
            check: false,
            checker: None,
            in_flight: None,
            offline: BTreeMap::new(),
            players,
        }
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        // The interval and the player switch are settings.
        let Self {
            interval: _,
            next_due,
            requested,
            backup,
            check,
            checker,
            in_flight,
            offline,
            players: _,
        } = self;
        h.u64(*next_due);
        h.u64(u64::from(*requested));
        h.u64(u64::from(*backup));
        h.u64(u64::from(*check));
        h.u64(match checker {
            Some(Sender::Player(id)) => id.session().get().wrapping_add(2),
            Some(Sender::Console) => 1,
            None => 0,
        });
        match in_flight {
            Some(InFlight {
                start,
                chunks,
                players,
                worlds,
            }) => {
                h.u64(start.wrapping_add(1));
                h.usize(chunks.len());
                for &(world, pos) in chunks {
                    h.usize(world);
                    h.u64(u64::from(pos.x.cast_unsigned()));
                    h.u64(u64::from(pos.z.cast_unsigned()));
                }
                h.usize(players.len());
                for key in players {
                    h.bytes(key.as_str().as_bytes());
                }
                h.usize(worlds.len());
                for &world in worlds {
                    h.usize(world);
                }
            }
            None => h.u64(0),
        }
        h.usize(offline.len());
        for (key, Offline { data, snapshot }) in offline {
            h.bytes(key.as_str().as_bytes());
            h.u64(snapshot.map_or(0, |s| s.wrapping_add(1)));
            let PlayerData {
                world,
                position,
                extra,
            } = data;
            h.bytes(world.as_bytes());
            for c in position {
                h.u64(c.to_bits());
            }
            extra::hash_nbt(h, extra);
        }
    }

    /// The start tick of the snapshot in flight if it holds the chunk `key`.
    pub(super) fn holds_chunk(&self, key: &Key) -> Option<u64> {
        self.in_flight
            .as_ref()
            .filter(|f| f.chunks.contains(key))
            .map(|f| f.start)
    }

    /// Whether nothing is in flight or waiting in memory (for leak tests).
    #[cfg(test)]
    pub(super) fn is_idle(&self) -> bool {
        self.in_flight.is_none() && self.offline.is_empty()
    }
}

super::checkpoint::fields!(Saving {
    interval,
    next_due,
    requested,
    backup,
    check,
    checker,
    in_flight,
    offline,
    players,
});
super::checkpoint::fields!(InFlight {
    start,
    chunks,
    players,
    worlds,
});
super::checkpoint::fields!(Offline { data, snapshot });
super::checkpoint::variants!(Sender {
    0 => Player(id),
    1 => Console,
});

/// The storage key of a player: the XUID in decimal, or `offline:` and the name in ASCII lowercase.
pub(super) fn storage_key(key: &PlayerKey) -> StoredPlayer {
    match key {
        PlayerKey::Xuid(xuid) => StoredPlayer::xuid(*xuid),
        PlayerKey::Name(name) => StoredPlayer::offline(name),
    }
}

/// The data kept of a player: its world, position and extra ([`extra::player_saved`]), the
/// user's data values included (`task::save_player`).
#[expect(
    clippy::expect_used,
    reason = "invariant: a player is in a loaded world, which no other world replaces at its place"
)]
pub(super) fn data_of(state: &State, player: &Player) -> PlayerData {
    PlayerData {
        world: state
            .worlds
            .get(player.world)
            .map(|w| w.name().to_owned())
            .expect("a player's world is one of the worlds"),
        position: [player.position.x, player.position.y, player.position.z],
        extra: {
            let mut extra = extra::player_saved(&player.extra);
            super::inventory::save(&player.inventory, &mut extra);
            super::survival::save(&player.survival, &mut extra);
            let join_ms = player.identity.client.join_time_ms;
            super::survival::save_played(player.presence.saved(join_ms, state.tick), &mut extra);
            super::task::save_player(&state.data, &player.extra, &mut extra);
            extra
        },
    }
}

/// Keeps the data of a leaving player in memory until a commit holds it. Leaving dates the
/// play history (`lastPlayed`), so the data is newer than any snapshot taken before.
pub(super) fn left(state: &mut State, player: &Player) {
    if !state.saving.players || player.phase == Phase::Loading {
        return;
    }
    let key = storage_key(&player.key);
    let data = data_of(state, player);
    keep(&mut state.saving, key, data);
}

/// Keeps `data`, the newest of player `key` who is not online, in memory until a commit holds it.
pub(super) fn keep(saving: &mut Saving, key: StoredPlayer, data: PlayerData) {
    saving.offline.insert(
        key,
        Offline {
            data,
            snapshot: None,
        },
    );
}

/// Whether a chunk a view releases stays in memory until a commit holds it: a part has changes
/// no snapshot holds yet, or the snapshot in flight holds them. Not if the file has it all.
pub(super) fn is_kept_unloaded(saving: &Saving, key: &Key, chunk: &Chunk, extras: &Held) -> bool {
    chunk.is_dirty() || extras.is_dirty() || saving.holds_chunk(key).is_some()
}

/// The [`Unsaved::snapshot`] of a chunk [`is_kept_unloaded`]: `None` while a part has changes no
/// snapshot holds yet, else the snapshot in flight that holds them.
pub(super) fn unsaved_snapshot(
    saving: &Saving,
    key: &Key,
    chunk: &Chunk,
    extras: &Held,
) -> Option<u64> {
    if chunk.is_dirty() || extras.is_dirty() {
        None
    } else {
        saving.holds_chunk(key)
    }
}

/// Takes a snapshot at this tick boundary if one is due (periodic, asked for, too many unloaded
/// chunks or departed players waiting, or a quarter of the input log's bound recorded since the
/// last commit) and none is in flight, and starts a new log segment there. The last three are
/// due with periodic saves off too: they bound memory. `last` takes the final snapshot of a
/// shutdown whatever is in flight: the save thread commits in order.
pub(super) fn snapshot(state: &mut State, log: &mut InputLog, last: bool) -> Option<Request> {
    let due = (state.tick >= state.saving.next_due && state.commands.is_auto_save_on())
        || state.saving.requested
        || state.chunks.unsaved.len() > MAX_UNSAVED_CHUNKS
        || state.saving.offline.len() > MAX_OFFLINE_PLAYERS
        || log.wants_commit(state.tick);
    if !last && (!due || state.saving.in_flight.is_some()) {
        return None;
    }
    let data_saves = ask_data_saves(state);
    // What the listeners deferred changes the state too, so it goes into this snapshot.
    dispatch::run_deferred(state, &mut 0);
    // The rule modules' changed chunk state goes to the extras holder first.
    state.rules.flush(&mut state.chunks.extras);
    let start = state.tick;
    log.snapshot(start);
    let mut snapshot = Snapshot {
        start,
        identity: log.identity(),
        deleted_worlds: super::world_api::take_deleted_worlds(state),
        new_worlds: super::world_api::take_new_worlds(state),
        backup: std::mem::take(&mut state.saving.backup),
        check: std::mem::take(&mut state.saving.check),
        ..Snapshot::default()
    };
    let mut held = InFlight {
        start,
        chunks: BTreeSet::new(),
        players: BTreeSet::new(),
        worlds: BTreeSet::new(),
    };
    for world in state.worlds.iter_mut() {
        if world.kind() != Kind::Persistent {
            continue;
        }
        let index = world.id().index();
        // Unloaded worlds too: a world unloaded since the last snapshot is saved by this one.
        snapshot.worlds.push((index, WorldValues::of(world)));
        held.worlds.insert(index);
        for (pos, chunk) in world.take_dirty() {
            held.chunks.insert((index, pos));
            snapshot.chunks.push((index, pos, chunk));
        }
        for (pos, kind, list) in state.chunks.extras.take_dirty(index) {
            held.chunks.insert((index, pos));
            snapshot.extras.push((index, pos, kind, list));
        }
    }
    for (&(index, pos), unsaved) in &mut state.chunks.unsaved {
        if unsaved.snapshot.is_none() {
            unsaved.snapshot = Some(start);
            held.chunks.insert((index, pos));
            if unsaved.chunk.is_dirty() {
                snapshot
                    .chunks
                    .push((index, pos, unsaved.chunk.take_save()));
            }
            for (kind, list) in unsaved.extras.take_dirty() {
                snapshot.extras.push((index, pos, kind, list));
            }
        }
    }
    if state.saving.players {
        super::task::flush_offline(state);
        take_players(state, start, &mut snapshot, &mut held);
    }
    // Operators, bans and the whitelist live in memory for good, so nothing waits for their
    // commit: the snapshot carries what changed, and later changes go into later snapshots.
    snapshot.access = state.commands.take_access_changes();
    // Server data lives in memory for good as well.
    snapshot.user = state.data.take_changes();
    debug!(
        start,
        chunks = snapshot.chunks.len(),
        extras = snapshot.extras.len(),
        players = snapshot.players.len(),
        access = snapshot.access.len(),
        user = snapshot.user.len(),
        "save snapshot taken"
    );
    let saving = &mut state.saving;
    saving.requested = false;
    saving.next_due = start.saturating_add(saving.interval);
    saving.in_flight = Some(held);
    for e in &data_saves {
        dispatch::watch(state, e);
    }
    Some(Request::Commit(Box::new(snapshot)))
}

/// Moves the changed player data into the snapshot taken at `start`: online players whose data
/// changed, and departed players no snapshot holds yet; `held` keeps their keys until the commit.
#[expect(
    clippy::expect_used,
    reason = "invariant: the sessions are taken from the players just before, and none leaves"
)]
fn take_players(state: &mut State, start: u64, snapshot: &mut Snapshot, held: &mut InFlight) {
    let sessions: Vec<_> = dirty_players(state).map(|(session, _)| session).collect();
    for session in sessions {
        let player = state
            .players
            .get(&session)
            .expect("the session is a player");
        let key = storage_key(&player.key);
        let data = data_of(state, player);
        let player = state
            .players
            .get_mut(&session)
            .expect("the session is a player");
        player.data_dirty = false;
        super::task::player_saved(&mut player.extra, &data.extra);
        snapshot.players.push((key.clone(), data));
        held.players.insert(key);
    }
    for (key, offline) in &mut state.saving.offline {
        if offline.snapshot.is_none() {
            offline.snapshot = Some(start);
            snapshot.players.push((key.clone(), offline.data.clone()));
            held.players.insert(key.clone());
        }
    }
}

/// Raises [`PlayerDataSave`] for each online player whose data the snapshot about to be taken
/// holds, before anything goes into it, so that what the listeners change is in it too. A player
/// the listeners or what they deferred ([`dispatch::run_deferred`]) made changed gets the event
/// as well, so no player's data is saved without it. The events come back for the monitors, who
/// see them once the snapshot is taken.
///
/// Cancelling saves the data all the same: a snapshot holds the whole state of one tick boundary,
/// and a player's data left out of it would come back after a crash beside a newer world (an item
/// both in a chest and in the inventory). `save-player-data` turns player data off for good.
fn ask_data_saves(state: &mut State) -> Vec<PlayerDataSave> {
    if !state.saving.players || !dispatch::heard::<PlayerDataSave>(state) {
        return Vec::new();
    }
    let mut asked = BTreeSet::new();
    let mut events = Vec::new();
    let mut runs = 0;
    loop {
        let dirty: Vec<_> = dirty_players(state)
            .filter(|(session, _)| !asked.contains(session))
            .map(|(session, p)| (session, p.name().to_owned()))
            .collect();
        if dirty.is_empty() {
            return events;
        }
        for (session, name) in dirty {
            asked.insert(session);
            let mut e = PlayerDataSave {
                player: player::Id::new(session),
                record_key: name,
                cancelled: false,
            };
            dispatch::call(state, &mut e);
            if e.cancelled {
                debug!(
                    player = e.record_key.as_str(),
                    "a cancelled player data save is saved all the same"
                );
            }
            events.push(e);
        }
        // What the listeners deferred changes the state too, so it goes into this snapshot.
        dispatch::run_deferred(state, &mut runs);
    }
}

/// The online players whose data changed since the last snapshot.
fn dirty_players(state: &State) -> impl Iterator<Item = (SessionId, &Player)> {
    state
        .players
        .iter()
        .filter(|(_, p)| p.data_dirty)
        .map(|(&session, p)| (session, p))
}

/// The snapshot taken before tick `start` is committed: the memory that held it for the commit
/// is let go, and the input log learns of the commit (`InputLog::committed`).
pub(super) fn committed(state: &mut State, log: &mut InputLog, start: u64) {
    let worlds = state
        .saving
        .in_flight
        .take_if(|f| f.start == start)
        .map(|f| f.worlds)
        .unwrap_or_default();
    let chunks = &mut state.chunks;
    let done = chunks
        .unsaved
        .extract_if(.., |_, u| u.snapshot == Some(start));
    chunks.discarded.extend(done.map(|(_, u)| u.chunk));
    state
        .saving
        .offline
        .retain(|_, o| o.snapshot != Some(start));
    log.committed(start);
    debug!(start, "save committed");
    if dispatch::heard::<WorldSave>(state) {
        // Only the worlds the snapshot held, and of them only those still loaded.
        for index in worlds {
            if !state.catalog.is_loaded(index) {
                continue;
            }
            let mut e = WorldSave {
                world: crate::world::Id::of(index),
            };
            dispatch::call(state, &mut e);
            dispatch::watch(state, &e);
        }
    }
}

/// Asks for a check of the data files after the next save (`/check`), for `sender`; false, asking
/// for nothing, while another is asked for or being made.
pub(super) fn ask_check(state: &mut State, sender: Sender) -> bool {
    if state.saving.checker.is_some() {
        return false;
    }
    state.saving.checker = Some(sender);
    state.saving.check = true;
    state.saving.requested = true;
    true
}

/// Tells who asked for the check what it `found`; a player who has left is told nothing.
pub(super) fn checked(state: &mut State, found: &Checked) {
    let Some(sender) = state.saving.checker.take() else {
        return;
    };
    let text = match found {
        Checked::Passed { values } => {
            format!("Data file check passed: all {values} stored values are whole")
        }
        Checked::Damaged {
            values,
            damaged,
            first,
        } => format!(
            "Data file check found {damaged} damaged of {values} stored values (first: \
             {first}); the server log has the details"
        ),
        Checked::Failed(why) => format!("Data file check could not finish: {why}"),
    };
    if let Err(e) = (Context { state }).send_to(sender, player::Message::Raw(text)) {
        debug!(
            error = &e as &(dyn std::error::Error + 'static),
            "the check result was not sent: the player is leaving"
        );
    }
}

/// The snapshot taken before tick `start` could not be committed: the tick stops without saving.
pub(super) fn failed(start: u64) {
    error!(
        start,
        "the world could not be saved; stopping without saving (the last save is kept)"
    );
}

/// A batch of chunks could not be read: the tick stops without saving, as for a failed commit.
pub(super) fn load_failed() {
    error!("a world file could not be read; stopping without saving (the last save is kept)");
}

/// The file of the world at `index` did not open on the save thread: the world unloads, and
/// loading it again tries its file again, so the failure shows to the program that loaded it.
pub(super) fn world_open_failed(state: &mut State, index: WorldIndex) {
    // A world gone since the answer was sent (deleted, made temporary, or its place taken by
    // another world) has no file to try again.
    let Some(world) = state
        .worlds
        .get(index)
        .filter(|w| w.kind() == Kind::Persistent)
    else {
        return;
    };
    warn!(
        world = world.name(),
        "world file did not open; unloading the world"
    );
    super::world_api::open_failed(state, index);
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;

    use crate::event::{Event, Listeners, Priority};
    use crate::tick::tests::{initialised, join, new_state, radius, settings, tick};
    use crate::tick::{Context, Output, SessionId};

    thread_local! {
        /// What the listeners of a test saw, in order.
        static LOG: Cell<Vec<String>> = const { Cell::new(Vec::new()) };
    }

    fn log(line: String) {
        let mut lines = LOG.take();
        lines.push(line);
        LOG.set(lines);
    }

    fn logged() -> Vec<String> {
        LOG.take()
    }

    fn on<E: Event>(listener: impl FnMut(&mut E, &mut Context<'_>) + Send + 'static) -> Listeners {
        let mut l = Listeners::default();
        l.push(Priority::Normal, listener);
        l
    }

    /// A server that saves player data with session 1 spawned, its data changed since the last
    /// snapshot.
    fn state(listeners: Listeners) -> State {
        LOG.set(Vec::new());
        let mut state = new_state(settings(|c| c.general.save_player_data = true), listeners);
        let mut output = Output::default();
        let (j, _rx) = join(1, 0);
        tick(&mut state, &mut vec![j], &mut output);
        tick(&mut state, &mut vec![radius(1, 1, 2)], &mut output);
        let runtime_id = state.players[&SessionId::new(1)].runtime_id;
        tick(
            &mut state,
            &mut vec![initialised(1, 2, runtime_id)],
            &mut output,
        );
        for _ in 0..3 {
            tick(&mut state, &mut Vec::new(), &mut output);
        }
        if let Some(p) = state.players.get_mut(&SessionId::new(1)) {
            p.data_dirty = true;
        }
        state
    }

    /// The player data of the snapshot taken now.
    fn saved(state: &mut State) -> Vec<(String, PlayerData)> {
        state.saving.requested = true;
        let Some(Request::Commit(snapshot)) = snapshot(state, &mut InputLog::default(), false)
        else {
            panic!("a snapshot was asked for");
        };
        snapshot
            .players
            .into_iter()
            .map(|(k, d)| (k.as_str().to_owned(), d))
            .collect()
    }

    // api: PlayerDataSaveEvent
    #[test]
    fn player_data_save_runs_before_the_snapshot_so_listener_changes_are_in_it() {
        let mut listeners = on(|e: &mut PlayerDataSave, cx: &mut Context<'_>| {
            log(format!("{} {}", e.player().session().get(), e.record_key()));
            cx.player_mut(e.player()).unwrap().set_xp_level(7).unwrap();
        });
        listeners.push_monitor(|e: &PlayerDataSave, cx: &Context<'_>| {
            let dirty = cx.state.players[&e.player().session()].data_dirty;
            log(format!("monitor {dirty}"));
        });
        let mut quiet = state(Listeners::default());
        let mut heard = state(listeners);
        let before = saved(&mut quiet);
        let after = saved(&mut heard);
        let player = &heard.players[&SessionId::new(1)];
        let line = format!("1 {}", player.name());
        assert_eq!(logged(), [line, "monitor false".to_owned()]);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].0, "2535000001");
        assert_eq!(player.xp_level(), 7);
        // The snapshot holds the player as the listener left it, unlike without the listener.
        assert_eq!(after[0].1, data_of(&heard, player));
        assert_ne!(after[0].1, before[0].1);
        // Nothing is saved twice, and nothing is asked without a snapshot.
        assert!(snapshot(&mut heard, &mut InputLog::default(), false).is_none());
        assert!(logged().is_empty());
    }

    // api: PlayerDataSaveEvent
    #[test]
    fn a_cancelled_player_data_save_is_saved_all_the_same() {
        let cancel = on(|e: &mut PlayerDataSave, _: &mut Context<'_>| e.cancel());
        let mut quiet = state(Listeners::default());
        let mut cancelled = state(cancel);
        let before = saved(&mut quiet);
        let after = saved(&mut cancelled);
        // One commit holds the whole state of a tick boundary: the world and its players.
        assert_eq!(after, before);
        assert!(!cancelled.players[&SessionId::new(1)].data_dirty);
    }

    // api: PlayerDataSaveEvent
    #[test]
    fn a_player_a_data_save_listener_changed_gets_the_event_too() {
        let listeners = on(|e: &mut PlayerDataSave, cx: &mut Context<'_>| {
            log(e.player().session().get().to_string());
            if e.player().session() == SessionId::new(1) {
                cx.player_mut(player::Id::new(SessionId::new(2)))
                    .unwrap()
                    .set_xp_level(5)
                    .unwrap();
            }
        });
        LOG.set(Vec::new());
        let mut state = new_state(settings(|c| c.general.save_player_data = true), listeners);
        let mut output = Output::default();
        let (join_1, _rx_1) = join(1, 0);
        let (join_2, _rx_2) = join(2, 0);
        tick(&mut state, &mut vec![join_1, join_2], &mut output);
        tick(
            &mut state,
            &mut vec![radius(1, 1, 2), radius(2, 1, 2)],
            &mut output,
        );
        let runtime_ids: Vec<_> = state.players.values().map(|p| p.runtime_id).collect();
        tick(
            &mut state,
            &mut vec![
                initialised(1, 2, runtime_ids[0]),
                initialised(2, 2, runtime_ids[1]),
            ],
            &mut output,
        );
        for _ in 0..3 {
            tick(&mut state, &mut Vec::new(), &mut output);
        }
        for (session, p) in &mut state.players {
            p.data_dirty = *session == SessionId::new(1);
        }
        let saved = saved(&mut state);
        assert_eq!(logged(), ["1", "2"]);
        assert_eq!(saved.len(), 2);
    }

    /// What a listener defers while the player data saves goes into the same snapshot.
    #[test]
    fn a_callback_deferred_by_a_data_save_listener_is_in_the_snapshot() {
        let listeners = on(|_: &mut PlayerDataSave, cx: &mut Context<'_>| {
            cx.defer(|cx| {
                cx.player_mut(player::Id::new(SessionId::new(1)))
                    .unwrap()
                    .set_xp_level(9)
                    .unwrap();
            })
            .unwrap();
        });
        let mut state = state(listeners);
        let saved = saved(&mut state);
        let player = &state.players[&SessionId::new(1)];
        assert_eq!(player.xp_level(), 9);
        assert_eq!(saved[0].1, data_of(&state, player));
    }

    /// Runs 200 ticks of 1 MiB of input each with saves every 1440 minutes, auto-save on or off,
    /// the stand-in save thread committing every snapshot: the commits it took and the log.
    fn heavy_inputs(auto_save: bool) -> (usize, InputLog) {
        let mut state = new_state(
            settings(|c| c.developer.save_interval_minutes = 1440),
            Listeners::default(),
        );
        state.commands.finish().unwrap();
        if !auto_save {
            crate::tick::command::console(&mut state, "save off");
            assert!(!state.commands.is_auto_save_on());
        }
        let pool = crate::runtime::cpu_pool(1).unwrap();
        let mut log = InputLog::default();
        let mut answers = Vec::new();
        let mut commits = 0;
        for seq in 0..200 {
            let packet = crate::protocol::packet::Packet::ClientCacheStatus(
                crate::protocol::packet::ClientCacheStatus { enabled: true },
            );
            let mut inputs = vec![crate::tick::queue::Input {
                session: SessionId::new(1),
                seq,
                kind: crate::tick::queue::InputKind::Packet {
                    packet,
                    size: 1 << 20,
                },
            }];
            let mut output = Output::default();
            crate::tick::step(
                &mut state,
                &mut inputs,
                &mut answers,
                &pool,
                &mut output,
                &mut log,
            );
            crate::tick::deliver(&mut state, &mut output, &mut log);
            crate::tick::boundary(&mut state, &mut output, &mut log);
            let requests = std::mem::take(&mut output.requests);
            commits += requests
                .iter()
                .filter(|r| matches!(r, Request::Commit(_)))
                .count();
            answers = crate::tick::tests::answer(&state, requests);
        }
        assert!(pool.shutdown());
        (commits, log)
    }

    #[test]
    fn heavy_input_asks_for_commits_before_the_log_fills_with_auto_save_on_or_off() {
        for auto_save in [true, false] {
            let (commits, log) = heavy_inputs(auto_save);
            assert!(
                !log.is_truncated(),
                "auto-save {auto_save}: the log was truncated"
            );
            // 200 MiB of input: a commit at least every 16 MiB.
            assert!(
                commits >= 200 / 16,
                "auto-save {auto_save}: {commits} commits"
            );
        }
    }

    #[test]
    fn a_backup_rides_the_next_snapshot_once_even_with_one_in_flight() {
        let mut state = state(Listeners::default());
        let mut log = InputLog::default();
        let backup = |request: Option<Request>| match request {
            Some(Request::Commit(snapshot)) => Some(snapshot.backup),
            _ => None,
        };
        state.saving.requested = true;
        assert_eq!(backup(snapshot(&mut state, &mut log, false)), Some(false));
        // Asked twice while that snapshot is in flight: it waits for the next one.
        Context { state: &mut state }.backup();
        Context { state: &mut state }.backup();
        assert_eq!(backup(snapshot(&mut state, &mut log, false)), None);
        let start = state.saving.in_flight.as_ref().unwrap().start;
        committed(&mut state, &mut log, start);
        assert_eq!(backup(snapshot(&mut state, &mut log, false)), Some(true));
        let start = state.saving.in_flight.as_ref().unwrap().start;
        committed(&mut state, &mut log, start);
        state.saving.requested = true;
        assert_eq!(backup(snapshot(&mut state, &mut log, false)), Some(false));
    }

    /// `backups = false`: [`Context::backup`] refuses and asks nothing of the next snapshot.
    #[test]
    fn without_backups_the_api_refuses_a_backup() {
        let mut state = new_state(
            settings(|c| c.general.backups = false),
            Listeners::default(),
        );
        assert!(!Context { state: &mut state }.backup());
        assert!(!state.saving.backup && !state.saving.requested);
        let mut state = new_state(settings(|_| {}), Listeners::default());
        assert!(Context { state: &mut state }.backup());
        assert!(state.saving.backup && state.saving.requested);
    }

    /// A world made after the snapshot was taken is not in it: its commit raises no save for it.
    #[test]
    fn world_save_names_only_the_worlds_the_snapshot_held() {
        let listeners = on(|e: &mut WorldSave, _: &mut Context<'_>| {
            log(e.world().index().to_string());
        });
        let mut state = state(listeners);
        let mut input_log = InputLog::default();
        state.saving.requested = true;
        assert!(snapshot(&mut state, &mut input_log, false).is_some());
        Context { state: &mut state }
            .create_world("arena", &crate::world::Options::new())
            .unwrap();
        let start = state.saving.in_flight.as_ref().unwrap().start;
        committed(&mut state, &mut input_log, start);
        assert_eq!(logged(), ["0"]);
    }

    /// A world whose file did not open unloads, and loading it again tells the save thread of it
    /// again.
    #[test]
    fn a_world_whose_file_did_not_open_unloads_and_loads_again() {
        let mut state = state(Listeners::default());
        let mut output = Output::default();
        let options = crate::world::Options::new();
        let world = Context { state: &mut state }
            .create_world("arena", &options)
            .unwrap();
        assert_eq!(crate::tick::world_api::take_new_worlds(&mut state).len(), 1);
        world_open_failed(&mut state, world.index());
        tick(&mut state, &mut Vec::new(), &mut output);
        assert!(!Context { state: &mut state }.is_world_loaded("arena"));
        Context { state: &mut state }
            .load_world("arena", &options)
            .unwrap();
        assert_eq!(crate::tick::world_api::take_new_worlds(&mut state).len(), 1);
    }

    /// The failed open of a world's file, heard after the world was deleted and another world
    /// took its place, changes nothing: the world in the place stays loaded.
    #[test]
    fn a_late_open_failure_of_a_world_gone_leaves_the_world_in_its_place() {
        let mut state = state(Listeners::default());
        let mut output = Output::default();
        let options = crate::world::Options::new();
        let old = Context { state: &mut state }
            .create_world("arena", &options)
            .unwrap();
        tick(&mut state, &mut Vec::new(), &mut output);
        Context { state: &mut state }.unload_world(old).unwrap();
        tick(&mut state, &mut Vec::new(), &mut output);
        let mut cx = Context { state: &mut state };
        cx.delete_world("arena").unwrap();
        let new = cx.create_world("lobby", &options).unwrap();
        assert_eq!(
            crate::save::place(new.index()),
            crate::save::place(old.index())
        );
        world_open_failed(&mut state, old.index());
        tick(&mut state, &mut Vec::new(), &mut output);
        assert!(Context { state: &mut state }.is_world_loaded("lobby"));
    }

    // api: WorldSaveEvent, WorldEvent::getWorld
    #[test]
    fn world_save_follows_each_commit() {
        let mut listeners = on(|e: &mut WorldSave, cx: &mut Context<'_>| {
            log(format!("{} {}", e.world().index(), cx.tick()));
        });
        listeners.push_monitor(|_: &WorldSave, cx: &Context<'_>| {
            log(format!("monitor {}", cx.state.saving.in_flight.is_none()));
        });
        let mut state = state(listeners);
        let mut output = Output::default();
        state.saving.requested = true;
        tick(&mut state, &mut Vec::new(), &mut output);
        assert!(logged().is_empty(), "the commit is not known yet");
        let tick_now = state.tick;
        tick(&mut state, &mut Vec::new(), &mut output);
        assert_eq!(
            logged(),
            [format!("0 {tick_now}"), "monitor true".to_owned()]
        );
        tick(&mut state, &mut Vec::new(), &mut output);
        assert!(logged().is_empty());
    }
}
