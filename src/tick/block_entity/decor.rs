//! The data of the block entities without slots (PMMP `tile`): signs, beds, banners, flower pots,
//! item frames, jukeboxes, lecterns, mob heads, cauldrons and beacons, read from and written to
//! their Bedrock NBT keys (PMMP `readSaveData`, `writeSaveData`, `addAdditionalSpawnData`).
//!
//! Reading takes the keys a kind owns out of the saved compound; the caller keeps the rest as it
//! came. Values out of range are fixed the way PMMP fixes them on load.

use tracing::{info, warn};

use crate::block;
use crate::custom::Registry;
use crate::item::Stack;
use crate::nbt::{Compound, List, Value};
use crate::tick::interact::{book, sign};

/// The text colour of a new sign: opaque black (PMMP `new Color(0, 0, 0)`).
pub(in crate::tick) use crate::tile::BLACK_TEXT;
/// One side of a sign (PMMP `SignText`): the public value type, stored as it is.
pub(in crate::tick) use crate::tile::SignText as SignSide;
/// The most patterns a banner keeps; more in saved data are left out (Bedrock's loom stops at
/// six; PMMP has no limit).
pub(in crate::tick) const MAX_PATTERNS: usize = 16;
/// Rotations of a framed item (PMMP `ItemFrame::ROTATIONS`).
pub(in crate::tick) const FRAME_ROTATIONS: u8 = 8;

/// A sign or hanging sign.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::tick) struct Sign {
    pub(in crate::tick) front: SignSide,
    pub(in crate::tick) back: SignSide,
    pub(in crate::tick) waxed: bool,
    /// A hanging sign (block entity `HangingSign`).
    pub(in crate::tick) hanging: bool,
}

/// A banner: base colour and patterns by PMMP's inverted dye colour IDs (black 0 … white 15).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::tick) struct Banner {
    pub(in crate::tick) base: i32,
    /// Pattern ID (`bo`, `cre`…) and colour, at most [`MAX_PATTERNS`].
    pub(in crate::tick) patterns: Vec<(String, i32)>,
    /// 0 normal, 1 ominous.
    pub(in crate::tick) ty: i32,
}

/// An item frame or glow item frame.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::tick) struct Frame {
    pub(in crate::tick) item: Stack,
    /// 0 to [`FRAME_ROTATIONS`] - 1, eighths of a turn.
    pub(in crate::tick) rotation: u8,
    /// 0 to 1.
    pub(in crate::tick) drop_chance: f32,
    pub(in crate::tick) glow: bool,
}

/// A cauldron's potion and colour (PMMP `tile\Cauldron`), as saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tick) struct Cauldron {
    /// The potion container type: -1 none, 0 potion, 1 splash, 2 lingering.
    pub(in crate::tick) container: i16,
    /// The potion type ID, -1 for none.
    pub(in crate::tick) potion: i16,
    /// The dyed water colour, signed ARGB.
    pub(in crate::tick) colour: Option<i32>,
}

impl Default for Cauldron {
    fn default() -> Self {
        Self {
            container: -1,
            potion: -1,
            colour: None,
        }
    }
}

/// Takes a key's value out of `nbt`.
fn take(nbt: &mut Compound, key: &str) -> Option<Value> {
    nbt.remove(key)
}

fn take_byte(nbt: &mut Compound, key: &str) -> Option<u8> {
    match take(nbt, key)? {
        Value::Byte(v) => Some(v),
        _ => None,
    }
}

fn take_short(nbt: &mut Compound, key: &str) -> Option<i16> {
    match take(nbt, key)? {
        Value::Short(v) => Some(v),
        _ => None,
    }
}

fn take_int(nbt: &mut Compound, key: &str) -> Option<i32> {
    match take(nbt, key)? {
        Value::Int(v) => Some(v),
        _ => None,
    }
}

fn take_float(nbt: &mut Compound, key: &str) -> Option<f32> {
    match take(nbt, key)? {
        Value::Float(v) if v.is_finite() => Some(v),
        _ => None,
    }
}

fn take_compound(nbt: &mut Compound, key: &str) -> Option<Compound> {
    match take(nbt, key)? {
        Value::Compound(v) => Some(v),
        _ => None,
    }
}

