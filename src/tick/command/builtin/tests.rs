//! The built-in commands through the tick: what each does, its refusals (arguments, permission,
//! the console where only a player fits) and its messages.

use std::net::IpAddr;

use tokio::sync::mpsc;

use super::super::tests::{id, lines, server, server_with};
use super::super::{Error, Output, Sender, available_commands, execute};
use crate::command::{self, Command};
use crate::config::{Difficulty, GameMode};
use crate::effect;
use crate::event::{
    EntityTeleport, Listeners, PlayerGameModeChange, PlayerKick, PlayerTransfer, Priority,
};
use crate::item::{self, Stack};
use crate::math::Vec3;
use crate::permission;
use crate::protocol::consts::CommandArg;
use crate::protocol::packet::Packet;
use crate::runtime::ThreadTotals;
use crate::tick::log::InputLog;
use crate::tick::queue::{Measure, Outbound, SessionId};
use crate::tick::tests::{received, tick};
use crate::tick::{Context, Output as TickOutput, State, save};
use crate::world::BlockPos;

/// Runs `line` as `sender`, then the actions it queued, as the tick does.
fn run(state: &mut State, sender: Sender, line: &str) -> command::Result<Output> {
    let result = execute(state, sender, line);
    crate::tick::dispatch::run_deferred(state, &mut 0);
    result
}

/// The output lines (texts or keys) of a success.
fn ok(state: &mut State, sender: Sender, line: &str) -> Vec<String> {
    lines(run(state, sender, line))
}

/// The text or key of a failure.
fn fails(state: &mut State, sender: Sender, line: &str) -> String {
    match run(state, sender, line) {
        Err(e) => e.line().text,
        Ok(out) => panic!("{line:?} succeeded: {out:?}"),
    }
}

/// Whether a failure is a line that does not fit the command: a syntax error, or the command's
/// usage from its handler.
fn is_usage(result: &command::Result<Output>) -> bool {
    matches!(result, Err(Error::Syntax { .. } | Error::Usage { .. }))
}

/// Whether a failure is a missing permission.
fn is_refused(result: &command::Result<Output>) -> bool {
    matches!(result, Err(Error::Permission { .. }))
}

/// A server with player 1 an operator and player 2 not.
fn with_op() -> (State, Vec<mpsc::Receiver<Outbound>>) {
    let (mut state, rx) = server(vec![]);
    Context { state: &mut state }.op("player1").unwrap();
    (state, rx)
}

/// The player of `session`.
fn player(state: &State, session: u64) -> &crate::player::Player {
    &state.players[&SessionId::new(session)]
}

/// The chat lines (text or key, parameters) `rx` got after a tick, and the final packet.
fn chat(
    state: &mut State,
    rx: &mut mpsc::Receiver<Outbound>,
) -> (Vec<(String, Vec<String>)>, Option<Packet>) {
    let mut output = TickOutput::default();
    tick(state, &mut vec![], &mut output);
    let (packets, last, _) = received(rx);
    let texts = packets
        .into_iter()
        .filter_map(|p| match p {
            Packet::Text(t) => Some((t.message, t.parameters)),
            _ => None,
        })
        .collect();
    (texts, last)
}

/// The message a player was disconnected with.
fn kicked_with(last: Option<Packet>) -> String {
    match last {
        Some(Packet::Disconnect(d)) => d.message,
        other => panic!("not disconnected: {other:?}"),
    }
}

#[test]
fn pmmp_texts_fill_each_parameter_once() {
    let msg = super::Msg::own("a {%0} b {%1} {%2}");
    assert_eq!(msg.render(&["{%1}", "x"]).unwrap(), "a {%1} b x {%2}");
    assert_eq!(super::Msg::client("k").render(&[]), None);
    assert_eq!(
        msg.output(&["1", "2"]).lines().collect::<Vec<_>>(),
        ["a 1 b 2 {%2}"]
    );
}

#[test]
fn every_builtin_registers_and_users_replace_or_drop_them() {
    let (state, _rx) = server(vec![]);
    let mut names: Vec<&str> = state.commands.list.iter().map(Command::name).collect();
    assert_eq!(
        names.len(),
        39,
        "PMMP's 41 commands but 4, backup and check"
    );
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), 39);
    for skipped in ["dumpmemory", "gc", "plugins", "timings"] {
        assert!(!names.contains(&skipped));
    }
    let mut commands = crate::Server::builder()
        .command(Command::new("me", "mine").overload(&[], |_, _, _| Ok(Output::from("user's me"))))
        .without_command("STOP")
        .setup
        .commands;
    commands.finish().unwrap();
    let mut state = crate::tick::tests::new_state(
        crate::tick::tests::settings(|_| {}),
        crate::event::Listeners::default(),
    );
    state.commands = commands;
    assert_eq!(ok(&mut state, Sender::Console, "me"), ["user's me"]);
    assert!(matches!(
        run(&mut state, Sender::Console, "stop"),
        Err(Error::Unknown { .. })
    ));
}

