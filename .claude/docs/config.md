# Configuration

Every key of `anchor.toml` with its type, range, and meaning, and how the file is read, checked, and updated. Rules are in `.claude/rules/config.md`. Why settings are never stored in the data files, and what that means for values changed at run time, is in `storage.md`.

## Reading the file

- The server program reads the file with `Config::default().load("anchor.toml")?` and passes the result to `ServerBuilder::config`. Anchor touches a settings file only in `Config::load` and `Config::update_file`; a program that calls neither has no settings file. As a library, Anchor leaves the path, and whether to use a file at all, to the program.
- `load` does blocking file I/O, so the program calls it before the server starts.
- If the file is missing, `load` writes `DEFAULT_FILE` (through a temporary file renamed into place) and then reads it, so the first start and every later start with that file give the same settings. A file that cannot be written is `Error::Write`; a path that cannot be read (a directory, for example) is `Error::Read`.
- Precedence: `Config::default()`, then values set in code, then the keys present in the file (`load`), then values set in code after `load`. A key left out keeps the earlier value, so a newer Anchor starts with an older file. The default file sets every key, so when `load` writes it, only `[worlds.<name>]` values set in code before `load` survive.
- The file is UTF-8 TOML of at most 1 MiB (`Error::TooLarge`); a byte order mark is allowed. A syntax error is `Error::Syntax`, with the line and column when the parser reports them.

## Default file

`DEFAULT_FILE` in `src/config.rs`, which `Config::load` writes. It holds every key with its default value.

## Validation

`load` checks the values after reading the file, and `ServerBuilder::build` checks the whole config again, so values set in code pass the same checks. A failure is a `config::Error` that names the key (`anchor::Error::Config` from `build`):

- `UnknownKey`: a key Anchor does not know, at the top level or inside a known table (`[general]`, `[developer]`, `[worlds.<name>]`).
- `UnknownTable`: an unknown top-level table. `load` keeps it and `build` rejects it.
- `UnknownWorld`: a `[worlds.<name>]` table that names no declared world (`level-name` or `ServerBuilder::world`). Only `build` checks this.
- `Invalid`: a value of the wrong type or form.
- `Range`: a whole number outside its range, with the key, the value, the minimum, and the maximum.
- `Batch`: a `compression-threshold`, `compression-level`, or `max-batch-bytes` that `batch::Settings::new` rejects, with the key and the cause. A number that does not fit the field's type is `Range` instead. A `max-batch-bytes` that the codec accepts but that is below one player list entry (the minimum in the `max-batch-bytes` row below) is `Invalid`.
- `hardcore = true` requires `difficulty = "hard"` (`Invalid`).
- A `level-name` that cannot be a world file name is `Invalid`, naming `general.level-name`.
- An IPv6 `public-ip` with `enable-ipv6 = false` is `Invalid`, naming `general.public-ip`.
- A `[worlds.<name>]` `spawn` with a number outside the world border (`world::BORDER`, from -BORDER to BORDER - 1) is `Invalid`, naming `worlds.<name>.spawn`.
- `nethernet-tls-cert` and `nethernet-tls-key` are both empty or both set (`Invalid`).

`build` also fails, with `Invalid` naming the thread key, when the thread counts exceed the budget (see `cpu-threads`), and with `anchor::Error::InvalidWorldName` when the name of a world declared with `ServerBuilder::world` cannot be a file name.

## `[general]`

`data/` here and below is the data folder, which `ServerBuilder::data_dir` can move.

