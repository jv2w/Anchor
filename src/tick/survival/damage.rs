//! Damage, healing, melee combat, death and respawn (PMMP `Entity::attack`, `Living::attack`,
//! `Living::applyDamageModifiers`, `Human::applyDamageModifiers`, `Player::attack`,
//! `Player::attackEntity`, `Player::onDeath`, `Player::respawn`, `DeathPacketHandler`).
//!
//! A hit is worked out in two steps, so events can sit between them: [`assess`] checks
//! whether the hit lands and computes its modifiers, [`apply`] changes the state.

use tracing::debug;

use super::attributes::{self, ABSORPTION, HEALTH};
use super::effects::{self, Active};
use super::{
    ENTITY_REACH, EYE_HEIGHT, MAX_FIRE_TICKS, PLAYER_HEIGHT, Point, Survival, broadcast_from,
    broadcast_to_viewers, can_interact, clock, feet, food, movement, player_random, sound,
    world_seed,
};
use crate::config::{CriticalHits, GameMode};
use crate::effect::Type as Effect;
use crate::event::{
    Actor, CombustSource, DamageCause, DamageModifier, DamageSource, EntityCombust, EntityDamage,
    EntityExtinguish, EntityRegainHealth, ExhaustCause, ExtinguishCause, PlayerDeath,
    PlayerRespawn, RegainReason,
};
use crate::item::{Class, Enchantment, Stack, Type};
use crate::math::{self, Vec3};
use crate::player;
use crate::protocol::EntityProperties;
use crate::protocol::consts::{ActorEvent, AnimateAction, RespawnState};
use crate::protocol::packet::{
    self, Animate, DeathInfo, Packet, Respawn, SetActorData, SetActorMotion,
};
use crate::random::{self, Random};
use crate::save::WorldIndex;
use crate::tick::entity::{
    ATTACK_COOLDOWN, FIRE_DAMAGE_TICKS, KNOCKBACK, KNOCKBACK_VERTICAL_LIMIT, SAFE_FALL,
    SUFFOCATION_DAMAGE, VOID_DAMAGE, VOID_Y,
};
use crate::tick::{Phase, Player, SessionId, State, dispatch, entity, interact, inventory};
use crate::world::{self, BlockPos, Sound};

/// Why a player takes damage (PMMP `EntityDamageEvent::CAUSE_*`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::tick) enum Damage {
    /// A melee hit by another player.
    Attack(SessionId),
    /// A melee hit by an entity that is not a player.
    Melee,
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
    /// An explosion (primed TNT).
    Explosion,
    /// An exploding block.
    BlockExplosion,
    /// Below the world.
    Void,
    /// An effect (poison, harming).
    Magic,
    /// Empty food bar.
    Starvation,
    /// Cactus, berry bush.
    Contact,
    /// A falling block landing on the player.
    FallingBlock,
    /// The player killed itself: no modifier, invulnerability or game mode spares it.
    Suicide,
    /// A lightning bolt (no PMMP cause: an [`EntityDamage`] names it custom).
    Lightning,
    /// Damage without a game cause.
    Custom,
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
                | Self::Suicide
        )
    }

    /// The cause an [`EntityDamage`] names.
    fn event_cause(self) -> DamageCause {
        match self {
            Self::Attack(_) | Self::Melee => DamageCause::EntityAttack,
            Self::Entity => DamageCause::Projectile,
            Self::Fall => DamageCause::Fall,
            Self::Fire => DamageCause::Fire,
            Self::FireTick => DamageCause::FireTick,
            Self::Lava => DamageCause::Lava,
            Self::Drowning => DamageCause::Drowning,
            Self::Suffocation => DamageCause::Suffocation,
            Self::Explosion => DamageCause::EntityExplosion,
            Self::BlockExplosion => DamageCause::BlockExplosion,
            Self::Void => DamageCause::Void,
            Self::Magic => DamageCause::Magic,
            Self::Starvation => DamageCause::Starvation,
            Self::Contact => DamageCause::Contact,
            Self::FallingBlock => DamageCause::FallingBlock,
            Self::Suicide => DamageCause::Suicide,
            Self::Lightning | Self::Custom => DamageCause::Custom,
        }
    }

    /// The damage of `cause` dealt by `source` (an action): a melee hit by a player is that
    /// player's attack.
    pub(in crate::tick) fn of(cause: DamageCause, source: DamageSource) -> Self {
        match cause {
            DamageCause::EntityAttack => match source {
                DamageSource::Entity(Actor::Player(p)) => Self::Attack(p.session()),
                _ => Self::Melee,
            },
            DamageCause::Projectile => Self::Entity,
            DamageCause::Contact => Self::Contact,
            DamageCause::Suffocation => Self::Suffocation,
            DamageCause::Fall => Self::Fall,
            DamageCause::Fire => Self::Fire,
            DamageCause::FireTick => Self::FireTick,
            DamageCause::Lava => Self::Lava,
            DamageCause::Drowning => Self::Drowning,
            DamageCause::BlockExplosion => Self::BlockExplosion,
            DamageCause::EntityExplosion => Self::Explosion,
            DamageCause::Void => Self::Void,
            DamageCause::Suicide => Self::Suicide,
            DamageCause::Magic => Self::Magic,
            DamageCause::Custom => Self::Custom,
            DamageCause::Starvation => Self::Starvation,
            DamageCause::FallingBlock => Self::FallingBlock,
        }
    }

    /// BDS's damage cause number, which the hurt event carries ([`bds_cause`]; P6: lightning 24,
    /// which no event cause names).
    pub(in crate::tick) fn bds_cause(self) -> i32 {
        match self {
            Self::Lightning => 24,
            _ => bds_cause(self.event_cause()),
        }
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
            Self::Explosion | Self::BlockExplosion => "death.attack.explosion",
            Self::Void => "death.attack.outOfWorld",
            Self::Magic => "death.attack.magic",
            Self::Starvation => "death.attack.starve",
            Self::Contact => "death.attack.cactus",
            Self::FallingBlock => "death.attack.fallingBlock",
            Self::Melee | Self::Suicide | Self::Lightning | Self::Custom => GENERIC_DEATH,
        }
    }
}

/// BDS's damage cause number of `cause`, which the hurt event of a player or an entity carries
/// (Mojang `ActorDamageCause`; P6: an attack 2, a projectile 3, drowning 9; the others in the
/// same numbering, PMMP's `EntityDamageEvent::CAUSE_*` plus one, 0 for none).
pub(in crate::tick) fn bds_cause(cause: DamageCause) -> i32 {
    match cause {
        DamageCause::Custom => 0,
        DamageCause::Contact => 1,
        DamageCause::EntityAttack => 2,
        DamageCause::Projectile => 3,
        DamageCause::Suffocation => 4,
        DamageCause::Fall => 5,
        DamageCause::Fire => 6,
        DamageCause::FireTick => 7,
        DamageCause::Lava => 8,
        DamageCause::Drowning => 9,
        DamageCause::BlockExplosion => 10,
        DamageCause::EntityExplosion => 11,
        DamageCause::Void => 12,
        DamageCause::Suicide => 13,
        DamageCause::Magic => 14,
        DamageCause::Starvation => 16,
        DamageCause::FallingBlock => 19,
    }
}

