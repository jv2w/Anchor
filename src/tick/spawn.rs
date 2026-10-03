//! Joining and spawning: the saved player data, the start-game packets, the chunk radius and the
//! spawn status.
//!
//! The packet order and the `StartGame` values follow what BDS 1.26.52 sends (P6, PLAN.md 10.2,
//! 10.6); the packets BDS sends that Anchor does not yet are listed in
//! `xtask/data/p6/deferred.txt`.

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use std::sync::Arc;

use super::queue::{Identity, Message, Outbound, SessionId, SharedBatches};
use super::save::storage_key;
use super::view::{self, Changes, Encoded};
use super::{
    DISCONNECT_REASON_LOGGED_IN_OTHER_LOCATION, Output, Phase, Player, PlayerKey, Settings, State,
};
use crate::config::{Difficulty, GameMode};
use crate::crafting;
use crate::event::{DuplicateOutcome, PlayerDuplicateLogin};
use crate::item::{self, SHIELD_RUNTIME_ID};
use crate::pool::Pool;
use crate::protocol::batch;
use crate::protocol::packet::{
    self, ChunkRadiusUpdated, Disconnect, Packet, PlayStatus, RequestChunkRadius,
    SetLocalPlayerAsInitialised, SetSpawnPosition, StartGame,
};
use crate::protocol::{PlayerMovementSettings, ServerJoinInformation};
use crate::save::{PlayerData, PlayerLoad, Request};
use crate::storage::PlayerKey as StoredPlayer;
use crate::world::{World, biome};

/// `PlayStatus` after the spawn chunks (gophertunnel `PlayStatusPlayerSpawn`).
const PLAY_STATUS_PLAYER_SPAWN: i32 = 3;
/// `PlayStatus` that rejects a join (gophertunnel `PlayStatusLoginFailedServerFull`).
const PLAY_STATUS_SERVER_FULL: i32 = 7;
/// `Disconnect` reason without a specific meaning (gophertunnel `DisconnectReasonUnknown`).
const DISCONNECT_REASON_UNKNOWN: i32 = 0;
/// What a player whose saved data is damaged sees.
const DAMAGED_DATA_MESSAGE: &str = "Your saved player data is damaged. Ask the server operator.";
/// Eye height above the feet.
const EYE_HEIGHT: f32 = 1.62;
/// Generator 2, flat: what BDS sends for a flat world (P6).
const GENERATOR_FLAT: i32 = 2;
/// `PlayerGameMode` 5: the player uses the world game mode, as BDS sends for a new player (P6).
const PLAYER_GAME_MODE_WORLD: i32 = 5;
/// The limited world size BDS sends (P6); the client ignores it for infinite worlds.
const LIMITED_WORLD_SIZE: i32 = 16;
/// BDS rewinds 40 ticks of movement history for server-authoritative movement (P6).
const REWIND_HISTORY_SIZE: i32 = 40;
/// Player permission 1, member.
const PERMISSION_MEMBER: u8 = 1;
/// The position BDS's `SetSpawnPosition` carries for a default spawn (P6).
const DEFAULT_SPAWN: [i32; 3] = [i32::MIN; 3];
/// The dimension BDS's `SetSpawnPosition` names at join (P6).
const SPAWN_POSITION_DIMENSION: i32 = 3;

/// The start-game batches every player gets the same (PLAN.md 10.4, 10.6): the item registry,
/// the biome definitions, the creative inventory and the recipes. Each is encoded and compressed once at start; sessions
/// only encrypt them.
#[derive(Debug, Clone)]
pub(crate) struct Shared {
    item_registry: SharedBatches,
    biome_definitions: SharedBatches,
    creative_content: SharedBatches,
    crafting_data: SharedBatches,
}