| Key | Type | Range and form | Meaning |
|---|---|---|---|
| `motd` | string | 1 to 256 bytes; no `;` or control characters | Name shown in the server list. The RakNet pong carries it and `level-name` in one datagram that must fit the 576-byte minimum MTU, which bounds both lengths |
| `server-ip` | string | IPv4 address | IPv4 address to listen on; `0.0.0.0` is every interface |
| `server-ipv6` | string | IPv6 address | IPv6 address to listen on; `::` is every interface |
| `enable-ipv6` | bool | | Whether to listen on IPv6 too |
| `server-port` | integer | 0 to 65535 | The one port number the server uses, on IPv4 and IPv6: UDP for RakNet, NetherNet game data, and server list pings; TCP for NetherNet signaling |
| `public-ip` | string | empty or an IP address; IPv6 only with `enable-ipv6` | Address NetherNet players use to reach the server, advertised alone; empty advertises the address each player reached the signaling port on and the address the player typed, which reaches a server behind a NAT through its port forward (`network.md`) |
| `max-players` | integer | 1 to 10000 | Most players online at once; sizes the player and session tables |
| `xbox-auth` | bool | | Whether players must sign in to Xbox Live; `false` lets anyone join under any name |
| `white-list` | bool | | Whether only listed players can join; the `whitelist` command manages the list |
| `gamemode` | string | `survival`, `creative`, `adventure`, `spectator` | Game mode of new players. The setting takes the full names only; the `gamemode` command also takes letters and numbers (`s`, `0`) |
| `force-gamemode` | bool | | Whether players get the default game mode on every join |
| `difficulty` | string | `peaceful`, `easy`, `normal`, `hard` | Difficulty |
| `hardcore` | bool | `true` only with `difficulty = "hard"` | Hardcore |
| `pvp` | bool | | Whether players can hurt each other |
| `food` | bool | | Food |
| `xp` | bool | | Gaining XP points and levels. `false` also means no XP orbs |
| `xp-orbs` | bool | | XP orbs and throwing bottles o' enchanting. `false` computes no dropped XP, makes and reads no orbs (a loading chunk's saved orbs stay as saved and are written back), and refuses throwing bottles; no XP is given directly in their place. Points and levels follow `xp` only |
| `fall-damage` | bool | | Fall damage |
| `drowning` | bool | | Air supply under water, drowning damage, and suffocation damage |
| `armor` | bool | | Armor protection, armor wear, and thorns; `false` removes all three |
| `durability` | bool | | Tools, weapons, and armor wearing out with use. With `armor = true`, `false` keeps armor protecting without wearing |
| `daylight-cycle` | bool | | The time of day moving on; `false` also stops the client's clock (game rule `doDayLightCycle`) |
| `item-despawn-ticks` | integer | 1 to 72000 | Ticks a dropped item stays on the ground |
| `critical-hits` | string | `off`, `sprint`, `always` | Critical hits: `off` never, `sprint` only when the attacker is not sprinting (vanilla), `always` sprinting or not |
| `break-time-check` | bool | | Whether the server checks that a block took long enough to break. Reach is checked either way |
| `movement-check` | bool | | Whether the server refuses a move of more than 15 blocks in one input. With `false`, the ground check of such a move looks only at where it ends, not at the path |
| `show-coordinates` | bool | | Whether every player sees its coordinates on screen (game rule `showCoordinates`) |
| `locator-bar` | bool | | Whether the locator bar shows the other players. `false` makes and sends no `LocatorBar` or `PlayerLocation` packets and sets game rule `playerWaypoints` to 0, the value the official server saves for a world with that rule off. Game rule `locatorbar` stays `true`, as the official server saves no such rule |
| `immediate-respawn` | bool | | Whether dead players come back at once, without the death screen (game rule `doImmediateRespawn`). The client that receives the rule skips the screen and asks to respawn at once; the server's respawn flow (the `Respawn` packet, `PlayerRespawn`) is the same |
| `show-death-messages` | bool | | Whether every player sees in chat how a player died (game rule `showDeathMessages`). The message is the death screen's translation key with the dead player's and the attacker's names, which the client translates. `PlayerDeath::death_message` starts as that key (empty when the setting is off); a listener that leaves it shows the translation, other text shows as it is, and an empty message shows nothing |
| `level-name` | string | 1 to 64 bytes; no `;` or control characters; a name that can be a world file name (`world::Error::InvalidName` gives the rule) | The default world's name; its file is `data/worlds/<name>.redb`. The length bound is explained at `motd` |
| `level-type` | string | `flat`, `void` | Generator of the default world's chunks that were never saved. A custom `Generator` goes only to a world declared with `ServerBuilder::world` |
| `level-seed` | string | empty or an `i64` as text | The default world's seed; empty makes one from the world name, the same at every start (`storage.md`) |
| `view-distance` | integer | 2 to 64 | Largest view distance a player may use, in chunks |
| `save-player-data` | bool | | Whether inventories and positions are kept between joins |
| `backups` | bool | | Whether the data files are copied to `data/backups` at start and by the `backup` command (`storage.md`) |
| `max-backups` | integer | 1 to 1000 | The backups kept of each data file (`storage.md`) |
| `shutdown-message` | string | at most 1024 bytes | Message players see when the server stops |
| `resource-packs` | array of strings | non-empty paths; a relative path is relative to `data/packs` | Resource pack files (`.mcpack`, `.zip`). Earlier entries override later ones. An encrypted pack needs its key in `<file>.key` next to it. Players keep a downloaded pack by its uuid and version, so a changed pack reaches them only with a higher manifest version |
| `force-resources` | bool | | Whether players must accept the resource packs to join |
| `log-level` | string | `off`, `error`, `warn`, `info`, `debug`, `trace` | Detail of Anchor's console log; `off` writes no console log |

