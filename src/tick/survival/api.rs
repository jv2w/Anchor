//! The public [`Player`] methods on its survival state (PMMP `Player`, `Human`, `Living`, `Entity`,
//! `HungerManager`, `ExperienceManager`, `EffectManager`): game mode, health, hunger, experience,
//! effects, fire, flight, the spawn point and the entity data the viewers see.
//!
//! Each setter changes the value at once. What the client and the viewers must learn is marked
//! (attribute and data bits, [`Custom`] marks) and sent by the tick's attribute flush or by
//! [`super::sync`] at the end of the tick. What needs the world (a death's drops, a respawn,
//! leaving a bed) waits for the player to be spawned. No call raises an event.

use super::attributes::{
    self, ABILITIES, ABSORPTION, ADVENTURE, DATA, EXHAUSTION, EXPERIENCE, HEALTH, LEVEL, MOVEMENT,
    SATURATION, TAGS,
};
use super::effects::{self, Active};
use super::{
    MAX_AIR_TICKS, MAX_EXHAUSTION, MAX_FIRE_TICKS, MAX_FOOD, MAX_SATURATION, Point, Survival,
    appearance, broadcast_from, broadcast_to_viewers, damage, food, has_finite_resources,
    is_survival,
};
use crate::block::{AIR, Face};
use crate::config::GameMode;
use crate::effect::{self, Instance};
use crate::event::{DamageCause, DamageSource};
use crate::item::{self, Stack};
use crate::math::{Vec3, to_f32};
use crate::player::{Error, Id, Result, check_text};
use crate::protocol::consts::ABILITY_BASE_FLY_SPEED;
use crate::protocol::packet::{Packet, SetActorMotion, UpdatePlayerGameType};
use crate::tick::interact::drops;
use crate::tick::queue::Skin;
use crate::tick::{Context, Phase, Player, SessionId, State, entity, inventory};
use crate::world::{BlockPos, Entry};

/// The farthest a look reaches, in blocks (PMMP `Living::getLineOfSight`).
const MAX_SIGHT: u32 = 120;
/// The most blocks a look of [`MAX_SIGHT`] passes through: a ray crosses at most one block
/// boundary per axis per block travelled.
const MAX_SIGHT_CELLS: usize = 3 * MAX_SIGHT as usize + 4;

/// What the API sets on top of the rules (PMMP setters): entity data, abilities and the marks of
/// work that waits for the end of the tick or the spawn. None of it is saved, as in PMMP.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Custom {
    /// The name above the player (PMMP `nameTag`); `None` shows its name.
    pub(super) name_tag: Option<String>,
    /// The line under the name tag (PMMP `scoreTag`).
    pub(super) score_tag: String,
    /// Hidden from the viewers (PMMP `Entity::$invisible`).
    pub(super) invisible: bool,
    /// Makes no sounds (PMMP `Entity::$silent`).
    pub(super) silent: bool,
    /// The client does not move the player (PMMP `noClientPredictions`, the `NoAi` flag).
    pub(super) no_client_predictions: bool,
    /// May the player fly; `None` follows the game mode (creative and spectator).
    pub(super) allow_flight: Option<bool>,
    /// Do blocks stop the player; `None` follows the game mode (all but spectator).
    pub(super) block_collision: Option<bool>,
    /// The flight speed (PMMP `flightSpeedMultiplier`), finite.
    pub(super) flight_speed: f32,
    /// The health without effects (PMMP `maxHealth`), at least 1.
    pub(super) max_health: u32,
    /// The player was killed by the API: its death drops wait for its next survival step.
    pub(super) killed: bool,
    /// The player asked to respawn through the API.
    pub(super) respawn: bool,
    /// The player asked to leave its bed through the API.
    pub(super) wake: bool,
    /// The player is an operator: its abilities carry operator permissions (BDS, PMMP
    /// `NetworkSession::syncAbilities`). Follows the operator list, which is saved.
    pub(super) operator: bool,
    /// May the player climb ladders and vines (PMMP `canClimb`, the `Climb` flag).
    pub(super) can_climb: bool,
    /// May the player climb any wall (PMMP `canClimbWalls`, the `WallClimbing` flag).
    pub(super) can_climb_walls: bool,
    /// Does hunger run for the player (PMMP `HungerManager::$enabled`), on top of the `hunger`
    /// setting and the game mode.
    pub(super) hunger: bool,
    /// Do experience orbs fly to the player (PMMP `ExperienceManager::$canAttractXpOrbs`).
    pub(super) attracts_orbs: bool,
    /// The movement speed before sprinting and effects (PMMP's movement attribute), finite and
    /// not negative.
    pub(super) movement_speed: f64,
    /// The full air supply in ticks (PMMP `maxBreathTicks`), at least 1.
    pub(super) max_air: i16,
    /// Does the client jump up blocks by itself (PMMP `autoJump`).
    pub(super) auto_jump: bool,
    /// The motion the API gave the player, for it and its viewers at the end of the tick.
    pub(super) motion: Option<Vec3>,
    /// The emote the API plays, for the viewers at the end of the tick.
    pub(super) emote: Option<String>,
    /// The API turned the player: it and its viewers see the rotation at the end of the tick,
    /// in the tick's teleport if there is one.
    pub(super) turned: bool,
    /// The skin changed (the API or the client): every player gets it at the end of the tick,
    /// or of a later one while the server's skin budget is spent ([`appearance::SkinBudget`]).
    pub(super) skin: bool,
    /// An API call changed what the client or the viewers see since the last flush, so the end
    /// of the tick flushes it (the rules' own changes keep their flush).
    pub(super) touched: bool,
}

impl Custom {
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            name_tag,
            score_tag,
            invisible,
            silent,
            no_client_predictions,
            allow_flight,
            block_collision,
            flight_speed,
            max_health,
            killed,
            respawn,
            wake,
            operator,
            can_climb,
            can_climb_walls,
            hunger,
            attracts_orbs,
            movement_speed,
            max_air,
            auto_jump,
            motion,
            emote,
            turned,
            skin,
            touched,
        } = self;
        match name_tag {
            Some(tag) => h.bytes(tag.as_bytes()),
            None => h.u64(u64::MAX),
        }
        h.bytes(score_tag.as_bytes());
        for v in [allow_flight, block_collision] {
            h.u64(v.map_or(2, u64::from));
        }
        h.u64(
            u64::from(*invisible)
                | u64::from(*silent) << 1
                | u64::from(*no_client_predictions) << 2
                | u64::from(*killed) << 3
                | u64::from(*respawn) << 4
                | u64::from(*wake) << 5
                | u64::from(*touched) << 6
                | u64::from(*operator) << 7
                | u64::from(*can_climb) << 8
                | u64::from(*can_climb_walls) << 9
                | u64::from(*hunger) << 10
                | u64::from(*attracts_orbs) << 11
                | u64::from(*auto_jump) << 12
                | u64::from(*skin) << 13
                | u64::from(*turned) << 14,
        );
        h.u64(movement_speed.to_bits());
        h.u64(u64::from(max_air.cast_unsigned()));
        match motion {
            Some(m) => {
                for c in [m.x, m.y, m.z] {
                    h.u64(c.to_bits());
                }
            }
            None => h.u64(u64::MAX),
        }
        match emote {
            Some(id) => h.bytes(id.as_bytes()),
            None => h.u64(u64::MAX),
        }
        h.u64(u64::from(flight_speed.to_bits()));
        h.u64(u64::from(*max_health));
    }
}

impl Default for Custom {
    fn default() -> Self {
        Self {
            name_tag: None,
            score_tag: String::new(),
            invisible: false,
            silent: false,
            no_client_predictions: false,
            allow_flight: None,
            block_collision: None,
            flight_speed: ABILITY_BASE_FLY_SPEED,
            max_health: 20,
            killed: false,
            respawn: false,
            wake: false,
            operator: false,
            can_climb: true,
            can_climb_walls: false,
            hunger: true,
            attracts_orbs: true,
            movement_speed: attributes::BASE_SPEED,
            max_air: MAX_AIR_TICKS,
            auto_jump: true,
            motion: None,
            emote: None,
            turned: false,
            skin: false,
            touched: false,
        }
    }
}

crate::tick::checkpoint::fields!(Custom {
    name_tag,
    score_tag,
    invisible,
    silent,
    no_client_predictions,
    allow_flight,
    block_collision,
    flight_speed,
    max_health,
    killed,
    respawn,
    wake,
    operator,
    can_climb,
    can_climb_walls,
    hunger,
    attracts_orbs,
    movement_speed,
    max_air,
    auto_jump,
    motion,
    emote,
    turned,
    skin,
    touched,
});

impl Survival {
    /// Whether the player may start flying: its own setting, or creative and spectator.
    pub(super) fn can_fly(&self) -> bool {
        self.custom.allow_flight.unwrap_or(matches!(
            self.mode(),
            GameMode::Creative | GameMode::Spectator
        ))
    }

    /// Whether blocks stop the player: its own setting, or all game modes but spectator.
    pub(super) fn has_block_collision(&self) -> bool {
        self.custom
            .block_collision
            .unwrap_or(self.mode() != GameMode::Spectator)
    }

    /// Marks client state to send, by the end of the tick even after the rules' flush.
    pub(super) fn touch(&mut self, bits: u16) {
        self.mark(bits);
        self.custom.touched = true;
    }

    /// Shows the client operator permissions (`op`) or a member's; a change sends
    /// `UpdateAbilities` by the end of the tick, as BDS does after `op` and `deop`.
    pub(in crate::tick) fn set_operator(&mut self, op: bool) {
        if self.custom.operator != op {
            self.custom.operator = op;
            self.touch(ABILITIES);
        }
    }
}

impl Player {
    /// The player's game mode.
    #[doc(alias = "Player::getGamemode")]
    pub fn game_mode(&self) -> GameMode {
        self.survival.mode()
    }

    /// Whether the player is in survival or adventure mode, the modes that take damage (PMMP's
    /// default, not literal, check). Items are used up in every mode but creative
    /// ([`has_finite_resources`](Self::has_finite_resources)).
    #[doc(alias = "Player::isSurvival")]
    pub fn is_survival(&self) -> bool {
        is_survival(self.game_mode())
    }

    /// Whether the player is in creative mode.
    #[doc(alias = "Player::isCreative")]
    pub fn is_creative(&self) -> bool {
        self.game_mode() == GameMode::Creative
    }

    /// Whether the player is in adventure mode.
    #[doc(alias = "Player::isAdventure")]
    pub fn is_adventure(&self) -> bool {
        self.game_mode() == GameMode::Adventure
    }

    /// Whether the player is in spectator mode.
    #[doc(alias = "Player::isSpectator")]
    pub fn is_spectator(&self) -> bool {
        self.game_mode() == GameMode::Spectator
    }

    /// Whether placing and using items uses them up: every mode but creative.
    #[doc(alias = "Player::hasFiniteResources")]
    pub fn has_finite_resources(&self) -> bool {
        has_finite_resources(self.game_mode())
    }

    /// The health in half hearts, 0 when dead.
    #[doc(alias = "Entity::getHealth")]
    pub fn health(&self) -> f64 {
        self.survival.health
    }

    /// The largest health: [`set_max_health`](Self::set_max_health)'s value (20 at join) and 4
    /// for each level of health boost.
    #[doc(alias = "Entity::getMaxHealth")]
    #[doc(alias = "Living::getMaxHealth")]
    pub fn max_health(&self) -> f64 {
        self.survival.max_health()
    }

    /// Whether the player is alive: not dead and waiting to respawn.
    #[doc(alias = "Entity::isAlive")]
    pub fn is_alive(&self) -> bool {
        super::is_alive(self)
    }

    /// The extra health absorbed first (golden apples, the absorption effect).
    #[doc(alias = "Living::getAbsorption")]
    pub fn absorption(&self) -> f64 {
        self.survival.absorption
    }

    /// The food level, 0 to 20.
    #[doc(alias = "HungerManager::getFood")]
    pub fn food(&self) -> f64 {
        self.survival.food
    }

    /// Whether hunger runs for the player while the `hunger` setting and its game mode let it.
    #[doc(alias = "HungerManager::isEnabled")]
    pub fn hunger_enabled(&self) -> bool {
        self.survival.custom.hunger
    }

    /// Whether the player's food is below 20. Food that needs hunger is also eaten at 20 in
    /// peaceful and creative.
    #[doc(alias = "HungerManager::isHungry")]
    pub fn is_hungry(&self) -> bool {
        self.survival.food < MAX_FOOD
    }

    /// The saturation, 0 to 20: spent before food.
    #[doc(alias = "HungerManager::getSaturation")]
    pub fn saturation(&self) -> f64 {
        self.survival.saturation
    }

    /// The exhaustion, 0 to 20: every 4 cost a saturation or food point.
    #[doc(alias = "HungerManager::getExhaustion")]
    pub fn exhaustion(&self) -> f64 {
        self.survival.exhaustion
    }

    /// The experience level, 0 to 24,791.
    #[doc(alias = "ExperienceManager::getXpLevel")]
    pub fn xp_level(&self) -> u32 {
        super::xp_level(self)
    }

    /// The progress through the level, 0 to below 1.
    #[doc(alias = "ExperienceManager::getXpProgress")]
    pub fn xp_progress(&self) -> f64 {
        self.survival.xp_progress
    }

    /// The experience points the level and progress hold, at most those of the highest level.
    #[doc(alias = "ExperienceManager::getCurrentTotalXp")]
    pub fn current_total_xp(&self) -> u64 {
        food::current_xp(&self.survival)
    }

    /// The experience points collected since the last death.
    #[doc(alias = "ExperienceManager::getLifetimeTotalXp")]
    pub fn lifetime_total_xp(&self) -> u32 {
        self.survival.xp_lifetime
    }

    /// The experience points collected into the current level.
    #[doc(alias = "ExperienceManager::getRemainderXp")]
    pub fn remainder_xp(&self) -> u64 {
        let s = &self.survival;
        food::current_xp(s).saturating_sub(crate::player::xp_to_reach_level(s.xp_level))
    }

    /// Whether the player may pick up an experience orb now: 2 ticks pass between pickups.
    #[doc(alias = "ExperienceManager::canPickupXp")]
    pub fn can_pickup_xp(&self) -> bool {
        self.survival.xp_cooldown == 0
    }

    /// Whether experience orbs fly to the player.
    #[doc(alias = "ExperienceManager::canAttractXpOrbs")]
    pub fn can_attract_xp_orbs(&self) -> bool {
        self.survival.custom.attracts_orbs
    }

    /// The enchanting seed, 0 until the player's first tick draws one.
    #[doc(alias = "Human::getEnchantmentSeed")]
    pub fn enchantment_seed(&self) -> i32 {
        super::enchantment_seed(self)
    }

