//! The values of an exported chunk, keyed as the import's `chunk` module reads them: the chunk
//! version, `Data3D` (the height map, then the biomes of each sub-chunk), a value per sub-chunk
//! that has blocks, the block entities and the finalization state. Their layouts are those the official server
//! 1.26.52 writes (`xtask/data/import/flat`): [`CHUNK_VERSION`], [`SUB_CHUNK_VERSION`], block
//! layers with a width byte of `bits << 1` (disk palettes of NBT block states), biome storages
//! with `bits << 1 | 1` and [`SAME_BIOMES`] for one the same as the storage below, and
//! [`FINISHED`]. The height map is left at zero; the official server loads such chunks and sends their blocks as
//! stored (`cargo xtask p6 import`).

use crate::import::chunk::{
    CHUNK_TAG_BLOCK_ENTITIES, CHUNK_TAG_SUB_CHUNK, CHUNK_TAG_VERSION, prefix,
};
use crate::nbt::{Compound, Encoding};
use crate::protocol::{Result, Writer, length};
use crate::storage::codec::{Biomes, BlockLayer, ChunkRecord};
use crate::world::{ChunkPos, MIN_Y};

/// The chunk version of the layout written.
const CHUNK_VERSION: u8 = 42;
/// The key tag of a chunk's height map and biomes, which the import does not read.
const DATA_3D: u8 = 0x2b;
/// The key tag of a chunk's finalization state, which the import does not read.
const FINALIZED_STATE: u8 = 0x36;
/// The finalization state of a chunk whose generation is finished: the game does not populate
/// it again.
const FINISHED: i32 = 2;
/// The height map at the start of `Data3D`: 256 `i16`.
const HEIGHT_MAP_BYTES: usize = 512;
/// A biome storage header that repeats the storage below.
const SAME_BIOMES: u8 = 0xff;
/// The sub-chunk version of the layout written.
const SUB_CHUNK_VERSION: u8 = 9;

/// Returns the values of the chunk at `pos` with the terrain `record` and the block entities
/// `block_entities`, sorted by key.
pub(super) fn values(
    pos: ChunkPos,
    record: &ChunkRecord,
    block_entities: &[&Compound],
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let key = |tag: u8, more: &[u8]| {
        let mut key = prefix(pos.x, pos.z).to_vec();
        key.push(tag);
        key.extend_from_slice(more);
        key
    };
    let mut values = vec![
        (key(CHUNK_TAG_VERSION, &[]), vec![CHUNK_VERSION]),
        (key(DATA_3D, &[]), data_3d(record)?),
        (key(FINALIZED_STATE, &[]), FINISHED.to_le_bytes().to_vec()),
    ];
    for (sub, y) in record.sub_chunks.iter().zip((MIN_Y >> 4)..) {
        if sub.layers.is_empty() {
            continue;
        }
        // The sub-chunk y as a signed byte.
        let [y, ..] = y.to_le_bytes();
        let mut w = Writer::new(Vec::new(), 0);
        w.uint8(SUB_CHUNK_VERSION);
        w.uint8(length(sub.layers.len())?);
        w.uint8(y);
        for layer in &sub.layers {
            write_layer(&mut w, layer)?;
        }
        values.push((key(CHUNK_TAG_SUB_CHUNK, &[y]), w.into_inner()));
    }
    if !block_entities.is_empty() {
        let mut w = Writer::new(Vec::new(), 0);
        for compound in block_entities {
            w.nbt(compound, Encoding::LittleEndian)?;
        }
        values.push((key(CHUNK_TAG_BLOCK_ENTITIES, &[]), w.into_inner()));
    }
    values.sort_unstable();
    Ok(values)
}

