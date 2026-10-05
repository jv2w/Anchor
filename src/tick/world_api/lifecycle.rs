//! The server's worlds while it runs (PLAN.md 10.9 대응표 재검토; PMMP `WorldManager`,
//! `World::stopTime`, `World::setDifficulty`): making, loading and unloading worlds, the default
//! world, the time stop and each world's difficulty.
//!
//! Every world keeps its index, its [`world::Id`], for the run: an unloaded world stays in the
//! state's list without chunks, holding only its settings, and loads again in the same place; a
//! temporary world made again under its name takes the place too. A run names at most
//! [`MAX_WORLDS`] worlds, as many as the server file holds.
//!
//! Loading a world reads nothing on the tick: the server file's records are read when it opens
//! and live here after (only the tick writes records, so memory is never older than the file),
//! and the chunks load through the save thread as always. Unloading waits for the view phase,
//! where [`WorldUnload`] may cancel it: the world's players go to the default world's spawn
//! (PMMP), its chunks unload as every chunk does (their changes wait in memory for the commit
//! the unload asks for, so the server file gets them atomically with the rest of the state), and
//! the world forgets everything but its settings. The save thread learns of new worlds before
//! the first load or commit that names them.

use std::collections::BTreeMap;
use std::sync::Arc;

use tracing::info;

use super::super::{Context, Phase, Settings, State, dispatch, survival, view};
use crate::config::Difficulty;
use crate::event::{WorldInit, WorldLoad, WorldUnload};
use crate::math::Vec3;
use crate::protocol::packet::{Packet, SetDifficulty};
use crate::save::{self, NewWorld, WorldIndex};
use crate::storage::codec::{MAX_NAME, WorldRecord};
use crate::world::{self, Generators, Kind, World};

/// The most worlds a run names, and the most persistent worlds the server file holds.
pub(in crate::tick) const MAX_WORLDS: usize = crate::storage::MAX_WORLDS;

/// What the tick knows of its worlds besides the [`World`]s themselves.
#[derive(Debug, Default)]
pub(in crate::tick) struct Catalog {
    /// Whether each world is loaded, by index.
    loaded: Vec<bool>,
    /// Each world's index by name, loaded or not: derived from the worlds (a place keeps its
    /// name), so neither saved nor hashed.
    names: BTreeMap<String, WorldIndex>,
    /// The persistent worlds the server file holds that the server has not loaded, by name.
    stored: BTreeMap<String, WorldRecord>,
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
    /// The generators, fixed at build.
    generators: Generators,
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
crate::tick::checkpoint::fields!(WorldRecord { number, time });

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

impl Catalog {
    /// The catalog of a state made with `worlds`, all loaded, and the build's `generators`; the
    /// server file's part comes with [`State::open_worlds`].
    pub(in crate::tick) fn new(worlds: &[World], generators: Generators) -> Self {
        Self {
            loaded: vec![true; worlds.len()],
            names: (0..)
                .zip(worlds)
                .map(|(i, w)| (w.name().to_owned(), i))
                .collect(),
            generators,
            ..Self::default()
        }
    }

    /// The index of the world named `name` the server has had this run, loaded or not.
    pub(in crate::tick) fn named(&self, name: &str) -> Option<WorldIndex> {
        self.names.get(name).copied()
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
            next_number,
            default,
            pending,
            announced,
            unloads,
            generators: _,
        } = self;
        stored.encode(w);
        next_number.encode(w);
        default.encode(w);
        pending.encode(w);
        announced.encode(w);
        unloads.encode(w);
    }

