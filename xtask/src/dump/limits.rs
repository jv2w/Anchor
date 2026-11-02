//! The per-field decode limits: every `maxLength`, `maxItems` and
//! `maxProperties` in the JSON schemas of Mojang's bedrock-protocol-docs, joined with the
//! gophertunnel field each one limits, written as [`FILE`] for `gen`. The join is the table
//! below; the numbers come from the schemas. A schema limit without a row, or a row without a
//! schema limit, fails the dump, so a protocol update shows every limit that came or went.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value as Json;

/// Schema limits Anchor applies: schema title, property path (the JSON path without
/// `properties`), and the fields it limits as `<Go type>.<field>`. A field limits a string or
/// list read straight into it or through an `Optional`; `[]` after it limits each element of
/// the list.
const FIELDS: &[(&str, &str, &[&str])] = &[
    (
        "ActorDefinition",
        "Event Name",
        &["protocol.EntityDefinitionTriggerEvent.EventName"],
    ),
    (
        "AddTimeMarkerData",
        "TimeMarkers",
        &["packet.SyncWorldClocks.AddTimeMarkers"],
    ),
    (
        "AttributeLayerData",
        "Attributes",
        &["protocol.AttributeLayerData.EnvironmentAttributes"],
    ),
    (
        "AttributeLayerData",
        "Name",
        &["protocol.AttributeLayerData.Name"],
    ),
    (
        "AudioContentRegistrationEntry",
        "Audio Content ID",
        &["protocol.AudioContentRegistrationEntry.AudioContentID"],
    ),
    ("AuthorAndMessage", "Message", &["packet.Text.Message"]),
    (
        "AuthorAndMessage",
        "Player Name",
        &["packet.Text.SourceName"],
    ),
    (
        "AvailableCommandsPacketChainedSubcommandData",
        "Name",
        &["protocol.ChainedSubcommand.Name"],
    ),
    (
        "AvailableCommandsPacketChainedSubcommandData",
        "SubCommand values",
        &["protocol.ChainedSubcommand.Values"],
    ),
    (
        "AvailableCommandsPacketCommandData",
        "CommandData Chained Subcommand Indexes",
        &["protocol.Command.ChainedSubcommandOffsets"],
    ),
    (
        "AvailableCommandsPacketCommandData",
        "Description",
        &["protocol.Command.Description"],
    ),
    (
        "AvailableCommandsPacketCommandData",
        "Name",
        &["protocol.Command.Name"],
    ),
    (
        "AvailableCommandsPacketCommandData",
        "Overloads",
        &["protocol.Command.Overloads"],
    ),
    // gophertunnel reads the one-byte indices as a byte slice.
    (
        "AvailableCommandsPacketConstrainedValueData",
        "Constraint Indices",
        &["protocol.CommandEnumConstraint.Constraints"],
    ),
    (
        "AvailableCommandsPacketPayload",
        "Chained Subcommand Data",
        &["packet.AvailableCommands.ChainedSubcommands"],
    ),
    (
        "AvailableCommandsPacketSoftEnumData",
        "Enum Name",
        &["protocol.DynamicEnum.Type"],
    ),
    (
        "BookEditAction::AddPage",
        "Page Text",
        &["packet.BookEdit.Text"],
    ),
    (
        "BookEditAction::AddPage",
        "Photo Name",
        &["packet.BookEdit.PhotoName"],
    ),
    (
        "BookEditAction::Finalize",
        "Author",
        &["packet.BookEdit.Author"],
    ),
    (
        "BookEditAction::Finalize",
        "Title",
        &["packet.BookEdit.Title"],
    ),
    (
        "BookEditAction::Finalize",
        "XUID",
        &["packet.BookEdit.XUID"],
    ),
    (
        "BookEditAction::ReplacePage",
        "Page Text",
        &["packet.BookEdit.Text"],
    ),
    (
        "BookEditAction::ReplacePage",
        "Photo Name",
        &["packet.BookEdit.PhotoName"],
    ),
    (
        "BossEventPacketPayload",
        "FilteredName",
        &["packet.BossEvent.FilteredBossBarTitle"],
    ),
    (
        "BossEventPacketPayload",
        "Name",
        &["packet.BossEvent.BossBarTitle"],
    ),
    (
        "CameraInstruction::SplineInstruction",
        "splineIdentifier",
        &["protocol.CameraSplineInstruction.SplineIdentifier"],
    ),
    (
        "ClientCacheBlobStatusPacketPayload",
        "Found Ids",
        &["packet.ClientCacheBlobStatus.HitHashes"],
    ),
    (
        "ClientCacheBlobStatusPacketPayload",
        "Missing Ids",
        &["packet.ClientCacheBlobStatus.MissHashes"],
    ),
    (
        "ClientCacheMissResponsePacketPayload",
        "Missing Blobs",
        &["packet.ClientCacheMissResponse.Blobs"],
    ),
    (
        "ClientboundMapItemDataPacketPayload",
        "Creation Map IDs",
        &["packet.ClientBoundMapItemData.MapsIncludedIn"],
    ),
    (
        "ClientboundMapItemDataPacketPayload",
        "Decorations",
        &["packet.ClientBoundMapItemData.Decorations"],
    ),
    (
        "ClientboundMapItemDataPacketPayload",
        "Pixels",
        &["packet.ClientBoundMapItemData.Pixels"],
    ),
    (
        "ClientboundMapItemDataPacketPayload",
        "Tracked Actor IDs",
        &["packet.ClientBoundMapItemData.TrackedObjects"],
    ),
    (
        "ClientboundMatchmakingStatePacketPayload",
        "Destination Name",
        &["packet.ClientboundMatchmakingState.DestinationName"],
    ),
    (
        "Clientbound Data Driven UI Show Screen Payload",
        "ScreenId",
        &["packet.ClientBoundDataDrivenUIShowScreen.ScreenID"],
    ),
    (
        "Clientbound Data Store Payload",
        "Updates",
        &["packet.ClientBoundDataStore.Updates"],
    ),
    (
        "CodeBuilderRuntimeAction",
        "CodeBuilder Runtime Action",
        &["protocol.CodeBuilderRuntimeActionEvent.Action"],
    ),
    (
        "CodeBuilderScoreboard",
        "Objective Name",
        &["protocol.CodeBuilderScoreboardEvent.ObjectiveName"],
    ),
    (
        "CommandOriginData",
        "RequestId",
        &["protocol.CommandOrigin.RequestID"],
    ),
    (
        "CommandRequestPacketPayload",
        "Command",
        &["packet.CommandRequest.CommandLine"],
    ),
    (
        "Data Store Change",
        "Data Store Name",
        &["protocol.DataStoreChange.DataStoreName"],
    ),
    (
        "Data Store Change",
        "Property",
        &["protocol.DataStoreChange.Property"],
    ),
    (
        "Data Store Removal",
        "Data Store Name",
        &["protocol.DataStoreRemoval.DataStoreName"],
    ),
    // The string alternative of the value.
    (
        "Data Store Update",
        "Data/oneOf/2",
        &["protocol.DataStoreUpdate.StringValue"],
    ),
    (
        "Data Store Update",
        "Data Store Name",
        &["protocol.DataStoreUpdate.DataStoreName"],
    ),
    (
        "Data Store Update",
        "Path",
        &["protocol.DataStoreUpdate.Path"],
    ),
    (
        "Data Store Update",
        "Property",
        &["protocol.DataStoreUpdate.Property"],
    ),
    ("DebugMarkerData", "Text", &["packet.DebugMarkerData.Text"]),
    // The schema's map key is the dimension name, a field in gophertunnel.
    (
        "DimensionDataPacketPayload",
        "Definitions/propertyNames",
        &["protocol.DimensionDefinition.Name"],
    ),
    (
        "DimensionDefinition",
        "Default Biome",
        &["protocol.DimensionDefinition.DefaultBiome"],
    ),
    (
        "EnvironmentAttributeData",
        "AttributeName",
        &["protocol.EnvironmentAttributeData.AttributeName"],
    ),
    (
        "GraphicsParameterOverridePacketPayload",
        "Biome Identifier",
        &["packet.GraphicsOverrideParameter.BiomeIdentifier"],
    ),
    // A map in the schema, a list of key frames in gophertunnel.
    (
        "GraphicsParameterOverridePacketPayload",
        "Parameter Keyframe Values",
        &["packet.GraphicsOverrideParameter.Values"],
    ),
    (
        "GraphicsParameterOverridePacketPayload",
        "Player Identifier",
        &["packet.GraphicsOverrideParameter.PlayerIdentifier"],
    ),
    (
        "InitializeRegistryData",
        "ClockData",
        &["packet.SyncWorldClocks.Clocks"],
    ),
    (
        "ItemEnchantOption",
        "Enchant Name",
        &["protocol.EnchantmentOption.Name"],
    ),
    (
        "ItemStackRequest",
        "Actions",
        &["protocol.ItemStackRequest.Actions"],
    ),
    (
        "ItemStackRequest",
        "Strings To Filter/items",
        &["protocol.ItemStackRequest.FilterStrings[]"],
    ),
    (
        "ItemStackRequestReservedAction",
        "Reserved Id",
        &["protocol.CraftReservedStackRequestAction.ReservedID"],
    ),
    (
        "ItemStackRequestPacketPayload",
        "Requests",
        &["packet.ItemStackRequest.Requests"],
    ),
    // One blob hash per sub-chunk plus the biome blob (gophertunnel: at most 64 sub-chunks).
    (
        "LevelChunkPacketPayload",
        "Cache Metadata",
        &["packet.LevelChunk.BlobHashes"],
    ),
    (
        "LocatorBarPacketPayload",
        "Waypoints",
        &["packet.LocatorBar.Waypoints"],
    ),
    (
        "MapInfoRequestPacketPayload",
        "Client Pixels List",
        &["packet.MapInfoRequest.ClientPixels"],
    ),
    ("MessageAndParams", "Message", &["packet.Text.Message"]),
    (
        "MessageAndParams",
        "Parameter List",
        &["packet.Text.Parameters"],
    ),
    (
        "MatchmakingStateOptions",
        "Triggering Player Name",
        &["protocol.MatchmakingStateOptions.TriggeringPlayerName"],
    ),
    ("MessageOnly", "Message", &["packet.Text.Message"]),
    (
        "MobKilled",
        "Trader Name",
        &["protocol.MobKilledEvent.VillagerDisplayName"],
    ),
    (
        "NetworkChunkPublisherUpdatePacketPayload",
        "Server Built Chunks List",
        &["packet.NetworkChunkPublisherUpdate.SavedChunks"],
    ),
    (
        "NoiseDescriptor",
        "amplitudes",
        &["protocol.NoiseDescriptor.Amplitudes"],
    ),
    (
        "NoiseTransitionSettingsData",
        "ClockName",
        &["protocol.AttributeNoiseTransitionSettings.ClockName"],
    ),
    (
        "NoiseTransitionSettingsData",
        "NoiseName",
        &["protocol.AttributeNoiseTransitionSettings.NoiseName"],
    ),
    (
        "PartyDestinationCookieResponsePacketPayload",
        "cookie",
        &["packet.PartyDestinationCookieResponse.Cookie"],
    ),
    (
        "PhotoTransferPacketPayload",
        "Photo Data",
        &["packet.PhotoTransfer.PhotoData"],
    ),
    (
        "PlayerArmorDamagePacketPayload",
        "Armor Slot and Damage Pairs",
        &["packet.PlayerArmourDamage.List"],
    ),
    (
        "PlayerAuthInputPacketPayload",
        "Player Block Actions",
        &["packet.PlayerAuthInput.BlockActions"],
    ),
    (
        "PlayerEnchantOptionsPacketPayload",
        "Options",
        &["packet.PlayerEnchantOptions.Options"],
    ),
    (
        "PlayerListPacketPayload",
        "Entries",
        &["packet.PlayerList.Entries"],
    ),
    ("PlayerPartyInfo", "party_id", &["packet.PartyInfo.PartyID"]),
    (
        "PrimitiveShapesPacketPayload",
        "Array of primitive shapes (can be a mix of new, updated or removed)",
        &["packet.PrimitiveShapes.Shapes"],
    ),
    (
        "PurchaseReceiptPacketPayload",
        "PurchaseReceipts",
        &["packet.PurchaseReceipt.Receipts"],
    ),
    (
        "RecipeUnlockingRequirement",
        "Unlocking Ingredients",
        &["protocol.RecipeUnlockRequirement.Ingredients"],
    ),
    (
        "RemoveEnvironmentAttributesData",
        "Attribute Layer Name",
        &["packet.ClientBoundAttributeLayerSync.LayerName"],
    ),
    (
        "RemoveEnvironmentAttributesData",
        "Attributes",
        &["packet.ClientBoundAttributeLayerSync.RemoveAttributeNames"],
    ),
    (
        "RemoveEnvironmentAttributesData",
        "Attributes/items",
        &["packet.ClientBoundAttributeLayerSync.RemoveAttributeNames[]"],
    ),
    (
        "RemoveTimeMarkerData",
        "TimeMarkerIds",
        &["packet.SyncWorldClocks.RemoveTimeMarkerIDs"],
    ),
    (
        "Resource Pack Client Response - Downloading",
        "Downloading Packs",
        &["packet.ResourcePackClientResponse.PacksToDownload"],
    ),
    (
        "ResourcePackStackPacketPayload",
        "Texture Pack List",
        &["packet.ResourcePackStack.TexturePacks"],
    ),
    (
        "ResourcePacksInfoPacketPayload",
        "Resource Packs",
        &["packet.ResourcePacksInfo.TexturePacks"],
    ),
    (
        "SendPartyDestinationCookiePacketPayload",
        "cookie",
        &["packet.SendPartyDestinationCookie.Cookie"],
    ),
    (
        "SendPartyDestinationCookiePacketPayload",
        "destination_name",
        &["packet.SendPartyDestinationCookie.DestinationName"],
    ),
    // A hand codec (`src/protocol/values.rs`): `gen` writes the limit as a constant.
    (
        "ServerboundPackSettingChangePacketPayload",
        "PackSettingName",
        &["protocol.PackSetting.Name"],
    ),
    (
        "ServerboundRegisterAudioContentPacketPayload",
        "Registrations",
        &["packet.ServerboundRegisterAudioContent.Registrations"],
    ),
    (
        "ShapedRecipePayload",
        "Ingredients",
        &["protocol.ShapedRecipe.Input"],
    ),
    (
        "ShapelessRecipePayload",
        "Ingredients",
        &["protocol.ShapelessRecipe.Input"],
    ),
    (
        "SignedAudioContent",
        "Compact JWT",
        &[
            "protocol.AudioContentRegistrationEntry.SharedMetadata",
            "protocol.AudioContentRegistrationEntry.ServerContent",
            "protocol.AudioContentRegistrationEntry.PlaybackContent",
            "packet.ClientboundPlayAudioContent.SharedMetadata",
            "packet.ClientboundPlayAudioContent.PlaybackContent",
        ],
    ),
    (
        "SkinImage",
        "ImageBytes",
        &[
            "protocol.Skin.SkinData",
            "protocol.Skin.CapeData",
            "protocol.SkinAnimation.ImageData",
        ],
    ),
    (
        "SlashCommand",
        "Command Name",
        &["protocol.SlashCommandExecutedEvent.CommandName"],
    ),
    (
        "SlashCommand",
        "Error List",
        &["protocol.SlashCommandExecutedEvent.OutputMessages"],
    ),
    (
        "StructureSettings",
        "Structure Palette Name",
        &["protocol.StructureSettings.PaletteName"],
    ),
    (
        "StructureTemplateDataRequestPacketPayload",
        "Structure Name",
        &["packet.StructureTemplateDataRequest.StructureName"],
    ),
    (
        "SubChunkPacketPayload",
        "SubChunk Data",
        &["packet.SubChunk.SubChunkEntries"],
    ),
    (
        "SubChunkRequestPacketPayload",
        "SubChunk Position Offset List",
        &["packet.SubChunkRequest.Offsets"],
    ),
    (
        "SyncStateData",
        "ClockData",
        &["packet.SyncWorldClocks.SyncStates"],
    ),
    (
        "TextPacketPayload",
        "Filtered Message",
        &["packet.Text.FilteredMessage"],
    ),
    (
        "TextPacketPayload",
        "Platform Id",
        &["packet.Text.PlatformChatID"],
    ),
    ("TextPacketPayload", "Sender's XUID", &["packet.Text.XUID"]),
    ("TimeMarkerData", "Name", &["protocol.TimeMarkerData.Name"]),
    (
        "TransitionSettingsData",
        "ClockName",
        &["protocol.AttributeTransitionSettings.ClockName"],
    ),
    (
        "UpdateAttributeLayerSettingsData",
        "Attribute Layer Name",
        &["packet.ClientBoundAttributeLayerSync.LayerName"],
    ),
    (
        "UpdateAttributeLayersData",
        "Attribute Layers",
        &["packet.ClientBoundAttributeLayerSync.Layers"],
    ),
    (
        "UpdateEnvironmentAttributesData",
        "Attribute Layer Name",
        &["packet.ClientBoundAttributeLayerSync.LayerName"],
    ),
    (
        "UpdateEnvironmentAttributesData",
        "Attributes",
        &["packet.ClientBoundAttributeLayerSync.EnvironmentAttributes"],
    ),
    (
        "VoxelShapes::SerializableCells",
        "Storage",
        &["protocol.VoxelCells.Storage"],
    ),
    (
        "VoxelShapes::SerializableVoxelShape",
        "X Coordinates",
        &["protocol.VoxelShape.XCoordinates"],
    ),
    (
        "VoxelShapes::SerializableVoxelShape",
        "Y Coordinates",
        &["protocol.VoxelShape.YCoordinates"],
    ),
    (
        "VoxelShapes::SerializableVoxelShape",
        "Z Coordinates",
        &["protocol.VoxelShape.ZCoordinates"],
    ),
    ("WorldClockData", "Name", &["protocol.WorldClockData.Name"]),
    (
        "WorldClockData",
        "TimeMarkers",
        &["protocol.WorldClockData.TimeMarkers"],
    ),
    (
        "gatheringsConfig",
        "creatorId",
        &["protocol.GatheringJoinInfo.CreatorID"],
    ),
    (
        "gatheringsConfig",
        "experienceName",
        &["protocol.GatheringJoinInfo.ExperienceName"],
    ),
    (
        "gatheringsConfig",
        "scenarioId",
        &["protocol.GatheringJoinInfo.ScenarioID"],
    ),
    (
        "gatheringsConfig",
        "serverId",
        &["protocol.GatheringJoinInfo.ServerID"],
    ),
    (
        "gatheringsConfig",
        "worldName",
        &["protocol.GatheringJoinInfo.ExperienceWorldName"],
    ),
    (
        "presenceConfig",
        "richPresenceId",
        &["protocol.PresenceInfo.RichPresenceID"],
    ),
];
/// The written file in `xtask/data/reference/`: gophertunnel field to its largest length in
/// bytes (strings, byte slices) or elements (lists).
pub(crate) const FILE: &str = "protocol_limits.json";
/// The limit keywords read from the schemas.
const KEYWORDS: &[&str] = &["maxLength", "maxItems", "maxProperties"];