// pmmp: Command::setPermissions, Command::getPermissions
#[test]
fn available_commands_list_the_builtins_a_player_may_use_with_typed_overloads() {
    let (mut state, _rx) = server(vec![]);
    let s2 = SessionId::new(2);
    let packet = available_commands(state.commands.listing(), s2, &state.players[&s2]);
    let Packet::AvailableCommands(p) = &packet else {
        panic!("{packet:?}");
    };
    let mut names: Vec<&str> = p.commands.iter().map(|c| c.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["clear", "kill", "me", "tell", "version"],
        "PMMP's defaults for players who are not operators, but help (the client has its own)"
    );
    Context { state: &mut state }.op("player2").unwrap();
    let packet = available_commands(state.commands.listing(), s2, &state.players[&s2]);
    let Packet::AvailableCommands(p) = &packet else {
        panic!("{packet:?}");
    };
    assert_eq!(p.commands.len(), 38, "all but help");
    let tp = p.commands.iter().find(|c| c.name == "tp").unwrap();
    assert_eq!(tp.overloads.len(), 4);
    let shapes: Vec<Vec<&str>> = tp
        .overloads
        .iter()
        .map(|o| o.parameters.iter().map(|p| p.name.as_str()).collect())
        .collect();
    assert_eq!(
        shapes,
        [
            vec!["destination"],
            vec!["victim", "destination"],
            vec!["destination"],
            vec!["victim", "destination"]
        ],
        "a position, then a player, as BDS"
    );
    let gamemode = p.commands.iter().find(|c| c.name == "gamemode").unwrap();
    let mode = &gamemode.overloads[0].parameters[0];
    assert_ne!(mode.r#type & u32::from(CommandArg::Enum), 0);
    let index = usize::try_from(mode.r#type & 0xffff).unwrap();
    assert_eq!(p.enums[index].r#type, "GameMode");
    assert_eq!(p.enums[index].value_indices.len(), 9, "BDS's names");
    let tell = p.commands.iter().find(|c| c.name == "tell").unwrap();
    let aliases = &p.enums[usize::try_from(tell.aliases_offset).unwrap()];
    let aliases: Vec<&str> = aliases
        .value_indices
        .iter()
        .map(|&i| p.enum_values[usize::try_from(i).unwrap()].as_str())
        .collect();
    assert_eq!(aliases, ["tell", "w", "msg"]);
    let give = state
        .commands
        .list
        .iter()
        .find(|c| c.name() == "give")
        .unwrap();
    assert_eq!(
        give.permissions(),
        [
            permission::COMMAND_GIVE_SELF,
            permission::COMMAND_GIVE_OTHER
        ]
    );
}

#[test]
fn op_and_deop_change_the_list_tell_the_player_and_copy_operators() {
    let (mut state, mut rx) = with_op();
    assert!(is_refused(&run(&mut state, id(2), "op player2")));
    assert!(is_usage(&run(&mut state, Sender::Console, "op bad-name!")));
    assert!(is_usage(&run(&mut state, Sender::Console, "op console")));
    assert_eq!(ok(&mut state, id(1), "op Player2"), ["commands.op.success"]);
    assert!(Context { state: &mut state }.is_op("player2"));
    let (to2, _) = chat(&mut state, &mut rx[1]);
    assert_eq!(to2[0], ("§b%commands.op.message".to_owned(), vec![]));
    assert_eq!(
        ok(&mut state, Sender::Console, "deop player2"),
        ["commands.deop.success"]
    );
    assert!(!Context { state: &mut state }.is_op("player2"));
    let (to2, _) = chat(&mut state, &mut rx[1]);
    assert_eq!(to2, [("§b%commands.deop.message".to_owned(), vec![])]);
    // Player 1, an operator, got a copy of the console's result, not of its own.
    let (to1, _) = chat(&mut state, &mut rx[0]);
    assert_eq!(
        to1,
        [(
            "§7§o[Console: %commands.deop.success]".to_owned(),
            vec!["player2".to_owned()]
        )]
    );
}

// pmmp: CommandSender::getName, ConsoleCommandSender::getName, CommandSender::sendMessage, ConsoleCommandSender::sendMessage, Command::broadcastCommandMessage
#[test]
fn senders_have_names_get_messages_and_operators_get_copies() {
    let (mut state, mut rx) = with_op();
    let mut cx = Context { state: &mut state };
    assert_eq!(cx.sender_name(id(1)), Some("player1"));
    assert_eq!(cx.sender_name(Sender::Console), Some("Console"));
    assert_eq!(cx.sender_name(id(9)), None);
    let raw = |text: &str| crate::player::Message::Raw(text.to_owned());
    cx.send_to(id(2), raw("hi")).unwrap();
    cx.send_to(Sender::Console, raw("logged")).unwrap();
    cx.send_to(id(9), raw("nobody")).unwrap();
    cx.broadcast_command_message(id(2), "did it");
    let (to2, _) = chat(&mut state, &mut rx[1]);
    assert_eq!(to2, [("hi".to_owned(), vec![])], "no copy for the sender");
    let (to1, _) = chat(&mut state, &mut rx[0]);
    assert_eq!(
        to1,
        [(
            "§7§o%chat.type.admin".to_owned(),
            vec!["player2".to_owned(), "did it".to_owned()]
        )]
    );
}

#[test]
fn ban_and_pardon_keep_the_list_and_kick_the_online_player() {
    let (mut state, mut rx) = with_op();
    assert!(is_refused(&run(&mut state, id(2), "ban player1")));
    assert!(is_usage(&run(&mut state, id(1), "ban")));
    assert_eq!(
        ok(&mut state, id(1), "ban player2 too much spam"),
        ["commands.ban.success"]
    );
    let cx = Context { state: &mut state };
    let ban = cx.ban_entry("player2").unwrap();
    assert_eq!((ban.reason(), ban.source()), ("too much spam", "player1"));
    let (_, last) = chat(&mut state, &mut rx[1]);
    assert_eq!(
        kicked_with(last),
        "This server has banned you: too much spam"
    );
    assert_eq!(
        ok(&mut state, Sender::Console, "banlist"),
        ["commands.banlist.players", "player2"]
    );
    assert_eq!(
        ok(&mut state, Sender::Console, "pardon PLAYER2"),
        ["commands.unban.success"]
    );
    assert!(!Context { state: &mut state }.is_banned("player2"));
    assert!(is_usage(&run(&mut state, Sender::Console, "banlist nope")));
}

#[test]
fn ban_ip_takes_an_address_or_an_online_players_and_pardon_ip_lifts_it() {
    let (mut state, mut rx) = with_op();
    let ip = IpAddr::from([10, 0, 0, 2]);
    state
        .players
        .get_mut(&SessionId::new(2))
        .unwrap()
        .identity
        .address = Some(ip);
    assert_eq!(
        fails(&mut state, Sender::Console, "ban-ip nobody"),
        "commands.banip.invalid"
    );
    assert_eq!(
        ok(&mut state, Sender::Console, "ban-ip player2 bye"),
        ["commands.banip.success.players"]
    );
    assert!(Context { state: &mut state }.is_ip_banned(ip));
    let (_, last) = chat(&mut state, &mut rx[1]);
    assert_eq!(kicked_with(last), "This server has banned you: bye");
    assert_eq!(
        ok(&mut state, Sender::Console, "ban-ip ::1"),
        ["commands.banip.success"]
    );
    assert_eq!(
        ok(&mut state, Sender::Console, "banlist ips"),
        ["commands.banlist.ips", "10.0.0.2, ::1"]
    );
    assert_eq!(
        fails(&mut state, Sender::Console, "pardon-ip x"),
        "commands.unbanip.invalid"
    );
    assert_eq!(
        ok(&mut state, Sender::Console, "pardon-ip 10.0.0.2"),
        ["commands.unbanip.success"]
    );
    assert!(!Context { state: &mut state }.is_ip_banned(ip));
    assert!(is_refused(&run(&mut state, id(2), "pardon-ip ::1")));
}

#[test]
fn whitelist_actions_each_need_their_permission_and_kick_who_is_left_out() {
    let (mut state, mut rx) = with_op();
    assert!(is_refused(&run(&mut state, id(2), "whitelist list")));
    let player2 = crate::player::Id::new(SessionId::new(2));
    Context { state: &mut state }.set_permission(player2, permission::COMMAND_WHITELIST_LIST, true);
    assert_eq!(
        ok(&mut state, id(2), "whitelist list"),
        ["commands.allowlist.list", ""]
    );
    assert!(is_refused(&run(&mut state, id(2), "whitelist on")));
    assert!(is_usage(&run(&mut state, id(1), "whitelist add")));
    assert!(is_usage(&run(&mut state, id(1), "whitelist add bad/name")));
    assert_eq!(
        ok(&mut state, id(1), "whitelist add Alex"),
        ["commands.allowlist.add.success"]
    );
    assert_eq!(
        ok(&mut state, id(1), "whitelist on"),
        ["commands.allowlist.enabled"]
    );
    assert!(Context { state: &mut state }.whitelist_enabled());
    // Player 1 is an operator and stays; player 2 is not listed.
    let (_, last) = chat(&mut state, &mut rx[1]);
    assert_eq!(kicked_with(last), "disconnectionScreen.notAllowed");
    assert!(state.players.contains_key(&SessionId::new(1)));
    assert_eq!(
        ok(&mut state, Sender::Console, "whitelist remove alex"),
        ["commands.allowlist.remove.success"]
    );
    assert_eq!(
        ok(&mut state, Sender::Console, "whitelist off"),
        ["commands.allowlist.disabled"]
    );
    assert!(!Context { state: &mut state }.whitelist_enabled());
}

#[test]
fn kick_disconnects_with_the_reason_or_the_clients_kicked_text() {
    let (mut state, mut rx) = with_op();
    assert!(is_refused(&run(&mut state, id(2), "kick player1")));
    assert!(matches!(
        run(&mut state, id(1), "kick nobody"),
        Err(Error::PlayerNotFound { .. })
    ));
    assert_eq!(
        ok(&mut state, id(1), "kick player2 be nice"),
        ["commands.kick.success.reason"]
    );
    let (_, last) = chat(&mut state, &mut rx[1]);
    assert_eq!(kicked_with(last), "be nice");
    assert_eq!(
        ok(&mut state, Sender::Console, "kick player1"),
        ["commands.kick.success"]
    );
    let (_, last) = chat(&mut state, &mut rx[0]);
    assert_eq!(kicked_with(last), "disconnect.kicked");
}

// pmmp: Player::kick, Player::setGamemode, Entity::teleport, Player::transfer
#[test]
fn builtin_kick_gamemode_teleport_and_transfer_raise_their_events() {
    let mut listeners = Listeners::default();
    listeners.push(
        Priority::Normal,
        |e: &mut PlayerKick, _: &mut Context<'_>| {
            e.cancel();
        },
    );
    listeners.push(
        Priority::Normal,
        |e: &mut PlayerGameModeChange, _: &mut Context<'_>| e.cancel(),
    );
    // Coordinates are refused; a teleport to a player is sent elsewhere.
    listeners.push(
        Priority::Normal,
        |e: &mut EntityTeleport, _: &mut Context<'_>| {
            if e.target().x == 10.0 {
                e.cancel();
            } else {
                e.set_target(e.target_world(), Vec3::new(1.0, 80.0, 1.0));
            }
        },
    );
    listeners.push(
        Priority::Normal,
        |e: &mut PlayerTransfer, _: &mut Context<'_>| {
            e.cancel();
        },
    );
    let (mut state, _rx) = server_with(vec![], listeners);
    Context { state: &mut state }.op("player2").unwrap();
    let start = player(&state, 2).position();
    // PMMP's success lines come before the listeners decide.
    assert_eq!(
        ok(&mut state, Sender::Console, "kick player2"),
        ["commands.kick.success"]
    );
    ok(&mut state, Sender::Console, "ban player2");
    assert!(player(&state, 2).is_online(), "the kicks were cancelled");
    ok(&mut state, Sender::Console, "gamemode creative player2");
    assert_eq!(player(&state, 2).game_mode(), GameMode::Survival);
    ok(&mut state, Sender::Console, "tp player2 10 70 10");
    assert_eq!(player(&state, 2).position(), start);
    ok(&mut state, Sender::Console, "tp player2 player1");
    assert_eq!(player(&state, 2).position(), Vec3::new(1.0, 80.0, 1.0));
    ok(&mut state, id(2), "transferserver example.com");
    assert!(player(&state, 2).is_online(), "the transfer was cancelled");
}

