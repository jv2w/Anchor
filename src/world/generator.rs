//! World generators (PLAN.md 6, 10.6): the [`Generator`] extension point, the built-in flat
//! and void worlds, and the generators a server makes worlds with by name.

use std::any::TypeId;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use crate::block;

use super::sub_chunk::Entry;
use super::{BlockPos, Chunk, ChunkPos};

/// Fills the chunks of a world that were never saved.
///
/// It runs outside the tick, on the CPU pool, for chunks no save holds (PLAN.md 5): it must be a
/// pure function of the seed and the chunk position, so the same seed and position always give
/// the same chunk and a crash replay rebuilds the same world. A chunk that was generated and
/// never changed is not saved but generated again when it is loaded, so changing a world's
/// generator also changes its unchanged chunks.
///
/// The generator's block states are kept as written: neighbour connections (fences, panes,
/// walls, stairs, tripwire) are worked out only for blocks set through
/// [`World::set_block`](super::World::set_block).
// pmmp-class: Generator
pub trait Generator: Send + Sync + 'static {
    /// Fills `chunk`, the empty chunk at `pos` (all air, plains biomes), with blocks and biomes.
    #[doc(alias = "Generator::generateChunk")]
    fn generate(&self, seed: i64, pos: ChunkPos, chunk: &mut Chunk);

    /// Where players appear in a new world made with `seed`. The default is x 0, y 64, z 0.
    fn spawn(&self, _seed: i64) -> BlockPos {
        BlockPos::new(0, 64, 0)
    }

    /// The name saved with the world, at most 256 bytes ([`Server`](crate::Server) refuses to
    /// build with a longer one). A world saved with another generator's name is still filled by
    /// this one, with a warning. The default is the type's name.
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

/// The generators a server makes worlds with, by [`Generator::name`] (PMMP `GeneratorManager`):
/// the built-in ones, those of the worlds it was built with and those registered with
/// [`ServerBuilder::generator`](crate::ServerBuilder::generator). Fixed when it is built.
#[derive(Clone)]
pub(crate) struct Generators {
    by_name: BTreeMap<&'static str, (TypeId, Arc<dyn Generator>)>,
    /// A name two generator types share: the build fails with it.
    clash: Option<&'static str>,
}

/// The built-in generators.
impl Default for Generators {
    fn default() -> Self {
        let mut generators = Self {
            by_name: BTreeMap::new(),
            clash: None,
        };
        generators.add_typed(Flat);
        generators.add_typed(Void);
        generators
    }
}

impl Generators {
    /// Adds `generator` of type `type_id`. A name already taken by a generator of the same type
    /// keeps the first (two values of one type with one name must generate alike); one taken
    /// by another type is a clash.
    pub(crate) fn add(&mut self, type_id: TypeId, generator: Arc<dyn Generator>) {
        let name = generator.name();
        match self.by_name.get(name) {
            None => {
                self.by_name.insert(name, (type_id, generator));
            }
            Some((known, _)) if *known == type_id => {}
            Some(_) => {
                self.clash.get_or_insert(name);
            }
        }
    }

    /// Adds a generator of type `G`.
    pub(crate) fn add_typed<G: Generator>(&mut self, generator: G) -> Arc<dyn Generator> {
        let generator: Arc<dyn Generator> = Arc::new(generator);
        self.add(TypeId::of::<G>(), Arc::clone(&generator));
        generator
    }

    /// The name two generator types share, if any.
    pub(crate) fn clash(&self) -> Option<&'static str> {
        self.clash
    }

    /// The generator named `name`.
    pub(crate) fn get(&self, name: &str) -> Option<&Arc<dyn Generator>> {
        self.by_name.get(name).map(|(_, g)| g)
    }

    /// The names, in order.
    pub(crate) fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.by_name.keys().copied()
    }
}

/// Lists the names: the registration fingerprint of a replay compares them (PLAN.md 10.9).
impl fmt::Debug for Generators {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.names()).finish()
    }
}

/// BDS's default flat world: bedrock at y -64, dirt at -63 and -62, grass at -61, plains
/// everywhere. Its chunks are the same bytes as BDS's flat world (P6).
#[derive(Debug, Clone, Copy, Default)]
// pmmp-class: Flat
pub struct Flat;

impl Generator for Flat {
    fn generate(&self, _seed: i64, _pos: ChunkPos, chunk: &mut Chunk) {
        let layers = [
            (-64, block::BEDROCK),
            (-63, block::DIRT),
            (-62, block::DIRT),
            (-61, block::GRASS_BLOCK),
        ];
        for (y, state) in layers {
            for x in 0..16 {
                for z in 0..16 {
                    // Every position is inside the chunk, so nothing is left unset.
                    chunk.put(x, y, z, Entry::Known(state));
                }
            }
        }
    }

    /// Standing on the grass at x 0, z 0: feet at y -60.
    fn spawn(&self, _seed: i64) -> BlockPos {
        BlockPos::new(0, -60, 0)
    }

    /// `flat`, the `level-type` value.
    fn name(&self) -> &'static str {
        "flat"
    }
}

/// A world of air with plains biomes. Its spawn is the default, x 0, y 64, z 0: there is no
/// ground, so a server places its own platform or spawn.
#[derive(Debug, Clone, Copy, Default)]
pub struct Void;

impl Generator for Void {
    fn generate(&self, _seed: i64, _pos: ChunkPos, _chunk: &mut Chunk) {}

    /// `void`, the `level-type` value.
    fn name(&self) -> &'static str {
        "void"
    }
}
