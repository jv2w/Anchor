//! Player events: joining, leaving, moving, chatting and the other things a player does.
//!
//! Messages are plain text or client translation keys, which the default join, quit and death
//! lines are. An empty join, quit or chat death message shows nothing; an empty kick or death
//! screen message shows the client's own text.

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use super::set_clamped;
use crate::block::{Face, State};
use crate::config::GameMode;
use crate::entity;
use crate::item::Stack;
use crate::math::{self, Vec3};
use crate::player;
use crate::protocol::{self, Uuid};
use crate::tick;
use crate::tick::queue::{Identity, Skin};
use crate::world::{self, BlockPos, ChunkPos};

/// Raised when a player picks a block (middle click), before the item goes to the hotbar: a
/// creative player gets the block's item, others only select or swap one they carry. Cancelling
/// gives nothing.
#[doc(alias = "PlayerBlockPickEvent")]
#[derive(Debug)]
pub struct PlayerBlockPick {
    pub(crate) player: player::Id,
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) result_item: Stack,
    pub(crate) cancelled: bool,
}

getters!(PlayerBlockPick {
    /// The player.
    player: player::Id,
    /// The world of the block.
    world: world::Id,
    /// Where the block is.
    pos: BlockPos,
    /// The block picked.
    #[doc(alias = "PlayerBlockPickEvent::getBlock")]
    block: State,
    /// The item the player gets.
    #[doc(alias = "PlayerBlockPickEvent::getResultItem")]
    result_item: &Stack,
});

cancellable! {
    PlayerBlockPick
        #[doc(alias = "PlayerBlockPickEvent::cancel")]
        #[doc(alias = "PlayerBlockPickEvent::uncancel")]
        #[doc(alias = "PlayerBlockPickEvent::isCancelled")];
}

/// Raised when a player changes skin in game.
///
/// Cancelling keeps the old skin, and so does a listener that sets the player's skin through the
/// API.
#[doc(alias = "PlayerChangeSkinEvent")]
#[derive(Debug)]
pub struct PlayerChangeSkin {
    pub(crate) player: player::Id,
    pub(crate) old_skin: Skin,
    pub(crate) new_skin: Skin,
    pub(crate) cancelled: bool,
}

getters!(PlayerChangeSkin {
    /// The player.
    player: player::Id,
});

impl PlayerChangeSkin {
    /// The skin after.
    #[doc(alias = "PlayerChangeSkinEvent::getNewSkin")]
    pub fn new_skin(&self) -> &protocol::Skin {
        self.new_skin.get()
    }

    /// The skin before.
    #[doc(alias = "PlayerChangeSkinEvent::getOldSkin")]
    pub fn old_skin(&self) -> &protocol::Skin {
        self.old_skin.get()
    }

    /// Replaces the skin after. It is checked like a login skin (image sizes, at most 4 MiB in
    /// all, geometry and resource patch JSON nested at most 64 deep); a skin that fails cancels
    /// the change.
    #[doc(alias = "PlayerChangeSkinEvent::setNewSkin")]
    pub fn set_new_skin(&mut self, skin: protocol::Skin) {
        self.new_skin = Skin::new(Arc::new(skin));
    }
}

cancellable! {
    PlayerChangeSkin
        #[doc(alias = "PlayerChangeSkinEvent::cancel")]
        #[doc(alias = "PlayerChangeSkinEvent::uncancel")]
        #[doc(alias = "PlayerChangeSkinEvent::isCancelled")];
}

/// Raised when a player sends a chat message. Cancelling sends it to nobody, and so does a
/// speaker who is not online when the listeners are done or a line longer than
/// [`player::MAX_TEXT_BYTES`] bytes; monitors hear none of these.
#[doc(alias = "PlayerChatEvent")]
#[derive(Debug)]
pub struct PlayerChat {
    pub(crate) player: player::Id,
    pub(crate) message: String,
    pub(crate) recipients: Vec<player::Id>,
    pub(crate) format: Option<String>,
    pub(crate) cancelled: bool,
}

getters!(PlayerChat {
    /// The player who speaks.
    player: player::Id,
    /// The message.
    #[doc(alias = "PlayerChatEvent::getMessage")]
    message: &str,
    /// The players who see it. Starts as every player that has joined and is not leaving.
    #[doc(alias = "PlayerChatEvent::getRecipients")]
    recipients: &[player::Id],
});

setters!(PlayerChat {
    /// Makes another player the speaker; if that player is not online when the listeners are
    /// done (joined and not leaving), the line goes to nobody and no monitor sees it.
    #[doc(alias = "PlayerChatEvent::setPlayer")]
    set_player => player: player::Id,
    /// Replaces the message; a line over [`player::MAX_TEXT_BYTES`] bytes goes to nobody and no monitor
    /// sees it.
    #[doc(alias = "PlayerChatEvent::setMessage")]
    set_message => message: String,
    /// Replaces the players who see it; players who left are skipped.
    #[doc(alias = "PlayerChatEvent::setRecipients")]
    set_recipients => recipients: Vec<player::Id>,
    /// Replaces the format: `None` for the standard line (sent as the official server sends it, see
    /// [`PlayerChat::format`]), or a text where `{%0}` is the speaker's display name, `{%1}` the
    /// message and `{%%` a literal `{%`. Text put into the format, such as a name,
    /// keeps its own `{%0}` with every `{%` written as `{%%`: `text.replace("{%", "{%%")`. The
    /// name and the message filled in are taken as they are.
    #[doc(alias = "PlayerChatEvent::setFormatter")]
    set_format => format: Option<String>,
});

impl PlayerChat {
    /// How the line looks: `None` for the standard line, sent as the official server sends it (a
    /// chat text with the speaker's name as its source and the raw message, which the client shows
    /// as `<name> message`), or a text where `{%0}` is the speaker's display name, `{%1}` the
    /// message and `{%%` a literal `{%`. Starts as `None`.
    #[doc(alias = "PlayerChatEvent::getFormatter")]
    pub fn format(&self) -> Option<&str> {
        self.format.as_deref()
    }
}

cancellable! {
    PlayerChat
        #[doc(alias = "PlayerChatEvent::cancel")]
        #[doc(alias = "PlayerChatEvent::uncancel")]
        #[doc(alias = "PlayerChatEvent::isCancelled")];
}

/// Raised before an online player's changed data goes into the next save of the server's state.
/// A player who has left raises none: the data is saved without the event. Cancelling is ignored:
/// the data is saved anyway, so the commit stays atomic; what listeners change before the
/// snapshot is saved. A held offline record
/// ([`Context::hold_offline_player`](crate::Context::hold_offline_player)) raises none: it changes
/// only through the program's own calls, which raise no events.
#[doc(alias = "PlayerDataSaveEvent")]
#[derive(Debug)]
pub struct PlayerDataSave {
    pub(crate) player: player::Id,
    pub(crate) record_key: String,
    pub(crate) cancelled: bool,
}

getters!(PlayerDataSave {
    /// The player.
    #[doc(alias = "PlayerDataSaveEvent::getPlayer")]
    player: player::Id,
    /// The key the record is saved under: the XUID in decimal, or `offline:` and the lowercase
    /// name when the player has no XUID.
    #[doc(alias = "PlayerDataSaveEvent::getPlayerName")]
    record_key: &str,
});

impl PlayerDataSave {
    /// Whether a listener called [`cancel`](Self::cancel); the data is saved either way.
    #[doc(alias = "PlayerDataSaveEvent::isCancelled")]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Marks the event cancelled, which Anchor ignores: the data is saved anyway, because a
    /// snapshot holds the whole state of one tick boundary, and a player's data
    /// left out of it would come back after a crash beside a newer world. `save-player-data`
    /// turns player data saving off.
    #[doc(alias = "PlayerDataSaveEvent::cancel")]
    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    /// Undoes [`cancel`](Self::cancel).
    #[doc(alias = "PlayerDataSaveEvent::uncancel")]
    pub fn uncancel(&mut self) {
        self.cancelled = false;
    }
}

/// Raised when a player dies, before the drops fall and the death message shows.
#[doc(alias = "PlayerDeathEvent")]
#[derive(Debug)]
pub struct PlayerDeath {
    pub(crate) player: player::Id,
    pub(crate) drops: Vec<Stack>,
    pub(crate) xp_drop_amount: u32,
    pub(crate) death_message: String,
    pub(crate) death_screen_message: String,
    pub(crate) keep_inventory: bool,
    pub(crate) keep_xp: bool,
}

getters!(PlayerDeath {
    /// The player.
    #[doc(alias = "PlayerDeathEvent::getPlayer")]
    #[doc(alias = "PlayerDeathEvent::getEntity")]
    player: player::Id,
    /// What the player drops. Starts as its main slots, armor and off hand without items that
    /// vanish or stay on death. Ignored when the player keeps the inventory or earlier death drops
    /// still wait for room (then it starts empty).
    drops: &[Stack],
    /// The XP the player drops.
    xp_drop_amount: u32,
    /// The message every player sees in chat. With `show-death-messages` it starts as the
    /// death's translation key, which, left so, the client translates with the dead player's and
    /// the attacker's names; otherwise it starts empty. An empty one shows nothing, one longer
    /// than [`player::MAX_TEXT_BYTES`] bytes is not sent, any other shows as it is.
    #[doc(alias = "PlayerDeathEvent::getDeathMessage")]
    death_message: &str,
    /// The message on the dead player's death screen. Starts empty, which shows the death's
    /// translated message; so does one longer than [`player::MAX_TEXT_BYTES`] bytes.
    #[doc(alias = "PlayerDeathEvent::getDeathScreenMessage")]
    death_screen_message: &str,
});

setters!(PlayerDeath {
    /// Replaces the XP the player drops.
    set_xp_drop_amount => xp_drop_amount: u32,
    /// Replaces the chat message; one longer than [`player::MAX_TEXT_BYTES`] bytes shows nothing.
    #[doc(alias = "PlayerDeathEvent::setDeathMessage")]
    set_death_message => death_message: String,
    /// Replaces the death screen message; an empty one, or one longer than [`player::MAX_TEXT_BYTES`]
    /// bytes, shows the death's translated message.
    #[doc(alias = "PlayerDeathEvent::setDeathScreenMessage")]
    set_death_screen_message => death_screen_message: String,
    /// Sets whether the player keeps the inventory.
    #[doc(alias = "PlayerDeathEvent::setKeepInventory")]
    set_keep_inventory => keep_inventory: bool,
    /// Sets whether the player keeps the XP.
    #[doc(alias = "PlayerDeathEvent::setKeepXp")]
    set_keep_xp => keep_xp: bool,
});

impl PlayerDeath {
    /// Whether the player keeps the inventory.
    #[doc(alias = "PlayerDeathEvent::getKeepInventory")]
    pub fn keeps_inventory(&self) -> bool {
        self.keep_inventory
    }

    /// Whether the player keeps the XP.
    #[doc(alias = "PlayerDeathEvent::getKeepXp")]
    pub fn keeps_xp(&self) -> bool {
        self.keep_xp
    }

    /// Replaces what the player drops. At most as many stacks as a player carries are kept, none
    /// of them empty, each cut to its item's stack size, and [`drops`](Self::drops) shows what is
    /// kept. A stack of the player's own that the event showed and these leave out is gone; every
    /// other stack it carries drops or stays as without a listener.
    pub fn set_drops(&mut self, drops: Vec<Stack>) {
        self.drops = tick::valid_stacks(drops, tick::MAX_LISTENER_DROPS);
    }
}

/// Raised when a player drops an item, before it leaves the inventory. Cancelling keeps it.
#[doc(alias = "PlayerDropItemEvent")]
#[derive(Debug)]
pub struct PlayerDropItem {
    pub(crate) player: player::Id,
    pub(crate) item: Stack,
    pub(crate) cancelled: bool,
}

getters!(PlayerDropItem {
    /// The player.
    player: player::Id,
    /// What the player drops.
    #[doc(alias = "PlayerDropItemEvent::getItem")]
    item: &Stack,
});

cancellable! {
    PlayerDropItem
        #[doc(alias = "PlayerDropItemEvent::cancel")]
        #[doc(alias = "PlayerDropItemEvent::uncancel")]
        #[doc(alias = "PlayerDropItemEvent::isCancelled")];
}

