//! Entity movement (PMMP `Entity::tryChangeMovement`, `move`, `checkObstruction`,
//! `checkBlockIntersections`, `Liquid::getFlowVector`): pure functions of an entity's body and
//! the world's blocks, so the tick runs them on the CPU pool and applies the results in entity
//! order (PLAN.md 5, 10.7). Only `crate::math` arithmetic is used, so the bits do not depend on
//! the platform or the thread.
//!
//! Blocks of chunks that are not loaded, and states Anchor does not know, are full cubes: an
//! entity never moves into a chunk that is not loaded (a projectile sticks at its border).
//! Positions above or below the world height are air, so entities fall out of the world.
//!
//! Projectiles move along a ray that stops at the first block it meets (PMMP `Projectile::move`,
//! `VoxelRayTrace`, `AxisAlignedBB::calculateIntercept`); the entities on the ray are found in the
//! serial phase, where the other entities' places are final.

use super::EPSILON;
use super::kind::Physics;
use crate::block::{self, Class, Face};
use crate::math::{self, Aabb, Vec3, max};
use crate::random::Random;
use crate::world::{BlockPos, Entry, MAX_Y, MIN_Y, World};

/// A motion component at or below this counts as zero (PMMP `Entity::MOTION_THRESHOLD`).
pub(in crate::tick) const MOTION_THRESHOLD: f64 = 0.000_01;
/// What `y_size` keeps of itself per move (PMMP `STEP_CLIP_MULTIPLIER`).
const STEP_CLIP_MULTIPLIER: f64 = 0.4;
/// The inset of the block cells an entity is inside (PMMP `getBlocksAroundWithEntityInsideActions`).
const INSIDE_INSET: f64 = 0.001;
/// The push of flowing liquid per tick (PMMP `checkBlockIntersections`).
const CURRENT: f64 = 0.014;
/// The largest speed component an entity keeps, in blocks per tick: falling reaches 2 (gravity
/// 0.04 against drag 0.02), and a saved or given speed above this is cut, so one move looks at a
/// bounded number of blocks.
pub(super) const MAX_SPEED: f64 = 10.0;
/// The horizontal directions in PMMP's `Facing::HORIZONTAL` order: north, south, west, east.
const HORIZONTAL: [(i32, i32); 4] = [(0, -1), (0, 1), (-1, 0), (1, 0)];
/// The most cells a ray visits: a move of at most [`MAX_SPEED`] per axis crosses at most 33
/// cells, so this never cuts a projectile's ray short.
const MAX_RAY_CELLS: usize = 128;
/// How far a stuck projectile's box reaches for its block (PMMP `onNearbyBlockChange`).
const STUCK_REACH: f64 = 0.001;

/// Where an entity is and how it moves.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(super) struct Body {
    /// The centre of the bottom of the bounding box.
    pub(super) position: Vec3,
    /// Blocks per tick.
    pub(super) motion: Vec3,
    /// Whether the last move ended on a block.
    pub(super) on_ground: bool,
    /// Blocks fallen since it last stood on something.
    pub(super) fall_distance: f64,
    /// PMMP `ySize`: the box's lift after stepping up, shrinking each move.
    pub(super) y_size: f64,
}

crate::tick::checkpoint::fields!(Body {
    position,
    motion,
    on_ground,
    fall_distance,
    y_size,
});

impl Body {
    /// The bounding box (PMMP `recalculateBoundingBox`).
    pub(super) fn bounding_box(&self, physics: &Physics) -> Aabb {
        let half = physics.width / 2.0;
        let p = self.position;
        Aabb::new(
            Vec3::new(p.x - half, p.y + self.y_size, p.z - half),
            Vec3::new(p.x + half, p.y + physics.height + self.y_size, p.z + half),
        )
    }

    /// Whether the entity moves this tick (PMMP `hasMovementUpdate`).
    pub(super) fn has_movement_update(&self, forced: bool) -> bool {
        forced || self.motion != Vec3::ZERO || !self.on_ground
    }
}

/// What a block the entity is inside does to it (PMMP `onEntityInside`), for the serial phase:
/// these change health and fire, which the kind rules decide on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Inside {
    /// Fire or soul fire at this block: damage, then burning for 8 seconds.
    Fire(u8, BlockPos),
    /// Lava, in a block or a cauldron at this block: 4 damage, then burning for 8 seconds.
    Lava(BlockPos),
    /// A cactus at this block: 1 contact damage.
    Cactus(BlockPos),
    /// Water, in a block or (`cauldron`) a cauldron: puts the fire out.
    Water {
        /// In a water cauldron.
        cauldron: bool,
    },
}

/// The input of one entity's step.
#[derive(Debug, Clone, Copy)]
pub(super) struct Input {
    pub(super) body: Body,
    pub(super) physics: Physics,
    /// A block changed nearby: move even at rest (PMMP `forceMovementUpdate`).
    pub(super) forced: bool,
    /// The random stream of this entity this tick.
    pub(super) random: (i64, u64, u64),
    /// The block a projectile is stuck in: it does not move unless that block lets it go.
    pub(super) stuck: Option<BlockPos>,
}

/// A projectile's move this tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Flight {
    /// Where it started.
    pub(super) start: Vec3,
    /// The motion it moved with (its speed when it hits something).
    pub(super) motion: Vec3,
    /// Where it stopped: the first block on the way, or `start + motion`.
    pub(super) end: Vec3,
}

