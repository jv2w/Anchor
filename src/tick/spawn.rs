//! Joining and spawning: the saved player data, the start-game packets, the chunk radius and the
//! spawn status.
//!
//! The packet order and the values follow what BDS 1.26.52 sends (P6, `xtask/data/p6/facts.md`);
//! the packets BDS sends that Anchor does not send are listed in `xtask/data/p6/deferred.txt`. The
//! player list is kept for everyone: a placed player gets the whole list in its window and is added
//! to everyone else's, and a player who leaves is removed from the others' lists.

mod vanilla;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::command::{self, Refusal};
use super::hook::{self, StartHook};
use super::queue::{self, Identity, Message, Outbound, SessionId, SharedBatches, SharedPacket};
use super::save::storage_key;
use super::survival::EYE_HEIGHT;
use super::view::{self, Changes, Encoded};
use super::{Output, Phase, Player, PlayerKey, Settings, State, inside_border};
use super::{dispatch, extra, interact, inventory, survival};
use crate::config::Difficulty;
use crate::crafting;
use crate::custom;
use crate::entity::MAX_SKIN_BYTES;
use crate::event::{
    DuplicateOutcome, KickFlag, PlayerDuplicateLogin, PlayerInfo, PlayerJoin, PlayerLogin,
    PlayerViewDistanceChange,
};
use crate::item::SHIELD_RUNTIME_ID;
use crate::login::MAX_TOKEN_PAYLOAD_BYTES;
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
/// The limited world size BDS sends (P6); the client ignores it for infinite worlds.
const LIMITED_WORLD_SIZE: i32 = 16;
/// BDS rewinds 40 ticks of movement history for server-authoritative movement (P6).
const REWIND_HISTORY_SIZE: i32 = 40;
/// The position BDS's `SetSpawnPosition` carries for a default spawn (P6).
const DEFAULT_SPAWN: [i32; 3] = [i32::MIN; 3];
/// The dimension BDS's `SetSpawnPosition` names at join (P6).
const SPAWN_POSITION_DIMENSION: i32 = 3;
/// The most wire bytes of a player list entry besides its name, XUID, PlayFab ID and skin
/// ([`queue::Skin::size`]): the entry kind (a varint, at most 5, and a byte), the UUID (16), the
/// entity ID (at most 10), 4 text length prefixes (at most 5 each), the build platform (4), 3
/// bools and the colour (4).
const LIST_ENTRY_FIXED_BYTES: usize = 63;
/// The most bytes a `PlayerList` packet takes in a batch besides its entries: its length, its
/// header and its entry count (at most 5 each).
const LIST_PACKET_FIXED_BYTES: usize = 15;
/// The most digits of a XUID (`u64::MAX`).
const XUID_DIGITS: usize = 20;
/// The most bytes a `PlayerList` packet of one entry takes in a batch: a name and a PlayFab ID
/// together as long as the one identity token that holds both and a skin of [`MAX_SKIN_BYTES`].
/// A spawn window cannot split an entry, so `max-batch-bytes` is at least this (the settings
/// check it).
pub(crate) const MAX_LIST_ENTRY_BYTES: usize = LIST_PACKET_FIXED_BYTES
    + LIST_ENTRY_FIXED_BYTES
    + MAX_TOKEN_PAYLOAD_BYTES
    + XUID_DIGITS
    + MAX_SKIN_BYTES;
/// The colour of every entry of the player list (BDS's value, P6).
pub(super) const PLAYER_LIST_COLOUR: Rgba = Rgba {
    r: 237,
    g: 237,
    b: 237,
    a: 255,
};
/// The entity types of `AvailableActorIdentifiers`: `CloudburstMC`'s `entity_identifiers.dat`
/// (8,971 bytes), sent as it is; BDS 1.26.52 sent its version of the file byte for byte (P6).
const ENTITY_IDENTIFIERS: &[u8] =
    include_bytes!("../../xtask/data/reference/entity_identifiers.dat");
/// The block collision shapes BDS 1.26.52 sends in `VoxelShapes`: `CloudburstMC`'s
/// `voxel_shapes.json`, whose counts (287 shapes, 220 names), name order and encoded size (23,178
/// bytes) equal BDS's (P6).
const VOXEL_SHAPES: &str = include_str!("../../xtask/data/reference/voxel_shapes.json");
/// The y BDS sends in its first two spawn `Respawn` packets: the top of the build limit, above
/// the eyes and at the feet (P6).
const RESPAWN_SEARCH_Y: f64 = 32_768.0;
/// The join message, BDS's yellow "joined the game" line: BDS sends it as a translated `Text`
/// with the name (P6 duplicate capture). Listeners see this key; a message left at it is sent so,
/// any other as it is (like [`LEFT`](super::player::LEFT)).
const JOINED_KEY: &str = "\u{a7}e%multiplayer.player.joined";

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

