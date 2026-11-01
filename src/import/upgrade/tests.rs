//! The block state, legacy value and item upgrade against PMMP's own upgrader (golden files) and
//! the schema rules.

use serde_json::Value as Json;

use super::apply::{self, Flatten, Kind, NewName, Remap, Rules, Schema, Tag};
use super::*;

/// PMMP's own upgrader over the 5.3.0 schemas (`xtask/php/upgrade.php`, `cargo xtask dump`).
const GOLDEN_STATES: &str = include_str!("../../../xtask/data/upgrade/golden/states.jsonl");
const GOLDEN_LEGACY: &str = include_str!("../../../xtask/data/upgrade/golden/legacy.jsonl");
const GOLDEN_REFERENCE: &str = include_str!("../../../xtask/data/upgrade/golden/reference.jsonl");

/// A golden state or input object as NBT: `name` and `val` plain, `states` typed.
fn compound(json: &Json) -> Compound {
    let mut c = Compound::new();
    for (k, v) in json.as_object().unwrap() {
        let value = match k.as_str() {
            "name" => Value::String(v.as_str().unwrap().to_owned()),
            "val" => Value::Short(i16::try_from(v.as_i64().unwrap()).unwrap()),
            "version" => Value::Int(i32::try_from(v.as_i64().unwrap()).unwrap()),
            "states" => Value::Compound(
                v.as_object()
                    .unwrap()
                    .iter()
                    .map(|(p, t)| (p.clone(), typed(t)))
                    .collect(),
            ),
            _ => panic!("unexpected key {k}"),
        };
        c.insert(k.clone(), value);
    }
    c
}

fn typed(t: &Json) -> Value {
    let (kind, v) = t.as_object().unwrap().iter().next().unwrap();
    match kind.as_str() {
        "byte" => Value::Byte(u8::try_from(v.as_u64().unwrap()).unwrap()),
        "int" => Value::Int(i32::try_from(v.as_i64().unwrap()).unwrap()),
        "string" => Value::String(v.as_str().unwrap().to_owned()),
        "short" => Value::Short(i16::try_from(v.as_i64().unwrap()).unwrap()),
        _ => panic!("unexpected type {kind}"),
    }
}

fn golden(json: &Json) -> Option<Compound> {
    (!json.is_null()).then(|| compound(json))
}

fn has_legacy_entry(name: &str, meta: u8) -> bool {
    find(&table::LEGACY, name).is_some_and(|metas| find_copied(metas, meta).is_some())
}

#[test]
fn states_upgrade_as_pmmp_upgrades_them_except_for_metas_the_legacy_table_lacks() {
    let mut fallbacks = 0;
    let mut lines = 0;
    for line in GOLDEN_STATES.lines() {
        let json: Json = serde_json::from_str(line).unwrap();
        let input = compound(&json["in"]);
        let expected = golden(&json["out"]);
        lines += 1;
        if let Some(out) = state(&input) {
            assert_eq!(Some(out), expected, "{line}");
        } else {
            // PMMP falls back to meta 0 (or throws); Anchor keeps the entry as unknown.
            let (Some(Value::String(name)), Some(Value::Short(meta))) =
                (input.get("name"), input.get("val"))
            else {
                panic!("a modern state was not upgraded: {line}");
            };
            assert!(
                !has_legacy_entry(name, u8::try_from(*meta).unwrap()),
                "{line}"
            );
            let zero = Compound::from([
                ("name".to_owned(), Value::String(name.clone())),
                ("val".to_owned(), Value::Short(0)),
            ]);
            assert_eq!(expected, state(&zero), "{line}");
            fallbacks += 1;
        }
    }
    assert_eq!(lines, 15_560);
    assert!(fallbacks > 0);
}

#[test]
fn legacy_terrain_values_upgrade_as_pmmp_and_unknown_metas_stay_unknown() {
    let mut known = 0;
    for line in GOLDEN_LEGACY.lines() {
        let json: Json = serde_json::from_str(line).unwrap();
        let id = u16::try_from(json["id"].as_u64().unwrap()).unwrap();
        let meta = u8::try_from(json["meta"].as_u64().unwrap()).unwrap();
        let expected = golden(&json["state"]);
        match legacy(id, meta) {
            Some(out) => {
                assert_eq!(Some(out), expected, "{line}");
                known += 1;
            }
            // PMMP gives meta 0's state or throws.
            None => assert_eq!(expected, legacy(id, 0), "{line}"),
        }
    }
    assert!(known > 2_000, "{known}");
    assert_eq!(
        legacy(1, 0).unwrap().get("name"),
        Some(&Value::String("minecraft:stone".to_owned()))
    );
    assert_eq!(legacy(1, 15), None);
    assert_eq!(legacy(u16::MAX, 0), None);
}

