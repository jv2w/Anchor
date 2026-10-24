//! Joining and spawning: the saved player data, the start-game packets, the chunk radius and the
//! spawn status.
//!
//! The packet order and the values follow what BDS 1.26.52 sends (P6, `xtask/data/p6/facts.md`);
//! the packets BDS sends that Anchor does not send are listed in `xtask/data/p6/deferred.txt`. The
//! player list is kept for everyone: a placed player gets the whole list in its window and is added
//! to everyone else's, and a player who leaves is removed from the others' lists.

mod vanilla;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use super::command;
use super::hook::{self, StartHook};
use super::queue::{self, Identity, Message, Outbound, SessionId, SharedBatches};
use super::save::storage_key;
use super::view::{self, Changes, Encoded};
use super::{Output, Phase, Player, PlayerKey, Settings, State, inside_border};
use super::{dispatch, extra, interact, inventory, survival};
use crate::config::Difficulty;
use crate::crafting;
use crate::custom;
use crate::event::{
    DuplicateOutcome, KickFlag, PlayerDuplicateLogin, PlayerInfo, PlayerJoin, PlayerLogin,
    PlayerViewDistanceChange,
};
use crate::item::SHIELD_RUNTIME_ID;
use crate::math::Vec3;
use crate::player::{self, Message as Text};
use crate::pool::Pool;
use crate::protocol::consts::{
    self, DisconnectReason, GameType, Generator, PermissionLevel, PlayerListAction, SpawnType,
};
use crate::protocol::packet::{
    self, AvailableActorIdentifiers, ChunkRadiusUpdated, CurrentStructureFeature, Disconnect,
    Packet, PlayStatus, PlayerFog, PlayerList, RequestChunkRadius, Respawn, SetActorData,
    SetCommandsEnabled, SetDifficulty, SetHealth, SetLocalPlayerAsInitialised, SetSpawnPosition,
    StartGame, TrimData, VoxelShapes,
};
use crate::protocol::{self, batch};
use crate::protocol::{
    EntityProperties, PLAYER_LIST_ENTRIES_MAX_LEN, PlayerListEntry, PlayerMovementSettings, Rgba,
    ServerJoinInformation, TrimMaterial, TrimPattern, VoxelCells, VoxelShape, VoxelShapeNameEntry,
};
use crate::save::{PlayerData, PlayerLoad, Request, WorldIndex};
use crate::storage::PlayerKey as StoredPlayer;
use crate::world::{World, biome};

/// What a player whose saved data is damaged sees.
const DAMAGED_DATA_MESSAGE: &str = "Your saved player data is damaged. Ask the server operator.";
use super::survival::EYE_HEIGHT;
/// The limited world size BDS sends (P6); the client ignores it for infinite worlds.
const LIMITED_WORLD_SIZE: i32 = 16;
/// BDS rewinds 40 ticks of movement history for server-authoritative movement (P6).
const REWIND_HISTORY_SIZE: i32 = 40;
/// The position BDS's `SetSpawnPosition` carries for a default spawn (P6).
const DEFAULT_SPAWN: [i32; 3] = [i32::MIN; 3];
/// The dimension BDS's `SetSpawnPosition` names at join (P6).
const SPAWN_POSITION_DIMENSION: i32 = 3;

/// The start-game batches every player gets the same, the ones that skip the send hooks
/// ([`ServerBuilder::on_send`](crate::ServerBuilder::on_send) lists them). Each is encoded and
/// compressed once at start; sessions only encrypt them.
#[derive(Debug, Clone)]
pub(crate) struct Shared {
    item_registry: SharedBatches,
    biome_definitions: SharedBatches,
    /// The creative inventories ([`custom::Registry::creatives`]), each player's by its index.
    creative_content: Vec<SharedBatches>,
    crafting_data: SharedBatches,
    voxel_shapes: SharedBatches,
    actor_identifiers: SharedBatches,
    trim_data: SharedBatches,
    jigsaw_structure_data: SharedBatches,
    /// The 13 `SyncActorProperty` packets.
    actor_properties: SharedBatches,
    /// `CameraPresets`, `CameraAimAssistPresets` and `CameraSpline`.
    camera: SharedBatches,
    /// The custom block definitions `StartGame` carries (empty without custom blocks).
    blocks: Arc<[protocol::BlockEntry]>,
}

/// The entity types BDS 1.26.52 sends in `AvailableActorIdentifiers`: `CloudburstMC`'s
/// `entity_identifiers.dat`, byte for byte what BDS sends (P6, 8,907 bytes).
const ENTITY_IDENTIFIERS: &[u8] =
    include_bytes!("../../xtask/data/reference/entity_identifiers.dat");

/// The block collision shapes BDS 1.26.52 sends in `VoxelShapes`: `CloudburstMC`'s
/// `voxel_shapes.json`, whose counts (287 shapes, 220 names), name order and encoded size (23,178
/// bytes) equal BDS's (P6).
const VOXEL_SHAPES: &str = include_str!("../../xtask/data/reference/voxel_shapes.json");

impl Shared {
    /// Builds the batches with the server's batch settings. Fails if a packet cannot be encoded
    /// or its batch is larger than `max-batch-bytes`.
    ///
    /// With the server's custom content: the custom items after the vanilla ones in the item
    /// registry and the creative inventory, the custom recipes after PMMP's, the custom block
    /// definitions for `StartGame` and the custom entity identifiers. Without custom content the
    /// batches are byte for byte the vanilla ones.
    pub(crate) fn new(
        settings: batch::Settings,
        custom: &custom::Registry,
        custom_entities: &super::entity::Customs,
    ) -> Result<Self, batch::Error> {
        let mut encoder = batch::Encoder::new(settings);
        Ok(Self {
            item_registry: encode(
                &mut encoder,
                &[Packet::ItemRegistry(custom.item_registry())],
            )?,
            biome_definitions: encode(
                &mut encoder,
                &[Packet::BiomeDefinitionList(biome::definition_list())],
            )?,
            creative_content: custom
                .creatives()
                .iter()
                .map(|c| {
                    encode(
                        &mut encoder,
                        &[Packet::CreativeContent(c.content().clone())],
                    )
                })
                .collect::<Result<_, _>>()?,
            crafting_data: encode(
                &mut encoder,
                &[Packet::CraftingData(Box::new(custom.crafting_data()))],
            )?,
            voxel_shapes: encode(&mut encoder, &[Packet::VoxelShapes(voxel_shapes())])?,
            actor_identifiers: encode(
                &mut encoder,
                &[Packet::AvailableActorIdentifiers(
                    AvailableActorIdentifiers {
                        serialised_entity_identifiers: custom_entities
                            .identifier_list(ENTITY_IDENTIFIERS)
                            .map_err(batch::Error::Packet)?,
                    },
                )],
            )?,
            trim_data: encode(&mut encoder, &[Packet::TrimData(trim_data())])?,
            jigsaw_structure_data: encode(&mut encoder, &[vanilla::jigsaw_structure_data()])?,
            actor_properties: encode(&mut encoder, &vanilla::actor_properties())?,
            camera: encode(&mut encoder, &vanilla::camera())?,
            blocks: custom.block_entries().into(),
        })
    }
}

impl Shared {
    /// The batches of every creative inventory, in the server's order (tests).
    #[cfg(test)]
    pub(super) fn creative_batches(&self) -> &[SharedBatches] {
        &self.creative_content
    }

    /// The batches of `player`'s creative inventory.
    fn creative(&self, player: &Player) -> Option<Message> {
        self.creative_content
            .get(usize::from(player.inventory.creative))
            .map(|c| Message::Shared(Arc::clone(c)))
    }
}

/// The voxel shapes of [`VOXEL_SHAPES`], names in the file's order (BDS's order).
fn voxel_shapes() -> VoxelShapes {
    #[expect(
        clippy::expect_used,
        reason = "invariant: the committed file parses (the voxel shape test reads it)"
    )]
    let file: ShapesFile = serde_json::from_str(VOXEL_SHAPES).expect("voxel_shapes.json parses");
    VoxelShapes {
        shapes: file
            .shapes
            .into_iter()
            .map(|s| VoxelShape {
                cells: VoxelCells {
                    x_size: s.cells.x_size,
                    y_size: s.cells.y_size,
                    z_size: s.cells.z_size,
                    storage: s.cells.storage,
                },
                x_coordinates: s.x_coordinates,
                y_coordinates: s.y_coordinates,
                z_coordinates: s.z_coordinates,
            })
            .collect(),
        name_map: file
            .names
            .0
            .into_iter()
            .map(|(name, id)| VoxelShapeNameEntry { name, id })
            .collect(),
        custom_shape_count: 0,
    }
}

