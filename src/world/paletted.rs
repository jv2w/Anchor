//! Paletted storage of one 16×16×16 cube: a palette and a bit array of palette
//! indices in Bedrock's widths and word packing, so the network and disk forms copy the words.
//!
//! Each palette entry counts the slots that use it. A new value takes the place of an entry no
//! slot uses before the palette grows, so a palette never holds more than 4096 entries. The bit
//! array widens one step when the palette is full, and narrows when at most a quarter of its
//! capacity is in use (to the width holding twice the entries in use, so widening and narrowing
//! cannot alternate on every set).

use std::collections::BTreeMap;
use std::fmt;

use crate::protocol::{BLOCKS, WIDTHS, Writer, word_count};

use super::{Error, Result};

/// Slots in a cube: the protocol's blocks in a sub-chunk, as a slot offset.
pub(crate) const SLOTS: u16 = 4096;
const _: () = assert!(SLOTS as usize == BLOCKS);
/// The most bytes a 32-bit varint takes.
const MAX_VARINT32_BYTES: usize = 5;

/// A cube of values of type `T`: block entries or biome IDs.
///
/// Invariants: `bits` is a Bedrock width; `words` has [`word_count`] words; every slot's index
/// points into `palette` and the bits that are no slot's are 0; `palette` holds distinct entries,
/// at most [`capacity`]; `derived[i]` is `palette[i].derive()`; `counts[i]` is the number of
/// slots at entry `i` (they sum to [`SLOTS`]); `dead` is the number of zero counts; `lookup`
/// holds every entry with its index, sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Paletted<T: Derive> {
    bits: u8,
    words: Vec<u32>,
    palette: Vec<T>,
    derived: Vec<T::Derived>,
    counts: Vec<u16>,
    lookup: Vec<(T, u16)>,
    dead: usize,
}

impl<T: Derive> Paletted<T> {
    /// A cube with `value` in every slot, in width 0 (no words).
    pub(crate) fn uniform(value: T) -> Self {
        Self {
            bits: 0,
            words: Vec::new(),
            palette: vec![value],
            derived: vec![value.derive()],
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
        if (0..SLOTS).any(|o| usize::from(index_in(bits, words, o)) >= palette.len()) {
            return Err(Error::Malformed("a slot points outside its palette"));
        }
        Ok(Self::compacted(bits, words, palette))
    }

    /// A cube from the exact parts of a live one ([`Self::bits`], [`Self::words`],
    /// [`Self::palette`]), as a checkpoint for the replay tool keeps it: unlike
    /// [`Self::from_parts`] nothing is compacted, so it is the cube that was kept, unused entries
    /// and width included. Fails unless the parts keep the invariants: a Bedrock width, its word
    /// count, distinct entries the width indexes, every slot's index in the palette and the bits
    /// that are no slot's 0.
    pub(crate) fn from_raw(bits: u8, words: &[u32], palette: &[T]) -> Result<Self> {
        if !WIDTHS.contains(&bits) || words.len() != word_count(bits) {
            return Err(Error::Malformed("a kept palette width or word count"));
        }
        if palette.is_empty() || palette.len() > capacity(bits) {
            return Err(Error::Malformed("a kept palette size"));
        }
        let mut out = Self::with_palette(bits, words.to_vec(), palette.to_vec());
        if out
            .lookup
            .iter()
            .zip(out.lookup.iter().skip(1))
            .any(|(a, b)| a.0 == b.0)
        {
            return Err(Error::Malformed("repeated kept palette entries"));
        }
        if bits == 0 {
            out.counts = vec![SLOTS];
            return Ok(out);
        }
        // The bits of each word that hold slots: whole indices, and in the last word only the
        // slots left.
        let per = per_word(bits);
        let mut left = usize::from(SLOTS);
        for &word in words {
            let here = left.min(per);
            left -= here;
            let used = u32::try_from(here * usize::from(bits)).unwrap_or(32);
            let mask = if used >= 32 {
                u32::MAX
            } else {
                (1 << used) - 1
            };
            if word & !mask != 0 {
                return Err(Error::Malformed("kept padding bits"));
            }
        }
        for offset in 0..SLOTS {
            let index = out.index(offset);
            let Some(c) = out.counts.get_mut(usize::from(index)) else {
                return Err(Error::Malformed("a kept slot points outside its palette"));
            };
            *c += 1;
        }
        out.count_dead();
        Ok(out)
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

    /// The value of a slot (`offset` below [`SLOTS`]).
    #[inline]
    pub(crate) fn get(&self, offset: u16) -> T {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: every slot's index points into the palette"
        )]
        self.palette[usize::from(self.index(offset))]
    }

