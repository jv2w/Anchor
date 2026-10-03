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
mod error;
pub mod event;
pub mod item;
mod login;
pub mod nbt;
mod nethernet;
pub mod protocol;
mod raknet;
mod runtime;
mod server;
mod session;
mod tick;

pub use config::Config;
pub use error::{Error, Result};
pub use server::{Server, ServerBuilder, ShutdownHandle};
