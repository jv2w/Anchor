//! Block updates the server still runs: what a changed block leaves to do around it, the random
//! ticks of custom blocks, and the API's tree growth.
//!
//! No block changes by itself or because a neighbour changed. Each world runs, once a tick and in
//! this order:
//!
//! 1. **Changed blocks**: every block change of the world ([`World::next_changed`]: players, tasks
//!    and the API alike), at most [`MAX_CHANGES_PER_TICK`]; the rest waits for the next tick. A
//!    changed block with a block entity first checks that the entity still belongs to it
//!    ([`block_entity::block_changed`], which closes the windows of a block entity that goes), and
//!    the entities on or in the block and its neighbours see the change
//!    ([`entity::block_changed`]).
//! 2. **Random ticks**: the chunks within `tick-radius` of a player of the world whose view is open
//!    (as for entities) and whose 3 × 3 neighbourhood is loaded and lit, in position order; in each
//!    sub-chunk that is not air, [`RANDOM_TICKS_PER_SUB_CHUNK`] positions from the engine random
//!    stream of the chunk and tick. A custom block ticks if its definition asks for it, through its
//!    behaviour's `on_random_tick`.
//!
//! [`World::next_changed`]: crate::world::World::next_changed

use std::collections::BTreeSet;

use super::{Context, State, block_entity, entity, interact, view};
use crate::block::Face;
use crate::random::{self, Random};
use crate::save::WorldIndex;
use crate::world::{self, BlockPos, ChunkPos, MAX_Y, MIN_Y, SUB_CHUNKS, TreeType, World, tree};

/// The most changed blocks a world follows up in one tick; the rest waits in the world's change
/// feed.
const MAX_CHANGES_PER_TICK: usize = 16_384;
/// The random stream purpose of a random tick.
const RANDOM_TICK: u64 = random::tag("random-tick");
/// The random ticks of each sub-chunk that is not air, every tick: at most 5, as one draw of the
/// stream gives 12 bits for each position.
pub(in crate::tick) const RANDOM_TICKS_PER_SUB_CHUNK: u32 = 3;
/// The random stream purpose of a tree the API grows.
const TREE: u64 = random::tag("tree");

/// What block updates keep between ticks.
#[derive(Debug, Default)]
pub(super) struct Updates {
    /// Random streams opened so far for trees: each growth gets a stream of its own.
    draws: u64,
    /// The chunks [`ticking_chunks`] gave the last world, kept so random ticks do not allocate
    /// every tick: at most the players' discs of the largest `tick-radius`. Neither saved nor
    /// hashed.
    ticking: Vec<ChunkPos>,
}

impl Updates {
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut super::Fnv) {
        let Self { draws, ticking: _ } = self;
        h.u64(*draws);
    }
}

/// Every field but the reused chunk list.
impl super::checkpoint::Codec for Updates {
    fn encode(&self, w: &mut super::checkpoint::Writer) {
        let Self { draws, ticking: _ } = self;
        draws.encode(w);
    }

    fn decode(r: &mut super::checkpoint::Reader<'_>) -> super::checkpoint::Result<Self> {
        use super::checkpoint::Codec;
        Ok(Self {
            draws: Codec::decode(r)?,
            ticking: Vec::new(),
        })
    }
}

