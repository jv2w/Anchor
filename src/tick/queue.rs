//! The queues between sessions and the tick: the input queue, its per-session stamping sender,
//! and the outbound queue of each session.

use std::fmt;

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::protocol::Uuid;
use crate::protocol::packet::Packet;

/// Inputs the tick queue holds (PLAN.md 10.2). A session that finds it full disconnects.
pub(crate) const INPUT_CAPACITY: usize = 65_536;
/// Messages a session's outbound queue holds (PLAN.md 10.2). The tick drops a session whose queue
/// is full instead of waiting.
const OUTBOUND_CAPACITY: usize = 1_024;

/// A session: one connection, numbered by the session layer in connection order and never
/// reused while the process runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SessionId(u64);

impl SessionId {
    /// The session numbered `id`.
    pub(crate) const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The number.
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

/// Who a session's player is, from the verified login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    /// The display name (Xbox gamertag online).
    pub(crate) name: String,
    /// The Xbox user ID; `None` offline.
    pub(crate) xuid: Option<u64>,
    /// The identity UUID.
    pub(crate) uuid: Uuid,
}

/// One input for the tick, ordered by (`session`, `seq`) before it is applied.
#[derive(Debug, Clone)]
pub(crate) struct Input {
    /// The session it came from.
    pub(crate) session: SessionId,
    /// Its position among the session's inputs, from 0.
    pub(crate) seq: u64,
    /// What happened.
    pub(crate) kind: InputKind,
}

/// What a session reports to the tick.
#[derive(Debug, Clone)]
pub(crate) enum InputKind {
    /// The session finished the resource pack sequence and wants to play.
    Join {
        /// The verified player.
        identity: Identity,
        /// Where the tick sends this session's packets (from [`outbound_queue`]).
        outbound: mpsc::Sender<Outbound>,
    },
    /// An in-game packet (after `StartGame`).
    Packet(Packet),
    /// The connection closed.
    Leave,
}

/// A message from the tick to one session.
#[derive(Debug)]
pub(crate) enum Outbound {
    /// Packets to send, in order.
    Packets(Vec<Packet>),
    /// The last packet to send (a `Disconnect` or a failed `PlayStatus`) before closing the
    /// connection. The tick has already forgotten the session.
    Close(Packet),
}

/// Why an input was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    /// The queue is full; the session should disconnect.
    Full,
    /// The tick has stopped.
    Closed,
}

/// A result with the queue [`enum@Error`].
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Full => "the tick input queue is full",
            Self::Closed => "the tick has stopped",
        })
    }
}

impl std::error::Error for Error {}

/// The tick input queue: the sending side for the session layer and the tick's receiver.
pub(crate) fn input_queue() -> (InputSender, mpsc::Receiver<Input>) {
    let (tx, rx) = mpsc::channel(INPUT_CAPACITY);
    (InputSender(tx), rx)
}

/// A session's outbound queue; the sender goes to the tick in [`InputKind::Join`].
pub(crate) fn outbound_queue() -> (mpsc::Sender<Outbound>, mpsc::Receiver<Outbound>) {
    mpsc::channel(OUTBOUND_CAPACITY)
}

/// The sending side of the tick input queue, shared by every session.
#[derive(Debug, Clone)]
pub(crate) struct InputSender(mpsc::Sender<Input>);

impl InputSender {
    /// A sender that stamps the inputs of session `id` with its sequence numbers.
    pub(crate) fn session(&self, id: SessionId) -> SessionInputs {
        SessionInputs {
            session: id,
            next_seq: 0,
            queue: self.0.clone(),
        }
    }
}

/// The inputs of one session, stamped in order. Owned by that session's task.
#[derive(Debug)]
pub(crate) struct SessionInputs {
    session: SessionId,
    next_seq: u64,
    queue: mpsc::Sender<Input>,
}

impl SessionInputs {
    /// Queues `kind` without waiting. On [`Error::Full`] the session should disconnect; it may
    /// still call [`leave`](Self::leave).
    pub(crate) fn try_send(&mut self, kind: InputKind) -> Result<()> {
        let input = Input {
            session: self.session,
            seq: self.next_seq,
            kind,
        };
        match self.queue.try_send(input) {
            Ok(()) => {
                // 2^64 inputs never arrive, so the order never wraps.
                self.next_seq = self.next_seq.saturating_add(1);
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(Error::Full),
            Err(TrySendError::Closed(_)) => Err(Error::Closed),
        }
    }

    /// Queues [`InputKind::Leave`], waiting for room so that the tick always learns the session
    /// ended. Returns at once if the tick has stopped.
    pub(crate) async fn leave(self) {
        let input = Input {
            session: self.session,
            seq: self.next_seq,
            kind: InputKind::Leave,
        };
        // A closed queue means the tick has stopped and forgets every session anyway.
        if self.queue.send(input).await.is_err() {
            tracing::debug!(session = self.session.get(), "tick stopped before leave");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_are_stamped_in_order_per_session() {
        let (sender, mut rx) = input_queue();
        let mut a = sender.session(SessionId::new(7));
        let mut b = sender.session(SessionId::new(3));
        a.try_send(InputKind::Leave).unwrap();
        b.try_send(InputKind::Leave).unwrap();
        a.try_send(InputKind::Leave).unwrap();
        let got: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|i| (i.session.get(), i.seq))
            .collect();
        assert_eq!(got, [(7, 0), (3, 0), (7, 1)]);
    }

    #[test]
    fn a_full_queue_and_a_stopped_tick_are_reported() {
        let (tx, rx) = mpsc::channel(1);
        let mut s = InputSender(tx).session(SessionId::new(1));
        s.try_send(InputKind::Leave).unwrap();
        assert_eq!(s.try_send(InputKind::Leave), Err(Error::Full));
        drop(rx);
        assert_eq!(s.try_send(InputKind::Leave), Err(Error::Closed));
    }
}
