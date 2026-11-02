//! Anchor's own commands: PMMP's default commands that are in scope, one per PMMP class in
//! `command\defaults`, registered through the same [`Command`] API as the user's.
//!
//! Each follows its PMMP class: names, aliases, permissions (PMMP's names under `anchor`),
//! arguments and behaviour. A message the client knows (`commands.*`, `chat.*`, `gameMode.*`,
//! descriptions as BDS sends them) goes as its translation key and parameters; a message the client
//! has no key for is Anchor's own text (no other server's sentences). Where Anchor differs, the
//! command's function says why. Game mode names and coordinates are written as BDS writes them, and
//! the player a command acted on is told as BDS tells it ([`tell_object`]). Results that PMMP
//! broadcasts to operators (`Command::broadcastCommandMessage`) go to the players with
//! [`BROADCAST_ADMIN`](crate::permission::BROADCAST_ADMIN) as `[sender: result]`.
//!
//! Arguments are Bedrock's typed parameters, so the client autocompletes them: players are target
//! parameters (exact names and `@s`, `@a`, `@p`, `@r`, where PMMP takes a name prefix), and a
//! command on several players does its work for each. A command that queues one action for each
//! player checks first that the deferred queue has room for all of them, so it acts on all or
//! none.
//!
//! Left out: `dumpmemory`, `gc`, `plugins` and `timings` (PHP and plugins).

mod check;
mod lists;
mod players;
mod server;

pub(super) use server::WORLD_CLOCKS;

use serde_json::{Value as Json, json};

use super::{ADMIN_PREFIX, CONSOLE_NAME};
use crate::action::{self, Action};
use crate::command::{Args, Command, Error, Output, Param, Result, Sender};
use crate::config::GameMode;
use crate::console::text;
use crate::math::{self, Vec3};
use crate::permission::Permission;
use crate::player::{self, Message, Player};
use crate::protocol::consts::{GameType, TextType};
use crate::protocol::packet::{Packet, Text};
use crate::tick::Context;
use crate::tick::dispatch::MAX_DEFERRED;
use crate::world::{self, MAX_Y, MIN_Y};

/// BDS's `GameMode` enum (P6): the names, their letters and `default` (`d`), the server's default
/// game mode.
const GAME_MODE_NAMES: [&str; 9] = [
    "default",
    "creative",
    "spectator",
    "survival",
    "adventure",
    "d",
    "c",
    "s",
    "a",
];
/// The cap PMMP's commands put on counts and amounts (PMMP `VanillaCommand::MAX_COORD`); the
/// positions they take stop at the world border instead ([`in_world`]).
const MAX_COORD: i32 = 30_000_000;
/// `commands.generic.num.tooBig`: a number above its bound.
const NUM_TOO_BIG: Msg = Msg::client("commands.generic.num.tooBig");
/// `commands.generic.num.tooSmall`: a number below its bound.
const NUM_TOO_SMALL: Msg = Msg::client("commands.generic.num.tooSmall");
/// A syntax only a player may use, run by the console: the client's "invalid origin" text.
const PLAYER_USER_ONLY: Msg = Msg::client("commands.generic.invalidOrigin");

/// A command message: a key the client translates, or Anchor's own text.
#[derive(Debug, Clone, Copy)]
struct Msg {
    /// The client's translation key, or Anchor's text with `{%0}`, `{%1}`... for the
    /// parameters.
    key: &'static str,
    /// Whether `key` is Anchor's text, which is sent as it is.
    own: bool,
}

impl Msg {
    /// Makes a message the client translates (Bedrock's language files have the key).
    const fn client(key: &'static str) -> Self {
        Self { key, own: false }
    }

    /// Makes Anchor's own message, for which the client has no key: sent as its text.
    const fn own(text: &'static str) -> Self {
        Self {
            key: text,
            own: true,
        }
    }

    /// Fills the own text with `params`, in one pass so that a parameter's own text is kept as
    /// it is; `None` for a client key.
    fn render(self, params: &[&str]) -> Option<String> {
        self.own.then(|| text::fill(self.key, params))
    }

    /// Turns the message into a command result line.
    fn output(self, params: &[&str]) -> Output {
        match self.render(params) {
            Some(text) => Output::from(text),
            None => Output::new().translated(self.key, params),
        }
    }

