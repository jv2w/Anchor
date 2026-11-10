//! Damage, healing, melee combat, death and respawn.
//!
//! A hit is worked out in two steps, so events can sit between them: [`assess`] checks
//! whether the hit lands and computes its modifiers, [`apply`] changes the state. Between them,
//! the modifiers that depend on the player as it is (the absorption, a helmet against a falling
//! block, a totem) are worked out again from what the listeners left ([`finish`]).

use tracing::debug;

use super::attributes::{self, ABSORPTION, HEALTH};
use super::effects::{self, Active};
use super::{
    ENTITY_REACH, MAX_FIRE_TICKS, PLAYER_HEIGHT, Point, Stay, Survival, broadcast_from,
    broadcast_to_viewers, can_interact, feet, food, movement, player_random, sound, world_seed,
};
use crate::config::{CriticalHits, GameMode};
use crate::effect::Type as Effect;
use crate::entity::Animation;
use crate::event::{
    Actor, CombustSource, DamageCause, DamageModifier, DamageSource, EntityCombust, EntityDamage,
    EntityExtinguish, EntityRegainHealth, ExhaustCause, ExtinguishCause, MAX_DAMAGE, Modifiers,
    PlayerDeath, PlayerRespawn, RegainReason,
};
use crate::item::{Class, Enchantment, Stack, Type};
use crate::math::{self, Vec3};
use crate::player;
use crate::protocol::consts::{ActorEvent, RespawnState};
use crate::protocol::packet::{self, DeathInfo, Packet, Respawn};
use crate::random::{self, Random};
use crate::save::WorldIndex;
use crate::tick::entity::{
    ATTACK_COOLDOWN_TICKS, FIRE_DAMAGE_TICKS, KNOCKBACK, MAX_KNOCKBACK_VERTICAL, SAFE_FALL,
    SUFFOCATION_DAMAGE, VOID_DAMAGE, VOID_Y,
};
use crate::tick::interact::drops;
use crate::tick::{
    Context, Phase, Player, SessionId, Settings, State, dispatch, entity, interact, inventory,
};
use crate::world::{self, BlockPos, Sound};

/// The random stream of armor wear.
const ARMOR_WEAR: u64 = random::tag("armor-wear");
/// The exhaustion a hit adds to the player it hurts.
const DAMAGE_EXHAUSTION: f64 = 0.1;
/// The modifiers [`finish`] works out from the player as it is when the hit applies.
const FINISHED: [DamageModifier; 3] = [
    DamageModifier::Absorption,
    DamageModifier::Totem,
    DamageModifier::ArmorHelmet,
];
/// The death message of a player killed through the API (the cause was not kept).
pub(super) const GENERIC_DEATH: &str = "death.attack.generic";
/// Half a player's height: hit sounds play there.
const HALF_HEIGHT: f64 = PLAYER_HEIGHT / 2.0;
/// The highest the summed protection factor counts.
const MAX_EPF: u32 = 25;
/// No modifier applies.
const NO_MODIFIERS: Modifiers = [None; DamageModifier::all().len()];
/// The random stream of protection rolls.
const PROTECTION: u64 = random::tag("protection");
/// Ticks a respawned player ignores damage.
const RESPAWN_INVULNERABLE_TICKS: u32 = 60;
/// The position the official server sends in the `Respawn` (searching for the spawn) it answers
/// a client ready to spawn with, before the respawn position (P6).
const RESPAWN_SEARCH_POSITION: Vec3 = Vec3::new(0.0, 32_767.0, 0.0);
/// The random stream of respiration rolls.
const RESPIRATION: u64 = random::tag("respiration");
/// The random stream of thorns rolls.
const THORNS: u64 = random::tag("thorns");
/// The random stream of tool wear.
const TOOL_WEAR: u64 = random::tag("tool-wear");

/// Why a player takes damage.
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
    /// The player killed itself: no modifier or invulnerability spares it, only creative.
    Suicide,
    /// A lightning bolt, dealt only quietly ([`damage_quiet`]): it raises no [`EntityDamage`],
    /// and its cause is custom where one is named.
    Lightning,
    /// Damage without a game cause.
    Custom,
}

