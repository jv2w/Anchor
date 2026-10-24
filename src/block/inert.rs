//! Inert blocks: the block states of the protocol's reference list that are not
//! Anchor states. A stored block state Anchor does not know that is one of them is shown to
//! clients as itself, by its network ID, and stays put: no rule changes it. The server knows only
//! what it needs to judge players by (BDS's values from `CloudburstMC`'s dump): its collision boxes,
//! its light and its hardness. A player breaks it like a block of its hardness for whichever tool
//! is held, with no drops.

use super::{BreakInfo, PropertyValue, State, ToolType, network_hash};
use crate::math::Aabb;
use crate::nbt::{Compound, Value};

mod table;

/// The physics of an inert block state.
#[derive(Debug)]
struct Profile {
    /// BDS's hardness (negative: it cannot be broken).
    hardness: f64,
    /// The light it gives off.
    light_emission: u8,
    /// How much it reduces light passing through it (BDS's light dampening).
    light_filter: u8,
    /// Its first collision box in [`table::BOXES`].
    boxes_start: u16,
    /// Its collision boxes.
    boxes_len: u8,
    /// Whether its only collision box is the whole cell (PMMP `isSolid`).
    solid: bool,
}

impl Profile {
    const fn new(
        hardness: f64,
        light_emission: u8,
        light_filter: u8,
        boxes_start: u16,
        boxes_len: u8,
        solid: bool,
    ) -> Self {
        Self {
            hardness,
            light_emission,
            light_filter,
            boxes_start,
            boxes_len,
            solid,
        }
    }
}

/// Every tool type: an inert block counts the held tool as the right one, so the break time the
/// server allows is never longer than the client's.
const ANY_TOOL: ToolType = ToolType::from_bits(0b11_1111);

/// An inert block state: one of the reference list's states that is not an Anchor state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Inert(u16);

impl Inert {
    /// The inert state a stored palette entry `{name, states}` names, if any: the network hash
    /// of its name and states is a reference state of that name that is not an Anchor state.
    pub(crate) fn from_nbt(entry: &Compound) -> Option<Self> {
        let Some(Value::String(name)) = entry.get("name") else {
            return None;
        };
        let states = match entry.get("states") {
            Some(Value::Compound(states)) => Some(states),
            None => None,
            Some(_) => return None,
        };
        let typed = |v: &Value| matches!(v, Value::Byte(_) | Value::Int(_) | Value::String(_));
        if !states.is_none_or(|s| s.values().all(typed)) {
            return None;
        }
        // A compound iterates in name order, the order the hash takes.
        let properties = states.into_iter().flatten().filter_map(|(k, v)| {
            let value = match v {
                Value::Byte(b) => PropertyValue::Byte(*b),
                Value::Int(i) => PropertyValue::Int(*i),
                Value::String(s) => PropertyValue::String(s),
                _ => return None,
            };
            Some((k.as_str(), value))
        });
        let inert = Self::from_network_id(network_hash(name, properties))?;
        (inert.name() == name).then_some(inert)
    }

    /// The inert state with this network ID, if any.
    pub(crate) fn from_network_id(id: u32) -> Option<Self> {
        let index = table::STATES.binary_search_by_key(&id, |s| s.0).ok()?;
        Some(Self(u16::try_from(index).ok()?))
    }

