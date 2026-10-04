//! World events (PMMP `pocketmine\event\world`): chunks, saving and spawn.
//!
//! Worlds are fixed when the server is built, so PMMP's `WorldInitEvent`, `WorldLoadEvent` and
//! `WorldUnloadEvent` have nothing to announce. Only the direct API changes the difficulty, and
//! direct calls raise no events, so there is no `WorldDifficultyChangeEvent`; sounds and
//! particles reach players from many rules with no single place to raise `WorldSoundEvent` and
//! `WorldParticleEvent`, so the packet send hook filters them (PLAN.md 10.8 E2).

use crate::world::{self, BlockPos, ChunkPos};

/// Raised when a chunk is loaded from disk or made by the generator, after it is in the world.
#[doc(alias = "ChunkLoadEvent")]
#[doc(alias = "ChunkPopulateEvent")]
#[derive(Debug)]
pub struct ChunkLoad {
    pub(crate) world: world::Id,
    pub(crate) chunk: ChunkPos,
    pub(crate) new_chunk: bool,
}

getters!(ChunkLoad {
    /// The world of the chunk.
    world: world::Id,
    /// The chunk.
    #[doc(alias = "ChunkEvent::getChunkX")]
    #[doc(alias = "ChunkEvent::getChunkZ")]
    chunk: ChunkPos,
});

impl ChunkLoad {
    /// Whether the generator just made the chunk (PMMP's `ChunkPopulateEvent` too).
    #[doc(alias = "ChunkLoadEvent::isNewChunk")]
    pub fn is_new_chunk(&self) -> bool {
        self.new_chunk
    }
}

/// Raised when a chunk no player needs is about to be unloaded. Cancelling keeps it loaded until
/// the next check.
#[doc(alias = "ChunkUnloadEvent")]
#[derive(Debug)]
pub struct ChunkUnload {
    pub(crate) world: world::Id,
    pub(crate) chunk: ChunkPos,
    pub(crate) cancelled: bool,
}

getters!(ChunkUnload {
    /// The world of the chunk.
    world: world::Id,
    /// The chunk.
    chunk: ChunkPos,
});

/// Raised after a world's spawn moved.
#[doc(alias = "SpawnChangeEvent")]
#[derive(Debug)]
pub struct SpawnChange {
    pub(crate) world: world::Id,
    pub(crate) previous_spawn: BlockPos,
}

getters!(SpawnChange {
    /// The world.
    world: world::Id,
    /// Where the spawn was.
    #[doc(alias = "SpawnChangeEvent::getPreviousSpawn")]
    previous_spawn: BlockPos,
});

/// Raised after the world was saved with the server's state (every save, for each saved world).
#[doc(alias = "WorldSaveEvent")]
#[derive(Debug)]
pub struct WorldSave {
    pub(crate) world: world::Id,
}

getters!(WorldSave {
    /// The world.
    #[doc(alias = "WorldEvent::getWorld")]
    world: world::Id,
});

cancellable! {
    ChunkUnload
        #[doc(alias = "ChunkUnloadEvent::cancel")]
        #[doc(alias = "ChunkUnloadEvent::uncancel")]
        #[doc(alias = "ChunkUnloadEvent::isCancelled")];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::fixtures::POS;

    const WORLD: world::Id = world::Id::DEFAULT;
    const CHUNK: ChunkPos = ChunkPos::new(-3, 4);

    // pmmp: ChunkPopulateEvent, ChunkLoadEvent, ChunkUnloadEvent, ChunkEvent::getChunkX, ChunkEvent::getChunkZ, ChunkLoadEvent::isNewChunk, ChunkUnloadEvent::cancel, ChunkUnloadEvent::uncancel, ChunkUnloadEvent::isCancelled
    #[test]
    fn chunk_events_name_the_chunk() {
        let load = ChunkLoad {
            world: WORLD,
            chunk: CHUNK,
            new_chunk: true,
        };
        assert_eq!((load.world(), load.chunk()), (WORLD, CHUNK));
        assert!(load.is_new_chunk());
        let mut unload = ChunkUnload {
            world: WORLD,
            chunk: CHUNK,
            cancelled: false,
        };
        assert_eq!((unload.world(), unload.chunk()), (WORLD, CHUNK));
        unload.cancel();
        assert!(unload.is_cancelled());
        unload.uncancel();
        assert!(!unload.is_cancelled());
    }

    // pmmp: WorldSaveEvent, SpawnChangeEvent, SpawnChangeEvent::getPreviousSpawn, WorldEvent::getWorld
    #[test]
    fn world_changes_name_the_old_value() {
        let spawn = SpawnChange {
            world: WORLD,
            previous_spawn: POS,
        };
        assert_eq!((spawn.world(), spawn.previous_spawn()), (WORLD, POS));
        assert_eq!(WorldSave { world: WORLD }.world(), WORLD);
    }
}
