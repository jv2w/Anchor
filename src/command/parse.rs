//! Command lines: splitting a line into words and fitting the words to an overload's parameters.
//!
//! Fitting is syntax only: names of players and relative coordinates are resolved against the
//! game afterwards, for the overload that fits (`tick::command`).

use std::borrow::Cow;

use serde::de::IgnoredAny;

use super::{Error, Kind, Param, Result, Value};
use crate::block;
use crate::effect;
use crate::item;

/// The longest line, in bytes: the chat limit, which commands share.
pub(crate) const MAX_LINE_BYTES: usize = 2048;
/// The longest line, in characters.
pub(crate) const MAX_LINE_CHARS: usize = 512;
/// The namespace item and block names may carry.
pub(crate) const NAMESPACE: &str = "minecraft:";
/// The characters of the official server's syntax error on each side of the word.
pub(crate) const SYNTAX_CONTEXT: usize = 10;

/// A word of a command line ([`split`]) and where it starts (its opening quote, if quoted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word<'a> {
    /// Owned only for a quoted word, which has its quotes and escapes taken out.
    pub(crate) text: Cow<'a, str>,
    pub(crate) start: usize,
    /// The byte offset just after the word (after its closing quote, if quoted).
    pub(crate) end: usize,
}

impl Word<'_> {
    /// Whether it was in double quotes: such a word is never a selector (`@a`).
    pub fn is_quoted(&self) -> bool {
        matches!(self.text, Cow::Owned(_))
    }

    /// The word: a quoted one without its quotes, with `\"` and `\\` read as `"` and `\`.
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// A target parameter as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// A player's name.
    Name(String),
    /// `@s`.
    Sender,
    /// `@a`.
    All,
    /// `@p`.
    Nearest,
    /// `@r`.
    Random,
}

/// One coordinate of a position parameter as typed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Coord {
    /// `~`: added to the sender's coordinate.
    pub(crate) relative: bool,
    /// The number, finite; 0 for a lone `~`.
    pub(crate) value: f64,
}

/// Why the words stop fitting an overload ([`fit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Misfit {
    /// The byte offset in the line where they stop fitting.
    At(usize),
    /// A quote that a word of the overload reads is not closed.
    UnclosedQuote,
}

/// An argument as fitted, before players and relative coordinates are resolved.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Raw {
    Value(Value),
    Target(Target),
    Position([Coord; 3]),
}

/// The names item, block, effect and enchantment parameters take, built once when the commands
/// are made.
#[derive(Debug)]
pub(crate) struct Names {
    /// Effect names (`speed`) in name order.
    effects: Vec<(String, effect::Type)>,
    /// Enchantment names (`sharpness`) in name order.
    enchantments: Vec<(String, item::Enchantment)>,
    /// What the client offers for an item parameter: every item and block name but the Education
    /// items', in name order.
    items: Vec<&'static str>,
}

impl Names {
    /// The name tables of every effect, enchantment, item and block.
    pub(crate) fn new() -> Self {
        let mut effects: Vec<_> = effect::Type::all()
            .iter()
            .map(|&t| (snake_name(&format!("{t:?}")), t))
            .collect();
        effects.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut enchantments: Vec<_> = item::Enchantment::all()
            .iter()
            .map(|&e| (snake_name(&format!("{e:?}")), e))
            .collect();
        enchantments.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut items: Vec<&'static str> = item::Kind::all()
            .iter()
            .map(|k| k.name())
            .chain(block::Kind::all().iter().map(|k| k.name()))
            .collect();
        items.sort_unstable();
        items.dedup();
        // The dump's names (`element_hydrogen`) are judged by the Bedrock name of their item.
        items.retain(|n| !item::Type::find(n).is_some_and(|t| item::is_education(t.name())));
        Self {
            effects,
            enchantments,
            items,
        }
    }

    /// The block kind named `name` (with or without `minecraft:`).
    pub(crate) fn block(name: &str) -> Option<block::Kind> {
        let name = name.strip_prefix(NAMESPACE).unwrap_or(name);
        let kinds = block::Kind::all();
        let i = kinds.binary_search_by(|k| k.name().cmp(name)).ok()?;
        kinds.get(i).copied()
    }

    /// The effect named `name`.
    pub(crate) fn effect(&self, name: &str) -> Option<effect::Type> {
        let name = name.strip_prefix(NAMESPACE).unwrap_or(name);
        let i = self
            .effects
            .binary_search_by(|(n, _)| n.as_str().cmp(name))
            .ok()?;
        self.effects.get(i).map(|(_, t)| *t)
    }

