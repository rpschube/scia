//! The OS-neutral now-playing type system shared by every metadata backend.
//!
//! These types are the contract a backend speaks; the MPRIS backend
//! ([`crate::mpris`]) and any other platform backend (e.g. a Windows SMTC
//! backend) produce exactly the same [`MetaEvent`] stream from them, so a
//! consumer is written once against this surface and never learns which OS it
//! is running on.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Whether the winning player is currently advancing, held, or idle.
///
/// `Stopped` is the "no active session" state a selector treats as absence; a
/// `Paused` player still carries valid now-playing metadata and is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PlaybackStatus {
    /// The player is actively playing.
    Playing,
    /// The player holds a loaded track but is not advancing.
    Paused,
    /// The player has no active track. Treated as absence by selection.
    #[default]
    Stopped,
}

impl PlaybackStatus {
    /// Whether this player is actively playing. Selection policies prefer a
    /// playing session over any paused or stopped one.
    #[must_use]
    pub fn is_playing(self) -> bool {
        matches!(self, PlaybackStatus::Playing)
    }
}

/// A playback position sample, tagged with the instant it was read so a
/// consumer can extrapolate the live position without polling the player.
///
/// `position` is the reported offset into the track at `reported_at`; a
/// consumer that wants the current offset while `status` is
/// [`PlaybackStatus::Playing`] adds `reported_at.elapsed()` to it (clamped to
/// `length` when known).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositionInfo {
    /// The playback offset into the track as the player reported it.
    pub position: Duration,
    /// The total track length, when the player publishes it.
    pub length: Option<Duration>,
    /// The instant `position` was sampled, the anchor for extrapolation.
    pub reported_at: Instant,
}

/// A snapshot of what a single player is playing right now.
///
/// Every text field is optional because partial metadata is normal — a player
/// may publish a title with no album, or nothing but a status. `track_key` is
/// always present: it is a stable, normalized identity for the track (see
/// [`track_key`]) used to correlate a later [`MetaEvent::Artwork`] event with
/// the track it belongs to, and to key an artwork cache.
#[derive(Clone, Debug, PartialEq)]
pub struct NowPlaying {
    /// Track title, if published.
    pub title: Option<String>,
    /// Track artist(s), joined into one string if the player lists several.
    pub artist: Option<String>,
    /// Album title, if published.
    pub album: Option<String>,
    /// Stable cache identity for this track, derived from the metadata fields.
    pub track_key: String,
    /// Current playback status of the player this snapshot came from.
    pub status: PlaybackStatus,
    /// Position/length info when the player provides it.
    pub position: Option<PositionInfo>,
    /// Identity of the application that owns the session (the Windows
    /// `AppUserModelId` under SMTC, the MPRIS bus name under Linux), when known.
    /// It lets a downstream consumer apply app-specific handling — for example,
    /// cropping the letterbox padding Spotify bakes into its SMTC thumbnails —
    /// without this crate ever touching pixels. It is not part of `track_key`.
    pub source_app: Option<String>,
}

impl NowPlaying {
    /// Build a snapshot, deriving [`NowPlaying::track_key`] from the metadata.
    /// `source_app` is the owning application's identity (bus name / AUMID) when
    /// the backend knows it; it does not affect the derived `track_key`.
    pub fn new(
        title: Option<String>,
        artist: Option<String>,
        album: Option<String>,
        status: PlaybackStatus,
        position: Option<PositionInfo>,
        source_app: Option<String>,
    ) -> Self {
        let track_key = track_key(artist.as_deref(), album.as_deref(), title.as_deref());
        Self {
            title,
            artist,
            album,
            track_key,
            status,
            position,
            source_app,
        }
    }
}

/// The stable cache identity for a track: artist, album and title normalized
/// (trimmed, internal whitespace collapsed, lowercased) and joined with a unit
/// separator. Two spellings that differ only in case or spacing map to the same
/// key, so an artwork event published a beat after the track event still
/// matches, and an artwork cache keyed on it survives a metadata re-publish.
pub fn track_key(artist: Option<&str>, album: Option<&str>, title: Option<&str>) -> String {
    fn norm(s: Option<&str>) -> String {
        s.unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }
    format!("{}\u{1f}{}\u{1f}{}", norm(artist), norm(album), norm(title))
}

/// A resolved reference to where a track's artwork lives, produced by parsing a
/// player's art URL. The fetch of the actual bytes happens off the event thread
/// (see [`crate::fetch`]); this is only the address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtworkRef {
    /// A local file to read directly (from a `file://` URL).
    File(PathBuf),
    /// An `http`/`https` URL to fetch.
    Url(String),
    /// Bytes decoded inline from a `data:` URL — already the encoded image.
    Inline(Vec<u8>),
}