#[test]
fn current_states_that_pmmp_turns_back_are_kept() {
    let mut lines = 0;
    for line in GOLDEN_REFERENCE.lines() {
        let json: Json = serde_json::from_str(line).unwrap();
        let input = compound(&json["in"]);
        assert_ne!(Some(&input), golden(&json["out"]).as_ref());
        assert_eq!(state(&input), Some(input), "{line}");
        lines += 1;
    }
    // red_mushroom_block with huge_mushroom_bits 10 and 15 (BedrockBlockUpgradeSchema #22).
    assert_eq!(lines, 2);
}

#[test]
fn entries_that_are_not_states_are_not_upgraded() {
    let stone = || {
        Compound::from([
            (
                "name".to_owned(),
                Value::String("minecraft:stone".to_owned()),
            ),
            ("states".to_owned(), Value::Compound(Compound::new())),
        ])
    };
    let upgraded = state(&stone()).unwrap();
    assert_eq!(
        upgraded.get("version"),
        Some(&Value::Int(BLOCK_STATE_VERSION))
    );
    let mut extra = stone();
    extra.insert("extra".to_owned(), Value::Byte(1));
    let mut bad_version = stone();
    bad_version.insert("version".to_owned(), Value::Long(1));
    let mut bad_property = stone();
    bad_property.insert(
        "states".to_owned(),
        Value::Compound(Compound::from([("p".to_owned(), Value::Long(1))])),
    );
    let mut no_states = stone();
    no_states.remove("states");
    let bad_val = Compound::from([
        (
            "name".to_owned(),
            Value::String("minecraft:stone".to_owned()),
        ),
        ("val".to_owned(), Value::Int(0)),
    ]);
    // A legacy state is `{name, val}` alone, as a modern one has no key of its own.
    let legacy = || {
        Compound::from([
            (
                "name".to_owned(),
                Value::String("minecraft:stone".to_owned()),
            ),
            ("val".to_owned(), Value::Short(0)),
        ])
    };
    assert!(state(&legacy()).is_some());
    let mut legacy_extra = legacy();
    legacy_extra.insert("extra".to_owned(), Value::Byte(1));
    let mut legacy_states = legacy();
    legacy_states.insert("states".to_owned(), Value::Compound(Compound::new()));
    for entry in [
        extra,
        bad_version,
        bad_property,
        no_states,
        bad_val,
        legacy_extra,
        legacy_states,
    ] {
        assert_eq!(state(&entry), None, "{entry:?}");
    }
}

#[test]
fn a_state_pmmp_saved_with_its_data_version_upgrades_without_it() {
    // PMMP writes `PMMPDataVersion` next to every state it saves (`BlockStateData::toNbt`) and
    // drops it when it reads one (`fromNbt`). 17_825_808 is 1.16.0.16, before wool split.
    let wool = || {
        Compound::from([
            ("name".to_owned(), s("minecraft:wool")),
            (
                "states".to_owned(),
                Value::Compound(Compound::from([("color".to_owned(), s("red"))])),
            ),
            ("version".to_owned(), Value::Int(17_825_808)),
            ("PMMPDataVersion".to_owned(), Value::Long(1)),
        ])
    };
    let red_wool = Compound::from([
        ("name".to_owned(), s("minecraft:red_wool")),
        ("states".to_owned(), Value::Compound(Compound::new())),
        ("version".to_owned(), Value::Int(BLOCK_STATE_VERSION)),
    ]);
    assert_eq!(state(&wool()), Some(red_wool.clone()));
    let mut stack = item_stack(&[
        ("Name", s("minecraft:red_wool")),
        ("Count", Value::Byte(1)),
        ("Block", Value::Compound(wool())),
    ]);
    assert_eq!(nbt(&mut stack), 1);
    assert_eq!(stack.get("Block"), Some(&Value::Compound(red_wool.clone())));
    // A current state saved by PMMP counts as changed too: only the key goes.
    let mut current = red_wool.clone();
    current.insert("PMMPDataVersion".to_owned(), Value::Long(1));
    let mut block = item_stack(&[("Block", Value::Compound(current))]);
    assert_eq!(nbt(&mut block), 1);
    assert_eq!(block.get("Block"), Some(&Value::Compound(red_wool)));
}

