//! `cargo xtask p6 compare`: compares the spawn window of two bot captures. The window is every
//! packet received before the bot sent `SetLocalPlayerAsInitialised`. Packets in the deferral table
//! are removed from both windows; the remaining name sequences must be equal, and so must the
//! fixed fields of each pair of same-named packets (paired in order). NetherNet captures also hold
//! the server's answer SDP, whose line structure must be equal (see `compare_structure`). The item
//! packets the bot writes in full (`<capture>.items.jsonl`) are compared by `compare_items`.

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
}