/// Raised when a player joins, after its [`PlayerLogin`] and the resource packs, while the same
/// player is already online: the same XUID with Xbox sign-in on, otherwise the same name
/// ignoring ASCII case.
///
/// If no listener cancels it, the connecting player is checked for a refusal that applies only
/// since its login (the slot the existing player would free counts as free); once admitted, the
/// existing player is disconnected and the connecting one joins, and if refused, it is
/// disconnected and the existing player stays. If the event is cancelled, the connecting player
/// is disconnected at once. A player this event disconnects sees
/// [`kick_screen_message`](Self::kick_screen_message), or
/// [`kick_reason`](Self::kick_reason) when that is `None`.
#[doc(alias = "PlayerDuplicateLoginEvent")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerDuplicateLogin {
    pub(crate) connecting: PlayerInfo,
    pub(crate) existing: PlayerInfo,
    pub(crate) reason: String,
    pub(crate) screen_message: Option<String>,
    pub(crate) cancelled: bool,
}

impl PlayerDuplicateLogin {
    /// The reason the event starts with: the client's "logged in from another location" text.
    pub(crate) const DEFAULT_REASON: &str = "disconnectionScreen.loggedinOtherLocation";

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

    // Written by hand, not by `cancellable!`, which takes one alias per method; these carry the
    // extra `CancellableTrait` and `Cancellable` ones.
    /// Whether a listener cancelled the event.
    #[doc(alias = "PlayerDuplicateLoginEvent::isCancelled")]
    #[doc(alias = "CancellableTrait::isCancelled")]
    #[doc(alias = "Cancellable::isCancelled")]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// The reason given to the disconnected player. Starts as the client's text key
    /// `"disconnectionScreen.loggedinOtherLocation"`; whether the official client shows a key
    /// sent as the reason in its language or as it is is not confirmed.
    #[doc(alias = "PlayerDuplicateLoginEvent::getDisconnectReason")]
    pub fn kick_reason(&self) -> &str {
        &self.reason
    }

    /// The message the disconnected player sees instead of the reason, if any. Starts as `None`.
    #[doc(alias = "PlayerDuplicateLoginEvent::getDisconnectScreenMessage")]
    pub fn kick_screen_message(&self) -> Option<&str> {
        self.screen_message.as_deref()
    }

    /// Keeps the existing player and disconnects the connecting one.
    #[doc(alias = "PlayerDuplicateLoginEvent::cancel")]
    #[doc(alias = "CancellableTrait::cancel")]
    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    /// Replaces the reason given to the disconnected player. A text longer than
    /// [`player::MAX_TEXT_BYTES`] bytes is cut to it when sent.
    #[doc(alias = "PlayerDuplicateLoginEvent::setDisconnectReason")]
    pub fn set_kick_reason(&mut self, reason: impl Into<String>) {
        self.reason = reason.into();
    }

    /// Replaces the message the disconnected player sees; `None` shows the reason. A text longer
    /// than [`player::MAX_TEXT_BYTES`] bytes is cut to it when sent.
    #[doc(alias = "PlayerDuplicateLoginEvent::setDisconnectScreenMessage")]
    pub fn set_kick_screen_message(&mut self, message: Option<String>) {
        self.screen_message = message;
    }

    /// Undoes [`cancel`](Self::cancel): the existing player is disconnected again.
    #[doc(alias = "PlayerDuplicateLoginEvent::uncancel")]
    #[doc(alias = "CancellableTrait::uncancel")]
    pub fn uncancel(&mut self) {
        self.cancelled = false;
    }

    /// The outcome the listeners chose, and the connecting player's identity back.
    pub(crate) fn into_outcome(self) -> (Identity, DuplicateOutcome) {
        let outcome = DuplicateOutcome {
            cancelled: self.cancelled,
            message: self.screen_message.unwrap_or_else(|| self.reason.clone()),
            reason: self.reason,
        };
        (self.connecting.0, outcome)
    }
}

/// What the tick does after a [`PlayerDuplicateLogin`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DuplicateOutcome {
    /// Disconnect the connecting player instead of the existing one.
    pub(crate) cancelled: bool,
    /// The text of the `Disconnect` the disconnected player gets.
    pub(crate) message: String,
    /// Why the disconnected player left, for [`QuitReason::Replaced`]: the reason, never the
    /// screen message.
    pub(crate) reason: String,
}

impl DuplicateOutcome {
    /// The outcome when no listener is registered, without building the event.
    pub(crate) fn unheard() -> Self {
        Self {
            cancelled: false,
            message: PlayerDuplicateLogin::DEFAULT_REASON.to_owned(),
            reason: PlayerDuplicateLogin::DEFAULT_REASON.to_owned(),
        }
    }
}

/// Raised when a player edits a book and quill in its inventory, before the book changes.
/// Cancelling keeps the old book.
#[doc(alias = "PlayerEditBookEvent")]
#[derive(Debug)]
pub struct PlayerEditBook {
    pub(crate) player: player::Id,
    pub(crate) old_book: Stack,
    pub(crate) new_book: Stack,
    pub(crate) action: BookAction,
    pub(crate) modified_pages: Vec<u32>,
    pub(crate) cancelled: bool,
}

getters!(PlayerEditBook {
    /// The player.
    player: player::Id,
    /// What the player did.
    #[doc(alias = "PlayerEditBookEvent::getAction")]
    action: BookAction,
    /// The book before the edit.
    #[doc(alias = "PlayerEditBookEvent::getOldBook")]
    old_book: &Stack,
    /// The book after the edit, which replaces the old one.
    #[doc(alias = "PlayerEditBookEvent::getNewBook")]
    new_book: &Stack,
    /// The pages the edit changed, counted from 0 (none for signing).
    #[doc(alias = "PlayerEditBookEvent::getModifiedPages")]
    modified_pages: &[u32],
});

impl PlayerEditBook {
    /// Replaces the book the player gets. A stack that is not a book and quill or a written book
    /// is ignored.
    #[doc(alias = "PlayerEditBookEvent::setNewBook")]
    pub fn set_new_book(&mut self, book: Stack) {
        if book
            .item_type()
            .and_then(crate::item::Type::kind)
            .is_some_and(|k| {
                matches!(
                    k,
                    crate::item::Kind::WritableBook | crate::item::Kind::WrittenBook
                )
            })
        {
            self.new_book = book;
        }
    }
}

cancellable! {
    PlayerEditBook
        #[doc(alias = "PlayerEditBookEvent::cancel")]
        #[doc(alias = "PlayerEditBookEvent::uncancel")]
        #[doc(alias = "PlayerEditBookEvent::isCancelled")];
}

/// What a player did to a book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BookAction {
    /// Rewrote a page.
    #[doc(alias = "PlayerEditBookEvent::ACTION_REPLACE_PAGE")]
    ReplacePage,
    /// Inserted a page before another.
    #[doc(alias = "PlayerEditBookEvent::ACTION_ADD_PAGE")]
    AddPage,
    /// Removed a page.
    #[doc(alias = "PlayerEditBookEvent::ACTION_DELETE_PAGE")]
    DeletePage,
    /// Swapped two pages.
    #[doc(alias = "PlayerEditBookEvent::ACTION_SWAP_PAGES")]
    SwapPages,
    /// Signed the book: it becomes a written book.
    #[doc(alias = "PlayerEditBookEvent::ACTION_SIGN_BOOK")]
    SignBook,
}

/// Raised when a player plays an emote. Cancelling shows it to nobody.
#[doc(alias = "PlayerEmoteEvent")]
#[derive(Debug)]
pub struct PlayerEmote {
    pub(crate) player: player::Id,
    pub(crate) emote_id: String,
    pub(crate) cancelled: bool,
}

getters!(PlayerEmote {
    /// The player.
    player: player::Id,
    /// The emote's ID (a UUID).
    #[doc(alias = "PlayerEmoteEvent::getEmoteId")]
    emote_id: &str,
});

setters!(PlayerEmote {
    /// Plays another emote instead; an ID that is empty or longer than 128 bytes cancels the
    /// emote.
    #[doc(alias = "PlayerEmoteEvent::setEmoteId")]
    set_emote_id => emote_id: String,
});

cancellable! {
    PlayerEmote
        #[doc(alias = "PlayerEmoteEvent::cancel")]
        #[doc(alias = "PlayerEmoteEvent::uncancel")]
        #[doc(alias = "PlayerEmoteEvent::isCancelled")];
}

/// Raised when a player uses (right clicks) an entity within reach, before the entity reacts: a
/// villager NPC opens trading, a custom entity runs its behaviour
/// ([`Behaviour::on_interact`](crate::entity::Behaviour::on_interact)). Cancelling keeps the
/// entity from reacting.
///
/// Using another player raises nothing: a player has no [`entity::Id`]. Monitors are called only
/// when the entity reacted.
#[doc(alias = "PlayerEntityInteractEvent")]
#[derive(Debug)]
pub struct PlayerEntityInteract {
    pub(crate) player: player::Id,
    pub(crate) entity: entity::Id,
    pub(crate) click_position: Vec3,
    pub(crate) cancelled: bool,
}

getters!(PlayerEntityInteract {
    /// The player.
    player: player::Id,
    /// The entity used.
    #[doc(alias = "PlayerEntityInteractEvent::getEntity")]
    entity: entity::Id,
    /// Where the player clicked, relative to the entity's position.
    #[doc(alias = "PlayerEntityInteractEvent::getClickPosition")]
    click_position: Vec3,
});

cancellable! {
    PlayerEntityInteract
        #[doc(alias = "PlayerEntityInteractEvent::cancel")]
        #[doc(alias = "PlayerEntityInteractEvent::uncancel")]
        #[doc(alias = "PlayerEntityInteractEvent::isCancelled")];
}

/// Raised when a player picks an entity (middle click), before the entity's item goes to the
/// hotbar: a creative player gets the entity's item, others only select or swap one they carry.
/// Nothing is raised when there is no item to pick. Cancelling gives nothing.
#[doc(alias = "PlayerEntityPickEvent")]
#[derive(Debug)]
pub struct PlayerEntityPick {
    pub(crate) player: player::Id,
    pub(crate) entity: entity::Id,
    pub(crate) result_item: Stack,
    pub(crate) cancelled: bool,
}

getters!(PlayerEntityPick {
    /// The player.
    player: player::Id,
    /// The entity picked.
    #[doc(alias = "PlayerEntityPickEvent::getEntity")]
    entity: entity::Id,
    /// The item the player gets.
    #[doc(alias = "PlayerEntityPickEvent::getResultItem")]
    result_item: &Stack,
});

cancellable! {
    PlayerEntityPick
        #[doc(alias = "PlayerEntityPickEvent::cancel")]
        #[doc(alias = "PlayerEntityPickEvent::uncancel")]
        #[doc(alias = "PlayerEntityPickEvent::isCancelled")];
}

/// Raised when exhaustion is about to be added to a player whose food is on, which uses up its
/// food; walking raises it with an amount of 0. Cancelling keeps the player's exhaustion.
#[doc(alias = "PlayerExhaustEvent")]
#[derive(Debug)]
pub struct PlayerExhaust {
    pub(crate) player: player::Id,
    pub(crate) amount: f64,
    pub(crate) cause: ExhaustCause,
    pub(crate) cancelled: bool,
}

getters!(PlayerExhaust {
    /// The player.
    #[doc(alias = "PlayerExhaustEvent::getPlayer")]
    player: player::Id,
    /// The exhaustion added (4 costs one saturation or food point).
    #[doc(alias = "PlayerExhaustEvent::getAmount")]
    amount: f64,
    /// Why.
    #[doc(alias = "PlayerExhaustEvent::getCause")]
    cause: ExhaustCause,
});

impl PlayerExhaust {
    /// Sets the exhaustion added, raised to 0 if negative. A value that is not finite is ignored.
    #[doc(alias = "PlayerExhaustEvent::setAmount")]
    pub fn set_amount(&mut self, amount: f64) {
        set_clamped(&mut self.amount, amount, 0.0, f64::MAX);
    }
}

cancellable! {
    PlayerExhaust
        #[doc(alias = "PlayerExhaustEvent::cancel")]
        #[doc(alias = "PlayerExhaustEvent::uncancel")]
        #[doc(alias = "PlayerExhaustEvent::isCancelled")];
}

