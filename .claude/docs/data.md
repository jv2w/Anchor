# Block and item data

Rules: `.claude/rules/data.md`.

The data is what the dump source's own registries hold when its dump PHP runs them, so it equals what the dump source runs with.

## Sources and pins

| Source | Pin | License | Used for |
|---|---|---|---|
| pmmp/PocketMine-MP | commit `6a7cc02e9dff59b69241aa0bcffdb9903ce86beb` (after 5.44.3, Bedrock 1.26.30 data) | LGPL-3.0, archived | The dump source: block and item kinds and behavior, recipes, the creative inventory, effect and enchantment tables, goldens |
| pocketmine/bedrock-data | 6.7.0+bedrock-1.26.30 (the dump source's `composer.lock`) | CC0 | Recipes and the other data the dump source reads |
| pmmp/PHP-Binaries | php-build-524, PHP 8.4 PM5 Windows x64 (zip SHA-256 `cf4023b8549c99eec47cfd403805ac7d78100fa366394dab4e45bb544dcc9be4`) | PHP License | The dump PHP (development tool) |
| df-mc/dragonfly | head `d86aefd9ff9b6a357d6dfadab2093af811545256` of the unmerged PR #1481 (fork HashimTheArab/dragonfly, branch `update/1.26.60-upstream`, gophertunnel v1.63.0, 1.26.60) | MIT | Reference list source A: block states, items, creative item states |
| CloudburstMC/Data | `293c848098cc636a2758937fb6610b8bed1f12c5` (26.60) | Apache-2.0 | Reference list source B: block network IDs, cross-checks, biomes, the official server's block physics, files sent as they are |
| Mojang/bedrock-protocol-docs | the pinned-version table of `.claude/docs/protocol.md` | Minecraft EULA | Field length limits (numbers only) |
| EndstoneMC/bedrock-protocol | v0.4.4, commit `0fb448a6a9940f197e199dcea488fc4129755d48` | MIT | The schema source: judging gophertunnel defects only (`protocol.md`) |
| CloudburstMC/Protocol | branch `3.0`, codec `v2225`, commit `17a89ac156ff874fc8773e42a96bbc0635be9eed` | Apache-2.0 | The codec library: judging gophertunnel defects only (`protocol.md`) |
| opencollab-incubator/BedrockBlockUpgradeSchema | 5.4.1, `1077ee640ed39f60266f7a7c8ab331ef76d7931e` | CC0 | Old block state upgrades |
| opencollab-incubator/BedrockItemUpgradeSchema | 1.19.0, `dfc12b388be886ba6980d9a3b67cb1b432b563e2` | CC0 | Old item name upgrades |
| The official server (Mojang) | 1.26.60.6 (protocol 2225) | Minecraft EULA | Comparison runs only. The camera and aim assist presets come from a spawn capture of 1.26.52.3 (protocol 2193; `bds_camera.json`) |

- `cargo xtask dump` first checks that the `HEAD` of each of its six checkouts is the pinned commit, and fails otherwise.
- The block schemas are 5.4.1, not the 5.2.0 of the dump source's `composer.lock`: 5.2.0 lacks the changes from 1.26.30 on.
- The item schemas are 1.19.0, not the dump source's 1.17.0: 1.19.0 has the 1.26.50 rename (`photo` → `photo_item`) and the 1.26.60 `frostbite_spawn_egg`.
- Commit `88a2436` of source B, after the pinned 26.60 commit, only changes the format of `blocks.json`, so it is not used.

## Dump (`cargo xtask dump`)

Before it starts, `dump` checks the pinned commits and that `.cache/pmmp/vendor` exists; without it, it stops and prints the composer command. Then:
1. `xtask/go/reference` reads reference list sources A and B and writes `blocks.json`, `items.json`, `item_blocks.json`, `jigsaw_structure_data.dat`, and `entity_properties.dat` into `xtask/data/reference/`. It runs first because step 2 reads every reference state with the dump source's deserializer.
2. The dump PHP runs `xtask/php/dump.php` with `.cache/pmmp` as the working directory and writes `xtask/data/pmmp/`.
3. It copies source B's biome definitions, entity identifiers, and block shapes into `xtask/data/reference/`.
4. It writes the field limits (`protocol_limits.json`) from Mojang's protocol schemas (`.claude/docs/protocol.md`).
5. It copies the upgrade schemas into `xtask/data/upgrade/` and makes the goldens with the dump source's upgrader (see the upgrade section). It fails when `xtask/data/upgrade/{blocks,items}` holds a file the sources do not make.
6. It writes `block_physics.json`: the official server's block physics for every reference state (see block physics). It fails on a state without physics.

`--check` does the same into `target/xtask-dump/` and fails where a committed file differs. `verify` does not run `dump`: it needs the checkouts, the dump PHP, and Go, and its pinned inputs never change. `verify`'s `gen --check` checks the code made from the committed data instead.

`dump` needs Go on `PATH`. It runs only on Windows: the dump PHP is the Windows x64 build at `.cache/php-pm5/bin/php/php.exe`. Plain PHP lacks the extensions the dump source's registries load.

### PHP scripts

| Script | Run by | Output |
|---|---|---|
| `xtask/php/dump.php` | `dump` | `blocks.json`, `deserialized.json`, `items.json`, `creative.json`, `recipes.json`, `tables.json` in `xtask/data/pmmp/` |
| `xtask/php/upgrade.php` | `dump` | The three files of `xtask/data/upgrade/golden/` |
| `xtask/php/place.php` | hand: `php place.php <output file>` | `xtask/data/pmmp/place.json`: placement goldens, which the tests of `src/tick/interact/place.rs` compare with |
| `xtask/php/liquid.php` | hand: `php liquid.php > liquid.txt` | `xtask/data/pmmp/liquid.txt`: liquid flow scene goldens, which the tests of `src/tick/update.rs` compare with |
| `xtask/php/api.php` | hand: `php api.php <output file>` | `xtask/data/pmmp/api.json`: the dump source's public API inventory, from which `cargo xtask table` builds the API table |

NBT values in the dump files are typed JSON (`{"byte": 1}`), floats as their bit patterns.

### Dump contents

- `blocks.json`: per dump state, its Bedrock serialization, light emission and filter, direct sky light blocking, random ticking, the legacy meta the dump source sends for it as an exact recipe ingredient, its class, parent classes and traits, break info, fire values, physical flags, friction, type tags, the support of each face, collision boxes, item form, the drops and experience for an empty hand, the lowest-tier compatible tool and that tool with Silk Touch, and, for blocks whose state follows from their neighbours (fences, panes, walls, fence gates, stairs, tripwire and its hooks), the values of each variant.
- `deserialized.json`: per reference state the dump source does not read as itself, the state it reads (null where it rejects the state). The dump source ignores some vanilla properties (a pumpkin's facing, a hanging sign's unused facing) and writes one value for them.
- `items.json`: per dump item type, its Bedrock serialization, the dump source's names, and its data (class, stack size, durability, tool, armor, food, fuel, enchanting, cooldown).
- `creative.json`: the creative inventory in order, with its categories and groups.
- `recipes.json`: the recipes the dump source loads from bedrock-data, plus the smithing recipes it leaves out, read with its own item reader. Recipes naming an item the dump source lacks are left out, as the dump source does.
- `tables.json`: status effects, potion types, enchantments with the enchanting power range of each level, item enchantment tags, item tags, banner patterns, furnace types, tool tiers.

### Dump source defects the dump and `gen` correct

- The dump source drops enchantments it does not know and keeps the empty enchanted books. Leaving those 61 out takes the creative inventory from 1,694 to 1,633 items.
- The ominous banner: the dump source has it black (meta 0); the official server sends it white (meta 15).
- The network form of block items: the dump source's default state (a stair's facing, for example) differs from the official server's.
- The wall hanging sign's collision box is inside out (3/4 cut from each side where 3/8 was meant). `gen` rebuilds it with 3/8, the official server's box (`INSIDE_OUT_BOXES` in `xtask/src/gen/data.rs`), and fails on an inside-out box of any other class.

