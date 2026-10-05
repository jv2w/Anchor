//! Ground and other non-plant block rules (PMMP `onRandomTick`, `onNearbyBlockChange`,
//! `onScheduledUpdate` and bone meal of grass, mycelium, nylium, netherrack, dirt, farmland,
//! grass paths, ice, frosted ice, snow, leaves, redstone ore, budding amethyst, coral, concrete
//! powder, cauldrons and campfires), and copper oxidation, which PMMP does not have (dragonfly
//! `attemptOxidation`).

use std::collections::BTreeSet;

use super::Cx;
use super::props;
use super::support::{self, side};
use crate::block::{self, Class, CopperOxidation, Face, Kind, Trait};
use crate::event::{Actor, EntityTrampleFarmland, FarmlandHydrationChange, LeavesDecay};
use crate::random::Random;
use crate::tick::interact;
use crate::world::BlockPos;

/// Whether `s` is plain dirt (PMMP `Dirt` with `DirtType::NORMAL`).
fn is_plain_dirt(s: block::State) -> bool {
    s.name() == "minecraft:dirt"
}

/// PMMP `Grass::onRandomTick`: grass in the dark under a light filter dies; in the light it
/// spreads to plain dirt nearby. Both are spreads, as in PMMP.
pub(super) fn grass(cx: &mut Cx<'_>, pos: BlockPos) {
    let Some(up) = pos.step(Face::Up) else {
        return;
    };
    let light_above = cx.full_light(up);
    if light_above < 4 && cx.block(Some(up)).light_filter() >= 2 {
        cx.spread(pos, block::DIRT, pos);
    } else if light_above >= 9 {
        let mut rng = cx.rng();
        for _ in 0..4 {
            let x = rng.range(pos.x - 1, pos.x + 1);
            let y = rng.range(pos.y - 3, pos.y + 1);
            let z = rng.range(pos.z - 1, pos.z + 1);
            let at = BlockPos::new(x, y, z);
            let above = BlockPos::new(x, y + 1, z);
            if cx.get(at).is_some_and(is_plain_dirt)
                && cx.full_light(above) >= 4
                && cx.block(Some(above)).light_filter() < 2
            {
                cx.spread(at, block::GRASS_BLOCK, pos);
            }
        }
    }
}

/// PMMP `Mycelium::onRandomTick`: spreads to plain dirt under a transparent block.
pub(super) fn mycelium(cx: &mut Cx<'_>, pos: BlockPos) {
    let mut rng = cx.rng();
    let x = rng.range(pos.x - 1, pos.x + 1);
    let y = rng.range(pos.y - 2, pos.y + 2);
    let z = rng.range(pos.z - 1, pos.z + 1);
    let at = BlockPos::new(x, y, z);
    if cx.get(at).is_some_and(is_plain_dirt) && cx.block(at.step(Face::Up)).is(Class::Transparent) {
        cx.spread(at, Kind::Mycelium.default_state(), pos);
    }
}

/// PMMP `Nylium::onRandomTick`: covered nylium turns to netherrack; uncovered spreads to
/// netherrack nearby.
pub(super) fn nylium(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if !cx.block(pos.step(Face::Up)).is_transparent() {
        cx.spread(pos, Kind::Netherrack.default_state(), pos);
        return;
    }
    let mut rng = cx.rng();
    for _ in 0..4 {
        let (dx, dy, dz) = (rng.range(-1, 1), rng.range(-2, 0), rng.range(-1, 1));
        let Some(at) = pos.offset(dx, dy, dz) else {
            continue;
        };
        if cx.get(at).is_some_and(|s| s.kind() == Kind::Netherrack)
            && cx.block(at.step(Face::Up)).is_transparent()
        {
            cx.spread(at, block, pos);
        }
    }
}

/// The brightest block light next to `pos` (PMMP `getHighestAdjacentBlockLight`).
fn adjacent_block_light(cx: &Cx<'_>, pos: BlockPos) -> u8 {
    Face::ALL
        .into_iter()
        .filter_map(|f| pos.step(f))
        .map(|p| cx.block_light(p))
        .max()
        .unwrap_or(0)
}

/// PMMP `Ice::onRandomTick`: melts next to block light of 12 or more.
pub(super) fn ice(cx: &mut Cx<'_>, pos: BlockPos) {
    if adjacent_block_light(cx, pos) >= 12 {
        cx.melt(pos, Kind::Water.default_state());
    }
}

