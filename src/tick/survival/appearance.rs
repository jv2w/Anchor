//! Emotes and skin changes in game, with their events.

use std::ops::Bound;
use std::sync::Arc;

use tracing::debug;

use super::{Phase, Player, SessionId, State, broadcast_to_viewers};
use crate::entity::MAX_SKIN_BYTES;
use crate::event::{PlayerChangeSkin, PlayerEmote};
use crate::player::Id;
use crate::protocol;
use crate::protocol::consts::EmoteFlag;
use crate::protocol::packet::{Emote, Packet, PlayerSkin};
use crate::tick::Context;
use crate::tick::queue::{SharedPacket, Skin};
use crate::tick::{checkpoint, spawn};

/// Ticks that must pass between two emotes of a player: more than 5.
const EMOTE_COOLDOWN_TICKS: u64 = 5;
/// The longest emote ID passed on: an emote ID is a UUID (36 bytes); more is not an emote.
const MAX_EMOTE_ID_BYTES: usize = 128;
/// Ticks between two skin changes of a player: one a second, as each change sends a skin of up
/// to 4 MiB to every player.
const SKIN_COOLDOWN_TICKS: u64 = Context::TICKS_PER_SECOND as u64;
/// The skin bytes the server's skin budget ([`SkinBudget`]) gains each tick: 128 KiB a second,
/// about 1 Mbit/s. Every player but a skin's owner gets every broadcast, so this is the most
/// other players' skin changes add to each player's link however many players change skins: well
/// below a slow mobile link, next to the game's own traffic. A receiver that drains only 64 KiB/s
/// (the floor the spawn deadline allows) falls behind by skins alone by one full bucket (4 MiB)
/// and then at most 64 KiB/s. A session counts every broadcast by its bytes against 32 MiB
/// (`MAX_PENDING_BYTES` in `session/connection.rs`) as soon as it is queued, so from an empty
/// queue skins alone take over 7 minutes of nonstop changes to reach it. Behind a spawn window's
/// player list that is still going out nothing queued after it drains, so skins alone at this
/// rate reach the bound after about 4 minutes (the trade-off in `.claude/docs/network.md`). The
/// skins sent back to a player's own client ([`send_owed_skin`]), each in a player list entry a
/// little larger than the skin, come at about this rate too, and only after the server set its
/// skin or refused or replaced its own change: a client that keeps asking for refused skins about
/// doubles its own skin traffic and so reaches the bound in about half that time.
pub(super) const SKIN_REFILL_BYTES_PER_TICK: usize =
    (128 << 10) / Context::TICKS_PER_SECOND as usize;

/// The server's budget for skin broadcasts: a token bucket of skin bytes that holds one skin of
/// the largest size and refills by [`SKIN_REFILL_BYTES_PER_TICK`]. Each broadcast is held once
/// for all its receivers
/// ([`Player::send_shared`]) and encoded and compressed once for all their sessions, but every
/// receiver's link carries it. A change that does not fit stays marked, and only the player's
/// latest skin goes out when it does, so a player that changes fast costs one waiting broadcast.
///
/// [`Player::send_shared`]: crate::tick::Player::send_shared
#[derive(Debug)]
pub(in crate::tick) struct SkinBudget {
    /// The skin bytes that may go out at tick `at`, at most [`MAX_SKIN_BYTES`].
    bytes: usize,
    /// The tick `bytes` was counted at; the bucket refills by the ticks since.
    at: u64,
    /// The session whose waiting skin goes first: the one that did not fit last. Skins go out in
    /// session order from it and round to it, so with a spent budget every waiting player gets
    /// its turn instead of the lowest sessions first.
    turn: SessionId,
}

impl SkinBudget {
    /// Hashes every field (state hash).
    #[cfg(test)]
    pub(in crate::tick) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self { bytes, at, turn } = self;
        h.usize(*bytes);
        h.u64(*at);
        h.u64(turn.get());
    }

    /// Adds what the ticks from `at` to `tick` refill, up to one skin of the largest size.
    fn refill(&mut self, tick: u64) {
        let ticks = usize::try_from(tick.saturating_sub(self.at)).unwrap_or(usize::MAX);
        self.bytes = self
            .bytes
            .saturating_add(ticks.saturating_mul(SKIN_REFILL_BYTES_PER_TICK))
            .min(MAX_SKIN_BYTES);
        self.at = tick;
    }
}

impl Default for SkinBudget {
    fn default() -> Self {
        Self {
            bytes: MAX_SKIN_BYTES,
            at: 0,
            turn: SessionId::new(0),
        }
    }
}

