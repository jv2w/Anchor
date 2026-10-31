//! Custom entity types: their definition ([`Custom`]), what drives them
//! ([`Behaviour`]) and why one is refused ([`Error`]).

use std::fmt;
use std::num::NonZeroU32;

use super::Id;
use crate::Context;
use crate::player;

/// What a custom entity type does, called by the tick. Every method does nothing by default.
///
/// A behaviour is called only in the tick's serial steps, with a [`Context`] on the whole game,
/// and never from inside another callback (direct calls through the context raise no events and
/// call no behaviour):
///
/// - [`on_tick`](Self::on_tick) in the entity tick, in entity [`Id`] order, for the entities in
///   chunks within `tick-radius` of a player, after the entity's movement, the blocks it is in,
///   burning, the void and its living rules (suffocation, hit cooldown), every `tick_period`
///   ticks of its age (the ticks it has been ticked; the first call comes when it is
///   `tick_period` ticks old). A dying entity is not ticked.
/// - [`on_interact`](Self::on_interact) when a player within reach interacts with a live entity
///   (PMMP `Player::interactEntity`), after the
///   [`PlayerEntityInteract`](crate::event::PlayerEntityInteract) listeners and before its
///   monitors.
///
/// Damage and death are the [`EntityDamage`](crate::event::EntityDamage) and
/// [`EntityDeath`](crate::event::EntityDeath) events, which name the entity; its
/// [`custom_type`](crate::entity::Entity::custom_type) tells the type.
pub trait Behaviour: Send {
    /// One tick of the custom entity `entity`, every `tick_period` ticks of its age.
    #[doc(alias = "Entity::onUpdate")]
    fn on_tick(&mut self, cx: &mut Context<'_>, entity: Id) {
        let _ = (cx, entity);
    }

    /// The player `player` interacts with the custom entity `entity` (a right click within reach).
    #[doc(alias = "Entity::onInteract")]
    fn on_interact(&mut self, cx: &mut Context<'_>, entity: Id, player: player::Id) {
        let _ = (cx, entity, player);
    }
}

/// The definition of a custom entity type, registered with
/// [`ServerBuilder::custom_entity`](crate::ServerBuilder::custom_entity), drawn by the client
/// from a resource pack and driven by a [`Behaviour`].
///
/// A custom entity is a living entity without AI (as Anchor's NPCs): it falls and collides with
/// the physics constants of its type, takes damage (fire, lava, the void, falls, suffocation,
/// explosions, hits and projectiles) with PMMP `Living`'s hit cooldown, knockback and death
/// animation, raises the entity events, and is saved with its chunk under its identifier unless
/// its type is temporary; a saved entity whose type is no longer registered stays in the chunk as
/// it was saved. The engine lists the registered identifiers in `AvailableActorIdentifiers` and
/// shows each entity with `AddActor` of its identifier, its name tag, scale, box and health.
///
/// The defaults are PMMP `Living`'s: a 0.6 × 1.8 box, gravity 0.08, drag 0.02, 20 health; no
/// name tag, saved with its chunk, ticked every tick, not summonable, without a spawn egg and
/// without a behaviour. [`build`](crate::ServerBuilder::build) checks the values
/// ([`Error::InvalidValue`]).
pub struct Custom {
    pub(crate) identifier: String,
    pub(crate) summonable: bool,
    pub(crate) spawn_egg: bool,
    pub(crate) width: f64,
    pub(crate) height: f64,
    pub(crate) gravity: f64,
    pub(crate) drag: f64,
    pub(crate) max_health: f64,
    pub(crate) name_tag: String,
    pub(crate) saved: bool,
    pub(crate) tick_period: NonZeroU32,
    pub(crate) behaviour: Option<Box<dyn Behaviour>>,
}

impl Custom {
    /// A type named `identifier`: `namespace:name` in lowercase ASCII letters, digits and `_.-`
    /// (`/` too in the name), at most 128 bytes, in a namespace other than `minecraft`.
    pub fn new(identifier: impl Into<String>) -> Self {
        Self {
            identifier: identifier.into(),
            summonable: false,
            spawn_egg: false,
            width: 0.6,
            height: 1.8,
            gravity: 0.08,
            drag: 0.02,
            max_health: 20.0,
            name_tag: String::new(),
            saved: true,
            tick_period: NonZeroU32::MIN,
            behaviour: None,
        }
    }

    /// Its identifier.
    pub fn identifier(&self) -> &str {
        &self.identifier
    }