/// PMMP `FrostedIce::onRandomTick` (and `onScheduledUpdate`).
pub(super) fn frosted_ice(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let Some(age) = props::int(block, "age") else {
        return;
    };
    let mut found = 0;
    let mut crowded = false;
    'scan: for x in -1..=1 {
        for z in -1..=1 {
            if (x != 0 || z != 0) && cx.block(pos.offset(x, 0, z)).class() == Class::FrostedIce {
                found += 1;
                if found >= 4 {
                    crowded = true;
                    break 'scan;
                }
            }
        }
    }
    let light = Face::ALL
        .into_iter()
        .filter_map(|f| pos.step(f))
        .map(|p| cx.full_light(p))
        .max()
        .unwrap_or(0);
    let mut rng = cx.rng();
    if (!crowded || rng.range(0, 2) == 0) && i32::from(light) >= 12 - age {
        if melt_frosted(cx, &mut rng, pos, block) {
            for face in Face::ALL {
                if let Some(p) = pos.step(face)
                    && let Some(s) = cx.get(p)
                    && s.class() == Class::FrostedIce
                {
                    melt_frosted(cx, &mut rng, p, s);
                }
            }
        }
    } else {
        let delay = rng.range(20, 40);
        cx.schedule(pos, u32::try_from(delay).unwrap_or(20));
    }
}

/// PMMP `FrostedIce::tryMelt`: older, or water at the last age. Whether it reached the last age
/// (melted, unless a listener kept it).
fn melt_frosted(cx: &mut Cx<'_>, rng: &mut Random, pos: BlockPos, block: block::State) -> bool {
    let age = props::int(block, "age").unwrap_or(3);
    if age >= 3 {
        cx.melt(pos, Kind::Water.default_state());
        return true;
    }
    if let Some(s) = props::with_int(block, "age", age + 1) {
        cx.set(pos, s);
    }
    let delay = rng.range(20, 40);
    cx.schedule(pos, u32::try_from(delay).unwrap_or(20));
    false
}

/// PMMP `SnowLayer::onRandomTick`: melts in block light of 12 or more.
pub(super) fn snow_layer(cx: &mut Cx<'_>, pos: BlockPos) {
    if cx.block_light(pos) >= 12 {
        cx.melt(pos, block::AIR);
    }
}

/// PMMP `Farmland::onRandomTick`: wet near water (9 × 9 × 2 above its level), drying
/// otherwise, then dirt.
pub(super) fn farmland(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let wetness = props::int(block, "moisturized_amount").unwrap_or(0);
    let mut water = false;
    'search: for y in 0..2 {
        for x in -4..=4 {
            for z in -4..=4 {
                if cx.block(pos.offset(x, y, z)).class() == Class::Water {
                    water = true;
                    break 'search;
                }
            }
        }
    }
    if !water {
        if wetness > 0 {
            hydrate(cx, pos, block, wetness, wetness - 1);
        } else {
            cx.set(pos, block::DIRT);
        }
    } else if wetness < 7 {
        hydrate(cx, pos, block, wetness, 7);
    }
}

/// Farmland's wetness goes from `old` to `new` (0 to 7), raising
/// [`FarmlandHydrationChange`], whose listeners may set another wetness.
fn hydrate(cx: &mut Cx<'_>, pos: BlockPos, block: block::State, old: i32, new: i32) {
    let wetness = |v: i32| u8::try_from(v.clamp(0, 7)).unwrap_or(0);
    let Ok(event) = cx.ask(
        [pos],
        |cx| FarmlandHydrationChange {
            world: cx.world_id(),
            pos,
            block,
            old_hydration: wetness(old),
            new_hydration: wetness(new),
            cancelled: false,
        },
        FarmlandHydrationChange::is_cancelled,
    ) else {
        return;
    };
    let new = event
        .as_ref()
        .map_or(new, |e| i32::from(e.new_hydration().min(7)));
    if let Some(s) = props::with_int(block, "moisturized_amount", new)
        && cx.set_quietly(pos, s)
    {
        cx.tell(event);
    }
}