#[test]
fn gamemode_changes_players_and_needs_the_permission_for_others() {
    let (mut state, mut rx) = server(vec![]);
    let player1 = crate::player::Id::new(SessionId::new(1));
    Context { state: &mut state }.set_permission(player1, permission::COMMAND_GAMEMODE_SELF, true);
    assert_eq!(
        ok(&mut state, id(1), "gamemode c"),
        ["commands.gamemode.success.self"]
    );
    assert_eq!(player(&state, 1).game_mode(), GameMode::Creative);
    assert!(is_refused(&run(&mut state, id(1), "gamemode 1 player2")));
    assert_eq!(
        fails(&mut state, id(1), "gamemode creative"),
        "The game mode of player1 stays as it is"
    );
    assert!(is_usage(&run(
        &mut state,
        Sender::Console,
        "gamemode creative"
    )));
    assert!(is_usage(&run(
        &mut state,
        Sender::Console,
        "gamemode flying @a"
    )));
    assert_eq!(
        ok(&mut state, Sender::Console, "gamemode spectator player2"),
        ["commands.gamemode.success.other"]
    );
    assert_eq!(player(&state, 2).game_mode(), GameMode::Spectator);
    let (to2, _) = chat(&mut state, &mut rx[1]);
    let changed = r#"{"rawtext":[{"translate":"gameMode.changed","with":{"rawtext":[{"text":"%createWorldScreen.gameMode.spectator"}]}}]}"#;
    assert!(
        to2.contains(&(format!("{changed}\n"), Vec::new())),
        "{to2:?}"
    );
    // BDS tells the sender too.
    let (to1, _) = chat(&mut state, &mut rx[0]);
    assert!(
        to1.iter()
            .any(|(m, _)| m.contains("%createWorldScreen.gameMode.creative")),
        "{to1:?}"
    );
}

#[test]
fn messages_to_the_players_acted_on_are_bds_raw_text() {
    let (mut state, mut rx) = with_op();
    chat(&mut state, &mut rx[0]);
    ok(&mut state, id(1), "give @s stone 3");
    ok(&mut state, id(1), "tp 10.5 70 -3.25");
    let (to1, _) = chat(&mut state, &mut rx[0]);
    let object = |key: &str, param: &str| {
        format!(
            r#"{{"rawtext":[{{"translate":"{key}","with":{{"rawtext":[{{"text":"{param}"}}]}}}}]}}"#
        )
    };
    let given = r#"{"rawtext":[{"translate":"commands.give.successRecipient","with":{"rawtext":[{"text":"minecraft:stone"},{"text":"3"}]}}]}"#;
    assert!(to1.contains(&(format!("{given}\n"), Vec::new())), "{to1:?}");
    let moved = object("commands.tp.successVictim", "10.50, 70.00, -3.25");
    assert!(to1.contains(&(format!("{moved}\n"), Vec::new())), "{to1:?}");
    let packets = {
        ok(&mut state, id(1), "give @s stone 1");
        tick(&mut state, &mut vec![], &mut TickOutput::default());
        received(&mut rx[0]).0
    };
    assert!(packets.iter().any(|p| matches!(
        p,
        Packet::Text(t) if t.text_type == 10 && !t.needs_translation && t.source_name.is_empty()
    )));
}

#[test]
fn defaultgamemode_and_difficulty_change_the_running_settings() {
    let (mut state, _rx) = with_op();
    assert!(is_refused(&run(&mut state, id(2), "difficulty hard")));
    assert_eq!(
        ok(&mut state, Sender::Console, "defaultgamemode a"),
        ["commands.defaultgamemode.success"]
    );
    assert_eq!(
        Context { state: &mut state }.default_game_mode(),
        GameMode::Adventure
    );
    assert_eq!(
        ok(&mut state, id(1), "difficulty P"),
        ["commands.difficulty.success"]
    );
    assert_eq!(
        Context { state: &mut state }.difficulty(),
        Difficulty::Peaceful
    );
    assert!(is_usage(&run(&mut state, id(1), "difficulty 4")));
    // A hardcore server stays hard.
    state.settings.hardcore = true;
    ok(&mut state, id(1), "difficulty easy");
    assert_eq!(Context { state: &mut state }.difficulty(), Difficulty::Hard);
    assert!(!Context { state: &mut state }.set_difficulty(Difficulty::Easy));
}

#[test]
fn give_fills_the_inventory_in_stacks_and_reports_what_fit() {
    let (mut state, _rx) = with_op();
    assert!(is_refused(&run(&mut state, id(2), "give player2 stone")));
    assert_eq!(
        ok(&mut state, id(1), "give @s stone 100"),
        ["commands.give.success"]
    );
    let inv = &player(&state, 1).inventory;
    let counts: Vec<u8> = (0..3).map(|i| inv.main_stack(i).unwrap().count()).collect();
    assert_eq!(counts, [64, 36, 0]);
    let packet = run(&mut state, id(1), "give player2 diamond_sword 99999");
    assert_eq!(packet, Err(super::NUM_TOO_BIG.error(&["99999", "32767"])));
    assert!(matches!(
        run(&mut state, Sender::Console, "give player2 no_such_item"),
        Err(Error::Syntax { .. })
    ));
    // The console gives others; a full inventory takes what fits.
    let out = Context { state: &mut state }
        .dispatch_command(Sender::Console, "give player2 diamond_sword 40");
    assert_eq!(lines(out), ["commands.give.success"]);
    let inv = &player(&state, 2).inventory;
    assert_eq!(
        (0..36)
            .filter(|&i| !inv.main_stack(i).unwrap().is_empty())
            .count(),
        36
    );
}

#[test]
fn clear_counts_and_takes_items_from_every_slot() {
    let (mut state, _rx) = with_op();
    let stone = crate::command::parse::Names::item("stone").unwrap();
    let stick = crate::command::parse::Names::item("stick").unwrap();
    let p2 = crate::player::Id::new(SessionId::new(2));
    let mut cx = Context { state: &mut state };
    cx.player_mut(p2)
        .unwrap()
        .inventory_mut(crate::inventory::Kind::Main)
        .add(&Stack::new(stone, 64));
    cx.player_mut(p2)
        .unwrap()
        .inventory_mut(crate::inventory::Kind::Main)
        .add(&Stack::new(stone, 10));
    cx.player_mut(p2)
        .unwrap()
        .inventory_mut(crate::inventory::Kind::Main)
        .add(&Stack::new(stick, 5));
    // Clearing oneself is everyone's; others need the operator's permission.
    assert!(is_refused(&run(&mut state, id(2), "clear player1")));
    assert_eq!(
        ok(&mut state, id(1), "clear player2 stone -1 0"),
        ["commands.clear.testing"]
    );
    assert_eq!(
        ok(&mut state, id(1), "clear player2 stone 0 70"),
        ["commands.clear.success"]
    );
    let inv = &player(&state, 2).inventory;
    let left: Vec<(Option<item::Type>, u8)> = (0..3)
        .map(|i| {
            let s = inv.main_stack(i).unwrap();
            (s.item(), s.count())
        })
        .collect();
    assert_eq!(
        left,
        [(None, 0), (Some(stone), 4), (Some(stick), 5)],
        "70 of 74 stone went, in slot order"
    );
    assert_eq!(ok(&mut state, id(2), "clear"), ["commands.clear.success"]);
    assert_eq!(
        fails(&mut state, id(2), "clear"),
        "commands.clear.failure.no.items"
    );
    assert!(is_usage(&run(&mut state, Sender::Console, "clear")));
}

