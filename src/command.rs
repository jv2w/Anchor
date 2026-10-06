//! Commands: what players and the console type after `/`, with typed arguments and
//! autocompletion.
//!
//! A [`Command`] has a name, aliases, a [`Permission`] and one or more overloads: a list of
//! [`Param`]s and the handler that runs when a line fits them. Register it with
//! [`ServerBuilder::command`](crate::ServerBuilder::command):
//!
//! ```
//! use anchor::command::{Command, Output, Param};
//! use anchor::permission::{Default, Permission};
//!
//! const HEAL: Permission = Permission::new("lobby.heal", Default::Op);
//! let heal = Command::new("heal", "Heals players")
//!     .permission(HEAL)
//!     .overload(&[Param::target("player").optional()], |cx, sender, args| {
//!         let _ = (cx, sender, args.targets("player"));
//!         Ok(Output::from("Healed"))
//!     });
//! # let _ = heal;
//! ```
//!
//! A line is split into words like Bedrock does: spaces separate words, double quotes keep one
//! word with spaces (`"Steve Jobs"`), and inside quotes `\"` and `\\` stand for `"` and `\`. The
//! first overload whose parameters the words fit runs. Players get the result as a
//! `CommandOutput` packet; the console's goes to the log. Each player's client autocompletes the
//! commands that player may use (`AvailableCommands`), sent again when its permissions change.
//!
//! The server also has Anchor's own commands, PMMP's default ones (`/give`, `/tp`, `/ban`, ...),
//! with the permissions of [`permission`](crate::permission). A command of the user's with the
//! same name replaces one, and
//! [`ServerBuilder::without_command`](crate::ServerBuilder::without_command) drops one.

pub(crate) mod parse;

use std::fmt;

use crate::block;
use crate::effect;
use crate::item;
use crate::math::Vec3;
use crate::permission::Permission;
use crate::player;
use crate::tick::Context;

/// The most overloads one command has.
pub(crate) const MAX_OVERLOADS: usize = 64;
/// The most parameters one overload has.
pub(crate) const MAX_PARAMS: usize = 32;
/// The longest command name or alias, in bytes.
pub(crate) const MAX_NAME: usize = 64;
/// The most values one enum parameter offers.
pub(crate) const MAX_ENUM_VALUES: usize = 4096;
/// The most permissions one command names.
pub(crate) const MAX_PERMISSIONS: usize = 16;
/// The client's "unknown command" text, filled with the name typed (BDS).
const UNKNOWN: &str = "commands.generic.unknown";
/// The client's "no permission" text, filled with the command's name (BDS).
const NO_PERMISSION: &str = "commands.generic.error.permissions";
/// The client's "usage" text, filled with the usage.
const USAGE: &str = "commands.generic.usage";
/// The client's text for a line that does not fit a command: before, the word, after (BDS).
const SYNTAX: &str = "commands.generic.syntax";
/// The client's "player not found" text.
const PLAYER_NOT_FOUND: &str = "commands.generic.player.notFound";
/// The client's "no targets matched" text.
const NO_TARGETS: &str = "commands.generic.noTargetMatch";

/// What runs a command: it gets the game, who sent the command and the parsed arguments, and
/// returns what the sender sees.
pub(crate) type Handler = Box<dyn FnMut(&mut Context<'_>, &Sender, &Args) -> Result<Output> + Send>;

/// A command: its name, description, aliases, permission and overloads.
#[doc(alias = "ClosureCommand")]
// pmmp-class: Command
pub struct Command {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) aliases: Vec<String>,
    /// A sender needs one of them; none: everyone may use the command.
    pub(crate) permissions: Vec<Permission>,
    pub(crate) usage: Option<String>,
    pub(crate) overloads: Vec<Overload>,
}

/// One way to call a command: its parameters and its handler. The handler is taken out while it
/// runs, so a command that dispatches itself again finds it missing.
pub(crate) struct Overload {
    pub(crate) params: Vec<Param>,
    pub(crate) handler: Option<Handler>,
}

