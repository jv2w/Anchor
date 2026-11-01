//! The tick loop, the tick step and the game state.
//!
//! One thread owns [`State`] and changes it only in [`step`], once per tick, from the inputs the
//! sessions queued and the answers of the save thread. Outputs leave through each session's bounded
//! queue and the save thread's request queue.

mod block_entity;
mod chat;
pub(crate) mod checkpoint;
mod command;
mod dispatch;
mod emit;
mod entity;
mod extra;
mod form;
mod hook;
mod interact;
mod inventory;
mod log;
mod network;
mod player;
pub(crate) mod queue;
mod rules;
mod save;
mod spawn;
mod survival;
mod task;
mod trade;
mod update;
mod view;
mod weather;
mod world_api;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use tokio::sync::mpsc::error::{TryRecvError, TrySendError};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, trace, warn};

use crate::clock;
use crate::config::{Config, CriticalHits, Difficulty, GameMode, Lighting, ViewLimits};
use crate::event::Listeners;
use crate::math::Vec3;
use crate::pool::Pool;
use crate::protocol::Uuid;
use crate::protocol::consts::DisconnectReason;
use crate::protocol::packet::{Disconnect, Packet};
use crate::random::Fnv;
use crate::save::{Request, Response, WorldIndex};
use crate::storage::PlayerKey as StoredPlayer;
use crate::world::World;

pub(crate) use self::log::{
    Answer, EntryKind, Fingerprints, InputLog, MAX_FILE_BYTES as MAX_LOG_FILE_BYTES, Origin,
    Segment, Tick as LoggedTick, write_crash_log,
};
#[cfg(test)]
pub(crate) use entity::Customs;
pub(crate) use entity::EntityTypes;
pub(crate) use entity::bounded_motion;
pub use entity::{Entity, MAX_ENTITIES_PER_WORLD, MAX_NAME_TAG_BYTES, Mut};
pub use hook::SentPacket;
use queue::{Identity, Input, InputKind, InputReceiver, Message, Outbound, SessionId};
pub(crate) use rules::ChunkRules;
pub(crate) use spawn::{MAX_LIST_ENTRY_BYTES, Shared};
pub(crate) use task::Tasks;
pub use task::{Context, Offline};
pub use trade::check_offers;
pub use view::{MAX_TICKET_CHUNKS, MAX_TICKET_RADIUS, MAX_TICKETS};

/// The length of a tick at [`Context::TICKS_PER_SECOND`].
pub(crate) const TICK: Duration = Duration::from_millis(1000 / Context::TICKS_PER_SECOND as u64);
/// The most inputs one tick applies; the rest wait for the next tick. The batch buffer is
/// allocated once at this size.
pub(crate) const MAX_INPUTS_PER_TICK: usize = 8_192;
/// Save thread answers one tick applies; the rest wait for the next tick.
pub(crate) const MAX_RESPONSES_PER_TICK: usize = 1024;
/// Light work per world per tick, in blocks looked at: about 1 to 1.4 ms.
const LIGHT_BUDGET: u32 = 16_384;
/// The largest coordinate a client may report: the world border.
const MAX_COORDINATE: i32 = crate::world::BORDER;

/// Whether a player position is finite and inside the world border on every axis.
fn inside_border(pos: Vec3) -> bool {
    let border = f64::from(MAX_COORDINATE);
    pos.is_finite() && [pos.x, pos.y, pos.z].iter().all(|c| c.abs() <= border)
}

/// The settings the tick reads, taken from [`Config`] once at start.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    motd: String,
    game_mode: GameMode,
    difficulty: Difficulty,
    hardcore: bool,
    /// The view values of a world without `[worlds.<name>]` values.
    view: ViewLimits,
    /// The global view values as the keys hold them, which a world made with its own values
    /// starts from ([`Config::global_view`]).
    global_view: ViewLimits,
    /// The view values of the worlds with `[worlds.<name>]` values, by name.
    named_views: BTreeMap<String, ViewLimits>,
    /// The view values of the worlds the server started with, by index ([`State::new`]), and
    /// of the worlds made with their own values since (`Context::create_world`).
    world_views: Vec<ViewLimits>,
    max_players: u32,
    shutdown_message: String,
    /// Ticks between save snapshots.
    save_interval: u64,
    /// Whether player data is loaded and saved (`save-player-data`).
    save_player_data: bool,
    /// Whether the server file is backed up at start and on request (`backups`).
    backups: bool,
    /// How light is calculated (`lighting`).
    lighting: Lighting,
    /// The sky light everywhere with [`Lighting::Fixed`] (`fixed-light-level`).
    fixed_light_level: u8,
    /// Whether players can hurt each other (`pvp`).
    pvp: bool,
    /// Whether players get hungry (`hunger`).
    hunger: bool,
    /// Whether players gain experience (`experience`).
    experience: bool,
    /// Whether experience orbs exist and bottles o' enchanting can be thrown (`experience-orbs`);
    /// the rules ask [`Settings::has_orbs`], which also needs `experience`.
    experience_orbs: bool,
    /// Whether falling hurts (`fall-damage`).
    fall_damage: bool,
    /// Whether air, drowning and suffocation exist (`drowning`).
    drowning: bool,
    /// Whether armour protects and wears (`armor`).
    armor: bool,
    /// Whether tools, weapons and armour wear out (`durability`).
    durability: bool,
    /// Whether beds can be slept in and set spawn points (`beds`).
    beds: bool,
    /// Whether the time of day moves on (`daylight-cycle`).
    daylight_cycle: bool,
    /// Ticks a dropped item stays on the ground (`item-despawn-ticks`).
    item_despawn_ticks: u32,
    /// When a hit can be critical (`critical-hits`).
    critical_hits: CriticalHits,
    /// Whether block breaking time is checked (`break-time-check`).
    break_time_check: bool,
    /// Whether moves of more than 15 blocks are refused (`movement-check`).
    movement_check: bool,
    /// Whether players see their coordinates (`show-coordinates`).
    show_coordinates: bool,
    /// Whether players see the others on the locator bar (`locator-bar`).
    locator_bar: bool,
    /// Whether dead players respawn without the death screen (`immediate-respawn`).
    immediate_respawn: bool,
    /// Whether every player sees how a player died (`show-death-messages`).
    show_death_messages: bool,
    /// Whether a world without players runs one tick in every 100 (`slow-empty-worlds`).
    slow_empty_worlds: bool,
    /// Ticks between item pickup checks (`item-pickup-interval`).
    item_pickup_interval: u8,
    /// The largest batch a session sends (`max-batch-bytes`), which each player list packet of a
    /// spawn window fits.
    max_batch_bytes: usize,
    /// Whether movement packets sent pass the send hooks (`hook-movement-packets`).
    hook_movement_packets: bool,
    /// Whether `PlayerAuthInput` packets pass the receive hooks (`hook-player-input`).
    hook_player_input: bool,
    /// Whether players always get the default game mode (`force-gamemode`).
    force_game_mode: bool,
    /// Random block ticks per sub-chunk per tick (`random-tick-speed`).
    random_tick_speed: u32,
    /// The block kinds that never take random ticks (`disable-block-ticking`), sorted.
    no_random_ticks: Vec<crate::block::Kind>,
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
            view: config.view_limits(None),
            global_view: config.global_view(),
            named_views: config
                .worlds
                .iter()
                .map(|(name, world)| (name.clone(), config.view_limits(Some(world))))
                .collect(),
            world_views: Vec::new(),
            max_players: general.max_players,
            shutdown_message: general.shutdown_message.clone(),
            save_interval: u64::try_from(
                u128::from(developer.save_interval_minutes) * 60_000 / TICK.as_millis(),
            )
            .unwrap_or(u64::MAX),
            save_player_data: general.save_player_data,
            backups: general.backups,
            lighting: developer.lighting,
            fixed_light_level: developer.fixed_light_level,
            pvp: general.pvp,
            hunger: general.hunger,
            experience: general.experience,
            experience_orbs: general.experience_orbs,
            fall_damage: general.fall_damage,
            drowning: general.drowning,
            armor: general.armor,
            durability: general.durability,
            beds: general.beds,
            daylight_cycle: general.daylight_cycle,
            item_despawn_ticks: general.item_despawn_ticks,
            critical_hits: general.critical_hits,
            break_time_check: general.break_time_check,
            movement_check: general.movement_check,
            show_coordinates: general.show_coordinates,
            locator_bar: general.locator_bar,
            immediate_respawn: general.immediate_respawn,
            show_death_messages: general.show_death_messages,
            slow_empty_worlds: developer.slow_empty_worlds,
            item_pickup_interval: developer.item_pickup_interval,
            max_batch_bytes: developer.max_batch_bytes,
            hook_movement_packets: developer.hook_movement_packets,
            hook_player_input: developer.hook_player_input,
            force_game_mode: general.force_gamemode,
            random_tick_speed: developer.random_tick_speed,
            no_random_ticks: update::untickable(&developer.disable_block_ticking),
        }
    }

    /// Whether experience orbs exist: `experience-orbs` with `experience`, as no experience means
    /// no orbs. Off, no rule works out experience to drop, no orb is made or loaded and no bottle
    /// o' enchanting is thrown; points and levels follow `experience` alone.
    fn has_orbs(&self) -> bool {
        self.experience && self.experience_orbs
    }

    /// How the worlds' light is worked out (`lighting`, `fixed-light-level`).
    pub(crate) fn light_mode(&self) -> crate::world::LightMode {
        crate::world::LightMode::of(self.lighting, self.fixed_light_level)
    }

    /// The view values in effect in world `world`: its `[worlds.<name>]` values over the global
    /// ones. A world made while the server runs has its options' values over the global ones; one
    /// loaded while it runs has the global ones.
    pub(crate) fn view(&self, world: WorldIndex) -> ViewLimits {
        self.world_views.get(world).copied().unwrap_or(self.view)
    }
}

checkpoint::fields!(ViewLimits {
    view_distance,
    chunks_per_tick,
    spawn_radius,
    tick_radius,
});

/// What the user registers on the [`ServerBuilder`](crate::ServerBuilder), fixed when the server is
/// built and moved into the state when it runs. Each field belongs to the module that uses it.
#[derive(Debug, Default)]
pub(crate) struct Setup {
    /// The event listeners and monitors, each in calling order.
    pub(crate) listeners: Listeners,
    /// The scheduled tasks.
    pub(crate) tasks: Tasks,
    /// The commands and permissions.
    pub(crate) commands: command::Commands,
    /// The packet hooks.
    pub(crate) hooks: hook::Hooks,
    /// The user's data types and the server's values.
    pub(crate) data: task::UserData,
    /// The custom entity types.
    pub(crate) custom_entities: entity::Customs,
    /// The custom items, blocks and recipes, numbered by `build`.
    pub(crate) custom: crate::custom::Registry,
    /// The generators worlds are made with, by name.
    pub(crate) generators: crate::world::Generators,
    /// What the server was built with, for the input log.
    pub(crate) fingerprints: log::Fingerprints,
}

/// The game state.
#[derive(Debug)]
pub(crate) struct State {
    settings: Settings,
    /// The start-game batches every player gets the same.
    shared: Shared,
    /// Engine time: ticks applied since the world was created.
    tick: u64,
    /// Whether this run's first tick raised [`ServerStart`](crate::event::ServerStart); kept in
    /// checkpoints, so a replay raises it only where the run did.
    started: bool,
    /// The next entity runtime ID; IDs are never reused.
    next_runtime_id: u64,
    players: BTreeMap<SessionId, Player>,
    /// The session of each joined player by [`PlayerKey`]: `index[p.key] == s` exactly when
    /// `players[s] == p`. Derived from `players`, so it changes only where `players` does.
    index: BTreeMap<PlayerKey, SessionId>,
    /// Each joined player's name in ASCII lowercase and its session, for the name lookups of
    /// [`Context`] (players signed in with Xbox may share a name). Derived from `players`.
    names: BTreeSet<(String, SessionId)>,
    /// Each joined player's UUID and its session, for [`Context::player_by_uuid`] (a login
    /// chooses its UUID, so two may share one). Derived from `players`.
    uuids: BTreeSet<(Uuid, SessionId)>,
    /// The user's event listeners, called only in [`step`].
    listeners: Listeners,
    /// The user's tasks and blocking jobs, called only in [`step`].
    tasks: Tasks,
    /// The user's data types and the server's values.
    data: task::UserData,
    /// The user's commands and permissions.
    commands: command::Commands,
    /// The user's packet hooks.
    hooks: hook::Hooks,
    /// Open forms, scoreboards and boss bars of the players.
    forms: form::Forms,
    /// The click handlers of the players' open menus.
    menus: inventory::Menus,
    /// Callbacks deferred to after the running one ([`Context::defer`]); empty between ticks
    /// unless a tick hit its run limit.
    deferred: dispatch::Deferred,
    /// The worlds the server had this run, by index, loaded or not (`world_api::Catalog`).
    worlds: Vec<World>,
    /// Which worlds are loaded, the default one, the server file's other worlds, and the world
    /// events and unloads waiting for the view phase.
    catalog: world_api::Catalog,
    /// Which chunks are held, in flight or waiting for a commit.
    chunks: view::Chunks,
    /// Save snapshots and what waits for their commit.
    saving: save::Saving,
    /// The session of each player waiting for its saved data, by storage key.
    loading: BTreeMap<StoredPlayer, SessionId>,
    /// The session of each joined player by runtime ID (players are entities to each other).
    /// Derived from `players`, so it changes only where `players` does.
    runtime_ids: BTreeMap<u64, SessionId>,
    /// The rule modules' chunk-bound state: block entities, entities, block updates.
    rules: rules::Rules,
    /// The world clocks.
    clock: survival::Clock,
    /// Who trades with which villager NPC.
    trades: trade::Trades,
    /// The skin bytes the server may still broadcast, and whose skin goes next.
    skin_budget: survival::SkinBudget,
    /// The custom items, blocks and recipes (fixed at build).
    custom: std::sync::Arc<crate::custom::Registry>,
    /// What the server was built with, which the input log names (fixed at build).
    fingerprints: log::Fingerprints,
    /// The commit identity of the state the server file held at the start, which the input log
    /// chains on from (fixed at start; `None` for a file that has none).
    identity: Option<u64>,
    /// The measured tick times and connection counts, and the controls for the networking.
    network: network::Network,
    /// How each world runs its world rules this tick, by index, while `slow-empty-worlds` holds
    /// worlds without players back ([`State::is_awake`]); empty when every world runs. Worked
    /// out at the start of the rules from `players`, its buffer kept from tick to tick.
    awake: Vec<Pace>,
}

