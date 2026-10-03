//! Damage, healing, melee combat, death and respawn (PMMP `Entity::attack`, `Living::attack`,
//! `Living::applyDamageModifiers`, `Human::applyDamageModifiers`, `Player::attack`,
//! `Player::attackEntity`, `Player::onDeath`, `Player::respawn`, `DeathPacketHandler`).
//!
//! A hit is worked out in two steps, so events (M8) can sit between them: [`assess`] checks
//! whether the hit lands and computes its modifiers, [`apply`] changes the state.

use super::attributes::{self, ABSORPTION, HEALTH};
use super::effects::{self, Active};
use super::{
    EYE_HEIGHT, MAX_AIR, Point, Survival, block_at, broadcast_from, broadcast_to_viewers, clock,
    feet, food, game_mode_of, movement, player_random, sound, world_seed,
};
use crate::config::GameMode;
use crate::effect::Type as Effect;
use crate::item::{Class, Enchantment, Stack, Type};
use crate::math::{self, Vec3};
use crate::protocol::EntityProperties;
use crate::protocol::consts::{ActorEvent, AnimateAction, LevelEvent, RespawnState, SoundEvent};
use crate::protocol::packet::{
    self, Animate, DeathInfo, Packet, Respawn, SetActorData, SetActorMotion, SetPlayerGameType,
};
use crate::random::{self, Random};
use crate::save::WorldIndex;
use crate::tick::{Phase, Player, SessionId, State, entity, inventory};
use crate::world::{self, BlockPos};

/// Why a player takes damage (PMMP `EntityDamageEvent::CAUSE_*`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::tick) enum Damage {
    /// A melee hit by another player.
    Attack(SessionId),
    /// Hit by a projectile or an entity.
    Entity,
    /// A fall.
    Fall,
    /// Standing in fire.
    Fire,
    /// Burning after leaving fire.
    FireTick,
    /// Touching lava.
    Lava,
    /// Under water without air.
    Drowning,
    /// Inside a solid block.
    Suffocation,
    /// An explosion.
    Explosion,
    /// Below the world.
    Void,
    /// An effect (poison, harming).
    Magic,
    /// Empty food bar.
    Starvation,
    /// Cactus, berry bush.
    Contact,
}

impl Damage {
    /// Whether armour reduces it (PMMP `EntityDamageEvent::canBeReducedByArmor`).
    fn reducible(self) -> bool {
        !matches!(
            self,
            Self::FireTick
                | Self::Suffocation
                | Self::Drowning
                | Self::Starvation
                | Self::Fall
                | Self::Void
                | Self::Magic
        )
    }

    /// Whether it is fire damage (fire resistance cancels it).
    fn is_fire(self) -> bool {
        matches!(self, Self::Fire | Self::FireTick | Self::Lava)
    }

    /// The translation key of the death message (PMMP `PlayerDeathEvent::deriveMessage`).
    fn death_message(self) -> &'static str {
        match self {
            Self::Attack(_) => "death.attack.player",
            Self::Entity => "death.attack.arrow",
            Self::Fall => "death.attack.fall",
            Self::Fire => "death.attack.inFire",
            Self::FireTick => "death.attack.onFire",
            Self::Lava => "death.attack.lava",
            Self::Drowning => "death.attack.drown",
            Self::Suffocation => "death.attack.inWall",
            Self::Explosion => "death.attack.explosion",
            Self::Void => "death.attack.outOfWorld",
            Self::Magic => "death.attack.magic",
            Self::Starvation => "death.attack.starve",
            Self::Contact => "death.attack.cactus",
        }
    }
}

/// Ticks a weaker hit does nothing after a hit (PMMP `EntityDamageEvent::$attackCooldown`).
const ATTACK_COOLDOWN: u32 = 10;
/// Ticks a respawned player ignores damage (PMMP `Player::actuallyRespawn`).
const RESPAWN_INVULNERABLE_TICKS: u32 = 60;
/// Horizontal knockback of a hit (PMMP `Living::DEFAULT_KNOCKBACK_FORCE`).
const KNOCKBACK: f64 = 0.4;
/// The most upward knockback (PMMP `Living::DEFAULT_KNOCKBACK_VERTICAL_LIMIT`).
const KNOCKBACK_VERTICAL_LIMIT: f64 = 0.4;
/// How far a player reaches to hit (PMMP `Player::MAX_REACH_DISTANCE_ENTITY_INTERACTION`).
const MAX_REACH: f64 = 8.0;
/// How far behind the look direction a target may be (PMMP `canInteract` `M_SQRT3 / 2`).
const MAX_REACH_BEHIND: f64 = 0.866_025_403_784_438_6;
/// Damage the void deals each tick (PMMP `Entity::entityBaseTick`).
const VOID_DAMAGE: f64 = 10.0;
/// Below this feet height the void hurts: 16 under the lowest block.
const VOID_Y: f64 = (world::MIN_Y - 16) as f64;
/// Ticks before burning hurts again (PMMP `Entity::doOnFireTick`).
const FIRE_DAMAGE_TICKS: u16 = 20;
/// The longest burn (PMMP `Limits::INT16_MAX`).
const MAX_FIRE_TICKS: u32 = 32_767;
/// The highest the summed protection factor counts (PMMP `applyDamageModifiers`).
const MAX_EPF: u32 = 25;
/// Half a player's height: hit sounds play there (PMMP `attackEntity`'s `$soundPos`).
const HALF_HEIGHT: f64 = 0.9;

const ARMOR_WEAR: u64 = random::tag("armor-wear");
const PROTECTION: u64 = random::tag("protection");
const THORNS: u64 = random::tag("thorns");
const TOOL_WEAR: u64 = random::tag("tool-wear");

