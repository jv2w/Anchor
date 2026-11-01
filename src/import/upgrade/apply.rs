//! Block state upgrade rules and their interpreter: a port of PMMP `BlockStateUpgrader`.
//! The import runs the generated rules (`super::table`); `cargo xtask gen`
//! includes this file by path to check the schemas with the same code before it generates them,
//! so the rules and their meaning live in one place.

use std::collections::BTreeMap;

/// A property value in a rule (PMMP `BlockStateUpgradeSchemaModelTag`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tag<'a> {
    Byte(u8),
    Int(i32),
    String(&'a str),
}

impl Tag<'_> {
    /// Returns the tag as a value a state holds.
    pub(crate) fn to_value(self) -> Value {
        match self {
            Self::Byte(b) => Value::Byte(b),
            Self::Int(i) => Value::Int(i),
            Self::String(s) => Value::String(s.to_owned()),
        }
    }

    /// Whether `value` is this tag: same type and value (PMMP `Tag::equals`).
    fn matches(self, value: &Value) -> bool {
        match (self, value) {
            (Self::Byte(a), Value::Byte(b)) => a == *b,
            (Self::Int(a), Value::Int(b)) => a == *b,
            (Self::String(a), Value::String(b)) => a == b,
            _ => false,
        }
    }
}

/// A property value of a state being upgraded. Bedrock states hold only these three tag types.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Value {
    Byte(u8),
    Int(i32),
    String(String),
}

/// The properties of a state, by name.
pub(crate) type States = BTreeMap<String, Value>;

/// The tag type a flattened property must have for the flattening to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Byte,
    Int,
    String,
}

/// A property folded into the block name (PMMP `BlockStateUpgradeSchemaFlattenInfo`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Flatten<'a> {
    pub(crate) prefix: &'a str,
    pub(crate) property: &'a str,
    pub(crate) suffix: &'a str,
    pub(crate) kind: Kind,
    /// Values whose name part differs from their text, sorted by value.
    pub(crate) remaps: &'a [(&'a str, &'a str)],
}

/// The name a matching remapped state gets.
#[derive(Debug, Clone, Copy)]
pub(crate) enum NewName<'a> {
    Name(&'a str),
    Flatten(Flatten<'a>),
}

/// A state rule (PMMP `BlockStateUpgradeSchemaBlockRemap`): a state that has every `old`
/// property becomes `new_name` with `new` and the `copied` properties it has.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Remap<'a> {
    pub(crate) old: &'a [(&'a str, Tag<'a>)],
    pub(crate) new_name: NewName<'a>,
    pub(crate) new: &'a [(&'a str, Tag<'a>)],
    pub(crate) copied: &'a [&'a str],
}

/// Every rule of one schema for one old block name. All of them use the old name and the old
/// property names, as the schema format says.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rules<'a> {
    pub(crate) renamed: Option<&'a str>,
    pub(crate) flattened: Option<Flatten<'a>>,
    /// Sorted by property.
    pub(crate) added: &'a [(&'a str, Tag<'a>)],
    pub(crate) removed: &'a [&'a str],
    /// Sorted by old property; order does not matter, the generator rejects a rename to a
    /// renamed property.
    pub(crate) renamed_properties: &'a [(&'a str, &'a str)],
    /// Sorted by property; each property's (old, new) pairs in schema order, the first match
    /// winning.
    pub(crate) remapped_values: &'a [(&'a str, &'a [(Tag<'a>, Tag<'a>)])],
    /// In schema order, most specific first: the first match wins.
    pub(crate) remapped_states: &'a [Remap<'a>],
}

/// One schema file (PMMP `BlockStateUpgradeSchema`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Schema<'a> {
    /// The block state version it upgrades to: (major << 24) | (minor << 16) | (patch << 8) |
    /// revision.
    pub(crate) version: i32,
    /// The rules by old block name, sorted by name.
    pub(crate) blocks: &'a [(&'a str, Rules<'a>)],
}

impl<'a> Schema<'a> {
    fn rules(&self, name: &str) -> Option<&Rules<'a>> {
        let i = self.blocks.binary_search_by(|(n, _)| (*n).cmp(name)).ok()?;
        self.blocks.get(i).map(|(_, rules)| rules)
    }
}