    /// The values of the client's `Effect` enum.
    pub(crate) fn effect_names(&self) -> impl Iterator<Item = &str> {
        self.effects.iter().map(|(n, _)| n.as_str())
    }

    /// The enchantment named `name`.
    pub(crate) fn enchantment(&self, name: &str) -> Option<item::Enchantment> {
        let name = name.strip_prefix(NAMESPACE).unwrap_or(name);
        let i = self
            .enchantments
            .binary_search_by(|(n, _)| n.as_str().cmp(name))
            .ok()?;
        self.enchantments.get(i).map(|(_, e)| *e)
    }

    /// The values of the client's `Enchant` enum.
    pub(crate) fn enchantment_names(&self) -> impl Iterator<Item = &str> {
        self.enchantments.iter().map(|(n, _)| n.as_str())
    }

    /// The values of the client's `Item` enum.
    pub(crate) fn item_names(&self) -> &[&'static str] {
        &self.items
    }

    /// The item an item parameter names ([`item::Type::find`]), if the client offers that name
    /// (no Education item).
    pub(crate) fn item_param(&self, name: &str) -> Option<item::Type> {
        let name = name.strip_prefix(NAMESPACE).unwrap_or(name);
        self.items.binary_search(&name).ok()?;
        item::Type::find(name)
    }
}

/// Splits `line` into words as commands are split:
/// whitespace separates words; a word that starts with `"` ends at the next `"` not escaped, and
/// inside it `\"` and `\\` stand for `"` and `\`; a quote inside a word is kept as it is.
/// A line within the bounds has at most 256 words.
///
/// # Errors
///
/// [`Error::UnclosedQuote`] for an unclosed quote.
/// [`Error::TooLong`] for a line over 2,048 bytes or 512 characters, which no command runs.
pub fn split(line: &str) -> Result<Vec<Word<'_>>> {
    check_length(line)?;
    let mut words = Vec::new();
    let mut at = 0;
    while let Some(word) = next_word(line, at)? {
        at = word.end;
        words.push(word);
    }
    Ok(words)
}

/// The first word of `line`, the command's name; `None` for a line without words. Fails like
/// [`split`] for a line that is too long, and for an unclosed quote in the word itself; the
/// words after it are read by [`fit`], since a parameter may take the rest of the line as typed.
pub(crate) fn first_word(line: &str) -> Result<Option<Word<'_>>> {
    check_length(line)?;
    next_word(line, 0)
}