    /// Reads what [`Self::checkpoint`] wrote, with the list `slots` the checkpoint kept.
    pub(in crate::tick) fn restore(
        &mut self,
        r: &mut crate::tick::checkpoint::Reader<'_>,
        slots: &[WorldSlot],
    ) -> crate::tick::checkpoint::Result<()> {
        use crate::tick::checkpoint::Codec;
        self.loaded = slots.iter().map(|s| s.loaded).collect();
        self.names = (0..).zip(slots).map(|(i, s)| (s.name.clone(), i)).collect();
        self.stored = Codec::decode(r)?;
        self.next_number = Codec::decode(r)?;
        self.default = Codec::decode(r)?;
        self.pending = Codec::decode(r)?;
        self.announced = Codec::decode(r)?;
        self.unloads = Codec::decode(r)?;
        if !self.is_loaded(self.default) {
            return Err(crate::tick::checkpoint::Error::Format("default world"));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(in crate::tick) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            loaded,
            names: _,
            stored,
            next_number,
            default,
            pending,
            announced,
            unloads,
            generators: _,
        } = self;
        h.usize(loaded.len());
        for &l in loaded {
            h.u64(u64::from(l));
        }
        h.usize(stored.len());
        for (name, record) in stored {
            h.bytes(name.as_bytes());
            h.bytes(format!("{record:?}").as_bytes());
        }
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
    }
}

impl State {
    /// Gives the state what the server file says of the worlds it does not start with: the
    /// persistent worlds it may load, and the next world number.
    pub(crate) fn open_worlds(&mut self, catalog: save::Catalog) {
        let save::Catalog {
            stored,
            next_number,
        } = catalog;
        self.catalog.stored = stored.into_iter().collect();
        self.catalog.next_number = next_number;
    }
}

#[cfg(test)]
impl State {
    /// Adds `world`, loaded, holding nothing (tests that need a second world and no events).
    pub(in crate::tick) fn add_world(&mut self, mut world: World) {
        let index = self.worlds.len();
        world.set_id(world::Id::of(index));
        self.catalog.names.insert(world.name().to_owned(), index);
        self.worlds.push(world);
        self.catalog.loaded.push(true);
    }
}

/// The index of the world named `name` the server has had this run, loaded or not.
fn named(state: &State, name: &str) -> Option<WorldIndex> {
    state.catalog.named(name)
}

/// The persistent worlds the server has or the server file holds.
fn persistent_count(state: &State) -> usize {
    let had = state
        .worlds
        .iter()
        .filter(|w| w.kind() == Kind::Persistent)
        .count();
    had.saturating_add(state.catalog.stored.len())
}

/// The difficulty the rules of the world at `world` use ([`difficulty_of`]).
pub(in crate::tick) fn difficulty(state: &State, world: WorldIndex) -> Difficulty {
    state
        .worlds
        .get(world)
        .map_or(state.settings.difficulty, |w| {
            difficulty_of(&state.settings, w)
        })
}

/// The difficulty the rules of `world` use: hard on a `hardcore` server (of `settings`),
/// whatever the world's own says (a world made or saved with another keeps it for a server that
/// is not hardcore); otherwise its own, or the server's.
pub(in crate::tick) fn difficulty_of(settings: &Settings, world: &World) -> Difficulty {
    if settings.hardcore {
        Difficulty::Hard
    } else {
        world.difficulty().unwrap_or(settings.difficulty)
    }
}