/// What makes two logins the same player: the XUID with Xbox sign-in, otherwise the name in ASCII
/// lowercase (PMMP compares with `strcasecmp`, which folds ASCII only).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PlayerKey {
    Xuid(u64),
    Name(String),
}

impl PlayerKey {
    fn of(identity: &Identity) -> Self {
        Self::new(identity.xuid, &identity.name)
    }

    /// The key of a player with Xbox user ID `xuid` (`None` without Xbox sign-in) and name `name`.
    fn new(xuid: Option<u64>, name: &str) -> Self {
        match xuid {
            Some(xuid) => Self::Xuid(xuid),
            None => Self::Name(name.to_ascii_lowercase()),
        }
    }
}

/// A joined player, as a [`Context`] lends it. Its getters show what the player is and has; its
/// setters change it and tell its client and the players who see it by the end of the tick.
#[derive(Debug)]
// pmmp-class: Player
pub struct Player {
    /// The player's session: its [`crate::player::Id`].
    session: SessionId,
    runtime_id: u64,
    identity: Identity,
    /// [`PlayerKey::of`] the identity, kept to leave the index without rebuilding it.
    key: PlayerKey,
    /// The world the player is in.
    world: WorldIndex,
    /// The eye position, finite and inside the world border.
    position: Vec3,
    /// The position changed since the last save snapshot.
    data_dirty: bool,
    /// The granted chunk radius, once the client asked.
    chunk_radius: Option<u8>,
    /// The radius the player wants, once its client asked: the client's last request or the
    /// API's last [`Context::set_view_distance`]. Each world grants it up to its own
    /// `view-distance`.
    asked_radius: Option<u8>,
    phase: Phase,
    /// The chunks the player sees.
    view: view::View,
    outbound: mpsc::Sender<Outbound>,
    /// What the output phase sends this player at the end of the tick; empty between ticks.
    pending: spawn::Pending,
    /// What the rule modules keep of the player between joins (`extra`).
    extra: extra::PlayerExtra,
    /// Slots, cursor, crafting grids and open windows.
    inventory: inventory::Inventory,
    /// Health, hunger, experience, effects, game mode…
    survival: survival::Survival,
    /// Block breaking in progress.
    breaking: interact::Breaking,
    /// Packets the serial phases queued for this player this tick, sent after the output phase's
    /// packets ([`Player::send_packet`]).
    outbox: Vec<Packet>,
    /// Packets held once for every player they go to (a skin of up to 4 MiB), queued by the
    /// serial phases this tick and sent after the outbox ([`Player::send_shared`]).
    shared_outbox: Vec<std::sync::Arc<Packet>>,
    /// The outbox, or the blocks to send again ([`view::MAX_RESYNCS`]), overflowed this tick: the
    /// player is disconnected at the end of the tick.
    overflowed: bool,
    /// What the public API marked for the end of the tick ([`player::sync`]).
    marks: player::Marks,
    /// The latest ping and the play history.
    presence: player::Presence,
}

/// The most packets the serial phases may queue for one player in one tick; more means a runaway
/// rule, and the player is disconnected instead of growing the queue.
const MAX_OUTBOX: usize = 4096;

impl Player {
    /// Queues `packet` for this player: it is sent after this tick's other packets. More than
    /// 4,096 packets in one tick disconnect the player instead (a runaway rule or callback). A
    /// packet that does not encode, such as one with a string or list over its Mojang limit
    /// (the client would refuse it), ends the player's connection when it is sent.
    #[doc(alias = "NetworkSession::sendDataPacket")]
    pub fn send_packet(&mut self, packet: Packet) {
        if self.outbox.len().saturating_add(self.shared_outbox.len()) < MAX_OUTBOX {
            self.outbox.push(packet);
        } else {
            self.overflowed = true;
        }
    }

    /// Queues `packet`, which other players get too, without a copy for this player: it is sent
    /// after this tick's outbox and counts against the same bound.
    pub(crate) fn send_shared(&mut self, packet: std::sync::Arc<Packet>) {
        if self.outbox.len().saturating_add(self.shared_outbox.len()) < MAX_OUTBOX {
            self.shared_outbox.push(packet);
        } else {
            self.overflowed = true;
        }
    }

    /// Queues `packet`, which this player gets as a viewer of what goes on around it (an effect,
    /// another player's action), unless its outbox is nearly full ([`Self::has_viewer_room`]).
    pub(crate) fn send_to_viewer(&mut self, packet: Packet) {
        if self.has_viewer_room() {
            self.send_packet(packet);
        }
    }

    /// Whether the outbox has room for what the player gets as a viewer: that leaves
    /// [`world_api::EFFECT_RESERVE`] packets of room, so one client's inputs, which can make every
    /// viewer get packets in a loop, cannot fill another player's outbox and disconnect it.
    pub(crate) fn has_viewer_room(&self) -> bool {
        self.outbox.len().saturating_add(self.shared_outbox.len())
            < MAX_OUTBOX - world_api::EFFECT_RESERVE
    }
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
    /// A server at engine time `tick` with `worlds` (the default world first, none loaded) and
    /// what the user registered (`setup`, listeners already sorted). The spawn chunks of every
    /// world are held from the start.
    pub(crate) fn new(
        mut settings: Settings,
        shared: Shared,
        setup: Setup,
        mut worlds: Vec<World>,
        tick: u64,
    ) -> Self {
        let Setup {
            listeners,
            tasks,
            commands,
            hooks,
            data,
            custom_entities,
            custom,
            generators,
            fingerprints,
        } = setup;
        let custom = std::sync::Arc::new(custom);
        settings.world_views = worlds
            .iter()
            .map(|w| {
                settings
                    .named_views
                    .get(w.name())
                    .copied()
                    .unwrap_or(settings.view)
            })
            .collect();
        for world in &mut worlds {
            world.set_lighting(settings.light_mode());
            world.set_custom(std::sync::Arc::clone(&custom));
        }
        let catalog = world_api::Catalog::new(&worlds, generators);
        let saving = save::Saving::new(tick, settings.save_interval, settings.save_player_data);
        let mut state = Self {
            settings,
            shared,
            tick,
            started: false,
            next_runtime_id: 1,
            players: BTreeMap::new(),
            index: BTreeMap::new(),
            names: BTreeSet::new(),
            uuids: BTreeSet::new(),
            listeners,
            tasks,
            commands,
            hooks,
            data,
            forms: form::Forms::default(),
            menus: inventory::Menus::default(),
            deferred: dispatch::Deferred::default(),
            worlds,
            catalog,
            chunks: view::Chunks::default(),
            saving,
            loading: BTreeMap::new(),
            runtime_ids: BTreeMap::new(),
            rules: rules::Rules::default(),
            clock: survival::Clock::default(),
            trades: trade::Trades::default(),
            skin_budget: survival::SkinBudget::default(),
            custom,
            fingerprints,
            identity: None,
            network: network::Network::default(),
            awake: Vec::new(),
        };
        state.rules.entities.set_customs(custom_entities);
        state
            .rules
            .entities
            .set_experience(state.settings.has_orbs());
        view::hold_spawns(&mut state);
        state
    }

    /// Sends what the API asks of the networking ([`queue::Control`]) to `link` from now on.
    pub(crate) fn connect_network(&mut self, link: mpsc::Sender<queue::Control>) {
        self.network.connect(link);
    }

    /// Sets the commit identity of the state the server file holds
    /// ([`crate::save::Opened::identity`]).
    pub(crate) fn set_identity(&mut self, identity: Option<u64>) {
        self.identity = identity;
    }

    /// Whether world `world` runs its world rules this tick: its time, block updates, block
    /// entities and light (`slow-empty-worlds`, [`wake`]). Server tasks and events always run.
    pub(crate) fn is_awake(&self, world: WorldIndex) -> bool {
        self.awake.get(world) != Some(&Pace::Asleep)
    }

    /// Whether world `world` runs this tick as a world `slow-empty-worlds` holds back: on its one
    /// tick in [`SLOW_WORLD_PERIOD`] ([`wake`]).
    pub(crate) fn is_slowed(&self, world: WorldIndex) -> bool {
        self.awake.get(world) == Some(&Pace::Slowed)
    }

    /// The custom content, which the save thread also resolves saved custom blocks against.
    pub(crate) fn custom(&self) -> &std::sync::Arc<crate::custom::Registry> {
        &self.custom
    }

    /// The custom entity types, which the save thread also reads saved entities with.
    pub(crate) fn entity_types(&self) -> &std::sync::Arc<EntityTypes> {
        self.rules.entities.types()
    }

