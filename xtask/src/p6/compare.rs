//! `cargo xtask p6 compare`: compares the spawn window of two bot captures. The window is every
//! packet received before the bot sent `SetLocalPlayerAsInitialised`. Packets in the deferral table
//! are removed from both windows; the remaining name sequences must be equal, and so must the
//! fixed fields of each pair of same-named packets (paired in order). NetherNet captures also hold
//! the server's answer SDP, whose line structure must be equal (see `compare_structure`). The item
//! packets the bot writes in full (`<capture>.items.jsonl`) are compared by `compare_items`, the
//! chunk packets of the whole captures by `compare_chunks` and the steps of two blocks captures by
//! `compare_steps`.

use std::collections::{BTreeMap, BTreeSet};

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

/// Packets left out of the spawn window comparison, by name, with their status (`-`: Anchor never
/// sends the packet).
#[derive(Debug, Default)]
pub(crate) struct Deferred(BTreeMap<String, String>);

impl Deferred {
    /// Whether the table holds the packet.
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }
}

/// Fields that must be equal, as field paths by packet name.
#[derive(Debug, Default)]
pub(crate) struct Fields(BTreeMap<String, Vec<Vec<String>>>);

impl Fields {
    /// The fields of the named packets only.
    pub(crate) fn only(&self, names: &[&str]) -> Self {
        Self(
            self.0
                .iter()
                .filter(|(name, _)| names.contains(&name.as_str()))
                .map(|(name, paths)| (name.clone(), paths.clone()))
                .collect(),
        )
    }
}

