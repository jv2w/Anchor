//! Permissions: what a player may do, with the defaults op, not op, everyone and nobody.
//!
//! A [`Permission`] is a name and a [`Default`](enum@Default): who has it unless the server says
//! otherwise. Register yours with
//! [`ServerBuilder::permission`](crate::ServerBuilder::permission); a command's permission is
//! registered with the command. Whether someone has a permission comes from its default and the
//! operator list, unless a grant made while the player is online
//! ([`Context::set_permission`](crate::Context::set_permission)) says otherwise. Grants are not
//! saved; the operator list, the ban lists and the whitelist are saved in the data files with
//! everything else.
//!
//! Anchor's own permissions are the constants of this module, such as `anchor.command.give.self`.
//! [`ServerBuilder::permission_default`](crate::ServerBuilder::permission_default) gives one
//! another default for the whole server.

use std::fmt;
use std::net::IpAddr;

/// The most entries one list holds (operators, name bans, IP bans, whitelist).
pub(crate) const MAX_ENTRIES: usize = 262_144;
/// The longest name the operator list, the ban list and the whitelist hold, in bytes.
pub(crate) const MAX_NAME: usize = 64;
/// The longest ban reason, in bytes.
pub(crate) const MAX_REASON: usize = 256;
/// The longest ban source, in bytes.
pub(crate) const MAX_SOURCE: usize = 64;
/// The source of a ban nobody named.
const UNKNOWN_SOURCE: &str = "(Unknown)";

/// A permission: a name, unique on the server, and who has it by default.
///
/// It is a `Copy` value, usually a constant:
///
/// ```
/// use anchor::permission::{Default, Permission};
/// const FLY: Permission = Permission::new("lobby.fly", Default::Op);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
// api-class: Permission
pub struct Permission {
    name: &'static str,
    default: Default,
}

impl Permission {
    /// A permission named `name` that the players `default` names have.
    #[doc(alias = "Permission::__construct")]
    pub const fn new(name: &'static str, default: Default) -> Self {
        Self { name, default }
    }

    /// Who has the permission unless a grant says otherwise: as made, which the server's checks
    /// follow unless a default set at build replaces it
    /// ([`Context::permission`](crate::Context::permission) returns the registered one).
    pub const fn default(self) -> Default {
        self.default
    }

    /// Whether a player who is (`op`) or is not an operator has the permission by default. The
    /// console counts as an operator.
    pub(crate) const fn granted_by_default(self, op: bool) -> bool {
        match self.default {
            Default::Op => op,
            Default::NotOp => !op,
            Default::True => true,
            Default::False => false,
        }
    }

    /// The name, such as `anchor.command.give.self`.
    #[doc(alias = "Permission::getName")]
    pub const fn name(self) -> &'static str {
        self.name
    }
}

/// Who has a permission by default.
#[doc(alias = "PermissionParser")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Default {
    /// Operators and the console.
    Op,
    /// Players who are not operators (not the console).
    NotOp,
    /// Everyone.
    True,
    /// Nobody, unless granted.
    False,
}

impl Default {
    /// The default a permission file names: `op`, `notop`, `true`, `false` and the other
    /// spellings (`isop`, `admin`, `!op`, `notadmin`, ...), ignoring ASCII case.
    #[doc(alias = "PermissionParser::defaultFromString")]
    pub fn from_name(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        Some(match lower.as_str() {
            "op" | "isop" | "operator" | "isoperator" | "admin" | "isadmin" => Self::Op,
            "!op" | "notop" | "!operator" | "notoperator" | "!admin" | "notadmin" => Self::NotOp,
            "true" => Self::True,
            "false" => Self::False,
            _ => return None,
        })
    }
}

/// A ban: a player name or an IP address that may not join, why, who banned it, when, and until
/// when.
///
/// Times are engine ticks ([`Context::tick`](crate::Context::tick)), not wall-clock dates: the
/// game state never reads the wall clock, so a ban that ends 72,000 ticks after the current tick
/// lasts an hour of server running time, and the time the server is down does not count.
#[derive(Debug, Clone, PartialEq, Eq)]
// api-class: BanEntry
pub struct BanEntry {
    /// The name in ASCII lowercase, or the IP address as text.
    name: String,
    /// The address of an IP ban.
    ip: Option<IpAddr>,
    reason: String,
    source: String,
    created: u64,
    expires: Option<u64>,
}

