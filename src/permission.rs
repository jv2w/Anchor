//! Permissions: what a player may do, with PMMP's defaults (op, not op, everyone, nobody)
//! (PLAN.md 10.8).
//!
//! A [`Permission`] is a name and a [`Default`](enum@Default): who has it unless the server says otherwise.
//! Register yours with [`ServerBuilder::permission`](crate::ServerBuilder::permission); a
//! command's permission is registered with the command. Whether someone has a permission comes
//! from its default and the operator list, unless a grant made while the player is online
//! ([`Context::set_permission`](crate::Context::set_permission), PMMP's permission attachments)
//! says otherwise. Grants are not saved; the operator list, the ban lists and the whitelist are
//! saved in the server file with everything else.
//!
//! Anchor's own permissions are the constants of this module. Their names follow PMMP's
//! `DefaultPermissionNames` with `anchor` in place of `pocketmine`: `pocketmine.command.give.self`
//! is `anchor.command.give.self`.

use std::fmt;
use std::net::IpAddr;

/// The longest name the operator list, the ban list and the whitelist hold, in bytes.
pub(crate) const MAX_NAME: usize = 64;
/// The longest ban reason, in bytes.
pub(crate) const MAX_REASON: usize = 256;
/// The longest ban source, in bytes.
pub(crate) const MAX_SOURCE: usize = 64;
/// The most entries one list holds (operators, name bans, IP bans, whitelist).
pub(crate) const MAX_ENTRIES: usize = 262_144;
/// The source of a ban nobody named (PMMP `BanEntry`'s default).
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
// pmmp-class: Permission
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

    /// The name, such as `anchor.command.give.self`.
    #[doc(alias = "Permission::getName")]
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// Who has the permission unless a grant says otherwise.
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
}

/// Who has a permission by default (PMMP's `PermissionParser` defaults).
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
    /// The default a PMMP permission file names: `op`, `notop`, `true`, `false` and PMMP's other
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
/// game state never reads the wall clock (CLAUDE.md 2.1), so a ban of 72,000 ticks lasts an hour
/// of server running time, and the time the server is down does not count.
#[derive(Debug, Clone, PartialEq, Eq)]
// pmmp-class: BanEntry
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

    /// A ban of the IP address `ip`, without reason or end.
    pub fn ip(ip: IpAddr) -> Self {
        Self {
            name: ip.to_string(),
            ip: Some(ip),
            ..Self::new("")
        }
    }

    /// The ban with `reason`, which a banned player sees when refused.
    #[doc(alias = "BanEntry::setReason")]
    #[must_use]
    pub fn with_reason(mut self, reason: &str) -> Self {
        reason.clone_into(&mut self.reason);
        self
    }

    /// The ban with `source`: who or what banned (`(Unknown)` by default).
    #[doc(alias = "BanEntry::setSource")]
    #[must_use]
    pub fn with_source(mut self, source: &str) -> Self {
        source.clone_into(&mut self.source);
        self
    }

    /// The ban ending at engine tick `tick`; a ban without an end lasts until it is lifted.
    #[doc(alias = "BanEntry::setExpires")]
    #[must_use]
    pub fn with_expires(mut self, tick: u64) -> Self {
        self.expires = Some(tick);
        self
    }

    /// The banned name in ASCII lowercase, or the banned IP address as text.
    #[doc(alias = "BanEntry::getName")]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The banned IP address, for an IP ban.
    pub fn ip_address(&self) -> Option<IpAddr> {
        self.ip
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

    /// The entry as the ban list keeps it, added at `tick`; fails if a field is over its bound or
    /// the name is not a valid list name.
    pub(crate) fn checked(mut self, tick: u64) -> Result<Self> {
        if self.ip.is_none() {
            check_name(&self.name)?;
        }
        if self.reason.len() > MAX_REASON || self.source.len() > MAX_SOURCE {
            return Err(Error::TooLong);
        }
        self.created = tick;
        Ok(self)
    }

    /// An entry as the server file stores it.
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
}

/// Fails unless `name` can be on a list: 1 to [`MAX_NAME`] bytes without control characters.
pub(crate) fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(Error::InvalidName);
    }
    Ok(())
}