/// A hit about to land: the cause, the base damage and the modifiers the attacker's side adds
/// (strength, weakness, weapon enchantments, a critical hit).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Hit {
    cause: Damage,
    base: f64,
    extra: f64,
    /// The player whose hit this is: the victim is knocked back from it, and thorns answer it.
    attacker: Option<SessionId>,
    /// Whether thorns may answer this hit (not a thorns hit itself).
    thorns: bool,
}

/// What [`assess`] decided for a hit that lands.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Landed {
    hit: Hit,
    /// The damage after every modifier.
    final_damage: f64,
    /// The absorption the hit used (0 or negative).
    absorption: f64,
    /// Whether a totem keeps the player alive.
    totem: bool,
}

/// Deals `amount` damage for `cause`; false if no damage was taken.
pub(super) fn damage(state: &mut State, session: SessionId, amount: f64, cause: Damage) -> bool {
    if !amount.is_finite() || amount < 0.0 {
        return false;
    }
    let attacker = match cause {
        Damage::Attack(a) => Some(a),
        _ => None,
    };
    let extra = attacker.map_or(0.0, |a| attacker_modifiers(state, a, amount, cause));
    hurt(
        state,
        session,
        Hit {
            cause,
            base: amount,
            extra,
            attacker,
            thorns: attacker.is_some(),
        },
    )
}

/// The attacker's strength and weakness modifiers (PMMP
/// `EntityDamageByEntityEvent::addAttackerModifiers`).
fn attacker_modifiers(state: &State, attacker: SessionId, base: f64, cause: Damage) -> f64 {
    let Some(a) = state.players.get(&attacker) else {
        return 0.0;
    };
    let strength = f64::from(a.survival.effect_level(Effect::Strength));
    let weakness = f64::from(a.survival.effect_level(Effect::Weakness));
    let mut extra = base * 0.3 * strength;
    if matches!(cause, Damage::Attack(_)) {
        extra -= base * 0.2 * weakness;
    }
    extra
}

/// Checks and applies a hit.
fn hurt(state: &mut State, session: SessionId, hit: Hit) -> bool {
    match assess(state, session, hit) {
        Some(landed) => {
            apply(state, session, landed);
            true
        }
        None => false,
    }
}

/// Whether a hit lands and how much it deals (PMMP `Player::attack`, `Living::attack`,
/// `Living::applyDamageModifiers`, `Human::applyDamageModifiers`); `None` if it is cancelled.
fn assess(state: &mut State, session: SessionId, hit: Hit) -> Option<Landed> {
    let tick = state.tick;
    let seed = world_seed(state, session);
    let settings = &state.settings;
    let player = state.players.get_mut(&session)?;
    if player.phase != Phase::Spawned {
        return None;
    }
    let mode = game_mode_of(settings, player);
    let s = &player.survival;
    if s.is_dead()
        || mode == GameMode::Creative
        || (mode == GameMode::Spectator && hit.cause != Damage::Void)
        || s.no_damage_ticks > 0
        || (hit.cause.is_fire() && s.effects.contains_key(&Effect::FireResistance))
    {
        return None;
    }
    let base = hit.base;
    let mut sum = hit.extra;
    let total = |sum: f64| {
        let v = base + sum;
        if v > 0.0 { v } else { 0.0 }
    };
    if s.attack_time > 0
        && let Some(last) = s.last_damage
    {
        if last >= base {
            return None;
        }
        sum -= last;
    }
    if hit.cause.reducible() {
        sum -= total(sum) * f64::from(armor_points(player)) * 0.04;
    }
    let resistance = f64::from(s.effect_level(Effect::Resistance));
    if resistance > 0.0 && hit.cause != Damage::Void {
        sum -= total(sum) * math::min(1.0, 0.2 * resistance);
    }
    let epf = protection_factor(player, hit.cause).min(MAX_EPF);
    if epf > 0 {
        let mut r = player_random(seed, tick, player, PROTECTION);
        let roll = f64::from(r.range(50, 100)) / 100.0;
        let reduction = math::min((f64::from(epf) * roll).ceil(), 20.0);
        sum -= total(sum) * reduction * 0.04;
    }
    let s = &player.survival;
    let absorption = -math::min(s.absorption, total(sum));
    sum += absorption;
    let mut totem = false;
    if hit.cause != Damage::Void && holds_totem(player) {
        let compensation = s.health - total(sum) - 1.0;
        if compensation <= -1.0 {
            sum += compensation;
            totem = true;
        }
    }
    Some(Landed {
        hit,
        final_damage: total(sum),
        absorption,
        totem,
    })
}

/// Applies a hit that landed (PMMP `Entity::attack` → `setHealth`, then `Living::attack`'s
/// cooldown, knockback, hurt animation and `applyPostDamageEffects`).
fn apply(state: &mut State, session: SessionId, landed: Landed) {
    let Landed {
        hit,
        final_damage,
        absorption,
        totem,
    } = landed;
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let s = &mut player.survival;
    s.last_damage = Some(hit.base);
    let health = s.health - final_damage;
    s.health = if health > 0.0 { health } else { 0.0 };
    s.mark(HEALTH);
    player.data_dirty = true;
    let cold = s.attack_time == 0;
    if cold {
        s.attack_time = ATTACK_COOLDOWN;
    }
    if player.survival.is_dead() {
        die(state, session, hit.cause);
        return;
    }
    if cold {
        if let Some(attacker) = hit.attacker {
            knock_back_from(state, session, attacker, KNOCKBACK);
        }
        let packet = actor_event(player_runtime_id(state, session), ActorEvent::Hurt, 0);
        broadcast_from(state, session, &packet);
    }
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let s = &mut player.survival;
    let left = s.absorption + absorption;
    s.absorption = if left > 0.0 { left } else { 0.0 };
    s.mark(ABSORPTION);
    if hit.cause.reducible() {
        damage_armor(state, session, hit.base);
    }
    if hit.thorns
        && let Some(attacker) = hit.attacker
    {
        thorns(state, session, attacker);
    }
    if let Some(player) = state.players.get_mut(&session) {
        food::exhaust(&mut player.survival, 0.1);
    }
    if totem {
        use_totem(state, session);
    }
    clock::wake(state, session);
}

