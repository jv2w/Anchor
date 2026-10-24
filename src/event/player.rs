//! Player events (PMMP `pocketmine\event\player`): joining, leaving, moving, chatting and the
//! other things a player does.
//!
//! Messages (join, quit, death, kick) are plain text; an empty message shows nothing.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use crate::block::{Face, State};
use crate::config::GameMode;
use crate::entity;
use crate::item::{Enchantment, Stack};
use crate::math::{self, Vec3};
use crate::player;
use crate::protocol::{self, Uuid};
use crate::tick::queue::{Identity, Skin};
use crate::world::{self, BlockPos, ChunkPos};

/// Who a player is, from the verified login.
#[derive(Debug, Clone, PartialEq, Eq)]
// pmmp-class: PlayerInfo
pub struct PlayerInfo(pub(crate) Identity);

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

/// Raised when a player lies down in a bed. Cancelling keeps the player up.
#[doc(alias = "PlayerBedEnterEvent")]
#[derive(Debug)]
pub struct PlayerBedEnter {
    pub(crate) player: player::Id,
    pub(crate) world: world::Id,
    pub(crate) bed: BlockPos,
    pub(crate) cancelled: bool,
}

getters!(PlayerBedEnter {
    /// The player.
    player: player::Id,
    /// The world of the bed.
    world: world::Id,
    /// Where the bed's head is.
    #[doc(alias = "PlayerBedEnterEvent::getBed")]
    bed: BlockPos,
});

/// Raised when a player gets out of a bed.
#[doc(alias = "PlayerBedLeaveEvent")]
#[derive(Debug)]
pub struct PlayerBedLeave {
    pub(crate) player: player::Id,
    pub(crate) world: world::Id,
    pub(crate) bed: BlockPos,
}

getters!(PlayerBedLeave {
    /// The player.
    player: player::Id,
    /// The world of the bed.
    world: world::Id,
    /// Where the bed's head is.
    #[doc(alias = "PlayerBedLeaveEvent::getBed")]
    bed: BlockPos,
});

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

/// Raised when a player empties a bucket into the world. Cancelling keeps the world and the
/// bucket as they were.
#[doc(alias = "PlayerBucketEmptyEvent")]
#[doc(alias = "PlayerBucketEvent")]
#[derive(Debug)]
pub struct PlayerBucketEmpty {
    pub(crate) player: player::Id,
    pub(crate) world: world::Id,
    pub(crate) block_clicked: BlockPos,
    pub(crate) block_face: Face,
    pub(crate) bucket: Stack,
    pub(crate) item: Stack,
    pub(crate) cancelled: bool,
}

getters!(PlayerBucketEmpty {
    /// The player.
    player: player::Id,
    /// The world of the block.
    world: world::Id,
    /// The block the player clicked.
    #[doc(alias = "PlayerBucketEvent::getBlockClicked")]
    block_clicked: BlockPos,
    /// The face the player clicked.
    #[doc(alias = "PlayerBucketEvent::getBlockFace")]
    block_face: Face,
    /// The bucket before.
    #[doc(alias = "PlayerBucketEvent::getBucket")]
    bucket: &Stack,
    /// What the player holds after (an empty bucket in survival).
    #[doc(alias = "PlayerBucketEvent::getItem")]
    item: &Stack,
});

setters!(PlayerBucketEmpty {
    /// Replaces what the player holds after.
    #[doc(alias = "PlayerBucketEvent::setItem")]
    set_item => item: Stack,
});

/// Raised when a player fills a bucket from the world. Cancelling keeps the world and the bucket
/// as they were.
#[doc(alias = "PlayerBucketFillEvent")]
#[derive(Debug)]
pub struct PlayerBucketFill {
    pub(crate) player: player::Id,
    pub(crate) world: world::Id,
    pub(crate) block_clicked: BlockPos,
    pub(crate) block_face: Face,
    pub(crate) bucket: Stack,
    pub(crate) item: Stack,
    pub(crate) cancelled: bool,
}

getters!(PlayerBucketFill {
    /// The player.
    player: player::Id,
    /// The world of the block.
    world: world::Id,
    /// The block the player clicked.
    block_clicked: BlockPos,
    /// The face the player clicked.
    block_face: Face,
    /// The bucket before.
    bucket: &Stack,
    /// What the player holds after (the filled bucket).
    item: &Stack,
});

setters!(PlayerBucketFill {
    /// Replaces what the player holds after.
    set_item => item: Stack,
});

/// Raised when a player changes skin in game. Cancelling keeps the old skin.
#[doc(alias = "PlayerChangeSkinEvent")]
#[derive(Debug)]
pub struct PlayerChangeSkin {
    pub(crate) player: player::Id,
    pub(crate) old_skin: Skin,
    pub(crate) new_skin: Skin,
    pub(crate) cancelled: bool,
}

impl PlayerChangeSkin {
    /// The player.
    pub fn player(&self) -> player::Id {
        self.player
    }

    /// The skin before.
    #[doc(alias = "PlayerChangeSkinEvent::getOldSkin")]
    pub fn old_skin(&self) -> &protocol::Skin {
        self.old_skin.get()
    }

    /// The skin after.
    #[doc(alias = "PlayerChangeSkinEvent::getNewSkin")]
    pub fn new_skin(&self) -> &protocol::Skin {
        self.new_skin.get()
    }

    /// Replaces the skin after. It is checked like a login skin (image sizes, at most 4 MiB in
    /// all); a skin that fails cancels the change.
    #[doc(alias = "PlayerChangeSkinEvent::setNewSkin")]
    pub fn set_new_skin(&mut self, skin: protocol::Skin) {
        self.new_skin = Skin::new(Arc::new(skin));
    }
}

/// Raised when a player sends a chat message. Cancelling sends it to nobody.
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
    /// The players who see it. Starts as every player.
    #[doc(alias = "PlayerChatEvent::getRecipients")]
    recipients: &[player::Id],
});

setters!(PlayerChat {
    /// Makes another player the speaker.
    #[doc(alias = "PlayerChatEvent::setPlayer")]
    set_player => player: player::Id,
    /// Replaces the message.
    #[doc(alias = "PlayerChatEvent::setMessage")]
    set_message => message: String,
    /// Replaces the players who see it; players who left are skipped.
    #[doc(alias = "PlayerChatEvent::setRecipients")]
    set_recipients => recipients: Vec<player::Id>,
    /// Replaces the format: `None` for the standard line (sent as BDS sends it, see
    /// [`PlayerChat::format`]), or a text where `{%0}` stands for the speaker's name and `{%1}`
    /// for the message.
    #[doc(alias = "PlayerChatEvent::setFormatter")]
    set_format => format: Option<String>,
});

impl PlayerChat {
    /// How the line looks: `None` for the standard line, sent as BDS sends it (a chat text with
    /// the speaker's name as its source and the raw message, which the client shows as
    /// `<name> message`), or a text where `{%0}` stands for the speaker's name and `{%1}` for the
    /// message (PMMP `LegacyRawChatFormatter`). Starts as `None`.
    #[doc(alias = "PlayerChatEvent::getFormatter")]
    pub fn format(&self) -> Option<&str> {
        self.format.as_deref()
    }
}

/// Raised before a player's data goes into the next save of the server's state. Cancelling is
/// ignored: the data is saved anyway, so the commit stays atomic; what listeners
/// change before the snapshot is saved.
#[doc(alias = "PlayerDataSaveEvent")]
#[derive(Debug)]
pub struct PlayerDataSave {
    pub(crate) player: player::Id,
    pub(crate) player_name: String,
    pub(crate) cancelled: bool,
}

getters!(PlayerDataSave {
    /// The player.
    #[doc(alias = "PlayerDataSaveEvent::getPlayer")]
    player: player::Id,
    /// The name the record is saved under.
    #[doc(alias = "PlayerDataSaveEvent::getPlayerName")]
    player_name: &str,
});

impl PlayerDataSave {
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

    /// Whether a listener called [`cancel`](Self::cancel); the data is saved either way.
    #[doc(alias = "PlayerDataSaveEvent::isCancelled")]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
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
    /// What the player drops. Starts as its main slots, armour and off hand without items that
    /// vanish or stay on death, or empty for a creative player. Ignored when the player keeps the
    /// inventory or earlier death drops still wait for room (then it starts empty too).
    drops: &[Stack],
    /// The experience the player drops.
    xp_drop_amount: u32,
    /// The message every player sees in chat.
    #[doc(alias = "PlayerDeathEvent::getDeathMessage")]
    death_message: &str,
    /// The message on the dead player's death screen. Starts as the death message.
    #[doc(alias = "PlayerDeathEvent::getDeathScreenMessage")]
    death_screen_message: &str,
    /// Whether the player keeps the inventory.
    #[doc(alias = "PlayerDeathEvent::getKeepInventory")]
    keep_inventory: bool,
    /// Whether the player keeps the experience.
    #[doc(alias = "PlayerDeathEvent::getKeepXp")]
    keep_xp: bool,
});

