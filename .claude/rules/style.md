---
paths:
  - "src/**/*.rs"
  - "tests/**/*.rs"
  - "examples/**/*.rs"
  - "xtask/src/**/*.rs"
  - "xtask/tests/**/*.rs"
  - "CLAUDE.md"
  - "README.md"
  - ".claude/**/*.md"
---

# Style rules

Every kind of code has exactly one form, written here. Two forms for one kind is a CRITICAL FAILURE (`CLAUDE.md`). A kind not listed here takes the crate's majority form; with no majority, add a rule here and convert every instance in the same change.

## Modules and visibility
- Modules are `foo.rs` and `foo/bar.rs`; `mod.rs` is forbidden. Module placement is in `.claude/rules/architecture.md`.
- The first line of every module is a `//!` comment stating its responsibility.
- Visibility is the narrowest that covers the item's users: private, `pub(super)` (the parent module and its children), `pub(in crate::<module>)` (one subtree), `pub(crate)` (the crate), `pub` only for API reachable from the crate root. Every public item has `///` documentation and a `Debug` implementation.
- Re-exports of any visibility (`pub use`, `pub(crate) use`, `pub(super) use`) form one block in rustfmt's path order. No `use` path starts with `self::` (`pub use stack::Stack;`, `use stack::Slot;`).
- Paths: `super::` names only the parent module and what it holds (`super::Item`, `super::sibling::Item`, and `use super::*;` in tests). Every other crate item is named from `crate::`; never `super::super::`.

## File layout
Every `.rs` file has exactly this order:
1. `//!` module documentation.
2. `mod` declarations, alphabetical; an inline module with a body (`mod sealed { … }`) stands among them, except `mod tests`.
3. `pub use` re-exports.
4. `use` declarations in three groups separated by one blank line: `std`/`core`/`alloc`, then external crates, then `crate::`/`super::` and child modules (`use kind::Kind;`). Macros of an external crate are imported (`use tracing::warn;`, `use serde::Deserialize;`), never written with their crate path.
5. Constants and statics.
6. Types, each followed directly by its inherent `impl` and then its trait `impl`s; a checkpoint macro counts as a trait `impl`, called through the imported module as `checkpoint::fields!(…)` and `checkpoint::variants!(…)`, its list ending with a trailing comma when it spans lines. After the module's own types come its `impl`s of other modules' types (`impl Context<'_>`), one block per type.
7. Free functions.
8. `#[cfg(test)] mod tests` as the last item.

Items are ordered top-down: the module's main item first, every caller above the items it calls.

## Inside an `impl`
- Order: associated constants → constructors (`new`, `with_*`, `from_*`) → queries (getters, `is_*`, `has_*`, `can_*`) → mutators (`set_*`, `*_mut`, other `&mut self` methods) → conversions (`as_*`, `to_*`, `into_*`) → private helpers. Private helpers are every private method, in the same kind order. Within one kind, a caller comes above the methods it calls; methods without that relation are alphabetical.
- Trait implementations of one type: `Default`, `Debug`, `Display`, `std::error::Error`, `From`, `TryFrom`, operator traits, then all other traits alphabetically.

## Derives
Always in this order, leaving out what is not derived: `Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize`.

## Lists of the same things
- An enum's declaration order is canonical: every `match`, table, doc list, serialization, and configuration listing of its variants follows it. An arm of several variants takes the place of its first; `Option` and `Result` arms put `Some` and `Ok` first.
- A struct's field declaration order is canonical: every struct literal, destructuring pattern, manual `Debug`, state hash and checkpoint destructuring, doc table, and serialization follows it.
- The key order of `DEFAULT_FILE` is the key order of `.claude/docs/config.md` and of the configuration struct.
- Module constants and statics are alphabetical, each with its own `///` line, except the tag bytes and bit flags of a written format (`<GROUP>_<VARIANT>`), which follow their values. Where no order carries meaning, the order is alphabetical, comparing names word by word with a shorter word first (`QUEUE_CAPACITY` before `QUEUED_WORDS`).

