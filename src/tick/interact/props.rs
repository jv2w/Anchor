//! Reading and changing the Bedrock properties of block states by their PMMP meaning: facing,
//! axis, rotation and the plain integer, flag and string properties (PMMP `BlockStateWriter`,
//! `ValueMappings`, `CommonProperties`).
//!
//! A state is a Bedrock state, so a PMMP property such as "facing north" is one of several
//! Bedrock properties with a block-specific encoding. The encoding is chosen by the property the
//! state has and, where Bedrock reuses a name with different values (`direction`), by the PMMP
//! class, as PMMP's serializer does.

use crate::block::{Class, Face, PropertyValue, State};

/// `minecraft:cardinal_direction`.
const CARDINAL: &str = "minecraft:cardinal_direction";
/// `minecraft:block_face`.
const BLOCK_FACE: &str = "minecraft:block_face";
/// `facing_direction`.
const FACING_DIRECTION: &str = "facing_direction";
/// `direction`.
const DIRECTION: &str = "direction";
/// `weirdo_direction` (stairs).
const WEIRDO_DIRECTION: &str = "weirdo_direction";
/// `coral_direction` (wall coral fans).
const CORAL_DIRECTION: &str = "coral_direction";
/// `torch_facing_direction`.
const TORCH_FACING: &str = "torch_facing_direction";
/// `ground_sign_direction`: a rotation in sixteenths of a turn.
pub(super) const ROTATION: &str = "ground_sign_direction";
/// `pillar_axis`.
const PILLAR_AXIS: &str = "pillar_axis";

/// The integer property `name`.
pub(super) fn int(state: State, name: &str) -> Option<i32> {
    match state.value(name)? {
        PropertyValue::Int(v) => Some(v),
        _ => None,
    }
}

/// The flag (byte) property `name`.
pub(super) fn flag(state: State, name: &str) -> Option<bool> {
    match state.value(name)? {
        PropertyValue::Byte(v) => Some(v != 0),
        _ => None,
    }
}

/// The string property `name`.
pub(super) fn string(state: State, name: &str) -> Option<&'static str> {
    match state.value(name)? {
        PropertyValue::String(v) => Some(v),
        _ => None,
    }
}

/// The state with the integer property `name` set to `value`.
pub(super) fn with_int(state: State, name: &str, value: i32) -> Option<State> {
    state.with(name, PropertyValue::Int(value))
}

/// The state with the flag property `name` set to `value`.
pub(super) fn with_flag(state: State, name: &str, value: bool) -> Option<State> {
    state.with(name, PropertyValue::Byte(u8::from(value)))
}

/// The state with the string property `name` set to `value`.
pub(super) fn with_string(state: State, name: &str, value: &str) -> Option<State> {
    state.with(name, PropertyValue::String(value))
}

/// How a block writes a facing into an integer or string property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    /// `minecraft:cardinal_direction`: "north", "south", "west", "east".
    Cardinal,
    /// Doors' `minecraft:cardinal_direction`, a quarter turn clockwise from the facing (PMMP
    /// `CommonProperties::$doorProperties`: a door facing north is "east").
    DoorCardinal,
    /// `minecraft:block_face`: every face by name.
    BlockFace,
    /// `facing_direction`: down 0, up 1, north 2, south 3, west 4, east 5.
    Classic,
    /// End rods: classic with the horizontal faces reversed.
    EndRod,
    /// Stems: classic, with "not attached" (PMMP up) written as 0.
    Stem,
    /// `direction` south 0, west 1, north 2, east 3.
    Swne,
    /// `direction` north 0, east 1, south 2, west 3 (cocoa, chemistry tables).
    SwneInverted,
    /// `direction` or `weirdo_direction` east 0, west 1, south 2, north 3 (trapdoors, stairs).
    FiveMinus,
    /// `coral_direction` west 0, east 1, north 2, south 3.
    Coral,
    /// `torch_facing_direction`, whose horizontal names are reversed (MCPE-152036).
    Torch,
}

/// The property a state keeps its facing in and how it is written; `None` for states without a
/// facing.
fn encoding(state: State) -> Option<(&'static str, Encoding)> {
    for p in state.properties() {
        let found = match p.name() {
            CARDINAL if state.is(Class::Door) => Encoding::DoorCardinal,
            CARDINAL => Encoding::Cardinal,
            BLOCK_FACE => Encoding::BlockFace,
            WEIRDO_DIRECTION => Encoding::FiveMinus,
            CORAL_DIRECTION => Encoding::Coral,
            TORCH_FACING => Encoding::Torch,
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
            _ => continue,
        };
        return Some((p.name(), found));
    }
    None
}

/// The lowercase name of a face.
pub(super) const fn face_name(face: Face) -> &'static str {
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
fn face_of(name: &str) -> Option<Face> {
    Face::ALL.into_iter().find(|f| face_name(*f) == name)
}