setters!(PlayerDeath {
    /// Replaces what the player drops.
    set_drops => drops: Vec<Stack>,
    /// Replaces the experience the player drops.
    set_xp_drop_amount => xp_drop_amount: u32,
    /// Replaces the chat message.
    #[doc(alias = "PlayerDeathEvent::setDeathMessage")]
    set_death_message => death_message: String,
    /// Replaces the death screen message.
    #[doc(alias = "PlayerDeathEvent::setDeathScreenMessage")]
    set_death_screen_message => death_screen_message: String,
    /// Sets whether the player keeps the inventory.
    #[doc(alias = "PlayerDeathEvent::setKeepInventory")]
    set_keep_inventory => keep_inventory: bool,
    /// Sets whether the player keeps the experience.
    #[doc(alias = "PlayerDeathEvent::setKeepXp")]
    set_keep_xp => keep_xp: bool,
});

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

/// Raised when a player joins while the same player is already online: the same XUID with Xbox
/// sign-in on, otherwise the same name ignoring ASCII case.
///
/// If no listener cancels it, the connecting player goes on to [`PlayerLogin`]; once admitted,
/// the existing player is disconnected and the connecting one joins, and if refused, it is
/// disconnected and the existing player stays. If the event is cancelled, the connecting player
/// is disconnected at once. A player this event disconnects sees
/// [`disconnect_screen_message`](Self::disconnect_screen_message), or
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

    /// The outcome the listeners chose, and the connecting player's identity back.
    pub(crate) fn into_outcome(self) -> (Identity, DuplicateOutcome) {
        let outcome = DuplicateOutcome {
            cancelled: self.cancelled,
            message: self.screen_message.unwrap_or_else(|| self.reason.clone()),
            reason: self.reason,
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

    /// The reason given to the disconnected player. Starts as the client's key
    /// `"disconnectionScreen.loggedinOtherLocation"`, which the client shows in its language.
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
    #[doc(alias = "CancellableTrait::cancel")]
    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    /// Undoes [`cancel`](Self::cancel): the existing player is disconnected again.
    #[doc(alias = "PlayerDuplicateLoginEvent::uncancel")]
    #[doc(alias = "CancellableTrait::uncancel")]
    pub fn uncancel(&mut self) {
        self.cancelled = false;
    }

    /// Whether a listener cancelled the event.
    #[doc(alias = "PlayerDuplicateLoginEvent::isCancelled")]
    #[doc(alias = "CancellableTrait::isCancelled")]
    #[doc(alias = "Cancellable::isCancelled")]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

/// What the tick does after a [`PlayerDuplicateLogin`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DuplicateOutcome {
    /// Disconnect the connecting player instead of the existing one.
    pub(crate) cancelled: bool,
    /// The text of the `Disconnect` the disconnected player gets.
    pub(crate) message: String,
    /// Why the disconnected player left, for [`PlayerQuit`]: the reason, never the screen message.
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

/// What a player did to a book (PMMP `PlayerEditBookEvent::ACTION_*`).
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
            .item()
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
    /// Plays another emote instead.
    #[doc(alias = "PlayerEmoteEvent::setEmoteId")]
    set_emote_id => emote_id: String,
});

/// One enchanting option of an enchanting table (PMMP `EnchantingOption`).
#[doc(alias = "EnchantingOption")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnchantOption {
    pub(crate) required_level: u8,
    pub(crate) name: String,
    pub(crate) enchantments: Vec<(Enchantment, u8)>,
}

impl EnchantOption {
    /// An option needing `required_level`, shown as `name` in the enchanting alphabet, that gives
    /// `enchantments` (each with its level).
    pub fn new(required_level: u8, name: String, enchantments: Vec<(Enchantment, u8)>) -> Self {
        Self {
            required_level,
            name,
            enchantments,
        }
    }
}

getters!(EnchantOption {
    /// The experience level the option needs (it costs 1 to 3 levels, by its place).
    #[doc(alias = "EnchantingOption::getRequiredXpLevel")]
    required_level: u8,
    /// The name shown in the enchanting alphabet.
    #[doc(alias = "EnchantingOption::getDisplayName")]
    name: &str,
    /// What it enchants, each with its level, in order.
    #[doc(alias = "EnchantingOption::getEnchantments")]
    enchantments: &[(Enchantment, u8)],
});

/// Raised when an item goes into an enchanting table and the table shows its options. Cancelling
/// shows none.
#[doc(alias = "PlayerEnchantingOptionsRequestEvent")]
#[derive(Debug)]
pub struct PlayerEnchantingOptionsRequest {
    pub(crate) player: player::Id,
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) options: Vec<EnchantOption>,
    pub(crate) cancelled: bool,
}

getters!(PlayerEnchantingOptionsRequest {
    /// The player.
    player: player::Id,
    /// The world of the table.
    world: world::Id,
    /// Where the table is.
    #[doc(alias = "PlayerEnchantingOptionsRequestEvent::getInventory")]
    pos: BlockPos,
    /// The options shown, at most three.
    #[doc(alias = "PlayerEnchantingOptionsRequestEvent::getOptions")]
    options: &[EnchantOption],
});

impl PlayerEnchantingOptionsRequest {
    /// Replaces the options shown; options past the third are dropped.
    #[doc(alias = "PlayerEnchantingOptionsRequestEvent::setOptions")]
    pub fn set_options(&mut self, mut options: Vec<EnchantOption>) {
        options.truncate(protocol::PLAYER_ENCHANT_OPTIONS_OPTIONS_MAX_LEN);
        self.options = options;
    }
}

/// Raised when a player uses (right clicks) an entity: a villager NPC, an item frame. Cancelling
/// does nothing.
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

/// Raised when a player picks an entity (middle click), before the entity's item goes to the
/// hotbar. Cancelling gives nothing.
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

/// Why a player gets hungrier (PMMP `PlayerExhaustEvent::CAUSE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// Raised when a player gets hungrier. Cancelling keeps the player's exhaustion.
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
    /// Sets the exhaustion added. A value that is not finite is ignored.
    #[doc(alias = "PlayerExhaustEvent::setAmount")]
    pub fn set_amount(&mut self, amount: f64) {
        if amount.is_finite() {
            self.amount = amount;
        }
    }
}

/// Raised when a player's experience level or progress changes. Cancelling keeps both.
#[doc(alias = "PlayerExperienceChangeEvent")]
#[derive(Debug)]
pub struct PlayerExperienceChange {
    pub(crate) player: player::Id,
    pub(crate) old_level: u32,
    pub(crate) old_progress: f64,
    pub(crate) new_level: Option<u32>,
    pub(crate) new_progress: Option<f64>,
    pub(crate) cancelled: bool,
}

getters!(PlayerExperienceChange {
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

setters!(PlayerExperienceChange {
    /// Sets the level after (limited to the highest level when applied); `None` keeps the level.
    #[doc(alias = "PlayerExperienceChangeEvent::setNewLevel")]
    set_new_level => new_level: Option<u32>,
});

impl PlayerExperienceChange {
    /// Sets the progress after, limited to 0 to 1; `None` keeps the progress. A value that is not
    /// finite is ignored.
    #[doc(alias = "PlayerExperienceChangeEvent::setNewProgress")]
    pub fn set_new_progress(&mut self, progress: Option<f64>) {
        match progress {
            Some(p) if !p.is_finite() => {}
            Some(p) => self.new_progress = Some(math::clamp(p, 0.0, 1.0)),
            None => self.new_progress = None,
        }
    }
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

/// How a player touches a block (PMMP `PlayerInteractEvent::LEFT_CLICK_BLOCK`,
/// `RIGHT_CLICK_BLOCK`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InteractAction {
    /// Starting to break it.
    #[doc(alias = "PlayerInteractEvent::LEFT_CLICK_BLOCK")]
    LeftClickBlock,
    /// Using it, or using the held item on it.
    #[doc(alias = "PlayerInteractEvent::RIGHT_CLICK_BLOCK")]
    RightClickBlock,
}

/// Raised when a player clicks a block, before the block or the held item reacts. Cancelling does
/// neither (and a left click does not start breaking).
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
    /// Where on the block the player clicked, each part 0 to 1.
    #[doc(alias = "PlayerInteractEvent::getTouchVector")]
    touch_vector: Vec3,
    /// The click.
    #[doc(alias = "PlayerInteractEvent::getAction")]
    action: InteractAction,
});

