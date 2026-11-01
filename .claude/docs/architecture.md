# Architecture

Work rules are in `.claude/rules/architecture.md`.

This document records where the code lives, which thread does what, how a tick runs, and where users extend the server.

```rust
use anchor::{command::Command, event::Priority};

fn main() -> anchor::Result<()> {
    anchor::Server::builder()
        .config(anchor::Config::default().load("anchor.toml")?)
        .on(Priority::Normal, on_player_join)
        .command(Command::new("spawn", "..").overload(&[], spawn))
        .build()?
        .run()
}
```

Related documents: `network.md` (transports and login), `protocol.md` (generated code), `storage.md` (data files and replay files), `data.md` (block and item data), `config.md` (settings keys), `testing.md` (verification), `dependencies.md` (external dependencies).

## Repository layout

```
CLAUDE.md                   rules
Cargo.toml, Cargo.lock      the anchor package, the workspace, and the canonical lints
rust-toolchain.toml         the pinned toolchain
.rustfmt.toml, .clippy.toml, .cargo/config.toml, .gitattributes, .gitignore
README.md, LICENSE-APACHE, LICENSE-MIT, THIRD-PARTY-NOTICES.md
src/                        the library
tests/                      public API tests (<topic>.rs)
examples/                   lobby, pvp, custom, minigame (tested by bot scenarios)
xtask/                      development tools (check, verify, gen, dump, table, vendor-test, p6);
                            go/: extractors, bots, comparison tools; php/: dump scripts;
                            data/: the IR, dumps, reference lists, P6 tables
vendor/                     patched dependency copies (dependencies.md)
.claude/rules/, .claude/docs/   the rules and these documents
```

- The workspace has two members, `anchor` and `xtask`. `vendor/` is excluded from it.
- Every server program answers three commands from its process arguments without code of its own (`args`): `update-config` (`config.md`), `import <world folder> [<world name>]`, and `export <new folder> [<world name>]` (`storage.md`). `Config::load` does `update-config` and `Server::run` does the other two; neither then starts the server. Any other argument is the program's own and starts the server as usual.
- `rust-toolchain.toml` and `Cargo.lock` cannot take a dot name.
- `.cache/` is ignored by git.

## Module placement

| Area | Modules | What they do |
|---|---|---|
| Transport | `udp`, `raknet` (`listener`, `peer`, `wire`), `nethernet` (`signaling`, `peer`, `sdp`, `identity`, `message`) | The UDP port both transports share and its task, RakNet over UDP, NetherNet (WebRTC) and its HTTP signaling |
| Session | `session` (`connection`, `transport`, `packs`), `login`, `auth`, `crypto` | Batch decoding, compression, encryption, the login sequence, resource pack download, relaying to and from the tick queues |
| Protocol | `protocol` (generated `packet`, `types`, `consts`; hand-written `io`, `batch`, `compression`, `common`, `inventory`, `entity_data`, `flags`, `login`, `union_frame`, `values`, `block_storage`) | Packet codecs, batches, compression |
| Tick rules | under `tick`: `block_entity`, `chat`, `checkpoint`, `command`, `dispatch`, `emit`, `entity`, `extra`, `form`, `hook`, `interact`, `inventory`, `log`, `network`, `player`, `queue`, `rules`, `save`, `spawn`, `survival`, `task`, `trade`, `update`, `view`, `weather`, `world_api` | The game state `State` and the only code that changes it |
| World | `world` (`chunk`, `sub_chunk`, `paletted`, `light`, `generator`, `connect`, `tree`, `biome`, `index`, `particle`, `sound`) | Chunks, palettes, light, generators, neighbour connection properties |
| Storage | `save` (the save thread), `storage` (one redb file), `import` (importing a LevelDB world), `export` (exporting to a LevelDB world), `replay` | Snapshot commits, loads, backups, replays |
| Block and item data | `block`, `item`, `crafting`, `effect`, `nbt` | Tables generated from the dump and their value types |
| Public API | `server`, `player`, `entity`, `world`, `event`, `action`, `command`, `permission`, `form`, `display`, `menu`, `inventory`, `tile`, `custom`, `task`, `data`, `pack`, `config`, `math`, `protocol`, `replay`, `import`, `export` | The types users see. The crate root re-exports `Config`, `Context`, `Error`, `Result`, `SentPacket`, `Server`, `ServerBuilder`, `ShutdownHandle` |
| Foundation | `runtime`, `pool`, `clock`, `random`, `bucket`, `console`, `fs`, `error`, `args` | Thread creation, the CPU pool, the clocks and tick pacing, randomness, token buckets (ping and input budgets), the console and its log, bounded whole-file reads and atomic small-file writes, the crate error, process-argument commands |

