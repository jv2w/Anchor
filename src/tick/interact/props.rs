//! Reading and changing the Bedrock properties of block states by their meaning: facing, axis,
//! rotation and the plain integer, flag and string properties.
//!
//! A state is a Bedrock state, so a property such as "facing north" is one of several Bedrock
//! properties with a block-specific encoding. The encoding is chosen by the property the state
//! has and, where Bedrock reuses a name with different values (`direction`), by the block's
//! class.

use crate::block::{Class, Face, MAX_PROPERTIES, PropertyValue, State};

/// The property `minecraft:block_face`.
const BLOCK_FACE: &str = "minecraft:block_face";
/// The property `minecraft:cardinal_direction`.
const CARDINAL: &str = "minecraft:cardinal_direction";
/// The part of a copper slab's name that Bedrock puts `double_` in front of for the double slab.
const COPPER_SLAB_SUFFIX: &str = "cut_copper_slab";
/// The property `coral_direction` (wall coral fans).
const CORAL_DIRECTION: &str = "coral_direction";
/// The name prefix of the dead form of coral, after the namespace.
const DEAD_PREFIX: &str = "dead_";
/// The property `direction`.
const DIRECTION: &str = "direction";
/// The end of a double slab's name, after the name of the single slab without its `_slab`.
const DOUBLE_SLAB_SUFFIX: &str = "_double_slab";
/// The property `facing_direction`.
const FACING_DIRECTION: &str = "facing_direction";
/// The degrees of a full turn.
const FULL_TURN_DEGREES: f64 = 360.0;
/// The namespace of the blocks Anchor knows.
const NAMESPACE: &str = "minecraft:";
/// The property `pillar_axis`.
const PILLAR_AXIS: &str = "pillar_axis";
/// The property `portal_axis`: a nether portal's axis, without Y.
const PORTAL_AXIS: &str = "portal_axis";
/// The property `ground_sign_direction`: a rotation in sixteenths of a turn.
pub(super) const ROTATION: &str = "ground_sign_direction";
/// The values of a rotation: sixteenths of a turn.
const ROTATION_STEPS: i32 = 16;
/// The property `torch_facing_direction`.
const TORCH_FACING: &str = "torch_facing_direction";
/// The property `weirdo_direction` (stairs).
const WEIRDO_DIRECTION: &str = "weirdo_direction";

/// A pillar axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tick) enum Axis {
    /// East-west.
    X,
    /// Up-down.
    Y,
    /// North-south.
    Z,
}

impl Axis {
    /// The axis of a face.
    pub(in crate::tick) const fn of(face: Face) -> Self {
        match face {
            Face::Down | Face::Up => Self::Y,
            Face::North | Face::South => Self::Z,
            Face::West | Face::East => Self::X,
        }
    }
}

/// How a block writes a facing into an integer or string property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tick) enum Encoding {
    /// `minecraft:block_face`: every face by name.
    BlockFace,
    /// `minecraft:cardinal_direction`: "north", "south", "west", "east".
    Cardinal,
    /// `facing_direction`: down 0, up 1, north 2, south 3, west 4, east 5.
    Classic,
    /// `coral_direction` west 0, east 1, north 2, south 3.
    Coral,
    /// Doors' `minecraft:cardinal_direction`, a quarter turn clockwise from the facing (a door
    /// facing north is "east").
    DoorCardinal,
    /// End rods: classic with the horizontal faces reversed.
    EndRod,
    /// `direction` or `weirdo_direction` east 0, west 1, south 2, north 3 (trapdoors, stairs).
    FiveMinus,
    /// Stems: classic, with "not attached" (up) written as 0.
    Stem,
    /// `direction` south 0, west 1, north 2, east 3.
    Swne,
    /// `direction` north 0, east 1, south 2, west 3 (cocoa, chemistry tables).
    SwneInverted,
    /// `torch_facing_direction`, whose horizontal names are reversed (MCPE-152036).
    Torch,
}

