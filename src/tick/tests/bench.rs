//! Tick cost measurements: whole ticks of the in-process server (inputs, rules, views, output and
//! delivery; the stand-in save thread answers outside the measured time, lighting new chunks on the
//! CPU pool as the save thread does) on a CPU pool of max(logical CPUs, 4) threads, with a flat
//! world and a view distance of 8: `cargo test --release --lib tick::tests::bench -- --ignored
//! --nocapture`. Each measurement runs without listeners, then with an idle listener and monitor on
//! every event, so every event is built and every rule checks again.
//!
//! Each tick is timed per [`Part`] of the step by the mark [`step_marked`] calls as each ends.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use super::super::queue::{Input, Outbound};
use super::super::{
    InputLog, Output, Part, Phase, Settings, State, boundary, deliver, entity, step_marked,
    survival,
};
use super::{initialised, input, join, new_state, packet, radius, settings};
use crate::block::{self, PropertyValue};
use crate::config::{Config, Lighting};
use crate::event::Listeners;
use crate::item::{Stack, Type};
use crate::math::Vec3;
use crate::nbt::Compound;
use crate::pool::Pool;
use crate::protocol::packet::{Packet, PlayerAuthInput};
use crate::save::{Loaded, PlayerLoad, Request, Response};
use crate::world::{BlockPos, Chunk, Flat};

/// The names of the [`Part`]s, in order; "output" also holds the delivery and the boundary.
const PHASES: [&str; 10] = [
    "input", "survival", "interact", "update", "entity", "trade", "blockent", "light", "view",
    "output",
];
/// The view distance of every measurement.
const VIEW_DISTANCE: u8 = 8;
/// A walking player's speed in blocks per tick (4.317 blocks per second).
const WALK: f64 = 0.215;
/// The feet height on the flat world.
const FEET: f64 = -60.0;

/// The stand-in save thread of the measurements: chunks are generated and lit on the CPU pool
/// and their extras read (as `save::load_chunks` does), players are new, commits succeed.
fn answer(state: &State, pool: &Pool, requests: Vec<Request>) -> Vec<Response> {
    let lighting = state.settings.lighting.is_calculated();
    let mut out = Vec::new();
    let mut chunks = Vec::new();
    for request in requests {
        match request {
            Request::Chunks(keys) => chunks.extend(keys),
            Request::Player(key) => out.push(Response::Player {
                key,
                data: PlayerLoad::Absent,
            }),
            Request::Commit(snapshot) => out.push(Response::Committed {
                start: snapshot.start,
            }),
            Request::Worlds(_) | Request::DeleteWorlds(_) | Request::Finish => {}
        }
    }
    let seeds: Vec<i64> = state.worlds.iter().map(crate::world::World::seed).collect();
    let (custom, types) = (&**state.custom(), &**state.entity_types());
    out.extend(pool.map(&chunks, |&(world, pos)| {
        let seed = seeds.get(world).copied().unwrap_or_default();
        let mut chunk = Box::new(Chunk::generate(&Flat, seed, pos));
        if lighting {
            chunk.populate_light();
        }
        let mut stored = crate::tick::extra::tests::sample(world, pos);
        stored.rules =
            crate::tick::ChunkRules::read((world, pos), &stored.extras, &chunk, custom, types);
        Response::Chunk {
            world,
            pos,
            loaded: Loaded::Generated(chunk, stored),
        }
    }));
    out
}

/// A server and its sessions, run tick by tick.
struct Bench {
    state: State,
    pool: Pool,
    output: Output,
    receivers: Vec<mpsc::Receiver<Outbound>>,
    phases: [Duration; PHASES.len()],
    /// The measured time of each tick since the last [`Bench::reset`].
    ticks: Vec<Duration>,
}

impl Bench {
    /// A flat world with `edit` applied to the measurement settings and `listeners`, its spawn
    /// area lit.
    fn new(listeners: Listeners, edit: impl FnOnce(&mut Config)) -> Self {
        let threads = crate::runtime::logical_cpus().max(4);
        let settings = settings(|c| {
            c.general.view_distance = VIEW_DISTANCE;
            c.general.max_players = 1_000;
            c.developer.chunks_per_tick = Config::default().developer.chunks_per_tick;
            edit(c);
        });
        let mut bench = Self::with(settings, listeners, threads);
        bench.settle(0);
        bench
    }