#[test]
fn effect_gives_takes_and_clears_with_pmmp_bounds() {
    let (mut state, _rx) = with_op();
    assert!(is_refused(&run(&mut state, id(2), "effect @s speed")));
    assert_eq!(
        ok(&mut state, id(1), "effect @s speed 10 2"),
        ["commands.effect.success"]
    );
    let speed = player(&state, 1).effect(effect::Type::Speed).unwrap();
    assert_eq!((speed.duration_ticks(), speed.amplifier()), (200, 2));
    assert_eq!(
        fails(&mut state, id(1), "effect @s speed 1 256"),
        "commands.generic.num.tooBig"
    );
    assert_eq!(
        fails(&mut state, id(1), "effect @s speed -1"),
        "commands.generic.num.tooSmall"
    );
    assert_eq!(
        fails(&mut state, id(1), "effect @s haste 0"),
        "commands.effect.failure.notActive"
    );
    assert_eq!(
        ok(&mut state, id(1), "effect @s speed 0"),
        ["commands.effect.success.removed"]
    );
    assert_eq!(
        fails(&mut state, id(1), "effect player1 speed 0"),
        "commands.effect.failure.notActive.all"
    );
    ok(&mut state, Sender::Console, "effect player2 regeneration");
    let regeneration = player(&state, 2)
        .effect(effect::Type::Regeneration)
        .unwrap();
    assert_eq!(
        regeneration.duration_ticks(),
        effect::Type::Regeneration.default_duration_ticks()
    );
    assert_eq!(
        ok(&mut state, Sender::Console, "effect player2 clear"),
        ["commands.effect.success.removed.all"]
    );
    assert!(player(&state, 2).effects().next().is_none());
    // BDS's `hideParticles` and `infinite`.
    ok(&mut state, id(1), "effect @s resistance 600 255 true");
    let resistance = player(&state, 1).effect(effect::Type::Resistance).unwrap();
    assert!(!resistance.is_visible() && !resistance.is_infinite());
    assert_eq!(
        (resistance.duration_ticks(), resistance.amplifier()),
        (12_000, 255)
    );
    ok(&mut state, id(1), "effect @s speed 5 0 false");
    let speed = player(&state, 1).effect(effect::Type::Speed).unwrap();
    assert!(speed.is_visible());
    assert_eq!(
        ok(&mut state, id(1), "effect @s haste infinite 1 true"),
        ["commands.effect.success.infinite"]
    );
    let haste = player(&state, 1).effect(effect::Type::Haste).unwrap();
    assert!(haste.is_infinite() && !haste.is_visible());
    assert_eq!(haste.amplifier(), 1);
    ok(&mut state, id(1), "effect @s jump_boost infinite");
    let jump = player(&state, 1).effect(effect::Type::JumpBoost).unwrap();
    assert!(jump.is_infinite() && jump.is_visible());
}

#[test]
fn enchant_enchants_the_held_item_up_to_the_highest_level() {
    let (mut state, _rx) = with_op();
    assert_eq!(
        fails(&mut state, id(1), "enchant @s sharpness"),
        "commands.enchant.noItem"
    );
    let sword = crate::command::parse::Names::item("diamond_sword").unwrap();
    let book = crate::command::parse::Names::item("book").unwrap();
    let p1 = crate::player::Id::new(SessionId::new(1));
    Context { state: &mut state }
        .player_mut(p1)
        .unwrap()
        .inventory_mut(crate::inventory::Kind::Main)
        .add(&Stack::new(sword, 1));
    assert_eq!(
        fails(&mut state, id(1), "enchant @s sharpness 6"),
        "commands.generic.num.tooBig"
    );
    assert_eq!(
        ok(&mut state, id(1), "enchant @s sharpness 5"),
        ["commands.enchant.success"]
    );
    let held = player(&state, 1).inventory.held().clone();
    assert_eq!(
        held.extra().unwrap().enchantments,
        [(item::Enchantment::Sharpness, 5)]
    );
    assert!(is_refused(&run(&mut state, id(2), "enchant @s sharpness")));
    let p2 = crate::player::Id::new(SessionId::new(2));
    Context { state: &mut state }
        .player_mut(p2)
        .unwrap()
        .inventory_mut(crate::inventory::Kind::Main)
        .add(&Stack::new(book, 1));
    ok(&mut state, Sender::Console, "enchant player2 unbreaking");
    let held = player(&state, 2).inventory.held();
    assert_eq!(
        held.item().and_then(item::Type::kind),
        Some(item::Kind::EnchantedBook),
        "a book becomes an enchanted book"
    );
}

#[test]
fn kill_kills_oneself_by_default_and_others_with_permission() {
    let (mut state, _rx) = with_op();
    assert!(is_refused(&run(&mut state, id(2), "kill player1")));
    assert_eq!(
        ok(&mut state, id(2), "suicide"),
        ["commands.kill.successful"]
    );
    assert!(!player(&state, 2).is_alive());
    assert!(is_usage(&run(&mut state, Sender::Console, "kill")));
    ok(&mut state, Sender::Console, "kill player1");
    assert!(!player(&state, 1).is_alive());
}

#[test]
fn xp_adds_points_and_levels_and_takes_levels() {
    let (mut state, _rx) = with_op();
    assert!(is_refused(&run(&mut state, id(2), "xp 5")));
    assert_eq!(
        ok(&mut state, id(1), "xp 5L"),
        ["commands.xp.success.levels"]
    );
    assert_eq!(player(&state, 1).xp_level(), 5);
    assert_eq!(
        ok(&mut state, id(1), "xp -2L"),
        ["commands.xp.success.negative.levels"]
    );
    assert_eq!(player(&state, 1).xp_level(), 3);
    ok(&mut state, id(1), "xp -100L");
    assert_eq!(player(&state, 1).xp_level(), 0, "not below 0");
    assert_eq!(
        ok(&mut state, Sender::Console, "xp 7 player2"),
        ["commands.xp.success"]
    );
    assert_eq!(player(&state, 2).current_total_xp(), 7);
    assert_eq!(
        fails(&mut state, id(1), "xp -1"),
        "commands.xp.failure.widthdrawXp"
    );
    assert!(is_usage(&run(&mut state, id(1), "xp lots")));
    assert!(is_usage(&run(&mut state, Sender::Console, "xp 1")));
}

/// `experience = false`: `/xp` fails and changes nothing.
#[test]
fn xp_fails_without_experience() {
    let (mut state, _rx) = with_op();
    state.settings.experience = false;
    let off = crate::player::Error::Disabled.to_string();
    assert_eq!(fails(&mut state, id(1), "xp 5L"), off);
    assert_eq!(fails(&mut state, Sender::Console, "xp 7 player2"), off);
    assert_eq!(player(&state, 1).xp_level(), 0);
    assert_eq!(player(&state, 2).current_total_xp(), 0);
}

#[test]
fn tp_moves_players_to_players_and_positions() {
    let (mut state, _rx) = with_op();
    assert!(is_refused(&run(&mut state, id(2), "tp 0 100 0")));
    assert_eq!(
        ok(&mut state, id(1), "tp 10.5 70 -3.25"),
        ["commands.tp.success.coordinates"]
    );
    let near = |a: Vec3, b: Vec3| (a - b).length_squared() < 1e-12;
    assert!(near(
        player(&state, 1).position(),
        Vec3::new(10.5, 70.0, -3.25)
    ));
    assert_eq!(
        ok(&mut state, Sender::Console, "teleport player2 player1"),
        ["commands.tp.success"]
    );
    assert!(near(
        player(&state, 2).position(),
        Vec3::new(10.5, 70.0, -3.25)
    ));
    // Relative to the sender; heights are kept inside the world.
    ok(&mut state, id(1), "tp player2 ~1 ~1000 ~");
    assert!(near(
        player(&state, 2).position(),
        Vec3::new(11.5, 320.0, -3.25)
    ));
    assert!(is_usage(&run(&mut state, Sender::Console, "tp 0 0 0")));
    assert!(is_usage(&run(&mut state, id(1), "tp player2 @a")));
    assert_eq!(super::two_decimals(-3.256), "-3.26");
    assert_eq!(super::two_decimals(70.0), "70.00", "BDS's coordinates");
}

#[test]
fn spawnpoint_and_setworldspawn_take_the_senders_block_or_a_position() {
    let (mut state, _rx) = with_op();
    ok(&mut state, id(1), "tp 5.7 70.5 -0.5");
    let out = run(&mut state, id(1), "spawnpoint").unwrap();
    assert_eq!(
        (out.lines[0].text.as_str(), out.lines[0].params.as_slice()),
        (
            "commands.spawnpoint.success.single",
            ["player1", "5", "70", "-1"].map(str::to_owned).as_slice()
        )
    );
    assert_eq!(
        player(&state, 1).spawn(),
        Some(("world", BlockPos::new(5, 70, -1)))
    );
    ok(&mut state, Sender::Console, "spawnpoint player2 1 2 3");
    assert_eq!(
        player(&state, 2).spawn(),
        Some(("world", BlockPos::new(1, 2, 3)))
    );
    assert!(is_usage(&run(
        &mut state,
        Sender::Console,
        "spawnpoint player2"
    )));
    assert!(is_refused(&run(&mut state, id(2), "spawnpoint")));
    assert_eq!(
        ok(&mut state, id(1), "setworldspawn"),
        ["commands.setworldspawn.success"]
    );
    assert_eq!(state.worlds[0].spawn(), BlockPos::new(5, 70, -1));
    ok(&mut state, Sender::Console, "setworldspawn 8 64 8");
    assert_eq!(state.worlds[0].spawn(), BlockPos::new(8, 64, 8));
    assert_eq!(
        fails(&mut state, Sender::Console, "setworldspawn"),
        "commands.generic.invalidOrigin"
    );
}