/// The facing of a state (horizontal or any), whatever Bedrock property holds it.
pub(in crate::tick) fn facing(state: State) -> Option<Face> {
    let (name, encoding) = encoding(state)?;
    facing_in(state, name, encoding)
}

/// The state facing `face`; `None` if the state has no facing or cannot face that way.
pub(in crate::tick) fn with_facing(state: State, face: Face) -> Option<State> {
    let (name, encoding) = encoding(state)?;
    match state.value(name)? {
        PropertyValue::Int(_) => with_int(state, name, encode_int(encoding, face)?),
        PropertyValue::String(_) => with_string(state, name, encode_string(encoding, face)?),
        PropertyValue::Byte(_) => None,
    }
}

/// The facing the property `name` of a state holds in `encoding`; `None` without the property
/// or for a value the encoding does not have.
pub(in crate::tick) fn facing_in(state: State, name: &str, encoding: Encoding) -> Option<Face> {
    match state.value(name)? {
        PropertyValue::Int(v) => int_table(encoding)
            .iter()
            .find(|(_, value)| *value == v)
            .map(|(f, _)| *f)
            // A stem's 1 is not attached too.
            .or((encoding == Encoding::Stem && v == 1).then_some(Face::Up)),
        PropertyValue::String(v) => match encoding {
            Encoding::DoorCardinal => face_of(v).map(rotate_counterclockwise),
            Encoding::Torch => Face::ALL
                .into_iter()
                .find(|f| encode_string(encoding, *f) == Some(v))
                .or((v == "unknown").then_some(Face::Up)),
            _ => face_of(v),
        },
        PropertyValue::Byte(_) => None,
    }
}

/// The property a state keeps its facing in and how it is written; `None` for states without a
/// facing.
fn encoding(state: State) -> Option<(&'static str, Encoding)> {
    for p in state.properties() {
        let found = match p.name() {
            BLOCK_FACE => Encoding::BlockFace,
            CARDINAL if state.is(Class::Door) => Encoding::DoorCardinal,
            CARDINAL => Encoding::Cardinal,
            CORAL_DIRECTION => Encoding::Coral,
            DIRECTION => {
                if state.is(Class::Trapdoor) {
                    Encoding::FiveMinus
                } else if state.is(Class::CocoaBlock) || state.is(Class::ChemistryTable) {
                    Encoding::SwneInverted
                } else {
                    Encoding::Swne
                }
            }
            FACING_DIRECTION => {
                if state.is(Class::EndRod) {
                    Encoding::EndRod
                } else if state.is(Class::Stem) {
                    Encoding::Stem
                } else {
                    Encoding::Classic
                }
            }
            TORCH_FACING => Encoding::Torch,
            WEIRDO_DIRECTION => Encoding::FiveMinus,
            _ => continue,
        };
        return Some((p.name(), found));
    }
    None
}

/// The integer a face is written as, or `None` if the encoding cannot hold it.
fn encode_int(encoding: Encoding, face: Face) -> Option<i32> {
    int_table(encoding)
        .iter()
        .find(|(f, _)| *f == face)
        .map(|(_, v)| *v)
}

/// The string a face is written as, or `None` if the encoding cannot hold it.
fn encode_string(encoding: Encoding, face: Face) -> Option<&'static str> {
    match encoding {
        Encoding::BlockFace => Some(face_name(face)),
        Encoding::Cardinal => Face::HORIZONTAL.contains(&face).then(|| face_name(face)),
        Encoding::DoorCardinal => Face::HORIZONTAL
            .contains(&face)
            .then(|| face_name(rotate_clockwise(face))),
        Encoding::Torch => match face {
            Face::Down => None,
            Face::Up => Some("top"),
            Face::North => Some("south"),
            Face::South => Some("north"),
            Face::West => Some("east"),
            Face::East => Some("west"),
        },
        _ => None,
    }
}

