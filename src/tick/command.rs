//! Commands in the tick: the registered commands and permissions, `CommandRequest`, the console
//! lines and `AvailableCommands`, and the operator list, bans and whitelist with the join policy
//! that reads them.
//!
//! A command runs in the tick's serial step, where its input arrived: a player's
//! `CommandRequest` in the packet inputs, a console line as its own input
//! ([`InputKind::Console`](super::queue::InputKind::Console)), both recorded in the input log, so
//! a replay runs the same commands at the same points.

mod access;
mod available;
mod builtin;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use tracing::{debug, info};

pub(crate) use self::access::Access;
pub(crate) use self::available::{Listing, available_commands};
use super::checkpoint::{self, Codec, Reader, Writer};
use super::queue::{Identity, SessionId};
use super::{Context, Phase, PlayerKey, State, chat, dispatch, spawn};
use crate::ServerBuilder;
use crate::command::parse::{self, Coord, Names, Raw, Target};
use crate::command::{
    self, Args, Command, Error, Kind, Line, MAX_ENUM_VALUES, MAX_NAME, MAX_OVERLOADS, MAX_PARAMS,
    MAX_PERMISSIONS, Output, Sender, Value,
};
use crate::config::{Difficulty, GameMode};
use crate::console::text;
use crate::math::Vec3;
use crate::permission::{self, BanEntry, Permission};
use crate::player::{self, MAX_TEXT};
use crate::protocol::consts::{CommandOutputType, DisconnectReason};
use crate::protocol::packet::{
    CommandOutput, CommandRequest, Disconnect, Packet, PlayStatus, SetDifficulty,
};
use crate::protocol::{COMMAND_DESCRIPTION_MAX_LEN, CommandOrigin, CommandOutputMessage};
use crate::random::{self, Random};
use crate::save::{AccessData, AccessWrite};
use crate::storage::AccessKey;

/// Permissions one player may be granted or denied while online.
const MAX_GRANTS: usize = 1024;
/// `PlayStatus` that refuses a join because the server is full (gophertunnel
/// `PlayStatusLoginFailedServerFull`).
const PLAY_STATUS_SERVER_FULL: i32 = 7;
/// The random stream of `@r`.
const RANDOM_TARGET: u64 = random::tag("command-random-target");
/// What the console is called in messages (PMMP `ConsoleCommandSender::getName`).
const CONSOLE_NAME: &str = "Console";
/// The prefix of a copy of a command result for operators: grey italics, as the client shows
/// its own `chat.type.admin` lines.
const ADMIN_PREFIX: &str = "§7§o";
/// The client's `[sender: message]` text of a copy of a command result for operators.
const ADMIN_KEY: &str = "chat.type.admin";
/// The most lines of one command result a sender gets; the lines past it are replaced by one
/// telling how many were left out (a client lists at most 1,024 in a `CommandOutput`).
const MAX_OUTPUT_LINES: usize = 64;
/// The most parameters of one line of a command result a sender gets.
const MAX_OUTPUT_PARAMS: usize = 32;

/// The grants and denials of online players by session (PMMP permission attachments).
pub(crate) type Grants = BTreeMap<SessionId, BTreeMap<&'static str, bool>>;

/// Where a registered command comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// The user's: it must not clash with another of the user's.
    User,
    /// Anchor's own: skipped when the user has a command of the name or dropped it, and it loses
    /// the aliases the user's commands have.
    Builtin,
}

/// The registered commands and permissions, fixed when the server is built, and the state that
/// goes with them: runtime permission grants, the operator list, the bans and the whitelist.
#[derive(Debug)]
pub(crate) struct Commands {
    /// The commands, the user's first, then Anchor's own.
    list: Vec<Command>,
    /// The command of each name and alias.
    by_name: BTreeMap<String, usize>,
    /// Names of Anchor's commands the user dropped.
    removed: BTreeSet<String>,
    /// Every registered permission by name.
    permissions: BTreeMap<&'static str, Permission>,
    /// The first registration error, which `finish` reports.
    error: Option<Error>,
    /// Item, block, effect and enchantment names.
    names: Names,
    /// The network form of the commands, built by [`finish`](Self::finish).
    catalog: available::Catalog,
    /// Grants and denials of online players (PMMP permission attachments), not saved; a player's
    /// go when it leaves ([`left`]).
    grants: Grants,
    /// Players whose permissions changed this tick: they get `AvailableCommands` again.
    resend: BTreeSet<SessionId>,
    /// Commands run since start: the key of `@r`'s random stream.
    runs: u64,
    /// The operator list, the bans and the whitelist.
    access: Access,
    /// Whether periodic saves are on (`/save-on`, `/save-off`); not saved.
    auto_save: bool,
    /// Whether a command or callback asked the server to stop after this tick.
    stopping: bool,
    /// The tick of the first tick boundary: where the server's uptime counts from.
    started: Option<u64>,
}

impl Default for Commands {
    fn default() -> Self {
        Self {
            list: Vec::new(),
            by_name: BTreeMap::new(),
            removed: BTreeSet::new(),
            permissions: permission::BUILTIN.iter().map(|p| (p.name(), *p)).collect(),
            error: None,
            names: Names::new(),
            catalog: available::Catalog::default(),
            grants: BTreeMap::new(),
            resend: BTreeSet::new(),
            runs: 0,
            access: Access::default(),
            auto_save: true,
            stopping: false,
            started: None,
        }
    }
}

impl Commands {
    /// Registers `command` from `origin` ([`Origin`] tells how a clash is handled). The first
    /// error is kept for [`finish`](Self::finish).
    fn register(&mut self, mut command: Command, origin: Origin) {
        if let Err(e) = check(&command) {
            self.error.get_or_insert(e);
            return;
        }
        let builtin = origin == Origin::Builtin;
        if builtin
            && (self.removed.contains(&command.name) || self.by_name.contains_key(&command.name))
        {
            return;
        }
        let mut names = vec![command.name.clone()];
        for alias in std::mem::take(&mut command.aliases) {
            if self.by_name.contains_key(&alias) {
                if !builtin {
                    self.error.get_or_insert(Error::Duplicate { name: alias });
                    return;
                }
            } else if !names.contains(&alias) {
                command.aliases.push(alias.clone());
                names.push(alias);
            }
        }
        if !builtin && self.by_name.contains_key(&command.name) {
            self.error.get_or_insert(Error::Duplicate {
                name: command.name.clone(),
            });
            return;
        }
        for &p in &command.permissions {
            self.add_permission(p);
        }
        let index = self.list.len();
        for name in names {
            self.by_name.insert(name, index);
        }
        self.list.push(command);
    }

