//! Reads Rust sources for the mapping table: the `#[doc(alias = "..")]` attributes and
//! `// pmmp-class: Name` comments on items, and the `// pmmp: A::b, C::d` comments above test
//! functions.
//!
//! rustdoc rejects an alias equal to the item's own name, so a type named like its PMMP class
//! (`pub struct Permission` for `Permission`) carries the class alias as a `// pmmp-class: Name`
//! line above the item instead (its doc comments, attributes and other comments may be in
//! between, blank lines may not). The name must be the item's name; any other alias is a
//! `#[doc(alias)]`.
//!
//! A `// pmmp:` comment is above the test's attributes (`#[test]` first or not); doc comments,
//! other comments and attributes (one or more lines each) may be between it and the `fn`, blank
//! lines may not.

/// One alias attribute entry on an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Alias {
    /// The alias text, escapes resolved.
    pub(crate) name: String,
    /// The line of the attribute, from 1.
    pub(crate) line: usize,
    /// The name of the item the attribute is on (empty if none was found).
    pub(crate) item: String,
}

/// One `// pmmp:` comment and the test it is above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Covered {
    /// The aliases the comment lists.
    pub(crate) aliases: Vec<String>,
    /// The line of the (first) comment line, from 1.
    pub(crate) line: usize,
    /// The test function's name.
    pub(crate) test: String,
}

/// The attribute that starts an alias; `doc(...)` may list other entries too.
const ATTRIBUTE: &str = "#[doc(";
/// The comment that lists the aliases a test covers.
const COMMENT: &str = "// pmmp:";
/// The comment that gives the item below it the class alias equal to its name.
const CLASS_COMMENT: &str = "// pmmp-class:";

/// The aliases of `text` (alias attributes and `// pmmp-class:` comments, by line), or the
/// errors (`line: message`) of malformed ones.
pub(crate) fn aliases(text: &str) -> (Vec<Alias>, Vec<(usize, String)>) {
    let (mut found, mut errors) = attribute_aliases(text);
    let (classes, class_errors) = class_aliases(text);
    found.extend(classes);
    found.sort_by_key(|a| a.line);
    errors.extend(class_errors);
    errors.sort_by_key(|(line, _)| *line);
    (found, errors)
}

/// The `// pmmp-class: Name` comments of `text`.
fn class_aliases(text: &str) -> (Vec<Alias>, Vec<(usize, String)>) {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    let mut offset = 0;
    for (i, raw) in text.split_inclusive('\n').enumerate() {
        let start = offset;
        offset += raw.len();
        let Some(name) = raw.trim().strip_prefix(CLASS_COMMENT).map(str::trim) else {
            continue;
        };
        let line = i + 1;
        let mut cursor = Cursor {
            text,
            pos: start + line_len(raw),
        };
        if name.is_empty() || name.contains(|c: char| !is_ident(c)) {
            errors.push((
                line,
                "`// pmmp-class:` names one class (no namespace, no member)".to_owned(),
            ));
        } else if !skip_to_item(&mut cursor, |_| {}) {
            errors.push((
                line,
                "`// pmmp-class:` is not directly above an item".to_owned(),
            ));
        } else {
            let item = item_name(cursor.rest());
            if item == name {
                found.push(Alias {
                    name: name.to_owned(),
                    line,
                    item,
                });
            } else {
                errors.push((
                    line,
                    format!(
                        "`// pmmp-class: {name}` is on `{item}`: it is for an item named \
                         `{name}`; write `#[doc(alias = \"{name}\")]`"
                    ),
                ));
            }
        }
    }
    (found, errors)
}

/// The alias attributes of `text`, or the errors (`line: message`) of malformed ones.
fn attribute_aliases(text: &str) -> (Vec<Alias>, Vec<(usize, String)>) {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    let mut line = 1;
    let mut counted = 0;
    for (start, _) in text.match_indices(ATTRIBUTE) {
        line += count_newlines(text.get(counted..start).unwrap_or_default());
        counted = start;
        let line_start = text
            .get(..start)
            .and_then(|s| s.rfind('\n'))
            .map_or(0, |i| i + 1);
        let before = text.get(line_start..start).unwrap_or_default().trim();
        // Only attributes on code lines: the line starts with an attribute.
        if !(before.is_empty() || before.ends_with(']')) || before.starts_with("//") {
            continue;
        }
        let mut cursor = Cursor {
            text,
            pos: start + ATTRIBUTE.len(),
        };
        match doc_entries(&mut cursor) {
            Ok(entries) if entries.is_empty() => {}
            Ok(entries) => {
                let item = item_name(text.get(cursor.pos..).unwrap_or_default());
                found.extend(entries.into_iter().map(|name| Alias {
                    name,
                    line,
                    item: item.clone(),
                }));
            }
            Err(e) => errors.push((line, e)),
        }
    }
    (found, errors)
}