/// Why a player's exhaustion grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExhaustCause {
    /// Hitting.
    #[doc(alias = "PlayerExhaustEvent::CAUSE_ATTACK")]
    Attack,
    /// Taking damage.
    #[doc(alias = "PlayerExhaustEvent::CAUSE_DAMAGE")]
    Damage,
    /// Breaking a block.
    #[doc(alias = "PlayerExhaustEvent::CAUSE_MINING")]
    Mining,
    /// Healing from a full food bar.
    #[doc(alias = "PlayerExhaustEvent::CAUSE_HEALTH_REGEN")]
    HealthRegen,
    /// The Hunger effect.
    #[doc(alias = "PlayerExhaustEvent::CAUSE_POTION")]
    Potion,
    /// Walking.
    #[doc(alias = "PlayerExhaustEvent::CAUSE_WALKING")]
    Walking,
    /// Sprinting.
    #[doc(alias = "PlayerExhaustEvent::CAUSE_SPRINTING")]
    Sprinting,
    /// Jumping.
    #[doc(alias = "PlayerExhaustEvent::CAUSE_JUMPING")]
    Jumping,
    /// Jumping while sprinting.
    #[doc(alias = "PlayerExhaustEvent::CAUSE_SPRINT_JUMPING")]
    SprintJumping,
}

/// Raised when a player's game mode is about to change. Cancelling keeps it.
#[doc(alias = "PlayerGameModeChangeEvent")]
#[derive(Debug)]
pub struct PlayerGameModeChange {
    pub(crate) player: player::Id,
    pub(crate) new_game_mode: GameMode,
    pub(crate) cancelled: bool,
}

getters!(PlayerGameModeChange {
    /// The player.
    player: player::Id,
    /// The game mode after.
    #[doc(alias = "PlayerGameModeChangeEvent::getNewGamemode")]
    new_game_mode: GameMode,
});

cancellable! {
    PlayerGameModeChange
        #[doc(alias = "PlayerGameModeChangeEvent::cancel")]
        #[doc(alias = "PlayerGameModeChangeEvent::uncancel")]
        #[doc(alias = "PlayerGameModeChangeEvent::isCancelled")];
}

/// Raised when a player clicks a block, before the block or the held item reacts. Cancelling does
/// neither (and a left click does not start breaking).
///
/// A player's right click raises it only when it can take effect: the player is spawned, alive
/// and not a spectator, holds the item the client says it holds, is within reach and looks toward
/// the block, the block is not air, the block and the one next to the clicked face are loaded
/// and inside the height limits, and the click is not a repeat of the previous one (same block,
/// face, point and player position) in the same or the next tick. Otherwise nothing is raised and
/// the client gets the blocks around the click again as they are. A left click is raised when the
/// player is not a spectator and the block is within 100 blocks.
#[doc(alias = "PlayerInteractEvent")]
#[derive(Debug)]
pub struct PlayerInteract {
    pub(crate) player: player::Id,
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) face: Face,
    pub(crate) item: Stack,
    pub(crate) touch_vector: Vec3,
    pub(crate) action: InteractAction,
    pub(crate) first: bool,
    pub(crate) use_item: bool,
    pub(crate) use_block: bool,
    pub(crate) cancelled: bool,
}

getters!(PlayerInteract {
    /// The player.
    player: player::Id,
    /// The world of the block.
    world: world::Id,
    /// Where the block is.
    pos: BlockPos,
    /// The block clicked.
    #[doc(alias = "PlayerInteractEvent::getBlock")]
    block: State,
    /// The face clicked.
    #[doc(alias = "PlayerInteractEvent::getFace")]
    face: Face,
    /// The item held.
    #[doc(alias = "PlayerInteractEvent::getItem")]
    item: &Stack,
    /// Where on the block the player clicked, each part 0 to 1 (zero for a left click).
    #[doc(alias = "PlayerInteractEvent::getTouchVector")]
    touch_vector: Vec3,
    /// The click.
    #[doc(alias = "PlayerInteractEvent::getAction")]
    action: InteractAction,
});

setters!(PlayerInteract {
    /// Turns the held item's use action on or off.
    #[doc(alias = "PlayerInteractEvent::setUseItem")]
    set_use_item => use_item: bool,
    /// Turns the clicked block's response on or off.
    #[doc(alias = "PlayerInteractEvent::setUseBlock")]
    set_use_block => use_block: bool,
});

impl PlayerInteract {
    /// Whether this is the first event of the click. While the player holds the right button on
    /// blocks, the client repeats the click; a repeat identical to the previous click in the same
    /// or the next tick raises nothing and any other repeat raises this event again; only the
    /// first of the press is first, even if the player looks at other blocks meanwhile. Left
    /// clicks and clicks of [`UseItemOn`](crate::action::UseItemOn) are always first.
    pub fn is_first(&self) -> bool {
        self.first
    }

    /// Whether a right click runs the clicked block's own response (a door opens, a container
    /// shows). Starts false for a sneaking player holding an item. It has no effect on a left
    /// click.
    #[doc(alias = "PlayerInteractEvent::useBlock")]
    pub fn uses_block(&self) -> bool {
        self.use_block
    }

    /// Whether a right click runs the held item's use action. Starts false for a sneaking player.
    /// Turning it off still lets the held item be placed as a block (a
    /// [`BlockPlace`](crate::event::BlockPlace) listener can stop that). It has no effect on a
    /// left click.
    #[doc(alias = "PlayerInteractEvent::useItem")]
    pub fn uses_item(&self) -> bool {
        self.use_item
    }
}

cancellable! {
    PlayerInteract
        #[doc(alias = "PlayerInteractEvent::cancel")]
        #[doc(alias = "PlayerInteractEvent::uncancel")]
        #[doc(alias = "PlayerInteractEvent::isCancelled")];
}

/// How a player touches a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InteractAction {
    /// Starting to break it.
    #[doc(alias = "PlayerInteractEvent::LEFT_CLICK_BLOCK")]
    LeftClickBlock,
    /// Using it, or using the held item on it.
    #[doc(alias = "PlayerInteractEvent::RIGHT_CLICK_BLOCK")]
    RightClickBlock,
}

/// Raised when a player finishes eating or drinking, before the item is used up. Cancelling keeps
/// the item and gives nothing.
#[doc(alias = "PlayerItemConsumeEvent")]
#[derive(Debug)]
pub struct PlayerItemConsume {
    pub(crate) player: player::Id,
    pub(crate) item: Stack,
    pub(crate) residue: Vec<Stack>,
    pub(crate) cancelled: bool,
}

getters!(PlayerItemConsume {
    /// The player.
    player: player::Id,
    /// The item eaten or drunk.
    #[doc(alias = "PlayerItemConsumeEvent::getItem")]
    item: &Stack,
    /// What is left after (a bowl, a bottle).
    #[doc(alias = "PlayerItemConsumeEvent::getResidue")]
    residue: &[Stack],
});

impl PlayerItemConsume {
    /// Replaces what is left after. At most 4 stacks are kept, none of them empty, each cut to
    /// its item's stack size, and [`residue`](Self::residue) shows what is kept. A creative
    /// player gets none, whatever is set.
    #[doc(alias = "PlayerItemConsumeEvent::setResidue")]
    pub fn set_residue(&mut self, residue: Vec<Stack>) {
        self.residue = tick::valid_stacks(residue, tick::MAX_RESIDUE);
    }
}

cancellable! {
    PlayerItemConsume
        #[doc(alias = "PlayerItemConsumeEvent::cancel")]
        #[doc(alias = "PlayerItemConsumeEvent::uncancel")]
        #[doc(alias = "PlayerItemConsumeEvent::isCancelled")];
}

/// Raised when a player selects another hotbar slot. Cancelling keeps the old one.
#[doc(alias = "PlayerItemHeldEvent")]
#[derive(Debug)]
pub struct PlayerItemHeld {
    pub(crate) player: player::Id,
    pub(crate) item: Stack,
    pub(crate) slot: usize,
    pub(crate) cancelled: bool,
}

getters!(PlayerItemHeld {
    /// The player.
    player: player::Id,
    /// The item in the new slot.
    #[doc(alias = "PlayerItemHeldEvent::getItem")]
    item: &Stack,
    /// The new hotbar slot, 0 to 8.
    #[doc(alias = "PlayerItemHeldEvent::getSlot")]
    slot: usize,
});

cancellable! {
    PlayerItemHeld
        #[doc(alias = "PlayerItemHeldEvent::cancel")]
        #[doc(alias = "PlayerItemHeldEvent::uncancel")]
        #[doc(alias = "PlayerItemHeldEvent::isCancelled")];
}

/// Raised when a player uses the held item in the air (throwing, drawing a bow, eating, drinking,
/// putting on armor, sounding a goat horn). A spectator, or an item whose cooldown still runs,
/// raises nothing. Cancelling refuses the use: nothing is thrown, drawn, eaten, drunk, worn or
/// sounded, and no cooldown starts. Monitors hear only a use that worked.
#[doc(alias = "PlayerItemUseEvent")]
#[derive(Debug)]
pub struct PlayerItemUse {
    pub(crate) player: player::Id,
    pub(crate) item: Stack,
    pub(crate) direction_vector: Vec3,
    pub(crate) cancelled: bool,
}

getters!(PlayerItemUse {
    /// The player.
    player: player::Id,
    /// The item used.
    #[doc(alias = "PlayerItemUseEvent::getItem")]
    item: &Stack,
    /// Where the player looks, a unit vector.
    #[doc(alias = "PlayerItemUseEvent::getDirectionVector")]
    direction_vector: Vec3,
});

cancellable! {
    PlayerItemUse
        #[doc(alias = "PlayerItemUseEvent::cancel")]
        #[doc(alias = "PlayerItemUseEvent::uncancel")]
        #[doc(alias = "PlayerItemUseEvent::isCancelled")];
}

/// Raised when a player has joined and spawned, before the join message shows.
#[doc(alias = "PlayerJoinEvent")]
#[derive(Debug)]
pub struct PlayerJoin {
    pub(crate) player: player::Id,
    pub(crate) join_message: String,
}

getters!(PlayerJoin {
    /// The player.
    #[doc(alias = "PlayerEvent::getPlayer")]
    player: player::Id,
    /// The message every other player sees. Starts as `"§e%multiplayer.player.joined"`: left so,
    /// the client shows its own "joined the game" line with the player's name.
    #[doc(alias = "PlayerJoinEvent::getJoinMessage")]
    join_message: &str,
});

setters!(PlayerJoin {
    /// Replaces the message every other player sees; an empty one shows nothing.
    #[doc(alias = "PlayerJoinEvent::setJoinMessage")]
    set_join_message => join_message: String,
});

/// Raised when a player jumps.
#[doc(alias = "PlayerJumpEvent")]
#[derive(Debug)]
pub struct PlayerJump {
    pub(crate) player: player::Id,
}

getters!(PlayerJump {
    /// The player.
    player: player::Id,
});

/// Raised when a player is about to be kicked. Cancelling keeps the player, and so does a text a
/// listener sets longer than [`player::MAX_TEXT_BYTES`] bytes.
#[doc(alias = "PlayerKickEvent")]
#[derive(Debug)]
pub struct PlayerKick {
    pub(crate) player: player::Id,
    pub(crate) kick_reason: String,
    pub(crate) screen_message: Option<String>,
    pub(crate) quit_message: String,
    pub(crate) cancelled: bool,
}

getters!(PlayerKick {
    /// The player.
    player: player::Id,
    /// The reason, for the log and the player's screen.
    #[doc(alias = "PlayerKickEvent::getDisconnectReason")]
    #[doc(alias = "PlayerDisconnectEventTrait::getDisconnectReason")]
    kick_reason: &str,
    /// The message every other player sees. Unless the kick gave one, it starts as
    /// `"§e%multiplayer.player.left"`: left so, the client shows its own "left the game" line
    /// with the player's name.
    #[doc(alias = "PlayerKickEvent::getQuitMessage")]
    quit_message: &str,
});

setters!(PlayerKick {
    /// Replaces the reason. An empty one, with no screen message, shows the client's own
    /// "disconnected" text.
    #[doc(alias = "PlayerKickEvent::setDisconnectReason")]
    #[doc(alias = "PlayerDisconnectEventTrait::setDisconnectReason")]
    set_kick_reason => kick_reason: String,
    /// Replaces the message the player sees instead of the reason; `None` shows the reason.
    #[doc(alias = "PlayerKickEvent::setDisconnectScreenMessage")]
    #[doc(alias = "PlayerDisconnectEventTrait::setDisconnectScreenMessage")]
    set_kick_screen_message => screen_message: Option<String>,
    /// Replaces the message every other player sees; an empty one shows nothing.
    #[doc(alias = "PlayerKickEvent::setQuitMessage")]
    set_quit_message => quit_message: String,
});

