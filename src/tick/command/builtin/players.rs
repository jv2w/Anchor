//! Commands on players: `/clear`, `/gamemode`, `/give`, `/kill` and `/tp`.

use super::{
    Msg, Results, announce, bounded, game_mode_arg, game_mode_name, game_mode_param, has_room,
    in_world, perform, permitted_targets, player_name, sender_world, tell_object, two_decimals,
    usage,
};
use crate::action;
use crate::command::{Args, Command, Output, Param, Result, Sender};
use crate::item::Stack;
use crate::permission;
use crate::player::Player;
use crate::tick::{Context, inventory};

/// The failure line of `/clear` that found nothing to clear.
const CLEAR_FAILURE: Msg = Msg::client("commands.clear.failure.no.items");
/// The success line of `/clear`.
const CLEAR_SUCCESS: Msg = Msg::client("commands.clear.success");
/// The result line of `/clear` with a max count of 0.
const CLEAR_TESTING: Msg = Msg::client("commands.clear.testing");
/// What a player sees when its game mode changes.
const GAME_MODE_CHANGED: Msg = Msg::client("gameMode.changed");
/// The failure line of `/gamemode` for a player already in the mode.
const GAMEMODE_FAILURE: Msg = Msg::own("The game mode of {%0} stays as it is");
/// The success line of `/gamemode` for another player.
const GAMEMODE_SUCCESS_OTHER: Msg = Msg::client("commands.gamemode.success.other");
/// The success line of `/gamemode` for the sender.
const GAMEMODE_SUCCESS_SELF: Msg = Msg::client("commands.gamemode.success.self");
/// The success line of `/give`.
const GIVE_SUCCESS: Msg = Msg::client("commands.give.success");
/// What a player sees when `/give` gives it items.
const GIVE_SUCCESS_RECIPIENT: Msg = Msg::client("commands.give.successRecipient");
/// The failure line of `/kill` for a player that does not die.
const KILL_FAILURE: Msg = Msg::own("{%0} cannot be killed");
/// The success line of `/kill`.
const KILL_SUCCESSFUL: Msg = Msg::client("commands.kill.successful");
/// The longest `/give` amount.
const MAX_GIVE: i32 = 32_767;
/// The success line of `/tp` to a player.
const TP_SUCCESS: Msg = Msg::client("commands.tp.success");
/// The success line of `/tp` to coordinates.
const TP_SUCCESS_COORDINATES: Msg = Msg::client("commands.tp.success.coordinates");
/// What a player sees when `/tp` moves it.
const TP_SUCCESS_VICTIM: Msg = Msg::client("commands.tp.successVictim");

/// A slot `/clear` empties, in the order it empties them: the inventory, the cursor, the armor,
/// the off hand.
#[derive(Debug, Clone, Copy)]
enum Slot {
    Main(u8),
    Cursor,
    Armor(usize),
    Offhand,
}

/// Adds the commands of this module.
pub(super) fn add(commands: &mut Vec<Command>) {
    commands.extend([
        Command::new("clear", "commands.clear.description")
            .permission(permission::COMMAND_CLEAR)
            .overload(
                &[
                    Param::target("player").optional(),
                    Param::item("itemName").optional(),
                    Param::int("data").optional(),
                    Param::int("maxCount").optional(),
                ],
                clear,
            ),
        Command::new("gamemode", "commands.gamemode.description")
            .permission(permission::COMMAND_GAMEMODE_SELF)
            .permission(permission::COMMAND_GAMEMODE_OTHER)
            .overload(
                &[game_mode_param(), Param::target("player").optional()],
                gamemode,
            )
            .overload(
                &[Param::int("gameMode"), Param::target("player").optional()],
                gamemode,
            ),
        Command::new("give", "commands.give.description")
            .permission(permission::COMMAND_GIVE_SELF)
            .permission(permission::COMMAND_GIVE_OTHER)
            .overload(
                &[
                    Param::target("player"),
                    Param::item("itemName"),
                    Param::int("amount").optional(),
                ],
                give,
            ),
        Command::new("kill", "commands.kill.description")
            .permission(permission::COMMAND_KILL)
            .overload(&[Param::target("target").optional()], kill),
        Command::new("tp", "commands.tp.description")
            .alias("teleport")
            .permission(permission::COMMAND_TELEPORT_SELF)
            .permission(permission::COMMAND_TELEPORT_OTHER)
            .overload(&[Param::position("destination")], teleport)
            .overload(
                &[Param::target("victim"), Param::position("destination")],
                teleport,
            )
            .overload(&[Param::target("destination")], teleport)
            .overload(
                &[Param::target("victim"), Param::target("destination")],
                teleport,
            ),
    ]);
}

