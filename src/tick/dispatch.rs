//! Raising events from the rule code, and the deferred work that runs after a callback.
//!
//! A rule raises a cancellable event like this:
//!
//! ```text
//! let plan = check(state, ..)?;                 // may the action happen?
//! if dispatch::heard::<E>(state) {
//!     let mut event = E { .., cancelled: false };
//!     dispatch::call(state, &mut event);
//!     if event.cancelled { return refuse(..) }
//!     if dispatch::listened::<E>(state) { plan = check(state, ..)?; } // listeners may have
//!                                               // changed the game: check again
//! }
//! apply(state, plan /* with the event's changes */);
//! dispatch::watch(state, &event);               // monitors, only if the event was built
//! ```
//!
//! Listeners get the whole state through a [`Context`], so the listener list of the event is
//! taken out of the state while it runs. No listener can raise an event (direct API calls raise
//! none and [`Context::perform`] defers), so the list is never needed while it is out.

use std::collections::VecDeque;
use std::fmt;

use tracing::warn;

use super::{Context, State};
use crate::action::{self, Action};
use crate::event::Event;

/// The most callbacks waiting in the deferred queue.
pub(crate) const MAX_DEFERRED: usize = 4096;
/// The most deferred callbacks one tick runs; a callback that keeps deferring more cannot hold
/// the tick, and the rest run in the next tick.
const MAX_DEFERRED_RUNS_PER_TICK: u32 = 16_384;

/// Whether anyone listens to or monitors `E`; when not, the rule builds no event.
pub(crate) fn heard<E: Event>(state: &State) -> bool {
    !E::list_ref(&state.listeners).is_empty()
}

/// Whether a listener (not only a monitor) is registered for `E`: only then can the game have
/// changed during [`call`], and only then must the rule check again.
pub(crate) fn listened<E: Event>(state: &State) -> bool {
    E::list_ref(&state.listeners).has_listeners()
}

/// Calls the listeners of `E` on `event`, in priority order, each with a [`Context`] on the whole
/// state.
pub(crate) fn call<E: Event>(state: &mut State, event: &mut E) {
    let mut list = std::mem::take(E::list(&mut state.listeners));
    list.call(event, &mut Context { state });
    *E::list(&mut state.listeners) = list;
}

/// Calls the monitors of `E` on the applied `event`, with shared access only.
pub(crate) fn watch<E: Event>(state: &mut State, event: &E) {
    let mut list = std::mem::take(E::list(&mut state.listeners));
    list.watch(event, &Context { state });
    *E::list(&mut state.listeners) = list;
}

/// A callback waiting to run after the current one.
type Callback = Box<dyn FnOnce(&mut Context<'_>) + Send>;

/// Callbacks queued with [`Context::defer`] and [`Context::perform`], in queueing order.
#[derive(Default)]
pub(crate) struct Deferred(VecDeque<Callback>);

impl Deferred {
    /// How many callbacks wait.
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
}

impl fmt::Debug for Deferred {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deferred")
            .field("len", &self.0.len())
            .finish()
    }
}

impl Context<'_> {
    /// Runs `callback` after the callback that is running now has returned, in the same tick and
    /// in queueing order. Use it from a listener for work that should follow the event.
    ///
    /// Fails, queueing nothing, while 4,096 callbacks wait.
    pub fn defer(
        &mut self,
        callback: impl FnOnce(&mut Context<'_>) + Send + 'static,
    ) -> action::Result<()> {
        let queue = &mut self.state.deferred.0;
        if queue.len() >= MAX_DEFERRED {
            return Err(action::Error::Full);
        }
        queue.push_back(Box::new(callback));
        Ok(())
    }

    /// Does `action` the way a player or the game would, through the game rules and with its
    /// events, after the running callback has returned ([`defer`](Self::defer)). Direct calls
    /// such as [`Player::teleport`](crate::player::Player) raise no events; this does.
    ///
    /// Fails, queueing nothing, while 4,096 callbacks wait.
    pub fn perform(&mut self, action: impl Action) -> action::Result<()> {
        self.defer(move |cx| action::Sealed::perform(action, cx))
    }
}

