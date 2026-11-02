//! Block entities: the data of blocks beyond their state (PMMP `pocketmine\block\tile`), read
//! through [`Tile`] and changed through [`Mut`].
//!
//! [`Context::block_entity`](crate::Context) lends the block entity at a position to read and
//! `block_entity_mut` to change: a sign's text, a banner's patterns, an item frame's item, a
//! flower pot's plant, a jukebox's record, a lectern's book, a campfire's cooking times, a
//! beacon's effects, a bed's colour, a mob head's rotation, a cauldron's potion, a container's
//! name and a chest's pair. The slots of containers are the inventory API's
//! ([`Context::block_inventory`](crate::Context)).
//!
//! A read of a kind the block entity is not gives `None`. A write changes the block entity at
//! once, tells the players who see its chunk (its client data, as when the rules change it) and
//! is saved with the chunk at the next commit; a value it does not take or a kind it does not
//! apply to is an [`Error`], changing nothing. Writes raise no events.

use std::fmt;

use crate::protocol::Rgba;
use crate::save::WorldIndex;
use crate::tick::State;
use crate::world::BlockPos;

/// The text colour of a new sign: opaque black (PMMP `new Color(0, 0, 0)`), as signed ARGB.
pub(crate) const BLACK_TEXT: i32 = 0xff00_0000_u32.cast_signed();
/// The longest custom name of a container, in bytes.
pub const MAX_NAME_BYTES: usize = 1024;
/// The most text bytes a sign side holds, all lines together (PMMP `BaseSign::updateFaceText`,
/// the limit on players' edits; PMMP has none for plugins, Anchor keeps one bound for both).
pub const MAX_SIGN_TEXT_BYTES: usize = 1000;
/// The lines of a sign side (PMMP `SignText::LINE_COUNT`).
pub const SIGN_LINES: usize = 4;

/// A block entity to read ([`Context::block_entity`](crate::Context)): its position, its block
/// and the data of its kind.
#[derive(Clone, Copy)]
// api-class: Tile
pub struct Tile<'a> {
    pub(crate) state: &'a State,
    pub(crate) world: WorldIndex,
    pub(crate) pos: BlockPos,
}

impl fmt::Debug for Tile<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tile")
            .field("world", &self.world)
            .field("pos", &self.pos)
            .finish_non_exhaustive()
    }
}

/// A block entity to change ([`Context::block_entity_mut`](crate::Context)).
pub struct Mut<'a> {
    pub(crate) state: &'a mut State,
    pub(crate) world: WorldIndex,
    pub(crate) pos: BlockPos,
}

impl fmt::Debug for Mut<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mut")
            .field("world", &self.world)
            .field("pos", &self.pos)
            .finish_non_exhaustive()
    }
}

/// Why a block entity write did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The block entity is not of the kind the write applies to (a sign's text on a chest).
    WrongKind,
    /// The value is out of range or not one the kind takes (a rotation of 9, a stone in a flower
    /// pot, chests that are not side by side).
    Invalid,
    /// The text or the list is longer than its limit ([`MAX_SIGN_TEXT_BYTES`],
    /// [`MAX_NAME_BYTES`], a banner's 16 patterns).
    TooLong,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongKind => {
                f.write_str("the block entity is not of the kind the write applies to")
            }
            Self::Invalid => f.write_str("the value is not one the block entity takes"),
            Self::TooLong => f.write_str("the value is too long"),
        }
    }
}

impl std::error::Error for Error {}

/// A result with the module's [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A side of a sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// The front: the side a standing sign faces, the side text goes on first.
    Front,
    /// The back.
    Back,
}

/// The text of one side of a sign: four lines, a colour and whether it glows.
#[derive(Debug, Clone, PartialEq, Eq)]
// api-class: SignText
pub struct SignText {
    pub(crate) lines: [String; SIGN_LINES],
    /// Signed ARGB, as saved.
    pub(crate) colour: i32,
    pub(crate) glowing: bool,
}

impl SignText {
    /// Text of four lines in `colour`; [`Error::Invalid`] if a line holds a newline,
    /// [`Error::TooLong`] beyond [`MAX_SIGN_TEXT_BYTES`] in all.
    pub fn new(lines: [String; SIGN_LINES], colour: Rgba, glowing: bool) -> Result<Self> {
        check_lines(lines.iter().map(String::as_str))?;
        Ok(Self {
            lines,
            colour: to_argb(colour),
            glowing,
        })
    }

    /// Text from lines joined by newlines (lines past the fourth are left out, missing ones are
    /// empty); [`Error::TooLong`] beyond [`MAX_SIGN_TEXT_BYTES`].
    #[doc(alias = "SignText::fromBlob")]
    pub fn from_blob(blob: &str, colour: Rgba, glowing: bool) -> Result<Self> {
        let mut lines: [String; SIGN_LINES] = Default::default();
        for (slot, line) in lines.iter_mut().zip(blob_lines(blob)) {
            line.clone_into(slot);
        }
        Self::new(lines, colour, glowing)
    }

