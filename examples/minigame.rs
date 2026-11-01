//! A round-based minigame, built on Anchor's public API only.
//!
//! Players wait in the lobby, the main world. Once enough of them are there, a countdown shows a
//! title each second and a boss bar that drains; then everyone is teleported into `arena`, a
//! temporary world that is generated fresh at every start and never saved. A round lasts 10
//! seconds. At its end each player answers a form: play again (back to the lobby 3 seconds
//! later) or leave, and a summary of the round, made off the tick, is shown in chat.
//!
//! What it uses:
//! - [`ServerBuilder::world`] with [`Kind::Temporary`]: the arena.
//! - [`ServerBuilder::every`]: the game loop, once a second; its closure owns the round state.
//! - [`Context::schedule`]: the trip back to the lobby.
//! - Titles and the boss bar: [`Player::send`] with [`Message::Title`], [`Context::set_boss_bar`].
//! - A [`BlockBreak`] listener: blocks break only inside the arena area.
//! - [`Context::send_form`]: the modal form at the end of a round.
//! - [`Context::spawn_blocking`]: the round summary.
//! - [`ServerBuilder::on_receive`]: a packet hook that counts what each player sends, in a player
//!   extension.
//!
//! Run it in a folder of its own with `cargo run --example minigame`: it reads `anchor.toml`
//! there, writing the default file first, and keeps the lobby in `data/`.
//!
//! [`ServerBuilder::world`]: anchor::ServerBuilder::world
//! [`ServerBuilder::every`]: anchor::ServerBuilder::every
//! [`ServerBuilder::on_receive`]: anchor::ServerBuilder::on_receive

use std::fmt::{Display, Write};
use std::num::NonZeroU32;

use anchor::display::{BossBar, Colour};
use anchor::event::{BlockBreak, PlayerJoin, Priority};
use anchor::form::{Modal, Response};
use anchor::math::Vec3;
use anchor::player::{self, Message, Player};
use anchor::world::{BlockPos, Flat, Kind, World};
use anchor::{Config, Context, Server};
use tracing::debug;

/// The arena world's name.
const ARENA: &str = "arena";
/// Players the lobby needs before a countdown starts. One lets a single player try it.
const MIN_PLAYERS: usize = 1;
/// Seconds of the lobby countdown.
const COUNTDOWN_SECS: u8 = 5;
/// Seconds of a round.
const ROUND_SECS: u8 = 10;
/// The block players stand on when a round starts: on the grass of the flat arena.
const ARENA_CENTRE: BlockPos = BlockPos::new(16, -60, 16);
/// Blocks break only this far (in x and z) from the arena centre.
const ARENA_RADIUS: u32 = 12;
/// Ticks from the end-of-round answer to the trip back to the lobby.
const LOBBY_DELAY_TICKS: u32 = 3 * 20;
/// One second: 20 ticks.
const SECOND: NonZeroU32 = NonZeroU32::MIN.saturating_add(19);

/// What a player did in the current round. A player extension: not saved.
#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    packets: u64,
    blocks: u32,
}

/// Where the game is.
#[derive(Debug, Clone, Copy)]
enum Phase {
    /// Waiting for players in the lobby.
    Waiting,
    /// The lobby countdown, with the seconds left.
    Countdown(u8),
    /// A round, with the seconds left.
    Round(u8),
}

/// The round state, owned by the game loop.
#[derive(Debug)]
struct Game {
    phase: Phase,
    /// Rounds started so far.
    round: u32,
}

fn main() -> Result<(), anchor::Error> {
    let mut game = Game {
        phase: Phase::Waiting,
        round: 0,
    };
    Server::builder()
        .config(Config::default().load("anchor.toml")?)
        .world(ARENA, Kind::Temporary, Flat)
        .every(SECOND, move |cx| game.second(cx))
        .on(Priority::Normal, on_join)
        .on(Priority::Normal, protect_blocks)
        .on_receive(|cx, player, _packet| {
            if let Some(tally) = cx
                .player_mut(player)
                .and_then(|p| p.extension_mut::<Tally>())
            {
                tally.packets = tally.packets.saturating_add(1);
            }
            // Keep the packet: Anchor handles it as usual.
            true
        })
        .build()?
        .run()
}

impl Game {
    /// One step of the game loop.
    fn second(&mut self, cx: &mut Context<'_>) {
        let Some(arena) = cx.world_named(ARENA).map(World::id) else {
            return;
        };
        let lobby: Vec<player::Id> = cx.players_in(cx.default_world().id()).collect();
        let playing: Vec<player::Id> = cx.players_in(arena).collect();
        self.phase = match self.phase {
            Phase::Waiting | Phase::Countdown(_) if lobby.len() < MIN_PLAYERS => {
                for &id in &lobby {
                    cx.remove_boss_bar(id);
                }
                Phase::Waiting
            }
            Phase::Waiting => self.count_down(cx, &lobby, arena, COUNTDOWN_SECS),
            Phase::Countdown(left) => self.count_down(cx, &lobby, arena, left),
            Phase::Round(_) if playing.is_empty() => Phase::Waiting,
            Phase::Round(0) => {
                self.end_round(cx, &playing);
                Phase::Waiting
            }
            Phase::Round(left) => {
                let bar = BossBar::new(format!("Round {}: {left}s left", self.round))
                    .with_progress(f32::from(left) / f32::from(ROUND_SECS))
                    .with_colour(Colour::Red);
                for &id in &playing {
                    or_log(cx.set_boss_bar(id, bar.clone()));
                }
                Phase::Round(left - 1)
            }
        };
    }

