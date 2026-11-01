//! The server's worlds while it runs (PMMP `WorldManager`, `World::stopTime`,
//! `World::setDifficulty`): making, loading, unloading and deleting worlds, the default world, the
//! time stop and each world's difficulty.
//!
//! Every world keeps its index, its [`world::Id`], for the run: an unloaded world stays in the
//! state's list without chunks, holding only its settings, and loads again in the same place; a
//! temporary world unloaded or a persistent world deleted, made again under its name, takes the
//! place too. A run names at most [`MAX_WORLDS`] worlds, as many world files as `data/worlds/`
//! may hold.
//!
//! Every persistent world is its file `data/worlds/<name>.redb`. Names that differ only in letter
//! case are one world: every call here finds a world by its name in any letter case, and the world
//! and its file keep the letter case of the name it was made with. Loading a world reads nothing
//! on the tick: the records of the world files are read when the server starts and live here
//! after (only the tick writes records, so memory is never older than the files), and the chunks
//! load through the save thread as always. Unloading waits for the view phase, where
//! [`WorldUnload`] may cancel it: the world's players go to the default world's spawn (PMMP), its
//! chunks unload without [`ChunkUnload`](crate::event::ChunkUnload) (their changes wait in memory
//! for the commit the unload asks for, so the server file gets them atomically with the rest of
//! the state), and the world forgets everything but its settings. The save thread learns of new
//! worlds before the first load or commit that names them.

use std::collections::BTreeMap;
use std::sync::Arc;

use tracing::info;

use crate::config::Difficulty;
use crate::event::{WorldInit, WorldLoad, WorldUnload};
use crate::protocol::packet::{Packet, SetDifficulty};
use crate::save::{self, DeletedWorld, NewWorld, WorldIndex};
use crate::storage::codec::WorldRecord;
use crate::storage::{MAX_WORLDS, world_name_key};
use crate::tick::{Context, Phase, Settings, State, dispatch, entity, player, survival, view};
use crate::world::{self, ChunkPos, Generators, Kind, World};

/// What the tick knows of its worlds besides the [`World`]s themselves.
#[derive(Debug, Default)]
pub(in crate::tick) struct Catalog {
    /// Whether each world is loaded, by index.
    loaded: Vec<bool>,
    /// Each world's index by [`world_name_key`] of its name, loaded or not: derived from the
    /// worlds (a place keeps its name), so neither saved nor hashed.
    names: BTreeMap<String, WorldIndex>,
    /// The persistent worlds the server file holds that the server has not loaded, by
    /// [`world_name_key`] of their name: the name of their file and their record.
    stored: BTreeMap<String, (String, WorldRecord)>,
    /// The world files the server file held at start that are no world of the run
    /// ([`save::Catalog::left_out`]): they count toward [`MAX_WORLDS`].
    left_out: usize,
    /// The `chunks` key number of the next new persistent world.
    next_number: u32,
    /// The world players join.
    default: WorldIndex,
    /// Worlds made or loaded the save thread has not heard of, in order.
    pending: Vec<WorldIndex>,
    /// Worlds made (`true`) or loaded since the view phase last raised their events, in order.
    announced: Vec<(WorldIndex, bool)>,
    /// Unloads asked for, for the next view phase.
    unloads: BTreeMap<WorldIndex, Unload>,
    /// Worlds deleted the save thread has not heard of, in order.
    deleted: Vec<DeletedWorld>,
    /// The generators, fixed at build.
    generators: Generators,
}

impl Catalog {
    /// The catalog of a state made with `worlds`, all loaded, and the build's `generators`; the
    /// server file's part comes with [`State::open_worlds`].
    pub(in crate::tick) fn new(worlds: &[World], generators: Generators) -> Self {
        Self {
            loaded: vec![true; worlds.len()],
            names: (0..)
                .zip(worlds)
                .map(|(i, w)| (world_name_key(w.name()), i))
                .collect(),
            generators,
            ..Self::default()
        }
    }

