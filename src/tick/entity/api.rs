//! The public entity API.
//!
//! It is what an [`Entity`] shows, what a [`Mut`] changes, and the [`Context`] methods that find,
//! lend and spawn entities: dropped items, orbs, falling blocks, TNT, projectiles, clouds,
//! paintings, end crystals, firework rockets, and villager, human, zombie and custom NPCs.
//!
//! Setters change the entity at once, mark its chunk for the next save and, for what viewers
//! draw (name tag, scale, kind values), mark it for the next movement pass, which sends it with
//! the position, motion, rotation and flags it compares anyway. No setter raises an event
//! (direct calls do not re-enter). A spawned entity is found by [`Context::entity`] and
//! [`Context::entity_mut`] at once, though a runtime ID comes at the next entity tick.
//!
//! Lookups go through the store's slots and indexes: an [`Id`] is O(1), a runtime ID O(log n),
//! "what is near" visits the chunk index, and the entities of a world are the store's slots in
//! `Id` order.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Deref;

use super::kind::{
    Arrow, Character, Cloud, Falling, Form, Item, Kind, MAX_DESPAWN_DELAY_TICKS,
    MAX_NAME_TAG_BYTES, NEVER, Orb, Projectile, Shot, Tnt, Trident,
};
use super::{
    Cause, EYE_HEIGHT, Entities, Entity, Hurt, Knock, MAX_ENTITIES_PER_WORLD,
    MAX_HIDDEN_PER_PLAYER, Npc, Raise, SessionId, State, chunk_of, create, drop_stack, hurt_in,
    is_valid_position, is_valid_scale, item_despawn_delay_ticks, object, physics, projectile,
    send_to,
};
use crate::action;
use crate::block::{self, Face};
use crate::effect::{Instance, Potion};
use crate::entity::{Animation, ArrowPickup, Error, Id, PaintingMotive, Result, Skin, Type};
use crate::event::Actor;
use crate::item::Stack;
use crate::math::{self, Aabb, Vec3, to_f32};
use crate::nbt::Compound;
use crate::player;
use crate::random::{self, Random};
use crate::save::WorldIndex;
use crate::tick::world_api::{Worlds, index};
use crate::tick::{Context, Phase, Player, survival};
use crate::world::{self, BlockPos, ChunkPos, World};

/// The random stream of a rocket the API launches.
const LAUNCH_RANDOM: u64 = random::tag("api-firework-launch");

/// An entity lent to change ([`Context::entity_mut`]); it reads as the [`Entity`] it changes.
/// Each setter changes the entity at once and fails, changing nothing, for a value it does not
/// take or a kind the value does not apply to.
pub struct Mut<'a> {
    id: Id,
    entities: &'a mut Entities,
    worlds: &'a Worlds,
    players: &'a mut BTreeMap<SessionId, Player>,
    /// The despawn delay a new dropped item gets now, which a dropped item may be set past.
    item_despawn_delay_ticks: i32,
}