/// Ticks a respawned player ignores damage (PMMP `Player::actuallyRespawn`).
const RESPAWN_INVULNERABLE_TICKS: u32 = 60;
/// The highest the summed protection factor counts (PMMP `applyDamageModifiers`).
const MAX_EPF: u32 = 25;
/// Half a player's height: hit sounds play there (PMMP `attackEntity`'s `$soundPos`).
const HALF_HEIGHT: f64 = PLAYER_HEIGHT / 2.0;

const ARMOR_WEAR: u64 = random::tag("armor-wear");
const PROTECTION: u64 = random::tag("protection");
const RESPIRATION: u64 = random::tag("respiration");
const THORNS: u64 = random::tag("thorns");
const TOOL_WEAR: u64 = random::tag("tool-wear");

/// The most a listener may set a hit's knockback force or its upward limit to, either way, in
/// blocks per tick: the client applies the motion as sent, and a packet carries `f32`.
const MAX_KNOCKBACK: f64 = 16.0;

/// The modifiers of a hit in [`DamageModifier`] order, the form [`EntityDamage`] holds; `None`
/// where one does not apply.
type Modifiers = [Option<f64>; DamageModifier::ALL.len()];
/// No modifier applies.
const NO_MODIFIERS: Modifiers = [None; DamageModifier::ALL.len()];

/// Sets `modifier` in `modifiers` to `value`.
fn set(modifiers: &mut Modifiers, modifier: DamageModifier, value: f64) {
    if let Some(slot) = modifiers.get_mut(modifier as usize) {
        *slot = Some(value);
    }
}

/// A hit about to land: the cause, the base damage and the modifiers the attacker's side adds
/// (strength, weakness, weapon enchantments, a critical hit).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Hit {
    cause: Damage,
    base: f64,
    /// The attacker's modifiers added up, in the order the rule adds them.
    extra: f64,
    /// The attacker's modifiers one by one.
    parts: Modifiers,
    /// The player whose hit this is: the victim is knocked back from it, and thorns answer it.
    attacker: Option<SessionId>,
    /// Whether thorns may answer this hit (not a thorns hit itself).
    thorns: bool,
    /// What deals the hit, as the event names it.
    source: DamageSource,
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
    /// Every modifier, as the event shows them.
    modifiers: Modifiers,
    /// The knockback force of a cold hit (PMMP `EntityDamageByEntityEvent::$knockBack`).
    knockback: f64,
    /// The most a cold hit knocks upward.
    vertical_limit: f64,
    /// The ticks within which a weaker hit does nothing.
    cooldown: u32,
}

impl Landed {
    /// The event that announces the hit on `player`.
    fn event(&self, player: player::Id) -> EntityDamage {
        EntityDamage {
            entity: Actor::Player(player),
            cause: self.hit.cause.event_cause(),
            source: self.hit.source,
            base_damage: self.hit.base,
            original_base_damage: self.hit.base,
            modifiers: self.modifiers,
            original_modifiers: self.modifiers,
            attack_cooldown_ticks: self.cooldown,
            knockback: self.knockback,
            vertical_knockback_limit: self.vertical_limit,
            cancelled: false,
        }
    }

    /// Makes the hit what the listeners left in `event`. While the base damage and the modifiers
    /// are the ones the hit started with, its own damage, absorption and totem stand (bit for bit
    /// what no listener gives); otherwise they follow the event (PMMP `applyPostDamageEffects`:
    /// the absorption and totem modifiers). The absorption modifier can only use absorption up,
    /// and the knockback is kept within [`MAX_KNOCKBACK`]: listener values are external input.
    fn change(&mut self, event: &EntityDamage) {
        let bits = |m: &Modifiers| m.map(|v| v.map(f64::to_bits));
        if event.base_damage.to_bits() != self.hit.base.to_bits()
            || bits(&event.modifiers) != bits(&self.modifiers)
        {
            self.hit.base = event.base_damage;
            self.final_damage = event.final_damage();
            self.absorption = math::min(event.modifier(DamageModifier::Absorption), 0.0);
            self.totem = event.modifier(DamageModifier::Totem) < 0.0;
        }
        self.knockback = math::clamp(event.knockback, -MAX_KNOCKBACK, MAX_KNOCKBACK);
        self.vertical_limit = math::clamp(
            event.vertical_knockback_limit,
            -MAX_KNOCKBACK,
            MAX_KNOCKBACK,
        );
        self.cooldown = event.attack_cooldown_ticks;
    }
}

/// Deals `amount` damage for `cause`; false if no damage was taken. A melee hit names its player
/// as the source.
pub(super) fn damage(state: &mut State, session: SessionId, amount: f64, cause: Damage) -> bool {
    let source = match cause {
        Damage::Attack(a) => DamageSource::Entity(Actor::Player(player::Id::new(a))),
        _ => DamageSource::None,
    };
    damage_by(state, session, amount, cause, source)
}

/// Deals `amount` damage for `cause` from `source` (PMMP `Entity::attack` with an
/// `EntityDamageEvent` or one of its subclasses); false if no damage was taken. A player in
/// `source` is the attacker: its Strength counts, the victim is knocked back from it and the
/// victim's thorns answer it.
pub(super) fn damage_by(
    state: &mut State,
    session: SessionId,
    amount: f64,
    cause: Damage,
    source: DamageSource,
) -> bool {
    if !amount.is_finite() || amount < 0.0 {
        return false;
    }
    let attacker = match (cause, source) {
        (Damage::Attack(a), _) => Some(a),
        (_, DamageSource::Entity(Actor::Player(p))) => Some(p.session()),
        _ => None,
    };
    let (extra, parts) = attacker.map_or((0.0, NO_MODIFIERS), |a| {
        attacker_modifiers(state, a, amount, cause)
    });
    hurt(
        state,
        session,
        Hit {
            cause,
            base: amount,
            extra,
            parts,
            attacker,
            thorns: attacker.is_some(),
            source,
        },
    )
    .is_some()
}

/// The attacker's strength and weakness modifiers, added up and one by one (PMMP
/// `EntityDamageByEntityEvent::addAttackerModifiers`).
fn attacker_modifiers(
    state: &State,
    attacker: SessionId,
    base: f64,
    cause: Damage,
) -> (f64, Modifiers) {
    let mut parts = NO_MODIFIERS;
    let Some(a) = state.players.get(&attacker) else {
        return (0.0, parts);
    };
    let strength = f64::from(a.survival.effect_level(Effect::Strength));
    let weakness = f64::from(a.survival.effect_level(Effect::Weakness));
    let up = base * 0.3 * strength;
    let mut extra = up;
    if strength > 0.0 {
        set(&mut parts, DamageModifier::Strength, up);
    }
    if matches!(cause, Damage::Attack(_)) {
        let down = base * 0.2 * weakness;
        extra -= down;
        if weakness > 0.0 {
            set(&mut parts, DamageModifier::Weakness, -down);
        }
    }
    (extra, parts)
}

/// Checks a hit, asks the [`EntityDamage`] listeners and applies it; its modifiers as the
/// listeners left them, `None` if it did not land.
fn hurt(state: &mut State, session: SessionId, hit: Hit) -> Option<Modifiers> {
    let landed = assess(state, session, hit)?;
    let id = player::Id::new(session);
    let Ok(event) = super::ask(
        state,
        || landed.event(id),
        EntityDamage::is_cancelled,
        |state, _| {
            state
                .players
                .get(&session)
                .is_some_and(|p| lands(p, &landed.hit))
        },
    ) else {
        return None;
    };
    let mut landed = landed;
    if let Some(e) = &event {
        landed.change(e);
    }
    apply(state, session, &landed, entity::Raise::Events);
    super::watch(state, event.as_ref());
    Some(event.map_or(landed.modifiers, |e| e.modifiers))
}