    /// The index of the world named `name`, in any letter case, the server has had this run,
    /// loaded or not.
    pub(in crate::tick) fn named(&self, name: &str) -> Option<WorldIndex> {
        self.names.get(&world_name_key(name)).copied()
    }

    /// Whether the world at `index` is loaded.
    pub(in crate::tick) fn is_loaded(&self, index: WorldIndex) -> bool {
        self.loaded.get(index).copied().unwrap_or(false)
    }

    /// The world players join.
    pub(in crate::tick) fn default_world(&self) -> WorldIndex {
        self.default
    }

    /// The list as a checkpoint keeps it.
    pub(in crate::tick) fn slots(&self, worlds: &[World]) -> Vec<WorldSlot> {
        self.loaded
            .iter()
            .zip(worlds)
            .map(|(&loaded, w)| WorldSlot {
                name: w.name().to_owned(),
                kind: w.kind(),
                generator: w.generator().to_owned(),
                loaded,
            })
            .collect()
    }

    /// Writes what lasts between ticks besides the list ([`Self::slots`]); the generators are
    /// fixed at build.
    pub(in crate::tick) fn checkpoint(&self, w: &mut crate::tick::checkpoint::Writer) {
        use crate::tick::checkpoint::Codec;
        let Self {
            loaded: _,
            names: _,
            stored,
            left_out,
            next_number,
            default,
            pending,
            announced,
            unloads,
            deleted,
            generators: _,
        } = self;
        stored.encode(w);
        left_out.encode(w);
        next_number.encode(w);
        default.encode(w);
        pending.encode(w);
        announced.encode(w);
        unloads.encode(w);
        deleted.encode(w);
    }

    #[cfg(test)]
    pub(in crate::tick) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            loaded,
            names: _,
            stored,
            left_out,
            next_number,
            default,
            pending,
            announced,
            unloads,
            deleted,
            generators: _,
        } = self;
        h.usize(loaded.len());
        for &l in loaded {
            h.u64(u64::from(l));
        }
        h.usize(stored.len());
        for (name, record) in stored.values() {
            h.bytes(name.as_bytes());
            h.bytes(format!("{record:?}").as_bytes());
        }
        h.usize(*left_out);
        h.u64(u64::from(*next_number));
        h.usize(*default);
        h.usize(pending.len());
        for &i in pending {
            h.usize(i);
        }
        h.usize(announced.len());
        for &(i, made) in announced {
            h.usize(i);
            h.u64(u64::from(made));
        }
        h.usize(unloads.len());
        for (&i, &how) in unloads {
            h.usize(i);
            h.u64(u64::from(how == Unload::Forced));
        }
        h.usize(deleted.len());
        for d in deleted {
            h.bytes(d.name.as_bytes());
            h.u64(
                d.index
                    .map_or(0, |i| u64::try_from(i).unwrap_or(u64::MAX).wrapping_add(1)),
            );
        }
    }

    /// Reads what [`Self::checkpoint`] wrote, with the list `slots` the checkpoint kept.
    pub(in crate::tick) fn restore(
        &mut self,
        r: &mut crate::tick::checkpoint::Reader<'_>,
        slots: &[WorldSlot],
    ) -> crate::tick::checkpoint::Result<()> {
        use crate::tick::checkpoint::Codec;
        self.loaded = slots.iter().map(|s| s.loaded).collect();
        self.names = (0..)
            .zip(slots)
            .map(|(i, s)| (world_name_key(&s.name), i))
            .collect();
        self.stored = Codec::decode(r)?;
        self.left_out = Codec::decode(r)?;
        self.next_number = Codec::decode(r)?;
        self.default = Codec::decode(r)?;
        self.pending = Codec::decode(r)?;
        self.announced = Codec::decode(r)?;
        self.unloads = Codec::decode(r)?;
        self.deleted = Codec::decode(r)?;
        if !self.is_loaded(self.default) {
            return Err(crate::tick::checkpoint::Error::Format("default world"));
        }
        Ok(())
    }

    /// The slot of the world at `index` in [`Self::loaded`], to mark it loaded or not.
    fn loaded_mut(&mut self, index: WorldIndex) -> &mut bool {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: every world the server had this run has a slot"
        )]
        &mut self.loaded[index]
    }
}

