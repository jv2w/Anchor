//! Commands in the tick: parsing every parameter kind, refusals, permissions, `AvailableCommands`,
//! `CommandRequest` and console lines through the tick, the lists and the join policy.

use std::collections::BTreeMap;
use std::net::IpAddr;

use tokio::sync::mpsc;

use super::*;
use crate::command::Param;
use crate::event::Listeners;
use crate::permission::Default as By;
use crate::protocol::Uuid;
use crate::protocol::consts::{CommandArg, CommandArgType};
use crate::protocol::packet::Packet as P;
use crate::save::Request;
use crate::tick::log::{EntryKind, InputLog};
use crate::tick::queue::{Input, InputKind, Outbound, outbound_queue};
use crate::tick::tests::{
    answer, initialised, input, join, new_state, packet, radius, received, settings, tick,
};
use crate::tick::{Output as TickOutput, boundary, deliver, step};

const FLY: Permission = Permission::new("test.fly", By::Op);

pub(super) fn id(session: u64) -> Sender {
    Sender::Player(player::Id::new(SessionId::new(session)))
}

/// A command whose overload `params` answers with the debug form of what each getter gave.
fn echo(name: &str, params: &[Param]) -> Command {
    let names: Vec<String> = params.iter().map(|p| p.name().to_owned()).collect();
    Command::new(name, "echoes").overload(params, move |_, _, args| {
        let mut out = Output::new();
        for n in &names {
            let line = if let Some(v) = args.int(n) {
                format!("int {v}")
            } else if let Some(v) = args.float(n) {
                format!("float {v}")
            } else if let Some(v) = args.string(n) {
                format!("text {v}")
            } else if let Some(v) = args.bool(n) {
                format!("bool {v}")
            } else if let Some(v) = args.targets(n) {
                format!(
                    "targets {:?}",
                    v.iter().map(|i| i.session().get()).collect::<Vec<_>>()
                )
            } else if let Some(v) = args.position(n) {
                format!("position {} {} {}", v.x, v.y, v.z)
            } else if let Some(v) = args.item(n) {
                format!("item {}", v.name())
            } else if let Some(v) = args.block(n) {
                format!("block {}", v.name())
            } else if let Some(v) = args.effect(n) {
                format!("effect {v:?}")
            } else if let Some(v) = args.enchantment(n) {
                format!("enchantment {v:?}")
            } else {
                format!("absent {}", args.has(n))
            };
            out = out.line(line);
        }
        Ok(out)
    })
}

/// A server with `commands` registered and players 1 and 2 joined (not spawned).
pub(super) fn server(commands: Vec<Command>) -> (State, Vec<mpsc::Receiver<Outbound>>) {
    server_with(commands, Listeners::default())
}

/// [`server`] with `listeners`.
pub(super) fn server_with(
    commands: Vec<Command>,
    mut listeners: Listeners,
) -> (State, Vec<mpsc::Receiver<Outbound>>) {
    listeners.sort();
    let mut state = new_state(settings(|_| {}), listeners);
    for command in commands {
        state.commands.register(command, Origin::User);
    }
    state.commands.finish().unwrap();
    let mut output = TickOutput::default();
    let (a, rx1) = join(1, 0);
    let (b, rx2) = join(2, 0);
    tick(&mut state, &mut vec![a, b], &mut output);
    (state, vec![rx1, rx2])
}

pub(super) fn lines(result: command::Result<Output>) -> Vec<String> {
    result.unwrap().lines().map(str::to_owned).collect()
}

