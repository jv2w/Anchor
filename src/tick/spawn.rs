//! Joining and spawning: the start-game packets, the spawn chunks and the spawn status.
//!
//! The packet order and the `StartGame` values follow what BDS 1.26.52 sends (P6, PLAN.md 10.2);
//! the packets BDS sends that Anchor does not yet are listed in `xtask/data/p6/deferred.txt`.

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::queue::{Identity, Outbound, SessionId, SharedBatches};
use super::{
    DISCONNECT_REASON_LOGGED_IN_OTHER_LOCATION, Output, Phase, Player, PlayerKey, Settings, State,
};
use crate::config::{Difficulty, GameMode};
use crate::crafting;
use crate::event::{DuplicateOutcome, PlayerDuplicateLogin};
use crate::item::{self, SHIELD_RUNTIME_ID};
use crate::protocol::batch;
use crate::protocol::packet::{
    self, ChunkRadiusUpdated, Disconnect, LevelChunk, NetworkChunkPublisherUpdate, Packet,
    PlayStatus, RequestChunkRadius, SetLocalPlayerAsInitialised, StartGame,
};
use crate::protocol::{BlockPos, ChunkPos, PlayerMovementSettings, ServerJoinInformation};

/// `PlayStatus` after the spawn chunks (gophertunnel `PlayStatusPlayerSpawn`).
const PLAY_STATUS_PLAYER_SPAWN: i32 = 3;
/// `PlayStatus` that rejects a join (gophertunnel `PlayStatusLoginFailedServerFull`).
const PLAY_STATUS_SERVER_FULL: i32 = 7;
/// The world spawn block. The world is empty until M6; any fixed position works.
const SPAWN_BLOCK: BlockPos = [0, 64, 0];
/// Where players appear: the middle of the block above the spawn block, at eye height.
const SPAWN_POSITION: [f32; 3] = [0.5, 65.62, 0.5];
/// Overworld sub-chunk sections, y = -64 to 319.
const OVERWORLD_SECTIONS: usize = 24;
/// Generator 2, flat: what BDS sends for a flat world (P6). Anchor worlds are flat or void (an
/// empty flat world) until M6.
const GENERATOR_FLAT: i32 = 2;
/// `PlayerGameMode` 5: the player uses the world game mode, as BDS sends for a new player (P6).
const PLAYER_GAME_MODE_WORLD: i32 = 5;
/// The limited world size BDS sends (P6); the client ignores it for infinite worlds.
const LIMITED_WORLD_SIZE: i32 = 16;
/// BDS rewinds 40 ticks of movement history for server-authoritative movement (P6).
const REWIND_HISTORY_SIZE: i32 = 40;
/// Player permission 1, member.
const PERMISSION_MEMBER: u8 = 1;
/// The biome of the empty world: plains (ID 1).
const PLAINS: u8 = 1;