/// PMMP `Farmland`/`GrassPath::onNearbyBlockChange`: a solid block on top turns it to dirt.
pub(super) fn trample_check(cx: &mut Cx<'_>, pos: BlockPos) {
    if pos
        .step(Face::Up)
        .and_then(|p| cx.get(p))
        .is_some_and(block::State::is_solid)
    {
        cx.set(pos, block::DIRT);
    }
}

/// PMMP `Leaves::onNearbyBlockChange`: leaves that decay start checking for a log.
pub(super) fn leaves_on_nearby(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if props::flag(block, "persistent_bit") == Some(false)
        && props::flag(block, "update_bit") == Some(false)
        && let Some(s) = props::with_flag(block, "update_bit", true)
    {
        cx.set_quietly(pos, s);
    }
}

/// PMMP `Leaves::onRandomTick`: leaves without a log within four leaves decay, raising
/// [`LeavesDecay`]; leaves that find a log, or whose decay is cancelled, stop checking until a
/// neighbour changes. PMMP raises the event before it looks for a log, so for leaves that do not
/// decay too: Anchor raises it only for leaves that decay.
pub(super) fn leaves_on_random(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if props::flag(block, "persistent_bit") != Some(false)
        || props::flag(block, "update_bit") != Some(true)
    {
        return;
    }
    let mut visited = BTreeSet::new();
    if !find_log(cx, pos, &mut visited, 0) {
        let asked = cx.ask(
            [pos],
            |cx| LeavesDecay {
                world: cx.world_id(),
                pos,
                block,
                cancelled: false,
            },
            LeavesDecay::is_cancelled,
        );
        if let Ok(event) = asked {
            cx.destroy(pos);
            cx.tell(event);
            return;
        }
        // Cancelled: the leaves stay, and a listener that put another block there keeps it.
        if cx.get(pos) != Some(block) {
            return;
        }
    }
    if let Some(s) = props::with_flag(block, "update_bit", false) {
        cx.set_quietly(pos, s);
    }
}

/// PMMP `Leaves::findLog`: a depth-first search through leaves up to distance 4 (at most the
/// positions within 5 blocks).
fn find_log(cx: &Cx<'_>, pos: BlockPos, visited: &mut BTreeSet<BlockPos>, distance: i32) -> bool {
    if !visited.insert(pos) {
        return false;
    }
    let s = cx.block(Some(pos));
    if s.is(Class::Wood) {
        return true;
    }
    if s.is(Class::Leaves) && distance <= 4 {
        for face in Face::ALL {
            if let Some(next) = pos.step(face)
                && find_log(cx, next, visited, distance + 1)
            {
                return true;
            }
        }
    }
    false
}

/// The lit or unlit redstone ore of `block`.
fn redstone_ore(block: block::State, lit: bool) -> Option<block::State> {
    let name = block.name().strip_prefix("minecraft:")?;
    let base = name.strip_prefix("lit_").unwrap_or(name);
    let target = if lit {
        format!("minecraft:lit_{base}")
    } else {
        format!("minecraft:{base}")
    };
    props::plain(&target)
}

/// PMMP `RedstoneOre::onNearbyBlockChange`: lights up.
pub(super) fn redstone_ore_on_nearby(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if !block.name().contains(":lit_")
        && let Some(lit) = redstone_ore(block, true)
    {
        cx.set(pos, lit);
    }
}

/// PMMP `RedstoneOre::onRandomTick`: goes out.
pub(super) fn redstone_ore_on_random(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if block.name().contains(":lit_")
        && let Some(unlit) = redstone_ore(block, false)
    {
        cx.set(pos, unlit);
    }
}

/// The amethyst growth stages, by Bedrock name.
const AMETHYST_STAGES: [&str; 4] = [
    "minecraft:small_amethyst_bud",
    "minecraft:medium_amethyst_bud",
    "minecraft:large_amethyst_bud",
    "minecraft:amethyst_cluster",
];

