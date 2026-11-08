//! The operator list, the bans, the whitelist and kicks: `/op`, `/deop`, `/ban`, `/ban-ip`,
//! `/banlist`, `/unban`, `/unban-ip`, `/whitelist` and `/kick`.

use std::net::IpAddr;

use super::{
    Msg, announce, has_room, infallible, kick, name_of, player_name, require, tell, usage,
};
use crate::command::{Args, Command, Error, Output, Param, Result, Sender};
use crate::permission::{self, BanEntry};
use crate::tick::Context;
use crate::tick::command::Refusal;

/// The success line of `/ban`.
const BAN_SUCCESS: Msg = Msg::client("commands.ban.success");
/// The failure line of `/ban-ip` for a value that is no address and no online player.
const BANIP_INVALID: Msg = Msg::client("commands.banip.invalid");
/// The success line of `/ban-ip` for an address.
const BANIP_SUCCESS: Msg = Msg::client("commands.banip.success");
/// The success line of `/ban-ip` for the address of an online player.
const BANIP_SUCCESS_PLAYERS: Msg = Msg::client("commands.banip.success.players");
/// The first line of `/banlist ips`.
const BANLIST_IPS: Msg = Msg::client("commands.banlist.ips");
/// The first line of `/banlist players`.
const BANLIST_PLAYERS: Msg = Msg::client("commands.banlist.players");
/// What a player sees when it loses operator status.
const DEOP_MESSAGE: Msg = Msg::client("commands.deop.message");
/// The success line of `/deop`.
const DEOP_SUCCESS: Msg = Msg::client("commands.deop.success");
/// The success line of `/kick` without a reason.
const KICK_SUCCESS: Msg = Msg::client("commands.kick.success");
/// The success line of `/kick` with a reason.
const KICK_SUCCESS_REASON: Msg = Msg::client("commands.kick.success.reason");
/// What a player kicked without a reason sees: the client's "kicked" text.
const KICKED: &str = "disconnect.kicked";
/// The most names `/banlist` and `/whitelist list` show (a list may hold 262,144); the others
/// are counted in a last line.
const MAX_LISTED: usize = 100;
/// What an operator sees when its status changes: aqua.
const OP_CHANGE_PREFIX: &str = "§b";
/// What a player sees when it becomes an operator.
const OP_MESSAGE: Msg = Msg::client("commands.op.message");
/// The success line of `/op`.
const OP_SUCCESS: Msg = Msg::client("commands.op.success");
/// The success line of `/unban`.
const UNBAN_SUCCESS: Msg = Msg::client("commands.unban.success");
/// The failure line of `/unban-ip` for a value that is no address.
const UNBANIP_INVALID: Msg = Msg::client("commands.unbanip.invalid");
/// The success line of `/unban-ip`.
const UNBANIP_SUCCESS: Msg = Msg::client("commands.unbanip.success");
// The client names the whitelist "allowlist": it has no `commands.whitelist.*` keys.
/// The success line of `/whitelist add`.
const WHITELIST_ADD_SUCCESS: Msg = Msg::client("commands.allowlist.add.success");
/// The success line of `/whitelist off`.
const WHITELIST_DISABLED: Msg = Msg::client("commands.allowlist.disabled");
/// The success line of `/whitelist on`.
const WHITELIST_ENABLED: Msg = Msg::client("commands.allowlist.enabled");
/// The first line of `/whitelist list`.
const WHITELIST_LIST: Msg = Msg::client("commands.allowlist.list");
/// The success line of `/whitelist remove`.
const WHITELIST_REMOVE_SUCCESS: Msg = Msg::client("commands.allowlist.remove.success");

/// Adds the commands of this module.
pub(super) fn add(commands: &mut Vec<Command>) {
    commands.extend([
        Command::new("ban", "commands.ban.description")
            .permission(permission::COMMAND_BAN_PLAYER)
            .overload(
                &[
                    Param::player_name("name"),
                    Param::message("reason").optional(),
                ],
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
        Command::new("unban", "Lifts the ban on a name")
            .permission(permission::COMMAND_UNBAN_PLAYER)
            .overload(&[Param::player_name("name")], unban),
        Command::new("unban-ip", "Lifts the ban on an IP address")
            .permission(permission::COMMAND_UNBAN_IP)
            .overload(&[Param::string("address")], unban_ip),
        Command::new("op", "commands.op.description")
            .permission(permission::COMMAND_OP_GIVE)
            .overload(&[Param::player_name("player")], op),
        Command::new("deop", "commands.deop.description")
            .permission(permission::COMMAND_OP_TAKE)
            .overload(&[Param::player_name("player")], deop),
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
                &[Param::target("name"), Param::message("reason").optional()],
                kick_command,
            ),
    ]);
}

