//! Survival rules of players: movement, health, hunger, experience, effects, damage, death and
//! respawn, game modes and abilities, attributes, combat, sleeping and the world clock
//! (PLAN.md 10.7 "플레이어", PMMP `Player`, `Human`, `Living`, `HungerManager`,
//! `ExperienceManager`, `EffectManager`).
//!
//! Every rule runs on the tick thread in a serial phase. A player's survival state changes only
//! here; other modules call the functions below. The client hears about changes at the end of
//! [`tick`]: changed attributes (`UpdateAttributes`), changed entity data (`SetActorData`), and the
//! packets the rules queue on the way (effects, respawn, hurt animations). Nothing is sent to a
//! player before it is spawned (`SetLocalPlayerAsInitialised`), so the spawn window holds only the
//! start-game packets ([`super::spawn`]).

mod attributes;
mod clock;
mod damage;
mod effects;
mod food;
mod movement;
mod saved;
mod sound;

use std::collections::BTreeMap;
use std::ops::Bound;

use super::{Phase, Player, SessionId, Settings, State, inventory};
use crate::config::{Difficulty, GameMode};
use crate::effect;
use crate::item::Stack;
use crate::math::Vec3;
use crate::nbt::Compound;
use crate::protocol::consts::{MoveMode, SoundEvent};
use crate::protocol::packet::{Animate, Packet, PlayerAction, PlayerAuthInput, Respawn};
use crate::random::{self, Random};
use crate::save::WorldIndex;
use crate::world::BlockPos;

pub(super) use attributes::{
    adventure_settings, all_attributes, full_metadata, game_rules, update_abilities,
};
pub(super) use clock::{Clock, clock_definition, clock_state, sleeping_players};
pub(super) use damage::Damage;
pub(super) use saved::game_mode_id;

/// Eye height above the feet (PMMP `Human` size 1.62).
pub(super) const EYE_HEIGHT: f64 = 1.62;
/// The health of a player without effects (PMMP `Attribute::HEALTH` default).
const MAX_HEALTH: f64 = 20.0;
/// The largest food level (PMMP `Attribute::HUNGER`).
const MAX_FOOD: f64 = 20.0;
/// The largest saturation (PMMP `Attribute::SATURATION`).
const MAX_SATURATION: f64 = 20.0;
/// The saturation of a new or respawned player: BDS sends 5 (P6 facts 7; PMMP starts at 20).
const DEFAULT_SATURATION: f64 = 5.0;
/// Air supply in ticks (PMMP `Living::DEFAULT_BREATH_TICKS`).
const MAX_AIR: i16 = 300;
/// The most held-back death drops: one inventory (36 slots, 4 armour pieces, the off hand).
const MAX_HELD_BACK: usize = 41;

/// A player's survival state.
#[derive(Debug)]
pub(super) struct Survival {
    /// The game mode, once set from the saved data or a change; `None` uses the server default.
    game_mode: Option<GameMode>,
    /// Health in half hearts, 0 to [`Self::max_health`]; 0 is dead.
    health: f64,
    /// Extra health absorbed first (PMMP `Living::getAbsorption`), 0 or more.
    absorption: f64,
    /// Food level, 0 to 20 (PMMP keeps a float).
    food: f64,
    /// Saturation, 0 to 20.
    saturation: f64,
    /// Exhaustion, 0 to below 4 between calls.
    exhaustion: f64,
    /// Ticks of the food cycle, 0 to 79 (PMMP `HungerManager::$foodTickTimer`).
    food_timer: u32,
    /// Experience level, 0 to 24,791.
    xp_level: u32,
    /// Progress through the level, 0 to below 1.
    xp_progress: f64,
    /// Experience collected since the last death (PMMP `XpTotal`), at most `i32::MAX`.
    xp_lifetime: u32,
    /// Ticks until the next orb pickup, 0 to 2 (PMMP `ExperienceManager::$xpCooldown`).
    xp_cooldown: u8,
    /// The enchanting seed (PMMP `XpSeed`), drawn on the first tick if the save has none.
    xp_seed: Option<i32>,
    /// Active effects; at most one per effect type.
    effects: BTreeMap<effect::Type, effects::Active>,
    /// Ticks the player keeps burning, 0 to 32,767.
    fire_ticks: u16,
    /// Air supply in ticks, -20 to 300.
    air: i16,
    /// Whether the player breathes (not under water, or refilled since).
    breathing: bool,
    /// Blocks fallen since the player last stood on something.
    fall_distance: f64,
    /// The respawn point set by sleeping (PMMP `Player::setSpawn`).
    spawn: Option<Point>,
    /// Where the player last died.
    death: Option<Point>,
    /// The bed the player sleeps in.
    sleeping: Option<BlockPos>,
    /// Movement and input state.
    motion: movement::Motion,
    /// Ticks the player ignores damage after respawning (PMMP `noDamageTicks`).
    no_damage_ticks: u32,
    /// Ticks since the last hit within which a weaker hit does nothing (PMMP `attackTime`).
    attack_time: u32,
    /// The base damage of the last hit taken (PMMP `lastDamageCause->getBaseDamage()`).
    last_damage: Option<f64>,
    /// The item the player started eating or drinking, and when.
    using: Option<food::Using>,
    /// Item cooldowns: the cooldown key and the tick it ends (PMMP `usedItemsCooldown`).
    cooldowns: BTreeMap<food::CooldownKey, u64>,
    /// Death drops that could not be dropped (entity limit) nor put back; dropped as soon as the
    /// world takes them: at most one inventory ([`MAX_HELD_BACK`]). Saved with the player.
    held_back: Vec<Stack>,
    /// Random draws made for this player, so no tick opens the same stream twice.
    draws: u64,
    /// What the client must be told at the end of the tick.
    dirty: u16,
    /// The entity data the viewers were last sent.
    shown: attributes::Shown,
}

/// A position in a world by name (worlds are saved by name).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Point {
    world: String,
    pos: BlockPos,
}

impl Default for Survival {
    fn default() -> Self {
        Self {
            game_mode: None,
            health: MAX_HEALTH,
            absorption: 0.0,
            food: MAX_FOOD,
            saturation: DEFAULT_SATURATION,
            exhaustion: 0.0,
            food_timer: 0,
            xp_level: 0,
            xp_progress: 0.0,
            xp_lifetime: 0,
            xp_cooldown: 0,
            xp_seed: None,
            effects: BTreeMap::new(),
            fire_ticks: 0,
            air: MAX_AIR,
            breathing: true,
            fall_distance: 0.0,
            spawn: None,
            death: None,
            sleeping: None,
            motion: movement::Motion::default(),
            no_damage_ticks: 0,
            attack_time: 0,
            last_damage: None,
            using: None,
            cooldowns: BTreeMap::new(),
            held_back: Vec::new(),
            draws: 0,
            dirty: 0,
            shown: attributes::Shown::default(),
        }
    }
}

impl Survival {
    /// The largest health: 20 and 4 for each level of health boost.
    fn max_health(&self) -> f64 {
        let boost = self
            .effects
            .get(&effect::Type::HealthBoost)
            .map_or(0.0, |a| f64::from(a.level()));
        MAX_HEALTH + 4.0 * boost
    }

    /// Whether the player is dead and waits to respawn.
    fn is_dead(&self) -> bool {
        self.health <= 0.0
    }

    /// The level of an active effect (amplifier + 1), 0 when it is not active.
    fn effect_level(&self, effect: effect::Type) -> u32 {
        self.effects.get(&effect).map_or(0, |a| a.level())
    }

    /// Marks attributes or other client state to send at the end of the tick.
    fn mark(&mut self, bits: u16) {
        self.dirty |= bits;
    }

    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            game_mode,
            health,
            absorption,
            food,
            saturation,
            exhaustion,
            food_timer,
            xp_level,
            xp_progress,
            xp_lifetime,
            xp_cooldown,
            xp_seed,
            effects,
            fire_ticks,
            air,
            breathing,
            fall_distance,
            spawn,
            death,
            sleeping,
            motion,
            no_damage_ticks,
            attack_time,
            last_damage,
            using,
            cooldowns,
            held_back,
            draws,
            dirty,
            shown,
        } = self;
        h.u64(game_mode.map_or(0, |m| u64::from(saved::game_mode_id(m).cast_unsigned()) + 1));
        for v in [
            health,
            absorption,
            food,
            saturation,
            exhaustion,
            xp_progress,
            fall_distance,
        ] {
            h.u64(v.to_bits());
        }
        h.u64(u64::from(*food_timer));
        h.u64(u64::from(*xp_level));
        h.u64(u64::from(*xp_lifetime));
        h.u64(u64::from(*xp_cooldown));
        h.u64(xp_seed.map_or(0, |s| u64::from(s.cast_unsigned()) + 1));
        h.usize(effects.len());
        for (effect, active) in effects {
            h.u64(u64::from(effect.id()));
            active.hash_into(h);
        }
        h.u64(u64::from(*fire_ticks));
        h.u64(u64::from(air.cast_unsigned()));
        h.u64(u64::from(*breathing));
        for point in [spawn, death] {
            match point {
                Some(p) => {
                    h.bytes(p.world.as_bytes());
                    hash_pos(h, p.pos);
                }
                None => h.u64(0),
            }
        }
        match sleeping {
            Some(p) => hash_pos(h, *p),
            None => h.u64(0),
        }
        motion.hash_into(h);
        h.u64(u64::from(*no_damage_ticks));
        h.u64(u64::from(*attack_time));
        h.u64(last_damage.map_or(0, f64::to_bits));
        match using {
            Some(u) => u.hash_into(h),
            None => h.u64(0),
        }
        h.usize(cooldowns.len());
        for (key, end) in cooldowns {
            key.hash_into(h);
            h.u64(*end);
        }
        h.usize(held_back.len());
        for stack in held_back {
            super::inventory::hash_stack(h, stack);
        }
        h.u64(*draws);
        h.u64(u64::from(*dirty));
        shown.hash_into(h);
    }
}

#[cfg(test)]
fn hash_pos(h: &mut super::Fnv, pos: BlockPos) {
    for c in [pos.x, pos.y, pos.z] {
        h.u64(u64::from(c.cast_unsigned()));
    }
}

/// Reads the player's survival keys from its saved extra (empty for a new player).
pub(super) fn load(survival: &mut Survival, extra: &Compound) {
    saved::load(survival, extra);
}

/// Writes the player's survival keys into the extra to save.
pub(super) fn save(survival: &Survival, extra: &mut Compound) {
    saved::save(survival, extra);
}

/// `PlayerAuthInput`: rotation, input flags (sneaking, sprinting, swimming, gliding, flying,
/// jumping, missed swings) and the movement, checked as PMMP does (PMMP
/// `InGamePacketHandler::handlePlayerAuthInput`, `Player::handleMovement`). Positions that are
/// not finite or outside the world border are ignored: tick state holds no NaN or infinity
/// (PLAN.md 2.1).
pub(super) fn on_auth_input(state: &mut State, session: SessionId, packet: &PlayerAuthInput) {
    movement::on_auth_input(state, session, packet);
}

/// `PlayerAction`: respawn, jumping, sprinting, sneaking, sleeping, gliding…
pub(super) fn on_player_action(state: &mut State, session: SessionId, packet: &PlayerAction) {
    movement::on_player_action(state, session, packet);
}

/// `Animate`: ignored, as PMMP does (`InGamePacketHandler::handleAnimate` filters it as noisy):
/// the arm swing other players see comes from the attacks and missed swings the server handles,
/// so a client cannot make every viewer receive a packet per packet it sends.
pub(super) fn on_animate(state: &mut State, session: SessionId, packet: &Animate) {
    let _ = (state, session, packet);
}

/// `Respawn` from the client: the respawn button on the death screen
/// (`RespawnStateClientReadyToSpawn`) is answered with the respawn position (PMMP
/// `DeathPacketHandler::handleRespawn`); the respawn itself follows `PlayerAction` respawn.
pub(super) fn on_respawn(state: &mut State, session: SessionId, packet: &Respawn) {
    damage::on_respawn(state, session, packet);
}

/// One tick of the world clocks, then of every spawned player's survival state (PMMP
/// `World::actuallyDoTick` then `Player::onUpdate`), then the client updates.
pub(super) fn tick(state: &mut State) {
    clock::tick(state);
    let mut next = first_session(state);
    while let Some(session) = next {
        player_tick(state, session);
        next = next_session(state, session);
    }
    attributes::flush(state);
}

