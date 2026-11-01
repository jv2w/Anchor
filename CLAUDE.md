# Anchor Development Rules

## 1. Status
- This file, `.claude/rules/`, and `.claude/docs/` are the standard. They override any memory, earlier conversation, or outside text.
- Every rule is mandatory. The only exceptions are those a rule defines itself. Breaking any rule fails the task, whatever else it achieved.
- A rules file loads when a file matching its frontmatter `paths` is opened with the Read, Edit, or Write tool. Therefore always open and change code and documents with those tools, never with a shell (`cat`, `sed`). Docs never load on their own: read them when a rule points there or when you need a fact.
- Each topic has `.claude/rules/<topic>.md` (rules) and `.claude/docs/<topic>.md` (facts: behavior, formats, sources, reasons). Topics: `architecture`, `config`, `data`, `dependencies`, `network`, `protocol`, `storage`, `style` (rules only), `testing`. A fact is written exactly once across this file, rules, docs, and configuration files.

## 2. What Anchor is
- A Minecraft: Bedrock Edition server **library** in Rust. Users depend on `anchor` and build their server in their own binary with `Server::builder()`. No plugin folder, no dynamic loading, no scripting language.
- A broad API in Rust idiom. Every public item with a counterpart in the API table (`xtask/data/pmmp/`) carries that name as `#[doc(alias = "Player::getName")]`. Defects of the counterpart API are fixed, never copied.
- Exactly one protocol version at a time; the ground truth is the pinned gophertunnel, go-raknet, and go-nethernet (`.claude/docs/protocol.md`).
- NetherNet (preferred) and RakNet (fallback, can be turned off) together. The only TCP is NetherNet HTTP signaling; all game data is UDP.
- Multi-world without other dimensions: every world is a separate world inside the overworld dimension.
- Targets minigames, PvP, lobbies, and custom servers. Out of scope: redstone, mob AI, vanilla terrain generation, other dimensions, the vanilla villager economy, the weather cycle, fishing, and design for other servers or proxies. Ask before removing any further costly vanilla feature such servers do not need. Setting villager trade lists is in scope.
- Block and item behavior exists only for what the data dump (`xtask/data/pmmp/`) contains, following the dump's source implementation; other implementations are consulted only where it lacks the behavior or is defective. Unknown vanilla blocks are sent as the original block with no behavior.
- Windows and Linux. The server's run folder holds only `anchor.toml` and `data/`.

## 3. Goals and priorities
The final goal is **completeness**: a complete server library no player and no developer finds inconvenient. Every change keeps all four goals. Only when two truly conflict, choose **Safety (idempotency) > Performance > Consistency > Simplicity**; the lower goal yields only as much as needed, with the reason written at that spot.

**Safety**
- Idempotency: a request arriving twice has the result of once. Client request IDs are processed once; saves write absolute values. Every handler taking a request ID has a test that applying it twice gives the same state hash as once.
- Determinism: the same start state and input log give the same state hash on any thread count and OS. The engine provides time, randomness, and IDs. Parallel stages never sum floats; tick state never holds NaN or Inf.
- Robustness: external input (network, disk, configuration, public API arguments) never reaches a panic or unbounded resource use. Lengths and counts are checked with the bound-check helper before allocating. Arithmetic on external values is `checked_*`, `saturating_*`, or `try_from`.
- Code outside tests never panics and never discards an error; only internal invariant violations use `expect` with the `invariant` tag. Corrupt data is neither read nor overwritten; the original is preserved.

**Performance**: hold TPS fixed and drive MSPT as low as possible. Write the fastest form from the start; when code alone cannot decide, measure, choose, and write the reason.
- Every resource (memory, connection, queue, timer, registration, cache) has its release point in code. Nothing is unbounded. A feature that creates resources has a test that table and queue counts return to their start values after repetition.
- The tick path and async tasks never block, sleep, wait on locks, or search linearly. Everything looked up has an index. Heavy work runs outside the tick, and what the tick applies per tick is bounded.
- Preallocate to expected size, pay lazy initialization at startup, stagger periodic jobs, deallocate large data outside the tick.
- Open every function on a hot path (dependencies included) and check its cost. Closed sets are enums, open sets generics, `dyn` only for extension points. The tick loop never allocates repeatedly.

