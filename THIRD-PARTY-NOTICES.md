# Third-party notices

Anchor is licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
The parts below come from other projects and stay under their own terms.

## gophertunnel (MIT)

`xtask/data/protocol.json` is extracted from gophertunnel's packet definitions, and
`src/protocol/types.rs`, `src/protocol/packet.rs` and `src/protocol/consts.rs` are generated from it.

```
MIT License

Copyright (c) 2019 Sandertv

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Dragonfly (MIT)

The block states and items in `xtask/data/reference/blocks.json` and `xtask/data/reference/items.json`
come from Dragonfly's `block_states.nbt` and `vanilla_items.nbt`; the block and item tables generated
into `src/block/` and `src/item/` include them.

```
MIT License

Copyright (c) 2019 Dragonfly Tech

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## CloudburstMC Data (Apache-2.0)

Licensed under the Apache License, Version 2.0; the license text is in [LICENSE-APACHE](LICENSE-APACHE).
From CloudburstMC/Data, in `xtask/data/reference/`:

- `entity_identifiers.dat` and `voxel_shapes.json`: copied unchanged.
- `jigsaw_structure_data.dat` and `entity_properties.dat`: converted from the `.nbt` files to network NBT.
- `biomes.json`, `block_physics.json` and `item_blocks.json`: values taken from
  `stripped_biome_definitions.json`, `blocks.json` and `creative_items.json` into Anchor's own format.
- The block network IDs in `blocks.json`, from `block_palette.nbt`.

The biome and block tables generated into `src/world/biome/` and `src/block/` include these values.

## FreeBSD msun / fdlibm

`atan2`, `atan` and the sine and cosine kernels and argument reduction in `src/math.rs` follow fdlibm
(`e_atan2.c`, `s_atan.c`, `k_sin.c`, `k_cos.c`, `e_rem_pio2.c`) and use its constants.

```
Copyright (C) 1993 by Sun Microsystems, Inc. All rights reserved.

Developed at SunPro, a Sun Microsystems, Inc. business.
Permission to use, copy, modify, and distribute this
software is freely granted, provided that this notice
is preserved.
```

`e_atan2.c`, `k_sin.c` and `k_cos.c` carry the same notice with "Developed at SunSoft".

## CC0 data

Dedicated to the public domain under CC0 1.0; listed for attribution only.

- opencollab-incubator/BedrockBlockUpgradeSchema and opencollab-incubator/BedrockItemUpgradeSchema:
  `xtask/data/upgrade/` and the table generated into `src/import/upgrade/`.
- pocketmine/bedrock-data: the recipes in `xtask/data/pmmp/recipes.json` and the table generated into
  `src/crafting/`.

## PocketMine-MP output

`blocks.json`, `deserialized.json`, `items.json`, `creative.json`, `recipes.json`, `tables.json`,
`place.json`, `liquid.txt` and `api.json` in `xtask/data/pmmp/` are output of PocketMine-MP (LGPL-3.0)
run by the scripts in `xtask/php/`: block and item values, recipes, effect and enchantment tables, test
goldens, and a list of its public class and method names and signatures. They contain no PocketMine-MP
implementation code.

## Vendored crates

Each crate in `vendor/` keeps its upstream license files in its own directory. Anchor uses them under
their MIT terms.

## Minecraft

Minecraft is a trademark of Mojang Synergies AB. Game data in `xtask/data/` (block, item, biome and entity
data, the protocol field limits in `xtask/data/reference/protocol_limits.json`, and the camera presets in
`xtask/data/reference/bds_camera.json` captured from the official server) describes Minecraft and belongs
to Mojang. NOT AN OFFICIAL MINECRAFT PRODUCT. NOT APPROVED BY OR ASSOCIATED WITH MOJANG OR MICROSOFT.
