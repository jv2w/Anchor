//! The public world API on [`Context`] (PMMP `WorldManager`, `World` and `Explosion`): the worlds
//! by [`world::Id`], the players in a world or who see a chunk, the world clock, the weather and
//! lightning ([`weather`]), sounds, particles and packets for those who see a place, explosions,
//! and the [`World`] queries that are rule code (the respawn rule's safe spawn, the entity rules'
//! collision boxes). The recipe lookups are in `recipes` (PMMP `CraftingManager`).
//!
//! Sounds, particles and packets go out at once from the call (no queue): each is one packet per
//! player who was sent the chunk of the position, queued in the player's outbox for the end of the
//! tick. A player whose outbox is nearly full (a loop of effects) is skipped rather than
//! disconnected, as for every cosmetic packet a player gets as a viewer: the outbox keeps
//! [`EFFECT_RESERVE`] packets for the player's own.

mod lifecycle;
mod recipes;

pub(crate) use lifecycle::WorldSlot;
pub(super) use lifecycle::{
    Catalog, difficulty, difficulty_of, step, take_deleted_worlds, take_new_worlds,
};

use super::view::chunk_of;
use super::{Context, Phase, State, entity, survival, weather};
use crate::math::{Aabb, Vec3};
use crate::player;
use crate::protocol::packet::Packet;
use crate::world::{self, BlockPos, ChunkPos, Lightning, Particle, Sound, Weather, World};

/// Outbox room that the cosmetic packets a player gets as a viewer (sounds, particles, API
/// packets, the swings and hit sounds of others) leave to the player's own packets: none is sent
/// once its outbox holds `MAX_OUTBOX - EFFECT_RESERVE` packets
/// ([`super::Player::send_to_viewer`]).
pub(super) const EFFECT_RESERVE: usize = 1024;

/// The most blocks a box of [`World::collision_boxes`] spans on each axis: it overlaps at most 33
/// cells on each axis, and the cells under and over it are looked at too (33 × 35 × 33 cells, x,
/// y, z).
const MAX_COLLISION_SPAN: f64 = 32.0;

