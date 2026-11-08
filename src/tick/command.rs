//! Commands in the tick: the registered commands and permissions, `CommandRequest`, the console
//! lines and `AvailableCommands`, and the operator list, bans and whitelist with the join policy
//! that reads them.
//! It also holds `Context::set_default_game_mode` and `set_difficulty`, the API of the
//! `defaultgamemode` and `difficulty` commands Anchor removes.
//!
//! A command runs in the tick's serial step, where its input arrived: a player's
//! `CommandRequest` in the packet inputs, a console line as its own input
//! ([`InputKind::Console`](super::queue::InputKind::Console)), both recorded in the input log, so
//! a replay runs the same commands at the same points.

mod access;
mod available;
mod builtin;

pub(crate) use access::Lists;
pub(crate) use available::{Listing, available_commands};

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::Arc;

use tracing::{debug, info};

use super::checkpoint::{self, Codec, Reader, Writer};
use super::queue::{self, Identity, SessionId, SharedPacket};
use super::{Context, Phase, PlayerKey, State, chat, dispatch, spawn};
use crate::ServerBuilder;
use crate::command::parse::{self, Coord, Names, Raw, Target};
use crate::command::{
    self, Args, Command, Error, Kind, Line, MAX_ENUM_VALUES, MAX_NAME, MAX_OVERLOADS, MAX_PARAMS,
    MAX_PERMISSIONS, Output, Sender, Value, register,
};
use crate::config::{Difficulty, GameMode};
use crate::console::text;
use crate::math::Vec3;
use crate::permission::{self, BanEntry, Permission};
use crate::player::{self, MAX_TEXT_BYTES};
use crate::protocol::consts::{CommandOutputType, DisconnectReason, SoftEnumAction};
use crate::protocol::packet::{
    CommandOutput, CommandRequest, Disconnect, Packet, SetDifficulty, UpdateSoftEnum,
};
use crate::protocol::{COMMAND_DESCRIPTION_MAX_LEN, CommandOrigin, CommandOutputMessage};
use crate::random::{self, Random};
use crate::save::{AccessData, AccessWrite};
use crate::storage::AccessKey;
use access::Access;

/// The client's `[sender: message]` text of a copy of a command result for operators.
const ADMIN_KEY: &str = "chat.type.admin";
/// The prefix of a copy of a command result for operators: grey italics, as the client shows
/// its own `chat.type.admin` lines.
const ADMIN_PREFIX: &str = "§7§o";
/// What the console is called in messages.
const CONSOLE_NAME: &str = "Console";
/// Permissions one player may be granted or denied while online.
const MAX_GRANTS: usize = 1024;
/// The most lines of one command result a sender gets; the lines past it are replaced by one
/// telling how many were left out (a client lists at most 1,024 in a `CommandOutput`).
const MAX_OUTPUT_LINES: usize = 64;
/// The most parameters of one line of a command result a sender gets.
const MAX_OUTPUT_PARAMS: usize = 32;
/// The random stream of `@r`.
const RANDOM_TARGET: u64 = random::tag("command-random-target");

/// The grants and denials of online players by session.
type Grants = BTreeMap<SessionId, BTreeMap<&'static str, bool>>;

/// Where a registered command comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// The user's: it must not clash with another of the user's.
    User,
    /// Anchor's own: skipped when the user has a command of the name or dropped it, and it loses
    /// the aliases the user's commands have.
    Builtin,
}

/// The players a copy of a command result goes to: those with
/// [`BROADCAST_ADMIN`](permission::BROADCAST_ADMIN), kept while a command runs, so that a command
/// that acts on every player looks them up once instead of once a player.
#[derive(Debug)]
enum Admins {
    /// No command runs: each copy looks them up.
    Idle,
    /// A command runs and has not looked them up, or a permission changed since it did.
    Unknown,
    /// A command runs, and these are they, in [`Id`](player::Id) order.
    Known(Vec<player::Id>),
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
    /// The defaults set at build ([`ServerBuilder::permission_default`]), which `finish` puts in
    /// place of the permissions' own.
    defaults: BTreeMap<&'static str, permission::Default>,
    /// The first registration error, which `finish` reports.
    error: Option<register::Error>,
    /// Item, block, effect and enchantment names.
    names: Names,
    /// The network form of the commands, built by [`finish`](Self::finish).
    catalog: available::Catalog,
    /// Grants and denials of online players, not saved; a player's go when it leaves ([`left`]).
    grants: Grants,
    /// Players whose permissions changed this tick: they get `AvailableCommands` again.
    resend: BTreeSet<SessionId>,
    /// The values of each soft enum of the catalog ([`Context::set_soft_enum`]), not saved.
    soft_values: Vec<Vec<String>>,
    /// Soft enums set this tick: every player gets their values again.
    soft_changed: BTreeSet<usize>,
    /// Commands run since start: the key of `@r`'s random streams.
    runs: u64,
    /// The operator list, the bans and the whitelist.
    access: Access,
    /// Whether periodic saves are on (`/save on`, `/save off`); not saved.
    auto_save: bool,
    /// Whether a command or callback asked the server to stop after this tick.
    stopping: bool,
    /// The tick of the first tick boundary: where the server's uptime counts from.
    started: Option<u64>,
    /// The player a [`Context::run_command`] runs as, while it runs: it has every permission,
    /// and its results are copied to nobody. `None` between callbacks.
    unrestricted: Option<player::Id>,
    /// The players a copy of a command result goes to; [`Admins::Idle`] between callbacks.
    admins: Admins,
}

impl Commands {
    /// The operator list, the bans and the whitelist, shared with the state until the next change.
    pub(super) fn access_lists(&self) -> Arc<Lists> {
        self.access.lists()
    }

    /// Writes what changes while the server runs, as `Self::hash_into` hashes it, and the
    /// start tick.
    pub(super) fn checkpoint(&self, w: &mut Writer) {
        let Self {
            // Code, fixed at build.
            list: _,
            // Tables built from the code at build.
            by_name: _,
            removed: _,
            permissions: _,
            defaults: _,
            // `finish` takes it at build.
            error: _,
            // Tables.
            names: _,
            catalog: _,
            grants,
            resend,
            soft_values,
            soft_changed,
            runs,
            access,
            auto_save,
            stopping,
            started,
            // None between callbacks, where checkpoints are taken.
            unrestricted: _,
            // Idle between callbacks, where checkpoints are taken.
            admins: _,
        } = self;
        w.len(grants.len());
        for (session, grants) in grants {
            session.encode(w);
            w.len(grants.len());
            for (&name, value) in grants {
                w.text(name);
                value.encode(w);
            }
        }
        resend.encode(w);
        soft_values.encode(w);
        soft_changed.encode(w);
        runs.encode(w);
        access.encode(w);
        auto_save.encode(w);
        stopping.encode(w);
        started.encode(w);
    }