/// PMMP `BuddingAmethyst::onRandomTick`: one in five, a bud on a random face grows a stage.
pub(super) fn budding_amethyst(cx: &mut Cx<'_>, pos: BlockPos) {
    let mut rng = cx.rng();
    if rng.range(1, 5) != 1 {
        return;
    }
    let Some(face) = Face::ALL.get(rng.below(6) as usize).copied() else {
        return;
    };
    let Some(at) = pos.step(face) else {
        return;
    };
    let Some(adjacent) = cx.get(at) else {
        return;
    };
    let stage = if adjacent.kind() == Kind::Air {
        Some(0)
    } else if adjacent.kind() == Kind::AmethystCluster
        && props::text(adjacent, "minecraft:block_face").and_then(props::face_name) == Some(face)
    {
        AMETHYST_STAGES
            .iter()
            .position(|n| *n == adjacent.name())
            .filter(|s| *s < 3)
            .map(|s| s + 1)
    } else {
        None
    };
    let Some(name) = stage.and_then(|s| AMETHYST_STAGES.get(s)) else {
        return;
    };
    let Some(bud) = block::State::all().find(|s| {
        s.name() == *name && props::text(*s, "minecraft:block_face") == Some(props::name_of(face))
    }) else {
        return;
    };
    cx.grow(at, bud);
}

/// The oxidation stage of an unwaxed copper block, if it is one.
fn oxidation(block: block::State) -> Option<CopperOxidation> {
    if !block.has_trait(Trait::Copper) {
        return None;
    }
    let (stage, waxed, _) = block::copper_parts(block)?;
    (!waxed).then_some(stage)
}

/// Whether the block is unwaxed copper that can oxidize further: it takes random ticks.
pub(super) fn is_oxidizable(block: block::State) -> bool {
    oxidation(block).is_some_and(|l| l.next().is_some())
}

/// dragonfly `attemptOxidation` (vanilla's rule; PMMP does not oxidize copper): a chance of
/// 64/1125, then by how many unwaxed copper blocks within four blocks (taxicab) are more
/// oxidized, unless one is less oxidized. A door oxidizes both halves.
pub(super) fn oxidize(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let Some((level, next_level)) = oxidation(block).and_then(|l| Some((l, l.next()?))) else {
        return;
    };
    let mut rng = cx.rng();
    if rng.unit() > 64.0 / 1125.0 {
        return;
    }
    let (mut all, mut higher) = (0u32, 0u32);
    for x in -4i32..=4 {
        for y in -4i32..=4 {
            for z in -4i32..=4 {
                if (x == 0 && y == 0 && z == 0) || x.abs() + y.abs() + z.abs() > 4 {
                    continue;
                }
                let Some(other) = oxidation(cx.block(pos.offset(x, y, z))) else {
                    continue;
                };
                if other < level {
                    return;
                }
                all += 1;
                if other > level {
                    higher += 1;
                }
            }
        }
    }
    let base = f64::from(higher + 1) / f64::from(all + 1);
    let chance = if level == CopperOxidation::None {
        base * base * 0.75
    } else {
        base * base
    };
    if rng.unit() >= chance {
        return;
    }
    let Some((_, _, rest)) = block::copper_parts(block) else {
        return;
    };
    let name = block::copper_name(next_level, false, rest);
    let Some(next) = interact::props::renamed(block, &name) else {
        return;
    };
    cx.set(pos, next);
    if block.is(Class::Door)
        && let Some(top) = props::flag(block, "upper_block_bit")
        && let Some(other_pos) = pos.step(if top { Face::Down } else { Face::Up })
        && let Some(other) = cx.get(other_pos)
        && other.kind() == block.kind()
        && oxidation(other) == Some(level)
        && let Some(other_next) = interact::props::renamed(other, &name)
    {
        cx.set(other_pos, other_next);
    }
}

/// Whether coral is dead (its name says so).
pub(super) fn is_dead_coral(block: block::State) -> bool {
    block.name().contains(":dead_")
}

/// PMMP `BaseCoral`/`CoralBlock::onNearbyBlockChange`: coral without support breaks; living
/// coral checks for water in 40 to 200 ticks.
pub(super) fn coral_on_nearby(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if block.class() != Class::CoralBlock {
        match support::supported(cx, pos, block) {
            Some(false) => {
                cx.destroy(pos);
                return;
            }
            None => return,
            Some(true) => {}
        }
    }
    if !is_dead_coral(block) {
        cx.schedule_random(pos, 40, 200);
    }
}

