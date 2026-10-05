//! The public world API on [`Context`] (PLAN.md 10.8, PMMP `WorldManager`, `World` and
//! `Explosion`): the worlds by [`world::Id`], the players in a world or who see a chunk, the
//! world clock, the weather and lightning (PLAN.md 10.9, [`weather`]), sounds,
//! particles and packets for those who see a place, explosions, and the
//! [`World`] queries that are rule code (the respawn rule's safe spawn, the entity rules'
//! collision boxes). The recipe lookups are in `recipes` (PMMP `CraftingManager`).
//!
//! Sounds, particles and packets go out at once from the call (no queue): each is one packet per
//! player who was sent the chunk of the position, queued in the player's outbox for the end of the
//! tick. A player whose outbox is nearly full (a loop of effects) is skipped rather than
//! disconnected: effects are cosmetic, and the outbox keeps [`EFFECT_RESERVE`] packets for the
//! rules.

mod lifecycle;
mod recipes;

pub(crate) use lifecycle::WorldSlot;
pub(super) use lifecycle::{Catalog, difficulty, difficulty_of, step, take_new_worlds};

use super::view::chunk_of;
use super::{Context, MAX_OUTBOX, Phase, State, entity, survival, weather};
use crate::math::{Aabb, Vec3};
use crate::player;
use crate::protocol::packet::Packet;
use crate::world::{self, BlockPos, ChunkPos, Lightning, Particle, Sound, Weather, World};

/// Outbox room that sounds, particles and API packets leave to the rules: a player is not sent
/// one when its outbox holds more than `MAX_OUTBOX - EFFECT_RESERVE` packets.
pub(super) const EFFECT_RESERVE: usize = 1024;

/// The most blocks a box of [`World::collision_boxes`] spans on each axis: at most 34 × 33 × 33
/// cells are looked at, with the cells under and over it.
const MAX_COLLISION_SPAN: f64 = 32.0;

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

/// The world index of `world` if the server has it loaded.
pub(super) fn index(state: &State, world: world::Id) -> Option<usize> {
    let i = world.index();
    state.catalog.is_loaded(i).then_some(i)
}