fn s(name: &str) -> Value {
    Value::String(name.to_owned())
}

fn item_stack(entries: &[(&str, Value)]) -> Compound {
    entries
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

#[test]
fn item_names_and_metas_go_through_the_item_schemas_lowercased() {
    assert_eq!(
        item_id_meta("minecraft:photo", 0),
        ("minecraft:photo_item".to_owned(), 0)
    );
    assert_eq!(
        item_id_meta("MINECRAFT:PHOTO", 3),
        ("minecraft:photo_item".to_owned(), 3)
    );
    assert_eq!(
        item_id_meta("minecraft:wool", 14),
        ("minecraft:red_wool".to_owned(), 0)
    );
    assert_eq!(
        item_id_meta("minecraft:diamond", 0),
        ("minecraft:diamond".to_owned(), 0)
    );
    assert_eq!(
        item_id_meta("anchor:unknown", 5),
        ("anchor:unknown".to_owned(), 5)
    );
}

#[test]
fn a_legacy_numeric_block_item_gets_its_name_and_block_state() {
    let mut wool = item_stack(&[
        ("id", Value::Short(35)),
        ("Damage", Value::Short(14)),
        ("Count", Value::Byte(3)),
        ("Slot", Value::Byte(2)),
    ]);
    assert!(item(&mut wool));
    let red_wool = Compound::from([
        ("name".to_owned(), s("minecraft:red_wool")),
        ("states".to_owned(), Value::Compound(Compound::new())),
        ("version".to_owned(), Value::Int(BLOCK_STATE_VERSION)),
    ]);
    assert_eq!(
        wool,
        item_stack(&[
            ("Name", s("minecraft:red_wool")),
            ("Damage", Value::Short(0)),
            ("Count", Value::Byte(3)),
            ("Slot", Value::Byte(2)),
            ("Block", Value::Compound(red_wool)),
        ])
    );
    // Upgrading again changes nothing.
    assert!(!item(&mut wool));
}

#[test]
fn unknown_and_air_items_and_non_items_stay_as_they_were() {
    for entry in [
        item_stack(&[("id", Value::Short(0)), ("Count", Value::Byte(1))]),
        item_stack(&[("id", Value::Short(30_000)), ("Count", Value::Byte(1))]),
        item_stack(&[("Name", s("anchor:unknown")), ("Count", Value::Byte(1))]),
        item_stack(&[("Name", s("minecraft:photo"))]),
        item_stack(&[
            ("Name", s("minecraft:photo")),
            ("Count", Value::Byte(1)),
            ("Damage", Value::Int(0)),
        ]),
    ] {
        let mut changed = entry.clone();
        assert!(!item(&mut changed));
        assert_eq!(changed, entry);
    }
}

#[test]
fn an_item_counts_as_changed_only_when_one_of_its_keys_changes() {
    // Current, with a meta written again as it was: nothing changes.
    let mut current = item_stack(&[
        ("Name", s("minecraft:diamond")),
        ("Count", Value::Byte(1)),
        ("Damage", Value::Short(0)),
    ]);
    assert!(!item(&mut current));
    // A legacy ID beside a current name goes.
    let mut both = item_stack(&[
        ("Name", s("minecraft:diamond")),
        ("id", Value::Short(264)),
        ("Count", Value::Byte(1)),
    ]);
    assert!(item(&mut both));
    assert_eq!(
        both,
        item_stack(&[("Name", s("minecraft:diamond")), ("Count", Value::Byte(1))])
    );
}

#[test]
fn block_entity_and_entity_nbt_is_upgraded_at_every_depth() {
    let old_flower = || {
        Compound::from([
            ("name".to_owned(), s("minecraft:red_flower")),
            (
                "states".to_owned(),
                Value::Compound(Compound::from([("mapped_type".to_owned(), Value::Int(0))])),
            ),
        ])
    };
    let poppy = state(&old_flower()).unwrap();
    assert_eq!(poppy.get("name"), Some(&s("minecraft:poppy")));
    let photo = || item_stack(&[("Name", s("minecraft:photo")), ("Count", Value::Byte(1))]);
    let photo_item = item_stack(&[
        ("Name", s("minecraft:photo_item")),
        ("Count", Value::Byte(1)),
    ]);
    let shulker = |inner: Compound| {
        item_stack(&[
            ("Name", s("minecraft:diamond")),
            ("Count", Value::Byte(1)),
            (
                "tag",
                Value::Compound(Compound::from([(
                    "Items".to_owned(),
                    Value::List(List::Compound(vec![inner])),
                )])),
            ),
        ])
    };
    let chest = |items: Vec<Compound>, plant: Compound| {
        item_stack(&[
            ("id", s("Chest")),
            ("Items", Value::List(List::Compound(items))),
            ("PlantBlock", Value::Compound(plant)),
            (
                "Nested",
                Value::List(List::List(vec![List::Compound(vec![photo()])])),
            ),
        ])
    };
    let mut nbt_value = chest(vec![photo(), shulker(photo())], old_flower());
    assert_eq!(nbt(&mut nbt_value), 4);
    let mut expected = chest(vec![photo_item.clone(), shulker(photo_item.clone())], poppy);
    expected.insert(
        "Nested".to_owned(),
        Value::List(List::List(vec![List::Compound(vec![photo_item])])),
    );
    assert_eq!(nbt_value, expected);
    assert_eq!(nbt(&mut nbt_value), 0);
}

// The interpreter's rules, on small schemas.

const NONE: Rules<'static> = Rules {
    renamed: None,
    flattened: None,
    added: &[],
    removed: &[],
    renamed_properties: &[],
    remapped_values: &[],
    remapped_states: &[],
};

