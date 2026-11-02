//! Non-player characters without AI, and players' melee hits and interactions with entities.
//!
//! The NPCs are villagers that trade, humans and PMMP's zombie (all PMMP `Living`), and custom
//! entities ([`super::custom`]). The hits and interactions follow PMMP `Player::attackEntity` and
//! `Player::interactEntity`.
//!
//! NPCs that [`spawn_npc`] makes are invulnerable, except zombies: lobby and shop NPCs are the
//! use, and a trader killed by a stray hit would take its offers with it. An NPC whose
//! `Invulnerable` flag is off follows PMMP `Living`: a 10-tick cooldown in which only stronger
//! hits count the difference, knockback and the hurt animation on a cold hit, fall damage,
//! suffocation, burning and the blocks that hurt a living thing inside them (the players' rule,
//! `Harm`), and at 0 health the death animation and removal 25 ticks later; its drops (a
//! zombie's; other NPCs drop nothing) come at its first tick after the death. NPCs have no
//! lasting effects (no AI they could change), so a wither rose does nothing to them; instant
//! health and damage apply. Drowning is not counted (no air supply).
//!
//! Melee hits on items, orbs, projectiles and clouds do nothing (PMMP kicks the player for items
//! and arrows; a hit packet is not worth a kick). A hit on an NPC, a painting or an end crystal
//! counts what the survival rules count for a hit on a player: the attacker's Strength and
//! Weakness, Sharpness and a critical hit (a painting breaks, an end crystal explodes; the hit
//! animations, knockback and Fire Aspect are for NPCs, PMMP `Living`).
//!
//! [`spawn_npc`]: super::spawn_npc

use std::sync::Arc;

use super::kind::{Character, Form, Kind, MAX_NAME_TAG_BYTES};
use super::{
    Blow, Cause, Hooks, Hurt, Knock, Npc, SAFE_FALL, SUFFOCATION_DAMAGE, State, combust, create,
    hurt, is_inside_solid, player_actor, push_entity, send_to, with,
};
use crate::config::GameMode;
use crate::entity::{Animation, Id};
use crate::event::{CombustSource, DamageSource};
use crate::item::Stack;
use crate::math::Vec3;
use crate::nbt::Compound;
use crate::protocol::Uuid;
use crate::save::WorldIndex;
use crate::tick::{Phase, SessionId, survival};
use crate::world::Sound;

/// Spawns `npc` at `pos`, or `None` at the entity limit, in an unloaded chunk or for a long name.
/// A name is long past [`MAX_NAME_TAG_BYTES`]. The NPC is invulnerable (see the module
/// documentation), except a zombie.
pub(super) fn spawn(state: &mut State, world: WorldIndex, pos: Vec3, npc: Npc) -> Option<Id> {
    let (form, name) = match npc {
        Npc::Villager { profession, offers } => {
            let villager = Form::Villager {
                profession,
                offers,
                baby: false,
            };
            (villager, String::new())
        }
        Npc::Zombie => (Form::Zombie { baby: false }, String::new()),
        Npc::Human { name, skin } => {
            if name.len() > MAX_NAME_TAG_BYTES {
                return None;
            }
            let form = Form::Human {
                uuid: Uuid::from_bytes([0; 16]),
                skin: skin.map(Arc::new),
            };
            (form, name)
        }
    };
    let kind = Kind::Npc(Character {
        // PMMP's zombie is a mob, not a lobby figure: it takes damage.
        invulnerable: !matches!(form, Form::Zombie { .. }),
        form,
        attack_time: 0,
        last_damage: 0.0,
        dead_ticks: None,
        data: Compound::new(),
        held: Stack::EMPTY,
    });
    let id = create(state, world, pos, kind, |_| (Vec3::ZERO, 0.0, 0.0))?;
    with(state, id, |e| e.name_tag = name);
    Some(id)
}

/// Interacts as `session` with the entity `target` (PMMP `Player::interactEntity`).
/// A villager NPC within reach opens trading with `open` (`trade::open`), and a live custom
/// entity calls its behaviour. False if nothing happened.
pub(super) fn interact_with(
    state: &mut State,
    session: SessionId,
    target: Id,
    hooks: Hooks,
    open: fn(&mut State, SessionId, Id) -> bool,
) -> bool {
    if reaches(state, session, target, hooks).is_none() {
        return false;
    }
    if state
        .rules
        .entities
        .get(target)
        .is_some_and(|e| e.kind.custom().is_some())
    {
        return super::custom::interact(state, session, target);
    }
    let villager = state.rules.entities.get(target).is_some_and(|e| {
        matches!(
            &e.kind,
            Kind::Npc(Character {
                form: Form::Villager { .. },
                dead_ticks: None,
                ..
            })
        )
    });
    villager && open(state, session, target)
}