impl PlayerKick {
    /// The message the player sees: the screen message, or the reason when there is none.
    #[doc(alias = "PlayerKickEvent::getDisconnectScreenMessage")]
    #[doc(alias = "PlayerDisconnectEventTrait::getDisconnectScreenMessage")]
    pub fn kick_screen_message(&self) -> &str {
        self.screen_message.as_deref().unwrap_or(&self.kick_reason)
    }
}

cancellable! {
    PlayerKick
        #[doc(alias = "PlayerKickEvent::cancel")]
        #[doc(alias = "PlayerKickEvent::uncancel")]
        #[doc(alias = "PlayerKickEvent::isCancelled")];
}

/// Raised when a player has logged in, before its client gets the resource packs: Anchor has
/// already set the kick flags that apply (server full, not whitelisted, banned). The player
/// goes on to the resource packs and the join when no flag is set after the listeners; otherwise
/// the player is refused with the first flag's message, having downloaded nothing. When the same
/// player is online, its slot does not count toward a full server; [`PlayerDuplicateLogin`]
/// decides at the join. At the join, a refusal that applies only since this event (the server
/// filled, a ban or a whitelist change) refuses the player too, unless a listener cleared its
/// flag here.
#[doc(alias = "PlayerPreLoginEvent")]
#[doc(alias = "PlayerLoginEvent")]
#[derive(Debug)]
pub struct PlayerLogin {
    pub(crate) info: PlayerInfo,
    pub(crate) ip: Option<IpAddr>,
    pub(crate) port: u16,
    pub(crate) kicks: [Option<Kick>; KickFlag::all().len()],
}

impl PlayerLogin {
    /// The reason of the first set flag, if any.
    #[doc(alias = "PlayerPreLoginEvent::getFinalDisconnectReason")]
    pub fn final_kick_reason(&self) -> Option<&str> {
        self.kicks
            .iter()
            .flatten()
            .next()
            .map(|k| k.reason.as_str())
    }

    /// The screen message of the first set flag, if any: what the refused player sees.
    #[doc(alias = "PlayerPreLoginEvent::getFinalDisconnectScreenMessage")]
    pub fn final_kick_screen_message(&self) -> Option<&str> {
        self.kicks
            .iter()
            .flatten()
            .next()
            .map(|k| k.screen_message.as_str())
    }

    /// Who the player is.
    #[doc(alias = "PlayerPreLoginEvent::getPlayerInfo")]
    pub fn info(&self) -> &PlayerInfo {
        &self.info
    }

    /// The IP address the client connected from, as [`Player::ip`](crate::player::Player::ip)
    /// gives it; `None` only for a player made without a connection (tests).
    #[doc(alias = "PlayerPreLoginEvent::getIp")]
    pub fn ip(&self) -> Option<IpAddr> {
        self.ip
    }

    /// Whether no flag is set, so the player joins.
    #[doc(alias = "PlayerPreLoginEvent::isAllowed")]
    pub fn is_allowed(&self) -> bool {
        self.kicks.iter().all(Option::is_none)
    }

    /// Whether `flag` is set.
    #[doc(alias = "PlayerPreLoginEvent::isKickFlagSet")]
    pub fn is_kick_flag_set(&self, flag: KickFlag) -> bool {
        self.kick(flag).is_some()
    }

    /// The flags set, in order of precedence.
    #[doc(alias = "PlayerPreLoginEvent::getKickFlags")]
    pub fn kick_flags(&self) -> impl Iterator<Item = KickFlag> + '_ {
        KickFlag::all()
            .iter()
            .copied()
            .zip(&self.kicks)
            .filter_map(|(flag, kick)| kick.as_ref().map(|_| flag))
    }

    /// The reason of `flag`, if it is set.
    #[doc(alias = "PlayerPreLoginEvent::getDisconnectReason")]
    pub fn kick_reason(&self, flag: KickFlag) -> Option<&str> {
        self.kick(flag).map(|k| k.reason.as_str())
    }

    /// The screen message of `flag`, if it is set.
    #[doc(alias = "PlayerPreLoginEvent::getDisconnectScreenMessage")]
    pub fn kick_screen_message(&self, flag: KickFlag) -> Option<&str> {
        self.kick(flag).map(|k| k.screen_message.as_str())
    }

    /// The port the client connected from; 0 only for a player made without a connection
    /// (tests).
    #[doc(alias = "PlayerPreLoginEvent::getPort")]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Clears every flag: the player joins.
    #[doc(alias = "PlayerPreLoginEvent::clearAllKickFlags")]
    pub fn clear_all_kick_flags(&mut self) {
        self.kicks = Default::default();
    }

    /// Clears `flag`.
    #[doc(alias = "PlayerPreLoginEvent::clearKickFlag")]
    pub fn clear_kick_flag(&mut self, flag: KickFlag) {
        *self.kick_mut(flag) = None;
    }

    /// Sets `flag` with the reason for the log and the message for the player's screen (`None`:
    /// the reason). A text longer than [`player::MAX_TEXT_BYTES`] bytes is cut to it when sent.
    #[doc(alias = "PlayerPreLoginEvent::setKickFlag")]
    pub fn set_kick_flag(
        &mut self,
        flag: KickFlag,
        reason: impl Into<String>,
        screen_message: Option<String>,
    ) {
        let reason = reason.into();
        let screen_message = screen_message.unwrap_or_else(|| reason.clone());
        *self.kick_mut(flag) = Some(Kick {
            reason,
            screen_message,
        });
    }

    /// The messages of `flag`, if it is set.
    fn kick(&self, flag: KickFlag) -> Option<&Kick> {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: `kicks` has a slot for every flag of `KickFlag::all`"
        )]
        self.kicks[flag as usize].as_ref()
    }

    /// The messages of `flag`, to change.
    fn kick_mut(&mut self, flag: KickFlag) -> &mut Option<Kick> {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: `kicks` has a slot for every flag of `KickFlag::all`"
        )]
        &mut self.kicks[flag as usize]
    }
}

/// Why a joining player may be refused, in order of precedence: the first set flag gives the
/// message.
#[doc(alias = "PlayerPreLoginEvent::KICK_FLAG_PRIORITY")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KickFlag {
    /// A listener refuses the player.
    #[doc(alias = "PlayerPreLoginEvent::KICK_FLAG_PLUGIN")]
    Listener,
    /// The server is full.
    #[doc(alias = "PlayerPreLoginEvent::KICK_FLAG_SERVER_FULL")]
    ServerFull,
    /// The whitelist is on and the player is not on it.
    #[doc(alias = "PlayerPreLoginEvent::KICK_FLAG_SERVER_WHITELISTED")]
    ServerWhitelisted,
    /// The player's name or address is banned.
    #[doc(alias = "PlayerPreLoginEvent::KICK_FLAG_BANNED")]
    Banned,
}

impl KickFlag {
    /// Every flag, in order of precedence.
    ///
    /// It is a `const fn` so that [`PlayerLogin`] sizes its flag slots by it.
    pub const fn all() -> &'static [Self] {
        &[
            Self::Listener,
            Self::ServerFull,
            Self::ServerWhitelisted,
            Self::Banned,
        ]
    }
}

/// The messages of a set kick flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Kick {
    /// For the log.
    pub(crate) reason: String,
    /// For the player's screen.
    pub(crate) screen_message: String,
}

/// Who a player is, from the verified login.
#[derive(Debug, Clone, PartialEq, Eq)]
// api-class: PlayerInfo
pub struct PlayerInfo(pub(crate) Identity);

impl PlayerInfo {
    /// The player's name: the Xbox gamertag when Xbox sign-in is on, otherwise the name the
    /// client chose.
    #[doc(alias = "PlayerInfo::getUsername")]
    pub fn name(&self) -> &str {
        &self.0.name
    }

    /// The identity UUID the client sent.
    #[doc(alias = "PlayerInfo::getUuid")]
    pub fn uuid(&self) -> Uuid {
        self.0.uuid
    }

    /// The Xbox user ID, or `None` when Xbox sign-in (`xbox-auth`) is off.
    #[doc(alias = "XboxLivePlayerInfo::getXuid")]
    pub fn xuid(&self) -> Option<u64> {
        self.0.xuid
    }
}

/// Raised when a player swings at nothing. Cancelling shows no swing.
#[doc(alias = "PlayerMissSwingEvent")]
#[derive(Debug)]
pub struct PlayerMissSwing {
    pub(crate) player: player::Id,
    pub(crate) cancelled: bool,
}

getters!(PlayerMissSwing {
    /// The player.
    player: player::Id,
});

cancellable! {
    PlayerMissSwing
        #[doc(alias = "PlayerMissSwingEvent::cancel")]
        #[doc(alias = "PlayerMissSwingEvent::uncancel")]
        #[doc(alias = "PlayerMissSwingEvent::isCancelled")];
}

/// Raised once a tick for a player who moved, for the whole move since the last one the server
/// accepted.
///
/// The position, the blocks entered and the fall already follow each move packet, so the event
/// comes after them, in the tick's rules; a move under 0.01 blocks raises nothing. Cancelling
/// sends only the position back to the origin.
#[doc(alias = "PlayerMoveEvent")]
#[derive(Debug)]
pub struct PlayerMove {
    pub(crate) player: player::Id,
    pub(crate) origin: Vec3,
    pub(crate) target: Vec3,
    pub(crate) cancelled: bool,
}

getters!(PlayerMove {
    /// The player.
    player: player::Id,
    /// Where the player's feet were.
    #[doc(alias = "PlayerMoveEvent::getFrom")]
    origin: Vec3,
    /// Where the player's feet go.
    #[doc(alias = "PlayerMoveEvent::getTo")]
    target: Vec3,
});

impl PlayerMove {
    /// Moves the player elsewhere instead (the player is teleported there). A position that is
    /// not finite is ignored; one whose eyes lie outside the world border sends the player back,
    /// as a cancel does.
    #[doc(alias = "PlayerMoveEvent::setTo")]
    pub fn set_target(&mut self, target: Vec3) {
        if target.is_finite() {
            self.target = target;
        }
    }
}

cancellable! {
    PlayerMove
        #[doc(alias = "PlayerMoveEvent::cancel")]
        #[doc(alias = "PlayerMoveEvent::uncancel")]
        #[doc(alias = "PlayerMoveEvent::isCancelled")];
}

/// Raised in the view phase of the tick a chunk is picked to be sent to a player, before it goes
/// out. A player who no longer sees the chunk by then raises nothing.
///
/// The chunk's `LevelChunk` goes out at the end of that tick; its blocks follow when the client's
/// sub-chunk requests are answered, so a block packet a listener queues comes before them.
/// [`Context::send_fake_block`](crate::Context::send_fake_block) shows a block that stays.
#[doc(alias = "PlayerPostChunkSendEvent")]
#[derive(Debug)]
pub struct PlayerPostChunkSend {
    pub(crate) player: player::Id,
    pub(crate) chunk: ChunkPos,
}

getters!(PlayerPostChunkSend {
    /// The player.
    player: player::Id,
    /// The chunk sent.
    #[doc(alias = "PlayerPostChunkSendEvent::getChunkX")]
    #[doc(alias = "PlayerPostChunkSendEvent::getChunkZ")]
    chunk: ChunkPos,
});

/// Raised when a player leaves, before the player is saved and removed: what a listener does to
/// the player through the API takes effect before the save (a teleport is the saved place, a kill
/// dies with its drops and [`PlayerDeath`]). Its open window closes after the listeners and
/// monitors, or once the listeners return when one's call closes it (a teleport); a menu's
/// handler hears [`Close`](crate::menu::Close) then. A player that leaves after its saved data
/// loaded but before it spawned raises it too, with no [`PlayerJoin`] before; one still loading
/// raises none.
#[doc(alias = "PlayerQuitEvent")]
#[derive(Debug)]
pub struct PlayerQuit {
    pub(crate) player: player::Id,
    pub(crate) quit_message: String,
    pub(crate) reason: QuitReason,
}

