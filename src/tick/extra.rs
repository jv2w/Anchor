//! Chunk and player extras on the tick side: what the server file keeps of a chunk besides its
//! terrain (block entities, entities) and of a player besides its world
//! and position, and the hooks where rule modules take it over.
//!
//! # Contract for rule modules
//!
//! - **Arrival.** A chunk's extras arrive with its terrain, in the same load answer, and the tick
//!   calls [`Extras::chunk_loaded`] right after it puts the terrain into its world; the rule
//!   modules take them over as [`rules`](mod@super::rules) says. A player's extra arrives with
//!   its saved data and the tick calls [`player_loaded`] when it places the player. A module
//!   that owns a key of it reads its typed state from [`Player::raw`] there and from then on
//!   owns that state.
//! - **Snapshot.** At a save snapshot the tick asks for every chunk kind marked dirty
//!   ([`Extras::take_dirty`]); the modules wrote their changed typed state back as NBT before
//!   ([`rules`](mod@super::rules)), and the snapshot clears the chunk marks. For every player whose
//!   `data_dirty` is set it copies the compound as saved ([`player_saved`]) and every module that
//!   owns a key writes its typed state over it.
//! - **Unload.** When the last holder of a chunk releases it, [`Extras::chunk_unloaded`] gives
//!   the chunk's extras back as NBT. If a kind is dirty, or a snapshot in flight holds the chunk,
//!   the tick keeps them in memory with the terrain until a commit holds them; holding the chunk
//!   again before that calls [`Extras::chunk_restored`] with them, marks included. A module drops
//!   its typed state for the chunk at unload.
//! - **Dirty.** Whoever changes a chunk's extras marks that kind dirty, in a serial phase:
//!   [`Extras::get_mut`] does it. Whoever changes what a player keeps sets the player's
//!   `data_dirty`. A list or key no module touches is never marked, so it is written back byte for
//!   byte, untouched, for as long as it lives.
//! - **Ownership.** The holder keeps an entry for every loaded chunk, empty or not, from
//!   [`Extras::chunk_loaded`] to [`Extras::chunk_unloaded`]; a damaged (quarantined) chunk has
//!   none. Lists are NBT compounds in the shape their module defines; an empty list deletes the
//!   stored value at the next commit.

use std::collections::{BTreeMap, BTreeSet};

use super::view::Key;
use crate::nbt::Compound;
use crate::save::{ChunkExtras, WorldIndex};
use crate::storage::ChunkKind;
use crate::world::ChunkPos;

/// The extras of every loaded chunk.
#[derive(Debug, Default)]
pub(super) struct Extras {
    /// The extras of each loaded chunk.
    chunks: BTreeMap<Key, Held>,
    /// The chunks with a kind marked dirty, so that a snapshot visits only those.
    dirty: BTreeSet<Key>,
}

impl Extras {
    /// Writes each loaded chunk's [`Held::checkpoint`].
    pub(super) fn checkpoint(&self, w: &mut super::checkpoint::Writer) {
        use super::checkpoint::Codec;
        // The dirty set repeats the marks of the chunks, which `Held::checkpoint` writes.
        let Self { chunks, dirty: _ } = self;
        w.len(chunks.len());
        for (key, held) in chunks {
            key.encode(w);
            held.checkpoint(w);
        }
    }

    /// The extras of a loaded chunk.
    #[cfg(test)]
    pub(super) fn get(&self, key: Key) -> Option<&ChunkExtras> {
        self.chunks.get(&key).map(|h| &h.extras)
    }