/// Schema limits with no gophertunnel list or string to apply to: title, path and why.
const UNUSED: &[(&str, &str, &str)] = &[
    (
        "ActorDataBoundingBoxComponent",
        "Actor Data Bounding Box",
        "three fixed floats in gophertunnel",
    ),
    (
        "Color255RGB",
        "oneOf/1",
        "the array form of a colour; gophertunnel reads fixed components",
    ),
    (
        "Color255RGBA",
        "oneOf/1",
        "the array form of a colour; gophertunnel reads fixed components",
    ),
    (
        "ColorNormRGB",
        "oneOf/1",
        "the array form of a colour; gophertunnel reads fixed components",
    ),
    (
        "ItemEnchants",
        "ItemEnchants",
        "a fixed array of three lists in gophertunnel",
    ),
    (
        "MovementAttributesComponent",
        "",
        "nine fixed floats in gophertunnel",
    ),
    (
        "SubChunkHeightmapData",
        "Subchunk Height Map",
        "a fixed 16 × 16 array in gophertunnel",
    ),
    (
        "SubChunkHeightmapData",
        "Subchunk Height Map/items",
        "a fixed 16 × 16 array in gophertunnel",
    ),
    (
        "SubChunkHeightmapData",
        "Subchunk Render Height Map",
        "a fixed 16 × 16 array in gophertunnel",
    ),
    (
        "SubChunkHeightmapData",
        "Subchunk Render Height Map/items",
        "a fixed 16 × 16 array in gophertunnel",
    ),
    (
        "TintMapColor",
        "Colors",
        "a fixed array of four colours in gophertunnel",
    ),
];