impl Command {
    /// A command named `name` (ASCII case does not matter; Bedrock shows it in lowercase) with
    /// `description` shown in the client's command list. Without a [`permission`](Self::permission)
    /// everyone may use it; without an [`overload`](Self::overload), or with a description longer
    /// than 1,000 bytes, the server refuses to build.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into().to_ascii_lowercase(),
            description: description.into(),
            aliases: Vec::new(),
            permissions: Vec::new(),
            usage: None,
            overloads: Vec::new(),
        }
    }

    /// Adds another name for the command.
    #[doc(alias = "Command::setAliases")]
    #[must_use]
    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into().to_ascii_lowercase());
        self
    }

    /// Lets only senders with `permission` use the command (and see it in their command list).
    /// Called again, it adds another: a sender needs any one of them (PMMP `setPermissions`), and
    /// the handler checks which one an argument needs, such as `/give`'s permissions for giving
    /// oneself and others. A command names at most 16.
    #[doc(alias = "Command::setPermission")]
    #[doc(alias = "Command::setPermissions")]
    #[must_use]
    pub fn permission(mut self, permission: Permission) -> Self {
        if !self.permissions.contains(&permission) {
            self.permissions.push(permission);
        }
        self
    }

    /// The usage shown when a line fits no overload, such as `/heal [player]`. Without it the
    /// usage is made from the overloads.
    #[doc(alias = "Command::setUsage")]
    #[must_use]
    pub fn usage(mut self, usage: impl Into<String>) -> Self {
        self.usage = Some(usage.into());
        self
    }

    /// Adds an overload: `params` in order, and `handler`, which runs when a line fits them.
    /// Overloads are tried in the order they were added. A required parameter may not follow an
    /// optional one, and a [`message`](Param::message) or [`raw_text`](Param::raw_text) parameter
    /// takes the rest of the line, so it must be the last.
    #[must_use]
    pub fn overload(
        mut self,
        params: &[Param],
        handler: impl FnMut(&mut Context<'_>, &Sender, &Args) -> Result<Output> + Send + 'static,
    ) -> Self {
        self.overloads.push(Overload {
            params: params.to_vec(),
            handler: Some(Box::new(handler)),
        });
        self
    }

    /// The name.
    #[doc(alias = "Command::getName")]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The description.
    #[doc(alias = "Command::getDescription")]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The other names.
    #[doc(alias = "Command::getAliases")]
    pub fn aliases(&self) -> &[String] {
        &self.aliases
    }

    /// The permissions a sender needs one of; empty if everyone may use the command.
    #[doc(alias = "Command::getPermissions")]
    pub fn permissions(&self) -> &[Permission] {
        &self.permissions
    }

    /// The usage shown when a line fits no overload: the one set, or one made from the
    /// overloads, such as `/give <player: target> <item: Item> [amount: int]`, one per line.
    #[doc(alias = "Command::getUsage")]
    pub fn usage_text(&self) -> String {
        if let Some(usage) = &self.usage {
            return usage.clone();
        }
        let mut out = String::new();
        for (i, overload) in self.overloads.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push('/');
            out.push_str(&self.name);
            for param in &overload.params {
                out.push(' ');
                param.write_usage(&mut out);
            }
        }
        out
    }
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Command")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("aliases", &self.aliases)
            .field("permissions", &self.permissions)
            .field("usage", &self.usage)
            .field(
                "overloads",
                &self.overloads.iter().map(|o| &o.params).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// A parameter of an overload: its name, what it takes and whether it may be left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub(crate) name: String,
    pub(crate) kind: Kind,
    pub(crate) optional: bool,
}

/// What a parameter takes; each is a Bedrock `AvailableCommands` argument type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A 32-bit whole number.
    Int,
    /// A finite number.
    Float,
    /// One word, or words in quotes.
    String,
    /// `true` or `false`.
    Bool,
    /// Players: a name or `@s`, `@a`, `@p`, `@r`.
    Target,
    /// A player's name, online or not; a target for the client.
    Name,
    /// A 32-bit whole number with `suffix` after it, such as `10L`.
    Suffixed { suffix: String },
    /// Three coordinates, each a number, `~` or `~` and a number.
    Position,
    /// The rest of the line, as a chat message.
    Message,
    /// The rest of the line, as typed.
    RawText,
    /// One JSON value.
    Json,
    /// One of fixed words; `name` is the type the client shows.
    Enum { name: String, values: Vec<String> },
    /// An item name.
    Item,
    /// A block name.
    Block,
    /// An effect name.
    Effect,
    /// An enchantment name.
    Enchantment,
}

impl Param {
    fn new(name: impl Into<String>, kind: Kind) -> Self {
        Self {
            name: name.into(),
            kind,
            optional: false,
        }
    }