- The generated files are listed in `protocol.md` (codecs) and `data.md` (block and item data).

## Threads and queues

| Thread | Count | What it does |
|---|---|---|
| Tick (`anchor-tick`) | 1 | The only thread that changes game state |
| Save (`anchor-save`) | 1 | Chunk and player loads, snapshot commits, backups |
| Network workers (tokio, `anchor-network`) | `network-threads` | Transports, sessions, codecs, compression, encryption, signaling |
| CPU pool (rayon, `anchor-cpu-<n>`) | `cpu-threads` | The tick's parallel read-only phases; for the save thread, chunk decoding, generation and first lighting, chunk extras reading, chunk sealing, and the file checks at start |
| Blocking pool (tokio) | at most 8 | 4 for the engine (the authorization fetch, pack and key loading), 4 for user `spawn_blocking` jobs |
| Console (`anchor-console`) | 1 | Puts standard input lines into a bounded queue (`console-input`) |
| Console log (`anchor-log`) | 1 | Anchor's default console writer, writing standard error (when `log-level` is not `off` and `without_console_log` was not called) |

- How `"auto"` resolves the thread counts, and how settings over the compute-thread budget are refused, is in `config.md` (`network-threads`, `cpu-threads`).
- Blocking threads wait on I/O, so they do not count against the budget. Neither do the console threads.
- The 4 MiB stack holds the recursion of dropping NBT nested 512 deep, whatever the OS default or `RUST_MIN_STACK`.
- Main queue bounds:

| Queue | Bound | When full |
|---|---|---|
| Session → tick input | 1,024 per session; the queue is sized at start so every share fits (`network.md`); the tick applies at most 8,192 a tick | Only the session that used up its share disconnects |
| Tick → session outbound | 1,024 messages | The tick does not wait; it drops that session |
| Tick → save requests / answers | 64 / 2,048 | Requests wait for the next tick; the tick applies at most 1,024 answers a tick, the rest in the next tick |
| Player outbox (packets the serial phases queue) | 4,096 a tick | That player is disconnected (a runaway rule or callback) |
| Block positions refused changes send again | 4,096 per player a tick | That player is disconnected (a runaway client) |
| `defer` | 4,096 waiting (16,384 runs a tick) | `Err` |
| Scheduled tasks / blocking jobs / job result | 65,536 / 1,024 / 1 MiB | `Err` |
| Controls to the networking (server name, IP blocks) | 256 a tick | The call changes nothing |

- rayon's drop does not wait for its threads, so a clean shutdown counts the CPU pool threads ending through the exit handler (`Pool::shutdown`).
- The save thread hands the CPU pool its work in turns of as many items as there are threads, and runs each turn's first item itself (`Pool::map_in_turns`). So at most threads − 1 pool threads hold save work, and one is left for the tick's parallel phases. rayon's workers take work from outside the pool only when there is nothing to steal, so if every worker held save work, a tick's parallel phase would wait behind the whole turn. When a turn starts during a tick's parallel phase, the pool thread that runs a tick's map, waiting in a join, can still pick up an unclaimed save item, so the tick may wait for one item. Turns cost save throughput when items differ in cost. This shortens only the tick's wait, not its work, and changes nothing when `cpu-threads` is 1.

## Tick