    /// The active effects, in the order of [`effect::Type`].
    #[doc(alias = "Living::getEffects")]
    #[doc(alias = "EffectCollection::all")]
    pub fn effects(&self) -> impl Iterator<Item = Instance> + '_ {
        self.survival
            .effects
            .iter()
            .map(|(&effect, &active)| active.instance(effect))
    }

    /// The active effect of type `effect`, if any.
    #[doc(alias = "EffectCollection::get")]
    pub fn effect(&self, effect: effect::Type) -> Option<Instance> {
        self.survival
            .effects
            .get(&effect)
            .map(|&active| active.instance(effect))
    }

    /// Whether an effect of type `effect` is active.
    #[doc(alias = "EffectCollection::has")]
    pub fn has_effect(&self, effect: effect::Type) -> bool {
        self.survival.effects.contains_key(&effect)
    }

    /// Whether the player burns.
    #[doc(alias = "Entity::isOnFire")]
    pub fn is_on_fire(&self) -> bool {
        self.survival.fire_ticks > 0
    }

    /// The ticks the player keeps burning.
    #[doc(alias = "Entity::getFireTicks")]
    pub fn fire_ticks(&self) -> u32 {
        u32::from(self.survival.fire_ticks)
    }

    /// The air supply in ticks, -20 to [`max_air_supply_ticks`](Self::max_air_supply_ticks);
    /// below 0 the player drowns.
    #[doc(alias = "Living::getAirSupplyTicks")]
    pub fn air_supply_ticks(&self) -> i32 {
        i32::from(self.survival.air)
    }

    /// The full air supply in ticks: 300 unless
    /// [`set_max_air_supply_ticks`](Self::set_max_air_supply_ticks) changed it.
    #[doc(alias = "Living::getMaxAirSupplyTicks")]
    pub fn max_air_supply_ticks(&self) -> i32 {
        i32::from(self.survival.custom.max_air)
    }

    /// Whether the player could breathe at its last tick (not under water, or protected).
    #[doc(alias = "Living::isBreathing")]
    pub fn is_breathing(&self) -> bool {
        self.survival.breathing
    }

    /// The blocks fallen since the player last stood on something.
    #[doc(alias = "Entity::getFallDistance")]
    pub fn fall_distance(&self) -> f64 {
        self.survival.fall_distance
    }

    /// The ticks since the player last stood on something.
    #[doc(alias = "Player::getInAirTicks")]
    pub fn in_air_ticks(&self) -> u32 {
        self.survival.motion.in_air
    }

    /// The yaw in degrees, 0 to below 360 (0 looks south, 90 west).
    pub fn yaw(&self) -> f64 {
        super::rotation(self).0
    }

    /// The pitch in degrees: -90 (up) to 90 (down) as the official client and
    /// [`set_rotation`](Self::set_rotation) give it; a pitch another client sends is kept as
    /// sent, above -360 and below 360.
    pub fn pitch(&self) -> f64 {
        super::rotation(self).1
    }

    /// Whether [`set_rotation`](Self::set_rotation) takes `yaw` and `pitch` in degrees: both
    /// finite as floats, the pitch -90 (up) to 90 (down) as a float.
    pub fn is_rotation(yaw: f64, pitch: f64) -> bool {
        super::is_float_angle(yaw) && (-90.0..=90.0).contains(&to_f32(pitch))
    }

    /// The unit vector the player looks along.
    #[doc(alias = "Entity::getDirectionVector")]
    pub fn direction(&self) -> Vec3 {
        super::look_direction(self)
    }

    /// The side the player faces, from its yaw: south, west, north or east.
    #[doc(alias = "Entity::getHorizontalFacing")]
    pub fn horizontal_facing(&self) -> Face {
        crate::tick::interact::props::horizontal_facing(self.yaw())
    }

    /// Whether the player may reach `pos`: within `max_distance` of its eyes, and not more than
    /// √3/2 blocks behind the plane it looks through (PMMP's default `maxDiff`). False for a
    /// position or distance that is not finite.
    #[doc(alias = "Player::canInteract")]
    pub fn can_interact(&self, pos: Vec3, max_distance: f64) -> bool {
        pos.is_finite()
            && max_distance.is_finite()
            && super::can_interact(self.eye_position(), self.direction(), pos, max_distance)
    }

    /// Whether the player stands on something.
    #[doc(alias = "Entity::isOnGround")]
    pub fn is_on_ground(&self) -> bool {
        super::is_on_ground(self)
    }

    /// Whether the player sneaks.
    #[doc(alias = "Living::isSneaking")]
    pub fn is_sneaking(&self) -> bool {
        super::is_sneaking(self)
    }

    /// Whether the player holds the sneak key.
    #[doc(alias = "Player::isSneakPressed")]
    pub fn is_sneak_pressed(&self) -> bool {
        super::is_sneak_pressed(self)
    }

    /// Whether the player sprints.
    #[doc(alias = "Living::isSprinting")]
    pub fn is_sprinting(&self) -> bool {
        self.survival.motion.sprinting
    }

    /// The movement speed before sprinting (× 1.3) and the speed and slowness effects: 0.1
    /// unless [`set_movement_speed`](Self::set_movement_speed) changed it.
    #[doc(alias = "Living::getMovementSpeed")]
    pub fn movement_speed(&self) -> f64 {
        self.survival.custom.movement_speed
    }

    /// The armour points of what the player wears, 0 to 30 for vanilla armour: each point takes
    /// 4 % off the damage armour reduces.
    #[doc(alias = "Living::getArmorPoints")]
    pub fn armor_points(&self) -> u32 {
        damage::armor_points(self)
    }

    /// The Frost Walker level of the boots the player wears (0 without): moving freezes the
    /// water around its feet, and magma does not burn it.
    #[doc(alias = "Living::getFrostWalkerLevel")]
    pub fn frost_walker_level(&self) -> u32 {
        let feet = item::ArmorSlot::Feet.index();
        self.inventory.armor().get(feet).map_or(0, |boots| {
            u32::from(drops::level(boots, item::Enchantment::FrostWalker))
        })
    }

    /// Why and from what the player last took damage, kept through a respawn; `None` before its
    /// first damage in this session. Not saved, as in PMMP.
    #[doc(alias = "Entity::getLastDamageCause")]
    pub fn last_damage_cause(&self) -> Option<(DamageCause, DamageSource)> {
        self.survival.last_cause
    }

    /// Whether the player may climb ladders, vines and the like.
    #[doc(alias = "Entity::canClimb")]
    pub fn can_climb(&self) -> bool {
        self.survival.custom.can_climb
    }

    /// Whether the player may climb any wall, as a spider does.
    #[doc(alias = "Entity::canClimbWalls")]
    pub fn can_climb_walls(&self) -> bool {
        self.survival.custom.can_climb_walls
    }

    /// Whether the client jumps up blocks by itself (its auto-jump setting applies).
    #[doc(alias = "Player::hasAutoJump")]
    pub fn auto_jump(&self) -> bool {
        self.survival.custom.auto_jump
    }

    /// Whether the player swims.
    #[doc(alias = "Living::isSwimming")]
    pub fn is_swimming(&self) -> bool {
        self.survival.motion.swimming
    }

    /// Whether the player glides with elytra.
    #[doc(alias = "Living::isGliding")]
    pub fn is_gliding(&self) -> bool {
        self.survival.motion.gliding
    }

    /// Whether the player eats, drinks or uses a bow, trident, goat horn or spyglass.
    #[doc(alias = "Player::isUsingItem")]
    pub fn is_using_item(&self) -> bool {
        self.survival.using.is_some() || self.breaking.is_using()
    }

    /// Whether the player flies.
    #[doc(alias = "Player::isFlying")]
    pub fn is_flying(&self) -> bool {
        super::is_flying(self)
    }

    /// Whether the player may start flying: its own setting, else creative and spectator.
    /// Stopping needs no right.
    #[doc(alias = "Player::getAllowFlight")]
    pub fn allow_flight(&self) -> bool {
        self.survival.can_fly()
    }

    /// The flight speed: 0.05 by default; 10 times it is the speed in blocks a tick.
    #[doc(alias = "Player::getFlightSpeedMultiplier")]
    pub fn flight_speed(&self) -> f32 {
        self.survival.custom.flight_speed
    }

    /// Whether blocks stop the player: its own setting, else every mode but spectator.
    #[doc(alias = "Player::hasBlockCollision")]
    pub fn has_block_collision(&self) -> bool {
        self.survival.has_block_collision()
    }

    /// Whether the player sleeps in a bed.
    #[doc(alias = "Player::isSleeping")]
    pub fn is_sleeping(&self) -> bool {
        self.survival.sleeping.is_some()
    }

    /// The player's own respawn point: the world name and block, set by a bed or
    /// [`set_spawn`](Self::set_spawn); `None` respawns it at the default world's spawn. A point
    /// in a world the server does not run counts as none when the player respawns.
    #[doc(alias = "Player::getSpawn")]
    #[doc(alias = "Player::hasValidCustomSpawn")]
    pub fn spawn(&self) -> Option<(&str, BlockPos)> {
        self.survival
            .spawn
            .as_ref()
            .map(|p| (p.world.as_str(), p.pos))
    }

    /// Where the player last died: the world name and block.
    #[doc(alias = "Player::getDeathPosition")]
    pub fn death_position(&self) -> Option<(&str, BlockPos)> {
        self.survival
            .death
            .as_ref()
            .map(|p| (p.world.as_str(), p.pos))
    }

    /// The name shown above the player: its name unless [`set_name_tag`](Self::set_name_tag)
    /// changed it.
    #[doc(alias = "Entity::getNameTag")]
    pub fn name_tag(&self) -> &str {
        self.survival
            .custom
            .name_tag
            .as_deref()
            .unwrap_or(&self.identity.name)
    }

    /// The line shown under the name tag; empty for none.
    #[doc(alias = "Entity::getScoreTag")]
    pub fn score_tag(&self) -> &str {
        &self.survival.custom.score_tag
    }

    /// Whether the player is invisible: set so, or under the invisibility effect.
    #[doc(alias = "Entity::isInvisible")]
    pub fn is_invisible(&self) -> bool {
        self.survival.custom.invisible
            || self
                .survival
                .effects
                .contains_key(&effect::Type::Invisibility)
    }

    /// Whether the player makes no sounds for its viewers.
    #[doc(alias = "Entity::isSilent")]
    pub fn is_silent(&self) -> bool {
        self.survival.custom.silent
    }

    /// Whether the client is kept from moving the player.
    #[doc(alias = "Entity::hasNoClientPredictions")]
    pub fn has_no_client_predictions(&self) -> bool {
        self.survival.custom.no_client_predictions
    }

    /// Changes the player's game mode as PMMP does: creative may fly, spectator flies through
    /// blocks, survival and adventure stop flying; the client and its abilities follow. False,
    /// changing nothing, if it already has `mode`. The mode is saved; with `force-gamemode` the
    /// server's mode applies again at the next join.
    #[doc(alias = "Player::setGamemode")]
    pub fn set_game_mode(&mut self, mode: GameMode) -> bool {
        if self.game_mode() == mode {
            return false;
        }
        let s = &mut self.survival;
        s.game_mode = Some(mode);
        s.custom.allow_flight = None;
        s.custom.block_collision = None;
        // Through `set_flying`: a change of flight forgets the fall.
        if mode == GameMode::Spectator {
            self.survival.motion.on_ground = false;
            self.set_flying(true);
        } else if is_survival(mode) {
            self.set_flying(false);
        }
        self.survival.touch(ABILITIES | DATA);
        self.data_dirty = true;
        let packet = game_type_packet(self, mode);
        self.send_packet(packet);
        true
    }

    /// Sets the health, at most [`max_health`](Self::max_health); 0 or less kills the player as
    /// [`kill`](Self::kill) does. A dead player stays dead until it respawns. Fails for a value
    /// that is not finite.
    #[doc(alias = "Entity::setHealth")]
    #[doc(alias = "Living::setHealth")]
    pub fn set_health(&mut self, health: f64) -> Result<()> {
        if !health.is_finite() {
            return Err(Error::OutOfRange);
        }
        if self.survival.is_dead() {
            return Ok(());
        }
        if health <= 0.0 {
            self.kill();
            return Ok(());
        }
        let s = &mut self.survival;
        let max = s.max_health();
        s.health = if health > max { max } else { health };
        s.touch(HEALTH);
        self.data_dirty = true;
        Ok(())
    }

    /// Sets the health without effects; the health above the new maximum is cut (PMMP leaves it
    /// above, which shows a health bar beyond its end). Not saved, as in PMMP. Fails for 0.
    #[doc(alias = "Entity::setMaxHealth")]
    #[doc(alias = "Living::setMaxHealth")]
    pub fn set_max_health(&mut self, max: u32) -> Result<()> {
        if max == 0 {
            return Err(Error::OutOfRange);
        }
        let s = &mut self.survival;
        s.custom.max_health = max;
        let max = s.max_health();
        if s.health > max {
            s.health = max;
            self.data_dirty = true;
        }
        s.touch(HEALTH);
        Ok(())
    }

    /// Heals the player by `amount` half hearts up to its maximum; nothing for a dead player or a
    /// value that is not positive.
    #[doc(alias = "Entity::heal")]
    pub fn heal(&mut self, amount: f64) {
        damage::heal_player(self, amount);
        self.survival.touch(0);
    }

    /// Kills the player: its health drops to 0, and once it is spawned it drops what it carries
    /// and sees the death screen with the generic death message. False if it is already dead.
    #[doc(alias = "Entity::kill")]
    pub fn kill(&mut self) -> bool {
        let s = &mut self.survival;
        if s.is_dead() {
            return false;
        }
        s.health = 0.0;
        s.custom.killed = true;
        s.touch(HEALTH);
        self.data_dirty = true;
        true
    }

    /// Respawns a dead, spawned player at its respawn point by the end of the tick, as the
    /// respawn button does. False if it is alive or not spawned yet.
    #[doc(alias = "Player::respawn")]
    pub fn respawn(&mut self) -> bool {
        if !self.survival.is_dead() || self.phase != Phase::Spawned {
            return false;
        }
        self.survival.custom.respawn = true;
        true
    }

    /// Sets the absorbed health. Fails for a value that is negative, above `f32::MAX` (the most
    /// the client and the save hold) or not finite.
    #[doc(alias = "Living::setAbsorption")]
    pub fn set_absorption(&mut self, absorption: f64) -> Result<()> {
        within(absorption, 0.0..=f64::from(f32::MAX))?;
        let s = &mut self.survival;
        s.absorption = absorption;
        s.touch(ABSORPTION);
        self.data_dirty = true;
        Ok(())
    }

    /// Sets the food level. Fails outside 0 to 20, and with `hunger = false`.
    #[doc(alias = "HungerManager::setFood")]
    pub fn set_food(&mut self, food: f64) -> Result<()> {
        enabled(self.survival.switches.hunger)?;
        within(food, 0.0..=MAX_FOOD)?;
        food::set_food(&mut self.survival, food);
        self.survival.touch(0);
        self.data_dirty = true;
        Ok(())
    }

    /// Adds food up to 20. Fails for a value that is negative or not finite, and with
    /// `hunger = false`.
    #[doc(alias = "HungerManager::addFood")]
    pub fn add_food(&mut self, food: f64) -> Result<()> {
        enabled(self.survival.switches.hunger)?;
        within(food, 0.0..=f64::MAX)?;
        food::add_food(&mut self.survival, food, 0.0);
        self.survival.touch(0);
        self.data_dirty = true;
        Ok(())
    }

    /// Turns hunger on or off for the player: off, its food and saturation stay as they are.
    /// Not saved, as in PMMP.
    #[doc(alias = "HungerManager::setEnabled")]
    pub fn set_hunger_enabled(&mut self, enabled: bool) {
        self.survival.custom.hunger = enabled;
        self.survival.motion.hunger &= enabled;
    }

    /// Sets the saturation. Fails outside 0 to 20, and with `hunger = false`.
    #[doc(alias = "HungerManager::setSaturation")]
    pub fn set_saturation(&mut self, saturation: f64) -> Result<()> {
        enabled(self.survival.switches.hunger)?;
        within(saturation, 0.0..=MAX_SATURATION)?;
        self.survival.saturation = saturation;
        self.survival.touch(SATURATION);
        self.data_dirty = true;
        Ok(())
    }

    /// Adds saturation up to 20. Fails for a value that is negative or not finite, and with
    /// `hunger = false`.
    #[doc(alias = "HungerManager::addSaturation")]
    pub fn add_saturation(&mut self, saturation: f64) -> Result<()> {
        enabled(self.survival.switches.hunger)?;
        within(saturation, 0.0..=f64::MAX)?;
        food::add_food(&mut self.survival, 0.0, saturation);
        self.survival.touch(0);
        self.data_dirty = true;
        Ok(())
    }

    /// Sets the exhaustion; what is over 4 is spent at the next exhaustion. Fails outside 0 to
    /// 20, and with `hunger = false`.
    #[doc(alias = "HungerManager::setExhaustion")]
    pub fn set_exhaustion(&mut self, exhaustion: f64) -> Result<()> {
        enabled(self.survival.switches.hunger)?;
        within(exhaustion, 0.0..=MAX_EXHAUSTION)?;
        self.survival.exhaustion = exhaustion;
        self.survival.touch(EXHAUSTION);
        self.data_dirty = true;
        Ok(())
    }

    /// Adds exhaustion, spending saturation, then food, for every 4; nothing while the player
    /// does not get hungry (creative, spectator, [`set_hunger_enabled`](Self::set_hunger_enabled)).
    /// Fails for a value that is negative or not finite, and with `hunger = false`.
    #[doc(alias = "HungerManager::exhaust")]
    pub fn exhaust(&mut self, amount: f64) -> Result<()> {
        enabled(self.survival.switches.hunger)?;
        within(amount, 0.0..=f64::MAX)?;
        food::exhaust(&mut self.survival, amount);
        self.survival.touch(0);
        self.data_dirty = true;
        Ok(())
    }

    /// Sets the experience level, keeping the progress. Fails above 24,791, and with
    /// `experience = false`.
    #[doc(alias = "ExperienceManager::setXpLevel")]
    pub fn set_xp_level(&mut self, level: u32) -> Result<()> {
        enabled(self.survival.switches.experience)?;
        if level > crate::player::MAX_LEVEL {
            return Err(Error::OutOfRange);
        }
        let s = &mut self.survival;
        s.xp_level = level;
        s.touch(LEVEL);
        self.data_dirty = true;
        Ok(())
    }

    /// Adds experience levels, keeping the progress. Fails if the level would pass 24,791, and
    /// with `experience = false`.
    #[doc(alias = "ExperienceManager::addXpLevels")]
    pub fn add_xp_levels(&mut self, levels: u32) -> Result<()> {
        let level = self.survival.xp_level.saturating_add(levels);
        self.set_xp_level(level)
    }

    /// Takes experience levels, keeping the progress; not below 0. Nothing with
    /// `experience = false`.
    #[doc(alias = "ExperienceManager::subtractXpLevels")]
    pub fn subtract_xp_levels(&mut self, levels: u32) {
        super::subtract_levels(self, levels);
        self.survival.touch(0);
    }

    /// Sets the progress through the level. Fails outside 0 to below 1 (a full bar is the next
    /// level), and with `experience = false`.
    #[doc(alias = "ExperienceManager::setXpProgress")]
    pub fn set_xp_progress(&mut self, progress: f64) -> Result<()> {
        enabled(self.survival.switches.experience)?;
        if !(progress.is_finite() && (0.0..1.0).contains(&progress)) {
            return Err(Error::OutOfRange);
        }
        let s = &mut self.survival;
        s.xp_progress = progress;
        s.touch(EXPERIENCE);
        self.data_dirty = true;
        Ok(())
    }

    /// Adds experience points, raising the level as they fill it (without PMMP's sound); the
    /// points ever collected stay below 2^31. Nothing with `experience = false`.
    #[doc(alias = "ExperienceManager::addXp")]
    pub fn add_xp(&mut self, points: u32) {
        if !self.survival.switches.experience {
            return;
        }
        food::add_experience(self, points);
        self.survival.touch(0);
    }

    /// Takes experience points, lowering the level; not below 0. Nothing with
    /// `experience = false`.
    #[doc(alias = "ExperienceManager::subtractXp")]
    pub fn subtract_xp(&mut self, points: u32) {
        if !self.survival.switches.experience {
            return;
        }
        let s = &mut self.survival;
        let total = food::current_xp(s).saturating_sub(u64::from(points));
        food::set_xp(s, total);
        s.touch(0);
        self.data_dirty = true;
    }

    /// Sets the level and progress from experience points. Fails above the points of the highest
    /// level, 24,791 (2,761,645,247), and with `experience = false`.
    #[doc(alias = "ExperienceManager::setCurrentTotalXp")]
    pub fn set_current_total_xp(&mut self, points: u32) -> Result<()> {
        enabled(self.survival.switches.experience)?;
        if u64::from(points) > crate::player::xp_to_reach_level(crate::player::MAX_LEVEL) {
            return Err(Error::OutOfRange);
        }
        food::set_xp(&mut self.survival, u64::from(points));
        self.survival.touch(0);
        self.data_dirty = true;
        Ok(())
    }

    /// Sets the experience points collected since the last death. Fails above 2^31 - 1, and
    /// with `experience = false`.
    #[doc(alias = "ExperienceManager::setLifetimeTotalXp")]
    pub fn set_lifetime_total_xp(&mut self, points: u32) -> Result<()> {
        enabled(self.survival.switches.experience)?;
        if points > i32::MAX.cast_unsigned() {
            return Err(Error::OutOfRange);
        }
        self.survival.xp_lifetime = points;
        self.data_dirty = true;
        Ok(())
    }

    /// Lets experience orbs fly to the player, or not. Not saved, as in PMMP.
    #[doc(alias = "ExperienceManager::setCanAttractXpOrbs")]
    pub fn set_can_attract_xp_orbs(&mut self, attracts: bool) {
        self.survival.custom.attracts_orbs = attracts;
    }

    /// Sets the enchanting seed, which picks the enchanting table's offers.
    #[doc(alias = "Human::setEnchantmentSeed")]
    pub fn set_enchantment_seed(&mut self, seed: i32) {
        self.survival.xp_seed = Some(seed);
        self.data_dirty = true;
    }

    /// Draws a new enchanting seed on the player's next tick;
    /// [`enchantment_seed`](Self::enchantment_seed) is 0 until then. Enchanting at a table draws
    /// the new seed at once.
    #[doc(alias = "Human::regenerateEnchantmentSeed")]
    pub fn regenerate_enchantment_seed(&mut self) {
        self.survival.xp_seed = None;
        self.data_dirty = true;
    }

    /// Gives the player `effect`: its type, duration, amplifier, particles (shown, faint or
    /// hidden) and whether it never ends all come from the [`Instance`], which
    /// [`effect::Type::instance`] starts with PMMP's defaults. False if an effect of the type with
    /// a higher amplifier, or the same amplifier and a longer duration, stays.
    ///
    /// ```
    /// use anchor::effect;
    /// use anchor::player::Player;
    ///
    /// // Speed II for 10 seconds, without particles.
    /// fn hidden_speed(player: &mut Player) -> bool {
    ///     let speed = effect::Type::Speed
    ///         .instance()
    ///         .with_duration_ticks(200)
    ///         .with_amplifier(1)
    ///         .with_visible(false);
    ///     player.add_effect(speed)
    /// }
    /// ```
    #[doc(alias = "EffectManager::add")]
    #[doc(alias = "EffectCollection::add")]
    pub fn add_effect(&mut self, effect: Instance) -> bool {
        let added = effects::add_to(self, effect.effect(), Active::of(effect));
        self.survival.touch(0);
        added
    }

    /// Ends the effect of type `effect`, undoing what it changed. False if none was active.
    #[doc(alias = "EffectManager::remove")]
    #[doc(alias = "EffectCollection::remove")]
    pub fn remove_effect(&mut self, effect: effect::Type) -> bool {
        let removed = effects::remove_from(self, effect);
        self.survival.touch(0);
        removed
    }

    /// Ends every effect.
    #[doc(alias = "EffectCollection::clear")]
    pub fn clear_effects(&mut self) {
        // In effect ID order, as BDS removes them (P6 `syntax` `effect @s clear`).
        for effect in effects::active_types_by_id(&self.survival)
            .into_iter()
            .flatten()
        {
            effects::remove_from(self, effect);
        }
        self.survival.touch(0);
    }

    /// Sets the ticks the player keeps burning; 0 puts the fire out (PMMP `extinguish`, merged:
    /// a direct call raises no event). Fails above 32,767.
    #[doc(alias = "Entity::setFireTicks")]
    #[doc(alias = "Entity::extinguish")]
    pub fn set_fire_ticks(&mut self, ticks: u32) -> Result<()> {
        let ticks = u16::try_from(ticks)
            .ok()
            .filter(|&t| u32::from(t) <= MAX_FIRE_TICKS)
            .ok_or(Error::OutOfRange)?;
        self.survival.fire_ticks = ticks;
        self.survival.touch(DATA);
        self.data_dirty = true;
        Ok(())
    }

    /// Sets the player on fire for `seconds`, shortened by fire protection; a longer burn
    /// already running stays.
    #[doc(alias = "Entity::setOnFire")]
    #[doc(alias = "Living::setOnFire")]
    pub fn set_on_fire(&mut self, seconds: u32) {
        damage::set_on_fire(self, seconds);
        self.survival.touch(DATA);
    }

    /// Sets the air supply in ticks; the client and the viewers see it. Fails outside -20 to
    /// [`max_air_supply_ticks`](Self::max_air_supply_ticks) (PMMP takes any number, which the
    /// air rules never reach again), and with `drowning = false`.
    #[doc(alias = "Living::setAirSupplyTicks")]
    pub fn set_air_supply_ticks(&mut self, ticks: i32) -> Result<()> {
        enabled(self.survival.switches.drowning)?;
        let s = &mut self.survival;
        let ticks = i16::try_from(ticks)
            .ok()
            .filter(|t| (super::MIN_AIR_TICKS..=s.custom.max_air).contains(t))
            .ok_or(Error::OutOfRange)?;
        s.air = ticks;
        s.touch(DATA);
        self.data_dirty = true;
        Ok(())
    }

    /// Sets the full air supply in ticks: the air refills to it, and the air above it is cut.
    /// Not saved, as in PMMP. Fails outside 1 to 32,767, and with `drowning = false`.
    #[doc(alias = "Living::setMaxAirSupplyTicks")]
    pub fn set_max_air_supply_ticks(&mut self, ticks: i32) -> Result<()> {
        enabled(self.survival.switches.drowning)?;
        let max = i16::try_from(ticks)
            .ok()
            .filter(|&t| t >= 1)
            .ok_or(Error::OutOfRange)?;
        let s = &mut self.survival;
        s.custom.max_air = max;
        if s.air > max {
            s.air = max;
            self.data_dirty = true;
        }
        s.touch(DATA);
        Ok(())
    }

    /// Sets whether the player counts as breathing (the Breathing entity flag); the client and
    /// the viewers see it by the end of the tick. Each tick of a living, spawned player sets it
    /// again from whether it can breathe, as BDS does, so it holds only while the player does not
    /// tick; the air refills or drops whatever this says. Nothing with `drowning = false`: the
    /// player then always breathes.
    #[doc(alias = "Living::setBreathing")]
    pub fn set_breathing(&mut self, breathing: bool) {
        if self.survival.switches.drowning {
            self.survival.breathing = breathing;
            self.survival.touch(DATA);
        }
    }

    /// Sets the blocks fallen; the landing hurts for what is beyond 3. It is saved. Fails for a
    /// value that is negative or not finite.
    #[doc(alias = "Entity::setFallDistance")]
    pub fn set_fall_distance(&mut self, distance: f64) -> Result<()> {
        within(distance, 0.0..=f64::MAX)?;
        self.survival.fall_distance = distance;
        self.data_dirty = true;
        Ok(())
    }

    /// Forgets the fall, so the landing does not hurt, and restarts the
    /// [ticks in the air](Self::in_air_ticks).
    #[doc(alias = "Entity::resetFallDistance")]
    #[doc(alias = "Player::resetFallDistance")]
    pub fn reset_fall_distance(&mut self) {
        self.survival.fall_distance = 0.0;
        self.survival.motion.in_air = 0;
        self.data_dirty = true;
    }

    /// Turns the player to `yaw` and `pitch` in degrees, kept to float precision as the client
    /// sends them: [`yaw`](Self::yaw) and [`pitch`](Self::pitch) read the new rotation at once,
    /// and by the end of the tick the client and the viewers see it, in the tick's teleport if
    /// there is one (BDS `/tp` with a rotation). Fails for a rotation
    /// [`is_rotation`](Self::is_rotation) refuses.
    #[doc(alias = "Entity::setRotation")]
    pub fn set_rotation(&mut self, yaw: f64, pitch: f64) -> Result<()> {
        if !Self::is_rotation(yaw, pitch) {
            return Err(Error::OutOfRange);
        }
        let (yaw, pitch) = (to_f32(yaw), to_f32(pitch));
        super::movement::turn(&mut self.survival.motion, yaw, pitch);
        self.survival.custom.turned = true;
        Ok(())
    }

    /// Makes the player sneak, or not, for it and its viewers, until the client reports a
    /// change (the client owns its input, as in PMMP).
    #[doc(alias = "Living::setSneaking")]
    pub fn set_sneaking(&mut self, sneaking: bool) {
        self.survival.motion.sneaking = sneaking;
        self.survival.touch(DATA);
    }

    /// Makes the player sprint, or not, with the sprinting speed, until the client reports a
    /// change.
    #[doc(alias = "Living::setSprinting")]
    pub fn set_sprinting(&mut self, sprinting: bool) {
        self.survival.motion.sprinting = sprinting;
        self.survival.touch(DATA | MOVEMENT);
    }

    /// Makes the player swim, or not, until the client reports a change.
    #[doc(alias = "Living::setSwimming")]
    pub fn set_swimming(&mut self, swimming: bool) {
        self.survival.motion.swimming = swimming;
        self.survival.touch(DATA);
    }

    /// Makes the player glide, or not, until the client reports a change.
    #[doc(alias = "Living::setGliding")]
    pub fn set_gliding(&mut self, gliding: bool) {
        self.survival.motion.gliding = gliding;
        self.survival.touch(DATA);
    }

    /// Sets the movement speed before sprinting and effects; the client walks at it. PMMP sets
    /// the attribute with sprinting and effects in it, which drifts as they change; Anchor works
    /// the speed out from the base. Not saved, as in PMMP. Fails for a speed that is negative,
    /// above `f32::MAX` or not finite.
    #[doc(alias = "Living::setMovementSpeed")]
    pub fn set_movement_speed(&mut self, speed: f64) -> Result<()> {
        within(speed, 0.0..=f64::from(f32::MAX))?;
        self.survival.custom.movement_speed = speed;
        self.survival.touch(MOVEMENT);
        Ok(())
    }

    /// Sets what [`last_damage_cause`](Self::last_damage_cause) reports, for example before a
    /// [`kill`](Self::kill) that a death listener should credit to someone.
    #[doc(alias = "Entity::setLastDamageCause")]
    pub fn set_last_damage_cause(&mut self, cause: Option<(DamageCause, DamageSource)>) {
        self.survival.last_cause = cause;
    }

    /// Lets the player climb ladders and vines, or not. Not saved, as in PMMP.
    #[doc(alias = "Entity::setCanClimb")]
    pub fn set_can_climb(&mut self, can: bool) {
        self.survival.custom.can_climb = can;
        self.survival.touch(DATA);
    }

    /// Lets the player climb any wall, or not. Not saved, as in PMMP.
    #[doc(alias = "Entity::setCanClimbWalls")]
    pub fn set_can_climb_walls(&mut self, can: bool) {
        self.survival.custom.can_climb_walls = can;
        self.survival.touch(DATA);
    }

    /// Lets the client jump up blocks by itself, or not. Not saved, as in PMMP.
    #[doc(alias = "Player::setAutoJump")]
    pub fn set_auto_jump(&mut self, auto_jump: bool) {
        let s = &mut self.survival;
        if s.custom.auto_jump != auto_jump {
            s.custom.auto_jump = auto_jump;
            s.touch(ADVENTURE);
        }
    }

    /// Pushes the player: its client moves it by `motion` blocks a tick, and its viewers see
    /// it, at the end of the tick (PMMP `Player::setMotion`; players have no server-side motion,
    /// as with knockback). A second call in the tick wins. Fails for a motion that is not
    /// finite.
    #[doc(alias = "Player::setMotion")]
    pub fn set_motion(&mut self, motion: Vec3) -> Result<()> {
        if !motion.is_finite() {
            return Err(Error::OutOfRange);
        }
        self.survival.custom.motion = Some(motion);
        Ok(())
    }

    /// Plays the emote `emote_id` (a UUID from the emote catalogue) for the players that see
    /// the player, at the end of the tick. A second call in the tick wins. Fails for an empty ID
    /// or one longer than 128 bytes.
    #[doc(alias = "Human::emote")]
    pub fn emote(&mut self, emote_id: &str) -> Result<()> {
        if !appearance::valid_emote(emote_id) {
            return Err(Error::OutOfRange);
        }
        self.survival.custom.emote = Some(emote_id.to_owned());
        Ok(())
    }

    /// Changes the player's skin: every player sees it at the end of the tick, or of a later one
    /// while the server's budget for skin broadcasts (about 128 KiB a second) is spent. Not
    /// saved: the client sends its own skin at its next join. Fails for a skin a login would
    /// refuse (image sizes that do not match their data, more than 4 MiB, a geometry or resource
    /// patch that is not JSON nested at most 64 deep; a skin without a custom geometry has `{}`).
    #[doc(alias = "Human::setSkin")]
    pub fn set_skin(&mut self, skin: crate::protocol::Skin) -> Result<()> {
        let skin = Skin::new(std::sync::Arc::new(skin));
        if !skin.is_acceptable() {
            return Err(Error::OutOfRange);
        }
        self.identity.skin = skin;
        self.survival.custom.skin = true;
        Ok(())
    }

    /// Starts or stops the player's flight, whether or not it [may fly](Self::allow_flight); the
    /// fall is forgotten.
    #[doc(alias = "Player::setFlying")]
    pub fn set_flying(&mut self, flying: bool) {
        if self.survival.motion.flying != flying {
            self.survival.motion.flying = flying;
            self.reset_fall_distance();
            self.survival.touch(ABILITIES);
        }
    }

    /// Lets the player start flying, or not, until its game mode changes. A flight under way goes
    /// on until the client or [`set_flying`](Self::set_flying) stops it.
    #[doc(alias = "Player::setAllowFlight")]
    pub fn set_allow_flight(&mut self, allow: bool) {
        let s = &mut self.survival;
        if s.custom.allow_flight != Some(allow) {
            s.custom.allow_flight = Some(allow);
            s.touch(ABILITIES);
        }
    }

    /// Sets the flight speed; 0 stops horizontal flight and a negative speed reverses the
    /// controls. Fails for a value that is not finite.
    #[doc(alias = "Player::setFlightSpeedMultiplier")]
    pub fn set_flight_speed(&mut self, speed: f32) -> Result<()> {
        if !speed.is_finite() {
            return Err(Error::OutOfRange);
        }
        let s = &mut self.survival;
        if s.custom.flight_speed.to_bits() != speed.to_bits() {
            s.custom.flight_speed = speed;
            s.touch(ABILITIES);
        }
        Ok(())
    }

    /// Lets the player move through blocks, or not, until its game mode changes. Without block
    /// collision the client flies, as PMMP makes it.
    #[doc(alias = "Player::setHasBlockCollision")]
    pub fn set_block_collision(&mut self, collision: bool) {
        let s = &mut self.survival;
        if s.custom.block_collision != Some(collision) {
            s.custom.block_collision = Some(collision);
            s.touch(ABILITIES);
        }
    }

    /// Gets the player out of its bed by the end of the tick, freeing the bed. False if it does
    /// not sleep.
    #[doc(alias = "Player::stopSleep")]
    pub fn stop_sleep(&mut self) -> bool {
        if self.survival.sleeping.is_none() {
            return false;
        }
        self.survival.custom.wake = true;
        true
    }

    /// Sets or clears the player's respawn point: a world name and the block to respawn on. It is
    /// saved. Fails for a world name longer than 250 bytes, which no world has.
    #[doc(alias = "Player::setSpawn")]
    pub fn set_spawn(&mut self, spawn: Option<(&str, BlockPos)>) -> Result<()> {
        self.survival.spawn = point(spawn)?;
        self.data_dirty = true;
        Ok(())
    }

    /// Sets or clears where the player last died, which its recovery compass points to. It is
    /// saved. Fails for a world name longer than 250 bytes, which no world has.
    #[doc(alias = "Player::setDeathPosition")]
    pub fn set_death_position(&mut self, death: Option<(&str, BlockPos)>) -> Result<()> {
        self.survival.death = point(death)?;
        self.survival.touch(DATA);
        self.data_dirty = true;
        Ok(())
    }

    /// Sets the name shown above the player, for it and its viewers. Not saved, as in PMMP.
    /// Fails for a tag longer than 65,536 bytes.
    #[doc(alias = "Entity::setNameTag")]
    pub fn set_name_tag(&mut self, tag: impl Into<String>) -> Result<()> {
        let tag = tag.into();
        check_text(&tag)?;
        let s = &mut self.survival;
        s.custom.name_tag = Some(tag);
        s.touch(TAGS);
        Ok(())
    }

    /// Sets the line shown under the name tag. Fails for a tag longer than 65,536 bytes.
    #[doc(alias = "Entity::setScoreTag")]
    pub fn set_score_tag(&mut self, tag: impl Into<String>) -> Result<()> {
        let tag = tag.into();
        check_text(&tag)?;
        let s = &mut self.survival;
        s.custom.score_tag = tag;
        s.touch(TAGS);
        Ok(())
    }

    /// Makes the player invisible to its viewers, or not (the invisibility effect still hides
    /// it while it lasts).
    #[doc(alias = "Entity::setInvisible")]
    pub fn set_invisible(&mut self, invisible: bool) {
        self.survival.custom.invisible = invisible;
        self.survival.touch(DATA);
    }

    /// Silences the player, or not.
    #[doc(alias = "Entity::setSilent")]
    pub fn set_silent(&mut self, silent: bool) {
        self.survival.custom.silent = silent;
        self.survival.touch(DATA);
    }

    /// Keeps the client from moving the player (it cannot walk, only turn), or lets it again.
    #[doc(alias = "Entity::setNoClientPredictions")]
    pub fn set_no_client_predictions(&mut self, frozen: bool) {
        self.survival.custom.no_client_predictions = frozen;
        self.survival.touch(DATA);
    }
}