/// Fits the words of `line` from the byte `from` on to `params`: one argument per parameter
/// given, in order, or where the words stop fitting. That is the byte offset in `line` of a word
/// of the wrong kind or of the first word left over, or the end of the line's text where a
/// required word is missing. Words are read as the parameters take them, so a parameter that
/// takes the rest of the line (or a JSON value) reads quotes as typed.
pub(crate) fn fit(
    params: &[Param],
    line: &str,
    from: usize,
    names: &Names,
) -> Result<Vec<Raw>, Misfit> {
    let end = line.trim_end().len();
    let mut out = Vec::with_capacity(params.len());
    let mut at = from;
    for param in params {
        let Some(word) = word_at(line, at)? else {
            if param.optional {
                break;
            }
            return Err(Misfit::At(end));
        };
        let bad = Misfit::At(word.start);
        let text = word.text.as_ref();
        let mut next = word.end;
        let raw = match &param.kind {
            Kind::Int => Raw::Value(Value::Int(text.parse().map_err(|_| bad)?)),
            Kind::Float => Raw::Value(Value::Float(number(text).ok_or(bad)?)),
            Kind::String | Kind::SoftEnum { .. } => Raw::Value(Value::Text(text.to_owned())),
            Kind::Bool => Raw::Value(Value::Bool(if text.eq_ignore_ascii_case("true") {
                true
            } else if text.eq_ignore_ascii_case("false") {
                false
            } else {
                return Err(bad);
            })),
            Kind::Target => Raw::Target(target(&word).ok_or(bad)?),
            Kind::Name => match target(&word) {
                Some(Target::Name(name)) => Raw::Value(Value::Text(name)),
                _ => return Err(bad),
            },
            Kind::Suffixed { suffix } => {
                let digits = text
                    .len()
                    .checked_sub(suffix.len())
                    .filter(|&at| {
                        text.get(at..)
                            .is_some_and(|s| s.eq_ignore_ascii_case(suffix))
                    })
                    .and_then(|at| text.get(..at))
                    .ok_or(bad)?;
                Raw::Value(Value::Int(digits.parse().map_err(|_| bad)?))
            }
            Kind::Position => {
                let mut coords = [Coord {
                    relative: false,
                    value: 0.0,
                }; 3];
                let mut from_here = at;
                for c in &mut coords {
                    let w = word_at(line, from_here)?.ok_or(Misfit::At(end))?;
                    *c = coord(&w).ok_or(Misfit::At(w.start))?;
                    from_here = w.end;
                }
                next = from_here;
                Raw::Position(coords)
            }
            Kind::Message | Kind::RawText => {
                let rest = rest_from(line, word.start).trim_end();
                next = line.len();
                Raw::Value(Value::Text(rest.to_owned()))
            }
            Kind::Json => {
                let rest = rest_from(line, word.start);
                let json_len = json_end(rest).ok_or(bad)?;
                let json = rest.get(..json_len).ok_or(bad)?;
                next = word.start.checked_add(json_len).ok_or(bad)?;
                // The value must end where a word does.
                if line
                    .get(next..)
                    .and_then(|s| s.chars().next())
                    .is_some_and(|c| !c.is_whitespace())
                {
                    return Err(bad);
                }
                Raw::Value(Value::Text(json.to_owned()))
            }
            Kind::Enum { values, .. } => {
                let value = values
                    .iter()
                    .find(|v| v.eq_ignore_ascii_case(text))
                    .ok_or(bad)?;
                Raw::Value(Value::Text(value.clone()))
            }
            Kind::Item => Raw::Value(Value::Item(names.item_param(text).ok_or(bad)?)),
            Kind::Block => Raw::Value(Value::Block(Names::block(text).ok_or(bad)?)),
            Kind::Effect => Raw::Value(Value::Effect(names.effect(text).ok_or(bad)?)),
            Kind::Enchantment => {
                Raw::Value(Value::Enchantment(names.enchantment(text).ok_or(bad)?))
            }
        };
        out.push(raw);
        at = next;
    }
    match word_at(line, at)? {
        Some(left_over) => Err(Misfit::At(left_over.start)),
        None => Ok(out),
    }
}

/// The official server's syntax error for `line` (without its `/`) that stops fitting at byte
/// `at` ([`fit`]): the word there, with up to [`SYNTAX_CONTEXT`] characters of `/` and the line
/// before and after it.
pub(crate) fn syntax_error(line: &str, at: usize) -> Error {
    let typed = format!("/{line}");
    let at = at.saturating_add(1).min(typed.len());
    let (head, tail) = typed.split_at_checked(at).unwrap_or((&typed, ""));
    let word_len = tail.find(char::is_whitespace).unwrap_or(tail.len());
    let (word, rest) = tail.split_at(word_len);
    let rest = if word.is_empty() { "" } else { rest };
    let skip = head.chars().count().saturating_sub(SYNTAX_CONTEXT);
    Error::Syntax {
        before: head.chars().skip(skip).collect(),
        word: word.to_owned(),
        after: rest.chars().take(SYNTAX_CONTEXT).collect(),
    }
}

/// A target word: `@s`, `@a`, `@p`, `@r` or a name. Other selectors and selector arguments do
/// not fit.
fn target(word: &Word<'_>) -> Option<Target> {
    let text = word.text();
    if !word.is_quoted() && text.starts_with('@') {
        return Some(match text {
            "@s" => Target::Sender,
            "@a" => Target::All,
            "@p" => Target::Nearest,
            "@r" => Target::Random,
            _ => return None,
        });
    }
    Some(Target::Name(text.to_owned()))
}

/// A coordinate word: a number, `~`, or `~` and a number.
fn coord(word: &Word<'_>) -> Option<Coord> {
    let text = word.text.as_ref();
    match text.strip_prefix('~') {
        Some("") => Some(Coord {
            relative: true,
            value: 0.0,
        }),
        Some(rest) => Some(Coord {
            relative: true,
            value: number(rest)?,
        }),
        None => Some(Coord {
            relative: false,
            value: number(text)?,
        }),
    }
}

/// A finite number (no `NaN`, no infinity: the tick holds none).
fn number(text: &str) -> Option<f64> {
    let v: f64 = text.parse().ok()?;
    // `parse` takes "inf" and "NaN"; only digits, signs, a point and an exponent pass.
    let plain = text
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'));
    (plain && v.is_finite()).then_some(v)
}

