//! Rails joining their neighbours when placed.
//!
//! A rail's shape is Bedrock `rail_direction`: two connections, each a horizontal face, one of
//! which may ascend a block.

use super::place::block_at;
use super::props;
use crate::block::{Class, Face, State};
use crate::world::{BlockPos, World};

/// The curved shapes of plain rails, `rail_direction` 6 to 9.
const CURVES: [[Link; 2]; 4] = [
    [flat(Face::South), flat(Face::East)],
    [flat(Face::South), flat(Face::West)],
    [flat(Face::North), flat(Face::West)],
    [flat(Face::North), flat(Face::East)],
];

/// The shapes every rail has, by `rail_direction`.
const STRAIGHT: [[Link; 2]; 6] = [
    [flat(Face::North), flat(Face::South)],
    [flat(Face::East), flat(Face::West)],
    [flat(Face::West), up(Face::East)],
    [flat(Face::East), up(Face::West)],
    [flat(Face::South), up(Face::North)],
    [flat(Face::North), up(Face::South)],
];

/// A connection of a rail: toward a face, and whether it goes up a block that way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Link {
    face: Face,
    ascend: bool,
}

/// The blocks that join the rail just placed at `pos` to the neighbouring rails with a free end,
/// turning those toward it, in the order to set them.
pub(super) fn reconnect(world: &World, pos: BlockPos) -> Vec<(BlockPos, State)> {
    let mut out = Vec::new();
    let this = block_at(world, Some(pos));
    if !this.is(Class::BaseRail) {
        return out;
    }
    let mut links = connected(world, pos, this);
    let mut changed = false;
    // At most two links: the loop ends after two joins or a pass without one.
    for _ in 0..2 {
        let mut joined = false;
        for link in possible(this, &links) {
            let Some((other_pos, other, back)) = neighbour(world, pos, link) else {
                continue;
            };
            if !other.is(Class::BaseRail) {
                continue;
            }
            let other_links = connected(world, other_pos, other);
            if other_links.len() >= 2 || !possible(other, &other_links).contains(&back) {
                continue;
            }
            let mut joined_links = other_links;
            joined_links.push(back);
            if let Some(turned) = shaped(other, &joined_links) {
                out.push((other_pos, turned));
            }
            changed = true;
            links.push(link);
            joined = true;
            break;
        }
        if !joined || links.len() >= 2 {
            break;
        }
    }
    if changed && let Some(turned) = shaped(this, &links) {
        out.push((pos, turned));
    }
    out
}

/// The links of the rail at `pos` that a rail answers.
fn connected(world: &World, pos: BlockPos, state: State) -> Vec<Link> {
    let Some(links) = shape(state) else {
        return Vec::new();
    };
    links
        .into_iter()
        .filter(|link| {
            neighbour(world, pos, *link)
                .is_some_and(|(_, other, back)| shape(other).is_some_and(|o| o.contains(&back)))
        })
        .collect()
}

/// The rail a link of the rail at `pos` reaches, and the link it needs back: one up for an
/// ascending link, one down when the neighbour is a lower rail whose slope rises to this one.
fn neighbour(world: &World, pos: BlockPos, link: Link) -> Option<(BlockPos, State, Link)> {
    let side = pos.step(link.face)?;
    let back = link.face.opposite();
    if link.ascend {
        let p = side.step(Face::Up)?;
        return Some((p, block_at(world, Some(p)), flat(back)));
    }
    let s = block_at(world, Some(side));
    if s.is(Class::BaseRail) {
        return Some((side, s, flat(back)));
    }
    let p = side.step(Face::Down)?;
    Some((p, block_at(world, Some(p)), up(back)))
}

