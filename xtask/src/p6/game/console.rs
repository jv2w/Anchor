//! Anchor's console for the game and API scenarios (`p6 game anchor`, `p6 api anchor`): the
//! commands the bot asks for (see [`Command`]) carried out through Anchor's public task API
//! ([`anchor::Context`]), so the scenarios need no test-only entry point.
//!
//! The harness parses each command line ([`parse`]) and hands it to a task that Anchor runs every
//! tick (`ServerBuilder::every(1, ..)`, [`console`]). The task runs the commands in order in the
//! tick's serial step; a command that cannot run yet (its player has not joined, its chunk is not
//! loaded) waits for a later tick, up to [`MAX_WAIT_TICKS`]. Each command gets one [`Report`]
//! back, which [`Console::settle`] prints after each scenario.
//!
//! Where Anchor has no counterpart, the command is accepted with a note: villager NPCs have no AI
//! (`effect` on them changes nothing), Anchor spawns no mobs and its weather has no cycle
//! (`domobspawning`, `doweathercycle`), and a wind charge is not in its item table. The day
//! cycle and the fall and drowning damage (`dodaylightcycle`, `falldamage`, `drowningdamage`) are
//! accepted with a note too: Anchor takes them from its settings, and the API session turns the day
//! cycle off through its settings while the game session leaves it on.

use std::collections::VecDeque;
use std::io::Write;
use std::num::NonZeroU32;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::Duration;

use anchor::block::State;
use anchor::config::Difficulty;
use anchor::custom::Offer;
use anchor::entity::Id;
use anchor::form::{Button, Custom, Form, Image, Menu, Modal};
use anchor::item::{Stack, Type};
use anchor::math::Vec3;
use anchor::nbt::Compound;
use anchor::player::Player;
use anchor::world::{self, BlockPos};
use anchor::{Context, effect, inventory};

use crate::p6;

/// The task's period: every tick.
pub(crate) const EVERY_TICK: NonZeroU32 = NonZeroU32::MIN;
/// The actor variant of a farmer villager (`EntityVariant` of `minecraft:villager_v2`).
const FARMER: u8 = 1;
/// The most blocks one `fill` may change (BDS's limit).
const MAX_FILL: u64 = 32_768;
/// The most items one `give` may hand out (BDS's limit).
const MAX_GIVE: u32 = 32_767;
/// Commands waiting in the task at most; the harness's send blocks beyond it.
const MAX_PENDING: usize = 256;
/// How long a command may wait for its player or chunk: 10 seconds.
const MAX_WAIT_TICKS: u32 = 200;
/// The selector of every entity that is not a player, the only entity selector the bot uses.
const NON_PLAYERS: &str = "@e[type=!player]";
/// How long [`Console::settle`] waits for the reports of the commands sent.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

/// A parsed console command.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Command {
    /// `tp <player> <x> <y> <z>`: the feet position.
    Teleport { player: String, feet: Vec3 },
    /// `give <player> <item> [count]`, split into stacks of the item's stack size.
    Give { player: String, stacks: Vec<Stack> },
    /// `xp <points> <player>` or `xp <levels>L <player>`.
    Experience { player: String, amount: Xp },
    /// `setblock` and `fill`: every block of the box, corners included.
    Fill {
        from: BlockPos,
        to: BlockPos,
        state: State,
    },
    /// `summon villager <x> <y> <z> ...`: a farmer villager NPC with [`farmer_offers`].
    Villager { pos: Vec3 },
    /// `effect <player> <effect> [seconds] [amplifier] [hideParticles]`: the effect with the
    /// ticks, amplifier and particles given, or none for 0 seconds.
    Effect {
        player: String,
        effect: effect::Instance,
    },
    /// `kill @e[type=!player]`: the villagers this console summoned.
    KillNonPlayers,
    /// `kill <player>`: [`anchor::player::Player::kill`].
    Kill(String),
    /// `summon ender_crystal <x> <y> <z>`: [`Context::spawn_end_crystal`].
    EndCrystal { pos: Vec3 },
    /// `gamerule randomtickspeed <n>`.
    RandomTickSpeed(u32),
    /// `op <player>`: [`Context::op`].
    Op(String),
    /// `kick <player> <reason>`: [`anchor::player::Player::kick`].
    Kick { player: String, reason: String },
    /// `scriptevent p6:<kind>`: the forms scenario's form of that kind to every player, as the
    /// BDS script pack shows it (`xtask/data/p6/forms-pack`, [`p6_form`]).
    Form(FormKind),
    /// `summon snowball|egg <x> <y> <z>`: the projectile, dropped without motion.
    Projectile {
        projectile: anchor::entity::Projectile,
        pos: Vec3,
    },
    /// `weather clear|rain|thunder`: [`Context::set_weather`] in the default world.
    Weather(world::Weather),
    /// `difficulty peaceful|easy|normal|hard`: [`Context::set_difficulty`].
    Difficulty(Difficulty),
    /// `summon lightning_bolt <x> <y> <z>` (whole coordinates, the bolt at the middle of the
    /// block's floor as BDS puts it): [`Context::strike_lightning`] with BDS's damage and fire.
    Lightning { pos: Vec3 },
    /// A command Anchor has no counterpart for; the text says why it is fine.
    Accepted(&'static str),
}

