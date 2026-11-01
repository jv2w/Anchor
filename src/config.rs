//! Server settings: the `anchor.toml` file, its defaults and its validation.

mod update;

pub use update::Update;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU16;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use toml::{Table, Value};

use crate::fs::{self, Access};
use crate::protocol::batch;
use crate::protocol::compression::Compression;
use crate::protocol::consts::GameType;

/// The file [`Config::load`] writes when the settings file is missing; a test checks that it
/// loads as the default settings. [`Config::update_file`] adds a missing key with the comment
/// lines right above it here, and a missing table with those above its header.
const DEFAULT_FILE: &str = r#"# Anchor server settings. Anchor reads this file at start and writes it only to update it:
# "<server program> update-config" adds the keys of a newer Anchor and removes unknown keys.
# A wrong value or an unknown key stops the start with a message naming the key.
# A key left out keeps its default, so new Anchor versions start with an old file.

# Settings for anyone opening a server.
[general]
# Name shown in the server list.
motd = "Anchor Server"
# Addresses to listen on. "0.0.0.0" and "::" mean every interface.
server-ip = "0.0.0.0"
server-ipv6 = "::"
enable-ipv6 = true
# The one port players reach: UDP carries RakNet, NetherNet game data and server list pings;
# TCP carries NetherNet signaling.
server-port = 19132
# Address NetherNet players use to reach this server, if it differs from the local one (NAT).
# Empty: advertise the address each player reached the signaling port on.
# An IPv6 address needs enable-ipv6.
public-ip = ""
max-players = 20
# true: players must sign in to Xbox Live. false: anyone can join under any name.
xbox-auth = true
# true: only listed players can join. Manage the list with the whitelist command.
white-list = false
# Default for new players: "survival", "creative", "adventure" or "spectator".
gamemode = "survival"
# true: players get the default game mode on every join.
force-gamemode = false
# "peaceful", "easy", "normal" or "hard".
difficulty = "normal"
hardcore = false
pvp = true
# Each switch below turns a game system off completely when false: its rules stop running.
hunger = true
# Gaining experience points and levels. false also means no experience orbs.
experience = true
# Experience orbs and throwing bottles o' enchanting. false deletes the orbs of loading chunks.
experience-orbs = true
fall-damage = true
# Air supply under water, drowning and suffocation damage.
drowning = true
# Armour protection and armour wear.
armor = true
# Tools, weapons and armour wear out with use.
durability = true
# Sleeping in beds and bed spawn points.
beds = true
# The time of day moving on.
daylight-cycle = true
# The recipe book: false hides it and refuses its quick crafting; the crafting grid still works.
recipe-book = true
# The vanilla recipes; false keeps only recipes the server program registers.
vanilla-recipes = true
# Ticks a dropped item stays on the ground (1 to 72000).
item-despawn-ticks = 6000
# Critical hits: "off", "sprint" (only when not sprinting) or "always" (sprinting too).
critical-hits = "sprint"
# The server checks that a block took long enough to break.
break-time-check = true
# The server checks that a move is not too far (more than 15 blocks in one input).
movement-check = true
# Every player sees its coordinates on screen.
show-coordinates = false
# The other players on the locator bar. false sends no locator bar data.
locator-bar = true
# Dead players come back at once, without the death screen.
immediate-respawn = false
# Every player sees in chat how a player died.
show-death-messages = false
# Main world. level-type fills the chunks never saved: "flat" or "void".
level-name = "world"
level-type = "flat"
# Whole number as text. Empty: made from the world name, the same at every start. Other text stops the start.
level-seed = ""
# Largest view distance a player may use, in chunks (2 to 64).
view-distance = 16
# false: player inventories and positions are not kept between joins.
save-player-data = true
# Copies of the data files in data/backups, at start and with the backup command.
backups = true
# Message players see when the server stops.
shutdown-message = "The server has stopped"
# Resource pack files (.mcpack or .zip), relative to data/packs. Earlier entries override later ones.
# An encrypted pack needs its key in "<file>.key" next to it.
# Players keep a downloaded pack by its uuid and version: raise the manifest version when
# a pack changes.
resource-packs = []
# true: players must accept the resource packs to join.
force-resources = false
# Console log detail: "off", "error", "warn", "info", "debug" or "trace".
log-level = "info"

# Settings that tune performance and protocol behavior. Change them with care.
[developer]
# Network compression: "snappy" (less CPU) or "zlib" (less bandwidth).
compression = "snappy"
# Batches smaller than this many bytes are sent uncompressed (1 to 65535).
compression-threshold = 256
# zlib only: 1 (fastest) to 9 (smallest).
compression-level = 6
# Largest decompressed batch received from a player or sent to one, in bytes (4210786 to
# 67108864: at least what one player with the largest skin takes in the player list).
max-batch-bytes = 16777216
# false: RakNet game traffic is sent in plain form. Anyone between a player and the server
# can then read it and change it, login included. Use false only on a trusted network.
# NetherNet traffic is always encrypted by its own transport.
enable-encryption = true
# false: accept only NetherNet. A RakNet client gets no reply to its connection request;
# the UDP port still answers server list pings.
raknet = true
# Largest RakNet packet in bytes (576 to 1492). Lower it behind tunnels or VPNs.
max-mtu-size = 1492
# PEM certificate chain and private key for HTTPS NetherNet signaling.
# Empty: plain HTTP, and players confirm the server key on first join.
nethernet-tls-cert = ""
nethernet-tls-key = ""
# Threads for network work and for parallel game work: "auto" or a number.
network-threads = "auto"
cpu-threads = "auto"
# Chunks sent to each player per tick.
chunks-per-tick = 4
# Chunk radius sent before a player spawns (at most view-distance).
spawn-radius = 4
# Chunk radius around players where blocks get random ticks and entities are ticked (at most
# view-distance). Block updates (liquids, fire, falling blocks) run in every loaded chunk.
tick-radius = 4
# Random block ticks per sub-chunk per tick (crop growth, leaf decay). 0 turns them off.
random-tick-speed = 3
# Blocks that never get random ticks, by name, for example "minecraft:grass_block". Every block
# of the same kind as a named block stops too. Custom blocks ignore this list.
disable-block-ticking = []
# true: a world without players runs one tick in every 100.
slow-empty-worlds = false
# "dynamic": light is calculated and follows block changes.
# "on-load": light is calculated when a chunk loads but not after block changes.
# "fixed": no light is calculated; every block has fixed-light-level as sky light.
lighting = "dynamic"
# Sky light everywhere when lighting is "fixed" (0 to 15). Block light is then 0.
fixed-light-level = 15
# Ticks between checks for items a player can pick up (1 to 20).
item-pickup-interval = 1
# false: movement packets the server sends skip the send hooks.
hook-movement-packets = true
# false: PlayerAuthInput packets skip the receive hooks.
hook-player-input = true
# Minutes between whole-state saves (1 to 1440).
save-interval-minutes = 10
# When the data files are checked at start: "always", "after-unclean-shutdown" or "never".
# The check command checks them while the server runs.
integrity-check = "always"
# false: Anchor does not read commands from standard input.
console-input = true

# Per-world values, by world name. They override view-distance, chunks-per-tick,
# spawn-radius and tick-radius for that world, and spawn = [x, y, z] sets where players
# appear in a new world (default: the generator's spawn), for example:
# [worlds.lobby]
# view-distance = 8
# chunks-per-tick = 8
# spawn = [0, -60, 0]
"#;

/// The largest settings file read. A real file is about 4 KiB; the cap bounds the parser's memory.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// The longest `motd`, in bytes ([`is_motd`]). The RakNet pong carries `motd` and `level-name` in
/// one datagram; 256 + 64 bytes and the other fields fit the 576-byte minimum MTU.
pub const MAX_MOTD_BYTES: usize = 256;
/// The `max-players` range; the player and session tables are sized from it.
const PLAYERS_RANGE: RangeInclusive<u32> = 1..=10_000;
/// The `item-despawn-ticks` range: one tick to an hour.
pub(crate) const ITEM_DESPAWN_TICKS_RANGE: RangeInclusive<u32> = 1..=72_000;
/// See [`MAX_MOTD_BYTES`].
const MAX_LEVEL_NAME_BYTES: usize = 64;
/// The `view-distance` range the settings file accepts.
pub(crate) const VIEW_DISTANCE_RANGE: RangeInclusive<u8> = 2..=64;
/// The longest `shutdown-message`, in bytes: a disconnect message several lines long fits.
const MAX_SHUTDOWN_MESSAGE_BYTES: usize = 1024;
/// The `compression` values.
const COMPRESSION_NAMES: [(&str, Compression); 2] =
    [("snappy", Compression::Snappy), ("zlib", Compression::Zlib)];