/// How an unload was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unload {
    /// [`WorldUnload`] listeners may cancel it.
    Asked,
    /// It happens whatever the listeners say.
    Forced,
}

crate::tick::checkpoint::variants!(Unload {
    0 => Asked,
    1 => Forced,
});
crate::tick::checkpoint::variants!(Kind {
    0 => Persistent,
    1 => Temporary,
});
crate::tick::checkpoint::fields!(WorldRecord {
    number,
    time,
    spawn
});
crate::tick::checkpoint::fields!(DeletedWorld { name, index });

/// A world of the list as a checkpoint keeps it, before the state that holds it is made: a
/// replay makes the same worlds from it ([`crate::replay`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorldSlot {
    pub(crate) name: String,
    pub(crate) kind: Kind,
    pub(crate) generator: String,
    loaded: bool,
}

crate::tick::checkpoint::fields!(WorldSlot {
    name,
    kind,
    generator,
    loaded
});

impl State {
    /// Gives the state what the server file says of the worlds it does not start with: the
    /// persistent worlds it may load, the world files it left out, and the next world number.
    pub(crate) fn open_worlds(&mut self, catalog: save::Catalog) {
        let save::Catalog {
            stored,
            next_number,
            left_out,
        } = catalog;
        self.catalog.stored = stored
            .into_iter()
            .map(|(name, record)| (world_name_key(&name), (name, record)))
            .collect();
        self.catalog.next_number = next_number;
        self.catalog.left_out = left_out;
    }
}

#[cfg(test)]
impl State {
    /// Adds `world`, loaded, holding nothing, with the global view values (tests that need a
    /// second world and no events).
    pub(in crate::tick) fn add_world(&mut self, mut world: World) {
        let index = self.worlds.len();
        world.set_id(world::Id::of(index));
        self.catalog
            .names
            .insert(world_name_key(world.name()), index);
        self.worlds.push(world);
        self.catalog.loaded.push(true);
        self.settings.world_views.push(self.settings.view);
    }
}

