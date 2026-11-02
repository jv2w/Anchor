# Tests and verification

Work rules are in `.claude/rules/testing.md`.

Anchor's checks have three layers. `cargo xtask check` is the light check of what changed, `cargo xtask verify` is the full check, and P6 is a separate tool that compares packets with the official server (the server program Mojang distributes). What none of them can see, such as the client's screen, is checked with the real client.

## `cargo xtask check`

It looks only at the files that differ from HEAD (changed, staged, deleted, or untracked), runs its steps in order, and stops at the first failure. The last line is `PASS` or `FAIL`.

1. `cargo fmt --check`
2. `cargo clippy --workspace --all-targets -- -D warnings` (this builds every target of the workspace, `examples/` included)
3. The tests of what changed:
   - A file under `src/`: the library tests of its module (`cargo test -p anchor --lib -- <module>::`). That is the file's own module when it has submodules, else the module that owns its folder (`foo/bar.rs` and `foo/tests.rs` run `foo::`). `src/lib.rs` runs every library test.
   - A build file (`Cargo.toml`, `Cargo.lock`, `.clippy.toml`, `.rustfmt.toml`, `rust-toolchain.toml`, by file name in any folder, or a file under `.cargo/`): under `xtask/`, xtask's unit tests; anywhere else, every library test.
   - `tests/<name>.rs` or `xtask/tests/<name>.rs`: that test file. A changed shared file in a subfolder (such as `tests/support/`) runs every integration test of its package.
   - A Rust file under `xtask/src/`: xtask's unit tests.
   - A file a Rust source pulls in (the file of a `#[path = "…"]` module, an `include_str!` or `include_bytes!` whose path is a string literal, both from the source's folder, or `concat!(env!("CARGO_MANIFEST_DIR"), "/…")` from the source's crate folder): it counts as a change of that source, which runs that source's tests by the rules above, in the source's own crate.

- When no Rust source, build file, or pulled-in file changed, the check prints `no code changed` and passes. This includes changes to Go sources (`xtask/go/`) alone: their tests run in `verify` (`xtask/tests/go.rs` and the bot tests). A changed Rust file under `examples/` or `vendor/` runs only the format check and clippy; the vendor copies' tests run in `verify`.
- The builds go to `target/check/`, so a check neither waits on a running `verify` nor rebuilds the running `xtask.exe`.
- A pass writes a stamp of HEAD and of the changed files the build reads, with their contents, to `target/check/pass`, only when the tree after the checks equals the tree before them. An unchanged tree then prints `unchanged since the last pass` and `PASS` at once.
- A deleted `tests/<name>.rs` or `xtask/tests/<name>.rs` has no test target, so it adds no test step.

## `cargo xtask verify`

The steps run in order, and every step runs even after a failure. Each step's line starts with `ok  ` or `FAIL`, after the output of a failure. The last line is the overall result, `PASS` or `FAIL`. Any failed step fails the whole run; a cargo step also fails when it prints a warning.