#[test]
fn every_parameter_kind_parses() {
    let (mut state, _rx) = server(vec![
        echo("n", &[Param::int("a"), Param::float("b")]),
        echo("s", &[Param::string("a"), Param::bool("b").optional()]),
        echo("t", &[Param::target("who")]),
        echo("p", &[Param::position("at")]),
        echo("m", &[Param::int("n"), Param::message("text")]),
        echo("r", &[Param::raw_text("text")]),
        echo("j", &[Param::json("data"), Param::int("after").optional()]),
        echo(
            "e",
            &[
                Param::literal("set"),
                Param::enumeration("mode", "Mode", &["Creative", "survival"]),
            ],
        ),
        echo(
            "i",
            &[
                Param::item("item"),
                Param::block("block"),
                Param::effect("effect"),
                Param::enchantment("enchantment"),
            ],
        ),
    ]);
    let mut run = |sender, line: &str| lines(execute(&mut state, sender, line));
    assert_eq!(run(Sender::Console, "n -7 2.5"), ["int -7", "float 2.5"]);
    assert_eq!(run(Sender::Console, "n 3 4"), ["int 3", "float 4"]);
    assert_eq!(
        run(Sender::Console, r#"s "a b""#),
        ["text a b", "absent false"]
    );
    assert_eq!(
        run(Sender::Console, "S word TRUE"),
        ["text word", "bool true"]
    );
    assert_eq!(run(Sender::Console, "t PLAYER2"), ["targets [2]"]);
    assert_eq!(run(Sender::Console, "t @a"), ["targets [1, 2]"]);
    assert_eq!(run(id(2), "t @s"), ["targets [2]"]);
    assert_eq!(
        run(id(1), "t @p"),
        ["targets [1]"],
        "@p includes the sender"
    );
    assert_eq!(run(Sender::Console, "t @r").len(), 1);
    // Relative to the sender's feet; the console stands on the spawn block.
    let eye = state.players[&SessionId::new(1)].position;
    let spawn = state.worlds[0].spawn();
    let mut run = |sender, line: &str| lines(execute(&mut state, sender, line));
    assert_eq!(
        run(id(1), "p ~ ~1 -3.5"),
        [format!(
            "position {} {} -3.5",
            eye.x,
            eye.y - crate::tick::survival::EYE_HEIGHT + 1.0
        )]
    );
    assert_eq!(
        run(Sender::Console, "p ~2 64 ~"),
        [format!(
            "position {} 64 {}",
            f64::from(spawn.x) + 2.0,
            f64::from(spawn.z)
        )]
    );
    assert_eq!(
        run(Sender::Console, "m 5 hello  \"world\" "),
        ["int 5", r#"text hello  "world""#]
    );
    assert_eq!(run(Sender::Console, r#"r "x" y"#), [r#"text "x" y"#]);
    assert_eq!(
        run(Sender::Console, r#"j {"a": [1, "b c"]} 9"#),
        [r#"text {"a": [1, "b c"]}"#, "int 9"]
    );
    assert_eq!(
        run(Sender::Console, "j [1,2]"),
        ["text [1,2]", "absent false"]
    );
    assert_eq!(
        run(Sender::Console, "e SET creative"),
        ["text set", "text Creative"]
    );
    assert_eq!(
        run(
            Sender::Console,
            "i minecraft:stick oak_planks speed sharpness"
        ),
        [
            "item minecraft:stick",
            "block oak_planks",
            "effect Speed",
            "enchantment Sharpness"
        ]
    );
}

#[test]
fn unmatched_players_and_selectors_show_the_clients_texts() {
    let not_found = Error::PlayerNotFound {
        name: "x".to_owned(),
    };
    assert_eq!(not_found.line().text, "commands.generic.player.notFound");
    assert_eq!(
        Error::NoTargets.line().text,
        "commands.generic.noTargetMatch"
    );
}

#[test]
fn bad_lines_get_the_clients_or_anchors_messages_and_run_nothing() {
    let (mut state, _rx) = server(vec![
        echo("n", &[Param::int("a")]),
        echo("t", &[Param::target("who")]),
    ]);
    // BDS's syntax error (P6 `syntax`, m9-facts.md): the word where the line stops fitting, empty
    // where words are missing, with up to 10 characters of the line before and after it.
    let syntax = |before: &str, word: &str, after: &str| Error::Syntax {
        before: before.to_owned(),
        word: word.to_owned(),
        after: after.to_owned(),
    };
    let usage = Error::Usage {
        usage: "/n <a: int>".to_owned(),
    };
    for (line, expected) in [
        (
            "",
            Error::Unknown {
                name: String::new(),
            },
        ),
        (
            "nope 1",
            Error::Unknown {
                name: "nope".to_owned(),
            },
        ),
        ("n", syntax("/n", "", "")),
        ("n x", syntax("/n ", "x", "")),
        ("n 1 2", syntax("/n 1 ", "2", "")),
        ("n 99999999999", syntax("/n ", "99999999999", "")),
        ("n 1.5", syntax("/n ", "1.5", "")),
        ("n abc 3 and more words", syntax("/n ", "abc", " 3 and mor")),
        ("n    1  bad tail", syntax("/n    1  ", "bad", " tail")),
        (r#"n "1"#, Error::UnclosedQuote),
        (
            "t nobody",
            Error::PlayerNotFound {
                name: "nobody".to_owned(),
            },
        ),
        ("t @e", syntax("/t ", "@e", "")),
        ("t @a[r=3]", syntax("/t ", "@a[r=3]", "")),
        ("t @s", Error::NoTargets),
    ] {
        assert_eq!(
            execute(&mut state, Sender::Console, line),
            Err(expected),
            "{line:?}"
        );
    }
    assert_eq!(
        execute(&mut state, Sender::Console, &"n 1 ".repeat(600)),
        Err(Error::TooLong)
    );
    assert_eq!(
        Error::Unknown {
            name: "nope".to_owned()
        }
        .line(),
        Line {
            text: "commands.generic.unknown".to_owned(),
            params: vec!["nope".to_owned()],
        },
        "BDS's translation"
    );
    assert_eq!(
        usage.line(),
        Line {
            text: "commands.generic.usage".to_owned(),
            params: vec!["/n <a: int>".to_owned()],
        }
    );
    assert_eq!(
        Error::Permission {
            name: "fly".to_owned()
        }
        .line(),
        Line {
            text: "commands.generic.error.permissions".to_owned(),
            params: vec!["fly".to_owned()],
        },
        "BDS's translation"
    );
}

/// Syntax errors name the word where the line stops fitting the overload that fits furthest, as
/// BDS does (P6 `syntax`, m9-facts.md).
#[test]
fn syntax_errors_name_the_word_where_the_line_stops_fitting() {
    let syntax = |before: &str, word: &str, after: &str| Error::Syntax {
        before: before.to_owned(),
        word: word.to_owned(),
        after: after.to_owned(),
    };
    assert_eq!(
        syntax("/give @s ", "no_such_item", " 3 and mor").line(),
        Line {
            text: "commands.generic.syntax".to_owned(),
            params: vec![
                "/give @s ".to_owned(),
                "no_such_item".to_owned(),
                " 3 and mor".to_owned()
            ],
        },
        "BDS's translation"
    );
    let (mut state, _rx) = server(vec![
        echo("p", &[Param::position("at")]),
        echo("e", &[Param::target("who"), Param::effect("effect")]).overload(
            &[
                Param::target("who"),
                Param::effect("effect"),
                Param::int("seconds"),
            ],
            |_, _, _| Ok(Output::new()),
        ),
    ]);
    for (line, expected) in [
        ("p 1 2", syntax("/p 1 2", "", "")),
        ("p 1 2 x", syntax("/p 1 2 ", "x", "")),
        ("p ^ ^ ^", syntax("/p ", "^", " ^ ^")),
        ("p ~inf 0 0", syntax("/p ", "~inf", " 0 0")),
        ("p 1 2 3 4", syntax("/p 1 2 3 ", "4", "")),
        // The overload that fits furthest names the word.
        ("e @s speed abc", syntax(" @s speed ", "abc", "")),
        (
            "e @s no_such_effect",
            syntax("/e @s ", "no_such_effect", ""),
        ),
    ] {
        assert_eq!(
            execute(&mut state, Sender::Console, line),
            Err(expected),
            "{line}"
        );
    }
}

// pmmp: Permissible::hasPermission, Player::hasPermission, Command::testPermissionSilent, Permissible::isPermissionSet, Player::isPermissionSet, PermissionAttachment::setPermission, PermissionAttachment::unsetPermission, Command::setPermission, Server::addOp, Server::removeOp, Server::isOp, Server::getOps, CommandSender, ConsoleCommandSender
#[test]
fn permissions_follow_defaults_operators_and_grants() {
    let fly = Command::new("fly", "flies")
        .permission(FLY)
        .overload(&[], |_, _, _| Ok("flying".into()));
    let (mut state, _rx) = server(vec![fly]);
    let player1 = player::Id::new(SessionId::new(1));
    assert_eq!(
        execute(&mut state, id(1), "fly"),
        Err(Error::Permission {
            name: "fly".to_owned()
        })
    );
    assert_eq!(
        lines(execute(&mut state, Sender::Console, "fly")),
        ["flying"]
    );
    let mut cx = Context { state: &mut state };
    let fly = cx.command("fly").unwrap();
    assert!(cx.can_use(Sender::Console, fly) && !cx.can_use(id(1), fly));
    assert!(cx.has_permission(Sender::Console, FLY));
    assert!(!cx.has_permission(Sender::Console, Permission::new("x", By::NotOp)));
    assert!(!cx.has_permission(id(1), FLY));
    assert!(cx.has_permission(id(1), Permission::new("x", By::NotOp)));
    cx.op("PLAYER1").unwrap();
    assert!(cx.is_op("player1") && cx.has_permission(id(1), FLY));
    assert_eq!(cx.ops().collect::<Vec<_>>(), ["player1"]);
    assert!(!cx.has_permission(id(1), Permission::new("x", By::NotOp)));
    // A denial beats operator status; unsetting it brings the default back.
    assert!(cx.set_permission(player1, FLY, false));
    assert!(cx.is_permission_set(player1, FLY));
    assert!(!cx.has_permission(id(1), FLY));
    assert!(cx.unset_permission(player1, FLY));
    assert!(!cx.unset_permission(player1, FLY));
    assert!(cx.has_permission(id(1), FLY));
    assert!(cx.deop("player1") && !cx.deop("player1"));
    assert!(cx.set_permission(player1, FLY, true));
    assert!(cx.has_permission(id(1), FLY));
    assert!(!cx.set_permission(player::Id::new(SessionId::new(9)), FLY, true));
    assert!(
        !cx.has_permission(id(9), Permission::new("x", By::True)),
        "a player who left"
    );
    assert_eq!(lines(execute(&mut state, id(1), "fly")), ["flying"]);
    // Grants go with the player.
    let mut output = TickOutput::default();
    tick(
        &mut state,
        &mut vec![crate::tick::tests::leave(1, 1)],
        &mut output,
    );
    assert!(state.commands.grants.is_empty());
}

// pmmp: PermissionAttachment::getPermissions, PermissionAttachment::clearPermissions, Permissible::getEffectivePermissions, Player::getEffectivePermissions
#[test]
fn grants_are_listed_and_cleared_and_effective_permissions_follow_them() {
    let fly = Command::new("fly", "flies")
        .permission(FLY)
        .overload(&[], |_, _, _| Ok("flying".into()));
    let (mut state, _rx) = server(vec![fly]);
    let player1 = player::Id::new(SessionId::new(1));
    let gone = player::Id::new(SessionId::new(9));
    let mut cx = Context { state: &mut state };
    let effective = |cx: &Context<'_>, sender: Sender| {
        cx.effective_permissions(sender)
            .find(|(p, _)| *p == FLY)
            .map(|(_, has)| has)
    };
    assert_eq!(effective(&cx, id(1)), Some(false));
    assert_eq!(effective(&cx, Sender::Console), Some(true));
    assert_eq!(
        cx.effective_permissions(id(9))
            .filter(|(_, has)| *has)
            .count(),
        0
    );
    assert!(cx.effective_permissions(id(1)).any(|(p, has)| {
        p == permission::BUILTIN[0] && has == permission::BUILTIN[0].granted_by_default(false)
    }));
    assert_eq!(cx.permission_grants(player1).count(), 0);
    let other = Permission::new("lobby.other", By::True);
    assert!(cx.set_permission(player1, FLY, true));
    assert!(cx.set_permission(player1, other, false));
    assert_eq!(
        cx.permission_grants(player1).collect::<Vec<_>>(),
        [("lobby.other", false), ("test.fly", true)]
    );
    assert_eq!(effective(&cx, id(1)), Some(true));
    assert!(cx.clear_permissions(player1));
    assert!(!cx.clear_permissions(player1), "none left");
    assert!(!cx.clear_permissions(gone));
    assert_eq!(cx.permission_grants(player1).count(), 0);
    assert_eq!(effective(&cx, id(1)), Some(false));
    assert!(state.commands.resend.contains(&SessionId::new(1)));
}

// pmmp: CommandMap::register, SimpleCommandMap::register, SimpleCommandMap::unregister, PermissionManager::addPermission, PermissionManager::getPermission, PermissionManager::getPermissions, CommandMap::getCommand, SimpleCommandMap::getCommand, SimpleCommandMap::getCommands, Command::getName, Command::getDescription, Command::getAliases, Command::setAliases, Command::getPermissions, Command::getUsage, Command::setUsage, ClosureCommand
#[test]
fn registration_keeps_user_commands_first_and_checks_the_rules() {
    let ok = |c: Command| c.overload(&[], |_, _, _| Ok(Output::new()));
    let builder = crate::Server::builder()
        .command(ok(Command::new("Spawn", "go to spawn")
            .alias("hub")
            .alias("lobby")))
        .permission(FLY)
        .without_command("me");
    let mut commands = builder.setup.commands;
    commands.register(ok(Command::new("me", "builtin")), Origin::Builtin);
    commands.register(ok(Command::new("spawn", "builtin")), Origin::Builtin);
    commands.register(
        ok(Command::new("list", "builtin").alias("hub").alias("who")),
        Origin::Builtin,
    );
    assert!(commands.finish().is_ok());
    let mut state = new_state(settings(|_| {}), Listeners::default());
    state.commands = commands;
    let cx = Context { state: &mut state };
    let spawn = cx.command("HUB").unwrap();
    assert_eq!(
        (spawn.name(), spawn.description()),
        ("spawn", "go to spawn")
    );
    assert_eq!(spawn.aliases(), ["hub", "lobby"]);
    assert!(spawn.permissions().is_empty());
    assert_eq!(spawn.usage_text(), "/spawn");
    assert!(cx.command("me").is_none(), "dropped with without_command");
    let list = cx.command("who").unwrap();
    assert_eq!(
        (list.name(), list.aliases()),
        ("list", &["who".to_owned()][..])
    );
    let first: Vec<_> = cx.commands().map(Command::name).take(3).collect();
    assert_eq!(first, ["spawn", "list", "ban"], "then Anchor's own");
    assert_eq!(cx.permission("test.fly"), Some(FLY));
    assert_eq!(
        cx.permission("anchor.command.give.self"),
        Some(permission::COMMAND_GIVE_SELF)
    );
    assert_eq!(cx.permissions().count(), permission::BUILTIN.len() + 1);
    let custom = Command::new("x", "")
        .usage("/x thing")
        .overload(&[Param::int("n")], |_, _, _| Ok(Output::new()));
    assert_eq!(custom.usage_text(), "/x thing");
    // Broken registrations fail the build with the first error.
    let fails = |c: Command| {
        let mut commands = Commands::default();
        commands.register(c, Origin::User);
        commands.finish().unwrap_err()
    };
    assert!(matches!(
        fails(Command::new("a b", "")),
        Error::InvalidName { .. }
    ));
    assert!(matches!(
        fails(ok(Command::new("", ""))),
        Error::InvalidName { .. }
    ));
    assert!(matches!(
        fails(ok(Command::new("a", "").alias("/b"))),
        Error::InvalidName { .. }
    ));
    assert!(matches!(
        fails(Command::new("a", "")),
        Error::InvalidOverload { .. }
    ));
    let handler = |_: &mut Context<'_>, _: &Sender, _: &Args| Ok(Output::new());
    for params in [
        vec![Param::int("a").optional(), Param::int("b")],
        vec![Param::message("m"), Param::int("b")],
        vec![Param::int("a"), Param::int("a")],
        vec![Param::enumeration("e", "E", &[])],
        vec![Param::enumeration("e", "E", &["a b"])],
        vec![Param::int(""); 1],
        (0..33).map(|i| Param::int(i.to_string())).collect(),
    ] {
        assert!(
            matches!(
                fails(Command::new("a", "").overload(&params, handler)),
                Error::InvalidOverload { .. }
            ),
            "{params:?}"
        );
    }
    let mut commands = Commands::default();
    commands.register(ok(Command::new("a", "").alias("b")), Origin::User);
    commands.register(ok(Command::new("b", "")), Origin::User);
    assert_eq!(
        commands.finish(),
        Err(Error::Duplicate {
            name: "b".to_owned()
        })
    );
    let mut commands = Commands::default();
    commands.add_permission(Permission::new("anchor.command.give.self", By::True));
    assert!(matches!(
        commands.finish(),
        Err(Error::ConflictingPermission { .. })
    ));
}

#[test]
fn a_description_longer_than_a_client_decodes_refuses_the_build() {
    let finish = |description: String| {
        let mut commands = Commands::default();
        let command = Command::new("a", description).overload(&[], |_, _, _| Ok(Output::new()));
        commands.register(command, Origin::User);
        commands.finish()
    };
    let max = crate::protocol::COMMAND_DESCRIPTION_MAX_LEN;
    assert_eq!(finish("x".repeat(max)), Ok(()));
    assert_eq!(
        finish("x".repeat(max + 1)),
        Err(Error::DescriptionTooLong {
            command: "a".to_owned()
        })
    );
}

#[test]
fn an_enum_type_name_reused_with_other_values_refuses_the_build() {
    let handler = |_: &mut Context<'_>, _: &Sender, _: &Args| Ok(Output::new());
    let finish = |commands: Vec<Command>| {
        let mut registered = Commands::default();
        for c in commands {
            registered.register(c, Origin::User);
        }
        registered.finish()
    };
    // Anchor's `/gamemode` names its enum `GameMode`.
    let arena = Command::new("arena", "").overload(
        &[Param::enumeration("mode", "GameMode", &["solo", "duo"])],
        handler,
    );
    assert_eq!(
        finish(vec![arena]),
        Err(Error::ConflictingEnum {
            name: "GameMode".to_owned()
        })
    );
    let a =
        Command::new("a", "").overload(&[Param::enumeration("m", "Mode", &["x", "y"])], handler);
    let b =
        Command::new("b", "").overload(&[Param::enumeration("m", "Mode", &["y", "x"])], handler);
    assert_eq!(
        finish(vec![a, b]),
        Err(Error::ConflictingEnum {
            name: "Mode".to_owned()
        })
    );
}

#[test]
fn an_enum_type_name_reused_with_the_same_values_builds() {
    let handler = |_: &mut Context<'_>, _: &Sender, _: &Args| Ok(Output::new());
    let mut commands = Commands::default();
    commands.register(
        Command::new("a", "").overload(
            &[
                Param::enumeration("on", "Boolean", &["true", "false"]),
                Param::bool("off"),
            ],
            handler,
        ),
        Origin::User,
    );
    commands.register(
        Command::new("b", "").overload(&[Param::literal("list")], handler),
        Origin::User,
    );
    assert_eq!(commands.finish(), Ok(()));
}

/// Commands by name with their parameters: name, network type, optional.
type Listed = BTreeMap<String, Vec<(String, u32, bool)>>;

/// The commands of an `AvailableCommands` packet by name, with their parameter types, and the
/// enums by type.
fn listed(packet: &Packet) -> (Listed, BTreeMap<String, Vec<String>>) {
    let Packet::AvailableCommands(p) = packet else {
        panic!("{packet:?}");
    };
    let enums = p
        .enums
        .iter()
        .map(|e| {
            let values = e
                .value_indices
                .iter()
                .map(|&i| p.enum_values[usize::try_from(i).unwrap()].clone())
                .collect();
            (e.r#type.clone(), values)
        })
        .collect();
    let commands = p
        .commands
        .iter()
        .map(|c| {
            let params = c
                .overloads
                .iter()
                .flat_map(|o| &o.parameters)
                .map(|p| (p.name.clone(), p.r#type, p.optional))
                .collect();
            (c.name.clone(), params)
        })
        .collect();
    // The packet encodes and decodes as gophertunnel's.
    let mut bytes = Vec::new();
    packet.encode(&mut bytes, 0).unwrap();
    (commands, enums)
}

#[test]
fn available_commands_list_what_each_player_may_use() {
    let fly = Command::new("fly", "flies")
        .permission(FLY)
        .alias("f")
        .overload(&[Param::bool("on").optional()], |_, _, _| Ok(Output::new()));
    let (mut state, _rx) = server(vec![
        fly,
        echo(
            "give",
            &[
                Param::target("who"),
                Param::item("item"),
                Param::int("n").optional(),
            ],
        ),
        echo(
            "mode",
            &[
                Param::enumeration("mode", "Mode", &["a", "b"]),
                Param::position("at"),
            ],
        ),
        echo("shout", &[Param::message("text")]),
    ]);
    let s1 = SessionId::new(1);
    let packet = available_commands(state.commands.listing(), s1, &state.players[&s1]);
    let (commands, enums) = listed(&packet);
    assert!(
        commands.contains_key("give") && commands.contains_key("mode"),
        "the test's commands replace Anchor's give"
    );
    assert!(!commands.contains_key("fly"), "fly needs op");
    let valid = u32::from(CommandArg::Valid);
    let enumerated = valid | u32::from(CommandArg::Enum);
    let give = &commands["give"];
    assert_eq!(
        give[0],
        (
            "who".to_owned(),
            valid | u32::from(CommandArgType::Target),
            false
        )
    );
    assert_eq!(give[1].1 & !0xffff, enumerated);
    assert_eq!(
        give[2],
        ("n".to_owned(), valid | u32::from(CommandArgType::Int), true)
    );
    assert_eq!(
        commands["mode"][1].1,
        valid | u32::from(CommandArgType::Position)
    );
    assert_eq!(
        commands["shout"][0].1,
        valid | u32::from(CommandArgType::MessageRoot),
        "68 as BDS and PMMP send it, not gophertunnel's 67"
    );
    assert_eq!(enums["Mode"], ["a", "b"]);
    assert!(enums["Item"].contains(&"stick".to_owned()));
    Context { state: &mut state }.op("player1").unwrap();
    assert!(state.commands.resend.contains(&s1));
    let packet = available_commands(state.commands.listing(), s1, &state.players[&s1]);
    let Packet::AvailableCommands(p) = &packet else {
        panic!();
    };
    let fly = p.commands.iter().find(|c| c.name == "fly").unwrap();
    let aliases = &p.enums[usize::try_from(fly.aliases_offset).unwrap()];
    assert_eq!(aliases.r#type, "flyCommandAliases");
    let (_, enums) = listed(&packet);
    assert_eq!(
        enums["flyCommandAliases"],
        ["fly", "f"],
        "BDS puts the name first"
    );
    assert_eq!(enums["Boolean"], ["true", "false"]);
    assert!(
        p.commands
            .iter()
            .filter(|c| c.name == "give" || c.name == "mode")
            .all(|c| c.aliases_offset == u32::MAX)
    );
}

#[test]
fn a_permission_change_resends_the_command_list_after_the_spawn_window() {
    let fly = Command::new("fly", "")
        .permission(FLY)
        .overload(&[], |_, _, _| Ok(Output::new()));
    let (mut state, mut rx) = server(vec![fly]);
    let mut output = TickOutput::default();
    // Player 1 got its spawn window; player 2 has not asked for a chunk radius yet.
    tick(&mut state, &mut vec![radius(1, 1, 4)], &mut output);
    received(&mut rx[0]);
    Context { state: &mut state }.op("player1").unwrap();
    Context { state: &mut state }.op("player2").unwrap();
    tick(&mut state, &mut vec![], &mut output);
    let count = |rx: &mut mpsc::Receiver<Outbound>| {
        received(rx)
            .0
            .iter()
            .filter(|p| matches!(p, P::AvailableCommands(_)))
            .count()
    };
    assert_eq!(count(&mut rx[0]), 1);
    assert_eq!(count(&mut rx[1]), 0, "its spawn window will carry the list");
    assert!(state.commands.resend.is_empty());
    tick(&mut state, &mut vec![], &mut output);
    assert_eq!(count(&mut rx[0]), 0, "sent once");
}

// pmmp: Server::addOp, Server::removeOp
#[test]
fn op_and_deop_send_the_abilities_bds_sends() {
    use crate::protocol::consts::Ability;
    let (mut state, mut rx) = server(Vec::new());
    let mut output = TickOutput::default();
    let runtime_id = state.players[&SessionId::new(1)].runtime_id;
    tick(&mut state, &mut vec![radius(1, 1, 4)], &mut output);
    tick(
        &mut state,
        &mut vec![initialised(1, 2, runtime_id)],
        &mut output,
    );
    received(&mut rx[0]);
    let abilities = |rx: &mut mpsc::Receiver<Outbound>| -> Vec<(u8, u8, u32)> {
        received(rx)
            .0
            .iter()
            .filter_map(|p| match p {
                P::UpdateAbilities(u) => Some((
                    u.ability_data.command_permissions,
                    u.ability_data.player_permissions,
                    u.ability_data.layers[0].values,
                )),
                _ => None,
            })
            .collect()
    };
    let operator = u32::from(Ability::OperatorCommands) | u32::from(Ability::Teleport);
    Context { state: &mut state }.op("PLAYER1").unwrap();
    tick(&mut state, &mut vec![], &mut output);
    let got = abilities(&mut rx[0]);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!((got[0].0, got[0].1), (1, 2), "BDS: operator");
    assert_eq!(got[0].2, 63 | operator, "BDS: survival values 255");
    Context { state: &mut state }.op("player1").unwrap();
    tick(&mut state, &mut vec![], &mut output);
    assert!(abilities(&mut rx[0]).is_empty(), "already an operator");
    assert!(Context { state: &mut state }.deop("player1"));
    tick(&mut state, &mut vec![], &mut output);
    let got = abilities(&mut rx[0]);
    assert_eq!(got, [(0, 1, 63)], "a member again");
}

pub(super) fn request(session: u64, seq: u64, line: &str) -> Input {
    let origin = CommandOrigin {
        origin: 0,
        uuid: Uuid::from_bytes([3; 16]),
        request_id: "req".to_owned(),
        player_unique_id: 0,
    };
    input(
        session,
        seq,
        packet(Packet::CommandRequest(CommandRequest {
            command_line: line.to_owned(),
            command_origin: origin,
            internal: false,
            version: String::new(),
        })),
    )
}

/// `/mark`: renames its player sender `marked`.
fn mark() -> Command {
    Command::new("mark", "").overload(&[], |cx, sender, _| {
        if let Sender::Player(id) = sender
            && let Some(p) = cx.player_mut(*id)
        {
            p.set_display_name("marked").unwrap();
        }
        Ok(Output::new())
    })
}

// pmmp: CommandEvent
#[test]
fn command_listeners_see_typed_lines_and_cancel_or_change_them() {
    use crate::event::{Command as CommandEvent, Priority};
    fn drain<T>(rx: &mut mpsc::Receiver<T>) -> Vec<T> {
        let mut out = Vec::new();
        while let Ok(x) = rx.try_recv() {
            out.push(x);
        }
        out
    }
    let (tx, mut seen) = mpsc::channel(16);
    let (mtx, mut watched) = mpsc::channel(16);
    let mut listeners = Listeners::default();
    listeners.push(
        Priority::Normal,
        move |e: &mut CommandEvent, cx: &mut Context<'_>| {
            tx.try_send((e.sender(), e.command().to_owned())).unwrap();
            match e.command() {
                "stop it" => e.cancel(),
                "rename" => e.set_command("mark".to_owned()),
                "leave" => {
                    let Sender::Player(id) = e.sender() else {
                        panic!("a player typed it");
                    };
                    cx.player_mut(id).unwrap().kick("bye").unwrap();
                    e.set_command("mark".to_owned());
                }
                _ => {}
            }
        },
    );
    listeners.push_monitor(move |e: &CommandEvent, cx: &Context<'_>| {
        let name = match e.sender() {
            Sender::Player(id) => cx.player(id).map(|p| p.display_name().to_owned()),
            Sender::Console => None,
        };
        mtx.try_send((e.command().to_owned(), name)).unwrap();
    });
    let (mut state, mut rx) = server_with(vec![mark()], listeners);
    let (p1, p2) = (
        player::Id::new(SessionId::new(1)),
        player::Id::new(SessionId::new(2)),
    );
    let name = |state: &State, s: u64| state.players[&SessionId::new(s)].display_name().to_owned();
    let outputs = |rx: &mut mpsc::Receiver<Outbound>| {
        received(rx)
            .0
            .iter()
            .filter(|p| matches!(p, P::CommandOutput(_)))
            .count()
    };
    // Cancelled: nothing runs and nothing answers.
    let mut output = TickOutput::default();
    tick(
        &mut state,
        &mut vec![request(1, 10, "/stop it")],
        &mut output,
    );
    assert_eq!(outputs(&mut rx[0]), 0);
    assert_eq!(name(&state, 1), "player1");
    assert_eq!(
        drain(&mut seen),
        [(Sender::Player(p1), "stop it".to_owned())]
    );
    assert!(drain(&mut watched).is_empty(), "nothing ran");
    // The listener's line runs; the monitor sees it after it ran.
    tick(
        &mut state,
        &mut vec![request(1, 11, "/rename")],
        &mut output,
    );
    assert_eq!(outputs(&mut rx[0]), 1);
    assert_eq!(name(&state, 1), "marked");
    assert_eq!(
        drain(&mut watched),
        [("mark".to_owned(), Some("marked".to_owned()))]
    );
    drain(&mut seen);
    // The console and a chat line ask too; a direct dispatch does not.
    console(&mut state, " /mark ");
    state.players.get_mut(&p2.session()).unwrap().phase = Phase::Spawned;
    Context { state: &mut state }.chat_as(p2, "/mark");
    assert_eq!(name(&state, 2), "marked");
    Context { state: &mut state }
        .dispatch_command(id(1), "mark")
        .unwrap();
    assert_eq!(
        drain(&mut seen),
        [
            (Sender::Console, "mark".to_owned()),
            (Sender::Player(p2), "mark".to_owned())
        ]
    );
    // A listener that makes its sender leave cancels the command.
    state
        .players
        .get_mut(&p1.session())
        .unwrap()
        .set_display_name("p1")
        .unwrap();
    assert!(typed(&mut state, id(1), "leave").is_none());
    assert_eq!(name(&state, 1), "p1");
}

// pmmp: Server::dispatchCommand, CommandMap::dispatch, SimpleCommandMap::dispatch
#[test]
fn a_command_request_answers_with_command_output() {
    let hello = Command::new("hello", "").overload(&[Param::target("who")], |cx, sender, args| {
        let who = args.targets("who").unwrap_or_default().len();
        // A handler can run other commands; the same one cannot run itself.
        let again = cx.dispatch_command(*sender, "hello @s");
        assert!(matches!(again, Err(Error::Failed { .. })));
        Ok(Output::new()
            .line(format!("hello {who}"))
            .translated("commands.op.success", &["x"]))
    });
    let (mut state, mut rx) = server(vec![hello]);
    let mut output = TickOutput::default();
    tick(
        &mut state,
        &mut vec![
            request(1, 1, "/hello @a"),
            request(1, 2, "/nope"),
            request(1, 3, "hello @a"),
            request(1, 4, "/hello @a"),
        ],
        &mut output,
    );
    let outputs: Vec<_> = received(&mut rx[0])
        .0
        .into_iter()
        .filter_map(|p| match p {
            P::CommandOutput(o) => Some(o),
            _ => None,
        })
        .collect();
    assert_eq!(
        outputs.len(),
        2,
        "a line without '/' and a third request in a tick are ignored"
    );
    let ok = &outputs[0];
    assert_eq!(ok.command_origin.request_id, "req");
    assert_eq!((ok.output_type, ok.success_count), (3, 1));
    assert_eq!(
        ok.output_messages
            .iter()
            .map(|m| (m.success, m.message.as_str(), m.parameters.clone()))
            .collect::<Vec<_>>(),
        [
            (true, "hello 2", vec![]),
            (true, "commands.op.success", vec!["x".to_owned()])
        ]
    );
    let failed = &outputs[1];
    assert_eq!(failed.success_count, 0);
    assert!(!failed.output_messages[0].success);
    assert_eq!(
        (
            failed.output_messages[0].message.as_str(),
            &failed.output_messages[0].parameters[..]
        ),
        ("commands.generic.unknown", &["nope".to_owned()][..])
    );
    // The handler is back: the next tick runs it again.
    tick(
        &mut state,
        &mut vec![request(1, 5, "/hello player2")],
        &mut output,
    );
    assert!(
        received(&mut rx[0])
            .0
            .iter()
            .any(|p| matches!(p, P::CommandOutput(_)))
    );
    assert_eq!(
        Context { state: &mut state }.dispatch_command(Sender::Console, "hello @s"),
        Err(Error::NoTargets)
    );
}

/// A command that makes its argument an operator, as `/op` will.
fn op_command() -> Command {
    Command::new("op", "").overload(&[Param::string("name")], |cx, _, args| {
        cx.op(args.string("name").unwrap_or_default())
            .map_err(|e| Error::failed(e.to_string()))?;
        Ok(Output::new())
    })
}

/// Runs `ticks` of inputs through `step` with one input log, as the tick thread does.
fn run_logged(state: &mut State, ticks: Vec<Vec<Input>>) -> (InputLog, Vec<u64>) {
    let pool = crate::runtime::cpu_pool(1).unwrap();
    let mut log = InputLog::starting_at(state.tick);
    let mut output = TickOutput::default();
    let mut answers = Vec::new();
    let mut hashes = Vec::new();
    for mut batch in ticks {
        step(
            state,
            &mut batch,
            &mut answers,
            &pool,
            &mut output,
            &mut log,
        );
        deliver(state, &mut output, &mut log);
        boundary(state, &mut output, &mut log);
        // Commits stay unanswered, so the log keeps every tick from the start.
        let requests = output
            .requests
            .drain(..)
            .filter(|r| !matches!(r, Request::Commit(_)))
            .collect();
        answers = answer(state, requests);
        hashes.push(state.hash());
    }
    assert!(pool.shutdown());
    (log, hashes)
}

// pmmp: ConsoleCommandSender
#[test]
fn console_lines_run_in_the_tick_and_replay_from_the_input_log() {
    let ticks = || {
        let (j, rx) = join(1, 0);
        let mut joined = j;
        if let InputKind::Join { identity, .. } = &mut joined.kind {
            identity.address = Some(IpAddr::from([192, 0, 2, 7]));
        }
        let console = |seq, line: &str| input(u64::MAX, seq, InputKind::Console(line.to_owned()));
        (
            vec![
                vec![joined, console(0, "/op Player1")],
                vec![console(1, "op \"bad")],
                vec![],
            ],
            rx,
        )
    };
    let mut original = new_state(settings(|_| {}), Listeners::default());
    original.commands.register(op_command(), Origin::User);
    let (inputs, _rx) = ticks();
    let (log, hashes) = run_logged(&mut original, inputs);
    assert!(original.commands.access.is_op("player1"));
    let decoded = InputLog::decode(&log.encode().unwrap()).unwrap();
    let ticks_logged = decoded.ticks();
    assert_eq!(ticks_logged, log.ticks());
    let console: Vec<_> = ticks_logged
        .iter()
        .flat_map(|t| &t.inputs)
        .filter_map(|e| match &e.kind {
            EntryKind::Console(line) => Some((e.session, line.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        console,
        [
            (SessionId::CONSOLE, "/op Player1"),
            (SessionId::CONSOLE, "op \"bad")
        ]
    );
    let joined = ticks_logged[0].inputs.iter().find_map(|e| match &e.kind {
        EntryKind::Join(identity) => Some(identity.address),
        _ => None,
    });
    assert_eq!(joined, Some(Some(IpAddr::from([192, 0, 2, 7]))));
    // A replay of the logged inputs reaches the same states.
    let mut replay = new_state(settings(|_| {}), Listeners::default());
    replay.commands.register(op_command(), Origin::User);
    let mut keep = Vec::new();
    let inputs = ticks_logged
        .iter()
        .map(|t| {
            t.inputs
                .iter()
                .map(|e| {
                    let kind = match &e.kind {
                        EntryKind::Join(identity) => {
                            let (outbound, rx) = outbound_queue();
                            keep.push(rx);
                            InputKind::Join {
                                identity: identity.clone(),
                                outbound,
                            }
                        }
                        EntryKind::Packet(p) => packet(p.clone()),
                        EntryKind::Leave => InputKind::Leave,
                        EntryKind::Console(line) => InputKind::Console(line.clone()),
                        EntryKind::Job(outcome) => InputKind::Job(outcome.clone()),
                        EntryKind::Measure(measure) => InputKind::Measure(*measure),
                    };
                    Input {
                        session: e.session,
                        seq: e.seq,
                        kind,
                    }
                })
                .collect()
        })
        .collect();
    assert_eq!(run_logged(&mut replay, inputs).1, hashes);
}

// pmmp: BanList::addBan, BanList::add, Server::getNameBans, Server::getIPBans, BanList::remove, BanList::isBanned, BanList::getEntry, BanList::getEntries, Server::addWhitelist, Server::removeWhitelist, Server::isWhitelisted, Server::getWhitelisted, Server::hasWhitelist
#[test]
fn list_changes_are_idempotent_and_go_into_the_next_snapshot() {
    let ip = IpAddr::from([10, 1, 2, 3]);
    let change = |state: &mut State, times: usize| {
        let mut cx = Context { state };
        for _ in 0..times {
            cx.op("Steve").unwrap();
            cx.ban(
                BanEntry::new("Griefer")
                    .with_reason("tnt")
                    .with_expires(500),
            )
            .unwrap();
            cx.ban(BanEntry::ip(ip).with_source("console")).unwrap();
            cx.add_to_whitelist("Alex").unwrap();
        }
    };
    let (mut once, _rx) = server(vec![]);
    change(&mut once, 1);
    let (mut twice, _rx2) = server(vec![]);
    change(&mut twice, 2);
    assert_eq!(
        once.hash(),
        twice.hash(),
        "the same changes twice are the changes once"
    );
    let mut cx = Context { state: &mut once };
    assert!(cx.is_banned("GRIEFER") && cx.is_ip_banned(ip));
    let entry = cx.ban_entry("griefer").unwrap();
    assert_eq!(
        (entry.reason(), entry.created(), entry.expires()),
        ("tnt", 1, Some(500))
    );
    assert_eq!(cx.ip_ban_entry(ip).map(BanEntry::source), Some("console"));
    assert_eq!(cx.bans().count() + cx.ip_bans().count(), 2);
    assert!(!cx.whitelist_enabled() && cx.is_whitelisted("anyone"));
    cx.set_whitelist_enabled(true);
    assert!(cx.is_whitelisted("ALEX") && cx.is_whitelisted("steve") && !cx.is_whitelisted("bob"));
    assert_eq!(cx.whitelist().collect::<Vec<_>>(), ["alex"]);
    assert_eq!(
        cx.ban(BanEntry::new("")),
        Err(permission::Error::InvalidName)
    );
    assert_eq!(
        cx.add_to_whitelist(&"x".repeat(65)),
        Err(permission::Error::InvalidName)
    );
    assert!(cx.state.saving.requested);
    let mut output = TickOutput::default();
    tick(&mut once, &mut vec![], &mut output);
    let commit = output.answers.is_empty();
    assert!(!commit, "the change asked for a commit");
    // The snapshot carried every changed entry once; later changes go to the next one.
    let mut cx = Context { state: &mut once };
    assert!(cx.pardon("griefer") && !cx.pardon("griefer"));
    assert!(cx.pardon_ip(ip) && cx.remove_from_whitelist("alex") && cx.deop("steve"));
    let changes = once.commands.take_access_changes();
    assert_eq!(changes.len(), 4);
    assert!(changes.iter().all(|(_, w)| *w == AccessWrite::Delete));
    // An ended ban no longer counts, and the next ban list change drops it.
    let (mut state, _rx) = server(vec![]);
    let mut cx = Context { state: &mut state };
    cx.ban(BanEntry::new("old").with_expires(2)).unwrap();
    cx.state.tick = 2;
    assert!(!cx.is_banned("old") && cx.bans().count() == 0);
    cx.ban(BanEntry::new("new")).unwrap();
    let names: Vec<_> = state
        .commands
        .take_access_changes()
        .into_iter()
        .map(|(k, w)| (k.name, w == AccessWrite::Delete))
        .collect();
    assert_eq!(names, [("new".to_owned(), false), ("old".to_owned(), true)]);
}

#[test]
fn a_snapshot_holds_the_list_changes() {
    let (mut state, _rx) = server(vec![]);
    Context { state: &mut state }.op("steve").unwrap();
    let mut log = InputLog::default();
    let mut output = TickOutput::default();
    boundary(&mut state, &mut output, &mut log);
    let [Request::Commit(snapshot)] = &output.requests[..] else {
        panic!("{:?}", output.requests);
    };
    assert_eq!(snapshot.access.len(), 1);
    assert_eq!(snapshot.access[0].1, AccessWrite::Mark);
    assert!(state.commands.take_access_changes().is_empty());
}

#[test]
fn joins_are_refused_in_pmmp_order_with_the_clients_or_anchors_messages() {
    let (mut state, _rx) = server(vec![]);
    let mut someone = state.players[&SessionId::new(1)].identity.clone();
    someone.name = "Someone".to_owned();
    someone.address = Some(IpAddr::from([10, 0, 0, 9]));
    assert_eq!(admit(&state, &someone), Ok(()));
    let mut cx = Context { state: &mut state };
    cx.ban(BanEntry::ip(IpAddr::from([10, 0, 0, 9]))).unwrap();
    assert_eq!(
        admit(cx.state, &someone),
        Err(Refusal::IpBanned(String::new()))
    );
    let mut cx = Context { state: &mut state };
    cx.ban(BanEntry::new("someone").with_reason("spam"))
        .unwrap();
    assert_eq!(
        admit(cx.state, &someone),
        Err(Refusal::Banned("spam".to_owned()))
    );
    let mut cx = Context { state: &mut state };
    cx.set_whitelist_enabled(true);
    assert_eq!(admit(cx.state, &someone), Err(Refusal::NotWhitelisted));
    let mut cx = Context { state: &mut state };
    cx.op("someone").unwrap();
    assert_eq!(
        admit(cx.state, &someone),
        Err(Refusal::Banned("spam".to_owned())),
        "ops pass the whitelist"
    );
    state.settings.max_players = 2;
    assert_eq!(admit(&state, &someone), Err(Refusal::Full));
    assert_eq!(
        Refusal::Banned(String::new()).message(),
        "This server has banned you"
    );
    assert_eq!(
        Refusal::Banned("spam".to_owned()).message(),
        "This server has banned you: spam"
    );
    assert_eq!(
        Refusal::IpBanned(String::new()).message(),
        "This server has banned your address"
    );
    assert_eq!(
        Refusal::NotWhitelisted.message(),
        "disconnectionScreen.notAllowed"
    );
    assert!(matches!(
        Refusal::Full.packet(),
        P::PlayStatus(PlayStatus { status: 7 })
    ));
    let P::Disconnect(d) = Refusal::NotWhitelisted.packet() else {
        panic!();
    };
    assert_eq!(
        (d.reason, d.message.as_str()),
        (45, "disconnectionScreen.notAllowed")
    );
}

#[test]
fn random_and_nearest_targets_are_deterministic() {
    let pick = |runs: u64| {
        let (mut state, _rx) = server(vec![echo("t", &[Param::target("who")])]);
        state.commands.runs = runs;
        lines(execute(&mut state, Sender::Console, "t @r"))
    };
    for runs in 0..8 {
        assert_eq!(pick(runs), pick(runs));
    }
    let picks: std::collections::BTreeSet<_> = (0..32).map(pick).collect();
    assert_eq!(picks.len(), 2, "both players get picked");
    let (mut state, _rx) = server(vec![echo("t", &[Param::target("who")])]);
    let far = state.players.get_mut(&SessionId::new(1)).unwrap();
    far.position.x += 100.0;
    assert_eq!(
        lines(execute(&mut state, Sender::Console, "t @p")),
        ["targets [2]"]
    );
}

#[test]
fn command_results_are_bounded_in_lines_parameters_and_bytes() {
    let many = |n: usize| (0..n).fold(Output::new(), |out, i| out.line(format!("line {i}")));
    let (success, shown_lines) = shown(&Ok(many(100)));
    assert!(success);
    assert_eq!(shown_lines.len(), MAX_OUTPUT_LINES);
    assert_eq!(shown_lines[62].text, "line 62");
    assert_eq!(shown_lines[63].text, "... 37 more lines left out");
    assert_eq!(shown(&Ok(many(MAX_OUTPUT_LINES))).1.len(), MAX_OUTPUT_LINES);
    // A text past one client text is cut at a character boundary; the lines after it go.
    let wide = "\u{e9}".repeat(MAX_TEXT);
    let (_, shown_lines) = shown(&Ok(Output::new().line(wide).line("after")));
    assert_eq!(shown_lines[0].text.len(), MAX_TEXT);
    assert_eq!(shown_lines[1].text, "... 1 more lines left out");
    let params: Vec<String> = (0..40).map(|n| n.to_string()).collect();
    let params: Vec<&str> = params.iter().map(String::as_str).collect();
    let (_, shown_lines) = shown(&Ok(Output::new().translated("key", &params)));
    assert_eq!(shown_lines[0].params.len(), MAX_OUTPUT_PARAMS);
    let (success, shown_lines) = shown(&Err(Error::failed("x".repeat(MAX_TEXT + 10))));
    assert!(!success);
    assert_eq!(shown_lines.len(), 1);
    assert_eq!(shown_lines[0].text.len(), MAX_TEXT);
    // Through the tick: the player's CommandOutput holds the bounded lines.
    let spam = Command::new("spam", "").overload(&[], move |_, _, _| Ok(many(5000)));
    let (mut state, mut rx) = server(vec![spam]);
    tick(
        &mut state,
        &mut vec![request(1, 1, "/spam")],
        &mut TickOutput::default(),
    );
    let sent: Vec<usize> = received(&mut rx[0])
        .0
        .into_iter()
        .filter_map(|p| match p {
            P::CommandOutput(o) => Some(o.output_messages.len()),
            _ => None,
        })
        .collect();
    assert_eq!(sent, [MAX_OUTPUT_LINES]);
}

#[test]
fn available_commands_carry_only_the_enums_of_the_commands_a_player_may_use() {
    fn shared<T: Sync>(_: &T) {}
    let secret = Command::new("secret", "")
        .permission(FLY)
        .alias("hidden")
        .overload(
            &[Param::enumeration("what", "Secret", &["x", "y"])],
            |_, _, _| Ok(Output::new()),
        );
    let (mut state, _rx) = server(vec![secret]);
    let s1 = SessionId::new(1);
    let listing = state.commands.listing();
    shared(&listing);
    let (commands, enums) = listed(&available_commands(listing, s1, &state.players[&s1]));
    assert!(!commands.contains_key("secret"));
    assert!(!enums.contains_key("Secret") && !enums.contains_key("secretCommandAliases"));
    assert!(!enums["CommandName"].contains(&"hidden".to_owned()));
    Context { state: &mut state }.op("player1").unwrap();
    let packet = available_commands(state.commands.listing(), s1, &state.players[&s1]);
    let (commands, enums) = listed(&packet);
    assert!(commands.contains_key("secret"));
    assert_eq!(enums["Secret"], ["x", "y"]);
    assert_eq!(enums["secretCommandAliases"], ["secret", "hidden"]);
    // The tables are renumbered for the player: each value once, every one used.
    let Packet::AvailableCommands(p) = &packet else {
        panic!();
    };
    let distinct: std::collections::BTreeSet<&String> = p.enum_values.iter().collect();
    assert_eq!(distinct.len(), p.enum_values.len());
    let used: std::collections::BTreeSet<u32> = p
        .enums
        .iter()
        .flat_map(|e| e.value_indices.iter().copied())
        .collect();
    assert_eq!(used.len(), p.enum_values.len(), "every value is used");
}

#[test]
fn every_name_and_alias_reaches_the_client_as_bds_lists_them() {
    let (mut state, _rx) = server(Vec::new());
    let s1 = SessionId::new(1);
    let packet =
        |state: &State| available_commands(state.commands.listing(), s1, &state.players[&s1]);
    let (commands, enums) = listed(&packet(&state));
    let typed = &enums["CommandName"];
    for name in ["tell", "w", "msg", "kill", "suicide", "me", "help", "?"] {
        assert!(typed.contains(&name.to_owned()), "{name}");
    }
    assert!(!typed.contains(&"op".to_owned()), "a member may not use op");
    assert!(
        !commands.contains_key("help"),
        "the client has its own help"
    );
    assert_eq!(enums["tellCommandAliases"], ["tell", "w", "msg"]);
    assert_eq!(enums["killCommandAliases"], ["kill", "suicide"]);
    Context { state: &mut state }.op("player1").unwrap();
    let sent = packet(&state);
    let (commands, enums) = listed(&sent);
    let Packet::AvailableCommands(p) = &sent else {
        panic!();
    };
    let cx = Context { state: &mut state };
    let usable: Vec<&Command> = cx.commands().filter(|c| cx.can_use(id(1), c)).collect();
    assert!(usable.iter().any(|c| c.name() == "op"));
    let mut names = Vec::new();
    for c in &usable {
        names.push(c.name().to_owned());
        names.extend(c.aliases().iter().cloned());
        if c.name() == "help" {
            continue;
        }
        let net = p
            .commands
            .iter()
            .find(|n| n.name == c.name())
            .unwrap_or_else(|| panic!("{}", c.name()));
        if c.aliases().is_empty() {
            assert_eq!(net.aliases_offset, u32::MAX, "{}", c.name());
            continue;
        }
        let aliases = &p.enums[usize::try_from(net.aliases_offset).unwrap()];
        let enum_name = format!("{}CommandAliases", c.name());
        assert_eq!(aliases.r#type, enum_name);
        let mut expected = vec![c.name().to_owned()];
        expected.extend(c.aliases().iter().cloned());
        assert_eq!(enums[&enum_name], expected);
    }
    assert!(!commands.contains_key("help"));
    assert_eq!(
        enums["CommandName"], names,
        "every name the operator may type, once"
    );
    assert_eq!(p.commands.len(), usable.len() - 1, "all but help");
}

#[test]
fn a_number_suffix_is_ascii_letters() {
    let handler = |_: &mut Context<'_>, _: &Sender, _: &Args| Ok(Output::new());
    for (suffix, fine) in [("l", true), ("ticks", true), ("", false), ("1", false)] {
        let mut commands = Commands::default();
        commands.register(
            Command::new("a", "").overload(&[Param::suffixed_int("n", suffix)], handler),
            Origin::User,
        );
        assert_eq!(commands.finish().is_ok(), fine, "{suffix:?}");
    }
}

#[test]
fn a_parameter_may_not_take_the_command_name_enum() {
    let handler = |_: &mut Context<'_>, _: &Sender, _: &Args| Ok(Output::new());
    let mut commands = Commands::default();
    commands.register(
        Command::new("a", "").overload(&[Param::enumeration("x", "CommandName", &["a"])], handler),
        Origin::User,
    );
    assert_eq!(
        commands.finish(),
        Err(Error::ConflictingEnum {
            name: "CommandName".to_owned()
        })
    );
}