/// The amount of an `xp` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Xp {
    /// Experience points.
    Points(u32),
    /// Levels added to the player's level, keeping the progress.
    Levels(u32),
}

/// A kind of form the forms scenario asks for (`scriptevent p6:<kind>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormKind {
    Modal,
    Menu,
    Custom,
    Closed,
}

impl FormKind {
    /// The kind a command names, if it is one.
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "modal" => Some(Self::Modal),
            "menu" => Some(Self::Menu),
            "custom" => Some(Self::Custom),
            "closed" => Some(Self::Closed),
            _ => None,
        }
    }

    /// The name in the command.
    fn name(self) -> &'static str {
        match self {
            Self::Modal => "modal",
            Self::Menu => "menu",
            Self::Custom => "custom",
            Self::Closed => "closed",
        }
    }
}

/// What became of one command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    /// The command line.
    pub(crate) line: String,
    /// Done with an optional note, or failed with the reason.
    pub(crate) result: Result<Option<String>, String>,
}

/// What running a command once did.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Done(Option<String>),
    /// It cannot run yet; the text says what it waits for.
    Wait(String),
    Failed(String),
}

/// The harness side of the console.
#[derive(Debug)]
pub(crate) struct Console {
    commands: SyncSender<(String, Command)>,
    reports: Receiver<Report>,
    /// Commands sent and not reported yet.
    outstanding: usize,
    /// Reports of earlier scenarios' commands that a [`Console::settle`] gave up waiting for and
    /// that are still to arrive, before any report of a command sent since.
    owed: usize,
    /// Lines that did not parse, reported by the next [`Console::settle`].
    refused: Vec<Report>,
    /// What the players answered to the forms of `scriptevent` commands, as notes.
    answers: Receiver<String>,
}

/// The console and the task for `ServerBuilder::every(EVERY_TICK, ..)`.
pub(crate) fn console() -> (Console, impl FnMut(&mut Context<'_>) + Send + 'static) {
    let (commands, received) = channel(MAX_PENDING);
    let (reported, reports) = channel(MAX_PENDING);
    let (answered, answers) = channel(MAX_PENDING);
    let mut pending: VecDeque<(String, Command, u32)> = VecDeque::with_capacity(MAX_PENDING);
    let mut villagers: Vec<Id> = Vec::new();
    let task = move |ctx: &mut Context<'_>| {
        while pending.len() < MAX_PENDING {
            let Ok((line, command)) = received.try_recv() else {
                break;
            };
            pending.push_back((line, command, 0));
        }
        while let Some((line, command, waited)) = pending.front_mut() {
            let result = match execute(ctx, command, &mut villagers, &answered) {
                Outcome::Wait(_) if *waited < MAX_WAIT_TICKS => {
                    *waited += 1;
                    break;
                }
                Outcome::Wait(why) => Err(format!("gave up waiting: {why}")),
                Outcome::Done(note) => Ok(note),
                Outcome::Failed(why) => Err(why),
            };
            let report = Report {
                line: std::mem::take(line),
                result,
            };
            pending.pop_front();
            // The harness drains the reports after each scenario; a full queue loses the report,
            // and `settle` then times out naming the missing count.
            if let Err(TrySendError::Disconnected(_)) = reported.try_send(report) {
                return;
            }
        }
    };
    let console = Console {
        commands,
        reports,
        outstanding: 0,
        owed: 0,
        refused: Vec::new(),
        answers,
    };
    (console, task)
}

impl Console {
    /// Hands `line` to Anchor's task, or keeps the reason it does not parse for [`Self::settle`].
    pub(crate) fn send(&mut self, line: &str) -> Result<(), String> {
        match parse(line) {
            Ok(command) => {
                self.commands
                    .send((line.to_owned(), command))
                    .map_err(|_| "Anchor's console task is gone".to_owned())?;
                self.outstanding += 1;
            }
            Err(why) => self.refused.push(Report {
                line: line.to_owned(),
                result: Err(why),
            }),
        }
        Ok(())
    }