/// Where a projectile's ray met a block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct BlockHit {
    /// The block.
    pub(super) pos: BlockPos,
    /// The point on its box.
    pub(super) point: Vec3,
    /// The face of the box it entered.
    pub(super) face: Face,
}

/// The result of one entity's step.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Step {
    pub(super) body: Body,
    /// The fall distance when the entity hit the ground this tick (PMMP `onHitGround`).
    pub(super) landed: Option<f64>,
    /// The blocks acting on it, in PMMP's cell order (z, x, y).
    pub(super) inside: Vec<Inside>,
    /// A projectile's flight this tick: the serial phase looks for entities on it.
    pub(super) flight: Option<Flight>,
    /// The block a projectile's ray met.
    pub(super) hit: Option<BlockHit>,
    /// A projectile's new yaw and pitch (it turns along its flight while nothing is hit).
    pub(super) rotation: Option<(f64, f64)>,
    /// The block a stuck projectile was stuck in changed and holds it no more.
    pub(super) released: bool,
}

/// One tick of movement of an entity (PMMP `onUpdate` up to `updateMovement`, with the block
/// intersections of `entityBaseTick`).
pub(super) fn step(world: &World, input: &Input) -> Step {
    let physics = &input.physics;
    let mut body = input.body;
    let mut out = Step {
        body,
        landed: None,
        inside: Vec::new(),
        flight: None,
        hit: None,
        rotation: None,
        released: false,
    };
    let mut checked = false;
    let held = match input.stuck {
        Some(pos) if input.forced && !holds(world, pos, body.bounding_box(physics)) => {
            out.released = true;
            false
        }
        Some(_) => true,
        None => false,
    };
    if !held && body.has_movement_update(input.forced) {
        let (seed, tick, key) = input.random;
        let mut random = Random::new(seed, tick, key);
        change_movement(world, physics, &mut body, &mut random);
        let small = |v: f64| if v.abs() <= MOTION_THRESHOLD { 0.0 } else { v };
        body.motion = Vec3::new(
            small(body.motion.x),
            small(body.motion.y),
            small(body.motion.z),
        );
        if body.motion != Vec3::ZERO {
            if physics.ray {
                fly(world, physics, &mut body, &mut out);
            } else {
                travel(world, physics, &mut body, &mut out);
            }
            checked = true;
        }
    }
    if !checked {
        intersect(world, physics, &mut body, &mut out.inside);
    }
    out.body = body;
    out
}

/// What occupies a block cell, for collisions.
#[derive(Debug, Clone, Copy)]
enum Cell {
    /// Above or below the world: nothing.
    Outside,
    /// A known block.
    Block(block::State),
    /// A block only clients know: its collision boxes, nothing else (no liquid flows into it).
    Inert(block::Inert),
    /// A chunk that is not loaded or a state neither Anchor nor clients know: a full cube.
    Wall,
}

impl Cell {
    /// The block-local collision boxes, if the cell is a block.
    fn boxes(self) -> Option<&'static [Aabb]> {
        match self {
            Self::Block(state) => Some(state.collision_boxes()),
            Self::Inert(state) => Some(state.collision_boxes()),
            Self::Outside | Self::Wall => None,
        }
    }
}

/// What occupies the cell (x, y, z).
fn cell(world: &World, x: i32, y: i32, z: i32) -> Cell {
    if !(MIN_Y..=MAX_Y).contains(&y) {
        return Cell::Outside;
    }
    match world.entry(BlockPos::new(x, y, z)) {
        Some(Entry::Known(state)) => Cell::Block(state),
        Some(Entry::Inert(state, _)) => Cell::Inert(state),
        Some(Entry::Unknown(_)) | None => Cell::Wall,
    }
}

/// Whether an entity cannot stand in the cell (PMMP `isSolid`).
fn is_solid(world: &World, x: i32, y: i32, z: i32) -> bool {
    match cell(world, x, y, z) {
        Cell::Outside => false,
        Cell::Block(state) => state.is_solid(),
        Cell::Inert(state) => state.is_solid(),
        Cell::Wall => true,
    }
}

/// Gravity, drag and ground friction (PMMP `tryChangeMovement`, with `checkObstruction` first
/// for kinds that push themselves out of blocks).
fn change_movement(world: &World, physics: &Physics, body: &mut Body, random: &mut Random) {
    if physics.unstuck {
        unstick(world, physics, body, random);
    }
    let mut friction = 1.0 - physics.drag;
    let mut motion_y = body.motion.y;
    if physics.drag_before_gravity {
        motion_y *= friction;
    }
    motion_y -= physics.gravity;
    if !physics.drag_before_gravity {
        motion_y *= friction;
    }
    if body.on_ground {
        let p = body.position;
        let below = BlockPos::containing(Vec3::new(p.x, p.y - 1.0, p.z));
        let below = match cell(world, below.x, below.y, below.z) {
            Cell::Block(state) => state.friction_factor(),
            Cell::Outside | Cell::Inert(_) | Cell::Wall => block::AIR.friction_factor(),
        };
        friction *= below;
    }
    body.motion = Vec3::new(body.motion.x * friction, motion_y, body.motion.z * friction);
}