/// The `max-mtu-size` range; go-raknet's dialer probes 1492, 1200 and 576, and 1492 is the
/// largest RakNet MTU.
const MTU_SIZE_RANGE: RangeInclusive<u16> = 576..=crate::raknet::MAX_MTU;
/// The `network-threads` and `cpu-threads` counts; the runtime checks the total against the
/// logical CPUs.
const THREADS_RANGE: RangeInclusive<u16> = 1..=u16::MAX;
/// The `chunks-per-tick` range: 0 would never send a chunk, and 256 sends the largest view in
/// about 3 seconds.
pub(crate) const CHUNKS_PER_TICK_RANGE: RangeInclusive<u32> = 1..=256;
/// The `spawn-radius` and `tick-radius` range; the effective radius is at most `view-distance`,
/// so a larger value means nothing.
const RADIUS_RANGE: RangeInclusive<u8> = 1..=*VIEW_DISTANCE_RANGE.end();
/// The `random-tick-speed` range; a sub-chunk holds 4096 blocks, so more ticks than blocks add
/// only cost.
pub(crate) const RANDOM_TICK_SPEED_RANGE: RangeInclusive<u32> = 0..=4096;
/// The `fixed-light-level` range: the light levels.
const FIXED_LIGHT_LEVEL_RANGE: RangeInclusive<u8> = 0..=15;
/// The `item-pickup-interval` range: every tick to once a second.
const ITEM_PICKUP_INTERVAL_RANGE: RangeInclusive<u8> = 1..=20;
/// The `save-interval-minutes` range: one minute to one day.
const SAVE_INTERVAL_MINUTES_RANGE: RangeInclusive<u16> = 1..=1440;

/// Every server setting. [`Config::default`] holds the values of the default `anchor.toml`.
///
/// The data files save none of them: every start takes the settings from the config it is built
/// with, so a value changed while the server runs (the difficulty, a world's difficulty, a game
/// rule) is back to its setting after a restart. Time stop and weather have no setting: every
/// world starts with time running and clear weather. A world's spawn is its state, not a
/// setting: the world's file keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// Settings for anyone opening a server (`[general]`).
    pub general: General,
    /// Settings that tune performance and protocol behavior (`[developer]`).
    pub developer: Developer,
    /// Per-world values (`[worlds.<name>]`), by world name. A name must be a world the server
    /// declares (`level-name` or [`ServerBuilder::world`](crate::ServerBuilder::world)), or the
    /// build fails.
    pub worlds: BTreeMap<String, WorldSettings>,
    /// Top-level tables of the loaded file that Anchor does not know; [`Config::validate`]
    /// rejects them.
    unknown_tables: BTreeSet<String>,
    /// What [`Config::load`] changed in the file when the program was started as
    /// `<program> update-config`; [`Server::run`](crate::Server::run) reports it.
    updated: Option<Update>,
}

impl Config {
    /// Reads the settings file at `path` over these values and checks the result.
    ///
    /// A key in the file replaces the value here, and a key left out keeps the value here. A wrong
    /// type, an out-of-range value or an unknown key is an error naming the key; nothing is fixed
    /// silently. Unknown tables are kept and rejected when the server is built.
    ///
    /// When the file does not exist, the default file is written next to it (a temporary file
    /// renamed into place) and then read like any other file, so the first start and every later
    /// start with that file give the same settings.
    ///
    /// This does blocking file I/O: call it before the server starts.
    ///
    /// Relative `resource-packs` paths, in the file or set in code, resolve against `packs` in the
    /// data folder (`data/packs`).
    ///
    /// When the program was started as `<program> update-config`, the file is first updated as
    /// [`Config::update_file`] does, and [`Server::run`](crate::Server::run) then reports what
    /// changed instead of running the server.
    pub fn load(self, path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match crate::args::current() {
            Ok(crate::args::Task::UpdateConfig) => self.load_updated(path),
            _ => self.load_file(path),
        }
    }

    /// Updates the settings file at `path` to this Anchor's keys and returns what changed.
    ///
    /// Each key of the default file that the file lacks is added with its default value and the
    /// comment lines right above it in the default file: after the line of the key before it
    /// there, or after the table's header when the file has none of those. A table the file
    /// lacks is added at its end. A `key = value` that Anchor does not know, at the top, in a
    /// table of the default file or in a `[worlds.<name>]` table written with its header, is
    /// removed with the comment lines right above it. Every other byte stays as it was: values,
    /// comments, key order and line endings. Unknown tables are not touched, and no key is added
    /// to a `[worlds.<name>]` table, since a key left out there means the global value.
    ///
    /// The added values are the defaults, so the file means what it meant without them. A value
    /// set in code before [`Config::load`] for a key this adds is then replaced by the file's, as
    /// with a missing file.
    ///
    /// The update fixes no value. The file is left as it was, and the error returned, when it is
    /// missing ([`Error::Read`]), when it would not load after the update, when a table lacking a
    /// key is written without its `[table]` header ([`Error::HeaderlessTable`]), or when the
    /// update would make it larger than 1 MiB. Otherwise, if anything changed, the file is
    /// replaced whole (a temporary file renamed into place), so running it again changes nothing.
    ///
    /// A server program started as `<program> update-config` runs this from [`Config::load`],
    /// whether the server runs or not: a running server reads the file only at its next start.
    /// This does blocking file I/O.
    pub fn update_file(path: impl AsRef<Path>) -> Result<Update> {
        update::run(path.as_ref())
    }

    /// The batch compression settings, checked by [`batch::Settings::new`].
    pub(crate) fn batch_settings(&self) -> Result<batch::Settings> {
        let d = &self.developer;
        batch::Settings::new(
            d.compression,
            d.compression_threshold,
            d.compression_level,
            d.max_batch_bytes,
        )
        .map_err(|source| Error::Batch {
            key: batch_key(&source),
            source,
        })
    }

    /// The first `[worlds]` entry whose name `declared` does not hold, as an error; the server
    /// builder calls it with the names of its worlds. A name must match a declared name exactly,
    /// while world files ignore letter case.
    pub(crate) fn check_world_names(&self, declared: impl Fn(&str) -> bool) -> Result<()> {
        match self.worlds.keys().find(|name| !declared(name)) {
            Some(name) => Err(Error::UnknownWorld(name.clone())),
            None => Ok(()),
        }
    }

    /// The `spawn-radius` limited to `view-distance`: the chunk radius sent before a player
    /// spawns, less when the player asks for a smaller view.
    pub fn effective_spawn_radius(&self) -> u8 {
        self.view_limits(None).spawn_radius
    }

    /// The `tick-radius` limited to `view-distance`: the chunk radius around players that gets
    /// random block ticks and entity ticks.
    pub fn effective_tick_radius(&self) -> u8 {
        self.view_limits(None).tick_radius
    }

    /// The view values in effect in a world with the `[worlds.<name>]` values `world` (`None`:
    /// a world without them): each value it sets replaces the global one, and the radii are at
    /// most the view distance.
    pub(crate) fn view_limits(&self, world: Option<&WorldSettings>) -> ViewLimits {
        self.global_view()
            .apply(&world.copied().unwrap_or_default())
    }

    /// The global view values as the keys hold them, the radii not yet bounded by the view
    /// distance: what a world's own values replace ([`ViewLimits::apply`]).
    pub(crate) fn global_view(&self) -> ViewLimits {
        let (g, d) = (&self.general, &self.developer);
        ViewLimits {
            view_distance: g.view_distance,
            chunks_per_tick: d.chunks_per_tick,
            spawn_radius: d.spawn_radius,
            tick_radius: d.tick_radius,
        }
    }

    /// The `resource-packs` files, relative paths resolved against the pack folder `folder`.
    pub(crate) fn resource_pack_paths(&self, folder: &Path) -> Vec<PathBuf> {
        let packs = &self.general.resource_packs;
        packs.iter().map(|p| folder.join(p)).collect()
    }

    /// What [`Config::load`] changed in the file under `<program> update-config`; `None` when
    /// it did not run so.
    pub(crate) fn updated(&self) -> Option<&Update> {
        self.updated.as_ref()
    }

