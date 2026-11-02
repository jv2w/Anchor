//! The saved data of players who are not online ([`Context::hold_offline_player`]): a load goes to
//! the save thread as a join's does ([`Request::Player`]), its answer comes back as an input of a
//! later tick, which the input log records, and the record is then held in memory, read and
//! changed through [`Offline`], until its last ticket is let go or the player joins.
//!
//! The newest data of a player who is not online is its held record, else its data waiting for a
//! commit since it left (`save::Saving::offline`), else the data file's. The save thread answers
//! in request order and data waiting for a commit stays in memory until the tick learns of the
//! commit, so a load that finds neither in memory when its answer arrives read the newest data.
//! A held record's changes go to wait for a commit as a departed player's data does, when a
//! snapshot is taken, the record is let go or the player joins: the next commit writes them with
//! the rest of the game, and the join takes them as it takes a departed player's data. No record
//! is held for a player online or joining.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::{Context, UserData};
use crate::login::check_user_name;
use crate::player::{Error, Result, Ticket};
use crate::save::{PlayerData, PlayerLoad, Request};
use crate::storage::PlayerKey as StoredPlayer;
use crate::tick::checkpoint::{self, Codec, Reader, Writer};
use crate::tick::extra;
use crate::tick::inventory::{self, Inventory};
use crate::tick::{PlayerKey, State, save};

/// The most tickets held or asked for at once.
const MAX_TICKETS: usize = 64;

/// A load's callback, taking its ticket or why it holds nothing.
type Done = Box<dyn FnOnce(&mut Context<'_>, Result<Ticket>) + Send>;

/// The saved data of a player who is not online, as its tickets hold it
/// ([`Context::offline_player`]): its inventories and its data values, read and changed as an
/// online [`Player`](crate::player::Player)'s are, without a client to tell. The rest of the
/// record stays as saved.
pub struct Offline {
    pub(in crate::tick) world: String,
    pub(in crate::tick) position: [f64; 3],
    pub(in crate::tick) extra: extra::Player,
    pub(in crate::tick) inventory: Inventory,
    /// The inventories changed since the load: a save writes their keys.
    pub(in crate::tick) inventory_changed: bool,
    /// The data values changed since the load: a save writes them.
    pub(in crate::tick) data_changed: bool,
    /// Changed since the record last went to wait for a commit.
    pub(in crate::tick) dirty: bool,
}

impl Offline {
    /// Marks an inventory changed by a write.
    pub(in crate::tick) fn change_inventory(&mut self) {
        self.inventory_changed = true;
        self.dirty = true;
    }

    /// Reads the record of saved data `data` as a join reads it.
    fn load(state: &State, data: PlayerData) -> Self {
        let PlayerData {
            world,
            position,
            extra,
        } = data;
        let mut extra = extra::player_loaded(extra);
        let mut inventory = Inventory::default();
        inventory::load(&mut inventory, extra.raw(), &state.custom);
        super::placed(&state.data, &mut extra);
        Self {
            world,
            position,
            extra,
            inventory,
            inventory_changed: false,
            data_changed: false,
            dirty: false,
        }
    }

    /// The data a save writes: the record as saved, with the parts changed since the load
    /// written over it, so a change of one part leaves the others' saved bytes alone.
    fn saved(&self, data: &UserData) -> PlayerData {
        let mut extra = extra::player_saved(&self.extra);
        if self.inventory_changed {
            inventory::save(&self.inventory, &mut extra);
        }
        if self.data_changed {
            super::save_player(data, &self.extra, &mut extra);
        }
        PlayerData {
            world: self.world.clone(),
            position: self.position,
            extra,
        }
    }
}

impl fmt::Debug for Offline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Offline")
            .field("world", &self.world)
            .field("position", &self.position)
            .field("inventory_changed", &self.inventory_changed)
            .field("data_changed", &self.data_changed)
            .field("dirty", &self.dirty)
            .finish_non_exhaustive()
    }
}

