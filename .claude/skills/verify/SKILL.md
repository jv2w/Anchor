---
name: verify
description: Runs Anchor's full check `cargo xtask verify` at low priority and reports the result. Use only when the user orders `/verify`.
disable-model-invocation: true
---

# Full check

The steps `verify` runs are in `.claude/docs/testing.md`.

1. If a `cargo xtask verify` is already running, wait until it ends. Heavy tests run one at a time (`.claude/rules/testing.md`).
2. Run it in the background at low priority from the repository root, with its output in a file:
   - Windows (Git Bash): `cmd //c "start /low /b /wait cargo xtask verify" > target/verify.log 2>&1`
   - Linux: `nice -n 19 cargo xtask verify > target/verify.log 2>&1`
   - If cargo is not on PATH, add it as `CLAUDE.md` says.
3. When it ends, read the lines starting with `ok  ` or `FAIL` and the last line (`PASS` or `FAIL`). For a `FAIL` step, find the cause in the output printed just above its line.
4. Report to the user in short, plain Korean (`CLAUDE.md` §7): the last line, each failed step and its cause, and the time taken. List separately the failures this change did not cause (environment, another session's changes).
5. After fixing a failure, rerun only the fixed step yourself and run `cargo xtask check` (`CLAUDE.md` §6). Run the whole `verify` again only when the user orders it.