    /// The first error among the unknown tables and the values that break their rules; the
    /// server builder calls it.
    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(name) = self.unknown_tables.first() {
            return Err(Error::UnknownTable(name.clone()));
        }
        self.check()
    }

    /// [`Config::load`] after updating the file first, as `<program> update-config` asks.
    fn load_updated(mut self, path: &Path) -> Result<Self> {
        // Before the read: an old file may not load until it is updated.
        self.updated = Some(update::run(path)?);
        self.load_file(path)
    }

    /// [`Config::load`] without the update: the file's keys over these values, or, when the file
    /// is missing, the default file written and read.
    fn load_file(self, path: &Path) -> Result<Self> {
        if let Some(bytes) = read(path)? {
            return self.apply(&bytes);
        }
        let written = fs::write_atomic(path, DEFAULT_FILE.as_bytes(), Access::Shared);
        written.map_err(|source| Error::Write {
            path: path.to_owned(),
            source,
        })?;
        self.apply(DEFAULT_FILE.as_bytes())
    }

    /// Parses a settings file's bytes, lays its keys over these values and checks the result.
    fn apply(mut self, bytes: &[u8]) -> Result<Self> {
        let text = utf8(bytes)?;
        let table = text.parse::<Table>().map_err(|e| syntax(text, &e))?;
        let sections = update::sections();
        for (name, value) in &table {
            // A key the default file lacks is unknown here as it is to `update-config`, which
            // removes it.
            if let Some(section) = sections.iter().find(|s| s.name == name)
                && let Some(key) = table_of(value, Key::top(name))?
                    .keys()
                    .find(|key| !section.has_key(key))
            {
                return Err(Key::new(name, key).unknown());
            }
            match name.as_str() {
                "general" => self.general.merge(table_of(value, Key::top(name))?)?,
                "developer" => self.developer.merge(table_of(value, Key::top(name))?)?,
                "worlds" => {
                    for (world, values) in table_of(value, Key::top(name))? {
                        let table = format!("worlds.{world}");
                        let values = table_of(values, Key::top(&table))?;
                        self.worlds
                            .entry(world.clone())
                            .or_default()
                            .merge(&table, values)?;
                    }
                }
                _ if value.is_table() => {
                    self.unknown_tables.insert(name.clone());
                }
                _ => return Err(Key::top(name).unknown()),
            }
        }
        self.check()?;
        Ok(self)
    }

    /// The first value that breaks its rule, as an error; the one check for loaded and code-set
    /// values.
    fn check(&self) -> Result<()> {
        self.check_general()?;
        self.check_developer()?;
        for (name, world) in &self.worlds {
            world.check(&format!("worlds.{name}"))?;
        }
        Ok(())
    }

    /// The first `[developer]` value that breaks its rule, as an error.
    fn check_developer(&self) -> Result<()> {
        let d = &self.developer;
        self.batch_settings()?;
        if d.max_batch_bytes < crate::tick::MAX_LIST_ENTRY_BYTES {
            return Err(Key::developer("max-batch-bytes").invalid(format!(
                "at least {}, so that a player with the largest skin can be listed",
                crate::tick::MAX_LIST_ENTRY_BYTES
            )));
        }
        range(
            Key::developer("max-mtu-size"),
            d.max_mtu_size,
            MTU_SIZE_RANGE,
        )?;
        let cert = Key::developer("nethernet-tls-cert");
        let key = Key::developer("nethernet-tls-key");
        for (setting, path) in [(cert, &d.nethernet_tls_cert), (key, &d.nethernet_tls_key)] {
            if path.as_ref().is_some_and(|p| p.as_os_str().is_empty()) {
                return Err(setting.invalid("empty or a path"));
            }
        }
        match (&d.nethernet_tls_cert, &d.nethernet_tls_key) {
            (Some(_), None) => {
                return Err(key.invalid("set when `developer.nethernet-tls-cert` is set"));
            }
            (None, Some(_)) => {
                return Err(cert.invalid("set when `developer.nethernet-tls-key` is set"));
            }
            _ => {}
        }
        range(
            Key::developer("chunks-per-tick"),
            d.chunks_per_tick,
            CHUNKS_PER_TICK_RANGE,
        )?;
        range(Key::developer("spawn-radius"), d.spawn_radius, RADIUS_RANGE)?;
        range(Key::developer("tick-radius"), d.tick_radius, RADIUS_RANGE)?;
        range(
            Key::developer("random-tick-speed"),
            d.random_tick_speed,
            RANDOM_TICK_SPEED_RANGE,
        )?;
        if !d.disable_block_ticking.iter().all(|n| is_block_name(n)) {
            return Err(Key::developer("disable-block-ticking")
                .invalid("a list of block names like \"minecraft:grass_block\""));
        }
        range(
            Key::developer("fixed-light-level"),
            d.fixed_light_level,
            FIXED_LIGHT_LEVEL_RANGE,
        )?;
        range(
            Key::developer("item-pickup-interval"),
            d.item_pickup_interval,
            ITEM_PICKUP_INTERVAL_RANGE,
        )?;
        range(
            Key::developer("save-interval-minutes"),
            d.save_interval_minutes,
            SAVE_INTERVAL_MINUTES_RANGE,
        )
    }

    /// The first `[general]` value that breaks its rule, as an error.
    fn check_general(&self) -> Result<()> {
        let g = &self.general;
        list_text(Key::general("motd"), &g.motd, MAX_MOTD_BYTES)?;
        // A NetherNet offer is answered with an address of the family of this one, and the
        // server opens no IPv6 socket without `enable-ipv6`.
        if !g.enable_ipv6 && g.public_ip.is_some_and(|ip| ip.to_canonical().is_ipv6()) {
            return Err(Key::general("public-ip")
                .invalid("an IPv4 address unless `general.enable-ipv6` is true"));
        }
        range(Key::general("max-players"), g.max_players, PLAYERS_RANGE)?;
        if g.hardcore && g.difficulty != Difficulty::Hard {
            return Err(
                Key::general("hardcore").invalid("false unless `general.difficulty` is \"hard\"")
            );
        }
        range(
            Key::general("item-despawn-ticks"),
            g.item_despawn_ticks,
            ITEM_DESPAWN_TICKS_RANGE,
        )?;
        let level_name = Key::general("level-name");
        list_text(level_name, &g.level_name, MAX_LEVEL_NAME_BYTES)?;
        if !crate::storage::is_world_file_name(&g.level_name) {
            return Err(level_name.invalid("a name that can be the file name of a world"));
        }
        range(
            Key::general("view-distance"),
            g.view_distance,
            VIEW_DISTANCE_RANGE,
        )?;
        length(
            Key::general("shutdown-message"),
            &g.shutdown_message,
            MAX_SHUTDOWN_MESSAGE_BYTES,
        )?;
        // A path into the old `packs` folder would now name `data/packs/packs/...`; Anchor never
        // writes the file, so the setting must be changed.
        if g.resource_packs
            .iter()
            .any(|p| p.as_os_str().is_empty() || p.starts_with(crate::pack::FOLDER))
        {
            return Err(Key::general("resource-packs")
                .invalid("a list of non-empty paths relative to data/packs, not under packs/"));
        }
        Ok(())
    }
}

/// The `[general]` table: settings for anyone opening a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct General {
    /// `motd`: the name shown in the server list.
    pub motd: String,
    /// `server-ip`: the IPv4 address to listen on.
    #[doc(alias = "Server::getIp")]
    pub server_ip: Ipv4Addr,
    /// `server-ipv6`: the IPv6 address to listen on.
    #[doc(alias = "Server::getIpV6")]
    pub server_ipv6: Ipv6Addr,
    /// `enable-ipv6`: whether to listen on `server_ipv6` too.
    pub enable_ipv6: bool,
    /// `server-port`: UDP for RakNet, NetherNet game data and server list pings, TCP for
    /// NetherNet signaling, on IPv4 and IPv6 alike.
    #[doc(alias = "Server::getPort")]
    #[doc(alias = "Server::getPortV6")]
    pub server_port: u16,
    /// `public-ip`: the address NetherNet players use to reach this server; `None` advertises the
    /// address each player reached the signaling port on. An IPv6 address needs `enable_ipv6`.
    pub public_ip: Option<IpAddr>,
    /// `max-players`: the most players online at once.
    pub max_players: u32,
    /// `xbox-auth`: whether players must sign in to Xbox Live.
    #[doc(alias = "Server::getOnlineMode")]
    #[doc(alias = "Server::requiresAuthentication")]
    pub xbox_auth: bool,
    /// `white-list`: whether only listed players can join.
    pub white_list: bool,
    /// `gamemode`: the default game mode of new players.
    pub gamemode: GameMode,
    /// `force-gamemode`: whether players get the default game mode on every join.
    pub force_gamemode: bool,
    /// `difficulty`: the world difficulty.
    pub difficulty: Difficulty,
    /// `hardcore`: whether the world is hardcore; requires [`Difficulty::Hard`].
    pub hardcore: bool,
    /// `pvp`: whether players can hurt each other.
    pub pvp: bool,
    /// `hunger`: whether players get hungry.
    pub hunger: bool,
    /// `experience`: whether players gain experience points and levels.
    pub experience: bool,
    /// `experience-orbs`: whether experience orbs exist and bottles o' enchanting can be thrown;
    /// `false` deletes the orbs of chunks that load.
    pub experience_orbs: bool,
    /// `fall-damage`: whether falling hurts.
    pub fall_damage: bool,
    /// `drowning`: whether air runs out under water and drowning and suffocation hurt.
    pub drowning: bool,
    /// `armor`: whether armour protects and wears.
    pub armor: bool,
    /// `durability`: whether tools, weapons and armour wear out with use.
    pub durability: bool,
    /// `beds`: whether players can sleep in beds and set their spawn point with them.
    pub beds: bool,
    /// `daylight-cycle`: whether the time of day moves on.
    pub daylight_cycle: bool,
    /// `recipe-book`: whether the recipe book is shown and its quick crafting accepted.
    pub recipe_book: bool,
    /// `vanilla-recipes`: whether the vanilla recipes exist besides those the server registers.
    pub vanilla_recipes: bool,
    /// `item-despawn-ticks`: ticks a dropped item stays on the ground.
    pub item_despawn_ticks: u32,
    /// `critical-hits`: when a hit can be critical.
    pub critical_hits: CriticalHits,
    /// `break-time-check`: whether the server checks that a block took long enough to break.
    pub break_time_check: bool,
    /// `movement-check`: whether the server refuses a move of more than 15 blocks in one input.
    pub movement_check: bool,
    /// `show-coordinates`: whether every player sees its coordinates on screen (the game rule
    /// `showCoordinates`).
    pub show_coordinates: bool,
    /// `locator-bar`: whether players see the others on the locator bar; `false` sends no
    /// locator bar packets and sets the game rule `playerWaypoints` to 0.
    pub locator_bar: bool,
    /// `immediate-respawn`: whether dead players come back at once, without the death screen
    /// (the game rule `doImmediateRespawn`, by which the client asks to respawn straight away).
    pub immediate_respawn: bool,
    /// `show-death-messages`: whether every player sees in chat how a player died (the game rule
    /// `showDeathMessages`; [`PlayerDeath`](crate::event::PlayerDeath) listeners may change it).
    pub show_death_messages: bool,
    /// `level-name`: the main world's name.
    pub level_name: String,
    /// `level-type`: the generator of the main world's chunks never saved.
    pub level_type: LevelType,
    /// `level-seed`: the main world's seed; `None` makes one from its name, the same at every
    /// start.
    pub level_seed: Option<i64>,
    /// `view-distance`: the largest view distance a player may use, in chunks.
    pub view_distance: u8,
    /// `save-player-data`: whether inventories and positions are kept between joins.
    pub save_player_data: bool,
    /// `backups`: whether the data files are copied to `data/backups` at start and by the backup
    /// command.
    pub backups: bool,
    /// `shutdown-message`: the message players see when the server stops.
    pub shutdown_message: String,
    /// `resource-packs`: resource pack files, relative to `packs` in the data folder; a relative
    /// path may not start with `packs`. Players keep a downloaded pack by its uuid and version, so
    /// the manifest version must rise when a pack changes.
    #[doc(alias = "ResourcePackManager::__construct")]
    pub resource_packs: Vec<PathBuf>,
    /// `force-resources`: whether players must accept the resource packs to join.
    #[doc(alias = "ResourcePackManager::resourcePacksRequired")]
    #[doc(alias = "ResourcePackManager::setResourcePacksRequired")]
    pub force_resources: bool,
    /// `log-level`: the console log detail.
    pub log_level: LogLevel,
}