/// The offline players' records held and the loads waiting for the save thread.
#[derive(Default)]
pub(crate) struct Holds {
    /// Every ticket given and not let go, held or waiting: its record's storage key. At most
    /// [`MAX_TICKETS`].
    tickets: BTreeMap<u64, StoredPlayer>,
    /// The number of the next ticket; numbers are never reused.
    next_ticket: u64,
    held: BTreeMap<StoredPlayer, Held>,
    /// One load request is in flight for each.
    waiting: BTreeMap<StoredPlayer, Waiting>,
    /// The keys of `waiting` whose request the tick boundary has not sent yet, in order.
    unsent: Vec<StoredPlayer>,
}

impl Holds {
    /// Writes the next ticket number. A waiting load's callback is a closure, and a held record
    /// has no checkpoint form: either makes the checkpoint opaque. Without them no ticket is
    /// given, and the tick boundary before a checkpoint sent every request.
    pub(super) fn checkpoint(&self, w: &mut Writer) {
        let Self {
            tickets: _,
            next_ticket,
            held,
            waiting,
            unsent: _,
        } = self;
        w.opaque("offline player loads waiting", waiting.len());
        w.opaque("offline players held", held.len());
        Codec::encode(next_ticket, w);
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, data: &UserData, h: &mut crate::tick::Fnv) {
        let Self {
            tickets,
            next_ticket,
            held,
            waiting,
            unsent,
        } = self;
        h.u64(*next_ticket);
        h.usize(tickets.len());
        for (n, key) in tickets {
            h.u64(*n);
            h.bytes(key.as_str().as_bytes());
        }
        h.usize(held.len());
        for (key, Held { record, tickets }) in held {
            h.bytes(key.as_str().as_bytes());
            h.usize(tickets.len());
            for n in tickets {
                h.u64(*n);
            }
            // The world, position, extra and inventories are hashed in their saved form, which
            // holds every change.
            let Offline {
                world: _,
                position: _,
                extra: _,
                inventory: _,
                inventory_changed,
                data_changed,
                dirty,
            } = record;
            for flag in [inventory_changed, data_changed, dirty] {
                h.u64(u64::from(*flag));
            }
            let saved = record.saved(data);
            h.bytes(saved.world.as_bytes());
            for c in saved.position {
                h.u64(c.to_bits());
            }
            extra::hash_nbt(h, &saved.extra);
        }
        // Callbacks are code; which loads wait is state. The player key is the storage key's.
        h.usize(waiting.len());
        for (key, Waiting { key: _, loads }) in waiting {
            h.bytes(key.as_str().as_bytes());
            h.usize(loads.len());
            for (n, _) in loads {
                h.u64(*n);
            }
        }
        h.usize(unsent.len());
        for key in unsent {
            h.bytes(key.as_str().as_bytes());
        }
    }

    /// Reads what [`Self::checkpoint`] wrote: nothing held or waiting, and the next ticket number.
    pub(super) fn restore(&mut self, r: &mut Reader<'_>) -> checkpoint::Result<()> {
        *self = Self {
            next_ticket: Codec::decode(r)?,
            ..Self::default()
        };
        Ok(())
    }

    /// Whether nothing is held or waiting (for leak tests).
    #[cfg(test)]
    fn is_idle(&self) -> bool {
        self.tickets.is_empty()
            && self.held.is_empty()
            && self.waiting.is_empty()
            && self.unsent.is_empty()
    }
}

impl fmt::Debug for Holds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Holds")
            .field("tickets", &self.tickets.len())
            .field("next_ticket", &self.next_ticket)
            .field("held", &self.held.len())
            .field("waiting", &self.waiting.len())
            .finish_non_exhaustive()
    }
}

/// A held record and the tickets that hold it.
struct Held {
    record: Offline,
    tickets: BTreeSet<u64>,
}

/// The loads of one player waiting for the save thread's answer, in the order they were asked.
pub(in crate::tick) struct Waiting {
    key: PlayerKey,
    loads: Vec<(u64, Done)>,
}