/// Why a list did not change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The name is empty, longer than 64 bytes or holds control characters.
    InvalidName,
    /// The ban reason is longer than 256 bytes or its source longer than 64.
    TooLong,
    /// The list holds 262,144 entries.
    Full,
}

/// A result with the permission [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidName => "the name is empty, too long or holds control characters",
            Self::TooLong => "the ban reason or source is too long",
            Self::Full => "the list is full",
        })
    }
}

impl std::error::Error for Error {}

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
    /// Get a copy of what operators' commands did, such as `[Steve: Opped Alex]` (PMMP's
    /// administrative broadcast channel).
    #[doc(alias = "DefaultPermissionNames")]
    BROADCAST_ADMIN = "anchor.broadcast.admin", Op;
    /// Back up the world while the server runs (`/backup`; Anchor's own, PMMP has none).
    COMMAND_BACKUP = "anchor.command.backup", Op;
    /// Check the server file while the server runs (`/check`; Anchor's own, PMMP has none).
    COMMAND_CHECK = "anchor.command.check", Op;
    /// Ban IP addresses (`/ban-ip`).
    COMMAND_BAN_IP = "anchor.command.ban.ip", Op;
    /// List the bans (`/banlist`).
    COMMAND_BAN_LIST = "anchor.command.ban.list", Op;
    /// Ban players (`/ban`).
    COMMAND_BAN_PLAYER = "anchor.command.ban.player", Op;
    /// Clear another player's inventory (`/clear`).
    COMMAND_CLEAR_OTHER = "anchor.command.clear.other", Op;
    /// Clear one's own inventory (`/clear`).
    COMMAND_CLEAR_SELF = "anchor.command.clear.self", True;
    /// Change the default game mode (`/defaultgamemode`).
    COMMAND_DEFAULTGAMEMODE = "anchor.command.defaultgamemode", Op;
    /// Change the difficulty (`/difficulty`).
    COMMAND_DIFFICULTY = "anchor.command.difficulty", Op;
    /// Change another player's effects (`/effect`).
    COMMAND_EFFECT_OTHER = "anchor.command.effect.other", Op;
    /// Change one's own effects (`/effect`).
    COMMAND_EFFECT_SELF = "anchor.command.effect.self", Op;
    /// Enchant another player's held item (`/enchant`).
    COMMAND_ENCHANT_OTHER = "anchor.command.enchant.other", Op;
    /// Enchant one's own held item (`/enchant`).
    COMMAND_ENCHANT_SELF = "anchor.command.enchant.self", Op;
    /// Change another player's game mode (`/gamemode`).
    COMMAND_GAMEMODE_OTHER = "anchor.command.gamemode.other", Op;
    /// Change one's own game mode (`/gamemode`).
    COMMAND_GAMEMODE_SELF = "anchor.command.gamemode.self", Op;
    /// Give items to another player (`/give`).
    COMMAND_GIVE_OTHER = "anchor.command.give.other", Op;
    /// Give items to oneself (`/give`).
    COMMAND_GIVE_SELF = "anchor.command.give.self", Op;
    /// List the commands (`/help`).
    COMMAND_HELP = "anchor.command.help", True;
    /// Kick players (`/kick`).
    COMMAND_KICK = "anchor.command.kick", Op;
    /// Kill another player (`/kill`).
    COMMAND_KILL_OTHER = "anchor.command.kill.other", Op;
    /// Kill oneself (`/kill`).
    COMMAND_KILL_SELF = "anchor.command.kill.self", True;
    /// List the players online (`/list`).
    COMMAND_LIST = "anchor.command.list", Op;
    /// Describe an action in chat (`/me`).
    COMMAND_ME = "anchor.command.me", True;
    /// Make players operators (`/op`).
    COMMAND_OP_GIVE = "anchor.command.op.give", Op;
    /// Take operator status away (`/deop`).
    COMMAND_OP_TAKE = "anchor.command.op.take", Op;
    /// Show particles (`/particle`).
    COMMAND_PARTICLE = "anchor.command.particle", Op;
    /// Turn periodic saving off (`/save-off`).
    COMMAND_SAVE_DISABLE = "anchor.command.save.disable", Op;
    /// Turn periodic saving on (`/save-on`).
    COMMAND_SAVE_ENABLE = "anchor.command.save.enable", Op;
    /// Save now (`/save-all`).
    COMMAND_SAVE_PERFORM = "anchor.command.save.perform", Op;
    /// Broadcast a message (`/say`).
    COMMAND_SAY = "anchor.command.say", Op;
    /// Show the world seed (`/seed`).
    COMMAND_SEED = "anchor.command.seed", Op;
    /// Move the world spawn (`/setworldspawn`).
    COMMAND_SETWORLDSPAWN = "anchor.command.setworldspawn", Op;
    /// Set another player's spawn point (`/spawnpoint`).
    COMMAND_SPAWNPOINT_OTHER = "anchor.command.spawnpoint.other", Op;
    /// Set one's own spawn point (`/spawnpoint`).
    COMMAND_SPAWNPOINT_SELF = "anchor.command.spawnpoint.self", Op;
    /// Show the server's uptime and load (`/status`).
    COMMAND_STATUS = "anchor.command.status", Op;
    /// Stop the server (`/stop`).
    COMMAND_STOP = "anchor.command.stop", Op;
    /// Teleport other players (`/tp`).
    COMMAND_TELEPORT_OTHER = "anchor.command.teleport.other", Op;
    /// Teleport oneself (`/tp`).
    COMMAND_TELEPORT_SELF = "anchor.command.teleport.self", Op;
    /// Send a private message (`/tell`).
    COMMAND_TELL = "anchor.command.tell", True;
    /// Add to the world time (`/time add`).
    COMMAND_TIME_ADD = "anchor.command.time.add", Op;
    /// Read the world time (`/time query`).
    COMMAND_TIME_QUERY = "anchor.command.time.query", Op;
    /// Set the world time (`/time set`).
    COMMAND_TIME_SET = "anchor.command.time.set", Op;
    /// Start the day cycle (`/time start`).
    COMMAND_TIME_START = "anchor.command.time.start", Op;
    /// Stop the day cycle (`/time stop`).
    COMMAND_TIME_STOP = "anchor.command.time.stop", Op;
    /// Show titles to other players (`/title`).
    COMMAND_TITLE_OTHER = "anchor.command.title.other", Op;
    /// Show titles to oneself (`/title`).
    COMMAND_TITLE_SELF = "anchor.command.title.self", Op;
    /// Send players to another server (`/transferserver`).
    COMMAND_TRANSFERSERVER = "anchor.command.transferserver", Op;
    /// Lift IP bans (`/pardon-ip`).
    COMMAND_UNBAN_IP = "anchor.command.unban.ip", Op;
    /// Lift player bans (`/pardon`).
    COMMAND_UNBAN_PLAYER = "anchor.command.unban.player", Op;
    /// Show the server version (`/version`).
    COMMAND_VERSION = "anchor.command.version", True;
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
    /// Give experience to another player (`/xp`).
    COMMAND_XP_OTHER = "anchor.command.xp.other", Op;
    /// Give experience to oneself (`/xp`).
    COMMAND_XP_SELF = "anchor.command.xp.self", Op;
}

#[cfg(test)]
mod tests {
    use super::*;

    // pmmp: PermissionParser::defaultFromString, PermissionParser, Permission::__construct
    #[test]
    fn defaults_parse_pmmp_spellings_and_decide_by_operator_status() {
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

    // pmmp: Permission::getName, DefaultPermissionNames, Permission
    #[test]
    fn builtin_names_are_unique_and_follow_pmmp_under_anchor() {
        let mut names: Vec<&str> = BUILTIN.iter().map(|p| p.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), BUILTIN.len());
        assert!(names.iter().all(|n| n.starts_with("anchor.")));
        assert_eq!(COMMAND_GIVE_SELF.name(), "anchor.command.give.self");
        assert_eq!(COMMAND_ME.default(), Default::True);
    }

    // pmmp: BanEntry::getName, BanEntry::getReason, BanEntry::setReason, BanEntry::getSource, BanEntry::setSource, BanEntry::getCreated, BanEntry::getExpires, BanEntry::setExpires, BanEntry::hasExpired, BanEntry
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
}