/// The links the rail could still make, in the order they are tried.
fn possible(state: State, constraints: &[Link]) -> Vec<Link> {
    match constraints {
        [] => Face::HORIZONTAL
            .into_iter()
            .map(flat)
            .chain(Face::HORIZONTAL.into_iter().map(up))
            .collect(),
        [only] => {
            let opposite = only.face.opposite();
            let mut out = vec![flat(opposite)];
            if !only.ascend {
                out.push(up(opposite));
                if curves(state) {
                    out.extend(
                        Face::HORIZONTAL
                            .into_iter()
                            .filter(|d| *d != only.face && *d != opposite)
                            .map(flat),
                    );
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// The shape of a rail with `links` (one link goes straight through).
fn shaped(state: State, links: &[Link]) -> Option<State> {
    match links {
        [one] => with_shape(state, [*one, flat(one.face.opposite())]),
        [a, b] => with_shape(state, [*a, *b]),
        _ => None,
    }
}

/// The rail state with the shape of `links` (as given or reversed, straight shapes first).
fn with_shape(state: State, links: [Link; 2]) -> Option<State> {
    let reversed = [links[1], links[0]];
    let find = |table: &[[Link; 2]]| {
        table
            .iter()
            .position(|s| *s == links)
            .or_else(|| table.iter().position(|s| *s == reversed))
    };
    let direction = find(&STRAIGHT).or_else(|| {
        if curves(state) {
            find(&CURVES).map(|c| c + STRAIGHT.len())
        } else {
            None
        }
    })?;
    props::with_int(state, "rail_direction", i32::try_from(direction).ok()?)
}

/// The rail's two connections, if it is a rail.
fn shape(state: State) -> Option<[Link; 2]> {
    if !state.is(Class::BaseRail) {
        return None;
    }
    let direction = usize::try_from(props::int(state, "rail_direction")?).ok()?;
    match direction.checked_sub(STRAIGHT.len()) {
        Some(curve) if curves(state) => CURVES.get(curve).copied(),
        Some(_) => None,
        None => STRAIGHT.get(direction).copied(),
    }
}

/// Whether the rail can curve (a plain rail; powered, detector and activator rails cannot).
fn curves(state: State) -> bool {
    state.is(Class::Rail)
}

/// A connection toward `face` on the same level.
const fn flat(face: Face) -> Link {
    Link {
        face,
        ascend: false,
    }
}

/// A connection toward `face` that goes up a block.
const fn up(face: Face) -> Link {
    Link { face, ascend: true }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::block::Kind;
    use crate::world::{Chunk, ChunkPos, Kind as WorldKind};

    fn world() -> World {
        let mut w = World::new("rails", WorldKind::Temporary, 0, BlockPos::new(0, 0, 0));
        for x in -1..=0 {
            for z in -1..=0 {
                w.insert_chunk(ChunkPos::new(x, z), Chunk::new());
            }
        }
        for x in -4..=4 {
            for z in -4..=4 {
                w.set_block(BlockPos::new(x, 0, z), Kind::Stone.default_state())
                    .unwrap();
            }
        }
        w
    }

    fn rail(direction: i32) -> State {
        props::with_int(Kind::Rail.default_state(), "rail_direction", direction).unwrap()
    }

    fn place(w: &mut World, pos: BlockPos, s: State) {
        w.set_block(pos, s).unwrap();
        for (p, s) in reconnect(w, pos) {
            w.set_block(p, s).unwrap();
        }
    }

    #[test]
    fn every_rail_shape_round_trips() {
        for s in State::all().filter(|s| s.is(Class::BaseRail)) {
            let links = shape(s).unwrap();
            assert_eq!(with_shape(s, links), Some(s), "{s:?}");
        }
    }

    #[test]
    fn rails_join_in_lines_and_corners() {
        let mut w = world();
        let at = |x, z| BlockPos::new(x, 1, z);
        // Two rails in a row east-west turn east-west.
        place(&mut w, at(0, 0), rail(0));
        place(&mut w, at(1, 0), rail(0));
        assert_eq!(
            props::int(block_at(&w, Some(at(0, 0))), "rail_direction"),
            Some(1)
        );
        assert_eq!(
            props::int(block_at(&w, Some(at(1, 0))), "rail_direction"),
            Some(1)
        );
        // A rail south of the east end makes a corner there (south-west: 7).
        place(&mut w, at(1, 1), rail(0));
        assert_eq!(
            props::int(block_at(&w, Some(at(1, 0))), "rail_direction"),
            Some(7)
        );
        // A rail one block up to the west makes the west end ascend west (3).
        w.set_block(BlockPos::new(-1, 1, 0), Kind::Stone.default_state())
            .unwrap();
        place(&mut w, BlockPos::new(-1, 2, 0), rail(0));
        assert_eq!(
            props::int(block_at(&w, Some(at(0, 0))), "rail_direction"),
            Some(3)
        );
    }
}
