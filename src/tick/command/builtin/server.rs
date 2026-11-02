//! Commands on the server and its worlds: `/help`, `/list`, `/me`, `/say`, `/seed`,
//! `/setworldspawn`, `/defaultgamemode`, `/difficulty`, `/time`, `/particle`, `/save-all`,
//! `/save-off`, `/save-on`, `/status`, `/stop` and `/version` (PMMP's classes of the same names),
//! and Anchor's own `/backup`. `/status` shows Anchor's own lines.

use tracing::info;

use super::{
    Msg, PLAYER_USER_ONLY, announce, bounded, display_name_of, game_mode_arg, game_mode_name,
    game_mode_param, in_world, infallible, require, sender_world, usage,
};
use crate::command::parse::NAMESPACE;
use crate::command::{Args, Command, Error, Output, Param, Result, Sender};
use crate::config::Difficulty;
use crate::item::{self, Kind as ItemKind};
use crate::math::Vec3;
use crate::permission::{self, Permission};
use crate::player::{Message, Player};
use crate::protocol::Rgba;
use crate::protocol::packet::PROTOCOL_VERSION;
use crate::random::{self, Random};
use crate::runtime::ThreadTotals;
use crate::tick::Context;
use crate::tick::survival::{MARKERS, OVERWORLD_CLOCK_NAME};
use crate::world::{self, BlockPos, DAY, Particle, World};

const ANNOUNCEMENT: Msg = Msg::client("chat.type.announcement");
/// `backups = false`.
const BACKUP_OFF: Msg = Msg::own("Backups are turned off on this server");
/// PMMP has no backup command.
const BACKUP_START: Msg =
    Msg::own("Saving and backing up the data files; the server log tells when the backup is made");
const CLOCK_ADDED: Msg = Msg::client("commands.time.clock.added");
const CLOCK_MARKER_UNKNOWN: Msg = Msg::client("commands.time.clock.timemarker.failed");
const CLOCK_PAUSED: Msg = Msg::client("commands.time.clock.paused");
const CLOCK_QUERY_TIME: Msg = Msg::client("commands.time.clock.query.time");
const CLOCK_RESUMED: Msg = Msg::client("commands.time.clock.resumed");
const CLOCK_SET: Msg = Msg::client("commands.time.clock.set");
const CLOCK_TOO_SMALL: Msg = Msg::client("commands.time.clock.num.tooSmall");
const CLOCK_UNKNOWN: Msg = Msg::client("commands.time.clock.failed");
const COMMAND_UNKNOWN: Msg = Msg::client("commands.generic.unknown");
const DEFAULTGAMEMODE_SUCCESS: Msg = Msg::client("commands.defaultgamemode.success");
/// BDS's `Difficulty` enum (P6; PMMP `World::getDifficultyFromString` takes the same words) and
/// what `/difficulty` reads them as; a number is the difficulty's ID.
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
const DIFFICULTY_SUCCESS: Msg = Msg::client("commands.difficulty.success");
const EMOTE: Msg = Msg::client("chat.type.emote");
/// The last line of `/help <command>` for a command with other names.
const HELP_COMMAND_ALIASES: Msg = Msg::own("§7Also: {%0}");
/// The first line of `/help <command>`.
const HELP_COMMAND_NAME: Msg = Msg::own("§e/{%0}");
const HELP_HEADER: Msg = Msg::client("commands.help.header");
/// Which occurrence of a time marker `/time set <marker>` takes, `next` when left out (BDS
/// 1.26.60's `TimeMarkerOccurrence` enum).
const MARKER_OCCURRENCES: [&str; 3] = ["next", "previous", "stay"];
/// The most particles one `/particle` shows: PMMP has no bound, and a count of 2^31 would stall
/// the tick.
const MAX_PARTICLES: i32 = 1024;
/// The bytes in a MiB.
const MIB: u64 = 1 << 20;
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
/// The random stream that spreads `/particle`'s particles.
const PARTICLE_SPREAD: u64 = random::tag("command-particle-spread");
const PARTICLE_SUCCESS: Msg = Msg::client("commands.particle.success");
const PLAYERS_LIST: Msg = Msg::client("commands.players.list");
const SAVE_DISABLED: Msg = Msg::client("commands.save.disabled");
const SAVE_ENABLED: Msg = Msg::client("commands.save.enabled");
const SAVE_START: Msg = Msg::client("commands.save.start");
const SEED_SUCCESS: Msg = Msg::client("commands.seed.success");
const SETWORLDSPAWN_SUCCESS: Msg = Msg::client("commands.setworldspawn.success");
const STOP_START: Msg = Msg::client("commands.stop.start");
/// What `/time query` reads, as BDS 1.26.60 names it (its `TimeQuery2` enum).
const TIME_QUERIES: [&str; 4] = ["time", "daytime", "gametime", "day"];
const TIME_QUERY_DAY: Msg = Msg::client("commands.time.query.day");
const TIME_QUERY_DAYTIME: Msg = Msg::client("commands.time.query.daytime");
const TIME_QUERY_GAMETIME: Msg = Msg::client("commands.time.query.gametime");
/// The `/version` line: the software, its version, the Minecraft version and protocol, and the
/// operating system.
const VERSION_LINE: Msg = Msg::own("{%0} {%1} for Minecraft {%2} (protocol {%3}) on {%4}");
/// The soft enum of the clocks `/time of` names (BDS 1.26.60's).
pub(in crate::tick::command) const WORLD_CLOCKS: &str = "WorldClocks";