impl General {
    /// Replaces the values of the keys present in `table`.
    fn merge(&mut self, table: &Table) -> Result<()> {
        for (name, value) in table {
            let key = Key::new("general", name);
            match name.as_str() {
                "motd" => text(value, key)?.clone_into(&mut self.motd),
                "server-ip" => self.server_ip = parsed(value, key, "an IPv4 address")?,
                "server-ipv6" => self.server_ipv6 = parsed(value, key, "an IPv6 address")?,
                "enable-ipv6" => self.enable_ipv6 = boolean(value, key)?,
                "server-port" => self.server_port = int(value, key, u16::MIN..=u16::MAX)?,
                "public-ip" => {
                    self.public_ip = optional(value, key, "empty or an IP address")?;
                }
                "max-players" => self.max_players = int(value, key, PLAYERS_RANGE)?,
                "xbox-auth" => self.xbox_auth = boolean(value, key)?,
                "white-list" => self.white_list = boolean(value, key)?,
                "gamemode" => self.gamemode = choice(value, key, &GameMode::NAMES)?,
                "force-gamemode" => self.force_gamemode = boolean(value, key)?,
                "difficulty" => self.difficulty = choice(value, key, &Difficulty::NAMES)?,
                "hardcore" => self.hardcore = boolean(value, key)?,
                "pvp" => self.pvp = boolean(value, key)?,
                "hunger" => self.hunger = boolean(value, key)?,
                "experience" => self.experience = boolean(value, key)?,
                "experience-orbs" => self.experience_orbs = boolean(value, key)?,
                "fall-damage" => self.fall_damage = boolean(value, key)?,
                "drowning" => self.drowning = boolean(value, key)?,
                "armor" => self.armor = boolean(value, key)?,
                "durability" => self.durability = boolean(value, key)?,
                "beds" => self.beds = boolean(value, key)?,
                "daylight-cycle" => self.daylight_cycle = boolean(value, key)?,
                "recipe-book" => self.recipe_book = boolean(value, key)?,
                "vanilla-recipes" => self.vanilla_recipes = boolean(value, key)?,
                "item-despawn-ticks" => {
                    self.item_despawn_ticks = int(value, key, ITEM_DESPAWN_TICKS_RANGE)?;
                }
                "critical-hits" => self.critical_hits = choice(value, key, &CriticalHits::NAMES)?,
                "break-time-check" => self.break_time_check = boolean(value, key)?,
                "movement-check" => self.movement_check = boolean(value, key)?,
                "show-coordinates" => self.show_coordinates = boolean(value, key)?,
                "locator-bar" => self.locator_bar = boolean(value, key)?,
                "immediate-respawn" => self.immediate_respawn = boolean(value, key)?,
                "show-death-messages" => self.show_death_messages = boolean(value, key)?,
                "level-name" => text(value, key)?.clone_into(&mut self.level_name),
                "level-type" => self.level_type = choice(value, key, &LevelType::NAMES)?,
                "level-seed" => {
                    self.level_seed = optional(value, key, "empty or a whole number as text")?;
                }
                "view-distance" => self.view_distance = int(value, key, VIEW_DISTANCE_RANGE)?,
                "save-player-data" => self.save_player_data = boolean(value, key)?,
                "backups" => self.backups = boolean(value, key)?,
                "shutdown-message" => text(value, key)?.clone_into(&mut self.shutdown_message),
                "resource-packs" => {
                    self.resource_packs = strings(value, key)?.map(PathBuf::from).collect();
                }
                "force-resources" => self.force_resources = boolean(value, key)?,
                "log-level" => self.log_level = choice(value, key, &LogLevel::NAMES)?,
                _ => return Err(key.unknown()),
            }
        }
        Ok(())
    }
}

impl Default for General {
    fn default() -> Self {
        Self {
            motd: "Anchor Server".to_owned(),
            server_ip: Ipv4Addr::UNSPECIFIED,
            server_ipv6: Ipv6Addr::UNSPECIFIED,
            enable_ipv6: true,
            server_port: 19_132,
            public_ip: None,
            max_players: 20,
            xbox_auth: true,
            white_list: false,
            gamemode: GameMode::Survival,
            force_gamemode: false,
            difficulty: Difficulty::Normal,
            hardcore: false,
            pvp: true,
            hunger: true,
            experience: true,
            experience_orbs: true,
            fall_damage: true,
            drowning: true,
            armor: true,
            durability: true,
            beds: true,
            daylight_cycle: true,
            recipe_book: true,
            vanilla_recipes: true,
            item_despawn_ticks: 6000,
            critical_hits: CriticalHits::Sprint,
            break_time_check: true,
            movement_check: true,
            show_coordinates: false,
            locator_bar: true,
            immediate_respawn: false,
            show_death_messages: false,
            level_name: "world".to_owned(),
            level_type: LevelType::Flat,
            level_seed: None,
            view_distance: 16,
            save_player_data: true,
            backups: true,
            shutdown_message: "The server has stopped".to_owned(),
            resource_packs: Vec::new(),
            force_resources: false,
            log_level: LogLevel::Info,
        }
    }
}

/// The `[developer]` table: settings that tune performance and protocol behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Developer {
    /// `compression`: the network compression algorithm.
    pub compression: Compression,
    /// `compression-threshold`: batches shorter than this many bytes are sent uncompressed.
    pub compression_threshold: u16,
    /// `compression-level`: the zlib level, 1 (fastest) to 9 (smallest).
    pub compression_level: u8,
    /// `max-batch-bytes`: the largest decompressed batch received from a client or sent to one.
    pub max_batch_bytes: usize,
    /// `enable-encryption`: whether RakNet game traffic is encrypted. NetherNet traffic always is,
    /// by its own transport.
    pub enable_encryption: bool,
    /// `raknet`: whether RakNet connections are accepted; pings are answered either way, and a
    /// RakNet client that tries to connect gets no reply when it is `false`.
    pub raknet: bool,
    /// `max-mtu-size`: the largest RakNet packet in bytes.
    pub max_mtu_size: u16,
    /// `nethernet-tls-cert`: the PEM certificate chain for HTTPS signaling; `None` is plain HTTP.
    pub nethernet_tls_cert: Option<PathBuf>,
    /// `nethernet-tls-key`: the PEM private key for HTTPS signaling.
    pub nethernet_tls_key: Option<PathBuf>,
    /// `network-threads`: threads for network work.
    pub network_threads: Threads,
    /// `cpu-threads`: threads for parallel game work.
    pub cpu_threads: Threads,
    /// `chunks-per-tick`: chunks sent to each player per tick.
    pub chunks_per_tick: u32,
    /// `spawn-radius`: the chunk radius sent before a player spawns.
    pub spawn_radius: u8,
    /// `tick-radius`: the chunk radius around players where blocks get random ticks and entities
    /// are ticked; block updates (liquids, fire, falling blocks) run in every loaded chunk.
    pub tick_radius: u8,
    /// `random-tick-speed`: random block ticks per sub-chunk per tick.
    pub random_tick_speed: u32,
    /// `disable-block-ticking`: blocks that never get random ticks, by name; every block of the
    /// same kind as a named block stops too, and custom blocks ignore the list.
    pub disable_block_ticking: Vec<String>,
    /// `slow-empty-worlds`: whether a world without players runs one tick in every 100.
    pub slow_empty_worlds: bool,
    /// `lighting`: how light is calculated.
    pub lighting: Lighting,
    /// `fixed-light-level`: the sky light everywhere when `lighting` is [`Lighting::Fixed`].
    pub fixed_light_level: u8,
    /// `item-pickup-interval`: ticks between checks for items a player can pick up.
    pub item_pickup_interval: u8,
    /// `hook-movement-packets`: whether movement packets the server sends pass the send hooks.
    pub hook_movement_packets: bool,
    /// `hook-player-input`: whether `PlayerAuthInput` packets pass the receive hooks.
    pub hook_player_input: bool,
    /// `save-interval-minutes`: minutes between whole-state saves.
    pub save_interval_minutes: u16,
    /// `integrity-check`: when the data files are checked at start.
    pub integrity_check: IntegrityCheck,
    /// `console-input`: whether commands are read from standard input.
    pub console_input: bool,
}

