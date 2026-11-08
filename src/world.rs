//! Worlds: block and chunk positions, the loaded chunks of a world, generators,
//! the neighbour rules that keep fence, pane, wall, fence gate, stair and tripwire states up to
//! date when a block is set, and the light of the loaded chunks.
//!
//! The overworld is the only dimension: blocks from y -64 to 319 in 24 sub-chunks.

pub(crate) mod biome;
mod chunk;
mod connect;
mod generator;
mod index;
mod light;
mod paletted;
mod particle;
mod sound;
mod sub_chunk;
pub(crate) mod tree;

pub use crate::tick::{MAX_TICKET_CHUNKS, MAX_TICKET_RADIUS, MAX_TICKETS};
pub use chunk::Chunk;
pub(crate) use chunk::{NO_HEIGHT, PLAINS, SUB_CHUNKS};
pub(crate) use generator::Generators;
pub use generator::{Flat, Generator, Void};
pub(crate) use light::Mode as LightMode;
pub(crate) use paletted::{Paletted, SLOTS, offset};
pub use particle::Particle;
pub(crate) use particle::id as particle_id;
pub(crate) use sound::PLAYER_ENTITY_TYPE;
pub use sound::{NoteInstrument, Sound};
pub(crate) use sub_chunk::{Entry, SubChunk};
pub use tree::TreeType;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use crate::block::{self, Facing, State};

/// The Bedrock world border: the farthest a player, an entity or a command position may be from
/// the world's middle on an axis, in blocks.
pub const BORDER: i32 = 30_000_000;
/// Ticks in a day.
pub(crate) const DAY: i64 = 24_000;
/// The highest block y of the world.
#[doc(alias = "World::getMaxY")]
pub const MAX_Y: i32 = 319;
/// The lowest sub-chunk y of a world.
pub(crate) const MIN_SUB_CHUNK_Y: i32 = MIN_Y >> 4;
/// The lowest block y of the world.
#[doc(alias = "World::getMinY")]
pub const MIN_Y: i32 = -64;
/// How many chunks around the spawn's chunk the server keeps loaded, on each axis: the 3 × 3
/// spawn chunks.
pub(crate) const SPAWN_CHUNK_RADIUS: i32 = 1;

/// A block position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BlockPos {
    /// East is positive.
    pub x: i32,
    /// Up is positive; blocks are from [`MIN_Y`] to [`MAX_Y`].
    pub y: i32,
    /// South is positive.
    pub z: i32,
}

impl BlockPos {
    /// The position (x, y, z).
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    /// The chunk that holds the position.
    pub const fn chunk(self) -> ChunkPos {
        ChunkPos::new(self.x >> 4, self.z >> 4)
    }

    /// The block that holds the point `v`: each coordinate rounded down, saturating at the `i32`
    /// range (a coordinate that is not a number counts as 0).
    pub fn containing(v: crate::math::Vec3) -> Self {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "consistency: `as` saturates and maps NaN to 0, the same on every platform"
        )]
        let floor = |c: f64| c.floor() as i32;
        Self::new(floor(v.x), floor(v.y), floor(v.z))
    }

    /// The position inside its chunk: x and z from 0 to 15.
    pub(crate) fn local(self) -> (u8, u8) {
        #[expect(
            clippy::expect_used,
            reason = "invariant: a value masked to 0..=15 fits u8"
        )]
        let local = |v: i32| u8::try_from(v & 15).expect("a masked coordinate fits u8");
        (local(self.x), local(self.z))
    }

    /// The position one block toward `face`, if it is a valid `i32` position (the world height
    /// is not checked).
    #[doc(alias = "Position::getSide")]
    pub fn step(self, face: block::Face) -> Option<Self> {
        let (dx, dy, dz) = face.offset();
        self.offset(dx, dy, dz)
    }

    /// The position moved by (dx, dy, dz), if it is a valid `i32` position (the world height is
    /// not checked).
    pub(crate) fn offset(self, dx: i32, dy: i32, dz: i32) -> Option<Self> {
        Some(Self::new(
            self.x.checked_add(dx)?,
            self.y.checked_add(dy)?,
            self.z.checked_add(dz)?,
        ))
    }

    /// The center of the block cell.
    pub(crate) fn to_center(self) -> crate::math::Vec3 {
        crate::math::Vec3::new(
            f64::from(self.x) + 0.5,
            f64::from(self.y) + 0.5,
            f64::from(self.z) + 0.5,
        )
    }

    /// The lowest corner of the block cell.
    pub(crate) fn to_corner(self) -> crate::math::Vec3 {
        crate::math::Vec3::new(f64::from(self.x), f64::from(self.y), f64::from(self.z))
    }

    /// The position one block toward `facing`, if it is a valid `i32` position.
    fn side(self, facing: Facing) -> Option<Self> {
        let (dx, dz) = facing.offset();
        Some(Self::new(
            self.x.checked_add(dx)?,
            self.y,
            self.z.checked_add(dz)?,
        ))
    }

    /// The position `dy` blocks up (down when negative), if it is a valid `i32` position.
    fn up(self, dy: i32) -> Option<Self> {
        Some(Self::new(self.x, self.y.checked_add(dy)?, self.z))
    }
}

/// A chunk position: a 16×16 column of blocks (block x and z divided by 16, rounded down).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ChunkPos {
    /// East is positive.
    pub x: i32,
    /// South is positive.
    pub z: i32,
}

impl ChunkPos {
    /// The chunk position (x, z).
    pub const fn new(x: i32, z: i32) -> Self {
        Self { x, z }
    }
}

/// A world of the server: its place in the order the server first had it (the settings' world
/// first, then the worlds of [`ServerBuilder::world`](crate::ServerBuilder::world) in
/// registration order, then those made or loaded while it runs), with a generation for a world
/// that took the place of one gone. A world keeps its `Id` while the server runs, through unloads
/// and loads; no other world ever has it, so the `Id` of a temporary world unloaded or a
/// persistent world deleted names no world from then on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(u32);

impl Id {
    /// The settings' world (`level-name`), where players join unless
    /// [`Context::set_default_world`](crate::Context::set_default_world) names another.
    pub const DEFAULT: Self = Self(0);

    /// The world of index `index` ([`crate::save::WorldIndex`]).
    pub(crate) fn new(index: usize) -> Option<Self> {
        u32::try_from(index).ok().map(Self)
    }

    /// The world's index ([`crate::save::WorldIndex`]).
    pub(crate) fn index(self) -> usize {
        // u32 fits usize on every target Anchor builds for (32 and 64 bits).
        self.0 as usize
    }

    /// The world at `index` of a world the server has.
    pub(crate) fn of(index: usize) -> Self {
        #[expect(
            clippy::expect_used,
            reason = "invariant: a world takes an index only while it fits u32 (`next_index`)"
        )]
        Self::new(index).expect("a world index fits u32")
    }
}

/// The kind of a world, which decides whether it is saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Saved with every commit.
    Persistent,
    /// Never saved: it starts from its generator at every start, such as a minigame arena. A
    /// chunk nobody holds loaded unloads for good, so it loads again from the generator, without
    /// the changes made to it.
    Temporary,
}

/// A world operation that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The position is outside the world height or border (a chunk ticket's center too), or a
    /// local position outside its chunk; or it is not finite, a spawn where a player would stand
    /// outside the border, or an explosion's size that is not above 0.
    OutOfRange,
    /// The chunk of the position is not loaded.
    NotLoaded,
    /// The chunk of the position is quarantined: its saved data is damaged, so it is shown
    /// empty and never changed or saved.
    Quarantined,
    /// Stored chunk data, or a checkpoint's list of chunks, breaks a rule of the format.
    Malformed(&'static str),
    /// The server has no loaded world with that ID or name.
    WorldNotLoaded,
    /// A world of that name exists: the server file holds it, or the server runs it.
    WorldExists,
    /// The server file holds no world of that name.
    NotGenerated,
    /// The world is the default world, where players join: make another the default first.
    DefaultWorld,
    /// The world is loaded: unload it first.
    WorldLoaded,
    /// No generator of that name is registered ([`ServerBuilder::generator`]).
    ///
    /// [`ServerBuilder::generator`]: crate::ServerBuilder::generator
    UnknownGenerator,
    /// The name cannot be the name of the world's file `data/worlds/<name>.redb` on Windows and
    /// Linux alike: it is empty or too long; holds a control character or one of
    /// `< > : " / \ | ? *` (so no path); ends in a dot or a space or starts with a space; or is a
    /// name Windows keeps for a device, in any letter case and with any extension (`CON`, `PRN`,
    /// `AUX`, `NUL`, `CONIN$`, `CONOUT$`, `COM0` to `COM9`, `LPT0` to `LPT9`, and those with a
    /// superscript digit); or has the form of a Windows 8.3 short name (at most 8 bytes with `~`
    /// and a digit, then at most one dot and 3 bytes, as `LONGWO~1`), which Windows can take as
    /// another file's or folder's. Letter case is kept in the file name, but names that differ only in
    /// letter case are one world ([`WorldExists`](Self::WorldExists)), as Windows sees one file.
    InvalidName,
    /// A view value of the world's [`Options::settings`] is outside the range its
    /// `[worlds.<name>]` key allows.
    InvalidSettings,
    /// A limit is reached: the most worlds the server has at once (or the server file holds),
    /// [`MAX_TICKETS`] chunk tickets or [`MAX_TICKET_CHUNKS`] chunks they hold, a ticket radius
    /// over [`MAX_TICKET_RADIUS`], the unknown states a chunk can hold, or the lightning bolts
    /// still showing (1,024).
    Full,
    /// The server is `hardcore`: every world plays on hard.
    Hardcore,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => f.write_str("position is outside the world"),
            Self::NotLoaded => f.write_str("chunk is not loaded"),
            Self::Quarantined => f.write_str("chunk is quarantined"),
            Self::Malformed(reason) => write!(f, "malformed chunk data: {reason}"),
            Self::WorldNotLoaded => f.write_str("world is not loaded"),
            Self::WorldExists => f.write_str("world already exists"),
            Self::NotGenerated => f.write_str("world was never generated"),
            Self::DefaultWorld => f.write_str("world is the default world"),
            Self::WorldLoaded => f.write_str("world is loaded"),
            Self::UnknownGenerator => f.write_str("no generator of that name is registered"),
            Self::InvalidName => f.write_str("world name cannot be a file name"),
            Self::InvalidSettings => f.write_str("world view value is out of range"),
            Self::Full => f.write_str(
                "world, chunk ticket, chunk unknown state or lightning bolt limit reached",
            ),
            Self::Hardcore => f.write_str("a hardcore server plays every world on hard"),
        }
    }
}

impl std::error::Error for Error {}

/// A result with the module's [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The settings [`Context::create_world`](crate::Context::create_world) makes a world with and
/// [`Context::load_world`](crate::Context::load_world) loads a stored one with. The defaults: a
/// [`Kind::Persistent`] world of the `flat` generator, a seed made from the world's name, the
/// generator's spawn, the server's difficulty and the global view values. The server file saves
/// none of them: a world loaded after a restart takes the options of that load.
#[doc(alias = "WorldCreationOptions")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    kind: Kind,
    generator: String,
    seed: Option<i64>,
    difficulty: Option<crate::config::Difficulty>,
    /// The view values and the spawn.
    settings: crate::config::WorldSettings,
}

impl Options {
    /// The default options.
    #[doc(alias = "WorldCreationOptions::create")]
    pub fn new() -> Self {
        Self::default()
    }