/// Bans the name, with the sender as the source, and kicks the player of that name if online
/// (`/ban <name> [reason]`).
fn ban(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let Some(name) = args.string("name") else {
        return Err(usage(cx, "ban"));
    };
    let reason = args.string("reason").unwrap_or_default();
    let online = cx.player_by_name(name);
    has_room(cx, usize::from(online.is_some()))?;
    let source = name_of(cx, *sender);
    cx.ban(BanEntry::new(name).with_reason(reason).with_source(&source))
        .map_err(list_error)?;
    let shown = match online {
        Some(id) => {
            kick(cx, id, &Refusal::Banned(reason.to_owned()).to_message())?;
            player_name(cx, id)
        }
        None => name.to_owned(),
    };
    Ok(announce(cx, *sender, BAN_SUCCESS, &[&shown]))
}

/// Bans the address, or the address of the online player of that name, and kicks every player
/// at it (`/ban-ip <address|player> [reason]`).
fn ban_ip(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let Some(value) = args.string("address") else {
        return Err(usage(cx, "ban-ip"));
    };
    let reason = args.string("reason").unwrap_or_default();
    let (ip, owner) = if let Some(ip) = parse_ip(value) {
        (ip, None)
    } else {
        let address = cx.player_by_name(value).and_then(|id| {
            let p = cx.player(id)?;
            Some((p.identity.address?, p.identity.name.clone()))
        });
        let Some((ip, name)) = address else {
            return Err(BANIP_INVALID.error(&[]));
        };
        (ip, Some(name))
    };
    let at_address: Vec<_> = cx
        .players()
        .filter(|p| p.identity.address == Some(ip))
        .map(crate::player::Player::id)
        .collect();
    has_room(cx, at_address.len())?;
    let source = name_of(cx, *sender);
    cx.ban(BanEntry::ip(ip).with_reason(reason).with_source(&source))
        .map_err(list_error)?;
    let message = Refusal::IpBanned(reason.to_owned()).to_message();
    for id in at_address {
        kick(cx, id, &message)?;
    }
    let ip = ip.to_string();
    Ok(match owner {
        Some(name) => announce(cx, *sender, BANIP_SUCCESS_PLAYERS, &[&ip, &name]),
        None => announce(cx, *sender, BANIP_SUCCESS, &[&ip]),
    })
}

/// Shows how many bans are in force and the banned names in text order or the banned addresses
/// in address order, at most [`MAX_LISTED`] of them (`/banlist [ips|players]`).
fn banlist(cx: &mut Context<'_>, _: &Sender, args: &Args) -> Output {
    if args.string("type") == Some("ips") {
        let count = cx.ip_bans().count();
        listed(
            BANLIST_IPS.output(&[&count.to_string()]),
            count,
            cx.ip_bans().map(BanEntry::name),
        )
    } else {
        let count = cx.bans().count();
        listed(
            BANLIST_PLAYERS.output(&[&count.to_string()]),
            count,
            cx.bans().map(BanEntry::name),
        )
    }
}

/// Lifts the ban of the name (`/unban <name>`).
fn unban(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let Some(name) = args.string("name") else {
        return Err(usage(cx, "unban"));
    };
    cx.unban(name);
    Ok(announce(cx, *sender, UNBAN_SUCCESS, &[name]))
}

/// Lifts the ban of the address (`/unban-ip <address>`).
fn unban_ip(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let Some(value) = args.string("address") else {
        return Err(usage(cx, "unban-ip"));
    };
    let Some(ip) = parse_ip(value) else {
        return Err(UNBANIP_INVALID.error(&[]));
    };
    cx.unban_ip(ip);
    Ok(announce(cx, *sender, UNBANIP_SUCCESS, &[value]))
}

/// Reads an IP address the way a connection's is: an IPv4 address written as IPv6
/// (`::ffff:1.2.3.4`) is the IPv4 address.
fn parse_ip(text: &str) -> Option<IpAddr> {
    text.parse::<IpAddr>().ok().map(|ip| ip.to_canonical())
}

/// Makes the name an operator and tells the player if online (`/op <player>`).
fn op(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let name = listed_name(cx, args, "op")?;
    cx.op(name).map_err(list_error)?;
    if let Some(id) = cx.player_by_name(name) {
        tell(cx, id, OP_MESSAGE.chat(OP_CHANGE_PREFIX, &[]));
    }
    Ok(announce(cx, *sender, OP_SUCCESS, &[name]))
}