impl Developer {
    /// Replaces the values of the keys present in `table`.
    fn merge(&mut self, table: &Table) -> Result<()> {
        for (name, value) in table {
            let key = Key::new("developer", name);
            match name.as_str() {
                "compression" => self.compression = choice(value, key, &COMPRESSION_NAMES)?,
                // The other limits of these three live in `batch::Settings::new`, which `check`
                // calls; here a value only has to fit the field.
                "compression-threshold" => {
                    self.compression_threshold = int(value, key, u16::MIN..=u16::MAX)?;
                }
                "compression-level" => {
                    self.compression_level = int(value, key, u8::MIN..=u8::MAX)?;
                }
                "max-batch-bytes" => {
                    self.max_batch_bytes = int(value, key, usize::MIN..=usize::MAX)?;
                }
                "enable-encryption" => self.enable_encryption = boolean(value, key)?,
                "raknet" => self.raknet = boolean(value, key)?,
                "max-mtu-size" => self.max_mtu_size = int(value, key, MTU_SIZE_RANGE)?,
                "nethernet-tls-cert" => {
                    self.nethernet_tls_cert = optional(value, key, "empty or a path")?;
                }
                "nethernet-tls-key" => {
                    self.nethernet_tls_key = optional(value, key, "empty or a path")?;
                }
                "network-threads" => self.network_threads = threads(value, key)?,
                "cpu-threads" => self.cpu_threads = threads(value, key)?,
                "chunks-per-tick" => {
                    self.chunks_per_tick = int(value, key, CHUNKS_PER_TICK_RANGE)?;
                }
                "spawn-radius" => self.spawn_radius = int(value, key, RADIUS_RANGE)?,
                "tick-radius" => self.tick_radius = int(value, key, RADIUS_RANGE)?,
                "random-tick-speed" => {
                    self.random_tick_speed = int(value, key, RANDOM_TICK_SPEED_RANGE)?;
                }
                "disable-block-ticking" => {
                    self.disable_block_ticking = strings(value, key)?.map(str::to_owned).collect();
                }
                "slow-empty-worlds" => self.slow_empty_worlds = boolean(value, key)?,
                "lighting" => self.lighting = lighting(value, key)?,
                "fixed-light-level" => {
                    self.fixed_light_level = int(value, key, FIXED_LIGHT_LEVEL_RANGE)?;
                }
                "item-pickup-interval" => {
                    self.item_pickup_interval = int(value, key, ITEM_PICKUP_INTERVAL_RANGE)?;
                }
                "hook-movement-packets" => self.hook_movement_packets = boolean(value, key)?,
                "hook-player-input" => self.hook_player_input = boolean(value, key)?,
                "save-interval-minutes" => {
                    self.save_interval_minutes = int(value, key, SAVE_INTERVAL_MINUTES_RANGE)?;
                }
                "integrity-check" => {
                    self.integrity_check = choice(value, key, &IntegrityCheck::NAMES)?;
                }
                "console-input" => self.console_input = boolean(value, key)?,
                _ => return Err(key.unknown()),
            }
        }
        Ok(())
    }
}

impl Default for Developer {
    fn default() -> Self {
        Self {
            compression: Compression::Snappy,
            compression_threshold: 256,
            compression_level: 6,
            max_batch_bytes: 16 * 1024 * 1024,
            enable_encryption: true,
            raknet: true,
            max_mtu_size: 1492,
            nethernet_tls_cert: None,
            nethernet_tls_key: None,
            network_threads: Threads::Auto,
            cpu_threads: Threads::Auto,
            chunks_per_tick: 4,
            spawn_radius: 4,
            tick_radius: 4,
            random_tick_speed: 3,
            disable_block_ticking: Vec::new(),
            slow_empty_worlds: false,
            lighting: Lighting::Dynamic,
            fixed_light_level: 15,
            item_pickup_interval: 1,
            hook_movement_packets: true,
            hook_player_input: true,
            save_interval_minutes: 10,
            integrity_check: IntegrityCheck::Always,
            console_input: true,
        }
    }
}

/// One world's `[worlds.<name>]` table: the view values it sets replace the global ones in that
/// world, `None` keeps the global value; `spawn` sets the world's spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorldSettings {
    /// `view-distance`: the largest view distance a player in the world may use, in chunks.
    pub view_distance: Option<u8>,
    /// `chunks-per-tick`: chunks of the world sent to each player per tick.
    pub chunks_per_tick: Option<u32>,
    /// `spawn-radius`: the chunk radius sent before a player spawns in the world.
    pub spawn_radius: Option<u8>,
    /// `tick-radius`: the chunk radius around players where the world's blocks get random ticks
    /// and its entities are ticked.
    pub tick_radius: Option<u8>,
    /// `spawn = [x, y, z]`: where players appear in the world when it is new; `None` is the
    /// generator's spawn. The world's file then keeps its spawn, moved or not, and a later start
    /// does not use this.
    pub spawn: Option<crate::world::BlockPos>,
}

impl WorldSettings {
    /// The keys of a `[worlds.<name>]` table.
    const KEYS: [&str; 5] = [
        "view-distance",
        "chunks-per-tick",
        "spawn-radius",
        "tick-radius",
        "spawn",
    ];

    /// The first value set, of the table `[<path>]`, that breaks its rule, as an error; the
    /// global keys are checked the same way.
    pub(crate) fn check(&self, path: &str) -> Result<()> {
        if let Some(v) = self.view_distance {
            range(Key::new(path, "view-distance"), v, VIEW_DISTANCE_RANGE)?;
        }
        if let Some(v) = self.chunks_per_tick {
            range(Key::new(path, "chunks-per-tick"), v, CHUNKS_PER_TICK_RANGE)?;
        }
        if let Some(v) = self.spawn_radius {
            range(Key::new(path, "spawn-radius"), v, RADIUS_RANGE)?;
        }
        if let Some(v) = self.tick_radius {
            range(Key::new(path, "tick-radius"), v, RADIUS_RANGE)?;
        }
        Ok(())
    }

    /// Replaces the values of the keys present in `table`, the table `[<path>]`.
    fn merge(&mut self, path: &str, table: &Table) -> Result<()> {
        for (name, value) in table {
            let key = Key::new(path, name);
            match name.as_str() {
                "view-distance" => {
                    self.view_distance = Some(int(value, key, VIEW_DISTANCE_RANGE)?);
                }
                "chunks-per-tick" => {
                    self.chunks_per_tick = Some(int(value, key, CHUNKS_PER_TICK_RANGE)?);
                }
                "spawn-radius" => self.spawn_radius = Some(int(value, key, RADIUS_RANGE)?),
                "tick-radius" => self.tick_radius = Some(int(value, key, RADIUS_RANGE)?),
                "spawn" => self.spawn = Some(block_pos(value, key)?),
                _ => return Err(key.unknown()),
            }
        }
        Ok(())
    }
}

/// The view values in effect in one world ([`Config::view_limits`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ViewLimits {
    /// The largest chunk radius a player may use.
    pub(crate) view_distance: u8,
    /// Chunks sent to each player per tick.
    pub(crate) chunks_per_tick: u32,
    /// The chunk radius sent before a player spawns, at most `view_distance`.
    pub(crate) spawn_radius: u8,
    /// The chunk radius ticked around players, at most `view_distance`.
    pub(crate) tick_radius: u8,
}

impl ViewLimits {
    /// These values (the global ones, [`Config::global_view`]) with each value `world` sets in
    /// their place, and the radii at most the view distance.
    pub(crate) fn apply(self, world: &WorldSettings) -> Self {
        let view_distance = world.view_distance.unwrap_or(self.view_distance);
        Self {
            view_distance,
            chunks_per_tick: world.chunks_per_tick.unwrap_or(self.chunks_per_tick),
            spawn_radius: world
                .spawn_radius
                .unwrap_or(self.spawn_radius)
                .min(view_distance),
            tick_radius: world
                .tick_radius
                .unwrap_or(self.tick_radius)
                .min(view_distance),
        }
    }
}

/// How light is calculated (`lighting`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lighting {
    /// `"dynamic"`: light is calculated and follows block changes.
    Dynamic,
    /// `"on-load"`: light is calculated when a chunk loads but not after block changes.
    OnLoad,
    /// `"fixed"`: no light is calculated; every block has `fixed-light-level` as sky light and no
    /// block light. Random ticks see that light everywhere, underground too: at 15 crops and
    /// saplings grow in caves (saplings by day, as sky light dims at night), below 9 crops never
    /// grow and below 8 saplings never grow. Without block light, ice blocks and snow layers never
    /// melt (frosted ice still melts in sky light).
    Fixed,
}

