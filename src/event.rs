//! Events: listener priorities, the events Anchor raises, and the listener lists the tick calls.
//!
//! Listeners are registered with [`ServerBuilder::on`](crate::ServerBuilder::on), sorted once
//! when the server is built, and called only in the tick's serial step (PLAN.md 5, 8.2). Every
//! listener sees the event, cancelled or not; one that should not act on a cancelled event checks
//! `is_cancelled` (PMMP calls a listener on a cancelled event only with `@handleCancelled`).

use std::fmt;

use crate::protocol::Uuid;
use crate::tick::queue::Identity;

pub(crate) use sealed::{List, Listeners};

/// When a listener runs relative to the other listeners of the same event. Listeners run from
/// [`Lowest`](Self::Lowest) to [`Highest`](Self::Highest), so a higher priority has the last say;
/// listeners of equal priority run in registration order.
///
/// PMMP's `MONITOR` is missing: a read-only monitor arrives with the full event system (M8).
#[doc(alias = "EventPriority")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// Runs first, so that other listeners can change the outcome.
    #[doc(alias = "EventPriority::LOWEST")]
    Lowest,
    /// Runs after [`Lowest`](Self::Lowest).
    #[doc(alias = "EventPriority::LOW")]
    Low,
    /// The usual priority.
    #[doc(alias = "EventPriority::NORMAL")]
    Normal,
    /// Runs after [`Normal`](Self::Normal).
    #[doc(alias = "EventPriority::HIGH")]
    High,
    /// Runs last and decides the outcome.
    #[doc(alias = "EventPriority::HIGHEST")]
    Highest,
}

/// An event Anchor raises. Only Anchor's own events implement it.
pub trait Event: sealed::Sealed + Send + 'static {}

/// Who a player is, from the verified login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerInfo(Identity);

impl PlayerInfo {
    /// The player's name: the Xbox gamertag when Xbox sign-in is on, otherwise the name the
    /// client chose.
    #[doc(alias = "PlayerInfo::getUsername")]
    pub fn name(&self) -> &str {
        &self.0.name
    }

    /// The Xbox user ID, or `None` when Xbox sign-in (`xbox-auth`) is off.
    #[doc(alias = "XboxLivePlayerInfo::getXuid")]
    pub fn xuid(&self) -> Option<u64> {
        self.0.xuid
    }

    /// The identity UUID the client sent.
    #[doc(alias = "PlayerInfo::getUuid")]
    pub fn uuid(&self) -> Uuid {
        self.0.uuid
    }
}

/// Raised when a player joins while the same player is already online: the same XUID with Xbox
/// sign-in on, otherwise the same name ignoring ASCII case.
///
/// If no listener cancels it, the existing player is disconnected and the connecting one joins;
/// if it is cancelled, the connecting player is disconnected instead. Either way the
/// disconnected player sees [`disconnect_screen_message`](Self::disconnect_screen_message), or
/// [`disconnect_reason`](Self::disconnect_reason) when that is `None`.
#[doc(alias = "PlayerDuplicateLoginEvent")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerDuplicateLogin {
    connecting: PlayerInfo,
    existing: PlayerInfo,
    reason: String,
    screen_message: Option<String>,
    cancelled: bool,
}

impl PlayerDuplicateLogin {
    /// The reason the event starts with: PMMP's English `disconnectionScreen.loggedinOtherLocation`.
    pub(crate) const DEFAULT_REASON: &str = "Logged in from other location";

    /// The event for `connecting` meeting `existing`, with the default reason, not cancelled.
    pub(crate) fn new(connecting: Identity, existing: Identity) -> Self {
        Self {
            connecting: PlayerInfo(connecting),
            existing: PlayerInfo(existing),
            reason: Self::DEFAULT_REASON.to_owned(),
            screen_message: None,
            cancelled: false,
        }
    }

