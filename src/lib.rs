//! Anchor: a Minecraft: Bedrock Edition server library.

// Per-connection panic isolation needs unwinding, and the binary's profile decides the strategy.
#[cfg(not(panic = "unwind"))]
compile_error!("anchor requires panic = \"unwind\"");

pub mod action;
mod auth;
pub mod block;
mod clock;
pub mod command;
pub mod config;
mod console;
mod crafting;
mod crypto;
pub mod custom;
pub mod data;
pub mod display;
pub mod effect;
pub mod entity;
mod error;
pub mod event;
pub mod form;
pub mod inventory;
pub mod item;
mod login;
pub mod math;
pub mod menu;
pub mod nbt;
mod nethernet;
pub mod pack;
pub mod permission;
pub mod player;
mod pool;
pub mod protocol;
mod raknet;
mod random;
mod runtime;
mod save;
mod server;
mod session;
mod storage;
pub mod task;
mod tick;
pub mod tile;
pub mod world;

pub use config::Config;
pub use error::{Error, Result};
pub use server::{Server, ServerBuilder, ShutdownHandle};
pub use tick::Context;
