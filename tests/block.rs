//! Block states through the public API (PLAN.md 10.4 "M4 판정식"): the state table against the
//! reference list (network IDs included) and the PMMP dump, and unknown input to the lookups.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anchor::block::{Face, PropertyValue, State, SupportType};
use serde_json::Value as Json;

/// Inputs per unknown-input test.
const RANDOM_INPUTS: usize = 100_000;

/// An owned property value, typed like [`PropertyValue`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Owned {
    Byte(u8),
    Int(i32),
    String(String),
}

impl Owned {
    fn from_value(v: PropertyValue<'_>) -> Self {
        match v {
            PropertyValue::Byte(b) => Self::Byte(b),
            PropertyValue::Int(i) => Self::Int(i),
            PropertyValue::String(s) => Self::String(s.to_owned()),
        }
    }

    fn as_value(&self) -> PropertyValue<'_> {
        match self {
            Self::Byte(b) => PropertyValue::Byte(*b),
            Self::Int(i) => PropertyValue::Int(*i),
            Self::String(s) => PropertyValue::String(s),
        }
    }

    /// Reads a typed JSON value (`{"byte": 1}`, `{"int": 3}`, `{"string": "x"}`).
    fn from_json(json: &Json) -> Self {
        let obj = json.as_object().expect("typed value is an object");
        assert_eq!(obj.len(), 1, "typed value {json} has one type key");
        let (kind, value) = obj.iter().next().expect("one type key");
        match kind.as_str() {
            "byte" => Self::Byte(
                u8::try_from(value.as_i64().expect("byte is a number")).expect("byte in range"),
            ),
            "int" => Self::Int(
                i32::try_from(value.as_i64().expect("int is a number")).expect("int in range"),
            ),
            "string" => Self::String(value.as_str().expect("string value").to_owned()),
            other => panic!("unexpected property type {other}"),
        }
    }
}

/// A block name and its properties sorted by name.
type Key = (String, Vec<(String, Owned)>);

fn key_of(state: State) -> Key {
    let properties = state
        .properties()
        .map(|p| (p.name().to_owned(), Owned::from_value(p.value())))
        .collect();
    (state.name().to_owned(), properties)
}

/// Reads the properties of a JSON `states` object, sorted by name.
fn properties_of(states: &Json) -> Vec<(String, Owned)> {
    let mut properties: Vec<_> = states
        .as_object()
        .expect("states is an object")
        .iter()
        .map(|(k, v)| (k.clone(), Owned::from_json(v)))
        .collect();
    properties.sort();
    properties
}

fn borrowed(properties: &[(String, Owned)]) -> Vec<(&str, PropertyValue<'_>)> {
    properties
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_value()))
        .collect()
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
fn read_json(relative: &str) -> Json {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let bytes = std::fs::read(&path).expect("read a committed data file");
    serde_json::from_slice(&bytes).expect("data file is JSON")
}

/// The reference block list: (name, properties, network ID).
fn reference() -> Vec<(Key, u32)> {
    read_json("xtask/data/reference/blocks.json")
        .as_array()
        .expect("reference list is an array")
        .iter()
        .map(|b| {
            let name = b["name"].as_str().expect("name").to_owned();
            let id = u32::try_from(b["network_id"].as_u64().expect("network_id"))
                .expect("network_id in range");
            ((name, properties_of(&b["states"])), id)
        })
        .collect()
}