    /// Hashes the state (state hash): grants, pending resends, the soft enums, the run count, the
    /// lists and the save and stop switches.
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self {
            // Code, fixed at build.
            list: _,
            // Tables built from the code at build.
            by_name: _,
            removed: _,
            permissions: _,
            defaults: _,
            // `finish` takes it at build.
            error: _,
            // Tables.
            names: _,
            catalog: _,
            grants,
            resend,
            soft_values,
            soft_changed,
            runs,
            access,
            auto_save,
            stopping,
            // The process's own: a replay starts its own, like the uptime it gives.
            started: _,
            // None between callbacks, where hashes are taken.
            unrestricted: _,
            // Idle between callbacks, where hashes are taken.
            admins: _,
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
        h.usize(soft_values.len());
        for values in soft_values {
            h.usize(values.len());
            for value in values {
                h.bytes(value.as_bytes());
            }
        }
        h.usize(soft_changed.len());
        for &index in soft_changed {
            h.usize(index);
        }
        h.u64(*runs);
        access.hash_into(h);
        h.u64(u64::from(*auto_save));
        h.u64(u64::from(*stopping));
    }

    /// Whether periodic saves are on: the save snapshot due every `save-interval-minutes` is
    /// taken only then.
    pub(super) fn is_auto_save_on(&self) -> bool {
        self.auto_save
    }

    /// The source of a player's `AvailableCommands`, without the handlers, so that the output
    /// phase can build it on its threads.
    pub(crate) fn listing(&self) -> Listing<'_> {
        Listing {
            catalog: &self.catalog,
            grants: &self.grants,
            access: &self.access,
            soft_values: &self.soft_values,
        }
    }

    /// The sessions with a grant and the sessions waiting for a resend (leak tests).
    #[cfg(test)]
    pub(super) fn session_entries(&self) -> (usize, usize) {
        (self.grants.len(), self.resend.len())
    }

    /// Whether the server should stop after this tick ([`Context::shutdown`]).
    pub(super) fn stop_requested(&self) -> bool {
        self.stopping
    }

    /// Registers Anchor's own commands the user kept, puts the defaults set at build in place,
    /// builds the commands' network form and reports the first registration error.
    /// `ServerBuilder::build` calls it once.
    pub(crate) fn finish(&mut self) -> register::Result<()> {
        let builtins = builtin::all();
        if let Some(name) = self
            .removed
            .iter()
            .find(|n| !builtins.iter().any(|c| &c.name == *n))
        {
            return Err(register::Error::UnknownCommand { name: name.clone() });
        }
        for command in builtins {
            self.register(command, Origin::Builtin);
        }
        if let Some(e) = self.error.take() {
            return Err(e);
        }
        // After every registration, so a set default replaces the one a command registered.
        for (name, default) in std::mem::take(&mut self.defaults) {
            let Some(registered) = self.permissions.get_mut(name) else {
                return Err(register::Error::UnknownPermission { name });
            };
            *registered = Permission::new(name, default);
        }
        for p in self.list.iter_mut().flat_map(|c| c.permissions.iter_mut()) {
            #[expect(
                clippy::expect_used,
                reason = "invariant: register adds every permission of a command it keeps, and a conflict returned above"
            )]
            let registered = self
                .permissions
                .get(p.name())
                .expect("a command's permission is registered");
            *p = *registered;
        }
        self.catalog = available::Catalog::new(&self.list, &self.names)?;
        self.soft_values = vec![Vec::new(); self.catalog.soft_count()];
        Ok(())
    }

    /// Reads what [`Self::checkpoint`] wrote into the commands registered at build. A grant
    /// names a registered permission, and the soft enums are the catalog's.
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
        let soft_values: Vec<Vec<String>> = Codec::decode(r)?;
        if soft_values.len() != self.catalog.soft_count() {
            return Err(checkpoint::Error::Format("soft enums"));
        }
        self.soft_values = soft_values;
        let soft_changed: BTreeSet<usize> = Codec::decode(r)?;
        if soft_changed.iter().any(|&i| i >= self.soft_values.len()) {
            return Err(checkpoint::Error::Format("changed soft enum"));
        }
        self.soft_changed = soft_changed;
        self.runs = Codec::decode(r)?;
        self.access = Codec::decode(r)?;
        self.auto_save = Codec::decode(r)?;
        self.stopping = Codec::decode(r)?;
        self.started = Codec::decode(r)?;
        Ok(())
    }

    /// Puts in the lists of a checkpoint ([`Self::access_lists`]), after [`Self::restore`].
    pub(super) fn restore_access_lists(&mut self, lists: Arc<Lists>) {
        self.access.set_lists(lists);
    }

    /// Takes the operator list, the bans and the whitelist the server file held, and whether the
    /// whitelist is on (`white-list`). `Server::run` calls it once, before the tick starts.
    pub(crate) fn start(&mut self, access: AccessData, whitelist_enabled: bool) {
        self.access = Access::new(access, whitelist_enabled);
    }

    /// Takes the operator, ban and whitelist changes since the last save snapshot.
    pub(super) fn take_access_changes(&mut self) -> Vec<(AccessKey, AccessWrite)> {
        self.access.take_changes()
    }

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
                    self.error
                        .get_or_insert(register::Error::Duplicate { name: alias });
                    return;
                }
            } else if !names.contains(&alias) {
                command.aliases.push(alias.clone());
                names.push(alias);
            }
        }
        if !builtin && self.by_name.contains_key(&command.name) {
            self.error.get_or_insert(register::Error::Duplicate {
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
                self.error
                    .get_or_insert(register::Error::ConflictingPermission {
                        name: permission.name(),
                    });
            }
            Some(_) => {}
            None => {
                self.permissions.insert(permission.name(), permission);
            }
        }
    }

    /// Forgets the players a copy of a command result goes to, which a permission change may
    /// have changed, so that the running command looks them up again.
    fn forget_admins(&mut self) {
        if let Admins::Known(_) = self.admins {
            self.admins = Admins::Unknown;
        }
    }
}

