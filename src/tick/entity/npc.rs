//! Non-player characters: villager NPCs that trade, human NPCs and PMMP's zombie, all PMMP `Living`
//! without AI, and players' melee hits and interactions with entities (PMMP `Player::attackEntity`,
//! `Player::interactEntity`).
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

use super::super::{Phase, SessionId, survival, update};
use std::sync::Arc;

use super::kind::{Character, Form, Kind, MAX_NAME_TAG_BYTES};
use super::{
    Blow, Cause, EYE_HEIGHT, Hooks, Hurt, Knock, Npc, SAFE_FALL, SUFFOCATION_DAMAGE, State,
    combust, create, hurt, is_inside_solid, player_actor, push_entity, send_to, with,
};
use crate::config::GameMode;
use crate::entity::Id;
use crate::event::{CombustSource, DamageSource};
use crate::math::Vec3;
use crate::protocol::Uuid;
use crate::protocol::consts::AnimateAction;
use crate::protocol::packet::{Animate, Packet};
use crate::save::WorldIndex;
use crate::world::BlockPos;

/// Spawns `npc` at `pos`; `None` at the entity limit, in an unloaded chunk, or for a name longer
/// than [`MAX_NAME_TAG_BYTES`]. The NPC is invulnerable (see the module documentation), except a
/// zombie.
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
    });
    let id = create(state, world, pos, kind, |_| (Vec3::ZERO, 0.0, 0.0))?;
    with(state, id, |e| e.name_tag = name);
    Some(id)
}

/// The player `session` if it can act on the entity `target`: spawned, alive, not a spectator,
/// in the same world and within reach of its eyes and look (PMMP `Player::canInteract`). Gives
/// the player's eye position and the entity's position.
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

/// `session` interacts with the entity `target` (PMMP `Player::interactEntity`): a villager NPC
/// within reach opens trading with `open` (`trade::open`), a live custom entity calls its
/// behaviour. False if nothing happened.
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

/// `session` hits the entity `target` in melee (PMMP `Player::attackEntity`): an NPC within
/// reach takes the survival rules' melee damage (attack points, Strength, Weakness, Sharpness, a
/// critical hit), with knockback from the player, Knockback and Fire Aspect; the weapon wears and
/// the player gets hungrier. The viewers see the arm swing and hear the hit (or a hit that did
/// nothing: out of reach, within the NPC's cooldown, invulnerable), and see a critical or
/// enchanted hit on the NPC.
pub(super) fn attack_with(state: &mut State, session: SessionId, target: Id, hooks: Hooks) {
    let Some(npc) = state.rules.entities.get(target).filter(|e| {
        let object = matches!(e.kind, Kind::Painting(_) | Kind::EndCrystal(_)) && e.is_alive();
        object || e.kind.is_living()
    }) else {
        return;
    };
    let living = npc.kind.is_living();
    let middle = npc.body.position + Vec3::new(0.0, npc.kind.physics().height / 2.0, 0.0);
    let (npc_runtime_id, viewers) = (npc.runtime_id, npc.viewers.clone());
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
            let feet = eye - Vec3::new(0.0, EYE_HEIGHT, 0.0);
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
        crate::world::Sound::EntityAttack
    } else {
        crate::world::Sound::EntityAttackNoDamage
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
    for (on, action) in [
        (melee_critical, AnimateAction::CriticalHit),
        (melee_magic, AnimateAction::MagicCriticalHit),
    ] {
        if on {
            let packet = Packet::Animate(Animate {
                action_type: u8::from(action),
                entity_runtime_id: npc_runtime_id,
                data: 0.0,
                swing_source: 0,
            });
            send_to(&mut state.players, &viewers, &packet);
        }
    }
    let feet = eye - Vec3::new(0.0, EYE_HEIGHT, 0.0);
    let (dx, dz) = (at.x - feet.x, at.z - feet.z);
    survival::after_melee(state, session, &held, |state, knockback, fire| {
        if living {
            enchanted_hit(state, session, target, (knockback, fire), (dx, dz));
        }
    });
}

/// The weapon's Knockback force and Fire Aspect seconds on a hit that landed along `(dx, dz)`.
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

