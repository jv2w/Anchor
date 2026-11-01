//! World events (PMMP `pocketmine\event\world`): worlds made, loaded and unloaded, chunks,
//! saving, and the sounds and particles the game plays.
//!
//! Only the direct API changes the difficulty and moves the spawn, and direct calls raise no
//! events, so there is no `WorldDifficultyChangeEvent` or `SpawnChangeEvent`. For the same reason
//! [`Context::add_sound`](crate::Context::add_sound) and
//! [`add_particle`](crate::Context::add_particle) raise no [`WorldSound`] or [`WorldParticle`].

use crate::math::Vec3;
use crate::player;
use crate::world::{self, ChunkPos, Particle, Sound};

/// Raised when [`Context::create_world`](crate::Context::create_world) made a world, before its
/// [`WorldLoad`], in the view phase of the tick it was made in (or the next one, for a world made
/// after that phase).
#[doc(alias = "WorldInitEvent")]
#[derive(Debug)]
pub struct WorldInit {
    pub(crate) world: world::Id,
}

getters!(WorldInit {
    /// The world.
    world: world::Id,
});

/// Raised when a world was made or loaded ([`Context::load_world`](crate::Context::load_world)),
/// in the view phase of the tick it happened in (or the next one), while its chunks load.
#[doc(alias = "WorldLoadEvent")]
#[derive(Debug)]
pub struct WorldLoad {
    pub(crate) world: world::Id,
}

getters!(WorldLoad {
    /// The world.
    world: world::Id,
});

/// Raised before a world asked to unload ([`Context::unload_world`](crate::Context::unload_world))
/// does, in the next view phase. Cancelling keeps it loaded, unless the unload was forced; a
/// listener that makes the world the default one keeps it either way. The monitors see only an
/// unload that happened, not cancelled, and the world gone: its players moved to the default
/// world's spawn, its chunks unloaded and its changes waiting for the next commit, which the
/// unload asks for. It stands for PMMP's unload callbacks (`World::addOnUnloadCallback`) too.
#[doc(alias = "WorldUnloadEvent")]
#[doc(alias = "World::addOnUnloadCallback")]
#[doc(alias = "World::removeOnUnloadCallback")]
#[derive(Debug)]
pub struct WorldUnload {
    pub(crate) world: world::Id,
    pub(crate) cancelled: bool,
}

getters!(WorldUnload {
    /// The world.
    world: world::Id,
});

/// Raised once a chunk is in the world, whether it was read from disk, made by the generator or
/// brought back from memory (unloaded with changes still waiting for a save).
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

/// Raised when a chunk no player needs is about to be unloaded. Cancelling keeps it loaded for a
/// while, then the listeners are asked again; once the listeners keep too many chunks, a cancel
/// is ignored and the chunk unloads with a warning
/// ([`Context::hold_chunks`](crate::Context::hold_chunks) keeps chunks until released). A world's
/// unload ([`WorldUnload`]) unloads its chunks without this event.
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

/// Raised when the game plays a sound at a place (PMMP `World::addSound`: a block used, an
/// explosion, a hit...), before the players hear it. Cancelling plays it to nobody.
#[doc(alias = "WorldSoundEvent")]
#[derive(Debug)]
pub struct WorldSound {
    pub(crate) world: world::Id,
    pub(crate) position: Vec3,
    pub(crate) sound: Sound,
    pub(crate) recipients: Vec<player::Id>,
    pub(crate) cancelled: bool,
}

getters!(WorldSound {
    /// The world.
    world: world::Id,
    /// Where it plays.
    #[doc(alias = "WorldSoundEvent::getPosition")]
    position: Vec3,
    /// The sound.
    #[doc(alias = "WorldSoundEvent::getSound")]
    sound: Sound,
    /// Who hears it: the players who see the place. Only those of them who still see it when the
    /// listeners are done hear it.
    #[doc(alias = "WorldSoundEvent::getRecipients")]
    recipients: &[player::Id],
});

setters!(WorldSound {
    /// Plays another sound instead.
    #[doc(alias = "WorldSoundEvent::setSound")]
    set_sound => sound: Sound,
    /// Replaces who hears it. Players who do not see the place are left out (PMMP).
    #[doc(alias = "WorldSoundEvent::setRecipients")]
    set_recipients => recipients: Vec<player::Id>,
});