getters!(PlayerQuit {
    /// The player.
    player: player::Id,
    /// The message every other player sees. It starts as `"§e%multiplayer.player.left"` unless a
    /// kick gave another: left so, the client shows its own "left the game" line with the
    /// player's name.
    #[doc(alias = "PlayerQuitEvent::getQuitMessage")]
    quit_message: &str,
    /// Why the player left: what the transport reported for a connection that ended, or the
    /// reason the server itself gave (a kick, a transfer, a replacement, the shutdown).
    #[doc(alias = "PlayerQuitEvent::getQuitReason")]
    reason: &QuitReason,
});

setters!(PlayerQuit {
    /// Replaces the message every other player sees; an empty one shows nothing.
    #[doc(alias = "PlayerQuitEvent::setQuitMessage")]
    set_quit_message => quit_message: String,
});

/// Why a player left, as the log and [`PlayerQuit`] tell it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum QuitReason {
    /// The client closed the connection itself.
    Left,
    /// The connection ended without the client closing it: the session failed (a broken protocol
    /// or limit, a missed login or spawn deadline) or the transport closed it, or could not tell
    /// why.
    ConnectionClosed,
    /// The client stayed silent until its connection timed out.
    TimedOut,
    /// The server dropped a session whose outbound queue was full or closed.
    ConnectionLost,
    /// The player overflowed its output.
    TooManyPackets,
    /// A kick, with the reason it gave.
    Kicked(String),
    /// A transfer to another server, with its message.
    Transferred(String),
    /// A new login replaced the player, with the [`PlayerDuplicateLogin`] disconnect reason.
    Replaced(String),
    /// The server stops: every player still online leaves so once the last tick has run, and
    /// what the listeners change is in the final save.
    Shutdown,
}

impl fmt::Display for QuitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Left => f.write_str("client left"),
            Self::ConnectionClosed => f.write_str("connection closed"),
            Self::TimedOut => f.write_str("timed out"),
            Self::ConnectionLost => f.write_str("connection lost"),
            Self::TooManyPackets => f.write_str("too many packets"),
            Self::Kicked(reason) => write!(f, "kicked: {reason}"),
            Self::Transferred(message) => write!(f, "transferred: {message}"),
            Self::Replaced(reason) => write!(f, "replaced: {reason}"),
            Self::Shutdown => f.write_str("server shutdown"),
        }
    }
}

/// Raised when a dead player respawns, before the player is placed and before its health, food,
/// absorption, effects, fire and air are reset, so a listener's changes to them are replaced. A
/// player that joins dead (it left, or the server stopped, before it respawned) respawns as it
/// joins, before it spawns.
#[doc(alias = "PlayerRespawnEvent")]
#[derive(Debug)]
pub struct PlayerRespawn {
    pub(crate) player: player::Id,
    pub(crate) world: world::Id,
    pub(crate) position: Vec3,
}

getters!(PlayerRespawn {
    /// The player.
    player: player::Id,
    /// The world the player respawns in.
    world: world::Id,
    /// Where the player's feet go.
    #[doc(alias = "PlayerRespawnEvent::getRespawnPosition")]
    position: Vec3,
});

impl PlayerRespawn {
    /// Respawns the player elsewhere.
    ///
    /// A position that is not finite is ignored. A world the server does not run, or a position
    /// outside the world border, is ignored when the player respawns: it respawns at its respawn
    /// point, and monitors see that point.
    #[doc(alias = "PlayerRespawnEvent::setRespawnPosition")]
    pub fn set_position(&mut self, world: world::Id, position: Vec3) {
        if position.is_finite() {
            self.world = world;
            self.position = position;
        }
    }
}

/// Raised when a player starts flying, which only a player who may fly can, or stops flying.
/// Cancelling keeps the old state.
#[doc(alias = "PlayerToggleFlightEvent")]
#[derive(Debug)]
pub struct PlayerToggleFlight {
    pub(crate) player: player::Id,
    pub(crate) flying: bool,
    pub(crate) cancelled: bool,
}

getters!(PlayerToggleFlight {
    /// The player.
    player: player::Id,
});

impl PlayerToggleFlight {
    /// Whether the player starts flying (otherwise stops).
    #[doc(alias = "PlayerToggleFlightEvent::isFlying")]
    pub fn is_flying(&self) -> bool {
        self.flying
    }
}

cancellable! {
    PlayerToggleFlight
        #[doc(alias = "PlayerToggleFlightEvent::cancel")]
        #[doc(alias = "PlayerToggleFlightEvent::uncancel")]
        #[doc(alias = "PlayerToggleFlightEvent::isCancelled")];
}

/// Raised when a player starts or stops gliding. Cancelling keeps the old state.
#[doc(alias = "PlayerToggleGlideEvent")]
#[derive(Debug)]
pub struct PlayerToggleGlide {
    pub(crate) player: player::Id,
    pub(crate) gliding: bool,
    pub(crate) cancelled: bool,
}

getters!(PlayerToggleGlide {
    /// The player.
    player: player::Id,
});

impl PlayerToggleGlide {
    /// Whether the player starts gliding (otherwise stops).
    #[doc(alias = "PlayerToggleGlideEvent::isGliding")]
    pub fn is_gliding(&self) -> bool {
        self.gliding
    }
}

cancellable! {
    PlayerToggleGlide
        #[doc(alias = "PlayerToggleGlideEvent::cancel")]
        #[doc(alias = "PlayerToggleGlideEvent::uncancel")]
        #[doc(alias = "PlayerToggleGlideEvent::isCancelled")];
}

/// Raised when a player starts or stops sneaking. Cancelling keeps the old state.
#[doc(alias = "PlayerToggleSneakEvent")]
#[derive(Debug)]
pub struct PlayerToggleSneak {
    pub(crate) player: player::Id,
    pub(crate) sneaking: bool,
    pub(crate) sneak_pressed: bool,
    pub(crate) cancelled: bool,
}

getters!(PlayerToggleSneak {
    /// The player.
    player: player::Id,
});

impl PlayerToggleSneak {
    /// Whether the sneak key is down (a player can sneak without it, for example while flying
    /// down).
    #[doc(alias = "PlayerToggleSneakEvent::isSneakPressed")]
    pub fn is_sneak_pressed(&self) -> bool {
        self.sneak_pressed
    }

    /// Whether the player starts sneaking (otherwise stops).
    #[doc(alias = "PlayerToggleSneakEvent::isSneaking")]
    pub fn is_sneaking(&self) -> bool {
        self.sneaking
    }
}

cancellable! {
    PlayerToggleSneak
        #[doc(alias = "PlayerToggleSneakEvent::cancel")]
        #[doc(alias = "PlayerToggleSneakEvent::uncancel")]
        #[doc(alias = "PlayerToggleSneakEvent::isCancelled")];
}

/// Raised when a player starts or stops sprinting. Cancelling keeps the old state.
#[doc(alias = "PlayerToggleSprintEvent")]
#[derive(Debug)]
pub struct PlayerToggleSprint {
    pub(crate) player: player::Id,
    pub(crate) sprinting: bool,
    pub(crate) cancelled: bool,
}

getters!(PlayerToggleSprint {
    /// The player.
    player: player::Id,
});

impl PlayerToggleSprint {
    /// Whether the player starts sprinting (otherwise stops).
    #[doc(alias = "PlayerToggleSprintEvent::isSprinting")]
    pub fn is_sprinting(&self) -> bool {
        self.sprinting
    }
}

cancellable! {
    PlayerToggleSprint
        #[doc(alias = "PlayerToggleSprintEvent::cancel")]
        #[doc(alias = "PlayerToggleSprintEvent::uncancel")]
        #[doc(alias = "PlayerToggleSprintEvent::isCancelled")];
}

/// Raised when a player starts or stops swimming. Cancelling keeps the old state.
#[doc(alias = "PlayerToggleSwimEvent")]
#[derive(Debug)]
pub struct PlayerToggleSwim {
    pub(crate) player: player::Id,
    pub(crate) swimming: bool,
    pub(crate) cancelled: bool,
}

getters!(PlayerToggleSwim {
    /// The player.
    player: player::Id,
});

impl PlayerToggleSwim {
    /// Whether the player starts swimming (otherwise stops).
    #[doc(alias = "PlayerToggleSwimEvent::isSwimming")]
    pub fn is_swimming(&self) -> bool {
        self.swimming
    }
}

cancellable! {
    PlayerToggleSwim
        #[doc(alias = "PlayerToggleSwimEvent::cancel")]
        #[doc(alias = "PlayerToggleSwimEvent::uncancel")]
        #[doc(alias = "PlayerToggleSwimEvent::isCancelled")];
}

/// Raised when a player is about to be sent to another server. Cancelling keeps the player here,
/// and so does an empty address or one longer than 253 bytes, or a message longer than
/// [`player::MAX_TEXT_BYTES`] bytes.
#[doc(alias = "PlayerTransferEvent")]
#[derive(Debug)]
pub struct PlayerTransfer {
    pub(crate) player: player::Id,
    pub(crate) address: String,
    pub(crate) port: u16,
    pub(crate) message: String,
    pub(crate) cancelled: bool,
}

getters!(PlayerTransfer {
    /// The player.
    player: player::Id,
    /// The other server's address: a host name or an IP address.
    #[doc(alias = "PlayerTransferEvent::getAddress")]
    address: &str,
    /// The other server's port.
    #[doc(alias = "PlayerTransferEvent::getPort")]
    port: u16,
    /// The message for the log and [`QuitReason::Transferred`].
    #[doc(alias = "PlayerTransferEvent::getMessage")]
    message: &str,
});

setters!(PlayerTransfer {
    /// Sends the player to another address.
    #[doc(alias = "PlayerTransferEvent::setAddress")]
    set_address => address: String,
    /// Sends the player to another port.
    #[doc(alias = "PlayerTransferEvent::setPort")]
    set_port => port: u16,
    /// Replaces the message for the log and [`QuitReason::Transferred`].
    #[doc(alias = "PlayerTransferEvent::setMessage")]
    set_message => message: String,
});

cancellable! {
    PlayerTransfer
        #[doc(alias = "PlayerTransferEvent::cancel")]
        #[doc(alias = "PlayerTransferEvent::uncancel")]
        #[doc(alias = "PlayerTransferEvent::isCancelled")];
}

/// Raised after a player's view distance changed (the client asked for another one).
#[doc(alias = "PlayerViewDistanceChangeEvent")]
#[derive(Debug)]
pub struct PlayerViewDistanceChange {
    pub(crate) player: player::Id,
    pub(crate) old_distance: Option<u8>,
    pub(crate) new_distance: u8,
}

getters!(PlayerViewDistanceChange {
    /// The player.
    player: player::Id,
    /// The view distance before, in chunks; `None` before the client first asked.
    #[doc(alias = "PlayerViewDistanceChangeEvent::getOldDistance")]
    old_distance: Option<u8>,
    /// The view distance after, in chunks.
    #[doc(alias = "PlayerViewDistanceChangeEvent::getNewDistance")]
    new_distance: u8,
});

/// Raised after a player moved into another world, once per move, when the rule that moved it is
/// done: a teleport the API or an action marked, at the end of the tick; an ender pearl, after its
/// fall damage; a respawn, after the player's health, food, effects, fire and air are reset, so a
/// listener's changes to them stay; an unload of its world, which sends it to the default world's
/// spawn (`old_world` then names a world no longer loaded). Listeners see the player in the new
/// world: [`Player::world`](crate::player::Player::world) is `new_world` unless a teleport that
/// waits for the end of the tick is marked. The new world's chunks are sent later in the tick.
/// When it is raised at the end of the tick (a teleport, a respawn
/// [`Player::respawn`](crate::player::Player::respawn) asked for, an unload), a teleport or kick
/// its listeners ask for the player is applied at the next tick's end. A player moved after it
/// joined and before it spawned raises it before [`PlayerJoin`]. Joining, a move within one
/// world, and a move applied as the player leaves (after its [`PlayerQuit`]) raise nothing.
#[derive(Debug)]
pub struct PlayerWorldChange {
    pub(crate) player: player::Id,
    pub(crate) old_world: world::Id,
    pub(crate) new_world: world::Id,
}

getters!(PlayerWorldChange {
    /// The player.
    player: player::Id,
    /// The world the player left.
    old_world: world::Id,
    /// The world the player is in now.
    new_world: world::Id,
});