fn run(
    schemas: &[Schema<'_>],
    version: i32,
    name: &str,
    states: &[(&str, apply::Value)],
) -> (String, States) {
    let mut name = name.to_owned();
    let mut states: States = states
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect();
    apply::upgrade(schemas, version, &mut name, &mut states);
    (name, states)
}

fn st(entries: &[(&str, apply::Value)]) -> States {
    entries
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn vs(s: &str) -> apply::Value {
    apply::Value::String(s.to_owned())
}

#[test]
fn schemas_apply_above_the_state_version_and_after_the_first_of_its_own() {
    static RENAME: [(&str, Rules<'static>); 1] = [(
        "a",
        Rules {
            renamed: Some("b"),
            ..NONE
        },
    )];
    static RENAME_B: [(&str, Rules<'static>); 1] = [(
        "b",
        Rules {
            renamed: Some("c"),
            ..NONE
        },
    )];
    let schemas = [
        Schema {
            version: 10,
            blocks: &RENAME,
        },
        Schema {
            version: 20,
            blocks: &RENAME_B,
        },
        Schema {
            version: 20,
            blocks: &RENAME,
        },
    ];
    assert_eq!(run(&schemas, 0, "a", &[]).0, "c");
    // The single schema of version 10 has applied to a state of version 10.
    assert_eq!(run(&schemas, 10, "a", &[]).0, "b");
    // Of version 20's two, the first has applied; the second still does.
    assert_eq!(run(&schemas, 20, "a", &[]).0, "b");
    assert_eq!(run(&schemas, 20, "b", &[]).0, "b");
    // A state numbered past every schema is upgraded as one of the newest version.
    assert_eq!(run(&schemas, 21, "a", &[]).0, "b");
}

#[test]
fn a_matching_state_rule_replaces_every_other_rule_and_the_first_match_wins() {
    static REMAPS: [Remap<'static>; 2] = [
        Remap {
            old: &[("x", Tag::Int(1)), ("y", Tag::Byte(1))],
            new_name: NewName::Name("specific"),
            new: &[("n", Tag::String("v"))],
            copied: &["z", "missing"],
        },
        Remap {
            old: &[("x", Tag::Int(1))],
            new_name: NewName::Name("general"),
            new: &[],
            copied: &[],
        },
    ];
    static BLOCKS: [(&str, Rules<'static>); 1] = [(
        "a",
        Rules {
            renamed: Some("renamed"),
            added: &[("added", Tag::Byte(0))],
            remapped_states: &REMAPS,
            ..NONE
        },
    )];
    let schemas = [Schema {
        version: 1,
        blocks: &BLOCKS,
    }];
    let int = apply::Value::Int;
    let byte = apply::Value::Byte;
    assert_eq!(
        run(
            &schemas,
            0,
            "a",
            &[("x", int(1)), ("y", byte(1)), ("z", vs("q"))]
        ),
        ("specific".to_owned(), st(&[("n", vs("v")), ("z", vs("q"))]))
    );
    // A byte 1 is not an int 1.
    assert_eq!(
        run(&schemas, 0, "a", &[("x", int(1)), ("y", int(1))]),
        ("general".to_owned(), States::new())
    );
    assert_eq!(
        run(&schemas, 0, "a", &[("x", int(2))]),
        (
            "renamed".to_owned(),
            st(&[("added", byte(0)), ("x", int(2))])
        )
    );
}

#[test]
fn properties_are_added_removed_renamed_and_remapped_by_their_old_names() {
    static VALUES: [(Tag<'static>, Tag<'static>); 2] = [
        (Tag::Int(0), Tag::String("zero")),
        (Tag::Int(0), Tag::String("second match")),
    ];
    static OTHER: [(Tag<'static>, Tag<'static>); 1] = [(Tag::Byte(1), Tag::Byte(0))];
    static BLOCKS: [(&str, Rules<'static>); 1] = [(
        "a",
        Rules {
            added: &[("kept", Tag::Int(9)), ("new", Tag::Int(5))],
            removed: &["gone"],
            renamed_properties: &[("old", "renamed")],
            remapped_values: &[("old", &VALUES), ("other", &OTHER)],
            ..NONE
        },
    )];
    let schemas = [Schema {
        version: 1,
        blocks: &BLOCKS,
    }];
    let int = apply::Value::Int;
    let byte = apply::Value::Byte;
    assert_eq!(
        run(
            &schemas,
            0,
            "a",
            &[
                ("kept", int(1)),
                ("gone", int(1)),
                ("old", int(0)),
                ("other", byte(1))
            ]
        ),
        (
            "a".to_owned(),
            st(&[
                ("kept", int(1)),
                ("new", int(5)),
                ("other", byte(0)),
                ("renamed", vs("zero"))
            ])
        )
    );
    // A value without a remap keeps its value under the new name.
    assert_eq!(
        run(&schemas, 0, "a", &[("old", int(7))]).1,
        st(&[("kept", int(9)), ("new", int(5)), ("renamed", int(7))])
    );
}

#[test]
fn a_flattened_property_names_the_block_only_with_its_expected_type() {
    static REMAPS: [(&str, &str); 1] = [("-1", "minus_one")];
    static BLOCKS: [(&str, Rules<'static>); 1] = [(
        "old",
        Rules {
            flattened: Some(Flatten {
                prefix: "minecraft:",
                property: "kind",
                suffix: "_block",
                kind: Kind::Byte,
                remaps: &REMAPS,
            }),
            ..NONE
        },
    )];
    let schemas = [Schema {
        version: 1,
        blocks: &BLOCKS,
    }];
    let byte = apply::Value::Byte;
    assert_eq!(
        run(&schemas, 0, "old", &[("kind", byte(3)), ("x", byte(1))]),
        ("minecraft:3_block".to_owned(), st(&[("x", byte(1))]))
    );
    // Bytes are signed, as PMMP reads them.
    assert_eq!(
        run(&schemas, 0, "old", &[("kind", byte(255))]).0,
        "minecraft:minus_one_block"
    );
    assert_eq!(
        run(&schemas, 0, "old", &[("kind", apply::Value::Int(3))]),
        ("old".to_owned(), st(&[("kind", apply::Value::Int(3))]))
    );
    assert_eq!(run(&schemas, 0, "old", &[]).0, "old");
}

#[test]
fn states_a_game_numbered_past_the_schemas_get_the_newest_properties() {
    // Palette entries of a real 1.21 world: version 18,186,752 (1.21.130), above the
    // newest schema's 1.21.60.33, without the 1.26 connection and corner properties.
    let entry = |name: &str, states: &[(&str, Value)]| {
        Compound::from([
            ("name".to_owned(), Value::String(name.to_owned())),
            (
                "states".to_owned(),
                Value::Compound(
                    states
                        .iter()
                        .map(|(k, v)| ((*k).to_owned(), v.clone()))
                        .collect(),
                ),
            ),
            ("version".to_owned(), Value::Int(18_186_752)),
        ])
    };
    let fence = state(&entry("minecraft:spruce_fence", &[])).unwrap();
    let Some(Value::Compound(states)) = fence.get("states") else {
        panic!("{fence:?}")
    };
    assert_eq!(
        states.get("minecraft:connection_north"),
        Some(&Value::Byte(0))
    );
    assert!(crate::save::find_state(&fence).is_some(), "{fence:?}");
    let stairs = state(&entry(
        "minecraft:spruce_stairs",
        &[
            ("upside_down_bit", Value::Byte(1)),
            ("weirdo_direction", Value::Int(2)),
        ],
    ))
    .unwrap();
    assert!(crate::save::find_state(&stairs).is_some(), "{stairs:?}");
}