impl Shared {
    /// Builds the batches with the server's batch settings. Fails if a packet cannot be encoded
    /// or its batch is larger than `max-batch-bytes`.
    pub(crate) fn new(settings: batch::Settings) -> Result<Self, batch::Error> {
        let mut encoder = batch::Encoder::new(settings);
        Ok(Self {
            item_registry: encode(&mut encoder, &Packet::ItemRegistry(item::registry()))?,
            biome_definitions: encode(
                &mut encoder,
                &Packet::BiomeDefinitionList(biome::definition_list()),
            )?,
            creative_content: encode(
                &mut encoder,
                &Packet::CreativeContent(item::creative_content()),
            )?,
            crafting_data: encode(
                &mut encoder,
                &Packet::CraftingData(Box::new(crafting::crafting_data())),
            )?,
        })
    }
}

/// Encodes and compresses one packet into its batch bodies.
fn encode(encoder: &mut batch::Encoder, packet: &Packet) -> Result<SharedBatches, batch::Error> {
    let header = batch::Header {
        id: packet.id(),
        ..batch::Header::default()
    };
    let mut payload = Vec::new();
    let mut bodies = Vec::new();
    batch::encode_packets([(header, packet)], SHIELD_RUNTIME_ID, &mut payload, |p| {
        let mut body = Vec::new();
        encoder.encode(p, &mut body)?;
        bodies.push(body);
        Ok(())
    })?;
    Ok(bodies.into())
}

/// What the output phase sends a player at the end of a tick (PLAN.md 10.5). The input and view
/// phases set it and the output phase clears it, so it is empty between ticks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Pending {
    /// `StartGame` and the item registry.
    start_game: bool,
    /// The answer to the chunk radius requests of the tick.
    pub(super) radius: Option<RadiusAnswer>,
    /// `PlayStatus(PlayerSpawn)`: the spawn chunks were sent.
    pub(super) spawn_status: bool,
}

/// The answer to a chunk radius request.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct RadiusAnswer {
    /// The granted radius.
    radius: u8,
    /// The first answer: the spawn position, the creative inventory and the recipes go first.
    first: bool,
}

/// The output phase: the sub-chunks the answers need, encoded once each on the CPU pool; then the
/// packets of every player with something to send, built on the CPU pool from read-only state in
/// session order; then the pending marks are cleared.
pub(super) fn output(
    state: &mut State,
    pool: &Pool,
    changes: &Changes,
) -> Vec<(SessionId, Vec<Message>)> {
    let wanted = view::wanted_sub_chunks(state);
    let worlds = &state.worlds;
    let encoded: Encoded = wanted
        .iter()
        .copied()
        .zip(pool.map(&wanted, |&key| view::encode(worlds, key)))
        .collect();
    let marked: Vec<(SessionId, &Player)> = state
        .players
        .iter()
        .filter(|(_, p)| {
            p.pending != Pending::default()
                || !p.view.out.is_empty()
                || (!changes.is_empty() && p.view.is_open())
        })
        .map(|(&s, p)| (s, p))
        .collect();
    let (settings, shared) = (&state.settings, &state.shared);
    let packets: Vec<(SessionId, Vec<Message>)> = pool
        .map(&marked, |&(session, player)| {
            let world = worlds.get(player.world);
            (
                session,
                messages(settings, shared, world, player, &encoded, changes),
            )
        })
        .into_iter()
        .filter(|(_, m)| !m.is_empty())
        .collect();
    for player in state.players.values_mut() {
        player.pending = Pending::default();
        player.view.out = view::Out::default();
    }
    packets
}