/// Deals `amount` damage for `cause` with no source and no events, for the direct API (an explosion
/// through `Context::explode`: a direct API call never re-enters events); false if no damage was
/// taken. What follows from it happens at the player's survival sync, raising its events there: a
/// death (with the generic death message, as `Player::kill`) and a wake.
pub(super) fn damage_quiet(
    state: &mut State,
    session: SessionId,
    amount: f64,
    cause: Damage,
) -> bool {
    if !amount.is_finite() || amount < 0.0 {
        return false;
    }
    let hit = Hit {
        cause,
        base: amount,
        extra: 0.0,
        parts: NO_MODIFIERS,
        attacker: None,
        thorns: false,
        source: DamageSource::None,
    };
    let Some(landed) = assess(state, session, hit) else {
        return false;
    };
    apply(state, session, &landed, entity::Raise::Quiet);
    true
}

/// Whether a hit can land on the player at all (PMMP `Player::attack`, `Living::attack`, the
/// cooldown of `Living::applyDamageModifiers`): spawned and alive, not creative nor just
/// respawned, a spectator only in the void, no fall for a player that may fly, fire resistance
/// against fire, and within the hit cooldown only a stronger hit. Nothing spares a player from
/// [`Damage::Suicide`] but spectator mode outside it, death and fire resistance.
fn lands(player: &Player, hit: &Hit) -> bool {
    let s = &player.survival;
    let mode = s.mode();
    let suicide = hit.cause == Damage::Suicide;
    player.phase == Phase::Spawned
        && !s.is_dead()
        && (suicide || (mode != GameMode::Creative && s.no_damage_ticks == 0))
        && (mode != GameMode::Spectator || matches!(hit.cause, Damage::Void | Damage::Suicide))
        && !(hit.cause == Damage::Fall && s.may_fly())
        && !(hit.cause.is_fire() && s.effects.contains_key(&Effect::FireResistance))
        && (suicide || s.attack_time == 0 || s.last_damage.is_none_or(|last| last < hit.base))
}

/// Whether a hit lands and how much it deals (PMMP `Player::attack`, `Living::attack`,
/// `Living::applyDamageModifiers`, `Human::applyDamageModifiers`); `None` if it is cancelled.
/// Each modifier is kept for the event as well as added up, in PMMP's order. A system the
/// settings turn off hurts no one, whatever deals the hit (`fall-damage`: an ender pearl's fall
/// too; `drowning`), and with `armor = false` worn armour reduces nothing.
fn assess(state: &mut State, session: SessionId, hit: Hit) -> Option<Landed> {
    let tick = state.tick;
    let seed = world_seed(state, session);
    let settings = &state.settings;
    let spared = match hit.cause {
        Damage::Fall => !settings.fall_damage,
        Damage::Drowning | Damage::Suffocation => !settings.drowning,
        _ => false,
    };
    if spared {
        return None;
    }
    let armor = settings.armor;
    let player = state.players.get_mut(&session)?;
    if !lands(player, &hit) {
        return None;
    }
    let base = hit.base;
    let mut sum = hit.extra;
    let mut modifiers = hit.parts;
    let total = |sum: f64| {
        let v = base + sum;
        if v > 0.0 { v } else { 0.0 }
    };
    let mut absorption = 0.0;
    let mut totem = false;
    // PMMP `Living::attack` skips the modifiers of a suicide.
    if hit.cause != Damage::Suicide {
        let s = &player.survival;
        if s.attack_time > 0
            && let Some(last) = s.last_damage
        {
            sum -= last;
            set(
                &mut modifiers,
                DamageModifier::PreviousDamageCooldown,
                -last,
            );
        }
        if armor && hit.cause.reducible() {
            let d = total(sum) * f64::from(armor_points(player)) * 0.04;
            sum -= d;
            set(&mut modifiers, DamageModifier::Armor, -d);
        }
        let resistance = f64::from(s.effect_level(Effect::Resistance));
        if resistance > 0.0 && hit.cause != Damage::Void {
            let d = total(sum) * math::min(1.0, 0.2 * resistance);
            sum -= d;
            set(&mut modifiers, DamageModifier::Resistance, -d);
        }
        let epf = if armor {
            protection_factor(player, hit.cause).min(MAX_EPF)
        } else {
            0
        };
        if epf > 0 {
            let mut r = player_random(seed, tick, player, PROTECTION);
            let roll = f64::from(r.range(50, 100)) / 100.0;
            let reduction = math::min((f64::from(epf) * roll).ceil(), 20.0);
            let d = total(sum) * reduction * 0.04;
            sum -= d;
            set(&mut modifiers, DamageModifier::ArmorEnchantments, -d);
        }
        let s = &player.survival;
        absorption = -math::min(s.absorption, total(sum));
        sum += absorption;
        set(&mut modifiers, DamageModifier::Absorption, absorption);
        let helmet = || {
            player
                .inventory
                .armor()
                .first()
                .is_some_and(|h| h.item().and_then(Type::armor).is_some())
        };
        if armor && hit.cause == Damage::FallingBlock && helmet() {
            let d = total(sum) / 4.0;
            sum -= d;
            set(&mut modifiers, DamageModifier::ArmorHelmet, -d);
        }
        if hit.cause != Damage::Void && holds_totem(player) {
            let compensation = s.health - total(sum) - 1.0;
            if compensation <= -1.0 {
                sum += compensation;
                totem = true;
                set(&mut modifiers, DamageModifier::Totem, compensation);
            }
        }
    }
    Some(Landed {
        hit,
        final_damage: total(sum),
        absorption,
        totem,
        modifiers,
        knockback: KNOCKBACK,
        vertical_limit: KNOCKBACK_VERTICAL_LIMIT,
        cooldown: u32::from(ATTACK_COOLDOWN),
    })
}