impl PlayerInteract {
    /// Whether this is the first event of the click. While the player holds the right button on
    /// blocks, the client repeats the click and each repeat raises this event again; only the
    /// first of the press is first, even if the player looks at other blocks meanwhile. Left
    /// clicks and clicks of [`UseItemOn`](crate::action::UseItemOn) are always first.
    pub fn is_first(&self) -> bool {
        self.first
    }

    /// Whether a right click runs the held item's use action. Starts true.
    #[doc(alias = "PlayerInteractEvent::useItem")]
    pub fn uses_item(&self) -> bool {
        self.use_item
    }

    /// Whether a right click runs the clicked block's own response (a door opens, a container
    /// shows). Starts true.
    #[doc(alias = "PlayerInteractEvent::useBlock")]
    pub fn uses_block(&self) -> bool {
        self.use_block
    }
}

setters!(PlayerInteract {
    /// Turns the held item's use action on or off.
    #[doc(alias = "PlayerInteractEvent::setUseItem")]
    set_use_item => use_item: bool,
    /// Turns the clicked block's response on or off.
    #[doc(alias = "PlayerInteractEvent::setUseBlock")]
    set_use_block => use_block: bool,
});

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

setters!(PlayerItemConsume {
    /// Replaces what is left after.
    #[doc(alias = "PlayerItemConsumeEvent::setResidue")]
    set_residue => residue: Vec<Stack>,
});

/// Raised when a player picks an enchanting option, before the item is enchanted and the levels
/// are paid. Cancelling keeps the item and the levels.
#[doc(alias = "PlayerItemEnchantEvent")]
#[derive(Debug)]
pub struct PlayerItemEnchant {
    pub(crate) player: player::Id,
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) option: EnchantOption,
    pub(crate) input_item: Stack,
    pub(crate) output_item: Stack,
    pub(crate) cost: u8,
    pub(crate) cancelled: bool,
}

getters!(PlayerItemEnchant {
    /// The player.
    player: player::Id,
    /// The world of the table.
    world: world::Id,
    /// Where the table is.
    pos: BlockPos,
    /// The option picked.
    #[doc(alias = "PlayerItemEnchantEvent::getOption")]
    option: &EnchantOption,
    /// The item before.
    #[doc(alias = "PlayerItemEnchantEvent::getInputItem")]
    input_item: &Stack,
    /// The enchanted item.
    #[doc(alias = "PlayerItemEnchantEvent::getOutputItem")]
    output_item: &Stack,
    /// The levels (and lapis) it costs, 1 to 3.
    #[doc(alias = "PlayerItemEnchantEvent::getCost")]
    cost: u8,
});

/// Raised when a player selects another hotbar slot. Cancelling keeps the old one.
#[doc(alias = "PlayerItemHeldEvent")]
#[derive(Debug)]
pub struct PlayerItemHeld {
    pub(crate) player: player::Id,
    pub(crate) item: Stack,
    pub(crate) slot: u8,
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
    slot: u8,
});

/// Raised when a player uses the held item in the air (throwing, drawing a bow, eating, drinking).
/// Cancelling does nothing.
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

/// Raised when a player is about to be kicked. Cancelling keeps the player.
#[doc(alias = "PlayerKickEvent")]
#[derive(Debug)]
pub struct PlayerKick {
    pub(crate) player: player::Id,
    pub(crate) disconnect_reason: String,
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
    disconnect_reason: &str,
    /// The message every other player sees.
    #[doc(alias = "PlayerKickEvent::getQuitMessage")]
    quit_message: &str,
});

setters!(PlayerKick {
    /// Replaces the reason.
    #[doc(alias = "PlayerKickEvent::setDisconnectReason")]
    #[doc(alias = "PlayerDisconnectEventTrait::setDisconnectReason")]
    set_disconnect_reason => disconnect_reason: String,
    /// Replaces the message the player sees instead of the reason; `None` shows the reason.
    #[doc(alias = "PlayerKickEvent::setDisconnectScreenMessage")]
    #[doc(alias = "PlayerDisconnectEventTrait::setDisconnectScreenMessage")]
    set_disconnect_screen_message => screen_message: Option<String>,
    /// Replaces the message every other player sees; an empty one shows nothing.
    #[doc(alias = "PlayerKickEvent::setQuitMessage")]
    set_quit_message => quit_message: String,
});

impl PlayerKick {
    /// The message the player sees: the screen message, or the reason when there is none.
    #[doc(alias = "PlayerKickEvent::getDisconnectScreenMessage")]
    #[doc(alias = "PlayerDisconnectEventTrait::getDisconnectScreenMessage")]
    pub fn disconnect_screen_message(&self) -> &str {
        self.screen_message
            .as_deref()
            .unwrap_or(&self.disconnect_reason)
    }
}

/// Why a joining player may be refused (PMMP `PlayerPreLoginEvent::KICK_FLAG_*`), in order of
/// precedence: the first set flag gives the message (PMMP `KICK_FLAG_PRIORITY`).
#[doc(alias = "PlayerPreLoginEvent::KICK_FLAG_PRIORITY")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KickFlag {
    /// A listener refuses the player.
    #[doc(alias = "PlayerPreLoginEvent::KICK_FLAG_PLUGIN")]
    Plugin,
    /// The server is full.
    #[doc(alias = "PlayerPreLoginEvent::KICK_FLAG_SERVER_FULL")]
    ServerFull,
    /// The white-list is on and the player is not on it.
    #[doc(alias = "PlayerPreLoginEvent::KICK_FLAG_SERVER_WHITELISTED")]
    ServerWhitelisted,
    /// The player's name or address is banned.
    #[doc(alias = "PlayerPreLoginEvent::KICK_FLAG_BANNED")]
    Banned,
}

impl KickFlag {
    /// Every flag, in order of precedence.
    pub const ALL: [Self; 4] = [
        Self::Plugin,
        Self::ServerFull,
        Self::ServerWhitelisted,
        Self::Banned,
    ];
}

/// The messages of a set kick flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Kick {
    /// For the log.
    pub(crate) reason: String,
    /// For the player's screen.
    pub(crate) screen_message: String,
}

/// Raised when a player has logged in, before the player joins: Anchor has already set the kick
/// flags that apply (server full, not white-listed, banned). The player joins when no flag is set
/// after the listeners; otherwise the player is refused with the first flag's message. When the
/// same player is still online ([`PlayerDuplicateLogin`] not cancelled), it is online during this
/// event, its slot does not count toward a full server, and it is disconnected only if this
/// player is admitted.
#[doc(alias = "PlayerPreLoginEvent")]
#[doc(alias = "PlayerLoginEvent")]
#[derive(Debug)]
pub struct PlayerLogin {
    pub(crate) info: PlayerInfo,
    pub(crate) address: SocketAddr,
    pub(crate) kicks: [Option<Kick>; KickFlag::ALL.len()],
}

impl PlayerLogin {
    /// Who the player is.
    #[doc(alias = "PlayerPreLoginEvent::getPlayerInfo")]
    pub fn info(&self) -> &PlayerInfo {
        &self.info
    }

    /// The player's IP address.
    #[doc(alias = "PlayerPreLoginEvent::getIp")]
    pub fn ip(&self) -> IpAddr {
        self.address.ip()
    }

    /// The port the player connected from.
    #[doc(alias = "PlayerPreLoginEvent::getPort")]
    pub fn port(&self) -> u16 {
        self.address.port()
    }

