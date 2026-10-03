//! Reading and changing the Bedrock properties of block states the rules use, and the facings
//! PMMP's serializer writes into them (`BlockStateWriter`, `ValueMappings`).

use crate::block::{Face, PropertyValue, State};

/// An integer or boolean property as a number.
pub(super) fn int(s: State, name: &str) -> Option<i32> {
    match s.value(name)? {
        PropertyValue::Byte(b) => Some(i32::from(b)),
        PropertyValue::Int(i) => Some(i),
        PropertyValue::String(_) => None,
    }
}

/// A boolean property.
pub(super) fn flag(s: State, name: &str) -> Option<bool> {
    int(s, name).map(|v| v != 0)
}

/// A string property.
pub(super) fn text(s: State, name: &str) -> Option<&'static str> {
    match s.value(name)? {
        PropertyValue::String(v) => Some(v),
        PropertyValue::Byte(_) | PropertyValue::Int(_) => None,
    }
}

/// `s` with the integer or boolean property `name` set to `v`, keeping its NBT type; `None` if
/// the state has no such property or value.
pub(super) fn with_int(s: State, name: &str, v: i32) -> Option<State> {
    let value = match s.value(name)? {
        PropertyValue::Byte(_) => PropertyValue::Byte(u8::try_from(v).ok()?),
        PropertyValue::Int(_) => PropertyValue::Int(v),
        PropertyValue::String(_) => return None,
    };
    s.with(name, value)
}

/// `s` with the boolean property `name` set.
pub(super) fn with_flag(s: State, name: &str, v: bool) -> Option<State> {
    with_int(s, name, i32::from(v))
}

/// `s` with the string property `name` set.
pub(super) fn with_text(s: State, name: &str, v: &str) -> Option<State> {
    s.with(name, PropertyValue::String(v))
}

/// The state named `name` with the properties of `s` (copper oxidation, dead coral…).
pub(super) fn renamed(s: State, name: &str) -> Option<State> {
    let mut buffer = [("", PropertyValue::Byte(0)); 16];
    let mut n = 0;
    for p in s.properties() {
        let slot = buffer.get_mut(n)?;
        *slot = (p.name(), p.value());
        n += 1;
    }
    State::find(name, buffer.get(..n)?)
}

/// The default state of the block named `name`, if it has no properties or they are all zero.
pub(super) fn plain(name: &str) -> Option<State> {
    State::find(name, &[])
}

/// `facing_direction` as PMMP's `Facing` (down, up, north, south, west, east).
pub(super) fn facing_direction(s: State) -> Option<Face> {
    Some(match int(s, "facing_direction")? {
        0 => Face::Down,
        1 => Face::Up,
        2 => Face::North,
        3 => Face::South,
        4 => Face::West,
        5 => Face::East,
        _ => return None,
    })
}

/// `minecraft:cardinal_direction` as a facing.
pub(super) fn cardinal(s: State) -> Option<Face> {
    face_name(text(s, "minecraft:cardinal_direction")?)
}

/// A facing from its name (`minecraft:block_face`, `minecraft:cardinal_direction`).
pub(super) fn face_name(name: &str) -> Option<Face> {
    Some(match name {
        "down" => Face::Down,
        "up" => Face::Up,
        "north" => Face::North,
        "south" => Face::South,
        "west" => Face::West,
        "east" => Face::East,
        _ => return None,
    })
}

/// The name of a facing (`minecraft:cardinal_direction`).
pub(super) fn name_of(face: Face) -> &'static str {
    match face {
        Face::Down => "down",
        Face::Up => "up",
        Face::North => "north",
        Face::South => "south",
        Face::West => "west",
        Face::East => "east",
    }
}

/// `direction` in PMMP's south-west-north-east order (bells, beds).
pub(super) fn direction_swne(s: State) -> Option<Face> {
    [Face::South, Face::West, Face::North, Face::East]
        .get(usize::try_from(int(s, "direction")?).ok()?)
        .copied()
}

/// `direction` in north-east-south-west order (cocoa: PMMP `horizontalFacingSWNEInverted`).
pub(super) fn direction_nesw(s: State) -> Option<Face> {
    [Face::North, Face::East, Face::South, Face::West]
        .get(usize::try_from(int(s, "direction")?).ok()?)
        .copied()
}