    /// Registers `permission`; another permission of the name with another default is an error.
    fn add_permission(&mut self, permission: Permission) {
        match self.permissions.get(permission.name()) {
            Some(known) if *known != permission => {
                self.error.get_or_insert(Error::ConflictingPermission {
                    name: permission.name(),
                });
            }
            Some(_) => {}
            None => {
                self.permissions.insert(permission.name(), permission);
            }
        }
    }

    /// Registers Anchor's own commands the user kept, builds the commands' network form and
    /// reports the first registration error. `ServerBuilder::build` calls it once.
    pub(crate) fn finish(&mut self) -> command::Result<()> {
        for command in builtin::all() {
            self.register(command, Origin::Builtin);
        }
        if let Some(e) = self.error.take() {
            return Err(e);
        }
        self.catalog = available::Catalog::new(&self.list, &self.names)?;
        Ok(())
    }

    /// What a player's `AvailableCommands` is built from, without the handlers, so that the
    /// output phase can build it on its threads.
    pub(crate) fn listing(&self) -> Listing<'_> {
        Listing {
            catalog: &self.catalog,
            grants: &self.grants,
            access: &self.access,
        }
    }

    /// Takes the operator list, the bans and the whitelist the server file held, and whether the
    /// whitelist is on (`white-list`). `Server::run` calls it once, before the tick starts.
    pub(crate) fn start(&mut self, access: AccessData, whitelist_enabled: bool) {
        self.access = Access::new(access, whitelist_enabled);
    }

    /// The operator, ban and whitelist changes since the last save snapshot.
    pub(super) fn take_access_changes(&mut self) -> Vec<(AccessKey, AccessWrite)> {
        self.access.take_changes()
    }

    /// Whether periodic saves are on: the save snapshot due every `save-interval-minutes` is
    /// taken only then (PMMP `WorldManager::getAutoSave`).
    pub(super) fn auto_save(&self) -> bool {
        self.auto_save
    }

    /// Whether the server should stop after this tick ([`Context::shutdown`]).
    pub(super) fn stop_requested(&self) -> bool {
        self.stopping
    }

    /// Hashes the state (state hash): grants, pending resends, the run count, the lists and the
    /// save and stop switches. The commands and permissions are code, fixed at build; the names
    /// and the catalog are tables; the start tick belongs to the process (a replay starts its
    /// own), like the uptime it gives.
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            list: _,
            by_name: _,
            removed: _,
            permissions: _,
            error: _,
            names: _,
            catalog: _,
            grants,
            resend,
            runs,
            access,
            auto_save,
            stopping,
            started: _,
        } = self;
        h.usize(grants.len());
        for (session, grants) in grants {
            h.u64(session.get());
            h.usize(grants.len());
            for (name, value) in grants {
                h.bytes(name.as_bytes());
                h.u64(u64::from(*value));
            }
        }
        h.usize(resend.len());
        for session in resend {
            h.u64(session.get());
        }
        h.u64(*runs);
        access.hash_into(h);
        h.u64(u64::from(*auto_save));
        h.u64(u64::from(*stopping));
    }
}

impl Commands {
    /// Writes what changes while the server runs (as `Self::hash_into` hashes it, and the
    /// start tick): the commands and permissions are code, fixed at build.
    pub(super) fn checkpoint(&self, w: &mut Writer) {
        let Self {
            list: _,
            by_name: _,
            removed: _,
            permissions: _,
            error: _,
            names: _,
            catalog: _,
            grants,
            resend,
            runs,
            access,
            auto_save,
            stopping,
            started,
        } = self;
        w.len(grants.len());
        for (session, grants) in grants {
            session.encode(w);
            w.len(grants.len());
            for (&name, value) in grants {
                name.to_owned().encode(w);
                value.encode(w);
            }
        }
        resend.encode(w);
        runs.encode(w);
        access.encode(w);
        auto_save.encode(w);
        stopping.encode(w);
        started.encode(w);
    }

    /// Reads what [`Self::checkpoint`] wrote into the commands registered at build. A grant
    /// names a registered permission.
    pub(super) fn restore(&mut self, r: &mut Reader<'_>) -> checkpoint::Result<()> {
        let players = r.len()?;
        let mut grants = Grants::new();
        for _ in 0..players {
            let session = SessionId::decode(r)?;
            let n = r.len()?;
            let mut own = BTreeMap::new();
            for _ in 0..n {
                let name = String::decode(r)?;
                let (&name, _) = self
                    .permissions
                    .get_key_value(name.as_str())
                    .ok_or(checkpoint::Error::Format("granted permission"))?;
                own.insert(name, bool::decode(r)?);
            }
            grants.insert(session, own);
        }
        self.grants = grants;
        self.resend = Codec::decode(r)?;
        self.runs = Codec::decode(r)?;
        self.access = Codec::decode(r)?;
        self.auto_save = Codec::decode(r)?;
        self.stopping = Codec::decode(r)?;
        self.started = Codec::decode(r)?;
        Ok(())
    }
}

/// Checks a command's names and overloads (the rules of [`Command::overload`]).
fn check(command: &Command) -> command::Result<()> {
    for name in std::iter::once(&command.name).chain(&command.aliases) {
        if name.is_empty()
            || name.len() > MAX_NAME
            || name
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '/')
        {
            return Err(Error::InvalidName { name: name.clone() });
        }
    }
    let invalid = |reason| {
        Err(Error::InvalidOverload {
            command: command.name.clone(),
            reason,
        })
    };
    if command.description.len() > COMMAND_DESCRIPTION_MAX_LEN {
        return Err(Error::DescriptionTooLong {
            command: command.name.clone(),
        });
    }
    if command.overloads.is_empty() {
        return invalid("the command has no overload");
    }
    if command.overloads.len() > MAX_OVERLOADS {
        return invalid("more than 64 overloads");
    }
    if command.permissions.len() > MAX_PERMISSIONS {
        return invalid("more than 16 permissions");
    }
    for overload in &command.overloads {
        let params = &overload.params;
        if params.len() > MAX_PARAMS {
            return invalid("more than 32 parameters");
        }
        for (i, param) in params.iter().enumerate() {
            if param.name.is_empty() {
                return invalid("a parameter has no name");
            }
            if params.iter().take(i).any(|p| p.name == param.name) {
                return invalid("two parameters have the same name");
            }
            if !param.optional && params.iter().take(i).any(|p| p.optional) {
                return invalid("a required parameter follows an optional one");
            }
            if param.takes_rest() && i + 1 != params.len() {
                return invalid("a message or raw text parameter is not the last");
            }
            if let Kind::Enum { name, values } = &param.kind
                && (name.is_empty()
                    || values.is_empty()
                    || values.len() > MAX_ENUM_VALUES
                    || values
                        .iter()
                        .any(|v| v.is_empty() || v.chars().any(char::is_whitespace)))
            {
                return invalid("an enum has no type name, no values or a value with spaces");
            }
            if let Kind::Suffixed { suffix } = &param.kind
                && (suffix.is_empty()
                    || suffix.len() > MAX_NAME
                    || !suffix.chars().all(|c| c.is_ascii_alphabetic()))
            {
                return invalid("a suffix is not one to 64 ASCII letters");
            }
        }
    }
    Ok(())
}