impl Context<'_> {
    /// Makes a world named `name` with `options` and loads it (PMMP
    /// `WorldManager::generateWorld`): its spawn chunks load from the next view phase on, where
    /// [`WorldInit`] and [`WorldLoad`] are raised. A persistent world is saved from the next
    /// commit on; a temporary one never. The generator is the first one registered under the
    /// options' generator name. A seed the options leave out is made from the name, and a spawn
    /// they leave out is the generator's. The server file keeps the world's chunks, time and
    /// spawn, never its options: [`load_world`](Self::load_world) after a restart takes them
    /// again. A world made again under the name of a temporary world unloaded or of a persistent
    /// world deleted this run takes its [`world::Id`].
    ///
    /// A persistent world is its file `data/worlds/<name>.redb`, made by the save thread.
    ///
    /// Fails, making nothing, with [`world::Error::InvalidName`] for a name that cannot be a file
    /// name (the rule is there), [`WorldExists`](world::Error::WorldExists) if the server runs a
    /// world of that name or `data/worlds/` held one it could read at start
    /// ([`is_world_generated`](Self::is_world_generated)), also in another letter case,
    /// [`UnknownGenerator`](world::Error::UnknownGenerator) for a generator not registered,
    /// [`InvalidSettings`](world::Error::InvalidSettings) for a view value of
    /// [`Options::settings`](world::Options::settings) out of its range,
    /// [`OutOfRange`](world::Error::OutOfRange) for a spawn (the options' or the generator's)
    /// where a player would stand outside the world border, and [`Full`](world::Error::Full) past
    /// the most worlds a run names, or, for a persistent world, past as many world files: those of
    /// the run's persistent worlds and those `data/worlds/` held at start, damaged ones too. A
    /// file put there while the server runs (an import) is not counted: past the most, the save
    /// thread makes no file for the world, which then runs with its chunks held as damaged and is
    /// not saved.
    ///
    /// The world keeps the difficulty of [`Options::difficulty`](world::Options::difficulty) as
    /// its own, but a `hardcore` server plays every world on hard.
    #[doc(alias = "WorldManager::generateWorld")]
    pub fn create_world(
        &mut self,
        name: &str,
        options: &world::Options,
    ) -> world::Result<world::Id> {
        let state = &mut *self.state;
        if !crate::storage::is_world_file_name(name) {
            return Err(world::Error::InvalidName);
        }
        check_view(options)?;
        let place = named(state, name);
        if name_taken(state, name) {
            return Err(world::Error::WorldExists);
        }
        let generator = generator_of(state, name, options)?;
        let persistent = options.kind() == Kind::Persistent;
        if (place.is_none() && state.worlds.len() >= MAX_WORLDS)
            || (persistent && persistent_count(state) >= MAX_WORLDS)
        {
            return Err(world::Error::Full);
        }
        let mut world = make_world(name, options.kind(), generator.as_ref(), options);
        if !entity::is_valid_position(survival::standing_eye(world.spawn())) {
            return Err(world::Error::OutOfRange);
        }
        let number = persistent.then(|| {
            let n = state.catalog.next_number;
            state.catalog.next_number = n.saturating_add(1);
            n
        });
        world.set_number(number);
        info!(world = name, seed = world.seed(), "world made");
        let index = place.unwrap_or(state.worlds.len());
        Ok(install(state, index, world, options, true))
    }

    /// Loads the persistent world `name` whose file `data/worlds/<name>.redb` was there when the
    /// server started, declared or not (PMMP `WorldManager::loadWorld`), with the settings of
    /// `options`, as [`create_world`](Self::create_world) makes one: the file keeps its chunks,
    /// time and spawn, never its settings, so a world stored in an earlier run gets its generator,
    /// seed, difficulty and view values from here (its kind is persistent whatever the options
    /// say, and their spawn is not used). It keeps the name of its file, whatever the letter case
    /// of `name`. Its spawn chunks load from the next view phase on, where [`WorldLoad`] is
    /// raised. A world the server already has this run keeps the settings it has and the options
    /// are not used: a loaded one is its `Id` as it is, and an unload asked for it before the next
    /// view phase is dropped (the later call wins; no [`WorldUnload`] is raised for it). Inside a
    /// [`WorldUnload`] listener, cancelling the event is what keeps the world.
    ///
    /// Fails with [`world::Error::NotGenerated`] if the server runs no world of that name and
    /// `data/worlds/` held none it could read at start (a temporary world unloaded is gone), and,
    /// for a stored world, with [`UnknownGenerator`](world::Error::UnknownGenerator) for a
    /// generator not registered, [`InvalidSettings`](world::Error::InvalidSettings) for a view
    /// value out of its range and [`Full`](world::Error::Full) past the most worlds a run names.
    #[doc(alias = "WorldManager::loadWorld")]
    pub fn load_world(&mut self, name: &str, options: &world::Options) -> world::Result<world::Id> {
        let state = &mut *self.state;
        if let Some(i) = named(state, name) {
            if state.catalog.is_loaded(i) {
                state.catalog.unloads.remove(&i);
                return Ok(world::Id::of(i));
            }
            if super::world_at(state, i).kind() == Kind::Persistent {
                return Ok(reload(state, i));
            }
            return Err(world::Error::NotGenerated);
        }
        let key = world_name_key(name);
        let Some((file_name, record)) = state.catalog.stored.get(&key) else {
            return Err(world::Error::NotGenerated);
        };
        check_view(options)?;
        let generator = generator_of(state, name, options)?;
        if state.worlds.len() >= MAX_WORLDS {
            return Err(world::Error::Full);
        }
        let mut world = make_world(file_name, Kind::Persistent, generator.as_ref(), options);
        save::resume(&mut world, *record);
        state.catalog.stored.remove(&key);
        let index = state.worlds.len();
        Ok(install(state, index, world, options, false))
    }

    /// Unloads `world` in the next view phase, after [`WorldUnload`] listeners, who may cancel
    /// it (PMMP `WorldManager::unloadWorld`): its players go to the default world's spawn, its
    /// chunks unload, and the commit that follows saves what changed. The unload is dropped, with
    /// no event, if the world is the default one by then. Its [`world::Id`] stays its own:
    /// [`load_world`](Self::load_world) loads a persistent world in the same place, and its file
    /// stays open until the server stops.
    ///
    /// Fails with [`world::Error::WorldNotLoaded`] for a world that is not loaded and with
    /// [`DefaultWorld`](world::Error::DefaultWorld) for the default world (PMMP refuses it
    /// unless forced, then leaves the server without one; Anchor always has one).
    #[doc(alias = "WorldManager::unloadWorld")]
    pub fn unload_world(&mut self, world: world::Id) -> world::Result<()> {
        self.ask_unload(world, Unload::Asked)
    }

    /// [`unload_world`](Self::unload_world) that a cancelled [`WorldUnload`] does not stop (PMMP
    /// `unloadWorld` with `forceUnload`): its monitors get the event not cancelled. A listener
    /// that makes the world the default one still keeps it.
    #[doc(alias = "WorldManager::unloadWorld")]
    pub fn force_unload_world(&mut self, world: world::Id) -> world::Result<()> {
        self.ask_unload(world, Unload::Forced)
    }

    /// Whether a world named `name` exists: the server runs it, or its file was in
    /// `data/worlds/` at start and could be read (PMMP `WorldManager::isWorldGenerated`). A
    /// temporary world unloaded does not.
    #[doc(alias = "WorldManager::isWorldGenerated")]
    pub fn is_world_generated(&self, name: &str) -> bool {
        name_taken(self.state, name)
    }

    /// Deletes the persistent world `name` and its file `data/worlds/<name>.redb`, and nothing
    /// else: other worlds and the players' data stay as they are (a player saved in it joins the
    /// default world). PMMP has no such call; it is the deletion of a world's folder. The world
    /// must not be loaded: [`unload_world`](Self::unload_world) first, which saves it, and delete
    /// it after the view phase that unloads it (from a callback a [`WorldUnload`] listener
    /// defers, or later); its changes since are dropped. The save thread removes the file after
    /// the saves already asked for; a world of the name may be made at once. The file's backups
    /// in `data/backups/worlds/<name>/` are kept.
    ///
    /// Fails with [`world::Error::WorldLoaded`] for a loaded world (one whose unload is asked for
    /// but has not happened too), and with [`NotGenerated`](world::Error::NotGenerated) if no
    /// persistent world of that name exists ([`is_world_generated`](Self::is_world_generated)).
    pub fn delete_world(&mut self, name: &str) -> world::Result<()> {
        let state = &mut *self.state;
        let index = match named(state, name) {
            Some(i) if state.catalog.is_loaded(i) => return Err(world::Error::WorldLoaded),
            Some(i) if super::world_at(state, i).kind() == Kind::Persistent => Some(i),
            _ if state.catalog.stored.remove(&world_name_key(name)).is_some() => None,
            _ => return Err(world::Error::NotGenerated),
        };
        if let Some(i) = index {
            // Its chunks waiting in memory are its file's: none may reach a world made here.
            let all =
                (i, ChunkPos::new(i32::MIN, i32::MIN))..=(i, ChunkPos::new(i32::MAX, i32::MAX));
            let unsaved: Vec<_> = state
                .chunks
                .unsaved
                .range(all)
                .map(|(&key, _)| key)
                .collect();
            for key in unsaved {
                state.chunks.unsaved.remove(&key);
            }
            super::world_at_mut(state, i).forget();
        }
        state.catalog.deleted.push(DeletedWorld {
            name: name.to_owned(),
            index,
        });
        info!(world = name, "world deleted");
        Ok(())
    }

    /// Whether the world named `name` is loaded (PMMP `WorldManager::isWorldLoaded`,
    /// `World::isLoaded`).
    #[doc(alias = "WorldManager::isWorldLoaded")]
    #[doc(alias = "World::isLoaded")]
    pub fn is_world_loaded(&self, name: &str) -> bool {
        named(self.state, name).is_some_and(|i| self.state.catalog.is_loaded(i))
    }

    /// Makes `world` the default world, where players without a saved place join and dead
    /// players without a spawn point respawn, until the server stops (PMMP
    /// `WorldManager::setDefaultWorld`; `level-name` stays as it is). Fails with
    /// [`world::Error::WorldNotLoaded`] for a world that is not loaded.
    #[doc(alias = "WorldManager::setDefaultWorld")]
    pub fn set_default_world(&mut self, world: world::Id) -> world::Result<()> {
        let index = super::index(self.state, world).ok_or(world::Error::WorldNotLoaded)?;
        self.state.catalog.default = index;
        Ok(())
    }

    /// Stops the time of `world`: its day cycle and sun stay where they are, for its players too,
    /// until [`start_time`](Self::start_time) or the server stops: it is not saved, as in PMMP.
    /// False for a world that is not loaded.
    #[doc(alias = "World::stopTime")]
    pub fn stop_time(&mut self, world: world::Id) -> bool {
        self.set_time_stopped(world, true)
    }

    /// Starts the time of `world` again after [`stop_time`](Self::stop_time). False for a world
    /// that is not loaded.
    #[doc(alias = "World::startTime")]
    pub fn start_time(&mut self, world: world::Id) -> bool {
        self.set_time_stopped(world, false)
    }

    /// Gives `world` its own difficulty, or `None` to follow the server's
    /// ([`difficulty`](Self::difficulty)), and tells its players (PMMP `World::setDifficulty`). Its
    /// rules (hunger, fire) use it until the server stops: it is not saved, and a restart takes the
    /// difficulty of the settings again. False, changing nothing, for a world that is not loaded,
    /// or a difficulty other than hard on a `hardcore` server.
    #[doc(alias = "World::setDifficulty")]
    pub fn set_world_difficulty(
        &mut self,
        world: world::Id,
        difficulty: Option<Difficulty>,
    ) -> bool {
        let Some(w) = super::index(self.state, world) else {
            return false;
        };
        let settings = &self.state.settings;
        if settings.hardcore && difficulty.unwrap_or(settings.difficulty) != Difficulty::Hard {
            return false;
        }
        super::world_at_mut(self.state, w).set_difficulty(difficulty);
        let applied = difficulty_of(&self.state.settings, super::world_at(self.state, w));
        let packet = Packet::SetDifficulty(SetDifficulty {
            difficulty: crate::tick::spawn::difficulty_id(applied).cast_unsigned(),
        });
        super::send_to_world(self.state, w, &packet);
        true
    }

    /// Asks for the unload of `world` in the next view phase; a forced ask overrides an earlier
    /// one that was not.
    fn ask_unload(&mut self, world: world::Id, how: Unload) -> world::Result<()> {
        let index = super::index(self.state, world).ok_or(world::Error::WorldNotLoaded)?;
        if index == self.state.catalog.default {
            return Err(world::Error::DefaultWorld);
        }
        let asked = self.state.catalog.unloads.entry(index).or_insert(how);
        if how == Unload::Forced {
            *asked = how;
        }
        Ok(())
    }

    /// Stops or starts the time and tells the world's players (PMMP `World::sendTime`).
    fn set_time_stopped(&mut self, world: world::Id, stopped: bool) -> bool {
        let Some(w) = super::index(self.state, world) else {
            return false;
        };
        super::world_at_mut(self.state, w).set_time_stopped(stopped);
        let packet = survival::clock_state(self.state, w);
        super::send_to_world(self.state, w, &packet);
        true
    }
}

