//! Survival rules of players: movement, health, food, xp, effects, damage, death and
//! respawn, game modes and abilities, attributes, combat and the world clock.
//!
//! Every rule runs on the tick thread in a serial phase. A player's survival state changes only
//! here; other modules call the functions below. The client hears about changes at the end of
//! [`tick`]: changed attributes (`UpdateAttributes`), changed entity data (`SetActorData`), and the
//! packets the rules queue on the way (effects, respawn, hurt animations). These updates of a
//! player's own state wait until it is spawned (`SetLocalPlayerAsInitialised`), so they stay out
//! of the spawn window ([`super::spawn`]); a teleport before then still tells the client its
//! position and another world's difficulty, weather and clock.

mod api;
mod appearance;
mod attributes;
mod clock;
mod damage;
mod effects;
mod food;
mod movement;
mod saved;
mod sound;
#[cfg(test)]
mod wiring;

pub(super) use crate::player::{HEIGHT as PLAYER_HEIGHT, WIDTH as PLAYER_WIDTH};
pub(super) use api::{marked, sync};
pub(super) use appearance::{SkinBudget, send_skins, skin_waits};
pub(super) use attributes::{
    HEALTH_ATTRIBUTE, actor_data, adventure_settings, all_attributes, full_metadata, game_rules,
    layer_of, mode_abilities, update_abilities,
};
pub(super) use clock::{Clock, clock_definition, clock_state, day_cycle_lock_time};
pub(super) use damage::{Damage, actor_cause, heals, protection_level, respawn_joined};
pub(super) use saved::{game_mode_id, load_played, save_played};

use std::collections::BTreeMap;
use std::ops::Bound;

use super::{Context, Phase, Player, SessionId, Settings, State, checkpoint, dispatch, inventory};
use crate::config::{Difficulty, GameMode};
use crate::effect;
use crate::event::{
    CombustSource, DamageCause, DamageSource, Event, ExhaustCause, PlayerGameModeChange,
    PlayerWorldChange, RegainReason,
};
use crate::item::Stack;
use crate::math::{Vec3, to_f32};
use crate::nbt::Compound;
use crate::player::Id;
use crate::protocol::consts::MoveMode;
use crate::protocol::packet::{Emote, Packet, PlayerAction, PlayerAuthInput, PlayerSkin, Respawn};
use crate::random::{self, Random};
use crate::save::WorldIndex;
use crate::world::{BlockPos, Weather, World};

/// The saturation of a new or respawned player (P6).
const DEFAULT_SATURATION: f64 = 5.0;
/// How far a player reaches an entity or a player.
pub(super) const ENTITY_REACH: f64 = 8.0;
/// Eye height above the feet; a human NPC's too.
pub(super) const EYE_HEIGHT: f64 = 1.62;
/// Air supply in ticks.
const MAX_AIR_TICKS: i16 = 300;
/// The largest exhaustion: the official server's `minecraft:player.exhaustion` maximum (P6).
const MAX_EXHAUSTION: f64 = 20.0;
/// The longest burn in ticks.
const MAX_FIRE_TICKS: u32 = 0x7fff;
/// The largest food level.
const MAX_FOOD: f64 = 20.0;
/// The health of a player without effects.
const MAX_HEALTH: f64 = 20.0;
/// The most held-back death drops: what death takes (the slots and the stacks waiting for room,
/// [`inventory::MAX_SAVED_PENDING`]) and drops a listener set in its place, all refused by the
/// world.
const MAX_HELD_BACK: usize = inventory::CARRIED + inventory::MAX_SAVED_PENDING + MAX_LISTENER_DROPS;
/// The most drops a [`PlayerDeath`](crate::event::PlayerDeath) listener sets that are kept: one
/// inventory.
pub(crate) const MAX_LISTENER_DROPS: usize = inventory::CARRIED;
/// The most residue stacks a [`PlayerItemConsume`](crate::event::PlayerItemConsume) listener
/// leaves after eating or drinking.
pub(crate) const MAX_RESIDUE: usize = 4;
/// The largest saturation.
const MAX_SATURATION: f64 = 20.0;
/// The lowest air supply in ticks: a player under water reaching it takes drowning damage and
/// has 0 again.
const MIN_AIR_TICKS: i16 = -20;
/// How far behind the plane its eyes look through a player reaches: `√3 / 2`.
const REACH_BEHIND: f64 = 0.866_025_403_784_438_6;

/// A player's survival state.
#[derive(Debug)]
pub(super) struct Survival {
    /// The game mode, once set from the saved data or a change; `None` uses the server default.
    game_mode: Option<GameMode>,
    /// The server's game mode (`gamemode`), fixed at start.
    default_mode: GameMode,
    /// Whether a join ignores the saved game mode (`force-gamemode`), fixed at start.
    forced_mode: bool,
    /// The systems the settings turn on, fixed at start.
    switches: Switches,
    /// Health in half hearts, 0 to [`Self::max_health`]; 0 is dead.
    health: f64,
    /// Extra health absorbed first, 0 or more.
    absorption: f64,
    /// Food level, 0 to 20.
    food: f64,
    /// Saturation, 0 to 20.
    saturation: f64,
    /// Exhaustion, 0 to [`MAX_EXHAUSTION`]; below 4 after each exhaustion.
    exhaustion: f64,
    /// Ticks of the food cycle, 0 to 79.
    food_timer: u32,
    /// Xp level, 0 to 24,791.
    xp_level: u32,
    /// Progress through the level, 0 to below 1.
    xp_progress: f64,
    /// Xp collected since the last death, at most `i32::MAX`.
    xp_lifetime: u32,
    /// Ticks until the next orb pickup, 0 to 2.
    xp_cooldown: u8,
    /// Active effects; at most one per effect type.
    effects: BTreeMap<effect::Type, effects::Active>,
    /// Ticks the player keeps burning, 0 to 32,767.
    fire_ticks: u16,
    /// Air supply in ticks, [`MIN_AIR_TICKS`] to the largest the API set ([`MAX_AIR_TICKS`] unless changed).
    air: i16,
    /// Whether the player could breathe at its last tick (the Breathing entity flag): false until
    /// its first tick, as the official server shows a new player (P6).
    breathing: bool,
    /// Blocks fallen since the player last stood on something.
    fall_distance: f64,
    /// The respawn point, set by the API and saved; `None` respawns the player at the default
    /// world's spawn.
    spawn: Option<Point>,
    /// Where the player last died.
    death: Option<Point>,
    /// Movement and input state.
    motion: movement::Motion,
    /// Ticks the player ignores damage after respawning.
    no_damage_ticks: u32,
    /// Ticks since the last hit within which a weaker hit does nothing.
    attack_time: u32,
    /// The base damage of the last hit taken.
    last_damage: Option<f64>,
    /// Why and from what the player last took damage; kept through a respawn. Not saved.
    last_cause: Option<(DamageCause, DamageSource)>,
    /// The item the player started eating or drinking, and when.
    using: Option<food::Using>,
    /// Item cooldowns: the cooldown key and the tick it ends.
    cooldowns: BTreeMap<food::CooldownKey, u64>,
    /// Death drops that could not be dropped (entity limit) nor put back; dropped as soon as the
    /// world takes them: at most [`MAX_HELD_BACK`]. Saved with the player.
    held_back: Vec<Stack>,
    /// Random draws made for this player, so no tick opens the same stream twice.
    draws: u64,
    /// What the client must be told at the end of the tick.
    dirty: u16,
    /// The entity data the viewers were last sent.
    shown: attributes::Shown,
    /// What the public API set on the player.
    custom: api::Custom,
    /// When the player last emoted and changed its skin.
    appearance: appearance::Appearance,
}

impl Survival {
    /// The survival state of a player who just joined, before its saved data.
    pub(super) fn new(settings: &Settings) -> Self {
        Self {
            forced_mode: settings.force_game_mode,
            switches: Switches {
                food: settings.food,
                xp: settings.xp,
                drowning: settings.drowning,
                armor: settings.armor,
            },
            // Without air the player always breathes.
            breathing: !settings.drowning,
            ..Self::with_mode(settings.game_mode)
        }
    }

    /// Hashes the state kept between ticks (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            game_mode,
            default_mode,
            forced_mode,
            switches,
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
            effects,
            fire_ticks,
            air,
            breathing,
            fall_distance,
            spawn,
            death,
            motion,
            no_damage_ticks,
            attack_time,
            last_damage,
            last_cause,
            using,
            cooldowns,
            held_back,
            draws,
            dirty,
            shown,
            custom,
            appearance,
        } = self;
        h.u64(game_mode.map_or(0, |m| u64::from(saved::game_mode_id(m).cast_unsigned()) + 1));
        h.u64(u64::from(
            saved::game_mode_id(*default_mode).cast_unsigned(),
        ));
        h.u64(u64::from(*forced_mode));
        switches.hash_into(h);
        for v in [health, absorption, food, saturation, exhaustion] {
            h.u64(v.to_bits());
        }
        h.u64(u64::from(*food_timer));
        h.u64(u64::from(*xp_level));
        h.u64(xp_progress.to_bits());
        h.u64(u64::from(*xp_lifetime));
        h.u64(u64::from(*xp_cooldown));
        h.usize(effects.len());
        for (effect, active) in effects {
            h.u64(u64::from(effect.id()));
            active.hash_into(h);
        }
        h.u64(u64::from(*fire_ticks));
        h.u64(u64::from(air.cast_unsigned()));
        h.u64(u64::from(*breathing));
        h.u64(fall_distance.to_bits());
        for point in [spawn, death] {
            match point {
                Some(p) => {
                    h.bytes(p.world.as_bytes());
                    h.block_pos(p.pos);
                }
                None => h.u64(0),
            }
        }
        motion.hash_into(h);
        h.u64(u64::from(*no_damage_ticks));
        h.u64(u64::from(*attack_time));
        h.u64(last_damage.map_or(0, f64::to_bits));
        h.bytes(format!("{last_cause:?}").as_bytes());
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
        custom.hash_into(h);
        appearance.hash_into(h);
    }

    /// A new player's state on a server whose game mode is `default_mode`.
    fn with_mode(default_mode: GameMode) -> Self {
        Self {
            game_mode: None,
            default_mode,
            forced_mode: false,
            switches: Switches::ALL,
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
            effects: BTreeMap::new(),
            fire_ticks: 0,
            air: MAX_AIR_TICKS,
            breathing: false,
            fall_distance: 0.0,
            spawn: None,
            death: None,
            motion: movement::Motion::default(),
            no_damage_ticks: 0,
            attack_time: 0,
            last_damage: None,
            last_cause: None,
            using: None,
            cooldowns: BTreeMap::new(),
            held_back: Vec::new(),
            draws: 0,
            dirty: 0,
            shown: attributes::Shown::default(),
            custom: api::Custom::default(),
            appearance: appearance::Appearance::default(),
        }
    }

    /// The level of an active effect (amplifier + 1), 0 when it is not active.
    fn effect_level(&self, effect: effect::Type) -> u32 {
        self.effects.get(&effect).map_or(0, |a| a.level())
    }

    /// Whether the player is dead and waits to respawn.
    fn is_dead(&self) -> bool {
        self.health <= 0.0
    }

    /// The largest health: 20 (or what the API set) and 4 for each level of health boost.
    fn max_health(&self) -> f64 {
        let boost = self
            .effects
            .get(&effect::Type::HealthBoost)
            .map_or(0.0, |a| f64::from(a.level()));
        f64::from(self.custom.max_health) + 4.0 * boost
    }

    /// The game mode: the player's own, or the server's. `force-gamemode` applies at the join:
    /// the saved mode is not loaded, and a later change stays until the player leaves.
    fn mode(&self) -> GameMode {
        self.game_mode.unwrap_or(self.default_mode)
    }

    /// Marks attributes or other client state to send at the end of the tick.
    fn mark(&mut self, bits: u16) {
        self.dirty |= bits;
    }
}

#[cfg(test)]
impl Default for Survival {
    fn default() -> Self {
        Self::with_mode(GameMode::Survival)
    }
}

checkpoint::fields!(Survival {
    game_mode,
    default_mode,
    forced_mode,
    switches,
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
    effects,
    fire_ticks,
    air,
    breathing,
    fall_distance,
    spawn,
    death,
    motion,
    no_damage_ticks,
    attack_time,
    last_damage,
    last_cause,
    using,
    cooldowns,
    held_back,
    draws,
    dirty,
    shown,
    custom,
    appearance,
});

/// The survival systems the server settings turn on, fixed at start and kept with each player: the
/// rules read them here, as the public API, which has only the player, must; the API refuses to
/// change a system that is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Switches {
    /// `food`: food, saturation and exhaustion.
    food: bool,
    /// `xp`: points, levels and orbs.
    xp: bool,
    /// `drowning`: air, drowning and suffocation.
    drowning: bool,
    /// `armor`: everything worn armor does against harm: protection and its enchantments
    /// (against damage, burning and explosion knockback), wear and thorns.
    armor: bool,
}

impl Switches {
    /// Every system on, as the default settings have them.
    const ALL: Self = Self {
        food: true,
        xp: true,
        drowning: true,
        armor: true,
    };

    /// Hashes every field (state hash).
    #[cfg(test)]
    fn hash_into(self, h: &mut super::Fnv) {
        let Self {
            food,
            xp,
            drowning,
            armor,
        } = self;
        h.u64(
            u64::from(food) | u64::from(xp) << 1 | u64::from(drowning) << 2 | u64::from(armor) << 3,
        );
    }
}

checkpoint::fields!(Switches {
    food,
    xp,
    drowning,
    armor,
});

/// A position in a world by name (worlds are saved by name).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Point {
    world: String,
    pos: BlockPos,
}

checkpoint::fields!(Point { world, pos });

/// An action [`ask`] refused: a listener cancelled it, or the game the listeners left no longer
/// allows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Refused;

/// A melee hit a player makes on an entity that is not a player, from the attacker's side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Melee {
    /// The damage: the held item's attack points with Strength, Weakness, Sharpness and a
    /// critical hit.
    pub(super) damage: f64,
    /// A critical hit (falling, not flying, not blind, not under water, and not sprinting
    /// under `critical-hits = "sprint"`) that added damage.
    pub(super) critical: bool,
    /// A weapon enchantment added damage.
    pub(super) magic: bool,
}

/// Whether the player a respawn moves stays online or is leaving, its
/// [`PlayerQuit`](crate::event::PlayerQuit) raised: the move of a leaving player raises no
/// [`PlayerWorldChange`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stay {
    /// The player stays.
    Online,
    /// The player leaves once the rule is done.
    Leaving,
}