    /// Turns the message into a command failure.
    fn error(self, params: &[&str]) -> Error {
        match self.render(params) {
            Some(text) => Error::failed(text),
            None => Error::translated(self.key, params),
        }
    }

    /// Turns the message into a chat line with `prefix` (format codes) before it. A client key
    /// goes as `<prefix>%<key>`, which the client translates in place (PMMP
    /// `Translatable::prefix`).
    fn chat(self, prefix: &str, params: &[&str]) -> Message {
        match self.render(params) {
            Some(text) => Message::Raw(format!("{prefix}{text}")),
            None => Message::Translation {
                key: if prefix.is_empty() {
                    self.key.to_owned()
                } else {
                    format!("{prefix}%{}", self.key)
                },
                parameters: params.iter().map(|p| (*p).to_owned()).collect(),
            },
        }
    }
}

/// What a command on several players did: the lines of those it acted on, or, if it acted on
/// none, why it could not act on the last (PMMP acts on one player and fails for it).
#[derive(Debug, Default)]
struct Results {
    out: Output,
    failure: Option<Error>,
}

impl Results {
    /// Adds what the command did for one player.
    fn add(&mut self, result: Result<Output>) {
        match result {
            Ok(out) => self.out.lines.extend(out.lines),
            Err(e) => self.failure = Some(e),
        }
    }

    /// Turns the results into the lines, or the failure if no player was acted on.
    fn finish(self) -> Result<Output> {
        match self.failure {
            Some(e) if self.out.lines.is_empty() => Err(e),
            _ => Ok(self.out),
        }
    }
}

/// Lists Anchor's own commands, in registration order.
pub(super) fn all() -> Vec<Command> {
    // Built once, at build.
    let mut commands = Vec::new();
    lists::add(&mut commands);
    players::add(&mut commands);
    server::add(&mut commands);
    check::add(&mut commands);
    commands
}

/// Wraps a handler that cannot fail in the form [`Command::overload`] takes.
fn infallible(
    handler: fn(&mut Context<'_>, &Sender, &Args) -> Output,
) -> impl FnMut(&mut Context<'_>, &Sender, &Args) -> Result<Output> + Send + 'static {
    move |cx, sender, args| Ok(handler(cx, sender, args))
}

/// Shows `player` `message`; a player who has left is shown nothing.
fn tell(cx: &mut Context<'_>, player: player::Id, message: Message) {
    if let Some(p) = cx.player_mut(player) {
        #[expect(
            clippy::expect_used,
            reason = "invariant: a command's message holds a command line's text and names, far below a client's bounds"
        )]
        p.send(message)
            .expect("a command's message fits a client text");
    }
}

/// Shows `player` the client's translation of `msg` (a client key) with `params`, as BDS tells
/// the player a command acted on: an object `Text` holding raw-text JSON
/// (`{"rawtext":[{"translate":key,"with":{"rawtext":[{"text":param},...]}}]}` and a line break).
/// PMMP sends no such message; the parameters come from a command line, so the text stays far
/// below what a client decodes.
fn tell_object(cx: &mut Context<'_>, player: player::Id, msg: Msg, params: &[&str]) {
    let with: Vec<Json> = params.iter().map(|p| json!({ "text": p })).collect();
    let with = if with.is_empty() {
        Json::Null
    } else {
        Json::Array(with)
    };
    let raw = json!({ "rawtext": [{ "translate": msg.key, "with": { "rawtext": with } }] });
    if let Some(p) = cx.player_mut(player) {
        p.send_packet(Packet::Text(Box::new(Text {
            text_type: TextType::Object.into(),
            message: format!("{raw}\n"),
            ..Text::default()
        })));
    }
}

/// Checks that the deferred queue has room for `actions` more, so that a command that queues
/// one action for each player queues all of them or, failing as a full queue does, none.
fn has_room(cx: &Context<'_>, actions: usize) -> Result<()> {
    if cx.state.deferred.len().saturating_add(actions) > MAX_DEFERRED {
        return Err(Error::failed(action::Error::Full.to_string()));
    }
    Ok(())
}