/// `voxel_shapes.json`.
#[derive(Deserialize)]
struct ShapesFile {
    names: Names,
    shapes: Vec<ShapeJson>,
}

/// The `names` object as pairs in file order.
struct Names(Vec<(String, u16)>);

impl<'de> Deserialize<'de> for Names {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        /// Collects the entries in order.
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Names;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShapeJson {
    cells: CellsJson,
    x_coordinates: Vec<f32>,
    y_coordinates: Vec<f32>,
    z_coordinates: Vec<f32>,
}

/// The cells of a shape.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CellsJson {
    x_size: u8,
    y_size: u8,
    z_size: u8,
    storage: Vec<u8>,
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

/// What the spawn windows of a tick share: the player list and each world's clock and sleeping
/// packets. Built only when a player needs a window this tick.
#[derive(Debug, Default)]
struct Window {
    /// The list of every placed player, in session order, built once for every window: it holds
    /// each player's skin (up to 4 MiB). A client takes at most
    /// [`PLAYER_LIST_ENTRIES_MAX_LEN`] entries in one packet and a session sends at most
    /// `max-batch-bytes` in one batch, so a longer or larger list goes in several.
    player_list: Vec<Arc<SharedPacket>>,
    /// The packets of each world a window is in. Only those worlds, as each world's sleeping
    /// counts look at every player.
    worlds: BTreeMap<WorldIndex, WorldPackets>,
}

impl Window {
    /// Builds what the spawn windows of `marked` share; empty when none goes out this tick.
    fn new(state: &State, marked: &[Marked<'_>]) -> Self {
        let mut worlds = BTreeMap::new();
        for Marked { player: p, .. } in marked {
            if (p.pending.start_game || p.pending.radius.is_some_and(|r| r.first))
                && !worlds.contains_key(&p.world)
            {
                let w = p.world;
                worlds.insert(
                    w,
                    WorldPackets {
                        clock_state: survival::clock_state(state, w),
                        clock_definition: survival::clock_definition(state, w),
                        sleeping: survival::sleeping_players(state, w),
                    },
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
            player_list: list_packets(&placed, state.settings.max_batch_bytes),
            worlds,
        }
    }

    /// The packets of `world`, if a window this tick is in it.
    fn world(&self, world: WorldIndex) -> Option<&WorldPackets> {
        self.worlds.get(&world)
    }

    /// The player list of the window, shared.
    fn player_list(&self) -> impl Iterator<Item = Message> + '_ {
        self.player_list
            .iter()
            .map(|p| Message::SpawnList(Arc::clone(p)))
    }
}

/// The packets of one world the spawn windows of a tick send.
#[derive(Debug)]
struct WorldPackets {
    /// The clock state.
    clock_state: Packet,
    /// The clock definition.
    clock_definition: Packet,
    /// The sleeping counts.
    sleeping: Packet,
}

/// A player with something to send at the end of a tick, and the packets the serial phases
/// queued for it, taken out of it so that the output phase moves them instead of copying them.
struct Marked<'a> {
    session: SessionId,
    player: &'a Player,
    outbox: Vec<Packet>,
    shared_outbox: Vec<Arc<SharedPacket>>,
}

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

/// How a refused join ends: the reason for the log and the session's last packet.
struct Refused {
    reason: String,
    packet: Packet,
}

/// A player waiting for its saved data: its session, and which of Anchor's refusals
/// ([`command::refusals`], in their order) its login let through because a [`PlayerLogin`]
/// listener cleared them. When the data arrives, a ban or a whitelist change made since the
/// login refuses the player ([`player_loaded`]); what the login let through stays let through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Loading {
    pub(super) session: SessionId,
    let_through: [bool; 3],
}

impl Loading {
    /// Hashes every field (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            session,
            let_through,
        } = self;
        h.u64(session.get());
        for &let_through in let_through {
            h.u64(u64::from(let_through));
        }
    }
}

super::checkpoint::fields!(Loading {
    session,
    let_through,
});

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
        let asked = distance.max(1);
        let radius = granted_radius(&self.state.settings, p.world, asked);
        let ready = p.phase != Phase::Loading && p.chunk_radius.is_some();
        if ready {
            p.asked_radius = Some(asked);
            grant_radius(self.state, &mut p, radius);
        }
        self.state.players.insert(session, p);
        ready.then_some(radius)
    }
}

/// Reads the voxel shapes of [`VOXEL_SHAPES`], names in the file's order (BDS's order).
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

