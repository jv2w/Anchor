//! Typed flag sets: the flag groups of [`consts`](super::consts) (input flags, entity data
//! flags, abilities, ...) as sets by bit position, with views over the wire types that carry
//! them.

use std::fmt;
use std::hash::Hash;
use std::marker::PhantomData;

use super::common::Bitset;
use super::consts::InputFlag;
use super::io::{Error, Result};
use super::types::InputFlags;

/// The 64-bit words of a [`FlagSet`]: room for 192 bit positions. A group with more fails to
/// compile where a set of it is made (the generated tests make one of every group).
const WORDS: usize = 3;

/// A flag of a protocol flag group, a closed set of bit positions.
pub trait Flag: Copy + Eq + Hash + fmt::Debug + 'static {
    /// One past the highest bit position of the group.
    const COUNT: u32;
    /// The group's name in errors.
    const NAME: &'static str;
    /// The bit position of the flag.
    fn bit(self) -> u32;
    /// The flag at a bit position, if any.
    fn from_bit(bit: u32) -> Option<Self>;
}

/// A set of flags of one group, by bit position: [`contains`](Self::contains) and the updates
/// are O(1).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FlagSet<F> {
    words: [u64; WORDS],
    flag: PhantomData<F>,
}

impl<F: Flag> FlagSet<F> {
    /// The empty set.
    pub const fn new() -> Self {
        const {
            assert!(
                F::COUNT as usize <= WORDS * 64,
                "the flag group does not fit a FlagSet"
            );
        }
        Self {
            words: [0; WORDS],
            flag: PhantomData,
        }
    }

    /// The word index and mask of a flag's bit.
    fn place(flag: F) -> (usize, u64) {
        let bit = flag.bit();
        (bit as usize / 64, 1 << (bit % 64))
    }

    /// Whether the set holds `flag`.
    pub fn contains(&self, flag: F) -> bool {
        let (word, mask) = Self::place(flag);
        self.words.get(word).is_some_and(|w| w & mask != 0)
    }

    /// Adds `flag`.
    pub fn insert(&mut self, flag: F) {
        let (word, mask) = Self::place(flag);
        if let Some(w) = self.words.get_mut(word) {
            *w |= mask;
        }
    }

    /// Removes `flag`.
    pub fn remove(&mut self, flag: F) {
        let (word, mask) = Self::place(flag);
        if let Some(w) = self.words.get_mut(word) {
            *w &= !mask;
        }
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    /// The flags in the set, by ascending bit position.
    pub fn iter(&self) -> impl Iterator<Item = F> + '_ {
        (0..F::COUNT)
            .filter_map(F::from_bit)
            .filter(|&f| self.contains(f))
    }

    /// The set with bit `i` of `words[i / 64]` for each flag at bit position `i`: a mask group's
    /// wire integer is `words[0]`. Fails with [`Error::UnknownEnum`] (the value is the bit
    /// position) on a set bit that is no flag of the group.
    pub fn from_words(words: &[u64]) -> Result<Self> {
        let mut set = Self::new();
        for (i, &word) in words.iter().enumerate() {
            let mut rest = word;
            while rest != 0 {
                let low = rest.trailing_zeros();
                rest &= rest - 1;
                let bit = u32::try_from(i)
                    .ok()
                    .and_then(|i| i.checked_mul(64))
                    .and_then(|base| base.checked_add(low));
                let flag = bit.and_then(F::from_bit).ok_or(Error::UnknownEnum {
                    name: F::NAME,
                    value: i64::try_from(i)
                        .unwrap_or(i64::MAX)
                        .saturating_mul(64)
                        .saturating_add(low.into()),
                })?;
                set.insert(flag);
            }
        }
        Ok(set)
    }

    /// The set as words, the form [`Self::from_words`] reads.
    pub fn words(&self) -> [u64; WORDS] {
        self.words
    }

    /// The set from a [`Bitset`] indexed by bit position. Fails like [`Self::from_words`].
    pub fn from_bitset(bitset: &Bitset) -> Result<Self> {
        let mut set = Self::new();
        for i in (0..bitset.len()).filter(|&i| bitset.get(i)) {
            let flag = u32::try_from(i)
                .ok()
                .and_then(F::from_bit)
                .ok_or(Error::UnknownEnum {
                    name: F::NAME,
                    value: i64::try_from(i).unwrap_or(i64::MAX),
                })?;
            set.insert(flag);
        }
        Ok(set)
    }