/// A saved item compound as a stack (custom items through `custom`, the server's custom
/// content); an unknown item is left out (PMMP `Item::safeNbtDeserialize`), logged at info: an
/// imported world holds vanilla items PMMP lacks.
pub(in crate::tick) fn read_stack(nbt: &Compound, what: &str, custom: &Registry) -> Stack {
    Stack::from_nbt_with(nbt, custom).unwrap_or_else(|| {
        info!(what, "a saved block entity item is unknown; it is left out");
        Stack::EMPTY
    })
}

impl SignSide {
    /// PMMP `Sign::readTextTag` (the lighting bug resolved in a side compound).
    fn read(mut nbt: Compound) -> Self {
        let mut side = Self::default();
        if let Some(Value::String(blob)) = nbt.remove(sign::TEXT) {
            for (slot, line) in side.lines.iter_mut().zip(sign::blob_lines(&blob)) {
                line.clone_into(slot);
            }
        }
        if let Some(colour) = take_int(&mut nbt, "SignTextColor") {
            side.colour = colour;
        }
        side.glowing = take_byte(&mut nbt, "IgnoreLighting").is_some_and(|g| g != 0);
        side
    }

    /// PMMP `Sign::writeTextTag`.
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

impl Sign {
    /// PMMP `Sign::readSaveData` (front and back compounds, or the 1.2 `Text` blob).
    pub(in crate::tick) fn read(nbt: &mut Compound, hanging: bool) -> Self {
        let front = match take_compound(nbt, sign::FRONT) {
            Some(side) => SignSide::read(side),
            None => match nbt.remove(sign::TEXT) {
                Some(Value::String(blob)) => {
                    // The 1.2 format keeps the side's keys at the root, and its glow counts only
                    // once the lighting bug was resolved (PMMP `readTextTag`).
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
                    SignSide::read(side)
                }
                _ => SignSide::default(),
            },
        };
        let back = take_compound(nbt, sign::BACK).map_or_else(SignSide::default, SignSide::read);
        let waxed = take_byte(nbt, "IsWaxed").is_some_and(|w| w != 0);
        Self {
            front,
            back,
            waxed,
            hanging,
        }
    }

    /// The saved keys (PMMP `writeSaveData`).
    pub(in crate::tick) fn write(&self, nbt: &mut Compound) {
        nbt.insert(sign::FRONT.to_owned(), self.front.write());
        nbt.insert(sign::BACK.to_owned(), self.back.write());
        nbt.insert("IsWaxed".to_owned(), Value::Byte(u8::from(self.waxed)));
    }

    /// The client keys: the saved ones and nobody editing (PMMP `addAdditionalSpawnData`).
    pub(in crate::tick) fn spawn(&self, nbt: &mut Compound) {
        self.write(nbt);
        nbt.insert("LockedForEditingBy".to_owned(), Value::Long(-1));
    }