    /// A hash of everything a replay must reproduce, independent of thread count, OS and the
    /// order of the map in memory (FNV-1a 64 over fields in a fixed order and width).
    ///
    /// The indexes (by key, name, UUID and runtime ID) and each player's key are left out: they
    /// are functions of the hashed sessions, runtime IDs and identities
    /// ([`Self::index_matches_players`] checks this in tests). Listeners and tasks are code, fixed
    /// at build; the settings no API call changes and the shared batches are fixed at start;
    /// outbound queues are not state; pending output and the outbox are empty between ticks, and
    /// a player whose outbox overflowed is gone by then. Each world's light and light work are
    /// hashed by the world ([`World::light_hash`]). Task bodies are code too: when they run is
    /// hashed. Every field is named (no `..`), so a new field does not compile until it is hashed
    /// or left out here.
    #[cfg(test)]
    #[expect(
        clippy::too_many_lines,
        reason = "consistency: one destructure names every field of the state"
    )]
    pub(crate) fn hash(&self) -> u64 {
        let Self {
            settings,
            shared: _,
            tick,
            started,
            next_runtime_id,
            players,
            index: _,
            names: _,
            uuids: _,
            listeners: _,
            tasks,
            data,
            commands,
            hooks: _,
            forms,
            // Handlers are code; which menus are open is in each player's inventory.
            menus: _,
            deferred,
            worlds,
            catalog,
            chunks,
            saving,
            loading,
            runtime_ids: _,
            rules,
            clock,
            trades,
            skin_budget,
            custom: _,
            fingerprints: _,
            // Fixed at start; the input log chains on from it.
            identity: _,
            network,
            // Worked out from the players at the start of each tick's rules.
            awake: _,
        } = self;
        let mut h = Fnv::new();
        // The settings the API changes; the others are fixed at start.
        let Settings {
            motd,
            game_mode,
            difficulty,
            hardcore: _,
            view: _,
            global_view: _,
            named_views: _,
            world_views,
            max_players,
            shutdown_message: _,
            save_interval: _,
            save_player_data: _,
            backups: _,
            lighting: _,
            fixed_light_level: _,
            pvp: _,
            hunger: _,
            experience: _,
            experience_orbs: _,
            fall_damage: _,
            drowning: _,
            armor: _,
            durability: _,
            beds: _,
            daylight_cycle: _,
            item_despawn_ticks: _,
            critical_hits: _,
            break_time_check: _,
            movement_check: _,
            show_coordinates: _,
            locator_bar: _,
            immediate_respawn: _,
            show_death_messages: _,
            slow_empty_worlds: _,
            item_pickup_interval: _,
            max_batch_bytes: _,
            hook_movement_packets: _,
            hook_player_input: _,
            force_game_mode: _,
            random_tick_speed,
            no_random_ticks: _,
        } = settings;
        h.u64(u64::from(
            survival::game_mode_id(*game_mode).cast_unsigned(),
        ));
        h.u64(u64::from(spawn::difficulty_id(*difficulty).cast_unsigned()));
        h.u64(u64::from(*max_players));
        h.u64(u64::from(*random_tick_speed));
        h.bytes(motd.as_bytes());
        h.usize(world_views.len());
        for v in world_views {
            h.u64(u64::from(v.view_distance));
            h.u64(u64::from(v.chunks_per_tick));
            h.u64(u64::from(v.spawn_radius));
            h.u64(u64::from(v.tick_radius));
        }
        h.u64(*tick);
        h.u64(u64::from(*started));
        h.u64(*next_runtime_id);
        // Callbacks are code; how many wait is state.
        h.usize(deferred.len());
        h.usize(players.len());
        for (session, p) in players {
            let Player {
                // The map key, hashed below.
                session: _,
                runtime_id,
                identity,
                key: _,
                world,
                position,
                data_dirty,
                chunk_radius,
                asked_radius,
                phase,
                view,
                outbound: _,
                pending: _,
                extra,
                inventory,
                survival,
                breaking,
                outbox: _,
                shared_outbox: _,
                overflowed: _,
                marks,
                presence,
            } = p;
            h.u64(session.get());
            h.u64(*runtime_id);
            identity.hash_into(&mut h);
            h.usize(*world);
            for c in [position.x, position.y, position.z] {
                h.u64(c.to_bits());
            }
            h.u64(u64::from(*data_dirty));
            h.u64(chunk_radius.map_or(0, |r| u64::from(r) + 1));
            h.u64(asked_radius.map_or(0, |r| u64::from(r) + 1));
            h.u64(match phase {
                Phase::Loading => 3,
                Phase::Joined => 0,
                Phase::Spawning => 1,
                Phase::Spawned => 2,
            });
            view.hash_into(&mut h);
            extra.hash_into(&mut h);
            inventory.hash_into(&mut h);
            survival.hash_into(&mut h);
            breaking.hash_into(&mut h);
            marks.hash_into(&mut h);
            presence.hash_into(&mut h);
        }
        catalog.hash_into(&mut h);
        for world in worlds {
            h.bytes(world.name().as_bytes());
            h.u64(u64::from(world.kind() == crate::world::Kind::Persistent));
            h.u64(world.seed().cast_unsigned());
            h.u64(world.number().map_or(0, |n| u64::from(n) + 1));
            let spawn = world.spawn();
            for c in [spawn.x, spawn.y, spawn.z] {
                h.u64(u64::from(c.cast_unsigned()));
            }
            h.u64(world.time_ticks().cast_unsigned());
            h.u64(u64::from(world.is_time_stopped()));
            h.u64(world.age_ticks());
            h.u64(world.difficulty().map_or(0, |d| {
                u64::from(spawn::difficulty_id(d).cast_unsigned()) + 1
            }));
            h.u64(world.weather() as u64);
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
            h.usize(world.changed().len());
            for p in world.changed() {
                h.bytes(&[p.x, p.y, p.z].map(i32::to_le_bytes).concat());
            }
            h.u64(world.light_hash());
        }
        chunks.hash_into(&mut h);
        saving.hash_into(&mut h);
        h.usize(loading.len());
        for (key, session) in loading {
            h.bytes(key.as_str().as_bytes());
            h.u64(session.get());
        }
        rules.hash_into(&mut h);
        clock.hash_into(&mut h);
        trades.hash_into(&mut h);
        skin_budget.hash_into(&mut h);
        commands.hash_into(&mut h);
        forms.hash_into(&mut h);
        tasks.hash_into(data, &mut h);
        data.hash_into(players, &mut h);
        network.hash_into(&mut h);
        h.0
    }

    /// Whether the indexes hold exactly the key, name, UUID and session of every player.
    #[cfg(test)]
    pub(crate) fn index_matches_players(&self) -> bool {
        let n = self.players.len();
        self.index.len() == n
            && self.runtime_ids.len() == n
            && self.names.len() == n
            && self.uuids.len() == n
            && self.players.iter().all(|(&session, p)| {
                p.key == PlayerKey::of(&p.identity)
                    && self.index.get(&p.key) == Some(&session)
                    && self.runtime_ids.get(&p.runtime_id) == Some(&session)
                    && self
                        .names
                        .contains(&(p.identity.name.to_ascii_lowercase(), session))
                    && self.uuids.contains(&(p.identity.uuid, session))
            })
    }

    /// Adds a player, which must have a new session and a key no player has.
    fn insert(&mut self, session: SessionId, player: Player) {
        self.index.insert(player.key.clone(), session);
        self.names
            .insert((player.identity.name.to_ascii_lowercase(), session));
        self.uuids.insert((player.identity.uuid, session));
        self.runtime_ids.insert(player.runtime_id, session);
        self.players.insert(session, player);
    }

    /// Removes the player of `session`, if any, with its index entries and the chunks it held; its
    /// open window closes first, so the cursor and grid items are back in its inventory. Unsaved
    /// data stays in memory for the next commit. Returns its outbound queue.
    fn remove(&mut self, session: SessionId) -> Option<Player> {
        inventory::close_window(self, session);
        let mut player = self.players.remove(&session)?;
        self.index.remove(&player.key);
        self.names
            .remove(&(player.identity.name.to_ascii_lowercase(), session));
        self.uuids.remove(&(player.identity.uuid, session));
        self.runtime_ids.remove(&player.runtime_id);
        spawn::left(self, &mut player);
        form::left(self, session);
        command::left(self, session);
        entity::left(self, session);
        let stored = save::storage_key(&player.key);
        if self.loading.get(&stored) == Some(&session) {
            self.loading.remove(&stored);
        }
        view::close(self, &mut player);
        save::left(self, &player);
        Some(player)
    }

    /// Closes the session of `player`, which [`Self::remove`] returned, with `packet` after the
    /// packets the serial phases queued for it this tick, passed through the send hooks as the
    /// output phase passes them: what the player was sent in the step that removed it (before a
    /// kick, by a quit listener) reaches it, as PMMP flushes the queued packets ahead of the
    /// disconnect packet. A player whose outbox or blocks to send again overflowed this tick
    /// gets `packet` alone: what it was to be sent is incomplete.
    fn close(&self, player: Player, packet: Packet, output: &mut Output) {
        let Player {
            session,
            outbound,
            outbox,
            shared_outbox,
            overflowed,
            ..
        } = player;
        let mut messages: Vec<Message> = if overflowed {
            Vec::new()
        } else {
            outbox
                .into_iter()
                .map(Message::Packet)
                .chain(shared_outbox.into_iter().map(Message::SharedPacket))
                .collect()
        };
        hook::send(self.hooks.send(), &self.settings, session, &mut messages);
        output.closes.push((outbound, messages, packet));
    }

    /// The player of `session`, if any, leaves for `reason` (PMMP `Player::disconnect` without a
    /// message to the client): [`player::quit`] with BDS's left line, then it is forgotten and its
    /// outbound queue closes.
    fn leave(&mut self, session: SessionId, reason: &str) {
        player::quit(
            self,
            session,
            reason,
            player::LEFT.to_owned(),
            player::QuitCause::Disconnect,
        );
        if let Some(p) = self.remove(session) {
            info!(session = session.get(), name = %p.identity.name, reason, "player left");
        }
    }

    /// Queues `packet` for every player that sees chunk `chunk` of `world`, except `except`
    /// (block effects, entity packets, another player's movement and animations), as a viewer
    /// ([`Player::send_to_viewer`]).
    fn broadcast(
        &mut self,
        world: WorldIndex,
        chunk: crate::world::ChunkPos,
        packet: &Packet,
        except: Option<SessionId>,
    ) {
        for (&session, player) in &mut self.players {
            if Some(session) != except && player.world == world && player.view.sees(chunk) {
                player.send_to_viewer(packet.clone());
            }
        }
    }

    /// Disconnects every player with the shutdown message, then runs the callbacks their quits
    /// deferred, so that what they change is in the final snapshot.
    fn close_all(&mut self, output: &mut Output) {
        let sessions: Vec<SessionId> = self.players.keys().copied().collect();
        for session in sessions {
            let reason = self.settings.shutdown_message.clone();
            player::quit(
                self,
                session,
                &reason,
                String::new(),
                player::QuitCause::Shutdown,
            );
            let Some(p) = self.remove(session) else {
                continue;
            };
            debug!(session = session.get(), "closing for shutdown");
            let packet = Packet::Disconnect(Disconnect {
                reason: i32::from(DisconnectReason::Shutdown),
                hide_disconnection_screen: false,
                message: self.settings.shutdown_message.clone(),
                filtered_message: String::new(),
            });
            self.close(p, packet, output);
        }
        dispatch::run_deferred(self, &mut 0);
    }
}

/// What one tick sends, delivered after the step.
#[derive(Debug, Default)]
pub(crate) struct Output {
    /// Messages per session from the output phase, in session order.
    packets: Vec<(SessionId, Vec<Message>)>,
    /// Final packets for sessions the tick has already forgotten, each after the messages that
    /// go before it.
    closes: Vec<(mpsc::Sender<Outbound>, Vec<Message>, Packet)>,
    /// Requests for the save thread, in order.
    requests: Vec<Request>,
    /// A commit or a chunk load failed: the server stops without saving.
    save_failed: bool,
    /// Tests: the answers of a stand-in save thread, applied by the next tick.
    #[cfg(test)]
    answers: Vec<Response>,
}

impl Output {
    fn close(&mut self, outbound: mpsc::Sender<Outbound>, packet: Packet) {
        self.closes.push((outbound, Vec::new(), packet));
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.packets.is_empty() && self.closes.is_empty()
    }
}

/// Applies one tick of `inputs` and save thread `responses`, leaving both empty, and records them
/// in `log`.
///
/// 1. Input (serial): the save thread's answers in arrival order (loaded chunks and player data,
///    commits), in a run's first tick [`ServerStart`](crate::event::ServerStart), then the
///    session inputs in (session, seq) order, so the result does not depend on how sessions
///    interleaved in the queue, each followed by the callbacks it deferred. Then the scheduled
///    tasks.
/// 2. Rules ([`rules()`]): survival and the world clocks, breaking, block updates, entities (their
///    physics computed on the CPU pool read-only, then applied in entity order), trades, block
///    entities and light.
/// 3. Sync (serial): the deferred callbacks, the command and player changes the API marked, the
///    players whose outbox overflowed this tick ([`drop_overflowed`]), then the views: each
///    follows its player, picks the chunks to send and asks for the loads it needs; last, the
///    block entities changed this tick, each sent once ([`block_entity::send_marked`]). A session
///    whose outbound queue is full is dropped after the step, when the output is delivered.
/// 4. Output (parallel, read-only): the sub-chunks the answers need, encoded once each, then
///    every marked player's packets, built on the CPU pool in player order, so the output does
///    not depend on the thread count.
///
/// After the output is delivered, the tick ends with [`boundary`].
pub(crate) fn step(
    state: &mut State,
    inputs: &mut Vec<Input>,
    responses: &mut Vec<Response>,
    pool: &Pool,
    output: &mut Output,
    log: &mut InputLog,
) {
    step_marked(state, inputs, responses, pool, output, log, |_| {});
}

/// The parts of a [`step`], in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Part {
    /// The save thread's answers, the session inputs and the tasks.
    Input,
    Survival,
    Interact,
    Update,
    Entity,
    Trade,
    BlockEntity,
    Light,
    /// The callbacks the rules deferred, the API's marks and the views.
    View,
    Output,
}

/// [`step`], calling `mark` as each [`Part`] ends: the tick cost measurements time the parts, the
/// tick passes a mark that does nothing.
fn step_marked(
    state: &mut State,
    inputs: &mut Vec<Input>,
    responses: &mut Vec<Response>,
    pool: &Pool,
    output: &mut Output,
    log: &mut InputLog,
    mut mark: impl FnMut(Part),
) {
    log.begin(state.tick);
    for response in responses.drain(..) {
        respond(state, response, output, log);
    }
    let mut deferred_runs = 0;
    if !std::mem::replace(&mut state.started, true) {
        start(state);
        dispatch::run_deferred(state, &mut deferred_runs);
    }
    // Keys are unique per input, so the unstable sort is deterministic and allocates nothing.
    inputs.sort_unstable_by_key(|i| (i.session, i.seq));
    // Each input is logged before it applies, so the log of a tick that panics in a user callback
    // holds the input that raised it.
    for input in inputs.drain(..) {
        let (session, seq) = (input.session, input.seq);
        let entry = match input.kind {
            InputKind::Join { identity, outbound } => {
                log.record(
                    session,
                    seq,
                    EntryKind::Join(identity.clone()),
                    identity.size(),
                );
                if let Some(request) = spawn::join(state, session, identity, outbound, output) {
                    output.requests.push(request);
                }
                continue;
            }
            InputKind::Packet { packet, size } => {
                log.record(session, seq, EntryKind::Packet(packet), size)
            }
            InputKind::Leave => log.record(session, seq, EntryKind::Leave, 0),
            InputKind::Console(line) => {
                let size = line.len();
                log.record(session, seq, EntryKind::Console(line), size)
            }
            InputKind::Job(outcome) => {
                let size = outcome.as_ref().map_or(0, Vec::len);
                log.record(session, seq, EntryKind::Job(outcome), size)
            }
            InputKind::Measure(measure) => log.record(session, seq, EntryKind::Measure(measure), 0),
        };
        match &*entry {
            EntryKind::Packet(packet) => {
                if let Some(packet) = hook::receive(state, session, packet) {
                    apply_packet(state, session, &packet);
                }
            }
            EntryKind::Leave => state.leave(session, "client disconnect"),
            EntryKind::Console(line) => command::console(state, line),
            EntryKind::Job(outcome) => task::finish(state, seq, outcome),
            EntryKind::Measure(measure) => network::apply(state, session, *measure),
            // A join applied above: its outbound queue is not part of the log.
            EntryKind::Join(_) => {}
        }
        dispatch::run_deferred(state, &mut deferred_runs);
    }
    Tasks::run(state);
    dispatch::run_deferred(state, &mut deferred_runs);
    mark(Part::Input);
    rules(state, pool, &mut mark);
    dispatch::run_deferred(state, &mut deferred_runs);
    command::sync(state);
    player::sync(state, output);
    drop_overflowed(state, output);
    output.requests.extend(view::update(state));
    dispatch::run_deferred(state, &mut deferred_runs);
    block_entity::send_marked(state);
    let changes = view::take_changes(&mut state.worlds);
    mark(Part::View);
    output.packets = spawn::output(state, pool, &changes);
    state.tick = state.tick.saturating_add(1);
    mark(Part::Output);
}

