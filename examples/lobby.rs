//! A hub server, built on Anchor's public API only.
//!
//! - Every player gets a compass, a welcome title and a boss bar when it joins. Right-clicking
//!   with the compass, or `/games`, opens the games menu, a form with two buttons:
//!   - "Arena" teleports the player into `arena`, a second world;
//!   - "Shop" opens a chest menu of items: clicking one gives the player a copy, and the menu
//!     closes, keeping its own item.
//! - `/spawn` brings the player back to the lobby spawn.
//! - A sidebar shows how many players are online.
//! - Nobody gets hungry or hurt in the lobby (the default world); the arena plays as usual.
//!
//! What it uses: [`ServerBuilder::world`], [`ServerBuilder::every`], [`ServerBuilder::command`],
//! listeners ([`ServerBuilder::on`]) for [`PlayerItemUse`], [`PlayerInteract`],
//! [`PlayerExhaust`] and [`EntityDamage`], [`Context::send_form`] with a [`form::Buttons`],
//! [`Context::open_menu`] with a [`menu::Menu`], [`Player::inventory_mut`],
//! [`Context::teleport`], [`Player::send`], [`Context::set_scoreboard`] and
//! [`Context::set_boss_bar`].
//!
//! New players are noticed by the once-a-second loop, which marks them with a player extension:
//! the loop also refreshes the sidebars.
//!
//! Run it in a folder of its own with `cargo run --example lobby`: it reads `anchor.toml` there,
//! writing the default file first, and keeps the lobby in `data/`.
//!
//! [`ServerBuilder::world`]: anchor::ServerBuilder::world
//! [`ServerBuilder::every`]: anchor::ServerBuilder::every
//! [`ServerBuilder::command`]: anchor::ServerBuilder::command
//! [`ServerBuilder::on`]: anchor::ServerBuilder::on

use std::error::Error;
use std::num::NonZeroU32;

use anchor::command::{self, Command, Output, Sender};
use anchor::display::{BossBar, Color, Scoreboard, Slot};
use anchor::event::{
    Actor, EntityDamage, InteractAction, PlayerExhaust, PlayerInteract, PlayerItemUse, Priority,
};
use anchor::form::{self, Button, Image, Response};
use anchor::inventory::Kind;
use anchor::item::{self, Stack};
use anchor::math::Vec3;
use anchor::menu::{self, Input};
use anchor::player::{self, Message, Player};
use anchor::world::{self, BlockPos, Flat};
use anchor::{Config, Context, Server};
use tracing::debug;

/// The arena world's name.
const ARENA: &str = "arena";
/// Where players land in the arena: on the grass of the flat world.
const ARENA_START: BlockPos = BlockPos::new(8, -60, 8);
/// One second, in ticks.
const SECOND: NonZeroU32 = NonZeroU32::MIN.saturating_add(Context::TICKS_PER_SECOND - 1);
/// What the shop sells: menu slot, item and count.
const SHOP: [(usize, item::Kind, u8); 3] = [
    (11, item::Kind::DiamondSword, 1),
    (13, item::Kind::Bow, 1),
    (15, item::Kind::GoldenApple, 4),
];

/// Marks a player the lobby has welcomed. A player extension: not saved.
#[derive(Debug)]
struct Welcomed;

fn main() -> anchor::Result<()> {
    Server::builder()
        .config(Config::default().load("anchor.toml")?)
        .world(ARENA, world::Kind::Persistent, Flat)
        .every(SECOND, every_second)
        .command(
            Command::new("games", "Opens the games menu").overload(&[], |cx, sender, _| {
                open_games(cx, player_of(sender)?);
                Ok(Output::new())
            }),
        )
        .command(
            Command::new("spawn", "Back to the lobby spawn").overload(&[], |cx, sender, _| {
                let id = player_of(sender)?;
                let lobby = cx.default_world();
                let (world, spawn) = (lobby.id(), lobby.spawn());
                cx.teleport(id, world, feet(spawn))
                    .map_err(|e| command::Error::failed(e.to_string()))?;
                Ok(Output::from("Back at the lobby spawn"))
            }),
        )
        .on(Priority::Normal, |e: &mut PlayerItemUse, cx| {
            if is_compass(e.item()) {
                open_games(cx, e.player());
            }
        })
        .on(Priority::Normal, |e: &mut PlayerInteract, cx| {
            if e.action() == InteractAction::RightClickBlock && is_compass(e.item()) {
                e.cancel();
                open_games(cx, e.player());
            }
        })
        .on(Priority::Normal, |e: &mut PlayerExhaust, cx| {
            if in_lobby(cx, e.player()) {
                e.cancel();
            }
        })
        .on(Priority::Normal, |e: &mut EntityDamage, cx| {
            if let Actor::Player(id) = e.entity()
                && in_lobby(cx, id)
            {
                e.cancel();
            }
        })
        .build()?
        .run()
}

