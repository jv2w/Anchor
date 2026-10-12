//! When sessions send with `send-release = "after-pass"`: the way the official server's RakNet
//! update thread does.
//!
//! One update cycle there sends everything the server thread queued for every connection, and
//! the thread runs one whenever a datagram arrives from any connection, and otherwise when its
//! 10 ms wait times out, which at the default Windows timer resolution ends at the first
//! 15.625 ms timer tick at least 10 ms after the wait began: the ticks that also end the server
//! passes. The server thread queues a pass's output once the pass's inputs have taken effect, so
//! a timeout at a pass's own end does not send that pass yet.
//!
//! The tick tags each output with the input pass it belongs to and reports each pass whose output
//! it delivered ([`Release::finish`]); [`Release`] counts the passes the sessions may send, and
//! each session holds its output until then ([`Held`]).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::watch;

use crate::clock;
use crate::tick::queue::{Message, OUTBOUND_CAPACITY};

/// The passes the sessions may send, shared by the tick, the transports and the sessions.
#[derive(Debug)]
pub(crate) struct Release {
    passes: clock::Passes,
    /// `send-release-wait-us`.
    wait_us: u64,
    /// How many passes have their output delivered by the tick.
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: the tick thread reports without waiting; the count is no game state"
    )]
    finished: std::sync::atomic::AtomicU64,
    /// When a datagram or a timeout last released, in microseconds from the epoch.
    #[expect(
        clippy::disallowed_types,
        reason = "off-tick: the transports' tasks and the timeout task share it without waiting"
    )]
    last_wake_us: std::sync::atomic::AtomicU64,
    /// How many passes the sessions may send; it only grows.
    released: watch::Sender<u64>,
}

impl Release {
    /// Releases for `passes`, waiting `wait_us` for a datagram, from now on.
    #[expect(clippy::disallowed_types, reason = "off-tick: makes the counts above")]
    pub(crate) fn new(passes: clock::Passes, wait_us: u32) -> Self {
        Self {
            passes,
            wait_us: u64::from(wait_us),
            finished: std::sync::atomic::AtomicU64::new(0),
            last_wake_us: std::sync::atomic::AtomicU64::new(passes.micros(clock::now())),
            released: watch::Sender::new(0),
        }
    }

    /// The tick delivered the output of every pass before pass `count`.
    pub(crate) fn finish(&self, count: u64) {
        self.finished.fetch_max(count, Ordering::AcqRel);
    }

    /// The count of passes the sessions may send, from now on.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.released.subscribe()
    }

    /// A datagram arrived on a connection: every finished pass may be sent, and the wait starts
    /// over.
    pub(crate) fn datagram(&self) {
        let now = self.passes.micros(clock::now());
        self.last_wake_us.fetch_max(now, Ordering::AcqRel);
        self.release(u64::MAX);
    }

    /// Lets the sessions send the finished passes before pass `up_to`.
    fn release(&self, up_to: u64) {
        let count = self.finished.load(Ordering::Acquire).min(up_to);
        self.released.send_if_modified(|released| {
            let more = count > *released;
            if more {
                *released = count;
            }
            more
        });
    }

    /// When the wait begun at `woke_us` times out: `wait_us` later, at the next pass end.
    fn timeout(&self, woke_us: u64) -> u64 {
        self.passes.end_from(woke_us.saturating_add(self.wait_us))
    }

    /// The timeouts: at each, unless a datagram restarted the wait, the passes that ended before
    /// it may be sent. Runs until the networking stops it.
    pub(crate) async fn run(self: Arc<Self>) {
        loop {
            let due = self.timeout(self.last_wake_us.load(Ordering::Acquire));
            let at = tokio::time::Instant::from_std(self.passes.at(due));
            tokio::time::sleep_until(at).await;
            self.time_out(due);
        }
    }

    /// The wait that was due at `due` (microseconds from the epoch) ended, if no datagram has
    /// moved it past `due` since.
    fn time_out(&self, due: u64) {
        if self.timeout(self.last_wake_us.load(Ordering::Acquire)) <= due {
            self.last_wake_us.fetch_max(due, Ordering::AcqRel);
            self.release(self.passes.ended_before(due));
        }
    }
}

/// A session's output waiting for its pass to be released, in the order the tick sent it: at
/// most [`OUTBOUND_CAPACITY`] sends.
#[derive(Debug, Default)]
pub(super) struct Held(VecDeque<(u64, Vec<Message>)>);

impl Held {
    /// Room for every send the tick's queue holds.
    pub(super) fn new() -> Self {
        Self(VecDeque::with_capacity(OUTBOUND_CAPACITY))
    }

    /// Whether it holds as many sends as it may.
    pub(super) fn is_full(&self) -> bool {
        self.0.len() >= OUTBOUND_CAPACITY
    }

    /// Holds `messages`, the output of pass `pass`.
    pub(super) fn push(&mut self, pass: u64, messages: Vec<Message>) {
        self.0.push_back((pass, messages));
    }