/// Queues `action` ([`Context::perform`]), so that its events are raised once the command has
/// returned (a command may run inside a listener: the events must not re-enter it). A full
/// queue fails the command.
fn perform(cx: &mut Context<'_>, action: impl Action) -> Result<()> {
    cx.perform(action).map_err(|e| Error::failed(e.to_string()))
}

/// Kicks `player` with `reason` (PMMP `Player::kick`): the [`PlayerKick`](crate::event::PlayerKick)
/// listeners decide after the command has returned; a player already leaving stays as it is.
fn kick(cx: &mut Context<'_>, player: player::Id, reason: &str) -> Result<()> {
    perform(cx, action::Kick::new(player, reason))
}

/// Copies the result `msg` for the operators ([`notify`]; PMMP
/// `Command::broadcastCommandMessage`) and returns it for `sender`.
fn announce(cx: &mut Context<'_>, sender: Sender, msg: Msg, params: &[&str]) -> Output {
    notify(cx, sender, msg, params);
    msg.output(params)
}

/// Copies `sender`'s result `msg` for every other player with
/// [`BROADCAST_ADMIN`](crate::permission::BROADCAST_ADMIN), and for the log when a player sent
/// the command (the console logs its own results): [`Context::broadcast_command_message`] for a
/// text, a translation for a client key ([`Context::copy_result`]). Nothing during a
/// [`Context::run_command`].
fn notify(cx: &mut Context<'_>, sender: Sender, msg: Msg, params: &[&str]) {
    let copied = if let Some(text) = msg.render(params) {
        cx.broadcast_command_message(sender, &text)
    } else {
        let source = name_of(cx, sender);
        // The client translates the key inside the text with its parameters. The brackets are
        // `chat.type.admin`'s shape, which cannot carry a translation with parameters itself.
        let copy = Message::Translation {
            key: format!("{ADMIN_PREFIX}[{source}: %{}]", msg.key),
            parameters: params.iter().map(|p| (*p).to_owned()).collect(),
        };
        cx.copy_result(sender, copy, &text::render(msg.key, params))
    };
    #[expect(
        clippy::expect_used,
        reason = "invariant: a command result holds a command line's text, names and numbers, far below a client's bounds"
    )]
    copied.expect("a copy of a command result fits a client text");
}

/// Finds the sender's name ([`Context::sender_name`]); empty for a player who has left.
fn name_of(cx: &Context<'_>, sender: Sender) -> String {
    cx.sender_name(sender).unwrap_or_default().to_owned()
}

/// Finds the name a player shows (PMMP `Player::getDisplayName`); the console's name.
fn display_name_of(cx: &Context<'_>, sender: Sender) -> String {
    match sender {
        Sender::Player(id) => cx
            .player(id)
            .map_or_else(String::new, |p| p.display_name().to_owned()),
        Sender::Console => CONSOLE_NAME.to_owned(),
    }
}

/// Finds the name of player `id`; empty once it has left.
fn player_name(cx: &Context<'_>, id: player::Id) -> String {
    name_of(cx, Sender::Player(id))
}

/// Finds the world `sender` is in: a player's own, the default world for the console.
fn sender_world(cx: &Context<'_>, sender: Sender) -> world::Id {
    match sender {
        Sender::Player(id) => cx
            .player(id)
            .map_or_else(|| cx.default_world().id(), Player::world),
        Sender::Console => cx.default_world().id(),
    }
}

/// Builds the failure of a line that fits none of `command`'s forms after all (PMMP
/// `InvalidCommandSyntaxException`).
fn usage(cx: &Context<'_>, command: &str) -> Error {
    #[expect(
        clippy::expect_used,
        reason = "invariant: a running built-in command is registered under its own name"
    )]
    let command = cx
        .command(command)
        .expect("a running built-in command is registered");
    Error::Usage {
        usage: command.usage_text(),
    }
}

