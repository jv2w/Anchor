//! User data on the tick side ([`crate::data`]): the registered types and the server's values, each
//! player's values and extensions, and how they reach a save.
//!
//! A player's data values are saved in its record's extra compound, under [`DATA_KEY`]: a
//! compound of one byte array (the JSON) per registered name. So they ride the same record, the
//! same commit and the same input log entry as the rest of the player. Server values are the
//! `user` table, one value per name, written by the snapshot that follows their change.
//!
//! performance: the JSON is encoded on the tick thread, at the snapshot's tick boundary
//! ([`UserData::take_changes`], [`save_player`]). The values are game state that a snapshot must
//! take whole at one boundary (atomicity comes before performance), so the encoding is not capped
//! per snapshot: a cap would save part of a boundary. The cost is bounded instead: only server
//! values changed since the last snapshot ([`MAX_SERVER_TYPES`] × [`MAX_SERVER_VALUE_BYTES`] at
//! most) and the players marked dirty ([`MAX_PLAYER_TYPES`] × [`MAX_PLAYER_VALUE_BYTES`] each),
//! once per save interval (10 minutes by default), with a write that stops at each value's
//! limit. The values are `Send + Sync`, so the encoding could move to the CPU pool if a
//! measurement shows the boundary tick's MSPT matters; that needs the pool at the boundary.

use std::any::{Any, TypeId};
use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::{error, warn};

use super::super::Player;
use super::super::checkpoint::{self, Codec as _, Reader, Writer};
use super::super::extra::PlayerExtra;
use super::Context;
use crate::ServerBuilder;
use crate::data::{
    Codec, Error, MAX_EXTENSIONS, MAX_PLAYER_TYPES, MAX_PLAYER_VALUE_BYTES, MAX_SERVER_TYPES,
    MAX_SERVER_VALUE_BYTES, Result, Value, check_name,
};
use crate::nbt::{self, Compound};

/// The key of the player's extra compound that holds its data values.
pub(in crate::tick) const DATA_KEY: &str = "anchor:data";

/// A server value and whether it changed since a snapshot last took it.
struct Server {
    codec: Codec,
    value: Value,
    dirty: bool,
}

/// The registered data types and the server's values, fixed at build.
#[derive(Default)]
pub(crate) struct UserData {
    /// Player data types, in registration order.
    players: Vec<Codec>,
    /// Server values, in registration order.
    servers: Vec<Server>,
    /// The place of each server type in `servers`.
    server_index: BTreeMap<TypeId, usize>,
    /// The first registration that broke a rule, reported by [`Self::finish`].
    error: Option<Error>,
}