/// Once a second: welcomes the players that joined since, and refreshes every sidebar (only the
/// lines that changed are sent).
fn every_second(cx: &mut Context<'_>) {
    let players: Vec<player::Id> = cx.players().map(Player::id).collect();
    let board = Scoreboard::new(Slot::Sidebar, "§l§bLobby")
        .line(format!("Online: {}", players.len()), 0)
        .line("§7/games to play", 1);
    for &id in &players {
        if cx
            .player(id)
            .is_some_and(|p| p.extension::<Welcomed>().is_none())
        {
            welcome(cx, id);
        }
        or_log(cx.set_scoreboard(id, board.clone()));
    }
}

/// Greets a new player: the compass in the first hotbar slot, a title and a boss bar.
fn welcome(cx: &mut Context<'_>, id: player::Id) {
    let Some(player) = cx.player_mut(id) else {
        return;
    };
    or_log(player.insert_extension(Welcomed));
    // The first slot, not the first free one: the compass is not doubled when a player who kept
    // it joins again.
    let compass = Stack::new(item::Kind::Compass.default_type(), 1);
    or_log(player.inventory_mut(Kind::Main).set(0, compass));
    or_log(player.send(Message::Title {
        title: "§bWelcome!".to_owned(),
        subtitle: "Use the compass or /games".to_owned(),
        times: None,
    }));
    let bar =
        BossBar::new(format!("Welcome to the lobby, {}", player.name())).with_color(Color::Blue);
    or_log(cx.set_boss_bar(id, bar));
}

/// Sends `id` the games menu.
fn open_games(cx: &mut Context<'_>, id: player::Id) {
    let games = form::Buttons::new("Games")
        .with_content("Pick one")
        .button(Button::new("Arena").with_image(Image::Path("textures/items/iron_sword".into())))
        .button(Button::new("Shop").with_image(Image::Path("textures/items/emerald".into())));
    or_log(cx.send_form(id, games, |cx, id, answer| match answer {
        Response::Buttons(0) => to_arena(cx, id),
        Response::Buttons(1) => open_shop(cx, id),
        _ => {}
    }));
}

/// Teleports `id` to the arena.
fn to_arena(cx: &mut Context<'_>, id: player::Id) {
    let Some(arena) = cx.world_by_name(ARENA) else {
        return;
    };
    or_log(cx.teleport(id, arena, feet(ARENA_START)));
    if let Some(player) = cx.player_mut(id) {
        or_log(player.send(Message::Tip("§aArena! /spawn takes you back".to_owned())));
    }
}

/// Opens the shop: a click on an item gives the player a copy and closes the menu.
fn open_shop(cx: &mut Context<'_>, id: player::Id) {
    let mut shop = menu::Menu::new(menu::Kind::Chest).with_title("Shop");
    for (slot, kind, count) in SHOP {
        shop = shop.item(slot, Stack::new(kind.default_type(), count));
    }
    or_log(cx.open_menu(id, shop, |cx, id, input| {
        // The shop has no editable slots: every input but the close is a click.
        let Input::Click(click) = input else {
            return menu::Response::Keep;
        };
        if click.item().is_empty() {
            return menu::Response::Keep;
        }
        if let Some(player) = cx.player_mut(id) {
            // What does not fit is not given.
            let _left = player.inventory_mut(Kind::Main).add(click.item());
        }
        menu::Response::Close
    }));
}

/// The player who sent a command; the console has no menu or spawn.
fn player_of(sender: &Sender) -> command::Result<player::Id> {
    match *sender {
        Sender::Player(id) => Ok(id),
        Sender::Console => Err(command::Error::failed("Only players can do this")),
    }
}

/// Whether `id` is in the lobby, the default world.
fn in_lobby(cx: &Context<'_>, id: player::Id) -> bool {
    cx.player(id)
        .is_some_and(|p| p.world() == cx.default_world().id())
}

fn is_compass(stack: &Stack) -> bool {
    stack
        .item_type()
        .is_some_and(|t| t.kind() == Some(item::Kind::Compass))
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
/// lobby goes on for the others.
fn or_log<T, E: Error + 'static>(result: Result<T, E>) {
    if let Err(error) = result {
        debug!(
            error = &error as &(dyn Error + 'static),
            "skipped for one player"
        );
    }
}