/// Finds the players a command acts on (PMMP `VanillaCommand::fetchPermittedPlayerTarget`):
/// those the target parameter `param` named, or the sender itself when the line left it out (a
/// usage failure for the console). The sender needs `own` to act on itself and `other` to act on
/// anyone else, for every player, before anything changes.
fn permitted_targets(
    cx: &Context<'_>,
    sender: Sender,
    args: &Args,
    param: &str,
    (own, other): (Permission, Permission),
    command: &str,
) -> Result<Vec<player::Id>> {
    let targets = match (args.targets(param), sender) {
        (Some(targets), _) => targets.to_vec(),
        (None, Sender::Player(id)) => vec![id],
        (None, Sender::Console) => return Err(usage(cx, command)),
    };
    for &id in &targets {
        let needed = if Sender::Player(id) == sender {
            own
        } else {
            other
        };
        if !cx.has_permission(sender, needed) {
            return Err(Error::Permission {
                name: command.to_owned(),
            });
        }
    }
    Ok(targets)
}

/// Checks that `sender` has `permission`, failing with the permission failure of `command` if
/// not (PMMP `Command::testPermission`).
fn require(cx: &Context<'_>, sender: Sender, permission: Permission, command: &str) -> Result<()> {
    if cx.has_permission(sender, permission) {
        Ok(())
    } else {
        Err(Error::Permission {
            name: command.to_owned(),
        })
    }
}

/// Converts `value` to a `T` if it is from `min` to `max`, otherwise fails as PMMP does
/// (`getBoundedInt`).
fn bounded<T>(value: i32, min: T, max: T) -> Result<T>
where
    T: Copy + Into<i32> + TryFrom<i32>,
    <T as TryFrom<i32>>::Error: std::fmt::Debug,
{
    let (low, high) = (min.into(), max.into());
    if value > high {
        return Err(NUM_TOO_BIG.error(&[&value.to_string(), &high.to_string()]));
    }
    if value < low {
        return Err(NUM_TOO_SMALL.error(&[&value.to_string(), &low.to_string()]));
    }
    #[expect(
        clippy::expect_used,
        reason = "invariant: a value from `min` to `max` fits their type"
    )]
    Ok(T::try_from(value).expect("a value between two of a type's values fits the type"))
}

/// Keeps a position a command takes inside the world border and height (PMMP
/// `VanillaCommand::getRelativeDouble`'s bounds).
fn in_world(v: Vec3) -> Vec3 {
    let border = f64::from(world::BORDER);
    Vec3::new(
        math::clamp(v.x, -border, border),
        math::clamp(v.y, f64::from(MIN_Y), f64::from(MAX_Y) + 1.0),
        math::clamp(v.z, -border, border),
    )
}

/// Writes `v` with two decimals, as BDS prints coordinates: `1.50`, `-3.00`, `0.33` (PMMP prints
/// `round($v, 2)`: `1.5`).
fn two_decimals(v: f64) -> String {
    format!("{v:.2}")
}

/// Builds the `gameMode` parameter of the game mode commands: one of [`GAME_MODE_NAMES`].
fn game_mode_param() -> Param {
    Param::enumeration("gameMode", "GameMode", &GAME_MODE_NAMES)
}

/// Reads the game mode a `gameMode` argument names: a name or letter, or a Bedrock game type
/// number (0 survival, 1 creative, 2 adventure, 6 spectator); `default` (`d`, 5) gives `default`.
fn game_mode_arg(args: &Args, default: Option<GameMode>) -> Option<GameMode> {
    let game_type = args.int("gameMode").map(GameType::try_from);
    match (args.string("gameMode"), game_type) {
        (Some("default" | "d"), _) | (_, Some(Ok(GameType::Default))) => default,
        (Some(name), _) => GameMode::from_name(name),
        (_, Some(Ok(game_type))) => GameMode::from_game_type(game_type),
        _ => None,
    }
}

/// Names the game mode for the client to translate, as BDS sends it (P6:
/// `createWorldScreen.gameMode.*`; PMMP `GameMode::getTranslatableName` sends `gameMode.*`).
fn game_mode_name(mode: GameMode) -> &'static str {
    match mode {
        GameMode::Survival => "%createWorldScreen.gameMode.survival",
        GameMode::Creative => "%createWorldScreen.gameMode.creative",
        GameMode::Adventure => "%createWorldScreen.gameMode.adventure",
        GameMode::Spectator => "%createWorldScreen.gameMode.spectator",
    }
}

#[cfg(test)]
mod tests;
