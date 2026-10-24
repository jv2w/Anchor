//! The facings PMMP's serializer writes into block state properties (`BlockStateWriter`,
//! `ValueMappings`) by the property each block update rule reads; the encodings themselves and
//! the plain property accessors are the interaction rules'.

use crate::block::{Face, State};
pub(super) use crate::tick::interact::props::{
    Axis, face_name, face_of, flag, int, multi_face_bit, string, vine_bit, with_flag, with_int,
    with_string,
};
use crate::tick::interact::props::{Encoding, facing_in};

/// The state of the block named `name`, which has no properties; `None` if it has any.
pub(super) fn plain(name: &str) -> Option<State> {
    State::find(name, &[])
}

/// `facing_direction` as PMMP's `Facing` (down, up, north, south, west, east).
pub(super) fn facing_direction(s: State) -> Option<Face> {
    facing_in(s, "facing_direction", Encoding::Classic)
}

/// `minecraft:cardinal_direction` as a facing.
pub(super) fn cardinal(s: State) -> Option<Face> {
    facing_in(s, "minecraft:cardinal_direction", Encoding::Cardinal)
}

/// `direction` in PMMP's south-west-north-east order (bells, beds).
pub(super) fn direction_swne(s: State) -> Option<Face> {
    facing_in(s, "direction", Encoding::Swne)
}

/// `direction` in north-east-south-west order (cocoa: PMMP `horizontalFacingSWNEInverted`).
pub(super) fn direction_nesw(s: State) -> Option<Face> {
    facing_in(s, "direction", Encoding::SwneInverted)
}

/// A torch's facing (PMMP `torchFacing`).
pub(super) fn torch_facing(s: State) -> Option<Face> {
    facing_in(s, "torch_facing_direction", Encoding::Torch)
}

/// A lever's facing (PMMP `LeverFacing::getFacing`).
pub(super) fn lever_facing(s: State) -> Option<Face> {
    Some(match string(s, "lever_direction")? {
        "down_east_west" | "down_north_south" => Face::Down,
        "up_east_west" | "up_north_south" => Face::Up,
        other => face_of(other)?,
    })
}

/// A wall coral fan's facing (PMMP `horizontalFacingCoral`).
pub(super) fn coral_facing(s: State) -> Option<Face> {
    facing_in(s, "coral_direction", Encoding::Coral)
}

/// The faces of a multi-face block (`multi_face_direction_bits`: glow lichen, resin) or of vines
/// (`vine_direction_bits`), in [`Face::ALL`] order.
pub(super) fn faces(s: State) -> Option<Vec<Face>> {
    if let Some(bits) = int(s, "multi_face_direction_bits") {
        return Some(
            Face::ALL
                .into_iter()
                .filter(|f| bits & multi_face_bit(*f) != 0)
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
    }

    #[test]
    fn facings_follow_pmmp_mappings() {
        let torch = |v: &str| {
            State::all()
                .find(|s| {
                    s.name() == "minecraft:torch" && string(*s, "torch_facing_direction") == Some(v)
                })
                .unwrap()
        };
        assert_eq!(torch_facing(torch("east")), Some(Face::West));
        assert_eq!(torch_facing(torch("top")), Some(Face::Up));
    }
}
