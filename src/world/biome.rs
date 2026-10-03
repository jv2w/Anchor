//! The biome definitions the client gets once in the spawn window (PLAN.md 10.6): the 89 vanilla
//! biomes BDS sends, generated from `CloudburstMC`'s stripped definitions into [`table`].

mod table;

use crate::protocol::BiomeDefinition;
use crate::protocol::packet::BiomeDefinitionList;

/// The biome ID BDS sends for every vanilla biome: the client finds a vanilla biome by its name.
const VANILLA_ID: i16 = -1;

/// One biome definition as BDS sends it, its strings as indices into [`table::STRINGS`].
#[derive(Debug)]
struct Definition {
    name: i16,
    temperature: f32,
    downfall: f32,
    foliage_snow: f32,
    depth: f32,
    scale: f32,
    /// ARGB.
    map_water_colour: u32,
    rain: bool,
    tags: &'static [u16],
}

/// The `BiomeDefinitionList` packet: every biome of the table with the vanilla ID and no chunk
/// generation data, like BDS.
pub(crate) fn definition_list() -> BiomeDefinitionList {
    BiomeDefinitionList {
        biome_definitions: table::DEFINITIONS
            .iter()
            .map(|d| BiomeDefinition {
                name_index: d.name,
                biome_id: VANILLA_ID,
                temperature: d.temperature,
                downfall: d.downfall,
                foliage_snow: d.foliage_snow,
                depth: d.depth,
                scale: d.scale,
                map_water_colour: d.map_water_colour.cast_signed(),
                rain: d.rain,
                tags: Some(d.tags.to_vec()),
                chunk_generation: None,
            })
            .collect(),
        string_list: table::STRINGS.iter().map(|&s| s.to_owned()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::Value as Json;

    use super::*;
    use crate::protocol::packet::Packet;

    const DATA: &str = include_str!("../../xtask/data/reference/biomes.json");

    /// The payload size of BDS 1.26.52's `BiomeDefinitionList` (P6: 7,668 bytes with the one-byte
    /// packet header). It depends only on the content, not on the order of biomes or strings.
    const BDS_PAYLOAD_SIZE: usize = 7667;

    /// A data number at the packet's precision, rounded once from its decimal text.
    fn single(json: &Json) -> f32 {
        json.as_number().unwrap().to_string().parse().unwrap()
    }

    #[test]
    fn the_list_round_trips_with_every_biome_and_no_generation_data() {
        let list = definition_list();
        assert_eq!(list.biome_definitions.len(), 89);
        assert_eq!(list.string_list.len(), 187);
        assert!(
            list.biome_definitions
                .iter()
                .all(|d| d.biome_id == -1 && d.chunk_generation.is_none() && d.tags.is_some())
        );
        let packet = Packet::BiomeDefinitionList(list);
        let mut bytes = Vec::new();
        packet.encode(&mut bytes, 0).unwrap();
        assert_eq!(bytes.len(), BDS_PAYLOAD_SIZE);
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

    #[test]
    fn every_definition_resolved_by_name_equals_the_data() {
        const FLOATS: [&str; 5] = ["temperature", "downfall", "foliage_snow", "depth", "scale"];
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
