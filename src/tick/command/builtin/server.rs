//! Commands on the server and its worlds: `/help`, `/list`, `/me`, `/say`, `/seed`,
//! `/setworldspawn`, `/defaultgamemode`, `/difficulty`, `/time`, `/particle`, `/save-all`,
//! `/save-off`, `/save-on`, `/status`, `/stop` and `/version` (PMMP's classes of the same names).

use tracing::info;

use super::{
    Msg, PLAYER_USER_ONLY, announce, display_name_of, game_mode_name, in_world, infallible,
    require, usage,
};
use crate::command::{Args, Command, Error, Output, Param, Result, Sender};
use crate::config::{Difficulty, GameMode};
use crate::item::{self, Kind as ItemKind};
use crate::math::Vec3;
use crate::permission;
use crate::player::{Message, Player};
use crate::protocol::Rgba;
use crate::protocol::packet::PROTOCOL_VERSION;
use crate::random::{self, Random};
use crate::tick::Context;
use crate::tick::queue::SessionId;
use crate::world::{self, BlockPos, Particle};

/// Adds the commands of this module.
pub(super) fn add(commands: &mut Vec<Command>) {
    commands.extend([
        Command::new("help", "Shows the help menu")
            .alias("?")
            .permission(permission::COMMAND_HELP)
            .overload(&[Param::int("page").optional()], help)
            .overload(&[Param::string("command")], help),
        Command::new("list", "Lists all online players")
            .permission(permission::COMMAND_LIST)
            .overload(&[], infallible(list)),
        Command::new("me", "Performs the specified action in chat")
            .permission(permission::COMMAND_ME)
            .overload(&[Param::message("action")], me),
        Command::new("say", "Broadcasts the given message as the sender")
            .permission(permission::COMMAND_SAY)
            .overload(&[Param::message("message")], say),
        Command::new("seed", "Shows the world seed")
            .permission(permission::COMMAND_SEED)
            .overload(&[], infallible(seed)),
        Command::new(
            "setworldspawn",
            "Sets a world's spawn point. If no coordinates are specified, the player's coordinates will be used.",
        )
        .permission(permission::COMMAND_SETWORLDSPAWN)
        .overload(&[], setworldspawn)
        .overload(&[Param::position("spawnPoint")], setworldspawn),
        Command::new("defaultgamemode", "Set the default gamemode")
            .permission(permission::COMMAND_DEFAULTGAMEMODE)
            .overload(
                &[Param::enumeration(
                    "gameMode",
                    "GameMode",
                    &super::game_mode_names(),
                )],
                defaultgamemode,
            ),
        Command::new("difficulty", "Sets the game difficulty")
            .permission(permission::COMMAND_DIFFICULTY)
            .overload(
                &[Param::enumeration(
                    "difficulty",
                    "Difficulty",
                    &DIFFICULTIES.map(|(name, _)| name),
                )],
                difficulty,
            ),
        Command::new("time", "Changes the time on each world")
            .permission(permission::COMMAND_TIME_ADD)
            .permission(permission::COMMAND_TIME_SET)
            .permission(permission::COMMAND_TIME_QUERY)
            .overload(
                &[
                    Param::literal("set"),
                    Param::enumeration("time", "TimeSpec", &TIME_NAMES),
                ],
                time,
            )
            .overload(&[Param::literal("set"), Param::int("amount")], time)
            .overload(&[Param::literal("add"), Param::int("amount")], time)
            .overload(&[Param::literal("query")], time),
        Command::new("particle", "Adds particles to a world")
            .permission(permission::COMMAND_PARTICLE)
            .overload(
                &[
                    Param::enumeration("name", "ParticleName", &PARTICLE_NAMES),
                    Param::position("position"),
                    Param::float("xd"),
                    Param::float("yd"),
                    Param::float("zd"),
                    Param::int("count").optional(),
                    Param::string("data").optional(),
                ],
                particle,
            ),
        Command::new("save-all", "Saves the server to disk")
            .permission(permission::COMMAND_SAVE_PERFORM)
            .overload(&[], infallible(save_all)),
        Command::new("save-off", "Disables server autosaving")
            .permission(permission::COMMAND_SAVE_DISABLE)
            .overload(&[], infallible(save_off)),
        Command::new("save-on", "Enables server autosaving")
            .permission(permission::COMMAND_SAVE_ENABLE)
            .overload(&[], infallible(save_on)),
        Command::new("status", "Reads back the server's performance.")
            .permission(permission::COMMAND_STATUS)
            .overload(&[], infallible(status)),
        Command::new("stop", "Stops the server")
            .permission(permission::COMMAND_STOP)
            .overload(&[], infallible(stop)),
        Command::new(
            "version",
            "Gets the version of this server including any plugins in use",
        )
        .alias("ver")
        .alias("about")
        .permission(permission::COMMAND_VERSION)
        .overload(&[], infallible(version)),
    ]);
}