    /// The outcome the listeners chose, and the connecting player's identity back.
    pub(crate) fn into_outcome(self) -> (Identity, DuplicateOutcome) {
        let outcome = DuplicateOutcome {
            cancelled: self.cancelled,
            message: self.screen_message.unwrap_or(self.reason),
        };
        (self.connecting.0, outcome)
    }

    /// The player who is joining.
    #[doc(alias = "PlayerDuplicateLoginEvent::getConnectingSession")]
    pub fn connecting(&self) -> &PlayerInfo {
        &self.connecting
    }

    /// The player already online.
    #[doc(alias = "PlayerDuplicateLoginEvent::getExistingSession")]
    pub fn existing(&self) -> &PlayerInfo {
        &self.existing
    }

    /// The reason given to the disconnected player. Starts as `"Logged in from other location"`.
    #[doc(alias = "PlayerDuplicateLoginEvent::getDisconnectReason")]
    pub fn disconnect_reason(&self) -> &str {
        &self.reason
    }

    /// Replaces the reason given to the disconnected player.
    #[doc(alias = "PlayerDuplicateLoginEvent::setDisconnectReason")]
    pub fn set_disconnect_reason(&mut self, reason: impl Into<String>) {
        self.reason = reason.into();
    }

    /// The message the disconnected player sees instead of the reason, if any. Starts as `None`.
    #[doc(alias = "PlayerDuplicateLoginEvent::getDisconnectScreenMessage")]
    pub fn disconnect_screen_message(&self) -> Option<&str> {
        self.screen_message.as_deref()
    }

    /// Replaces the message the disconnected player sees; `None` shows the reason.
    #[doc(alias = "PlayerDuplicateLoginEvent::setDisconnectScreenMessage")]
    pub fn set_disconnect_screen_message(&mut self, message: Option<String>) {
        self.screen_message = message;
    }

    /// Keeps the existing player and disconnects the connecting one.
    #[doc(alias = "PlayerDuplicateLoginEvent::cancel")]
    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    /// Undoes [`cancel`](Self::cancel): the existing player is disconnected again.
    #[doc(alias = "PlayerDuplicateLoginEvent::uncancel")]
    pub fn uncancel(&mut self) {
        self.cancelled = false;
    }

    /// Whether a listener cancelled the event.
    #[doc(alias = "PlayerDuplicateLoginEvent::isCancelled")]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

impl Event for PlayerDuplicateLogin {}

impl sealed::Sealed for PlayerDuplicateLogin {
    fn list(listeners: &mut Listeners) -> &mut List<Self> {
        &mut listeners.duplicate_login
    }
}

/// What the tick does after a [`PlayerDuplicateLogin`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DuplicateOutcome {
    /// Disconnect the connecting player instead of the existing one.
    pub(crate) cancelled: bool,
    /// The text of the `Disconnect` the disconnected player gets.
    pub(crate) message: String,
}

impl DuplicateOutcome {
    /// The outcome when no listener is registered, without building the event.
    pub(crate) fn unheard() -> Self {
        Self {
            cancelled: false,
            message: PlayerDuplicateLogin::DEFAULT_REASON.to_owned(),
        }
    }
}

/// Public in name only: the module is private, so other crates can neither name nor implement
/// these items, and [`Event`] stays closed to Anchor's events.
mod sealed {
    use super::{Event, PlayerDuplicateLogin, Priority, fmt};

    /// Finds an event's listener list, so that registration needs no type map.
    pub trait Sealed: Sized {
        /// The list of this event in `listeners`.
        fn list(listeners: &mut Listeners) -> &mut List<Self>;
    }

    /// Every event's listeners. Moves into the tick state when the server runs.
    #[derive(Debug, Default)]
    pub struct Listeners {
        pub(super) duplicate_login: List<PlayerDuplicateLogin>,
    }

    /// A listener: an extension point, so a boxed closure (CLAUDE.md 2.2).
    type Listener<E> = Box<dyn FnMut(&mut E) + Send>;

    /// The listeners of one event, in calling order after [`Listeners::sort`].
    pub struct List<E>(Vec<(Priority, Listener<E>)>);