#[test]
fn time_sets_adds_and_queries_with_a_permission_each() {
    let (mut state, _rx) = with_op();
    assert_eq!(
        ok(&mut state, Sender::Console, "time set noon"),
        ["commands.time.set"]
    );
    assert_eq!(state.worlds[0].time_ticks(), 6000);
    ok(&mut state, Sender::Console, "time add 100");
    assert_eq!(state.worlds[0].time_ticks(), 6100);
    ok(&mut state, Sender::Console, "time set -5");
    assert_eq!(state.worlds[0].time_ticks(), 0, "PMMP takes 0 at least");
    // The query reads as BDS's: the time of day, the clock and the days passed.
    ok(&mut state, Sender::Console, "time set 30000");
    for (what, key, value) in [
        ("daytime", "commands.time.query.daytime", "6000"),
        ("gametime", "commands.time.query.gametime", "30000"),
        ("day", "commands.time.query.day", "1"),
    ] {
        let out = run(&mut state, Sender::Console, &format!("time query {what}")).unwrap();
        assert_eq!(
            (out.lines[0].text.as_str(), out.lines[0].params.as_slice()),
            (key, [value.to_owned()].as_slice())
        );
    }
    assert!(is_usage(&run(&mut state, Sender::Console, "time query")));
    let player2 = crate::player::Id::new(SessionId::new(2));
    Context { state: &mut state }.set_permission(player2, permission::COMMAND_TIME_QUERY, true);
    assert_eq!(
        ok(&mut state, id(2), "time query day"),
        ["commands.time.query.day"]
    );
    assert!(is_refused(&run(&mut state, id(2), "time set day")));
}

// `/time start` and `/time stop` act on every world, each with its own permission (as PMMP TimeCommand).
#[test]
fn time_stop_and_start_stop_and_start_every_worlds_time() {
    let (mut state, _rx) = with_op();
    state.add_world(crate::world::World::new(
        "arena",
        crate::world::Kind::Temporary,
        0,
        BlockPos::new(0, 0, 0),
    ));
    let stopped = |state: &mut State| {
        Context { state }
            .worlds()
            .map(crate::world::World::is_time_stopped)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ok(&mut state, Sender::Console, "time stop"),
        ["The time stands still in every world"]
    );
    assert_eq!(stopped(&mut state), [true, true]);
    assert_eq!(
        ok(&mut state, id(1), "time start"),
        ["The time runs again in every world"]
    );
    assert_eq!(stopped(&mut state), [false, false]);
    assert!(is_refused(&run(&mut state, id(2), "time stop")));
    let player2 = crate::player::Id::new(SessionId::new(2));
    Context { state: &mut state }.set_permission(player2, permission::COMMAND_TIME_STOP, true);
    ok(&mut state, id(2), "time stop");
    assert_eq!(stopped(&mut state), [true, true]);
    assert!(is_refused(&run(&mut state, id(2), "time start")));
}

/// The console's world is the default world, also after `set_default_world` named another
/// (PMMP's `/seed` and `/setworldspawn` use `getDefaultWorld` for a sender that is no player).
#[test]
fn the_consoles_world_is_the_default_world_set_at_run_time() {
    let (mut state, _rx) = with_op();
    state.add_world(crate::world::World::new(
        "arena",
        crate::world::Kind::Temporary,
        77,
        BlockPos::new(0, 0, 0),
    ));
    let arena = state.worlds[1].id();
    Context { state: &mut state }
        .set_default_world(arena)
        .unwrap();
    let before = state.worlds[0].spawn();
    ok(&mut state, Sender::Console, "setworldspawn 1 2 3");
    assert_eq!(state.worlds[1].spawn(), BlockPos::new(1, 2, 3));
    assert_eq!(state.worlds[0].spawn(), before);
    let out = run(&mut state, Sender::Console, "seed").unwrap();
    assert_eq!(out.lines[0].params, ["77"]);
}

#[test]
fn title_shows_each_kind_and_needs_the_permission_for_others() {
    let (mut state, mut rx) = with_op();
    assert!(is_refused(&run(&mut state, id(2), "title @s title hi")));
    for line in [
        "title player2 title Hello there",
        "title player2 subtitle sub",
        "title player2 actionbar bar",
        "title player2 times 10 40 10",
        "title player2 clear",
        "title player2 reset",
    ] {
        assert_eq!(
            ok(&mut state, id(1), line),
            ["commands.title.success"],
            "{line}"
        );
    }
    let mut output = TickOutput::default();
    tick(&mut state, &mut vec![], &mut output);
    let titles: Vec<(i32, String, i32)> = received(&mut rx[1])
        .0
        .into_iter()
        .filter_map(|p| match p {
            Packet::SetTitle(t) => Some((t.action_type, t.text, t.remain_duration)),
            _ => None,
        })
        .collect();
    assert_eq!(titles.len(), 6);
    assert_eq!(titles[0].1, "Hello there");
    assert_eq!(titles[3].2, 40, "ticks");
    assert!(is_usage(&run(&mut state, id(1), "title player2 bogus")));
}

#[test]
fn tell_whispers_to_others_and_not_to_oneself() {
    let (mut state, mut rx) = with_op();
    assert_eq!(
        fails(&mut state, id(2), "tell player2 hi"),
        "commands.message.sameTarget"
    );
    assert_eq!(
        ok(&mut state, id(2), "msg player1 hello you"),
        ["commands.message.display.outgoing"]
    );
    let (to1, _) = chat(&mut state, &mut rx[0]);
    assert_eq!(
        to1,
        [
            (
                "§b§o%commands.message.display.incoming".to_owned(),
                vec!["player2".to_owned(), "hello you".to_owned()]
            ),
            (
                "§7§o[player2: %commands.message.display.outgoing]".to_owned(),
                vec!["player1".to_owned(), "hello you".to_owned()]
            )
        ],
        "the whisper, and the operator's copy"
    );
    assert_eq!(
        ok(&mut state, Sender::Console, "w @a psst"),
        ["commands.message.display.outgoing"; 2]
    );
}

#[test]
fn me_and_say_broadcast_pmmp_chat_lines() {
    let (mut state, mut rx) = with_op();
    assert!(lines(run(&mut state, id(2), "me waves")).is_empty());
    assert!(is_refused(&run(&mut state, id(2), "say hi")));
    assert!(lines(run(&mut state, Sender::Console, "say hello all")).is_empty());
    let (to1, _) = chat(&mut state, &mut rx[0]);
    assert_eq!(
        to1,
        [
            (
                "chat.type.emote".to_owned(),
                vec!["player2".to_owned(), "§rwaves".to_owned()]
            ),
            (
                "§d%chat.type.announcement".to_owned(),
                vec!["Server".to_owned(), "hello all".to_owned()]
            )
        ]
    );
    assert!(is_usage(&run(&mut state, id(2), "me")));
}

#[test]
fn list_seed_status_and_version_report_the_server() {
    let (mut state, _rx) = with_op();
    assert!(is_refused(&run(&mut state, id(2), "list")));
    assert_eq!(
        ok(&mut state, id(1), "list"),
        ["commands.players.list", "player1, player2"]
    );
    assert_eq!(ok(&mut state, id(1), "seed"), ["commands.seed.success"]);
    let version = ok(&mut state, id(2), "ver");
    assert_eq!(version.len(), 1);
    assert!(version[0].starts_with(&format!(
        "Anchor {} for Minecraft ",
        Context::ANCHOR_VERSION
    )));
    assert!(version[0].contains(Context::VERSION));
}