impl Default for Commands {
    fn default() -> Self {
        Self {
            list: Vec::new(),
            by_name: BTreeMap::new(),
            removed: BTreeSet::new(),
            permissions: permission::BUILTIN.iter().map(|p| (p.name(), *p)).collect(),
            defaults: BTreeMap::new(),
            error: None,
            names: Names::new(),
            catalog: available::Catalog::default(),
            grants: BTreeMap::new(),
            resend: BTreeSet::new(),
            soft_values: Vec::new(),
            soft_changed: BTreeSet::new(),
            runs: 0,
            access: Access::default(),
            auto_save: true,
            stopping: false,
            started: None,
            unrestricted: None,
            admins: Admins::Idle,
        }
    }
}

/// Why fitted arguments did not resolve.
enum Resolve {
    /// A message for the sender.
    Error(Error),
    /// A position left the finite numbers: the line does not fit after all.
    Unfit,
}

/// Why a join is refused, in their priority order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// `max-players` players have joined or are joining.
    Full,
    /// The whitelist is on and the player is neither listed nor an operator.
    NotWhitelisted,
    /// The name is banned, with the ban's reason.
    Banned(String),
    /// The address is banned, with the ban's reason.
    IpBanned(String),
}

impl Refusal {
    /// The last packet the refused session gets: [`queue::server_full`] for a full server,
    /// otherwise a `Disconnect` with the reason `NotAllowed`. A whitelist refusal carries no
    /// message, so the client shows its own text for that reason in the player's language: the
    /// client shows a message as it is and translates no key in it.
    pub(crate) fn packet(&self) -> Packet {
        let message = match self {
            Self::Full => return queue::server_full(),
            Self::NotWhitelisted => String::new(),
            Self::Banned(_) | Self::IpBanned(_) => self.to_message(),
        };
        Packet::Disconnect(Disconnect {
            reason: i32::from(DisconnectReason::NotAllowed),
            hide_disconnection_screen: false,
            message,
            filtered_message: String::new(),
        })
    }

    /// The reason for the log and the `PlayerLogin` kick flag, and the text a player the
    /// refusal kicks out of the game sees.
    pub(crate) fn to_message(&self) -> String {
        match self {
            Self::Full => "disconnectionScreen.serverFull".to_owned(),
            Self::NotWhitelisted => "This server's whitelist does not have you".to_owned(),
            Self::Banned(reason) if reason.is_empty() => "This server has banned you".to_owned(),
            Self::IpBanned(reason) if reason.is_empty() => {
                "This server has banned your address".to_owned()
            }
            Self::Banned(reason) | Self::IpBanned(reason) => {
                format!("This server has banned you: {reason}")
            }
        }
    }
}

impl ServerBuilder {
    /// Registers `command`. The user's commands win over Anchor's own: one of Anchor's commands
    /// whose name a command of the user's takes, as its name or an alias, is left out with its
    /// aliases, and an alias of Anchor's that the user's take is dropped. Two of the user's
    /// commands may not share a name or alias, and [`build`](Self::build) fails with
    /// [`register::Error`] if they do or if a command breaks a rule of [`Command::overload`].
    #[doc(alias = "CommandMap::register")]
    #[doc(alias = "SimpleCommandMap::register")]
    #[must_use]
    pub fn command(mut self, command: Command) -> Self {
        self.setup.commands.register(command, Origin::User);
        self
    }

    /// Registers `permission`, so that [`Context::permission`] finds it by name and
    /// [`Context::set_permission`] grants it. A command's permission is registered with the
    /// command; two permissions of one name with different defaults make
    /// [`build`](Self::build) fail.
    #[doc(alias = "PermissionManager::addPermission")]
    #[must_use]
    pub fn permission(mut self, permission: Permission) -> Self {
        self.setup.commands.add_permission(permission);
        self
    }

    /// Makes `default` who has `permission` unless a grant says otherwise, in place of the
    /// default it was made with: how one of Anchor's own permissions (the
    /// [`permission`] constants) gets another default, such as `/about` for
    /// operators only, `.permission_default(permission::COMMAND_ABOUT, Default::Op)`. Every
    /// check of the permission follows it, a check with the constant included, and
    /// [`Context::permission`] returns it so. Set again for the same permission, the last wins.
    /// A permission no command and no [`permission`](Self::permission) registers makes
    /// [`build`](Self::build) fail ([`register::Error::UnknownPermission`]).
    #[must_use]
    pub fn permission_default(
        mut self,
        permission: Permission,
        default: permission::Default,
    ) -> Self {
        self.setup
            .commands
            .defaults
            .insert(permission.name(), default);
        self
    }

    /// Drops Anchor's own command named `name` (ASCII case does not matter), such as `"list"`. An
    /// alias, or a name none of Anchor's commands has, fails the build with
    /// [`register::Error::UnknownCommand`].
    #[doc(alias = "SimpleCommandMap::unregister")]
    #[must_use]
    pub fn without_command(mut self, name: &str) -> Self {
        self.setup
            .commands
            .removed
            .insert(name.to_ascii_lowercase());
        self
    }
}