/// One tick of a spawned, living player (PMMP `Player::onUpdate`, `Human::entityBaseTick`,
/// `Living::entityBaseTick`, `Entity::entityBaseTick`).
fn player_tick(state: &mut State, session: SessionId) {
    let tick = state.tick;
    let seed = world_seed(state, session);
    let settings = &state.settings;
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if player.phase != Phase::Spawned {
        return;
    }
    let hunger = settings.hunger && is_survival(game_mode_of(settings, player));
    player.survival.motion.hunger = hunger;
    if player.survival.xp_seed.is_none() {
        let mut random = player_random(seed, tick, player, SEED);
        let [a, b, c, d, ..] = random.next_u64().to_le_bytes();
        player.survival.xp_seed = Some(i32::from_le_bytes([a, b, c, d]));
        player.data_dirty = true;
    }
    if player.survival.is_dead() {
        if !player.survival.motion.death_shown {
            damage::show_death(state, session, damage::GENERIC_DEATH, None);
        }
        return;
    }
    if player.survival.sleeping.is_some() && !player.survival.motion.in_bed {
        clock::wake(state, session);
    }
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    movement::process(player);
    damage::retry_held_back(state, session);
    movement::block_intersections(state, session);
    damage::base_tick(state, session);
    effects::tick(state, session);
    if let Some(player) = state.players.get_mut(&session) {
        food::xp_tick(&mut player.survival);
    }
    damage::living_tick(state, session);
    food::tick(state, session, hunger);
    food::using_tick(state, session);
    if let Some(player) = state.players.get_mut(&session) {
        movement::end_tick(player);
    }
}

/// The random stream key of the enchanting seed.
const SEED: u64 = random::tag("player-seed");

/// A random stream for one draw purpose of a player in a world with `seed`; each call opens a
/// new stream (the player's draw counter is part of the key), so a tick never opens the same
/// stream twice.
fn player_random(seed: i64, tick: u64, player: &mut Player, purpose: u64) -> Random {
    let s = &mut player.survival;
    s.draws = s.draws.wrapping_add(1);
    Random::new(
        seed,
        tick,
        random::key(purpose, &[player.runtime_id, s.draws]),
    )
}

/// The seed of the world the player of `session` is in (0 if it is gone).
fn world_seed(state: &State, session: SessionId) -> i64 {
    state
        .players
        .get(&session)
        .and_then(|p| state.worlds.get(p.world))
        .map_or(0, crate::world::World::seed)
}

/// The first session in order, if any.
fn first_session(state: &State) -> Option<SessionId> {
    state.players.keys().next().copied()
}

/// The session after `session` in order, if any (the map may change between calls).
fn next_session(state: &State, session: SessionId) -> Option<SessionId> {
    state
        .players
        .range((Bound::Excluded(session), Bound::Unbounded))
        .next()
        .map(|(&s, _)| s)
}

/// The player's game mode: its own, or the server default (always the default with
/// `force-gamemode`).
pub(super) fn game_mode(state: &State, player: &Player) -> GameMode {
    game_mode_of(&state.settings, player)
}

/// [`game_mode`] from the settings alone.
fn game_mode_of(settings: &Settings, player: &Player) -> GameMode {
    if settings.force_game_mode {
        return settings.game_mode;
    }
    player.survival.game_mode.unwrap_or(settings.game_mode)
}

/// The player's own game mode, `None` when it uses the server default (`StartGame` then names
/// the world's mode, as BDS does for a new player).
pub(super) fn own_game_mode(settings: &Settings, player: &Player) -> Option<GameMode> {
    if settings.force_game_mode {
        return None;
    }
    player.survival.game_mode
}

/// The health the client shows (`SetHealth`): rounded up.
pub(super) fn health_shown(player: &Player) -> i32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "invariant: health is finite and at most a few hundred"
    )]
    let health = player.survival.health.ceil() as i32;
    health
}

/// The player confirmed its spawn: its entity data is what the spawn window showed, and it
/// learns its active effects (PMMP sends them once the player is in game).
pub(super) fn spawned(settings: &Settings, player: &mut Player) {
    attributes::spawned(settings, player);
    let packets: Vec<Packet> = effects::all_packets(player).collect();
    for packet in packets {
        player.send(packet);
    }
}

/// Whether the game mode loses items and takes damage (PMMP `Player::isSurvival`: survival or
/// adventure).
fn is_survival(mode: GameMode) -> bool {
    matches!(mode, GameMode::Survival | GameMode::Adventure)
}

/// Deals `amount` damage (half hearts) for `cause` after armour, effects and invulnerability;
/// false if no damage was taken.
pub(super) fn damage(state: &mut State, session: SessionId, amount: f64, cause: Damage) -> bool {
    damage::damage(state, session, amount, cause)
}

/// Adds food exhaustion (PMMP `HungerManager::exhaust`); nothing while hunger is off for the
/// player (`hunger = false`, creative, spectator).
pub(super) fn exhaust(player: &mut Player, amount: f64) {
    food::exhaust(&mut player.survival, amount);
}

/// Adds experience points (orbs, furnaces, trades, breaking ores).
pub(super) fn add_experience(player: &mut Player, amount: u32) {
    food::add_experience(player, amount);
}

/// The enchanting seed of the player (PMMP `Human::getEnchantmentSeed`), 0 until its first tick.
pub(super) fn enchantment_seed(player: &Player) -> i32 {
    player.survival.xp_seed.unwrap_or(0)
}

/// Draws a new enchanting seed on the player's next tick (PMMP `regenerateEnchantmentSeed`).
pub(super) fn regenerate_enchantment_seed(player: &mut Player) {
    player.survival.xp_seed = None;
    player.data_dirty = true;
}

/// The experience level (PMMP `ExperienceManager::getXpLevel`).
pub(super) fn xp_level(player: &Player) -> u32 {
    player.survival.xp_level
}

/// The experience points the player ever gained (PMMP `ExperienceManager::getLifetimeTotalXp`).
#[cfg(test)]
pub(super) fn lifetime_experience(player: &Player) -> u32 {
    player.survival.xp_lifetime
}

/// Takes `levels` experience levels, keeping the progress (PMMP `subtractXpLevels`); not below 0.
pub(super) fn subtract_levels(player: &mut Player, levels: u32) {
    if levels == 0 {
        return;
    }
    let s = &mut player.survival;
    s.xp_level = s.xp_level.saturating_sub(levels);
    s.mark(attributes::LEVEL);
    player.data_dirty = true;
}

/// `attacker` hits the player `target` in melee with its held item (PMMP `Player::attackEntity`).
pub(super) fn attack_player(state: &mut State, attacker: SessionId, target: SessionId) {
    damage::attack_player(state, attacker, target);
}

/// The player starts or finishes using its held item in the air (eating, drinking): PMMP
/// `Player::useHeldItem`/`consumeHeldItem`. True when the item is a consumable it handled.
pub(super) fn use_consumable(state: &mut State, session: SessionId) -> bool {
    food::use_consumable(state, session)
}

/// The player lies down in the bed at `pos` (PMMP `Bed::onInteract`, `Player::sleepOn`): false if
/// it cannot sleep now (day, not a whole bed, too far, occupied, dead).
pub(super) fn sleep(
    state: &mut State,
    session: SessionId,
    world: crate::save::WorldIndex,
    pos: crate::world::BlockPos,
) -> bool {
    clock::sleep(state, session, world, pos)
}

/// The player leaves: it wakes up (the bed is free again) and stops eating (PMMP
/// `Player::onPostDisconnect` → `stopSleep`).
pub(super) fn left(state: &mut State, player: &mut Player) {
    clock::stop_sleep(state, player);
    player.survival.using = None;
}

/// The player's feet position (the tick keeps the eye position).
fn feet(player: &Player) -> Vec3 {
    player.position - Vec3::new(0.0, EYE_HEIGHT, 0.0)
}

/// The block that holds a position.
fn block_at(v: Vec3) -> BlockPos {
    BlockPos::new(floor_i32(v.x), floor_i32(v.y), floor_i32(v.z))
}

/// `v` rounded down to an `i32`, saturating (tick positions are finite and inside the border).
fn floor_i32(v: f64) -> i32 {
    let f = v.floor();
    if f >= f64::from(i32::MAX) {
        i32::MAX
    } else if f <= f64::from(i32::MIN) {
        i32::MIN
    } else {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "invariant: the value is an integer inside the i32 range here"
        )]
        let i = f as i32;
        i
    }
}

/// `v` as the `f32` a packet carries.
fn f32_of(v: f64) -> f32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "consistency: packets carry f32, and rounding f64 to f32 is exact IEEE behaviour"
    )]
    let f = v as f32;
    f
}

/// Queues `packet` for the player of `session` and every player that sees it (its own view
/// includes its chunk).
fn broadcast_from(state: &mut State, session: SessionId, packet: &Packet) {
    let Some(player) = state.players.get(&session) else {
        return;
    };
    let (world, chunk) = (player.world, super::view::chunk_of(player.position));
    state.broadcast(world, chunk, packet, None);
}

/// Queues `packet` for the players that see the player of `session`, except that player.
fn broadcast_to_viewers(state: &mut State, session: SessionId, packet: &Packet) {
    let Some(player) = state.players.get(&session) else {
        return;
    };
    let (world, chunk) = (player.world, super::view::chunk_of(player.position));
    state.broadcast(world, chunk, packet, Some(session));
}

/// The difficulty the rules use.
fn difficulty(state: &State) -> Difficulty {
    state.settings.difficulty
}

/// Whether the player is alive (PMMP `Entity::isAlive`): not dead and waiting to respawn.
pub(super) fn is_alive(player: &Player) -> bool {
    !player.survival.is_dead()
}

/// Gives the player an effect (potions, beacons, food, clouds; PMMP `EffectManager::add`): false
/// if one of the same type with a higher amplifier, or the same amplifier and a longer duration,
/// stays. A replaced effect is undone first (health boost, absorption), the new one applies, and
/// the client hears `MobEffect` add or modify. An instant effect (healing, harming) acts on the
/// player's next tick, as PMMP's effect tick does.
pub(super) fn add_effect(state: &mut State, session: SessionId, effect: effect::Instance) -> bool {
    effects::add(state, session, effect.effect(), effects::Active::of(effect))
}

/// Moves the player's eyes to `pos` in `world` (PMMP `Player::teleport`; ender pearls, chorus
/// fruit, respawns, commands): its window closes, it wakes, its breaking stops, its fall ends,
/// the client gets
/// `MovePlayer` in teleport mode and the players that see it the same packet. Another world
/// closes the view of the old one, opens it around `pos` in the new one (its chunks are sent
/// again from the next view phase) and the client gets the new world's clock. Nothing happens for
/// a position that is not finite or lies outside the world border, a world that does not exist,
/// or a player whose data is still loading.
pub(super) fn teleport(state: &mut State, session: SessionId, world: WorldIndex, pos: Vec3) {
    let border = f64::from(super::MAX_COORDINATE);
    let inside = pos.is_finite() && [pos.x, pos.y, pos.z].iter().all(|c| c.abs() <= border);
    let Some(player) = state.players.get(&session) else {
        return;
    };
    if !inside || state.worlds.get(world).is_none() || player.phase == Phase::Loading {
        return;
    }
    let other_world = player.world != world;
    inventory::close_window(state, session);
    clock::wake(state, session);
    super::interact::stop_breaking(state, session);
    if other_world {
        switch_world(state, session, world, pos);
    }
    movement::teleport(state, session, pos);
    if let Some(packet) = state
        .players
        .get(&session)
        .map(|p| movement::position_packet(p, MoveMode::Teleport))
    {
        broadcast_to_viewers(state, session, &packet);
    }
}

/// Moves the player into another world: the old world's chunks are let go, the view opens at
/// `pos` in the new world with the same radius, and the client learns the new world's clock and
/// sleeping counts (PMMP `Player::setPosition`, `NetworkSession::onEnterWorld`). The other
/// players see it leave and arrive through entity tracking, which follows the world.
fn switch_world(state: &mut State, session: SessionId, world: WorldIndex, pos: Vec3) {
    let Some(mut player) = state.players.remove(&session) else {
        return;
    };
    let radius = player.view.radius();
    super::view::close(state, &mut player);
    player.world = world;
    player.position = pos;
    player.data_dirty = true;
    if radius > 0 {
        super::view::set_radius(state, &mut player, radius);
    }
    if player.phase == Phase::Spawned {
        player.send(clock_state(state, world));
        player.send(sleeping_players(state, world));
    }
    state.players.insert(session, player);
}

/// The level of the haste effect (0 without it), for the break speed (PMMP
/// `SurvivalBlockBreakHandler::calculateBreakProgressPerTick`).
pub(super) fn haste_level(player: &Player) -> u32 {
    player.survival.effect_level(effect::Type::Haste)
}

/// The level of the mining fatigue effect (0 without it), for the break speed.
pub(super) fn mining_fatigue_level(player: &Player) -> u32 {
    player.survival.effect_level(effect::Type::MiningFatigue)
}

/// Whether the player flies (creative or spectator flight, PMMP `Player::isFlying`).
pub(super) fn is_flying(player: &Player) -> bool {
    player.survival.motion.flying
}

/// Whether the player stands on something (PMMP `Entity::isOnGround`).
pub(super) fn is_on_ground(player: &Player) -> bool {
    player.survival.motion.on_ground
}

/// Whether the player sneaks (PMMP `Player::isSneaking`): a sneaking player places against
/// blocks it would otherwise use.
pub(super) fn is_sneaking(player: &Player) -> bool {
    player.survival.motion.sneaking
}

