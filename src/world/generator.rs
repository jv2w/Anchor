//! World generators: the [`Generator`] extension point, the built-in flat
//! and void worlds, and the generators a server makes worlds with by name.

use std::any::TypeId;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use super::sub_chunk::Entry;
use super::{BlockPos, Chunk, ChunkPos};
use crate::block;
use crate::storage::world_name_key;

/// Fills the chunks of a world that were never saved.
///
/// It runs outside the tick, on the CPU pool, for chunks no save holds: it must be a
/// pure function of the seed and the chunk position, so the same seed and position always give
/// the same chunk and a crash replay rebuilds the same world. A chunk that was generated and
/// never changed is not saved but generated again when it is loaded, so changing a world's
/// generator also changes its unchanged chunks.
///
/// The generator's block states are kept as written: neighbour connections (fences, panes,
/// walls, stairs, tripwire) are worked out only for blocks set through
/// [`World::set_block`](super::World::set_block).
// api-class: Generator
pub trait Generator: Send + Sync + 'static {
    /// Fills `chunk`, the empty chunk at `pos` (all air, plains biomes), with blocks and biomes.
    #[doc(alias = "Generator::generateChunk")]
    fn generate(&self, seed: i64, pos: ChunkPos, chunk: &mut Chunk);

    /// Where players appear in a new world made with `seed`. The default is x 0, y 64, z 0.
    fn spawn(&self, _seed: i64) -> BlockPos {
        BlockPos::new(0, 64, 0)
    }

    /// The name worlds pick this generator by
    /// ([`Options::with_generator`](super::Options::with_generator)); it is not saved with the
    /// world. Two values of one type with one name are one generator: a world picked by the name
    /// gets the first registered, except a world of
    /// [`ServerBuilder::world`](crate::ServerBuilder::world), which keeps its own value. The
    /// default is the type's name ([`std::any::type_name`]), which is not fixed across compiler
    /// versions: a generator that worlds pick by name, or that the replay of a crash log must
    /// find again, returns a name of its own.
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
    /// The values [`ServerBuilder::world`](crate::ServerBuilder::world) gave its worlds, by
    /// [`world_name_key`]: [`Self::of_world`] picks them over the first value of their name.
    by_world: BTreeMap<String, Arc<dyn Generator>>,
    /// A name two generator types share: the build fails with it.
    clash: Option<&'static str>,
}

impl Generators {
    /// The name two generator types share, if any.
    pub(crate) fn clash(&self) -> Option<&'static str> {
        self.clash
    }

    /// The generator a world named `world` that picks the generator named `name` fills its chunks
    /// with, at its start, a load, a remaking or the replay of a crash log alike: the value the
    /// builder gave the world of that name when its generator has that name, else the first
    /// registered under `name`.
    pub(crate) fn of_world(&self, world: &str, name: &str) -> Option<&Arc<dyn Generator>> {
        self.by_world
            .get(&world_name_key(world))
            .filter(|g| g.name() == name)
            .or_else(|| self.by_name.get(name).map(|(_, g)| g))
    }

    /// The names, in order.
    pub(crate) fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.by_name.keys().copied()
    }

    /// Adds `generator`, the value of the builder's world `world` ([`Self::of_world`]), as
    /// [`Self::add_typed`] does.
    pub(crate) fn add_world<G: Generator>(
        &mut self,
        world: &str,
        generator: G,
    ) -> Arc<dyn Generator> {
        let generator = self.add_typed(generator);
        self.by_world
            .insert(world_name_key(world), Arc::clone(&generator));
        generator
    }

    /// Adds a generator of type `G`.
    pub(crate) fn add_typed<G: Generator>(&mut self, generator: G) -> Arc<dyn Generator> {
        let generator: Arc<dyn Generator> = Arc::new(generator);
        self.add(TypeId::of::<G>(), Arc::clone(&generator));
        generator
    }

    /// Adds `generator` of type `type_id`. A name already taken by a generator of the same type
    /// keeps the first (two values of one type with one name must generate alike); one taken
    /// by another type is a clash.
    pub(crate) fn add(&mut self, type_id: TypeId, generator: Arc<dyn Generator>) {
        let name = generator.name();
        match self.by_name.get(name) {
            Some((known, _)) if *known == type_id => {}
            Some(_) => {
                self.clash.get_or_insert(name);
            }
            None => {
                self.by_name.insert(name, (type_id, generator));
            }
        }
    }
}

/// The built-in generators.
impl Default for Generators {
    fn default() -> Self {
        let mut generators = Self {
            by_name: BTreeMap::new(),
            by_world: BTreeMap::new(),
            clash: None,
        };
        generators.add_typed(Flat);
        generators.add_typed(Void);
        generators
    }
}

/// Lists the names: the registration fingerprint of a replay compares them.
impl fmt::Debug for Generators {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.names()).finish()
    }
}

/// BDS's default flat world: bedrock at y -64, dirt at -63 and -62, grass at -61, plains
/// everywhere. Its chunks are the same bytes as BDS's flat world (P6).
#[derive(Debug, Clone, Copy, Default)]
// api-class: Flat
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A generator with a value: the height of its one layer of bedrock.
    struct Layer(i32);

    impl Generator for Layer {
        fn generate(&self, _seed: i64, _pos: ChunkPos, chunk: &mut Chunk) {
            for x in 0..16 {
                for z in 0..16 {
                    chunk.put(x, self.0, z, Entry::Known(block::BEDROCK));
                }
            }
        }

        fn name(&self) -> &'static str {
            "layer"
        }
    }

    /// Two builder worlds with values of one generator type under one name each keep their own
    /// value whenever they are picked by that name (a load, a crash replay); any other world
    /// picking the name gets the first value registered.
    #[test]
    fn a_builder_world_keeps_its_own_value_of_a_shared_generator_name() {
        let mut generators = Generators::default();
        let low = generators.add_world("low", Layer(-60));
        let high = generators.add_world("High", Layer(10));
        assert!(generators.clash().is_none());
        let of = |world: &str, name: &str| Arc::clone(generators.of_world(world, name).unwrap());
        assert!(Arc::ptr_eq(&of("high", "layer"), &high), "any letter case");
        assert!(Arc::ptr_eq(&of("low", "layer"), &low));
        assert!(
            Arc::ptr_eq(&of("other", "layer"), &low),
            "the first registered"
        );
        assert_eq!(of("high", "flat").name(), "flat", "another name it picks");
        assert!(generators.of_world("high", "nothing").is_none());
    }
}
