//! Anchor's console for the game scenarios (`p6 game anchor`): the commands the bot asks for
//! (`tp`, `give`, `setblock`, `fill`, `xp`, `summon villager`, `effect`, `kill`, `gamerule`) carried
//! out through Anchor's public task API ([`anchor::Context`], PLAN.md 10.6: no test-only entry
//! point).
//!
//! The harness parses each command line ([`parse`]) and hands it to a task that Anchor runs every
//! tick (`ServerBuilder::every(1, ..)`, [`console`]). The task runs the commands in order in the
//! tick's serial step; a command that cannot run yet (its player has not joined, its chunk is not
//! loaded) waits for a later tick, up to [`MAX_WAIT_TICKS`]. Each command gets one [`Report`]
//! back, which [`Console::settle`] prints after each scenario.
//!
//! Where Anchor has no counterpart, the command is accepted with a note: villager NPCs have no AI
//! (`effect` on them changes nothing), Anchor spawns no mobs and has no weather
//! (`domobspawning`, `doweathercycle`), and its clock has no stop (`dodaylightcycle`).

use std::collections::VecDeque;
use std::io::Write;
use std::num::NonZeroU32;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::Duration;

use anchor::block::State;
use anchor::custom::Offer;
use anchor::entity::Id;
use anchor::form::{Button, Custom, Form, Image, Menu, Modal};
use anchor::item::{Stack, Type};
use anchor::math::Vec3;
use anchor::nbt::Compound;
use anchor::world::{self, BlockPos};
use anchor::{Context, effect, inventory};

/// The task's period: every tick.
pub(in crate::p6) const EVERY_TICK: NonZeroU32 = NonZeroU32::MIN;
/// Commands waiting in the task at most; the harness's send blocks beyond it.
const MAX_PENDING: usize = 256;
/// How long a command may wait for its player or chunk: 10 seconds.
const MAX_WAIT_TICKS: u32 = 200;
/// The most blocks one `fill` may change (BDS's limit).
const MAX_FILL: u64 = 32_768;
/// The most items one `give` may hand out (BDS's limit).
const MAX_GIVE: u32 = 32_767;
/// How long [`Console::settle`] waits for the reports of the commands sent.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(30);
/// The actor variant of a farmer villager (`EntityVariant` of `minecraft:villager_v2`).
const FARMER: u8 = 1;
/// The selector of every entity that is not a player, the only entity selector the bot uses.
const NON_PLAYERS: &str = "@e[type=!player]";

