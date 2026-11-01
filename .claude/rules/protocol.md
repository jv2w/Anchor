---
paths:
  - "src/protocol.rs"
  - "src/protocol/**"
  - "src/nbt.rs"
  - "xtask/src/gen.rs"
  - "xtask/src/gen/body.rs"
  - "xtask/src/gen/consts.rs"
  - "xtask/src/gen/emit.rs"
  - "xtask/src/gen/ir.rs"
  - "xtask/src/gen/model.rs"
  - "xtask/src/gen/names.rs"
  - "xtask/src/dump.rs"
  - "xtask/src/dump/limits.rs"
  - "xtask/data/protocol.json"
  - "xtask/data/reference/protocol_limits.json"
  - "xtask/go/go.mod"
  - "xtask/go/go.sum"
  - "xtask/go/extract/**"
  - "xtask/go/fix/**"
  - "xtask/go/fixmod/**"
  - "xtask/go/oracle/**"
  - "xtask/go/bot/issuer.go"
  - "xtask/go/bot/login.go"
  - "xtask/tests/protocol.rs"
  - "xtask/tests/compression.rs"
  - "xtask/tests/spawn.rs"
  - "xtask/tests/go.rs"
---
# Protocol rules

## Sources of truth
- Base every protocol fact only on the pinned gophertunnel code, Mojang's documents, and values the bot observes from the official server.
- Add the comparison sources (the schema source and the codec library, `.claude/docs/protocol.md`) to the evidence only when judging a gophertunnel defect.
- When gophertunnel disagrees with the official server (P6), handle it with the distrust procedure of `.claude/rules/dependencies.md`.

## Generated code
- Generate the packet codecs from the IR extracted from the pinned gophertunnel.
- Make the extractor (`xtask/go/extract`) fail on any construct it does not understand. Never skip one.
- Commit the IR (`xtask/data/protocol.json`). Never edit it by hand.
- Fix wrong generated output in the generator (`xtask/src/gen*`), the extractor, or the corrections of `xtask/go/fix`.
- List every IR constant group the generated code leaves out in `SPECS` of `xtask/src/gen/consts.rs` as `Excluded`, with its reason.
- Keep the naming rules in `xtask/src/gen/names.rs` alone. Put a name the rules do not give into the `EXCEPTIONS` table of that file.

## Rust form
- Generate each Go struct as a flat Rust struct of the same shape. Keep conditional fields always present, holding `Default` when absent. Make every generated struct derive `Debug, Clone, PartialEq, Default`.
- Give a better public shape in the higher-level API that wraps the generated type.
- Fail decoding on a string that is not UTF-8 and on a bool byte other than 0 or 1.
- Never truncate a length silently when writing; return an error (encoding returns `Result` too).
- Keep the element type of every NBT list (`nbt::List`), empty lists included.
- Keep NBT nesting on a heap stack, never on the thread stack.
- Keep `anchor::protocol` the public codec layer: packets and types, `Packet` decode and encode, batch encode and decode, `Compression`. Keep `Reader`, `Writer`, and `Codec` `pub(crate)`. Keep NBT in `anchor::nbt`.
- Generate the variant structs and ID tables of unions. Hand-write only the frames that read and write their tags (`src/protocol/union_frame.rs`).

## Decode limits
- Fail with the size error (`Error::Size`) past any decode limit. Route every length, count, and field limit through the one `Reader::limit`. List nesting, NBT depth and size, and the packets per batch are counted by their own decoders.
- Take `count × size_of` from the reader's allocation budget before every `Vec`, `String`, and NBT allocation. Make a nested reader share its parent's budget.
- Take per-field limits from the `maxLength`, `maxItems`, and `maxProperties` of the JSON schemas of Mojang's bedrock-protocol-docs.
- Make `gen` apply every field limit in both the decoder and the encoder.
- Give every limit (length, count, budget, NBT depth and size, decompressed size, packets per batch, field limit) a P4 test in which a declaration past the limit ends in a size error.

## Fixing gophertunnel defects
- Fix defects of the pinned gophertunnel in Anchor. Where Anchor must keep gophertunnel's format, align the test samples instead.
- Correct gophertunnel's codec or constants only when the schema source and at least one independent source (Mojang's documents, the codec library, or an observation of the official server) agree against gophertunnel.
- Add a codec or constant correction to the replacement table of `xtask/go/fix` and run `cargo xtask gen`. Build every Go tool against the corrected module that `xtask/go/fix` writes.
- Read MaterialReducer as `mix >> 16` and `mix & 0xffff`. Make a write out of range an error.
- Make reading and writing an Event type without an ordinal an error.
- Keep the values gophertunnel overwrites when writing, or reads and discards, the same in the generated code as in gophertunnel. The tick code that handles the client packet checks them.
- Bound the EntityMetadata count, the NBT depth count, partial reads, and the ItemInstance and Login lengths with limits and errors.

## Tests
- Use the Go tools only in development. Make every test run under `cargo test`, so that `cargo xtask verify` includes it.
- Never commit the oracle corpus.
- Never give the bot a real Xbox token. Its online login uses the test issuer (`.claude/rules/network.md`).

## Moving to a new protocol version
Do these steps in order. Start a step only after the one before it passes.
1. Choose the source: the latest release tag of gophertunnel, never a preview branch. Read master's commits after the tag; if one fixes a format of the same protocol, pin that commit. Take go-raknet and go-nethernet from gophertunnel's go.mod, unless a release fixes a defect of that version; then pin that release (`.claude/docs/dependencies.md`).
2. Review: apply the distrust procedure (`.claude/rules/dependencies.md`) to the changed Go code (gophertunnel's `protocol`, `packet`, `login`, and compression code, go-raknet, go-nethernet): read it, record its defects, and remove them. Check that the gophertunnel defect list in `.claude/docs/protocol.md` still holds.
3. Pin: write the Go versions into `xtask/go/go.mod` and `xtask/go/go.sum` as a tag or a pseudo-version, and set `version` and `commit` in `xtask/go/extract/main.go` to the same values.
4. Generate: run `cargo xtask gen`. When extraction fails on an unknown construct, extend the extractor. Read the IR diff (`xtask/data/protocol.json`) for new, changed, and removed packets, fields, unions, and constants, and align the hand-written codecs and the engine code that uses them.
5. Field limits: set `PROTOCOL_DOCS` in `xtask/src/dump.rs` to the bedrock-protocol-docs release commit of the same version and run `cargo xtask dump`. When `dump` or `gen` fails on a limit, fix the table in `xtask/src/dump/limits.rs`.
6. Test: `cargo xtask verify`, P1 to P5 included, must PASS. It runs only when the user orders `/verify` (`CLAUDE.md`).
7. Data: move the dump source and the reference list sources in the pinned-source table of `.claude/docs/data.md` to the same version, and align the state comparison and the upgrade schemas (`.claude/rules/data.md`).
8. Compare with the official server: run all of P6 against a copy of the new official server in `.cache/bds/<version>/`, and follow the official server wherever they differ (`.claude/rules/testing.md`).
9. Publish: treat the version change as a breaking change of the public API (semver). Update the supported version and the pinned-version table in `.claude/docs/protocol.md`.

Background and detailed facts are in `.claude/docs/protocol.md`.
