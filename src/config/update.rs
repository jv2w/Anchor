//! Updating a settings file to this Anchor ([`Config::update_file`]): the keys of the default
//! file that it lacks are added with the comment lines above them there, and keys Anchor does
//! not know are removed with the comment lines right above them. Each edit is made in the text,
//! at a place the TOML parser reports, so every other byte of the file stays as it was.

use std::io;
use std::path::Path;

use toml::Spanned;
use toml::de::{DeString, DeTable, DeValue};

use super::{Config, DEFAULT_FILE, Error, Key, MAX_FILE_BYTES, Result, WorldSettings};
use crate::fs::{self, Access};

/// What [`Config::update_file`] changed in the settings file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Update {
    added: Vec<String>,
    removed: Vec<String>,
}

impl Update {
    /// The keys added, such as `general.motd`, in the order of the default file.
    pub fn added(&self) -> &[String] {
        &self.added
    }

    /// Whether nothing changed, so the file was not written.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }

    /// The unknown keys removed, such as `general.old-key`, in file order.
    pub fn removed(&self) -> &[String] {
        &self.removed
    }
}

/// A table of the default file.
pub(super) struct Section {
    /// The table's name, such as `general`.
    pub(super) name: &'static str,
    /// The comment lines right above the table's header, and the header line.
    header: &'static str,
    /// The table's keys, in file order.
    keys: Vec<Entry>,
}

impl Section {
    /// Whether the default file holds the key `name` in this table: the one list of the keys
    /// Anchor knows, which loading and updating a settings file both follow.
    pub(super) fn has_key(&self, name: &str) -> bool {
        self.keys.iter().any(|k| k.name == name)
    }
}

/// A key of the default file.
struct Entry {
    /// The key's name, such as `motd`.
    name: &'static str,
    /// The comment lines right above the key's line, and that line.
    lines: &'static str,
}

/// One change to the text: `text` replaces the bytes `start..end`, which are empty for an
/// insertion.
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// Updates the settings file at `path` (see [`Config::update_file`]).
pub(super) fn run(path: &Path) -> Result<Update> {
    let bytes = super::read(path)?.ok_or_else(|| Error::Read {
        path: path.to_owned(),
        source: io::ErrorKind::NotFound.into(),
    })?;
    let text = super::utf8(&bytes)?;
    let (updated, update) = edit(text)?;
    if u64::try_from(updated.len()).map_or(true, |len| len > MAX_FILE_BYTES) {
        return Err(Error::TooLarge {
            path: path.to_owned(),
        });
    }
    // The update fixes no fault: a file that does not load is left as it is, for its error to
    // be fixed first.
    Config::default().apply(updated.as_bytes())?;
    if !update.is_empty() {
        let written = fs::write_atomic(path, updated.as_bytes(), Access::Shared);
        written.map_err(|source| Error::Write {
            path: path.to_owned(),
            source,
        })?;
    }
    Ok(update)
}