- The tick rate is fixed (`Context::TICKS_PER_SECOND`; a tick lasts `tick::TICK`) on the engine clock. A tick that ends late starts the next one at once, which catches up; when the schedule falls a whole period or more behind, the missed ticks are dropped with a warning (`clock::Pacer`).
- On Windows, Rust's `sleep` (toolchain 1.98.1) uses a high-resolution waitable timer (Windows 10 1803 and later), which measured a real tick interval p99 of about 50.5 ms. Waiting in steps of the default 15.625 ms timer jitters between 46.9 and 62.5 ms, and the movement of other players stutters. tokio's `sleep_until` is coarser, and spin waiting burns a CPU.
- The tick function is `step(state, inputs, responses, pool, output, log)`, followed by the delivery and `boundary`.

Phases:
1. **Input (serial)**: the save thread's answers (loaded chunks and player data, commit results) in arrival order; in a run's first tick, `ServerStart`; then the session inputs sorted by (session, sequence number), each followed by the callbacks it deferred. Receive hooks run here. Then the scheduled tasks.
2. **Rules**: survival and the world clocks → breaking in progress → block updates (random, scheduled, neighbour) → entities (physics computed read-only on the CPU pool, then applied in entity order) → trades with vanished villagers → block entities → light (a budget per world).
3. **Sync (serial)**: the deferred callbacks, command sync, `player::sync` (the changes the direct API marked), the players whose outbox overflowed this tick, the views (each follows its player, picks the chunks to send, asks for loads and unloads them, and loads and unloads worlds), then the block entities changed this tick, each sent once with its last value.
4. **Output (parallel, read-only)**: the sub-chunks the answers need, each encoded once, then the packets of every marked player, built on the CPU pool in session order. Send hooks and StartGame hooks run here. The result does not depend on the thread count.
5. **Boundary**: after the delivery, the callbacks the quits of the sessions dropped there deferred, a save snapshot for the save thread if one is due, the offline player loads the tick asked for, then the tick's controls (server name, IP blocks) to the networking.

- Wherever the tick sends a player off (a kick, a transfer, a duplicate login, damaged saved data, a shutdown), the player's outbox goes through the send hooks and out before the last packet (`Disconnect`, `Transfer`). So what was sent right before a kick, or by a PlayerQuit listener, arrives. Only an overflowed outbox is not sent, as its end was cut.
- `PlayerAuthInput` applies in this order: movement → block actions and item interaction → its item stack request. Every other packet goes to the rule module that owns it.
- A refused click or break marks the blocks around it, and a refused sign edit marks that block, instead of sending them on the spot. The output phase sends each marked position once with its block at the end of the tick (the fake block if there is one), and no separate block change for that position. Sent on the spot, they would go out in the outbox after the block changes, so the client would undo a block a later input or rule of the same tick had changed.
- Who sees a chunk is an index the views keep as they send and drop chunks (`view::Chunks`), so a broadcast, the tracking of entities and players, and the packets about a player ask it instead of every player. The players a projectile, potion, cloud, explosion, falling anvil, firework or lightning bolt reaches are looked up, during the entity tick, in an index by chunk it builds once (one sort of the players); outside it, or after a player was teleported during it (an ender pearl), every player is passed over. Both give the same players, in the same order, as a pass over every player, which tests check.
- Chunk radius requests in one tick share one answer and one resize, with the last radius. An open view follows the granted radius in the view phase: nothing bounds how many requests a client sends, and rebuilding the whole view disc per request would let one client hold the tick.
- The chunk load bounds (1,024 in flight, 256 asked a tick) are shared by every view, and the first view to ask takes the slots freed. So the view phase starts each tick one player further on (player tick % player count, in session order, wrapping around). Starting from the same session every tick would let the first sessions take every freed slot, and when the save thread is slow the later sessions would not get their spawn chunks before the spawn deadline (`network.md`).
- With `slow-empty-worlds`, a world without players runs its world rules on one tick in 100, world `w` on the ticks `t` where (t + w) % 100 = 0, so empty worlds do not all run on one tick.

## Determinism

