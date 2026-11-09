---
paths:
  - "src/block.rs"
  - "src/block/**"
  - "src/item.rs"
  - "src/item/**"
  - "src/effect.rs"
  - "src/effect/**"
  - "src/world/biome.rs"
  - "src/world/biome/**"
  - "src/world/chunk.rs"
  - "src/world/sub_chunk.rs"
  - "src/world/connect.rs"
  - "src/import/upgrade.rs"
  - "src/import/upgrade/**"
  - "src/tick/update.rs"
  - "src/tick/interact.rs"
  - "src/tick/interact/breaking.rs"
  - "src/tick/interact/place.rs"
  - "src/tick/interact/using.rs"
  - "src/tick/block_entity.rs"
  - "src/tick/block_entity/**"
  - "xtask/src/gen.rs"
  - "xtask/src/gen/**"
  - "xtask/src/dump.rs"
  - "xtask/src/dump/**"
  - "xtask/php/**"
  - "xtask/go/reference/**"
  - "xtask/data/dump/*.json"
  - "xtask/data/reference/**"
  - "xtask/data/upgrade/**"
---
# Block and item data rules

## Sources
- Never write block, item, creative inventory, effect, or enchantment tables by hand. Commit what running or converting the pinned sources produces to `xtask/data/`, and generate the tables under `src/` from it with `cargo xtask gen`.
- Take block and item kinds, states, and serialization from the dump (the sources table in `.claude/docs/data.md`).
- Take version-specific numbers (block network IDs, item runtime IDs) from the reference list. Use the reference list only to check and number, never to define a block. Exception: the item component values clients receive (`use_duration`) also drive behavior.
- Move reference list source A's pin to a release when one ships.

## Dump and PHP scripts
- Keep `dump` writing the reference list (`xtask/go/reference`) before it runs the dump source.
- Run the PHP scripts (`xtask/php/`) with the dump PHP of the sources table, never plain PHP, with `.cache/dump` as the working directory.
- Make PHP script output sort every list and write one entry per line, so the same checkout writes the same bytes.

## Dump source defects
- Leave out of the creative inventory the empty enchanted books the dump source makes by dropping enchantments it does not know.
- Correct the ominous banner to the official server's value, in the one place every dumped item passes (`corrected_meta` in `xtask/src/gen/data.rs`).
- Where an item name has exactly one dump block item and `item_blocks.json` gives it one vanilla creative state, send the block item with that vanilla state. The item still places the dump state.
- Count creative item network IDs from 1, as the official server does.
- Correct the effect colors that differ from the official server's in the one table `OFFICIAL_COLORS` (`xtask/src/gen/effects.rs`).