    /// Waits for the reports of every command sent, prints the notes and failures, and returns
    /// whether every command ran.
    pub(crate) fn settle(&mut self, out: &mut impl Write) -> Result<bool, String> {
        let mut reports = std::mem::take(&mut self.refused);
        let mut late = Vec::new();
        let deadline = p6::now() + SETTLE_TIMEOUT;
        while self.outstanding > 0 {
            match self.reports.try_recv() {
                Ok(report) if self.owed > 0 => {
                    self.owed -= 1;
                    late.push(report);
                }
                Ok(report) => {
                    reports.push(report);
                    self.outstanding -= 1;
                }
                Err(_) if p6::now() < deadline => p6::pause(),
                Err(_) => {
                    let missing = std::mem::take(&mut self.outstanding);
                    self.owed += missing;
                    reports.push(Report {
                        line: format!("({missing} commands)"),
                        result: Err(format!("no report within {SETTLE_TIMEOUT:?}")),
                    });
                }
            }
        }
        while let Ok(answer) = self.answers.try_recv() {
            p6::say(out, &format!("note anchor form: {answer}"))?;
        }
        for Report { line, result } in &late {
            let text = format!(
                "note anchor console `{line}` answered after an earlier scenario gave up on it: \
                 {result:?}"
            );
            p6::say(out, &text)?;
        }
        let mut ran = true;
        for Report { line, result } in &reports {
            let text = match result {
                Ok(None) => continue,
                Ok(Some(note)) => format!("note anchor console `{line}`: {note}"),
                Err(why) => {
                    ran = false;
                    format!("FAIL anchor console `{line}`: {why}")
                }
            };
            p6::say(out, &text)?;
        }
        Ok(ran)
    }
}

/// A bounded channel between the harness and Anchor's task.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: the harness hands the task its commands through a channel"
)]
fn channel<T>(bound: usize) -> (SyncSender<T>, Receiver<T>) {
    std::sync::mpsc::sync_channel(bound)
}

/// Parses a console command line in the form the bot writes (BDS syntax, absolute coordinates).
pub(crate) fn parse(line: &str) -> Result<Command, String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        ["tp", player, x, y, z] => Ok(Command::Teleport {
            player: (*player).to_owned(),
            feet: Vec3::new(number(x)?, number(y)?, number(z)?),
        }),
        ["give", _, "wind_charge", ..] => Ok(Command::Accepted(
            "Anchor's item table has no wind charge: the numbers scenario records BDS's only",
        )),
        ["give", player, item, rest @ ..] => give(player, item, rest),
        ["xp", amount, player] => Ok(Command::Experience {
            player: (*player).to_owned(),
            amount: experience(amount)?,
        }),
        ["setblock", x, y, z, ..] => {
            let at = block_pos(x, y, z)?;
            Ok(Command::Fill {
                from: at,
                to: at,
                state: block_spec(words.get(4..).unwrap_or_default())?,
            })
        }
        ["fill", x1, y1, z1, x2, y2, z2, ..] => {
            let (from, to) = (block_pos(x1, y1, z1)?, block_pos(x2, y2, z2)?);
            let size = |a: i32, b: i32| i64::from(a).abs_diff(i64::from(b)) + 1;
            let volume = size(from.x, to.x) * size(from.y, to.y) * size(from.z, to.z);
            if volume > MAX_FILL {
                return Err(format!("{volume} blocks is more than {MAX_FILL}"));
            }
            Ok(Command::Fill {
                from,
                to,
                state: block_spec(words.get(7..).unwrap_or_default())?,
            })
        }
        [
            "summon",
            "villager" | "minecraft:villager" | "minecraft:villager_v2",
            x,
            y,
            z,
            ..,
        ] => Ok(Command::Villager {
            pos: Vec3::new(number(x)?, number(y)?, number(z)?),
        }),
        ["effect", target, name, ..] if *target == NON_PLAYERS => {
            effect_type(name)?;
            Ok(Command::Accepted(
                "villager NPCs have no AI: an effect changes nothing",
            ))
        }
        ["effect", player, name, rest @ ..] => {
            let seconds = rest.first().map_or(Ok(30), |s| {
                s.parse::<u32>().map_err(|_| format!("bad seconds {s:?}"))
            })?;
            let amplifier = rest.get(1).map_or(Ok(0), |a| {
                a.parse::<u8>().map_err(|_| format!("bad amplifier {a:?}"))
            })?;
            let hide = rest.get(2).map_or(Ok(false), |h| {
                h.parse::<bool>()
                    .map_err(|_| format!("bad hideParticles {h:?}"))
            })?;
            Ok(Command::Effect {
                player: (*player).to_owned(),
                effect: effect_type(name)?
                    .instance()
                    .with_duration_ticks(seconds.saturating_mul(20))
                    .with_amplifier(amplifier)
                    .with_visible(!hide),
            })
        }
        ["kill", target] if *target == NON_PLAYERS => Ok(Command::KillNonPlayers),
        ["gamerule", rule, value] => match (rule.to_ascii_lowercase().as_str(), *value) {
            ("randomtickspeed", speed) => speed
                .parse()
                .map(Command::RandomTickSpeed)
                .map_err(|_| format!("bad random tick speed {speed:?}")),
            ("domobspawning", "false") => Ok(Command::Accepted("Anchor spawns no mobs")),
            ("doweathercycle", "false") => Ok(Command::Accepted("Anchor's weather has no cycle")),
            ("dodaylightcycle", "false") => Ok(Command::Accepted(
                "Anchor takes the day cycle from its settings (daylight-cycle)",
            )),
            ("falldamage" | "drowningdamage", "false" | "true") => Ok(Command::Accepted(
                "Anchor takes these rules from its settings (fall-damage, drowning), not a command",
            )),
            _ => Err(format!("game rule {rule} {value} is not supported")),
        },
        _ => parse_api(&words),
    }
}