impl Context<'_> {
    /// The record `ticket` holds; `None` while its load waits, and once it was let go or its
    /// player joined.
    pub fn offline_player(&self, ticket: Ticket) -> Option<&Offline> {
        let holds = &self.state.tasks.offline;
        let key = holds.tickets.get(&ticket.get())?;
        holds
            .held
            .get(key)
            .filter(|held| held.tickets.contains(&ticket.get()))
            .map(|held| &held.record)
    }

    /// Loads the saved data of the player with Xbox user ID `xuid`, or, without Xbox sign-in
    /// (`None`), named `name` in any ASCII case: the key Anchor saves a player under
    /// ([`Player::xuid`](crate::player::Player::xuid),
    /// [`Player::name`](crate::player::Player::name)); `name` is not looked at with a `xuid`. The
    /// save thread reads it, and in a later tick `done` gets a ticket that holds the record in
    /// memory, to read and change through [`offline_player`](Self::offline_player) and
    /// [`offline_player_mut`](Self::offline_player_mut), until
    /// [`release_offline_player`](Self::release_offline_player) lets it go or the player joins.
    /// Every ticket of one player holds the same record. Changes are saved by the next snapshot,
    /// in the same commit as the rest of the game, and a player who joins first joins with them.
    /// The answer is recorded in the input log, so a replay gives `done` the same record without
    /// reading the file.
    ///
    /// `done` gets [`Error::Online`] if the player joined meanwhile, [`Error::NotSaved`] if no data
    /// is saved for it, and [`Error::Damaged`] if its data is damaged (it stays as saved). Fails,
    /// asking nothing, with [`Error::Disabled`] when `save-player-data` is false, [`Error::Online`]
    /// for a player online or joining, [`Error::NotSaved`] for a name no player can have, and
    /// [`Error::Full`] past 64 tickets held or asked for.
    #[doc(alias = "Server::getOfflinePlayerData")]
    pub fn hold_offline_player<D>(&mut self, xuid: Option<u64>, name: &str, done: D) -> Result<()>
    where
        D: FnOnce(&mut Context<'_>, Result<Ticket>) + Send + 'static,
    {
        let state = &mut *self.state;
        if !state.saving.players {
            return Err(Error::Disabled);
        }
        if xuid.is_none() && check_user_name(name).is_err() {
            return Err(Error::NotSaved);
        }
        let key = PlayerKey::new(xuid, name);
        if state.index.contains_key(&key) {
            return Err(Error::Online);
        }
        let holds = &mut state.tasks.offline;
        if holds.tickets.len() >= MAX_TICKETS {
            return Err(Error::Full);
        }
        let n = holds.next_ticket;
        // 2^64 tickets are never given; refuse rather than reuse a number.
        holds.next_ticket = n.checked_add(1).ok_or(Error::Full)?;
        let stored = save::storage_key(&key);
        holds.tickets.insert(n, stored.clone());
        let load: (u64, Done) = (n, Box::new(done));
        match holds.waiting.entry(stored) {
            Entry::Occupied(mut waiting) => waiting.get_mut().loads.push(load),
            Entry::Vacant(slot) => {
                holds.unsent.push(slot.key().clone());
                slot.insert(Waiting {
                    key,
                    loads: vec![load],
                });
            }
        }
        Ok(())
    }

    /// Lends the record `ticket` holds to change, as [`offline_player`](Self::offline_player)
    /// finds it; the changes are saved by the next snapshot.
    #[doc(alias = "Server::saveOfflinePlayerData")]
    pub fn offline_player_mut(&mut self, ticket: Ticket) -> Option<&mut Offline> {
        let holds = &mut self.state.tasks.offline;
        let key = holds.tickets.get(&ticket.get())?;
        holds
            .held
            .get_mut(key)
            .filter(|held| held.tickets.contains(&ticket.get()))
            .map(|held| &mut held.record)
    }

    /// Lets `ticket` go: once no ticket holds its record, the record leaves memory, its changes
    /// waiting for the next snapshot as a departed player's data does. False, changing nothing,
    /// while its load waits, and once it was let go or its player joined.
    pub fn release_offline_player(&mut self, ticket: Ticket) -> bool {
        let n = ticket.get();
        let state = &mut *self.state;
        let holds = &mut state.tasks.offline;
        let Some(key) = holds.tickets.get(&n).cloned() else {
            return false;
        };
        let Entry::Occupied(mut held) = holds.held.entry(key) else {
            return false;
        };
        if !held.get_mut().tickets.remove(&n) {
            return false;
        }
        holds.tickets.remove(&n);
        if held.get().tickets.is_empty() {
            let (key, held) = held.remove_entry();
            let_go(state, key, &held.record);
        }
        true
    }
}