/// The difficulty the rules of the world at `world` use ([`difficulty_of`]).
pub(in crate::tick) fn difficulty(state: &State, world: WorldIndex) -> Difficulty {
    difficulty_of(&state.settings, super::world_at(state, world))
}

/// The difficulty the rules of `world` use: hard on a `hardcore` server (of `settings`),
/// whatever the world's own says (a world made or loaded with another keeps it for a server that
/// is not hardcore); otherwise its own, or the server's.
pub(in crate::tick) fn difficulty_of(settings: &Settings, world: &World) -> Difficulty {
    if settings.hardcore {
        Difficulty::Hard
    } else {
        world.difficulty().unwrap_or(settings.difficulty)
    }
}

/// The start of the view phase: the events of the worlds made and loaded since the last one
/// ([`WorldInit`], then [`WorldLoad`]), then the unloads asked for, each after [`WorldUnload`]
/// listeners; its monitors see only an unload that happened. The save thread hears of new worlds
/// later in the phase, with the worlds these listeners make.
pub(in crate::tick) fn step(state: &mut State) {
    for (index, made) in std::mem::take(&mut state.catalog.announced) {
        if !state.catalog.is_loaded(index) {
            continue;
        }
        let w = super::world_at(state, index);
        info!(world = w.name(), kind = ?w.kind(), "world loaded");
        let world = world::Id::of(index);
        if made && dispatch::heard::<WorldInit>(state) {
            let mut e = WorldInit { world };
            dispatch::call(state, &mut e);
            dispatch::watch(state, &e);
        }
        if dispatch::heard::<WorldLoad>(state) {
            let mut e = WorldLoad { world };
            dispatch::call(state, &mut e);
            dispatch::watch(state, &e);
        }
    }
    for (index, how) in std::mem::take(&mut state.catalog.unloads) {
        if !state.catalog.is_loaded(index) || index == state.catalog.default {
            continue;
        }
        let mut event = None;
        if dispatch::heard::<WorldUnload>(state) {
            let e = event.insert(WorldUnload {
                world: world::Id::of(index),
                cancelled: false,
            });
            dispatch::call(state, e);
            // A listener that made the world the default one cancels the unload, forced or not:
            // the default world never unloads.
            let made_default =
                dispatch::listened::<WorldUnload>(state) && index == state.catalog.default;
            if (e.cancelled && how == Unload::Asked) || made_default {
                continue;
            }
            e.cancelled = false;
        }
        unload(state, index);
        if let Some(e) = &event {
            dispatch::watch(state, e);
        }
    }
}