    /// The set as a [`Bitset`] of [`Flag::COUNT`] bits (gophertunnel sizes flag bitsets so).
    pub fn to_bitset(self) -> Bitset {
        let mut bitset = Bitset::new(F::COUNT as usize);
        for flag in self.iter() {
            bitset.set(flag.bit() as usize);
        }
        bitset
    }
}

impl<F: Flag> Default for FlagSet<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: Flag> fmt::Debug for FlagSet<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<F: Flag> FromIterator<F> for FlagSet<F> {
    fn from_iter<I: IntoIterator<Item = F>>(iter: I) -> Self {
        let mut set = Self::new();
        for flag in iter {
            set.insert(flag);
        }
        set
    }
}

impl InputFlags {
    /// The flags as a set, for O(1) [`FlagSet::contains`]. Fails with [`Error::UnknownEnum`]
    /// on an ID that is no [`InputFlag`] (decoded packets hold only known IDs).
    pub fn to_set(&self) -> Result<FlagSet<InputFlag>> {
        let mut set = FlagSet::new();
        for &id in &self.ids {
            let flag =
                u32::try_from(id)
                    .ok()
                    .and_then(InputFlag::from_bit)
                    .ok_or(Error::UnknownEnum {
                        name: InputFlag::NAME,
                        value: id.into(),
                    })?;
            set.insert(flag);
        }
        Ok(set)
    }

    /// Input flags holding `set`, sized for every [`InputFlag`] like gophertunnel's
    /// `NewInputFlags`, with the IDs in ascending order.
    pub fn from_set(set: &FlagSet<InputFlag>) -> Self {
        Self {
            set: true,
            size: InputFlag::COUNT.into(),
            ids: set
                .iter()
                .filter_map(|f| i32::try_from(f.bit()).ok())
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::consts::{Ability, EntityDataFlag, MoveFlag};

    #[test]
    fn a_set_holds_what_was_inserted() {
        let mut set = FlagSet::new();
        assert!(set.is_empty());
        set.insert(EntityDataFlag::OnFire);
        set.insert(EntityDataFlag::NotPickableFromInside);
        assert!(set.contains(EntityDataFlag::OnFire));
        assert!(set.contains(EntityDataFlag::NotPickableFromInside));
        assert!(!set.contains(EntityDataFlag::Sneaking));
        set.remove(EntityDataFlag::OnFire);
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            [EntityDataFlag::NotPickableFromInside]
        );
        assert!(EntityDataFlag::NotPickableFromInside.bit() >= 128);
    }

    #[test]
    fn mask_flags_read_and_write_their_wire_integer() {
        let set = FlagSet::<MoveFlag>::from_words(&[0b1001]).unwrap();
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            [MoveFlag::OnGround, MoveFlag::ForceCompletion]
        );
        let [bits, ..] = set.words();
        assert_eq!(bits, 0b1001);
        let abilities: FlagSet<Ability> = [Ability::Build, Ability::Flying].into_iter().collect();
        let [bits, ..] = abilities.words();
        assert_eq!(
            bits,
            u64::from(u32::from(Ability::Build) | u32::from(Ability::Flying))
        );
    }

    #[test]
    fn unknown_bits_are_errors() {
        assert_eq!(
            FlagSet::<MoveFlag>::from_words(&[1 << 4]),
            Err(Error::UnknownEnum {
                name: "move flag",
                value: 4
            })
        );
        assert!(FlagSet::<MoveFlag>::from_words(&[0, 1]).is_err());
        let mut bitset = Bitset::new(200);
        bitset.set(EntityDataFlag::COUNT as usize);
        assert!(FlagSet::<EntityDataFlag>::from_bitset(&bitset).is_err());
    }

    #[test]
    fn bitsets_round_trip() {
        let set: FlagSet<EntityDataFlag> =
            [EntityDataFlag::Baby, EntityDataFlag::NotPickableFromInside]
                .into_iter()
                .collect();
        let bitset = set.to_bitset();
        assert_eq!(bitset.len(), EntityDataFlag::COUNT as usize);
        assert_eq!(FlagSet::from_bitset(&bitset), Ok(set));
    }

    #[test]
    fn input_flags_convert_to_a_set_and_back() {
        let set: FlagSet<InputFlag> = [InputFlag::Jumping, InputFlag::InternalUpdate]
            .into_iter()
            .collect();
        let flags = InputFlags::from_set(&set);
        assert_eq!(flags.size, 66);
        assert_eq!(flags.ids, [6, 65]);
        assert_eq!(flags.to_set(), Ok(set));
        let bad = InputFlags {
            set: true,
            size: 66,
            ids: vec![66],
        };
        assert!(bad.to_set().is_err());
    }
}