    /// What the palette keeps for the value of a slot (`offset` below [`SLOTS`]).
    #[inline]
    pub(crate) fn derived(&self, offset: u16) -> T::Derived {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: every slot's index points into the palette, which `derived` matches"
        )]
        self.derived[usize::from(self.index(offset))]
    }

    /// Whether every slot holds `value`.
    pub(crate) fn is_uniform(&self, value: T) -> bool {
        self.find(value)
            .is_some_and(|i| self.counts.get(usize::from(i)) == Some(&SLOTS))
    }

    /// The most bytes [`Self::encode`] writes: the header, the words, and the palette length and
    /// entries as varints of at most [`MAX_VARINT32_BYTES`].
    pub(crate) fn max_encoded_bytes(&self) -> usize {
        1 + 4 * self.words.len() + MAX_VARINT32_BYTES * (1 + self.palette.len())
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

    /// Compacts the cube as [`Self::from_parts`] does. Generated chunks are compacted, which
    /// gives BDS's palette order (P6).
    pub(crate) fn compact(&mut self) {
        *self = Self::compacted(self.bits, &self.words, &self.palette);
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

    /// The cube of `palette` whose slot indices `words` packs at width `bits`, every one pointing
    /// into `palette`, with repeated and unused entries dropped, the others in the order of their
    /// first slot, in the narrowest width.
    fn compacted(bits: u8, words: &[u32], palette: &[T]) -> Self {
        // A width 0 cube holds its one entry in every slot: it is compact already.
        if bits == 0
            && let Some(&value) = palette.first()
        {
            return Self::uniform(value);
        }
        // `map` takes an old index to the new one (u16::MAX: not seen yet).
        let mut map = vec![u16::MAX; palette.len()];
        let mut values: Vec<T> = Vec::new();
        let mut seen: BTreeMap<T, u16> = BTreeMap::new();
        let mut indices = Vec::with_capacity(usize::from(SLOTS));
        for offset in 0..SLOTS {
            let i = usize::from(index_in(bits, words, offset));
            let (Some(slot), Some(&value)) = (map.get_mut(i), palette.get(i)) else {
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
        let bits = width_for(values.len());
        Self::build(values, bits, &indices)
    }

    /// A cube of `palette` (distinct entries) in width `bits` with the slot indices `indices`.
    fn build(palette: Vec<T>, bits: u8, indices: &[u16]) -> Self {
        let mut out = Self::with_palette(bits, vec![0; word_count(bits)], palette);
        for (offset, &index) in (0..SLOTS).zip(indices) {
            out.write_index(offset, index);
            if let Some(c) = out.counts.get_mut(usize::from(index)) {
                *c += 1;
            }
        }
        out.count_dead();
        out
    }

    /// A cube of `palette` (distinct entries) whose slots are not counted yet: `words` as given,
    /// the kept values and the lookup worked out, every count 0.
    fn with_palette(bits: u8, words: Vec<u32>, palette: Vec<T>) -> Self {
        let derived = derive_all(&palette);
        let counts = vec![0; palette.len()];
        let mut lookup: Vec<(T, u16)> = (0..).zip(&palette).map(|(i, &v)| (v, i)).collect();
        lookup.sort_unstable();
        Self {
            bits,
            words,
            palette,
            derived,
            counts,
            lookup,
            dead: 0,
        }
    }

    /// The palette index of a slot (`offset` below [`SLOTS`]).
    #[inline]
    fn index(&self, offset: u16) -> u16 {
        index_in(self.bits, &self.words, offset)
    }

    /// The palette index of `value`, if the palette has it.
    fn find(&self, value: T) -> Option<u16> {
        let at = self.lookup.binary_search_by(|(v, _)| v.cmp(&value)).ok()?;
        self.lookup.get(at).map(|&(_, i)| i)
    }

    /// Counts the entries no slot uses.
    fn count_dead(&mut self) {
        self.dead = self.counts.iter().filter(|&&c| c == 0).count();
    }

    /// Writes the palette index of a slot (`offset` below [`SLOTS`]); width 0 holds none.
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
        self.derived.push(value.derive());
        self.counts.push(0);
        let at = self.lookup.partition_point(|(v, _)| *v < value);
        self.lookup.insert(at, (value, index));
        index
    }

    /// Puts `value` in the place of an entry no slot uses. Only a full palette does this, so the
    /// scan runs once per new value of a full palette.
    fn reuse(&mut self, value: T) -> u16 {
        #[expect(
            clippy::expect_used,
            reason = "invariant: insert calls it only while an entry is unused"
        )]
        let slot = self
            .counts
            .iter()
            .position(|&c| c == 0)
            .expect("a palette with dead entries has an unused one");
        self.dead -= 1;
        let index = to_index(slot);
        if let Some(kept) = self.derived.get_mut(slot) {
            *kept = value.derive();
        }
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
        *self = Self::build(palette, bits, &indices);
    }
}