const HELP_HEADER: Msg = Msg::client("commands.help.header");
const HELP_COMMAND_HEADER: Msg = Msg::pmmp(
    "pocketmine.command.help.specificCommand.header",
    "§e--------- §rHelp: /{%0}§e ---------",
);
const HELP_COMMAND_DESCRIPTION: Msg = Msg::pmmp(
    "pocketmine.command.help.specificCommand.description",
    "§6Description: §r{%0}",
);
const HELP_COMMAND_USAGE: Msg = Msg::pmmp(
    "pocketmine.command.help.specificCommand.usage",
    "§6Usage: §r{%0}",
);
const HELP_COMMAND_ALIASES: Msg = Msg::pmmp(
    "pocketmine.command.help.specificCommand.aliases",
    "§6Aliases: §r{%0}",
);
const COMMAND_NOT_FOUND: Msg = Msg::pmmp(
    "pocketmine.command.notFound",
    "Unknown command: {%0}. Use {%1} for a list of available commands.",
);
const PLAYERS_LIST: Msg = Msg::client("commands.players.list");
const EMOTE: Msg = Msg::client("chat.type.emote");
const ANNOUNCEMENT: Msg = Msg::client("chat.type.announcement");
const SEED_SUCCESS: Msg = Msg::client("commands.seed.success");
const SETWORLDSPAWN_SUCCESS: Msg = Msg::client("commands.setworldspawn.success");
const DEFAULTGAMEMODE_SUCCESS: Msg = Msg::client("commands.defaultgamemode.success");
const DIFFICULTY_SUCCESS: Msg = Msg::client("commands.difficulty.success");
const TIME_ADDED: Msg = Msg::client("commands.time.added");
const TIME_QUERY: Msg = Msg::client("commands.time.query");
const TIME_SET: Msg = Msg::client("commands.time.set");
const PARTICLE_SUCCESS: Msg = Msg::client("commands.particle.success");
const SAVE_START: Msg = Msg::pmmp("pocketmine.save.start", "Saving server data...");
const SAVE_DISABLED: Msg = Msg::client("commands.save.disabled");
const SAVE_ENABLED: Msg = Msg::client("commands.save.enabled");
const STOP_START: Msg = Msg::client("commands.stop.start");
const STATUS_HEADER: Msg = Msg::pmmp(
    "pocketmine.command.status.header",
    "§a---- §rServer status§a ----§r",
);
/// Anchor's line: PMMP's uptime is wall-clock time, which the tick does not read.
const STATUS_UPTIME: Msg = Msg::pmmp("pocketmine.command.status.uptime", "§6Uptime: §c{%0} ticks");
/// Anchor's line: PMMP has no player count in its status.
const STATUS_PLAYERS: Msg = Msg::pmmp("anchor.command.status.players", "§6Players: §c{%0}/{%1}");
/// PMMP's world line without the tick time (wall clock).
const STATUS_WORLD: Msg = Msg::pmmp(
    "pocketmine.command.status.world",
    "§6World \"{%0}\": §c{%1}§a loaded chunks, §c{%2}§a entities",
);
const VERSION_SOFTWARE_NAME: Msg = Msg::pmmp(
    "pocketmine.command.version.serverSoftwareName",
    "This server is running §a{%0}§r",
);
/// PMMP's line without the git hash, which a library build does not know.
const VERSION_SOFTWARE_VERSION: Msg = Msg::pmmp(
    "pocketmine.command.version.serverSoftwareVersion",
    "Server version: §a{%0}§r",
);
const VERSION_MINECRAFT: Msg = Msg::pmmp(
    "pocketmine.command.version.minecraftVersion",
    "Compatible Minecraft version: §a{%0}§r (protocol version: §a{%1}§r)",
);
const VERSION_OS: Msg = Msg::pmmp(
    "pocketmine.command.version.operatingSystem",
    "Operating system: §a{%0}§r",
);

/// The most particles one `/particle` shows: PMMP has no bound, and a count of 2^31 would stall
/// the tick.
const MAX_PARTICLES: i32 = 1024;
/// The random stream that spreads `/particle`'s particles.
const PARTICLE_SPREAD: u64 = random::tag("command-particle-spread");