## Reference list
- Keep `xtask/go/reference` failing on any difference between reference list sources A and B in block states (name, property names, types and values, version) and items (name, runtime ID, version, component flag).
- Use the reference list's block network IDs. Use the hash function only for custom blocks.
- Use only the biome definitions without chunk generation data.
- Dump states that map to one Bedrock state may differ only in data the state does not hold (a bed's or banner's color, which the block entity holds; a cauldron's fill, which Anchor does not keep), and only for the Bedrock states in `SHARED` (`xtask/src/gen/data.rs`).
- Use the official server's light dampening only for states the dump has no type for.
- Never use the official server's `isSolid`. Solidity comes from collision boxes.
- Never send the data-driven block definitions the official server puts in StartGame.

## Generated tables
- Make every generated table a `static`.
- Work out the 1.26.50 properties (`minecraft:connection_*`, `minecraft:corner`) on the server from the neighbours. The state table holds all their values.
- The state table also holds all values of the 1.26.60 properties (`minecraft:connection_*` of fire and chorus plants, `redstone_*` of redstone wire), but the server never works them out and leaves them at their default.
- Upgrade the dump states with the added properties of every block schema after the dump's data version (`ADDING` in `xtask/src/gen/upgrade.rs`).
- `dump` fails on a reference state without block physics, except under its two temporary rules (`.claude/docs/data.md`). Delete those rules when reference list source B updates its physics.
- Keep `gen` failing on each of these:
  - an upgraded dump state that is not in the reference list;
  - an upgrade entry for a block that neither the dump nor the reference list has;
  - dump states of one Bedrock state that disagree outside `SHARED`, or a `SHARED` entry no group needs;
  - an item name the reference list lacks, or a creative item that resolves to no item type;
  - a vanilla creative state of an item with a single dump block item type that does not read (state, else alias) as a block whose item is that item;
  - a reference state the dump source reads as itself that is not an Anchor state, an Anchor state it reads as another state, or a reference state it reads as a state with no Anchor state.

## Unknown blocks and states without a dump type
- Keep a stored block state Anchor does not know as its original NBT.
- When a client sends a block or item Anchor does not know, reject only that request.
- Hold every reference state in the one block table. A state the dump has no type for (kind `InfoUpdate`, classes `Opaque` and `Block`, no item, no connection) has the official server's light dampening and the physics of `block_physics.json`: solid with full support on every face when its only collision box is the whole cell, otherwise transparent with no support. Its break info is the hardness, five times the hardness as blast resistance, and every tool type. It is an ordinary `block::State` everywhere: events carry it, it breaks like any block, explosions break it, and it drops nothing. A state outside the table is `Entry::Unknown` and goes to clients as `minecraft:info_update`.
- Keep a block state outside the table as stored in a block entity too: a flower pot plant (`PlantBlock`) Anchor does not know stays as it came and never goes to clients. A banner pattern ID Anchor does not know stays and goes to clients.
- The public API shows only blocks and patterns Anchor knows.
- Send a block entity whose kind Anchor does not type (`Kind::Other`) to clients as stored when it has an `id`.
- Drop items Anchor does not know from a saved inventory, block entity, or entity through `inventory::read_saved_stack`, which warns with the owner and says the next save loses the item for good (renaming or removing a custom item ID loses its stored items).

## Upgrade schemas
- Use only the generated constant `BLOCK_STATE_VERSION` as the block state version.
- In a group of schemas that share a state version, treat the first as already applied and skip it.
- Keep `gen` failing when the newest block schema's version differs from the reference list's (`xtask/data/reference/blocks.json`), or when a reference state does not upgrade to itself.
- Keep `gen` rejecting, within one block schema, an ID both renamed and flattened, a property rename whose result depends on rename order, and a byte value above 127.
- Keep a legacy value the 1.12.0 table lacks as an unknown block: a numeric terrain value as `{id, val}`, a `{name, val}` palette entry as it came.
- Treat a state whose version is above the newest schema's as a state of the newest version.
- Keep `gen` checking that item schema IDs are lowercase, item schema metas are numbers, and every legacy ID map is one to one.
- Make `dump` store every copied schema file with LF line ends.
- Share the rule interpreter (`src/import/upgrade/apply.rs`) with `gen`, which includes it by path. Never copy it.
- Share the network hash (`src/block/hash.rs`) with `dump`, which includes it by path. Never copy it.

## Rebuilding `.cache/`
- Builds, tests, and `verify` run on committed data alone, except the dimpl copy's tests, which need Perl, and the tests that read reference list source B, which skip their check without `.cache/` (`.claude/docs/data.md`).
- Rebuild `.cache/` from the repository root in this order. `<origin>` and the SHA-256 values are in the `.cache/` table of `.claude/docs/data.md`; `<commit>` and the official server version are in the constants its sources table names (bedrock-protocol-docs: `PROTOCOL_DOCS` in `xtask/src/dump.rs`).
  1. The six git checkouts, each: `git init .cache/<folder>`, `git -C .cache/<folder> remote add origin <origin>`, `git -C .cache/<folder> fetch --depth 1 origin <commit>`, `git -C .cache/<folder> checkout --detach FETCH_HEAD`.
  2. The dump PHP: download the release zip, check its SHA-256, and unpack it into `.cache/dump-php/` so that `bin/php/php.exe` and `bin/php/php.ini` are there.
  3. The dump source's dependencies: in `.cache/dump`, run composer with the dump PHP: `composer install --no-dev --ignore-platform-reqs`. This puts the dump source's `composer.lock` versions into `.cache/dump/vendor`.
  4. The official server: download the zip for the OS, check its SHA-256, and unpack it into `.cache/official/<version>/` (on Linux as `.claude/docs/testing.md` says).
  5. Perl: on Windows, download the Strawberry Perl portable zip, check its SHA-256, and unpack it into the folder of the `.cache/` table. On Linux, install the system packages `.claude/docs/testing.md` lists.
  6. The str0m captures: `cargo xtask vendor-test str0m` downloads them.
  7. Confirm that `cargo xtask dump --check` passes.

Background and detailed facts: `.claude/docs/data.md`.
