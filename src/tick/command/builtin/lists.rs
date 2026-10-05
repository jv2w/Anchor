//! The operator list, the bans, the whitelist and kicks: `/op`, `/deop`, `/ban`, `/ban-ip`,
//! `/banlist`, `/pardon`, `/pardon-ip`, `/whitelist` and `/kick` (PMMP's classes of the same
//! names).

use std::net::IpAddr;

use super::super::Refusal;
use super::{Msg, announce, infallible, kick, name_of, player_name, tell, usage};
use crate::command::{Args, Command, Error, Output, Param, Result, Sender};
use crate::permission::{self, BanEntry};
use crate::tick::Context;

/// Adds the commands of this module.
pub(super) fn add(commands: &mut Vec<Command>) {
    commands.extend([
        Command::new("ban", "commands.ban.description")
            .permission(permission::COMMAND_BAN_PLAYER)
            .overload(
                &[Param::string("name"), Param::message("reason").optional()],
                ban,
            ),
        Command::new("ban-ip", "commands.banip.description")
            .permission(permission::COMMAND_BAN_IP)
            .overload(
                &[
                    Param::string("address"),
                    Param::message("reason").optional(),
                ],
                ban_ip,
            ),
        Command::new("banlist", "Shows the banned names and addresses")
            .permission(permission::COMMAND_BAN_LIST)
            .overload(
                &[Param::enumeration("type", "BanListType", &["ips", "players"]).optional()],
                infallible(banlist),
            ),
        Command::new("pardon", "Lifts the ban on a name")
            .alias("unban")
            .permission(permission::COMMAND_UNBAN_PLAYER)
            .overload(&[Param::string("name")], infallible(pardon)),
        Command::new("pardon-ip", "Lifts the ban on an IP address")
            .alias("unban-ip")
            .permission(permission::COMMAND_UNBAN_IP)
            .overload(&[Param::string("address")], pardon_ip),
        Command::new("op", "commands.op.description")
            .permission(permission::COMMAND_OP_GIVE)
            .overload(&[Param::string("player")], op),
        Command::new("deop", "commands.deop.description")
            .permission(permission::COMMAND_OP_TAKE)
            .overload(&[Param::string("player")], deop),
        Command::new("whitelist", "commands.allowlist.description")
            .permission(permission::COMMAND_WHITELIST_ENABLE)
            .permission(permission::COMMAND_WHITELIST_DISABLE)
            .permission(permission::COMMAND_WHITELIST_LIST)
            .permission(permission::COMMAND_WHITELIST_ADD)
            .permission(permission::COMMAND_WHITELIST_REMOVE)
            .overload(
                &[Param::enumeration(
                    "action",
                    "WhitelistAction",
                    &["on", "off", "list"],
                )],
                whitelist,
            )
            .overload(
                &[
                    Param::enumeration("action", "WhitelistEdit", &["add", "remove"]),
                    Param::string("player"),
                ],
                whitelist,
            ),
        Command::new("kick", "commands.kick.description")
            .permission(permission::COMMAND_KICK)
            .overload(
                &[Param::target("player"), Param::message("reason").optional()],
                kick_command,
            ),
    ]);
}

const BAN_SUCCESS: Msg = Msg::client("commands.ban.success");
const BANIP_INVALID: Msg = Msg::client("commands.banip.invalid");
const BANIP_SUCCESS: Msg = Msg::client("commands.banip.success");
const BANIP_SUCCESS_PLAYERS: Msg = Msg::client("commands.banip.success.players");
const BANLIST_IPS: Msg = Msg::client("commands.banlist.ips");
const BANLIST_PLAYERS: Msg = Msg::client("commands.banlist.players");
const UNBAN_SUCCESS: Msg = Msg::client("commands.unban.success");
const UNBANIP_INVALID: Msg = Msg::client("commands.unbanip.invalid");
const UNBANIP_SUCCESS: Msg = Msg::client("commands.unbanip.success");
const OP_MESSAGE: Msg = Msg::client("commands.op.message");
const OP_SUCCESS: Msg = Msg::client("commands.op.success");
const DEOP_MESSAGE: Msg = Msg::client("commands.deop.message");
const DEOP_SUCCESS: Msg = Msg::client("commands.deop.success");
// The client names the whitelist "allowlist": it has no `commands.whitelist.*` keys.
const WHITELIST_ENABLED: Msg = Msg::client("commands.allowlist.enabled");
const WHITELIST_DISABLED: Msg = Msg::client("commands.allowlist.disabled");
const WHITELIST_LIST: Msg = Msg::client("commands.allowlist.list");
const WHITELIST_ADD_SUCCESS: Msg = Msg::client("commands.allowlist.add.success");
const WHITELIST_REMOVE_SUCCESS: Msg = Msg::client("commands.allowlist.remove.success");
const KICK_SUCCESS: Msg = Msg::client("commands.kick.success");
const KICK_SUCCESS_REASON: Msg = Msg::client("commands.kick.success.reason");
/// What an operator sees when its status changes: aqua.
const OP_CHANGE_PREFIX: &str = "§b";
/// What a player kicked without a reason sees: the client's "kicked" text.
const KICKED: &str = "disconnect.kicked";
/// The most names `/banlist` and `/whitelist list` show (PMMP shows every one, and a list may
/// hold 262,144); the others are counted in a last line.
const MAX_LISTED: usize = 100;