/// PMMP's names of the difficulties (`World::getDifficultyFromString`): the values of the
/// client's `Difficulty` enum and what `/difficulty` reads them as.
const DIFFICULTIES: [(&str, Difficulty); 12] = [
    ("peaceful", Difficulty::Peaceful),
    ("p", Difficulty::Peaceful),
    ("0", Difficulty::Peaceful),
    ("easy", Difficulty::Easy),
    ("e", Difficulty::Easy),
    ("1", Difficulty::Easy),
    ("normal", Difficulty::Normal),
    ("n", Difficulty::Normal),
    ("2", Difficulty::Normal),
    ("hard", Difficulty::Hard),
    ("h", Difficulty::Hard),
    ("3", Difficulty::Hard),
];
/// PMMP's names of times of day (`TimeCommand`).
const TIME_NAMES: [&str; 6] = ["day", "noon", "sunset", "night", "midnight", "sunrise"];
/// PMMP's particle names (`ParticleCommand::getParticle`).
const PARTICLE_NAMES: [&str; 35] = [
    "explode",
    "hugeexplosion",
    "hugeexplosionseed",
    "bubble",
    "splash",
    "wake",
    "water",
    "crit",
    "smoke",
    "spell",
    "instantspell",
    "dripwater",
    "driplava",
    "townaura",
    "spore",
    "portal",
    "flame",
    "lava",
    "reddust",
    "snowballpoof",
    "slime",
    "itembreak",
    "terrain",
    "heart",
    "ink",
    "droplet",
    "enchantmenttable",
    "happyvillager",
    "angryvillager",
    "forcefield",
    "mobflame",
    "iconcrack",
    "blockcrack",
    "blockdust",
    "sonicexplosion",
];

/// `/help [page]` and `/help <command>` (alias `/?`; PMMP `HelpCommand`): the commands the
/// sender may use in name order, a player's screen line height a page (7 unless set) and one
/// page for the console, or one command's description, usage and aliases.
fn help(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    if let Some(name) = args.string("command") {
        let command = cx
            .command(name)
            .filter(|c| cx.can_use(*sender, c))
            .ok_or_else(|| COMMAND_NOT_FOUND.error(&[name, "/help"]))?;
        let mut aliases: Vec<&str> = command.aliases().iter().map(String::as_str).collect();
        aliases.sort_unstable();
        let usage = command.usage_text().replace('\n', "\n§r");
        let mut out = HELP_COMMAND_HEADER.output(&[command.name()]);
        for (msg, value) in [
            (HELP_COMMAND_DESCRIPTION, command.description().to_owned()),
            (HELP_COMMAND_USAGE, usage),
            (HELP_COMMAND_ALIASES, aliases.join(", ")),
        ] {
            out.lines.extend(msg.output(&[&value]).lines);
        }
        return Ok(out);
    }
    let mut usable: Vec<(&str, &str)> = cx
        .commands()
        .filter(|c| cx.can_use(*sender, c))
        .map(|c| (c.name(), c.description()))
        .collect();
    usable.sort_unstable();
    let height = match sender {
        Sender::Player(id) => cx.player(*id).map_or(1, |p| {
            usize::try_from(p.screen_line_height()).unwrap_or(usize::MAX)
        }),
        Sender::Console => usable.len().max(1),
    };
    let pages = usable.len().div_ceil(height).max(1);
    let wanted = usize::try_from(args.int("page").unwrap_or(1)).unwrap_or(1);
    let page = wanted.clamp(1, pages);
    let mut out = HELP_HEADER.output(&[&page.to_string(), &pages.to_string()]);
    for (name, description) in usable.iter().skip((page - 1) * height).take(height) {
        out = out.line(format!("§2/{name}: §r{description}"));
    }
    Ok(out)
}

/// `/list` (PMMP `ListCommand`): how many players are online of how many may be, and their
/// names in text order.
fn list(cx: &mut Context<'_>, _: &Sender, _: &Args) -> Output {
    let mut names: Vec<&str> = cx.players().map(Player::name).collect();
    names.sort_unstable();
    let count = names.len().to_string();
    let max = cx.max_players().to_string();
    PLAYERS_LIST.output(&[&count, &max]).line(names.join(", "))
}

