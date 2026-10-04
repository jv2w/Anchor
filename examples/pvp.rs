//! A small player-versus-player server, built on Anchor's public API only.
//!
//! - Every life starts with the kit: the inventory is reset on join and on respawn.
//! - `/kit` refills the arrows and the food, at most once every 30 seconds per player. It needs
//!   the custom permission `pvp.kit` (everyone has it); holders of `pvp.kit.nocooldown`
//!   (operators) skip the wait. The cooldown lives in a player extension, which is not saved.
//! - Damage events: no fall damage; a kill counts for the last player who hit the victim within
//!   10 seconds, with a kill streak message from 3 kills in a row; deaths drop nothing.
//! - Kills and deaths are persistent player data ([`ServerBuilder::player_data`]), saved with
//!   the player in the same commit as the world.
//! - A sidebar shows the player's kills and deaths and how many players are online.
//! - Players respawn at the world spawn.
//!
//! Run it in a folder of its own with `cargo run --example pvp`: it reads `anchor.toml` there,
//! writing the default file first, and keeps the world in `data/`.
//!
//! [`ServerBuilder::player_data`]: anchor::ServerBuilder::player_data

use std::fmt::Display;

use anchor::command::{self, Command, Output, Sender};
use anchor::display::{Scoreboard, Slot};
use anchor::event::{
    Actor, DamageCause, DamageSource, EntityDamage, PlayerDeath, PlayerJoin, PlayerQuit,
    PlayerRespawn, Priority,
};
use anchor::inventory::Kind;
use anchor::item::{Stack, Type};
use anchor::math::Vec3;
use anchor::permission::{Default, Permission};
use anchor::player::{self, Message, Player};
use anchor::{Config, Context, Server, nbt};
use serde::{Deserialize, Serialize};
use tracing::debug;

/// Who may use `/kit`: everyone.
const KIT: Permission = Permission::new("pvp.kit", Default::True);
/// Who uses `/kit` without waiting: operators.
const NO_COOLDOWN: Permission = Permission::new("pvp.kit.nocooldown", Default::Op);
/// Ticks between two `/kit` refills (20 ticks a second).
const KIT_COOLDOWN_TICKS: u64 = 30 * 20;
/// How long a hit still earns the kill, in ticks.
const KILL_CREDIT_TICKS: u64 = 10 * 20;
/// The kill streak that gets announced.
const STREAK: u32 = 3;

/// What `/kit` refills: item name and count.
const REFILL: [(&str, u8); 3] = [
    ("minecraft:arrow", 16),
    ("minecraft:golden_apple", 2),
    ("minecraft:cooked_beef", 8),
];
/// What every life starts with besides the refill.
const WEAPONS: [&str; 2] = ["minecraft:iron_sword", "minecraft:bow"];
/// The armour, helmet to boots (the order of the armour slots).
const ARMOR: [&str; 4] = [
    "minecraft:iron_helmet",
    "minecraft:iron_chestplate",
    "minecraft:iron_leggings",
    "minecraft:iron_boots",
];

/// A player's record, saved with the player.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Stats {
    kills: u32,
    deaths: u32,
}

/// What the server keeps on a player while it is online: not saved.
#[derive(Debug, Default)]
struct Life {
    /// Kills since the last death.
    streak: u32,
    /// The last player who hit this one, and the tick of the hit.
    last_hit: Option<(player::Id, u64)>,
    /// The tick from which `/kit` works again.
    kit_ready: u64,
}

fn main() -> Result<(), anchor::Error> {
    Server::builder()
        .config(Config::default().load("anchor.toml")?)
        .player_data::<Stats>("pvp:stats")
        .permission(NO_COOLDOWN)
        .command(kit_command())
        .on(Priority::Normal, on_join)
        .on(Priority::Normal, on_quit)
        .on(Priority::Normal, no_fall_damage)
        // Last, so that it records only hits no other listener cancelled.
        .on(Priority::Highest, remember_attacker)
        .on(Priority::Normal, on_death)
        .on(Priority::Normal, on_respawn)
        .build()?
        .run()
}

/// `/kit`: refills the arrows and the food, once per cooldown.
fn kit_command() -> Command {
    Command::new("kit", "Refills your arrows and food")
        .permission(KIT)
        .overload(&[], |cx, sender, _args| {
            let &Sender::Player(id) = sender else {
                return Err(command::Error::failed("Only players have a kit"));
            };
            let now = cx.tick();
            let waits = !cx.has_permission(*sender, NO_COOLDOWN);
            let player = cx
                .player_mut(id)
                .ok_or_else(|| command::Error::failed("You are not in the game"))?;
            let life = player
                .extension_mut::<Life>()
                .ok_or_else(|| command::Error::failed("You have not joined yet"))?;
            if waits && now < life.kit_ready {
                let seconds = (life.kit_ready - now).div_ceil(20);
                return Err(command::Error::failed(format!(
                    "You can use /kit again in {seconds}s"
                )));
            }
            life.kit_ready = now.saturating_add(KIT_COOLDOWN_TICKS);
            refill(player);
            Ok(Output::from("Kit refilled"))
        })
}

/// A stack of the item `name`, empty if Anchor has no such item.
fn item(name: &str, count: u8) -> Stack {
    Type::from_name(name, 0, None, &nbt::Compound::new())
        .map_or(Stack::EMPTY, |item| Stack::new(item, count))
}