- **Time**: every measured value the tick uses arrives as a logged input `Measure`: the tick time on the engine's own session, the connection counts and the process values (memory, uptime) on sessions of their own, a ping on its player's session. The join time is part of the join identity. Ban durations are engine ticks.
- **Randomness**: `random` is counter-based. `Random::new(seed, tick, key)` starts SplitMix64 from a mix of the three and keeps no state, so restarts, replays, and parallel phases give the same numbers. The command selector `@r` and the painting choice use it too.
- **IDs**: an entity `Id` is an index and a generation; runtime IDs, stack network IDs, and form IDs come from engine counters. Custom content numbers follow registration order.
- **Floats**: `math` gives deterministic `min`, `max`, `clamp` (`b` on a tie, so ±0 is decided) and polynomial `sin`, `cos`, `atan2` (fdlibm coefficients). The sky light time correction is a table instead of `cos`.
- **Parallel phases**: `Pool::map` returns results in input order.
- **Order**: the input sort key is unique per input, so the unstable sort is deterministic.
- **State hash**: structs are destructured without `..`, so a new field that is left out fails to compile.

## Input log and replay

- The log holds every input the tick applies: join identities, decoded packets (their sizes count against the bound only and are not kept), leaves, console lines, job results, measured values, save thread answers, and the sessions dropped because their outbound queue was full. An input enters the log before it applies, so the log of a tick that panicked holds the input that did.
- The log is a list of segments. One starts with the process and one with each save snapshot, each headed by its start tick, its commit identity, and a checkpoint of that state. The log head holds the SHA-256 fingerprints of the tick settings and worlds and of the registrations (commands, custom items, blocks and entities, listeners per event, task periods, data types, hooks).
- The bound is 64 MiB of counted bytes: each input counts its wire size, or for a packet the larger of that and what decoding it allocated (item NBT allocates far more than its bytes), and every list the log holds counts the room it reserves (ticks, inputs, answers and forgotten sessions at their own size, the room doubling as a list grows), checkpoints the room of their body, so the bound counts the memory the log holds, idle ticks included; reading a log back, all packets share one 64 MiB decode budget. Recording stops when the inputs alone would pass the bound; committed segments are dropped oldest first when the inputs and the checkpoints of every kept segment would, never the newest committed one. Checkpoints have no bound of their own. A quarter of the bound recorded since the last commit asks for a snapshot even with automatic saving off, so the other three quarters cover the commit's time. A segment whose recording stops, at the bound or at a save thread answer that cannot be logged, is marked truncated, and a replay ends there.
- Commit identity: the log chains every input the tick applies (its place, its kind, and the measured values, not packet bodies) into an identity that each commit writes into the files with its tick. So a replay never takes a file another run committed at the same tick.
- When the tick thread panics or a save fails, the log is written to `data/crash/input-<unix seconds>-<last tick>.log`: never over an existing file, at most 1 GiB (from the newest committed segment on if the whole log is larger, or not at all).
- Replay: `Server::replay(log) -> replay::Result<Replay>`, on a server built with the settings, worlds, and registrations of the run that wrote the log (otherwise `SettingsDiffer`, `RegistrationsDiffer`). It runs the same `step` from the committed files and a checkpoint and compares the checkpoints of later segments (`Diverged` when they differ). It ends with `End::Completed`, `End::Truncated`, or `End::Panicked`.
  - State kept in closures starts over, and what user code does outside the server (files, network) happens again.
  - Which data files and backups a replay picks is in `storage.md`.

## Panic policy

| Where | What happens |
|---|---|
| Connection task (session, codec) | Only that connection closes. A panic in str0m closes only its NetherNet peer (`network.md`) |
| Tick thread | It ends without `catch_unwind`. A drop guard sees `thread::panicking()` and hands the input log to the main thread; `run` writes the crash log and returns `Error::TickPanicked`. Nothing more is committed: the save thread commits no snapshot left without `Finish` |
| Save thread | The tick sees the answer queue closed and stops without saving; `run` writes the crash log and returns `Error::Storage` |
| User `spawn_blocking` body | Its done handler gets `task::Error::Panicked` |
| CPU pool job | The panic reaches the caller |

- The retries and the stop when a commit keeps failing are in `storage.md`.

## Public API structure