checkpoint::fields!(SkinBudget { bytes, at, turn });

/// When a player last emoted and changed its skin, the skin it last asked for, and when its own
/// client may be sent its skin again.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Appearance {
    /// The tick of the last emote shown.
    emote: Option<u64>,
    /// The tick of the last skin change asked for.
    skin: Option<u64>,
    /// The full ID of the skin last asked for or sent to the player's own client: the client
    /// sends back a skin the server sent it, which must not count as a change.
    skin_id: Option<String>,
    /// The first tick the player's own client may be sent its skin again ([`send_owed_skin`]).
    own_skin_at: u64,
}

impl Appearance {
    /// Hashes every field (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            emote,
            skin,
            skin_id,
            own_skin_at,
        } = self;
        for t in [emote, skin] {
            h.u64(t.unwrap_or(u64::MAX));
        }
        match skin_id {
            Some(id) => h.bytes(id.as_bytes()),
            None => h.u64(u64::MAX),
        }
        h.u64(*own_skin_at);
    }
}

checkpoint::fields!(Appearance {
    emote,
    skin,
    skin_id,
    own_skin_at,
});

/// Plays the emote of an `Emote` packet; see [`super::on_emote`].
pub(super) fn on_emote(state: &mut State, session: SessionId, packet: &Emote) {
    let tick = state.tick;
    let ready = |state: &State| {
        state.players.get(&session).is_some_and(|p| {
            p.phase == Phase::Spawned
                && p.survival
                    .appearance
                    .emote
                    .is_none_or(|last| tick.saturating_sub(last) > EMOTE_COOLDOWN_TICKS)
        })
    };
    if !ready(state) || !valid_emote(&packet.emote_id) {
        return;
    }
    if let Some(player) = state.players.get_mut(&session) {
        player.survival.appearance.emote = Some(tick);
    }
    let Ok(event) = super::ask(
        state,
        || PlayerEmote {
            player: Id::new(session),
            emote_id: packet.emote_id.clone(),
            cancelled: false,
        },
        PlayerEmote::is_cancelled,
        // A listener that sets an emote ID `valid_emote` refuses cancels the emote.
        |state, e| state.players.contains_key(&session) && valid_emote(&e.emote_id),
    ) else {
        return;
    };
    let Some(player) = state.players.get(&session) else {
        return;
    };
    let emote_id = event
        .as_ref()
        .map_or_else(|| packet.emote_id.clone(), |e| e.emote_id.clone());
    let packet = emote_packet(player.runtime_id, emote_id);
    broadcast_to_viewers(state, session, &packet);
    super::watch(state, event.as_ref());
}

/// The `Emote` packet that shows the player with `runtime_id` playing `emote_id`.
pub(super) fn emote_packet(runtime_id: u64, emote_id: String) -> Packet {
    Packet::Emote(Emote {
        entity_runtime_id: runtime_id,
        // The client ignores it, and a value from a client must not be passed on.
        emote_length: 0,
        emote_id,
        xuid: String::new(),
        platform_id: String::new(),
        flags: u8::from(EmoteFlag::ServerSide) | u8::from(EmoteFlag::MuteChat),
    })
}

/// Whether `id` may be an emote ID: not empty and at most [`MAX_EMOTE_ID_BYTES`] bytes.
pub(super) fn valid_emote(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_EMOTE_ID_BYTES
}