impl UserData {
    /// Registers `T` as player data under `name`.
    fn register_player<T: Default + Serialize + DeserializeOwned + Send + Sync + 'static>(
        &mut self,
        name: String,
    ) {
        let check = || {
            check_name(&name)?;
            if self.players.len() >= MAX_PLAYER_TYPES {
                return Err(Error::TooManyTypes);
            }
            if self.players.iter().any(|c| c.name == name) {
                return Err(Error::DuplicateName { name: name.clone() });
            }
            if self.players.iter().any(|c| c.type_id == TypeId::of::<T>()) {
                return Err(Error::DuplicateType { name: name.clone() });
            }
            Ok(())
        };
        match check() {
            Ok(()) => self.players.push(Codec::of::<T>(name)),
            Err(e) => {
                self.error.get_or_insert(e);
            }
        }
    }

    /// Registers `T` as server data under `name`, at its default until [`Self::load`].
    fn register_server<T: Default + Serialize + DeserializeOwned + Send + Sync + 'static>(
        &mut self,
        name: String,
    ) {
        let check = || {
            check_name(&name)?;
            if self.servers.len() >= MAX_SERVER_TYPES {
                return Err(Error::TooManyTypes);
            }
            if self.servers.iter().any(|s| s.codec.name == name) {
                return Err(Error::DuplicateName { name: name.clone() });
            }
            if self.server_index.contains_key(&TypeId::of::<T>()) {
                return Err(Error::DuplicateType { name: name.clone() });
            }
            Ok(())
        };
        match check() {
            Ok(()) => {
                let codec = Codec::of::<T>(name);
                self.server_index
                    .insert(TypeId::of::<T>(), self.servers.len());
                self.servers.push(Server {
                    value: codec.default_value(),
                    codec,
                    dirty: false,
                });
            }
            Err(e) => {
                self.error.get_or_insert(e);
            }
        }
    }

    /// The first registration that broke a rule; the server is not built then.
    pub(crate) fn finish(&self) -> Result<()> {
        self.error.clone().map_or(Ok(()), Err)
    }

    /// Reads the saved server values before the threads start. A value never saved stays at its
    /// default. A damaged value, or one that does not deserialize into its type, refuses the start
    /// and stays as saved.
    pub(crate) fn load(&mut self, thread: &mut crate::save::Thread) -> crate::Result<()> {
        let names: Vec<&str> = self.servers.iter().map(|s| s.codec.name.as_str()).collect();
        let saved = thread.read_user(&names)?;
        for (server, json) in self.servers.iter_mut().zip(saved) {
            let Some(json) = json else {
                continue;
            };
            let Some(value) = server.codec.decode(&json) else {
                let name = server.codec.name.clone();
                error!(
                    name,
                    "saved server data does not deserialize into its type; the start is refused and the value kept as saved"
                );
                return Err(crate::Error::WorldDamaged(Box::new(Error::Undecodable {
                    name,
                })));
            };
            server.value = value;
        }
        Ok(())
    }

    /// The JSON of every server value changed since the last call, for a snapshot. A value whose
    /// JSON is over its limit is not written (the saved form stays) and is tried again by the
    /// next snapshot.
    pub(in crate::tick) fn take_changes(&mut self) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        for server in self.servers.iter_mut().filter(|s| s.dirty) {
            match server.codec.encode(&server.value, MAX_SERVER_VALUE_BYTES) {
                Ok(json) => {
                    server.dirty = false;
                    out.push((server.codec.name.clone(), json));
                }
                Err(e) => warn!(
                    name = server.codec.name,
                    error = ?e,
                    max_bytes = MAX_SERVER_VALUE_BYTES,
                    "server data not saved: its json is over max_bytes or does not serialize; the last saved value stays"
                ),
            }
        }
        out
    }

    /// Hashes the server values and every player's data values by their JSON, and how many
    /// extensions each player holds (state hash; extensions are not serializable).
    #[cfg(test)]
    pub(in crate::tick) fn hash_into(
        &self,
        players: &BTreeMap<super::super::queue::SessionId, Player>,
        h: &mut super::super::Fnv,
    ) {
        let json = |h: &mut super::super::Fnv, codec: &Codec, value: &Value| match codec
            .encode(value, usize::MAX)
        {
            Ok(bytes) => h.bytes(&bytes),
            Err(_) => h.u64(u64::MAX),
        };
        h.usize(self.servers.len());
        for server in &self.servers {
            h.u64(u64::from(server.dirty));
            json(h, &server.codec, &server.value);
        }
        for player in players.values() {
            let user = &player.extra.user;
            h.usize(user.extensions.len());
            h.usize(user.data.len());
            for codec in &self.players {
                match user.data.get(&codec.type_id) {
                    Some(value) => json(h, codec, value),
                    None => h.u64(0),
                }
            }
        }
    }
}

impl UserData {
    /// Writes the server values by their JSON (as `Self::hash_into` hashes them), with their
    /// marks; the types are code, fixed at build. A value whose JSON does not encode makes the
    /// checkpoint opaque.
    pub(in crate::tick) fn checkpoint(&self, w: &mut Writer) {
        w.len(self.servers.len());
        for server in &self.servers {
            server.dirty.encode(w);
            json(w, &server.codec, &server.value);
        }
    }

    /// Reads what [`Self::checkpoint`] wrote into the types registered at build.
    pub(in crate::tick) fn restore(&mut self, r: &mut Reader<'_>) -> checkpoint::Result<()> {
        if r.len()? != self.servers.len() {
            return Err(checkpoint::Error::Format("server data types"));
        }
        for server in &mut self.servers {
            server.dirty = bool::decode(r)?;
            server.value = value(r, &server.codec)?;
        }
        Ok(())
    }