/// Takes operator status from the name and tells the player if online (`/deop <player>`).
fn deop(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let name = listed_name(cx, args, "deop")?;
    cx.deop(name);
    if let Some(id) = cx.player_by_name(name) {
        tell(cx, id, DEOP_MESSAGE.chat(OP_CHANGE_PREFIX, &[]));
    }
    Ok(announce(cx, *sender, DEOP_SUCCESS, &[name]))
}

/// Turns the whitelist on or off, lists it, or adds or removes a name (`/whitelist
/// on|off|list` and `/whitelist add|remove <player>`). Turning it on
/// kicks the online players it leaves out; removing a player kicks it unless it is an operator.
/// Each action has its own permission. The whitelist is kept in the server file, so there is no
/// `reload`. `on` and `off` last until the server stops
/// (`white-list` decides at start). `list` shows at most [`MAX_LISTED`] names.
fn whitelist(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let needed = match args.string("action") {
        Some("on") => permission::COMMAND_WHITELIST_ENABLE,
        Some("off") => permission::COMMAND_WHITELIST_DISABLE,
        Some("list") => permission::COMMAND_WHITELIST_LIST,
        Some("add") => permission::COMMAND_WHITELIST_ADD,
        Some("remove") => permission::COMMAND_WHITELIST_REMOVE,
        _ => return Err(usage(cx, "whitelist")),
    };
    require(cx, *sender, needed, "whitelist")?;
    match args.string("action") {
        Some("on") => {
            // Every online player may be left out.
            has_room(cx, cx.player_count())?;
            cx.set_whitelist_enabled(true);
            let left_out: Vec<_> = cx
                .players()
                .filter(|p| !cx.is_whitelisted(p.name()))
                .map(crate::player::Player::id)
                .collect();
            let message = Refusal::NotWhitelisted.to_message();
            for id in left_out {
                kick(cx, id, &message)?;
            }
            Ok(announce(cx, *sender, WHITELIST_ENABLED, &[]))
        }
        Some("off") => {
            cx.set_whitelist_enabled(false);
            Ok(announce(cx, *sender, WHITELIST_DISABLED, &[]))
        }
        Some("list") => {
            let count = cx.state.commands.access.whitelisted().len();
            let shown = count.to_string();
            Ok(listed(
                WHITELIST_LIST.output(&[&shown, &shown]),
                count,
                cx.whitelist(),
            ))
        }
        Some("add") => {
            let name = listed_name(cx, args, "whitelist")?;
            cx.add_to_whitelist(name).map_err(list_error)?;
            Ok(announce(cx, *sender, WHITELIST_ADD_SUCCESS, &[name]))
        }
        _ => {
            let name = listed_name(cx, args, "whitelist")?;
            has_room(cx, 1)?;
            cx.remove_from_whitelist(name);
            if !cx.is_whitelisted(name)
                && let Some(id) = cx.player_by_name(name)
            {
                kick(cx, id, &Refusal::NotWhitelisted.to_message())?;
            }
            Ok(announce(cx, *sender, WHITELIST_REMOVE_SUCCESS, &[name]))
        }
    }
}

/// Turns a refused list change (a name over the list's bounds, a full list) into a failure.
fn list_error(e: permission::Error) -> Error {
    Error::failed(e.to_string())
}

/// Adds to `out` the line of `names`, `count` in all (the first [`MAX_LISTED`], comma
/// separated) and, when there are more, a line telling how many more.
fn listed<'a>(out: Output, count: usize, names: impl Iterator<Item = &'a str>) -> Output {
    let mut line = String::new();
    for (i, name) in names.take(MAX_LISTED).enumerate() {
        if i > 0 {
            line.push_str(", ");
        }
        line.push_str(name);
    }
    let out = out.line(line);
    let more = count.saturating_sub(MAX_LISTED);
    if more > 0 {
        out.line(format!("and {more} more"))
    } else {
        out
    }
}

/// Reads the name argument of `/op`, `/deop` and `/whitelist`, if it can be a player's name (a
/// usage failure otherwise).
fn listed_name<'a>(cx: &Context<'_>, args: &'a Args, command: &str) -> Result<&'a str> {
    match args.string("player") {
        Some(name) if crate::login::check_user_name(name).is_ok() => Ok(name),
        _ => Err(usage(cx, command)),
    }
}

/// Disconnects the players, showing the reason as it is, or the client's "kicked" text without
/// one (`/kick <player> [reason]`). The success line comes before the
/// [`PlayerKick`](crate::event::PlayerKick) listeners decide.
fn kick_command(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let reason = args.string("reason").unwrap_or_default().trim();
    let text = if reason.is_empty() { KICKED } else { reason };
    let targets = args.targets("name").unwrap_or_default();
    has_room(cx, targets.len())?;
    let mut out = Output::new();
    for &id in targets {
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