impl BanEntry {
    /// An entry as the data files store it.
    pub(crate) fn from_parts(
        name: String,
        ip: Option<IpAddr>,
        reason: String,
        source: String,
        created: u64,
        expires: Option<u64>,
    ) -> Self {
        Self {
            name,
            ip,
            reason,
            source,
            created,
            expires,
        }
    }

    /// A ban of the IP address `ip`, without reason or end. An IPv4 address written as IPv6
    /// (`::ffff:1.2.3.4`) is the IPv4 address.
    pub fn ip(ip: IpAddr) -> Self {
        let ip = ip.to_canonical();
        Self {
            name: ip.to_string(),
            ip: Some(ip),
            ..Self::new("")
        }
    }

    /// A ban of the player named `name` (ASCII case does not matter), without reason or end.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_ascii_lowercase(),
            ip: None,
            reason: String::new(),
            source: UNKNOWN_SOURCE.to_owned(),
            created: 0,
            expires: None,
        }
    }

    /// The engine tick the ban was added at.
    #[doc(alias = "BanEntry::getCreated")]
    pub fn created(&self) -> u64 {
        self.created
    }

    /// The engine tick the ban ends at; `None` if it lasts until lifted.
    #[doc(alias = "BanEntry::getExpires")]
    pub fn expires(&self) -> Option<u64> {
        self.expires
    }

    /// Whether the ban has ended at engine tick `tick`.
    #[doc(alias = "BanEntry::hasExpired")]
    pub fn has_expired(&self, tick: u64) -> bool {
        self.expires.is_some_and(|end| tick >= end)
    }

    /// The banned IP address, for an IP ban.
    pub fn ip_address(&self) -> Option<IpAddr> {
        self.ip
    }

    /// The banned name in ASCII lowercase, or the banned IP address as text.
    #[doc(alias = "BanEntry::getName")]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Why; empty if no reason was given.
    #[doc(alias = "BanEntry::getReason")]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Who or what banned.
    #[doc(alias = "BanEntry::getSource")]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The entry as the ban list keeps it, added at `tick`; fails if a field is over its bound,
    /// the name is not a valid list name or the ban has ended already.
    pub(crate) fn checked(mut self, tick: u64) -> Result<Self> {
        if self.ip.is_none() {
            check_name(&self.name)?;
        }
        check_ban_fields(&self.reason, &self.source)?;
        if self.has_expired(tick) {
            return Err(Error::Expired);
        }
        self.created = tick;
        Ok(self)
    }

    /// The ban ending at engine tick `tick`; a ban without an end lasts until it is lifted. A tick
    /// that has passed when the ban is added refuses it ([`Error::Expired`]).
    #[doc(alias = "BanEntry::setExpires")]
    #[must_use]
    pub fn with_expires(mut self, tick: u64) -> Self {
        self.expires = Some(tick);
        self
    }

    /// The ban with `reason`, which a banned player sees when refused.
    #[doc(alias = "BanEntry::setReason")]
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = reason.into();
        self
    }

    /// The ban with `source`: who or what banned (`(Unknown)` by default).
    #[doc(alias = "BanEntry::setSource")]
    #[must_use]
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }
}

/// Why a list did not change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The name is empty, longer than 64 bytes or holds control characters.
    InvalidName,
    /// The ban reason is longer than 256 bytes or its source longer than 64.
    TooLong,
    /// The ban ends at an engine tick that has passed already.
    Expired,
    /// The list holds 262,144 entries.
    Full,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => {
                f.write_str("the name is empty, too long or holds control characters")
            }
            Self::TooLong => f.write_str("the ban reason or source is too long"),
            Self::Expired => f.write_str("the ban has ended already"),
            Self::Full => f.write_str("the list is full"),
        }
    }
}

impl std::error::Error for Error {}