- Accessors of one shape: `entity`/`entity_mut`, `world`/`world_mut`, `player`/`player_mut`, `worlds()`, `players()` (in `Id` order).
- `Player` and `Entity` are the tick-state structs themselves.
- The message kinds (chat, tip, popup, title, action bar, toast, and the others of `player::Message`) are one call, `Player::send(Message)`. A raw packet is `Player::send_packet`.
- Forms, scoreboards, boss bars, fake blocks, and menus are `Context` methods: their handlers take `&mut Context`, and their tables are in `State`.

## Events

- Registration: `ServerBuilder::on::<E>(Priority, |e: &mut E, ctx: &mut Context| ..)` and `ServerBuilder::monitor::<E>(|e: &E, ctx: &Context| ..)`. There are five priorities, `Lowest` to `Highest`. A listener that only watches registers with `monitor`, read-only by its types, and sees the state after the outcome is applied.
- Why the second check: applying without looking again at the state a listener changed would duplicate or lose items.
- Every listener also sees cancelled events and filters them with `is_cancelled`.
- **No re-entry**: while listeners run, the event's listener list is lent out of the state (what is registered stays readable), and nothing calls it then. An entity a listener makes is announced by the next announcement, a spawn listener's included.
  - `ctx.defer(f)`: runs after the running callback returns, in the same tick, in queue order.
  - `ctx.perform(action)`: defers an action that goes through the game rules, events included. Actions are the sealed `action::Action` implementations (`BreakBlock`, `UseItemOn`, `UseItem`, `AttackBlock`, `InteractEntity`, `Damage`, `SetGameMode`, `Chat`, `Kick`, `Transfer`, `Attack`, `Explode`, and others).
- Why the entity tick raises EntitySpawn and ItemSpawn: listeners never run in the middle of another rule or of a direct API call.
- Why user code that is no event of an action runs only after the action is applied: a listener that changes the hand or the block in between would duplicate or lose items.
- There are 104 events, in `event/{block,entity,inventory,player,server,world}.rs`, all public flat as `anchor::event::*`. The API table's events out of scope (plugins, PHP and operations internals, redstone) are left out; the raw packet events (DataPacketReceive, DataPacketSend, DataPacketDecode) are the packet hooks; PlayerPreLogin is merged into PlayerLogin. `TradeOpen` and `Trade` have no API-table counterpart.
- Why a PlayerDataSave cancel is ignored: it keeps the save atomic.

## Extension points

| Need | What to use |
|---|---|
| Change how the game reacts | Events (`on`, `monitor`), `defer`, `perform` |
| See, change, or drop packets | `ServerBuilder::on_receive` (serial input phase, with a `Context`; the log keeps the packet as it arrived, so a replay runs the hooks again) and `on_send` (parallel output phase). Both hooks get a `SentPacket`, copied only when the hook calls `get_mut`: a received packet as the log holds it, a sent one shared by several players for that player alone. The start-game data every player gets the same, encoded once at start, the last packet of a session the server closes, and the login and resource pack sequence skip the hooks. `hook-movement-packets` and `hook-player-input` take frequent packets out of them |
| Change StartGame | `ServerBuilder::start_game` (the engine sets the fields its game state decides back after the hook) |
| Login policy | The PlayerLogin event (refusal reasons as a list of flags), permissions, operators, bans, whitelist |
| Commands | `ServerBuilder::command(Command)` (typed arguments, AvailableCommands completion), `without_command`, `permission`, `permission_default` |
| UI | Forms, menus (fake containers), scoreboards, boss bars, titles, fake blocks |
| Scheduling and blocking work | `ctx.schedule`, `schedule_repeating`, `ServerBuilder::every`, cancel handles, `ctx.spawn_blocking(work, done)` (the result goes into the log as JSON through serde) |
| User data | Per-player Extensions that are not saved (keyed by type, at most 64 types), `ServerBuilder::player_data` and `server_data` (serde types, in the same commit), the inventory and data of a player who is not online through `ctx.hold_offline_player(xuid, name, done)` (64 tickets, `storage.md`) |
| Custom content | `custom_item`, `custom_block` (behaviour trait `custom::Behaviour`), `custom_entity` (`entity::Behaviour`), `recipe`, `creative_inventory`. The engine fills the StartGame properties, ItemRegistry, entity identifiers, and block hash IDs. Textures come from resource packs |
| Resource packs | `ServerBuilder::resource_pack`, the setting `resource-packs` |
| World generation | The `Generator` trait, `ServerBuilder::generator` (registered by name; `flat` and `void` are built in) |
| Settings | `anchor.toml` and `Config` in code (`config.md`) |
| Villager trading | The typed `custom::Offer` and the trade API (below) |

