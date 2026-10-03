//! A sub-chunk's 4-bit light values (PLAN.md 10.7): one value per block, two to a byte, or one
//! value for the whole cube, which costs no allocation (PMMP `LightArray::fill`).

use std::fmt;

/// Bytes of a full array: 4096 values of 4 bits.
const BYTES: usize = 2048;

/// The light values of a 16×16×16 cube, indexed like the block layers (x, then z, then y).
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Nibbles {
    /// Every block has this value.
    Uniform(u8),
    /// One value per block: the low nibble is the even offset.
    Full(Box<[u8; BYTES]>),
}

impl Nibbles {
    /// The value at `offset` (0 to 4095).
    pub(crate) fn get(&self, offset: u16) -> u8 {
        match self {
            Self::Uniform(v) => *v,
            Self::Full(bytes) => {
                let byte = bytes.get(usize::from(offset >> 1)).copied().unwrap_or(0);
                if offset & 1 == 0 {
                    byte & 15
                } else {
                    byte >> 4
                }
            }
        }
    }

    /// Sets the value at `offset` (0 to 4095) to `value` (0 to 15). A uniform cube becomes a full
    /// array on its first differing value.
    pub(crate) fn set(&mut self, offset: u16, value: u8) {
        let value = value & 15;
        if let Self::Uniform(v) = *self {
            if v == value {
                return;
            }
            *self = Self::Full(Box::new([v | (v << 4); BYTES]));
        }
        if let Self::Full(bytes) = self
            && let Some(byte) = bytes.get_mut(usize::from(offset >> 1))
        {
            *byte = if offset & 1 == 0 {
                (*byte & 0xf0) | value
            } else {
                (*byte & 0x0f) | (value << 4)
            };
        }
    }

    /// The value of a uniform cube.
    pub(crate) fn uniform(&self) -> Option<u8> {
        match self {
            Self::Uniform(v) => Some(*v),
            Self::Full(_) => None,
        }
    }

    /// The cube of these 4096 values (one byte each, 0 to 15): uniform if they are all the same.
    pub(crate) fn from_values(values: &[u8]) -> Self {
        let first = values.first().copied().unwrap_or(0);
        if values.iter().all(|&v| v == first) {
            return Self::Uniform(first & 15);
        }
        let mut bytes = Box::new([0; BYTES]);
        for (byte, [low, high]) in bytes.iter_mut().zip(values.as_chunks::<2>().0) {
            *byte = (low & 15) | (high << 4);
        }
        Self::Full(bytes)
    }

    /// The heap bytes the cube holds.
    pub(crate) fn heap_bytes(&self) -> usize {
        match self {
            Self::Uniform(_) => 0,
            Self::Full(_) => BYTES,
        }
    }
}

impl fmt::Debug for Nibbles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uniform(v) => write!(f, "Uniform({v})"),
            Self::Full(_) => f.write_str("Full"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_and_packed_values_match_an_array() {
        let mut n = Nibbles::Uniform(15);
        let mut model = [15u8; 4096];
        n.set(7, 15);
        assert_eq!(n, Nibbles::Uniform(15), "an equal value keeps it uniform");
        for i in 0..4096u16 {
            let v = u8::try_from((u32::from(i) * 7 + 3) % 16).unwrap();
            n.set(i, v);
            model[usize::from(i)] = v;
        }
        for i in 0..4096u16 {
            assert_eq!(n.get(i), model[usize::from(i)]);
        }
        assert!(n.uniform().is_none());
        assert_eq!(n.heap_bytes(), 2048);
        assert_eq!(Nibbles::from_values(&model), n);
        assert_eq!(Nibbles::from_values(&[9; 4096]), Nibbles::Uniform(9));
        assert_eq!(Nibbles::Uniform(9).heap_bytes(), 0);
        n.set(4095, 16);
        assert_eq!(n.get(4095), 0, "values keep 4 bits");
    }
}