/// `/status` shows, in Anchor's own lines, the measured uptime, tick load and memory, the
/// threads, the players and each world's chunks, entities and block entities.
#[test]
fn status_reports_uptime_load_memory_threads_players_and_worlds() {
    let (mut state, _rx) = with_op();
    let status = ok(&mut state, Sender::Console, "status");
    // Nothing measured yet: 20 TPS in no time, memory unknown, no threads (no running server).
    assert_eq!(status[2], "TPS: 20.00 now, 20.00 average");
    assert_eq!(status[3], "Tick time: 0.00 ms now, 0.00 ms average");
    assert_eq!(status[4], "Memory: unknown");
    state.set_threads(ThreadTotals {
        network: 3,
        cpu: 9,
        console: true,
        log: true,
    });
    crate::tick::network::apply(&mut state, SessionId::ENGINE, Measure::TickTime(100_000));
    let process = Measure::Process {
        memory: 3 << 19,
        uptime_ms: 3_723_999,
    };
    crate::tick::network::apply(&mut state, SessionId::PROCESS, process);
    let mut cx = Context { state: &mut state };
    let at = cx
        .player(crate::player::Id::new(SessionId::new(1)))
        .unwrap()
        .position();
    let apple = Stack::new(item::Kind::Apple.default_type(), 1);
    cx.drop_item(crate::world::Id::DEFAULT, at, apple, None)
        .unwrap();
    state.add_world(crate::world::World::new(
        "arena",
        crate::world::Kind::Temporary,
        0,
        BlockPos::new(0, 0, 0),
    ));
    let ticks = state.tick - state.commands.started.unwrap();
    let chunks = state.worlds[0].loaded_chunks().len();
    assert!(chunks > 0, "the players hold the spawn chunks");
    assert_eq!(
        ok(&mut state, Sender::Console, "status"),
        [
            "Server status".to_owned(),
            format!("Uptime: 1:02:03 ({ticks} ticks)"),
            // 100 ms allow 10 ticks a second; the mean of 19 × 20 and 10 is 19.5.
            "TPS: 10.00 now, 19.50 average".to_owned(),
            "Tick time: 100.00 ms now, 5.00 ms average".to_owned(),
            "Memory: 1.5 MiB resident".to_owned(),
            "Threads: tick and save 2, network 3, CPU pool 9, blocking up to 8, console 1, log 1"
                .to_owned(),
            "Players: 2 of 20".to_owned(),
            format!("World \"world\": {chunks} chunks, 1 entities, 0 block entities"),
            "World \"arena\": 0 chunks, 0 entities, 0 block entities".to_owned(),
        ]
    );
    // Players see the same lines.
    assert_eq!(ok(&mut state, id(1), "status")[6], "Players: 2 of 20");
    // A player whose data is still loading is not online yet, as for `/list`.
    state.players.get_mut(&SessionId::new(2)).unwrap().phase = crate::tick::Phase::Loading;
    assert_eq!(ok(&mut state, id(1), "status")[6], "Players: 1 of 20");
    assert_eq!(
        ok(&mut state, id(1), "list")[0..2],
        ["commands.players.list", "player1"]
    );
}

/// The memory line rounds to a tenth of a MiB.
#[test]
fn status_memory_rounds_to_a_tenth_of_a_mib() {
    assert_eq!(super::server::memory_line(0), "Memory: unknown");
    assert_eq!(super::server::memory_line(1), "Memory: 0.0 MiB resident");
    assert_eq!(
        super::server::memory_line(104_858),
        "Memory: 0.1 MiB resident"
    );
    assert_eq!(
        super::server::memory_line(u64::MAX),
        "Memory: 1759218604441.5 MiB resident"
    );
}

#[test]
fn help_pages_the_usable_commands_and_describes_one() {
    let (mut state, _rx) = with_op();
    let page = ok(&mut state, id(2), "help");
    assert_eq!(page[0], "commands.help.header");
    let names: Vec<&str> = page[1..]
        .iter()
        .map(|l| l.split(' ').next().unwrap_or_default())
        .collect();
    assert_eq!(
        names,
        [
            "§2/clear",
            "§2/help",
            "§2/kill",
            "§2/me",
            "§2/tell",
            "§2/version"
        ],
        "each command's first usage line"
    );
    assert_eq!(page[5], "§2/tell <target: target> <message: message>");
    assert_eq!(ok(&mut state, id(1), "? 2").len(), 8, "7 a page");
    assert_eq!(
        ok(&mut state, id(1), "help 99").len(),
        5,
        "the last page: 39 = 5 * 7 + 4"
    );
    assert_eq!(
        ok(&mut state, Sender::Console, "help").len(),
        40,
        "one page"
    );
    assert_eq!(
        ok(&mut state, id(2), "help tell"),
        [
            "§e/tell",
            "commands.tell.description",
            "/tell <target: target> <message: message>",
            "§7Also: msg, w",
        ]
    );
    assert_eq!(
        ok(&mut state, id(2), "help kill").len(),
        4,
        "kill has suicide"
    );
    assert_eq!(ok(&mut state, id(2), "help me").len(), 3, "no other names");
    assert_eq!(
        fails(&mut state, id(2), "help stop"),
        "commands.generic.unknown"
    );
}

#[test]
fn particle_shows_a_bounded_number_of_known_particles() {
    let (mut state, _rx) = with_op();
    assert!(is_refused(&run(
        &mut state,
        id(2),
        "particle heart ~ ~ ~ 1 1 1"
    )));
    assert_eq!(
        ok(&mut state, id(1), "particle heart ~ ~2 ~ 1 1 1 5"),
        ["commands.particle.success"]
    );
    ok(
        &mut state,
        Sender::Console,
        "particle blockdust 0 70 0 0 0 0 1 255_0_0",
    );
    ok(
        &mut state,
        Sender::Console,
        "particle terrain 0 70 0 0 0 0 1 stone",
    );
    assert!(is_usage(&run(
        &mut state,
        id(1),
        "particle nope ~ ~ ~ 0 0 0"
    )));
    assert!(is_usage(&run(
        &mut state,
        id(1),
        "particle terrain ~ ~ ~ 0 0 0 1 air"
    )));
    assert_eq!(
        fails(&mut state, id(1), "particle flame ~ ~ ~ 0 0 0 5000"),
        "commands.generic.num.tooBig"
    );
    // PMMP's `sonicexplosion` is offered and shown.
    assert_eq!(
        ok(&mut state, id(1), "particle sonicexplosion ~ ~ ~ 0 0 0"),
        ["commands.particle.success"]
    );
}

#[test]
fn long_ban_and_white_lists_show_the_first_hundred_names_and_count_the_rest() {
    let (mut state, _rx) = with_op();
    let mut cx = Context { state: &mut state };
    for n in 0..150 {
        cx.add_to_whitelist(&format!("name{n:03}")).unwrap();
        cx.ban(crate::permission::BanEntry::new(&format!("banned{n:03}")))
            .unwrap();
    }
    for (line, header) in [
        ("whitelist list", "commands.allowlist.list"),
        ("banlist", "commands.banlist.players"),
    ] {
        let out = ok(&mut state, id(1), line);
        assert_eq!(out.len(), 3, "{line}");
        assert_eq!(out[0], header);
        let names: Vec<&str> = out[1].split(", ").collect();
        assert_eq!(names.len(), 100, "{line}");
        assert!(names[0].ends_with("000") && names[99].ends_with("099"));
        assert_eq!(out[2], "and 50 more");
    }
    let Ok(out) = run(&mut state, id(1), "whitelist list") else {
        panic!("whitelist list failed");
    };
    assert_eq!(
        out.lines[0].params,
        ["150", "150"],
        "the count is every name"
    );
}

#[test]
fn difficulty_names_and_numbers_come_from_one_table() {
    let (mut state, _rx) = with_op();
    for (name, expected, number) in [
        ("p", Difficulty::Peaceful, "0"),
        ("1", Difficulty::Easy, "1"),
        ("normal", Difficulty::Normal, "2"),
        ("h", Difficulty::Hard, "3"),
    ] {
        let Ok(out) = run(&mut state, id(1), &format!("difficulty {name}")) else {
            panic!("difficulty {name} failed");
        };
        assert_eq!(out.lines[0].params, [number]);
        assert_eq!(Context { state: &mut state }.difficulty(), expected);
    }
}

// pmmp: Server::shutdown
#[test]
fn stop_asks_the_tick_loop_to_end() {
    let (mut state, _rx) = with_op();
    assert!(is_refused(&run(&mut state, id(2), "stop")));
    assert!(!state.commands.stop_requested());
    assert_eq!(
        ok(&mut state, Sender::Console, "stop"),
        ["commands.stop.start"]
    );
    assert!(state.commands.stop_requested());
}

// pmmp: WorldManager::setAutoSave, WorldManager::getAutoSave
#[test]
fn save_off_skips_the_periodic_save_but_not_save_all() {
    let (mut state, _rx) = with_op();
    // The commit `op` asked for is taken and answered.
    let mut output = TickOutput::default();
    tick(&mut state, &mut vec![], &mut output);
    tick(&mut state, &mut vec![], &mut output);
    assert_eq!(
        ok(&mut state, id(1), "save-off"),
        ["commands.save.disabled"]
    );
    assert!(!Context { state: &mut state }.auto_save_enabled());
    // Far past the save interval: nothing is due while saves are off.
    state.tick += 1_000_000;
    let mut log = InputLog::default();
    assert!(save::snapshot(&mut state, &mut log, false).is_none());
    assert_eq!(ok(&mut state, id(1), "save-all"), ["commands.save.start"]);
    assert!(save::snapshot(&mut state, &mut log, false).is_some());
    let mut state = with_op().0;
    let mut output = TickOutput::default();
    tick(&mut state, &mut vec![], &mut output);
    tick(&mut state, &mut vec![], &mut output);
    ok(&mut state, Sender::Console, "save-off");
    assert_eq!(
        ok(&mut state, Sender::Console, "save-on"),
        ["commands.save.enabled"]
    );
    assert!(Context { state: &mut state }.auto_save_enabled());
    state.tick += 1_000_000;
    assert!(save::snapshot(&mut state, &mut log, false).is_some());
    assert!(is_refused(&run(&mut state, id(2), "save-all")));
}