/// The tick boundary after a step and its delivery: a save snapshot for the save thread if one is
/// due, the offline player loads the tick asked for, then the tick's controls to the networking.
/// It comes after the delivery because sessions dropped there change the state of the tick, and
/// their quit listeners may ask for controls or loads; none waits past the tick, where its
/// checkpoint would not keep it.
pub(crate) fn boundary(state: &mut State, output: &mut Output, log: &mut InputLog) {
    if let Some(request) = save::snapshot(state, log, false) {
        output.requests.push(request);
    }
    output.requests.extend(task::offline_requests(state));
    state.network.flush();
}

/// Raises [`ServerStart`](crate::event::ServerStart) in the first tick of a run.
fn start(state: &mut State) {
    if dispatch::heard::<crate::event::ServerStart>(state) {
        let mut e = crate::event::ServerStart {};
        dispatch::call(state, &mut e);
        dispatch::watch(state, &e);
    }
}

/// Applies one answer of the save thread and records it.
fn respond(state: &mut State, response: Response, output: &mut Output, log: &mut InputLog) {
    log.response(&response);
    match response {
        Response::Chunk { world, pos, loaded } => view::loaded(state, world, pos, loaded),
        // A joining player and offline loads may wait on the same answer: the join takes it
        // first, so the loads find the player joined, or its data damaged if that refused it.
        Response::Player { key, data } => match task::take_offline_loads(state, &key) {
            None => spawn::player_loaded(state, &key, data, output),
            Some(loads) => {
                spawn::player_loaded(state, &key, data.clone(), output);
                task::answer_offline_loads(state, &key, loads, &data);
            }
        },
        Response::Committed { start } => save::committed(state, log, start),
        Response::CommitFailed { start } => {
            save::failed(start);
            output.save_failed = true;
        }
        Response::LoadFailed => {
            save::load_failed();
            output.save_failed = true;
        }
        Response::Checked(found) => save::checked(state, &found),
    }
}

/// Applies an in-game packet of `session`: each packet goes to the rule module that owns it.
/// `PlayerAuthInput` runs in PMMP's order: movement, block actions and item interaction, then its
/// item stack request.
fn apply_packet(state: &mut State, session: SessionId, packet: &Packet) {
    match packet {
        Packet::RequestChunkRadius(p) => spawn::request_chunk_radius(state, session, p),
        Packet::SetLocalPlayerAsInitialised(p) => spawn::initialised(state, session, p),
        Packet::PlayerAuthInput(p) => {
            survival::on_auth_input(state, session, p);
            interact::on_auth_input(state, session, p);
            if let Some(request) = &p.item_stack_request {
                inventory::on_item_stack_request(state, session, request);
            }
        }
        Packet::SubChunkRequest(p) => {
            if let Some(player) = state.players.get_mut(&session) {
                view::queue_request(player, p.clone());
            }
        }
        Packet::ItemStackRequest(p) => {
            for request in &p.requests {
                inventory::on_item_stack_request(state, session, request);
            }
        }
        Packet::InventoryTransaction(p) => interact::on_inventory_transaction(state, session, p),
        Packet::MobEquipment(p) => inventory::on_mob_equipment(state, session, p),
        Packet::ContainerClose(p) => inventory::on_container_close(state, session, p),
        Packet::Interact(p) => inventory::on_interact(state, session, p),
        Packet::BlockPickRequest(p) => inventory::on_block_pick_request(state, session, p),
        Packet::ActorPickRequest(p) => entity::on_actor_pick_request(state, session, p),
        Packet::BlockActorData(p) => interact::on_block_actor_data(state, session, p),
        Packet::LecternUpdate(p) => interact::on_lectern_update(state, session, p),
        Packet::BookEdit(p) => interact::on_book_edit(state, session, p),
        Packet::PlayerAction(p) => survival::on_player_action(state, session, p),
        Packet::Animate(p) => survival::on_animate(state, session, p),
        Packet::Respawn(p) => survival::on_respawn(state, session, p),
        Packet::Text(p) => chat::on_text(state, session, p),
        Packet::Emote(p) => survival::on_emote(state, session, p),
        Packet::PlayerSkin(p) => survival::on_skin(state, session, p),
        Packet::CommandRequest(p) => command::on_request(state, session, p),
        Packet::ModalFormResponse(p) => form::on_response(state, session, p),
        p => trace!(session = session.get(), id = p.id(), "packet not handled"),
    }
}

/// Runs the rule modules' tick after the inputs and tasks (phase 2 of [`step`]): survival and the
/// world clocks, breaking in progress, block updates, entities, trades with vanished villagers,
/// block entities, then light work, calling `mark` after each.
fn rules(state: &mut State, pool: &Pool, mark: &mut impl FnMut(Part)) {
    wake(state);
    survival::tick(state);
    mark(Part::Survival);
    interact::tick(state);
    mark(Part::Interact);
    update::tick(state);
    mark(Part::Update);
    entity::tick(state, pool);
    mark(Part::Entity);
    trade::tick(state);
    mark(Part::Trade);
    block_entity::tick(state);
    mark(Part::BlockEntity);
    for index in 0..state.worlds.len() {
        if state.is_awake(index)
            && let Some(world) = state.worlds.get_mut(index)
        {
            world.step_light(LIGHT_BUDGET);
        }
    }
    mark(Part::Light);
}

/// The ticks between the runs of a world without players with `slow-empty-worlds`.
const SLOW_WORLD_PERIOD: u64 = 100;

/// How a world runs its world rules this tick ([`wake`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pace {
    /// Not held back: the world has players.
    Every,
    /// A world without players on its one tick in [`SLOW_WORLD_PERIOD`].
    Slowed,
    /// A world without players between its runs.
    Asleep,
}

/// Works out which worlds run their world rules this tick ([`State::is_awake`]): with
/// `slow-empty-worlds`, world `w` without players only on the ticks `t` where `(t + w) %
/// SLOW_WORLD_PERIOD == 0`, so empty worlds run on ticks of their own instead of all on one.
/// Entities and random block ticks need no check: they tick only near players.
fn wake(state: &mut State) {
    state.awake.clear();
    if !state.settings.slow_empty_worlds {
        return;
    }
    let tick = state.tick;
    state.awake.extend((0..state.worlds.len()).map(|w| {
        let phase = u64::try_from(w).unwrap_or_default();
        if tick.wrapping_add(phase).is_multiple_of(SLOW_WORLD_PERIOD) {
            Pace::Slowed
        } else {
            Pace::Asleep
        }
    }));
    for player in state.players.values() {
        if let Some(pace) = state.awake.get_mut(player.world) {
            *pace = Pace::Every;
        }
    }
}

/// Disconnects the players whose outbox ([`MAX_OUTBOX`]) or blocks to send again
/// ([`view::MAX_RESYNCS`]) overflowed this tick.
fn drop_overflowed(state: &mut State, output: &mut Output) {
    let overflowed: Vec<SessionId> = state
        .players
        .iter()
        .filter(|(_, p)| p.overflowed)
        .map(|(&s, _)| s)
        .collect();
    for session in overflowed {
        player::quit(
            state,
            session,
            "too many packets",
            String::new(),
            player::QuitCause::Disconnect,
        );
        if let Some(p) = state.remove(session) {
            warn!(
                session = session.get(),
                "too many packets queued in one tick; disconnecting"
            );
            let disconnect = Packet::Disconnect(Disconnect {
                reason: i32::from(DisconnectReason::Unknown),
                hide_disconnection_screen: false,
                message: String::new(),
                filtered_message: String::new(),
            });
            state.close(p, disconnect, output);
        }
    }
}

/// Sends `output` to the sessions. A session whose queue is full or closed is [`dropped`]; the
/// tick never waits.
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
        dropped(state, log, session);
    }
    for (outbound, messages, packet) in output.closes.drain(..) {
        // The session closes when the queue does; a full or closed queue only loses what was
        // left to send.
        let queued = messages.is_empty() || outbound.try_send(Outbound::Send(messages)).is_ok();
        if !queued || outbound.try_send(Outbound::Close(packet)).is_err() {
            debug!("could not queue the last packets of a closed session");
        }
    }
}

/// Forgets a session whose outbound queue was full or closed after the output: its player leaves
/// as on a disconnect, and `log` records it, so a replay drops it at the same point.
fn dropped(state: &mut State, log: &mut InputLog, session: SessionId) {
    log.dropped(session);
    state.leave(session, "connection lost");
}

/// Hands the input log to the main thread if the tick thread panics, so it can be written to
/// `data/crash/` without the tick thread touching files.
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
    /// A commit or a chunk load failed: the players were disconnected and nothing more was saved.
    /// The input log since the last commit goes to `data/crash/`.
    SaveFailed(InputLog),
}