/// Puts `world` at `index` (the next one, or a world's place it takes again), loaded, filled by
/// `generator`; the save thread hears of it before its first load, and the view phase raises its
/// events. Holds its spawn chunks.
fn install(
    state: &mut State,
    index: WorldIndex,
    mut world: World,
    generator: &dyn world::Generator,
    made: bool,
) -> world::Id {
    let id = world::Id::of(index);
    world.set_id(id);
    world.set_generator(generator.name());
    world.set_lighting(state.settings.light_mode());
    world.set_custom(Arc::clone(&state.custom));
    if let (Some(w), Some(loaded)) = (
        state.worlds.get_mut(index),
        state.catalog.loaded.get_mut(index),
    ) {
        *w = world;
        *loaded = true;
    } else {
        state.catalog.names.insert(world.name().to_owned(), index);
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
    if settings.world_views.len() <= index {
        settings.world_views.resize(index + 1, settings.view);
    }
    if let Some(slot) = settings.world_views.get_mut(index) {
        *slot = view;
    }
}

/// Loads again the world at `index`, unloaded: the save thread still has it.
fn reload(state: &mut State, index: WorldIndex) -> world::Id {
    if let Some(loaded) = state.catalog.loaded.get_mut(index) {
        *loaded = true;
    }
    state.catalog.announced.push((index, false));
    view::hold_world(state, index);
    world::Id::of(index)
}

/// What the save thread must hear of worlds it does not know, taken: for the next request or the
/// next snapshot, whichever comes first.
pub(in crate::tick) fn take_new_worlds(state: &mut State) -> Vec<NewWorld> {
    let pending = std::mem::take(&mut state.catalog.pending);
    pending
        .into_iter()
        .filter_map(|index| {
            let world = state.worlds.get(index)?;
            Some(NewWorld {
                index,
                name: world.name().to_owned(),
                number: world.number(),
                seed: world.seed(),
                generator: Arc::clone(state.catalog.generators.get(world.generator())?),
            })
        })
        .collect()
}

/// The start of the view phase: the events of the worlds made and loaded since the last one
/// ([`WorldInit`], then [`WorldLoad`]), then the unloads asked for, each after [`WorldUnload`].
/// The save thread hears of new worlds later in the phase, with the worlds these listeners make.
pub(in crate::tick) fn step(state: &mut State) {
    for (index, made) in std::mem::take(&mut state.catalog.announced) {
        if !state.catalog.is_loaded(index) {
            continue;
        }
        if let Some(w) = state.worlds.get(index) {
            info!(world = w.name(), kind = ?w.kind(), "world loaded");
        }
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
            if e.cancelled && how == Unload::Asked {
                dispatch::watch(state, e);
                continue;
            }
        }
        // A listener may have made the world the default one.
        if index != state.catalog.default {
            unload(state, index);
        }
        if let Some(e) = &event {
            dispatch::watch(state, e);
        }
    }
}

/// Unloads the world at `index`, not the default one: its players go to the default world's
/// safe spawn (PMMP `WorldManager::unloadWorld`), its chunks unload and the next tick boundary
/// commits what changed.
fn unload(state: &mut State, index: WorldIndex) {
    let default = state.catalog.default;
    let eye = state.worlds.get(default).map_or(Vec3::ZERO, |w| {
        let feet = w.safe_spawn(w.spawn());
        Vec3::new(
            f64::from(feet.x) + 0.5,
            f64::from(feet.y) + survival::EYE_HEIGHT,
            f64::from(feet.z) + 0.5,
        )
    });
    let players: Vec<_> = state
        .players
        .iter()
        .filter(|(_, p)| p.world == index && p.phase != Phase::Loading)
        .map(|(&s, _)| s)
        .collect();
    for session in players {
        survival::teleport(state, session, default, eye);
    }
    view::unload_world(state, index);
    if let Some(loaded) = state.catalog.loaded.get_mut(index) {
        *loaded = false;
    }
    let Some(world) = state.worlds.get_mut(index) else {
        return;
    };
    world.clear();
    if world.kind() == Kind::Persistent {
        state.saving.requested = true;
    }
    info!(world = world.name(), kind = ?world.kind(), "world unloaded");
}

