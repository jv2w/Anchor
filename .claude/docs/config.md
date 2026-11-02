# Configuration

Every key of `anchor.toml` with its type, default, range, and meaning, and how the file is read, checked, and updated. Rules are in `.claude/rules/config.md`. Why settings are never stored in the data files, and what that means for values changed at run time, is in `storage.md`.

## Reading the file

- The server program reads the file with `Config::default().load("anchor.toml")?` and passes the result to `ServerBuilder::config`. Anchor touches a settings file only in `Config::load` and `Config::update_file`; a program that calls neither has no settings file. As a library, Anchor leaves the path, and whether to use a file at all, to the program.
- `load` does blocking file I/O, so the program calls it before the server starts.
- If the file is missing, `load` writes the default file below (a temporary file renamed into place) and then reads it, so the first start and every later start with that file give the same settings. A file that cannot be written is `Error::Write`; a path that cannot be read (a directory, for example) is `Error::Read`.
- Precedence: `Config::default()`, then values set in code, then the keys present in the file (`load`), then values set in code after `load`. A key left out keeps the earlier value, so a newer Anchor starts with an older file. The default file sets every key, so when `load` writes it, only `[worlds.<name>]` values set in code before `load` survive.
- The file is UTF-8 TOML of at most 1 MiB (`Error::TooLarge`); a byte order mark is allowed. A syntax error is `Error::Syntax`, with the line and column when the parser reports them.

## Default file

`DEFAULT_FILE` in `src/config.rs`, which `Config::load` writes. It holds every key.

```toml
# Anchor server settings. Anchor reads this file at start and writes it only to update it:
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
food = true
# Gaining XP points and levels. false also means no XP orbs.
xp = true
# XP orbs and throwing bottles o' enchanting. false deletes the orbs of loading chunks.
xp-orbs = true
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
```

## Validation

`load` checks the values after reading the file, and `ServerBuilder::build` checks the whole config again, so values set in code pass the same checks. A failure is a `config::Error` that names the key (`anchor::Error::Config` from `build`):

- `UnknownKey`: a key Anchor does not know, at the top level or inside a known table (`[general]`, `[developer]`, `[worlds.<name>]`).
- `UnknownTable`: an unknown top-level table. `load` keeps it and `build` rejects it.
- `Invalid`: a value of the wrong type or form.
- `Range`: a whole number outside its range, with the key, the value, the minimum, and the maximum.
- `Batch`: a `compression-threshold`, `compression-level`, or `max-batch-bytes` that `batch::Settings::new` rejects, with the key and the cause. A number that does not fit the field's type is `Range` instead. A `max-batch-bytes` inside the codec's 1 MiB to 64 MiB but below one player list entry (4210786 bytes) is `Invalid`.
- `hardcore = true` requires `difficulty = "hard"` (`Invalid`).
- A `level-name` that cannot be a world file name is `Invalid`, naming `general.level-name`.
- An IPv6 `public-ip` with `enable-ipv6 = false` is `Invalid`, naming `general.public-ip`.
- `nethernet-tls-cert` and `nethernet-tls-key` are both empty or both set (`Invalid`).
- `UnknownWorld`: a `[worlds.<name>]` table that names no declared world (`level-name` or `ServerBuilder::world`). Only `build` checks this.

`build` also fails, with `Invalid` naming the thread key, when the thread counts exceed the budget (see `cpu-threads`), and with `anchor::Error::InvalidWorldName` when the name of a world declared with `ServerBuilder::world` cannot be a file name.

## `[general]`

`data/` here and below is the data folder, which `ServerBuilder::data_dir` can move.