/// What needs the engine tick or the world besides the player.
impl Context<'_> {
    /// Whether `item`'s cooldown runs for `player` (ender pearls, chorus fruit, goat horns, or a
    /// cooldown [`reset_item_cooldown`](Self::reset_item_cooldown) started); false if the player
    /// is gone.
    #[doc(alias = "Player::hasItemCooldown")]
    pub fn has_item_cooldown(&self, player: Id, item: item::Type) -> bool {
        self.item_cooldown_expiry(player, item).is_some()
    }

    /// The engine tick ([`Context::tick`]) `item`'s cooldown for `player` ends at, while it
    /// runs (PMMP returns 0 for none).
    #[doc(alias = "Player::getItemCooldownExpiry")]
    pub fn item_cooldown_expiry(&self, player: Id, item: item::Type) -> Option<u64> {
        let p = self.state.players.get(&player.session())?;
        food::cooldown_end(self.state.tick, &p.survival, item)
    }

    /// The ticks `player` has been eating or drinking; `None` if it is not, or is gone.
    #[doc(alias = "Player::getItemUseDuration")]
    pub fn item_use_duration_ticks(&self, player: Id) -> Option<u64> {
        let p = self.state.players.get(&player.session())?;
        p.survival
            .using
            .map(|u| self.state.tick.saturating_sub(u.since))
    }

    /// Whether `player`'s eyes are under water, below the surface; false if it is gone.
    #[doc(alias = "Entity::isUnderwater")]
    pub fn is_player_underwater(&self, player: Id) -> bool {
        self.state
            .players
            .get(&player.session())
            .is_some_and(|p| super::movement::is_underwater(self.state, p))
    }

    /// Whether `player`'s eyes are inside an opaque solid block, where it suffocates; false if it
    /// is gone.
    #[doc(alias = "Entity::isInsideOfSolid")]
    pub fn is_player_inside_solid(&self, player: Id) -> bool {
        self.state
            .players
            .get(&player.session())
            .is_some_and(|p| super::movement::is_inside_solid(self.state, p))
    }

    /// The blocks `player` looks through, from its eyes along its look for at most
    /// `max_distance` blocks (at most 120), up to and with the first block that is not air
    /// (PMMP `Living::getLineOfSight` with no transparent list); only the last `max_length`
    /// unless it is 0. Blocks of chunks that are not loaded and positions above or below the
    /// world count as air; blocks only clients know stop the look, as clients show them. Empty
    /// if the player is gone.
    #[doc(alias = "Living::getLineOfSight")]
    pub fn line_of_sight(&self, player: Id, max_distance: u32, max_length: usize) -> Vec<BlockPos> {
        let Some(p) = self.state.players.get(&player.session()) else {
            return Vec::new();
        };
        let mut blocks = Vec::with_capacity(MAX_SIGHT_CELLS);
        look(self.state, p, max_distance, |pos| blocks.push(pos));
        if max_length != 0 && blocks.len() > max_length {
            blocks.drain(..blocks.len() - max_length);
        }
        blocks
    }

    /// The block `player` looks at within `max_distance` blocks (at most 120): the first that is
    /// not air, or the last block in reach if all are air (PMMP `Living::getTargetBlock`), by
    /// the rules of [`line_of_sight`](Self::line_of_sight). `None` if the player is gone.
    #[doc(alias = "Living::getTargetBlock")]
    pub fn target_block(&self, player: Id, max_distance: u32) -> Option<BlockPos> {
        let p = self.state.players.get(&player.session())?;
        let mut last = None;
        look(self.state, p, max_distance, |pos| last = Some(pos));
        last
    }

    /// Starts `item`'s cooldown for `player` again: `ticks` long, or the item's own cooldown for
    /// `None`; the client shows it. Items sharing a cooldown (PMMP's cooldown tag) share it.
    /// False, starting nothing, if the player is gone or the cooldown would be 0 ticks.
    #[doc(alias = "Player::resetItemCooldown")]
    pub fn reset_item_cooldown(
        &mut self,
        player: Id,
        item: item::Type,
        ticks: Option<u32>,
    ) -> bool {
        let tick = self.state.tick;
        let Some(p) = self.state.players.get_mut(&player.session()) else {
            return false;
        };
        let ticks = ticks.unwrap_or_else(|| item.cooldown().map_or(0, |c| u32::from(c.ticks())));
        if ticks == 0 {
            return false;
        }
        food::start_cooldown(tick, p, item, ticks);
        true
    }

    /// Makes `player` drop `stack` as Q does (PMMP `Player::dropItem`): it leaves below its eyes
    /// along its look, may be picked up after 40 ticks, and the viewers see the arm swing. The
    /// stack does not come from the inventory. `Err` gives the stack back when it is empty, the
    /// player is gone or not spawned, or the world is at its entity limit.
    #[doc(alias = "Player::dropItem")]
    pub fn drop_from(&mut self, player: Id, stack: Stack) -> std::result::Result<(), Stack> {
        let session = player.session();
        let spawned = self
            .state
            .players
            .get(&session)
            .is_some_and(|p| p.phase == Phase::Spawned);
        if stack.is_empty() || !spawned {
            return Err(stack);
        }
        let back = stack.clone();
        if inventory::throw_stack(self.state, session, stack) {
            Ok(())
        } else {
            Err(back)
        }
    }

    /// Puts `player` to sleep in the bed at `pos` in its world, night or day and however far
    /// (PMMP `Player::sleepOn`, without its event: a direct call raises none): the bed shows
    /// occupied and becomes its respawn point. False unless the player is spawned, alive and
    /// awake and `pos` is a half of a whole bed nobody sleeps in; always false with
    /// `beds = false`.
    #[doc(alias = "Player::sleepOn")]
    pub fn sleep_on(&mut self, player: Id, pos: BlockPos) -> bool {
        super::clock::sleep_on(self.state, player.session(), pos)
    }

    /// Hurts `player` by `amount` half hearts as [`Mut::attack`](crate::entity::Mut::attack)
    /// hurts an entity (PMMP `Player::attack` with `CAUSE_CUSTOM`, without the event): armour,
    /// effects, invulnerability and the game mode count, within the hit cooldown only a stronger
    /// hit lands (its difference), and a hit that starts the cooldown shows the hurt animation. A
    /// death follows at the end of the tick with the generic death message. False if no damage
    /// was taken, as for a player gone or not spawned, or an amount that is below 0, above
    /// [`MAX_DAMAGE`](crate::event::MAX_DAMAGE) or not finite.
    #[doc(alias = "Player::attack")]
    pub fn attack_player(&mut self, player: Id, amount: f64) -> bool {
        damage::damage_quiet(self.state, player.session(), amount, damage::Damage::Custom)
    }
}

