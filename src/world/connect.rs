//! Neighbour connections: after a block is set, the states of the block and of
//! the neighbours that depend on it are worked out again from their neighbours.
//!
//! The rules follow BDS where P6 recorded it (fences, panes, bars, stairs, tripwire) and PMMP's
//! `readStateFromWorld` rules elsewhere (walls, fence gates, what counts as a solid face):
//! - Fences connect to fences of the same group, fence gates and full faces. PMMP connects a
//!   nether brick fence to wooden fences but not the other way (`instanceof static`); the group
//!   makes it symmetric, as in vanilla.
//! - Panes and bars connect to panes and bars, walls and full faces.
//! - Walls connect to walls, fence gates, panes and bars and full faces, always "short" (PMMP
//!   does not make tall connections). The post is up unless the wall runs straight through (two
//!   opposite sides only), as BDS shows (P6), or when the block above is not air (PMMP).
//! - A fence gate is "in wall" when a wall is on its left or right.
//! - Stairs take PMMP's corner rule (outer when the stair behind is perpendicular, inner when the
//!   one in front is).
//! - Tripwire (not in PMMP; BDS P6 and vanilla) connects to tripwire and to tripwire hooks that
//!   face it, and a tripwire set is suspended (BDS sets `suspended_bit` on every placement). A
//!   straight line of at most [`MAX_TRIPWIRE`] tripwire between two hooks facing each other is
//!   attached, the hooks too (`attached_bit`, BDS P6); every other tripwire and hook is not.
//!
//! A neighbour in a chunk that is not loaded reads as air and is not changed. Which blocks are
//! worked out again: the set block, its four horizontal neighbours and the block below (a wall's
//! post depends on the block above), and the horizontal neighbours of a neighbour whose full faces
//! changed (a stair whose corner changed). No rule depends on a state that a second round could
//! change, so this covers every block whose state follows.

use crate::block::{Connection, Facing, PropertyValue, State};

use super::sub_chunk::Entry;
use super::{BlockPos, World};

/// The most tripwire between two hooks that still attaches them (vanilla's 40).
const MAX_TRIPWIRE: usize = 40;

/// A straight line of tripwire and the hooks at its ends that face it.
struct Line {
    wires: Vec<BlockPos>,
    hooks: [Option<BlockPos>; 2],
    /// Both ends have a facing hook and the line is short enough.
    attached: bool,
}

impl World {
    /// Works out the states around `pos` after it was set over `old`. When the old and the new
    /// block look the same to their neighbours (stone over dirt), only the block itself is
    /// worked out.
    pub(super) fn connect_around(&mut self, pos: BlockPos, old: Entry) {
        let placed = match self.reconnect(pos) {
            Some((_, new)) => Some(new),
            None => self.known(Some(pos)),
        };
        if let (Entry::Known(old), Some(placed)) = (old, placed)
            && old.neighbour_view() == placed.neighbour_view()
        {
            return;
        }
        for facing in Facing::ALL {
            let Some(side) = pos.side(facing) else {
                continue;
            };
            if let Some((old, new)) = self.reconnect(side)
                && Facing::ALL
                    .into_iter()
                    .any(|f| old.has_full_face(f) != new.has_full_face(f))
            {
                for f in Facing::ALL {
                    if let Some(next) = side.side(f) {
                        self.reconnect(next);
                    }
                }
            }
        }
        if let Some(below) = pos.up(-1) {
            self.reconnect(below);
        }
    }