| Key | Type | Default | Range and form | Meaning |
|---|---|---|---|---|
| `motd` | string | `"Anchor Server"` | 1 to 256 bytes; no `;` or control characters | Name shown in the server list. The RakNet pong carries it and `level-name` in one datagram that must fit the 576-byte minimum MTU, which bounds both lengths |
| `server-ip` | string | `"0.0.0.0"` | IPv4 address | IPv4 address to listen on; `0.0.0.0` is every interface |
| `server-ipv6` | string | `"::"` | IPv6 address | IPv6 address to listen on; `::` is every interface |
| `enable-ipv6` | bool | `true` | | Whether to listen on IPv6 too |
| `server-port` | integer | `19132` | 0 to 65535 | The one port number the server uses, on IPv4 and IPv6: UDP for RakNet, NetherNet game data, and server list pings; TCP for NetherNet signaling |
| `public-ip` | string | `""` | empty or an IP address; IPv6 only with `enable-ipv6` | Address NetherNet players use to reach the server when it differs from the local one (NAT); empty advertises the address each player reached the signaling port on |
| `max-players` | integer | `20` | 1 to 10000 | Most players online at once; sizes the player and session tables |
| `xbox-auth` | bool | `true` | | Whether players must sign in to Xbox Live; `false` lets anyone join under any name |
| `white-list` | bool | `false` | | Whether only listed players can join; the `whitelist` command manages the list |
| `gamemode` | string | `"survival"` | `survival`, `creative`, `adventure`, `spectator` | Game mode of new players. The setting takes the full names only; the `gamemode` command also takes letters and numbers (`s`, `0`) |
| `force-gamemode` | bool | `false` | | Whether players get the default game mode on every join |
| `difficulty` | string | `"normal"` | `peaceful`, `easy`, `normal`, `hard` | Difficulty |
| `hardcore` | bool | `false` | `true` only with `difficulty = "hard"` | Hardcore |
| `pvp` | bool | `true` | | Whether players can hurt each other |
| `food` | bool | `true` | | Food |
| `xp` | bool | `true` | | Gaining XP points and levels. `false` also means no XP orbs, and enchanting tables and anvils count the player's level as 0, refusing anything that costs levels |
| `xp-orbs` | bool | `true` | | XP orbs and throwing bottles o' enchanting. `false` computes no dropped XP, makes and reads no orbs (the orbs of loading chunks are deleted), and refuses throwing bottles; no XP is given directly in their place. Points, levels, and `/xp` follow `xp` only |
| `fall-damage` | bool | `true` | | Fall damage |
| `drowning` | bool | `true` | | Air supply under water, drowning damage, and suffocation damage |
| `armor` | bool | `true` | | Armor protection, armor wear, and thorns; `false` removes all three |
| `durability` | bool | `true` | | Tools, weapons, and armor wearing out with use. With `armor = true`, `false` keeps armor protecting without wearing |
| `beds` | bool | `true` | | Sleeping in beds and bed spawn points; `false` makes beds decorative blocks |
| `daylight-cycle` | bool | `true` | | The time of day moving on; `false` also stops the client's clock (game rule `doDayLightCycle`) |
| `recipe-book` | bool | `true` | | The recipe book; `false` hides it and refuses its quick crafting, while crafting in the grid still works |
| `vanilla-recipes` | bool | `true` | | The vanilla recipes; `false` leaves only the recipes the server program registers |
| `item-despawn-ticks` | integer | `6000` | 1 to 72000 | Ticks a dropped item stays on the ground |
| `critical-hits` | string | `"sprint"` | `off`, `sprint`, `always` | Critical hits: `off` never, `sprint` only when the attacker is not sprinting (vanilla), `always` sprinting or not |
| `break-time-check` | bool | `true` | | Whether the server checks that a block took long enough to break. Reach is checked either way |
| `movement-check` | bool | `true` | | Whether the server refuses a move of more than 15 blocks in one input. With `false`, the ground check of such a move looks only at where it ends, not at the path |
| `show-coordinates` | bool | `false` | | Whether every player sees its coordinates on screen (game rule `showCoordinates`) |
| `locator-bar` | bool | `true` | | Whether the locator bar shows the other players. `false` makes and sends no `LocatorBar` or `PlayerLocation` packets and sets game rule `playerWaypoints` to 0, the value the official server saves for a world with that rule off. Game rule `locatorbar` stays `true`, as the official server saves no such rule |
| `immediate-respawn` | bool | `false` | | Whether dead players come back at once, without the death screen (game rule `doImmediateRespawn`). The client that receives the rule skips the screen and asks to respawn at once; the server's respawn flow (the `Respawn` packet, `PlayerRespawn`) is the same |
| `show-death-messages` | bool | `false` | | Whether every player sees in chat how a player died (game rule `showDeathMessages`). The message is the death screen's translation key with the dead player's and the attacker's names, which the client translates. `PlayerDeath::death_message` starts as that key (empty when the setting is off); a listener that leaves it shows the translation, other text shows as it is, and an empty message shows nothing |
| `level-name` | string | `"world"` | 1 to 64 bytes; no `;` or control characters; a name that can be a world file name (`world::Error::InvalidName` gives the rule) | The default world's name; its file is `data/worlds/<name>.redb`. The length bound is explained at `motd` |
| `level-type` | string | `"flat"` | `flat`, `void` | Generator of the default world's chunks that were never saved |
| `level-seed` | string | `""` | empty or an `i64` as text | The default world's seed; empty makes one from the world name, the same at every start (`storage.md`) |
| `view-distance` | integer | `16` | 2 to 64 | Largest view distance a player may use, in chunks |
| `save-player-data` | bool | `true` | | Whether inventories and positions are kept between joins |
| `backups` | bool | `true` | | Whether the data files are copied to `data/backups` at start and by the `backup` command (`storage.md`) |
| `shutdown-message` | string | `"The server has stopped"` | at most 1024 bytes | Message players see when the server stops |
| `resource-packs` | array of strings | `[]` | non-empty paths; a relative path is relative to `data/packs` and may not start with `packs` | Resource pack files (`.mcpack`, `.zip`). Earlier entries override later ones. An encrypted pack needs its key in `<file>.key` next to it. Players keep a downloaded pack by its uuid and version: raise the manifest version when a pack changes |
| `force-resources` | bool | `false` | | Whether players must accept the resource packs to join |
| `log-level` | string | `"info"` | `off`, `error`, `warn`, `info`, `debug`, `trace` | Detail of Anchor's console log; `off` writes no console log |

