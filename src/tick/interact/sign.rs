//! Sign text edits from clients (PMMP `InGamePacketHandler::handleBlockActorData` and
//! `updateSignText`, `SignText::fromBlob`, `BaseSign::updateFaceText`, `TextFormat::clean`).
//!
//! A client sends the sign's whole block entity data; the text of each side is one string of
//! lines joined by newlines. This module checks and cleans it; the block entity keeps it. The
//! editor opens when a player places or uses a sign, or through [`Context::open_sign_editor`].

use super::super::{Context, Phase};
use crate::block::Class;
use crate::nbt::{Compound, Value};
use crate::player;
use crate::tile::{MAX_SIGN_TEXT_BYTES, SIGN_LINES, Side};
use crate::world::BlockPos;

/// PMMP `Sign::TAG_FRONT_TEXT`, `TAG_BACK_TEXT`, `TAG_TEXT_BLOB`.
pub(in crate::tick) const FRONT: &str = "FrontText";
pub(in crate::tick) const BACK: &str = "BackText";
pub(in crate::tick) const TEXT: &str = "Text";

/// The lines of a side's text blob (PMMP `SignText::fromBlob`: split at newlines, at most
/// [`SIGN_LINES`] lines, the rest dropped).
pub(in crate::tick) fn blob_lines(blob: &str) -> impl Iterator<Item = &str> {
    blob.splitn(SIGN_LINES + 1, '\n').take(SIGN_LINES)
}

impl Context<'_> {
    /// Opens the editor of the sign at `pos` in `player`'s world for the player, on its `side`
    /// (PMMP `Player::openSignEditor`): the player may then write that sign once, as after
    /// placing it (a waxed sign still refuses the text), and its edit raises
    /// [`SignChange`](crate::event::SignChange). False, sending nothing, if the player is not in
    /// the game or the block there is not a sign (or its chunk is not loaded).
    #[doc(alias = "Player::openSignEditor")]
    pub fn open_sign_editor(&mut self, player: player::Id, pos: BlockPos, side: Side) -> bool {
        let session = player.session();
        let Some(world) = self
            .state
            .players
            .get(&session)
            .filter(|p| p.phase != Phase::Loading)
            .map(|p| p.world)
        else {
            return false;
        };
        let sign = self
            .state
            .worlds
            .get(world)
            .and_then(|w| w.block(pos))
            .is_some_and(|b| b.is(Class::BaseSign));
        if sign {
            super::open_sign_editor(self.state, session, world, pos, side == Side::Front);
        }
        sign
    }
}

/// The text a client wrote on both sides of a sign, cleaned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Edit {
    /// The front side's lines.
    pub(super) front: [String; SIGN_LINES],
    /// The back side's lines.
    pub(super) back: [String; SIGN_LINES],
}

/// Why an edit is refused (PMMP disconnects for these; Anchor drops the packet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refused {
    /// A side or its text has the wrong tag type or is missing.
    Malformed,
    /// A side holds more than 1000 bytes of text.
    TooLong,
}

/// Reads both sides of a sign edit.
pub(super) fn parse(nbt: &Compound) -> Result<Edit, Refused> {
    Ok(Edit {
        front: side(nbt, FRONT)?,
        back: side(nbt, BACK)?,
    })
}

/// One side's lines (PMMP `SignText::fromBlob`: split at newlines into at most four lines, the
/// rest dropped), each cleaned (PMMP `TextFormat::clean($line, false)`).
fn side(nbt: &Compound, tag: &str) -> Result<[String; SIGN_LINES], Refused> {
    let Some(Value::Compound(side)) = nbt.get(tag) else {
        return Err(Refused::Malformed);
    };
    let Some(Value::String(blob)) = side.get(TEXT) else {
        return Err(Refused::Malformed);
    };
    let mut lines: [String; SIGN_LINES] = Default::default();
    let mut total = 0usize;
    for (slot, line) in lines.iter_mut().zip(blob_lines(blob)) {
        total = total.saturating_add(line.len());
        *slot = clean(line);
    }
    if total > MAX_SIGN_TEXT_BYTES {
        return Err(Refused::TooLong);
    }
    Ok(lines)
}