/// PMMP `BaseCoral`/`CoralBlock::onScheduledUpdate`: living coral without water next to it dies.
pub(super) fn coral_check(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if is_dead_coral(block) {
        return;
    }
    let mut water = false;
    for face in Face::ALL {
        match side(cx, pos, face) {
            Some(s) if s.class() == Class::Water => water = true,
            Some(_) => {}
            None => return,
        }
    }
    if !water
        && let Some(rest) = block.name().strip_prefix("minecraft:")
        && let Some(dead) = interact::props::renamed(block, &format!("minecraft:dead_{rest}"))
    {
        cx.die(pos, dead);
    }
}

/// PMMP `ConcretePowder::onNearbyBlockChange`: water beside or above (the first in PMMP's face
/// order) hardens it; otherwise it falls.
pub(super) fn concrete_powder(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let water = [Face::Up, Face::North, Face::South, Face::West, Face::East]
        .into_iter()
        .filter_map(|f| pos.step(f))
        .find(|p| cx.block(Some(*p)).class() == Class::Water);
    if let Some(water) = water {
        if let Some(concrete) = block.name().strip_suffix("_powder").and_then(props::plain) {
            cx.form(pos, concrete, water);
        }
    } else {
        support::fall_unless_hardened(cx, pos, block);
    }
}

/// PMMP `Cauldron`/`WaterCauldron`/`PotionCauldron::onNearbyBlockChange`: water above fills it.
/// A water cauldron's custom colour lives in its block entity (inventory rules).
pub(super) fn cauldron_on_nearby(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if props::int(block, "fill_level").is_some_and(|f| f >= 6) {
        return;
    }
    if cx.block(pos.step(Face::Up)).kind() == Kind::Water
        && let Some(full) = props::with_int(block, "fill_level", 6)
            .and_then(|s| props::with_text(s, "cauldron_liquid", "water"))
    {
        cx.set(pos, full);
    }
}

/// PMMP `Campfire::onNearbyBlockChange`: water above puts a lit campfire out.
pub(super) fn campfire_on_nearby(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    if props::flag(block, "extinguished") == Some(false)
        && cx.block(pos.step(Face::Up)).kind() == Kind::Water
        && let Some(out) = props::with_flag(block, "extinguished", true)
    {
        cx.set(pos, out);
    }
}

/// Bone meal on ground blocks: whether it is used up.
pub(super) fn fertilize(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) -> bool {
    match block.class() {
        Class::Grass => {
            if cx.block(pos.step(Face::Up)).kind() != Kind::Air {
                return false;
            }
            grow_grass(cx, pos);
            true
        }
        Class::Dirt => {
            if block.name() != "minecraft:dirt_with_roots" {
                return false;
            }
            let Some(below) = pos.step(Face::Down) else {
                return false;
            };
            if cx.get(below).is_none_or(|d| d.kind() != Kind::Air) {
                return false;
            }
            cx.set(below, Kind::HangingRoots.default_state());
            true
        }
        Class::Netherrack => netherrack(cx, pos),
        Class::Nylium => {
            if cx.block(pos.step(Face::Up)).kind() != Kind::Air {
                return false;
            }
            nylium_vegetation(cx, pos, block);
            true
        }
        _ => false,
    }
}

/// PMMP `TallGrass::growGrass` with bone meal's count 8 and radius 2.
fn grow_grass(cx: &mut Cx<'_>, pos: BlockPos) {
    let plants = [
        Kind::Dandelion,
        Kind::Poppy,
        Kind::TallGrass,
        Kind::TallGrass,
        Kind::TallGrass,
        Kind::TallGrass,
    ];
    let mut rng = cx.rng();
    for _ in 0..8 {
        let x = rng.range(pos.x - 2, pos.x + 2);
        let z = rng.range(pos.z - 2, pos.z + 2);
        let ground = BlockPos::new(x, pos.y, z);
        let above = BlockPos::new(x, pos.y + 1, z);
        if cx.get(above).is_some_and(|s| s.kind() == Kind::Air)
            && cx.get(ground).is_some_and(|s| s.kind() == Kind::Grass)
        {
            let pick = usize::try_from(rng.range(0, 5)).unwrap_or(0);
            if let Some(k) = plants.get(pick) {
                cx.set(above, k.default_state());
            }
        }
    }
}