/// `give <player> <item> [count] [data value]`.
fn give(player: &str, item: &str, rest: &[&str]) -> Result<Command, String> {
    let parse_count = |count: &str| {
        count
            .parse::<u32>()
            .ok()
            .filter(|c| (1..=MAX_GIVE).contains(c))
            .ok_or_else(|| format!("bad count {count:?}"))
    };
    let (count, meta) = match rest {
        [] => (1, 0),
        [count] => (parse_count(count)?, 0),
        [count, meta] => (
            parse_count(count)?,
            meta.parse::<u16>()
                .map_err(|_| format!("bad data value {meta:?}"))?,
        ),
        _ => return Err("give takes a player, an item, a count and a data value".to_owned()),
    };
    Ok(Command::Give {
        player: player.to_owned(),
        stacks: stacks(item_type_with(item, meta)?, count),
    })
}

/// Parses a console command of the API scenarios (`p6 api`).
fn parse_api(words: &[&str]) -> Result<Command, String> {
    match words {
        ["op", player] => Ok(Command::Op((*player).to_owned())),
        ["kick", player, reason @ ..] => Ok(Command::Kick {
            player: (*player).to_owned(),
            reason: reason.join(" "),
        }),
        ["scriptevent", id] => id
            .strip_prefix("p6:")
            .and_then(FormKind::from_name)
            .map(Command::Form)
            .ok_or_else(|| format!("no P6 form {id}")),
        ["kill", player] if !player.starts_with('@') => Ok(Command::Kill((*player).to_owned())),
        [
            "summon",
            "ender_crystal" | "minecraft:ender_crystal",
            x,
            y,
            z,
        ] => Ok(Command::EndCrystal {
            pos: Vec3::new(number(x)?, number(y)?, number(z)?),
        }),
        ["summon", entity @ ("snowball" | "egg"), x, y, z] => Ok(Command::Projectile {
            projectile: if *entity == "egg" {
                anchor::entity::Projectile::Egg
            } else {
                anchor::entity::Projectile::Snowball
            },
            pos: Vec3::new(number(x)?, number(y)?, number(z)?),
        }),
        ["weather", weather] => match *weather {
            "clear" => Ok(Command::Weather(world::Weather::Clear)),
            "rain" => Ok(Command::Weather(world::Weather::Rain)),
            "thunder" => Ok(Command::Weather(world::Weather::Thunder)),
            _ => Err(format!("no weather {weather}")),
        },
        ["difficulty", difficulty] => match *difficulty {
            "peaceful" => Ok(Command::Difficulty(Difficulty::Peaceful)),
            "easy" => Ok(Command::Difficulty(Difficulty::Easy)),
            "normal" => Ok(Command::Difficulty(Difficulty::Normal)),
            "hard" => Ok(Command::Difficulty(Difficulty::Hard)),
            _ => Err(format!("no difficulty {difficulty}")),
        },
        [
            "summon",
            "lightning_bolt" | "minecraft:lightning_bolt",
            x,
            y,
            z,
        ] => {
            let at = block_pos(x, y, z)?;
            Ok(Command::Lightning {
                pos: Vec3::new(
                    f64::from(at.x) + 0.5,
                    f64::from(at.y),
                    f64::from(at.z) + 0.5,
                ),
            })
        }
        _ => Err("not a command the P6 console knows".to_owned()),
    }
}

/// A finite coordinate.
fn number(text: &str) -> Result<f64, String> {
    text.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| format!("bad coordinate {text:?}"))
}

/// A block position of whole coordinates.
fn block_pos(x: &str, y: &str, z: &str) -> Result<BlockPos, String> {
    let c = |t: &str| {
        t.parse::<i32>()
            .map_err(|_| format!("bad coordinate {t:?}"))
    };
    Ok(BlockPos::new(c(x)?, c(y)?, c(z)?))
}

/// The block state of `name ["key"=value,...] [replace]`.
fn block_spec(words: &[&str]) -> Result<State, String> {
    let words = match words.split_last() {
        Some((&"replace", rest)) => rest,
        Some((&mode @ ("keep" | "destroy" | "hollow" | "outline"), _)) => {
            return Err(format!("mode {mode} is not supported"));
        }
        _ => words,
    };
    p6::step_state(&words.join(" "))
}

/// The item type of a `give` name: an item, else the item of the block's default state.
fn item_type(name: &str) -> Result<Type, String> {
    item_type_with(name, 0)
}

/// [`item_type`] with a data value (the item's meta: a potion's type).
fn item_type_with(name: &str, meta: u16) -> Result<Type, String> {
    let full = format!("minecraft:{name}");
    Type::from_name(&full, meta, None, &Compound::new())
        .or_else(|| {
            let state = p6::step_state(name).ok()?;
            match state.item() {
                anchor::block::ItemForm::Item(t) => Some(t),
                _ => None,
            }
        })
        .ok_or_else(|| format!("no item {full}"))
}