impl Context<'_> {
    /// The name bans in force now, in name order.
    #[doc(alias = "BanList::getEntries")]
    #[doc(alias = "Server::getNameBans")]
    pub fn bans(&self) -> impl Iterator<Item = &BanEntry> {
        self.state.commands.access.bans(self.state.tick)
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
        Some(command_at(self.state, index))
    }

    /// Every command, the user's first, in registration order.
    #[doc(alias = "SimpleCommandMap::getCommands")]
    pub fn commands(&self) -> impl Iterator<Item = &Command> {
        self.state.commands.list.iter()
    }

    /// Every registered permission in name order, with whether `sender` has it now, as
    /// [`has_permission`](Self::has_permission) tells: its grant or denial, otherwise the default
    /// for its operator status (Anchor has no child permissions to expand). A player who has left
    /// has none; the player a [`run_command`](Self::run_command) runs as has every one.
    #[doc(alias = "Permissible::getEffectivePermissions")]
    #[doc(alias = "Player::getEffectivePermissions")]
    pub fn effective_permissions(
        &self,
        sender: Sender,
    ) -> impl Iterator<Item = (Permission, bool)> {
        let commands = &self.state.commands;
        let unrestricted =
            matches!(sender, Sender::Player(id) if commands.unrestricted == Some(id));
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
            let has = unrestricted
                || holder.is_some_and(|(session, op)| {
                    has_permission_of(&commands.grants, session, op, p)
                });
            (p, has)
        })
    }

    /// Whether `sender` has `permission`: a player's grant or denial if it has one, otherwise the
    /// permission's default for its operator status (the registered default of its name, which
    /// [`ServerBuilder::permission_default`] may have set). The console counts as an operator. A
    /// player who has left has no permission.
    #[doc(alias = "Permissible::hasPermission")]
    #[doc(alias = "Player::hasPermission")]
    pub fn has_permission(&self, sender: Sender, permission: Permission) -> bool {
        has_permission(self.state, sender, permission)
    }

    /// The IP bans in force now, in address order.
    #[doc(alias = "Server::getIPBans")]
    pub fn ip_bans(&self) -> impl Iterator<Item = &BanEntry> {
        self.state.commands.access.ip_bans(self.state.tick)
    }

    /// Whether the periodic save is on.
    #[doc(alias = "WorldManager::getAutoSave")]
    pub fn is_auto_save_enabled(&self) -> bool {
        self.state.commands.auto_save
    }

    /// Whether `name` (any ASCII case) is banned now.
    #[doc(alias = "BanList::isBanned")]
    pub fn is_banned(&self, name: &str) -> bool {
        self.ban_entry(name).is_some()
    }

    /// Whether `ip` is banned now. An IPv4 address written as IPv6 (`::ffff:1.2.3.4`) is the
    /// IPv4 address.
    pub fn is_ip_banned(&self, ip: IpAddr) -> bool {
        self.ip_ban_entry(ip).is_some()
    }

    /// The ban of `name` (any ASCII case) in force now.
    #[doc(alias = "BanList::getEntry")]
    pub fn ban_entry(&self, name: &str) -> Option<&BanEntry> {
        self.state.commands.access.ban_entry(name, self.state.tick)
    }

    /// The ban of `ip` in force now. An IPv4 address written as IPv6 is the IPv4 address.
    pub fn ip_ban_entry(&self, ip: IpAddr) -> Option<&BanEntry> {
        self.state.commands.access.ip_ban_entry(ip, self.state.tick)
    }

    /// Whether `name` (any ASCII case) is an operator.
    #[doc(alias = "Server::isOp")]
    pub fn is_op(&self, name: &str) -> bool {
        self.state.commands.access.is_op(name)
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

    /// Whether the whitelist is on (`white-list`).
    #[doc(alias = "Server::hasWhitelist")]
    pub fn is_whitelist_enabled(&self) -> bool {
        self.state.commands.access.is_whitelist_enabled()
    }

    /// Whether `name` may join as far as the whitelist goes: the whitelist is off, or `name` is on
    /// it or an operator.
    #[doc(alias = "Server::isWhitelisted")]
    pub fn is_whitelisted(&self, name: &str) -> bool {
        self.state.commands.access.can_join(name)
    }

    /// The operators' names in ASCII lowercase, in name order.
    #[doc(alias = "Server::getOps")]
    pub fn ops(&self) -> impl Iterator<Item = &str> {
        self.state.commands.access.ops()
    }

    /// The registered permission named `name`.
    #[doc(alias = "PermissionManager::getPermission")]
    pub fn permission(&self, name: &str) -> Option<Permission> {
        self.state.commands.permissions.get(name).copied()
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

    /// Every registered permission, in name order.
    #[doc(alias = "PermissionManager::getPermissions")]
    pub fn permissions(&self) -> impl Iterator<Item = Permission> {
        self.state.commands.permissions.values().copied()
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

    /// The whitelisted names in ASCII lowercase, in name order.
    #[doc(alias = "Server::getWhitelisted")]
    pub fn whitelist(&self) -> impl Iterator<Item = &str> {
        self.state.commands.access.whitelisted()
    }

    /// Puts `name` (any ASCII case) on the whitelist, saved at the next tick boundary.
    ///
    /// # Errors
    ///
    /// [`permission::Error::InvalidName`] for a name that is empty, over 64 bytes or holds
    /// control characters.
    /// [`permission::Error::Full`] for a list that holds 262,144 entries already.
    #[doc(alias = "Server::addWhitelist")]
    pub fn add_to_whitelist(&mut self, name: &str) -> permission::Result<()> {
        if self.state.commands.access.whitelist_add(name)? {
            self.state.saving.requested = true;
        }
        Ok(())
    }

    /// Adds `entry` to the name bans, or to the IP bans for [`BanEntry::ip`], replacing a ban of
    /// the same name or address, saved at the next tick boundary. Its creation time is now. The
    /// name and IP bans that have ended are dropped first. A banned player who is online stays
    /// until kicked; the ban refuses its next join, and the join of a player whose saved data
    /// is still loading when its data arrives.
    ///
    /// # Errors
    ///
    /// [`permission::Error::InvalidName`] for a name ban whose name is empty, over 64 bytes or
    /// holds control characters.
    /// [`permission::Error::TooLong`] for a reason over 256 bytes or a source over 64.
    /// [`permission::Error::Expired`] for a ban that ends at a tick that has passed.
    /// [`permission::Error::Full`] for a new entry in a list that holds 262,144 entries.
    #[doc(alias = "BanList::addBan")]
    #[doc(alias = "BanList::add")]
    pub fn ban(&mut self, entry: BanEntry) -> permission::Result<()> {
        let tick = self.state.tick;
        self.state.commands.access.ban(entry, tick)?;
        self.state.saving.requested = true;
        Ok(())
    }

    /// Shows what `sender`'s command did, `message`, to every other online player with
    /// [`BROADCAST_ADMIN`](permission::BROADCAST_ADMIN), as the client's `[sender: message]`
    /// (`chat.type.admin`) in grey italics, and logs it when a player sent the command (the
    /// console logs its own results). The sender itself sees what its handler returns. Nothing
    /// during a [`Self::run_command`].
    ///
    /// # Errors
    ///
    /// [`player::Error::TooLong`] for a message a client does not decode (over 65,536 bytes);
    /// nothing is sent.
    #[doc(alias = "Command::broadcastCommandMessage")]
    pub fn broadcast_command_message(
        &mut self,
        sender: Sender,
        message: &str,
    ) -> player::Result<()> {
        let source = self.sender_name(sender).unwrap_or_default().to_owned();
        let copy = player::Message::Translation {
            key: format!("{ADMIN_PREFIX}%{ADMIN_KEY}"),
            parameters: vec![source, message.to_owned()],
        };
        self.copy_result(sender, copy, message)
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
        commands.forget_admins();
        true
    }

    /// Shows `copy`, the copy of what `sender`'s command did, to every other online player with
    /// [`BROADCAST_ADMIN`](permission::BROADCAST_ADMIN), and logs `result` when a player sent the
    /// command (the console logs its own results). Nothing during a [`Self::run_command`]. Fails,
    /// sending nothing, for a copy a client does not decode.
    pub(crate) fn copy_result(
        &mut self,
        sender: Sender,
        copy: player::Message,
        result: &str,
    ) -> player::Result<()> {
        if self.state.commands.unrestricted.is_some() {
            return Ok(());
        }
        let packets = copy.into_packets()?;
        self.tell_admins(sender, &packets);
        if let Sender::Player(_) = sender {
            let source = self.sender_name(sender).unwrap_or_default();
            debug!(player = %source, %result, "command result");
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

    /// Runs `line` (without `/`) as `sender` would type it, checking `sender`'s permission, and
    /// returns what the sender would see; nothing is sent. A command cannot run itself again
    /// while it runs, through any of its overloads: that fails.
    ///
    /// # Errors
    ///
    /// [`Error::TooLong`] for a line over 2,048 bytes or 512 characters.
    /// [`Error::UnclosedQuote`] for a quote that is not closed.
    /// [`Error::Unknown`] for a line without a command name or with one no command has.
    /// [`Error::Permission`] if `sender` lacks the command's permission.
    /// [`Error::Syntax`] for a line that fits none of the command's overloads.
    /// [`Error::PlayerNotFound`] for a player name no online player has.
    /// [`Error::NoTargets`] for a target selector that matched nobody playing.
    /// [`Error::Usage`] for arguments that fit an overload but cannot be used.
    /// [`Error::Failed`] for the command running itself again, or the handler failing; a handler
    /// may return any other [`Error`] too.
    #[doc(alias = "Server::dispatchCommand")]
    #[doc(alias = "CommandMap::dispatch")]
    #[doc(alias = "SimpleCommandMap::dispatch")]
    pub fn dispatch_command(&mut self, sender: Sender, line: &str) -> command::Result<Output> {
        execute(self.state, sender, line)
    }

    /// Makes `name` (any ASCII case) an operator, saved at the next tick boundary; an online
    /// player of that name gets its new command list and operator abilities at the end of the
    /// tick.
    ///
    /// # Errors
    ///
    /// [`permission::Error::InvalidName`] for a name that is empty, over 64 bytes or holds
    /// control characters.
    /// [`permission::Error::Full`] for a list that holds 262,144 entries already.
    #[doc(alias = "Server::addOp")]
    pub fn op(&mut self, name: &str) -> permission::Result<()> {
        if self.state.commands.access.op(name)? {
            self.state.saving.requested = true;
            self.operator_changed(name, true);
        }
        Ok(())
    }

    /// Takes `name` off the whitelist; false if it was not on it. A player who is online stays
    /// until kicked; one whose saved data is still loading is refused when its data arrives, if
    /// the whitelist then keeps it out.
    #[doc(alias = "Server::removeWhitelist")]
    pub fn remove_from_whitelist(&mut self, name: &str) -> bool {
        let removed = self.state.commands.access.whitelist_remove(name);
        self.state.saving.requested |= removed;
        removed
    }

    /// Runs `line` (without `/`) as `player` would type it (`@s` and relative positions are the
    /// player's), but as if it had every permission, and returns what it would see; nothing is
    /// sent, and no operator gets a copy of the result (the Script API's `Player::runCommand`).
    /// A player who has left runs nothing: [`Error::NoTargets`], as there is nobody to run it
    /// as. A command cannot run itself again while it runs, through any of its overloads: that
    /// fails.
    ///
    /// # Errors
    ///
    /// [`Error::NoTargets`] for a player who has left, or a target selector that matched nobody
    /// playing. The other errors are those of [`dispatch_command`](Self::dispatch_command), except
    /// [`Error::Permission`], which the player never meets.
    pub fn run_command(&mut self, player: player::Id, line: &str) -> command::Result<Output> {
        if !self.state.players.contains_key(&player.session()) {
            return Err(Error::NoTargets);
        }
        let outer = self.state.commands.unrestricted.replace(player);
        let result = execute(self.state, Sender::Player(player), line);
        self.state.commands.unrestricted = outer;
        result
    }

    /// Shows `sender` `message`: a player as [`Player::send`](player::Player::send) shows it, the
    /// console in the log. For a player who has left it sends nothing.
    ///
    /// # Errors
    ///
    /// [`player::Error::TooLong`] for a message too long for a client to decode, as
    /// [`Player::send`](player::Player::send) refuses it.
    /// [`player::Error::TooManyParameters`] for a message with more parameters than a client
    /// decodes.
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

    /// Turns the periodic save (every `save-interval-minutes`) off or on until the server
    /// stops. Only the periodic save stops: a save something asks for ([`save`](Self::save),
    /// [`backup`](Self::backup), a change of the operator, ban or whitelist lists, a world
    /// created or deleted), a save that memory or the input log waiting for one makes due, and
    /// the save at shutdown still happen. A crash replays from the last save.
    #[doc(alias = "WorldManager::setAutoSave")]
    pub fn set_auto_save_enabled(&mut self, enabled: bool) {
        if self.state.commands.auto_save != enabled {
            info!(enabled, "periodic save switched until the server stops");
        }
        self.state.commands.auto_save = enabled;
    }

    /// Sets the game mode new players get (`gamemode`) until the server stops; with
    /// `force-gamemode` every join gets it. The server list shows it once this tick ends.
    ///
    /// # Errors
    ///
    /// [`Error::NetworkBusy`](crate::task::Error::NetworkBusy) for more than 256 network changes
    /// in one tick; nothing is changed.
    pub fn set_default_game_mode(&mut self, mode: GameMode) -> crate::task::Result<()> {
        if !self.state.network.control(queue::Control::GameMode(mode)) {
            return Err(crate::task::Error::NetworkBusy);
        }
        self.state.settings.game_mode = mode;
        Ok(())
    }

    /// Sets the difficulty until the server stops and tells the players. Every loaded world
    /// follows it from now on: a difficulty of its own
    /// ([`set_world_difficulty`](Self::set_world_difficulty)) is dropped. On a `hardcore` server
    /// only [`Difficulty::Hard`] is allowed.
    ///
    /// # Errors
    ///
    /// [`Error::Hardcore`](crate::world::Error::Hardcore) for another difficulty on a hardcore
    /// server, changing nothing.
    pub fn set_difficulty(&mut self, difficulty: Difficulty) -> crate::world::Result<()> {
        if self.state.settings.hardcore && difficulty != Difficulty::Hard {
            return Err(crate::world::Error::Hardcore);
        }
        self.state.settings.difficulty = difficulty;
        for world in self.state.worlds.iter_mut() {
            if self.state.catalog.is_loaded(world.id().index()) {
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
        Ok(())
    }

    /// Grants (`value` true) or denies `permission` to `player` until it leaves, whatever its
    /// default says; its command list follows at the end of the tick. Not saved. Does nothing if
    /// no command and no [`ServerBuilder::permission`] registered a permission of its name, the
    /// player has left or it has 1,024 grants.
    #[doc(alias = "PermissionAttachment::setPermission")]
    pub fn set_permission(&mut self, player: player::Id, permission: Permission, value: bool) {
        let session = player.session();
        if !self.state.players.contains_key(&session) {
            return;
        }
        let commands = &mut self.state.commands;
        // A checkpoint names a grant by its permission, and a replay finds only registered ones.
        if !commands.permissions.contains_key(permission.name()) {
            return;
        }
        let grants = commands.grants.entry(session).or_default();
        if !grants.contains_key(permission.name()) && grants.len() >= MAX_GRANTS {
            return;
        }
        if grants.insert(permission.name(), value) != Some(value) {
            commands.resend.insert(session);
            commands.forget_admins();
        }
    }

    /// Sets the values the client suggests for the soft enum type `name` of a
    /// [`Param::soft_enumeration`](command::Param::soft_enumeration), in order; every player's
    /// list follows at the end of the tick. Not saved: none at start. A value may hold spaces: the
    /// parameter takes it typed in double quotes.
    ///
    /// # Errors
    ///
    /// [`register::Error::InvalidSoftEnum`] for a `name` no registered command takes, an empty
    /// value, or more than 4,096 or more than 65,536 bytes of values in all; nothing is changed.
    pub fn set_soft_enum(&mut self, name: &str, values: &[&str]) -> register::Result<()> {
        let commands = &mut self.state.commands;
        let invalid = || register::Error::InvalidSoftEnum {
            name: name.to_owned(),
        };
        let index = commands.catalog.soft_enum(name).ok_or_else(invalid)?;
        if values.len() > MAX_ENUM_VALUES
            || values
                .iter()
                .map(|v| v.len())
                .fold(0, usize::saturating_add)
                > MAX_TEXT_BYTES
            || values.iter().any(|v| v.is_empty())
        {
            return Err(invalid());
        }
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: the catalog's soft enum positions are those of `soft_values`"
        )]
        let current = &mut commands.soft_values[index];
        if current
            .iter()
            .map(String::as_str)
            .ne(values.iter().copied())
        {
            *current = values.iter().map(|v| (*v).to_owned()).collect();
            commands.soft_changed.insert(index);
        }
        Ok(())
    }

    /// Turns the whitelist on or off until the server stops (`white-list` decides at start:
    /// Anchor never writes the settings file). Players already online stay; one whose saved data
    /// is still loading is refused when its data arrives, if the whitelist then keeps it out.
    pub fn set_whitelist_enabled(&mut self, enabled: bool) {
        if self.state.commands.access.is_whitelist_enabled() != enabled {
            info!(enabled, "whitelist switched until the server stops");
        }
        self.state.commands.access.set_whitelist_enabled(enabled);
    }

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

    /// Lifts the ban of `name` (any ASCII case); false if there was none.
    #[doc(alias = "BanList::remove")]
    pub fn unban(&mut self, name: &str) -> bool {
        let lifted = self.state.commands.access.unban(name);
        self.state.saving.requested |= lifted;
        lifted
    }

    /// Lifts the ban of `ip`; false if there was none. An IPv4 address written as IPv6 is the
    /// IPv4 address.
    pub fn unban_ip(&mut self, ip: IpAddr) -> bool {
        let lifted = self.state.commands.access.unban_ip(ip);
        self.state.saving.requested |= lifted;
        lifted
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
        commands.forget_admins();
        true
    }

    /// Makes every joined player named `name` (ASCII case), loading or not, follow a change of
    /// operator status to `op` with its command list and abilities; players signed in with Xbox
    /// may share a name.
    fn operator_changed(&mut self, name: &str, op: bool) {
        let state = &mut *self.state;
        state.commands.forget_admins();
        let named = state
            .names
            .range((name.to_ascii_lowercase(), SessionId::new(0))..)
            .take_while(|(n, _)| n.eq_ignore_ascii_case(name));
        for &(_, session) in named {
            if let Some(p) = state.players.get_mut(&session) {
                state.commands.resend.insert(session);
                p.survival.set_operator(op);
            }
        }
    }

    /// Sends `packets` to every online player with
    /// [`BROADCAST_ADMIN`](permission::BROADCAST_ADMIN) but `sender`, as a viewer: a command on
    /// many players makes a copy for each, so one whose outbox is nearly full is skipped. While a
    /// command runs, the players are looked up once ([`Admins`]).
    fn tell_admins(&mut self, sender: Sender, packets: &player::Packets) {
        let running = !matches!(self.state.commands.admins, Admins::Idle);
        let admins = match std::mem::replace(&mut self.state.commands.admins, Admins::Idle) {
            Admins::Idle | Admins::Unknown => self
                .players()
                .map(player::Player::id)
                .filter(|&id| self.has_permission(Sender::Player(id), permission::BROADCAST_ADMIN))
                .collect(),
            Admins::Known(admins) => admins,
        };
        for &id in &admins {
            if Sender::Player(id) == sender {
                continue;
            }
            if let Some(p) = self.player_mut(id)
                && p.has_viewer_room()
            {
                for packet in packets.iter().flatten() {
                    p.send_packet(packet.clone());
                }
            }
        }
        if running {
            self.state.commands.admins = Admins::Known(admins);
        }
    }
}

/// Checks a command's names and overloads (the rules of [`Command::overload`]).
fn check(command: &Command) -> register::Result<()> {
    for name in std::iter::once(&command.name).chain(&command.aliases) {
        if name.is_empty()
            || name.len() > MAX_NAME
            || name
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '/')
        {
            return Err(register::Error::InvalidName { name: name.clone() });
        }
    }
    let invalid = |reason| {
        Err(register::Error::InvalidOverload {
            command: command.name.clone(),
            reason,
        })
    };
    if command.description.len() > COMMAND_DESCRIPTION_MAX_LEN {
        return Err(register::Error::DescriptionTooLong {
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
            if let Kind::Enum { values, .. } = &param.kind
                && values.len() > MAX_ENUM_VALUES
            {
                return invalid("more than 4096 enum values");
            }
            if let Kind::Enum { name, values, .. } = &param.kind
                && (name.is_empty()
                    || values.is_empty()
                    || values
                        .iter()
                        .any(|v| v.is_empty() || v.chars().any(char::is_whitespace)))
            {
                return invalid(
                    "an enum has no type name, no values, an empty value or a value with spaces",
                );
            }
            if let Kind::SoftEnum { name } = &param.kind
                && name.is_empty()
            {
                return invalid("a soft enum has no type name");
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

/// Handles a `CommandRequest` of `session`: a `/` line from a joined player. Each spends one line
/// of the player's chat limit ([`chat::spend_line`]: a command request counts as a chat line), so
/// chat lines and commands together pass 2 per tick. The player gets a `CommandOutput` that
/// answers the request's origin.
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
            info!(output = %line, "console command failed");
        }
    }
}

/// Runs a command line a player or the console typed (without its `/`) after asking the
/// [`crate::event::Command`] listeners, which may cancel it or change the
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

/// Runs `line` (without its `/`) as `sender`: finds the command, checks the permission, fits the
/// words to the first overload they fit, resolves players and positions, and runs the handler.
/// A command fails while one of its handlers runs: it cannot run itself again through any of its
/// overloads.
fn execute(state: &mut State, sender: Sender, line: &str) -> command::Result<Output> {
    let Some(label) = parse::first_word(line)? else {
        return Err(Error::Unknown {
            name: String::new(),
        });
    };
    let rest_start = label.end;
    let label = label.text.to_ascii_lowercase();
    let Some(&index) = state.commands.by_name.get(&label) else {
        return Err(Error::Unknown { name: label });
    };
    let command = command_at(state, index);
    let name = command.name.clone();
    if !allowed(&command.permissions, |p| has_permission(state, sender, p)) {
        return Err(Error::Permission { name });
    }
    // The first overload that fits, else where the overload that fits furthest stops.
    let mut furthest = 0;
    let mut unclosed = false;
    let mut fitted = None;
    for (i, o) in command.overloads.iter().enumerate() {
        match parse::fit(&o.params, line, rest_start, &state.commands.names) {
            Ok(raw) => {
                fitted = Some((o, i, raw));
                break;
            }
            Err(parse::Misfit::At(at)) => furthest = furthest.max(at),
            Err(parse::Misfit::UnclosedQuote) => unclosed = true,
        }
    }
    let Some((fitted, overload, raw)) = fitted else {
        if unclosed {
            return Err(Error::UnclosedQuote);
        }
        return Err(parse::syntax_error(line, furthest));
    };
    let names: Vec<String> = fitted.params.iter().map(|p| p.name.clone()).collect();
    let running = command.overloads.iter().any(|o| o.handler.is_none());
    state.commands.runs = state.commands.runs.wrapping_add(1);
    let args = match resolve(state, sender, names, raw) {
        Ok(args) => args,
        Err(Resolve::Error(e)) => return Err(e),
        Err(Resolve::Unfit) => {
            return Err(Error::Usage {
                usage: command_at(state, index).usage_text(),
            });
        }
    };
    if running {
        return Err(Error::failed(format!(
            "the command {name} is already running"
        )));
    }
    #[expect(
        clippy::expect_used,
        reason = "invariant: no handler of the command runs, so each waits in its slot"
    )]
    let mut handler = handler_slot_mut(state, index, overload)
        .take()
        .expect("a handler that is not running waits in its slot");
    let outer = std::mem::replace(&mut state.commands.admins, Admins::Unknown);
    let result = handler(&mut Context { state }, &sender, &args);
    *handler_slot_mut(state, index, overload) = Some(handler);
    // A run inside this one may have changed a permission: the outer run looks them up again.
    state.commands.admins = match outer {
        Admins::Idle => Admins::Idle,
        Admins::Unknown | Admins::Known(_) => Admins::Unknown,
    };
    result
}

/// The registered command at `index`, a position `by_name` gave.
fn command_at(state: &State, index: usize) -> &Command {
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: `by_name` holds positions of `list`"
    )]
    &state.commands.list[index]
}

/// The slot where the handler of overload `overload` of command `index` waits between
/// runs.
fn handler_slot_mut(
    state: &mut State,
    index: usize,
    overload: usize,
) -> &mut Option<command::Handler> {
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: `index` and `overload` name an overload of a registered command"
    )]
    &mut state.commands.list[index].overloads[overload].handler
}