A relative `resource-packs` path that starts with `packs` is rejected, and the user changes it: it would name `data/packs/packs/...`, which is where a path written relative to a `packs` folder beside the settings file ends up. Packs are read only from `data/packs`.

## `[developer]`

| Key | Type | Default | Range and form | Meaning |
|---|---|---|---|---|
| `compression` | string | `"snappy"` | `snappy`, `zlib` | Network compression: Snappy saves CPU, zlib saves bandwidth |
| `compression-threshold` | integer | `256` | 1 to 65535 | Batches shorter than this many bytes are sent uncompressed. 0 is rejected because implementations disagree on what it means |
| `compression-level` | integer | `6` | 1 to 9 | zlib level, 1 (fastest) to 9 (smallest); checked even when `compression = "snappy"` |
| `max-batch-bytes` | integer | `16777216` | 4210786 to 67108864 | Largest decompressed batch received from a player or sent to one, in bytes. The minimum is one player list entry with the largest skin (`MAX_SKIN_BYTES`, 4 MiB): an entry cannot be split, so it must fit one batch |
| `enable-encryption` | bool | `true` | | Whether RakNet game traffic is encrypted. With `false`, anyone between a player and the server can read and change it, login included, so `false` is only for a trusted network. NetherNet traffic is always encrypted by its own transport |
| `raknet` | bool | `true` | | Whether RakNet connections are accepted; `false` accepts only NetherNet: a RakNet client gets no reply to its connection request, and the UDP port still answers server list pings |
| `max-mtu-size` | integer | `1492` | 576 to 1492 | Largest RakNet packet in bytes; lower it behind tunnels or VPNs. go-raknet's dialer probes 1492, 1200, and 576 |
| `nethernet-tls-cert` | string | `""` | empty or a path | PEM certificate chain for HTTPS signaling. Empty is plain HTTP, and players confirm the server key on first join. A file that cannot be read or used stops the start (`anchor::Error::TlsCertificate`) |
| `nethernet-tls-key` | string | `""` | empty or a path | PEM private key for HTTPS signaling |
| `network-threads` | string or integer | `"auto"` | `"auto"` or 1 to 65535 | Threads for network work. `auto` is max(1, ⌊L/4⌋), L being the logical CPU count |
| `cpu-threads` | string or integer | `"auto"` | `"auto"` or 1 to 65535 | Threads for parallel game work. `auto` is max(1, max(L, 4) − 2 − network threads). When network threads + CPU threads + 2 (the tick and save threads) exceed max(L, 4), `build` fails with `anchor::Error::Config` (`config::Error::Invalid`) naming `developer.network-threads` when the total leaves no room for one CPU thread, or else `developer.cpu-threads` |
| `chunks-per-tick` | integer | `4` | 1 to 256 | Chunks sent to each player per tick. 0 would never send a chunk; 256 sends the largest view in about 3 seconds |
| `spawn-radius` | integer | `4` | 1 to 64 | Chunk radius sent before a player spawns; the effective value is min(value, `view-distance`) and, before a player spawns, the radius that player asks for |
| `tick-radius` | integer | `4` | 1 to 64 | Chunk radius around players where blocks get random ticks and entities are ticked; the effective value is min(value, `view-distance`). Block updates (liquids, fire, falling blocks) run in every loaded chunk |
| `random-tick-speed` | integer | `3` | 0 to 4096 | Random block ticks per sub-chunk per tick (crop growth, leaf decay); 0 turns them off. A sub-chunk holds 4096 blocks, so more ticks only add cost |
| `disable-block-ticking` | array of strings | `[]` | block names `namespace:path` of `a`–`z`, `0`–`9`, `_`, `-`, `.`, and `/` in the path | Blocks that never get random ticks. Every block of the same kind as a named block stops too. Custom blocks ignore this list |
| `slow-empty-worlds` | bool | `false` | | Whether a world without players runs only on ticks that are multiples of 100 |
| `lighting` | string | `"dynamic"` | `dynamic`, `on-load`, `fixed`, or a bool | How the server calculates light. `dynamic` calculates it and follows block changes; `on-load` calculates it when a chunk loads but not after block changes; `fixed` calculates nothing and gives every position block light 0 and sky light `fixed-light-level`. `true` reads as `dynamic` and `false` as `fixed` |
| `fixed-light-level` | integer | `15` | 0 to 15 | Sky light everywhere when `lighting = "fixed"` |
| `item-pickup-interval` | integer | `1` | 1 to 20 | Ticks between checks for items a player can pick up |
| `hook-movement-packets` | bool | `true` | | Whether movement packets the server sends pass the send hooks |
| `hook-player-input` | bool | `true` | | Whether `PlayerAuthInput` packets pass the receive hooks |
| `save-interval-minutes` | integer | `10` | 1 to 1440 | Minutes between whole-state saves |
| `integrity-check` | string | `"always"` | `always`, `after-unclean-shutdown`, `never` | When the data files are checked at start; the `check` command checks them while the server runs. What each value checks, and its start cost, is in `storage.md` |
| `console-input` | bool | `true` | | Whether commands are read from standard input |