/// A list change refused (a name over the list's bounds, a full list), as a failure.
fn list_error(e: permission::Error) -> Error {
    Error::failed(e.to_string())
}

/// `/ban <name> [reason]`: bans the name, with the sender as the source, and kicks the player of
/// that name if online (PMMP `BanCommand`).
fn ban(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let name = args.string("name").unwrap_or_default();
    let reason = args.string("reason").unwrap_or_default();
    let source = name_of(cx, *sender);
    cx.ban(BanEntry::new(name).with_reason(reason).with_source(&source))
        .map_err(list_error)?;
    let shown = match cx.player_exact(name) {
        Some(id) => {
            kick(cx, id, &Refusal::Banned(reason.to_owned()).message())?;
            player_name(cx, id)
        }
        None => name.to_owned(),
    };
    Ok(announce(cx, *sender, BAN_SUCCESS, &[&shown]))
}

/// `/ban-ip <address|player> [reason]`: bans the address, or the address of the online player of
/// that name, and kicks every player at it (PMMP `BanIpCommand`; PMMP takes a name prefix).
fn ban_ip(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let value = args.string("address").unwrap_or_default();
    let reason = args.string("reason").unwrap_or_default();
    let (ip, owner) = if let Ok(ip) = value.parse::<IpAddr>() {
        (ip, None)
    } else {
        let address = cx.player_exact(value).and_then(|id| {
            let p = cx.state.players.get(&id.session())?;
            Some((p.identity.address?, p.identity.name.clone()))
        });
        let Some((ip, name)) = address else {
            return Err(BANIP_INVALID.error(&[]));
        };
        (ip, Some(name))
    };
    let source = name_of(cx, *sender);
    cx.ban(BanEntry::ip(ip).with_reason(reason).with_source(&source))
        .map_err(list_error)?;
    let message = Refusal::IpBanned(reason.to_owned()).message();
    let at_address: Vec<_> = cx
        .players()
        .filter(|p| p.identity.address == Some(ip))
        .map(crate::player::Player::id)
        .collect();
    for id in at_address {
        kick(cx, id, &message)?;
    }
    let ip = ip.to_string();
    Ok(match owner {
        Some(name) => announce(cx, *sender, BANIP_SUCCESS_PLAYERS, &[&ip, &name]),
        None => announce(cx, *sender, BANIP_SUCCESS, &[&ip]),
    })
}

/// `out` with the line of `names` (the first [`MAX_LISTED`], comma separated) and, when there
/// are more, a line telling how many more.
fn listed<'a>(out: Output, names: impl Iterator<Item = &'a str>) -> Output {
    let mut line = String::new();
    let mut more = 0_usize;
    for (i, name) in names.enumerate() {
        if i >= MAX_LISTED {
            more += 1;
            continue;
        }
        if i > 0 {
            line.push_str(", ");
        }
        line.push_str(name);
    }
    let out = out.line(line);
    if more > 0 {
        out.line(format!("and {more} more"))
    } else {
        out
    }
}

/// `/banlist [ips|players]`: how many bans are in force and the banned names in text order or
/// the banned addresses in address order (PMMP `BanListCommand`, which sorts the addresses as
/// text: address order needs no sort), at most [`MAX_LISTED`] of them.
fn banlist(cx: &mut Context<'_>, _: &Sender, args: &Args) -> Output {
    if args.string("type") == Some("ips") {
        let count = cx.ip_bans().count().to_string();
        listed(
            BANLIST_IPS.output(&[&count]),
            cx.ip_bans().map(BanEntry::name),
        )
    } else {
        let count = cx.bans().count().to_string();
        listed(
            BANLIST_PLAYERS.output(&[&count]),
            cx.bans().map(BanEntry::name),
        )
    }
}

