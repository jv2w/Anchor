---
name: audit-performance
description: Finds Anchor's performance defects (leaks, blocking and bottlenecks on the tick and async paths, spikes) that real load triggers and that break a rule or seriously hurt MSPT or memory, and fixes only those whose improvement the code alone proves, without benchmarks. Use only when the user orders `/audit-performance`.
argument-hint: "[file or folder; empty for all of src]"
disable-model-invocation: true
---

# Performance audit

Target: `$ARGUMENTS` (empty means all of `src/`). The standard is Performance in `CLAUDE.md` §3 and invariants 1 and 2 in `CLAUDE.md` §4.

## Principles
- Fix only defects that **happen under real load**, **break a rule or cause serious harm when they happen**, and whose fix is **proven better by the code alone**. Prove all three.
- Decide whether to fix by reading code (dependency sources included) only. Never run benchmarks, profilers, timing, or measurement tests (`#[ignore = "measurement"]`); run only correctness tests and `cargo xtask check`. Where `CLAUDE.md` §3 says to measure because code alone cannot decide, this audit does not measure: it reports such candidates as unconfirmed and leaves them unfixed.
- Zero defects is a normal result; the count is not judged. The worst result is fixing an unproven improvement: such a fix only complicates the code and becomes noise for the next audit.
- The burden of proof is on the candidate. "Seems slow", "would be faster", and "if there are many" are not evidence. Evidence is the frequency, the bound N, the opened cost, and the cost formulas before and after the fix.
- Collect candidates broadly and decide defects narrowly. This does not mean checking shallowly.
- A violation of a rule in `CLAUDE.md` or `.claude/rules/` that real load reaches is never minor: it is at least MAJOR and is fixed (`CLAUDE.md` §1: every rule is mandatory). MINOR covers only findings that break no rule.

## Defect definition
A defect has all five, proven.

1. **Frequency**: confirm how often the path actually runs (every tick, every player every tick, every packet, every chunk load, every join) by following its callers. Never call a path hot from its name or module location.
2. **Scale**: name the variables that set the cost (players, entities, chunks, worlds, queue length, input size) and their bounds as numbers. Take bounds from the configuration ranges (`.claude/docs/config.md`), the queue and input budgets (`.claude/docs/architecture.md`, `.claude/docs/network.md`, `.claude/docs/protocol.md`), and documented limits. Never inflate a cost with a large N that has no source.
3. **Cost**: open every function the path calls, dependencies included, and count the real cost (allocation, copy, iteration, hashing, locking, system calls). An unopened call is neither cheap nor expensive.
4. **Harm**: a rule in `CLAUDE.md` §3 Performance or §4 invariants 1 and 2 is broken, or the result reaches CRITICAL or MAJOR in the grade table.
5. **Proof**: show by code that the fixed code costs no more than before on every input, and, for a defect, clearly less.
   - Write the cost formulas before and after the fix in the same unit (operations, allocations, elements iterated, work on the tick thread, memory held) and compare them at the bound N.
   - List every cost the fix adds (index upkeep, allocation, hashing, queue sends, indirect calls, memory kept) and show by code that it is smaller than the cost removed.
   - A gain that depends on constant factors, cache locality, branch prediction, the allocator, thread count, or hardware, so that only measurement can show it, is not proven.

## Grades
| Grade | Result | Show by code |
|---|---|---|
| CRITICAL | Leak: in repeated normal operation (join and quit, world load and unload, creating and removing entities, windows, tasks, and timers, long uptime) a table, queue, cache, or registration grows and never shrinks | The spot that creates it, and the absence of any path that removes it in normal operation (`file:line`) |
| CRITICAL | Blocking: the tick thread or a tokio worker stops on disk I/O, a lock or channel wait, sleep, or synchronous networking | The path to the blocking call as `file:line`, and the evidence that the call blocks (dependency code) |
| CRITICAL | Amplification: one external input causes cost out of proportion to its size or input budget (a full scan, quadratic work, an allocation larger than the input) on the tick or a shared worker | The cost formula per input and its value at the bound N |
| MAJOR | Bottleneck: at the load bound, a path that runs every tick is superlinear in N (N², N·M), repeats a linear search where an index gives O(1), or repeats an allocation proportional to N every tick | The cost formula and its value at the bound N, and the formula after the fix |
| MAJOR | Spike: periodic jobs pile into one tick, or a large deallocation, lazy initialization, or reallocation proportional to the bound N happens inside the tick | The formula of the work piled into that tick, and the maximum one tick keeps after the fix |
| MAJOR | Any other reached violation of a `CLAUDE.md` §3 Performance or §4 invariant 1 or 2 rule | The rule and the violating `file:line` |
| MINOR | Anything else that breaks no rule (constant-factor gains, an improvement that keeps the cost order) | Not fixed |

