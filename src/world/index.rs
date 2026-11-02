//! The lookup-only chunk index: chunk positions to loaded chunks through a hash table with a
//! fixed hash, so a block lookup costs one probe instead of a tree search (measured in
//! `world::tests::block_access_cost`: 1.6 ns against 14 to 29 ns for a `BTreeMap` of 25 to 1,089
//! chunks).
//!
//! It has no iteration API, so its order can never reach game logic; the world keeps the
//! positions in order beside it.

use super::{Chunk, ChunkPos};

/// Slots of a new or emptied table.
const MIN_SLOTS: usize = 64;

/// Open addressing with linear probing and backward-shift deletion. Invariants: the slot count
/// is a power of two at least [`MIN_SLOTS`]; at most half the slots are full, so every probe
/// ends at an empty slot; a key sits at or after its home slot with no empty slot between.
#[derive(Debug)]
pub(crate) struct Chunks {
    slots: Vec<Option<(ChunkPos, Box<Chunk>)>>,
    len: usize,
}

impl Chunks {
    /// An index with no chunks.
    pub(crate) fn new() -> Self {
        Self {
            slots: empty(MIN_SLOTS),
            len: 0,
        }
    }

    /// Whether a chunk is at `pos`.
    #[inline]
    pub(crate) fn contains(&self, pos: ChunkPos) -> bool {
        self.find(pos).is_some()
    }

    /// The chunk at `pos`.
    #[inline]
    pub(crate) fn get(&self, pos: ChunkPos) -> Option<&Chunk> {
        let i = self.find(pos)?;
        self.slots.get(i)?.as_ref().map(|(_, c)| c.as_ref())
    }

    /// The chunk at `pos`, to change it.
    pub(crate) fn get_mut(&mut self, pos: ChunkPos) -> Option<&mut Chunk> {
        let i = self.find(pos)?;
        self.slots.get_mut(i)?.as_mut().map(|(_, c)| c.as_mut())
    }

    /// Inserts or replaces the chunk at `pos`; returns the replaced one.
    pub(crate) fn insert(&mut self, pos: ChunkPos, chunk: Chunk) -> Option<Chunk> {
        if let Some(i) = self.find(pos)
            && let Some(Some((_, old))) = self.slots.get_mut(i)
        {
            return Some(*std::mem::replace(old, Box::new(chunk)));
        }
        if (self.len + 1) * 2 > self.slots.len() {
            self.resize(self.slots.len() * 2);
        }
        self.place(pos, Box::new(chunk));
        self.len += 1;
        None
    }

    /// Removes the chunk at `pos`. Later keys of its run move back so no probe skips them, and
    /// the table halves when at most an eighth of it is full.
    pub(crate) fn remove(&mut self, pos: ChunkPos) -> Option<Chunk> {
        let mut hole = self.find(pos)?;
        let (_, chunk) = self.slots.get_mut(hole)?.take()?;
        self.len -= 1;
        let mask = self.mask();
        let mut j = (hole + 1) & mask;
        while let Some(Some((k, _))) = self.slots.get(j) {
            let wanted = self.home(*k);
            // Move the key back unless its home lies cyclically in (hole, j].
            if (j.wrapping_sub(wanted) & mask) >= (j.wrapping_sub(hole) & mask) {
                let moved = self.slots.get_mut(j).and_then(Option::take);
                if let Some(slot) = self.slots.get_mut(hole) {
                    *slot = moved;
                }
                hole = j;
            }
            j = (j + 1) & mask;
        }
        if self.slots.len() > MIN_SLOTS && self.len * 8 <= self.slots.len() {
            self.resize(self.slots.len() / 2);
        }
        Some(*chunk)
    }

    /// The slot holding `pos`.
    #[inline]
    fn find(&self, pos: ChunkPos) -> Option<usize> {
        let mut i = self.home(pos);
        loop {
            match self.slots.get(i)? {
                Some((k, _)) if *k == pos => return Some(i),
                Some(_) => i = (i + 1) & self.mask(),
                None => return None,
            }
        }
    }

