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
use crate::nbt::Compound;

/// The longest line, in bytes: PMMP's chat limit (`Player::MAX_CHAT_BYTE_LENGTH`), which its
/// commands share.
pub(crate) const MAX_LINE_BYTES: usize = 2048;
/// The longest line, in characters (PMMP `Player::MAX_CHAT_CHAR_LENGTH`).
pub(crate) const MAX_LINE_CHARS: usize = 512;
/// The namespace item and block names may carry.
const NAMESPACE: &str = "minecraft:";

/// A word of a line and where it starts (its opening quote, if quoted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Word<'a> {
    pub(crate) text: Cow<'a, str>,
    pub(crate) start: usize,
}

/// Splits `line` into words (PMMP `CommandStringHelper::parseQuoteAware`): whitespace separates
/// words; a word that starts with `"` ends at the next `"` not escaped, and inside it `\"` and
/// `\\` stand for `"` and `\`; a quote inside a word is kept as it is. An unclosed quote is an
/// error (PMMP would split the rest at spaces). A line within the bounds has at most 256 words.
pub(crate) fn split(line: &str) -> Result<Vec<Word<'_>>> {
    if line.len() > MAX_LINE_BYTES || line.chars().count() > MAX_LINE_CHARS {
        return Err(Error::TooLong);
    }
    let mut words = Vec::new();
    let mut chars = line.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if c == '"' {
            chars.next();
            let mut text = String::new();
            let mut closed = false;
            while let Some((_, c)) = chars.next() {
                match c {
                    '"' => {
                        closed = true;
                        break;
                    }
                    '\\' => match chars.peek() {
                        Some(&(_, e @ ('"' | '\\'))) => {
                            text.push(e);
                            chars.next();
                        }
                        _ => text.push('\\'),
                    },
                    c => text.push(c),
                }
            }
            if !closed {
                return Err(Error::UnclosedQuote);
            }
            words.push(Word {
                text: Cow::Owned(text),
                start,
            });
        } else {
            let mut end = line.len();
            while let Some(&(i, c)) = chars.peek() {
                if c.is_whitespace() {
                    end = i;
                    break;
                }
                chars.next();
            }
            words.push(Word {
                text: Cow::Borrowed(line.get(start..end).unwrap_or_default()),
                start,
            });
        }
    }
    Ok(words)
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
    /// What the client offers for an item parameter: every item and block name, in name order.
    items: Vec<&'static str>,
}