impl ServerBuilder {
    /// Registers `command`. The user's commands win over Anchor's own of the same name; two of
    /// the user's commands may not share a name or alias, and [`build`](Self::build) fails with
    /// [`command::Error`] if they do or if a command breaks a rule of [`Command::overload`].
    #[doc(alias = "CommandMap::register")]
    #[doc(alias = "SimpleCommandMap::register")]
    #[must_use]
    pub fn command(mut self, command: Command) -> Self {
        self.setup.commands.register(command, Origin::User);
        self
    }

    /// Drops Anchor's own command named `name` (ASCII case does not matter), such as `"me"`.
    #[doc(alias = "SimpleCommandMap::unregister")]
    #[must_use]
    pub fn without_command(mut self, name: &str) -> Self {
        self.setup
            .commands
            .removed
            .insert(name.to_ascii_lowercase());
        self
    }

    /// Registers `permission`, so that [`Context::permission`] finds it by name. A command's
    /// permission is registered with the command; two permissions of one name with different
    /// defaults make [`build`](Self::build) fail.
    #[doc(alias = "PermissionManager::addPermission")]
    #[must_use]
    pub fn permission(mut self, permission: Permission) -> Self {
        self.setup.commands.add_permission(permission);
        self
    }
}

/// Whether the sender with `session` (`None`: the console) and operator status `op` has
/// `permission`: its grant if it has one, otherwise the permission's default.
fn has_permission_of(
    grants: &Grants,
    session: Option<SessionId>,
    op: bool,
    permission: Permission,
) -> bool {
    session
        .and_then(|s| grants.get(&s))
        .and_then(|g| g.get(permission.name()))
        .copied()
        .unwrap_or_else(|| permission.granted_by_default(op))
}

/// Whether a sender may use a command needing one of `permissions` (none: everyone), `has`
/// telling whether it has one.
fn allowed(permissions: &[Permission], has: impl Fn(Permission) -> bool) -> bool {
    permissions.is_empty() || permissions.iter().any(|&p| has(p))
}

/// Whether `sender` has `permission`; a player who has left has none.
fn has_permission(state: &State, sender: Sender, permission: Permission) -> bool {
    match sender {
        Sender::Console => permission.granted_by_default(true),
        Sender::Player(id) => state.players.get(&id.session()).is_some_and(|p| {
            let op = state.commands.access.is_op(&p.identity.name);
            has_permission_of(&state.commands.grants, Some(id.session()), op, permission)
        }),
    }
}

/// Runs `line` (without its `/`) as `sender`: finds the command, checks the permission, fits the
/// words to the first overload they fit, resolves players and positions, and runs the handler.
fn execute(state: &mut State, sender: Sender, line: &str) -> command::Result<Output> {
    let words = parse::split(line)?;
    let Some((label, rest)) = words.split_first() else {
        return Err(Error::Unknown {
            name: String::new(),
        });
    };
    let label = label.text.to_ascii_lowercase();
    let Some(&index) = state.commands.by_name.get(&label) else {
        return Err(Error::Unknown { name: label });
    };
    let Some(command) = state.commands.list.get(index) else {
        return Err(Error::Unknown { name: label });
    };
    let name = command.name.clone();
    if !allowed(&command.permissions, |p| has_permission(state, sender, p)) {
        return Err(Error::Permission { name });
    }
    let commands = &state.commands;
    // The first overload that fits, else where the overload that fits furthest stops.
    let mut furthest = 0;
    let mut fitted = None;
    for (i, o) in command.overloads.iter().enumerate() {
        match parse::fit(&o.params, line, rest, &commands.names) {
            Ok(raw) => {
                fitted = Some((i, raw));
                break;
            }
            Err(at) => furthest = furthest.max(at),
        }
    }
    let Some((overload, raw)) = fitted else {
        return Err(parse::syntax_error(line, furthest));
    };
    let names: Vec<String> = command
        .overloads
        .get(overload)
        .map(|o| o.params.iter().map(|p| p.name.clone()).collect())
        .unwrap_or_default();
    state.commands.runs = state.commands.runs.wrapping_add(1);
    let args = resolve(state, sender, names, raw).map_err(|e| match e {
        Resolve::Error(e) => e,
        Resolve::Unfit => Error::Usage {
            usage: state
                .commands
                .list
                .get(index)
                .map(Command::usage_text)
                .unwrap_or_default(),
        },
    })?;
    let Some(mut handler) = handler_slot(state, index, overload).and_then(Option::take) else {
        return Err(Error::failed(format!(
            "the command {name} is already running"
        )));
    };
    let result = handler(&mut Context { state }, &sender, &args);
    if let Some(slot) = handler_slot(state, index, overload) {
        *slot = Some(handler);
    }
    result
}

/// Where the handler of overload `overload` of command `index` waits between runs.
fn handler_slot(
    state: &mut State,
    index: usize,
    overload: usize,
) -> Option<&mut Option<command::Handler>> {
    state
        .commands
        .list
        .get_mut(index)
        .and_then(|c| c.overloads.get_mut(overload))
        .map(|o| &mut o.handler)
}

/// Why fitted arguments did not resolve.
enum Resolve {
    /// A message for the sender.
    Error(Error),
    /// A position left the finite numbers: the line does not fit after all.
    Unfit,
}

