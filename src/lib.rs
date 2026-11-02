//! Anchor: a Minecraft: Bedrock Edition server library.
//!
//! A server is a binary of its own that depends on Anchor by path or git, with nothing else to
//! set up: the fixed copies of dependencies in Anchor's `vendor/` come with it as path
//! dependencies, so a new Anchor commit fetches nothing but Anchor.
//!
//! ```toml
//! [dependencies]
//! anchor = { git = "https://github.com/jv2w/Anchor", rev = "<commit>" }
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
//! closes only that connection. A panic in a callback that runs in the tick (a listener, a task, a
//! command) ends the server without saving: [`Server::run`] writes the input log to `data/crash/`
//! and returns [`Error::TickPanicked`]. [`Server::replay`] runs that log again.
//!
//! Items that have a counterpart in the API table carry its names as search aliases, so
//! searching these docs for `Player::getName` finds [`Player::name`](player::Player::name). The
//! design notes (threads and tick, network, protocol, storage, data sources, dependencies,
//! testing, and every `anchor.toml` key) are in the repository's `.claude/docs/` folder.
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
//!
//! ## Start and stop
//!
//! A stop writes only the data files that changed and compacts none of them, so it takes well
//! under a second. What a start costs depends on two settings in `anchor.toml`:
//!
//! ```toml
//! [general]
//! # A start backs up every data file that existed into data/backups while the server runs;
//! # false skips the backup.
//! backups = true
//! [developer]
//! # "after-unclean-shutdown": the files are read through only when the last run did not stop
//! # normally (a crash or a killed process), instead of on every start.
//! integrity-check = "after-unclean-shutdown"
//! ```
//!
//! Measured with an imported world of 59,740 chunks (381 MB of data files), a release build and
//! `backups = false`: a start after a normal stop took under 0.1 s with
//! `integrity-check = "after-unclean-shutdown"` and 5.6 to 16.4 s with `"always"`, the default,
//! almost all of it spent reading the files from disk; a stop took 0.2 to 0.5 s.

// Per-connection panic isolation needs unwinding, and the binary's profile decides the strategy.
#[cfg(not(panic = "unwind"))]
compile_error!("anchor requires panic = \"unwind\"");

pub mod action;
mod args;
mod auth;
pub mod block;
mod bucket;
mod clock;
pub mod command;
pub mod config;
mod console;
mod crypto;
pub mod custom;
pub mod data;
pub mod display;
pub mod effect;
pub mod entity;
mod error;
pub mod event;
pub mod export;
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
mod udp;
pub mod world;

pub use config::Config;
pub use error::{Error, Result};
pub use server::{Server, ServerBuilder, ShutdownHandle};
pub use tick::{Context, SentPacket};