/// Builds the armour trim patterns and materials BDS 1.26.52 sends (P6), from the tables the
/// smithing table uses ([`crafting::TRIM_PATTERNS`], [`crafting::TRIM_MATERIAL_TABLE`]).
fn trim_data() -> TrimData {
    TrimData {
        patterns: crafting::TRIM_PATTERNS
            .iter()
            .map(|p| TrimPattern {
                item_name: format!("minecraft:{p}_armor_trim_smithing_template"),
                pattern_id: (*p).to_owned(),
            })
            .collect(),
        materials: crafting::TRIM_MATERIAL_TABLE
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
    let mut bodies = Vec::new();
    batch::encode_bodies(
        packets,
        SHIELD_RUNTIME_ID,
        Some(encoder),
        &mut Vec::new(),
        &mut bodies,
    )?;
    Ok(bodies.into())
}

/// Runs the output phase: works out the sub-chunks the answers need, once each with their height
/// maps on the CPU pool; then builds the packets of every player with something to send on the
/// CPU pool from read-only state in session order, after them the packets the serial phases
/// queued (moved out of the outboxes, never copied), and passes them through the send hooks;
/// then clears the pending marks.
pub(super) fn output(
    state: &mut State,
    pool: &Pool,
    changes: &Changes,
) -> Vec<(SessionId, Vec<Message>)> {
    let ops = operators_starting(state);
    let encoded = Encoded::new(state, pool);
    let mut taken = Vec::new();
    for (&session, p) in &mut state.players {
        if p.pending != Pending::default()
            || !p.view.out.is_empty()
            || !p.outbox.is_empty()
            || !p.shared_outbox.is_empty()
            || (changes.contains_key(&p.world) && p.view.is_open())
        {
            let outboxes = (
                std::mem::take(&mut p.outbox),
                std::mem::take(&mut p.shared_outbox),
            );
            taken.push((session, outboxes));
        }
    }
    // Both in session order: each taken outbox meets its player.
    let mut taken = taken.into_iter().peekable();
    let marked: Vec<Marked<'_>> = state
        .players
        .iter()
        .filter_map(|(session, player)| {
            let (_, (outbox, shared_outbox)) = taken.next_if(|(s, _)| s == session)?;
            Some(Marked {
                session: *session,
                player,
                outbox,
                shared_outbox,
            })
        })
        .collect();
    let window = Window::new(state, &marked);
    let (worlds, settings, shared) = (&state.worlds, &state.settings, &state.shared);
    let (sends, starts) = (state.hooks.send(), state.hooks.start_game());
    let commands = state.commands.listing();
    let sent = pool.map_owned(marked, |marked| {
        let Marked {
            session,
            player,
            mut outbox,
            mut shared_outbox,
        } = marked;
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
        let mut out = messages(&built, worlds.get(player.world), session, player);
        // The serial phases' packets follow the output phase's ([`Player::send_packet`]).
        out.extend(outbox.drain(..).map(Message::Packet));
        out.extend(shared_outbox.drain(..).map(Message::SharedPacket));
        hook::send(sends, settings, session, &mut out);
        (session, out, (outbox, shared_outbox))
    });
    let mut packets = Vec::with_capacity(sent.len());
    for (session, out, outboxes) in sent {
        // The emptied outboxes go back, keeping their capacity for the next tick.
        if let Some(player) = state.players.get_mut(&session) {
            (player.outbox, player.shared_outbox) = outboxes;
        }
        if !out.is_empty() {
            packets.push((session, out));
        }
    }
    for player in state.players.values_mut() {
        player.pending = Pending::default();
        player.view.out.clear();
    }
    packets
}

/// Finds the operators among the players whose `StartGame` goes out this tick: their `StartGame`
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

/// Builds one player's packets of the tick, in the order BDS sends them (P6): the start-game
/// packets; for the first chunk radius answer the spawn window (spawn position, clocks,
/// difficulty, settings, weather, abilities, rules, player list, biomes, entity types, fog, camera
/// presets, attributes, creative inventory, trims, inventory, recipes, commands, respawn, entity
/// data, structure feature); the granted radius; the chunk packets; the health and the spawn
/// status; then the block changes it sees.
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
    let packets = window.world(player.world);
    if player.pending.start_game {
        out.extend(packets.map(|w| Message::Packet(w.sleeping.clone())));
        out.extend(window.player_list());
        out.extend(packets.map(|w| Message::Packet(w.clock_state.clone())));
        out.push(Message::Shared(Arc::clone(&shared.jigsaw_structure_data)));
        out.push(Message::Shared(Arc::clone(&shared.voxel_shapes)));
        let permissions = if ops.contains(&session) {
            PermissionLevel::Operator
        } else {
            PermissionLevel::Member
        };
        let start = start_game(settings, shared, world, player, permissions);
        let start = hook::start_game(starts, start);
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

/// Puts in `out` the packets BDS sends between the chunk radius request and the chunks (P6), with
/// the player's `AvailableCommands`, but for the spawn sound Anchor never sends (deferred.txt).
/// BDS sends the entity data and the structure feature with its first chunks; the chunk packets
/// leave the order of the others as it is.
fn spawn_window(
    settings: &Settings,
    shared: &Shared,
    world: &World,
    player: &Player,
    window: &Window,
    commands: Packet,
    out: &mut Vec<Message>,
) {
    let packet = |p: Packet| Message::Packet(p);
    out.push(packet(Packet::SetSpawnPosition(SetSpawnPosition {
        spawn_type: i32::from(SpawnType::Player),
        position: DEFAULT_SPAWN,
        dimension: SPAWN_POSITION_DIMENSION,
        spawn_position: DEFAULT_SPAWN,
    })));
    out.extend(
        window
            .world(player.world)
            .map(|w| packet(w.clock_definition.clone())),
    );
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

/// Builds a player's entry in the player list (BDS values, P6): the skin and device the client
/// sent at login, unchanged. The PlayFab ID is the identity token's (gophertunnel: the
/// server-verified one; BDS's value is not confirmed).
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
        playfab_id: player.identity.client.playfab_id.clone(),
        platform_chat_id: String::new(),
        // BDS sends the login's device (PMMP the unknown device, -1).
        build_platform: player.identity.device_os,
        skin: Arc::clone(player.identity.skin.shared()),
        teacher: false,
        host: false,
        sub_client: false,
        player_colour: PLAYER_LIST_COLOUR,
    }
}

/// Builds the `PlayerList` packets that list `players` in order: each of at most
/// [`PLAYER_LIST_ENTRIES_MAX_LEN`] entries and of at most `max_batch_bytes` in a batch. One
/// entry alone always fits ([`MAX_LIST_ENTRY_BYTES`]).
fn list_packets(players: &[&Player], max_batch_bytes: usize) -> Vec<Arc<SharedPacket>> {
    let mut packets = Vec::new();
    let mut entries = Vec::new();
    let mut bytes = LIST_PACKET_FIXED_BYTES;
    for player in players {
        let entry_bytes = LIST_ENTRY_FIXED_BYTES
            .saturating_add(player.identity.name.len())
            .saturating_add(XUID_DIGITS)
            .saturating_add(player.identity.client.playfab_id.len())
            .saturating_add(player.identity.skin.size());
        if entries.len() == PLAYER_LIST_ENTRIES_MAX_LEN
            || (!entries.is_empty() && bytes.saturating_add(entry_bytes) > max_batch_bytes)
        {
            let entries = std::mem::take(&mut entries);
            packets.push(shared_list(entries));
            bytes = LIST_PACKET_FIXED_BYTES;
        }
        entries.push(list_entry(player));
        bytes = bytes.saturating_add(entry_bytes);
    }
    if !entries.is_empty() {
        packets.push(shared_list(entries));
    }
    packets
}

/// Adds a placed player to the list of every other placed player (their own windows, if they are
/// placed this tick too, list everyone), its skin held once for all of them.
fn announce(state: &mut State, player: &Player) {
    let packet = shared_list(vec![list_entry(player)]);
    for p in state.players.values_mut() {
        if gets_list_changes(p) {
            p.send_shared(Arc::clone(&packet));
        }
    }
}

/// Whether `player` is sent the changes of the player list: it is placed, and its own window,
/// which lists every placed player, is not still to go out.
fn gets_list_changes(player: &Player) -> bool {
    player.phase != Phase::Loading && !player.pending.start_game
}

/// Wakes up a player that left (`State::remove`) and takes it out of the other players' lists,
/// if they were told of it.
pub(super) fn left(state: &mut State, player: &mut Player) {
    survival::left(state, player);
    if player.phase == Phase::Loading {
        return;
    }
    let entries = vec![PlayerListEntry {
        action_type: PlayerListAction::Remove.into(),
        uuid: player.identity.uuid,
        ..PlayerListEntry::default()
    }];
    let told = state.players.values_mut().filter(|p| gets_list_changes(p));
    if player.pending.start_game {
        // Placed this tick: `announce` added it through the shared queue, which goes out after
        // the packets, so the removal follows it there.
        let packet = shared_list(entries);
        for p in told {
            p.send_shared(Arc::clone(&packet));
        }
    } else {
        let packet = Packet::PlayerList(PlayerList { entries });
        for p in told {
            p.send_packet(packet.clone());
        }
    }
}

/// Holds a `PlayerList` of `entries` for every player it goes to.
fn shared_list(entries: Vec<PlayerListEntry>) -> Arc<SharedPacket> {
    Arc::new(SharedPacket::new(Packet::PlayerList(PlayerList {
        entries,
    })))
}

/// Registers the player of `session`, or rejects it when the server is full. A second join of
/// the same session changes nothing. Returns the load of the player's saved data, if the tick
/// does not hold it; the player waits in [`Phase::Loading`] until it arrives.
///
/// When the same player is already online, [`PlayerDuplicateLogin`] decides which of the two is
/// disconnected. Cancelled, the connecting player goes at once. Otherwise the connecting player
/// is checked first ([`login`], where the slot it would take over is free, so a full server still
/// lets a player take over its own place), and the existing player is disconnected only once the
/// connecting one is admitted: a login that is refused leaves the existing player online.
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
    let mut replaced = None;
    let mut duplicate = None;
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
        replaced = Some((existing, outcome.reason, packet));
        duplicate = event;
    }
    let (admitted, event) = login(state, &identity);
    let request = match admitted {
        Err(Refused { reason, packet }) => {
            info!(session = session.get(), name = %identity.name, %reason, "join rejected");
            output.close(outbound, packet);
            None
        }
        Ok(let_through) => {
            if let Some((existing, reason, packet)) = replaced {
                replace(state, existing, session, &reason, packet, output);
            }
            let loading = Loading {
                session,
                let_through,
            };
            enter(state, loading, identity, key, outbound, output)
        }
    };
    if let Some(e) = &duplicate {
        dispatch::watch(state, e);
    }
    if let Some(e) = &event {
        dispatch::watch(state, e);
    }
    request
}