    /// Works out which tripwire and hooks are attached after `pos` was set over `old`: the lines
    /// through `pos` and next to it, on both axes. Does nothing unless a tripwire or a hook is
    /// involved.
    pub(super) fn attach_tripwire(&mut self, pos: BlockPos, old: Entry) {
        let involved = |s: Option<State>| {
            s.is_some_and(|s| {
                matches!(
                    s.connection(),
                    Connection::Tripwire | Connection::TripwireHook
                )
            })
        };
        let old = match old {
            Entry::Known(s) => Some(s),
            Entry::Unknown(_) | Entry::Inert(..) => None,
        };
        if !involved(self.known(Some(pos))) && !involved(old) {
            return;
        }
        for axis in [Facing::East, Facing::South] {
            let starts = [Some(pos), pos.side(axis), pos.side(axis.opposite())];
            for start in starts.into_iter().flatten() {
                let Some(line) = self.line(start, axis) else {
                    continue;
                };
                for hook in line.hooks.into_iter().flatten() {
                    self.set_attached(hook, line.attached);
                }
                // A tripwire is attached if the line along either axis attaches it.
                let across = if axis == Facing::East {
                    Facing::South
                } else {
                    Facing::East
                };
                for wire in line.wires {
                    let attached =
                        line.attached || self.line(wire, across).is_some_and(|l| l.attached);
                    self.set_attached(wire, attached);
                }
            }
        }
    }

    /// The line of tripwire through `start` along `axis` (both ways), or `None` if `start` is
    /// neither a tripwire nor a hook facing along the axis. A hook with no tripwire in front of
    /// it is a line of no tripwire.
    fn line(&self, start: BlockPos, axis: Facing) -> Option<Line> {
        let wire = |at: Option<BlockPos>| {
            self.known(at)
                .filter(|s| s.connection() == Connection::Tripwire)
                .and(at)
        };
        let state = self.known(Some(start))?;
        let first = match state.connection() {
            Connection::Tripwire => start,
            Connection::TripwireHook => {
                let f = state
                    .facing()
                    .filter(|f| *f == axis || *f == axis.opposite())?;
                let Some(next) = wire(start.side(f)) else {
                    return Some(Line {
                        wires: Vec::new(),
                        hooks: [Some(start), None],
                        attached: false,
                    });
                };
                next
            }
            _ => return None,
        };
        // Walk each way over at most MAX_TRIPWIRE tripwire: that reaches both ends of any line
        // short enough to be attached, so none of its tripwire and hooks keeps a stale
        // `attached_bit`, and a longer line still counts more than MAX_TRIPWIRE.
        let mut wires = vec![first];
        let mut ends = [first, first];
        for (end, f) in ends.iter_mut().zip([axis, axis.opposite()]) {
            for _ in 0..MAX_TRIPWIRE {
                let Some(next) = wire(end.side(f)) else {
                    break;
                };
                *end = next;
                wires.push(next);
            }
        }
        // A hook just past each end, facing back into the line.
        let hook = |end: BlockPos, f: Facing| {
            let at = end.side(f)?;
            let s = self.known(Some(at))?;
            (s.connection() == Connection::TripwireHook && s.facing() == Some(f.opposite()))
                .then_some(at)
        };
        let [a, b] = ends;
        let hooks = [hook(a, axis), hook(b, axis.opposite())];
        let attached = wires.len() <= MAX_TRIPWIRE && hooks.iter().all(Option::is_some);
        Some(Line {
            wires,
            hooks,
            attached,
        })
    }

    /// Sets `attached_bit` of the tripwire or hook at `pos`, if it has one.
    fn set_attached(&mut self, pos: BlockPos, attached: bool) {
        if let Some(state) = self.known(Some(pos))
            && let Some(new) = state.with("attached_bit", PropertyValue::Byte(u8::from(attached)))
            && new != state
        {
            self.write(pos, Entry::Known(new));
        }
    }

    /// Works out the state at `pos` from its neighbours and writes it if it differs. Returns the
    /// old and new states of a change.
    fn reconnect(&mut self, pos: BlockPos) -> Option<(State, State)> {
        let Entry::Known(state) = self.entry(pos)? else {
            return None;
        };
        let new = self.connected(pos, state)?;
        if new == state {
            return None;
        }
        self.write(pos, Entry::Known(new))?;
        Some((state, new))
    }