/// `voxel_shapes.json`.
#[derive(serde::Deserialize)]
struct ShapesFile {
    names: Names,
    shapes: Vec<ShapeJson>,
}

/// The `names` object as pairs in file order.
struct Names(Vec<(String, u16)>);

impl<'de> serde::Deserialize<'de> for Names {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        /// Collects the entries in order.
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Names;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a map of shape names to IDs")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Names, A::Error> {
                let mut out = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    out.push(entry);
                }
                Ok(Names(out))
            }
        }
        d.deserialize_map(Visitor)
    }
}

/// One shape of `voxel_shapes.json`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShapeJson {
    cells: CellsJson,
    x_coordinates: Vec<f32>,
    y_coordinates: Vec<f32>,
    z_coordinates: Vec<f32>,
}

/// The cells of a shape.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CellsJson {
    x_size: u8,
    y_size: u8,
    z_size: u8,
    storage: Vec<u8>,
}

/// The armour trim patterns and materials BDS 1.26.52 sends (P6), from the tables the smithing
/// table uses ([`crafting::TRIM_PATTERNS`], [`crafting::TRIM_MATERIALS`]).
fn trim_data() -> TrimData {
    TrimData {
        patterns: crafting::TRIM_PATTERNS
            .iter()
            .map(|p| TrimPattern {
                item_name: format!("minecraft:{p}_armor_trim_smithing_template"),
                pattern_id: (*p).to_owned(),
            })
            .collect(),
        materials: crafting::TRIM_MATERIALS
            .iter()
            .map(|&(id, colour, item)| TrimMaterial {
                material_id: id.to_owned(),
                colour: colour.to_owned(),
                item_name: item.to_owned(),
            })
            .collect(),
    }
}

/// Encodes and compresses packets, in order, into their batch bodies.
fn encode(encoder: &mut batch::Encoder, packets: &[Packet]) -> Result<SharedBatches, batch::Error> {
    let headed = packets.iter().map(|packet| {
        let header = batch::Header {
            id: packet.id(),
            ..batch::Header::default()
        };
        (header, packet)
    });
    let mut payload = Vec::new();
    let mut bodies = Vec::new();
    batch::encode_packets(headed, SHIELD_RUNTIME_ID, &mut payload, |p| {
        let mut body = Vec::new();
        encoder.encode(p, &mut body)?;
        bodies.push(body);
        Ok(())
    })?;
    Ok(bodies.into())
}

/// What the output phase sends a player at the end of a tick. The input and view phases set it and
/// the output phase clears it, so it is empty between ticks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Pending {
    /// `StartGame` and the item registry.
    start_game: bool,
    /// The answer to the chunk radius requests of the tick.
    pub(super) radius: Option<RadiusAnswer>,
    /// `PlayStatus(PlayerSpawn)`: the spawn chunks were sent.
    pub(super) spawn_status: bool,
    /// The player's creative inventory again: it was switched once the client asked for its
    /// chunk radius (the spawn window has it or is sent this tick).
    pub(super) creative: bool,
}

/// The answer to a chunk radius request.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct RadiusAnswer {
    /// The granted radius.
    radius: u8,
    /// The first answer: the spawn position, the creative inventory and the recipes go first.
    first: bool,
}

/// The output phase: the sub-chunks the answers need, encoded once each with their height maps on
/// the CPU pool; then the packets of every player with something to send, built on the CPU pool
/// from read-only state in session order and passed through the send hooks; then the pending
/// marks are cleared.
pub(super) fn output(
    state: &mut State,
    pool: &Pool,
    changes: &Changes,
) -> Vec<(SessionId, Vec<Message>)> {
    let ops = operators_starting(state);
    let wanted = view::wanted_sub_chunks(state);
    let worlds = &state.worlds;
    let blocks = &state.rules.block_entities;
    let encoded: Encoded = wanted
        .iter()
        .copied()
        .zip(pool.map(&wanted, |&key| view::encode(worlds, blocks, key)))
        .collect();
    let marked: Vec<(SessionId, &Player)> = state
        .players
        .iter()
        .filter(|(_, p)| {
            p.pending != Pending::default()
                || !p.view.out.is_empty()
                || !p.outbox.is_empty()
                || !p.shared_outbox.is_empty()
                || (changes.contains_key(&p.world) && p.view.is_open())
        })
        .map(|(&s, p)| (s, p))
        .collect();
    let window = Window::new(state, &marked);
    let (settings, shared) = (&state.settings, &state.shared);
    let (sends, starts) = (state.hooks.send(), state.hooks.start_game());
    let commands = state.commands.listing();
    let mut packets: Vec<(SessionId, Vec<Message>)> = pool.map(&marked, |&(session, player)| {
        let world = worlds.get(player.world);
        let built = Built {
            settings,
            shared,
            encoded: &encoded,
            changes,
            window: &window,
            starts,
            commands,
            ops: &ops,
        };
        let mut out = messages(&built, world, session, player);
        if !sends.is_empty() {
            // The hooks need the packets to change; only then are the serial phases' copied.
            out.extend(player.outbox.iter().cloned().map(Message::Packet));
            out.extend(
                player
                    .shared_outbox
                    .iter()
                    .cloned()
                    .map(Message::SharedPacket),
            );
            hook::send(sends, settings, session, &mut out);
        }
        (session, out)
    });
    // The serial phases' packets follow the output phase's ([`Player::send_packet`]); the outbox keeps
    // its capacity for the next tick.
    let hooked = !sends.is_empty();
    for (session, out) in &mut packets {
        if let Some(player) = state.players.get_mut(session) {
            if hooked {
                player.outbox.clear();
                player.shared_outbox.clear();
            } else {
                out.extend(player.outbox.drain(..).map(Message::Packet));
                out.extend(player.shared_outbox.drain(..).map(Message::SharedPacket));
            }
        }
    }
    packets.retain(|(_, m)| !m.is_empty());
    for player in state.players.values_mut() {
        player.pending = Pending::default();
        player.view.out.clear();
    }
    packets
}

/// What the spawn windows of a tick share: the player list and each world's clock and sleeping
/// packets. Built only when a player needs a window this tick.
#[derive(Debug, Default)]
struct Window {
    /// The list of every placed player, in session order, built once for every window: it holds
    /// each player's skin (up to 4 MiB). A client takes at most
    /// [`PLAYER_LIST_ENTRIES_MAX_LEN`] entries in one packet, so a longer list goes in several.
    player_list: Vec<Arc<Packet>>,
    /// Per world of a window: the clock state, the clock definition and the sleeping counts. Only
    /// those worlds, as each world's sleeping counts look at every player.
    worlds: BTreeMap<WorldIndex, [Packet; 3]>,
}

impl Window {
    fn new(state: &State, marked: &[(SessionId, &Player)]) -> Self {
        let mut worlds = BTreeMap::new();
        for (_, p) in marked {
            if (p.pending.start_game || p.pending.radius.is_some_and(|r| r.first))
                && !worlds.contains_key(&p.world)
            {
                let w = p.world;
                worlds.insert(
                    w,
                    [
                        survival::clock_state(state, w),
                        survival::clock_definition(state, w),
                        survival::sleeping_players(state, w),
                    ],
                );
            }
        }
        if worlds.is_empty() {
            return Self::default();
        }
        let placed: Vec<&Player> = state
            .players
            .values()
            .filter(|p| p.phase != Phase::Loading)
            .collect();
        Self {
            player_list: placed
                .chunks(PLAYER_LIST_ENTRIES_MAX_LEN)
                .map(|players| {
                    let entries = players.iter().map(|p| list_entry(p)).collect();
                    Arc::new(Packet::PlayerList(PlayerList { entries }))
                })
                .collect(),
            worlds,
        }
    }

    /// The world's packet `which` (0 clock state, 1 clock definition, 2 sleeping counts).
    fn world(&self, world: usize, which: usize) -> Option<Message> {
        let packet = self.worlds.get(&world)?.get(which)?;
        Some(Message::Packet(packet.clone()))
    }

