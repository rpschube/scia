//! Scene engine for scia: it loads TOML scene presets, evaluates their
//! expressions and sandboxed scripts against the feature bus produced by
//! `scia-core`, and drives the per-frame state that a frontend renders. It
//! depends on the core for its feature inputs and stays free of any concrete
//! rendering backend.

/// The crate name, resolved at compile time from Cargo metadata.
pub const NAME: &str = env!("CARGO_PKG_NAME");
