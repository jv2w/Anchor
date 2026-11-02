//! Commands on the server: `/list`, `/save-all`, `/save-off`, `/save-on`, `/status`, `/stop`,
//! `/version` and `/backup`. `/status` shows Anchor's own lines.

use super::{Msg, announce, infallible};
use crate::command::{Args, Command, Output, Result, Sender};
use crate::permission;
use crate::player::Player;
use crate::protocol::packet::PROTOCOL_VERSION;
use crate::runtime::ThreadTotals;
use crate::tick::Context;

/// The failure line of `/backup` with `backups = false`.
const BACKUP_OFF: Msg = Msg::own("Backups are turned off on this server");
/// The success line of `/backup`.
const BACKUP_START: Msg =
    Msg::own("Saving and backing up the data files; the server log tells when the backup is made");
/// The bytes in a MiB.
const MIB: u64 = 1 << 20;
/// The first line of `/list`.
const PLAYERS_LIST: Msg = Msg::client("commands.players.list");
/// The success line of `/save-off`.
const SAVE_DISABLED: Msg = Msg::client("commands.save.disabled");
/// The success line of `/save-on`.
const SAVE_ENABLED: Msg = Msg::client("commands.save.enabled");
/// The success line of `/save-all`.
const SAVE_START: Msg = Msg::client("commands.save.start");
/// The success line of `/stop`.
const STOP_START: Msg = Msg::client("commands.stop.start");
/// The `/version` line: the software, its version, the Minecraft version and protocol, and the
/// operating system.
const VERSION_LINE: Msg = Msg::own("{%0} {%1} for Minecraft {%2} (protocol {%3}) on {%4}");

/// Adds the commands of this module.
pub(super) fn add(commands: &mut Vec<Command>) {
    commands.extend([
        Command::new("list", "commands.list.description")
            .permission(permission::COMMAND_LIST)
            .overload(&[], infallible(list)),
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

/// Shows how many players are online of how many may be, and their names in text order
/// (`/list`).
fn list(cx: &mut Context<'_>, _: &Sender, _: &Args) -> Output {
    let mut names: Vec<&str> = cx.players().map(Player::name).collect();
    names.sort_unstable();
    let count = names.len().to_string();
    let max = cx.max_players().to_string();
    PLAYERS_LIST.output(&[&count, &max]).line(names.join(", "))
}

/// Asks for a save of everything at the end of the tick, as one commit ([`Context::save`])
/// (`/save-all`). The end of the commit is not reported: it comes later, off the tick.
fn save_all(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    let out = announce(cx, *sender, SAVE_START, &[]);
    cx.save();
    out
}

/// Asks for a save of everything, then a backup of the saved data files while the server runs
/// ([`Context::backup`]) (`/backup`). The backup ends later, off the tick, so the log tells its
/// result, as for `/save-all`'s commit.
fn backup(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Result<Output> {
    if !cx.backup() {
        return Err(BACKUP_OFF.error(&[]));
    }
    Ok(announce(cx, *sender, BACKUP_START, &[]))
}

/// Turns the periodic save off until the server stops or `/save-on` (`/save-off`;
/// [`Context::set_auto_save`] says what still saves).
fn save_off(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    cx.set_auto_save(false);
    announce(cx, *sender, SAVE_DISABLED, &[])
}

/// Turns the periodic save back on (`/save-on`).
fn save_on(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    cx.set_auto_save(true);
    announce(cx, *sender, SAVE_ENABLED, &[])
}

/// Shows the uptime by the wall clock and in ticks, the TPS and tick times now and averaged over
/// the last 20 ticks, the resident memory, the threads, the players, and each loaded world's
/// chunks, entities and block entities (`/status`). The uptime, memory and tick times are
/// measured off the tick and read from the input log, so a replay sees the same values; the
/// uptime and memory are at most a second old.
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

/// Stops the server after this tick (`/stop`).
fn stop(cx: &mut Context<'_>, sender: &Sender, _: &Args) -> Output {
    let out = announce(cx, *sender, STOP_START, &[]);
    cx.shutdown();
    out
}

/// Shows one line with [`VERSION_LINE`] (`/version`, aliases `/ver` and `/about`).
fn version(_: &mut Context<'_>, _: &Sender, _: &Args) -> Output {
    VERSION_LINE.output(&[
        Context::NAME,
        Context::ANCHOR_VERSION,
        Context::VERSION,
        &PROTOCOL_VERSION.to_string(),
        std::env::consts::OS,
    ])
}