    /// A 32-bit whole number ([`Args::int`]).
    pub fn int(name: impl Into<String>) -> Self {
        Self::new(name, Kind::Int)
    }

    /// A finite number ([`Args::float`]).
    pub fn float(name: impl Into<String>) -> Self {
        Self::new(name, Kind::Float)
    }

    /// One word, or words in double quotes ([`Args::string`]).
    pub fn string(name: impl Into<String>) -> Self {
        Self::new(name, Kind::String)
    }

    /// `true` or `false` ([`Args::bool`]).
    pub fn bool(name: impl Into<String>) -> Self {
        Self::new(name, Kind::Bool)
    }

    /// Online players ([`Args::targets`]): a player's name (ASCII case does not matter), `@s`
    /// (the sender), `@a` (everyone), `@p` (the nearest to the sender in its world) or `@r` (one
    /// at random). Selector arguments (`@a[...]`) are not supported.
    pub fn target(name: impl Into<String>) -> Self {
        Self::new(name, Kind::Target)
    }

    /// A player's name, online or not, as typed ([`Args::string`]), for commands on players who
    /// may be offline, such as `/op` and `/ban`. The client offers it as a target (online
    /// players); a selector (`@a`) does not fit.
    pub fn player_name(name: impl Into<String>) -> Self {
        Self::new(name, Kind::Name)
    }

    /// A 32-bit whole number with `suffix` right after it ([`Args::int`]), such as `/xp`'s `10L`
    /// with the suffix `l` (ASCII case does not matter). A suffix other than 1 to 64 ASCII letters
    /// refuses the build ([`Error::InvalidOverload`]).
    pub fn suffixed_int(name: impl Into<String>, suffix: &str) -> Self {
        Self::new(
            name,
            Kind::Suffixed {
                suffix: suffix.to_ascii_lowercase(),
            },
        )
    }

    /// A position ([`Args::position`]): three coordinates, each a number, or `~` and an optional
    /// number for one relative to the sender's feet (the default world's spawn for the console).
    /// Local coordinates (`^`) are not supported.
    pub fn position(name: impl Into<String>) -> Self {
        Self::new(name, Kind::Position)
    }

    /// The rest of the line as a chat message ([`Args::string`]); the last parameter.
    pub fn message(name: impl Into<String>) -> Self {
        Self::new(name, Kind::Message)
    }

    /// The rest of the line as typed ([`Args::string`]); the last parameter.
    pub fn raw_text(name: impl Into<String>) -> Self {
        Self::new(name, Kind::RawText)
    }

    /// One JSON value, which may hold spaces ([`Args::string`] gives its text).
    pub fn json(name: impl Into<String>) -> Self {
        Self::new(name, Kind::Json)
    }

    /// One of `values` ([`Args::string`] gives the value as listed, whatever ASCII case was
    /// typed). `type_name` is the enum type the client shows: every enum of a type name must have
    /// the same values in the same order, or [`build`](crate::ServerBuilder::build) fails with
    /// [`Error::ConflictingEnum`]. Anchor's own commands use BDS's names and values: `Boolean`,
    /// `Item`, `Block`, `Effect`, `Enchant`, `GameMode`, `Difficulty`, `ClearEffects`,
    /// `AddInfiniteEffect`, `TitleClear`, `TitleReset`, `TitleSet`, `TitleTimes`, `TimeModeAdd`, `TimeModeSet`,
    /// `TimeModeQuery`, `TimeSpec`, `TimeQuery` and `<name>CommandAliases` for a command with
    /// aliases, and Anchor's `TimeModeStart`, `TimeModeStop`, `BanListType`, `WhitelistAction`,
    /// `WhitelistEdit` and `ParticleName`, unless the user replaces or drops the command.
    /// `CommandName` is the client's list of command names: no parameter may take it.
    pub fn enumeration(
        name: impl Into<String>,
        type_name: impl Into<String>,
        values: &[&str],
    ) -> Self {
        Self::new(
            name,
            Kind::Enum {
                name: type_name.into(),
                values: values.iter().map(|v| (*v).to_owned()).collect(),
            },
        )
    }

    /// The word `word` itself, as in `/whitelist add <player>`: an enum of one value named
    /// after it.
    pub fn literal(word: &str) -> Self {
        Self::enumeration(word, word, &[word])
    }