## `[developer]`

| Key | Type | Range and form | Meaning |
|---|---|---|---|
| `compression` | string | `snappy`, `zlib` | Network compression: Snappy saves CPU, zlib saves bandwidth |
| `compression-threshold` | integer | 1 to 65535 | Batches shorter than this many bytes are sent uncompressed. 0 is rejected because implementations disagree on what it means |
| `compression-level` | integer | 1 to 9 | zlib level, 1 (fastest) to 9 (smallest); checked even when `compression = "snappy"` |
| `max-batch-bytes` | integer | 4210786 to 67108864 | Largest decompressed batch received from a player or sent to one, in bytes. The minimum is one player list entry with the largest skin (`MAX_SKIN_BYTES`, 4 MiB): an entry cannot be split, so it must fit one batch |
| `enable-encryption` | bool | | Whether RakNet game traffic is encrypted. With `false`, anyone between a player and the server can read and change it, login included, so `false` is only for a trusted network. NetherNet traffic is always encrypted by its own transport |
| `transport` | string | `all`, `nethernet`, `raknet` | Transports that accept players. `nethernet`: a RakNet client gets no reply to its connection request, and the UDP port still answers server list pings. `raknet`: no NetherNet code runs (no TCP signaling listener, no `data/identity.key`), every UDP datagram goes to RakNet, and the NetherNet keys (`public-ip`, `nethernet-tls-cert`, `nethernet-tls-key`) are checked but unused |
| `raknet-cookies` | bool | | Whether a RakNet client must send back a connection cookie before it gets a connection; what the cookie is, what each value does, and the reason for the default are in `network.md` |
| `max-mtu-size` | integer | 576 to 1492 | Largest RakNet packet in bytes; tunnels and VPNs need a lower value. go-raknet's dialer probes 1492, 1200, and 576 |
| `nethernet-tls-cert` | string | empty or a path | PEM certificate chain for HTTPS signaling. Empty is plain HTTP, and players confirm the server key on first join. A file that cannot be read or used stops the start (`anchor::Error::TlsCertificate`) |
| `nethernet-tls-key` | string | empty or a path | PEM private key for HTTPS signaling |
| `network-threads` | string or integer | `"auto"` or 1 to 65535 | Threads for network work. `auto` is max(2, ⌊L/4⌋), L being the logical CPU count |
| `cpu-threads` | string or integer | `"auto"` or 1 to 65535 | Threads for parallel game work. `auto` is max(1, B − 2 − network threads), B being the thread budget max(L, 5), the bound on compute threads in `CLAUDE.md`. When network threads + CPU threads + 2 (the tick and save threads) exceed B, `build` fails with `anchor::Error::Config` (`config::Error::Invalid`) naming `developer.network-threads` when the total leaves no room for one CPU thread, or else `developer.cpu-threads` |
| `chunks-per-tick` | integer | 1 to 256 | Chunks sent to each player per tick. 0 would never send a chunk; 256 sends the largest view in about 3 seconds |
| `spawn-radius` | integer | 1 to 64 | Chunk radius sent before a player spawns; the effective value is min(value, `view-distance`) and, before a player spawns, the radius that player asks for |
| `tick-radius` | integer | 1 to 64 | Chunk radius around players where custom blocks get random ticks and entities are ticked; the effective value is min(value, `view-distance`) |
| `slow-empty-worlds` | bool | | Whether a world without players runs its world rules only on one tick in every 100: world `w` on the ticks `t` where (t + w) % 100 = 0, so empty worlds do not all run on one tick |
| `lighting` | string | `dynamic`, `on-load`, `fixed` | How the server calculates light. `dynamic` calculates it and follows block changes; `on-load` calculates it when a chunk loads but not after block changes; `fixed` calculates nothing and gives every position block light 0 and sky light `fixed-light-level` |
| `fixed-light-level` | integer | 0 to 15 | Sky light everywhere when `lighting = "fixed"` |
| `item-pickup-interval` | integer | 1 to 20 | Ticks between checks for items a player can pick up |
| `hook-movement-packets` | bool | | Whether movement packets the server sends pass the send hooks |
| `hook-player-input` | bool | | Whether `PlayerAuthInput` packets pass the receive hooks |
| `save-interval-minutes` | integer | 1 to 1440 | Minutes between whole-state saves |
| `integrity-check` | string | `always`, `after-unclean-shutdown`, `never` | When the data files are checked at start; the `check` command checks them while the server runs. What each value checks, and its start cost, is in `storage.md` |
| `console-input` | bool | | Whether commands are read from standard input |

