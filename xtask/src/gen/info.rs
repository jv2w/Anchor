//! The gameplay data of the PMMP dump: the JSON forms of block physics, block and
//! item data and the tables of `tables.json`, with the checks and conversions the generators
//! share. `data` loads them; `blocks`, `items`, `effects`, `drops` and `recipes` emit them.

use serde::Deserialize;
use serde_json::Value as Json;

use super::body::int_lit;

type R<T> = Result<T, String>;

/// PMMP `BlockBreakInfo`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Break {
    pub(crate) hardness: f64,
    pub(crate) blast_resistance: f64,
    /// `BlockToolType` bits.
    pub(crate) tool_type: u8,
    pub(crate) harvest_level: u8,
    pub(crate) explosion_harvestable: bool,
}

/// The block flags of the dump, in the order of their bits in the generated tables (bit 0 first).
pub(crate) const BLOCK_FLAGS: [&str; 8] = [
    "solid",
    "transparent",
    "replaceable",
    "flowable",
    "climbable",
    "entity_collision",
    "silk_touch",
    "position_offset",
];

/// PMMP `SupportType` cases, in the order of their 2-bit codes in the generated tables.
pub(crate) const SUPPORT_TYPES: [&str; 4] = ["full", "center", "edge", "none"];

/// The physical values of a block state: overlapping PMMP states must agree on them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Physics {
    pub(crate) break_info: Break,
    pub(crate) flammability: u8,
    pub(crate) encouragement: u8,
    pub(crate) burns_forever: bool,
    /// [`BLOCK_FLAGS`] bits.
    pub(crate) flags: u8,
    pub(crate) friction: f64,
    /// Type tags, sorted.
    pub(crate) tags: Vec<String>,
    /// [`SUPPORT_TYPES`] codes in PMMP `Facing` order (down, up, north, south, west, east).
    pub(crate) support: [u8; 6],
    /// Block-local collision boxes: min x, y, z, max x, y, z.
    pub(crate) boxes: Vec<[f64; 6]>,
}

impl Physics {
    /// Reads the dumped values. Every float must be exact (see [`exact`]).
    pub(crate) fn new(
        break_info: Break,
        fire: (u8, u8, bool),
        flags: &[String],
        friction: f64,
        tags: &[String],
        support: &[String],
        boxes: Vec<[f64; 6]>,
    ) -> R<Self> {
        let mut bits = 0;
        for flag in flags {
            let at = BLOCK_FLAGS
                .iter()
                .position(|f| f == flag)
                .ok_or_else(|| format!("unknown block flag {flag}"))?;
            bits |= 1 << at;
        }
        for v in [break_info.hardness, break_info.blast_resistance, friction]
            .iter()
            .chain(boxes.iter().flatten())
        {
            exact(*v)?;
        }
        let mut tags = tags.to_vec();
        tags.sort();
        Ok(Self {
            break_info,
            flammability: fire.0,
            encouragement: fire.1,
            burns_forever: fire.2,
            flags: bits,
            friction,
            tags,
            support: support_codes(support)?,
            boxes,
        })
    }

    /// The horizontal faces with full support, as bits north 1, east 2, south 4, west 8 (the
    /// connection table's order).
    pub(crate) fn full_faces(&self) -> u8 {
        // Support order: down, up, north, south, west, east.
        let full = |i: usize| u8::from(self.support.get(i) == Some(&0));
        full(2) | full(5) << 1 | full(3) << 2 | full(4) << 3
    }
}

/// [`SUPPORT_TYPES`] codes of six dumped support types.
pub(crate) fn support_codes(support: &[String]) -> R<[u8; 6]> {
    let mut codes = [0; 6];
    if support.len() != 6 {
        return Err(format!("{} support types, not 6", support.len()));
    }
    for (code, name) in codes.iter_mut().zip(support) {
        *code = SUPPORT_TYPES
            .iter()
            .position(|s| s == name)
            .and_then(|p| u8::try_from(p).ok())
            .ok_or_else(|| format!("unknown support type {name}"))?;
    }
    Ok(codes)
}

/// What PMMP type a state is: data a block entity may tell apart where PMMP states share a
/// Bedrock state (see `data::SHARED`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Identity {
    /// The `VanillaBlocks` name, lowercase.
    pub(crate) kind: String,
    /// The PMMP class and its parents, leaf first, ending with `Block`.
    pub(crate) classes: Vec<String>,
    /// PMMP traits and interfaces, sorted.
    pub(crate) traits: Vec<String>,
}