/// Raised when the game shows a particle at a place (PMMP `World::addParticle`: a block broken,
/// a potion splashing...), before the players see it. Cancelling shows it to nobody.
#[doc(alias = "WorldParticleEvent")]
#[derive(Debug)]
pub struct WorldParticle {
    pub(crate) world: world::Id,
    pub(crate) position: Vec3,
    pub(crate) particle: Particle,
    pub(crate) recipients: Vec<player::Id>,
    pub(crate) cancelled: bool,
}

getters!(WorldParticle {
    /// The world.
    world: world::Id,
    /// Where it shows.
    #[doc(alias = "WorldParticleEvent::getPosition")]
    position: Vec3,
    /// The particle.
    #[doc(alias = "WorldParticleEvent::getParticle")]
    particle: Particle,
    /// Who sees it: the players who see the place. Only those of them who still see it when the
    /// listeners are done see it.
    #[doc(alias = "WorldParticleEvent::getRecipients")]
    recipients: &[player::Id],
});

setters!(WorldParticle {
    /// Shows another particle instead.
    #[doc(alias = "WorldParticleEvent::setParticle")]
    set_particle => particle: Particle,
    /// Replaces who sees it. Players who do not see the place are left out (PMMP).
    #[doc(alias = "WorldParticleEvent::setRecipients")]
    set_recipients => recipients: Vec<player::Id>,
});

cancellable! {
    ChunkUnload
        #[doc(alias = "ChunkUnloadEvent::cancel")]
        #[doc(alias = "ChunkUnloadEvent::uncancel")]
        #[doc(alias = "ChunkUnloadEvent::isCancelled")];
    WorldParticle
        #[doc(alias = "WorldParticleEvent::cancel")]
        #[doc(alias = "WorldParticleEvent::uncancel")]
        #[doc(alias = "WorldParticleEvent::isCancelled")];
    WorldSound
        #[doc(alias = "WorldSoundEvent::cancel")]
        #[doc(alias = "WorldSoundEvent::uncancel")]
        #[doc(alias = "WorldSoundEvent::isCancelled")];
    WorldUnload
        #[doc(alias = "WorldUnloadEvent::cancel")]
        #[doc(alias = "WorldUnloadEvent::uncancel")]
        #[doc(alias = "WorldUnloadEvent::isCancelled")];
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // pmmp: WorldSaveEvent, WorldEvent::getWorld
    #[test]
    fn a_world_save_names_its_world() {
        assert_eq!(WorldSave { world: WORLD }.world(), WORLD);
    }

    // pmmp: WorldSoundEvent::setSound, WorldSoundEvent::setRecipients, WorldSoundEvent::uncancel, WorldSoundEvent::isCancelled, WorldParticleEvent::setParticle, WorldParticleEvent::setRecipients, WorldParticleEvent::uncancel, WorldParticleEvent::isCancelled
    #[test]
    fn sound_and_particle_events_change_what_plays_and_who_hears_it() {
        let at = Vec3::new(1.0, 2.0, 3.0);
        let mut sound = WorldSound {
            world: WORLD,
            position: at,
            sound: Sound::Explode,
            recipients: vec![crate::event::fixtures::player(1)],
            cancelled: false,
        };
        assert_eq!((sound.world(), sound.position()), (WORLD, at));
        sound.set_sound(Sound::Burp);
        sound.set_recipients(Vec::new());
        assert_eq!(
            (sound.sound(), sound.recipients()),
            (Sound::Burp, [].as_slice())
        );
        sound.cancel();
        sound.uncancel();
        assert!(!sound.is_cancelled());
        let mut particle = WorldParticle {
            world: WORLD,
            position: at,
            particle: Particle::Flame,
            recipients: Vec::new(),
            cancelled: true,
        };
        particle.set_particle(Particle::Portal);
        particle.set_recipients(vec![crate::event::fixtures::player(2)]);
        assert_eq!(particle.particle(), Particle::Portal);
        assert_eq!(particle.recipients(), [crate::event::fixtures::player(2)]);
        assert_eq!((particle.world(), particle.position()), (WORLD, at));
        particle.uncancel();
        assert!(!particle.is_cancelled());
    }
}