/// Disconnects the player of `existing`, whom the admitted login of `by` replaces, with `packet`.
fn replace(
    state: &mut State,
    existing: SessionId,
    by: SessionId,
    reason: &str,
    packet: Packet,
    output: &mut Output,
) {
    if let Some(old) = state.players.get(&existing) {
        info!(
            session = existing.get(),
            by = by.get(),
            name = %old.identity.name,
            "player replaced by a new login"
        );
    }
    state.send_off(
        existing,
        reason,
        super::player::LEFT.to_owned(),
        super::player::QuitCause::Disconnect,
        packet,
        output,
    );
}

/// Registers the admitted player of `loading`'s session (see [`join`]), which waits as `loading`
/// if its saved data must be loaded; one placed dead respawns ([`survival::respawn_joined`]).
fn enter(
    state: &mut State,
    loading: Loading,
    identity: Identity,
    key: PlayerKey,
    outbound: mpsc::Sender<Outbound>,
    output: &mut Output,
) -> Option<Request> {
    let session = loading.session;
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
    // A held record of the player goes first, its changes to the departed players' data.
    super::task::offline_joined(state, &stored);
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
        state.loading.insert(stored.clone(), loading);
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

/// Converts a saved position to the tick's, if it is inside the world border (the file is
/// external input: a finite position may still be anywhere).
fn tick_position(position: [f64; 3]) -> Option<Vec3> {
    let [x, y, z] = position;
    let position = Vec3::new(x, y, z);
    inside_border(position).then_some(position)
}

/// Computes the eye position of a player standing on a world's spawn block.
fn spawn_position(world: Option<&World>) -> Vec3 {
    let spawn = world.map(World::spawn).unwrap_or_default();
    survival::standing_eye(spawn)
}

/// Places a loading player whose saved data arrived: it joins where it was, at the spawn if it
/// is new, or at its respawn point if it was dead ([`survival::respawn_joined`]). Damaged data
/// refuses the join and leaves the saved bytes as they are. A ban or a whitelist change made
/// while the data loaded refuses the join as the login would have, the data left as saved: the
/// commands and calls that kick the players a ban or whitelist change leaves out see only
/// players in the game ([`Loading`]).
pub(super) fn player_loaded(
    state: &mut State,
    key: &StoredPlayer,
    data: PlayerLoad,
    output: &mut Output,
) {
    let Some(Loading {
        session,
        let_through,
    }) = state.loading.remove(key)
    else {
        return;
    };
    let Some(player) = state.players.get(&session) else {
        return;
    };
    if player.phase != Phase::Loading {
        return;
    }
    // Asked while the player is in the table, which the slot count reads. A full server is not
    // asked again: the player holds its slot since the login.
    let [_, whitelist, ban] = command::refusals(state, &player.identity);
    let [_, whitelist_let_through, ban_let_through] = let_through;
    let since_login = whitelist
        .filter(|_| !whitelist_let_through)
        .or(ban.filter(|_| !ban_let_through));
    let data = match data {
        PlayerLoad::Found(data) => Some(data),
        PlayerLoad::Absent => None,
        PlayerLoad::Damaged => {
            error!(
                player = key.as_str(),
                "saved player data is damaged; the join is refused and the data kept as saved"
            );
            let packet = Packet::Disconnect(Disconnect {
                reason: i32::from(DisconnectReason::Unknown),
                hide_disconnection_screen: false,
                message: DAMAGED_DATA_MESSAGE.to_owned(),
                filtered_message: String::new(),
            });
            refuse_loaded(state, session, packet, output);
            return;
        }
    };
    if let Some(refusal) = since_login {
        let reason = refusal.message();
        info!(session = session.get(), player = key.as_str(), %reason, "join rejected after loading");
        refuse_loaded(state, session, refusal.packet(), output);
        return;
    }
    #[expect(
        clippy::expect_used,
        reason = "invariant: the loading player was found in the table above"
    )]
    let mut player = state
        .players
        .remove(&session)
        .expect("the loading player is in the table");
    place(state, &mut player, data);
    state.players.insert(session, player);
    survival::respawn_joined(state, session);
}