    /// The known state at `pos`, or `None` for air, unknown states and unloaded chunks.
    fn known(&self, pos: Option<BlockPos>) -> Option<State> {
        match self.entry(pos?)? {
            Entry::Known(state) => Some(state),
            Entry::Unknown(_) | Entry::Inert(..) => None,
        }
    }

    /// The state `state` takes at `pos` given its neighbours, or `None` if it has no variants.
    fn connected(&self, pos: BlockPos, state: State) -> Option<State> {
        let side = |f: Facing| self.known(pos.side(f));
        // The connection set: a bit per side for which `connects` holds.
        let sides = |connects: &dyn Fn(Facing, State) -> bool| {
            Facing::ALL.into_iter().fold(0usize, |bits, f| {
                let on = side(f).is_some_and(|s| connects(f, s));
                bits | if on { usize::from(f.bit()) } else { 0 }
            })
        };
        // Whether the neighbour toward `f` gives full support back toward `pos`.
        let full = |f: Facing, s: State| s.has_full_face(f.opposite());
        match state.connection() {
            Connection::Fence { group } => state.variant(sides(&|f, s| match s.connection() {
                Connection::Fence { group: g } => g == group,
                Connection::FenceGate => true,
                _ => full(f, s),
            })),
            Connection::Thin => state.variant(sides(&|f, s| {
                matches!(s.connection(), Connection::Thin | Connection::Wall) || full(f, s)
            })),
            Connection::Tripwire => {
                let connected = state.variant(sides(&|f, s| match s.connection() {
                    Connection::Tripwire => true,
                    Connection::TripwireHook => s.facing() == Some(f.opposite()),
                    _ => false,
                }))?;
                connected.with("suspended_bit", PropertyValue::Byte(1))
            }
            Connection::Wall => {
                let short = sides(&|f, s| {
                    matches!(
                        s.connection(),
                        Connection::Wall | Connection::FenceGate | Connection::Thin
                    ) || full(f, s)
                });
                // Each connected side is "short" (1) in the wall's base-3 side number.
                let index = [Facing::North, Facing::East, Facing::South, Facing::West]
                    .into_iter()
                    .fold(0, |n, f| {
                        n * 3 + usize::from(short & usize::from(f.bit()) != 0)
                    });
                let above = self.entry(pos.up(1)?);
                let bits = |a: Facing, b: Facing| usize::from(a.bit() | b.bit());
                let straight = short == bits(Facing::North, Facing::South)
                    || short == bits(Facing::East, Facing::West);
                let post = !straight || above.is_some_and(|e| e != Entry::AIR);
                state.variant(index * 2 + usize::from(post))
            }
            Connection::FenceGate => {
                let facing = state.facing()?;
                let in_wall = [facing.clockwise(), facing.opposite().clockwise()]
                    .into_iter()
                    .any(|f| side(f).is_some_and(|s| s.connection() == Connection::Wall));
                state.variant(usize::from(in_wall))
            }
            Connection::Stair => {
                let facing = state.facing()?;
                let upside_down = state.is_upside_down();
                // The facing of a stair toward `f` that can make a corner with this one.
                let corner = |f: Facing| {
                    let s = side(f)?;
                    let g = s.facing()?;
                    (s.connection() == Connection::Stair
                        && s.is_upside_down() == upside_down
                        && g != facing
                        && g != facing.opposite())
                    .then_some(g)
                };
                let right = facing.clockwise();
                // Variant order: none, inner left, inner right, outer left, outer right.
                let shape = match (corner(facing), corner(facing.opposite())) {
                    (Some(back), _) => 3 + usize::from(back == right),
                    (None, Some(front)) => 1 + usize::from(front == right),
                    (None, None) => 0,
                };
                state.variant(shape)
            }
            Connection::None | Connection::TripwireHook => None,
        }
    }
}