    /// Writes a player's data values by their JSON, one per registered player type that has a
    /// value. An extension has no checkpoint form (it is not serializable): it makes the
    /// checkpoint opaque.
    pub(in crate::tick) fn checkpoint_player(&self, user: &PlayerUser, w: &mut Writer) {
        let PlayerUser { data, extensions } = user;
        w.opaque("player extensions", extensions.len());
        for codec in &self.players {
            match data.get(&codec.type_id) {
                Some(v) => {
                    w.u8(1);
                    json(w, codec, v);
                }
                None => w.u8(0),
            }
        }
    }

    /// Reads what [`Self::checkpoint_player`] wrote: the values, and no extension.
    pub(in crate::tick) fn restore_player(
        &self,
        r: &mut Reader<'_>,
    ) -> checkpoint::Result<PlayerUser> {
        let mut data = BTreeMap::new();
        for codec in &self.players {
            if bool::decode(r)? {
                data.insert(codec.type_id, value(r, codec)?);
            }
        }
        Ok(PlayerUser {
            data,
            extensions: BTreeMap::new(),
        })
    }
}

/// Writes a value's JSON; one that does not encode makes the checkpoint opaque.
fn json(w: &mut Writer, codec: &crate::data::Codec, value: &Value) {
    if let Ok(bytes) = codec.encode(value, usize::MAX) {
        w.bytes(&bytes);
    } else {
        w.opaque("user data that does not serialize", 1);
        w.len(0);
    }
}

/// Reads a value's JSON into its type.
fn value(r: &mut Reader<'_>, codec: &crate::data::Codec) -> checkpoint::Result<Value> {
    let bytes = Vec::<u8>::decode(r)?;
    codec
        .decode(&bytes)
        .ok_or(checkpoint::Error::Format("user data"))
}

impl fmt::Debug for UserData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = |codecs: Vec<&Codec>| codecs.into_iter().map(|c| c.name.clone()).collect();
        let players: Vec<String> = names(self.players.iter().collect());
        let servers: Vec<String> = names(self.servers.iter().map(|s| &s.codec).collect());
        f.debug_struct("UserData")
            .field("players", &players)
            .field("servers", &servers)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// The user's part of an online player: its data values (one per registered type that has a
/// value; a saved value that does not deserialize has none and stays as saved) and its
/// extensions.
#[derive(Default)]
pub(in crate::tick) struct PlayerUser {
    data: BTreeMap<TypeId, Value>,
    extensions: BTreeMap<TypeId, Value>,
}

impl fmt::Debug for PlayerUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlayerUser")
            .field("data", &self.data.len())
            .field("extensions", &self.extensions.len())
            .finish()
    }
}

/// The player's saved data values, from its extra compound: what [`DATA_KEY`] holds, if it is a
/// compound.
fn saved_values(extra: &Compound) -> Option<&Compound> {
    match extra.get(DATA_KEY) {
        Some(nbt::Value::Compound(values)) => Some(values),
        _ => None,
    }
}

/// A player is placed in the game (a new player, or its saved data arrived): its data values
/// come from its saved extra, or are their types' defaults. A saved value that does not
/// deserialize into its type is left as saved and reads as `None`.
pub(in crate::tick) fn placed(data: &UserData, extra: &mut PlayerExtra) {
    let mut values = BTreeMap::new();
    let raw = extra.raw();
    let foreign = raw.contains_key(DATA_KEY) && saved_values(raw).is_none();
    let saved = saved_values(raw);
    for codec in &data.players {
        let value = match saved.and_then(|c| c.get(&codec.name)) {
            None if !foreign => Some(codec.default_value()),
            Some(nbt::Value::ByteArray(json)) => codec.decode(json),
            None | Some(_) => None,
        };
        if let Some(value) = value {
            values.insert(codec.type_id, value);
        } else {
            error!(
                name = codec.name,
                "saved player data does not deserialize into its type; it reads as none and is kept as saved"
            );
        }
    }
    extra.user.data = values;
}