/// Whether [`sync`] or [`appearance::send_skins`] has anything to do for the player: it is
/// spawned and a survival mark is set.
pub(in crate::tick) fn marked(player: &Player) -> bool {
    let c = &player.survival.custom;
    player.phase == Phase::Spawned
        && (c.killed
            || c.respawn
            || c.wake
            || c.touched
            || c.skin
            || c.motion.is_some()
            || c.emote.is_some()
            || c.turned)
}

/// The marks of a spawned player's survival state at the end of the tick: an API death drops its
/// items, a respawn and a wake happen, and the attributes and entity data changed since the rules'
/// flush are sent. These are the game's own steps in the tick's serial phase, so they raise their
/// events (the call raised none, what follows from it does). A changed skin is left marked for
/// [`appearance::send_skins`].
pub(in crate::tick) fn sync(state: &mut State, session: SessionId) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if player.phase != Phase::Spawned {
        return;
    }
    let c = &mut player.survival.custom;
    let (killed, respawn, wake, touched) = (
        std::mem::take(&mut c.killed),
        std::mem::take(&mut c.respawn),
        std::mem::take(&mut c.wake),
        std::mem::take(&mut c.touched),
    );
    let (motion, emote) = (c.motion.take(), c.emote.take());
    let turned = std::mem::take(&mut c.turned);
    let runtime_id = player.runtime_id;
    let dies = killed && player.survival.is_dead() && !player.survival.motion.death_shown;
    if let Some(motion) = motion {
        let packet = Packet::SetActorMotion(SetActorMotion {
            entity_runtime_id: runtime_id,
            velocity: motion.to_f32(),
            tick: 0,
        });
        broadcast_from(state, session, &packet);
    }
    if let Some(emote_id) = emote {
        let packet = appearance::emote_packet(runtime_id, emote_id);
        broadcast_to_viewers(state, session, &packet);
    }
    if turned {
        super::send_rotation(state, session);
    }
    if dies {
        damage::die(state, session, damage::GENERIC_DEATH, None);
    }
    if respawn {
        damage::respawn(state, session);
    }
    if wake {
        super::clock::wake(state, session);
    }
    if killed || respawn || wake || touched {
        attributes::flush_one(state, session);
    }
}

