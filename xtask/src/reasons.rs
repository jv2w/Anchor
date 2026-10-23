//! `verify`'s check of lint exceptions: the reason of every `#[expect]` in the repository's Rust
//! sources ([`SOURCE_DIRS`]) starts with a tag. Comments and string literals are skipped,
//! so test data that spells an attribute is not taken for one.

use std::path::Path;

use crate::SOURCE_DIRS;
use crate::table::sources;

/// The tags a reason starts with, each followed by `: `.
const TAGS: &[&str] = &[
    "safety",
    "performance",
    "consistency",
    "invariant",
    "off-tick",
];

/// `path:line: message` for every lint reason without a tag.
pub(crate) fn check(root: &Path) -> Result<Vec<String>, String> {
    let mut errors = Vec::new();
    for (path, text) in sources(root, SOURCE_DIRS)? {
        for (line, reason) in reasons(&text) {
            if !tagged(reason) {
                errors.push(format!(
                    "{path}:{line}: lint reason without a tag ({}, then `: `)",
                    TAGS.join(", ")
                ));
            }
        }
    }
    Ok(errors)
}

/// Whether `reason` (a string's text, up to the end of the file) starts with a tag and `: `.
fn tagged(reason: &str) -> bool {
    TAGS.iter().any(|tag| {
        reason
            .strip_prefix(tag)
            .is_some_and(|rest| rest.starts_with(": "))
    })
}

/// The `reason = "..."` (or raw string) arguments (after `(` or `,`) in the code of `text`: each string's line
/// and its text, up to the end of the file.
fn reasons(text: &str) -> Vec<(usize, &str)> {
    let mut found = Vec::new();
    let mut line = 1;
    let mut at = 0;
    while let (Some(before), Some(rest)) = (text.get(..at), text.get(at..)) {
        let Some(c) = rest.chars().next() else {
            break;
        };
        let literal = literal_len(before, rest);
        if literal.is_some() && is_reason(before) {
            // A string (`"`) or raw string (`r#"`); a comment or char literal has no `"` here.
            if let Some(reason) = rest.trim_start_matches(['r', '#']).strip_prefix('"') {
                found.push((line, reason));
            }
        }
        let len = literal.unwrap_or(c.len_utf8());
        line += rest.get(..len).map_or(0, |s| s.matches('\n').count());
        at += len;
    }
    found
}

/// Whether the code `before` a string ends with `reason =` as an argument.
fn is_reason(before: &str) -> bool {
    before
        .trim_end()
        .strip_suffix('=')
        .and_then(|b| b.trim_end().strip_suffix("reason"))
        .is_some_and(|b| b.trim_end().ends_with(['(', ',']))
}

/// The length of the comment or literal `rest` starts with, `before` being the code before it;
/// `None` if it starts none.
fn literal_len(before: &str, rest: &str) -> Option<usize> {
    if rest.starts_with("//") {
        return Some(rest.find('\n').unwrap_or(rest.len()));
    }
    if rest.starts_with("/*") {
        return Some(block_comment_len(rest));
    }
    if rest.starts_with('"') {
        return Some(string_len(rest));
    }
    if rest.starts_with('r') {
        // `r"`, `r#"` or `br"`, not the end of an identifier.
        let ident = before.len() - before.trim_end_matches(is_ident).len();
        if ident == 0 || (ident == 1 && before.ends_with('b')) {
            return raw_string_len(rest);
        }
        return None;
    }
    if rest.starts_with('\'') {
        return char_len(rest);
    }
    None
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The length of the block comment `rest` starts with; block comments nest.
fn block_comment_len(rest: &str) -> usize {
    let mut depth = 0_usize;
    let mut at = 0;
    while let Some(tail) = rest.get(at..) {
        if tail.starts_with("/*") {
            depth += 1;
            at += 2;
        } else if tail.starts_with("*/") {
            depth -= 1;
            at += 2;
            if depth == 0 {
                return at;
            }
        } else if let Some(c) = tail.chars().next() {
            at += c.len_utf8();
        } else {
            break;
        }
    }
    rest.len()
}

/// The length of the string literal `rest` starts with (its `"`).
fn string_len(rest: &str) -> usize {
    let mut escaped = false;
    for (i, c) in rest.char_indices().skip(1) {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return i + 1,
            _ => {}
        }
    }
    rest.len()
}

