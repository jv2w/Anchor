---
paths:
  - "tests/**"
  - "xtask/tests/**"
  - "src/**/tests.rs"
  - "src/**/tests/**"
  - "xtask/src/p6.rs"
  - "xtask/src/p6/**"
  - "xtask/go/**"
  - "xtask/data/p6/**"
---
# Testing rules

## Checks
- `cargo xtask check` runs the format check, clippy, and the tests of what changed: the library tests of the changed modules, the changed integration test files, and xtask's unit tests when xtask's sources change; it also runs the API table check and the documentation builds when what they read changed. `cargo xtask verify` adds the generated-code, API-table, and lint-reason checks, every workspace test (the bot tests included), the vendor copies' tests, and the documentation build. The exact steps are in `.claude/docs/testing.md`.
- Support and verify both Windows and Linux. Check everyday work with `cargo xtask check` on Windows. For a change that touches the OS (files, signals, sockets, console, xtask tools), also run the check on WSL Ubuntu in an ext4 copy. Run `verify` and full P6 runs on both.

## Writing tests
- Public API tests in `tests/` use only the public API, as a user's binary does.
- Mark a measurement test `#[ignore = "measurement"]`.
- Mark a test that serves as another test's child process `#[ignore = "the child process of <parent>"]`; the parent runs it.
- Decide moments by tick counts and commit reports, never by wall-clock time.
- Fix the cause of a test that fails under load. Never cover it with retries, sleeps, or longer time limits.

## P6
- Compare every client-visible change with the official server through P6. A bot scenario must first pass on the official server before it verifies Anchor.
- Run the official server only from its copy in `.cache/official/<version>/`, the version the harness pins. Never run the original download. Never analyze the executable.
- Compare with the official server only through the packets the bot receives.
- For what only the official client shows, make Anchor send the values the official server sends, and give the user a checklist of what to look at in the client.
- Keep the P6 test settings at Anchor's defaults, except the values that align the two servers (no Xbox login, world name, seed) and what the harness needs to run (ports, transport, compression, cheats for the API session).
- Where Anchor differs from the official server, follow the official server. Only a difference Anchor keeps on purpose in an API or import comparison goes into `xtask/data/p6/intended.txt`, with its reason. The spawn and game comparisons do not read that file.
- Review `intended.txt` like a code change.
- Never put a value that differs per world or per run into `fields.txt`.
- Keep `facts.md` grouped by topic, with the open questions and the real-client checks at the end.
- Run the `auto` fallback of both servers with `FALLBACK_COMPRESSION` (`xtask/src/p6.rs`).

## Adding a P6 scenario
1. Write the scenario function in `xtask/go/bot/` and register it in `gameScenarios` with its own area in `gameAreas`, apart from every other scenario's area. Split it into steps (`step` records), write the world setup as `command` records (console commands), and record each fact to compare as a `check` line; for a game scenario, add the check's name to `FIXED_CHECKS` in `xtask/src/p6/game/trace.rs`.
2. Add its name to the harness's `SCENARIOS` in `xtask/src/p6/game.rs` or `xtask/src/p6/api.rs`. An API scenario that only records facts also goes into `FACTS_ONLY`; one that compares only its check lines goes into `CHECKS_ONLY`.
3. When Anchor must carry out a console command the scenario uses, translate it to the public API in `xtask/src/p6/game/console.rs`.
4. Run `cargo xtask p6 game official <name>` or `cargo xtask p6 api official <name>` at least twice: it must pass on the official server with the same values. Leave out of the comparison every value that differs per run.
5. Write the observed values and Anchor's decision for each into `facts.md`.
6. Compare Anchor with `cargo xtask p6 game anchor <name>` or `cargo xtask p6 api anchor <name>`. Make each difference follow the official server, or, for an API scenario, list it in `intended.txt`.

## Running tests
- When a test that uses NetherNet signaling, or a NetherNet run of `p6 anchor`, fails on a connection timeout, run it once more; this PC's loopback TCP is the known cause (`.claude/docs/testing.md`). A second failure is a real failure.
- Run heavy tests at low priority and one at a time. Never run two builds that share a `target` folder at the same time (lock contention).
- Bound every heavy test run: the number of runs, the build and test parallelism, the tests selected by name, and for P6 the scenario names.
- Run measurement tests in release, alone, never alongside another build.

Background and detailed facts are in `.claude/docs/testing.md`.
