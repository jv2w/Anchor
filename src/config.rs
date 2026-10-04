//! Server settings: the `anchor.toml` file (PLAN.md 8.5), its defaults and its validation.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU16;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use toml::{Table, Value};

use crate::protocol::batch;
use crate::protocol::compression::Compression;

/// The file [`Config::load`] writes when the settings file is missing. PLAN.md 8.5 holds the same
/// text; a test keeps the two byte-identical.
const DEFAULT_FILE: &str = r#"# Anchor server settings. Anchor reads this file at start and never writes it again.
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
# UDP: RakNet and server list ping. TCP: NetherNet signaling. Both use this port.
server-port = 19132
# UDP port for NetherNet game data, shared by every player.
nethernet-port = 19133
# Address players use to reach this server, if it differs from the local one (NAT).
# Empty: advertise the addresses of the local interfaces.
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
hunger = true
# Main world. Created on first start with level-type: "flat" or "void".
level-name = "world"
level-type = "flat"
# Whole number as text. Empty: random, chosen once when the world is created. Other text stops the start.
level-seed = ""
# Largest view distance a player may use, in chunks (2 to 64).
view-distance = 16
# false: player inventories and positions are not kept between joins.
save-player-data = true
# Message players see when the server stops.
shutdown-message = "Server closed"
# Resource pack files (.mcpack or .zip), relative to this file. Earlier entries override later ones.
# An encrypted pack needs its key in "<file>.key" next to it.
resource-packs = []
# true: players must accept the resource packs to join.
force-resources = false

# Settings that tune performance and protocol behavior. Change them with care.
[developer]
# Network compression: "snappy" (less CPU) or "zlib" (less bandwidth).
compression = "snappy"
# Batches smaller than this many bytes are sent uncompressed (1 to 65535).
compression-threshold = 256
# zlib only: 1 (fastest) to 9 (smallest).
compression-level = 6
# Largest decompressed batch a client may send, in bytes (1 MiB to 64 MiB).
max-batch-bytes = 16777216
# false: accept only NetherNet. The RakNet port still answers server list pings.
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
# Chunk radius around players where blocks and entities are ticked (at most view-distance).
tick-radius = 4
# Random block ticks per sub-chunk per tick (crop growth, leaf decay). 0 turns them off.
random-tick-speed = 3
# Blocks that never get random ticks, by name, for example "minecraft:grass_block".
disable-block-ticking = []
# false: no light is calculated; every check sees an open sky (see the docs for the effects).
lighting = true
# Minutes between whole-state saves (1 to 1440).
save-interval-minutes = 10
# false: Anchor does not read commands from standard input.
console-input = true
"#;

/// The largest settings file read. A real file is about 4 KiB; the cap bounds the parser's memory.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Bounds the player and session tables that are sized from it.
const MAX_PLAYERS: RangeInclusive<u32> = 1..=10_000;
/// PLAN.md 8.5.
const VIEW_DISTANCE: RangeInclusive<u8> = 2..=64;
/// The effective radius is at most `view-distance`, so a larger value means nothing.
const RADIUS: RangeInclusive<u8> = 1..=*VIEW_DISTANCE.end();
/// 0 would never send a chunk; 256 sends the largest view in about 3 seconds.
const CHUNKS_PER_TICK: RangeInclusive<u32> = 1..=256;
/// A sub-chunk holds 4096 blocks; more ticks than blocks add only cost.
pub(crate) const RANDOM_TICK_SPEED: RangeInclusive<u32> = 0..=4096;
/// go-raknet's dialer probes 1492, 1200 and 576 (PLAN.md 10.2).
const MAX_MTU_SIZE: RangeInclusive<u16> = 576..=1492;
/// One minute to one day (PLAN.md 8.5).
const SAVE_INTERVAL_MINUTES: RangeInclusive<u16> = 1..=1440;
/// A fixed thread count; the runtime checks the total against the logical CPUs (PLAN.md 10.2).
const THREADS: RangeInclusive<u16> = 1..=u16::MAX;
/// The RakNet pong carries `motd` and `level-name` in one datagram; 256 + 64 bytes and the other
/// fields fit the 576-byte minimum MTU.
const MAX_MOTD_BYTES: usize = 256;
/// See [`MAX_MOTD_BYTES`].
const MAX_LEVEL_NAME_BYTES: usize = 64;
/// A disconnect message several lines long fits.
const MAX_SHUTDOWN_MESSAGE_BYTES: usize = 1024;