impl Context<'_> {
    /// The world `id`, if it is loaded (the default world always is).
    #[doc(alias = "WorldManager::getWorld")]
    #[doc(alias = "Position::isValid")]
    pub fn world(&self, id: world::Id) -> Option<&World> {
        Some(world_at(self.state, index(self.state, id)?))
    }

    /// The world `id`, to change it, if it is loaded (the default world always is).
    pub fn world_mut(&mut self, id: world::Id) -> Option<&mut World> {
        let i = index(self.state, id)?;
        Some(world_at_mut(self.state, i))
    }

    /// Every loaded world, in `Id` order (the settings' world first).
    #[doc(alias = "WorldManager::getWorlds")]
    pub fn worlds(&self) -> impl Iterator<Item = &World> + '_ {
        let catalog = &self.state.catalog;
        self.state
            .worlds
            .iter()
            .enumerate()
            .filter(|&(i, _)| catalog.is_loaded(i))
            .map(|(_, w)| w)
    }

    /// The players in `world` who have their saved data (joining or spawned), in `Id` order;
    /// nobody for a world that is not loaded. A player teleported this tick is in its new
    /// [`world`](crate::player::Player::world).
    #[doc(alias = "World::getPlayers")]
    pub fn players_in(&self, world: world::Id) -> impl Iterator<Item = player::Id> + '_ {
        let loaded = index(self.state, world).is_some();
        self.state
            .players
            .iter()
            .filter(move |(_, p)| loaded && p.world() == world && p.phase != Phase::Loading)
            .map(|(&s, _)| player::Id::new(s))
    }

    /// The players in `world` who were sent the chunk `chunk` (they see what happens in it), in
    /// `Id` order; nobody for a world that is not loaded. [`Self::broadcast_packet`] sends there to
    /// each of them whose outbox is not nearly full. For the viewers of a block, pass
    /// [`BlockPos::chunk`].
    #[doc(alias = "World::getChunkPlayers")]
    #[doc(alias = "World::getViewersForPosition")]
    pub fn chunk_viewers(
        &self,
        world: world::Id,
        chunk: ChunkPos,
    ) -> impl Iterator<Item = player::Id> + '_ {
        let w = index(self.state, world);
        self.state
            .players
            .iter()
            .filter(move |(_, p)| Some(p.world) == w && p.view.sees(chunk))
            .map(|(&s, _)| player::Id::new(s))
    }

    /// Sets the time of `world` to `time_ticks` (its clock; the day cycle follows it) and sends
    /// its players the clock at once. False for a world that is not loaded.
    #[doc(alias = "World::setTime")]
    pub fn set_time(&mut self, world: world::Id, time_ticks: i64) -> bool {
        let Some(w) = index(self.state, world) else {
            return false;
        };
        world_at_mut(self.state, w).set_time(time_ticks);
        let packet = survival::clock_state(self.state, w);
        send_to_world(self.state, w, &packet);
        true
    }

    /// Sets the weather of `world` and tells its players at once (the level events BDS sends
    /// for the change; none for the weather it has). It stays until changed or the server stops:
    /// Anchor has no natural weather and does not save it, so every world is clear at start
    /// ([`Weather`] shows how to build a cycle and keep it). Raises no event. Players who join or
    /// arrive later are told when they get there. False for a world that is not loaded.
    pub fn set_weather(&mut self, world: world::Id, weather: Weather) -> bool {
        let Some(w) = index(self.state, world) else {
            return false;
        };
        let world = world_at_mut(self.state, w);
        let from = world.weather();
        world.set_weather(weather);
        for packet in weather::changes(from, weather) {
            send_to_world(self.state, w, &packet);
        }
        true
    }

    /// Strikes a lightning bolt at `pos` in `world`, as BDS's `summon lightning_bolt`: the
    /// players who see its chunk see the bolt and hear its thunder, and it goes after a second.
    /// `lightning` says whether it also hurts and sets fire ([`Lightning`]); like every direct
    /// call it raises no events. It strikes where it is told, even under a roof. False, with
    /// nothing done, for a world that is not loaded, a `pos` that is not finite or outside the
    /// world border, or 1,024 bolts still showing.
    pub fn strike_lightning(&mut self, world: world::Id, pos: Vec3, lightning: Lightning) -> bool {
        index(self.state, world)
            .is_some_and(|w| entity::strike_lightning(self.state, w, pos, lightning))
    }

    /// Plays `sound` at `pos` in `world` to the players who see its chunk, as
    /// [`Self::broadcast_packet`] sends (PMMP `World::addSound`). False for a world that is not
    /// loaded or a `pos` that is not finite or outside the world border.
    #[doc(alias = "World::addSound")]
    pub fn add_sound(&mut self, world: world::Id, pos: Vec3, sound: Sound) -> bool {
        self.broadcast_packet(world, pos, &sound.packet(pos))
    }

    /// Shows `particle` at `pos` in `world` to the players who see its chunk, as
    /// [`Self::broadcast_packet`] sends (PMMP `World::addParticle`). False for a world that is not
    /// loaded or a `pos` that is not finite or outside the world border.
    #[doc(alias = "World::addParticle")]
    pub fn add_particle(&mut self, world: world::Id, pos: Vec3, particle: Particle) -> bool {
        self.broadcast_packet(world, pos, &particle.packet(pos))
    }

    /// Sends `packet` to the players who see the chunk of `pos` in `world` (PMMP
    /// `World::broadcastPacketToViewers`). A player whose outbox is nearly full is skipped, as for
    /// every cosmetic packet; [`Player::send_packet`](crate::player::Player::send_packet) sends
    /// what must arrive. False for a world that is not loaded or a `pos` that is not finite or
    /// outside the world border.
    #[doc(alias = "World::broadcastPacketToViewers")]
    pub fn broadcast_packet(&mut self, world: world::Id, pos: Vec3, packet: &Packet) -> bool {
        let Some(w) = index(self.state, world) else {
            return false;
        };
        if !entity::is_valid_position(pos) {
            return false;
        }
        self.state.broadcast(w, chunk_of(pos), packet, None);
        true
    }

    /// Explodes with `size` at `center` in `world` (PMMP `Explosion`, `explodeA` then
    /// `explodeB`): blocks break and drop with a 1 / size chance, TNT it reaches is lit, and
    /// entities and players near it are hurt and pushed by their exposure. A size above 8
    /// explodes as 8. Its rays stop at chunks that are not loaded and at blocks neither Anchor
    /// nor clients know; blocks only clients know weaken them and stay. False, with nothing done,
    /// for a world that is not loaded, a `center` that is not finite or outside the world border,
    /// or a size that is not finite or not above 0. Like every direct call it raises no events
    /// (the entity hits and pushes included); [`action::Explode`](crate::action::Explode)
    /// explodes with them.
    #[doc(alias = "Explosion::__construct")]
    #[doc(alias = "Explosion::explodeA")]
    #[doc(alias = "Explosion::explodeB")]
    pub fn explode(&mut self, world: world::Id, center: Vec3, size: f64) -> bool {
        self.explosion(world, center, size, false)
    }

    /// Explodes as [`Self::explode`] does, and sets a third of the broken blocks on fire (PMMP's
    /// default fire chance) where the block under them can hold fire.
    #[doc(alias = "Explosion::setFireChance")]
    pub fn explode_with_fire(&mut self, world: world::Id, center: Vec3, size: f64) -> bool {
        self.explosion(world, center, size, true)
    }

    /// Explodes as [`Self::explode`] does, setting fire when `fire`.
    fn explosion(&mut self, world: world::Id, center: Vec3, size: f64, fire: bool) -> bool {
        let Some(w) = index(self.state, world) else {
            return false;
        };
        if !entity::is_valid_explosion(center, size) {
            return false;
        }
        entity::explode(self.state, w, center, size, fire, entity::Raise::Quiet);
        true
    }
}