/// Upgrades a state of block state version `version` through `schemas`, which are sorted by
/// (version, id) as PMMP sorts them. Schemas of a version above the state's apply. Of the schemas
/// of the state's own version, the first introduced that version, so the state has it already;
/// the others, written by releases that kept the version, apply, since the version does not say
/// which of them did. PMMP applies the first one too: from 0321 on every schema writes
/// 1.21.60.33, so 0321 runs again on current states and turns `red_mushroom_block` 10 and 15 into
/// `mushroom_stem` (`BedrockBlockUpgradeSchema` #22). With one schema of the version this is
/// PMMP's rule.
///
/// A state of a version above the newest schema's is upgraded as one of the newest version, where
/// PMMP upgrades nothing: a 1.21 game wrote 18,186,752 (1.21.130) in a real world for states
/// without the properties the 1.26 schemas of version 1.21.60.33 add (fence connections, stair
/// corners), which BDS 1.26.52 still numbers 1.21.60.33. Those schemas only add properties that
/// are missing and rename old names, so they leave a state that has them as it is.
pub(crate) fn upgrade(
    schemas: &[Schema<'_>],
    version: i32,
    name: &mut String,
    states: &mut States,
) {
    let version = schemas
        .last()
        .map_or(version, |newest| version.min(newest.version));
    for group in schemas.chunk_by(|a, b| a.version == b.version) {
        let Some(first) = group.first() else {
            continue;
        };
        if version > first.version {
            continue;
        }
        let applied = usize::from(version == first.version);
        for schema in group.get(applied..).unwrap_or_default() {
            if let Some(rules) = schema.rules(name) {
                apply(rules, name, states);
            }
        }
    }
}

/// Applies one schema's rules for a block (PMMP `applySchema`): a matching state rule replaces
/// everything; otherwise the rename or flattening, then the added, removed and renamed properties
/// and the value remaps.
fn apply(rules: &Rules<'_>, name: &mut String, states: &mut States) {
    if let Some((new_name, new_states)) = remapped(rules, name, states) {
        *name = new_name;
        *states = new_states;
        return;
    }
    if let Some(renamed) = rules.renamed {
        renamed.clone_into(name);
    } else if let Some(flatten) = &rules.flattened
        && let Some(new_name) = flattened_name(flatten, states)
    {
        states.remove(flatten.property);
        *name = new_name;
    }
    for (property, value) in rules.added {
        if !states.contains_key(*property) {
            states.insert((*property).to_owned(), value.to_value());
        }
    }
    for property in rules.removed {
        states.remove(*property);
    }
    for (old, new) in rules.renamed_properties {
        if let Some(value) = states.remove(*old) {
            let value = remapped_value(rules, old, value);
            states.insert((*new).to_owned(), value);
        }
    }
    for (property, _) in rules.remapped_values {
        if let Some(value) = states.remove(*property) {
            let value = remapped_value(rules, property, value);
            states.insert((*property).to_owned(), value);
        }
    }
}

/// The state the first matching state rule makes, if one matches (PMMP `applyStateRemapped`).
fn remapped(rules: &Rules<'_>, name: &str, states: &States) -> Option<(String, States)> {
    let remap = rules.remapped_states.iter().find(|remap| {
        remap.old.len() <= states.len()
            && remap
                .old
                .iter()
                .all(|(k, v)| states.get(*k).is_some_and(|s| v.matches(s)))
    })?;
    let new_name = match &remap.new_name {
        NewName::Name(n) => (*n).to_owned(),
        NewName::Flatten(f) => flattened_name(f, states).unwrap_or_else(|| name.to_owned()),
    };
    let mut new_states: States = remap
        .new
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.to_value()))
        .collect();
    for property in remap.copied {
        if let Some(value) = states.get(*property) {
            new_states.insert((*property).to_owned(), value.clone());
        }
    }
    Some((new_name, new_states))
}

/// The value a property's value remaps to, or the value itself (PMMP `locateNewPropertyValue`).
fn remapped_value(rules: &Rules<'_>, property: &str, value: Value) -> Value {
    let pairs = rules
        .remapped_values
        .binary_search_by(|(p, _)| (*p).cmp(property))
        .ok()
        .and_then(|i| rules.remapped_values.get(i));
    match pairs.and_then(|(_, pairs)| pairs.iter().find(|(old, _)| old.matches(&value))) {
        Some((_, new)) => new.to_value(),
        None => value,
    }
}

/// The name a flattening gives, or `None` if the property is missing or of another type (PMMP
/// `applyPropertyFlattened`, which then leaves the state alone). A byte is written signed, as
/// PMMP reads it.
fn flattened_name(flatten: &Flatten<'_>, states: &States) -> Option<String> {
    let key = match (flatten.kind, states.get(flatten.property)?) {
        (Kind::String, Value::String(s)) => s.clone(),
        (Kind::Byte, Value::Byte(b)) => b.cast_signed().to_string(),
        (Kind::Int, Value::Int(i)) => i.to_string(),
        _ => return None,
    };
    let embed = flatten
        .remaps
        .binary_search_by(|(k, _)| (*k).cmp(&key))
        .ok()
        .and_then(|i| flatten.remaps.get(i))
        .map_or(key.as_str(), |(_, v)| v);
    Some(format!("{}{embed}{}", flatten.prefix, flatten.suffix))
}