impl ArtworkRef {
    /// Parse a player-published art location into a reference, or `None` when
    /// it is empty or an unsupported scheme. Handles `file://` (percent-decoded
    /// to a path), `http`/`https` (kept as a URL to fetch), and `data:`
    /// (base64 or percent-encoded payload decoded to inline bytes).
    ///
    /// This does no platform-specific rewriting; a backend applies its own
    /// quirk fixes (e.g. the Spotify CDN rewrite) to the raw string first.
    pub fn parse(art_url: &str) -> Option<Self> {
        let url = art_url.trim();
        if url.is_empty() {
            return None;
        }
        if let Some(rest) = url.strip_prefix("file://") {
            // file:///path → "/path"; file://host/path → "/path" (host ignored).
            let path = match rest.find('/') {
                Some(0) => rest,
                Some(i) => &rest[i..],
                None => return None,
            };
            let decoded = percent_decode(path);
            return Some(ArtworkRef::File(PathBuf::from(decoded)));
        }
        if url.starts_with("http://") || url.starts_with("https://") {
            return Some(ArtworkRef::Url(url.to_string()));
        }
        if let Some(rest) = url.strip_prefix("data:") {
            let (meta, data) = rest.split_once(',')?;
            let bytes = if meta.split(';').any(|p| p.eq_ignore_ascii_case("base64")) {
                base64_decode(data)?
            } else {
                percent_decode(data).into_bytes()
            };
            if bytes.is_empty() {
                return None;
            }
            return Some(ArtworkRef::Inline(bytes));
        }
        None
    }
}

/// An event pushed by a backend over the channel handed to its `start`
/// constructor. The stream is the entire downstream contract: a consumer reacts
/// to these and never polls a player.
#[derive(Clone, Debug)]
pub enum MetaEvent {
    /// The winning player changed track, status, or became the winner. Carries
    /// the full current [`NowPlaying`]; artwork, if any, follows separately.
    TrackChanged(NowPlaying),
    /// Encoded artwork bytes (JPEG/PNG as the player published them) for the
    /// track identified by `track_key`. Arrives asynchronously after the
    /// matching [`MetaEvent::TrackChanged`] — often a beat later — and may be
    /// absent entirely for a track that publishes no art.
    Artwork {
        /// The [`NowPlaying::track_key`] these bytes belong to.
        track_key: String,
        /// Raw encoded image bytes.
        bytes: Vec<u8>,
        /// Identity of the application the artwork came from (the Windows
        /// `AppUserModelId` / MPRIS bus name), mirroring
        /// [`NowPlaying::source_app`] so the palette stage can decide whether
        /// the bytes need app-specific cropping before it decodes them. `None`
        /// when the backend does not know the source.
        source_app: Option<String>,
    },
    /// The media session went away: no player is active. This is a normal
    /// state (nothing is playing), never an error.
    Cleared,
}

/// A running backend. Constructed by a backend's `start` function; joins its
/// worker threads when dropped. Holding it keeps the backend running; dropping
/// it stops the threads and blocks until they have finished.
///
/// Shutdown is two steps, in order: set the shared stop flag, then fire every
/// registered *waker*. A flag alone only stops a thread that is polling it; a
/// thread parked in an async `await` (as the MPRIS backend is, inside its
/// D-Bus reconcile loop) never observes it. Each waker is a one-shot trigger a
/// backend supplies to cancel such a park — e.g. closing the channel the MPRIS
/// loop races its reconcile against — so the join that follows returns promptly
/// instead of blocking forever. A backend whose loop already polls the flag on
/// a short cadence (the Windows SMTC backend) registers no waker.
pub struct MetaHandle {
    stop: Arc<AtomicBool>,
    wakers: Vec<Box<dyn FnOnce() + Send>>,
    joins: Vec<JoinHandle<()>>,
}

impl MetaHandle {
    /// Wrap a backend's stop flag, its shutdown wakers, and its worker threads.
    /// Backends construct this; the flag is shared with the threads, which
    /// observe it and unwind, and each waker is fired once (before the joins) to
    /// cancel a thread parked in an `await` rather than polling the flag. Pass an
    /// empty `wakers` vector when every thread already polls the flag promptly.
    pub(crate) fn new(
        stop: Arc<AtomicBool>,
        wakers: Vec<Box<dyn FnOnce() + Send>>,
        joins: Vec<JoinHandle<()>>,
    ) -> Self {
        Self {
            stop,
            wakers,
            joins,
        }
    }

