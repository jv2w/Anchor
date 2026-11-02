---
paths:
  - "src/**/*.rs"
  - "tests/**/*.rs"
  - "xtask/src/**/*.rs"
  - "xtask/data/dump/status.txt"
  - "Cargo.toml"
  - "xtask/Cargo.toml"
  - ".clippy.toml"
---
# Architecture rules

## Repository and modules
- Place every module in its area (the table in `.claude/docs/architecture.md` lists the submodules):
  - transport: `udp`, `raknet`, `nethernet`;
  - session: `session`, `login`, `auth`, `crypto`;
  - protocol: `protocol`;
  - tick rules: under `tick/`;
  - world: `world`;
  - storage: `save`, `storage`, `import`, `export`, `replay`;
  - block and item data: `block`, `item`, `effect`, `nbt`;
  - public API: `server` (re-exported at the crate root), `player`, `entity`, `event`, `command`, `form`, and the other modules reachable from the crate root;
  - foundation: `runtime`, `pool`, `clock`, `random`, `bucket`, `console`, `fs`, `error`, `args`.
- Keep tick code and off-tick code in separate modules.
- Put development tools in `xtask/` and patched dependency copies in `vendor/`. Put development-tool dependencies in `xtask`, never in the `anchor` library.
- Name every tool configuration file with a leading dot where the tool accepts one.
- Put downloaded external sources and tools in `.cache/`.
- Create threads only in `runtime`, read the system clocks only in `clock`, and call rayon's parallel APIs only in `pool`.

## Threads
- The tick thread never touches the disk.
- Never leave tokio at its default of 512 blocking threads; `runtime` sets the bound.
- Give every thread the same stack size, 4 MiB.
- On a clean shutdown, wait until every CPU pool thread has ended.
- The save thread uses the CPU pool only through `Pool::map_in_turns`.

## Tick
- Wait between ticks with `std::thread::sleep`, through `clock::sleep_until`.
- Never change the order of a tick: input → rules → sync → output, then the boundary after the output is delivered.
- Put every input the tick applies into the input log **before** applying it.
- Compute heavy rules (entity physics) read-only on the CPU pool, then apply the results serially.
- Send hooks and the StartGame hook run in parallel in the output phase: keep them `Fn + Sync`, so they cannot reach game state.
- Give a world generator (`Generator`) only the seed, the chunk position, and the empty chunk.

## Determinism
- Make the key of `Random::new` with `random::key` from a purpose `random::tag` and the position, entity, or session it is for. Never open the same key twice in one tick.
- Number sessions only in the accept task, in connection order, and never reuse a number.
- Keep the floats of tick state as `f64`; convert to `f32` only at the protocol boundary.
- In `math`, use only the operations whose results IEEE 754 fixes: the four arithmetic operations, `sqrt`, `floor`, `ceil`, `round`, `abs`.
- Use the `min`, `max`, `clamp`, and trigonometric functions of `math`, never std's (`.clippy.toml` forbids std's).
- Check that every coordinate a client sends is finite and inside the world border (`world::BORDER`).
- An event's float setter ignores a value that is not finite and keeps the field inside its valid range.
- Keep every iteration order deterministic (`BTreeMap`, `Id` order).
- Keep the input sort key, (session, sequence number), unique per input.
- The state hash and the checkpoint (`tick/checkpoint.rs`) destructure every struct without `..`. Give every field left out a comment with the reason.

## Input log and replay
- Never encode packets on the tick: the log encodes them when it is written, on the main thread.
- A replay creates no runtime and no network. It opens data files as `.claude/rules/storage.md` says for replays.
- A replay never runs blocking jobs again; it hands their logged results to their callbacks.
- Never replay from a checkpoint that holds state without a checkpoint form (a closure such as a scheduled task, a waiting form or job, an open menu, or a deferred callback; a player extension; a held offline player): refuse it with `replay::Error::Opaque`. Never guess.

## Public API
- Only `Server` owns game state; callbacks borrow it through `Context`.
- Keep nothing across ticks but `Copy` IDs that point at nothing else: numbers never reused (`player::Id`), indexes with a generation (`entity::Id`), or a world's place, which only a world made again under its name takes (`world::Id`).
- Give state access this shape: `cx.player(id) -> Option<&Player>` to read, `cx.player_mut(id)` to change.
- Keep the fields of tick-state structs such as `Player` and `Entity` private.
- Lend an entity, whose position change must also update an index, through the guard `entity::Mut`.
- Make positions, block states, and IDs `Copy` values, and `item::Stack` an immutable value.
- A direct API call (a setter, `teleport`, `kick`) changes the value at once. Mark what other players must see and send it at the tick's fixed point.

## Events
- Raise every event with the flow of `tick/dispatch.rs`: check → build the event → listeners → check again → apply → monitors.
- Treat a second check that fails because a listener changed the state as a cancel.
- Without listeners and monitors, build no event and check nothing again. Check again only when a listener, not only a monitor, is registered.
- Build events as stack values.
- Run user code that is no event of an action (sound and particle listeners, custom block hooks) only after the action is applied, the held item and drops included.
- No listener and an empty listener give the same state hash, output, and log bytes.
- Handle what a direct API call causes (`kick` → the player quits) at the tick's fixed point, and raise its events there.
- Raise EntitySpawn and ItemSpawn in the entity tick.
- Keep event fields `pub(crate)`; make public only getters, setters, and `cancel`, `uncancel`, `is_cancelled`.
- Merge an API-table event's subclass hierarchy into one struct with a cause enum (`EntityDamageByEntity` → a `DamageSource` of `EntityDamage`).
- Ignore a cancel of PlayerDataSave.
- Give no change event to a value only the program's own direct calls change, such as the weather.

## Extension points
- Carry custom block and item definitions as `&'static` references in the values. Keep no global registry.
- Never turn the block state into 4 bytes plus a server table.
- Add no API that registers custom status effects or enchantments.
- Add a user storage provider trait only when a need arises that it can meet while keeping the atomic commit.

## Using admitted crates
- Never use rayon's global entry points (`spawn` and the others the deny list names); the pool comes from `runtime` and its parallel APIs from `pool`. The one exception is the `Scope::spawn` that `Pool::map_in_turns` calls inside the pool's own `in_place_scope`.
- Use aws-lc-rs randomness only in the `crypto` module.
- Always give rustls the aws-lc-rs provider explicitly (`builder_with_provider`).
- Use tokio-rustls only inside the signaling's 10-second request deadline.
- Use only `message()` and `span()` of a toml error.
- Read untrusted JSON into a typed struct, after checking its length against its bound.
- Call memory-stats only on a blocking thread.
- Use redb's open calls, `begin_read`, `begin_write`, `check_integrity`, `compact`, and `commit` only in the `storage` module.
- Set str0m's crypto provider, ICE credentials, and certificate explicitly.

## API table
- When an alias's short class name is ambiguous, write its namespace path instead (`block\tile\Chest::getPair`).
- When rustdoc rejects an alias equal to the item's own name, cover the class row with a `// api-class: Name` comment above the item.
- Cover the aliases a test checks with a `// api: A::b, C::d` comment above the test function.

## Villager trading
- Send prices as the real counts. Never rely on the client's price formula.
- Process a trade request ID once, accept only offer IDs of the list as it is now, and check the payments and the uses left.
- Repeat a trade at most 256 times per request. Apply a trade as one unit; on failure undo all of it.
- Return the payment slots' items when the window closes or the villager is gone.
- Never drop a saved offer Anchor cannot use; write it back as it was.

## Multiple worlds
- Never unload the default world, not even by force.

Background and detailed facts are in `.claude/docs/architecture.md`.