/// The length of the raw string `rest` starts with (its `r`), or `None` if it is no raw string
/// (`r#ident`, or an identifier starting with `r`).
fn raw_string_len(rest: &str) -> Option<usize> {
    let after = rest.get(1..)?;
    let hashes = after.len() - after.trim_start_matches('#').len();
    let body = after.get(hashes..)?.strip_prefix('"')?;
    let end = format!("\"{}", "#".repeat(hashes));
    let start = 2 + hashes;
    Some(
        body.find(&end)
            .map_or(rest.len(), |i| start + i + end.len()),
    )
}

/// The length of the character literal `rest` starts with (its `'`), or `None` for a lifetime.
fn char_len(rest: &str) -> Option<usize> {
    let mut chars = rest.char_indices().skip(1);
    let (_, first) = chars.next()?;
    if first == '\\' {
        // The escaped character, then up to the closing quote (`'\u{1F600}'`).
        chars.next()?;
        return chars.find(|&(_, c)| c == '\'').map(|(i, _)| i + 1);
    }
    let (i, second) = chars.next()?;
    (second == '\'').then_some(i + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_are_found_outside_comments_and_literals() {
        let text = r###"
#[expect(clippy::a, reason = "safety: fine")]
#[expect(
    clippy::b,
    reason = "no tag"
)]
// #[expect(clippy::c, reason = "in a comment")]
/* #[expect(clippy::c, reason = "in /* a nested */ block")] */
const DATA: &str = r##"#[expect(clippy::d, reason = "a ] b")]"##;
const QUOTED: &str = "#[expect(clippy::e, reason = \"escaped\")]";
fn f<'a>(x: &'a str) -> char { let _ = br#"x"#; let _ = '\''; '"' }
#[expect(clippy::f, reason="off-tick:no space")]
#[expect(clippy::g, reason = "performance: two \
          lines")]
let reason = "a variable";
#[expect(clippy::h, reason = "invariant: after é")]
#[expect(clippy::i, reason = r#"no tag"#)]
#[expect(clippy::j, reason = r"safety: raw")]
"###;
        let found: Vec<(usize, bool)> = reasons(text)
            .into_iter()
            .map(|(line, reason)| (line, tagged(reason)))
            .collect();
        assert_eq!(
            found,
            [
                (2, true),
                (5, false),
                (12, false),
                (13, true),
                (16, true),
                (17, false),
                (18, true)
            ]
        );
    }

    #[test]
    fn examples_and_xtask_tests_are_checked() {
        let root = std::env::temp_dir().join(format!("anchor-reasons-{}", std::process::id()));
        for dir in ["examples", "xtask/tests"] {
            let dir = root.join(dir);
            crate::dump::create_dir(&dir).expect("dir");
            crate::dump::write(
                &dir.join("a.rs"),
                br#"#[expect(clippy::a, reason = "no tag")]"#,
            )
            .expect("source");
        }
        let errors = check(&root);
        remove_dir(&root);
        let errors = errors.expect("check");
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].starts_with("examples/a.rs:1: "), "{errors:?}");
        assert!(errors[1].starts_with("xtask/tests/a.rs:1: "), "{errors:?}");
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: xtask is a development tool"
    )]
    fn remove_dir(path: &Path) {
        std::fs::remove_dir_all(path).expect("cleanup");
    }

    #[test]
    fn every_tag_is_accepted_and_nothing_else() {
        for tag in TAGS {
            assert!(tagged(&format!("{tag}: why\")]")), "{tag}");
        }
        assert!(!tagged("safety why"));
        assert!(!tagged("Safety: why"));
        assert!(!tagged("off-tick:why"));
    }
}