impl Context<'_> {
    /// Deals `amount` damage for `cause` from `source` to `player` through the game rules, with
    /// its events ([`crate::action::Damage`]); false if no damage was taken.
    pub(crate) fn hurt_player(
        &mut self,
        player: Id,
        amount: f64,
        cause: DamageCause,
        source: DamageSource,
    ) -> bool {
        let damage = Damage::of(cause, source);
        damage::damage_by(self.state, player.session(), amount, damage, source).is_some()
    }

    /// Changes `player`'s game mode to `mode` with its [`PlayerGameModeChange`] event
    /// ([`crate::action::SetGameMode`]); false if it had the mode or a listener cancelled it.
    pub(crate) fn change_game_mode(&mut self, player: Id, mode: GameMode) -> bool {
        let session = player.session();
        let Ok(event) = ask_game_mode(self.state, session, mode) else {
            return false;
        };
        let changed = self.state.players.get_mut(&session).is_some_and(|p| {
            let differs = p.game_mode() != mode;
            p.set_game_mode(mode);
            differs
        });
        watch(self.state, event.as_ref());
        changed
    }
}

/// Reads the player's survival keys from its saved extra (empty for a new player), its items
/// with the server's `custom` items and blocks.
pub(super) fn load(survival: &mut Survival, extra: &Compound, custom: &crate::custom::Registry) {
    saved::load(survival, extra, custom);
}

/// Writes the player's survival keys into the extra to save.
pub(super) fn save(survival: &Survival, extra: &mut Compound) {
    saved::save(survival, extra);
}

/// Applies a `PlayerAuthInput`: rotation, input flags (sneaking, sprinting, swimming, gliding, flying,
/// jumping, missed swings) and the movement, checked. Positions that are not finite or outside
/// the world border are ignored: tick state holds no NaN or infinity.
pub(super) fn on_auth_input(state: &mut State, session: SessionId, packet: &PlayerAuthInput) {
    movement::on_auth_input(state, session, packet);
}

/// Applies a `PlayerAction`: respawn, a jump, or a switch of sprinting, sneaking, gliding, or
/// swimming on or off; any other action does nothing.
pub(super) fn on_player_action(state: &mut State, session: SessionId, packet: &PlayerAction) {
    movement::on_player_action(state, session, packet);
}

/// Answers a `Respawn` from the client: the respawn button on the death screen
/// (`RespawnStateClientReadyToSpawn`) is answered with the respawn position; the respawn itself
/// follows `PlayerAction` respawn.
pub(super) fn on_respawn(state: &mut State, session: SessionId, packet: &Respawn) {
    damage::on_respawn(state, session, packet);
}

/// Applies an `Emote`: a spawned player plays an emote, at most once every 6 ticks, and the players that see
/// it see it too; a [`PlayerEmote`](crate::event::PlayerEmote) listener may cancel it or play
/// another.
pub(super) fn on_emote(state: &mut State, session: SessionId, packet: &Emote) {
    appearance::on_emote(state, session, packet);
}

/// Applies a `PlayerSkin`: a spawned player changes its skin in game: checked like a login skin (image
/// sizes, at most 4 MiB), at most once a second; every player then gets it, or the player gets
/// its old skin back if a [`PlayerChangeSkin`](crate::event::PlayerChangeSkin) listener cancels
/// it.
pub(super) fn on_skin(state: &mut State, session: SessionId, packet: &PlayerSkin) {
    appearance::on_skin(state, session, packet);
}

/// Asks the listeners of `E` whether an action goes ahead ([`dispatch`]): the event is built with
/// `build` only when something listens to or monitors `E`. [`Refused`] if a listener cancelled it
/// or, after a listener ran, `still` no longer holds for the game the listeners left (they may have
/// changed it); otherwise the event, if built, to read the listeners' changes from and to [`watch`]
/// once the action is applied.
fn ask<E: Event>(
    state: &mut State,
    build: impl FnOnce() -> E,
    cancelled: fn(&E) -> bool,
    still: impl FnOnce(&State, &E) -> bool,
) -> Result<Option<E>, Refused> {
    if !dispatch::heard::<E>(state) {
        return Ok(None);
    }
    let mut event = build();
    dispatch::call(state, &mut event);
    if cancelled(&event) || (dispatch::listened::<E>(state) && !still(state, &event)) {
        return Err(Refused);
    }
    Ok(Some(event))
}

/// Calls the monitors of an event [`ask`] built, once its action is applied.
fn watch<E: Event>(state: &mut State, event: Option<&E>) {
    if let Some(event) = event {
        dispatch::watch(state, event);
    }
}

/// Raises [`PlayerWorldChange`] if the player of `session` is no longer in the world `from`, the
/// one it was in before a rule moved it ([`teleport`], which raises nothing). The one place the
/// event is raised: each rule that may move a player into another world calls it once the move
/// and the rest of the rule are done.
pub(super) fn world_changed(state: &mut State, session: SessionId, from: WorldIndex) {
    let Some(to) = state
        .players
        .get(&session)
        .map(|p| p.world)
        .filter(|&w| w != from)
    else {
        return;
    };
    notify(state, || PlayerWorldChange {
        player: Id::new(session),
        old_world: crate::world::Id::of(from),
        new_world: crate::world::Id::of(to),
    });
}

/// Raises an event that only announces what happened (it has nothing to cancel or change):
/// listeners, then monitors, built only when heard.
fn notify<E: Event>(state: &mut State, build: impl FnOnce() -> E) {
    if dispatch::heard::<E>(state) {
        let mut event = build();
        dispatch::call(state, &mut event);
        dispatch::watch(state, &event);
    }
}

/// Keeps the stacks a listener set that can exist: at most `limit` non-empty stacks, each cut to
/// its item's stack size.
pub(crate) fn valid_stacks(stacks: Vec<Stack>, limit: usize) -> Vec<Stack> {
    stacks
        .into_iter()
        .filter_map(|s| {
            let max = s.item_type()?.max_stack_size();
            (!s.is_empty()).then(|| s.with_count(s.count().min(max)))
        })
        .take(limit)
        .collect()
}

/// Asks the [`PlayerGameModeChange`] listeners whether the player of `session` may change to
/// `mode`: [`Refused`] if it already has the mode, if a listener cancels it or if, after the
/// listeners, the player is gone or already has the mode; otherwise the event to [`watch`] once
/// the mode changed.
fn ask_game_mode(
    state: &mut State,
    session: SessionId,
    mode: GameMode,
) -> Result<Option<PlayerGameModeChange>, Refused> {
    let differs = |state: &State| {
        state
            .players
            .get(&session)
            .is_some_and(|p| p.survival.mode() != mode)
    };
    if !differs(state) {
        return Err(Refused);
    }
    ask(
        state,
        || PlayerGameModeChange {
            player: Id::new(session),
            new_game_mode: mode,
            cancelled: false,
        },
        PlayerGameModeChange::is_cancelled,
        |state, _| differs(state),
    )
}

/// Runs one tick of the world clocks, then of every spawned player's survival state, then the
/// client updates.
pub(super) fn tick(state: &mut State) {
    clock::tick(state);
    let mut next = first_session(state);
    while let Some(session) = next {
        player_tick(state, session);
        next = next_session(state, session);
    }
    attributes::flush(state);
}

/// Runs one tick of a spawned, living player.
fn player_tick(state: &mut State, session: SessionId) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if player.phase != Phase::Spawned {
        return;
    }
    let s = &player.survival;
    let is_survival_mode = is_survival(s.mode());
    player.survival.motion.food = s.switches.food && s.custom.food && is_survival_mode;
    if player.survival.is_dead() {
        // Killed through the API: the death's drops come now.
        if !player.survival.motion.death_shown && std::mem::take(&mut player.survival.custom.killed)
        {
            damage::die(state, session, damage::GENERIC_DEATH, None);
        }
        return;
    }
    movement::process(state, session);
    if let Some(player) = state.players.get_mut(&session) {
        let m = &mut player.survival.motion;
        m.in_air_ticks = if m.on_ground {
            0
        } else {
            m.in_air_ticks.saturating_add(1)
        };
    }
    damage::retry_held_back(state, session);
    movement::block_intersections(state, session);
    damage::base_tick(state, session);
    effects::tick(state, session);
    if let Some(player) = state.players.get_mut(&session)
        && player.survival.switches.xp
    {
        food::xp_tick(&mut player.survival);
    }
    damage::living_tick(state, session);
    if is_survival_mode {
        food::tick(state, session);
    }
    food::using_tick(state, session);
    if let Some(player) = state.players.get_mut(&session) {
        movement::end_tick(player);
    }
}

/// Opens a random stream for one draw purpose of a player in a world with `seed`; each call opens
/// a new stream (the player's draw counter is part of the key), so a tick never opens the same
/// stream twice.
pub(super) fn player_random(seed: i64, tick: u64, player: &mut Player, purpose: u64) -> Random {
    let s = &mut player.survival;
    s.draws = s.draws.wrapping_add(1);
    Random::new(
        seed,
        tick,
        random::key(purpose, &[player.runtime_id, s.draws]),
    )
}

/// The seed of the world the player of `session` is in, if the player is there.
fn world_seed(state: &State, session: SessionId) -> Option<i64> {
    state
        .players
        .get(&session)
        .and_then(|p| state.worlds.get(p.world))
        .map(crate::world::World::seed)
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

/// The player's own game mode, `None` when it uses the server default (`StartGame` then names
/// the world's mode, as the official server does for a new player).
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
        reason = "consistency: health is finite; one past the i32 range (a huge maximum health) saturates"
    )]
    let health = player.survival.health.ceil() as i32;
    health
}

/// The player confirmed its spawn: its entity data is what the spawn window showed, and it
/// learns its active effects.
pub(super) fn spawned(player: &mut Player) {
    attributes::spawned(player);
    let packets: Vec<Packet> = effects::all_packets(player).collect();
    for packet in packets {
        player.send_packet(packet);
    }
}

/// Whether the game mode takes damage and gets hungry: survival or adventure.
fn is_survival(mode: GameMode) -> bool {
    matches!(mode, GameMode::Survival | GameMode::Adventure)
}

/// Whether the game mode uses items up: placing, eating, shooting and hitting wear them: every
/// mode but creative.
pub(super) fn has_finite_resources(mode: GameMode) -> bool {
    mode != GameMode::Creative
}

/// Deals damage as [`damage_by`] does, with the source of `cause` (a melee hit's player,
/// otherwise none); the rules call [`damage_by`].
#[cfg(test)]
pub(super) fn damage(state: &mut State, session: SessionId, amount: f64, cause: Damage) -> bool {
    damage::damage(state, session, amount, cause)
}

/// Deals damage as [`damage_by`] does, without a source and without events, for the direct API: a
/// death follows at the player's survival sync.
pub(super) fn damage_quiet(
    state: &mut State,
    session: SessionId,
    amount: f64,
    cause: Damage,
) -> bool {
    damage::damage_quiet(state, session, amount, cause)
}

/// Deals `amount` damage (half hearts) for `cause` from `source` (an entity, a projectile and its
/// owner, a block, or none) after armor, effects and invulnerability, raising
/// [`EntityDamage`](crate::event::EntityDamage), which names the source; a player in `source` is
/// the attacker (its Strength, knockback, thorns). `None` if no damage was taken, otherwise the
/// knockback force and vertical limit the listeners left.
pub(super) fn damage_by(
    state: &mut State,
    session: SessionId,
    amount: f64,
    cause: Damage,
    source: DamageSource,
) -> Option<(f64, f64)> {
    damage::damage_by(state, session, amount, cause, source)
}

/// Adds food exhaustion for `cause`, raising [`PlayerExhaust`](crate::event::PlayerExhaust).
pub(super) fn exhaust_by(state: &mut State, session: SessionId, amount: f64, cause: ExhaustCause) {
    food::exhaust_as(state, session, amount, cause);
}

/// Adds xp points (orbs, trades); the API has [`Player::add_xp`].
#[cfg(test)]
pub(super) fn add_xp(player: &mut Player, amount: u32) {
    food::add_xp(player, amount);
}

/// The xp level of the player.
pub(super) fn xp_level(player: &Player) -> u32 {
    player.survival.xp_level
}

/// Hits the player `target` in melee with the item `attacker` holds.
pub(super) fn attack_player(state: &mut State, attacker: SessionId, target: SessionId) {
    damage::attack_player(state, attacker, target);
}

/// Finishes the eating or drinking of the player who clicks in the air while it eats or drinks the
/// item it holds. False, changing nothing, if it does not eat the held item.
pub(super) fn finish_eating(state: &mut State, session: SessionId) -> bool {
    food::finish_eating(state, session)
}

/// Starts the player eating or drinking the item it holds. True when the item is a consumable it
/// handled.
pub(super) fn start_eating(state: &mut State, session: SessionId) -> bool {
    food::start_eating(state, session)
}

/// Stops the player using its held item: drawing a bow or trident, eating or drinking.
pub(super) fn stop_using_item(player: &mut Player) {
    player.breaking.stop_using();
    player.survival.using = None;
}

/// Handles the player leaving: it stops eating.
pub(super) fn left(player: &mut Player) {
    player.survival.using = None;
}

/// The player's feet position (the tick keeps the eye position): [`feet_at`] its eyes.
pub(super) fn feet(player: &Player) -> Vec3 {
    feet_at(player.position)
}

/// The feet position of a player whose eyes are at `eye`, rounded to 4 decimals: the one rule
/// every feet position the tick and the API use follows. Unrounded, the `f32` eye of a player on
/// a block is a hair below it and its feet floor to the block under that.
pub(super) fn feet_at(eye: Vec3) -> Vec3 {
    let round = |v: f64| (v * 10_000.0).round() / 10_000.0;
    let f = eye - Vec3::new(0.0, EYE_HEIGHT, 0.0);
    Vec3::new(round(f.x), round(f.y), round(f.z))
}

/// The eye position of a player standing at `feet`, in the middle of the block: where a player
/// joins, respawns, or is sent when its world unloads.
pub(super) fn standing_eye(feet: BlockPos) -> Vec3 {
    Vec3::new(
        f64::from(feet.x) + 0.5,
        f64::from(feet.y) + EYE_HEIGHT,
        f64::from(feet.z) + 0.5,
    )
}

/// The player's box: its pose's size around its feet; the one box the survival and the entity
/// rules use.
pub(super) fn bounding_box(player: &Player) -> crate::math::Aabb {
    movement::bounding_box(&player.survival, feet(player))
}

/// Whether `degrees` is an angle the tick keeps: finite as the `f32` packets and saves carry, so
/// no rotation in tick state is infinite (a double past the float range rounds to one).
pub(super) const fn is_float_angle(degrees: f64) -> bool {
    to_f32(degrees).is_finite()
}

/// Queues `packet` for the player of `session` and every player shown it
/// ([`super::entity::send_about_player`]).
fn broadcast_from(state: &mut State, session: SessionId, packet: &Packet) {
    super::entity::send_about_player(state, session, super::entity::Audience::Itself, packet);
}