/// One player's packets of the tick, in the order BDS sends them (P6, PLAN.md 10.2, 10.4, 10.6):
/// the start-game packets; for a chunk radius request the spawn position, the creative inventory
/// and the recipes (first answer only) and the granted radius; the chunk packets; the spawn
/// status; then the block changes it sees.
fn messages(
    settings: &Settings,
    shared: &Shared,
    world: Option<&World>,
    player: &Player,
    encoded: &Encoded,
    changes: &Changes,
) -> Vec<Message> {
    let mut out = Vec::new();
    let Some(world) = world else {
        return out;
    };
    if player.pending.start_game {
        out.push(Message::Packet(Packet::StartGame(Box::new(start_game(
            settings, world, player,
        )))));
        out.push(Message::Shared(Arc::clone(&shared.item_registry)));
    }
    if let Some(answer) = player.pending.radius {
        if answer.first {
            out.push(Message::Packet(Packet::SetSpawnPosition(
                SetSpawnPosition {
                    spawn_type: 0,
                    position: DEFAULT_SPAWN,
                    dimension: SPAWN_POSITION_DIMENSION,
                    spawn_position: DEFAULT_SPAWN,
                },
            )));
            out.push(Message::Shared(Arc::clone(&shared.biome_definitions)));
            out.push(Message::Shared(Arc::clone(&shared.creative_content)));
            out.push(Message::Shared(Arc::clone(&shared.crafting_data)));
        }
        out.push(Message::Packet(Packet::ChunkRadiusUpdated(
            ChunkRadiusUpdated {
                chunk_radius: i32::from(answer.radius),
            },
        )));
    }
    out.extend(
        view::packets(world, player, encoded)
            .into_iter()
            .map(Message::Packet),
    );
    if player.pending.spawn_status {
        out.push(Message::Packet(Packet::PlayStatus(PlayStatus {
            status: PLAY_STATUS_PLAYER_SPAWN,
        })));
    }
    out.extend(
        view::block_packets(player, changes)
            .into_iter()
            .map(Message::Packet),
    );
    out
}

/// Registers the player of `session`, or rejects it when the server is full. A second join of
/// the same session changes nothing. Returns the load of the player's saved data, if the tick
/// does not hold it; the player waits in [`Phase::Loading`] until it arrives.
///
/// When the same player is already online, [`PlayerDuplicateLogin`] decides which of the two is
/// disconnected (PLAN.md 10.3). That check comes before the full check: replacing the existing
/// player frees its slot, so a full server still lets a player take over its own place.
pub(super) fn join(
    state: &mut State,
    session: SessionId,
    mut identity: Identity,
    outbound: mpsc::Sender<Outbound>,
    output: &mut Output,
) -> Option<Request> {
    if state.players.contains_key(&session) {
        debug!(session = session.get(), "duplicate join ignored");
        return None;
    }
    let key = PlayerKey::of(&identity);
    if let Some(&existing) = state.index.get(&key) {
        let (back, outcome) = duplicate_login(state, existing, identity);
        identity = back;
        let packet = logged_in_other_location(outcome.message);
        if outcome.cancelled {
            info!(
                session = session.get(),
                existing = existing.get(),
                name = %identity.name,
                "join rejected: the player is already online"
            );
            output.close(outbound, packet);
            return None;
        }
        if let Some(old) = state.remove(existing) {
            info!(
                session = existing.get(),
                by = session.get(),
                name = %old.identity.name,
                "player replaced by a new login"
            );
            output.close(old.outbound, packet);
        }
    }
    let max_players = usize::try_from(state.settings.max_players).unwrap_or(usize::MAX);
    if state.players.len() >= max_players {
        info!(session = session.get(), name = %identity.name, "join rejected: server full");
        output.close(outbound, server_full());
        return None;
    }
    let Some(next) = state.next_runtime_id.checked_add(1) else {
        // 2^64 joins never happen in one process; refuse rather than reuse an ID.
        warn!(session = session.get(), "entity runtime IDs exhausted");
        output.close(outbound, server_full());
        return None;
    };
    let runtime_id = state.next_runtime_id;
    state.next_runtime_id = next;
    info!(session = session.get(), name = %identity.name, runtime_id, "player joined");
    let stored = storage_key(&key);
    let mut player = Player {
        runtime_id,
        identity,
        key,
        world: 0,
        position: [0.0; 3],
        data_dirty: false,
        chunk_radius: None,
        phase: Phase::Loading,
        view: view::View::default(),
        outbound,
        pending: Pending::default(),
    };
    let request = if !state.saving.players {
        place(state, &mut player, None);
        None
    } else if let Some(offline) = state.saving.offline.remove(&stored) {
        // The data of a player who left is newer than the file until its commit is known.
        let dirty = offline.snapshot.is_none();
        place(state, &mut player, Some(offline.data));
        player.data_dirty = dirty;
        None
    } else {
        state.loading.insert(stored.clone(), session);
        Some(Request::Player(stored))
    };
    state.insert(session, player);
    request
}

