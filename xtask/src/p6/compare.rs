//! `cargo xtask p6 compare`: compares the spawn window of two bot captures. The window is every
//! packet received before the bot sent `SetLocalPlayerAsInitialised`. Packets in the deferral table
//! are removed from both windows; the remaining name sequences must be equal, and so must the
//! fixed fields of each pair of same-named packets (paired in order). NetherNet captures also hold
//! the server's answer SDP, whose line structure must be equal (see `compare_structure`).

use std::collections::BTreeMap;

use serde_json::Value;

/// The deferral table, relative to the repository root.
pub(crate) const DEFERRED_PATH: &str = "xtask/data/p6/deferred.txt";
/// The fixed-field list, relative to the repository root.
pub(crate) const FIELDS_PATH: &str = "xtask/data/p6/fields.txt";

/// The client packet whose sending ends the spawn window.
const WINDOW_END: &str = "SetLocalPlayerAsInitialised";

/// One received packet of a capture.
#[derive(Debug, PartialEq)]
pub(crate) struct Received {
    pub(crate) name: String,
    pub(crate) fields: Value,
}

/// Packets Anchor does not send yet, by name, with the milestone that adds them.
#[derive(Debug, Default)]
pub(crate) struct Deferred(BTreeMap<String, String>);

/// Fields that must be equal, as field paths by packet name.
#[derive(Debug, Default)]
pub(crate) struct Fields(BTreeMap<String, Vec<Vec<String>>>);

/// Reads the deferral table: one `<packet> <milestone> <reason>` per line, `#` starts a comment.
pub(crate) fn parse_deferred(text: &str) -> Result<Deferred, String> {
    let mut map = BTreeMap::new();
    for (number, line) in table_lines(text) {
        let mut words = line.split_whitespace();
        let (Some(name), Some(milestone), Some(_reason)) =
            (words.next(), words.next(), words.next())
        else {
            return Err(format!(
                "{DEFERRED_PATH}:{number}: want `<packet> <milestone> <reason>`"
            ));
        };
        let valid = milestone
            .strip_prefix('M')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if !valid {
            return Err(format!(
                "{DEFERRED_PATH}:{number}: milestone `{milestone}` is not M<number>"
            ));
        }
        if map.insert(name.to_owned(), milestone.to_owned()).is_some() {
            return Err(format!(
                "{DEFERRED_PATH}:{number}: `{name}` is listed twice"
            ));
        }
    }
    Ok(Deferred(map))
}

/// Reads the fixed-field list: one `<packet> <field>[.<field>...]` per line. A packet listed here
/// must not be deferred.
pub(crate) fn parse_fields(text: &str, deferred: &Deferred) -> Result<Fields, String> {
    let mut map: BTreeMap<String, Vec<Vec<String>>> = BTreeMap::new();
    for (number, line) in table_lines(text) {
        let words: Vec<&str> = line.split_whitespace().collect();
        let [name, path] = words.as_slice() else {
            return Err(format!("{FIELDS_PATH}:{number}: want `<packet> <field>`"));
        };
        if deferred.0.contains_key(*name) {
            return Err(format!(
                "{FIELDS_PATH}:{number}: `{name}` is in the deferral table"
            ));
        }
        let path: Vec<String> = path.split('.').map(str::to_owned).collect();
        if path.iter().any(String::is_empty) {
            return Err(format!("{FIELDS_PATH}:{number}: empty field name"));
        }
        let paths = map.entry((*name).to_owned()).or_default();
        if paths.contains(&path) {
            return Err(format!("{FIELDS_PATH}:{number}: listed twice"));
        }
        paths.push(path);
    }
    Ok(Fields(map))
}

/// Non-empty lines without comments, with their 1-based line numbers.
fn table_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines().enumerate().filter_map(|(i, line)| {
        let line = line.split_once('#').map_or(line, |(code, _)| code).trim();
        (!line.is_empty()).then_some((i + 1, line))
    })
}