| Order | Step | What it checks |
|---|---|---|
| 1 | `cargo xtask gen --check` | The generated protocol and data code equals the generator's output |
| 2 | `cargo xtask table --check` | The API table (`xtask/data/dump/table.md`) is current and has no blank row |
| 3 | lint reason tags | Every `#[expect]` reason in the repository's Rust sources (`src`, `tests`, `examples`, `xtask/src`, `xtask/tests`) starts with one of the tags in `CLAUDE.md` |
| 4 | `cargo fmt --check` | Formatting |
| 5 | `cargo clippy --workspace --all-targets -- -D warnings` | Lints (`[workspace.lints]`, the `.clippy.toml` deny lists) |
| 6 | `cargo test --workspace` | Every unit and integration test of Anchor and xtask: protocol conformance, the bot tests, the Go tests, the vendor reproduction tests |
| 7 | vendor copy tests | The own tests of `vendor/sctp-proto` (library), `vendor/redb` (library and integration), `vendor/str0m` (library only, with Anchor's features), and `vendor/dimpl` (library and integration) |
| 8 | `cargo doc --workspace --no-deps` | Public documentation and its links (`RUSTDOCFLAGS=-D warnings`) |
| 9 | `cargo doc --workspace --no-deps --document-private-items` | Links in private documentation (built separately in `target/verify-private-docs/`) |

- The other cargo steps build in `target/verify/`, so they never rebuild the running `xtask.exe`.
- str0m's integration tests run only in `cargo xtask vendor-test str0m` (`.claude/docs/dependencies.md`).
- Tools it needs: Go on `PATH` (the bot, the oracle, the Go tests), and Perl for the OpenSSL build of the dimpl tests: on Windows the Strawberry Perl copy in `.cache/tools/` (`.claude/docs/data.md`), which `verify` passes as `OPENSSL_SRC_PERL`; on Linux the system `perl` and `make`. On Linux the dimpl tests also build their wolfSSL dev-dependency from source, which needs `autoconf`, `automake`, and `libtool`. Without a tool, its step fails.
- P6 is not part of `verify`: it needs the official server. For `dump --check`, see `.claude/docs/data.md`.

## Where tests live

- Public API tests (`tests/`):
  - `block.rs`, `item.rs`: block state and item lookups, checked against the reference list and the dump.
  - `config.rs`: the default settings file, every key's default, type, and range, unknown keys.
  - `server.rs`: building, the thread budget, running until shutdown. `world.rs`: the world API (positions, errors, a generator written outside Anchor).
  - `shutdown.rs`: a normal stop by the console's `stop` (and on Unix by SIGINT, SIGTERM, or SIGHUP) commits what the last tick changed.
  - `crash.rs`: repeated forced kills never damage the data files, and the block exists exactly once.
  - `replay.rs`: a crash log replays to the same panic.
  - `import.rs`: importing a world the official server made.
  - `export.rs`: an imported world, exported and imported again.
  - `log.rs`: the console log and turning it off.
- xtask integration tests (`xtask/tests/`, run by `cargo test --workspace`):
  - `protocol.rs`, `compression.rs`: conformance with gophertunnel, P1 to P4 (`.claude/docs/protocol.md`).
  - `spawn.rs`: P5, the bot joining Anchor in the same process (`.claude/docs/protocol.md`).
  - `examples.rs`: each example binary runs in a fresh folder, and the bot plays the script `xtask/data/examples/<name>.txt`.
  - `go.rs`: the Go tests of `xtask/go`. `import_reader.rs`: goleveldb against the world import reader.
  - `downstream.rs`, `dimpl_repro.rs`, `redb_repro.rs`: a user project gets the vendor copies, and reproductions of the fixed defects (`.claude/docs/dependencies.md`).
- Running a measurement test, for example the tick cost: `cargo test --release --lib tick::tests::bench -- --ignored --nocapture`.

## Idempotency, determinism, conservation, and leak tests

- Idempotency, for example: `the_same_request_twice_leaves_the_state_of_once` (inventory, `src/tick/inventory/tests.rs`), `a_repeated_break_changes_nothing_twice` (`src/tick/interact/tests.rs`), `the_same_answer_twice_changes_nothing_twice` (forms, `src/tick/form/tests.rs`), `a_duplicate_join_changes_nothing` (`src/tick.rs`).
- Determinism: a scripted client (`src/tick/tests/client.rs`) sends game inputs from fixed seeds. A CPU pool of 1 thread and of max(logical CPUs, 4) threads must give the same per-tick state hashes and output and the same input log bytes, and replaying the log gives the same ticks (`one_and_many_pool_threads_give_the_same_ticks_and_the_log_replays_them` in `src/tick.rs`; with listeners, `src/tick/tests/busy.rs`).
- Conservation: fixed-seed random inventory requests change item totals only as far as each request explains (`random_requests_keep_every_item_accounted_for` in `src/tick/inventory/tests.rs`). The forced-kill tests (`tests/crash.rs`, `src/tick/tests/crash.rs`, `src/tick/task/crash.rs`) kill the server at moments counted in ticks from its commit reports, never in wall-clock time, and check that nothing is duplicated or lost.
- Leaks: a player joins, opens and closes every window, is sent a form, places and breaks blocks with block entities, makes and loses entities, trades, and leaves, 100 times; every table and queue the test lists is back at its start value (`src/tick/tests/leak.rs`).

## P6: comparison with the official server

P6 runs the same bot scenarios against the official server and Anchor and compares the packets the bot receives.

- The bot: `xtask/go/bot`, a Go client built on gophertunnel. Like the official client, it uses server-authoritative inventories, PlayerAuthInput, and block actions, and it records every received packet as a JSON line.
- The official server: the version pinned as `OFFICIAL_DIR` in `xtask/src/p6.rs`, in the copy `.cache/official/<version>/` (sources in `.claude/docs/data.md`). Before each run the harness writes the copy's `server.properties` and deletes its `worlds` folder; it starts the server and ends it with `stop`.
- Test settings: both servers run without Xbox login (`online-mode=false`, Anchor `xbox-auth = false`), with a flat world named `p6` and seed 1. Game mode, difficulty, maximum players, view distance, and compression threshold are Anchor's defaults, so fields that depend on settings compare equal. The API session adds `allow-cheats=true` on the official server.
- Captures: all under `target/p6/`: `official-<compression>/`, `official-nethernet-<compression>/`, `official-fallback/`, `official-bind-mismatch/`, `official-duplicate/`, `official-game/`, `official-api/`, the matching `anchor-*` folders, and `import-*` and `export-*` for `p6 import`. The game and API comparisons are also written to `anchor-game/compare.txt` and `anchor-api/compare.txt`.

### Running on Linux

- The official server for Linux is its Linux build (source in `.claude/docs/data.md`), unpacked into `.cache/official/<version>/` of the Linux checkout. The folder name is the same as on Windows; each OS keeps its own `.cache`. Unpacking with `unzip` keeps the execute permission of `bedrock_server`.
- The harness runs the executable of its OS (`bedrock_server.exe` on Windows, `bedrock_server` on Linux) with `LD_LIBRARY_PATH` set to the server folder, as the official server's `bedrock_server_how_to.html` starts it.
- Needs Go on `PATH` and `unzip`. The official server's how-to supports Ubuntu 18 or later only.
- On a Windows PC, copy the work tree, without `target/` and `.cache/`, into WSL2 Ubuntu's Linux file system (ext4), and build and test there. Never run from `/mnt/c`: its file system behavior and fsync differ from real Linux, so nothing is verified there. Edit only the Windows repository; the copy is only for testing.
- aarch64 (`aarch64-unknown-linux-gnu`) is checked by cross-building only (`gcc-aarch64-linux-gnu`, `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc`).

### Commands

| Command | What it does |
|---|---|
| `cargo xtask p6 official [zlib\|snappy]...` | For each compression (default both, zlib first), starts the official server once over RakNet and once over NetherNet. RakNet: `spawn`, `big-skin`, and `blocks`. NetherNet: `spawn` and `big-skin`, plus `big-skin` sent in 65,535-byte segments. Once, in the first compression's runs: the `auto` fallback, an offer identity mismatch, and a duplicate-name login. Last, the game session and the API session once each |
| `cargo xtask p6 anchor [zlib\|snappy]...` | Runs the same scenarios against Anchor in this process and compares each capture with its official server capture; the fallback run advertises an unreachable IPv6 address and uses the first compression. Then runs and compares the game and API sessions |
| `cargo xtask p6 game official\|anchor [scenario]...` | The game session alone (RakNet, zlib, a fresh flat world) |
| `cargo xtask p6 api official\|anchor [scenario]...` | The API session alone (RakNet, zlib, a fresh flat world; on the official server `allow-cheats=true` and the forms script pack) |
| `cargo xtask p6 import` | Imports a world the official server made into Anchor and compares the blocks both servers send, block by block. Then exports that world, opens the export on the official server, and compares its blocks with the original world's |
| `cargo xtask p6 run --addr HOST:PORT --out DIR [--transport raknet\|nethernet\|auto] [--token-key PEM --kid KID --iss ISS]` | Runs the scenarios against a server already running; the token options add the online scenario |
| `cargo xtask p6 compare A B`, `p6 game compare DIR_A DIR_B`, `p6 api compare DIR_A DIR_B` | Compares two captures |

`p6 anchor` compares its fallback capture with `official-fallback/`, which `p6 official` records in its `FALLBACK_COMPRESSION` (zlib) run; `p6 official snappy` alone records none, and `p6 anchor` then fails with "no capture".

### Scenarios

- Spawn (`p6 official`, `p6 anchor`):
  - `spawn`: the spawn window from login to PlayStatus(PlayerSpawn). Compares the packet order and kinds, the fixed fields of `fields.txt`, the full item packets, and the chunk and sub-chunk answers.
  - `big-skin`: a large Login request.
  - `blocks` (RakNet runs only): console commands place fences, glass panes, iron and copper bars, walls, a fence gate, stairs, and tripwire next to each other and remove some again; the block update states of each step are compared.
  - The `auto` fallback, the offer identity mismatch (`bind-mismatch`), and the duplicate-name login. NetherNet runs also compare the answer SDP structure, and print each capture's segment facts. `bot worlddata` checks each spawn capture's biome definitions and jigsaw data against the reference list source's data.
- Game (`p6 game`): `trade`. Per bot step it compares the commands, request statuses, ItemStackResponse slots (stack IDs numbered in order of appearance), the items the client sees at the end of the step, the last block state of each position, window opens and closes, block entity keys, LevelEvent types, entities by type, and the trade window shape and offer keys. Random values (offers, XP orbs), timing, sounds, chat, and `Event` types are only shown. A `setup` step compares only its commands; a run of `random` steps compares only the shapes of its lines, with names and counts blanked. Changes farther than `AREA_REACH` (4 blocks) from the scenario's area are only shown (`xtask/src/p6/game/trace.rs`). The bot's `check` lines are compared only when their name is in `FIXED_CHECKS` (the `offers` checks by shape), and every `Disconnect` and every packet that failed to decode is compared.
- API (`p6 api`):
  - `commands` (player commands and their output, op), `particles`, `kick`, `forms`, `script` (a self-test of the bot's `-script` actions, `script.txt`).
  - `weather`, `joinrain`, `joinstorm` (weather changes, joining during rain or thunder), `boltpeaceful`, `bolteasy`, `bolthard` (lightning by difficulty).
  - `potions` (potion colors and the cloud), `entities` (painting, firework, end crystal, trident), `syntax` (syntax errors and translated names), `tradeup` (trade tier up), `others` with the companion bot `mover` (another player's join, movement, attack, death, respawn), `itemuse` (bow, trident, and potion use), `sneak` (the input tick of a sneak's answer).
  - `clocks` (every form of `/time`: the world clock and its time markers, the answers and the clock times sent).
  - `villagers` (profession numbers), `numbers` (numbers only the official server uses, and telemetry), `rules`, `rulesjoin` (game rules), `line<bytes>`, `lineunknown600`, `linekorean<bytes>` (command line lengths; the official server ends the connection from 508 bytes), `limits`, `limitchat` (string limits).
  - In every step that is not a setup step, it compares the number and fixed fields of the packets `fields.txt` names, and the bot's output, check, and form lines. Command lists are compared only for the commands both servers have, and only as notes. Facts-only scenarios are in `FACTS_ONLY`, and scenarios that compare check lines without fixed fields are in `CHECKS_ONLY` (`xtask/src/p6/api.rs`).
- Import (`p6 import`): in a world with the official server's own terrain, every block of every sub-chunk both servers send, in every layer, must have the same network ID, and a sub-chunk answered all-air holds air. A block the dump lacks, which Anchor keeps as an unknown block, passes through an `import | block <name> | <reason>` line of `intended.txt`, as does a block whose stored state Anchor sends where the official server updated it after loading. The export writes Anchor's imported world with `Server::export_world` (`target/p6/export-world/`); the official server opens it with the same game rules, and the blocks it sends (`export-official/`) are compared with those it sent of the original world, by the same rules.

### Data files (`xtask/data/p6/`)

- `facts.md`: the values the official server sent and what Anchor does about each. Code comments point to it.
- `fields.txt`: the fixed fields both servers must send equal (`<packet> <field path>`).
- `deferred.txt`: packets the official server sends in the spawn window and Anchor never sends (`<packet> <status> <reason>`, status `-`). The spawn comparison removes them from both captures.
- `intended.txt`: differences Anchor keeps on purpose (`<scenario> | <difference text> | <reason>`), read by the API comparison and `p6 import`. A difference passes only when its text matches a line exactly; a listed line the comparison no longer finds is printed as a note.
- `script.txt`: the actions of the `script` scenario. `forms-pack/`: the forms script pack for the official server.

## Checks only the real client can do

The bot and the official server cannot show the client's screen or the values the official client sends. The user's checklist, to go through by joining with the official client, is the real-client section at the end of `xtask/data/p6/facts.md` plus these items:

- Network: the remaining risks listed in `network.md`, and how resource pack chunks are requested.
- Spawn: when PlayStatus(PlayerSpawn) is sent (Anchor sends it after the chunks of the `spawn-radius` circle), the chunk publisher update sent only when the player's block changes (not every tick, no repeats of the same value), the BiomeDefinitionList order.
- Creative inventory: how education edition items show in creative, and the effect of `deprecated` block names.
- Windows and worlds: the 2-tick window open delay, the pairing display of double chests, old chunks and the difficulty after a teleport to another world in the same dimension.
- Custom content: custom block rendering, friction, the `"none"` category, property limits, custom entity `rid` and `hasspawnegg`.
- Screen: boss bar color updates.

The open questions are at the end of `xtask/data/p6/facts.md`.

## Known flakiness on this PC

- Loopback TCP: on this PC a loopback TCP connection sometimes times out after 1 second (23 of 300 Go connections, without Anchor). Tests that use NetherNet signaling (HTTP) and the NetherNet runs of `p6 anchor` fail now and then because of it.
- Right after a teleport, MovePlayer mode 2 and the on-ground flag vary with the input's arrival time on both servers, so P6 does not compare them: `fields.txt` lists no MovePlayer field, and the game trace only counts MovePlayer.

## Running heavy tests

A full `verify`, P6, and the determinism and forced-kill tests use much CPU and memory. Under heavy load the editor and other processes can stall.

- Low priority on Windows: `start /low /b /wait <command>` from `cmd`, or `cmd //c "start /low /b /wait <command>"` from Git Bash. Child processes (rustc, test binaries) inherit the priority. On Linux: `nice -n 19 <command>`.
- Bounding a run: build parallelism with `CARGO_BUILD_JOBS`, test parallelism with `-- --test-threads=N`, a name filter such as `cargo test --lib tick::tests::crash`, and scenario names for P6.
- In the game and API sessions each bot has a time limit (`BOT_TIMEOUT`, 150 seconds) and the harness kills it 30 seconds after that (`xtask/src/p6/game.rs`).
- A measurement test that overlaps another build gives unstable values.