/// What the save thread must hear of worlds it does not know, taken: for the next request or the
/// next snapshot, whichever comes first.
pub(in crate::tick) fn take_new_worlds(state: &mut State) -> Vec<NewWorld> {
    let pending = std::mem::take(&mut state.catalog.pending);
    pending
        .into_iter()
        .map(|index| {
            let world = super::world_at(state, index);
            #[expect(
                clippy::expect_used,
                reason = "invariant: every world's generator is a registered one"
            )]
            let generator = state
                .catalog
                .generators
                .of_world(world.name(), world.generator())
                .expect("a world's generator is registered");
            NewWorld {
                index,
                name: world.name().to_owned(),
                number: world.number(),
                seed: world.seed(),
                generator: Arc::clone(generator),
            }
        })
        .collect()
}

/// The worlds deleted that the save thread must hear of, taken: for the next request or the next
/// snapshot, whichever comes first, and before the new worlds taken with them (one may take a
/// deleted world's name).
pub(in crate::tick) fn take_deleted_worlds(state: &mut State) -> Vec<DeletedWorld> {
    std::mem::take(&mut state.catalog.deleted)
}

/// Checks the view values of `options` against the ranges of their `[worlds.<name>]` keys.
fn check_view(options: &world::Options) -> world::Result<()> {
    if options.settings().check("worlds").is_err() {
        return Err(world::Error::InvalidSettings);
    }
    Ok(())
}