/// Queues `packet` for the players shown the player of `session`, except that player.
fn broadcast_to_viewers(state: &mut State, session: SessionId, packet: &Packet) {
    super::entity::send_about_player(state, session, super::entity::Audience::Viewers, packet);
}

/// The difficulty the rules of the world at `world` use: its own, or the server's.
fn difficulty(state: &State, world: WorldIndex) -> Difficulty {
    super::world_api::difficulty(state, world)
}

/// Whether the player is alive: not dead and waiting to respawn.
pub(super) fn is_alive(player: &Player) -> bool {
    !player.survival.is_dead()
}

/// Gives the player an effect (potions, food, clouds): false if one of the same type with a
/// higher amplifier, or the same amplifier and a longer duration, stays. A replaced effect is
/// undone first (health boost, absorption), the new one applies, and the client hears
/// `MobEffect` add or modify. An instant effect (healing, harming) acts on the player's next
/// tick.
pub(super) fn add_effect(state: &mut State, session: SessionId, effect: effect::Instance) -> bool {
    effects::add(state, session, effect.effect(), effects::Active::of(effect))
}

/// Moves the player's eyes to `pos` in `world` (ender pearls, chorus fruit, respawns, commands):
/// its window closes, its breaking stops, its fall ends, the client gets `MovePlayer` in
/// teleport mode and the players that see it the same packet.
/// Another world first closes, without an event, a window an `InventoryClose` listener opened
/// (its blocks stand in the old world), closes the view of the old world, opens it around `pos`
/// in the new one (its chunks are sent again from the next view phase) and the client gets the
/// new world's clock. Nothing happens for a position that is not finite or lies outside the
/// world border or a world that is not loaded ([`check_destination`], which the callers that
/// must know ask first), or a player whose data is still loading.
pub(super) fn teleport(state: &mut State, session: SessionId, world: WorldIndex, pos: Vec3) {
    let allowed = check_destination(state, world, pos).is_ok();
    let Some(player) = state.players.get(&session) else {
        return;
    };
    if !allowed || player.phase == Phase::Loading {
        return;
    }
    let other_world = player.world != world;
    inventory::close_window(state, session);
    super::interact::stop_breaking(state, session);
    if other_world {
        inventory::close_window_without_event(state, session);
        switch_world(state, session, world, pos);
    }
    movement::teleport(state, session, pos);
    super::entity::player_moved(state);
    let packet = state.players.get_mut(&session).map(|p| {
        // The teleport shows a rotation the API set too.
        p.survival.custom.turned = false;
        movement::position_packet(p, MoveMode::Teleport)
    });
    if let Some(packet) = packet {
        broadcast_to_viewers(state, session, &packet);
    }
}

/// The check that a player's eyes may be moved to `eye` in `world`: the one destination rule of
/// every teleport (the API, commands, ender pearls, chorus fruit, respawns). It fails for a world
/// that is not loaded (unloaded, or an ID of another server) and a position that is not finite or
/// lies outside the world border.
pub(super) fn check_destination(
    state: &State,
    world: WorldIndex,
    eye: Vec3,
) -> crate::player::Result<()> {
    if !state.catalog.is_loaded(world) {
        return Err(crate::player::Error::WorldNotLoaded);
    }
    if !super::inside_border(eye) {
        return Err(crate::player::Error::OutsideWorld);
    }
    Ok(())
}

/// Shows the player and the players that see it the rotation the API set, as a teleport where it
/// stands (as the official server does for `/tp` with a rotation): `MovePlayer` in teleport
/// mode.
fn send_rotation(state: &mut State, session: SessionId) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    movement::send_position(player, MoveMode::Teleport);
    let packet = movement::position_packet(player, MoveMode::Teleport);
    broadcast_to_viewers(state, session, &packet);
}

/// Moves the player into another world: the old world's chunks are let go, the view opens at
/// `pos` in the new world with the radius the player asked for, up to the new world's
/// `view-distance`, and the client learns the new world's difficulty, weather and clock. The
/// other players see it leave and arrive through entity tracking, which follows the world.
fn switch_world(state: &mut State, session: SessionId, world: WorldIndex, pos: Vec3) {
    let Some(mut player) = state.players.remove(&session) else {
        return;
    };
    // The radius granted, which a request of this tick may have changed before the view follows.
    let radius = if player.view.is_open() {
        player.chunk_radius.unwrap_or(0)
    } else {
        0
    };
    super::view::close(state, &mut player);
    let entered = difficulty(state, world);
    if difficulty(state, player.world) != entered {
        // Entering a world syncs its difficulty; the player has had its `StartGame` (see the
        // weather below).
        player.send_packet(Packet::SetDifficulty(
            crate::protocol::packet::SetDifficulty {
                difficulty: super::spawn::difficulty_id(entered).cast_unsigned(),
            },
        ));
    }
    // The player has had its `StartGame` (a teleport skips one still loading), so its client
    // has the old world's weather, whether its spawn window went out yet or not.
    let weather = |w| state.worlds.get(w).map_or(Weather::Clear, World::weather);
    for packet in super::weather::changes(weather(player.world), weather(world)) {
        player.send_packet(packet);
    }
    state.set_world(&mut player, world);
    player.position = pos;
    player.data_dirty = true;
    // Each world grants the radius the player asked for up to its `view-distance`; the client
    // hears of a change.
    let allowed = player
        .asked_radius
        .map(|asked| super::spawn::granted_radius(&state.settings, world, asked));
    if let Some(allowed) = allowed.filter(|a| Some(*a) != player.chunk_radius) {
        super::spawn::grant_radius(state, &mut player, allowed);
    } else if radius > 0 {
        super::view::set_radius(state, &mut player, radius);
    }
    // Its client has the old world's clock from its join, as it has the weather.
    player.send_packet(clock_state(super::world_api::world_at(state, world)));
    state.players.insert(session, player);
}

/// The level of the haste effect (0 without it), for the break speed.
pub(super) fn haste_level(player: &Player) -> u32 {
    player.survival.effect_level(effect::Type::Haste)
}

/// The level of the mining fatigue effect (0 without it), for the break speed.
pub(super) fn mining_fatigue_level(player: &Player) -> u32 {
    player.survival.effect_level(effect::Type::MiningFatigue)
}

/// Whether the player flies (creative or spectator flight).
pub(super) fn is_flying(player: &Player) -> bool {
    player.survival.motion.flying
}

/// Whether the player stands on something.
pub(super) fn is_on_ground(player: &Player) -> bool {
    player.survival.motion.on_ground
}

/// Whether the player sneaks: a sneaking player places against blocks it would otherwise use.
pub(super) fn is_sneaking(player: &Player) -> bool {
    player.survival.motion.sneaking
}

/// Whether the player holds the sneak key: a click on a block then uses the held item instead of
/// the block.
pub(super) fn is_sneak_pressed(player: &Player) -> bool {
    player.survival.motion.sneak_pressed
}

/// The player's yaw and pitch in degrees, yaw in [0, 360): the facing of placed blocks, the
/// flight of thrown items and arrows.
pub(super) fn rotation(player: &Player) -> (f64, f64) {
    (player.survival.motion.yaw, player.survival.motion.pitch)
}

/// Whether a player with its eyes at `eye`, looking along the unit vector `direction`, reaches
/// `target`: within `max` blocks of the eyes and at most √3/2 behind the plane they look
/// through.
pub(super) fn can_interact(eye: Vec3, direction: Vec3, target: Vec3, max: f64) -> bool {
    eye.distance_squared(target) <= max * max
        && direction.dot(target) - direction.dot(eye) >= -REACH_BEHIND
}

/// The unit vector the player looks along.
pub(super) fn look_direction(player: &Player) -> Vec3 {
    let m = &player.survival.motion;
    super::entity::direction(m.yaw, m.pitch)
}

/// Whether the player's eyes are under water, for the break speed.
pub(super) fn is_underwater(state: &State, player: &Player) -> bool {
    movement::is_underwater(state, player)
}

/// Whether `item`'s cooldown still runs for the player: ender pearls, chorus fruit, goat horns.
pub(super) fn has_item_cooldown(state: &State, player: &Player, item: crate::item::Type) -> bool {
    food::has_cooldown(state.tick, &player.survival, item)
}

/// Starts `item`'s cooldown after a use and tells the client; nothing for an item without one.
pub(super) fn reset_item_cooldown(state: &mut State, session: SessionId, item: crate::item::Type) {
    let tick = state.tick;
    if let Some(player) = state.players.get_mut(&session) {
        food::reset_cooldown(tick, player, item);
    }
}

/// Picks up an xp orb worth `amount` points for the player: false, and the orb stays, while the
/// pickup cooldown of 2 ticks runs. Mending on a damaged held, off-hand or worn item (one picked
/// at random) repairs 2 durability per point first; the rest becomes xp, with the collect or
/// level-up sound.
pub(super) fn pickup_xp(state: &mut State, session: SessionId, amount: u32) -> bool {
    food::pickup_xp(state, session, amount)
}

/// What an instant health or damage effect of `amplifier` heals or hurts at full potency, for
/// players and entities alike ([`effects::instant_amount`]).
pub(super) fn instant_amount(effect: effect::Type, amplifier: u8) -> f64 {
    effects::instant_amount(effect, amplifier)
}

/// Heals the player by `amount` (a splash potion of healing, weaker with the distance), raising
/// [`EntityRegainHealth`](crate::event::EntityRegainHealth) for magic; nothing for a dead player
/// or one at full health.
pub(super) fn heal(state: &mut State, session: SessionId, amount: f64) {
    damage::heal(state, session, amount, RegainReason::Magic);
}

/// Sets the player on fire for `seconds` by `source` (a burning arrow), raising
/// [`EntityCombust`](crate::event::EntityCombust), which names the source; a longer burn already
/// running stays.
pub(super) fn set_on_fire_by(
    state: &mut State,
    session: SessionId,
    seconds: u32,
    source: CombustSource,
) {
    damage::combust(state, session, seconds, source);
}

/// Whether the player's hit cooldown runs: a hit now only counts what it deals beyond the last
/// one and does not knock the player back.
pub(super) fn has_attack_cooldown(player: &Player) -> bool {
    player.survival.attack_time > 0
}

/// The melee hit the player of `session` makes now; `None` if it cannot hit (not spawned, dead
/// or a spectator).
pub(super) fn melee(state: &State, session: SessionId) -> Option<Melee> {
    damage::melee(state, session)
}

/// Applies the attacker's side of a melee hit the player of `session` landed with `held`:
/// `enchant` applies the weapon's Knockback force and Fire Aspect seconds to the target, then a
/// living attacker's weapon wears and it gets hungrier.
pub(super) fn after_melee(
    state: &mut State,
    session: SessionId,
    held: &Stack,
    enchant: impl FnOnce(&mut State, Option<f64>, Option<u32>),
) {
    damage::after_melee(state, session, held, enchant);
}

/// Shows the player's arm swing to the players that see it.
pub(super) fn swing(state: &mut State, session: SessionId) {
    damage::swing(state, session);
}