/// Lines a listener set, checked like the player's (PMMP `SignText` refuses a line with a
/// newline; `BaseSign::updateFaceText` more than 1000 bytes) and cleaned; `None` if they fail.
pub(super) fn check(lines: &[String; SIGN_LINES]) -> Option<[String; SIGN_LINES]> {
    let mut total = 0usize;
    for line in lines {
        if line.contains('\n') {
            return None;
        }
        total = total.saturating_add(line.len());
    }
    (total <= MAX_SIGN_TEXT_BYTES).then(|| lines.each_ref().map(|l| clean(l)))
}

/// PMMP `TextFormat::clean($text, false)`: drops private-use characters and terminal escape
/// sequences, keeping colour codes.
pub(super) fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // The private-use characters go first, so a sequence they split still goes.
    let mut chars = text
        .chars()
        .filter(|c| !('\u{E000}'..='\u{F8FF}').contains(c))
        .peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // `\x1b[\(\][[0-9;\[\(]+[Bm]` is one class (`(`, `]`, `[`, digits, `;`): skip a whole
            // sequence; a lone escape goes too.
            let mut lookahead = chars.clone();
            let mut body = 0;
            while lookahead
                .peek()
                .is_some_and(|b| b.is_ascii_digit() || matches!(b, '(' | ']' | '[' | ';'))
            {
                lookahead.next();
                body += 1;
            }
            if body > 0 && lookahead.peek().is_some_and(|e| matches!(e, 'B' | 'm')) {
                lookahead.next();
                chars = lookahead;
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(front: &str, back: &str) -> Compound {
        let side = |text: &str| {
            let mut c = Compound::new();
            c.insert(TEXT.to_owned(), Value::String(text.to_owned()));
            Value::Compound(c)
        };
        let mut nbt = Compound::new();
        nbt.insert(FRONT.to_owned(), side(front));
        nbt.insert(BACK.to_owned(), side(back));
        nbt
    }

    #[test]
    fn text_splits_into_four_lines_and_drops_the_rest() {
        let got = parse(&edit("a\nb\nc\nd\ne\nf", "x")).unwrap();
        assert_eq!(got.front, ["a", "b", "c", "d"].map(String::from));
        assert_eq!(got.back, ["x", "", "", ""].map(String::from));
    }

    #[test]
    fn long_or_malformed_text_is_refused() {
        assert_eq!(parse(&edit(&"x".repeat(1001), "")), Err(Refused::TooLong));
        assert!(parse(&edit(&"x".repeat(1000), "")).is_ok());
        let mut nbt = edit("a", "b");
        nbt.insert(FRONT.to_owned(), Value::Int(1));
        assert_eq!(parse(&nbt), Err(Refused::Malformed));
        assert_eq!(parse(&Compound::new()), Err(Refused::Malformed));
    }

    #[test]
    fn cleaning_keeps_colours_and_drops_escapes_and_private_use() {
        assert_eq!(clean("§cRed"), "§cRed");
        assert_eq!(clean("a\u{E123}b"), "ab");
        assert_eq!(clean("a\u{1b}[31mb"), "ab");
        assert_eq!(clean("a\u{1b}b"), "ab");
    }

    /// PMMP's escape pattern `\x1b[\(\][[0-9;\[\(]+[Bm]` is one character class, and the
    /// private-use characters go before it is matched.
    #[test]
    fn cleaning_reads_the_escape_pattern_as_pmmp() {
        assert_eq!(clean("a\u{1b}[mb"), "ab");
        assert_eq!(clean("a\u{1b}1mb"), "ab");
        assert_eq!(clean("a\u{1b}]]mb"), "ab");
        assert_eq!(clean("a\u{1b}\u{E000}[31mb"), "ab");
    }
}
