//! The data of the block entities without slots: signs, bed colours, banners, flower pots and mob
//! heads, read from and written to their Bedrock NBT keys.
//!
//! Reading takes the keys a kind owns out of the saved compound; the caller keeps the rest as it
//! came. Values out of range are fixed on load.

use tracing::warn;

use super::Form;
use crate::block;
use crate::nbt::{Compound, List, Value};
use crate::tick::interact::sign;
use crate::tile::{DyeColour, SignText};

/// The colour of a bed whose colour is missing or unknown: red.
pub(in crate::tick) const DEFAULT_BED_COLOUR: u8 = 14;
/// The most patterns a banner keeps; more in saved data are left out.
pub(in crate::tick) const MAX_PATTERNS: usize = 16;
/// Rotations of a mob head on the floor: sixteenths of a turn.
pub(in crate::tick) const SKULL_ROTATIONS: u8 = 16;
/// The mob head blocks by their `SkullType` byte.
pub(in crate::tick) const SKULL_TYPES: [&str; 7] = [
    "minecraft:skeleton_skull",
    "minecraft:wither_skeleton_skull",
    "minecraft:zombie_head",
    "minecraft:player_head",
    "minecraft:creeper_head",
    "minecraft:dragon_head",
    "minecraft:piglin_head",
];

/// A sign or hanging sign.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(in crate::tick) struct Sign {
    pub(in crate::tick) front: SignText,
    pub(in crate::tick) back: SignText,
    pub(in crate::tick) waxed: bool,
    /// A hanging sign (block entity `HangingSign`).
    pub(in crate::tick) hanging: bool,
}

impl Sign {
    /// Reads the front and back compounds, or the 1.2 `Text` blob.
    pub(in crate::tick) fn read(nbt: &mut Compound, hanging: bool) -> Self {
        let front = match take_compound(nbt, sign::FRONT) {
            Some(side) => SignText::read(side),
            None => match nbt.remove(sign::TEXT) {
                Some(Value::String(blob)) => {
                    // The 1.2 format keeps the side's keys at the root, and its glow counts only
                    // once the lighting bug was resolved.
                    let mut side = Compound::from([(sign::TEXT.to_owned(), Value::String(blob))]);
                    if let Some(colour) = nbt.remove("SignTextColor") {
                        side.insert("SignTextColor".to_owned(), colour);
                    }
                    let resolved =
                        take_byte(nbt, "TextIgnoreLegacyBugResolved").is_some_and(|b| b != 0);
                    if let Some(glow) = nbt.remove("IgnoreLighting")
                        && resolved
                    {
                        side.insert("IgnoreLighting".to_owned(), glow);
                    }
                    SignText::read(side)
                }
                _ => SignText::default(),
            },
        };
        let back = take_compound(nbt, sign::BACK).map_or_else(SignText::default, SignText::read);
        let waxed = take_byte(nbt, "IsWaxed").is_some_and(|w| w != 0);
        Self {
            front,
            back,
            waxed,
            hanging,
        }
    }

    /// The saved keys.
    pub(in crate::tick) fn write(&self, nbt: &mut Compound) {
        nbt.insert(sign::FRONT.to_owned(), self.front.write());
        nbt.insert(sign::BACK.to_owned(), self.back.write());
        nbt.insert("IsWaxed".to_owned(), Value::Byte(u8::from(self.waxed)));
    }

    /// The client keys: the saved ones and nobody editing.
    pub(in crate::tick) fn spawn(&self, nbt: &mut Compound) {
        self.write(nbt);
        nbt.insert("LockedForEditingBy".to_owned(), Value::Long(-1));
    }
}

/// A banner: base colour and patterns by the inverted dye colour IDs (black 0 … white 15).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(in crate::tick) struct Banner {
    pub(in crate::tick) base: i32,
    /// Pattern ID (`bo`, `cre`…) and colour, at most [`MAX_PATTERNS`].
    pub(in crate::tick) patterns: Vec<(String, i32)>,
    /// 0 normal, 1 ominous.
    pub(in crate::tick) ty: i32,
}

impl Banner {
    /// Reads the saved keys.
    pub(in crate::tick) fn read(nbt: &mut Compound) -> Self {
        let base = take_int(nbt, "Base")
            .filter(|b| DyeColour::from_banner_id(*b).is_some())
            .unwrap_or(0);
        let patterns = match nbt.remove("Patterns") {
            Some(Value::List(List::Compound(list))) => read_patterns(&list),
            _ => Vec::new(),
        };
        let ty = take_int(nbt, "Type").unwrap_or(0);
        Self { base, patterns, ty }
    }

    /// The saved and client keys.
    pub(in crate::tick) fn write(&self, nbt: &mut Compound) {
        nbt.insert("Base".to_owned(), Value::Int(self.base));
        nbt.insert("Patterns".to_owned(), patterns_value(&self.patterns));
        nbt.insert("Type".to_owned(), Value::Int(self.ty));
    }
}

