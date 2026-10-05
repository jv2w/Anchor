//! Emotes and skin changes in game (PMMP `Player::emote`, `Human::emote`, `Player::changeSkin`,
//! `InGamePacketHandler::handleEmote`, `handlePlayerSkin`), with their events.

use std::sync::Arc;

use tracing::debug;

use super::{Phase, SessionId, State, broadcast_to_viewers};
use crate::entity::MAX_SKIN_BYTES;
use crate::event::{PlayerChangeSkin, PlayerEmote};
use crate::player::Id;
use crate::protocol::consts::EmoteFlag;
use crate::protocol::packet::{Emote, Packet, PlayerSkin};
use crate::protocol::{self, skin_validate};
use crate::tick::queue::Skin;

/// Ticks that must pass between two emotes of a player: more than 5 (PMMP `Player::emote`).
const EMOTE_COOLDOWN: u64 = 5;
/// The longest emote ID passed on: an emote ID is a UUID (36 bytes); more is not an emote.
const MAX_EMOTE_ID: usize = 128;
/// Ticks between two skin changes of a player: one a second. PMMP has no limit, and each change
/// sends a skin of up to 4 MiB to every player.
const SKIN_COOLDOWN: u64 = 20;
/// The most skin bytes broadcast in one tick: one skin of the largest size, or many small ones.
/// Each broadcast is held once for all its receivers ([`Player::send_shared`]), but every
/// receiver's session encodes and sends it, so this bounds what each player is sent per tick
/// (PMMP sends every change at once). A skin over the tick's rest waits for a later tick.
///
/// [`Player::send_shared`]: crate::tick::Player::send_shared
pub(in crate::tick) const SKIN_BYTES_PER_TICK: usize = MAX_SKIN_BYTES;

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
    let new = Skin::new(packet.skin.clone());
    if !valid_skin(&new) {
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
        |state, e| state.players.contains_key(&session) && valid_skin(&e.new_skin),
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
    // Broadcast at the end of the tick within the tick's skin budget (`api::sync`).
    player.survival.custom.skin = true;
    super::watch(state, event.as_ref());
}

/// Sends the skin of the player of `session` to every player (PMMP `Human::sendSkin` to all),
/// one copy shared by all of them, if it fits in `budget`, the skin bytes left this tick
/// ([`SKIN_BYTES_PER_TICK`]); false, sending nothing, if it does not.
pub(super) fn send_skin(state: &mut State, session: SessionId, budget: &mut usize) -> bool {
    let Some(player) = state.players.get(&session) else {
        return true;
    };
    let Some(left) = budget.checked_sub(player.identity.skin.size()) else {
        return false;
    };
    *budget = left;
    let packet = Arc::new(skin_packet(player.identity.uuid, &player.identity.skin));
    for p in state.players.values_mut() {
        if p.phase != Phase::Loading && p.is_online() {
            p.send_shared(Arc::clone(&packet));
        }
    }
    true
}

/// Whether a skin is one a login would accept: image sizes that match their data, finite
/// animation frame counts, at most [`MAX_SKIN_BYTES`] in all.
pub(super) fn valid_skin(skin: &Skin) -> bool {
    skin_validate(skin.get()).is_none()
        && skin
            .get()
            .animations
            .iter()
            .all(|a| a.frame_count.is_finite() && a.frame_count >= 0.0)
        && skin.size() <= MAX_SKIN_BYTES
}

/// The `PlayerSkin` packet that shows `skin` on the player with `uuid` (PMMP `Human::sendSkin`).
fn skin_packet(uuid: protocol::Uuid, skin: &Skin) -> Packet {
    Packet::PlayerSkin(Box::new(PlayerSkin {
        uuid,
        skin: skin.get().clone(),
        new_skin_name: String::new(),
        old_skin_name: String::new(),
    }))
}