/// Runs the deferred callbacks, including those they queue, up to the tick's limit.
pub(super) fn run_deferred(state: &mut State, runs: &mut u32) {
    while *runs < MAX_DEFERRED_RUNS_PER_TICK {
        let Some(callback) = state.deferred.0.pop_front() else {
            return;
        };
        *runs = runs.saturating_add(1);
        callback(&mut Context { state });
    }
    if !state.deferred.0.is_empty() {
        warn!(
            waiting = state.deferred.0.len(),
            "deferred callbacks keep queueing more; the rest run next tick"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{new_state, settings};
    use super::*;
    use crate::event::{Listeners, PlayerDuplicateLogin, Priority};
    use crate::tick::queue::{Identity, Skin};

    fn identity(name: &str) -> Identity {
        Identity {
            name: name.to_owned(),
            xuid: None,
            uuid: crate::protocol::Uuid::from_bytes([1; 16]),
            skin: Skin::default(),
            device_os: -1,
            address: None,
            client: crate::tick::queue::Client::default(),
        }
    }

    // pmmp: EventPriority::LOWEST, EventPriority::LOW, EventPriority::NORMAL, EventPriority::HIGH, EventPriority::HIGHEST
    #[test]
    fn listeners_run_by_priority_then_in_registration_order_and_monitors_last() {
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
            listeners.push(
                priority,
                move |e: &mut PlayerDuplicateLogin, _: &mut Context<'_>| {
                    let mut reason = e.disconnect_reason().to_owned();
                    reason.push_str(", ");
                    reason.push_str(name);
                    e.set_disconnect_reason(reason);
                },
            );
        }
        listeners.push_monitor(|e: &PlayerDuplicateLogin, cx: &Context<'_>| {
            assert!(e.disconnect_reason().ends_with("highest"));
            assert_eq!(cx.tick(), 0);
        });
        let mut state = new_state(settings(|_| {}), listeners);
        assert!(heard::<PlayerDuplicateLogin>(&state));
        assert!(listened::<PlayerDuplicateLogin>(&state));
        let mut event = PlayerDuplicateLogin::new(identity("a"), identity("A"));
        event.set_disconnect_reason("start");
        call(&mut state, &mut event);
        watch(&mut state, &event);
        assert_eq!(
            event.disconnect_reason(),
            "start, lowest, low, normal 1, normal 2, normal 3, high, highest"
        );
        assert_eq!(
            format!("{:?}", state.listeners),
            "Listeners { duplicate_login: List { listeners: 7, monitors: 1 } }"
        );
    }

    #[test]
    fn an_unheard_event_is_not_built() {
        let state = new_state(settings(|_| {}), Listeners::default());
        assert!(!heard::<PlayerDuplicateLogin>(&state));
        assert!(!listened::<PlayerDuplicateLogin>(&state));
    }

    #[test]
    fn deferred_callbacks_run_in_order_after_the_caller_and_are_bounded() {
        let mut state = new_state(settings(|_| {}), Listeners::default());
        let mut cx = Context { state: &mut state };
        for i in 0..MAX_DEFERRED {
            let tick = u64::try_from(i).expect("small");
            cx.defer(move |cx| {
                assert_eq!(cx.state.tick, tick);
                cx.state.tick += 1;
            })
            .expect("room");
        }
        assert_eq!(cx.defer(|_| {}), Err(action::Error::Full));
        let mut runs = 0;
        run_deferred(&mut state, &mut runs);
        assert_eq!(state.tick, u64::try_from(MAX_DEFERRED).expect("small"));
        assert_eq!(state.deferred.len(), 0);
    }

    #[test]
    fn a_callback_that_keeps_deferring_stops_at_the_tick_limit() {
        fn again(cx: &mut Context<'_>) {
            cx.defer(again).expect("room");
        }
        let mut state = new_state(settings(|_| {}), Listeners::default());
        Context { state: &mut state }.defer(again).expect("room");
        let mut runs = 0;
        run_deferred(&mut state, &mut runs);
        assert_eq!(runs, MAX_DEFERRED_RUNS_PER_TICK);
        assert_eq!(state.deferred.len(), 1);
    }
}
