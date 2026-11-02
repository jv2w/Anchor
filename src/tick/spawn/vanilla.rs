//! The vanilla data packets of the spawn window that are the same for every server and player:
//! the jigsaw structure data, the entity property definitions and the camera presets, built
//! once at start from the committed data the official server 1.26.52 sends (P6).

use serde::Deserialize;

use crate::nbt;
use crate::protocol::packet::{
    CameraAimAssistPresets, CameraPresets, CameraSpline, JigsawStructureData, Packet,
    SyncActorProperty,
};
use crate::protocol::{
    CameraAimAssistCategory, CameraAimAssistItemSettings, CameraAimAssistPreset,
    CameraAimAssistPriorities, CameraAimAssistPriority, CameraPreset, Reader,
};

/// The official server's `CameraPresets` and `CameraAimAssistPresets` from a P6 capture (the file names its
/// source); no pinned data source holds them.
const CAMERA: &str = include_str!("../../../xtask/data/reference/camera.json");

/// `entity_properties.nbt` as network NBT (`cargo xtask dump`): one root per
/// entity type in the file's order, the order and `PropertyData` of the official server's 13
/// `SyncActorProperty` packets (P6).
const ENTITY_PROPERTIES: &[u8] =
    include_bytes!("../../../xtask/data/reference/entity_properties.dat");

/// The number of entity types in [`ENTITY_PROPERTIES`].
const ENTITY_PROPERTY_TYPES: usize = 13;

/// `jigsaw_structure_data.nbt` as network NBT (`cargo xtask dump`), sent as it
/// is; the official server 1.26.52 sent that `StructureData` (P6 `bot worlddata`). Without it the
/// official client leaves the join with "disconnectionScreen.missingStructureData".
const JIGSAW_STRUCTURE_DATA: &[u8] =
    include_bytes!("../../../xtask/data/reference/jigsaw_structure_data.dat");

/// `camera.json`; its `source` is for readers only.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CameraFile {
    camera_presets: PresetsJson,
    camera_aim_assist_presets: AimAssistJson,
}

/// `CameraPresets` with the preset fields the official server sets; another field fails the parse.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct PresetsJson {
    presets: Vec<PresetJson>,
}

/// One camera preset.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct PresetJson {
    name: String,
    parent: String,
    pos_x: Option<f32>,
    pos_y: Option<f32>,
    pos_z: Option<f32>,
    rot_x: Option<f32>,
    rot_y: Option<f32>,
    view_offset: Option<[f32; 2]>,
    entity_offset: Option<[f32; 3]>,
    radius: Option<f32>,
    apply_inherited_starting_rotation: bool,
}

/// `CameraAimAssistPresets`.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct AimAssistJson {
    categories: Vec<CategoryJson>,
    presets: Vec<AimPresetJson>,
    operation: u8,
}

/// One aim assist category.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct CategoryJson {
    name: String,
    priorities: PrioritiesJson,
}

/// The priorities of an aim assist category.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct PrioritiesJson {
    entities: Vec<PriorityJson>,
    blocks: Vec<PriorityJson>,
    block_tags: Vec<PriorityJson>,
    entity_type_families: Vec<PriorityJson>,
    entity_default: Option<i32>,
    block_default: Option<i32>,
}

/// One aim assist priority.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct PriorityJson {
    identifier: String,
    priority: i32,
}

/// One aim assist preset.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct AimPresetJson {
    identifier: String,
    block_exclusions: Vec<String>,
    entity_exclusions: Vec<String>,
    block_tag_exclusions: Vec<String>,
    entity_type_family_exclusions: Vec<String>,
    liquid_targets: Vec<String>,
    item_settings: Vec<ItemSettingsJson>,
    default_item_settings: Option<String>,
    hand_settings: Option<String>,
}

/// The aim assist category of one item.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct ItemSettingsJson {
    item: String,
    category: String,
}

/// Decodes `JigsawStructureData` with [`JIGSAW_STRUCTURE_DATA`].
pub(super) fn jigsaw_structure_data() -> Packet {
    let id = Packet::JigsawStructureData(JigsawStructureData::default()).id();
    let mut budget = usize::MAX;
    #[expect(
        clippy::expect_used,
        reason = "invariant: the committed file decodes (the jigsaw test reads it)"
    )]
    Packet::decode(id, JIGSAW_STRUCTURE_DATA, &mut budget, 0)
        .expect("jigsaw_structure_data.dat decodes")
}

/// Decodes one `SyncActorProperty` per entity type of [`ENTITY_PROPERTIES`], in the file's order.
pub(super) fn actor_properties() -> Vec<Packet> {
    let mut r = Reader::new(ENTITY_PROPERTIES, usize::MAX, 0);
    let mut out = Vec::with_capacity(ENTITY_PROPERTY_TYPES);
    while !r.remaining().is_empty() {
        #[expect(
            clippy::expect_used,
            reason = "invariant: the committed file decodes (the property test reads it)"
        )]
        let property_data = r
            .nbt(nbt::Encoding::NetworkLittleEndian)
            .expect("entity_properties.dat decodes");
        out.push(Packet::SyncActorProperty(SyncActorProperty {
            property_data,
        }));
    }
    out
}