#[test]
fn backup_is_for_operators_and_backs_up_after_the_next_save() {
    let (mut state, _rx) = with_op();
    // The commit `op` asked for is taken and answered.
    let mut output = TickOutput::default();
    tick(&mut state, &mut vec![], &mut output);
    tick(&mut state, &mut vec![], &mut output);
    assert_eq!(
        permission::COMMAND_BACKUP.default(),
        permission::Default::Op
    );
    assert!(is_refused(&run(&mut state, id(2), "backup")));
    assert!(!state.saving.backup && !state.saving.requested);
    assert_eq!(
        ok(&mut state, id(1), "backup"),
        ["Saving and backing up the world; the server log tells when the backup is made"]
    );
    let mut log = InputLog::default();
    let Some(crate::save::Request::Commit(snapshot)) = save::snapshot(&mut state, &mut log, false)
    else {
        panic!("the backup saves first");
    };
    assert!(snapshot.backup);
    ok(&mut state, Sender::Console, "backup");
    assert!(state.saving.backup);
}

#[test]
fn check_is_for_operators_one_at_a_time_and_tells_who_asked() {
    let (mut state, mut rx) = with_op();
    // The commit `op` asked for is taken and answered.
    let mut output = TickOutput::default();
    tick(&mut state, &mut vec![], &mut output);
    tick(&mut state, &mut vec![], &mut output);
    assert_eq!(permission::COMMAND_CHECK.default(), permission::Default::Op);
    assert!(is_refused(&run(&mut state, id(2), "check")));
    assert_eq!(
        ok(&mut state, id(1), "check"),
        ["Saving and checking the data files; you are told the result when it ends"]
    );
    assert_eq!(
        fails(&mut state, Sender::Console, "check"),
        "A data file check is already running"
    );
    let mut log = InputLog::default();
    let Some(crate::save::Request::Commit(snapshot)) = save::snapshot(&mut state, &mut log, false)
    else {
        panic!("the check saves first");
    };
    assert!(snapshot.check);
    received(&mut rx[0]);
    save::checked(
        &mut state,
        &crate::storage::Checked::Damaged {
            values: 9,
            damaged: 1,
            first: "table chunks: a stored value does not match its checksum".to_owned(),
        },
    );
    let (texts, _) = chat(&mut state, &mut rx[0]);
    assert_eq!(
        texts,
        [(
            "World file check found 1 damaged of 9 stored values (first: table chunks: a stored \
             value does not match its checksum); the server log has the details"
                .to_owned(),
            vec![]
        )]
    );
    // The next check may start; its result goes to the console, and a result nobody waits for
    // goes nowhere.
    ok(&mut state, Sender::Console, "check");
    save::checked(&mut state, &crate::storage::Checked::Passed { values: 9 });
    save::checked(&mut state, &crate::storage::Checked::Passed { values: 9 });
    // A player who has left is told nothing.
    assert!(save::ask_check(&mut state, id(77)));
    save::checked(
        &mut state,
        &crate::storage::Checked::Failed("why".to_owned()),
    );
    ok(&mut state, Sender::Console, "check");
}

/// `backups = false`: `/backup` refuses and asks for no save.
#[test]
fn backup_is_refused_without_backups() {
    let (mut state, _rx) = with_op();
    // The commit `op` asked for is taken and answered.
    let mut output = TickOutput::default();
    tick(&mut state, &mut vec![], &mut output);
    tick(&mut state, &mut vec![], &mut output);
    state.settings.backups = false;
    assert_eq!(
        fails(&mut state, Sender::Console, "backup"),
        "Backups are turned off on this server"
    );
    assert!(!state.saving.backup && !state.saving.requested);
}

#[test]
fn transferserver_sends_the_sending_player_only() {
    let (mut state, mut rx) = with_op();
    Context { state: &mut state }.op("player2").unwrap();
    assert_eq!(
        fails(&mut state, Sender::Console, "transferserver example.com"),
        "commands.generic.invalidOrigin"
    );
    assert_eq!(
        fails(&mut state, id(2), "transferserver example.com 70000"),
        "commands.generic.num.tooBig"
    );
    assert!(lines(run(&mut state, id(2), "transferserver example.com")).is_empty());
    let (_, last) = chat(&mut state, &mut rx[1]);
    let Some(Packet::Transfer(t)) = last else {
        panic!("{last:?}");
    };
    assert_eq!((t.address.as_str(), t.port), ("example.com", 19132));
}

/// BDS 1.26.52.3's overloads of the commands Anchor shares with it, from the `AvailableCommands`
/// the P6 bot received (`p6 api bds`, scenario `commands`): `command name=type ...`, `?` for an
/// optional parameter.
const BDS_OVERLOADS: [&str; 42] = [
    "clear player=target? itemName=enum:Item? data=int? maxCount=int?",
    "deop player=target",
    "difficulty difficulty=enum:Difficulty",
    "difficulty difficulty=int",
    "effect player=target Mode=enum:ClearEffects effect=enum:Effect?",
    "effect player=target effect=enum:Effect seconds=int? amplifier=int? hideParticles=enum:Boolean?",
    "effect player=target effect=enum:Effect Mode=enum:AddInfiniteEffect amplifier=int? hideParticles=enum:Boolean?",
    "enchant player=target enchantmentName=enum:Enchant level=int?",
    "enchant player=target enchantmentId=int level=int?",
    "gamemode gameMode=enum:GameMode player=target?",
    "gamemode gameMode=int player=target?",
    "give player=target itemName=enum:Item amount=int? data=int? components=json?",
    "kick name=target reason=message",
    "kill target=target?",
    "list",
    "me message=message",
    "op player=target",
    "say message=message",
    "setworldspawn spawnPoint=position?",
    "spawnpoint player=target? spawnPos=position?",
    "stop",
    "tell target=target message=message",
    "teleport destination=position checkForBlocks=enum:Boolean? forceProvidedPositionOnDimensionChange=enum:Boolean?",
    "teleport destination=position yRot=value? xRot=value? checkForBlocks=enum:Boolean? forceProvidedPositionOnDimensionChange=enum:Boolean?",
    "teleport destination=position facing=enum:TeleportFacing lookAtPosition=position checkForBlocks=enum:Boolean? forceProvidedPositionOnDimensionChange=enum:Boolean?",
    "teleport destination=position facing=enum:TeleportFacing lookAtEntity=target checkForBlocks=enum:Boolean? forceProvidedPositionOnDimensionChange=enum:Boolean?",
    "teleport victim=target destination=position yRot=value? xRot=value? checkForBlocks=enum:Boolean? forceProvidedPositionOnDimensionChange=enum:Boolean?",
    "teleport victim=target destination=position checkForBlocks=enum:Boolean? forceProvidedPositionOnDimensionChange=enum:Boolean?",
    "teleport victim=target destination=position facing=enum:TeleportFacing lookAtPosition=position checkForBlocks=enum:Boolean? forceProvidedPositionOnDimensionChange=enum:Boolean?",
    "teleport victim=target destination=position facing=enum:TeleportFacing lookAtEntity=target checkForBlocks=enum:Boolean? forceProvidedPositionOnDimensionChange=enum:Boolean?",
    "teleport destination=target",
    "teleport victim=target destination=target checkForBlocks=enum:Boolean? forceProvidedPositionOnDimensionChange=enum:Boolean?",
    "time mode=enum:TimeModeAdd amount=int",
    "time mode=enum:TimeModeSet amount=int",
    "time mode=enum:TimeModeSet time=enum:TimeSpec",
    "time mode=enum:TimeModeQuery time=enum:TimeQuery",
    "title player=target clear=enum:TitleClear",
    "title player=target reset=enum:TitleReset",
    "title player=target titleLocation=enum:TitleSet titleText=message",
    "title player=target times=enum:TitleTimes fadeIn=int stay=int fadeOut=int",
    "xp amount=int player=target?",
    "xp amount=suffix:l player=target?",
];