    /// The side the player faces.
    pub(in crate::tick) fn side_mut(&mut self, front: bool) -> &mut SignSide {
        if front {
            &mut self.front
        } else {
            &mut self.back
        }
    }
}

impl Banner {
    /// PMMP `Banner::readSaveData`.
    pub(in crate::tick) fn read(nbt: &mut Compound) -> Self {
        let base = take_int(nbt, "Base")
            .filter(|b| (0..16).contains(b))
            .unwrap_or(0);
        let patterns = match take(nbt, "Patterns") {
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

/// A banner's `Patterns` list: entries with a pattern ID, at most [`MAX_PATTERNS`]. An ID Anchor
/// does not know stays, as clients may know it (PMMP skips it); a missing colour is black, 0.
pub(in crate::tick) fn read_patterns(list: &[Compound]) -> Vec<(String, i32)> {
    let mut out = Vec::with_capacity(list.len().min(MAX_PATTERNS));
    for entry in list {
        let Some(Value::String(id)) = entry.get("Pattern") else {
            continue;
        };
        let colour = match entry.get("Color") {
            Some(Value::Int(c)) if (0..16).contains(c) => *c,
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

/// A flower pot's plant.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::tick) enum Plant {
    /// A block state Anchor knows.
    Known(block::State),
    /// A saved plant Anchor does not know, kept as an unknown block in a chunk is: no rule takes it
    /// out or drops it.
    Unknown {
        /// The `PlantBlock` compound as it came.
        nbt: Compound,
        /// Whether clients know it: an inert block, sent as itself.
        shown: bool,
    },
}

/// A flower pot's plant (`PlantBlock`, a block state compound); `None` for none or air. A state
/// Anchor does not know stays as it came (PMMP fails the tile).
pub(in crate::tick) fn read_plant(nbt: &mut Compound) -> Option<Plant> {
    let plant = take_compound(nbt, "PlantBlock")?;
    if let Some(state) = crate::save::find_state(&plant) {
        return (state != block::AIR).then_some(Plant::Known(state));
    }
    let shown = block::Inert::from_nbt(&plant).is_some();
    Some(Plant::Unknown { nbt: plant, shown })
}

/// The `PlantBlock` key; clients get a plant Anchor does not know only if they know it.
pub(in crate::tick) fn write_plant(plant: Option<&Plant>, nbt: &mut Compound, client: bool) {
    let value = match plant {
        Some(Plant::Known(state)) => crate::save::state_nbt(*state),
        Some(Plant::Unknown { nbt: saved, shown }) if *shown || !client => saved.clone(),
        _ => return,
    };
    nbt.insert("PlantBlock".to_owned(), Value::Compound(value));
}

impl Frame {
    /// PMMP `ItemFrame::readSaveData`: the rotation as a float of degrees (or a legacy byte).
    pub(in crate::tick) fn read(nbt: &mut Compound, glow: bool, custom: &Registry) -> Self {
        let item =
            take_compound(nbt, "Item").map_or(Stack::EMPTY, |i| read_stack(&i, "frame", custom));
        let rotation = match take(nbt, "ItemRotation") {
            Some(Value::Float(r)) if r.is_finite() => float_rotation(r),
            Some(Value::Byte(r)) => r % FRAME_ROTATIONS,
            _ => 0,
        };
        let drop_chance = take_float(nbt, "ItemDropChance")
            .filter(|c| (0.0..=1.0).contains(c))
            .unwrap_or(1.0);
        Self {
            item,
            rotation,
            drop_chance,
            glow,
        }
    }

    /// The saved keys; the client gets the item in its network form (`Stack::to_nbt` is that
    /// form's compound).
    pub(in crate::tick) fn write(&self, nbt: &mut Compound) {
        nbt.insert("ItemDropChance".to_owned(), Value::Float(self.drop_chance));
        nbt.insert(
            "ItemRotation".to_owned(),
            Value::Float(f32::from(self.rotation) * 45.0),
        );
        if !self.item.is_empty() {
            nbt.insert("Item".to_owned(), Value::Compound(self.item.to_nbt()));
        }
    }
}

/// An item frame rotation in degrees as eighths of a turn (PMMP `(int) (degrees / 45)`).
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "safety: the value is finite and reduced into 0..8 first"
)]
fn float_rotation(degrees: f32) -> u8 {
    let eighths = (degrees / 45.0)
        .trunc()
        .rem_euclid(f32::from(FRAME_ROTATIONS));
    (eighths as u8) % FRAME_ROTATIONS
}

/// An item key read as a stack (empty when missing).
pub(in crate::tick) fn read_item(nbt: &mut Compound, key: &str, custom: &Registry) -> Stack {
    take_compound(nbt, key).map_or(Stack::EMPTY, |i| read_stack(&i, key, custom))
}

/// Writes an item key when the stack is not empty.
pub(in crate::tick) fn write_item(stack: &Stack, key: &str, nbt: &mut Compound) {
    if !stack.is_empty() {
        nbt.insert(key.to_owned(), Value::Compound(stack.to_nbt()));
    }
}

/// A lectern: PMMP `Lectern::readSaveData`.
pub(in crate::tick) fn read_lectern(nbt: &mut Compound, custom: &Registry) -> (Stack, i32) {
    let page = take_int(nbt, "page").unwrap_or(0).max(0);
    let book = read_item(nbt, "book", custom).with_count(1);
    nbt.remove("hasBook");
    nbt.remove("totalPages");
    (book, page)
}

/// A lectern's keys (PMMP `writeSaveData`, the same as its client keys).
pub(in crate::tick) fn write_lectern(book: &Stack, page: i32, nbt: &mut Compound) {
    nbt.insert(
        "hasBook".to_owned(),
        Value::Byte(u8::from(!book.is_empty())),
    );
    nbt.insert("page".to_owned(), Value::Int(page));
    if !book.is_empty() {
        nbt.insert("book".to_owned(), Value::Compound(book.to_nbt()));
        let pages = i32::try_from(book::page_count(book)).unwrap_or(i32::MAX);
        nbt.insert("totalPages".to_owned(), Value::Int(pages));
    }
}

/// A mob head: PMMP `MobHead::readSaveData` (the rotation as degrees, or the legacy `Rot`).
pub(in crate::tick) fn read_skull(nbt: &mut Compound) -> (u8, u8) {
    let skull = take_byte(nbt, "SkullType").filter(|t| *t <= 6).unwrap_or(0);
    let rotation = match take_float(nbt, "Rotation") {
        Some(yaw) => skull_rotation(yaw),
        None => take_byte(nbt, "Rot").filter(|r| *r <= 15).unwrap_or(0),
    };
    nbt.remove("Rot");
    (skull, rotation)
}

/// PMMP `((int) floor(($yaw * 16 / 360) + 0.5)) & 0xf`.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "safety: the value is finite and reduced into 0..16 first"
)]
pub(in crate::tick) fn skull_rotation(yaw: f32) -> u8 {
    let sixteenths = (f64::from(yaw) * 16.0 / 360.0 + 0.5)
        .floor()
        .rem_euclid(16.0);
    (sixteenths as u8) & 0xf
}

/// A mob head's keys.
pub(in crate::tick) fn write_skull(skull: u8, rotation: u8, nbt: &mut Compound) {
    nbt.insert("SkullType".to_owned(), Value::Byte(skull));
    nbt.insert(
        "Rotation".to_owned(),
        Value::Float(f32::from(rotation) * 360.0 / 16.0),
    );
}

impl Cauldron {
    /// PMMP `Cauldron::readSaveData`; a potion without a container or type is none.
    pub(in crate::tick) fn read(nbt: &mut Compound) -> Self {
        let container = take_short(nbt, "PotionType").unwrap_or(-1);
        let potion = take_short(nbt, "PotionId").unwrap_or(-1);
        let (container, potion) = if (0..=2).contains(&container) && potion >= 0 {
            (container, potion)
        } else {
            (-1, -1)
        };
        let colour = take_int(nbt, "CustomColor");
        Self {
            container,
            potion,
            colour,
        }
    }