/// A float the generated code writes as its shortest decimal form, which must be one that
/// `serde_json` reads exactly: at most 15 significant digits, so the significand fits in 53 bits
/// and the reading is one correctly rounded division by a power of ten.
pub(crate) fn exact(v: f64) -> R<f64> {
    let text = format!("{v:e}");
    let digits = text
        .split('e')
        .next()
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_digit)
        .count();
    if !v.is_finite() || digits > 15 {
        return Err(format!(
            "{v} has more than 15 significant digits or is not finite"
        ));
    }
    Ok(v)
}

/// Rust source of an `f64` literal: the shortest exact form, with integer digits grouped by three
/// from the right and fraction digits by three from the left when clippy calls them long (more
/// than five).
pub(crate) fn f64_lit(v: f64) -> String {
    let text = format!("{v:?}");
    let (int, rest) = text.split_at(text.find(['.', 'e']).unwrap_or(text.len()));
    let int = int_lit(int);
    let (fraction, exponent) = match rest.strip_prefix('.') {
        Some(r) => r.split_at(r.find('e').unwrap_or(r.len())),
        None => ("", rest),
    };
    let mut grouped = String::with_capacity(fraction.len() + fraction.len() / 3);
    for (i, c) in fraction.chars().enumerate() {
        if i > 0 && i.is_multiple_of(3) && fraction.len() > 5 {
            grouped.push('_');
        }
        grouped.push(c);
    }
    let point = if rest.starts_with('.') { "." } else { "" };
    format!("{int}{point}{grouped}{exponent}")
}