## Per-world values `[worlds.<name>]`

A table may exist for each declared world. Each key it sets replaces the global value in that world only; a key left out keeps the global value.

| Key | Type | Range | Meaning |
|---|---|---|---|
| `view-distance` | integer | 2 to 64 | The world's largest view distance |
| `chunks-per-tick` | integer | 1 to 256 | The world's chunks per player per tick |
| `spawn-radius` | integer | 1 to 64 | The world's chunk radius sent before spawning; the effective value is min(value, the world's view distance) |
| `tick-radius` | integer | 1 to 64 | The world's tick radius; the effective value is min(value, the world's view distance) |
| `spawn` | three integers `[x, y, z]` | each inside the world border | Where players appear in the world; without it, the generator's spawn |

`spawn` applies only to a world whose file has no record yet; from then on the world file keeps its spawn as world state (`storage.md`). A world made or loaded at run time (`Context::create_world`, `Context::load_world`) takes these values from its `world::Options`, never from a `[worlds.<name>]` table, which only declared worlds may have. A table's name must equal a declared world's name exactly; world files ignore letter case, tables do not.

## Updating the file

Running the server program as `<program> update-config` updates a file written by an older Anchor to this Anchor's keys, with no code in the program. `Config::load` runs `Config::update_file(path)` before reading the file, since an old file may not load until it is updated; a missing file is then `Error::Read` (NotFound), and no default file is written. `Server::run` then writes the result to standard output (one `added <key>` or `removed <key>` line per key, or that the file is already up to date) and returns without starting the server. A program that never calls `Config::load` prints that nothing was updated. `update-config` with any further argument makes `Server::run` fail with `anchor::Error::Usage`. Starting never updates the file, so the file changes only when the operator chooses. The command may run while the server runs; a running server reads the new file at its next start.

- Adding: each key of the default file that the file lacks is added with its default value and the comment lines right above it in the default file. It goes after the line of the nearest key before it in the default file that the file has, or after the table's header line when the file has none of those. A table the file lacks is appended at the end with its header's comment lines and all its keys, after one blank line (none in an empty file).
- Removing: a `key = value` Anchor does not know, at the top level or inside a `[general]` or `[developer]` written with its header, is removed with its lines and the comment lines right above it (lines whose first non-blank character is `#`; a blank line ends them). A line of a multi-line string is not a comment line. A dotted key or a subtable (`[general.x]`) is left, so the result does not load and the update fails with `UnknownKey`. Unknown tables are kept; an unknown key inside a `[worlds.<name>]` table written with its header is removed like one in `[general]`.
- Every other byte stays as it was: values, comments, key order, a byte order mark, and line endings. When the file contains a CRLF line ending, added lines use CRLF. No key is added to `[worlds.<name>]` tables: a key missing there means the global value.
- No value is fixed. When the result would not load (an unknown key, an out-of-range value, a syntax error), the update returns that error and leaves the file as it was. It also refuses, leaving the file, when a table that lacks a key is written without its header (an inline table or dotted keys: `HeaderlessTable`) and when the result would exceed 1 MiB (`TooLarge`).
- The file is written only when something changed, whole (a temporary file renamed into place). The returned `config::Update` lists the added keys (`added`, in default file order) and the removed keys (`removed`, in file order). A second run changes nothing.
- The added values are the defaults, so the file means what it meant without them. A value set in code before `load`, for a key the update adds, is then replaced by the file's value, as with a missing file.

## Feature switches

The feature switches that `.claude/rules/config.md` covers are the bool keys of `[general]` from `food` to `movement-check`, `locator-bar`, and `backups`.