/// Applies a hit that landed (PMMP `Entity::attack` → `setHealth`, then `Living::attack`'s
/// cooldown, knockback, hurt animation and `applyPostDamageEffects`). A quiet hit raises no
/// events: its death and wake wait for the player's survival sync, its exhaustion and totem
/// effects go in directly. With `armor = false` worn armour neither wears nor answers with
/// thorns.
fn apply(state: &mut State, session: SessionId, landed: &Landed, raise: entity::Raise) {
    let Landed {
        hit,
        final_damage,
        absorption,
        totem,
        modifiers: _,
        knockback,
        vertical_limit,
        cooldown,
    } = *landed;
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let s = &mut player.survival;
    s.last_damage = Some(hit.base);
    s.last_cause = Some((hit.cause.event_cause(), hit.source));
    let health = s.health - final_damage;
    s.health = if health > 0.0 { health } else { 0.0 };
    s.mark(HEALTH);
    player.data_dirty = true;
    let cold = s.attack_time == 0;
    if cold {
        s.attack_time = cooldown;
    }
    if player.survival.is_dead() {
        if raise == entity::Raise::Quiet {
            player.survival.custom.killed = true;
            return;
        }
        // A projectile names its shooter; one no player shot is a generic death (PMMP
        // `PlayerDeathEvent::deriveMessage`).
        let attacker = match (hit.cause, hit.source) {
            (Damage::Attack(a), _) => Some(a),
            (
                Damage::Entity,
                DamageSource::Child {
                    owner: Some(Actor::Player(p)),
                    ..
                },
            ) => Some(p.session()),
            _ => None,
        };
        let message = match hit.cause {
            Damage::Entity if attacker.is_none() => GENERIC_DEATH,
            Damage::Entity if is_trident(state, hit.source) => "death.attack.trident",
            // Only a cactus is named for contact (PMMP `deriveMessage`).
            Damage::Contact if !is_cactus(state, session, hit.source) => GENERIC_DEATH,
            cause => cause.death_message(),
        };
        die(state, session, message, attacker);
        return;
    }
    if cold {
        if let Some(attacker) = hit.attacker {
            knock_back_from(state, session, attacker, knockback, vertical_limit);
        }
        let packet = actor_event(
            player_runtime_id(state, session),
            ActorEvent::Hurt,
            hit.cause.bds_cause(),
        );
        broadcast_from(state, session, &packet);
    }
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let s = &mut player.survival;
    let left = s.absorption + absorption;
    s.absorption = if left > 0.0 { left } else { 0.0 };
    s.mark(ABSORPTION);
    let armor = state.settings.armor;
    if armor && hit.cause.reducible() {
        damage_armor(state, session, hit.base);
    }
    if armor
        && hit.thorns
        && let Some(attacker) = hit.attacker
    {
        thorns(state, session, attacker);
    }
    if totem {
        use_totem(state, session, raise);
    }
    match raise {
        entity::Raise::Events => {
            food::exhaust_as(state, session, 0.1, ExhaustCause::Damage);
            clock::wake(state, session);
        }
        entity::Raise::Quiet => {
            if let Some(player) = state.players.get_mut(&session) {
                food::exhaust(&mut player.survival, 0.1);
                player.stop_sleep();
            }
        }
    }
}

/// Whether `source` is a thrown trident.
fn is_trident(state: &mut State, source: DamageSource) -> bool {
    let DamageSource::Child { child, .. } = source else {
        return false;
    };
    crate::tick::Context { state }
        .entity(child)
        .is_some_and(|e| e.entity_type() == crate::entity::Type::Trident)
}

/// Whether `source` is a cactus in the world of the player of `session`.
fn is_cactus(state: &State, session: SessionId, source: DamageSource) -> bool {
    let DamageSource::Block(pos) = source else {
        return false;
    };
    state
        .players
        .get(&session)
        .and_then(|p| state.worlds.get(p.world))
        .and_then(|w| w.block(pos))
        .is_some_and(|b| b.is(crate::block::Class::Cactus))
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

/// Heals a player for `reason` (PMMP `Entity::heal` with its `EntityRegainHealthEvent`): health
/// up to its maximum. Nothing happens, and no event is raised, for a dead player, a player at
/// full health or an amount that is not positive.
pub(super) fn heal(state: &mut State, session: SessionId, amount: f64, reason: RegainReason) {
    let heals_now = |state: &State, amount: f64| {
        state
            .players
            .get(&session)
            .is_some_and(|p| heals(p, amount))
    };
    if !heals_now(state, amount) {
        return;
    }
    let id = player::Id::new(session);
    let Ok(event) = super::ask(
        state,
        || EntityRegainHealth {
            entity: Actor::Player(id),
            amount,
            regain_reason: reason,
            cancelled: false,
        },
        EntityRegainHealth::is_cancelled,
        |state, e| heals_now(state, e.amount),
    ) else {
        return;
    };
    let amount = event.as_ref().map_or(amount, |e| e.amount);
    if let Some(player) = state.players.get_mut(&session) {
        heal_player(player, amount);
    }
    super::watch(state, event.as_ref());
}

/// Whether `amount` heals the player: it is alive and below its maximum, and the amount is
/// positive.
fn heals(player: &Player, amount: f64) -> bool {
    let s = &player.survival;
    !s.is_dead() && amount > 0.0 && s.health < s.max_health()
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
pub(super) fn armor_points(player: &Player) -> u32 {
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
    u32::from(stack.enchantment_level(enchantment).unwrap_or(0))
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
        Enchantment::BlastProtection => {
            matches!(cause, Damage::Explosion | Damage::BlockExplosion).then_some(1.5)
        }
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
fn use_totem(state: &mut State, session: SessionId, raise: entity::Raise) {
    let given = [
        (Effect::Regeneration, 40 * 20),
        (Effect::FireResistance, 40 * 20),
        (Effect::Absorption, 5 * 20),
    ];
    match raise {
        entity::Raise::Events => {
            effects::clear(state, session);
            for (effect, ticks) in given {
                effects::add(state, session, effect, Active::new(ticks, 1));
            }
        }
        entity::Raise::Quiet => {
            if let Some(player) = state.players.get_mut(&session) {
                player.clear_effects();
                for (effect, ticks) in given {
                    let instance = effect.instance().with_duration_ticks(ticks);
                    player.add_effect(instance.with_amplifier(1));
                }
            }
        }
    }
    let packet = actor_event(
        player_runtime_id(state, session),
        ActorEvent::TalismanActivate,
        0,
    );
    broadcast_from(state, session, &packet);
    sound::at_feet(state, session, Sound::TotemUse);
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

/// Wears a durable stack by `amount` (PMMP `Durable::applyDamage`, the one rule of
/// [`interact::breaking::damage`]); `None` if the stack does not change.
pub(super) fn wear(
    settings: &crate::tick::Settings,
    stack: &Stack,
    amount: u32,
    r: &mut Random,
) -> Option<Stack> {
    let worn = interact::breaking::damage(settings, stack, amount, r);
    (worn != *stack).then_some(worn)
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
        if let Some(worn) = wear(&state.settings, stack, points, &mut r) {
            broke |= worn.count() < stack.count();
            inventory::set_armor(player, slot, worn);
        }
    }
    if broke {
        sound::at_feet(state, session, Sound::ItemBreak);
    }
}

/// Thorns on the victim's armour hurts the attacker (PMMP `Living::applyPostDamageEffects`): each
/// piece with level L answers with chance 15 % × L, dealing 1 to 4 (L - 10 above level 10) and
/// wearing the piece by 3, else wearing it by 1; the wearer's Strength adds to the answer as to
/// any hit by it. A thorns hit is not answered again: PMMP lets
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
        if let Some(worn) = wear(&state.settings, stack, wear_by, &mut r) {
            broke |= worn.count() < stack.count();
            inventory::set_armor(player, slot, worn);
        }
    }
    if broke {
        sound::at_feet(state, victim, Sound::ItemBreak);
    }
    if dealt > 0.0 {
        let (extra, parts) = attacker_modifiers(state, victim, dealt, Damage::Magic);
        hurt(
            state,
            attacker,
            Hit {
                cause: Damage::Magic,
                base: dealt,
                extra,
                parts,
                attacker: Some(victim),
                thorns: false,
                source: DamageSource::Entity(Actor::Player(player::Id::new(victim))),
            },
        );
    }
}

/// Knocks the player back away from another player (PMMP `Living::knockBack`): the motion the
/// client is sent (players have no server-side motion, PMMP `Player::setMotion`), at most
/// `vertical_limit` upward.
fn knock_back_from(
    state: &mut State,
    session: SessionId,
    from: SessionId,
    force: f64,
    vertical_limit: f64,
) {
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
        vertical_limit,
    );
}

