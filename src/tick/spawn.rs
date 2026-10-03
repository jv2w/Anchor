//! Joining and spawning: the start-game packets, the spawn chunks and the spawn status.
//!
//! The packet order and the `StartGame` values follow what BDS 1.26.52 sends (P6, PLAN.md 10.2);
//! the packets BDS sends that Anchor does not yet are listed in `xtask/data/p6/deferred.txt`.

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::queue::{Identity, Outbound, SessionId};
use super::{Output, Phase, Player, Settings, State};
use crate::config::{Difficulty, GameMode};
use crate::protocol::packet::{
    self, ChunkRadiusUpdated, ItemRegistry, LevelChunk, NetworkChunkPublisherUpdate, Packet,
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

/// Registers the player of `session` and sends the start-game packets, or rejects it when the
/// server is full. A second join of the same session changes nothing.
pub(super) fn join(
    state: &mut State,
    session: SessionId,
    identity: Identity,
    outbound: mpsc::Sender<Outbound>,
    output: &mut Output,
) {
    if state.players.contains_key(&session) {
        debug!(session = session.get(), "duplicate join ignored");
        return;
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
        position: SPAWN_POSITION,
        chunk_radius: None,
        phase: Phase::Joined,
        outbound,
    };
    output.send(
        session,
        Packet::StartGame(Box::new(start_game(&state.settings, &player))),
    );
    // Items arrive in M4.
    output.send(session, Packet::ItemRegistry(ItemRegistry::default()));
    state.players.insert(session, player);
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