impl Names {
    /// Builds the name tables.
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
        Self {
            effects,
            enchantments,
            items,
        }
    }

    /// The item named `name` (with or without `minecraft:`): an item's, or the item of a
    /// block's default state.
    pub(crate) fn item(name: &str) -> Option<item::Type> {
        let name = name.strip_prefix(NAMESPACE).unwrap_or(name);
        let kinds = item::Kind::all();
        if let Ok(i) = kinds.binary_search_by(|k| k.name().cmp(name)) {
            return kinds.get(i).map(|k| k.default_type());
        }
        let state = Self::block(name)?.default_state();
        item::Type::from_name(state.name(), 0, Some(state), &Compound::new())
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

    /// The enchantment named `name`.
    pub(crate) fn enchantment(&self, name: &str) -> Option<item::Enchantment> {
        let name = name.strip_prefix(NAMESPACE).unwrap_or(name);
        let i = self
            .enchantments
            .binary_search_by(|(n, _)| n.as_str().cmp(name))
            .ok()?;
        self.enchantments.get(i).map(|(_, e)| *e)
    }

    /// The values of the client's `Item` enum.
    pub(crate) fn item_names(&self) -> &[&'static str] {
        &self.items
    }

    /// The values of the client's `Effect` enum.
    pub(crate) fn effect_names(&self) -> impl Iterator<Item = &str> {
        self.effects.iter().map(|(n, _)| n.as_str())
    }

    /// The values of the client's `Enchant` enum.
    pub(crate) fn enchantment_names(&self) -> impl Iterator<Item = &str> {
        self.enchantments.iter().map(|(n, _)| n.as_str())
    }
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

/// Fits `words` (of `line`) to `params`: one argument per parameter given, in order, or `None`
/// if the words do not fit (a word of the wrong kind, too few words, or words left over).
pub(crate) fn fit(
    params: &[Param],
    line: &str,
    words: &[Word<'_>],
    names: &Names,
) -> Option<Vec<Raw>> {
    let mut out = Vec::with_capacity(params.len());
    let mut i = 0;
    for param in params {
        let Some(word) = words.get(i) else {
            if param.optional {
                break;
            }
            return None;
        };
        let text = word.text.as_ref();
        let raw = match &param.kind {
            Kind::Int => Raw::Value(Value::Int(text.parse().ok()?)),
            Kind::Float => Raw::Value(Value::Float(number(text)?)),
            Kind::String => Raw::Value(Value::Text(text.to_owned())),
            Kind::Bool => Raw::Value(Value::Bool(if text.eq_ignore_ascii_case("true") {
                true
            } else if text.eq_ignore_ascii_case("false") {
                false
            } else {
                return None;
            })),
            Kind::Target => Raw::Target(target(word)?),
            Kind::Position => {
                let [x, y, z] = [words.get(i)?, words.get(i + 1)?, words.get(i + 2)?];
                i += 2;
                Raw::Position([coord(x)?, coord(y)?, coord(z)?])
            }
            Kind::Message | Kind::RawText => {
                let rest = line.get(word.start..).unwrap_or_default().trim_end();
                i = words.len();
                out.push(Raw::Value(Value::Text(rest.to_owned())));
                continue;
            }
            Kind::Json => {
                let rest = line.get(word.start..).unwrap_or_default();
                let end = json_end(rest)?;
                let json = rest.get(..end)?;
                let after = word.start.checked_add(end)?;
                // The value must end where a word does.
                if line
                    .get(after..)
                    .and_then(|s| s.chars().next())
                    .is_some_and(|c| !c.is_whitespace())
                {
                    return None;
                }
                i = words
                    .iter()
                    .position(|w| w.start >= after)
                    .unwrap_or(words.len());
                out.push(Raw::Value(Value::Text(json.to_owned())));
                continue;
            }
            Kind::Enum { values, .. } => {
                let value = values.iter().find(|v| v.eq_ignore_ascii_case(text))?;
                Raw::Value(Value::Text(value.clone()))
            }
            Kind::Item => Raw::Value(Value::Item(Names::item(text)?)),
            Kind::Block => Raw::Value(Value::Block(Names::block(text)?)),
            Kind::Effect => Raw::Value(Value::Effect(names.effect(text)?)),
            Kind::Enchantment => Raw::Value(Value::Enchantment(names.enchantment(text)?)),
        };
        out.push(raw);
        i += 1;
    }
    (i == words.len()).then_some(out)
}

/// A finite number (no `NaN`, no infinity: the tick holds none, CLAUDE.md 2.1).
fn number(text: &str) -> Option<f64> {
    let v: f64 = text.parse().ok()?;
    // `parse` takes "inf" and "NaN"; only digits, signs, a point and an exponent pass.
    let plain = text
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'));
    (plain && v.is_finite()).then_some(v)
}

/// A target word: `@s`, `@a`, `@p`, `@r` or a name. Other selectors and selector arguments do
/// not fit.
fn target(word: &Word<'_>) -> Option<Target> {
    let text = word.text.as_ref();
    let quoted = matches!(word.text, Cow::Owned(_));
    if !quoted && text.starts_with('@') {
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

/// The length of the JSON value `text` starts with; `None` if it does not start with one.
fn json_end(text: &str) -> Option<usize> {
    let mut values = serde_json::Deserializer::from_str(text).into_iter::<IgnoredAny>();
    values.next()?.ok()?;
    Some(values.byte_offset())
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
            texts(r#"say "This is a \"string containing quotes\"""#),
            ["say", r#"This is a "string containing quotes""#]
        );
        assert_eq!(texts(r#"a "x\\y" "\n""#), ["a", r"x\y", r"\n"]);
        assert_eq!(texts(r#"ab"cd "e"f"#), [r#"ab"cd"#, "e", "f"]);
        assert_eq!(texts("   "), Vec::<String>::new());
        assert_eq!(split(r#"say "open"#), Err(Error::UnclosedQuote));
        assert_eq!(split(r#"say "a\""#), Err(Error::UnclosedQuote));
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
        assert!(Names::item("minecraft:stick").is_some());
        assert!(Names::item("stone").is_some(), "a block's item");
        assert!(Names::item("no_such_item").is_none());
        assert_eq!(
            Names::block("oak_planks").map(block::Kind::name),
            Some("oak_planks")
        );
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