/// The runtime ID of a player, 0 if it is gone.
fn player_runtime_id(state: &State, session: SessionId) -> u64 {
    state.players.get(&session).map_or(0, |p| p.runtime_id)
}

/// An `ActorEvent` for an entity.
fn actor_event(runtime_id: u64, event: ActorEvent, data: i32) -> Packet {
    Packet::ActorEvent(packet::ActorEvent {
        entity_runtime_id: runtime_id,
        event_type: u8::from(event),
        event_data: data,
        fire_at_position: None,
    })
}

/// Heals a player (PMMP `Entity::heal`): health up to its maximum.
pub(super) fn heal(state: &mut State, session: SessionId, amount: f64) {
    if let Some(player) = state.players.get_mut(&session) {
        heal_player(player, amount);
    }
}

/// Heals a player that is alive.
pub(super) fn heal_player(player: &mut Player, amount: f64) {
    let s = &mut player.survival;
    if s.is_dead() || amount.is_nan() || amount <= 0.0 {
        return;
    }
    let max = s.max_health();
    if s.health < max {
        let health = s.health + amount;
        s.health = if health > max { max } else { health };
        s.mark(HEALTH);
        player.data_dirty = true;
    }
}

/// The armour points the player wears (PMMP `Living::getArmorPoints`).
fn armor_points(player: &Player) -> u32 {
    player
        .inventory
        .armor()
        .iter()
        .filter_map(Stack::item)
        .map(|i| u32::from(i.defense_points()))
        .sum()
}

/// The level of an enchantment on a stack (PMMP `getEnchantmentLevel`), 0 if absent.
pub(super) fn level_of(stack: &Stack, enchantment: Enchantment) -> u32 {
    stack
        .extra()
        .and_then(|e| e.enchantments.iter().find(|(x, _)| *x == enchantment))
        .map_or(0, |&(_, level)| u32::from(level))
}

/// The highest level of an enchantment on the armour worn (PMMP
/// `Living::getHighestArmorEnchantmentLevel`).
pub(super) fn highest_armor_level(player: &Player, enchantment: Enchantment) -> u32 {
    player
        .inventory
        .armor()
        .iter()
        .map(|s| level_of(s, enchantment))
        .max()
        .unwrap_or(0)
}

/// The protection factor of the armour for a cause (PMMP `Armor::getEnchantmentProtectionFactor`,
/// `ProtectionEnchantment::getProtectionFactor`): ⌊(6 + level²) × type modifier / 3⌋ for each
/// protection enchantment that applies.
fn protection_factor(player: &Player, cause: Damage) -> u32 {
    let applies = |e: Enchantment| match e {
        Enchantment::Protection => Some(0.75),
        Enchantment::FireProtection => cause.is_fire().then_some(1.25),
        Enchantment::FeatherFalling => (cause == Damage::Fall).then_some(2.5),
        Enchantment::BlastProtection => (cause == Damage::Explosion).then_some(1.5),
        Enchantment::ProjectileProtection => (cause == Damage::Entity).then_some(1.5),
        _ => None,
    };
    let mut total: u32 = 0;
    for stack in player.inventory.armor() {
        if stack.item().and_then(Type::armor).is_none() {
            continue;
        }
        let Some(extra) = stack.extra() else {
            continue;
        };
        for &(e, level) in &extra.enchantments {
            if let Some(modifier) = applies(e) {
                total = total.saturating_add(epf(level, modifier));
            }
        }
    }
    total
}

/// One enchantment's protection factor.
pub(super) fn epf(level: u8, modifier: f64) -> u32 {
    let l = f64::from(level);
    let v = ((6.0 + l * l) * modifier / 3.0).floor();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "invariant: (6 + 255²) × 2.5 / 3 is far below u32::MAX and positive"
    )]
    let v = v as u32;
    v
}

/// Whether a stack is a totem of undying.
fn is_totem(stack: &Stack) -> bool {
    stack.item().is_some_and(|i| i.is(Class::Totem))
}

/// Whether the player holds a totem in its main hand or its off hand (PMMP
/// `Human::applyDamageModifiers`).
fn holds_totem(player: &Player) -> bool {
    is_totem(player.inventory.held()) || is_totem(player.inventory.offhand())
}