/// Puts a player into its saved world and position, or the default world's spawn, and marks the
/// start-game packets.
fn place(state: &State, player: &mut Player, data: Option<PlayerData>) {
    let saved = data.and_then(|d| {
        let world = state.worlds.iter().position(|w| w.name() == d.world)?;
        Some((world, d.position))
    });
    let (world, position) = saved.unwrap_or_else(|| (0, spawn_position(state.worlds.first())));
    player.world = world;
    player.position = position;
    player.phase = Phase::Joined;
    player.pending.start_game = true;
}

/// The eye position of a player standing on a world's spawn block.
fn spawn_position(world: Option<&World>) -> [f32; 3] {
    let spawn = world.map(World::spawn).unwrap_or_default();
    // Block coordinates are far inside f32's exact integer range for the world border.
    #[expect(
        clippy::cast_precision_loss,
        reason = "invariant: spawn coordinates are block coordinates within the world border"
    )]
    let [x, y, z] = [spawn.x, spawn.y, spawn.z].map(|c| c as f32);
    [x + 0.5, y + EYE_HEIGHT, z + 0.5]
}

/// The saved data of a loading player arrived: it joins where it was, or at the spawn if it is
/// new. Damaged data refuses the join and leaves the saved bytes as they are (PLAN.md 10.6).
pub(super) fn player_loaded(
    state: &mut State,
    key: &StoredPlayer,
    data: PlayerLoad,
    output: &mut Output,
) {
    let Some(session) = state.loading.remove(key) else {
        return;
    };
    let Some(mut player) = state.players.remove(&session) else {
        return;
    };
    if player.phase != Phase::Loading {
        state.players.insert(session, player);
        return;
    }
    match data {
        PlayerLoad::Found(data) => place(state, &mut player, Some(data)),
        PlayerLoad::Absent => place(state, &mut player, None),
        PlayerLoad::Damaged => {
            error!(
                player = key.as_str(),
                "saved player data is damaged; the join is refused and the data kept as saved"
            );
            state.players.insert(session, player);
            if let Some(p) = state.remove(session) {
                output.close(
                    p.outbound,
                    Packet::Disconnect(Disconnect {
                        reason: DISCONNECT_REASON_UNKNOWN,
                        hide_disconnection_screen: false,
                        message: DAMAGED_DATA_MESSAGE.to_owned(),
                        filtered_message: String::new(),
                    }),
                );
            }
            return;
        }
    }
    state.players.insert(session, player);
}

/// Raises [`PlayerDuplicateLogin`] for `connecting` against the player of `existing` and returns
/// `connecting` with the outcome. Without listeners the event is not built (PLAN.md 8.2): the
/// outcome is the default one and nothing is cloned.
fn duplicate_login(
    state: &mut State,
    existing: SessionId,
    connecting: Identity,
) -> (Identity, DuplicateOutcome) {
    let listeners = state.listeners.duplicate_login();
    if listeners.is_empty() {
        return (connecting, DuplicateOutcome::unheard());
    }
    let Some(old) = state.players.get(&existing) else {
        // The index names only joined players (State::insert and State::remove keep it so).
        warn!(
            session = existing.get(),
            "player index names a missing player"
        );
        return (connecting, DuplicateOutcome::unheard());
    };
    // One clone of the existing identity per duplicate login, only when someone listens.
    let mut event = PlayerDuplicateLogin::new(connecting, old.identity.clone());
    listeners.call(&mut event);
    event.into_outcome()
}

/// The packet that disconnects the losing side of a duplicate login.
fn logged_in_other_location(message: String) -> Packet {
    Packet::Disconnect(Disconnect {
        reason: DISCONNECT_REASON_LOGGED_IN_OTHER_LOCATION,
        hide_disconnection_screen: false,
        message,
        filtered_message: String::new(),
    })
}

