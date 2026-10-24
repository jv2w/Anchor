//! The network sub-chunk payload: versions 8 and 9 with layers of paletted block storage
//! holding block runtime IDs (dragonfly `chunk.EncodeSubChunk` with `NetworkEncoding`).
//!
//! Decoding is stricter than dragonfly: only the bit widths Bedrock writes are read, only the
//! runtime palette form (header bit 0 set) is read, a palette may not have more entries than
//! the width can index, and every block index must point into the palette.

use std::mem::size_of;

use super::io::{Codec, Error, Reader, Result, Writer, length};

/// Blocks (and biome cells) in a sub-chunk.
pub(crate) const BLOCKS: usize = 4096;
/// Bits per block index Bedrock writes (dragonfly `sizes`).
pub(crate) const WIDTHS: [u8; 9] = [0, 1, 2, 3, 4, 5, 6, 8, 16];

/// Little-endian 32-bit words that hold `BLOCKS` indices of `bits` bits each, without
/// splitting an index across words.
pub(crate) fn word_count(bits: u8) -> usize {
    if bits == 0 {
        0
    } else {
        BLOCKS.div_ceil(32 / usize::from(bits))
    }
}

/// One layer of 16×16×16 block indices into a palette of block runtime IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PalettedStorage {
    bits: u8,
    words: Vec<u32>,
    palette: Vec<u32>,
}

impl PalettedStorage {
    /// A layer with `palette` and every block at index 0, in the narrowest width that indexes
    /// the palette. `None` if the palette is empty or has more than 65536 entries.
    pub fn new(palette: Vec<u32>) -> Option<Self> {
        if palette.is_empty() {
            return None;
        }
        let bits = WIDTHS
            .into_iter()
            .find(|&bits| palette.len() <= 1 << bits)?;
        Some(Self {
            bits,
            words: vec![0; word_count(bits)],
            palette,
        })
    }

    /// Bits per block index: 0, 1, 2, 3, 4, 5, 6, 8 or 16.
    pub fn bits(&self) -> u8 {
        self.bits
    }

    /// The block runtime IDs the indices point to.
    pub fn palette(&self) -> &[u32] {
        &self.palette
    }

    /// The palette, to change entries in place.
    pub fn palette_mut(&mut self) -> &mut [u32] {
        &mut self.palette
    }

    /// The word and bit shift of block (x, y, z); each coordinate is taken modulo 16.
    fn position(&self, x: u8, y: u8, z: u8) -> (usize, u32) {
        let offset = (usize::from(x & 15) << 8) | (usize::from(z & 15) << 4) | usize::from(y & 15);
        let per_word = 32 / usize::from(self.bits.max(1));
        let shift = (offset % per_word) * usize::from(self.bits);
        (offset / per_word, u32::try_from(shift).unwrap_or(0))
    }

    fn mask(&self) -> u32 {
        (1u32 << self.bits) - 1
    }

    /// The palette index of block (x, y, z); each coordinate is taken modulo 16.
    pub fn index(&self, x: u8, y: u8, z: u8) -> u16 {
        let (word, shift) = self.position(x, y, z);
        let w = self.words.get(word).copied().unwrap_or(0);
        u16::try_from((w >> shift) & self.mask()).unwrap_or(0)
    }

    /// Points block (x, y, z) at palette entry `index`. Returns `false`, changing nothing, if
    /// `index` is not in the palette.
    pub fn set_index(&mut self, x: u8, y: u8, z: u8, index: u16) -> bool {
        if usize::from(index) >= self.palette.len() {
            return false;
        }
        let (word, shift) = self.position(x, y, z);
        let mask = self.mask();
        if let Some(w) = self.words.get_mut(word) {
            *w = (*w & !(mask << shift)) | (u32::from(index) << shift);
        }
        true
    }

    /// Whether every block index points into the palette.
    fn indices_in_palette(&self) -> bool {
        (0..BLOCKS).all(|offset| {
            let [x, z, y] = [offset >> 8, (offset >> 4) & 15, offset & 15]
                .map(|c| u8::try_from(c).unwrap_or(0));
            usize::from(self.index(x, y, z)) < self.palette.len()
        })
    }
}

