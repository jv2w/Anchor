//! Status effects of players (PMMP `EffectManager`, `EffectCollection`, `entity/effect/*`):
//! adding with PMMP's replacement rule, what each effect does when added, every few ticks and
//! when removed, and the `MobEffect` packets that tell the player.

use super::attributes::{ABSORPTION, HEALTH, MOVEMENT};
use super::damage::{self, Damage};
use super::{Survival, food};
use crate::effect::{self, Instance, MAX_DURATION, Type};
use crate::event::{Actor, EntityEffectAdd, EntityEffectRemove, ExhaustCause, RegainReason};
use crate::player::Id;
use crate::protocol::consts::MobEffectOperation;
use crate::protocol::packet::{MobEffect, Packet};
use crate::tick::{Phase, Player, SessionId, State};

/// An active effect (PMMP `EffectInstance`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Active {
    /// Ticks left; an infinite effect wraps around instead of ending.
    pub(super) duration: u32,
    /// The level minus one.
    pub(super) amplifier: u8,
    /// Whether it shows particles.
    pub(super) visible: bool,
    /// Whether its particles are faint (beacons).
    pub(super) ambient: bool,
    /// Whether it never ends (saved as duration -1).
    pub(super) infinite: bool,
}

crate::tick::checkpoint::fields!(Active {
    duration,
    amplifier,
    visible,
    ambient,
    infinite,
});

impl Active {
    /// The effect a potion, food or the API gives.
    pub(super) fn of(instance: Instance) -> Self {
        Self {
            duration: instance.duration_ticks().min(MAX_DURATION),
            amplifier: instance.amplifier(),
            visible: instance.is_visible(),
            ambient: instance.is_ambient(),
            infinite: instance.is_infinite(),
        }
    }

    /// A plain effect of `duration` ticks at `amplifier`, with particles.
    pub(super) const fn new(duration: u32, amplifier: u8) -> Self {
        Self {
            duration,
            amplifier,
            visible: true,
            ambient: false,
            infinite: false,
        }
    }

    /// The level: amplifier + 1 (PMMP `getEffectLevel`).
    pub(super) fn level(self) -> u32 {
        u32::from(self.amplifier) + 1
    }

    /// The public form of the effect of type `effect`.
    pub(super) fn instance(self, effect: Type) -> Instance {
        Instance::new(
            effect,
            self.duration,
            self.amplifier,
            self.visible,
            self.ambient,
        )
        .with_infinite(self.infinite)
    }

    #[cfg(test)]
    pub(super) fn hash_into(self, h: &mut crate::tick::Fnv) {
        let Self {
            duration,
            amplifier,
            visible,
            ambient,
            infinite,
        } = self;
        h.u64(u64::from(duration));
        h.u64(u64::from(amplifier));
        h.u64(u64::from(visible) | u64::from(ambient) << 1 | u64::from(infinite) << 2);
    }
}

/// Ticks between applications of an effect (PMMP `Effect::getApplyInterval`); 0 never applies.
pub(super) fn interval(effect: Type, amplifier: u8) -> u32 {
    let shifted = |base: u32| base.checked_shr(u32::from(amplifier)).unwrap_or(0).max(1);
    match effect {
        Type::Poison | Type::FatalPoison => shifted(25),
        Type::Wither => shifted(40),
        Type::Regeneration => shifted(50),
        Type::Hunger | Type::InstantDamage | Type::InstantHealth | Type::Saturation => 1,
        _ => 0,
    }
}

/// Whether `new` may replace `old`, both of type `effect` ([`Instance::can_replace`]).
fn can_replace(effect: Type, old: Active, new: Active) -> bool {
    new.instance(effect).can_replace(old.instance(effect))
}

