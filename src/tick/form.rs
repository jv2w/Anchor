//! Forms in the tick: the forms each player has open, their answers, and the per-player
//! scoreboard and boss bar.
//!
//! A form's handler is kept with the shape of the answer it waits for, under the form ID sent to
//! the player. The answer (`ModalFormResponse`) removes it whether it fits or not, so a second
//! answer to the same ID finds nothing; one that fits runs the handler in the tick's serial step,
//! with the handler already out of the state. A form stays open on the client through a death, a
//! world move and a newer form (they queue), so only the player's answer, leaving, or the server's
//! [`close_forms`](Context::close_forms) closes it, with one notice each for the first two.
//!
//! The form API is on [`Context`] rather than [`Player`]: the handler
//! needs a `Context`, and the forms are kept in the state beside the players.

mod display;

use std::collections::BTreeMap;
use std::fmt;

use tracing::debug;

use super::checkpoint::{self, Codec, Reader, Writer};
use super::queue::SessionId;
use super::{Context, Player, State};
use crate::form::{self, Error, Form, MAX_PENDING, MAX_RESPONSE_BYTES, Response, Shape};
use crate::player;
use crate::protocol::consts::ModalFormCancelReason;
use crate::protocol::packet::{ClientBoundCloseForm, ModalFormRequest, ModalFormResponse, Packet};

/// The open forms and displays of every player, by session.
#[derive(Debug, Default)]
pub(crate) struct Forms {
    players: BTreeMap<SessionId, Open>,
}

impl Forms {
    /// Hashes what a replay must reproduce (state hash): form IDs, waiting forms and what is shown.
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self { players } = self;
        h.usize(players.len());
        for (session, open) in players {
            let Open {
                next,
                pending,
                display,
            } = open;
            h.u64(session.get());
            h.u64(*next);
            h.usize(pending.len());
            for id in pending.keys() {
                h.u64(u64::from(*id));
            }
            display.hash_into(h);
        }
    }

    /// How many players have an entry and how many forms wait in all.
    #[cfg(test)]
    fn counts(&self) -> (usize, usize) {
        let pending = self.players.values().map(|o| o.pending.len()).sum();
        (self.players.len(), pending)
    }
}

checkpoint::fields!(Forms { players });

/// What one player has open.
#[derive(Debug, Default)]
struct Open {
    /// The ID of the next form; IDs at or past 2^32 are refused.
    next: u64,
    /// The forms waiting for an answer, by ID; at most [`MAX_PENDING`].
    pending: BTreeMap<u32, Pending>,
    /// The scoreboards and the boss bar last sent.
    display: display::Shown,
}

/// A waiting form's handler is a closure: it has no checkpoint form, so a waiting form makes the
/// checkpoint opaque, and reads back as none.
impl Codec for Open {
    fn encode(&self, w: &mut Writer) {
        let Self {
            next,
            pending,
            display,
        } = self;
        next.encode(w);
        w.opaque("forms waiting for an answer", pending.len());
        display.encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> checkpoint::Result<Self> {
        Ok(Self {
            next: Codec::decode(r)?,
            pending: BTreeMap::new(),
            display: Codec::decode(r)?,
        })
    }
}

/// What runs when a form is answered.
type Handler = Box<dyn FnOnce(&mut Context<'_>, player::Id, Response) + Send>;

/// A form waiting for its answer.
struct Pending {
    shape: Shape,
    handler: Handler,
}

impl fmt::Debug for Pending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pending")
            .field("shape", &self.shape)
            .finish_non_exhaustive()
    }
}