/// BDS's values of the enums of [`BDS_OVERLOADS`] (but `Item`), from the same packet.
const BDS_ENUMS: [(&str, &[&str]); 16] = [
    ("AddInfiniteEffect", &["infinite"]),
    ("Boolean", &["true", "false"]),
    ("ClearEffects", &["clear"]),
    (
        "Difficulty",
        &["normal", "peaceful", "easy", "hard", "p", "e", "n", "h"],
    ),
    (
        "Effect",
        &[
            "wither",
            "speed",
            "slowness",
            "haste",
            "mining_fatigue",
            "strength",
            "instant_health",
            "instant_damage",
            "jump_boost",
            "nausea",
            "regeneration",
            "resistance",
            "fire_resistance",
            "water_breathing",
            "invisibility",
            "blindness",
            "night_vision",
            "hunger",
            "weakness",
            "poison",
            "health_boost",
            "absorption",
            "saturation",
            "levitation",
            "fatal_poison",
            "conduit_power",
            "slow_falling",
            "bad_omen",
            "village_hero",
            "darkness",
            "trial_omen",
            "wind_charged",
            "weaving",
            "oozing",
            "infested",
            "raid_omen",
            "breath_of_the_nautilus",
        ],
    ),
    (
        "Enchant",
        &[
            "protection",
            "fire_protection",
            "feather_falling",
            "blast_protection",
            "projectile_protection",
            "thorns",
            "respiration",
            "depth_strider",
            "aqua_affinity",
            "sharpness",
            "smite",
            "bane_of_arthropods",
            "knockback",
            "fire_aspect",
            "looting",
            "efficiency",
            "silk_touch",
            "unbreaking",
            "fortune",
            "power",
            "punch",
            "flame",
            "infinity",
            "luck_of_the_sea",
            "lure",
            "frost_walker",
            "mending",
            "binding",
            "vanishing",
            "impaling",
            "riptide",
            "loyalty",
            "channeling",
            "multishot",
            "piercing",
            "quick_charge",
            "soul_speed",
            "swift_sneak",
            "wind_burst",
            "density",
            "breach",
            "lunge",
        ],
    ),
    (
        "GameMode",
        &[
            "default",
            "creative",
            "spectator",
            "survival",
            "adventure",
            "d",
            "c",
            "s",
            "a",
        ],
    ),
    ("TimeModeAdd", &["add"]),
    ("TimeModeQuery", &["query"]),
    ("TimeModeSet", &["set"]),
    ("TimeQuery", &["daytime", "gametime", "day"]),
    (
        "TimeSpec",
        &["day", "sunrise", "noon", "sunset", "night", "midnight"],
    ),
    ("TitleClear", &["clear"]),
    ("TitleReset", &["reset"]),
    ("TitleSet", &["title", "subtitle", "actionbar"]),
    ("TitleTimes", &["times"]),
];

/// The record form of the overloads of an `AvailableCommands`: `command name=type ...`.
fn overloads_of(p: &crate::protocol::packet::AvailableCommands) -> Vec<String> {
    use crate::protocol::consts::CommandArgType as T;
    let basic = [
        (T::Int, "int"),
        (T::Float, "float"),
        (T::Target, "target"),
        (T::String, "string"),
        (T::Position, "position"),
        (T::MessageRoot, "message"),
        (T::RawText, "raw_text"),
        (T::Json, "json"),
    ];
    let mut out = Vec::new();
    for c in &p.commands {
        let name = if c.name == "tp" { "teleport" } else { &c.name };
        for o in &c.overloads {
            let mut line = name.to_owned();
            for param in &o.parameters {
                let t = param.r#type;
                let index = usize::try_from(t & 0xffff).unwrap();
                let ty = if t & u32::from(CommandArg::Suffixed) != 0 {
                    format!("suffix:{}", p.suffixes[index])
                } else if t & u32::from(CommandArg::Enum) != 0 {
                    format!("enum:{}", p.enums[index].r#type)
                } else {
                    let (_, ty) = basic
                        .iter()
                        .find(|(b, _)| u32::from(*b) == t & 0xffff)
                        .unwrap_or_else(|| panic!("type {t:#x}"));
                    (*ty).to_owned()
                };
                line.push(' ');
                line.push_str(&param.name);
                line.push('=');
                line.push_str(&ty);
                if param.optional {
                    line.push('?');
                }
            }
            out.push(line);
        }
    }
    out
}

#[test]
fn shared_commands_take_the_argument_types_and_enums_bds_sends() {
    let (mut state, _rx) = server(vec![]);
    let s1 = SessionId::new(1);
    Context { state: &mut state }.op("player1").unwrap();
    let packet = available_commands(state.commands.listing(), s1, &state.players[&s1]);
    let Packet::AvailableCommands(p) = &packet else {
        panic!("{packet:?}");
    };
    let ours = overloads_of(p);
    let words = |line: &str| -> Vec<String> { line.split(' ').map(str::to_owned).collect() };
    for bds in BDS_OVERLOADS {
        let command = bds.split(' ').next().unwrap();
        assert!(
            ours.iter().any(|o| o.split(' ').next() == Some(command)),
            "{command} is shared"
        );
    }
    for line in &ours {
        let command = line.split(' ').next().unwrap();
        let bds: Vec<&&str> = BDS_OVERLOADS
            .iter()
            .filter(|b| b.split(' ').next() == Some(command))
            .collect();
        // PMMP's `/time start|stop`, which BDS does not have.
        if bds.is_empty() || line.contains("TimeModeStart") || line.contains("TimeModeStop") {
            continue;
        }
        // Each overload is one of BDS's, perhaps without optional parameters at the end. A
        // parameter BDS requires may be optional here (PMMP's `/kick` reason).
        let ours = words(line);
        let fits = bds.iter().any(|b| {
            let theirs = words(b);
            ours.len() <= theirs.len()
                && ours
                    .iter()
                    .zip(&theirs)
                    .all(|(o, t)| o.trim_end_matches('?') == t.trim_end_matches('?'))
                && theirs[ours.len()..].iter().all(|t| t.ends_with('?'))
        });
        assert!(fits, "{line} is not one of BDS's {bds:?}");
    }
    let enums: std::collections::BTreeMap<&str, Vec<&str>> = p
        .enums
        .iter()
        .map(|e| {
            let values = e
                .value_indices
                .iter()
                .map(|&i| p.enum_values[usize::try_from(i).unwrap()].as_str())
                .collect();
            (e.r#type.as_str(), values)
        })
        .collect();
    for (name, values) in &BDS_ENUMS {
        let ours = &enums[name];
        if matches!(*name, "Effect" | "Enchant") {
            // Anchor has PMMP's effects and enchantments, each under BDS's name.
            assert!(ours.iter().all(|v| values.contains(v)), "{name}: {ours:?}");
        } else {
            assert_eq!(ours, values, "{name}");
        }
    }
    let item = &enums["Item"];
    for name in [
        "minecraft:stone",
        "stone",
        "minecraft:diamond_sword",
        "diamond_sword",
    ] {
        assert!(item.contains(&name), "{name}");
    }
    let give = p.commands.iter().find(|c| c.name == "give").unwrap();
    assert_eq!(
        give.overloads[0].parameters[1].options,
        u8::from(crate::protocol::consts::ParamOption::HasSemanticConstraint),
        "BDS's item parameters"
    );
}

#[test]
fn bds_forms_of_the_shared_commands_run() {
    let (mut state, _rx) = with_op();
    let me = id(1);
    assert_eq!(
        ok(&mut state, me, "gamemode 1"),
        ["commands.gamemode.success.self"]
    );
    assert_eq!(
        ok(&mut state, me, "gamemode d"),
        ["commands.gamemode.success.self"],
        "the default mode, survival"
    );
    assert!(is_usage(&run(&mut state, me, "gamemode 4")));
    assert_eq!(
        ok(&mut state, me, "difficulty 3"),
        ["commands.difficulty.success"]
    );
    assert_eq!(Context { state: &mut state }.difficulty(), Difficulty::Hard);
    assert_eq!(ok(&mut state, me, "xp 3L"), ["commands.xp.success.levels"]);
    assert_eq!(
        ok(&mut state, me, "xp 3l @s"),
        ["commands.xp.success.levels"]
    );
    assert_eq!(ok(&mut state, me, "xp 5"), ["commands.xp.success"]);
    assert!(run(&mut state, me, "xp 3x").is_err());
    assert_eq!(
        ok(&mut state, me, "time set sunrise"),
        ["commands.time.set"]
    );
    assert_eq!(
        ok(&mut state, Sender::Console, "op player2"),
        ["commands.op.success"]
    );
    assert!(
        run(&mut state, Sender::Console, "op @a").is_err(),
        "a name, not a selector"
    );
}