impl Lighting {
    /// The `lighting` values.
    const NAMES: [(&str, Self); 3] = [
        ("dynamic", Self::Dynamic),
        ("on-load", Self::OnLoad),
        ("fixed", Self::Fixed),
    ];

    /// Whether any light is calculated: not [`Lighting::Fixed`].
    pub(crate) fn is_calculated(self) -> bool {
        self != Self::Fixed
    }
}

/// When the data files are checked at start (`integrity-check`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntegrityCheck {
    /// `"always"`: on every start.
    Always,
    /// `"after-unclean-shutdown"`: only when the last run did not stop normally.
    AfterUncleanShutdown,
    /// `"never"`: never at start; the check command still checks on request.
    Never,
}

impl IntegrityCheck {
    /// The `integrity-check` values.
    const NAMES: [(&str, Self); 3] = [
        ("always", Self::Always),
        ("after-unclean-shutdown", Self::AfterUncleanShutdown),
        ("never", Self::Never),
    ];
}

/// The console log detail (`log-level`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogLevel {
    /// `"off"`: Anchor writes no console log; its events go to the program's own `tracing`
    /// subscriber, if any (as with
    /// [`ServerBuilder::without_console_log`](crate::ServerBuilder::without_console_log)).
    Off,
    /// `"error"`: errors only.
    Error,
    /// `"warn"`: warnings and errors.
    Warn,
    /// `"info"`: starts, stops, joins and the levels above.
    Info,
    /// `"debug"`: connections and the levels above.
    Debug,
    /// `"trace"`: packets and the levels above.
    Trace,
}

impl LogLevel {
    /// The `log-level` values.
    const NAMES: [(&str, Self); 6] = [
        ("off", Self::Off),
        ("error", Self::Error),
        ("warn", Self::Warn),
        ("info", Self::Info),
        ("debug", Self::Debug),
        ("trace", Self::Trace),
    ];
}

/// A player game mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
// pmmp-class: GameMode
pub enum GameMode {
    /// `"survival"`.
    Survival,
    /// `"creative"`.
    Creative,
    /// `"adventure"`.
    Adventure,
    /// `"spectator"`.
    Spectator,
}

impl GameMode {
    /// PMMP's names of the game modes (`GameMode::getAliases`), which [`from_name`](Self::from_name)
    /// takes.
    const ALIASES: [(&str, Self); 13] = [
        ("survival", Self::Survival),
        ("s", Self::Survival),
        ("0", Self::Survival),
        ("creative", Self::Creative),
        ("c", Self::Creative),
        ("1", Self::Creative),
        ("adventure", Self::Adventure),
        ("a", Self::Adventure),
        ("2", Self::Adventure),
        ("spectator", Self::Spectator),
        ("v", Self::Spectator),
        ("view", Self::Spectator),
        ("3", Self::Spectator),
    ];

    /// The `gamemode` values.
    const NAMES: [(&str, Self); 4] = [
        ("survival", Self::Survival),
        ("creative", Self::Creative),
        ("adventure", Self::Adventure),
        ("spectator", Self::Spectator),
    ];

    /// The game mode one of PMMP's names gives, ignoring ASCII case: the name (`survival`), its
    /// letter (`s`, `c`, `a`, `v` or `view`) or its number (`0` to `3`). The `gamemode` setting
    /// takes the full names only.
    #[doc(alias = "GameMode::fromString")]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALIASES
            .iter()
            .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
            .map(|&(_, mode)| mode)
    }

    /// The protocol's game type of the mode (`StartGame`, `UpdatePlayerGameType`, the server
    /// list).
    pub(crate) const fn game_type(self) -> GameType {
        match self {
            Self::Survival => GameType::Survival,
            Self::Creative => GameType::Creative,
            Self::Adventure => GameType::Adventure,
            Self::Spectator => GameType::Spectator,
        }
    }

    /// The mode of a protocol game type ([`game_type`](Self::game_type) reversed); `None` for
    /// [`GameType::Default`], which names the world's mode.
    pub(crate) fn from_game_type(game_type: GameType) -> Option<Self> {
        Self::NAMES
            .into_iter()
            .map(|(_, mode)| mode)
            .find(|mode| mode.game_type() == game_type)
    }
}

/// A world difficulty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Difficulty {
    /// `"peaceful"`.
    Peaceful,
    /// `"easy"`.
    Easy,
    /// `"normal"`.
    Normal,
    /// `"hard"`.
    Hard,
}

impl Difficulty {
    /// The `difficulty` values.
    const NAMES: [(&str, Self); 4] = [
        ("peaceful", Self::Peaceful),
        ("easy", Self::Easy),
        ("normal", Self::Normal),
        ("hard", Self::Hard),
    ];

    /// The protocol ID of the difficulty (`SetDifficulty`, `StartGame`), which PMMP and
    /// `/difficulty <0-3>` use too: peaceful 0 to hard 3.
    pub(crate) const fn id(self) -> u8 {
        match self {
            Self::Peaceful => 0,
            Self::Easy => 1,
            Self::Normal => 2,
            Self::Hard => 3,
        }
    }

    /// The difficulty of a protocol ID ([`id`](Self::id) reversed).
    pub(crate) fn from_id(id: u8) -> Option<Self> {
        Self::NAMES
            .into_iter()
            .map(|(_, difficulty)| difficulty)
            .find(|difficulty| difficulty.id() == id)
    }
}

/// The generator of a new world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LevelType {
    /// `"flat"`.
    Flat,
    /// `"void"`.
    Void,
}

impl LevelType {
    /// The `level-type` values.
    const NAMES: [(&str, Self); 2] = [("flat", Self::Flat), ("void", Self::Void)];
}

/// When a melee hit can be critical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CriticalHits {
    /// `"off"`: never.
    Off,
    /// `"sprint"`: only when the attacker is not sprinting.
    Sprint,
    /// `"always"`: sprinting or not.
    Always,
}

impl CriticalHits {
    /// The `critical-hits` values.
    const NAMES: [(&str, Self); 3] = [
        ("off", Self::Off),
        ("sprint", Self::Sprint),
        ("always", Self::Always),
    ];
}

/// A thread count setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Threads {
    /// `"auto"`: chosen from the number of logical CPUs.
    Auto,
    /// A fixed number of threads.
    Count(NonZeroU16),
}