/// A totem saved the player (PMMP `Human::applyPostDamageEffects`): effects cleared, then
/// regeneration II and fire resistance II for 40 s and absorption II for 5 s, the totem
/// animation and sound; the totem in the main hand, else the one in the off hand, is used up.
fn use_totem(state: &mut State, session: SessionId) {
    effects::clear(state, session);
    for (effect, active) in [
        (Effect::Regeneration, Active::new(40 * 20, 1)),
        (Effect::FireResistance, Active::new(40 * 20, 1)),
        (Effect::Absorption, Active::new(5 * 20, 1)),
    ] {
        effects::add(state, session, effect, active);
    }
    let packet = actor_event(
        player_runtime_id(state, session),
        ActorEvent::TalismanActivate,
        0,
    );
    broadcast_from(state, session, &packet);
    sound::level_event(state, session, LevelEvent::SoundTotemUsed);
    if let Some(player) = state.players.get_mut(&session) {
        let held = player.inventory.held().clone();
        let off = player.inventory.offhand().clone();
        if is_totem(&held) {
            inventory::set_held(player, held.with_count(held.count().saturating_sub(1)));
        } else if is_totem(&off) {
            inventory::set_offhand(player, off.with_count(off.count().saturating_sub(1)));
        }
    }
}

/// Wears a durable stack by `amount` (PMMP `Durable::applyDamage`): unbreaking negates each
/// point with chance 1 - 1/(level + 1), for armour only 40 % of the time; a stack worn out
/// breaks (loses one item). `None` if the stack does not change.
pub(super) fn wear(stack: &Stack, amount: u32, armor: bool, r: &mut Random) -> Option<Stack> {
    let max = stack.item()?.max_durability()?;
    if stack.damage() >= max
        || stack
            .extra()
            .is_some_and(|e| matches!(e.nbt.get("Unbreakable"), Some(crate::nbt::Value::Byte(1))))
    {
        return None;
    }
    let amount = amount.min(u32::from(max));
    let unbreaking = level_of(stack, Enchantment::Unbreaking);
    let mut negated = 0;
    if unbreaking > 0 {
        let chance = 1.0 / f64::from(unbreaking + 1);
        for _ in 0..amount {
            let applies = !armor || r.range(1, 100) > 60;
            if applies && r.unit() > chance {
                negated += 1;
            }
        }
    }
    let amount = amount - negated;
    if amount == 0 {
        return None;
    }
    let damage = u32::from(stack.damage()) + amount;
    if damage >= u32::from(max) {
        Some(
            stack
                .with_count(stack.count().saturating_sub(1))
                .with_damage(0),
        )
    } else {
        Some(stack.with_damage(u16::try_from(damage).unwrap_or(max)))
    }
}

/// Wears the armour after a hit (PMMP `Living::damageArmor`): ⌊damage / 4⌋ points, at least 1.
fn damage_armor(state: &mut State, session: SessionId, base: f64) {
    let tick = state.tick;
    let seed = world_seed(state, session);
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let points = (base / 4.0).floor();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "invariant: a finite non-negative value saturates to the u32 range"
    )]
    let points = if points >= 1.0 { points as u32 } else { 1 };
    let mut r = player_random(seed, tick, player, ARMOR_WEAR);
    let mut broke = false;
    for slot in 0..4 {
        let Some(stack) = player.inventory.armor().get(slot) else {
            continue;
        };
        if stack.item().and_then(Type::armor).is_none() {
            continue;
        }
        if let Some(worn) = wear(stack, points, true, &mut r) {
            broke |= worn.count() < stack.count();
            inventory::set_armor(player, slot, worn);
        }
    }
    if broke {
        sound::plain(state, session, SoundEvent::Break, -1);
    }
}

/// Thorns on the victim's armour hurts the attacker (PMMP `Living::applyPostDamageEffects`): each
/// piece with level L answers with chance 15 % × L, dealing 1 to 4 (L - 10 above level 10) and
/// wearing the piece by 3, else wearing it by 1. A thorns hit is not answered again: PMMP lets
/// two thorns wearers trade hits for as long as the cooldowns allow.
fn thorns(state: &mut State, victim: SessionId, attacker: SessionId) {
    let tick = state.tick;
    let seed = world_seed(state, victim);
    let Some(player) = state.players.get_mut(&victim) else {
        return;
    };
    let mut r = player_random(seed, tick, player, THORNS);
    let mut dealt = 0.0;
    let mut broke = false;
    for slot in 0..4 {
        let Some(stack) = player.inventory.armor().get(slot) else {
            continue;
        };
        if stack.item().and_then(Type::armor).is_none() {
            continue;
        }
        let level = level_of(stack, Enchantment::Thorns);
        if level == 0 {
            continue;
        }
        let (wear_by, damage) = if u32::try_from(r.range(0, 99)).unwrap_or(0) < level * 15 {
            let damage = if level > 10 {
                f64::from(level - 10)
            } else {
                f64::from(1 + r.range(0, 3))
            };
            (3, damage)
        } else {
            (1, 0.0)
        };
        dealt += damage;
        if let Some(worn) = wear(stack, wear_by, true, &mut r) {
            broke |= worn.count() < stack.count();
            inventory::set_armor(player, slot, worn);
        }
    }
    if broke {
        sound::plain(state, victim, SoundEvent::Break, -1);
    }
    if dealt > 0.0 {
        hurt(
            state,
            attacker,
            Hit {
                cause: Damage::Magic,
                base: dealt,
                extra: 0.0,
                attacker: Some(victim),
                thorns: false,
            },
        );
    }
}

/// Knocks the player back away from another player (PMMP `Living::knockBack`): the motion the
/// client is sent (players have no server-side motion, PMMP `Player::setMotion`).
fn knock_back_from(state: &mut State, session: SessionId, from: SessionId, force: f64) {
    let Some(source) = state.players.get(&from).map(feet) else {
        return;
    };
    let Some(target) = state.players.get(&session).map(feet) else {
        return;
    };
    knock_back(
        state,
        session,
        target.x - source.x,
        target.z - source.z,
        force,
    );
}