/// PMMP `Netherrack::tryTransform`: nylium next to it spreads onto it.
fn netherrack(cx: &mut Cx<'_>, pos: BlockPos) -> bool {
    if !cx.block(pos.step(Face::Up)).is_transparent() {
        return false;
    }
    let (mut warped, mut crimson) = (false, false);
    for x in -1..=1 {
        for y in -1..=1 {
            for z in -1..=1 {
                match cx.block(pos.offset(x, y, z)).kind() {
                    Kind::WarpedNylium => warped = true,
                    Kind::CrimsonNylium => crimson = true,
                    _ => {}
                }
            }
        }
    }
    let kind = match (warped, crimson) {
        (true, true) => {
            if cx.rng().range(0, 1) == 0 {
                Kind::WarpedNylium
            } else {
                Kind::CrimsonNylium
            }
        }
        (true, false) => Kind::WarpedNylium,
        (false, true) => Kind::CrimsonNylium,
        (false, false) => return false,
    };
    cx.set(pos, kind.default_state());
    true
}

/// PMMP `Nylium::growVegetation`: fungi and roots on the 5 × 5 around.
fn nylium_vegetation(cx: &mut Cx<'_>, pos: BlockPos, block: block::State) {
    let plants: &[Kind] = if block.kind() == Kind::CrimsonNylium {
        &[Kind::CrimsonFungus, Kind::CrimsonRoots]
    } else {
        &[Kind::WarpedFungus, Kind::WarpedRoots, Kind::NetherSprouts]
    };
    let mut rng = cx.rng();
    for x in -2..=2 {
        for z in -2..=2 {
            if rng.below(3) != 0 {
                continue;
            }
            let pick = rng.below(u32::try_from(plants.len()).unwrap_or(1)) as usize;
            let Some(place) = plants.get(pick).map(|k| k.default_state()) else {
                continue;
            };
            let Some(at) = pos.offset(x, 1, z) else {
                continue;
            };
            if cx.get(at).is_some_and(|s| s.kind() == Kind::Air)
                && support::supported(cx, at, place) == Some(true)
            {
                cx.set(at, place);
            }
        }
    }
}

/// `entity` landed on farmland after falling `fall_distance` blocks: a living one tramples it
/// to dirt with PMMP's chance (`getRandomFloat() < fallDistance - 0.5`), raising
/// [`EntityTrampleFarmland`].
pub(super) fn trample(cx: &mut Cx<'_>, pos: BlockPos, fall_distance: f64, entity: Actor) {
    let Some(block) = cx.get(pos).filter(|s| s.kind() == Kind::Farmland) else {
        return;
    };
    let tramples = cx.rng().unit() < fall_distance - 0.5;
    if !tramples {
        return;
    }
    let Ok(event) = cx.ask(
        [pos],
        |cx| EntityTrampleFarmland {
            entity,
            world: cx.world_id(),
            pos,
            block,
            cancelled: false,
        },
        EntityTrampleFarmland::is_cancelled,
    ) else {
        return;
    };
    if cx.set(pos, block::DIRT) {
        cx.tell(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> block::State {
        block::State::all().find(|s| s.name() == name).unwrap()
    }

    #[test]
    fn copper_oxidation_stages_of_unwaxed_copper_only() {
        use CopperOxidation as O;
        assert_eq!(oxidation(named("minecraft:copper_block")), Some(O::None));
        assert_eq!(
            oxidation(named("minecraft:exposed_copper")),
            Some(O::Exposed)
        );
        assert_eq!(
            oxidation(named("minecraft:weathered_cut_copper")),
            Some(O::Weathered)
        );
        assert_eq!(oxidation(named("minecraft:waxed_exposed_copper")), None);
        assert!(!is_oxidizable(named("minecraft:oxidized_copper")));
        assert!(is_oxidizable(named("minecraft:copper_door")));
        assert!(!is_oxidizable(named("minecraft:stone")));
    }

    #[test]
    fn redstone_ore_and_concrete_names_convert() {
        let ore = named("minecraft:deepslate_redstone_ore");
        assert_eq!(
            redstone_ore(ore, true).unwrap().name(),
            "minecraft:lit_deepslate_redstone_ore"
        );
        let lit = named("minecraft:lit_redstone_ore");
        assert_eq!(
            redstone_ore(lit, false).unwrap().name(),
            "minecraft:redstone_ore"
        );
        let powder = named("minecraft:red_concrete_powder");
        assert_eq!(
            props::plain(powder.name().strip_suffix("_powder").unwrap())
                .unwrap()
                .name(),
            "minecraft:red_concrete"
        );
    }
}
