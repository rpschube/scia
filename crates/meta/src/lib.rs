//! Now-playing metadata for scia: it reads track title, artist and album from
//! whatever player is active and derives album-art colour palettes that scenes
//! can theme themselves from. It builds on `scia-core` and exposes this
//! information as another input alongside the audio feature bus.

/// The crate name, resolved at compile time from Cargo metadata.
pub const NAME: &str = env!("CARGO_PKG_NAME");