/// A flower pot's plant.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::tick) enum Plant {
    /// A block state Anchor knows.
    Known(block::State),
    /// A saved plant Anchor does not know, kept as an unknown block in a chunk is: no rule takes it
    /// out or drops it, and clients are not sent it. The `PlantBlock` compound as it came.
    Unknown(Compound),
}

impl SignText {
    /// Reads a side compound (the lighting bug resolved in it).
    fn read(mut nbt: Compound) -> Self {
        let mut side = Self::default();
        if let Some(Value::String(blob)) = nbt.remove(sign::TEXT) {
            for (slot, line) in side.lines.iter_mut().zip(crate::tile::blob_lines(&blob)) {
                line.clone_into(slot);
            }
        }
        if let Some(colour) = take_int(&mut nbt, "SignTextColor") {
            side.colour = colour;
        }
        side.glowing = take_byte(&mut nbt, "IgnoreLighting").is_some_and(|g| g != 0);
        side
    }

    /// The side compound to save.
    fn write(&self) -> Value {
        let text = self.lines.join("\n");
        Value::Compound(Compound::from([
            (
                sign::TEXT.to_owned(),
                Value::String(text.trim_end_matches('\n').to_owned()),
            ),
            ("SignTextColor".to_owned(), Value::Int(self.colour)),
            (
                "IgnoreLighting".to_owned(),
                Value::Byte(u8::from(self.glowing)),
            ),
            ("PersistFormatting".to_owned(), Value::Byte(1)),
        ]))
    }
}

/// A banner's `Patterns` list: entries with a pattern ID, at most [`MAX_PATTERNS`]. An ID Anchor
/// does not know stays, as clients may know it; a missing colour is black, 0.
pub(in crate::tick) fn read_patterns(list: &[Compound]) -> Vec<(String, i32)> {
    let mut out = Vec::with_capacity(list.len().min(MAX_PATTERNS));
    for entry in list {
        let Some(Value::String(id)) = entry.get("Pattern") else {
            continue;
        };
        let colour = match entry.get("Color") {
            Some(Value::Int(c)) if DyeColour::from_banner_id(*c).is_some() => *c,
            _ => 0,
        };
        if out.len() >= MAX_PATTERNS {
            warn!("a banner has more patterns than kept; the rest are left out");
            break;
        }
        out.push((id.clone(), colour));
    }
    out
}

/// A `Patterns` list.
pub(in crate::tick) fn patterns_value(patterns: &[(String, i32)]) -> Value {
    Value::List(List::Compound(
        patterns
            .iter()
            .map(|(id, colour)| {
                Compound::from([
                    ("Pattern".to_owned(), Value::String(id.clone())),
                    ("Color".to_owned(), Value::Int(*colour)),
                ])
            })
            .collect(),
    ))
}

/// A flower pot's plant (`PlantBlock`, a block state compound); `None` for none or air. A state
/// Anchor does not know stays as it came.
pub(in crate::tick) fn read_plant(nbt: &mut Compound) -> Option<Plant> {
    let plant = take_compound(nbt, "PlantBlock")?;
    if let Some(state) = crate::save::find_state(&plant) {
        return (state != block::AIR).then_some(Plant::Known(state));
    }
    Some(Plant::Unknown(plant))
}

/// The `PlantBlock` key of `form`: clients get no plant Anchor does not know.
pub(super) fn write_plant(plant: Option<&Plant>, form: Form, nbt: &mut Compound) {
    let value = match plant {
        Some(Plant::Known(state)) => crate::save::state_nbt(*state),
        Some(Plant::Unknown(saved)) if form == Form::Saved => saved.clone(),
        _ => return,
    };
    nbt.insert("PlantBlock".to_owned(), Value::Compound(value));
}

/// A mob head: the type and the rotation as degrees, or the legacy `Rot`.
pub(in crate::tick) fn read_skull(nbt: &mut Compound) -> (u8, u8) {
    let skull = take_byte(nbt, "SkullType")
        .filter(|t| usize::from(*t) < SKULL_TYPES.len())
        .unwrap_or(0);
    let rotation = match take_float(nbt, "Rotation") {
        Some(yaw) => skull_rotation(yaw),
        None => take_byte(nbt, "Rot")
            .filter(|r| *r < SKULL_ROTATIONS)
            .unwrap_or(0),
    };
    nbt.remove("Rot");
    (skull, rotation)
}

/// A yaw in degrees as sixteenths of a turn, rounded to the nearest.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "safety: the value is finite and reduced into 0..16 first"
)]
pub(in crate::tick) fn skull_rotation(yaw: f32) -> u8 {
    let turns = f64::from(SKULL_ROTATIONS);
    let sixteenths = (f64::from(yaw) * turns / 360.0 + 0.5)
        .floor()
        .rem_euclid(turns);
    (sixteenths as u8) % SKULL_ROTATIONS
}

