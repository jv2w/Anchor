//! Custom entity types on the tick.
//!
//! This is the registry the builder fills, the entries the engine adds to
//! `AvailableActorIdentifiers`, the calls of the types' behaviours, and the API that spawns custom
//! entities and reads and changes the data and held item of NPCs.
//!
//! A custom entity is an NPC of [`Form::Custom`]: it shares the living rules of NPCs (hits,
//! cooldown, knockback, death, suffocation, fall damage) and is not invulnerable unless the API
//! makes it so. Its type is an [`Arc`] of the registered [`Spec`], so the store, the physics
//! (which reads its constants on the CPU pool), tracking and saving need no lookup. The
//! behaviours stay in the registry, which lends one out while it runs: a behaviour cannot run
//! inside another (callbacks raise no events and call no behaviour).
//!
//! Saved custom entities keep their identifier, health, invulnerability, data and held item
//! ([`super::saved`]); one whose type is not registered at load stays as saved, like an unknown
//! block, and is written back unchanged. A type keeps its identifier for good: no entity moves to
//! another identifier, so a type that is dropped from the registry leaves its saved entities
//! unloaded until it is registered again.
//!
//! A type's box is its width and height; the API has no separate hit box, collision box or shadow.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use super::kind::{Character, Custom, Form, Kind, MAX_NAME_TAG_BYTES, Physics};
use super::{Entities, Entity, Mut, SessionId, State, create};
use crate::ServerBuilder;
use crate::custom::is_identifier;
use crate::entity::{Behaviour, Custom as Definition, Error, Id, Result};
use crate::item::Stack;
use crate::math::Vec3;
use crate::nbt::{self, Compound, List, Value};
use crate::player;
use crate::protocol::{self, Reader, Writer};
use crate::tick::Context;
use crate::tick::world_api::index;
use crate::world;

/// The allocation budget for reading the vanilla identifier list (it is 8,907 bytes).
const LIST_BUDGET: usize = 1 << 20;
/// The largest custom data of one entity, in bytes of its saved (little-endian NBT) form.
/// A chunk of a thousand custom entities stays within a few MiB.
pub(super) const MAX_DATA_BYTES: usize = 4096;
/// The largest maximum health of a type.
const MAX_HEALTH: f64 = 1_000_000.0;
/// The largest box side of a type, in blocks, and of any entity's scaled box.
pub(super) const MAX_SIZE: f64 = 16.0;
/// The most custom entity types a server registers.
pub(crate) const MAX_TYPES: usize = 1024;

/// The custom entity types of a server, fixed when it is built.
#[derive(Default)]
pub(crate) struct Customs {
    /// The types, shared with the save thread once the server is built.
    types: Arc<EntityTypes>,
    /// The behaviour of each type (`None` without one, or while it runs).
    behaviours: Vec<Option<Box<dyn Behaviour>>>,
    /// The first registration that was refused, reported by [`Self::finish`].
    refused: Option<Error>,
}

impl Customs {
    /// The type named `identifier`.
    pub(super) fn find(&self, identifier: &str) -> Option<&Arc<Spec>> {
        self.types.find(identifier)
    }

