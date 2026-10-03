//! Scheduled tasks (PLAN.md 8.3, 10.6): code the server runs every few ticks in the tick's serial
//! step, and the [`Context`] it gets.
//!
//! Tasks are registered with [`ServerBuilder::every`](crate::ServerBuilder::every) before the
//! server is built and run in registration order after the tick's inputs, so a replay of the
//! input log runs them at the same points with the same state.

use std::fmt;
use std::num::NonZeroU32;

use crate::world::World;

/// What a scheduled task can see and change: the engine time and the worlds.
///
/// The shape is preliminary: the API milestone (PLAN.md 10, M8) widens it.
pub struct Context<'a> {
    pub(super) tick: u64,
    pub(super) worlds: &'a mut [World],
    pub(super) save: &'a mut bool,
}

impl Context<'_> {
    /// The engine time: ticks since the world was created (20 per second).
    #[doc(alias = "Server::getTick")]
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// The default world: the one the settings name (`level-name`), where players join.
    #[doc(alias = "WorldManager::getDefaultWorld")]
    pub fn world(&self) -> &World {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: a server always has its default world at index 0"
        )]
        &self.worlds[0]
    }

    /// The default world, to change it.
    pub fn world_mut(&mut self) -> &mut World {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: a server always has its default world at index 0"
        )]
        &mut self.worlds[0]
    }

    /// The world named `name`, if the server has it.
    #[doc(alias = "WorldManager::getWorldByName")]
    pub fn world_named(&self, name: &str) -> Option<&World> {
        self.worlds.iter().find(|w| w.name() == name)
    }

    /// The world named `name`, to change it.
    pub fn world_named_mut(&mut self, name: &str) -> Option<&mut World> {
        self.worlds.iter_mut().find(|w| w.name() == name)
    }

    /// Saves every persistent world and the player data at the end of this tick, together, as
    /// one commit (PLAN.md 6). Calling it again in the same tick changes nothing.
    #[doc(alias = "WorldManager::save")]
    #[doc(alias = "World::save")]
    pub fn save(&mut self) {
        *self.save = true;
    }
}

impl fmt::Debug for Context<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("tick", &self.tick)
            .field("worlds", &self.worlds.len())
            .field("save", &self.save)
            .finish()
    }
}

/// A task body.
type Body = Box<dyn FnMut(&mut Context<'_>) + Send>;

/// The scheduled tasks, in registration order.
#[derive(Default)]
pub(crate) struct Tasks(Vec<(NonZeroU32, Body)>);

impl Tasks {
    /// Adds `task`, run on every tick that is a multiple of `period`.
    pub(crate) fn push(
        &mut self,
        period: NonZeroU32,
        task: impl FnMut(&mut Context<'_>) + Send + 'static,
    ) {
        self.0.push((period, Box::new(task)));
    }

    /// Runs the tasks due at `tick`.
    pub(super) fn run(&mut self, tick: u64, worlds: &mut [World], save: &mut bool) {
        for (period, task) in &mut self.0 {
            if tick.is_multiple_of(u64::from(period.get())) {
                task(&mut Context { tick, worlds, save });
            }
        }
    }
}

impl fmt::Debug for Tasks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(period, _)| period))
            .finish()
    }
}