    /// The text colour.
    #[doc(alias = "SignText::getBaseColor")]
    pub fn colour(&self) -> Rgba {
        from_argb(self.colour)
    }

    /// Whether the text glows (a glow ink sac was used on it).
    #[doc(alias = "SignText::isGlowing")]
    pub fn is_glowing(&self) -> bool {
        self.glowing
    }

    /// Line `index` (0 to 3), or `None`.
    #[doc(alias = "SignText::getLine")]
    pub fn line(&self, index: usize) -> Option<&str> {
        self.lines.get(index).map(String::as_str)
    }

    /// The four lines.
    #[doc(alias = "SignText::getLines")]
    pub fn lines(&self) -> &[String; SIGN_LINES] {
        &self.lines
    }
}

impl Default for SignText {
    /// Empty lines in black, not glowing (a new sign's text).
    fn default() -> Self {
        Self {
            lines: Default::default(),
            colour: BLACK_TEXT,
            glowing: false,
        }
    }
}

/// A dye colour, in PMMP `DyeColorIdMap` order (the Bedrock ID of a bed's or carpet's colour).
#[doc(alias = "DyeColor")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DyeColour {
    /// White, 0.
    White,
    /// Orange, 1.
    Orange,
    /// Magenta, 2.
    Magenta,
    /// Light blue, 3.
    LightBlue,
    /// Yellow, 4.
    Yellow,
    /// Lime, 5.
    Lime,
    /// Pink, 6.
    Pink,
    /// Gray, 7.
    Gray,
    /// Light gray, 8.
    LightGray,
    /// Cyan, 9.
    Cyan,
    /// Purple, 10.
    Purple,
    /// Blue, 11.
    Blue,
    /// Brown, 12.
    Brown,
    /// Green, 13.
    Green,
    /// Red, 14.
    Red,
    /// Black, 15.
    Black,
}

impl DyeColour {
    /// Every colour, by ID.
    pub const ALL: [Self; 16] = [
        Self::White,
        Self::Orange,
        Self::Magenta,
        Self::LightBlue,
        Self::Yellow,
        Self::Lime,
        Self::Pink,
        Self::Gray,
        Self::LightGray,
        Self::Cyan,
        Self::Purple,
        Self::Blue,
        Self::Brown,
        Self::Green,
        Self::Red,
        Self::Black,
    ];

    /// The colour of a banner's ID (Bedrock numbers them the other way: black 0 … white 15).
    pub(crate) fn from_banner_id(id: i32) -> Option<Self> {
        u8::try_from(15_i32.checked_sub(id)?)
            .ok()
            .and_then(Self::from_id)
    }

    /// The colour of a `DyeColorIdMap` ID, or `None`.
    pub(crate) fn from_id(id: u8) -> Option<Self> {
        Self::ALL.get(usize::from(id)).copied()
    }

    /// The banner ID of the colour.
    pub(crate) fn banner_id(self) -> i32 {
        15 - i32::from(self as u8)
    }

    /// The colour's RGB value, opaque (PMMP's: white is `#f0f0f0`, black `#1d1d21`).
    #[doc(alias = "DyeColor::getRgbValue")]
    #[expect(
        clippy::missing_panics_doc,
        reason = "invariant: the dye table holds all 16 colours"
    )]
    pub fn rgb(self) -> Rgba {
        #[expect(
            clippy::expect_used,
            reason = "invariant: the dye table holds all 16 colours"
        )]
        let argb = crate::item::dye_argb(self as u8).expect("every dye colour has a value");
        from_argb(argb.cast_signed())
    }
}

/// Whether a banner is a normal or an ominous (raid captain's) banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BannerType {
    /// A normal banner (PMMP `Banner::TYPE_NORMAL`).
    Normal,
    /// An ominous banner (PMMP `Banner::TYPE_OMINOUS`).
    Ominous,
}

/// One pattern layer of a banner: its pattern and colour.
#[doc(alias = "BannerPatternLayer")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BannerPattern {
    pattern: crate::block::BannerPattern,
    colour: DyeColour,
}

impl BannerPattern {
    /// A layer of `pattern` in `colour`.
    pub fn new(pattern: crate::block::BannerPattern, colour: DyeColour) -> Self {
        Self { pattern, colour }
    }

    /// The colour.
    #[doc(alias = "BannerPatternLayer::getColor")]
    pub fn colour(self) -> DyeColour {
        self.colour
    }

    /// The pattern.
    #[doc(alias = "BannerPatternLayer::getType")]
    pub fn pattern(self) -> crate::block::BannerPattern {
        self.pattern
    }
}

