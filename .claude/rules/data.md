---
paths:
  - "src/block.rs"
  - "src/block/**"
  - "src/item.rs"
  - "src/item/**"
  - "src/crafting.rs"
  - "src/crafting/**"
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
  - "src/tick/interact/inert.rs"
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
  - "xtask/data/pmmp/*.json"
  - "xtask/data/pmmp/liquid.txt"
  - "xtask/data/reference/**"
  - "xtask/data/upgrade/**"
---
# Block and item data rules

## Sources
- Never write block, item, recipe, creative inventory, effect, or enchantment tables by hand. Commit what running or converting the pinned sources produces to `xtask/data/`, and generate the tables under `src/` from it with `cargo xtask gen`.
- Take block and item kinds, states, and serialization from the dump (the sources table in `.claude/docs/data.md`).
- Take version-specific numbers (block network IDs, item runtime IDs) from the reference list. Use the reference list only to check and number, never to define a block. Exception: the item component values clients receive (`use_duration`) also drive behavior.
- Reference list source A is pinned to an unmerged pull request; move the pin to a release when one ships.

## Dump and PHP scripts
- `dump` writes the reference list (`xtask/go/reference`) before it runs the dump source, which reads every reference state.
- Run the PHP scripts (`xtask/php/`) with the dump PHP of the sources table, never plain PHP, with `.cache/pmmp` as the working directory.
- PHP script output sorts every list and writes one entry per line, so the same checkout writes the same bytes.

## Dump source defects
- Leave out of the creative inventory the empty enchanted books the dump source makes by dropping enchantments it does not know.
- Correct the ominous banner to the official server's value, in the one place every dumped item passes (`corrected_meta` in `xtask/src/gen/data.rs`).
- Where an item name has exactly one dump block item and `item_blocks.json` gives it one vanilla creative state, send the block item with that vanilla state. The item still places the dump state.
- Count creative item network IDs from 1, as the official server does.

## Reference list
- Reference list sources A and B hold exactly the same block states (name, property names, types and values, version) and items (name, runtime ID, version, component flag); `xtask/go/reference` fails on any difference.
- Use the reference list's block network IDs. Use the hash function only for custom blocks.
- Use only the biome definitions without chunk generation data.
- Dump states that map to one Bedrock state may differ only in data the block entity holds (a bed's colour, a banner's colour), and only for the Bedrock states in `SHARED` (`xtask/src/gen/data.rs`).
- Use the official server's light dampening only for inert blocks.
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
  - an item name the reference list lacks, or a recipe or creative item that resolves to no item type;
  - a reference state the dump source reads as itself that is not an Anchor state, an Anchor state it reads as another state, or a reference state it reads as a state with no Anchor state.

## Unknown and inert blocks
- Keep a stored block state Anchor does not know as its original NBT.
- When a client sends a block or item Anchor does not know, reject only that request.
- Send an inert block (`block::Inert`) to clients as its own hash ID, the value the official server sends. Send every other unknown state as `minecraft:info_update`.
- Decide whether a state is inert once, when the chunk is read, into the palette entry `Entry::Inert`. Never decide it again per tick.
- The block update rules and liquids read an inert block as bedrock. The connection and placement rules read it as its stand-in: bedrock (full support on every face) when its only collision box is the whole cell, otherwise a cobweb (no support, no collision). Neither stand-in can be replaced.
- Never replace a seagrass or kelp cell.
- An inert block breaks in its hardness × 1.5 ÷ the held tool's efficiency: count the held tool as the right tool, so the server never allows a longer break time than the client takes.
- Breaking an inert block turns it into air and removes its block entity. It has no drops of its own.
- Events carry Anchor states only. `BlockBreak` and right-click `PlayerInteract` carry `block::INFO_UPDATE` in place of an inert block. A cancel leaves the block and sends it again as it was.
- Never raise a left-click `PlayerInteract` or a `WorldParticle` for an inert block.
- Keep the blocks and patterns inside block entities as stored. A flower pot plant (`PlantBlock`) Anchor does not know stays as it came and goes to clients only when it is inert. A banner pattern ID Anchor does not know stays and goes to clients.
- The public API shows only blocks and patterns Anchor knows.
- Send a block entity whose kind Anchor does not type (`Kind::Other`) to clients as stored when it has an `id`.
- Drop items Anchor does not know from block entities, with an info log (warn for an item in no valid slot).

## Upgrade schemas
- Use only the generated constant `BLOCK_STATE_VERSION` as the block state version.
- In a group of schemas that share a state version, treat the first as already applied and skip it.
- Keep `gen` failing when the newest block schema's version differs from the reference list's (`xtask/data/reference/blocks.json`), or when a reference state does not upgrade to itself.
- Keep `gen` rejecting, within one block schema, an ID both renamed and flattened, a property rename whose result depends on rename order, and a byte value above 127.
- Keep a legacy value the 1.12.0 table lacks as an unknown block: a numeric terrain value as `{id, val}`, a `{name, val}` palette entry as it came.
- Treat a state whose version is above the newest schema's as a state of the newest version.
- Keep `gen` checking that item schema IDs are lowercase, item schema metas are numbers, and every legacy ID map is one to one.
- `dump` stores every copied schema file with LF line ends.
- Share the rule interpreter (`src/import/upgrade/apply.rs`) with `gen`, which includes it by path. Never copy it.

## Rebuilding `.cache/`
- Builds, tests, and `verify` run on committed data alone, except the dimpl copy's tests, which need Perl.
- Rebuild `.cache/` from the repository root in this order. `<origin>` and the SHA-256 values are in the `.cache/` table of `.claude/docs/data.md`; `<commit>` and the official server version are in its sources table (bedrock-protocol-docs: the pinned-version table of `.claude/docs/protocol.md`).
  1. The six git checkouts, each: `git init .cache/<folder>`, `git -C .cache/<folder> remote add origin <origin>`, `git -C .cache/<folder> fetch --depth 1 origin <commit>`, `git -C .cache/<folder> checkout --detach FETCH_HEAD`.
  2. The dump PHP: download the release zip, check its SHA-256, and unpack it into `.cache/php-pm5/` so that `bin/php/php.exe` and `bin/php/php.ini` are there.
  3. The dump source's dependencies: in `.cache/pmmp`, run composer with the dump PHP: `composer install --no-dev --ignore-platform-reqs`. This puts the dump source's `composer.lock` versions into `.cache/pmmp/vendor`.
  4. The official server: download the zip for the OS, check its SHA-256, and unpack it into `.cache/bds/<version>/` (on Linux as `.claude/docs/testing.md` says).
  5. Perl: on Windows, download the Strawberry Perl portable zip, check its SHA-256, and unpack it into `.cache/tools/strawberry-perl-5.42.3.1/`. On Linux, install the system packages `.claude/docs/testing.md` lists.
  6. The str0m captures: `cargo xtask vendor-test str0m` downloads them.
  7. Confirm that `cargo xtask dump --check` passes.

Background and detailed facts: `.claude/docs/data.md`.