/// Reads a capture (the bot's JSON lines) and returns its spawn window.
pub(crate) fn window(capture: &str) -> Result<Vec<Received>, String> {
    let mut received = Vec::new();
    for (i, line) in capture.lines().enumerate() {
        let value: Value =
            serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        match value.get("type").and_then(Value::as_str) {
            Some("packet") => {
                let name = value
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("line {}: packet without a name", i + 1))?;
                if value.get("error").is_some() {
                    return Err(format!("line {}: {name} failed to decode", i + 1));
                }
                received.push(Received {
                    name: name.to_owned(),
                    fields: value.get("fields").cloned().unwrap_or(Value::Null),
                });
            }
            Some("sent") if value.get("name").and_then(Value::as_str) == Some(WINDOW_END) => {
                return Ok(received);
            }
            _ => {}
        }
    }
    Err(format!("the bot never sent {WINDOW_END}"))
}

/// The capture's line of the given type that also has the given field value, if any.
fn find_line(
    capture: &str,
    kind: &str,
    field: Option<(&str, &str)>,
) -> Result<Option<Value>, String> {
    for (i, line) in capture.lines().enumerate() {
        let value: Value =
            serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        let matches = value.get("type").and_then(Value::as_str) == Some(kind)
            && field.is_none_or(|(k, v)| value.get(k).and_then(Value::as_str) == Some(v));
        if matches {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

/// A field of a JSON object, or null.
pub(crate) fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    static NULL: Value = Value::Null;
    value.get(key).unwrap_or(&NULL)
}

/// The bot's final `result` line.
pub(crate) fn result(capture: &str) -> Result<Value, String> {
    find_line(capture, "result", None)?.ok_or_else(|| "the capture has no result line".to_owned())
}

/// The line keys (`a=identity`, `m=application`, ...) of the server's answer SDP, or `None` when
/// the capture has no answer (a RakNet capture).
pub(crate) fn answer_structure(capture: &str) -> Result<Option<Vec<String>>, String> {
    let Some(sdp) = find_line(capture, "sdp", Some(("role", "answer")))? else {
        return Ok(None);
    };
    sdp.get("structure")
        .and_then(Value::as_array)
        .and_then(|keys| keys.iter().map(|k| k.as_str().map(str::to_owned)).collect())
        .map(Some)
        .ok_or_else(|| "the answer SDP line has no structure list".to_owned())
}

/// Compares two answer SDP structures. Runs of the same key count once: a server lists one
/// candidate per interface, and the machines' interfaces are not part of the format.
pub(crate) fn compare_structure(a: &[String], b: &[String]) -> Vec<String> {
    let collapse = |keys: &[String]| {
        let mut out: Vec<String> = keys.to_vec();
        out.dedup();
        out
    };
    let (a, b) = (collapse(a), collapse(b));
    if a == b {
        return Vec::new();
    }
    vec![format!(
        "answer SDP structures differ:\n  A: {}\n  B: {}",
        a.join(" "),
        b.join(" ")
    )]
}

/// The usernames of the players a capture's `PlayerList` packets added, in order.
pub(crate) fn added_players(capture: &str) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for (i, line) in capture.lines().enumerate() {
        let value: Value =
            serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        if value.get("name").and_then(Value::as_str) != Some("PlayerList") {
            continue;
        }
        let entries = value
            .get("fields")
            .and_then(|f| f.get("Entries"))
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        // gophertunnel keeps the action (0 = add) on each entry.
        names.extend(
            entries
                .iter()
                .filter(|e| e.get("ActionType").and_then(Value::as_u64) == Some(0))
                .filter_map(|e| e.get("Username").and_then(Value::as_str))
                .map(str::to_owned),
        );
    }
    Ok(names)
}

/// Compares two windows and returns the differences, or an empty list when they match.
pub(crate) fn compare(
    a: &[Received],
    b: &[Received],
    deferred: &Deferred,
    fields: &Fields,
) -> Vec<String> {
    let (a, b) = (kept(a, deferred), kept(b, deferred));
    let mut differences = Vec::new();
    let names_a: Vec<&str> = a.iter().map(|p| p.name.as_str()).collect();
    let names_b: Vec<&str> = b.iter().map(|p| p.name.as_str()).collect();
    if names_a != names_b {
        let at = names_a
            .iter()
            .zip(&names_b)
            .take_while(|(x, y)| x == y)
            .count();
        differences.push(format!(
            "packet sequences differ at position {at}:\n  A: {}\n  B: {}",
            names_a.join(" "),
            names_b.join(" ")
        ));
    }
    for (name, paths) in &fields.0 {
        let pick_a = a.iter().filter(|p| &p.name == name).map(|p| &p.fields);
        let pick_b = b.iter().filter(|p| &p.name == name).map(|p| &p.fields);
        for (n, (x, y)) in pick_a.zip(pick_b).enumerate() {
            for path in paths {
                let (vx, vy) = (lookup(x, path), lookup(y, path));
                if vx != vy {
                    differences.push(format!(
                        "{name}#{n}.{}: A {} != B {}",
                        path.join("."),
                        show(vx),
                        show(vy)
                    ));
                }
            }
        }
    }
    differences
}

/// The packets of a window that are not deferred.
fn kept<'a>(window: &'a [Received], deferred: &Deferred) -> Vec<&'a Received> {
    window
        .iter()
        .filter(|p| !deferred.0.contains_key(&p.name))
        .collect()
}