/// Plays a melee hit's sound at `at` in the world of `session`.
pub(super) fn hit_sound(
    state: &mut State,
    session: SessionId,
    at: Vec3,
    sound: crate::world::Sound,
) {
    sound::hit(state, session, at, sound);
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::mpsc;

    use super::effects::Active;
    use crate::block::{self, PropertyValue};
    use crate::config::Config;
    use crate::event::Listeners;
    use crate::item::{Enchantment, Extra, Kind as ItemKind, Stack};
    use crate::protocol::consts::{
        ActorEvent, AnimateAction, EntityDataKey, InputFlag, LevelEvent, MobEffectOperation,
        MoveMode, PlayerAction as Action, RespawnState,
    };
    use crate::protocol::packet::{self as p, Packet};
    use crate::protocol::{
        FlagSet, InputFlags, MetadataValue, PersonaPiece, PersonaPieceTintColour, Rgba,
        SkinAnimation,
    };
    use crate::save::{self, Request, Response, WorldSpec};
    use crate::tick::interact::drops;
    use crate::tick::queue::{Input, Outbound};
    use crate::tick::tests::{
        initialised, input, join, leave, new_state, packet, radius, received, settings, tick,
    };
    use crate::tick::{Output, inventory};

    /// The official server's player flags (P6 `craft`): the spawn window's, and with Breathing
    /// (bit 35).
    const FLAGS: i64 = 844_424_930_656_256;
    /// [`FLAGS`] with Breathing.
    const FLAGS_BREATHING: i64 = FLAGS | 1 << 35;

    fn id(packet: &Packet) -> u32 {
        packet.id()
    }

    #[test]
    fn the_spawn_window_has_the_official_order() {
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
        // The official server's spawn window (P6) without the spawn sound Anchor never sends.
        let property = id(&Packet::SyncActorProperty(p::SyncActorProperty::default()));
        let mut want = vec![
            id(&Packet::PlayerList(p::PlayerList::default())),
            id(&Packet::SyncWorldClocks(p::SyncWorldClocks::default())),
            id(&Packet::JigsawStructureData(
                p::JigsawStructureData::default(),
            )),
            id(&Packet::VoxelShapes(p::VoxelShapes::default())),
            id(&Packet::StartGame(Box::default())),
        ];
        want.extend([property; 13]);
        want.extend([
            id(&Packet::ItemRegistry(p::ItemRegistry::default())),
            id(&Packet::SetSpawnPosition(p::SetSpawnPosition::default())),
            id(&Packet::SyncWorldClocks(p::SyncWorldClocks::default())),
            id(&Packet::SetDifficulty(p::SetDifficulty::default())),
            id(&Packet::SetCommandsEnabled(p::SetCommandsEnabled::default())),
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
            id(&Packet::CameraPresets(p::CameraPresets::default())),
            id(&Packet::CameraAimAssistPresets(
                p::CameraAimAssistPresets::default(),
            )),
            id(&Packet::CameraSpline(p::CameraSpline::default())),
            id(&Packet::UpdateAttributes(p::UpdateAttributes::default())),
            id(&Packet::CreativeContent(p::CreativeContent::default())),
            id(&Packet::TrimData(p::TrimData::default())),
            id(&Packet::InventoryContent(Box::default())),
            id(&Packet::InventoryContent(Box::default())),
            id(&Packet::InventoryContent(Box::default())),
            id(&Packet::InventoryContent(Box::default())),
            id(&Packet::PlayerHotBar(p::PlayerHotBar::default())),
            id(&Packet::CraftingData(Box::default())),
            id(&Packet::AvailableCommands(Box::default())),
            id(&Packet::Respawn(p::Respawn::default())),
            id(&Packet::Respawn(p::Respawn::default())),
            id(&Packet::Respawn(p::Respawn::default())),
            id(&Packet::SetActorData(p::SetActorData::default())),
            id(&Packet::CurrentStructureFeature(
                p::CurrentStructureFeature::default(),
            )),
            id(&Packet::UpdateAttributes(p::UpdateAttributes::default())),
            id(&Packet::ChunkRadiusUpdated(p::ChunkRadiusUpdated::default())),
            id(&Packet::SetHealth(p::SetHealth::default())),
            id(&Packet::PlayStatus(p::PlayStatus::default())),
        ]);
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
    fn the_spawn_window_has_the_official_player_list_and_clocks() {
        let packets = window();
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
    fn the_spawn_window_has_the_official_settings_abilities_and_rules() {
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

    /// The encoded payload size of a packet (without the packet header).
    fn size(packet: &Packet) -> usize {
        let mut bytes = Vec::new();
        packet.encode(&mut bytes).unwrap();
        bytes.len()
    }

    #[test]
    fn the_spawn_window_has_the_official_player_state() {
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
        assert_eq!(metadata.len(), 73);
        assert_eq!(metadata[&0], MetadataValue::Int64(844_424_930_656_256));
        assert_eq!(metadata[&92], MetadataValue::Int64(35_184_405_643_264));
        assert_eq!(metadata[&7], MetadataValue::Int16(300));
        assert_eq!(metadata[&130], MetadataValue::Vec3([0.6, 1.8, 0.0]));
        assert_eq!(metadata[&4], MetadataValue::String("player1".to_owned()));
        // The official server's types of keys 136 to 141 (P6 `others` data types); 139 is not
        // sent.
        assert_eq!(metadata[&136], MetadataValue::Int32(-1));
        assert_eq!(metadata[&137], MetadataValue::Int32(-1));
        assert_eq!(metadata[&138], MetadataValue::Int32(-1));
        assert_eq!(metadata[&140], MetadataValue::Float32(64.0));
        assert_eq!(metadata[&141], MetadataValue::Byte(0));
        // The official server sends the last death position in block coordinates and the bed
        // entry as floats.
        assert_eq!(metadata[&127], MetadataValue::BlockPos([0, 0, 0]));
        assert_eq!(metadata[&133], MetadataValue::Vec3([0.0, 0.0, 0.0]));
        // Its SetActorData for "AnchorBot" is 400 payload bytes (P6); this name is 2 shorter.
        assert_eq!(size(&Packet::SetActorData(data[0].clone())), 400 - 2);
        assert!(packets.contains(&Packet::SetHealth(p::SetHealth { health: 20 })));
    }

    /// The entity data session 1's own player (runtime ID 1) gets in `packets`.
    fn own_data(packets: &[Packet]) -> Vec<&crate::protocol::EntityMetadata> {
        packets
            .iter()
            .filter_map(|p| match p {
                Packet::SetActorData(d) if d.entity_runtime_id == 1 => Some(&d.entity_metadata),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_breathing_flag_comes_with_the_first_tick_after_the_spawn_window() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        let (joined, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![joined, radius(1, 1, 8)], &mut output);
        let window = received(&mut rx).0;
        assert_eq!(own_data(&window)[0][&0], MetadataValue::Int64(FLAGS));
        tick(&mut state, &mut vec![initialised(1, 2, 1)], &mut output);
        let after = received(&mut rx).0;
        let want =
            crate::protocol::EntityMetadata::from([(0, MetadataValue::Int64(FLAGS_BREATHING))]);
        assert_eq!(own_data(&after), [&want]);
    }

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
        fn send(&mut self, s: u64, sent: Packet) -> Input {
            self.seq += 1;
            input(s, self.seq, packet(sent))
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

        fn survival_mut(&mut self, s: u64) -> &mut Survival {
            &mut self.player_mut(s).survival
        }

        fn hurt(&mut self, s: u64, amount: f64, cause: Damage) -> bool {
            damage(&mut self.state, SessionId::new(s), amount, cause)
        }
    }

    /// The hit cooldown counts down after drowning hurts: its last tick still weighs a drowning
    /// hit against the last hit, and a drowning hit's own cooldown starts counting the same tick.
    #[test]
    fn drowning_weighs_against_the_last_hit_before_the_cooldown_counts_down() {
        let mut server = Server::new(1, |_| {});
        let eye = BlockPos::containing(server.player(1).position);
        server.state.worlds[0]
            .set_block(eye, block::Kind::Water.default_state())
            .unwrap();
        let s = server.survival_mut(1);
        s.air = -19;
        s.attack_time = 1;
        s.last_damage = Some(7.0);
        server.idle(1);
        let s = server.survival_mut(1);
        assert_eq!((s.health, s.attack_time), (20.0, 0));
        s.air = -19;
        server.idle(1);
        let s = server.survival_mut(1);
        let cooldown = u32::from(crate::tick::entity::ATTACK_COOLDOWN_TICKS);
        assert_eq!((s.health, s.attack_time), (18.0, cooldown - 1));
    }

    #[test]
    fn under_water_the_air_drops_and_out_of_it_refills() {
        let mut server = Server::new(1, |_| {});
        let eye = BlockPos::containing(server.player(1).position);
        server.state.worlds[0]
            .set_block(eye, block::Kind::Water.default_state())
            .unwrap();
        server.take(1);
        server.idle(1);
        let got = server.take(1);
        let want = crate::protocol::EntityMetadata::from([
            (0, MetadataValue::Int64(FLAGS)),
            (7, MetadataValue::Int16(299)),
        ]);
        assert_eq!(
            own_data(&got),
            [&want],
            "out of breath with the first air lost"
        );
        server.idle(318);
        assert_eq!(server.survival_mut(1).air, -19);
        assert_eq!(server.survival_mut(1).health, 20.0);
        server.idle(1);
        let s = server.survival_mut(1);
        assert_eq!((s.air, s.health), (0, 18.0), "drowning at -20 deals 2");
        server.state.worlds[0]
            .set_block(eye, block::Kind::Air.default_state())
            .unwrap();
        server.take(1);
        server.idle(1);
        let got = server.take(1);
        let want = crate::protocol::EntityMetadata::from([
            (0, MetadataValue::Int64(FLAGS_BREATHING)),
            (7, MetadataValue::Int16(4)),
        ]);
        assert_eq!(
            own_data(&got),
            [&want],
            "breathing at once, 4 air back a tick"
        );
        server.idle(80);
        assert_eq!(server.survival_mut(1).air, 300);
    }

    #[test]
    fn spawn_window_packets_have_the_official_sizes() {
        // Payload sizes of the official server's packets (P6 capture sizes minus the packet
        // header).
        let server = Server::new(1, |_| {});
        let settings = &server.state.settings;
        assert_eq!(size(&game_rules(settings)), 749);
        assert_eq!(size(&all_attributes(server.player(1))), 840);
        assert_eq!(size(&adventure_settings()), 5);
        let mut state = server.state;
        state.worlds[0].set_time(26);
        assert_eq!(size(&clock_state(&state.worlds[0])), 13);
        assert_eq!(size(&clock_definition(&state.worlds[0])), 236);
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

    fn sid(s: u64) -> SessionId {
        SessionId::new(s)
    }

    fn stack(kind: ItemKind, count: u8) -> Stack {
        Stack::new(kind.default_type(), count)
    }

    fn enchanted(kind: ItemKind, enchantments: &[(Enchantment, u16)]) -> Stack {
        stack(kind, 1)
            .with_extra(Some(Extra {
                enchantments: enchantments.to_vec(),
                ..Extra::default()
            }))
            .unwrap()
    }

    #[test]
    fn armor_resistance_and_protection_reduce_damage() {
        let mut server = Server::new(1, |_| {});
        let chest = stack(ItemKind::DiamondChestplate, 1);
        let points = f64::from(chest.item_type().unwrap().armor_points());
        inventory::set_armor(server.player_mut(1), 1, chest);
        assert!(server.hurt(1, 10.0, Damage::Contact));
        let want = 20.0 - 10.0 * (1.0 - points * 0.04);
        assert!((server.survival_mut(1).health - want).abs() < 1e-9);
        // Not reducible by armor: the whole hit lands.
        server.idle(10);
        server.survival_mut(1).health = 20.0;
        assert!(server.hurt(1, 4.0, Damage::Fall));
        assert_eq!(server.survival_mut(1).health, 16.0);
        // Resistance II takes 40 % of what armor left.
        server.idle(10);
        server.survival_mut(1).health = 20.0;
        effects::add(
            &mut server.state,
            sid(1),
            effect::Type::Resistance,
            Active::new(1000, 1),
        );
        assert!(server.hurt(1, 10.0, Damage::Contact));
        let want = 20.0 - 10.0 * (1.0 - points * 0.04) * 0.6;
        assert!((server.survival_mut(1).health - want).abs() < 1e-9);
        // Protection IV on the chestplate: factor 5, scaled by 50 to 100 % and rounded up.
        effects::remove(&mut server.state, sid(1), effect::Type::Resistance);
        let protected = enchanted(ItemKind::DiamondChestplate, &[(Enchantment::Protection, 4)]);
        inventory::set_armor(server.player_mut(1), 1, protected);
        server.idle(10);
        server.survival_mut(1).health = 20.0;
        assert!(server.hurt(1, 10.0, Damage::Magic));
        let taken = 20.0 - server.survival_mut(1).health;
        assert!((10.0 * 0.8..=10.0 * 0.88).contains(&taken), "{taken}");
    }

    #[test]
    fn blast_protection_counts_against_block_and_entity_explosions() {
        let taken = |cause: Damage, level: u16| {
            let mut server = Server::new(1, |_| {});
            let boots = enchanted(
                ItemKind::DiamondBoots,
                &[(Enchantment::BlastProtection, level)],
            );
            inventory::set_armor(server.player_mut(1), 3, boots);
            assert!(server.hurt(1, 10.0, cause));
            20.0 - server.survival_mut(1).health
        };
        let entity = taken(Damage::Explosion, 4);
        assert!(entity < taken(Damage::Explosion, 1), "{entity}");
        assert_eq!(taken(Damage::BlockExplosion, 4), entity);
    }

    /// Fire protection IV cuts a 10-second burn to 4 seconds, and with `armor = false` to
    /// nothing; nor does blast protection count then.
    #[test]
    fn armor_off_turns_off_fire_and_blast_protection() {
        for (armor, burn_ticks) in [(true, 80), (false, 200)] {
            let mut server = Server::new(1, |c| c.general.armor = armor);
            let chest = enchanted(
                ItemKind::DiamondChestplate,
                &[(Enchantment::FireProtection, 4)],
            );
            inventory::set_armor(server.player_mut(1), 1, chest);
            let boots = enchanted(ItemKind::DiamondBoots, &[(Enchantment::BlastProtection, 4)]);
            inventory::set_armor(server.player_mut(1), 3, boots);
            assert!(damage::ignite(server.player_mut(1), 10));
            assert_eq!(server.survival_mut(1).fire_ticks, burn_ticks);
            let blast = protection_level(server.player(1), Enchantment::BlastProtection);
            assert_eq!(blast, if armor { 4 } else { 0 });
        }
    }

    #[test]
    fn protection_factors_follow_the_formula() {
        assert_eq!(damage::epf(4, 0.75), 5);
        assert_eq!(damage::epf(1, 0.75), 1);
        assert_eq!(damage::epf(4, 1.25), 9);
        assert_eq!(damage::epf(4, 2.5), 18);
        assert_eq!(damage::epf(4, 1.5), 11);
        assert_eq!(damage::epf(255, 2.5), 54_192);
        assert_eq!(
            damage::epf(crate::item::MAX_ENCHANTMENT_LEVEL, 2.5),
            894_730_245
        );
    }

    #[test]
    fn fall_damage_follows_the_formula() {
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
        assert_eq!(server.survival_mut(1).health, 14.0);
        server.idle(10);
        assert!(server.hurt(1, 1.0, Damage::Contact));
        assert_eq!(server.survival_mut(1).health, 13.0);
    }

    #[test]
    fn creative_players_respawned_players_and_fire_resistance_take_no_damage() {
        let mut server = Server::new(1, |c| c.general.gamemode = GameMode::Creative);
        assert!(!server.hurt(1, 5.0, Damage::Magic));
        let mut server = Server::new(1, |_| {});
        server.survival_mut(1).no_damage_ticks = 5;
        assert!(!server.hurt(1, 5.0, Damage::Magic));
        server.survival_mut(1).no_damage_ticks = 0;
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

    /// A player the fire kills stops its effects in the same tick: they tick only while alive.
    #[test]
    fn effects_do_not_tick_on_a_player_that_just_died() {
        let mut server = Server::new(1, |_| {});
        let speed = Active::new(100, 0);
        effects::add(&mut server.state, sid(1), effect::Type::Speed, speed);
        server.survival_mut(1).health = 1.0;
        server.survival_mut(1).fire_ticks = 21;
        server.idle(1);
        assert!(!is_alive(server.player(1)));
        let left = server
            .survival_mut(1)
            .effects
            .get(&effect::Type::Speed)
            .copied();
        assert_eq!(left, Some(speed));
    }

    /// A player shot dead sees who shot it; a projectile no player shot is a generic death.
    #[test]
    fn a_death_by_arrow_names_the_shooter() {
        let mut server = Server::new(2, |_| {});
        let shot = |owner| DamageSource::Child {
            child: crate::entity::Id::new(0, 0),
            owner,
        };
        let by_two = shot(Some(crate::event::Actor::Player(Id::new(sid(2)))));
        assert!(
            damage::damage_by(&mut server.state, sid(1), 100.0, Damage::Entity, by_two).is_some()
        );
        server.idle(1);
        let shooter = server.player(2).identity.name.clone();
        let deaths = |packets: Vec<Packet>| -> Vec<(String, Vec<String>)> {
            packets
                .into_iter()
                .filter_map(|p| match p {
                    Packet::DeathInfo(d) => Some((d.cause, d.messages)),
                    _ => None,
                })
                .collect()
        };
        let death = deaths(server.take(1));
        assert_eq!(death.len(), 1);
        assert_eq!(death[0].0, "death.attack.arrow");
        assert_eq!(death[0].1.get(1), Some(&shooter));
        assert!(
            damage::damage_by(&mut server.state, sid(2), 100.0, Damage::Entity, shot(None))
                .is_some()
        );
        server.idle(1);
        let death = deaths(server.take(2));
        assert_eq!(death.len(), 1);
        assert_eq!(death[0].0, damage::GENERIC_DEATH);
    }

    /// A trident's kill names the trident, not an arrow.
    #[test]
    fn a_death_by_trident_says_trident() {
        let mut server = Server::new(2, |_| {});
        let world = crate::world::Id::DEFAULT;
        let at = server.player(1).position + Vec3::new(0.0, 5.0, 0.0);
        let trident = Context {
            state: &mut server.state,
        }
        .spawn_trident(world, at, stack(ItemKind::Trident, 1), Vec3::ZERO)
        .unwrap();
        let by_two = DamageSource::Child {
            child: trident,
            owner: Some(crate::event::Actor::Player(Id::new(sid(2)))),
        };
        assert!(
            damage::damage_by(&mut server.state, sid(1), 100.0, Damage::Entity, by_two).is_some()
        );
        server.idle(1);
        let died = server
            .take(1)
            .into_iter()
            .any(|p| matches!(p, Packet::DeathInfo(d) if d.cause == "death.attack.trident"));
        assert!(died);
    }

    /// A player that may fly takes no fall damage, flying or not.
    #[test]
    fn a_player_that_may_fly_takes_no_fall_damage() {
        let mut server = Server::new(1, |_| {});
        server.player_mut(1).set_can_fly(true);
        assert!(!server.hurt(1, 4.0, Damage::Fall));
        assert!(server.hurt(1, 4.0, Damage::Contact));
    }

    /// The respawn invulnerability counts down after the void and the fire hurt: its last tick
    /// still keeps them off.
    #[test]
    fn the_last_invulnerable_tick_keeps_the_fire_off() {
        let mut server = Server::new(1, |_| {});
        server.survival_mut(1).no_damage_ticks = 1;
        server.survival_mut(1).fire_ticks = 21;
        damage::base_tick(&mut server.state, sid(1));
        assert_eq!(server.survival_mut(1).health, 20.0);
        assert_eq!(server.survival_mut(1).no_damage_ticks, 0);
    }

    /// Puts player 2 one block south of player 1, facing it, and returns the server.
    fn duel(edit: impl FnOnce(&mut Config)) -> Server {
        let mut server = Server::new(2, edit);
        let base = server.player(1).position;
        server.player_mut(2).position = base + Vec3::new(0.0, 0.0, 1.0);
        server.survival_mut(1).motion.yaw = 0.0;
        server.idle(1);
        server.take(1);
        server.take(2);
        server
    }

    /// The hurt event's data is the official server's damage cause number (P6: drowning 9 in
    /// `craft`, an attack 2 in `others`, lightning 24 in `weather`).
    #[test]
    fn hurt_events_carry_the_official_damage_cause() {
        let mut server = duel(|_| {});
        let hurt = |packets: &[Packet]| {
            packets.iter().find_map(|p| match p {
                Packet::ActorEvent(e) if e.event_type == u8::from(ActorEvent::Hurt) => {
                    Some(e.event_data)
                }
                _ => None,
            })
        };
        attack_player(&mut server.state, sid(1), sid(2));
        server.idle(1);
        assert_eq!(hurt(&server.take(2)), Some(2), "an attack");
        for (cause, number) in [
            (Damage::Contact, 1),
            (Damage::Entity, 3),
            (Damage::Suffocation, 4),
            (Damage::Fall, 5),
            (Damage::Fire, 6),
            (Damage::FireTick, 7),
            (Damage::Lava, 8),
            (Damage::Drowning, 9),
            (Damage::BlockExplosion, 10),
            (Damage::Explosion, 11),
            (Damage::Void, 12),
            (Damage::Magic, 14),
            (Damage::Starvation, 16),
            (Damage::FallingBlock, 19),
            (Damage::Lightning, 24),
            (Damage::Custom, 0),
        ] {
            server.idle(20);
            server.survival_mut(2).health = 20.0;
            server.take(2);
            assert!(damage(&mut server.state, sid(2), 1.0, cause), "{cause:?}");
            server.idle(1);
            assert_eq!(hurt(&server.take(2)), Some(number), "{cause:?}");
        }
    }

    #[test]
    fn melee_hits_deal_the_weapon_damage_with_criticals_and_knock_back() {
        let mut server = duel(|_| {});
        let sword = stack(ItemKind::DiamondSword, 1);
        let points = f64::from(sword.item_type().unwrap().attack_points());
        inventory::set_held(server.player_mut(1), sword);
        attack_player(&mut server.state, sid(1), sid(2));
        assert_eq!(server.survival_mut(2).health, 20.0 - points);
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
        server.survival_mut(2).health = 20.0;
        server.survival_mut(1).fall_distance = 1.0;
        attack_player(&mut server.state, sid(1), sid(2));
        assert_eq!(server.survival_mut(2).health, 20.0 - points * 1.5);
        // Sharpness I adds 1.
        server.idle(10);
        server.survival_mut(2).health = 20.0;
        server.survival_mut(1).fall_distance = 0.0;
        let sharp = enchanted(ItemKind::DiamondSword, &[(Enchantment::Sharpness, 1)]);
        inventory::set_held(server.player_mut(1), sharp);
        attack_player(&mut server.state, sid(1), sid(2));
        assert_eq!(server.survival_mut(2).health, 20.0 - points - 1.0);
    }

    /// An attack on a dead player does nothing: no swing, no sound.
    #[test]
    fn attacking_a_dead_player_neither_swings_nor_sounds() {
        let mut server = duel(|_| {});
        assert!(server.hurt(2, 100.0, Damage::Magic));
        server.idle(1);
        server.take(1);
        server.take(2);
        attack_player(&mut server.state, sid(1), sid(2));
        server.idle(1);
        assert!(sounds(&server.take(1)).is_empty(), "no sound");
        let attacking = u8::from(ActorEvent::StartAttacking);
        let swung = server
            .take(2)
            .iter()
            .any(|p| matches!(p, Packet::ActorEvent(e) if e.event_type == attacking));
        assert!(!swung, "no swing");
    }

    /// The critical hit animation shows only when the critical hit added damage: Weakness V takes
    /// a bare hand's point away, so half of nothing is nothing.
    #[test]
    fn a_critical_hit_that_adds_nothing_shows_no_animation() {
        let mut server = duel(|_| {});
        effects::add(
            &mut server.state,
            sid(1),
            effect::Type::Weakness,
            Active::new(100, 4),
        );
        server.survival_mut(1).fall_distance = 1.0;
        server.idle(1);
        server.take(1);
        attack_player(&mut server.state, sid(1), sid(2));
        server.idle(1);
        let critical = u8::from(AnimateAction::CriticalHit);
        let shown = |p: &Packet| matches!(p, Packet::Animate(a) if a.action_type == critical);
        assert!(!server.take(1).iter().any(shown));
    }

    /// A falling hit with a Sharpness sword shows the critical and the enchanted stars with
    /// their particle counts: the client draws none for a count of 0.
    #[test]
    fn critical_and_enchanted_hits_show_their_stars() {
        let mut server = duel(|_| {});
        let sharp = enchanted(ItemKind::DiamondSword, &[(Enchantment::Sharpness, 1)]);
        inventory::set_held(server.player_mut(1), sharp);
        server.survival_mut(1).fall_distance = 1.0;
        server.idle(1);
        server.take(1);
        attack_player(&mut server.state, sid(1), sid(2));
        server.idle(1);
        let stars: Vec<(u8, f32)> = server
            .take(1)
            .iter()
            .filter_map(|p| match p {
                Packet::Animate(a) => Some((a.action_type, a.data)),
                _ => None,
            })
            .collect();
        assert_eq!(
            stars,
            [
                (u8::from(AnimateAction::CriticalHit), 55.0),
                (u8::from(AnimateAction::MagicCriticalHit), 15.0),
            ]
        );
    }

    /// `durability = false`: a hit wears neither the weapon nor the armor, which still
    /// protects; with it both wear.
    #[test]
    fn durability_off_wears_neither_weapons_nor_armor() {
        for durability in [true, false] {
            let mut server = duel(|c| c.general.durability = durability);
            inventory::set_held(server.player_mut(1), stack(ItemKind::DiamondSword, 1));
            inventory::set_armor(
                server.player_mut(2),
                1,
                stack(ItemKind::DiamondChestplate, 1),
            );
            attack_player(&mut server.state, sid(1), sid(2));
            assert!(
                server.survival_mut(2).health > 20.0 - 8.0,
                "the armor protects"
            );
            let worn = (
                server.player(1).inventory.held().damage(),
                server.player(2).inventory.armor()[1].damage(),
            );
            let want = if durability { (1, 2) } else { (0, 0) };
            assert_eq!(worn, want, "durability {durability}");
        }
    }

    /// A melee hit wears a trident by 1.
    #[test]
    fn a_trident_hit_on_a_player_wears_the_trident_by_one() {
        let mut server = duel(|_| {});
        inventory::set_held(server.player_mut(1), stack(ItemKind::Trident, 1));
        attack_player(&mut server.state, sid(1), sid(2));
        assert!(server.survival_mut(2).health < 20.0, "the hit landed");
        assert_eq!(server.player(1).inventory.held().damage(), 1);
    }

    #[test]
    fn attacks_need_pvp_reach_and_a_living_target() {
        let mut server = duel(|c| c.general.pvp = false);
        attack_player(&mut server.state, sid(1), sid(2));
        assert_eq!(server.survival_mut(2).health, 20.0);
        let mut server = duel(|_| {});
        let far = server.player(1).position + Vec3::new(0.0, 0.0, 9.0);
        server.player_mut(2).position = far;
        attack_player(&mut server.state, sid(1), sid(2));
        assert_eq!(server.survival_mut(2).health, 20.0);
        let behind = server.player(1).position + Vec3::new(0.0, 0.0, -2.0);
        server.player_mut(2).position = behind;
        attack_player(&mut server.state, sid(1), sid(2));
        assert_eq!(server.survival_mut(2).health, 20.0);
        attack_player(&mut server.state, sid(1), sid(1));
        assert_eq!(server.survival_mut(1).health, 20.0);
    }

    #[test]
    fn a_full_bar_heals_every_80_ticks_for_6_exhaustion() {
        let mut server = Server::new(1, |_| {});
        server.survival_mut(1).health = 10.0;
        let timer = server.survival_mut(1).food_timer;
        server.idle(80 - timer % 80);
        let s = server.survival_mut(1);
        assert_eq!(s.health, 11.0);
        assert_eq!((s.saturation, s.exhaustion), (4.0, 2.0));
    }

    #[test]
    fn an_empty_bar_starves_down_to_the_difficulty_floor() {
        let mut server = Server::new(1, |_| {});
        server.survival_mut(1).food = 0.0;
        server.survival_mut(1).saturation = 0.0;
        server.survival_mut(1).food_timer = 0;
        server.idle(80);
        assert_eq!(server.survival_mut(1).health, 19.0);
        server.survival_mut(1).health = 1.0;
        server.idle(80);
        assert_eq!(server.survival_mut(1).health, 1.0, "normal stops at 1");
        let mut easy = Server::new(1, |c| c.general.difficulty = Difficulty::Easy);
        easy.survival_mut(1).food = 0.0;
        easy.survival_mut(1).saturation = 0.0;
        easy.survival_mut(1).health = 10.0;
        easy.survival_mut(1).food_timer = 0;
        easy.idle(160);
        assert_eq!(easy.survival_mut(1).health, 10.0, "easy stops at 10");
    }

    #[test]
    fn peaceful_refills_food_and_health() {
        let mut server = Server::new(1, |c| c.general.difficulty = Difficulty::Peaceful);
        server.survival_mut(1).food = 10.0;
        server.survival_mut(1).health = 10.0;
        server.survival_mut(1).food_timer = 0;
        server.idle(20);
        let s = server.survival_mut(1);
        assert_eq!(s.food, 12.0);
        assert_eq!(s.health, 11.0);
    }

    /// Peaceful fills food up to 20, not past it.
    #[test]
    fn peaceful_fills_food_no_higher_than_twenty() {
        let mut server = Server::new(1, |c| c.general.difficulty = Difficulty::Peaceful);
        server.player_mut(1).set_food(19.5).unwrap();
        server.survival_mut(1).food_timer = 9;
        server.idle(1);
        assert_eq!(server.survival_mut(1).food, MAX_FOOD);
    }

    #[test]
    fn food_off_freezes_food_and_exhaustion() {
        let mut server = Server::new(1, |c| c.general.food = false);
        server.survival_mut(1).food = 10.0;
        server.idle(200);
        food::exhaust(server.survival_mut(1), 40.0);
        let s = server.survival_mut(1);
        assert_eq!((s.food, s.exhaustion, s.saturation), (10.0, 0.0, 5.0));
    }

    /// With food off a low or empty bar heals as a full one, every 80 ticks, for no exhaustion,
    /// and never starves.
    #[test]
    fn food_off_heals_as_a_full_bar_for_nothing() {
        for food in [10.0, 0.0] {
            let mut server = Server::new(1, |c| c.general.food = false);
            let s = server.survival_mut(1);
            (s.food, s.health, s.food_timer) = (food, 10.0, 0);
            server.idle(80);
            let s = server.survival_mut(1);
            assert_eq!(
                (s.health, s.food, s.saturation, s.exhaustion),
                (11.0, food, DEFAULT_SATURATION, 0.0)
            );
        }
    }

    #[test]
    fn sprinting_and_jumping_exhaust() {
        let mut server = Server::new(1, |_| {});
        let start = server.player(1).position;
        let flags: FlagSet<InputFlag> = [InputFlag::StartSprinting, InputFlag::StartJumping]
            .into_iter()
            .collect();
        let auth = p::PlayerAuthInput {
            position: (start + Vec3::new(2.0, 0.0, 0.0)).to_f32(),
            input_data: InputFlags::from_set(&flags),
            ..p::PlayerAuthInput::default()
        };
        let input = server.send(1, Packet::PlayerAuthInput(Box::new(auth)));
        server.run(vec![input]);
        let s = server.survival_mut(1);
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
            ..p::PlayerAuthInput::default()
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
            ..p::PlayerAuthInput::default()
        };
        let input = server.send(1, Packet::PlayerAuthInput(Box::new(near)));
        server.run(vec![input]);
        assert_eq!(server.player(1).position, start);
        server.survival_mut(1).motion.force_sync = false;
        let up = p::PlayerAuthInput {
            position: (start + Vec3::new(0.0, 5.0, 0.0)).to_f32(),
            tick: 1,
            ..p::PlayerAuthInput::default()
        };
        let input = server.send(1, Packet::PlayerAuthInput(Box::new(up)));
        server.run(vec![input]);
        let down = p::PlayerAuthInput {
            position: start.to_f32(),
            tick: 2,
            ..p::PlayerAuthInput::default()
        };
        let input = server.send(1, Packet::PlayerAuthInput(Box::new(down)));
        server.run(vec![input]);
        assert_eq!(server.survival_mut(1).health, 18.0);
        assert_eq!(server.survival_mut(1).fall_distance, 0.0);
    }

    /// Clearing the effects removes them in effect ID order, each removal carrying the effect's
    /// amplifier and the time it had left, as the official server sends them (P6 `syntax`).
    #[test]
    fn cleared_effects_go_in_id_order_with_what_they_had() {
        let mut server = Server::new(1, |_| {});
        for (effect, amplifier) in [
            (effect::Type::Regeneration, 0),
            (effect::Type::JumpBoost, 1),
            (effect::Type::NightVision, 0),
        ] {
            effects::add(
                &mut server.state,
                sid(1),
                effect,
                Active::new(100, amplifier),
            );
        }
        server.idle(1);
        server.take(1);
        effects::clear(&mut server.state, sid(1));
        server.idle(1);
        let removed: Vec<(i32, i32, i32)> = server
            .take(1)
            .iter()
            .filter_map(|p| match p {
                Packet::MobEffect(m) if m.operation == u8::from(MobEffectOperation::Remove) => {
                    Some((m.effect_type, m.amplifier, m.duration))
                }
                _ => None,
            })
            .collect();
        assert_eq!(removed, [(8, 1, 99), (10, 0, 99), (16, 0, 99)]);
        // The API's clear removes in the same order.
        for (effect, amplifier) in [
            (effect::Type::Regeneration, 0),
            (effect::Type::JumpBoost, 1),
            (effect::Type::NightVision, 0),
        ] {
            effects::add(
                &mut server.state,
                sid(1),
                effect,
                Active::new(100, amplifier),
            );
        }
        server.take(1);
        server.player_mut(1).clear_effects();
        let order: Vec<i32> = server
            .player_mut(1)
            .outbox
            .iter()
            .filter_map(|p| match p {
                Packet::MobEffect(m) if m.operation == u8::from(MobEffectOperation::Remove) => {
                    Some(m.effect_type)
                }
                _ => None,
            })
            .collect();
        assert_eq!(order, [8, 10, 16]);
    }

    #[test]
    fn effects_tick_at_their_intervals_and_end() {
        let mut server = Server::new(1, |_| {});
        server.survival_mut(1).food = 17.0;
        effects::add(
            &mut server.state,
            sid(1),
            effect::Type::Poison,
            Active::new(100, 0),
        );
        server.idle(100);
        assert_eq!(server.survival_mut(1).health, 16.0, "poison every 25 ticks");
        assert!(server.survival_mut(1).effects.is_empty());
        let sent = server.take(1);
        assert!(sent.iter().any(|p| matches!(
            p,
            Packet::MobEffect(m) if m.operation == u8::from(MobEffectOperation::Add) && m.duration == 100
        )));
        assert!(sent.iter().any(|p| matches!(
            p,
            Packet::MobEffect(m) if m.operation == u8::from(MobEffectOperation::Remove)
        )));
        server.survival_mut(1).health = 10.0;
        effects::add(
            &mut server.state,
            sid(1),
            effect::Type::Regeneration,
            Active::new(100, 0),
        );
        server.idle(100);
        assert_eq!(
            server.survival_mut(1).health,
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
        assert_eq!(
            server.survival_mut(1).health,
            20.0,
            "instant health II heals 8"
        );
        effects::add(
            &mut server.state,
            sid(1),
            effect::Type::HealthBoost,
            Active::new(20, 0),
        );
        assert_eq!(server.survival_mut(1).max_health(), 24.0);
        effects::add(
            &mut server.state,
            sid(1),
            effect::Type::Absorption,
            Active::new(20, 1),
        );
        assert_eq!(server.survival_mut(1).absorption, 8.0);
        server.idle(20);
        assert_eq!(server.survival_mut(1).max_health(), 20.0);
        assert_eq!(server.survival_mut(1).absorption, 0.0);
    }

    #[test]
    fn xp_levels_follow_the_formula() {
        let mut server = Server::new(1, |_| {});
        add_xp(server.player_mut(1), 7);
        assert_eq!(server.survival_mut(1).xp_level, 1);
        add_xp(server.player_mut(1), 345);
        assert_eq!(server.survival_mut(1).xp_level, 16);
        assert_eq!(server.survival_mut(1).xp_lifetime, 352);
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
        server.survival_mut(1).food = 10.0;
        server.survival_mut(1).saturation = 0.0;
        inventory::set_held(server.player_mut(1), stack(ItemKind::Bread, 3));
        assert!(start_eating(&mut server.state, sid(1)));
        assert!(server.survival_mut(1).using.is_some());
        server.idle(5);
        assert!(finish_eating(&mut server.state, sid(1)));
        assert_eq!(
            server.survival_mut(1).food,
            10.0,
            "too early: nothing eaten"
        );
        assert!(start_eating(&mut server.state, sid(1)));
        server.idle(32);
        assert!(finish_eating(&mut server.state, sid(1)));
        let bread = ItemKind::Bread.default_type().food().unwrap();
        assert_eq!(
            server.survival_mut(1).food,
            10.0 + f64::from(bread.food_restore())
        );
        assert_eq!(
            server.survival_mut(1).saturation,
            bread.saturation_restore()
        );
        assert_eq!(server.player(1).inventory.held().count(), 2);
        inventory::set_held(server.player_mut(1), stack(ItemKind::DiamondSword, 1));
        assert!(!start_eating(&mut server.state, sid(1)));
    }

    /// The client drinks a honey bottle for 40 ticks (its item data's `use_duration`), not the 32
    /// of most food; a quarter of that is forgiven.
    #[test]
    fn a_honey_bottle_needs_the_clients_drinking_time() {
        let mut server = Server::new(1, |_| {});
        server.survival_mut(1).food = 10.0;
        inventory::set_held(server.player_mut(1), stack(ItemKind::HoneyBottle, 1));
        assert!(start_eating(&mut server.state, sid(1)));
        server.idle(25);
        assert!(finish_eating(&mut server.state, sid(1)));
        assert_eq!(
            server.survival_mut(1).food,
            10.0,
            "too early: nothing drunk"
        );
        assert_eq!(server.player(1).inventory.held().count(), 1);
        assert!(start_eating(&mut server.state, sid(1)));
        server.idle(30);
        assert!(finish_eating(&mut server.state, sid(1)));
        assert!(server.survival_mut(1).food > 10.0);
        let glass_bottle = ItemKind::GlassBottle.default_type();
        assert_eq!(
            server.player(1).inventory.held().item_type(),
            Some(glass_bottle)
        );
    }

    /// A chorus fruit puts the feet on the floor it found, not the eyes.
    #[test]
    fn a_chorus_fruit_lands_the_feet_on_a_floor() {
        let mut server = Server::new(1, |_| {});
        inventory::set_held(server.player_mut(1), stack(ItemKind::ChorusFruit, 1));
        let start = server.player(1).position;
        assert!(start_eating(&mut server.state, sid(1)));
        server.idle(32);
        assert!(finish_eating(&mut server.state, sid(1)));
        let player = server.player(1);
        assert_ne!(player.position, start, "teleported");
        let at = BlockPos::containing(feet(player));
        let world = &server.state.worlds[0];
        let solid = |pos| world.block(pos).is_some_and(block::State::is_solid);
        assert!(!solid(at), "the feet are not in a block");
        assert!(
            solid(BlockPos::new(at.x, at.y - 1, at.z)),
            "they stand on one"
        );
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
            .filter(|s| drops::level(s, Enchantment::Vanishing) > 0)
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
    fn death_and_respawn_follow_their_sequence() {
        let mut server = Server::new(1, |_| {});
        add_xp(server.player_mut(1), 100);
        server.idle(1);
        server.take(1);
        assert!(server.hurt(1, 100.0, Damage::Magic));
        assert!(server.survival_mut(1).is_dead());
        assert_eq!(server.survival_mut(1).xp_level, 0);
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
            ..p::PlayerAction::default()
        };
        let input = server.send(1, Packet::PlayerAction(action));
        server.run(vec![input]);
        let s = server.survival_mut(1);
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

    /// A player that dies leaves the others' locator bar until it respawns, as the official
    /// server shows it (P6 `others`): a hidden location and an invisible waypoint, then a visible
    /// one.
    #[test]
    fn the_dead_leave_the_locator_bar_until_they_respawn() {
        let mut server = duel(|_| {});
        let waypoints = |packets: &[Packet]| -> Vec<(u8, u32, Option<bool>)> {
            packets
                .iter()
                .filter_map(|p| match p {
                    Packet::LocatorBar(b) => Some(
                        b.waypoints
                            .iter()
                            .map(|w| (w.action, w.waypoint.update_flag, w.waypoint.visible)),
                    ),
                    _ => None,
                })
                .flatten()
                .collect()
        };
        server.hurt(2, 100.0, Damage::Magic);
        server.idle(1);
        let seen = server.take(1);
        assert!(
            seen.iter()
                .any(|p| matches!(p, Packet::PlayerLocation(l) if l.r#type == 1))
        );
        assert_eq!(waypoints(&seen), [(3, 2, Some(false))]);
        damage::respawn(&mut server.state, sid(2), Stay::Online);
        server.idle(1);
        assert_eq!(waypoints(&server.take(1)), [(3, 2, Some(true))]);
    }

    /// `locator-bar = false`: the rule tells the client there are no waypoints, and no locator
    /// packet is made when a player the others see dies, respawns or leaves (seeing is
    /// `entity::tests::without_the_locator_bar_players_see_each_other_without_it`).
    #[test]
    fn without_the_locator_bar_no_locator_packet_is_sent() {
        let mut server = duel(|c| c.general.locator_bar = false);
        let rule = match game_rules(&server.state.settings) {
            Packet::GameRulesChanged(g) => g
                .game_rules
                .into_iter()
                .find(|r| r.name == "playerWaypoints"),
            _ => None,
        };
        assert_eq!(
            rule.map(|r| r.value),
            Some(crate::protocol::GameRuleValue::Uint32(0))
        );
        assert!(server.hurt(2, 100.0, Damage::Magic));
        server.idle(1);
        damage::respawn(&mut server.state, sid(2), Stay::Online);
        server.idle(1);
        server.run(vec![leave(2, 99)]);
        server.idle(1);
        let seen = server.take(1);
        assert!(
            seen.iter().any(|p| matches!(p, Packet::RemoveActor(_))),
            "player 1 saw player 2 go"
        );
        assert!(
            !seen
                .iter()
                .any(|p| matches!(p, Packet::LocatorBar(_) | Packet::PlayerLocation(_))),
            "{seen:?}"
        );
    }

    /// The client shrinks a dying player's box and only the box size keys bring it back: the
    /// official server sends 0.2 by 0.2 at the death and 0.6 by 1.8 after the respawn, to the
    /// player and its viewers (P6 `others`). Without them the others see a respawned player with a
    /// small hit box and a low name tag.
    #[test]
    fn viewers_see_a_respawned_player_at_full_size() {
        let mut server = duel(|_| {});
        let runtime_id = server.player(2).runtime_id;
        let sizes = |packets: &[Packet]| -> Vec<(f32, f32)> {
            packets
                .iter()
                .filter_map(|p| match p {
                    Packet::SetActorData(d) if d.entity_runtime_id == runtime_id => {
                        let m = &d.entity_metadata;
                        match (
                            m.get(&EntityDataKey::Width.into()),
                            m.get(&EntityDataKey::Height.into()),
                        ) {
                            (Some(MetadataValue::Float32(w)), Some(MetadataValue::Float32(h))) => {
                                Some((*w, *h))
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                })
                .collect()
        };
        server.hurt(2, 100.0, Damage::Magic);
        server.idle(1);
        assert_eq!(sizes(&server.take(1)), [(0.2, 0.2)]);
        assert_eq!(sizes(&server.take(2)), [(0.2, 0.2)]);
        let ready = p::Respawn {
            position: [0.0; 3],
            state: u8::from(RespawnState::ClientReadyToSpawn),
            entity_runtime_id: runtime_id,
        };
        let action = p::PlayerAction {
            entity_runtime_id: runtime_id,
            action_type: i32::from(Action::Respawn),
            ..p::PlayerAction::default()
        };
        let inputs = vec![
            server.send(2, Packet::Respawn(ready)),
            server.send(2, Packet::PlayerAction(action)),
        ];
        server.run(inputs);
        server.idle(1);
        assert_eq!(sizes(&server.take(1)), [(0.6, 1.8)]);
        assert_eq!(sizes(&server.take(2)), [(0.6, 1.8)]);
    }

    /// Viewers hear another player's health when it changes, at its death and after its respawn,
    /// as the official server tells them (P6 `others`): the client counts a player at 0 health as
    /// dead.
    #[test]
    fn viewers_see_the_health_of_other_players() {
        let mut server = duel(|_| {});
        let runtime_id = server.player(2).runtime_id;
        let healths = |packets: &[Packet]| -> Vec<Vec<(String, f32)>> {
            packets
                .iter()
                .filter_map(|p| match p {
                    Packet::UpdateAttributes(a) if a.entity_runtime_id == runtime_id => Some(
                        a.attributes
                            .iter()
                            .map(|x| (x.attribute_value.name.clone(), x.attribute_value.value))
                            .collect(),
                    ),
                    _ => None,
                })
                .collect()
        };
        let health = |v: f32| vec![vec![("minecraft:health".to_owned(), v)]];
        assert!(server.hurt(2, 1.0, Damage::Magic));
        server.idle(1);
        assert_eq!(healths(&server.take(1)), health(19.0));
        server.idle(100);
        server.take(1);
        server.survival_mut(2).no_damage_ticks = 0;
        server.survival_mut(2).attack_time = 0;
        assert!(server.hurt(2, 100.0, Damage::Magic));
        server.idle(1);
        assert_eq!(healths(&server.take(1)), health(0.0));
        let ready = p::Respawn {
            position: [0.0; 3],
            state: u8::from(RespawnState::ClientReadyToSpawn),
            entity_runtime_id: runtime_id,
        };
        let action = p::PlayerAction {
            entity_runtime_id: runtime_id,
            action_type: i32::from(Action::Respawn),
            ..p::PlayerAction::default()
        };
        let inputs = vec![
            server.send(2, Packet::Respawn(ready)),
            server.send(2, Packet::PlayerAction(action)),
        ];
        server.run(inputs);
        assert_eq!(healths(&server.take(1)), health(20.0));
    }

    /// A player that respawns where its client has no chunks yet (another world, or far away)
    /// still gets its own respawn data: it does not see itself through the chunks it was sent.
    #[test]
    fn a_player_respawning_out_of_its_view_gets_its_own_respawn_data() {
        let mut server = Server::new(1, |_| {});
        let spawn = BlockPos::new(0, -60, 0);
        let other = crate::world::World::new("other", crate::world::Kind::Temporary, 3, spawn);
        server.state.add_world(other);
        server.survival_mut(1).spawn = Some(Point {
            world: "other".to_owned(),
            pos: BlockPos::new(2, -60, 2),
        });
        assert!(server.hurt(1, 100.0, Damage::Magic));
        server.idle(1);
        server.take(1);
        let ready = p::Respawn {
            position: [0.0; 3],
            state: u8::from(RespawnState::ClientReadyToSpawn),
            entity_runtime_id: 1,
        };
        let action = p::PlayerAction {
            entity_runtime_id: 1,
            action_type: i32::from(Action::Respawn),
            ..p::PlayerAction::default()
        };
        let inputs = vec![
            server.send(1, Packet::Respawn(ready)),
            server.send(1, Packet::PlayerAction(action)),
        ];
        server.run(inputs);
        assert_eq!(server.player(1).world, 1);
        let back = server.take(1);
        assert!(back.iter().any(|p| matches!(
            p,
            Packet::ActorEvent(e) if e.event_type == u8::from(ActorEvent::SpawnAlive)
        )));
        let data = back
            .iter()
            .filter(|p| matches!(p, Packet::SetActorData(d) if d.entity_runtime_id == 1))
            .count();
        assert_eq!(data, 2, "the whole entity data and the box size");
    }

    #[test]
    fn hardcore_players_come_back_as_spectators() {
        let mut server = Server::new(1, |c| {
            c.general.hardcore = true;
            c.general.difficulty = Difficulty::Hard;
        });
        server.hurt(1, 100.0, Damage::Magic);
        damage::respawn(&mut server.state, sid(1), Stay::Online);
        assert_eq!(server.player(1).game_mode(), GameMode::Spectator);
        assert!(!server.hurt(1, 5.0, Damage::Magic));
    }

    /// The hardcore spectator is a game mode change like any: flight and collision overrides of
    /// the old mode end.
    #[test]
    fn a_hardcore_spectator_drops_the_flight_and_collision_overrides() {
        let mut server = Server::new(1, |c| {
            c.general.hardcore = true;
            c.general.difficulty = Difficulty::Hard;
        });
        server.player_mut(1).set_can_fly(false);
        server.player_mut(1).set_block_collision(true);
        server.hurt(1, 100.0, Damage::Magic);
        damage::respawn(&mut server.state, sid(1), Stay::Online);
        let player = server.player(1);
        assert_eq!(player.game_mode(), GameMode::Spectator);
        assert!(player.can_fly());
        assert!(player.is_flying());
        assert!(!player.has_block_collision());
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
                    ..p::PlayerAction::default()
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
                        ..p::PlayerAction::default()
                    };
                    server.send(1, Packet::PlayerAction(action))
                })
                .collect();
            server.run(inputs);
            server.state.hash()
        };
        assert_eq!(sprint(1), sprint(2));
    }

    /// The identity of `player1` ([`join`]) for a join of another session.
    fn player1() -> crate::tick::queue::Identity {
        crate::tick::queue::Identity {
            name: "player1".to_owned(),
            xuid: Some(2_535_000_001),
            uuid: crate::protocol::Uuid::from_bytes([1; 16]),
            skin: crate::tick::queue::Skin::default(),
            device_os: -1,
            address: None,
            client: crate::tick::queue::Client::default(),
        }
    }

    /// Joins `player1` as session `s` with saved data on (the data the stand-in save thread
    /// answers is `data`, if any), spawns it and returns the packets it got.
    fn join_saved(
        server: &mut Server,
        s: u64,
        data: Option<&crate::save::PlayerData>,
    ) -> Vec<Packet> {
        let (j, rx) = join_with(s, player1());
        server.receivers.push(rx);
        server.run(vec![j]);
        for answer in &mut server.output.answers {
            if let crate::save::Response::Player { data: load, .. } = answer
                && let Some(data) = data
            {
                *load = crate::save::PlayerLoad::Found(data.clone());
            }
        }
        server.idle(1);
        server.run(vec![radius(s, 1, 8)]);
        let runtime_id = server.player(s).runtime_id;
        server.run(vec![initialised(s, 2, runtime_id)]);
        server.idle(2);
        server.take(u64::try_from(server.receivers.len()).unwrap())
    }

    /// A player that comes back dead (the server stopped, or it left, before it respawned)
    /// respawns before its start-game packets: a client that spawns dead shows no death screen
    /// and so never asks to respawn.
    #[test]
    fn a_player_that_joins_dead_respawns_before_it_spawns() {
        let saving = |c: &mut Config| c.general.save_player_data = true;
        // The first run: the player dies away from the spawn and the server stops.
        let mut first = Server::new(1, |_| {});
        let away = first.player(1).position + Vec3::new(20.0, 0.0, 0.0);
        teleport(&mut first.state, sid(1), 0, away);
        assert!(first.hurt(1, 100.0, Damage::Magic));
        let data = crate::tick::save::data_of(&first.state, first.player(1));
        let home = spawn_eye(&first.state);
        let mut server = Server {
            state: new_state(settings(saving), Listeners::default()),
            output: Output::default(),
            receivers: Vec::new(),
            seq: 10,
        };
        let respawned = |server: &Server, s: u64, sent: &[Packet]| {
            let p = server.player(s);
            assert!(is_alive(p), "alive");
            assert_eq!(p.position, home, "at the respawn point");
            assert!(sent.iter().any(|p| matches!(
                p,
                Packet::StartGame(g) if g.player_position == home.to_f32()
            )));
            assert!(sent.contains(&Packet::SetHealth(p::SetHealth { health: 20 })));
            assert!(!sent.iter().any(|p| matches!(p, Packet::DeathInfo(_))));
        };
        // The next run: the saved data comes back.
        let sent = join_saved(&mut server, 1, Some(&data));
        respawned(&server, 1, &sent);
        // Past its respawn invulnerability, it dies again and leaves; the server keeps its data
        // until the commit.
        server.idle(80);
        teleport(&mut server.state, sid(1), 0, away);
        assert!(server.hurt(1, 100.0, Damage::Magic));
        server.run(vec![leave(1, 3)]);
        let sent = join_saved(&mut server, 2, None);
        respawned(&server, 2, &sent);
    }

    /// The eye position at the default world's spawn.
    fn spawn_eye(state: &State) -> Vec3 {
        standing_eye(state.worlds[0].safe_spawn(state.worlds[0].spawn()))
    }

    #[test]
    fn a_hardcore_player_that_joins_dead_comes_back_as_a_spectator() {
        let hardcore = |c: &mut Config| {
            c.general.hardcore = true;
            c.general.difficulty = Difficulty::Hard;
        };
        let mut first = Server::new(1, hardcore);
        assert!(first.hurt(1, 100.0, Damage::Magic));
        let data = crate::tick::save::data_of(&first.state, first.player(1));
        let mut server = Server {
            state: new_state(
                settings(|c| {
                    hardcore(c);
                    c.general.save_player_data = true;
                }),
                Listeners::default(),
            ),
            output: Output::default(),
            receivers: Vec::new(),
            seq: 10,
        };
        let sent = join_saved(&mut server, 1, Some(&data));
        assert!(is_alive(server.player(1)));
        let spectator = saved::game_mode_id(GameMode::Spectator);
        assert!(sent.iter().any(|p| matches!(
            p,
            Packet::StartGame(g) if g.player_game_mode == spectator
        )));
    }

    #[test]
    fn the_world_clock_advances_and_corrects_clients() {
        let mut server = Server::new(1, |_| {});
        let before = server.state.worlds[0].time_ticks();
        server.idle(300);
        assert_eq!(server.state.worlds[0].time_ticks(), before + 300);
        let syncs = server
            .take(1)
            .into_iter()
            .filter(|p| matches!(p, Packet::SyncWorldClocks(_)))
            .count();
        assert!((1..=2).contains(&syncs), "{syncs}");
    }

    #[test]
    fn join_and_leave_cycles_leave_nothing_behind() {
        let mut server = Server::new(1, |_| {});
        let clocks = server.state.worlds[0].time_ticks();
        for i in 0..100 {
            let s = 2 + i;
            let (j, _rx) = join(s, 0);
            server.run(vec![j, radius(s, 1, 4)]);
            server.run(vec![initialised(s, 2, server.state.next_runtime_id - 1)]);
            server.run(vec![leave(s, 3)]);
        }
        assert_eq!(server.state.players.len(), 1);
        assert_eq!(server.state.clock.worlds_len(), 1);
        assert!(server.state.worlds[0].time_ticks() > clocks);
        assert!(server.player(1).outbox.is_empty());
        let s = server.survival_mut(1);
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

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test stands in for the tick handing a snapshot to the save thread"
    )]
    fn world_time_and_spawn_survive_a_save_and_restart_to_the_tick() {
        let specs = || {
            vec![WorldSpec {
                name: "world".to_owned(),
                kind: crate::world::Kind::Persistent,
                generator: std::sync::Arc::new(crate::world::Flat),
                seed: Some(0),
                spawn: None,
            }]
        };
        // A world 23,990 ticks old with its spawn moved, run 5 ticks, then saved at the tick
        // boundary.
        let moved = crate::world::BlockPos::new(8, -59, -8);
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut output = Output::default();
        state.worlds[0].set_time(23_990);
        state.worlds[0].set_spawn(moved).unwrap();
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
            snapshot.worlds,
            [(
                0,
                save::WorldValues {
                    time: 23_995,
                    spawn: moved
                }
            )],
            "the snapshot carries the time and the spawn"
        );
        let dir = crate::save::tests::TempDir::new("survival-time");
        let opened = save::open(&dir.0, specs()).unwrap();
        let (requests, queue) = tokio::sync::mpsc::channel(save::MAX_REQUESTS);
        let (answers, mut answered) = tokio::sync::mpsc::channel(save::MAX_RESPONSES);
        let thread = opened.thread;
        let handle = crate::runtime::spawn_save(move || {
            let pool = crate::runtime::cpu_pool(1).unwrap();
            let result = save::run(thread, &pool, queue, &answers);
            assert!(pool.shutdown());
            result
        })
        .unwrap();
        requests.blocking_send(Request::Commit(snapshot)).unwrap();
        requests.blocking_send(Request::Finish).unwrap();
        drop(requests);
        handle.join().unwrap().unwrap();
        assert!(matches!(
            answered.try_recv(),
            Ok(Response::Committed { start }) if start == state.tick
        ));

        // The restart takes the engine tick, the time and the spawn from the file, not the
        // generator's spawn; both runs go on alike through the night's end into the next day.
        let reopened = save::open(&dir.0, specs()).unwrap();
        assert_eq!(reopened.tick, state.tick);
        assert_eq!(reopened.worlds[0].time_ticks(), 23_995);
        assert_eq!(reopened.worlds[0].spawn(), moved);
        let mut restarted = State::new(
            settings(|_| {}),
            crate::tick::Shared::new(
                crate::protocol::batch::Settings::default(),
                &crate::custom::Registry::default(),
                &crate::tick::entity::Customs::default(),
            )
            .unwrap(),
            crate::tick::Setup::default(),
            reopened.worlds,
            reopened.tick,
        );
        drop(reopened.thread);
        let mut restarted_output = Output::default();
        for _ in 0..40 {
            tick(&mut state, &mut Vec::new(), &mut output);
            tick(&mut restarted, &mut Vec::new(), &mut restarted_output);
            assert_eq!(restarted.tick, state.tick);
            assert_eq!(
                restarted.worlds[0].time_ticks(),
                state.worlds[0].time_ticks()
            );
        }
        assert_eq!(restarted.worlds[0].time_ticks(), 24_035);
        assert_eq!(restarted.worlds[0].time_of_day(), 35, "a new day");
    }

    /// The attacker's side of a hit on an NPC is the one of a hit on a player: Strength and
    /// Weakness, then half again for a critical hit.
    #[test]
    fn melee_hits_count_strength_weakness_and_criticals() {
        let mut server = Server::new(1, |_| {});
        let plain = Melee {
            damage: 1.0,
            critical: false,
            magic: false,
        };
        assert_eq!(melee(&server.state, sid(1)), Some(plain));
        let s = server.survival_mut(1);
        s.effects
            .insert(effect::Type::Strength, Active::new(100, 1));
        s.fall_distance = 1.0;
        let m = melee(&server.state, sid(1)).unwrap();
        assert!(m.critical && !m.magic);
        assert!((m.damage - 1.6 * 1.5).abs() < 1e-9, "{}", m.damage);
        let s = server.survival_mut(1);
        s.effects
            .insert(effect::Type::Weakness, Active::new(100, 0));
        let m = melee(&server.state, sid(1)).unwrap();
        assert!((m.damage - 1.4 * 1.5).abs() < 1e-9, "{}", m.damage);
        assert!(server.hurt(1, 100.0, Damage::Magic));
        assert_eq!(melee(&server.state, sid(1)), None, "the dead hit nothing");
    }

    /// A falling hit that Weakness brings to nothing is no critical hit: half of nothing adds
    /// nothing, so the animation is not shown.
    #[test]
    fn a_critical_hit_of_nothing_is_not_critical() {
        let mut server = Server::new(1, |_| {});
        let s = server.survival_mut(1);
        s.effects
            .insert(effect::Type::Weakness, Active::new(100, 4));
        s.fall_distance = 1.0;
        let m = melee(&server.state, sid(1)).unwrap();
        assert_eq!(m.damage, 0.0);
        assert!(!m.critical);
    }

    /// Healing, burning and the hit cooldown other rules reach through the contract.
    #[test]
    fn heal_burn_and_the_hit_cooldown_work_through_the_contract() {
        let mut server = Server::new(1, |_| {});
        server.survival_mut(1).health = 10.0;
        heal(&mut server.state, sid(1), 4.0);
        assert_eq!(server.survival_mut(1).health, 14.0);
        heal(&mut server.state, sid(1), 100.0);
        assert_eq!(server.survival_mut(1).health, 20.0, "up to the maximum");
        let fire = CombustSource::Block(BlockPos::new(0, -60, 0));
        set_on_fire_by(&mut server.state, sid(1), 5, fire);
        assert_eq!(server.survival_mut(1).fire_ticks, 100);
        set_on_fire_by(&mut server.state, sid(1), 2, fire);
        assert_eq!(
            server.survival_mut(1).fire_ticks,
            100,
            "a longer burn stays"
        );
        assert!(!has_attack_cooldown(server.player(1)));
        assert!(server.hurt(1, 1.0, Damage::Entity));
        assert!(has_attack_cooldown(server.player(1)));
        server.idle(10);
        assert!(!has_attack_cooldown(server.player(1)));
    }

    #[test]
    fn the_contract_reads_follow_the_player_and_the_world() {
        let mut server = Server::new(1, |_| {});
        let s = server.survival_mut(1);
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
    fn effects_replace_only_stronger_or_longer_ones() {
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
            server.survival_mut(1).effects[&effect::Type::Speed],
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
        // A replaced health boost is undone first.
        let boost = |amplifier| {
            effect::Instance::new(effect::Type::HealthBoost, 600, amplifier, true, false)
        };
        assert!(add_effect(&mut server.state, sid(1), boost(1)));
        server.survival_mut(1).health = 28.0;
        assert!(add_effect(&mut server.state, sid(1), boost(2)));
        assert_eq!(server.survival_mut(1).health, 20.0);
        assert_eq!(server.survival_mut(1).max_health(), 32.0);
    }

    #[test]
    fn a_totem_in_either_hand_saves_the_player() {
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
            let s = server.survival_mut(1);
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
        assert!(server.survival_mut(1).is_dead());
    }

    #[test]
    fn orb_pickups_mend_first_then_add_xp() {
        let mut server = Server::new(1, |_| {});
        let pick = enchanted(ItemKind::DiamondPickaxe, &[(Enchantment::Mending, 1)])
            .with_damage(10)
            .unwrap();
        inventory::set_held(server.player_mut(1), pick);
        server.idle(1);
        server.take(1);
        // 3 points repair 6 durability and are used up.
        assert!(pickup_xp(&mut server.state, sid(1), 3));
        assert_eq!(server.player(1).inventory.held().damage(), 4);
        assert_eq!(food::current_xp(server.survival_mut(1)), 0);
        assert!(
            !pickup_xp(&mut server.state, sid(1), 3),
            "the pickup cooldown"
        );
        server.idle(2);
        // 5 points: 4 durability left to repair takes 2 points; 3 are xp.
        assert!(pickup_xp(&mut server.state, sid(1), 5));
        assert_eq!(server.player(1).inventory.held().damage(), 0);
        assert_eq!(food::current_xp(server.survival_mut(1)), 3);
        server.idle(1);
        let sent = server.take(1);
        assert!(level_events(&sent).contains(&i32::from(LevelEvent::SoundExperienceOrbPickup)));
        // Worn armor mends too; a level that crosses a multiple of 5 plays the level-up sound.
        let helmet = enchanted(ItemKind::DiamondHelmet, &[(Enchantment::Mending, 1)])
            .with_damage(1)
            .unwrap();
        inventory::set_held(server.player_mut(1), Stack::EMPTY);
        inventory::set_armor(server.player_mut(1), 0, helmet);
        add_xp(server.player_mut(1), 38);
        assert_eq!(server.survival_mut(1).xp_level, 4);
        server.idle(3);
        server.take(1);
        assert!(pickup_xp(&mut server.state, sid(1), 20));
        assert_eq!(server.player(1).inventory.armor()[0].damage(), 0);
        assert_eq!(server.survival_mut(1).xp_level, 5);
        server.idle(1);
        assert_eq!(sounds(&server.take(1)), ["levelup"]);
        // Dead players pick nothing up.
        server.hurt(1, 100.0, Damage::Magic);
        server.idle(3);
        assert!(!pickup_xp(&mut server.state, sid(1), 5));
    }

    #[test]
    fn medicine_cures_its_effect_and_starts_only_with_it() {
        let mut server = Server::new(1, |_| {});
        let antidote =
            crate::item::Type::from_name("minecraft:medicine", 2, None, &Compound::new()).unwrap();
        inventory::set_held(server.player_mut(1), Stack::new(antidote, 1));
        assert!(start_eating(&mut server.state, sid(1)));
        assert!(
            server.survival_mut(1).using.is_none(),
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
        assert!(start_eating(&mut server.state, sid(1)));
        server.idle(32);
        assert!(finish_eating(&mut server.state, sid(1)));
        let s = server.survival_mut(1);
        assert!(!s.effects.contains_key(&effect::Type::Poison));
        assert!(s.effects.contains_key(&effect::Type::Blindness));
        let held = server.player(1).inventory.held().item_type().unwrap();
        assert_eq!(held.name(), "minecraft:glass_bottle");
    }

    #[test]
    fn cactus_and_lava_hurt_a_player_inside_them() {
        let mut server = Server::new(1, |_| {});
        let at = BlockPos::containing(feet(server.player(1)));
        server.state.worlds[0]
            .set_block(at, block::Kind::Cactus.default_state())
            .unwrap();
        server.idle(1);
        assert_eq!(server.survival_mut(1).health, 19.0, "cactus pricks for 1");
        server.idle(10);
        server.state.worlds[0]
            .set_block(at, block::Kind::Lava.default_state())
            .unwrap();
        server.idle(1);
        let s = server.survival_mut(1);
        assert!(s.health <= 15.0, "lava burns for 4: {}", s.health);
        assert!(s.fire_ticks > 0);
    }

    /// A contact death names the cactus only when a cactus dealt it; a sweet berry bush is a
    /// generic death.
    #[test]
    fn a_contact_death_names_the_cactus_only_for_a_cactus() {
        let bush = block::Kind::SweetBerryBush
            .default_state()
            .with("growth", PropertyValue::Int(3))
            .unwrap();
        for (block, want) in [
            (bush, damage::GENERIC_DEATH),
            (block::Kind::Cactus.default_state(), "death.attack.cactus"),
        ] {
            let mut server = Server::new(1, |_| {});
            let at = BlockPos::containing(feet(server.player(1)));
            server.survival_mut(1).health = 1.0;
            server.state.worlds[0].set_block(at, block).unwrap();
            server.idle(1);
            assert!(server.survival_mut(1).is_dead(), "{want}");
            let died = server
                .take(1)
                .into_iter()
                .any(|p| matches!(p, Packet::DeathInfo(d) if d.cause == want));
            assert!(died, "{want}");
        }
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
        let color = |v: u8| Rgba {
            r: v,
            g: v + 1,
            b: v + 2,
            a: 255,
        };
        crate::protocol::Skin {
            skin_id: "c18e65aa-7b21-4637-9b63-8ad63622ef01_Custom".to_owned(),
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
            skin_colour: color(10),
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
                colours: [color(1), color(2), color(3), color(4)],
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
            skin: crate::tick::queue::Skin::new(std::sync::Arc::new(skin.clone())),
            device_os: 7,
            address: None,
            client: crate::tick::queue::Client::default(),
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
        // The joining player's own list (twice, as the official server) and the other player's
        // addition.
        let own = entries(&received(&mut rx).0);
        let theirs = entries(&server.take(1));
        let shown: Vec<_> = own
            .iter()
            .chain(&theirs)
            .filter(|e| e.username == "Alex")
            .collect();
        assert_eq!(shown.len(), 3);
        for entry in shown {
            assert_eq!(*entry.skin, skin);
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
        server.survival_mut(1).fall_distance = 7.0;
        let near = server.player(1).position + Vec3::new(20.0, 0.0, 0.0);
        teleport(&mut server.state, sid(1), 0, near);
        assert_eq!(server.player(1).position, near);
        assert_eq!(server.survival_mut(1).fall_distance, 0.0);
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
        assert!(server.player(1).view.can_see(chunk));
        // Out of the world or into no world: nothing happens.
        teleport(&mut server.state, sid(1), 0, Vec3::new(f64::NAN, 0.0, 0.0));
        teleport(&mut server.state, sid(1), 0, Vec3::new(4.0e7, 0.0, 0.0));
        teleport(&mut server.state, sid(1), 5, far);
        assert_eq!(server.player(1).position, far);
        // Another world: its chunks and clock, the old world's chunks let go.
        let spawn = BlockPos::new(0, -60, 0);
        let other = crate::world::World::new("other", crate::world::Kind::Temporary, 3, spawn);
        server.state.add_world(other);
        server.state.worlds[1].set_time(5000);
        server.take(1);
        let there = Vec3::new(0.5, -58.38, 0.5);
        teleport(&mut server.state, sid(1), 1, there);
        assert_eq!(server.player(1).world, 1);
        assert_eq!(server.player(1).position, there);
        assert!(!server.player(1).view.can_see(chunk));
        server.idle(4);
        assert!(
            server
                .player(1)
                .view
                .can_see(crate::tick::view::chunk_of(there))
        );
        let sent = server.take(1);
        assert!(sent.iter().any(|p| matches!(
            p,
            Packet::SyncWorldClocks(c) if c.sync_states.first().is_some_and(|s| s.time > 5000)
        )));
        assert!(sent.iter().any(|p| matches!(p, Packet::LevelChunk(_))));
        assert!(
            !sent
                .iter()
                .any(|p| matches!(p, Packet::LevelEventGeneric(_)))
        );
        // Back home, nothing of the trip is held.
        let home = server.player(2).position;
        teleport(&mut server.state, sid(1), 0, home);
        server.idle(4);
        server.player_mut(1).view.out = crate::tick::view::Out::default();
        assert_eq!(server.state.chunks.held(), held);
    }

    // api: Player::teleport
    #[test]
    fn a_player_still_waiting_for_its_spawn_chunks_hears_the_new_worlds_difficulty() {
        let mut state = new_state(
            settings(|c| c.developer.chunks_per_tick = 1),
            Listeners::default(),
        );
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 2)], &mut output);
        received(&mut rx);
        assert_eq!(state.players[&sid(1)].phase, Phase::Joined);
        let spawn = BlockPos::new(0, -60, 0);
        let mut other = crate::world::World::new("other", crate::world::Kind::Temporary, 3, spawn);
        other.set_difficulty(Some(Difficulty::Peaceful));
        state.add_world(other);
        teleport(&mut state, sid(1), 1, Vec3::new(0.5, -58.38, 0.5));
        tick(&mut state, &mut Vec::new(), &mut output);
        let set: Vec<u32> = received(&mut rx)
            .0
            .iter()
            .filter_map(|p| match p {
                Packet::SetDifficulty(d) => Some(d.difficulty),
                _ => None,
            })
            .collect();
        assert_eq!(set, [0]);
    }

    /// A player that got its start-game packets has the old world's clock, so a teleport to
    /// another world before its spawn tells it the new world's, as it does a spawned player.
    #[test]
    fn a_player_still_waiting_for_its_spawn_chunks_hears_the_new_worlds_clock() {
        let mut state = new_state(
            settings(|c| c.developer.chunks_per_tick = 1),
            Listeners::default(),
        );
        let mut output = Output::default();
        let (j, mut rx) = join(1, 0);
        tick(&mut state, &mut vec![j, radius(1, 1, 2)], &mut output);
        received(&mut rx);
        assert_eq!(state.players[&sid(1)].phase, Phase::Joined);
        let spawn = BlockPos::new(0, -60, 0);
        let other = crate::world::World::new("other", crate::world::Kind::Temporary, 3, spawn);
        state.add_world(other);
        state.worlds[1].set_time(5000);
        teleport(&mut state, sid(1), 1, Vec3::new(0.5, -58.38, 0.5));
        tick(&mut state, &mut Vec::new(), &mut output);
        let clocks: Vec<i32> = received(&mut rx)
            .0
            .iter()
            .filter_map(|p| match p {
                Packet::SyncWorldClocks(c) => c.sync_states.first().map(|s| s.time),
                _ => None,
            })
            .collect();
        assert!(clocks.iter().any(|&time| time >= 5000), "{clocks:?}");
    }

    #[test]
    fn a_world_with_a_smaller_view_distance_shrinks_the_radius_and_tells_the_client() {
        let mut server = Server::new(1, |c| {
            c.general.view_distance = 4;
            let world = crate::config::WorldSettings {
                view_distance: Some(10),
                ..crate::config::WorldSettings::default()
            };
            c.worlds.insert("world".to_owned(), world);
        });
        assert_eq!(
            server.player(1).view.radius(),
            8,
            "the default world allows 10"
        );
        let spawn = BlockPos::new(0, -60, 0);
        let other = crate::world::World::new("other", crate::world::Kind::Temporary, 3, spawn);
        server.state.add_world(other);
        teleport(&mut server.state, sid(1), 1, Vec3::new(0.5, -58.38, 0.5));
        assert_eq!(
            server.player(1).view.radius(),
            4,
            "the other world allows 4"
        );
        assert_eq!(server.player(1).chunk_radius, Some(4));
        server.idle(1);
        assert!(
            server
                .take(1)
                .iter()
                .any(|p| matches!(p, Packet::ChunkRadiusUpdated(c) if c.chunk_radius == 4))
        );
    }

    #[test]
    fn players_respawn_at_a_spawn_point_in_another_world() {
        let mut server = Server::new(1, |_| {});
        let spawn = BlockPos::new(0, -60, 0);
        let other = crate::world::World::new("other", crate::world::Kind::Temporary, 3, spawn);
        server.state.add_world(other);
        server.survival_mut(1).spawn = Some(Point {
            world: "other".to_owned(),
            pos: BlockPos::new(2, -60, 2),
        });
        assert!(server.hurt(1, 100.0, Damage::Magic));
        damage::respawn(&mut server.state, sid(1), Stay::Online);
        let p = server.player(1);
        assert_eq!(p.world, 1);
        assert_eq!(p.position, Vec3::new(2.5, -60.0 + EYE_HEIGHT, 2.5));
        assert!(!p.survival.is_dead());
        // A spawn point in a world the server does not run: the default world's spawn.
        server.survival_mut(1).spawn = Some(Point {
            world: "gone".to_owned(),
            pos: BlockPos::new(2, -60, 2),
        });
        server.idle(80);
        assert!(server.hurt(1, 100.0, Damage::Magic));
        damage::respawn(&mut server.state, sid(1), Stay::Online);
        assert_eq!(server.player(1).world, 0);
        let home = server.state.worlds[0].spawn();
        assert_eq!(server.player(1).position.x, f64::from(home.x) + 0.5);
    }

    #[test]
    fn held_back_death_drops_survive_a_restart_and_drop_without_loss() {
        let named = stack(ItemKind::DiamondSword, 1)
            .with_extra(Some(Extra {
                custom_name: Some("Edge".to_owned()),
                enchantments: vec![(Enchantment::Sharpness, 3)],
                ..Extra::default()
            }))
            .unwrap();
        let held: Vec<Stack> = vec![
            stack(ItemKind::Bread, 64),
            named,
            stack(ItemKind::IronPickaxe, 1).with_damage(40).unwrap(),
            stack(ItemKind::Totem, 1),
        ];
        let s = Survival {
            held_back: held.clone(),
            ..Survival::default()
        };
        let mut extra = Compound::new();
        save(&s, &mut extra);
        // Through the server file's player record.
        let record = crate::storage::codec::PlayerRecord {
            world: "world".to_owned(),
            position: [0.5, -58.38, 0.5],
            extra,
        };
        let sealed = crate::storage::seal::seal(b"player", &record).unwrap();
        let read: crate::storage::codec::PlayerRecord =
            crate::storage::seal::unseal(b"player", &sealed).unwrap();
        let mut loaded = Survival::default();
        load(
            &mut loaded,
            &read.extra,
            &crate::custom::Registry::default(),
        );
        assert_eq!(loaded.held_back, held);
        let mut again = Compound::new();
        save(&loaded, &mut again);
        assert_eq!(again, read.extra, "saved again byte for byte");
        // Once the world takes them, every item drops.
        let mut server = Server::new(1, |_| {});
        server.survival_mut(1).held_back = loaded.held_back;
        server.idle(3);
        assert!(server.survival_mut(1).held_back.is_empty());
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
                ..p::PlayerAction::default()
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
        let s = server.survival_mut(1);
        assert!(s.held_back.is_empty() && s.cooldowns.is_empty() && s.effects.is_empty());
    }
}