/// What one form of `/time` does, after its permission and clock are checked.
type TimeAction = fn(&mut Context<'_>, Sender, &Args) -> Result<Output>;

/// Where `/time set` puts a clock.
#[derive(Debug, Clone, Copy)]
enum SetTo<'a> {
    /// That time.
    Ticks(i64),
    /// A time marker at this time of day, at its occurrence `next`, `previous` or `stay`.
    Marker(i64, &'a str),
}

impl SetTo<'_> {
    /// The time a clock at `now` goes to: the marker's first time at or after `now` (`next`), its
    /// last at or before it (`previous`) or its time on `now`'s day (`stay`).
    fn time_from(self, now: i64) -> i64 {
        match self {
            Self::Ticks(ticks) => ticks,
            Self::Marker(time_of_day, occurrence) => {
                let today = now
                    .saturating_sub(now.rem_euclid(DAY))
                    .saturating_add(time_of_day);
                match occurrence {
                    "next" if today < now => today.saturating_add(DAY),
                    "previous" if today > now => today.saturating_sub(DAY),
                    _ => today,
                }
            }
        }
    }
}

/// Adds the commands of this module.
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
            .overload(&[game_mode_param()], defaultgamemode),
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
        with_time_forms(
            Command::new("time", "commands.time.description")
                .permission(permission::COMMAND_TIME_ADD)
                .permission(permission::COMMAND_TIME_SET)
                .permission(permission::COMMAND_TIME_QUERY)
                .permission(permission::COMMAND_TIME_START)
                .permission(permission::COMMAND_TIME_STOP),
        ),
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
        Command::new("backup", "Saves the server and backs up its data files")
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

/// Builds the word of a `/time` form: `word`, BDS's enum `type_name`.
fn time_mode(type_name: &str, word: &str) -> Param {
    Param::enumeration("mode", type_name, &[word])
}