With `lighting = "fixed"`, random ticks see that light everywhere, underground too. At 15, crops and saplings grow in caves (saplings only by day, as sky light dims at night); below 9 crops never grow, and below 8 saplings never grow. Without block light, ice blocks and snow layers never melt (frosted ice still melts in sky light). The default is 15 because at 0 crops never grow, and block light 15 would melt ice and snow.

## Per-world values `[worlds.<name>]`

A table may exist for each declared world. Each key it sets replaces the global value in that world only; a key left out keeps the global value.

| Key | Type | Range | Meaning |
|---|---|---|---|
| `view-distance` | integer | 2 to 64 | The world's largest view distance |
| `chunks-per-tick` | integer | 1 to 256 | The world's chunks per player per tick |
| `spawn-radius` | integer | 1 to 64 | The world's chunk radius sent before spawning, at most the world's view distance |
| `tick-radius` | integer | 1 to 64 | The world's tick radius, at most the world's view distance |
| `spawn` | three integers `[x, y, z]` | each an `i32` | Where players appear in the world when it is new; without it, the generator's spawn |

`spawn` applies only to a world whose file has no record yet; from then on the world file keeps its spawn as world state (`storage.md`). A world made or loaded at run time (`Context::create_world`, `Context::load_world`) takes these values from its `world::Options`, never from a `[worlds.<name>]` table, which only declared worlds may have. A table's name must equal a declared world's name exactly; world files ignore letter case, tables do not.

## Keys named differently in PMMP