    /// An item name such as `diamond_sword` or `minecraft:stone` ([`Args::item`]).
    pub fn item(name: impl Into<String>) -> Self {
        Self::new(name, Kind::Item)
    }

    /// A block name such as `oak_planks` ([`Args::block`]).
    pub fn block(name: impl Into<String>) -> Self {
        Self::new(name, Kind::Block)
    }

    /// An effect name such as `speed` ([`Args::effect`]).
    pub fn effect(name: impl Into<String>) -> Self {
        Self::new(name, Kind::Effect)
    }

    /// An enchantment name such as `sharpness` ([`Args::enchantment`]).
    pub fn enchantment(name: impl Into<String>) -> Self {
        Self::new(name, Kind::Enchantment)
    }

    /// The parameter, but one a line may leave out (only at the end).
    #[must_use]
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    /// The name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether a line may leave it out.
    pub fn is_optional(&self) -> bool {
        self.optional
    }

    /// Whether it takes the rest of the line.
    pub(crate) fn takes_rest(&self) -> bool {
        matches!(self.kind, Kind::Message | Kind::RawText)
    }

    /// `<name: type>`, or `[name: type]` when optional.
    fn write_usage(&self, out: &mut String) {
        let suffixed;
        let ty = match &self.kind {
            Kind::Int => "int",
            Kind::Suffixed { suffix } => {
                suffixed = format!("int{suffix}");
                &suffixed
            }
            Kind::Float => "float",
            Kind::String => "string",
            Kind::Bool => "Boolean",
            Kind::Target | Kind::Name => "target",
            Kind::Position => "x y z",
            Kind::Message => "message",
            Kind::RawText => "text",
            Kind::Json => "json",
            Kind::Enum { values, .. } if values.len() == 1 => {
                out.push_str(&self.name);
                return;
            }
            Kind::Enum { name, .. } => name,
            Kind::Item => "Item",
            Kind::Block => "Block",
            Kind::Effect => "Effect",
            Kind::Enchantment => "Enchant",
        };
        let (open, close) = if self.optional {
            ('[', ']')
        } else {
            ('<', '>')
        };
        out.push(open);
        out.push_str(&self.name);
        out.push_str(": ");
        out.push_str(ty);
        out.push(close);
    }
}

/// Who sent a command.
#[doc(alias = "CommandSender")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Sender {
    /// A player, by a `/` line in chat.
    Player(player::Id),
    /// The server console (standard input, `console-input`), which counts as an operator.
    #[doc(alias = "ConsoleCommandSender")]
    Console,
}

/// The arguments of the overload that ran, by parameter name. A getter gives `None` for a name
/// the overload does not have, for a parameter of another kind, and for an optional parameter
/// the line left out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Args {
    /// In parameter order; at most [`MAX_PARAMS`], so a lookup looks at a few entries.
    pub(crate) values: Vec<(String, Value)>,
}

/// A parsed argument.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Int(i32),
    Float(f64),
    Text(String),
    Bool(bool),
    Targets(Vec<player::Id>),
    Position(Vec3),
    Item(item::Type),
    Block(block::Kind),
    Effect(effect::Type),
    Enchantment(item::Enchantment),
}