    /// Stop the backend and wait for its threads to finish. Equivalent to
    /// dropping the handle, but explicit at a call site.
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for MetaHandle {
    fn drop(&mut self) {
        // Set the flag first (the error-path idle loop and the fetch worker poll
        // it), then fire every waker so a thread blocked in an async await is
        // cancelled and can exit — only then join, or the join could block
        // forever on a thread that never sees the flag.
        self.stop.store(true, Ordering::Relaxed);
        for waker in self.wakers.drain(..) {
            waker();
        }
        for join in self.joins.drain(..) {
            let _ = join.join();
        }
    }
}

/// Percent-decode a URL component (`%XX` → byte), passing everything else
/// through. Invalid escapes are left verbatim. Bytes are interpreted as UTF-8
/// lossily so the result is always a valid `String`.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode a standard-alphabet base64 string (`A-Za-z0-9+/`, `=` padding),
/// skipping ASCII whitespace. Returns `None` on a malformed body.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::new();
    for &b in s.as_bytes() {
        if b == b'=' || b.is_ascii_whitespace() {
            continue;
        }
        let v = val(b)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_key_normalizes_case_and_whitespace() {
        let a = track_key(Some("Daft  Punk"), Some("Discovery"), Some("One More Time"));
        let b = track_key(
            Some("daft punk"),
            Some("  discovery "),
            Some("one more time"),
        );
        assert_eq!(a, b);
    }

    #[test]
    fn track_key_is_stable_and_field_separated() {
        // Distinct fields do not collide across the separator boundary.
        let ab = track_key(Some("ab"), None, None);
        let a_b = track_key(Some("a"), Some("b"), None);
        assert_ne!(ab, a_b);
    }

    #[test]
    fn track_key_handles_all_absent() {
        assert_eq!(track_key(None, None, None), "\u{1f}\u{1f}");
    }

    #[test]
    fn nowplaying_new_derives_key() {
        let np = NowPlaying::new(
            Some("Title".into()),
            Some("Artist".into()),
            Some("Album".into()),
            PlaybackStatus::Playing,
            None,
            Some("org.mpris.MediaPlayer2.spotify".into()),
        );
        assert_eq!(
            np.track_key,
            track_key(Some("Artist"), Some("Album"), Some("Title"))
        );
        // source_app is carried verbatim and never folded into the key.
        assert_eq!(
            np.source_app.as_deref(),
            Some("org.mpris.MediaPlayer2.spotify")
        );
    }

    #[test]
    fn nowplaying_source_app_does_not_affect_key() {
        let a = NowPlaying::new(
            Some("T".into()),
            None,
            None,
            PlaybackStatus::Playing,
            None,
            Some("AppA".into()),
        );
        let b = NowPlaying::new(
            Some("T".into()),
            None,
            None,
            PlaybackStatus::Paused,
            None,
            Some("AppB".into()),
        );
        assert_eq!(a.track_key, b.track_key);
    }

    #[test]
    fn playback_status_only_playing_is_active() {
        assert!(PlaybackStatus::Playing.is_playing());
        assert!(!PlaybackStatus::Paused.is_playing());
        assert!(!PlaybackStatus::Stopped.is_playing());
    }

    #[test]
    fn artwork_parse_file_url_is_decoded() {
        let got = ArtworkRef::parse("file:///music/cover/My%20Art.png");
        assert_eq!(
            got,
            Some(ArtworkRef::File(PathBuf::from("/music/cover/My Art.png")))
        );
    }

    #[test]
    fn artwork_parse_file_url_with_host() {
        let got = ArtworkRef::parse("file://localhost/tmp/a.jpg");
        assert_eq!(got, Some(ArtworkRef::File(PathBuf::from("/tmp/a.jpg"))));
    }

    #[test]
    fn artwork_parse_https_kept_as_url() {
        let got = ArtworkRef::parse("https://i.scdn.co/image/abc");
        assert_eq!(
            got,
            Some(ArtworkRef::Url("https://i.scdn.co/image/abc".into()))
        );
    }

    #[test]
    fn artwork_parse_data_base64() {
        // "Hi" base64-encoded is "SGk=".
        let got = ArtworkRef::parse("data:image/png;base64,SGk=");
        assert_eq!(got, Some(ArtworkRef::Inline(b"Hi".to_vec())));
    }

    #[test]
    fn artwork_parse_empty_and_unknown_scheme() {
        assert_eq!(ArtworkRef::parse(""), None);
        assert_eq!(ArtworkRef::parse("   "), None);
        assert_eq!(ArtworkRef::parse("spotify:track:xyz"), None);
    }

    #[test]
    fn base64_decode_roundtrip_len() {
        assert_eq!(base64_decode("TWFu").unwrap(), b"Man");
        assert_eq!(base64_decode("TWE=").unwrap(), b"Ma");
        assert_eq!(base64_decode("TQ==").unwrap(), b"M");
    }

    #[test]
    fn percent_decode_passthrough_and_escape() {
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }
}
