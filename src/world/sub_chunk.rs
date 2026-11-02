//! A sub-chunk: a 16×16×16 cube of blocks in two layers (layer 1 holds water
//! logging), and its network form.

use crate::block::{self, State};
use crate::math::Aabb;
use crate::protocol::Writer;

use super::paletted::Paletted;

/// The network sub-chunk version Anchor writes (version 9: with the y index).
const NETWORK_VERSION: u8 = 9;

/// A palette entry of a sub-chunk layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Entry {
    /// A block state of the table.
    Known(State),
    /// A block state Anchor does not know: the index of its original NBT in the chunk's unknown
    /// list. It is kept as read and sent as `minecraft:info_update`.
    Unknown(u32),
}

impl Entry {
    /// Air, the value of an empty slot.
    pub(crate) const AIR: Self = Self::Known(block::AIR);

    /// The block-local collision boxes, or `None` for a state Anchor does not know.
    #[inline]
    pub(crate) fn collision_boxes(self) -> Option<&'static [Aabb]> {
        match self {
            Self::Known(state) => Some(state.collision_boxes()),
            Self::Unknown(_) => None,
        }
    }

    /// The network ID the client is sent for this entry.
    pub(crate) fn network_id(self) -> u32 {
        match self {
            Self::Known(state) => state.network_id(),
            Self::Unknown(_) => block::INFO_UPDATE.network_id(),
        }
    }

    /// The block state of an entry Anchor knows.
    pub(crate) fn state(self) -> Option<State> {
        match self {
            Self::Known(state) => Some(state),
            Self::Unknown(_) => None,
        }
    }

    /// The index in the chunk's unknown list of a state Anchor does not know.
    pub(crate) fn unknown_index(self) -> Option<u32> {
        match self {
            Self::Known(_) => None,
            Self::Unknown(i) => Some(i),
        }
    }
}

crate::tick::checkpoint::variants!(Entry {
    0 => Known(state),
    1 => Unknown(index),
});

/// The two block layers of a 16×16×16 cube.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubChunk {
    layers: [Paletted<Entry>; 2],
}

impl SubChunk {
    /// A sub-chunk of air.
    pub(crate) fn new() -> Self {
        Self {
            layers: [Paletted::uniform(Entry::AIR), Paletted::uniform(Entry::AIR)],
        }
    }

    /// A sub-chunk of these layers (layer 0, then layer 1).
    pub(crate) fn from_layers(layers: [Paletted<Entry>; 2]) -> Self {
        Self { layers }
    }

    /// Writes the network form (version 9): the layers in runtime form with network IDs, unknown
    /// entries as `minecraft:info_update`. Layer 1 is left out when it is all air, as the official
    /// server does.
    pub(crate) fn encode(&self, y_index: i8, w: &mut Writer) {
        let [blocks, water] = &self.layers;
        let with_water = !water.is_uniform(Entry::AIR);
        w.uint8(NETWORK_VERSION);
        w.uint8(if with_water { 2 } else { 1 });
        w.int8(y_index);
        blocks.encode(w, Entry::network_id);
        if with_water {
            water.encode(w, Entry::network_id);
        }
    }

    /// The layer 0 entry of a slot.
    #[inline]
    pub(crate) fn get(&self, offset: u16) -> Entry {
        let [blocks, _] = &self.layers;
        blocks.get(offset)
    }

    /// Whether both layers are all air: such a sub-chunk is not kept.
    pub(crate) fn is_empty(&self) -> bool {
        self.layers.iter().all(|l| l.is_uniform(Entry::AIR))
    }

    /// The layers: 0 holds blocks, 1 water logging.
    pub(crate) fn layers(&self) -> &[Paletted<Entry>; 2] {
        &self.layers
    }

    /// The light values of the layer 0 block of a slot, kept by its palette.
    #[inline]
    pub(crate) fn light_values(&self, offset: u16) -> super::light::Values {
        let [blocks, _] = &self.layers;
        blocks.derived(offset)
    }

    /// The most bytes [`Self::encode`] writes: three header bytes and both layers.
    pub(crate) fn max_encoded_bytes(&self) -> usize {
        3 + self
            .layers
            .iter()
            .map(Paletted::max_encoded_bytes)
            .sum::<usize>()
    }

    /// Compacts both layers ([`Paletted::compact`]).
    pub(crate) fn compact(&mut self) {
        for layer in &mut self.layers {
            layer.compact();
        }
    }

    /// Sets the layer 0 entry of a slot, clears its layer 1 entry (the water a stored block held,
    /// which no rule keeps in step with a new block), and returns the old layer 0 entry.
    pub(crate) fn set(&mut self, offset: u16, entry: Entry) -> Entry {
        let [blocks, water] = &mut self.layers;
        water.set(offset, Entry::AIR);
        blocks.set(offset, entry)
    }
}

crate::tick::checkpoint::fields!(SubChunk { layers });