    /// The flags set, in order of precedence.
    #[doc(alias = "PlayerPreLoginEvent::getKickFlags")]
    pub fn kick_flags(&self) -> impl Iterator<Item = KickFlag> + '_ {
        KickFlag::ALL
            .into_iter()
            .zip(&self.kicks)
            .filter_map(|(flag, kick)| kick.as_ref().map(|_| flag))
    }

    /// Whether `flag` is set.
    #[doc(alias = "PlayerPreLoginEvent::isKickFlagSet")]
    pub fn is_kick_flag_set(&self, flag: KickFlag) -> bool {
        self.kick(flag).is_some()
    }

    /// Sets `flag` with the reason for the log and the message for the player's screen (`None`:
    /// the reason).
    #[doc(alias = "PlayerPreLoginEvent::setKickFlag")]
    pub fn set_kick_flag(
        &mut self,
        flag: KickFlag,
        reason: impl Into<String>,
        screen_message: Option<String>,
    ) {
        if let Some(slot) = self.kicks.get_mut(flag as usize) {
            let reason = reason.into();
            let screen_message = screen_message.unwrap_or_else(|| reason.clone());
            *slot = Some(Kick {
                reason,
                screen_message,
            });
        }
    }

    /// Clears `flag`.
    #[doc(alias = "PlayerPreLoginEvent::clearKickFlag")]
    pub fn clear_kick_flag(&mut self, flag: KickFlag) {
        if let Some(slot) = self.kicks.get_mut(flag as usize) {
            *slot = None;
        }
    }

    /// Clears every flag: the player joins.
    #[doc(alias = "PlayerPreLoginEvent::clearAllKickFlags")]
    pub fn clear_all_kick_flags(&mut self) {
        self.kicks = Default::default();
    }

    /// Whether no flag is set, so the player joins.
    #[doc(alias = "PlayerPreLoginEvent::isAllowed")]
    pub fn is_allowed(&self) -> bool {
        self.kicks.iter().all(Option::is_none)
    }

    /// The reason of `flag`, if it is set.
    #[doc(alias = "PlayerPreLoginEvent::getDisconnectReason")]
    pub fn disconnect_reason(&self, flag: KickFlag) -> Option<&str> {
        self.kick(flag).map(|k| k.reason.as_str())
    }

    /// The screen message of `flag`, if it is set.
    #[doc(alias = "PlayerPreLoginEvent::getDisconnectScreenMessage")]
    pub fn disconnect_screen_message(&self, flag: KickFlag) -> Option<&str> {
        self.kick(flag).map(|k| k.screen_message.as_str())
    }

    /// The reason of the first set flag, if any.
    #[doc(alias = "PlayerPreLoginEvent::getFinalDisconnectReason")]
    pub fn final_disconnect_reason(&self) -> Option<&str> {
        self.kicks
            .iter()
            .flatten()
            .next()
            .map(|k| k.reason.as_str())
    }

    /// The screen message of the first set flag, if any: what the refused player sees.
    #[doc(alias = "PlayerPreLoginEvent::getFinalDisconnectScreenMessage")]
    pub fn final_disconnect_screen_message(&self) -> Option<&str> {
        self.kicks
            .iter()
            .flatten()
            .next()
            .map(|k| k.screen_message.as_str())
    }

    /// The messages of `flag`, if it is set.
    fn kick(&self, flag: KickFlag) -> Option<&Kick> {
        self.kicks.get(flag as usize).and_then(Option::as_ref)
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

/// Raised when a player moves, after the move is checked. Cancelling sends the player back.
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
    /// not finite is ignored.
    #[doc(alias = "PlayerMoveEvent::setTo")]
    pub fn set_target(&mut self, target: Vec3) {
        if target.is_finite() {
            self.target = target;
        }
    }
}

/// Raised after a chunk was sent to a player.
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

/// Raised when a player leaves, before the player is saved and removed.
#[doc(alias = "PlayerQuitEvent")]
#[derive(Debug)]
pub struct PlayerQuit {
    pub(crate) player: player::Id,
    pub(crate) quit_message: String,
    pub(crate) quit_reason: String,
}

getters!(PlayerQuit {
    /// The player.
    player: player::Id,
    /// The message every other player sees.
    #[doc(alias = "PlayerQuitEvent::getQuitMessage")]
    quit_message: &str,
    /// Why the player left.
    #[doc(alias = "PlayerQuitEvent::getQuitReason")]
    quit_reason: &str,
});

setters!(PlayerQuit {
    /// Replaces the message every other player sees; an empty one shows nothing.
    #[doc(alias = "PlayerQuitEvent::setQuitMessage")]
    set_quit_message => quit_message: String,
});

/// Raised when a dead player respawns, before the player is placed. A player that joins dead (it
/// left, or the server stopped, before it respawned) respawns as it joins, before it spawns.
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
    /// Respawns the player elsewhere. A position that is not finite is ignored.
    #[doc(alias = "PlayerRespawnEvent::setRespawnPosition")]
    pub fn set_position(&mut self, world: world::Id, position: Vec3) {
        if position.is_finite() {
            self.world = world;
            self.position = position;
        }
    }
}

/// What using a charged respawn anchor does (PMMP `PlayerRespawnAnchorUseEvent::ACTION_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RespawnAnchorAction {
    /// The anchor explodes: what a player's use does.
    #[doc(alias = "PlayerRespawnAnchorUseEvent::ACTION_EXPLODE")]
    Explode,
    /// The anchor becomes the player's respawn point.
    #[doc(alias = "PlayerRespawnAnchorUseEvent::ACTION_SET_SPAWN")]
    SetSpawn,
}

/// Raised when a player uses a charged respawn anchor with anything but glowstone it can take,
/// before the anchor acts. Cancelling does nothing (the held item may then be placed, as for any
/// block that does not react).
#[doc(alias = "PlayerRespawnAnchorUseEvent")]
#[derive(Debug)]
pub struct PlayerRespawnAnchorUse {
    pub(crate) player: player::Id,
    pub(crate) world: world::Id,
    pub(crate) pos: BlockPos,
    pub(crate) block: State,
    pub(crate) action: RespawnAnchorAction,
    pub(crate) cancelled: bool,
}

getters!(PlayerRespawnAnchorUse {
    /// The player.
    player: player::Id,
    /// The world of the anchor.
    world: world::Id,
    /// Where the anchor is.
    pos: BlockPos,
    /// The anchor.
    #[doc(alias = "PlayerRespawnAnchorUseEvent::getBlock")]
    block: State,
    /// What the anchor does. Starts as [`RespawnAnchorAction::Explode`].
    #[doc(alias = "PlayerRespawnAnchorUseEvent::getAction")]
    action: RespawnAnchorAction,
});

setters!(PlayerRespawnAnchorUse {
    /// Changes what the anchor does.
    #[doc(alias = "PlayerRespawnAnchorUseEvent::setAction")]
    set_action => action: RespawnAnchorAction,
});

/// Raised when a player who may fly starts or stops flying. Cancelling keeps the old state.
#[doc(alias = "PlayerToggleFlightEvent")]
#[derive(Debug)]
pub struct PlayerToggleFlight {
    pub(crate) player: player::Id,
    pub(crate) flying: bool,
    pub(crate) cancelled: bool,
}

/// Raised when a player starts or stops gliding. Cancelling keeps the old state.
#[doc(alias = "PlayerToggleGlideEvent")]
#[derive(Debug)]
pub struct PlayerToggleGlide {
    pub(crate) player: player::Id,
    pub(crate) gliding: bool,
    pub(crate) cancelled: bool,
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

/// Raised when a player starts or stops sprinting. Cancelling keeps the old state.
#[doc(alias = "PlayerToggleSprintEvent")]
#[derive(Debug)]
pub struct PlayerToggleSprint {
    pub(crate) player: player::Id,
    pub(crate) sprinting: bool,
    pub(crate) cancelled: bool,
}

/// Raised when a player starts or stops swimming. Cancelling keeps the old state.
#[doc(alias = "PlayerToggleSwimEvent")]
#[derive(Debug)]
pub struct PlayerToggleSwim {
    pub(crate) player: player::Id,
    pub(crate) swimming: bool,
    pub(crate) cancelled: bool,
}

impl PlayerToggleFlight {
    /// The player.
    pub fn player(&self) -> player::Id {
        self.player
    }

    /// Whether the player starts flying (otherwise stops).
    #[doc(alias = "PlayerToggleFlightEvent::isFlying")]
    pub fn is_flying(&self) -> bool {
        self.flying
    }
}

impl PlayerToggleGlide {
    /// The player.
    pub fn player(&self) -> player::Id {
        self.player
    }

    /// Whether the player starts gliding (otherwise stops).
    #[doc(alias = "PlayerToggleGlideEvent::isGliding")]
    pub fn is_gliding(&self) -> bool {
        self.gliding
    }
}

impl PlayerToggleSneak {
    /// The player.
    pub fn player(&self) -> player::Id {
        self.player
    }