/// Empties the inventory, cursor, armor and off hand, or takes the stacks of `item` with damage
/// `data` (-1 or none: any damage), at most `maxCount` items of them (-1: all; 0 only counts
/// them) (`/clear [player] [item] [data] [maxCount]`). Operators only.
fn clear(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let operators = permission::COMMAND_CLEAR;
    let targets = permitted_targets(cx, *sender, args, "player", (operators, operators), "clear")?;
    let item = args.item("itemName");
    let data = args.int("data").filter(|&d| d != -1);
    let max = args.int("maxCount");
    let mut results = Results::default();
    for id in targets {
        let name = player_name(cx, id);
        let Some(p) = cx.player_mut(id) else {
            continue;
        };
        let matches = |s: &Stack| {
            !s.is_empty()
                && item.is_none_or(|t| s.item_type() == Some(t))
                && data.is_none_or(|d| i32::from(s.damage()) == d)
        };
        let held: Vec<(Slot, Stack)> = slots(p)
            .into_iter()
            .filter(|(_, s)| matches(s))
            .map(|(slot, s)| (slot, s.clone()))
            .collect();
        let total: u64 = held.iter().map(|(_, s)| u64::from(s.count())).sum();
        if item.is_some() && max == Some(0) {
            results.add(if total == 0 {
                Err(CLEAR_FAILURE.error(&[&name]))
            } else {
                Ok(CLEAR_TESTING.output(&[&name, &total.to_string()]))
            });
            continue;
        }
        // At most `left` items go: all without a count.
        let mut left = max
            .filter(|_| item.is_some())
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(u64::MAX);
        let mut cleared = 0_u64;
        for (slot, stack) in held {
            if left == 0 {
                break;
            }
            let count = u64::from(stack.count());
            let taken = count.min(left);
            let keep = u8::try_from(count - taken).unwrap_or(0);
            set_slot(p, slot, stack.with_count(keep));
            cleared += taken;
            left -= taken;
        }
        results.add(if cleared == 0 {
            Err(CLEAR_FAILURE.error(&[&name]))
        } else {
            let params = [name.as_str(), &cleared.to_string()];
            Ok(announce(cx, *sender, CLEAR_SUCCESS, &params))
        });
    }
    results.finish()
}

/// Lists the slots `/clear` empties and what they hold.
fn slots(p: &Player) -> Vec<(Slot, &Stack)> {
    let inv = &p.inventory;
    let mut out: Vec<(Slot, &Stack)> = (0_u8..)
        .map_while(|i| inv.main_stack(i).map(|s| (Slot::Main(i), s)))
        .collect();
    out.push((Slot::Cursor, inv.cursor()));
    out.extend(
        inv.armor()
            .iter()
            .enumerate()
            .map(|(i, s)| (Slot::Armor(i), s)),
    );
    out.push((Slot::Offhand, inv.offhand()));
    out
}

/// Replaces the stack in `slot` and tells the client.
fn set_slot(p: &mut Player, slot: Slot, stack: Stack) {
    match slot {
        Slot::Main(i) => inventory::set_main(p, i, stack),
        Slot::Cursor => inventory::set_cursor(p, stack),
        Slot::Armor(i) => inventory::set_armor(p, i, stack),
        Slot::Offhand => inventory::set_offhand(p, stack),
    }
}

/// Changes the players' game mode (`/gamemode <mode> [player]`): a player already in the mode is
/// a failure; the player is told its new mode, the sender too. The
/// [`PlayerGameModeChange`](crate::event::PlayerGameModeChange) listeners decide once the command
/// has returned, so the result and the message come before them.
fn gamemode(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let own = (
        permission::COMMAND_GAMEMODE_SELF,
        permission::COMMAND_GAMEMODE_OTHER,
    );
    let targets = permitted_targets(cx, *sender, args, "player", own, "gamemode")?;
    let Some(mode) = game_mode_arg(args, cx.default_game_mode()) else {
        return Err(usage(cx, "gamemode"));
    };
    has_room(cx, targets.len())?;
    let mode_name = game_mode_name(mode);
    let mut results = Results::default();
    for id in targets {
        let name = player_name(cx, id);
        if cx.player(id).is_none_or(|p| p.game_mode() == mode) {
            results.add(Err(GAMEMODE_FAILURE.error(&[&name])));
            continue;
        }
        perform(cx, action::SetGameMode::new(id, mode))?;
        tell_object(cx, id, GAME_MODE_CHANGED, &[mode_name]);
        if Sender::Player(id) == *sender {
            let line = announce(cx, *sender, GAMEMODE_SUCCESS_SELF, &[mode_name]);
            results.add(Ok(line));
        } else {
            let params = [mode_name, name.as_str()];
            results.add(Ok(announce(cx, *sender, GAMEMODE_SUCCESS_OTHER, &params)));
        }
    }
    results.finish()
}