/// Ends the join of the loading player of `session` with `packet`, keeping its saved data as
/// it is.
fn refuse_loaded(state: &mut State, session: SessionId, packet: Packet, output: &mut Output) {
    if let Some(p) = state.remove(session) {
        state.close(p, packet, output);
    }
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
    #[expect(
        clippy::expect_used,
        reason = "invariant: the player index names only players in the table (State::insert and \
                  State::remove keep it so)"
    )]
    let old = state
        .players
        .get(&existing)
        .expect("the indexed player is in the table");
    // One clone of the existing identity per duplicate login, only when someone listens; the
    // monitors get a copy of the event after the outcome is applied.
    let mut event = PlayerDuplicateLogin::new(connecting, old.identity.clone());
    dispatch::call(state, &mut event);
    let watched = event.clone();
    let (identity, outcome) = event.into_outcome();
    (identity, outcome, Some(watched))
}

/// Decides whether `identity` may join (PMMP `PlayerPreLoginEvent`, which Anchor raises here, in
/// the tick, after the duplicate login check): Anchor's refusals ([`command::refusals`]) are the
/// event's kick flags, listeners may clear them or set others, and the first flag left refuses
/// the player with its screen message. Without listeners the first refusal decides
/// ([`command::admit`]) and no event is built. Returns which of Anchor's refusals an admitted
/// login let through (listeners cleared them; [`Loading`]) or the refusal, and the event for the
/// monitors, who see it after the join is applied.
fn login(
    state: &mut State,
    identity: &Identity,
) -> (Result<[bool; 3], Refused>, Option<PlayerLogin>) {
    if !dispatch::heard::<PlayerLogin>(state) {
        let admitted = command::admit(state, identity)
            .map(|()| [false; 3])
            .map_err(|r| Refused {
                reason: r.message(),
                packet: r.packet(),
            });
        return (admitted, None);
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
    for r in anchor.iter().flatten() {
        event.set_kick_flag(kick_flag(r), r.message(), None);
    }
    dispatch::call(state, &mut event);
    if dispatch::listened::<PlayerLogin>(state) {
        // A listener may have changed the game (banned the player, whitelisted the server): a
        // refusal that was not there before sets its flag; one a listener cleared stays clear.
        let now = command::refusals(state, identity);
        for (was, is) in anchor.iter_mut().zip(now) {
            if was.is_none()
                && let Some(r) = is
            {
                event.set_kick_flag(kick_flag(&r), r.message(), None);
                *was = Some(r);
            }
        }
    }
    let refused = event.kick_flags().next().map(|flag| {
        let reason = event.disconnect_reason(flag).unwrap_or_default().to_owned();
        let screen = event.disconnect_screen_message(flag).unwrap_or_default();
        // Anchor's own refusal, unchanged, ends as without listeners (a full server's
        // `PlayStatus` lets the client show its own text).
        let own = anchor
            .iter()
            .flatten()
            .find(|r| kick_flag(r) == flag)
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
            Refusal::packet,
        );
        Refused { reason, packet }
    });
    let admitted = match refused {
        Some(refused) => Err(refused),
        None => Ok(anchor.map(|r| r.is_some())),
    };
    (admitted, Some(event))
}