/// An entity inside a solid block gets a push toward the nearest open side (PMMP
/// `checkObstruction`).
fn unstick(world: &World, physics: &Physics, body: &mut Body, random: &mut Random) {
    let mut boxes = Vec::new();
    collisions(world, body.bounding_box(physics), &mut boxes);
    if boxes.is_empty() {
        return;
    }
    let p = body.position;
    let BlockPos { x, y, z } = BlockPos::containing(p);
    if !is_solid(world, x, y, z) {
        return;
    }
    let (dx, dy, dz) = (p.x - f64::from(x), p.y - f64::from(y), p.z - f64::from(z));
    let open = |ox: i32, oy: i32, oz: i32| {
        !is_solid(
            world,
            x.saturating_add(ox),
            y.saturating_add(oy),
            z.saturating_add(oz),
        )
    };
    // (axis 0 x, 1 y, 2 z; sign), as PMMP picks: the nearest open face, earlier ones on ties.
    let mut direction: Option<(u8, f64)> = None;
    let mut limit = 9999.0;
    for (is_open, distance, face) in [
        (open(-1, 0, 0), dx, (0, -1.0)),
        (open(1, 0, 0), 1.0 - dx, (0, 1.0)),
        (open(0, -1, 0), dy, (1, -1.0)),
        (open(0, 1, 0), 1.0 - dy, (1, 1.0)),
        (open(0, 0, -1), dz, (2, -1.0)),
        (open(0, 0, 1), 1.0 - dz, (2, 1.0)),
    ] {
        if is_open && distance < limit {
            limit = distance;
            direction = Some(face);
        }
    }
    let Some((axis, sign)) = direction else {
        return;
    };
    let force = (random.unit() * 0.2 + 0.1) * sign;
    match axis {
        0 => body.motion.x = force,
        1 => body.motion.y = force,
        _ => body.motion.z = force,
    }
}

/// The collision boxes of the blocks that overlap `area` (PMMP `getBlockCollisionBoxes`), in
/// cell order. Boxes leave their cell only up and down: fences and walls are 1.5 high, a pitcher
/// crop reaches 1/16 below (`collision_boxes_leave_their_cell_only_up_and_down` checks the
/// data), so the cells under and over the area are looked at too.
pub(in crate::tick) fn collisions(world: &World, area: Aabb, out: &mut Vec<Aabb>) {
    out.clear();
    let BlockPos {
        x: x0,
        y: y0,
        z: z0,
    } = BlockPos::containing(area.min);
    let BlockPos {
        x: x1,
        y: y1,
        z: z1,
    } = BlockPos::containing(area.max);
    for z in z0..=z1 {
        for x in x0..=x1 {
            for y in y0.saturating_sub(1)..=y1.saturating_add(1) {
                let at = Vec3::new(f64::from(x), f64::from(y), f64::from(z));
                let cell = cell(world, x, y, z);
                if let Some(boxes) = cell.boxes() {
                    for b in boxes {
                        let b = b.offset(at);
                        if b.overlaps(area, EPSILON) {
                            out.push(b);
                        }
                    }
                } else if matches!(cell, Cell::Wall) {
                    let cube = Aabb::new(at, at + Vec3::new(1.0, 1.0, 1.0));
                    if cube.overlaps(area, EPSILON) {
                        out.push(cube);
                    }
                }
            }
        }
    }
}

/// Moves the box by `d`, axis by axis (y, x, z), each stopped by the first box in the way.
fn clip_move(boxes: &[Aabb], mut moving: Aabb, d: Vec3) -> (Aabb, Vec3) {
    let mut dy = d.y;
    for b in boxes {
        dy = b.clip_y(moving, dy);
    }
    moving = moving.offset(Vec3::new(0.0, dy, 0.0));
    let mut dx = d.x;
    for b in boxes {
        dx = b.clip_x(moving, dx);
    }
    moving = moving.offset(Vec3::new(dx, 0.0, 0.0));
    let mut dz = d.z;
    for b in boxes {
        dz = b.clip_z(moving, dz);
    }
    moving = moving.offset(Vec3::new(0.0, 0.0, dz));
    (moving, Vec3::new(dx, dy, dz))
}

/// Moves the entity by its motion against the blocks (PMMP `Entity::move`): clipping, stepping
/// up, the block intersections, the ground state and the fall.
fn travel(world: &World, physics: &Physics, body: &mut Body, out: &mut Step) {
    let wanted = body.motion;
    body.y_size *= STEP_CLIP_MULTIPLIER;
    let start = body.bounding_box(physics);
    let mut boxes = Vec::new();
    collisions(world, start.stretch(wanted), &mut boxes);
    let (mut moved, mut d) = clip_move(&boxes, start, wanted);
    let falling = body.on_ground || (d.y != wanted.y && wanted.y < 0.0);
    if physics.step_height > 0.0 && falling && (wanted.x != d.x || wanted.z != d.z) {
        let up = Vec3::new(wanted.x, physics.step_height, wanted.z);
        collisions(world, start.stretch(up), &mut boxes);
        let (mut stepped, mut s) = clip_move(&boxes, start, up);
        let mut down = -s.y;
        for b in &boxes {
            down = b.clip_y(stepped, down);
        }
        s.y += down;
        stepped = stepped.offset(Vec3::new(0.0, down, 0.0));
        if d.x * d.x + d.z * d.z < s.x * s.x + s.z * s.z {
            moved = stepped;
            d = s;
            body.y_size += s.y;
        }
    }
    body.position = Vec3::new(
        f64::midpoint(moved.min.x, moved.max.x),
        moved.min.y - body.y_size,
        f64::midpoint(moved.min.z, moved.max.z),
    );
    intersect(world, physics, body, &mut out.inside);
    body.on_ground = d.y != wanted.y && wanted.y < 0.0;
    if d.y < body.fall_distance {
        body.fall_distance -= d.y;
    } else {
        body.fall_distance = 0.0;
    }
    if body.on_ground && body.fall_distance > 0.0 {
        out.landed = Some(body.fall_distance);
        body.fall_distance = 0.0;
    }
    let keep = |w: f64, got: f64, v: f64| if w == got { v } else { 0.0 };
    body.motion = Vec3::new(
        keep(wanted.x, d.x, body.motion.x),
        keep(wanted.y, d.y, body.motion.y),
        keep(wanted.z, d.z, body.motion.z),
    );
}