impl Codec for PalettedStorage {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let header = r.uint8()?;
        if header & 1 == 0 {
            return Err(Error::InvalidValue {
                field: "block storage",
                reason: "persistent palette in a network sub-chunk",
            });
        }
        let bits = header >> 1;
        if !WIDTHS.contains(&bits) {
            return Err(Error::UnknownEnum {
                name: "block storage bits",
                value: bits.into(),
            });
        }
        let count = r.limit(length(word_count(bits))?, usize::MAX, size_of::<u32>())?;
        let mut words = Vec::with_capacity(count);
        for _ in 0..count {
            words.push(r.uint32()?);
        }
        let palette_len = if bits == 0 {
            1
        } else {
            u64::try_from(r.varint32()?).map_err(|_| Error::Size)?
        };
        if palette_len == 0 {
            return Err(Error::InvalidValue {
                field: "block storage palette",
                reason: "empty",
            });
        }
        let palette_len = r.limit(palette_len, 1 << bits, size_of::<u32>())?;
        let mut palette = Vec::with_capacity(palette_len);
        for _ in 0..palette_len {
            palette.push(r.varint32()?.cast_unsigned());
        }
        let storage = Self {
            bits,
            words,
            palette,
        };
        if !storage.indices_in_palette() {
            return Err(Error::InvalidValue {
                field: "block storage",
                reason: "block index outside the palette",
            });
        }
        Ok(storage)
    }

    fn encode(&self, w: &mut Writer) -> Result<()> {
        w.uint8((self.bits << 1) | 1);
        for &word in &self.words {
            w.uint32(word);
        }
        if self.bits != 0 {
            w.varint32(length(self.palette.len())?);
        }
        for &id in &self.palette {
            w.varint32(id.cast_signed());
        }
        Ok(())
    }
}

/// A network sub-chunk: block layers, with the sub-chunk's y index in version 9.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubChunk {
    /// The sub-chunk's y index (version 9), or `None` for version 8.
    pub y_index: Option<i8>,
    /// The block layers; layer 0 holds blocks and layer 1 usually water logging.
    pub layers: Vec<PalettedStorage>,
}