    /// Shows the lobby the seconds `left`, or starts the round when none are.
    fn count_down(
        &mut self,
        cx: &mut Context<'_>,
        lobby: &[player::Id],
        arena: anchor::world::Id,
        left: u8,
    ) -> Phase {
        if left == 0 {
            self.round = self.round.saturating_add(1);
            for &id in lobby {
                cx.remove_boss_bar(id);
                if let Some(player) = cx.player_mut(id) {
                    or_log(player.send(title("Go!", "Break blocks in the arena")));
                    or_log(player.insert_extension(Tally::default()));
                }
                or_log(cx.teleport(id, arena, feet(ARENA_CENTRE)));
            }
            return Phase::Round(ROUND_SECS);
        }
        let bar = BossBar::new(format!("Starting in {left}s"))
            .with_progress(f32::from(left) / f32::from(COUNTDOWN_SECS))
            .with_colour(Colour::Green);
        for &id in lobby {
            or_log(cx.set_boss_bar(id, bar.clone()));
            if let Some(player) = cx.player_mut(id) {
                or_log(player.send(title(&left.to_string(), "Get ready")));
            }
        }
        Phase::Countdown(left - 1)
    }

    /// Ends the round: a form for every player, and a summary made off the tick.
    fn end_round(&self, cx: &mut Context<'_>, playing: &[player::Id]) {
        let mut tallies = Vec::with_capacity(playing.len());
        for &id in playing {
            cx.remove_boss_bar(id);
            if let Some(player) = cx.player(id) {
                let tally = player.extension::<Tally>().copied().unwrap_or_default();
                tallies.push((player.name().to_owned(), tally));
            }
            let form = Modal::new(
                "Round over",
                "GG! Play another round?",
                "Play again",
                "Leave",
            );
            or_log(cx.send_form(id, form, |cx, id, answer| after_round(cx, id, &answer)));
        }
        let round = self.round;
        // The work gets its inputs by value and cannot touch the game; the result comes back to
        // the tick, which shows it.
        or_log(cx.spawn_blocking(
            move || summary(round, tallies),
            |cx, result| match result {
                Ok(text) => or_log(cx.broadcast(Message::Raw(text))),
                Err(error) => debug!(%error, "no round summary"),
            },
        ));
    }
}

/// The round's summary, best first. It stands for work too slow for the tick, such as a
/// database write or a web request.
fn summary(round: u32, mut tallies: Vec<(String, Tally)>) -> String {
    tallies.sort_by(|a, b| b.1.blocks.cmp(&a.1.blocks).then_with(|| a.0.cmp(&b.0)));
    let mut text = format!("Round {round} summary:");
    for (name, tally) in &tallies {
        // Writing to a String cannot fail.
        let _written = write!(
            text,
            " {name} broke {} blocks and sent {} packets.",
            tally.blocks, tally.packets
        );
    }
    text
}

/// The answer to the end-of-round form.
fn after_round(cx: &mut Context<'_>, id: player::Id, answer: &Response) {
    let Some(player) = cx.player_mut(id) else {
        return;
    };
    if *answer == Response::Modal(false) {
        or_log(player.kick("Thanks for playing!"));
        return;
    }
    or_log(player.send(Message::Raw(format!(
        "Back to the lobby in {} seconds",
        LOBBY_DELAY_TICKS / SECOND.get()
    ))));
    or_log(cx.schedule(LOBBY_DELAY_TICKS, move |cx| {
        let lobby = cx.default_world();
        let (world, spawn) = (lobby.id(), lobby.spawn());
        or_log(cx.teleport(id, world, feet(spawn)));
    }));
}

fn on_join(event: &mut PlayerJoin, cx: &mut Context<'_>) {
    if let Some(player) = cx.player_mut(event.player()) {
        or_log(player.insert_extension(Tally::default()));
        or_log(player.send(Message::Raw(
            "Welcome! The next round starts when the lobby is full".to_owned(),
        )));
    }
}

/// Blocks break only inside the arena area; the lobby and the rest of the arena stay as they
/// are.
fn protect_blocks(event: &mut BlockBreak, cx: &mut Context<'_>) {
    let in_arena = cx
        .world_named(ARENA)
        .is_some_and(|w| w.id() == event.world());
    let pos = event.pos();
    let inside = pos.x.abs_diff(ARENA_CENTRE.x) <= ARENA_RADIUS
        && pos.z.abs_diff(ARENA_CENTRE.z) <= ARENA_RADIUS;
    if !in_arena || !inside {
        event.cancel();
        return;
    }
    if let Some(tally) = cx
        .player_mut(event.player())
        .and_then(Player::extension_mut::<Tally>)
    {
        tally.blocks = tally.blocks.saturating_add(1);
    }
}

/// A title with a subtitle, at the client's timing.
fn title(text: &str, subtitle: &str) -> Message {
    Message::Title {
        title: text.to_owned(),
        subtitle: subtitle.to_owned(),
        times: None,
    }
}

/// The feet of a player standing on the block `pos`, in its middle.
fn feet(pos: BlockPos) -> Vec3 {
    Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y),
        f64::from(pos.z) + 0.5,
    )
}

/// Logs a call that could not be done for one player (it left, or has not spawned yet); the
/// game goes on for the others.
fn or_log<T, E: Display>(result: Result<T, E>) {
    if let Err(error) = result {
        debug!(%error, "skipped for one player");
    }
}