/// The packet that tells `player` its new game `mode`, as BDS sends it: `UpdatePlayerGameType`
/// with the player's own unique ID (P6; PMMP sends `SetPlayerGameType`). The client ignores it for
/// another player's ID, so only the player gets it.
pub(super) fn game_type_packet(player: &Player, mode: GameMode) -> Packet {
    Packet::UpdatePlayerGameType(UpdatePlayerGameType {
        game_type: super::saved::game_mode_id(mode),
        player_unique_id: entity::unique_id(player.runtime_id),
        tick: 0,
    })
}

/// Fails for a value outside `range` or not finite.
fn within(value: f64, range: std::ops::RangeInclusive<f64>) -> Result<()> {
    if value.is_finite() && range.contains(&value) {
        Ok(())
    } else {
        Err(Error::OutOfRange)
    }
}

/// Fails with [`Error::Disabled`] unless the system is `on`.
fn enabled(on: bool) -> Result<()> {
    if on { Ok(()) } else { Err(Error::Disabled) }
}

/// The respawn or death point `at` names, as the player's record keeps it. Fails for a world name
/// longer than a world's name may be, which no world has and the players' record could not
/// always save.
fn point(at: Option<(&str, BlockPos)>) -> Result<Option<Point>> {
    let Some((world, pos)) = at else {
        return Ok(None);
    };
    if world.len() > crate::storage::MAX_FILE_NAME {
        return Err(Error::TooLong);
    }
    Ok(Some(Point {
        world: world.to_owned(),
        pos,
    }))
}