/// Writes the player's data values into `out`, the extra compound a save writes (it starts as
/// the one kept as saved). A value whose JSON is over its limit keeps its saved form.
pub(in crate::tick) fn save_player(data: &UserData, extra: &PlayerExtra, out: &mut Compound) {
    if extra.user.data.is_empty() {
        return;
    }
    let mut values = match out.remove(DATA_KEY) {
        Some(nbt::Value::Compound(values)) => values,
        None => Compound::new(),
        Some(other) => {
            // Not written by Anchor: kept as it is (`placed` read no value from it).
            out.insert(DATA_KEY.to_owned(), other);
            return;
        }
    };
    for codec in &data.players {
        let Some(value) = extra.user.data.get(&codec.type_id) else {
            continue;
        };
        match codec.encode(value, MAX_PLAYER_VALUE_BYTES) {
            Ok(json) => {
                values.insert(codec.name.clone(), nbt::Value::ByteArray(json));
            }
            Err(e) => warn!(
                name = codec.name,
                error = ?e,
                max_bytes = MAX_PLAYER_VALUE_BYTES,
                "player data not saved: its json is over max_bytes or does not serialize; the last saved value stays"
            ),
        }
    }
    out.insert(DATA_KEY.to_owned(), nbt::Value::Compound(values));
}

/// A snapshot took `saved` for the player: its data values in it become the ones kept as saved.
pub(in crate::tick) fn player_saved(extra: &mut PlayerExtra, saved: &Compound) {
    extra.keep_saved(DATA_KEY, saved);
}

impl Player {
    /// The player's value of the registered player data type `T`
    /// ([`ServerBuilder::player_data`]): saved with the player, its [`Default`] for a new
    /// player. `None` if `T` is not registered, or if its saved value no longer deserializes
    /// into `T` (it is kept as saved).
    pub fn data<T: Any>(&self) -> Option<&T> {
        self.extra
            .user
            .data
            .get(&TypeId::of::<T>())
            .and_then(|v| v.downcast_ref())
    }

    /// [`Self::data`] to change; the player is saved with the next snapshot.
    pub fn data_mut<T: Any>(&mut self) -> Option<&mut T> {
        let value = self
            .extra
            .user
            .data
            .get_mut(&TypeId::of::<T>())?
            .downcast_mut()?;
        self.data_dirty = true;
        Some(value)
    }

    /// The player's extension of type `T`: any value code keeps on the player while it is
    /// online. Extensions are not saved and go when the player leaves.
    pub fn extension<T: Any>(&self) -> Option<&T> {
        self.extra
            .user
            .extensions
            .get(&TypeId::of::<T>())
            .and_then(|v| v.downcast_ref())
    }

    /// [`Self::extension`] to change.
    pub fn extension_mut<T: Any>(&mut self) -> Option<&mut T> {
        self.extra
            .user
            .extensions
            .get_mut(&TypeId::of::<T>())
            .and_then(|v| v.downcast_mut())
    }

    /// Sets the player's extension of type `T`, returning the one it replaces. Fails, keeping
    /// nothing, when `T` is new and the player holds [`MAX_EXTENSIONS`] types already.
    pub fn insert_extension<T: Any + Send + Sync>(&mut self, value: T) -> Result<Option<T>> {
        let extensions = &mut self.extra.user.extensions;
        let id = TypeId::of::<T>();
        if !extensions.contains_key(&id) && extensions.len() >= MAX_EXTENSIONS {
            return Err(Error::TooManyExtensions);
        }
        let old = extensions.insert(id, Box::new(value));
        Ok(old.and_then(|v| v.downcast().ok()).map(|v| *v))
    }

    /// Takes the player's extension of type `T` off it.
    pub fn remove_extension<T: Any>(&mut self) -> Option<T> {
        let value = self.extra.user.extensions.remove(&TypeId::of::<T>())?;
        value.downcast().ok().map(|v| *v)
    }
}