/// Adds to `command` BDS 1.26.60's `/time` forms (P6 `commands`): `add`, `set` a number, `set` a
/// time marker, `query`, `pause` and `resume`, then the same six after `of <clock>`, whose
/// `query` reads the time only.
fn with_time_forms(mut command: Command) -> Command {
    let of = [
        Param::enumeration("command", "TimeOfCommand", &["of"]),
        Param::soft_enumeration("clockName", WORLD_CLOCKS),
    ];
    for prefix in [&[][..], &of[..]] {
        let query = if prefix.is_empty() {
            Param::enumeration("time", "TimeQuery2", &TIME_QUERIES)
        } else {
            Param::enumeration("clockQuery", "ClockTimeQuery", &["time"])
        };
        let forms = [
            vec![time_mode("TimeModeAdd", "add"), Param::int("amount")],
            vec![time_mode("TimeModeSet", "set"), Param::int("amount")],
            vec![
                time_mode("TimeModeSet", "set"),
                Param::clock_time_marker("timeMarkerName"),
                Param::enumeration(
                    "timeMarkerOccurrence",
                    "TimeMarkerOccurrence",
                    &MARKER_OCCURRENCES,
                )
                .optional(),
            ],
            vec![time_mode("TimeModeQuery", "query"), query],
            vec![time_mode("TimeModePause", "pause")],
            vec![time_mode("TimeModeResume", "resume")],
        ];
        for form in forms {
            let params: Vec<Param> = prefix.iter().cloned().chain(form).collect();
            command = command.overload(&params, time);
        }
    }
    command
}

/// Shows the first usage line of each command the sender may use, in name order, a player's
/// screen line height a page (7 unless set) and one page for the console; or one command's name,
/// its description on a line of its own (the client translates a description that is a key), its
/// usage lines and its other names (`/help [page]` and `/help <command>`, alias `/?`, PMMP
/// `HelpCommand`, in Anchor's layout).
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

/// Shows how many players are online of how many may be, and their names in text order
/// (`/list`, PMMP `ListCommand`).
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

/// Shows everyone `* name action` (`/me <action>`, PMMP `MeCommand`).
fn me(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let action = args.string("message").unwrap_or_default();
    let name = display_name_of(cx, *sender);
    let text = format!("§r{action}");
    broadcast(cx, EMOTE.chat("", &[&name, &text]), action)
}

/// Shows everyone `[name] message` in light purple; the console speaks as `Server`
/// (`/say <message>`, PMMP `SayCommand`).
fn say(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let message = args.string("message").unwrap_or_default();
    let name = match sender {
        Sender::Player(_) => display_name_of(cx, *sender),
        Sender::Console => "Server".to_owned(),
    };
    broadcast(cx, ANNOUNCEMENT.chat("§d", &[&name, message]), message)
}

/// Shows the seed of the sender's world (`/seed`, PMMP `SeedCommand`).
fn seed(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    let seed = world_of(cx, *sender).seed();
    SEED_SUCCESS.output(&[&seed.to_string()])
}

/// Moves the spawn of the sender's world to the block at the position, or at the player's feet;
/// relative coordinates count from the sender (the default world's spawn for the console; PMMP:
/// 0 0 0) (`/setworldspawn [x y z]`, PMMP `SetWorldSpawnCommand`).
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

/// Sets the game mode new players get, until the server stops (`/defaultgamemode <mode>`, PMMP
/// `DefaultGamemodeCommand`).
fn defaultgamemode(cx: &mut Context<'_>, _: &Sender, args: &Args) -> Result<Output> {
    // `default` names no mode here.
    let Some(mode) = game_mode_arg(args, None) else {
        return Err(usage(cx, "defaultgamemode"));
    };
    cx.set_default_game_mode(mode);
    Ok(DEFAULTGAMEMODE_SUCCESS.output(&[game_mode_name(mode)]))
}

