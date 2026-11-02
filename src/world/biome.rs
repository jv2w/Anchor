//! The biome definitions the client gets once in the spawn window.
//!
//! The 89 vanilla biomes the official server sends, generated from stripped definitions into
//! [`table`].

mod table;

use crate::protocol::BiomeDefinition;
use crate::protocol::packet::BiomeDefinitionList;

/// The biome ID the official server sends for every vanilla biome: the client finds a vanilla
/// biome by its name.
const VANILLA_ID: i16 = -1;

/// One biome definition as the official server sends it, its strings as indices into [`table::STRINGS`].
#[derive(Debug)]
struct Definition {
    /// The index of the biome's name.
    name: i16,
    /// The biome's temperature.
    temperature: f32,
    /// The biome's downfall.
    downfall: f32,
    /// The biome's foliage snow value.
    foliage_snow: f32,
    /// The terrain depth.
    depth: f32,
    /// The terrain scale.
    scale: f32,
    /// The water colour on maps, as ARGB.
    map_water_colour: u32,
    /// Whether it rains in the biome.
    rain: bool,
    /// The indices of the biome's tags.
    tags: &'static [u16],
}

/// The `BiomeDefinitionList` packet with every biome of the table.
///
/// Every biome has the vanilla ID and no chunk generation data, like the official server.
pub(crate) fn definition_list() -> BiomeDefinitionList {
    BiomeDefinitionList {
        biome_definitions: table::DEFINITIONS
            .iter()
            .map(|definition| BiomeDefinition {
                name_index: definition.name,
                biome_id: VANILLA_ID,
                temperature: definition.temperature,
                downfall: definition.downfall,
                foliage_snow: definition.foliage_snow,
                depth: definition.depth,
                scale: definition.scale,
                map_water_colour: definition.map_water_colour.cast_signed(),
                rain: definition.rain,
                tags: Some(definition.tags.to_vec()),
                chunk_generation: None,
            })
            .collect(),
        string_list: table::STRINGS
            .iter()
            .map(|&string| string.to_owned())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::Value as Json;

    use crate::protocol::packet::Packet;

    /// The reference list's biome definitions.
    const DATA: &str = include_str!("../../xtask/data/reference/biomes.json");
    /// The biome definition keys that hold a float.
    const FLOATS: [&str; 5] = ["temperature", "downfall", "foliage_snow", "depth", "scale"];
    /// The payload size of the official server 1.26.52's `BiomeDefinitionList`.
    ///
    /// P6 measured 7,668 bytes with the one-byte packet header. It depends only on the content,
    /// not on the order of biomes or strings.
    const OFFICIAL_PAYLOAD_SIZE: usize = 7667;

    #[test]
    fn the_list_round_trips_with_every_biome_and_no_generation_data() {
        let list = definition_list();
        assert_eq!(list.biome_definitions.len(), 89);
        assert_eq!(list.string_list.len(), 187);
        assert!(
            list.biome_definitions
                .iter()
                .all(|d| d.biome_id == VANILLA_ID
                    && d.chunk_generation.is_none()
                    && d.tags.is_some())
        );
        let packet = Packet::BiomeDefinitionList(list);
        let mut bytes = Vec::new();
        packet.encode(&mut bytes, 0).unwrap();
        assert_eq!(bytes.len(), OFFICIAL_PAYLOAD_SIZE);
        let mut budget = usize::MAX;
        let decoded = Packet::decode(packet.id(), &bytes, &mut budget, 0).unwrap();
        assert_eq!(decoded, packet);
    }

    #[test]
    fn each_string_is_listed_once() {
        let list = definition_list();
        let unique: BTreeSet<&String> = list.string_list.iter().collect();
        assert_eq!(unique.len(), list.string_list.len());
    }

    /// A data number at the packet's precision, rounded once from its decimal text.
    fn single(json: &Json) -> f32 {
        json.as_number().unwrap().to_string().parse().unwrap()
    }

    #[test]
    fn every_definition_resolved_by_name_equals_the_data() {
        let list = definition_list();
        let string = |i: usize| list.string_list[i].clone();
        // Name -> (float bits, the other fields as data JSON).
        let built: BTreeMap<String, ([u32; 5], Json)> = list
            .biome_definitions
            .iter()
            .map(|d| {
                let floats = [d.temperature, d.downfall, d.foliage_snow, d.depth, d.scale];
                let c = d.map_water_colour.cast_unsigned().to_be_bytes();
                let tags: Vec<String> = d
                    .tags
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|&t| string(usize::from(t)))
                    .collect();
                let rest = serde_json::json!({
                    "map_water_colour": {"a": c[0], "r": c[1], "g": c[2], "b": c[3]},
                    "rain": d.rain,
                    "tags": tags,
                });
                let name = string(usize::try_from(d.name_index).unwrap());
                (name, (floats.map(f32::to_bits), rest))
            })
            .collect();
        assert_eq!(built.len(), list.biome_definitions.len());

        let data: Vec<Json> = serde_json::from_str(DATA).unwrap();
        assert_eq!(data.len(), built.len());
        for mut want in data {
            let object = want.as_object_mut().unwrap();
            let name = object.remove("name").unwrap();
            let (floats, rest) = &built[name.as_str().unwrap()];
            let want_floats = FLOATS.map(|key| single(&object.remove(key).unwrap()).to_bits());
            assert_eq!(floats, &want_floats, "{name}");
            assert_eq!(rest, &want, "{name}");
        }
    }
}