/// Raised when a player picks up XP orbs, before the points are added to the level and
/// progress. Cancelling keeps both. The reset on death ([`PlayerDeath::keeps_xp`] decides it) and
/// the points [`Player::add_xp`](crate::player::Player::add_xp) adds raise nothing.
#[doc(alias = "PlayerExperienceChangeEvent")]
#[derive(Debug)]
pub struct PlayerXpChange {
    pub(crate) player: player::Id,
    pub(crate) old_level: u32,
    pub(crate) old_progress: f64,
    pub(crate) new_level: Option<u32>,
    pub(crate) new_progress: Option<f64>,
    pub(crate) cancelled: bool,
}

getters!(PlayerXpChange {
    /// The player.
    player: player::Id,
    /// The level before.
    #[doc(alias = "PlayerExperienceChangeEvent::getOldLevel")]
    old_level: u32,
    /// The progress to the next level before, 0 to 1.
    #[doc(alias = "PlayerExperienceChangeEvent::getOldProgress")]
    old_progress: f64,
    /// The level after; `None` keeps the level.
    #[doc(alias = "PlayerExperienceChangeEvent::getNewLevel")]
    new_level: Option<u32>,
    /// The progress after; `None` keeps the progress.
    #[doc(alias = "PlayerExperienceChangeEvent::getNewProgress")]
    new_progress: Option<f64>,
});

impl PlayerXpChange {
    /// Sets the level after, limited to the highest level (24,791); `None` keeps the level.
    #[doc(alias = "PlayerExperienceChangeEvent::setNewLevel")]
    pub fn set_new_level(&mut self, level: Option<u32>) {
        self.new_level = level.map(|l| l.min(player::MAX_LEVEL));
    }

    /// Sets the progress after, limited to 0 through just below a full bar (a full bar is the
    /// next level); `None` keeps the progress. A value that is not finite is ignored.
    #[doc(alias = "PlayerExperienceChangeEvent::setNewProgress")]
    pub fn set_new_progress(&mut self, progress: Option<f64>) {
        match progress {
            Some(p) if !p.is_finite() => {}
            Some(p) => self.new_progress = Some(math::clamp(p, 0.0, player::MAX_PROGRESS)),
            None => self.new_progress = None,
        }
    }
}