/// The integer values of an integer encoding, by face.
fn int_table(encoding: Encoding) -> &'static [(Face, i32)] {
    use Face::{Down, East, North, South, Up, West};
    match encoding {
        Encoding::BlockFace | Encoding::Cardinal | Encoding::DoorCardinal | Encoding::Torch => &[],
        Encoding::Classic => &[
            (Down, 0),
            (Up, 1),
            (North, 2),
            (South, 3),
            (West, 4),
            (East, 5),
        ],
        Encoding::Coral => &[(West, 0), (East, 1), (North, 2), (South, 3)],
        Encoding::EndRod => &[
            (Down, 0),
            (Up, 1),
            (South, 2),
            (North, 3),
            (East, 4),
            (West, 5),
        ],
        Encoding::FiveMinus => &[(East, 0), (West, 1), (South, 2), (North, 3)],
        Encoding::Stem => &[(Up, 0), (North, 2), (South, 3), (West, 4), (East, 5)],
        Encoding::Swne => &[(South, 0), (West, 1), (North, 2), (East, 3)],
        Encoding::SwneInverted => &[(North, 0), (East, 1), (South, 2), (West, 3)],
    }
}

/// The state with its pillar axis set; `None` without one.
pub(super) fn with_axis(state: State, axis: Axis) -> Option<State> {
    let value = match axis {
        Axis::X => "x",
        Axis::Y => "y",
        Axis::Z => "z",
    };
    if state.value(PILLAR_AXIS).is_some() {
        return with_string(state, PILLAR_AXIS, value);
    }
    with_string(state, PORTAL_AXIS, value)
}

/// The dead form of living coral, with its properties; `None` for dead coral or a block without
/// a dead form.
pub(in crate::tick) fn dead_coral(state: State) -> Option<State> {
    if is_dead_coral(state) {
        return None;
    }
    let name = state.name().strip_prefix(NAMESPACE)?;
    renamed(state, &namespaced(DEAD_PREFIX, name))
}

/// The sixteenth-of-a-turn rotation of a yaw.
pub(super) fn rotation_from_yaw(yaw: f64) -> i32 {
    let steps = f64::from(ROTATION_STEPS);
    let turned = ((yaw + FULL_TURN_DEGREES / 2.0) * steps / FULL_TURN_DEGREES + 0.5).floor();
    // A finite yaw gives a small whole number; an out-of-range one saturates.
    float_to_i32(turned) & (ROTATION_STEPS - 1)
}

/// The horizontal face a yaw looks toward: yaw 0 looks south, 90 west, 180 north, 270 east.
pub(in crate::tick) fn horizontal_facing(yaw: f64) -> Face {
    let mut angle = yaw % FULL_TURN_DEGREES;
    if angle < 0.0 {
        angle += FULL_TURN_DEGREES;
    }
    if angle < 45.0 || angle >= 315.0 {
        Face::South
    } else if angle < 135.0 {
        Face::West
    } else if angle < 225.0 {
        Face::North
    } else {
        Face::East
    }
}

/// The flag property `name`.
#[inline]
pub(in crate::tick) fn flag(state: State, name: &str) -> Option<bool> {
    int(state, name).map(|v| v != 0)
}

/// The integer or flag (byte) property `name` as a number.
#[inline]
pub(in crate::tick) fn int(state: State, name: &str) -> Option<i32> {
    match state.value(name)? {
        PropertyValue::Byte(v) => Some(i32::from(v)),
        PropertyValue::Int(v) => Some(v),
        PropertyValue::String(_) => None,
    }
}

/// The string property `name`.
pub(in crate::tick) fn string(state: State, name: &str) -> Option<&'static str> {
    match state.value(name)? {
        PropertyValue::String(v) => Some(v),
        PropertyValue::Byte(_) | PropertyValue::Int(_) => None,
    }
}

/// The state with the flag property `name` set to `value`.
pub(in crate::tick) fn with_flag(state: State, name: &str, value: bool) -> Option<State> {
    with_int(state, name, i32::from(value))
}