impl Context<'_> {
    /// Makes a world named `name` with `options` and loads it (PMMP
    /// `WorldManager::generateWorld`): its spawn chunks load from the next view phase on, where
    /// [`WorldInit`] and [`WorldLoad`] are raised. A persistent world is saved from the next
    /// commit on; a temporary one never. A seed the options leave out is made from the name, and
    /// a spawn they leave out is the generator's. The server file keeps the world's chunks and
    /// time, never its options: [`load_world`](Self::load_world) after a restart takes them again.
    ///
    /// Fails, making nothing, with [`world::Error::InvalidName`] for an empty name or one over
    /// 256 bytes, [`WorldExists`](world::Error::WorldExists) if the server runs a world of that
    /// name or the server file holds one ([`is_world_generated`](Self::is_world_generated)),
    /// [`UnknownGenerator`](world::Error::UnknownGenerator) for a generator not registered,
    /// [`InvalidSettings`](world::Error::InvalidSettings) for a view value of
    /// [`Options::settings`](world::Options::settings) out of its range, and
    /// [`Full`](world::Error::Full) past 1,024 worlds named this run or held by the server file.
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
        if name.is_empty() || name.len() > MAX_NAME {
            return Err(world::Error::InvalidName);
        }
        if options.settings().check("worlds").is_err() {
            return Err(world::Error::InvalidSettings);
        }
        let place = named(state, name);
        if let Some(i) = place
            && (state.catalog.is_loaded(i)
                || state.worlds.get(i).map(World::kind) == Some(Kind::Persistent))
        {
            return Err(world::Error::WorldExists);
        }
        if state.catalog.stored.contains_key(name) {
            return Err(world::Error::WorldExists);
        }
        let generator = state
            .catalog
            .generators
            .get(options.generator())
            .cloned()
            .ok_or(world::Error::UnknownGenerator)?;
        let persistent = options.kind() == Kind::Persistent;
        if (place.is_none() && state.worlds.len() >= MAX_WORLDS)
            || (persistent && persistent_count(state) >= MAX_WORLDS)
        {
            return Err(world::Error::Full);
        }
        let index = place.unwrap_or(state.worlds.len());
        let number = persistent.then(|| {
            let n = state.catalog.next_number;
            state.catalog.next_number = n.saturating_add(1);
            n
        });
        let mut world = save::new_world(
            name,
            options.kind(),
            generator.as_ref(),
            options.seed(),
            options.spawn(),
        );
        world.set_difficulty(options.difficulty());
        world.set_number(number);
        info!(world = name, seed = world.seed(), "world made");
        set_view(state, index, options);
        Ok(install(state, index, world, generator.as_ref(), true))
    }

    /// Loads the persistent world `name` the server file holds (PMMP `WorldManager::loadWorld`)
    /// with the settings of `options`, as [`create_world`](Self::create_world) makes one: the
    /// server file keeps its chunks and time, never its settings (PLAN.md 10.6), so a world
    /// stored in an earlier run gets its generator, seed, spawn, difficulty and view values from
    /// here (its kind is persistent whatever the options say). Its spawn chunks load from the
    /// next view phase on, where [`WorldLoad`] is raised. A world the server already has this run
    /// keeps the settings it has and the options are not used: a loaded one is its `Id` as it is,
    /// and an unload asked for it before the next view phase is dropped (the later call wins; no
    /// [`WorldUnload`] is raised for it). Inside a [`WorldUnload`] listener, cancelling the event
    /// is what keeps the world.
    ///
    /// Fails with [`world::Error::NotGenerated`] if the server runs no world of that name and
    /// the server file holds none (a temporary world unloaded is gone), and, for a stored world,
    /// with [`UnknownGenerator`](world::Error::UnknownGenerator) for a generator not registered,
    /// [`InvalidSettings`](world::Error::InvalidSettings) for a view value out of its range and
    /// [`Full`](world::Error::Full) past 1,024 worlds named this run.
    #[doc(alias = "WorldManager::loadWorld")]
    pub fn load_world(&mut self, name: &str, options: &world::Options) -> world::Result<world::Id> {
        let state = &mut *self.state;
        if let Some(i) = named(state, name) {
            if state.catalog.is_loaded(i) {
                state.catalog.unloads.remove(&i);
                return Ok(world::Id::of(i));
            }
            if state.worlds.get(i).map(World::kind) == Some(Kind::Persistent) {
                return Ok(reload(state, i));
            }
            return Err(world::Error::NotGenerated);
        }
        let Some(&record) = state.catalog.stored.get(name) else {
            return Err(world::Error::NotGenerated);
        };
        if options.settings().check("worlds").is_err() {
            return Err(world::Error::InvalidSettings);
        }
        let generator = state
            .catalog
            .generators
            .get(options.generator())
            .cloned()
            .ok_or(world::Error::UnknownGenerator)?;
        if state.worlds.len() >= MAX_WORLDS {
            return Err(world::Error::Full);
        }
        let mut world = save::new_world(
            name,
            Kind::Persistent,
            generator.as_ref(),
            options.seed(),
            options.spawn(),
        );
        save::resume(&mut world, record);
        world.set_difficulty(options.difficulty());
        state.catalog.stored.remove(name);
        let index = state.worlds.len();
        set_view(state, index, options);
        Ok(install(state, index, world, generator.as_ref(), false))
    }

    /// Unloads `world` in the next view phase, after [`WorldUnload`] listeners, who may cancel
    /// it (PMMP `WorldManager::unloadWorld`): its players go to the default world's spawn, its
    /// chunks unload, and the commit that follows saves what changed. Its [`world::Id`] stays its
    /// own: [`load_world`](Self::load_world) loads a persistent world in the same place.
    ///
    /// Fails with [`world::Error::WorldNotLoaded`] for a world that is not loaded and with
    /// [`DefaultWorld`](world::Error::DefaultWorld) for the default world (PMMP refuses it
    /// unless forced, then leaves the server without one; Anchor always has one).
    #[doc(alias = "WorldManager::unloadWorld")]
    pub fn unload_world(&mut self, world: world::Id) -> world::Result<()> {
        self.ask_unload(world, Unload::Asked)
    }

    /// [`unload_world`](Self::unload_world) that a cancelled [`WorldUnload`] does not stop (PMMP
    /// `unloadWorld` with `forceUnload`).
    #[doc(alias = "WorldManager::unloadWorld")]
    pub fn force_unload_world(&mut self, world: world::Id) -> world::Result<()> {
        self.ask_unload(world, Unload::Forced)
    }

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

    /// Whether a world named `name` exists: the server runs it, or the server file holds it
    /// (PMMP `WorldManager::isWorldGenerated`). A temporary world unloaded does not.
    #[doc(alias = "WorldManager::isWorldGenerated")]
    pub fn is_world_generated(&self, name: &str) -> bool {
        let state = &*self.state;
        named(state, name).is_some_and(|i| {
            state.catalog.is_loaded(i)
                || state.worlds.get(i).map(World::kind) == Some(Kind::Persistent)
        }) || state.catalog.stored.contains_key(name)
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

    /// Stops or starts the time and tells the world's players (PMMP `World::sendTime`).
    fn set_time_stopped(&mut self, world: world::Id, stopped: bool) -> bool {
        let Some(w) = super::index(self.state, world) else {
            return false;
        };
        if let Some(world) = self.state.worlds.get_mut(w) {
            world.set_time_stopped(stopped);
        }
        let packet = survival::clock_state(self.state, w);
        super::send_to_world(self.state, w, &packet);
        true
    }

    /// Gives `world` its own difficulty, or `None` to follow the server's
    /// ([`difficulty`](Self::difficulty)), and tells its players (PMMP `World::setDifficulty`).
    /// Its rules (hunger, fire) use it until the server stops: it is not saved, and a restart
    /// takes the difficulty of the settings again (PLAN.md 10.6). False, changing nothing, for
    /// a world that is not loaded, or a difficulty other than hard on a `hardcore` server.
    #[doc(alias = "World::setDifficulty")]
    pub fn set_world_difficulty(
        &mut self,
        world: world::Id,
        difficulty: Option<Difficulty>,
    ) -> bool {
        let Some(w) = super::index(self.state, world) else {
            return false;
        };
        let applied = difficulty.unwrap_or(self.state.settings.difficulty);
        if self.state.settings.hardcore && applied != Difficulty::Hard {
            return false;
        }
        if let Some(world) = self.state.worlds.get_mut(w) {
            world.set_difficulty(difficulty);
        }
        let packet = Packet::SetDifficulty(SetDifficulty {
            difficulty: crate::tick::spawn::difficulty_id(applied).cast_unsigned(),
        });
        super::send_to_world(self.state, w, &packet);
        true
    }
}

#[cfg(test)]
mod tests;