/// Builds the camera packets in the official server's order: the presets, the aim assist presets and an empty
/// spline list.
pub(super) fn camera() -> [Packet; 3] {
    #[expect(
        clippy::expect_used,
        reason = "invariant: the committed file parses (the camera test reads it)"
    )]
    let file: CameraFile = serde_json::from_str(CAMERA).expect("camera.json parses");
    let presets = file
        .camera_presets
        .presets
        .into_iter()
        .map(|p| CameraPreset {
            name: p.name,
            parent: p.parent,
            pos_x: p.pos_x,
            pos_y: p.pos_y,
            pos_z: p.pos_z,
            rot_x: p.rot_x,
            rot_y: p.rot_y,
            view_offset: p.view_offset,
            entity_offset: p.entity_offset,
            radius: p.radius,
            apply_inherited_starting_rotation: p.apply_inherited_starting_rotation,
            ..CameraPreset::default()
        })
        .collect();
    let aim = file.camera_aim_assist_presets;
    let priorities = |list: Vec<PriorityJson>| {
        list.into_iter()
            .map(|p| CameraAimAssistPriority {
                identifier: p.identifier,
                priority: p.priority,
            })
            .collect()
    };
    let categories = aim
        .categories
        .into_iter()
        .map(|c| CameraAimAssistCategory {
            name: c.name,
            priorities: CameraAimAssistPriorities {
                entities: priorities(c.priorities.entities),
                blocks: priorities(c.priorities.blocks),
                block_tags: priorities(c.priorities.block_tags),
                entity_type_families: priorities(c.priorities.entity_type_families),
                entity_default: c.priorities.entity_default,
                block_default: c.priorities.block_default,
            },
        })
        .collect();
    let aim_presets = aim
        .presets
        .into_iter()
        .map(|p| CameraAimAssistPreset {
            identifier: p.identifier,
            block_exclusions: p.block_exclusions,
            entity_exclusions: p.entity_exclusions,
            block_tag_exclusions: p.block_tag_exclusions,
            entity_type_family_exclusions: p.entity_type_family_exclusions,
            liquid_targets: p.liquid_targets,
            item_settings: p
                .item_settings
                .into_iter()
                .map(|s| CameraAimAssistItemSettings {
                    item: s.item,
                    category: s.category,
                })
                .collect(),
            default_item_settings: p.default_item_settings,
            hand_settings: p.hand_settings,
        })
        .collect();
    [
        Packet::CameraPresets(CameraPresets { presets }),
        Packet::CameraAimAssistPresets(CameraAimAssistPresets {
            categories,
            presets: aim_presets,
            operation: aim.operation,
        }),
        Packet::CameraSpline(CameraSpline {
            splines: Vec::new(),
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tick::spawn::tests::payload;

    #[test]
    fn the_jigsaw_structure_data_is_the_committed_data_at_the_official_size() {
        let packet = jigsaw_structure_data();
        let Packet::JigsawStructureData(data) = &packet else {
            panic!("a JigsawStructureData");
        };
        for key in ["jigsaws", "processors", "structure_sets", "template_pools"] {
            assert!(data.structure_data.contains_key(key), "{key}");
        }
        // Anchor writes the decoded data back to the same bytes, so it sends the file as it is.
        assert_eq!(payload(&packet), JIGSAW_STRUCTURE_DATA);
        // The 1.26.60 file; with its two-byte ID the packet is 209,537 bytes.
        assert_eq!(JIGSAW_STRUCTURE_DATA.len(), 209_535);
    }

    #[test]
    fn the_actor_properties_are_the_official_types_in_order_and_sizes() {
        let packets = actor_properties();
        let types: Vec<&str> = packets
            .iter()
            .map(|p| match p {
                Packet::SyncActorProperty(s) => match s.property_data.get("type") {
                    Some(nbt::Value::String(t)) => t.as_str(),
                    other => panic!("type {other:?}"),
                },
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            types,
            [
                "minecraft:zombie_nautilus",
                "minecraft:wolf",
                "minecraft:sulfur_cube",
                "minecraft:chicken",
                "minecraft:creaking",
                "minecraft:pig",
                "minecraft:cat",
                "minecraft:egg",
                "minecraft:armadillo",
                "minecraft:happy_ghast",
                "minecraft:copper_golem",
                "minecraft:bee",
                "minecraft:cow",
            ]
        );
        // The official server's packet sizes with their two-byte ID (P6), and the file read back byte for byte.
        let sizes: Vec<usize> = packets.iter().map(|p| payload(p).len() + 2).collect();
        assert_eq!(
            sizes,
            [105, 125, 222, 171, 214, 170, 99, 107, 169, 80, 252, 74, 167]
        );
        assert_eq!(
            packets.iter().flat_map(payload).collect::<Vec<u8>>(),
            ENTITY_PROPERTIES
        );
    }

    #[test]
    fn the_camera_packets_have_the_official_presets_and_sizes() {
        let [presets, aim, spline] = camera();
        let Packet::CameraPresets(p) = &presets else {
            panic!("CameraPresets");
        };
        let names: Vec<&str> = p.presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "minecraft:first_person",
                "minecraft:fixed_boom",
                "minecraft:follow_orbit",
                "minecraft:free",
                "minecraft:third_person",
                "minecraft:third_person_front",
            ]
        );
        assert_eq!(p.presets[2].radius, Some(10.0));
        // The official server's packet sizes with their two-byte IDs (P6).
        let sizes = [&presets, &aim, &spline].map(|p| payload(p).len() + 2);
        assert_eq!(sizes, [339, 1098, 3]);
        for packet in [presets, aim, spline] {
            let bytes = payload(&packet);
            let mut budget = usize::MAX;
            assert_eq!(
                Packet::decode(packet.id(), &bytes, &mut budget, 0).unwrap(),
                packet
            );
        }
    }
}