/// Adds an effect to the player (PMMP `EffectManager::add`): false if a stronger or longer one is
/// active or an [`EntityEffectAdd`] listener cancels it. A replaced effect is removed first, so
/// its changes (health boost, speed) undo.
pub(super) fn add(state: &mut State, session: SessionId, effect: Type, new: Active) -> bool {
    let addable = |state: &State| {
        state.players.get(&session).map(|p| {
            let old = p.survival.effects.get(&effect).copied();
            (old.is_none_or(|old| can_replace(effect, old, new)), old)
        })
    };
    let Some((true, old)) = addable(state) else {
        return false;
    };
    let Ok(event) = super::ask(
        state,
        || EntityEffectAdd {
            entity: Actor::Player(Id::new(session)),
            effect: new.instance(effect),
            old_effect: old.map(|o| o.instance(effect)),
            cancelled: false,
        },
        EntityEffectAdd::is_cancelled,
        |state, _| matches!(addable(state), Some((true, _))),
    ) else {
        return false;
    };
    let added = state
        .players
        .get_mut(&session)
        .is_some_and(|player| add_to(player, effect, new));
    super::watch(state, event.as_ref());
    added
}

/// [`add`] on a player at hand.
pub(super) fn add_to(player: &mut Player, effect: Type, new: Active) -> bool {
    let s = &mut player.survival;
    let old = s.effects.get(&effect).copied();
    if old.is_some_and(|old| !can_replace(effect, old, new)) {
        return false;
    }
    if let Some(old) = old {
        s.effects.remove(&effect);
        on_remove(s, effect, old);
    }
    s.effects.insert(effect, new);
    on_add(s, effect, new);
    player.data_dirty = true;
    let operation = if old.is_some() {
        MobEffectOperation::Modify
    } else {
        MobEffectOperation::Add
    };
    if player.phase == Phase::Spawned {
        let packet = packet(player, effect, Some(new), operation);
        player.send_packet(packet);
    }
    true
}

/// Removes an effect, undoing what it changed, unless an [`EntityEffectRemove`] listener keeps
/// it (one that ran out cannot stay).
pub(super) fn remove(state: &mut State, session: SessionId, effect: Type) {
    let active = |state: &State| {
        state
            .players
            .get(&session)
            .and_then(|p| p.survival.effects.get(&effect).copied())
    };
    let Some(old) = active(state) else {
        return;
    };
    let Ok(event) = super::ask(
        state,
        || EntityEffectRemove {
            entity: Actor::Player(Id::new(session)),
            effect: old.instance(effect),
            cancelled: false,
        },
        EntityEffectRemove::is_cancelled,
        |state, _| active(state).is_some(),
    ) else {
        return;
    };
    if let Some(player) = state.players.get_mut(&session) {
        remove_from(player, effect);
    }
    super::watch(state, event.as_ref());
}

/// [`remove`] on a player at hand; false if the effect was not active.
pub(super) fn remove_from(player: &mut Player, effect: Type) -> bool {
    let Some(old) = player.survival.effects.remove(&effect) else {
        return false;
    };
    on_remove(&mut player.survival, effect, old);
    player.data_dirty = true;
    if player.phase == Phase::Spawned {
        // BDS's removal carries the effect's amplifier and the time it had left (P6 `syntax`).
        let packet = packet(player, effect, Some(old), MobEffectOperation::Remove);
        player.send_packet(packet);
    }
    true
}

/// Removes every effect (milk, respawn, a totem), in effect ID order as BDS removes them (P6
/// `syntax` `effect @s clear`).
pub(super) fn clear(state: &mut State, session: SessionId) {
    let mut active = active_types(state, session);
    active.sort_unstable_by_key(|t| t.map(Type::id));
    for effect in active.into_iter().flatten() {
        remove(state, session, effect);
    }
}

/// The active effect types, without allocating (at most one per type).
fn active_types(state: &State, session: SessionId) -> [Option<Type>; effect::TYPE_COUNT] {
    let mut out = [None; effect::TYPE_COUNT];
    if let Some(player) = state.players.get(&session) {
        for (slot, &effect) in out.iter_mut().zip(player.survival.effects.keys()) {
            *slot = Some(effect);
        }
    }
    out
}

/// What an effect changes when it starts (PMMP `Effect::add`).
fn on_add(s: &mut Survival, effect: Type, new: Active) {
    match effect {
        Type::Absorption => {
            let amount = 4.0 * f64::from(new.level());
            if amount > s.absorption {
                s.absorption = amount;
            }
            s.mark(ABSORPTION);
        }
        Type::HealthBoost => s.mark(HEALTH),
        Type::Speed | Type::Slowness => s.mark(MOVEMENT),
        _ => {}
    }
}