/// Every server setting. [`Config::default`] holds the values of the default `anchor.toml`.
///
/// Two configs are equal when their settings are: the folder of the loaded file, against which
/// relative `resource-packs` paths resolve, is not a setting and is not compared.
#[derive(Debug, Clone, Eq, Default)]
pub struct Config {
    /// Settings for anyone opening a server (`[general]`).
    pub general: General,
    /// Settings that tune performance and protocol behavior (`[developer]`).
    pub developer: Developer,
    /// Top-level tables of the loaded file that Anchor does not know; [`Config::validate`]
    /// rejects them (PLAN.md 8.5).
    unknown_tables: BTreeSet<String>,
    /// The folder of the last file [`Config::load`] read; `None` before any.
    dir: Option<PathBuf>,
}

impl PartialEq for Config {
    fn eq(&self, other: &Self) -> bool {
        self.general == other.general
            && self.developer == other.developer
            && self.unknown_tables == other.unknown_tables
    }
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
    /// Relative `resource-packs` paths, in the file or set in code, resolve against the folder of
    /// the last file loaded (the working directory before any).
    pub fn load(mut self, path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        self.dir = path.parent().map(Path::to_path_buf);
        if let Some(bytes) = read(path)? {
            return self.apply(&bytes);
        }
        write_default(path).map_err(|source| Error::Write {
            path: path.to_owned(),
            source,
        })?;
        self.apply(DEFAULT_FILE.as_bytes())
    }

    /// The `resource-packs` files, relative paths resolved against the loaded file's folder.
    pub(crate) fn resource_pack_paths(&self) -> Vec<PathBuf> {
        self.general
            .resource_packs
            .iter()
            .map(|p| match &self.dir {
                Some(dir) => dir.join(p),
                None => p.clone(),
            })
            .collect()
    }

    /// Returns `spawn-radius` limited to `view-distance`: the chunk radius actually sent before a
    /// player spawns.
    pub fn effective_spawn_radius(&self) -> u8 {
        self.developer.spawn_radius.min(self.general.view_distance)
    }

    /// Returns `tick-radius` limited to `view-distance`: the chunk radius actually ticked.
    pub fn effective_tick_radius(&self) -> u8 {
        self.developer.tick_radius.min(self.general.view_distance)
    }