impl Context<'_> {
    /// The server's value of the registered server data type `T`
    /// ([`ServerBuilder::server_data`]): loaded at start, its [`Default`] if never saved.
    /// `None` if `T` is not registered.
    pub fn data<T: Any>(&self) -> Option<&T> {
        let &i = self.state.data.server_index.get(&TypeId::of::<T>())?;
        self.state.data.servers.get(i)?.value.downcast_ref()
    }

    /// [`Self::data`] to change; it is saved with the next snapshot.
    pub fn data_mut<T: Any>(&mut self) -> Option<&mut T> {
        let &i = self.state.data.server_index.get(&TypeId::of::<T>())?;
        let server = self.state.data.servers.get_mut(i)?;
        let value = server.value.downcast_mut()?;
        server.dirty = true;
        Some(value)
    }
}

impl ServerBuilder {
    /// Gives every player a value of `T`, saved under `name` with the player's other data in
    /// the same commit ([`Player::data`], [`crate::data`]). A new player starts at
    /// `T::default()`. `name` is 1 to 64 bytes of `a-z`, `0-9`, `_`, `-`, `.`, `:`; at most 16
    /// types. [`build`](Self::build) fails on a bad or repeated name or type.
    #[must_use]
    pub fn player_data<T: Default + Serialize + DeserializeOwned + Send + Sync + 'static>(
        mut self,
        name: impl Into<String>,
    ) -> Self {
        self.setup.data.register_player::<T>(name.into());
        self
    }

    /// Gives the server one value of `T`, saved under `name` in the same commit as the rest of
    /// the game ([`Context::data`], [`crate::data`]), and read back at start. Names follow
    /// [`Self::player_data`]; at most 64 types.
    #[must_use]
    pub fn server_data<T: Default + Serialize + DeserializeOwned + Send + Sync + 'static>(
        mut self,
        name: impl Into<String>,
    ) -> Self {
        self.setup.data.register_server::<T>(name.into());
        self
    }
}