/// The start-game batches every player gets the same (PLAN.md 10.4): the item registry, the
/// creative inventory and the recipes. Each is encoded and compressed once at start; sessions
/// only encrypt them.
#[derive(Debug, Clone)]
pub(crate) struct Shared {
    item_registry: SharedBatches,
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

/// Registers the player of `session` and sends the start-game packets, or rejects it when the
/// server is full. A second join of the same session changes nothing.
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
) {
    if state.players.contains_key(&session) {
        debug!(session = session.get(), "duplicate join ignored");
        return;
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
            return;
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
        return;
    }
    let Some(next) = state.next_runtime_id.checked_add(1) else {
        // 2^64 joins never happen in one process; refuse rather than reuse an ID.
        warn!(session = session.get(), "entity runtime IDs exhausted");
        output.close(outbound, server_full());
        return;
    };
    let runtime_id = state.next_runtime_id;
    state.next_runtime_id = next;
    info!(session = session.get(), name = %identity.name, runtime_id, "player joined");
    let player = Player {
        runtime_id,
        identity,
        key,
        position: SPAWN_POSITION,
        chunk_radius: None,
        phase: Phase::Joined,
        outbound,
    };
    output.send(
        session,
        Packet::StartGame(Box::new(start_game(&state.settings, &player))),
    );
    output.share(session, &state.shared.item_registry);
    state.insert(session, player);
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

/// Answers a chunk radius request: the granted radius, the publisher position, the spawn chunks,
/// and the spawn status the first time.
pub(super) fn request_chunk_radius(
    state: &mut State,
    session: SessionId,
    request: &RequestChunkRadius,
    output: &mut Output,
) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if request.chunk_radius < 1 {
        debug!(
            session = session.get(),
            radius = request.chunk_radius,
            "chunk radius below 1 ignored"
        );
        return;
    }
    // BDS sends the creative inventory and the recipes after the chunk radius request, before
    // its answer (P6, PLAN.md 10.4).
    if player.phase == Phase::Joined && player.chunk_radius.is_none() {
        output.share(session, &state.shared.creative_content);
        output.share(session, &state.shared.crafting_data);
    }
    let view_distance = state.settings.view_distance;
    let radius = u8::try_from(request.chunk_radius).map_or(view_distance, |r| r.min(view_distance));
    player.chunk_radius = Some(radius);
    output.send(
        session,
        Packet::ChunkRadiusUpdated(ChunkRadiusUpdated {
            chunk_radius: i32::from(radius),
        }),
    );
    output.send(
        session,
        Packet::NetworkChunkPublisherUpdate(NetworkChunkPublisherUpdate {
            position: SPAWN_BLOCK,
            radius: u32::from(radius) << 4,
            saved_chunks: Vec::new(),
        }),
    );
    let center = [SPAWN_BLOCK[0] >> 4, SPAWN_BLOCK[2] >> 4];
    for position in spawn_chunks(center, radius.min(state.settings.spawn_radius)) {
        output.send(session, Packet::LevelChunk(empty_chunk(position)));
    }
    if player.phase == Phase::Joined {
        player.phase = Phase::Spawning;
        output.send(
            session,
            Packet::PlayStatus(PlayStatus {
                status: PLAY_STATUS_PLAYER_SPAWN,
            }),
        );
    }
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
        Phase::Joined => warn!(
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

fn start_game(settings: &Settings, player: &Player) -> StartGame {
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
    StartGame {
        // Runtime IDs stay far below 2^63; the unique ID reuses it like most servers.
        entity_unique_id: i64::try_from(player.runtime_id).unwrap_or(i64::MAX),
        entity_runtime_id: player.runtime_id,
        player_game_mode: PLAYER_GAME_MODE_WORLD,
        player_position: player.position,
        world_seed: settings.seed,
        generator: GENERATOR_FLAT,
        world_game_mode: game_mode,
        hardcore: settings.hardcore,
        difficulty,
        world_spawn: SPAWN_BLOCK,
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

/// The chunks within `radius` of `center` (x² + z² ≤ r²), nearest first, then by x and z.
fn spawn_chunks(center: ChunkPos, radius: u8) -> Vec<ChunkPos> {
    let r = i32::from(radius);
    let mut offsets: Vec<[i32; 2]> = (-r..=r)
        .flat_map(|x| (-r..=r).map(move |z| [x, z]))
        .filter(|[x, z]| x * x + z * z <= r * r)
        .collect();
    offsets.sort_unstable_by_key(|&[x, z]| (x * x + z * z, x, z));
    offsets
        .into_iter()
        .map(|[x, z]| [center[0].saturating_add(x), center[1].saturating_add(z)])
        .collect()
}

/// A chunk with no sub-chunks: plains biomes and no border blocks (dragonfly network encoding).
fn empty_chunk(position: ChunkPos) -> LevelChunk {
    LevelChunk {
        position,
        raw_payload: EMPTY_CHUNK_PAYLOAD.to_vec(),
        ..LevelChunk::default()
    }
}

/// The payload of [`empty_chunk`]: the first section's biomes as a single-entry palette
/// (header 0x01: 0 bits per entry, runtime form; zigzag varint 2 = plains), 0xff ("same as the
/// previous section") for the other 23 sections, then 0 border blocks.
const EMPTY_CHUNK_PAYLOAD: [u8; OVERWORLD_SECTIONS + 2] = {
    let mut payload = [0xff; OVERWORLD_SECTIONS + 2];
    payload[0] = 0x01;
    payload[1] = PLAINS << 1;
    payload[OVERWORLD_SECTIONS + 1] = 0;
    payload
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_chunks_form_a_disc_nearest_first() {
        let chunks = spawn_chunks([10, -3], 2);
        // r = 2: 5 × 5 minus the 4 corners and the 8 cells with x² + z² = 5.
        assert_eq!(chunks.len(), 13);
        assert_eq!(chunks[0], [10, -3]);
        assert!(chunks.contains(&[12, -3]) && !chunks.contains(&[11, -1]));
        assert_eq!(spawn_chunks([0, 0], 0), [[0, 0]]);
    }

    #[test]
    fn the_empty_chunk_payload_matches_the_dragonfly_layout() {
        let mut expected = vec![0x01, 0x02];
        expected.extend([0xff; 23]);
        expected.push(0x00);
        assert_eq!(EMPTY_CHUNK_PAYLOAD.as_slice(), expected);
    }
}