/// A result with the module's [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Declares Anchor's own permissions and the list of them.
macro_rules! builtin {
    ($($(#[$doc:meta])* $name:ident = $text:literal, $default:ident;)*) => {
        $(
            $(#[$doc])*
            pub const $name: Permission = Permission::new($text, Default::$default);
        )*

        /// Anchor's own permissions, registered on every server.
        pub(crate) const BUILTIN: &[Permission] = &[$($name),*];
    };
}

builtin! {
    /// Get a copy of what operators' commands did, such as `[Steve: Opped Alex]`.
    #[doc(alias = "DefaultPermissionNames")]
    BROADCAST_ADMIN = "anchor.broadcast.admin", Op;
    /// Show the server version (`/about`).
    COMMAND_ABOUT = "anchor.command.about", True;
    /// Back up the data files while the server runs (`/backup`).
    COMMAND_BACKUP = "anchor.command.backup", Op;
    /// Ban IP addresses (`/ban-ip`).
    COMMAND_BAN_IP = "anchor.command.ban.ip", Op;
    /// List the bans (`/banlist`).
    COMMAND_BAN_LIST = "anchor.command.ban.list", Op;
    /// Ban players (`/ban`).
    COMMAND_BAN_PLAYER = "anchor.command.ban.player", Op;
    /// Check the data files while the server runs (`/check`).
    COMMAND_CHECK = "anchor.command.check", Op;
    /// Clear players' inventories (`/clear`).
    COMMAND_CLEAR = "anchor.command.clear", Op;
    /// Change another player's game mode (`/gamemode`).
    COMMAND_GAMEMODE_OTHER = "anchor.command.gamemode.other", Op;
    /// Change one's own game mode (`/gamemode`).
    COMMAND_GAMEMODE_SELF = "anchor.command.gamemode.self", Op;
    /// Give items to another player (`/give`).
    COMMAND_GIVE_OTHER = "anchor.command.give.other", Op;
    /// Give items to oneself (`/give`).
    COMMAND_GIVE_SELF = "anchor.command.give.self", Op;
    /// Kick players (`/kick`).
    COMMAND_KICK = "anchor.command.kick", Op;
    /// Kill players (`/kill`).
    COMMAND_KILL = "anchor.command.kill", Op;
    /// List the players online (`/list`).
    COMMAND_LIST = "anchor.command.list", Op;
    /// Make players operators (`/op`).
    COMMAND_OP_GIVE = "anchor.command.op.give", Op;
    /// Take operator status away (`/deop`).
    COMMAND_OP_TAKE = "anchor.command.op.take", Op;
    /// Turn periodic saving off (`/save-off`).
    COMMAND_SAVE_DISABLE = "anchor.command.save.disable", Op;
    /// Turn periodic saving on (`/save-on`).
    COMMAND_SAVE_ENABLE = "anchor.command.save.enable", Op;
    /// Save now (`/save-all`).
    COMMAND_SAVE_PERFORM = "anchor.command.save.perform", Op;
    /// Show the server's uptime and load (`/status`).
    COMMAND_STATUS = "anchor.command.status", Op;
    /// Stop the server (`/stop`).
    COMMAND_STOP = "anchor.command.stop", Op;
    /// Teleport other players (`/tp`).
    COMMAND_TELEPORT_OTHER = "anchor.command.teleport.other", Op;
    /// Teleport oneself (`/tp`).
    COMMAND_TELEPORT_SELF = "anchor.command.teleport.self", Op;
    /// Lift IP bans (`/unban-ip`).
    COMMAND_UNBAN_IP = "anchor.command.unban.ip", Op;
    /// Lift player bans (`/unban`).
    COMMAND_UNBAN_PLAYER = "anchor.command.unban.player", Op;
    /// Add players to the whitelist (`/whitelist add`).
    COMMAND_WHITELIST_ADD = "anchor.command.whitelist.add", Op;
    /// Turn the whitelist off (`/whitelist off`).
    COMMAND_WHITELIST_DISABLE = "anchor.command.whitelist.disable", Op;
    /// Turn the whitelist on (`/whitelist on`).
    COMMAND_WHITELIST_ENABLE = "anchor.command.whitelist.enable", Op;
    /// List the whitelist (`/whitelist list`).
    COMMAND_WHITELIST_LIST = "anchor.command.whitelist.list", Op;
    /// Remove players from the whitelist (`/whitelist remove`).
    COMMAND_WHITELIST_REMOVE = "anchor.command.whitelist.remove", Op;
}

/// Fails unless a ban's `reason` is at most [`MAX_REASON`] bytes and its `source` at most
/// [`MAX_SOURCE`].
pub(crate) fn check_ban_fields(reason: &str, source: &str) -> Result<()> {
    if reason.len() > MAX_REASON || source.len() > MAX_SOURCE {
        return Err(Error::TooLong);
    }
    Ok(())
}

/// Fails unless `name` can be on a list: 1 to [`MAX_NAME`] bytes without control characters.
pub(crate) fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(Error::InvalidName);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // api: PermissionParser::defaultFromString, PermissionParser, Permission::__construct
    #[test]
    fn defaults_parse_their_spellings_and_decide_by_operator_status() {
        assert_eq!(Default::from_name("IsOp"), Some(Default::Op));
        assert_eq!(Default::from_name("!admin"), Some(Default::NotOp));
        assert_eq!(Default::from_name("TRUE"), Some(Default::True));
        assert_eq!(Default::from_name("false"), Some(Default::False));
        assert_eq!(Default::from_name("maybe"), None);
        let table = [
            (Default::Op, [false, true]),
            (Default::NotOp, [true, false]),
            (Default::True, [true, true]),
            (Default::False, [false, false]),
        ];
        for (default, expected) in table {
            let p = Permission::new("x", default);
            assert_eq!([false, true].map(|op| p.granted_by_default(op)), expected);
        }
    }

    // api: Permission::getName, DefaultPermissionNames, Permission
    #[test]
    fn builtin_names_are_unique_and_start_with_anchor() {
        let mut names: Vec<&str> = BUILTIN.iter().map(|p| p.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), BUILTIN.len());
        assert!(names.iter().all(|n| n.starts_with("anchor.")));
        assert_eq!(COMMAND_GIVE_SELF.name(), "anchor.command.give.self");
        assert_eq!(COMMAND_ABOUT.default(), Default::True);
    }

    // api: BanEntry::getName, BanEntry::getReason, BanEntry::setReason, BanEntry::getSource,
    // api: BanEntry::setSource, BanEntry::getCreated, BanEntry::getExpires,
    // api: BanEntry::setExpires, BanEntry::hasExpired, BanEntry
    #[test]
    fn ban_entries_keep_their_fields_and_expire_at_their_tick() {
        let entry = BanEntry::new("Steve")
            .with_reason("griefing")
            .with_source("console")
            .with_expires(100)
            .checked(40)
            .unwrap();
        assert_eq!(entry.name(), "steve");
        assert_eq!(
            (entry.reason(), entry.source(), entry.created()),
            ("griefing", "console", 40)
        );
        assert_eq!(entry.expires(), Some(100));
        assert!(!entry.has_expired(99) && entry.has_expired(100));
        assert_eq!(BanEntry::new("x").source(), "(Unknown)");
        let ip = BanEntry::ip(IpAddr::from([10, 0, 0, 1]));
        assert_eq!(
            (ip.name(), ip.ip_address()),
            ("10.0.0.1", Some(IpAddr::from([10, 0, 0, 1])))
        );
        assert_eq!(BanEntry::new("").checked(0), Err(Error::InvalidName));
        assert_eq!(BanEntry::new("a\nb").checked(0), Err(Error::InvalidName));
        let long = "x".repeat(MAX_REASON + 1);
        assert_eq!(
            BanEntry::new("a").with_reason(&long).checked(0),
            Err(Error::TooLong)
        );
    }

    #[test]
    fn a_ban_that_has_ended_already_is_refused() {
        let ban = |end: u64| BanEntry::new("steve").with_expires(end).checked(40);
        assert_eq!(ban(39), Err(Error::Expired));
        assert_eq!(ban(40), Err(Error::Expired));
        assert_eq!(ban(41).map(|entry| entry.expires()), Ok(Some(41)));
        assert!(BanEntry::new("steve").checked(40).is_ok());
    }

    #[test]
    fn an_ip_ban_of_an_ipv4_address_written_as_ipv6_bans_the_ipv4_address() {
        let mapped: IpAddr = "::ffff:1.2.3.4".parse().unwrap();
        let ip = BanEntry::ip(mapped);
        assert_eq!(ip.ip_address(), Some(IpAddr::from([1, 2, 3, 4])));
        assert_eq!(ip.name(), "1.2.3.4");
        let v6: IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(BanEntry::ip(v6).ip_address(), Some(v6));
    }
}