    /// The player list of the window, shared.
    fn player_list(&self) -> impl Iterator<Item = Message> + '_ {
        self.player_list
            .iter()
            .map(|p| Message::SharedPacket(Arc::clone(p)))
    }
}

/// The y BDS sends in its first two spawn `Respawn` packets: the top of the build limit, above
/// the eyes and at the feet (P6).
const RESPAWN_SEARCH_Y: f64 = 32_768.0;

/// What every player's packets of a tick are built from.
struct Built<'a> {
    settings: &'a Settings,
    shared: &'a Shared,
    encoded: &'a Encoded,
    changes: &'a Changes,
    window: &'a Window,
    starts: &'a [StartHook],
    /// What the `AvailableCommands` of a player whose spawn window goes out this tick is built
    /// from: the commands' network form without their handlers, so it is `Sync`.
    commands: command::Listing<'a>,
    /// The operators among the players whose `StartGame` goes out this tick.
    ops: &'a BTreeSet<SessionId>,
}

/// The operators among the players whose `StartGame` goes out this tick: their `StartGame`
/// carries operator permissions, as their abilities do.
fn operators_starting(state: &mut State) -> BTreeSet<SessionId> {
    let starting: Vec<(SessionId, String)> = state
        .players
        .iter()
        .filter(|(_, p)| p.pending.start_game)
        .map(|(&s, p)| (s, p.identity.name.clone()))
        .collect();
    let cx = super::Context { state };
    starting
        .into_iter()
        .filter(|(_, name)| cx.is_op(name))
        .map(|(s, _)| s)
        .collect()
}

/// One player's packets of the tick, in the order BDS sends them (P6): the start-game packets; for
/// the first chunk radius answer the spawn window (spawn position, clocks, difficulty, settings,
/// weather, abilities, rules, player list, biomes, entity types, fog, camera presets, attributes,
/// creative inventory, trims, inventory, recipes, commands, respawn, entity data, structure
/// feature); the granted radius; the chunk packets; the health and the spawn status; then the block
/// changes it sees.
fn messages(
    built: &Built<'_>,
    world: Option<&World>,
    session: SessionId,
    player: &Player,
) -> Vec<Message> {
    let Built {
        settings,
        shared,
        encoded,
        changes,
        window,
        starts,
        commands,
        ops,
    } = *built;
    let mut out = Vec::new();
    let Some(world) = world else {
        return out;
    };
    let index = player.world;
    if player.pending.start_game {
        out.extend(window.world(index, 2));
        out.extend(window.player_list());
        out.extend(window.world(index, 0));
        out.push(Message::Shared(Arc::clone(&shared.jigsaw_structure_data)));
        out.push(Message::Shared(Arc::clone(&shared.voxel_shapes)));
        let op = ops.contains(&session);
        let start = hook::start_game(starts, start_game(settings, shared, world, player, op));
        out.push(Message::Packet(Packet::StartGame(Box::new(start))));
        out.push(Message::Shared(Arc::clone(&shared.actor_properties)));
        out.push(Message::Shared(Arc::clone(&shared.item_registry)));
    }
    if let Some(answer) = player.pending.radius {
        if answer.first {
            let commands = command::available_commands(commands, session, player);
            spawn_window(settings, shared, world, player, window, commands, &mut out);
        }
        out.push(Message::Packet(Packet::ChunkRadiusUpdated(
            ChunkRadiusUpdated {
                chunk_radius: i32::from(answer.radius),
            },
        )));
    }
    // A spawn window sent now has the inventory already.
    if player.pending.creative && player.pending.radius.is_none_or(|a| !a.first) {
        out.extend(shared.creative(player));
    }
    view::packets(world, player, encoded, &mut out);
    if player.pending.spawn_status {
        out.push(Message::Packet(Packet::SetHealth(SetHealth {
            health: survival::health_shown(player),
        })));
        out.push(Message::Packet(Packet::PlayStatus(PlayStatus {
            status: i32::from(consts::PlayStatus::PlayerSpawn),
        })));
    }
    view::block_packets(player, changes, &mut out);
    out
}

/// The packets BDS sends between the chunk radius request and the chunks (P6), with the player's
/// `AvailableCommands`, but for the spawn sound Anchor never sends (deferred.txt). BDS sends the
/// entity data and the structure feature with its first chunks; the chunk packets leave the order
/// of the others as it is.
fn spawn_window(
    settings: &Settings,
    shared: &Shared,
    world: &World,
    player: &Player,
    window: &Window,
    commands: Packet,
    out: &mut Vec<Message>,
) {
    let index = player.world;
    let packet = |p: Packet| Message::Packet(p);
    out.push(packet(Packet::SetSpawnPosition(SetSpawnPosition {
        spawn_type: i32::from(SpawnType::Player),
        position: DEFAULT_SPAWN,
        dimension: SPAWN_POSITION_DIMENSION,
        spawn_position: DEFAULT_SPAWN,
    })));
    out.extend(window.world(index, 1));
    out.push(packet(Packet::SetDifficulty(SetDifficulty {
        difficulty: difficulty_id(super::world_api::difficulty_of(settings, world)).cast_unsigned(),
    })));
    out.push(packet(Packet::SetCommandsEnabled(SetCommandsEnabled {
        enabled: true,
    })));
    out.extend(super::weather::joined(world.weather()).map(packet));
    out.push(packet(survival::adventure_settings()));
    let abilities = survival::update_abilities(settings, player);
    out.push(packet(abilities.clone()));
    out.push(packet(survival::game_rules(settings)));
    out.extend(window.player_list());
    out.push(packet(abilities));
    out.push(Message::Shared(Arc::clone(&shared.biome_definitions)));
    out.push(Message::Shared(Arc::clone(&shared.actor_identifiers)));
    out.push(packet(Packet::PlayerFog(PlayerFog { stack: Vec::new() })));
    out.push(Message::Shared(Arc::clone(&shared.camera)));
    let attributes = survival::all_attributes(player);
    out.push(packet(attributes.clone()));
    out.extend(shared.creative(player));
    out.push(Message::Shared(Arc::clone(&shared.trim_data)));
    let mut inventory = Vec::new();
    inventory::spawn_packets(player, &mut inventory);
    out.extend(inventory.into_iter().map(Message::Packet));
    out.push(Message::Shared(Arc::clone(&shared.crafting_data)));
    out.push(packet(commands));
    let eye = player.position;
    for (y, state) in [
        (RESPAWN_SEARCH_Y + EYE_HEIGHT, 0),
        (RESPAWN_SEARCH_Y, 0),
        (eye.y, 1),
    ] {
        out.push(packet(Packet::Respawn(Respawn {
            position: Vec3::new(eye.x, y, eye.z).to_f32(),
            state,
            entity_runtime_id: 0,
        })));
    }
    out.push(packet(Packet::SetActorData(SetActorData {
        entity_runtime_id: player.runtime_id,
        entity_metadata: survival::full_metadata(settings, player),
        entity_properties: EntityProperties::default(),
        tick: 0,
    })));
    // The structure the player is in: none, as Anchor generates no vanilla structures.
    out.push(packet(Packet::CurrentStructureFeature(
        CurrentStructureFeature {
            current_feature: String::new(),
        },
    )));
    out.push(packet(attributes));
}

/// A player's entry in the player list (BDS values, P6): the skin and device the client sent at
/// login, unchanged.
fn list_entry(player: &Player) -> PlayerListEntry {
    PlayerListEntry {
        action_type: PlayerListAction::Add.into(),
        uuid: player.identity.uuid,
        entity_unique_id: super::entity::unique_id(player.runtime_id),
        username: player.identity.name.clone(),
        xuid: player
            .identity
            .xuid
            .map_or_else(String::new, |x| x.to_string()),
        platform_chat_id: String::new(),
        // BDS sends the login's device (PMMP the unknown device, -1).
        build_platform: player.identity.device_os,
        skin: player.identity.skin.get().clone(),
        teacher: false,
        host: false,
        sub_client: false,
        player_colour: Rgba {
            r: 237,
            g: 237,
            b: 237,
            a: 255,
        },
    }
}