    /// The world's own difficulty, or `None` to follow the server's ([`World::difficulty`]; a
    /// `hardcore` server plays every world on hard).
    #[doc(alias = "WorldCreationOptions::getDifficulty")]
    pub fn difficulty(&self) -> Option<crate::config::Difficulty> {
        self.difficulty
    }

    /// The name of the generator that fills the world: one registered with
    /// [`ServerBuilder::generator`](crate::ServerBuilder::generator), or a built-in one (`flat`,
    /// `void`) or a generator of a world the server was built with.
    #[doc(alias = "WorldCreationOptions::getGeneratorClass")]
    pub fn generator(&self) -> &str {
        &self.generator
    }

    /// The world's kind: whether it is saved.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The seed, or `None` for one made from the world's name (the same at every start).
    #[doc(alias = "WorldCreationOptions::getSeed")]
    pub fn seed(&self) -> Option<i64> {
        self.seed
    }

    /// The world's own view values and spawn: each view value set replaces the global one in
    /// this world, as a `[worlds.<name>]` table does for the worlds the server is built with.
    /// They last while the server runs: a later run loads the world with the options of its load,
    /// and the world keeps the spawn its file stores.
    pub fn settings(&self) -> crate::config::WorldSettings {
        self.settings
    }

    /// The spawn of a new world, or `None` for the generator's ([`Generator::spawn`]): the
    /// `spawn` of [`settings`](Self::settings). A world its file stores keeps its own spawn.
    #[doc(alias = "WorldCreationOptions::getSpawnPosition")]
    pub fn spawn(&self) -> Option<BlockPos> {
        self.settings.spawn
    }

    /// The options with the world's own `difficulty` (a `hardcore` server still plays the world
    /// on hard).
    #[doc(alias = "WorldCreationOptions::setDifficulty")]
    #[must_use]
    pub fn with_difficulty(mut self, difficulty: crate::config::Difficulty) -> Self {
        self.difficulty = Some(difficulty);
        self
    }

    /// The options with the generator named `name`: the first value registered under it, or,
    /// for a world of [`ServerBuilder::world`](crate::ServerBuilder::world) whose generator has
    /// the name, that world's own value.
    #[doc(alias = "WorldCreationOptions::setGeneratorClass")]
    #[must_use]
    pub fn with_generator(mut self, name: impl Into<String>) -> Self {
        self.generator = name.into();
        self
    }

    /// The options with `kind`.
    #[must_use]
    pub fn with_kind(mut self, kind: Kind) -> Self {
        self.kind = kind;
        self
    }

    /// The options with `seed`.
    #[doc(alias = "WorldCreationOptions::setSeed")]
    #[must_use]
    pub fn with_seed(mut self, seed: i64) -> Self {
        self.seed = Some(seed);
        self
    }

    /// The options with the world's own view values and spawn `settings` (each view value within
    /// its key's range); it replaces [`with_spawn`](Self::with_spawn).
    #[must_use]
    pub fn with_settings(mut self, settings: crate::config::WorldSettings) -> Self {
        self.settings = settings;
        self
    }

    /// The options with `spawn`.
    #[doc(alias = "WorldCreationOptions::setSpawnPosition")]
    #[must_use]
    pub fn with_spawn(mut self, spawn: BlockPos) -> Self {
        self.settings.spawn = Some(spawn);
        self
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            kind: Kind::Persistent,
            generator: "flat".to_owned(),
            seed: None,
            difficulty: None,
            settings: crate::config::WorldSettings::default(),
        }
    }
}

/// Chunks [`Context::hold_chunks`](crate::Context::hold_chunks) keeps loaded until
/// [`Context::release_chunks`](crate::Context::release_chunks) lets them go. A ticket is never
/// given out twice while the server runs. A ticket never released keeps its chunks loaded for as
/// long as the server runs.
#[doc(alias = "ChunkLoader")]
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ticket(u64);

impl Ticket {
    /// The ticket of number `n`.
    pub(crate) fn new(n: u64) -> Self {
        Self(n)
    }

    /// Its number.
    pub(crate) fn get(self) -> u64 {
        self.0
    }
}

/// A block as a world holds it ([`World::stored_block`]), to put elsewhere with
/// [`World::set_stored_block`].
#[derive(Debug, Clone)]
pub enum StoredBlock {
    /// A state Anchor knows.
    Known(State),
    /// A state Anchor does not know, which [`World::block`] reads as `None`.
    Unknown(UnknownBlock),
}

/// A block state Anchor does not know, kept as the server file stored it.
#[derive(Clone)]
pub struct UnknownBlock {
    /// The unknown states of the chunk it was read from, shared so a read copies no NBT.
    list: std::sync::Arc<Vec<crate::nbt::Compound>>,
    /// Its place in `list`.
    index: usize,
}

impl UnknownBlock {
    /// Its stored NBT: `{name, states, version}`.
    #[expect(
        clippy::missing_panics_doc,
        reason = "invariant: Chunk::stored_block makes it only from an index inside the list"
    )]
    pub fn nbt(&self) -> &crate::nbt::Compound {
        #[expect(
            clippy::expect_used,
            reason = "invariant: Chunk::stored_block makes it only from an index inside the list"
        )]
        self.list
            .get(self.index)
            .expect("an unknown block indexes past its list")
    }
}

impl fmt::Debug for UnknownBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("UnknownBlock").field(self.nbt()).finish()
    }
}

/// The weather of a world ([`World::weather`]). It stays as set: there is no natural cycle and
/// no duration. Clients draw rain as snow where their biome is cold. The official server strikes
/// lightning at random during thunder; Anchor strikes only where
/// [`Context::strike_lightning`](crate::Context::strike_lightning) says.
///
/// Anchor neither saves the weather nor changes it by itself: every world is clear when the
/// server starts, and only [`Context::set_weather`](crate::Context::set_weather) changes it. A
/// server program that wants a cycle builds one: a repeating task
/// ([`Context::schedule_repeating`](crate::Context::schedule_repeating) or
/// [`ServerBuilder::every`](crate::ServerBuilder::every)) sets the weather, and a server data
/// value ([`ServerBuilder::server_data`](crate::ServerBuilder::server_data),
/// [`Context::data_mut`](crate::Context::data_mut)) keeps the cycle's place across restarts,
/// committed with the rest of the game. The weather itself starts clear again, so the program
/// sets it from the kept place at start if it wants the cycle to go on. No event tells of a
/// change: only the program's own calls make one, and direct calls raise no events.
///
/// ```no_run
/// use std::num::NonZeroU32;
///
/// use anchor::world::Weather;
/// use anchor::{Context, Server};
/// use serde::{Deserialize, Serialize};
///
/// /// The default world's place in the cycle, kept across restarts.
/// #[derive(Default, Serialize, Deserialize)]
/// struct Sky {
///     turn: u8,
/// }
///
/// /// Ten minutes of each weather in turn: clear, rain, thunder.
/// fn turn(cx: &mut Context<'_>) {
///     let world = cx.default_world().id();
///     let turn = cx.data::<Sky>().map_or(0, |sky| sky.turn.wrapping_add(1) % 3);
///     let weather = [Weather::Clear, Weather::Rain, Weather::Thunder][usize::from(turn)];
///     cx.set_weather(world, weather);
///     if let Some(sky) = cx.data_mut::<Sky>() {
///         sky.turn = turn;
///     }
/// }
///
/// # fn main() -> Result<(), anchor::Error> {
/// let ten_minutes = NonZeroU32::new(Context::TICKS_PER_SECOND * 60 * 10).expect("not zero");
/// Server::builder()
///     .server_data::<Sky>("sky")
///     .every(ten_minutes, turn)
///     .build()?
///     .run()
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Weather {
    /// No rain.
    #[default]
    Clear,
    /// Rain, or snow in cold biomes.
    Rain,
    /// Rain with thunder: a darker sky.
    Thunder,
}

/// What a lightning bolt of [`Context::strike_lightning`](crate::Context::strike_lightning)
/// does besides being seen and heard, which it always is. The default does nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Lightning {
    /// The players and living entities near the strike take 5 damage, at every difficulty.
    pub damage: bool,
}

/// A world: its settings and the chunks loaded in memory.
///
/// Only the tick thread changes it. The engine loads and unloads chunks; a block
/// of a chunk that is not loaded reads as `None` and cannot be set.
// api-class: World
pub struct World {
    /// Its place in the server's world list (the tick numbers the worlds when it starts).
    id: Id,
    name: String,
    kind: Kind,
    seed: i64,
    spawn: BlockPos,
    /// The world's clock in ticks; the day cycle follows it.
    time: i64,
    /// The time stands still, until the server stops.
    time_stopped: bool,
    /// Ticks the world has run since it was made or loaded ([`Self::age_ticks`]).
    age_ticks: u64,
    /// The world's own difficulty; `None` follows the server's.
    difficulty: Option<crate::config::Difficulty>,
    /// The weather, clear at start.
    weather: Weather,
    /// The `chunks` key number of a persistent world the server file holds or will hold.
    number: Option<u32>,
    /// The name of the generator that fills its chunks never saved (empty in tests that make
    /// worlds without one).
    generator: &'static str,
    /// The loaded chunks; `loaded` holds their positions in order.
    chunks: index::Chunks,
    loaded: BTreeSet<ChunkPos>,
    /// Positions whose saved chunk is damaged: shown empty, never changed or saved.
    quarantined: BTreeSet<ChunkPos>,
    /// The last state of each block changed since the tick last took them.
    changes: BTreeMap<BlockPos, Entry>,
    /// Every block changed since the block updates last took it, oldest first, each at most once
    /// ([`Self::next_changed`]): bounded by the loaded blocks.
    feed: VecDeque<BlockPos>,
    /// The positions in `feed`.
    fed: BTreeSet<BlockPos>,
    /// The light work of the loaded chunks.
    light: light::Engine,
}

impl World {
    /// A world with no chunks loaded. The save thread fills its chunks with the world's
    /// generator; the tick only needs its seed and spawn.
    pub(crate) fn new(name: impl Into<String>, kind: Kind, seed: i64, spawn: BlockPos) -> Self {
        Self {
            id: Id::DEFAULT,
            name: name.into(),
            kind,
            seed,
            spawn,
            time: 0,
            time_stopped: false,
            age_ticks: 0,
            difficulty: None,
            weather: Weather::Clear,
            number: None,
            generator: "",
            chunks: index::Chunks::new(),
            loaded: BTreeSet::new(),
            quarantined: BTreeSet::new(),
            changes: BTreeMap::new(),
            feed: VecDeque::new(),
            fed: BTreeSet::new(),
            light: light::Engine::new(),
        }
    }

    /// Ticks the world has run since it was made or loaded, its clock stopped or not, which
    /// moving the clock does not change. Not saved, so it starts at 0 at every load.
    #[cfg(test)]
    pub(crate) fn age_ticks(&self) -> u64 {
        self.age_ticks
    }

    /// The biome ID at `pos` (such as 1 for plains), or `None` if its chunk is not loaded or
    /// `pos` is outside the world height.
    #[doc(alias = "World::getBiomeId")]
    #[doc(alias = "World::getBiome")]
    pub fn biome(&self, pos: BlockPos) -> Option<u32> {
        let (x, z) = pos.local();
        self.chunks.get(pos.chunk())?.biome(x, pos.y, z)
    }