/// Sends every held record's changes to wait for a commit, before a snapshot takes the data that
/// waits.
pub(in crate::tick) fn flush(state: &mut State) {
    let State {
        tasks,
        saving,
        data,
        ..
    } = state;
    for (key, held) in &mut tasks.offline.held {
        if held.record.dirty {
            held.record.dirty = false;
            let saved = held.record.saved(data);
            // As for an online player: a value a later save cannot write keeps this form.
            super::player_saved(&mut held.record.extra, &saved.extra);
            save::keep(saving, key.clone(), saved);
        }
    }
}

/// Lets the held record of `key` go as its player joins: its changes wait where the join takes a
/// departed player's data, and its tickets hold nothing from now on.
pub(in crate::tick) fn joined(state: &mut State, key: &StoredPlayer) {
    let Some(held) = state.tasks.offline.held.remove(key) else {
        return;
    };
    for n in &held.tickets {
        state.tasks.offline.tickets.remove(n);
    }
    let_go(state, key.clone(), &held.record);
}

/// Takes the load requests asked for since the last tick boundary, in order.
pub(in crate::tick) fn requests(state: &mut State) -> impl Iterator<Item = Request> {
    std::mem::take(&mut state.tasks.offline.unsent)
        .into_iter()
        .map(Request::Player)
}

/// Takes out the loads waiting for the save thread's answer for `key`, to answer them.
pub(in crate::tick) fn take_loads(state: &mut State, key: &StoredPlayer) -> Option<Waiting> {
    state.tasks.offline.waiting.remove(key)
}

/// Answers the loads `waiting` for `key` with the save thread's `load`: each callback runs in
/// order, with its ticket holding the record or why it holds nothing.
pub(in crate::tick) fn answer_loads(
    state: &mut State,
    key: &StoredPlayer,
    waiting: Waiting,
    load: &PlayerLoad,
) {
    let Waiting { key: player, loads } = waiting;
    for (n, done) in loads {
        let held = hold(state, key, &player, load, n);
        if held.is_err() {
            state.tasks.offline.tickets.remove(&n);
        }
        done(&mut Context { state }, held.map(|()| Ticket::new(n)));
    }
}

/// Lets a record leave memory: its changes wait for a commit as a departed player's data does.
fn let_go(state: &mut State, key: StoredPlayer, record: &Offline) {
    if record.dirty {
        let data = record.saved(&state.data);
        save::keep(&mut state.saving, key, data);
    }
}