    impl Listeners {
        /// Adds `listener` for `E` at `priority`, after the listeners already added.
        pub(crate) fn push<E: Event>(
            &mut self,
            priority: Priority,
            listener: impl FnMut(&mut E) + Send + 'static,
        ) {
            E::list(self).0.push((priority, Box::new(listener)));
        }

        /// Orders every list by priority, keeping registration order within a priority.
        pub(crate) fn sort(&mut self) {
            let Self { duplicate_login } = self;
            duplicate_login.sort();
        }

        /// The listeners of a duplicate login.
        pub(crate) fn duplicate_login(&mut self) -> &mut List<PlayerDuplicateLogin> {
            &mut self.duplicate_login
        }
    }

    impl<E> List<E> {
        /// Whether no listener is registered, so that the event need not be built.
        pub(crate) fn is_empty(&self) -> bool {
            self.0.is_empty()
        }

        /// Calls every listener on `event` in order.
        pub(crate) fn call(&mut self, event: &mut E) {
            for (_, listener) in &mut self.0 {
                listener(event);
            }
        }

        /// Stable, so equal priorities keep registration order.
        fn sort(&mut self) {
            self.0.sort_by_key(|&(priority, _)| priority);
        }
    }

    impl<E> Default for List<E> {
        fn default() -> Self {
            Self(Vec::new())
        }
    }

    impl<E> fmt::Debug for List<E> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("List")
                .field("listeners", &self.0.len())
                .finish()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str) -> Identity {
        Identity {
            name: name.to_owned(),
            xuid: None,
            uuid: Uuid::from_bytes([1; 16]),
            skin: crate::tick::queue::Skin::default(),
            device_os: -1,
        }
    }

    #[test]
    fn listeners_run_by_priority_then_in_registration_order() {
        let mut listeners = Listeners::default();
        let order = [
            (Priority::Highest, "highest"),
            (Priority::Normal, "normal 1"),
            (Priority::Lowest, "lowest"),
            (Priority::Normal, "normal 2"),
            (Priority::Low, "low"),
            (Priority::High, "high"),
            (Priority::Normal, "normal 3"),
        ];
        for (priority, name) in order {
            listeners.push(priority, move |e: &mut PlayerDuplicateLogin| {
                let mut reason = e.disconnect_reason().to_owned();
                reason.push_str(", ");
                reason.push_str(name);
                e.set_disconnect_reason(reason);
            });
        }
        listeners.sort();
        let mut event = PlayerDuplicateLogin::new(identity("a"), identity("A"));
        event.set_disconnect_reason("start");
        listeners.duplicate_login().call(&mut event);
        assert_eq!(
            event.disconnect_reason(),
            "start, lowest, low, normal 1, normal 2, normal 3, high, highest"
        );
    }

    #[test]
    fn the_outcome_uses_the_screen_message_before_the_reason() {
        let event = PlayerDuplicateLogin::new(identity("a"), identity("A"));
        let (connecting, outcome) = event.clone().into_outcome();
        assert_eq!(connecting, identity("a"));
        assert_eq!(outcome, DuplicateOutcome::unheard());
        let mut event = event;
        event.set_disconnect_reason("reason");
        assert_eq!(event.into_outcome().1.message, "reason");
        let mut event = PlayerDuplicateLogin::new(identity("a"), identity("A"));
        event.set_disconnect_reason("reason");
        event.set_disconnect_screen_message(Some("screen".to_owned()));
        event.cancel();
        let outcome = event.into_outcome().1;
        assert!(outcome.cancelled);
        assert_eq!(outcome.message, "screen");
    }

    #[test]
    fn the_listener_holder_prints_counts() {
        let mut listeners = Listeners::default();
        listeners.push(Priority::Normal, |_: &mut PlayerDuplicateLogin| {});
        assert_eq!(
            format!("{listeners:?}"),
            "Listeners { duplicate_login: List { listeners: 1 } }"
        );
    }
}