/// A settings failure: reading or writing the file, its syntax, or a value.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The settings file cannot be read, or is missing when [`Config::update_file`] updates it.
    Read {
        /// The settings file.
        path: PathBuf,
        /// The I/O failure.
        source: io::Error,
    },
    /// The settings file cannot be written: the default file when it is missing, or an update.
    Write {
        /// The settings file.
        path: PathBuf,
        /// The I/O failure.
        source: io::Error,
    },
    /// The settings file is larger than 1 MiB, or an update would make it so.
    TooLarge {
        /// The settings file.
        path: PathBuf,
    },
    /// The file is not UTF-8 TOML.
    Syntax {
        /// The 1-based line and column of the fault, when the parser reports one.
        position: Option<(usize, usize)>,
        /// The parser's description of the fault.
        message: String,
    },
    /// A key Anchor does not know, such as `general.motdd`.
    UnknownKey(String),
    /// A top-level table Anchor does not know, such as `plugin` for `[plugin]`.
    UnknownTable(String),
    /// A table lacking a key that [`Config::update_file`] cannot add to, as it is written
    /// without its `[table]` header (an inline table or dotted keys), such as `general`.
    HeaderlessTable(String),
    /// A `[worlds.<name>]` table whose name is not a world the server declares, such as
    /// `lobby` for `[worlds.lobby]`.
    UnknownWorld(String),
    /// A value of the wrong type or form.
    Invalid {
        /// The key, such as `general.motd`.
        key: String,
        /// What the value must be.
        expected: String,
    },
    /// A whole number outside its allowed range.
    Range {
        /// The key, such as `general.view-distance`.
        key: String,
        /// The value given.
        value: i64,
        /// The smallest allowed value.
        min: i64,
        /// The largest allowed value.
        max: i64,
    },
    /// A compression setting that [`batch::Settings::new`] rejects; the source names the rule.
    Batch {
        /// The key, such as `developer.compression-level`.
        key: &'static str,
        /// The rejection.
        source: batch::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, .. } => write!(f, "cannot read {}", path.display()),
            Self::Write { path, .. } => write!(f, "cannot write {}", path.display()),
            Self::TooLarge { path } => {
                write!(
                    f,
                    "{} is larger than {MAX_FILE_BYTES} bytes",
                    path.display()
                )
            }
            Self::Syntax {
                position: Some((line, column)),
                message,
            } => write!(f, "invalid toml at line {line}, column {column}: {message}"),
            Self::Syntax {
                position: None,
                message,
            } => write!(f, "invalid toml: {message}"),
            Self::UnknownKey(key) => write!(f, "unknown key `{key}`"),
            Self::UnknownTable(name) => write!(f, "unknown table `[{name}]`"),
            Self::HeaderlessTable(name) => write!(
                f,
                "keys cannot be added to `{name}`: write it as a `[{name}]` table"
            ),
            Self::UnknownWorld(name) => write!(f, "`[worlds.{name}]` names no declared world"),
            Self::Invalid { key, expected } => write!(f, "`{key}` must be {expected}"),
            Self::Range {
                key,
                value,
                min,
                max,
            } => write!(f, "`{key}` = {value} is outside {min}..={max}"),
            Self::Batch { key, .. } => write!(f, "`{key}` is invalid"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            Self::Batch { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// The result of a settings operation.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A key's place in the file: its table and name. The full path is built only for an error.
#[derive(Clone, Copy)]
struct Key<'a> {
    table: &'a str,
    name: &'a str,
}

impl<'a> Key<'a> {
    fn new(table: &'a str, name: &'a str) -> Self {
        Self { table, name }
    }

    fn top(name: &'a str) -> Self {
        Self::new("", name)
    }

    fn general(name: &'a str) -> Self {
        Self::new("general", name)
    }

    fn developer(name: &'a str) -> Self {
        Self::new("developer", name)
    }

    /// The dotted path, such as `general.motd`.
    fn path(self) -> String {
        if self.table.is_empty() {
            self.name.to_owned()
        } else {
            format!("{}.{}", self.table, self.name)
        }
    }

    /// The error for a key Anchor does not read.
    fn unknown(self) -> Error {
        Error::UnknownKey(self.path())
    }

    fn invalid(self, expected: impl Into<String>) -> Error {
        Error::Invalid {
            key: self.path(),
            expected: expected.into(),
        }
    }

    fn out_of_range<T: TryInto<i64>>(self, value: i64, range: RangeInclusive<T>) -> Error {
        let (min, max) = range.into_inner();
        Error::Range {
            key: self.path(),
            value,
            min: min.try_into().unwrap_or(i64::MIN),
            max: max.try_into().unwrap_or(i64::MAX),
        }
    }
}

fn table_of<'v>(value: &'v Value, key: Key<'_>) -> Result<&'v Table> {
    value.as_table().ok_or_else(|| key.invalid("a table"))
}

fn boolean(value: &Value, key: Key<'_>) -> Result<bool> {
    value.as_bool().ok_or_else(|| key.invalid("true or false"))
}

/// Reads `"auto"` or a thread count.
fn threads(value: &Value, key: Key<'_>) -> Result<Threads> {
    if value.as_str() == Some("auto") {
        return Ok(Threads::Auto);
    }
    if !value.is_integer() {
        return Err(key.invalid("\"auto\" or a whole number"));
    }
    let count = int(value, key, THREADS_RANGE)?;
    NonZeroU16::new(count)
        .map(Threads::Count)
        .ok_or_else(|| key.out_of_range(0, THREADS_RANGE))
}

/// Reads `[x, y, z]`: three whole numbers that fit `i32`.
fn block_pos(value: &Value, key: Key<'_>) -> Result<crate::world::BlockPos> {
    let [x, y, z] = value
        .as_array()
        .and_then(|items| <&[Value; 3]>::try_from(items.as_slice()).ok())
        .ok_or_else(|| key.invalid("[x, y, z], three whole numbers"))?;
    let coordinate = |v| int(v, key, i32::MIN..=i32::MAX);
    Ok(crate::world::BlockPos::new(
        coordinate(x)?,
        coordinate(y)?,
        coordinate(z)?,
    ))
}

/// Reads a whole number that fits `T` and lies in `range`.
fn int<T>(value: &Value, key: Key<'_>, range: RangeInclusive<T>) -> Result<T>
where
    T: Copy + PartialOrd + TryFrom<i64> + TryInto<i64>,
{
    let number = value
        .as_integer()
        .ok_or_else(|| key.invalid("a whole number"))?;
    T::try_from(number)
        .ok()
        .filter(|n| range.contains(n))
        .ok_or_else(|| key.out_of_range(number, range))
}

/// Reads an empty string as `None`, any other string as [`parsed`] does.
fn optional<T: FromStr>(value: &Value, key: Key<'_>, expected: &str) -> Result<Option<T>> {
    if text(value, key)?.is_empty() {
        Ok(None)
    } else {
        parsed(value, key, expected).map(Some)
    }
}

/// Reads a string in the form `T` parses.
fn parsed<T: FromStr>(value: &Value, key: Key<'_>, expected: &str) -> Result<T> {
    text(value, key)?.parse().map_err(|_| key.invalid(expected))
}

fn text<'v>(value: &'v Value, key: Key<'_>) -> Result<&'v str> {
    value.as_str().ok_or_else(|| key.invalid("a string"))
}

/// Reads an array of strings.
fn strings<'v>(value: &'v Value, key: Key<'_>) -> Result<impl Iterator<Item = &'v str>> {
    let items = value
        .as_array()
        .filter(|items| items.iter().all(Value::is_str))
        .ok_or_else(|| key.invalid("a list of strings"))?;
    Ok(items.iter().filter_map(Value::as_str))
}

/// Reads a `lighting` value; the `true` and `false` of older files mean `"dynamic"` and `"fixed"`.
fn lighting(value: &Value, key: Key<'_>) -> Result<Lighting> {
    match value.as_bool() {
        Some(true) => Ok(Lighting::Dynamic),
        Some(false) => Ok(Lighting::Fixed),
        None => choice(value, key, &Lighting::NAMES),
    }
}

/// Reads one of `choices` by name.
fn choice<T: Copy>(value: &Value, key: Key<'_>, choices: &[(&str, T)]) -> Result<T> {
    let name = value.as_str();
    choices
        .iter()
        .find(|(n, _)| Some(*n) == name)
        .map(|&(_, choice)| choice)
        .ok_or_else(|| {
            let names: Vec<String> = choices.iter().map(|(n, _)| format!("\"{n}\"")).collect();
            key.invalid(format!("one of {}", names.join(", ")))
        })
}

/// The error naming `key` when `value` lies outside `range`.
fn range<T>(key: Key<'_>, value: T, range: RangeInclusive<T>) -> Result<()>
where
    T: Copy + PartialOrd + Into<i64>,
{
    if range.contains(&value) {
        Ok(())
    } else {
        Err(key.out_of_range(value.into(), range))
    }
}

/// The error naming `key` unless `value` is a text that the RakNet pong lists between `;`
/// separators: not empty, at most `max` bytes, and free of `;` and control characters.
fn list_text(key: Key<'_>, value: &str, max: usize) -> Result<()> {
    length(key, value, max)?;
    if !is_list_text(value) {
        return Err(key.invalid("a non-empty text without `;` or control characters"));
    }
    Ok(())
}

/// The error naming `key` when `value` is longer than `max` bytes.
fn length(key: Key<'_>, value: &str, max: usize) -> Result<()> {
    if value.len() <= max {
        Ok(())
    } else {
        Err(key.invalid(format!("at most {max} bytes long")))
    }
}

/// Whether `motd` is one the `motd` setting and [`Context::set_motd`](crate::Context::set_motd)
/// take: at most [`MAX_MOTD_BYTES`], not empty, without `;` or control characters.
pub fn is_motd(motd: &str) -> bool {
    motd.len() <= MAX_MOTD_BYTES && is_list_text(motd)
}

/// Whether `value` is not empty and free of `;` and control characters: a text the RakNet pong
/// can list.
fn is_list_text(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(|c| c == ';' || c.is_control())
}

/// Whether `name` has the shape `namespace:path` of a block identifier.
fn is_block_name(name: &str) -> bool {
    let Some((namespace, path)) = name.split_once(':') else {
        return false;
    };
    let base = |b: u8| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.');
    !namespace.is_empty()
        && !path.is_empty()
        && namespace.bytes().all(base)
        && path.bytes().all(|b| base(b) || b == b'/')
}

/// A settings file's bytes as text; [`Error::Syntax`] if they are not UTF-8.
fn utf8(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(|e| Error::Syntax {
        position: position(bytes, e.valid_up_to()),
        message: "invalid utf-8".to_owned(),
    })
}

/// The [`Error::Syntax`] of a TOML parse failure of `text`.
fn syntax(text: &str, error: &toml::de::Error) -> Error {
    Error::Syntax {
        position: error
            .span()
            .and_then(|span| position(text.as_bytes(), span.start)),
        message: error.message().to_owned(),
    }
}

/// The 1-based line and column of byte `offset` in `bytes`.
fn position(bytes: &[u8], offset: usize) -> Option<(usize, usize)> {
    let before = bytes.get(..offset)?;
    let line = before.split(|&b| b == b'\n').count();
    // A column counts characters: every byte except UTF-8 continuation bytes.
    let column = before
        .rsplit(|&b| b == b'\n')
        .next()?
        .iter()
        .filter(|&&b| b & 0xc0 != 0x80)
        .count()
        + 1;
    Some((line, column))
}

/// The key of the batch setting that `error` rejected.
#[expect(
    clippy::unreachable,
    reason = "invariant: `batch::Settings::new` rejects only the three values matched here"
)]
fn batch_key(error: &batch::Error) -> &'static str {
    match error {
        batch::Error::Threshold => "developer.compression-threshold",
        batch::Error::ZlibLevel(_) => "developer.compression-level",
        batch::Error::MaxBatchBytes(_) => "developer.max-batch-bytes",
        _ => unreachable!("batch settings are rejected for one of three values"),
    }
}