/// Whether the player holds the sneak key (PMMP `Player::isSneakPressed`): a click on a block
/// then uses the held item instead of the block.
pub(super) fn is_sneak_pressed(player: &Player) -> bool {
    player.survival.motion.sneak_pressed
}

/// The player's yaw and pitch in degrees, yaw in [0, 360) (PMMP `Location::getYaw`,
/// `getPitch`): the facing of placed blocks, the flight of thrown items and arrows.
pub(super) fn rotation(player: &Player) -> (f64, f64) {
    (player.survival.motion.yaw, player.survival.motion.pitch)
}

/// The unit vector the player looks along (PMMP `Entity::getDirectionVector`).
pub(super) fn look_direction(player: &Player) -> Vec3 {
    movement::direction(&player.survival)
}

/// Whether the player's eyes are under water (PMMP `Entity::isUnderwater`), for the break speed.
pub(super) fn is_underwater(state: &State, player: &Player) -> bool {
    movement::is_underwater(state, player)
}

/// Whether `item`'s cooldown still runs for the player (PMMP `Player::hasItemCooldown`): ender
/// pearls, chorus fruit, goat horns.
pub(super) fn has_item_cooldown(state: &State, player: &Player, item: crate::item::Type) -> bool {
    food::has_cooldown(state.tick, &player.survival, item)
}

/// Starts `item`'s cooldown after a use and tells the client (PMMP `Player::resetItemCooldown`);
/// nothing for an item without one.
pub(super) fn reset_item_cooldown(state: &mut State, session: SessionId, item: crate::item::Type) {
    let tick = state.tick;
    if let Some(player) = state.players.get_mut(&session) {
        food::reset_cooldown(tick, player, item);
    }
}

/// The player eats a slice of cake (PMMP `BaseCake::onInteract` → `Human::consumeObject`): false,
/// and nothing changes, if it cannot eat now (food full outside peaceful and creative, dead, a
/// spectator). Otherwise food +2, saturation +0.4 and the burp sound; the caller then takes a
/// bite off the block (PMMP `BaseCake::onConsume`).
pub(super) fn eat_cake(state: &mut State, session: SessionId) -> bool {
    food::eat_cake(state, session)
}

/// The player picks up an experience orb worth `amount` points (PMMP
/// `ExperienceManager::onPickupXp`): false, and the orb stays, while the pickup cooldown of 2
/// ticks runs. Mending on a damaged held, off-hand or worn item (one picked at random) repairs 2
/// durability per point first; the rest becomes experience, with the collect or level-up sound.
pub(super) fn pickup_experience(state: &mut State, session: SessionId, amount: u32) -> bool {
    food::pickup_experience(state, session, amount)
}

/// Heals the player by `amount` (PMMP `Entity::heal`: a splash potion of healing, weaker with
/// the distance); nothing for a dead player or one at full health.
pub(super) fn heal(state: &mut State, session: SessionId, amount: f64) {
    damage::heal(state, session, amount);
}

/// Sets the player on fire for `seconds` (PMMP `Living::setOnFire`: burning arrows, Fire
/// Aspect); a longer burn already running stays.
pub(super) fn set_on_fire(state: &mut State, session: SessionId, seconds: u32) {
    if let Some(player) = state.players.get_mut(&session) {
        damage::set_on_fire(player, seconds);
    }
}

/// Whether the player's hit cooldown runs (PMMP `Living::$attackTime` above 0): a hit now only
/// counts what it deals beyond the last one and does not knock the player back.
pub(super) fn has_attack_cooldown(player: &Player) -> bool {
    player.survival.attack_time > 0
}

/// A melee hit a player makes on an entity that is not a player, from the attacker's side (PMMP
/// `Player::attackEntity`, `EntityDamageByEntityEvent::addAttackerModifiers`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Melee {
    /// The damage: the held item's attack points with Strength, Weakness, Sharpness and a
    /// critical hit.
    pub(super) damage: f64,
    /// A critical hit (falling, not sprinting or flying, not blind, not under water).
    pub(super) critical: bool,
    /// A weapon enchantment added damage (PMMP `MagicHitAnimation`).
    pub(super) magic: bool,
}

/// The melee hit the player of `session` makes now; `None` if it cannot hit (not spawned, dead
/// or a spectator).
pub(super) fn melee(state: &State, session: SessionId) -> Option<Melee> {
    damage::melee(state, session)
}

/// The player's arm swing, for the players that see it (PMMP `ArmSwingAnimation`).
pub(super) fn swing(state: &mut State, session: SessionId) {
    damage::swing(state, session);
}

