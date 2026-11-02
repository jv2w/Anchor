//! Ground and other non-plant block rules (PMMP `onRandomTick`, `onNearbyBlockChange`,
//! `onScheduledUpdate` and bone meal of grass, mycelium, nylium, netherrack, dirt, farmland,
//! grass paths, ice, frosted ice, snow, leaves, redstone ore, budding amethyst, coral, concrete
//! powder, cauldrons and campfires), and copper oxidation, which PMMP does not have (dragonfly
//! `attemptOxidation`).

use std::collections::BTreeSet;

use super::props;
use super::support::{self, side};
use super::{Rule, pick};
use crate::block::{self, Class, CopperOxidation, Face, Kind, Trait};
use crate::event::{Actor, EntityTrampleFarmland, FarmlandHydrationChange, LeavesDecay};
use crate::random::Random;
use crate::world::BlockPos;

/// The amethyst growth stages, by Bedrock name.
const AMETHYST_STAGES: [&str; 4] = [
    "minecraft:small_amethyst_bud",
    "minecraft:medium_amethyst_bud",
    "minecraft:large_amethyst_bud",
    "minecraft:amethyst_cluster",
];
/// PMMP `FillableCauldron::MAX_FILL_LEVEL`.
pub(in crate::tick) const CAULDRON_FULL: i32 = 6;
/// The upper bound of living coral's delay to its water check (PMMP `mt_rand(40, 200)`).
const MAX_CORAL_DELAY_TICKS: u32 = 200;
/// The upper bound of frosted ice's delay to its next check (PMMP `mt_rand(20, 40)`).
const MAX_FROSTED_ICE_DELAY_TICKS: u32 = 40;
/// The lower bound of living coral's delay to its water check.
const MIN_CORAL_DELAY_TICKS: u32 = 40;
/// The lower bound of frosted ice's delay to its next check.
const MIN_FROSTED_ICE_DELAY_TICKS: u32 = 20;
/// How far the rules that read only the blocks next to a block reach.
const NEIGHBOURS: i32 = 1;

/// Runs PMMP's `Grass::onRandomTick`: grass in the dark under a light filter dies; in the light
/// it spreads to plain dirt nearby. Both are spreads, as in PMMP.
pub(super) fn grass(rule: &mut Rule<'_>, pos: BlockPos) {
    let Some(up) = pos.step(Face::Up) else {
        return;
    };
    let light_above = rule.full_light(up);
    if light_above < 4 && rule.block(Some(up)).light_filter() >= 2 {
        rule.spread(pos, block::DIRT, pos);
    } else if light_above >= 9 {
        let mut rng = rule.rng();
        for _ in 0..4 {
            let x = rng.range(pos.x - 1, pos.x + 1);
            let y = rng.range(pos.y - 3, pos.y + 1);
            let z = rng.range(pos.z - 1, pos.z + 1);
            let at = BlockPos::new(x, y, z);
            let above = BlockPos::new(x, y + 1, z);
            if rule.get(at).is_some_and(is_plain_dirt)
                && rule.full_light(above) >= 4
                && rule.block(Some(above)).light_filter() < 2
            {
                rule.spread(at, block::GRASS_BLOCK, pos);
            }
        }
    }
}

/// Runs PMMP's `Mycelium::onRandomTick`: mycelium spreads to plain dirt under a transparent
/// block.
pub(super) fn mycelium(rule: &mut Rule<'_>, pos: BlockPos) {
    let mut rng = rule.rng();
    let x = rng.range(pos.x - 1, pos.x + 1);
    let y = rng.range(pos.y - 2, pos.y + 2);
    let z = rng.range(pos.z - 1, pos.z + 1);
    let at = BlockPos::new(x, y, z);
    if rule.get(at).is_some_and(is_plain_dirt)
        && rule.block(at.step(Face::Up)).is(Class::Transparent)
    {
        rule.spread(at, Kind::Mycelium.default_state(), pos);
    }
}

/// Tells whether `s` is plain dirt (PMMP `Dirt` with `DirtType::NORMAL`).
fn is_plain_dirt(s: block::State) -> bool {
    s.name() == "minecraft:dirt"
}