    /// Hashes the extras of every loaded chunk, for the state hash of tests.
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        // The dirty set repeats the marks of the chunks, which `Held::hash_into` hashes.
        let Self { chunks, dirty: _ } = self;
        h.usize(chunks.len());
        for (&(world, pos), held) in chunks {
            h.usize(world);
            h.u64(u64::from(pos.x.cast_unsigned()));
            h.u64(u64::from(pos.z.cast_unsigned()));
            held.hash_into(h);
        }
    }

    /// The number of chunks with an entry: the loaded ones (for leak tests).
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.chunks.len()
    }

    /// Reads what [`Self::checkpoint`] wrote, the lists of unmarked chunks from `stored`.
    pub(super) fn restore(
        r: &mut super::checkpoint::Reader<'_>,
        stored: &mut BTreeMap<Key, ChunkExtras>,
    ) -> super::checkpoint::Result<Self> {
        use super::checkpoint::{Codec, Error};
        let n = r.len()?;
        let mut chunks = BTreeMap::new();
        let mut dirty = BTreeSet::new();
        for _ in 0..n {
            let key = Key::decode(r)?;
            let held = Held::restore(r, stored.remove(&key))?;
            if held.is_dirty() {
                dirty.insert(key);
            }
            if chunks.insert(key, held).is_some() {
                return Err(Error::Format("repeated chunk extras"));
            }
        }
        Ok(Self { chunks, dirty })
    }

    /// Takes the extras that arrived with a chunk's terrain, which is now loaded: the raw lists
    /// from the file (empty for a chunk without any), unchanged.
    pub(super) fn chunk_loaded(&mut self, key: Key, extras: ChunkExtras) {
        self.chunks.insert(key, Held { extras, dirty: 0 });
        self.dirty.remove(&key);
    }

    /// Takes the extras of a chunk that unloaded before a commit held them and is loaded again
    /// from memory.
    pub(super) fn chunk_restored(&mut self, key: Key, held: Held) {
        if held.is_dirty() {
            self.dirty.insert(key);
        } else {
            self.dirty.remove(&key);
        }
        self.chunks.insert(key, held);
    }

    /// Gives back the extras of a chunk that unloads (or is quarantined) as NBT with their marks,
    /// if it was loaded.
    pub(super) fn chunk_unloaded(&mut self, key: Key) -> Option<Held> {
        self.dirty.remove(&key);
        self.chunks.remove(&key)
    }

    /// The `kind` list of a loaded chunk to change, marked dirty; `None` if the chunk is not
    /// loaded.
    pub(super) fn get_mut(&mut self, key: Key, kind: ChunkKind) -> Option<&mut Vec<Compound>> {
        let held = self.chunks.get_mut(&key)?;
        let list = held.extras.get_mut(kind);
        held.dirty |= bit(kind);
        self.dirty.insert(key);
        Some(list)
    }

    /// Takes the changed lists of `world`'s loaded chunks for a snapshot, in chunk then kind
    /// order, and clears their marks.
    #[expect(
        clippy::expect_used,
        reason = "invariant: a chunk in the dirty set is held"
    )]
    pub(super) fn take_dirty(
        &mut self,
        world: WorldIndex,
    ) -> Vec<(ChunkPos, ChunkKind, Vec<Compound>)> {
        let first = (world, ChunkPos::new(i32::MIN, i32::MIN));
        let last = (world, ChunkPos::new(i32::MAX, i32::MAX));
        let keys: Vec<Key> = self.dirty.range(first..=last).copied().collect();
        let mut out = Vec::new();
        for key in keys {
            self.dirty.remove(&key);
            let held = self
                .chunks
                .get_mut(&key)
                .expect("a chunk in the dirty set is held");
            out.extend(
                held.take_dirty()
                    .into_iter()
                    .map(|(kind, list)| (key.1, kind, list)),
            );
        }
        out
    }
}

/// One chunk's extras and which kinds changed since a snapshot took them.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Held {
    extras: ChunkExtras,
    /// Bit `kind as u8` set: that kind changed since a snapshot last took it.
    dirty: u8,
}

impl Held {
    /// Writes the marks, and the lists when a kind is marked: an unmarked list is the world
    /// file's or the generator's (`Self::hash_into`), which a replay reads again.
    pub(super) fn checkpoint(&self, w: &mut super::checkpoint::Writer) {
        use super::checkpoint::Codec;
        let Self { extras, dirty } = self;
        dirty.encode(w);
        if *dirty != 0 {
            for kind in ChunkKind::EXTRAS {
                w.slice(extras.get(kind));
            }
        }
    }

    /// The lists as held.
    pub(super) fn extras(&self) -> &ChunkExtras {
        &self.extras
    }