/// Adds a placed player to the list of every other placed player (their own windows, if they are
/// placed this tick too, list everyone), its skin held once for all of them.
fn announce(state: &mut State, player: &Player) {
    let packet = Arc::new(Packet::PlayerList(PlayerList {
        entries: vec![list_entry(player)],
    }));
    for p in state.players.values_mut() {
        if p.phase != Phase::Loading && !p.pending.start_game {
            p.send_shared(Arc::clone(&packet));
        }
    }
}

/// A player left (`State::remove`): it wakes up and leaves the other players' lists, if they
/// were told of it.
pub(super) fn left(state: &mut State, player: &mut Player) {
    survival::left(state, player);
    if player.phase == Phase::Loading {
        return;
    }
    let packet = Packet::PlayerList(PlayerList {
        entries: vec![PlayerListEntry {
            action_type: PlayerListAction::Remove.into(),
            uuid: player.identity.uuid,
            ..PlayerListEntry::default()
        }],
    });
    let told = state
        .players
        .values_mut()
        .filter(|p| p.phase != Phase::Loading && !p.pending.start_game);
    if player.pending.start_game {
        // Placed this tick: `announce` added it through the shared queue, which goes out after
        // the packets, so the removal follows it there.
        let packet = Arc::new(packet);
        for p in told {
            p.send_shared(Arc::clone(&packet));
        }
    } else {
        for p in told {
            p.send_packet(packet.clone());
        }
    }
}

/// Registers the player of `session`, or rejects it when the server is full. A second join of
/// the same session changes nothing. Returns the load of the player's saved data, if the tick
/// does not hold it; the player waits in [`Phase::Loading`] until it arrives.
///
/// When the same player is already online, [`PlayerDuplicateLogin`] decides which of the two is
/// disconnected. That check comes before the full check: replacing the existing player frees its
/// slot, so a full server still lets a player take over its own place.
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
        let (back, outcome, event) = duplicate_login(state, existing, identity);
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
            if let Some(e) = &event {
                dispatch::watch(state, e);
            }
            return None;
        }
        super::player::quit(
            state,
            existing,
            PlayerDuplicateLogin::DEFAULT_REASON,
            super::player::LEFT.to_owned(),
        );
        if let Some(old) = state.remove(existing) {
            info!(
                session = existing.get(),
                by = session.get(),
                name = %old.identity.name,
                "player replaced by a new login"
            );
            output.close(old.outbound, packet);
        }
        if let Some(e) = &event {
            dispatch::watch(state, e);
        }
    }
    let (refused, event) = login(state, &identity);
    let request = match refused {
        Some(Refused { reason, packet }) => {
            info!(session = session.get(), name = %identity.name, %reason, "join rejected");
            output.close(outbound, packet);
            None
        }
        None => enter(state, session, identity, key, outbound, output),
    };
    if let Some(e) = &event {
        dispatch::watch(state, e);
    }
    request
}

/// Registers the admitted player of `session` (see [`join`]); one placed dead respawns
/// ([`survival::respawn_joined`]).
fn enter(
    state: &mut State,
    session: SessionId,
    identity: Identity,
    key: PlayerKey,
    outbound: mpsc::Sender<Outbound>,
    output: &mut Output,
) -> Option<Request> {
    let Some(next) = state.next_runtime_id.checked_add(1) else {
        // 2^64 joins never happen in one process; refuse rather than reuse an ID.
        warn!(session = session.get(), "entity runtime IDs exhausted");
        output.close(outbound, queue::server_full());
        return None;
    };
    let runtime_id = state.next_runtime_id;
    state.next_runtime_id = next;
    info!(session = session.get(), name = %identity.name, runtime_id, "player joined");
    let stored = storage_key(&key);
    let mut player = Player {
        session,
        runtime_id,
        identity,
        key,
        world: 0,
        position: Vec3::ZERO,
        data_dirty: false,
        chunk_radius: None,
        asked_radius: None,
        phase: Phase::Loading,
        view: view::View::default(),
        outbound,
        pending: Pending::default(),
        extra: extra::PlayerExtra::default(),
        inventory: inventory::Inventory::default(),
        survival: survival::Survival::new(&state.settings),
        breaking: interact::Breaking::default(),
        outbox: Vec::new(),
        shared_outbox: Vec::new(),
        overflowed: false,
        marks: super::player::Marks::default(),
        presence: super::player::Presence::default(),
    };
    let op = super::Context { state: &mut *state }.is_op(&player.identity.name);
    player.survival.set_operator(op);
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
    survival::respawn_joined(state, session);
    request
}

/// Puts a player into its saved world and position, or the default world's spawn if it has none
/// (a new player, a world not loaded, a position outside the world border), hands its
/// saved extra to the rule modules ([`extra::player_loaded`]), marks the start-game packets and
/// adds it to the other players' lists.
fn place(state: &mut State, player: &mut Player, data: Option<PlayerData>) {
    let saved = data.and_then(|d| {
        player.extra = extra::player_loaded(d.extra);
        inventory::load(&mut player.inventory, player.extra.raw(), &state.custom);
        inventory::restore(&mut player.inventory);
        survival::load(&mut player.survival, player.extra.raw(), &state.custom);
        let world = state.catalog.named(&d.world)?;
        state.catalog.is_loaded(world).then_some(())?;
        Some((world, tick_position(d.position)?))
    });
    super::task::placed(&state.data, &mut player.extra);
    let history = survival::load_played(player.extra.raw());
    let join_ms = player.identity.client.join_time_ms;
    player.presence.place(history, join_ms, state.tick);
    let default = state.catalog.default_world();
    let (world, position) =
        saved.unwrap_or_else(|| (default, spawn_position(state.worlds.get(default))));
    player.world = world;
    player.position = position;
    player.phase = Phase::Joined;
    announce(state, player);
    player.pending.start_game = true;
}

/// A saved position as the tick holds it, if it is inside the world border (the file is external
/// input: a finite position may still be anywhere).
fn tick_position(position: [f64; 3]) -> Option<Vec3> {
    let [x, y, z] = position;
    let position = Vec3::new(x, y, z);
    inside_border(position).then_some(position)
}

/// The eye position of a player standing on a world's spawn block.
fn spawn_position(world: Option<&World>) -> Vec3 {
    let spawn = world.map(World::spawn).unwrap_or_default();
    Vec3::new(
        f64::from(spawn.x) + 0.5,
        f64::from(spawn.y) + EYE_HEIGHT,
        f64::from(spawn.z) + 0.5,
    )
}

/// The saved data of a loading player arrived: it joins where it was, at the spawn if it is
/// new, or at its respawn point if it was dead ([`survival::respawn_joined`]). Damaged data
/// refuses the join and leaves the saved bytes as they are.
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
                        reason: i32::from(DisconnectReason::Unknown),
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
    survival::respawn_joined(state, session);
}

/// Raises [`PlayerDuplicateLogin`] for `connecting` against the player of `existing` and returns
/// `connecting` with the outcome. Without listeners the event is not built: the outcome is the
/// default one and nothing is cloned.
fn duplicate_login(
    state: &mut State,
    existing: SessionId,
    connecting: Identity,
) -> (Identity, DuplicateOutcome, Option<PlayerDuplicateLogin>) {
    if !dispatch::heard::<PlayerDuplicateLogin>(state) {
        return (connecting, DuplicateOutcome::unheard(), None);
    }
    let Some(old) = state.players.get(&existing) else {
        // The index names only joined players (State::insert and State::remove keep it so).
        warn!(
            session = existing.get(),
            "player index names a missing player"
        );
        return (connecting, DuplicateOutcome::unheard(), None);
    };
    // One clone of the existing identity per duplicate login, only when someone listens; the
    // monitors get a copy of the event after the outcome is applied.
    let mut event = PlayerDuplicateLogin::new(connecting, old.identity.clone());
    dispatch::call(state, &mut event);
    let watched = event.clone();
    let (identity, outcome) = event.into_outcome();
    (identity, outcome, Some(watched))
}

/// How a refused join ends: the reason for the log and the session's last packet.
struct Refused {
    reason: String,
    packet: Packet,
}

/// The kick flags of [`PlayerLogin`] that Anchor sets, in the order of [`command::refusals`].
const REFUSAL_FLAGS: [KickFlag; 3] = [
    KickFlag::ServerFull,
    KickFlag::ServerWhitelisted,
    KickFlag::Banned,
];