/// A torch's facing: Bedrock writes the horizontal directions the other way round (PMMP
/// `torchFacing`, MCPE-152036); "top" and "unknown" are up.
pub(super) fn torch_facing(s: State) -> Option<Face> {
    Some(match text(s, "torch_facing_direction")? {
        "top" | "unknown" => Face::Up,
        "east" => Face::West,
        "west" => Face::East,
        "north" => Face::South,
        "south" => Face::North,
        _ => return None,
    })
}

/// A lever's facing (PMMP `LeverFacing::getFacing`).
pub(super) fn lever_facing(s: State) -> Option<Face> {
    Some(match text(s, "lever_direction")? {
        "down_east_west" | "down_north_south" => Face::Down,
        "up_east_west" | "up_north_south" => Face::Up,
        other => face_name(other)?,
    })
}

/// A wall coral fan's facing (PMMP `horizontalFacingCoral`).
pub(super) fn coral_facing(s: State) -> Option<Face> {
    [Face::West, Face::East, Face::North, Face::South]
        .get(usize::try_from(int(s, "coral_direction")?).ok()?)
        .copied()
}

/// The faces of a multi-face block (`multi_face_direction_bits`: glow lichen, resin) or of vines
/// (`vine_direction_bits`), in [`Face::ALL`] order.
pub(super) fn faces(s: State) -> Option<Vec<Face>> {
    if let Some(bits) = int(s, "multi_face_direction_bits") {
        // PMMP `MULTI_FACE_DIRECTION_FLAG_*`: down 1, up 2, south 4, west 8, north 16, east 32.
        let bit = |f: Face| match f {
            Face::Down => 1,
            Face::Up => 2,
            Face::South => 4,
            Face::West => 8,
            Face::North => 16,
            Face::East => 32,
        };
        return Some(
            Face::ALL
                .into_iter()
                .filter(|f| bits & bit(*f) != 0)
                .collect(),
        );
    }
    let bits = int(s, "vine_direction_bits")?;
    Some(
        Face::ALL
            .into_iter()
            .filter(|f| vine_bit(*f).is_some_and(|b| bits & b != 0))
            .collect(),
    )
}

/// The `vine_direction_bits` bit of a side (PMMP `VINE_FLAG_*`: south 1, west 2, north 4, east
/// 8).
pub(super) fn vine_bit(face: Face) -> Option<i32> {
    match face {
        Face::South => Some(1),
        Face::West => Some(2),
        Face::North => Some(4),
        Face::East => Some(8),
        Face::Down | Face::Up => None,
    }
}

/// The `multi_face_direction_bits` bit of a face.
pub(super) fn multi_face_bit(face: Face) -> i32 {
    match face {
        Face::Down => 1,
        Face::Up => 2,
        Face::South => 4,
        Face::West => 8,
        Face::North => 16,
        Face::East => 32,
    }
}

/// Whether two faces are on the same axis.
pub(super) fn same_axis(a: Face, b: Face) -> bool {
    a == b || a == b.opposite()
}

/// The horizontal faces in PMMP `Facing::HORIZONTAL` order: north, south, west, east.
pub(super) const HORIZONTAL: [Face; 4] = [Face::North, Face::South, Face::West, Face::East];

#[cfg(test)]
mod tests {
    use super::*;

    fn find(name: &str) -> State {
        State::all().find(|s| s.name() == name).unwrap()
    }

    #[test]
    fn properties_change_with_their_types_kept() {
        let wheat = find("minecraft:wheat");
        assert_eq!(int(wheat, "growth"), Some(0));
        let grown = with_int(wheat, "growth", 7).unwrap();
        assert_eq!(int(grown, "growth"), Some(7));
        assert_eq!(with_int(wheat, "growth", 8), None);
        let leaves = find("minecraft:oak_leaves");
        let checked = with_flag(leaves, "update_bit", true).unwrap();
        assert_eq!(flag(checked, "update_bit"), Some(true));
        let water = find("minecraft:water");
        let flowing = renamed(water, "minecraft:flowing_water").unwrap();
        assert_eq!(flowing.name(), "minecraft:flowing_water");
        assert_eq!(int(flowing, "liquid_depth"), int(water, "liquid_depth"));
    }

    #[test]
    fn facings_follow_pmmp_mappings() {
        let torch = |v: &str| {
            State::all()
                .find(|s| {
                    s.name() == "minecraft:torch" && text(*s, "torch_facing_direction") == Some(v)
                })
                .unwrap()
        };
        assert_eq!(torch_facing(torch("east")), Some(Face::West));
        assert_eq!(torch_facing(torch("top")), Some(Face::Up));
        assert!(same_axis(Face::West, Face::East));
        assert!(!same_axis(Face::West, Face::North));
    }
}
