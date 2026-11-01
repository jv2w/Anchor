//! The exported world's `level.dat`, in the layout the import's `level_dat` module reads (a
//! little-endian `i32` storage version, the `i32` length of the rest, one little-endian NBT root
//! compound). It holds the world's state (name, spawn, time), the storage and network versions
//! of this protocol's release, and a flat generator, which makes the chunks the world never
//! saved: BDS's default flat layers for a world of [`Flat`] (the same terrain Anchor generated),
//! a layer of air for any other generator. No setting is written (game mode, difficulty, game
//! rules, seed): the game takes its defaults, and a server its own settings.

use crate::import::level_dat::STORAGE_VERSION;
use crate::nbt::{Compound, Encoding, Value};
use crate::protocol::Writer;
use crate::protocol::packet::PROTOCOL_VERSION;
use crate::storage::codec::WorldRecord;
use crate::world::{Flat, Generator};

/// The `Generator` of a flat world.
const FLAT_GENERATOR: i32 = 2;
/// `FlatWorldLayers` as BDS 1.26.60 writes it for its default flat world: bedrock at y -64,
/// dirt at -63 and -62, grass at -61, plains.
const FLAT_LAYERS: &str = "{\"biome_id\":1,\"block_layers\":[\
    {\"block_name\":\"minecraft:bedrock\",\"count\":1},\
    {\"block_name\":\"minecraft:dirt\",\"count\":2},\
    {\"block_name\":\"minecraft:grass_block\",\"count\":1}],\
    \"encoding_version\":6,\"structure_options\":null,\
    \"world_version\":\"version.post_1_18\"}\n";
/// The same with one layer of air: chunks of nothing but plains.
const AIR_LAYERS: &str = "{\"biome_id\":1,\"block_layers\":[\
    {\"block_name\":\"minecraft:air\",\"count\":1}],\
    \"encoding_version\":6,\"structure_options\":null,\
    \"world_version\":\"version.post_1_18\"}\n";

/// The `level.dat` of the world `name` with the state `record`, made with `generator`.
#[expect(
    clippy::expect_used,
    reason = "invariant: a world name is at most 250 bytes, so the compound is far within the NBT \
              limits and a u32 length"
)]
pub(super) fn encode(name: &str, record: &WorldRecord, generator: &dyn Generator) -> Vec<u8> {
    let layers = if generator.name() == Flat.name() {
        FLAT_LAYERS
    } else {
        AIR_LAYERS
    };
    let root = Compound::from([
        ("StorageVersion".to_owned(), Value::Int(STORAGE_VERSION)),
        ("NetworkVersion".to_owned(), Value::Int(PROTOCOL_VERSION)),
        ("LevelName".to_owned(), Value::String(name.to_owned())),
        ("Generator".to_owned(), Value::Int(FLAT_GENERATOR)),
        (
            "FlatWorldLayers".to_owned(),
            Value::String(layers.to_owned()),
        ),
        ("SpawnX".to_owned(), Value::Int(record.spawn.x)),
        ("SpawnY".to_owned(), Value::Int(record.spawn.y)),
        ("SpawnZ".to_owned(), Value::Int(record.spawn.z)),
        ("Time".to_owned(), Value::Long(record.time)),
    ]);
    let mut w = Writer::new(Vec::new(), 0);
    w.nbt(&root, Encoding::LittleEndian)
        .expect("a small compound encodes");
    let body = w.into_inner();
    let mut out = STORAGE_VERSION.to_le_bytes().to_vec();
    let length = u32::try_from(body.len()).expect("a small compound's length fits a u32");
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Reader;
    use crate::world::{BlockPos, Void};

    /// The root compound of the `level.dat` bytes `bytes`.
    fn root(bytes: &[u8]) -> Compound {
        let (_, body) = bytes.split_first_chunk::<8>().unwrap();
        Reader::new(body, 1 << 20, 0)
            .nbt(Encoding::LittleEndian)
            .unwrap()
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn a_flat_world_has_the_layers_bds_writes_and_any_other_air() {
        // The level.dat BDS 1.26.60 wrote for its P6 flat world (`cargo xtask p6 bds`).
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/xtask/data/export/level.dat");
        let bds = root(&std::fs::read(path).unwrap());
        let record = WorldRecord {
            number: 0,
            time: 0,
            spawn: BlockPos::new(0, -60, 0),
        };
        let flat = root(&encode("w", &record, &Flat));
        for key in [
            "Generator",
            "FlatWorldLayers",
            "StorageVersion",
            "NetworkVersion",
        ] {
            assert_eq!(flat.get(key), bds.get(key), "{key}");
        }
        let void = root(&encode("w", &record, &Void));
        assert_eq!(
            void.get("FlatWorldLayers"),
            Some(&Value::String(AIR_LAYERS.to_owned()))
        );
    }
}
