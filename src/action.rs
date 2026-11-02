//! Actions: what a player or the game does, done on request through the game rules.
//!
//! [`Context::perform`](crate::Context::perform) takes one and raises its events.
//!
//! The direct API (for example [`Player`](crate::player::Player) setters) changes the game
//! without events; an action goes the way the game itself would, events and all. An action is
//! queued and checks its conditions when it runs, so one that fails does nothing and reports
//! nothing.

mod entity;
mod interact;
mod sealed {
    use crate::tick::Context;

    /// Does the action.
    pub trait Sealed {
        /// Does the action now, through the game rules, raising its events.
        fn perform(self, cx: &mut Context<'_>);
    }
}
mod survival;

pub use entity::{Attack, Explode};
pub use interact::{AttackBlock, BreakBlock, InteractEntity, UseItem, UseItemOn};
/// Public in name only: other crates can neither name nor implement it.
pub(crate) use sealed::Sealed;
pub use survival::{Chat, Damage, Kick, SetGameMode, Teleport, Transfer};

use std::fmt;

/// An action [`Context::perform`](crate::Context::perform) can do.
///
/// Only Anchor's actions implement it.
pub trait Action: Sealed + Send + 'static {}

/// Why an action or a deferred callback was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The deferred queue is full, so nothing was queued.
    Full,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => f.write_str("the deferred queue is full"),
        }
    }
}

impl std::error::Error for Error {}

/// A result with the module's [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_queue_is_an_error_with_a_lowercase_message() {
        let e: Result<()> = Err(Error::Full);
        assert_eq!(
            e.map_err(|e| e.to_string()),
            Err("the deferred queue is full".to_owned())
        );
    }
}