## Signatures
- Parameter order: receiver, context (`cx`, `&Server`), the subject acted on (or its ID), the value, options and limits, an output buffer (`&mut Vec<_>`) last. Functions with the same verb take the same parameters in the same order.
- Getters have no `get_` and return `&T` or a `Copy` value; a lookup by key in a collection type, and the one value of a wrapper or ID type, is `get`/`get_mut`, as in std. Immutable updates that validate return `Result<Self>` or `Option<Self>`. A mutator other than a setter that may do nothing as a normal outcome returns whether it did (`bool`). Mutable access is `_mut`. Setters are `set_x(&mut self, value)` returning `()` or `Result<()>`. Immutable updates are `with_<field>(self, value) -> Self`. Queries are `is_`/`has_`/`can_`. Conversions are `as_`/`to_`/`into_`/`from_`.
- Absence is `Option`, failure is `Result`; never a sentinel (`-1`, `0`, empty string). Never a `bool` flag argument; use an enum or a separate method.
- Chained methods taking `self` and returning `Self`: one that sets a field is `with_<field>` (above); one that adds an element to a list is named after the element (`button`, `line`, `alias`); one that turns on a switch is named after the state (`optional`, `temporary`), and one that turns off a switch that is on by default is `without_<state>`. A method that lends a value to a closure is named after what it lends, never `with_`. A builder type (`<Thing>Builder`, finished by `build()`) is the one exception: its setters drop `with_` (`ServerBuilder::data_dir`).
- Paths and strings: internal functions take `&Path` and `&str`. Public functions take `impl AsRef<Path>` or `&str` for a value they only read, and `impl Into<PathBuf>` or `impl Into<String>` for one they keep.