/// Shows every online player `message` and logs `text`.
fn broadcast(cx: &mut Context<'_>, message: Message, text: &str) -> Result<Output> {
    cx.broadcast(message)
        .map_err(|e| Error::failed(e.to_string()))?;
    info!(%text, "broadcast");
    Ok(Output::new())
}

/// `/me <action>` (PMMP `MeCommand`): `* name action` to everyone.
fn me(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let action = args.string("action").unwrap_or_default();
    let name = display_name_of(cx, *sender);
    let text = format!("§r{action}");
    broadcast(cx, EMOTE.chat("", &[&name, &text]), action)
}

/// `/say <message>` (PMMP `SayCommand`): `[name] message` in light purple to everyone; the
/// console speaks as `Server`.
fn say(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let message = args.string("message").unwrap_or_default();
    let name = match sender {
        Sender::Player(_) => display_name_of(cx, *sender),
        Sender::Console => "Server".to_owned(),
    };
    broadcast(cx, ANNOUNCEMENT.chat("§d", &[&name, message]), message)
}

/// The world `sender` is in: a player's own, the default world for the console.
fn sender_world(cx: &Context<'_>, sender: Sender) -> world::Id {
    match sender {
        Sender::Player(id) => cx.player(id).map_or(world::Id::DEFAULT, Player::world),
        Sender::Console => world::Id::DEFAULT,
    }
}

/// `/seed` (PMMP `SeedCommand`): the seed of the sender's world.
fn seed(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    let world = sender_world(cx, *sender);
    let seed = cx.world(world).map_or(0, world::World::seed);
    SEED_SUCCESS.output(&[&seed.to_string()])
}

/// `/setworldspawn [x y z]` (PMMP `SetWorldSpawnCommand`): moves the spawn of the sender's world
/// to the block at the position, or at the player's feet. Relative coordinates count from the
/// sender (the default world's spawn for the console; PMMP: 0 0 0).
fn setworldspawn(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let at = match (args.position("spawnPoint"), *sender) {
        (Some(pos), _) => pos,
        (None, Sender::Player(id)) => cx.player(id).map_or(Vec3::ZERO, Player::position),
        (None, Sender::Console) => return Err(PLAYER_USER_ONLY.error(&[])),
    };
    let pos = BlockPos::containing(in_world(at));
    let world = sender_world(cx, *sender);
    if let Some(w) = cx.world_mut(world) {
        w.set_spawn(pos);
    }
    let params = [pos.x.to_string(), pos.y.to_string(), pos.z.to_string()];
    let params: Vec<&str> = params.iter().map(String::as_str).collect();
    Ok(announce(cx, *sender, SETWORLDSPAWN_SUCCESS, &params))
}

/// `/defaultgamemode <mode>` (PMMP `DefaultGamemodeCommand`): the game mode new players get,
/// until the server stops.
fn defaultgamemode(cx: &mut Context<'_>, _: &Sender, args: &Args) -> Result<Output> {
    let Some(mode) = args.string("gameMode").and_then(GameMode::from_name) else {
        return Err(usage(cx, "defaultgamemode"));
    };
    cx.set_default_game_mode(mode);
    Ok(DEFAULTGAMEMODE_SUCCESS.output(&[game_mode_name(mode)]))
}

/// `/difficulty <difficulty>` (PMMP `DifficultyCommand`): the difficulty until the server stops;
/// always hard on a hardcore server.
fn difficulty(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let name = args.string("difficulty").unwrap_or_default();
    let Some(&(_, chosen)) = DIFFICULTIES.iter().find(|(n, _)| *n == name) else {
        return Err(usage(cx, "difficulty"));
    };
    let chosen = if cx.is_hardcore() {
        Difficulty::Hard
    } else {
        chosen
    };
    cx.set_difficulty(chosen);
    let number = crate::tick::spawn::difficulty_id(chosen).to_string();
    Ok(announce(cx, *sender, DIFFICULTY_SUCCESS, &[&number]))
}