    /// Checks every value and rejects unknown tables; the server builder calls it.
    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(name) = self.unknown_tables.first() {
            return Err(Error::UnknownTable(name.clone()));
        }
        self.check()
    }

    /// Returns the batch compression settings, checked by [`batch::Settings::new`].
    pub(crate) fn batch_settings(&self) -> Result<batch::Settings> {
        let d = &self.developer;
        batch::Settings::new(
            d.compression,
            d.compression_threshold,
            d.compression_level,
            d.max_batch_bytes,
        )
        .map_err(|source| {
            let key = match source {
                batch::Error::Threshold => "developer.compression-threshold",
                batch::Error::ZlibLevel(_) => "developer.compression-level",
                batch::Error::MaxBatchBytes(_) => "developer.max-batch-bytes",
                _ => "developer",
            };
            Error::Batch { key, source }
        })
    }

    /// Parses a settings file's bytes, lays its keys over these values and checks the result.
    fn apply(mut self, bytes: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes).map_err(|e| Error::Syntax {
            position: position(bytes, e.valid_up_to()),
            message: "invalid utf-8".to_owned(),
        })?;
        let table = text.parse::<Table>().map_err(|e| Error::Syntax {
            position: e.span().and_then(|span| position(bytes, span.start)),
            message: e.message().to_owned(),
        })?;
        for (name, value) in &table {
            match name.as_str() {
                "general" => self.general.merge(table_of(value, Key::top(name))?)?,
                "developer" => self.developer.merge(table_of(value, Key::top(name))?)?,
                _ if value.is_table() => {
                    self.unknown_tables.insert(name.clone());
                }
                _ => return Err(Error::UnknownKey(name.clone())),
            }
        }
        self.check()?;
        Ok(self)
    }

    /// Checks every value against its rule; the one check for loaded and code-set values.
    fn check(&self) -> Result<()> {
        let g = &self.general;
        list_text(Key::general("motd"), &g.motd, MAX_MOTD_BYTES)?;
        range(Key::general("max-players"), g.max_players, MAX_PLAYERS)?;
        if g.hardcore && g.difficulty != Difficulty::Hard {
            return Err(
                Key::general("hardcore").invalid("false unless `general.difficulty` is \"hard\"")
            );
        }
        list_text(
            Key::general("level-name"),
            &g.level_name,
            MAX_LEVEL_NAME_BYTES,
        )?;
        range(
            Key::general("view-distance"),
            g.view_distance,
            VIEW_DISTANCE,
        )?;
        length(
            Key::general("shutdown-message"),
            &g.shutdown_message,
            MAX_SHUTDOWN_MESSAGE_BYTES,
        )?;
        if g.resource_packs.iter().any(|p| p.as_os_str().is_empty()) {
            return Err(Key::general("resource-packs").invalid("a list of non-empty paths"));
        }

        let d = &self.developer;
        self.batch_settings()?;
        range(Key::developer("max-mtu-size"), d.max_mtu_size, MAX_MTU_SIZE)?;
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
            CHUNKS_PER_TICK,
        )?;
        range(Key::developer("spawn-radius"), d.spawn_radius, RADIUS)?;
        range(Key::developer("tick-radius"), d.tick_radius, RADIUS)?;
        range(
            Key::developer("random-tick-speed"),
            d.random_tick_speed,
            RANDOM_TICK_SPEED,
        )?;
        if !d.disable_block_ticking.iter().all(|n| is_block_name(n)) {
            return Err(Key::developer("disable-block-ticking")
                .invalid("a list of block names like \"minecraft:grass_block\""));
        }
        range(
            Key::developer("save-interval-minutes"),
            d.save_interval_minutes,
            SAVE_INTERVAL_MINUTES,
        )
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
    /// `server-port`: UDP for RakNet and server list pings, TCP for NetherNet signaling, on
    /// IPv4 and IPv6 alike.
    #[doc(alias = "Server::getPort")]
    #[doc(alias = "Server::getPortV6")]
    pub server_port: u16,
    /// `nethernet-port`: the UDP port for NetherNet game data.
    pub nethernet_port: u16,
    /// `public-ip`: the address players use to reach this server; `None` advertises the local
    /// interfaces.
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
    /// `level-name`: the main world's name.
    pub level_name: String,
    /// `level-type`: the generator of the main world when it is created.
    pub level_type: LevelType,
    /// `level-seed`: the world seed; `None` picks a random one when the world is created.
    pub level_seed: Option<i64>,
    /// `view-distance`: the largest view distance a player may use, in chunks.
    pub view_distance: u8,
    /// `save-player-data`: whether inventories and positions are kept between joins.
    pub save_player_data: bool,
    /// `shutdown-message`: the message players see when the server stops.
    pub shutdown_message: String,
    /// `resource-packs`: resource pack files, relative to the settings file.
    #[doc(alias = "ResourcePackManager::__construct")]
    pub resource_packs: Vec<PathBuf>,
    /// `force-resources`: whether players must accept the resource packs to join.
    #[doc(alias = "ResourcePackManager::resourcePacksRequired")]
    #[doc(alias = "ResourcePackManager::setResourcePacksRequired")]
    pub force_resources: bool,
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
                "nethernet-port" => self.nethernet_port = int(value, key, u16::MIN..=u16::MAX)?,
                "public-ip" => {
                    self.public_ip = optional(value, key, "empty or an IP address")?;
                }
                "max-players" => self.max_players = int(value, key, MAX_PLAYERS)?,
                "xbox-auth" => self.xbox_auth = boolean(value, key)?,
                "white-list" => self.white_list = boolean(value, key)?,
                "gamemode" => self.gamemode = choice(value, key, &GameMode::NAMES)?,
                "force-gamemode" => self.force_gamemode = boolean(value, key)?,
                "difficulty" => self.difficulty = choice(value, key, &Difficulty::NAMES)?,
                "hardcore" => self.hardcore = boolean(value, key)?,
                "pvp" => self.pvp = boolean(value, key)?,
                "hunger" => self.hunger = boolean(value, key)?,
                "level-name" => text(value, key)?.clone_into(&mut self.level_name),
                "level-type" => self.level_type = choice(value, key, &LevelType::NAMES)?,
                "level-seed" => {
                    self.level_seed = optional(value, key, "empty or a whole number as text")?;
                }
                "view-distance" => self.view_distance = int(value, key, VIEW_DISTANCE)?,
                "save-player-data" => self.save_player_data = boolean(value, key)?,
                "shutdown-message" => text(value, key)?.clone_into(&mut self.shutdown_message),
                "resource-packs" => {
                    self.resource_packs = strings(value, key)?.map(PathBuf::from).collect();
                }
                "force-resources" => self.force_resources = boolean(value, key)?,
                _ => return Err(Error::UnknownKey(key.path())),
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
            server_port: 19132,
            nethernet_port: 19133,
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
            level_name: "world".to_owned(),
            level_type: LevelType::Flat,
            level_seed: None,
            view_distance: 16,
            save_player_data: true,
            shutdown_message: "Server closed".to_owned(),
            resource_packs: Vec::new(),
            force_resources: false,
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
    /// `max-batch-bytes`: the largest decompressed batch a client may send.
    pub max_batch_bytes: usize,
    /// `raknet`: whether RakNet connections are accepted; pings are answered either way.
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
    /// `tick-radius`: the chunk radius around players where blocks and entities are ticked.
    pub tick_radius: u8,
    /// `random-tick-speed`: random block ticks per sub-chunk per tick.
    pub random_tick_speed: u32,
    /// `disable-block-ticking`: blocks that never get random ticks, by name.
    pub disable_block_ticking: Vec<String>,
    /// `lighting`: whether light is calculated.
    pub lighting: bool,
    /// `save-interval-minutes`: minutes between whole-state saves.
    pub save_interval_minutes: u16,
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
                "raknet" => self.raknet = boolean(value, key)?,
                "max-mtu-size" => self.max_mtu_size = int(value, key, MAX_MTU_SIZE)?,
                "nethernet-tls-cert" => {
                    self.nethernet_tls_cert = optional(value, key, "empty or a path")?;
                }
                "nethernet-tls-key" => {
                    self.nethernet_tls_key = optional(value, key, "empty or a path")?;
                }
                "network-threads" => self.network_threads = threads(value, key)?,
                "cpu-threads" => self.cpu_threads = threads(value, key)?,
                "chunks-per-tick" => self.chunks_per_tick = int(value, key, CHUNKS_PER_TICK)?,
                "spawn-radius" => self.spawn_radius = int(value, key, RADIUS)?,
                "tick-radius" => self.tick_radius = int(value, key, RADIUS)?,
                "random-tick-speed" => {
                    self.random_tick_speed = int(value, key, RANDOM_TICK_SPEED)?;
                }
                "disable-block-ticking" => {
                    self.disable_block_ticking = strings(value, key)?.map(str::to_owned).collect();
                }
                "lighting" => self.lighting = boolean(value, key)?,
                "save-interval-minutes" => {
                    self.save_interval_minutes = int(value, key, SAVE_INTERVAL_MINUTES)?;
                }
                "console-input" => self.console_input = boolean(value, key)?,
                _ => return Err(Error::UnknownKey(key.path())),
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
            lighting: true,
            save_interval_minutes: 10,
            console_input: true,
        }
    }
}