/// `/pardon <name>`: lifts the ban of the name (PMMP `PardonCommand`).
fn pardon(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Output {
    let name = args.string("name").unwrap_or_default();
    cx.pardon(name);
    announce(cx, *sender, UNBAN_SUCCESS, &[name])
}

/// `/pardon-ip <address>`: lifts the ban of the address (PMMP `PardonIpCommand`).
fn pardon_ip(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let value = args.string("address").unwrap_or_default();
    let Ok(ip) = value.parse::<IpAddr>() else {
        return Err(UNBANIP_INVALID.error(&[]));
    };
    cx.pardon_ip(ip);
    Ok(announce(cx, *sender, UNBANIP_SUCCESS, &[value]))
}

/// The name argument of `/op`, `/deop` and `/whitelist`, if it can be a player's name (PMMP
/// `Player::isValidUserName`, a usage failure otherwise).
fn listed_name<'a>(cx: &Context<'_>, args: &'a Args, command: &str) -> Result<&'a str> {
    match args.string("player") {
        Some(name) if crate::login::check_user_name(name).is_ok() => Ok(name),
        _ => Err(usage(cx, command)),
    }
}

/// `/op <player>`: makes the name an operator and tells the player if online (PMMP
/// `OpCommand`).
fn op(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let name = listed_name(cx, args, "op")?;
    cx.op(name).map_err(list_error)?;
    if let Some(id) = cx.player_exact(name) {
        tell(cx, id, OP_MESSAGE.chat(OP_CHANGE_PREFIX, &[]));
    }
    Ok(announce(cx, *sender, OP_SUCCESS, &[name]))
}

/// `/deop <player>`: takes operator status from the name and tells the player if online (PMMP
/// `DeopCommand`).
fn deop(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let name = listed_name(cx, args, "deop")?;
    cx.deop(name);
    if let Some(id) = cx.player_exact(name) {
        tell(cx, id, DEOP_MESSAGE.chat(OP_CHANGE_PREFIX, &[]));
    }
    Ok(announce(cx, *sender, DEOP_SUCCESS, &[name]))
}

/// `/whitelist on|off|list` and `/whitelist add|remove <player>` (PMMP `WhitelistCommand`).
/// Turning it on kicks the online players it leaves out; removing a player kicks it unless it is
/// an operator. Each action has its own permission. PMMP's `reload` is left out: the whitelist
/// is kept in the world file, not reread from a text file. `on` and `off` last until the server
/// stops (`white-list` decides at start). `list` shows at most [`MAX_LISTED`] names.
fn whitelist(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let action = args.string("action").unwrap_or_default();
    let needed = match action {
        "on" => permission::COMMAND_WHITELIST_ENABLE,
        "off" => permission::COMMAND_WHITELIST_DISABLE,
        "list" => permission::COMMAND_WHITELIST_LIST,
        "add" => permission::COMMAND_WHITELIST_ADD,
        _ => permission::COMMAND_WHITELIST_REMOVE,
    };
    super::require(cx, *sender, needed, "whitelist")?;
    match action {
        "on" => {
            cx.set_whitelist_enabled(true);
            let left_out: Vec<_> = cx
                .players()
                .filter(|p| !cx.is_whitelisted(p.name()))
                .map(crate::player::Player::id)
                .collect();
            let message = Refusal::NotWhitelisted.message();
            for id in left_out {
                kick(cx, id, &message)?;
            }
            Ok(announce(cx, *sender, WHITELIST_ENABLED, &[]))
        }
        "off" => {
            cx.set_whitelist_enabled(false);
            Ok(announce(cx, *sender, WHITELIST_DISABLED, &[]))
        }
        "list" => {
            let count = cx.whitelist().count().to_string();
            Ok(listed(
                WHITELIST_LIST.output(&[&count, &count]),
                cx.whitelist(),
            ))
        }
        "add" => {
            let name = listed_name(cx, args, "whitelist")?;
            cx.add_to_whitelist(name).map_err(list_error)?;
            Ok(announce(cx, *sender, WHITELIST_ADD_SUCCESS, &[name]))
        }
        _ => {
            let name = listed_name(cx, args, "whitelist")?;
            cx.remove_from_whitelist(name);
            if !cx.is_whitelisted(name)
                && let Some(id) = cx.player_exact(name)
            {
                kick(cx, id, &Refusal::NotWhitelisted.message())?;
            }
            Ok(announce(cx, *sender, WHITELIST_REMOVE_SUCCESS, &[name]))
        }
    }
}

/// `/kick <player> [reason]` (PMMP `KickCommand`): disconnects the players showing the reason as
/// it is, as BDS's kick does, or the client's "kicked" text without one. The success line comes
/// before the [`PlayerKick`](crate::event::PlayerKick) listeners decide, as in PMMP.
fn kick_command(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let reason = args.string("reason").unwrap_or_default().trim();
    let text = if reason.is_empty() { KICKED } else { reason };
    let mut out = Output::new();
    for &id in args.targets("player").unwrap_or_default() {
        let name = player_name(cx, id);
        kick(cx, id, text)?;
        let line = if reason.is_empty() {
            announce(cx, *sender, KICK_SUCCESS, &[&name])
        } else {
            announce(cx, *sender, KICK_SUCCESS_REASON, &[&name, reason])
        };
        out.lines.extend(line.lines);
    }
    Ok(out)
}