/// The tick's user data for tests: `T` registered as player data under `name` and `S` as
/// server data under `server`.
#[cfg(test)]
pub(in crate::tick) fn registered<T, S>(name: &str, server: &str) -> UserData
where
    T: Default + Serialize + DeserializeOwned + Send + Sync + 'static,
    S: Default + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    let mut data = UserData::default();
    data.register_player::<T>(name.to_owned());
    data.register_server::<S>(server.to_owned());
    data
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde::Deserialize;

    use super::super::super::queue::SessionId;
    use super::super::super::tests::{join, leave, new_state, settings, tick};
    use super::super::super::{Output, Phase, State};
    use super::*;
    use crate::event::Listeners;
    use crate::save::{PlayerData, PlayerLoad, Request, Response, Snapshot};

    #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
    struct Kills {
        total: u32,
        names: Vec<String>,
    }

    #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
    struct Round(u32);

    /// A state with player data `Kills` and server data `Round` and player data on.
    fn state() -> State {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        state.data = registered::<Kills, Round>("kills", "round");
        state.saving.players = true;
        state
    }

    /// Joins session 1, answering its load with `load`; returns its queue.
    fn join_with(
        state: &mut State,
        load: &PlayerLoad,
    ) -> tokio::sync::mpsc::Receiver<super::super::super::queue::Outbound> {
        let (j, rx) = join(1, 0);
        let mut output = Output::default();
        tick(state, &mut vec![j], &mut output);
        for a in &mut output.answers {
            if let Response::Player { data, .. } = a {
                *data = load.clone();
            }
        }
        tick(state, &mut Vec::new(), &mut output);
        assert_ne!(state.players[&SessionId::new(1)].phase, Phase::Loading);
        rx
    }

    fn player(state: &mut State) -> &mut Player {
        state.players.get_mut(&SessionId::new(1)).unwrap()
    }

    /// The snapshot the next tick boundary takes, then its commit.
    fn snapshot(state: &mut State) -> Snapshot {
        state.saving.requested = true;
        let mut output = Output::default();
        let mut log = super::super::super::InputLog::default();
        super::super::super::boundary(state, &mut output, &mut log);
        let snapshot = output
            .requests
            .into_iter()
            .find_map(|r| match r {
                Request::Commit(s) => Some(*s),
                _ => None,
            })
            .expect("a snapshot");
        super::super::super::save::committed(state, &mut log, snapshot.start);
        snapshot
    }

    fn found(extra: Compound) -> PlayerLoad {
        PlayerLoad::Found(PlayerData {
            world: "world".to_owned(),
            position: [0.5, -58.38, 0.5],
            extra,
        })
    }

    #[test]
    fn extensions_are_typed_per_player_bounded_and_go_when_the_player_leaves() {
        #[derive(Debug, PartialEq)]
        struct Team(&'static str);
        struct Slot<const N: usize>;
        let mut state = state();
        let _rx = join_with(&mut state, &PlayerLoad::Absent);
        let (second, _rx2) = join(2, 0);
        tick(&mut state, &mut vec![second], &mut Output::default());
        let held = Arc::new(());
        let before = state.hash();
        let p = player(&mut state);
        assert_eq!(p.extension::<Team>(), None);
        assert_eq!(p.insert_extension(Team("red")), Ok(None));
        assert_eq!(p.insert_extension(Team("blue")), Ok(Some(Team("red"))));
        p.extension_mut::<Team>().unwrap().0 = "green";
        assert_eq!(p.extension::<Team>(), Some(&Team("green")));
        assert_eq!(p.insert_extension(Arc::clone(&held)), Ok(None));
        assert!(!p.data_dirty, "extensions are not saved");
        assert_ne!(state.hash(), before);
        let other = state.players.get(&SessionId::new(2)).unwrap();
        assert_eq!(other.extension::<Team>(), None, "per player");
        // 64 types at most; replacing one is always allowed.
        let p = player(&mut state);
        macro_rules! fill {
            ($($n:literal)*) => {$(
                p.insert_extension(Slot::<$n>).unwrap();
            )*};
        }
        fill!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29
            30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57
            58 59 60 61);
        assert_eq!(p.extra.user.extensions.len(), MAX_EXTENSIONS);
        assert!(matches!(
            p.insert_extension(Slot::<62>),
            Err(Error::TooManyExtensions)
        ));
        assert!(p.insert_extension(Team("again")).is_ok());
        assert!(p.remove_extension::<Slot<0>>().is_some());
        assert!(p.remove_extension::<Slot<0>>().is_none());
        assert!(p.insert_extension(Slot::<62>).is_ok());
        assert_eq!(Arc::strong_count(&held), 2);
        tick(&mut state, &mut vec![leave(1, 1)], &mut Output::default());
        assert_eq!(Arc::strong_count(&held), 1, "dropped with the player");
    }

    #[test]
    fn player_data_starts_at_its_default_and_survives_a_save_and_a_rejoin() {
        let mut state = state();
        let _rx = join_with(&mut state, &PlayerLoad::Absent);
        let p = player(&mut state);
        assert_eq!(p.data::<Kills>(), Some(&Kills::default()));
        assert_eq!(p.data::<Round>(), None, "server data is not player data");
        assert!(!p.data_dirty);
        let kills = p.data_mut::<Kills>().unwrap();
        kills.total = 3;
        kills.names.push("alex".to_owned());
        assert!(p.data_dirty);
        let saved = snapshot(&mut state);
        assert_eq!(saved.players.len(), 1);
        let extra = saved.players[0].1.extra.clone();
        let Some(nbt::Value::Compound(values)) = extra.get(DATA_KEY) else {
            panic!("{extra:?}");
        };
        assert_eq!(
            values.get("kills"),
            Some(&nbt::Value::ByteArray(
                br#"{"total":3,"names":["alex"]}"#.to_vec()
            ))
        );
        // The same player in a new server reads its saved value.
        let mut again = self::state();
        let _rx = join_with(&mut again, &found(extra));
        let p = player(&mut again);
        assert_eq!(p.data::<Kills>().unwrap().total, 3);
        assert!(!p.data_dirty);
    }

    #[test]
    fn player_data_that_does_not_deserialize_reads_as_none_and_stays_as_saved() {
        let mut state = state();
        let mut values = Compound::new();
        values.insert(
            "kills".to_owned(),
            nbt::Value::ByteArray(b"\"not kills\"".to_vec()),
        );
        values.insert("removed".to_owned(), nbt::Value::ByteArray(b"[1]".to_vec()));
        let extra = Compound::from([(DATA_KEY.to_owned(), nbt::Value::Compound(values.clone()))]);
        let _rx = join_with(&mut state, &found(extra));
        let p = player(&mut state);
        assert_eq!(p.data::<Kills>(), None);
        assert_eq!(p.data_mut::<Kills>(), None);
        // A save writes the kept bytes back as they were, and the unknown name with them.
        p.data_dirty = true;
        let saved = snapshot(&mut state);
        assert_eq!(
            saved.players[0].1.extra.get(DATA_KEY),
            Some(&nbt::Value::Compound(values))
        );
        // A key Anchor did not write is left alone too.
        let mut state = self::state();
        let odd = Compound::from([(DATA_KEY.to_owned(), nbt::Value::Int(5))]);
        let _rx = join_with(&mut state, &found(odd));
        let p = player(&mut state);
        assert_eq!(p.data::<Kills>(), None);
        p.data_dirty = true;
        let saved = snapshot(&mut state);
        assert_eq!(
            saved.players[0].1.extra.get(DATA_KEY),
            Some(&nbt::Value::Int(5))
        );
    }

    #[test]
    fn an_oversized_player_value_keeps_its_last_saved_form() {
        let mut state = state();
        let _rx = join_with(&mut state, &PlayerLoad::Absent);
        player(&mut state).data_mut::<Kills>().unwrap().total = 1;
        let first = snapshot(&mut state);
        let kept = first.players[0].1.extra.get(DATA_KEY).cloned();
        let p = player(&mut state);
        let kills = p.data_mut::<Kills>().unwrap();
        kills.total = 2;
        kills.names = vec!["x".repeat(MAX_PLAYER_VALUE_BYTES)];
        let second = snapshot(&mut state);
        assert_eq!(second.players[0].1.extra.get(DATA_KEY).cloned(), kept);
        // Back under the limit, it saves again.
        player(&mut state)
            .data_mut::<Kills>()
            .unwrap()
            .names
            .clear();
        let third = snapshot(&mut state);
        let Some(nbt::Value::Compound(values)) = third.players[0].1.extra.get(DATA_KEY) else {
            panic!("no values");
        };
        assert_eq!(
            values.get("kills"),
            Some(&nbt::Value::ByteArray(
                br#"{"total":2,"names":[]}"#.to_vec()
            ))
        );
    }

    #[test]
    fn server_data_is_saved_once_per_change_and_an_oversized_value_waits() {
        let mut state = state();
        let cx = Context { state: &mut state };
        assert_eq!(cx.data::<Round>(), Some(&Round(0)));
        assert_eq!(cx.data::<Kills>(), None, "player data is not server data");
        assert!(snapshot(&mut state).user.is_empty(), "nothing changed");
        let before = state.hash();
        Context { state: &mut state }.data_mut::<Round>().unwrap().0 = 4;
        assert_ne!(state.hash(), before);
        let saved = snapshot(&mut state);
        assert_eq!(saved.user, [("round".to_owned(), b"4".to_vec())]);
        assert!(snapshot(&mut state).user.is_empty());
        // A value over its limit is not written and is tried again.
        let mut big = new_state(settings(|_| {}), Listeners::default());
        big.data = registered::<Kills, Vec<u8>>("kills", "blob");
        *Context { state: &mut big }.data_mut::<Vec<u8>>().unwrap() =
            vec![9; MAX_SERVER_VALUE_BYTES / 2];
        assert!(snapshot(&mut big).user.is_empty());
        Context { state: &mut big }
            .data_mut::<Vec<u8>>()
            .unwrap()
            .truncate(3);
        assert_eq!(
            snapshot(&mut big).user,
            [("blob".to_owned(), b"[9,9,9]".to_vec())]
        );
    }

    #[test]
    fn registration_errors_are_kept_for_the_build() {
        let ok = || UserData::default();
        let mut data = ok();
        data.register_player::<Kills>("Kills".to_owned());
        assert!(matches!(data.finish(), Err(Error::InvalidName { .. })));
        let mut data = ok();
        data.register_player::<Kills>("a".to_owned());
        data.register_player::<Round>("a".to_owned());
        assert!(matches!(data.finish(), Err(Error::DuplicateName { .. })));
        let mut data = ok();
        data.register_server::<Kills>("a".to_owned());
        data.register_server::<Kills>("b".to_owned());
        assert!(matches!(data.finish(), Err(Error::DuplicateType { .. })));
        // The same type and name as player and as server data are two values.
        let mut data = ok();
        data.register_player::<Kills>("a".to_owned());
        data.register_server::<Kills>("a".to_owned());
        assert_eq!(data.finish(), Ok(()));
        let mut data = ok();
        macro_rules! many {
            ($($n:literal)*) => {$(
                {
                    #[derive(Default, Serialize, Deserialize)]
                    struct T;
                    data.register_player::<T>(format!("t{}", $n));
                }
            )*};
        }
        many!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15);
        assert_eq!(data.finish(), Ok(()));
        data.register_player::<Kills>("one-more".to_owned());
        assert_eq!(data.finish(), Err(Error::TooManyTypes));
        assert!(format!("{data:?}").contains("t15"));
        // The builder keeps the first error for `build`.
        let builder = crate::Server::builder()
            .player_data::<Kills>("kills")
            .server_data::<Round>("bad name");
        assert!(matches!(
            builder.setup.data.finish(),
            Err(Error::InvalidName { .. })
        ));
    }

    #[test]
    fn saved_server_data_loads_at_start_and_an_undecodable_value_refuses_it() {
        use crate::save::tests::{TempDir, specs};
        let dir = TempDir::new("user-load");
        let mut opened = crate::save::open(&dir.0, specs()).unwrap();
        let mut data = registered::<Kills, Round>("kills", "round");
        data.load(&mut opened.thread).unwrap();
        let mut state = new_state(settings(|_| {}), Listeners::default());
        state.data = data;
        assert_eq!(
            Context { state: &mut state }.data::<Round>(),
            Some(&Round(0))
        );
        drop(opened);
        // Saved as 7: the next start reads 7.
        let snapshot = Snapshot {
            start: 1,
            user: vec![("round".to_owned(), b"7".to_vec())],
            ..Snapshot::default()
        };
        commit(&dir, snapshot);
        let mut opened = crate::save::open(&dir.0, specs()).unwrap();
        let mut data = registered::<Kills, Round>("kills", "round");
        data.load(&mut opened.thread).unwrap();
        state.data = data;
        assert_eq!(
            Context { state: &mut state }.data::<Round>(),
            Some(&Round(7))
        );
        drop(opened);
        // A value of another type refuses the start; the value stays as saved.
        let mut opened = crate::save::open(&dir.0, specs()).unwrap();
        let mut data = registered::<Kills, Kills>("kills", "round");
        let e = data.load(&mut opened.thread).unwrap_err();
        assert!(matches!(e, crate::Error::WorldDamaged(_)), "{e:?}");
        assert_eq!(
            opened.thread.read_user(&["round"]).unwrap(),
            [Some(b"7".to_vec())]
        );
    }

    /// Commits `snapshot` through the save thread on `dir`.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test waits for the save thread"
    )]
    fn commit(dir: &crate::save::tests::TempDir, snapshot: Snapshot) {
        let opened = crate::save::open(&dir.0, crate::save::tests::specs()).unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let (answer, mut answers) = tokio::sync::mpsc::channel(1);
        let thread = opened.thread;
        let handle = crate::runtime::spawn_save(move || {
            let pool = crate::runtime::cpu_pool(1).unwrap();
            let result = crate::save::run(thread, &pool, rx, &answer);
            assert!(pool.shutdown());
            result
        })
        .unwrap();
        tx.blocking_send(Request::Commit(Box::new(snapshot)))
            .unwrap();
        assert!(matches!(
            answers.blocking_recv(),
            Some(Response::Committed { .. })
        ));
        drop(tx);
        handle.join().unwrap().unwrap();
    }
}