impl Context<'_> {
    /// Grows a tree of `kind` from `pos` in `world`, as a sapling (or an azalea, a fungus) there
    /// would: the engine's random numbers, so the same server state and calls grow the same
    /// tree. Nothing changes, and the result is false, where the tree does not fit, `pos` is
    /// outside the world height or a block it needs is in a chunk that is not loaded, or for a
    /// world the server does not have. Like every
    /// direct call it raises no event.
    #[doc(alias = "TreeFactory")]
    #[doc(alias = "TreeFactory::get")]
    pub fn grow_tree(&mut self, world: world::Id, pos: BlockPos, kind: TreeType) -> bool {
        let Some(index) = super::world_api::index(self.state, world) else {
            return false;
        };
        let updates = &mut self.state.rules.updates;
        let draw = updates.draws;
        updates.draws = updates.draws.wrapping_add(1);
        let Some(w) = self.state.worlds.get_mut(index) else {
            return false;
        };
        if w.entry(pos).is_none() {
            return false;
        }
        let key = random::key(
            TREE,
            &[
                index as u64,
                u64::from(pos.x.cast_unsigned()),
                u64::from(pos.y.cast_unsigned()),
                u64::from(pos.z.cast_unsigned()),
                draw,
            ],
        );
        let mut rng = Random::new(w.seed(), self.state.tick, key);
        let Some(blocks) = tree::shape(w, pos, kind, &mut rng) else {
            return false;
        };
        if blocks.keys().any(|p| w.entry(*p).is_none()) {
            return false;
        }
        let mut changed = false;
        for (pos, s) in blocks {
            if w.block(pos) != Some(s) && w.set_block(pos, s).is_ok() {
                changed = true;
            }
        }
        changed
    }
}

/// Runs one tick of block updates: the follow-up of the blocks changed since the last tick, then
/// random ticks.
///
/// Everything runs on the tick thread, in the serial step, so the hooks run where they change
/// blocks: handing the candidate positions to the CPU pool would not save more than it costs, and
/// hooks change the blocks later candidates read.
pub(super) fn tick(state: &mut State) {
    // The players' chunks, gathered in one pass for every world rather than one pass per world.
    // No update moves a player or changes its world.
    let centers = view::tick_centres(state);
    for world in 0..state.worlds.len() {
        if !state.is_awake(world) {
            continue;
        }
        run_changes(state, world);
        run_random(state, world, centers.get(&world));
    }
}

/// Follows up the blocks changed in `world`, oldest first.
fn run_changes(state: &mut State, world: WorldIndex) {
    for _ in 0..MAX_CHANGES_PER_TICK {
        let Some(pos) = state.worlds.get_mut(world).and_then(World::next_changed) else {
            break;
        };
        let takes_entity = state
            .worlds
            .get(world)
            .and_then(|w| w.block(pos))
            .is_some_and(block_entity::takes_entity);
        if takes_entity || state.rules.block_entities.get(world, pos).is_some() {
            block_entity::block_changed(state, world, pos);
        }
        let around = std::iter::once(Some(pos)).chain(Face::all().iter().map(|&f| pos.step(f)));
        for p in around.flatten() {
            if (MIN_Y..=MAX_Y).contains(&p.y) {
                entity::block_changed(state, world, p);
            }
        }
    }
}

/// Runs the random ticks of `world` around `centers`, the chunks its players stand in.
fn run_random(state: &mut State, world: WorldIndex, centers: Option<&BTreeSet<ChunkPos>>) {
    let radius = state.settings.view(world).tick_radius;
    if radius == 0 {
        return;
    }
    let Some(world_seed) = state.worlds.get(world).map(World::seed) else {
        return;
    };
    let mut chunks = std::mem::take(&mut state.rules.updates.ticking);
    ticking_chunks(state, world, radius, centers, &mut chunks);
    for &chunk in &chunks {
        let k = random::key(
            RANDOM_TICK,
            &[
                world as u64,
                u64::from(chunk.x.cast_unsigned()),
                u64::from(chunk.z.cast_unsigned()),
            ],
        );
        let mut rng = Random::new(world_seed, state.tick, k);
        for (index, base_y) in (0..SUB_CHUNKS).zip((MIN_Y..).step_by(16)) {
            let filled = state
                .worlds
                .get(world)
                .and_then(|w| w.chunk(chunk))
                .is_some_and(|c| c.sub_chunk(index).is_some());
            if !filled {
                continue;
            }
            let mut bits = rng.next_u64();
            for _ in 0..RANDOM_TICKS_PER_SUB_CHUNK {
                let pos = random_position(chunk, base_y, bits);
                bits >>= 12;
                let block = state.worlds.get(world).and_then(|w| w.block(pos));
                if let Some(block) = block
                    && block
                        .custom_block()
                        .is_some_and(crate::custom::Block::has_random_ticks)
                {
                    interact::random_tick(state, world, pos, block);
                }
            }
        }
    }
    state.rules.updates.ticking = chunks;
}