/// A cube's checkpoint form: its exact parts ([`Paletted::from_raw`]); the kept values, the
/// counts and the lookup are the parts' again.
impl<T: Derive + crate::tick::checkpoint::Codec> crate::tick::checkpoint::Codec for Paletted<T> {
    fn encode(&self, w: &mut crate::tick::checkpoint::Writer) {
        let Self {
            bits,
            words,
            palette,
            derived: _,
            counts: _,
            lookup: _,
            dead: _,
        } = self;
        bits.encode(w);
        words.encode(w);
        palette.encode(w);
    }

    fn decode(
        r: &mut crate::tick::checkpoint::Reader<'_>,
    ) -> crate::tick::checkpoint::Result<Self> {
        use crate::tick::checkpoint::{Codec, Error};
        let bits = u8::decode(r)?;
        let words: Vec<u32> = Codec::decode(r)?;
        let palette: Vec<T> = Codec::decode(r)?;
        Self::from_raw(bits, &words, &palette).map_err(|_| Error::Format("palette"))
    }
}

/// A value of a palette, with what the palette keeps worked out beside each entry
/// ([`Paletted::derived`]): a reader that needs only that reads it by the slot's index, never
/// the entry. The light work reads a block's light values (3 bytes) this way instead of its
/// 24-byte entry and the state table behind it, 2 to 5 times faster per block.
pub(crate) trait Derive: Copy + Ord {
    /// What is kept beside each entry; `()` keeps nothing and takes no memory.
    type Derived: Copy + Eq + fmt::Debug;

    /// What is kept beside `self`.
    fn derive(self) -> Self::Derived;
}

/// Biome IDs keep nothing.
impl Derive for u32 {
    type Derived = ();

    fn derive(self) {}
}

/// Imported block IDs keep nothing.
impl Derive for u16 {
    type Derived = ();

    fn derive(self) {}
}

/// The slot of (x, y, z) in a cube, each taken modulo 16, in Bedrock's x, z, y order.
pub(crate) fn offset(x: u8, y: u8, z: u8) -> u16 {
    (u16::from(x & 15) << 8) | (u16::from(z & 15) << 4) | u16::from(y & 15)
}