/// A mob head's keys.
pub(in crate::tick) fn write_skull(skull: u8, rotation: u8, nbt: &mut Compound) {
    nbt.insert("SkullType".to_owned(), Value::Byte(skull));
    nbt.insert(
        "Rotation".to_owned(),
        Value::Float(f32::from(rotation) * 360.0 / f32::from(SKULL_ROTATIONS)),
    );
}

/// A bed's colour (a missing or unknown colour is [`DEFAULT_BED_COLOUR`]).
pub(in crate::tick) fn read_bed(nbt: &mut Compound) -> u8 {
    take_byte(nbt, "color")
        .filter(|c| DyeColour::from_id(*c).is_some())
        .unwrap_or(DEFAULT_BED_COLOUR)
}

/// Takes a `Byte` key's value out of `nbt`.
fn take_byte(nbt: &mut Compound, key: &str) -> Option<u8> {
    match nbt.remove(key)? {
        Value::Byte(v) => Some(v),
        _ => None,
    }
}

/// Takes an `Int` key's value out of `nbt`.
fn take_int(nbt: &mut Compound, key: &str) -> Option<i32> {
    match nbt.remove(key)? {
        Value::Int(v) => Some(v),
        _ => None,
    }
}

/// Takes a finite `Float` key's value out of `nbt`.
fn take_float(nbt: &mut Compound, key: &str) -> Option<f32> {
    match nbt.remove(key)? {
        Value::Float(v) if v.is_finite() => Some(v),
        _ => None,
    }
}

/// Takes a `Compound` key's value out of `nbt`.
fn take_compound(nbt: &mut Compound, key: &str) -> Option<Compound> {
    match nbt.remove(key)? {
        Value::Compound(v) => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_text_round_trips_and_keeps_its_colour_and_glow() {
        let sign = Sign {
            front: SignText {
                lines: ["a".to_owned(), "b".to_owned(), String::new(), String::new()],
                colour: 0xff00_ff00_u32.cast_signed(),
                glowing: true,
            },
            back: SignText::default(),
            waxed: true,
            hanging: false,
        };
        let mut nbt = Compound::new();
        sign.write(&mut nbt);
        assert_eq!(
            nbt.get("FrontText").and_then(|f| match f {
                Value::Compound(c) => c.get("Text").cloned(),
                _ => None,
            }),
            Some(Value::String("a\nb".to_owned()))
        );
        let mut read = nbt.clone();
        assert_eq!(Sign::read(&mut read, false), sign);
        assert!(read.is_empty());
    }

    #[test]
    fn a_legacy_sign_keeps_its_colour_and_its_glow_once_the_lighting_bug_was_resolved() {
        let red = 0xffff_0000_u32.cast_signed();
        for resolved in [false, true] {
            let mut nbt = Compound::from([
                ("Text".to_owned(), Value::String("a\nb".to_owned())),
                ("SignTextColor".to_owned(), Value::Int(red)),
                ("IgnoreLighting".to_owned(), Value::Byte(1)),
            ]);
            if resolved {
                nbt.insert("TextIgnoreLegacyBugResolved".to_owned(), Value::Byte(1));
            }
            let sign = Sign::read(&mut nbt, false);
            assert_eq!(sign.front.lines[..2], ["a".to_owned(), "b".to_owned()]);
            assert_eq!((sign.front.colour, sign.front.glowing), (red, resolved));
            assert!(nbt.is_empty(), "{nbt:?}");
        }
    }

    #[test]
    fn a_mob_head_rotation_rounds_to_sixteenths_of_a_turn() {
        assert_eq!(skull_rotation(0.0), 0);
        assert_eq!(skull_rotation(90.0), 4);
        assert_eq!(skull_rotation(-22.5), 15);
        assert_eq!(skull_rotation(350.0), 0);
    }

    #[test]
    fn banner_patterns_keep_unknown_ids_and_stay_bounded() {
        let mut list = vec![Compound::from([
            ("Pattern".to_owned(), Value::String("nope".to_owned())),
            ("Color".to_owned(), Value::Int(3)),
        ])];
        for _ in 0..40 {
            list.push(Compound::from([
                ("Pattern".to_owned(), Value::String("bo".to_owned())),
                ("Color".to_owned(), Value::Int(3)),
            ]));
        }
        let read = read_patterns(&list);
        assert_eq!(read.len(), MAX_PATTERNS);
        assert_eq!(read[0], ("nope".to_owned(), 3));
        assert!(read[1..].iter().all(|(id, c)| id == "bo" && *c == 3));
        let mut saved = Compound::from([("Patterns".to_owned(), patterns_value(&read[..2]))]);
        let banner = Banner::read(&mut saved);
        let mut written = Compound::new();
        banner.write(&mut written);
        assert_eq!(written.get("Patterns"), Some(&patterns_value(&read[..2])));
    }
}
