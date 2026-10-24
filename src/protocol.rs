//! Bedrock network protocol: packet codecs, batches and compression.

pub mod batch;
mod block_storage;
mod common;
pub mod compression;
pub mod consts;
mod entity_data;
mod flags;
mod inventory;
mod io;
mod login;
pub mod packet;
mod types;
mod union_frame;
mod values;

pub use block_storage::{PalettedStorage, SubChunk};
pub use common::{Bitset, BlockPos, ChunkPos, Rgba, SubChunkPos, Uuid};
pub use entity_data::{EntityMetadata, MetadataValue};
pub use flags::{Flag, FlagSet};
pub use inventory::{
    ItemInstance, ItemStack, ItemType, MaterialReducer, MaterialReducerOutput, StackRequestItem,
};
pub use io::{Error, Result};
pub use login::ConnectionRequest;
pub use types::*;
pub use values::{AbilityValue, GameRule, GameRuleValue, PackSetting, PackSettingValue};

pub(crate) use block_storage::{BLOCKS, WIDTHS, word_count};
pub(crate) use io::{Codec, MAX_LIST_LEN, Reader, Writer, length, length_max, map_charges};

#[cfg(test)]
pub(crate) use io::tests::{Rng, fuzz, fuzz_round_trip};
