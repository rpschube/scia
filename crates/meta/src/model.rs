//! The cross-platform now-playing type system: the metadata snapshot, the
//! artwork payload, the event enum a backend emits, and the channel contract
//! backends push over.
//!
//! Every type here is platform-neutral and dependency-free so the selection
//! policy, the artwork retry driver and the platform backends (SMTC on
//! Windows, MPRIS on Linux) all speak the same vocabulary. A backend owns one
//! thread that subscribes to its platform's session events and pushes
//! [`MetaEvent`]s over an [`mpsc::Sender`]; the rest of scia reads them off the
//! matching receiver as another input alongside the audio feature bus.

use std::sync::mpsc;

/// Playback state of a media session, normalised across platforms.
///
/// Windows' `GlobalSystemMediaTransportControlsSessionPlaybackStatus` and the
/// MPRIS `PlaybackStatus` property both map onto this. Only [`Playing`] is
/// load-bearing for selection; the rest are carried through so the policy can
/// prefer an active session over an idle one and so callers can distinguish a
/// paused track from a stopped one.
///
/// [`Playing`]: PlaybackStatus::Playing
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlaybackStatus {
    /// The session exists but is closed (no content).
    Closed,
    /// Content is opened but not yet started.
    Opened,
    /// The session is mid-transition between tracks.
    Changing,
    /// Playback is stopped.
    Stopped,
    /// Playback is active.
    Playing,
    /// Playback is paused.
    Paused,
}

impl PlaybackStatus {
    /// Whether this session is actively playing. The selection policy prefers
    /// playing sessions over every other state.
    #[must_use]
    pub fn is_playing(self) -> bool {
        matches!(self, PlaybackStatus::Playing)
    }
}

/// A snapshot of the current track's textual metadata.
///
/// Every field is optional: a session can be active while a player has not yet
/// populated (or does not expose) a given field. `source_app` carries the
/// originating application's identity — the Windows `AppUserModelId` or the
/// MPRIS bus name — so downstream consumers can apply app-specific handling
/// (for example, cropping the letterbox padding Spotify bakes into its SMTC
/// thumbnails) without this crate ever touching pixels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NowPlaying {
    /// Track title.
    pub title: Option<String>,
    /// Primary artist.
    pub artist: Option<String>,
    /// Album title.
    pub album: Option<String>,
    /// Identity of the application that owns the session (Windows
    /// `AppUserModelId` / MPRIS bus name), when known.
    pub source_app: Option<String>,
}

impl NowPlaying {
    /// Whether every textual field is absent. A backend may still emit such a
    /// snapshot (a session exists but exposes no metadata yet); it is distinct
    /// from [`MetaEvent::Cleared`], which means no session exists at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.artist.is_none() && self.album.is_none()
    }
}

/// Album-art bytes exactly as the platform delivered them — an encoded image
/// (PNG/JPEG/…), never decoded pixels. `source_app` mirrors
/// [`NowPlaying::source_app`] so the palette module can decide whether the
/// bytes need app-specific cropping before it decodes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artwork {
    /// The encoded image bytes as delivered by the platform.
    pub bytes: Vec<u8>,
    /// Identity of the application the artwork came from, when known.
    pub source_app: Option<String>,
}

/// An event emitted by a now-playing backend over the [`MetaSender`] contract.
///
/// A backend emits [`Track`] whenever the winning session's textual metadata
/// changes, [`Artwork`] when its album art has been fetched, and [`Cleared`]
/// when no media session exists at all — the normal idle state, never an error.
///
/// [`Track`]: MetaEvent::Track
/// [`Artwork`]: MetaEvent::Artwork
/// [`Cleared`]: MetaEvent::Cleared
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaEvent {
    /// The winning session's textual metadata (title/artist/album) changed.
    Track(NowPlaying),
    /// Album art for the current track has been fetched.
    Artwork(Artwork),
    /// No media session is present. A normal, quiet idle state — not a failure.
    Cleared,
}

/// The channel a backend pushes [`MetaEvent`]s over. The backend owns the
/// [`mpsc::Sender`]; the consumer owns the matching [`mpsc::Receiver`].
pub type MetaSender = mpsc::Sender<MetaEvent>;

/// The receiving half of the [`MetaSender`] contract.
pub type MetaReceiver = mpsc::Receiver<MetaEvent>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playing_is_the_only_active_state() {
        assert!(PlaybackStatus::Playing.is_playing());
        for s in [
            PlaybackStatus::Closed,
            PlaybackStatus::Opened,
            PlaybackStatus::Changing,
            PlaybackStatus::Stopped,
            PlaybackStatus::Paused,
        ] {
            assert!(!s.is_playing(), "{s:?} must not count as playing");
        }
    }

    #[test]
    fn now_playing_empty_tracks_only_text_fields() {
        let mut np = NowPlaying::default();
        assert!(np.is_empty());
        // source_app alone does not make a snapshot non-empty.
        np.source_app = Some("App".into());
        assert!(np.is_empty());
        np.title = Some("Song".into());
        assert!(!np.is_empty());
    }
}