    /// The block at `pos`, or `None` if its chunk is not loaded (or quarantined), `pos` is outside
    /// the world height, or the block is a state Anchor does not know.
    #[doc(alias = "World::getBlockAt")]
    #[doc(alias = "World::getBlock")]
    #[inline]
    pub fn block(&self, pos: BlockPos) -> Option<State> {
        self.entry(pos)?.state()
    }

    /// The block at `pos` as the rules see it: air where there is none (`pos` is `None`, its chunk
    /// is not loaded or is quarantined, or `pos` is outside the world height), and bedrock (solid
    /// and full, which a placement does not replace) for a state Anchor does not know.
    #[inline]
    pub(crate) fn block_for_rules(&self, pos: Option<BlockPos>) -> State {
        match pos.and_then(|p| self.entry(p)) {
            Some(Entry::Known(s)) => s,
            Some(Entry::Unknown(_)) => block::BEDROCK,
            None => block::AIR,
        }
    }

    /// The block light at `pos`, 0 to 15, or `None` if its chunk is not loaded or not lit yet,
    /// or `pos` is outside the world height.
    ///
    /// Light is worked out on the server for game rules and never sent to clients. With the
    /// setting `lighting = "fixed"` nothing is worked out: block light is 0 and sky light is
    /// `fixed-light-level` (15 by default, as under an open sky at noon) at every position of a
    /// loaded chunk. With `lighting = "on-load"` a chunk's light is worked out when it loads and
    /// block changes leave it as it is.
    #[doc(alias = "World::getBlockLightAt")]
    pub fn block_light(&self, pos: BlockPos) -> Option<u8> {
        self.light.light(&self.chunks, pos, light::Channel::Block)
    }