/// Reads the settings file, or returns `None` when it does not exist.
fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read_bounded(path, MAX_FILE_BYTES) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::FileTooLarge => Err(Error::TooLarge {
            path: path.to_owned(),
        }),
        Err(source) => Err(Error::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str) -> Result<Config> {
        Config::default().apply(text.as_bytes())
    }

    #[test]
    fn default_text_gives_the_default_config() {
        assert_eq!(
            apply(DEFAULT_FILE).expect("default file"),
            Config::default()
        );
        Config::default()
            .validate()
            .expect("default config is valid");
    }

    #[test]
    fn validate_rejects_unknown_tables_that_load_keeps() {
        let config = apply("[plugin]\nanswer = 42\n[general]\npvp = false\n").expect("loads");
        assert!(!config.general.pvp);
        let error = config.validate().expect_err("unknown table");
        assert!(matches!(&error, Error::UnknownTable(name) if name == "plugin"));
        assert_eq!(error.to_string(), "unknown table `[plugin]`");
        let inline = apply("plugin = { answer = 42 }").expect("an inline table is a table");
        assert!(inline.validate().is_err());
    }

    // pmmp: Server::getIp, Server::getIpV6, Server::getPort, Server::getPortV6
    // pmmp: Server::getOnlineMode, Server::requiresAuthentication
    #[test]
    fn addresses_port_and_sign_in_come_from_the_general_table() {
        let config = apply(
            "[general]\nserver-ip = \"127.0.0.1\"\nserver-ipv6 = \"::1\"\nserver-port = 19200\n\
             xbox-auth = false\n",
        )
        .expect("loads");
        let general = &config.general;
        assert_eq!(general.server_ip, Ipv4Addr::LOCALHOST);
        assert_eq!(general.server_ipv6, Ipv6Addr::LOCALHOST);
        assert_eq!(general.server_port, 19_200);
        assert!(!general.xbox_auth);
        assert!(Config::default().general.xbox_auth);
    }

    #[test]
    fn loading_twice_is_idempotent() {
        let text = "[plugin]\n[general]\nmotd = \"x\"\n";
        let once = apply(text).expect("loads");
        let twice = once.clone().apply(text.as_bytes()).expect("loads");
        assert_eq!(once, twice);
    }

    #[test]
    fn validate_checks_values_set_in_code() {
        let mut config = Config::default();
        config.general.view_distance = 1;
        let error = config.validate().expect_err("view distance 1");
        assert_eq!(
            error.to_string(),
            "`general.view-distance` = 1 is outside 2..=64"
        );

        let mut config = Config::default();
        config.developer.nethernet_tls_cert = Some("cert.pem".into());
        assert!(config.validate().is_err());
        config.developer.nethernet_tls_key = Some(PathBuf::new());
        assert!(config.validate().is_err());
        config.developer.nethernet_tls_key = Some("key.pem".into());
        config.validate().expect("both set");

        let mut config = Config::default();
        config.developer.max_batch_bytes = 0;
        let error = config.validate().expect_err("zero batch bytes");
        assert!(matches!(
            error,
            Error::Batch {
                key: "developer.max-batch-bytes",
                source: batch::Error::MaxBatchBytes(0)
            }
        ));
        // A batch the codec takes, but too small for one player list entry with a large skin.
        config.developer.max_batch_bytes = crate::tick::MAX_LIST_ENTRY_BYTES - 1;
        let error = config
            .validate()
            .expect_err("batch smaller than a list entry");
        assert!(matches!(error, Error::Invalid { key, .. } if key == "developer.max-batch-bytes"));
        config.developer.max_batch_bytes = crate::tick::MAX_LIST_ENTRY_BYTES;
        config.validate().expect("a list entry fits");

        let mut config = Config::default();
        config.general.resource_packs.push(PathBuf::new());
        assert!(config.validate().is_err());

        // A path into the old `packs` folder is refused; another relative path is not.
        for (path, valid) in [
            ("packs/b.zip", false),
            ("packs", false),
            ("sub/b.zip", true),
        ] {
            let mut config = Config::default();
            config.general.resource_packs = vec![path.into()];
            let error = config.validate().err().map(|e| e.to_string());
            match error {
                None => assert!(valid, "{path}"),
                Some(error) => assert!(!valid && error.contains("not under packs/"), "{error}"),
            }
        }
    }

    // pmmp: ResourcePackManager::__construct
    #[test]
    fn pack_paths_resolve_against_the_data_pack_folder() {
        let mut config = Config::default();
        config.general.resource_packs = vec!["a.zip".into(), "/abs/b.mcpack".into()];
        let folder = Path::new("data").join(crate::pack::FOLDER);
        let paths = config.resource_pack_paths(&folder);
        assert_eq!(paths[0], Path::new("data/packs/a.zip"));
        assert_eq!(paths[1], Path::new("/abs/b.mcpack"));
    }

    // pmmp: ResourcePackManager::resourcePacksRequired
    // pmmp: ResourcePackManager::setResourcePacksRequired
    #[test]
    fn force_resources_reaches_the_pack_offer() {
        let mut config = Config::default();
        assert!(!config.general.force_resources);
        config.general.force_resources = true;
        let offer = crate::pack::Offer::new(Vec::new(), config.general.force_resources).unwrap();
        assert!(offer.info().texture_pack_required);
    }

    #[test]
    fn batch_settings_follow_the_config() {
        let settings = Config::default().batch_settings().expect("valid");
        assert_eq!(settings, batch::Settings::default());
    }

    #[test]
    fn effective_radii_are_limited_by_view_distance() {
        let mut config = Config::default();
        config.general.view_distance = 3;
        config.developer.spawn_radius = 8;
        config.developer.tick_radius = 2;
        assert_eq!(config.effective_spawn_radius(), 3);
        assert_eq!(config.effective_tick_radius(), 2);
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn load_under_update_config_updates_the_file_first() {
        let dir = crate::save::tests::TempDir::new("config-update");
        let path = dir.0.join("anchor.toml");
        let old = "[general]
old-key = false
";
        std::fs::write(&path, old).unwrap();
        let error = Config::default().load_file(&path).unwrap_err();
        assert!(matches!(error, Error::UnknownKey(_)), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
        assert_eq!(Config::default().updated(), None);

        let config = Config::default().load_updated(&path).unwrap();
        let update = config.updated().unwrap();
        assert_eq!(update.removed(), ["general.old-key"]);
        assert_eq!(update.added().len(), 68);
        let again = Config::default().load_updated(&path).unwrap();
        assert!(again.updated().unwrap().is_empty());
    }

    #[test]
    fn an_ipv6_public_address_needs_ipv6_listening() {
        let mut config = Config::default();
        config.general.enable_ipv6 = false;
        for (address, valid) in [
            ("203.0.113.7", true),
            ("::ffff:203.0.113.7", true),
            ("2001:db8::1", false),
        ] {
            config.general.public_ip = Some(address.parse().unwrap());
            let error = config.validate().err().map(|e| e.to_string());
            match error {
                None => assert!(valid, "{address}"),
                Some(error) => assert!(
                    !valid && error.starts_with("`general.public-ip` must be"),
                    "{error}"
                ),
            }
        }
        config.general.enable_ipv6 = true;
        config.validate().expect("IPv6 listening is on");
    }

    #[test]
    fn a_level_name_that_is_no_file_name_is_refused_naming_the_key() {
        let mut config = Config::default();
        for name in ["con", "a/b", "ends."] {
            config.general.level_name = name.to_owned();
            let error = config.validate().expect_err(name);
            assert!(
                matches!(&error, Error::Invalid { key, .. } if key == "general.level-name"),
                "{name}: {error}"
            );
        }
        config.general.level_name = "lobby".to_owned();
        config.validate().expect("a file name");
    }

    #[test]
    fn the_world_keys_are_those_a_worlds_table_reads() {
        let values = [
            ("view-distance", "8"),
            ("chunks-per-tick", "8"),
            ("spawn-radius", "2"),
            ("tick-radius", "2"),
            ("spawn", "[0, 64, 0]"),
        ];
        let keys = values.map(|(key, _)| key);
        assert_eq!(keys, WorldSettings::KEYS);
        for (key, value) in values {
            apply(&format!("[worlds.w]\n{key} = {value}\n")).expect(key);
        }
        let error = apply("[worlds.w]\nmotd = \"x\"\n").expect_err("an unknown key");
        assert_eq!(error.to_string(), "unknown key `worlds.w.motd`");
    }

    #[test]
    fn errors_are_send_sync_and_static() {
        fn assert_error<T: std::error::Error + Send + Sync + 'static>() {}
        assert_error::<Error>();
    }

    // pmmp: GameMode::fromString, GameMode
    #[test]
    fn game_modes_parse_pmmps_names_letters_and_numbers() {
        assert_eq!(GameMode::from_name("Survival"), Some(GameMode::Survival));
        assert_eq!(GameMode::from_name("c"), Some(GameMode::Creative));
        assert_eq!(GameMode::from_name("2"), Some(GameMode::Adventure));
        assert_eq!(GameMode::from_name("VIEW"), Some(GameMode::Spectator));
        assert_eq!(GameMode::from_name("v"), Some(GameMode::Spectator));
        for bad in ["", "4", "creativ", "survival "] {
            assert_eq!(GameMode::from_name(bad), None, "{bad:?}");
        }
        for (name, mode) in GameMode::NAMES {
            assert_eq!(GameMode::from_name(name), Some(mode));
        }
    }
}