    /// Hashes the marks and the changed lists, for the state hash of tests.
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        // As for terrain (`State::hash`): a clean list is the file's or the last snapshot's,
        // fixed by earlier hashed state, so only changed lists are hashed in full.
        let Self { extras, dirty } = self;
        h.u64(u64::from(*dirty));
        for kind in ChunkKind::EXTRAS {
            if dirty & bit(kind) == 0 {
                continue;
            }
            let list = extras.get(kind);
            h.usize(list.len());
            for entry in list {
                hash_nbt(h, entry);
            }
        }
    }

    /// Whether a kind changed since a snapshot last took it.
    pub(super) fn is_dirty(&self) -> bool {
        self.dirty != 0
    }

    /// Reads what [`Self::checkpoint`] wrote; `stored` gives the lists the server file or the
    /// generator has for the chunk, used when no kind is marked.
    pub(super) fn restore(
        r: &mut super::checkpoint::Reader<'_>,
        stored: Option<ChunkExtras>,
    ) -> super::checkpoint::Result<Self> {
        use super::checkpoint::{Codec, Error};
        let dirty = u8::decode(r)?;
        let extras = if dirty == 0 {
            stored.ok_or(Error::Format("chunk extras the server file does not give"))?
        } else {
            let mut extras = ChunkExtras::default();
            for kind in ChunkKind::EXTRAS {
                *extras.get_mut(kind) = Codec::decode(r)?;
            }
            extras
        };
        Ok(Self { extras, dirty })
    }

    /// Takes the changed lists, cloned for a snapshot, and clears their marks.
    pub(super) fn take_dirty(&mut self) -> Vec<(ChunkKind, Vec<Compound>)> {
        let mut out = Vec::new();
        for kind in ChunkKind::EXTRAS {
            if self.dirty & bit(kind) != 0 {
                out.push((kind, self.extras.get(kind).to_vec()));
            }
        }
        self.dirty = 0;
        out
    }
}

/// What the rule modules keep of an online player, between [`player_loaded`] and
/// [`player_saved`], and the user's values on it (`task::PlayerUser`: its data, saved under one
/// key of the compound, and its extensions, never saved).
#[derive(Debug, Default)]
pub(super) struct Player {
    /// The compound as saved: modules read their keys from it when the player loads, and a save
    /// copies it ([`player_saved`]) before they write their keys over the copy.
    /// [`Self::keep_saved`] keeps the newest saved form of one key.
    raw: Compound,
    /// The user's data and extensions.
    pub(super) user: super::task::PlayerUser,
}

impl Player {
    /// Writes the compound as saved and the user's values ([`UserData::checkpoint_player`]).
    ///
    /// [`UserData::checkpoint_player`]: super::task::UserData::checkpoint_player
    pub(super) fn checkpoint(
        &self,
        data: &super::task::UserData,
        w: &mut super::checkpoint::Writer,
    ) {
        use super::checkpoint::Codec;
        let Self { raw, user } = self;
        raw.encode(w);
        data.checkpoint_player(user, w);
    }

    /// Hashes the compound as saved, for the state hash of tests.
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        // The user's values are hashed with their types' codecs (`task::UserData::hash_into`).
        let Self { raw, user: _ } = self;
        hash_nbt(h, raw);
    }

    /// The compound as saved, for the modules that own keys in it to read on load.
    pub(super) fn raw(&self) -> &Compound {
        &self.raw
    }

    /// Reads what [`Self::checkpoint`] wrote.
    pub(super) fn restore(
        r: &mut super::checkpoint::Reader<'_>,
        data: &super::task::UserData,
    ) -> super::checkpoint::Result<Self> {
        use super::checkpoint::Codec;
        Ok(Self {
            raw: Codec::decode(r)?,
            user: data.restore_player(r)?,
        })
    }

    /// Keeps the value of `key` in `saved`, which a save wrote, as the one kept as saved, so a
    /// later save that cannot write that key keeps the newest saved form.
    pub(super) fn keep_saved(&mut self, key: &str, saved: &Compound) {
        match saved.get(key) {
            Some(value) => self.raw.insert(key.to_owned(), value.clone()),
            None => self.raw.remove(key),
        };
    }
}

/// What the player keeps while online, made from the saved extra that arrived with it (empty for
/// a new player).
pub(super) fn player_loaded(extra: Compound) -> Player {
    Player {
        raw: extra,
        user: super::task::PlayerUser::default(),
    }
}

/// A copy of the extra as saved, for a save to write the keys of the modules over.
pub(super) fn player_saved(extra: &Player) -> Compound {
    extra.raw.clone()
}

/// The dirty bit of an extras kind.
const fn bit(kind: ChunkKind) -> u8 {
    1 << kind as u8
}

