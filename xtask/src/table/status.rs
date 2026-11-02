//! The status file `xtask/data/dump/status.txt`: rules that give rows without an Anchor alias a
//! status (merged, skipped), rows with one a reason they differ, and known gaps a note (open).

/// What a rule says about the rows its pattern matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Anchor has the row under its alias but behaves differently, for this reason.
    Differs(String),
    /// Anchor does the row's work in the item carrying this (other) alias.
    Merged(String),
    /// Anchor does not have the row, for this reason.
    Skipped(String),
    /// Anchor does not have the row yet and has not decided against it: the row stays blank with
    /// this note (what is missing), and broader rules below do not decide it.
    Open(String),
}

/// One line of the status file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rule {
    /// The line, from 1.
    pub(crate) line: usize,
    /// The FQN pattern: `*` matches within a namespace segment or name, `**` matches anything.
    pub(crate) pattern: String,
    pub(crate) kind: Kind,
}

/// Parses the status file: `<pattern> <status> <target or reason>` per line, the first two
/// fields ended by whitespace (the reason is the rest of the line), `#` comment lines and blank
/// lines ignored.
pub(crate) fn parse(text: &str) -> (Vec<Rule>, Vec<(usize, String)>) {
    let mut rules = Vec::new();
    let mut errors = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line_number = i + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (pattern, rest) = field(line);
        let (status, rest) = field(rest);
        let kind = match status {
            _ if rest.is_empty() => {
                errors.push((
                    line_number,
                    "expected `<pattern> <status> <target or reason>`".to_owned(),
                ));
                continue;
            }
            "differs" => Kind::Differs(rest.to_owned()),
            "merged" if !rest.contains(' ') => Kind::Merged(rest.to_owned()),
            "merged" => {
                errors.push((line_number, "a merge target is one alias".to_owned()));
                continue;
            }
            "skipped" => Kind::Skipped(rest.to_owned()),
            "open" => Kind::Open(rest.to_owned()),
            other => {
                errors.push((
                    line_number,
                    format!("unknown status `{other}` (differs, merged, skipped or open)"),
                ));
                continue;
            }
        };
        rules.push(Rule {
            line: line_number,
            pattern: pattern.to_owned(),
            kind,
        });
    }
    (rules, errors)
}

/// The first whitespace-separated field of `text` and the trimmed rest.
fn field(text: &str) -> (&str, &str) {
    text.split_once(char::is_whitespace)
        .map_or((text, ""), |(first, rest)| (first, rest.trim()))
}

/// Whether `fqn` matches the glob `pattern`: `**` matches any text, `*` any text without `\`
/// or `:`, every other character itself.
pub(crate) fn matches(pattern: &str, fqn: &str) -> bool {
    glob(pattern.as_bytes(), fqn.as_bytes())
}

fn glob(pattern: &[u8], text: &[u8]) -> bool {
    match pattern {
        [] => text.is_empty(),
        [b'*', b'*', rest @ ..] => {
            (0..=text.len()).any(|i| glob(rest, text.get(i..).unwrap_or_default()))
        }
        [b'*', rest @ ..] => {
            let end = text
                .iter()
                .position(|&b| b == b'\\' || b == b':')
                .unwrap_or(text.len());
            (0..=end).any(|i| glob(rest, text.get(i..).unwrap_or_default()))
        }
        [c, rest @ ..] => text
            .split_first()
            .is_some_and(|(first, tail)| first == c && glob(rest, tail)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_within_and_across_segments() {
        let cases = [
            (r"plugin\**", r"plugin\PluginBase", true),
            (r"plugin\**", r"plugin\PluginBase::onEnable", true),
            (r"plugin\**", r"pluginx\A", false),
            (r"block\**::get*", r"block\tile\Chest::getPair", true),
            (r"block\**::get*", r"block\Chest", false),
            (r"*::get*", r"block\Server::getName", false),
            (r"**::get*", r"Server::getName", true),
            (r"block\*", r"block\Chest", true),
            (r"block\*", r"block\tile\Chest", false),
            (r"block\*", r"block\Chest::getPair", false),
            (r"Server", r"Server", true),
            (r"Server", r"ServerX", false),
        ];
        for (pattern, fqn, expected) in cases {
            assert_eq!(matches(pattern, fqn), expected, "{pattern} {fqn}");
        }
    }

    #[test]
    fn parse_reads_rules_and_reports_bad_lines() {
        let text = "# comment\n\nplugin\\** skipped no plugins: a library\n\
                    Player::sendTip merged Player::sendMessage\n\
                    a::b \t differs  rounds  down\nx bogus y\nlonely skipped\nm merged a b\n\
                    Player::getPing open the ping is not in tick state\n";
        let (rules, errors) = parse(text);
        let expected = [
            (
                3,
                r"plugin\**",
                Kind::Skipped("no plugins: a library".to_owned()),
            ),
            (
                4,
                "Player::sendTip",
                Kind::Merged("Player::sendMessage".to_owned()),
            ),
            (5, "a::b", Kind::Differs("rounds  down".to_owned())),
            (
                9,
                "Player::getPing",
                Kind::Open("the ping is not in tick state".to_owned()),
            ),
        ];
        let got: Vec<_> = rules
            .into_iter()
            .map(|r| (r.line, r.pattern, r.kind))
            .collect();
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(l, p, k)| (l, p.to_owned(), k))
            .collect();
        assert_eq!(got, expected);
        let lines: Vec<usize> = errors.iter().map(|(line, _)| *line).collect();
        assert_eq!(lines, [6, 7, 8]);
    }
}