/// `count` of `item` in stacks of its stack size.
fn stacks(item: Type, count: u32) -> Vec<Stack> {
    let size = u32::from(item.max_stack_size().max(1));
    let mut left = count;
    let mut out = Vec::with_capacity(usize::try_from(count.div_ceil(size)).unwrap_or_default());
    while left > 0 {
        let n = left.min(size);
        out.push(Stack::new(item, u8::try_from(n).unwrap_or(u8::MAX)));
        left -= n;
    }
    out
}

/// An `xp` amount: points, or `<n>L` levels.
fn experience(amount: &str) -> Result<Xp, String> {
    let bad = |_| format!("bad experience amount {amount:?}");
    match amount.strip_suffix(['L', 'l']) {
        Some(levels) => levels.parse().map(Xp::Levels).map_err(bad),
        None => amount.parse().map(Xp::Points).map_err(bad),
    }
}

/// The effect type of a BDS effect name (`slowness`, `instant_health`).
fn effect_type(name: &str) -> Result<effect::Type, String> {
    let wanted: String = name.chars().filter(|c| *c != '_').collect();
    effect::Type::all()
        .iter()
        .copied()
        .find(|t| format!("{t:?}").eq_ignore_ascii_case(&wanted))
        .ok_or_else(|| format!("no effect {name}"))
}

/// The offers of the console's villager: a farmer's first-tier trades as BDS sends them (P6,
/// `xtask/data/p6/facts.md`: payments of any damage, price multipliers 0.05 and 0; the trade module numbers
/// them, `netId`).
fn farmer_offers() -> Result<Vec<Offer>, String> {
    let offer = |buy: (&str, u8), sell: (&str, u8), trader_exp: u32| {
        let buy = Stack::new(item_type(buy.0)?, buy.1);
        let sell = Stack::new(item_type(sell.0)?, sell.1);
        Ok(Offer::new(buy, sell, 16)
            .with_any_damage()
            .with_trader_experience(trader_exp)
            .with_price_multipliers(0.05, 0.0))
    };
    [
        (("wheat", 20), ("emerald", 1), 2),
        (("potato", 26), ("emerald", 1), 2),
        (("carrot", 22), ("emerald", 1), 2),
        (("beetroot", 15), ("emerald", 1), 2),
        (("emerald", 1), ("bread", 6), 1),
    ]
    .into_iter()
    .map(|(buy, sell, exp)| offer(buy, sell, exp))
    .collect()
}

/// Runs `command` once on Anchor; the answers to the forms it sends go to `answers`.
fn execute(
    ctx: &mut Context<'_>,
    command: &Command,
    villagers: &mut Vec<Id>,
    answers: &SyncSender<String>,
) -> Outcome {
    match command {
        Command::Teleport { player, feet } => {
            let Some((id, world)) = ctx
                .player_exact(player)
                .and_then(|id| Some((id, ctx.player(id)?.world())))
            else {
                return not_joined(player);
            };
            match ctx.teleport(id, world, *feet) {
                Ok(()) => Outcome::Done(None),
                Err(e) => Outcome::Failed(e.to_string()),
            }
        }
        Command::Give { player, stacks } => {
            let Some(p) = joined(ctx, player) else {
                return not_joined(player);
            };
            let mut main = p.inventory_mut(inventory::Kind::Main);
            let left: u32 = stacks.iter().map(|s| u32::from(main.add(s).count())).sum();
            Outcome::Done((left > 0).then(|| format!("{left} did not fit")))
        }
        Command::Experience { player, amount } => {
            let Some(p) = joined(ctx, player) else {
                return not_joined(player);
            };
            match *amount {
                Xp::Points(points) => {
                    p.add_xp(points);
                    Outcome::Done(None)
                }
                Xp::Levels(levels) => match p.add_xp_levels(levels) {
                    Ok(()) => Outcome::Done(None),
                    Err(e) => Outcome::Failed(e.to_string()),
                },
            }
        }
        Command::Fill { from, to, state } => fill(ctx, *from, *to, *state),
        Command::Villager { pos } => {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "safety: parse() keeps coordinates finite; `as` saturates"
            )]
            let at = BlockPos::new(
                pos.x.floor() as i32,
                pos.y.floor() as i32,
                pos.z.floor() as i32,
            );
            if !ctx.default_world().is_chunk_loaded(at.chunk()) {
                return Outcome::Wait("the villager's chunk is not loaded".to_owned());
            }
            let offers = match farmer_offers() {
                Ok(offers) => offers,
                Err(e) => return Outcome::Failed(e),
            };
            match ctx.spawn_villager(world::Id::DEFAULT, *pos, FARMER, &offers) {
                Ok(id) => {
                    villagers.push(id);
                    Outcome::Done(None)
                }
                Err(e) => Outcome::Failed(format!("no villager NPC was spawned: {e}")),
            }
        }
        Command::Effect { player, effect } => {
            let Some(p) = joined(ctx, player) else {
                return not_joined(player);
            };
            // As on BDS, 0 seconds takes the effect away.
            if effect.is_expired() {
                let removed = p.remove_effect(effect.effect());
                return Outcome::Done((!removed).then(|| "not removed".to_owned()));
            }
            let added = p.add_effect(*effect);
            Outcome::Done((!added).then(|| "not added".to_owned()))
        }
        Command::KillNonPlayers => {
            let removed = villagers
                .drain(..)
                .filter_map(|id| ctx.entity_mut(id).map(|mut e| e.flag_for_despawn()))
                .count();
            Outcome::Done(Some(format!(
                "removed {removed} villagers (the harness removes only the villagers it summoned)"
            )))
        }
        Command::RandomTickSpeed(speed) => {
            if ctx.set_random_tick_speed(*speed) {
                Outcome::Done(None)
            } else {
                Outcome::Failed(format!("random tick speed {speed} is above the limit"))
            }
        }
        Command::Accepted(note) => Outcome::Done(Some((*note).to_owned())),
        Command::Op(_)
        | Command::Kick { .. }
        | Command::Form(_)
        | Command::Projectile { .. }
        | Command::Kill(_)
        | Command::EndCrystal { .. }
        | Command::Weather(_)
        | Command::Difficulty(_)
        | Command::Lightning { .. } => execute_api(ctx, command, answers),
    }
}