/// Reads the deferral table: one `<packet> <status> <reason>` per line, `#` starts a comment.
pub(crate) fn parse_deferred(text: &str) -> Result<Deferred, String> {
    let mut map = BTreeMap::new();
    for (number, line) in table_lines(text) {
        let mut words = line.split_whitespace();
        let (Some(name), Some(status), Some(_reason)) = (words.next(), words.next(), words.next())
        else {
            return Err(format!(
                "{DEFERRED_PATH}:{number}: want `<packet> <status> <reason>`"
            ));
        };
        // `-`: Anchor never sends the packet (the reason says why).
        if status != "-" {
            return Err(format!(
                "{DEFERRED_PATH}:{number}: status `{status}` is not -"
            ));
        }
        if map.insert(name.to_owned(), status.to_owned()).is_some() {
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

/// Compares the packets of two lists whose names the fixed-field list holds: the same number of
/// each, and their fixed fields paired in order (`p6 api` compares each step this way). Returns
/// the differences.
pub(crate) fn compare_listed(a: &[Received], b: &[Received], fields: &Fields) -> Vec<String> {
    let mut differences = Vec::new();
    for (name, paths) in &fields.0 {
        let pick_a: Vec<&Value> = a
            .iter()
            .filter(|p| &p.name == name)
            .map(|p| &p.fields)
            .collect();
        let pick_b: Vec<&Value> = b
            .iter()
            .filter(|p| &p.name == name)
            .map(|p| &p.fields)
            .collect();
        if pick_a.len() != pick_b.len() {
            differences.push(format!("{name}: A {} != B {}", pick_a.len(), pick_b.len()));
        }
        for (n, (x, y)) in pick_a.iter().zip(&pick_b).enumerate() {
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

/// The chunk packets: they interleave with the spawn window by timing (a server sends them as
/// chunks load, BDS from before the radius answer), so the window leaves them out and
/// [`compare_chunks`] compares them by their own rules.
const CHUNK_PACKETS: [&str; 3] = ["LevelChunk", "SubChunk", "NetworkChunkPublisherUpdate"];

/// Entity packets that also carry other entities' data: BDS sends them for the flat world's
/// mobs that come into view during the spawn window (P6, `xtask/data/p6/facts.md`), Anchor never
/// (it simulates no mobs). Only the player's own are compared.
const ENTITY_DATA: [&str; 2] = ["SetActorData", "UpdateAttributes"];

/// The packets of a window that are neither deferred nor chunk packets, nor entity data of
/// another entity than the player (`ENTITY_DATA`).
fn kept<'a>(window: &'a [Received], deferred: &Deferred) -> Vec<&'a Received> {
    let player = window
        .iter()
        .find(|p| p.name == "StartGame")
        .and_then(|p| field(&p.fields, "EntityRuntimeID").as_u64());
    let other_entity = |p: &Received| {
        ENTITY_DATA.contains(&p.name.as_str())
            && field(&p.fields, "EntityRuntimeID").as_u64() != player
    };
    window
        .iter()
        .filter(|p| {
            !deferred.0.contains_key(&p.name)
                && !CHUNK_PACKETS.contains(&p.name.as_str())
                && !other_entity(p)
        })
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

/// The full item packets of a capture (the bot's `<capture>.items.jsonl`): the first
/// `ItemRegistry`, `CreativeContent` and `CraftingData`, each as its fields.
#[derive(Debug, Default)]
pub(crate) struct Items {
    registry: Option<Value>,
    creative: Option<Value>,
    crafting: Option<Value>,
}

/// Reads a full item packet file.
pub(crate) fn parse_items(text: &str) -> Result<Items, String> {
    let mut items = Items::default();
    for (i, line) in text.lines().enumerate() {
        let value: Value =
            serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        let slot = match field(&value, "name").as_str() {
            Some("ItemRegistry") => &mut items.registry,
            Some("CreativeContent") => &mut items.creative,
            Some("CraftingData") => &mut items.crafting,
            _ => return Err(format!("line {}: not an item packet", i + 1)),
        };
        if slot.is_none() {
            *slot = Some(field(&value, "fields").clone());
        }
    }
    Ok(items)
}

/// How many entries of one kind a difference list shows before it only counts.
const SHOWN: usize = 10;

/// Compares the item packets of capture A (BDS) and B (Anchor) where both have them. Returns the
/// differences, which fail the comparison, and notes, which are information:
/// - `ItemRegistry`: the `Items` lists must hold the same entries (`compare_registry`).
/// - `CreativeContent`: every item of B must be in A with the same `NetworkID`, `MetadataValue`,
///   `BlockRuntimeID` and `NBTData`; B's groups must be consistent (`group_problems`).
/// - `CraftingData`: every item stack (`NetworkID`, `MetadataValue`, `BlockRuntimeID`) and every
///   ingredient descriptor of B must be one A uses somewhere in its creative or crafting data
///   (`Representations`). Recipe content differences are notes.
pub(crate) fn compare_items(a: &Items, b: &Items) -> (Vec<String>, Vec<String>) {
    let mut differences = Vec::new();
    let mut notes = Vec::new();
    if let (Some(x), Some(y)) = (&a.registry, &b.registry) {
        differences.extend(compare_registry(field(x, "Items"), field(y, "Items")));
    }
    if let (Some(x), Some(y)) = (&a.creative, &b.creative) {
        let known: BTreeSet<String> = array(x, "Items")
            .iter()
            .map(|item| creative_key(field(item, "Item")))
            .collect();
        let missing: Vec<String> = array(y, "Items")
            .iter()
            .map(|item| creative_key(field(item, "Item")))
            .filter(|key| !known.contains(key))
            .collect();
        // An item A has in its creative inventory must have A's form; an item A leaves out
        // (education items BDS hides) is content, not a form difference.
        let (wrong, absent) = split_by_item(&missing, &known);
        differences.extend(listed(
            "CreativeContent items of B whose item A has in another form (NetworkID, \
             MetadataValue, BlockRuntimeID, NBT)",
            &wrong,
        ));
        notes.extend(listed(
            "CreativeContent items of B that A does not list",
            &absent,
        ));
        differences.extend(
            group_problems(y)
                .into_iter()
                .map(|p| format!("CreativeContent of B: {p}")),
        );
    }
    if let (Some(x), Some(y)) = (&a.crafting, &b.crafting) {
        let used = Representations::of(a);
        let mut missing = BTreeSet::new();
        Representations::walk(y, &mut |kind, key| {
            if !used.has(kind, &key) {
                missing.insert(format!("{kind} {key}"));
            }
        });
        let stacks: BTreeSet<String> = used
            .0
            .iter()
            .filter(|(kind, _)| *kind == "stack")
            .map(|(kind, key)| format!("{kind} {key}"))
            .collect();
        // A stack of an item A never has in a stack is recipe content (an item BDS uses in no
        // recipe or creative entry), not a form difference.
        let (stack_keys, others): (Vec<String>, Vec<String>) =
            missing.into_iter().partition(|k| k.starts_with("stack "));
        let (mut wrong, absent) = split_by_item(&stack_keys, &stacks);
        wrong.extend(others);
        wrong.sort();
        differences.extend(listed(
            "CraftingData item representations of B that A never uses",
            &wrong,
        ));
        notes.extend(listed(
            "CraftingData item representations of B for items A never uses",
            &absent,
        ));
        notes.extend(recipe_notes(x, y));
    }
    (differences, notes)
}

/// Splits keys (`[kind ]NetworkID:...`) into those whose item (the part before the first `:`)
/// some key of `known` has, and the others.
fn split_by_item(keys: &[String], known: &BTreeSet<String>) -> (Vec<String>, Vec<String>) {
    let item = |key: &str| key.split(':').next().unwrap_or_default().to_owned();
    let items: BTreeSet<String> = known.iter().map(|k| item(k)).collect();
    keys.iter().cloned().partition(|k| items.contains(&item(k)))
}

/// The array at `key` of a JSON object, or an empty slice.
fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    field(value, key).as_array().map_or(&[], Vec::as_slice)
}

/// One difference line for a non-empty list, showing at most `SHOWN` entries.
fn listed(what: &str, entries: &[String]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let shown: Vec<&str> = entries.iter().take(SHOWN).map(String::as_str).collect();
    let more = entries.len().saturating_sub(SHOWN);
    Some(format!(
        "{what}: {}:\n  {}{}",
        entries.len(),
        shown.join("\n  "),
        if more > 0 {
            format!("\n  ... {more} more")
        } else {
            String::new()
        }
    ))
}

/// Compares two `ItemRegistry` item lists in runtime ID order: BDS lists them in an order that is
/// the same on every start but neither by runtime ID nor by name, and the client looks entries up
/// by runtime ID. Reports the names only one side has and the first differing entry.
fn compare_registry(a: &Value, b: &Value) -> Vec<String> {
    let (Some(a), Some(b)) = (a.as_array(), b.as_array()) else {
        return vec!["ItemRegistry Items is not a list".to_owned()];
    };
    let (a, b) = (by_runtime_id(a), by_runtime_id(b));
    if a == b {
        return Vec::new();
    }
    let name = |e: &&Value| field(e, "Name").as_str().unwrap_or_default().to_owned();
    let names_a: BTreeSet<String> = a.iter().map(name).collect();
    let names_b: BTreeSet<String> = b.iter().map(name).collect();
    let only = |x: &BTreeSet<String>, y: &BTreeSet<String>| -> Vec<String> {
        x.difference(y).cloned().collect()
    };
    let mut differences = vec![format!(
        "ItemRegistry Items differ: A {} entries, B {}",
        a.len(),
        b.len()
    )];
    differences.extend(listed(
        "ItemRegistry names only in A",
        &only(&names_a, &names_b),
    ));
    differences.extend(listed(
        "ItemRegistry names only in B",
        &only(&names_b, &names_a),
    ));
    if let Some((x, y)) = a.iter().zip(&b).find(|(x, y)| x != y) {
        differences.push(format!(
            "ItemRegistry first different entry by runtime ID: A {} != B {}",
            short(x),
            short(y)
        ));
    }
    differences
}

/// `ItemRegistry` entries sorted by runtime ID (then by content, for duplicate IDs).
fn by_runtime_id(list: &[Value]) -> Vec<&Value> {
    let mut list: Vec<&Value> = list.iter().collect();
    list.sort_by_cached_key(|e| (field(e, "RuntimeID").as_i64(), e.to_string()));
    list
}

/// A JSON value cut to 300 characters, for messages.
fn short(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() <= 300 {
        return text;
    }
    let mut cut: String = text.chars().take(300).collect();
    cut.push_str("...");
    cut
}

/// The identity of a creative item: `NetworkID`, `MetadataValue`, `BlockRuntimeID` and NBT.
fn creative_key(stack: &Value) -> String {
    format!(
        "{} {}",
        stack_key(stack).unwrap_or_default(),
        field(stack, "NBTData")
    )
}

/// `NetworkID:MetadataValue:BlockRuntimeID` of an item stack (gophertunnel's `ItemStack`, whose
/// embedded `ItemType` holds the first two), or `None` for other objects.
fn stack_key(stack: &Value) -> Option<String> {
    let item = stack.get("ItemType")?;
    Some(format!(
        "{}:{}:{}",
        item.get("NetworkID")?,
        item.get("MetadataValue")?,
        stack.get("BlockRuntimeID")?
    ))
}

/// What is wrong with a `CreativeContent`'s groups: an item's group index out of range, item group
/// indexes that go back (items are listed group by group), a group without items, or creative
/// item network IDs that are not 1, 2, 3, ... in order.
fn group_problems(creative: &Value) -> Vec<String> {
    let groups = array(creative, "Groups").len();
    let mut problems = Vec::new();
    let mut used = vec![false; groups];
    let mut last = 0;
    for (i, item) in array(creative, "Items").iter().enumerate() {
        let index = field(item, "GroupIndex").as_u64();
        match index.and_then(|g| usize::try_from(g).ok()) {
            Some(g) if g < groups => {
                if let Some(slot) = used.get_mut(g) {
                    *slot = true;
                }
                if g < last {
                    problems.push(format!(
                        "item {i} goes back to group {g} after group {last}"
                    ));
                }
                last = g;
            }
            _ => problems.push(format!(
                "item {i} has group index {} of {groups} groups",
                field(item, "GroupIndex")
            )),
        }
        let id = field(item, "CreativeItemNetworkID").as_u64();
        if id != u64::try_from(i + 1).ok() {
            problems.push(format!(
                "item {i} has creative network ID {}, want {}",
                field(item, "CreativeItemNetworkID"),
                i + 1
            ));
        }
    }
    problems.extend(
        used.iter()
            .enumerate()
            .filter(|(_, u)| !**u)
            .map(|(g, _)| format!("group {g} has no items")),
    );
    problems.truncate(SHOWN);
    problems
}

/// The item representations a capture uses in its creative and crafting data, by kind: `stack`
/// (`NetworkID:MetadataValue:BlockRuntimeID` of an item stack), `item` (`Name:MetadataValue` of a
/// name descriptor), `tag`, `molang`, `potion` (`ID:metadata` of a potion recipe item) and
/// `potion-id` (an item ID of a potion container change recipe). A name descriptor, potion item
/// or potion ID also matches any stack of the same item (and metadata); names come from the
/// capture's `ItemRegistry`.
#[derive(Debug, Default)]
struct Representations(BTreeSet<(&'static str, String)>);

impl Representations {
    fn of(items: &Items) -> Self {
        let mut set = BTreeSet::new();
        let mut add = |kind: &'static str, key: String| {
            set.insert((kind, key));
        };
        for value in [&items.creative, &items.crafting].into_iter().flatten() {
            Self::walk(value, &mut add);
        }
        // Stacks also stand for name descriptors and potion items of the same item and metadata.
        let names: BTreeMap<i64, &str> = items
            .registry
            .as_ref()
            .map(|r| array(r, "Items"))
            .unwrap_or_default()
            .iter()
            .filter_map(|e| Some((field(e, "RuntimeID").as_i64()?, field(e, "Name").as_str()?)))
            .collect();
        let stacks: Vec<(i64, String)> = set
            .iter()
            .filter(|(kind, _)| *kind == "stack")
            .filter_map(|(_, key)| {
                let mut parts = key.split(':');
                let id = parts.next()?.parse().ok()?;
                Some((id, parts.next()?.to_owned()))
            })
            .collect();
        for (id, meta) in stacks {
            if let Some(name) = names.get(&id) {
                set.insert(("item", format!("{name}:{meta}")));
            }
            set.insert(("potion", format!("{id}:{meta}")));
            set.insert(("potion-id", id.to_string()));
        }
        Self(set)
    }

    fn has(&self, kind: &'static str, key: &str) -> bool {
        self.0.contains(&(kind, key.to_owned()))
    }

    /// Calls `f` with every item representation in a JSON tree: objects with `ItemType` and
    /// `BlockRuntimeID` are stacks, `Descriptor` objects are ingredient descriptors,
    /// `PotionRecipes` entries give their three items and `PotionContainerChangeRecipes` entries
    /// their three item IDs.
    fn walk(value: &Value, f: &mut impl FnMut(&'static str, String)) {
        match value {
            Value::Array(list) => list.iter().for_each(|v| Self::walk(v, f)),
            Value::Object(map) => {
                // Air (network ID 0) is an empty slot, not an item.
                if let Some(key) = stack_key(value).filter(|k| !k.starts_with("0:")) {
                    f("stack", key);
                }
                if let Some(descriptor) = map.get("Descriptor") {
                    Self::descriptor(descriptor, f);
                }
                for recipe in map
                    .get("PotionRecipes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    for (id, meta) in [
                        ("InputPotionID", "InputPotionMetadata"),
                        ("ReagentItemID", "ReagentItemMetadata"),
                        ("OutputPotionID", "OutputPotionMetadata"),
                    ] {
                        f(
                            "potion",
                            format!("{}:{}", field(recipe, id), field(recipe, meta)),
                        );
                    }
                }
                let changes = map
                    .get("PotionContainerChangeRecipes")
                    .and_then(Value::as_array);
                for recipe in changes.into_iter().flatten() {
                    for id in ["InputItemID", "ReagentItemID", "OutputItemID"] {
                        f("potion-id", field(recipe, id).to_string());
                    }
                }
                for (key, v) in map {
                    if !matches!(
                        key.as_str(),
                        "Descriptor" | "PotionRecipes" | "PotionContainerChangeRecipes"
                    ) {
                        Self::walk(v, f);
                    }
                }
            }
            _ => {}
        }
    }

    /// Calls `f` with one ingredient descriptor; the invalid (empty) descriptor has no fields.
    fn descriptor(descriptor: &Value, f: &mut impl FnMut(&'static str, String)) {
        if let Some(name) = field(descriptor, "Name").as_str() {
            f(
                "item",
                format!("{name}:{}", field(descriptor, "MetadataValue")),
            );
        } else if let Some(tag) = field(descriptor, "Tag").as_str() {
            f("tag", tag.to_owned());
        } else if let Some(expression) = field(descriptor, "Expression").as_str() {
            f(
                "molang",
                format!("{expression} v{}", field(descriptor, "Version")),
            );
        }
    }
}

/// Notes on recipe content: per recipe list of `CraftingData`, how many recipes only one side has,
/// compared without their network IDs and UUIDs (BDS makes new recipe UUIDs on every start; only
/// `MultiRecipes` keep theirs, which name the recipe kind), and the first few recipe IDs of each.
fn recipe_notes(a: &Value, b: &Value) -> Vec<String> {
    let lists: BTreeSet<&String> = a
        .as_object()
        .into_iter()
        .chain(b.as_object())
        .flat_map(|m| m.iter())
        .filter(|(_, v)| v.is_array())
        .map(|(k, _)| k)
        .collect();
    let mut notes = Vec::new();
    for list in lists {
        let content = |v: &Value| -> BTreeMap<String, String> {
            array(v, list)
                .iter()
                .map(|recipe| {
                    let mut recipe = recipe.clone();
                    if let Some(map) = recipe.as_object_mut() {
                        map.remove("RecipeNetworkID");
                        if list != "MultiRecipes" {
                            map.remove("UUID");
                        }
                    }
                    let id = field(&recipe, "RecipeID")
                        .as_str()
                        .map_or_else(|| short(&recipe), str::to_owned);
                    (recipe.to_string(), id)
                })
                .collect()
        };
        let (x, y) = (content(a), content(b));
        let only = |p: &BTreeMap<String, String>, q: &BTreeMap<String, String>| -> Vec<String> {
            p.iter()
                .filter(|(k, _)| !q.contains_key(*k))
                .map(|(_, id)| id.clone())
                .collect()
        };
        let (only_a, only_b) = (only(&x, &y), only(&y, &x));
        if only_a.is_empty() && only_b.is_empty() {
            continue;
        }
        notes.push(format!(
            "CraftingData {list}: A {}, B {}, only in A {}, only in B {}",
            array(a, list).len(),
            array(b, list).len(),
            only_a.len(),
            only_b.len()
        ));
        let first = |ids: &[String]| {
            ids.iter()
                .take(SHOWN)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        };
        if !only_a.is_empty() {
            notes.push(format!("  only in A: {}", first(&only_a)));
        }
        if !only_b.is_empty() {
            notes.push(format!("  only in B: {}", first(&only_b)));
        }
    }
    notes
}

/// The chunk packets of a capture, made independent of where the player spawned: chunk positions
/// are relative to the spawn chunk (the player position of `StartGame`) and publisher positions relative
/// to the player's block column.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Chunks {
    /// Each `LevelChunk` in order: its relative position and its fields without the position.
    pub(crate) level: Vec<([i64; 2], String)>,
    /// How many `LevelChunk`s came before `PlayStatus(PlayerSpawn)`.
    pub(crate) before_spawn: usize,
    /// The `SubChunk` entries by relative request centre and offset: the entry's fields without the
    /// offset, the packet's `CacheEnabled` and `Dimension`, and the bot's strict payload check
    /// (`strict` and each layer's `content_sha256`). A key answered twice has two values.
    pub(crate) sub: BTreeMap<([i64; 3], [i64; 3]), Vec<String>>,
    /// Payloads that broke Anchor's strict sub-chunk rules: key and reason.
    pub(crate) not_strict: Vec<String>,
    /// Each `NetworkChunkPublisherUpdate`: relative x and z, radius, saved chunk count, and whether
    /// it came after `ChunkRadiusUpdated`.
    pub(crate) publisher: Vec<([i64; 2], u64, usize, bool)>,
    /// The radius of the first `ChunkRadiusUpdated`.
    pub(crate) radius: Option<u64>,
}

/// Reads the chunk packets of a capture (the whole capture, not only the spawn window).
pub(crate) fn chunks(capture: &str) -> Result<Chunks, String> {
    let mut reader = ChunkReader::default();
    for (i, line) in capture.lines().enumerate() {
        let value: Value =
            serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        reader
            .line(&value)
            .map_err(|e| format!("line {}: {e}", i + 1))?;
    }
    reader.flush(None);
    Ok(reader.out)
}

/// The entries of one `SubChunk` packet, waiting for the bot's check line of it.
#[derive(Debug)]
struct SubChunkPacket {
    index: u64,
    centre: [i64; 3],
    /// Each entry's offset and its other fields.
    entries: Vec<(Value, Value)>,
}

/// The state of `chunks` between capture lines.
#[derive(Debug, Default)]
struct ChunkReader {
    out: Chunks,
    /// The spawn chunk and the player's block column (x, z).
    origin: Option<([i64; 2], [i64; 2])>,
    spawned: bool,
    pending: Option<SubChunkPacket>,
}

impl ChunkReader {
    fn line(&mut self, value: &Value) -> Result<(), String> {
        let fields = field(value, "fields");
        let kind = field(value, "type").as_str();
        let name = field(value, "name").as_str().unwrap_or_default();
        if kind == Some("subchunk") {
            self.flush(Some(value));
            return Ok(());
        }
        if kind != Some("packet") {
            return Ok(());
        }
        match name {
            "StartGame" => {
                let position = field(fields, "PlayerPosition");
                let block = |i: usize| {
                    position
                        .get(i)
                        .and_then(Value::as_f64)
                        .map(f64::floor)
                        .filter(|v| v.abs() < 1e9)
                        .ok_or_else(|| format!("StartGame PlayerPosition {position}"))
                };
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "safety: whole numbers below 1e9 in size"
                )]
                let column = [block(0)? as i64, block(2)? as i64];
                self.origin = Some((column.map(|c| c.div_euclid(16)), column));
            }
            "LevelChunk" | "SubChunk" | "NetworkChunkPublisherUpdate" => {
                let (chunk, column) = self
                    .origin
                    .ok_or_else(|| format!("{name} before StartGame"))?;
                let position = field(fields, "Position");
                match name {
                    "LevelChunk" => {
                        let at = [at(position, 0)? - chunk[0], at(position, 1)? - chunk[1]];
                        let mut rest = fields.clone();
                        if let Some(map) = rest.as_object_mut() {
                            map.remove("Position");
                        }
                        self.out.level.push((at, rest.to_string()));
                        if !self.spawned {
                            self.out.before_spawn += 1;
                        }
                    }
                    "SubChunk" => {
                        let centre = [
                            at(position, 0)? - chunk[0],
                            at(position, 1)?,
                            at(position, 2)? - chunk[1],
                        ];
                        self.flush(None);
                        self.pending = Some(SubChunkPacket {
                            index: field(value, "index").as_u64().unwrap_or(u64::MAX),
                            centre,
                            entries: sub_chunk_entries(fields),
                        });
                    }
                    _ => self.out.publisher.push((
                        [at(position, 0)? - column[0], at(position, 2)? - column[1]],
                        field(fields, "Radius").as_u64().unwrap_or(u64::MAX),
                        array(fields, "SavedChunks").len(),
                        self.out.radius.is_some(),
                    )),
                }
            }
            "ChunkRadiusUpdated" => {
                self.out.radius = self
                    .out
                    .radius
                    .or_else(|| field(fields, "ChunkRadius").as_u64());
            }
            // 3 is PlayerSpawn.
            "PlayStatus" => self.spawned |= field(fields, "Status").as_u64() == Some(3),
            _ => {}
        }
        Ok(())
    }

    /// Stores the pending `SubChunk` packet with the bot's check line of it, if `check` is one.
    fn flush(&mut self, check: Option<&Value>) {
        let Some(packet) = self.pending.take() else {
            return;
        };
        let checks = check
            .filter(|c| field(c, "index").as_u64() == Some(packet.index))
            .map_or(&[][..], |c| array(c, "entries"));
        for (n, (offset, mut entry)) in packet.entries.into_iter().enumerate() {
            let offset =
                [0, 1, 2].map(|i| offset.get(i).and_then(Value::as_i64).unwrap_or(i64::MAX));
            let payload = checks
                .get(n)
                .map(|c| field(c, "payload"))
                .filter(|p| !p.is_null());
            if let (Some(payload), Some(map)) = (payload, entry.as_object_mut()) {
                let strict = field(payload, "strict").clone();
                if strict.as_str() != Some("ok") {
                    self.out
                        .not_strict
                        .push(format!("{:?} {offset:?}: {strict}", packet.centre));
                }
                let content: Vec<Value> = array(payload, "layers")
                    .iter()
                    .map(|l| field(l, "content_sha256").clone())
                    .collect();
                map.insert("strict".to_owned(), strict);
                map.insert("content".to_owned(), Value::Array(content));
            }
            self.out
                .sub
                .entry((packet.centre, offset))
                .or_default()
                .push(entry.to_string());
        }
    }
}

/// Coordinate `i` of a JSON position.
fn at(position: &Value, i: usize) -> Result<i64, String> {
    position
        .get(i)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("{position} is not a position"))
}

/// The entries of a `SubChunk` packet's fields: each entry's offset and its other fields, with the
/// packet's `CacheEnabled` and `Dimension` added.
fn sub_chunk_entries(fields: &Value) -> Vec<(Value, Value)> {
    array(fields, "SubChunkEntries")
        .iter()
        .map(|e| {
            let mut e = e.clone();
            let offset = e
                .as_object_mut()
                .and_then(|m| m.remove("Offset"))
                .unwrap_or_default();
            if let Some(m) = e.as_object_mut() {
                for key in ["CacheEnabled", "Dimension"] {
                    m.insert(key.to_owned(), field(fields, key).clone());
                }
            }
            (offset, e)
        })
        .collect()
}

/// Compares the chunk packets of A (BDS) and B. Differences:
/// - the distinct `LevelChunk` field sets (without the position) differ: the same flat world must
///   give the same chunks;
/// - a `SubChunk` answer (by relative centre and offset) differs or only one side has it: result,
///   height maps, payload hash and length, strict check and layer contents;
/// - B's payloads break Anchor's strict sub-chunk rules;
/// - B sent its chunks before spawning not nearest first, or a chunk twice;
/// - the publisher updates after `ChunkRadiusUpdated`, runs of equal values counted once, differ.
///
/// The counts and the publisher updates before `ChunkRadiusUpdated` are notes.
pub(crate) fn compare_chunks(a: &Chunks, b: &Chunks) -> (Vec<String>, Vec<String>) {
    let mut differences = Vec::new();
    let mut notes = Vec::new();
    let shapes =
        |c: &Chunks| -> BTreeSet<String> { c.level.iter().map(|(_, s)| s.clone()).collect() };
    let (shapes_a, shapes_b) = (shapes(a), shapes(b));
    if shapes_a != shapes_b {
        differences.push(format!(
            "LevelChunk fields differ:\n  A: {}\n  B: {}",
            shapes_a.into_iter().collect::<Vec<_>>().join(" | "),
            shapes_b.into_iter().collect::<Vec<_>>().join(" | ")
        ));
    }
    let keys: BTreeSet<&([i64; 3], [i64; 3])> = a.sub.keys().chain(b.sub.keys()).collect();
    let mut shown = 0;
    for key in keys {
        let (x, y) = (a.sub.get(key), b.sub.get(key));
        if x != y {
            shown += 1;
            if shown <= SHOWN {
                let show = |v: Option<&Vec<String>>| {
                    v.map_or_else(|| "(none)".to_owned(), |v| v.join(", "))
                };
                differences.push(format!(
                    "SubChunk centre {:?} offset {:?}:\n  A: {}\n  B: {}",
                    key.0,
                    key.1,
                    show(x),
                    show(y)
                ));
            }
        }
    }
    if shown > SHOWN {
        differences.push(format!("... {} more SubChunk differences", shown - SHOWN));
    }
    differences.extend(listed(
        "B payloads that break the strict sub-chunk rules",
        &b.not_strict,
    ));
    notes.extend(listed(
        "A payloads that break the strict sub-chunk rules",
        &a.not_strict,
    ));
    let before: Vec<i64> = b
        .level
        .iter()
        .take(b.before_spawn)
        .map(|(p, _)| p[0] * p[0] + p[1] * p[1])
        .collect();
    if before.windows(2).any(|w| w.first() > w.get(1)) {
        differences.push("B sent its chunks before spawning not nearest first".to_owned());
    }
    let positions: BTreeSet<[i64; 2]> = b.level.iter().map(|(p, _)| *p).collect();
    if positions.len() != b.level.len() {
        differences.push(format!(
            "B sent {} LevelChunks for {} positions",
            b.level.len(),
            positions.len()
        ));
    }
    let runs = |c: &Chunks, after: bool| {
        let mut out: Vec<String> = Vec::new();
        for (at, radius, saved, later) in &c.publisher {
            let text = format!("({},{}) radius {radius} saved {saved}", at[0], at[1]);
            if *later == after && out.last() != Some(&text) {
                out.push(text);
            }
        }
        out
    };
    let (after_a, after_b) = (runs(a, true), runs(b, true));
    if after_a != after_b {
        differences.push(format!(
            "NetworkChunkPublisherUpdate after ChunkRadiusUpdated differs:\n  A: {}\n  B: {}",
            after_a.join(", "),
            after_b.join(", ")
        ));
    }
    for (name, c) in [("A", a), ("B", b)] {
        let count = |after: bool| c.publisher.iter().filter(|p| p.3 == after).count();
        notes.push(format!(
            "{name}: {} LevelChunks ({} before spawning), {} SubChunk answers, chunk radius {:?}, \
             publisher updates {} before the radius ({}) and {} after",
            c.level.len(),
            c.before_spawn,
            c.sub.values().map(Vec::len).sum::<usize>(),
            c.radius,
            count(false),
            runs(c, false).join(", "),
            count(true)
        ));
    }
    (differences, notes)
}

/// The marker of the blocks scenario: its position relative to the base position and the states
/// (as `p6::block_states` writes them) that end a step (alternating) and the scenario; and the
/// positions the steps can change (each step position and its 8 horizontal neighbours). Changes
/// elsewhere are the server's own (BDS turns grass under placed blocks into dirt on random ticks)
/// and are only counted.
#[derive(Debug)]
pub(crate) struct Markers {
    pub(crate) position: [i64; 3],
    pub(crate) steps: [String; 2],
    pub(crate) end: String,
    pub(crate) near: BTreeSet<[i64; 3]>,
}

/// One block change entry of a blocks capture.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Change {
    /// `UpdateBlock` or `UpdateSubChunkBlocks`.
    pub(crate) kind: &'static str,
    /// The capture index of the packet that carried it.
    pub(crate) packet: u64,
    /// Relative to the base position.
    pub(crate) position: [i64; 3],
    pub(crate) layer: u64,
    pub(crate) flags: u64,
    pub(crate) state: String,
}

/// The block packets of one step of the blocks scenario.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Step {
    pub(crate) changes: Vec<Change>,
    /// The names of the other block packets (`BlockActorData`, `UpdateBlockSynced`), in order.
    pub(crate) others: Vec<String>,
    /// Changes away from the step positions (`Markers::near`), not compared.
    pub(crate) ignored: Vec<Change>,
}

