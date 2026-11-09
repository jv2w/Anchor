# Protocol

Rules are in `.claude/rules/protocol.md`.

This document holds the sources of truth and pinned versions, the pipeline from the IR to the generated code, the decode limits, the gophertunnel defects, and tests P1 to P5. Transport, login, encryption, and compression are in `network.md`; the official-server comparison (P6) in `testing.md`; block and item data in `data.md`; dependency review in `dependencies.md`.

## One version at a time
- Supported: the protocol number and game version of the pinned gophertunnel (`protocol::packet::PROTOCOL_VERSION`, `GAME_VERSION`, generated from the IR's `source`). Authentication discovery builds its URL from `GAME_VERSION` (`src/auth/fetch.rs`).
- Why only one: every further version multiplies the codecs, the block tables, and the tests, costing consistency and simplicity.
- What a client of another protocol number gets is in `network.md`.

## Sources and pinned versions
- The IR's `source` records the gophertunnel version and commit of `xtask/go/go.mod`. The extractor pins both (`version` and `commit` in `xtask/go/extract/main.go`) and fails when go.mod loads another version.

| Module | Where it is pinned |
|---|---|
| github.com/sandertv/gophertunnel | `xtask/go/go.mod`; `version` and `commit` in `xtask/go/extract/main.go` |
| github.com/sandertv/go-raknet | `xtask/go/go.mod`: the version in gophertunnel's go.mod |
| github.com/df-mc/go-nethernet | `xtask/go/go.mod`: a release that fixes the defect of gophertunnel's version (`dependencies.md`) |
| Mojang bedrock-protocol-docs (field limits) | `PROTOCOL_DOCS` in `xtask/src/dump.rs`: the release commit of the supported game version |

Two comparison sources serve only to judge gophertunnel defects and never enter the code. Their pins are in the pinned-source table of `data.md`.
- **The schema source**: a published protocol schema built from reverse-engineered official-server headers and a cereal schema dump, the public source closest to the official server. It has no golden bytes for shapes after protocol 2193.
- **The codec library**: an independent protocol codec library, at its codec for the supported protocol.

- Why a tag and not a later commit: no gophertunnel master commit after the pinned tag fixes a format of the supported protocol.
- The official server version used for comparison is in the pinned-source table of `data.md`.
- Passing P3 means only agreement with gophertunnel, not correctness.

## Pipeline: Go extraction → IR → Rust generation

```
gophertunnel source + xtask/go/fix ──(xtask/go/extract)──> xtask/data/protocol.json ──(cargo xtask gen)──> src/protocol/{types,packet,consts}.rs
Mojang schemas ──(cargo xtask dump)──> xtask/data/reference/protocol_limits.json ──┘
```

### gophertunnel corrections (`xtask/go/fix`)
- The codec and constant corrections among the gophertunnel defects below replace text that occurs exactly once in a file of a copy of the pinned module. Text that is missing or occurs more than once fails, so a version change shows every stale correction.
- The extractor calls `fix.Module` itself; the oracle and the bot build with `-modfile` after `go run ./fixmod -out <dir>`. The IR, P1 to P5, and the P6 bot therefore see the same corrections.

### Extractor (`xtask/go/extract`)
- Loads gophertunnel's `protocol` and `packet` packages with type information through `golang.org/x/tools/go/packages` and walks each packet's and type's `Marshal(io protocol.IO)` into the IR.
- Why it fails on an unknown construct: a field skipped silently becomes a wrong codec.

### IR (`xtask/data/protocol.json`)
- Its schema, and its only documentation, is `xtask/go/extract/ir.go`.

### Generator (`cargo xtask gen`, `xtask/src/gen*`)
- `gen` runs the extractor into the IR and generates `src/protocol/types.rs` (the types of gophertunnel's `protocol` package), `src/protocol/packet.rs` (the packets, the `Packet` enum, `PROTOCOL_VERSION`, `GAME_VERSION`), and `src/protocol/consts.rs` (enums and constants from the IR's const declarations). The same run generates the data tables (`data.md`).
- `cargo xtask gen --check` extracts the IR into `target/xtask-gen/`, regenerates every file, and compares the IR and every generated file byte for byte with the committed ones. `cargo xtask verify` runs it first. Without Go on PATH, it and P1 to P5 fail.
- Names follow `xtask/src/gen/names.rs`.

### Rust form
- Generated structs match the Go structs one to one, which keeps P3 judgments and version-change diffs simple.
- Mapping: `Optional` → `Option`, slice → `Vec`, union interface → enum, `mgl32.Vec3` → `[f32; 3]`, `uuid.UUID` → `Uuid`, NBT compound → `BTreeMap<String, nbt::Value>` (keys written in byte order, a repeated key keeps its last value, as gophertunnel's Go map does), EntityMetadata → `BTreeMap<u32, _>` (gophertunnel also writes its keys sorted). The types written by hand and the external types are `HAND_TYPES` and `EXTERNAL_TYPES` in `xtask/src/gen/model.rs`.
- Strings are `String`; gophertunnel does not check UTF-8.
- Fields of which the server sends one value to many players (`PlayerListEntry.skin`, `PlayerSkin.skin`, `UpdateTrade.serialised_offers`: `SHARED_FIELDS` in `model.rs`) are `Arc<T>`, so the tick shares a skin of up to 4 MiB or a villager's offers instead of copying them into every packet. Decoding takes that allocation from the budget too.
- Byte fields that gophertunnel reads and writes raw but that hold one network little-endian NBT root (`UpdateTrade.SerialisedOffers`: `NBT_FIELDS` in `model.rs`) are generated as NBT compounds, so the tick hands over the compound and the output phase encodes it. The wire bytes are the same; the corpus generator (`xtask/go/oracle/random.go`) fills such a field with an NBT root.
- Because an NBT list keeps its element type even when empty, every list writes back to the same bytes; gophertunnel keeps the type only for empty `TAG_Int` and `TAG_Long` lists and writes other empty lists as `TAG_End`. Reading and writing keep NBT nesting on a heap stack, so the thread stack stays flat at any depth; dropping, cloning and comparing a value recurse, which the NBT depth limit (below) bounds.
- Why `anchor::protocol` is public: packet hooks handle packet values, and the xtask tests call this layer. Storage uses NBT too.

### Hand-written codecs
The `protocol.IO` methods that are not primitives have no Marshal body to extract and are written by hand (under `src/protocol/` unless another path is given):
- vectors, positions, UUIDs, colors, and bitsets (`common.rs`);
- NBT (`src/nbt.rs`) and entity metadata (`entity_data.rs`);
- items, item instances, stack request items, and MaterialReducer (`inventory.rs`);
- game rules, ability values, and pack settings (`values.rs`);
- the player auth input flag list, and the flag sets of the generated flag groups (`flags.rs`);
- the tag frames of the unions: stack request actions, events, inventory transaction data, shape data, item descriptor counts, and player inventory actions (`union_frame.rs`).

Login's connection-request envelope (`login.rs`) and the network sub-chunk payload (`block_storage.rs`) are hand-written too.

### Batch format (`src/protocol/batch.rs`)
- Packet header, a varuint32: the packet ID in bits 0 to 9, the sender sub-client in bits 10 and 11, the target sub-client in bits 12 and 13. Reading ignores higher bits, like gophertunnel: only official clients show whether they set them, so the reading side accepts every case.
- A batch holds at most `MAX_PACKETS` packets (gophertunnel `maximumInBatch`). Encoding starts a new payload at `MAX_PACKETS` packets and before a packet that would take it past `SPLIT_BYTES`, so only a payload of one packet is larger. The compressed and decompressed size limits are in `network.md`.

## Decode limits
- A declared length is at most the remaining input. A list count is at most 1024 (gophertunnel `maxSliceLength`; the EntityMetadata count included) and at most the remaining input.
- Allocation budget: one batch shares 2 × `max-batch-bytes`, the decoded packet list included. Why: count limits alone let nested lists and large elements allocate tens of times the input size.
- Maps (NBT compounds, `EntityMetadata`) take their `BTreeMap` nodes from the budget, not only their entries (`map_charges` in `src/protocol/io.rs`): the first key one node (11 entries, counted as an internal node with 12 child links), each further key a fifth of a node. While a map only grows, every node but the root holds at least 5 entries, so n keys take at most 1 + (n − 1)/5 nodes. The NBT encoder counts the same amount (`Cost.alloc`), so storage splits its pages at what reading them back takes (`src/storage/codec.rs`).
- NBT: depth at most `nbt::MAX_DEPTH`, the root included; one root at most `nbt::MAX_ROOT_BYTES` on the wire.
- Lists nest at most 64 deep, a packet's own lists included (`MAX_LIST_DEPTH`). The decoder of the self-containing type `DataStorePropertyValue` recurses on the thread stack once per list; without the bound, one packet overflows the stack and ends the process.
- Per-field limits:
  - `cargo xtask dump` reads the schemas at the pinned commit and writes `xtask/data/reference/protocol_limits.json` through the table in `xtask/src/dump/limits.rs` that joins each schema limit to the gophertunnel fields it limits. Strings and byte slices count bytes, lists count elements; a key ending in `[]` limits each element of its list.
  - A schema limit without a table row, or a row without a schema limit, fails `dump`; a limit that no decoder applies fails `gen`. A version change therefore shows every limit that came or went.
  - Decoding a list applies min(field limit, 1024). The encoder fails at the field limit, so Anchor never sends a packet past the schema. 1024 is a receiving limit only and is not applied when encoding: the server sends lists of more than 1024 entries, such as ItemRegistry and CreativeContent.
- The sub-chunk payload decodes strictly: only bit widths 0, 1, 2, 3, 4, 5, 6, 8, and 16; the persistent palette form and the header 0x7f are refused; a palette is not empty and holds at most 2^bits entries; every index points into the palette.

## gophertunnel defects
Defects found in the pinned gophertunnel, with what Anchor does about each:
- MaterialReducer: the Reader reads the input item's network ID as `mix << 16` (it should be `mix >> 16`) and its metadata as `mix & 0x7fff`, which drops bit 15, so a value does not survive a round trip. The corpus therefore uses only mix values 0 to 0x7fff.
- Event types 8, 9, 10, 12, 14, 20, 21, and 22 (AgentCommand, AgentCreated, PatternRemoved, FishBucketed, PetDied, MovementAnomaly, MovementCorrected, ExtractHoney) have no ordinal, so they can be neither read nor written. Ordinal 21 is shared by three events (StriderRiddenInLavaInOverworld, SneakCloseToSculkSensor, CarefulRestoration).
- Values overwritten on write, or read and discarded: ItemDescriptorCount aux 32767, the StackRequestAction legacy byte, Skin `trusted`, the name strings that PlayerUpdateEntityOverrides and ResourcePackClientResponse write from their numeric type, InventoryTransaction `hasLegacy`.
- No bound on the EntityMetadata count, a leak in the NBT depth count, partial reads accepted, ItemInstance and Login lengths truncated silently.
- Snappy decompression is `s2.Decode`, so it also accepts the S2 extension.
- Skin tint piece names: the codec removes only `persona_` from the login names and so sends `facial_hair`, `face_accessory`, `left_leg`, `right_leg`, `left_arm`, `right_arm`, and `classic_skin`. Mojang's documents (the `PieceTintColors` keys of `SerializedSkin` are `persona::PieceType` names) and the codec library use lowercase names without underscores (`facialhair` and so on). The generated codec stays as gophertunnel's; the login conversion (`TINT_PROTOCOL_NAMES` in `src/login/client_data.rs`) puts in the value from which the codec writes that name. Not yet confirmed against official-server bytes (P6).
- `PieceType` lacks `CoCo` (28), which the supported game version added; gophertunnel has `Unsupported` at 28. The schema source (`COCO = 28`, `UNSUPPORTED` 29), the codec library, and Mojang's documents agree. Corrected: `CoCo` is 28 and `Unsupported` 29, and the login maps `persona_coco` to 28.
- ClientboundUpdateSoundData's `SoundDataUpdate`: gophertunnel writes only the type byte. The schema source (the variant selector is a uvarint32 variant index, and the variant writes its uint8 type again), the codec library (the same shape, checking that both are equal), and Mojang's documents (`oneOf` with a control uint32, `Type` first in each variant) all put the variant index first. Corrected: the variant index (equal to the type) as uvarint32, then the type as u8; a mismatch is a decode error.
- EntityDataKey 139 to 143: gophertunnel has `ArrowShooterID` 139, `FireworkDirection` 140, `FireworkShooterID` 141, `Reserved139` 142, and `NameplateRenderDistanceMax` 143 (the name `Reserved139` on 142 already contradicts itself). The schema source (`RESERVED_139` 139, `NAMEPLATE_RENDER_DISTANCE_MAX` 140) and the codec library (v975: 139 Long, 140 Float, the arrow and firework shooter keys at 17 and 18) agree. The official server sends players key 140 as Float 64.0 (the name tag distance) and key 141 as Byte 0, a key without a public name, and sends 139 (Int64 0) only in another player's AddPlayer (P6, `xtask/data/p6/facts.md`). Corrected: 139 `Reserved139`, 140 `NameplateRenderDistanceMax`, 141 `Reserved141`; `ArrowShooterID` and `FireworkDirection` are aliases of 17 and `FireworkShooterID` of 18.
- Login `ServerAddress`: the pinned tag's `ClientData.Validate` accepts a NetherNet address only as `http(s)://host:port:port`. Clients of the supported version send `https://host:port`, and master after the tag (`531fd12d9a`) accepts both. Anchor follows master (`is_server_address` in `src/login/client_data.rs`).
- Empty skin geometry: the login `Validate` and the codec accept and send an empty `SkinGeometry`. The protocol requires the geometry and the resource patch to be depth-bounded JSON (Mojang's documents). Anchor turns an empty login geometry into `{}` (`NO_CUSTOM_GEOMETRY`), and the skin check (`Skin::is_acceptable` in `src/tick/queue.rs`) requires both fields to be JSON nested at most 64 deep.
- Skin sizes: `Skin.validate` multiplies the image height and width as `uint32`, so a product past 2^32 wraps and a size that does not match its data passes. Corrected in `xtask/go/fix`: the check compares in `uint64`.
- No login deadline, no bound on the deferred packet queue, no bound on raw 0xff payloads: Anchor's session has all three (`network.md`).

## Tests P1 to P5
P6 is in `testing.md`.

- The Go oracle (`xtask/go/oracle`):
  - `corpus`: makes samples by running each packet's Marshal on a `protocol.IO` that picks random values as it writes (PCG seeded with the seed and the packet ID, list lengths 0 to 4, `-limits` keeping every sample within the field limits). It writes with gophertunnel's plain Writer, except NBT roots, which its own encoder writes sorted because Go map order is random. It keeps only the samples gophertunnel's Reader reads back. The tests build the corpus under `target/`.
  - `check`: decodes the bytes it receives with gophertunnel and returns the re-encoding; Rust compares the bytes.
  - `compress`, `decompress`: the zlib and Snappy of gophertunnel's `packet` package.
  - Both sides use the shield item network ID 358.
- P1, complete decode (`xtask/tests/protocol.rs`): 64 corpus samples per packet; every sample decodes in Rust with no bytes left. The IR packet count equals the union of gophertunnel's two pools and the `Packet` variant count (244).
- P2, round trip: `encode(decode(b)) == b` for every sample b. For every P4 mutation m that decodes, with `e = encode(decode(m))`, `encode(decode(e)) == e` (a byte comparison, so NaN passes too).
- P3, agreement with gophertunnel both ways: (a) Rust re-encodes the bytes Go produced to the same bytes. (b) For the Rust encoding R of every decoded mutation, R → Go decode → Go re-encode → Rust decode → Rust re-encode gives R. The only differences allowed are the element type of an empty NBT list, compared by value structure, and the CraftingData MaterialReducer input that gophertunnel reads wrongly (above), each reproduced exactly (`is_known_difference`). Batch compression (`xtask/tests/compression.rs`): for zlib and Snappy each, Rust compression → Go decompression and Go compression → Rust decompression give back the original (sizes 0, 1, 255, 256, 64 KiB, and 1 MiB; random and repetitive data).
- P4, mutated input: 1000 fixed-seed mutations per packet (1 to 3 stacked bit flips, byte inserts and deletes, truncations, and varint inflations) never panic. Five malformed Snappy bodies, Snappy declared lengths over the limits, and zlib and Snappy bodies inflating past `max-batch-bytes` are errors. The other decoders of external input (RakNet datagrams, signaling requests, SDP, NetherNet peer datagrams) each take at least 10,000 fixed-seed mutations without a panic.
- P5, bot join (`xtask/tests/spawn.rs`, whose module documentation lists every case): a Go bot (`xtask/go/bot`) built on gophertunnel's `packet` and `login`, go-raknet, and go-nethernet spawns on an Anchor server started in the same process, over RakNet and NetherNet, with zlib and Snappy each. The test issuer is an OpenID configuration, a JWKS, and tokens made from its own key.
- `xtask/tests/go.rs` runs the Go tools' own tests (`go test ./...`).
