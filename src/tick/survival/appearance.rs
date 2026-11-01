//! Emotes and skin changes in game (PMMP `Player::emote`, `Human::emote`, `Player::changeSkin`,
//! `InGamePacketHandler::handleEmote`, `handlePlayerSkin`), with their events.

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

/// Ticks that must pass between two emotes of a player: more than 5 (PMMP `Player::emote`).
const EMOTE_COOLDOWN: u64 = 5;
/// The longest emote ID passed on: an emote ID is a UUID (36 bytes); more is not an emote.
const MAX_EMOTE_ID: usize = 128;
/// Ticks between two skin changes of a player: one a second. PMMP has no limit, and each change
/// sends a skin of up to 4 MiB to every player.
const SKIN_COOLDOWN: u64 = Context::TICKS_PER_SECOND as u64;
/// The skin bytes the server's skin budget ([`SkinBudget`]) gains each tick: 128 KiB a second,
/// about 1 Mbit/s. Every receiver gets every broadcast, so this is the most skin changes add to
/// each player's link however many players change skins: well below a slow mobile link, next to
/// the game's own traffic. A receiver that drains only 64 KiB/s (the floor the spawn deadline
/// allows) falls behind by skins alone by one full bucket (4 MiB) and then at most 64 KiB/s. A
/// session counts every broadcast by its bytes against 32 MiB (`MAX_PENDING_BYTES` in
/// `session/connection.rs`) as soon as it is queued, so from an empty queue skins alone take
/// over 7 minutes of nonstop changes to reach it. Behind a spawn window's player list that is
/// still going out nothing queued after it drains, so skins alone at this rate reach the bound
/// after about 4 minutes (the trade-off in `.claude/docs/network.md`).
const SKIN_REFILL_BYTES_PER_TICK: usize = (128 << 10) / Context::TICKS_PER_SECOND as usize;

/// The server's budget for skin broadcasts (PMMP sends every change at once): a token bucket of
/// skin bytes that holds one skin of the largest size and refills by
/// [`SKIN_REFILL_BYTES_PER_TICK`]. Each broadcast is held once for all its receivers
/// ([`Player::send_shared`]) and encoded and compressed once for all their sessions, but every
/// receiver's link carries it. A change that
/// does not fit stays marked, and only the player's latest skin goes out when it does, so a
/// player that changes fast costs one waiting broadcast.
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

crate::tick::checkpoint::fields!(SkinBudget { bytes, at, turn });

impl Default for SkinBudget {
    fn default() -> Self {
        Self {
            bytes: MAX_SKIN_BYTES,
            at: 0,
            turn: SessionId::new(0),
        }
    }
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

/// When a player last emoted and changed its skin, and the skin it last asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Appearance {
    /// The tick of the last emote shown.
    emote: Option<u64>,
    /// The tick of the last skin change asked for.
    skin: Option<u64>,
    /// The full ID of the skin last asked for (PMMP `lastRequestedFullSkinId`): the client sends
    /// back a skin the server sent it, which must not count as a change.
    skin_id: Option<String>,
}

crate::tick::checkpoint::fields!(Appearance {
    emote,
    skin,
    skin_id
});

impl Appearance {
    /// Hashes every field (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            emote,
            skin,
            skin_id,
        } = self;
        for t in [emote, skin] {
            h.u64(t.unwrap_or(u64::MAX));
        }
        match skin_id {
            Some(id) => h.bytes(id.as_bytes()),
            None => h.u64(u64::MAX),
        }
    }
}

/// `Emote`: see [`super::on_emote`].
pub(super) fn on_emote(state: &mut State, session: SessionId, packet: &Emote) {
    let tick = state.tick;
    let ready = |state: &State| {
        state.players.get(&session).is_some_and(|p| {
            p.phase == Phase::Spawned
                && p.survival
                    .appearance
                    .emote
                    .is_none_or(|last| tick.saturating_sub(last) > EMOTE_COOLDOWN)
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

/// The `Emote` packet that shows the player with `runtime_id` playing `emote_id` (PMMP
/// `EntityEventBroadcaster::onEmote`).
pub(super) fn emote_packet(runtime_id: u64, emote_id: String) -> Packet {
    Packet::Emote(Emote {
        entity_runtime_id: runtime_id,
        // PMMP: the client ignores it, and a value from a client must not be passed on.
        emote_length: 0,
        emote_id,
        xuid: String::new(),
        platform_id: String::new(),
        flags: u8::from(EmoteFlag::ServerSide) | u8::from(EmoteFlag::MuteChat),
    })
}

/// Whether `id` may be an emote ID: not empty and at most [`MAX_EMOTE_ID`] bytes.
pub(super) fn valid_emote(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_EMOTE_ID
}

/// `PlayerSkin`: see [`super::on_skin`].
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
    if a.skin
        .is_some_and(|last| tick.saturating_sub(last) < SKIN_COOLDOWN)
    {
        debug!(session = session.get(), "skin changes too fast; ignored");
        return;
    }
    let new = Skin::new(Arc::clone(&packet.skin));
    if !new.is_acceptable() {
        debug!(session = session.get(), "invalid skin ignored");
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
        |state, e| state.players.contains_key(&session) && e.new_skin.is_acceptable(),
    );
    let Ok(event) = event else {
        // The client shows the skin it asked for: give it its own back.
        if let Some(player) = state.players.get_mut(&session) {
            let packet = skin_packet(player.identity.uuid, &old);
            player.send_packet(packet);
        }
        return;
    };
    let skin = event.as_ref().map_or(new, |e| e.new_skin.clone());
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    player.identity.skin = skin;
    // Broadcast at the end of the tick within the server's skin budget (`send_skins`).
    player.survival.custom.skin = true;
    super::watch(state, event.as_ref());
}

/// Whether the player's skin changed and waits to be broadcast ([`send_skins`]).
pub(in crate::tick) fn skin_waits(player: &Player) -> bool {
    player.survival.custom.skin
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

/// Sends the skin of the player of `session` to every player (PMMP `Human::sendSkin` to all),
/// one copy shared by all of them, and clears its mark, if it fits in the skin budget; false,
/// sending nothing, if it does not.
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
    for p in state.players.values_mut() {
        if p.gets_broadcasts() {
            p.send_shared(Arc::clone(&packet));
        }
    }
    true
}

/// The `PlayerSkin` packet that shows `skin` on the player with `uuid` (PMMP `Human::sendSkin`).
fn skin_packet(uuid: protocol::Uuid, skin: &Skin) -> Packet {
    Packet::PlayerSkin(PlayerSkin {
        uuid,
        skin: Arc::clone(skin.shared()),
        new_skin_name: String::new(),
        old_skin_name: String::new(),
    })
}