/// Whether `identity` may join (PMMP `PlayerPreLoginEvent`, which Anchor raises here, in the
/// tick, after the duplicate login check): Anchor's refusals ([`command::refusals`]) are the
/// event's kick flags, listeners may clear them or set others, and the first flag left refuses
/// the player with its screen message. Without listeners the first refusal decides
/// ([`command::admit`]) and no event is built. Returns the refusal, if any, and the event for
/// the monitors, who see it after the join is applied.
fn login(state: &mut State, identity: &Identity) -> (Option<Refused>, Option<PlayerLogin>) {
    if !dispatch::heard::<PlayerLogin>(state) {
        let refused = command::admit(state, identity).err().map(|r| Refused {
            reason: r.message(),
            packet: r.packet(),
        });
        return (refused, None);
    }
    let mut anchor = command::refusals(state, identity);
    let ip = identity
        .address
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let mut event = PlayerLogin {
        info: PlayerInfo(identity.clone()),
        address: SocketAddr::new(ip, identity.client.port),
        kicks: Default::default(),
    };
    for (flag, refusal) in REFUSAL_FLAGS.into_iter().zip(&anchor) {
        if let Some(r) = refusal {
            event.set_kick_flag(flag, r.message(), None);
        }
    }
    dispatch::call(state, &mut event);
    if dispatch::listened::<PlayerLogin>(state) {
        // A listener may have changed the game (banned the player, whitelisted the server): a
        // refusal that was not there before sets its flag; one a listener cleared stays clear.
        let now = command::refusals(state, identity);
        for ((flag, was), is) in REFUSAL_FLAGS.into_iter().zip(&mut anchor).zip(now) {
            if was.is_none()
                && let Some(r) = is
            {
                event.set_kick_flag(flag, r.message(), None);
                *was = Some(r);
            }
        }
    }
    let refused = event.kick_flags().next().map(|flag| {
        let reason = event.disconnect_reason(flag).unwrap_or_default().to_owned();
        let screen = event.disconnect_screen_message(flag).unwrap_or_default();
        // Anchor's own refusal, unchanged, ends as without listeners (a full server's
        // `PlayStatus` lets the client show its own text).
        let own = REFUSAL_FLAGS
            .iter()
            .position(|&f| f == flag)
            .and_then(|i| anchor.get(i))
            .and_then(Option::as_ref)
            .filter(|r| r.message() == reason && r.message() == screen);
        let packet = own.map_or_else(
            || {
                Packet::Disconnect(Disconnect {
                    reason: i32::from(DisconnectReason::NotAllowed),
                    hide_disconnection_screen: false,
                    message: screen.to_owned(),
                    filtered_message: String::new(),
                })
            },
            command::Refusal::packet,
        );
        Refused { reason, packet }
    });
    (refused, Some(event))
}

/// The packet that disconnects the losing side of a duplicate login.
fn logged_in_other_location(message: String) -> Packet {
    Packet::Disconnect(Disconnect {
        reason: i32::from(DisconnectReason::LoggedInOtherLocation),
        hide_disconnection_screen: false,
        message,
        filtered_message: String::new(),
    })
}

/// Grants a chunk radius request: the view opens at once or resizes in the view phase, and the
/// output phase sends the granted radius (the first time with the spawn position, the creative
/// inventory and the recipes). Requests in one tick share one answer and one resize with the last
/// radius.
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
    let Some(mut player) = state.players.remove(&session) else {
        return;
    };
    let view_distance = state.settings.view(player.world).view_distance;
    let asked = u8::try_from(request.chunk_radius).unwrap_or(u8::MAX);
    let radius = asked.min(view_distance);
    let mut changed = None;
    if player.phase == Phase::Loading {
        debug!(
            session = session.get(),
            "chunk radius before the player data ignored"
        );
    } else {
        player.asked_radius = Some(asked);
        // An open view follows the granted radius in the view phase ([`view::update`]), once a
        // tick: each request resizing it would cost the whole disc, and nothing bounds how many
        // requests a client sends.
        let old = if player.view.is_open() {
            answer_radius(&mut player, radius)
        } else {
            grant_radius(state, &mut player, radius)
        };
        if old != Some(radius) {
            changed = Some(old);
        }
    }
    state.players.insert(session, player);
    if let Some(old_distance) = changed
        && dispatch::heard::<PlayerViewDistanceChange>(state)
    {
        let mut event = PlayerViewDistanceChange {
            player: player::Id::new(session),
            old_distance,
            new_distance: radius,
        };
        dispatch::call(state, &mut event);
        dispatch::watch(state, &event);
    }
}

/// Grants `radius` to `player`, taken out of the player table and past loading: the view opens
/// or resizes, and the output phase sends the radius (the first time with the spawn window).
/// Returns the radius it had.
pub(super) fn grant_radius(state: &mut State, player: &mut Player, radius: u8) -> Option<u8> {
    let old = answer_radius(player, radius);
    if player.view.radius() != radius {
        view::set_radius(state, player, radius);
    }
    old
}

/// [`grant_radius`] but for the view, which follows in the view phase. Returns the radius it had.
fn answer_radius(player: &mut Player, radius: u8) -> Option<u8> {
    let earlier = player.pending.radius.unwrap_or_default();
    player.pending.radius = Some(RadiusAnswer {
        radius,
        first: earlier.first || player.chunk_radius.is_none(),
    });
    let old = player.chunk_radius;
    player.chunk_radius = Some(radius);
    old
}

impl super::Context<'_> {
    /// Sets the chunk radius `player` sees (PMMP `Player::setViewDistance`), kept from 1 to
    /// the `view-distance` of its world: its view grows or shrinks at once, and its client is
    /// told the radius at the end of the tick. A world the player goes to later grants the
    /// radius set here up to its own `view-distance`. The client asks again when its own
    /// setting changes, and that request wins. A direct call: it raises no
    /// [`PlayerViewDistanceChange`].
    ///
    /// Returns the radius set; `None`, changing nothing, if the player is not in the game or its
    /// client has not asked for a radius yet (the first answer opens the spawn window).
    #[doc(alias = "Player::setViewDistance")]
    pub fn set_view_distance(&mut self, player: player::Id, distance: u8) -> Option<u8> {
        let session = player.session();
        let mut p = self.state.players.remove(&session)?;
        let view_distance = self.state.settings.view(p.world).view_distance;
        let radius = distance.min(view_distance).max(1);
        let ready = p.phase != Phase::Loading && p.chunk_radius.is_some();
        if ready {
            p.asked_radius = Some(distance.max(1));
            grant_radius(self.state, &mut p, radius);
        }
        self.state.players.insert(session, p);
        ready.then_some(radius)
    }
}

/// Marks the player spawned when the client confirms with its own runtime ID.
pub(super) fn initialised(
    state: &mut State,
    session: SessionId,
    packet: &SetLocalPlayerAsInitialised,
) {
    let settings = &state.settings;
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
        Phase::Spawning => {
            player.phase = Phase::Spawned;
            survival::spawned(settings, player);
        }
        Phase::Spawned => return,
        Phase::Loading | Phase::Joined => {
            warn!(
                session = session.get(),
                "initialised before spawning; ignored"
            );
            return;
        }
    }
    joined(state, session);
}

/// The join message, BDS's yellow "joined the game" line: BDS sends it as a translated `Text`
/// with the name (P6 duplicate capture). Listeners see this key; a message left at it is sent so,
/// any other as it is (like [`LEFT`](super::player::LEFT)).
const JOINED_KEY: &str = "\u{a7}e%multiplayer.player.joined";

/// The player of `session` spawned (PMMP `Player::doFirstSpawn`): [`PlayerJoin`] may change the
/// join message, which every other player in the game then sees (BDS sends it to the others
/// only; PMMP to everyone). An empty message shows nothing; a player a listener kicked shows
/// none.
fn joined(state: &mut State, session: SessionId) {
    let Some(name) = state
        .players
        .get(&session)
        .map(|p| p.display_name().to_owned())
    else {
        return;
    };
    let translated = |name: String| Text::Translation {
        key: JOINED_KEY.to_owned(),
        parameters: vec![name],
    };
    let mut event = None;
    let message = if dispatch::heard::<PlayerJoin>(state) {
        let e = event.insert(PlayerJoin {
            player: player::Id::new(session),
            join_message: JOINED_KEY.to_owned(),
        });
        dispatch::call(state, e);
        let still = state.players.get(&session).is_some_and(Player::is_online);
        if !still || e.join_message.is_empty() {
            None
        } else if e.join_message == JOINED_KEY {
            Some(translated(name))
        } else {
            Some(Text::Raw(e.join_message.clone()))
        }
    } else {
        Some(translated(name))
    };
    if let Some(message) = message {
        tell_others(state, session, message);
    }
    if let Some(e) = &event {
        dispatch::watch(state, e);
    }
}