/// What an effect undoes when it ends (PMMP `Effect::remove`).
fn on_remove(s: &mut Survival, effect: Type, _old: Active) {
    match effect {
        Type::Absorption => {
            s.absorption = 0.0;
            s.mark(ABSORPTION);
        }
        Type::HealthBoost => {
            let max = s.max_health();
            if s.health > max {
                s.health = max;
            }
            s.mark(HEALTH);
        }
        Type::Speed | Type::Slowness => s.mark(MOVEMENT),
        _ => {}
    }
}

/// The `MobEffect` packet for an effect change.
fn packet(
    player: &Player,
    effect: Type,
    active: Option<Active>,
    operation: MobEffectOperation,
) -> Packet {
    let active = active.unwrap_or(Active::new(0, 0));
    Packet::MobEffect(MobEffect {
        entity_runtime_id: player.runtime_id,
        operation: u8::from(operation),
        effect_type: i32::from(effect.id()),
        amplifier: i32::from(active.amplifier),
        particles: active.visible,
        duration: if active.infinite {
            -1
        } else {
            i32::try_from(active.duration).unwrap_or(i32::MAX)
        },
        tick: 0,
        ambient: active.ambient,
    })
}

/// The `MobEffect` packets for every active effect (a player that just spawned).
pub(super) fn all_packets(player: &Player) -> impl Iterator<Item = Packet> + '_ {
    player
        .survival
        .effects
        .iter()
        .map(|(&effect, &active)| packet(player, effect, Some(active), MobEffectOperation::Add))
}

/// One tick of the living player's effects (PMMP `EffectManager::tick`): each effect acts if its
/// interval divides its remaining duration, then counts down and ends at 0.
pub(super) fn tick(state: &mut State, session: SessionId) {
    if state
        .players
        .get(&session)
        .is_none_or(|p| p.survival.is_dead())
    {
        return;
    }
    for effect in active_types(state, session).into_iter().flatten() {
        let Some(active) = state
            .players
            .get(&session)
            .and_then(|p| p.survival.effects.get(&effect).copied())
        else {
            continue;
        };
        let every = interval(effect, active.amplifier);
        if every > 0 && active.duration % every == 0 {
            apply(state, session, effect, active);
        }
        let Some(player) = state.players.get_mut(&session) else {
            return;
        };
        let Some(current) = player.survival.effects.get_mut(&effect) else {
            continue;
        };
        current.duration = match current.duration.checked_sub(1) {
            Some(d) if d > 0 => d,
            _ if current.infinite => MAX_DURATION,
            _ => 0,
        };
        let expired = current.duration == 0;
        player.data_dirty = true;
        if expired {
            remove(state, session, effect);
        }
    }
}

/// What an effect does when it acts (PMMP `Effect::applyEffect`).
fn apply(state: &mut State, session: SessionId, effect: Type, active: Active) {
    match effect {
        Type::Poison | Type::FatalPoison => {
            let health = state
                .players
                .get(&session)
                .map_or(0.0, |p| p.survival.health);
            if health > 1.0 || effect == Type::FatalPoison {
                damage::damage(state, session, 1.0, Damage::Magic);
            }
        }
        Type::Wither => {
            damage::damage(state, session, 1.0, Damage::Magic);
        }
        Type::Regeneration => damage::heal(state, session, 1.0, RegainReason::Regen),
        Type::InstantHealth => damage::heal(
            state,
            session,
            instant_amount(effect, active.amplifier),
            RegainReason::Magic,
        ),
        Type::InstantDamage => {
            let amount = instant_amount(effect, active.amplifier);
            damage::damage(state, session, amount, Damage::Magic);
        }
        Type::Hunger => {
            let amount = 0.1 * f64::from(active.level());
            food::exhaust_as(state, session, amount, ExhaustCause::Potion);
        }
        Type::Saturation => {
            if let Some(player) = state.players.get_mut(&session) {
                let level = f64::from(active.level());
                food::add_food(&mut player.survival, level, 2.0 * level);
                player.data_dirty = true;
            }
        }
        _ => {}
    }
}