/// Sends the knockback motion along (x, z).
fn knock_back(state: &mut State, session: SessionId, x: f64, z: f64, force: f64) {
    let length = (x * x + z * z).sqrt();
    if length.is_nan() || length <= 0.0 {
        return;
    }
    let f = 1.0 / length;
    let motion = Vec3::new(
        x * f * force,
        math::min(force, KNOCKBACK_VERTICAL_LIMIT),
        z * f * force,
    );
    let packet = Packet::SetActorMotion(SetActorMotion {
        entity_runtime_id: player_runtime_id(state, session),
        velocity: motion.to_f32(),
        tick: 0,
    });
    broadcast_from(state, session, &packet);
}

/// Sets the player on fire for `seconds` (PMMP `Living::setOnFire`): fire protection shortens it
/// by 15 % per level of the best piece; a longer burn replaces a shorter one.
pub(super) fn set_on_fire(player: &mut Player, seconds: u32) {
    let level = f64::from(highest_armor_level(player, Enchantment::FireProtection));
    let s = f64::from(seconds);
    let cut = math::min(s, s * level * 0.15).floor();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "invariant: 0 <= cut <= seconds, which fits u32"
    )]
    let seconds = seconds - cut as u32;
    let ticks = seconds.saturating_mul(20).min(MAX_FIRE_TICKS);
    let ticks = u16::try_from(ticks).unwrap_or(u16::MAX);
    if ticks > player.survival.fire_ticks {
        player.survival.fire_ticks = ticks;
        player.data_dirty = true;
    }
}

/// `attacker` hits `target` (PMMP `Player::attackEntity`): the held item's attack points, the
/// attacker's effects, sharpness, a critical hit (falling, not sprinting, flying, blind or under
/// water) for half again; cancelled out of reach, in spectator mode or with `pvp = false`.
pub(super) fn attack_player(state: &mut State, attacker: SessionId, target: SessionId) {
    if attacker == target {
        return;
    }
    let Some(hit) = melee_hit(state, attacker, target) else {
        if state.players.contains_key(&attacker) {
            swing(state, attacker);
            hit_sound(state, target, SoundEvent::AttackNoDamage);
        }
        return;
    };
    let critical = hit.1;
    let magic = hit.2;
    let landed = hurt(state, target, hit.0);
    swing(state, attacker);
    if !landed {
        hit_sound(state, target, SoundEvent::AttackNoDamage);
        return;
    }
    hit_sound(state, target, SoundEvent::AttackStrong);
    let target_runtime = player_runtime_id(state, target);
    for (on, action) in [
        (critical, AnimateAction::CriticalHit),
        (magic, AnimateAction::MagicCriticalHit),
    ] {
        if on {
            let packet = Packet::Animate(Animate {
                action_type: u8::from(action),
                entity_runtime_id: target_runtime,
                data: 0.0,
                swing_source: 0,
            });
            broadcast_from(state, target, &packet);
        }
    }
    let held = state
        .players
        .get(&attacker)
        .map(|p| p.inventory.held().clone())
        .unwrap_or_default();
    let knockback = level_of(&held, Enchantment::Knockback);
    if knockback > 0 {
        knock_back_from(state, target, attacker, f64::from(knockback) * 0.5);
    }
    let fire = level_of(&held, Enchantment::FireAspect);
    if fire > 0
        && let Some(player) = state.players.get_mut(&target)
    {
        set_on_fire(player, fire * 4);
    }
    wear_weapon(state, attacker);
    if let Some(player) = state.players.get_mut(&attacker)
        && !player.survival.is_dead()
    {
        food::exhaust(&mut player.survival, 0.1);
        player.data_dirty = true;
    }
}

/// A hit sound at the middle of the player `target` (PMMP `attackEntity`: `EntityAttackSound`,
/// or `EntityAttackNoDamageSound` for a hit that did nothing).
fn hit_sound(state: &mut State, target: SessionId, sound: SoundEvent) {
    let Some(at) = state.players.get(&target).map(feet) else {
        return;
    };
    sound::hit(state, target, at + Vec3::new(0.0, HALF_HEIGHT, 0.0), sound);
}

/// The hit of a melee attack and whether it is critical and enchanted; `None` when the attack
/// cannot happen.
fn melee_hit(state: &State, attacker: SessionId, target: SessionId) -> Option<(Hit, bool, bool)> {
    let a = state.players.get(&attacker)?;
    let t = state.players.get(&target)?;
    if a.phase != Phase::Spawned
        || t.phase != Phase::Spawned
        || a.world != t.world
        || a.survival.is_dead()
        || t.survival.is_dead()
        || !state.settings.pvp
        || game_mode_of(&state.settings, a) == GameMode::Spectator
        || !can_reach(a, feet(t))
    {
        return None;
    }
    let (base, extra, critical, magic) = melee_parts(state, attacker, a);
    Some((
        Hit {
            cause: Damage::Attack(attacker),
            base,
            extra,
            attacker: Some(attacker),
            thorns: true,
        },
        critical,
        magic,
    ))
}

/// The attacker's side of a melee hit by `a` (PMMP `Player::attackEntity`): the held item's
/// attack points, the extra of Strength, Weakness and Sharpness, a critical hit (falling, not
/// sprinting or flying, not blind, not under water) for half again; whether it is critical and
/// whether a weapon enchantment added damage.
fn melee_parts(state: &State, attacker: SessionId, a: &Player) -> (f64, f64, bool, bool) {
    let held = a.inventory.held();
    let base = f64::from(held.item().map_or(1, Type::attack_points));
    let mut extra = attacker_modifiers(state, attacker, base, Damage::Attack(attacker));
    let sharpness = level_of(held, Enchantment::Sharpness);
    let weapon = if sharpness > 0 {
        0.5 * f64::from(sharpness + 1)
    } else {
        0.0
    };
    extra += weapon;
    let s = &a.survival;
    let critical = !s.motion.sprinting
        && !s.motion.flying
        && s.fall_distance > 0.0
        && !s.effects.contains_key(&Effect::Blindness)
        && !movement::is_underwater(state, a);
    if critical {
        let total = base + extra;
        extra += if total > 0.0 { total / 2.0 } else { 0.0 };
    }
    (base, extra, critical, weapon > 0.0)
}

