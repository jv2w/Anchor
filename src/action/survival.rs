//! The actions of a player's survival, place and connection: damage, game mode, chat, teleport,
//! kick, transfer.

use super::{Action, Sealed};
use crate::config::GameMode;
use crate::event::{DamageCause, DamageSource, TeleportCause};
use crate::math::Vec3;
use crate::tick::Context;
use crate::{player, world};

/// A player says something in chat as if it had typed it (PMMP `Player::chat`): the chat limits
/// apply (2 lines a tick, 512 bytes a line), a line starting with `/` runs as its command,
/// and each other line raises [`PlayerChat`](crate::event::PlayerChat).
#[doc(alias = "Player::chat")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chat {
    player: player::Id,
    message: String,
}

impl Chat {
    /// `player` says `message`.
    pub fn new(player: player::Id, message: impl Into<String>) -> Self {
        Self {
            player,
            message: message.into(),
        }
    }
}

impl Action for Chat {}

impl Sealed for Chat {
    fn perform(self, ctx: &mut Context<'_>) {
        ctx.chat_as(self.player, &self.message);
    }
}

/// Damage to a player as the game would deal it (PMMP `Entity::attack` with an
/// `EntityDamageEvent`): its armour, effects, enchantments, invulnerability and game mode count
/// (except for [`DamageCause::Suicide`], which skips them), the
/// [`EntityDamage`](crate::event::EntityDamage) listeners decide, and a death raises
/// [`PlayerDeath`](crate::event::PlayerDeath). A player in the source is the attacker: its
/// Strength adds damage, the player is knocked back from it, and thorns answer the hit.
/// `/kill` deals no damage: it calls [`Player::kill`](crate::player::Player::kill).
#[doc(alias = "Entity::attack")]
#[doc(alias = "Living::attack")]
#[doc(alias = "Player::attack")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Damage {
    player: player::Id,
    amount: f64,
    cause: DamageCause,
    source: DamageSource,
}

impl Damage {
    /// `amount` damage (half hearts, before the modifiers) to `player` for `cause`, from no source
    /// in particular. An amount that is negative, not finite or above
    /// [`MAX_DAMAGE`](crate::event::MAX_DAMAGE) does nothing.
    pub fn new(player: player::Id, amount: f64, cause: DamageCause) -> Self {
        Self {
            player,
            amount,
            cause,
            source: DamageSource::None,
        }
    }

    /// The same damage dealt by `source`.
    #[must_use]
    pub fn with_source(self, source: DamageSource) -> Self {
        Self { source, ..self }
    }
}

impl Action for Damage {}

impl Sealed for Damage {
    fn perform(self, ctx: &mut Context<'_>) {
        ctx.hurt_player(self.player, self.amount, self.cause, self.source);
    }
}

/// A kick of a player as the game would do it (PMMP `Player::kick`): the
/// [`PlayerKick`](crate::event::PlayerKick) listeners may keep it or change the messages; it
/// leaves at the end of the tick, raising [`PlayerQuit`](crate::event::PlayerQuit). Nothing
/// happens if the player is gone or already leaving, a text is longer than 65,536 bytes, or a
/// listener cancels it. [`Player::kick`](crate::player::Player::kick) kicks without asking.
#[doc(alias = "Player::kick")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kick {
    player: player::Id,
    reason: String,
    screen_message: Option<String>,
    quit_message: Option<String>,
}

impl Kick {
    /// A kick of `player` with `reason` for the log and its screen (the client's "disconnected"
    /// text when empty); the other players see that it left the game.
    pub fn new(player: player::Id, reason: impl Into<String>) -> Self {
        Self {
            player,
            reason: reason.into(),
            screen_message: None,
            quit_message: None,
        }
    }

    /// The same kick with `message` shown to the other players in chat.
    #[must_use]
    pub fn with_quit_message(self, message: impl Into<String>) -> Self {
        Self {
            quit_message: Some(message.into()),
            ..self
        }
    }

    /// The same kick with `message` on the player's screen instead of the reason.
    #[must_use]
    pub fn with_screen_message(self, message: impl Into<String>) -> Self {
        Self {
            screen_message: Some(message.into()),
            ..self
        }
    }
}

impl Action for Kick {}

impl Sealed for Kick {
    fn perform(self, ctx: &mut Context<'_>) {
        ctx.kick_player(
            self.player,
            self.reason,
            self.screen_message,
            self.quit_message,
        );
    }
}

/// A change of a player's game mode as the game would make it (PMMP `Player::setGamemode`): the
/// [`PlayerGameModeChange`](crate::event::PlayerGameModeChange) listeners may keep the old one.
/// [`Player::set_game_mode`](crate::player::Player::set_game_mode) changes it without asking.
#[doc(alias = "Player::setGamemode")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetGameMode {
    player: player::Id,
    mode: GameMode,
}

impl SetGameMode {
    /// The change that puts `player` in `mode`; nothing if it already has it.
    pub fn new(player: player::Id, mode: GameMode) -> Self {
        Self { player, mode }
    }
}

impl Action for SetGameMode {}

impl Sealed for SetGameMode {
    fn perform(self, ctx: &mut Context<'_>) {
        ctx.change_game_mode(self.player, self.mode);
    }
}

/// A teleport of a player as the game would do it (PMMP `Entity::teleport`): the
/// [`EntityTeleport`](crate::event::EntityTeleport) listeners may keep it where it is or send it
/// elsewhere. [`Context::teleport`] moves it without asking.
#[doc(alias = "Entity::teleport")]
#[doc(alias = "Player::teleport")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Teleport {
    player: player::Id,
    world: world::Id,
    feet: Vec3,
    cause: TeleportCause,
}

impl Teleport {
    /// A teleport of `player`'s feet to `feet` in `world`; nothing where [`Context::teleport`]
    /// fails.
    pub fn new(player: player::Id, world: world::Id, feet: Vec3) -> Self {
        Self {
            player,
            world,
            feet,
            cause: TeleportCause::Action,
        }
    }

    /// The same teleport as `/tp`'s.
    pub(crate) fn by_command(self) -> Self {
        Self {
            cause: TeleportCause::Command,
            ..self
        }
    }
}

impl Action for Teleport {}

impl Sealed for Teleport {
    fn perform(self, ctx: &mut Context<'_>) {
        ctx.teleport_player(self.player, self.world, self.feet, self.cause);
    }
}

/// A transfer of a player to another server as the game would do it (PMMP `Player::transfer`):
/// the [`PlayerTransfer`](crate::event::PlayerTransfer) listeners may keep it or change where it
/// goes; it leaves at the end of the tick, raising [`PlayerQuit`](crate::event::PlayerQuit).
/// Nothing happens if the player is gone or already leaving, the address is empty or longer than
/// 253 bytes, a text is longer than 65,536 bytes, or a listener cancels it.
/// [`Player::transfer`](crate::player::Player::transfer) sends it without asking.
#[doc(alias = "Player::transfer")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    player: player::Id,
    address: String,
    port: u16,
    message: Option<String>,
}

impl Transfer {
    /// A transfer of `player` to the server at `address` (a host name or IP address) and `port`.
    pub fn new(player: player::Id, address: impl Into<String>, port: u16) -> Self {
        Self {
            player,
            address: address.into(),
            port,
            message: None,
        }
    }

    /// The same transfer with `message` for the log instead of "Moved to another server".
    #[must_use]
    pub fn with_message(self, message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
            ..self
        }
    }
}

impl Action for Transfer {}

impl Sealed for Transfer {
    fn perform(self, ctx: &mut Context<'_>) {
        ctx.transfer_player(self.player, self.address, self.port, self.message);
    }
}
