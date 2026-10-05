//! Anchor: a Minecraft: Bedrock Edition server library.
//!
//! A server is a binary of its own that depends on Anchor by path or git, with nothing else to
//! set up: the fixed copies of dependencies in Anchor's `vendor/` come with it as path
//! dependencies.
//!
//! ```toml
//! [dependencies]
//! anchor = { git = "<repository>" }
//! ```
//!
//! ```no_run
//! fn main() -> anchor::Result<()> {
//!     anchor::Server::builder()
//!         .config(anchor::Config::default().load("anchor.toml")?)
//!         .build()?
//!         .run()
//! }
//! ```
//!
//! The binary's profiles must keep `panic = "unwind"`, the default: a panicking connection task
//! closes only that connection.
//!
//! # Build, start and stop times
//!
//! ## Build
//!
//! Anchor is most of a server's build, and Cargo builds it again only when it changes. Two
//! settings in the server's project make the debug build and each rebuild after a change to the
//! server's own code shorter; neither changes the code a release build makes.
//!
//! ```toml
//! # Cargo.toml: dependencies, Anchor among them, keep only file and line for panics and
//! # backtraces instead of full debug info. Your own crate keeps full debug info.
//! [profile.dev.package."*"]
//! debug = "line-tables-only"
//! ```
//!
//! ```toml
//! # .cargo/config.toml, on Windows: the LLVM linker that comes with Rust instead of link.exe.
//! [target.x86_64-pc-windows-msvc]
//! linker = "rust-lld.exe"
//! ```
//!
//! Measured with an empty server, 8 build jobs: a clean debug build 25% shorter, a debug rebuild
//! after changing Anchor 35% shorter, a rebuild after changing only the server's code 55% shorter
//! in debug and 14% shorter in release.

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
mod fs;
pub mod import;
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
pub mod replay;
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
