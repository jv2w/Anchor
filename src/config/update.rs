//! Updating a settings file to this Anchor ([`Config::update_file`]): the keys of the default
//! file that it lacks are added with the comment lines above them there, and keys an older Anchor
//! knew under another name are renamed. Each edit is made in the text, at a place the TOML parser
//! reports, so every other byte of the file stays as it was.

use std::io;
use std::path::Path;

use toml::de::DeTable;

use super::{Config, DEFAULT_FILE, Error, MAX_FILE_BYTES, RENAMED_KEYS, Result};
use crate::fs::{self, Access};

/// What [`Config::update_file`] changed in the settings file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Update {
    added: Vec<String>,
    renamed: Vec<(&'static str, &'static str)>,
}

impl Update {
    /// The keys added, such as `general.recipe-book`, in the order of the default file.
    pub fn added(&self) -> &[String] {
        &self.added
    }

    /// The keys renamed, each as its old and its new key, such as
    /// (`general.experience-bottles`, `general.experience-orbs`).
    pub fn renamed(&self) -> &[(&'static str, &'static str)] {
        &self.renamed
    }

    /// Whether nothing changed, so the file was not written.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.renamed.is_empty()
    }
}

/// A table of the default file.
struct Section {
    /// The table's name, such as `general`.
    name: &'static str,
    /// The comment lines right above the table's header, and the header line.
    header: &'static str,
    /// The table's keys, in file order.
    keys: Vec<Entry>,
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

/// Returns `text` with the missing keys and tables added and the old keys renamed, and what
/// changed.
fn edit(text: &str) -> Result<(String, Update)> {
    let root = DeTable::parse(text).map_err(|e| super::syntax(text, &e))?;
    let root = root.get_ref();
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut update = Update::default();
    let mut edits = Vec::new();
    // Missing tables, put after everything else: an insertion at the end of the text belongs to
    // the table the text ends in.
    let mut appended = String::new();
    for section in sections() {
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
        let header = text.as_bytes().get(table.span().start) == Some(&b'[');
        // A key goes after the line of the key before it in the default file.
        let mut after = line_end(text, table.span().end);
        for key in &section.keys {
            if let Some(value) = entries.get(key.name) {
                after = line_end(text, value.span().end);
                continue;
            }
            // With both keys, the old one stays for the load to reject.
            if let Some((old_name, renamed)) = renamed_to(section.name, key.name)
                && let Some((old, value)) = entries.get_key_value(old_name)
            {
                edits.push(Edit {
                    start: old.span().start,
                    end: old.span().end,
                    text: key.name.to_owned(),
                });
                update.renamed.push(renamed);
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
    // Stable: insertions at one place keep the default file's order and come before a key
    // renamed there.
    edits.sort_by_key(|e| (e.start, e.end));
    Ok((splice(text, &edits, &appended, eol), update))
}

/// The tables of the default file, in file order.
#[expect(
    clippy::string_slice,
    reason = "invariant: the bounds are line starts and ends of the text"
)]
fn sections() -> Vec<Section> {
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

/// The old name of key `name` of table `table`, if [`RENAMED_KEYS`] renamed it within that
/// table, and that entry.
fn renamed_to(table: &str, name: &str) -> Option<(&'static str, (&'static str, &'static str))> {
    let in_table = |path: &'static str| path.strip_prefix(table)?.strip_prefix('.');
    RENAMED_KEYS.iter().find_map(|&(old, new)| {
        let old_name = in_table(old)?;
        (in_table(new)? == name).then_some((old_name, (old, new)))
    })
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

/// Returns `text` with `edits` (sorted by place, not overlapping) made and `appended` at the end,
/// a blank line before it.
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
    use super::{RENAMED_KEYS, sections};

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
        assert_eq!(keys.len(), 65);
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

    #[test]
    fn every_rename_stays_in_its_table() {
        for (old, new) in RENAMED_KEYS {
            let table = |path: &str| path.split_once('.').map(|(table, _)| table.to_owned());
            assert_eq!(table(old), table(new), "{old} -> {new}");
        }
    }
}
