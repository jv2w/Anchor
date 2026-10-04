//! Paletted storage of one 16×16×16 cube (PLAN.md 10.6): a palette and a bit array of palette
//! indices in Bedrock's widths and word packing, so the network and disk forms copy the words.
//!
//! Each palette entry counts the slots that use it. A new value takes the place of an entry no
//! slot uses before the palette grows, so a palette never holds more than 4096 entries. The bit
//! array widens one step when the palette is full, and narrows when at most a quarter of its
//! capacity is in use (to the width holding twice the entries in use, so widening and narrowing
//! cannot alternate on every set).

use std::collections::BTreeMap;

use crate::protocol::Writer;

use super::{Error, Result};

/// Slots in a cube.
pub(crate) const SLOTS: u16 = 4096;
/// Bits per index Bedrock writes (dragonfly `sizes`; `crate::protocol` reads the same).
const WIDTHS: [u8; 9] = [0, 1, 2, 3, 4, 5, 6, 8, 16];

/// The slot of (x, y, z) in a cube, each taken modulo 16, in Bedrock's x, z, y order.
pub(crate) fn offset(x: u8, y: u8, z: u8) -> u16 {
    (u16::from(x & 15) << 8) | (u16::from(z & 15) << 4) | u16::from(y & 15)
}

/// Indices per 32-bit word at a width; an index never spans two words.
fn per_word(bits: u8) -> usize {
    if bits == 0 {
        usize::from(SLOTS)
    } else {
        32 / usize::from(bits)
    }
}

/// Words that hold every slot's index at a width.
fn word_count(bits: u8) -> usize {
    if bits == 0 {
        0
    } else {
        usize::from(SLOTS).div_ceil(per_word(bits))
    }
}

/// Entries a palette may hold at a width: what the width indexes, at most one per slot.
fn capacity(bits: u8) -> usize {
    (1usize << bits).min(usize::from(SLOTS))
}

/// The narrowest width whose capacity holds `entries`.
fn width_for(entries: usize) -> u8 {
    WIDTHS
        .into_iter()
        .find(|&bits| capacity(bits) >= entries)
        .unwrap_or(16)
}

/// The word and bit shift of a slot's index at a width. Matching on the width makes each
/// division one by a constant.
#[inline]
fn locate(bits: u8, offset: u16) -> (usize, u32) {
    #[inline]
    fn at<const PER: u32, const BITS: u32>(offset: u16) -> (usize, u32) {
        let o = u32::from(offset);
        (
            usize::try_from(o / PER).unwrap_or(usize::MAX),
            o % PER * BITS,
        )
    }
    match bits {
        1 => at::<32, 1>(offset),
        2 => at::<16, 2>(offset),
        3 => at::<10, 3>(offset),
        4 => at::<8, 4>(offset),
        5 => at::<6, 5>(offset),
        6 => at::<5, 6>(offset),
        8 => at::<4, 8>(offset),
        16 => at::<2, 16>(offset),
        _ => (usize::MAX, 0),
    }
}

/// Converts a palette position, at most 4096, to an index.
fn to_index(i: usize) -> u16 {
    u16::try_from(i).unwrap_or(u16::MAX)
}

/// A cube of values of type `T`: block entries or biome IDs.
///
/// Invariants: `bits` is a Bedrock width; `words` has [`word_count`] words; every slot's index
/// points into `palette` and the bits that are no slot's are 0; `palette` holds distinct entries,
/// at most [`capacity`]; `counts[i]` is the number of slots at entry `i` (they sum to
/// [`SLOTS`]); `dead` is the number of zero counts; `lookup` holds every entry with its index,
/// sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Paletted<T> {
    bits: u8,
    words: Vec<u32>,
    palette: Vec<T>,
    counts: Vec<u16>,
    lookup: Vec<(T, u16)>,
    dead: usize,
}

impl<T: Copy + Ord> Paletted<T> {
    /// A cube with `value` in every slot, in width 0 (no words).
    pub(crate) fn uniform(value: T) -> Self {
        Self {
            bits: 0,
            words: Vec::new(),
            palette: vec![value],
            counts: vec![SLOTS],
            lookup: vec![(value, 0)],
            dead: 0,
        }
    }

