//! Commands on the server and its worlds: `/help`, `/list`, `/me`, `/say`, `/seed`,
//! `/setworldspawn`, `/defaultgamemode`, `/difficulty`, `/time`, `/particle`, `/save-all`,
//! `/save-off`, `/save-on`, `/status`, `/stop` and `/version` (PMMP's classes of the same names),
//! and Anchor's own `/backup`. `/status` shows Anchor's own lines.

use tracing::info;

use super::{
    Msg, PLAYER_USER_ONLY, announce, display_name_of, game_mode_name, in_world, infallible,
    require, usage,
};
use crate::command::{Args, Command, Error, Output, Param, Result, Sender};
use crate::config::Difficulty;
use crate::item::{self, Kind as ItemKind};
use crate::math::Vec3;
use crate::permission;
use crate::player::{Message, Player};
use crate::protocol::Rgba;
use crate::protocol::packet::PROTOCOL_VERSION;
use crate::random::{self, Random};
use crate::runtime::ThreadTotals;
use crate::tick::Context;
use crate::tick::queue::SessionId;
use crate::world::{self, BlockPos, Particle};

/// Adds the commands of this module.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: the commands read as one table, one entry per PMMP class"
)]
pub(super) fn add(commands: &mut Vec<Command>) {
    commands.extend([
        Command::new("help", "commands.help.description")
            .alias("?")
            .permission(permission::COMMAND_HELP)
            .overload(&[Param::int("page").optional()], help)
            .overload(&[Param::string("command")], help),
        Command::new("list", "commands.list.description")
            .permission(permission::COMMAND_LIST)
            .overload(&[], infallible(list)),
        Command::new("me", "commands.me.description")
            .permission(permission::COMMAND_ME)
            .overload(&[Param::message("message")], me),
        Command::new("say", "commands.say.description")
            .permission(permission::COMMAND_SAY)
            .overload(&[Param::message("message")], say),
        Command::new("seed", "Shows the seed of the world")
            .permission(permission::COMMAND_SEED)
            .overload(&[], infallible(seed)),
        Command::new("setworldspawn", "commands.setworldspawn.description")
            .permission(permission::COMMAND_SETWORLDSPAWN)
            .overload(&[], setworldspawn)
            .overload(&[Param::position("spawnPoint")], setworldspawn),
        Command::new("defaultgamemode", "commands.defaultgamemode.description")
            .permission(permission::COMMAND_DEFAULTGAMEMODE)
            .overload(&[super::game_mode_param()], defaultgamemode),
        Command::new("difficulty", "commands.difficulty.description")
            .permission(permission::COMMAND_DIFFICULTY)
            .overload(
                &[Param::enumeration(
                    "difficulty",
                    "Difficulty",
                    &DIFFICULTIES.map(|(name, _)| name),
                )],
                difficulty,
            )
            .overload(&[Param::int("difficulty")], difficulty),
        Command::new("time", "commands.time.description")
            .permission(permission::COMMAND_TIME_ADD)
            .permission(permission::COMMAND_TIME_SET)
            .permission(permission::COMMAND_TIME_QUERY)
            .permission(permission::COMMAND_TIME_START)
            .permission(permission::COMMAND_TIME_STOP)
            .overload(
                &[time_mode("TimeModeAdd", "add"), Param::int("amount")],
                time,
            )
            .overload(
                &[time_mode("TimeModeSet", "set"), Param::int("amount")],
                time,
            )
            .overload(
                &[
                    time_mode("TimeModeSet", "set"),
                    Param::enumeration("time", "TimeSpec", &TIME_NAMES.map(|(name, _)| name)),
                ],
                time,
            )
            .overload(
                &[
                    time_mode("TimeModeQuery", "query"),
                    Param::enumeration("time", "TimeQuery", &TIME_QUERIES),
                ],
                time,
            )
            .overload(&[time_mode("TimeModeStart", "start")], time)
            .overload(&[time_mode("TimeModeStop", "stop")], time),
        Command::new("particle", "commands.particle.description")
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
        Command::new("save-all", "Saves every world and player now")
            .permission(permission::COMMAND_SAVE_PERFORM)
            .overload(&[], infallible(save_all)),
        Command::new("save-off", "Turns automatic saving off until save-on")
            .permission(permission::COMMAND_SAVE_DISABLE)
            .overload(&[], infallible(save_off)),
        Command::new("save-on", "commands.save-on.description")
            .permission(permission::COMMAND_SAVE_ENABLE)
            .overload(&[], infallible(save_on)),
        Command::new("backup", "Saves the server and backs up the world")
            .permission(permission::COMMAND_BACKUP)
            .overload(&[], backup),
        Command::new("status", "Shows how the server is running")
            .permission(permission::COMMAND_STATUS)
            .overload(&[], infallible(status)),
        Command::new("stop", "commands.stop.description")
            .permission(permission::COMMAND_STOP)
            .overload(&[], infallible(stop)),
        Command::new("version", "Shows the Anchor and Minecraft versions")
            .alias("ver")
            .alias("about")
            .permission(permission::COMMAND_VERSION)
            .overload(&[], infallible(version)),
    ]);
}