/// Turns fitted arguments into [`Args`]: names and selectors into online players (not those
/// still loading), relative coordinates into absolute ones.
fn resolve(
    state: &mut State,
    sender: Sender,
    names: Vec<String>,
    raw: Vec<Raw>,
) -> Result<Args, Resolve> {
    let origin = origin(state, sender);
    let mut values = Vec::with_capacity(raw.len());
    for (name, raw) in names.into_iter().zip(raw) {
        let value = match raw {
            Raw::Value(v) => v,
            Raw::Target(target) => Value::Targets(targets(state, sender, origin, target)?),
            Raw::Position(coords) => {
                let base = origin.map_or(Vec3::ZERO, |(_, feet)| feet);
                let [x, y, z] = [
                    (coords[0], base.x),
                    (coords[1], base.y),
                    (coords[2], base.z),
                ]
                .map(|(c, b): (Coord, f64)| if c.relative { b + c.value } else { c.value });
                let pos = Vec3::new(x, y, z);
                if !pos.is_finite() {
                    return Err(Resolve::Unfit);
                }
                Value::Position(pos)
            }
        };
        values.push((name, value));
    }
    Ok(Args { values })
}

/// Where `sender` stands: its world and feet, after a teleport earlier in the tick
/// ([`Player::position`](crate::player::Player::position)). The console stands on the default
/// world's spawn block.
fn origin(state: &State, sender: Sender) -> Option<(usize, Vec3)> {
    match sender {
        Sender::Player(id) => state
            .players
            .get(&id.session())
            .map(|p| (p.world().index(), p.position())),
        Sender::Console => {
            let default = state.catalog.default_world();
            state.worlds.get(default).map(|w| {
                let spawn = w.spawn();
                (
                    default,
                    Vec3::new(f64::from(spawn.x), f64::from(spawn.y), f64::from(spawn.z)),
                )
            })
        }
    }
}

/// The players a target names, at least one.
fn targets(
    state: &mut State,
    sender: Sender,
    origin: Option<(usize, Vec3)>,
    target: Target,
) -> Result<Vec<player::Id>, Resolve> {
    let playing = |p: &super::Player| p.phase != Phase::Loading;
    let found: Vec<player::Id> = match target {
        Target::Name(name) => {
            let id = Context { state: &mut *state }.player_exact(&name);
            match id.filter(|id| state.players.get(&id.session()).is_some_and(playing)) {
                Some(id) => vec![id],
                None => return Err(Resolve::Error(Error::PlayerNotFound { name })),
            }
        }
        Target::Sender => match sender {
            Sender::Player(id) if state.players.get(&id.session()).is_some_and(playing) => {
                vec![id]
            }
            Sender::Player(_) | Sender::Console => Vec::new(),
        },
        Target::All => state
            .players
            .iter()
            .filter(|(_, p)| playing(p))
            .map(|(&s, _)| player::Id::new(s))
            .collect(),
        Target::Nearest => {
            let Some((world, at)) = origin else {
                return Err(Resolve::Error(Error::NoTargets));
            };
            // Squared distances are exact sums of products: the same on every machine; ties go
            // to the lowest session.
            let mut best: Option<(f64, SessionId)> = None;
            for (&session, p) in &state.players {
                if !playing(p) || p.world().index() != world {
                    continue;
                }
                let distance = p.position().distance_squared(at);
                if best.is_none_or(|(b, _)| distance < b) {
                    best = Some((distance, session));
                }
            }
            best.map(|(_, s)| vec![player::Id::new(s)])
                .unwrap_or_default()
        }
        Target::Random => {
            let candidates: Vec<SessionId> = state
                .players
                .iter()
                .filter(|(_, p)| playing(p))
                .map(|(&s, _)| s)
                .collect();
            let seed = state.worlds.first().map_or(0, crate::world::World::seed);
            let key = random::key(RANDOM_TARGET, &[state.commands.runs]);
            let mut rng = Random::new(seed, state.tick, key);
            let n = u32::try_from(candidates.len()).unwrap_or(u32::MAX);
            let pick = usize::try_from(rng.below(n)).unwrap_or(usize::MAX);
            candidates
                .get(pick)
                .map(|&s| vec![player::Id::new(s)])
                .unwrap_or_default()
        }
    };
    if found.is_empty() {
        return Err(Resolve::Error(Error::NoTargets));
    }
    Ok(found)
}

/// Handles a `CommandRequest` of `session`: a `/` line from a joined player. Each spends one line
/// of the player's chat limit ([`chat::spend_line`]: PMMP runs a command request as a chat
/// line), so chat lines and commands together pass 2 per tick. The player gets a
/// `CommandOutput` that answers the request's origin.
pub(super) fn on_request(state: &mut State, session: SessionId, packet: &CommandRequest) {
    let Some(player) = state.players.get(&session) else {
        return;
    };
    if player.phase == Phase::Loading {
        return;
    }
    if !chat::spend_line(state, session) {
        debug!(
            session = session.get(),
            "too many chat lines and commands in one tick; ignored"
        );
        return;
    }
    let Some(line) = packet.command_line.strip_prefix('/') else {
        debug!(session = session.get(), "command without '/' ignored");
        return;
    };
    let Some(result) = typed(state, Sender::Player(player::Id::new(session)), line) else {
        debug!(session = session.get(), "player command cancelled");
        return;
    };
    debug!(
        session = session.get(),
        ok = result.is_ok(),
        "player command"
    );
    let output = output_packet(&packet.command_origin, &result);
    if let Some(p) = state.players.get_mut(&session) {
        p.send_packet(output);
    }
}

/// The `CommandOutput` answering `origin` with `result`: the lines of a success ([`shown`], one
/// success counted), or the error in red (none counted).
fn output_packet(origin: &CommandOrigin, result: &command::Result<Output>) -> Packet {
    let (success, lines) = shown(result);
    let output_messages = lines
        .into_iter()
        .map(|Line { text, params }| CommandOutputMessage {
            success,
            message: text,
            parameters: params,
        })
        .collect();
    Packet::CommandOutput(CommandOutput {
        command_origin: origin.clone(),
        output_type: u8::from(CommandOutputType::AllOutput),
        success_count: u32::from(success),
        output_messages,
        data_set: None,
    })
}

