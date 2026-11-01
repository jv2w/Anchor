---
name: audit-consistency
description: Finds, proves, and fixes Anchor's consistency and simplicity defects (one form per kind of code, one way per task, one place per fact, text that matches the code, no dead code) that are wrong now or go silently wrong when one side changes. Use only when the user orders `/audit-consistency`.
argument-hint: "[file or folder; empty for the whole repository]"
disable-model-invocation: true
---

# Consistency audit

Target: `$ARGUMENTS` (empty means the whole repository, excluding `vendor/`, `.cache/`, and `target/`). The standard is Consistency and Simplicity in `CLAUDE.md` §3 and every rule in `.claude/rules/style.md`.

## Principles
- Fix only what is **proven**: two forms for one kind of code, a fact or rule in two places, two ways for one task, text that contradicts the code, or something dead.
- Zero defects is a normal result; the count is not judged. The worst result is merging two things that only look alike: when two different meanings are merged, fixing one breaks the other.
- The burden of proof is on the candidate. Equal values or shapes prove nothing. Proof is evidence of the same fact, a sentence that contradicts the code, a search with no caller, or a `style.md` rule (or the crate's majority form) next to the instance that departs from it.
- Collect candidates broadly and decide defects narrowly. This does not mean checking shallowly.
- Every proven defect is fixed. A rule violation is never minor (`CLAUDE.md` §1: every rule is mandatory).

## Defect definition
A defect is one of these five, with its proof.

1. **Two forms for one kind of code**: one kind of thing (file layout, `impl` order, derive order, list order, signature, error type, log call, name, literal, comment, test) is written in two forms. Proof: the form `style.md` fixes, or, for a kind it does not cover, the crate's majority form, against each departing instance. A different form is allowed only for a different meaning; show the meaning or it is a defect.
2. **One fact in two places**: two spots express the same fact. Proof: "when one changes, the other must change to stay correct": the same protocol field, configuration key, bound shared by encode and decode, rule followed by two handlers, or format version. Equal values alone are not proof.
3. **Two ways for one task**: one purpose (async runtime, CPU pool, queue family, clock, configuration format, logging, error representation, bounded file read, one computation) is served by two or more means now.
4. **Text that contradicts the code**: a fact (number, name, order, "only", "always") stated in `.claude/rules/`, `.claude/docs/`, `CLAUDE.md`, rustdoc, a comment, or a log or error message differs from the code now, shown by following the code.
5. **Dead**: a private or `pub(crate)` item with no caller outside tests, a configuration key nobody reads, a variant never built outside tests, a debug leftover, commented-out code, a trait with one implementation that is not an extension point, two public APIs doing the same API-table job, a feature or setting nobody asked for. Search for callers across `src/`, `tests/`, `examples/`, and `xtask/`.

## Grades
| Grade | Result | Evidence |
|---|---|---|
| CRITICAL | Two forms for one kind of code (`CLAUDE.md` §3) | The `style.md` rule or majority count, and every departing `file:line` |
| CRITICAL | Two places of one fact already differ, giving a wrong result on normal input (normal data rejected, wrong save, mismatch with the client, check and apply following different rules) | A test that shows the mismatch and fails now |
| MAJOR | One fact in two places that goes silently wrong when one side changes, with no test or compile error to catch it | Both `file:line` and the proof of the same fact |
| MAJOR | Two ways for one task | The `file:line` of each means in use |
| MAJOR | Text that contradicts the code; a person or AI trusting it does the wrong thing | The sentence and the contradicting code `file:line` |
| MAJOR | Dead | The name search (including callers through macros, generated code, and strings) |

## Not candidates
- Values or shapes that are equal by chance: constants with the same number but different meanings, two handlers that look alike but follow different rules (dump source behavior, protocol differences).
- What tests, the compiler, or clippy already tie together: the test comparing the first toml block of `.claude/docs/config.md` with `DEFAULT_FILE` (`tests/config.rs`), state hashes that destructure without `..`, `multiple_crate_versions` with `allowed-duplicate-crates`, the `dead_code` and `unreachable_pub` warnings, and lints such as `mod_module_files`, `module_name_repetitions`, and `redundant_test_prefix`. An `allowed-duplicate-crates` entry or `#[expect]` reason that is no longer true is a candidate.
- Repetition inside generated files (the source is the generator), differences from external implementations, public API with no internal caller (Anchor is a library).
- Deliberate exceptions with a written reason (for example, only the wait between ticks uses `std::thread::sleep`; `.claude/rules/architecture.md` requires it).
- A rules file and a docs file covering one topic (rules and facts). It is a duplicate only when the same fact sentence is in both.
- Refactors of taste: splitting or merging files, extracting a shared function, renaming. They are candidates only when they bring one kind to its one form, put one fact in one place, or actually remove code.
- Several spots with one cause are one defect.

## Steps
1. **Read**: `CLAUDE.md`, `.claude/rules/style.md`, and the target topic's `.claude/rules/` and `.claude/docs/` files.
2. **Collect**: start from the five kinds in the defect definition, not from the look of the code.
   - Forms: for each `style.md` section, find every instance of that kind in the target and compare it with the fixed form. For a kind `style.md` does not cover, count the forms across the crate.
   - Facts, means, text, items: from where each is defined, find where it is restated or used.
   - Write a candidate down only when you can fill in both spots (or the text and the code) as `file:line` and the proof that they are one kind or one fact. Otherwise drop it on the spot.
   - For a large target, split it into code areas and `.claude/` documents and give each to a subagent that only collects and edits no file. Have it read this file and return those fields per candidate. Decide every candidate yourself.
3. **Refute**: for each candidate, first ask "if the two have different meanings, what makes them different?", "if the text is right, where does the code do that?", "if it is not dead, who calls it?", and look for the evidence. If you find it, reject the candidate.
4. **Reproduce**: for a fact that already diverged, write a test that shows the mismatch and fails. A form conversion, a duplicate that still agrees, two ways, text, or something dead whose fix keeps behavior records `file:line` evidence instead of a test (`CLAUDE.md` §6).
5. **Independent review**: give each remaining candidate to a new subagent with only the claim and its evidence, not how you found it. Its task: "Find evidence that this claim is wrong: why the two have different meanings, why the text is right, who calls this item (macros, generated code, strings, other packages included), why the second way is intended." If the evidence holds as `file:line`, reject the candidate. Launch the reviews for several candidates together.
6. **Fix**: fix only the remaining defects.
   - Two forms: convert every instance in the crate to the `style.md` form in the same change. For a kind `style.md` does not cover, use the majority form; with no majority, choose one, add it to `style.md`, and convert every instance (`CLAUDE.md` §3). A half-converted codebase is a failure.
   - For a fact that already diverged, decide which side is right from the ground truth that `CLAUDE.md` §2 and the topic's rules file name. If that does not decide it, ask.
   - Keep one fact in one place and have the other spots take it from there (one constant, one function, a generator). Where that is impossible (a document and code), add a test that compares the two.
   - Code that breaks a rule in `.claude/rules/` is changed to follow it. When a factual description in docs or a comment differs from the code, decide which is intended from evidence (tests, commits, ground truth); if that does not decide it, ask.
   - Delete what is dead. Ask first when deleting changes the public API, a storage format, a configuration key, or the feature scope.
   - Confirm that performance and safety stay the same after the fix (`CLAUDE.md` §3 priorities). Do not merge when merging adds indirect calls or allocation on a hot path.
7. **Check**: `cargo xtask check`.

Do not restart the audit from the beginning. When the same cause appears again around a fixed spot, handle only that, with the same steps.

## Report
Write the report to the user in short, plain Korean (`CLAUDE.md` §7).

1. First line: defects fixed (CRITICAL a, MAJOR b), unconfirmed, rejected.
2. For each fixed defect:
   - `[grade] one-line summary` — `file:line`
   - What diverged or overlapped, and the proof that it is one kind or one fact
   - Result: what is wrong now, or what goes wrong when one side changes
   - Evidence: the test name (fails before the fix, passes after), or `file:line` evidence
   - Fix: which side became the standard and what was removed, in one line
3. Unconfirmed: one line each, saying what is unproven and what would confirm it.
4. Rejected: counts per reason only (different meaning, text is right, has a caller, intended exception, tied by a lint or test, refuted by independent review).
5. The items `CLAUDE.md` §7 requires.