/// The `// pmmp:` comments of `text` and their tests, or the errors (`line: message`) of
/// comments that are empty or not directly above a test function.
pub(crate) fn covered(text: &str) -> (Vec<Covered>, Vec<(usize, String)>) {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    let mut lines = Vec::new();
    let mut offset = 0;
    for raw in text.split_inclusive('\n') {
        lines.push((offset, raw.trim()));
        offset += raw.len();
    }
    let mut i = 0;
    while let Some(&(_, first)) = lines.get(i) {
        if !first.starts_with(COMMENT) {
            i += 1;
            continue;
        }
        let line = i + 1;
        let below_test = i
            .checked_sub(1)
            .and_then(|previous| lines.get(previous))
            .is_some_and(|&(_, l)| l.starts_with("#[") && is_test_attribute(l));
        let mut aliases = Vec::new();
        let mut end = 0;
        while let Some(&(start, l)) = lines.get(i) {
            let Some(list) = l.strip_prefix(COMMENT) else {
                break;
            };
            aliases.extend(
                list.split(',')
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .map(str::to_owned),
            );
            end = start + line_len(text.get(start..).unwrap_or_default());
            i += 1;
        }
        let mut cursor = Cursor { text, pos: end };
        let mut is_test = false;
        let direct = skip_to_item(&mut cursor, |attribute| {
            is_test |= is_test_attribute(attribute);
        });
        let function = direct.then(|| fn_name(cursor.rest())).flatten();
        match function {
            _ if aliases.is_empty() => errors.push((line, "`// pmmp:` lists no alias".to_owned())),
            Some(function) if is_test => found.push(Covered {
                aliases,
                line,
                test: function.to_owned(),
            }),
            _ if below_test => errors.push((
                line,
                "`// pmmp:` is below `#[test]`: put it above the test's attributes".to_owned(),
            )),
            _ => errors.push((
                line,
                "`// pmmp:` is not directly above a `#[test]` function".to_owned(),
            )),
        }
    }
    (found, errors)
}

/// The length of the first line of `text` without its line break and trailing whitespace.
fn line_len(text: &str) -> usize {
    text.split('\n').next().unwrap_or_default().trim_end().len()
}

/// Skips the whitespace, comments and attributes before the next item, passing each attribute
/// to `attribute`; false when a blank line is between or no item follows.
fn skip_to_item(cursor: &mut Cursor<'_>, mut attribute: impl FnMut(&str)) -> bool {
    let mut direct = true;
    loop {
        let rest = cursor.rest();
        let space = rest.len() - rest.trim_start().len();
        direct &= count_newlines(rest.get(..space).unwrap_or_default()) <= 1;
        cursor.pos += space;
        let rest = cursor.rest();
        if rest.starts_with("//") {
            cursor.pos += rest.find('\n').unwrap_or(rest.len());
        } else if rest.starts_with("#[") {
            let len = attribute_len(rest);
            attribute(rest.get(..len).unwrap_or_default());
            cursor.pos += len;
        } else {
            return direct && !rest.is_empty();
        }
    }
}

fn count_newlines(text: &str) -> usize {
    text.bytes().filter(|&b| b == b'\n').count()
}

/// Whether a one-line attribute marks a test (`#[test]`, `#[tokio::test(..)]`).
fn is_test_attribute(attribute: &str) -> bool {
    let inner = attribute
        .strip_prefix("#[")
        .and_then(|a| a.strip_suffix(']'))
        .unwrap_or_default();
    let path = inner.split('(').next().unwrap_or_default().trim();
    path == "test" || path.ends_with("::test")
}