const HELP_HEADER: Msg = Msg::client("commands.help.header");
/// The first line of `/help <command>`.
const HELP_COMMAND_NAME: Msg = Msg::own("§e/{%0}");
/// The last line of `/help <command>` for a command with other names.
const HELP_COMMAND_ALIASES: Msg = Msg::own("§7Also: {%0}");
const COMMAND_UNKNOWN: Msg = Msg::client("commands.generic.unknown");
const PLAYERS_LIST: Msg = Msg::client("commands.players.list");
const EMOTE: Msg = Msg::client("chat.type.emote");
const ANNOUNCEMENT: Msg = Msg::client("chat.type.announcement");
const SEED_SUCCESS: Msg = Msg::client("commands.seed.success");
const SETWORLDSPAWN_SUCCESS: Msg = Msg::client("commands.setworldspawn.success");
const DEFAULTGAMEMODE_SUCCESS: Msg = Msg::client("commands.defaultgamemode.success");
const DIFFICULTY_SUCCESS: Msg = Msg::client("commands.difficulty.success");
const TIME_ADDED: Msg = Msg::client("commands.time.added");
const TIME_QUERY_DAYTIME: Msg = Msg::client("commands.time.query.daytime");
const TIME_QUERY_GAMETIME: Msg = Msg::client("commands.time.query.gametime");
const TIME_QUERY_DAY: Msg = Msg::client("commands.time.query.day");
const TIME_SET: Msg = Msg::client("commands.time.set");
/// `/time stop`: the client has no key for it.
const TIME_STOPPED: Msg = Msg::own("The time stands still in every world");
/// `/time start`: the client has no key for it.
const TIME_STARTED: Msg = Msg::own("The time runs again in every world");
const PARTICLE_SUCCESS: Msg = Msg::client("commands.particle.success");
const SAVE_START: Msg = Msg::client("commands.save.start");
/// PMMP has no backup command.
const BACKUP_START: Msg =
    Msg::own("Saving and backing up the world; the server log tells when the backup is made");
/// `backups = false`.
const BACKUP_OFF: Msg = Msg::own("Backups are turned off on this server");
const SAVE_DISABLED: Msg = Msg::client("commands.save.disabled");
const SAVE_ENABLED: Msg = Msg::client("commands.save.enabled");
const STOP_START: Msg = Msg::client("commands.stop.start");
/// The `/version` line: the software, its version, the Minecraft version and protocol, and the
/// operating system.
const VERSION_LINE: Msg = Msg::own("{%0} {%1} for Minecraft {%2} (protocol {%3}) on {%4}");

/// The most particles one `/particle` shows: PMMP has no bound, and a count of 2^31 would stall
/// the tick.
const MAX_PARTICLES: i32 = 1024;
/// The random stream that spreads `/particle`'s particles.
const PARTICLE_SPREAD: u64 = random::tag("command-particle-spread");

/// BDS's `Difficulty` enum (P6, m9-facts.md; PMMP `World::getDifficultyFromString` takes the
/// same words) and what `/difficulty` reads them as; a number is the difficulty's ID.
const DIFFICULTIES: [(&str, Difficulty); 8] = [
    ("normal", Difficulty::Normal),
    ("peaceful", Difficulty::Peaceful),
    ("easy", Difficulty::Easy),
    ("hard", Difficulty::Hard),
    ("p", Difficulty::Peaceful),
    ("e", Difficulty::Easy),
    ("n", Difficulty::Normal),
    ("h", Difficulty::Hard),
];
/// The difficulties by ID (`/difficulty <0-3>`).
const DIFFICULTY_IDS: [Difficulty; 4] = [
    Difficulty::Peaceful,
    Difficulty::Easy,
    Difficulty::Normal,
    Difficulty::Hard,
];
/// Times of day, as BDS's `TimeSpec` enum names them, and their ticks (PMMP `TimeCommand`).
const TIME_NAMES: [(&str, i32); 6] = [
    ("day", 1000),
    ("sunrise", 23_000),
    ("noon", 6000),
    ("sunset", 12_000),
    ("night", 13_000),
    ("midnight", 18_000),
];
/// What `/time query` reads, as BDS names it (its `TimeQuery` enum).
const TIME_QUERIES: [&str; 3] = ["daytime", "gametime", "day"];
/// Ticks in a Minecraft day.
const DAY_TICKS: i64 = 24_000;
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