## Errors
- Every module that can fail has one hand-written `Error` enum (its alias's rustdoc is "A result with the module's [`enum@Error`].") and exactly this alias, with the module's visibility: `pub type Result<T, E = Error> = std::result::Result<T, E>;`, placed directly after the `Error` type's `impl`s. Development tools under `xtask/` are the exception: their functions return `Result<T, String>`, written out, with no alias and no `Error` enum.
- `Display` is `impl fmt::Display for Error` after `use std::fmt;`, with `f: &mut fmt::Formatter<'_>`. Public errors are `std::error::Error + Send + Sync`. Variants never end in `Error`; the cause is given only through `source()`.
- A variant wrapping a cause is a tuple variant `Variant(Cause)`; conversion is `.map_err(Error::Variant)`, never a closure that only wraps. A variant that carries context besides its cause is a struct variant with the cause in a field named `source`. Only the crate-root `anchor::Error` implements `From`, for the module errors that reach it through `?`.
- `Display` writes fixed text with `f.write_str("…")` in each arm and text with arguments with `write!(f, "…", …)`. A struct variant gets its cause through `.map_err(|source| Error::Variant { …, source })`.
- Messages (errors, `expect`, test assertions, logs) start lowercase and have no final period. An `expect` message states what must hold; the `invariant` tag lives in the `#[expect]` reason, never in the message.

## Logging
- `tracing` with structured fields: `level!(field = value, ..., "message")`. Fields first, the message last, values only in fields. A field named like its variable is written short (`%name`, `?tick`).
- A concept has one field name across the crate (`session`, `player` for the player's name, `world` for the world's name, `addr` for a socket address, `path` for a file path, `error`). An error is `error = &e as &(dyn std::error::Error + 'static)`, so the log writes its causes.
- Levels: error is operator action, warn a recovered anomaly, info startup, shutdown, and connections, debug per connection, trace per packet.
- Secrets (tokens, keys) are never logged. Anchor sets no global subscriber and attaches its default console writer only to its own threads.

## Names
- Rust API Guidelines: types and variants UpperCamelCase; functions, fields, modules, files snake_case; constants SCREAMING_SNAKE_CASE; packages and configuration keys kebab-case.
- Acronyms are one word (`Raknet`, `Nethernet`, `Xuid`, `Uuid`, `Id`, `Nbt`, `Json`, `Http`, `Udp`, `Aabb`); prose keeps the original spelling (RakNet).
- A module name is not repeated in its types (`event::PlayerJoin`). API-table event names drop their `Event` suffix.
- Units are a type (`Duration`, `Tick`) or a suffix (`_ticks`, `_ms`, `_bytes`). Upper bounds are `MAX_<THING>`, lower bounds `MIN_<THING>`, an inclusive range of allowed values `<THING>_RANGE`.
- A `Context` value is named `cx`, and `cx` names nothing else; other context-like values take a name of their own (`attempt`, `settings`).
- One concept has one word across the crate, and one word means one concept.
- API-table conversion: `getX/setX/isX` → `x/set_x/is_x`; singleton → a `&Server` or `Context` argument; `null`, exceptions, `-1` → `Option` or `Result`; constant groups → an enum; mutable `Vector3` → a `Copy` value; twin methods → one.

## Literals and values
- A decimal literal of five or more digits groups thousands with `_` (`16_384`); one of four digits or fewer is never grouped (`4096`). A type suffix is separated by `_` (`0_usize`, `1.5_f32`).
- A literal whose meaning is not obvious at its use site, or that several places share, is a named constant.
- Constants and statics are module items or associated constants, never declared inside a function body.
- A constant that describes one type's values is an associated constant of that type; every other constant is a module `const`. A closed set of related values is an enum, never a group of constants; the tag bytes of a written format are module constants `<GROUP>_<VARIANT>`, since the enum rule covers Rust values, not bytes.
- A `String` from a string literal is `"…".to_owned()`. An empty vector is `Vec::new()` or `Vec::with_capacity(n)`; `vec![…]` only with elements. A default value names its type (`Type::default()`, `..Type::default()`); `Default::default()` only where the type has no name to write (arrays, tuples).

## Control flow
- An early exit on a pattern that does not match is `let … else`.
- A loop run for its effects is a `for` loop, never `for_each`; an iterator chain only computes a value.

## Comments and documentation
- Comments say only what the code cannot: reasons, ordering, the trap avoided, the invariant kept. Never restate the code; never write history, work process, previous states, authors, or schedules.
- A wrong comment is worse than none; changing code changes its comments in the same change.
- A `// pmmp-class: Name` line sits after the item's attributes, directly above the item.
- Outside explanation points to `.claude/docs/<file>.md` by file name only, never by section.
- The first line of every rustdoc is one sentence ending with a period. Functions start with a third-person verb ("Reads", "Sets"); types, fields, variants, constants, constructors, getters, `_mut` lenders, and queries (methods and free functions that only read) with a noun phrase ("A new empty chunk.", "The chunks written.", "The chunk, to change it.", "Whether the file is new."). A test's rustdoc states the behavior it checks, never "Checks that".

## Language and names in documents
- Identifiers, comments, rustdoc, logs, `CLAUDE.md`, and everything under `.claude/` are English. Reports to the user are Korean (`CLAUDE.md`).
- Documents never name external servers. Use "the official server" (the server program Mojang distributes), "the dump" or "the dump source" (data source), "the API table" (API parity), "the reference list source". Names stay only where unavoidable: the pinned-source and `.cache/` tables in `.claude/docs/data.md`, the configuration migration table in `.claude/docs/config.md`, literal paths, commands, and markers (`xtask/data/pmmp/`, `.cache/bds`, `p6 bds`, `// pmmp:`), and Go library names (gophertunnel, go-raknet, go-nethernet). This rule covers documents (`CLAUDE.md`, `README.md`, `.claude/`, `xtask/data/p6/facts.md`) only; code comments, tool data (`intended.txt`, `deferred.txt`, `fields.txt`), and generated files are outside it.

## Tests
- Unit tests are `#[cfg(test)] mod tests`, the last item of the file, whose first line is `use super::*;`; other imports follow after one blank line in the three groups. A test module's own `mod` declarations come before `use super::*;`, and its constants and statics directly after its imports. They move to `<module>/tests.rs` only when longer than the code they test; such a file follows the file layout, with `use super::*;` in the crate group. Public API tests are `tests/<topic>.rs`, which follow the file layout too, a helper directly above its first user.
- In a test module, a helper sits directly above the first test or helper that uses it.
- Test names describe the behavior as a phrase (no `test_` prefix). Tests fail through `unwrap`, `expect`, `assert*` and `panic!`. Equality is `assert_eq!(actual, expected)`, actual first. A test's `// pmmp:` line comes before its `///` documentation; a line past 100 columns is split into several lines, each starting with `// pmmp:`.

## Lints and formatting
- The canonical sources are `[workspace.lints]` in Cargo.toml, `.clippy.toml`, and `.rustfmt.toml`. Formatting is rustfmt only. Exceptions follow `CLAUDE.md`.
- The `.clippy.toml` deny lists block clocks, randomness, blocking, unbounded queues and allocation, and nondeterministic math in tick code. Only off-tick code and lookup-only index types lift them, with the `off-tick` tag, on the smallest item that actually calls the listed item. The foundation modules that own the job (`clock`, `pool`, `fs`) lift them with the tag matching the reason.

## Generated files
The first line is `// @generated by cargo xtask gen; do not edit.`. Never edit a generated file; change its generator (`xtask/src/gen*`) and run `cargo xtask gen`.