/// Sends `message` to every player in the game but the one of `session` (join and quit
/// messages). A message a client does not decode is not sent.
pub(super) fn tell_others(state: &mut State, session: SessionId, message: Text) {
    let packets = match message.into_packets() {
        Ok(packets) => packets,
        Err(e) => {
            debug!(session = session.get(), error = %e, "message to the others not sent");
            return;
        }
    };
    for (&s, p) in &mut state.players {
        if s != session && p.phase != Phase::Loading && p.is_online() {
            for packet in packets.iter().flatten() {
                p.send_packet(packet.clone());
            }
        }
    }
}

/// The protocol ID of a difficulty ([`Difficulty::id`]) as the packets carry it.
pub(super) fn difficulty_id(difficulty: Difficulty) -> i32 {
    i32::from(difficulty.id())
}

fn start_game(
    settings: &Settings,
    shared: &Shared,
    world: &World,
    player: &Player,
    op: bool,
) -> StartGame {
    let game_mode = survival::game_mode_id(settings.game_mode);
    let difficulty = difficulty_id(super::world_api::difficulty_of(settings, world));
    // BDS sends the default type (the world's mode) for a player without its own mode (P6).
    let player_game_mode = survival::own_game_mode(settings, player)
        .map_or(i32::from(GameType::Default), survival::game_mode_id);
    let spawn = world.spawn();
    let (rain_level, lightning_level) = super::weather::levels(world.weather());
    StartGame {
        entity_unique_id: super::entity::unique_id(player.runtime_id),
        entity_runtime_id: player.runtime_id,
        player_game_mode,
        player_position: player.position.to_f32(),
        world_seed: world.seed(),
        // What BDS sends for a flat world (P6).
        generator: i32::from(Generator::Flat),
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
        player_permissions: u8::from(if op {
            PermissionLevel::Operator
        } else {
            PermissionLevel::Member
        }),
        server_chunk_tick_radius: i32::from(settings.view(player.world).tick_radius),
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
        // The custom block definitions; none for a vanilla server, as BDS's data-driven blocks are
        // not PMMP blocks.
        blocks: shared.blocks.to_vec(),
        // PMMP `PreSpawnPacketHandler`: the world's time, so the day cycle starts where it is.
        time: world.time_ticks(),
        day_cycle_lock_time: survival::day_cycle_lock_time(settings, world.time_ticks()),
        rain_level,
        lightning_level,
        // Education features stay off, as PMMP leaves them: the Education items are never
        // offered.
        ..StartGame::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `CloudburstMC` data copy, when the external sources are there (`.cache`).
    fn cache(name: &str) -> Option<Vec<u8>> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".cache/cloudburst-data")
            .join(name);
        #[expect(
            clippy::disallowed_methods,
            reason = "off-tick: a test reads the external data copy"
        )]
        let bytes = std::fs::read(path).ok();
        bytes
    }

    fn payload(packet: &Packet) -> Vec<u8> {
        let mut bytes = Vec::new();
        packet.encode(&mut bytes, 0).unwrap();
        bytes
    }

    #[test]
    fn the_entity_types_are_the_cloudburst_bytes_bds_sends() {
        assert_eq!(ENTITY_IDENTIFIERS.len(), 8907, "BDS sends 8,907 bytes (P6)");
        if let Some(copy) = cache("entity_identifiers.dat") {
            assert_eq!(ENTITY_IDENTIFIERS, copy.as_slice());
        }
        let packet = Packet::AvailableActorIdentifiers(AvailableActorIdentifiers {
            serialised_entity_identifiers: ENTITY_IDENTIFIERS.to_vec(),
        });
        assert_eq!(payload(&packet), ENTITY_IDENTIFIERS);
    }

    #[test]
    fn voxel_shapes_match_bds_counts_order_and_size() {
        if let Some(copy) = cache("voxel_shapes.json") {
            assert_eq!(VOXEL_SHAPES.as_bytes(), copy.as_slice());
        }
        let shapes = voxel_shapes();
        assert_eq!((shapes.shapes.len(), shapes.name_map.len()), (287, 220));
        // BDS's first names, in its order (P6 capture).
        let first: Vec<(&str, u16)> = shapes
            .name_map
            .iter()
            .take(3)
            .map(|e| (e.name.as_str(), e.id))
            .collect();
        assert_eq!(
            first,
            [
                (
                    "minecraft:wall_east_low_north_low_south_low_up_west_tall",
                    60
                ),
                ("minecraft:wall_east_low_north_low_south_low_up", 58),
                ("minecraft:wall_south_low_up_west_tall", 206),
            ]
        );
        let packet = Packet::VoxelShapes(shapes);
        let bytes = payload(&packet);
        assert_eq!(bytes.len(), 23_178, "BDS's VoxelShapes payload (P6)");
        let mut budget = usize::MAX;
        assert_eq!(
            Packet::decode(packet.id(), &bytes, &mut budget, 0).unwrap(),
            packet
        );
    }

    #[test]
    fn trim_data_has_the_bds_patterns_and_materials() {
        let trims = trim_data();
        assert_eq!((trims.patterns.len(), trims.materials.len()), (18, 11));
        assert_eq!(
            trims.patterns[0].item_name,
            "minecraft:ward_armor_trim_smithing_template"
        );
        assert_eq!(trims.materials[10].colour, "\u{a7}v");
        // BDS: 1,271 bytes with the two-byte header of packet 302 (P6).
        assert_eq!(payload(&Packet::TrimData(trims)).len(), 1269);
    }

    /// The trim tables are the BDS `TrimData` the P6 run recorded, in its order.
    #[test]
    fn trim_data_equals_the_bds_record() {
        let facts = include_str!("../../xtask/data/p6/facts.md");
        let line = facts
            .lines()
            .find(|l| l.starts_with("- TrimData:"))
            .expect("the TrimData fact");
        let list = |after: &str| {
            let rest = line.split_once(after).expect(after).1;
            rest.split_once(';')
                .expect("list end")
                .0
                .split(", ")
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let trims = trim_data();
        let patterns: Vec<String> = trims
            .patterns
            .iter()
            .map(|p| p.pattern_id.clone())
            .collect();
        assert_eq!(patterns, list("무늬 18개("));
        for p in &trims.patterns {
            assert_eq!(
                p.item_name,
                format!("minecraft:{}_armor_trim_smithing_template", p.pattern_id)
            );
        }
        let materials: Vec<String> = trims
            .materials
            .iter()
            .map(|m| format!("{} {}", m.material_id, m.colour))
            .collect();
        assert_eq!(materials, list("재료 11개("));
    }

    #[test]
    fn the_plain_skin_is_a_valid_classic_skin() {
        let skin = crate::tick::queue::Skin::default();
        let skin = skin.get();
        let pixels = usize::try_from(skin.skin_image_width * skin.skin_image_height).unwrap();
        assert_eq!(skin.skin_data.len(), pixels * 4);
        assert_eq!(crate::protocol::skin_validate(skin), None);
    }

    /// The Education items are never offered, so the client gets no Education features.
    #[test]
    fn start_game_leaves_education_features_off() {
        use super::super::tests::{join, new_state, received, settings, tick};
        use crate::event::Listeners;

        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j], &mut output);
        let sent = received(&mut rx).0.into_iter().find_map(|p| match p {
            Packet::StartGame(s) => Some(s.education_features_enabled),
            _ => None,
        });
        assert_eq!(sent, Some(false));
    }

    #[test]
    fn a_player_list_longer_than_a_client_takes_goes_in_several_packets() {
        use super::super::tests::{join, new_state, radius, received, settings, tick};
        use crate::event::Listeners;

        let count = PLAYER_LIST_ENTRIES_MAX_LEN + 1;
        let players = u32::try_from(count).unwrap();
        let mut state = new_state(
            settings(|c| c.general.max_players = players),
            Listeners::default(),
        );
        let mut output = Output::default();
        let (mut joins, mut queues): (Vec<_>, Vec<_>) = (1..=u64::try_from(count).unwrap())
            .map(|s| join(s, 0))
            .unzip();
        tick(&mut state, &mut joins, &mut output);
        tick(&mut state, &mut vec![radius(1, 1, 1)], &mut output);
        let lists: Vec<usize> = received(&mut queues[0])
            .0
            .into_iter()
            .filter_map(|p| match p {
                Packet::PlayerList(l) => Some(l.entries.len()),
                _ => None,
            })
            .collect();
        // The start-game list and the spawn window's, each of every placed player.
        assert_eq!(
            lists,
            [
                PLAYER_LIST_ENTRIES_MAX_LEN,
                1,
                PLAYER_LIST_ENTRIES_MAX_LEN,
                1
            ]
        );
    }

    #[test]
    fn a_player_placed_and_gone_in_one_tick_leaves_the_other_lists() {
        use super::super::tests::{join, leave, new_state, received, settings, tick};
        use crate::event::Listeners;

        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        let (first, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![first], &mut output);
        received(&mut rx);
        let (second, _rx2) = join(2, 0);
        tick(&mut state, &mut vec![second, leave(2, 1)], &mut output);
        let actions: Vec<u8> = received(&mut rx)
            .0
            .into_iter()
            .filter_map(|p| match p {
                Packet::PlayerList(l) => Some(l.entries),
                _ => None,
            })
            .flatten()
            .map(|e| e.action_type)
            .collect();
        assert_eq!(actions, [0, 1], "added, then removed");
    }

    /// The login, join, duplicate login and view distance events.
    mod events {
        use std::cell::Cell;
        use std::net::{IpAddr, Ipv4Addr};

        use tokio::sync::mpsc;

        use super::super::super::tests::{
            initialised, input, join, new_state, radius, received, settings, tick,
        };
        use super::super::super::{Context, InputKind, Output, Phase, SessionId, State};
        use super::super::{Identity, JOINED_KEY, Outbound};
        use crate::event::{
            Event, KickFlag, Listeners, PlayerDuplicateLogin, PlayerJoin, PlayerLogin,
            PlayerViewDistanceChange, Priority,
        };
        use crate::permission::BanEntry;
        use crate::player::Id;
        use crate::protocol::consts::TextType;
        use crate::protocol::packet::Packet;

        thread_local! {
            /// What the listeners of a test saw, in order.
            static LOG: Cell<Vec<String>> = const { Cell::new(Vec::new()) };
        }

        fn log(line: String) {
            let mut lines = LOG.take();
            lines.push(line);
            LOG.set(lines);
        }

        fn logged() -> Vec<String> {
            LOG.take()
        }

        /// Listeners with one listener of `E` at normal priority.
        fn on<E: Event>(
            listener: impl FnMut(&mut E, &mut Context<'_>) + Send + 'static,
        ) -> Listeners {
            let mut l = Listeners::default();
            l.push(Priority::Normal, listener);
            l
        }

        fn state(edit: impl FnOnce(&mut crate::config::Config), listeners: Listeners) -> State {
            LOG.set(Vec::new());
            let listeners = listeners;
            new_state(settings(edit), listeners)
        }

        fn run(state: &mut State, inputs: Vec<crate::tick::queue::Input>) {
            tick(state, &mut { inputs }, &mut Output::default());
        }

        /// Joins session `s`, opens a view of radius 2 and confirms the spawn; its queue.
        fn spawn(state: &mut State, s: u64) -> mpsc::Receiver<Outbound> {
            let (j, rx) = join(s, 0);
            let mut output = Output::default();
            tick(state, &mut vec![j], &mut output);
            tick(state, &mut vec![radius(s, 1, 2)], &mut output);
            let runtime_id = state.players[&SessionId::new(s)].runtime_id;
            tick(state, &mut vec![initialised(s, 2, runtime_id)], &mut output);
            rx
        }

        fn identity(name: &str, address: Option<IpAddr>) -> Identity {
            Identity {
                name: name.to_owned(),
                xuid: None,
                uuid: crate::protocol::Uuid::from_bytes([9; 16]),
                skin: crate::tick::queue::Skin::default(),
                device_os: -1,
                address,
                client: crate::tick::queue::Client::default(),
            }
        }

        fn join_as(
            s: u64,
            identity: Identity,
        ) -> (crate::tick::queue::Input, mpsc::Receiver<Outbound>) {
            let (outbound, rx) = crate::tick::queue::outbound_queue();
            (input(s, 0, InputKind::Join { identity, outbound }), rx)
        }

        /// The texts in what `rx` was sent: (type, message, parameters).
        fn texts(rx: &mut mpsc::Receiver<Outbound>) -> Vec<(u8, String, Vec<String>)> {
            received(rx)
                .0
                .into_iter()
                .filter_map(|p| match p {
                    Packet::Text(t) => Some((t.text_type, t.message, t.parameters)),
                    _ => None,
                })
                .collect()
        }

        fn flags(e: &PlayerLogin) -> String {
            format!("{:?}", e.kick_flags().collect::<Vec<_>>())
        }

        // pmmp: PlayerLoginEvent, PlayerPreLoginEvent
        #[test]
        fn login_listeners_get_every_refusal_and_decide_the_join() {
            let listener = |e: &mut PlayerLogin, _: &mut Context<'_>| {
                log(format!(
                    "{} {} {} {}",
                    e.info().name(),
                    e.ip(),
                    e.port(),
                    flags(e)
                ));
                e.clear_kick_flag(KickFlag::ServerFull);
            };
            let mut state = state(|c| c.general.max_players = 1, on(listener));
            let _first = spawn(&mut state, 1);
            Context { state: &mut state }
                .ban(BanEntry::new("Banned"))
                .unwrap();
            let address = Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7)));
            let (j, mut rx) = join_as(2, identity("Banned", address));
            run(&mut state, vec![j]);
            // Clearing the full flag does not let in a player the ban refuses.
            assert_eq!(
                logged(),
                [
                    "player1 0.0.0.0 0 []",
                    "Banned 192.0.2.7 0 [ServerFull, Banned]"
                ]
            );
            let Some(Packet::Disconnect(d)) = received(&mut rx).1 else {
                panic!("a refused join ends with a Disconnect");
            };
            assert_eq!(d.message, "This server has banned you");
            assert_eq!(state.players.len(), 1);
            Context { state: &mut state }.pardon("banned");
            let (j, mut rx) = join_as(3, identity("Banned", address));
            run(&mut state, vec![j]);
            assert_eq!(logged(), ["Banned 192.0.2.7 0 [ServerFull]"]);
            assert!(
                received(&mut rx).1.is_none(),
                "a listener lets a full server take one more"
            );
            assert_eq!(state.players.len(), 2);
        }

        // pmmp: PlayerPreLoginEvent::getIp, PlayerPreLoginEvent::getPort
        #[test]
        fn the_login_event_has_the_address_and_port_the_client_connected_from() {
            let listener = |e: &mut PlayerLogin, _: &mut Context<'_>| {
                log(format!("{} {}", e.ip(), e.port()));
            };
            let mut state = state(|_| {}, on(listener));
            let mut who = identity("Ported", Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7))));
            who.client.port = 19_133;
            let (j, _rx) = join_as(1, who);
            run(&mut state, vec![j]);
            assert_eq!(logged(), ["192.0.2.7 19133"]);
        }

        // pmmp: PlayerPreLoginEvent
        #[test]
        fn anchors_own_refusal_left_set_ends_as_without_listeners() {
            for listeners in [
                Listeners::default(),
                on(|_: &mut PlayerLogin, _: &mut Context<'_>| {}),
            ] {
                let mut state = state(|c| c.general.max_players = 1, listeners);
                let _first = spawn(&mut state, 1);
                let (j, mut rx) = join(2, 0);
                run(&mut state, vec![j]);
                let Some(Packet::PlayStatus(s)) = received(&mut rx).1 else {
                    panic!("a full server answers with PlayStatus");
                };
                assert_eq!(
                    s.status,
                    i32::from(crate::protocol::consts::PlayStatus::LoginFailedServerFull)
                );
            }
        }

        // pmmp: PlayerPreLoginEvent
        #[test]
        fn a_plugin_kick_flag_refuses_the_join_and_changes_nothing() {
            let refuse = |e: &mut PlayerLogin, _: &mut Context<'_>| {
                e.set_kick_flag(
                    KickFlag::Plugin,
                    "closed",
                    Some("Come back later".to_owned()),
                );
            };
            let mut refused = state(|_| {}, on(refuse));
            let mut idle = state(|_| {}, on(refuse));
            let (j, mut rx) = join(1, 0);
            run(&mut refused, vec![j]);
            run(&mut idle, Vec::new());
            assert_eq!(refused.hash(), idle.hash());
            let Some(Packet::Disconnect(d)) = received(&mut rx).1 else {
                panic!("a refused join ends with a Disconnect");
            };
            assert_eq!(d.message, "Come back later");
            assert!(refused.players.is_empty());
        }

        // pmmp: PlayerPreLoginEvent
        #[test]
        fn a_login_listener_that_bans_the_player_refuses_it() {
            let ban = |e: &mut PlayerLogin, cx: &mut Context<'_>| {
                cx.ban(BanEntry::new(e.info().name()).with_reason("spam"))
                    .unwrap();
            };
            let mut state = state(|_| {}, on(ban));
            let (j, mut rx) = join(1, 0);
            run(&mut state, vec![j]);
            let Some(Packet::Disconnect(d)) = received(&mut rx).1 else {
                panic!("a refused join ends with a Disconnect");
            };
            assert_eq!(d.message, "This server has banned you: spam");
            assert!(state.players.is_empty());
        }

        // pmmp: PlayerPreLoginEvent
        #[test]
        fn login_monitors_see_the_joined_player() {
            let mut listeners = Listeners::default();
            listeners.push_monitor(|e: &PlayerLogin, cx: &Context<'_>| {
                log(format!(
                    "{} {} {}",
                    e.info().name(),
                    e.is_allowed(),
                    cx.state.players.len()
                ));
            });
            let mut state = state(|_| {}, listeners);
            let (j, _rx) = join(1, 0);
            run(&mut state, vec![j]);
            assert_eq!(logged(), ["player1 true 1"]);
        }

        // pmmp: PlayerJoinEvent
        #[test]
        fn the_join_message_goes_to_the_others_as_bds_sends_it() {
            let noop = on(|_: &mut PlayerJoin, _: &mut Context<'_>| {});
            for listeners in [Listeners::default(), noop] {
                let mut state = state(|_| {}, listeners);
                let mut first = spawn(&mut state, 1);
                let first_own = texts(&mut first);
                let mut second = spawn(&mut state, 2);
                assert!(first_own.is_empty(), "a player is not told of its own join");
                assert!(texts(&mut second).is_empty());
                assert_eq!(
                    texts(&mut first),
                    [(
                        u8::from(TextType::Translation),
                        JOINED_KEY.to_owned(),
                        vec!["player2".to_owned()]
                    )]
                );
            }
        }

        // pmmp: PlayerJoinEvent
        #[test]
        fn join_listeners_change_or_clear_the_message() {
            for (set, expected) in [
                (
                    Some("hello"),
                    vec![(u8::from(TextType::Raw), "hello".to_owned(), Vec::new())],
                ),
                (Some(""), Vec::new()),
                // A player kicked by a listener gets no join line, only the usual left line.
                (
                    None,
                    vec![(
                        u8::from(TextType::Translation),
                        "\u{a7}e%multiplayer.player.left".to_owned(),
                        vec!["player2".to_owned()],
                    )],
                ),
            ] {
                let listener = move |e: &mut PlayerJoin, cx: &mut Context<'_>| {
                    if e.player() != Id::new(SessionId::new(2)) {
                        return;
                    }
                    log(e.join_message().to_owned());
                    match set {
                        Some(text) => e.set_join_message(text.to_owned()),
                        None => cx.player_mut(e.player()).unwrap().kick("bye").unwrap(),
                    }
                };
                let mut listeners = on(listener);
                listeners.push_monitor(|e: &PlayerJoin, cx: &Context<'_>| {
                    let spawned = cx.state.players[&e.player().session()].phase == Phase::Spawned;
                    log(format!("monitor {spawned}"));
                });
                let mut state = state(|_| {}, listeners);
                let mut first = spawn(&mut state, 1);
                texts(&mut first);
                logged();
                let _second = spawn(&mut state, 2);
                assert_eq!(logged(), [JOINED_KEY, "monitor true"]);
                assert_eq!(texts(&mut first), expected);
            }
        }

        // pmmp: PlayerDuplicateLoginEvent
        #[test]
        fn duplicate_login_monitors_see_the_outcome_applied() {
            let mut listeners = Listeners::default();
            listeners.push_monitor(|e: &PlayerDuplicateLogin, cx: &Context<'_>| {
                let sessions: Vec<u64> = cx.state.players.keys().map(|s| s.get()).collect();
                log(format!("{} {sessions:?}", e.is_cancelled()));
            });
            let mut state = state(|_| {}, listeners);
            let (a, _rx1) = join_as(1, identity("Alex", None));
            run(&mut state, vec![a]);
            let (b, _rx2) = join_as(2, identity("alex", None));
            run(&mut state, vec![b]);
            assert_eq!(logged(), ["false []"]);
            assert_eq!(state.players.len(), 1);
        }

        // pmmp: PlayerViewDistanceChangeEvent
        #[test]
        fn a_view_distance_change_is_raised_once_per_change() {
            let mut listeners = on(|e: &mut PlayerViewDistanceChange, _: &mut Context<'_>| {
                log(format!("{:?} {}", e.old_distance(), e.new_distance()));
            });
            listeners.push_monitor(|e: &PlayerViewDistanceChange, cx: &Context<'_>| {
                let radius = cx.state.players[&e.player().session()].chunk_radius;
                log(format!("monitor {radius:?}"));
            });
            let mut state = state(|_| {}, listeners);
            let (j, _rx) = join(1, 0);
            run(&mut state, vec![j]);
            run(&mut state, vec![radius(1, 1, 2), radius(1, 2, 2)]);
            run(&mut state, vec![radius(1, 3, 4), radius(1, 4, 99)]);
            assert_eq!(
                logged(),
                [
                    "None 2",
                    "monitor Some(2)",
                    "Some(2) 4",
                    "monitor Some(4)",
                    "Some(4) 16",
                    "monitor Some(16)"
                ]
            );
        }

        // pmmp: Player::setViewDistance
        #[test]
        fn the_api_sets_the_view_distance_within_the_setting_and_tells_the_client() {
            let listeners = on(|e: &mut PlayerViewDistanceChange, _: &mut Context<'_>| {
                log(format!("{:?} {}", e.old_distance(), e.new_distance()));
            });
            let mut state = state(|c| c.general.view_distance = 6, listeners);
            let mut rx = spawn(&mut state, 1);
            let (j, _rx2) = join(2, 0);
            run(&mut state, vec![j]);
            logged();
            received(&mut rx);
            let (one, two) = (Id::new(SessionId::new(1)), Id::new(SessionId::new(2)));
            let mut cx = Context { state: &mut state };
            assert_eq!(
                cx.set_view_distance(two, 4),
                None,
                "its client has not asked"
            );
            assert_eq!(cx.set_view_distance(Id::new(SessionId::new(9)), 4), None);
            assert_eq!(
                cx.set_view_distance(one, 99),
                Some(6),
                "at most view-distance"
            );
            assert_eq!(cx.player(one).unwrap().view_distance(), Some(6));
            assert_eq!(state.players[&SessionId::new(1)].view.radius(), 6);
            run(&mut state, Vec::new());
            let told = |rx: &mut mpsc::Receiver<Outbound>| -> Vec<i32> {
                received(rx)
                    .0
                    .into_iter()
                    .filter_map(|p| match p {
                        Packet::ChunkRadiusUpdated(c) => Some(c.chunk_radius),
                        _ => None,
                    })
                    .collect()
            };
            assert_eq!(told(&mut rx), [6]);
            let mut cx = Context { state: &mut state };
            assert_eq!(cx.set_view_distance(one, 0), Some(1), "at least 1");
            assert_eq!(state.players[&SessionId::new(1)].view.radius(), 1);
            run(&mut state, Vec::new());
            assert_eq!(told(&mut rx), [1]);
            assert!(logged().is_empty(), "a direct call raises no event");
        }
    }
}