/// Whether `sender` has `permission`, with the registered default of its name (one set at
/// build, [`ServerBuilder::permission_default`]); a player who has left has none, and the player
/// a [`Context::run_command`] runs as has every one.
fn has_permission(state: &State, sender: Sender, permission: Permission) -> bool {
    let permission = state
        .commands
        .permissions
        .get(permission.name())
        .copied()
        .unwrap_or(permission);
    match sender {
        Sender::Console => permission.granted_by_default(true),
        Sender::Player(id) if state.commands.unrestricted == Some(id) => true,
        Sender::Player(id) => state.players.get(&id.session()).is_some_and(|p| {
            let op = state.commands.access.is_op(&p.identity.name);
            has_permission_of(&state.commands.grants, Some(id.session()), op, permission)
        }),
    }
}

/// Whether a sender may use a command needing one of `permissions` (none: everyone), `has`
/// telling whether it has one.
fn allowed(permissions: &[Permission], has: impl Fn(Permission) -> bool) -> bool {
    permissions.is_empty() || permissions.iter().any(|&p| has(p))
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
    for ((name, raw), param) in names.into_iter().zip(raw).zip(0_u64..) {
        let value = match raw {
            Raw::Value(v) => v,
            Raw::Target(target) => Value::Targets(targets(state, sender, origin, target, param)?),
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
            let spawn = super::world_api::world_at(state, default).spawn();
            Some((
                default,
                Vec3::new(f64::from(spawn.x), f64::from(spawn.y), f64::from(spawn.z)),
            ))
        }
    }
}

