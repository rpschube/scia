//! Terminal frontend for scia: it owns the terminal, drives the render loop at
//! the target frame rate, and paints the frames produced by the scene engine
//! from the core's live audio features. It draws on `scia-core` for features
//! and `scia-scenes` for the scenes it renders, keeping all terminal-specific
//! concerns out of those lower layers.

/// The crate name, resolved at compile time from Cargo metadata.
pub const NAME: &str = env!("CARGO_PKG_NAME");