    /// A cube from stored parts, which are external input: the width must be a Bedrock width,
    /// the word count must match it, the palette must have 1 to 4096 entries the width can
    /// index, and every slot's index must point into the palette. The result is compacted:
    /// repeated and unused entries are dropped (the others keep the order of their first slot)
    /// and the width is the narrowest that fits, so equal contents give equal parts.
    pub(crate) fn from_parts(bits: u8, words: &[u32], palette: &[T]) -> Result<Self> {
        if !WIDTHS.contains(&bits) {
            return Err(Error::Malformed("a palette width is not a Bedrock width"));
        }
        if words.len() != word_count(bits) {
            return Err(Error::Malformed("a word count does not match its width"));
        }
        if palette.is_empty() || palette.len() > capacity(bits) {
            return Err(Error::Malformed("a palette size does not fit its width"));
        }
        let stored = Self {
            bits,
            words: words.to_vec(),
            palette: palette.to_vec(),
            counts: Vec::new(),
            lookup: Vec::new(),
            dead: 0,
        };
        if (0..SLOTS).any(|o| usize::from(stored.index(o)) >= palette.len()) {
            return Err(Error::Malformed("a slot points outside its palette"));
        }
        Ok(stored.compacted())
    }

    /// The cube with every entry passed through `f`, compacted ([`Self::from_parts`]): entries
    /// `f` makes equal merge.
    pub(crate) fn map(&self, f: impl Fn(T) -> T) -> Self {
        let mut mapped = self.clone();
        for v in &mut mapped.palette {
            *v = f(*v);
        }
        mapped.compacted()
    }

    /// Compacts the cube as [`Self::from_parts`] does. Generated chunks are compacted, which
    /// gives BDS's palette order (P6).
    pub(crate) fn compact(&mut self) {
        *self = self.compacted();
    }

    /// The cube with repeated and unused entries dropped, the others in the order of their first
    /// slot, in the narrowest width. Needs only `bits`, `words` and `palette`, every slot's index
    /// pointing into `palette`.
    fn compacted(&self) -> Self {
        // `map` takes an old index to the new one (u16::MAX: not seen yet).
        let mut map = vec![u16::MAX; self.palette.len()];
        let mut values: Vec<T> = Vec::new();
        let mut seen: BTreeMap<T, u16> = BTreeMap::new();
        let mut indices = Vec::with_capacity(usize::from(SLOTS));
        for offset in 0..SLOTS {
            let i = usize::from(self.index(offset));
            let (Some(slot), Some(&value)) = (map.get_mut(i), self.palette.get(i)) else {
                indices.push(0);
                continue;
            };
            if *slot == u16::MAX {
                // A repeated entry shares the index of its first copy.
                let next = to_index(values.len());
                *slot = *seen.entry(value).or_insert(next);
                if *slot == next {
                    values.push(value);
                }
            }
            indices.push(*slot);
        }
        Self::build(&values, width_for(values.len()), &indices)
    }

    /// A cube of `palette` (distinct entries) in width `bits` with the slot indices `indices`.
    fn build(palette: &[T], bits: u8, indices: &[u16]) -> Self {
        let mut out = Self {
            bits,
            words: vec![0; word_count(bits)],
            palette: palette.to_vec(),
            counts: vec![0; palette.len()],
            lookup: (0..).zip(palette).map(|(i, &v)| (v, i)).collect(),
            dead: 0,
        };
        out.lookup.sort_unstable();
        for (offset, &index) in (0..SLOTS).zip(indices) {
            out.write_index(offset, index);
            if let Some(c) = out.counts.get_mut(usize::from(index)) {
                *c += 1;
            }
        }
        out.dead = out.counts.iter().filter(|&&c| c == 0).count();
        out
    }

    /// Bits per index: 0, 1, 2, 3, 4, 5, 6, 8 or 16.
    pub(crate) fn bits(&self) -> u8 {
        self.bits
    }