/// What an instant effect at full potency heals or hurts (PMMP `InstantHealthEffect`:
/// 4 << amplifier, `InstantDamageEffect`: 6 << amplifier), for players and entities, drunk or
/// splashed; 0 for any other effect.
pub(super) fn instant_amount(effect: Type, amplifier: u8) -> f64 {
    let base = match effect {
        Type::InstantHealth => 4.0,
        Type::InstantDamage => 6.0,
        _ => 0.0,
    };
    base * doubled(amplifier)
}

/// 2 to the power `n` (PMMP's `x << amplifier`, without the integer overflow).
fn doubled(n: u8) -> f64 {
    (0..n).fold(1.0, |v, _| v * 2.0)
}

/// The visible effects packed for the entity data (PMMP `Living::syncNetworkData`): up to 8
/// effects with particles, by ID, 7 bits each (6-bit ID, ambient bit).
pub(super) fn packed(s: &Survival) -> i64 {
    let mut ids = [(0_u8, false); effect::TYPE_COUNT];
    let mut n = 0;
    for (effect, active) in &s.effects {
        if let Some(slot) = ids.get_mut(n)
            && effect::shows_bubbles(&active.instance(*effect))
        {
            *slot = (effect.id(), active.ambient);
            n += 1;
        }
    }
    let ids = ids.get_mut(..n).unwrap_or_default();
    ids.sort_unstable_by_key(|&(id, _)| id);
    ids.iter().take(8).fold(0_i64, |data, &(id, ambient)| {
        (data << 7) | (i64::from(id & 0x3f) << 1) | i64::from(ambient)
    })
}

/// The effects a consumable gives, as active effects.
pub(super) fn of_instances(instances: &[effect::Instance]) -> impl Iterator<Item = (Type, Active)> {
    instances.iter().map(|i| (i.effect(), Active::of(*i)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_follow_pmmp() {
        assert_eq!(interval(Type::Poison, 0), 25);
        assert_eq!(interval(Type::Poison, 1), 12);
        assert_eq!(interval(Type::Poison, 5), 1);
        assert_eq!(interval(Type::Poison, 255), 1);
        assert_eq!(interval(Type::Wither, 0), 40);
        assert_eq!(interval(Type::Wither, 1), 20);
        assert_eq!(interval(Type::Regeneration, 0), 50);
        assert_eq!(interval(Type::Regeneration, 1), 25);
        assert_eq!(interval(Type::Regeneration, 2), 12);
        assert_eq!(interval(Type::Hunger, 3), 1);
        assert_eq!(interval(Type::Speed, 0), 0);
        assert_eq!(doubled(0), 1.0);
        assert_eq!(doubled(3), 8.0);
        assert!(doubled(255).is_finite());
    }

    #[test]
    fn weaker_or_shorter_effects_do_not_replace() {
        let old = Active::new(100, 1);
        let speed = Type::Speed;
        assert!(!can_replace(speed, old, Active::new(500, 0)));
        assert!(!can_replace(speed, old, Active::new(99, 1)));
        assert!(can_replace(speed, old, Active::new(100, 1)));
        assert!(can_replace(speed, old, Active::new(1, 2)));
    }

    #[test]
    fn an_instance_keeps_its_flags_through_an_active_effect() {
        let given = Type::Speed
            .instance()
            .with_duration_ticks(40)
            .with_amplifier(2)
            .with_visible(false)
            .with_ambient(true)
            .with_infinite(true);
        let active = Active::of(given);
        assert!(active.infinite);
        assert_eq!(active.instance(Type::Speed), given);
        assert!(!Active::of(given.with_infinite(false)).infinite);
    }

    #[test]
    fn visible_effects_pack_in_id_order() {
        let mut s = Survival::default();
        s.effects.insert(Type::Speed, Active::new(10, 0));
        s.effects.insert(Type::Poison, Active::new(10, 0));
        let mut hidden = Active::new(10, 0);
        hidden.visible = false;
        s.effects.insert(Type::Regeneration, hidden);
        let speed = i64::from(Type::Speed.id());
        let poison = i64::from(Type::Poison.id());
        let (a, b) = if speed < poison {
            (speed, poison)
        } else {
            (poison, speed)
        };
        assert_eq!(packed(&s), (a << 1) << 7 | b << 1);
    }
}
