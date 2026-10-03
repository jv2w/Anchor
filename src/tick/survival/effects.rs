//! Status effects of players (PMMP `EffectManager`, `EffectCollection`, `entity/effect/*`):
//! adding with PMMP's replacement rule, what each effect does when added, every few ticks and
//! when removed, and the `MobEffect` packets that tell the player.

use super::attributes::{ABSORPTION, HEALTH, MOVEMENT};
use super::damage::{self, Damage};
use super::{Survival, food};
use crate::effect::{self, Instance, Type};
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

/// The longest finite duration (PMMP `Limits::INT32_MAX`).
pub(super) const MAX_DURATION: u32 = i32::MAX.cast_unsigned();

impl Active {
    /// The effect a potion or food gives.
    pub(super) fn of(instance: Instance) -> Self {
        Self {
            duration: instance.duration().min(MAX_DURATION),
            amplifier: instance.amplifier(),
            visible: instance.is_visible(),
            ambient: instance.is_ambient(),
            infinite: false,
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

/// Whether `new` may replace `old` (PMMP `EffectCollection::canAdd`): not a weaker one, nor an
/// equal one that ends sooner.
fn can_replace(old: Active, new: Active) -> bool {
    !(new.amplifier < old.amplifier
        || (new.amplifier == old.amplifier && new.duration < old.duration))
}

/// Adds an effect to the player (PMMP `EffectManager::add`): false if a stronger or longer one is
/// active. A replaced effect is removed first, so its changes (health boost, speed) undo.
pub(super) fn add(state: &mut State, session: SessionId, effect: Type, new: Active) -> bool {
    let Some(player) = state.players.get_mut(&session) else {
        return false;
    };
    let s = &mut player.survival;
    let old = s.effects.get(&effect).copied();
    if old.is_some_and(|old| !can_replace(old, new)) {
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
        player.send(packet);
    }
    true
}

/// Removes an effect, undoing what it changed.
pub(super) fn remove(state: &mut State, session: SessionId, effect: Type) {
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    let Some(old) = player.survival.effects.remove(&effect) else {
        return;
    };
    on_remove(&mut player.survival, effect, old);
    player.data_dirty = true;
    if player.phase == Phase::Spawned {
        let packet = packet(player, effect, None, MobEffectOperation::Remove);
        player.send(packet);
    }
}

/// Removes every effect (milk, respawn, a totem).
pub(super) fn clear(state: &mut State, session: SessionId) {
    for effect in active_types(state, session).into_iter().flatten() {
        remove(state, session, effect);
    }
}

/// The active effect types in order, without allocating (at most one per type).
fn active_types(state: &State, session: SessionId) -> [Option<Type>; EFFECT_TYPES] {
    let mut out = [None; EFFECT_TYPES];
    if let Some(player) = state.players.get(&session) {
        for (slot, &effect) in out.iter_mut().zip(player.survival.effects.keys()) {
            *slot = Some(effect);
        }
    }
    out
}

/// The number of effect types.
const EFFECT_TYPES: usize = 27;

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

/// One tick of the player's effects (PMMP `EffectManager::tick`): each effect acts if its
/// interval divides its remaining duration, then counts down and ends at 0.
pub(super) fn tick(state: &mut State, session: SessionId) {
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
        Type::Regeneration => damage::heal(state, session, 1.0),
        Type::InstantHealth => damage::heal(state, session, 4.0 * doubled(active.amplifier)),
        Type::InstantDamage => {
            damage::damage(
                state,
                session,
                6.0 * doubled(active.amplifier),
                Damage::Magic,
            );
        }
        Type::Hunger => {
            if let Some(player) = state.players.get_mut(&session) {
                food::exhaust(&mut player.survival, 0.1 * f64::from(active.level()));
            }
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

/// 2 to the power `n` (PMMP's `x << amplifier`, without the integer overflow).
fn doubled(n: u8) -> f64 {
    (0..n).fold(1.0, |v, _| v * 2.0)
}

/// The visible effects packed for the entity data (PMMP `Living::syncNetworkData`): up to 8
/// effects with particles, by ID, 7 bits each (6-bit ID, ambient bit).
pub(super) fn packed(s: &Survival) -> i64 {
    let mut ids = [(0_u8, false); EFFECT_TYPES];
    let mut n = 0;
    for (effect, active) in &s.effects {
        if let Some(slot) = ids.get_mut(n)
            && active.visible
            && effect.has_bubbles()
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
        assert!(!can_replace(old, Active::new(500, 0)));
        assert!(!can_replace(old, Active::new(99, 1)));
        assert!(can_replace(old, Active::new(100, 1)));
        assert!(can_replace(old, Active::new(1, 2)));
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