/// Sets the difficulty until the server stops, always hard on a hardcore server
/// (`/difficulty <difficulty>`, PMMP `DifficultyCommand`).
fn difficulty(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let chosen = match (args.string("difficulty"), args.int("difficulty")) {
        (Some(name), _) => DIFFICULTIES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|&(_, d)| d),
        (_, Some(id)) => u8::try_from(id).ok().and_then(Difficulty::from_id),
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

/// Runs `/time` as BDS 1.26.60 has it (P6 `clocks`; PMMP `TimeCommand`'s `start` and `stop` are
/// its `resume` and `pause`), on the clock every world has (`minecraft:overworld`; `of` names it
/// with or without `minecraft:`, in its case): `add`, `set`, `pause` and `resume` act on every
/// world, `query` reads the sender's. Each action has its own permission: `pause` that of PMMP's
/// `/time stop`, `resume` that of `/time start`.
fn time(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let (needed, action): (Permission, TimeAction) = match args.string("mode") {
        Some("add") => (permission::COMMAND_TIME_ADD, time_add),
        Some("set") => (permission::COMMAND_TIME_SET, time_set),
        Some("query") => (permission::COMMAND_TIME_QUERY, time_query),
        Some("pause") => (permission::COMMAND_TIME_STOP, time_pause),
        Some("resume") => (permission::COMMAND_TIME_START, time_resume),
        _ => return Err(usage(cx, "time")),
    };
    require(cx, *sender, needed, "time")?;
    if let Some(clock) = args.string("clockName")
        && !is_named(clock, OVERWORLD_CLOCK_NAME)
    {
        return Err(CLOCK_UNKNOWN.error(&[clock]));
    }
    action(cx, *sender, args)
}

/// Tells whether `typed` names `name`, a namespaced name, with or without its `minecraft:`, in
/// its case (BDS's clocks and time markers).
fn is_named(typed: &str, name: &str) -> bool {
    typed == name || name.strip_prefix(NAMESPACE) == Some(typed)
}

/// Finds the world `sender` is in ([`sender_world`]).
fn world_of<'a>(cx: &'a Context<'_>, sender: Sender) -> &'a World {
    #[expect(
        clippy::expect_used,
        reason = "invariant: a sender's world, a player's or the default one, is loaded"
    )]
    cx.world(sender_world(cx, sender))
        .expect("the sender's world is loaded")
}

/// Lists every loaded world and its time.
fn world_times(cx: &Context<'_>) -> Vec<(world::Id, i64)> {
    cx.worlds().map(|w| (w.id(), w.time_ticks())).collect()
}

/// Moves the clock of `world` from `now` to `to`; its players hear of a change only, as BDS
/// tells them (P6 `clocks`).
fn move_clock(cx: &mut Context<'_>, world: world::Id, now: i64, to: i64) {
    if to != now {
        cx.set_time(world, to);
    }
}

/// Moves every world's clock by `amount`, unless that takes one below 0 (BDS then names the
/// smallest amount it takes) (`/time add <amount>`). The answer has the sender's world's new
/// time.
fn time_add(cx: &mut Context<'_>, sender: Sender, args: &Args) -> Result<Output> {
    let amount = i64::from(args.int("amount").ok_or_else(|| usage(cx, "time"))?);
    let sender_now = world_of(cx, sender).time_ticks();
    let worlds = world_times(cx);
    let smallest = worlds
        .iter()
        .map(|&(_, now)| now)
        .fold(sender_now, Ord::min)
        .saturating_neg();
    if amount < smallest {
        return Err(CLOCK_TOO_SMALL.error(&[&smallest.to_string()]));
    }
    for (id, now) in worlds {
        move_clock(cx, id, now, now.saturating_add(amount));
    }
    let new = sender_now.saturating_add(amount).to_string();
    Ok(announce(
        cx,
        sender,
        CLOCK_ADDED,
        &[&amount.to_string(), OVERWORLD_CLOCK_NAME, &new],
    ))
}