/// Reads every schema in `json` (the docs' `json/` directory) and returns the text of [`FILE`].
pub(crate) fn generate(json: &Path) -> Result<String, String> {
    let mut found: BTreeMap<(String, String), u64> = BTreeMap::new();
    for path in super::read_dir(json)? {
        let schema: Json = serde_json::from_slice(&super::read(&path)?)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let title = schema
            .get("title")
            .and_then(Json::as_str)
            .ok_or_else(|| format!("{}: no title", path.display()))?
            .to_owned();
        collect(&schema, &title, &mut Vec::new(), &mut found)?;
    }
    let mut limits: BTreeMap<&str, u64> = BTreeMap::new();
    let mut errors = Vec::new();
    for &(title, at, fields) in FIELDS {
        let Some(&max) = found.get(&(title.to_owned(), at.to_owned())) else {
            errors.push(format!("{title} {at:?} has no schema limit"));
            continue;
        };
        for &field in fields {
            if let Some(other) = limits.insert(field, max)
                && other != max
            {
                errors.push(format!("{field} gets the limits {other} and {max}"));
            }
        }
    }
    for &(title, at, _) in UNUSED {
        if !found.contains_key(&(title.to_owned(), at.to_owned())) {
            errors.push(format!("{title} {at:?} has no schema limit"));
        }
    }
    for (title, at) in found.keys() {
        let listed = FIELDS
            .iter()
            .map(|&(t, a, _)| (t, a))
            .chain(UNUSED.iter().map(|&(t, a, _)| (t, a)))
            .any(|(t, a)| t == title && a == at);
        if !listed {
            errors.push(format!("{title} {at:?} is in no row"));
        }
    }
    if !errors.is_empty() {
        return Err(format!("protocol limits:\n{}", errors.join("\n")));
    }
    let mut text = serde_json::to_string_pretty(&limits).map_err(|e| format!("{FILE}: {e}"))?;
    text.push('\n');
    Ok(text)
}

/// Records the limits in `node` at the property path `at` (keys without `properties`).
fn collect(
    node: &Json,
    title: &str,
    at: &mut Vec<String>,
    found: &mut BTreeMap<(String, String), u64>,
) -> Result<(), String> {
    match node {
        Json::Object(map) => {
            let mut limit = None;
            for keyword in KEYWORDS {
                if let Some(v) = map.get(*keyword) {
                    let v = v
                        .as_u64()
                        .ok_or_else(|| format!("{title} {at:?}: {keyword} {v} is not a count"))?;
                    if limit.replace(v).is_some() {
                        return Err(format!("{title} {at:?}: two limit keywords"));
                    }
                }
            }
            if let Some(v) = limit
                && found
                    .insert((title.to_owned(), at.join("/")), v)
                    .is_some_and(|old| old != v)
            {
                return Err(format!("{title} {at:?}: two schemas give different limits"));
            }
            for (key, child) in map {
                if key == "properties" {
                    collect(child, title, at, found)?;
                    continue;
                }
                at.push(key.clone());
                collect(child, title, at, found)?;
                at.pop();
            }
        }
        Json::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                at.push(i.to_string());
                collect(child, title, at, found)?;
                at.pop();
            }
        }
        _ => {}
    }
    Ok(())
}