/// Splits the block changes of a blocks capture into steps. A step ends with the change that sets
/// the marker position to the step's marker state; the end marker ends the scenario. Changes before
/// the first step's block (none are expected) belong to step 0. Returns the steps and whether the
/// end marker came. Chunks and sub-chunks are not block changes.
pub(crate) fn block_steps(
    capture: &str,
    base: [i64; 3],
    markers: &Markers,
    states: &BTreeMap<u32, String>,
) -> Result<(Vec<Step>, bool), String> {
    let mut steps = Vec::new();
    let mut current = Step::default();
    let state = |id: &Value| {
        id.as_u64()
            .and_then(|id| u32::try_from(id).ok())
            .and_then(|id| states.get(&id))
            .map_or_else(|| format!("unknown {id}"), String::clone)
    };
    let relative = |position: &Value| -> Result<[i64; 3], String> {
        let mut out = [0; 3];
        for (i, (slot, b)) in out.iter_mut().zip(base).enumerate() {
            let v = position
                .get(i)
                .and_then(Value::as_i64)
                .ok_or_else(|| format!("block position {position} is not a position"))?;
            *slot = v.saturating_sub(b);
        }
        Ok(out)
    };
    for (i, line) in capture.lines().enumerate() {
        let value: Value =
            serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        if field(&value, "type").as_str() != Some("packet") {
            continue;
        }
        let fields = field(&value, "fields");
        let packet = field(&value, "index").as_u64().unwrap_or_default();
        let mut changes = Vec::new();
        match field(&value, "name").as_str().unwrap_or_default() {
            "UpdateBlock" => changes.push(Change {
                kind: "UpdateBlock",
                packet,
                position: relative(field(fields, "Position"))?,
                layer: field(fields, "Layer").as_u64().unwrap_or(u64::MAX),
                flags: field(fields, "Flags").as_u64().unwrap_or(u64::MAX),
                state: state(field(fields, "NewBlockRuntimeID")),
            }),
            "UpdateSubChunkBlocks" => {
                for (layer, key) in [(0, "Blocks"), (1, "Extra")] {
                    for entry in array(fields, key) {
                        changes.push(Change {
                            kind: "UpdateSubChunkBlocks",
                            packet,
                            position: relative(field(entry, "BlockPos"))?,
                            layer,
                            flags: field(entry, "Flags").as_u64().unwrap_or(u64::MAX),
                            state: state(field(entry, "BlockRuntimeID")),
                        });
                    }
                }
            }
            name @ ("BlockActorData" | "UpdateBlockSynced") => current.others.push(name.to_owned()),
            _ => {}
        }
        for change in changes {
            if change.position != markers.position {
                if markers.near.contains(&change.position) {
                    current.changes.push(change);
                } else {
                    current.ignored.push(change);
                }
            } else if change.state == markers.end {
                // Changes after the last step's marker form one more step, which fails a comparison.
                if !current.changes.is_empty() || !current.others.is_empty() {
                    steps.push(current);
                }
                return Ok((steps, true));
            } else {
                let want = markers.steps.get(steps.len() % 2).map(String::as_str);
                if Some(change.state.as_str()) != want {
                    return Err(format!(
                        "step {}: the marker became {}, want {}",
                        steps.len(),
                        change.state,
                        want.unwrap_or_default()
                    ));
                }
                steps.push(std::mem::take(&mut current));
            }
        }
    }
    Ok((steps, false))
}