## Reference list (`xtask/data/reference/`)

| File | Source | Holds |
|---|---|---|
| `blocks.json` | Source A `block_states.nbt`, with source B `block_palette.nbt` `network_id` | 23,960 block states with their network IDs |
| `items.json` | Source A `vanilla_items.nbt`, checked against source B `runtime_item_states.json` | 2,080 items in runtime ID order, with component data: the whole ItemRegistry |
| `item_blocks.json` | Source B `creative_items.json`, checked against source A `creative_items.nbt` and the block list | The vanilla creative state of every block item that has exactly one: the network form of block items |
| `block_physics.json` | Source B `blocks.json` (the official server's per-state dump), with the rules under block physics | Hardness, light, and collision boxes of inert blocks |
| `biomes.json` | Source B `stripped_biome_definitions.json` | BiomeDefinitionList (89 biomes) |
| `entity_identifiers.dat`, `voxel_shapes.json` | Source B, byte for byte | AvailableActorIdentifiers, VoxelShapes |
| `jigsaw_structure_data.dat`, `entity_properties.dat` | Source B `.nbt` files converted to network NBT | Data sent at spawn |
| `protocol_limits.json` | Mojang's protocol schemas | Decoder field limits |
| `bds_camera.json` | A spawn capture of the official server 1.26.52.3 | Camera and aim assist presets |

- Block network IDs are hashes: FNV-1a 32 over the little-endian NBT of the name and the states sorted by name. The official server uses hash IDs too. A golden test in `src/block/hash.rs` checks the Rust hash against every reference state.
- Block physics: source B's `blocks.json` at the pinned commit predates 1.26.60, so `dump` has two temporary rules (`reference_physics` in `xtask/src/dump.rs`; a unit test fixes how many states each covers):
  - Connections: the 1,872 states with the connection properties 1.26.60 added (fire 512, redstone wire 1,296, chorus plant 64) take the physics of the same state without them, the state block schema `0361` upgrades to them.
  - Borrowing: the 30 states of blocks new in 1.26.60 take the physics of a similar block: `icicle` (20) from `pointed_dripstone`, `ice_crystal` (6) from `amethyst_cluster`, `disc_press` (4) from `stone`. Each takes the one lender state whose shared properties (direction, thickness) have the same values; another number of matches fails `dump`. These blocks have no behavior (inert blocks).
  - Any other state without physics fails `dump`.
- `dump` fails on a biome with an ID, chunk generation data, or a nonzero density: the packet carries none of them.
- The 11,420 dump states have 9,532 Bedrock serializations. 76 of these take several dump states (cave vines, farmland, beds, standing banners, wall banners, cauldrons). Those must agree in every value, except the `SHARED` names: bed, cauldron (as the water cauldron), standing banner, wall banner.
- The official server's light dampening differs from the dump's light filter for some blocks: oak leaves 1 against 0, glowstone 15 against 0.
- The official server sends data-driven block definitions in StartGame. None of them is a dump block.

## Generated tables (`cargo xtask gen`)

| Generated file | Input | Holds |
|---|---|---|
| `src/block/table.rs` | The dump, block schemas `0351` and `0361`, the reference list | 14,307 Anchor states and 4,884 aliases |
| `src/block/inert/table.rs` | The reference list, `block_physics.json` | 4,769 inert states (264 names) with their physics |
| `src/block/drops.rs` | The dump | Drop goldens |
| `src/item/table.rs` | The dump (`items.json`, `tables.json`), the reference list | 1,613 dump item types; enchantments, item tags, tool tiers |
| `src/item/registry.rs` | The reference list | ItemRegistry, 2,080 entries |
| `src/item/creative.rs` | The dump | The creative inventory: 107 groups, 1,633 items |
| `src/crafting/table.rs` | The dump | Shaped, shapeless, furnace, brewing, and smithing recipes |
| `src/effect/table.rs` | `tables.json` | 27 status effects, 43 potion types |
| `src/world/biome/table.rs` | `biomes.json` | Biome definitions |
| `src/import/upgrade/table.rs` | `xtask/data/upgrade/` | The import's upgrade tables |

The tables are `static`, so they cost nothing at startup and never allocate.

An Anchor block state is a dump state's Bedrock serialization (deduplicated) with the properties of the newest block schemas added, together with every other value of those properties. The newest schemas are all those after the dump source's data (1.26.30): `0351` and `0361`. 1.26.50 added properties to 115 dump block names (`minecraft:connection_*` of fences, panes, bars, and tripwire; `minecraft:corner` of stairs); the official server works them out from the neighbours. States are ordered by name, then by their (property name, value) pairs in property name order; values order by type (byte, int, string), then by value. `block::State` is the index in that order, and `gen` checks the order.

The 23,960 reference states are the 14,307 Anchor states, the 4,884 aliases, and the 4,769 inert states.

An alias is a reference state the dump source reads as another state (a pumpkin facing east is the pumpkin facing south). `State::read` looks up the exact state first, then the alias table, so imported worlds, disk palettes, and item block NBT become the state the dump source makes of them.

`gen` fails on data errors so that a data error never becomes a silent misbehavior at run time.

## Unknown and inert blocks

- A state Anchor does not know is inert when the hash of its `{name, states}` is a reference state that is neither an Anchor state nor an alias, and its name is that state's name. An inert block shows the original block; it does not implement it.
- The lookup is a binary search of the inert table. The storage format is unchanged, so an already imported world also shows its original blocks without a new import.
- The server knows only the official server's collision boxes, light (emission and filter; a filter above 0 also blocks sky light), and hardness. It has no behavior: no flow, growth, or random ticks.
- The data does not name the right tool.
- Because of the placement stand-in, a block placed on an inert block goes into the cell next to the clicked face. The data does not say whether seagrass and kelp are replaced, so keeping the original was chosen.
- Remaining limits: an explosion neither breaks an inert block nor passes it (the ray stops there), and entities do not treat seagrass and kelp cells as water.
- A flower pot plant Anchor does not know that is not inert shows as an empty pot. Players cannot take it out or swap it, and it has no drops. The dump source fails to read such a block entity, and drops banner pattern IDs it does not know.
- Imported worlds hold vanilla items the dump lacks, so dropping one from a block entity is logged at info, not warn.

## Upgrade schemas and old data (`xtask/data/upgrade/`)

The world import (`.claude/docs/storage.md`) upgrades old block states and item names with these schemas.

| File | Holds |
|---|---|
| `blocks/` | 37 block schemas (`nbt_upgrade_schema/`) |
| `items/` | 30 item schemas (`id_meta_upgrade_schema/`), `item_legacy_id_map.json`, `1.12.0_item_id_to_block_id_map.json` |
| `block_legacy_id_map.json`, `legacy_1.12.0.jsonl` | Legacy numeric block IDs, and the 1.12.0 `id_meta_to_nbt` table converted from binary to typed JSON lines |
| `golden/` | Output of the dump source's upgrader: `legacy.jsonl`, `states.jsonl`, `reference.jsonl` |
| `sources.json` | The source commits and the SHA-256 of every source file |

The generated tables hold 37 block schemas, 3,311 legacy entries (upgrading to 2,812 states), and 30 item schemas. `BLOCK_STATE_VERSION` is 18,168,865 (= 1.21.60.33), the version of the reference list and of the newest block schema.

Defects of the schema repositories and the dump source's upgrader:
- From `0321` on, every block schema writes the same version (1.21.60.33). The dump source applies every schema of a state's own version, so it runs `0321` again on current states and turns `red_mushroom_block` with `huge_mushroom_bits` 10 and 15 into `mushroom_stem` (`golden/reference.jsonl`).
- The dump source throws at run time on an ID both renamed and flattened in one schema, and it reads byte values as signed.
- The dump source turns a legacy meta its table lacks into meta 0.
- For a state version above the newest schema's, the dump source applies nothing, so blocks without the 1.26 properties (fences, stairs) become unknown blocks. The newest version's schemas only add missing properties and rename old names, so applying them leaves current states as they are; `gen` checks this for every reference state.
- The dump source looks item IDs up lowercased, so an ID with a capital letter never matches.

`gen` checks the schemas with the interpreter the import runs, so a defect the rules hide fails at generation. The goldens come from the dump source's upgrader run by the dump PHP and built from the 5.4.1 copies, not the 5.2.0 in the dump source's `vendor/`; the import's tests compare with them.

## Rebuilding `.cache/`

`.cache/` is ignored by git. It is needed by `dump`, `p6`, `vendor-test str0m`, the dimpl copy's tests in `verify` (Perl), and the xtask tests that read source B (`xtask/tests/spawn.rs` and a unit test in `xtask/src/dump.rs`), which return without checking when it is missing. Each SHA-256 is of the file named.

| Folder | Origin | Pin | Read by |
|---|---|---|---|
| `pmmp` | `https://github.com/pmmp/PocketMine-MP.git` | The sources table's commit + `vendor/` (`composer.lock`) | `dump`, every PHP script |
| `dragonfly` | `https://github.com/HashimTheArab/dragonfly.git` (the fork of PR #1481) | The sources table's commit | `dump` |
| `cloudburst-data` | `https://github.com/CloudburstMC/Data.git` | The sources table's commit | `dump`, `p6` (biome and jigsaw comparison), `xtask/tests/spawn.rs`, the physics unit test of `xtask/src/dump.rs` |
| `bedrock-protocol-docs` | `https://github.com/Mojang/bedrock-protocol-docs.git` | The commit of the pinned-version table of `.claude/docs/protocol.md` | `dump` (field limits) |
| `block-upgrade-schema` | `https://github.com/opencollab-incubator/BedrockBlockUpgradeSchema.git` | The sources table's commit | `dump` |
| `item-upgrade-schema` | `https://github.com/opencollab-incubator/BedrockItemUpgradeSchema.git` | The sources table's commit | `dump` |
| `php-pm5/bin/php/php.exe` | pmmp/PHP-Binaries release php-build-524, PHP 8.4 PM5 Windows x64 | zip `cf4023b8…9be4` (sources table), `php.exe` `80625be7d15a3f156bb5ae5f8fb454bcfbec043d800022d70fa59333512f484c` (PHP 8.4.16 ZTS) | `dump`, every PHP script |
| `bds/1.26.60.6/` | minecraft.net Bedrock Dedicated Server, Windows 1.26.60.6 (`bedrock-server-1.26.60.6.zip`) | zip `21620db4f80d0fe206f21cb08f52f4e5f14252ce865ec49ccfbe0d9f93ce88b0`, `bedrock_server.exe` `d7b26d6c5e8f3e910f66e8cbf7e1dfde06e570032349bf7abdd070deb6e9d884` | `p6 bds`, `p6 game bds`, `p6 api bds`, `p6 import` |
| `bds/1.26.60.6/` (Linux checkout) | minecraft.net Bedrock Dedicated Server, Linux 1.26.60.6 (`bin-linux/bedrock-server-1.26.60.6.zip`) | Not pinned yet: no SHA-256 recorded | Every `p6` command on Linux |
| `tools/strawberry-perl-5.42.3.1/` | StrawberryPerl GitHub release SP_54231_64bit, portable zip | zip `6a081a811781c30aca51dbc036afd93092af91e3297901f02c17043795a10690`, `perl/bin/perl.exe` `d4fa3a6ebe78f5f60e185cf7d7809a7dd8c541739ca4a7bc8687c75548473f42` | The dimpl copy's tests in `verify` (OpenSSL source build) |
| `str0m-test-data/` | `https://raw.githubusercontent.com/algesten/str0m/<commit>/tests/data/<file>` | The commit in `vendor/str0m/.cargo_vcs_info.json`; per file, `xtask/data/vendor/str0m-test-data.sha256` | `vendor-test str0m` (downloads what is missing) |

`vendor-test str0m` checks the SHA-256 of every capture on every run, downloaded or cached.