/// Puts every world's clock at `amount`, or at the marker's time from its own time now; no
/// clock goes below 0 (`/time set <amount>` and `/time set <marker> [next|previous|stay]`, `next`
/// when left out). The answer has the sender's world's new time.
fn time_set(cx: &mut Context<'_>, sender: Sender, args: &Args) -> Result<Output> {
    let to = match (args.int("amount"), args.string("timeMarkerName")) {
        (Some(amount), _) => SetTo::Ticks(i64::from(amount)),
        (None, Some(typed)) => {
            let Some(&(_, _, time_of_day)) =
                MARKERS.iter().find(|&&(_, name, _)| is_named(typed, name))
            else {
                return Err(CLOCK_MARKER_UNKNOWN.error(&[typed, OVERWORLD_CLOCK_NAME]));
            };
            let occurrence = args.string("timeMarkerOccurrence").unwrap_or("next");
            SetTo::Marker(i64::from(time_of_day), occurrence)
        }
        (None, None) => return Err(usage(cx, "time")),
    };
    let shown = to.time_from(world_of(cx, sender).time_ticks());
    let worlds = world_times(cx);
    if shown < 0 || worlds.iter().any(|&(_, now)| to.time_from(now) < 0) {
        return Err(CLOCK_TOO_SMALL.error(&["0"]));
    }
    for (id, now) in worlds {
        move_clock(cx, id, now, to.time_from(now));
    }
    Ok(announce(
        cx,
        sender,
        CLOCK_SET,
        &[OVERWORLD_CLOCK_NAME, &shown.to_string()],
    ))
}

/// Reads the sender's world's clock, the time of day, its age ([`World::age_ticks`]) or the days
/// passed (`/time query <time|daytime|gametime|day>`; `of`: `time` only).
fn time_query(cx: &mut Context<'_>, sender: Sender, args: &Args) -> Result<Output> {
    let world = world_of(cx, sender);
    let now = world.time_ticks();
    Ok(
        match args.string("time").or_else(|| args.string("clockQuery")) {
            Some("time") => CLOCK_QUERY_TIME.output(&[OVERWORLD_CLOCK_NAME, &now.to_string()]),
            Some("daytime") => TIME_QUERY_DAYTIME.output(&[&now.rem_euclid(DAY).to_string()]),
            Some("gametime") => TIME_QUERY_GAMETIME.output(&[&world.age_ticks().to_string()]),
            Some("day") => TIME_QUERY_DAY.output(&[&now.div_euclid(DAY).to_string()]),
            _ => return Err(usage(cx, "time")),
        },
    )
}

/// Stops every world's clock ([`Context::stop_time`]) (`/time pause`). The answer has the
/// sender's world's time.
#[expect(
    clippy::unnecessary_wraps,
    reason = "consistency: one signature with the other `/time` actions ([`TimeAction`])"
)]
fn time_pause(cx: &mut Context<'_>, sender: Sender, _: &Args) -> Result<Output> {
    let now = world_of(cx, sender).time_ticks().to_string();
    for (id, _) in world_times(cx) {
        cx.stop_time(id);
    }
    Ok(announce(
        cx,
        sender,
        CLOCK_PAUSED,
        &[OVERWORLD_CLOCK_NAME, &now],
    ))
}

/// Runs every world's clock again ([`Context::start_time`]) (`/time resume`). The answer has the
/// sender's world's time.
#[expect(
    clippy::unnecessary_wraps,
    reason = "consistency: one signature with the other `/time` actions ([`TimeAction`])"
)]
fn time_resume(cx: &mut Context<'_>, sender: Sender, _: &Args) -> Result<Output> {
    let now = world_of(cx, sender).time_ticks().to_string();
    for (id, _) in world_times(cx) {
        cx.start_time(id);
    }
    Ok(announce(
        cx,
        sender,
        CLOCK_RESUMED,
        &[OVERWORLD_CLOCK_NAME, &now],
    ))
}

/// Reads the number PHP reads from `data` (`(int)`): 0 when it is not one.
fn data_int(data: Option<&str>, default: i32) -> i32 {
    data.map_or(default, |d| d.parse().unwrap_or(0))
}

/// Finds the block state of the item named `data`, if it is a block other than air.
fn data_block(data: Option<&str>) -> Option<crate::block::State> {
    let state = crate::command::parse::Names::item(data?)?.block()?;
    (state.name() != "minecraft:air").then_some(state)
}

/// Finds the particle `/particle` names with its `data` (PMMP `ParticleCommand::getParticle`).
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