/// The melee hit the player of `session` makes on an entity that is not a player; `None` if it
/// cannot hit (not spawned, dead or a spectator).
pub(super) fn melee(state: &State, session: SessionId) -> Option<super::Melee> {
    let a = state.players.get(&session)?;
    if a.phase != Phase::Spawned
        || a.survival.is_dead()
        || game_mode_of(&state.settings, a) == GameMode::Spectator
    {
        return None;
    }
    let (base, extra, critical, magic) = melee_parts(state, session, a);
    let total = base + extra;
    Some(super::Melee {
        damage: if total > 0.0 { total } else { 0.0 },
        critical,
        magic,
    })
}

/// Whether the player reaches a position (PMMP `Player::canInteract`): within 8 blocks of the
/// eyes and not behind the look direction.
fn can_reach(player: &Player, pos: Vec3) -> bool {
    let eye = player.position;
    if eye.distance_squared(pos) > MAX_REACH * MAX_REACH {
        return false;
    }
    let d = movement::direction(&player.survival);
    d.dot(pos) - d.dot(eye) >= -MAX_REACH_BEHIND
}

/// The attacker's arm swing, for the players that see it (PMMP `ArmSwingAnimation`).
pub(super) fn swing(state: &mut State, session: SessionId) {
    let packet = actor_event(
        player_runtime_id(state, session),
        ActorEvent::StartAttacking,
        0,
    );
    broadcast_to_viewers(state, session, &packet);
}

/// The attacker's weapon wears from the hit (PMMP `onAttackEntity`): swords, tridents and hoes by
/// 1, axes, pickaxes and shovels by 2.
fn wear_weapon(state: &mut State, session: SessionId) {
    let tick = state.tick;
    let seed = world_seed(state, session);
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if game_mode_of(&state.settings, player) == GameMode::Creative {
        return;
    }
    let held = player.inventory.held().clone();
    let Some(item) = held.item() else {
        return;
    };
    let amount = if item.is(Class::Sword) || item.is(Class::Hoe) || item.name() == "trident" {
        1
    } else if item.is(Class::Axe) || item.is(Class::Pickaxe) || item.is(Class::Shovel) {
        2
    } else {
        return;
    };
    let mut r = player_random(seed, tick, player, TOOL_WEAR);
    if let Some(worn) = wear(&held, amount, false, &mut r) {
        let broke = worn.count() < held.count();
        inventory::set_held(player, worn);
        if broke {
            sound::plain(state, session, SoundEvent::Break, -1);
        }
    }
}

/// The player died (PMMP `Player::onDeath`): its window closes, it drops what it carries and
/// its experience, viewers see it die and it sees the death screen.
fn die(state: &mut State, session: SessionId, cause: Damage) {
    inventory::close_window(state, session);
    retry_held_back(state, session);
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let finite = game_mode_of(&state.settings, player) != GameMode::Creative;
    let world_name = state
        .worlds
        .get(player.world)
        .map(|w| w.name().to_owned())
        .unwrap_or_default();
    let at = feet(player);
    player.survival.death = Some(Point {
        world: world_name,
        pos: block_at(at),
    });
    player.survival.using = None;
    player.data_dirty = true;
    let world = player.world;
    // While earlier drops still wait for room, the player keeps what it carries: the waiting
    // list then never holds more than one inventory.
    let stacks = if finite && player.survival.held_back.is_empty() {
        inventory::take_all(player)
    } else {
        Vec::new()
    };
    let xp = food::take_death_xp(&mut player.survival, finite);
    let refused = drop_all(stacks, |stack| {
        if entity::drop_item(state, world, at, stack.clone()) {
            None
        } else {
            Some(stack)
        }
    });
    if let Some(player) = state.players.get_mut(&session) {
        let left = give_back(refused, |stack| inventory::give(player, stack));
        player.survival.held_back.extend(left);
    }
    if xp > 0 {
        entity::drop_experience(state, world, at, xp);
    }
    clock::wake(state, session);
    let runtime_id = player_runtime_id(state, session);
    broadcast_from(
        state,
        session,
        &actor_event(runtime_id, ActorEvent::Death, 0),
    );
    let attacker = match cause {
        Damage::Attack(a) => Some(a),
        _ => None,
    };
    show_death(state, session, cause.death_message(), attacker);
}

/// The death message of a player that joined dead (the cause was not kept).
pub(super) const GENERIC_DEATH: &str = "death.attack.generic";

/// The death screen (PMMP `DeathPacketHandler::setUp`): the respawn position being searched and
/// the death message (a translation key with the player's and the attacker's names).
pub(super) fn show_death(
    state: &mut State,
    session: SessionId,
    message: &'static str,
    attacker: Option<SessionId>,
) {
    let (_, respawn_at) = respawn_point(state, session);
    let attacker_name =
        attacker.and_then(|a| state.players.get(&a).map(|p| p.identity.name.clone()));
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    player.survival.motion.death_shown = true;
    let messages = std::iter::once(player.identity.name.clone())
        .chain(attacker_name)
        .collect();
    let runtime_id = player.runtime_id;
    player.send(respawn_packet(
        respawn_at,
        RespawnState::SearchingForSpawn,
        runtime_id,
    ));
    player.send(Packet::DeathInfo(DeathInfo {
        cause: message.to_owned(),
        messages,
    }));
}