/// `/time set <day|noon|...|ticks>`, `/time add <ticks>` and `/time query` (PMMP `TimeCommand`),
/// on every world; each action has its own permission. A number is taken from 0 to 30,000,000.
/// PMMP's `start` and `stop` are left out: Anchor's day cycle cannot be stopped (PLAN.md 10.8).
fn time(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    if args.has("query") {
        require(cx, *sender, permission::COMMAND_TIME_QUERY, "time")?;
        let world = sender_world(cx, *sender);
        let now = cx.world(world).map_or(0, world::World::time);
        return Ok(TIME_QUERY.output(&[&now.to_string()]));
    }
    let amount = args.int("amount").map(|n| n.clamp(0, super::MAX_COORD));
    let worlds: Vec<(world::Id, i64)> = cx.worlds().map(|w| (w.id(), w.time())).collect();
    if args.has("add") {
        require(cx, *sender, permission::COMMAND_TIME_ADD, "time")?;
        let added = amount.unwrap_or(0);
        for (id, now) in worlds {
            cx.set_time(id, now.saturating_add(i64::from(added)));
        }
        return Ok(announce(cx, *sender, TIME_ADDED, &[&added.to_string()]));
    }
    require(cx, *sender, permission::COMMAND_TIME_SET, "time")?;
    let value = match (args.string("time"), amount) {
        (Some("day"), _) => 1000,
        (Some("noon"), _) => 6000,
        (Some("sunset"), _) => 12_000,
        (Some("night"), _) => 13_000,
        (Some("midnight"), _) => 18_000,
        (Some("sunrise"), _) => 23_000,
        (_, Some(n)) => n,
        _ => return Err(usage(cx, "time")),
    };
    for (id, _) in worlds {
        cx.set_time(id, i64::from(value));
    }
    Ok(announce(cx, *sender, TIME_SET, &[&value.to_string()]))
}

/// A number PHP reads from `data` (`(int)`): 0 when it is not one.
fn data_int(data: Option<&str>, default: i32) -> i32 {
    data.map_or(default, |d| d.parse().unwrap_or(0))
}

/// The block state of the item named `data`, if it is a block other than air.
fn data_block(data: Option<&str>) -> Option<crate::block::State> {
    let state = crate::command::parse::Names::item(data?)?.block()?;
    (state.name() != "minecraft:air").then_some(state)
}

/// The particle `/particle` names with its `data` (PMMP `ParticleCommand::getParticle`).
fn particle_of(name: &str, data: Option<&str>) -> Option<Particle> {
    let black = Rgba {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };
    let item_of = |kind: ItemKind| Particle::ItemBreak(kind.default_type());
    Some(match name {
        "explode" => Particle::Explode,
        "hugeexplosion" => Particle::HugeExplode,
        "hugeexplosionseed" => Particle::HugeExplodeSeed,
        "bubble" => Particle::Bubble,
        "splash" => Particle::Splash,
        "wake" | "water" => Particle::Water,
        "crit" => Particle::Critical(2),
        "smoke" => Particle::Smoke(data_int(data, 0)),
        "spell" => Particle::Enchant(black),
        "instantspell" => Particle::InstantEnchant(black),
        "dripwater" => Particle::WaterDrip,
        "driplava" => Particle::LavaDrip,
        "townaura" | "spore" => Particle::Spore,
        "portal" => Particle::Portal,
        "flame" => Particle::Flame,
        "lava" => Particle::Lava,
        "reddust" => Particle::Redstone(data_int(data, 1)),
        "snowballpoof" => item_of(ItemKind::Snowball),
        "slime" => item_of(ItemKind::Slimeball),
        "itembreak" | "iconcrack" => {
            let item: item::Type = crate::command::parse::Names::item(data?)?;
            Particle::ItemBreak(item)
        }
        "terrain" | "blockcrack" => Particle::Terrain(data_block(data)?),
        "heart" => Particle::Heart(data_int(data, 0)),
        "ink" => Particle::Ink(data_int(data, 0)),
        "droplet" => Particle::RainSplash,
        "enchantmenttable" => Particle::EnchantmentTable,
        "happyvillager" => Particle::HappyVillager,
        "angryvillager" => Particle::AngryVillager,
        "forcefield" => Particle::BlockForceField(data_int(data, 0)),
        "mobflame" => Particle::EntityFlame,
        "blockdust" => {
            let mut parts = data?.splitn(5, '_');
            let mut channel = || {
                parts
                    .next()
                    .map(|p| (p.parse::<i64>().unwrap_or(0) & 0xff).to_le_bytes()[0])
            };
            let (r, g, b) = (channel()?, channel()?, channel()?);
            let a = channel().unwrap_or(255);
            Particle::Dust(Rgba { r, g, b, a })
        }
        "sonicexplosion" => Particle::SonicExplosion,
        _ => return None,
    })
}