impl Context<'_> {
    /// Whether `player` has a form waiting for an answer. False if the player is not in the game.
    pub fn has_form_open(&self, player: player::Id) -> bool {
        self.state
            .forms
            .players
            .get(&player.session())
            .is_some_and(|open| !open.pending.is_empty())
    }

    /// Closes the form `player` sees and those queued behind it on its client. Their handlers
    /// never run, and answers that still arrive for them are ignored; the caller is how its own
    /// close is known. False if the player is not in the game.
    #[doc(alias = "Player::closeAllForms")]
    pub fn close_forms(&mut self, player: player::Id) -> bool {
        let Some((p, open)) = open(self.state, player) else {
            return false;
        };
        open.pending.clear();
        p.send_packet(Packet::ClientBoundCloseForm(ClientBoundCloseForm {}));
        true
    }

    /// Sends `form` to `player`; `handler` runs once, in a later tick, with the player's answer:
    /// what the player chose, [`Response::Closed`] if it closed the form, or [`Response::Busy`]
    /// if the client could not show it. An answer that does not fit the form is dropped, and the
    /// handler never runs. A player who leaves with the form open closes it: the handler runs with
    /// [`Response::Closed`], the player already gone from the game. A form the server closes
    /// ([`close_forms`](Self::close_forms)) is a direct call and raises no notice.
    ///
    /// # Errors
    ///
    /// Sends nothing with the first of these that holds:
    /// [`Error::Invalid`] for a form that breaks a rule of its builder or is too large.
    /// [`Error::Offline`] for a player who is not in the game.
    /// [`Error::Full`] for a player who has 16 forms waiting.
    /// [`Error::Exhausted`] for a player who was sent 2^32 forms already.
    #[doc(alias = "Player::sendForm")]
    #[doc(alias = "Form::handleResponse")]
    pub fn send_form(
        &mut self,
        player: player::Id,
        form: impl Into<Form>,
        handler: impl FnOnce(&mut Context<'_>, player::Id, Response) + Send + 'static,
    ) -> form::Result<()> {
        let (json, shape) = form.into().prepare()?;
        let (p, open) = open(self.state, player).ok_or(Error::Offline)?;
        if open.pending.len() >= MAX_PENDING {
            return Err(Error::Full);
        }
        let id = u32::try_from(open.next).map_err(|_| Error::Exhausted)?;
        open.next = open.next.saturating_add(1);
        open.pending.insert(
            id,
            Pending {
                shape,
                handler: Box::new(handler),
            },
        );
        p.send_packet(Packet::ModalFormRequest(ModalFormRequest {
            form_id: id,
            form_data: json,
        }));
        Ok(())
    }
}

/// Handles a `ModalFormResponse` of `session`: an answer to a form it was sent runs the form's
/// handler if it fits; anything else is dropped.
pub(super) fn on_response(state: &mut State, session: SessionId, packet: &ModalFormResponse) {
    let id = packet.form_id;
    let Some(pending) = state
        .forms
        .players
        .get_mut(&session)
        .and_then(|open| open.pending.remove(&id))
    else {
        debug!(
            session = session.get(),
            form = id,
            "answer to an unknown form ignored"
        );
        return;
    };
    let response = match (packet.cancel_reason, &packet.response_data) {
        (Some(reason), _) if reason == u8::from(ModalFormCancelReason::UserBusy) => {
            Some(Response::Busy)
        }
        (Some(_), _) => Some(Response::Closed),
        (None, Some(data)) if data.len() <= MAX_RESPONSE_BYTES => pending.shape.parse(data),
        (None, _) => None,
    };
    let Some(response) = response else {
        debug!(
            session = session.get(),
            form = id,
            len = packet.response_data.as_ref().map_or(0, Vec::len),
            "form answer that does not fit the form dropped"
        );
        return;
    };
    (pending.handler)(&mut Context { state }, player::Id::new(session), response);
}

/// Forgets what `session` had open when the player leaves; the handler of each form it still
/// waited for hears [`Response::Closed`] once, in form ID order, with the player already gone.
pub(super) fn left(state: &mut State, session: SessionId) {
    let Some(open) = state.forms.players.remove(&session) else {
        return;
    };
    for pending in open.pending.into_values() {
        (pending.handler)(
            &mut Context { state },
            player::Id::new(session),
            Response::Closed,
        );
    }
}

/// The player of `id` and its entry, if it is in the game ([`Context::player`]).
fn open(state: &mut State, id: player::Id) -> Option<(&mut Player, &mut Open)> {
    Context { state: &mut *state }.player(id)?;
    let session = id.session();
    let player = state.players.get_mut(&session)?;
    Some((player, state.forms.players.entry(session).or_default()))
}

#[cfg(test)]
mod tests;