    fn with(settings: Settings, listeners: Listeners, threads: usize) -> Self {
        Self {
            state: new_state(settings, listeners),
            pool: crate::runtime::cpu_pool(threads).expect("CPU pool"),
            output: Output::default(),
            receivers: Vec::new(),
            phases: [Duration::ZERO; PHASES.len()],
            ticks: Vec::new(),
        }
    }

    /// Runs one tick of `inputs` and records its time: the step, the delivery and the boundary.
    #[expect(
        clippy::disallowed_methods,
        reason = "consistency: a measurement reads the wall clock"
    )]
    fn tick(&mut self, mut inputs: Vec<Input>) {
        let mut answers = std::mem::take(&mut self.output.answers);
        let mut log = InputLog::default();
        let start = Instant::now();
        let mut lap = start;
        let phases = &mut self.phases;
        step_marked(
            &mut self.state,
            &mut inputs,
            &mut answers,
            &self.pool,
            &mut self.output,
            &mut log,
            |part: Part| {
                let now = Instant::now();
                if let Some(p) = phases.get_mut(part as usize) {
                    *p += now - lap;
                }
                lap = now;
            },
        );
        let delivered = Instant::now();
        deliver(&mut self.state, &mut self.output, &mut log);
        boundary(&mut self.state, &mut self.output, &mut log);
        let end = Instant::now();
        if let Some(output) = self.phases.last_mut() {
            *output += end - delivered;
        }
        self.ticks.push(end - start);
        let requests = std::mem::take(&mut self.output.requests);
        self.output.answers = answer(&self.state, &self.pool, requests);
        for rx in &mut self.receivers {
            while rx.try_recv().is_ok() {}
        }
    }

    /// Runs empty ticks until the world's light work is done, at most `limit` more than `at_least`.
    fn settle(&mut self, at_least: u32) {
        for _ in 0..at_least {
            self.tick(Vec::new());
        }
        for _ in 0..2_000 {
            if self.state.worlds[0].is_light_idle() && self.output.answers.is_empty() {
                break;
            }
            self.tick(Vec::new());
        }
    }

    /// Joins `n` players at the spawn and runs ticks until all of them are spawned.
    fn join(&mut self, n: u64) {
        let mut inputs = Vec::new();
        for s in 1..=n {
            let (j, rx) = join(s, 0);
            self.receivers.push(rx);
            inputs.extend([j, radius(s, 1, i32::from(VIEW_DISTANCE))]);
        }
        self.tick(inputs);
        for _ in 0..1_000 {
            let spawning: Vec<Input> = self
                .state
                .players
                .iter()
                .filter(|(_, p)| p.phase == Phase::Spawning)
                .map(|(s, p)| initialised(s.get(), 2, p.runtime_id))
                .collect();
            let players = &self.state.players;
            assert_eq!(
                players.len(),
                usize::try_from(n).expect("few"),
                "a player left"
            );
            if players.values().all(|p| p.phase == Phase::Spawned) {
                return;
            }
            self.tick(spawning);
        }
        panic!("the players did not spawn");
    }

    /// Forgets the times measured so far.
    fn reset(&mut self) {
        self.phases = [Duration::ZERO; PHASES.len()];
        self.ticks.clear();
    }

    /// Prints the measured ticks: mean, 99th percentile and worst, and the mean per phase.
    #[expect(
        clippy::print_stderr,
        reason = "consistency: a measurement prints its result"
    )]
    fn report(&self, name: &str) {
        let mut sorted = self.ticks.clone();
        sorted.sort_unstable();
        let n = u32::try_from(sorted.len()).expect("few ticks").max(1);
        let total: Duration = sorted.iter().sum();
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        let p99 = sorted
            .get(sorted.len().saturating_sub(1) * 99 / 100)
            .copied()
            .unwrap_or_default();
        let worst = sorted.last().copied().unwrap_or_default();
        eprintln!(
            "{name:<34} {n:>4} ticks  mean {:>7.3} ms  p99 {:>7.3} ms  max {:>7.3} ms",
            ms(total / n),
            ms(p99),
            ms(worst)
        );
        let phases: Vec<String> = PHASES
            .iter()
            .zip(self.phases)
            .map(|(p, d)| format!("{p} {:.3}", ms(d / n)))
            .collect();
        eprintln!("    per phase (ms): {}", phases.join(", "));
    }
}