/// Whether a stack stays with its owner on death (PMMP `Item::keepOnDeath`).
fn keeps_on_death(stack: &Stack) -> bool {
    stack.extra().is_some_and(|e| {
        matches!(
            e.nbt.get("minecraft:keep_on_death"),
            Some(crate::nbt::Value::Byte(1))
        )
    })
}

/// Drops a dead player's stacks (PMMP `Player::onDeath`, `World::dropItem`): curse of vanishing
/// destroys its items, as PMMP does; items kept on death and the stacks `drop` refuses (the
/// world's entity limit) are returned.
pub(super) fn drop_all(
    stacks: Vec<Stack>,
    mut drop: impl FnMut(Stack) -> Option<Stack>,
) -> Vec<Stack> {
    let mut refused = Vec::new();
    for stack in stacks {
        if stack.is_empty() || level_of(&stack, Enchantment::Vanishing) > 0 {
            continue;
        }
        if keeps_on_death(&stack) {
            refused.push(stack);
        } else if let Some(stack) = drop(stack) {
            refused.push(stack);
        }
    }
    refused
}

/// Puts refused stacks back with `give` (which returns what did not fit); returns what is left,
/// at most as many stacks as were refused. Nothing is lost or duplicated: what is left waits in
/// the player's held-back list and drops as soon as the world takes it ([`retry_held_back`]).
pub(super) fn give_back(refused: Vec<Stack>, mut give: impl FnMut(Stack) -> Stack) -> Vec<Stack> {
    refused
        .into_iter()
        .map(&mut give)
        .filter(|left| !left.is_empty())
        .collect()
}

/// Drops held-back death drops at the player's feet as the world takes them. The list holds at
/// most one death's stacks (one inventory): a player with stacks waiting keeps its inventory
/// when it dies again.
pub(super) fn retry_held_back(state: &mut State, session: SessionId) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if player.survival.held_back.is_empty() {
        return;
    }
    let (world, at) = (player.world, feet(player));
    while let Some(stack) = state
        .players
        .get_mut(&session)
        .and_then(|p| p.survival.held_back.pop())
    {
        if !entity::drop_item(state, world, at, stack.clone()) {
            if let Some(player) = state.players.get_mut(&session) {
                player.survival.held_back.push(stack);
            }
            return;
        }
        if let Some(player) = state.players.get_mut(&session) {
            player.data_dirty = true;
        }
    }
}

/// A `Respawn` packet.
fn respawn_packet(position: Vec3, state: RespawnState, runtime_id: u64) -> Packet {
    Packet::Respawn(Respawn {
        position: position.to_f32(),
        state: u8::from(state),
        entity_runtime_id: runtime_id,
    })
}

/// The world and eye position the player respawns at (PMMP `Player::getSpawn`): its spawn point
/// (a bed) in whichever world holds it, else the default world's spawn; the block's centre,
/// moved up to the first two free blocks of the loaded chunks (PMMP `World::getSafeSpawn`). A
/// spawn point in a world the server no longer runs counts as none.
fn respawn_point(state: &State, session: SessionId) -> (WorldIndex, Vec3) {
    let Some(player) = state.players.get(&session) else {
        return (0, Vec3::ZERO);
    };
    let own = player.survival.spawn.as_ref().and_then(|p| {
        let index = state.worlds.iter().position(|w| w.name() == p.world)?;
        Some((index, p.pos))
    });
    let Some((index, spawn)) = own.or_else(|| state.worlds.first().map(|w| (0, w.spawn()))) else {
        return (player.world, player.position);
    };
    let Some(world) = state.worlds.get(index) else {
        return (player.world, player.position);
    };
    let mut y = spawn.y;
    for _ in 0..world::MAX_Y.saturating_sub(world::MIN_Y) {
        let free = |y: i32| {
            world
                .block(BlockPos::new(spawn.x, y, spawn.z))
                .is_none_or(|b| !b.is_solid())
        };
        if free(y) && free(y + 1) {
            break;
        }
        y += 1;
    }
    let eye = Vec3::new(
        f64::from(spawn.x) + 0.5,
        f64::from(y) + EYE_HEIGHT,
        f64::from(spawn.z) + 0.5,
    );
    (index, eye)
}

/// `Respawn` from the client: on the death screen, the client is ready to spawn and gets the
/// respawn position (PMMP `DeathPacketHandler::handleRespawn`). Anything else is ignored.
pub(super) fn on_respawn(state: &mut State, session: SessionId, packet: &Respawn) {
    let dead = state
        .players
        .get(&session)
        .is_some_and(|p| p.phase == Phase::Spawned && p.survival.is_dead());
    if !dead || packet.state != u8::from(RespawnState::ClientReadyToSpawn) {
        return;
    }
    let (_, position) = respawn_point(state, session);
    if let Some(player) = state.players.get_mut(&session) {
        let runtime_id = player.runtime_id;
        player.send(respawn_packet(
            position,
            RespawnState::ReadyToSpawn,
            runtime_id,
        ));
    }
}