/// A melee hit's sound at `at` for the players that see `session` (PMMP `EntityAttackSound`,
/// `EntityAttackNoDamageSound`).
pub(super) fn hit_sound(state: &mut State, session: SessionId, at: Vec3, sound: SoundEvent) {
    sound::hit(state, session, at, sound);
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::effects::Active;
    use super::*;
    use crate::block::{self, PropertyValue};
    use crate::config::Config;
    use crate::event::Listeners;
    use crate::item::{Enchantment, Extra, Kind as ItemKind, Stack};
    use crate::protocol::consts::{
        ActorEvent, InputFlag, LevelEvent, MobEffectOperation, MoveMode, PlayerAction as Action,
        RespawnState,
    };
    use crate::protocol::packet::{self as p, Packet};
    use crate::protocol::{FlagSet, InputFlags, MetadataValue};
    use crate::tick::queue::{Input, Outbound};
    use crate::tick::tests::{
        initialised, input, join, leave, new_state, packet, radius, received, settings, tick,
    };
    use crate::tick::{Output, inventory};

    /// A server with players 1..=n spawned; session k has runtime ID k.
    struct Server {
        state: State,
        output: Output,
        receivers: Vec<mpsc::Receiver<Outbound>>,
        seq: u64,
    }

    impl Server {
        fn new(n: u64, edit: impl FnOnce(&mut Config)) -> Self {
            let mut server = Self {
                state: new_state(settings(edit), Listeners::default()),
                output: Output::default(),
                receivers: Vec::new(),
                seq: 10,
            };
            let mut first = Vec::new();
            for s in 1..=n {
                let (j, rx) = join(s, 0);
                server.receivers.push(rx);
                first.extend([j, radius(s, 1, 8)]);
            }
            server.run(first);
            server.run((1..=n).map(|s| initialised(s, 2, s)).collect());
            for s in 1..=n {
                server.take(s);
            }
            server
        }

        fn run(&mut self, mut inputs: Vec<Input>) {
            tick(&mut self.state, &mut inputs, &mut self.output);
        }

        fn idle(&mut self, ticks: u32) {
            for _ in 0..ticks {
                self.run(Vec::new());
            }
        }

        /// An input of session `s` carrying `packet`.
        fn send(&mut self, s: u64, packet_: Packet) -> Input {
            self.seq += 1;
            input(s, self.seq, packet(packet_))
        }

        /// Everything session `s` received since the last call.
        fn take(&mut self, s: u64) -> Vec<Packet> {
            let index = usize::try_from(s - 1).unwrap();
            received(&mut self.receivers[index]).0
        }

        fn player(&self, s: u64) -> &Player {
            &self.state.players[&SessionId::new(s)]
        }

        fn player_mut(&mut self, s: u64) -> &mut Player {
            self.state.players.get_mut(&SessionId::new(s)).unwrap()
        }

        fn survival(&mut self, s: u64) -> &mut Survival {
            &mut self.player_mut(s).survival
        }

        fn hurt(&mut self, s: u64, amount: f64, cause: Damage) -> bool {
            damage(&mut self.state, SessionId::new(s), amount, cause)
        }
    }

    fn sid(s: u64) -> SessionId {
        SessionId::new(s)
    }

    fn stack(kind: ItemKind, count: u8) -> Stack {
        Stack::new(kind.default_type(), count)
    }

    fn enchanted(kind: ItemKind, enchantments: &[(Enchantment, u8)]) -> Stack {
        stack(kind, 1).with_extra(Some(Extra {
            enchantments: enchantments.to_vec(),
            ..Extra::default()
        }))
    }

    /// The encoded payload size of a packet (without the packet header).
    fn size(packet: &Packet) -> usize {
        let mut bytes = Vec::new();
        packet.encode(&mut bytes, 0).unwrap();
        bytes.len()
    }

    fn id(packet: &Packet) -> u32 {
        packet.id()
    }

    #[test]
    fn the_spawn_window_has_the_bds_order() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 8)], &mut output);
        let chunks = [
            id(&Packet::LevelChunk(p::LevelChunk::default())),
            id(&Packet::SubChunk(p::SubChunk::default())),
            id(&Packet::NetworkChunkPublisherUpdate(
                p::NetworkChunkPublisherUpdate::default(),
            )),
        ];
        let sent: Vec<u32> = received(&mut rx)
            .2
            .into_iter()
            .filter(|i| !chunks.contains(i))
            .collect();
        // m7-facts.md 7 without the deferred packets (SyncActorProperty, JigsawStructureData,
        // SetCommandsEnabled, the camera packets, AvailableCommands, LevelSoundEvent,
        // CurrentStructureFeature).
        let want = [
            id(&Packet::LevelEventGeneric(p::LevelEventGeneric::default())),
            id(&Packet::PlayerList(p::PlayerList::default())),
            id(&Packet::SyncWorldClocks(p::SyncWorldClocks::default())),
            id(&Packet::VoxelShapes(p::VoxelShapes::default())),
            id(&Packet::StartGame(Box::default())),
            id(&Packet::ItemRegistry(p::ItemRegistry::default())),
            id(&Packet::SetSpawnPosition(p::SetSpawnPosition::default())),
            id(&Packet::SyncWorldClocks(p::SyncWorldClocks::default())),
            id(&Packet::SetDifficulty(p::SetDifficulty::default())),
            id(&Packet::UpdateAdventureSettings(
                p::UpdateAdventureSettings::default(),
            )),
            id(&Packet::UpdateAbilities(p::UpdateAbilities::default())),
            id(&Packet::GameRulesChanged(p::GameRulesChanged::default())),
            id(&Packet::PlayerList(p::PlayerList::default())),
            id(&Packet::UpdateAbilities(p::UpdateAbilities::default())),
            id(&Packet::BiomeDefinitionList(
                p::BiomeDefinitionList::default(),
            )),
            id(&Packet::AvailableActorIdentifiers(
                p::AvailableActorIdentifiers::default(),
            )),
            id(&Packet::PlayerFog(p::PlayerFog::default())),
            id(&Packet::UpdateAttributes(p::UpdateAttributes::default())),
            id(&Packet::CreativeContent(p::CreativeContent::default())),
            id(&Packet::TrimData(p::TrimData::default())),
            id(&Packet::InventoryContent(Box::default())),
            id(&Packet::InventoryContent(Box::default())),
            id(&Packet::InventoryContent(Box::default())),
            id(&Packet::InventoryContent(Box::default())),
            id(&Packet::PlayerHotBar(p::PlayerHotBar::default())),
            id(&Packet::CraftingData(Box::default())),
            id(&Packet::Respawn(p::Respawn::default())),
            id(&Packet::Respawn(p::Respawn::default())),
            id(&Packet::Respawn(p::Respawn::default())),
            id(&Packet::SetActorData(p::SetActorData::default())),
            id(&Packet::UpdateAttributes(p::UpdateAttributes::default())),
            id(&Packet::ChunkRadiusUpdated(p::ChunkRadiusUpdated::default())),
            id(&Packet::SetHealth(p::SetHealth::default())),
            id(&Packet::PlayStatus(p::PlayStatus::default())),
        ];
        assert_eq!(sent, want);
    }

    /// The packets a new player gets in its first tick (join and chunk radius request).
    fn window() -> Vec<Packet> {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        let (joined, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![joined, radius(1, 1, 8)], &mut output);
        received(&mut rx).0
    }

    /// The packets of `window` that `pick` keeps.
    fn picked<T>(window: &[Packet], pick: impl Fn(&Packet) -> Option<&T>) -> Vec<&T> {
        window.iter().filter_map(pick).collect()
    }

    #[test]
    fn the_spawn_window_has_the_bds_sleeping_list_and_clocks() {
        let packets = window();
        let sleeping = picked(&packets, |p| match p {
            Packet::LevelEventGeneric(x) => Some(x),
            _ => None,
        });
        assert_eq!(sleeping[0].event_id, 9801);
        assert_eq!(size(&Packet::LevelEventGeneric(sleeping[0].clone())), 63);
        let lists = picked(&packets, |p| match p {
            Packet::PlayerList(x) => Some(x),
            _ => None,
        });
        assert_eq!(lists.len(), 2);
        assert_eq!(lists[0], lists[1]);
        let entry = &lists[0].entries[0];
        assert_eq!((entry.action_type, entry.username.as_str()), (0, "player1"));
        assert_eq!(
            (
                entry.entity_unique_id,
                entry.player_colour.r,
                entry.player_colour.a
            ),
            (1, 237, 255)
        );
        let clocks = picked(&packets, |p| match p {
            Packet::SyncWorldClocks(x) => Some(x),
            _ => None,
        });
        assert_eq!((clocks[0].payload_type, clocks[1].payload_type), (0, 1));
        assert_eq!(clocks[0].sync_states[0].clock_id, 7_194_480_507_151_251_734);
        let markers: Vec<_> = clocks[1].clocks[0]
            .time_markers
            .iter()
            .map(|m| (m.name.as_str(), m.time, m.period))
            .collect();
        assert_eq!(markers.len(), 6);
        assert!(markers.contains(&("minecraft:night", 13_000, Some(24_000))));
    }

    #[test]
    fn the_spawn_window_has_the_bds_settings_abilities_and_rules() {
        let packets = window();
        assert!(packets.contains(&Packet::SetDifficulty(p::SetDifficulty { difficulty: 2 })));
        assert!(packets.contains(&adventure_settings()));
        assert!(packets.contains(&Packet::PlayerFog(p::PlayerFog { stack: Vec::new() })));
        let abilities = picked(&packets, |p| match p {
            Packet::UpdateAbilities(x) => Some(&x.ability_data),
            _ => None,
        });
        assert_eq!(abilities.len(), 2);
        let data = abilities[0];
        assert_eq!((data.player_permissions, data.command_permissions), (1, 0));
        assert_eq!(
            (data.layers[0].abilities, data.layers[0].values),
            (1_048_575, 63)
        );
        let rules = picked(&packets, |p| match p {
            Packet::GameRulesChanged(x) => Some(&x.game_rules),
            _ => None,
        });
        let rules = rules[0];
        assert_eq!(rules.len(), 39);
        assert_eq!(rules[0].name, "commandBlockOutput");
        assert_eq!(rules[38].name, "tntExplosionDropDecay");
        assert!(rules.iter().all(|r| r.can_be_modified_by_player));
    }

    #[test]
    fn the_spawn_window_has_the_bds_player_state() {
        let packets = window();
        let attributes = picked(&packets, |p| match p {
            Packet::UpdateAttributes(x) => Some(&x.attributes),
            _ => None,
        });
        assert_eq!(attributes.len(), 2);
        assert_eq!(attributes[0].len(), 17);
        let value = |name: &str| {
            attributes[0]
                .iter()
                .find(|a| a.attribute_value.name == name)
                .map(|a| a.attribute_value.value)
        };
        assert_eq!(value("minecraft:player.hunger"), Some(20.0));
        assert_eq!(value("minecraft:player.saturation"), Some(5.0));
        assert_eq!(value("minecraft:health"), Some(20.0));
        assert_eq!(value("minecraft:movement"), Some(0.1));
        let respawns: Vec<_> = picked(&packets, |p| match p {
            Packet::Respawn(x) => Some(x),
            _ => None,
        })
        .into_iter()
        .map(|r| (r.position, r.state, r.entity_runtime_id))
        .collect();
        assert_eq!(
            respawns,
            [
                ([0.5, 32_769.62, 0.5], 0, 0),
                ([0.5, 32_768.0, 0.5], 0, 0),
                ([0.5, -58.38, 0.5], 1, 0),
            ]
        );
        let data = picked(&packets, |p| match p {
            Packet::SetActorData(x) => Some(x),
            _ => None,
        });
        let metadata = &data[0].entity_metadata;
        assert_eq!(metadata.len(), 72);
        assert_eq!(metadata[&0], MetadataValue::Int64(844_424_930_656_256));
        assert_eq!(metadata[&92], MetadataValue::Int64(35_184_405_643_264));
        assert_eq!(metadata[&7], MetadataValue::Int16(300));
        assert_eq!(metadata[&130], MetadataValue::Vec3([0.6, 1.8, 0.0]));
        assert_eq!(metadata[&4], MetadataValue::String("player1".to_owned()));
        // BDS's SetActorData for "AnchorBot" is 395 payload bytes (P6); this name is 2 shorter.
        assert_eq!(size(&Packet::SetActorData(data[0].clone())), 395 - 2);
        assert!(packets.contains(&Packet::SetHealth(p::SetHealth { health: 20 })));
    }

    #[test]
    fn spawn_window_packets_have_the_bds_sizes() {
        // Payload sizes of BDS 1.26.52's packets (P6 capture sizes minus the packet header).
        let server = Server::new(1, |_| {});
        let settings = &server.state.settings;
        assert_eq!(size(&game_rules(settings)), 749);
        assert_eq!(size(&all_attributes(server.player(1))), 840);
        assert_eq!(size(&adventure_settings()), 5);
        assert_eq!(size(&sleeping_players(&server.state, 0)), 63);
        let mut state = server.state;
        state.worlds[0].set_time(26);
        assert_eq!(size(&clock_state(&state, 0)), 13);
        assert_eq!(size(&clock_definition(&state, 0)), 236);
    }

    #[test]
    fn players_see_each_other_join_and_leave_in_the_list() {
        let mut server = Server::new(2, |_| {});
        let (j, mut rx) = join(3, 0);
        server.run(vec![j, radius(3, 1, 8)]);
        let lists = |packets: &[Packet]| {
            packets
                .iter()
                .filter_map(|p| match p {
                    Packet::PlayerList(l) => Some(
                        l.entries
                            .iter()
                            .map(|e| (e.action_type, e.username.clone()))
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let all: Vec<_> = (1..=3).map(|n| (0, format!("player{n}"))).collect();
        assert_eq!(lists(&received(&mut rx).0), [all.clone(), all]);
        let added = vec![vec![(0, "player3".to_owned())]];
        assert_eq!(lists(&server.take(1)), added);
        assert_eq!(lists(&server.take(2)), added);
        server.run(vec![leave(3, 5)]);
        assert_eq!(lists(&server.take(2)), [vec![(1, String::new())]]);
    }

    #[test]
    fn armour_resistance_and_protection_reduce_damage_as_pmmp() {
        let mut server = Server::new(1, |_| {});
        let chest = stack(ItemKind::DiamondChestplate, 1);
        let points = f64::from(chest.item().unwrap().defense_points());
        inventory::set_armor(server.player_mut(1), 1, chest);
        assert!(server.hurt(1, 10.0, Damage::Contact));
        let want = 20.0 - 10.0 * (1.0 - points * 0.04);
        assert!((server.survival(1).health - want).abs() < 1e-9);
        // Not reducible by armour: the whole hit lands.
        server.idle(10);
        server.survival(1).health = 20.0;
        assert!(server.hurt(1, 4.0, Damage::Fall));
        assert_eq!(server.survival(1).health, 16.0);
        // Resistance II takes 40 % of what armour left.
        server.idle(10);
        server.survival(1).health = 20.0;
        effects::add(
            &mut server.state,
            sid(1),
            effect::Type::Resistance,
            Active::new(1000, 1),
        );
        assert!(server.hurt(1, 10.0, Damage::Contact));
        let want = 20.0 - 10.0 * (1.0 - points * 0.04) * 0.6;
        assert!((server.survival(1).health - want).abs() < 1e-9);
        // Protection IV on the chestplate: factor 5, scaled by 50 to 100 % and rounded up.
        effects::remove(&mut server.state, sid(1), effect::Type::Resistance);
        let protected = enchanted(ItemKind::DiamondChestplate, &[(Enchantment::Protection, 4)]);
        inventory::set_armor(server.player_mut(1), 1, protected);
        server.idle(10);
        server.survival(1).health = 20.0;
        assert!(server.hurt(1, 10.0, Damage::Magic));
        let taken = 20.0 - server.survival(1).health;
        assert!((10.0 * 0.8..=10.0 * 0.88).contains(&taken), "{taken}");
    }

    #[test]
    fn protection_factors_follow_pmmp() {
        assert_eq!(damage::epf(4, 0.75), 5);
        assert_eq!(damage::epf(1, 0.75), 1);
        assert_eq!(damage::epf(4, 1.25), 9);
        assert_eq!(damage::epf(4, 2.5), 18);
        assert_eq!(damage::epf(4, 1.5), 11);
        assert_eq!(damage::epf(255, 2.5), 54_192);
    }

    #[test]
    fn fall_damage_follows_pmmp() {
        let mut s = Survival::default();
        assert_eq!(damage::fall_damage(&s, 3.0), 0.0);
        assert_eq!(damage::fall_damage(&s, 3.5), 1.0);
        assert_eq!(damage::fall_damage(&s, 5.0), 2.0);
        s.effects
            .insert(effect::Type::JumpBoost, Active::new(10, 0));
        assert_eq!(damage::fall_damage(&s, 5.0), 1.0);
        s.motion.flying = true;
        assert_eq!(damage::fall_damage(&s, 50.0), 0.0);
    }

    #[test]
    fn a_weaker_hit_within_the_cooldown_does_nothing_and_a_stronger_one_deals_the_difference() {
        let mut server = Server::new(1, |_| {});
        assert!(server.hurt(1, 4.0, Damage::Contact));
        assert!(!server.hurt(1, 4.0, Damage::Contact));
        assert!(!server.hurt(1, 3.0, Damage::Contact));
        assert!(server.hurt(1, 6.0, Damage::Contact));
        assert_eq!(server.survival(1).health, 14.0);
        server.idle(10);
        assert!(server.hurt(1, 1.0, Damage::Contact));
        assert_eq!(server.survival(1).health, 13.0);
    }

    #[test]
    fn creative_players_respawned_players_and_fire_resistance_take_no_damage() {
        let mut server = Server::new(1, |c| c.general.gamemode = GameMode::Creative);
        assert!(!server.hurt(1, 5.0, Damage::Magic));
        let mut server = Server::new(1, |_| {});
        server.survival(1).no_damage_ticks = 5;
        assert!(!server.hurt(1, 5.0, Damage::Magic));
        server.survival(1).no_damage_ticks = 0;
        effects::add(
            &mut server.state,
            sid(1),
            effect::Type::FireResistance,
            Active::new(100, 0),
        );
        assert!(!server.hurt(1, 5.0, Damage::Lava));
        assert!(server.hurt(1, 5.0, Damage::Magic));
        assert!(!damage(&mut server.state, sid(1), f64::NAN, Damage::Magic));
        assert!(!damage(&mut server.state, sid(1), -1.0, Damage::Magic));
    }

    /// Puts player 2 one block south of player 1, facing it, and returns the server.
    fn duel(edit: impl FnOnce(&mut Config)) -> Server {
        let mut server = Server::new(2, edit);
        let base = server.player(1).position;
        server.player_mut(2).position = base + Vec3::new(0.0, 0.0, 1.0);
        server.survival(1).motion.yaw = 0.0;
        server.idle(1);
        server.take(1);
        server.take(2);
        server
    }

    #[test]
    fn melee_hits_deal_the_weapon_damage_with_criticals_and_knock_back() {
        let mut server = duel(|_| {});
        let sword = stack(ItemKind::DiamondSword, 1);
        let points = f64::from(sword.item().unwrap().attack_points());
        inventory::set_held(server.player_mut(1), sword);
        attack_player(&mut server.state, sid(1), sid(2));
        assert_eq!(server.survival(2).health, 20.0 - points);
        server.idle(1);
        let target = server.take(2);
        assert!(
            target
                .iter()
                .any(|p| matches!(p, Packet::SetActorMotion(m) if m.velocity[2] > 0.0))
        );
        assert!(target.iter().any(|p| matches!(
            p,
            Packet::ActorEvent(e) if e.event_type == u8::from(ActorEvent::Hurt)
        )));
        // A falling attacker lands a critical hit: half again.
        server.idle(10);
        server.survival(2).health = 20.0;
        server.survival(1).fall_distance = 1.0;
        attack_player(&mut server.state, sid(1), sid(2));
        assert_eq!(server.survival(2).health, 20.0 - points * 1.5);
        // Sharpness I adds 1.
        server.idle(10);
        server.survival(2).health = 20.0;
        server.survival(1).fall_distance = 0.0;
        let sharp = enchanted(ItemKind::DiamondSword, &[(Enchantment::Sharpness, 1)]);
        inventory::set_held(server.player_mut(1), sharp);
        attack_player(&mut server.state, sid(1), sid(2));
        assert_eq!(server.survival(2).health, 20.0 - points - 1.0);
    }

    #[test]
    fn attacks_need_pvp_reach_and_a_living_target() {
        let mut server = duel(|c| c.general.pvp = false);
        attack_player(&mut server.state, sid(1), sid(2));
        assert_eq!(server.survival(2).health, 20.0);
        let mut server = duel(|_| {});
        let far = server.player(1).position + Vec3::new(0.0, 0.0, 9.0);
        server.player_mut(2).position = far;
        attack_player(&mut server.state, sid(1), sid(2));
        assert_eq!(server.survival(2).health, 20.0);
        let behind = server.player(1).position + Vec3::new(0.0, 0.0, -2.0);
        server.player_mut(2).position = behind;
        attack_player(&mut server.state, sid(1), sid(2));
        assert_eq!(server.survival(2).health, 20.0);
        attack_player(&mut server.state, sid(1), sid(1));
        assert_eq!(server.survival(1).health, 20.0);
    }

    #[test]
    fn a_full_bar_heals_every_80_ticks_for_6_exhaustion() {
        let mut server = Server::new(1, |_| {});
        server.survival(1).health = 10.0;
        let timer = server.survival(1).food_timer;
        server.idle(80 - timer % 80);
        let s = server.survival(1);
        assert_eq!(s.health, 11.0);
        assert_eq!((s.saturation, s.exhaustion), (4.0, 2.0));
    }

    #[test]
    fn an_empty_bar_starves_down_to_the_difficulty_floor() {
        let mut server = Server::new(1, |_| {});
        server.survival(1).food = 0.0;
        server.survival(1).saturation = 0.0;
        server.survival(1).food_timer = 0;
        server.idle(80);
        assert_eq!(server.survival(1).health, 19.0);
        server.survival(1).health = 1.0;
        server.idle(80);
        assert_eq!(server.survival(1).health, 1.0, "normal stops at 1");
        let mut easy = Server::new(1, |c| c.general.difficulty = Difficulty::Easy);
        easy.survival(1).food = 0.0;
        easy.survival(1).saturation = 0.0;
        easy.survival(1).health = 10.0;
        easy.survival(1).food_timer = 0;
        easy.idle(160);
        assert_eq!(easy.survival(1).health, 10.0, "easy stops at 10");
    }

    #[test]
    fn peaceful_refills_food_and_health() {
        let mut server = Server::new(1, |c| c.general.difficulty = Difficulty::Peaceful);
        server.survival(1).food = 10.0;
        server.survival(1).health = 10.0;
        server.survival(1).food_timer = 0;
        server.idle(20);
        let s = server.survival(1);
        assert_eq!(s.food, 12.0);
        assert_eq!(s.health, 11.0);
    }

    #[test]
    fn hunger_off_freezes_food_and_exhaustion() {
        let mut server = Server::new(1, |c| c.general.hunger = false);
        server.survival(1).food = 10.0;
        server.idle(200);
        exhaust(server.player_mut(1), 40.0);
        let s = server.survival(1);
        assert_eq!((s.food, s.exhaustion, s.saturation), (10.0, 0.0, 5.0));
    }

    #[test]
    fn sprinting_and_jumping_exhaust_as_pmmp() {
        let mut server = Server::new(1, |_| {});
        let start = server.player(1).position;
        let flags: FlagSet<InputFlag> = [InputFlag::StartSprinting, InputFlag::StartJumping]
            .into_iter()
            .collect();
        let auth = p::PlayerAuthInput {
            position: (start + Vec3::new(2.0, 0.0, 0.0)).to_f32(),
            input_data: InputFlags::from_set(&flags),
            ..Default::default()
        };
        let input = server.send(1, Packet::PlayerAuthInput(Box::new(auth)));
        server.run(vec![input]);
        let s = server.survival(1);
        assert!(s.motion.sprinting);
        // 0.2 for the sprint jump, 0.01 per block sprinted (2 blocks).
        assert!((s.exhaustion - 0.22).abs() < 1e-6, "{}", s.exhaustion);
    }

    #[test]
    fn far_moves_are_sent_back_and_falls_hurt_on_landing() {
        let mut server = Server::new(1, |_| {});
        let start = server.player(1).position;
        let far = p::PlayerAuthInput {
            position: (start + Vec3::new(20.0, 0.0, 0.0)).to_f32(),
            ..Default::default()
        };
        let input = server.send(1, Packet::PlayerAuthInput(Box::new(far)));
        server.run(vec![input]);
        assert_eq!(server.player(1).position, start);
        let reset = server
            .take(1)
            .into_iter()
            .any(|p| matches!(p, Packet::MovePlayer(m) if m.mode == u8::from(MoveMode::Reset)));
        assert!(reset);
        // A move back near the position ends the reset; then a fall of 5 blocks lands.
        let near = p::PlayerAuthInput {
            position: (start + Vec3::new(0.0, 5.0, 0.0)).to_f32(),
            ..Default::default()
        };
        let input = server.send(1, Packet::PlayerAuthInput(Box::new(near)));
        server.run(vec![input]);
        assert_eq!(server.player(1).position, start);
        server.survival(1).motion.force_sync = false;
        let up = p::PlayerAuthInput {
            position: (start + Vec3::new(0.0, 5.0, 0.0)).to_f32(),
            tick: 1,
            ..Default::default()
        };
        let input = server.send(1, Packet::PlayerAuthInput(Box::new(up)));
        server.run(vec![input]);
        let down = p::PlayerAuthInput {
            position: start.to_f32(),
            tick: 2,
            ..Default::default()
        };
        let input = server.send(1, Packet::PlayerAuthInput(Box::new(down)));
        server.run(vec![input]);
        assert_eq!(server.survival(1).health, 18.0);
        assert_eq!(server.survival(1).fall_distance, 0.0);
    }

    #[test]
    fn effects_tick_at_pmmp_intervals_and_end() {
        let mut server = Server::new(1, |_| {});
        server.survival(1).food = 17.0;
        effects::add(
            &mut server.state,
            sid(1),
            effect::Type::Poison,
            Active::new(100, 0),
        );
        server.idle(100);
        assert_eq!(server.survival(1).health, 16.0, "poison every 25 ticks");
        assert!(server.survival(1).effects.is_empty());
        let sent = server.take(1);
        assert!(sent.iter().any(|p| matches!(
            p,
            Packet::MobEffect(m) if m.operation == u8::from(MobEffectOperation::Add) && m.duration == 100
        )));
        assert!(sent.iter().any(|p| matches!(
            p,
            Packet::MobEffect(m) if m.operation == u8::from(MobEffectOperation::Remove)
        )));
        server.survival(1).health = 10.0;
        effects::add(
            &mut server.state,
            sid(1),
            effect::Type::Regeneration,
            Active::new(100, 0),
        );
        server.idle(100);
        assert_eq!(
            server.survival(1).health,
            12.0,
            "regeneration every 50 ticks"
        );
        effects::add(
            &mut server.state,
            sid(1),
            effect::Type::InstantHealth,
            Active::new(1, 1),
        );
        server.idle(1);
        assert_eq!(server.survival(1).health, 20.0, "instant health II heals 8");
        effects::add(
            &mut server.state,
            sid(1),
            effect::Type::HealthBoost,
            Active::new(20, 0),
        );
        assert_eq!(server.survival(1).max_health(), 24.0);
        effects::add(
            &mut server.state,
            sid(1),
            effect::Type::Absorption,
            Active::new(20, 1),
        );
        assert_eq!(server.survival(1).absorption, 8.0);
        server.idle(20);
        assert_eq!(server.survival(1).max_health(), 20.0);
        assert_eq!(server.survival(1).absorption, 0.0);
    }

    #[test]
    fn experience_levels_follow_the_pmmp_formula() {
        let mut server = Server::new(1, |_| {});
        add_experience(server.player_mut(1), 7);
        assert_eq!(server.survival(1).xp_level, 1);
        add_experience(server.player_mut(1), 345);
        assert_eq!(server.survival(1).xp_level, 16);
        assert_eq!(server.survival(1).xp_lifetime, 352);
        server.idle(1);
        assert!(
            server
                .take(1)
                .iter()
                .any(|p| matches!(p, Packet::UpdateAttributes(a)
            if a.attributes.iter().any(|x| x.attribute_value.name == "minecraft:player.level"
                && x.attribute_value.value == 16.0)))
        );
    }

    #[test]
    fn eating_takes_the_eating_time_and_restores_food() {
        let mut server = Server::new(1, |_| {});
        server.survival(1).food = 10.0;
        server.survival(1).saturation = 0.0;
        inventory::set_held(server.player_mut(1), stack(ItemKind::Bread, 3));
        assert!(use_consumable(&mut server.state, sid(1)));
        assert!(server.survival(1).using.is_some());
        server.idle(5);
        assert!(use_consumable(&mut server.state, sid(1)));
        assert_eq!(server.survival(1).food, 10.0, "too early: nothing eaten");
        assert!(use_consumable(&mut server.state, sid(1)));
        server.idle(32);
        assert!(use_consumable(&mut server.state, sid(1)));
        let bread = ItemKind::Bread.default_type().food().unwrap();
        assert_eq!(
            server.survival(1).food,
            10.0 + f64::from(bread.food_restore())
        );
        assert_eq!(server.survival(1).saturation, bread.saturation_restore());
        assert_eq!(server.player(1).inventory.held().count(), 2);
        inventory::set_held(server.player_mut(1), stack(ItemKind::DiamondSword, 1));
        assert!(!use_consumable(&mut server.state, sid(1)));
    }

    #[test]
    fn death_drops_neither_lose_nor_duplicate_items() {
        let stacks: Vec<Stack> = (0..41)
            .map(|i| {
                if i % 7 == 0 {
                    enchanted(ItemKind::DiamondSword, &[(Enchantment::Vanishing, 1)])
                } else {
                    stack(ItemKind::Bread, u8::try_from(i % 5 + 1).unwrap())
                }
            })
            .collect();
        let vanishing = stacks
            .iter()
            .filter(|s| damage::level_of(s, Enchantment::Vanishing) > 0)
            .count();
        let total = |v: &[Stack]| v.iter().map(|s| u32::from(s.count())).sum::<u32>();
        let want = total(&stacks) - u32::try_from(vanishing).unwrap();
        // The world takes every third stack; the inventory takes one item of each refused stack.
        let mut dropped = Vec::new();
        let mut n = 0;
        let refused = damage::drop_all(stacks, |s| {
            n += 1;
            if n % 3 == 0 {
                dropped.push(s);
                None
            } else {
                Some(s)
            }
        });
        let mut given = Vec::new();
        let left = damage::give_back(refused, |s| {
            given.push(s.with_count(1));
            s.with_count(s.count() - 1)
        });
        assert!(left.len() <= 41);
        assert_eq!(total(&dropped) + total(&given) + total(&left), want);
        // Nothing fits anywhere (entity limit, full inventory): everything waits.
        let full: Vec<Stack> = (0..41).map(|_| stack(ItemKind::Bread, 64)).collect();
        let refused = damage::drop_all(full.clone(), Some);
        let left = damage::give_back(refused, |s| s);
        assert_eq!(left, full);
    }

    #[test]
    fn death_and_respawn_follow_the_pmmp_sequence() {
        let mut server = Server::new(1, |_| {});
        add_experience(server.player_mut(1), 100);
        server.idle(1);
        server.take(1);
        assert!(server.hurt(1, 100.0, Damage::Magic));
        assert!(server.survival(1).is_dead());
        assert_eq!(server.survival(1).xp_level, 0);
        server.idle(1);
        let death = server.take(1);
        let searching = death.iter().any(|p| {
            matches!(
                p,
                Packet::Respawn(r) if r.state == u8::from(RespawnState::SearchingForSpawn)
            )
        });
        assert!(searching);
        assert!(
            death
                .iter()
                .any(|p| matches!(p, Packet::DeathInfo(d) if d.cause == "death.attack.magic"))
        );
        // The dead ignore damage, movement and healing.
        assert!(!server.hurt(1, 1.0, Damage::Magic));
        let ready = p::Respawn {
            position: [0.0; 3],
            state: u8::from(RespawnState::ClientReadyToSpawn),
            entity_runtime_id: 1,
        };
        let input = server.send(1, Packet::Respawn(ready));
        server.run(vec![input]);
        let answer = server.take(1);
        assert!(answer.iter().any(|p| matches!(
            p,
            Packet::Respawn(r) if r.state == u8::from(RespawnState::ReadyToSpawn) && r.entity_runtime_id == 1
        )));
        let action = p::PlayerAction {
            entity_runtime_id: 1,
            action_type: i32::from(Action::Respawn),
            ..Default::default()
        };
        let input = server.send(1, Packet::PlayerAction(action));
        server.run(vec![input]);
        let s = server.survival(1);
        assert_eq!(
            (s.health, s.food, s.saturation, s.no_damage_ticks > 0),
            (20.0, 20.0, 5.0, true)
        );
        assert!(s.death.is_some());
        let back = server.take(1);
        let kinds: Vec<u32> = back.iter().map(Packet::id).collect();
        for want in [
            id(&Packet::MovePlayer(p::MovePlayer::default())),
            id(&Packet::UpdateAttributes(p::UpdateAttributes::default())),
            id(&Packet::UpdateAbilities(p::UpdateAbilities::default())),
            id(&Packet::SetActorData(p::SetActorData::default())),
        ] {
            assert!(kinds.contains(&want), "{want} in {kinds:?}");
        }
        assert!(back.iter().any(|p| matches!(
            p,
            Packet::ActorEvent(e) if e.event_type == u8::from(ActorEvent::SpawnAlive)
        )));
    }

    #[test]
    fn hardcore_players_come_back_as_spectators() {
        let mut server = Server::new(1, |c| {
            c.general.hardcore = true;
            c.general.difficulty = Difficulty::Hard;
        });
        server.hurt(1, 100.0, Damage::Magic);
        damage::respawn(&mut server.state, sid(1));
        assert_eq!(
            game_mode(&server.state, server.player(1)),
            GameMode::Spectator
        );
        assert!(!server.hurt(1, 5.0, Damage::Magic));
    }

    #[test]
    fn repeated_respawn_requests_change_nothing_twice() {
        let run = |actions: u32, readies: u32| {
            let mut server = Server::new(1, |_| {});
            server.hurt(1, 100.0, Damage::Magic);
            let mut inputs = Vec::new();
            for _ in 0..readies {
                let ready = p::Respawn {
                    position: [0.0; 3],
                    state: u8::from(RespawnState::ClientReadyToSpawn),
                    entity_runtime_id: 1,
                };
                inputs.push(server.send(1, Packet::Respawn(ready)));
            }
            for _ in 0..actions {
                let action = p::PlayerAction {
                    entity_runtime_id: 1,
                    action_type: i32::from(Action::Respawn),
                    ..Default::default()
                };
                inputs.push(server.send(1, Packet::PlayerAction(action)));
            }
            server.run(inputs);
            server.idle(3);
            server.state.hash()
        };
        assert_eq!(run(1, 1), run(2, 1));
        assert_eq!(run(1, 1), run(1, 3));
        let sprint = |times: u32| {
            let mut server = Server::new(1, |_| {});
            let inputs = (0..times)
                .map(|_| {
                    let action = p::PlayerAction {
                        entity_runtime_id: 1,
                        action_type: i32::from(Action::StartSprint),
                        ..Default::default()
                    };
                    server.send(1, Packet::PlayerAction(action))
                })
                .collect();
            server.run(inputs);
            server.state.hash()
        };
        assert_eq!(sprint(1), sprint(2));
    }

    /// The bed state facing south (direction 0), head or foot, occupied or not.
    fn bed(head: bool, occupied: bool) -> block::State {
        block::State::find(
            "minecraft:bed",
            &[
                ("direction", PropertyValue::Int(0)),
                ("head_piece_bit", PropertyValue::Byte(u8::from(head))),
                ("occupied_bit", PropertyValue::Byte(u8::from(occupied))),
            ],
        )
        .unwrap()
    }

    #[test]
    fn sleeping_through_the_night_skips_to_morning() {
        let mut server = Server::new(1, |_| {});
        let spawn = server.state.worlds[0].spawn();
        let foot = BlockPos::new(spawn.x, spawn.y, spawn.z + 1);
        let head = BlockPos::new(spawn.x, spawn.y, spawn.z + 2);
        server.state.worlds[0]
            .set_block(foot, bed(false, false))
            .unwrap();
        server.state.worlds[0]
            .set_block(head, bed(true, false))
            .unwrap();
        assert!(!sleep(&mut server.state, sid(1), 0, foot), "not at day");
        server.state.worlds[0].set_time(13_000);
        assert!(sleep(&mut server.state, sid(1), 0, foot));
        assert!(!sleep(&mut server.state, sid(1), 0, foot), "already asleep");
        assert_eq!(server.survival(1).sleeping, Some(head));
        assert_eq!(server.state.worlds[0].block(head), Some(bed(true, true)));
        assert_eq!(server.state.worlds[0].block(foot), Some(bed(false, true)));
        server.idle(60);
        let time = server.state.worlds[0].time();
        assert_eq!(time % 24_000, 0, "{time}");
        assert_eq!(server.survival(1).sleeping, None);
        assert_eq!(server.state.worlds[0].block(head), Some(bed(true, false)));
        assert!(
            server
                .take(1)
                .iter()
                .any(|p| matches!(p, Packet::Animate(_)))
        );
        let spawn_point = server.survival(1).spawn.clone().map(|p| p.pos);
        assert_eq!(spawn_point, Some(head));
    }

    #[test]
    fn damage_wakes_a_sleeper_and_occupied_beds_refuse() {
        let mut server = Server::new(2, |_| {});
        let spawn = server.state.worlds[0].spawn();
        let foot = BlockPos::new(spawn.x, spawn.y, spawn.z + 1);
        let head = BlockPos::new(spawn.x, spawn.y, spawn.z + 2);
        server.state.worlds[0]
            .set_block(foot, bed(false, false))
            .unwrap();
        server.state.worlds[0]
            .set_block(head, bed(true, false))
            .unwrap();
        server.state.worlds[0].set_time(14_000);
        assert!(sleep(&mut server.state, sid(1), 0, head));
        assert!(!sleep(&mut server.state, sid(2), 0, foot), "occupied");
        server.idle(60);
        assert_eq!(
            server.state.worlds[0].time() % 24_000,
            14_060,
            "player 2 is awake"
        );
        assert!(server.hurt(1, 1.0, Damage::Magic));
        assert_eq!(server.survival(1).sleeping, None);
        assert_eq!(server.state.worlds[0].block(head), Some(bed(true, false)));
    }

    #[test]
    fn a_player_that_joins_dead_sees_the_death_screen_and_a_saved_sleeper_wakes() {
        let mut server = Server::new(1, |_| {});
        let spawn = server.state.worlds[0].spawn();
        let foot = BlockPos::new(spawn.x, spawn.y, spawn.z + 1);
        let head = BlockPos::new(spawn.x, spawn.y, spawn.z + 2);
        server.state.worlds[0]
            .set_block(foot, bed(false, true))
            .unwrap();
        server.state.worlds[0]
            .set_block(head, bed(true, true))
            .unwrap();
        // As loaded from a snapshot taken while the player slept.
        server.survival(1).sleeping = Some(head);
        server.idle(1);
        assert_eq!(server.survival(1).sleeping, None);
        assert_eq!(server.state.worlds[0].block(head), Some(bed(true, false)));
        assert_eq!(server.state.worlds[0].block(foot), Some(bed(false, false)));
        // As loaded with no health left.
        server.survival(1).health = 0.0;
        server.idle(2);
        let sent = server.take(1);
        let screens = sent
            .iter()
            .filter(|p| matches!(p, Packet::DeathInfo(d) if d.cause == "death.attack.generic"))
            .count();
        assert_eq!(screens, 1, "shown once");
    }

    #[test]
    fn the_world_clock_advances_and_corrects_clients() {
        let mut server = Server::new(1, |_| {});
        let before = server.state.worlds[0].time();
        server.idle(300);
        assert_eq!(server.state.worlds[0].time(), before + 300);
        let syncs = server
            .take(1)
            .into_iter()
            .filter(|p| matches!(p, Packet::SyncWorldClocks(_)))
            .count();
        assert!((1..=2).contains(&syncs), "{syncs}");
    }

    #[test]
    fn the_enchanting_seed_is_drawn_once_and_saved() {
        let mut a = Server::new(1, |_| {});
        let mut b = Server::new(1, |_| {});
        let seed = enchantment_seed(a.player(1));
        assert_eq!(seed, enchantment_seed(b.player(1)));
        a.idle(5);
        assert_eq!(enchantment_seed(a.player(1)), seed);
        let mut extra = Compound::new();
        save(b.survival(1), &mut extra);
        assert_eq!(extra.get("XpSeed"), Some(&crate::nbt::Value::Int(seed)));
    }

    #[test]
    fn join_and_leave_cycles_leave_nothing_behind() {
        let mut server = Server::new(1, |_| {});
        let clocks = server.state.worlds[0].time();
        for i in 0..100 {
            let s = 2 + i;
            let (j, _rx) = join(s, 0);
            server.run(vec![j, radius(s, 1, 4)]);
            server.run(vec![initialised(s, 2, server.state.next_runtime_id - 1)]);
            server.run(vec![leave(s, 3)]);
        }
        assert_eq!(server.state.players.len(), 1);
        assert_eq!(server.state.clock.worlds_len(), 1);
        assert!(server.state.worlds[0].time() > clocks);
        assert!(server.player(1).outbox.is_empty());
        let s = server.survival(1);
        assert!(s.held_back.is_empty() && s.cooldowns.is_empty() && s.effects.is_empty());
        assert!(server.state.index_matches_players());
    }

    /// Every packet of `packets` that `pick` keeps.
    fn all<T>(packets: &[Packet], pick: impl Fn(&Packet) -> Option<T>) -> Vec<T> {
        packets.iter().filter_map(pick).collect()
    }

    /// The level events of `packets`.
    fn level_events(packets: &[Packet]) -> Vec<i32> {
        all(packets, |p| match p {
            Packet::LevelEvent(e) => Some(e.event_type),
            _ => None,
        })
    }

    /// The sound names of `packets`.
    fn sounds(packets: &[Packet]) -> Vec<String> {
        all(packets, |p| match p {
            Packet::LevelSoundEvent(e) => Some(e.sound_type.clone()),
            _ => None,
        })
    }

    /// A world directory removed when the test ends.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        #[expect(
            clippy::disallowed_methods,
            reason = "off-tick: the test makes the world directory a server start would use"
        )]
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("anchor-survival-{name}-{}", std::process::id()));
            drop(std::fs::remove_dir_all(&dir));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        #[expect(
            clippy::disallowed_methods,
            reason = "off-tick: the test removes its world directory"
        )]
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.0));
        }
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test stands in for the tick handing a snapshot to the save thread"
    )]
    fn world_time_survives_a_save_and_restart_to_the_tick() {
        use crate::save::{self, Request, Response, WorldSpec};
        let specs = || {
            vec![WorldSpec {
                name: "world".to_owned(),
                kind: crate::world::Kind::Persistent,
                generator: std::sync::Arc::new(crate::world::Flat),
                seed: Some(0),
            }]
        };
        // A world 23,990 ticks old, run 5 ticks, then saved at the tick boundary.
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        state.worlds[0].set_time(23_990);
        for _ in 0..5 {
            tick(&mut state, &mut Vec::new(), &mut output);
        }
        state.saving.requested = true;
        let mut log = crate::tick::InputLog::default();
        let Some(Request::Commit(snapshot)) =
            crate::tick::save::snapshot(&mut state, &mut log, false)
        else {
            panic!("no snapshot");
        };
        assert_eq!(snapshot.start, state.tick);
        assert_eq!(
            snapshot.spawns,
            [(0, state.worlds[0].spawn(), 23_995)],
            "the snapshot carries the time"
        );
        let dir = TempDir::new("time");
        let opened = save::open(&dir.0, specs()).unwrap();
        let (requests, queue) = tokio::sync::mpsc::channel(save::REQUEST_CAPACITY);
        let (answers, mut answered) = tokio::sync::mpsc::channel(save::RESPONSE_CAPACITY);
        let thread = opened.thread;
        let handle = crate::runtime::spawn_save(move || {
            let pool = crate::runtime::cpu_pool(1).unwrap();
            let result = save::run(thread, &pool, queue, &answers);
            assert!(pool.shutdown());
            result
        })
        .unwrap();
        requests.blocking_send(Request::Commit(snapshot)).unwrap();
        drop(requests);
        handle.join().unwrap().unwrap();
        assert!(matches!(
            answered.try_recv(),
            Ok(Response::Committed { start }) if start == state.tick
        ));

        // The restart takes the engine tick and the time from the file; both runs go on alike
        // through the night's end into the next day.
        let reopened = save::open(&dir.0, specs()).unwrap();
        assert_eq!(reopened.tick, state.tick);
        assert_eq!(reopened.worlds[0].time(), 23_995);
        let mut restarted = State::new(
            settings(|_| {}),
            crate::tick::Shared::new(crate::protocol::batch::Settings::default()).unwrap(),
            Listeners::default(),
            crate::tick::Tasks::default(),
            reopened.worlds,
            reopened.tick,
        );
        drop(reopened.thread);
        let mut restarted_output = Output::default();
        for _ in 0..40 {
            tick(&mut state, &mut Vec::new(), &mut output);
            tick(&mut restarted, &mut Vec::new(), &mut restarted_output);
            assert_eq!(restarted.tick, state.tick);
            assert_eq!(restarted.worlds[0].time(), state.worlds[0].time());
        }
        assert_eq!(restarted.worlds[0].time(), 24_035);
        assert_eq!(restarted.worlds[0].time_of_day(), 35, "a new day");
    }

    /// The attacker's side of a hit on an NPC is the one of a hit on a player: Strength and
    /// Weakness (PMMP `addAttackerModifiers`), then half again for a critical hit.
    #[test]
    fn melee_hits_count_strength_weakness_and_criticals() {
        let mut server = Server::new(1, |_| {});
        let plain = Melee {
            damage: 1.0,
            critical: false,
            magic: false,
        };
        assert_eq!(melee(&server.state, sid(1)), Some(plain));
        let s = server.survival(1);
        s.effects
            .insert(effect::Type::Strength, Active::new(100, 1));
        s.fall_distance = 1.0;
        let m = melee(&server.state, sid(1)).unwrap();
        assert!(m.critical && !m.magic);
        assert!((m.damage - 1.6 * 1.5).abs() < 1e-9, "{}", m.damage);
        let s = server.survival(1);
        s.effects
            .insert(effect::Type::Weakness, Active::new(100, 0));
        let m = melee(&server.state, sid(1)).unwrap();
        assert!((m.damage - 1.4 * 1.5).abs() < 1e-9, "{}", m.damage);
        assert!(server.hurt(1, 100.0, Damage::Magic));
        assert_eq!(melee(&server.state, sid(1)), None, "the dead hit nothing");
    }

    /// Healing, burning and the hit cooldown other rules reach through the contract.
    #[test]
    fn heal_burn_and_the_hit_cooldown_follow_pmmp() {
        let mut server = Server::new(1, |_| {});
        server.survival(1).health = 10.0;
        heal(&mut server.state, sid(1), 4.0);
        assert_eq!(server.survival(1).health, 14.0);
        heal(&mut server.state, sid(1), 100.0);
        assert_eq!(server.survival(1).health, 20.0, "up to the maximum");
        set_on_fire(&mut server.state, sid(1), 5);
        assert_eq!(server.survival(1).fire_ticks, 100);
        set_on_fire(&mut server.state, sid(1), 2);
        assert_eq!(server.survival(1).fire_ticks, 100, "a longer burn stays");
        assert!(!has_attack_cooldown(server.player(1)));
        assert!(server.hurt(1, 1.0, Damage::Entity));
        assert!(has_attack_cooldown(server.player(1)));
        server.idle(10);
        assert!(!has_attack_cooldown(server.player(1)));
    }

    #[test]
    fn the_contract_reads_follow_the_player_and_the_world() {
        let mut server = Server::new(1, |_| {});
        let s = server.survival(1);
        s.motion.flying = true;
        s.motion.sneaking = true;
        s.motion.sneak_pressed = true;
        s.motion.on_ground = true;
        s.motion.yaw = 90.0;
        s.motion.pitch = 10.0;
        s.effects.insert(effect::Type::Haste, Active::new(100, 1));
        s.effects
            .insert(effect::Type::MiningFatigue, Active::new(100, 2));
        let p = server.player(1);
        assert!(is_flying(p) && is_sneaking(p) && is_sneak_pressed(p) && is_on_ground(p));
        assert_eq!(rotation(p), (90.0, 10.0));
        assert!(look_direction(p).x < -0.9);
        assert_eq!((haste_level(p), mining_fatigue_level(p)), (2, 3));
        assert!(!is_underwater(&server.state, p));
        assert!(is_alive(p));
        let pearl = ItemKind::EnderPearl.default_type();
        assert!(!has_item_cooldown(&server.state, server.player(1), pearl));
        server.take(1);
        reset_item_cooldown(&mut server.state, sid(1), pearl);
        assert!(has_item_cooldown(&server.state, server.player(1), pearl));
        server.idle(1);
        let told = server.take(1).into_iter().any(|p| {
            p == Packet::ClientStartItemCooldown(p::ClientStartItemCooldown {
                category: "minecraft:ender_pearl".to_owned(),
                duration: 20,
            })
        });
        assert!(told, "the client hears the cooldown");
        server.idle(19);
        assert!(!has_item_cooldown(&server.state, server.player(1), pearl));
        assert!(server.hurt(1, 100.0, Damage::Magic));
        assert!(!is_alive(server.player(1)));
    }

    #[test]
    fn cake_feeds_two_and_needs_hunger() {
        let mut server = Server::new(1, |_| {});
        server.take(1);
        assert!(!eat_cake(&mut server.state, sid(1)), "full");
        server.survival(1).food = 10.0;
        server.survival(1).saturation = 0.0;
        assert!(eat_cake(&mut server.state, sid(1)));
        let s = server.survival(1);
        assert_eq!(s.food, 12.0);
        assert!((s.saturation - 0.4).abs() < 1e-12);
        server.idle(1);
        assert_eq!(sounds(&server.take(1)), ["burp"]);
        let mut peaceful = Server::new(1, |c| c.general.difficulty = Difficulty::Peaceful);
        assert!(
            eat_cake(&mut peaceful.state, sid(1)),
            "peaceful eats when full"
        );
    }

    #[test]
    fn effects_replace_only_stronger_or_longer_ones_as_pmmp() {
        let mut server = Server::new(1, |_| {});
        let speed = |duration, amplifier| {
            effect::Instance::new(effect::Type::Speed, duration, amplifier, true, false)
        };
        assert!(add_effect(&mut server.state, sid(1), speed(100, 1)));
        assert!(
            !add_effect(&mut server.state, sid(1), speed(500, 0)),
            "weaker"
        );
        assert!(
            !add_effect(&mut server.state, sid(1), speed(99, 1)),
            "as strong, shorter"
        );
        assert!(
            add_effect(&mut server.state, sid(1), speed(100, 1)),
            "as strong, as long"
        );
        assert!(
            add_effect(&mut server.state, sid(1), speed(10, 2)),
            "stronger"
        );
        assert_eq!(
            server.survival(1).effects[&effect::Type::Speed],
            Active::new(10, 2)
        );
        server.idle(1);
        let operations = all(&server.take(1), |p| match p {
            Packet::MobEffect(m) => Some(m.operation),
            _ => None,
        });
        let (add, modify) = (
            u8::from(MobEffectOperation::Add),
            u8::from(MobEffectOperation::Modify),
        );
        assert_eq!(operations, [add, modify, modify]);
        // A replaced health boost is undone first (PMMP `HealthBoostEffect::remove`).
        let boost = |amplifier| {
            effect::Instance::new(effect::Type::HealthBoost, 600, amplifier, true, false)
        };
        assert!(add_effect(&mut server.state, sid(1), boost(1)));
        server.survival(1).health = 28.0;
        assert!(add_effect(&mut server.state, sid(1), boost(2)));
        assert_eq!(server.survival(1).health, 20.0);
        assert_eq!(server.survival(1).max_health(), 32.0);
    }

    #[test]
    fn a_totem_in_either_hand_saves_the_player_as_pmmp() {
        for main in [true, false] {
            let mut server = Server::new(1, |_| {});
            let totem = stack(ItemKind::Totem, 1);
            if main {
                inventory::set_held(server.player_mut(1), totem.clone());
            }
            inventory::set_offhand(server.player_mut(1), totem);
            server.idle(1);
            server.take(1);
            assert!(server.hurt(1, 100.0, Damage::Magic));
            let s = server.survival(1);
            assert_eq!(s.health, 1.0, "the totem leaves one health");
            assert_eq!(s.absorption, 8.0);
            for (effect, amplifier, duration) in [
                (effect::Type::Regeneration, 1, 800),
                (effect::Type::FireResistance, 1, 800),
                (effect::Type::Absorption, 1, 100),
            ] {
                assert_eq!(s.effects[&effect], Active::new(duration, amplifier));
            }
            let inv = &server.player(1).inventory;
            let (held, off) = (inv.held().is_empty(), inv.offhand().is_empty());
            assert_eq!(
                (held, off),
                (true, !main),
                "the main hand's totem goes first"
            );
            server.idle(1);
            let sent = server.take(1);
            assert!(level_events(&sent).contains(&i32::from(LevelEvent::SoundTotemUsed)));
            assert!(sent.iter().any(|p| matches!(
                p,
                Packet::ActorEvent(e) if e.event_type == u8::from(ActorEvent::TalismanActivate)
            )));
        }
        // The void is not stopped.
        let mut server = Server::new(1, |_| {});
        inventory::set_offhand(server.player_mut(1), stack(ItemKind::Totem, 1));
        assert!(server.hurt(1, 100.0, Damage::Void));
        assert!(server.survival(1).is_dead());
    }

    #[test]
    fn orb_pickups_mend_first_then_add_experience_as_pmmp() {
        let mut server = Server::new(1, |_| {});
        let pick =
            enchanted(ItemKind::DiamondPickaxe, &[(Enchantment::Mending, 1)]).with_damage(10);
        inventory::set_held(server.player_mut(1), pick);
        server.idle(1);
        server.take(1);
        // 3 points repair 6 durability and are used up.
        assert!(pickup_experience(&mut server.state, sid(1), 3));
        assert_eq!(server.player(1).inventory.held().damage(), 4);
        assert_eq!(food::current_xp(server.survival(1)), 0);
        assert!(
            !pickup_experience(&mut server.state, sid(1), 3),
            "the pickup cooldown"
        );
        server.idle(2);
        // 5 points: 4 durability left to repair takes 2 points; 3 are experience.
        assert!(pickup_experience(&mut server.state, sid(1), 5));
        assert_eq!(server.player(1).inventory.held().damage(), 0);
        assert_eq!(food::current_xp(server.survival(1)), 3);
        server.idle(1);
        let sent = server.take(1);
        assert!(level_events(&sent).contains(&i32::from(LevelEvent::SoundExperienceOrbPickup)));
        // Worn armour mends too; a level that crosses a multiple of 5 plays the level-up sound.
        let helmet =
            enchanted(ItemKind::DiamondHelmet, &[(Enchantment::Mending, 1)]).with_damage(1);
        inventory::set_held(server.player_mut(1), Stack::EMPTY);
        inventory::set_armor(server.player_mut(1), 0, helmet);
        add_experience(server.player_mut(1), 38);
        assert_eq!(server.survival(1).xp_level, 4);
        server.idle(3);
        server.take(1);
        assert!(pickup_experience(&mut server.state, sid(1), 20));
        assert_eq!(server.player(1).inventory.armor()[0].damage(), 0);
        assert_eq!(server.survival(1).xp_level, 5);
        server.idle(1);
        assert_eq!(sounds(&server.take(1)), ["levelup"]);
        // Dead players pick nothing up.
        server.hurt(1, 100.0, Damage::Magic);
        server.idle(3);
        assert!(!pickup_experience(&mut server.state, sid(1), 5));
    }

    #[test]
    fn medicine_cures_its_effect_and_starts_only_with_it() {
        let mut server = Server::new(1, |_| {});
        let antidote =
            crate::item::Type::from_name("minecraft:medicine", 2, None, &Compound::new()).unwrap();
        inventory::set_held(server.player_mut(1), Stack::new(antidote, 1));
        assert!(use_consumable(&mut server.state, sid(1)));
        assert!(
            server.survival(1).using.is_none(),
            "not poisoned: nothing to drink"
        );
        effects::add(
            &mut server.state,
            sid(1),
            effect::Type::Poison,
            Active::new(1000, 0),
        );
        effects::add(
            &mut server.state,
            sid(1),
            effect::Type::Blindness,
            Active::new(1000, 0),
        );
        assert!(use_consumable(&mut server.state, sid(1)));
        server.idle(32);
        assert!(use_consumable(&mut server.state, sid(1)));
        let s = server.survival(1);
        assert!(!s.effects.contains_key(&effect::Type::Poison));
        assert!(s.effects.contains_key(&effect::Type::Blindness));
        let held = server.player(1).inventory.held().item().unwrap();
        assert_eq!(held.name(), "minecraft:glass_bottle");
    }

    #[test]
    fn landing_on_farmland_tramples_it_as_pmmp() {
        // PMMP `Farmland::onEntityLand`: trampled when a random float is below the fall distance
        // less 0.5, so always after a fall of 1.5 blocks or more, never after 0.5 or less.
        for (fall, trampled, sound) in [
            (0.5_f64, false, "land"),
            (2.0, true, "land"),
            (5.0, true, "fall.small"),
        ] {
            let mut server = Server::new(1, |c| c.developer.random_tick_speed = 0);
            let start = server.player(1).position;
            let below = block_at(feet(server.player(1)));
            let below = BlockPos::new(below.x, below.y - 1, below.z);
            let farmland = block::Kind::Farmland.default_state();
            server.state.worlds[0].set_block(below, farmland).unwrap();
            server.survival(1).motion.force_sync = false;
            for (i, y) in [fall, 0.0].into_iter().enumerate() {
                let auth = p::PlayerAuthInput {
                    position: (start + Vec3::new(0.0, y, 0.0)).to_f32(),
                    tick: u64::try_from(i).unwrap() + 1,
                    ..Default::default()
                };
                let input = server.send(1, Packet::PlayerAuthInput(Box::new(auth)));
                server.run(vec![input]);
            }
            let now = server.state.worlds[0].block(below).unwrap();
            assert_eq!(now == block::DIRT, trampled, "fall {fall}");
            assert_eq!(server.survival(1).fall_distance, 0.0);
            assert!(
                sounds(&server.take(1)).contains(&sound.to_owned()),
                "{fall}"
            );
        }
    }

    #[test]
    fn cactus_and_lava_hurt_a_player_inside_them() {
        let mut server = Server::new(1, |_| {});
        let at = block_at(feet(server.player(1)));
        server.state.worlds[0]
            .set_block(at, block::Kind::Cactus.default_state())
            .unwrap();
        server.idle(1);
        assert_eq!(server.survival(1).health, 19.0, "cactus pricks for 1");
        server.idle(10);
        server.state.worlds[0]
            .set_block(at, block::Kind::Lava.default_state())
            .unwrap();
        server.idle(1);
        let s = server.survival(1);
        assert!(s.health <= 15.0, "lava burns for 4: {}", s.health);
        assert!(s.fire_ticks > 0);
    }

    /// A join of session `s` with `identity`.
    fn join_with(
        s: u64,
        identity: crate::tick::queue::Identity,
    ) -> (Input, mpsc::Receiver<Outbound>) {
        let (tx, rx) = crate::tick::queue::outbound_queue();
        let kind = crate::tick::queue::InputKind::Join {
            identity,
            outbound: tx,
        };
        (input(s, 0, kind), rx)
    }

    /// A skin with something in every field.
    fn custom_skin() -> crate::protocol::Skin {
        use crate::protocol::{PersonaPiece, PersonaPieceTintColour, Rgba, SkinAnimation};
        let colour = |v: u8| Rgba {
            r: v,
            g: v + 1,
            b: v + 2,
            a: 255,
        };
        crate::protocol::Skin {
            skin_id: "c18e65aa-7b21-4637-9b63-8ad63622ef01_Custom".to_owned(),
            playfab_id: "0123456789abcdef".to_owned(),
            skin_resource_patch: br#"{"geometry":{"default":"geometry.humanoid.customSlim"}}"#
                .to_vec(),
            skin_image_width: 64,
            skin_image_height: 64,
            skin_data: (0..64 * 64 * 4)
                .map(|i| u8::try_from(i % 251).unwrap())
                .collect(),
            animations: vec![SkinAnimation {
                image_width: 32,
                image_height: 64,
                image_data: vec![7; 32 * 64 * 4],
                animation_type: 1,
                frame_count: 2.0,
                expression_type: 1,
            }],
            cape_image_width: 64,
            cape_image_height: 32,
            cape_data: vec![9; 64 * 32 * 4],
            skin_geometry: br#"{"format_version":"1.12.0"}"#.to_vec(),
            animation_data: b"anim".to_vec(),
            geometry_data_engine_version: b"1.14.0".to_vec(),
            premium_skin: true,
            persona_skin: true,
            persona_cape_on_classic_skin: true,
            primary_user: true,
            cape_id: "cape".to_owned(),
            full_id: "c18e65aa-7b21-4637-9b63-8ad63622ef01_Customcape".to_owned(),
            skin_colour: colour(10),
            arm_size: 0,
            persona_pieces: vec![PersonaPiece {
                piece_id: "piece".to_owned(),
                piece_type: 2,
                pack_id: crate::protocol::Uuid::from_bytes([3; 16]),
                default: true,
                product_id: String::new(),
            }],
            piece_tint_colours: vec![PersonaPieceTintColour {
                piece_type: "persona_eyes".to_owned(),
                colours: [colour(1), colour(2), colour(3), colour(4)],
            }],
            trusted: true,
            override_appearance: true,
            profile_hash: "hash".to_owned(),
        }
    }

    /// The protocol bytes of a skin.
    fn skin_bytes(skin: &crate::protocol::Skin) -> Vec<u8> {
        let mut w = crate::protocol::Writer::new(Vec::new(), 0);
        crate::protocol::Codec::encode(skin, &mut w).unwrap();
        w.into_inner()
    }

    #[test]
    fn login_skins_reach_every_player_list_byte_for_byte() {
        let mut server = Server::new(1, |_| {});
        let skin = custom_skin();
        let identity = crate::tick::queue::Identity {
            name: "Alex".to_owned(),
            xuid: None,
            uuid: crate::protocol::Uuid::from_bytes([9; 16]),
            skin: crate::tick::queue::Skin::new(skin.clone()),
            device_os: 7,
        };
        let (j, mut rx) = join_with(2, identity.clone());
        server.run(vec![j, radius(2, 1, 8)]);
        let entries = |packets: &[Packet]| {
            all(packets, |p| match p {
                Packet::PlayerList(l) => Some(l.entries.clone()),
                _ => None,
            })
            .concat()
        };
        // The joining player's own list (twice, as BDS) and the other player's addition.
        let own = entries(&received(&mut rx).0);
        let theirs = entries(&server.take(1));
        let shown: Vec<_> = own
            .iter()
            .chain(&theirs)
            .filter(|e| e.username == "Alex")
            .collect();
        assert_eq!(shown.len(), 3);
        for entry in shown {
            assert_eq!(entry.skin, skin);
            assert_eq!(skin_bytes(&entry.skin), skin_bytes(&skin));
            assert_eq!(entry.build_platform, 7);
        }
        // The input log keeps the skin for a replay, counted against its bound.
        let mut log = crate::tick::InputLog::starting_at(0);
        log.begin(0);
        let size = identity.skin.size();
        assert!(size > 64 * 64 * 4 + 64 * 32 * 4);
        log.record(
            SessionId::new(2),
            0,
            crate::tick::log::EntryKind::Join(identity),
            size,
        );
        let bytes = log.encode().unwrap();
        let skin_data = &skin.skin_data[..];
        assert!(bytes.windows(skin_data.len()).any(|w| w == skin_data));
        let again = crate::tick::InputLog::decode(&bytes).unwrap();
        assert_eq!(again.encode().unwrap(), bytes);
    }

    #[test]
    fn teleports_move_the_view_reset_the_fall_and_cross_worlds() {
        let mut server = Server::new(2, |_| {});
        let held = server.state.chunks.held();
        server.survival(1).fall_distance = 7.0;
        let near = server.player(1).position + Vec3::new(20.0, 0.0, 0.0);
        teleport(&mut server.state, sid(1), 0, near);
        assert_eq!(server.player(1).position, near);
        assert_eq!(server.survival(1).fall_distance, 0.0);
        let teleported = |packets: &[Packet]| {
            packets.iter().any(|p| {
                matches!(p, Packet::MovePlayer(m)
                    if m.mode == u8::from(MoveMode::Teleport)
                        && m.entity_runtime_id == 1
                        && m.position == near.to_f32())
            })
        };
        server.idle(1);
        assert!(teleported(&server.take(1)), "the player");
        assert!(teleported(&server.take(2)), "a viewer");
        // Far away: the view follows (chunks there are loaded and sent).
        let far = Vec3::new(400.5, -58.38, 0.5);
        teleport(&mut server.state, sid(1), 0, far);
        server.idle(4);
        let chunk = crate::tick::view::chunk_of(far);
        assert!(server.player(1).view.sees(chunk));
        // Out of the world or into no world: nothing happens.
        teleport(&mut server.state, sid(1), 0, Vec3::new(f64::NAN, 0.0, 0.0));
        teleport(&mut server.state, sid(1), 0, Vec3::new(4.0e7, 0.0, 0.0));
        teleport(&mut server.state, sid(1), 5, far);
        assert_eq!(server.player(1).position, far);
        // Another world: its chunks and clock, the old world's chunks let go.
        let spawn = BlockPos::new(0, -60, 0);
        let other = crate::world::World::new("other", crate::world::Kind::Temporary, 3, spawn);
        server.state.worlds.push(other);
        server.state.worlds[1].set_time(5_000);
        server.take(1);
        let there = Vec3::new(0.5, -58.38, 0.5);
        teleport(&mut server.state, sid(1), 1, there);
        assert_eq!(server.player(1).world, 1);
        assert_eq!(server.player(1).position, there);
        assert!(!server.player(1).view.sees(chunk));
        server.idle(4);
        assert!(
            server
                .player(1)
                .view
                .sees(crate::tick::view::chunk_of(there))
        );
        let sent = server.take(1);
        assert!(sent.iter().any(|p| matches!(
            p,
            Packet::SyncWorldClocks(c) if c.sync_states.first().is_some_and(|s| s.time > 5_000)
        )));
        assert!(sent.iter().any(|p| matches!(p, Packet::LevelChunk(_))));
        // Back home, nothing of the trip is held.
        let home = server.player(2).position;
        teleport(&mut server.state, sid(1), 0, home);
        server.idle(4);
        server.player_mut(1).view.out = crate::tick::view::Out::default();
        assert_eq!(server.state.chunks.held(), held);
    }

    #[test]
    fn players_respawn_at_a_spawn_point_in_another_world() {
        let mut server = Server::new(1, |_| {});
        let spawn = BlockPos::new(0, -60, 0);
        let other = crate::world::World::new("other", crate::world::Kind::Temporary, 3, spawn);
        server.state.worlds.push(other);
        server.survival(1).spawn = Some(Point {
            world: "other".to_owned(),
            pos: BlockPos::new(2, -60, 2),
        });
        assert!(server.hurt(1, 100.0, Damage::Magic));
        damage::respawn(&mut server.state, sid(1));
        let p = server.player(1);
        assert_eq!(p.world, 1);
        assert_eq!(p.position, Vec3::new(2.5, -60.0 + EYE_HEIGHT, 2.5));
        assert!(!p.survival.is_dead());
        // A spawn point in a world the server does not run: the default world's spawn.
        server.survival(1).spawn = Some(Point {
            world: "gone".to_owned(),
            pos: BlockPos::new(2, -60, 2),
        });
        server.idle(80);
        assert!(server.hurt(1, 100.0, Damage::Magic));
        damage::respawn(&mut server.state, sid(1));
        assert_eq!(server.player(1).world, 0);
        let home = server.state.worlds[0].spawn();
        assert_eq!(server.player(1).position.x, f64::from(home.x) + 0.5);
    }

    #[test]
    fn held_back_death_drops_survive_a_restart_and_drop_without_loss() {
        let named = stack(ItemKind::DiamondSword, 1).with_extra(Some(Extra {
            custom_name: Some("Edge".to_owned()),
            enchantments: vec![(Enchantment::Sharpness, 3)],
            ..Extra::default()
        }));
        let held: Vec<Stack> = vec![
            stack(ItemKind::Bread, 64),
            named,
            stack(ItemKind::IronPickaxe, 1).with_damage(40),
            stack(ItemKind::Totem, 1),
        ];
        let s = Survival {
            held_back: held.clone(),
            ..Survival::default()
        };
        let mut extra = Compound::new();
        save(&s, &mut extra);
        // Through the world file's player record.
        let record = crate::storage::codec::PlayerRecord {
            world: "world".to_owned(),
            position: [0.5, -58.38, 0.5],
            extra,
        };
        let sealed = crate::storage::seal::seal(b"player", &record).unwrap();
        let read: crate::storage::codec::PlayerRecord =
            crate::storage::seal::unseal(b"player", &sealed).unwrap();
        let mut loaded = Survival::default();
        load(&mut loaded, &read.extra);
        assert_eq!(loaded.held_back, held);
        let mut again = Compound::new();
        save(&loaded, &mut again);
        assert_eq!(again, read.extra, "saved again byte for byte");
        // Once the world takes them, every item drops.
        let mut server = Server::new(1, |_| {});
        server.survival(1).held_back = loaded.held_back;
        server.idle(3);
        assert!(server.survival(1).held_back.is_empty());
        let dropped: u32 = all(&server.take(1), |p| match p {
            Packet::AddItemActor(a) => Some(u32::from(a.item.stack.count)),
            _ => None,
        })
        .iter()
        .sum();
        let want: u32 = held.iter().map(|s| u32::from(s.count())).sum();
        assert_eq!(dropped, want);
    }

    #[test]
    fn join_die_respawn_and_leave_cycles_leave_nothing_behind() {
        let mut server = Server::new(1, |_| {});
        let held = server.state.chunks.held();
        for i in 0..100 {
            let s = 2 + i;
            let (j, _rx) = join(s, 0);
            server.run(vec![j, radius(s, 1, 4)]);
            let runtime_id = server.state.next_runtime_id - 1;
            server.run(vec![initialised(s, 2, runtime_id)]);
            assert!(server.hurt(s, 100.0, Damage::Magic));
            let ready = p::Respawn {
                position: [0.0; 3],
                state: u8::from(RespawnState::ClientReadyToSpawn),
                entity_runtime_id: runtime_id,
            };
            let action = p::PlayerAction {
                entity_runtime_id: runtime_id,
                action_type: i32::from(Action::Respawn),
                ..Default::default()
            };
            let a = input(s, 3, packet(Packet::Respawn(ready)));
            let b = input(s, 4, packet(Packet::PlayerAction(action)));
            server.run(vec![a, b]);
            assert!(is_alive(server.player(s)));
            server.run(vec![leave(s, 5)]);
        }
        server.idle(2);
        assert_eq!(server.state.players.len(), 1);
        assert!(server.state.index_matches_players());
        assert_eq!(server.state.clock.worlds_len(), 1);
        assert_eq!(server.state.chunks.held(), held);
        assert!(server.player(1).outbox.is_empty());
        let s = server.survival(1);
        assert!(s.held_back.is_empty() && s.cooldowns.is_empty() && s.effects.is_empty());
    }
}
