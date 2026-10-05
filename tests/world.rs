//! The world API as a user sees it (PLAN.md 10.6): positions, errors, and a generator written
//! outside Anchor. Worlds themselves are made by the server builder and tested inside the crate.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::sync::Arc;

use anchor::Server;
use anchor::block::State;
use anchor::world::{BlockPos, Chunk, ChunkPos, Error, Flat, Generator, Kind, MAX_Y, MIN_Y, Void};

#[test]
fn block_positions_round_down_to_their_chunk() {
    assert_eq!(BlockPos::new(0, 0, 0).chunk(), ChunkPos::new(0, 0));
    assert_eq!(BlockPos::new(15, 300, 15).chunk(), ChunkPos::new(0, 0));
    assert_eq!(BlockPos::new(16, -64, -1).chunk(), ChunkPos::new(1, -1));
    assert_eq!(BlockPos::new(-17, 0, -16).chunk(), ChunkPos::new(-2, -1));
    assert_eq!(
        BlockPos::new(i32::MIN, 0, i32::MAX).chunk(),
        ChunkPos::new(i32::MIN >> 4, i32::MAX >> 4)
    );
    assert!(BlockPos::new(0, 0, 1) > BlockPos::new(0, 0, 0));
    assert_eq!((MIN_Y, MAX_Y), (-64, 319));
}

#[test]
fn errors_are_lowercase_and_shareable() {
    fn shareable<E: std::error::Error + Send + Sync + 'static>(_: &E) {}
    for e in [
        Error::OutOfRange,
        Error::NotLoaded,
        Error::Quarantined,
        Error::Malformed("a word count does not match its width"),
    ] {
        shareable(&e);
        let text = e.to_string();
        assert!(text.starts_with(|c: char| c.is_ascii_lowercase()), "{text}");
        assert!(!text.ends_with('.'), "{text}");
    }
}

/// A user generator: stone up to y 0 with a biome of its own.
#[derive(Debug)]
struct Stone;

impl Generator for Stone {
    fn generate(&self, _seed: i64, _pos: ChunkPos, chunk: &mut Chunk) {
        let stone = State::find("minecraft:stone", &[]).expect("stone");
        for x in 0..16 {
            for z in 0..16 {
                for y in MIN_Y..=0 {
                    chunk.set_block(x, y, z, stone).expect("inside the chunk");
                }
                chunk.set_biome(x, 0, z, 2).expect("inside the chunk");
            }
        }
    }
}

#[test]
fn a_generator_fills_an_empty_chunk() {
    let mut chunk = Chunk::new();
    assert_eq!(chunk.highest_block(0, 0), None);
    Stone.generate(1, ChunkPos::new(3, -4), &mut chunk);
    let stone = State::find("minecraft:stone", &[]).expect("stone");
    assert_eq!(chunk.highest_block(0, 0), Some(0));
    assert_eq!(chunk.highest_block(16, 0), None);
    assert_eq!(chunk.block(5, MIN_Y, 5), Some(stone));
    assert_eq!(chunk.block(0, 1, 0).map(State::name), Some("minecraft:air"));
    assert_eq!(chunk.biome(15, 0, 15), Some(2));
    assert_eq!(chunk.biome(15, 16, 15), Some(1), "plains elsewhere");
    assert_eq!(chunk.set_block(16, 0, 0, stone), Err(Error::OutOfRange));
    assert_eq!(
        chunk.set_block(0, MAX_Y + 1, 0, stone),
        Err(Error::OutOfRange)
    );
    assert_eq!(chunk.set_biome(0, MIN_Y - 1, 0, 1), Err(Error::OutOfRange));
    assert_eq!(chunk.block(0, 0, 16), None);
    let mut flat = Chunk::default();
    Flat.generate(0, ChunkPos::new(0, 0), &mut flat);
    assert_eq!(flat.highest_block(9, 9), Some(-61));
    assert_eq!(
        flat.block(9, -61, 9).map(State::name),
        Some("minecraft:grass_block")
    );
}

/// A generator whose name is longer than the world file holds (256 bytes).
#[derive(Debug)]
struct LongName;

impl Generator for LongName {
    fn generate(&self, _seed: i64, _pos: ChunkPos, _chunk: &mut Chunk) {}

    fn name(&self) -> &'static str {
        concat!(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "!",
        )
    }
}

#[test]
fn build_refuses_a_world_or_generator_name_longer_than_the_store_holds() {
    let longest = "w".repeat(256);
    assert!(
        Server::builder()
            .world(longest.clone(), Kind::Persistent, Flat)
            .build()
            .is_ok()
    );
    let long = format!("{longest}w");
    let e = Server::builder()
        .world(long.clone(), Kind::Temporary, Flat)
        .build()
        .expect_err("a world name the store cannot hold");
    assert!(matches!(e, anchor::Error::NameTooLong { ref name } if *name == long));
    let e = Server::builder()
        .world("arena", Kind::Persistent, LongName)
        .build()
        .expect_err("a generator name the store cannot hold");
    assert!(matches!(e, anchor::Error::NameTooLong { ref name } if name == LongName.name()));
    let text = e.to_string();
    assert!(text.starts_with(|c: char| c.is_ascii_lowercase()), "{text}");
}

#[test]
fn generators_are_shareable_extension_points() {
    let generators: Vec<Arc<dyn Generator>> = vec![Arc::new(Flat), Arc::new(Void), Arc::new(Stone)];
    let spawns: Vec<BlockPos> = generators.iter().map(|g| g.spawn(7)).collect();
    assert_eq!(
        spawns,
        [
            BlockPos::new(0, -60, 0),
            BlockPos::new(0, 64, 0),
            BlockPos::new(0, 64, 0)
        ]
    );
    assert_ne!(Kind::Persistent, Kind::Temporary);
}