impl World {
    /// Where a player two blocks tall stands safely at the column of `pos`: the first block at
    /// or above `pos` (its y first kept in the world height) that is free with a free block over
    /// it, free meaning that an entity can be in it, as the entity rules see it; every block of a
    /// chunk that is not loaded is free. A column with no such room gives the y one above the
    /// world height. The respawn rule uses it (PMMP `World::getSafeSpawn`; PMMP also looks down
    /// first, and loads the chunk).
    #[doc(alias = "World::getSafeSpawn")]
    pub fn safe_spawn(&self, pos: BlockPos) -> BlockPos {
        let mut y = pos.y.clamp(world::MIN_Y, world::MAX_Y);
        if self.is_chunk_loaded(pos.chunk()) {
            // Each step looks at one new block, the one over the block below it.
            let mut free = !entity::is_solid(self, pos.x, y, pos.z);
            while y <= world::MAX_Y {
                let free_above = !entity::is_solid(self, pos.x, y + 1, pos.z);
                if free && free_above {
                    break;
                }
                free = free_above;
                y += 1;
            }
        }
        BlockPos::new(pos.x, y, pos.z)
    }

    /// The collision boxes of the blocks that overlap `area`, where they are in the world (PMMP
    /// `World::getBlockCollisionBoxes`), as the entity rules see them: a chunk that is not loaded
    /// is a full cube in every cell. Entities in a box are
    /// [`Context::entities_in`](crate::Context::entities_in). `None` if a corner of `area` is not
    /// finite or outside the world border, or `area` spans more than 32 blocks on an axis.
    #[doc(alias = "World::getBlockCollisionBoxes")]
    pub fn collision_boxes(&self, area: Aabb) -> Option<Vec<Aabb>> {
        let span = area.max - area.min;
        let small = [span.x, span.y, span.z]
            .iter()
            .all(|s| *s <= MAX_COLLISION_SPAN);
        if !(entity::is_valid_position(area.min) && entity::is_valid_position(area.max) && small) {
            return None;
        }
        let mut boxes = Vec::new();
        entity::collisions(self, area, &mut boxes);
        Some(boxes)
    }
}

/// The world index of `world` if the server has it loaded.
pub(super) fn index(state: &State, world: world::Id) -> Option<usize> {
    let i = world.index();
    state.catalog.is_loaded(i).then_some(i)
}

/// Queues `packet` for every player of `world` past its start-game packets. A change made while
/// a player waits for its spawn chunks reaches it too, after its `StartGame` (the outbox follows
/// the start-game packets); a spawn window that then repeats the same state is harmless.
pub(super) fn send_to_world(state: &mut State, world: usize, packet: &Packet) {
    for p in state.players.values_mut() {
        if p.world == world && p.phase != Phase::Loading {
            p.send_packet(packet.clone());
        }
    }
}

/// The world at `index`, an index of a world the server has had this run.
fn world_at(state: &State, index: usize) -> &World {
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: the server keeps every world it had this run at its index"
    )]
    &state.worlds[index]
}

/// The world at `index`, an index of a world the server has had this run, to change it.
fn world_at_mut(state: &mut State, index: usize) -> &mut World {
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: the server keeps every world it had this run at its index"
    )]
    &mut state.worlds[index]
}

#[cfg(test)]
mod tests;