/// The name of the function a line declares (`pub async fn name(`).
fn fn_name(line: &str) -> Option<&str> {
    let mut words = line.split(|c: char| !is_ident(c)).filter(|w| !w.is_empty());
    loop {
        match words.next()? {
            "pub" | "crate" | "super" | "async" | "unsafe" | "const" => {}
            "fn" => return words.next(),
            _ => return None,
        }
    }
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A position in a source text.
struct Cursor<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn rest(&self) -> &'a str {
        self.text.get(self.pos..).unwrap_or_default()
    }

    fn skip_whitespace(&mut self) {
        let rest = self.rest();
        self.pos += rest.len() - rest.trim_start().len();
    }

    /// Skips whitespace, then consumes `token` if it comes next.
    fn eat(&mut self, token: &str) -> bool {
        self.skip_whitespace();
        let found = self.rest().starts_with(token);
        if found {
            self.pos += token.len();
        }
        found
    }

    fn expect(&mut self, token: &str) -> Result<(), String> {
        if self.eat(token) {
            Ok(())
        } else {
            Err(format!("malformed doc attribute: expected `{token}`"))
        }
    }

    fn ident(&mut self) -> &'a str {
        self.skip_whitespace();
        let rest = self.rest();
        let len = rest.find(|c: char| !is_ident(c)).unwrap_or(rest.len());
        self.pos += len;
        rest.get(..len).unwrap_or_default()
    }

    /// A string literal: `"..."` with `\\` and `\"` escapes, or a raw `r"..."`, `r#"..."#`.
    fn string(&mut self) -> Result<String, String> {
        self.skip_whitespace();
        let rest = self.rest();
        if let Some(raw) = rest.strip_prefix('r') {
            let hashes = raw.len() - raw.trim_start_matches('#').len();
            let open = format!("{}\"", "#".repeat(hashes));
            let close = format!("\"{}", "#".repeat(hashes));
            let body = raw
                .strip_prefix(open.as_str())
                .ok_or("malformed raw string in doc attribute")?;
            let end = body
                .find(close.as_str())
                .ok_or("unterminated raw string in doc attribute")?;
            let value = body.get(..end).unwrap_or_default().to_owned();
            self.pos += 1 + open.len() + end + close.len();
            return Ok(value);
        }
        let body = rest
            .strip_prefix('"')
            .ok_or("malformed doc attribute: expected a string")?;
        let mut value = String::new();
        let mut chars = body.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => {
                    self.pos += 1 + i + 1;
                    return Ok(value);
                }
                '\\' => match chars.next() {
                    Some((_, e @ ('\\' | '"'))) => value.push(e),
                    _ => return Err("unsupported escape in doc alias".to_owned()),
                },
                c => value.push(c),
            }
        }
        Err("unterminated string in doc attribute".to_owned())
    }
}

/// The aliases of a `doc(...)` attribute whose `#[doc(` the cursor is after; entries other than
/// `alias = ".."` and `alias(".." , ..)` are skipped, up to the closing `)]`.
fn doc_entries(cursor: &mut Cursor<'_>) -> Result<Vec<String>, String> {
    let mut aliases = Vec::new();
    loop {
        if cursor.eat(")") {
            cursor.expect("]")?;
            return Ok(aliases);
        }
        let key = cursor.ident().to_owned();
        if key.is_empty() {
            return Err("malformed doc attribute".to_owned());
        }
        if cursor.eat("=") {
            let value = cursor.string()?;
            if key == "alias" {
                aliases.push(value);
            }
        } else if cursor.eat("(") {
            if key != "alias" {
                return Err(format!("unsupported doc attribute entry `{key}(..)`"));
            }
            while !cursor.eat(")") {
                aliases.push(cursor.string()?);
                if !cursor.eat(",") {
                    cursor.expect(")")?;
                    break;
                }
            }
        }
        if !cursor.eat(",") {
            cursor.expect(")")?;
            cursor.expect("]")?;
            return Ok(aliases);
        }
    }
}

/// The name of the first item in `text`, after attributes and comments: a function, type,
/// constant, module, enum variant or field (`impl Type` for an impl block).
fn item_name(text: &str) -> String {
    let mut cursor = Cursor { text, pos: 0 };
    skip_to_item(&mut cursor, |_| {});
    let mut words = cursor
        .rest()
        .split(|c: char| !is_ident(c) && c != '!')
        .filter(|w| !w.is_empty());
    let mut previous_const = false;
    while let Some(word) = words.next() {
        match word {
            // `pub(crate)`, `pub(in path)` and `extern "C"` split into these words too.
            "pub" | "crate" | "super" | "self" | "in" | "async" | "unsafe" | "extern" | "C"
            | "default" | "safe" => {}
            "const" => previous_const = true,
            "fn" | "struct" | "enum" | "trait" | "type" | "mod" | "static" | "union"
            | "macro_rules!" => return words.next().unwrap_or_default().to_owned(),
            "impl" => return format!("impl {}", words.next().unwrap_or_default()),
            name if previous_const => return name.to_owned(),
            name => return name.trim_end_matches('!').to_owned(),
        }
    }
    String::new()
}