/// Moves a projectile along its motion up to the first block (PMMP `Projectile::move` without the
/// entities): on a block it stops on the block's box (water is a full block to an ice bomb),
/// lands and stops moving; otherwise it turns along its flight. Then the blocks it is inside act
/// on it.
fn fly(world: &World, physics: &Physics, body: &mut Body, out: &mut Step) {
    let start = body.position;
    let mut end = start + body.motion;
    let mut hit = None;
    cells(start, end, |x, y, z| {
        let pos = BlockPos::new(x, y, z);
        let water = physics.water
            && matches!(cell(world, x, y, z), Cell::Block(state) if state.is(Class::Water));
        let met = if water {
            let at = Vec3::new(f64::from(x), f64::from(y), f64::from(z));
            intercept(Aabb::new(at, at + Vec3::new(1.0, 1.0, 1.0)), start, end)
        } else {
            block_intercept(world, pos, start, end)
        };
        match met {
            Some((point, face)) => {
                end = point;
                hit = Some(BlockHit { pos, point, face });
                true
            }
            None => false,
        }
    });
    body.position = end;
    out.flight = Some(Flight {
        start,
        motion: body.motion,
        end,
    });
    out.hit = hit;
    if hit.is_some() {
        body.on_ground = true;
        body.motion = Vec3::ZERO;
    } else {
        body.on_ground = false;
        out.rotation = Some(heading(body.motion));
    }
    intersect(world, physics, body, &mut out.inside);
}

/// The yaw and pitch of a flight along `motion` in degrees (PMMP `Projectile::move`).
pub(super) fn heading(motion: Vec3) -> (f64, f64) {
    let f = (motion.x * motion.x + motion.z * motion.z).sqrt();
    (
        math::degrees(math::atan2(motion.x, motion.z)),
        math::degrees(math::atan2(motion.y, f)),
    )
}