impl Damage {
    /// The damage of `cause` dealt by `source` (an action): a melee hit by a player is that
    /// player's attack.
    pub(in crate::tick) fn of(cause: DamageCause, source: DamageSource) -> Self {
        match cause {
            DamageCause::Contact => Self::Contact,
            DamageCause::EntityAttack => match source {
                DamageSource::Entity(Actor::Player(p)) => Self::Attack(p.session()),
                _ => Self::Melee,
            },
            DamageCause::Projectile => Self::Entity,
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

    /// The official damage cause number, which the hurt event carries ([`actor_cause`]; P6:
    /// lightning 24, which no event cause names).
    pub(in crate::tick) fn actor_cause(self) -> i32 {
        match self {
            Self::Lightning => 24,
            _ => actor_cause(self.event_cause()),
        }
    }

    /// The translation key of the death message.
    fn death_message(self) -> &'static str {
        match self {
            Self::Attack(_) => "death.attack.player",
            Self::Melee | Self::Suicide | Self::Lightning | Self::Custom => GENERIC_DEATH,
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
        }
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

    /// Whether it is fire damage (fire resistance cancels it).
    fn is_fire(self) -> bool {
        matches!(self, Self::Fire | Self::FireTick | Self::Lava)
    }
}

/// A hit about to land: the cause, the base damage and the modifiers the attacker's side adds
/// (strength, weakness, weapon enchantments, a critical hit).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Hit {
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

impl Hit {
    /// The player that made the hit, by hand or through a projectile it shot.
    fn player_cause(&self) -> Option<SessionId> {
        match self.source {
            DamageSource::Child {
                owner: Some(Actor::Player(shooter)),
                ..
            } => Some(shooter.session()),
            _ => self.attacker,
        }
    }
}

/// What [`assess`] decided for a hit that lands.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Landed {
    hit: Hit,
    /// The modifiers added up before the ones [`finish`] works out from the player.
    sum: f64,
    /// The modifiers before the ones [`finish`] works out from the player.
    before: Modifiers,
    /// The damage after every modifier.
    final_damage: f64,
    /// The absorption the hit used (0 or negative).
    absorption: f64,
    /// Whether a totem keeps the player alive.
    totem: bool,
    /// Every modifier, as the event shows them.
    modifiers: Modifiers,
    /// The knockback force of a cold hit.
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

    /// Makes the hit what the listeners left in `event` on `player` as they left it. The
    /// absorption, helmet and totem modifiers are worked out again ([`finish`]) from the other
    /// modifiers and from the player now, and written back into the event for its monitors: a
    /// listener that changed the damage, the absorption or the hands neither wastes a totem nor
    /// is saved by one it took away. Unchanged, they are bit for bit what no listener gives. A
    /// listener that set one of those three
    /// itself, or changed a self-kill, is followed as it left the event; the absorption modifier
    /// can only use absorption up.
    fn change(&mut self, event: &mut EntityDamage, player: &Player) {
        let bits = |m: &Modifiers| m.map(|v| v.map(f64::to_bits));
        let (now, then) = (bits(&event.modifiers), bits(&self.modifiers));
        let changed = event.base_damage.to_bits() != self.hit.base.to_bits() || now != then;
        let own = FINISHED
            .iter()
            .any(|&m| now.get(m as usize) != then.get(m as usize));
        if own || self.hit.cause == Damage::Suicide {
            if changed {
                self.hit.base = event.base_damage;
                self.final_damage = event.final_damage();
                self.absorption = math::min(event.modifier(DamageModifier::Absorption), 0.0);
                self.totem = event.modifier(DamageModifier::Totem) < 0.0;
                self.modifiers = event.modifiers;
            }
        } else {
            let (sum, before) = if changed {
                let mut before = event.modifiers;
                for m in FINISHED {
                    if let Some(slot) = before.get_mut(m as usize) {
                        *slot = None;
                    }
                }
                let sum = before.into_iter().flatten().fold(0.0, |sum, v| sum + v);
                (sum, before)
            } else {
                (self.sum, self.before)
            };
            self.hit.base = event.base_damage;
            (
                self.final_damage,
                self.absorption,
                self.totem,
                self.modifiers,
            ) = finish(player, self.hit.cause, event.base_damage, sum, before);
            event.modifiers = self.modifiers;
        }
        self.knockback = event.knockback;
        self.vertical_limit = event.vertical_knockback_limit;
        self.cooldown = event.attack_cooldown_ticks;
    }
}

/// What asks for a respawn ([`respawn`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Trigger {
    /// The client's respawn button: its client was sent the respawn position when it said it
    /// was ready ([`on_respawn`]).
    Client,
    /// [`Player::respawn`](crate::player::Player::respawn): its client, which has not asked, is
    /// sent the respawn position too.
    Api,
}

/// The official damage cause number of `cause`, which the hurt event of a player or an entity
/// carries (Mojang `ActorDamageCause`; P6: an attack 2, a projectile 3, drowning 9).
pub(in crate::tick) fn actor_cause(cause: DamageCause) -> i32 {
    match cause {
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
        DamageCause::Custom => 0,
        DamageCause::Starvation => 16,
        DamageCause::FallingBlock => 19,
    }
}

/// Deals `amount` damage for `cause`; false if no damage was taken. A melee hit names its player
/// as the source.
pub(super) fn damage(state: &mut State, session: SessionId, amount: f64, cause: Damage) -> bool {
    let source = match cause {
        Damage::Attack(a) => DamageSource::Entity(Actor::Player(player::Id::new(a))),
        _ => DamageSource::None,
    };
    damage_by(state, session, amount, cause, source).is_some()
}

/// Deals `amount` damage for `cause` from `source`; `None` if no damage was taken, otherwise the
/// knockback force and vertical limit the [`EntityDamage`] listeners left, for a cause that
/// pushes the victim itself (a projectile, an explosion). A player in `source` is the attacker:
/// its Strength counts, the victim is knocked back from it and the victim's thorns answer it.
pub(super) fn damage_by(
    state: &mut State,
    session: SessionId,
    amount: f64,
    cause: Damage,
    source: DamageSource,
) -> Option<(f64, f64)> {
    if !(0.0..=MAX_DAMAGE).contains(&amount) {
        return None;
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
        &Hit {
            cause,
            base: amount,
            extra,
            parts,
            attacker,
            thorns: attacker.is_some(),
            source,
        },
    )
    .map(|landed| (landed.knockback, landed.vertical_limit))
}

/// Lets `attacker` hit `target`: the held item's attack points, the attacker's effects,
/// sharpness, a critical hit (falling, not flying, blind or under water, and not sprinting under
/// `critical-hits = "sprint"`) for half again; cancelled out of reach, in spectator mode or with
/// `pvp = false`; nothing at all for a dead target or one in another world.
pub(super) fn attack_player(state: &mut State, attacker: SessionId, target: SessionId) {
    // The target is found in the attacker's world alone, and a dead one is left at once: no
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
    let landed = hurt(state, target, &hit);
    swing(state, attacker);
    let Some(Landed { modifiers, .. }) = landed else {
        hit_sound(state, target, Sound::EntityAttackNoDamage);
        return;
    };
    hit_sound(state, target, Sound::EntityAttack);
    if let Some(target_runtime) = player_runtime_id(state, target) {
        for (modifier, animation) in [
            (
                DamageModifier::Critical,
                Animation::CriticalHit {
                    particles: Animation::CRITICAL_HIT_PARTICLES,
                },
            ),
            (
                DamageModifier::WeaponEnchantments,
                Animation::MagicHit {
                    particles: Animation::MAGIC_HIT_PARTICLES,
                },
            ),
        ] {
            // As the listeners left the hit: what added nothing shows nothing.
            if modifiers
                .get(modifier as usize)
                .copied()
                .flatten()
                .is_some_and(|v| v > 0.0)
            {
                broadcast_from(state, target, &animation.packet(target_runtime));
            }
        }
    }
    after_melee(state, attacker, &held, |state, knockback, fire| {
        if let Some(force) = knockback {
            let knock = (force, MAX_KNOCKBACK_VERTICAL);
            knock_back_from(state, target, attacker, knock, entity::Raise::Events);
        }
        if let Some(seconds) = fire {
            let by = CombustSource::Entity(Actor::Player(player::Id::new(attacker)));
            combust(state, target, seconds, by);
        }
    });
}