- `block::State` and `item::Type` are `Copy` values that hold either a vanilla table index or the custom definition itself (`&'static`), so the vanilla path reads its table after one branch and custom content needs no registry. A custom block whose state hash collides fails the build.
- The client cannot show new status effects or enchantments.
- There is no user storage provider trait.

## API table

- The rows are the public, non-`@internal` API of the dump source commit pinned in the source table of `data.md`, without what is out of scope. `xtask/php/api.php` reflects it into `xtask/data/pmmp/api.json` (the class-likes and the public methods each declares).
- `xtask/data/pmmp/status.txt` gives the rows Anchor does not implement a status; its header defines the line format, the patterns, and the statuses.
- `cargo xtask table [--check]` writes `xtask/data/pmmp/table.md` (each row's FQN, status, location `file:line`, tests, reason). `verify` checks that it is current and has no blank row. The row counts per status are at the head of `table.md`.
- The broadcast channels are merged into `broadcast` and `send`.
- Defects of the API table's source fixed instead of carried over (main ones): global singletons, a mutable shared `Vector3`, getters that copy and NBT serialized again, string keys, an object allocated per event, AsyncTask serialization, raw packet events, hidden O(n) lookups, re-entrant dispatch, a MONITOR priority enforced only by comments, no second check of the state listeners changed, the wall clock and global randomness in game logic, non-atomic saves, IDs that change with load order, light updates (no bound, no spreading across chunk borders), scheduled updates not saved, an unload that leaves no default world, a furnace that loses results by overwriting, shaped recipes matched without their shape.

## Game scope

Scope = the behaviour of the dump and of the API table ∪ survival systems built from other implementations (anvil, smithing table, loom, beacon payment, copper oxidation, furnace experience) − what is out of scope (`CLAUDE.md`).

Implemented:
- Survival: health, hunger, air and drowning, experience, status effects, damage, combat and knockback, death and respawn, game modes, movement checks, server-authoritative break times.
- Blocks: liquids and buckets, falling blocks, explosions, fire, farming (crops, stems, sugar cane, cactus, cocoa, nether wart, bamboo, vines, berries, farmland, saplings into trees), the random tick kinds (leaf decay, fire and grass spread, ice and snow melting, copper oxidation), neighbour connection properties.
- Workstations: crafting (smithing included), furnaces and campfires, brewing stands, enchanting tables, anvils, smithing tables, stonecutters, looms, beacons, bone meal.
- Entities: dropped items, experience orbs, falling blocks, TNT, projectiles, area effect clouds, paintings, end crystals, fireworks, tridents, NPCs without AI (villager-like, human-like, zombie), custom entities.
- Beds and sleep, server light (never sent to clients; `lighting = "fixed"` calculates none), villager trading by code.
- Weather: each world has a state (clear, rain or snow, thunder) and lightning, changed only through the API (`Context::set_weather`, `strike_lightning`). A cycle is built with a repeating task and `server_data`.
- The built-in command `status` shows the uptime, TPS and tick time, memory, threads, players, and each world's chunk, entity, and block entity counts, in Anchor's own wording.

- The vanilla villager economy that is out of scope means the default trade templates.
- Blocks the dump lacks include the composter, kelp, and the grindstone. In an imported world they are inert blocks that only look as they did: no behaviour, item, placing, or generation (`data.md`).

## Villager trading

The API table has no counterpart. The server sets the trade lists of villager NPCs without AI.

- Spawning: `Context::spawn_villager(world, pos, profession, &[Offer]) -> Result<entity::Id, custom::Error>`. A villager spawned by a spawn egg, a command, or the entity API has an empty list.
- List API: `trade_offers`, `set_trade_offers`, `add_trade_offer`, `replace_trade_offer`, `remove_trade_offer`, `trade_tier`, `trade_experience`, `trade_tier_experience`, `set_trade_tier`, `set_trade_experience`, `set_trade_tier_experience`, `reset_trade_uses`. `custom::check_offers` checks a list in advance by the rules of spawning (at most `custom::MAX_OFFERS` offers). An offer is a typed `custom::Offer` instead of NBT (`new(buy, sell, max_uses)`, `with_second_payment`, `with_any_damage`, `with_tier`, `with_price_multipliers`, `with_demand`, and others).
- Events: `TradeOpen` (the window opens), `Trade` (one trade).
- Bound: a villager's offers take at most 2 MiB in saved form.
- Protocol values (compared with the official server): window type 15 with no `ContainerOpen`; offer NBT keys are the official server's (`Recipes`, `netId`, and others); payment slots 4 and 5 and output 50 (in ItemStackRequest the payment containers 47 and 48, the result from the created output 60:50). `netId`s come from a server counter that starts after the last `CraftingData` recipe; reopening an unchanged list shows the same IDs, and every change of the list gives new ones. After a trade no `UpdateTrade` is sent, not even when the tier rises (as the official server does); every change of the list through the API sends it. Each rewarded trade drops 3 to 6 experience. `Damage` 32767 on a payment means any damage.
- Trade data goes into the same commit as the entity.

## Multiple worlds

There is one dimension, the overworld (y −64 to 319, 24 sub-chunks).

- A world is persistent (file `data/worlds/<name>.redb`) or temporary (never saved). Worlds are declared at build time with `ServerBuilder::world`, `world_settings`, and the `[worlds.<name>]` settings tables.
- Life cycle while running (`Context`): `create_world(name, &Options)`, `load_world(name, &Options)` (a persistent world whose file was in `data/worlds/` at start), `unload_world` (a WorldUnload listener may cancel it), `force_unload_world`, `delete_world` (`storage.md`), `set_default_world`, `is_world_generated`, `is_world_loaded`. The events `WorldInit`, `WorldLoad`, and `WorldUnload` are raised in the view phase.
- Unloading: the world's players go to the default world's spawn, its chunks unload by the usual path, and the next commit writes the world and the players in one snapshot. An unloaded world keeps its `world::Id` and index, so it loads again in the same place; a world made again under the name of an unloaded temporary world or a deleted persistent one takes the same place. A run names at most 1,024 worlds.
- A teleport (`Context::teleport`, `action::Teleport`, ender pearls, respawns, `/tp`) goes only to a loaded world and to a place whose eyes are inside the world border; anything else fails at once, or counts as a cancel where a listener chose it. A teleport waiting for the end of the tick into a world that unloads first goes to the default world's spawn with the world's players.
- A world declared with `ServerBuilder::world` keeps its generator value wherever it is made under its name with its generator's name (the start, `load_world` after an unload, a crash replay); every other world that picks a generator by name gets the first value registered under the name.
- What a world file holds, and where the seed and spawn come from, is in `storage.md`.
- Per-world values: difficulty (`set_world_difficulty`), time stop (`stop_time`, `start_time`; not saved), chunk tickets (`hold_chunks`; bounds `world::MAX_TICKETS`, `MAX_TICKET_CHUNKS`, `MAX_TICKET_RADIUS`), view and sending values (`[worlds.<name>]`), weather.

## Known limits and risks

- What user code wrote to outside systems is not undone by a crash, and a replay does it again.
- A segment whose checkpoint holds closures or Extensions cannot be replayed. Scheduled tasks are gone after a restart.
- A teleport to another world in the same dimension sends no ChangeDimension. Whether the official client keeps old chunks can be checked only with a real connection (`testing.md`).
- Turning the block state into 4 bytes plus a server table would need a global registry or an API change for custom states.
- The stand-in block physics of new 1.26.60 states and the unconnected 1.26.60 connection properties are in `data.md`.