/// Runs a command of the API scenarios (`p6 api`) once on Anchor.
fn execute_api(ctx: &mut Context<'_>, command: &Command, answers: &SyncSender<String>) -> Outcome {
    let done = |result: Result<(), String>| match result {
        Ok(()) => Outcome::Done(None),
        Err(e) => Outcome::Failed(e),
    };
    match command {
        Command::Op(player) => done(ctx.op(player).map_err(|e| e.to_string())),
        Command::Kick { player, reason } => {
            let Some(p) = joined(ctx, player) else {
                return not_joined(player);
            };
            done(p.kick(reason.as_str()).map_err(|e| e.to_string()))
        }
        Command::Form(kind) => {
            let kind = *kind;
            let players: Vec<_> = ctx.players().map(Player::id).collect();
            for player in players {
                let answers = answers.clone();
                let sent = ctx.send_form(player, p6_form(kind), move |_, _, response| {
                    match answers.try_send(format!("{} answered {response:?}", kind.name())) {
                        // A full or closed queue loses the note only.
                        Ok(()) | Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {}
                    }
                });
                if let Err(e) = sent {
                    return Outcome::Failed(e.to_string());
                }
            }
            Outcome::Done(None)
        }
        Command::Kill(player) => {
            let Some(p) = joined(ctx, player) else {
                return not_joined(player);
            };
            if p.kill() {
                Outcome::Done(None)
            } else {
                Outcome::Failed(format!("{player} is already dead"))
            }
        }
        Command::EndCrystal { pos } => match ctx.spawn_end_crystal(world::Id::DEFAULT, *pos) {
            Some(_) => Outcome::Done(None),
            None => Outcome::Failed("no end crystal was spawned".to_owned()),
        },
        Command::Projectile { projectile, pos } => {
            match ctx.spawn_projectile(world::Id::DEFAULT, *pos, *projectile, Vec3::ZERO) {
                Some(_) => Outcome::Done(None),
                None => Outcome::Failed("no projectile was spawned".to_owned()),
            }
        }
        Command::Weather(weather) => done(
            ctx.set_weather(world::Id::DEFAULT, *weather)
                .then_some(())
                .ok_or_else(|| "the default world is not loaded".to_owned()),
        ),
        Command::Difficulty(difficulty) => done(
            ctx.set_difficulty(*difficulty)
                .then_some(())
                .ok_or_else(|| "a hardcore server keeps hard".to_owned()),
        ),
        Command::Lightning { pos } => {
            let bds = world::Lightning {
                damage: true,
                fire: true,
            };
            done(
                ctx.strike_lightning(world::Id::DEFAULT, *pos, bds)
                    .then_some(())
                    .ok_or_else(|| "no bolt was struck".to_owned()),
            )
        }
        _ => Outcome::Failed("not a command of the API scenarios".to_owned()),
    }
}

/// The player named `name` who has joined, to change.
fn joined<'c>(ctx: &'c mut Context<'_>, name: &str) -> Option<&'c mut Player> {
    let id = ctx.player_exact(name)?;
    ctx.player_mut(id)
}

/// The outcome of a command whose player has not joined yet.
fn not_joined(player: &str) -> Outcome {
    Outcome::Wait(format!("{player} has not joined"))
}