/// Runs PMMP's `Nylium::onRandomTick`: covered nylium turns to netherrack; uncovered spreads to
/// netherrack nearby.
pub(super) fn nylium(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if !rule.block(pos.step(Face::Up)).is_transparent() {
        rule.spread(pos, Kind::Netherrack.default_state(), pos);
        return;
    }
    let mut rng = rule.rng();
    for _ in 0..4 {
        let (dx, dy, dz) = (rng.range(-1, 1), rng.range(-2, 0), rng.range(-1, 1));
        let Some(at) = pos.offset(dx, dy, dz) else {
            continue;
        };
        if rule.get(at).is_some_and(|s| s.kind() == Kind::Netherrack)
            && rule.block(at.step(Face::Up)).is_transparent()
        {
            rule.spread(at, block, pos);
        }
    }
}

/// Runs PMMP's `Ice::onRandomTick`: ice melts next to block light of 12 or more.
pub(super) fn ice(rule: &mut Rule<'_>, pos: BlockPos) {
    if adjacent_block_light(rule, pos) >= 12 {
        rule.melt(pos, Kind::Water.default_state());
    }
}

/// Reads the brightest block light next to `pos` (PMMP `getHighestAdjacentBlockLight`).
fn adjacent_block_light(rule: &Rule<'_>, pos: BlockPos) -> u8 {
    Face::ALL
        .into_iter()
        .filter_map(|f| pos.step(f))
        .map(|p| rule.block_light(p))
        .max()
        .unwrap_or(0)
}

/// Runs PMMP's `FrostedIce::onNearbyBlockChange`: the ice checks again in 20 to 40 ticks, with a
/// random stream of its own.
pub(super) fn frosted_ice_on_nearby(rule: &mut Rule<'_>, pos: BlockPos) {
    let mut rng = rule.rng();
    rule.schedule_random(
        &mut rng,
        pos,
        MIN_FROSTED_ICE_DELAY_TICKS,
        MAX_FROSTED_ICE_DELAY_TICKS,
    );
}