fn lookup<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(value, |v, key| v.get(key))
}

fn show(value: Option<&Value>) -> String {
    value.map_or_else(|| "(missing)".to_owned(), Value::to_string)
}

/// The packet names of a window in order, without deferred packets and with runs of the same
/// name shown once with a count.
pub(crate) fn sequence(window: &[Received], deferred: &Deferred) -> Vec<String> {
    let mut out: Vec<(String, usize)> = Vec::new();
    for p in kept(window, deferred) {
        match out.last_mut() {
            Some((name, count)) if *name == p.name => *count += 1,
            _ => out.push((p.name.clone(), 1)),
        }
    }
    out.into_iter()
        .map(|(name, count)| {
            if count == 1 {
                name
            } else {
                format!("{name} x{count}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPTURE: &str = r#"{"type":"raknet"}
{"type":"sent","name":"RequestNetworkSettings","after":-1}
{"type":"packet","index":0,"name":"NetworkSettings","fields":{"CompressionThreshold":256}}
{"type":"packet","index":1,"name":"LevelChunk","fields":{}}
{"type":"packet","index":2,"name":"StartGame","fields":{"GameVersion":"1.26.52","Seed":1,"Movement":{"Rewind":40}}}
{"type":"sent","name":"SetLocalPlayerAsInitialised","after":2}
{"type":"packet","index":3,"name":"Text","fields":{}}
"#;

    fn tables(deferred: &str, fields: &str) -> (Deferred, Fields) {
        let deferred = parse_deferred(deferred).expect("deferral table");
        let fields = parse_fields(fields, &deferred).expect("field list");
        (deferred, fields)
    }

    #[test]
    fn window_ends_where_the_bot_initialised() {
        let names: Vec<String> = window(CAPTURE)
            .expect("window")
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["NetworkSettings", "LevelChunk", "StartGame"]);
    }

    #[test]
    fn window_needs_the_spawn_and_decoded_packets() {
        assert!(window("{\"type\":\"packet\",\"name\":\"A\"}\n").is_err());
        let bad = "{\"type\":\"packet\",\"name\":\"A\",\"error\":\"x\"}\n{\"type\":\"sent\",\"name\":\"SetLocalPlayerAsInitialised\"}\n";
        assert!(window(bad).is_err());
    }

    #[test]
    fn deferred_packets_and_unlisted_fields_are_ignored() {
        let (deferred, fields) = tables(
            "LevelChunk M6 chunks\n",
            "StartGame GameVersion\nStartGame Movement.Rewind\n",
        );
        let a = window(CAPTURE).expect("window");
        let other = CAPTURE.replace("\"Seed\":1", "\"Seed\":2");
        let mut b = window(&other).expect("window");
        b.remove(1);
        assert_eq!(compare(&a, &b, &deferred, &fields), Vec::<String>::new());
    }

    #[test]
    fn sequence_and_field_differences_are_reported() {
        let (deferred, fields) = tables("", "StartGame Movement.Rewind\n");
        let a = window(CAPTURE).expect("window");
        let other = CAPTURE.replace("\"Rewind\":40", "\"Rewind\":20");
        let mut b = window(&other).expect("window");
        b.remove(1);
        let differences = compare(&a, &b, &deferred, &fields);
        assert_eq!(differences.len(), 2, "{differences:?}");
        assert!(differences[0].starts_with("packet sequences differ at position 1"));
        assert_eq!(differences[1], "StartGame#0.Movement.Rewind: A 40 != B 20");
    }

    #[test]
    fn missing_fields_differ_from_present_ones() {
        let (deferred, fields) = tables("", "StartGame Absent\nStartGame GameVersion\n");
        let a = window(CAPTURE).expect("window");
        let other = CAPTURE.replace("\"GameVersion\"", "\"Version\"");
        let b = window(&other).expect("window");
        assert_eq!(
            compare(&a, &b, &deferred, &fields),
            ["StartGame#0.GameVersion: A \"1.26.52\" != B (missing)"]
        );
    }

    #[test]
    fn tables_reject_malformed_lines() {
        assert!(parse_deferred("LevelChunk M6\n").is_err());
        assert!(parse_deferred("LevelChunk 6 chunks\n").is_err());
        assert!(parse_deferred("A M4 x\nA M6 y\n").is_err());
        let deferred =
            parse_deferred("# comment\nLevelChunk M6 chunks # trailing\n").expect("deferral table");
        assert!(parse_fields("LevelChunk Position\n", &deferred).is_err());
        assert!(parse_fields("StartGame\n", &deferred).is_err());
        assert!(parse_fields("StartGame a..b\n", &deferred).is_err());
        assert!(parse_fields("StartGame A\nStartGame A\n", &deferred).is_err());
    }

    #[test]
    fn answer_structures_compare_without_candidate_counts() {
        let capture = r#"{"type":"sdp","role":"offer","structure":["v"]}
{"type":"sdp","role":"answer","structure":["v","a=identity","m=application","a=candidate","a=candidate","a=setup"]}
{"type":"result","transport":"nethernet","pass":true}
"#;
        let a = answer_structure(capture).expect("capture").expect("answer");
        assert_eq!(a.len(), 6);
        let one: Vec<String> = ["v", "a=identity", "m=application", "a=candidate", "a=setup"]
            .map(str::to_owned)
            .into();
        assert!(compare_structure(&a, &one).is_empty());
        let moved: Vec<String> = ["v", "m=application", "a=identity", "a=candidate", "a=setup"]
            .map(str::to_owned)
            .into();
        assert_eq!(compare_structure(&a, &moved).len(), 1);
        assert_eq!(answer_structure(CAPTURE).expect("capture"), None);
        assert_eq!(result(capture).expect("result")["transport"], "nethernet");
        assert!(result(CAPTURE).is_err());
    }

    #[test]
    fn added_players_come_from_player_list_additions() {
        let capture = r#"{"type":"packet","name":"PlayerList","fields":{"Entries":[{"ActionType":0,"Username":"Dup"}]}}
{"type":"packet","name":"PlayerList","fields":{"Entries":[{"ActionType":1,"Username":""}]}}
{"type":"packet","name":"PlayerList","fields":{"Entries":[{"ActionType":0,"Username":"Dup(2)"}]}}
"#;
        assert_eq!(added_players(capture).expect("capture"), ["Dup", "Dup(2)"]);
    }

    #[test]
    fn sequence_counts_runs_without_deferred_packets() {
        let (deferred, _) = tables("LevelChunk M6 chunks\n", "");
        let w = window(&CAPTURE.replace("StartGame", "NetworkSettings")).expect("window");
        assert_eq!(sequence(&w, &deferred), ["NetworkSettings x2"]);
    }
}