| PMMP | Anchor |
|---|---|
| `network.batch-threshold` | `compression-threshold` |
| `chunk-ticking.blocks-per-subchunk-per-tick` | `random-tick-speed` (the name of the `randomTickSpeed` game rule; default 3 as in PMMP, while the official server's rule defaults to 1) |
| `chunk-sending.per-tick` | `chunks-per-tick` |
| `ticks-per.autosave` | `save-interval-minutes` (minutes, not ticks) |
| `debug.level` | `log-level` (named levels and `off`, not a number) |
| `console.enable-input` | `console-input` |

Every other PMMP key with an Anchor counterpart keeps its last path segment, such as `network.max-mtu-size` → `max-mtu-size`.

PMMP keys with no counterpart:
- PHP runtime, reports, and updates: `memory`, `settings.async-workers`, `settings.enable-profiling`, `settings.profile-report-trigger`, `timings`, `auto-report`, `anonymous-statistics`, `auto-updater`, `settings.enable-dev-builds`.
- Plugins: `plugins`.
- Out of scope: `enable-query` and `settings.query-plugins` (query), `network.upnp-forwarding`, `level-settings.default-format` (Anchor has one world format).
- Replaced by Anchor's structure: `player.verify-xuid`, `generator-settings` (generators are Rust types), `aliases` and `worlds` (commands and worlds are registered in code), `language` and `settings.force-language`, `server-portv6` (one `server-port` serves IPv4 and IPv6), `auto-save` (`save-interval-minutes` always saves), `network.async-compression` and `network.async-compression-threshold` (compression always runs outside the tick).
- Not provided: `chunk-generation.population-queue-size`, `console.title-tick`.

## Updating the file

Running the server program as `<program> update-config` updates a file written by an older Anchor to this Anchor's keys, with no code in the program. `Config::load` runs `Config::update_file(path)` before reading the file, since an old file may not load until it is updated; a missing file is then `Error::Read` (NotFound), and no default file is written. `Server::run` then writes the result to standard output (one `added <key>` or `removed <key>` line per key, or that the file is already up to date) and returns without starting the server. A program that never calls `Config::load` prints that nothing was updated. `update-config` with any further argument makes `Server::run` fail with `anchor::Error::Usage`. Starting never updates the file, so the file changes only when the operator chooses. The command may run while the server runs; a running server reads the new file at its next start.

- Adding: each key of the default file that the file lacks is added with its default value and the comment lines right above it in the default file. It goes after the line of the nearest key before it in the default file that the file has, or after the table's header line when the file has none of those. A table the file lacks is appended at the end with its header's comment lines and all its keys, after one blank line (none in an empty file).
- Removing: a `key = value` Anchor does not know, at the top level or inside a `[general]` or `[developer]` written with its header, is removed with its lines and the comment lines right above it (lines whose first non-blank character is `#`; a blank line ends them). A line of a multi-line string is not a comment line. A dotted key or a subtable (`[general.x]`) is left, so the result does not load and the update fails with `UnknownKey`. Unknown tables are kept; an unknown key inside a `[worlds.<name>]` table written with its header is removed like one in `[general]`.
- Every other byte stays as it was: values, comments, key order, a byte order mark, and line endings. When the file contains a CRLF line ending, added lines use CRLF. No key is added to `[worlds.<name>]` tables: a key missing there means the global value.
- No value is fixed. When the result would not load (an unknown key, an out-of-range value, a syntax error), the update returns that error and leaves the file as it was. It also refuses, leaving the file, when a table that lacks a key is written without its header (an inline table or dotted keys: `HeaderlessTable`) and when the result would exceed 1 MiB (`TooLarge`).
- The file is written only when something changed, whole (a temporary file renamed into place). The returned `config::Update` lists the added keys (`added`, in default file order) and the removed keys (`removed`, in file order). A second run changes nothing.
- The added values are the defaults, so the file means what it meant without them. A value set in code before `load`, for a key the update adds, is then replaced by the file's value, as with a missing file.
- Older value forms that still load (`lighting = true`) and the comments of keys already present are not changed.

## Feature switches

The feature switches that `.claude/rules/config.md` covers are the bool keys of `[general]` from `food` to `movement-check`, `locator-bar`, and `backups`.