impl Args {
    fn get(&self, name: &str) -> Option<&Value> {
        self.values.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    /// Whether the line gave the parameter `name`.
    pub fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// A [`Param::int`] or [`Param::suffixed_int`] (its number).
    pub fn int(&self, name: &str) -> Option<i32> {
        match self.get(name)? {
            Value::Int(v) => Some(*v),
            _ => None,
        }
    }

    /// A [`Param::float`].
    pub fn float(&self, name: &str) -> Option<f64> {
        match self.get(name)? {
            Value::Float(v) => Some(*v),
            _ => None,
        }
    }

    /// The text of a [`Param::string`], [`player_name`](Param::player_name),
    /// [`message`](Param::message), [`raw_text`](Param::raw_text), [`json`](Param::json) or
    /// [`enumeration`](Param::enumeration).
    pub fn string(&self, name: &str) -> Option<&str> {
        match self.get(name)? {
            Value::Text(v) => Some(v),
            _ => None,
        }
    }

    /// A [`Param::bool`].
    pub fn bool(&self, name: &str) -> Option<bool> {
        match self.get(name)? {
            Value::Bool(v) => Some(*v),
            _ => None,
        }
    }

    /// The players a [`Param::target`] named, at least one, in player order.
    pub fn targets(&self, name: &str) -> Option<&[player::Id]> {
        match self.get(name)? {
            Value::Targets(v) => Some(v),
            _ => None,
        }
    }

    /// A [`Param::position`], relative coordinates resolved, finite.
    pub fn position(&self, name: &str) -> Option<Vec3> {
        match self.get(name)? {
            Value::Position(v) => Some(*v),
            _ => None,
        }
    }

    /// A [`Param::item`].
    pub fn item(&self, name: &str) -> Option<item::Type> {
        match self.get(name)? {
            Value::Item(v) => Some(*v),
            _ => None,
        }
    }

    /// A [`Param::block`].
    pub fn block(&self, name: &str) -> Option<block::Kind> {
        match self.get(name)? {
            Value::Block(v) => Some(*v),
            _ => None,
        }
    }

    /// A [`Param::effect`].
    pub fn effect(&self, name: &str) -> Option<effect::Type> {
        match self.get(name)? {
            Value::Effect(v) => Some(*v),
            _ => None,
        }
    }

    /// A [`Param::enchantment`].
    pub fn enchantment(&self, name: &str) -> Option<item::Enchantment> {
        match self.get(name)? {
            Value::Enchantment(v) => Some(*v),
            _ => None,
        }
    }
}

/// What a command shows its sender when it succeeds: lines of text, each plain or a translation
/// key the client fills with parameters (like `commands.give.success`). Empty shows nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub(crate) lines: Vec<Line>,
}

/// One line of output or an error message: text or a translation key, and its parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    pub(crate) text: String,
    pub(crate) params: Vec<String>,
}

impl Output {
    /// No output.
    pub fn new() -> Self {
        Self::default()
    }

    /// The output with `text` as another line.
    #[must_use]
    pub fn line(mut self, text: impl Into<String>) -> Self {
        self.lines.push(Line {
            text: text.into(),
            params: Vec::new(),
        });
        self
    }

    /// The output with another line: the client's translation of `key` filled with `params`.
    /// The console log shows the key and the parameters.
    #[must_use]
    pub fn translated(mut self, key: impl Into<String>, params: &[&str]) -> Self {
        self.lines.push(Line {
            text: key.into(),
            params: params.iter().map(|p| (*p).to_owned()).collect(),
        });
        self
    }

    /// The lines' texts (or keys), in order.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(|l| l.text.as_str())
    }
}

impl From<&str> for Output {
    fn from(text: &str) -> Self {
        Self::new().line(text)
    }
}

impl From<String> for Output {
    fn from(text: String) -> Self {
        Self::new().line(text)
    }
}

/// Why a command did not run or failed, or why the commands registered on a
/// [`ServerBuilder`](crate::ServerBuilder) cannot be built.
///
/// The first kinds are what a sender sees (in red): the client's translation where it has a text
/// for the case, as BDS sends it, and Anchor's text otherwise; a handler fails with
/// [`Error::failed`] or [`Error::translated`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// No command has this name or alias.
    Unknown {
        /// The name typed.
        name: String,
    },
    /// The sender lacks the command's permission.
    Permission {
        /// The command's name.
        name: String,
    },
    /// The line fits none of the command's overloads: the word where it stops fitting the
    /// overload that fits furthest, with up to 10 characters of the line (from its `/`) before
    /// and after it, as BDS reports it (`commands.generic.syntax`).
    Syntax {
        /// Up to 10 characters before the word.
        before: String,
        /// The word; empty where the line ends before a required word.
        word: String,
        /// Up to 10 characters after the word.
        after: String,
    },
    /// A command's handler found the arguments wrong after they fitted its overload: the
    /// command's usage.
    Usage {
        /// The command's usage text ([`Command::usage_text`]).
        usage: String,
    },
    /// The line is longer than 512 characters or 2,048 bytes.
    TooLong,
    /// A quote is not closed.
    UnclosedQuote,
    /// No online player has the name a target parameter gave.
    PlayerNotFound {
        /// The name typed.
        name: String,
    },
    /// A target selector matched nobody (`@s` and `@p` from the console, or nobody online).
    NoTargets,
    /// The handler failed, with a message for the sender: text, or a translation key and its
    /// parameters.
    Failed {
        /// The text or translation key.
        message: String,
        /// The translation parameters; empty for text.
        params: Vec<String>,
    },
    /// A command or alias name is empty, too long (64 bytes) or holds spaces, control characters
    /// or `/`.
    InvalidName {
        /// The name.
        name: String,
    },
    /// Two registered commands share a name or alias.
    Duplicate {
        /// The name.
        name: String,
    },
    /// An overload breaks a rule of [`Command::overload`], the command has none, or it names
    /// more than 16 permissions.
    InvalidOverload {
        /// The command's name.
        command: String,
        /// What is wrong.
        reason: &'static str,
    },
    /// A command's description is longer than a client takes (1,000 bytes).
    DescriptionTooLong {
        /// The command's name.
        command: String,
    },
    /// Two permissions with the same name have different defaults.
    ConflictingPermission {
        /// The permission's name.
        name: &'static str,
    },
    /// Two enum parameters with the same type name have different values.
    ConflictingEnum {
        /// The enum's type name.
        name: String,
    },
}