/// Calls `visit` with the blocks `player` looks through, from its eyes along its look for at most
/// `max_distance` blocks (at most [`MAX_SIGHT`]), in order, up to and with the first block that
/// stops the look (PMMP `VoxelRayTrace::inDirection`, the entity rules'
/// [`entity::cells_along`]). Air, chunks that are not loaded and positions above or below the
/// world let the look through; every other block stops it, blocks only clients know included.
fn look(state: &State, player: &Player, max_distance: u32, mut visit: impl FnMut(BlockPos)) {
    let world = state.worlds.get(player.world);
    let (eye, direction) = (player.eye_position(), player.direction());
    let max = f64::from(max_distance.min(MAX_SIGHT));
    entity::cells_along(eye, direction, max, MAX_SIGHT_CELLS, |x, y, z| {
        let pos = BlockPos::new(x, y, z);
        visit(pos);
        world
            .and_then(|w| w.entry(pos))
            .is_some_and(|e| e != Entry::Known(AIR))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::item::{Kind as ItemKind, Stack};
    use crate::nbt::{Compound, Value};
    use crate::player::MAX_TEXT;
    use crate::protocol::consts::{Ability, EntityDataFlag, EntityDataKey};
    use crate::protocol::{FlagSet, MetadataValue};
    use crate::tick::inventory;
    use crate::tick::player::tests::{player_mut, run, two};
    use crate::tick::tests::received;

    /// A game mode change that starts or stops flight forgets the fall, as `set_flying` does
    /// (PMMP `Player::setFlying`).
    #[test]
    fn a_game_mode_change_of_flight_forgets_the_fall() {
        let (mut state, _rx) = two();
        let p = player_mut(&mut state, 1);
        assert!(p.set_game_mode(GameMode::Creative));
        p.set_flying(true);
        p.survival.fall_distance = 30.0;
        assert!(p.set_game_mode(GameMode::Survival));
        assert_eq!(p.fall_distance(), 0.0);
        p.survival.fall_distance = 30.0;
        assert!(p.set_game_mode(GameMode::Spectator));
        assert_eq!(p.fall_distance(), 0.0);
    }

    /// The values of the base ability layer in the last `UpdateAbilities` of `packets`.
    fn abilities(packets: &[Packet]) -> Option<(FlagSet<Ability>, f32, usize)> {
        packets.iter().rev().find_map(|p| match p {
            Packet::UpdateAbilities(u) => {
                let layer = u.ability_data.layers.first()?;
                let values = FlagSet::from_words(&[u64::from(layer.values)]).ok()?;
                Some((values, layer.fly_speed, u.ability_data.layers.len()))
            }
            _ => None,
        })
    }

    // pmmp: Player::getGamemode, Player::setGamemode, Player::isSurvival, Player::isCreative,
    // pmmp: Player::isAdventure, Player::isSpectator, Player::hasFiniteResources
    #[test]
    fn game_mode_changes_tell_the_client_its_mode_and_abilities() {
        let (mut state, [mut rx1, _rx2]) = two();
        let p = player_mut(&mut state, 1);
        assert_eq!(p.game_mode(), GameMode::Survival);
        assert!(p.is_survival() && p.has_finite_resources() && !p.allow_flight());
        assert!(!p.set_game_mode(GameMode::Survival), "already survival");
        assert!(p.set_game_mode(GameMode::Creative));
        assert!(p.is_creative() && !p.has_finite_resources() && p.allow_flight());
        run(&mut state, Vec::new());
        let got = received(&mut rx1).0;
        let modes: Vec<i32> = got
            .iter()
            .filter_map(|p| match p {
                Packet::UpdatePlayerGameType(g) => {
                    assert_eq!(
                        g.player_unique_id,
                        i64::try_from(state.players.values().next().unwrap().runtime_id).unwrap()
                    );
                    Some(g.game_type)
                }
                Packet::SetPlayerGameType(_) => panic!("BDS sends UpdatePlayerGameType"),
                _ => None,
            })
            .collect();
        assert_eq!(modes, [1]);
        let (values, _, _) = abilities(&got).expect("abilities sent");
        assert!(values.contains(Ability::MayFly) && values.contains(Ability::InstantBuild));
        let p = player_mut(&mut state, 1);
        assert!(p.set_game_mode(GameMode::Adventure));
        assert!(p.is_adventure() && p.is_survival());
        assert!(p.set_game_mode(GameMode::Spectator));
        assert!(p.is_spectator() && p.is_flying() && !p.has_block_collision());
        assert!(p.set_game_mode(GameMode::Survival));
        assert!(!p.is_flying() && p.has_block_collision());
        assert!(state.players.values().next().unwrap().data_dirty);
    }

    #[test]
    fn force_gamemode_ignores_the_saved_mode_at_the_join_only() {
        let creative = Survival {
            game_mode: Some(GameMode::Creative),
            ..Survival::default()
        };
        let mut extra = Compound::new();
        crate::tick::survival::saved::save(&creative, &mut extra);
        let mut forced = Survival {
            forced_mode: true,
            ..Survival::with_mode(GameMode::Adventure)
        };
        crate::tick::survival::saved::load(
            &mut forced,
            &extra,
            &crate::custom::Registry::default(),
        );
        assert_eq!(forced.mode(), GameMode::Adventure);
        let mut free = Survival::with_mode(GameMode::Adventure);
        crate::tick::survival::saved::load(&mut free, &extra, &crate::custom::Registry::default());
        assert_eq!(free.mode(), GameMode::Creative);
        // A change after the join stays, forced or not.
        forced.game_mode = Some(GameMode::Spectator);
        assert_eq!(forced.mode(), GameMode::Spectator);
    }

    /// The value and maximum of attribute `name` in the last `UpdateAttributes` that has it.
    fn attribute(packets: &[Packet], name: &str) -> Option<(f32, f32)> {
        packets.iter().rev().find_map(|p| match p {
            Packet::UpdateAttributes(u) => u
                .attributes
                .iter()
                .find(|a| a.attribute_value.name == name)
                .map(|a| (a.attribute_value.value, a.attribute_value.max)),
            _ => None,
        })
    }

    // pmmp: Entity::getHealth, Entity::setHealth, Living::setHealth, Entity::getMaxHealth,
    // pmmp: Living::getMaxHealth, Entity::setMaxHealth, Living::setMaxHealth, Entity::heal,
    // pmmp: Living::getAbsorption, Living::setAbsorption
    #[test]
    fn health_stays_within_its_maximum_and_the_client_hears_it() {
        let (mut state, [mut rx1, _rx2]) = two();
        let p = player_mut(&mut state, 1);
        assert_eq!(p.set_health(f64::NAN), Err(Error::OutOfRange));
        p.set_health(5.5).unwrap();
        assert_eq!(p.health(), 5.5);
        p.set_health(100.0).unwrap();
        assert_eq!(p.health(), 20.0);
        assert_eq!(p.set_max_health(0), Err(Error::OutOfRange));
        p.set_max_health(10).unwrap();
        assert_eq!((p.health(), p.max_health()), (10.0, 10.0));
        p.heal(5.0);
        assert_eq!(p.health(), 10.0);
        p.set_max_health(40).unwrap();
        p.heal(5.0);
        p.heal(f64::NAN);
        assert_eq!(p.health(), 15.0);
        assert_eq!(p.set_absorption(-1.0), Err(Error::OutOfRange));
        // The attribute and the save keep an f32: more would be infinite there.
        assert_eq!(p.set_absorption(1e39), Err(Error::OutOfRange));
        p.set_absorption(4.0).unwrap();
        assert_eq!(p.absorption(), 4.0);
        run(&mut state, Vec::new());
        let got = received(&mut rx1).0;
        assert_eq!(attribute(&got, "minecraft:health"), Some((15.0, 40.0)));
        assert_eq!(attribute(&got, "minecraft:absorption"), Some((4.0, 16.0)));
    }

    // pmmp: Entity::kill, Entity::isAlive, Player::respawn
    #[test]
    fn a_respawn_waits_for_the_api_death_to_run() {
        let (mut state, [mut rx1, _rx2]) = two();
        let p = player_mut(&mut state, 1);
        let bread = Stack::new(ItemKind::Bread.default_type(), 5);
        assert!(inventory::give(p, bread).is_empty());
        assert!(p.kill());
        // The client asks to respawn in the same input phase, before the death runs.
        let session = crate::tick::SessionId::new(1);
        damage::respawn(&mut state, session);
        assert!(!player_mut(&mut state, 1).is_alive(), "not revived first");
        run(&mut state, Vec::new());
        let deaths = received(&mut rx1)
            .0
            .iter()
            .filter(|p| matches!(p, Packet::DeathInfo(_)))
            .count();
        assert_eq!(deaths, 1);
        let p = player_mut(&mut state, 1);
        assert!(
            inventory::take_all(p).iter().all(Stack::is_empty),
            "dropped"
        );
        damage::respawn(&mut state, session);
        assert!(player_mut(&mut state, 1).is_alive());
    }

    // pmmp: Entity::setRotation
    /// A rotation reads at once and reaches the client and the viewer by the end of the tick, in
    /// the tick's one teleport packet when there is one.
    #[test]
    fn a_rotation_reaches_the_client_and_the_viewer_with_the_teleport_of_the_tick() {
        let (mut state, [mut rx1, mut rx2]) = two();
        let p = player_mut(&mut state, 1);
        for (yaw, pitch) in [
            (f64::NAN, 0.0),
            (0.0, f64::INFINITY),
            (1e300, 0.0),
            (0.0, 90.5),
        ] {
            assert_eq!(p.set_rotation(yaw, pitch), Err(Error::OutOfRange));
        }
        assert!(!marked(p), "a refused rotation marks nothing");
        p.set_rotation(-90.0, 30.0).unwrap();
        assert_eq!((p.yaw(), p.pitch()), (270.0, 30.0));
        let moves = |packets: Vec<Packet>| -> Vec<(f32, f32, [f32; 3])> {
            packets
                .into_iter()
                .filter_map(|p| match p {
                    Packet::MovePlayer(m) if m.mode == 2 => Some((m.yaw, m.pitch, m.position)),
                    _ => None,
                })
                .collect()
        };
        run(&mut state, Vec::new());
        let at = state.players[&crate::tick::SessionId::new(1)]
            .position
            .to_f32();
        assert_eq!(moves(received(&mut rx1).0), [(270.0, 30.0, at)]);
        assert_eq!(moves(received(&mut rx2).0), [(270.0, 30.0, at)]);
        let feet = Vec3::new(3.5, -60.0, 2.5);
        let id = Id::new(crate::tick::SessionId::new(1));
        let mut cx = crate::tick::Context { state: &mut state };
        cx.teleport(id, crate::world::Id::DEFAULT, feet).unwrap();
        player_mut(&mut state, 1).set_rotation(90.0, -10.0).unwrap();
        run(&mut state, Vec::new());
        let eye = (feet + Vec3::new(0.0, crate::tick::survival::EYE_HEIGHT, 0.0)).to_f32();
        assert_eq!(moves(received(&mut rx1).0), [(90.0, -10.0, eye)]);
        assert_eq!(moves(received(&mut rx2).0), [(90.0, -10.0, eye)]);
        run(&mut state, Vec::new());
        assert!(moves(received(&mut rx1).0).is_empty(), "sent once");
    }

    #[test]
    fn a_kill_drops_the_items_once_and_a_respawn_brings_the_player_back() {
        let (mut state, [mut rx1, _rx2]) = two();
        let p = player_mut(&mut state, 1);
        let bread = Stack::new(ItemKind::Bread.default_type(), 5);
        assert!(inventory::give(p, bread).is_empty());
        assert!(!p.respawn(), "alive");
        assert!(p.kill());
        assert!(!p.kill(), "already dead");
        assert!(!p.is_alive());
        assert_eq!(p.set_health(10.0), Ok(()));
        assert_eq!(p.health(), 0.0, "the dead stay dead");
        run(&mut state, Vec::new());
        let got = received(&mut rx1).0;
        let deaths = got
            .iter()
            .filter(|p| matches!(p, Packet::DeathInfo(d) if d.cause == damage::GENERIC_DEATH))
            .count();
        assert_eq!(deaths, 1);
        let p = player_mut(&mut state, 1);
        assert!(
            inventory::take_all(p).iter().all(Stack::is_empty),
            "dropped"
        );
        assert!(p.death_position().is_some());
        assert!(p.respawn());
        run(&mut state, Vec::new());
        let p = player_mut(&mut state, 1);
        assert!(p.is_alive());
        assert_eq!(p.health(), 20.0);
        run(&mut state, Vec::new());
        assert_eq!(
            received(&mut rx1)
                .0
                .iter()
                .filter(|p| matches!(p, Packet::DeathInfo(_)))
                .count(),
            0
        );
    }

    // pmmp: HungerManager::getFood, HungerManager::setFood, HungerManager::addFood,
    // pmmp: HungerManager::isHungry, HungerManager::getSaturation, HungerManager::setSaturation,
    // pmmp: HungerManager::addSaturation, HungerManager::getExhaustion,
    // pmmp: HungerManager::setExhaustion, HungerManager::exhaust
    #[test]
    fn hunger_values_are_set_within_their_bounds() {
        let (mut state, [mut rx1, _rx2]) = two();
        run(&mut state, Vec::new());
        let p = player_mut(&mut state, 1);
        assert!(!p.is_hungry());
        for bad in [-1.0, 20.5, f64::NAN] {
            assert_eq!(p.set_food(bad), Err(Error::OutOfRange));
            assert_eq!(p.set_saturation(bad), Err(Error::OutOfRange));
            assert_eq!(p.set_exhaustion(bad), Err(Error::OutOfRange));
        }
        assert_eq!(p.add_food(f64::INFINITY), Err(Error::OutOfRange));
        p.set_food(6.0).unwrap();
        p.add_food(3.0).unwrap();
        assert_eq!(p.food(), 9.0);
        assert!(p.is_hungry());
        p.add_food(50.0).unwrap();
        assert_eq!(p.food(), 20.0);
        p.set_saturation(1.0).unwrap();
        p.add_saturation(2.5).unwrap();
        assert_eq!(p.saturation(), 3.5);
        p.set_exhaustion(3.0).unwrap();
        assert_eq!(p.exhaustion(), 3.0);
        // Hunger runs for a survival player: 4 exhaustion cost a saturation point.
        p.exhaust(1.5).unwrap();
        assert_eq!((p.saturation(), p.exhaustion()), (2.5, 0.5));
        assert_eq!(p.exhaust(-1.0), Err(Error::OutOfRange));
        run(&mut state, Vec::new());
        let got = received(&mut rx1).0;
        assert_eq!(
            attribute(&got, "minecraft:player.saturation").map(|a| a.0),
            Some(2.5)
        );
    }

    #[test]
    fn an_exhaustion_the_api_set_survives_a_save_and_load() {
        let (mut state, _rx) = two();
        let p = player_mut(&mut state, 1);
        assert!(p.set_game_mode(GameMode::Creative));
        p.set_exhaustion(10.0).unwrap();
        run(&mut state, Vec::new());
        let p = player_mut(&mut state, 1);
        assert_eq!(
            p.exhaustion(),
            10.0,
            "a creative player does not get hungry"
        );
        let mut extra = Compound::new();
        crate::tick::survival::save(&p.survival, &mut extra);
        let mut loaded = Survival::default();
        crate::tick::survival::load(&mut loaded, &extra, &crate::custom::Registry::default());
        assert_eq!(loaded.exhaustion, 10.0);
    }

    // pmmp: ExperienceManager::getXpLevel, ExperienceManager::setXpLevel,
    // pmmp: ExperienceManager::addXpLevels, ExperienceManager::subtractXpLevels,
    // pmmp: ExperienceManager::getXpProgress, ExperienceManager::setXpProgress,
    // pmmp: ExperienceManager::addXp, ExperienceManager::subtractXp,
    // pmmp: ExperienceManager::getCurrentTotalXp, ExperienceManager::setCurrentTotalXp,
    // pmmp: ExperienceManager::getLifetimeTotalXp, Human::getEnchantmentSeed,
    // pmmp: Human::setEnchantmentSeed, Human::regenerateEnchantmentSeed
    #[test]
    fn experience_and_the_enchanting_seed_follow_pmmp() {
        let (mut state, [mut rx1, _rx2]) = two();
        let p = player_mut(&mut state, 1);
        p.add_xp(1395);
        assert_eq!((p.xp_level(), p.current_total_xp()), (30, 1395));
        assert_eq!(p.lifetime_total_xp(), 1395);
        p.subtract_xp(1395 - 352);
        assert_eq!((p.xp_level(), p.xp_progress()), (16, 0.0));
        assert_eq!(p.lifetime_total_xp(), 1395, "the lifetime total stays");
        assert_eq!(p.set_xp_level(24_792), Err(Error::OutOfRange));
        assert_eq!(p.add_xp_levels(u32::MAX), Err(Error::OutOfRange));
        p.set_xp_level(5).unwrap();
        p.add_xp_levels(2).unwrap();
        p.subtract_xp_levels(10);
        assert_eq!(p.xp_level(), 0);
        assert_eq!(p.set_xp_progress(1.0), Err(Error::OutOfRange));
        p.set_xp_progress(0.5).unwrap();
        assert_eq!(p.xp_progress(), 0.5);
        assert_eq!(p.set_current_total_xp(u32::MAX), Err(Error::OutOfRange));
        assert_eq!(p.set_lifetime_total_xp(1 << 31), Err(Error::OutOfRange));
        assert_eq!(
            (p.xp_level(), p.xp_progress()),
            (0, 0.5),
            "a refused call changes nothing"
        );
        p.set_xp_level(24_791).unwrap();
        p.add_xp(1);
        assert_eq!(
            p.xp_level(),
            24_791,
            "points added at the top level keep it"
        );
        let held = u32::try_from(p.current_total_xp()).unwrap();
        p.set_current_total_xp(held).unwrap();
        assert_eq!(p.xp_level(), 24_791, "the points held can be set again");
        p.set_current_total_xp(7).unwrap();
        assert_eq!((p.xp_level(), p.xp_progress()), (1, 0.0));
        p.set_enchantment_seed(42);
        assert_eq!(p.enchantment_seed(), 42);
        p.regenerate_enchantment_seed();
        assert_eq!(p.enchantment_seed(), 0);
        run(&mut state, Vec::new());
        assert_ne!(state.players.values().next().unwrap().enchantment_seed(), 0);
        let got = received(&mut rx1).0;
        assert_eq!(
            attribute(&got, "minecraft:player.level").map(|a| a.0),
            Some(1.0)
        );
    }

    // pmmp: Living::getEffects, EffectCollection::all, EffectCollection::get,
    // pmmp: EffectCollection::has, EffectManager::add, EffectCollection::add,
    // pmmp: EffectManager::remove, EffectCollection::remove, EffectCollection::clear
    #[test]
    fn effects_are_added_read_and_removed_as_pmmp() {
        let (mut state, [mut rx1, _rx2]) = two();
        let p = player_mut(&mut state, 1);
        let speed = effect::Type::Speed.instance().with_duration_ticks(200);
        assert!(p.add_effect(speed.with_amplifier(1)));
        assert!(!p.add_effect(speed.with_duration_ticks(100)), "weaker");
        let haste = effect::Type::Haste.instance();
        assert!(p.add_effect(haste.with_duration_ticks(u32::MAX)));
        assert!(p.has_effect(effect::Type::Speed));
        let haste = p.effect(effect::Type::Haste).unwrap();
        assert_eq!(haste.duration_ticks(), crate::effect::MAX_DURATION);
        let all: Vec<effect::Type> = p.effects().map(Instance::effect).collect();
        assert_eq!(all, [effect::Type::Haste, effect::Type::Speed]);
        assert!(p.remove_effect(effect::Type::Speed));
        assert!(!p.remove_effect(effect::Type::Speed));
        p.clear_effects();
        assert_eq!(p.effects().count(), 0);
        run(&mut state, Vec::new());
        let ops: Vec<u8> = received(&mut rx1)
            .0
            .iter()
            .filter_map(|p| match p {
                Packet::MobEffect(m) => Some(m.operation),
                _ => None,
            })
            .collect();
        // Add, add, remove, remove.
        assert_eq!(ops, [1, 1, 3, 3]);
    }

    /// The entity data of the last `SetActorData` in `packets`.
    fn last_data(packets: &[Packet]) -> Option<&crate::protocol::EntityMetadata> {
        packets.iter().rev().find_map(|p| match p {
            Packet::SetActorData(d) => Some(&d.entity_metadata),
            _ => None,
        })
    }

    // pmmp: EffectInstance::setVisible, EffectInstance::setAmbient, EffectInstance::isInfinite
    /// An instance's particles, faint particles and endless duration reach the player as BDS
    /// sends `/effect <player> resistance 600 255 true` (P6 `potions`: `MobEffect` with
    /// `Particles` false, key 131 still 0) and as PMMP packs the visible effects.
    #[test]
    fn effects_keep_their_particles_and_endless_duration() {
        let (mut state, [mut rx1, _rx2]) = two();
        run(&mut state, Vec::new());
        received(&mut rx1);
        let p = player_mut(&mut state, 1);
        let hidden = effect::Type::Resistance
            .instance()
            .with_duration_ticks(12_000)
            .with_amplifier(255)
            .with_visible(false);
        assert!(p.add_effect(hidden));
        assert_eq!(p.effect(effect::Type::Resistance), Some(hidden));
        run(&mut state, Vec::new());
        let got = received(&mut rx1).0;
        let mob_effects = |got: &[Packet]| -> Vec<_> {
            got.iter()
                .filter_map(|p| match p {
                    Packet::MobEffect(m) => {
                        Some((m.effect_type, m.particles, m.ambient, m.duration))
                    }
                    _ => None,
                })
                .collect()
        };
        assert_eq!(mob_effects(&got), [(11, false, false, 12_000)]);
        let effects_data = |data: Option<&crate::protocol::EntityMetadata>| {
            data.and_then(|d| d.get(&EntityDataKey::VisibleMobEffects.into()).cloned())
        };
        // A hidden effect is not one of the visible effects: no change to send.
        assert_eq!(effects_data(last_data(&got)), None);
        let p = player_mut(&mut state, 1);
        let faint = effect::Type::Speed
            .instance()
            .with_ambient(true)
            .with_infinite(true);
        assert!(p.add_effect(faint));
        run(&mut state, Vec::new());
        let got = received(&mut rx1).0;
        assert_eq!(mob_effects(&got), [(1, true, true, -1)]);
        // Speed (ID 1) with the ambient bit; the hidden resistance stays out.
        assert_eq!(
            effects_data(last_data(&got)),
            Some(MetadataValue::Int64(1 << 1 | 1))
        );
        let speed = player_mut(&mut state, 1)
            .effect(effect::Type::Speed)
            .unwrap();
        assert!(speed.is_infinite() && speed.is_ambient() && speed.is_visible());
    }

    // pmmp: Entity::isOnFire, Entity::getFireTicks, Entity::setFireTicks, Entity::setOnFire,
    // pmmp: Living::setOnFire, Entity::extinguish, Living::getAirSupplyTicks,
    // pmmp: Living::getMaxAirSupplyTicks, Living::isBreathing, Entity::getFallDistance,
    // pmmp: Entity::setFallDistance, Entity::resetFallDistance, Player::resetFallDistance
    #[test]
    fn fire_air_and_falls_read_and_change() {
        let (mut state, [_rx1, mut rx2]) = two();
        let p = player_mut(&mut state, 1);
        assert!(!p.is_on_fire());
        assert_eq!(p.set_fire_ticks(0x8000), Err(Error::OutOfRange));
        p.set_on_fire(3);
        assert_eq!(p.fire_ticks(), 60);
        assert!(p.is_on_fire());
        p.set_fire_ticks(100).unwrap();
        assert_eq!(p.fire_ticks(), 100);
        run(&mut state, Vec::new());
        // The viewer sees the burning flag.
        let burning = received(&mut rx2).0.iter().any(|p| match p {
            Packet::SetActorData(d) => matches!(
                d.entity_metadata.get(&EntityDataKey::Flags.into()),
                Some(MetadataValue::Int64(w))
                    if FlagSet::<EntityDataFlag>::from_words(&[w.cast_unsigned()])
                        .is_ok_and(|f| f.contains(EntityDataFlag::OnFire))
            ),
            _ => false,
        });
        assert!(burning);
        let p = player_mut(&mut state, 1);
        p.set_fire_ticks(0).unwrap();
        assert!(!p.is_on_fire());
        assert_eq!(p.air_supply_ticks(), p.max_air_supply_ticks());
        assert!(p.is_breathing());
        assert_eq!(p.set_fall_distance(f64::NAN), Err(Error::OutOfRange));
        p.set_fall_distance(7.0).unwrap();
        assert_eq!(p.fall_distance(), 7.0);
        p.reset_fall_distance();
        assert_eq!(p.fall_distance(), 0.0);
    }

    /// The fall setters mark the player's data for the next save snapshot, and a breathing
    /// change is sent by the end of the tick.
    #[test]
    fn fall_and_breathing_setters_mark_what_they_change() {
        let (mut state, _rx) = two();
        let p = player_mut(&mut state, 1);
        p.data_dirty = false;
        p.set_fall_distance(7.0).unwrap();
        assert!(p.data_dirty, "the set fall is saved");
        p.data_dirty = false;
        p.reset_fall_distance();
        assert!(p.data_dirty, "the forgotten fall is saved");
        assert!(!marked(p));
        p.set_breathing(false);
        assert!(marked(p), "the breathing flag is sent");
    }

    // pmmp: Entity::isOnGround, Living::isSneaking, Player::isSneakPressed, Living::isSprinting,
    // pmmp: Living::isSwimming, Living::isGliding, Player::isUsingItem, Entity::getDirectionVector
    #[test]
    fn movement_state_reads_what_the_client_reported() {
        let (mut state, _rx) = two();
        let p = player_mut(&mut state, 1);
        assert!(!p.is_sneaking() && !p.is_sneak_pressed() && !p.is_sprinting());
        assert!(!p.is_swimming() && !p.is_gliding() && !p.is_using_item());
        let m = &mut p.survival.motion;
        m.sneaking = true;
        m.sneak_pressed = true;
        m.sprinting = true;
        m.swimming = true;
        m.gliding = true;
        m.on_ground = true;
        m.yaw = 90.0;
        m.pitch = 0.0;
        assert!(p.is_sneaking() && p.is_sneak_pressed() && p.is_sprinting());
        assert!(p.is_swimming() && p.is_gliding() && p.is_on_ground());
        assert_eq!((p.yaw(), p.pitch()), (90.0, 0.0));
        assert!(p.direction().x < -0.99, "90 looks west");
    }

    // pmmp: Player::isFlying, Player::setFlying, Player::getAllowFlight, Player::setAllowFlight,
    // pmmp: Player::getFlightSpeedMultiplier, Player::setFlightSpeedMultiplier,
    // pmmp: Player::hasBlockCollision, Player::setHasBlockCollision
    #[test]
    fn flight_and_block_collision_reach_the_ability_layers() {
        let (mut state, [mut rx1, _rx2]) = two();
        let p = player_mut(&mut state, 1);
        assert!(!p.allow_flight() && !p.is_flying() && p.has_block_collision());
        assert_eq!(p.flight_speed(), ABILITY_BASE_FLY_SPEED);
        p.set_allow_flight(true);
        p.set_flying(true);
        assert_eq!(p.set_flight_speed(f32::NAN), Err(Error::OutOfRange));
        p.set_flight_speed(0.1).unwrap();
        p.set_block_collision(false);
        assert!(p.allow_flight() && p.is_flying() && !p.has_block_collision());
        run(&mut state, Vec::new());
        let (values, speed, layers) = abilities(&received(&mut rx1).0).expect("abilities sent");
        assert!(values.contains(Ability::MayFly));
        assert!(values.contains(Ability::Flying));
        assert!(values.contains(Ability::NoClip));
        assert_eq!(
            (speed, layers),
            (0.1, 2),
            "PMMP's spectator layer without collision"
        );
        let p = player_mut(&mut state, 1);
        p.set_allow_flight(false);
        p.set_flying(false);
        p.set_block_collision(true);
        run(&mut state, Vec::new());
        let (values, _, layers) = abilities(&received(&mut rx1).0).expect("abilities sent");
        assert!(!values.contains(Ability::MayFly) && !values.contains(Ability::NoClip));
        assert_eq!(layers, 1);
    }

    // pmmp: Player::getSpawn, Player::hasValidCustomSpawn, Player::setSpawn,
    // pmmp: Player::getDeathPosition
    #[test]
    fn a_set_spawn_point_is_where_the_player_respawns() {
        let (mut state, _rx) = two();
        let p = player_mut(&mut state, 1);
        assert_eq!(p.spawn(), None);
        assert_eq!(p.death_position(), None);
        assert_eq!(
            p.set_spawn(Some((
                &"w".repeat(crate::storage::MAX_FILE_NAME + 1),
                BlockPos::new(0, 0, 0)
            ))),
            Err(Error::TooLong)
        );
        let at = BlockPos::new(3, -60, 4);
        p.set_spawn(Some(("world", at))).unwrap();
        assert_eq!(p.spawn(), Some(("world", at)));
        p.kill();
        run(&mut state, Vec::new());
        assert!(player_mut(&mut state, 1).respawn());
        run(&mut state, Vec::new());
        let p = player_mut(&mut state, 1);
        assert_eq!(p.position(), crate::math::Vec3::new(3.5, -60.0, 4.5));
        p.set_spawn(None).unwrap();
        assert_eq!(p.spawn(), None);
    }

    // pmmp: Entity::getNameTag, Entity::setNameTag, Entity::getScoreTag, Entity::setScoreTag,
    // pmmp: Entity::isInvisible, Entity::setInvisible, Entity::isSilent, Entity::setSilent,
    // pmmp: Entity::hasNoClientPredictions, Entity::setNoClientPredictions
    #[test]
    fn tags_and_flags_reach_the_player_and_its_viewers() {
        let (mut state, [mut rx1, mut rx2]) = two();
        let before = state.hash();
        let p = player_mut(&mut state, 1);
        assert_eq!((p.name_tag(), p.score_tag()), ("player1", ""));
        assert_eq!(
            p.set_name_tag("x".repeat(MAX_TEXT + 1)),
            Err(Error::TooLong)
        );
        p.set_name_tag("§cRed").unwrap();
        p.set_score_tag("10 kills").unwrap();
        p.set_invisible(true);
        p.set_silent(true);
        p.set_no_client_predictions(true);
        assert!(p.is_invisible() && p.is_silent() && p.has_no_client_predictions());
        assert_eq!((p.name_tag(), p.score_tag()), ("§cRed", "10 kills"));
        assert_ne!(state.hash(), before);
        run(&mut state, Vec::new());
        for rx in [&mut rx1, &mut rx2] {
            let data: Vec<_> = received(rx)
                .0
                .into_iter()
                .filter_map(|p| match p {
                    Packet::SetActorData(d) => Some(d.entity_metadata),
                    _ => None,
                })
                .collect();
            assert_eq!(data.len(), 1);
            let m = &data[0];
            assert_eq!(
                m.get(&EntityDataKey::Name.into()),
                Some(&MetadataValue::String("§cRed".to_owned()))
            );
            assert_eq!(
                m.get(&EntityDataKey::Score.into()),
                Some(&MetadataValue::String("10 kills".to_owned()))
            );
            let Some(MetadataValue::Int64(w)) = m.get(&EntityDataKey::Flags.into()) else {
                panic!("flags sent");
            };
            let flags = FlagSet::<EntityDataFlag>::from_words(&[w.cast_unsigned()]).unwrap();
            for f in [
                EntityDataFlag::Invisible,
                EntityDataFlag::Silent,
                EntityDataFlag::NoAi,
            ] {
                assert!(flags.contains(f), "{f:?}");
            }
        }
        // Nothing changed: nothing more is sent.
        run(&mut state, Vec::new());
        assert!(
            !received(&mut rx2)
                .0
                .iter()
                .any(|p| matches!(p, Packet::SetActorData(_)))
        );
    }

    // pmmp: Living::setAirSupplyTicks, Living::getMaxAirSupplyTicks, Living::setMaxAirSupplyTicks,
    // pmmp: Living::setBreathing
    #[test]
    fn the_air_supply_is_set_within_its_maximum_and_refills_to_it() {
        let (mut state, [_rx1, mut rx2]) = two();
        let p = player_mut(&mut state, 1);
        assert_eq!(p.max_air_supply_ticks(), 300);
        assert_eq!(p.set_max_air_supply_ticks(0), Err(Error::OutOfRange));
        assert_eq!(p.set_max_air_supply_ticks(40_000), Err(Error::OutOfRange));
        p.set_max_air_supply_ticks(100).unwrap();
        assert_eq!((p.air_supply_ticks(), p.max_air_supply_ticks()), (100, 100));
        for bad in [-21, 101] {
            assert_eq!(p.set_air_supply_ticks(bad), Err(Error::OutOfRange));
        }
        p.set_air_supply_ticks(50).unwrap();
        p.set_breathing(false);
        run(&mut state, Vec::new());
        // Out of water: breathing again and 4 ticks of air back each tick (BDS).
        let p = player_mut(&mut state, 1);
        assert_eq!(p.air_supply_ticks(), 54);
        assert!(p.is_breathing());
        let got = received(&mut rx2).0;
        let data = last_data(&got).expect("the viewer sees the air");
        assert!(matches!(
            data.get(&EntityDataKey::AirSupplyMax.into()),
            Some(MetadataValue::Int16(100))
        ));
        for _ in 0..12 {
            run(&mut state, Vec::new());
        }
        assert_eq!(player_mut(&mut state, 1).air_supply_ticks(), 100, "full");
    }

    /// The entity flags of `data`.
    fn flags_of(data: &crate::protocol::EntityMetadata) -> FlagSet<EntityDataFlag> {
        let Some(MetadataValue::Int64(w)) = data.get(&EntityDataKey::Flags.into()) else {
            panic!("flags sent");
        };
        FlagSet::from_words(&[w.cast_unsigned()]).unwrap()
    }

    // pmmp: Living::setSneaking, Living::setSprinting, Living::setSwimming, Living::setGliding,
    // pmmp: Living::getMovementSpeed, Living::setMovementSpeed
    #[test]
    fn movement_flags_and_speed_reach_the_client_and_viewers() {
        let (mut state, [mut rx1, mut rx2]) = two();
        let p = player_mut(&mut state, 1);
        assert_eq!(p.movement_speed(), 0.1);
        for bad in [-0.1, f64::NAN, f64::INFINITY, 1e39] {
            assert_eq!(p.set_movement_speed(bad), Err(Error::OutOfRange));
        }
        p.set_movement_speed(0.2).unwrap();
        p.set_sneaking(true);
        p.set_sprinting(true);
        p.set_swimming(true);
        p.set_gliding(true);
        assert!(p.is_sneaking() && p.is_sprinting() && p.is_swimming() && p.is_gliding());
        run(&mut state, Vec::new());
        let got = received(&mut rx1).0;
        let (speed, _) = attribute(&got, "minecraft:movement").expect("speed sent");
        assert!((speed - 0.26).abs() < 1e-6, "{speed}");
        let flags = flags_of(last_data(&received(&mut rx2).0).expect("the viewer sees it"));
        for f in [
            EntityDataFlag::Sneaking,
            EntityDataFlag::Sprinting,
            EntityDataFlag::Swimming,
            EntityDataFlag::Gliding,
        ] {
            assert!(flags.contains(f), "{f:?}");
        }
        assert_eq!(player_mut(&mut state, 1).movement_speed(), 0.2);
    }

    // pmmp: ExperienceManager::setLifetimeTotalXp, ExperienceManager::getRemainderXp,
    // pmmp: ExperienceManager::canPickupXp, ExperienceManager::canAttractXpOrbs,
    // pmmp: ExperienceManager::setCanAttractXpOrbs
    #[test]
    fn experience_remainder_lifetime_and_orb_settings() {
        let (mut state, _rx) = two();
        let p = player_mut(&mut state, 1);
        p.add_xp(10);
        // Level 1 needs 7 points: 3 into it.
        assert_eq!((p.xp_level(), p.remainder_xp()), (1, 3));
        assert_eq!(
            p.set_lifetime_total_xp(i32::MAX.cast_unsigned() + 1),
            Err(Error::OutOfRange)
        );
        p.set_lifetime_total_xp(500).unwrap();
        assert_eq!(p.lifetime_total_xp(), 500);
        assert!(p.can_pickup_xp());
        p.survival.xp_cooldown = 2;
        assert!(!p.can_pickup_xp());
        assert!(p.can_attract_xp_orbs());
        p.set_can_attract_xp_orbs(false);
        assert!(!p.can_attract_xp_orbs());
    }

    // pmmp: HungerManager::isEnabled, HungerManager::setEnabled
    #[test]
    fn hunger_turned_off_for_one_player_spends_nothing() {
        let (mut state, _rx) = two();
        run(&mut state, Vec::new());
        let p = player_mut(&mut state, 1);
        assert!(p.hunger_enabled());
        p.set_hunger_enabled(false);
        assert!(!p.hunger_enabled());
        p.exhaust(8.0).unwrap();
        assert_eq!((p.saturation(), p.exhaustion()), (5.0, 0.0));
        run(&mut state, Vec::new());
        let p = player_mut(&mut state, 1);
        p.exhaust(8.0).unwrap();
        assert_eq!(p.saturation(), 5.0, "still off after a tick");
        p.set_hunger_enabled(true);
        run(&mut state, Vec::new());
        let p = player_mut(&mut state, 1);
        p.exhaust(8.0).unwrap();
        assert_eq!(p.saturation(), 3.0);
        // The other player was never touched.
        assert!(player_mut(&mut state, 2).hunger_enabled());
    }

    // pmmp: Living::getArmorPoints, Entity::getLastDamageCause, Entity::setLastDamageCause
    #[test]
    fn armour_points_and_the_last_damage_cause() {
        let (mut state, _rx) = two();
        let p = player_mut(&mut state, 1);
        assert_eq!(p.armor_points(), 0);
        let helmet = Stack::new(ItemKind::DiamondHelmet.default_type(), 1);
        let points = u32::from(helmet.item().unwrap().defense_points());
        inventory::set_armor(p, 0, helmet);
        assert_eq!(p.armor_points(), points);
        assert_eq!(p.last_damage_cause(), None);
        assert!(crate::tick::survival::damage(
            &mut state,
            crate::tick::SessionId::new(1),
            2.0,
            crate::tick::survival::Damage::Starvation
        ));
        let p = player_mut(&mut state, 1);
        assert_eq!(
            p.last_damage_cause(),
            Some((DamageCause::Starvation, DamageSource::None))
        );
        p.set_last_damage_cause(Some((DamageCause::Magic, DamageSource::None)));
        assert_eq!(
            p.last_damage_cause(),
            Some((DamageCause::Magic, DamageSource::None))
        );
        p.set_last_damage_cause(None);
        assert_eq!(p.last_damage_cause(), None);
    }

    // pmmp: Entity::canClimb, Entity::setCanClimb, Entity::canClimbWalls, Entity::setCanClimbWalls,
    // pmmp: Player::hasAutoJump, Player::setAutoJump, Player::setMotion
    #[test]
    fn climbing_auto_jump_and_motion_reach_the_client() {
        let (mut state, [mut rx1, mut rx2]) = two();
        let p = player_mut(&mut state, 1);
        assert!(p.can_climb() && !p.can_climb_walls() && p.auto_jump());
        p.set_can_climb(false);
        p.set_can_climb_walls(true);
        p.set_auto_jump(false);
        assert_eq!(
            p.set_motion(Vec3::new(f64::NAN, 0.0, 0.0)),
            Err(Error::OutOfRange)
        );
        p.set_motion(Vec3::new(0.0, 5.0, 0.0)).unwrap();
        p.set_motion(Vec3::new(0.0, 1.0, 0.0)).unwrap();
        assert!(!p.can_climb() && p.can_climb_walls() && !p.auto_jump());
        run(&mut state, Vec::new());
        let got = received(&mut rx1).0;
        let flags = flags_of(last_data(&got).expect("data sent"));
        assert!(!flags.contains(EntityDataFlag::Climb));
        assert!(flags.contains(EntityDataFlag::WallClimbing));
        assert!(
            got.iter()
                .any(|p| matches!(p, Packet::UpdateAdventureSettings(a) if !a.auto_jump))
        );
        let motions = |packets: &[Packet]| -> Vec<[f32; 3]> {
            packets
                .iter()
                .filter_map(|p| match p {
                    Packet::SetActorMotion(m) => Some(m.velocity),
                    _ => None,
                })
                .collect()
        };
        // The last motion of the tick, once, to the player and its viewer.
        assert_eq!(motions(&got), [[0.0, 1.0, 0.0]]);
        assert_eq!(motions(&received(&mut rx2).0), [[0.0, 1.0, 0.0]]);
        run(&mut state, Vec::new());
        assert!(motions(&received(&mut rx1).0).is_empty());
    }

    /// The skin ID counts toward the skin bound as it does at a login.
    #[test]
    fn a_skin_whose_texts_pass_the_bound_is_refused() {
        let (mut state, _receivers) = two();
        let p = player_mut(&mut state, 1);
        let mut skin = p.skin().clone();
        skin.skin_id = "a".repeat(crate::entity::MAX_SKIN_BYTES);
        assert_eq!(p.set_skin(skin), Err(Error::OutOfRange));
    }

    /// A geometry that is not JSON (empty included) is refused: the protocol requires JSON.
    #[test]
    fn a_skin_without_json_geometry_is_refused() {
        let (mut state, _receivers) = two();
        let p = player_mut(&mut state, 1);
        let mut skin = p.skin().clone();
        skin.skin_geometry = Vec::new();
        assert_eq!(p.set_skin(skin.clone()), Err(Error::OutOfRange));
        skin.skin_geometry = b"{}".to_vec();
        p.set_skin(skin).unwrap();
    }

    /// Each persona piece counts its wire bytes toward the skin bound, empty texts or not, so a
    /// skin's piece count is bounded too.
    #[test]
    fn a_skin_of_too_many_empty_pieces_is_refused() {
        let (mut state, _receivers) = two();
        let p = player_mut(&mut state, 1);
        let mut skin = p.skin().clone();
        skin.persona_pieces = vec![crate::protocol::PersonaPiece::default(); 200_000];
        assert_eq!(p.set_skin(skin), Err(Error::OutOfRange));
    }

    // pmmp: Human::emote, Human::setSkin
    #[test]
    fn emotes_reach_the_viewers_and_skins_everyone() {
        let (mut state, [mut rx1, mut rx2]) = two();
        let p = player_mut(&mut state, 1);
        assert_eq!(p.emote(""), Err(Error::OutOfRange));
        assert_eq!(p.emote(&"e".repeat(129)), Err(Error::OutOfRange));
        p.emote("4c8ae710-df2e-47cd-814d-cc7bf21a3d67").unwrap();
        let mut bad = p.skin().clone();
        bad.skin_image_width = 63;
        assert_eq!(p.set_skin(bad), Err(Error::OutOfRange));
        let mut skin = p.skin().clone();
        skin.skin_id = "new".to_owned();
        p.set_skin(skin).unwrap();
        assert_eq!(p.skin().skin_id, "new");
        run(&mut state, Vec::new());
        let (got1, got2) = (received(&mut rx1).0, received(&mut rx2).0);
        let emotes = |g: &[Packet]| g.iter().filter(|p| matches!(p, Packet::Emote(_))).count();
        assert_eq!((emotes(&got1), emotes(&got2)), (0, 1), "the viewers only");
        for got in [&got1, &got2] {
            assert!(
                got.iter()
                    .any(|p| matches!(p, Packet::PlayerSkin(s) if s.skin.skin_id == "new"))
            );
        }
    }

    // pmmp: Player::setDeathPosition, Player::getInAirTicks, Entity::getHorizontalFacing,
    // pmmp: Player::canInteract
    #[test]
    fn death_position_air_ticks_facing_and_reach() {
        let (mut state, [_rx1, mut rx2]) = two();
        let p = player_mut(&mut state, 1);
        assert_eq!(
            p.set_death_position(Some((
                &"w".repeat(crate::storage::MAX_FILE_NAME + 1),
                BlockPos::new(0, 0, 0)
            ))),
            Err(Error::TooLong)
        );
        let at = BlockPos::new(1, -60, 2);
        p.set_death_position(Some(("world", at))).unwrap();
        assert_eq!(p.death_position(), Some(("world", at)));
        // Nothing reported a landing: the player is in the air from the reset on.
        p.reset_fall_distance();
        assert_eq!(p.in_air_ticks(), 0);
        p.survival.motion.on_ground = false;
        run(&mut state, Vec::new());
        let got = received(&mut rx2).0;
        assert!(matches!(
            last_data(&got).and_then(|d| d.get(&EntityDataKey::PlayerHasDied.into())),
            Some(MetadataValue::Byte(1))
        ));
        let p = player_mut(&mut state, 1);
        assert_eq!(p.in_air_ticks(), 1);
        p.reset_fall_distance();
        assert_eq!(p.in_air_ticks(), 0);
        p.set_death_position(None).unwrap();
        assert_eq!(p.death_position(), None);
        for (yaw, face) in [
            (0.0, Face::South),
            (44.9, Face::South),
            (45.0, Face::West),
            (180.0, Face::North),
            (270.0, Face::East),
            (359.9, Face::South),
        ] {
            p.survival.motion.yaw = yaw;
            assert_eq!(p.horizontal_facing(), face, "{yaw}");
        }
        // Looking south, level.
        p.survival.motion.yaw = 0.0;
        p.survival.motion.pitch = 0.0;
        let eye = p.eye_position();
        assert!(p.can_interact(eye + Vec3::new(0.0, 0.0, 4.0), 5.0));
        assert!(
            !p.can_interact(eye + Vec3::new(0.0, 0.0, 6.0), 5.0),
            "too far"
        );
        assert!(
            !p.can_interact(eye + Vec3::new(0.0, 0.0, -2.0), 5.0),
            "behind"
        );
        assert!(
            p.can_interact(eye + Vec3::new(0.0, 0.0, -0.5), 5.0),
            "just behind"
        );
        assert!(!p.can_interact(Vec3::new(f64::NAN, 0.0, 0.0), 5.0));
        assert!(!p.can_interact(eye, f64::INFINITY));
    }

    // pmmp: Player::hasItemCooldown, Player::getItemCooldownExpiry, Player::resetItemCooldown,
    // pmmp: Player::getItemUseDuration
    #[test]
    fn item_cooldowns_and_the_use_time_count_engine_ticks() {
        let (mut state, [mut rx1, _rx2]) = two();
        let id = Id::new(crate::tick::SessionId::new(1));
        let gone = Id::new(crate::tick::SessionId::new(9));
        let pearl = ItemKind::EnderPearl.default_type();
        let bread = ItemKind::Bread.default_type();
        let tick = state.tick;
        let mut cx = Context { state: &mut state };
        assert!(!cx.has_item_cooldown(id, pearl));
        assert!(cx.reset_item_cooldown(id, pearl, None));
        assert!(cx.has_item_cooldown(id, pearl));
        assert_eq!(cx.item_cooldown_expiry(id, pearl), Some(tick + 20));
        assert!(!cx.reset_item_cooldown(id, bread, None), "bread has none");
        assert!(!cx.reset_item_cooldown(id, bread, Some(0)));
        assert!(cx.reset_item_cooldown(id, bread, Some(5)));
        assert_eq!(cx.item_cooldown_expiry(id, bread), Some(tick + 5));
        assert!(!cx.reset_item_cooldown(gone, pearl, None));
        assert!(!cx.has_item_cooldown(gone, pearl));
        assert_eq!(cx.item_use_duration_ticks(id), None);
        cx.state
            .players
            .get_mut(&id.session())
            .unwrap()
            .survival
            .using = Some(food::Using {
            since: tick,
            item: bread,
        });
        assert_eq!(cx.item_use_duration_ticks(id), Some(0));
        assert_eq!(cx.item_use_duration_ticks(gone), None);
        for _ in 0..5 {
            run(&mut state, Vec::new());
        }
        let cx = Context { state: &mut state };
        assert!(!cx.has_item_cooldown(id, bread), "ended");
        assert!(cx.has_item_cooldown(id, pearl));
        let started = received(&mut rx1)
            .0
            .iter()
            .filter(|p| matches!(p, Packet::ClientStartItemCooldown(_)))
            .count();
        assert_eq!(started, 2);
    }

    // pmmp: Player::dropItem, Living::getLineOfSight, Living::getTargetBlock, Entity::isUnderwater,
    // pmmp: Entity::isInsideOfSolid
    #[test]
    fn a_player_drops_items_and_sees_the_ground_below() {
        let (mut state, _rx) = two();
        let id = Id::new(crate::tick::SessionId::new(1));
        let gone = Id::new(crate::tick::SessionId::new(9));
        let p = player_mut(&mut state, 1);
        p.survival.motion.pitch = 90.0;
        let feet = p.position();
        let ground = BlockPos::containing(feet);
        let below = BlockPos::new(ground.x, ground.y - 1, ground.z);
        let entities = Context { state: &mut state }
            .entities(crate::world::Id::DEFAULT)
            .count();
        let mut cx = Context { state: &mut state };
        assert!(!cx.is_player_underwater(id) && !cx.is_player_inside_solid(id));
        assert!(!cx.is_player_underwater(gone) && !cx.is_player_inside_solid(gone));
        let line = cx.line_of_sight(id, 10, 0);
        assert_eq!(line.last(), Some(&below), "{line:?}");
        assert!(line.iter().all(|b| b.x == below.x && b.z == below.z));
        assert_eq!(cx.target_block(id, 10), Some(below));
        assert_eq!(cx.line_of_sight(id, 10, 1), [below]);
        assert_eq!(cx.target_block(gone, 10), None);
        let empty = Stack::new(ItemKind::Bread.default_type(), 0);
        assert!(cx.drop_from(id, empty).is_err());
        let bread = Stack::new(ItemKind::Bread.default_type(), 3);
        assert_eq!(cx.drop_from(gone, bread.clone()), Err(bread.clone()));
        assert_eq!(cx.drop_from(id, bread), Ok(()));
        assert_eq!(
            Context { state: &mut state }
                .entities(crate::world::Id::DEFAULT)
                .count(),
            entities + 1
        );
    }

    /// A block only clients know, and one nobody knows (shown as `info_update`), stops a look as
    /// any block the client shows does.
    #[test]
    fn a_look_stops_at_a_block_only_clients_know() {
        let (mut state, _rx) = two();
        let id = Id::new(crate::tick::SessionId::new(1));
        let p = player_mut(&mut state, 1);
        p.survival.motion.pitch = 90.0;
        let at_feet = BlockPos::containing(p.position());
        for name in ["minecraft:dripstone_block", "minecraft:not_a_block"] {
            let stored = Compound::from([
                ("name".to_owned(), Value::String(name.to_owned())),
                ("states".to_owned(), Value::Compound(Compound::new())),
                ("version".to_owned(), Value::Int(18_168_865)),
            ]);
            state.worlds[0].put_unknown(at_feet, stored).unwrap();
            let cx = Context { state: &mut state };
            assert_eq!(cx.target_block(id, 10), Some(at_feet), "{name}");
            assert_eq!(cx.line_of_sight(id, 10, 0).last(), Some(&at_feet), "{name}");
        }
    }

    // pmmp: Player::attack
    /// An API hit follows the hit cooldown and shows the viewers only the hit that starts it.
    #[test]
    fn an_api_hit_on_a_player_follows_the_hit_cooldown() {
        let (mut state, [_rx1, mut rx2]) = two();
        let id = Id::new(crate::tick::SessionId::new(1));
        let gone = Id::new(crate::tick::SessionId::new(9));
        let mut cx = Context { state: &mut state };
        assert!(!cx.attack_player(id, f64::NAN));
        assert!(!cx.attack_player(id, -1.0));
        assert!(!cx.attack_player(gone, 1.0));
        assert!(cx.attack_player(id, 5.0));
        assert_eq!(cx.player(id).map(Player::health), Some(15.0));
        // Within the cooldown only a stronger hit counts its difference.
        assert!(!cx.attack_player(id, 4.0));
        assert!(cx.attack_player(id, 7.0));
        assert_eq!(cx.player(id).map(Player::health), Some(13.0));
        run(&mut state, Vec::new());
        let hurts = received(&mut rx2)
            .0
            .iter()
            .filter(|p| {
                matches!(p, Packet::ActorEvent(e)
                    if e.event_type == u8::from(crate::protocol::consts::ActorEvent::Hurt))
            })
            .count();
        assert_eq!(hurts, 1);
    }
}
