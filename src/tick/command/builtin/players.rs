//! Commands on players: `/clear`, `/effect`, `/enchant`, `/gamemode`, `/give`, `/kill`,
//! `/spawnpoint`, `/tp`, `/xp`, `/title`, `/tell` and `/transferserver` (PMMP's classes of the
//! same names).

use std::time::Duration;

use super::super::super::inventory;
use super::super::super::player::is_valid_address;
use super::{
    Msg, PLAYER_USER_ONLY, Results, announce, bounded, display_name_of, game_mode_name, in_world,
    notify, perform, permitted_targets, player_name, tell, tell_object, two_decimals, usage,
};
use crate::action;
use crate::command::{Args, Command, Error, Output, Param, Result, Sender};
use crate::item::Stack;
use crate::permission;
use crate::player::{self, Message, Player, TitleTimes};
use crate::tick::Context;
use crate::world::BlockPos;

/// Adds the commands of this module.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: the commands read as one table, one entry per PMMP class"
)]
pub(super) fn add(commands: &mut Vec<Command>) {
    commands.extend([
        Command::new("clear", "commands.clear.description")
            .permission(permission::COMMAND_CLEAR_SELF)
            .permission(permission::COMMAND_CLEAR_OTHER)
            .overload(
                &[
                    Param::target("player").optional(),
                    Param::item("itemName").optional(),
                    Param::int("data").optional(),
                    Param::int("maxCount").optional(),
                ],
                clear,
            ),
        Command::new("effect", "commands.effect.description")
            .permission(permission::COMMAND_EFFECT_SELF)
            .permission(permission::COMMAND_EFFECT_OTHER)
            .overload(
                &[
                    Param::target("player"),
                    Param::enumeration("Mode", "ClearEffects", &["clear"]),
                ],
                effect,
            )
            .overload(
                &[
                    Param::target("player"),
                    Param::effect("effect"),
                    Param::int("seconds").optional(),
                    Param::int("amplifier").optional(),
                    Param::bool("hideParticles").optional(),
                ],
                effect,
            )
            .overload(
                &[
                    Param::target("player"),
                    Param::effect("effect"),
                    Param::enumeration("Mode", "AddInfiniteEffect", &["infinite"]),
                    Param::int("amplifier").optional(),
                    Param::bool("hideParticles").optional(),
                ],
                effect,
            ),
        Command::new("enchant", "commands.enchant.description")
            .permission(permission::COMMAND_ENCHANT_SELF)
            .permission(permission::COMMAND_ENCHANT_OTHER)
            .overload(
                &[
                    Param::target("player"),
                    Param::enchantment("enchantmentName"),
                    Param::int("level").optional(),
                ],
                enchant,
            ),
        Command::new("gamemode", "commands.gamemode.description")
            .permission(permission::COMMAND_GAMEMODE_SELF)
            .permission(permission::COMMAND_GAMEMODE_OTHER)
            .overload(
                &[super::game_mode_param(), Param::target("player").optional()],
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
            .alias("suicide")
            .permission(permission::COMMAND_KILL_SELF)
            .permission(permission::COMMAND_KILL_OTHER)
            .overload(&[Param::target("target").optional()], kill),
        Command::new("spawnpoint", "commands.spawnpoint.description")
            .permission(permission::COMMAND_SPAWNPOINT_SELF)
            .permission(permission::COMMAND_SPAWNPOINT_OTHER)
            .overload(&[Param::target("player").optional()], spawnpoint)
            .overload(
                &[Param::target("player"), Param::position("spawnPos")],
                spawnpoint,
            ),
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
        Command::new("xp", "commands.xp.description")
            .permission(permission::COMMAND_XP_SELF)
            .permission(permission::COMMAND_XP_OTHER)
            .overload(
                &[Param::int("amount"), Param::target("player").optional()],
                |cx, sender, args| xp(cx, sender, args, Xp::Points),
            )
            .overload(
                &[
                    Param::suffixed_int("amount", "l"),
                    Param::target("player").optional(),
                ],
                |cx, sender, args| xp(cx, sender, args, Xp::Levels),
            ),
        Command::new("title", "commands.title.description")
            .permission(permission::COMMAND_TITLE_SELF)
            .permission(permission::COMMAND_TITLE_OTHER)
            .overload(
                &[
                    Param::target("player"),
                    Param::enumeration("clear", "TitleClear", &["clear"]),
                ],
                title,
            )
            .overload(
                &[
                    Param::target("player"),
                    Param::enumeration("reset", "TitleReset", &["reset"]),
                ],
                title,
            )
            .overload(
                &[
                    Param::target("player"),
                    Param::enumeration(
                        "titleLocation",
                        "TitleSet",
                        &["title", "subtitle", "actionbar"],
                    ),
                    Param::message("titleText"),
                ],
                title,
            )
            .overload(
                &[
                    Param::target("player"),
                    Param::enumeration("times", "TitleTimes", &["times"]),
                    Param::int("fadeIn"),
                    Param::int("stay"),
                    Param::int("fadeOut"),
                ],
                title,
            ),
        Command::new("tell", "commands.tell.description")
            .alias("w")
            .alias("msg")
            .permission(permission::COMMAND_TELL)
            .overload(
                &[Param::target("target"), Param::message("message")],
                tell_command,
            ),
        Command::new("transferserver", "commands.transferserver.description")
            .permission(permission::COMMAND_TRANSFERSERVER)
            .overload(
                &[Param::string("server"), Param::int("port").optional()],
                transferserver,
            ),
    ]);
}

const CLEAR_FAILURE: Msg = Msg::client("commands.clear.failure.no.items");
const CLEAR_SUCCESS: Msg = Msg::client("commands.clear.success");
const CLEAR_TESTING: Msg = Msg::client("commands.clear.testing");
const EFFECT_NOT_ACTIVE: Msg = Msg::client("commands.effect.failure.notActive");
const EFFECT_NOT_ACTIVE_ALL: Msg = Msg::client("commands.effect.failure.notActive.all");
const EFFECT_REMOVED: Msg = Msg::client("commands.effect.success.removed");
const EFFECT_REMOVED_ALL: Msg = Msg::client("commands.effect.success.removed.all");
const EFFECT_SUCCESS: Msg = Msg::client("commands.effect.success");
const EFFECT_SUCCESS_INFINITE: Msg = Msg::client("commands.effect.success.infinite");
const ENCHANT_NO_ITEM: Msg = Msg::client("commands.enchant.noItem");
const ENCHANT_SUCCESS: Msg = Msg::client("commands.enchant.success");
const ENCHANT_CANT: Msg = Msg::client("commands.enchant.cantEnchant");
const GAMEMODE_FAILURE: Msg = Msg::own("The game mode of {%0} stays as it is");
const GAMEMODE_SUCCESS_SELF: Msg = Msg::client("commands.gamemode.success.self");
const GAMEMODE_SUCCESS_OTHER: Msg = Msg::client("commands.gamemode.success.other");
const GAME_MODE_CHANGED: Msg = Msg::client("gameMode.changed");
const GIVE_SUCCESS: Msg = Msg::client("commands.give.success");
const GIVE_SUCCESS_RECIPIENT: Msg = Msg::client("commands.give.successRecipient");
const KILL_SUCCESSFUL: Msg = Msg::client("commands.kill.successful");
const SPAWNPOINT_SUCCESS: Msg = Msg::client("commands.spawnpoint.success.single");
const TP_SUCCESS: Msg = Msg::client("commands.tp.success");
const TP_SUCCESS_COORDINATES: Msg = Msg::client("commands.tp.success.coordinates");
const TP_SUCCESS_VICTIM: Msg = Msg::client("commands.tp.successVictim");
const XP_WITHDRAW: Msg = Msg::client("commands.xp.failure.widthdrawXp");
const XP_SUCCESS: Msg = Msg::client("commands.xp.success");
const XP_SUCCESS_LEVELS: Msg = Msg::client("commands.xp.success.levels");
const XP_TAKEN_LEVELS: Msg = Msg::client("commands.xp.success.negative.levels");
const TITLE_SUCCESS: Msg = Msg::client("commands.title.success");
const MESSAGE_SAME_TARGET: Msg = Msg::client("commands.message.sameTarget");
const MESSAGE_INCOMING: Msg = Msg::client("commands.message.display.incoming");
const MESSAGE_OUTGOING: Msg = Msg::client("commands.message.display.outgoing");
/// How whispers look (PMMP: gray, italic).
const WHISPER_PREFIX: &str = "§b§o";
/// The longest `/give` amount (PMMP `GiveCommand`).
const MAX_GIVE: i32 = 32_767;
/// The highest experience level (PMMP's attribute maximum).
const MAX_XP_LEVEL: i32 = 24_791;
/// The port `/transferserver` uses when none is given.
const DEFAULT_PORT: i32 = 19_132;

/// A slot `/clear` empties, in PMMP's order: the inventory, the cursor, the armour, the off hand.
#[derive(Debug, Clone, Copy)]
enum Slot {
    Main(u8),
    Cursor,
    Armor(usize),
    Offhand,
}

/// The slots `/clear` empties and what they hold.
fn slots(p: &Player) -> Vec<(Slot, &Stack)> {
    let inv = &p.inventory;
    let mut out: Vec<(Slot, &Stack)> = (0u8..)
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

/// `/clear [player] [item] [data] [maxCount]` (PMMP `ClearCommand`, BDS's parameters): empties
/// the inventory, cursor, armour and off hand, or takes the stacks of `item` with damage `data`
/// (-1 or none: any damage), at most `maxCount` items of them (-1: all; 0 only counts them).
fn clear(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let own = (
        permission::COMMAND_CLEAR_SELF,
        permission::COMMAND_CLEAR_OTHER,
    );
    let targets = permitted_targets(cx, *sender, args, "player", own, "clear")?;
    let item = args.item("itemName");
    let data = args.int("data").filter(|&d| d != -1);
    let max = args.int("maxCount").map(|n| n.clamp(-1, super::MAX_COORD));
    let mut results = Results::default();
    for id in targets {
        let name = player_name(cx, id);
        let Some(p) = cx.state.players.get_mut(&id.session()) else {
            continue;
        };
        let matches = |s: &Stack| {
            !s.is_empty()
                && item.is_none_or(|t| s.item() == Some(t))
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
        let mut cleared = 0u64;
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

/// `/effect <player> clear` and `/effect <player> <effect> [seconds|infinite] [amplifier]
/// [hideParticles]` (PMMP `EffectCommand`, in BDS's overloads): 0 seconds takes the effect away;
/// no seconds gives its default duration; `infinite` never ends.
fn effect(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let own = (
        permission::COMMAND_EFFECT_SELF,
        permission::COMMAND_EFFECT_OTHER,
    );
    let targets = permitted_targets(cx, *sender, args, "player", own, "effect")?;
    let mut results = Results::default();
    let Some(kind) = args.effect("effect") else {
        for id in targets {
            let shown = display_name_of(cx, Sender::Player(id));
            if let Some(p) = cx.player_mut(id) {
                p.clear_effects();
            }
            results.add(Ok(EFFECT_REMOVED_ALL.output(&[&shown])));
        }
        return results.finish();
    };
    let seconds = args
        .int("seconds")
        .map(|s| bounded(s, 0, i32::MAX / 20))
        .transpose()?;
    let amplifier = bounded(args.int("amplifier").unwrap_or(0), 0, 255)?;
    let amplifier = u8::try_from(amplifier).unwrap_or(u8::MAX);
    let duration = seconds.map_or(kind.default_duration_ticks(), |s| s.unsigned_abs() * 20);
    // Only the `infinite` overload has a `Mode` with an effect.
    let infinite = args.has("Mode");
    let given = kind
        .instance()
        .with_duration_ticks(duration)
        .with_amplifier(amplifier)
        .with_visible(args.bool("hideParticles") != Some(true))
        .with_infinite(infinite);
    let effect_name = format!("%{}", kind.text());
    for id in targets {
        let shown = display_name_of(cx, Sender::Player(id));
        let Some(p) = cx.player_mut(id) else {
            continue;
        };
        if duration == 0 {
            results.add(if p.remove_effect(kind) {
                Ok(EFFECT_REMOVED.output(&[&effect_name, &shown]))
            } else if p.effects().next().is_none() {
                Err(EFFECT_NOT_ACTIVE_ALL.error(&[&shown]))
            } else {
                Err(EFFECT_NOT_ACTIVE.error(&[&effect_name, &shown]))
            });
        } else {
            p.add_effect(given);
            let amplifier = amplifier.to_string();
            results.add(Ok(if infinite {
                let params = [effect_name.as_str(), &amplifier, &shown];
                announce(cx, *sender, EFFECT_SUCCESS_INFINITE, &params)
            } else {
                let seconds = (f64::from(duration) / 20.0).to_string();
                let params = [effect_name.as_str(), &amplifier, &shown, &seconds];
                announce(cx, *sender, EFFECT_SUCCESS, &params)
            }));
        }
    }
    results.finish()
}

/// `/enchant <player> <enchantment> [level]` (PMMP `EnchantCommand`): enchants the held item at
/// the level (1 by default, at most the enchantment's highest); a book becomes an enchanted
/// book. Like PMMP it does not check what the item may take.
fn enchant(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let own = (
        permission::COMMAND_ENCHANT_SELF,
        permission::COMMAND_ENCHANT_OTHER,
    );
    let targets = permitted_targets(cx, *sender, args, "player", own, "enchant")?;
    let Some(kind) = args.enchantment("enchantmentName") else {
        return Err(usage(cx, "enchant"));
    };
    let level = bounded(
        args.int("level").unwrap_or(1),
        1,
        i32::from(kind.max_level()),
    )?;
    let level = u8::try_from(level).unwrap_or(1);
    let mut results = Results::default();
    for id in targets {
        let name = player_name(cx, id);
        let Some(p) = cx.state.players.get_mut(&id.session()) else {
            continue;
        };
        let held = p.inventory.held();
        if held.is_empty() {
            results.add(Err(ENCHANT_NO_ITEM.error(&[])));
            continue;
        }
        // A held item at the stack size limit takes no more (BDS's message for an item that
        // cannot take the enchantment).
        let Ok(enchanted) = inventory::enchant_item(held, &[(kind, level)]) else {
            results.add(Err(ENCHANT_CANT.error(&[&name])));
            continue;
        };
        inventory::set_held(p, enchanted);
        results.add(Ok(announce(cx, *sender, ENCHANT_SUCCESS, &[&name])));
    }
    results.finish()
}

/// `/gamemode <mode> [player]` (PMMP `GamemodeCommand`): a player already in the mode is a
/// failure; the player is told its new mode, the sender too as BDS does (PMMP tells only another
/// player). The [`PlayerGameModeChange`](crate::event::PlayerGameModeChange) listeners decide
/// once the command has returned, so the result and the message come before them (PMMP asks
/// first and reports a cancel as a failure).
fn gamemode(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let own = (
        permission::COMMAND_GAMEMODE_SELF,
        permission::COMMAND_GAMEMODE_OTHER,
    );
    let targets = permitted_targets(cx, *sender, args, "player", own, "gamemode")?;
    let Some(mode) = super::game_mode_arg(args, Some(cx.default_game_mode())) else {
        return Err(usage(cx, "gamemode"));
    };
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

/// `/give <player> <item> [amount]` (PMMP `GiveCommand`): `amount` items (1 to 32,767; a full
/// stack by default) in full stacks where they fit. PMMP loses what does not fit; here it stays
/// out, and the result counts what was given. The player is told what it got, as BDS does.
/// Left out: PMMP's `item:damage` names and NBT tags.
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
        let mut given = 0u32;
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

/// `/kill [player]` (PMMP `KillCommand`): kills with the generic death (PMMP deals suicide damage
/// of the full health, which the damage event may cancel).
fn kill(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let own = (
        permission::COMMAND_KILL_SELF,
        permission::COMMAND_KILL_OTHER,
    );
    let targets = permitted_targets(cx, *sender, args, "target", own, "kill")?;
    let mut results = Results::default();
    for id in targets {
        let name = player_name(cx, id);
        if let Some(p) = cx.player_mut(id) {
            p.kill();
        }
        results.add(Ok(if Sender::Player(id) == *sender {
            KILL_SUCCESSFUL.output(&[&name])
        } else {
            announce(cx, *sender, KILL_SUCCESSFUL, &[&name])
        }));
    }
    results.finish()
}

/// `/spawnpoint [player]` and `/spawnpoint <player> <x> <y> <z>` (PMMP `SpawnpointCommand`):
/// the players respawn at the block of the sender's feet, or at the position, in their own
/// world. Without a position only a player may use it.
fn spawnpoint(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let own = (
        permission::COMMAND_SPAWNPOINT_SELF,
        permission::COMMAND_SPAWNPOINT_OTHER,
    );
    let targets = permitted_targets(cx, *sender, args, "player", own, "spawnpoint")?;
    let at = match (args.position("spawnPos"), *sender) {
        (Some(pos), _) => (None, BlockPos::containing(in_world(pos))),
        (None, Sender::Player(me)) => {
            let Some(p) = cx.player(me) else {
                return Ok(Output::new());
            };
            (
                Some(p.world()),
                BlockPos::containing(in_world(p.position())),
            )
        }
        (None, Sender::Console) => return Err(usage(cx, "spawnpoint")),
    };
    let mut results = Results::default();
    for id in targets {
        let name = player_name(cx, id);
        let Some(world) = at.0.or_else(|| cx.player(id).map(Player::world)) else {
            continue;
        };
        let world_name = cx
            .world(world)
            .map(|w| w.name().to_owned())
            .unwrap_or_default();
        let pos = at.1;
        if let Some(p) = cx.player_mut(id) {
            p.set_spawn(Some((&world_name, pos)))
                .map_err(|e| Error::failed(e.to_string()))?;
        }
        let params = [
            name.as_str(),
            &pos.x.to_string(),
            &pos.y.to_string(),
            &pos.z.to_string(),
        ];
        results.add(Ok(announce(cx, *sender, SPAWNPOINT_SUCCESS, &params)));
    }
    results.finish()
}

/// `/tp <destination>`, `/tp <x> <y> <z>`, `/tp <victim> <destination>` and
/// `/tp <victim> <x> <y> <z>` (PMMP `TeleportCommand`). Relative coordinates count from the
/// sender, as Bedrock's do (PMMP: from the player moved). The destination is one player. Each
/// player moved is told where to, as BDS does. The [`EntityTeleport`](crate::event::EntityTeleport)
/// listeners decide once the command has returned, after the result, as in PMMP. Left out until
/// the player API turns players: PMMP's yaw and pitch forms.
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
        let to_name = player_name(cx, to);
        for id in victims {
            let name = player_name(cx, id);
            perform(cx, action::Teleport::new(id, world, feet))?;
            tell_object(cx, id, TP_SUCCESS_VICTIM, &[&to_name]);
            results.add(Ok(announce(cx, *sender, TP_SUCCESS, &[&name, &to_name])));
        }
        return results.finish();
    }
    let Some(pos) = args.position("destination").map(in_world) else {
        return Err(usage(cx, "tp"));
    };
    for id in victims {
        let name = player_name(cx, id);
        if let Some(world) = cx.player(id).map(Player::world) {
            perform(cx, action::Teleport::new(id, world, pos))?;
        }
        let [x, y, z] = [pos.x, pos.y, pos.z].map(two_decimals);
        tell_object(cx, id, TP_SUCCESS_VICTIM, &[&format!("{x}, {y}, {z}")]);
        let params = [name.as_str(), &x, &y, &z];
        results.add(Ok(announce(cx, *sender, TP_SUCCESS_COORDINATES, &params)));
    }
    results.finish()
}

/// What `/xp` gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Xp {
    /// `/xp <amount>`.
    Points,
    /// `/xp <amount>L`.
    Levels,
}

/// `/xp <amount>[L] [player]` (PMMP `XpCommand`): points, or levels with `L` (BDS's suffixed
/// number, any case); negative levels take levels away, negative points are refused. Fails with
/// `experience = false`.
fn xp(cx: &mut Context<'_>, sender: &Sender, args: &Args, what: Xp) -> Result<Output> {
    if !cx.state.settings.experience {
        return Err(Error::failed(player::Error::Disabled.to_string()));
    }
    let own = (permission::COMMAND_XP_SELF, permission::COMMAND_XP_OTHER);
    let targets = permitted_targets(cx, *sender, args, "player", own, "xp")?;
    let Some(value) = args.int("amount") else {
        return Err(usage(cx, "xp"));
    };
    let levels = what == Xp::Levels;
    let mut results = Results::default();
    for id in targets {
        let name = player_name(cx, id);
        let Some(p) = cx.player_mut(id) else {
            continue;
        };
        if levels {
            let current = i32::try_from(p.xp_level()).unwrap_or(MAX_XP_LEVEL);
            let change = value.clamp(-current, MAX_XP_LEVEL - current);
            if change >= 0 {
                p.add_xp_levels(change.unsigned_abs())
                    .map_err(|e| Error::failed(e.to_string()))?;
                results.add(Ok(XP_SUCCESS_LEVELS.output(&[&change.to_string(), &name])));
            } else {
                let taken = change.unsigned_abs();
                p.subtract_xp_levels(taken);
                results.add(Ok(XP_TAKEN_LEVELS.output(&[&taken.to_string(), &name])));
            }
        } else {
            let points = value.clamp(-super::MAX_COORD, i32::MAX);
            let Ok(points) = u32::try_from(points) else {
                return Err(XP_WITHDRAW.error(&[]));
            };
            p.add_xp(points);
            results.add(Ok(XP_SUCCESS.output(&[&points.to_string(), &name])));
        }
    }
    results.finish()
}

/// The title timing of a number of ticks (negative: none).
fn ticks(n: i32) -> Duration {
    Duration::from_millis(u64::try_from(n).unwrap_or(0).saturating_mul(50))
}

/// `/title <player> clear|reset|title|subtitle|actionbar|times ...` (PMMP `TitleCommand`).
fn title(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let own = (
        permission::COMMAND_TITLE_SELF,
        permission::COMMAND_TITLE_OTHER,
    );
    let targets = permitted_targets(cx, *sender, args, "player", own, "title")?;
    let text = args.string("titleText").unwrap_or_default().to_owned();
    let message = if args.has("clear") {
        Message::ClearTitle
    } else if args.has("reset") {
        Message::ResetTitle
    } else if args.has("times") {
        let int = |name| {
            args.int(name)
                .unwrap_or(0)
                .clamp(-super::MAX_COORD, super::MAX_COORD)
        };
        Message::TitleTimes(TitleTimes {
            fade_in: ticks(int("fadeIn")),
            stay: ticks(int("stay")),
            fade_out: ticks(int("fadeOut")),
        })
    } else {
        match args.string("titleLocation") {
            Some("subtitle") => Message::Subtitle(text),
            Some("actionbar") => Message::ActionBar(text),
            _ => Message::Title {
                title: text,
                subtitle: String::new(),
                times: None,
            },
        }
    };
    for id in targets {
        tell(cx, id, message.clone());
    }
    Ok(TITLE_SUCCESS.output(&[]))
}

/// `/tell <player> <message>` (aliases `/w`, `/msg`; PMMP `TellCommand`): whispers to each
/// player but the sender; a line naming only the sender is a failure.
fn tell_command(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let text = args.string("message").unwrap_or_default();
    let to: Vec<player::Id> = args
        .targets("target")
        .unwrap_or_default()
        .iter()
        .copied()
        .filter(|&id| Sender::Player(id) != *sender)
        .collect();
    if to.is_empty() {
        return Err(MESSAGE_SAME_TARGET.error(&[]));
    }
    let from = display_name_of(cx, *sender);
    let mut results = Results::default();
    for id in to {
        let shown = display_name_of(cx, Sender::Player(id));
        tell(
            cx,
            id,
            MESSAGE_INCOMING.chat(WHISPER_PREFIX, &[&from, text]),
        );
        notify(cx, *sender, MESSAGE_OUTGOING, &[&shown, text]);
        results.add(Ok(MESSAGE_OUTGOING.output(&[&shown, text])));
    }
    results.finish()
}

/// `/transferserver <server> [port]` (PMMP `TransferServerCommand`): sends the sending player to
/// another server (port 19132 by default), once the
/// [`PlayerTransfer`](crate::event::PlayerTransfer) listeners have let it.
fn transferserver(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let Sender::Player(me) = *sender else {
        return Err(PLAYER_USER_ONLY.error(&[]));
    };
    let port = bounded(
        args.int("port").unwrap_or(DEFAULT_PORT),
        0,
        i32::from(u16::MAX),
    )?;
    let port = u16::try_from(port).unwrap_or_default();
    let address = args.string("server").unwrap_or_default();
    if !is_valid_address(address) {
        return Err(Error::failed(player::Error::InvalidAddress.to_string()));
    }
    perform(cx, action::Transfer::new(me, address, port))?;
    Ok(Output::new())
}
