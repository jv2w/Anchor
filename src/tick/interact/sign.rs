//! Sign text edits from clients (PMMP `InGamePacketHandler::handleBlockActorData` and
//! `updateSignText`, `SignText::fromBlob`, `BaseSign::updateFaceText`, `TextFormat::clean`).
//!
//! A client sends the sign's whole block entity data; the text of each side is one string of
//! lines joined by newlines. This module checks and cleans it; the block entity keeps it.

use crate::nbt::{Compound, Value};

/// The lines of a sign side (PMMP `SignText::LINE_COUNT`).
pub(super) const LINES: usize = 4;
/// The most text bytes one side may hold (PMMP `BaseSign::updateFaceText`).
const MAX_BYTES: usize = 1000;
/// PMMP `Sign::TAG_FRONT_TEXT`, `TAG_BACK_TEXT`, `TAG_TEXT_BLOB`.
const FRONT: &str = "FrontText";
const BACK: &str = "BackText";
const TEXT: &str = "Text";

/// The text a client wrote on both sides of a sign, cleaned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Edit {
    /// The front side's lines.
    pub(super) front: [String; LINES],
    /// The back side's lines.
    pub(super) back: [String; LINES],
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
fn side(nbt: &Compound, tag: &str) -> Result<[String; LINES], Refused> {
    let Some(Value::Compound(side)) = nbt.get(tag) else {
        return Err(Refused::Malformed);
    };
    let Some(Value::String(blob)) = side.get(TEXT) else {
        return Err(Refused::Malformed);
    };
    let mut lines: [String; LINES] = Default::default();
    let mut total = 0usize;
    for (slot, line) in lines.iter_mut().zip(blob.splitn(LINES + 1, '\n')) {
        total = total.saturating_add(line.len());
        *slot = clean(line);
    }
    if total > MAX_BYTES {
        return Err(Refused::TooLong);
    }
    Ok(lines)
}

/// PMMP `TextFormat::clean($text, false)`: drops private-use characters and terminal escape
/// sequences, keeping colour codes.
pub(super) fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if ('\u{E000}'..='\u{F8FF}').contains(&c) {
            continue;
        }
        if c == '\u{1b}' {
            // `\x1b[(\][][0-9;[(]+[Bm]`: skip a whole sequence; a lone escape goes too.
            let mut lookahead = chars.clone();
            if lookahead
                .next()
                .is_some_and(|o| matches!(o, '(' | ']' | '['))
            {
                let mut body = 0;
                while lookahead
                    .peek()
                    .is_some_and(|b| b.is_ascii_digit() || matches!(b, ';' | '[' | '('))
                {
                    lookahead.next();
                    body += 1;
                }
                if body > 0 && lookahead.peek().is_some_and(|e| matches!(e, 'B' | 'm')) {
                    lookahead.next();
                    chars = lookahead;
                }
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
}