/// What a sender is shown of `result`: whether it succeeded, and the lines of the success or the
/// error's line, within bounds a client takes and a log keeps readable. At most
/// [`MAX_OUTPUT_LINES`] lines, the last telling how many were left out when there were more;
/// at most [`MAX_OUTPUT_PARAMS`] parameters a line; and at most [`MAX_TEXT`] bytes of text and
/// parameters in all (one client text), a text or parameter that passes it cut at a character
/// boundary and the lines after it left out.
pub(super) fn shown(result: &command::Result<Output>) -> (bool, Vec<Line>) {
    let error;
    let (success, lines) = match result {
        Ok(output) => (true, output.lines.as_slice()),
        Err(e) => {
            error = [e.line()];
            (false, error.as_slice())
        }
    };
    let mut budget = MAX_TEXT;
    let room = if lines.len() > MAX_OUTPUT_LINES {
        MAX_OUTPUT_LINES - 1
    } else {
        MAX_OUTPUT_LINES
    };
    let mut out = Vec::with_capacity(lines.len().min(MAX_OUTPUT_LINES));
    for line in lines.iter().take(room) {
        let text = cut(&line.text, &mut budget);
        let params = line
            .params
            .iter()
            .take(MAX_OUTPUT_PARAMS)
            .map(|p| cut(p, &mut budget))
            .collect();
        out.push(Line { text, params });
        if budget == 0 {
            break;
        }
    }
    let left_out = lines.len() - out.len();
    if left_out > 0 {
        out.push(Line {
            text: format!("... {left_out} more lines left out"),
            params: Vec::new(),
        });
    }
    (success, out)
}

/// `text` cut to at most `budget` bytes at a character boundary; the bytes kept are taken from
/// `budget`.
fn cut(text: &str, budget: &mut usize) -> String {
    let mut end = text.len().min(*budget);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    *budget -= end;
    text.get(..end).unwrap_or_default().to_owned()
}

/// Runs a command line a player or the console typed (without its `/`) after asking the
/// [`crate::event::Command`] listeners (PMMP `CommandEvent`), which may cancel it or change the
/// line; `None` if a listener cancelled it or, after a listener ran, the player who typed it is
/// leaving. The monitors see the line that ran, after it ran. A command run through
/// [`Context::dispatch_command`] raises nothing (no re-entry).
pub(super) fn typed(
    state: &mut State,
    sender: Sender,
    line: &str,
) -> Option<command::Result<Output>> {
    let staying = |state: &State| match sender {
        Sender::Console => true,
        Sender::Player(id) => state
            .players
            .get(&id.session())
            .is_some_and(super::Player::is_online),
    };
    let mut event = None;
    if dispatch::heard::<crate::event::Command>(state) {
        let e = event.insert(crate::event::Command {
            sender,
            line: line.to_owned(),
            cancelled: false,
        });
        dispatch::call(state, e);
        if e.cancelled || (dispatch::listened::<crate::event::Command>(state) && !staying(state)) {
            return None;
        }
    }
    let line = event.as_ref().map_or(line, |e| e.line.as_str());
    let result = execute(state, sender, line);
    if let Some(e) = &event {
        dispatch::watch(state, e);
    }
    Some(result)
}

/// Runs a console line as the console sender; its output goes to the log.
pub(super) fn console(state: &mut State, line: &str) {
    let line = line.trim();
    let line = line.strip_prefix('/').unwrap_or(line);
    if line.is_empty() {
        return;
    }
    let Some(result) = typed(state, Sender::Console, line) else {
        info!("console command cancelled");
        return;
    };
    let (success, lines) = shown(&result);
    for l in &lines {
        // The client's keys become Anchor's sentences; players get the keys.
        let line = text::render(&l.text, &l.params);
        if success {
            info!(output = %line, "console command");
        } else {
            info!(error = %line, "console command failed");
        }
    }
}

/// The end of the tick's serial phases: players whose permissions changed get
/// `AvailableCommands` again (only after their spawn window, which carries the first one).
pub(super) fn sync(state: &mut State) {
    state.commands.started.get_or_insert(state.tick);
    for session in std::mem::take(&mut state.commands.resend) {
        let Some(player) = state.players.get(&session) else {
            continue;
        };
        if player.chunk_radius.is_none() {
            continue;
        }
        let packet = available_commands(state.commands.listing(), session, player);
        if let Some(p) = state.players.get_mut(&session) {
            p.send_packet(packet);
        }
    }
}

/// Forgets the grants and the pending command list of the player of `session`, which is leaving:
/// the tick calls it where it removes a player, so grants live no longer than their player.
pub(super) fn left(state: &mut State, session: SessionId) {
    state.commands.grants.remove(&session);
    state.commands.resend.remove(&session);
}

/// Why a join is refused (PMMP `PlayerPreLoginEvent` kick flags, in their priority order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// `max-players` players are online.
    Full,
    /// The whitelist is on and the player is neither listed nor an operator.
    NotWhitelisted,
    /// The name is banned, with the ban's reason.
    Banned(String),
    /// The address is banned, with the ban's reason.
    IpBanned(String),
}

impl Refusal {
    /// What the refused player sees: the client's text where it has one, Anchor's otherwise.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Full => "disconnectionScreen.serverFull".to_owned(),
            Self::NotWhitelisted => "disconnectionScreen.notAllowed".to_owned(),
            Self::Banned(reason) if reason.is_empty() => "This server has banned you".to_owned(),
            Self::IpBanned(reason) if reason.is_empty() => {
                "This server has banned your address".to_owned()
            }
            Self::Banned(reason) | Self::IpBanned(reason) => {
                format!("This server has banned you: {reason}")
            }
        }
    }

    /// The last packet the refused session gets: `PlayStatus(LoginFailedServerFull)` for a full
    /// server (the client shows its own text), otherwise a `Disconnect` with the message.
    pub(crate) fn packet(&self) -> Packet {
        match self {
            Self::Full => Packet::PlayStatus(PlayStatus {
                status: PLAY_STATUS_SERVER_FULL,
            }),
            _ => Packet::Disconnect(Disconnect {
                reason: i32::from(DisconnectReason::NotAllowed),
                hide_disconnection_screen: false,
                message: self.message(),
                filtered_message: String::new(),
            }),
        }
    }
}

/// Whether `identity` may join now (PMMP `LoginPacketHandler`'s checks, first refusal in PMMP's
/// priority order): the server is not full, the whitelist lets it in, and neither its name nor
/// its address is banned. A replaced duplicate login frees its slot before this check.
pub(crate) fn admit(state: &State, identity: &Identity) -> Result<(), Refusal> {
    refusals(state, identity)
        .into_iter()
        .flatten()
        .next()
        .map_or(Ok(()), Err)
}

