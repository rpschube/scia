//! Now-playing metadata for scia: it reads track title, artist and album from
//! whatever player is active and derives album-art colour palettes that scenes
//! can theme themselves from. It builds on `scia-core` and exposes this
//! information as another input alongside the audio feature bus.
//!
//! The crate is split into a platform-neutral core and per-platform backends:
//!
//! * [`model`] — the shared type system: [`NowPlaying`](model::NowPlaying),
//!   [`Artwork`](model::Artwork), [`MetaEvent`](model::MetaEvent) and the
//!   [`MetaSender`](model::MetaSender) channel contract every backend pushes
//!   over.
//! * [`select`] — the session-selection policy (playing wins, then most recent
//!   activity, then a deterministic tie-break), pure and unit-tested off any
//!   particular platform.
//! * [`artwork`] — the debounce/retry driver and encoded-image sniffing that
//!   tame late thumbnail swaps, also pure and unit-tested.
//! * [`smtc`] — the Windows backend over the System Media Transport Controls,
//!   compiled only on Windows.
//!
//! The MPRIS (Linux) backend lands alongside this on the same shared `model`
//! surface.

// COM interop in the Windows backend needs a tightly-scoped `unsafe` for the
// apartment init; everything else in the crate is safe. Mirror `scia-core`:
// deny unsafe crate-wide, and let the Windows module opt in explicitly.
#![deny(unsafe_code)]

/// The crate name, resolved at compile time from Cargo metadata.
pub const NAME: &str = env!("CARGO_PKG_NAME");

pub mod artwork;
pub mod model;
pub mod select;

/// The Windows System Media Transport Controls backend. Compiled only on
/// Windows; the shared `model`/`select`/`artwork` surface it drives is
/// platform-neutral and available everywhere.
#[cfg(windows)]
#[allow(unsafe_code)]
pub mod smtc;

pub use model::{Artwork, MetaEvent, MetaReceiver, MetaSender, NowPlaying, PlaybackStatus};