/// Changes the skin of a `PlayerSkin` packet's player; see [`super::on_skin`].
pub(super) fn on_skin(state: &mut State, session: SessionId, packet: &PlayerSkin) {
    let tick = state.tick;
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    if player.phase != Phase::Spawned {
        return;
    }
    let a = &mut player.survival.appearance;
    if a.skin_id.as_deref() == Some(packet.skin.full_id.as_str()) {
        debug!(session = session.get(), "the skin sent back ignored");
        return;
    }
    // The client draws the skin it asked for, refused or not: a refused one is owed the player's.
    if a.skin
        .is_some_and(|last| tick.saturating_sub(last) < SKIN_COOLDOWN_TICKS)
    {
        debug!(session = session.get(), "skin changes too fast; refused");
        player.survival.custom.own_skin = true;
        return;
    }
    let new = Skin::new(Arc::clone(&packet.skin));
    if !new.is_acceptable() {
        debug!(session = session.get(), "invalid skin refused");
        player.survival.custom.own_skin = true;
        return;
    }
    a.skin = Some(tick);
    a.skin_id = Some(packet.skin.full_id.clone());
    let old = player.identity.skin.clone();
    let event = super::ask(
        state,
        || PlayerChangeSkin {
            player: Id::new(session),
            old_skin: old.clone(),
            new_skin: new.clone(),
            cancelled: false,
        },
        PlayerChangeSkin::is_cancelled,
        // A skin a listener set through the API in between is the player's now: a change that
        // went on would overwrite it.
        |state, e| {
            state
                .players
                .get(&session)
                .is_some_and(|p| Arc::ptr_eq(p.identity.skin.shared(), old.shared()))
                && e.new_skin.is_acceptable()
        },
    );
    let Ok(event) = event else {
        if let Some(player) = state.players.get_mut(&session) {
            player.survival.custom.own_skin = true;
        }
        return;
    };
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    // A listener's skin is not the one the client draws.
    let replaced = event
        .as_ref()
        .filter(|e| !Arc::ptr_eq(e.new_skin.shared(), new.shared()));
    player.survival.custom.own_skin = replaced.is_some();
    player.identity.skin = replaced.map_or(new, |e| e.new_skin.clone());
    // Broadcast at the end of the tick within the server's skin budget (`send_skins`).
    player.survival.custom.skin = true;
    super::watch(state, event.as_ref());
}

/// Sends the player's own client the skin it is owed, as its own list entry
/// ([`spawn::own_list`]), once the last one sent so is paid for at [`SKIN_REFILL_BYTES_PER_TICK`];
/// until then the mark stays. The client sending it back is no change.
pub(super) fn send_owed_skin(player: &mut Player, tick: u64) {
    let a = &mut player.survival.appearance;
    if tick < a.own_skin_at {
        return;
    }
    let skin = &player.identity.skin;
    let ticks = skin.size().div_ceil(SKIN_REFILL_BYTES_PER_TICK);
    a.own_skin_at = tick.saturating_add(u64::try_from(ticks).unwrap_or(u64::MAX));
    a.skin_id = Some(skin.get().full_id.clone());
    player.survival.custom.own_skin = false;
    let packet = spawn::own_list(player);
    player.send_packet(packet);
}

/// Broadcasts the changed skins of the spawned players while they fit in the server's skin
/// budget ([`SkinBudget`]), in session order from the budget's turn and round to it. The first
/// that does not fit, and every one after it, waits for a later tick, and its player has the
/// next turn.
pub(in crate::tick) fn send_skins(state: &mut State) {
    state.skin_budget.refill(state.tick);
    let turn = state.skin_budget.turn;
    for (start, end) in [
        (Bound::Included(turn), Bound::Unbounded),
        (Bound::Unbounded, Bound::Excluded(turn)),
    ] {
        let mut after = start;
        while let Some(session) = state
            .players
            .range((after, end))
            .find(|(_, p)| p.phase == Phase::Spawned && skin_waits(p))
            .map(|(&s, _)| s)
        {
            after = Bound::Excluded(session);
            if !send_skin(state, session) {
                state.skin_budget.turn = session;
                return;
            }
        }
    }
}

/// Whether the player's skin changed and waits to be broadcast ([`send_skins`]).
pub(in crate::tick) fn skin_waits(player: &Player) -> bool {
    player.survival.custom.skin
}

/// Sends the skin of the player of `session` to every other player, one copy shared by all of
/// them, and clears its mark, if it fits in the skin budget; false, sending nothing, if it does
/// not. The player's own client is sent its skin by [`send_owed_skin`]: a `PlayerSkin` of its own
/// player leaves it drawing the skin it has (`xtask/data/p6/facts.md`).
fn send_skin(state: &mut State, session: SessionId) -> bool {
    let Some(player) = state.players.get_mut(&session) else {
        return true;
    };
    let Some(left) = state
        .skin_budget
        .bytes
        .checked_sub(player.identity.skin.size())
    else {
        return false;
    };
    state.skin_budget.bytes = left;
    player.survival.custom.skin = false;
    let packet = Arc::new(SharedPacket::new(skin_packet(
        player.identity.uuid,
        &player.identity.skin,
    )));
    for (&s, p) in &mut state.players {
        if s != session && p.gets_broadcasts() {
            p.send_shared(Arc::clone(&packet));
        }
    }
    true
}

/// The `PlayerSkin` packet that shows `skin` on the player with `uuid`.
fn skin_packet(uuid: protocol::Uuid, skin: &Skin) -> Packet {
    Packet::PlayerSkin(PlayerSkin {
        uuid,
        skin: Arc::clone(skin.shared()),
        new_skin_name: String::new(),
        old_skin_name: String::new(),
    })
}