/// Every refusal of `identity` now, one per PMMP kick flag in priority order: the server is
/// full, the whitelist keeps it out, its name is banned (else its address). `PlayerLogin`
/// carries them all, so that a listener clearing one does not let in a player another refuses.
pub(crate) fn refusals(state: &State, identity: &Identity) -> [Option<Refusal>; 3] {
    let max_players = usize::try_from(state.settings.max_players).unwrap_or(usize::MAX);
    let full = (state.players.len() >= max_players).then_some(Refusal::Full);
    let access = &state.commands.access;
    let whitelisted = (access.whitelist_enabled()
        && !access.is_op(&identity.name)
        && !access.is_listed(&identity.name))
    .then_some(Refusal::NotWhitelisted);
    let banned = access
        .ban_entry(&identity.name, state.tick)
        .map(|ban| Refusal::Banned(ban.reason().to_owned()))
        .or_else(|| {
            identity
                .address
                .and_then(|ip| access.ip_ban_entry(ip, state.tick))
                .map(|ban| Refusal::IpBanned(ban.reason().to_owned()))
        });
    [full, whitelisted, banned]
}

impl Context<'_> {
    /// Runs `line` (without `/`) as `sender` would type it, checking `sender`'s permission, and
    /// returns what the sender would see; nothing is sent. A command cannot run itself again
    /// while it runs: that fails.
    #[doc(alias = "Server::dispatchCommand")]
    #[doc(alias = "CommandMap::dispatch")]
    #[doc(alias = "SimpleCommandMap::dispatch")]
    pub fn dispatch_command(&mut self, sender: Sender, line: &str) -> command::Result<Output> {
        execute(self.state, sender, line)
    }

    /// The name of `sender`: the player's name, or `Console`; `None` for a player who has left.
    #[doc(alias = "CommandSender::getName")]
    #[doc(alias = "ConsoleCommandSender::getName")]
    pub fn sender_name(&self, sender: Sender) -> Option<&str> {
        match sender {
            Sender::Player(id) => self.player(id).map(player::Player::name),
            Sender::Console => Some(CONSOLE_NAME),
        }
    }

    /// Shows `sender` `message`: a player as [`Player::send`](player::Player::send) shows it, the
    /// console in the log. Fails as `Player::send` does; for a player who has left it sends
    /// nothing.
    #[doc(alias = "CommandSender::sendMessage")]
    #[doc(alias = "ConsoleCommandSender::sendMessage")]
    pub fn send_to(&mut self, sender: Sender, message: player::Message) -> player::Result<()> {
        match sender {
            Sender::Player(id) => self.player_mut(id).map_or(Ok(()), |p| p.send(message)),
            Sender::Console => {
                let message = text::message(&message);
                info!(%message, "console message");
                Ok(())
            }
        }
    }

    /// Shows what `sender`'s command did, `message`, to every other online player with
    /// [`BROADCAST_ADMIN`](permission::BROADCAST_ADMIN), as the client's `[sender: message]`
    /// (`chat.type.admin`) in grey italics, and logs it when a player sent the command (the
    /// console logs its own results). The sender itself sees what its handler returns.
    #[doc(alias = "Command::broadcastCommandMessage")]
    pub fn broadcast_command_message(&mut self, sender: Sender, message: &str) {
        let source = self.sender_name(sender).unwrap_or_default().to_owned();
        let copy = player::Message::Translation {
            key: format!("{ADMIN_PREFIX}%{ADMIN_KEY}"),
            parameters: vec![source.clone(), message.to_owned()],
        };
        self.tell_admins(sender, &copy);
        if let Sender::Player(_) = sender {
            info!(%source, message, "command result");
        }
    }

    /// Shows `copy` to every online player with [`BROADCAST_ADMIN`](permission::BROADCAST_ADMIN)
    /// but `sender`. A copy of a command line's result fits what a client decodes, so a failure
    /// only means the player is leaving.
    fn tell_admins(&mut self, sender: Sender, copy: &player::Message) {
        let admins: Vec<player::Id> = self
            .players()
            .map(player::Player::id)
            .filter(|&id| {
                Sender::Player(id) != sender
                    && self.has_permission(Sender::Player(id), permission::BROADCAST_ADMIN)
            })
            .collect();
        for id in admins {
            if let Some(p) = self.player_mut(id)
                && let Err(e) = p.send(copy.clone())
            {
                debug!(%e, "command result copy not sent");
            }
        }
    }

    /// Whether `sender` may use `command`: it has one of the command's permissions, or the
    /// command needs none.
    #[doc(alias = "Command::testPermissionSilent")]
    pub fn can_use(&self, sender: Sender, command: &Command) -> bool {
        allowed(&command.permissions, |p| {
            has_permission(self.state, sender, p)
        })
    }

    /// The command named or aliased `name` (ASCII case does not matter).
    #[doc(alias = "CommandMap::getCommand")]
    #[doc(alias = "SimpleCommandMap::getCommand")]
    pub fn command(&self, name: &str) -> Option<&Command> {
        let index = *self
            .state
            .commands
            .by_name
            .get(&name.to_ascii_lowercase())?;
        self.state.commands.list.get(index)
    }

    /// Every command, the user's first, in registration order.
    #[doc(alias = "SimpleCommandMap::getCommands")]
    pub fn commands(&self) -> impl Iterator<Item = &Command> {
        self.state.commands.list.iter()
    }

    /// The registered permission named `name`.
    #[doc(alias = "PermissionManager::getPermission")]
    pub fn permission(&self, name: &str) -> Option<Permission> {
        self.state.commands.permissions.get(name).copied()
    }

    /// Every registered permission, in name order.
    #[doc(alias = "PermissionManager::getPermissions")]
    pub fn permissions(&self) -> impl Iterator<Item = Permission> {
        self.state.commands.permissions.values().copied()
    }

    /// Whether `sender` has `permission`: a player's grant or denial if it has one, otherwise the
    /// permission's default for its operator status. The console counts as an operator. A player
    /// who has left has no permission.
    #[doc(alias = "Permissible::hasPermission")]
    #[doc(alias = "Player::hasPermission")]
    pub fn has_permission(&self, sender: Sender, permission: Permission) -> bool {
        has_permission(self.state, sender, permission)
    }

    /// Whether `player` has a grant or denial of `permission`.
    #[doc(alias = "Permissible::isPermissionSet")]
    #[doc(alias = "Player::isPermissionSet")]
    pub fn is_permission_set(&self, player: player::Id, permission: Permission) -> bool {
        self.state
            .commands
            .grants
            .get(&player.session())
            .is_some_and(|g| g.contains_key(permission.name()))
    }

    /// Grants (`value` true) or denies `permission` to `player` until it leaves, whatever its
    /// default says; its command list follows at the end of the tick. Not saved. False if the
    /// player has left or has 1,024 grants.
    #[doc(alias = "PermissionAttachment::setPermission")]
    pub fn set_permission(
        &mut self,
        player: player::Id,
        permission: Permission,
        value: bool,
    ) -> bool {
        let session = player.session();
        if !self.state.players.contains_key(&session) {
            return false;
        }
        let commands = &mut self.state.commands;
        let grants = commands.grants.entry(session).or_default();
        if !grants.contains_key(permission.name()) && grants.len() >= MAX_GRANTS {
            return false;
        }
        if grants.insert(permission.name(), value) != Some(value) {
            commands.resend.insert(session);
        }
        true
    }

    /// Removes `player`'s grant or denial of `permission`, so its default applies again; false
    /// if it had none.
    #[doc(alias = "PermissionAttachment::unsetPermission")]
    pub fn unset_permission(&mut self, player: player::Id, permission: Permission) -> bool {
        let session = player.session();
        let commands = &mut self.state.commands;
        let Some(grants) = commands.grants.get_mut(&session) else {
            return false;
        };
        if grants.remove(permission.name()).is_none() {
            return false;
        }
        if grants.is_empty() {
            commands.grants.remove(&session);
        }
        commands.resend.insert(session);
        true
    }

    /// Removes every grant and denial of `player`, so the defaults apply again; false if it had
    /// none.
    #[doc(alias = "PermissionAttachment::clearPermissions")]
    pub fn clear_permissions(&mut self, player: player::Id) -> bool {
        let session = player.session();
        let commands = &mut self.state.commands;
        if commands.grants.remove(&session).is_none() {
            return false;
        }
        commands.resend.insert(session);
        true
    }

    /// The grants (`true`) and denials (`false`) of `player`, by permission name in name order.
    #[doc(alias = "PermissionAttachment::getPermissions")]
    pub fn permission_grants(
        &self,
        player: player::Id,
    ) -> impl Iterator<Item = (&'static str, bool)> {
        self.state
            .commands
            .grants
            .get(&player.session())
            .into_iter()
            .flat_map(|g| g.iter().map(|(&name, &value)| (name, value)))
    }

    /// Every registered permission in name order, with whether `sender` has it now: its grant or
    /// denial, otherwise the default for its operator status (PMMP lists the permissions set on
    /// the sender with their values; Anchor has no child permissions to expand). A player who
    /// has left has none.
    #[doc(alias = "Permissible::getEffectivePermissions")]
    #[doc(alias = "Player::getEffectivePermissions")]
    pub fn effective_permissions(
        &self,
        sender: Sender,
    ) -> impl Iterator<Item = (Permission, bool)> {
        let commands = &self.state.commands;
        // The session and operator status once, not per permission; `None` for a player gone.
        let holder = match sender {
            Sender::Console => Some((None, true)),
            Sender::Player(id) => self
                .state
                .players
                .get(&id.session())
                .map(|p| (Some(id.session()), commands.access.is_op(&p.identity.name))),
        };
        commands.permissions.values().map(move |&p| {
            let has = holder
                .is_some_and(|(session, op)| has_permission_of(&commands.grants, session, op, p));
            (p, has)
        })
    }

    /// The joined player named `name` (ASCII case), loading or not: its command list and
    /// abilities follow a change of operator status to `op`. Without Xbox sign-in the name is
    /// the player's key and the lookup is an index search; with it, the players' keys are their
    /// XUIDs, and the players (at most `max-players`) are compared one by one, only when the
    /// operator list changes.
    fn operator_changed(&mut self, name: &str, op: bool) {
        let state = &mut *self.state;
        let key = PlayerKey::Name(name.to_ascii_lowercase());
        let session = state.index.get(&key).copied().or_else(|| {
            state
                .players
                .iter()
                .find(|(_, p)| {
                    p.identity.xuid.is_some() && p.identity.name.eq_ignore_ascii_case(name)
                })
                .map(|(&session, _)| session)
        });
        if let Some(session) = session
            && let Some(p) = state.players.get_mut(&session)
        {
            state.commands.resend.insert(session);
            p.survival.set_operator(op);
        }
    }

    /// Makes `name` (any ASCII case) an operator, saved at the next tick boundary; an online
    /// player of that name gets its new command list and operator abilities at the end of the
    /// tick.
    #[doc(alias = "Server::addOp")]
    pub fn op(&mut self, name: &str) -> permission::Result<()> {
        if self.state.commands.access.op(name)? {
            self.state.saving.requested = true;
            self.operator_changed(name, true);
        }
        Ok(())
    }

    /// Takes operator status from `name`; false if it was not an operator. An online player of
    /// that name gets its new command list and a member's abilities at the end of the tick.
    #[doc(alias = "Server::removeOp")]
    pub fn deop(&mut self, name: &str) -> bool {
        if !self.state.commands.access.deop(name) {
            return false;
        }
        self.state.saving.requested = true;
        self.operator_changed(name, false);
        true
    }

    /// Whether `name` (any ASCII case) is an operator.
    #[doc(alias = "Server::isOp")]
    pub fn is_op(&self, name: &str) -> bool {
        self.state.commands.access.is_op(name)
    }

    /// The operators' names in ASCII lowercase, in name order.
    #[doc(alias = "Server::getOps")]
    pub fn ops(&self) -> impl Iterator<Item = &str> {
        self.state.commands.access.ops()
    }

    /// Adds `entry` to the name bans, or to the IP bans for [`BanEntry::ip`], replacing a ban of
    /// the same name or address, saved at the next tick boundary. Its creation time is now. Bans
    /// that have ended are dropped from the list first. A banned player who is online stays
    /// until kicked; the ban refuses its next join.
    #[doc(alias = "BanList::addBan")]
    #[doc(alias = "BanList::add")]
    pub fn ban(&mut self, entry: BanEntry) -> permission::Result<()> {
        let tick = self.state.tick;
        self.state.commands.access.ban(entry, tick)?;
        self.state.saving.requested = true;
        Ok(())
    }

    /// Lifts the ban of `name` (any ASCII case); false if there was none.
    #[doc(alias = "BanList::remove")]
    pub fn pardon(&mut self, name: &str) -> bool {
        let lifted = self.state.commands.access.pardon(name);
        self.state.saving.requested |= lifted;
        lifted
    }

    /// Lifts the ban of `ip`; false if there was none.
    pub fn pardon_ip(&mut self, ip: IpAddr) -> bool {
        let lifted = self.state.commands.access.pardon_ip(ip);
        self.state.saving.requested |= lifted;
        lifted
    }

    /// Whether `name` (any ASCII case) is banned now.
    #[doc(alias = "BanList::isBanned")]
    pub fn is_banned(&self, name: &str) -> bool {
        self.ban_entry(name).is_some()
    }

    /// Whether `ip` is banned now.
    pub fn is_ip_banned(&self, ip: IpAddr) -> bool {
        self.ip_ban_entry(ip).is_some()
    }

    /// The ban of `name` (any ASCII case) in force now.
    #[doc(alias = "BanList::getEntry")]
    pub fn ban_entry(&self, name: &str) -> Option<&BanEntry> {
        self.state.commands.access.ban_entry(name, self.state.tick)
    }

    /// The ban of `ip` in force now.
    pub fn ip_ban_entry(&self, ip: IpAddr) -> Option<&BanEntry> {
        self.state.commands.access.ip_ban_entry(ip, self.state.tick)
    }

    /// The name bans in force now, in name order.
    #[doc(alias = "BanList::getEntries")]
    #[doc(alias = "Server::getNameBans")]
    pub fn bans(&self) -> impl Iterator<Item = &BanEntry> {
        self.state.commands.access.bans(self.state.tick)
    }

    /// The IP bans in force now, in address order.
    #[doc(alias = "Server::getIPBans")]
    pub fn ip_bans(&self) -> impl Iterator<Item = &BanEntry> {
        self.state.commands.access.ip_bans(self.state.tick)
    }

    /// Puts `name` (any ASCII case) on the whitelist, saved at the next tick boundary.
    #[doc(alias = "Server::addWhitelist")]
    pub fn add_to_whitelist(&mut self, name: &str) -> permission::Result<()> {
        if self.state.commands.access.whitelist_add(name)? {
            self.state.saving.requested = true;
        }
        Ok(())
    }

    /// Takes `name` off the whitelist; false if it was not on it.
    #[doc(alias = "Server::removeWhitelist")]
    pub fn remove_from_whitelist(&mut self, name: &str) -> bool {
        let removed = self.state.commands.access.whitelist_remove(name);
        self.state.saving.requested |= removed;
        removed
    }

    /// Whether `name` may join as far as the whitelist goes (PMMP): the whitelist is off, or
    /// `name` is on it or an operator.
    #[doc(alias = "Server::isWhitelisted")]
    pub fn is_whitelisted(&self, name: &str) -> bool {
        let access = &self.state.commands.access;
        !access.whitelist_enabled() || access.is_op(name) || access.is_listed(name)
    }

    /// The whitelisted names in ASCII lowercase, in name order.
    #[doc(alias = "Server::getWhitelisted")]
    pub fn whitelist(&self) -> impl Iterator<Item = &str> {
        self.state.commands.access.whitelisted()
    }

    /// Whether the whitelist is on (`white-list`).
    #[doc(alias = "Server::hasWhitelist")]
    pub fn whitelist_enabled(&self) -> bool {
        self.state.commands.access.whitelist_enabled()
    }

    /// Turns the whitelist on or off until the server stops (`white-list` decides at start:
    /// Anchor never writes the settings file). Players already online stay.
    pub fn set_whitelist_enabled(&mut self, enabled: bool) {
        if self.state.commands.access.whitelist_enabled() != enabled {
            info!(enabled, "whitelist switched until the server stops");
        }
        self.state.commands.access.set_whitelist_enabled(enabled);
    }
}