impl Mut<'_> {
    /// Its ID.
    pub fn id(&self) -> Id {
        self.id
    }

    /// Hurts it by `amount`, as damage dealt through the API, without the event.
    /// An NPC follows its hit cooldown and shows the hit, and dies at 0 health; anything else
    /// that reaches 0 is removed. Falling blocks, primed TNT, projectiles and invulnerable NPCs
    /// take no damage but the void's. False if nothing was taken, as for an amount that is below
    /// 0 or above [`MAX_DAMAGE`](crate::event::MAX_DAMAGE), or not finite.
    #[doc(alias = "Entity::attack")]
    #[doc(alias = "Living::attack")]
    pub fn attack(&mut self, amount: f64) -> bool {
        let id = self.id;
        let key = (self.world, self.chunk);
        let result = hurt_in(self.entities, self.players, id, Cause::Custom, amount, None);
        self.entities.changed.insert(key);
        result != Hurt::Ignored
    }

    /// Removes it at the end of the tick, with its removal sent to its viewers.
    /// Nothing if it is gone already.
    #[doc(alias = "Entity::flagForDespawn")]
    #[doc(alias = "Entity::close")]
    pub fn flag_for_despawn(&mut self) {
        let id = self.id;
        self.entities.doom(id);
    }

    /// Adds `amount` to its health, at most its maximum.
    /// False for an amount that is not finite or below 0, and for an entity that does not live.
    #[doc(alias = "Entity::heal")]
    pub fn heal(&mut self, amount: f64) -> bool {
        amount.is_finite() && amount >= 0.0 && self.set_health(self.health + amount).is_ok()
    }

    /// Sets its health, at most its maximum.
    /// 0 or less kills it.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for a value that is not a number.
    /// [`Error::NotAlive`] for an entity that does not live (a dying NPC).
    #[doc(alias = "Entity::setHealth")]
    #[doc(alias = "Living::setHealth")]
    pub fn set_health(&mut self, health: f64) -> Result<()> {
        if health.is_nan() {
            return Err(Error::OutOfRange);
        }
        if !self.is_alive() {
            return Err(Error::NotAlive);
        }
        if health <= 0.0 {
            self.kill();
            return Ok(());
        }
        let e = self.entity_mut();
        e.health = math::min(health, e.kind.max_health());
        Ok(())
    }

    /// Kills it: its health becomes 0.
    /// An NPC shows its death and goes 25 ticks later, a hurt end crystal explodes at its next
    /// tick, anything else goes at the end of the tick (a painting drops its item). False, doing
    /// nothing, for an entity that does not live.
    #[doc(alias = "Entity::kill")]
    pub fn kill(&mut self) -> bool {
        if !self.is_alive() {
            return false;
        }
        let id = self.id;
        let (e, players) = self.parts();
        e.health = 0.0;
        if let Kind::Npc(c) = &mut e.kind {
            c.dead_ticks = Some(0);
            send_to(players, &e.viewers, &Animation::Death.packet(e.runtime_id));
        } else if !matches!(&e.kind, Kind::EndCrystal(c) if c.primed) {
            self.entities.doom(id);
        }
        self.entities.note_ending(id);
        true
    }

    /// Knocks a living NPC back away along (`x`, `z`) with `force`.
    /// It goes up at most `vertical_limit`; the usual force and limit are 0.4 and 0.4. False for
    /// another kind, values that are not finite, or no horizontal direction.
    #[doc(alias = "Living::knockBack")]
    pub fn knock_back(&mut self, x: f64, z: f64, force: f64, vertical_limit: f64) -> bool {
        if !self.kind.is_living() || ![x, z, force, vertical_limit].iter().all(|v| v.is_finite()) {
            return false;
        }
        let knock = Knock {
            x,
            z,
            force,
            vertical: vertical_limit,
        };
        let Some(motion) = knock.applied_to(self.motion()) else {
            return false;
        };
        self.set_motion(motion).is_ok()
    }

    /// Turns its eyes to `target`. False for a target that is not finite.
    #[doc(alias = "Living::lookAt")]
    pub fn look_at(&mut self, target: Vec3) -> bool {
        if !target.is_finite() {
            return false;
        }
        let (at, eye) = (self.position(), self.eye_height());
        let (dx, dz) = (target.x - at.x, target.z - at.z);
        let horizontal = (dx * dx + dz * dz).sqrt();
        let vertical = target.y - (at.y + eye);
        let pitch = -math::degrees(math::atan2(vertical, horizontal));
        let mut yaw = math::degrees(math::atan2(dz, dx)) - 90.0;
        if yaw < 0.0 {
            yaw += 360.0;
        }
        self.set_rotation(yaw, pitch).is_ok()
    }

    /// Makes a villager or a zombie a baby or a grown-up.
    /// Viewers see the change (the `Baby` flag).
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind.
    pub fn set_baby(&mut self, baby: bool) -> Result<()> {
        match &mut self.shown_mut().kind {
            Kind::Npc(Character {
                form: Form::Villager { baby: b, .. } | Form::Zombie { baby: b },
                ..
            }) => {
                *b = baby;
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets a projectile's base damage, from 0 to the most an arrow of a strong bow deals.
    /// That is an arrow shot from a bow of Power
    /// [`MAX_ENCHANTMENT_LEVEL`](crate::item::MAX_ENCHANTMENT_LEVEL). A potion, an xp bottle and
    /// an ice bomb deal no damage, so it changes nothing for them.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for a damage outside that range.
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "Projectile::setBaseDamage")]
    pub fn set_base_damage(&mut self, damage: f64) -> Result<()> {
        if !(0.0..=Projectile::MAX_DAMAGE).contains(&damage) {
            return Err(Error::OutOfRange);
        }
        match &mut self.entity_mut().kind {
            Kind::Projectile(p) => {
                p.damage = damage;
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets whether an end crystal shows its bedrock base.
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "entity\\object\\EndCrystal::setShowBase")]
    pub fn set_base_visible(&mut self, visible: bool) -> Result<()> {
        match &mut self.entity_mut().kind {
            Kind::EndCrystal(c) => {
                c.show_base = visible;
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets the block an end crystal's beam points at, or none.
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "entity\\object\\EndCrystal::setBeamTarget")]
    pub fn set_beam_target(&mut self, target: Option<BlockPos>) -> Result<()> {
        match &mut self.shown_mut().kind {
            Kind::EndCrystal(c) => {
                c.beam_target = target;
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets whether it is saved with its chunk (a minigame's entities need not be).
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for `true` on a kind that is never saved with its chunk: a firework
    /// rocket, or an entity of a temporary custom type.
    #[doc(alias = "Entity::setCanSaveWithChunk")]
    pub fn set_can_save_with_chunk(&mut self, saved: bool) -> Result<()> {
        if saved && !self.kind.is_saved() {
            return Err(Error::WrongKind);
        }
        self.entity_mut().saved = saved;
        Ok(())
    }

    /// Sets the change of a cloud's maximum age for each entity it affects, -24,000 to 24,000.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for another value.
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "AreaEffectCloud::setMaxAgeChangeOnUse")]
    pub fn set_cloud_max_age_change_on_use_ticks(&mut self, ticks: i32) -> Result<()> {
        if !(-Cloud::MAX_AGE_TICKS..=Cloud::MAX_AGE_TICKS).contains(&ticks) {
            return Err(Error::OutOfRange);
        }
        self.cloud_mut(|c| c.max_age_on_use = ticks)
    }

    /// Sets the age at which a cloud goes, 0 to 24,000 ticks (as the saved form reads it).
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for another value.
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "AreaEffectCloud::setMaxAge")]
    pub fn set_cloud_max_age_ticks(&mut self, ticks: i32) -> Result<()> {
        if !(0..=Cloud::MAX_AGE_TICKS).contains(&ticks) {
            return Err(Error::OutOfRange);
        }
        self.shown_cloud_mut(|c| c.max_age = ticks)
    }

    /// Sets the change of a cloud's radius when a bottle takes from it, -64 to 64 blocks.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for another value.
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "AreaEffectCloud::setRadiusChangeOnPickup")]
    pub fn set_cloud_radius_change_on_pickup(&mut self, blocks: f64) -> Result<()> {
        let blocks = f64::from(to_f32(blocks));
        if !is_radius_change(blocks) {
            return Err(Error::OutOfRange);
        }
        self.shown_cloud_mut(|c| c.radius_on_pickup = blocks)
    }

    /// Sets the change of a cloud's radius for each entity it affects, -64 to 64 blocks.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for another value.
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "AreaEffectCloud::setRadiusChangeOnUse")]
    pub fn set_cloud_radius_change_on_use(&mut self, blocks: f64) -> Result<()> {
        let blocks = f64::from(to_f32(blocks));
        if !is_radius_change(blocks) {
            return Err(Error::OutOfRange);
        }
        self.cloud_mut(|c| c.radius_on_use = blocks)
    }

    /// Sets the change of a cloud's radius per tick, -64 to 64 blocks.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for another value.
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "AreaEffectCloud::setRadiusChangePerTick")]
    pub fn set_cloud_radius_change_per_tick(&mut self, blocks: f64) -> Result<()> {
        let blocks = f64::from(to_f32(blocks));
        if !is_radius_change(blocks) {
            return Err(Error::OutOfRange);
        }
        self.cloud_mut(|c| c.radius_per_tick = blocks)
    }

    /// Sets the ticks before a cloud affects the same entity again, 0 to 24,000.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for another value.
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "AreaEffectCloud::setReapplicationDelay")]
    pub fn set_cloud_reapplication_delay_ticks(&mut self, ticks: i32) -> Result<()> {
        if !(0..=Cloud::MAX_AGE_TICKS).contains(&ticks) {
            return Err(Error::OutOfRange);
        }
        self.cloud_mut(|c| c.reapplication_delay = ticks)
    }

    /// Sets whether an arrow is critical.
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "entity\\projectile\\Arrow::setCritical")]
    pub fn set_critical(&mut self, critical: bool) -> Result<()> {
        self.arrow_mut(|a| a.critical = critical)
    }

    /// Sets a dropped item's or orb's ticks until it disappears.
    /// It is -1 never, otherwise 0 to 38,767 (what the saved `Age` holds); a dropped item may
    /// also take up to `item-despawn-ticks` when that is more.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for a value outside that.
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "ItemEntity::setDespawnDelay")]
    #[doc(alias = "ExperienceOrb::setDespawnDelay")]
    pub fn set_despawn_delay_ticks(&mut self, ticks: i32) -> Result<()> {
        let longest = match &self.kind {
            Kind::Item(_) => MAX_DESPAWN_DELAY_TICKS.max(self.item_despawn_delay_ticks),
            _ => MAX_DESPAWN_DELAY_TICKS,
        };
        if ticks != i32::from(NEVER) && !(0..=longest).contains(&ticks) {
            return Err(Error::OutOfRange);
        }
        match &mut self.entity_mut().kind {
            Kind::Item(Item {
                despawn_delay_ticks,
                ..
            })
            | Kind::XpOrb(Orb {
                despawn_delay_ticks,
                ..
            }) => {
                *despawn_delay_ticks = ticks;
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets the ticks it burns; 0 puts its fire out. Fails above 32,767.
    /// A fire-proof entity burns none. A direct call raises no event.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for ticks above 32,767.
    #[doc(alias = "Entity::setFireTicks")]
    #[doc(alias = "Entity::extinguish")]
    pub fn set_fire_ticks(&mut self, ticks: u32) -> Result<()> {
        let ticks = i16::try_from(ticks).ok().ok_or(Error::OutOfRange)?;
        let e = self.entity_mut();
        if !e.kind.is_fire_proof() {
            e.fire_ticks = ticks;
        }
        Ok(())
    }

    /// Sets a firework rocket's explosions (see [`Entity::firework_explosions`]).
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind.
    /// [`Error::OutOfRange`], changing nothing, for more than 256 explosions, a compound that is
    /// no explosion (a shape byte 0 to 4, at least one colour, colours 0 to 15) or one bigger than
    /// Anchor takes (more than 16 colours in a list, or a key other than `FireworkType`,
    /// `FireworkColor`, `FireworkFade`, `FireworkFlicker` and `FireworkTrail`).
    #[doc(alias = "entity\\object\\FireworkRocket::setExplosions")]
    pub fn set_firework_explosions(&mut self, explosions: Vec<Compound>) -> Result<()> {
        if !matches!(self.kind, Kind::Firework(_)) {
            return Err(Error::WrongKind);
        }
        let Some(explosions) = object::checked_explosions(explosions) else {
            return Err(Error::OutOfRange);
        };
        if let Kind::Firework(f) = &mut self.shown_mut().kind {
            f.explosions = explosions;
        }
        Ok(())
    }

    /// Sets primed TNT's ticks until it explodes, at most 32,767.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for a longer fuse.
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "PrimedTNT::setFuse")]
    pub fn set_fuse_ticks(&mut self, ticks: u16) -> Result<()> {
        if ticks > Tnt::MAX_FUSE {
            return Err(Error::OutOfRange);
        }
        match &mut self.shown_mut().kind {
            Kind::Tnt(t) => {
                t.fuse = ticks;
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets whether gravity pulls it.
    /// Flowing liquid still pushes the kinds it pushes; no setting stops that.
    #[doc(alias = "Entity::setHasGravity")]
    pub fn set_has_gravity(&mut self, gravity: bool) {
        let e = self.entity_mut();
        e.gravity = gravity;
        e.forced = true;
    }

    /// Sets whether clients draw it.
    #[doc(alias = "Entity::setInvisible")]
    pub fn set_invisible(&mut self, invisible: bool) {
        self.entity_mut().invisible = invisible;
    }

    /// Sets whether only the void hurts an NPC or a custom entity.
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind.
    pub fn set_invulnerable(&mut self, invulnerable: bool) -> Result<()> {
        match &mut self.entity_mut().kind {
            Kind::Npc(c) => {
                c.invulnerable = invulnerable;
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets the item a thrown trident holds, which a player who picks it up gets.
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind.
    /// [`Error::OutOfRange`] for an empty stack.
    #[doc(alias = "entity\\projectile\\Trident::setItem")]
    pub fn set_item(&mut self, item: Stack) -> Result<()> {
        let Kind::Projectile(Projectile {
            shot: Shot::Trident(_),
            ..
        }) = self.kind
        else {
            return Err(Error::WrongKind);
        };
        if item.is_empty() {
            return Err(Error::OutOfRange);
        }
        if let Kind::Projectile(Projectile {
            shot: Shot::Trident(t),
            ..
        }) = &mut self.entity_mut().kind
        {
            t.item = item;
        }
        Ok(())
    }

    /// Sets whether a thrown potion leaves a cloud.
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "entity\\projectile\\SplashPotion::setLinger")]
    pub fn set_lingering(&mut self, lingering: bool) -> Result<()> {
        match &mut self.entity_mut().kind {
            Kind::Projectile(Projectile {
                shot: Shot::Potion { linger, .. },
                ..
            }) => {
                *linger = lingering;
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets the age in ticks at which a firework rocket bursts.
    /// An age it has reached bursts it at its next tick. Its item shows the flight multiplier of
    /// the new time.
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "entity\\object\\FireworkRocket::setMaxFlightTimeTicks")]
    pub fn set_max_flight_time_ticks(&mut self, ticks: u32) -> Result<()> {
        match &mut self.shown_mut().kind {
            Kind::Firework(f) => {
                f.max_flight = ticks;
                f.flight = object::flight_multiplier(ticks);
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets its motion in blocks per tick, each component cut to ±10; it replaces the motion,
    /// not adds to it.
    /// A projectile stuck in a block stays there until the block goes.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for a motion that is not finite.
    #[doc(alias = "Entity::setMotion")]
    #[doc(alias = "Entity::addMotion")]
    pub fn set_motion(&mut self, motion: Vec3) -> Result<()> {
        if !motion.is_finite() {
            return Err(Error::OutOfRange);
        }
        let e = self.entity_mut();
        e.body.motion = physics::bounded(motion);
        e.forced = true;
        Ok(())
    }

    /// Sets its name tag (a human NPC's name).
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for a name over [`MAX_NAME_TAG_BYTES`].
    #[doc(alias = "Entity::setNameTag")]
    pub fn set_name_tag(&mut self, name: impl Into<String>) -> Result<()> {
        let name = name.into();
        if name.len() > MAX_NAME_TAG_BYTES {
            return Err(Error::OutOfRange);
        }
        self.shown_mut().name_tag = name;
        Ok(())
    }

    /// Sets whether the name tag always shows.
    #[doc(alias = "Entity::setNameTagAlwaysVisible")]
    pub fn set_name_tag_always_visible(&mut self, visible: bool) {
        self.shown_mut().name_tag_always_visible = visible;
    }

    /// Sets whether the name tag shows when a player looks at it.
    #[doc(alias = "Entity::setNameTagVisible")]
    pub fn set_name_tag_visible(&mut self, visible: bool) {
        self.shown_mut().name_tag_visible = visible;
    }

    /// Sets it on fire for `seconds`, at most 32,767 ticks.
    /// Nothing changes if it burns longer already or is fire-proof.
    #[doc(alias = "Entity::setOnFire")]
    #[doc(alias = "Living::setOnFire")]
    pub fn set_on_fire(&mut self, seconds: u32) {
        // A longer time burns the longest a burn lasts, as a player's does.
        let seconds = i16::try_from(seconds).unwrap_or(i16::MAX);
        self.entity_mut().set_on_fire(seconds);
    }

    /// Sets a dropped item's owner (a player name).
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for a name over [`MAX_NAME_TAG_BYTES`].
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "ItemEntity::setOwner")]
    pub fn set_owner(&mut self, owner: impl Into<String>) -> Result<()> {
        let owner = owner.into();
        if owner.len() > MAX_NAME_TAG_BYTES {
            return Err(Error::OutOfRange);
        }
        match &mut self.entity_mut().kind {
            Kind::Item(item) => {
                item.owner = owner;
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets who a projectile counts as shot or thrown by, or a firework rocket as launched by.
    /// A projectile's hit is credited to that player, and an ender pearl teleports it; none
    /// clears it.
    ///
    /// # Errors
    ///
    /// [`Error::NotOnline`] for a player who is not online.
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "Entity::setOwningEntity")]
    pub fn set_owning_player(&mut self, owner: Option<player::Id>) -> Result<()> {
        let runtime_id = match owner {
            Some(id) => match self.players.get(&id.session()) {
                Some(p) if p.phase != Phase::Loading => p.runtime_id,
                _ => return Err(Error::NotOnline),
            },
            None => 0,
        };
        match &mut self.shown_mut().kind {
            Kind::Projectile(p) => {
                p.owner = owner.map(player::Id::session);
                p.owner_runtime_id = runtime_id;
                Ok(())
            }
            Kind::Firework(f) => {
                f.owner = owner.map(player::Id::session);
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets a dropped item's ticks until a player may pick it up; -1 never.
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "ItemEntity::setPickupDelay")]
    pub fn set_pickup_delay_ticks(&mut self, ticks: i16) -> Result<()> {
        match &mut self.entity_mut().kind {
            Kind::Item(item) => {
                item.pickup_delay_ticks = ticks.max(NEVER);
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets who may pick up an arrow once stuck.
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "entity\\projectile\\Arrow::setPickupMode")]
    pub fn set_pickup_mode(&mut self, pickup: ArrowPickup) -> Result<()> {
        self.arrow_mut(|a| a.pickup = pickup)
    }

    /// Sets a thrown potion's type.
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "entity\\projectile\\SplashPotion::setPotionType")]
    pub fn set_potion(&mut self, potion: Potion) -> Result<()> {
        match &mut self.shown_mut().kind {
            Kind::Projectile(Projectile {
                shot: Shot::Potion { potion: p, .. },
                ..
            }) => {
                *p = potion;
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets a villager NPC's profession (the actor variant).
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "Villager::setProfession")]
    pub fn set_profession(&mut self, profession: u8) -> Result<()> {
        match &mut self.shown_mut().kind {
            Kind::Npc(Character {
                form: Form::Villager { profession: p, .. },
                ..
            }) => {
                *p = profession;
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets an arrow's Punch level.
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "entity\\projectile\\Arrow::setPunchKnockback")]
    pub fn set_punch_knockback(&mut self, level: u8) -> Result<()> {
        self.arrow_mut(|a| a.punch = level)
    }

    /// Sets its yaw and pitch in degrees, kept to float precision as saved.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for an angle that is not finite as a float.
    #[doc(alias = "Entity::setRotation")]
    pub fn set_rotation(&mut self, yaw: f64, pitch: f64) -> Result<()> {
        if !survival::is_float_angle(yaw) || !survival::is_float_angle(pitch) {
            return Err(Error::OutOfRange);
        }
        let e = self.entity_mut();
        e.yaw = f64::from(to_f32(yaw));
        e.pitch = f64::from(to_f32(pitch));
        Ok(())
    }

    /// Sets the size multiplier of its box and model, kept to float precision as saved.
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for a painting, whose box is its picture on the wall.
    /// [`Error::OutOfRange`] for a scale that is not above 0, or whose scaled box is more than 16
    /// blocks wide or high.
    #[doc(alias = "Entity::setScale")]
    pub fn set_scale(&mut self, scale: f64) -> Result<()> {
        let scale = f64::from(to_f32(scale));
        if matches!(self.kind, Kind::Painting(_)) {
            return Err(Error::WrongKind);
        }
        if !is_valid_scale(scale, &self.kind) {
            return Err(Error::OutOfRange);
        }
        let e = self.shown_mut();
        e.scale = scale;
        e.forced = true;
        Ok(())
    }

    /// Sets how many items a dropped item holds, 1 to the item's stack size.
    /// Viewers see the new count.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for another count.
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "ItemEntity::setStackSize")]
    pub fn set_stack_size(&mut self, count: u8) -> Result<()> {
        let max = match &self.kind {
            Kind::Item(item) => item
                .stack
                .item()
                .map_or(0, crate::item::Type::max_stack_size),
            _ => return Err(Error::WrongKind),
        };
        if count == 0 || count > max {
            return Err(Error::OutOfRange);
        }
        let (e, players) = self.parts();
        if let Kind::Item(item) = &mut e.kind {
            item.stack = item.stack.with_count(count);
        }
        let packet = Animation::ItemStackSize { count }.packet(e.runtime_id);
        send_to(players, &e.viewers, &packet);
        Ok(())
    }

    /// Sets the player an orb flies to, or none.
    /// With none it looks for the nearest again within a second.
    ///
    /// # Errors
    ///
    /// [`Error::NotOnline`] for a player who is not online.
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "ExperienceOrb::setTargetPlayer")]
    pub fn set_target_player(&mut self, target: Option<player::Id>) -> Result<()> {
        if target.is_some_and(|id| {
            self.players
                .get(&id.session())
                .is_none_or(|p| p.phase == Phase::Loading)
        }) {
            return Err(Error::NotOnline);
        }
        match &mut self.entity_mut().kind {
            Kind::XpOrb(orb) => {
                orb.target = target.map(player::Id::session);
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets who threw a dropped item (a player name).
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for a name over [`MAX_NAME_TAG_BYTES`].
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "ItemEntity::setThrower")]
    pub fn set_thrower(&mut self, thrower: impl Into<String>) -> Result<()> {
        let thrower = thrower.into();
        if thrower.len() > MAX_NAME_TAG_BYTES {
            return Err(Error::OutOfRange);
        }
        match &mut self.entity_mut().kind {
            Kind::Item(item) => {
                item.thrower = thrower;
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets whether primed TNT is shown as underwater TNT.
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "PrimedTNT::setWorksUnderwater")]
    pub fn set_works_underwater(&mut self, underwater: bool) -> Result<()> {
        match &mut self.shown_mut().kind {
            Kind::Tnt(t) => {
                t.underwater = underwater;
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Sets an orb's xp points, 1 to 32,767 (the saved short).
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for another value.
    /// [`Error::WrongKind`] for another kind.
    #[doc(alias = "ExperienceOrb::setXpValue")]
    pub fn set_xp_value(&mut self, value: u16) -> Result<()> {
        if value == 0 || value > Orb::MAX_VALUE {
            return Err(Error::OutOfRange);
        }
        match &mut self.shown_mut().kind {
            Kind::XpOrb(orb) => {
                orb.value = value;
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// Moves it to `pos` in `world`, at rest.
    /// It has no motion and no fall distance afterwards; the rotation stays. False, moving
    /// nothing, if the world does not exist, `pos` is not finite or outside the world border, its
    /// chunk is not loaded, another world is at its entity limit, or it is a painting (it hangs
    /// on its wall: clients never move a painting).
    #[doc(alias = "Entity::teleport")]
    pub fn teleport(&mut self, world: world::Id, pos: Vec3) -> bool {
        if matches!(self.kind, Kind::Painting(_)) {
            return false;
        }
        let to = world.index();
        let loaded = self
            .worlds
            .get(to)
            .is_some_and(|w| is_valid_position(pos) && w.is_chunk_loaded(chunk_of(pos)));
        let from = self.world;
        if !loaded || (to != from && self.entities.count(to) >= MAX_ENTITIES_PER_WORLD) {
            return false;
        }
        let e = self.entity_mut();
        e.body.position = pos;
        e.body.motion = Vec3::ZERO;
        e.body.fall_distance = 0.0;
        e.forced = true;
        if let Kind::Projectile(p) = &mut e.kind {
            p.stuck = None;
        }
        if to != from {
            self.entities.move_world(self.id, to);
        }
        self.entities.reindex(self.id);
        true
    }

    /// The entity, to change.
    /// Its chunk is marked for the next save. While the entity tick applies its steps (a
    /// listener's change), it moves again from its new state rather than by the step computed
    /// before the change.
    pub(super) fn entity_mut(&mut self) -> &mut Entity {
        self.parts().0
    }

    /// The entity, to change what its viewers see too.
    pub(super) fn shown_mut(&mut self) -> &mut Entity {
        let e = self.entity_mut();
        e.resend = true;
        e
    }

    /// An arrow's own data, lent to `f`; [`Error::WrongKind`] for another kind.
    fn arrow_mut(&mut self, f: impl FnOnce(&mut Arrow)) -> Result<()> {
        match &mut self.entity_mut().kind {
            Kind::Projectile(Projectile {
                shot: Shot::Arrow(a),
                ..
            }) => {
                f(a);
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// A cloud's own data, lent to `f`; [`Error::WrongKind`] for another kind.
    fn cloud_mut(&mut self, f: impl FnOnce(&mut Cloud)) -> Result<()> {
        match &mut self.entity_mut().kind {
            Kind::Cloud(c) => {
                f(c);
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// A cloud's own data, which viewers see, lent to `f`; [`Error::WrongKind`] for another kind.
    fn shown_cloud_mut(&mut self, f: impl FnOnce(&mut Cloud)) -> Result<()> {
        match &mut self.shown_mut().kind {
            Kind::Cloud(c) => {
                f(c);
                Ok(())
            }
            _ => Err(Error::WrongKind),
        }
    }

    /// The entity and the players, to change both.
    /// The entity's chunk is marked for the next save, as [`Self::entity_mut`] says.
    fn parts(&mut self) -> (&mut Entity, &mut BTreeMap<SessionId, Player>) {
        let key = {
            let e = &**self;
            (e.world, e.chunk)
        };
        self.entities.changed.insert(key);
        if self.entities.applying {
            self.entities.restep.insert(self.id);
        }
        #[expect(
            clippy::expect_used,
            reason = "invariant: a `Mut` is made for a live entity and holds the store's only \
                      mutable borrow, so nothing removes it while the `Mut` lives"
        )]
        let entity = self
            .entities
            .get_mut(self.id)
            .expect("the lent entity is in the store");
        (entity, &mut *self.players)
    }
}

impl fmt::Debug for Mut<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mut")
            .field("id", &self.id)
            .field("entity", &**self)
            .finish_non_exhaustive()
    }
}

impl Deref for Mut<'_> {
    type Target = Entity;

    fn deref(&self) -> &Entity {
        #[expect(
            clippy::expect_used,
            reason = "invariant: a `Mut` is made for a live entity and holds the store's only \
                      mutable borrow, so nothing removes it while the `Mut` lives"
        )]
        self.entities
            .get(self.id)
            .expect("the lent entity is in the store")
    }
}

impl Entity {
    /// How high an entity steps up blocks.
    /// No kind steps up blocks.
    const STEP_HEIGHT: f64 = 0.0;

    /// A projectile's base damage.
    #[doc(alias = "Projectile::getBaseDamage")]
    pub fn base_damage(&self) -> Option<f64> {
        match &self.kind {
            Kind::Projectile(p) => Some(p.damage),
            _ => None,
        }
    }

    /// The block an end crystal's beam points at, if any.
    #[doc(alias = "entity\\object\\EndCrystal::getBeamTarget")]
    pub fn beam_target(&self) -> Option<BlockPos> {
        match &self.kind {
            Kind::EndCrystal(c) => c.beam_target,
            _ => None,
        }
    }

    /// Whether it blocks placing a block in its box.
    /// Falling blocks, primed TNT, living NPCs and end crystals do.
    #[doc(alias = "Entity::canBeCollidedWith")]
    pub fn can_be_collided_with(&self) -> bool {
        self.is_obstacle()
    }

    /// Whether it is saved with its chunk; one that is not goes when its chunk unloads.
    #[doc(alias = "Entity::canSaveWithChunk")]
    pub fn can_save_with_chunk(&self) -> bool {
        self.saved
    }

    /// A cloud's ticks since it appeared.
    #[doc(alias = "AreaEffectCloud::getAge")]
    pub fn cloud_age_ticks(&self) -> Option<u32> {
        self.cloud().map(|c| c.age)
    }

    /// A cloud's effects.
    /// Each entity in it gets them for a quarter of their duration, instant ones at half
    /// strength.
    #[doc(alias = "AreaEffectCloud::getEffects")]
    pub fn cloud_effects(&self) -> Option<&[Instance]> {
        self.cloud().map(|c| c.effects.as_slice())
    }

    /// The radius a cloud started with, in blocks.
    #[doc(alias = "AreaEffectCloud::getInitialRadius")]
    pub fn cloud_initial_radius(&self) -> Option<f64> {
        self.cloud().map(|c| c.initial_radius)
    }

    /// The change of a cloud's maximum age for each entity it affects.
    #[doc(alias = "AreaEffectCloud::getMaxAgeChangeOnUse")]
    pub fn cloud_max_age_change_on_use_ticks(&self) -> Option<i32> {
        self.cloud().map(|c| c.max_age_on_use)
    }

    /// The age at which a cloud goes.
    #[doc(alias = "AreaEffectCloud::getMaxAge")]
    pub fn cloud_max_age_ticks(&self) -> Option<i32> {
        self.cloud().map(|c| c.max_age)
    }

    /// A cloud's radius, in blocks.
    #[doc(alias = "AreaEffectCloud::getRadius")]
    pub fn cloud_radius(&self) -> Option<f64> {
        match &self.kind {
            Kind::Cloud(c) => Some(c.radius),
            _ => None,
        }
    }

    /// The change of a cloud's radius when a bottle takes from it.
    /// Clients are shown it, but nothing takes from a cloud: Anchor has no dragon's breath.
    #[doc(alias = "AreaEffectCloud::getRadiusChangeOnPickup")]
    pub fn cloud_radius_change_on_pickup(&self) -> Option<f64> {
        self.cloud().map(|c| c.radius_on_pickup)
    }

    /// The change of a cloud's radius for each entity it affects.
    #[doc(alias = "AreaEffectCloud::getRadiusChangeOnUse")]
    pub fn cloud_radius_change_on_use(&self) -> Option<f64> {
        self.cloud().map(|c| c.radius_on_use)
    }

    /// The change of a cloud's radius per tick.
    #[doc(alias = "AreaEffectCloud::getRadiusChangePerTick")]
    pub fn cloud_radius_change_per_tick(&self) -> Option<f64> {
        self.cloud().map(|c| c.radius_per_tick)
    }

    /// The ticks before a cloud affects the same entity again.
    #[doc(alias = "AreaEffectCloud::getReapplicationDelay")]
    pub fn cloud_reapplication_delay_ticks(&self) -> Option<i32> {
        self.cloud().map(|c| c.reapplication_delay)
    }

    /// A dropped item's or orb's ticks until it disappears; -1 never.
    #[doc(alias = "ItemEntity::getDespawnDelay")]
    #[doc(alias = "ExperienceOrb::getDespawnDelay")]
    pub fn despawn_delay_ticks(&self) -> Option<i32> {
        match &self.kind {
            Kind::Item(item) => Some(item.despawn_delay_ticks),
            Kind::XpOrb(orb) => Some(orb.despawn_delay_ticks),
            _ => None,
        }
    }

    /// The unit vector it looks along.
    #[doc(alias = "Entity::getDirectionVector")]
    pub fn direction(&self) -> Vec3 {
        super::direction(self.yaw, self.pitch)
    }

    /// What it is.
    pub fn entity_type(&self) -> Type {
        self.kind.entity_type()
    }

    /// The height of its eyes above its position, times its scale.
    /// A human NPC's is a player's, else half its height plus 0.1, at most its height.
    #[doc(alias = "Entity::getEyeHeight")]
    pub fn eye_height(&self) -> f64 {
        if let Kind::Npc(Character {
            form: Form::Human { .. },
            ..
        }) = self.kind
        {
            return EYE_HEIGHT * self.scale;
        }
        let (_, height) = self.kind.size();
        math::min(height / 2.0 + 0.1, height) * self.scale
    }

    /// Where its eyes are.
    #[doc(alias = "Entity::getEyePos")]
    pub fn eye_position(&self) -> Vec3 {
        self.body.position + Vec3::new(0.0, self.eye_height(), 0.0)
    }

    /// Blocks fallen since it last stood on something.
    #[doc(alias = "Entity::getFallDistance")]
    pub fn fall_distance(&self) -> f64 {
        self.body.fall_distance
    }

    /// The block a falling block is.
    #[doc(alias = "FallingBlock::getBlock")]
    pub fn falling_block(&self) -> Option<block::State> {
        match &self.kind {
            Kind::FallingBlock(f) => Some(f.block),
            _ => None,
        }
    }

    /// Ticks it keeps burning.
    #[doc(alias = "Entity::getFireTicks")]
    pub fn fire_ticks(&self) -> u32 {
        u32::from(self.fire_ticks.cast_unsigned())
    }

    /// A firework rocket's explosions, as a firework item's `Explosions` holds them.
    /// They are compounds of `FireworkType`, `FireworkColor`, `FireworkFade`, `FireworkFlicker`
    /// and `FireworkTrail`.
    #[doc(alias = "entity\\object\\FireworkRocket::getExplosions")]
    pub fn firework_explosions(&self) -> Option<&[Compound]> {
        match &self.kind {
            Kind::Firework(f) => Some(&f.explosions),
            _ => None,
        }
    }

    /// Primed TNT's ticks until it explodes.
    #[doc(alias = "PrimedTNT::getFuse")]
    pub fn fuse_ticks(&self) -> Option<u16> {
        match &self.kind {
            Kind::Tnt(t) => Some(t.fuse),
            _ => None,
        }
    }

    /// The downward acceleration its kind falls with, in blocks per tick².
    /// It is per kind. Gravity may not pull it now ([`Self::has_gravity`]).
    #[doc(alias = "Entity::getGravity")]
    pub fn gravity(&self) -> f64 {
        self.kind.physics().gravity
    }

    /// Whether gravity pulls it.
    #[doc(alias = "Entity::hasGravity")]
    pub fn has_gravity(&self) -> bool {
        self.gravity
    }

    /// Its health.
    #[doc(alias = "Entity::getHealth")]
    pub fn health(&self) -> f64 {
        self.health
    }

    /// Whether it lives: health above 0 and, for an NPC, not dying.
    #[doc(alias = "Entity::isAlive")]
    pub fn is_alive(&self) -> bool {
        self.health > 0.0 && !matches!(&self.kind, Kind::Npc(c) if c.dead_ticks.is_some())
    }

    /// Whether it is a baby.
    /// A villager or a zombie is one if [`Mut::set_baby`] made it so; false for every other kind.
    #[doc(alias = "entity\\Ageable")]
    #[doc(alias = "entity\\Ageable::isBaby")]
    #[doc(alias = "Villager::isBaby")]
    pub fn is_baby(&self) -> bool {
        self.kind.is_baby()
    }

    /// Whether an end crystal shows its bedrock base (a placed one does not).
    #[doc(alias = "entity\\object\\EndCrystal::showBase")]
    pub fn is_base_visible(&self) -> bool {
        matches!(&self.kind, Kind::EndCrystal(c) if c.show_base)
    }

    /// Whether it is a critical arrow (more damage, a trail).
    #[doc(alias = "entity\\projectile\\Arrow::isCritical")]
    pub fn is_critical(&self) -> bool {
        matches!(arrow(&self.kind), Some(a) if a.critical)
    }

    /// Whether fire and lava leave it alone (netherite items, clouds, end crystals).
    #[doc(alias = "Entity::isFireProof")]
    pub fn is_fire_proof(&self) -> bool {
        self.kind.is_fire_proof()
    }

    /// Whether clients do not draw it.
    #[doc(alias = "Entity::isInvisible")]
    pub fn is_invisible(&self) -> bool {
        self.invisible
    }

    /// Whether an NPC takes no damage but the void's.
    /// Villager and human NPCs spawn so, zombies and custom entities do not. It is false for
    /// every other kind, whatever it takes ([`Mut::attack`] says which).
    pub fn is_invulnerable(&self) -> bool {
        matches!(&self.kind, Kind::Npc(c) if c.invulnerable)
    }

    /// Whether a thrown potion leaves a cloud.
    #[doc(alias = "entity\\projectile\\SplashPotion::willLinger")]
    pub fn is_lingering(&self) -> bool {
        matches!(
            &self.kind,
            Kind::Projectile(Projectile {
                shot: Shot::Potion { linger: true, .. },
                ..
            })
        )
    }

    /// Whether the name tag always shows.
    #[doc(alias = "Entity::isNameTagAlwaysVisible")]
    pub fn is_name_tag_always_visible(&self) -> bool {
        self.name_tag_always_visible
    }

    /// Whether the name tag shows when a player looks at it.
    #[doc(alias = "Entity::isNameTagVisible")]
    pub fn is_name_tag_visible(&self) -> bool {
        self.name_tag_visible
    }

    /// Whether it burns.
    #[doc(alias = "Entity::isOnFire")]
    pub fn is_on_fire(&self) -> bool {
        self.fire_ticks > 0
    }

    /// Whether its last move ended on a block.
    #[doc(alias = "Entity::isOnGround")]
    pub fn is_on_ground(&self) -> bool {
        self.body.on_ground
    }

    /// A dropped item's stack, or the trident item a thrown trident holds.
    /// A player who picks the trident up gets that item.
    #[doc(alias = "ItemEntity::getItem")]
    #[doc(alias = "entity\\projectile\\Trident::getItem")]
    pub fn item(&self) -> Option<&Stack> {
        match &self.kind {
            Kind::Item(item) => Some(&item.stack),
            Kind::Projectile(Projectile {
                shot: Shot::Trident(t),
                ..
            }) => Some(&t.item),
            _ => None,
        }
    }

    /// The age in ticks at which a firework rocket bursts.
    #[doc(alias = "entity\\object\\FireworkRocket::getMaxFlightTimeTicks")]
    pub fn max_flight_time_ticks(&self) -> Option<u32> {
        match &self.kind {
            Kind::Firework(f) => Some(f.max_flight),
            _ => None,
        }
    }

    /// The health it starts with: its kind's, or a custom type's own.
    #[doc(alias = "Entity::getMaxHealth")]
    #[doc(alias = "Living::getMaxHealth")]
    pub fn max_health(&self) -> f64 {
        self.kind.max_health()
    }

    /// Its motion, in blocks per tick.
    #[doc(alias = "Entity::getMotion")]
    pub fn motion(&self) -> Vec3 {
        self.body.motion
    }

    /// The name of an NPC, or `None` for kinds that do not live.
    /// It is a human NPC's name tag, `Villager` for a villager, `Zombie` for a zombie, and a
    /// custom entity's type.
    #[doc(alias = "Villager::getName")]
    #[doc(alias = "Zombie::getName")]
    pub fn name(&self) -> Option<&str> {
        match &self.kind {
            Kind::Npc(Character { form, .. }) => Some(match form {
                Form::Villager { .. } => "Villager",
                Form::Zombie { .. } => "Zombie",
                Form::Human { .. } => &self.name_tag,
                Form::Custom(c) => &c.spec.identifier,
            }),
            _ => None,
        }
    }

    /// Its name tag; a human NPC's is its name. Empty without one.
    #[doc(alias = "Entity::getNameTag")]
    pub fn name_tag(&self) -> &str {
        &self.name_tag
    }

    /// The Bedrock actor type clients draw, such as `minecraft:item`.
    #[doc(alias = "Entity::getNetworkTypeId")]
    #[doc(alias = "Zombie::getNetworkTypeId")]
    pub fn network_type(&self) -> &str {
        self.kind.identifier()
    }

    /// A dropped item's owner (a player name, saved; empty without one).
    #[doc(alias = "ItemEntity::getOwner")]
    pub fn owner(&self) -> Option<&str> {
        match &self.kind {
            Kind::Item(item) => Some(&item.owner),
            _ => None,
        }
    }

    /// The player who shot or threw a projectile or launched a firework rocket.
    /// It is `None` once the player has left the server.
    #[doc(alias = "Entity::getOwningEntity")]
    #[doc(alias = "Entity::getOwningEntityId")]
    pub fn owning_player(&self) -> Option<player::Id> {
        match &self.kind {
            Kind::Projectile(p) => p.owner.map(player::Id::new),
            Kind::Firework(f) => f.owner.map(player::Id::new),
            _ => None,
        }
    }

    /// The side of its wall a painting hangs on: north, south, west or east.
    /// This is the face of the wall block it covers.
    #[doc(alias = "Painting::getFacing")]
    pub fn painting_facing(&self) -> Option<Face> {
        match &self.kind {
            Kind::Painting(p) => Some(p.facing),
            _ => None,
        }
    }

    /// A painting's picture.
    #[doc(alias = "Painting::getMotive")]
    pub fn painting_motive(&self) -> Option<PaintingMotive> {
        match &self.kind {
            Kind::Painting(p) => Some(p.motive),
            _ => None,
        }
    }

    /// The item a player gets who picks it with a middle click.
    /// It is an end crystal, the block of a falling block or primed TNT, a painting, or the spawn
    /// egg of a villager or a zombie; `None` for the other kinds.
    #[doc(alias = "Entity::getPickedItem")]
    #[doc(alias = "entity\\object\\EndCrystal::getPickedItem")]
    #[doc(alias = "FallingBlock::getPickedItem")]
    #[doc(alias = "Painting::getPickedItem")]
    #[doc(alias = "PrimedTNT::getPickedItem")]
    #[doc(alias = "Villager::getPickedItem")]
    #[doc(alias = "Zombie::getPickedItem")]
    pub fn picked_item(&self) -> Option<Stack> {
        super::pick::picked_item(&self.kind)
    }

    /// A dropped item's ticks until a player may pick it up; -1 never.
    #[doc(alias = "ItemEntity::getPickupDelay")]
    pub fn pickup_delay_ticks(&self) -> Option<i16> {
        match &self.kind {
            Kind::Item(item) => Some(item.pickup_delay_ticks),
            _ => None,
        }
    }

    /// Who may pick up an arrow once stuck.
    #[doc(alias = "entity\\projectile\\Arrow::getPickupMode")]
    pub fn pickup_mode(&self) -> Option<ArrowPickup> {
        arrow(&self.kind).map(|a| a.pickup)
    }

    /// Its pitch in degrees (negative is up).
    #[doc(alias = "Location::getPitch")]
    pub fn pitch(&self) -> f64 {
        self.pitch
    }

    /// Where it is: the center of the bottom of its box.
    #[doc(alias = "Entity::getPosition")]
    #[doc(alias = "Entity::getLocation")]
    pub fn position(&self) -> Vec3 {
        self.body.position
    }

    /// A thrown potion's type.
    #[doc(alias = "entity\\projectile\\SplashPotion::getPotionType")]
    pub fn potion(&self) -> Option<Potion> {
        match &self.kind {
            Kind::Projectile(Projectile {
                shot: Shot::Potion { potion, .. },
                ..
            }) => Some(*potion),
            _ => None,
        }
    }

    /// A thrown potion's effects: the potion type's, none for water.
    #[doc(alias = "entity\\projectile\\SplashPotion::getPotionEffects")]
    pub fn potion_effects(&self) -> Option<&'static [Instance]> {
        self.potion().map(Potion::effects)
    }

    /// A villager NPC's profession (the actor variant, such as 1 for a farmer).
    #[doc(alias = "Villager::getProfession")]
    pub fn profession(&self) -> Option<u8> {
        match &self.kind {
            Kind::Npc(Character {
                form: Form::Villager { profession, .. },
                ..
            }) => Some(*profession),
            _ => None,
        }
    }

    /// An arrow's Punch level.
    #[doc(alias = "entity\\projectile\\Arrow::getPunchKnockback")]
    pub fn punch_knockback(&self) -> Option<u8> {
        arrow(&self.kind).map(|a| a.punch)
    }

    /// The runtime ID clients know it by, or `None` before it has one.
    /// An entity whose chunk loaded this tick gets one at the next entity tick.
    #[doc(alias = "Entity::getId")]
    pub fn runtime_id(&self) -> Option<u64> {
        (self.runtime_id != 0).then_some(self.runtime_id)
    }

    /// The size multiplier of its box and model (1 normally).
    #[doc(alias = "Entity::getScale")]
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// A human NPC's skin; `None` for the default skin and for other kinds.
    /// It is fixed when the NPC is spawned ([`Context::spawn_human`]); to change it, remove the
    /// NPC and spawn another.
    pub fn skin(&self) -> Option<&Skin> {
        match &self.kind {
            Kind::Npc(Character {
                form: Form::Human { skin, .. },
                ..
            }) => skin.as_deref(),
            _ => None,
        }
    }

    /// How high it steps up blocks while moving (0 for every kind).
    #[doc(alias = "Entity::getStepHeight")]
    #[expect(
        clippy::unused_self,
        reason = "consistency: an entity's value, read like its other physics getters"
    )]
    pub fn step_height(&self) -> f64 {
        Self::STEP_HEIGHT
    }

    /// The player an orb follows.
    #[doc(alias = "ExperienceOrb::getTargetPlayer")]
    #[doc(alias = "ExperienceOrb::hasTargetPlayer")]
    pub fn target_player(&self) -> Option<player::Id> {
        match &self.kind {
            Kind::XpOrb(orb) => orb.target.map(player::Id::new),
            _ => None,
        }
    }

    /// Who threw a dropped item (a player name, saved; empty without one).
    #[doc(alias = "ItemEntity::getThrower")]
    pub fn thrower(&self) -> Option<&str> {
        match &self.kind {
            Kind::Item(item) => Some(&item.thrower),
            _ => None,
        }
    }

    /// The players it was sent to, in `Id` order.
    #[doc(alias = "Entity::getViewers")]
    pub fn viewers(&self) -> impl Iterator<Item = player::Id> + '_ {
        self.viewers.iter().map(|&s| player::Id::new(s))
    }

    /// Whether it is underwater TNT.
    #[doc(alias = "PrimedTNT::worksUnderwater")]
    pub fn works_underwater(&self) -> bool {
        matches!(&self.kind, Kind::Tnt(t) if t.underwater)
    }

    /// The world it is in.
    #[doc(alias = "Entity::getWorld")]
    #[doc(alias = "Position::getWorld")]
    pub fn world(&self) -> world::Id {
        world::Id::of(self.world)
    }

    /// An orb's xp points.
    #[doc(alias = "ExperienceOrb::getXpValue")]
    pub fn xp_value(&self) -> Option<u16> {
        match &self.kind {
            Kind::XpOrb(orb) => Some(orb.value),
            _ => None,
        }
    }

    /// Its yaw in degrees.
    #[doc(alias = "Location::getYaw")]
    pub fn yaw(&self) -> f64 {
        self.yaw
    }

    /// A cloud's own data.
    fn cloud(&self) -> Option<&Cloud> {
        match &self.kind {
            Kind::Cloud(c) => Some(c),
            _ => None,
        }
    }
}

impl Context<'_> {
    /// Whether a painting of `motive` can hang in the block `pos` of `world`.
    /// It hangs on the wall behind `facing`.
    /// Every block it covers must be loaded and not solid with a solid block behind it, and no
    /// other painting may be there. False for a world the server does not have or an up or down
    /// facing.
    #[doc(alias = "Painting::canFit")]
    pub fn can_hang_painting(
        &self,
        world: world::Id,
        pos: BlockPos,
        facing: Face,
        motive: PaintingMotive,
    ) -> bool {
        index(self.state, world)
            .is_some_and(|w| object::can_hang(self.state, w, pos, facing, motive))
    }

    /// Whether the player `viewer` may see the player `target`.
    /// It may if `target` is not hidden from it.
    #[doc(alias = "Player::canSee")]
    pub fn can_see(&self, viewer: player::Id, target: player::Id) -> bool {
        !self
            .state
            .rules
            .entities
            .hidden
            .contains(&(viewer.session(), target.session()))
    }

    /// The entities in chunk `chunk` of `world`, in `Id` order.
    #[doc(alias = "World::getChunkEntities")]
    pub fn chunk_entities(
        &self,
        world: world::Id,
        chunk: ChunkPos,
    ) -> impl Iterator<Item = (Id, &Entity)> + '_ {
        let entities = &self.state.rules.entities;
        let world = index(self.state, world).unwrap_or(usize::MAX);
        entities
            .columns
            .get(&(world, chunk))
            .into_iter()
            .flat_map(|c| c.ids.iter())
            .filter_map(|&id| entities.get(id).filter(|e| !e.flagged).map(|e| (id, e)))
    }

    /// The entities of `world` in `Id` order (none for a world the server does not have).
    /// It visits every slot of the store, so a rule that needs the entities of a place asks
    /// [`Self::entities_in`] instead.
    #[doc(alias = "World::getEntities")]
    pub fn entities(&self, world: world::Id) -> impl Iterator<Item = (Id, &Entity)> + '_ {
        let world = index(self.state, world).unwrap_or(usize::MAX);
        self.state.rules.entities.of_world(world)
    }

    /// The entity clients know by `runtime_id` (an index lookup; players are not entities here).
    #[doc(alias = "WorldManager::findEntity")]
    #[doc(alias = "World::getEntity")]
    pub fn find_entity(&self, runtime_id: u64) -> Option<Id> {
        super::find(self.state, runtime_id).filter(|&id| self.entity(id).is_some())
    }

    /// The entity `id`, while it exists (removed, despawned and unloaded entities are gone).
    pub fn entity(&self, id: Id) -> Option<&Entity> {
        self.state.rules.entities.get(id).filter(|e| !e.flagged)
    }

    /// The nearest entity of `world` within `max_distance` of `pos` that `filter` accepts.
    /// The distance is that of its position; ties go to the lower `Id`.
    /// `None` for a world the server does not have, or a `pos` or distance that is not finite.
    #[doc(alias = "World::getNearestEntity")]
    pub fn nearest_entity(
        &self,
        world: world::Id,
        pos: Vec3,
        max_distance: f64,
        mut filter: impl FnMut(&Entity) -> bool,
    ) -> Option<Id> {
        if !pos.is_finite() || !max_distance.is_finite() || max_distance < 0.0 {
            return None;
        }
        let reach = Vec3::new(max_distance, max_distance, max_distance);
        let area = Aabb::new(pos - reach, pos + reach);
        let mut best: Option<(f64, Id)> = None;
        self.entities_in(world, area, |id, e| {
            let d = e.body.position.distance_squared(pos);
            if d > max_distance * max_distance || !filter(e) {
                return;
            }
            if best.is_none_or(|(bd, bid)| d < bd || (d == bd && id < bid)) {
                best = Some((d, id));
            }
        });
        best.map(|(_, id)| id)
    }

    /// Calls `visit` with each entity of `world` whose box overlaps `area`, in chunk then `Id`
    /// order, through the chunk index.
    /// It visits none for a world the server does not have or an area that is not finite. It
    /// takes a callback, not a list, so that a lookup per skill or per tick allocates nothing
    /// (`visit` cannot change the store meanwhile).
    #[doc(alias = "World::getNearbyEntities")]
    #[doc(alias = "World::getCollidingEntities")]
    pub fn entities_in(&self, world: world::Id, area: Aabb, mut visit: impl FnMut(Id, &Entity)) {
        let Some(w) = index(self.state, world) else {
            return;
        };
        if !(area.min.is_finite() && area.max.is_finite()) {
            return;
        }
        let entities = &self.state.rules.entities;
        entities.each_nearby(w, area, |id| {
            if let Some(e) = entities.get(id) {
                visit(id, e);
            }
        });
    }

    /// Drops `stack` as an item entity at `pos` in `world`.
    /// It gets `motion`, or a random one for `None`, and a pickup delay of 10 ticks.
    ///
    /// # Errors
    ///
    /// Gives the stack back (as std's `mpsc::SendError` gives its value back, so the items are
    /// not lost) when it is empty, the world does not exist, `pos` is not finite, outside the
    /// world border or not loaded, or the world is at its entity limit.
    #[doc(alias = "World::dropItem")]
    pub fn drop_item(
        &mut self,
        world: world::Id,
        pos: Vec3,
        stack: Stack,
        motion: Option<Vec3>,
    ) -> Result<Id, Stack> {
        let Some(w) = index(self.state, world) else {
            return Err(stack);
        };
        if stack.is_empty() || !is_valid_position(pos) || motion.is_some_and(|m| !m.is_finite()) {
            return Err(stack);
        }
        let back = stack.clone();
        let delay = super::DROP_PICKUP_DELAY_TICKS;
        drop_stack(self.state, w, pos, stack, motion, delay, String::new()).ok_or(back)
    }

    /// Drops `amount` xp points as orbs at `pos` in `world`.
    /// The points split into the orb sizes, each with a random motion. It returns the points
    /// dropped: fewer at the world's entity limit, none for a world the server does not have, a
    /// `pos` that is not loaded, or with `xp = false` or `xp-orbs = false`.
    #[doc(alias = "World::dropExperience")]
    pub fn drop_xp(&mut self, world: world::Id, pos: Vec3, amount: u32) -> u32 {
        match index(self.state, world) {
            Some(w) if is_valid_position(pos) => super::drop_orbs(self.state, w, pos, amount),
            _ => 0,
        }
    }

    /// The entity `id`, to change it.
    pub fn entity_mut(&mut self, id: Id) -> Option<Mut<'_>> {
        let delay = item_despawn_delay_ticks(self.state);
        let State {
            rules,
            worlds,
            players,
            ..
        } = &mut *self.state;
        rules.entities.get(id).filter(|e| !e.flagged)?;
        Some(Mut {
            id,
            entities: &mut rules.entities,
            worlds,
            players,
            item_despawn_delay_ticks: delay,
        })
    }

    /// Stops showing the player `target` to the player `viewer`.
    /// It stays hidden until [`Self::show_player`] or either leaves: `viewer`'s client removes it
    /// at the end of the tick and is not sent it again. True if `target` is hidden from `viewer`
    /// now; false if either is not online, they are the same player, or `viewer` hides 1,024
    /// players already.
    #[doc(alias = "Player::hidePlayer")]
    pub fn hide_player(&mut self, viewer: player::Id, target: player::Id) -> bool {
        if viewer == target || self.player(viewer).is_none() || self.player(target).is_none() {
            return false;
        }
        let (v, t) = (viewer.session(), target.session());
        let hidden = &mut self.state.rules.entities.hidden;
        if hidden.contains(&(v, t)) {
            return true;
        }
        if hidden
            .range((v, SessionId::new(0))..=(v, SessionId::CONSOLE))
            .count()
            >= MAX_HIDDEN_PER_PLAYER
        {
            return false;
        }
        hidden.insert((v, t))
    }

    /// Plays `animation` on `actor` for the players who see it.
    /// A player sees its own too. It is a cosmetic packet like a sound: a player whose outbox is
    /// nearly full misses it. False if the actor is gone, or is an entity loaded this tick, which
    /// has no runtime ID.
    #[doc(alias = "Entity::broadcastAnimation")]
    #[doc(alias = "Player::broadcastAnimation")]
    pub fn play_animation(&mut self, actor: Actor, animation: Animation) -> bool {
        let shown = match actor {
            Actor::Player(id) => self
                .player(id)
                .map(|p| (world::Id::of(p.world), p.position, p.runtime_id)),
            Actor::Entity(id) => self
                .entity(id)
                .map(|e| (e.world(), e.body.position, e.runtime_id)),
        };
        match shown {
            Some((world, at, runtime_id)) if runtime_id != 0 => self
                .broadcast_packet(world, at, &animation.packet(runtime_id))
                .is_ok(),
            _ => false,
        }
    }

    /// Shows the player `target` to the player `viewer` again.
    /// It is sent at the end of the tick if `viewer` sees its chunk. False if it was not hidden.
    #[doc(alias = "Player::showPlayer")]
    pub fn show_player(&mut self, viewer: player::Id, target: player::Id) -> bool {
        self.state
            .rules
            .entities
            .hidden
            .remove(&(viewer.session(), target.session()))
    }

    /// Spawns a cloud at `pos` in `world` that gives `effects` (at most 32).
    /// It has a lingering potion's defaults: radius 3, gone after 600 ticks, shrinking as it is
    /// used.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for more effects.
    /// [`Error::NotSpawned`] for a world or place [`Self::drop_item`] refuses.
    #[doc(alias = "AreaEffectCloud::__construct")]
    pub fn spawn_area_effect_cloud(
        &mut self,
        world: world::Id,
        pos: Vec3,
        effects: &[Instance],
    ) -> Result<Id> {
        if effects.len() > Cloud::MAX_EFFECTS {
            return Err(Error::OutOfRange);
        }
        let kind = Kind::Cloud(projectile::new_cloud(effects));
        self.spawn(world, pos, kind, Vec3::ZERO)
    }

    /// Spawns an end crystal at `pos` (the middle of its bottom) in `world`.
    /// It has no base and no beam.
    ///
    /// # Errors
    ///
    /// [`Error::NotSpawned`] for a world or place [`Self::drop_item`] refuses.
    pub fn spawn_end_crystal(&mut self, world: world::Id, pos: Vec3) -> Result<Id> {
        self.spawn(world, pos, object::crystal(), Vec3::ZERO)
    }

    /// Spawns a falling block of `block` at `pos` in `world`.
    /// The world's block there stays as it is; the falling block lands as a block, or drops its
    /// item where it cannot. No block starts falling by itself.
    ///
    /// # Errors
    ///
    /// [`Error::NotSpawned`] for a world or place [`Self::drop_item`] refuses.
    #[doc(alias = "FallingBlock::__construct")]
    pub fn spawn_falling_block(
        &mut self,
        world: world::Id,
        pos: Vec3,
        block: block::State,
    ) -> Result<Id> {
        let kind = Kind::FallingBlock(Falling { block });
        self.spawn(world, pos, kind, Vec3::ZERO)
    }

    /// Launches a firework rocket from `pos` in `world` that bursts with `explosions` (see
    /// [`Entity::firework_explosions`]).
    /// It launches as a used rocket item of flight multiplier 1 does: it flies 20 ticks and up to
    /// 12 more, starting slowly upwards with a little random drift. It has no owner and is never
    /// saved. [`Mut::set_max_flight_time_ticks`] and [`Mut::set_motion`] change its flight.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for explosions [`Mut::set_firework_explosions`] refuses.
    /// [`Error::NotSpawned`] for a world or place [`Self::drop_item`] refuses.
    #[doc(alias = "entity\\object\\FireworkRocket::__construct")]
    pub fn spawn_firework_rocket(
        &mut self,
        world: world::Id,
        pos: Vec3,
        explosions: Vec<Compound>,
    ) -> Result<Id> {
        let explosions = object::checked_explosions(explosions).ok_or(Error::OutOfRange)?;
        let w = index(self.state, world)
            .filter(|_| is_valid_position(pos))
            .ok_or(Error::NotSpawned)?;
        if !super::can_create(self.state, w, pos) {
            return Err(Error::NotSpawned);
        }
        let seed = self.state.worlds.get(w).map_or(0, World::seed);
        // Opened once the rocket can be made: it takes this runtime ID, so no two launches share
        // the stream.
        let key = random::key(LAUNCH_RANDOM, &[self.state.next_runtime_id]);
        let mut random = Random::new(seed, self.state.tick, key);
        let item = (object::DEFAULT_FLIGHT_MULTIPLIER, explosions);
        object::launch(self.state, w, pos, item, None, &mut random).ok_or(Error::NotSpawned)
    }

    /// Spawns a human NPC at `pos` (its feet) in `world`, named `name`.
    /// The name is at most [`MAX_NAME_TAG_BYTES`]. The NPC has `skin` or the default skin, and
    /// its name always showing. Like a villager NPC it is invulnerable
    /// ([`Mut::set_invulnerable`]) and has no AI. The skin is a [`Skin`] made with `Skin::new`
    /// (a player's skin is another type with no conversion) and cannot change afterwards.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for a longer name.
    /// [`Error::NotSpawned`] for a world or place [`Self::drop_item`] refuses.
    #[doc(alias = "Human::__construct")]
    pub fn spawn_human(
        &mut self,
        world: world::Id,
        pos: Vec3,
        name: impl Into<String>,
        skin: Option<Skin>,
    ) -> Result<Id> {
        let name = name.into();
        if name.len() > MAX_NAME_TAG_BYTES {
            return Err(Error::OutOfRange);
        }
        let w = index(self.state, world).ok_or(Error::NotSpawned)?;
        let npc = Npc::Human { name, skin };
        super::spawn_npc(self.state, w, pos, npc).ok_or(Error::NotSpawned)
    }

    /// Hangs a painting of `motive` in the block `pos` of `world`, on the wall behind `facing`.
    /// The facing is north, south, west or east. The painting hangs whether or not it fits there
    /// (see [`Self::can_hang_painting`]).
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for an up or down facing.
    /// [`Error::NotSpawned`] for a world or place [`Self::drop_item`] refuses.
    #[doc(alias = "Painting::__construct")]
    pub fn spawn_painting(
        &mut self,
        world: world::Id,
        pos: BlockPos,
        facing: Face,
        motive: PaintingMotive,
    ) -> Result<Id> {
        if !object::is_horizontal(facing) {
            return Err(Error::OutOfRange);
        }
        // A painting hanging in a block stands at its corner.
        self.spawn(
            world,
            pos.to_corner(),
            object::painting(facing, motive),
            Vec3::ZERO,
        )
    }

    /// Launches `projectile` from `pos` in `world` with `motion` in blocks per tick.
    /// Each component of the motion is cut to ±10. The projectile has no owner.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for a motion that is not finite.
    /// [`Error::NotSpawned`] for a world or place [`Self::drop_item`] refuses.
    #[doc(alias = "Projectile::__construct")]
    pub fn spawn_projectile(
        &mut self,
        world: world::Id,
        pos: Vec3,
        projectile: crate::entity::Projectile,
        motion: Vec3,
    ) -> Result<Id> {
        use crate::entity::Projectile as P;
        if !motion.is_finite() {
            return Err(Error::OutOfRange);
        }
        let (shot, damage) = match projectile {
            P::Arrow { critical } => {
                let arrow = Arrow {
                    critical,
                    pickup: ArrowPickup::Any,
                    punch: 0,
                    life: 0,
                };
                (Shot::Arrow(arrow), Arrow::DAMAGE)
            }
            P::Snowball => (Shot::Snowball, 0.0),
            P::Egg => (Shot::Egg, 0.0),
            P::EnderPearl => (Shot::EnderPearl, 0.0),
            P::SplashPotion(potion) => (
                Shot::Potion {
                    potion,
                    linger: false,
                },
                0.0,
            ),
            P::LingeringPotion(potion) => (
                Shot::Potion {
                    potion,
                    linger: true,
                },
                0.0,
            ),
            P::XpBottle => (Shot::XpBottle, 0.0),
            P::IceBomb => (Shot::IceBomb, 0.0),
        };
        let kind = Kind::Projectile(Projectile {
            shot,
            damage,
            stuck: None,
            owner: None,
            owner_runtime_id: 0,
        });
        self.spawn(world, pos, kind, motion)
    }

    /// Spawns primed TNT at `pos` in `world` that explodes after `fuse_ticks` ticks.
    /// The fuse is at most 32,767.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for a longer fuse.
    /// [`Error::NotSpawned`] for a world or place [`Self::drop_item`] refuses.
    pub fn spawn_tnt(&mut self, world: world::Id, pos: Vec3, fuse_ticks: u16) -> Result<Id> {
        if fuse_ticks > Tnt::MAX_FUSE {
            return Err(Error::OutOfRange);
        }
        let kind = Kind::Tnt(Tnt {
            fuse: fuse_ticks,
            underwater: false,
        });
        self.spawn(world, pos, kind, Vec3::ZERO)
    }

    /// Throws a trident holding `item` from `pos` in `world` with `motion` in blocks per tick.
    /// Each component of the motion is cut to ±10. The trident has no owner, deals 8 damage and
    /// is picked up as `item`.
    ///
    /// # Errors
    ///
    /// Gives `item` back (as [`Self::drop_item`] does) when it is empty, the motion is not
    /// finite, or where [`Self::drop_item`] fails.
    #[doc(alias = "entity\\projectile\\Trident::__construct")]
    pub fn spawn_trident(
        &mut self,
        world: world::Id,
        pos: Vec3,
        item: Stack,
        motion: Vec3,
    ) -> Result<Id, Stack> {
        if item.is_empty() || !motion.is_finite() {
            return Err(item);
        }
        let back = item.clone();
        let kind = Kind::Projectile(Projectile {
            shot: Shot::Trident(Trident {
                item,
                creative: false,
                can_collide: true,
            }),
            damage: Trident::DAMAGE,
            stuck: None,
            owner: None,
            owner_runtime_id: 0,
        });
        self.spawn(world, pos, kind, motion).map_err(|_| back)
    }

    /// Spawns one xp orb of `value` points (1 to 32,767) at `pos` in `world`, at rest.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for another value.
    /// [`Error::NotSpawned`] for a world or place [`Self::drop_item`] refuses, and with
    /// `xp = false` or `xp-orbs = false`.
    #[doc(alias = "ExperienceOrb::__construct")]
    pub fn spawn_xp_orb(&mut self, world: world::Id, pos: Vec3, value: u16) -> Result<Id> {
        if value == 0 || value > Orb::MAX_VALUE {
            return Err(Error::OutOfRange);
        }
        if !self.state.settings.has_orbs() {
            return Err(Error::NotSpawned);
        }
        let orb = Orb {
            value,
            despawn_delay_ticks: super::DEFAULT_DESPAWN_DELAY_TICKS,
            look_for_target: 0,
            target: None,
        };
        self.spawn(world, pos, Kind::XpOrb(orb), Vec3::ZERO)
    }

    /// Spawns a zombie at `pos` (its feet) in `world`; it has no AI.
    /// It stands where it is put, falls, burns, takes damage and at its death drops rotten flesh
    /// and sometimes an iron ingot, a carrot or a potato, and 5 xp.
    ///
    /// # Errors
    ///
    /// [`Error::NotSpawned`] for a world or place [`Self::drop_item`] refuses.
    pub fn spawn_zombie(&mut self, world: world::Id, pos: Vec3) -> Result<Id> {
        let w = index(self.state, world).ok_or(Error::NotSpawned)?;
        super::spawn_npc(self.state, w, pos, Npc::Zombie).ok_or(Error::NotSpawned)
    }

    /// Creates an entity of `kind` at `pos` in `world` with `motion` and no rotation.
    fn spawn(&mut self, world: world::Id, pos: Vec3, kind: Kind, motion: Vec3) -> Result<Id> {
        let w = index(self.state, world)
            .filter(|_| is_valid_position(pos))
            .ok_or(Error::NotSpawned)?;
        create(self.state, w, pos, kind, |_| (motion, 0.0, 0.0)).ok_or(Error::NotSpawned)
    }
}

impl action::Sealed for action::Attack {
    fn perform(self, cx: &mut Context<'_>) {
        super::attack(cx.state, self.attacker.session(), self.target);
    }
}

impl action::Sealed for action::Explode {
    fn perform(self, cx: &mut Context<'_>) {
        let Self {
            world,
            center,
            size,
            fire,
        } = self;
        let Some(w) = index(cx.state, world) else {
            return;
        };
        super::explode(cx.state, w, center, size, fire, Raise::Events);
    }
}

impl Entities {
    /// The live, unflagged entities of `world` in `Id` order: the store's slots, filtered.
    fn of_world(&self, world: WorldIndex) -> impl Iterator<Item = (Id, &Entity)> + '_ {
        (0_u32..).zip(&self.slots).filter_map(move |(i, s)| {
            let e = s.entity.as_ref()?;
            (e.world == world && !e.flagged).then_some((Id::new(i, s.generation), e))
        })
    }

    /// Moves a live entity's count to world `to`; the caller checked the cap and reindexes it.
    fn move_world(&mut self, id: Id, to: WorldIndex) {
        let Some(e) = self.get_mut(id) else {
            return;
        };
        let (from, chunk) = (e.world, e.chunk);
        e.world = to;
        if let Some(n) = self.counts.get_mut(&from) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.counts.remove(&from);
            }
        }
        *self.counts.entry(to).or_insert(0) += 1;
        // The index entry moves as a chunk change of the old world to a chunk of the new one.
        if let Some(column) = self.columns.get_mut(&(from, chunk)) {
            column.ids.remove(&id);
            if column.ids.is_empty() {
                self.columns.remove(&(from, chunk));
            }
        }
        let column = self.columns.entry((to, chunk)).or_default();
        column.ids.insert(id);
        column.arrived.insert(id);
        self.changed.insert((from, chunk));
        self.changed.insert((to, chunk));
    }
}

/// An arrow's own data.
fn arrow(kind: &Kind) -> Option<&Arrow> {
    match kind {
        Kind::Projectile(Projectile {
            shot: Shot::Arrow(a),
            ..
        }) => Some(a),
        _ => None,
    }
}

/// Whether a cloud takes `blocks` as a radius change (finite, within the saved form's bound).
fn is_radius_change(blocks: f64) -> bool {
    (-Cloud::MAX_RADIUS..=Cloud::MAX_RADIUS).contains(&blocks)
}

#[cfg(test)]
mod tests;