/// The integer values of an integer encoding, by face.
fn int_table(encoding: Encoding) -> &'static [(Face, i32)] {
    use Face::{Down, East, North, South, Up, West};
    match encoding {
        Encoding::Classic => &[
            (Down, 0),
            (Up, 1),
            (North, 2),
            (South, 3),
            (West, 4),
            (East, 5),
        ],
        Encoding::EndRod => &[
            (Down, 0),
            (Up, 1),
            (South, 2),
            (North, 3),
            (East, 4),
            (West, 5),
        ],
        Encoding::Stem => &[(Up, 0), (North, 2), (South, 3), (West, 4), (East, 5)],
        Encoding::Swne => &[(South, 0), (West, 1), (North, 2), (East, 3)],
        Encoding::SwneInverted => &[(North, 0), (East, 1), (South, 2), (West, 3)],
        Encoding::FiveMinus => &[(East, 0), (West, 1), (South, 2), (North, 3)],
        Encoding::Coral => &[(West, 0), (East, 1), (North, 2), (South, 3)],
        Encoding::Cardinal | Encoding::DoorCardinal | Encoding::BlockFace | Encoding::Torch => &[],
    }
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
        Encoding::Cardinal => matches!(face, Face::North | Face::South | Face::West | Face::East)
            .then(|| face_name(face)),
        Encoding::DoorCardinal => {
            matches!(face, Face::North | Face::South | Face::West | Face::East)
                .then(|| face_name(rotate_y(face, true)))
        }
        Encoding::BlockFace => Some(face_name(face)),
        Encoding::Torch => match face {
            Face::West => Some("east"),
            Face::South => Some("north"),
            Face::North => Some("south"),
            Face::East => Some("west"),
            Face::Up => Some("top"),
            Face::Down => None,
        },
        _ => None,
    }
}

/// The PMMP facing of a state (horizontal or any), whatever Bedrock property holds it.
pub(super) fn facing(state: State) -> Option<Face> {
    let (name, encoding) = encoding(state)?;
    match state.value(name)? {
        PropertyValue::Int(v) => int_table(encoding)
            .iter()
            .find(|(_, value)| *value == v)
            .map(|(f, _)| *f)
            // PMMP reads a stem's 1 as not attached too.
            .or((encoding == Encoding::Stem && v == 1).then_some(Face::Up)),
        PropertyValue::String(v) => match encoding {
            Encoding::Torch => Face::ALL
                .into_iter()
                .find(|f| encode_string(encoding, *f) == Some(v))
                .or((v == "unknown").then_some(Face::Up)),
            Encoding::DoorCardinal => face_of(v).map(|f| rotate_y(f, false)),
            _ => face_of(v),
        },
        PropertyValue::Byte(_) => None,
    }
}

/// The state facing `face`; `None` if the state has no facing or cannot face that way.
pub(super) fn with_facing(state: State, face: Face) -> Option<State> {
    let (name, encoding) = encoding(state)?;
    match state.value(name)? {
        PropertyValue::Int(_) => with_int(state, name, encode_int(encoding, face)?),
        PropertyValue::String(_) => with_string(state, name, encode_string(encoding, face)?),
        PropertyValue::Byte(_) => None,
    }
}

/// A pillar axis (PMMP `Axis`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Axis {
    /// East-west.
    X,
    /// Up-down.
    Y,
    /// North-south.
    Z,
}

impl Axis {
    /// The axis of a face (PMMP `Facing::axis`).
    pub(super) const fn of(face: Face) -> Self {
        match face {
            Face::Down | Face::Up => Self::Y,
            Face::North | Face::South => Self::Z,
            Face::West | Face::East => Self::X,
        }
    }
}

/// The state with its pillar axis set (PMMP `PillarRotationTrait`); `None` without one.
pub(super) fn with_axis(state: State, axis: Axis) -> Option<State> {
    let value = match axis {
        Axis::X => "x",
        Axis::Y => "y",
        Axis::Z => "z",
    };
    if state.value(PILLAR_AXIS).is_some() {
        return with_string(state, PILLAR_AXIS, value);
    }
    // Nether portals keep an axis in `portal_axis`, without Y.
    with_string(state, "portal_axis", value)
}

/// The sixteenth-of-a-turn rotation of a yaw (PMMP `SignLikeRotationTrait::getRotationFromYaw`).
pub(super) fn rotation_from_yaw(yaw: f64) -> i32 {
    let turned = ((yaw + 180.0) * 16.0 / 360.0 + 0.5).floor();
    // A finite yaw gives a small whole number; an out-of-range one saturates.
    float_to_i32(turned) & 0xf
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

/// The horizontal face a yaw looks toward (PMMP `Entity::getHorizontalFacing`): yaw 0 looks
/// south, 90 west, 180 north, 270 east.
pub(super) fn horizontal_facing(yaw: f64) -> Face {
    let mut angle = yaw % 360.0;
    if angle < 0.0 {
        angle += 360.0;
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

/// The face a quarter turn from `face` seen from above (PMMP `Facing::rotateY`): clockwise north
/// goes east. Up and down stay.
pub(super) const fn rotate_y(face: Face, clockwise: bool) -> Face {
    match (face, clockwise) {
        (Face::North, true) | (Face::South, false) => Face::East,
        (Face::East, true) | (Face::West, false) => Face::South,
        (Face::South, true) | (Face::North, false) => Face::West,
        (Face::West, true) | (Face::East, false) => Face::North,
        (other, _) => other,
    }
}

/// The state named `name` with exactly the properties of `state`, as PMMP copies a block's
/// properties to another type (stripping a log, waxing copper, a dead coral); `None` if no such
/// state exists.
pub(in crate::tick) fn renamed(state: State, name: &str) -> Option<State> {
    let mut buffer = [("", PropertyValue::Byte(0)); 8];
    let mut count = 0;
    for (slot, p) in buffer.iter_mut().zip(state.properties()) {
        *slot = (p.name(), p.value());
        count += 1;
    }
    State::find(name, buffer.get(..count)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_facing_follows_pmmp_yaw_ranges() {
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
    fn rotation_from_yaw_matches_pmmp() {
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
    fn rotate_y_turns_a_quarter() {
        assert_eq!(rotate_y(Face::North, true), Face::East);
        assert_eq!(rotate_y(Face::North, false), Face::West);
        assert_eq!(rotate_y(Face::Up, true), Face::Up);
        for f in Face::ALL {
            assert_eq!(rotate_y(rotate_y(f, true), false), f);
        }
    }
}
