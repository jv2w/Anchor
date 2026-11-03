---
name: audit-safety
description: Finds, proves, and fixes Anchor's safety defects (idempotency, determinism, robustness, panics and error handling) that the current code actually reaches and that break a rule or cause serious harm. Use only when the user orders `/audit-safety`.
argument-hint: "[file or folder; empty for all of src]"
disable-model-invocation: true
---

# Safety audit

Target: `$ARGUMENTS` (empty means all of `src/`). The standard is Safety in `CLAUDE.md` §3 and the invariants in `CLAUDE.md` §4.

## Principles
- Fix only defects that **the current code actually reaches** and that **break a rule or cause serious harm**. Prove both.
- Zero defects is a normal result; the count is not judged. The worst result is fixing an unproven candidate as a defect: such a fix only adds code and becomes noise for the next audit.
- The burden of proof is on the candidate. "It could", "if", and "just in case" are not evidence. Evidence is a code path and a failing test.
- Collect candidates broadly and decide defects narrowly. This does not mean checking shallowly.
- A reached violation of a rule in `CLAUDE.md` or `.claude/rules/` is never minor: it is at least MAJOR and is fixed (`CLAUDE.md` §1: every rule is mandatory). MINOR covers only findings that break no rule.

## Defect definition
A defect has all three, proven.

1. **Reach**: an existing code path leads from a trust boundary or normal operation to the problem line.
   - Trust boundaries: network packets, disk files (worlds, redb, packs, keys, imported worlds), `anchor.toml`, console lines, public API arguments. User code may call public functions with any arguments, in any order.
   - Normal operation: repeated joins and quits, long uptime, inputs overlapping in one tick, a target disappearing mid-processing, crash and restart, world load and unload.
   - Open every check on the path (decoder bounds, the `Reader` allocation budget, input budgets, coordinate checks, types, earlier validation, queue bounds) and show that none stops this value. If any check is left unopened, reach is unproven.
2. **Harm**: a rule in `CLAUDE.md` §3 Safety or §4 is broken, or the result reaches CRITICAL or MAJOR in the grade table.
3. **Evidence**: a test that starts at the trust boundary, or from a state the boundary can produce, fails now. A test that feeds an internal function a value or state no entry point can produce is not evidence. What cannot be reproduced by a test on this PC (a result that differs only on another OS, a value only the official client sends) is not fixed but left unconfirmed; report the code path, the specification evidence (IEEE 754, the protocol source), and how to confirm it (a Linux run, a real client).

## Grades
| Grade | Result |
|---|---|
| CRITICAL | The whole server stops or exits (tick or save thread panic, deadlock), item or data duplication or loss, a corrupted save file or an overwritten corrupt original, the same start state and input log giving a different state hash on another thread count or OS, unbounded memory or CPU use from a remote peer, an authentication or permission bypass |
| MAJOR | A normal client or normal user code suffers a disconnect or wrong state, the same request twice changes state twice, an error the operator must know about (save, load, authentication) is discarded, any other reached violation of a `CLAUDE.md` §3 Safety or §4 rule |
| MINOR | Anything else that breaks no rule. Not fixed |

A malicious client that only disconnects itself causes no harm (it is the same as leaving), unless it breaks a rule (for example, external input reaching a panic). It is harm only when it spreads to other connections, shared resources, saves, or logs.

## Not candidates
- What `cargo xtask check` already blocks: the lints in `[workspace.lints]` (`unwrap_used`, `indexing_slicing`, `panic`, and others) and the `.clippy.toml` deny lists (clocks, randomness, `HashMap`, locks, blocking I/O, and others). A spot lifted with `#[expect(..., reason = "...")]` is a candidate for whether that reason is true; in particular, an `invariant` tag whose invariant external input can break is a defect.
- Test code (`#[cfg(test)]`, `tests/`) and generated files themselves. Look for defects of generated code in its generator (`xtask/src/gen*`) or in the hand-written helpers the generated code calls.
- The design limits in `.claude/docs/architecture.md`, intended decisions with a written reason, and `xtask/data/p6/intended.txt`. They are candidates only when you show the reason is wrong for the current code.
- Proposals to recheck a value already checked earlier, checks inside internal calls within the trust boundary, and error handling with no meaningful recovery.
- Anything whose only evidence is a name, a comment, or the look of the code ("there is an `as`", "this could overflow"). Follow where the value actually comes from first.
- Defects inside a dependency on paths Anchor never calls.
- Several spots with one cause are one defect.

## Steps
1. **Read**: the `.claude/rules/` and `.claude/docs/` topic files for the target, and the design limits in `.claude/docs/architecture.md`. Knowing the bounds, policies, and limits already set cuts noise.
2. **Collect**: start from the input, not from the look of the code.
   - For each trust boundary, find where values enter and follow each value to where it is used before being checked (allocation size, index, arithmetic, cast, recursion depth, state change, save).
   - Write a candidate down only when you can fill in the trigger (who, which input), the path (chained `file:line`), and the result. Otherwise drop it on the spot.
   - For a large target, split it by boundary (transport and session, protocol decode, storage, import and packs, configuration and console, tick handlers, public API) and give each to a subagent that only collects and edits no file. Have it read this file and return those three fields per candidate. Decide every candidate yourself.
3. **Refute**: for each candidate, first ask "if this is not a defect, what stops it?", then find and open that code. Follow every caller from the entry point to the problem line (each caller when there are several). If something stops it, reject the candidate.
4. **Reproduce**: write a failing test for each remaining candidate. Idempotency shows that the same input applied twice gives a different state hash than once; determinism shows that one and many CPU pool threads (or a replay) give different hashes; conservation shows that the item total or file contents change. Follow the existing tests listed in `.claude/docs/testing.md`. If the test does not fail, reject the candidate or leave it unconfirmed.
5. **Independent review**: give each reproduced candidate to a new subagent with only the claim (trigger, path, result) and the test, not how you found it. Its task: "Find evidence in the code that this claim is wrong: a check on the path, why the entry point cannot produce this state, whether the test skipped the entry point and built the state directly, why the result is actually harmless." If the evidence holds as `file:line`, reject the candidate. Launch the reviews for several candidates together.
6. **Fix**: fix only the remaining defects, at the root cause (`CLAUDE.md` §6). Do not add checks or guards that block the symptom. Confirm that the reproducing test passes and no other test breaks. Ask before fixing when the fix changes the public API, a storage format, a configuration key, or the feature scope.
7. **Check**: `cargo xtask check`.

Do not restart the audit from the beginning. When the same cause appears again around a fixed spot, handle only that, with the same steps.

## Report
Write the report to the user in short, plain Korean (`CLAUDE.md` §7).

1. First line: defects fixed (CRITICAL a, MAJOR b), unconfirmed, minor, rejected.
2. For each fixed defect:
   - `[grade] one-line summary` — `file:line`
   - Trigger: who, with which input or situation
   - Path: entry point → … → problem line, and why the checks on it did not stop the value
   - Result
   - Evidence: the test name (fails before the fix, passes after)
   - Fix: the root cause in one line
3. Unconfirmed: one line each, saying what is unproven and what would confirm it.
4. Minor: one line each. Not fixed.
5. Rejected: counts per reason only (a check stops it, unreachable, harmless, blocked by a lint, intended decision, refuted by independent review).
6. The items `CLAUDE.md` §7 requires.