/// Server control the built-in commands use: stopping, periodic saves and the settings that may
/// change while the server runs (until it stops, never written to the file).
impl Context<'_> {
    /// Stops the server after this tick, as a [`ShutdownHandle`](crate::ShutdownHandle) does:
    /// every player sees `shutdown-message`, the last state is saved, then
    /// [`Server::run`](crate::Server::run) returns. Calling it again changes nothing.
    #[doc(alias = "Server::shutdown")]
    pub fn shutdown(&mut self) {
        if !self.state.commands.stopping {
            info!("stop asked for by a command or callback");
        }
        self.state.commands.stopping = true;
    }

    /// Turns the periodic save (every `save-interval-minutes`) off or on until the server
    /// stops. While it is off, the state is saved only when asked ([`save`](Self::save)), when
    /// memory waiting for a save passes its bound, and at shutdown; a crash then replays from the
    /// last save, so the input log holds more ticks (up to its 64 MiB bound).
    #[doc(alias = "WorldManager::setAutoSave")]
    pub fn set_auto_save(&mut self, enabled: bool) {
        if self.state.commands.auto_save != enabled {
            info!(enabled, "periodic save switched until the server stops");
        }
        self.state.commands.auto_save = enabled;
    }

    /// Whether the periodic save is on.
    #[doc(alias = "WorldManager::getAutoSave")]
    pub fn auto_save_enabled(&self) -> bool {
        self.state.commands.auto_save
    }

    /// Sets the difficulty until the server stops and tells the players. Every loaded world
    /// follows it from now on, as PMMP's `/difficulty` sets each world's: a difficulty of its own
    /// ([`set_world_difficulty`](Self::set_world_difficulty)) is dropped. On a `hardcore` server
    /// only [`Difficulty::Hard`] is allowed: false, changing nothing, for another.
    pub fn set_difficulty(&mut self, difficulty: Difficulty) -> bool {
        if self.state.settings.hardcore && difficulty != Difficulty::Hard {
            return false;
        }
        self.state.settings.difficulty = difficulty;
        for (i, world) in self.state.worlds.iter_mut().enumerate() {
            if self.state.catalog.is_loaded(i) {
                world.set_difficulty(None);
            }
        }
        let packet = Packet::SetDifficulty(SetDifficulty {
            difficulty: spawn::difficulty_id(difficulty).cast_unsigned(),
        });
        // Every player past its start-game packets, as `world_api::send_to_world` tells one
        // world's.
        for p in self.state.players.values_mut() {
            if p.phase != Phase::Loading {
                p.send_packet(packet.clone());
            }
        }
        true
    }

    /// Sets the game mode new players get (`gamemode`) until the server stops; with
    /// `force-gamemode` every join gets it.
    pub fn set_default_game_mode(&mut self, mode: GameMode) {
        self.state.settings.game_mode = mode;
    }
}