/// The first word of a `/time` form: `word`, BDS's enum `type_name` (`TimeModeStart` and
/// `TimeModeStop` are Anchor's names for PMMP's forms BDS lacks).
fn time_mode(type_name: &str, word: &str) -> Param {
    Param::enumeration("mode", type_name, &[word])
}

/// `/help [page]` and `/help <command>` (alias `/?`; PMMP `HelpCommand`, in Anchor's layout):
/// the first usage line of each command the sender may use, in name order, a player's screen
/// line height a page (7 unless set) and one page for the console; or one command's name, its
/// description on a line of its own (the client translates a description that is a key, as
/// Anchor's own commands' are), its usage lines and its other names.
fn help(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    if let Some(name) = args.string("command") {
        let command = cx
            .command(name)
            .filter(|c| cx.can_use(*sender, c))
            .ok_or_else(|| COMMAND_UNKNOWN.error(&[name]))?;
        let mut out = HELP_COMMAND_NAME
            .output(&[command.name()])
            .translated(command.description(), &[]);
        for line in command.usage_text().lines() {
            out = out.line(line);
        }
        let mut aliases: Vec<&str> = command.aliases().iter().map(String::as_str).collect();
        if !aliases.is_empty() {
            aliases.sort_unstable();
            let line = HELP_COMMAND_ALIASES.output(&[&aliases.join(", ")]);
            out.lines.extend(line.lines);
        }
        return Ok(out);
    }
    let mut usable: Vec<(&str, String)> = cx
        .commands()
        .filter(|c| cx.can_use(*sender, c))
        .map(|c| (c.name(), c.usage_text()))
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
    for (_, usage) in usable.iter().skip((page - 1) * height).take(height) {
        out = out.line(format!("§2{}", usage.lines().next().unwrap_or_default()));
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
    let action = args.string("message").unwrap_or_default();
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
        Sender::Player(id) => cx
            .player(id)
            .map_or_else(|| cx.default_world().id(), Player::world),
        Sender::Console => cx.default_world().id(),
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
    // `default` names no mode here.
    let Some(mode) = super::game_mode_arg(args, None) else {
        return Err(usage(cx, "defaultgamemode"));
    };
    cx.set_default_game_mode(mode);
    Ok(DEFAULTGAMEMODE_SUCCESS.output(&[game_mode_name(mode)]))
}

/// `/difficulty <difficulty>` (PMMP `DifficultyCommand`): the difficulty until the server stops;
/// always hard on a hardcore server.
fn difficulty(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let chosen = match (args.string("difficulty"), args.int("difficulty")) {
        (Some(name), _) => DIFFICULTIES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|&(_, d)| d),
        (_, Some(id)) => usize::try_from(id)
            .ok()
            .and_then(|i| DIFFICULTY_IDS.get(i))
            .copied(),
        _ => None,
    };
    let Some(chosen) = chosen else {
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

/// `/time set <day|noon|...|ticks>`, `/time add <ticks>`, `/time stop` and `/time start` (PMMP
/// `TimeCommand`) on every world, and `/time query <daytime|gametime|day>` on the sender's world,
/// as BDS reads it: the time of day, the world's clock (Anchor keeps one clock, which `/time set`
/// moves) or the days passed. Each action has its own permission. A number is taken from 0 to
/// 30,000,000.
fn time(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let mode = args.string("mode").unwrap_or_default();
    if mode == "query" {
        require(cx, *sender, permission::COMMAND_TIME_QUERY, "time")?;
        let world = sender_world(cx, *sender);
        let now = cx.world(world).map_or(0, world::World::time_ticks);
        let (msg, value) = match args.string("time").unwrap_or_default() {
            "daytime" => (TIME_QUERY_DAYTIME, now.rem_euclid(DAY_TICKS)),
            "gametime" => (TIME_QUERY_GAMETIME, now),
            _ => (TIME_QUERY_DAY, now.div_euclid(DAY_TICKS)),
        };
        return Ok(msg.output(&[&value.to_string()]));
    }
    let amount = args.int("amount").map(|n| n.clamp(0, super::MAX_COORD));
    let worlds: Vec<(world::Id, i64)> = cx.worlds().map(|w| (w.id(), w.time_ticks())).collect();
    if mode == "stop" || mode == "start" {
        let stop = mode == "stop";
        let (needed, msg) = if stop {
            (permission::COMMAND_TIME_STOP, TIME_STOPPED)
        } else {
            (permission::COMMAND_TIME_START, TIME_STARTED)
        };
        require(cx, *sender, needed, "time")?;
        for (id, _) in worlds {
            if stop {
                cx.stop_time(id);
            } else {
                cx.start_time(id);
            }
        }
        return Ok(announce(cx, *sender, msg, &[]));
    }
    if mode == "add" {
        require(cx, *sender, permission::COMMAND_TIME_ADD, "time")?;
        let added = amount.unwrap_or(0);
        for (id, now) in worlds {
            cx.set_time(id, now.saturating_add(i64::from(added)));
        }
        return Ok(announce(cx, *sender, TIME_ADDED, &[&added.to_string()]));
    }
    require(cx, *sender, permission::COMMAND_TIME_SET, "time")?;
    let named = args
        .string("time")
        .and_then(|t| TIME_NAMES.iter().find(|(name, _)| *name == t))
        .map(|&(_, ticks)| ticks);
    let Some(value) = named.or(amount) else {
        return Err(usage(cx, "time"));
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

/// `/backup` (Anchor's own): saves everything at the end of the tick, then backs up the saved
/// server file while the server runs ([`Context::backup`]). The backup ends later, off the tick,
/// so the log tells its result, as for `/save-all`'s commit.
fn backup(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Result<Output> {
    if !cx.backup() {
        return Err(BACKUP_OFF.error(&[]));
    }
    Ok(announce(cx, *sender, BACKUP_START, &[]))
}

/// `/save-off` (PMMP `SaveOffCommand`): turns the periodic save off until the server stops or
/// `/save-on`; `/save-all`, the memory bounds, a quarter of the input log's bound recorded since
/// the last save (PLAN.md 10.9) and shutdown still save.
fn save_off(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    cx.set_auto_save(false);
    announce(cx, *sender, SAVE_DISABLED, &[])
}

/// `/save-on` (PMMP `SaveOnCommand`): turns the periodic save back on.
fn save_on(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    cx.set_auto_save(true);
    announce(cx, *sender, SAVE_ENABLED, &[])
}

/// `/status` (PMMP `StatusCommand`, in Anchor's own lines): the uptime by the wall clock and in
/// ticks, the TPS and tick times now and averaged over the last 20 ticks, the resident memory,
/// the threads, the players, and each loaded world's chunks, entities and block entities. The
/// uptime, memory and tick times are measured off the tick and read from the input log (PLAN.md
/// 10.9), so the uptime and memory are at most a second old.
fn status(cx: &mut Context<'_>, _: &Sender, _: &Args) -> Output {
    let state = &*cx.state;
    let network = &state.network;
    let ticks = state
        .tick
        .saturating_sub(state.commands.started.unwrap_or(state.tick));
    let secs = network.uptime().as_secs();
    let threads = network.threads();
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    let mut out = Output::from("Server status".to_owned())
        .line(format!(
            "Uptime: {}:{:02}:{:02} ({ticks} ticks)",
            secs / 3600,
            secs / 60 % 60,
            secs % 60
        ))
        .line(format!(
            "TPS: {:.2} now, {:.2} average",
            network.tps(),
            network.average_tps()
        ))
        .line(format!(
            "Tick time: {:.2} ms now, {:.2} ms average",
            ms(network.mspt()),
            ms(network.average_mspt())
        ))
        .line(memory_line(network.memory()))
        .line(format!(
            "Threads: tick and save {}, network {}, CPU pool {}, blocking up to {}, console {}, \
             log {}",
            ThreadTotals::FIXED,
            threads.network,
            threads.cpu,
            ThreadTotals::BLOCKING,
            u8::from(threads.console),
            u8::from(threads.log)
        ))
        .line(format!(
            "Players: {} of {}",
            cx.player_count(),
            cx.max_players()
        ));
    for world in cx.worlds() {
        let index = world.id().index();
        out = out.line(format!(
            "World \"{}\": {} chunks, {} entities, {} block entities",
            world.name(),
            world.loaded_chunks().len(),
            state.rules.entities.count(index),
            state.rules.block_entities.count(index)
        ));
    }
    out
}

/// The memory line of `/status` for `bytes` resident (0: unknown), in MiB to a tenth.
pub(super) fn memory_line(bytes: u64) -> String {
    const MIB: u64 = 1 << 20;
    if bytes == 0 {
        return "Memory: unknown".to_owned();
    }
    let tenths = bytes.saturating_mul(10).saturating_add(MIB / 2) / MIB;
    format!("Memory: {}.{} MiB resident", tenths / 10, tenths % 10)
}

/// `/stop` (PMMP `StopCommand`): stops the server after this tick.
fn stop(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    let out = announce(cx, *sender, STOP_START, &[]);
    cx.shutdown();
    out
}

/// `/version` (aliases `/ver`, `/about`; PMMP `VersionCommand`, in Anchor's layout): one line
/// with [`VERSION_LINE`]. PMMP's PHP lines and plugin lookup are left out.
fn version(_: &mut Context<'_>, _: &Sender, _: &Args) -> Output {
    VERSION_LINE.output(&[
        Context::NAME,
        Context::ANCHOR_VERSION,
        Context::VERSION,
        &PROTOCOL_VERSION.to_string(),
        std::env::consts::OS,
    ])
}