    /// The changed blocks waiting for [`Self::next_changed`], oldest first (state hash).
    #[cfg(test)]
    pub(crate) fn changed(&self) -> impl ExactSizeIterator<Item = BlockPos> + '_ {
        self.feed.iter().copied()
    }

    /// The entries of the changed-block tables (`changes`, `feed`, `fed`), for leak tests.
    #[cfg(test)]
    pub(crate) fn changed_len(&self) -> usize {
        self.changes.len() + self.feed.len() + self.fed.len()
    }

    /// What a checkpoint keeps of the world, naming every field; `fed` is `feed` again, and the
    /// id, name, kind, number and generator from the world list the checkpoint keeps (and the
    /// server file of its tick).
    pub(crate) fn checkpoint(&self) -> (Saved, Kept) {
        let Self {
            id: _,
            name: _,
            kind: _,
            seed,
            spawn,
            time,
            time_stopped,
            age_ticks,
            difficulty,
            weather,
            number: _,
            generator: _,
            chunks,
            loaded,
            quarantined,
            changes,
            feed,
            fed: _,
            light,
        } = self;
        let mut kept_chunks = Vec::with_capacity(loaded.len());
        for &pos in loaded {
            if let Some(chunk) = chunks.get(pos) {
                kept_chunks.push((pos, chunk.clone(), chunk.light().cloned()));
            }
        }
        let kept = Kept {
            light: light.clone(),
            chunks: kept_chunks,
        };
        let saved = Saved {
            seed: *seed,
            spawn: *spawn,
            time: *time,
            time_stopped: *time_stopped,
            age_ticks: *age_ticks,
            difficulty: *difficulty,
            weather: *weather,
            loaded: loaded.clone(),
            quarantined: quarantined.clone(),
            changes: changes.clone(),
            feed: feed.clone(),
        };
        (saved, kept)
    }

    /// The loaded chunk at `pos`, to read; `None` if it is not loaded. Blocks
    /// change through [`Self::set_block`], which keeps the block rules, light and viewers up to
    /// date.
    #[doc(alias = "World::getChunk")]
    pub fn chunk(&self, pos: ChunkPos) -> Option<&Chunk> {
        self.chunks.get(pos)
    }

    /// The world's own difficulty, or `None` while it follows the server's
    /// ([`Context::difficulty`](crate::Context::difficulty)), as a new world does unless it is
    /// made or loaded with one. The rules of the world use it, but on a `hardcore` server they use
    /// hard whatever it says. Not saved: it comes from the options of every load.
    #[doc(alias = "World::getDifficulty")]
    pub fn difficulty(&self) -> Option<crate::config::Difficulty> {
        self.difficulty
    }

    /// The layer 0 entry at `pos`, if its chunk is loaded and `pos` is in the world height.
    #[inline]
    pub(crate) fn entry(&self, pos: BlockPos) -> Option<Entry> {
        let (x, z) = pos.local();
        self.chunks.get(pos.chunk())?.entry(x, pos.y, z)
    }

    /// The brightest light at `pos` at the world's time: [`Self::real_sky_light`] or
    /// [`Self::block_light`], whichever is brighter.
    #[doc(alias = "World::getFullLightAt")]
    pub fn full_light(&self, pos: BlockPos) -> Option<u8> {
        let (block, sky) = self.light.lights(&self.chunks, pos)?;
        Some(
            sky.saturating_sub(light::sky_light_reduction(self.time))
                .max(block),
        )
    }

    /// The name of the generator that fills the chunks never saved ([`Generator::name`]).
    pub(crate) fn generator(&self) -> &'static str {
        self.generator
    }

    /// The top y in the column at (x, z) that holds a block other than air, or `None` if the
    /// column is all air or its chunk is not loaded.
    #[doc(alias = "World::getHighestBlockAt")]
    pub fn highest_block(&self, x: i32, z: i32) -> Option<i32> {
        let pos = BlockPos::new(x, 0, z);
        let (lx, lz) = pos.local();
        self.chunks.get(pos.chunk())?.highest_block(lx, lz)
    }

    /// Its ID on the server.
    #[doc(alias = "World::getId")]
    pub fn id(&self) -> Id {
        self.id
    }

    /// Whether the chunk is loaded and lit (always, with light off): random block ticks need it
    /// around a chunk.
    pub(crate) fn is_chunk_lit(&self, pos: ChunkPos) -> bool {
        self.light.is_lit(&self.chunks, pos)
    }

    /// Whether the chunk is loaded (quarantined chunks are not).
    #[doc(alias = "World::isChunkLoaded")]
    pub fn is_chunk_loaded(&self, pos: ChunkPos) -> bool {
        self.chunks.contains(pos)
    }

    /// Whether `pos` is inside the world height (x and z are always inside).
    #[doc(alias = "World::isInWorld")]
    pub fn is_in_world(pos: BlockPos) -> bool {
        (MIN_Y..=MAX_Y).contains(&pos.y)
    }

    /// Whether no light work is waiting.
    #[cfg(test)]
    pub(crate) fn is_light_idle(&self) -> bool {
        self.light.is_idle()
    }

    /// Whether the chunk at `pos` is quarantined: its saved data is damaged, so it never loads
    /// until the position unloads (no player or ticket holds it).
    pub fn is_quarantined(&self, pos: ChunkPos) -> bool {
        self.quarantined.contains(&pos)
    }

    /// Whether the chunk is one of the 3 × 3 spawn chunks the server keeps loaded: those around
    /// the spawn's chunk inside the world border.
    #[doc(alias = "World::isSpawnChunk")]
    pub fn is_spawn_chunk(&self, pos: ChunkPos) -> bool {
        let center = self.spawn.chunk();
        let near = |a: i32, b: i32| a.abs_diff(b) <= SPAWN_CHUNK_RADIUS.cast_unsigned();
        is_inside_border(pos) && near(pos.x, center.x) && near(pos.z, center.z)
    }

    /// Whether the time stands still ([`Context::stop_time`](crate::Context::stop_time)): the
    /// day cycle and the sun stay where they are. Not saved: every start runs the time
    /// (`daylight-cycle` stops every world's).
    pub fn is_time_stopped(&self) -> bool {
        self.time_stopped
    }

    /// The world's kind: whether it is saved.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// A hash of the world's light (state hashes): the light work waiting,
    /// then each loaded chunk's light in position order. About 260 word mixes per sub-chunk that
    /// is not uniform, a few per one that is.
    #[cfg(test)]
    pub(crate) fn light_hash(&self) -> u64 {
        let mut h = light::Digest::new();
        self.light.hash_into(&mut h);
        for &pos in &self.loaded {
            match self.chunks.get(pos).and_then(Chunk::light) {
                Some(l) => {
                    h.word(1);
                    l.hash_into(&mut h);
                }
                None => h.word(0),
            }
        }
        h.finish()
    }

    /// The loaded chunks, in position order.
    #[doc(alias = "World::getLoadedChunks")]
    pub fn loaded_chunks(&self) -> impl ExactSizeIterator<Item = ChunkPos> + '_ {
        self.loaded.iter().copied()
    }

    /// The world's name, unique on the server: its file name and the name it shows.
    #[doc(alias = "World::getFolderName")]
    #[doc(alias = "World::getDisplayName")]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The `chunks` key number of a persistent world, `None` for a temporary one.
    pub(crate) fn number(&self) -> Option<u32> {
        self.number
    }

    /// The quarantined positions, in order.
    #[cfg(test)]
    pub(crate) fn quarantined(&self) -> impl ExactSizeIterator<Item = ChunkPos> + '_ {
        self.quarantined.iter().copied()
    }

    /// The sky light at `pos` at the world's time: [`Self::sky_light`] less the time of day's
    /// reduction, 0 by day to 11 at night.
    #[doc(alias = "World::getRealBlockSkyLightAt")]
    pub fn real_sky_light(&self, pos: BlockPos) -> Option<u8> {
        let sky = self.sky_light(pos)?;
        Some(sky.saturating_sub(light::sky_light_reduction(self.time)))
    }

    /// The seed the generator gets.
    #[doc(alias = "World::getSeed")]
    pub fn seed(&self) -> i64 {
        self.seed
    }

    /// The sky light at `pos` whatever the time of day, 0 to 15: 15 under the open sky, less
    /// through filtering blocks and into shade (see [`Self::block_light`] for `None` and the
    /// `lighting` setting).
    #[doc(alias = "World::getPotentialBlockSkyLightAt")]
    pub fn sky_light(&self, pos: BlockPos) -> Option<u8> {
        self.light.light(&self.chunks, pos, light::Channel::Sky)
    }

    /// Where players appear.
    #[doc(alias = "World::getSpawnLocation")]
    pub fn spawn(&self) -> BlockPos {
        self.spawn
    }

    /// The block at `pos` as the world holds it, to put elsewhere with
    /// [`Self::set_stored_block`]: unlike [`Self::block`], a state Anchor does not know is kept,
    /// as its stored NBT. `None` if the chunk is not loaded (or quarantined) or `pos` is outside
    /// the world height.
    pub fn stored_block(&self, pos: BlockPos) -> Option<StoredBlock> {
        let (x, z) = pos.local();
        self.chunks.get(pos.chunk())?.stored_block(x, pos.y, z)
    }

    /// The time of the day cycle, 0 to 23,999: 1,000 is day, 6,000 noon, 13,000 night, 18,000
    /// midnight and 23,000 sunrise.
    #[doc(alias = "World::getTimeOfDay")]
    pub fn time_of_day(&self) -> u32 {
        tick_of_day(self.time)
    }

    /// The world's clock in ticks (0 for a new world), saved with the world: it is state, not a
    /// setting. It goes up by one each tick the world is loaded, unless its time is stopped or
    /// `daylight-cycle` is off, and jumps when [`Context::set_time`](crate::Context::set_time)
    /// moves it. It counts on past a day (the time of day is [`Self::time_of_day`]).
    #[doc(alias = "World::getTime")]
    pub fn time_ticks(&self) -> i64 {
        self.time
    }

    /// The weather ([`Context::set_weather`](crate::Context::set_weather) changes it). Not saved:
    /// every world is clear at start.
    pub fn weather(&self) -> Weather {
        self.weather
    }

    /// Counts one more tick of [`Self::age_ticks`]: the clock's tick.
    pub(crate) fn add_age_tick(&mut self) {
        self.age_ticks = self.age_ticks.saturating_add(1);
    }

    /// After the tick unloaded every chunk of the world (saving what it keeps first): forgets the
    /// quarantines, the changes waiting and the light work left, so the unloaded world holds
    /// nothing but its settings.
    pub(crate) fn clear(&mut self) {
        self.quarantined.clear();
        self.changes.clear();
        self.feed.clear();
        self.fed.clear();
        let mode = self.light.mode();
        self.light = light::Engine::new();
        self.light
            .set_mode(&mut self.chunks, std::iter::empty(), mode);
    }

    /// Makes an unloaded persistent world temporary when it is deleted: nothing of it is saved
    /// again, and a new world may take its place.
    pub(crate) fn forget(&mut self) {
        self.kind = Kind::Temporary;
        self.number = None;
    }

    /// Loads `chunk` at `pos`: from disk or a generator (clean, its saved custom blocks already
    /// resolved by the save thread), or from memory as it is (an unloaded chunk whose changes are
    /// not committed yet keeps its dirty mark). Lifts a quarantine of `pos` and returns a chunk it
    /// replaces, whose light leaves with it.
    ///
    /// A chunk lit on the CPU pool ([`Chunk::populate_light`]), as every load and every chunk
    /// from memory is, keeps its light and is connected to its lit neighbours; one without light
    /// is lit on the tick, in steps under the light budget of each tick.
    pub(crate) fn insert_chunk(&mut self, pos: ChunkPos, chunk: Chunk) -> Option<Chunk> {
        self.quarantined.remove(&pos);
        self.loaded.insert(pos);
        let mut old = self.chunks.insert(pos, chunk);
        if let Some(old) = &mut old {
            self.light.unloaded(&self.chunks, pos, old.take_light());
        }
        self.light.loaded(&mut self.chunks, pos);
        old
    }

    /// Takes the oldest block changed since the block update rules last took it: every change
    /// of [`Self::set_block`] (by players, tasks or rules, connection changes included) once,
    /// in order. A block changed again before it is taken keeps its place.
    pub(crate) fn next_changed(&mut self) -> Option<BlockPos> {
        let pos = self.feed.pop_front()?;
        self.fed.remove(&pos);
        Some(pos)
    }

    /// Puts the stored state `nbt`, which Anchor does not know, at `pos` as a chunk read from the
    /// server file holds it; the entry, or `None` if the chunk is not loaded.
    #[cfg(test)]
    pub(crate) fn put_unknown(
        &mut self,
        pos: BlockPos,
        nbt: crate::nbt::Compound,
    ) -> Option<Entry> {
        let (x, z) = pos.local();
        self.chunks
            .get_mut(pos.chunk())?
            .put_unknown(x, pos.y, z, nbt)
    }

    /// Quarantines `pos` because its saved data is damaged: a chunk loaded there is dropped,
    /// changes included, and the position stays quarantined until [`Self::remove_chunk`].
    pub(crate) fn quarantine(&mut self, pos: ChunkPos) {
        self.loaded.remove(&pos);
        self.take_chunk(pos);
        self.quarantined.insert(pos);
    }

    /// Unloads `pos`: removes its chunk (returned, dirty or not) and its quarantine. The caller
    /// saves a dirty chunk first.
    pub(crate) fn remove_chunk(&mut self, pos: ChunkPos) -> Option<Chunk> {
        self.quarantined.remove(&pos);
        self.loaded.remove(&pos);
        self.take_chunk(pos)
    }

    /// Puts the world back as [`Self::checkpoint`] kept it. Fails unless `kept` holds exactly the
    /// loaded chunks.
    pub(crate) fn restore(&mut self, saved: Saved, kept: Kept) -> Result<()> {
        let Saved {
            seed,
            spawn,
            time,
            time_stopped,
            age_ticks,
            difficulty,
            weather,
            loaded,
            quarantined,
            changes,
            feed,
        } = saved;
        let Kept {
            light,
            chunks: kept,
        } = kept;
        if kept.len() != loaded.len() || !kept.iter().map(|(p, _, _)| p).eq(loaded.iter()) {
            return Err(Error::Malformed(
                "a checkpoint keeps other chunks than the loaded ones",
            ));
        }
        let mut chunks = index::Chunks::new();
        for (pos, mut chunk, light) in kept {
            if let Some(l) = light {
                chunk.set_light(l);
            }
            chunks.insert(pos, chunk);
        }
        self.fed = feed.iter().copied().collect();
        self.seed = seed;
        self.spawn = spawn;
        self.time = time;
        self.time_stopped = time_stopped;
        self.age_ticks = age_ticks;
        self.difficulty = difficulty;
        self.weather = weather;
        self.chunks = chunks;
        self.loaded = loaded;
        self.quarantined = quarantined;
        self.changes = changes;
        self.feed = feed;
        self.light = light;
        Ok(())
    }

    /// Sets the biome ID at `pos`, saved with the chunk. Players see it when they are sent the
    /// chunk again.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for a position outside the world height.
    /// [`Error::NotLoaded`] for a chunk that is not loaded.
    /// [`Error::Quarantined`] for a quarantined chunk.
    #[doc(alias = "World::setBiomeId")]
    pub fn set_biome(&mut self, pos: BlockPos, biome: u32) -> Result<()> {
        let (x, z) = pos.local();
        let Some(chunk) = self
            .chunks
            .get_mut(pos.chunk())
            .filter(|_| Self::is_in_world(pos))
        else {
            return Err(self.unwritable(pos));
        };
        if chunk.biome(x, pos.y, z) != Some(biome) {
            chunk.set_biome(x, pos.y, z, biome)?;
            chunk.mark_dirty(pos.y);
        }
        Ok(())
    }

    /// Sets the block at `pos`.
    ///
    /// The neighbour rules then work out the connection properties of the block and of its
    /// neighbours (fences, panes and bars, walls, fence gates, stairs, tripwire),
    /// so the block may end up in a state that differs from `state` in those properties.
    /// Neighbours in chunks that are not loaded read as air and are not changed. No other block
    /// reacts: blocks never change by themselves or because a neighbour changed. The block's
    /// water layer is cleared.
    ///
    /// The block entity follows in the tick's next block change phase: a block that has one (a
    /// chest, a sign) gets an empty one there, which
    /// [`Context::block_entity_mut`](crate::Context::block_entity_mut) then lends, and a block
    /// entity the new block does not take goes, dropping what it held as items and losing its
    /// other data (sign text, banner patterns). Until then the old block entity is still there.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for a position outside the world height.
    /// [`Error::NotLoaded`] for a chunk that is not loaded.
    /// [`Error::Quarantined`] for a quarantined chunk.
    #[doc(alias = "World::setBlockAt")]
    #[doc(alias = "World::setBlock")]
    pub fn set_block(&mut self, pos: BlockPos, state: State) -> Result<()> {
        self.set_entry(pos, Entry::Known(state))
    }

    /// Sets the world's own difficulty: the options of its making or loading and the context's
    /// calls.
    pub(crate) fn set_difficulty(&mut self, difficulty: Option<crate::config::Difficulty>) {
        self.difficulty = difficulty;
    }

    /// Names the world's generator.
    pub(crate) fn set_generator(&mut self, name: &'static str) {
        self.generator = name;
    }

    /// Numbers the world: its place in the server's world list.
    pub(crate) fn set_id(&mut self, id: Id) {
        self.id = id;
    }

    /// Sets how light is worked out (setting `lighting`, [`LightMode::Dynamic`] for a new
    /// world). Turning the work off drops all light; turning it on lights every loaded chunk
    /// again.
    pub(crate) fn set_lighting(&mut self, mode: LightMode) {
        self.light
            .set_mode(&mut self.chunks, self.loaded.iter().copied(), mode);
    }

    /// Gives a persistent world its `chunks` key number.
    pub(crate) fn set_number(&mut self, number: Option<u32>) {
        self.number = number;
    }

    /// Moves the spawn. A persistent world saves it with its state, so it stays after a restart.
    /// The 3 × 3 spawn chunks the server keeps loaded (those inside the world border) move
    /// with it in the tick's view phase: the new ones load, the old ones unload unless a player
    /// sees them. No event tells of the move.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for a spawn where a player would stand outside the world border.
    #[doc(alias = "World::setSpawnLocation")]
    pub fn set_spawn(&mut self, spawn: BlockPos) -> Result<()> {
        if !is_valid_spawn(spawn) {
            return Err(Error::OutOfRange);
        }
        self.spawn = spawn;
        Ok(())
    }

    /// Sets the block at `pos` as [`Self::set_block`] does, a state Anchor does not know included:
    /// that one is kept as stored and sent to clients as a chunk read from the server file sends
    /// it.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for a position outside the world height.
    /// [`Error::NotLoaded`] for a chunk that is not loaded.
    /// [`Error::Quarantined`] for a quarantined chunk.
    /// [`Error::Full`] when the chunk holds the most unknown states it can.
    pub fn set_stored_block(&mut self, pos: BlockPos, block: &StoredBlock) -> Result<()> {
        let entry = match block {
            StoredBlock::Known(state) => Entry::Known(*state),
            StoredBlock::Unknown(unknown) => {
                // Checked before the chunk lists the state, so a failed write adds nothing.
                let Some(chunk) = self
                    .chunks
                    .get_mut(pos.chunk())
                    .filter(|_| Self::is_in_world(pos))
                else {
                    return Err(self.unwritable(pos));
                };
                chunk.unknown_entry(unknown)?
            }
        };
        self.set_entry(pos, entry)
    }

    /// Sets the time: the saved time at start, the clock's tick and the context's calls. Clients
    /// learn it from the clock's next correction.
    pub(crate) fn set_time(&mut self, time: i64) {
        self.time = time;
    }

    /// Stops or starts the time: the context's calls.
    pub(crate) fn set_time_stopped(&mut self, stopped: bool) {
        self.time_stopped = stopped;
    }

    /// Sets the weather: the context's calls.
    pub(crate) fn set_weather(&mut self, weather: Weather) {
        self.weather = weather;
    }

    /// Does up to `budget` units of queued light work (one unit per block looked at; the last item
    /// may look at up to one column more) and returns the units spent. The tick calls it once a
    /// tick; the rest waits for the next.
    pub(crate) fn step_light(&mut self, budget: u32) -> u32 {
        self.light.step(&mut self.chunks, budget)
    }

    /// Takes the blocks changed since the last call, with their last state: what the tick sends
    /// to the players who see them. The tick takes them every tick.
    pub(crate) fn take_changes(&mut self) -> BTreeMap<BlockPos, Entry> {
        std::mem::take(&mut self.changes)
    }

    /// Takes the chunks changed since the last call as copies that share their sub-chunks (a
    /// later write copies only the sub-chunk it changes) and marks them clean: the save
    /// snapshot. It visits every loaded chunk, once per save.
    pub(crate) fn take_dirty(&mut self) -> Vec<(ChunkPos, Chunk)> {
        let mut out = Vec::new();
        for &pos in &self.loaded {
            if let Some(chunk) = self.chunks.get_mut(pos)
                && chunk.is_dirty()
            {
                out.push((pos, chunk.take_save()));
            }
        }
        out
    }

    /// Why a block cannot be written at `pos`.
    fn unwritable(&self, pos: BlockPos) -> Error {
        if !Self::is_in_world(pos) {
            Error::OutOfRange
        } else if self.quarantined.contains(&pos.chunk()) {
            Error::Quarantined
        } else {
            Error::NotLoaded
        }
    }

    /// Writes `entry` at `pos` and works out the connections around it.
    fn set_entry(&mut self, pos: BlockPos, entry: Entry) -> Result<()> {
        let Some(old) = self.write(pos, entry) else {
            return Err(self.unwritable(pos));
        };
        self.connect_around(pos, old, entry);
        self.attach_tripwire(pos, old, entry);
        Ok(())
    }

    /// Takes the chunk at `pos` out of the index with its light work.
    fn take_chunk(&mut self, pos: ChunkPos) -> Option<Chunk> {
        let mut chunk = self.chunks.remove(pos)?;
        let light = chunk.take_light();
        self.light.unloaded(&self.chunks, pos, light);
        Some(chunk)
    }

    /// Writes an entry at `pos` of a loaded chunk; a change marks the chunk dirty and records the
    /// block in the change list. Returns the old entry, or `None` if nothing was written (the
    /// chunk is not loaded or `pos` is outside the world height).
    fn write(&mut self, pos: BlockPos, entry: Entry) -> Option<Entry> {
        let (x, z) = pos.local();
        let chunk = self.chunks.get_mut(pos.chunk())?;
        let old = chunk.put(x, pos.y, z, entry)?;
        if old != entry {
            chunk.mark_dirty(pos.y);
            self.changes.insert(pos, entry);
            if self.fed.insert(pos) {
                self.feed.push_back(pos);
            }
            self.light.changed(chunk, pos, old, entry);
        }
        Some(old)
    }
}