/// The state with the integer or flag property `name` set to `value`, keeping its NBT type;
/// `None` if the state has no such property or value.
pub(in crate::tick) fn with_int(state: State, name: &str, value: i32) -> Option<State> {
    let value = match state.value(name)? {
        PropertyValue::Byte(_) => PropertyValue::Byte(u8::try_from(value).ok()?),
        PropertyValue::Int(_) => PropertyValue::Int(value),
        PropertyValue::String(_) => return None,
    };
    state.with(name, value)
}

/// The state with the string property `name` set to `value`.
pub(in crate::tick) fn with_string(state: State, name: &str, value: &str) -> Option<State> {
    state.with(name, PropertyValue::String(value))
}

/// The state named `name` with exactly the properties of `state` (a dead coral, a double slab);
/// `None` if no such state exists.
pub(in crate::tick) fn renamed(state: State, name: &str) -> Option<State> {
    // `State::find` takes at most the properties a table state has.
    let mut buffer = [("", PropertyValue::Byte(0)); MAX_PROPERTIES];
    let mut count = 0;
    for p in state.properties() {
        *buffer.get_mut(count)? = (p.name(), p.value());
        count += 1;
    }
    State::find(name, buffer.get(..count)?)
}

/// The double form of a single slab state, with its properties; `None` for a state that is no
/// single slab. Bedrock names it `<name>_double_slab`, and puts `double_` in front of
/// `cut_copper_slab` for copper.
pub(super) fn double_slab(state: State) -> Option<State> {
    let name = state.name();
    let double = match name.strip_suffix(COPPER_SLAB_SUFFIX) {
        Some(copper) => format!("{copper}double_{COPPER_SLAB_SUFFIX}"),
        None => format!("{}{DOUBLE_SLAB_SUFFIX}", name.strip_suffix("_slab")?),
    };
    renamed(state, &double)
}

/// Whether coral is dead: Bedrock names the dead forms `dead_<name>`.
pub(in crate::tick) fn is_dead_coral(state: State) -> bool {
    state
        .name()
        .strip_prefix(NAMESPACE)
        .is_some_and(|name| name.starts_with(DEAD_PREFIX))
}

/// Whether a slab state is a double slab: Bedrock names them `*_double_slab`, or
/// `*double_cut_copper_slab`.
pub(super) fn is_double_slab(state: State) -> bool {
    state.name().contains("double")
}

/// The lowercase name of a face.
pub(in crate::tick) const fn face_name(face: Face) -> &'static str {
    match face {
        Face::Down => "down",
        Face::Up => "up",
        Face::North => "north",
        Face::South => "south",
        Face::West => "west",
        Face::East => "east",
    }
}

/// The face of a lowercase name.
pub(in crate::tick) fn face_of(name: &str) -> Option<Face> {
    Face::ALL.into_iter().find(|f| face_name(*f) == name)
}

/// The `multi_face_direction_bits` bit of a face: down 1, up 2, south 4, west 8, north 16, east
/// 32.
pub(in crate::tick) const fn multi_face_bit(face: Face) -> i32 {
    match face {
        Face::Down => 0x01,
        Face::Up => 0x02,
        Face::North => 0x10,
        Face::South => 0x04,
        Face::West => 0x08,
        Face::East => 0x20,
    }
}

/// The `vine_direction_bits` bit of a side: south 1, west 2, north 4, east 8; `None` up and down.
pub(in crate::tick) const fn vine_bit(face: Face) -> Option<i32> {
    match face {
        Face::Down | Face::Up => None,
        Face::North => Some(4),
        Face::South => Some(1),
        Face::West => Some(2),
        Face::East => Some(8),
    }
}

/// The face a quarter turn clockwise from `face` seen from above: north goes east. Up and down
/// stay.
pub(in crate::tick) const fn rotate_clockwise(face: Face) -> Face {
    match face {
        Face::Down | Face::Up => face,
        Face::North => Face::East,
        Face::South => Face::West,
        Face::West => Face::North,
        Face::East => Face::South,
    }
}

