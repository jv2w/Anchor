//! Custom entity types: their definition ([`Custom`]), what drives them ([`Behavior`]) and why a
//! call on entities fails ([`Error`]).

use std::fmt;
use std::num::NonZeroU32;

use super::Id;
use crate::Context;
use crate::player;

/// What a custom entity type does, called by the tick. Every method does nothing by default.
///
/// A behavior is called only in the tick's serial steps, with a [`Context`] on the whole game,
/// and never from inside another callback (direct calls through the context raise no events and
/// call no behavior):
///
/// - [`on_tick`](Self::on_tick) in the entity tick, in entity [`Id`] order, for the entities in
///   chunks within `tick-radius` of a player, after the entity's movement, the blocks it is in,
///   burning, the void and its living rules (suffocation, hit cooldown), every `tick_period`
///   ticks of its age (the ticks it has been ticked; the first call comes when it is
///   `tick_period` ticks old). A dying entity is not ticked.
/// - [`on_interact`](Self::on_interact) when a player within reach interacts with a live entity,
///   after the [`PlayerEntityInteract`](crate::event::PlayerEntityInteract) listeners and before
///   its monitors; not when a listener cancels the event.
///
/// Damage and death are the [`EntityDamage`](crate::event::EntityDamage) and
/// [`EntityDeath`](crate::event::EntityDeath) events, which name the entity; its
/// [`custom_type`](crate::entity::Entity::custom_type) tells the type.
pub trait Behavior: Send {
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
/// from a resource pack and driven by a [`Behavior`].
///
/// A custom entity is a living entity without AI (as Anchor's NPCs): it falls and collides with
/// the physics constants of its type, takes damage (fire, lava, the void, falls, suffocation,
/// explosions, hits and projectiles) with a living entity's hit cooldown, knockback and death
/// animation, raises the entity events, and is saved with its chunk under its identifier unless
/// its type is temporary; a saved entity whose type is no longer registered stays in the chunk as
/// it was saved. The engine lists the registered identifiers in `AvailableActorIdentifiers` and
/// shows each entity with `AddActor` of its identifier, its name tag, scale, box and health.
///
/// The defaults: a 0.6 × 1.8 box, gravity 0.08, drag 0.02, 20 health; no name tag, saved with its
/// chunk, ticked every tick, not summonable, without a spawn egg and without a behavior.
/// [`build`](crate::ServerBuilder::build) checks the values ([`Error::InvalidValue`]).
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
    pub(crate) behavior: Option<Box<dyn Behavior>>,
}

impl Custom {
    /// The drag of a type that sets none.
    const DEFAULT_DRAG: f64 = 0.02;
    /// The gravity, in blocks per tick², of a type that sets none.
    const DEFAULT_GRAVITY: f64 = 0.08;
    /// The box height, in blocks, of a type that sets none.
    const DEFAULT_HEIGHT: f64 = 1.8;
    /// The maximum health of a type that sets none.
    const DEFAULT_MAX_HEALTH: f64 = 20.0;
    /// The box width and depth, in blocks, of a type that sets none.
    const DEFAULT_WIDTH: f64 = 0.6;

    /// A type named `identifier`: `namespace:name` in lowercase ASCII letters, digits and `_.-`
    /// (`/` too in the name), at most 128 bytes, in a namespace other than `minecraft`.
    pub fn new(identifier: impl Into<String>) -> Self {
        Self {
            identifier: identifier.into(),
            summonable: false,
            spawn_egg: false,
            width: Self::DEFAULT_WIDTH,
            height: Self::DEFAULT_HEIGHT,
            gravity: Self::DEFAULT_GRAVITY,
            drag: Self::DEFAULT_DRAG,
            max_health: Self::DEFAULT_MAX_HEALTH,
            name_tag: String::new(),
            saved: true,
            tick_period: NonZeroU32::MIN,
            behavior: None,
        }
    }