/// The `compression` values.
const COMPRESSION_NAMES: [(&str, Compression); 2] =
    [("snappy", Compression::Snappy), ("zlib", Compression::Zlib)];

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
    /// The `gamemode` values.
    const NAMES: [(&str, Self); 4] = [
        ("survival", Self::Survival),
        ("creative", Self::Creative),
        ("adventure", Self::Adventure),
        ("spectator", Self::Spectator),
    ];

    /// PMMP's names of the game modes (`GameMode::getAliases`), which [`from_name`](Self::from_name)
    /// takes and the game mode commands offer.
    pub(crate) const ALIASES: [(&'static str, Self); 13] = [
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

/// A thread count setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Threads {
    /// `"auto"`: chosen from the number of logical CPUs (PLAN.md 10.2).
    Auto,
    /// A fixed number of threads.
    Count(NonZeroU16),
}

/// A settings failure: reading or writing the file, its syntax, or a value.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The settings file exists but cannot be read.
    Read {
        /// The settings file.
        path: PathBuf,
        /// The I/O failure.
        source: io::Error,
    },
    /// The settings file is missing and the default file cannot be written.
    Write {
        /// The settings file.
        path: PathBuf,
        /// The I/O failure.
        source: io::Error,
    },
    /// The settings file is larger than 1 MiB.
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
            Self::Write { path, .. } => {
                write!(f, "cannot write the default settings to {}", path.display())
            }
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
pub type Result<T> = std::result::Result<T, Error>;

/// A key's place in the file: its table and name. The full path is built only for an error.
#[derive(Clone, Copy)]
struct Key<'a> {
    table: &'static str,
    name: &'a str,
}