    /// The home slot of a key: Fibonacci hashing of the coordinates (fixed, deterministic).
    #[inline]
    fn home(&self, pos: ChunkPos) -> usize {
        let key = (u64::from(pos.x.cast_unsigned()) << 32) | u64::from(pos.z.cast_unsigned());
        let bits = self.slots.len().trailing_zeros();
        let hash = key.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> (64 - bits);
        #[expect(
            clippy::expect_used,
            reason = "invariant: the hash is shifted down to the slot count's bits, which fit usize"
        )]
        usize::try_from(hash).expect("a slot's home fits usize")
    }

    /// The bit mask that wraps a probe around the slots.
    #[inline]
    fn mask(&self) -> usize {
        self.slots.len() - 1
    }

    /// Rebuilds the table with `slots` slots, placing every chunk again.
    fn resize(&mut self, slots: usize) {
        let old = std::mem::replace(&mut self.slots, empty(slots));
        for (pos, chunk) in old.into_iter().flatten() {
            self.place(pos, chunk);
        }
    }

    /// Puts a key that is not in the table into the first empty slot from its home.
    fn place(&mut self, pos: ChunkPos, chunk: Box<Chunk>) {
        let mut i = self.home(pos);
        let mask = self.mask();
        while let Some(Some(_)) = self.slots.get(i) {
            i = (i + 1) & mask;
        }
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: the probe ends at an empty slot inside the table"
        )]
        let slot = &mut self.slots[i];
        *slot = Some((pos, chunk));
    }
}

/// A table of `slots` empty slots.
fn empty(slots: usize) -> Vec<Option<(ChunkPos, Box<Chunk>)>> {
    std::iter::repeat_with(|| None).take(slots).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use crate::block::State;
    use crate::protocol::Rng;

    /// A chunk told apart by the block at (0, 0, 0).
    fn tagged(tag: State) -> Chunk {
        let mut c = Chunk::new();
        c.set_block(0, 0, 0, tag).unwrap();
        c
    }

    #[test]
    fn random_operations_match_a_tree() {
        let tags: Vec<State> = State::all().take(50).collect();
        let mut rng = Rng(0x01de_0001);
        let mut index = Chunks::new();
        let mut model: BTreeMap<ChunkPos, State> = BTreeMap::new();
        for round in 0..200_000_usize {
            // Phases of growth and of mass removal, over a small and a wide area.
            let span = if (round / 50_000) % 2 == 0 { 40 } else { 4000 };
            let growing = (round / 10_000) % 2 == 0;
            let pos = ChunkPos::new(
                i32::try_from(rng.below(span)).unwrap() - 20,
                i32::try_from(rng.below(span)).unwrap() - 20,
            );
            if rng.below(3) != 0 || !growing {
                if rng.below(4) == 0 || !growing {
                    let got = index.remove(pos).map(|c| c.block(0, 0, 0).unwrap());
                    assert_eq!(got, model.remove(&pos));
                } else {
                    assert_eq!(
                        index.get(pos).map(|c| c.block(0, 0, 0).unwrap()),
                        model.get(&pos).copied()
                    );
                }
            } else {
                let tag = tags[rng.below(tags.len())];
                let got = index
                    .insert(pos, tagged(tag))
                    .map(|c| c.block(0, 0, 0).unwrap());
                assert_eq!(got, model.insert(pos, tag));
            }
            assert_eq!(index.len, model.len());
            assert!(index.len * 2 <= index.slots.len());
            assert!(index.slots.len() <= MIN_SLOTS.max(model.len() * 16));
        }
        for (&pos, &tag) in &model {
            assert_eq!(index.get(pos).unwrap().block(0, 0, 0), Some(tag));
            assert!(index.contains(pos));
        }
        assert!(index.get_mut(ChunkPos::new(i32::MAX, i32::MIN)).is_none());
    }
}