/// Returns a zero height map, then the biome storage of each sub-chunk from the lowest up.
fn data_3d(record: &ChunkRecord) -> Result<Vec<u8>> {
    let mut w = Writer::new(vec![0; HEIGHT_MAP_BYTES], 0);
    let mut below: Option<&Biomes> = None;
    for sub in record.sub_chunks.iter() {
        let biomes = &sub.biomes;
        if below == Some(biomes) {
            w.uint8(SAME_BIOMES);
            continue;
        }
        w.uint8(biomes.bits << 1 | 1);
        write_indices(&mut w, biomes.bits, &biomes.words, biomes.palette.len())?;
        for &id in &biomes.palette {
            w.uint32(id);
        }
        below = Some(biomes);
    }
    Ok(w.into_inner())
}

/// Writes a block layer: its width byte, indices and palette of NBT block states.
fn write_layer(w: &mut Writer, layer: &BlockLayer) -> Result<()> {
    w.uint8(layer.bits << 1);
    write_indices(w, layer.bits, &layer.words, layer.palette.len())?;
    for state in &layer.palette {
        w.nbt(state, Encoding::LittleEndian)?;
    }
    Ok(())
}

/// Writes the packed index words and, unless the width is 0 (one entry), the palette length.
fn write_indices(w: &mut Writer, bits: u8, words: &[u32], palette_len: usize) -> Result<()> {
    for &word in words {
        w.uint32(word);
    }
    if bits != 0 {
        w.uint32(length(palette_len)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::import::Db;
    use crate::protocol::{Rng, word_count};
    use crate::storage::codec::tests::{empty_chunk, random_chunk};

    /// Reads the biome storages of a `Data3D` value after its height map, one per sub-chunk.
    fn read_biomes(mut bytes: &[u8]) -> Vec<Biomes> {
        let u32_at = |bytes: &mut &[u8]| {
            let (head, rest) = bytes.split_first_chunk::<4>().unwrap();
            *bytes = rest;
            u32::from_le_bytes(*head)
        };
        let mut out: Vec<Biomes> = Vec::new();
        while let Some((&header, rest)) = bytes.split_first() {
            bytes = rest;
            if header == SAME_BIOMES {
                out.push(out.last().unwrap().clone());
                continue;
            }
            assert_eq!(header & 1, 1);
            let bits = header >> 1;
            let words = (0..word_count(bits)).map(|_| u32_at(&mut bytes)).collect();
            let len = if bits == 0 { 1 } else { u32_at(&mut bytes) };
            let palette = (0..len).map(|_| u32_at(&mut bytes)).collect();
            out.push(Biomes {
                bits,
                words,
                palette,
            });
        }
        out
    }

    #[test]
    fn plains_biomes_are_the_bytes_the_official_server_writes() {
        let mut db = Db::open(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/xtask/data/import/flat"
        ))
        .unwrap();
        let mut key = prefix(0, 0).to_vec();
        key.push(DATA_3D);
        let official = db.get(&key).unwrap().unwrap();
        let ours = data_3d(&empty_chunk()).unwrap();
        assert_eq!(
            ours.get(HEIGHT_MAP_BYTES..),
            official.get(HEIGHT_MAP_BYTES..)
        );
        assert!(
            ours.get(..HEIGHT_MAP_BYTES)
                .unwrap()
                .iter()
                .all(|&b| b == 0)
        );
    }

    #[test]
    fn biomes_read_back_per_sub_chunk() {
        let mut rng = Rng(0xb1_0e5);
        for _ in 0..20 {
            let mut chunk = random_chunk(&mut rng);
            // Runs of storages the same as the one below.
            for i in 1..chunk.sub_chunks.len() {
                if rng.below(3) == 0 {
                    chunk.sub_chunks[i].biomes = chunk.sub_chunks[i - 1].biomes.clone();
                }
            }
            let bytes = data_3d(&chunk).unwrap();
            let read = read_biomes(bytes.get(HEIGHT_MAP_BYTES..).unwrap());
            let written: Vec<Biomes> = chunk.sub_chunks.iter().map(|s| s.biomes.clone()).collect();
            assert_eq!(read, written);
        }
    }
}