/// Grants a chunk radius request: the view opens or resizes, and the output phase sends the
/// granted radius (the first time with the spawn position, the creative inventory and the
/// recipes). Requests in one tick share one answer with the last radius.
pub(super) fn request_chunk_radius(
    state: &mut State,
    session: SessionId,
    request: &RequestChunkRadius,
) {
    if request.chunk_radius < 1 {
        debug!(
            session = session.get(),
            radius = request.chunk_radius,
            "chunk radius below 1 ignored"
        );
        return;
    }
    let view_distance = state.settings.view_distance;
    let radius = u8::try_from(request.chunk_radius).map_or(view_distance, |r| r.min(view_distance));
    let Some(mut player) = state.players.remove(&session) else {
        return;
    };
    if player.phase == Phase::Loading {
        debug!(
            session = session.get(),
            "chunk radius before the player data ignored"
        );
    } else {
        let earlier = player.pending.radius.unwrap_or_default();
        player.pending.radius = Some(RadiusAnswer {
            radius,
            first: earlier.first || player.chunk_radius.is_none(),
        });
        player.chunk_radius = Some(radius);
        if player.view.radius() != radius {
            view::set_radius(state, &mut player, radius);
        }
    }
    state.players.insert(session, player);
}

/// Marks the player spawned when the client confirms with its own runtime ID.
pub(super) fn initialised(
    state: &mut State,
    session: SessionId,
    packet: &SetLocalPlayerAsInitialised,
) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if packet.entity_runtime_id != player.runtime_id {
        warn!(
            session = session.get(),
            expected = player.runtime_id,
            got = packet.entity_runtime_id,
            "initialised with a wrong runtime ID; ignored"
        );
        return;
    }
    match player.phase {
        Phase::Spawning => player.phase = Phase::Spawned,
        Phase::Spawned => {}
        Phase::Loading | Phase::Joined => warn!(
            session = session.get(),
            "initialised before spawning; ignored"
        ),
    }
}

/// The packet that rejects a join because the server is full.
fn server_full() -> Packet {
    Packet::PlayStatus(PlayStatus {
        status: PLAY_STATUS_SERVER_FULL,
    })
}

fn start_game(settings: &Settings, world: &World, player: &Player) -> StartGame {
    let game_mode = match settings.game_mode {
        GameMode::Survival => 0,
        GameMode::Creative => 1,
        GameMode::Adventure => 2,
        GameMode::Spectator => 6,
    };
    let difficulty = match settings.difficulty {
        Difficulty::Peaceful => 0,
        Difficulty::Easy => 1,
        Difficulty::Normal => 2,
        Difficulty::Hard => 3,
    };
    let spawn = world.spawn();
    StartGame {
        // Runtime IDs stay far below 2^63; the unique ID reuses it like most servers.
        entity_unique_id: i64::try_from(player.runtime_id).unwrap_or(i64::MAX),
        entity_runtime_id: player.runtime_id,
        player_game_mode: PLAYER_GAME_MODE_WORLD,
        player_position: player.position,
        world_seed: world.seed(),
        generator: GENERATOR_FLAT,
        world_game_mode: game_mode,
        hardcore: settings.hardcore,
        difficulty,
        world_spawn: [spawn.x, spawn.y, spawn.z],
        msa_gamer_tags_only: true,
        limited_world_width: LIMITED_WORLD_SIZE,
        limited_world_depth: LIMITED_WORLD_SIZE,
        multi_player_game: true,
        lan_broadcast_enabled: true,
        commands_enabled: true,
        player_permissions: PERMISSION_MEMBER,
        server_chunk_tick_radius: i32::from(settings.tick_radius),
        // BDS sends "*" (P6): any base game version.
        base_game_version: "*".to_owned(),
        world_name: settings.motd.clone(),
        player_movement_settings: PlayerMovementSettings {
            rewind_history_size: REWIND_HISTORY_SIZE,
            server_authoritative_block_breaking: true,
        },
        server_authoritative_inventory: true,
        server_authoritative_sound: true,
        server_join_information: Some(ServerJoinInformation::default()),
        game_version: packet::GAME_VERSION.to_owned(),
        use_block_network_id_hashes: true,
        ..StartGame::default()
    }
}