/// The length of the JSON value `text` starts with; `None` if it does not start with one.
fn json_end(text: &str) -> Option<usize> {
    let mut values = serde_json::Deserializer::from_str(text).into_iter::<IgnoredAny>();
    values.next()?.ok()?;
    Some(values.byte_offset())
}

/// The part of `line` from the byte `start` on, where one of its words starts.
#[expect(
    clippy::expect_used,
    reason = "invariant: a word starts on a character boundary of its line"
)]
fn rest_from(line: &str, start: usize) -> &str {
    line.get(start..).expect("a word starts inside its line")
}

/// `FireResistance` → `fire_resistance`: the snake-case name of a variant, which the generated
/// tables name in their docs.
fn snake_name(variant: &str) -> String {
    let mut out = String::with_capacity(variant.len() + 4);
    for (i, c) in variant.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Fails with [`Error::TooLong`] for a line over [`MAX_LINE_BYTES`] bytes or [`MAX_LINE_CHARS`]
/// characters.
fn check_length(line: &str) -> Result<()> {
    if line.len() > MAX_LINE_BYTES || line.chars().count() > MAX_LINE_CHARS {
        return Err(Error::TooLong);
    }
    Ok(())
}

/// [`next_word`] for [`fit`]: the only way it fails is an unclosed quote.
fn word_at(line: &str, from: usize) -> Result<Option<Word<'_>>, Misfit> {
    next_word(line, from).map_err(|_| Misfit::UnclosedQuote)
}