/// Finds the players target parameter `param` of the line names, at least one. `@r` draws from
/// the default world's seed and a stream of its own for each run and parameter, so two `@r` of
/// one line draw apart.
fn targets(
    state: &mut State,
    sender: Sender,
    origin: Option<(usize, Vec3)>,
    target: Target,
    param: u64,
) -> Result<Vec<player::Id>, Resolve> {
    let playing = |p: &super::Player| p.phase != Phase::Loading;
    let found: Vec<player::Id> = match target {
        Target::Name(name) => {
            let id = Context { state: &mut *state }.player_by_name(&name);
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
            let seed = Context { state: &mut *state }.default_world().seed();
            let key = random::key(RANDOM_TARGET, &[state.commands.runs, param]);
            let mut rng = Random::new(seed, state.tick, key);
            #[expect(
                clippy::expect_used,
                reason = "invariant: the player table holds far fewer than 2^32 players"
            )]
            let n = u32::try_from(candidates.len()).expect("the player count fits a u32");
            #[expect(
                clippy::expect_used,
                reason = "invariant: the supported targets' usize holds a u32"
            )]
            let pick = usize::try_from(rng.below(n)).expect("a u32 fits a usize");
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

/// Builds the `CommandOutput` answering `origin` with `result`: the lines of a success
/// ([`shown`], one success counted), or the error in red (none counted).
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