    /// This type with a box `width` wide and deep and `height` high, in blocks (above 0, at most
    /// 16), for collisions, hits and what clients draw.
    #[must_use]
    pub fn with_size(self, width: f64, height: f64) -> Self {
        Self {
            width,
            height,
            ..self
        }
    }

    /// This type with a downward acceleration of `gravity` blocks per tick² (0 to 1).
    #[must_use]
    pub fn with_gravity(self, gravity: f64) -> Self {
        Self { gravity, ..self }
    }

    /// This type losing the share `drag` of its speed per tick in the air (0 up to, not
    /// including, 1).
    #[must_use]
    pub fn with_drag(self, drag: f64) -> Self {
        Self { drag, ..self }
    }

    /// This type with `max_health` (above 0, at most 1,000,000), which new entities start with.
    #[must_use]
    pub fn with_max_health(self, max_health: f64) -> Self {
        Self { max_health, ..self }
    }

    /// This type with the name tag new entities start with (at most
    /// [`MAX_NAME_TAG_BYTES`](super::MAX_NAME_TAG_BYTES)).
    #[must_use]
    pub fn with_name_tag(self, name_tag: impl Into<String>) -> Self {
        Self {
            name_tag: name_tag.into(),
            ..self
        }
    }

    /// This type with its behaviour called every `period` ticks of an entity's age.
    #[must_use]
    pub fn with_tick_period(self, period: NonZeroU32) -> Self {
        Self {
            tick_period: period,
            ..self
        }
    }

    /// This type driven by `behaviour`.
    #[must_use]
    pub fn with_behaviour(self, behaviour: impl Behaviour + 'static) -> Self {
        Self {
            behaviour: Some(Box::new(behaviour)),
            ..self
        }
    }

    /// This type listed as summonable (the client's `/summon` completion).
    #[must_use]
    pub fn summonable(self) -> Self {
        Self {
            summonable: true,
            ..self
        }
    }

    /// This type listed with a spawn egg (the egg item comes from the resource pack).
    #[must_use]
    pub fn spawn_egg(self) -> Self {
        Self {
            spawn_egg: true,
            ..self
        }
    }

    /// This type's entities are never saved: they go when their chunk unloads (a minigame's
    /// props).
    #[must_use]
    pub fn temporary(self) -> Self {
        Self {
            saved: false,
            ..self
        }
    }
}

impl fmt::Debug for Custom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Custom")
            .field("identifier", &self.identifier)
            .field("summonable", &self.summonable)
            .field("spawn_egg", &self.spawn_egg)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("gravity", &self.gravity)
            .field("drag", &self.drag)
            .field("max_health", &self.max_health)
            .field("name_tag", &self.name_tag)
            .field("saved", &self.saved)
            .field("tick_period", &self.tick_period)
            .field("behaviour", &self.behaviour.is_some())
            .finish()
    }
}

/// Why a custom entity type was refused or a custom entity was not spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The identifier is not `namespace:name` of the allowed characters, is too long, or is in
    /// the `minecraft` namespace.
    InvalidIdentifier {
        /// The identifier.
        identifier: String,
    },
    /// Two types share an identifier.
    DuplicateIdentifier {
        /// The identifier.
        identifier: String,
    },
    /// More types than the server takes (1,024).
    TooManyTypes,
    /// A value of the definition is out of its range.
    InvalidValue {
        /// The type's identifier.
        identifier: String,
        /// What is out of range.
        field: &'static str,
    },
    /// No type of this identifier is registered.
    UnknownType {
        /// The identifier.
        identifier: String,
    },
    /// The world does not exist, the position is not finite, outside the world border, more than
    /// 64 blocks above or below the world height or not loaded, or the world is at its entity
    /// limit.
    NotSpawned,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentifier { identifier } => {
                write!(f, "{identifier:?} is not a custom entity identifier")
            }
            Self::DuplicateIdentifier { identifier } => {
                write!(f, "two custom entity types are named {identifier:?}")
            }
            Self::TooManyTypes => f.write_str("too many custom entity types"),
            Self::InvalidValue { identifier, field } => {
                write!(
                    f,
                    "the {field} of custom entity {identifier:?} is out of range"
                )
            }
            Self::UnknownType { identifier } => {
                write!(f, "no custom entity type is named {identifier:?}")
            }
            Self::NotSpawned => f.write_str("the custom entity could not be spawned there"),
        }
    }
}

impl std::error::Error for Error {}