/// Hits the entity `target` in melee as `session` (PMMP `Player::attackEntity`).
/// An NPC within reach takes the survival rules' melee damage (attack points, Strength,
/// Weakness, Sharpness, a critical hit), with knockback from the player, Knockback and Fire
/// Aspect; the weapon wears and the player gets hungrier. The viewers see the arm swing and hear
/// the hit (or a hit that did nothing: out of reach, within the NPC's cooldown, invulnerable),
/// and see a critical or enchanted hit on the NPC. A target in another world than the player's
/// is not hit, swung at or heard.
pub(super) fn attack_with(state: &mut State, session: SessionId, target: Id, hooks: Hooks) {
    let Some(npc) = state.rules.entities.get(target).filter(|e| {
        let object = matches!(e.kind, Kind::Painting(_) | Kind::EndCrystal(_)) && e.is_alive();
        object || e.kind.is_living()
    }) else {
        return;
    };
    if state
        .players
        .get(&session)
        .is_none_or(|p| p.world != npc.world)
    {
        return;
    }
    let living = npc.kind.is_living();
    let middle = npc.body.position + Vec3::new(0.0, npc.size().1 / 2.0, 0.0);
    let npc_runtime_id = npc.runtime_id;
    let Some(melee) = survival::melee(state, session) else {
        return;
    };
    let held = state
        .players
        .get(&session)
        .map(|p| p.inventory.held().clone())
        .unwrap_or_default();
    let reached = reaches(state, session, target, hooks);
    let result = match reached {
        Some((eye, at)) => {
            let feet = survival::feet_at(eye);
            let knock = Knock::new(at.x - feet.x, at.z - feet.z);
            let by = DamageSource::Entity(player_actor(session));
            hurt(
                state,
                target,
                Blow::of(Cause::Attack, melee.damage).by(by),
                Some(knock),
            )
        }
        None => Hurt::Ignored,
    };
    survival::swing(state, session);
    let landed = result != Hurt::Ignored;
    let sound = if landed {
        Sound::EntityAttack
    } else {
        Sound::EntityAttackNoDamage
    };
    survival::hit_sound(state, session, middle, sound);
    let Some((eye, at)) = reached.filter(|_| landed) else {
        return;
    };
    let (melee_critical, melee_magic) = if living {
        (melee.critical, melee.magic)
    } else {
        (false, false)
    };
    let State { rules, players, .. } = &mut *state;
    if let Some(npc) = rules.entities.get(target) {
        for (on, animation) in [
            (melee_critical, Animation::CriticalHit { particles: 0 }),
            (melee_magic, Animation::MagicHit { particles: 0 }),
        ] {
            if on {
                send_to(players, &npc.viewers, &animation.packet(npc_runtime_id));
            }
        }
    }
    let feet = survival::feet_at(eye);
    let (dx, dz) = (at.x - feet.x, at.z - feet.z);
    survival::after_melee(state, session, &held, |state, knockback, fire| {
        if living {
            enchanted_hit(state, session, target, (knockback, fire), (dx, dz));
        }
    });
}

/// The eye position of the player `session` and the position of the entity `target`.
/// They are there if the player can act on the entity: spawned, alive, not a spectator, in the
/// same world and within reach of its eyes and look (PMMP `Player::canInteract`).
pub(super) fn reaches(
    state: &State,
    session: SessionId,
    target: Id,
    hooks: Hooks,
) -> Option<(Vec3, Vec3)> {
    let player = state.players.get(&session)?;
    let e = state.rules.entities.get(target).filter(|e| !e.flagged)?;
    if player.phase != Phase::Spawned
        || !(hooks.is_alive)(player)
        || survival::game_mode(state, player) == GameMode::Spectator
        || player.world != e.world
    {
        return None;
    }
    let (eye, at) = (player.position, e.body.position);
    let (yaw, pitch) = (hooks.look)(player);
    let dir = super::direction(yaw, pitch);
    survival::can_interact(eye, dir, at, survival::ENTITY_REACH).then_some((eye, at))
}

/// Applies the weapon's Knockback force and Fire Aspect seconds to a landed hit.
/// The hit landed along `(dx, dz)`.
fn enchanted_hit(
    state: &mut State,
    session: SessionId,
    target: Id,
    (knockback, fire): (Option<f64>, Option<u32>),
    (dx, dz): (f64, f64),
) {
    if let Some(force) = knockback {
        let knock = Knock {
            force,
            ..Knock::new(dx, dz)
        };
        let motion = state
            .rules
            .entities
            .get(target)
            .filter(|e| !e.flagged)
            .and_then(|e| knock.applied_to(e.body.motion));
        if let Some(m) = motion {
            push_entity(state, target, m);
        }
    }
    if let Some(seconds) = fire {
        let by = CombustSource::Entity(player_actor(session));
        combust(
            state,
            target,
            i16::try_from(seconds).unwrap_or(i16::MAX),
            by,
        );
    }
}

/// Ticks an NPC (PMMP `Living::entityBaseTick`).
/// It suffocates inside a solid block, then its hit cooldown counts down.
pub(super) fn tick(state: &mut State, id: Id) {
    let Some(e) = state.rules.entities.get(id) else {
        return;
    };
    let inside = state
        .worlds
        .get(e.world)
        .is_some_and(|w| is_inside_solid(w, e.eye_position(), e.bounding_box()));
    if inside {
        hurt(
            state,
            id,
            Blow::of(Cause::Suffocation, SUFFOCATION_DAMAGE),
            None,
        );
    }
    with(state, id, |e| {
        if let Kind::Npc(c) = &mut e.kind
            && c.attack_time > 0
        {
            c.attack_time -= 1;
        }
    });
}

/// Handles an NPC landing after a fall of `fall` blocks (PMMP `Living::onHitGround`).
/// It takes the fall damage beyond 3 blocks.
pub(super) fn landed(state: &mut State, id: Id, fall: f64) {
    let damage = (fall - SAFE_FALL).ceil();
    if damage > 0.0 {
        hurt(state, id, Blow::of(Cause::Fall, damage), None);
    }
}

/// Whether the NPC `id` is dying: true while its death shows.
#[cfg(test)]
pub(super) fn is_dying(state: &State, id: Id) -> bool {
    matches!(
        state.rules.entities.get(id).map(|e| &e.kind),
        Some(Kind::Npc(Character {
            dead_ticks: Some(t),
            ..
        })) if *t < Character::MAX_DEAD_TICKS
    )
}

#[cfg(test)]
mod tests;