/// The length of the attribute `text` starts with, up to its balanced `]`.
fn attribute_len(text: &str) -> usize {
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in text.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '[' => depth += 1,
            ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alias_list(text: &str) -> Vec<(String, usize, String)> {
        let (found, errors) = aliases(text);
        assert!(errors.is_empty(), "{errors:?}");
        found
            .into_iter()
            .map(|a| (a.name, a.line, a.item))
            .collect()
    }

    #[test]
    fn aliases_read_every_attribute_form_and_their_item() {
        let text = r##"
/// Docs.
#[doc(alias = "World::getSeed")]
pub fn seed(&self) -> i64 {}
#[doc(alias = r"block\tile\Chest::getPair")] #[must_use]
pub(crate) const fn pair() {}
#[doc(alias("A::b", "C::d",))]
#[expect(
    clippy::x,
    reason = "a ] b"
)]
pub struct Thing;
#[doc(hidden, alias = "block\\Chest")]
    Variant { a: u8 },
#[doc(alias = r#"E"#)] field: u8,
impl X {}
#[doc(alias = "Imp")]
impl Foo for Bar {}
"##;
        let found = alias_list(text);
        let expected = [
            ("World::getSeed", 3, "seed"),
            (r"block\tile\Chest::getPair", 5, "pair"),
            ("A::b", 7, "Thing"),
            ("C::d", 7, "Thing"),
            (r"block\Chest", 13, "Variant"),
            ("E", 15, "field"),
            ("Imp", 17, "impl Foo"),
        ];
        let expected: Vec<_> = expected
            .iter()
            .map(|(a, l, i)| ((*a).to_owned(), *l, (*i).to_owned()))
            .collect();
        assert_eq!(found, expected);
    }

    #[test]
    fn aliases_ignore_comments_and_report_malformed_attributes() {
        assert!(alias_list("/// use #[doc(alias = \"X\")] on items\nfn a() {}").is_empty());
        let (_, errors) =
            aliases("#[doc(alias = X)]\nfn a() {}\n#[doc(alias = \"a\\n\")]\nfn b() {}");
        let lines: Vec<usize> = errors.iter().map(|(line, _)| *line).collect();
        assert_eq!(lines, [1, 3]);
    }

    #[test]
    fn covered_attributes_comments_to_the_test_below() {
        let text = "
mod tests {
    // pmmp: A::b, C::d
    // pmmp: E
    #[test]
    #[ignore = \"slow\"]
    fn works() {}

    // pmmp: F
    #[tokio::test(flavor = \"current_thread\")]
    async fn later() {}

    // pmmp: G
    /// Docs.
    #[cfg_attr(
        miri,
        ignore
    )]
    // A note.
    #[test]
    #[expect(clippy::x, reason = \"y\")]
    fn documented() {}
}
";
        let (found, errors) = covered(text);
        assert!(errors.is_empty(), "{errors:?}");
        let got: Vec<_> = found
            .iter()
            .map(|c| (c.aliases.join(" "), c.line, c.test.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("A::b C::d E".to_owned(), 3, "works"),
                ("F".to_owned(), 9, "later"),
                ("G".to_owned(), 13, "documented")
            ]
        );
    }

    #[test]
    fn covered_rejects_comments_away_from_tests() {
        let text = "// pmmp: A::b\nfn helper() {}\n// pmmp: A::b\n\n#[test]\nfn t() {}\n\
                    // pmmp:\n#[test]\nfn u() {}\n#[test]\n// pmmp: A::b\nfn v() {}\n\
                    // pmmp: A::b\n/// Docs.\n\n#[test]\nfn w() {}\n";
        let (found, errors) = covered(text);
        assert!(found.is_empty());
        let lines: Vec<usize> = errors.iter().map(|(line, _)| *line).collect();
        assert_eq!(lines, [1, 3, 7, 11, 13]);
        assert!(
            errors
                .get(3)
                .is_some_and(|(_, e)| e.contains("below `#[test]`")),
            "{errors:?}"
        );
    }

    #[test]
    fn class_comments_name_the_item_below_them() {
        let text = r"
// pmmp-class: Permission
/// Docs.
#[derive(Debug)]
pub struct Permission;
// pmmp-class: Skin
pub struct Other;
// pmmp-class: Form

pub enum Form {}
// pmmp-class: block\Chest
pub struct Chest;
";
        let (found, errors) = aliases(text);
        let found: Vec<_> = found
            .iter()
            .map(|a| (a.name.as_str(), a.line, a.item.as_str()))
            .collect();
        assert_eq!(found, [("Permission", 2, "Permission")]);
        let lines: Vec<usize> = errors.iter().map(|(line, _)| *line).collect();
        assert_eq!(lines, [6, 8, 11]);
    }
}