    /// The saved and client keys.
    pub(in crate::tick) fn write(&self, nbt: &mut Compound) {
        nbt.insert("PotionType".to_owned(), Value::Short(self.container));
        nbt.insert("PotionId".to_owned(), Value::Short(self.potion));
        if let Some(colour) = self.colour {
            nbt.insert("CustomColor".to_owned(), Value::Int(colour));
        }
    }
}

/// A beacon's effects (PMMP `Beacon` tile keys `primary`, `secondary`).
pub(in crate::tick) fn read_beacon(nbt: &mut Compound) -> (i32, i32) {
    (
        take_int(nbt, "primary").unwrap_or(0),
        take_int(nbt, "secondary").unwrap_or(0),
    )
}

/// A beacon's keys.
pub(in crate::tick) fn write_beacon(primary: i32, secondary: i32, nbt: &mut Compound) {
    nbt.insert("primary".to_owned(), Value::Int(primary));
    nbt.insert("secondary".to_owned(), Value::Int(secondary));
}

/// A bed's colour (PMMP `Bed::readSaveData`: a missing or unknown colour is red, 14).
pub(in crate::tick) fn read_bed(nbt: &mut Compound) -> u8 {
    take_byte(nbt, "color").filter(|c| *c < 16).unwrap_or(14)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_text_round_trips_and_keeps_its_colour_and_glow() {
        let sign = Sign {
            front: SignSide {
                lines: ["a".into(), "b".into(), String::new(), String::new()],
                colour: 0xff00_ff00_u32.cast_signed(),
                glowing: true,
            },
            back: SignSide::default(),
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
            Some(Value::String("a\nb".into()))
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
    fn rotations_follow_pmmp() {
        assert_eq!(float_rotation(90.0), 2);
        assert_eq!(float_rotation(359.0), 7);
        assert_eq!(float_rotation(-45.0), 7);
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