    /// Whether the player starts sneaking (otherwise stops).
    #[doc(alias = "PlayerToggleSneakEvent::isSneaking")]
    pub fn is_sneaking(&self) -> bool {
        self.sneaking
    }

    /// Whether the sneak key is down (a player can sneak without it, for example while flying
    /// down).
    #[doc(alias = "PlayerToggleSneakEvent::isSneakPressed")]
    pub fn is_sneak_pressed(&self) -> bool {
        self.sneak_pressed
    }
}

impl PlayerToggleSprint {
    /// The player.
    pub fn player(&self) -> player::Id {
        self.player
    }

    /// Whether the player starts sprinting (otherwise stops).
    #[doc(alias = "PlayerToggleSprintEvent::isSprinting")]
    pub fn is_sprinting(&self) -> bool {
        self.sprinting
    }
}

impl PlayerToggleSwim {
    /// The player.
    pub fn player(&self) -> player::Id {
        self.player
    }

    /// Whether the player starts swimming (otherwise stops).
    #[doc(alias = "PlayerToggleSwimEvent::isSwimming")]
    pub fn is_swimming(&self) -> bool {
        self.swimming
    }
}

/// Raised when a player is about to be sent to another server. Cancelling keeps the player here.
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
    /// The message for the log.
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
    /// Replaces the message for the log.
    #[doc(alias = "PlayerTransferEvent::setMessage")]
    set_message => message: String,
});

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