    /// The `AvailableActorIdentifiers` list with one entry per type appended to `base`.
    /// `base` is the vanilla list, network NBT `{idlist: [..]}`. The entries follow registration
    /// order, as the official server lists an add-on's entities (`bid` empty, `hasspawnegg`, `id`,
    /// `rid` after the largest vanilla one, `summonable`). The two flags only mark the entry:
    /// Anchor has no `summon` command and adds no spawn egg item. Without types it is `base`, byte
    /// for byte.
    pub(crate) fn identifier_list(&self, base: &[u8]) -> protocol::Result<Vec<u8>> {
        let types = &self.types.types;
        if types.is_empty() {
            return Ok(base.to_vec());
        }
        let mut root = Reader::new(base, LIST_BUDGET, 0).nbt(nbt::Encoding::NetworkLittleEndian)?;
        let Some(Value::List(List::Compound(list))) = root.get_mut("idlist") else {
            return Err(protocol::Error::InvalidValue {
                field: "idlist",
                reason: "not a list of compounds",
            });
        };
        let mut rid = list
            .iter()
            .filter_map(|c| match c.get("rid") {
                Some(Value::Int(r)) => Some(*r),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        list.reserve(types.len());
        for spec in types {
            rid = rid.saturating_add(1);
            list.push(Compound::from([
                ("bid".to_owned(), Value::String(String::new())),
                (
                    "hasspawnegg".to_owned(),
                    Value::Byte(u8::from(spec.spawn_egg)),
                ),
                ("id".to_owned(), Value::String(spec.identifier.clone())),
                ("rid".to_owned(), Value::Int(rid)),
                (
                    "summonable".to_owned(),
                    Value::Byte(u8::from(spec.summonable)),
                ),
            ]));
        }
        let mut w = Writer::new(Vec::with_capacity(base.len().saturating_mul(2)), 0);
        w.nbt(&root, nbt::Encoding::NetworkLittleEndian)?;
        Ok(w.into_inner())
    }

    /// The types without their behaviours.
    pub(crate) fn types(&self) -> &Arc<EntityTypes> {
        &self.types
    }

    /// Takes the first refused registration, which the build reports.
    pub(crate) fn finish(&mut self) -> Result<()> {
        self.refused.take().map_or(Ok(()), Err)
    }

    /// Adds a type.
    /// A type that breaks a rule is refused, and [`Self::finish`] reports the first.
    pub(crate) fn register(&mut self, entity: Definition) {
        if self.refused.is_some() {
            return;
        }
        match self.check(&entity) {
            Ok(index) => {
                let Definition {
                    identifier,
                    summonable,
                    spawn_egg,
                    width,
                    height,
                    gravity,
                    drag,
                    max_health,
                    name_tag,
                    saved,
                    tick_period,
                    behaviour,
                } = entity;
                // Registration ends before the server is built, so nothing shares the types yet
                // and this never copies them.
                let types = Arc::make_mut(&mut self.types);
                types.by_name.insert(identifier.clone(), index);
                types.types.push(Arc::new(Spec {
                    index,
                    identifier,
                    summonable,
                    spawn_egg,
                    physics: Physics {
                        width,
                        height,
                        gravity,
                        drag,
                        drag_before_gravity: false,
                        unstuck: false,
                        currents: true,
                        ray: false,
                        water: false,
                    },
                    max_health,
                    name_tag,
                    saved,
                    tick_period: tick_period.get(),
                }));
                self.behaviours.push(behaviour);
            }
            Err(e) => self.refused = Some(e),
        }
    }

    /// The index a new type would get, or why it is refused.
    fn check(&self, e: &Definition) -> Result<u16> {
        let identifier = &e.identifier;
        if !is_identifier(identifier) {
            return Err(Error::InvalidIdentifier {
                identifier: identifier.clone(),
            });
        }
        let types = &self.types.types;
        if self.types.by_name.contains_key(identifier) {
            return Err(Error::DuplicateIdentifier {
                identifier: identifier.clone(),
            });
        }
        let index = (types.len() < MAX_TYPES)
            .then(|| u16::try_from(types.len()).ok())
            .flatten()
            .ok_or(Error::TooManyTypes)?;
        let size = |v: f64| v > 0.0 && v <= MAX_SIZE;
        let field = if !size(e.width) {
            Some("width")
        } else if !size(e.height) {
            Some("height")
        } else if !(0.0..=1.0).contains(&e.gravity) {
            Some("gravity")
        } else if !(0.0..1.0).contains(&e.drag) {
            Some("drag")
        } else if !(e.max_health > 0.0 && e.max_health <= MAX_HEALTH) {
            Some("max health")
        } else if e.name_tag.len() > MAX_NAME_TAG_BYTES {
            Some("name tag")
        } else {
            None
        };
        match field {
            Some(field) => Err(Error::InvalidValue {
                identifier: identifier.clone(),
                field,
            }),
            None => Ok(index),
        }
    }
}

impl fmt::Debug for Customs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Customs")
            .field("types", &self.types.by_name.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// The custom entity types of a server without their behaviours.
/// It is what reading a saved entity needs, so the save thread reads loaded chunks with it too.
/// Fixed when the server is built.
#[derive(Debug, Clone, Default)]
pub(crate) struct EntityTypes {
    /// The types in registration order.
    types: Vec<Arc<Spec>>,
    /// The type index by identifier.
    by_name: BTreeMap<String, u16>,
}

impl EntityTypes {
    /// The type named `identifier`.
    pub(super) fn find(&self, identifier: &str) -> Option<&Arc<Spec>> {
        self.types.get(usize::from(*self.by_name.get(identifier)?))
    }
}

/// A registered custom entity type: what every entity of it shares.
#[derive(Debug, PartialEq)]
pub(super) struct Spec {
    /// Its place in the registry.
    pub(super) index: u16,
    /// Its identifier, such as `example:ghost`.
    pub(super) identifier: String,
    /// Whether its entry in the identifier list is marked summonable.
    pub(super) summonable: bool,
    /// Whether its entry in the identifier list is marked as having a spawn egg.
    pub(super) spawn_egg: bool,
    /// Its box and how it moves: the type's size, gravity and drag.
    pub(super) physics: Physics,
    /// The health of a new entity of it.
    pub(super) max_health: f64,
    /// The name tag of a new entity.
    pub(super) name_tag: String,
    /// Whether its entities are saved with their chunk.
    pub(super) saved: bool,
    /// The ticks of an entity's age between behaviour ticks, at least 1.
    pub(super) tick_period: u32,
}

impl Entities {
    /// The custom entity types without their behaviours: what reading saved entities needs.
    pub(in crate::tick) fn types(&self) -> &Arc<EntityTypes> {
        self.custom.types()
    }

    /// Takes the custom entity types of the server (at start, before any chunk loads).
    pub(crate) fn set_customs(&mut self, customs: Customs) {
        self.custom = customs;
    }

    /// Lends out the custom entity types.
    /// A checkpoint's restore reads saved custom entities with them; [`Self::set_customs`] gives
    /// them back.
    pub(crate) fn take_customs(&mut self) -> Customs {
        std::mem::take(&mut self.custom)
    }
}

impl ServerBuilder {
    /// Registers the custom entity type `entity`.
    /// [`Context::spawn_custom_entity`] spawns it by its identifier, and the client draws it from
    /// a resource pack. [`build`](Self::build) fails with [`Error`] if a type's identifier is not
    /// a custom one or is taken, a value is out of range, or more than 1,024 types are registered.
    #[doc(alias = "EntityFactory")]
    #[doc(alias = "EntityFactory::register")]
    #[must_use]
    pub fn custom_entity(mut self, entity: Definition) -> Self {
        self.setup.custom_entities.register(entity);
        self
    }
}

impl Context<'_> {
    /// Spawns an entity of the custom type `identifier` at `pos` (its feet) in `world`.
    /// It is at rest, with the type's health and name tag and empty data, and is shown to the
    /// players who see its chunk at the end of the tick.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownType`] for an `identifier` no registered custom type has.
    /// [`Error::NotSpawned`] for a world the server does not have, a `pos` that is not finite or
    /// outside the world border or not loaded, or a world at its entity limit.
    #[doc(alias = "Entity::spawnToAll")]
    pub fn spawn_custom_entity(
        &mut self,
        world: world::Id,
        pos: Vec3,
        identifier: &str,
    ) -> Result<Id> {
        let Some(spec) = self.state.rules.entities.custom.find(identifier).cloned() else {
            return Err(Error::UnknownType {
                identifier: identifier.to_owned(),
            });
        };
        let w = index(self.state, world)
            .filter(|_| super::is_valid_position(pos))
            .ok_or(Error::NotSpawned)?;
        let kind = kind(spec);
        create(self.state, w, pos, kind, |_| (Vec3::ZERO, 0.0, 0.0)).ok_or(Error::NotSpawned)
    }
}

impl Entity {
    /// An NPC's data, saved with it, empty until set.
    /// It is a custom entity's, a villager's ([`Context::spawn_villager`]), a human's
    /// ([`Context::spawn_human`]) or a zombie's. `None` for another kind. [`Mut::set_custom_data`]
    /// replaces it whole: to change one key, clone it, change the copy and set it back.
    pub fn custom_data(&self) -> Option<&Compound> {
        match &self.kind {
            Kind::Npc(c) => Some(&c.data),
            _ => None,
        }
    }

    /// The identifier of a custom entity's type, such as `"example:ghost"`.
    pub fn custom_type(&self) -> Option<&str> {
        self.kind.custom().map(|c| c.spec.identifier.as_str())
    }

    /// What an NPC holds in its hand ([`Stack::EMPTY`] for nothing); `None` for another kind.
    pub fn held_item(&self) -> Option<&Stack> {
        match &self.kind {
            Kind::Npc(c) => Some(&c.held),
            _ => None,
        }
    }
}

impl Mut<'_> {
    /// Sets an NPC's data ([`Entity::custom_data`]), saved with it.
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind and [`Error::OutOfRange`] for data whose saved form
    /// takes more than 4,096 bytes; nothing changes.
    pub fn set_custom_data(&mut self, data: Compound) -> Result<()> {
        if !fits(&data) {
            return Err(Error::OutOfRange);
        }
        let Kind::Npc(c) = &mut self.entity_mut().kind else {
            return Err(Error::WrongKind);
        };
        c.data = data;
        Ok(())
    }

    /// Puts `stack` in an NPC's hand ([`Entity::held_item`]; [`Stack::EMPTY`] empties it).
    /// Its viewers see it, and it is saved with the NPC.
    ///
    /// # Errors
    ///
    /// [`Error::WrongKind`] for another kind; nothing changes.
    pub fn set_held_item(&mut self, stack: Stack) -> Result<()> {
        let Kind::Npc(c) = &mut self.entity_mut().kind else {
            return Err(Error::WrongKind);
        };
        c.held = stack;
        self.shown_mut();
        Ok(())
    }
}

/// Ticks the custom entity `id` after its NPC rules.
/// It calls its behaviour's [`on_tick`](Behaviour::on_tick) when its age is a multiple of the
/// type's period.
pub(super) fn tick(state: &mut State, id: Id) {
    let due = state.rules.entities.get(id).is_some_and(|e| {
        e.kind
            .custom()
            .is_some_and(|c| e.age % c.spec.tick_period == 0)
    });
    if !due {
        return;
    }
    if let Some(index) = live_type(state, id) {
        call(state, index, |b, cx| b.on_tick(cx, id));
    }
}

/// Calls the behaviour of the custom entity `target` for an interaction by `session`.
/// The caller checked the reach. It calls [`on_interact`](Behaviour::on_interact); false if
/// `target` is not a live custom entity.
pub(super) fn interact(state: &mut State, session: SessionId, target: Id) -> bool {
    let Some(index) = live_type(state, target) else {
        return false;
    };
    call(state, index, |b, cx| {
        b.on_interact(cx, target, player::Id::new(session));
    });
    true
}

/// The type index of the live, unflagged custom entity `id`.
fn live_type(state: &State, id: Id) -> Option<u16> {
    let e = state.rules.entities.get(id).filter(|e| !e.flagged)?;
    e.kind
        .custom()
        .filter(|_| e.kind.is_living())
        .map(|c| c.spec.index)
}

/// Runs `f` with the behaviour of type `index` and a context on the whole state.
/// It does nothing if the type has no behaviour; the registry lends the behaviour out meanwhile.
fn call(state: &mut State, index: u16, f: impl FnOnce(&mut dyn Behaviour, &mut Context<'_>)) {
    let slot = usize::from(index);
    let Some(mut behaviour) = state
        .rules
        .entities
        .custom
        .behaviours
        .get_mut(slot)
        .and_then(Option::take)
    else {
        return;
    };
    f(behaviour.as_mut(), &mut Context { state });
    #[expect(
        clippy::expect_used,
        reason = "invariant: the behaviour list never shrinks after the build, so the slot the \
                  behaviour was taken from is still there"
    )]
    let back = state
        .rules
        .entities
        .custom
        .behaviours
        .get_mut(slot)
        .expect("the behaviour slot is in the registry");
    *back = Some(behaviour);
}

/// Whether `data` may be an NPC's: its saved form takes at most [`MAX_DATA_BYTES`].
/// Writing stops as it passes the limit, so a huge compound costs no more than the limit.
pub(super) fn fits(data: &Compound) -> bool {
    let mut w = Writer::new(Vec::new(), 0);
    w.nbt_within(data, nbt::Encoding::LittleEndian, MAX_DATA_BYTES)
        .is_ok()
}

/// The kind of a new custom entity of `spec`.
/// It is alive, not invulnerable, with empty data and empty-handed.
pub(super) fn kind(spec: Arc<Spec>) -> Kind {
    Kind::Npc(Character {
        form: Form::Custom(Custom { spec }),
        invulnerable: false,
        attack_time: 0,
        last_damage: 0.0,
        dead_ticks: None,
        data: Compound::new(),
        held: Stack::EMPTY,
    })
}

#[cfg(test)]
mod tests;