/// Holds the record of `key` for ticket `n`: the one held already, else one read from the newest
/// data in memory, else from the save thread's answer `load`.
fn hold(
    state: &mut State,
    key: &StoredPlayer,
    player: &PlayerKey,
    load: &PlayerLoad,
    n: u64,
) -> Result<()> {
    if state.index.contains_key(player) {
        return Err(Error::Online);
    }
    if let Some(held) = state.tasks.offline.held.get_mut(key) {
        held.tickets.insert(n);
        return Ok(());
    }
    let data = match (state.saving.offline.get(key), load) {
        (Some(departed), _) => departed.data.clone(),
        (None, PlayerLoad::Found(data)) => data.clone(),
        (None, PlayerLoad::Absent) => return Err(Error::NotSaved),
        (None, PlayerLoad::Damaged) => return Err(Error::Damaged),
    };
    let record = Offline::load(state, data);
    state.tasks.offline.held.insert(
        key.clone(),
        Held {
            record,
            tickets: BTreeSet::from([n]),
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use serde::{Deserialize, Serialize};

    use crate::event::Listeners;
    use crate::inventory::Kind;
    use crate::item::{Kind as ItemKind, Stack};
    use crate::nbt::{Compound, List, Value};
    use crate::save::Response;
    use crate::tick::queue::{Input, SessionId};
    use crate::tick::task::data::tests::snapshot;
    use crate::tick::task::data::{DATA_KEY, registered};
    use crate::tick::tests::{join, new_state, settings, tick};
    use crate::tick::{Output, Phase};

    #[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
    struct Kills(u32);

    #[derive(Debug, Default, Serialize, Deserialize)]
    struct Round(u32);

    /// The Xbox user ID of the player `join(1, _)` joins.
    const XUID: u64 = 2_535_000_001;

    /// What the load callbacks of a test got, in order.
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: tests share results with 'static callbacks"
    )]
    type Seen = Arc<std::sync::Mutex<Vec<Result<Ticket>>>>;

    fn note(seen: &Seen) -> impl FnOnce(&mut Context<'_>, Result<Ticket>) + Send + 'static {
        let seen = Arc::clone(seen);
        move |_, result| seen.lock().unwrap().push(result)
    }

    fn taken(seen: &Seen) -> Vec<Result<Ticket>> {
        std::mem::take(&mut *seen.lock().unwrap())
    }

    /// Makes a server that saves player data, with player data `Kills`.
    fn state() -> State {
        let mut state = new_state(
            settings(|c| c.general.save_player_data = true),
            Listeners::default(),
        );
        state.data = registered::<Kills, Round>("kills", "round");
        state
    }

    fn stick(count: u8) -> Stack {
        Stack::new(ItemKind::Stick.default_type(), count)
    }

    /// Makes a saved record: 3 kills, 5 sticks in main slot 0, and a key no module reads.
    fn saved() -> PlayerData {
        let mut slot = stick(5).to_nbt();
        slot.insert("Slot".to_owned(), Value::Byte(0));
        let kills = Compound::from([("kills".to_owned(), Value::ByteArray(b"3".to_vec()))]);
        let extra = Compound::from([
            (
                "Inventory".to_owned(),
                Value::List(List::Compound(vec![slot])),
            ),
            (DATA_KEY.to_owned(), Value::Compound(kills)),
            ("kept".to_owned(), Value::Int(7)),
        ]);
        PlayerData {
            world: "world".to_owned(),
            position: [0.5, 1.62, 0.5],
            extra,
        }
    }

    // pmmp: Server::getOfflinePlayerData, Server::saveOfflinePlayerData
    #[test]
    fn an_offline_players_record_is_held_changed_and_saved_by_the_next_snapshot() {
        let mut state = state();
        let mut output = Output::default();
        let seen = Seen::default();
        let mut cx = Context { state: &mut state };
        // Two loads of one player: one request, two tickets on one record.
        cx.hold_offline_player(Some(XUID), "", note(&seen)).unwrap();
        cx.hold_offline_player(Some(XUID), "", note(&seen)).unwrap();
        tick(&mut state, &mut Vec::new(), &mut output);
        assert_eq!(output.answers.len(), 1);
        assert!(taken(&seen).is_empty(), "the answer comes in a later tick");
        if let Response::Player { data, .. } = &mut output.answers[0] {
            *data = PlayerLoad::Found(saved());
        }
        tick(&mut state, &mut Vec::new(), &mut output);
        let [Ok(first), Ok(second)] = taken(&seen)[..] else {
            panic!("two tickets");
        };
        assert_ne!(first, second);
        let mut cx = Context { state: &mut state };
        let record = cx.offline_player(first).unwrap();
        assert_eq!(record.data::<Kills>(), Some(&Kills(3)));
        assert!(
            record.data::<Round>().is_none(),
            "server data is not player data"
        );
        assert_eq!(record.inventory(Kind::Main).item(0), Some(&stick(5)));
        let record = cx.offline_player_mut(first).unwrap();
        record.data_mut::<Kills>().unwrap().0 = 4;
        record
            .inventory_mut(Kind::EnderChest)
            .set(2, stick(1))
            .unwrap();
        assert_eq!(
            cx.offline_player(second).unwrap().data::<Kills>(),
            Some(&Kills(4))
        );
        assert!(cx.release_offline_player(first));
        assert!(!cx.release_offline_player(first));
        assert!(cx.offline_player(first).is_none());
        assert!(cx.offline_player(second).is_some());
        assert!(cx.release_offline_player(second));
        assert!(state.tasks.offline.is_idle());
        let snapshot = snapshot(&mut state);
        let [(key, data)] = &snapshot.players[..] else {
            panic!("{:?}", snapshot.players);
        };
        assert_eq!(key.as_str(), "2535000001");
        assert_eq!(data.extra.get("kept"), Some(&Value::Int(7)));
        assert_eq!(
            (data.world.as_str(), data.position),
            ("world", [0.5, 1.62, 0.5])
        );
        let again = Offline::load(&state, data.clone());
        assert_eq!(again.data::<Kills>(), Some(&Kills(4)));
        assert_eq!(again.inventory(Kind::Main).item(0), Some(&stick(5)));
        assert_eq!(again.inventory(Kind::EnderChest).item(2), Some(&stick(1)));
        assert!(state.saving.is_idle(), "the commit lets the data go");
    }

    /// Runs two ticks: the first applies `batch` and sends the loads asked for, the save thread
    /// answers each player load with `load`, and the second applies the answers.
    fn answered(state: &mut State, mut batch: Vec<Input>, output: &mut Output, load: &PlayerLoad) {
        tick(state, &mut batch, output);
        for answer in &mut output.answers {
            if let Response::Player { data, .. } = answer {
                *data = load.clone();
            }
        }
        tick(state, &mut Vec::new(), output);
    }

    /// Holds the player of `join(1, _)` with the saved record [`saved`]: its ticket.
    fn held(state: &mut State, output: &mut Output) -> Ticket {
        let seen = Seen::default();
        Context { state: &mut *state }
            .hold_offline_player(Some(XUID), "", note(&seen))
            .unwrap();
        answered(state, Vec::new(), output, &PlayerLoad::Found(saved()));
        match taken(&seen)[..] {
            [Ok(ticket)] => ticket,
            ref other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_record_only_read_is_not_saved_and_a_change_of_one_part_leaves_the_others_as_saved() {
        let mut state = state();
        let mut output = Output::default();
        let ticket = held(&mut state, &mut output);
        let mut cx = Context { state: &mut state };
        assert_eq!(
            cx.offline_player(ticket).unwrap().data::<Kills>(),
            Some(&Kills(3))
        );
        assert!(cx.release_offline_player(ticket));
        assert!(snapshot(&mut state).players.is_empty());
        let ticket = held(&mut state, &mut output);
        let mut cx = Context { state: &mut state };
        cx.offline_player_mut(ticket)
            .unwrap()
            .data_mut::<Kills>()
            .unwrap()
            .0 = 5;
        assert!(cx.release_offline_player(ticket));
        let snapshot = snapshot(&mut state);
        let mut extra = saved().extra;
        let written = &snapshot.players[0].1.extra;
        assert_ne!(written.get(DATA_KEY), extra.get(DATA_KEY));
        extra.insert(DATA_KEY.to_owned(), written[DATA_KEY].clone());
        assert_eq!(*written, extra, "the inventory keys are as saved");
    }

    #[test]
    fn a_snapshot_takes_a_held_records_changes_and_a_join_takes_the_later_ones() {
        let mut state = state();
        let mut output = Output::default();
        let ticket = held(&mut state, &mut output);
        let mut cx = Context { state: &mut state };
        cx.offline_player_mut(ticket)
            .unwrap()
            .data_mut::<Kills>()
            .unwrap()
            .0 = 8;
        assert_eq!(snapshot(&mut state).players.len(), 1);
        assert!(snapshot(&mut state).players.is_empty(), "taken once");
        let mut cx = Context { state: &mut state };
        cx.offline_player_mut(ticket)
            .unwrap()
            .data_mut::<Kills>()
            .unwrap()
            .0 = 9;
        // The player joins with the record still held: it joins from memory, with the change.
        let (j, _rx) = join(1, 0);
        tick(&mut state, &mut vec![j], &mut output);
        assert!(state.loading.is_empty(), "no load was asked");
        let p = &state.players[&SessionId::new(1)];
        assert_ne!(p.phase, Phase::Loading);
        assert_eq!(p.data::<Kills>(), Some(&Kills(9)));
        assert!(p.data_dirty, "the change is in no snapshot yet");
        let mut cx = Context { state: &mut state };
        assert!(cx.offline_player(ticket).is_none());
        assert!(!cx.release_offline_player(ticket));
        assert_eq!(
            cx.hold_offline_player(Some(XUID), "", |_, _| {}),
            Err(Error::Online)
        );
        assert!(state.tasks.offline.is_idle());
    }

    #[test]
    fn a_player_who_joins_while_its_load_waits_joins_with_the_answer_and_the_load_fails() {
        let mut state = state();
        let mut output = Output::default();
        let seen = Seen::default();
        Context { state: &mut state }
            .hold_offline_player(Some(XUID), "player1", note(&seen))
            .unwrap();
        let (j, _rx) = join(1, 0);
        // The join's load and the offline load, answered in order.
        answered(
            &mut state,
            vec![j],
            &mut output,
            &PlayerLoad::Found(saved()),
        );
        assert_eq!(taken(&seen), [Err(Error::Online)]);
        let p = &state.players[&SessionId::new(1)];
        assert_ne!(p.phase, Phase::Loading);
        assert_eq!(p.data::<Kills>(), Some(&Kills(3)));
        assert!(state.tasks.offline.is_idle());
    }

    #[test]
    fn loads_fail_for_a_player_never_saved_damaged_data_no_saving_a_bad_name_or_too_many() {
        let mut off = new_state(settings(|_| {}), Listeners::default());
        assert_eq!(
            Context { state: &mut off }.hold_offline_player(Some(XUID), "", |_, _| {}),
            Err(Error::Disabled)
        );
        let mut state = state();
        let mut output = Output::default();
        let seen = Seen::default();
        let mut cx = Context { state: &mut state };
        assert_eq!(
            cx.hold_offline_player(None, "no such name!", |_, _| {}),
            Err(Error::NotSaved)
        );
        for _ in 0..MAX_TICKETS {
            cx.hold_offline_player(None, "Alex", note(&seen)).unwrap();
        }
        assert_eq!(
            cx.hold_offline_player(None, "Steve", |_, _| {}),
            Err(Error::Full)
        );
        let documented = format!("past {MAX_TICKETS} tickets held or asked for");
        assert!(
            include_str!("offline.rs").contains(&documented),
            "the docs name the bound"
        );
        tick(&mut state, &mut Vec::new(), &mut output);
        let [Response::Player { key, .. }] = &output.answers[..] else {
            panic!("one request for one player");
        };
        assert_eq!(key.as_str(), "offline:alex");
        // The stand-in save thread has no data for anyone.
        tick(&mut state, &mut Vec::new(), &mut output);
        let results = taken(&seen);
        assert_eq!(results.len(), MAX_TICKETS);
        assert!(results.iter().all(|r| *r == Err(Error::NotSaved)));
        assert!(state.tasks.offline.is_idle());
        Context { state: &mut state }
            .hold_offline_player(None, "ALEX", note(&seen))
            .unwrap();
        answered(&mut state, Vec::new(), &mut output, &PlayerLoad::Damaged);
        assert_eq!(taken(&seen), [Err(Error::Damaged)]);
        assert!(state.tasks.offline.is_idle());
    }

    #[test]
    fn a_repeated_answer_changes_nothing() {
        let answer = || Response::Player {
            key: StoredPlayer::xuid(XUID),
            data: PlayerLoad::Found(saved()),
        };
        let run = |answers: Vec<Response>| {
            let mut state = state();
            Context { state: &mut state }
                .hold_offline_player(Some(XUID), "", |_, _| {})
                .unwrap();
            let mut output = Output::default();
            tick(&mut state, &mut Vec::new(), &mut output);
            output.answers = answers;
            tick(&mut state, &mut Vec::new(), &mut output);
            state.hash()
        };
        assert_eq!(run(vec![answer()]), run(vec![answer(), answer()]));
        assert_ne!(run(vec![answer()]), run(Vec::new()));
    }

    #[test]
    fn a_checkpoint_is_opaque_while_a_load_waits_or_a_record_is_held() {
        let pool = crate::runtime::cpu_pool(1).unwrap();
        let mut state = state();
        let mut output = Output::default();
        assert_eq!(state.checkpoint(&pool).opaque(), None);
        let seen = Seen::default();
        Context { state: &mut state }
            .hold_offline_player(Some(XUID), "", note(&seen))
            .unwrap();
        tick(&mut state, &mut Vec::new(), &mut output);
        assert!(state.checkpoint(&pool).opaque().is_some());
        tick(&mut state, &mut Vec::new(), &mut output);
        let [Err(Error::NotSaved)] = taken(&seen)[..] else {
            panic!("the stand-in save thread has no data");
        };
        assert_eq!(state.checkpoint(&pool).opaque(), None);
        let ticket = held(&mut state, &mut output);
        assert!(state.checkpoint(&pool).opaque().is_some());
        assert!(Context { state: &mut state }.release_offline_player(ticket));
        assert_eq!(state.checkpoint(&pool).opaque(), None);
        assert!(pool.shutdown());
    }
}