/// A `PlayerAuthInput` of session `s` with the eye at `eye`.
fn walked(s: u64, seq: u64, eye: Vec3) -> Input {
    let p = PlayerAuthInput {
        position: eye.to_f32(),
        tick: seq,
        ..Default::default()
    };
    input(s, seq, packet(Packet::PlayerAuthInput(Box::new(p))))
}

/// The feet of player `i` of `n`: a square grid 16 blocks apart around the spawn.
fn grid(i: u64, n: u64) -> Vec3 {
    let side = (1..=n).find(|s| s * s >= n).unwrap_or(1);
    let at = |k: u64| {
        (f64::from(u32::try_from(k).expect("small"))
            - f64::from(u32::try_from(side).expect("small")) / 2.0)
            * 16.0
            + 0.5
    };
    Vec3::new(at(i % side), FEET, at(i / side))
}

/// `n` players spawned, placed on a grid, then walking east together for 300 ticks (each
/// crossing about four chunks, so views keep loading and sending chunks).
fn players_walking(n: u64, run: Run) {
    let mut b = Bench::new((run.listeners)(), |_| {});
    b.join(n);
    let sessions: Vec<_> = b.state.players.keys().copied().collect();
    for (i, s) in (0u64..).zip(&sessions) {
        survival::teleport(
            &mut b.state,
            *s,
            0,
            grid(i, n) + Vec3::new(0.0, survival::EYE_HEIGHT, 0.0),
        );
    }
    // The teleported views fill up first (4 chunks per tick).
    b.settle(100);
    b.reset();
    for t in 1..=300u32 {
        let inputs = (0u64..)
            .zip(&sessions)
            .map(|(i, s)| {
                let feet = grid(i, n) + Vec3::new(WALK * f64::from(t), 0.0, 0.0);
                walked(
                    s.get(),
                    10 + u64::from(t),
                    feet + Vec3::new(0.0, survival::EYE_HEIGHT, 0.0),
                )
            })
            .collect();
        b.tick(inputs);
    }
    b.report(&format!("{n} players walking{}", run.label));
}

/// One player at the spawn watching `count` dropped swords (they do not merge), four to a block
/// around it, once they lie still.
fn items(count: u32, run: Run) {
    let mut b = Bench::new((run.listeners)(), |_| {});
    b.join(1);
    let sword =
        Type::from_name("minecraft:diamond_sword", 0, None, &Compound::new()).expect("sword");
    let spawn = b.state.worlds[0].spawn();
    for i in 0..count {
        let x = f64::from(spawn.x) + f64::from(i % 100) * 0.5 - 25.0 + 0.25;
        let z = f64::from(spawn.z) + f64::from(i / 100) * 0.5 - 25.0 + 0.25;
        let pos = Vec3::new(x, FEET, z);
        assert!(entity::drop_item_with(
            &mut b.state,
            0,
            pos,
            Stack::new(sword, 1),
            Some(Vec3::ZERO),
            10
        ));
    }
    b.settle(60);
    b.reset();
    for _ in 0..300 {
        b.tick(Vec::new());
    }
    b.report(&format!("{count} items lying{}", run.label));
}