/// Whether a world of the run (persistent, or loaded) or a stored one has a name that differs
/// from `name` at most in letter case: its file would be the same.
fn name_taken(state: &State, name: &str) -> bool {
    named(state, name).is_some_and(|i| {
        state.catalog.is_loaded(i) || super::world_at(state, i).kind() == Kind::Persistent
    }) || state.catalog.stored.contains_key(&world_name_key(name))
}

/// The index of the world named `name` the server has had this run, loaded or not.
fn named(state: &State, name: &str) -> Option<WorldIndex> {
    state.catalog.named(name)
}

/// The registered generator the world `name` gets from the generator name `options` give
/// ([`world::Generators::of_world`]).
fn generator_of<'a>(
    state: &'a State,
    name: &str,
    options: &world::Options,
) -> world::Result<&'a Arc<dyn world::Generator>> {
    state
        .catalog
        .generators
        .of_world(name, options.generator())
        .ok_or(world::Error::UnknownGenerator)
}

/// The world files the server has or makes: of its persistent worlds, the stored ones and those
/// it left out at start.
fn persistent_count(state: &State) -> usize {
    let had = state
        .worlds
        .iter()
        .filter(|w| w.kind() == Kind::Persistent)
        .count();
    had.saturating_add(state.catalog.stored.len())
        .saturating_add(state.catalog.left_out)
}