/// Shows `count` particles (1 to 1,024) spread at random up to the distances on each axis, in
/// the sender's world (`/particle <name> <x> <y> <z> <xd> <yd> <zd> [count] [data]`, PMMP
/// `ParticleCommand`). The spread comes from the engine's random stream of this command run, so
/// a replay shows the same and two runs in one tick spread apart. Particle numbers are
/// gophertunnel's.
fn particle(cx: &mut Context<'_>, sender: &Sender, args: &Args) -> Result<Output> {
    let name = args.string("name").unwrap_or_default();
    let Some(effect) = particle_of(name, args.string("data")) else {
        return Err(usage(cx, "particle"));
    };
    let count = bounded(args.int("count").unwrap_or(1).max(1), 1, MAX_PARTICLES)?;
    let center = in_world(args.position("position").unwrap_or(Vec3::ZERO));
    let spread = Vec3::new(
        args.float("xd").unwrap_or(0.0),
        args.float("yd").unwrap_or(0.0),
        args.float("zd").unwrap_or(0.0),
    );
    let world = sender_world(cx, *sender);
    let seed = cx.default_world().seed();
    let key = random::key(PARTICLE_SPREAD, &[cx.state.commands.runs]);
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

/// Asks for a save of everything at the end of the tick, as one commit ([`Context::save`])
/// (`/save-all`, PMMP `SaveCommand`). PMMP's "Save completed in ... seconds" is left out: the
/// commit ends later, off the tick.
fn save_all(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    let out = announce(cx, *sender, SAVE_START, &[]);
    cx.save();
    out
}

/// Asks for a save of everything, then a backup of the saved data files while the server runs
/// ([`Context::backup`]) (`/backup`, Anchor's own). The backup ends later, off the tick, so the
/// log tells its result, as for `/save-all`'s commit.
fn backup(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Result<Output> {
    if !cx.backup() {
        return Err(BACKUP_OFF.error(&[]));
    }
    Ok(announce(cx, *sender, BACKUP_START, &[]))
}

/// Turns the periodic save off until the server stops or `/save-on` (`/save-off`, PMMP
/// `SaveOffCommand`; [`Context::set_auto_save`] says what still saves).
fn save_off(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    cx.set_auto_save(false);
    announce(cx, *sender, SAVE_DISABLED, &[])
}

/// Turns the periodic save back on (`/save-on`, PMMP `SaveOnCommand`).
fn save_on(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    cx.set_auto_save(true);
    announce(cx, *sender, SAVE_ENABLED, &[])
}

/// Shows the uptime by the wall clock and in ticks, the TPS and tick times now and averaged over
/// the last 20 ticks, the resident memory, the threads, the players, and each loaded world's
/// chunks, entities and block entities (`/status`, PMMP `StatusCommand`, in Anchor's own lines).
/// The uptime, memory and tick times are measured off the tick and read from the input log, so a
/// replay sees the same values; the uptime and memory are at most a second old.
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

/// Writes the memory line of `/status` for `bytes` resident (0: unknown), in MiB to a tenth.
pub(super) fn memory_line(bytes: u64) -> String {
    if bytes == 0 {
        return "Memory: unknown".to_owned();
    }
    let tenths = bytes.saturating_mul(10).saturating_add(MIB / 2) / MIB;
    format!("Memory: {}.{} MiB resident", tenths / 10, tenths % 10)
}

/// Stops the server after this tick (`/stop`, PMMP `StopCommand`).
fn stop(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    let out = announce(cx, *sender, STOP_START, &[]);
    cx.shutdown();
    out
}

/// Shows one line with [`VERSION_LINE`] (`/version`, aliases `/ver` and `/about`, PMMP
/// `VersionCommand`, in Anchor's layout). PMMP's PHP lines and plugin lookup are left out.
fn version(_: &mut Context<'_>, _: &Sender, _: &Args) -> Output {
    VERSION_LINE.output(&[
        Context::NAME,
        Context::ANCHOR_VERSION,
        Context::VERSION,
        &PROTOCOL_VERSION.to_string(),
        std::env::consts::OS,
    ])
}
