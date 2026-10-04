//! The built-in commands through the tick: what each does, its refusals (arguments, permission,
//! the console where only a player fits) and its messages.

use std::net::IpAddr;

use tokio::sync::mpsc;

use super::super::tests::{id, lines, server};
use super::super::{Error, Output, Sender, available_commands, execute};
use crate::command::{self, Command};
use crate::config::{Difficulty, GameMode};
use crate::effect;
use crate::item::{self, Stack};
use crate::math::Vec3;
use crate::permission;
use crate::protocol::consts::CommandArg;
use crate::protocol::packet::Packet;
use crate::tick::log::InputLog;
use crate::tick::queue::{Outbound, SessionId};
use crate::tick::tests::{received, tick};
use crate::tick::{Context, Output as TickOutput, State, save};
use crate::world::BlockPos;

/// Runs `line` as `sender`.
fn run(state: &mut State, sender: Sender, line: &str) -> command::Result<Output> {
    execute(state, sender, line)
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

/// Whether a failure is the command's usage.
fn is_usage(result: &command::Result<Output>) -> bool {
    matches!(result, Err(Error::Usage { .. }))
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
    let msg = super::Msg::pmmp("k", "a {%0} b {%1} {%2}");
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
    assert_eq!(names.len(), 37, "PMMP's 41 commands but 4");
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), 37);
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
        ["clear", "help", "kill", "me", "tell", "version"],
        "PMMP's defaults for players who are not operators"
    );
    Context { state: &mut state }.op("player2").unwrap();
    let packet = available_commands(state.commands.listing(), s2, &state.players[&s2]);
    let Packet::AvailableCommands(p) = &packet else {
        panic!("{packet:?}");
    };
    assert_eq!(p.commands.len(), 37);
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
            vec!["position"],
            vec!["victim", "destination"],
            vec!["victim", "position"]
        ]
    );
    let gamemode = p.commands.iter().find(|c| c.name == "gamemode").unwrap();
    let mode = &gamemode.overloads[0].parameters[0];
    assert_ne!(mode.r#type & u32::from(CommandArg::Enum), 0);
    let index = usize::try_from(mode.r#type & 0xffff).unwrap();
    assert_eq!(p.enums[index].r#type, "GameMode");
    assert_eq!(p.enums[index].value_indices.len(), 13);
    let tell = p.commands.iter().find(|c| c.name == "tell").unwrap();
    let aliases = &p.enums[usize::try_from(tell.aliases_offset).unwrap()];
    let aliases: Vec<&str> = aliases
        .value_indices
        .iter()
        .map(|&i| p.enum_values[usize::try_from(i).unwrap()].as_str())
        .collect();
    assert_eq!(aliases, ["w", "msg", "tell"]);
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
    assert_eq!(to2[0], ("§7%commands.op.message".to_owned(), vec![]));
    assert_eq!(
        ok(&mut state, Sender::Console, "deop player2"),
        ["commands.deop.success"]
    );
    assert!(!Context { state: &mut state }.is_op("player2"));
    let (to2, _) = chat(&mut state, &mut rx[1]);
    assert_eq!(to2, [("§7%commands.deop.message".to_owned(), vec![])]);
    // Player 1, an operator, got a copy of the console's result, not of its own.
    let (to1, _) = chat(&mut state, &mut rx[0]);
    assert_eq!(
        to1,
        [(
            "§7§o[CONSOLE: %commands.deop.success]".to_owned(),
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
    assert_eq!(cx.sender_name(Sender::Console), Some("CONSOLE"));
    assert_eq!(cx.sender_name(id(9)), None);
    let raw = |text: &str| crate::player::Message::Raw(text.to_owned());
    cx.send_to(id(2), raw("hi")).unwrap();
    cx.send_to(Sender::Console, raw("logged")).unwrap();
    cx.send_to(id(9), raw("nobody")).unwrap();
    cx.broadcast_command_message(id(2), "did it");
    let (to2, _) = chat(&mut state, &mut rx[1]);
    assert_eq!(to2, [("hi".to_owned(), vec![])], "no copy for the sender");
    let (to1, _) = chat(&mut state, &mut rx[0]);
    assert_eq!(to1, [("§7§o[player2: did it]".to_owned(), vec![])]);
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
        "You are banned from this server. Reason: too much spam"
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
    assert_eq!(
        kicked_with(last),
        "You are banned from this server. Reason: bye"
    );
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
        ["commands.whitelist.list", ""]
    );
    assert!(is_refused(&run(&mut state, id(2), "whitelist on")));
    assert!(is_usage(&run(&mut state, id(1), "whitelist add")));
    assert!(is_usage(&run(&mut state, id(1), "whitelist add bad/name")));
    assert_eq!(
        ok(&mut state, id(1), "whitelist add Alex"),
        ["commands.whitelist.add.success"]
    );
    assert_eq!(
        ok(&mut state, id(1), "whitelist on"),
        ["commands.whitelist.enabled"]
    );
    assert!(Context { state: &mut state }.whitelist_enabled());
    // Player 1 is an operator and stays; player 2 is not listed.
    let (_, last) = chat(&mut state, &mut rx[1]);
    assert_eq!(
        kicked_with(last),
        "Kicked by admin. Reason: Server is whitelisted"
    );
    assert!(state.players.contains_key(&SessionId::new(1)));
    assert_eq!(
        ok(&mut state, Sender::Console, "whitelist remove alex"),
        ["commands.whitelist.remove.success"]
    );
    assert_eq!(
        ok(&mut state, Sender::Console, "whitelist off"),
        ["commands.whitelist.disabled"]
    );
    assert!(!Context { state: &mut state }.whitelist_enabled());
}

#[test]
fn kick_disconnects_with_pmmp_texts() {
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
    assert_eq!(kicked_with(last), "Kicked by admin. Reason: be nice");
    assert_eq!(
        ok(&mut state, Sender::Console, "kick player1"),
        ["commands.kick.success"]
    );
    let (_, last) = chat(&mut state, &mut rx[0]);
    assert_eq!(kicked_with(last), "Kicked by admin");
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
        "Game mode change for player1 failed!"
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
        ok(&mut state, Sender::Console, "gamemode view player2"),
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
        Err(Error::Usage { .. })
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
        ok(&mut state, id(1), "clear player2 stone 0"),
        ["commands.clear.testing"]
    );
    assert_eq!(
        ok(&mut state, id(1), "clear player2 stone 70"),
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
    assert_eq!((speed.duration(), speed.amplifier()), (200, 2));
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
        regeneration.duration(),
        effect::Type::Regeneration.default_duration()
    );
    assert_eq!(
        ok(&mut state, Sender::Console, "effect player2 clear"),
        ["commands.effect.success.removed.all"]
    );
    assert!(player(&state, 2).effects().next().is_none());
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
    assert_eq!(
        ok(&mut state, id(1), "spawnpoint"),
        ["Set player1's spawn point to (5, 70, -1)"]
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
        "This command syntax can only be used as a player"
    );
}

#[test]
fn time_sets_adds_and_queries_with_a_permission_each() {
    let (mut state, _rx) = with_op();
    assert_eq!(
        ok(&mut state, Sender::Console, "time set noon"),
        ["commands.time.set"]
    );
    assert_eq!(state.worlds[0].time(), 6000);
    ok(&mut state, Sender::Console, "time add 100");
    assert_eq!(state.worlds[0].time(), 6100);
    ok(&mut state, Sender::Console, "time set -5");
    assert_eq!(state.worlds[0].time(), 0, "PMMP takes 0 at least");
    assert_eq!(
        ok(&mut state, Sender::Console, "time query"),
        ["commands.time.query"]
    );
    let player2 = crate::player::Id::new(SessionId::new(2));
    Context { state: &mut state }.set_permission(player2, permission::COMMAND_TIME_QUERY, true);
    assert_eq!(ok(&mut state, id(2), "time query"), ["commands.time.query"]);
    assert!(is_refused(&run(&mut state, id(2), "time set day")));
    assert!(is_usage(&run(&mut state, Sender::Console, "time start")));
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
                "§7§o%commands.message.display.incoming".to_owned(),
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
    let status = ok(&mut state, Sender::Console, "status");
    assert_eq!(status.len(), 4);
    assert!(status[1].starts_with("§6Uptime: §c") && status[1].ends_with(" ticks"));
    assert_eq!(status[2], "§6Players: §c2/20");
    assert!(status[3].starts_with("§6World \"world\": §c"));
    let version = ok(&mut state, id(2), "ver");
    assert_eq!(version[0], "This server is running §aAnchor§r");
    assert!(version[2].contains(Context::VERSION));
    assert_eq!(version.len(), 4);
}

#[test]
fn help_pages_the_usable_commands_and_describes_one() {
    let (mut state, _rx) = with_op();
    let page = ok(&mut state, id(2), "help");
    assert_eq!(page[0], "commands.help.header");
    assert_eq!(
        page[1..],
        [
            "§2/clear: §rClears items from player inventory",
            "§2/help: §rShows the help menu",
            "§2/kill: §rCommit suicide or kill other players",
            "§2/me: §rPerforms the specified action in chat",
            "§2/tell: §rSends a private message to the given player",
            "§2/version: §rGets the version of this server including any plugins in use",
        ]
    );
    assert_eq!(ok(&mut state, id(1), "? 2").len(), 8, "7 a page");
    assert_eq!(
        ok(&mut state, id(1), "help 99").len(),
        3,
        "the last page: 37 = 5 * 7 + 2"
    );
    assert_eq!(
        ok(&mut state, Sender::Console, "help").len(),
        38,
        "one page"
    );
    assert_eq!(
        ok(&mut state, id(2), "help tell"),
        [
            "§e--------- §rHelp: /tell§e ---------",
            "§6Description: §rSends a private message to the given player",
            "§6Usage: §r/tell <target: target> <message: message>",
            "§6Aliases: §rmsg, w",
        ]
    );
    assert_eq!(
        fails(&mut state, id(2), "help stop"),
        "Unknown command: stop. Use /help for a list of available commands."
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
        ("whitelist list", "commands.whitelist.list"),
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
    assert_eq!(ok(&mut state, id(1), "save-all"), ["Saving server data..."]);
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
fn transferserver_sends_the_sending_player_only() {
    let (mut state, mut rx) = with_op();
    Context { state: &mut state }.op("player2").unwrap();
    assert_eq!(
        fails(&mut state, Sender::Console, "transferserver example.com"),
        "This command syntax can only be used as a player"
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