/// A parsed console command.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Command {
    /// `tp <player> <x> <y> <z>`: the feet position.
    Teleport { player: String, feet: Vec3 },
    /// `give <player> <item> [count]`, split into stacks of the item's stack size.
    Give { player: String, stacks: Vec<Stack> },
    /// `xp <points> <player>` or `xp <levels>L <player>`, as points.
    Experience { player: String, points: u32 },
    /// `setblock` and `fill`: every block of the box, corners included.
    Fill {
        from: BlockPos,
        to: BlockPos,
        state: State,
    },
    /// `summon villager <x> <y> <z> ...`: a farmer villager NPC with [`farmer_offers`].
    Villager { pos: Vec3 },
    /// `effect <player> <effect> [seconds] [amplifier]`.
    Effect {
        player: String,
        effect: effect::Type,
        ticks: u32,
        amplifier: u8,
    },
    /// `kill @e[type=!player]`: the villagers this console summoned.
    KillNonPlayers,
    /// `gamerule randomtickspeed <n>`.
    RandomTickSpeed(u32),
    /// `op <player>`: [`Context::op`].
    Op(String),
    /// `kick <player> <reason>`: [`anchor::player::Player::kick`].
    Kick { player: String, reason: String },
    /// `scriptevent p6:<kind>`: the forms scenario's form of that kind to every player, as the
    /// BDS script pack shows it (`xtask/data/p6/forms-pack`, [`p6_form`]).
    Form(&'static str),
    /// `summon snowball|egg <x> <y> <z>`: the projectile, dropped without motion.
    Projectile {
        projectile: anchor::entity::Projectile,
        pos: Vec3,
    },
    /// A command Anchor has no counterpart for; the text says why it is fine.
    Accepted(&'static str),
}

/// What became of one command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Report {
    /// The command line.
    pub(super) line: String,
    /// Done with an optional note, or failed with the reason.
    pub(super) result: Result<Option<String>, String>,
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
pub(in crate::p6) struct Console {
    commands: SyncSender<(String, Command)>,
    reports: Receiver<Report>,
    /// Commands sent and not reported yet.
    outstanding: usize,
    /// Lines that did not parse, reported by the next [`Console::settle`].
    refused: Vec<Report>,
    /// What the players answered to the forms of `scriptevent` commands, as notes.
    answers: Receiver<String>,
}

/// The console and the task for `ServerBuilder::every(EVERY_TICK, ..)`.
pub(in crate::p6) fn console() -> (Console, impl FnMut(&mut Context<'_>) + Send + 'static) {
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
        refused: Vec::new(),
        answers,
    };
    (console, task)
}

impl Console {
    /// Hands `line` to Anchor's task, or keeps the reason it does not parse for [`Self::settle`].
    pub(in crate::p6) fn send(&mut self, line: &str) -> Result<(), String> {
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
    pub(in crate::p6) fn settle(&mut self, out: &mut impl Write) -> Result<bool, String> {
        let mut reports = std::mem::take(&mut self.refused);
        let deadline = super::super::now() + SETTLE_TIMEOUT;
        while self.outstanding > 0 {
            match self.reports.try_recv() {
                Ok(report) => {
                    reports.push(report);
                    self.outstanding -= 1;
                }
                Err(_) if super::super::now() < deadline => super::super::pause(),
                Err(_) => {
                    let missing = std::mem::take(&mut self.outstanding);
                    reports.push(Report {
                        line: format!("({missing} commands)"),
                        result: Err(format!("no report within {SETTLE_TIMEOUT:?}")),
                    });
                }
            }
        }
        while let Ok(answer) = self.answers.try_recv() {
            super::super::say(out, &format!("note anchor form: {answer}"))?;
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
            super::super::say(out, &text)?;
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
pub(super) fn parse(line: &str) -> Result<Command, String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        ["tp", player, x, y, z] => Ok(Command::Teleport {
            player: (*player).to_owned(),
            feet: Vec3::new(number(x)?, number(y)?, number(z)?),
        }),
        ["give", player, item, rest @ ..] => {
            let count = match rest {
                [] => 1,
                [count] => count
                    .parse::<u32>()
                    .ok()
                    .filter(|c| (1..=MAX_GIVE).contains(c))
                    .ok_or_else(|| format!("bad count {count:?}"))?,
                _ => return Err("give takes a player, an item and a count".to_owned()),
            };
            Ok(Command::Give {
                player: (*player).to_owned(),
                stacks: stacks(item_type(item)?, count),
            })
        }
        ["xp", amount, player] => Ok(Command::Experience {
            player: (*player).to_owned(),
            points: experience(amount)?,
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
                "villager NPCs have no AI (PLAN.md 8.6): an effect changes nothing",
            ))
        }
        ["effect", player, name, rest @ ..] => {
            let seconds = rest.first().map_or(Ok(30), |s| {
                s.parse::<u32>().map_err(|_| format!("bad seconds {s:?}"))
            })?;
            let amplifier = rest.get(1).map_or(Ok(0), |a| {
                a.parse::<u8>().map_err(|_| format!("bad amplifier {a:?}"))
            })?;
            Ok(Command::Effect {
                player: (*player).to_owned(),
                effect: effect_type(name)?,
                ticks: seconds.saturating_mul(20),
                amplifier,
            })
        }
        ["kill", target] if *target == NON_PLAYERS => Ok(Command::KillNonPlayers),
        ["gamerule", rule, value] => match (rule.to_ascii_lowercase().as_str(), *value) {
            ("randomtickspeed", speed) => speed
                .parse()
                .map(Command::RandomTickSpeed)
                .map_err(|_| format!("bad random tick speed {speed:?}")),
            ("domobspawning", "false") => Ok(Command::Accepted("Anchor spawns no mobs")),
            ("doweathercycle", "false") => Ok(Command::Accepted("Anchor has no weather")),
            ("dodaylightcycle", "false") => Ok(Command::Accepted(
                "Anchor's clock has no stop (no API until M8); it keeps running",
            )),
            _ => Err(format!("game rule {rule} {value} is not supported")),
        },
        _ => parse_api(&words),
    }
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
            .and_then(|kind| FORM_KINDS.iter().find(|k| **k == kind))
            .map(|kind| Command::Form(kind))
            .ok_or_else(|| format!("no P6 form {id}")),
        ["summon", entity @ ("snowball" | "egg"), x, y, z] => Ok(Command::Projectile {
            projectile: if *entity == "egg" {
                anchor::entity::Projectile::Egg
            } else {
                anchor::entity::Projectile::Snowball
            },
            pos: Vec3::new(number(x)?, number(y)?, number(z)?),
        }),
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
    super::super::step_state(&words.join(" "))
}

/// The item type of a `give` name: an item, else the item of the block's default state.
fn item_type(name: &str) -> Result<Type, String> {
    let full = format!("minecraft:{name}");
    Type::from_name(&full, 0, None, &Compound::new())
        .or_else(|| {
            let state = super::super::step_state(name).ok()?;
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

/// The points of an `xp` amount: points, or `<n>L` levels. `Context` adds points only (M8 adds
/// levels), so levels become the points that reach them from level 0, where every scenario
/// player starts (PMMP `ExperienceUtils::getXpToReachLevel`).
fn experience(amount: &str) -> Result<u32, String> {
    let bad = || format!("bad experience amount {amount:?}");
    let Some(levels) = amount.strip_suffix(['L', 'l']) else {
        return amount.parse().map_err(|_| bad());
    };
    let l: u64 = levels.parse().map_err(|_| bad())?;
    // The formula's halves are whole: 5l² − 81l and 9l² − 325l are even for every l.
    let points = match l {
        0..=16 => l * l + 6 * l,
        17..=31 => (5 * l * l + 720 - 81 * l) / 2,
        32..=24_791 => (9 * l * l + 4440 - 325 * l) / 2,
        _ => return Err(bad()),
    };
    u32::try_from(points).map_err(|_| bad())
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

/// The offers of the console's villager: a farmer's first-tier trades as BDS sends them (P6
/// facts 6: payments of any damage, price multipliers 0.05 and 0; the trade module numbers
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
    let not_joined = |player: &str| Outcome::Wait(format!("{player} has not joined"));
    match command {
        Command::Teleport { player, feet } => {
            let Some(p) = ctx.player_exact(player).and_then(|id| ctx.player_mut(id)) else {
                return not_joined(player);
            };
            let world = p.world();
            match p.teleport(world, *feet) {
                Ok(()) => Outcome::Done(None),
                Err(_) => Outcome::Failed("the position is outside the world".to_owned()),
            }
        }
        Command::Give { player, stacks } => {
            let Some(p) = ctx.player_exact(player) else {
                return not_joined(player);
            };
            let Some(p) = ctx.player_mut(p) else {
                return not_joined(player);
            };
            let mut main = p.inventory_mut(inventory::Kind::Main);
            let left: u32 = stacks.iter().map(|s| u32::from(main.add(s).count())).sum();
            Outcome::Done((left > 0).then(|| format!("{left} did not fit")))
        }
        Command::Experience { player, points } => {
            let Some(p) = ctx.player_exact(player).and_then(|id| ctx.player_mut(id)) else {
                return not_joined(player);
            };
            p.add_xp(*points);
            Outcome::Done(None)
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
        Command::Effect {
            player,
            effect,
            ticks,
            amplifier,
        } => {
            let Some(p) = ctx.player_exact(player).and_then(|id| ctx.player_mut(id)) else {
                return not_joined(player);
            };
            let added = p.add_effect(*effect, *ticks, *amplifier);
            Outcome::Done((!added).then(|| "not added".to_owned()))
        }
        Command::KillNonPlayers => {
            let removed = villagers
                .drain(..)
                .filter_map(|id| ctx.entity_mut(id).map(|mut e| e.flag_for_despawn()))
                .count();
            Outcome::Done(Some(format!(
                "removed {removed} villagers (Context lists no other entities until M8)"
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
        Command::Op(_) | Command::Kick { .. } | Command::Form(_) | Command::Projectile { .. } => {
            execute_api(ctx, command, answers)
        }
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
            let Some(p) = ctx.player_exact(player).and_then(|id| ctx.player_mut(id)) else {
                return Outcome::Wait(format!("{player} has not joined"));
            };
            done(p.kick(reason.as_str()).map_err(|e| e.to_string()))
        }
        Command::Form(kind) => {
            let kind: &'static str = kind;
            let players: Vec<_> = ctx.players().map(anchor::player::Player::id).collect();
            for player in players {
                let answers = answers.clone();
                let sent = ctx.send_form(player, p6_form(kind), move |_, _, response| {
                    match answers.try_send(format!("{kind} answered {response:?}")) {
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
        Command::Projectile { projectile, pos } => {
            match ctx.spawn_projectile(world::Id::DEFAULT, *pos, *projectile, Vec3::ZERO) {
                Some(_) => Outcome::Done(None),
                None => Outcome::Failed("no projectile was spawned".to_owned()),
            }
        }
        _ => Outcome::Failed("not a command of the API scenarios".to_owned()),
    }
}

/// The kinds of form the forms scenario asks for (`scriptevent p6:<kind>`).
const FORM_KINDS: [&str; 4] = ["modal", "menu", "custom", "closed"];

/// The forms scenario's form of `kind`, the same as the BDS script pack's
/// (`xtask/data/p6/forms-pack/scripts/main.js`); keep the two the same.
fn p6_form(kind: &str) -> Form {
    match kind {
        "menu" => Menu::new("P6 menu")
            .content("Menu body")
            .button("First")
            .header("Menu header")
            .label("Menu label")
            .divider()
            .button(Button::new("Second").image(Image::Path("textures/items/apple".to_owned())))
            .into(),
        "custom" => Custom::new("P6 custom")
            .header("A header")
            .label("A label")
            .divider()
            .input("Name", "placeholder", "default")
            .toggle("Toggle", false)
            .slider("Slider", 0.0, 10.0, 1.0, 3.0)
            .dropdown("Dropdown", ["a", "b", "c"], 0)
            .into(),
        "closed" => Modal::new("P6 closed", "Closed body", "Yes", "No").into(),
        _ => Modal::new("P6 modal", "Modal body", "Yes", "No").into(),
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

    /// Every command line of the BDS game session (`target/p6/bds-game/*.jsonl`) and its setup.
    const SCENARIO_COMMANDS: &[&str] = &[
        "tp P6Craft -11.5 -60 -11.5",
        "give P6Craft oak_log 4",
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
        "summon villager -9.5 -60 20.5 0 0 minecraft:spawn_farmer P6Farmer",
        "summon villager -9.5 -60 20.5",
        "summon minecraft:villager -13.5 -60 20.5",
        "effect @e[type=!player] slowness 600 255 true",
        "kill @e[type=!player]",
    ];

    #[test]
    fn every_command_the_scenarios_send_parses() {
        for line in SCENARIO_COMMANDS.iter().chain(super::super::SESSION_SETUP) {
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
        assert!(matches!(
            parse("effect P6Trade slowness 600 255 true"),
            Ok(Command::Effect {
                effect: effect::Type::Slowness,
                ticks: 12_000,
                amplifier: 255,
                ..
            })
        ));
        assert!(matches!(
            parse("summon villager -9.5 -60 20.5 0 0 minecraft:spawn_farmer P6Farmer"),
            Ok(Command::Villager { .. })
        ));
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
            "xp 30000L P6Enchant",
            "summon zombie 0 0 0",
            "kill @a",
            "gamerule pvp false",
            "gamerule randomtickspeed fast",
            "effect P6Craft no_such_effect",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
    }

    #[test]
    fn levels_become_the_points_that_reach_them_from_zero() {
        // PMMP ExperienceUtils::getXpToReachLevel at the formula's borders.
        for (amount, points) in [
            ("0L", 0),
            ("16L", 352),
            ("17L", 394),
            ("30L", 1395),
            ("31L", 1507),
            ("32L", 1628),
            ("7", 7),
        ] {
            assert_eq!(experience(amount), Ok(points), "{amount}");
        }
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
