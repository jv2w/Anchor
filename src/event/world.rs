//! World events: worlds made, loaded and unloaded, chunks, saving, and the sounds and particles
//! the game plays.
//!
//! Only the direct API changes the difficulty and moves the spawn, and direct calls raise no
//! events, so there is no difficulty or spawn change event. For the same reason
//! [`Context::add_sound`](crate::Context::add_sound) and
//! [`add_particle`](crate::Context::add_particle) raise no [`WorldSound`] or [`WorldParticle`].

use crate::math::Vec3;
use crate::player;
use crate::world::{self, ChunkPos, Particle, Sound};

/// Raised once a chunk is in the world, whether it was read from disk, made by the generator or
/// brought back from memory (unloaded with changes still waiting for a save). A damaged saved
/// chunk is quarantined instead ([`World::is_quarantined`](crate::world::World::is_quarantined))
/// and raises none.
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
    /// Whether the generator just made the chunk.
    #[doc(alias = "ChunkLoadEvent::isNewChunk")]
    pub fn is_new_chunk(&self) -> bool {
        self.new_chunk
    }
}

/// Raised when a chunk that no player's view, spawn area or ticket holds is about to be unloaded.
/// Cancelling keeps it loaded for 600 ticks, then the listeners are asked again; while 1,024
/// chunks are kept this way, a further cancel is ignored: the chunk unloads with a warning and
/// the monitors see no cancel. A listener that holds the chunk instead
/// ([`Context::hold_chunks`](crate::Context::hold_chunks) keeps chunks until released) keeps it
/// too, which the monitors see as a cancel. A world's unload ([`WorldUnload`]) raises it for each
/// chunk the world loaded; a cancel changes nothing.
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

cancellable! {
    ChunkUnload
        #[doc(alias = "ChunkUnloadEvent::cancel")]
        #[doc(alias = "ChunkUnloadEvent::uncancel")]
        #[doc(alias = "ChunkUnloadEvent::isCancelled")];
}

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

/// Raised when the game shows a particle at a place (a block broken, a potion splashing...),
/// before the players see it. Cancelling shows it to nobody. A particle made for a direct call
/// such as [`Context::add_particle`](crate::Context::add_particle) raises none, and neither do the
/// effects the game sends as level events: block cracking, the generic particles of a
/// projectile's hit and an explosion's.
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
    /// Replaces who sees it. Players who do not see the place are left out.
    #[doc(alias = "WorldParticleEvent::setRecipients")]
    set_recipients => recipients: Vec<player::Id>,
});

cancellable! {
    WorldParticle
        #[doc(alias = "WorldParticleEvent::cancel")]
        #[doc(alias = "WorldParticleEvent::uncancel")]
        #[doc(alias = "WorldParticleEvent::isCancelled")];
}

/// Raised after a save commit, for each persistent world the committed snapshot held that is still
/// loaded. A world unloaded before the commit is answered raises none, and neither does a world
/// made after the snapshot was taken.
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

/// Raised when the game plays a sound at a place (an explosion, a hit...), before the players
/// hear it. Cancelling plays it to nobody. A sound made for a direct call such as
/// [`Context::add_sound`](crate::Context::add_sound) raises none, and neither do an entity's own
/// sounds (landing, falling).
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
    /// Replaces who hears it. Players who do not see the place are left out.
    #[doc(alias = "WorldSoundEvent::setRecipients")]
    set_recipients => recipients: Vec<player::Id>,
});

cancellable! {
    WorldSound
        #[doc(alias = "WorldSoundEvent::cancel")]
        #[doc(alias = "WorldSoundEvent::uncancel")]
        #[doc(alias = "WorldSoundEvent::isCancelled")];
}

/// Raised before a world asked to unload ([`Context::unload_world`](crate::Context::unload_world))
/// does, in the next view phase. Cancelling keeps it loaded, unless the unload was forced; a
/// listener that makes the world the default one keeps it either way. The monitors see only an
/// unload that happened, not cancelled, and the world gone: its players moved to the default
/// world's spawn, its chunks unloaded and its changes waiting for the next commit, which the
/// unload asks for.
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

cancellable! {
    WorldUnload
        #[doc(alias = "WorldUnloadEvent::cancel")]
        #[doc(alias = "WorldUnloadEvent::uncancel")]
        #[doc(alias = "WorldUnloadEvent::isCancelled")];
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::event::fixtures::player;

    const WORLD: world::Id = world::Id::DEFAULT;
    const CHUNK: ChunkPos = ChunkPos::new(-3, 4);

    // api: ChunkPopulateEvent, ChunkLoadEvent, ChunkUnloadEvent, ChunkEvent::getChunkX,
    // api: ChunkEvent::getChunkZ, ChunkLoadEvent::isNewChunk, ChunkUnloadEvent::cancel,
    // api: ChunkUnloadEvent::uncancel, ChunkUnloadEvent::isCancelled
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

    // api: WorldSaveEvent, WorldEvent::getWorld
    #[test]
    fn a_world_save_names_its_world() {
        assert_eq!(WorldSave { world: WORLD }.world(), WORLD);
    }

    // api: WorldSoundEvent::setSound, WorldSoundEvent::setRecipients, WorldSoundEvent::uncancel,
    // api: WorldSoundEvent::isCancelled, WorldParticleEvent::setParticle,
    // api: WorldParticleEvent::setRecipients, WorldParticleEvent::uncancel,
    // api: WorldParticleEvent::isCancelled
    #[test]
    fn sound_and_particle_events_change_what_plays_and_who_hears_it() {
        let at = Vec3::new(1.0, 2.0, 3.0);
        let mut sound = WorldSound {
            world: WORLD,
            position: at,
            sound: Sound::Explode,
            recipients: vec![player(1)],
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
        particle.set_recipients(vec![player(2)]);
        assert_eq!(particle.particle(), Particle::Portal);
        assert_eq!(particle.recipients(), [player(2)]);
        assert_eq!((particle.world(), particle.position()), (WORLD, at));
        particle.uncancel();
        assert!(!particle.is_cancelled());
    }
}