/// An NPC's own tick (PMMP `Living::entityBaseTick`): suffocation inside a solid block, then the
/// hit cooldown counts down.
pub(super) fn tick(state: &mut State, id: Id, _hooks: Hooks) {
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

/// An NPC landed after falling `fall` blocks (PMMP `Living::onHitGround`): it may trample the
/// farmland it lands on, and takes the fall damage beyond 3 blocks.
pub(super) fn landed(state: &mut State, id: Id, fall: f64) {
    let Some(e) = state.rules.entities.get(id) else {
        return;
    };
    let (world, p) = (e.world, e.body.position);
    let mut pos = BlockPos::containing(p);
    let empty = state
        .worlds
        .get(world)
        .and_then(|w| w.block(pos))
        .is_none_or(|s| s.collision_boxes().is_empty());
    if empty {
        pos = BlockPos::new(pos.x, pos.y.saturating_sub(1), pos.z);
    }
    update::trample(state, world, pos, fall, crate::event::Actor::Entity(id));
    let damage = (fall - SAFE_FALL).ceil();
    if damage > 0.0 {
        hurt(state, id, Blow::of(Cause::Fall, damage), None);
    }
}

/// Whether the NPC `id` has finished dying: true while its death shows.
#[cfg(test)]
pub(super) fn is_dying(state: &State, id: Id) -> bool {
    matches!(
        state.rules.entities.get(id).map(|e| &e.kind),
        Some(Kind::Npc(Character {
            dead_ticks: Some(t),
            ..
        })) if *t < super::kind::MAX_DEAD_TICKS
    )
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::super::tests::{Harness, TEST_HOOKS, adds, at, calls, standing};
    use super::*;
    use crate::item::Enchantment;
    use crate::nbt::{Compound, Value};
    use crate::protocol::MetadataValue::Int32;
    use crate::protocol::consts::EntityDataKey as K;
    use crate::protocol::consts::{ActorEvent, SoundEvent};
    use crate::protocol::packet::Packet;
    use crate::tick::entity::physics::tests::named;
    use crate::tick::entity::saved::tests::{bytes, stack};
    use crate::tick::trade::Offers;
    use crate::world::ChunkPos;

    thread_local! {
        /// Trade windows the stand-in opener opened.
        static OPENED: Cell<u32> = const { Cell::new(0) };
    }

    fn open(_: &mut State, _: SessionId, _: Id) -> bool {
        OPENED.with(|o| o.set(o.get() + 1));
        true
    }

    fn offers() -> Offers {
        let offer = |n: i32| {
            Compound::from([
                (
                    "buyA".to_owned(),
                    Value::Compound(stack("minecraft:emerald", 2).to_nbt()),
                ),
                (
                    "sell".to_owned(),
                    Value::Compound(stack("minecraft:apple", 4).to_nbt()),
                ),
                ("netId".to_owned(), Value::Int(n)),
                ("uses".to_owned(), Value::Int(0)),
            ])
        };
        Offers::from_nbt(
            &crate::nbt::Compound::from([(
                "Recipes".to_owned(),
                Value::List(crate::nbt::List::Compound(vec![
                    offer(1),
                    offer(2),
                    offer(3),
                ])),
            )]),
            &crate::custom::Registry::default(),
        )
    }

    fn villager(h: &mut Harness, pos: Vec3) -> Id {
        let npc = Npc::Villager {
            profession: 4,
            offers: offers(),
        };
        spawn(&mut h.state, 0, pos, npc).unwrap()
    }

    fn human(h: &mut Harness, pos: Vec3, name: &str) -> Id {
        let npc = Npc::Human {
            name: name.to_owned(),
            skin: None,
        };
        spawn(&mut h.state, 0, pos, npc).unwrap()
    }

    fn offers_of(h: &mut Harness, v: Id) -> &mut Offers {
        super::super::offers_mut(&mut h.state, v).unwrap()
    }

    fn vulnerable(h: &mut Harness, id: Id) {
        with(&mut h.state, id, |e| {
            if let Kind::Npc(c) = &mut e.kind {
                c.invulnerable = false;
            }
        });
    }

    fn health(h: &Harness, id: Id) -> f64 {
        h.state.rules.entities.get(id).unwrap().health
    }

    #[test]
    fn npcs_save_and_load_with_their_offers_byte_identically() {
        let mut h = Harness::new(1);
        let v = villager(&mut h, at(1.5, -60.0, 1.5));
        human(&mut h, at(3.5, -60.0, 1.5), "Guide");
        assert!(super::super::villager(&h.state, v).is_some());
        let key = (0, ChunkPos::new(0, 0));
        let ours = |list: &[Compound]| -> Vec<Compound> {
            list.iter()
                .filter(|c| c.contains_key("identifier"))
                .cloned()
                .collect()
        };
        let all = h.state.rules.entities.unload(key).expect("changed");
        let first = ours(&all);
        assert_eq!(first.len(), 2);
        h.state
            .rules
            .entities
            .load_list(key, &all, &crate::custom::Registry::default());
        h.state.rules.entities.changed.insert(key);
        let again = h.state.rules.entities.take_changed();
        let second = ours(&again[0].1);
        assert_eq!(second.len(), 2);
        for (a, b) in first.iter().zip(&second) {
            assert_eq!(bytes(a), bytes(b));
        }
        assert_eq!(
            first[0].get("identifier"),
            Some(&Value::String("minecraft:villager_v2".to_owned()))
        );
        assert_eq!(
            first[0].get("Offers"),
            Some(&Value::Compound(offers().to_nbt()))
        );
        assert_eq!(
            first[1].get("CustomName"),
            Some(&Value::String("Guide".to_owned()))
        );
    }

    // pmmp: Player::interactEntity
    #[test]
    fn interacting_with_a_villager_in_reach_opens_trading() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let near = villager(&mut h, at(2.5, -60.0, 0.5));
        let far = villager(&mut h, at(12.5, -60.0, 0.5));
        let guide = human(&mut h, at(0.5, -60.0, 2.5), "Guide");
        let s = SessionId::new(1);
        calls(|c| c.look = (270.0, 0.0));
        assert!(interact_with(&mut h.state, s, near, TEST_HOOKS, open));
        assert!(
            !interact_with(&mut h.state, s, far, TEST_HOOKS, open),
            "beyond 8 blocks"
        );
        assert!(
            !interact_with(&mut h.state, s, guide, TEST_HOOKS, open),
            "no trades"
        );
        calls(|c| c.look = (90.0, 0.0));
        assert!(
            !interact_with(&mut h.state, s, near, TEST_HOOKS, open),
            "behind the player"
        );
        calls(|c| c.dead.insert(1));
        calls(|c| c.look = (270.0, 0.0));
        assert!(
            !interact_with(&mut h.state, s, near, TEST_HOOKS, open),
            "dead"
        );
        assert_eq!(OPENED.with(Cell::get), 1);
    }

    // pmmp: Player::attackEntity
    #[test]
    fn hits_on_npcs_follow_pmmps_living_rules() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let s = SessionId::new(1);
        calls(|c| c.look = (270.0, 0.0));
        let guide = human(&mut h, at(2.5, -60.0, 0.5), "Guide");
        h.entities();
        attack_with(&mut h.state, s, guide, TEST_HOOKS);
        assert_eq!(health(&h, guide), 20.0, "spawned NPCs are invulnerable");
        vulnerable(&mut h, guide);
        let sword = stack("minecraft:diamond_sword", 1);
        let points = f64::from(sword.item().unwrap().attack_points());
        crate::tick::inventory::set_held(h.state.players.get_mut(&s).unwrap(), sword);
        h.take(1);
        attack_with(&mut h.state, s, guide, TEST_HOOKS);
        assert_eq!(health(&h, guide), 20.0 - points);
        let e = h.state.rules.entities.get(guide).unwrap();
        assert!(
            e.body.motion.x > 0.0 && e.body.motion.y > 0.0,
            "knocked back"
        );
        let held = h.state.players[&s].inventory.held().clone();
        assert_eq!(held.damage(), 1, "the sword wears");
        attack_with(&mut h.state, s, guide, TEST_HOOKS);
        assert_eq!(
            health(&h, guide),
            20.0 - points,
            "within the cooldown: ignored"
        );
        h.entities();
        let runtime_id = e_runtime(&h, guide);
        // The hurt event carries BDS's cause number, 2 for an attack (P6).
        let hurt = h.take(1).iter().any(|p| {
            matches!(p,
            Packet::ActorEvent(a) if a.entity_runtime_id == runtime_id
                && a.event_type == ActorEvent::Hurt as u8 && a.event_data == 2)
        });
        assert!(hurt, "the hit shows");
        // Hits after the cooldown until it dies; it shows its death and goes 25 ticks later.
        for _ in 0..10 {
            for _ in 0..10 {
                h.entities();
            }
            attack_with(&mut h.state, s, guide, TEST_HOOKS);
            if is_dying(&h.state, guide) {
                break;
            }
        }
        assert!(is_dying(&h.state, guide));
        assert_eq!(health(&h, guide), 0.0);
        let died = h.state.players[&s].outbox.iter().any(|p| matches!(p,
            Packet::ActorEvent(a) if a.entity_runtime_id == runtime_id && a.event_type == ActorEvent::Death as u8));
        assert!(died);
        for _ in 0..24 {
            h.entities();
        }
        assert!(h.state.rules.entities.get(guide).is_some());
        h.entities();
        assert!(
            h.state.rules.entities.get(guide).is_none(),
            "gone after 25 ticks"
        );
        // Items take no melee hits.
        assert!(super::super::drop_item(
            &mut h.state,
            0,
            at(2.5, -60.0, 0.5),
            stack("minecraft:stick", 1)
        ));
        let item = h.live()[0].runtime_id;
        let id = super::super::find(&h.state, item).unwrap();
        attack_with(&mut h.state, s, id, TEST_HOOKS);
        assert_eq!(health(&h, id), 5.0);
    }

    /// A hit on an NPC counts the attacker's Strength and the weapon's Sharpness as a hit on a
    /// player does; the viewers see the swing, hear the hit and see the enchanted hit on the NPC;
    /// a hit out of reach swings and sounds empty.
    #[test]
    fn npc_hits_count_the_attackers_effects_and_show_the_swing() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.spawn(2, standing(0.0, 3.0));
        let s = SessionId::new(1);
        calls(|c| c.look = (270.0, 0.0));
        let guide = human(&mut h, at(2.5, -60.0, 0.5), "Guide");
        let far = human(&mut h, at(12.5, -60.0, 0.5), "Far");
        vulnerable(&mut h, guide);
        h.entities();
        let strength =
            crate::effect::Instance::new(crate::effect::Type::Strength, 600, 0, true, false);
        assert!(survival::add_effect(&mut h.state, s, strength));
        let extra = crate::item::Extra {
            enchantments: vec![(Enchantment::Sharpness, 1)],
            ..crate::item::Extra::default()
        };
        let sword = stack("minecraft:diamond_sword", 1)
            .with_extra(Some(extra))
            .unwrap();
        let points = f64::from(sword.item().unwrap().attack_points());
        crate::tick::inventory::set_held(h.state.players.get_mut(&s).unwrap(), sword);
        h.state
            .players
            .get_mut(&SessionId::new(2))
            .unwrap()
            .outbox
            .clear();
        attack_with(&mut h.state, s, guide, TEST_HOOKS);
        let want = 20.0 - (points + points * 0.3 + 1.0);
        assert!(
            (health(&h, guide) - want).abs() < 1e-9,
            "{}",
            health(&h, guide)
        );
        let (attacker, npc) = (h.runtime_id(1), e_runtime(&h, guide));
        let seen = h.state.players[&SessionId::new(2)].outbox.clone();
        let swing = seen.iter().any(|p| {
            matches!(p,
            Packet::ActorEvent(a) if a.entity_runtime_id == attacker
                && a.event_type == u8::from(ActorEvent::StartAttacking))
        });
        assert!(swing, "the swing shows");
        let sound = |seen: &[Packet], sound: SoundEvent| {
            seen.iter().any(|p| {
                matches!(p,
                Packet::LevelSoundEvent(e) if e.sound_type == sound.as_str())
            })
        };
        assert!(sound(&seen, SoundEvent::AttackStrong));
        let magic = seen.iter().any(|p| {
            matches!(p,
            Packet::Animate(a) if a.entity_runtime_id == npc
                && a.action_type == u8::from(AnimateAction::MagicCriticalHit))
        });
        assert!(magic, "the enchanted hit shows on the NPC");
        h.state
            .players
            .get_mut(&SessionId::new(2))
            .unwrap()
            .outbox
            .clear();
        attack_with(&mut h.state, s, far, TEST_HOOKS);
        assert_eq!(health(&h, far), 20.0);
        let seen = h.state.players[&SessionId::new(2)].outbox.clone();
        assert!(sound(&seen, SoundEvent::AttackNoDamage), "out of reach");
    }

    fn zombie(h: &mut Harness, pos: Vec3) -> Id {
        crate::tick::Context {
            state: &mut h.state,
        }
        .spawn_zombie(crate::world::Id::DEFAULT, pos)
        .unwrap()
    }

    // pmmp: Zombie, Zombie::getName, Zombie::getNetworkTypeId
    #[test]
    fn a_zombie_is_an_ai_less_living_mob_that_takes_damage() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.take(1);
        let z = zombie(&mut h, at(2.5, -57.0, 0.5));
        let e = h.state.rules.entities.get(z).unwrap();
        assert_eq!(e.entity_type(), crate::entity::Type::Zombie);
        assert_eq!(e.name(), Some("Zombie"));
        assert_eq!(e.network_type(), "minecraft:zombie");
        assert_eq!(e.size(), (0.6, 1.9));
        assert!(!e.is_invulnerable(), "PMMP's zombie is a mob");
        assert_eq!(e.max_health(), 20.0);
        for _ in 0..40 {
            h.entities();
        }
        let e = h.state.rules.entities.get(z).unwrap();
        assert_eq!(
            e.body.position,
            at(2.5, -60.0, 0.5),
            "it falls 3 blocks unhurt and stands: no AI"
        );
        let packets = h.take(1);
        let added = packets
            .iter()
            .any(|p| matches!(p, Packet::AddActor(a) if a.entity_type == "minecraft:zombie"));
        assert!(added);
        crate::tick::Context {
            state: &mut h.state,
        }
        .entity_mut(z)
        .unwrap()
        .attack(5.0);
        assert_eq!(health(&h, z), 15.0);
    }

    /// The rotten flesh, rare items and experience on the ground.
    fn loot(h: &Harness) -> (u32, u32, u32) {
        let (mut flesh, mut rare, mut xp) = (0, 0, 0);
        for e in h.live() {
            match &e.kind {
                Kind::Item(i) => {
                    let count = u32::from(i.stack.count());
                    match i.stack.item().and_then(crate::item::Type::kind) {
                        Some(crate::item::Kind::RottenFlesh) => flesh += count,
                        Some(
                            crate::item::Kind::IronIngot
                            | crate::item::Kind::Carrot
                            | crate::item::Kind::Potato,
                        ) => rare += count,
                        other => panic!("{other:?}"),
                    }
                }
                Kind::ExperienceOrb(o) => xp += u32::from(o.value),
                _ => {}
            }
        }
        (flesh, rare, xp)
    }

    #[test]
    fn dead_zombies_drop_rotten_flesh_sometimes_a_rare_item_and_five_experience() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let count = 400;
        let mut zombies = Vec::new();
        for i in 0..count {
            let (x, z) = (f64::from(i % 20) - 10.0, f64::from(i / 20) - 10.0);
            zombies.push(zombie(&mut h, at(x + 0.5, -60.0, z + 0.5)));
        }
        let mut cx = crate::tick::Context {
            state: &mut h.state,
        };
        for &z in &zombies {
            cx.entity_mut(z).unwrap().kill();
        }
        assert_eq!(
            loot(&h),
            (0, 0, 0),
            "the drops come at the zombie's next tick"
        );
        h.entities();
        let (flesh, rare, xp) = loot(&h);
        // PMMP: 0 to 2 flesh each (1 on average), a rare item with a 5 in 200 chance.
        assert!((300..=500).contains(&flesh), "{flesh}");
        assert!((2..=25).contains(&rare), "{rare}");
        assert_eq!(xp, 5 * count);
        h.entities();
        assert_eq!(loot(&h).2, 5 * count, "once");
    }

    // pmmp: EntityDeathEvent::getDrops, EntityDeathEvent::setDrops, EntityDeathEvent::getXpDropAmount
    #[test]
    fn a_death_listener_sees_and_changes_a_zombies_drops() {
        let mut l = crate::event::Listeners::default();
        l.push(
            crate::event::Priority::Normal,
            |e: &mut crate::event::EntityDeath, _: &mut crate::tick::Context<'_>| {
                assert_eq!(e.xp_drop_amount(), 5);
                assert!(e.drops().iter().all(|s| s.count() <= 2));
                e.set_drops(Vec::new());
                e.set_xp_drop_amount(0);
            },
        );
        let mut h = Harness::with_listeners(1, l);
        h.spawn(1, standing(0.0, 0.0));
        let z = zombie(&mut h, at(2.5, -60.0, 0.5));
        crate::tick::Context {
            state: &mut h.state,
        }
        .entity_mut(z)
        .unwrap()
        .kill();
        h.entities();
        assert_eq!(loot(&h), (0, 0, 0));
    }

    /// `experience = false`: a zombie's death names no experience and drops none.
    #[test]
    fn without_experience_a_zombie_death_names_and_drops_no_experience() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut l = crate::event::Listeners::default();
        l.push(
            crate::event::Priority::Normal,
            move |e: &mut crate::event::EntityDeath, _: &mut crate::tick::Context<'_>| {
                tx.try_send(e.xp_drop_amount()).unwrap();
            },
        );
        let mut h = Harness::with_listeners(1, l);
        h.state.settings.experience = false;
        h.spawn(1, standing(0.0, 0.0));
        let z = zombie(&mut h, at(2.5, -60.0, 0.5));
        crate::tick::Context {
            state: &mut h.state,
        }
        .entity_mut(z)
        .unwrap()
        .kill();
        h.entities();
        assert_eq!(rx.try_recv(), Ok(0));
        assert_eq!(loot(&h).2, 0);
    }

    /// The `Baby` flag in the flags the viewers of `id` last got.
    fn shown_baby(h: &Harness, id: Id) -> bool {
        let flags = h.state.rules.entities.get(id).unwrap().sent.flags;
        flags & (1 << crate::protocol::consts::EntityDataFlag::Baby as u32) != 0
    }

    // pmmp: entity\Ageable, entity\Ageable::isBaby, Villager::isBaby
    #[test]
    fn villagers_and_zombies_can_be_babies_and_viewers_see_it() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let trader = villager(&mut h, at(2.5, -60.0, 0.5));
        let walker = zombie(&mut h, at(3.5, -60.0, 0.5));
        let guide = human(&mut h, at(4.5, -60.0, 0.5), "Guide");
        h.entities();
        let mut cx = crate::tick::Context {
            state: &mut h.state,
        };
        for id in [trader, walker] {
            assert!(!cx.entity(id).unwrap().is_baby());
            assert!(cx.entity_mut(id).unwrap().set_baby(true));
            assert!(cx.entity(id).unwrap().is_baby());
        }
        assert!(
            !cx.entity_mut(guide).unwrap().set_baby(true),
            "humans grow up"
        );
        assert!(!cx.entity(guide).unwrap().is_baby());
        h.take(1);
        h.entities();
        assert!(shown_baby(&h, trader) && shown_baby(&h, walker) && !shown_baby(&h, guide));
        let baby = 1i64 << crate::protocol::consts::EntityDataFlag::Baby as u32;
        let sent = h
            .take(1)
            .iter()
            .filter(|p| {
                matches!(p, Packet::SetActorData(d)
                if matches!(d.entity_metadata.get(&(K::Flags as u32)),
                    Some(crate::protocol::MetadataValue::Int64(f)) if f & baby != 0))
            })
            .count();
        assert_eq!(sent, 2);
        let mut cx = crate::tick::Context {
            state: &mut h.state,
        };
        assert!(cx.entity_mut(walker).unwrap().set_baby(false));
        assert!(!cx.entity(walker).unwrap().is_baby());
    }

    #[test]
    fn baby_zombies_and_villagers_survive_a_checkpoint() {
        let mut h = Harness::new(1);
        let v = villager(&mut h, at(2.5, -60.0, 0.5));
        let z = zombie(&mut h, at(3.5, -60.0, 0.5));
        let mut cx = crate::tick::Context {
            state: &mut h.state,
        };
        for id in [v, z] {
            assert!(cx.entity_mut(id).unwrap().set_baby(true));
        }
        h.entities();
        let mut fresh = crate::tick::tests::new_state(
            crate::tick::tests::settings(|_| {}),
            crate::event::Listeners::default(),
        );
        let loaded: Vec<ChunkPos> = fresh.worlds[0].loaded().collect();
        for pos in loaded {
            if let Some(extras) = fresh.chunks.extras.get((0, pos)).cloned() {
                let world = &fresh.worlds[0];
                let read = fresh
                    .rules
                    .read((0, pos), &extras, world.chunk(pos).unwrap(), world);
                fresh.rules.load((0, pos), read, &extras);
            }
        }
        assert!(crate::tick::checkpoint::tests::assert_round_trip(
            &h.state, fresh, &h.pool
        ));
    }
    fn e_runtime(h: &Harness, id: Id) -> u64 {
        h.state.rules.entities.get(id).unwrap().runtime_id
    }

    #[test]
    fn npcs_trample_farmland_and_take_fall_damage_when_they_land() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let soil = BlockPos::new(5, -61, 5);
        h.state.worlds[0]
            .set_block(soil, named("minecraft:farmland"))
            .unwrap();
        let v = villager(&mut h, at(5.5, -50.0, 5.5));
        vulnerable(&mut h, v);
        for _ in 0..40 {
            h.entities();
        }
        let y = h.state.rules.entities.get(v).unwrap().body.position.y;
        assert_eq!(y, -60.0625, "on the farmland (15/16 high)");
        assert_eq!(
            h.state.worlds[0].block(soil),
            Some(crate::block::DIRT),
            "trampled"
        );
        let lost = 20.0 - health(&h, v);
        assert!(
            (6.0..=8.0).contains(&lost),
            "about 10 blocks fallen: {lost}"
        );
    }

    #[test]
    fn a_trade_sends_the_villagers_viewers_the_keys_it_changed() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let v = villager(&mut h, at(2.5, -60.0, 0.5));
        h.entities();
        h.take(1);
        let data = |h: &mut Harness| -> Vec<Vec<(u32, crate::protocol::MetadataValue)>> {
            h.entities();
            h.take(1)
                .into_iter()
                .filter_map(|p| match p {
                    Packet::SetActorData(d) => Some(d.entity_metadata.into_iter().collect()),
                    _ => None,
                })
                .collect()
        };
        let mut recipe = crate::tick::trade::Recipe::new(
            stack("minecraft:emerald", 2),
            crate::item::Stack::EMPTY,
            stack("minecraft:apple", 4),
            16,
        );
        recipe.trader_exp = 5;
        offers_of(&mut h, v).set(vec![recipe]).unwrap();
        super::super::villager_traded(&mut h.state, v, (0, 0));
        assert!(data(&mut h).is_empty(), "nothing changed, nothing sent");
        offers_of(&mut h, v).apply(0, 1).unwrap();
        super::super::villager_traded(&mut h.state, v, (0, 0));
        assert_eq!(
            data(&mut h),
            vec![vec![(K::TradeExperience as u32, Int32(5))]]
        );
        offers_of(&mut h, v).apply(0, 1).unwrap();
        super::super::villager_traded(&mut h.state, v, (0, 5));
        assert_eq!(
            data(&mut h),
            vec![vec![
                (K::TradeTier as u32, Int32(1)),
                (K::TradeExperience as u32, Int32(10)),
            ]]
        );
    }

    #[test]
    fn npcs_are_shown_as_their_actor_and_humans_through_the_player_list() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.take(1);
        let v = villager(&mut h, at(2.5, -60.0, 0.5));
        let g = human(&mut h, at(4.5, -60.0, 0.5), "Guide");
        h.entities();
        let packets = h.take(1);
        let (vr, gr) = (e_runtime(&h, v), e_runtime(&h, g));
        assert_eq!(adds(&packets), vec![vr, gr]);
        let actor = packets.iter().find_map(|p| match p {
            Packet::AddActor(a) if a.entity_runtime_id == vr => Some(a),
            _ => None,
        });
        let actor = actor.unwrap();
        assert_eq!(actor.entity_type, "minecraft:villager_v2");
        // BDS `villager_v2` (P6 `trade`): the variant, tier 0 of 4, no experience, skin 0.
        for (key, value) in [
            (K::Variant, 4),
            (K::TradeTier, 0),
            (K::MaxTradeTier, 4),
            (K::TradeExperience, 0),
            (K::SkinId, 0),
        ] {
            assert_eq!(
                actor.entity_metadata.get(&(key as u32)),
                Some(&Int32(value)),
                "{key:?}"
            );
        }
        let order: Vec<&str> = packets
            .iter()
            .filter_map(|p| match p {
                Packet::PlayerList(l) if l.entries[0].action_type == 0 => Some("add"),
                Packet::PlayerList(_) => Some("remove"),
                Packet::AddPlayer(a) if a.entity_runtime_id == gr => Some("player"),
                _ => None,
            })
            .collect();
        assert_eq!(order, ["add", "player", "remove"]);
    }

    /// The hurt event of an NPC carries the official cause number of an explosion (Mojang
    /// `ActorDamageCause`): 10 for a block's or nothing's, 11 for an entity's.
    #[test]
    fn an_npc_hurt_by_an_explosion_shows_whose_explosion_it_was() {
        use super::super::Raise;
        use super::super::explosion::{Source, explode_with};
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let target = human(&mut h, at(3.5, -60.0, 0.5), "Target");
        let source = human(&mut h, at(-3.5, -60.0, 0.5), "Source");
        vulnerable(&mut h, target);
        h.entities();
        let runtime_id = e_runtime(&h, target);
        let mut causes = Vec::new();
        for by in [Source::None, Source::Entity(source)] {
            with(&mut h.state, target, |e| e.health = 20.0);
            h.take(1);
            let blast = super::super::Blast::new(2.0, false);
            let center = Vec3::new(0.5, -59.0, 0.5);
            explode_with(
                &mut h.state,
                0,
                center,
                blast,
                by,
                TEST_HOOKS,
                Raise::Events,
            );
            assert!(health(&h, target) > 0.0, "hurt, alive");
            h.entities();
            causes.extend(h.take(1).iter().filter_map(|p| match p {
                Packet::ActorEvent(a)
                    if a.entity_runtime_id == runtime_id
                        && a.event_type == ActorEvent::Hurt as u8 =>
                {
                    Some(a.event_data)
                }
                _ => None,
            }));
            for _ in 0..10 {
                h.entities();
            }
        }
        assert_eq!(causes, [10, 11]);
    }

    /// PMMP `SweetBerryBush::onEntityInside` and `Magma::onEntityInside` hurt every living thing:
    /// a vulnerable NPC in a bush with berries and on magma takes their damage, as a player does.
    #[test]
    fn a_berry_bush_and_magma_hurt_a_vulnerable_npc_inside_them() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let bush = crate::block::Kind::SweetBerryBush
            .default_state()
            .with("growth", crate::block::PropertyValue::Int(1))
            .unwrap();
        h.state.worlds[0]
            .set_block(BlockPos::new(3, -60, 0), bush)
            .unwrap();
        let pricked = human(&mut h, at(3.5, -60.0, 0.5), "Pricked");
        vulnerable(&mut h, pricked);
        h.entities();
        assert_eq!(health(&h, pricked), 19.0, "1 contact damage");
        // A magma block the NPC stands in: its box reaches into the block.
        h.state.worlds[0]
            .set_block(BlockPos::new(-3, -60, 0), named("minecraft:magma"))
            .unwrap();
        let burnt = human(&mut h, at(-2.5, -59.5, 0.5), "Burnt");
        vulnerable(&mut h, burnt);
        h.entities();
        assert!(health(&h, burnt) < 20.0, "fire damage");
    }

    // pmmp: Entity::isInsideOfSolid
    /// A zombie twice its size suffocates in a block at its scaled eye height.
    #[test]
    fn a_scaled_zombie_suffocates_at_its_scaled_eyes() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        h.state.worlds[0]
            .set_block(BlockPos::new(3, -58, 0), named("minecraft:stone"))
            .unwrap();
        let z = zombie(&mut h, at(3.5, -60.0, 0.5));
        let mut cx = crate::tick::Context {
            state: &mut h.state,
        };
        assert!(cx.entity_mut(z).unwrap().set_scale(2.0));
        h.entities();
        assert_eq!(health(&h, z), 19.0);
    }

    // pmmp: EntitySizeInfo::scale
    /// The scale multiplies the whole eye height, the 0.1 above half the height included.
    #[test]
    fn a_scaled_zombie_has_its_whole_eye_height_scaled() {
        let mut h = Harness::new(1);
        h.spawn(1, standing(0.0, 0.0));
        let z = zombie(&mut h, at(3.5, -60.0, 0.5));
        let mut cx = crate::tick::Context {
            state: &mut h.state,
        };
        for (scale, eye) in [(2.0, 2.1), (0.5, 0.525)] {
            assert!(cx.entity_mut(z).unwrap().set_scale(scale));
            let got = cx.entity(z).unwrap().eye_height();
            assert!((got - eye).abs() < 1e-9, "scale {scale}: {got}");
        }
    }
}