/// The tick thread: runs ticks every [`TICK`] until `stop` receives or closes or a commit fails,
/// then disconnects every player, hands the final snapshot to the save thread (unless a commit
/// failed) and returns. `_done` drops when the thread ends, normally or by
/// panic; on a panic the input log goes to `crash` first.
pub(crate) fn run(
    mut state: State,
    pool: &Pool,
    mut inputs: InputReceiver,
    mut save: SaveLink,
    mut stop: mpsc::Receiver<()>,
    crash: oneshot::Sender<InputLog>,
    _done: oneshot::Sender<()>,
) -> End {
    let mut guard = CrashGuard {
        log: InputLog::new(state.checkpoint(pool), state.fingerprints, state.identity),
        crash: Some(crash),
    };
    let mut batch = Vec::with_capacity(MAX_INPUTS_PER_TICK);
    let mut answers = Vec::with_capacity(MAX_RESPONSES_PER_TICK);
    let mut waiting = VecDeque::new();
    let mut output = Output::default();
    let mut pacer = clock::Pacer::new(TICK, clock::now());
    info!(tick = state.tick, "tick started");
    let mut failed = false;
    // How long the previous tick took, which the next one reads as a logged input so a replay sees
    // the same value: measured from its first input to its last request, outside the game logic.
    let mut took = None;
    loop {
        if !matches!(stop.try_recv(), Err(TryRecvError::Empty)) {
            break;
        }
        let started = clock::now();
        if let Some(micros) = took {
            batch.push(Input {
                session: SessionId::ENGINE,
                seq: state.tick,
                kind: InputKind::Measure(queue::Measure::TickTime(micros)),
            });
        }
        drain(|| inputs.try_recv(), &mut batch, MAX_INPUTS_PER_TICK);
        drain(
            || save.responses.try_recv(),
            &mut answers,
            MAX_RESPONSES_PER_TICK,
        );
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
        if guard.log.needs_checkpoint() {
            guard.log.set_checkpoint(state.checkpoint(pool));
        }
        waiting.extend(output.requests.drain(..));
        send_requests(&save.requests, &mut waiting);
        if output.save_failed {
            failed = true;
            break;
        }
        if state.commands.stop_requested() {
            break;
        }
        let now = clock::now();
        let micros = now.saturating_duration_since(started).as_micros();
        took = Some(u32::try_from(micros).unwrap_or(u32::MAX));
        clock::sleep_until(pacer.schedule(now));
    }
    // No boundary follows: the controls the shutdown's quit listeners ask for are dropped with
    // the networking, which stops.
    state.close_all(&mut output);
    if !failed {
        if let Some(request) = save::snapshot(&mut state, &mut guard.log, true) {
            waiting.push_back(request);
        }
        waiting.push_back(Request::Finish);
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

/// One tick of a replay, as [`run`] applies it: the step with the logged `inputs` and the save
/// thread's logged `responses`, the delivery (the replay drains the outbound queues, so none
/// fills), the sessions the live tick forgot after its output (`dropped`, in order), the tick
/// boundary, and the checkpoint of a segment it starts. Returns the requests for the save thread,
/// in order.
pub(crate) fn replay_tick(
    state: &mut State,
    inputs: &mut Vec<Input>,
    responses: &mut Vec<Response>,
    dropped: &[SessionId],
    pool: &Pool,
    log: &mut InputLog,
) -> Vec<Request> {
    let mut output = Output::default();
    step(state, inputs, responses, pool, &mut output, log);
    deliver(state, &mut output, log);
    for &session in dropped {
        self::dropped(state, log, session);
    }
    boundary(state, &mut output, log);
    if log.needs_checkpoint() {
        log.set_checkpoint(state.checkpoint(pool));
    }
    std::mem::take(&mut output.requests)
}

/// The engine tick of `state`: the next tick it applies.
pub(crate) fn tick_of(state: &State) -> u64 {
    state.tick
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

/// Moves up to `max` queued items, each taken by `next` without waiting, into `batch`.
fn drain<T>(mut next: impl FnMut() -> Result<T, TryRecvError>, batch: &mut Vec<T>, max: usize) {
    while batch.len() < max {
        match next() {
            Ok(item) => batch.push(item),
            // Disconnected: every sender is gone (networking or the save thread stopped).
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    mod bench;
    mod busy;
    mod client;
    mod crash;
    mod leak;
    mod replaced;
    mod replay;
    mod scenario;

    use std::collections::BTreeSet;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use super::queue::{input_queue, outbound_queue};
    use super::*;
    use crate::event::{KickFlag, PlayerDuplicateLogin, PlayerLogin, PlayerQuit, Priority};
    use crate::protocol::Uuid;
    use crate::protocol::packet::{
        ChunkRadiusUpdated, PlayStatus, RequestChunkRadius, SetLocalPlayerAsInitialised,
    };
    use crate::save::{Loaded, PlayerLoad};
    use crate::world::{Chunk, ChunkPos, Flat, Generator, Kind};

    /// Test settings: no player data (a join needs no load), and every loaded chunk of a view
    /// sent in one tick.
    pub(super) fn settings(edit: impl FnOnce(&mut Config)) -> Settings {
        let mut config = Config::default();
        config.general.save_player_data = false;
        config.developer.chunks_per_tick = 256;
        edit(&mut config);
        Settings::new(&config)
    }

    #[test]
    fn the_save_interval_counts_its_minutes_in_ticks() {
        assert_eq!(settings(|_| {}).save_interval, 12_000);
        let longest = settings(|c| c.developer.save_interval_minutes = u16::MAX);
        assert_eq!(longest.save_interval, u64::from(u16::MAX) * 1_200);
    }

    /// `ServerStart` comes once, at the start of the first tick, before its join; a state restored
    /// from a checkpoint taken after it does not raise it again, one from before does.
    #[test]
    fn the_server_start_comes_once_and_a_replay_raises_it_where_the_run_did() {
        let (tx, mut started) = mpsc::channel(8);
        let listeners = || {
            let tx = tx.clone();
            let mut listeners = Listeners::default();
            listeners.push(
                Priority::Normal,
                move |_: &mut crate::event::ServerStart, cx: &mut super::Context<'_>| {
                    tx.try_send((cx.state.tick, cx.state.players.len()))
                        .unwrap();
                },
            );
            listeners
        };
        let mut state = new_state(settings(|_| {}), listeners());
        let before = POOL.with(|pool| state.checkpoint(pool));
        let (j, _rx) = join(1, 0);
        let mut output = Output::default();
        tick(&mut state, &mut vec![j], &mut output);
        tick(&mut state, &mut Vec::new(), &mut output);
        assert_eq!(started.try_recv(), Ok((0, 0)), "in tick 0, before the join");
        assert!(started.try_recv().is_err(), "once");
        let after = POOL.with(|pool| state.checkpoint(pool));
        for (checkpoint, raised) in [(&after, false), (&before, true)] {
            let mut replay = new_state(settings(|_| {}), listeners());
            replay
                .restore(checkpoint, checkpoint::tests::extras_of(&state), |_| {
                    outbound_queue().0
                })
                .unwrap();
            tick(&mut replay, &mut Vec::new(), &mut Output::default());
            assert_eq!(started.try_recv().is_ok(), raised);
        }
    }

    /// The IDs of the start-game packets (BDS, P6): `LevelEventGeneric`, `PlayerList`,
    /// `SyncWorldClocks`, `JigsawStructureData`, `VoxelShapes`, `StartGame`, 13
    /// `SyncActorProperty`, `ItemRegistry`.
    const START_GAME: [u32; 20] = [
        124, 63, 344, 313, 337, 11, 165, 165, 165, 165, 165, 165, 165, 165, 165, 165, 165, 165,
        165, 162,
    ];

    /// Chunks within this many chunks of the spawn (a square) are loaded before the first tick,
    /// so a spawn needs no load; the others load through the stand-in save thread.
    const PRELOADED: i32 = 4;

    /// A server with a flat persistent world, seed 0, its spawn area loaded.
    pub(super) fn new_state(settings: Settings, listeners: Listeners) -> State {
        let world = World::new("world", Kind::Persistent, 0, Flat.spawn(0));
        let mut state = State::new(
            settings,
            shared(),
            Setup {
                listeners,
                ..Setup::default()
            },
            vec![world],
            0,
        );
        let center = state.worlds[0].spawn().chunk();
        for x in -PRELOADED..=PRELOADED {
            for z in -PRELOADED..=PRELOADED {
                let pos = ChunkPos::new(center.x + x, center.z + z);
                let chunk = Chunk::generate(&Flat, 0, pos);
                let extras = extra::tests::sample(0, pos).extras;
                let read = state
                    .rules
                    .read((0, pos), &extras, &chunk, &state.worlds[0]);
                state.worlds[0].insert_chunk(pos, chunk);
                state.rules.load((0, pos), read, &extras);
                state.chunks.extras.chunk_loaded((0, pos), extras);
            }
        }
        state
    }

    /// The stand-in save thread: chunks are generated, players are new, commits succeed.
    pub(super) fn answer(state: &State, requests: Vec<Request>) -> Vec<Response> {
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
                            loaded: extra::tests::sample_loaded(state, world, pos, chunk),
                        });
                    }
                }
                Request::Player(key) => out.push(Response::Player {
                    key,
                    data: PlayerLoad::Absent,
                }),
                Request::Commit(snapshot) => {
                    out.push(Response::Committed {
                        start: snapshot.start,
                    });
                    if snapshot.check {
                        out.push(Response::Checked(crate::storage::Checked::Passed {
                            values: 0,
                        }));
                    }
                }
                Request::Worlds(_) | Request::DeleteWorlds(_) | Request::Finish => {}
            }
        }
        out
    }

    /// The shared start-game batches, built once for all tests.
    pub(super) fn shared() -> Shared {
        static SHARED: std::sync::OnceLock<Shared> = std::sync::OnceLock::new();
        SHARED
            .get_or_init(|| {
                Shared::new(
                    crate::protocol::batch::Settings::default(),
                    &crate::custom::Registry::default(),
                    &entity::Customs::default(),
                )
                .unwrap()
            })
            .clone()
    }

    thread_local! {
        /// A CPU pool per test thread; tests do not shut it down.
        static POOL: Pool = crate::runtime::cpu_pool(2).unwrap();
    }

    /// Runs and delivers one tick of `batch`; the stand-in save thread answers its requests for
    /// the next tick.
    pub(super) fn tick(state: &mut State, batch: &mut Vec<Input>, output: &mut Output) {
        let mut log = InputLog::default();
        let mut answers = std::mem::take(&mut output.answers);
        POOL.with(|pool| step(state, batch, &mut answers, pool, output, &mut log));
        deliver(state, output, &mut log);
        boundary(state, output, &mut log);
        let requests = std::mem::take(&mut output.requests);
        output.answers = answer(state, requests);
    }

    /// An in-game packet input of its encoded size.
    pub(super) fn packet(packet: Packet) -> InputKind {
        let mut bytes = Vec::new();
        packet.encode(&mut bytes, 0).unwrap();
        InputKind::Packet {
            packet,
            size: bytes.len(),
        }
    }

    /// The blocks `packets` set: (position, network ID), from `UpdateBlock` and
    /// `UpdateSubChunkBlocks`, in order.
    pub(super) fn blocks_set(packets: &[Packet]) -> Vec<(BlockPos, u32)> {
        let mut out = Vec::new();
        for p in packets {
            match p {
                Packet::UpdateBlock(u) => {
                    let [x, y, z] = u.position;
                    out.push((BlockPos::new(x, y, z), u.new_block_runtime_id));
                }
                Packet::UpdateSubChunkBlocks(u) => {
                    for b in &u.blocks {
                        let [x, y, z] = b.block_pos;
                        out.push((BlockPos::new(x, y, z), b.block_runtime_id));
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn identity(n: u64) -> Identity {
        Identity {
            name: format!("player{n}"),
            xuid: Some(2_535_000_000 + n),
            uuid: Uuid::from_bytes([u8::try_from(n % 256).unwrap(); 16]),
            skin: crate::tick::queue::Skin::default(),
            device_os: -1,
            address: None,
            client: crate::tick::queue::Client::default(),
        }
    }

    pub(super) fn input(session: u64, seq: u64, kind: InputKind) -> Input {
        Input {
            session: SessionId::new(session),
            seq,
            kind,
        }
    }

    pub(super) fn join(session: u64, seq: u64) -> (Input, mpsc::Receiver<Outbound>) {
        let (tx, rx) = outbound_queue();
        let kind = InputKind::Join {
            identity: identity(session),
            outbound: tx,
        };
        (input(session, seq, kind), rx)
    }

    pub(super) fn radius(session: u64, seq: u64, chunk_radius: i32) -> Input {
        let p = RequestChunkRadius {
            chunk_radius,
            max_chunk_radius: 0,
        };
        input(session, seq, packet(Packet::RequestChunkRadius(p)))
    }

    pub(super) fn initialised(session: u64, seq: u64, entity_runtime_id: u64) -> Input {
        let p = SetLocalPlayerAsInitialised { entity_runtime_id };
        let kind = packet(Packet::SetLocalPlayerAsInitialised(p));
        input(session, seq, kind)
    }

    pub(super) fn leave(session: u64, seq: u64) -> Input {
        input(session, seq, InputKind::Leave)
    }

    /// Runs and delivers one tick per batch.
    fn run_ticks(settings: Settings, ticks: Vec<Vec<Input>>) -> State {
        run_ticks_with(settings, Listeners::default(), ticks)
    }

    /// [`run_ticks`] with event listeners.
    fn run_ticks_with(settings: Settings, listeners: Listeners, ticks: Vec<Vec<Input>>) -> State {
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
    pub(super) fn received(
        rx: &mut mpsc::Receiver<Outbound>,
    ) -> (Vec<Packet>, Option<Packet>, Vec<u32>) {
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
                            Message::SharedPacket(p) | Message::SpawnList(p) => {
                                sent.push(p.id());
                                packets.push((*p).clone());
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

    /// Runs 101 ticks of a server with `slow-empty-worlds` set to `slow` and, with `player`, one
    /// player joined: the world's time, its scheduled updates and its light hash after each.
    fn world_rules_over_ticks(slow: bool, player: bool) -> Vec<(i64, usize, u64)> {
        let mut state = new_state(
            settings(|c| c.developer.slow_empty_worlds = slow),
            Listeners::default(),
        );
        let mut output = Output::default();
        let (j, _rx) = join(1, 0);
        let mut batch = if player { vec![j] } else { Vec::new() };
        let spawn = state.worlds[0].spawn();
        update::schedule(&mut state, 0, spawn, 1);
        let mut seen = Vec::new();
        for _ in 0..=100 {
            tick(&mut state, &mut batch, &mut output);
            let w = &state.worlds[0];
            seen.push((w.time_ticks(), state.rules.updates.len().0, w.light_hash()));
        }
        seen
    }

    /// `slow-empty-worlds`: a world without players runs its time, block updates and light only
    /// on ticks that are multiples of 100; with a player, or with the setting off, every tick.
    #[test]
    fn slow_empty_worlds_run_one_tick_in_a_hundred_without_players() {
        let slow = world_rules_over_ticks(true, false);
        assert!(slow[1..100].iter().all(|s| *s == slow[0]), "asleep");
        assert_eq!(slow[100].0, slow[0].0 + 1, "time on tick 100");
        assert_eq!(
            slow[100].1,
            slow[0].1 - 1,
            "the scheduled update on tick 100"
        );
        for (slow, player) in [(false, false), (true, true)] {
            let every = world_rules_over_ticks(slow, player);
            assert_eq!(every[1].0, every[0].0 + 1, "time every tick");
            assert_eq!(every[1].1, every[0].1 - 1, "the update when due");
            assert_ne!(every[1].2, every[0].2, "light work every tick");
        }
    }

    /// How many times the time of each of three worlds moved over 100 ticks, and on which ticks,
    /// with `slow-empty-worlds` set to `slow` and, with `player`, one player in world 0.
    fn wakes_over_100_ticks(slow: bool, player: bool) -> Vec<Vec<u64>> {
        let mut state = new_state(
            settings(|c| c.developer.slow_empty_worlds = slow),
            Listeners::default(),
        );
        let spawn = state.worlds[0].spawn();
        state.add_world(World::new("a", Kind::Temporary, 1, spawn));
        state.add_world(World::new("b", Kind::Temporary, 2, spawn));
        let mut output = Output::default();
        let (j, _rx) = join(1, 0);
        let mut batch = if player { vec![j] } else { Vec::new() };
        let mut wakes = vec![Vec::new(); 3];
        for _ in 0..100 {
            let before: Vec<i64> = state.worlds.iter().map(World::time_ticks).collect();
            let at = state.tick;
            tick(&mut state, &mut batch, &mut output);
            for (w, time) in before.into_iter().enumerate() {
                if state.worlds[w].time_ticks() != time {
                    wakes[w].push(at);
                }
            }
        }
        wakes
    }

    /// `slow-empty-worlds`: each world without players wakes once in 100 ticks on a tick of its
    /// own (world 0 on the multiples of 100), so empty worlds do not all run on one tick; a world
    /// with a player, and every world with the setting off, runs every tick.
    #[test]
    fn empty_worlds_wake_on_ticks_of_their_own() {
        let slow = wakes_over_100_ticks(true, false);
        assert!(slow.iter().all(|w| w.len() == 1), "{slow:?}");
        assert!(slow[0][0].is_multiple_of(100), "{slow:?}");
        assert!(slow[0][0] != slow[1][0] && slow[1][0] != slow[2][0] && slow[0][0] != slow[2][0]);
        let with_player = wakes_over_100_ticks(true, true);
        assert_eq!(with_player[0].len(), 100);
        assert!(with_player[1..].iter().all(|w| w.len() == 1));
        let off = wakes_over_100_ticks(false, false);
        assert!(off.iter().all(|w| w.len() == 100), "{off:?}");
    }

    /// A player who went to a world with a smaller `view-distance` gets the radius its client
    /// asked for back in a world that allows it (min(asked, world limit) on every world change).
    #[test]
    fn the_asked_radius_comes_back_in_a_world_that_allows_it() {
        let mut state = new_state(
            settings(|c| {
                c.general.view_distance = 4;
                let world = crate::config::WorldSettings {
                    view_distance: Some(10),
                    ..crate::config::WorldSettings::default()
                };
                c.worlds.insert("world".to_owned(), world);
            }),
            Listeners::default(),
        );
        let spawn = state.worlds[0].spawn();
        state.add_world(World::new("other", Kind::Temporary, 3, spawn));
        let mut output = Output::default();
        let (j, _rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 8)], &mut output);
        let session = SessionId::new(1);
        let granted = |state: &State| state.players[&session].chunk_radius;
        assert_eq!(granted(&state), Some(8));
        let eye = Vec3::new(0.5, -58.38, 0.5);
        survival::teleport(&mut state, session, 1, eye);
        assert_eq!(granted(&state), Some(4), "the other world allows 4");
        survival::teleport(&mut state, session, 0, eye);
        assert_eq!(granted(&state), Some(8), "the asked radius again");
        assert_eq!(state.players[&session].view.radius(), 8);
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
        assert_eq!(received(&mut rx).2, [START_GAME; 3].concat());
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
        let (sender, mut queue) = input_queue(1);
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut batch = Vec::with_capacity(MAX_INPUTS_PER_TICK);
        let mut output = Output::default();
        let start = (
            state.players.len(),
            queue.len(),
            batch.capacity(),
            state.chunks.held(),
        );
        let mut tick = |queue: &mut InputReceiver, state: &mut State| {
            drain(|| queue.try_recv(), &mut batch, MAX_INPUTS_PER_TICK);
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
        thread_local! {
            /// The quits the listener saw.
            static QUITS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        }
        let mut listeners = Listeners::default();
        listeners.push(Priority::Normal, |e: &mut PlayerQuit, _| {
            assert_eq!(e.player().session(), SessionId::new(1));
            QUITS.set(QUITS.get() + 1);
        });
        let (tx, mut rx) = mpsc::channel(1);
        let kind = InputKind::Join {
            identity: identity(1),
            outbound: tx,
        };
        let (other, mut rx2) = join(2, 0);
        // The join fills the queue; the radius answer does not fit.
        let state = run_ticks_with(
            settings(|_| {}),
            listeners,
            vec![
                vec![input(1, 0, kind), other],
                vec![radius(1, 1, 4)],
                vec![],
            ],
        );
        assert_eq!(sessions(&state), [2]);
        assert_eq!(received(&mut rx).2, START_GAME);
        assert!(rx.is_closed());
        // It left like any player: once, and the other player saw it go.
        assert_eq!(QUITS.get(), 1);
        let left = received(&mut rx2).0.into_iter().any(|p| {
            matches!(p, Packet::Text(t) if t.message == player::LEFT && t.parameters == ["player1"])
        });
        assert!(left, "the other player sees the left line");
    }

    #[test]
    fn the_hash_changes_with_the_difficulty_and_default_game_mode() {
        let base = run_ticks(settings(|_| {}), vec![]);
        let mut difficulty = run_ticks(settings(|_| {}), vec![]);
        assert!(
            crate::tick::Context {
                state: &mut difficulty
            }
            .set_difficulty(Difficulty::Peaceful)
        );
        let mut game_mode = run_ticks(settings(|_| {}), vec![]);
        crate::tick::Context {
            state: &mut game_mode,
        }
        .set_default_game_mode(GameMode::Spectator);
        let hashes = [base.hash(), difficulty.hash(), game_mode.hash()];
        assert_ne!(hashes[0], hashes[1]);
        assert_ne!(hashes[0], hashes[2]);
        assert_ne!(hashes[1], hashes[2]);
    }

    /// The `PlayerList` packets shared among the messages queued on `rx`: those of spawn windows,
    /// then the others.
    fn shared_lists(
        rx: &mut mpsc::Receiver<Outbound>,
    ) -> (Vec<std::sync::Arc<Packet>>, Vec<std::sync::Arc<Packet>>) {
        let (mut windows, mut others) = (Vec::new(), Vec::new());
        while let Ok(Outbound::Send(messages)) = rx.try_recv() {
            for m in messages {
                match m {
                    Message::SpawnList(p) if matches!(*p, Packet::PlayerList(_)) => windows.push(p),
                    Message::SharedPacket(p) if matches!(*p, Packet::PlayerList(_)) => {
                        others.push(p);
                    }
                    _ => {}
                }
            }
        }
        (windows, others)
    }

    #[test]
    fn a_join_shares_each_skin_once() {
        joins_share_each_skin_once(hook::Hooks::default());
    }

    #[test]
    fn a_send_hook_that_changes_nothing_copies_nothing() {
        joins_share_each_skin_once(crate::Server::builder().on_send(|_, _| true).setup.hooks);
    }

    /// Five players with 3.5 MiB skins (under the 4 MiB a skin may hold, 17.5 MiB in all) are
    /// listed for a sixth in packets each of which fits a batch of the default size.
    #[test]
    fn a_join_among_large_skins_gets_a_list_every_batch_carries() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        let mut large = queue::Skin::default().get().clone();
        large.skin_image_width = 960;
        large.skin_image_height = 960;
        large.skin_data = vec![0; 960 * 960 * 4];
        let large = queue::Skin::new(std::sync::Arc::new(large));
        assert!(large.is_acceptable());
        let mut joins = Vec::new();
        let mut receivers = Vec::new();
        for s in 1..=5 {
            let (tx, rx) = outbound_queue();
            let identity = Identity {
                skin: large.clone(),
                ..identity(s)
            };
            let kind = InputKind::Join {
                identity,
                outbound: tx,
            };
            joins.extend([input(s, 0, kind), radius(s, 1, 2)]);
            receivers.push(rx);
        }
        tick(&mut state, &mut joins, &mut output);
        let (j, mut rx) = join(6, 0);
        tick(&mut state, &mut vec![j, radius(6, 1, 2)], &mut output);
        let settings = crate::protocol::batch::Settings::default();
        let mut encoder = crate::protocol::batch::Encoder::new(settings);
        let mut listed = 0;
        while let Ok(Outbound::Send(messages)) = rx.try_recv() {
            for m in messages {
                let packet = match m {
                    Message::Packet(p) => p,
                    Message::SharedPacket(p) | Message::SpawnList(p) => (*p).clone(),
                    Message::Shared(_) => continue,
                };
                if let Packet::PlayerList(l) = &packet {
                    listed += l.entries.len();
                }
                let header = crate::protocol::batch::Header {
                    id: packet.id(),
                    ..Default::default()
                };
                let mut payload = Vec::new();
                crate::protocol::batch::encode_packets(
                    [(header, &packet)],
                    crate::item::SHIELD_RUNTIME_ID,
                    &mut payload,
                    |p| encoder.encode(p, &mut Vec::new()),
                )
                .expect("every packet fits a batch");
            }
        }
        // The start game window and the spawn window each list all six.
        assert_eq!(listed, 12);
    }

    /// Three players join in one tick and a fourth later, with `hooks`: every player is sent the
    /// same lists, which hold the players' own skins.
    fn joins_share_each_skin_once(hooks: hook::Hooks) {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        state.hooks = hooks;
        let mut output = Output::default();
        let mut joins = Vec::new();
        let mut receivers = Vec::new();
        for s in 1..=3 {
            let (j, rx) = join(s, 0);
            joins.extend([j, radius(s, 1, 2)]);
            receivers.push(rx);
        }
        tick(&mut state, &mut joins, &mut output);
        // Every window of the tick (start game and spawn window) sends one and the same list,
        // marked as a spawn window's.
        let windows: Vec<_> = receivers.iter_mut().map(shared_lists).collect();
        let roster = std::sync::Arc::clone(&windows[0].0[0]);
        for (lists, others) in &windows {
            assert_eq!((lists.len(), others.len()), (2, 0));
            assert!(lists.iter().all(|l| std::sync::Arc::ptr_eq(l, &roster)));
        }
        // Each entry holds its player's own skin, not a copy.
        let Packet::PlayerList(list) = &*roster else {
            panic!("not a player list");
        };
        assert_eq!(list.entries.len(), 3);
        for entry in &list.entries {
            let player = state
                .players
                .values()
                .find(|p| p.identity.uuid == entry.uuid)
                .expect("a listed player");
            assert!(std::sync::Arc::ptr_eq(
                &entry.skin,
                player.identity.skin.shared()
            ));
        }
        // A later join is announced to the others with one shared entry, not a spawn window's.
        let (j, _rx4) = join(4, 0);
        tick(&mut state, &mut vec![j], &mut output);
        let announced: Vec<_> = receivers.iter_mut().map(shared_lists).collect();
        let entry = std::sync::Arc::clone(&announced[0].1[0]);
        for (windows, lists) in &announced {
            assert_eq!((windows.len(), lists.len()), (0, 1));
            assert!(std::sync::Arc::ptr_eq(&lists[0], &entry));
        }
    }

    #[test]
    fn packet_listener_panic_log_reproduces() {
        fn panicking() -> Listeners {
            let mut listeners = Listeners::default();
            listeners.push(
                Priority::Normal,
                |_: &mut crate::event::PlayerViewDistanceChange, _| {
                    panic!("listener failed");
                },
            );
            listeners
        }
        fn run(state: &mut State, mut batch: Vec<Input>, output: &mut Output, log: &mut InputLog) {
            let mut answers = std::mem::take(&mut output.answers);
            POOL.with(|pool| step(state, &mut batch, &mut answers, pool, output, log));
            deliver(state, output, log);
            boundary(state, output, log);
            let requests = std::mem::take(&mut output.requests);
            output.answers = answer(state, requests);
        }
        let mut state = script_state(panicking());
        let mut output = Output::default();
        let mut log = InputLog::default();
        let (j, _rx) = join(1, 0);
        run(&mut state, vec![j], &mut output, &mut log);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(&mut state, vec![radius(1, 1, 4)], &mut output, &mut log);
        }));
        assert!(
            panicked.is_err(),
            "the chunk radius raises the panicking event"
        );
        // The tick that panicked holds the packet that raised the event; a replay of the ticks
        // before it runs, and of the whole log panics again.
        let ticks = log.ticks();
        let (last, before) = ticks.split_last().unwrap();
        assert!(
            last.inputs
                .iter()
                .any(|e| matches!(e.kind, EntryKind::Packet(Packet::RequestChunkRadius(_)))),
            "{last:?}"
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
            let answer = packets
                .iter()
                .find(|p| matches!(p, Packet::ChunkRadiusUpdated(_)));
            assert_eq!(answer, Some(&Packet::ChunkRadiusUpdated(expected)));
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
                START_GAME,
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
        let Some(Packet::StartGame(start)) =
            packets.iter().find(|p| matches!(p, Packet::StartGame(_)))
        else {
            panic!("StartGame is sent");
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
        // The start-game packets, then the spawn window up to the chunk radius answer, the first
        // NetworkChunkPublisherUpdate and a chunk (BDS, P6; the survival tests check it).
        let window = START_GAME.len();
        assert_eq!(sent[..window], START_GAME);
        assert_eq!(sent[window..window + 3], [43, 344, 60]);
        let answer = sent.iter().position(|&id| id == 70).unwrap();
        assert_eq!(sent[answer..answer + 3], [70, 121, 58]);
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
            skin: crate::tick::queue::Skin::default(),
            device_os: -1,
            address: None,
            client: crate::tick::queue::Client::default(),
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
            all.push(priority, move |e: &mut PlayerDuplicateLogin, _| {
                listener(e);
            });
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

    const LOGGED_IN_OTHER_LOCATION: (i32, &str) = (43, "disconnectionScreen.loggedinOtherLocation");

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
            assert_eq!(received(&mut rx2).2, START_GAME, "the new player joined");
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
            // Registered out of order: Lowest runs first, so Highest decides.
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

    /// A second login the whitelist keeps out does not disconnect the player already online.
    #[test]
    fn a_refused_duplicate_login_leaves_the_existing_player() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        let (a, mut rx1) = join_as(1, 0, person("Alex", Some(5)));
        tick(&mut state, &mut vec![a], &mut output);
        received(&mut rx1);
        crate::Context { state: &mut state }.set_whitelist_enabled(true);
        let (b, mut rx2) = join_as(2, 0, person("Alex", Some(5)));
        tick(&mut state, &mut vec![b], &mut output);
        assert_eq!(sessions(&state), [1]);
        assert!(state.index_matches_players());
        assert!(!rx1.is_closed(), "the existing player stays");
        assert_eq!(kicked(&mut rx2).1, "disconnectionScreen.notAllowed");
    }

    /// A second login a `PlayerLogin` listener refuses does not disconnect the player already
    /// online either.
    #[test]
    fn a_duplicate_login_a_listener_refuses_leaves_the_existing_player() {
        let mut listeners = Listeners::default();
        let mut logins = 0_u32;
        listeners.push(Priority::Normal, move |e: &mut PlayerLogin, _| {
            logins += 1;
            if logins == 2 {
                e.set_kick_flag(KickFlag::Plugin, "one Alex is enough", None);
            }
        });
        let (a, _rx1) = join_as(1, 0, person("Alex", Some(5)));
        let (b, mut rx2) = join_as(2, 0, person("Alex", Some(5)));
        let state = run_ticks_with(settings(|_| {}), listeners, vec![vec![a], vec![b]]);
        assert_eq!(sessions(&state), [1]);
        assert_eq!(kicked(&mut rx2).1, "one Alex is enough");
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
        listeners.push(Priority::Normal, move |e: &mut PlayerDuplicateLogin, _| {
            calls += 1;
            if calls.is_multiple_of(2) {
                e.cancel();
            }
        });
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

    /// A fixed-seed script of inputs for `sessions` sessions: joins (some under one name, so
    /// duplicate logins happen), chunk radius requests (some invalid), right and wrong
    /// initialisations, moves (some to positions that are not finite), sub-chunk requests, packets
    /// the tick does not handle, leaves, and sessions whose outbound queue closes, which the tick
    /// must forget; and the game inputs of each session's scripted [`client::Client`]; and the
    /// measures a live server logs (tick times, connection counts, pings). The state also runs
    /// [`script_task`]. A [`busy`](Self::busy) script adds the API inputs of [`busy`]: chat,
    /// commands, form answers and blocking job results.
    struct Script {
        rng: crate::protocol::Rng,
        sessions: usize,
        seq: Vec<u64>,
        joined: Vec<bool>,
        /// Receivers of joined sessions; `None` once the session dropped its end.
        receivers: BTreeMap<u64, Option<mpsc::Receiver<Outbound>>>,
        /// What each session's client learnt from its packets.
        clients: BTreeMap<u64, client::Client>,
        /// The runtime IDs of the villagers any client was shown.
        villagers: BTreeSet<u64>,
        /// What the clients received, by kind.
        seen: client::Seen,
        /// Whether the script sends the API inputs too.
        extras: bool,
        /// The forms each session was sent and has not answered, by session.
        forms: BTreeMap<u64, Vec<u32>>,
        /// The sequence number of the networking's last report.
        network_seq: u64,
    }

    impl Script {
        fn new(seed: u64, sessions: usize) -> Self {
            Self {
                rng: crate::protocol::Rng(seed),
                sessions,
                seq: vec![0; sessions],
                joined: vec![false; sessions],
                receivers: BTreeMap::new(),
                clients: BTreeMap::new(),
                villagers: BTreeSet::new(),
                seen: client::Seen::new(),
                extras: false,
                forms: BTreeMap::new(),
                network_seq: 0,
            }
        }

        /// The script of `seed` with the API inputs.
        fn busy(seed: u64, sessions: usize) -> Self {
            Self {
                extras: true,
                ..Self::new(seed, sessions)
            }
        }

        /// The inputs of one tick, in an arrival order unlike the applied (session, seq) order.
        /// The game inputs follow what the clients were sent and where their players are.
        #[expect(
            clippy::too_many_lines,
            reason = "consistency: one generator keeps the input mix in one place"
        )]
        fn tick(&mut self, state: &State) -> Vec<Input> {
            let mut batch = Vec::new();
            if self.extras {
                batch.extend(busy::job_results(&mut self.rng, state));
            }
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
                    self.clients.insert(s, client::Client::default());
                    let mut identity = person(&name, None);
                    identity.client = queue::Client {
                        locale: "en_US".to_owned(),
                        input_mode: crate::protocol::consts::InputMode::Touch,
                        device_model: format!("model {s}"),
                        device_id: format!("device-{s}"),
                        self_signed_id: format!("00000000-0000-4000-8000-{s:012}"),
                        random_id: i64::try_from(s).unwrap(),
                        playfab_id: format!("{s:016X}"),
                        game_version: "1.26.52".to_owned(),
                        server_address: "play.example.com:19132".to_owned(),
                        port: 40_000 + u16::try_from(s).unwrap(),
                        join_time_ms: 1_700_000_000_000 + i64::try_from(seq).unwrap(),
                    };
                    let kind = InputKind::Join {
                        identity,
                        outbound: tx,
                    };
                    batch.push(input(s, seq, kind));
                    continue;
                }
                if self.extras && self.rng.below(6) == 0 {
                    let forms = self.forms.entry(s).or_default();
                    let p = busy::client_input(&mut self.rng, forms);
                    batch.push(input(s, seq, round_trip(p)));
                    continue;
                }
                let kind = match self.rng.below(40) {
                    14.. => {
                        let player = state.players.get(&SessionId::new(s));
                        let packets = match player {
                            Some(p) if p.phase == Phase::Spawned => {
                                let client = self.clients.entry(s).or_default();
                                client.inputs(&mut self.rng, state, p, &self.villagers)
                            }
                            // What a client sends to get spawned: its chunk radius, then the
                            // right initialisation.
                            Some(p) if p.phase == Phase::Joined => {
                                vec![Packet::RequestChunkRadius(RequestChunkRadius {
                                    chunk_radius: 4,
                                    max_chunk_radius: 0,
                                })]
                            }
                            Some(p) => vec![Packet::SetLocalPlayerAsInitialised(
                                SetLocalPlayerAsInitialised {
                                    entity_runtime_id: p.runtime_id,
                                },
                            )],
                            None => Vec::new(),
                        };
                        let mut packets = packets.into_iter();
                        let Some(first) = packets.next() else {
                            continue;
                        };
                        batch.push(input(s, seq, round_trip(first)));
                        for p in packets {
                            self.seq[i] += 1;
                            batch.push(input(s, self.seq[i], round_trip(p)));
                        }
                        continue;
                    }
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
            self.measures(state, &mut batch);
            batch.reverse();
            batch
        }

        /// The measures a live server logs, from the tick number alone (the random stream stays
        /// the inputs' own): the previous tick's time on every tick after the first, the
        /// connection counts every fifth tick, and every tenth tick a ping of each joined
        /// session.
        fn measures(&mut self, state: &State, batch: &mut Vec<Input>) {
            let tick = state.tick;
            if tick > 0 {
                let micros = u32::try_from(tick * 7_919 % 90_000).unwrap();
                batch.push(Input {
                    session: SessionId::ENGINE,
                    seq: tick,
                    kind: InputKind::Measure(queue::Measure::TickTime(micros)),
                });
            }
            let joined = u32::try_from(self.joined.iter().filter(|&&j| j).count()).unwrap();
            if tick.is_multiple_of(5) {
                self.network_seq += 1;
                batch.push(Input {
                    session: SessionId::NETWORK,
                    seq: self.network_seq,
                    kind: InputKind::Measure(queue::Measure::Connections {
                        all: joined + 3,
                        logged_in: joined,
                    }),
                });
            }
            for i in 0..self.sessions {
                let s = u64::try_from(i).unwrap();
                if self.joined[i] && (tick + s).is_multiple_of(10) {
                    self.seq[i] += 1;
                    let millis = u32::try_from((tick + s) % 300).unwrap();
                    let kind = InputKind::Measure(queue::Measure::Ping(millis));
                    batch.push(input(s, self.seq[i], kind));
                }
            }
        }

        /// What every session received since the last call, by session; each client learns from
        /// its packets.
        fn received(&mut self) -> Vec<(u64, Vec<u32>, Option<Packet>)> {
            let mut out = Vec::new();
            for (s, rx) in &mut self.receivers {
                let Some(rx) = rx.as_mut() else {
                    continue;
                };
                let (packets, close, ids) = received(rx);
                let client = self.clients.entry(*s).or_default();
                for p in &packets {
                    client.learn(p, &mut self.seen);
                    if let Packet::AddActor(a) = p
                        && a.entity_type == "minecraft:villager_v2"
                    {
                        self.villagers.insert(a.entity_runtime_id);
                    }
                    if let Packet::ModalFormRequest(f) = p {
                        *self.seen.entry("form").or_default() += 1;
                        let forms = self.forms.entry(*s).or_default();
                        if forms.len() < busy::MAX_REMEMBERED_FORMS {
                            forms.push(f.form_id);
                        }
                    }
                }
                if !ids.is_empty() || close.is_some() {
                    out.push((*s, ids, close));
                }
            }
            out
        }
    }

    /// `packet` as the input log gives it back to a replay: it must decode to itself.
    fn round_trip(packet: Packet) -> InputKind {
        let mut bytes = Vec::new();
        packet.encode(&mut bytes, 0).unwrap();
        let mut budget = usize::MAX;
        let decoded = Packet::decode(packet.id(), &bytes, &mut budget, 0).unwrap();
        assert_eq!(decoded, packet, "the packet does not survive its codec");
        InputKind::Packet {
            packet,
            size: bytes.len(),
        }
    }

    /// The item of a block kind.
    fn block_item(kind: crate::block::Kind, count: u8) -> crate::item::Stack {
        let crate::block::ItemForm::Item(t) = kind.default_state().item() else {
            panic!("{kind:?} has an item");
        };
        crate::item::Stack::new(t, count)
    }

    /// The task of the determinism runs: every other tick it sets a block of the spawn chunks
    /// (always loaded) to stone or air, and every 50 ticks it saves. Every tick it gives a player
    /// items, and it spawns villager NPCs trading apples for bread, removing the first one later.
    fn script_task() -> Tasks {
        use crate::block::Kind as B;
        use crate::item::{Kind as I, Stack};
        let stone = crate::block::State::find("minecraft:stone", &[]).unwrap();
        let item = |kind: I, count: u8| Stack::new(kind.default_type(), count);
        let gifts = [
            block_item(B::OakLog, 8),
            block_item(B::Chest, 2),
            block_item(B::Dirt, 16),
            item(I::Snowball, 8),
            block_item(B::Cobblestone, 16),
            item(I::Bread, 4),
            item(I::Apple, 12),
            block_item(B::CraftingTable, 1),
            item(I::Bow, 1),
            item(I::Arrow, 16),
            item(I::Egg, 4),
            block_item(B::Barrel, 1),
            item(I::DiamondSword, 1),
            block_item(B::Furnace, 1),
            item(I::Diamond, 3),
        ];
        let offer = |buy: Stack, sell: Stack| crate::custom::Offer::new(buy, sell, 12);
        let offers = [
            offer(item(I::Apple, 2), item(I::Bread, 1)),
            offer(block_item(B::Dirt, 4), item(I::Arrow, 2)),
        ];
        let mut villagers = Vec::new();
        let mut tasks = Tasks::default();
        tasks.push(
            std::num::NonZeroU32::MIN,
            move |ctx: &mut task::Context<'_>| {
                let tick = ctx.tick();
                let t = i32::try_from(tick % 1000).unwrap();
                if t % 2 == 0 {
                    let at = BlockPos::new((t * 7) % 48 - 16, -60 + t % 3, (t * 13) % 48 - 16);
                    let state = if t % 3 == 0 { crate::block::AIR } else { stone };
                    assert!(ctx.default_world_mut().set_block(at, state).is_ok());
                }
                // A gift a tick; 15 gifts and 64 players, so every player meets most gifts.
                let name = if tick.is_multiple_of(8) {
                    "Twin".to_owned()
                } else {
                    format!("p{}", tick % 64)
                };
                let gift = gifts[usize::try_from(tick).unwrap() % gifts.len()].clone();
                if let Some(player) = ctx.player_exact(&name).and_then(|id| ctx.player_mut(id)) {
                    player
                        .inventory_mut(crate::inventory::Kind::Main)
                        .add(&gift);
                }
                if tick % 150 == 30 {
                    let x = 2.5 + f64::from(u32::try_from(tick / 150).unwrap()) * 2.0;
                    let at = crate::math::Vec3::new(x, -60.0, 3.5);
                    villagers.extend(
                        ctx.spawn_villager(crate::world::Id::DEFAULT, at, 1, &offers)
                            .ok(),
                    );
                }
                if tick == 260
                    && let Some(&first) = villagers.first()
                {
                    ctx.entity_mut(first).unwrap().flag_for_despawn();
                }
                // The runs leave commits unanswered, so the first snapshot is the only one: it
                // comes after the first villager and gifts.
                if tick % 50 == 49 {
                    ctx.save();
                }
                // Network controls: a new server name, and a block that ends.
                if tick % 100 == 40 {
                    assert!(ctx.set_motd(format!("Anchor {tick}")));
                    let ip = std::net::IpAddr::from([192, 0, 2, 1]);
                    assert!(ctx.block_address(ip, Duration::from_secs(5)));
                }
            },
        );
        tasks
    }

    /// Ticks of a determinism run.
    const TICKS: usize = 400;

    /// The start state of the determinism runs, with `listeners` (sorted): the spawn area loaded,
    /// [`script_task`] and Anchor's commands.
    fn script_state(listeners: Listeners) -> State {
        let mut state = new_state(settings(|_| {}), listeners);
        state.tasks = script_task();
        state.commands.finish().unwrap();
        state
    }

    /// Runs `script` live with a pool of `threads` threads and `listeners` (sorted): the observed
    /// ticks, the log and what the clients received.
    fn live(
        threads: usize,
        mut script: Script,
        listeners: Listeners,
    ) -> (Vec<Observed>, InputLog, client::Seen) {
        let pool = crate::runtime::cpu_pool(threads).unwrap();
        let mut state = script_state(listeners);
        let mut output = Output::default();
        let mut log = InputLog::default();
        let mut observed = Vec::new();
        let mut extras_saved = 0;
        let mut restored = 0;
        for _ in 0..TICKS {
            let mut batch = script.tick(&state);
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
            for request in &requests {
                if let Request::Commit(snapshot) = request {
                    extras_saved += snapshot.extras.len();
                    // The state of every save boundary round-trips through a checkpoint.
                    let fresh = script_state(Listeners::default());
                    restored +=
                        usize::from(checkpoint::tests::assert_round_trip(&state, fresh, &pool));
                }
            }
            // Commits stay unanswered: these runs replay from the start, with no server file, and
            // an answered commit would let the log drop the ticks before it (tested apart).
            output.answers = answer(&state, requests);
            output
                .answers
                .retain(|a| !matches!(a, Response::Committed { .. }));
            assert!(state.index_matches_players());
            observed.push((state.hash(), script.received()));
        }
        assert!(pool.shutdown());
        assert!(extras_saved > 0, "the script saved no chunk extras");
        // The API inputs leave forms and jobs waiting, whose closures make a checkpoint opaque.
        assert!(
            restored > 0 || script.extras,
            "no save boundary was restored from its checkpoint"
        );
        (observed, log, script.seen)
    }

    /// Replays `ticks` of a log from the start state: the state hash after each tick and the log
    /// the replay records itself. Sessions the live run forgot after its output are forgotten at
    /// the same point.
    fn replay(ticks: &[log::Tick], listeners: Listeners, pool: &Pool) -> (Vec<u64>, InputLog) {
        let mut state = script_state(listeners);
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
                        EntryKind::Console(line) => InputKind::Console(line.clone()),
                        EntryKind::Job(outcome) => InputKind::Job(outcome.clone()),
                        EntryKind::Measure(measure) => InputKind::Measure(*measure),
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
                    log::Answer::Chunk {
                        world,
                        pos,
                        origin,
                        checksum,
                    } => Response::Chunk {
                        world: *world,
                        pos: *pos,
                        loaded: match origin {
                            log::Origin::Damaged => Loaded::Damaged,
                            log::Origin::Disk | log::Origin::Generated => {
                                let seed = state.worlds[*world].seed();
                                // What the load reads again must be what the log saw.
                                let stored = extra::tests::sample(*world, *pos);
                                assert_eq!(stored.checksum, *checksum);
                                let chunk = Box::new(Chunk::generate(&Flat, seed, *pos));
                                extra::tests::sample_loaded(&state, *world, *pos, chunk)
                            }
                        },
                    },
                    log::Answer::Player { key, data } => Response::Player {
                        key: crate::storage::PlayerKey::from_text(key),
                        data: data.to_load().unwrap(),
                    },
                    log::Answer::Committed(start) => Response::Committed { start: *start },
                    log::Answer::CommitFailed(start) => Response::CommitFailed { start: *start },
                    log::Answer::LoadFailed => Response::LoadFailed,
                    log::Answer::Checked(found) => Response::Checked(found.clone()),
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
                dropped(&mut state, &mut log, session);
            }
            boundary(&mut state, &mut output, &mut log);
            output.requests.clear();
            hashes.push(state.hash());
        }
        (hashes, log)
    }

    /// Seeds of the determinism runs: different inputs, sessions colliding differently. Four
    /// seeds of 400 ticks keep the run short while each tick carries the game inputs.
    const SEEDS: [u64; 4] = [0x5eed_0005, 42, u64::MAX, 0x8000_0000_0000_0000];

    /// `check` of every run, the runs side by side on a pool of their own, results in run
    /// order. The runs are independent (seeds, server files), so this checks what running them in
    /// turn checks, in a fraction of the time.
    pub(super) fn side_by_side<T: Sync, R: Send>(
        runs: &[T],
        check: impl Fn(&T) -> R + Sync + Send,
    ) -> Vec<R> {
        let pool = crate::runtime::cpu_pool(runs.len()).unwrap();
        let out = pool.map(runs, check);
        assert!(pool.shutdown());
        out
    }

    /// What the scripted clients must have received over all seeds: proof the game inputs reached
    /// their rules (moves and crafts done and refused, windows, item entities, villagers, trade
    /// screens, broken and placed blocks, attacks).
    const REACHED: [&str; 11] = [
        "stack done",
        "stack refused",
        "window opened",
        "item entity",
        "villager",
        "entity",
        "trade window",
        "block update",
        "level event",
        "actor event",
        "inventory slot",
    ];

    #[test]
    fn one_and_many_pool_threads_give_the_same_ticks_and_the_log_replays_them() {
        let many = crate::runtime::logical_cpus().max(4);
        let seen = side_by_side(&SEEDS, |&seed| {
            let (one, log_one, seen) = live(1, Script::new(seed, 64), Listeners::default());
            let (more, log_more, seen_more) =
                live(many, Script::new(seed, 64), Listeners::default());
            assert_eq!(one, more, "seed {seed:#x}: 1 and {many} threads");
            assert_eq!(seen, seen_more, "seed {seed:#x}");
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
            for threads in [1, many] {
                let pool = crate::runtime::cpu_pool(threads).unwrap();
                let (hashes, again) = replay(&read.ticks(), Listeners::default(), &pool);
                assert_eq!(hashes, live_hashes, "seed {seed:#x}: replay");
                assert_eq!(
                    again.encode().unwrap(),
                    bytes,
                    "seed {seed:#x}: re-recorded"
                );
                assert!(pool.shutdown());
            }
            seen
        });
        let mut reached = client::Seen::new();
        for (what, n) in seen.into_iter().flatten() {
            *reached.entry(what).or_default() += n;
        }
        for what in REACHED {
            assert!(
                reached.get(what).is_some_and(|n| *n > 0),
                "the scripts never reached {what}: {reached:?}"
            );
        }
    }

    #[test]
    fn corrupt_logs_are_errors_without_panics() {
        let (_, log, _) = live(2, Script::new(SEEDS[0], 64), Listeners::default());
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
            listeners.push(Priority::Normal, |_: &mut PlayerDuplicateLogin, _| {
                panic!("listener failed");
            });
            listeners
        };
        let (sender, queue) = input_queue(1);
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
        // Polls `done` until it holds; the deadline only catches a broken server.
        let until = |done: &mut dyn FnMut() -> bool| {
            let end = clock::now() + Duration::from_secs(20);
            while !done() {
                if clock::now() >= end {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            true
        };
        let mut first = join(1);
        // The first join applies in a tick of its own before the duplicate arrives: its start
        // packets go out at the end of the tick that placed the player, after that tick took
        // its inputs.
        assert!(
            until(&mut || first.try_recv().is_ok()),
            "the first player joins"
        );
        let _second = join(2);
        assert!(until(&mut || thread.is_finished()), "the tick thread ends");
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
            before
                .iter()
                .any(|t| t.inputs.iter().any(
                    |e| e.session == SessionId::new(1) && matches!(e.kind, EntryKind::Join(_))
                )),
            "the first join is in a tick before the duplicate's"
        );
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

    // Chunk views, sub-chunk answers, block changes, saving.

    /// Moves a player's eyes to `position` as a teleport does (view and save tests; a
    /// client's own moves are checked by the survival rules, which allow at most 15 blocks).
    pub(super) fn teleported(state: &mut State, session: u64, position: [f32; 3]) {
        let player = state.players.get_mut(&SessionId::new(session)).unwrap();
        player.position = Vec3::from_f32(position);
        player.data_dirty = true;
    }

    pub(super) fn moved_to(session: u64, seq: u64, position: [f32; 3]) -> Input {
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
                Vec3::new(0.5, -58.38, 0.5)
            );
        }
        teleported(&mut state, 1, [160.5, -58.38, 0.5]);
        tick(&mut state, &mut Vec::new(), &mut output);
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
    fn the_publisher_follows_each_block_the_player_moves_to_inside_a_chunk() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 2)], &mut output);
        settle(&mut state, &mut output);
        received(&mut rx);
        let publishers = |rx: &mut _| -> Vec<[i32; 3]> {
            received(rx)
                .0
                .iter()
                .filter_map(|p| match p {
                    Packet::NetworkChunkPublisherUpdate(p) => Some(p.position),
                    _ => None,
                })
                .collect()
        };
        teleported(&mut state, 1, [3.5, -58.38, 0.5]);
        tick(&mut state, &mut Vec::new(), &mut output);
        assert_eq!(publishers(&mut rx), [[3, -59, 0]]);
        // Inside the same block: nothing new to say.
        teleported(&mut state, 1, [3.9, -58.38, 0.9]);
        tick(&mut state, &mut Vec::new(), &mut output);
        assert!(publishers(&mut rx).is_empty());
    }

    /// At the most `chunks-per-tick`, a client that asks for each chunk it gets with a request of
    /// its own has every request of a tick answered in that tick: none waits, none is dropped.
    #[test]
    fn a_request_for_each_chunk_at_the_most_chunks_per_tick_is_answered_in_its_tick() {
        const MOST: u32 = 256;
        let settings = settings(|c| c.developer.chunks_per_tick = MOST);
        let mut state = new_state(settings, Listeners::default());
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 2)], &mut output);
        let mut seq = 1;
        for _ in 0..4 {
            let mut requests: Vec<_> = (0..MOST)
                .map(|_| {
                    seq += 1;
                    sub_chunks(1, seq, [0, 0, 0], vec![[0, 0, 0]])
                })
                .collect();
            tick(&mut state, &mut requests, &mut output);
            let answers = received(&mut rx)
                .0
                .iter()
                .filter(|p| matches!(p, Packet::SubChunk(_)))
                .count();
            assert_eq!(answers, usize::try_from(MOST).unwrap());
        }
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
                        ctx.default_world_mut().set_block(pos, state).unwrap();
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
        teleported(&mut state, 1, [150.5, -58.38, 0.5]);
        tick(&mut state, &mut Vec::new(), &mut output);
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
        teleported(&mut state, 1, [0.5, -58.38, 0.5]);
        tick(&mut state, &mut Vec::new(), &mut output);
        assert!(!state.worlds[0].is_chunk_loaded(pos));
        assert!(
            state
                .chunks
                .unsaved
                .get(&(0, pos))
                .is_some_and(|u| u.snapshot.is_none())
        );
        // Coming back takes it from memory, with its change and still dirty.
        teleported(&mut state, 1, [150.5, -58.38, 0.5]);
        tick(&mut state, &mut Vec::new(), &mut output);
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
        teleported(&mut state, 1, [3.5, -58.38, 3.5]);
        tick(&mut state, &mut vec![radius(1, 1, 2)], &mut output);
        tick(&mut state, &mut vec![leave(1, 3)], &mut output);
        assert_eq!(state.saving.offline.len(), 1);
        let (j, _rx) = join_as(2, 0, identity(1));
        tick(&mut state, &mut vec![j], &mut output);
        let player = &state.players[&SessionId::new(2)];
        assert_eq!(
            (player.phase, player.position),
            (Phase::Joined, Vec3::from_f32([3.5, -58.38, 3.5]))
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
        let (_sender, queue) = input_queue(1);
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

    #[test]
    fn a_failed_chunk_load_stops_the_tick_like_a_failed_commit() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        let mut log = InputLog::default();
        log.begin(0);
        respond(&mut state, Response::LoadFailed, &mut output, &mut log);
        assert!(output.save_failed);
        assert_eq!(log.ticks()[0].responses, [log::Answer::LoadFailed]);
    }
}