/// The palette index of a slot (`offset` below [`SLOTS`]) in `words` packed at width `bits`.
#[inline]
fn index_in(bits: u8, words: &[u32], offset: u16) -> u16 {
    if bits == 0 {
        return 0;
    }
    let (word, shift) = locate(bits, offset);
    let w = words.get(word).copied().unwrap_or(0);
    let mask = (1u32 << bits) - 1;
    u16::try_from((w >> shift) & mask).unwrap_or(0)
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

/// Indices per 32-bit word at a width; an index never spans two words.
fn per_word(bits: u8) -> usize {
    if bits == 0 {
        usize::from(SLOTS)
    } else {
        32 / usize::from(bits)
    }
}

/// The narrowest width whose capacity holds `entries`.
fn width_for(entries: usize) -> u8 {
    WIDTHS
        .into_iter()
        .find(|&bits| capacity(bits) >= entries)
        .unwrap_or(16)
}

/// Entries a palette may hold at a width: what the width indexes, at most one per slot.
fn capacity(bits: u8) -> usize {
    (1usize << bits).min(usize::from(SLOTS))
}

/// Converts a palette position, at most 4096, to an index.
fn to_index(i: usize) -> u16 {
    #[expect(
        clippy::expect_used,
        reason = "invariant: a palette holds at most 4096 entries"
    )]
    u16::try_from(i).expect("a palette position fits an index")
}

/// What each entry of `palette` keeps.
fn derive_all<T: Derive>(palette: &[T]) -> Vec<T::Derived> {
    palette.iter().map(|&v| v.derive()).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protocol::Rng;

    /// Checks every invariant of the type.
    pub(crate) fn check<T: Derive + std::fmt::Debug>(p: &Paletted<T>) {
        assert!(WIDTHS.contains(&p.bits));
        assert_eq!(p.words.len(), word_count(p.bits));
        assert!(p.palette.len() <= capacity(p.bits));
        assert_eq!(p.derived, derive_all(&p.palette), "kept values");
        for o in 0..SLOTS {
            assert_eq!(p.derived(o), p.get(o).derive());
        }
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
    fn widths_and_offsets_follow_bedrock() {
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
            let spread = if (round / 5000) % 2 == 0 { 3000 } else { 3 };
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

    /// A value whose kept value differs from it, so a stale one shows.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    struct Kept(u32);

    impl Derive for Kept {
        type Derived = u64;

        fn derive(self) -> u64 {
            u64::from(self.0) * 3 + 1
        }
    }

    #[test]
    fn kept_values_follow_their_entries_through_growth_reuse_shrinking_and_parts() {
        let mut rng = Rng(0x5eed_0002);
        let mut p = Paletted::uniform(Kept(0));
        for round in 0..30_000usize {
            // Many distinct values fill the palette (widening, then reusing dead entries at
            // 4096); few values narrow it again.
            let spread = if (round / 6000) % 2 == 0 { 6000 } else { 3 };
            let o = u16::try_from(rng.below(usize::from(SLOTS))).unwrap();
            p.set(o, Kept(u32::try_from(rng.below(spread)).unwrap()));
            if round % 1499 == 0 {
                check(&p);
            }
        }
        check(&p);
        let q = Paletted::from_raw(p.bits(), p.words(), p.palette()).unwrap();
        check(&q);
        let mut r = Paletted::from_parts(p.bits(), p.words(), p.palette()).unwrap();
        check(&r);
        r.compact();
        check(&r);
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
    fn raw_parts_give_the_exact_cube_and_malformed_ones_are_errors() {
        let mut p = Paletted::uniform(5u32);
        for o in 0..SLOTS {
            p.set(o, u32::from(o % 7) * 3);
        }
        p.set(0, 99);
        p.set(0, 0);
        let q = Paletted::from_raw(p.bits(), p.words(), p.palette()).unwrap();
        check(&q);
        assert_eq!(q, p, "unused entries, order and width kept");
        let u = Paletted::uniform(7u32);
        assert_eq!(Paletted::from_raw(0, &[], &[7]).unwrap(), u);
        assert!(Paletted::<u32>::from_raw(0, &[], &[7, 8]).is_err());
        assert!(
            Paletted::<u32>::from_raw(2, &[0; 256], &[4, 4]).is_err(),
            "repeated"
        );
        assert!(Paletted::<u32>::from_raw(1, &[0; 128], &[]).is_err());
        assert!(
            Paletted::<u32>::from_raw(2, &[0b11; 256], &[1, 2]).is_err(),
            "outside"
        );
        // 3 bits: 10 indices a word, the top 2 bits padding.
        let mut words = vec![0u32; 410];
        words[0] = 1 << 31;
        assert!(
            Paletted::<u32>::from_raw(3, &words, &[1]).is_err(),
            "padding"
        );
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
