//! Joining and spawning: the saved player data, the start-game packets, the chunk radius and the
//! spawn status.
//!
//! The packet order and the values follow what the official server sends (P6,
//! `xtask/data/p6/facts.md`); the packets it sends that Anchor does not send are listed in `xtask/data/p6/deferred.txt`. The
//! player list is kept for everyone: a placed player gets the whole list in its window and is added
//! to everyone else's, and a player who leaves is removed from the others' lists.

mod vanilla;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::command::{self, Refusal};
use super::hook::{self, StartHook};
use super::queue::{self, Identity, Message, Outbound, SessionId, SharedBatches, SharedPacket};
use super::save::storage_key;
use super::survival::EYE_OFFSET;
use super::view::{self, Changes, Encoded};
use super::{Output, Phase, Player, PlayerKey, Settings, State, inside_border};
use super::{checkpoint, dispatch, extra, interact, inventory, survival};
use crate::config::Difficulty;
use crate::custom;
use crate::entity::MAX_SKIN_BYTES;
use crate::event::{
    DuplicateOutcome, KickFlag, PlayerDuplicateLogin, PlayerInfo, PlayerJoin, PlayerLogin,
    PlayerViewDistanceChange, QuitReason,
};
use crate::item;
use crate::login::MAX_TOKEN_PAYLOAD_BYTES;
use crate::math::Vec3;
use crate::player::{self, MAX_TEXT_BYTES, Message as Text};
use crate::pool::Pool;
use crate::protocol::consts::{
    self, DisconnectReason, GameType, Generator, PermissionLevel, PlayerListAction, SpawnType,
};
use crate::protocol::packet::{
    self, AvailableActorIdentifiers, ChunkRadiusUpdated, CurrentStructureFeature, Disconnect,
    Packet, PlayStatus, PlayerFog, PlayerList, RequestChunkRadius, Respawn, SetCommandsEnabled,
    SetDifficulty, SetHealth, SetLocalPlayerAsInitialised, SetSpawnPosition, StartGame, TrimData,
    VoxelShapes,
};
use crate::protocol::{self, batch};
use crate::protocol::{
    MAX_PLAYER_LIST_ENTRIES, PlayerListEntry, PlayerMovementSettings, Rgba, ServerJoinInformation,
    TrimMaterial, TrimPattern, VoxelCells, VoxelShape, VoxelShapeNameEntry,
};
use crate::save::{PlayerData, PlayerLoad, Request, WorldIndex};
use crate::storage::PlayerKey as StoredPlayer;
use crate::world::{World, biome};

/// What a player whose saved data is damaged sees.
const DAMAGED_DATA_MESSAGE: &str = "Your saved player data is damaged: ask the server operator";
/// The position the official server's `SetSpawnPosition` carries for a default spawn (P6).
const DEFAULT_SPAWN: [i32; 3] = [i32::MIN; 3];
/// The entity types of `AvailableActorIdentifiers`: `entity_identifiers.dat`
/// (8,971 bytes), sent as it is; the official server sent the file byte for byte (P6).
const ENTITY_IDENTIFIERS: &[u8] =
    include_bytes!("../../xtask/data/reference/entity_identifiers.dat");
/// The join message, the yellow "joined the game" line: the official server sends a translated
/// `Text` with the name (P6 duplicate capture). Listeners see this key; a message left at it is
/// sent so, any other as it is (like [`LEFT`](super::player::LEFT)).
const JOINED_KEY: &str = "\u{a7}e%multiplayer.player.joined";
/// The limited world size the official server sends (P6); the client ignores it for infinite worlds.
const LIMITED_WORLD_SIZE: i32 = 16;
/// The most wire bytes of a player list entry besides its name, XUID, PlayFab ID and skin
/// ([`queue::Skin::size`]): the entry kind (a varint, at most 5, and a byte), the UUID (16), the
/// entity ID (at most 10), 4 text length prefixes (at most 5 each), the build platform (4), 3
/// bools and the color (4).
const LIST_ENTRY_FIXED_BYTES: usize = 63;
/// The most bytes a `PlayerList` packet takes in a batch besides its entries: its length, its
/// header and its entry count (at most 5 each).
const LIST_PACKET_FIXED_BYTES: usize = 15;
/// The most bytes a `PlayerList` packet of one entry takes in a batch: a name and a PlayFab ID
/// together as long as the one identity token that holds both and a skin of [`MAX_SKIN_BYTES`].
/// A spawn window cannot split an entry, so `max-batch-bytes` is at least this (the settings
/// check it).
pub(crate) const MAX_LIST_ENTRY_BYTES: usize = LIST_PACKET_FIXED_BYTES
    + LIST_ENTRY_FIXED_BYTES
    + MAX_TOKEN_PAYLOAD_BYTES
    + XUID_DIGITS
    + MAX_SKIN_BYTES;
/// The color of every entry of the player list (the official server's value, P6).
pub(super) const PLAYER_LIST_COLOR: Rgba = Rgba {
    r: 237,
    g: 237,
    b: 237,
    a: 255,
};
/// The y the official server sends in its first two spawn `Respawn` packets: the top of the build limit, above
/// the eyes and at the feet (P6).
const RESPAWN_SEARCH_Y: f64 = 32_768.0;
/// The official server rewinds 40 ticks of movement history for server-authoritative movement
/// (P6).
const REWIND_HISTORY_TICKS: i32 = 40;
/// The dimension the official server's `SetSpawnPosition` names at join (P6).
const SPAWN_POSITION_DIMENSION: i32 = 3;
/// The block collision shapes the official server sends in `VoxelShapes`:
/// `voxel_shapes.json`, whose counts (287 shapes, 220 names), name order and encoded size (23,178
/// bytes) equal the official server's (P6).
const VOXEL_SHAPES: &str = include_str!("../../xtask/data/reference/voxel_shapes.json");
/// The most digits of a XUID (`u64::MAX`).
const XUID_DIGITS: usize = 20;

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
    /// The batches built with the server's batch settings. Fails if a packet cannot be encoded
    /// or its batch is larger than `max-batch-bytes`.
    ///
    /// With the server's custom content: the custom items after the vanilla ones in the item
    /// registry and the creative inventory, the custom block definitions for `StartGame` and the
    /// custom entity identifiers. `CraftingData` is always empty. Without custom content the
    /// other batches are byte for byte the vanilla ones.
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
                &[Packet::CraftingData(Box::new(packet::CraftingData {
                    clear_recipes: true,
                    ..packet::CraftingData::default()
                }))],
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
            blocks: custom.to_block_entries().into(),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Pending {
    /// `StartGame` and the item registry.
    start_game: bool,
    /// The answer to the chunk radius requests of the tick.
    radius: Option<RadiusAnswer>,
    /// `PlayStatus(PlayerSpawn)`: the spawn chunks were sent.
    pub(super) spawn_status: bool,
    /// The player's creative inventory again: it was switched once the client asked for its
    /// chunk radius (the spawn window has it or is sent this tick).
    pub(super) creative: bool,
}