/// Checks a hit, asks the [`EntityDamage`] listeners and applies it; the hit as the listeners
/// left it, `None` if it did not land.
fn hurt(state: &mut State, session: SessionId, hit: &Hit) -> Option<Landed> {
    let landed = assess(state, session, hit)?;
    let id = player::Id::new(session);
    let Ok(mut event) = super::ask(
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
    if let (Some(e), Some(player)) = (&mut event, state.players.get(&session)) {
        landed.change(e, player);
    }
    apply(state, session, &landed, entity::Raise::Events);
    super::watch(state, event.as_ref());
    Some(landed)
}

/// Deals `amount` damage for `cause` with no source and no events, for the direct API (an explosion
/// through `Context::explode`: a direct API call never re-enters events); false if no damage was
/// taken. What follows from it happens at the player's survival sync, raising its events there: a
/// death (with the generic death message, as `Player::kill`).
pub(super) fn damage_quiet(
    state: &mut State,
    session: SessionId,
    amount: f64,
    cause: Damage,
) -> bool {
    if !(0.0..=MAX_DAMAGE).contains(&amount) {
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
    let Some(landed) = assess(state, session, &hit) else {
        return false;
    };
    apply(state, session, &landed, entity::Raise::Quiet);
    true
}

/// Works out whether a hit lands and how much it deals; `None` if it is cancelled. Each modifier
/// is kept for the event as well as added up, in the order the rule adds them. A system the
/// settings turn off hurts no one, whatever deals the hit (`fall-damage`: an ender pearl's fall
/// too; `drowning`; `pvp`: another player's melee, projectile or explosion), and with
/// `armor = false` worn armor reduces nothing.
fn assess(state: &mut State, session: SessionId, hit: &Hit) -> Option<Landed> {
    let tick = state.tick;
    let seed = world_seed(state, session)?;
    let settings = &state.settings;
    let player = state.players.get_mut(&session)?;
    let switches = player.survival.switches;
    let spared = match hit.cause {
        Damage::Fall => !settings.fall_damage,
        Damage::Drowning | Damage::Suffocation => !switches.drowning,
        Damage::Suicide => false,
        _ => {
            !settings.pvp
                && hit
                    .player_cause()
                    .is_some_and(|attacker| attacker != session)
        }
    };
    if spared || !lands(player, hit) {
        return None;
    }
    let armor = switches.armor;
    let base = hit.base;
    let mut sum = hit.extra;
    let mut modifiers = hit.parts;
    // A self-kill skips the modifiers.
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
        if armor && hit.cause.event_cause().is_reduced_by_armor() {
            let d = total(base, sum) * f64::from(armor_points(player)) * 0.04;
            sum -= d;
            set(&mut modifiers, DamageModifier::Armor, -d);
        }
        let resistance = f64::from(s.effect_level(Effect::Resistance));
        if resistance > 0.0 && hit.cause != Damage::Void {
            let d = total(base, sum) * math::min(1.0, 0.2 * resistance);
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
            let d = total(base, sum) * reduction * 0.04;
            sum -= d;
            set(&mut modifiers, DamageModifier::ArmorEnchantments, -d);
        }
    }
    let (final_damage, absorption, totem, finished) = if hit.cause == Damage::Suicide {
        (total(base, sum), 0.0, false, modifiers)
    } else {
        finish(player, hit.cause, base, sum, modifiers)
    };
    Some(Landed {
        hit: *hit,
        sum,
        before: modifiers,
        final_damage,
        absorption,
        totem,
        modifiers: finished,
        knockback: KNOCKBACK,
        vertical_limit: MAX_KNOCKBACK_VERTICAL,
        cooldown: u32::from(ATTACK_COOLDOWN_TICKS),
    })
}

/// Whether a hit can land on the player at all: spawned and alive, never creative, not just
/// respawned, a spectator only in the void, no fall for a player that may fly, fire resistance
/// against fire, and within the hit cooldown only a stronger hit. Nothing but death, creative or
/// not being spawned spares a player from [`Damage::Suicide`].
fn lands(player: &Player, hit: &Hit) -> bool {
    let s = &player.survival;
    let mode = s.mode();
    let self_kill = hit.cause == Damage::Suicide;
    player.phase == Phase::Spawned
        && !s.is_dead()
        && mode != GameMode::Creative
        && (self_kill || s.no_damage_ticks == 0)
        && (mode != GameMode::Spectator || matches!(hit.cause, Damage::Void | Damage::Suicide))
        && !(hit.cause == Damage::Fall && s.can_fly())
        && !(hit.cause.is_fire() && s.effects.contains_key(&Effect::FireResistance))
        && (self_kill || s.attack_time == 0 || s.last_damage.is_none_or(|last| last < hit.base))
}

/// The last modifiers of a hit of `base` damage for `cause` on `player` as it is now, after
/// `sum`, the modifiers before them added up: the absorption it uses, a helmet against a falling
/// block (with `armor` on), and a held totem's compensation where the hit would kill. The result
/// is the final damage, the absorption used (0 or negative), whether the totem saves the player,
/// and `modifiers` with those three added.
fn finish(
    player: &Player,
    cause: Damage,
    base: f64,
    mut sum: f64,
    mut modifiers: Modifiers,
) -> (f64, f64, bool, Modifiers) {
    let s = &player.survival;
    let absorption = -math::min(s.absorption, total(base, sum));
    sum += absorption;
    set(&mut modifiers, DamageModifier::Absorption, absorption);
    let helmet = || {
        player
            .inventory
            .armor()
            .first()
            .is_some_and(|h| h.item_type().and_then(Type::armor).is_some())
    };
    if s.switches.armor && cause == Damage::FallingBlock && helmet() {
        let d = total(base, sum) / 4.0;
        sum -= d;
        set(&mut modifiers, DamageModifier::ArmorHelmet, -d);
    }
    let mut totem = false;
    if cause != Damage::Void && holds_totem(player) {
        let compensation = s.health - total(base, sum) - 1.0;
        if compensation <= -1.0 {
            sum += compensation;
            totem = true;
            set(&mut modifiers, DamageModifier::Totem, compensation);
        }
    }
    (total(base, sum), absorption, totem, modifiers)
}

/// The damage of a hit of `base` with `sum` added, 0 if that is not positive.
fn total(base: f64, sum: f64) -> f64 {
    math::max(base + sum, 0.0)
}

/// Applies a hit that landed: the health, the cooldown, knockback, hurt animation and the
/// effects that follow it. A quiet hit raises no events: its death waits for the player's
/// survival sync, its exhaustion and totem effects go in directly. With `armor = false` worn
/// armor neither wears nor answers with thorns.
fn apply(state: &mut State, session: SessionId, landed: &Landed, raise: entity::Raise) {
    let Landed {
        hit,
        sum: _,
        before: _,
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
    let runtime_id = player.runtime_id;
    let s = &mut player.survival;
    s.last_damage = Some(hit.base);
    s.last_cause = Some((hit.cause.event_cause(), hit.source));
    s.health = math::max(s.health - final_damage, 0.0);
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
        // A projectile names its shooter; one no player shot is a generic death.
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
            // Only a cactus is named for contact.
            Damage::Contact if !is_cactus(state, session, hit.source) => GENERIC_DEATH,
            cause => cause.death_message(),
        };
        die(state, session, message, attacker);
        return;
    }
    if cold {
        if let Some(attacker) = hit.attacker {
            knock_back_from(state, session, attacker, (knockback, vertical_limit), raise);
        }
        let packet = actor_event(runtime_id, ActorEvent::Hurt, hit.cause.actor_cause());
        broadcast_from(state, session, &packet);
    }
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let s = &mut player.survival;
    s.absorption = math::max(s.absorption + absorption, 0.0);
    s.mark(ABSORPTION);
    let armor = s.switches.armor;
    if armor && hit.cause.event_cause().is_reduced_by_armor() {
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
            food::exhaust_as(state, session, DAMAGE_EXHAUSTION, ExhaustCause::Damage);
        }
        entity::Raise::Quiet => {
            if let Some(player) = state.players.get_mut(&session) {
                food::exhaust(&mut player.survival, DAMAGE_EXHAUSTION);
            }
        }
    }
}