    /// The row of the state: its network ID, name index and profile index.
    #[inline]
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: an Inert is only made from an index of the state rows"
    )]
    fn row(self) -> (u32, u16, u16) {
        table::STATES[usize::from(self.0)]
    }

    #[inline]
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: the generated rows index the generated profiles"
    )]
    fn profile(self) -> &'static Profile {
        &table::PROFILES[usize::from(self.row().2)]
    }

    /// The network ID clients know the state by.
    #[inline]
    pub(crate) fn network_id(self) -> u32 {
        self.row().0
    }

    /// The block name, such as `minecraft:moss_block`.
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: the generated rows index the generated names"
    )]
    pub(crate) fn name(self) -> &'static str {
        table::NAMES[usize::from(self.row().1)]
    }

    /// The block-local collision boxes.
    #[expect(
        clippy::indexing_slicing,
        reason = "invariant: the generated profiles index the generated boxes"
    )]
    pub(crate) fn collision_boxes(self) -> &'static [Aabb] {
        let p = self.profile();
        let start = usize::from(p.boxes_start);
        &table::BOXES[start..start + usize::from(p.boxes_len)]
    }

    /// Whether an entity cannot stand in the block: its only collision box is the whole cell.
    pub(crate) fn is_solid(self) -> bool {
        self.profile().solid
    }

    /// The light it gives off, 0 to 15.
    pub(crate) fn light_emission(self) -> u8 {
        self.profile().light_emission
    }

    /// How much it reduces light passing through it, 0 to 15 (as [`State::light_filter`]).
    ///
    /// [`State::light_filter`]: super::State::light_filter
    pub(crate) fn light_filter(self) -> u8 {
        self.profile().light_filter
    }

    /// How it breaks: its hardness, with every tool counted as the right one at harvest level 0
    /// (the server does not know its tool, so it allows the shortest time a tool could take).
    pub(crate) fn break_info(self) -> BreakInfo {
        let hardness = self.profile().hardness;
        BreakInfo::new(hardness, hardness * 5.0, ANY_TOOL, 0, false)
    }

    /// The Anchor state the placement and connection rules read in its place: bedrock (full support on every
    /// face) when it is solid, else a cobweb (no support, no collision). Neither can be replaced:
    /// BDS's data does not say which inert blocks vanilla replaces (seagrass, kelp), so a block
    /// placed on one goes into the cell beside it and the inert block stays.
    pub(crate) fn stand_in(self) -> State {
        if self.is_solid() {
            super::BEDROCK
        } else {
            super::Kind::Cobweb.default_state()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::State;

    const REFERENCE: &str = include_str!("../../xtask/data/reference/blocks.json");

    /// The `{name, states}` palette entry of a reference list entry.
    fn entry(b: &serde_json::Value) -> Compound {
        let states = b["states"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                let (kind, v) = v.as_object().unwrap().iter().next().unwrap();
                let value = match kind.as_str() {
                    "byte" => Value::Byte(u8::try_from(v.as_u64().unwrap()).unwrap()),
                    "int" => Value::Int(i32::try_from(v.as_i64().unwrap()).unwrap()),
                    _ => Value::String(v.as_str().unwrap().to_owned()),
                };
                (k.clone(), value)
            })
            .collect();
        Compound::from([
            (
                "name".to_owned(),
                Value::String(b["name"].as_str().unwrap().to_owned()),
            ),
            ("states".to_owned(), Value::Compound(states)),
            ("version".to_owned(), Value::Int(18_168_865)),
        ])
    }

    /// Every reference state is an Anchor state or inert, never both, and an inert one keeps the
    /// reference network ID: the client is sent the ID BDS sends for the same block.
    #[test]
    fn every_reference_state_is_an_anchor_state_or_inert_with_its_network_id() {
        let reference: Vec<serde_json::Value> = serde_json::from_str(REFERENCE).unwrap();
        let mut inert = 0;
        for b in &reference {
            let nbt = entry(b);
            let id = u32::try_from(b["network_id"].as_u64().unwrap()).unwrap();
            let known = crate::save::find_state(&nbt);
            match Inert::from_nbt(&nbt) {
                Some(s) => {
                    assert!(known.is_none(), "{nbt:?} is both");
                    assert_eq!(s.network_id(), id, "{nbt:?}");
                    assert_eq!(s.name(), b["name"].as_str().unwrap());
                    inert += 1;
                }
                None => assert!(known.is_some(), "{nbt:?} is neither"),
            }
        }
        assert_eq!(inert, table::STATES.len());
        assert!(table::STATES.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(State::all().all(|s| {
            table::STATES
                .binary_search_by_key(&s.network_id(), |r| r.0)
                .is_err()
        }));
    }

    /// Every reference state's network ID is a vanilla one, aliases' among them, so no custom
    /// state may take it.
    #[test]
    fn every_reference_network_id_is_a_vanilla_one() {
        let reference: Vec<serde_json::Value> = serde_json::from_str(REFERENCE).unwrap();
        for b in &reference {
            let id = u32::try_from(b["network_id"].as_u64().unwrap()).unwrap();
            assert!(crate::block::is_vanilla_network_id(id), "{b}");
        }
    }

    /// The blocks of the user's imported world that Anchor does not implement are all inert,
    /// with BDS's values.
    #[test]
    fn imported_vanilla_blocks_are_inert_with_bds_physics() {
        let names = [
            "beehive",
            "bubble_column",
            "cherry_sapling",
            "composter",
            "decorated_pot",
            "dispenser",
            "dripstone_block",
            "dropper",
            "frog_spawn",
            "kelp",
            "lodestone",
            "mangrove_propagule",
            "moss_block",
            "moss_carpet",
            "observer",
            "piston",
            "piston_arm_collision",
            "pointed_dripstone",
            "scaffolding",
            "seagrass",
            "sticky_piston",
            "suspicious_gravel",
            "turtle_egg",
        ];
        for name in names {
            let full = format!("minecraft:{name}");
            assert!(table::NAMES.binary_search(&full.as_str()).is_ok(), "{full}");
        }
        let moss = Inert::from_nbt(&Compound::from([
            (
                "name".to_owned(),
                Value::String("minecraft:moss_block".to_owned()),
            ),
            ("states".to_owned(), Value::Compound(Compound::new())),
        ]))
        .expect("moss block");
        assert!(moss.is_solid());
        assert_eq!(moss.light_filter(), 15);
        assert_eq!(moss.break_info().hardness(), 0.1);
        assert_eq!(moss.collision_boxes().len(), 1);
        let carpet = Inert::from_nbt(&Compound::from([(
            "name".to_owned(),
            Value::String("minecraft:moss_carpet".to_owned()),
        )]))
        .expect("moss carpet");
        assert!(!carpet.is_solid());
        assert_eq!(carpet.light_filter(), 0);
        assert_eq!(carpet.collision_boxes()[0].max.y, 0.0625);
        // Placements read moss as full support and carpet as none; neither is replaced.
        for (s, support) in [
            (moss.stand_in(), super::super::SupportType::Full),
            (carpet.stand_in(), super::super::SupportType::None),
        ] {
            assert!(!s.can_be_replaced(), "{s:?}");
            for face in super::super::Face::ALL {
                assert_eq!(s.support(face), support, "{s:?} {face:?}");
            }
        }
    }

    /// A state of an inert name with a wrong value, a property of the wrong type, or a name that
    /// is no reference block is not inert.
    #[test]
    fn states_outside_the_reference_list_are_not_inert() {
        let kelp = |age: Value| {
            Compound::from([
                (
                    "name".to_owned(),
                    Value::String("minecraft:kelp".to_owned()),
                ),
                (
                    "states".to_owned(),
                    Value::Compound(Compound::from([("kelp_age".to_owned(), age)])),
                ),
            ])
        };
        assert!(Inert::from_nbt(&kelp(Value::Int(3))).is_some());
        assert!(Inert::from_nbt(&kelp(Value::Int(99))).is_none());
        assert!(Inert::from_nbt(&kelp(Value::Short(3))).is_none());
        assert!(Inert::from_nbt(&kelp(Value::Byte(3))).is_none());
        let modded = Compound::from([(
            "name".to_owned(),
            Value::String("example:moss_block".to_owned()),
        )]);
        assert!(Inert::from_nbt(&modded).is_none());
    }
}