/// The answer to a chunk radius request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct RadiusAnswer {
    /// The granted radius.
    radius: u8,
    /// The first answer: the spawn position, the creative inventory and `CraftingData` go first.
    first: bool,
}

/// What the spawn windows of a tick share: the player list and each world's clock packets. Built
/// only when a player needs a window this tick.
#[derive(Debug, Default)]
struct Window {
    /// The list of every placed player, in session order, built once for every window: it holds
    /// each player's skin (up to 4 MiB). A client takes at most
    /// [`MAX_PLAYER_LIST_ENTRIES`] entries in one packet and a session sends at most
    /// `max-batch-bytes` in one batch, so a longer or larger list goes in several.
    player_list: Vec<Arc<SharedPacket>>,
    /// The packets of each world a window is in. Only those worlds.
    worlds: BTreeMap<WorldIndex, WorldPackets>,
}

impl Window {
    /// What the spawn windows of `marked` share; empty when none goes out this tick.
    fn new(state: &State, marked: &[Marked<'_>]) -> Self {
        let mut worlds = BTreeMap::new();
        for Marked { player: p, .. } in marked {
            if (p.pending.start_game || p.pending.radius.is_some_and(|r| r.first))
                && !worlds.contains_key(&p.world)
            {
                let world = super::world_api::world_at(state, p.world);
                worlds.insert(
                    p.world,
                    WorldPackets {
                        clock_state: survival::clock_state(world),
                        clock_definition: survival::clock_definition(world),
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

    /// The player list of the window, shared.
    fn player_list(&self) -> impl Iterator<Item = Message> + '_ {
        self.player_list
            .iter()
            .map(|p| Message::SpawnList(Arc::clone(p)))
    }

    /// The packets of `world`, if a window this tick is in it.
    fn world(&self, world: WorldIndex) -> Option<&WorldPackets> {
        self.worlds.get(&world)
    }
}

/// The packets of one world the spawn windows of a tick send.
#[derive(Debug)]
struct WorldPackets {
    /// The clock state.
    clock_state: Packet,
    /// The clock definition.
    clock_definition: Packet,
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

checkpoint::fields!(Loading {
    session,
    let_through,
});

/// What the [`PlayerLogin`] listeners decided for an admitted login, kept from the login until
/// the join or the leave ([`State::logins`]): which of Anchor's refusals ([`command::refusals`],
/// in their order) they let through, and the skin they set in place of the client's.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Admission {
    let_through: [bool; 3],
    skin: Option<queue::Skin>,
}

impl Admission {
    /// Hashes every field (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self { let_through, skin } = self;
        for &let_through in let_through {
            h.u64(u64::from(let_through));
        }
        match skin {
            Some(skin) => skin.hash_into(h),
            None => h.u64(u64::MAX),
        }
    }

    /// Whether the listeners decided anything the join needs.
    fn is_kept(&self) -> bool {
        self.let_through.contains(&true) || self.skin.is_some()
    }
}

checkpoint::fields!(Admission { let_through, skin });

impl super::Context<'_> {
    /// Sets the chunk radius `player` sees, kept from 1 to
    /// the `view-distance` of its world: its view grows or shrinks at once, and its client is
    /// told the radius at the end of the tick. A world the player goes to later grants the
    /// radius set here up to its own `view-distance`. The client asks again when its own
    /// setting changes, and that request wins. A direct call: it raises no
    /// [`PlayerViewDistanceChange`].
    ///
    /// [`Player::view_distance`](crate::player::Player::view_distance) reads the radius set.
    ///
    /// # Errors
    ///
    /// [`player::Error::Offline`] for a player who is not in the game or whose client has not
    /// asked for a radius yet (the first answer opens the spawn window); nothing is changed.
    #[doc(alias = "Player::setViewDistance")]
    pub fn set_view_distance(&mut self, player: player::Id, distance: u8) -> player::Result<()> {
        let session = player.session();
        let mut p = self
            .state
            .players
            .remove(&session)
            .ok_or(player::Error::Offline)?;
        let asked = distance.max(1);
        let radius = granted_radius(&self.state.settings, p.world, asked);
        let ready = p.phase != Phase::Loading && p.chunk_radius.is_some();
        if ready {
            p.asked_radius = Some(asked);
            grant_radius(self.state, &mut p, radius);
        }
        self.state.players.insert(session, p);
        if ready {
            Ok(())
        } else {
            Err(player::Error::Offline)
        }
    }
}

/// Reads the voxel shapes of [`VOXEL_SHAPES`], names in the file's order.
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

/// Builds the armor trim patterns and materials the official server sends, from
/// [`item::TRIM_PATTERNS`] and [`item::TRIM_MATERIAL_TABLE`].
fn trim_data() -> TrimData {
    TrimData {
        patterns: item::TRIM_PATTERNS
            .iter()
            .map(|p| TrimPattern {
                item_name: format!("minecraft:{p}_armor_trim_smithing_template"),
                pattern_id: (*p).to_owned(),
            })
            .collect(),
        materials: item::TRIM_MATERIAL_TABLE
            .iter()
            .map(|&(id, color, item)| TrimMaterial {
                material_id: id.to_owned(),
                colour: color.to_owned(),
                item_name: item.to_owned(),
            })
            .collect(),
    }
}

/// Encodes and compresses packets, in order, into their batch bodies.
fn encode(encoder: &mut batch::Encoder, packets: &[Packet]) -> Result<SharedBatches, batch::Error> {
    let mut bodies = Vec::new();
    batch::encode_bodies(packets, Some(encoder), &mut Vec::new(), &mut bodies)?;
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

/// Builds one player's packets of the tick, in the order the official server sends them (P6): the
/// start-game packets; for the first chunk radius answer the spawn window (spawn position, clocks,
/// difficulty, settings, weather, abilities, rules, player list, biomes, entity types, fog, camera
/// presets, attributes, creative inventory, trims, inventory, `CraftingData`, commands, respawn, entity
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

/// Puts in `out` the packets the official server sends between the chunk radius request and the chunks (P6),
/// with the player's `AvailableCommands`, but for the spawn sound Anchor never sends
/// (deferred.txt). The official server sends the entity data and the structure feature with its first chunks; the chunk packets
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
    let abilities = survival::update_abilities(player);
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
        (RESPAWN_SEARCH_Y + EYE_OFFSET, 0),
        (RESPAWN_SEARCH_Y, 0),
        (eye.y, 1),
    ] {
        out.push(packet(Packet::Respawn(Respawn {
            position: Vec3::new(eye.x, y, eye.z).to_f32(),
            state,
            entity_runtime_id: 0,
        })));
    }
    out.push(packet(survival::actor_data(
        player.runtime_id,
        survival::full_metadata(player),
        0,
    )));
    // The structure the player is in: none, as Anchor generates no vanilla structures.
    out.push(packet(Packet::CurrentStructureFeature(
        CurrentStructureFeature {
            current_feature: String::new(),
        },
    )));
    out.push(packet(attributes));
}

/// Builds the `PlayerList` packets that list `players` in order: each of at most
/// [`MAX_PLAYER_LIST_ENTRIES`] entries and of at most `max_batch_bytes` in a batch. One
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
        if entries.len() == MAX_PLAYER_LIST_ENTRIES
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

/// A `PlayerList` of the player's own entry alone: how its own client is sent its skin, as the
/// lists before its spawn and a `PlayerSkin` of its own player leave the client drawing the skin
/// it sent (seen on the client; whether this entry changes that is open: `xtask/data/p6/facts.md`).
pub(super) fn own_list(player: &Player) -> Packet {
    Packet::PlayerList(PlayerList {
        entries: vec![list_entry(player)],
    })
}

/// Builds a player's entry in the player list (P6): its skin ([`queue::Skin`]) and the device
/// the client sent at login. The PlayFab ID is the identity token's (gophertunnel: the
/// server-verified one; the official server's value is not confirmed).
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
        // The official server sends the login's device.
        build_platform: player.identity.device_os,
        skin: Arc::clone(player.identity.skin.shared()),
        teacher: false,
        host: false,
        sub_client: false,
        player_colour: PLAYER_LIST_COLOR,
    }
}