/// The world `name` of `kind` that `options` make with `generator`, before it has a place.
fn make_world(
    name: &str,
    kind: Kind,
    generator: &dyn world::Generator,
    options: &world::Options,
) -> World {
    let mut world = save::new_world(name, kind, generator, options.seed(), options.spawn());
    world.set_generator(generator.name());
    world.set_difficulty(options.difficulty());
    world
}

/// Puts `world` at `index` (the next one, or a world's place it takes again), loaded, with the
/// view values of `options`; the save thread hears of it before its first load, and the view
/// phase raises its events. Holds its spawn chunks.
fn install(
    state: &mut State,
    index: WorldIndex,
    mut world: World,
    options: &world::Options,
    made: bool,
) -> world::Id {
    let id = world::Id::of(index);
    world.set_id(id);
    world.set_lighting(state.settings.light_mode());
    world.set_custom(Arc::clone(&state.custom));
    set_view(state, index, options);
    if let Some(place) = state.worlds.get_mut(index) {
        *place = world;
        *state.catalog.loaded_mut(index) = true;
    } else {
        // Worlds are numbered in order, so a new one is the next.
        state
            .catalog
            .names
            .insert(world_name_key(world.name()), index);
        state.worlds.push(world);
        state.catalog.loaded.push(true);
    }
    state.catalog.pending.push(index);
    state.catalog.announced.push((index, made));
    view::hold_world(state, index);
    id
}

/// Gives the world at `index` the view values of `options`: each one set replaces the global one.
fn set_view(state: &mut State, index: WorldIndex, options: &world::Options) {
    let settings = &mut state.settings;
    let view = settings.global_view.apply(&options.settings());
    match settings.world_views.get_mut(index) {
        Some(slot) => *slot = view,
        // Every world has view values, and a new world is the next one.
        None => settings.world_views.push(view),
    }
}

/// Loads again the world at `index`, unloaded: the save thread still has it.
fn reload(state: &mut State, index: WorldIndex) -> world::Id {
    *state.catalog.loaded_mut(index) = true;
    state.catalog.announced.push((index, false));
    view::hold_world(state, index);
    world::Id::of(index)
}

/// Unloads the world at `index`, not the default one: its players go to the default world's
/// safe spawn (PMMP `WorldManager::unloadWorld`), its chunks unload and the next tick boundary
/// commits what changed.
fn unload(state: &mut State, index: WorldIndex) {
    let default = state.catalog.default;
    let home = super::world_at(state, default);
    let eye = survival::standing_eye(home.safe_spawn(home.spawn()));
    let players: Vec<_> = state
        .players
        .iter()
        .filter(|(_, p)| p.world == index && p.phase != Phase::Loading)
        .map(|(&s, _)| s)
        .collect();
    for session in players {
        survival::teleport(state, session, default, eye);
    }
    player::redirect_teleports(state, index, default, eye);
    view::unload_world(state, index);
    *state.catalog.loaded_mut(index) = false;
    let world = super::world_at_mut(state, index);
    world.clear();
    info!(world = world.name(), kind = ?world.kind(), "world unloaded");
    if world.kind() == Kind::Persistent {
        state.saving.requested = true;
    }
}

#[cfg(test)]
mod tests;