    /// Its identifier.
    pub fn identifier(&self) -> &str {
        &self.identifier
    }

    /// The same type flagged as having a spawn egg in the actor identifier list. It sets the flag
    /// only: Anchor adds no spawn egg item.
    #[must_use]
    pub fn spawn_egg(self) -> Self {
        Self {
            spawn_egg: true,
            ..self
        }
    }

    /// The same type flagged summonable in the actor identifier list. It sets the flag only:
    /// Anchor adds no `/summon` command.
    #[must_use]
    pub fn summonable(self) -> Self {
        Self {
            summonable: true,
            ..self
        }
    }

    /// The same type with entities that are never saved: they go when their chunk unloads (a
    /// minigame's props).
    #[must_use]
    pub fn temporary(self) -> Self {
        Self {
            saved: false,
            ..self
        }
    }

    /// The same type driven by `behavior`.
    #[must_use]
    pub fn with_behavior(self, behavior: impl Behavior + 'static) -> Self {
        Self {
            behavior: Some(Box::new(behavior)),
            ..self
        }
    }

    /// The same type losing the share `drag` of its speed per tick in the air (0 up to, not
    /// including, 1).
    #[must_use]
    pub fn with_drag(self, drag: f64) -> Self {
        Self { drag, ..self }
    }

    /// The same type with a downward acceleration of `gravity` blocks per tick² (0 to 1).
    #[must_use]
    pub fn with_gravity(self, gravity: f64) -> Self {
        Self { gravity, ..self }
    }

    /// The same type with `max_health` (above 0, at most 1,000,000), which new entities start with.
    #[must_use]
    pub fn with_max_health(self, max_health: f64) -> Self {
        Self { max_health, ..self }
    }

    /// The same type with the name tag new entities start with (at most
    /// [`MAX_NAME_TAG_BYTES`](super::MAX_NAME_TAG_BYTES)).
    #[must_use]
    pub fn with_name_tag(self, name_tag: impl Into<String>) -> Self {
        Self {
            name_tag: name_tag.into(),
            ..self
        }
    }

    /// The same type with a box `width` wide and deep and `height` high, in blocks (above 0, at most
    /// 16), for collisions, hits and what clients draw.
    #[must_use]
    pub fn with_size(self, width: f64, height: f64) -> Self {
        Self {
            width,
            height,
            ..self
        }
    }

    /// The same type with its behavior called every `period` ticks of an entity's age.
    #[must_use]
    pub fn with_tick_period(self, period: NonZeroU32) -> Self {
        Self {
            tick_period: period,
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
            .field("behavior", &self.behavior.is_some())
            .finish()
    }
}

/// Why a call on entities failed: a custom entity type was refused, an entity was not spawned, or
/// an entity or a player refused a value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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
    /// The entity does not live: it is dying.
    NotAlive,
    /// The player is not in the game: it has left, or has not joined yet.
    Offline,
    /// The entity was not spawned: the world does not exist, the position is not finite, outside
    /// the world border or not loaded, or the world is at its entity limit.
    NotSpawned,
    /// A value is outside the range the call allows, or is not a number.
    OutOfRange,
    /// A name (a name tag, an owner or thrower, a human NPC's name) is longer than
    /// [`MAX_NAME_TAG_BYTES`](crate::entity::MAX_NAME_TAG_BYTES).
    TooLong,
    /// The entity is of another kind than the call works on.
    WrongKind,
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
            Self::NotAlive => f.write_str("the entity is not alive"),
            Self::Offline => f.write_str("the player is not in the game"),
            Self::NotSpawned => f.write_str("the entity could not be spawned there"),
            Self::OutOfRange => f.write_str("a value is out of range"),
            Self::TooLong => f.write_str("the name is too long"),
            Self::WrongKind => f.write_str("the entity is of another kind"),
        }
    }
}

impl std::error::Error for Error {}

/// A result with the module's [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