/// Puts the refill items into the main inventory; what does not fit is not given.
fn refill(player: &mut Player) {
    let mut main = player.inventory_mut(Kind::Main);
    for (name, count) in REFILL {
        let _left = main.add(&item(name, count));
    }
}

/// Empties the inventory and gives the whole kit.
fn reset_kit(player: &mut Player) {
    let mut main = player.inventory_mut(Kind::Main);
    main.clear_all();
    for name in WEAPONS {
        let _left = main.add(&item(name, 1));
    }
    let mut armor = player.inventory_mut(Kind::Armor);
    for (slot, name) in ARMOR.into_iter().enumerate() {
        // The four armour slots exist and take one item each.
        let _set = armor.set(slot, item(name, 1));
    }
    refill(player);
}

fn on_join(event: &mut PlayerJoin, cx: &mut Context<'_>) {
    if let Some(player) = cx.player_mut(event.player()) {
        event.set_join_message(format!("§a+ {}", player.name()));
        or_log(player.insert_extension(Life::default()));
        reset_kit(player);
        or_log(player.send(Message::Raw(
            "Welcome! /kit refills your arrows and food every 30 seconds".to_owned(),
        )));
    }
    refresh_boards(cx, None);
}

fn on_quit(event: &mut PlayerQuit, cx: &mut Context<'_>) {
    refresh_boards(cx, Some(event.player()));
}

fn no_fall_damage(event: &mut EntityDamage, _cx: &mut Context<'_>) {
    if matches!(event.entity(), Actor::Player(_)) && event.cause() == DamageCause::Fall {
        event.cancel();
    }
}

/// Remembers who hit a player last, for the kill credit.
fn remember_attacker(event: &mut EntityDamage, cx: &mut Context<'_>) {
    let Actor::Player(victim) = event.entity() else {
        return;
    };
    let (DamageSource::Entity(Actor::Player(attacker))
    | DamageSource::Child {
        owner: Some(Actor::Player(attacker)),
        ..
    }) = event.source()
    else {
        return;
    };
    if event.is_cancelled() || attacker == victim {
        return;
    }
    let now = cx.tick();
    if let Some(life) = cx
        .player_mut(victim)
        .and_then(|p| p.extension_mut::<Life>())
    {
        life.last_hit = Some((attacker, now));
    }
}

fn on_death(event: &mut PlayerDeath, cx: &mut Context<'_>) {
    // Nothing drops: every life starts with the kit again.
    event.set_keep_inventory(false);
    event.set_drops(Vec::new());
    event.set_xp_drop_amount(0);
    let victim_id = event.player();
    let now = cx.tick();
    let Some(victim) = cx.player_mut(victim_id) else {
        return;
    };
    let victim_name = victim.name().to_owned();
    if let Some(stats) = victim.data_mut::<Stats>() {
        stats.deaths = stats.deaths.saturating_add(1);
    }
    let last_hit = victim.extension_mut::<Life>().and_then(|life| {
        life.streak = 0;
        life.last_hit.take()
    });
    let killer_id = last_hit
        .filter(|&(_, at)| now.saturating_sub(at) <= KILL_CREDIT_TICKS)
        .map(|(killer, _)| killer);
    if let Some(killer) = killer_id.and_then(|id| cx.player_mut(id)) {
        if let Some(stats) = killer.data_mut::<Stats>() {
            stats.kills = stats.kills.saturating_add(1);
        }
        let streak = killer.extension_mut::<Life>().map_or(0, |life| {
            life.streak = life.streak.saturating_add(1);
            life.streak
        });
        let streak = if streak >= STREAK {
            format!(" (§6{streak} kill streak§r)")
        } else {
            String::new()
        };
        event.set_death_message(format!(
            "{victim_name} was slain by {}{streak}",
            killer.name()
        ));
    }
    refresh_boards(cx, None);
}

fn on_respawn(event: &mut PlayerRespawn, cx: &mut Context<'_>) {
    let world = cx.default_world();
    let spawn = world.spawn();
    let feet = Vec3::new(
        f64::from(spawn.x) + 0.5,
        f64::from(spawn.y),
        f64::from(spawn.z) + 0.5,
    );
    event.set_position(world.id(), feet);
    if let Some(player) = cx.player_mut(event.player()) {
        reset_kit(player);
    }
}

/// Shows every online player its sidebar; `leaving` is a player about to leave, left out.
fn refresh_boards(cx: &mut Context<'_>, leaving: Option<player::Id>) {
    let players: Vec<player::Id> = cx
        .players()
        .map(Player::id)
        .filter(|&id| Some(id) != leaving)
        .collect();
    for &id in &players {
        let Some(stats) = cx.player(id).and_then(|p| p.data::<Stats>()) else {
            continue;
        };
        let board = Scoreboard::new(Slot::Sidebar, "§l§cPvP")
            .line(format!("Kills: {}", stats.kills), 0)
            .line(format!("Deaths: {}", stats.deaths), 1)
            .line(format!("Online: {}", players.len()), 2);
        or_log(cx.set_scoreboard(id, board));
    }
}

/// Logs a call that could not be done for one player (it left, or has not spawned yet); the
/// server goes on for the others.
fn or_log<T, E: Display>(result: Result<T, E>) {
    if let Err(error) = result {
        debug!(%error, "skipped for one player");
    }
}