impl Context<'_> {
    /// The world `id`, if it is loaded (the default world always is).
    #[doc(alias = "WorldManager::getWorld")]
    #[doc(alias = "Position::isValid")]
    pub fn world(&self, id: world::Id) -> Option<&World> {
        self.state.worlds.get(index(self.state, id)?)
    }

    /// The world `id`, to change it.
    pub fn world_mut(&mut self, id: world::Id) -> Option<&mut World> {
        let i = index(self.state, id)?;
        self.state.worlds.get_mut(i)
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

    /// The players in `world` who have their saved data (joining or spawned), in `Id` order.
    #[doc(alias = "World::getPlayers")]
    pub fn players_in(&self, world: world::Id) -> impl Iterator<Item = player::Id> + '_ {
        let w = index(self.state, world).unwrap_or(usize::MAX);
        self.state
            .players
            .iter()
            .filter(move |(_, p)| p.world == w && p.phase != Phase::Loading)
            .map(|(&s, _)| player::Id::new(s))
    }

    /// The players in `world` who were sent the chunk `chunk` (they see what happens in it), in
    /// `Id` order: those [`Self::broadcast_packet`] reaches there. For the viewers of a block,
    /// pass [`BlockPos::chunk`]. None for a world the server does not have.
    #[doc(alias = "World::getChunkPlayers")]
    #[doc(alias = "World::getViewersForPosition")]
    pub fn chunk_viewers(
        &self,
        world: world::Id,
        chunk: ChunkPos,
    ) -> impl Iterator<Item = player::Id> + '_ {
        let w = index(self.state, world).unwrap_or(usize::MAX);
        self.state
            .players
            .iter()
            .filter(move |(_, p)| p.world == w && p.view.sees(chunk))
            .map(|(&s, _)| player::Id::new(s))
    }

    /// Sets the time of `world` to `time_ticks` (its clock; the day cycle follows it) and sends
    /// its players the clock at once. False for a world the server does not have.
    #[doc(alias = "World::setTime")]
    pub fn set_time(&mut self, world: world::Id, time_ticks: i64) -> bool {
        let Some(w) = index(self.state, world) else {
            return false;
        };
        if let Some(world) = self.state.worlds.get_mut(w) {
            world.set_time(time_ticks);
        }
        let packet = survival::clock_state(self.state, w);
        send_to_world(self.state, w, &packet);
        true
    }

    /// Sets the weather of `world` and tells its players at once (the level events BDS sends
    /// for the change; none for the weather it has). It stays until changed: Anchor has no
    /// natural weather. Players who join or arrive later are told when they get there. False for
    /// a world that is not loaded.
    pub fn set_weather(&mut self, world: world::Id, weather: Weather) -> bool {
        let Some(w) = index(self.state, world) else {
            return false;
        };
        let Some(world) = self.state.worlds.get_mut(w) else {
            return false;
        };
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

    /// Plays `sound` at `pos` in `world` to the players who see its chunk (PMMP
    /// `World::addSound`). False for a world the server does not have or a `pos` that is not
    /// finite.
    #[doc(alias = "World::addSound")]
    pub fn add_sound(&mut self, world: world::Id, pos: Vec3, sound: Sound) -> bool {
        pos.is_finite() && self.broadcast_packet(world, pos, &sound.packet(pos))
    }

    /// Shows `particle` at `pos` in `world` to the players who see its chunk (PMMP
    /// `World::addParticle`). False for a world the server does not have or a `pos` that is not
    /// finite.
    #[doc(alias = "World::addParticle")]
    pub fn add_particle(&mut self, world: world::Id, pos: Vec3, particle: Particle) -> bool {
        pos.is_finite() && self.broadcast_packet(world, pos, &particle.packet(pos))
    }

    /// Sends `packet` to the players who see the chunk of `pos` in `world` (PMMP
    /// `World::broadcastPacketToViewers`). False for a world the server does not have or a `pos`
    /// that is not finite or outside the world border.
    #[doc(alias = "World::broadcastPacketToViewers")]
    pub fn broadcast_packet(&mut self, world: world::Id, pos: Vec3, packet: &Packet) -> bool {
        let Some(w) = index(self.state, world) else {
            return false;
        };
        if !entity::is_valid_position(pos) {
            return false;
        }
        let chunk = chunk_of(pos);
        let room = MAX_OUTBOX - EFFECT_RESERVE;
        for p in self.state.players.values_mut() {
            if p.world == w && p.view.sees(chunk) && p.outbox.len() < room {
                p.send_packet(packet.clone());
            }
        }
        true
    }

    /// An explosion of `size` (at most 8) at `center` in `world` (PMMP `Explosion`, `explodeA`
    /// then `explodeB`): blocks break and drop with a 1 / size chance, TNT it reaches is lit, and
    /// entities and players near it are hurt and pushed by their exposure. It stops at chunks
    /// that are not loaded. False, with nothing done, for a world the server does not have, a
    /// `center` that is not finite or outside the world border, or a size that is not above 0.
    /// Like every direct call it raises no events (the entity hits and pushes included);
    /// [`action::Explode`](crate::action::Explode) explodes with them.
    #[doc(alias = "Explosion::__construct")]
    #[doc(alias = "Explosion::explodeA")]
    #[doc(alias = "Explosion::explodeB")]
    pub fn explode(&mut self, world: world::Id, center: Vec3, size: f64) -> bool {
        self.explosion(world, center, size, false)
    }

    /// [`Self::explode`] that also sets a third of the broken blocks on fire (PMMP's default fire
    /// chance) where the block under them can hold fire.
    #[doc(alias = "Explosion::setFireChance")]
    pub fn explode_with_fire(&mut self, world: world::Id, center: Vec3, size: f64) -> bool {
        self.explosion(world, center, size, true)
    }

    /// [`Self::explode`], with or without fire.
    fn explosion(&mut self, world: world::Id, center: Vec3, size: f64, fire: bool) -> bool {
        let Some(w) = index(self.state, world) else {
            return false;
        };
        if !entity::is_valid_position(center) || !size.is_finite() || size <= 0.0 {
            return false;
        }
        entity::explode(self.state, w, center, size, fire, entity::Raise::Quiet);
        true
    }
}

impl World {
    /// Where a player two blocks tall stands safely at the column of `pos`: the first block at
    /// or above `pos` (its y kept in the world height) that is free with a free block over it,
    /// free meaning not solid or in a chunk that is not loaded. The respawn rule uses the same
    /// test (PMMP `World::getSafeSpawn`; PMMP also looks down first, and loads the chunk).
    #[doc(alias = "World::getSafeSpawn")]
    pub fn safe_spawn(&self, pos: BlockPos) -> BlockPos {
        let free = |y: i32| {
            self.block(BlockPos::new(pos.x, y, pos.z))
                .is_none_or(|b| !b.is_solid())
        };
        let mut y = pos.y.clamp(world::MIN_Y, world::MAX_Y);
        while y <= world::MAX_Y && !(free(y) && free(y.saturating_add(1))) {
            y = y.saturating_add(1);
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

#[cfg(test)]
mod tests;