/// Picks the position 12 random bits give in a sub-chunk: x, then y, then z, 4 bits each.
fn random_position(chunk: ChunkPos, base_y: i32, bits: u64) -> BlockPos {
    #[expect(clippy::expect_used, reason = "invariant: four bits fit i32")]
    let at = |shift: u32| i32::try_from((bits >> shift) & 15).expect("four bits fit i32");
    BlockPos::new(
        chunk.x.wrapping_mul(16).wrapping_add(at(0)),
        base_y + at(4),
        chunk.z.wrapping_mul(16).wrapping_add(at(8)),
    )
}

/// Fills `chunks` with the chunks of `world` that take random ticks, in order: within `radius`
/// of `centers` ([`view::Disc::around`]), the chunks of the players in the world whose view is
/// open ([`view::tick_centres`]), with their 3 × 3 neighbourhood loaded and lit. Players in one
/// chunk share a center.
fn ticking_chunks(
    state: &State,
    world: WorldIndex,
    radius: u8,
    centers: Option<&BTreeSet<ChunkPos>>,
    chunks: &mut Vec<ChunkPos>,
) {
    chunks.clear();
    let (Some(w), Some(centers)) = (state.worlds.get(world), centers) else {
        return;
    };
    for &c in centers {
        chunks.extend(state.chunks.disc.around(c, radius));
    }
    chunks.sort_unstable();
    chunks.dedup();
    // A chunk that is not loaded is not lit.
    chunks.retain(|c| {
        (-1..=1).all(|dx| {
            (-1..=1)
                .all(|dz| w.is_chunk_lit(ChunkPos::new(c.x.wrapping_add(dx), c.z.wrapping_add(dz))))
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use crate::block::{Kind, PropertyValue};
    use crate::event::Listeners;
    use crate::pool::Pool;
    use crate::tick::queue::Input;
    use crate::tick::tests::{answer, join, new_state, radius, settings};
    use crate::tick::{InputLog, Output, Phase, boundary, deliver, step};

    /// Lights the block on a random tick.
    struct Glow;

    impl crate::custom::Behaviour for Glow {
        fn on_random_tick(
            &self,
            cx: &mut Context<'_>,
            world: world::Id,
            pos: BlockPos,
            state: crate::block::State,
        ) {
            let lit = state.with("lit", PropertyValue::Byte(1)).unwrap();
            cx.world_mut(world).unwrap().set_block(pos, lit).unwrap();
        }
    }

    /// A custom block that asks for random ticks.
    static TICKING: crate::custom::Block = crate::custom::Block::new("test:ticking", "t")
        .with_properties(&[crate::custom::Property::bool("lit")])
        .with_behaviour(&Glow)
        .random_ticks();
    /// A custom block that does not.
    static STILL: crate::custom::Block = crate::custom::Block::new("test:still", "s")
        .with_properties(&[crate::custom::Property::bool("lit")])
        .with_behaviour(&Glow);

    /// A test server with light off (every chunk lit, so random ticks need no light work) and
    /// the spawn area loaded.
    fn server() -> State {
        new_state(
            settings(|c| c.developer.lighting = crate::config::Lighting::Fixed),
            Listeners::default(),
        )
    }

    /// A [`server`] holding [`TICKING`] and [`STILL`]; the states of the two blocks.
    fn custom_server() -> (State, crate::block::State, crate::block::State) {
        let mut registry = crate::custom::Registry::default();
        registry.add_block(&TICKING);
        registry.add_block(&STILL);
        registry.finish().unwrap();
        let registry = Arc::new(registry);
        let mut state = server();
        state.custom = Arc::clone(&registry);
        for w in &mut state.worlds {
            w.set_custom(Arc::clone(&registry));
        }
        let ticking = registry.block_state(&TICKING).unwrap();
        let still = registry.block_state(&STILL).unwrap();
        (state, ticking, still)
    }

    /// One tick of block updates and nothing else.
    fn advance(state: &mut State) {
        tick(state);
        state.tick += 1;
    }

    fn set(state: &mut State, pos: BlockPos, s: crate::block::State) {
        state.worlds[0].set_block(pos, s).unwrap();
    }

    fn get(state: &State, pos: BlockPos) -> crate::block::State {
        state.worlds[0].block(pos).unwrap()
    }

    fn pool() -> Pool {
        crate::runtime::cpu_pool(1).unwrap()
    }

    /// A whole tick with `inputs`, as the tick thread runs it, the stand-in save thread
    /// answering.
    fn full_tick(state: &mut State, inputs: Vec<Input>, pool: &Pool, output: &mut Output) {
        let mut batch = inputs;
        let mut log = InputLog::default();
        let mut answers = std::mem::take(&mut output.answers);
        step(state, &mut batch, &mut answers, pool, output, &mut log);
        deliver(state, output, &mut log);
        boundary(state, output, &mut log);
        let requests = std::mem::take(&mut output.requests);
        output.answers = answer(state, requests);
    }

    /// The blocks of the box around the scenes of [`stays_as_placed`].
    fn scene_cells() -> Vec<BlockPos> {
        let mut cells = Vec::new();
        for x in 0..12 {
            for y in -61..-54 {
                for z in 0..12 {
                    cells.push(BlockPos::new(x, y, z));
                }
            }
        }
        cells
    }

    /// Places `placed` and runs the block updates for a long time: no block of the scene has
    /// changed.
    fn stays_as_placed(placed: &[(BlockPos, crate::block::State)]) {
        let mut state = server();
        for &(pos, s) in placed {
            set(&mut state, pos, s);
        }
        let read = |state: &State| -> Vec<crate::block::State> {
            scene_cells().into_iter().map(|p| get(state, p)).collect()
        };
        let before = read(&state);
        for _ in 0..400 {
            advance(&mut state);
        }
        assert_eq!(read(&state), before);
        assert_eq!(state.worlds[0].changed_len(), 0);
    }

    #[test]
    fn liquid_does_not_flow() {
        let p = BlockPos::new;
        stays_as_placed(&[
            (p(6, -58, 6), Kind::Water.default_state()),
            (p(9, -60, 9), Kind::Lava.default_state()),
        ]);
    }

    #[test]
    fn fire_does_not_spread_or_burn_out() {
        let p = BlockPos::new;
        stays_as_placed(&[
            (p(6, -60, 6), Kind::OakPlanks.default_state()),
            (p(6, -59, 6), Kind::Fire.default_state()),
            (p(7, -59, 6), Kind::OakPlanks.default_state()),
        ]);
    }

    #[test]
    fn sand_does_not_fall() {
        stays_as_placed(&[(BlockPos::new(6, -57, 6), Kind::Sand.default_state())]);
    }

    #[test]
    fn blocks_stay_when_their_support_is_gone() {
        let p = BlockPos::new;
        stays_as_placed(&[
            (p(6, -58, 6), Kind::Torch.default_state()),
            (p(8, -58, 8), Kind::Dandelion.default_state()),
        ]);
    }

    #[test]
    fn a_changed_block_does_not_change_its_neighbours() {
        let p = BlockPos::new;
        let mut state = server();
        let ore = Kind::RedstoneOre.default_state();
        set(&mut state, p(2, -60, 2), ore);
        for _ in 0..3 {
            advance(&mut state);
        }
        set(&mut state, p(3, -60, 2), Kind::Stone.default_state());
        for _ in 0..40 {
            advance(&mut state);
        }
        assert_eq!(get(&state, p(2, -60, 2)), ore);
        assert_eq!(get(&state, p(3, -60, 2)), Kind::Stone.default_state());
        assert_eq!(state.worlds[0].changed_len(), 0);
    }

    /// Vanilla blocks around a player whose view is open get any number of random ticks and stay
    /// as they are.
    #[test]
    fn random_ticks_leave_vanilla_blocks_as_they_are() {
        let pool = pool();
        let mut state = server();
        let farmland = crate::block::State::find(
            "minecraft:farmland",
            &[("moisturized_amount", PropertyValue::Int(7))],
        )
        .unwrap();
        let wheat = Kind::Wheat.default_state();
        let sapling = Kind::OakSapling.default_state();
        for x in -20..20 {
            for z in -20..20 {
                set(&mut state, BlockPos::new(x, -61, z), farmland);
                let plant = if x % 2 == 0 { wheat } else { sapling };
                set(&mut state, BlockPos::new(x, -60, z), plant);
            }
        }
        let mut output = Output::default();
        let (j, rx) = join(1, 0);
        full_tick(&mut state, vec![j, radius(1, 1, 4)], &pool, &mut output);
        for _ in 0..300 {
            full_tick(&mut state, Vec::new(), &pool, &mut output);
        }
        for x in -20..20 {
            for z in -20..20 {
                assert_eq!(get(&state, BlockPos::new(x, -61, z)), farmland);
                let plant = if x % 2 == 0 { wheat } else { sapling };
                assert_eq!(get(&state, BlockPos::new(x, -60, z)), plant);
            }
        }
        drop(rx);
        assert!(pool.shutdown());
    }

    /// A floor of custom blocks around a player, every other one taking random ticks: those get
    /// them through their behaviour, the same way in a second run; the others never do.
    #[test]
    fn custom_blocks_that_ask_for_random_ticks_get_them_deterministically() {
        let run = || {
            let pool = pool();
            let (mut state, ticking, still) = custom_server();
            let ticks = |s: crate::block::State| {
                s.custom_block()
                    .is_some_and(crate::custom::Block::has_random_ticks)
            };
            assert!(ticks(ticking) && !ticks(still));
            for x in -16..16 {
                for z in -16..16 {
                    let s = if (x + z) % 2 == 0 { ticking } else { still };
                    set(&mut state, BlockPos::new(x, -60, z), s);
                }
            }
            let mut output = Output::default();
            let (j, rx) = join(1, 0);
            full_tick(&mut state, vec![j, radius(1, 1, 4)], &pool, &mut output);
            let mut hashes = Vec::new();
            for _ in 0..200 {
                full_tick(&mut state, Vec::new(), &pool, &mut output);
                hashes.push(state.hash());
            }
            let mut lit = [0, 0];
            for x in -16..16 {
                for z in -16..16 {
                    let s = get(&state, BlockPos::new(x, -60, z));
                    let def = s.custom_block().unwrap().identifier();
                    if s.value("lit") == Some(PropertyValue::Byte(1)) {
                        lit[usize::from(def == "test:still")] += 1;
                    }
                }
            }
            drop(rx);
            assert!(pool.shutdown());
            (hashes, lit)
        };
        let (first, [ticked, still]) = run();
        assert!(ticked > 0, "no random tick reached a custom block");
        assert_eq!(still, 0, "a block that did not ask got random ticks");
        let (second, lit) = run();
        assert_eq!(first, second);
        assert_eq!(lit, [ticked, 0]);
    }

    #[test]
    fn ticking_chunks_are_the_view_discs_with_lit_neighbourhoods() {
        let mut state = server();
        // A loaded area with holes, so some disc chunks lack a loaded neighbour.
        for x in -12..=12 {
            for z in -12..=12 {
                let pos = ChunkPos::new(x, z);
                if (x * 7 + z * 3) % 11 != 0 {
                    state.worlds[0].insert_chunk(pos, world::Chunk::generate(&world::Flat, 0, pos));
                }
            }
        }
        let centers = BTreeSet::from([ChunkPos::new(0, 0), ChunkPos::new(3, -2)]);
        // One list for every radius, as the tick reuses it.
        let mut chunks = Vec::new();
        for radius in 1..=8 {
            let w = &state.worlds[0];
            let expected: Vec<ChunkPos> = centers
                .iter()
                .flat_map(|&c| view::disc(c, radius))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .filter(|c| {
                    (-1..=1).all(|dx| {
                        (-1..=1).all(|dz| {
                            let n = ChunkPos::new(c.x + dx, c.z + dz);
                            w.is_chunk_loaded(n) && w.is_chunk_lit(n)
                        })
                    })
                })
                .collect();
            assert!(!expected.is_empty());
            ticking_chunks(&state, 0, radius, Some(&centers), &mut chunks);
            assert_eq!(chunks, expected, "{radius}");
        }
    }

    /// Blocks and entities tick around the same players: one whose view is not open yet (joined,
    /// no chunk radius asked) ticks neither; once it is open, both.
    #[test]
    fn blocks_and_entities_tick_around_the_same_players() {
        let pool = pool();
        let (mut state, ticking, _) = custom_server();
        for x in -16..16 {
            for z in -16..16 {
                set(&mut state, BlockPos::new(x, -60, z), ticking);
            }
        }
        let lit = |state: &State| {
            (-16..16).any(|x| {
                (-16..16).any(|z| {
                    get(state, BlockPos::new(x, -60, z)).value("lit")
                        == Some(PropertyValue::Byte(1))
                })
            })
        };
        let mut output = Output::default();
        let (j, _rx) = join(1, 0);
        full_tick(&mut state, vec![j], &pool, &mut output);
        full_tick(&mut state, Vec::new(), &pool, &mut output);
        let player = &state.players[&crate::tick::SessionId::new(1)];
        assert!(player.phase != Phase::Loading && !player.view.is_open());
        let start = crate::math::Vec3::new(0.5, -50.0, 0.5);
        let stick = crate::item::Stack::new(crate::item::Kind::Stick.default_type(), 1);
        let id = Context { state: &mut state }
            .drop_item(
                world::Id::DEFAULT,
                start,
                stick,
                Some(crate::math::Vec3::ZERO),
            )
            .unwrap();
        let moved = |state: &mut State| Context { state }.entity(id).unwrap().position() != start;
        for _ in 0..200 {
            full_tick(&mut state, Vec::new(), &pool, &mut output);
        }
        assert_eq!(
            (lit(&state), moved(&mut state)),
            (false, false),
            "view closed"
        );
        full_tick(&mut state, vec![radius(1, 1, 4)], &pool, &mut output);
        for _ in 0..200 {
            full_tick(&mut state, Vec::new(), &pool, &mut output);
        }
        assert_eq!((lit(&state), moved(&mut state)), (true, true), "view open");
    }

    #[test]
    fn random_positions_are_a_function_of_seed_tick_and_chunk() {
        let positions = |tick: u64, chunk: ChunkPos| {
            let k = random::key(RANDOM_TICK, &[0, u64::from(chunk.x.cast_unsigned()), 0]);
            let mut rng = Random::new(7, tick, k);
            let mut bits = rng.next_u64();
            (0..RANDOM_TICKS_PER_SUB_CHUNK)
                .map(|_| {
                    let p = random_position(chunk, -64, bits);
                    bits >>= 12;
                    p
                })
                .collect::<Vec<_>>()
        };
        let c = ChunkPos::new(2, 0);
        assert_eq!(positions(5, c), positions(5, c));
        assert_ne!(positions(5, c), positions(6, c));
        assert!(
            positions(5, c)
                .iter()
                .all(|p| p.chunk() == c && (-64..-48).contains(&p.y))
        );
        // Every position of a sub-chunk comes up about equally often.
        let mut hits = vec![0_u32; 4096];
        let mut rng = Random::new(1, 2, 3);
        for _ in 0..409_600 {
            let p = random_position(ChunkPos::new(0, 0), 0, rng.next_u64());
            hits[usize::try_from(p.x + 16 * p.z + 256 * p.y).unwrap()] += 1;
        }
        // Each count is binomial(409600, 1/4096): mean 100, sd 10; 6 sd either side.
        assert!(hits.iter().all(|&h| (40..=160).contains(&h)));
    }

    /// The oak logs in the column of `pos` from its height up.
    fn logs(state: &State, pos: BlockPos) -> usize {
        (0..20)
            .filter(|dy| {
                state.worlds[0]
                    .block(BlockPos::new(pos.x, pos.y + dy, pos.z))
                    .is_some_and(|b| b.kind() == Kind::OakLog)
            })
            .count()
    }

    // api: TreeFactory, TreeFactory::get
    #[test]
    fn the_api_grows_trees_where_they_fit() {
        let world = world::Id::DEFAULT;
        let at = BlockPos::new(20, -60, 20);
        let mut state = server();
        let mut cx = Context { state: &mut state };
        assert!(cx.grow_tree(world, at, TreeType::Oak));
        assert!(
            (3..=5).contains(&logs(&state, at)),
            "an oak is 4 to 6 high, its trunk one log less"
        );
        assert_eq!(
            state.worlds[0].block(at.step(Face::Down).unwrap()),
            Some(crate::block::DIRT)
        );
        let mut cx = Context { state: &mut state };
        assert!(
            !cx.grow_tree(world, at, TreeType::Oak),
            "a log is in the way"
        );
        let missing = world::Id::new(7).unwrap();
        assert!(!cx.grow_tree(missing, at, TreeType::Oak));
        assert!(!cx.grow_tree(world, BlockPos::new(10_000, -60, 0), TreeType::Oak));
        assert_eq!(
            logs(&state, at),
            logs(&state, at.step(Face::Up).unwrap()) + 1
        );
        for kind in [
            TreeType::Spruce,
            TreeType::Birch,
            TreeType::Jungle,
            TreeType::Acacia,
            TreeType::Crimson,
            TreeType::Warped,
            TreeType::Azalea,
        ] {
            let mut state = server();
            let at = BlockPos::new(-20, -60, -20);
            assert!(
                Context { state: &mut state }.grow_tree(world, at, kind),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn the_same_tree_grows_from_the_same_state() {
        let grow = || {
            let mut state = server();
            let at = BlockPos::new(-20, -60, 20);
            assert!(Context { state: &mut state }.grow_tree(
                world::Id::DEFAULT,
                at,
                TreeType::Birch
            ));
            state.hash()
        };
        assert_eq!(grow(), grow());
    }

    #[test]
    fn the_api_grows_nothing_far_outside_the_world() {
        let mut state = server();
        let mut cx = Context { state: &mut state };
        for pos in [
            BlockPos::new(0, i32::MAX, 0),
            BlockPos::new(i32::MAX, -60, 0),
            BlockPos::new(0, -60, i32::MAX),
        ] {
            for kind in [
                TreeType::Oak,
                TreeType::Spruce,
                TreeType::Birch,
                TreeType::Jungle,
                TreeType::Acacia,
                TreeType::Crimson,
                TreeType::Warped,
                TreeType::Azalea,
            ] {
                assert!(
                    !cx.grow_tree(world::Id::DEFAULT, pos, kind),
                    "{kind:?} at {pos:?}"
                );
            }
        }
    }
}
