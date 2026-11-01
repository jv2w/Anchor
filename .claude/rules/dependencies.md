---
paths:
  - "Cargo.toml"
  - "xtask/Cargo.toml"
  - "Cargo.lock"
  - ".clippy.toml"
  - ".cargo/config.toml"
  - "vendor/**"
  - "xtask/src/vendor.rs"
  - "xtask/data/vendor/**"
  - "xtask/tests/*_repro.rs"
  - "xtask/tests/downstream.rs"
  - "xtask/go/go.mod"
  - "xtask/go/extract/**"
  - "xtask/src/gen.rs"
  - "xtask/src/gen/**"
  - "xtask/src/dump.rs"
  - "xtask/src/dump/**"
  - "xtask/php/**"
---
# Dependency rules

## Admission
- Admit an external dependency only when all of these hold:
  - implementing it directly is very hard;
  - it is very accurate and can be checked against ground truth (a standard, the official server, test vectors) and tests;
  - it has no hidden threads, global state, unbounded queues, or panics reachable by external input, or these can be blocked;
  - it has no open RustSec advisory, active maintenance, and a permissive license;
  - it matches the Bedrock format byte for byte;
  - it supports Windows and Linux.
- Pin every version with `=`.
- Never admit a crate for anything in the "Implemented directly" table of `.claude/docs/dependencies.md`; use the direct implementation.
- Anchor is a library: never set a global allocator or a build profile for its users.

## Distrust procedure
- Apply it to everything taken from outside: crates, Go modules, data sources, documents.
- Before admitting, read the parts used yourself. Search known issues and advisories, and test the boundary conditions: panic, overflow, allocation, hidden thread, determinism.
- Remove every defect found, in this order: a fixed release, pinning, a guard, a different choice. A defect upstream has not fixed may instead be fixed in a vendor copy (below). If a defect cannot be removed, do not admit the dependency.
- Run the procedure again on every version bump.
- Record every defect and its measure in the table or the vendor-copy section of `.claude/docs/dependencies.md`.

## Guards
- The primary guard is `disallowed-methods` and `disallowed-types` in `.clippy.toml`.
- In the change that admits a dependency, add its clock, randomness, unbounded queue, blocking, and unbounded allocation entry points to those deny lists.
- Confirm with throwaway code that each new entry actually fires.

## Vendor copies
- Fix a defect upstream has not fixed in a copy at `vendor/<crate>/`.
- Fix only the defective part. Never add features or polish.
- Every fix has a reproduction test that fails on the published crate.
- Record each fix and its reason in the vendor-copy section of `.claude/docs/dependencies.md`, and mark it in the code with an `Anchor` comment.
- Start each copy from the pinned crates.io release. A copy differs from it only in its fixes, the `path` lines to other copies, and an upstream license file the published crate lacks.
- Depend on a copy by `path`, never by `[patch]`. Never make a copy a workspace member (`exclude = ["vendor"]`).
- When an upstream release fixes a defect, drop that fix from the copy. When no fix remains, delete the copy and return to the release.
- Review each new upstream release with the distrust procedure before moving to it.
- Never commit the capture files of str0m's integration tests. After changing the str0m copy, run `cargo xtask vendor-test str0m`.
- On every dimpl version bump, check again that no path returns a buffer from outside its pool.
- Delete the str0m-proto, str0m-aws-lc-rs, and is copies together with the str0m copy.

## Build settings
- The repository's build settings hold only what shortens build time with the same result.
- Never use dependency `opt-level = 1` or sccache.

## Copyright
- Never copy text or code from other servers and libraries verbatim, including output formats, comments, and blocks of code. Use only the principles.
- Every sentence the server produces (console logs, command output, kick reasons, error messages, status output) is Anchor's own wording. Sending client translation keys is fine.
- A format the protocol requires (such as the field order of the ping response) is a protocol rule, not wording: follow it exactly.
- The generator writes the documentation of generated code from the IR's information only: names, packet IDs and directions, wire formats, limits.
- The extractor never reads gophertunnel's comments.
- Data holds facts only: values, numbers, limits.
- From Mojang's protocol documentation take only numbers; never copy its files.
- Obtain dump data by running the dump source.
- When outside code or data enters the repository, add its source, files, and license notice to `THIRD-PARTY-NOTICES.md` in the same change.

## Evidence
- The only evidence is official documentation, code, and measurement.
- When sources disagree, follow the official server comparison, then the protocol source, then Mojang documentation.

Background and detailed facts: `.claude/docs/dependencies.md`.