/// Picks what a sender is shown of `result`: whether it succeeded, and the lines of the success
/// or the error's line, within bounds a client takes and a log keeps readable. At most
/// [`MAX_OUTPUT_LINES`] lines, the last telling how many were left out when there were more; at
/// most [`MAX_OUTPUT_PARAMS`] parameters a line; and at most [`MAX_TEXT_BYTES`] bytes of the result's
/// text and parameters in all (one client text), a text or parameter that passes it cut at a
/// character boundary and the lines after it left out, which the line telling how many were left
/// out adds to.
pub(super) fn shown(result: &command::Result<Output>) -> (bool, Vec<Line>) {
    let error;
    let (success, lines) = match result {
        Ok(output) => (true, output.lines.as_slice()),
        Err(e) => {
            error = [e.line()];
            (false, error.as_slice())
        }
    };
    let mut budget = MAX_TEXT_BYTES;
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

/// Cuts `text` to at most `budget` bytes at a character boundary and takes the bytes kept from
/// `budget`.
fn cut(text: &str, budget: &mut usize) -> String {
    let (kept, _) = text.split_at(text.floor_char_boundary(*budget));
    *budget -= kept.len();
    kept.to_owned()
}

/// Ends the tick's serial phases for commands: players whose permissions changed get
/// `AvailableCommands` again, and every player the soft enums set this tick (only after their
/// spawn window, which carries the first ones).
pub(super) fn sync(state: &mut State) {
    state.commands.started.get_or_insert(state.tick);
    for index in std::mem::take(&mut state.commands.soft_changed) {
        let commands = &state.commands;
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: a changed soft enum is a position of the catalog's soft enums"
        )]
        let options = commands.soft_values[index].clone();
        let packet = Arc::new(SharedPacket::new(Packet::UpdateSoftEnum(UpdateSoftEnum {
            enum_type: commands.catalog.soft_name(index).to_owned(),
            options,
            action_type: u8::from(SoftEnumAction::Set),
        })));
        for player in state.players.values_mut() {
            if player.chunk_radius.is_some() {
                player.send_shared(Arc::clone(&packet));
            }
        }
    }
    for session in std::mem::take(&mut state.commands.resend) {
        let listing = state.commands.listing();
        #[expect(
            clippy::expect_used,
            reason = "invariant: `left` takes a leaving player out of `resend`"
        )]
        let player = state
            .players
            .get_mut(&session)
            .expect("a player to resend to is online");
        if player.chunk_radius.is_none() {
            continue;
        }
        let packet = available_commands(listing, session, player);
        player.send_packet(packet);
    }
}