    /// The packed slot indices, as the network and disk forms hold them.
    pub(crate) fn words(&self) -> &[u32] {
        &self.words
    }

    /// The palette. An entry no slot uses stays until a new value takes its place.
    pub(crate) fn palette(&self) -> &[T] {
        &self.palette
    }

    /// The palette index of a slot (`offset` below [`SLOTS`]).
    #[inline]
    fn index(&self, offset: u16) -> u16 {
        if self.bits == 0 {
            return 0;
        }
        let (word, shift) = locate(self.bits, offset);
        let w = self.words.get(word).copied().unwrap_or(0);
        let mask = (1u32 << self.bits) - 1;
        u16::try_from((w >> shift) & mask).unwrap_or(0)
    }

    #[inline]
    fn write_index(&mut self, offset: u16, index: u16) {
        if self.bits == 0 {
            return;
        }
        let (word, shift) = locate(self.bits, offset);
        let mask = (1u32 << self.bits) - 1;
        if let Some(w) = self.words.get_mut(word) {
            *w = (*w & !(mask << shift)) | (u32::from(index) << shift);
        }
    }

    /// The value of a slot (`offset` below [`SLOTS`]).
    #[inline]
    pub(crate) fn get(&self, offset: u16) -> T {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: every slot's index points into the palette"
        )]
        self.palette[usize::from(self.index(offset))]
    }

    /// Whether every slot holds `value`.
    pub(crate) fn is_uniform(&self, value: T) -> bool {
        self.find(value)
            .is_some_and(|i| self.counts.get(usize::from(i)) == Some(&SLOTS))
    }

    /// The palette index of `value`, if the palette has it.
    fn find(&self, value: T) -> Option<u16> {
        let at = self.lookup.binary_search_by(|(v, _)| v.cmp(&value)).ok()?;
        self.lookup.get(at).map(|&(_, i)| i)
    }

    /// Sets a slot (`offset` below [`SLOTS`]) and returns its old value.
    pub(crate) fn set(&mut self, offset: u16, value: T) -> T {
        let old_index = self.index(offset);
        let old = self.get(offset);
        if old == value {
            return old;
        }
        // Release the old entry first: if every slot held a different value, its entry is the
        // one the new value takes.
        if let Some(c) = self.counts.get_mut(usize::from(old_index)) {
            *c -= 1;
            if *c == 0 {
                self.dead += 1;
            }
        }
        let index = match self.find(value) {
            Some(i) => {
                if self.counts.get(usize::from(i)) == Some(&0) {
                    self.dead -= 1;
                }
                i
            }
            None => self.insert(value),
        };
        self.write_index(offset, index);
        if let Some(c) = self.counts.get_mut(usize::from(index)) {
            *c += 1;
        }
        self.shrink();
        old
    }

    /// Adds `value` to the palette with count 0 and returns its index.
    fn insert(&mut self, value: T) -> u16 {
        if self.palette.len() >= capacity(self.bits) {
            if self.dead > 0 {
                return self.reuse(value);
            }
            self.repack(width_for(self.palette.len() + 1));
        }
        let index = to_index(self.palette.len());
        self.palette.push(value);
        self.counts.push(0);
        let at = self.lookup.partition_point(|(v, _)| *v < value);
        self.lookup.insert(at, (value, index));
        index
    }

    /// Puts `value` in the place of an entry no slot uses. Only a full palette does this, so the
    /// scan runs once per new value of a full palette.
    fn reuse(&mut self, value: T) -> u16 {
        let Some(slot) = self.counts.iter().position(|&c| c == 0) else {
            return 0;
        };
        self.dead -= 1;
        let index = to_index(slot);
        if let Some(entry) = self.palette.get_mut(slot) {
            let gone = std::mem::replace(entry, value);
            if let Ok(at) = self.lookup.binary_search(&(gone, index)) {
                self.lookup.remove(at);
            }
        }
        let at = self.lookup.partition_point(|(v, _)| *v < value);
        self.lookup.insert(at, (value, index));
        index
    }

    /// Rewrites the slot indices in width `bits`, keeping the palette. It walks both word
    /// arrays in order (no division per slot): a palette widens on every new value, so this
    /// runs often while a sub-chunk fills.
    fn repack(&mut self, bits: u8) {
        let old = std::mem::replace(&mut self.words, vec![0; word_count(bits)]);
        let old_bits = self.bits;
        self.bits = bits;
        // Width 0 has every index 0: the new words are 0 already.
        if old_bits == 0 || bits == 0 {
            return;
        }
        let (old_per, new_per) = (per_word(old_bits), per_word(bits));
        let old_mask = (1u32 << old_bits) - 1;
        let mut slots = usize::from(SLOTS);
        let (mut word, mut shift, mut in_word) = (0usize, 0u32, 0usize);
        for &w in &old {
            let mut from = 0u32;
            for _ in 0..old_per.min(slots) {
                let index = (w >> from) & old_mask;
                from += u32::from(old_bits);
                if let Some(target) = self.words.get_mut(word) {
                    *target |= index << shift;
                }
                in_word += 1;
                shift += u32::from(bits);
                if in_word == new_per {
                    (word, shift, in_word) = (word + 1, 0, 0);
                }
            }
            slots = slots.saturating_sub(old_per);
        }
    }

    /// When at most a quarter of the capacity is in use, drops the unused entries and rewrites
    /// the indices in the narrowest width (at least 1) holding twice the entries in use.
    fn shrink(&mut self) {
        let live = self.palette.len() - self.dead;
        if self.dead == 0 || live * 4 > capacity(self.bits) {
            return;
        }
        let bits = width_for(live * 2).max(1);
        if bits >= self.bits {
            return;
        }
        let mut map = vec![0u16; self.palette.len()];
        let mut palette = Vec::with_capacity(live);
        for ((m, &value), &count) in map.iter_mut().zip(&self.palette).zip(&self.counts) {
            if count > 0 {
                *m = to_index(palette.len());
                palette.push(value);
            }
        }
        let indices: Vec<u16> = (0..SLOTS)
            .map(|o| map.get(usize::from(self.index(o))).copied().unwrap_or(0))
            .collect();
        *self = Self::build(&palette, bits, &indices);
    }

    /// Writes the network form (runtime palette): the header, the words, and the palette as
    /// varints of `id(entry)`.
    pub(crate) fn encode(&self, w: &mut Writer, id: impl Fn(T) -> u32) {
        w.uint8((self.bits << 1) | 1);
        for &word in &self.words {
            w.uint32(word);
        }
        if self.bits != 0 {
            w.varint32(i32::try_from(self.palette.len()).unwrap_or(i32::MAX));
        }
        for &entry in &self.palette {
            w.varint32(id(entry).cast_signed());
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protocol::Rng;

    /// Checks every invariant of the type.
    pub(crate) fn check<T: Copy + Ord + std::fmt::Debug>(p: &Paletted<T>) {
        assert!(WIDTHS.contains(&p.bits));
        assert_eq!(p.words.len(), word_count(p.bits));
        assert!(p.palette.len() <= capacity(p.bits));
        let mut counts = vec![0u16; p.palette.len()];
        for o in 0..SLOTS {
            counts[usize::from(p.index(o))] += 1;
        }
        assert_eq!(counts, p.counts);
        assert_eq!(p.dead, counts.iter().filter(|&&c| c == 0).count());
        let mut lookup: Vec<_> = (0..).zip(&p.palette).map(|(i, &v)| (v, i)).collect();
        lookup.sort_unstable();
        assert_eq!(lookup, p.lookup);
        let mut distinct = p.palette.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), p.palette.len(), "distinct entries");
        if p.bits != 0 {
            let per = per_word(p.bits);
            for (i, &w) in p.words.iter().enumerate() {
                let slots = (usize::from(SLOTS) - i * per).min(per);
                let valid = slots * usize::from(p.bits);
                if valid < 32 {
                    assert_eq!(w >> valid, 0, "padding of word {i}");
                }
            }
        }
    }

    #[test]
    fn widths_have_dragonfly_word_counts() {
        assert_eq!(
            WIDTHS.map(word_count),
            [0, 128, 256, 410, 512, 683, 820, 1024, 2048]
        );
        assert_eq!(width_for(1), 0);
        assert_eq!(width_for(3), 2);
        assert_eq!(width_for(65), 8);
        assert_eq!(width_for(4096), 16);
        assert_eq!(offset(1, 2, 3), 0x132);
    }

    #[test]
    fn random_sets_match_an_array_and_keep_the_invariants() {
        let mut rng = Rng(0x5eed_0001);
        let mut p = Paletted::uniform(0u32);
        let mut model = vec![0u32; usize::from(SLOTS)];
        let mut widths = std::collections::BTreeSet::new();
        for round in 0..40_000usize {
            // Phases of many distinct values (widening) and of few (narrowing).
            let spread = if (round / 5_000) % 2 == 0 { 3000 } else { 3 };
            let o = rng.below(model.len());
            let v = u32::try_from(rng.below(spread)).unwrap();
            let offset = u16::try_from(o).unwrap();
            assert_eq!(p.set(offset, v), model[o]);
            model[o] = v;
            widths.insert(p.bits);
            if round % 997 == 0 {
                check(&p);
                for (o, &m) in (0..SLOTS).zip(&model) {
                    assert_eq!(p.get(o), m);
                }
            }
        }
        check(&p);
        assert!(widths.len() >= 6, "widths seen: {widths:?}");
    }

    #[test]
    fn all_distinct_values_fit_in_4096_entries() {
        let mut p = Paletted::uniform(u32::MAX);
        for round in 0..3u32 {
            for o in 0..SLOTS {
                p.set(o, u32::from(o) + round * 10_000);
                assert!(p.palette.len() <= usize::from(SLOTS));
            }
        }
        check(&p);
        assert_eq!(p.bits, 16);
        for o in 0..SLOTS {
            p.set(o, 7);
        }
        check(&p);
        assert!(p.is_uniform(7));
        assert_eq!(p.bits, 1, "narrowed");
    }

    #[test]
    fn parts_round_trip_and_are_compacted() {
        let mut p = Paletted::uniform(5u32);
        for o in 0..SLOTS {
            p.set(o, u32::from(o % 7) * 3);
        }
        p.set(0, 99);
        p.set(0, 0);
        let q = Paletted::from_parts(p.bits(), p.words(), p.palette()).unwrap();
        check(&q);
        for o in 0..SLOTS {
            assert_eq!(q.get(o), p.get(o));
        }
        assert_eq!(q.palette(), [0, 3, 6, 9, 12, 15, 18]);
        assert_eq!(q.bits(), 3);
        // A palette with a repeated entry and an unused one.
        let words = vec![0b10_01_00u32; 256];
        let r = Paletted::from_parts(2, &words, &[4u32, 4, 8, 1]).unwrap();
        check(&r);
        assert_eq!(r.palette(), [4, 8]);
        assert_eq!(r.get(offset(0, 2, 0)), 8);
        assert_eq!(r.get(offset(0, 1, 0)), 4);
    }

    #[test]
    fn malformed_parts_are_errors() {
        assert!(Paletted::<u32>::from_parts(7, &[0; 586], &[1]).is_err());
        assert!(Paletted::<u32>::from_parts(1, &[0; 127], &[1]).is_err());
        assert!(Paletted::<u32>::from_parts(1, &[0; 128], &[]).is_err());
        assert!(Paletted::<u32>::from_parts(1, &[0; 128], &[1, 2, 3]).is_err());
        assert!(Paletted::<u32>::from_parts(1, &[1; 128], &[1]).is_err());
        assert!(Paletted::<u32>::from_parts(0, &[], &[1]).is_ok());
        let big: Vec<u32> = (0..4097).collect();
        assert!(Paletted::<u32>::from_parts(16, &[0; 2048], &big).is_err());
    }
}
