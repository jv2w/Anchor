//! A sub-chunk's 4-bit light values, two to a byte or one for the whole cube.
//!
//! A uniform cube costs no allocation. A full array is shared until it
//! changes (copy on write), so a checkpoint keeps the light of every loaded chunk without copying
//! it on the tick, for the replay tool.

use std::fmt;
use std::sync::Arc;

use crate::tick::checkpoint::{self, Codec, Error, Reader, Writer};
use crate::world::SLOTS;

/// The bytes of a full array: one cube of values of 4 bits.
const FULL_BYTES: usize = SLOTS as usize / 2;

/// The light values of a 16×16×16 cube, indexed like the block layers (x, then z, then y).
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Nibbles {
    /// Every block has this value.
    Uniform(u8),
    /// One value per block, the low nibble being the even offset.
    ///
    /// Copies share the bytes until one of them changes.
    Full(Arc<[u8; FULL_BYTES]>),
}

impl Nibbles {
    /// The cube of these 4096 values (one byte each, 0 to 15): uniform if they are all the same.
    pub(crate) fn from_values(values: &[u8]) -> Self {
        let first = values.first().copied().unwrap_or(0);
        if values.iter().all(|&value| value == first) {
            return Self::Uniform(first & 15);
        }
        let mut bytes = Arc::new([0; FULL_BYTES]);
        let packed = Arc::make_mut(&mut bytes);
        for (byte, [low, high]) in packed.iter_mut().zip(values.as_chunks::<2>().0) {
            *byte = (low & 15) | (high << 4);
        }
        Self::Full(bytes)
    }

    /// The value at `offset` (0 to 4095).
    pub(crate) fn get(&self, offset: u16) -> u8 {
        match self {
            Self::Uniform(value) => *value,
            Self::Full(bytes) => {
                let bytes: &[u8; FULL_BYTES] = bytes;
                #[expect(
                    clippy::indexing_slicing,
                    reason = "invariant: an offset is below 4096"
                )]
                let byte = bytes[usize::from(offset >> 1)];
                if offset & 1 == 0 {
                    byte & 15
                } else {
                    byte >> 4
                }
            }
        }
    }

    /// The value of a uniform cube.
    pub(crate) fn uniform(&self) -> Option<u8> {
        match self {
            Self::Uniform(value) => Some(*value),
            Self::Full(_) => None,
        }
    }

    /// The heap bytes the cube holds.
    pub(crate) fn heap_bytes(&self) -> usize {
        match self {
            Self::Uniform(_) => 0,
            Self::Full(_) => FULL_BYTES,
        }
    }

    /// Sets the value at `offset` (0 to 4095) to `value` (0 to 15).
    ///
    /// A uniform cube becomes a full array on its first differing value.
    pub(crate) fn set(&mut self, offset: u16, value: u8) {
        let value = value & 15;
        if let Self::Uniform(uniform) = *self {
            if uniform == value {
                return;
            }
            *self = Self::Full(Arc::new([uniform | (uniform << 4); FULL_BYTES]));
        }
        if let Self::Full(bytes) = self {
            #[expect(
                clippy::indexing_slicing,
                reason = "invariant: an offset is below 4096"
            )]
            let byte = &mut Arc::make_mut(bytes)[usize::from(offset >> 1)];
            *byte = if offset & 1 == 0 {
                (*byte & 0xf0) | value
            } else {
                (*byte & 0x0f) | (value << 4)
            };
        }
    }
}

impl fmt::Debug for Nibbles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uniform(value) => write!(f, "Uniform({value})"),
            Self::Full(_) => f.write_str("Full"),
        }
    }
}

/// A uniform cube by its value, a full one by its 2048 bytes.
impl Codec for Nibbles {
    fn encode(&self, w: &mut Writer) {
        match self {
            Self::Uniform(value) => {
                w.u8(0);
                w.u8(*value);
            }
            Self::Full(bytes) => {
                w.u8(1);
                w.raw(&bytes[..]);
            }
        }
    }

    fn decode(r: &mut Reader<'_>) -> checkpoint::Result<Self> {
        match r.u8()? {
            0 => Ok(Self::Uniform(r.u8()? & 15)),
            1 => {
                let bytes: [u8; FULL_BYTES] = r
                    .take(FULL_BYTES)?
                    .try_into()
                    .map_err(|_| Error::Format("light array"))?;
                Ok(Self::Full(Arc::new(bytes)))
            }
            _ => Err(Error::Format("light array kind")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_and_packed_values_match_an_array() {
        let mut nibbles = Nibbles::Uniform(15);
        let mut model = [15_u8; 4096];
        nibbles.set(7, 15);
        assert_eq!(
            nibbles,
            Nibbles::Uniform(15),
            "an equal value keeps it uniform"
        );
        for i in 0..4096_u16 {
            let value = u8::try_from((u32::from(i) * 7 + 3) % 16).unwrap();
            nibbles.set(i, value);
            model[usize::from(i)] = value;
        }
        for i in 0..4096_u16 {
            assert_eq!(nibbles.get(i), model[usize::from(i)]);
        }
        assert!(nibbles.uniform().is_none());
        assert_eq!(nibbles.heap_bytes(), 2048);
        assert_eq!(nibbles, Nibbles::from_values(&model));
        assert_eq!(Nibbles::from_values(&[9; 4096]), Nibbles::Uniform(9));
        assert_eq!(Nibbles::Uniform(9).heap_bytes(), 0);
        nibbles.set(4095, 16);
        assert_eq!(nibbles.get(4095), 0, "values keep 4 bits");
    }
}