/// Gives the players `amount` items (1 to 32,767; a full stack by default) in full stacks where
/// they fit (`/give <player> <item> [amount]`). What does not fit stays out, and the result
/// counts what was given. The player is told what it got. Left out: `item:damage` names and NBT
/// tags. The item is a built-in item or block; a server with custom items gives them with a
/// command of its own.
fn give(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let own = (
        permission::COMMAND_GIVE_SELF,
        permission::COMMAND_GIVE_OTHER,
    );
    let targets = permitted_targets(cx, *sender, args, "player", own, "give")?;
    let Some(kind) = args.item("itemName") else {
        return Err(usage(cx, "give"));
    };
    let full = kind.max_stack_size().max(1);
    let amount = match args.int("amount") {
        Some(n) => bounded(n, 1, MAX_GIVE)?.unsigned_abs(),
        None => u32::from(full),
    };
    let mut results = Results::default();
    for id in targets {
        let name = player_name(cx, id);
        let mut given = 0_u32;
        // At most one stack more than the inventory has slots: the first that does not fit ends.
        while given < amount {
            let count = u8::try_from((amount - given).min(u32::from(full))).unwrap_or(full);
            let stack = Stack::new(kind, count);
            let left = cx.player_mut(id).map_or(stack.clone(), |p| {
                p.inventory_mut(crate::inventory::Kind::Main).add(&stack)
            });
            given += u32::from(count.saturating_sub(left.count()));
            if !left.is_empty() {
                break;
            }
        }
        let given = given.to_string();
        tell_object(cx, id, GIVE_SUCCESS_RECIPIENT, &[kind.name(), &given]);
        let params = [kind.name(), &given, &name];
        results.add(Ok(announce(cx, *sender, GIVE_SUCCESS, &params)));
    }
    results.finish()
}

/// Kills the players with the generic death; the sender when no target is given
/// (`/kill [target]`). A player that does not die (creative, already dead) is a failure.
/// Operators only.
fn kill(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let operators = permission::COMMAND_KILL;
    let targets = permitted_targets(cx, *sender, args, "target", (operators, operators), "kill")?;
    let mut results = Results::default();
    for id in targets {
        let name = player_name(cx, id);
        if !cx.player_mut(id).is_some_and(Player::kill) {
            results.add(Err(KILL_FAILURE.error(&[&name])));
            continue;
        }
        results.add(Ok(if Sender::Player(id) == *sender {
            KILL_SUCCESSFUL.output(&[&name])
        } else {
            announce(cx, *sender, KILL_SUCCESSFUL, &[&name])
        }));
    }
    results.finish()
}

/// Teleports the players to a player or to a position in the sender's world (the default world
/// for the console), whose relative coordinates count from the sender, as Bedrock's do
/// (`/tp <destination>`, `/tp <x> <y> <z>`, `/tp <victim> <destination>` and
/// `/tp <victim> <x> <y> <z>`). The destination is one player. Each player moved is told where
/// to. The [`EntityTeleport`](crate::event::EntityTeleport) listeners decide once the command has
/// returned, after the result. Left out: the yaw and pitch forms, which would turn the player;
/// the player API turns no player.
fn teleport(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let own = (
        permission::COMMAND_TELEPORT_SELF,
        permission::COMMAND_TELEPORT_OTHER,
    );
    let victims = permitted_targets(cx, *sender, args, "victim", own, "tp")?;
    let mut results = Results::default();
    if let Some(destination) = args.targets("destination") {
        let &[to] = destination else {
            return Err(usage(cx, "tp"));
        };
        let Some((world, feet)) = cx.player(to).map(|p| (p.world(), p.position())) else {
            return results.finish();
        };
        has_room(cx, victims.len())?;
        let to_name = player_name(cx, to);
        for id in victims {
            let name = player_name(cx, id);
            perform(cx, action::Teleport::new(id, world, feet).by_command())?;
            tell_object(cx, id, TP_SUCCESS_VICTIM, &[&to_name]);
            results.add(Ok(announce(cx, *sender, TP_SUCCESS, &[&name, &to_name])));
        }
        return results.finish();
    }
    let Some(pos) = args.position("destination").map(in_world) else {
        return Err(usage(cx, "tp"));
    };
    let world = sender_world(cx, *sender);
    has_room(cx, victims.len())?;
    for id in victims {
        let name = player_name(cx, id);
        perform(cx, action::Teleport::new(id, world, pos).by_command())?;
        let [x, y, z] = [pos.x, pos.y, pos.z].map(two_decimals);
        tell_object(cx, id, TP_SUCCESS_VICTIM, &[&format!("{x}, {y}, {z}")]);
        let params = [name.as_str(), &x, &y, &z];
        results.add(Ok(announce(cx, *sender, TP_SUCCESS_COORDINATES, &params)));
    }
    results.finish()
}