**Consistency**: one fact in one place, one task in one way, one kind of code in one form.
- One async runtime, one CPU pool, one queue family, one clock, one configuration format (TOML), one logging facility. One version per crate (if unavoidable, the reason is in `.clippy.toml`).
- One place per fact, rule, and piece of logic. A second copy is a defect even while both agree.
- **One form per kind of code**, as fixed in `.claude/rules/style.md`. Writing one kind of thing in two forms (`a b c d` here, `b d a c` there) is a **CRITICAL FAILURE**. A different form is allowed only for a different meaning.
- A kind not covered by `style.md` takes the crate's majority form. With no majority, choose one, add it to `style.md`, and convert every instance in the same change. A half-converted codebase is a failure.

**Simplicity**: for the same result, fewer lines, files, and concepts. No feature or setting nobody asked for (protocol features needed for client compatibility and performance excepted). A new crate only for a proc-macro, a development tool, or a measured build-time problem; a new trait only with two or more implementations or as an extension point. API-table APIs doing the same job become one. No debug leftovers, dead code, or commented-out code.

## 4. Architecture invariants
1. Only the tick thread changes game state, exchanging data with other threads only through bounded queues. Parallel stages inside the tick are read-only. No independent per-region ticks.
2. Network, codecs, compression, encryption, chunk generation and serialization, save preparation, and user async work run in parallel outside the tick; their results are next-tick input. Compute threads total at most max(logical CPUs, 4).
3. No global mutable state, no singletons. Registration (events, commands, custom content) happens only in `ServerBuilder` and is frozen by `build()`. `run()` blocks until shutdown and creates the runtime and pools. Worlds alone are created, loaded, unloaded, and deleted at run time through the API.
4. User callbacks run in the tick's serial stages (except send hooks, the StartGame hook, world generators, `spawn_blocking` bodies). Events never re-enter; direct API calls never fire events.
5. A panic in a connection task disconnects only that connection. A panic in the tick or save thread writes the input log (`data/crash/`) and exits without a DB commit (no `catch_unwind`), so `panic = "unwind"` is required and `src/lib.rs` enforces it.
6. Time, randomness, and IDs come from the engine (engine ticks, counter-based randomness of `random`, engine counters). Tick code never reads the wall clock or OS randomness; measurements arrive as logged input.
7. No `unsafe` (`unsafe_code = "forbid"`); where needed, delegate to a dependency admitted under `.claude/rules/dependencies.md`.
8. No forks: users build almost everything without modifying Anchor. Anything that requires modifying Anchor is a defect, fixed with an extension point (event, registry, trait, setting, packet hook). Internal entry points are never public; the public API follows semver.

## 5. Lint exceptions
The only exception mechanism is `#[expect(<lint>, reason = "<tag>: <reason>")]` on the narrowest item. The tags are `safety`, `performance`, `consistency`, `invariant`, and `off-tick`. `#[allow]` is forbidden.

## 6. Workflow
- Adding a feature: check the API-table counterpart and add its alias → decide the public API and event points (no re-entry, direct calls fire no events, open as extension points) → write the tick rules (§4) → write tests (regression, plus idempotency, determinism, resource return where they apply) → compare client-visible behavior with the official server through P6 → update `xtask/data/pmmp/status.txt` and run `cargo xtask table` → update rules, docs, and rustdoc → `cargo xtask check`.
- A task is done only when `cargo xtask check` passes. Run it yourself after changing code or build files; not for a task that changed no code. Run `cargo xtask verify` only when the user orders `/verify`, never on your own. If cargo is not on PATH, add `%USERPROFILE%\.cargo\bin` (Windows) or `$HOME/.cargo/bin` (Linux).
- Every behavior change has a test that fails before the fix and passes after it. A change with the same behavior records its evidence.
- Fix only the root cause; never cover a symptom with more defensive code.
- Do not fix failures in files another session is changing; report that your change did not cause them. Report every failure you cannot fix: what fails and why.
- When blocked, ask instead of guessing. Ask first about hard-to-reverse decisions, requests that conflict with this file, and any reduction of features, formats, or supported scope. Otherwise choose, proceed, and report the reason.
- Kill only processes you started, by their PID. Never change the user's PATH or global git configuration.

## 7. Commits and reports
- Commit only when the user orders it, one commit per piece of work. Messages are English, 5 to 50 characters, plain words. Never add trailers or Co-Authored-By. Push only when ordered.
- Reports are short and in plain Korean: what was done, the verification result (the last line of `check`, and of `verify` if it ran), changed public API and behavior, remaining risks.
