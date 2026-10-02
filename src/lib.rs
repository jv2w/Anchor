//! Anchor: a Minecraft: Bedrock Edition server library.

// Per-connection panic isolation needs unwinding, and the binary's profile decides the strategy.
#[cfg(not(panic = "unwind"))]
compile_error!("anchor requires panic = \"unwind\"");