impl fmt::Debug for World {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("World")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("seed", &self.seed)
            .field("spawn", &self.spawn)
            .field("time", &self.time)
            .field("time_stopped", &self.time_stopped)
            .field("age_ticks", &self.age_ticks)
            .field("difficulty", &self.difficulty)
            .field("weather", &self.weather)
            .field("generator", &self.generator)
            .field("chunks", &self.loaded.len())
            .field("quarantined", &self.quarantined.len())
            .field("changes", &self.changes.len())
            .field("feed", &self.feed.len())
            .field("light", &self.light)
            .finish_non_exhaustive()
    }
}

/// What a checkpoint keeps of a world besides its plain fields: the
/// light work and every loaded chunk as it is, its light apart. A chunk the server file holds is
/// kept too: a load compacts its palettes, and a replay must go on from the exact chunk. It
/// shares the sub-chunks and light arrays with the live world until they change, and is encoded
/// only when a crash log is written.
#[derive(Debug, Clone)]
pub(crate) struct Kept {
    light: light::Engine,
    /// Each loaded chunk without its light, and its light, in position order.
    chunks: Vec<(ChunkPos, Chunk, Option<light::Levels>)>,
}

impl Kept {
    /// The bytes it holds or will encode to, about: the light arrays, the queued light work and
    /// the terrain ([`terrain_size`](crate::tick::checkpoint::terrain_size)).
    pub(crate) fn size(&self) -> usize {
        self.chunks
            .iter()
            .map(|(_, c, l)| {
                crate::tick::checkpoint::terrain_size(c)
                    .saturating_add(l.as_ref().map_or(0, light::Levels::heap_bytes))
            })
            .fold(self.light.heap_bytes(), usize::saturating_add)
    }
}

crate::tick::checkpoint::fields!(Kept { light, chunks });

/// The plain fields a checkpoint keeps of a world: its seed, spawn, time and weather, the loaded and
/// quarantined positions, and the changes waiting for the tick and the block updates.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Saved {
    pub(crate) seed: i64,
    spawn: BlockPos,
    time: i64,
    time_stopped: bool,
    age_ticks: u64,
    difficulty: Option<crate::config::Difficulty>,
    weather: Weather,
    /// The loaded chunks, in order.
    pub(crate) loaded: BTreeSet<ChunkPos>,
    quarantined: BTreeSet<ChunkPos>,
    changes: BTreeMap<BlockPos, Entry>,
    feed: VecDeque<BlockPos>,
}

crate::tick::checkpoint::fields!(Saved {
    seed,
    spawn,
    time,
    time_stopped,
    age_ticks,
    difficulty,
    weather,
    loaded,
    quarantined,
    changes,
    feed,
});

/// The tick of the day cycle at world clock `time`, 0 to 23,999.
fn tick_of_day(time: i64) -> u32 {
    #[expect(
        clippy::expect_used,
        reason = "invariant: the remainder of a day is below DAY, which fits u32"
    )]
    u32::try_from(time.rem_euclid(DAY)).expect("a time of day fits u32")
}

/// Whether chunk `pos` is inside the world border. The server holds chunks only around centers
/// inside it (spawn chunks inside it, ticket centers), so a rule that adds a few blocks to a
/// loaded position cannot overflow.
pub(crate) fn is_inside_border(pos: ChunkPos) -> bool {
    let limit = (BORDER >> 4).unsigned_abs();
    pos.x.unsigned_abs() <= limit && pos.z.unsigned_abs() <= limit
}

