//! Anchor: a Minecraft: Bedrock Edition server library.

// Per-connection panic isolation needs unwinding, and the binary's profile decides the strategy.
#[cfg(not(panic = "unwind"))]
compile_error!("anchor requires panic = \"unwind\"");

mod auth;
pub mod block;
mod clock;
pub mod config;
mod crafting;
mod crypto;
pub mod effect;
pub mod entity;
mod error;
pub mod event;
pub mod item;
mod login;
pub mod math;
pub mod nbt;
mod nethernet;
mod pool;
pub mod protocol;
mod raknet;
mod random;
mod runtime;
mod save;
mod server;
mod session;
mod storage;
mod tick;
pub mod world;

pub use config::Config;
pub use error::{Error, Result};
pub use server::{Server, ServerBuilder, ShutdownHandle};
pub use tick::{Context, PlayerId};