/// splitmix64: a small fixed-seed generator for reproducible inputs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).expect("n fits u64")).expect("below n")
    }

    fn u32(&mut self) -> u32 {
        u32::try_from(self.next() >> 32).expect("high half fits u32")
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

// pmmp: BlockStateDictionary::getStates
#[test]
fn every_state_is_a_reference_state_with_its_network_id() {
    let reference: BTreeMap<Key, u32> = reference().into_iter().collect();
    // PMMP's 12,468 states and the 16 of the grindstone, a user exception (PLAN.md 8.4).
    assert_eq!(State::all().len(), 12_484);
    for state in State::all() {
        let key = key_of(state);
        assert_eq!(
            reference.get(&key),
            Some(&state.network_id()),
            "{state:?} is not in the reference list with its network ID"
        );
    }
}

// pmmp: BlockStateDictionary::generateDataFromStateId, BlockStateDictionary::lookupStateIdFromData
#[test]
fn every_state_is_found_by_its_network_id_and_properties() {
    let mut rng = Rng(1);
    for state in State::all() {
        assert_eq!(State::from_network_id(state.network_id()), Some(state));
        let (name, properties) = key_of(state);
        let mut input = borrowed(&properties);
        assert_eq!(State::find(&name, &input), Some(state));
        rng.shuffle(&mut input);
        assert_eq!(
            State::find(&name, &input),
            Some(state),
            "shuffled {input:?}"
        );
    }
}

/// Every state of the PMMP dump has PMMP's values: light, random ticks, the default state of its
/// type, break info, fire, flags, friction, support and its number of collision boxes.
// pmmp: Block, VanillaBlocks, Block::getTypeId, Block::getLightLevel, Block::getLightFilter,
// pmmp: Block::blocksDirectSkyLight, Block::ticksRandomly, Block::getBreakInfo,
// pmmp: BlockBreakInfo::getHardness, BlockBreakInfo::getBlastResistance,
// pmmp: BlockBreakInfo::getToolType, BlockBreakInfo::getToolHarvestLevel,
// pmmp: BlockBreakInfo::isExplosionHarvestable, Block::getFlammability,
// pmmp: Block::getFlameEncouragement, Block::burnsForever, Block::isSolid, Block::isTransparent,
// pmmp: Block::canBeReplaced, Block::canBeFlowedInto, Block::canClimb, Block::hasEntityCollision,
// pmmp: Block::isAffectedBySilkTouch, Block::getModelPositionOffset, Block::getFrictionFactor,
// pmmp: Block::getSupportType, Block::getCollisionBoxes
#[test]
fn every_pmmp_state_finds_its_upgraded_state_with_its_values() {
    let upgrade = read_json("xtask/data/upgrade/blocks/0351_1.26.40_to_1.26.50.json");
    let added = upgrade["addedProperties"]
        .as_object()
        .expect("addedProperties");
    let dump = read_json("xtask/data/pmmp/blocks.json");
    let dump = dump.as_array().expect("dump is an array");
    assert_eq!(dump.len(), 11_420);
    for block in dump {
        let name = block["bedrock"]["name"].as_str().expect("name");
        let mut properties = properties_of(&block["bedrock"]["states"]);
        if let Some(defaults) = added.get(name) {
            properties.extend(properties_of(defaults));
            properties.sort();
        }
        let state = State::find(name, &borrowed(&properties))
            .unwrap_or_else(|| panic!("{name} {properties:?} has no state"));
        let byte = |field: &str| {
            u8::try_from(block[field].as_u64().expect("number")).expect("byte in range")
        };
        assert_eq!(state.light_emission(), byte("light_emission"), "{state:?}");
        assert_eq!(state.light_filter(), byte("light_filter"), "{state:?}");
        assert_eq!(
            Some(state.blocks_direct_sky_light()),
            block["blocks_direct_sky_light"].as_bool(),
            "{state:?}"
        );
        assert_eq!(
            Some(state.ticks_randomly()),
            block["ticks_randomly"].as_bool(),
            "{state:?}"
        );
        // Where PMMP states share a Bedrock state, the state takes one PMMP type's values (the
        // block module documentation); the rest are checked on the state's own type.
        if state.kind().name() != block["type"] {
            continue;
        }
        if block["default"].as_bool() == Some(true) {
            assert_eq!(state.kind().default_state(), state, "{state:?}");
        }
        check_break_and_fire(state, block);
        check_physics(state, block);
    }
}

/// A dumped number.
fn float(v: &Json) -> f64 {
    v.as_f64().expect("number")
}

/// The state has the dumped break info and fire values.
fn check_break_and_fire(state: State, block: &Json) {
    let info = state.break_info();
    let want = &block["break"];
    assert_eq!(info.hardness(), float(&want["hardness"]), "{state:?}");
    assert_eq!(
        info.blast_resistance(),
        float(&want["blast_resistance"]),
        "{state:?}"
    );
    let tool = |field: &str| u8::try_from(want[field].as_u64().expect("number")).expect("byte");
    assert_eq!(info.tool_type().bits(), tool("tool_type"), "{state:?}");
    assert_eq!(info.harvest_level(), tool("harvest_level"), "{state:?}");
    assert_eq!(
        Some(info.is_explosion_harvestable()),
        want["explosion_harvestable"].as_bool(),
        "{state:?}"
    );
    let fire = &block["fire"];
    assert_eq!(
        [
            u64::from(state.flammability()),
            u64::from(state.flame_encouragement())
        ],
        [
            fire[0].as_u64().expect("number"),
            fire[1].as_u64().expect("number")
        ],
        "{state:?}"
    );
    assert_eq!(Some(state.burns_forever()), fire[2].as_bool(), "{state:?}");
}

/// The state has the dumped flags, friction, support and number of collision boxes.
fn check_physics(state: State, block: &Json) {
    let flags: BTreeSet<&str> = block["flags"]
        .as_array()
        .expect("flags")
        .iter()
        .map(|f| f.as_str().expect("flag"))
        .collect();
    for (flag, has) in [
        ("solid", state.is_solid()),
        ("transparent", state.is_transparent()),
        ("replaceable", state.can_be_replaced()),
        ("flowable", state.can_be_flowed_into()),
        ("climbable", state.can_climb()),
        ("entity_collision", state.has_entity_collision()),
        ("silk_touch", state.is_affected_by_silk_touch()),
        ("position_offset", state.has_position_offset()),
    ] {
        assert_eq!(has, flags.contains(flag), "{state:?} {flag}");
    }
    assert_eq!(
        state.friction_factor(),
        float(&block["friction"]),
        "{state:?}"
    );
    let support: Vec<&str> = Face::ALL
        .iter()
        .map(|f| match state.support(*f) {
            SupportType::Full => "full",
            SupportType::Center => "center",
            SupportType::Edge => "edge",
            SupportType::None => "none",
        })
        .collect();
    let want: Vec<&str> = block["support"]
        .as_array()
        .expect("support")
        .iter()
        .map(|s| s.as_str().expect("support type"))
        .collect();
    assert_eq!(support, want, "{state:?}");
    // PMMP's inside-out box (a wall hanging sign's bar) is rebuilt as BDS's bar, so the count
    // stays (m9-facts.md).
    let boxes = block["boxes"].as_array().expect("boxes").len();
    assert_eq!(state.collision_boxes().len(), boxes, "{state:?}");
}

#[test]
fn unknown_network_ids_give_none() {
    let ids: BTreeSet<u32> = State::all().map(State::network_id).collect();
    let table: Vec<u32> = ids.iter().copied().collect();
    let mut rng = Rng(2);
    let mut hits = [0_usize; 2];
    for i in 0..RANDOM_INPUTS {
        // Half fully random, half a table ID with one bit flipped or kept as is.
        let (kind, id) = if i % 2 == 0 {
            (0, rng.u32())
        } else {
            let id = table[rng.below(table.len())];
            let bit = rng.below(33);
            (1, if bit == 32 { id } else { id ^ (1 << bit) })
        };
        match State::from_network_id(id) {
            Some(state) => {
                assert_eq!(state.network_id(), id);
                hits[kind] += 1;
            }
            None => assert!(!ids.contains(&id), "table ID {id:#x} not found"),
        }
    }
    assert_eq!(hits, [0, 1_512]);
}

#[test]
fn unknown_names_and_properties_give_none() {
    // The expected answer: an exact (name, sorted properties) match, with no repeated name.
    let oracle: BTreeMap<Key, State> = State::all().map(|s| (key_of(s), s)).collect();
    let states: Vec<State> = State::all().collect();
    let names: Vec<&str> = states.iter().map(|s| s.name()).collect();
    let property_names: Vec<&str> = states
        .iter()
        .flat_map(|s| s.properties().map(|p| p.name()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let huge = "minecraft:".repeat(100_000);
    let mut rng = Rng(3);
    let mut found = 0_usize;
    for _ in 0..RANDOM_INPUTS {
        let state = states[rng.below(states.len())];
        let (mut name, mut properties) = key_of(state);
        match rng.below(10) {
            0 => {}
            1 => {
                if !properties.is_empty() {
                    properties.remove(rng.below(properties.len()));
                }
            }
            2 => {
                let extra = property_names[rng.below(property_names.len())].to_owned();
                properties.push((extra, Owned::Byte(0)));
            }
            3 => {
                if let Some(p) = properties.first().cloned() {
                    properties.push(p);
                }
            }
            4 => {
                // The same value under another type.
                if !properties.is_empty() {
                    let i = rng.below(properties.len());
                    properties[i].1 = match &properties[i].1 {
                        Owned::Byte(b) => Owned::Int(i32::from(*b)),
                        Owned::Int(v) => Owned::String(v.to_string()),
                        Owned::String(_) => Owned::Byte(0),
                    };
                }
            }
            5 => {
                if !properties.is_empty() {
                    let i = rng.below(properties.len());
                    properties[i].1 = match rng.below(3) {
                        0 => Owned::Byte(u8::try_from(rng.below(256)).expect("byte")),
                        1 => Owned::Int(i32::from_ne_bytes(rng.u32().to_ne_bytes())),
                        _ => Owned::String(names[rng.below(names.len())].to_owned()),
                    };
                }
            }
            6 => {
                name = match rng.below(4) {
                    0 => String::new(),
                    1 => huge.clone(),
                    2 => {
                        let mut bytes = name.into_bytes();
                        let i = rng.below(bytes.len());
                        bytes[i] = b'a' + u8::try_from(rng.below(26)).expect("letter");
                        String::from_utf8(bytes).expect("ASCII stays UTF-8")
                    }
                    _ => names[rng.below(names.len())].to_owned(),
                };
            }
            7 => properties.clear(),
            8 => {
                // More properties than any state has.
                for i in 0..9 + rng.below(32) {
                    properties.push((format!("extra_{i}"), Owned::Int(0)));
                }
            }
            _ => {
                let i = rng.below(properties.len() + 1);
                let value = Owned::String(huge.clone());
                if i == properties.len() {
                    properties.push((huge.clone(), value));
                } else {
                    properties[i].1 = value;
                }
            }
        }
        let mut sorted = properties.clone();
        sorted.sort();
        let repeated = sorted.windows(2).any(|w| w[0].0 == w[1].0);
        let expected = if repeated {
            None
        } else {
            oracle.get(&(name.clone(), sorted)).copied()
        };
        let mut input = borrowed(&properties);
        rng.shuffle(&mut input);
        assert_eq!(
            State::find(&name, &input),
            expected,
            "name of {} bytes, {} properties",
            name.len(),
            input.len()
        );
        found += usize::from(expected.is_some());
    }
    assert_eq!(found, 12_430);
}