/// The face a quarter turn counterclockwise from `face` seen from above: north goes west. Up and
/// down stay.
pub(in crate::tick) const fn rotate_counterclockwise(face: Face) -> Face {
    match face {
        Face::Down | Face::Up => face,
        Face::North => Face::West,
        Face::South => Face::East,
        Face::West => Face::South,
        Face::East => Face::North,
    }
}

/// A whole-number float as an `i32`, saturating outside its range (NaN is 0).
pub(super) fn float_to_i32(v: f64) -> i32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "invariant: the cast saturates and callers pass whole numbers"
    )]
    let i = v as i32;
    i
}

/// The name `minecraft:<prefix><name>`.
fn namespaced(prefix: &str, name: &str) -> String {
    let mut full = String::with_capacity(NAMESPACE.len() + prefix.len() + name.len());
    full.push_str(NAMESPACE);
    full.push_str(prefix);
    full.push_str(name);
    full
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_facing_follows_the_yaw_ranges() {
        assert_eq!(horizontal_facing(0.0), Face::South);
        assert_eq!(horizontal_facing(44.9), Face::South);
        assert_eq!(horizontal_facing(45.0), Face::West);
        assert_eq!(horizontal_facing(90.0), Face::West);
        assert_eq!(horizontal_facing(180.0), Face::North);
        assert_eq!(horizontal_facing(270.0), Face::East);
        assert_eq!(horizontal_facing(-90.0), Face::East);
        assert_eq!(horizontal_facing(315.0), Face::South);
        assert_eq!(horizontal_facing(720.0 + 100.0), Face::West);
    }

    #[test]
    fn rotation_from_yaw_gives_sixteenths_of_a_turn() {
        assert_eq!(rotation_from_yaw(0.0), 8);
        assert_eq!(rotation_from_yaw(30.0), 9);
        assert_eq!(rotation_from_yaw(-180.0), 0);
        assert_eq!(rotation_from_yaw(170.0), 0);
    }

    #[test]
    fn facing_round_trips_through_every_encoding() {
        let mut checked = 0;
        for state in State::all() {
            let Some(face) = facing(state) else {
                continue;
            };
            assert_eq!(with_facing(state, face), Some(state), "{state:?}");
            checked += 1;
        }
        assert!(checked > 1000, "{checked}");
    }

    #[test]
    fn renamed_keeps_the_properties() {
        let water = State::all()
            .find(|s| s.name() == "minecraft:water")
            .unwrap();
        let flowing = renamed(water, "minecraft:flowing_water").unwrap();
        assert_eq!(flowing.name(), "minecraft:flowing_water");
        assert_eq!(flowing.value("liquid_depth"), water.value("liquid_depth"));
    }

    #[test]
    fn a_single_slab_has_the_double_form_bedrock_names() {
        let single = |name: &str| {
            State::all()
                .find(|s| {
                    s.name() == name && string(*s, "minecraft:vertical_half") == Some("bottom")
                })
                .unwrap()
        };
        for (single_name, double_name) in [
            ("minecraft:oak_slab", "minecraft:oak_double_slab"),
            (
                "minecraft:cut_copper_slab",
                "minecraft:double_cut_copper_slab",
            ),
            (
                "minecraft:waxed_exposed_cut_copper_slab",
                "minecraft:waxed_exposed_double_cut_copper_slab",
            ),
        ] {
            let double = double_slab(single(single_name)).unwrap();
            assert_eq!(double.name(), double_name);
            assert!(is_double_slab(double));
        }
    }

    #[test]
    fn rotations_turn_a_quarter_and_undo_each_other() {
        assert_eq!(rotate_clockwise(Face::North), Face::East);
        assert_eq!(rotate_counterclockwise(Face::North), Face::West);
        assert_eq!(rotate_clockwise(Face::Up), Face::Up);
        for f in Face::ALL {
            assert_eq!(rotate_counterclockwise(rotate_clockwise(f)), f);
        }
    }
}