cancellable! {
    PlayerXpChange
        #[doc(alias = "PlayerExperienceChangeEvent::cancel")]
        #[doc(alias = "PlayerExperienceChangeEvent::uncancel")]
        #[doc(alias = "PlayerExperienceChangeEvent::isCancelled")];
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::block::DIRT;
    use crate::event::fixtures::{POS, entity, player, sticks};

    /// A position and vector the events carry.
    const AT: Vec3 = Vec3::new(0.5, 65.0, -1.5);
    /// The world of the events.
    const WORLD: world::Id = world::Id::DEFAULT;

    /// An identity named `name`, without an XUID or an address.
    fn identity(name: &str) -> Identity {
        Identity {
            name: name.to_owned(),
            xuid: None,
            uuid: Uuid::from_bytes([1; 16]),
            skin: Skin::default(),
            device_os: -1,
            address: None,
            client: crate::tick::queue::Client::default(),
        }
    }

    // api: PlayerInfo::getUsername, XboxLivePlayerInfo::getXuid, PlayerInfo::getUuid,
    // api: PlayerInfo
    #[test]
    fn player_info_reads_the_identity() {
        let mut id = identity("Steve");
        id.xuid = Some(42);
        let info = PlayerInfo(id);
        assert_eq!(
            (info.name(), info.xuid(), info.uuid()),
            ("Steve", Some(42), Uuid::from_bytes([1; 16]))
        );
    }

    // api: PlayerBlockPickEvent, PlayerBlockPickEvent::getBlock,
    // api: PlayerBlockPickEvent::getResultItem, PlayerBlockPickEvent::cancel,
    // api: PlayerBlockPickEvent::uncancel, PlayerBlockPickEvent::isCancelled
    #[test]
    fn block_pick_names_the_item() {
        let mut e = PlayerBlockPick {
            player: player(1),
            world: WORLD,
            pos: POS,
            block: DIRT,
            result_item: sticks(1),
            cancelled: false,
        };
        assert_eq!(
            (e.player(), e.world(), e.pos(), e.block()),
            (player(1), WORLD, POS, DIRT)
        );
        assert_eq!(e.result_item(), &sticks(1));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: PlayerChangeSkinEvent, PlayerChangeSkinEvent::getOldSkin,
    // api: PlayerChangeSkinEvent::getNewSkin, PlayerChangeSkinEvent::setNewSkin,
    // api: PlayerChangeSkinEvent::cancel, PlayerChangeSkinEvent::uncancel,
    // api: PlayerChangeSkinEvent::isCancelled
    #[test]
    fn change_skin_new_skin_can_change() {
        let mut e = PlayerChangeSkin {
            player: player(1),
            old_skin: Skin::default(),
            new_skin: Skin::default(),
            cancelled: false,
        };
        assert_eq!(e.player(), player(1));
        assert_eq!(e.old_skin(), e.new_skin());
        let skin = protocol::Skin {
            skin_id: "other".to_owned(),
            ..Skin::default().get().clone()
        };
        e.set_new_skin(skin);
        assert_eq!(e.new_skin().skin_id, "other");
        assert_eq!(e.old_skin().skin_id, "Standard_Custom");
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: PlayerChatEvent, PlayerChatEvent::getMessage, PlayerChatEvent::setMessage,
    // api: PlayerChatEvent::setPlayer, PlayerChatEvent::getRecipients,
    // api: PlayerChatEvent::setRecipients, PlayerChatEvent::getFormatter,
    // api: PlayerChatEvent::setFormatter, PlayerChatEvent::cancel,
    // api: PlayerChatEvent::uncancel, PlayerChatEvent::isCancelled
    #[test]
    fn chat_message_speaker_recipients_and_format_can_change() {
        let mut e = PlayerChat {
            player: player(1),
            message: "hi".to_owned(),
            recipients: vec![player(1), player(2)],
            format: None,
            cancelled: false,
        };
        assert_eq!(
            (e.player(), e.message(), e.format()),
            (player(1), "hi", None)
        );
        assert_eq!(e.recipients(), &[player(1), player(2)]);
        e.set_player(player(2));
        e.set_message("bye".to_owned());
        e.set_recipients(vec![player(3)]);
        e.set_format(Some("[{%0}] {%1}".to_owned()));
        assert_eq!((e.player(), e.message()), (player(2), "bye"));
        assert_eq!(e.recipients(), &[player(3)]);
        assert_eq!(e.format(), Some("[{%0}] {%1}"));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: PlayerDataSaveEvent, PlayerDataSaveEvent::getPlayer,
    // api: PlayerDataSaveEvent::getPlayerName, PlayerDataSaveEvent::cancel,
    // api: PlayerDataSaveEvent::uncancel, PlayerDataSaveEvent::isCancelled
    #[test]
    fn data_save_names_the_record() {
        let mut e = PlayerDataSave {
            player: player(1),
            record_key: "offline:steve".to_owned(),
            cancelled: false,
        };
        assert_eq!((e.player(), e.record_key()), (player(1), "offline:steve"));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: PlayerDeathEvent, PlayerDeathEvent::getPlayer, PlayerDeathEvent::getEntity,
    // api: PlayerDeathEvent::getDeathMessage, PlayerDeathEvent::setDeathMessage,
    // api: PlayerDeathEvent::getDeathScreenMessage, PlayerDeathEvent::setDeathScreenMessage,
    // api: PlayerDeathEvent::getKeepInventory, PlayerDeathEvent::setKeepInventory,
    // api: PlayerDeathEvent::getKeepXp, PlayerDeathEvent::setKeepXp
    #[test]
    fn death_drops_messages_and_keeping_can_change() {
        let mut e = PlayerDeath {
            player: player(1),
            drops: vec![sticks(5)],
            xp_drop_amount: 7,
            death_message: "Steve died".to_owned(),
            death_screen_message: "Steve died".to_owned(),
            keep_inventory: false,
            keep_xp: false,
        };
        assert_eq!(e.player(), player(1));
        assert_eq!((e.drops(), e.xp_drop_amount()), ([sticks(5)].as_slice(), 7));
        assert_eq!(
            (e.death_message(), e.death_screen_message()),
            ("Steve died", "Steve died")
        );
        assert!(!e.keeps_inventory() && !e.keeps_xp());
        e.set_drops(Vec::new());
        e.set_xp_drop_amount(0);
        e.set_death_message(String::new());
        e.set_death_screen_message("You fell".to_owned());
        e.set_keep_inventory(true);
        e.set_keep_xp(true);
        assert_eq!((e.drops(), e.xp_drop_amount()), ([].as_slice(), 0));
        assert_eq!(
            (e.death_message(), e.death_screen_message()),
            ("", "You fell")
        );
        assert!(e.keeps_inventory() && e.keeps_xp());
    }

    // api: PlayerDropItemEvent, PlayerDropItemEvent::getItem, PlayerDropItemEvent::cancel,
    // api: PlayerDropItemEvent::uncancel, PlayerDropItemEvent::isCancelled
    #[test]
    fn drop_item_names_the_item() {
        let mut e = PlayerDropItem {
            player: player(1),
            item: sticks(3),
            cancelled: false,
        };
        assert_eq!((e.player(), e.item()), (player(1), &sticks(3)));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: PlayerDuplicateLoginEvent, PlayerDuplicateLoginEvent::getConnectingSession,
    // api: PlayerDuplicateLoginEvent::getExistingSession,
    // api: PlayerDuplicateLoginEvent::getDisconnectReason,
    // api: PlayerDuplicateLoginEvent::setDisconnectReason,
    // api: PlayerDuplicateLoginEvent::getDisconnectScreenMessage,
    // api: PlayerDuplicateLoginEvent::setDisconnectScreenMessage,
    // api: PlayerDuplicateLoginEvent::cancel, PlayerDuplicateLoginEvent::uncancel,
    // api: PlayerDuplicateLoginEvent::isCancelled, CancellableTrait::cancel,
    // api: CancellableTrait::uncancel, CancellableTrait::isCancelled,
    // api: Cancellable::isCancelled
    #[test]
    fn the_outcome_uses_the_screen_message_before_the_reason() {
        let event = PlayerDuplicateLogin::new(identity("a"), identity("A"));
        assert_eq!(event.connecting().name(), "a");
        assert_eq!(event.existing().name(), "A");
        assert_eq!(event.kick_screen_message(), None);
        let (connecting, outcome) = event.clone().into_outcome();
        assert_eq!(connecting, identity("a"));
        assert_eq!(outcome, DuplicateOutcome::unheard());
        let mut event = event;
        event.set_kick_reason("reason");
        assert_eq!(event.kick_reason(), "reason");
        assert_eq!(event.into_outcome().1.message, "reason");
        let mut event = PlayerDuplicateLogin::new(identity("a"), identity("A"));
        event.set_kick_reason("reason");
        event.set_kick_screen_message(Some("screen".to_owned()));
        assert_eq!(event.kick_screen_message(), Some("screen"));
        event.cancel();
        assert!(event.is_cancelled());
        event.uncancel();
        assert!(!event.is_cancelled());
        event.cancel();
        let outcome = event.into_outcome().1;
        assert!(outcome.cancelled);
        assert_eq!(outcome.message, "screen");
    }

    // api: PlayerEmoteEvent, PlayerEmoteEvent::getEmoteId, PlayerEmoteEvent::setEmoteId,
    // api: PlayerEmoteEvent::cancel, PlayerEmoteEvent::uncancel, PlayerEmoteEvent::isCancelled
    #[test]
    fn emote_can_change() {
        let mut e = PlayerEmote {
            player: player(1),
            emote_id: "a".to_owned(),
            cancelled: false,
        };
        assert_eq!((e.player(), e.emote_id()), (player(1), "a"));
        e.set_emote_id("b".to_owned());
        assert_eq!(e.emote_id(), "b");
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: PlayerEntityInteractEvent, PlayerEntityInteractEvent::getEntity,
    // api: PlayerEntityInteractEvent::getClickPosition, PlayerEntityInteractEvent::cancel,
    // api: PlayerEntityInteractEvent::uncancel, PlayerEntityInteractEvent::isCancelled
    #[test]
    fn entity_interact_names_the_entity() {
        let mut e = PlayerEntityInteract {
            player: player(1),
            entity: entity(2),
            click_position: AT,
            cancelled: false,
        };
        assert_eq!(
            (e.player(), e.entity(), e.click_position()),
            (player(1), entity(2), AT)
        );
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: PlayerExhaustEvent, PlayerExhaustEvent::getPlayer, PlayerExhaustEvent::getAmount,
    // api: PlayerExhaustEvent::setAmount, PlayerExhaustEvent::getCause,
    // api: PlayerExhaustEvent::CAUSE_ATTACK, PlayerExhaustEvent::CAUSE_DAMAGE,
    // api: PlayerExhaustEvent::CAUSE_MINING, PlayerExhaustEvent::CAUSE_HEALTH_REGEN,
    // api: PlayerExhaustEvent::CAUSE_POTION, PlayerExhaustEvent::CAUSE_WALKING,
    // api: PlayerExhaustEvent::CAUSE_SPRINTING, PlayerExhaustEvent::CAUSE_JUMPING,
    // api: PlayerExhaustEvent::CAUSE_SPRINT_JUMPING, PlayerExhaustEvent::cancel,
    // api: PlayerExhaustEvent::uncancel, PlayerExhaustEvent::isCancelled
    #[test]
    fn exhaust_amount_can_change() {
        use ExhaustCause as C;
        let causes = [
            C::Attack,
            C::Damage,
            C::Mining,
            C::HealthRegen,
            C::Potion,
            C::Walking,
            C::Sprinting,
            C::Jumping,
            C::SprintJumping,
        ];
        for cause in causes {
            let mut e = PlayerExhaust {
                player: player(1),
                amount: 0.1,
                cause,
                cancelled: false,
            };
            assert_eq!((e.player(), e.amount(), e.cause()), (player(1), 0.1, cause));
            e.set_amount(0.5);
            e.set_amount(f64::INFINITY);
            assert_eq!(e.amount(), 0.5);
            e.set_amount(-1.0);
            assert_eq!(e.amount(), 0.0);
            e.cancel();
            assert!(e.is_cancelled());
            e.uncancel();
            assert!(!e.is_cancelled());
        }
    }

    // api: PlayerGameModeChangeEvent,PlayerGameModeChangeEvent::getNewGamemode,
    // api: PlayerGameModeChangeEvent::cancel, PlayerGameModeChangeEvent::uncancel,
    // api: PlayerGameModeChangeEvent::isCancelled
    #[test]
    fn game_mode_change_names_the_new_mode() {
        let mut e = PlayerGameModeChange {
            player: player(1),
            new_game_mode: GameMode::Creative,
            cancelled: false,
        };
        assert_eq!(
            (e.player(), e.new_game_mode()),
            (player(1), GameMode::Creative)
        );
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: PlayerInteractEvent, PlayerInteractEvent::getBlock, PlayerInteractEvent::getFace,
    // api: PlayerInteractEvent::getItem, PlayerInteractEvent::getTouchVector,
    // api: PlayerInteractEvent::getAction, PlayerInteractEvent::useItem,
    // api: PlayerInteractEvent::useBlock, PlayerInteractEvent::setUseItem,
    // api: PlayerInteractEvent::setUseBlock, PlayerInteractEvent::LEFT_CLICK_BLOCK,
    // api: PlayerInteractEvent::RIGHT_CLICK_BLOCK, PlayerInteractEvent::cancel,
    // api: PlayerInteractEvent::uncancel, PlayerInteractEvent::isCancelled
    #[test]
    fn interact_can_stop_the_item_or_the_block() {
        for action in [
            InteractAction::LeftClickBlock,
            InteractAction::RightClickBlock,
        ] {
            let mut e = PlayerInteract {
                player: player(1),
                world: WORLD,
                pos: POS,
                block: DIRT,
                face: Face::East,
                item: sticks(1),
                touch_vector: AT,
                action,
                first: true,
                use_item: true,
                use_block: true,
                cancelled: false,
            };
            assert_eq!(
                (e.player(), e.world(), e.pos(), e.block(), e.face()),
                (player(1), WORLD, POS, DIRT, Face::East)
            );
            assert_eq!(
                (e.item(), e.touch_vector(), e.action(), e.is_first()),
                (&sticks(1), AT, action, true)
            );
            assert!(e.uses_item() && e.uses_block());
            e.set_use_item(false);
            e.set_use_block(false);
            assert!(!e.uses_item() && !e.uses_block());
            e.cancel();
            assert!(e.is_cancelled());
            e.uncancel();
            assert!(!e.is_cancelled());
        }
    }

    // api: PlayerItemConsumeEvent, PlayerItemConsumeEvent::getItem,
    // api: PlayerItemConsumeEvent::getResidue, PlayerItemConsumeEvent::setResidue,
    // api: PlayerItemConsumeEvent::cancel, PlayerItemConsumeEvent::uncancel,
    // api: PlayerItemConsumeEvent::isCancelled
    #[test]
    fn item_consume_residue_can_change() {
        let mut e = PlayerItemConsume {
            player: player(1),
            item: sticks(1),
            residue: vec![sticks(2)],
            cancelled: false,
        };
        assert_eq!((e.player(), e.item()), (player(1), &sticks(1)));
        assert_eq!(e.residue(), &[sticks(2)]);
        e.set_residue(Vec::new());
        assert!(e.residue().is_empty());
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    /// The residue and drops a listener sets show only the stacks that are kept.
    #[test]
    fn residue_and_drops_show_what_is_kept() {
        let mut consume = PlayerItemConsume {
            player: player(1),
            item: sticks(1),
            residue: Vec::new(),
            cancelled: false,
        };
        consume.set_residue(vec![
            sticks(0),
            sticks(1),
            sticks(2),
            sticks(3),
            sticks(4),
            sticks(5),
        ]);
        assert_eq!(
            consume.residue(),
            &[sticks(1), sticks(2), sticks(3), sticks(4)]
        );
        let mut death = PlayerDeath {
            player: player(1),
            drops: Vec::new(),
            xp_drop_amount: 0,
            death_message: String::new(),
            death_screen_message: String::new(),
            keep_inventory: false,
            keep_xp: false,
        };
        death.set_drops(vec![sticks(0), sticks(200)]);
        assert_eq!(death.drops(), &[sticks(64)]);
    }

    // api: PlayerItemHeldEvent, PlayerItemUseEvent, PlayerItemHeldEvent::getItem,
    // api: PlayerItemHeldEvent::getSlot, PlayerItemHeldEvent::cancel,
    // api: PlayerItemHeldEvent::uncancel, PlayerItemHeldEvent::isCancelled,
    // api: PlayerItemUseEvent::getItem, PlayerItemUseEvent::getDirectionVector,
    // api: PlayerItemUseEvent::cancel, PlayerItemUseEvent::uncancel,
    // api: PlayerItemUseEvent::isCancelled
    #[test]
    fn item_held_and_use_name_the_item() {
        let mut held = PlayerItemHeld {
            player: player(1),
            item: sticks(1),
            slot: 8,
            cancelled: false,
        };
        assert_eq!(
            (held.player(), held.item(), held.slot()),
            (player(1), &sticks(1), 8)
        );
        held.cancel();
        assert!(held.is_cancelled());
        held.uncancel();
        assert!(!held.is_cancelled());
        let mut used = PlayerItemUse {
            player: player(1),
            item: sticks(1),
            direction_vector: AT,
            cancelled: false,
        };
        assert_eq!(
            (used.player(), used.item(), used.direction_vector()),
            (player(1), &sticks(1), AT)
        );
        used.cancel();
        assert!(used.is_cancelled());
        used.uncancel();
        assert!(!used.is_cancelled());
    }

    // api: PlayerJumpEvent, PlayerJoinEvent, PlayerQuitEvent, PlayerEvent::getPlayer,
    // api: PlayerJoinEvent::getJoinMessage, PlayerJoinEvent::setJoinMessage,
    // api: PlayerQuitEvent::getQuitMessage, PlayerQuitEvent::setQuitMessage,
    // api: PlayerQuitEvent::getQuitReason
    #[test]
    fn join_and_quit_messages_can_change() {
        let mut join = PlayerJoin {
            player: player(1),
            join_message: "joined".to_owned(),
        };
        assert_eq!((join.player(), join.join_message()), (player(1), "joined"));
        join.set_join_message(String::new());
        assert_eq!(join.join_message(), "");
        let mut quit = PlayerQuit {
            player: player(1),
            quit_message: "left".to_owned(),
            reason: QuitReason::TimedOut,
        };
        assert_eq!(
            (quit.player(), quit.quit_message(), quit.reason()),
            (player(1), "left", &QuitReason::TimedOut)
        );
        quit.set_quit_message("gone".to_owned());
        assert_eq!(quit.quit_message(), "gone");
        assert_eq!(PlayerJump { player: player(2) }.player(), player(2));
    }

    // api: PlayerKickEvent, PlayerKickEvent::getDisconnectReason,
    // api: PlayerDisconnectEventTrait::getDisconnectReason,
    // api: PlayerKickEvent::setDisconnectReason,
    // api: PlayerDisconnectEventTrait::setDisconnectReason,
    // api: PlayerKickEvent::getDisconnectScreenMessage,
    // api: PlayerDisconnectEventTrait::getDisconnectScreenMessage,
    // api: PlayerKickEvent::setDisconnectScreenMessage,
    // api: PlayerDisconnectEventTrait::setDisconnectScreenMessage,
    // api: PlayerKickEvent::getQuitMessage, PlayerKickEvent::setQuitMessage,
    // api: PlayerKickEvent::cancel, PlayerKickEvent::uncancel, PlayerKickEvent::isCancelled
    #[test]
    fn kick_screen_message_falls_back_to_the_reason() {
        let mut e = PlayerKick {
            player: player(1),
            kick_reason: "spam".to_owned(),
            screen_message: None,
            quit_message: "kicked".to_owned(),
            cancelled: false,
        };
        assert_eq!((e.player(), e.kick_reason()), (player(1), "spam"));
        assert_eq!(
            (e.kick_screen_message(), e.quit_message()),
            ("spam", "kicked")
        );
        e.set_kick_reason("flood".to_owned());
        e.set_quit_message(String::new());
        assert_eq!((e.kick_screen_message(), e.quit_message()), ("flood", ""));
        e.set_kick_screen_message(Some("Bye".to_owned()));
        assert_eq!(e.kick_screen_message(), "Bye");
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: PlayerLoginEvent, PlayerPreLoginEvent, PlayerPreLoginEvent::getPlayerInfo,
    // api: PlayerPreLoginEvent::getIp, PlayerPreLoginEvent::getPort,
    // api: PlayerPreLoginEvent::getKickFlags, PlayerPreLoginEvent::isKickFlagSet,
    // api: PlayerPreLoginEvent::setKickFlag, PlayerPreLoginEvent::clearKickFlag,
    // api: PlayerPreLoginEvent::clearAllKickFlags, PlayerPreLoginEvent::isAllowed,
    // api: PlayerPreLoginEvent::getDisconnectReason,
    // api: PlayerPreLoginEvent::getDisconnectScreenMessage,
    // api: PlayerPreLoginEvent::getFinalDisconnectReason,
    // api: PlayerPreLoginEvent::getFinalDisconnectScreenMessage,
    // api: PlayerPreLoginEvent::KICK_FLAG_PRIORITY, PlayerPreLoginEvent::KICK_FLAG_PLUGIN,
    // api: PlayerPreLoginEvent::KICK_FLAG_SERVER_FULL,
    // api: PlayerPreLoginEvent::KICK_FLAG_SERVER_WHITELISTED,
    // api: PlayerPreLoginEvent::KICK_FLAG_BANNED
    #[test]
    fn login_refuses_with_the_first_kick_flag() {
        let ip: IpAddr = "192.0.2.1".parse().expect("a valid IP address");
        let mut e = PlayerLogin {
            info: PlayerInfo(identity("Steve")),
            ip: Some(ip),
            port: 19_132,
            kicks: Default::default(),
        };
        assert_eq!(e.info().name(), "Steve");
        assert_eq!((e.ip(), e.port()), (Some(ip), 19_132));
        assert!(e.is_allowed());
        assert_eq!(e.final_kick_reason(), None);
        e.set_kick_flag(
            KickFlag::Banned,
            "banned",
            Some("You are banned".to_owned()),
        );
        e.set_kick_flag(KickFlag::ServerFull, "full", None);
        assert!(!e.is_allowed());
        assert!(e.is_kick_flag_set(KickFlag::Banned));
        assert!(!e.is_kick_flag_set(KickFlag::Listener));
        assert_eq!(
            e.kick_flags().collect::<Vec<_>>(),
            [KickFlag::ServerFull, KickFlag::Banned]
        );
        assert_eq!(e.kick_reason(KickFlag::Banned), Some("banned"));
        assert_eq!(
            e.kick_screen_message(KickFlag::Banned),
            Some("You are banned")
        );
        assert_eq!(e.kick_screen_message(KickFlag::ServerFull), Some("full"));
        assert_eq!(e.kick_reason(KickFlag::ServerWhitelisted), None);
        assert_eq!(e.final_kick_reason(), Some("full"));
        assert_eq!(e.final_kick_screen_message(), Some("full"));
        e.clear_kick_flag(KickFlag::ServerFull);
        assert_eq!(e.final_kick_screen_message(), Some("You are banned"));
        e.set_kick_flag(KickFlag::Listener, "no", None);
        assert_eq!(e.final_kick_reason(), Some("no"));
        e.clear_all_kick_flags();
        assert!(e.is_allowed());
        assert_eq!(e.kick_flags().count(), 0);
        let mut sorted = KickFlag::all().to_vec();
        sorted.sort();
        assert_eq!(sorted, KickFlag::all());
    }

    // api: PlayerMissSwingEvent, PlayerMoveEvent, PlayerMissSwingEvent::cancel,
    // api: PlayerMissSwingEvent::uncancel, PlayerMissSwingEvent::isCancelled,
    // api: PlayerMoveEvent::getFrom, PlayerMoveEvent::getTo, PlayerMoveEvent::setTo,
    // api: PlayerMoveEvent::cancel, PlayerMoveEvent::uncancel, PlayerMoveEvent::isCancelled
    #[test]
    fn move_target_can_change() {
        let mut swing = PlayerMissSwing {
            player: player(1),
            cancelled: false,
        };
        assert_eq!(swing.player(), player(1));
        swing.cancel();
        assert!(swing.is_cancelled());
        swing.uncancel();
        assert!(!swing.is_cancelled());
        let mut e = PlayerMove {
            player: player(1),
            origin: Vec3::ZERO,
            target: AT,
            cancelled: false,
        };
        assert_eq!(
            (e.player(), e.origin(), e.target()),
            (player(1), Vec3::ZERO, AT)
        );
        e.set_target(Vec3::new(1.0, 2.0, 3.0));
        e.set_target(Vec3::new(0.0, f64::INFINITY, 0.0));
        assert_eq!(e.target(), Vec3::new(1.0, 2.0, 3.0));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: PlayerPostChunkSendEvent, PlayerViewDistanceChangeEvent,
    // api: PlayerPostChunkSendEvent::getChunkX, PlayerPostChunkSendEvent::getChunkZ,
    // api: PlayerViewDistanceChangeEvent::getOldDistance,
    // api: PlayerViewDistanceChangeEvent::getNewDistance
    #[test]
    fn chunk_send_and_view_distance_name_their_values() {
        let sent = PlayerPostChunkSend {
            player: player(1),
            chunk: ChunkPos::new(2, -3),
        };
        assert_eq!(
            (sent.player(), sent.chunk()),
            (player(1), ChunkPos::new(2, -3))
        );
        let view = PlayerViewDistanceChange {
            player: player(1),
            old_distance: None,
            new_distance: 8,
        };
        assert_eq!(
            (view.player(), view.old_distance(), view.new_distance()),
            (player(1), None, 8)
        );
    }

    #[test]
    fn world_change_names_the_player_and_both_worlds() {
        let arena = world::Id::of(1);
        let e = PlayerWorldChange {
            player: player(1),
            old_world: WORLD,
            new_world: arena,
        };
        assert_eq!(
            (e.player(), e.old_world(), e.new_world()),
            (player(1), WORLD, arena)
        );
    }

    // api: PlayerExperienceChangeEvent, PlayerExperienceChangeEvent::getOldLevel,
    // api: PlayerExperienceChangeEvent::getOldProgress,
    // api: PlayerExperienceChangeEvent::getNewLevel,
    // api: PlayerExperienceChangeEvent::getNewProgress,
    // api: PlayerExperienceChangeEvent::setNewLevel,
    // api: PlayerExperienceChangeEvent::setNewProgress, PlayerExperienceChangeEvent::cancel,
    // api: PlayerExperienceChangeEvent::uncancel, PlayerExperienceChangeEvent::isCancelled
    #[test]
    fn xp_change_keeps_progress_in_range() {
        let mut e = PlayerXpChange {
            player: player(1),
            old_level: 3,
            old_progress: 0.5,
            new_level: Some(4),
            new_progress: None,
            cancelled: false,
        };
        assert_eq!(
            (e.player(), e.old_level(), e.old_progress()),
            (player(1), 3, 0.5)
        );
        assert_eq!((e.new_level(), e.new_progress()), (Some(4), None));
        e.set_new_level(Some(u32::MAX));
        assert_eq!(e.new_level(), Some(player::MAX_LEVEL));
        e.set_new_level(None);
        e.set_new_progress(Some(2.0));
        assert_eq!(e.new_level(), None);
        assert!(
            e.new_progress().is_some_and(|p| p < 1.0),
            "below a full bar"
        );
        let full = e.new_progress();
        e.set_new_progress(Some(f64::NAN));
        assert_eq!(e.new_progress(), full);
        e.set_new_progress(Some(0.5));
        e.set_new_progress(Some(f64::INFINITY));
        e.set_new_progress(Some(f64::NEG_INFINITY));
        assert_eq!(e.new_progress(), Some(0.5));
        e.set_new_progress(None);
        assert_eq!(e.new_progress(), None);
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: PlayerRespawnEvent, PlayerRespawnEvent::getRespawnPosition,
    // api: PlayerRespawnEvent::setRespawnPosition
    #[test]
    fn respawn_position_can_change() {
        let mut e = PlayerRespawn {
            player: player(1),
            world: WORLD,
            position: AT,
        };
        assert_eq!(
            (e.player(), e.world(), e.position()),
            (player(1), WORLD, AT)
        );
        e.set_position(WORLD, Vec3::ZERO);
        e.set_position(WORLD, Vec3::new(f64::NAN, 0.0, 0.0));
        assert_eq!(e.position(), Vec3::ZERO);
    }

    // api: PlayerToggleFlightEvent, PlayerToggleGlideEvent, PlayerToggleSneakEvent,
    // api: PlayerToggleSprintEvent, PlayerToggleSwimEvent, PlayerToggleFlightEvent::isFlying,
    // api: PlayerToggleFlightEvent::cancel, PlayerToggleFlightEvent::uncancel,
    // api: PlayerToggleFlightEvent::isCancelled, PlayerToggleGlideEvent::isGliding,
    // api: PlayerToggleGlideEvent::cancel, PlayerToggleGlideEvent::uncancel,
    // api: PlayerToggleGlideEvent::isCancelled, PlayerToggleSneakEvent::isSneaking,
    // api: PlayerToggleSneakEvent::isSneakPressed, PlayerToggleSneakEvent::cancel,
    // api: PlayerToggleSneakEvent::uncancel, PlayerToggleSneakEvent::isCancelled,
    // api: PlayerToggleSprintEvent::isSprinting, PlayerToggleSprintEvent::cancel,
    // api: PlayerToggleSprintEvent::uncancel, PlayerToggleSprintEvent::isCancelled,
    // api: PlayerToggleSwimEvent::isSwimming, PlayerToggleSwimEvent::cancel,
    // api: PlayerToggleSwimEvent::uncancel, PlayerToggleSwimEvent::isCancelled
    #[test]
    fn toggles_name_the_new_state() {
        let mut flight = PlayerToggleFlight {
            player: player(1),
            flying: true,
            cancelled: false,
        };
        assert_eq!((flight.player(), flight.is_flying()), (player(1), true));
        flight.cancel();
        assert!(flight.is_cancelled());
        flight.uncancel();
        assert!(!flight.is_cancelled());
        let mut glide = PlayerToggleGlide {
            player: player(1),
            gliding: false,
            cancelled: false,
        };
        assert_eq!((glide.player(), glide.is_gliding()), (player(1), false));
        glide.cancel();
        assert!(glide.is_cancelled());
        glide.uncancel();
        assert!(!glide.is_cancelled());
        let mut sneak = PlayerToggleSneak {
            player: player(1),
            sneaking: true,
            sneak_pressed: false,
            cancelled: false,
        };
        assert_eq!(
            (
                sneak.player(),
                sneak.is_sneaking(),
                sneak.is_sneak_pressed()
            ),
            (player(1), true, false)
        );
        sneak.cancel();
        assert!(sneak.is_cancelled());
        sneak.uncancel();
        assert!(!sneak.is_cancelled());
        let mut sprint = PlayerToggleSprint {
            player: player(1),
            sprinting: true,
            cancelled: false,
        };
        assert_eq!((sprint.player(), sprint.is_sprinting()), (player(1), true));
        sprint.cancel();
        assert!(sprint.is_cancelled());
        sprint.uncancel();
        assert!(!sprint.is_cancelled());
        let mut swim = PlayerToggleSwim {
            player: player(1),
            swimming: true,
            cancelled: false,
        };
        assert_eq!((swim.player(), swim.is_swimming()), (player(1), true));
        swim.cancel();
        assert!(swim.is_cancelled());
        swim.uncancel();
        assert!(!swim.is_cancelled());
    }

    // api: PlayerTransferEvent, PlayerTransferEvent::getAddress,
    // api: PlayerTransferEvent::setAddress, PlayerTransferEvent::getPort,
    // api: PlayerTransferEvent::setPort, PlayerTransferEvent::getMessage,
    // api: PlayerTransferEvent::setMessage, PlayerTransferEvent::cancel,
    // api: PlayerTransferEvent::uncancel, PlayerTransferEvent::isCancelled
    #[test]
    fn transfer_target_can_change() {
        let mut e = PlayerTransfer {
            player: player(1),
            address: "a.example".to_owned(),
            port: 19_132,
            message: "moved".to_owned(),
            cancelled: false,
        };
        assert_eq!(
            (e.player(), e.address(), e.port(), e.message()),
            (player(1), "a.example", 19_132, "moved")
        );
        e.set_address("b.example".to_owned());
        e.set_port(19_133);
        e.set_message(String::new());
        assert_eq!(
            (e.address(), e.port(), e.message()),
            ("b.example", 19_133, "")
        );
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: PlayerEditBookEvent, PlayerEditBookEvent::getAction,
    // api: PlayerEditBookEvent::getOldBook, PlayerEditBookEvent::getNewBook,
    // api: PlayerEditBookEvent::setNewBook, PlayerEditBookEvent::getModifiedPages,
    // api: PlayerEditBookEvent::cancel, PlayerEditBookEvent::uncancel,
    // api: PlayerEditBookEvent::isCancelled
    #[test]
    fn edit_book_takes_only_books() {
        let book = |kind: crate::item::Kind| Stack::new(kind.default_type(), 1);
        let quill = book(crate::item::Kind::WritableBook);
        let mut e = PlayerEditBook {
            player: player(1),
            old_book: quill.clone(),
            new_book: quill.clone(),
            action: BookAction::DeletePage,
            modified_pages: vec![3],
            cancelled: false,
        };
        assert_eq!(
            (e.player(), e.action()),
            (player(1), BookAction::DeletePage)
        );
        assert_eq!(
            (e.old_book(), e.modified_pages()),
            (&quill, [3_u32].as_slice())
        );
        e.set_new_book(sticks(1));
        assert_eq!(e.new_book(), &quill, "not a book: ignored");
        let written = book(crate::item::Kind::WrittenBook);
        e.set_new_book(written.clone());
        assert_eq!(e.new_book(), &written);
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // api: PlayerEntityPickEvent, PlayerEntityPickEvent::getEntity,
    // api: PlayerEntityPickEvent::getResultItem, PlayerEntityPickEvent::cancel,
    // api: PlayerEntityPickEvent::uncancel, PlayerEntityPickEvent::isCancelled
    #[test]
    fn entity_pick_names_the_entity_and_its_item() {
        let mut e = PlayerEntityPick {
            player: player(1),
            entity: entity(2),
            result_item: sticks(1),
            cancelled: false,
        };
        assert_eq!(
            (e.player(), e.entity(), e.result_item()),
            (player(1), entity(2), &sticks(1))
        );
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }
}