/// Runs PMMP's `FrostedIce::onRandomTick` (and `onScheduledUpdate`). Ice next to a chunk that is
/// not loaded checks again later.
pub(super) fn frosted_ice(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    let Some(age) = props::int(block, "age") else {
        return;
    };
    if !rule.is_area_loaded(pos, NEIGHBOURS) {
        frosted_ice_on_nearby(rule, pos);
        return;
    }
    let mut found = 0;
    let mut crowded = false;
    'scan: for x in -1..=1 {
        for z in -1..=1 {
            if (x != 0 || z != 0) && rule.block(pos.offset(x, 0, z)).class() == Class::FrostedIce {
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
        .map(|p| rule.full_light(p))
        .max()
        .unwrap_or(0);
    let mut rng = rule.rng();
    if (!crowded || rng.range(0, 2) == 0) && i32::from(light) >= 12 - age {
        if melt_frosted(rule, &mut rng, pos, block) {
            for face in Face::ALL {
                if let Some(p) = pos.step(face)
                    && let Some(s) = rule.get(p)
                    && s.class() == Class::FrostedIce
                {
                    melt_frosted(rule, &mut rng, p, s);
                }
            }
        }
    } else {
        rule.schedule_random(
            &mut rng,
            pos,
            MIN_FROSTED_ICE_DELAY_TICKS,
            MAX_FROSTED_ICE_DELAY_TICKS,
        );
    }
}

/// Runs PMMP's `FrostedIce::tryMelt`: the ice gets older, or water at the last age. Whether it
/// reached the last age (melted, unless a listener kept it).
fn melt_frosted(rule: &mut Rule<'_>, rng: &mut Random, pos: BlockPos, block: block::State) -> bool {
    let age = props::int(block, "age").unwrap_or(3);
    if age >= 3 {
        rule.melt(pos, Kind::Water.default_state());
        return true;
    }
    if let Some(s) = props::with_int(block, "age", age + 1) {
        rule.set(pos, s);
    }
    rule.schedule_random(
        rng,
        pos,
        MIN_FROSTED_ICE_DELAY_TICKS,
        MAX_FROSTED_ICE_DELAY_TICKS,
    );
    false
}

/// Runs PMMP's `SnowLayer::onRandomTick`: snow melts in block light of 12 or more.
pub(super) fn snow_layer(rule: &mut Rule<'_>, pos: BlockPos) {
    if rule.block_light(pos) >= 12 {
        rule.melt(pos, block::AIR);
    }
}

/// Runs PMMP's `Farmland::onRandomTick`: farmland gets wet near water (9 × 9 × 2 above its
/// level), dries otherwise, then turns to dirt.
pub(super) fn farmland(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    let wetness = props::int(block, "moisturized_amount").unwrap_or(0);
    let mut water = false;
    'search: for y in 0..2 {
        for x in -4..=4 {
            for z in -4..=4 {
                if rule.block(pos.offset(x, y, z)).class() == Class::Water {
                    water = true;
                    break 'search;
                }
            }
        }
    }
    if water {
        let wettest = i32::from(FarmlandHydrationChange::MAX_HYDRATION);
        if wetness < wettest {
            hydrate(rule, pos, block, wetness, wettest);
        }
    } else if wetness > 0 {
        hydrate(rule, pos, block, wetness, wetness - 1);
    } else {
        rule.set(pos, block::DIRT);
    }
}

/// Changes farmland's wetness from `old` to `new` (0 to 7), raising
/// [`FarmlandHydrationChange`], whose listeners may set another wetness.
fn hydrate(rule: &mut Rule<'_>, pos: BlockPos, block: block::State, old: i32, new: i32) {
    #[expect(
        clippy::expect_used,
        reason = "invariant: a farmland wetness is 0 to 7, the values of its property"
    )]
    let wetness = |v: i32| u8::try_from(v).expect("a wetness fits u8");
    let Ok(event) = rule.ask(
        [pos],
        |rule| FarmlandHydrationChange {
            world: rule.world_id(),
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
    let new = event.as_ref().map_or(new, |e| i32::from(e.new_hydration()));
    if let Some(s) = props::with_int(block, "moisturized_amount", new)
        && rule.set_quietly(pos, s)
    {
        rule.tell(event);
    }
}

/// Runs PMMP's `Farmland`/`GrassPath::onNearbyBlockChange`: a solid block on top turns the
/// block to dirt.
pub(super) fn trample_check(rule: &mut Rule<'_>, pos: BlockPos) {
    if pos
        .step(Face::Up)
        .and_then(|p| rule.get(p))
        .is_some_and(block::State::is_solid)
    {
        rule.set(pos, block::DIRT);
    }
}

/// Runs PMMP's `Leaves::onNearbyBlockChange`: leaves that decay start checking for a log.
pub(super) fn leaves_on_nearby(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if props::flag(block, "persistent_bit") == Some(false)
        && props::flag(block, "update_bit") == Some(false)
        && let Some(s) = props::with_flag(block, "update_bit", true)
    {
        rule.set_quietly(pos, s);
    }
}

/// Runs PMMP's `Leaves::onRandomTick`: leaves without a log within four leaves decay, raising
/// [`LeavesDecay`]; leaves that find a log, or whose decay is cancelled, stop checking until a
/// neighbour changes. PMMP raises the event before it looks for a log, so for leaves that do not
/// decay too: Anchor raises it only for leaves that decay.
pub(super) fn leaves_on_random(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if props::flag(block, "persistent_bit") != Some(false)
        || props::flag(block, "update_bit") != Some(true)
    {
        return;
    }
    let mut visited = BTreeSet::new();
    if !find_log(rule, pos, &mut visited, 0) {
        let asked = rule.ask(
            [pos],
            |rule| LeavesDecay {
                world: rule.world_id(),
                pos,
                block,
                cancelled: false,
            },
            LeavesDecay::is_cancelled,
        );
        if let Ok(event) = asked {
            rule.destroy(pos);
            rule.tell(event);
            return;
        }
        // Cancelled: the leaves stay, and a listener that put another block there keeps it.
        if rule.get(pos) != Some(block) {
            return;
        }
    }
    if let Some(s) = props::with_flag(block, "update_bit", false) {
        rule.set_quietly(pos, s);
    }
}

/// Searches for a log through leaves up to distance 4, depth first (PMMP `Leaves::findLog`; at
/// most the positions within 5 blocks).
fn find_log(
    rule: &Rule<'_>,
    pos: BlockPos,
    visited: &mut BTreeSet<BlockPos>,
    distance: i32,
) -> bool {
    if !visited.insert(pos) {
        return false;
    }
    let s = rule.block(Some(pos));
    if s.is(Class::Wood) {
        return true;
    }
    if s.is(Class::Leaves) && distance <= 4 {
        for face in Face::ALL {
            if let Some(next) = pos.step(face)
                && find_log(rule, next, visited, distance + 1)
            {
                return true;
            }
        }
    }
    false
}

/// Runs PMMP's `RedstoneOre::onNearbyBlockChange`: the ore lights up.
pub(super) fn redstone_ore_on_nearby(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if let Some(lit) = props::with_lit(block, true)
        && lit != block
    {
        rule.set(pos, lit);
    }
}

/// Runs PMMP's `RedstoneOre::onRandomTick`: the ore goes out.
pub(super) fn redstone_ore_on_random(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if let Some(unlit) = props::with_lit(block, false)
        && unlit != block
    {
        rule.set(pos, unlit);
    }
}

/// Runs PMMP's `BuddingAmethyst::onRandomTick`: one in five, a bud on a random face grows a
/// stage.
pub(super) fn budding_amethyst(rule: &mut Rule<'_>, pos: BlockPos) {
    let mut rng = rule.rng();
    if rng.range(1, 5) != 1 {
        return;
    }
    let face = pick(&mut rng, &Face::ALL);
    let Some(at) = pos.step(face) else {
        return;
    };
    let Some(adjacent) = rule.get(at) else {
        return;
    };
    let stage = if adjacent.kind() == Kind::Air {
        Some(0)
    } else if adjacent.kind() == Kind::AmethystCluster && props::facing(adjacent) == Some(face) {
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
    let face_value = block::PropertyValue::String(props::face_name(face));
    let Some(bud) = block::State::find(name, &[("minecraft:block_face", face_value)]) else {
        return;
    };
    rule.grow(at, bud, pos);
}

/// Tells whether the block is unwaxed copper that can oxidize further: it takes random ticks.
pub(super) fn is_oxidizable(block: block::State) -> bool {
    oxidation(block).is_some_and(|l| l.next().is_some())
}

/// Runs dragonfly's `attemptOxidation` (vanilla's rule; PMMP does not oxidize copper): a chance
/// of 64/1125, then by how many unwaxed copper blocks within four blocks (taxicab) are more
/// oxidized, unless one is less oxidized. A door oxidizes both halves.
pub(super) fn oxidize(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    let Some((level, next_level)) = oxidation(block).and_then(|l| Some((l, l.next()?))) else {
        return;
    };
    let mut rng = rule.rng();
    if rng.unit() > 64.0 / 1125.0 {
        return;
    }
    let (mut all, mut higher) = (0_u32, 0_u32);
    for x in -4_i32..=4 {
        for y in -4_i32..=4 {
            for z in -4_i32..=4 {
                if (x == 0 && y == 0 && z == 0) || x.abs() + y.abs() + z.abs() > 4 {
                    continue;
                }
                let Some(other) = oxidation(rule.block(pos.offset(x, y, z))) else {
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
    let Some(next) = props::renamed(block, &name) else {
        return;
    };
    rule.set(pos, next);
    if block.is(Class::Door)
        && let Some(top) = props::flag(block, "upper_block_bit")
        && let Some(other_pos) = pos.step(if top { Face::Down } else { Face::Up })
        && let Some(other) = rule.get(other_pos)
        && other.kind() == block.kind()
        && oxidation(other) == Some(level)
        && let Some(other_next) = props::renamed(other, &name)
    {
        rule.set(other_pos, other_next);
    }
}

/// Reads the oxidation stage of an unwaxed copper block, if it is one.
fn oxidation(block: block::State) -> Option<CopperOxidation> {
    if !block.has_trait(Trait::Copper) {
        return None;
    }
    let (stage, waxed, _) = block::copper_parts(block)?;
    (!waxed).then_some(stage)
}

/// Runs PMMP's `BaseCoral`/`CoralBlock::onNearbyBlockChange`: coral without support breaks;
/// living coral checks for water in 40 to 200 ticks.
pub(super) fn coral_on_nearby(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if block.class() != Class::CoralBlock {
        match support::supported(rule, pos, block) {
            Some(false) => {
                rule.destroy(pos);
                return;
            }
            Some(true) => {}
            None => return,
        }
    }
    if !props::is_dead_coral(block) {
        let mut rng = rule.rng();
        rule.schedule_random(&mut rng, pos, MIN_CORAL_DELAY_TICKS, MAX_CORAL_DELAY_TICKS);
    }
}

/// Runs PMMP's `BaseCoral`/`CoralBlock::onScheduledUpdate`: living coral without water next to
/// it dies.
pub(super) fn coral_check(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if props::is_dead_coral(block) {
        return;
    }
    let mut water = false;
    for face in Face::ALL {
        match side(rule, pos, face) {
            Some(s) if s.class() == Class::Water => water = true,
            Some(_) => {}
            None => return,
        }
    }
    if !water && let Some(dead) = props::dead_coral(block) {
        rule.die(pos, dead);
    }
}

/// Runs PMMP's `ConcretePowder::onNearbyBlockChange`: water beside or above (the first in
/// PMMP's face order) hardens the powder; otherwise it falls. Powder next to a chunk that is not
/// loaded stays as it is.
pub(super) fn concrete_powder(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if !rule.is_area_loaded(pos, NEIGHBOURS) {
        return;
    }
    let water = [Face::Up, Face::North, Face::South, Face::West, Face::East]
        .into_iter()
        .filter_map(|f| pos.step(f))
        .find(|p| rule.block(Some(*p)).class() == Class::Water);
    if let Some(water) = water {
        if let Some(concrete) = block.name().strip_suffix("_powder").and_then(props::plain) {
            rule.form(pos, concrete, water);
        }
    } else {
        support::fall_check(rule, pos, block);
    }
}

/// Runs PMMP's `Cauldron`/`WaterCauldron`/`PotionCauldron::onNearbyBlockChange`: water above
/// fills the cauldron with plain water. A cauldron's dye and potion live in its block entity
/// (`block_entity::Kind::Cauldron`) and are cleared here
/// ([`crate::tick::interact::clear_cauldron`]).
pub(super) fn cauldron_on_nearby(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if rule.block(pos.step(Face::Up)).kind() != Kind::Water {
        return;
    }
    if props::int(block, "fill_level").is_none_or(|f| f < CAULDRON_FULL)
        && let Some(full) = props::with_int(block, "fill_level", CAULDRON_FULL)
            .and_then(|s| props::with_string(s, "cauldron_liquid", "water"))
    {
        rule.set(pos, full);
    }
    crate::tick::interact::clear_cauldron(rule.state, rule.world, pos);
}

/// Runs PMMP's `Campfire::onNearbyBlockChange`: water above puts a lit campfire out.
pub(super) fn campfire_on_nearby(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    if props::flag(block, "extinguished") == Some(false)
        && rule.block(pos.step(Face::Up)).kind() == Kind::Water
        && let Some(out) = props::with_flag(block, "extinguished", true)
    {
        rule.set(pos, out);
    }
}

/// Grows a ground block with bone meal; whether the bone meal is used up.
pub(super) fn fertilize(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) -> bool {
    match block.class() {
        Class::Dirt => {
            if block.name() != "minecraft:dirt_with_roots" {
                return false;
            }
            let Some(below) = pos.step(Face::Down) else {
                return false;
            };
            if rule.get(below).is_none_or(|d| d.kind() != Kind::Air) {
                return false;
            }
            rule.set(below, Kind::HangingRoots.default_state());
            true
        }
        Class::Grass => {
            if rule.block(pos.step(Face::Up)).kind() != Kind::Air {
                return false;
            }
            grow_grass(rule, pos);
            true
        }
        Class::Netherrack => netherrack(rule, pos),
        Class::Nylium => {
            if rule.block(pos.step(Face::Up)).kind() != Kind::Air {
                return false;
            }
            nylium_vegetation(rule, pos, block);
            true
        }
        _ => false,
    }
}

/// Runs PMMP's `TallGrass::growGrass` with bone meal's count 8 and radius 2.
fn grow_grass(rule: &mut Rule<'_>, pos: BlockPos) {
    let plants = [
        Kind::Dandelion,
        Kind::Poppy,
        Kind::TallGrass,
        Kind::TallGrass,
        Kind::TallGrass,
        Kind::TallGrass,
    ];
    let mut rng = rule.rng();
    for _ in 0..8 {
        let x = rng.range(pos.x - 2, pos.x + 2);
        let z = rng.range(pos.z - 2, pos.z + 2);
        let ground = BlockPos::new(x, pos.y, z);
        let above = BlockPos::new(x, pos.y + 1, z);
        if rule.get(above).is_some_and(|s| s.kind() == Kind::Air)
            && rule.get(ground).is_some_and(|s| s.kind() == Kind::Grass)
        {
            let plant = pick(&mut rng, &plants);
            rule.set(above, plant.default_state());
        }
    }
}

/// Runs PMMP's `Netherrack::tryTransform`: nylium next to the netherrack spreads onto it.
fn netherrack(rule: &mut Rule<'_>, pos: BlockPos) -> bool {
    if !rule.block(pos.step(Face::Up)).is_transparent() {
        return false;
    }
    let (mut warped, mut crimson) = (false, false);
    for x in -1..=1 {
        for y in -1..=1 {
            for z in -1..=1 {
                match rule.block(pos.offset(x, y, z)).kind() {
                    Kind::CrimsonNylium => crimson = true,
                    Kind::WarpedNylium => warped = true,
                    _ => {}
                }
            }
        }
    }
    let kind = match (warped, crimson) {
        (true, true) => {
            if rule.rng().range(0, 1) == 0 {
                Kind::WarpedNylium
            } else {
                Kind::CrimsonNylium
            }
        }
        (true, false) => Kind::WarpedNylium,
        (false, true) => Kind::CrimsonNylium,
        (false, false) => return false,
    };
    rule.set(pos, kind.default_state());
    true
}

/// Runs PMMP's `Nylium::growVegetation`: fungi and roots grow on the 5 × 5 around.
fn nylium_vegetation(rule: &mut Rule<'_>, pos: BlockPos, block: block::State) {
    let plants: &[Kind] = if block.kind() == Kind::CrimsonNylium {
        &[Kind::CrimsonFungus, Kind::CrimsonRoots]
    } else {
        &[Kind::WarpedFungus, Kind::WarpedRoots, Kind::NetherSprouts]
    };
    let mut rng = rule.rng();
    for x in -2..=2 {
        for z in -2..=2 {
            if rng.below(3) != 0 {
                continue;
            }
            let place = pick(&mut rng, plants).default_state();
            let Some(at) = pos.offset(x, 1, z) else {
                continue;
            };
            if rule.get(at).is_some_and(|s| s.kind() == Kind::Air)
                && support::supported(rule, at, place) == Some(true)
            {
                rule.set(at, place);
            }
        }
    }
}

/// Tramples farmland that `entity` landed on after falling `fall_distance` blocks: a living one
/// turns it to dirt with PMMP's chance (`getRandomFloat() < fallDistance - 0.5`), raising
/// [`EntityTrampleFarmland`].
pub(super) fn trample(rule: &mut Rule<'_>, pos: BlockPos, fall_distance: f64, entity: Actor) {
    let Some(block) = rule.get(pos).filter(|s| s.kind() == Kind::Farmland) else {
        return;
    };
    let tramples = rule.rng().unit() < fall_distance - 0.5;
    if !tramples {
        return;
    }
    let Ok(event) = rule.ask(
        [pos],
        |rule| EntityTrampleFarmland {
            entity,
            world: rule.world_id(),
            pos,
            block,
            cancelled: false,
        },
        EntityTrampleFarmland::is_cancelled,
    ) else {
        return;
    };
    if rule.set(pos, block::DIRT) {
        rule.tell(event);
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
    fn amethyst_buds_are_found_by_name_and_face() {
        for name in AMETHYST_STAGES {
            for face in Face::ALL {
                let face_name = props::face_name(face);
                let scanned = block::State::all().find(|s| {
                    s.name() == name && props::string(*s, "minecraft:block_face") == Some(face_name)
                });
                assert!(scanned.is_some());
                let found = block::State::find(
                    name,
                    &[(
                        "minecraft:block_face",
                        block::PropertyValue::String(face_name),
                    )],
                );
                assert_eq!(found, scanned, "{name} {face_name}");
            }
        }
    }

    #[test]
    fn redstone_ore_and_concrete_names_convert() {
        let ore = named("minecraft:deepslate_redstone_ore");
        assert_eq!(
            props::with_lit(ore, true).unwrap().name(),
            "minecraft:lit_deepslate_redstone_ore"
        );
        let lit = named("minecraft:lit_redstone_ore");
        assert_eq!(
            props::with_lit(lit, false).unwrap().name(),
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