    /// Takes the messages of the passes before `released`, in order, stopping at the first of a
    /// later pass: nothing leaves before what was queued ahead of it.
    pub(super) fn take(&mut self, released: u64) -> Vec<Message> {
        let mut out = Vec::new();
        while let Some((_, mut messages)) = self.0.pop_front_if(|(pass, _)| *pass < released) {
            out.append(&mut messages);
        }
        out
    }

    /// Takes every message held, in order.
    pub(super) fn take_all(&mut self) -> Vec<Message> {
        self.take(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::time::Duration;

    use super::*;
    use crate::protocol::packet::{Packet, Text};

    const PERIOD_US: u64 = 15_625;

    fn release() -> Release {
        let passes = clock::Passes::new(clock::now(), NonZeroU32::new(15_625).unwrap());
        Release::new(passes, 10_000)
    }

    /// The microseconds of pass end `n`.
    fn tick(n: u64) -> u64 {
        n * PERIOD_US
    }

    /// The wait of the update thread, `WaitOnEvent(10)`, ends at the first 15.625 ms timer tick
    /// 10 ms or more after it began, and a timeout at a pass's own end does not send that pass.
    #[test]
    fn the_wait_ends_at_a_pass_end_and_releases_the_passes_ended_before() {
        let r = release();
        assert_eq!(r.timeout(tick(3)), tick(4));
        assert_eq!(r.timeout(tick(3) + 5_000), tick(4));
        assert_eq!(r.timeout(tick(3) + 6_000), tick(5));
        assert_eq!(r.passes.ended_before(tick(5)), 4, "pass 4 ends at tick 5");
        assert_eq!(r.passes.ended_before(tick(5) + 1), 5);

        let mut released = r.subscribe();
        r.finish(10);
        r.last_wake_us.store(tick(3), Ordering::Release);
        r.time_out(tick(5));
        assert_eq!(*released.borrow_and_update(), 4);
        assert_eq!(r.last_wake_us.load(Ordering::Acquire), tick(5));
        // A datagram restarted the wait: the timeout due before it sends nothing.
        r.last_wake_us.store(tick(5) + 6_000, Ordering::Release);
        r.time_out(tick(6));
        assert!(!released.has_changed().unwrap());
    }

    /// A datagram sends every finished pass at once; the count only grows.
    #[test]
    fn a_datagram_releases_every_finished_pass() {
        let r = release();
        let mut released = r.subscribe();
        r.finish(3);
        r.datagram();
        assert_eq!(*released.borrow_and_update(), 3);
        r.finish(2);
        r.datagram();
        assert!(!released.has_changed().unwrap());
        r.finish(5);
        r.last_wake_us.store(0, Ordering::Release);
        r.time_out(tick(2));
        assert!(!released.has_changed().unwrap(), "never back");
        r.datagram();
        assert_eq!(*released.borrow_and_update(), 5);
    }

    /// The timeouts run on their own: without a datagram, a finished pass goes at the first pass
    /// end at least the wait after the last release.
    #[test]
    fn the_timeouts_release_without_datagrams() {
        let passes = clock::Passes::new(clock::now(), NonZeroU32::new(1_000).unwrap());
        let r = Arc::new(Release::new(passes, 2_000));
        r.finish(1);
        let mut released = r.subscribe();
        crate::raknet::tests::run(async {
            let task = tokio::spawn(Arc::clone(&r).run());
            tokio::time::timeout(Duration::from_secs(5), released.changed())
                .await
                .unwrap()
                .unwrap();
            task.abort();
        });
        assert_eq!(*released.borrow(), 1);
    }

    fn text(message: &str) -> Message {
        Message::Packet(Packet::Text(Box::new(Text {
            message: message.to_owned(),
            ..Text::default()
        })))
    }

    fn texts(messages: &[Message]) -> Vec<&str> {
        messages
            .iter()
            .map(|m| match m {
                Message::Packet(Packet::Text(t)) => t.message.as_str(),
                other => panic!("not a text: {other:?}"),
            })
            .collect()
    }

    /// A session sends the held output of the released passes in the order the tick sent it,
    /// and nothing queued after output of a later pass.
    #[test]
    fn held_output_leaves_in_order_up_to_the_released_pass() {
        let mut held = Held::new();
        held.push(3, vec![text("a"), text("b")]);
        held.push(4, vec![text("c")]);
        held.push(3, vec![text("d")]);
        assert!(held.take(3).is_empty());
        assert_eq!(texts(&held.take(4)), ["a", "b"]);
        assert_eq!(texts(&held.take(5)), ["c", "d"]);
        held.push(9, vec![text("e")]);
        assert_eq!(texts(&held.take_all()), ["e"]);
        for _ in 0..OUTBOUND_CAPACITY {
            assert!(!held.is_full());
            held.push(0, Vec::new());
        }
        assert!(held.is_full());
    }
}