/// Whether `player` is sent the changes of the player list: it is placed, and its own window,
/// which lists every placed player, is not still to go out.
fn gets_list_changes(player: &Player) -> bool {
    player.phase != Phase::Loading && !player.pending.start_game
}

/// Stops what a player that left (`State::remove`) was doing and takes it out of the other
/// players' lists, if they were told of it.
pub(super) fn left(state: &mut State, player: &mut Player) {
    survival::left(player);
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

/// Admits or refuses the verified login of `session` before its client gets anything of the
/// resource packs: [`PlayerLogin`] decides ([`decide_login`]), and an admitted session is told
/// so through `outbound` and offers the packs, a refused one gets its last packet. What the
/// listeners decided is kept for the join ([`State::logins`]).
pub(super) fn login(
    state: &mut State,
    session: SessionId,
    identity: &Identity,
    outbound: mpsc::Sender<Outbound>,
    output: &mut Output,
) {
    let (admitted, event) = decide_login(state, identity);
    match admitted {
        Err(Refused { reason, packet }) => {
            info!(session = session.get(), player = %identity.name, %reason, "login rejected");
            output.close(outbound, packet);
        }
        Ok(admission) => {
            if admission.is_kept() {
                state.logins.insert(session, admission);
            }
            output.admit(outbound);
        }
    }
    if let Some(e) = &event {
        dispatch::watch(state, e);
    }
}

/// Registers the player of `session`, whose login was admitted, or rejects it when a refusal its
/// login did not let through applies now (the server filled, the whitelist or a ban changed
/// while the client downloaded the packs). A second join of the same session changes nothing.
/// Returns the load of the player's saved data, if the tick does not hold it; the player waits
/// in [`Phase::Loading`] until it arrives.
///
/// When the same player is already online, [`PlayerDuplicateLogin`] decides which of the two is
/// disconnected. Cancelled, the connecting player goes at once. Otherwise the connecting player
/// is checked first (where the slot it would take over is free, so a full server still lets a
/// player take over its own place), and the existing player is disconnected only once the
/// connecting one is admitted: a join that is refused leaves the existing player online.
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
    // Absent when the login's listeners decided nothing the join needs.
    let Admission { let_through, skin } = state.logins.remove(&session).unwrap_or_default();
    let chosen = skin.is_some();
    if let Some(skin) = skin {
        identity.skin = skin;
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
                player = %identity.name,
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
    let request = if let Some(refusal) = refusal_since_login(state, &identity, let_through) {
        let reason = refusal.to_message();
        info!(session = session.get(), player = %identity.name, %reason, "join rejected");
        output.close(outbound, refusal.packet());
        None
    } else {
        if let Some((existing, reason, packet)) = replaced {
            replace(state, existing, session, &reason, packet, output);
        }
        let loading = Loading {
            session,
            let_through,
        };
        let request = enter(state, loading, identity, key, outbound, output);
        if chosen && let Some(player) = state.players.get_mut(&session) {
            survival::skin_chosen(player);
        }
        request
    };
    if let Some(e) = &duplicate {
        dispatch::watch(state, e);
    }
    request
}

/// The first of Anchor's refusals ([`command::refusals`]) of `identity` now that its login did
/// not let through (`let_through`, in their order).
fn refusal_since_login(
    state: &State,
    identity: &Identity,
    let_through: [bool; 3],
) -> Option<Refusal> {
    command::refusals(state, identity)
        .into_iter()
        .zip(let_through)
        .find_map(|(refusal, let_through)| refusal.filter(|_| !let_through))
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
            player = %old.identity.name,
            "player replaced by a new login"
        );
    }
    state.send_off(
        existing,
        QuitReason::Replaced(reason.to_owned()),
        super::player::LEFT.to_owned(),
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
    info!(session = session.get(), player = %identity.name, runtime_id, "player joined");
    let stored = storage_key(&key);
    let mut player = Player {
        session,
        runtime_id,
        identity,
        key,
        // Where a loading player waits: a world the server has, as every player's world is
        // (the checkpoint's restore checks it); `place` moves it on.
        world: state.catalog.default_world(),
        position: Vec3::ZERO,
        data_dirty: false,
        chunk_radius: None,
        asked_radius: None,
        phase: Phase::Loading,
        view: view::View::default(),
        outbound,
        pending: Pending::default(),
        extra: extra::Player::default(),
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
        place(state, &mut player, Some(Arc::unwrap_or_clone(offline.data)));
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
        inventory::load(
            &mut player.inventory,
            player.extra.raw(),
            &state.custom,
            &player.identity.name,
        );
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
    state.set_world(player, world);
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
    // asked again: the player holds its slot since the join.
    let [_, whitelist, ban] = let_through;
    let since_login = refusal_since_login(state, &player.identity, [true, whitelist, ban]);
    let data = match data {
        PlayerLoad::Found(data) => Some(data),
        PlayerLoad::Absent => None,
        PlayerLoad::Damaged => {
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
        let reason = refusal.to_message();
        info!(session = session.get(), player = %player.identity.name, %reason, "join rejected after loading");
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
    let (identity, mut outcome) = event.into_outcome();
    outcome.message = bounded(outcome.message);
    outcome.reason = bounded(outcome.reason);
    (identity, outcome, Some(watched))
}

/// Decides whether `identity` may join (Anchor raises the event here, in the tick, before the
/// session offers the resource packs): Anchor's refusals ([`command::refusals`]) are the event's
/// kick flags, listeners may clear them or set others, and the first flag left refuses the
/// player with its screen message. Without listeners the first refusal decides
/// ([`command::admit`]) and no event is built. Returns what the listeners decided for an
/// admitted login ([`Admission`]: the refusals they cleared, the skin they set) or the refusal,
/// and the event for the monitors, who see it after the login is decided.
fn decide_login(
    state: &mut State,
    identity: &Identity,
) -> (Result<Admission, Refused>, Option<PlayerLogin>) {
    if !dispatch::heard::<PlayerLogin>(state) {
        let admitted = command::admit(state, identity)
            .map(|()| Admission::default())
            .map_err(|r| Refused {
                reason: r.to_message(),
                packet: r.packet(),
            });
        return (admitted, None);
    }
    let mut anchor = command::refusals(state, identity);
    let mut event = PlayerLogin {
        info: PlayerInfo(identity.clone()),
        ip: identity.address,
        port: identity.client.port,
        kicks: Default::default(),
    };
    for r in anchor.iter().flatten() {
        event.set_kick_flag(kick_flag(r), r.to_message(), None);
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
                event.set_kick_flag(kick_flag(&r), r.to_message(), None);
                *was = Some(r);
            }
        }
    }
    let refused = event.kick_flags().next().map(|flag| {
        let reason = bounded(event.kick_reason(flag).unwrap_or_default().to_owned());
        let screen = bounded(
            event
                .kick_screen_message(flag)
                .unwrap_or_default()
                .to_owned(),
        );
        // Anchor's own refusal, unchanged, ends as without listeners (a full server's
        // `PlayStatus` lets the client show its own text).
        let own = anchor
            .iter()
            .flatten()
            .find(|r| kick_flag(r) == flag)
            .filter(|r| r.to_message() == reason && r.to_message() == screen);
        let packet = own.map_or_else(
            || {
                Packet::Disconnect(Disconnect {
                    reason: i32::from(DisconnectReason::NotAllowed),
                    hide_disconnection_screen: false,
                    message: screen.clone(),
                    filtered_message: String::new(),
                })
            },
            Refusal::packet,
        );
        Refused { reason, packet }
    });
    let admitted = if let Some(refused) = refused {
        Err(refused)
    } else {
        let set = &event.info.0.skin;
        Ok(Admission {
            let_through: anchor.map(|r| r.is_some()),
            // The client's skin is the identity's own; a listener's is a new one.
            skin: (!Arc::ptr_eq(set.shared(), identity.skin.shared())).then(|| set.clone()),
        })
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

/// Cuts a listener's disconnect text to [`MAX_TEXT_BYTES`] bytes, at a character boundary.
fn bounded(mut text: String) -> String {
    text.truncate(text.floor_char_boundary(MAX_TEXT_BYTES));
    text
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
/// inventory and `CraftingData`). Requests in one tick share one answer and one resize with the last
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
            survival::spawned(player);
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

/// Announces that the player of `session` spawned: [`PlayerJoin`]
/// may change the join message, which every other player in the game then sees (the official
/// server sends it to the others only). An empty message shows nothing; a player a listener kicked
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
        params: vec![name],
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
            debug!(
                session = session.get(),
                error = &e as &(dyn std::error::Error + 'static),
                "message to the others not sent"
            );
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
    // The official server sends the default type (the world's mode) for a player without its own mode (P6).
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
        // What the official server sends for a flat world (P6).
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
        // The official server sends "*" (P6): any base game version.
        base_game_version: "*".to_owned(),
        world_name: settings.motd.clone(),
        player_movement_settings: PlayerMovementSettings {
            rewind_history_size: REWIND_HISTORY_TICKS,
            server_authoritative_block_breaking: true,
        },
        server_authoritative_inventory: true,
        server_authoritative_sound: true,
        server_join_information: Some(ServerJoinInformation::default()),
        game_version: packet::GAME_VERSION.to_owned(),
        use_block_network_id_hashes: true,
        // The custom block definitions; none without custom blocks.
        blocks: shared.blocks.to_vec(),
        // The world's time, so the day cycle starts where it is.
        time: world.time_ticks(),
        day_cycle_lock_time: survival::day_cycle_lock_time(settings, world),
        rain_level,
        lightning_level,
        // Education features stay off: the Education items are never
        // offered.
        ..StartGame::default()
    }
}

#[cfg(test)]
mod tests {
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
        use crate::player::{self, Id, MAX_TEXT_BYTES};
        use crate::protocol::consts::TextType;
        use crate::protocol::packet::Packet;
        use crate::tick::queue::{Identity, Outbound};
        use crate::tick::spawn::JOINED_KEY;
        use crate::tick::tests::{
            identity as numbered, initialised, input, join, leave, new_state, radius, received,
            settings, tick,
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

        /// The login and the join of session `s` as `identity`, as its session sends them, with
        /// one queue for both.
        fn log_in_and_join(
            s: u64,
            identity: Identity,
        ) -> (Vec<crate::tick::queue::Input>, mpsc::Receiver<Outbound>) {
            let (outbound, rx) = crate::tick::queue::outbound_queue();
            let login = InputKind::Login {
                identity: identity.clone(),
                outbound: outbound.clone(),
            };
            let join = InputKind::Join { identity, outbound };
            (vec![input(s, 0, login), input(s, 1, join)], rx)
        }

        /// Logs session `s` in and joins it, opens a view of radius 2 and confirms the spawn;
        /// its queue.
        fn spawn(state: &mut State, s: u64) -> mpsc::Receiver<Outbound> {
            let (mut entry, rx) = log_in_and_join(s, numbered(s));
            let mut output = Output::default();
            tick(state, &mut entry, &mut output);
            tick(state, &mut vec![radius(s, 2, 2)], &mut output);
            let runtime_id = state.players[&SessionId::new(s)].runtime_id;
            tick(state, &mut vec![initialised(s, 3, runtime_id)], &mut output);
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

        fn log_in_as(
            s: u64,
            identity: Identity,
        ) -> (crate::tick::queue::Input, mpsc::Receiver<Outbound>) {
            let (outbound, rx) = crate::tick::queue::outbound_queue();
            (input(s, 0, InputKind::Login { identity, outbound }), rx)
        }

        fn flags(e: &PlayerLogin) -> String {
            format!("{:?}", e.kick_flags().collect::<Vec<_>>())
        }

        // api: PlayerLoginEvent, PlayerPreLoginEvent
        #[test]
        fn login_listeners_get_every_refusal_and_decide_the_join() {
            let listener = |e: &mut PlayerLogin, _: &mut Context<'_>| {
                log(format!(
                    "{} {:?} {} {}",
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
            let (login, mut rx) = log_in_as(2, identity("Banned", address));
            run(&mut state, vec![login]);
            // Clearing the full flag does not let in a player the ban refuses.
            assert_eq!(
                logged(),
                [
                    "player1 None 0 []",
                    "Banned Some(192.0.2.7) 0 [ServerFull, Banned]"
                ]
            );
            let Some(Packet::Disconnect(d)) = received(&mut rx).1 else {
                panic!("a refused join ends with a Disconnect");
            };
            assert_eq!(d.message, "This server has banned you");
            assert_eq!(state.players.len(), 1);
            Context { state: &mut state }.unban("banned");
            let (entry, mut rx) = log_in_and_join(3, identity("Banned", address));
            run(&mut state, entry);
            assert_eq!(logged(), ["Banned Some(192.0.2.7) 0 [ServerFull]"]);
            assert!(
                received(&mut rx).1.is_none(),
                "a listener lets a full server take one more"
            );
            assert_eq!(state.players.len(), 2);
        }

        // api: PlayerPreLoginEvent::getIp, PlayerPreLoginEvent::getPort
        #[test]
        fn the_login_event_has_the_address_and_port_the_client_connected_from() {
            let listener = |e: &mut PlayerLogin, _: &mut Context<'_>| {
                log(format!("{:?} {}", e.ip(), e.port()));
            };
            let mut state = state(|_| {}, on(listener));
            let mut who = identity("Ported", Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7))));
            who.client.port = 19_133;
            let (login, _rx) = log_in_as(1, who);
            run(&mut state, vec![login]);
            assert_eq!(logged(), ["Some(192.0.2.7) 19133"]);
        }

        // api: PlayerPreLoginEvent
        #[test]
        fn anchors_own_refusal_left_set_ends_as_without_listeners() {
            for listeners in [
                Listeners::default(),
                on(|_: &mut PlayerLogin, _: &mut Context<'_>| {}),
            ] {
                let mut state = state(|c| c.general.max_players = 1, listeners);
                let _first = spawn(&mut state, 1);
                let (login, mut rx) = log_in_as(2, numbered(2));
                run(&mut state, vec![login]);
                let Some(Packet::PlayStatus(s)) = received(&mut rx).1 else {
                    panic!("a full server answers with PlayStatus");
                };
                assert_eq!(
                    s.status,
                    i32::from(crate::protocol::consts::PlayStatus::LoginFailedServerFull)
                );
            }
        }

        // api: PlayerPreLoginEvent
        #[test]
        fn a_plugin_kick_flag_refuses_the_join_and_changes_nothing() {
            let refuse = |e: &mut PlayerLogin, _: &mut Context<'_>| {
                e.set_kick_flag(
                    KickFlag::Listener,
                    "closed",
                    Some("Come back later".to_owned()),
                );
            };
            let mut refused = state(|_| {}, on(refuse));
            let mut idle = state(|_| {}, on(refuse));
            let (login, mut rx) = log_in_as(1, numbered(1));
            run(&mut refused, vec![login]);
            run(&mut idle, Vec::new());
            assert_eq!(refused.hash(), idle.hash());
            let Some(Packet::Disconnect(d)) = received(&mut rx).1 else {
                panic!("a refused join ends with a Disconnect");
            };
            assert_eq!(d.message, "Come back later");
            assert!(refused.players.is_empty());
        }

        // api: PlayerPreLoginEvent
        #[test]
        fn a_login_listener_that_bans_the_player_refuses_it() {
            let ban = |e: &mut PlayerLogin, cx: &mut Context<'_>| {
                cx.ban(BanEntry::new(e.info().name()).with_reason("spam"))
                    .unwrap();
            };
            let mut state = state(|_| {}, on(ban));
            let (login, mut rx) = log_in_as(1, numbered(1));
            run(&mut state, vec![login]);
            let Some(Packet::Disconnect(d)) = received(&mut rx).1 else {
                panic!("a refused join ends with a Disconnect");
            };
            assert_eq!(d.message, "This server has banned you: spam");
            assert!(state.players.is_empty());
        }

        /// Logs session 1 in and joins it as `name` on `state`, which loads saved player data:
        /// the player waits for its data, whose answer `output` holds for the next tick. Returns
        /// its queue.
        fn join_loading(
            state: &mut State,
            name: &str,
            output: &mut Output,
        ) -> mpsc::Receiver<Outbound> {
            state.saving.players = true;
            let (mut entry, rx) = log_in_and_join(1, identity(name, None));
            tick(state, &mut entry, output);
            assert_eq!(state.players[&SessionId::new(1)].phase, Phase::Loading);
            rx
        }

        /// A ban or a whitelist change made while a player's saved data loads, which the kicks of
        /// the commands and calls that make them do not reach, refuses the player when the data
        /// arrives.
        #[test]
        fn a_ban_or_whitelist_change_while_the_data_loads_refuses_the_join() {
            // The whitelist sends no message: the client shows its own text.
            for (banned, message) in [(true, "This server has banned you"), (false, "")] {
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
                    panic!("banned {banned}: the loaded player is refused with a Disconnect");
                };
                assert_eq!(d.message, message);
                assert!(state.players.is_empty(), "banned {banned}");
                assert!(state.loading.is_empty(), "banned {banned}");
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

        /// Takes the tick's answer to a login from `rx`: whether it was admitted, and the last
        /// packet if the session was closed.
        fn answer(rx: &mut mpsc::Receiver<Outbound>) -> (bool, Option<Packet>) {
            let (mut admitted, mut close) = (false, None);
            while let Ok(out) = rx.try_recv() {
                match out {
                    Outbound::Admitted => admitted = true,
                    Outbound::Send(_) => panic!("a login gets no packets"),
                    Outbound::Close(packet) => close = Some(packet),
                }
            }
            (admitted, close)
        }

        /// A login the whitelist or a ban keeps out is closed at once, before its session offers
        /// the resource packs: the whitelist with no message, so that the client shows its own
        /// text in the player's language; nothing of it stays.
        #[test]
        fn a_login_the_whitelist_or_a_ban_keeps_out_is_closed_before_the_packs() {
            for (banned, message) in [(false, ""), (true, "This server has banned you")] {
                let mut state = state(|_| {}, Listeners::default());
                let mut cx = Context { state: &mut state };
                if banned {
                    cx.ban(BanEntry::new("player1")).unwrap();
                } else {
                    cx.set_whitelist_enabled(true);
                }
                let (login, mut rx) = log_in_as(1, numbered(1));
                run(&mut state, vec![login]);
                let (admitted, close) = answer(&mut rx);
                assert!(!admitted, "banned {banned}");
                let Some(Packet::Disconnect(d)) = close else {
                    panic!("banned {banned}: a refused login ends with a Disconnect");
                };
                assert_eq!((d.reason, d.message.as_str()), (45, message));
                assert!(state.players.is_empty() && state.logins.is_empty());
            }
        }

        /// An admitted login is told so and holds nothing until the join; a whitelist change or
        /// a ban made while its client downloads the packs refuses the join, as it would have
        /// refused the login.
        #[test]
        fn a_whitelist_change_or_a_ban_after_the_login_refuses_the_join() {
            for (banned, message) in [(false, ""), (true, "This server has banned you")] {
                let mut state = state(|_| {}, Listeners::default());
                let (mut entry, mut rx) = log_in_and_join(1, numbered(1));
                let join = entry.pop().unwrap();
                run(&mut state, entry);
                assert_eq!(answer(&mut rx), (true, None), "banned {banned}");
                assert!(state.players.is_empty() && state.logins.is_empty());
                let mut cx = Context { state: &mut state };
                if banned {
                    cx.ban(BanEntry::new("player1")).unwrap();
                } else {
                    cx.set_whitelist_enabled(true);
                }
                run(&mut state, vec![join]);
                let Some(Packet::Disconnect(d)) = received(&mut rx).1 else {
                    panic!("banned {banned}: a refused join ends with a Disconnect");
                };
                assert_eq!((d.reason, d.message.as_str()), (45, message));
                assert!(state.players.is_empty(), "banned {banned}");
            }
        }

        /// A server that filled while an admitted client downloaded the packs refuses its join
        /// as full.
        #[test]
        fn a_server_full_since_the_login_refuses_the_join() {
            let mut state = state(|c| c.general.max_players = 1, Listeners::default());
            let (mut entry, mut rx) = log_in_and_join(2, numbered(2));
            let join = entry.pop().unwrap();
            run(&mut state, entry);
            let _first = spawn(&mut state, 1);
            run(&mut state, vec![join]);
            assert!(matches!(
                received(&mut rx).1,
                Some(Packet::PlayStatus(s))
                    if s.status
                        == i32::from(crate::protocol::consts::PlayStatus::LoginFailedServerFull)
            ));
            assert_eq!(state.players.len(), 1);
        }

        /// A refusal a login listener cleared is kept until the join, which it lets in, and is
        /// forgotten there or when the session leaves before joining.
        #[test]
        fn a_refusal_a_login_listener_cleared_holds_until_the_join_or_the_leave() {
            let let_in = |e: &mut PlayerLogin, _: &mut Context<'_>| {
                e.clear_kick_flag(KickFlag::ServerWhitelisted);
            };
            let mut state = state(|_| {}, on(let_in));
            Context { state: &mut state }.set_whitelist_enabled(true);
            let (mut entry, mut rx) = log_in_and_join(1, numbered(1));
            let join = entry.pop().unwrap();
            run(&mut state, entry);
            assert_eq!(answer(&mut rx), (true, None));
            assert_eq!(state.logins.len(), 1);
            run(&mut state, vec![join]);
            assert!(state.logins.is_empty());
            assert_eq!(state.players[&SessionId::new(1)].phase, Phase::Joined);
            assert!(received(&mut rx).1.is_none(), "the player stays");
            let (login, _rx2) = log_in_as(2, numbered(2));
            run(&mut state, vec![login]);
            assert_eq!(state.logins.len(), 1);
            run(&mut state, vec![leave(2, 1)]);
            assert!(state.logins.is_empty());
        }

        /// The skin IDs of the player list entries for `uuid` that `rx` got, packet by packet.
        fn listed_skins(
            rx: &mut mpsc::Receiver<Outbound>,
            uuid: crate::protocol::Uuid,
        ) -> Vec<Vec<String>> {
            received(rx)
                .0
                .into_iter()
                .filter_map(|p| match p {
                    Packet::PlayerList(l) => Some(
                        l.entries
                            .into_iter()
                            .filter(|e| e.uuid == uuid)
                            .map(|e| e.skin.skin_id.clone())
                            .collect::<Vec<_>>(),
                    ),
                    Packet::PlayerSkin(s) => panic!("a skin packet for {:?}", s.uuid),
                    _ => None,
                })
                .filter(|ids| !ids.is_empty())
                .collect()
        }

        // api: PlayerInfo::getSkin
        /// A skin a login listener sets is the only one any client gets for the player: the
        /// players already in get it in the list, and the player in both lists before its spawn
        /// and in its own entry once it spawns.
        #[test]
        fn a_skin_a_login_listener_sets_is_the_one_every_client_gets() {
            let choose = |e: &mut PlayerLogin, _: &mut Context<'_>| {
                if e.info().name() == "player2" {
                    let mut skin = e.info().skin().clone();
                    skin.skin_id = "chosen".to_owned();
                    e.set_skin(skin).unwrap();
                    log(e.info().skin().skin_id.clone());
                }
            };
            let mut state = state(|_| {}, on(choose));
            let mut first = spawn(&mut state, 1);
            received(&mut first);
            let mut second = spawn(&mut state, 2);
            assert_eq!(logged(), ["chosen"]);
            assert!(state.logins.is_empty());
            let uuid = numbered(2).uuid;
            assert_eq!(state.players[&SessionId::new(2)].skin().skin_id, "chosen");
            assert_eq!(listed_skins(&mut first, uuid), [["chosen"]]);
            assert_eq!(
                listed_skins(&mut second, uuid),
                [["chosen"], ["chosen"], ["chosen"]]
            );
        }

        /// A checkpoint between the login and the join keeps the skin a login listener set.
        #[test]
        fn a_checkpoint_keeps_the_skin_a_login_listener_set() {
            let choose = |e: &mut PlayerLogin, _: &mut Context<'_>| {
                let mut skin = e.info().skin().clone();
                skin.skin_id = "chosen".to_owned();
                e.set_skin(skin).unwrap();
            };
            let mut state = state(|_| {}, on(choose));
            let (login, _rx) = log_in_as(1, numbered(1));
            run(&mut state, vec![login]);
            assert_eq!(state.logins.len(), 1);
            let pool = crate::runtime::cpu_pool(1).unwrap();
            let fresh = new_state(settings(|_| {}), on(choose));
            assert!(crate::tick::checkpoint::tests::assert_round_trip(
                &state, fresh, &pool
            ));
            assert!(pool.shutdown());
        }

        /// A skin a login would refuse is refused and leaves the skin, and nothing is kept for the
        /// join: the player's own client gets no entry of its own after the spawn.
        #[test]
        fn a_login_listener_skin_a_login_would_refuse_changes_nothing() {
            let refused = |e: &mut PlayerLogin, _: &mut Context<'_>| {
                let before = e.info().skin().clone();
                let mut skin = before.clone();
                skin.skin_data.pop();
                log(format!("{:?}", e.set_skin(skin)));
                assert_eq!(e.info().skin(), &before);
            };
            let mut state = state(|_| {}, on(refused));
            let (login, _rx) = log_in_as(1, numbered(1));
            run(&mut state, vec![login]);
            assert_eq!(logged(), ["Err(OutOfRange)"]);
            assert!(state.logins.is_empty());
            let mut rx = spawn(&mut state, 2);
            assert_eq!(listed_skins(&mut rx, numbered(2).uuid).len(), 2);
        }

        // api: PlayerPreLoginEvent
        #[test]
        fn login_monitors_see_the_login_before_the_player_joins() {
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
            let (login, _rx) = log_in_as(1, numbered(1));
            run(&mut state, vec![login]);
            assert_eq!(logged(), ["player1 true 0"]);
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

        // api: PlayerJoinEvent
        #[test]
        fn the_join_message_goes_to_the_others_as_the_official_server_sends_it() {
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

        // api: PlayerJoinEvent
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

        fn join_as(
            s: u64,
            identity: Identity,
        ) -> (crate::tick::queue::Input, mpsc::Receiver<Outbound>) {
            let (outbound, rx) = crate::tick::queue::outbound_queue();
            (input(s, 0, InputKind::Join { identity, outbound }), rx)
        }

        // api: PlayerDuplicateLoginEvent
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

        // api: PlayerDuplicateLoginEvent::setDisconnectReason, PlayerQuitEvent::getQuitReason
        #[test]
        fn the_replaced_player_quits_with_the_reason_a_listener_gave() {
            for (screen, shown) in [(None, "moved"), (Some("bye"), "bye")] {
                let mut listeners = on(move |e: &mut PlayerDuplicateLogin, _: &mut Context<'_>| {
                    e.set_kick_reason("moved");
                    e.set_kick_screen_message(screen.map(str::to_owned));
                });
                listeners.push(
                    Priority::Normal,
                    |e: &mut PlayerQuit, _: &mut Context<'_>| {
                        log(e.reason().to_string());
                    },
                );
                let mut state = state(|_| {}, listeners);
                let (a, mut rx1) = join_as(1, identity("Alex", None));
                run(&mut state, vec![a]);
                run(&mut state, Vec::new());
                let (b, _rx2) = join_as(2, identity("alex", None));
                run(&mut state, vec![b]);
                assert_eq!(logged(), ["replaced: moved"], "{screen:?}");
                let (_, close, _) = received(&mut rx1);
                assert!(
                    matches!(close, Some(Packet::Disconnect(d)) if d.message == shown),
                    "{screen:?}"
                );
            }
        }

        // api: PlayerDuplicateLoginEvent::setDisconnectScreenMessage
        #[test]
        fn a_duplicate_login_message_is_cut_to_the_text_limit() {
            let listeners = on(|e: &mut PlayerDuplicateLogin, _: &mut Context<'_>| {
                e.set_kick_screen_message(Some("\u{e9}".repeat(MAX_TEXT_BYTES)));
            });
            let mut state = state(|_| {}, listeners);
            let (a, mut rx1) = join_as(1, identity("Alex", None));
            run(&mut state, vec![a]);
            run(&mut state, Vec::new());
            let (b, _rx2) = join_as(2, identity("alex", None));
            run(&mut state, vec![b]);
            let (_, close, _) = received(&mut rx1);
            let Some(Packet::Disconnect(d)) = close else {
                panic!("the replaced player got no disconnect");
            };
            assert_eq!(d.message.len(), MAX_TEXT_BYTES);
        }

        // api: PlayerPreLoginEvent::setKickFlag
        #[test]
        fn a_login_refusal_message_is_cut_to_the_text_limit() {
            let listeners = on(|e: &mut PlayerLogin, _: &mut Context<'_>| {
                e.set_kick_flag(KickFlag::Listener, "x".repeat(MAX_TEXT_BYTES + 1), None);
            });
            let mut state = state(|_| {}, listeners);
            let (a, mut rx) = log_in_as(1, identity("Alex", None));
            run(&mut state, vec![a]);
            let (_, close, _) = received(&mut rx);
            let Some(Packet::Disconnect(d)) = close else {
                panic!("the refused player got no disconnect");
            };
            assert_eq!(d.message.len(), MAX_TEXT_BYTES);
        }

        // api: PlayerViewDistanceChangeEvent
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

        // api: Player::setViewDistance
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
                Err(player::Error::Offline),
                "its client has not asked"
            );
            assert_eq!(
                cx.set_view_distance(Id::new(SessionId::new(9)), 4),
                Err(player::Error::Offline)
            );
            assert_eq!(cx.set_view_distance(one, 99), Ok(()));
            assert_eq!(
                cx.player(one).unwrap().view_distance(),
                Some(6),
                "at most view-distance"
            );
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
            assert_eq!(cx.set_view_distance(one, 0), Ok(()));
            assert_eq!(
                state.players[&SessionId::new(1)].view.radius(),
                1,
                "at least 1"
            );
            run(&mut state, Vec::new());
            assert_eq!(told(&mut rx), [1]);
            assert!(logged().is_empty(), "a direct call raises no event");
        }
    }

    use super::*;

    use crate::event::Listeners;
    use crate::tick::queue::InputKind;
    use crate::tick::tests::{join, leave, new_state, radius, received, settings, tick};

    /// Reads the data copy, when the external sources are there (`.cache`).
    fn cache(name: &str) -> Option<Vec<u8>> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".cache/reference-data")
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
        packet.encode(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn the_entity_types_are_the_bytes_the_official_server_sends() {
        assert_eq!(
            ENTITY_IDENTIFIERS.len(),
            8971,
            "the 1.26.60 file is 8,971 bytes"
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
    fn voxel_shapes_match_the_official_counts_order_and_size() {
        if let Some(copy) = cache("voxel_shapes.json") {
            assert_eq!(VOXEL_SHAPES.as_bytes(), copy.as_slice());
        }
        let shapes = voxel_shapes();
        assert_eq!((shapes.shapes.len(), shapes.name_map.len()), (287, 220));
        // The first names of the official server's, in its order (P6 capture).
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
        assert_eq!(
            bytes.len(),
            23_178,
            "the official server's VoxelShapes payload (P6)"
        );
        let mut budget = usize::MAX;
        assert_eq!(
            Packet::decode(packet.id(), &bytes, &mut budget).unwrap(),
            packet
        );
    }

    #[test]
    fn trim_data_has_the_official_patterns_and_materials() {
        let trims = trim_data();
        assert_eq!((trims.patterns.len(), trims.materials.len()), (18, 11));
        assert_eq!(
            trims.patterns[0].item_name,
            "minecraft:ward_armor_trim_smithing_template"
        );
        assert_eq!(trims.materials[10].colour, "\u{a7}v");
        // The official server: 1,271 bytes with the two-byte header of packet 302 (P6).
        assert_eq!(payload(&Packet::TrimData(trims)).len(), 1269);
    }

    /// The trim tables are the `TrimData` the P6 run recorded, in its order.
    #[test]
    fn trim_data_equals_the_official_record() {
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
        assert_eq!(patterns, list("18 patterns ("));
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
        assert_eq!(materials, list("11 materials ("));
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
        let count = MAX_PLAYER_LIST_ENTRIES + 1;
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
            [MAX_PLAYER_LIST_ENTRIES, 1, MAX_PLAYER_LIST_ENTRIES, 1]
        );
    }

    /// A player list goes in packets of at most the batch bytes a session sends, an entry alone
    /// whatever its size.
    #[test]
    fn a_player_list_larger_than_a_batch_goes_in_several_packets() {
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
}