impl<'a> Key<'a> {
    fn new(table: &'static str, name: &'a str) -> Self {
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

    /// Returns the dotted path, such as `general.motd`.
    fn path(self) -> String {
        if self.table.is_empty() {
            self.name.to_owned()
        } else {
            format!("{}.{}", self.table, self.name)
        }
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

fn text<'v>(value: &'v Value, key: Key<'_>) -> Result<&'v str> {
    value.as_str().ok_or_else(|| key.invalid("a string"))
}

fn boolean(value: &Value, key: Key<'_>) -> Result<bool> {
    value.as_bool().ok_or_else(|| key.invalid("true or false"))
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

/// Reads a string in the form `T` parses.
fn parsed<T: FromStr>(value: &Value, key: Key<'_>, expected: &str) -> Result<T> {
    text(value, key)?.parse().map_err(|_| key.invalid(expected))
}

/// Reads an empty string as `None`, any other string as [`parsed`] does.
fn optional<T: FromStr>(value: &Value, key: Key<'_>, expected: &str) -> Result<Option<T>> {
    if text(value, key)?.is_empty() {
        Ok(None)
    } else {
        parsed(value, key, expected).map(Some)
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

/// Reads an array of strings.
fn strings<'v>(value: &'v Value, key: Key<'_>) -> Result<impl Iterator<Item = &'v str>> {
    let items = value
        .as_array()
        .filter(|items| items.iter().all(Value::is_str))
        .ok_or_else(|| key.invalid("a list of strings"))?;
    Ok(items.iter().filter_map(Value::as_str))
}

/// Reads `"auto"` or a thread count.
fn threads(value: &Value, key: Key<'_>) -> Result<Threads> {
    if value.as_str() == Some("auto") {
        return Ok(Threads::Auto);
    }
    if !value.is_integer() {
        return Err(key.invalid("\"auto\" or a whole number"));
    }
    let count = int(value, key, THREADS)?;
    NonZeroU16::new(count)
        .map(Threads::Count)
        .ok_or_else(|| key.out_of_range(0, THREADS))
}

/// Checks that `value` lies in `range`.
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

/// Checks that `value` is at most `max` bytes long.
fn length(key: Key<'_>, value: &str, max: usize) -> Result<()> {
    if value.len() <= max {
        Ok(())
    } else {
        Err(key.invalid(format!("at most {max} bytes long")))
    }
}

/// Checks a text that the RakNet pong lists between `;` separators: not empty, at most `max`
/// bytes, and free of `;` and control characters.
fn list_text(key: Key<'_>, value: &str, max: usize) -> Result<()> {
    length(key, value, max)?;
    if value.is_empty() || value.chars().any(|c| c == ';' || c.is_control()) {
        return Err(key.invalid("a non-empty text without `;` or control characters"));
    }
    Ok(())
}

/// Returns whether `name` has the shape `namespace:path` of a block identifier.
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

/// Returns the 1-based line and column of byte `offset` in `bytes`.
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

/// Reads the settings file, or returns `None` when it does not exist.
#[expect(
    clippy::disallowed_types,
    reason = "off-tick: read once before the server starts"
)]
fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| Error::Read {
            path: path.to_owned(),
            source,
        })?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > MAX_FILE_BYTES) {
        return Err(Error::TooLarge {
            path: path.to_owned(),
        });
    }
    Ok(Some(bytes))
}

/// Writes [`DEFAULT_FILE`] to `path` through `<path>.tmp` in the same folder, so a crash leaves
/// either no settings file or a whole one.
#[expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "off-tick: written once before the server starts"
)]
fn write_default(path: &Path) -> io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut file = std::fs::File::create(&temp)?;
    file.write_all(DEFAULT_FILE.as_bytes())?;
    file.sync_all()?;
    // Windows cannot rename a file that is still open.
    drop(file);
    std::fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::{Config, DEFAULT_FILE, Error, GameMode, batch};

    fn apply(text: &str) -> super::Result<Config> {
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

    // pmmp: Server::getIp, Server::getIpV6, Server::getPort, Server::getPortV6, Server::getOnlineMode, Server::requiresAuthentication
    #[test]
    fn addresses_port_and_sign_in_come_from_the_general_table() {
        let config = apply(
            "[general]\nserver-ip = \"127.0.0.1\"\nserver-ipv6 = \"::1\"\nserver-port = 19200\n\
             xbox-auth = false\n",
        )
        .expect("loads");
        let general = &config.general;
        assert_eq!(general.server_ip, std::net::Ipv4Addr::LOCALHOST);
        assert_eq!(general.server_ipv6, std::net::Ipv6Addr::LOCALHOST);
        assert_eq!(general.server_port, 19200);
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
        config.developer.nethernet_tls_key = Some(std::path::PathBuf::new());
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

        let mut config = Config::default();
        config
            .general
            .resource_packs
            .push(std::path::PathBuf::new());
        assert!(config.validate().is_err());
    }

    // pmmp: ResourcePackManager::__construct
    #[test]
    fn pack_paths_resolve_against_the_loaded_file_folder() {
        let mut config = Config::default();
        config.general.resource_packs = vec!["a.zip".into(), "/abs/b.mcpack".into()];
        assert_eq!(
            config.resource_pack_paths()[0],
            std::path::Path::new("a.zip")
        );
        let loaded = Config {
            dir: Some("server/conf".into()),
            ..config.clone()
        };
        let paths = loaded.resource_pack_paths();
        assert_eq!(paths[0], std::path::Path::new("server/conf").join("a.zip"));
        assert_eq!(paths[1], std::path::Path::new("/abs/b.mcpack"));
        assert_eq!(loaded, config, "the folder is not a setting");
    }

    // pmmp: ResourcePackManager::resourcePacksRequired, ResourcePackManager::setResourcePacksRequired
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