/// What one step changed: per (layer, position) the last state and flags, and the packet kinds that
/// carried the changes.
#[derive(Debug, PartialEq)]
pub(crate) struct StepSummary {
    pub(crate) last: BTreeMap<(u64, [i64; 3]), (String, u64)>,
    pub(crate) kinds: BTreeSet<&'static str>,
    pub(crate) packets: usize,
    pub(crate) others: BTreeSet<String>,
}

impl StepSummary {
    pub(crate) fn of(step: &Step) -> Self {
        let mut last = BTreeMap::new();
        for c in &step.changes {
            last.insert((c.layer, c.position), (c.state.clone(), c.flags));
        }
        let packets: BTreeSet<u64> = step.changes.iter().map(|c| c.packet).collect();
        Self {
            last,
            kinds: step.changes.iter().map(|c| c.kind).collect(),
            packets: packets.len(),
            others: step.others.iter().cloned().collect(),
        }
    }

    /// The packet kind Anchor sends for this step: `UpdateBlock` when one position
    /// changed, else `UpdateSubChunkBlocks`. BDS repeats a position in one `UpdateSubChunkBlocks`
    /// (a block and its neighbour's update of it) and sometimes sends a block and its neighbours'
    /// updates in two packets of consecutive ticks; neither is followed.
    fn normalized_kinds(&self) -> BTreeSet<&'static str> {
        match self.last.len() {
            0 => BTreeSet::new(),
            1 => BTreeSet::from(["UpdateBlock"]),
            _ => BTreeSet::from(["UpdateSubChunkBlocks"]),
        }
    }
}