/// Names the kick flag of [`PlayerLogin`] that stands for one of Anchor's refusals.
fn kick_flag(refusal: &Refusal) -> KickFlag {
    match refusal {
        Refusal::Full => KickFlag::ServerFull,
        Refusal::NotWhitelisted => KickFlag::ServerWhitelisted,
        Refusal::Banned(_) | Refusal::IpBanned(_) => KickFlag::Banned,
    }
}

/// Builds the packet that disconnects the losing side of a duplicate login.
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
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if player.phase == Phase::Loading {
        debug!(
            session = session.get(),
            "chunk radius before the player data ignored"
        );
        return;
    }
    let asked = u8::try_from(request.chunk_radius).unwrap_or(u8::MAX);
    let radius = granted_radius(&state.settings, player.world, asked);
    player.asked_radius = Some(asked);
    // An open view follows the granted radius in the view phase ([`view::update`]), once a tick:
    // each request resizing it would cost the whole disc, and nothing bounds how many requests a
    // client sends. Only a view that opens needs the player out of the table.
    let old = if player.view.is_open() {
        answer_radius(player, radius)
    } else {
        #[expect(
            clippy::expect_used,
            reason = "invariant: the player was found in the table above"
        )]
        let mut player = state
            .players
            .remove(&session)
            .expect("the asking player is in the table");
        let old = grant_radius(state, &mut player, radius);
        state.players.insert(session, player);
        old
    };
    if old == Some(radius) || !dispatch::heard::<PlayerViewDistanceChange>(state) {
        return;
    }
    let mut event = PlayerViewDistanceChange {
        player: player::Id::new(session),
        old_distance: old,
        new_distance: radius,
    };
    dispatch::call(state, &mut event);
    dispatch::watch(state, &event);
}