/// The block state `name` with `properties`.
fn state(name: &str, properties: &[(&str, PropertyValue<'_>)]) -> block::State {
    block::State::find(name, properties).expect("block state")
}

/// Wheat on wet farmland on every column of the chunks within the tick radius (4) of one player,
/// at the default random tick speed (3).
fn crops(run: Run) {
    let mut b = Bench::new((run.listeners)(), |_| {});
    let farmland = state(
        "minecraft:farmland",
        &[("moisturized_amount", PropertyValue::Int(7))],
    );
    let wheat = state("minecraft:wheat", &[("growth", PropertyValue::Int(0))]);
    let center = b.state.worlds[0].spawn().chunk();
    let mut planted = Vec::with_capacity(81 * 256);
    for cx in -4..=4 {
        for cz in -4..=4 {
            for x in 0..16 {
                for z in 0..16 {
                    let (bx, bz) = ((center.x + cx) * 16 + x, (center.z + cz) * 16 + z);
                    let world = &mut b.state.worlds[0];
                    world
                        .set_block(BlockPos::new(bx, -61, bz), farmland)
                        .expect("farmland");
                    let at = BlockPos::new(bx, -60, bz);
                    world.set_block(at, wheat).expect("wheat");
                    planted.push(at);
                }
            }
        }
    }
    b.settle(0);
    b.join(1);
    b.settle(40);
    b.reset();
    for _ in 0..300 {
        b.tick(Vec::new());
    }
    let grown = planted
        .iter()
        .filter(|at| b.state.worlds[0].block(**at).is_some_and(|s| s != wheat))
        .count();
    assert!(grown > 0, "no crop grew: the random ticks did not run");
    b.report(&format!(
        "random ticks, {} crops ({grown} grew){}",
        planted.len(),
        run.label
    ));
}

/// One player at the spawn, then 1,000 glowstone blocks set at once in the air around it (each
/// a light source that also shades the sky light below), and the ticks after it until the light
/// work is done (at least 100).
fn burst(lighting: bool, run: Run) {
    let mut b = Bench::new((run.listeners)(), |c| {
        c.developer.lighting = if lighting {
            Lighting::Dynamic
        } else {
            Lighting::Fixed
        };
    });
    b.join(1);
    b.settle(20);
    b.reset();
    let glowstone = state("minecraft:glowstone", &[]);
    let spawn = b.state.worlds[0].spawn();
    for i in 0..1_000 {
        // A 10 × 10 × 10 lattice, 5 blocks apart, from y -55.
        let pos = BlockPos::new(
            spawn.x - 25 + (i % 10) * 5,
            -55 + (i / 100) * 5,
            spawn.z - 25 + (i / 10 % 10) * 5,
        );
        b.state.worlds[0]
            .set_block(pos, glowstone)
            .expect("glowstone");
    }
    b.tick(Vec::new());
    let first = b.ticks.last().copied().unwrap_or_default();
    let mut after = 1u32;
    let mut lit_after = None;
    while after < 100 || lit_after.is_none() {
        if lit_after.is_none() && b.state.worlds[0].is_light_idle() {
            lit_after = Some(after);
        }
        b.tick(Vec::new());
        after += 1;
        assert!(after < 10_000, "the light work did not end");
    }
    let name = format!(
        "1000 changes, lighting {}{}",
        if lighting { "on" } else { "off" },
        run.label
    );
    b.report(&name);
    #[expect(
        clippy::print_stderr,
        reason = "consistency: a measurement prints its result"
    )]
    {
        eprintln!(
            "    burst tick {:.3} ms, light work done after {} ticks",
            first.as_secs_f64() * 1e3,
            lit_after.unwrap_or_default()
        );
    }
}

/// The listeners of a measurement and the label its report ends with.
#[derive(Clone, Copy)]
struct Run {
    listeners: fn() -> Listeners,
    label: &'static str,
}

#[test]
#[ignore = "measurement; run in release with --ignored --nocapture"]
fn tick_costs() {
    let runs = [
        Run {
            listeners: Listeners::default,
            label: "",
        },
        Run {
            listeners: super::busy::idle,
            label: ", idle listeners",
        },
    ];
    // Each measurement runs with and without listeners side by side: later measurements in one
    // process run slower, so two passes one after the other would not compare.
    for n in [1, 20, 100] {
        for run in runs {
            players_walking(n, run);
        }
    }
    for count in [1_000, 10_000] {
        for run in runs {
            items(count, run);
        }
    }
    for run in runs {
        crops(run);
    }
    for lighting in [true, false] {
        for run in runs {
            burst(lighting, run);
        }
    }
}