/// The `text` with the missing keys and tables added and the unknown keys removed, and what
/// changed.
fn edit(text: &str) -> Result<(String, Update)> {
    let root = DeTable::parse(text).map_err(|e| super::syntax(text, &e))?;
    let root = root.get_ref();
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let sections = sections();
    let ends = value_ends(root, text);
    let mut update = Update::default();
    let mut edits = Vec::new();
    // The unknown keys with where their removal starts, for the file order.
    let mut removed = Vec::new();
    let mut remove = |edits: &mut Vec<Edit>, table: &str, key: &Spanned<DeString<'_>>, value| {
        if let Some(edit) = removal(text, &ends, key, value) {
            removed.push((edit.start, Key::new(table, key.get_ref()).path()));
            edits.push(edit);
        }
    };
    // Unknown tables, under a header or inline, stay for the build to reject.
    for (key, value) in root {
        let name: &str = key.get_ref();
        let is_table = value.get_ref().as_table().is_some();
        if name != "worlds" && !is_table && !sections.iter().any(|s| s.name == name) {
            remove(&mut edits, "", key, value);
        }
    }
    // The tables of the worlds lose their unknown keys as the other tables do, and gain none.
    if let Some(worlds) = root.get("worlds").and_then(|w| w.get_ref().as_table()) {
        for (world, table) in worlds {
            let Some(entries) = table.get_ref().as_table() else {
                continue;
            };
            if !has_header(text, table) {
                continue;
            }
            let world: &str = world.get_ref();
            let path = format!("worlds.{world}");
            for (key, value) in entries {
                let name: &str = key.get_ref();
                if !WorldSettings::KEYS.contains(&name) {
                    remove(&mut edits, path.as_str(), key, value);
                }
            }
        }
    }
    // Missing tables, put after everything else: an insertion at the end of the text belongs to
    // the table the text ends in.
    let mut appended = String::new();
    for section in &sections {
        let Some(table) = root.get(section.name) else {
            if !appended.is_empty() {
                appended.push_str(eol);
            }
            appended.push_str(&lines(section.header, eol));
            for key in &section.keys {
                appended.push_str(&lines(key.lines, eol));
                update.added.push(format!("{}.{}", section.name, key.name));
            }
            continue;
        };
        // Not a table: the load names the fault.
        let Some(entries) = table.get_ref().as_table() else {
            continue;
        };
        let header = has_header(text, table);
        // Without a header, the lines of a key may hold more than the key.
        if header {
            for (key, value) in entries {
                let name: &str = key.get_ref();
                if !section.has_key(name) {
                    remove(&mut edits, section.name, key, value);
                }
            }
        }
        // A key goes after the line of the key before it in the default file.
        let mut after = line_end(text, table.span().end);
        for key in &section.keys {
            if let Some(value) = entries.get(key.name) {
                after = line_end(text, value.span().end);
                continue;
            }
            // An inline table or dotted keys: a line after them would not be in the table.
            if !header {
                return Err(Error::HeaderlessTable(section.name.to_owned()));
            }
            edits.push(Edit {
                start: after,
                end: after,
                text: lines(key.lines, eol),
            });
            update.added.push(format!("{}.{}", section.name, key.name));
        }
    }
    // Stable: insertions at one place keep the default file's order.
    edits.sort_by_key(|e| (e.start, e.end));
    removed.sort();
    update.removed = removed.into_iter().map(|(_, path)| path).collect();
    Ok((splice(text, &edits, &appended, eol), update))
}

/// The removal of the unknown key `key` with its value `value`: its lines and the comment lines
/// right above them. `None` when it is not a `key = value` line, such as a dotted key or a table
/// under its own header, which the load then names. `ends` is [`value_ends`] of `text`.
fn removal(
    text: &str,
    ends: &[usize],
    key: &Spanned<DeString<'_>>,
    value: &Spanned<DeValue<'_>>,
) -> Option<Edit> {
    let (key, value) = (key.span(), value.span());
    let between = text.get(key.end..value.start)?;
    let quote = |c: char| c.is_whitespace() || c == '"' || c == '\'';
    if between.trim_matches(quote) != "=" {
        return None;
    }
    let line_start = |at: usize| {
        let before = text.as_bytes().get(..at).unwrap_or_default();
        before
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1)
    };
    let mut start = line_start(key.start);
    // A line of a multi-line string above is not a comment line.
    let floor = ends.partition_point(|&end| end <= start);
    let floor = floor.checked_sub(1).and_then(|i| ends.get(i)).copied();
    while start > floor.unwrap_or(0) {
        let above = line_start(start - 1);
        if !text.get(above..start)?.trim_start().starts_with('#') {
            break;
        }
        start = above;
    }
    Some(Edit {
        start,
        end: line_end(text, value.end),
        text: String::new(),
    })
}

/// The ends of the lines where the values of `root` that are not tables end, sorted.
fn value_ends(root: &DeTable<'_>, text: &str) -> Vec<usize> {
    let mut ends = Vec::new();
    // A stack, not recursion: the file sets the nesting depth.
    let mut tables = vec![root];
    while let Some(table) = tables.pop() {
        for value in table.values() {
            match value.get_ref() {
                DeValue::Table(inner) => tables.push(inner),
                _ => ends.push(line_end(text, value.span().end)),
            }
        }
    }
    ends.sort_unstable();
    ends
}