/// Hashes a compound by its little-endian NBT bytes.
#[cfg(test)]
pub(super) fn hash_nbt(h: &mut super::Fnv, compound: &Compound) {
    let mut w = crate::protocol::Writer::new(Vec::new(), 0);
    match w.nbt(compound, crate::nbt::Encoding::LittleEndian) {
        Ok(()) => h.bytes(w.as_bytes()),
        Err(_) => h.u64(u64::MAX),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    use crate::event::Listeners;
    use crate::nbt::Value;
    use crate::pool::Pool;
    use crate::protocol::Rng;
    use crate::save::{PlayerData, PlayerLoad, Request, Response, Snapshot, Stored};
    use crate::storage::codec::tests::random_compound;
    use crate::tick::queue::{Input, Outbound};
    use crate::tick::tests::{answer, join, leave, new_state, radius, settings, teleported};
    use crate::tick::{InputLog, Output, Phase, State, boundary, deliver, step};

    /// The extras the stand-in save thread answers for a chunk, as if the file held them: a
    /// function of the position, so a replay gets them again. One chunk in five has every kind,
    /// three in five one kind, one none.
    pub(in crate::tick) fn sample(world: WorldIndex, pos: ChunkPos) -> Stored {
        let mut extras = ChunkExtras::default();
        let pick = pos.x.wrapping_mul(31).wrapping_add(pos.z).rem_euclid(5);
        for (i, kind) in (1..).zip(ChunkKind::EXTRAS) {
            if pick == 0 || pick == i {
                let entry = |n: i32| {
                    Compound::from([
                        ("x".to_owned(), Value::Int(pos.x)),
                        ("z".to_owned(), Value::Int(pos.z)),
                        ("n".to_owned(), Value::Int(n)),
                        ("kind".to_owned(), Value::Byte(kind as u8)),
                    ])
                };
                *extras.get_mut(kind) = (0..=i).map(entry).collect();
            }
        }
        crate::save::tests::stored_as_read(u32::try_from(world).unwrap(), pos, extras)
    }

    /// The answer the stand-in save thread gives for the generated `chunk` at `pos`: [`sample`],
    /// read with the world's custom content and the server's entity types as the save thread
    /// reads it.
    pub(in crate::tick) fn sample_loaded(
        state: &State,
        world: WorldIndex,
        pos: ChunkPos,
        chunk: Box<crate::world::Chunk>,
    ) -> crate::save::Loaded {
        let mut stored = sample(world, pos);
        stored.rules = crate::tick::rules::read_kept(
            (world, pos),
            &stored.extras,
            &chunk,
            &state.rules.custom,
            state.rules.entities.types(),
        );
        crate::save::Loaded::Generated(chunk, stored)
    }

    /// Runs ticks without the network; the stand-in save thread answers loads and keeps the
    /// commits unanswered for the test to answer.
    struct Driver {
        pool: Pool,
        answers: Vec<Response>,
        commits: Vec<Snapshot>,
        /// Receivers of the joined sessions; dropping one would drop its session.
        receivers: Vec<tokio::sync::mpsc::Receiver<Outbound>>,
    }

    impl Driver {
        fn new() -> Self {
            Self {
                pool: crate::runtime::cpu_pool(1).unwrap(),
                answers: Vec::new(),
                commits: Vec::new(),
                receivers: Vec::new(),
            }
        }

        fn tick(&mut self, state: &mut State, mut inputs: Vec<Input>) {
            let mut output = Output::default();
            let mut log = InputLog::default();
            let mut answers = std::mem::take(&mut self.answers);
            step(
                state,
                &mut inputs,
                &mut answers,
                &self.pool,
                &mut output,
                &mut log,
            );
            deliver(state, &mut output, &mut log);
            boundary(state, &mut output, &mut log);
            let mut requests = Vec::new();
            for request in output.requests.drain(..) {
                match request {
                    Request::Commit(snapshot) => self.commits.push(*snapshot),
                    request => requests.push(request),
                }
            }
            self.answers = answer(state, requests);
        }

        fn join(&mut self, state: &mut State, mut inputs: Vec<Input>) {
            let (j, rx) = join(1, 0);
            self.receivers.push(rx);
            inputs.insert(0, j);
            self.tick(state, inputs);
        }

        fn settle(&mut self, state: &mut State) {
            for _ in 0..8 {
                self.tick(state, Vec::new());
            }
            assert!(self.answers.is_empty() && state.chunks.requested.is_empty());
        }
    }

    /// Every loaded chunk of world 0 and the holder agree.
    fn assert_one_entry_per_loaded_chunk(state: &State) {
        let loaded: Vec<ChunkPos> = state.worlds[0].loaded_chunks().collect();
        let held: Vec<ChunkPos> = state.chunks.extras.chunks.keys().map(|k| k.1).collect();
        assert_eq!(held, loaded);
    }

    #[test]
    fn extras_arrive_with_their_terrain_and_leave_with_it() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut driver = Driver::new();
        assert_one_entry_per_loaded_chunk(&state);
        let start = state.chunks.extras.len();
        driver.join(&mut state, vec![radius(1, 1, 8)]);
        driver.settle(&mut state);
        assert!(state.chunks.extras.len() > start);
        assert_one_entry_per_loaded_chunk(&state);
        for (&(world, pos), held) in &state.chunks.extras.chunks {
            let extras = state.chunks.extras.get((world, pos));
            assert_eq!(extras, Some(&sample(world, pos).extras), "{pos:?}");
            assert!(!held.is_dirty());
        }
        driver.tick(&mut state, vec![leave(1, 2)]);
        assert_eq!(state.chunks.extras.len(), 9, "only the spawn chunks stay");
        assert_one_entry_per_loaded_chunk(&state);
        assert!(
            state.chunks.unsaved.is_empty(),
            "unchanged extras are not kept"
        );
    }

    #[test]
    fn a_snapshot_takes_the_changed_lists_of_its_world_once() {
        let mut extras = Extras::default();
        let near = (0, ChunkPos::new(1, 2));
        let far = (0, ChunkPos::new(-5, 7));
        let other = (1, ChunkPos::new(1, 2));
        for key in [near, far, other] {
            extras.chunk_loaded(key, ChunkExtras::default());
        }
        let added = random_compound(&mut Rng(0xe7_0003), 3);
        extras
            .get_mut(far, ChunkKind::Entities)
            .unwrap()
            .push(added.clone());
        extras
            .get_mut(other, ChunkKind::BlockEntities)
            .unwrap()
            .push(added.clone());
        assert_eq!(
            extras.take_dirty(0),
            [(far.1, ChunkKind::Entities, vec![added.clone()])]
        );
        assert!(extras.take_dirty(0).is_empty(), "the marks were cleared");
        // A changed chunk that unloads takes its marks along, and coming back brings them again.
        let held = extras.chunk_unloaded(other).unwrap();
        assert!(held.is_dirty());
        assert!(extras.take_dirty(1).is_empty());
        extras.chunk_restored(other, held);
        assert_eq!(
            extras.take_dirty(1),
            [(other.1, ChunkKind::BlockEntities, vec![added])]
        );
    }

    #[test]
    fn changed_extras_of_an_unloaded_chunk_wait_in_memory_until_their_commit() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut driver = Driver::new();
        let far = [150.5, -58.38, 0.5];
        let home = [0.5, -58.38, 0.5];
        driver.join(&mut state, vec![radius(1, 1, 1)]);
        teleported(&mut state, 1, far);
        driver.tick(&mut state, Vec::new());
        driver.settle(&mut state);
        let key = (0, ChunkPos::new(9, 0));
        let mut rng = Rng(0xe7_0001);
        let added = random_compound(&mut rng, 4);
        let list = state
            .chunks
            .extras
            .get_mut(key, ChunkKind::Entities)
            .unwrap();
        list.push(added.clone());
        let changed = list.clone();

        // The view leaves before any save: terrain and extras wait in memory, extras dirty.
        teleported(&mut state, 1, home);
        driver.tick(&mut state, Vec::new());
        assert!(!state.worlds[0].is_chunk_loaded(key.1));
        let unsaved = &state.chunks.unsaved[&key];
        assert!(unsaved.snapshot.is_none() && unsaved.extras.is_dirty());
        assert!(!unsaved.chunk.is_dirty(), "the terrain did not change");
        // Coming back takes them from memory, still dirty, without a load.
        teleported(&mut state, 1, far);
        driver.tick(&mut state, Vec::new());
        assert!(!state.chunks.requested.contains(&key));
        let held = &state.chunks.extras.chunks[&key];
        assert!(held.is_dirty());
        assert_eq!(held.extras.get(ChunkKind::Entities), changed.as_slice());
        teleported(&mut state, 1, home);
        driver.tick(&mut state, Vec::new());

        // A snapshot takes the changed list alone, byte for byte; the terrain is not written.
        state.saving.requested = true;
        driver.tick(&mut state, Vec::new());
        let snapshot = driver.commits.pop().unwrap();
        let start = snapshot.start;
        assert!(snapshot.chunks.iter().all(|c| (c.0, c.1) != key));
        let taken: Vec<_> = snapshot
            .extras
            .iter()
            .filter(|e| (e.0, e.1) == key)
            .collect();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].2, ChunkKind::Entities);
        let bytes = |list: &[Compound]| {
            let mut w = crate::protocol::Writer::new(Vec::new(), 0);
            for c in list {
                w.nbt(c, crate::nbt::Encoding::LittleEndian).unwrap();
            }
            w.into_inner()
        };
        assert_eq!(bytes(&taken[0].3), bytes(&changed));
        assert_eq!(state.chunks.unsaved[&key].snapshot, Some(start));

        // Held again and released again before the commit: still in memory, clean.
        teleported(&mut state, 1, far);
        driver.tick(&mut state, Vec::new());
        assert!(!state.chunks.requested.contains(&key));
        assert!(!state.chunks.extras.chunks[&key].is_dirty());
        teleported(&mut state, 1, home);
        driver.tick(&mut state, Vec::new());
        assert_eq!(state.chunks.unsaved[&key].snapshot, Some(start));
        assert_eq!(
            state.chunks.unsaved[&key]
                .extras
                .extras
                .get(ChunkKind::Entities),
            changed.as_slice()
        );

        // The commit lets go of the memory; the next hold loads from the file.
        driver.answers.push(Response::Committed { start });
        driver.tick(&mut state, Vec::new());
        assert!(!state.chunks.unsaved.contains_key(&key));
        teleported(&mut state, 1, far);
        driver.tick(&mut state, Vec::new());
        assert!(state.chunks.requested.contains(&key));
    }

    #[test]
    fn a_player_extra_round_trips_untouched_through_join_leave_and_save() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        state.saving.players = true;
        let mut driver = Driver::new();
        let mut rng = Rng(0xe7_0002);
        let extra = random_compound(&mut rng, 6);
        let data = PlayerData {
            world: "world".to_owned(),
            position: [3.5, -58.38, 3.5],
            extra: extra.clone(),
        };
        driver.join(&mut state, Vec::new());
        for a in &mut driver.answers {
            if let Response::Player { data: load, .. } = a {
                *load = PlayerLoad::Found(data.clone());
            }
        }
        driver.tick(&mut state, Vec::new());
        let player = state.players.values().next().unwrap();
        assert_eq!(player.phase, Phase::Joined);
        assert_eq!(player.extra.raw(), &extra);
        // A move makes the data dirty; the snapshot carries the extra as it came.
        teleported(&mut state, 1, [4.5, -58.38, 3.5]);
        driver.tick(&mut state, Vec::new());
        state.saving.requested = true;
        driver.tick(&mut state, Vec::new());
        let snapshot = driver.commits.pop().unwrap();
        assert_eq!(snapshot.players.len(), 1);
        // The rule modules add their own keys (survival: health, food…); the rest is as it came.
        let kept = |saved: &Compound| extra.iter().all(|(k, v)| saved.get(k) == Some(v));
        assert!(kept(&snapshot.players[0].1.extra));
        assert_eq!(
            snapshot.players[0].1.position,
            [4.5, f64::from(-58.38_f32), 3.5]
        );
        // Leaving before the commit keeps the data, extra included, in memory.
        driver.tick(&mut state, vec![leave(1, 2)]);
        let offline = state.saving.offline.values().next().unwrap();
        assert!(kept(&offline.data.extra));
    }

    #[test]
    fn a_saved_position_outside_the_world_border_puts_the_player_at_the_spawn() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        state.saving.players = true;
        let mut driver = Driver::new();
        driver.join(&mut state, Vec::new());
        for a in &mut driver.answers {
            if let Response::Player { data: load, .. } = a {
                *load = PlayerLoad::Found(PlayerData {
                    world: "world".to_owned(),
                    position: [1.0e300, 0.0, 0.0],
                    extra: Compound::new(),
                });
            }
        }
        driver.tick(&mut state, Vec::new());
        let player = state.players.values().next().unwrap();
        assert_eq!(player.position, crate::math::Vec3::new(0.5, -58.38, 0.5));
    }
}