/// The forms scenario's form of `kind`, the same as the BDS script pack's
/// (`xtask/data/p6/forms-pack/scripts/main.js`); keep the two the same.
fn p6_form(kind: FormKind) -> Form {
    match kind {
        FormKind::Modal => Modal::new("P6 modal", "Modal body", "Yes", "No").into(),
        FormKind::Menu => Menu::new("P6 menu")
            .with_content("Menu body")
            .button("First")
            .header("Menu header")
            .label("Menu label")
            .divider()
            .button(
                Button::new("Second").with_image(Image::Path("textures/items/apple".to_owned())),
            )
            .into(),
        FormKind::Custom => Custom::new("P6 custom")
            .header("A header")
            .label("A label")
            .divider()
            .input("Name", "placeholder", "default")
            .toggle("Toggle", false)
            .slider("Slider", 0.0, 10.0, 1.0, 3.0)
            .dropdown("Dropdown", ["a", "b", "c"], 0)
            .into(),
        FormKind::Closed => Modal::new("P6 closed", "Closed body", "Yes", "No").into(),
    }
}

/// Sets every block of the box from `from` to `to`; waits while a chunk is not loaded (setting a
/// block again is harmless, so the whole box runs again).
fn fill(ctx: &mut Context<'_>, from: BlockPos, to: BlockPos, state: State) -> Outcome {
    let range = |a: i32, b: i32| a.min(b)..=a.max(b);
    for x in range(from.x, to.x) {
        for y in range(from.y, to.y) {
            for z in range(from.z, to.z) {
                match ctx
                    .default_world_mut()
                    .set_block(BlockPos::new(x, y, z), state)
                {
                    Ok(()) => {}
                    Err(world::Error::NotLoaded) => {
                        return Outcome::Wait(format!("the chunk of {x} {y} {z} is not loaded"));
                    }
                    Err(e) => return Outcome::Failed(format!("{x} {y} {z}: {e}")),
                }
            }
        }
    }
    Outcome::Done(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::p6::game;

    /// Every command line of the BDS game session (`target/p6/bds-game/*.jsonl`) and its setup.
    const SCENARIO_COMMANDS: &[&str] = &[
        "tp P6Craft -11.5 -60 -11.5",
        "give P6Craft oak_log 4",
        "fill -14 -60 -18 -10 -57 -14 glass",
        "fill -13 -60 -17 -11 -58 -15 water",
        "tp P6Craft -11.5 -60 -15.5",
        "tp P6Furnace -11.5 -60 -3.5",
        r#"setblock -10 -60 -4 furnace ["minecraft:cardinal_direction"="west"]"#,
        "give P6Furnace coal 1",
        "give P6Furnace raw_iron 2",
        "tp P6Enchant -11.5 -60 4.5",
        "fill -11 -60 2 -7 -59 6 bookshelf",
        "fill -10 -60 3 -8 -59 5 air",
        "fill -11 -60 4 -11 -59 4 air",
        "setblock -9 -60 4 enchanting_table",
        "xp 30L P6Enchant",
        "give P6Enchant iron_pickaxe 1",
        "give P6Enchant lapis_lazuli 3",
        "tp P6Crops -11.5 -60 12.5",
        "setblock -11 -61 12 farmland",
        r#"setblock -11 -60 12 wheat ["growth"=0]"#,
        "give P6Crops bone_meal 16",
        "tp P6Trade -11.5 -60 20.5",
        "give P6Trade emerald 64",
        "give P6Trade wheat 64",
        "give P6Trade potato 64",
        "give P6Trade carrot 64",
        "give P6Trade beetroot 64",
        "give P6Trade pumpkin 64",
        "give P6Trade melon_block 64",
        "summon villager -9.5 -60 20.5 0 0 minecraft:become_farmer P6Farmer",
        "summon villager -9.5 -60 20.5",
        "summon minecraft:villager -13.5 -60 20.5 0 0 minecraft:become_farmer",
        "effect @e[type=!player] slowness 600 255 true",
        "kill @e[type=!player]",
    ];

    #[test]
    fn every_command_the_scenarios_send_parses() {
        for line in SCENARIO_COMMANDS.iter().chain(game::SESSION_SETUP) {
            if let Err(e) = parse(line) {
                panic!("{line}: {e}");
            }
        }
    }

    #[test]
    fn commands_parse_into_their_api_calls() {
        assert_eq!(
            parse("tp P6Craft -11.5 -60 -11.5"),
            Ok(Command::Teleport {
                player: "P6Craft".to_owned(),
                feet: Vec3::new(-11.5, -60.0, -11.5),
            })
        );
        let Ok(Command::Give { stacks, .. }) = parse("give P6Trade emerald 100") else {
            panic!("give");
        };
        let counts: Vec<u8> = stacks.iter().map(Stack::count).collect();
        assert_eq!(counts, [64, 36]);
        assert_eq!(stacks[0].item().map(Type::name), Some("minecraft:emerald"));
        let Ok(Command::Give { stacks, .. }) = parse("give P6Craft oak_log 4") else {
            panic!("block item");
        };
        assert_eq!(stacks[0].item().map(Type::name), Some("minecraft:oak_log"));
        let Ok(Command::Fill { from, to, state }) =
            parse(r#"setblock -10 -60 -4 furnace ["minecraft:cardinal_direction"="west"]"#)
        else {
            panic!("setblock");
        };
        assert_eq!((from, to), (BlockPos::new(-10, -60, -4), from));
        assert_eq!(state.name(), "minecraft:furnace");
        assert_eq!(
            parse("gamerule randomtickspeed 0"),
            Ok(Command::RandomTickSpeed(0))
        );
        assert_eq!(parse("kill @e[type=!player]"), Ok(Command::KillNonPlayers));
        let Ok(Command::Effect { effect, .. }) = parse("effect P6Trade slowness 600 255 true")
        else {
            panic!("an effect");
        };
        assert_eq!(effect.effect(), effect::Type::Slowness);
        assert_eq!((effect.duration_ticks(), effect.amplifier()), (12_000, 255));
        assert!(!effect.is_visible(), "hideParticles");
        let Ok(Command::Effect { effect, .. }) = parse("effect P6Trade speed 10") else {
            panic!("an effect");
        };
        assert!(effect.is_visible());
        assert!(matches!(
            parse("summon villager -9.5 -60 20.5 0 0 minecraft:become_farmer P6Farmer"),
            Ok(Command::Villager { .. })
        ));
        assert_eq!(
            parse("weather thunder"),
            Ok(Command::Weather(world::Weather::Thunder))
        );
        assert_eq!(
            parse("difficulty peaceful"),
            Ok(Command::Difficulty(Difficulty::Peaceful))
        );
        assert_eq!(
            parse("summon lightning_bolt 4 -60 -20"),
            Ok(Command::Lightning {
                pos: Vec3::new(4.5, -60.0, -19.5)
            })
        );
    }

    #[test]
    fn bad_commands_are_refused_with_a_reason() {
        for line in [
            "",
            "tp P6Craft 1 2",
            "tp P6Craft nan 0 0",
            "give P6Craft no_such_item 1",
            "give P6Craft stick 0",
            "give P6Craft stick 40000",
            "fill 0 0 0 100 100 100 stone",
            "setblock 0 0 0 stone keep",
            "setblock 0 0 0 no_such_block",
            "xp 30Q P6Enchant",
            "summon zombie 0 0 0",
            "kill @a",
            "gamerule pvp false",
            "gamerule randomtickspeed fast",
            "effect P6Craft no_such_effect",
            "weather snow",
            "summon lightning_bolt 4.5 -60 -20",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
    }

    #[test]
    fn experience_amounts_are_points_or_levels() {
        for (amount, expected) in [
            ("7", Xp::Points(7)),
            ("30L", Xp::Levels(30)),
            ("30l", Xp::Levels(30)),
            ("30000L", Xp::Levels(30_000)),
        ] {
            assert_eq!(experience(amount), Ok(expected), "{amount}");
        }
    }

    #[test]
    fn a_report_that_came_late_is_not_the_next_scenarios() {
        let (commands, _received) = channel(1);
        let (reported, reports) = channel(2);
        let (_answered, answers) = channel(1);
        let mut console = Console {
            commands,
            reports,
            outstanding: 1,
            owed: 1,
            refused: Vec::new(),
            answers,
        };
        for (line, result) in [("late", Err("no player".to_owned())), ("current", Ok(None))] {
            let report = Report {
                line: line.to_owned(),
                result,
            };
            reported.try_send(report).expect("room for the report");
        }
        let mut out = Vec::new();
        assert_eq!(console.settle(&mut out), Ok(true));
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("`late` answered after"), "{text}");
    }

    #[test]
    fn scriptevent_names_the_form_kind() {
        for kind in [
            FormKind::Modal,
            FormKind::Menu,
            FormKind::Custom,
            FormKind::Closed,
        ] {
            assert_eq!(
                parse(&format!("scriptevent p6:{}", kind.name())),
                Ok(Command::Form(kind))
            );
        }
        assert!(parse("scriptevent p6:toast").is_err());
    }

    #[test]
    fn the_villager_offers_are_the_bds_farmer_trades() {
        let offers = farmer_offers().expect("every item exists");
        let trades: Vec<(&str, u8, &str, u8, u32)> = offers
            .iter()
            .map(|o| {
                (
                    o.buy().item().map_or("", |t| t.name()),
                    o.buy().count(),
                    o.sell().item().map_or("", |t| t.name()),
                    o.sell().count(),
                    o.trader_experience(),
                )
            })
            .collect();
        assert_eq!(
            trades,
            [
                ("minecraft:wheat", 20, "minecraft:emerald", 1, 2),
                ("minecraft:potato", 26, "minecraft:emerald", 1, 2),
                ("minecraft:carrot", 22, "minecraft:emerald", 1, 2),
                ("minecraft:beetroot", 15, "minecraft:emerald", 1, 2),
                ("minecraft:emerald", 1, "minecraft:bread", 6, 1),
            ]
        );
        for o in &offers {
            assert!(o.takes_any_damage() && o.rewards_experience());
            assert!(o.second_payment().is_empty());
            assert_eq!(
                (o.max_uses(), o.uses(), o.tier(), o.demand()),
                (16, 0, 0, 0)
            );
            assert_eq!(o.price_multipliers(), (0.05, 0.0));
        }
    }
}
