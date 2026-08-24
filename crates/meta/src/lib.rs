//! Now-playing metadata for scia: the title, artist, album and album art of
//! whatever the system is currently playing, delivered as an event stream that
//! scenes and other consumers can theme and react to alongside the audio
//! feature bus.
//!
//! # The backend contract
//!
//! A *backend* watches the OS media-session APIs and reports what is playing.
//! Every backend follows the same contract, so a consumer is written once and
//! never learns which platform it is on:
//!
//! - It runs on **its own thread(s)** and pushes [`MetaEvent`]s over an
//!   [`std::sync::mpsc::Sender`] the caller supplies.
//! - It is constructed by a `start(tx) -> MetaHandle` function. The returned
//!   [`MetaHandle`] owns the worker threads; **dropping it stops and joins
//!   them**.
//! - The event stream is the whole downstream contract. Consumers react to
//!   events and never poll a player:
//!   - [`MetaEvent::TrackChanged`] — the winning player changed track, status,
//!     or became the winner; carries the full [`NowPlaying`].
//!   - [`MetaEvent::Artwork`] — encoded image bytes for a `track_key`, arriving
//!     asynchronously *after* the track event (art is often published a beat
//!     late), fetched off the event thread with debounce and retry; may never
//!     arrive for a track with no art.
//!   - [`MetaEvent::Cleared`] — the media session went away. This is a **normal
//!     state** (nothing is playing), never an error.
//! - **Absence is normal.** No media session (a game, or nothing playing), no
//!   session bus, or a platform error all mean "metadata absent": the backend
//!   emits `Cleared`/nothing and idles quietly. It never crashes on absence.
//!
//! # Types
//!
//! [`NowPlaying`], [`PlaybackStatus`], [`PositionInfo`], [`ArtworkRef`],
//! [`MetaEvent`] and [`MetaHandle`] are OS-neutral and shared by every backend.
//! [`FetchScheduler`] is the shared, pure debounce/retry policy a backend uses
//! to fetch artwork off its event thread.
//!
//! # Backends
//!
//! - [`mpris`] — Linux, via the MPRIS D-Bus interface. Compiled only on Linux.
//!
//! A backend for another platform (e.g. Windows SMTC) produces the same
//! [`MetaEvent`] stream from the same shared types.

#![deny(unsafe_code)]

pub mod fetch;
mod types;

pub use fetch::FetchScheduler;
pub use types::{
    ArtworkRef, MetaEvent, MetaHandle, NowPlaying, PlaybackStatus, PositionInfo, track_key,
};

#[cfg(target_os = "linux")]
pub mod mpris;

/// The crate name, resolved at compile time from Cargo metadata.
pub const NAME: &str = env!("CARGO_PKG_NAME");