/// The first word of `line` at or after the byte `from`, a character boundary: `None` if only
/// whitespace follows.
fn next_word(line: &str, from: usize) -> Result<Option<Word<'_>>> {
    #[expect(
        clippy::expect_used,
        reason = "invariant: `from` is 0 or the end of a word, a character boundary of `line`"
    )]
    let tail = line.get(from..).expect("a word ends inside its line");
    let mut chars = tail.char_indices().map(|(i, c)| (from + i, c)).peekable();
    let Some((start, first)) = chars.find(|&(_, c)| !c.is_whitespace()) else {
        return Ok(None);
    };
    if first == '"' {
        let mut text = String::new();
        loop {
            match chars.next() {
                None => return Err(Error::UnclosedQuote),
                Some((closing, '"')) => {
                    return Ok(Some(Word {
                        text: Cow::Owned(text),
                        start,
                        end: closing + 1,
                    }));
                }
                Some((_, '\\')) => match chars.peek() {
                    Some(&(_, escaped @ ('"' | '\\'))) => {
                        text.push(escaped);
                        chars.next();
                    }
                    _ => text.push('\\'),
                },
                Some((_, c)) => text.push(c),
            }
        }
    }
    let end = chars
        .find(|&(_, c)| c.is_whitespace())
        .map_or(line.len(), |(i, _)| i);
    #[expect(
        clippy::expect_used,
        reason = "invariant: `start` and `end` are character boundaries of `line`"
    )]
    let text = line.get(start..end).expect("a word lies inside its line");
    Ok(Some(Word {
        text: Cow::Borrowed(text),
        start,
        end,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(line: &str) -> Vec<String> {
        split(line)
            .unwrap()
            .into_iter()
            .map(|w| w.text.into_owned())
            .collect()
    }

    #[test]
    fn lines_split_at_spaces_and_keep_quoted_words() {
        assert_eq!(texts("give  steve apple"), ["give", "steve", "apple"]);
        assert_eq!(
            texts(r#"give "steve jobs" apple"#),
            ["give", "steve jobs", "apple"]
        );
        assert_eq!(
            texts(r#"echo "This is a \"string containing quotes\"""#),
            ["echo", r#"This is a "string containing quotes""#]
        );
        assert_eq!(texts(r#"a "x\\y" "\n""#), ["a", r"x\y", r"\n"]);
        assert_eq!(texts(r#"ab"cd "e"f"#), [r#"ab"cd"#, "e", "f"]);
        assert_eq!(texts("   "), Vec::<String>::new());
        let quoted: Vec<bool> = split(r#"@a "@a" """#)
            .unwrap()
            .iter()
            .map(Word::is_quoted)
            .collect();
        assert_eq!(quoted, [false, true, true]);
        assert_eq!(split(r#"echo "open"#), Err(Error::UnclosedQuote));
        assert_eq!(split(r#"echo "a\""#), Err(Error::UnclosedQuote));
    }

    #[test]
    fn parameters_taking_the_rest_of_the_line_read_quotes_as_typed() {
        let names = Names::new();
        let text = |value: &str| Raw::Value(Value::Text(value.to_owned()));
        let line = r#"echo it's "open  "#;
        let from = first_word(line).unwrap().unwrap().end;
        assert_eq!(
            fit(&[Param::message("m")], line, from, &names),
            Ok(vec![text(r#"it's "open"#)])
        );
        assert_eq!(
            fit(&[Param::string("s")], line, from, &names),
            Err(Misfit::UnclosedQuote)
        );
        let line = r#"x "he said \"hi\" now" 5"#;
        let from = first_word(line).unwrap().unwrap().end;
        assert_eq!(
            fit(&[Param::string("s"), Param::int("n")], line, from, &names),
            Ok(vec![text("he said \"hi\" now"), Raw::Value(Value::Int(5))])
        );
        let line = r#"x {"a": "b c"} 5"#;
        let from = first_word(line).unwrap().unwrap().end;
        assert_eq!(
            fit(&[Param::json("j"), Param::int("n")], line, from, &names),
            Ok(vec![text(r#"{"a": "b c"}"#), Raw::Value(Value::Int(5))])
        );
    }

    #[test]
    fn long_lines_are_refused() {
        // 512 characters of 4 bytes: both bounds exactly.
        let full = "\u{1f600}".repeat(MAX_LINE_CHARS);
        assert_eq!(full.len(), MAX_LINE_BYTES);
        assert!(split(&full).is_ok());
        assert_eq!(split(&format!("{full}a")), Err(Error::TooLong));
        assert_eq!(split(&"a".repeat(MAX_LINE_CHARS + 1)), Err(Error::TooLong));
        assert_eq!(split(&"a ".repeat(MAX_LINE_CHARS / 2)).unwrap().len(), 256);
        let bytes = format!("{},{:03}", MAX_LINE_BYTES / 1000, MAX_LINE_BYTES % 1000);
        let documented = format!("over {bytes} bytes or {MAX_LINE_CHARS} characters");
        assert!(
            include_str!("parse.rs").contains(&documented),
            "the docs name the bounds"
        );
    }

    #[test]
    fn effect_and_enchantment_names_are_the_table_names() {
        let names = Names::new();
        assert_eq!(
            names.effect("fire_resistance"),
            Some(effect::Type::FireResistance)
        );
        assert_eq!(
            names.effect("minecraft:absorption"),
            Some(effect::Type::Absorption)
        );
        assert_eq!(names.effect("FireResistance"), None);
        assert_eq!(
            names.enchantment("aqua_affinity"),
            Some(item::Enchantment::AquaAffinity)
        );
        assert_eq!(names.effect_names().count(), effect::Type::all().len());
        assert_eq!(
            names.enchantment_names().count(),
            item::Enchantment::all().len()
        );
        assert!(names.item_names().windows(2).all(|w| w[0] < w[1]));
        assert!(item::Type::find("minecraft:stick").is_some());
        assert!(item::Type::find("stone").is_some(), "a block's item");
        assert!(item::Type::find("no_such_item").is_none());
        assert_eq!(
            Names::block("oak_planks").map(block::Kind::name),
            Some("oak_planks")
        );
    }

    /// The `Item` enum and item parameters leave the Education items out, the other items stay.
    #[test]
    fn item_names_leave_the_education_items_out() {
        // The names of an element, a compound, hardened glass and a coloured torch.
        let education = [
            "element_hydrogen",
            "chemical_ammonia",
            "hardened_glass",
            "minecraft:red_torch",
        ];
        for name in education {
            assert!(
                item::Type::find(name).is_some_and(|t| item::is_education(t.name())),
                "{name}"
            );
        }
        let names = Names::new();
        let left = names.item_names();
        assert!(!left.iter().any(|n| education.contains(n)));
        for name in education {
            assert!(names.item_param(name).is_none(), "{name}");
        }
        for name in ["stick", "glass", "torch", "glass_pane"] {
            assert!(names.item_param(name).is_some(), "{name}");
            assert!(left.contains(&name), "{name}");
        }
    }

    #[test]
    fn numbers_are_finite_and_plain() {
        assert_eq!(number("1.5e2"), Some(150.0));
        assert_eq!(number("-3"), Some(-3.0));
        for bad in ["inf", "NaN", "-infinity", "1e999", "0x10", ""] {
            assert_eq!(number(bad), None, "{bad}");
        }
    }
}