/// Whether `source` is a thrown trident.
fn is_trident(state: &mut State, source: DamageSource) -> bool {
    let DamageSource::Child { child, .. } = source else {
        return false;
    };
    Context { state }
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

/// Heals a player for `reason` up to its maximum health. Nothing happens, and no
/// [`EntityRegainHealth`] is raised, unless the amount [`heals`] it.
pub(super) fn heal(state: &mut State, session: SessionId, amount: f64, reason: RegainReason) {
    let heals_now = |state: &State, amount: f64| {
        state.players.get(&session).is_some_and(|p| {
            let s = &p.survival;
            heals(!s.is_dead(), s.health, s.max_health(), amount)
        })
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

/// Heals a player that is alive.
pub(super) fn heal_player(player: &mut Player, amount: f64) {
    let s = &mut player.survival;
    let max = s.max_health();
    if !heals(!s.is_dead(), s.health, max, amount) {
        return;
    }
    s.health = math::min(s.health + amount, max);
    s.mark(HEALTH);
    player.data_dirty = true;
}

/// Whether `amount` heals a player or an entity at `health` of `max_health`, the check before
/// [`EntityRegainHealth`] and again after its listeners: it is alive and below its maximum, and
/// the amount is positive.
pub(in crate::tick) fn heals(alive: bool, health: f64, max_health: f64, amount: f64) -> bool {
    alive && amount > 0.0 && health < max_health
}

/// The armor points the player wears.
pub(super) fn armor_points(player: &Player) -> u32 {
    player
        .inventory
        .armor()
        .iter()
        .filter_map(Stack::item_type)
        .map(|i| u32::from(i.armor_points()))
        .sum()
}

/// The protection factor of the armor for a cause: ⌊(6 + level²) × type modifier / 3⌋ for each
/// protection enchantment that applies.
fn protection_factor(player: &Player, cause: Damage) -> u32 {
    let applies = |e: Enchantment| match e {
        Enchantment::BlastProtection => {
            matches!(cause, Damage::Explosion | Damage::BlockExplosion).then_some(1.5)
        }
        Enchantment::FeatherFalling => (cause == Damage::Fall).then_some(2.5),
        Enchantment::FireProtection => cause.is_fire().then_some(1.25),
        Enchantment::ProjectileProtection => (cause == Damage::Entity).then_some(1.5),
        Enchantment::Protection => Some(0.75),
        _ => None,
    };
    let mut total: u32 = 0;
    for stack in player.inventory.armor() {
        if stack.item_type().and_then(Type::armor).is_none() {
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
pub(super) fn epf(level: u16, modifier: f64) -> u32 {
    let l = f64::from(level);
    let v = ((6.0 + l * l) * modifier / 3.0).floor();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "invariant: (6 + 65,535²) × 2.5 / 3 is below u32::MAX and positive"
    )]
    let v = v as u32;
    v
}

/// Whether the player holds a totem in its main hand or its off hand.
fn holds_totem(player: &Player) -> bool {
    is_totem(player.inventory.held()) || is_totem(player.inventory.offhand())
}

/// Uses up the totem that saved the player: effects cleared, then regeneration II and fire
/// resistance II for 40 s and absorption II for 5 s, the totem animation and sound; the totem in
/// the main hand, else the one in the off hand, is used up.
fn use_totem(state: &mut State, session: SessionId, raise: entity::Raise) {
    let given = [
        (Effect::Regeneration, 40 * Context::TICKS_PER_SECOND),
        (Effect::FireResistance, 40 * Context::TICKS_PER_SECOND),
        (Effect::Absorption, 5 * Context::TICKS_PER_SECOND),
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
    if let Some(runtime_id) = player_runtime_id(state, session) {
        let packet = actor_event(runtime_id, ActorEvent::TalismanActivate, 0);
        broadcast_from(state, session, &packet);
    }
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

/// Whether a stack is a totem of undying.
fn is_totem(stack: &Stack) -> bool {
    stack.item_type().is_some_and(|i| i.is(Class::Totem))
}

/// Wears the armor after a hit: ⌊damage / 4⌋ points, at least 1.
fn damage_armor(state: &mut State, session: SessionId, base: f64) {
    let tick = state.tick;
    let Some(seed) = world_seed(state, session) else {
        return;
    };
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
    for slot in 0..player.inventory.armor().len() {
        let Some(stack) = player.inventory.armor().get(slot) else {
            continue;
        };
        if stack.item_type().and_then(Type::armor).is_none() {
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

/// Lets thorns on the victim's armor hurt the attacker: each piece with level L answers with
/// chance 15 % × L, dealing 1 to 4 (L - 10 above level 10) and wearing the piece by 3, else
/// wearing it by 1; the wearer's Strength adds to the answer as to any hit by it. A thorns hit is
/// not answered again.
fn thorns(state: &mut State, victim: SessionId, attacker: SessionId) {
    let tick = state.tick;
    let Some(seed) = world_seed(state, victim) else {
        return;
    };
    let Some(player) = state.players.get_mut(&victim) else {
        return;
    };
    let mut r = player_random(seed, tick, player, THORNS);
    let mut dealt = 0.0;
    let mut broke = false;
    for slot in 0..player.inventory.armor().len() {
        let Some(stack) = player.inventory.armor().get(slot) else {
            continue;
        };
        if stack.item_type().and_then(Type::armor).is_none() {
            continue;
        }
        let level = u32::from(drops::level(stack, Enchantment::Thorns));
        if level == 0 {
            continue;
        }
        let (wear_by, damage) = if r.below(100) < level * 15 {
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
            &Hit {
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

/// Knocks the player back away from another player, at most `vertical_limit` upward, as every
/// push of a player goes (`entity::knock_player`).
fn knock_back_from(
    state: &mut State,
    session: SessionId,
    from: SessionId,
    (force, vertical_limit): (f64, f64),
    raise: entity::Raise,
) {
    let Some(source) = state.players.get(&from).map(feet) else {
        return;
    };
    let Some(target) = state.players.get(&session).map(feet) else {
        return;
    };
    let away = (target.x - source.x, target.z - source.z);
    entity::knock_player(state, session, away, (force, vertical_limit), raise);
}

/// Lets `source` set the player on fire for `seconds`: the [`EntityCombust`] listeners may cancel
/// it or change the time. The monitors hear it only if the player burns longer than before.
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
    let longer = state
        .players
        .get_mut(&session)
        .is_some_and(|player| ignite(player, seconds));
    if longer {
        super::watch(state, event.as_ref());
    }
}

/// Ignites the player for `seconds`: fire protection shortens it by 15 % per level of the
/// best piece; a longer burn replaces a shorter one. True if the player burns longer than before.
pub(super) fn ignite(player: &mut Player, seconds: u32) -> bool {
    let level = f64::from(protection_level(player, Enchantment::FireProtection));
    let s = f64::from(seconds);
    let cut = math::min(s, s * level * 0.15).floor();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "invariant: 0 <= cut <= seconds, which fits u32"
    )]
    let seconds = seconds - cut as u32;
    let ticks = seconds
        .saturating_mul(Context::TICKS_PER_SECOND)
        .min(MAX_FIRE_TICKS);
    let ticks = u16::try_from(ticks).unwrap_or(u16::MAX);
    let longer = ticks > player.survival.fire_ticks;
    if longer {
        player.survival.fire_ticks = ticks;
        player.data_dirty = true;
    }
    longer
}

/// The highest level of a protection enchantment on the armor worn, 0 with `armor = false`: the
/// armor then protects from nothing.
pub(in crate::tick) fn protection_level(player: &Player, enchantment: Enchantment) -> u32 {
    if player.survival.switches.armor {
        highest_armor_level(player, enchantment)
    } else {
        0
    }
}

/// The highest level of an enchantment on the armor worn.
pub(super) fn highest_armor_level(player: &Player, enchantment: Enchantment) -> u32 {
    player
        .inventory
        .armor()
        .iter()
        .map(|s| u32::from(drops::level(s, enchantment)))
        .max()
        .unwrap_or(0)
}

/// Finishes a melee hit by the player `attacker` with `held` that landed, on a player or an
/// entity: `enchant` applies the weapon's Knockback force (0.5 per level) and Fire Aspect burn
/// (4 seconds per level) to the target, each `None` without the enchantment; then, if the
/// attacker is still alive (a reactive hit such as thorns may have killed it), its weapon wears
/// and its exhaustion grows.
pub(super) fn after_melee(
    state: &mut State,
    attacker: SessionId,
    held: &Stack,
    enchant: impl FnOnce(&mut State, Option<f64>, Option<u32>),
) {
    let knockback = drops::level(held, Enchantment::Knockback);
    let fire = u32::from(drops::level(held, Enchantment::FireAspect));
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

/// Plays a hit sound at the middle of the player `target`.
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
        damage: math::max(total, 0.0),
        critical,
        magic,
    })
}

/// The attacker's side of a melee hit by `a`: the held item's attack points, the extra of
/// Strength, Weakness and Sharpness (added up and one by one), a critical hit (falling, not
/// flying, not blind, not under water, and not sprinting under `critical-hits = "sprint"`) for
/// half again; whether the critical hit and a weapon enchantment added damage (their animations
/// show only then).
fn melee_parts(
    state: &State,
    attacker: SessionId,
    a: &Player,
) -> (f64, f64, Modifiers, bool, bool) {
    let held = a.inventory.held();
    let base = f64::from(held.item_type().map_or(1, Type::attack_points));
    let (mut extra, mut parts) =
        attacker_modifiers(state, attacker, base, Damage::Attack(attacker));
    let sharpness = drops::level(held, Enchantment::Sharpness);
    let weapon = if sharpness > 0 {
        f64::midpoint(f64::from(sharpness), 1.0)
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

/// Works out the attacker's strength and weakness modifiers, added up and one by one.
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

/// Sets `modifier` in `modifiers` to `value`.
fn set(modifiers: &mut Modifiers, modifier: DamageModifier, value: f64) {
    if let Some(slot) = modifiers.get_mut(modifier as usize) {
        *slot = Some(value);
    }
}

/// Whether the player reaches a position to hit it ([`can_interact`] within 8 blocks).
fn can_reach(player: &Player, pos: Vec3) -> bool {
    let d = super::look_direction(player);
    can_interact(player.position, d, pos, ENTITY_REACH)
}

/// Shows the attacker's arm swing to the players that see it.
pub(super) fn swing(state: &mut State, session: SessionId) {
    let Some(runtime_id) = player_runtime_id(state, session) else {
        return;
    };
    let packet = actor_event(runtime_id, ActorEvent::StartAttacking, 0);
    broadcast_to_viewers(state, session, &packet);
}

/// Wears the weapon `held` that made a hit: swords, tridents and hoes by 1, axes, pickaxes and
/// shovels by 2, with the item-break sound when it breaks. Nothing for a player without finite
/// resources, or whose held item changed since the hit.
fn wear_weapon(state: &mut State, session: SessionId, held: &Stack) {
    let tick = state.tick;
    let Some(seed) = world_seed(state, session) else {
        return;
    };
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if !super::has_finite_resources(player.survival.mode()) || player.inventory.held() != held {
        return;
    }
    let Some(item) = held.item_type() else {
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

/// Wears a durable stack by `amount` (the one rule of [`interact::breaking::damage`]); `None` if
/// the stack does not change.
pub(super) fn wear(
    settings: &Settings,
    stack: &Stack,
    amount: u32,
    r: &mut Random,
) -> Option<Stack> {
    let worn = interact::breaking::damage(settings, stack, amount, r);
    (worn != *stack).then_some(worn)
}

/// Kills the player: its window closes, its breaking and item use stop, the [`PlayerDeath`]
/// listeners may change what drops, the xp dropped and the messages, it drops what it carries and
/// its xp, every player sees the chat death message ([`broadcast_death_message`]), viewers see it
/// die and it sees the death screen with `message` (a translation key) naming the player and its
/// `attacker`.
///
/// While earlier drops still wait for room, the player keeps what it carries and nothing more
/// drops: the waiting list then never holds more than one death's drops
/// ([`MAX_HELD_BACK`](super::MAX_HELD_BACK)). Of the drops a listener replaced, the player's own
/// stacks the event showed are gone, and every other stack it carries drops or stays as without
/// a listener ([`replace_drops`]).
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
    let orbs = state.settings.has_orbs();
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
    let runtime_id = player.runtime_id;
    let default_chat = if state.settings.show_death_messages {
        message
    } else {
        ""
    };
    let mut event = heard.then(|| {
        let drops = if player.survival.held_back.is_empty() {
            carried(player)
        } else {
            Vec::new()
        };
        PlayerDeath {
            player: player::Id::new(session),
            drops,
            xp_drop_amount: if orbs {
                food::death_xp(&player.survival)
            } else {
                0
            },
            death_message: default_chat.to_owned(),
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
        event.as_mut().zip(original.as_deref()),
    );
    let chat = event
        .as_ref()
        .map_or_else(|| default_chat.to_owned(), |e| e.death_message.clone());
    broadcast_death_message(state, session, message, attacker, chat);
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
    // The viewers hold the dying box now: the flush must not take it for the pose's box.
    if let Some(player) = state.players.get_mut(&session) {
        player.survival.shown.size = attributes::DYING_SIZE;
    }
    if state.settings.locator_bar {
        for packet in entity::locator_hidden(runtime_id) {
            broadcast_to_viewers(state, session, &packet);
        }
    }
    let screen = event
        .as_ref()
        .map(|e| e.death_screen_message.clone())
        .filter(|m| !m.is_empty() && player::check_text(m).is_ok());
    if let Some(e) = &mut event {
        e.death_screen_message = screen.clone().unwrap_or_default();
    }
    show_death(state, session, message, attacker, screen);
    super::watch(state, event.as_ref());
}

/// Tells every player the chat death message `chat` of the player of `session` (none if empty):
/// with `show-death-messages`, the death screen's `message` translated as the death screen does,
/// naming the player and its `attacker`; any other text (a listener's own) as it is.
fn broadcast_death_message(
    state: &mut State,
    session: SessionId,
    message: &'static str,
    attacker: Option<SessionId>,
    chat: String,
) {
    if chat.is_empty() {
        return;
    }
    let line = if state.settings.show_death_messages && chat == message {
        player::Message::Translation {
            key: chat,
            params: death_names(state, session, attacker),
        }
    } else {
        player::Message::Raw(chat)
    };
    if let Err(err) = crate::tick::player::broadcast(state, line) {
        debug!(
            session = session.get(),
            error = &err as &(dyn std::error::Error + 'static),
            "death message not sent"
        );
    }
}

/// Drops what a dead player carries and its xp at `place`, as the [`PlayerDeath`] listeners left
/// the `event` (with the drops it started with) if one was raised: kept inventory, replaced
/// drops, kept or changed xp. The event then holds what was applied: the stacks that entered the
/// world and the xp dropped, for the monitors.
fn drop_death(
    state: &mut State,
    session: SessionId,
    place: (WorldIndex, Vec3),
    mut event: Option<(&mut PlayerDeath, &[Stack])>,
) {
    let (world, at) = place;
    let orbs = state.settings.has_orbs();
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
    let to_drop = match &event {
        Some((e, original)) if free && e.drops != *original => {
            replace_drops(stacks, original, e.drops.clone())
        }
        _ => stacks,
    };
    let xp = if keep_xp {
        0
    } else {
        let xp = food::take_death_xp(&mut player.survival);
        // Without orbs nothing drops, whatever a listener asked for.
        if orbs {
            event.as_ref().map_or(xp, |(e, _)| e.xp_drop_amount)
        } else {
            0
        }
    };
    let mut dropped = Vec::new();
    let refused = drop_all(to_drop, |stack| {
        if entity::drop_item(state, world, at, stack.clone()) {
            dropped.push(stack);
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
        entity::drop_xp(state, world, at, xp);
    }
    if let Some((e, _)) = &mut event {
        e.drops = dropped;
        e.xp_drop_amount = xp;
    }
}

/// What a dead player drops if nothing changes it: its main slots, armor and off hand, without
/// items that vanish or stay on death.
fn carried(player: &Player) -> Vec<Stack> {
    let inv = &player.inventory;
    (0..=u8::MAX)
        .map_while(|slot| inv.main_stack(slot))
        .chain(inv.armor())
        .chain(std::iter::once(inv.offhand()))
        .filter(|s| {
            !s.is_empty() && drops::level(s, Enchantment::Vanishing) == 0 && !keeps_on_death(s)
        })
        .cloned()
        .collect()
}

/// What a dead player drops when [`PlayerDeath`] listeners replaced the `original` drops with
/// `drops`: those, and every stack it carried (`stacks`) that the original drops did not show,
/// such as the stacks waiting for room or what a listener gave it, so no item is lost.
fn replace_drops(stacks: Vec<Stack>, original: &[Stack], mut drops: Vec<Stack>) -> Vec<Stack> {
    let mut shown = original.to_vec();
    for stack in stacks {
        if let Some(i) = shown.iter().position(|s| *s == stack) {
            shown.swap_remove(i);
        } else {
            drops.push(stack);
        }
    }
    drops
}

/// Shows the death screen: the respawn position being searched, at the place of the death as
/// the official server sends it (P6), and the death message (a translation key with the
/// player's and the attacker's names), or the `screen` text a [`PlayerDeath`] listener chose.
fn show_death(
    state: &mut State,
    session: SessionId,
    message: &'static str,
    attacker: Option<SessionId>,
    screen: Option<String>,
) {
    let (cause, messages) = match screen {
        Some(text) => (text, Vec::new()),
        None => (message.to_owned(), death_names(state, session, attacker)),
    };
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    player.survival.motion.death_shown = true;
    player.send_packet(respawn_packet(
        player.position,
        RespawnState::SearchingForSpawn,
    ));
    player.send_packet(Packet::DeathInfo(DeathInfo { cause, messages }));
}

/// The names a death message translates with: the dead player's, then its `attacker`'s if it is
/// still online.
fn death_names(state: &State, session: SessionId, attacker: Option<SessionId>) -> Vec<String> {
    [Some(session), attacker]
        .into_iter()
        .flatten()
        .filter_map(|s| state.players.get(&s).map(|p| p.identity.name.clone()))
        .collect()
}

/// Drops a dead player's stacks: curse of vanishing destroys its items; items kept on death and
/// the stacks `drop` refuses (the world's entity limit) are returned.
pub(super) fn drop_all(
    stacks: Vec<Stack>,
    mut drop: impl FnMut(Stack) -> Option<Stack>,
) -> Vec<Stack> {
    let mut refused = Vec::new();
    for stack in stacks {
        if stack.is_empty() || drops::level(&stack, Enchantment::Vanishing) > 0 {
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

/// Whether a stack stays with its owner on death.
fn keeps_on_death(stack: &Stack) -> bool {
    stack.extra().is_some_and(|e| {
        matches!(
            e.nbt.get(crate::item::TAG_KEEP_ON_DEATH),
            Some(crate::nbt::Value::Byte(1))
        )
    })
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

/// Takes a `Respawn` from the client: on the death screen, the client is ready to spawn and gets
/// the search (as the official server sends it, P6) and the respawn position. A player already
/// back through the API gets where it is: its client may have asked before it heard of that
/// respawn, and stays dead until it is answered. Anything else is ignored.
pub(super) fn on_respawn(state: &mut State, session: SessionId, packet: &Respawn) {
    let Some(player) = state.players.get(&session) else {
        return;
    };
    if player.phase != Phase::Spawned || packet.state != u8::from(RespawnState::ClientReadyToSpawn)
    {
        return;
    }
    let position = if player.survival.is_dead() {
        respawn_point(state, session).1
    } else {
        player.position
    };
    if let Some(player) = state.players.get_mut(&session) {
        player.send_packet(respawn_packet(
            RESPAWN_SEARCH_POSITION,
            RespawnState::SearchingForSpawn,
        ));
        player.send_packet(respawn_packet(position, RespawnState::ReadyToSpawn));
    }
}

/// Respawns the player at its respawn point, in another world if the point is there, or where a
/// [`PlayerRespawn`] listener sends it (a world the server does not run or a place outside the
/// world border is ignored); nothing unless it is dead. In hardcore the player comes back as a
/// spectator, unless a [`PlayerGameModeChange`](crate::event::PlayerGameModeChange) listener
/// cancels that. Another world raises [`PlayerWorldChange`](crate::event::PlayerWorldChange)
/// before the [`PlayerRespawn`] monitors, unless the player is leaving (`stay`). The client that
/// did not ask (`trigger`) gets the respawn position first, without which it stays dead.
pub(super) fn respawn(state: &mut State, session: SessionId, trigger: Trigger, stay: Stay) {
    // A death the API made runs later in the tick (drops, event, death screen); the respawn waits
    // for it.
    let Some(from) = state
        .players
        .get(&session)
        .filter(|p| p.survival.motion.death_shown)
        .map(|p| p.world)
    else {
        return;
    };
    let Some((world, position, event)) = revive(state, session, Phase::Spawned) else {
        return;
    };
    if trigger == Trigger::Api
        && let Some(player) = state.players.get_mut(&session)
    {
        player.send_packet(respawn_packet(position, RespawnState::ReadyToSpawn));
    }
    super::teleport(state, session, world, position);
    let burning = restore(state, session);
    let Some(player) = state.players.get(&session) else {
        return;
    };
    // The official server's order (P6): the respawn event, the health, the attributes, then the
    // entity data.
    let alive = actor_event(player.runtime_id, ActorEvent::SpawnAlive, 0);
    let mut out = vec![
        Packet::SetHealth(packet::SetHealth {
            health: super::health_shown(player),
        }),
        attributes::all_attributes(player),
        attributes::update_abilities(player),
    ];
    inventory::spawn_packets(player, &mut out);
    let data = attributes::actor_data(player.runtime_id, attributes::full_metadata(player), 0);
    let size = attributes::size_data(player.runtime_id, attributes::size(&player.survival));
    let health = attributes::health_packet(player);
    let now = attributes::shown(player);
    for packet in [&alive, &data, &size, &health] {
        broadcast_to_viewers(state, session, packet);
    }
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    // The player gets its own straight away: it may respawn where its client has no chunks yet
    // (another world, far away), so it does not see itself as a viewer would.
    for packet in std::iter::once(alive).chain(out).chain([data, size]) {
        player.send_packet(packet);
    }
    // The packets above carry all but the adventure settings, which a listener may have changed.
    player.survival.dirty &= attributes::ADVENTURE;
    player.survival.shown = now;
    let runtime_id = state.players.get(&session).map(|p| p.runtime_id);
    if let Some(runtime_id) = runtime_id.filter(|_| state.settings.locator_bar) {
        let packet = entity::locator_visible(runtime_id);
        broadcast_to_viewers(state, session, &packet);
    }
    if burning {
        extinguished(state, session, ExtinguishCause::Respawn);
    }
    if stay == Stay::Online {
        super::world_changed(state, session, from);
    }
    super::watch(state, event.as_ref());
}

/// Respawns a player whose saved data has it dead (it left, or the server stopped, before it
/// respawned) as [`respawn`] does, but before its start-game packets, which then show it alive at
/// its respawn point: a client that spawns dead shows no death screen and never asks to respawn,
/// so it would stay dead. Nothing unless it is dead and joined, not yet spawned.
pub(in crate::tick) fn respawn_joined(state: &mut State, session: SessionId) {
    let Some((world, position, event)) = revive(state, session, Phase::Joined) else {
        return;
    };
    // Its view opens once its client asks for a radius, after the start-game packets.
    if let Some(mut player) = state.players.remove(&session) {
        state.set_world(&mut player, world);
        player.position = position;
        state.players.insert(session, player);
    }
    if restore(state, session) {
        extinguished(state, session, ExtinguishCause::Respawn);
    }
    super::watch(state, event.as_ref());
}

/// Works out where the dead player of `session` in `phase` respawns (see [`respawn`]): the world
/// and eye position, and the [`PlayerRespawn`] event, naming where it goes, to
/// [`watch`](super::watch) once it respawned; a hardcore player becomes a spectator first. `None`
/// if it is not dead in `phase`, before or after the listeners.
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
    let feet_at = super::feet_at(position);
    let world_id = world::Id::of(world);
    let mut event = super::ask(
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
    let target = event
        .as_ref()
        .filter(|e| e.world != world_id || e.position != feet_at)
        .and_then(|e| respawn_target(state, e.world, e.position));
    if let Some((world, position)) = target {
        return Some((world, position, event));
    }
    // A place the listeners chose that is ignored: the monitors see the respawn point used.
    if let Some(e) = &mut event {
        e.world = world_id;
        e.position = feet_at;
    }
    Some((world, position, event))
}

/// The world and eye position the player respawns at: its spawn point in whichever world holds
/// it, else the default world's spawn; the block's center, moved up to the first two free blocks
/// of the loaded chunks ([`world::World::safe_spawn`]). A spawn point in a world that is not
/// loaded, or whose center lies outside the world border, counts as none; with neither, the
/// player respawns where it is.
fn respawn_point(state: &State, session: SessionId) -> (WorldIndex, Vec3) {
    let Some(player) = state.players.get(&session) else {
        return (0, Vec3::ZERO);
    };
    let at = |index: WorldIndex, spawn: BlockPos| {
        let eye = super::standing_eye(state.worlds.get(index)?.safe_spawn(spawn));
        super::check_destination(state, index, eye)
            .is_ok()
            .then_some((index, eye))
    };
    let own = player
        .survival
        .spawn
        .as_ref()
        .and_then(|p| at(state.catalog.named(&p.world)?, p.pos));
    let default = state.catalog.default_world();
    own.or_else(|| at(default, state.worlds.get(default)?.spawn()))
        .unwrap_or((player.world, player.position))
}

/// A `Respawn` packet, with the runtime ID 0 the official server sends (P6).
fn respawn_packet(position: Vec3, respawn_state: RespawnState) -> Packet {
    Packet::Respawn(Respawn {
        position: position.to_f32(),
        state: u8::from(respawn_state),
        entity_runtime_id: 0,
    })
}

/// Restores the respawned player's state: no effects, full health and food, air, no fire, and a
/// short invulnerability. Returns whether it was burning.
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

/// Makes a hardcore player that respawns a spectator, unless a
/// [`PlayerGameModeChange`](crate::event::PlayerGameModeChange) listener cancels that.
fn hardcore_spectator(state: &mut State, session: SessionId) {
    Context { state }.change_game_mode(player::Id::new(session), GameMode::Spectator);
}

/// Where a [`PlayerRespawn`] listener sends the player: the world's index and the eye position;
/// `None` for a world the server does not run (or unloaded) or a place outside the world border.
fn respawn_target(state: &State, world: world::Id, feet: Vec3) -> Option<(WorldIndex, Vec3)> {
    let eye = super::eye_at(feet);
    super::check_destination(state, world.index(), eye)
        .is_ok()
        .then_some((world.index(), eye))
}

/// The runtime ID of a player; `None` if it is gone.
fn player_runtime_id(state: &State, session: SessionId) -> Option<u64> {
    state.players.get(&session).map(|p| p.runtime_id)
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

/// Runs the void, burning and the respawn invulnerability, each tick.
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

/// Tells the [`EntityExtinguish`] listeners and monitors that the player's fire went out for
/// `cause`.
pub(super) fn extinguished(state: &mut State, session: SessionId, cause: ExtinguishCause) {
    let id = player::Id::new(session);
    super::notify(state, || EntityExtinguish {
        entity: Actor::Player(id),
        cause,
    });
}

/// Runs suffocation, air and the hit cooldown, each tick; with `drowning = false` only the hit
/// cooldown.
pub(super) fn living_tick(state: &mut State, session: SessionId) {
    let Some(drowning) = state
        .players
        .get(&session)
        .filter(|p| !p.survival.is_dead())
        .map(|p| p.survival.switches.drowning)
    else {
        return;
    };
    if drowning && air_tick(state, session) {
        damage(state, session, 2.0, Damage::Drowning);
    }
    // After the air's hits: the cooldown's last tick still weighs them against the last hit.
    if let Some(player) = state.players.get_mut(&session)
        && player.survival.attack_time > 0
    {
        player.survival.attack_time -= 1;
    }
}

/// Runs suffocation inside a solid block, then the air supply of a living player; true when it
/// runs out of air now.
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
        // The official server (P6 craft dive): breathing again at once, 4 ticks of air back each
        // tick up to the full supply.
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
            .map_or(0, |helmet| drops::level(helmet, Enchantment::Respiration));
        let loses = respiration == 0 || {
            let seed = state.worlds.get(player.world).map_or(0, world::World::seed);
            let mut r = player_random(seed, tick, player, RESPIRATION);
            r.unit() <= 1.0 / (f64::from(respiration) + 1.0)
        };
        let s = &mut player.survival;
        s.breathing = false;
        if loses {
            s.air -= 1;
            if s.air <= super::MIN_AIR_TICKS {
                s.air = 0;
                drown = true;
            }
            player.data_dirty = true;
        }
    }
    drown
}

/// The fall damage of a landing: the blocks fallen beyond 3, less the jump boost level, rounded
/// up; none while flying.
pub(super) fn fall_damage(s: &Survival, distance: f64) -> f64 {
    if s.motion.flying {
        return 0.0;
    }
    (distance - SAFE_FALL - f64::from(s.effect_level(Effect::JumpBoost))).ceil()
}