/// `/particle <name> <x> <y> <z> <xd> <yd> <zd> [count] [data]` (PMMP `ParticleCommand`): `count`
/// particles (1 to 1,024) spread at random up to the distances on each axis, in the sender's
/// world. The spread comes from the engine's random streams, so a replay shows the same.
/// Particle numbers are gophertunnel's; comparing them with BDS is pending (PLAN.md 10.8).
fn particle(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let name = args.string("name").unwrap_or_default();
    let Some(effect) = particle_of(name, args.string("data")) else {
        return Err(usage(cx, "particle"));
    };
    let count = super::bounded(args.int("count").unwrap_or(1).max(1), 1, MAX_PARTICLES)?;
    let center = in_world(args.position("position").unwrap_or(Vec3::ZERO));
    let spread = Vec3::new(
        args.float("xd").unwrap_or(0.0),
        args.float("yd").unwrap_or(0.0),
        args.float("zd").unwrap_or(0.0),
    );
    let world = sender_world(cx, *sender);
    let seed = cx.default_world().seed();
    let session = match sender {
        Sender::Player(id) => id.session(),
        Sender::Console => SessionId::CONSOLE,
    };
    let key = random::key(PARTICLE_SPREAD, &[session.get()]);
    let mut rng = Random::new(seed, cx.tick(), key);
    let mut signed = || rng.unit() * 2.0 - 1.0;
    for _ in 0..count {
        let offset = Vec3::new(
            signed() * spread.x,
            signed() * spread.y,
            signed() * spread.z,
        );
        cx.add_particle(world, center + offset, effect);
    }
    Ok(PARTICLE_SUCCESS.output(&[name, &count.to_string()]))
}

/// `/save-all` (PMMP `SaveCommand`): saves everything at the end of the tick, as one commit.
/// PMMP's "Save completed in ... seconds" is left out: the commit ends later, off the tick.
fn save_all(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    let out = announce(cx, *sender, SAVE_START, &[]);
    cx.save();
    out
}

/// `/save-off` (PMMP `SaveOffCommand`): turns the periodic save off until the server stops or
/// `/save-on`; `/save-all`, the memory bounds and shutdown still save.
fn save_off(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    cx.set_auto_save(false);
    announce(cx, *sender, SAVE_DISABLED, &[])
}

/// `/save-on` (PMMP `SaveOnCommand`): turns the periodic save back on.
fn save_on(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    cx.set_auto_save(true);
    announce(cx, *sender, SAVE_ENABLED, &[])
}

/// `/status` (PMMP `StatusCommand`): what the tick can tell without a wall clock: the ticks run
/// since start, the players, and each world's loaded chunks and entities. PMMP's TPS, load,
/// network, thread and memory lines are left out: the tick reads no clock or process data.
fn status(cx: &mut Context<'_>, _: &Sender, _: &Args) -> Output {
    let uptime = cx
        .tick()
        .saturating_sub(cx.state.commands.started.unwrap_or(cx.tick()));
    let mut out = STATUS_HEADER.output(&[]);
    out.lines
        .extend(STATUS_UPTIME.output(&[&uptime.to_string()]).lines);
    let players = [cx.player_count().to_string(), cx.max_players().to_string()];
    out.lines
        .extend(STATUS_PLAYERS.output(&[&players[0], &players[1]]).lines);
    let worlds: Vec<(world::Id, String, usize)> = cx
        .worlds()
        .map(|w| (w.id(), w.name().to_owned(), w.loaded_chunks().len()))
        .collect();
    for (id, name, chunks) in worlds {
        let entities = cx.entities(id).count();
        let params = [name, chunks.to_string(), entities.to_string()];
        out.lines.extend(
            STATUS_WORLD
                .output(&[&params[0], &params[1], &params[2]])
                .lines,
        );
    }
    out
}

/// `/stop` (PMMP `StopCommand`): stops the server after this tick.
fn stop(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    let out = announce(cx, *sender, STOP_START, &[]);
    cx.shutdown();
    out
}

/// `/version` (aliases `/ver`, `/about`; PMMP `VersionCommand`): the server software, its
/// version, the Minecraft version and protocol, and the operating system. PMMP's PHP lines and
/// plugin lookup are left out.
fn version(_: &mut Context<'_>, _: &Sender, _: &Args) -> Output {
    let mut out = VERSION_SOFTWARE_NAME.output(&[Context::NAME]);
    for line in [
        VERSION_SOFTWARE_VERSION.output(&[Context::ANCHOR_VERSION]),
        VERSION_MINECRAFT.output(&[Context::VERSION, &PROTOCOL_VERSION.to_string()]),
        VERSION_OS.output(&[std::env::consts::OS]),
    ] {
        out.lines.extend(line.lines);
    }
    out
}