/// The lines of a sign side's text blob (PMMP `SignText::fromBlob`: split at newlines, at most
/// [`SIGN_LINES`] lines, the rest dropped).
pub(crate) fn blob_lines(blob: &str) -> impl Iterator<Item = &str> {
    blob.splitn(SIGN_LINES + 1, '\n').take(SIGN_LINES)
}

/// Checks the lines of a sign side: [`Error::Invalid`] if a line holds a newline (PMMP
/// `SignText`), [`Error::TooLong`] beyond [`MAX_SIGN_TEXT_BYTES`] in all (PMMP
/// `BaseSign::updateFaceText`).
pub(crate) fn check_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let mut bytes = 0_usize;
    for line in lines {
        if line.contains('\n') {
            return Err(Error::Invalid);
        }
        bytes = bytes.saturating_add(line.len());
    }
    if bytes > MAX_SIGN_TEXT_BYTES {
        return Err(Error::TooLong);
    }
    Ok(())
}

/// The colour of a signed ARGB int, as block entities save colours (PMMP `Color::fromARGB`).
pub(crate) fn from_argb(argb: i32) -> Rgba {
    let [a, r, g, b] = argb.to_be_bytes();
    Rgba { r, g, b, a }
}

/// The signed ARGB int of a colour, as block entities save colours (PMMP `Color::toARGB`).
pub(crate) fn to_argb(c: Rgba) -> i32 {
    i32::from_be_bytes([c.a, c.r, c.g, c.b])
}

#[cfg(test)]
mod tests {
    use super::*;

    // api: SignText, SignText::fromBlob, SignText::getLines, SignText::getLine,
    // api: SignText::getBaseColor, SignText::isGlowing
    #[test]
    fn sign_text_checks_its_lines_and_keeps_its_colour() {
        let colour = Rgba {
            r: 0x11,
            g: 0x22,
            b: 0x33,
            a: 0xff,
        };
        let text = SignText::from_blob(
            "a
b
c
d
e",
            colour,
            true,
        )
        .unwrap();
        assert_eq!(text.lines(), &["a", "b", "c", "d"].map(String::from));
        assert_eq!((text.line(1), text.line(4)), (Some("b"), None));
        assert_eq!((text.colour(), text.is_glowing()), (colour, true));
        assert_eq!(text.colour, 0xff11_2233_u32.cast_signed(), "saved as ARGB");
        let short = SignText::from_blob("x", Rgba::default(), false).unwrap();
        assert_eq!(short.lines(), &["x", "", "", ""].map(String::from));
        assert_eq!(
            SignText::new(
                [
                    "a\nb".to_owned(),
                    String::new(),
                    String::new(),
                    String::new()
                ],
                Rgba::default(),
                false
            ),
            Err(Error::Invalid)
        );
        assert_eq!(
            SignText::from_blob(&"x".repeat(MAX_SIGN_TEXT_BYTES + 1), Rgba::default(), false),
            Err(Error::TooLong)
        );
        let black = Rgba {
            a: 0xff,
            ..Rgba::default()
        };
        assert_eq!(SignText::default().colour(), black);
    }

    // api: BannerPatternLayer, BannerPatternLayer::getType, BannerPatternLayer::getColor
    #[test]
    fn banner_colours_count_the_other_way() {
        let layer = BannerPattern::new(crate::block::BannerPattern::Border, DyeColour::Red);
        assert_eq!(
            (layer.pattern(), layer.colour()),
            (crate::block::BannerPattern::Border, DyeColour::Red)
        );
        for c in DyeColour::ALL {
            assert_eq!(DyeColour::from_banner_id(c.banner_id()), Some(c));
            assert_eq!(DyeColour::from_id(c as u8), Some(c));
        }
        assert_eq!(DyeColour::Black.banner_id(), 0);
        assert_eq!(DyeColour::from_banner_id(16), None);
        assert_eq!(DyeColour::from_banner_id(-1), None);
    }

    // api: DyeColor, DyeColor::getRgbValue
    #[test]
    fn dye_colours_have_pmmps_rgb_values() {
        // PMMP `DyeColor::getMetadata`, white to black.
        let pmmp: [u32; 16] = [
            0xf0_f0f0, 0xf9_801d, 0xc7_4ebd, 0x3a_b3da, 0xfe_d83d, 0x80_c71f, 0xf3_8baa, 0x47_4f52,
            0x9d_9d97, 0x16_9c9c, 0x89_32b8, 0x3c_44aa, 0x83_5432, 0x5e_7c16, 0xb0_2e26, 0x1d_1d21,
        ];
        for (c, rgb) in DyeColour::ALL.into_iter().zip(pmmp) {
            let [_, r, g, b] = rgb.to_be_bytes();
            assert_eq!(c.rgb(), Rgba { r, g, b, a: 0xff }, "{c:?}");
        }
    }
}