/// The dumped item data (`items.json` `data`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ItemData {
    pub(crate) kind: Option<String>,
    pub(crate) classes: Vec<String>,
    pub(crate) traits: Vec<String>,
    pub(crate) max_stack: u8,
    pub(crate) max_durability: Option<u16>,
    pub(crate) fire_proof: bool,
    pub(crate) attack_points: u8,
    pub(crate) defense_points: u8,
    pub(crate) tool: ToolJson,
    pub(crate) armor: Option<ArmorJson>,
    pub(crate) food: Option<FoodJson>,
    pub(crate) consumable: Option<ConsumableJson>,
    pub(crate) fuel: FuelJson,
    pub(crate) enchantability: u8,
    /// Kept for reading; the generated tables use the enchantments PMMP derives from them.
    #[serde(rename = "enchantment_tags")]
    pub(crate) _enchantment_tags: Vec<String>,
    pub(crate) enchantments: EnchantmentsJson,
    pub(crate) cooldown: Option<CooldownJson>,
    pub(crate) throw_force: Option<f64>,
    pub(crate) potion: Option<String>,
    /// The Bedrock state placing the item puts down (PMMP `Item::getBlock()`).
    pub(crate) places: Option<Json>,
    /// The state it puts down against a side, where that differs (`getBlock(Facing::NORTH)`).
    pub(crate) places_on_wall: Option<Json>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolJson {
    #[serde(rename = "type")]
    pub(crate) tool_type: u8,
    pub(crate) harvest_level: u8,
    pub(crate) efficiency: f64,
    pub(crate) tier: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArmorJson {
    pub(crate) slot: u8,
    pub(crate) toughness: u8,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FoodJson {
    pub(crate) food: u8,
    pub(crate) saturation: f64,
    pub(crate) requires_hunger: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConsumableJson {
    pub(crate) residue: Option<Json>,
    /// A list of effects, or the string "random".
    pub(crate) effects: Json,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FuelJson {
    pub(crate) time: u16,
    pub(crate) residue: Option<Json>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnchantmentsJson {
    pub(crate) primary: Vec<String>,
    pub(crate) all: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CooldownJson {
    pub(crate) ticks: u16,
    pub(crate) tag: Option<String>,
}

/// An effect instance (PMMP `EffectInstance`) of a potion or food.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EffectJson {
    pub(crate) effect: String,
    pub(crate) duration: u32,
    pub(crate) amplifier: u8,
    pub(crate) visible: bool,
    pub(crate) ambient: bool,
}

/// `tables.json`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Tables {
    pub(crate) effects: Vec<EffectType>,
    pub(crate) potions: Vec<PotionJson>,
    pub(crate) enchantments: Vec<EnchantmentJson>,
    /// PMMP `ItemEnchantmentTagRegistry`: kept in the dump; the tables use the enchantments PMMP
    /// derives from the tags for each item type.
    #[serde(rename = "enchantment_tags")]
    pub(crate) _enchantment_tags: Vec<Json>,
    pub(crate) item_tags: Vec<ItemTagJson>,
    pub(crate) banner_patterns: Vec<BannerJson>,
    pub(crate) furnace_types: Vec<FurnaceJson>,
    pub(crate) tool_tiers: Vec<TierJson>,
}

/// PMMP `ToolTier`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TierJson {
    pub(crate) name: String,
    pub(crate) harvest_level: u8,
    pub(crate) max_durability: u16,
    pub(crate) base_attack_points: u8,
    pub(crate) base_efficiency: u8,
    pub(crate) enchantability: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EffectType {
    pub(crate) name: String,
    pub(crate) id: u8,
    pub(crate) text: String,
    #[serde(rename = "classes")]
    pub(crate) _classes: Vec<String>,
    pub(crate) colour: Colour,
    pub(crate) bad: bool,
    pub(crate) instant: bool,
    pub(crate) default_duration: u32,
    pub(crate) bubbles: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Colour {
    pub(crate) r: u8,
    pub(crate) g: u8,
    pub(crate) b: u8,
    pub(crate) a: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PotionJson {
    pub(crate) name: String,
    pub(crate) id: u8,
    pub(crate) text: String,
    pub(crate) effects: Vec<EffectJson>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnchantmentJson {
    pub(crate) name: String,
    pub(crate) id: u8,
    pub(crate) text: String,
    #[serde(rename = "classes")]
    pub(crate) _classes: Vec<String>,
    pub(crate) rarity: u8,
    pub(crate) max_level: u8,
    pub(crate) available: bool,
    /// Kept in the dump; item types carry the enchantments PMMP derives from the tags.
    #[serde(rename = "primary_tags")]
    pub(crate) _primary_tags: Vec<String>,
    #[serde(rename = "secondary_tags")]
    pub(crate) _secondary_tags: Vec<String>,
    pub(crate) incompatible: Vec<String>,
    /// (min, max) enchanting power of each level from 1.
    pub(crate) power: Vec<(u16, u16)>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ItemTagJson {
    pub(crate) tag: String,
    pub(crate) items: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BannerJson {
    pub(crate) name: String,
    pub(crate) id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FurnaceJson {
    pub(crate) name: String,
    pub(crate) cook_ticks: u16,
}

/// The names of `list` as indices into `names`, as a bit set; fails on an unknown name or more
/// than 64 names.
pub(crate) fn bit_set(list: &[String], names: &[&str], what: &str) -> R<u64> {
    let mut bits = 0_u64;
    for name in list {
        let at = names
            .iter()
            .position(|n| n == name)
            .ok_or_else(|| format!("unknown {what} {name}"))?;
        bits |= 1_u64
            .checked_shl(u32::try_from(at).map_err(|_| "bit index")?)
            .ok_or_else(|| format!("more than 64 {what}s"))?;
    }
    Ok(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_are_written_exactly_and_readably() {
        assert_eq!(f64_lit(1.5), "1.5");
        assert_eq!(f64_lit(30.0), "30.0");
        assert_eq!(f64_lit(18_000_003.75), "18_000_003.75");
        assert_eq!(f64_lit(-0.0625), "-0.0625");
        assert_eq!(f64_lit(0.015_625), "0.015_625");
        assert_eq!(f64_lit(0.001_953_125), "0.001_953_125");
        assert!(exact(0.6).is_ok());
        assert!(exact(1.0 / 3.0).is_err());
        assert!(exact(f64::NAN).is_err());
    }

    #[test]
    fn full_faces_follow_the_connection_bits() {
        let p = Physics::new(
            Break {
                hardness: 1.5,
                blast_resistance: 30.0,
                tool_type: 4,
                harvest_level: 1,
                explosion_harvestable: true,
            },
            (0, 0, false),
            &["solid".to_owned()],
            0.6,
            &[],
            &["none", "none", "full", "none", "none", "full"].map(str::to_owned),
            Vec::new(),
        )
        .unwrap();
        // North (index 2) is bit 1, east (index 5) bit 2.
        assert_eq!(p.full_faces(), 0b0011);
        assert_eq!(p.flags, 1);
    }
}