/// The player respawns (PMMP `Player::respawn`, `actuallyRespawn`, `NetworkSession::
/// onServerRespawn`) at its respawn point, in another world if the point is there; nothing
/// unless it is dead. In hardcore the player comes back as a spectator, as vanilla does (PMMP
/// bans the player instead; Anchor has no ban list).
pub(super) fn respawn(state: &mut State, session: SessionId) {
    let hardcore = state.settings.hardcore;
    let dead = state
        .players
        .get(&session)
        .is_some_and(|p| p.phase == Phase::Spawned && p.survival.is_dead());
    if !dead {
        return;
    }
    let (world, position) = respawn_point(state, session);
    if hardcore && let Some(player) = state.players.get_mut(&session) {
        player.survival.game_mode = Some(GameMode::Spectator);
        player.send(Packet::SetPlayerGameType(SetPlayerGameType {
            game_type: super::saved::game_mode_id(GameMode::Spectator),
        }));
    }
    super::teleport(state, session, world, position);
    effects::clear(state, session);
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let s = &mut player.survival;
    s.motion.sprinting = false;
    s.motion.sneaking = false;
    s.motion.flying = false;
    s.motion.death_shown = false;
    s.fire_ticks = 0;
    s.air = MAX_AIR;
    s.breathing = true;
    s.no_damage_ticks = RESPAWN_INVULNERABLE_TICKS;
    s.attack_time = 0;
    s.last_damage = None;
    s.fall_distance = 0.0;
    s.health = s.max_health();
    s.food = super::MAX_FOOD;
    s.saturation = super::DEFAULT_SATURATION;
    s.exhaustion = 0.0;
    s.absorption = 0.0;
    player.data_dirty = true;
    let Some(player) = state.players.get(&session) else {
        return;
    };
    let mut out = vec![
        attributes::all_attributes(player),
        attributes::update_abilities(&state.settings, player),
    ];
    inventory::spawn_packets(player, &mut out);
    let data = Packet::SetActorData(SetActorData {
        entity_runtime_id: player.runtime_id,
        entity_metadata: attributes::full_metadata(&state.settings, player),
        entity_properties: EntityProperties::default(),
        tick: 0,
    });
    let alive = actor_event(player.runtime_id, ActorEvent::SpawnAlive, 0);
    let now = attributes::shown(&state.settings, player);
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    for packet in out {
        player.send(packet);
    }
    player.survival.dirty = 0;
    player.survival.shown = now;
    broadcast_from(state, session, &data);
    broadcast_from(state, session, &alive);
}

/// The void, burning and the respawn invulnerability, each tick (PMMP `Entity::entityBaseTick`,
/// `Entity::doOnFireTick`, `Player::onUpdate`).
pub(super) fn base_tick(state: &mut State, session: SessionId) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let void = feet(player).y <= VOID_Y;
    let creative = game_mode_of(&state.settings, player) == GameMode::Creative;
    let s = &mut player.survival;
    let mut burn = false;
    if s.fire_ticks > 0 {
        s.fire_ticks -= 1;
        burn = s.fire_ticks % FIRE_DAMAGE_TICKS == 0;
        player.data_dirty = true;
    }
    if s.no_damage_ticks > 0 {
        s.no_damage_ticks -= 1;
    }
    if void {
        damage(state, session, VOID_DAMAGE, Damage::Void);
    }
    if burn {
        damage(state, session, 1.0, Damage::FireTick);
    }
    if creative
        && let Some(player) = state.players.get_mut(&session)
        && player.survival.fire_ticks > 1
    {
        player.survival.fire_ticks = 1;
    }
}

/// Suffocation, air and the hit cooldown, each tick (PMMP `Living::entityBaseTick`,
/// `Living::doAirSupplyTick`).
pub(super) fn living_tick(state: &mut State, session: SessionId) {
    let tick = state.tick;
    let seed = world_seed(state, session);
    let Some(player) = state.players.get(&session) else {
        return;
    };
    if player.survival.is_dead() {
        return;
    }
    let creative = game_mode_of(&state.settings, player) == GameMode::Creative;
    let inside = movement::is_inside_solid(state, player);
    let breathes = creative
        || player
            .survival
            .effects
            .contains_key(&Effect::WaterBreathing)
        || player.survival.effects.contains_key(&Effect::ConduitPower)
        || !movement::is_underwater(state, player);
    if inside {
        damage(state, session, 1.0, Damage::Suffocation);
    }
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let respiration = player
        .inventory
        .armor()
        .first()
        .map_or(0, |helmet| level_of(helmet, Enchantment::Respiration));
    let mut drown = false;
    if !breathes {
        let loses = respiration == 0 || {
            let mut r = player_random(seed, tick, player, random::tag("respiration"));
            r.unit() <= 1.0 / f64::from(respiration + 1)
        };
        let s = &mut player.survival;
        s.breathing = false;
        if loses {
            s.air -= 1;
            if s.air <= -20 {
                s.air = 0;
                drown = true;
            }
            player.data_dirty = true;
        }
    } else if !player.survival.breathing {
        let s = &mut player.survival;
        if s.air < MAX_AIR {
            s.air = s.air.saturating_add(5);
        }
        if s.air >= MAX_AIR {
            s.air = MAX_AIR;
            s.breathing = true;
        }
        player.data_dirty = true;
    }
    if player.survival.attack_time > 0 {
        player.survival.attack_time -= 1;
    }
    if drown {
        damage(state, session, 2.0, Damage::Drowning);
    }
}

/// The fall damage of a landing (PMMP `Living::calculateFallDamage`): the blocks fallen beyond 3,
/// less the jump boost level, rounded up; none while flying.
pub(super) fn fall_damage(s: &Survival, distance: f64) -> f64 {
    if s.motion.flying {
        return 0.0;
    }
    (distance - 3.0 - f64::from(s.effect_level(Effect::JumpBoost))).ceil()
}