/// Computes the chunk radius a player who asks for `asked` is granted in `world`: at most the
/// world's `view-distance`.
pub(super) fn granted_radius(settings: &Settings, world: WorldIndex, asked: u8) -> u8 {
    asked.min(settings.view(world).view_distance)
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

/// Records `radius` as the answer the output phase sends, as [`grant_radius`] does, but leaves
/// the view to follow in the view phase. Returns the radius it had.
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

/// Announces that the player of `session` spawned (PMMP `Player::doFirstSpawn`): [`PlayerJoin`]
/// may change the join message, which every other player in the game then sees (BDS sends it to
/// the others only; PMMP to everyone). An empty message shows nothing; a player a listener kicked
/// shows none.
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
        if s != session && p.gets_broadcasts() {
            for packet in packets.iter().flatten() {
                p.send_packet(packet.clone());
            }
        }
    }
}

/// Converts a difficulty to its protocol ID ([`Difficulty::id`]) as the packets carry it.
pub(super) fn difficulty_id(difficulty: Difficulty) -> i32 {
    i32::from(difficulty.id())
}

/// Builds the `StartGame` of `player` in `world`, with its `permissions`.
fn start_game(
    settings: &Settings,
    shared: &Shared,
    world: &World,
    player: &Player,
    permissions: PermissionLevel,
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
        player_permissions: u8::from(permissions),
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

    /// Reads the `CloudburstMC` data copy, when the external sources are there (`.cache`).
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

    /// Encodes the packet's payload: the bytes after its header.
    pub(super) fn payload(packet: &Packet) -> Vec<u8> {
        let mut bytes = Vec::new();
        packet.encode(&mut bytes, 0).unwrap();
        bytes
    }

    #[test]
    fn the_entity_types_are_the_cloudburst_bytes_bds_sends() {
        assert_eq!(
            ENTITY_IDENTIFIERS.len(),
            8971,
            "the CloudburstMC 1.26.60 file is 8,971 bytes"
        );
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
        use crate::event::Listeners;
        use crate::tick::tests::{join, new_state, received, settings, tick};

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
        use crate::event::Listeners;
        use crate::tick::tests::{join, new_state, radius, received, settings, tick};

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

    /// A player list goes in packets of at most the batch bytes a session sends, an entry alone
    /// whatever its size.
    #[test]
    fn a_player_list_larger_than_a_batch_goes_in_several_packets() {
        use crate::event::Listeners;
        use crate::tick::tests::{join, new_state, settings, tick};

        let mut state = new_state(settings(|_| {}), Listeners::default());
        let (mut joins, _queues): (Vec<_>, Vec<_>) = (1..=5).map(|s| join(s, 0)).unzip();
        tick(&mut state, &mut joins, &mut Output::default());
        let placed: Vec<&Player> = state.players.values().collect();
        let entries = |max_batch_bytes: usize| -> Vec<usize> {
            list_packets(&placed, max_batch_bytes)
                .iter()
                .map(|p| match p.get() {
                    Packet::PlayerList(l) => l.entries.len(),
                    other => panic!("{other:?}"),
                })
                .collect()
        };
        assert_eq!(entries(usize::MAX), [5]);
        assert_eq!(entries(1), [1, 1, 1, 1, 1]);
    }

    #[test]
    fn a_player_placed_and_gone_in_one_tick_leaves_the_other_lists() {
        use crate::event::Listeners;
        use crate::tick::tests::{join, leave, new_state, received, settings, tick};

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

    #[test]
    fn the_player_list_carries_the_identity_playfab_id() {
        use crate::event::Listeners;
        use crate::tick::queue::InputKind;
        use crate::tick::tests::{join, new_state, radius, received, settings, tick};

        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        let (mut first, mut rx) = join(1, 0);
        if let InputKind::Join { identity, .. } = &mut first.kind {
            identity.client.playfab_id = "0123456789abcdef".to_owned();
        }
        tick(&mut state, &mut vec![first], &mut output);
        tick(&mut state, &mut vec![radius(1, 1, 1)], &mut output);
        let ids: Vec<String> = received(&mut rx)
            .0
            .into_iter()
            .filter_map(|p| match p {
                Packet::PlayerList(l) => Some(l.entries),
                _ => None,
            })
            .flatten()
            .map(|e| e.playfab_id)
            .collect();
        assert!(!ids.is_empty());
        assert!(ids.iter().all(|id| id == "0123456789abcdef"));
    }

    /// The login, join, duplicate login and view distance events.
    mod events {
        use std::cell::Cell;
        use std::net::{IpAddr, Ipv4Addr};

        use tokio::sync::mpsc;

        use crate::event::{
            Event, KickFlag, Listeners, PlayerDuplicateLogin, PlayerJoin, PlayerLogin, PlayerQuit,
            PlayerViewDistanceChange, Priority,
        };
        use crate::permission::BanEntry;
        use crate::player::Id;
        use crate::protocol::consts::TextType;
        use crate::protocol::packet::Packet;
        use crate::tick::queue::{Identity, Outbound};
        use crate::tick::spawn::JOINED_KEY;
        use crate::tick::tests::{
            initialised, input, join, new_state, radius, received, settings, tick,
        };
        use crate::tick::{Context, InputKind, Output, Phase, SessionId, State};

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

        /// Builds listeners with one listener of `E` at normal priority.
        fn on<E: Event>(
            listener: impl FnMut(&mut E, &mut Context<'_>) + Send + 'static,
        ) -> Listeners {
            let mut l = Listeners::default();
            l.push(Priority::Normal, listener);
            l
        }

        fn state(edit: impl FnOnce(&mut crate::config::Config), listeners: Listeners) -> State {
            LOG.set(Vec::new());
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

        /// Joins session 1 as `name` on `state`, which loads saved player data: the player waits
        /// for its data, whose answer `output` holds for the next tick. Returns its queue.
        fn join_loading(
            state: &mut State,
            name: &str,
            output: &mut Output,
        ) -> mpsc::Receiver<Outbound> {
            state.saving.players = true;
            let (j, rx) = join_as(1, identity(name, None));
            tick(state, &mut vec![j], output);
            assert_eq!(state.players[&SessionId::new(1)].phase, Phase::Loading);
            rx
        }

        /// A ban or a whitelist change made while a player's saved data loads, which the kicks of
        /// the commands and calls that make them do not reach, refuses the player when the data
        /// arrives.
        #[test]
        fn a_ban_or_whitelist_change_while_the_data_loads_refuses_the_join() {
            for (banned, message) in [
                (true, "This server has banned you"),
                (false, "disconnectionScreen.notAllowed"),
            ] {
                let mut state = state(|_| {}, Listeners::default());
                let mut output = Output::default();
                let mut rx = join_loading(&mut state, "Loader", &mut output);
                let mut cx = Context { state: &mut state };
                if banned {
                    cx.ban(BanEntry::new("Loader")).unwrap();
                } else {
                    cx.set_whitelist_enabled(true);
                }
                tick(&mut state, &mut Vec::new(), &mut output);
                let Some(Packet::Disconnect(d)) = received(&mut rx).1 else {
                    panic!("{message}: the loaded player is refused with a Disconnect");
                };
                assert_eq!(d.message, message);
                assert!(state.players.is_empty(), "{message}");
                assert!(state.loading.is_empty(), "{message}");
            }
        }

        /// A refusal a login listener cleared stays cleared when the player's data arrives.
        #[test]
        fn a_refusal_a_login_listener_cleared_lets_the_loaded_player_in() {
            let let_in = |e: &mut PlayerLogin, _: &mut Context<'_>| {
                e.clear_kick_flag(KickFlag::ServerWhitelisted);
            };
            let mut state = state(|_| {}, on(let_in));
            Context { state: &mut state }.set_whitelist_enabled(true);
            let mut output = Output::default();
            let mut rx = join_loading(&mut state, "Guest", &mut output);
            tick(&mut state, &mut Vec::new(), &mut output);
            assert_eq!(state.players[&SessionId::new(1)].phase, Phase::Joined);
            assert!(received(&mut rx).1.is_none(), "the player stays");
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

        /// Takes the texts in what `rx` was sent: (type, message, parameters).
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
            assert_eq!(logged(), ["false [2]"]);
            assert_eq!(state.players.len(), 1);
        }

        // pmmp: PlayerDuplicateLoginEvent::setDisconnectReason, PlayerQuitEvent::getQuitReason
        #[test]
        fn the_replaced_player_quits_with_the_reason_a_listener_gave() {
            for (screen, shown) in [(None, "moved"), (Some("bye"), "bye")] {
                let mut listeners = on(move |e: &mut PlayerDuplicateLogin, _: &mut Context<'_>| {
                    e.set_disconnect_reason("moved");
                    e.set_disconnect_screen_message(screen.map(str::to_owned));
                });
                listeners.push(
                    Priority::Normal,
                    |e: &mut PlayerQuit, _: &mut Context<'_>| {
                        log(e.quit_reason().to_owned());
                    },
                );
                let mut state = state(|_| {}, listeners);
                let (a, mut rx1) = join_as(1, identity("Alex", None));
                run(&mut state, vec![a]);
                run(&mut state, Vec::new());
                let (b, _rx2) = join_as(2, identity("alex", None));
                run(&mut state, vec![b]);
                assert_eq!(logged(), ["moved"], "{screen:?}");
                let (_, close, _) = received(&mut rx1);
                assert!(
                    matches!(close, Some(Packet::Disconnect(d)) if d.message == shown),
                    "{screen:?}"
                );
            }
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