/// Whether the block at `pos` still holds a projectile with the box `bounds` (PMMP
/// `collidesWithBB` with the box grown by 0.001); a block that is not loaded holds it.
fn holds(world: &World, pos: BlockPos, bounds: Aabb) -> bool {
    let area = bounds.grow(Vec3::new(STUCK_REACH, STUCK_REACH, STUCK_REACH));
    let at = Vec3::new(f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
    let cell = cell(world, pos.x, pos.y, pos.z);
    match cell.boxes() {
        Some(boxes) => boxes.iter().any(|b| b.offset(at).intersects(area)),
        None => matches!(cell, Cell::Wall),
    }
}

/// The nearest point where the segment `from`–`to` enters a collision box of the block at `pos`,
/// and the face (PMMP `Block::calculateIntercept`); a block that is not loaded is a full cube.
pub(super) fn block_intercept(
    world: &World,
    pos: BlockPos,
    from: Vec3,
    to: Vec3,
) -> Option<(Vec3, Face)> {
    let at = Vec3::new(f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
    let cube = [Aabb::new(Vec3::ZERO, Vec3::new(1.0, 1.0, 1.0))];
    let cell = cell(world, pos.x, pos.y, pos.z);
    let boxes: &[Aabb] = match cell.boxes() {
        Some(boxes) => boxes,
        None if matches!(cell, Cell::Wall) => &cube,
        None => return None,
    };
    let mut best: Option<(Vec3, Face, f64)> = None;
    for b in boxes {
        if let Some((point, face)) = intercept(b.offset(at), from, to) {
            let d = point.distance_squared(from);
            if best.is_none_or(|(_, _, bd)| d < bd) {
                best = Some((point, face, d));
            }
        }
    }
    best.map(|(p, f, _)| (p, f))
}

/// The point where the segment `from`–`to` crosses the plane where coordinate `axis` (0 x, 1 y,
/// 2 z) is `v` (PMMP `Vector3::getIntermediateWith*Value`).
fn on_plane(from: Vec3, to: Vec3, axis: u8, value: f64) -> Option<Vec3> {
    let (start, stop) = match axis {
        0 => (from.x, to.x),
        1 => (from.y, to.y),
        _ => (from.z, to.z),
    };
    let span = stop - start;
    if span * span < 0.000_000_1 {
        return None;
    }
    let share = (value - start) / span;
    if !(0.0..=1.0).contains(&share) {
        return None;
    }
    let point = from + (to - from) * share;
    Some(match axis {
        0 => Vec3::new(value, point.y, point.z),
        1 => Vec3::new(point.x, value, point.z),
        _ => Vec3::new(point.x, point.y, value),
    })
}

/// The point nearest `from` where the segment `from`–`to` meets a face of `b`, and that face
/// (PMMP `AxisAlignedBB::calculateIntercept`).
pub(super) fn intercept(b: Aabb, from: Vec3, to: Vec3) -> Option<(Vec3, Face)> {
    let across_x = |v: &Vec3| v.y >= b.min.y && v.y <= b.max.y && v.z >= b.min.z && v.z <= b.max.z;
    let across_y = |v: &Vec3| v.x >= b.min.x && v.x <= b.max.x && v.z >= b.min.z && v.z <= b.max.z;
    let across_z = |v: &Vec3| v.x >= b.min.x && v.x <= b.max.x && v.y >= b.min.y && v.y <= b.max.y;
    let candidates = [
        (Face::West, on_plane(from, to, 0, b.min.x).filter(across_x)),
        (Face::East, on_plane(from, to, 0, b.max.x).filter(across_x)),
        (Face::Down, on_plane(from, to, 1, b.min.y).filter(across_y)),
        (Face::Up, on_plane(from, to, 1, b.max.y).filter(across_y)),
        (Face::North, on_plane(from, to, 2, b.min.z).filter(across_z)),
        (Face::South, on_plane(from, to, 2, b.max.z).filter(across_z)),
    ];
    let mut best: Option<(Vec3, Face, f64)> = None;
    for (face, v) in candidates {
        if let Some(v) = v {
            let d = from.distance_squared(v);
            if best.is_none_or(|(_, _, bd)| d < bd) {
                best = Some((v, face, d));
            }
        }
    }
    best.map(|(v, f, _)| (v, f))
}

/// Visits the cells the segment `start`–`end` passes, from the cell of `start`, each one step on
/// one axis from the last, crossing corners z, then y, then x first (PMMP
/// `VoxelRayTrace::betweenPoints`), until `visit` returns true; at most [`MAX_RAY_CELLS`].
pub(super) fn cells(start: Vec3, end: Vec3, mut visit: impl FnMut(i32, i32, i32) -> bool) {
    let length = start.distance_squared(end).sqrt();
    if !(length > 0.0 && length.is_finite()) {
        return;
    }
    let d = (end - start) * (1.0 / length);
    let sign = |c: f64| {
        if c > 0.0 {
            1
        } else if c < 0.0 {
            -1
        } else {
            0
        }
    };
    let (sx, sy, sz) = (sign(d.x), sign(d.y), sign(d.z));
    let delta = |s: i32, c: f64| if c == 0.0 { 0.0 } else { f64::from(s) / c };
    let (dx, dy, dz) = (delta(sx, d.x), delta(sy, d.y), delta(sz, d.z));
    let (mut tx, mut ty, mut tz) = (
        to_boundary(start.x, d.x),
        to_boundary(start.y, d.y),
        to_boundary(start.z, d.z),
    );
    let BlockPos {
        mut x,
        mut y,
        mut z,
    } = BlockPos::containing(start);
    for _ in 0..MAX_RAY_CELLS {
        if visit(x, y, z) {
            return;
        }
        if tx < ty && tx < tz {
            if tx > length {
                return;
            }
            x = x.saturating_add(sx);
            tx += dx;
        } else if ty < tz {
            if ty > length {
                return;
            }
            y = y.saturating_add(sy);
            ty += dy;
        } else {
            if tz > length {
                return;
            }
            z = z.saturating_add(sz);
            tz += dz;
        }
    }
}

/// The distance along a ray from `s` with direction component `ds` to the next cell boundary
/// (PMMP `rayTraceDistanceToBoundary`).
fn to_boundary(s: f64, ds: f64) -> f64 {
    if ds == 0.0 {
        return f64::INFINITY;
    }
    let (s, ds) = if ds < 0.0 {
        if (-s).floor() == -s {
            return 0.0;
        }
        (-s, -ds)
    } else {
        (s, ds)
    };
    (1.0 - (s - s.floor())) / ds
}

/// Whether a ray from `from` to `to` passes no block box (PMMP `Explosion::getExposure`: a
/// block intercepts the segment); blocks of chunks that are not loaded stop it.
pub(super) fn clear(world: &World, from: Vec3, to: Vec3) -> bool {
    let mut blocked = false;
    cells(from, to, |x, y, z| {
        blocked = block_intercept(world, BlockPos::new(x, y, z), from, to).is_some();
        blocked
    });
    !blocked
}

/// The blocks the entity is inside (PMMP `checkBlockIntersections`): falls reset at once, since
/// a landing in water hurts no one; flowing liquid pushes; the rest goes to `inside`.
fn intersect(world: &World, physics: &Physics, body: &mut Body, inside: &mut Vec<Inside>) {
    let b = body.bounding_box(physics);
    let inset = Vec3::new(INSIDE_INSET, INSIDE_INSET, INSIDE_INSET);
    let BlockPos {
        x: x0,
        y: y0,
        z: z0,
    } = BlockPos::containing(b.min + inset);
    let BlockPos {
        x: x1,
        y: y1,
        z: z1,
    } = BlockPos::containing(b.max - inset);
    let mut push = Vec3::ZERO;
    for z in z0..=z1 {
        for x in x0..=x1 {
            for y in y0..=y1 {
                let Cell::Block(state) = cell(world, x, y, z) else {
                    continue;
                };
                if !state.has_entity_collision() {
                    continue;
                }
                let pos = BlockPos::new(x, y, z);
                if state.is(Class::Fire) {
                    inside.push(Inside::Fire(1, pos));
                } else if state.is(Class::SoulFire) {
                    inside.push(Inside::Fire(2, pos));
                } else if state.is(Class::Lava) {
                    inside.push(Inside::Lava(pos));
                    body.fall_distance = 0.0;
                } else if state.is(Class::LavaCauldron) {
                    inside.push(Inside::Lava(pos));
                } else if state.is(Class::Cactus) {
                    inside.push(Inside::Cactus(pos));
                } else if state.is(Class::Water) {
                    inside.push(Inside::Water { cauldron: false });
                    body.fall_distance = 0.0;
                } else if state.is(Class::WaterCauldron) {
                    inside.push(Inside::Water { cauldron: true });
                } else if state.is(Class::Cobweb)
                    || state.is(Class::Vine)
                    || state.is(Class::CaveVines)
                    || state.is(Class::NetherVines)
                {
                    body.fall_distance = 0.0;
                }
                if physics.currents && state.is(Class::Liquid) {
                    push += flow(world, x, y, z, state);
                }
            }
        }
    }
    if push.length_squared() > 0.0 {
        body.motion += push.normalize() * CURRENT;
    }
}

/// A liquid state's decay and whether it falls (Bedrock `liquid_depth`: decay in the low three
/// bits, falling in bit 3).
fn depth(state: block::State) -> (i32, bool) {
    match state.value("liquid_depth") {
        Some(block::PropertyValue::Int(d)) => (d & 7, d & 8 != 0),
        _ => (0, false),
    }
}

/// Whether two liquid states are the same liquid (PMMP `hasSameTypeId`: still and flowing
/// water are one type).
fn same_liquid(a: block::State, b: block::State) -> bool {
    (a.is(Class::Water) && b.is(Class::Water)) || (a.is(Class::Lava) && b.is(Class::Lava))
}

/// PMMP `getEffectiveFlowDecay`: -1 for anything but the same liquid, 0 for falling liquid.
fn effective_decay(liquid: block::State, cell: Cell) -> i32 {
    match cell {
        Cell::Block(other) if same_liquid(liquid, other) => {
            let (decay, falling) = depth(other);
            if falling { 0 } else { decay }
        }
        _ => -1,
    }
}

/// Whether liquid can flow into the cell (PMMP `Liquid::canFlowInto`).
fn can_flow_into(cell: Cell) -> bool {
    match cell {
        Cell::Block(state) => {
            state.can_be_flowed_into()
                && !(state.is(Class::Liquid) && {
                    let (decay, falling) = depth(state);
                    !falling && decay == 0
                })
        }
        Cell::Outside | Cell::Inert(_) | Cell::Wall => false,
    }
}

/// The direction the liquid at (x, y, z) pushes (PMMP `Liquid::getFlowVector`).
fn flow(world: &World, x: i32, y: i32, z: i32, liquid: block::State) -> Vec3 {
    let (decay, falling) = depth(liquid);
    let decay = if falling { 0 } else { decay };
    let mut v = Vec3::ZERO;
    for (dx, dz) in HORIZONTAL {
        let (sx, sz) = (x.saturating_add(dx), z.saturating_add(dz));
        let side = cell(world, sx, y, sz);
        let mut side_decay = effective_decay(liquid, side);
        let real = if side_decay < 0 {
            let flowable = match side {
                Cell::Block(state) => state.can_be_flowed_into(),
                Cell::Outside => true,
                Cell::Inert(_) | Cell::Wall => false,
            };
            if !flowable {
                continue;
            }
            side_decay = effective_decay(liquid, cell(world, sx, y.saturating_sub(1), sz));
            if side_decay < 0 {
                continue;
            }
            side_decay - (decay - 8)
        } else {
            side_decay - decay
        };
        let real = f64::from(real);
        v += Vec3::new(f64::from(dx) * real, 0.0, f64::from(dz) * real);
    }
    if falling {
        for (dx, dz) in HORIZONTAL {
            let (sx, sz) = (x.saturating_add(dx), z.saturating_add(dz));
            if !can_flow_into(cell(world, sx, y, sz))
                || !can_flow_into(cell(world, sx, y.saturating_add(1), sz))
            {
                v = v.normalize() + Vec3::new(0.0, -6.0, 0.0);
                break;
            }
        }
    }
    v.normalize()
}

/// The largest absolute component a stored speed may have ([`MAX_SPEED`]); not finite is zero.
pub(super) fn bounded(v: Vec3) -> Vec3 {
    let one = |c: f64| {
        if c.is_finite() {
            crate::math::clamp(c, -MAX_SPEED, MAX_SPEED)
        } else {
            0.0
        }
    };
    Vec3::new(one(v.x), one(v.y), one(v.z))
}

/// The fall distance a stored value may have: finite and not negative.
pub(super) fn fall_distance(v: f64) -> f64 {
    if v.is_finite() { max(v, 0.0) } else { 0.0 }
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::kind::{Falling, Kind};
    use super::*;
    use crate::protocol::Rng;
    use crate::world::{Chunk, ChunkPos, Flat, Kind as WorldKind};

    /// A flat world (ground top at y -61, standing height -60) with chunks -2..=2 loaded.
    pub(in crate::tick) fn flat() -> World {
        let mut w = World::new("test", WorldKind::Persistent, 0, BlockPos::new(0, -60, 0));
        for x in -2..=2 {
            for z in -2..=2 {
                let pos = ChunkPos::new(x, z);
                w.insert_chunk(pos, Chunk::generate(&Flat, 0, pos));
            }
        }
        w
    }

    /// The first state of a block name.
    pub(in crate::tick) fn named(name: &str) -> block::State {
        block::State::all().find(|s| s.name() == name).unwrap()
    }

    fn item_physics() -> &'static Physics {
        &super::super::kind::ITEM
    }

    fn input(body: Body, physics: &Physics) -> Input {
        Input {
            body,
            physics: *physics,
            forced: false,
            random: (0, 0, 7),
            stuck: None,
        }
    }

    fn run(world: &World, physics: &Physics, mut body: Body, ticks: usize) -> Body {
        for _ in 0..ticks {
            body = step(world, &input(body, physics)).body;
        }
        body
    }

    #[test]
    fn a_dropped_item_falls_and_comes_to_rest_on_the_ground() {
        let w = flat();
        let start = Body {
            position: Vec3::new(0.5, -55.0, 0.5),
            motion: Vec3::new(0.05, 0.2, -0.03),
            ..Body::default()
        };
        let body = run(&w, item_physics(), start, 200);
        assert!(body.on_ground);
        assert_eq!(body.position.y, -60.0);
        assert_eq!(body.motion, Vec3::ZERO, "friction stops it");
        assert_eq!(body.fall_distance, 0.0);
    }

    #[test]
    fn a_falling_block_reports_its_fall_when_it_lands() {
        let w = flat();
        let falling = Kind::FallingBlock(Falling {
            block: named("minecraft:sand"),
        });
        let mut body = Body {
            position: Vec3::new(0.5, -50.0, 0.5),
            ..Body::default()
        };
        let mut landed = None;
        for _ in 0..100 {
            let s = step(&w, &input(body, falling.physics()));
            body = s.body;
            if s.landed.is_some() {
                landed = s.landed;
                break;
            }
        }
        let fall = landed.expect("it lands");
        assert!((fall - 10.0).abs() < 0.5, "fell about 10 blocks: {fall}");
        assert_eq!(body.position.y, -60.0);
        assert!(body.on_ground);
    }

    #[test]
    fn walls_stop_movement_axis_by_axis_and_unloaded_chunks_are_walls() {
        let mut w = flat();
        w.set_block(BlockPos::new(2, -60, 0), named("minecraft:stone"))
            .unwrap();
        let start = Body {
            position: Vec3::new(1.5, -60.0, 0.5),
            motion: Vec3::new(2.0, 0.0, 0.0),
            on_ground: true,
            ..Body::default()
        };
        let body = step(&w, &input(start, item_physics())).body;
        assert_eq!(body.position.x, 2.0 - 0.125, "stopped at the wall");
        assert_eq!(body.motion.x, 0.0);
        // The edge of the loaded area (chunk 2 ends at x 47).
        let edge = Body {
            position: Vec3::new(47.5, -60.0, 0.5),
            motion: Vec3::new(1.5, 0.0, 0.0),
            on_ground: true,
            ..Body::default()
        };
        let body = step(&w, &input(edge, item_physics())).body;
        assert_eq!(body.position.x, 48.0 - 0.125);
    }

    /// A block only clients know (inert) has BDS's collision boxes: an item comes to rest on
    /// moss carpet (1/16 high), not on a full cube.
    #[test]
    fn an_item_rests_on_the_collision_box_of_an_inert_block() {
        let mut w = flat();
        let carpet = crate::nbt::Compound::from([(
            "name".to_owned(),
            crate::nbt::Value::String("minecraft:moss_carpet".to_owned()),
        )]);
        let entry = w.put_unknown(BlockPos::new(0, -60, 0), carpet).unwrap();
        assert!(matches!(entry, Entry::Inert(..)));
        let start = Body {
            position: Vec3::new(0.5, -58.0, 0.5),
            ..Body::default()
        };
        let body = run(&w, item_physics(), start, 100);
        assert!(body.on_ground);
        assert_eq!(body.position.y, -60.0 + 0.0625);
    }

    #[test]
    fn an_item_inside_a_solid_block_is_pushed_out_toward_the_nearest_open_side() {
        let mut w = flat();
        w.set_block(BlockPos::new(0, -60, 0), named("minecraft:stone"))
            .unwrap();
        let start = Body {
            position: Vec3::new(0.9, -59.7, 0.5),
            on_ground: true,
            ..Body::default()
        };
        // A block placed on a resting entity makes it move (PMMP `onNearbyBlockChange`).
        let forced = Input {
            forced: true,
            ..input(start, item_physics())
        };
        let s = step(&w, &forced);
        assert!(s.body.position.x > 0.9, "pushed east: {:?}", s.body);
        assert_eq!(
            step(&w, &input(start, item_physics())).body,
            start,
            "at rest"
        );
    }

    #[test]
    fn blocks_act_on_entities_inside_them() {
        let mut w = flat();
        for (x, name) in [
            (0, "minecraft:fire"),
            (2, "minecraft:lava"),
            (4, "minecraft:cactus"),
            (6, "minecraft:water"),
            (8, "minecraft:soul_fire"),
        ] {
            w.set_block(BlockPos::new(x, -60, 0), named(name)).unwrap();
        }
        let at = |x: f64| Body {
            position: Vec3::new(x + 0.5, -60.0, 0.5),
            on_ground: true,
            fall_distance: 3.0,
            ..Body::default()
        };
        let inside = |x: f64| step(&w, &input(at(x), item_physics()));
        let cell = |x: i32| BlockPos::new(x, -60, 0);
        assert_eq!(inside(0.0).inside, vec![Inside::Fire(1, cell(0))]);
        let lava = inside(2.0);
        assert_eq!(lava.inside, vec![Inside::Lava(cell(2))]);
        assert_eq!(lava.body.fall_distance, 0.0);
        assert_eq!(inside(4.0).inside, vec![Inside::Cactus(cell(4))]);
        assert_eq!(inside(6.0).inside, vec![Inside::Water { cauldron: false }]);
        assert_eq!(inside(8.0).inside, vec![Inside::Fire(2, cell(8))]);
        assert!(inside(10.0).inside.is_empty());
    }

    #[test]
    fn flowing_water_pushes_items_downstream() {
        let mut w = flat();
        let water = |depth: i32| {
            named("minecraft:flowing_water")
                .with("liquid_depth", block::PropertyValue::Int(depth))
                .unwrap()
        };
        w.set_block(BlockPos::new(0, -60, 0), named("minecraft:water"))
            .unwrap();
        for x in 1..=4 {
            w.set_block(BlockPos::new(x, -60, 0), water(x)).unwrap();
        }
        let start = Body {
            position: Vec3::new(1.5, -60.0, 0.5),
            on_ground: true,
            ..Body::default()
        };
        let body = run(&w, item_physics(), start, 20);
        assert!(body.position.x > 1.6, "carried east: {:?}", body.position);
    }

    #[test]
    fn collision_boxes_leave_their_cell_only_up_and_down() {
        for s in block::State::all() {
            for b in s.collision_boxes() {
                for (lo, hi, bottom, top) in [
                    (b.min.x, b.max.x, 0.0, 1.0),
                    (b.min.y, b.max.y, -1.0, 2.0),
                    (b.min.z, b.max.z, 0.0, 1.0),
                ] {
                    assert!(
                        (bottom..=top).contains(&lo) && (bottom..=top).contains(&hi),
                        "{s:?}"
                    );
                }
            }
        }
    }

    /// Random bodies over a flat world with random blocks.
    fn scene(seed: u64) -> (World, Vec<Input>) {
        let mut w = flat();
        let mut rng = Rng(seed);
        let blocks = [
            "minecraft:stone",
            "minecraft:oak_fence",
            "minecraft:water",
            "minecraft:cactus",
            "minecraft:oak_slab",
            "minecraft:glass_pane",
        ];
        for _ in 0..400 {
            let pos = BlockPos::new(
                i32::try_from(rng.below(40)).unwrap() - 20,
                -60 + i32::try_from(rng.below(4)).unwrap(),
                i32::try_from(rng.below(40)).unwrap() - 20,
            );
            let name = blocks[rng.below(blocks.len())];
            w.set_block(pos, named(name)).unwrap();
        }
        let unit = |rng: &mut Rng| {
            f64::from(u32::try_from(rng.below(1 << 20)).unwrap()) / f64::from(1 << 20)
        };
        let inputs = (0..2000)
            .map(|i| {
                let body = Body {
                    position: Vec3::new(
                        unit(&mut rng) * 40.0 - 20.0,
                        -60.0 + unit(&mut rng) * 8.0,
                        unit(&mut rng) * 40.0 - 20.0,
                    ),
                    motion: Vec3::new(
                        unit(&mut rng) - 0.5,
                        unit(&mut rng) - 0.5,
                        unit(&mut rng) - 0.5,
                    ),
                    on_ground: rng.below(2) == 0,
                    ..Body::default()
                };
                Input {
                    body,
                    physics: *item_physics(),
                    forced: false,
                    random: (1, 2, i),
                    stuck: None,
                }
            })
            .collect();
        (w, inputs)
    }

    /// Every bit of a step's result.
    fn bits(s: &Step) -> (Vec<u64>, Vec<Inside>, Option<u64>) {
        let b = s.body;
        let floats = [
            b.position.x,
            b.position.y,
            b.position.z,
            b.motion.x,
            b.motion.y,
            b.motion.z,
            b.fall_distance,
            b.y_size,
        ];
        (
            floats.iter().map(|f| f.to_bits()).collect(),
            s.inside.clone(),
            s.landed.map(f64::to_bits),
        )
    }

    #[test]
    fn steps_give_the_same_bits_on_one_or_many_threads() {
        let (w, mut inputs) = scene(0xe7_e001);
        let one = crate::runtime::cpu_pool(1).unwrap();
        let many = crate::runtime::cpu_pool(4).unwrap();
        for _ in 0..40 {
            let a = one.map(&inputs, |i| step(&w, i));
            let b = many.map(&inputs, |i| step(&w, i));
            let c: Vec<Step> = inputs.iter().map(|i| step(&w, i)).collect();
            for ((a, b), c) in a.iter().zip(&b).zip(&c) {
                assert_eq!(bits(a), bits(b));
                assert_eq!(bits(a), bits(c));
            }
            for (i, s) in inputs.iter_mut().zip(a) {
                i.body = s.body;
            }
        }
        assert!(one.shutdown() && many.shutdown());
    }
}