/// Whether a player may stand at `spawn`: the middle of its block is inside the world border on
/// every axis.
pub(crate) fn is_valid_spawn(spawn: BlockPos) -> bool {
    [spawn.x, spawn.y, spawn.z]
        .iter()
        .all(|c| (-BORDER..BORDER).contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::chunk::tests::plain_states;
    use crate::block::{Face, PropertyValue};
    use crate::math::Vec3;
    use crate::nbt::Compound;
    use crate::protocol::Rng;
    use crate::save::palette_entry;

    /// A world with the chunks of x and z from `-r` to `r` loaded from its generator.
    fn world(generator: &dyn Generator, r: i32) -> World {
        let mut w = World::new("w", Kind::Temporary, 3, generator.spawn(3));
        for x in -r..=r {
            for z in -r..=r {
                let pos = ChunkPos::new(x, z);
                let chunk = Chunk::generate(generator, w.seed(), pos);
                assert!(w.insert_chunk(pos, chunk).is_none());
            }
        }
        w
    }

    /// A stored block copies a state Anchor does not know to another chunk with its NBT, listed
    /// once there however often it is set, and its clients are sent it as the chunk sends it.
    #[test]
    fn a_stored_unknown_block_copies_to_another_chunk_listed_once() {
        let nbt = palette_entry("minecraft:no_such_block".to_owned(), Compound::new());
        let mut w = world(&Void, 1);
        let from = BlockPos::new(0, -60, 0);
        let entry = w.put_unknown(from, nbt.clone()).unwrap();
        let Some(StoredBlock::Unknown(read)) = w.stored_block(from) else {
            panic!("an unknown block reads as stored");
        };
        assert_eq!(read.nbt(), &nbt);
        let other = |x| BlockPos::new(x, -60, 0);
        w.take_changes();
        for x in [16, 17] {
            w.set_stored_block(other(x), &StoredBlock::Unknown(read.clone()))
                .unwrap();
        }
        let chunk = w.chunk(ChunkPos::new(1, 0)).unwrap();
        assert_eq!(chunk.unknown(), std::slice::from_ref(&nbt));
        assert_eq!(w.block(other(16)), None);
        let changes = w.take_changes();
        assert_eq!(
            changes.get(&other(17)).map(|e| e.network_id()),
            Some(entry.network_id())
        );
        // Set back in its own chunk, the block reuses its entry.
        w.set_stored_block(other(1), &StoredBlock::Unknown(read))
            .unwrap();
        assert_eq!(w.chunk(ChunkPos::new(0, 0)).unwrap().unknown().len(), 1);
        assert_eq!(
            w.set_stored_block(BlockPos::new(64, -60, 0), &StoredBlock::Known(block::AIR)),
            Err(Error::NotLoaded)
        );
    }

    /// The block the rules see is air for no position, an unloaded or quarantined chunk and a
    /// position outside the world height, bedrock for a state Anchor does not know, and the state
    /// itself for a known one.
    #[test]
    fn rules_see_air_for_absent_bedrock_for_unknown_and_known_as_itself() {
        let nbt = palette_entry("minecraft:no_such_block".to_owned(), Compound::new());
        let mut w = world(&Void, 1);
        let unknown = BlockPos::new(0, -60, 0);
        let known = BlockPos::new(1, -60, 0);
        let quarantined = BlockPos::new(16, -60, 0);
        w.put_unknown(unknown, nbt).unwrap();
        w.set_block(known, block::DIRT).unwrap();
        w.set_block(quarantined, block::DIRT).unwrap();
        w.quarantine(quarantined.chunk());
        assert_eq!(w.block_for_rules(None), block::AIR);
        for absent in [
            BlockPos::new(64, -60, 0),
            quarantined,
            BlockPos::new(1, MAX_Y + 1, 0),
            BlockPos::new(1, MIN_Y - 1, 0),
        ] {
            assert_eq!(w.block_for_rules(Some(absent)), block::AIR, "{absent:?}");
        }
        assert_eq!(w.block_for_rules(Some(unknown)), block::BEDROCK);
        assert_eq!(w.block_for_rules(Some(known)), block::DIRT);
    }

    #[test]
    fn a_point_is_in_the_block_its_coordinates_round_down_to() {
        let at = |x, y, z| BlockPos::containing(Vec3::new(x, y, z));
        assert_eq!(at(0.5, -59.0, 15.99), BlockPos::new(0, -59, 15));
        assert_eq!(at(-0.01, -64.5, -16.0), BlockPos::new(-1, -65, -16));
        assert_eq!(
            at(f64::NAN, 1e300, -1e300),
            BlockPos::new(0, i32::MAX, i32::MIN)
        );
        assert_eq!(at(-0.5, 0.0, 0.0).chunk(), ChunkPos::new(-1, 0));
    }

    /// The center of a block cell is half a block from its lowest corner on every axis, also for
    /// negative coordinates.
    #[test]
    fn the_center_of_a_block_is_half_a_block_from_its_corner() {
        assert_eq!(BlockPos::new(0, 0, 0).to_center(), Vec3::new(0.5, 0.5, 0.5));
        assert_eq!(
            BlockPos::new(-1, -64, 16).to_center(),
            Vec3::new(-0.5, -63.5, 16.5)
        );
        assert_eq!(
            BlockPos::new(-1, -64, 16).to_corner(),
            Vec3::new(-1.0, -64.0, 16.0)
        );
        assert_eq!(
            BlockPos::containing(BlockPos::new(-7, 319, -30).to_center()),
            BlockPos::new(-7, 319, -30)
        );
    }

    // api: World::getBlock, World::getBlockAt, World::setBlock, World::setBlockAt, World
    #[test]
    fn random_sets_over_chunks_match_an_array() {
        let states = plain_states();
        let mut w = world(&Void, 1);
        let mut rng = Rng(0x00e1_0001);
        // x and z from -16 to 31 (the nine loaded chunks), y over the whole height.
        let mut model = BTreeMap::new();
        for round in 0..100_000_usize {
            let pool = if (round / 10_000) % 2 == 0 {
                states.len()
            } else {
                4
            };
            let pos = BlockPos::new(
                i32::try_from(rng.below(48)).unwrap() - 16,
                MIN_Y + i32::try_from(rng.below(384)).unwrap(),
                i32::try_from(rng.below(48)).unwrap() - 16,
            );
            let state = if rng.below(3) == 0 {
                block::AIR
            } else {
                states[rng.below(pool)]
            };
            w.set_block(pos, state).unwrap();
            model.insert(pos, state);
            if round % 9973 == 0 {
                let probe = BlockPos::new(pos.x, MIN_Y, pos.z);
                let top = (MIN_Y..=MAX_Y).rev().find(|&y| {
                    model
                        .get(&BlockPos::new(probe.x, y, probe.z))
                        .is_some_and(|&s| s != block::AIR)
                });
                assert_eq!(w.highest_block(pos.x, pos.z), top);
            }
        }
        for (&pos, &state) in &model {
            assert_eq!(w.block(pos), Some(state));
        }
        let changes = w.take_changes();
        assert!(
            changes
                .iter()
                .all(|(p, e)| model.get(p).map(|&s| Entry::Known(s)) == Some(*e))
        );
        assert!(w.take_changes().is_empty());
    }

    // api: World::getChunk, Position::getSide
    #[test]
    fn loaded_chunks_read_through_the_world_and_sides_are_one_block_away() {
        let w = world(&Flat, 0);
        let chunk = w.chunk(ChunkPos::new(0, 0)).unwrap();
        for (x, y, z) in [(3, -61, 5), (0, -64, 15), (15, 40, 0)] {
            let pos = BlockPos::new(i32::from(x), y, i32::from(z));
            assert_eq!(chunk.block(x, y, z), w.block(pos));
        }
        assert!(w.chunk(ChunkPos::new(1, 0)).is_none(), "not loaded");
        let p = BlockPos::new(1, 2, 3);
        let sides: Vec<_> = Face::all().iter().map(|&f| p.step(f).unwrap()).collect();
        assert_eq!(
            sides,
            [
                BlockPos::new(1, 1, 3),
                BlockPos::new(1, 3, 3),
                BlockPos::new(1, 2, 2),
                BlockPos::new(1, 2, 4),
                BlockPos::new(0, 2, 3),
                BlockPos::new(2, 2, 3),
            ]
        );
        assert_eq!(BlockPos::new(i32::MAX, 0, 0).step(Face::East), None);
        assert_eq!(BlockPos::new(0, i32::MIN, 0).step(Face::Down), None);
    }

    // api: World::getSeed, World::getTime, World::getTimeOfDay
    #[test]
    fn the_seed_and_the_day_cycle_are_the_worlds() {
        let mut w = world(&Flat, 0);
        assert_eq!(w.seed(), 3);
        assert_eq!((w.time_ticks(), w.time_of_day()), (0, 0));
        w.set_time(30_000);
        assert_eq!((w.time_ticks(), w.time_of_day()), (30_000, 6000));
        w.set_time(-1);
        assert_eq!(w.time_of_day(), 23_999);
    }

    // api: World::isInWorld, World::getMinY, World::getMaxY, World::getLoadedChunks,
    // api: World::getBiomeId, World::getBiome, World::setBiomeId, World::getHighestBlockAt,
    // api: World::isChunkLoaded
    #[test]
    fn heights_loaded_chunks_and_biomes_answer_for_loaded_chunks_only() {
        assert!(World::is_in_world(BlockPos::new(i32::MAX, MIN_Y, i32::MIN)));
        assert!(World::is_in_world(BlockPos::new(0, MAX_Y, 0)));
        assert!(!World::is_in_world(BlockPos::new(0, MAX_Y + 1, 0)));
        assert!(!World::is_in_world(BlockPos::new(0, MIN_Y - 1, 0)));
        let mut w = world(&Flat, 1);
        let loaded: Vec<ChunkPos> = w.loaded_chunks().collect();
        assert_eq!(loaded.len(), 9);
        assert!(loaded.iter().all(|&p| w.is_chunk_loaded(p)));
        assert!(loaded.windows(2).all(|p| p[0] < p[1]), "in position order");
        let at = BlockPos::new(3, 10, -5);
        let plains = w.biome(at).unwrap();
        assert_eq!(w.highest_block(3, -5), Some(-61));
        w.take_dirty();
        w.set_biome(at, plains).unwrap();
        assert!(w.take_dirty().is_empty(), "the same biome changes nothing");
        w.set_biome(at, 2).unwrap();
        assert_eq!(w.biome(at), Some(2));
        assert_eq!(w.take_dirty().len(), 1, "saved with the chunk");
        assert_eq!(
            w.set_biome(BlockPos::new(0, 400, 0), 2),
            Err(Error::OutOfRange)
        );
        assert_eq!(
            w.set_biome(BlockPos::new(1000, 0, 0), 2),
            Err(Error::NotLoaded)
        );
        assert_eq!(w.biome(BlockPos::new(1000, 0, 0)), None);
    }

    #[test]
    fn errors_name_the_reason() {
        let mut w = world(&Flat, 0);
        let stone = State::find("minecraft:stone", &[]).unwrap();
        assert_eq!(
            w.set_block(BlockPos::new(0, MAX_Y + 1, 0), stone),
            Err(Error::OutOfRange)
        );
        assert_eq!(
            w.set_block(BlockPos::new(0, MIN_Y - 1, 0), stone),
            Err(Error::OutOfRange)
        );
        assert_eq!(
            w.set_block(BlockPos::new(16, 0, 0), stone),
            Err(Error::NotLoaded)
        );
        assert_eq!(w.block(BlockPos::new(16, 0, 0)), None);
        w.quarantine(ChunkPos::new(1, 0));
        assert_eq!(
            w.set_block(BlockPos::new(16, 0, 0), stone),
            Err(Error::Quarantined)
        );
        assert!(w.is_quarantined(ChunkPos::new(1, 0)));
        assert!(!w.is_chunk_loaded(ChunkPos::new(1, 0)));
        assert_eq!(w.quarantined().collect::<Vec<_>>(), [ChunkPos::new(1, 0)]);
        assert!(w.remove_chunk(ChunkPos::new(1, 0)).is_none());
        assert!(!w.is_quarantined(ChunkPos::new(1, 0)));
        assert_eq!(
            w.block(BlockPos::new(-1, -64, -1)),
            None,
            "chunk (-1, -1) not loaded"
        );
        assert_eq!(w.block(BlockPos::new(15, -64, 15)), Some(block::BEDROCK));
        assert_eq!(w.spawn(), BlockPos::new(0, -60, 0));
        assert_eq!(Error::NotLoaded.to_string(), "chunk is not loaded");
        assert_eq!(Error::Malformed("x").to_string(), "malformed chunk data: x");
    }

    #[test]
    fn the_full_error_names_every_limit_it_stands_for() {
        assert_eq!(
            Error::Full.to_string(),
            "world, chunk ticket, chunk unknown state or lightning bolt limit reached"
        );
    }

    #[test]
    fn the_save_snapshot_keeps_the_state_of_its_tick() {
        let mut w = world(&Flat, 1);
        assert!(w.take_dirty().is_empty(), "generated chunks are clean");
        let stone = State::find("minecraft:stone", &[]).unwrap();
        let a = BlockPos::new(1, 10, 1);
        w.set_block(a, stone).unwrap();
        w.set_block(BlockPos::new(-1, 10, -1), stone).unwrap();
        let snapshot = w.take_dirty();
        let positions: Vec<_> = snapshot.iter().map(|(p, _)| *p).collect();
        assert_eq!(positions, [ChunkPos::new(-1, -1), ChunkPos::new(0, 0)]);
        assert!(w.take_dirty().is_empty());
        w.set_block(a, block::AIR).unwrap();
        let (_, chunk) = &snapshot[1];
        assert_eq!(chunk.block(1, 10, 1), Some(stone));
        assert_eq!(w.block(a), Some(block::AIR));
        assert_eq!(w.take_dirty().len(), 1);
        // Setting a block to its own state changes nothing.
        w.set_block(a, block::AIR).unwrap();
        assert!(w.take_dirty().is_empty());
        assert_eq!(w.loaded_chunks().count(), 9);
        assert!(w.remove_chunk(ChunkPos::new(0, 0)).is_some());
        assert_eq!(w.loaded_chunks().count(), 8);
    }

    /// A state written as the P6 report writes it: `minecraft:name[key=value,...]` with the keys
    /// in name order and without their `minecraft:` prefix.
    fn render(state: State) -> String {
        let properties: Vec<String> = state
            .properties()
            .map(|p| {
                let value = match p.value() {
                    PropertyValue::Byte(b) => b.to_string(),
                    PropertyValue::Int(i) => i.to_string(),
                    PropertyValue::String(s) => s.to_owned(),
                };
                let key = p.name().strip_prefix("minecraft:").unwrap_or(p.name());
                format!("{key}={value}")
            })
            .collect();
        format!("{}[{}]", state.name(), properties.join(","))
    }

    /// A step of [`OFFICIAL_STEPS`]: where the block goes (dx, dz), its state, and the last state
    /// the official server sent for each position it updated.
    type Step = (
        (i32, i32),
        &'static str,
        &'static [((i32, i32), &'static str)],
    );

    /// The official server's P6 `blocks` scenario (`xtask/src/p6.rs` `BLOCK_STEPS`): the block
    /// placed at (dx, dz) from the base, and the last state the official server sent for each
    /// position it updated in that step.
    const OFFICIAL_STEPS: &[Step] = &[
        (
            (0, 0),
            "minecraft:oak_fence[connection_east=0,connection_north=0,connection_south=0,connection_west=0]",
            &[(
                (0, 0),
                "minecraft:oak_fence[connection_east=0,connection_north=0,connection_south=0,connection_west=0]",
            )],
        ),
        (
            (1, 0),
            "minecraft:oak_fence[connection_east=0,connection_north=0,connection_south=0,connection_west=0]",
            &[
                (
                    (1, 0),
                    "minecraft:oak_fence[connection_east=0,connection_north=0,connection_south=0,connection_west=1]",
                ),
                (
                    (0, 0),
                    "minecraft:oak_fence[connection_east=1,connection_north=0,connection_south=0,connection_west=0]",
                ),
            ],
        ),
        (
            (0, 3),
            "minecraft:stone[]",
            &[((0, 3), "minecraft:stone[]")],
        ),
        (
            (1, 3),
            "minecraft:oak_fence[connection_east=0,connection_north=0,connection_south=0,connection_west=0]",
            &[(
                (1, 3),
                "minecraft:oak_fence[connection_east=0,connection_north=0,connection_south=0,connection_west=1]",
            )],
        ),
        (
            (4, 3),
            "minecraft:oak_fence[connection_east=0,connection_north=0,connection_south=0,connection_west=0]",
            &[(
                (4, 3),
                "minecraft:oak_fence[connection_east=0,connection_north=0,connection_south=0,connection_west=0]",
            )],
        ),
        (
            (5, 3),
            "minecraft:stone[]",
            &[
                ((5, 3), "minecraft:stone[]"),
                (
                    (4, 3),
                    "minecraft:oak_fence[connection_east=1,connection_north=0,connection_south=0,connection_west=0]",
                ),
            ],
        ),
        (
            (0, 6),
            "minecraft:glass_pane[connection_east=0,connection_north=0,connection_south=0,connection_west=0]",
            &[(
                (0, 6),
                "minecraft:glass_pane[connection_east=0,connection_north=0,connection_south=0,connection_west=0]",
            )],
        ),
        (
            (1, 6),
            "minecraft:glass_pane[connection_east=0,connection_north=0,connection_south=0,connection_west=0]",
            &[
                (
                    (1, 6),
                    "minecraft:glass_pane[connection_east=0,connection_north=0,connection_south=0,connection_west=1]",
                ),
                (
                    (0, 6),
                    "minecraft:glass_pane[connection_east=1,connection_north=0,connection_south=0,connection_west=0]",
                ),
            ],
        ),
        (
            (0, 7),
            "minecraft:glass_pane[connection_east=0,connection_north=0,connection_south=0,connection_west=0]",
            &[
                (
                    (0, 7),
                    "minecraft:glass_pane[connection_east=0,connection_north=1,connection_south=0,connection_west=0]",
                ),
                (
                    (0, 6),
                    "minecraft:glass_pane[connection_east=1,connection_north=0,connection_south=1,connection_west=0]",
                ),
            ],
        ),
        (
            (0, 10),
            "minecraft:iron_bars[connection_east=0,connection_north=0,connection_south=0,connection_west=0]",
            &[(
                (0, 10),
                "minecraft:iron_bars[connection_east=0,connection_north=0,connection_south=0,connection_west=0]",
            )],
        ),
        (
            (1, 10),
            "minecraft:iron_bars[connection_east=0,connection_north=0,connection_south=0,connection_west=0]",
            &[
                (
                    (1, 10),
                    "minecraft:iron_bars[connection_east=0,connection_north=0,connection_south=0,connection_west=1]",
                ),
                (
                    (0, 10),
                    "minecraft:iron_bars[connection_east=1,connection_north=0,connection_south=0,connection_west=0]",
                ),
            ],
        ),
        (
            (2, 10),
            "minecraft:iron_bars[connection_east=0,connection_north=0,connection_south=0,connection_west=0]",
            &[
                (
                    (2, 10),
                    "minecraft:iron_bars[connection_east=0,connection_north=0,connection_south=0,connection_west=1]",
                ),
                (
                    (1, 10),
                    "minecraft:iron_bars[connection_east=1,connection_north=0,connection_south=0,connection_west=1]",
                ),
            ],
        ),
        (
            (3, 10),
            "minecraft:copper_bars[connection_east=0,connection_north=0,connection_south=0,connection_west=0]",
            &[
                (
                    (3, 10),
                    "minecraft:copper_bars[connection_east=0,connection_north=0,connection_south=0,connection_west=1]",
                ),
                (
                    (2, 10),
                    "minecraft:iron_bars[connection_east=1,connection_north=0,connection_south=0,connection_west=1]",
                ),
            ],
        ),
        (
            (0, 13),
            "minecraft:oak_stairs[corner=none,upside_down_bit=0,weirdo_direction=0]",
            &[(
                (0, 13),
                "minecraft:oak_stairs[corner=none,upside_down_bit=0,weirdo_direction=0]",
            )],
        ),
        (
            (1, 13),
            "minecraft:oak_stairs[corner=none,upside_down_bit=0,weirdo_direction=2]",
            &[
                (
                    (1, 13),
                    "minecraft:oak_stairs[corner=none,upside_down_bit=0,weirdo_direction=2]",
                ),
                (
                    (0, 13),
                    "minecraft:oak_stairs[corner=outer_right,upside_down_bit=0,weirdo_direction=0]",
                ),
            ],
        ),
        (
            (1, 16),
            "minecraft:oak_stairs[corner=none,upside_down_bit=0,weirdo_direction=0]",
            &[(
                (1, 16),
                "minecraft:oak_stairs[corner=none,upside_down_bit=0,weirdo_direction=0]",
            )],
        ),
        (
            (0, 16),
            "minecraft:oak_stairs[corner=none,upside_down_bit=0,weirdo_direction=2]",
            &[
                (
                    (0, 16),
                    "minecraft:oak_stairs[corner=none,upside_down_bit=0,weirdo_direction=2]",
                ),
                (
                    (1, 16),
                    "minecraft:oak_stairs[corner=inner_right,upside_down_bit=0,weirdo_direction=0]",
                ),
            ],
        ),
        (
            (0, 19),
            "minecraft:oak_stairs[corner=none,upside_down_bit=0,weirdo_direction=0]",
            &[(
                (0, 19),
                "minecraft:oak_stairs[corner=none,upside_down_bit=0,weirdo_direction=0]",
            )],
        ),
        (
            (0, 22),
            "minecraft:stone[]",
            &[((0, 22), "minecraft:stone[]")],
        ),
        (
            (4, 22),
            "minecraft:stone[]",
            &[((4, 22), "minecraft:stone[]")],
        ),
        (
            (1, 22),
            "minecraft:tripwire_hook[attached_bit=0,direction=3,powered_bit=0]",
            &[(
                (1, 22),
                "minecraft:tripwire_hook[attached_bit=0,direction=3,powered_bit=0]",
            )],
        ),
        (
            (3, 22),
            "minecraft:tripwire_hook[attached_bit=0,direction=1,powered_bit=0]",
            &[(
                (3, 22),
                "minecraft:tripwire_hook[attached_bit=0,direction=1,powered_bit=0]",
            )],
        ),
        (
            (2, 22),
            "minecraft:trip_wire[attached_bit=0,disarmed_bit=0,connection_east=0,connection_north=0,connection_south=0,connection_west=0,powered_bit=0,suspended_bit=0]",
            &[
                (
                    (2, 22),
                    "minecraft:trip_wire[attached_bit=1,disarmed_bit=0,connection_east=1,connection_north=0,connection_south=0,connection_west=1,powered_bit=0,suspended_bit=1]",
                ),
                (
                    (3, 22),
                    "minecraft:tripwire_hook[attached_bit=1,direction=1,powered_bit=0]",
                ),
                (
                    (1, 22),
                    "minecraft:tripwire_hook[attached_bit=1,direction=3,powered_bit=0]",
                ),
            ],
        ),
        (
            (6, 22),
            "minecraft:trip_wire[attached_bit=0,disarmed_bit=0,connection_east=0,connection_north=0,connection_south=0,connection_west=0,powered_bit=0,suspended_bit=0]",
            &[(
                (6, 22),
                "minecraft:trip_wire[attached_bit=0,disarmed_bit=0,connection_east=0,connection_north=0,connection_south=0,connection_west=0,powered_bit=0,suspended_bit=1]",
            )],
        ),
        (
            (7, 22),
            "minecraft:trip_wire[attached_bit=0,disarmed_bit=0,connection_east=0,connection_north=0,connection_south=0,connection_west=0,powered_bit=0,suspended_bit=0]",
            &[
                (
                    (7, 22),
                    "minecraft:trip_wire[attached_bit=0,disarmed_bit=0,connection_east=0,connection_north=0,connection_south=0,connection_west=1,powered_bit=0,suspended_bit=1]",
                ),
                (
                    (6, 22),
                    "minecraft:trip_wire[attached_bit=0,disarmed_bit=0,connection_east=1,connection_north=0,connection_south=0,connection_west=0,powered_bit=0,suspended_bit=1]",
                ),
            ],
        ),
    ];

    #[test]
    fn connections_match_every_official_server_step() {
        let by_text: BTreeMap<String, State> = State::all().map(|s| (render(s), s)).collect();
        let mut w = world(&Flat, 2);
        // The P6 base: the spawn column plus 2, on the grass.
        let base = BlockPos::new(2, -60, 2);
        let at = |(dx, dz): (i32, i32)| BlockPos::new(base.x + dx, base.y, base.z + dz);
        for (n, (placed, input, expected)) in OFFICIAL_STEPS.iter().enumerate() {
            let before: BTreeMap<BlockPos, String> = expected
                .iter()
                .map(|(d, _)| (at(*d), render(w.block(at(*d)).unwrap())))
                .collect();
            let state = by_text[*input];
            w.set_block(at(*placed), state).unwrap();
            let changes = w.take_changes();
            let mut want_changed = BTreeSet::new();
            for (d, official) in *expected {
                let got = render(w.block(at(*d)).unwrap());
                assert_eq!(got, *official, "step {n} at {d:?}");
                if before[&at(*d)] != *official {
                    want_changed.insert(at(*d));
                }
            }
            let positions: BTreeSet<BlockPos> = changes.keys().copied().collect();
            assert_eq!(positions, want_changed, "step {n}: changed positions");
            for (pos, state) in &changes {
                assert_eq!(w.entry(*pos), Some(*state), "step {n}: last state");
            }
        }
    }

    fn by_name(name: &str) -> State {
        State::all()
            .find(|s| {
                s.name() == name && s.properties().all(|p| p.value() == PropertyValue::Byte(0))
            })
            .unwrap()
    }

    fn prop(state: State, key: &str) -> PropertyValue<'static> {
        state
            .properties()
            .find(|p| p.name() == key)
            .unwrap()
            .value()
    }

    /// The connection rules read a block with no behaviour of its own by its full faces: a fence
    /// joins a moss block's full face but not a moss carpet, and neither changes.
    #[test]
    fn fences_connect_to_a_solid_moss_block_only() {
        let fence = BlockPos::new(0, -60, 0);
        let beside = BlockPos::new(1, -60, 0);
        for (name, joined) in [("minecraft:moss_block", 1), ("minecraft:moss_carpet", 0)] {
            let mut w = world(&Flat, 1);
            let state = by_name(name);
            w.set_block(beside, state).unwrap();
            w.set_block(fence, by_name("minecraft:oak_fence")).unwrap();
            let east = prop(w.block(fence).unwrap(), "minecraft:connection_east");
            assert_eq!(east, PropertyValue::Byte(joined), "{name}");
            assert_eq!(w.block(beside), Some(state), "{name}");
        }
    }

    /// An attached line of 30 tripwire that a 31st block joins to 29 more: the whole line is now
    /// too long, and every tripwire and hook of it is unattached, also those more than
    /// `MAX_TRIPWIRE` blocks from the joining block.
    #[test]
    fn joining_an_attached_line_past_the_longest_unattaches_all_of_it() {
        let by_text: BTreeMap<String, State> = State::all().map(|s| (render(s), s)).collect();
        let hook = |direction: u8| {
            by_text[&format!(
                "minecraft:tripwire_hook[attached_bit=0,direction={direction},powered_bit=0]"
            )]
        };
        let wire = by_text["minecraft:trip_wire[attached_bit=0,disarmed_bit=0,connection_east=0,connection_north=0,connection_south=0,connection_west=0,powered_bit=0,suspended_bit=0]"];
        let mut w = world(&Flat, 4);
        let at = |x: i32| BlockPos::new(x, -60, 2);
        let attached = |w: &World, x: i32| prop(w.block(at(x)).unwrap(), "attached_bit");
        // A hook facing east, tripwire, a hook facing west.
        w.set_block(at(0), hook(3)).unwrap();
        for x in 1..=30 {
            w.set_block(at(x), wire).unwrap();
        }
        w.set_block(at(31), hook(1)).unwrap();
        for x in 32..=60 {
            w.set_block(at(x), wire).unwrap();
        }
        for x in 0..=31 {
            assert_eq!(attached(&w, x), PropertyValue::Byte(1), "{x}");
        }
        w.set_block(at(31), wire).unwrap();
        for x in 0..=60 {
            assert_eq!(attached(&w, x), PropertyValue::Byte(0), "{x}");
        }
    }

    fn find(name: &str, props: &[(&str, PropertyValue<'_>)]) -> State {
        State::find(name, props).unwrap()
    }

    /// A closed oak fence gate facing `facing`, not in a wall.
    fn gate(facing: &str) -> State {
        State::all()
            .find(|s| {
                s.name() == "minecraft:fence_gate"
                    && prop(*s, "minecraft:cardinal_direction") == PropertyValue::String(facing)
                    && prop(*s, "open_bit") == PropertyValue::Byte(0)
                    && prop(*s, "in_wall_bit") == PropertyValue::Byte(0)
            })
            .unwrap()
    }

    /// Walls (the official server's P6 run for the post, the API table for a block above), fence
    /// gates and panes next to walls.
    #[test]
    fn walls_and_fence_gates_follow_the_official_server_and_the_dump() {
        let none = PropertyValue::String("none");
        let wall = find(
            "minecraft:cobblestone_wall",
            &[
                ("wall_connection_type_east", none),
                ("wall_connection_type_north", none),
                ("wall_connection_type_south", none),
                ("wall_connection_type_west", none),
                ("wall_post_bit", PropertyValue::Byte(0)),
            ],
        );
        let mut w = world(&Flat, 1);
        let p = BlockPos::new(4, -60, 4);
        w.set_block(p, wall).unwrap();
        let post = |w: &World, at| prop(w.block(at).unwrap(), "wall_post_bit");
        // A lone wall: no connections, a post.
        assert_eq!(post(&w, p), PropertyValue::Byte(1));
        let east = BlockPos::new(5, -60, 4);
        w.set_block(east, wall).unwrap();
        let short = PropertyValue::String("short");
        assert_eq!(
            prop(w.block(p).unwrap(), "wall_connection_type_east"),
            short
        );
        assert_eq!(
            prop(w.block(east).unwrap(), "wall_connection_type_west"),
            short
        );
        // A line's end keeps its post; its middle, running straight through, has none.
        assert_eq!(post(&w, p), PropertyValue::Byte(1));
        let west = BlockPos::new(3, -60, 4);
        w.set_block(west, wall).unwrap();
        assert_eq!(post(&w, p), PropertyValue::Byte(0));
        // A block above makes the post.
        let stone = find("minecraft:stone", &[]);
        w.set_block(BlockPos::new(4, -59, 4), stone).unwrap();
        assert_eq!(post(&w, p), PropertyValue::Byte(1));
        w.set_block(BlockPos::new(4, -59, 4), block::AIR).unwrap();
        assert_eq!(post(&w, p), PropertyValue::Byte(0));
        w.set_block(west, block::AIR).unwrap();
        // A pane next to a wall connects, and the wall connects to it.
        let pane = by_name("minecraft:glass_pane");
        let north = BlockPos::new(4, -60, 3);
        w.set_block(north, pane).unwrap();
        assert_eq!(
            prop(w.block(north).unwrap(), "minecraft:connection_south"),
            PropertyValue::Byte(1)
        );
        assert_eq!(
            prop(w.block(p).unwrap(), "wall_connection_type_north"),
            short
        );
        // A fence gate between walls is in the wall; facing them it is not.
        let at_gate = BlockPos::new(6, -60, 4);
        w.set_block(at_gate, gate("south")).unwrap();
        assert_eq!(
            prop(w.block(at_gate).unwrap(), "in_wall_bit"),
            PropertyValue::Byte(1)
        );
        assert_eq!(
            prop(w.block(east).unwrap(), "wall_connection_type_east"),
            short
        );
        w.set_block(at_gate, gate("east")).unwrap();
        assert_eq!(
            prop(w.block(at_gate).unwrap(), "in_wall_bit"),
            PropertyValue::Byte(0)
        );
    }

    /// The fence groups, fences next to gates, and a stair corner that changes a fence's solid
    /// face.
    #[test]
    fn fences_and_stairs_connect_by_their_groups() {
        let mut w = world(&Flat, 1);
        let none = PropertyValue::String("none");
        // A nether brick fence and an oak fence do not connect either way.
        let oak = by_name("minecraft:oak_fence");
        let nether = by_name("minecraft:nether_brick_fence");
        let at_oak = BlockPos::new(0, -60, 10);
        let at_nether = BlockPos::new(1, -60, 10);
        w.set_block(at_oak, oak).unwrap();
        w.set_block(at_nether, nether).unwrap();
        assert_eq!(w.block(at_oak), Some(oak));
        assert_eq!(w.block(at_nether), Some(nether));
        // An oak fence connects to a fence gate in any direction.
        w.set_block(BlockPos::new(0, -60, 11), gate("east"))
            .unwrap();
        assert_eq!(
            prop(w.block(at_oak).unwrap(), "minecraft:connection_south"),
            PropertyValue::Byte(1)
        );
        // A stair turned into an inner corner by a new stair in front of it gains a full side
        // face, so the fence on that side connects: the second round of the update (the fence
        // is not a neighbour of the new stair).
        let stairs = |dir: i32| {
            find(
                "minecraft:oak_stairs",
                &[
                    ("minecraft:corner", none),
                    ("upside_down_bit", PropertyValue::Byte(0)),
                    ("weirdo_direction", PropertyValue::Int(dir)),
                ],
            )
        };
        let stair = BlockPos::new(0, -60, 20);
        let fence = BlockPos::new(0, -60, 21);
        w.set_block(stair, stairs(0)).unwrap();
        w.set_block(fence, oak).unwrap();
        let north = |w: &World| prop(w.block(fence).unwrap(), "minecraft:connection_north");
        assert_eq!(
            north(&w),
            PropertyValue::Byte(0),
            "a straight stair's side is not full"
        );
        w.set_block(BlockPos::new(-1, -60, 20), stairs(2)).unwrap();
        assert_eq!(
            prop(w.block(stair).unwrap(), "minecraft:corner"),
            PropertyValue::String("inner_right")
        );
        assert_eq!(north(&w), PropertyValue::Byte(1));
        w.set_block(BlockPos::new(-1, -60, 20), block::AIR).unwrap();
        assert_eq!(prop(w.block(stair).unwrap(), "minecraft:corner"), none);
        assert_eq!(north(&w), PropertyValue::Byte(0));
    }

    /// The crate API the tick and storage use: a chunk to parts and back, its network form and
    /// heights.
    #[test]
    fn a_loaded_chunk_converts_to_parts_and_back() {
        let mut w = world(&Flat, 0);
        let stone = State::find("minecraft:stone", &[]).unwrap();
        w.set_block(BlockPos::new(3, 40, 5), stone).unwrap();
        let chunk = w.chunk(ChunkPos::new(0, 0)).unwrap();
        let heights = chunk.heights();
        assert_eq!(heights[5 * 16 + 3], 40);
        assert_eq!(heights[0], -61);
        assert!(heights.iter().all(|&h| h != NO_HEIGHT));
        let subs: [Option<SubChunk>; SUB_CHUNKS] = std::array::from_fn(|i| {
            chunk.sub_chunk(i).map(|s| {
                SubChunk::from_layers(
                    s.layers()
                        .clone()
                        .map(|l| Paletted::from_parts(l.bits(), l.words(), l.palette()).unwrap()),
                )
            })
        });
        let biomes: [Paletted<u32>; SUB_CHUNKS] =
            std::array::from_fn(|_| Paletted::uniform(PLAINS));
        let back = Chunk::from_parts(subs, biomes, Vec::new()).unwrap();
        assert_eq!(back.block(3, 40, 5), Some(stone));
        assert_eq!(back.entry(3, 41, 5), Some(Entry::AIR));
        assert_eq!(back.sub_chunk_limit(), 7);
        assert_eq!(
            back.to_level_chunk_payload(),
            chunk.to_level_chunk_payload()
        );
        assert_eq!(
            w.insert_chunk(ChunkPos::new(0, 0), back)
                .map(|c| c.is_dirty()),
            Some(true)
        );
        assert_eq!(w.block(BlockPos::new(3, 40, 5)), Some(stone));
    }

    /// A fixed multiplicative hash (rustc's Fx), for the index measurement.
    #[derive(Default)]
    struct Fx(u64);

    impl std::hash::Hasher for Fx {
        fn finish(&self) -> u64 {
            self.0
        }
        fn write(&mut self, bytes: &[u8]) {
            for &b in bytes {
                self.write_u64(u64::from(b));
            }
        }
        fn write_i32(&mut self, i: i32) {
            self.write_u64(u64::from(i.cast_unsigned()));
        }
        fn write_u64(&mut self, i: u64) {
            self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }

    /// Measures `block` and `set_block`:
    /// `cargo test --release --lib world::tests::block_access_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement"]
    #[expect(
        clippy::print_stderr,
        clippy::disallowed_methods,
        reason = "consistency: a measurement prints its result and reads the wall clock"
    )]
    fn block_access_cost() {
        let states = plain_states();
        for r in [2, 8, 16] {
            let mut w = world(&Flat, r);
            let mut rng = Rng(0x00e1_0002);
            let span = usize::try_from(32 * r).unwrap();
            let positions: Vec<BlockPos> = (0..1_000_000)
                .map(|_| {
                    BlockPos::new(
                        i32::try_from(rng.below(span)).unwrap() - 16 * r,
                        -64 + i32::try_from(rng.below(64)).unwrap(),
                        i32::try_from(rng.below(span)).unwrap() - 16 * r,
                    )
                })
                .collect();
            let start = std::time::Instant::now();
            let mut found = 0_usize;
            for &p in &positions {
                found += usize::from(w.block(p).is_some());
            }
            let get = start.elapsed();
            let start = std::time::Instant::now();
            for (i, &p) in positions.iter().enumerate() {
                w.set_block(p, states[i % 64]).unwrap();
                if i % 1000 == 0 {
                    w.take_changes();
                }
            }
            let set = start.elapsed();
            // Again over the same positions: the sub-chunks exist and their palettes are wide.
            let start = std::time::Instant::now();
            for (i, &p) in positions.iter().enumerate() {
                w.set_block(p, states[(i + 7) % 64]).unwrap();
                if i % 1000 == 0 {
                    w.take_changes();
                }
            }
            let again = start.elapsed();
            // The same lookups through a hash map with a fixed hash, for comparison.
            #[expect(
                clippy::disallowed_types,
                reason = "consistency: the measurement compares the index with a hash map"
            )]
            let map: std::collections::HashMap<
                ChunkPos,
                u8,
                std::hash::BuildHasherDefault<Fx>,
            > = w.loaded_chunks().map(|p| (p, 0)).collect();
            let tree: BTreeMap<ChunkPos, u8> = w.loaded_chunks().map(|p| (p, 0)).collect();
            let start = std::time::Instant::now();
            let mut hits = 0_usize;
            for &p in &positions {
                hits += usize::from(map.contains_key(&p.chunk()));
            }
            let hash = start.elapsed();
            let start = std::time::Instant::now();
            for &p in &positions {
                hits += usize::from(tree.contains_key(&p.chunk()));
            }
            let btree = start.elapsed();
            eprintln!(
                "  lookup only: hash {:.1} ns, btree {:.1} ns ({hits})",
                hash.as_secs_f64() * 1e9 / 1e6,
                btree.as_secs_f64() * 1e9 / 1e6
            );
            eprintln!(
                "chunks {}: block {:.1} ns, set_block {:.1} ns (filling), {:.1} ns (again) ({found})",
                w.loaded_chunks().count(),
                get.as_secs_f64() * 1e9 / 1e6,
                set.as_secs_f64() * 1e9 / 1e6,
                again.as_secs_f64() * 1e9 / 1e6,
            );
        }
    }
}