/// A result with the command [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// A failure showing `message` to the sender.
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
            params: Vec::new(),
        }
    }

    /// A failure showing the client's translation of `key` filled with `params`.
    pub fn translated(key: impl Into<String>, params: &[&str]) -> Self {
        Self::Failed {
            message: key.into(),
            params: params.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    /// What the sender sees: the client's translation where it has a text for the case (filled
    /// as BDS fills it), Anchor's text otherwise, or the handler's message and parameters.
    pub(crate) fn line(&self) -> Line {
        let text = match self {
            Self::Unknown { name } => {
                return Line {
                    text: UNKNOWN.to_owned(),
                    params: vec![name.clone()],
                };
            }
            Self::Permission { name } => {
                return Line {
                    text: NO_PERMISSION.to_owned(),
                    params: vec![name.clone()],
                };
            }
            Self::Syntax {
                before,
                word,
                after,
            } => {
                return Line {
                    text: SYNTAX.to_owned(),
                    params: vec![before.clone(), word.clone(), after.clone()],
                };
            }
            Self::Usage { usage } => {
                return Line {
                    text: USAGE.to_owned(),
                    params: vec![usage.clone()],
                };
            }
            Self::TooLong => "The command is too long".to_owned(),
            Self::UnclosedQuote => "Syntax error: a quote is not closed".to_owned(),
            Self::PlayerNotFound { .. } => PLAYER_NOT_FOUND.to_owned(),
            Self::NoTargets => NO_TARGETS.to_owned(),
            Self::Failed { message, params } => {
                return Line {
                    text: message.clone(),
                    params: params.clone(),
                };
            }
            Self::InvalidName { .. }
            | Self::Duplicate { .. }
            | Self::InvalidOverload { .. }
            | Self::DescriptionTooLong { .. }
            | Self::ConflictingPermission { .. }
            | Self::ConflictingEnum { .. } => self.to_string(),
        };
        Line {
            text,
            params: Vec::new(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown { name } => write!(f, "unknown command {name:?}"),
            Self::Permission { name } => write!(f, "no permission for command {name:?}"),
            Self::Syntax { word, .. } => write!(f, "the line does not fit at {word:?}"),
            Self::Usage { usage } => write!(f, "the arguments do not fit; usage: {usage}"),
            Self::TooLong => f.write_str("the command line is too long"),
            Self::UnclosedQuote => f.write_str("a quote is not closed"),
            Self::PlayerNotFound { name } => write!(f, "no player named {name:?} is online"),
            Self::NoTargets => f.write_str("the target selector matched nobody"),
            Self::Failed { message, params } if params.is_empty() => {
                write!(f, "the command failed: {message}")
            }
            Self::Failed { message, params } => {
                write!(f, "the command failed: {message} {params:?}")
            }
            Self::InvalidName { name } => write!(f, "invalid command name {name:?}"),
            Self::Duplicate { name } => write!(f, "two commands are named {name:?}"),
            Self::InvalidOverload { command, reason } => {
                write!(f, "invalid overload of command {command:?}: {reason}")
            }
            Self::DescriptionTooLong { command } => {
                write!(
                    f,
                    "the description of command {command:?} is longer than a client takes"
                )
            }
            Self::ConflictingPermission { name } => {
                write!(f, "permission {name:?} is registered with two defaults")
            }
            Self::ConflictingEnum { name } => {
                write!(f, "enum type {name:?} is registered with two value lists")
            }
        }
    }
}

impl std::error::Error for Error {}