/// Forgets the grants and the pending command list of the player of `session`, which is leaving:
/// the tick calls it where it removes a player, so grants live no longer than their player.
pub(super) fn left(state: &mut State, session: SessionId) {
    state.commands.grants.remove(&session);
    state.commands.resend.remove(&session);
}

/// Checks whether `identity` may join now (first refusal in priority order): the server is not
/// full, the whitelist lets it in, and neither its name nor its address is banned. The slot of
/// the same player online counts as free: its join replaces that player once admitted.
pub(crate) fn admit(state: &State, identity: &Identity) -> Result<(), Refusal> {
    refusals(state, identity)
        .into_iter()
        .flatten()
        .next()
        .map_or(Ok(()), Err)
}

/// Every refusal of `identity` now, one per kick flag in priority order: the server
/// is full, the whitelist keeps it out, its name is banned (else its address). `PlayerLogin`
/// carries them all, so that a listener clearing one does not let in a player another refuses.
pub(crate) fn refusals(state: &State, identity: &Identity) -> [Option<Refusal>; 3] {
    let max_players = usize::try_from(state.settings.max_players).unwrap_or(usize::MAX);
    let replaced = state.index.contains_key(&PlayerKey::of(identity));
    let others = state.players.len() - usize::from(replaced);
    let full = (others >= max_players).then_some(Refusal::Full);
    let access = &state.commands.access;
    let whitelisted = (!access.can_join(&identity.name)).then_some(Refusal::NotWhitelisted);
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

#[cfg(test)]
mod tests;
