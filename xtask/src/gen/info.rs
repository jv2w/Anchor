//! The gameplay data of the dump: the JSON forms of block physics, block and item data and the
//! tables of `tables.json`, with the checks and conversions the generators share. `data` loads
//! them; `blocks`, `items` and `effects` emit them.

use serde::Deserialize;
use serde_json::Value as Json;

use super::body::int_lit;

/// The block flags of the dump, in the order of their bits in the generated tables (bit 0 first).
const BLOCK_FLAGS: [&str; 5] = [
    "solid",
    "transparent",
    "replaceable",
    "flowable",
    "entity_collision",
];

/// The tool type bits of a set holding every tool (every `block::ToolType` bit).
pub(super) const EVERY_TOOL: u8 = 0b11_1111;

/// The support types of the dump, in the order of their 2-bit codes in the generated tables.
const SUPPORT_TYPES: [&str; 4] = ["full", "center", "edge", "none"];

/// The break info of a block.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Break {
    pub(super) hardness: f64,
    pub(super) blast_resistance: f64,
    /// The tool type bits.
    pub(super) tool_type: u8,
    pub(super) harvest_level: u8,
}

/// The physical values of a block state: overlapping dump states must agree on them.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Physics {
    pub(super) break_info: Break,
    /// [`BLOCK_FLAGS`] bits.
    pub(super) flags: u8,
    pub(super) friction: f64,
    /// Type tags, sorted.
    pub(super) tags: Vec<String>,
    /// [`SUPPORT_TYPES`] codes in the dump's `Facing` order (down, up, north, south, west, east).
    pub(super) support: [u8; 6],
    /// Block-local collision boxes: min x, y, z, max x, y, z.
    pub(super) boxes: Vec<[f64; 6]>,
}

impl Physics {
    /// The dumped values as block physics. Every float must be exact (see [`exact`]).
    pub(super) fn new(
        break_info: Break,
        flags: &[String],
        friction: f64,
        tags: &[String],
        support: &[String],
        boxes: Vec<[f64; 6]>,
    ) -> Result<Self, String> {
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
            flags: bits,
            friction,
            tags,
            support: support_codes(support)?,
            boxes,
        })
    }

    /// The horizontal faces with full support, as bits north 1, east 2, south 4, west 8 (the
    /// connection table's order).
    pub(super) fn full_faces(&self) -> u8 {
        // Support order: down, up, north, south, west, east.
        let full = |i: usize| u8::from(self.support.get(i) == Some(&0));
        full(2) | full(5) << 1 | full(3) << 2 | full(4) << 3
    }
}

/// What block type a state is: data a block entity may tell apart where dump states share a
/// Bedrock state (see `data::SHARED`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Identity {
    /// The block type's name, lowercase.
    pub(super) kind: String,
    /// The class and its parents, leaf first, ending with `Block`.
    pub(super) classes: Vec<String>,
    /// Traits and interfaces, sorted.
    pub(super) traits: Vec<String>,
}

/// The dumped item data (`items.json` `data`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ItemData {
    pub(super) kind: Option<String>,
    pub(super) classes: Vec<String>,
    pub(super) max_stack: u8,
    pub(super) max_durability: Option<u16>,
    pub(super) fire_proof: bool,
    pub(super) attack_points: u8,
    pub(super) defense_points: u8,
    pub(super) tool: ToolJson,
    pub(super) armor: Option<ArmorJson>,
    pub(super) food: Option<FoodJson>,
    pub(super) consumable: Option<ConsumableJson>,
    pub(super) cooldown: Option<CooldownJson>,
    pub(super) throw_force: Option<f64>,
    pub(super) potion: Option<String>,
    /// The Bedrock state placing the item puts down.
    pub(super) places: Option<Json>,
    /// The state it puts down against a side, where that differs.
    pub(super) places_on_wall: Option<Json>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ToolJson {
    #[serde(rename = "type")]
    pub(super) tool_type: u8,
    pub(super) harvest_level: u8,
    pub(super) efficiency: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ArmorJson {
    pub(super) slot: u8,
    pub(super) toughness: u8,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FoodJson {
    pub(super) food: u8,
    pub(super) saturation: f64,
    pub(super) requires_hunger: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ConsumableJson {
    pub(super) residue: Option<Json>,
    /// A list of effects, or the string "random".
    pub(super) effects: Json,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CooldownJson {
    pub(super) ticks: u16,
    pub(super) tag: Option<String>,
}

/// An effect instance of a potion or food.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EffectJson {
    pub(super) effect: String,
    pub(super) duration: u32,
    pub(super) amplifier: u8,
    pub(super) visible: bool,
    pub(super) ambient: bool,
}

/// `tables.json`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Tables {
    pub(super) effects: Vec<EffectType>,
    pub(super) potions: Vec<PotionJson>,
    pub(super) enchantments: Vec<EnchantmentJson>,
    pub(super) banner_patterns: Vec<BannerJson>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EffectType {
    pub(super) name: String,
    pub(super) id: u8,
    pub(super) text: String,
    #[serde(rename = "classes")]
    pub(super) _classes: Vec<String>,
    pub(super) colour: Colour,
    pub(super) bad: bool,
    pub(super) instant: bool,
    pub(super) default_duration: u32,
    pub(super) bubbles: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Colour {
    pub(super) r: u8,
    pub(super) g: u8,
    pub(super) b: u8,
    pub(super) a: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PotionJson {
    pub(super) name: String,
    pub(super) id: u8,
    pub(super) text: String,
    pub(super) effects: Vec<EffectJson>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EnchantmentJson {
    pub(super) name: String,
    pub(super) id: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BannerJson {
    pub(super) name: String,
    pub(super) id: String,
}

/// A float the generated code writes as its shortest decimal form, which must be one that
/// `serde_json` reads exactly: at most 15 significant digits, so the significand fits in 53 bits
/// and the reading is one correctly rounded division by a power of ten.
pub(super) fn exact(v: f64) -> Result<f64, String> {
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

/// [`SUPPORT_TYPES`] codes of six dumped support types.
pub(super) fn support_codes(support: &[String]) -> Result<[u8; 6], String> {
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

/// The names of `list` as indices into `names`, as a bit set; fails on an unknown name or more
/// than 64 names.
pub(super) fn bit_set(list: &[String], names: &[&str], what: &str) -> Result<u64, String> {
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

/// Rust source of an `f64` literal: the shortest exact form, with integer digits grouped by three
/// from the right and fraction digits by three from the left when clippy calls them long (more
/// than five).
pub(super) fn f64_lit(v: f64) -> String {
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
            },
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