## Not candidates
- Allocation and linear search on startup and shutdown paths, which run outside the tick path and async tasks. On the tick path and in async tasks, a linear search breaks `CLAUDE.md` §3 even on a rare path.
- Collections and queues with a bound and a release point. Filling up to the bound is not a leak.
- What the design decides: the `sleep` between ticks, blocking on the save thread and the blocking pool, codec, compression, and encryption on network workers, choices whose reason is written in `.claude/docs/`, and the known limits and risks in `.claude/docs/architecture.md`. They are candidates only when you show by code that the reason no longer fits the current code.
- What `cargo xtask check` already blocks (the `.clippy.toml` deny lists, `clone_on_ref_ptr`, and others). A spot lifted with `#[expect]` is a candidate for whether its reason is true.
- `dyn` at extension points, costs the compiler removes (small `Copy` values, moves, small function calls), containers already sized.
- Test code and generated files themselves (fix the cost of generated code in its generator).
- Micro-optimizations and constant-factor gains that break no rule.
- Several spots with one cause are one defect.

## Steps
1. **Read**: `.claude/rules/architecture.md`, `.claude/docs/architecture.md` (threads and queues, the tick, known limits and risks), and the target topic's rules and docs.
2. **Collect**: start from frequency and resources, not from the look of the code.
   - Write a candidate down only when you can fill in the frequency, the bound N, and the cost formula (for example, N_player × N_entity comparisons every tick). Otherwise drop it on the spot.
   - For a large target, split it into groups (tick stages, network and session, storage, chunks and lighting) and give each to a subagent that only collects and edits no file. Have it read this file and return the frequency, N, cost formula, and `file:line` per candidate. Decide every candidate yourself.
3. **Refute**: for each candidate, first ask "if this is not a defect, what keeps the cost down?": a bound, a cache, an existing index, staggered periods, skipping the empty case, a setting such as `slow-empty-worlds`, a lower call frequency than assumed. If something keeps it down, reject the candidate.
4. **Prove**: for each remaining candidate, decide the fix and fill in condition 5 of the defect definition. Prefer removing the cost over moving it (remove → reduce → move off the tick → spread over ticks). If condition 5 cannot be filled in, do not fix; leave the candidate unconfirmed.
5. **Independent review**: give each proven candidate to a new subagent with only the claim (frequency, N, cost formulas before and after, added costs) and the planned fix, not how you found it. Its task: "Find evidence in the code that this fix is wrong: any input where it is slower or uses more memory, a missed added cost, why the frequency is lower, why the bound N is smaller, a bound or cache that already keeps the cost down, any part only measurement can decide." If the evidence holds as `file:line`, reject the candidate or leave it unconfirmed. Launch the reviews for several candidates together.
6. **Fix**: fix only the remaining defects, exactly as proven.
   - Reread the fixed code and confirm it matches the cost formula used in the proof. If an unplanned cost appeared, revert and leave the candidate unconfirmed.
   - Never break safety (`CLAUDE.md` §3 priorities). When moving work into parallel stages, check result order and float sums and run the determinism tests listed in `.claude/docs/testing.md` (one and many CPU pool threads). Give every cache and index a release point and a bound.
   - When fixing a leak, add the resource-return test that `CLAUDE.md` §3 Performance requires, in the style of `src/tick/tests/leak.rs`.
   - Ask before fixing when the fix changes the public API, a storage format, a configuration key, or the feature scope.
7. **Check**: `cargo xtask check`.

Do not restart the audit from the beginning. When the same cause appears again around a fixed spot, handle only that, with the same steps.

## Report
Write the report to the user in short, plain Korean (`CLAUDE.md` §7).

1. First line: defects fixed (CRITICAL a, MAJOR b), unconfirmed, minor, rejected.
2. For each fixed defect:
   - `[grade] one-line summary` — `file:line`
   - Frequency and bound N (with sources)
   - Cost: the formulas before and after the fix, and their values at the bound N
   - Proof: the costs added, and why they are smaller than the cost removed
   - Fix: the root cause in one line (for a leak, the name of the resource-return test)
3. Unconfirmed: one line each, saying what the code could not prove (the part only measurement can decide).
4. Minor: one line each. Not fixed.
5. Rejected: counts per reason only (a bound or cache keeps it down, low frequency, decided by design, blocked by a lint, refuted by independent review).
6. The items `CLAUDE.md` §7 requires.