/// Compares the steps of two blocks captures, A (BDS) and B: per step, the last state and flags of
/// every position, B's packet kinds against A's normalized ones (`StepSummary::normalized_kinds`)
/// and the other block packet names. `labels` name the steps in messages. Returns the differences
/// and notes on the steps where A's own packet kinds differ from its normalized ones.
pub(crate) fn compare_steps(
    a: &[Step],
    b: &[Step],
    labels: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut differences = Vec::new();
    let mut notes = Vec::new();
    if a.len() != b.len() {
        differences.push(format!("blocks: A has {} steps, B {}", a.len(), b.len()));
    }
    for (n, (x, y)) in a.iter().zip(b).enumerate() {
        let label = labels.get(n).map_or("", String::as_str);
        let (x, y) = (StepSummary::of(x), StepSummary::of(y));
        let positions: BTreeSet<&(u64, [i64; 3])> = x.last.keys().chain(y.last.keys()).collect();
        for key in positions {
            let (vx, vy) = (x.last.get(key), y.last.get(key));
            if vx != vy {
                let show = |v: Option<&(String, u64)>| {
                    v.map_or_else(
                        || "(nothing)".to_owned(),
                        |(state, flags)| format!("{state} flags {flags}"),
                    )
                };
                let [dx, dy, dz] = key.1;
                differences.push(format!(
                    "blocks step {n} ({label}): ({dx},{dy},{dz}) layer {}: A {} != B {}",
                    key.0,
                    show(vx),
                    show(vy)
                ));
            }
        }
        let kx = x.normalized_kinds();
        if kx != y.kinds {
            differences.push(format!(
                "blocks step {n} ({label}): packets A {kx:?} (normalized) != B {:?}",
                y.kinds
            ));
        }
        if kx != x.kinds {
            notes.push(format!(
                "blocks step {n} ({label}): A sent {:?} for {} positions in {} packets",
                x.kinds,
                x.last.len(),
                x.packets
            ));
        }
        if x.others != y.others {
            differences.push(format!(
                "blocks step {n} ({label}): other block packets A {:?} != B {:?}",
                x.others, y.others
            ));
        }
    }
    (differences, notes)
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
            "LevelChunk - chunks\n",
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
        // Chunk packets are left out of the sequence (compare_chunks has them), another is not.
        let other = CAPTURE
            .replace("\"Rewind\":40", "\"Rewind\":20")
            .replace("\"name\":\"LevelChunk\"", "\"name\":\"PlayStatus\"");
        let b = window(&other).expect("window");
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
        assert!(parse_deferred("LevelChunk -\n").is_err());
        assert!(parse_deferred("LevelChunk 6 chunks\n").is_err());
        assert!(parse_deferred("A - x\nA - y\n").is_err());
        let deferred =
            parse_deferred("# comment\nLevelChunk - chunks # trailing\n").expect("deferral table");
        assert!(parse_fields("LevelChunk Position\n", &deferred).is_err());
        assert!(parse_fields("StartGame\n", &deferred).is_err());
        assert!(parse_fields("StartGame a..b\n", &deferred).is_err());
        assert!(parse_fields("StartGame A\nStartGame A\n", &deferred).is_err());
    }

    #[test]
    fn the_committed_tables_parse_and_defer_no_cloudburst_packet() {
        let (deferred, _) = tables(
            include_str!("../../data/p6/deferred.txt"),
            include_str!("../../data/p6/fields.txt"),
        );
        // `bot cloudburst` compares these on both servers' captures and fails on any
        // difference; nothing may hide them from the window either.
        for name in ["BiomeDefinitionList", "JigsawStructureData"] {
            assert!(!deferred.contains(name), "{name} is deferred");
        }
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
    fn entity_data_of_other_entities_is_left_out() {
        let packet = |name: &str, id: u64| Received {
            name: name.to_owned(),
            fields: serde_json::json!({ "EntityRuntimeID": id }),
        };
        let window = [
            packet("StartGame", 7),
            packet("SetActorData", 7),
            packet("SetActorData", 31),
            packet("UpdateAttributes", 31),
            packet("UpdateAttributes", 7),
            packet("Respawn", 0),
        ];
        let deferred = parse_deferred("").expect("empty table");
        assert_eq!(
            sequence(&window, &deferred),
            ["StartGame", "SetActorData", "UpdateAttributes", "Respawn"]
        );
    }

    #[test]
    fn sequence_counts_runs_without_deferred_packets() {
        let (deferred, _) = tables("LevelChunk - chunks\n", "");
        let w = window(&CAPTURE.replace("StartGame", "NetworkSettings")).expect("window");
        assert_eq!(sequence(&w, &deferred), ["NetworkSettings x2"]);
    }

    /// A small item packet file: two registry entries, two creative items in two groups, one
    /// shaped recipe (stick and tag ingredients, planks output) and one potion recipe.
    const ITEMS: &str = r#"{"index":1,"name":"ItemRegistry","fields":{"Items":[{"Name":"minecraft:planks","RuntimeID":5,"Data":{}},{"Name":"minecraft:stick","RuntimeID":7,"Data":{}}]}}
{"index":2,"name":"CreativeContent","fields":{"Groups":[{"Category":1,"Name":"a","Icon":{"ItemType":{"NetworkID":0,"MetadataValue":0},"BlockRuntimeID":0}},{"Category":2,"Name":"","Icon":{"ItemType":{"NetworkID":0,"MetadataValue":0},"BlockRuntimeID":0}}],"Items":[{"CreativeItemNetworkID":1,"GroupIndex":0,"Item":{"ItemType":{"NetworkID":5,"MetadataValue":0},"BlockRuntimeID":99,"NBTData":{"compound":{}}}},{"CreativeItemNetworkID":2,"GroupIndex":1,"Item":{"ItemType":{"NetworkID":7,"MetadataValue":0},"BlockRuntimeID":0,"NBTData":{"compound":{}}}}]}}
{"index":3,"name":"CraftingData","fields":{"ShapedRecipes":[{"RecipeID":"r","RecipeNetworkID":1,"UUID":"u1","Input":[{"Count":1,"Descriptor":{"Name":"minecraft:stick","MetadataValue":32767}},{"Count":1,"Descriptor":{"Tag":"minecraft:logs"}},{"Count":0,"Descriptor":{}}],"Output":[{"ItemType":{"NetworkID":5,"MetadataValue":0},"BlockRuntimeID":99,"NBTData":{"compound":{}}}]}],"MultiRecipes":[{"UUID":"m","RecipeNetworkID":2}],"PotionRecipes":[{"InputPotionID":7,"InputPotionMetadata":0,"ReagentItemID":5,"ReagentItemMetadata":0,"OutputPotionID":7,"OutputPotionMetadata":0}],"ClearRecipes":true}}
"#;

    fn items(text: &str) -> Items {
        parse_items(text).expect("item packets")
    }

    #[test]
    fn identical_item_packets_have_no_differences() {
        let (differences, notes) = compare_items(&items(ITEMS), &items(ITEMS));
        assert_eq!(differences, Vec::<String>::new());
        assert_eq!(notes, Vec::<String>::new());
    }

    #[test]
    fn item_checks_need_both_captures_to_have_the_packet() {
        let changed = ITEMS.replace("minecraft:stick", "minecraft:other");
        let (differences, _) = compare_items(&Items::default(), &items(&changed));
        assert!(differences.is_empty());
        assert!(parse_items("{\"name\":\"StartGame\",\"fields\":{}}\n").is_err());
    }

    #[test]
    fn item_registry_compares_by_runtime_id_not_order() {
        let swapped = ITEMS.replace(
            r#"[{"Name":"minecraft:planks","RuntimeID":5,"Data":{}},{"Name":"minecraft:stick","RuntimeID":7,"Data":{}}]"#,
            r#"[{"Name":"minecraft:stick","RuntimeID":7,"Data":{}},{"Name":"minecraft:planks","RuntimeID":5,"Data":{}}]"#,
        );
        assert_ne!(swapped, ITEMS);
        assert!(compare_items(&items(ITEMS), &items(&swapped)).0.is_empty());
        let renumbered = ITEMS.replace(r#""RuntimeID":7,"Data":{}"#, r#""RuntimeID":8,"Data":{}"#);
        let (differences, _) = compare_items(&items(ITEMS), &items(&renumbered));
        assert_eq!(differences.len(), 2, "{differences:?}");
        assert!(differences[1].starts_with("ItemRegistry first different entry"));
        let data = ITEMS.replace(
            r#""RuntimeID":5,"Data":{}"#,
            r#""RuntimeID":5,"Data":{"a":{"byte":1}}"#,
        );
        let (differences, _) = compare_items(&items(ITEMS), &items(&data));
        assert_eq!(differences.len(), 2, "{differences:?}");
        assert!(differences[1].contains(r#"{"a":{"byte":1}}"#));
    }

    #[test]
    fn creative_items_of_b_must_be_in_a() {
        // B listing fewer items than A is fine; an item A does not have, or a different NBT, is not.
        let (a, fewer) = (
            items(ITEMS),
            ITEMS.replace(r#","GroupIndex":1,"#, r#","GroupIndex":0,"#),
        );
        let (differences, _) = compare_items(&a, &items(&fewer));
        assert_eq!(differences, ["CreativeContent of B: group 1 has no items"]);
        let nbt = ITEMS.replace(
            r#""BlockRuntimeID":99,"NBTData":{"compound":{}}}},"#,
            r#""BlockRuntimeID":99,"NBTData":{"compound":{"x":{"int":1}}}}},"#,
        );
        let (differences, _) = compare_items(&a, &items(&nbt));
        assert_eq!(differences.len(), 1, "{differences:?}");
        assert!(differences[0].contains("5:0:99 {\"compound\":{\"x\":{\"int\":1}}}"));
        // An item A does not list at all is content: a note, not a difference.
        let other = ITEMS.replace(
            r#"{"ItemType":{"NetworkID":5,"MetadataValue":0},"BlockRuntimeID":99,"NBTData":{"compound":{}}}},"#,
            r#"{"ItemType":{"NetworkID":6,"MetadataValue":0},"BlockRuntimeID":0,"NBTData":{"compound":{}}}},"#,
        );
        let (differences, notes) = compare_items(&a, &items(&other));
        assert!(differences.is_empty(), "{differences:?}");
        assert!(
            notes[0].starts_with("CreativeContent items of B that A does not list: 1:"),
            "{notes:?}"
        );
    }

    #[test]
    fn creative_groups_must_be_consistent() {
        let creative = |items: &str| -> Value {
            serde_json::from_str(&format!(r#"{{"Groups":[{{}},{{}}],"Items":{items}}}"#))
                .expect("json")
        };
        let item = |id: u32, group: u32| {
            format!(r#"{{"CreativeItemNetworkID":{id},"GroupIndex":{group}}}"#)
        };
        let good = creative(&format!("[{},{}]", item(1, 0), item(2, 1)));
        assert_eq!(group_problems(&good), Vec::<String>::new());
        let problems = group_problems(&creative(&format!("[{},{}]", item(1, 1), item(3, 0))));
        assert_eq!(
            problems,
            [
                "item 1 goes back to group 0 after group 1",
                "item 1 has creative network ID 3, want 2"
            ]
        );
        let problems = group_problems(&creative(&format!("[{}]", item(1, 2))));
        assert_eq!(
            problems,
            [
                "item 0 has group index 2 of 2 groups",
                "group 0 has no items",
                "group 1 has no items"
            ]
        );
    }

    #[test]
    fn crafting_representations_of_b_must_be_used_by_a() {
        let a = items(ITEMS);
        // A stick stack with a block runtime ID A never uses, an unknown tag and an unknown
        // descriptor item.
        let b = ITEMS
            .replace(
                r#""BlockRuntimeID":99,"NBTData":{"compound":{}}}]"#,
                r#""BlockRuntimeID":98,"NBTData":{"compound":{}}}]"#,
            )
            .replace("minecraft:logs", "minecraft:planks_tag")
            .replace(
                r#"{"Name":"minecraft:stick","MetadataValue":32767}"#,
                r#"{"Name":"minecraft:stick","MetadataValue":3}"#,
            );
        let (differences, notes) = compare_items(&a, &items(&b));
        assert_eq!(differences.len(), 1, "{differences:?}");
        assert!(
            differences[0]
                .starts_with("CraftingData item representations of B that A never uses: 3:")
        );
        for missing in [
            "item minecraft:stick:3",
            "stack 5:0:98",
            "tag minecraft:planks_tag",
        ] {
            assert!(
                differences[0].contains(missing),
                "{missing}: {differences:?}"
            );
        }
        assert_eq!(
            notes[0],
            "CraftingData ShapedRecipes: A 1, B 1, only in A 1, only in B 1"
        );
        assert_eq!(notes[1], "  only in A: r");
    }

    #[test]
    fn descriptors_match_stacks_of_the_same_item() {
        // A's stick stack (network ID 7, metadata 0) stands for the name descriptor stick:0 and the
        // potion item 7:0; a potion item A never uses is a difference.
        let b = ITEMS.replace(
            r#"{"Name":"minecraft:stick","MetadataValue":32767}"#,
            r#"{"Name":"minecraft:stick","MetadataValue":0}"#,
        );
        assert!(compare_items(&items(ITEMS), &items(&b)).0.is_empty());
        let potion = ITEMS.replace(r#""OutputPotionMetadata":0"#, r#""OutputPotionMetadata":4"#);
        let (differences, _) = compare_items(&items(ITEMS), &items(&potion));
        assert!(differences[0].contains("potion 7:4"), "{differences:?}");
    }

    #[test]
    fn recipe_notes_ignore_network_ids_and_uuids_but_not_multi_recipe_uuids() {
        let renumbered = ITEMS.replace(
            r#""RecipeNetworkID":1,"UUID":"u1""#,
            r#""RecipeNetworkID":9,"UUID":"u2""#,
        );
        let (differences, notes) = compare_items(&items(ITEMS), &items(&renumbered));
        assert!(
            differences.is_empty() && notes.is_empty(),
            "{differences:?} {notes:?}"
        );
        let multi = ITEMS.replace(r#""UUID":"m""#, r#""UUID":"n""#);
        let (_, notes) = compare_items(&items(ITEMS), &items(&multi));
        assert_eq!(
            notes[0],
            "CraftingData MultiRecipes: A 1, B 1, only in A 1, only in B 1"
        );
    }

    /// Network IDs of the blocks capture tests: 1 fence, 2 fence connected east, 3 gold, 4 iron,
    /// 5 diamond.
    fn block_names() -> BTreeMap<u32, String> {
        [
            (1, "minecraft:oak_fence[east=0]"),
            (2, "minecraft:oak_fence[east=1]"),
            (3, "minecraft:gold_block[]"),
            (4, "minecraft:iron_block[]"),
            (5, "minecraft:diamond_block[]"),
        ]
        .into_iter()
        .map(|(id, name)| (id, name.to_owned()))
        .collect()
    }

    fn markers() -> Markers {
        Markers {
            position: [-4, 0, 0],
            steps: [
                "minecraft:gold_block[]".to_owned(),
                "minecraft:iron_block[]".to_owned(),
            ],
            end: "minecraft:diamond_block[]".to_owned(),
            near: (-1..=2)
                .flat_map(|x| (-1..=1).map(move |z| [x, 0, z]))
                .collect(),
        }
    }

    fn update(index: u32, x: i64, id: u32) -> String {
        format!(
            r#"{{"type":"packet","index":{index},"name":"UpdateBlock","fields":{{"Position":[{x},-60,2],"NewBlockRuntimeID":{id},"Flags":3,"Layer":0}}}}"#
        )
    }

    fn sub_chunk_update(index: u32, entries: &[(i64, u32)]) -> String {
        let entries: Vec<String> = entries
            .iter()
            .map(|(x, id)| format!(r#"{{"BlockPos":[{x},-60,2],"BlockRuntimeID":{id},"Flags":3}}"#))
            .collect();
        format!(
            r#"{{"type":"packet","index":{index},"name":"UpdateSubChunkBlocks","fields":{{"Blocks":[{}],"Extra":[]}}}}"#,
            entries.join(",")
        )
    }

    /// A blocks capture (base (2, -60, 2), marker at x -2): step 0 places a fence, step 1 a second
    /// fence east of it, with BDS's repeated entry.
    fn blocks_capture() -> String {
        [
            update(1, 2, 1),
            update(2, -2, 3),
            sub_chunk_update(3, &[(3, 1), (2, 2), (3, 1)]),
            update(4, -2, 4),
            update(5, -2, 5),
        ]
        .join("\n")
    }

    fn steps(capture: &str) -> (Vec<Step>, bool) {
        block_steps(capture, [2, -60, 2], &markers(), &block_names()).expect("blocks capture")
    }

    #[test]
    fn markers_split_block_changes_into_steps() {
        let (steps, ended) = steps(&blocks_capture());
        assert!(ended);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].changes.len(), 1);
        assert_eq!(steps[0].changes[0].position, [0, 0, 0]);
        assert_eq!(steps[1].changes.len(), 3);
        assert_eq!(steps[1].changes[1].state, "minecraft:oak_fence[east=1]");
        // A change away from the steps (grass turning into dirt) is set aside.
        let dirt = blocks_capture().replace(
            &update(4, -2, 4),
            &format!("{}\n{}", update(9, 9, 1), update(4, -2, 4)),
        );
        let (with_dirt, _) = self::steps(&dirt);
        assert_eq!(with_dirt[1].changes, steps[1].changes);
        assert_eq!(with_dirt[1].ignored.len(), 1);
        // A wrong marker order is an error; a capture without the end marker is not ended.
        let swapped = blocks_capture().replace(
            r#"[-2,-60,2],"NewBlockRuntimeID":3"#,
            r#"[-2,-60,2],"NewBlockRuntimeID":4"#,
        );
        assert!(block_steps(&swapped, [2, -60, 2], &markers(), &block_names()).is_err());
        let cut = blocks_capture()
            .lines()
            .take(4)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!self::steps(&cut).1);
    }

    #[test]
    fn steps_compare_last_states_and_normalized_packet_kinds() {
        let (a, _) = steps(&blocks_capture());
        // B sends step 1 without the repeated entry: the same last states and the same kind.
        let b_capture = blocks_capture().replace(
            &sub_chunk_update(3, &[(3, 1), (2, 2), (3, 1)]),
            &sub_chunk_update(3, &[(3, 1), (2, 2)]),
        );
        let (b, _) = steps(&b_capture);
        let labels = ["s0".to_owned(), "s1".to_owned()];
        assert_eq!(compare_steps(&a, &b, &labels), (Vec::new(), Vec::new()));
        // A different last state and a single position sent in an UpdateSubChunkBlocks differ;
        // BDS's repeated entry for one position is a note, not a difference.
        let c_capture = blocks_capture()
            .replace(&update(1, 2, 1), &sub_chunk_update(1, &[(2, 1)]))
            .replace(
                &sub_chunk_update(3, &[(3, 1), (2, 2), (3, 1)]),
                &sub_chunk_update(3, &[(3, 1), (2, 1)]),
            );
        let (c, _) = steps(&c_capture);
        let (differences, notes) = compare_steps(&a, &c, &labels);
        assert_eq!(differences.len(), 2, "{differences:?}");
        assert!(differences[0].contains("packets A {\"UpdateBlock\"}"));
        assert!(differences[1].contains("(0,0,0) layer 0: A minecraft:oak_fence[east=1] flags 3"));
        assert!(notes.is_empty());
        let one_position =
            blocks_capture().replace(&update(1, 2, 1), &sub_chunk_update(1, &[(2, 1), (2, 1)]));
        let (d, _) = steps(&one_position);
        let (differences, notes) = compare_steps(&d, &a, &labels);
        assert!(differences.is_empty(), "{differences:?}");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(
            compare_steps(&a, &a[..1], &labels).0,
            ["blocks: A has 2 steps, B 1"]
        );
    }

    /// A spawn capture with chunks: the player at block (16.5, 70, -0.5), so the spawn chunk is
    /// (1, -1) and the column (16, -1).
    fn chunk_capture(limit: u32, height: u32) -> String {
        format!(
            r#"{{"type":"packet","index":0,"name":"StartGame","fields":{{"PlayerPosition":[16.5,70,-0.5]}}}}
{{"type":"packet","index":1,"name":"NetworkChunkPublisherUpdate","fields":{{"Position":[16,70,-1],"Radius":64,"SavedChunks":[]}}}}
{{"type":"packet","index":2,"name":"ChunkRadiusUpdated","fields":{{"ChunkRadius":16}}}}
{{"type":"packet","index":3,"name":"NetworkChunkPublisherUpdate","fields":{{"Position":[16,70,-1],"Radius":256,"SavedChunks":[]}}}}
{{"type":"packet","index":4,"name":"LevelChunk","fields":{{"Position":[1,-1],"SubChunkCount":0,"SubChunkLimit":{limit},"RawPayload":"AQ=="}}}}
{{"type":"packet","index":5,"name":"LevelChunk","fields":{{"Position":[2,-1],"SubChunkCount":0,"SubChunkLimit":{limit},"RawPayload":"AQ=="}}}}
{{"type":"packet","index":6,"name":"PlayStatus","fields":{{"Status":3}}}}
{{"type":"packet","index":7,"name":"SubChunk","fields":{{"CacheEnabled":false,"Dimension":0,"Position":[1,0,-1],"SubChunkEntries":[{{"Offset":[0,-4,0],"Result":1,"HeightMapType":1,"HeightMapData":[[{height}]],"RawPayload":{{"len":9,"sha256":"ab"}}}},{{"Offset":[0,-3,0],"Result":6,"HeightMapType":3,"HeightMapData":null,"RawPayload":null}}]}}}}
{{"type":"subchunk","index":7,"entries":[{{"offset":[0,-4,0],"result":1,"payload":{{"strict":"ok","layers":[{{"content_sha256":"c1"}}]}}}},{{"offset":[0,-3,0],"result":6}}]}}
{{"type":"packet","index":8,"name":"NetworkChunkPublisherUpdate","fields":{{"Position":[16,-59,-1],"Radius":256,"SavedChunks":[]}}}}"#
        )
    }

    #[test]
    fn chunk_packets_are_read_relative_to_the_spawn() {
        let c = chunks(&chunk_capture(1, 4)).expect("capture");
        assert_eq!(c.level.len(), 2);
        assert_eq!((c.level[0].0, c.level[1].0), ([0, 0], [1, 0]));
        assert_eq!(c.before_spawn, 2);
        assert_eq!(c.radius, Some(16));
        assert_eq!(
            c.publisher,
            [
                ([0, 0], 64, 0, false),
                ([0, 0], 256, 0, true),
                ([0, 0], 256, 0, true)
            ]
        );
        let key = ([0, 0, 0], [0, -4, 0]);
        assert!(
            c.sub[&key][0].contains(r#""content":["c1"]"#),
            "{:?}",
            c.sub
        );
        assert!(c.sub[&key][0].contains(r#""strict":"ok""#));
        assert!(c.not_strict.is_empty());
        assert_eq!(compare_chunks(&c, &c).0, Vec::<String>::new());
    }

    #[test]
    fn chunk_differences_are_reported() {
        let a = chunks(&chunk_capture(1, 4)).expect("capture");
        let b = chunks(&chunk_capture(2, 5)).expect("capture");
        let (differences, notes) = compare_chunks(&a, &b);
        assert_eq!(differences.len(), 2, "{differences:?}");
        assert!(differences[0].starts_with("LevelChunk fields differ"));
        assert!(differences[1].starts_with("SubChunk centre [0, 0, 0] offset [0, -4, 0]"));
        assert_eq!(notes.len(), 2);
        // B's chunks before spawning must be nearest first, and B's payloads strict.
        let far_first = chunk_capture(1, 4)
            .replace(r#""Position":[1,-1],"Sub"#, r#""Position":[9,-1],"Sub"#)
            .replace(r#""Position":[2,-1],"Sub"#, r#""Position":[1,-1],"Sub"#)
            .replace(r#""strict":"ok""#, r#""strict":"width 7""#);
        let c = chunks(&far_first).expect("capture");
        let (differences, _) = compare_chunks(&a, &c);
        assert!(
            differences.iter().any(|d| d.contains("not nearest first")),
            "{differences:?}"
        );
        assert!(
            differences.iter().any(|d| d.contains("break the strict")),
            "{differences:?}"
        );
    }
}