/// Whether the table `table` is written under its own `[header]` line, so that its keys have
/// lines of their own; an inline table or dotted keys are not.
fn has_header(text: &str, table: &Spanned<DeValue<'_>>) -> bool {
    text.as_bytes().get(table.span().start) == Some(&b'[')
}

/// The tables of the default file, in file order.
#[expect(
    clippy::string_slice,
    reason = "invariant: the bounds are line starts and ends of the text"
)]
pub(super) fn sections() -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    // The start of the comment lines right above the current line.
    let mut comment = None;
    let mut start = 0;
    for line in DEFAULT_FILE.split_inclusive('\n') {
        let end = start + line.len();
        if line.starts_with('#') {
            comment.get_or_insert(start);
        } else {
            let block = &DEFAULT_FILE[comment.take().unwrap_or(start)..end];
            if let Some(name) = line.trim_end().strip_prefix('[') {
                sections.push(Section {
                    name: name.trim_end_matches(']'),
                    header: block,
                    keys: Vec::new(),
                });
            } else if let Some((name, _)) = line.split_once(" = ")
                && let Some(section) = sections.last_mut()
            {
                section.keys.push(Entry { name, lines: block });
            }
        }
        start = end;
    }
    sections
}

/// The end of the line holding byte `at` of `text`: after its line feed, or the end of the text.
fn line_end(text: &str, at: usize) -> usize {
    let rest = text.as_bytes().get(at..).unwrap_or_default();
    rest.iter()
        .position(|&b| b == b'\n')
        .map_or(text.len(), |i| at + i + 1)
}

/// Lines of the default file with the line ending `eol`.
fn lines(text: &str, eol: &str) -> String {
    text.replace('\n', eol)
}

/// The `text` with `edits` (sorted by place, not overlapping) made and `appended` at the end, a
/// blank line before it.
#[expect(
    clippy::string_slice,
    reason = "invariant: edits start and end at token bounds and line ends the parser reported"
)]
fn splice(text: &str, edits: &[Edit], appended: &str, eol: &str) -> String {
    let added: usize = edits.iter().map(|e| e.text.len()).sum();
    let mut out = String::with_capacity(text.len() + added + appended.len() + 2 * eol.len());
    let mut at = 0;
    for edit in edits {
        out.push_str(&text[at..edit.start]);
        // The last line may have no line ending.
        if edit.start == text.len() && !out.is_empty() && !out.ends_with('\n') {
            out.push_str(eol);
        }
        out.push_str(&edit.text);
        at = edit.end;
    }
    out.push_str(&text[at..]);
    if !appended.is_empty() {
        if !out.is_empty() {
            if !out.ends_with('\n') {
                out.push_str(eol);
            }
            out.push_str(eol);
        }
        out.push_str(appended);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_file_splits_into_its_tables_and_keys() {
        let sections = sections();
        let names: Vec<&str> = sections.iter().map(|s| s.name).collect();
        assert_eq!(names, ["general", "developer"]);
        assert_eq!(
            sections[0].header,
            "# Settings for anyone opening a server.\n[general]\n"
        );
        let keys: Vec<_> = sections.iter().flat_map(|s| &s.keys).collect();
        assert_eq!(keys.len(), crate::config::tests::DEFAULT_KEY_COUNT);
        assert_eq!(
            keys[0].lines,
            "# Name shown in the server list.\nmotd = \"Anchor Server\"\n"
        );
        // A key right below another has no comment lines of its own.
        let ipv6 = keys
            .iter()
            .find(|k| k.name == "server-ipv6")
            .expect("a key");
        assert_eq!(ipv6.lines, "server-ipv6 = \"::\"\n");
        for key in keys {
            let line = key.lines.lines().last().expect("a line");
            assert!(line.starts_with(&format!("{} = ", key.name)), "{line}");
            assert!(key.lines.lines().rev().skip(1).all(|l| l.starts_with('#')));
        }
    }
}