impl Codec for SubChunk {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let version = r.uint8()?;
        if version != 8 && version != 9 {
            return Err(Error::UnknownEnum {
                name: "sub-chunk version",
                value: version.into(),
            });
        }
        let count = r.uint8()?;
        let y_index = if version == 9 { Some(r.int8()?) } else { None };
        let count = r.limit(count.into(), usize::MAX, size_of::<PalettedStorage>())?;
        let mut layers = Vec::with_capacity(count);
        for _ in 0..count {
            layers.push(PalettedStorage::decode(r)?);
        }
        Ok(Self { y_index, layers })
    }

    fn encode(&self, w: &mut Writer) -> Result<()> {
        w.uint8(if self.y_index.is_some() { 9 } else { 8 });
        w.uint8(length(self.layers.len())?);
        if let Some(y) = self.y_index {
            w.int8(y);
        }
        for layer in &self.layers {
            layer.encode(w)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::fuzz_round_trip;

    fn encode(x: &SubChunk) -> Vec<u8> {
        let mut w = Writer::new(Vec::new(), 0);
        x.encode(&mut w).unwrap();
        w.into_inner()
    }

    fn decode(data: &[u8]) -> Result<SubChunk> {
        SubChunk::decode(&mut Reader::new(data, 1 << 20, 0))
    }

    /// A layer of `len` palette entries with block (x, y, z) at index (x + y + z) % len.
    fn patterned(len: u16) -> PalettedStorage {
        let palette = (0..u32::from(len)).map(|i| i * 3 + 1).collect();
        let mut layer = PalettedStorage::new(palette).unwrap();
        for x in 0..16u8 {
            for y in 0..16u8 {
                for z in 0..16u8 {
                    let index = (u16::from(x) + u16::from(y) + u16::from(z)) % len;
                    assert!(layer.set_index(x, y, z, index));
                }
            }
        }
        layer
    }

    #[test]
    fn word_counts_match_dragonfly() {
        let counts = WIDTHS.map(word_count);
        assert_eq!(counts, [0, 128, 256, 410, 512, 683, 820, 1024, 2048]);
    }

    #[test]
    fn every_width_gets_and_sets_each_block() {
        for len in [1, 2, 3, 5, 9, 17, 33, 64, 65, 300] {
            let layer = patterned(len);
            assert_eq!(layer.palette().len(), usize::from(len));
            for x in 0..16u8 {
                for y in 0..16u8 {
                    for z in 0..16u8 {
                        let index = (u16::from(x) + u16::from(y) + u16::from(z)) % len;
                        assert_eq!(layer.index(x, y, z), index);
                    }
                }
            }
        }
        assert_eq!(patterned(3).bits(), 2);
        assert_eq!(patterned(65).bits(), 8);
        assert_eq!(patterned(300).bits(), 16);
        let mut layer = patterned(3);
        assert!(!layer.set_index(0, 0, 0, 3));
        assert!(PalettedStorage::new(Vec::new()).is_none());
        assert!(PalettedStorage::new(vec![0; 65537]).is_none());
    }

    #[test]
    fn a_single_block_layer_matches_dragonfly_bytes() {
        let x = SubChunk {
            y_index: None,
            layers: vec![PalettedStorage::new(vec![5]).unwrap()],
        };
        assert_eq!(encode(&x), [8, 1, 1, 10]);
        assert_eq!(decode(&[8, 1, 1, 10]), Ok(x));
    }

    #[test]
    fn sub_chunks_round_trip() {
        let x = SubChunk {
            y_index: Some(-4),
            layers: (1..=9).map(|n| patterned(n * 37)).collect(),
        };
        let bytes = encode(&x);
        assert_eq!(bytes[..3], [9, 9, 0xfc]);
        let mut r = Reader::new(&bytes, 1 << 20, 0);
        assert_eq!(SubChunk::decode(&mut r), Ok(x));
        assert!(r.remaining().is_empty());
    }

    #[test]
    fn malformed_payloads_are_rejected() {
        assert!(decode(&[1, 1, 1, 10]).is_err(), "version 1");
        assert!(decode(&[8, 1, 0, 10]).is_err(), "persistent palette");
        assert!(decode(&[8, 1, 0xff, 10]).is_err(), "previous-layer marker");
        assert!(decode(&[8, 1, (7 << 1) | 1, 10]).is_err(), "7 bits");
        assert_eq!(decode(&[8, 1, 1, 0x80]), Err(Error::UnexpectedEnd));
        let mut one_bit = vec![8, 1, (1 << 1) | 1];
        one_bit.extend([0; 512]);
        let mut empty = one_bit.clone();
        empty.push(0);
        assert!(decode(&empty).is_err(), "empty palette");
        let mut three = one_bit.clone();
        three.extend([6, 1, 2, 3]);
        assert_eq!(decode(&three), Err(Error::Size), "3 entries for 1 bit");
        let mut outside = vec![8, 1, (1 << 1) | 1, 1];
        outside.extend([0; 511]);
        outside.extend([2, 0]);
        assert!(decode(&outside).is_err(), "index 1 with one palette entry");
        let mut fine = one_bit;
        fine.extend([2, 0]);
        assert!(decode(&fine).is_ok());
        let truncated_words = [8, 1, (16 << 1) | 1, 0, 0];
        assert_eq!(decode(&truncated_words), Err(Error::Size));
        let bytes = encode(&SubChunk {
            y_index: None,
            layers: vec![patterned(300)],
        });
        let mut r = Reader::new(&bytes, 4096, 0);
        assert_eq!(SubChunk::decode(&mut r), Err(Error::Size), "budget");
    }

    #[test]
    fn mutated_input_never_panics() {
        let x = SubChunk {
            y_index: Some(3),
            layers: vec![patterned(5), PalettedStorage::new(vec![0]).unwrap()],
        };
        let write = |w: &mut Writer, x: &SubChunk| x.encode(w);
        assert!(fuzz_round_trip(&encode(&x), 0, SubChunk::decode, write) > 0);
    }
}