cancellable! {
    PlayerBedEnter
        #[doc(alias = "PlayerBedEnterEvent::cancel")]
        #[doc(alias = "PlayerBedEnterEvent::uncancel")]
        #[doc(alias = "PlayerBedEnterEvent::isCancelled")];
    PlayerBlockPick
        #[doc(alias = "PlayerBlockPickEvent::cancel")]
        #[doc(alias = "PlayerBlockPickEvent::uncancel")]
        #[doc(alias = "PlayerBlockPickEvent::isCancelled")];
    PlayerBucketEmpty
        #[doc(alias = "PlayerBucketEvent::cancel")]
        #[doc(alias = "PlayerBucketEvent::uncancel")]
        #[doc(alias = "PlayerBucketEvent::isCancelled")];
    PlayerBucketFill;
    PlayerChangeSkin
        #[doc(alias = "PlayerChangeSkinEvent::cancel")]
        #[doc(alias = "PlayerChangeSkinEvent::uncancel")]
        #[doc(alias = "PlayerChangeSkinEvent::isCancelled")];
    PlayerChat
        #[doc(alias = "PlayerChatEvent::cancel")]
        #[doc(alias = "PlayerChatEvent::uncancel")]
        #[doc(alias = "PlayerChatEvent::isCancelled")];
    PlayerDropItem
        #[doc(alias = "PlayerDropItemEvent::cancel")]
        #[doc(alias = "PlayerDropItemEvent::uncancel")]
        #[doc(alias = "PlayerDropItemEvent::isCancelled")];
    PlayerEditBook
        #[doc(alias = "PlayerEditBookEvent::cancel")]
        #[doc(alias = "PlayerEditBookEvent::uncancel")]
        #[doc(alias = "PlayerEditBookEvent::isCancelled")];
    PlayerEmote
        #[doc(alias = "PlayerEmoteEvent::cancel")]
        #[doc(alias = "PlayerEmoteEvent::uncancel")]
        #[doc(alias = "PlayerEmoteEvent::isCancelled")];
    PlayerEnchantingOptionsRequest
        #[doc(alias = "PlayerEnchantingOptionsRequestEvent::cancel")]
        #[doc(alias = "PlayerEnchantingOptionsRequestEvent::uncancel")]
        #[doc(alias = "PlayerEnchantingOptionsRequestEvent::isCancelled")];
    PlayerEntityInteract
        #[doc(alias = "PlayerEntityInteractEvent::cancel")]
        #[doc(alias = "PlayerEntityInteractEvent::uncancel")]
        #[doc(alias = "PlayerEntityInteractEvent::isCancelled")];
    PlayerEntityPick
        #[doc(alias = "PlayerEntityPickEvent::cancel")]
        #[doc(alias = "PlayerEntityPickEvent::uncancel")]
        #[doc(alias = "PlayerEntityPickEvent::isCancelled")];
    PlayerExhaust
        #[doc(alias = "PlayerExhaustEvent::cancel")]
        #[doc(alias = "PlayerExhaustEvent::uncancel")]
        #[doc(alias = "PlayerExhaustEvent::isCancelled")];
    PlayerExperienceChange
        #[doc(alias = "PlayerExperienceChangeEvent::cancel")]
        #[doc(alias = "PlayerExperienceChangeEvent::uncancel")]
        #[doc(alias = "PlayerExperienceChangeEvent::isCancelled")];
    PlayerGameModeChange
        #[doc(alias = "PlayerGameModeChangeEvent::cancel")]
        #[doc(alias = "PlayerGameModeChangeEvent::uncancel")]
        #[doc(alias = "PlayerGameModeChangeEvent::isCancelled")];
    PlayerInteract
        #[doc(alias = "PlayerInteractEvent::cancel")]
        #[doc(alias = "PlayerInteractEvent::uncancel")]
        #[doc(alias = "PlayerInteractEvent::isCancelled")];
    PlayerItemConsume
        #[doc(alias = "PlayerItemConsumeEvent::cancel")]
        #[doc(alias = "PlayerItemConsumeEvent::uncancel")]
        #[doc(alias = "PlayerItemConsumeEvent::isCancelled")];
    PlayerItemEnchant
        #[doc(alias = "PlayerItemEnchantEvent::cancel")]
        #[doc(alias = "PlayerItemEnchantEvent::uncancel")]
        #[doc(alias = "PlayerItemEnchantEvent::isCancelled")];
    PlayerItemHeld
        #[doc(alias = "PlayerItemHeldEvent::cancel")]
        #[doc(alias = "PlayerItemHeldEvent::uncancel")]
        #[doc(alias = "PlayerItemHeldEvent::isCancelled")];
    PlayerItemUse
        #[doc(alias = "PlayerItemUseEvent::cancel")]
        #[doc(alias = "PlayerItemUseEvent::uncancel")]
        #[doc(alias = "PlayerItemUseEvent::isCancelled")];
    PlayerKick
        #[doc(alias = "PlayerKickEvent::cancel")]
        #[doc(alias = "PlayerKickEvent::uncancel")]
        #[doc(alias = "PlayerKickEvent::isCancelled")];
    PlayerMissSwing
        #[doc(alias = "PlayerMissSwingEvent::cancel")]
        #[doc(alias = "PlayerMissSwingEvent::uncancel")]
        #[doc(alias = "PlayerMissSwingEvent::isCancelled")];
    PlayerMove
        #[doc(alias = "PlayerMoveEvent::cancel")]
        #[doc(alias = "PlayerMoveEvent::uncancel")]
        #[doc(alias = "PlayerMoveEvent::isCancelled")];
    PlayerRespawnAnchorUse
        #[doc(alias = "PlayerRespawnAnchorUseEvent::cancel")]
        #[doc(alias = "PlayerRespawnAnchorUseEvent::uncancel")]
        #[doc(alias = "PlayerRespawnAnchorUseEvent::isCancelled")];
    PlayerToggleFlight
        #[doc(alias = "PlayerToggleFlightEvent::cancel")]
        #[doc(alias = "PlayerToggleFlightEvent::uncancel")]
        #[doc(alias = "PlayerToggleFlightEvent::isCancelled")];
    PlayerToggleGlide
        #[doc(alias = "PlayerToggleGlideEvent::cancel")]
        #[doc(alias = "PlayerToggleGlideEvent::uncancel")]
        #[doc(alias = "PlayerToggleGlideEvent::isCancelled")];
    PlayerToggleSneak
        #[doc(alias = "PlayerToggleSneakEvent::cancel")]
        #[doc(alias = "PlayerToggleSneakEvent::uncancel")]
        #[doc(alias = "PlayerToggleSneakEvent::isCancelled")];
    PlayerToggleSprint
        #[doc(alias = "PlayerToggleSprintEvent::cancel")]
        #[doc(alias = "PlayerToggleSprintEvent::uncancel")]
        #[doc(alias = "PlayerToggleSprintEvent::isCancelled")];
    PlayerToggleSwim
        #[doc(alias = "PlayerToggleSwimEvent::cancel")]
        #[doc(alias = "PlayerToggleSwimEvent::uncancel")]
        #[doc(alias = "PlayerToggleSwimEvent::isCancelled")];
    PlayerTransfer
        #[doc(alias = "PlayerTransferEvent::cancel")]
        #[doc(alias = "PlayerTransferEvent::uncancel")]
        #[doc(alias = "PlayerTransferEvent::isCancelled")];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::DIRT;
    use crate::event::fixtures::{POS, entity, player, sticks};

    const WORLD: world::Id = world::Id::DEFAULT;
    const AT: Vec3 = Vec3::new(0.5, 65.0, -1.5);

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

    // pmmp: PlayerInfo::getUsername, XboxLivePlayerInfo::getXuid, PlayerInfo::getUuid, PlayerInfo
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

    // pmmp: PlayerBedEnterEvent, PlayerBedLeaveEvent, PlayerBedEnterEvent::getBed, PlayerBedEnterEvent::cancel, PlayerBedEnterEvent::uncancel, PlayerBedEnterEvent::isCancelled, PlayerBedLeaveEvent::getBed
    #[test]
    fn bed_events_name_the_bed() {
        let mut enter = PlayerBedEnter {
            player: player(1),
            world: WORLD,
            bed: POS,
            cancelled: false,
        };
        assert_eq!(
            (enter.player(), enter.world(), enter.bed()),
            (player(1), WORLD, POS)
        );
        enter.cancel();
        assert!(enter.is_cancelled());
        enter.uncancel();
        assert!(!enter.is_cancelled());
        let leave = PlayerBedLeave {
            player: player(1),
            world: WORLD,
            bed: POS,
        };
        assert_eq!(
            (leave.player(), leave.world(), leave.bed()),
            (player(1), WORLD, POS)
        );
    }

    // pmmp: PlayerBlockPickEvent, PlayerBlockPickEvent::getBlock, PlayerBlockPickEvent::getResultItem, PlayerBlockPickEvent::cancel, PlayerBlockPickEvent::uncancel, PlayerBlockPickEvent::isCancelled
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

    // pmmp: PlayerBucketEmptyEvent, PlayerBucketFillEvent, PlayerBucketEvent, PlayerBucketEvent::getBlockClicked, PlayerBucketEvent::getBlockFace, PlayerBucketEvent::getBucket, PlayerBucketEvent::getItem, PlayerBucketEvent::setItem, PlayerBucketEvent::cancel, PlayerBucketEvent::uncancel, PlayerBucketEvent::isCancelled
    #[test]
    fn bucket_events_item_can_change() {
        let mut empty = PlayerBucketEmpty {
            player: player(1),
            world: WORLD,
            block_clicked: POS,
            block_face: Face::Up,
            bucket: sticks(1),
            item: sticks(2),
            cancelled: false,
        };
        assert_eq!(
            (
                empty.player(),
                empty.world(),
                empty.block_clicked(),
                empty.block_face()
            ),
            (player(1), WORLD, POS, Face::Up)
        );
        assert_eq!((empty.bucket(), empty.item()), (&sticks(1), &sticks(2)));
        empty.set_item(sticks(3));
        assert_eq!(empty.item(), &sticks(3));
        empty.cancel();
        assert!(empty.is_cancelled());
        empty.uncancel();
        assert!(!empty.is_cancelled());
        let mut fill = PlayerBucketFill {
            player: player(1),
            world: WORLD,
            block_clicked: POS,
            block_face: Face::North,
            bucket: sticks(1),
            item: sticks(2),
            cancelled: false,
        };
        assert_eq!(
            (
                fill.player(),
                fill.world(),
                fill.block_clicked(),
                fill.block_face()
            ),
            (player(1), WORLD, POS, Face::North)
        );
        assert_eq!((fill.bucket(), fill.item()), (&sticks(1), &sticks(2)));
        fill.set_item(Stack::EMPTY);
        assert_eq!(fill.item(), &Stack::EMPTY);
        fill.cancel();
        assert!(fill.is_cancelled());
        fill.uncancel();
        assert!(!fill.is_cancelled());
    }

    // pmmp: PlayerChangeSkinEvent, PlayerChangeSkinEvent::getOldSkin, PlayerChangeSkinEvent::getNewSkin, PlayerChangeSkinEvent::setNewSkin, PlayerChangeSkinEvent::cancel, PlayerChangeSkinEvent::uncancel, PlayerChangeSkinEvent::isCancelled
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

    // pmmp: PlayerChatEvent, PlayerChatEvent::getMessage, PlayerChatEvent::setMessage, PlayerChatEvent::setPlayer, PlayerChatEvent::getRecipients, PlayerChatEvent::setRecipients, PlayerChatEvent::getFormatter, PlayerChatEvent::setFormatter, PlayerChatEvent::cancel, PlayerChatEvent::uncancel, PlayerChatEvent::isCancelled
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

    // pmmp: PlayerDataSaveEvent, PlayerDataSaveEvent::getPlayer, PlayerDataSaveEvent::getPlayerName, PlayerDataSaveEvent::cancel, PlayerDataSaveEvent::uncancel, PlayerDataSaveEvent::isCancelled
    #[test]
    fn data_save_names_the_record() {
        let mut e = PlayerDataSave {
            player: player(1),
            player_name: "Steve".to_owned(),
            cancelled: false,
        };
        assert_eq!((e.player(), e.player_name()), (player(1), "Steve"));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: PlayerDeathEvent, PlayerDeathEvent::getPlayer, PlayerDeathEvent::getEntity, PlayerDeathEvent::getDeathMessage, PlayerDeathEvent::setDeathMessage, PlayerDeathEvent::getDeathScreenMessage, PlayerDeathEvent::setDeathScreenMessage, PlayerDeathEvent::getKeepInventory, PlayerDeathEvent::setKeepInventory, PlayerDeathEvent::getKeepXp, PlayerDeathEvent::setKeepXp
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
        assert!(!e.keep_inventory() && !e.keep_xp());
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
        assert!(e.keep_inventory() && e.keep_xp());
    }

    // pmmp: PlayerDropItemEvent, PlayerDropItemEvent::getItem, PlayerDropItemEvent::cancel, PlayerDropItemEvent::uncancel, PlayerDropItemEvent::isCancelled
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

    // pmmp: PlayerDuplicateLoginEvent, PlayerDuplicateLoginEvent::getConnectingSession, PlayerDuplicateLoginEvent::getExistingSession, PlayerDuplicateLoginEvent::getDisconnectReason, PlayerDuplicateLoginEvent::setDisconnectReason, PlayerDuplicateLoginEvent::getDisconnectScreenMessage, PlayerDuplicateLoginEvent::setDisconnectScreenMessage, PlayerDuplicateLoginEvent::cancel, PlayerDuplicateLoginEvent::uncancel, PlayerDuplicateLoginEvent::isCancelled, CancellableTrait::cancel, CancellableTrait::uncancel, CancellableTrait::isCancelled, Cancellable::isCancelled
    #[test]
    fn the_outcome_uses_the_screen_message_before_the_reason() {
        let event = PlayerDuplicateLogin::new(identity("a"), identity("A"));
        assert_eq!(event.connecting().name(), "a");
        assert_eq!(event.existing().name(), "A");
        assert_eq!(event.disconnect_screen_message(), None);
        let (connecting, outcome) = event.clone().into_outcome();
        assert_eq!(connecting, identity("a"));
        assert_eq!(outcome, DuplicateOutcome::unheard());
        let mut event = event;
        event.set_disconnect_reason("reason");
        assert_eq!(event.disconnect_reason(), "reason");
        assert_eq!(event.into_outcome().1.message, "reason");
        let mut event = PlayerDuplicateLogin::new(identity("a"), identity("A"));
        event.set_disconnect_reason("reason");
        event.set_disconnect_screen_message(Some("screen".to_owned()));
        assert_eq!(event.disconnect_screen_message(), Some("screen"));
        event.cancel();
        assert!(event.is_cancelled());
        event.uncancel();
        assert!(!event.is_cancelled());
        event.cancel();
        let outcome = event.into_outcome().1;
        assert!(outcome.cancelled);
        assert_eq!(outcome.message, "screen");
    }

    // pmmp: PlayerEmoteEvent, PlayerEmoteEvent::getEmoteId, PlayerEmoteEvent::setEmoteId, PlayerEmoteEvent::cancel, PlayerEmoteEvent::uncancel, PlayerEmoteEvent::isCancelled
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

    // pmmp: EnchantingOption, PlayerEnchantingOptionsRequestEvent, EnchantingOption::getRequiredXpLevel, EnchantingOption::getDisplayName, EnchantingOption::getEnchantments, PlayerEnchantingOptionsRequestEvent::getInventory, PlayerEnchantingOptionsRequestEvent::getOptions, PlayerEnchantingOptionsRequestEvent::setOptions, PlayerEnchantingOptionsRequestEvent::cancel, PlayerEnchantingOptionsRequestEvent::uncancel, PlayerEnchantingOptionsRequestEvent::isCancelled
    #[test]
    fn enchanting_options_are_at_most_three() {
        let option = EnchantOption::new(5, "abc".to_owned(), vec![(Enchantment::Efficiency, 2)]);
        assert_eq!(
            (
                option.required_level(),
                option.name(),
                option.enchantments()
            ),
            (5, "abc", [(Enchantment::Efficiency, 2)].as_slice())
        );
        let mut e = PlayerEnchantingOptionsRequest {
            player: player(1),
            world: WORLD,
            pos: POS,
            options: vec![option.clone()],
            cancelled: false,
        };
        assert_eq!((e.player(), e.world(), e.pos()), (player(1), WORLD, POS));
        assert_eq!(e.options(), [option.clone()].as_slice());
        e.set_options(vec![option.clone(); 5]);
        assert_eq!(e.options().len(), 3);
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: PlayerEntityInteractEvent, PlayerEntityInteractEvent::getEntity, PlayerEntityInteractEvent::getClickPosition, PlayerEntityInteractEvent::cancel, PlayerEntityInteractEvent::uncancel, PlayerEntityInteractEvent::isCancelled
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

    // pmmp: PlayerExhaustEvent, PlayerExhaustEvent::getPlayer, PlayerExhaustEvent::getAmount, PlayerExhaustEvent::setAmount, PlayerExhaustEvent::getCause, PlayerExhaustEvent::CAUSE_ATTACK, PlayerExhaustEvent::CAUSE_DAMAGE, PlayerExhaustEvent::CAUSE_MINING, PlayerExhaustEvent::CAUSE_HEALTH_REGEN, PlayerExhaustEvent::CAUSE_POTION, PlayerExhaustEvent::CAUSE_WALKING, PlayerExhaustEvent::CAUSE_SPRINTING, PlayerExhaustEvent::CAUSE_JUMPING, PlayerExhaustEvent::CAUSE_SPRINT_JUMPING, PlayerExhaustEvent::cancel, PlayerExhaustEvent::uncancel, PlayerExhaustEvent::isCancelled
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
            e.cancel();
            assert!(e.is_cancelled());
            e.uncancel();
            assert!(!e.is_cancelled());
        }
    }

    // pmmp: PlayerExperienceChangeEvent, PlayerExperienceChangeEvent::getOldLevel, PlayerExperienceChangeEvent::getOldProgress, PlayerExperienceChangeEvent::getNewLevel, PlayerExperienceChangeEvent::getNewProgress, PlayerExperienceChangeEvent::setNewLevel, PlayerExperienceChangeEvent::setNewProgress, PlayerExperienceChangeEvent::cancel, PlayerExperienceChangeEvent::uncancel, PlayerExperienceChangeEvent::isCancelled
    #[test]
    fn experience_change_keeps_progress_in_range() {
        let mut e = PlayerExperienceChange {
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
        e.set_new_level(None);
        e.set_new_progress(Some(2.0));
        assert_eq!((e.new_level(), e.new_progress()), (None, Some(1.0)));
        e.set_new_progress(Some(f64::NAN));
        assert_eq!(e.new_progress(), Some(1.0));
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

    // pmmp: PlayerGameModeChangeEvent, PlayerGameModeChangeEvent::getNewGamemode, PlayerGameModeChangeEvent::cancel, PlayerGameModeChangeEvent::uncancel, PlayerGameModeChangeEvent::isCancelled
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

    // pmmp: PlayerInteractEvent, PlayerInteractEvent::getBlock, PlayerInteractEvent::getFace, PlayerInteractEvent::getItem, PlayerInteractEvent::getTouchVector, PlayerInteractEvent::getAction, PlayerInteractEvent::useItem, PlayerInteractEvent::useBlock, PlayerInteractEvent::setUseItem, PlayerInteractEvent::setUseBlock, PlayerInteractEvent::LEFT_CLICK_BLOCK, PlayerInteractEvent::RIGHT_CLICK_BLOCK, PlayerInteractEvent::cancel, PlayerInteractEvent::uncancel, PlayerInteractEvent::isCancelled
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

    // pmmp: PlayerItemConsumeEvent, PlayerItemConsumeEvent::getItem, PlayerItemConsumeEvent::getResidue, PlayerItemConsumeEvent::setResidue, PlayerItemConsumeEvent::cancel, PlayerItemConsumeEvent::uncancel, PlayerItemConsumeEvent::isCancelled
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

    // pmmp: PlayerItemEnchantEvent, PlayerItemEnchantEvent::getOption, PlayerItemEnchantEvent::getInputItem, PlayerItemEnchantEvent::getOutputItem, PlayerItemEnchantEvent::getCost, PlayerItemEnchantEvent::cancel, PlayerItemEnchantEvent::uncancel, PlayerItemEnchantEvent::isCancelled
    #[test]
    fn item_enchant_names_the_option_and_items() {
        let option = EnchantOption::new(1, "x".to_owned(), Vec::new());
        let mut e = PlayerItemEnchant {
            player: player(1),
            world: WORLD,
            pos: POS,
            option: option.clone(),
            input_item: sticks(1),
            output_item: sticks(2),
            cost: 1,
            cancelled: false,
        };
        assert_eq!((e.player(), e.world(), e.pos()), (player(1), WORLD, POS));
        assert_eq!((e.option(), e.cost()), (&option, 1));
        assert_eq!((e.input_item(), e.output_item()), (&sticks(1), &sticks(2)));
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: PlayerItemHeldEvent, PlayerItemUseEvent, PlayerItemHeldEvent::getItem, PlayerItemHeldEvent::getSlot, PlayerItemHeldEvent::cancel, PlayerItemHeldEvent::uncancel, PlayerItemHeldEvent::isCancelled, PlayerItemUseEvent::getItem, PlayerItemUseEvent::getDirectionVector, PlayerItemUseEvent::cancel, PlayerItemUseEvent::uncancel, PlayerItemUseEvent::isCancelled
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

    // pmmp: PlayerJumpEvent, PlayerJoinEvent, PlayerQuitEvent, PlayerEvent::getPlayer, PlayerJoinEvent::getJoinMessage, PlayerJoinEvent::setJoinMessage, PlayerQuitEvent::getQuitMessage, PlayerQuitEvent::setQuitMessage, PlayerQuitEvent::getQuitReason
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
            quit_reason: "timeout".to_owned(),
        };
        assert_eq!(
            (quit.player(), quit.quit_message(), quit.quit_reason()),
            (player(1), "left", "timeout")
        );
        quit.set_quit_message("gone".to_owned());
        assert_eq!(quit.quit_message(), "gone");
        assert_eq!(PlayerJump { player: player(2) }.player(), player(2));
    }

    // pmmp: PlayerKickEvent, PlayerKickEvent::getDisconnectReason, PlayerDisconnectEventTrait::getDisconnectReason, PlayerKickEvent::setDisconnectReason, PlayerDisconnectEventTrait::setDisconnectReason, PlayerKickEvent::getDisconnectScreenMessage, PlayerDisconnectEventTrait::getDisconnectScreenMessage, PlayerKickEvent::setDisconnectScreenMessage, PlayerDisconnectEventTrait::setDisconnectScreenMessage, PlayerKickEvent::getQuitMessage, PlayerKickEvent::setQuitMessage, PlayerKickEvent::cancel, PlayerKickEvent::uncancel, PlayerKickEvent::isCancelled
    #[test]
    fn kick_screen_message_falls_back_to_the_reason() {
        let mut e = PlayerKick {
            player: player(1),
            disconnect_reason: "spam".to_owned(),
            screen_message: None,
            quit_message: "kicked".to_owned(),
            cancelled: false,
        };
        assert_eq!((e.player(), e.disconnect_reason()), (player(1), "spam"));
        assert_eq!(
            (e.disconnect_screen_message(), e.quit_message()),
            ("spam", "kicked")
        );
        e.set_disconnect_reason("flood".to_owned());
        e.set_quit_message(String::new());
        assert_eq!(
            (e.disconnect_screen_message(), e.quit_message()),
            ("flood", "")
        );
        e.set_disconnect_screen_message(Some("Bye".to_owned()));
        assert_eq!(e.disconnect_screen_message(), "Bye");
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: PlayerLoginEvent, PlayerPreLoginEvent, PlayerPreLoginEvent::getPlayerInfo, PlayerPreLoginEvent::getIp, PlayerPreLoginEvent::getPort, PlayerPreLoginEvent::getKickFlags, PlayerPreLoginEvent::isKickFlagSet, PlayerPreLoginEvent::setKickFlag, PlayerPreLoginEvent::clearKickFlag, PlayerPreLoginEvent::clearAllKickFlags, PlayerPreLoginEvent::isAllowed, PlayerPreLoginEvent::getDisconnectReason, PlayerPreLoginEvent::getDisconnectScreenMessage, PlayerPreLoginEvent::getFinalDisconnectReason, PlayerPreLoginEvent::getFinalDisconnectScreenMessage, PlayerPreLoginEvent::KICK_FLAG_PRIORITY, PlayerPreLoginEvent::KICK_FLAG_PLUGIN, PlayerPreLoginEvent::KICK_FLAG_SERVER_FULL, PlayerPreLoginEvent::KICK_FLAG_SERVER_WHITELISTED, PlayerPreLoginEvent::KICK_FLAG_BANNED
    #[test]
    fn login_refuses_with_the_first_kick_flag() {
        let address: SocketAddr = "192.0.2.1:19132".parse().expect("address");
        let mut e = PlayerLogin {
            info: PlayerInfo(identity("Steve")),
            address,
            kicks: Default::default(),
        };
        assert_eq!(e.info().name(), "Steve");
        assert_eq!((e.ip(), e.port()), (address.ip(), 19132));
        assert!(e.is_allowed());
        assert_eq!(e.final_disconnect_reason(), None);
        e.set_kick_flag(
            KickFlag::Banned,
            "banned",
            Some("You are banned".to_owned()),
        );
        e.set_kick_flag(KickFlag::ServerFull, "full", None);
        assert!(!e.is_allowed());
        assert!(e.is_kick_flag_set(KickFlag::Banned));
        assert!(!e.is_kick_flag_set(KickFlag::Plugin));
        assert_eq!(
            e.kick_flags().collect::<Vec<_>>(),
            [KickFlag::ServerFull, KickFlag::Banned]
        );
        assert_eq!(e.disconnect_reason(KickFlag::Banned), Some("banned"));
        assert_eq!(
            e.disconnect_screen_message(KickFlag::Banned),
            Some("You are banned")
        );
        assert_eq!(
            e.disconnect_screen_message(KickFlag::ServerFull),
            Some("full")
        );
        assert_eq!(e.disconnect_reason(KickFlag::ServerWhitelisted), None);
        assert_eq!(e.final_disconnect_reason(), Some("full"));
        assert_eq!(e.final_disconnect_screen_message(), Some("full"));
        e.clear_kick_flag(KickFlag::ServerFull);
        assert_eq!(e.final_disconnect_screen_message(), Some("You are banned"));
        e.set_kick_flag(KickFlag::Plugin, "no", None);
        assert_eq!(e.final_disconnect_reason(), Some("no"));
        e.clear_all_kick_flags();
        assert!(e.is_allowed());
        assert_eq!(e.kick_flags().count(), 0);
        let mut sorted = KickFlag::ALL;
        sorted.sort();
        assert_eq!(sorted, KickFlag::ALL);
    }

    // pmmp: PlayerMissSwingEvent, PlayerMoveEvent, PlayerMissSwingEvent::cancel, PlayerMissSwingEvent::uncancel, PlayerMissSwingEvent::isCancelled, PlayerMoveEvent::getFrom, PlayerMoveEvent::getTo, PlayerMoveEvent::setTo, PlayerMoveEvent::cancel, PlayerMoveEvent::uncancel, PlayerMoveEvent::isCancelled
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

    // pmmp: PlayerPostChunkSendEvent, PlayerViewDistanceChangeEvent, PlayerPostChunkSendEvent::getChunkX, PlayerPostChunkSendEvent::getChunkZ, PlayerViewDistanceChangeEvent::getOldDistance, PlayerViewDistanceChangeEvent::getNewDistance
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

    // pmmp: PlayerRespawnEvent, PlayerRespawnEvent::getRespawnPosition, PlayerRespawnEvent::setRespawnPosition
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

    // pmmp: PlayerToggleFlightEvent, PlayerToggleGlideEvent, PlayerToggleSneakEvent, PlayerToggleSprintEvent, PlayerToggleSwimEvent, PlayerToggleFlightEvent::isFlying, PlayerToggleFlightEvent::cancel, PlayerToggleFlightEvent::uncancel, PlayerToggleFlightEvent::isCancelled, PlayerToggleGlideEvent::isGliding, PlayerToggleGlideEvent::cancel, PlayerToggleGlideEvent::uncancel, PlayerToggleGlideEvent::isCancelled, PlayerToggleSneakEvent::isSneaking, PlayerToggleSneakEvent::isSneakPressed, PlayerToggleSneakEvent::cancel, PlayerToggleSneakEvent::uncancel, PlayerToggleSneakEvent::isCancelled, PlayerToggleSprintEvent::isSprinting, PlayerToggleSprintEvent::cancel, PlayerToggleSprintEvent::uncancel, PlayerToggleSprintEvent::isCancelled, PlayerToggleSwimEvent::isSwimming, PlayerToggleSwimEvent::cancel, PlayerToggleSwimEvent::uncancel, PlayerToggleSwimEvent::isCancelled
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

    // pmmp: PlayerTransferEvent, PlayerTransferEvent::getAddress, PlayerTransferEvent::setAddress, PlayerTransferEvent::getPort, PlayerTransferEvent::setPort, PlayerTransferEvent::getMessage, PlayerTransferEvent::setMessage, PlayerTransferEvent::cancel, PlayerTransferEvent::uncancel, PlayerTransferEvent::isCancelled
    #[test]
    fn transfer_target_can_change() {
        let mut e = PlayerTransfer {
            player: player(1),
            address: "a.example".to_owned(),
            port: 19132,
            message: "moved".to_owned(),
            cancelled: false,
        };
        assert_eq!(
            (e.player(), e.address(), e.port(), e.message()),
            (player(1), "a.example", 19132, "moved")
        );
        e.set_address("b.example".to_owned());
        e.set_port(19133);
        e.set_message(String::new());
        assert_eq!(
            (e.address(), e.port(), e.message()),
            ("b.example", 19133, "")
        );
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: PlayerRespawnAnchorUseEvent, PlayerRespawnAnchorUseEvent::getBlock, PlayerRespawnAnchorUseEvent::getAction, PlayerRespawnAnchorUseEvent::setAction, PlayerRespawnAnchorUseEvent::cancel, PlayerRespawnAnchorUseEvent::uncancel, PlayerRespawnAnchorUseEvent::isCancelled
    #[test]
    fn respawn_anchor_use_names_the_anchor_and_its_action() {
        let mut e = PlayerRespawnAnchorUse {
            player: player(1),
            world: WORLD,
            pos: POS,
            block: DIRT,
            action: RespawnAnchorAction::Explode,
            cancelled: false,
        };
        assert_eq!(
            (e.player(), e.world(), e.pos(), e.block()),
            (player(1), WORLD, POS, DIRT)
        );
        assert_eq!(e.action(), RespawnAnchorAction::Explode);
        e.set_action(RespawnAnchorAction::SetSpawn);
        assert_eq!(e.action(), RespawnAnchorAction::SetSpawn);
        e.cancel();
        assert!(e.is_cancelled());
        e.uncancel();
        assert!(!e.is_cancelled());
    }

    // pmmp: PlayerEditBookEvent, PlayerEditBookEvent::getAction, PlayerEditBookEvent::getOldBook, PlayerEditBookEvent::getNewBook, PlayerEditBookEvent::setNewBook, PlayerEditBookEvent::getModifiedPages, PlayerEditBookEvent::cancel, PlayerEditBookEvent::uncancel, PlayerEditBookEvent::isCancelled
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
            (&quill, [3u32].as_slice())
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

    // pmmp: PlayerEntityPickEvent, PlayerEntityPickEvent::getEntity, PlayerEntityPickEvent::getResultItem, PlayerEntityPickEvent::cancel, PlayerEntityPickEvent::uncancel, PlayerEntityPickEvent::isCancelled
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