/// Sends the knockback motion along (x, z).
fn knock_back(
    state: &mut State,
    session: SessionId,
    x: f64,
    z: f64,
    force: f64,
    vertical_limit: f64,
) {
    let length = (x * x + z * z).sqrt();
    if length.is_nan() || length <= 0.0 {
        return;
    }
    let f = 1.0 / length;
    let motion = Vec3::new(
        x * f * force,
        math::min(force, vertical_limit),
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

/// `source` sets the player on fire for `seconds` (PMMP `EntityCombustByBlockEvent`,
/// `EntityCombustByEntityEvent`, then `Living::setOnFire`): the [`EntityCombust`] listeners may
/// cancel it or change the time.
pub(super) fn combust(state: &mut State, session: SessionId, seconds: u32, source: CombustSource) {
    if !state.players.contains_key(&session) {
        return;
    }
    let id = player::Id::new(session);
    let Ok(event) = super::ask(
        state,
        || EntityCombust {
            entity: Actor::Player(id),
            duration_secs: seconds,
            source,
            cancelled: false,
        },
        EntityCombust::is_cancelled,
        |state, _| state.players.contains_key(&session),
    ) else {
        return;
    };
    let seconds = event.as_ref().map_or(seconds, |e| e.duration_secs);
    if let Some(player) = state.players.get_mut(&session) {
        set_on_fire(player, seconds);
    }
    super::watch(state, event.as_ref());
}

/// The player's fire went out for `cause` (PMMP `Entity::extinguish`): the [`EntityExtinguish`]
/// listeners and monitors hear it.
pub(super) fn extinguished(state: &mut State, session: SessionId, cause: ExtinguishCause) {
    let id = player::Id::new(session);
    super::notify(state, || EntityExtinguish {
        entity: Actor::Player(id),
        cause,
    });
}

/// `attacker` hits `target` (PMMP `Player::attackEntity`): the held item's attack points, the
/// attacker's effects, sharpness, a critical hit (falling, not flying, blind or under water, and
/// not sprinting under `critical-hits = "sprint"`) for half again; cancelled out of reach, in
/// spectator mode or with `pvp = false`; nothing at all for a dead target or one in another world.
pub(super) fn attack_player(state: &mut State, attacker: SessionId, target: SessionId) {
    // PMMP finds the target in the attacker's world alone and leaves a dead one at once: no
    // swing, no sound.
    let found = attacker != target
        && state
            .players
            .get(&attacker)
            .zip(state.players.get(&target))
            .is_some_and(|(a, t)| a.world == t.world && !t.survival.is_dead());
    if !found {
        return;
    }
    let Some(hit) = melee_hit(state, attacker, target) else {
        swing(state, attacker);
        hit_sound(state, target, Sound::EntityAttackNoDamage);
        return;
    };
    let held = state
        .players
        .get(&attacker)
        .map(|p| p.inventory.held().clone())
        .unwrap_or_default();
    let landed = hurt(state, target, hit);
    swing(state, attacker);
    let Some(modifiers) = landed else {
        hit_sound(state, target, Sound::EntityAttackNoDamage);
        return;
    };
    hit_sound(state, target, Sound::EntityAttack);
    let target_runtime = player_runtime_id(state, target);
    for (modifier, action) in [
        (DamageModifier::Critical, AnimateAction::CriticalHit),
        (
            DamageModifier::WeaponEnchantments,
            AnimateAction::MagicCriticalHit,
        ),
    ] {
        // As the listeners left the hit: what added nothing shows nothing.
        if modifiers
            .get(modifier as usize)
            .copied()
            .flatten()
            .is_some_and(|v| v > 0.0)
        {
            let packet = Packet::Animate(Animate {
                action_type: u8::from(action),
                entity_runtime_id: target_runtime,
                data: 0.0,
                swing_source: 0,
            });
            broadcast_from(state, target, &packet);
        }
    }
    after_melee(state, attacker, &held, |state, knockback, fire| {
        if let Some(force) = knockback {
            knock_back_from(state, target, attacker, force, KNOCKBACK_VERTICAL_LIMIT);
        }
        if let Some(seconds) = fire {
            let by = CombustSource::Entity(Actor::Player(player::Id::new(attacker)));
            combust(state, target, seconds, by);
        }
    });
}

/// The attacker's side of a melee hit by the player `attacker` with `held` that landed, on a
/// player or an entity (PMMP `Player::attackEntity` after `attack`): `enchant` applies the
/// weapon's Knockback force (0.5 per level) and Fire Aspect burn (4 seconds per level) to the
/// target, each `None` without the enchantment (PMMP `MeleeWeaponEnchantment::onPostAttack`);
/// then, if the attacker is still alive (a reactive hit such as thorns may have killed it), its
/// weapon wears and it gets hungrier.
pub(super) fn after_melee(
    state: &mut State,
    attacker: SessionId,
    held: &Stack,
    enchant: impl FnOnce(&mut State, Option<f64>, Option<u32>),
) {
    let knockback = level_of(held, Enchantment::Knockback);
    let fire = level_of(held, Enchantment::FireAspect);
    enchant(
        state,
        (knockback > 0).then(|| f64::from(knockback) * 0.5),
        (fire > 0).then(|| fire * 4),
    );
    let Some(player) = state.players.get_mut(&attacker) else {
        return;
    };
    if player.survival.is_dead() {
        return;
    }
    player.data_dirty = true;
    wear_weapon(state, attacker, held);
    food::exhaust_as(state, attacker, 0.1, ExhaustCause::Attack);
}

/// A hit sound at the middle of the player `target` (PMMP `attackEntity`: `EntityAttackSound`,
/// or `EntityAttackNoDamageSound` for a hit that did nothing).
fn hit_sound(state: &mut State, target: SessionId, sound: Sound) {
    let Some(at) = state.players.get(&target).map(feet) else {
        return;
    };
    sound::hit(state, target, at + Vec3::new(0.0, HALF_HEIGHT, 0.0), sound);
}

/// The hit of a melee attack; `None` when the attack cannot happen.
fn melee_hit(state: &State, attacker: SessionId, target: SessionId) -> Option<Hit> {
    let a = state.players.get(&attacker)?;
    let t = state.players.get(&target)?;
    if a.phase != Phase::Spawned
        || t.phase != Phase::Spawned
        || a.survival.is_dead()
        || !state.settings.pvp
        || a.survival.mode() == GameMode::Spectator
        || !can_reach(a, feet(t))
    {
        return None;
    }
    let (base, extra, parts, _, _) = melee_parts(state, attacker, a);
    Some(Hit {
        cause: Damage::Attack(attacker),
        base,
        extra,
        parts,
        attacker: Some(attacker),
        thorns: true,
        source: DamageSource::Entity(Actor::Player(player::Id::new(attacker))),
    })
}

/// The attacker's side of a melee hit by `a` (PMMP `Player::attackEntity`): the held item's
/// attack points, the extra of Strength, Weakness and Sharpness (added up and one by one), a
/// critical hit (falling, not flying, not blind, not under water, and not sprinting under
/// `critical-hits = "sprint"`) for half again; whether the critical hit and a weapon enchantment
/// added damage (PMMP shows their animations only then).
fn melee_parts(
    state: &State,
    attacker: SessionId,
    a: &Player,
) -> (f64, f64, Modifiers, bool, bool) {
    let held = a.inventory.held();
    let base = f64::from(held.item().map_or(1, Type::attack_points));
    let (mut extra, mut parts) =
        attacker_modifiers(state, attacker, base, Damage::Attack(attacker));
    let sharpness = level_of(held, Enchantment::Sharpness);
    let weapon = if sharpness > 0 {
        0.5 * f64::from(sharpness + 1)
    } else {
        0.0
    };
    extra += weapon;
    if weapon > 0.0 {
        set(&mut parts, DamageModifier::WeaponEnchantments, weapon);
    }
    let s = &a.survival;
    let critical = match state.settings.critical_hits {
        CriticalHits::Off => false,
        CriticalHits::Sprint => !s.motion.sprinting,
        CriticalHits::Always => true,
    } && !s.motion.flying
        && s.fall_distance > 0.0
        && !s.effects.contains_key(&Effect::Blindness)
        && !movement::is_underwater(state, a);
    let mut more = 0.0;
    if critical {
        let total = base + extra;
        more = if total > 0.0 { total / 2.0 } else { 0.0 };
        extra += more;
        set(&mut parts, DamageModifier::Critical, more);
    }
    (base, extra, parts, more > 0.0, weapon > 0.0)
}

/// The melee hit the player of `session` makes on an entity that is not a player; `None` if it
/// cannot hit (not spawned, dead or a spectator).
pub(super) fn melee(state: &State, session: SessionId) -> Option<super::Melee> {
    let a = state.players.get(&session)?;
    if a.phase != Phase::Spawned || a.survival.is_dead() || a.survival.mode() == GameMode::Spectator
    {
        return None;
    }
    let (base, extra, _, critical, magic) = melee_parts(state, session, a);
    let total = base + extra;
    Some(super::Melee {
        damage: if total > 0.0 { total } else { 0.0 },
        critical,
        magic,
    })
}

/// Whether the player reaches a position to hit it ([`can_interact`] within 8 blocks).
fn can_reach(player: &Player, pos: Vec3) -> bool {
    let d = super::look_direction(player);
    can_interact(player.position, d, pos, ENTITY_REACH)
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

/// The weapon `held` that made a hit wears (PMMP `onAttackEntity`): swords, tridents and hoes by
/// 1, axes, pickaxes and shovels by 2, with the item-break sound when it breaks. Nothing for a
/// player without finite resources, or whose held item changed since the hit (PMMP
/// `Player::returnItemsFromAction`).
fn wear_weapon(state: &mut State, session: SessionId, held: &Stack) {
    let tick = state.tick;
    let seed = world_seed(state, session);
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if !super::is_survival(player.survival.mode()) || player.inventory.held() != held {
        return;
    }
    let Some(item) = held.item() else {
        return;
    };
    let amount = if item.is(Class::Sword) || item.is(Class::Hoe) || item.is(Class::Trident) {
        1
    } else if item.is(Class::Axe) || item.is(Class::Pickaxe) || item.is(Class::Shovel) {
        2
    } else {
        return;
    };
    let mut r = player_random(seed, tick, player, TOOL_WEAR);
    if let Some(worn) = wear(&state.settings, held, amount, &mut r) {
        let broke = worn.count() < held.count();
        inventory::set_held(player, worn);
        if broke {
            sound::at_feet(state, session, Sound::ItemBreak);
        }
    }
}

/// The player died (PMMP `Player::onDeath`): its window closes, its breaking and item use stop,
/// the [`PlayerDeath`] listeners
/// may change what drops, the experience dropped and the messages, it drops what it carries and
/// its experience, viewers see it die and it sees the death screen with `message` (a translation
/// key) naming the player and its `attacker`.
///
/// While earlier drops still wait for room, the player keeps what it carries and nothing more
/// drops: the waiting list then never holds more than one death's drops
/// ([`MAX_HELD_BACK`](super::MAX_HELD_BACK)). Drops a listener replaced are checked like external
/// input: at most one inventory of stacks
/// ([`MAX_LISTENER_DROPS`](super::MAX_LISTENER_DROPS)), each within its item's stack size; the
/// player's own stacks they replace are gone, but those kept on death.
pub(super) fn die(
    state: &mut State,
    session: SessionId,
    message: &'static str,
    attacker: Option<SessionId>,
) {
    inventory::close_window(state, session);
    interact::stop_breaking(state, session);
    retry_held_back(state, session);
    let heard = dispatch::heard::<PlayerDeath>(state);
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let finite = player.survival.mode() != GameMode::Creative;
    let world_name = state
        .worlds
        .get(player.world)
        .map(|w| w.name().to_owned())
        .unwrap_or_default();
    let at = feet(player);
    player.survival.death = Some(Point {
        world: world_name,
        pos: BlockPos::containing(at),
    });
    super::stop_using_item(player);
    player.breaking.forget_attacked();
    player.data_dirty = true;
    let world = player.world;
    let mut event = heard.then(|| {
        let drops = if finite && player.survival.held_back.is_empty() {
            carried(player)
        } else {
            Vec::new()
        };
        PlayerDeath {
            player: player::Id::new(session),
            drops,
            xp_drop_amount: food::death_xp(&player.survival, finite),
            death_message: String::new(),
            death_screen_message: String::new(),
            keep_inventory: false,
            keep_xp: false,
        }
    });
    let original = event.as_ref().map(|e| e.drops.clone());
    if let Some(e) = &mut event {
        // Death cannot be cancelled: the player stays dead whatever the listeners do.
        dispatch::call(state, e);
    }
    drop_death(
        state,
        session,
        (world, at),
        finite,
        event.as_ref().zip(original.as_deref()),
    );
    if let Some(e) = &event
        && !e.death_message.is_empty()
        && let Err(err) = crate::tick::player::broadcast(
            state,
            crate::player::Message::Raw(e.death_message.clone()),
        )
    {
        debug!(session = session.get(), error = %err, "death message not sent");
    }
    clock::wake(state, session);
    let runtime_id = player_runtime_id(state, session);
    broadcast_from(
        state,
        session,
        &actor_event(runtime_id, ActorEvent::Death, 0),
    );
    broadcast_from(
        state,
        session,
        &attributes::size_data(runtime_id, attributes::DYING_SIZE),
    );
    for packet in crate::tick::entity::locator_life(runtime_id, false) {
        broadcast_to_viewers(state, session, &packet);
    }
    let screen = event
        .as_ref()
        .map(|e| e.death_screen_message.clone())
        .filter(|m| !m.is_empty() && m.len() <= crate::player::MAX_TEXT);
    show_death(state, session, message, attacker, screen);
    super::watch(state, event.as_ref());
}

/// Drops what a dead player carries and its experience at `place` (PMMP `Player::onDeath`), as
/// the [`PlayerDeath`] listeners left the `event` (with the drops it started with) if one was
/// raised: kept inventory, replaced drops, kept or changed experience.
fn drop_death(
    state: &mut State,
    session: SessionId,
    place: (WorldIndex, Vec3),
    finite: bool,
    event: Option<(&PlayerDeath, &[Stack])>,
) {
    let (world, at) = place;
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let (keep_inventory, keep_xp) = event
        .as_ref()
        .map_or((false, false), |(e, _)| (e.keep_inventory, e.keep_xp));
    let free = player.survival.held_back.is_empty() && !keep_inventory;
    let stacks = if free {
        inventory::take_all(player)
    } else {
        Vec::new()
    };
    let replaced = match &event {
        Some((e, original)) if free && e.drops != *original => Some(valid_drops(&e.drops)),
        // A creative player drops nothing (PMMP `Player::getDrops`), yet loses what it carries
        // but for the items kept on death (PMMP `Player::onDeath`).
        _ if !finite => Some(Vec::new()),
        _ => None,
    };
    let (to_drop, kept) = match replaced {
        Some(drops) => {
            let kept = stacks
                .into_iter()
                .filter(|s| keeps_on_death(s) && level_of(s, Enchantment::Vanishing) == 0)
                .collect();
            (drops, kept)
        }
        None => (stacks, Vec::new()),
    };
    let xp = if keep_xp {
        0
    } else {
        let xp = food::take_death_xp(&mut player.survival, finite);
        event.as_ref().map_or(xp, |(e, _)| e.xp_drop_amount)
    };
    let mut refused = drop_all(to_drop, |stack| {
        if entity::drop_item(state, world, at, stack.clone()) {
            None
        } else {
            Some(stack)
        }
    });
    refused.extend(kept);
    if let Some(player) = state.players.get_mut(&session) {
        let left = give_back(refused, |stack| inventory::give(player, stack));
        player.survival.held_back.extend(left);
    }
    if xp > 0 {
        entity::drop_experience(state, world, at, xp);
    }
}

/// What a dead player drops if nothing changes it (PMMP `Human::getDrops`): its main slots,
/// armour and off hand, without items that vanish or stay on death.
fn carried(player: &Player) -> Vec<Stack> {
    let inv = &player.inventory;
    (0..=u8::MAX)
        .map_while(|slot| inv.main_stack(slot))
        .chain(inv.armor())
        .chain(std::iter::once(inv.offhand()))
        .filter(|s| !s.is_empty() && level_of(s, Enchantment::Vanishing) == 0 && !keeps_on_death(s))
        .cloned()
        .collect()
}

/// Drops a listener set, checked: at most one inventory of non-empty stacks, each within its
/// item's stack size.
fn valid_drops(drops: &[Stack]) -> Vec<Stack> {
    drops
        .iter()
        .filter_map(|s| {
            let max = s.item()?.max_stack_size();
            (!s.is_empty()).then(|| s.with_count(s.count().min(max)))
        })
        .take(super::MAX_LISTENER_DROPS)
        .collect()
}

/// The death message of a player killed through the API (the cause was not kept).
pub(super) const GENERIC_DEATH: &str = "death.attack.generic";

/// The death screen (PMMP `DeathPacketHandler::setUp`): the respawn position being searched and
/// the death message (a translation key with the player's and the attacker's names), or the
/// `screen` text a [`PlayerDeath`] listener chose.
fn show_death(
    state: &mut State,
    session: SessionId,
    message: &'static str,
    attacker: Option<SessionId>,
    screen: Option<String>,
) {
    let (_, respawn_at) = respawn_point(state, session);
    let attacker_name =
        attacker.and_then(|a| state.players.get(&a).map(|p| p.identity.name.clone()));
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    player.survival.motion.death_shown = true;
    let (cause, messages) = match screen {
        Some(text) => (text, Vec::new()),
        None => (
            message.to_owned(),
            std::iter::once(player.identity.name.clone())
                .chain(attacker_name)
                .collect(),
        ),
    };
    let runtime_id = player.runtime_id;
    player.send_packet(respawn_packet(
        respawn_at,
        RespawnState::SearchingForSpawn,
        runtime_id,
    ));
    player.send_packet(Packet::DeathInfo(DeathInfo { cause, messages }));
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
/// moved up to the first two free blocks of the loaded chunks ([`world::World::safe_spawn`]). A
/// spawn point in a world that is not loaded counts as none.
fn respawn_point(state: &State, session: SessionId) -> (WorldIndex, Vec3) {
    let Some(player) = state.players.get(&session) else {
        return (0, Vec3::ZERO);
    };
    let own = player.survival.spawn.as_ref().and_then(|p| {
        let index = state.catalog.named(&p.world)?;
        state.catalog.is_loaded(index).then_some((index, p.pos))
    });
    let default = state.catalog.default_world();
    let Some((index, spawn)) =
        own.or_else(|| state.worlds.get(default).map(|w| (default, w.spawn())))
    else {
        return (player.world, player.position);
    };
    let Some(world) = state.worlds.get(index) else {
        return (player.world, player.position);
    };
    let feet = world.safe_spawn(spawn);
    let eye = Vec3::new(
        f64::from(feet.x) + 0.5,
        f64::from(feet.y) + EYE_HEIGHT,
        f64::from(feet.z) + 0.5,
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
        player.send_packet(respawn_packet(
            position,
            RespawnState::ReadyToSpawn,
            runtime_id,
        ));
    }
}

/// The player respawns (PMMP `Player::respawn`, `actuallyRespawn`, `NetworkSession::
/// onServerRespawn`) at its respawn point, in another world if the point is there, or where a
/// [`PlayerRespawn`] listener sends it (a world the server does not run or a place outside the
/// world border is ignored); nothing unless it is dead. In hardcore the player comes back as a
/// spectator, as vanilla does, unless a
/// [`PlayerGameModeChange`](crate::event::PlayerGameModeChange) listener cancels that (PMMP
/// bans the player instead).
pub(super) fn respawn(state: &mut State, session: SessionId) {
    // A death the API made runs later in the tick (drops, event, death screen); the respawn waits
    // for it.
    if !state
        .players
        .get(&session)
        .is_some_and(|p| p.survival.motion.death_shown)
    {
        return;
    }
    let Some((world, position, event)) = revive(state, session, Phase::Spawned) else {
        return;
    };
    super::teleport(state, session, world, position);
    let burning = restore(state, session);
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
    let size = attributes::size_data(player.runtime_id, attributes::size(&player.survival));
    let health = attributes::health_packet(player);
    let now = attributes::shown(player);
    let seen = [data, alive, size];
    for packet in seen.iter().chain([&health]) {
        broadcast_to_viewers(state, session, packet);
    }
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    // The player gets its own straight away: it may respawn where its client has no chunks yet
    // (another world, far away), so it does not see itself as a viewer would.
    for packet in out.into_iter().chain(seen) {
        player.send_packet(packet);
    }
    // The packets above carry all but the adventure settings, which a listener may have changed.
    player.survival.dirty &= attributes::ADVENTURE;
    player.survival.shown = now;
    if let Some(runtime_id) = state.players.get(&session).map(|p| p.runtime_id) {
        for packet in crate::tick::entity::locator_life(runtime_id, true) {
            broadcast_to_viewers(state, session, &packet);
        }
    }
    if burning {
        extinguished(state, session, ExtinguishCause::Respawn);
    }
    super::watch(state, event.as_ref());
}

/// A player whose saved data has it dead (it left, or the server stopped, before it respawned)
/// respawns as [`respawn`] does, but before its start-game packets, which then show it alive at
/// its respawn point (PMMP `Player::doFirstSpawn` respawns it at its first spawn): a client that
/// spawns dead shows no death screen and never asks to respawn, so it would stay dead. Nothing
/// unless it is dead and joined, not yet spawned.
pub(in crate::tick) fn respawn_joined(state: &mut State, session: SessionId) {
    let Some((world, position, event)) = revive(state, session, Phase::Joined) else {
        return;
    };
    // Its view opens once its client asks for a radius, after the start-game packets.
    if let Some(player) = state.players.get_mut(&session) {
        player.world = world;
        player.position = position;
    }
    if restore(state, session) {
        extinguished(state, session, ExtinguishCause::Respawn);
    }
    super::watch(state, event.as_ref());
}

/// Where the dead player of `session` in `phase` respawns (see [`respawn`]): the world and eye
/// position, and the [`PlayerRespawn`] event to [`watch`](super::watch) once it respawned; a
/// hardcore player becomes a spectator first. `None` if it is not dead in `phase`, before or
/// after the listeners.
fn revive(
    state: &mut State,
    session: SessionId,
    phase: Phase,
) -> Option<(WorldIndex, Vec3, Option<PlayerRespawn>)> {
    let dead = |state: &State| {
        state
            .players
            .get(&session)
            .is_some_and(|p| p.phase == phase && p.survival.is_dead())
    };
    if !dead(state) {
        return None;
    }
    if state.settings.hardcore {
        hardcore_spectator(state, session);
    }
    let (world, position) = respawn_point(state, session);
    let feet_at = position - Vec3::new(0.0, EYE_HEIGHT, 0.0);
    let world_id = world::Id::of(world);
    let event = super::ask(
        state,
        || PlayerRespawn {
            player: player::Id::new(session),
            world: world_id,
            position: feet_at,
        },
        |_| false,
        |state, _| dead(state),
    )
    .ok()?;
    let (world, position) = event
        .as_ref()
        .filter(|e| e.world != world_id || e.position != feet_at)
        .and_then(|e| respawn_target(state, e.world, e.position))
        .unwrap_or((world, position));
    Some((world, position, event))
}

/// The respawned player's state (PMMP `Player::actuallyRespawn`): no effects, full health and
/// food, air, no fire, and a short invulnerability. Returns whether it was burning.
fn restore(state: &mut State, session: SessionId) -> bool {
    effects::clear(state, session);
    let Some(player) = state.players.get_mut(&session) else {
        return false;
    };
    let burning = player.survival.fire_ticks > 0;
    let s = &mut player.survival;
    s.motion.sprinting = false;
    s.motion.sneaking = false;
    // A spectator (a hardcore one included) flies, as `set_game_mode` made it.
    s.motion.flying = s.mode() == GameMode::Spectator;
    // While dead the client's stop flags are not read: nothing would end these.
    s.motion.swimming = false;
    s.motion.gliding = false;
    s.motion.death_shown = false;
    s.fire_ticks = 0;
    s.air = s.custom.max_air;
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
    burning
}

/// A hardcore player that respawns becomes a spectator, as vanilla does, unless a
/// [`PlayerGameModeChange`](crate::event::PlayerGameModeChange) listener cancels that.
fn hardcore_spectator(state: &mut State, session: SessionId) {
    crate::tick::Context { state }.change_game_mode(player::Id::new(session), GameMode::Spectator);
}

/// Where a [`PlayerRespawn`] listener sends the player: the world's index and the eye position;
/// `None` for a world the server does not run (or unloaded) or a place outside the world border.
fn respawn_target(state: &State, world: world::Id, feet: Vec3) -> Option<(WorldIndex, Vec3)> {
    let index = crate::tick::world_api::index(state, world)?;
    let eye = feet + Vec3::new(0.0, EYE_HEIGHT, 0.0);
    let border = f64::from(crate::tick::MAX_COORDINATE);
    (eye.is_finite() && [eye.x, eye.y, eye.z].iter().all(|c| c.abs() <= border))
        .then_some((index, eye))
}

/// The void, burning and the respawn invulnerability, each tick (PMMP `Entity::entityBaseTick`,
/// `Entity::doOnFireTick`, `Player::onUpdate`).
pub(super) fn base_tick(state: &mut State, session: SessionId) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let void = feet(player).y <= VOID_Y;
    let creative = player.survival.mode() == GameMode::Creative;
    let s = &mut player.survival;
    let mut burn = false;
    let mut out = false;
    if s.fire_ticks > 0 {
        s.fire_ticks -= 1;
        burn = s.fire_ticks % u16::from(FIRE_DAMAGE_TICKS) == 0;
        out = s.fire_ticks == 0;
        player.data_dirty = true;
    }
    if void {
        damage(state, session, VOID_DAMAGE, Damage::Void);
    }
    if burn {
        damage(state, session, 1.0, Damage::FireTick);
    }
    if out {
        extinguished(state, session, ExtinguishCause::Ticking);
    }
    // After the void and the fire: the last invulnerable tick still keeps them off.
    if let Some(player) = state.players.get_mut(&session)
        && player.survival.no_damage_ticks > 0
    {
        player.survival.no_damage_ticks -= 1;
    }
    if creative
        && let Some(player) = state.players.get_mut(&session)
        && player.survival.fire_ticks > 1
    {
        player.survival.fire_ticks = 1;
    }
}

/// Suffocation, air and the hit cooldown, each tick (PMMP `Living::entityBaseTick`,
/// `Living::doAirSupplyTick`); with `drowning = false` only the hit cooldown.
pub(super) fn living_tick(state: &mut State, session: SessionId) {
    if state
        .players
        .get(&session)
        .is_none_or(|p| p.survival.is_dead())
    {
        return;
    }
    if state.settings.drowning && air_tick(state, session) {
        damage(state, session, 2.0, Damage::Drowning);
    }
    // After the air's hits: the cooldown's last tick still weighs them against the last hit.
    if let Some(player) = state.players.get_mut(&session)
        && player.survival.attack_time > 0
    {
        player.survival.attack_time -= 1;
    }
}

/// Suffocation inside a solid block, then the air supply of a living player; true when it runs
/// out of air now.
fn air_tick(state: &mut State, session: SessionId) -> bool {
    let tick = state.tick;
    let Some(player) = state.players.get(&session) else {
        return false;
    };
    let creative = player.survival.mode() == GameMode::Creative;
    let inside = movement::is_inside_solid(state, player);
    let breathes = creative
        || player
            .survival
            .effects
            .contains_key(&Effect::WaterBreathing)
        || player.survival.effects.contains_key(&Effect::ConduitPower)
        || !movement::is_underwater(state, player);
    if inside {
        damage(state, session, SUFFOCATION_DAMAGE, Damage::Suffocation);
    }
    let Some(player) = state.players.get_mut(&session) else {
        return false;
    };
    let mut drown = false;
    if breathes {
        // BDS (P6 craft dive): breathing again at once, 4 ticks of air back each tick up to the
        // full supply (PMMP adds 5 and stays out of breath until full).
        let s = &mut player.survival;
        s.breathing = true;
        let max = s.custom.max_air;
        if s.air < max {
            s.air = s.air.saturating_add(4).min(max);
            player.data_dirty = true;
        }
    } else {
        let respiration = player
            .inventory
            .armor()
            .first()
            .map_or(0, |helmet| level_of(helmet, Enchantment::Respiration));
        let loses = respiration == 0 || {
            let seed = state.worlds.get(player.world).map_or(0, world::World::seed);
            let mut r = player_random(seed, tick, player, RESPIRATION);
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
    }
    drown
}

/// The fall damage of a landing (PMMP `Living::calculateFallDamage`): the blocks fallen beyond 3,
/// less the jump boost level, rounded up; none while flying.
pub(super) fn fall_damage(s: &Survival, distance: f64) -> f64 {
    if s.motion.flying {
        return 0.0;
    }
    (distance - SAFE_FALL - f64::from(s.effect_level(Effect::JumpBoost))).ceil()
}
