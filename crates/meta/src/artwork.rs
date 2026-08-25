//! The **SMTC artwork debounce / retry driver** and encoded-image helpers.
//!
//! Album art is the awkward part of a now-playing backend. Players swap the
//! thumbnail *after* they publish the new title — Spotify in particular can lag
//! by a few hundred milliseconds — so a fetch fired the instant the track
//! changes often returns nothing, an empty stream, or the *previous* track's
//! art. The fix is twofold and entirely policy, so it lives here as pure,
//! testable code with no platform or timing dependency:
//!
//! * a **debounce** window collapses a burst of rapid track/property changes
//!   into a single fetch, and
//! * a bounded **retry with backoff** re-attempts the fetch when it comes back
//!   empty or unusable, giving a lagging player time to populate the thumbnail.
//!
//! The driver computes *what to do and when* ([`ArtworkStep`]); the platform
//! backend performs the actual stream read and sleeps for the delays the driver
//! hands back. Splitting it this way keeps every scheduling decision on the
//! Linux-testable side of the `cfg(windows)` line.
//!
//! [`is_usable_artwork`] is the acceptance predicate the driver uses to decide
//! whether a fetch succeeded: bytes that are absent, too small, or not a
//! recognisable image are treated as "not ready yet" and drive a retry.
//!
//! # Why this exists beside [`FetchScheduler`](crate::FetchScheduler)
//!
//! The crate has two artwork schedulers because the two backends fetch art in
//! fundamentally different shapes, and forcing them through one type would
//! misrepresent one of them:
//!
//! * [`FetchScheduler`](crate::FetchScheduler) is **address-based with an
//!   injected clock**. MPRIS resolves a player's `mpris:artUrl` to an
//!   [`ArtworkRef`](crate::ArtworkRef) (a `file://` path, an `http(s)` URL, or
//!   inline `data:` bytes) and hands that address to a worker that fetches it
//!   off the event thread; the scheduler is told the current [`Instant`] and
//!   tracks *one* pending [`ArtworkRef`] through debounce and backoff. It owns
//!   *when to fetch which address*.
//! * [`ArtworkDriver`] is **live-handle based with a sleep-duration interface**.
//!   SMTC has no addressable art: each attempt re-queries the winning session's
//!   thumbnail stream directly from a COM handle — a handle that cannot be an
//!   [`ArtworkRef`](crate::ArtworkRef) (it is neither `Clone`/`Eq` nor `Send`
//!   into that value type), so there is nothing for `FetchScheduler` to hold.
//!   The driver instead hands the backend a [`Duration`] to sleep on its
//!   interruptible `recv_timeout`, and judges each re-query with
//!   [`is_usable_artwork`] to reject the empty or placeholder thumbnails Windows
//!   returns mid-swap. It owns *when to re-query and whether the bytes count*.
//!
//! Both encode the same intent — debounce, then bounded exponential backoff —
//! but against different I/O models. `FetchScheduler` is the canonical policy
//! for address-based fetches; `ArtworkDriver` is its SMTC-shaped counterpart,
//! adding the encoded-image acceptance and backoff ceiling that a live-thumbnail
//! re-query needs.
//!
//! [`Duration`]: std::time::Duration
//! [`Instant`]: std::time::Instant

use std::time::Duration;

/// Retry/debounce policy for one artwork fetch campaign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Delay before the *first* attempt, collapsing a burst of change
    /// notifications into one fetch and giving a lagging player a head start.
    pub debounce: Duration,
    /// Maximum number of fetch attempts before giving up (>= 1).
    pub max_attempts: u32,
    /// Backoff before the *second* attempt; it doubles each further attempt up
    /// to `max_backoff`.
    pub base_backoff: Duration,
    /// Ceiling for the doubling backoff.
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    /// The US-META-1 contract: a ~250 ms debounce, then a handful of retries
    /// with exponential backoff capped at 2 s — enough to outlast Spotify's
    /// late thumbnail swap without hammering the platform API.
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(250),
            max_attempts: 5,
            base_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(2),
        }
    }
}

impl RetryPolicy {
    /// The delay the backend should sleep *before* performing attempt `attempt`
    /// (0-indexed). Attempt 0 waits the debounce; each later attempt waits the
    /// doubling backoff, capped at `max_backoff`.
    #[must_use]
    pub fn delay_before(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return self.debounce;
        }
        // attempt 1 => base_backoff, attempt 2 => 2x, attempt 3 => 4x, ...
        let shift = attempt - 1;
        let scaled = self
            .base_backoff
            .checked_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX))
            .unwrap_or(self.max_backoff);
        scaled.min(self.max_backoff)
    }
}

/// What the backend should do next in an artwork fetch campaign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtworkStep {
    /// Sleep `delay`, then perform an attempt and feed the result back to
    /// [`ArtworkDriver::record`].
    Fetch { delay: Duration },
    /// A usable image was obtained (and it is *not* a suspect-stale hold-over
    /// from the previous track); emit these bytes and stop.
    Emit(Vec<u8>),
    /// Every attempt was exhausted with nothing but **suspect-stale** bytes —
    /// bytes byte-identical to the previous track's art while the metadata says
    /// the album changed (a player whose thumbnail lagged the whole campaign).
    /// Emit them anyway (stale art beats none), but the campaign is treated as
    /// *not having obtained confirmed art*, so the late-properties re-campaign
    /// ([`ArtCampaignTracker`]) stays armed to heal it. No user-visible label.
    EmitStale(Vec<u8>),
    /// Every attempt was exhausted without usable bytes; stop quietly.
    GiveUp,
}

/// A 64-bit FNV-1a hash of encoded artwork bytes — a cheap identity used to spot
/// a player still serving the *previous* track's thumbnail during the lag window
/// after a track change. Not cryptographic and never persisted; only two byte
/// blobs observed moments apart in one process are ever compared, so collision
/// risk is negligible and the same-album guard makes a collision harmless anyway.
#[must_use]
pub fn art_hash(bytes: &[u8]) -> u64 {
    // FNV-1a, 64-bit. Offset basis and prime per the reference specification.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The identity of the artwork most recently *emitted* for a source app: a cheap
/// [`art_hash`] of the emitted bytes plus the album they belonged to.
///
/// A fresh [`ArtworkDriver`] is seeded with the previous track's `PrevArt` so it
/// can recognise the lag window: if a fetch returns bytes whose hash matches the
/// previous emission *and* the album has since changed, the thumbnail has not
/// caught up yet and the bytes are treated as suspect-stale rather than emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrevArt {
    /// Hash of the previously emitted artwork bytes.
    pub hash: u64,
    /// The album those bytes belonged to (`None` if the player published none).
    pub album: Option<String>,
}

/// Drives one artwork fetch campaign through debounce and bounded retry.
///
/// Lifecycle: construct with [`ArtworkDriver::new`]; call [`next_step`] to learn
/// whether to fetch (and how long to wait first), emit, or give up; after each
/// fetch feed the outcome to [`record`]. The driver is a pure state machine —
/// it neither sleeps nor performs I/O — so the whole schedule is unit-testable.
///
/// [`next_step`]: ArtworkDriver::next_step
/// [`record`]: ArtworkDriver::record
#[derive(Debug, Clone)]
pub struct ArtworkDriver {
    policy: RetryPolicy,
    /// Number of attempts already performed.
    attempts: u32,
    /// Set once usable, non-suspect bytes have been recorded.
    done: Option<Vec<u8>>,
    /// Usable bytes that looked suspect-stale (byte-identical to the previous
    /// track's art under a changed album). Held as a last resort: if the whole
    /// campaign turns up nothing better, these are emitted via
    /// [`ArtworkStep::EmitStale`] rather than showing nothing.
    suspect: Option<Vec<u8>>,
    /// The previous track's emitted artwork identity for this source app, if
    /// known; the yardstick for the suspect-stale check.
    prev_art: Option<PrevArt>,
}

impl ArtworkDriver {
    /// Start a fresh campaign under `policy` with no previous-art context (the
    /// suspect-stale check is inert — every usable fetch is emitted as-is).
    #[must_use]
    pub fn new(policy: RetryPolicy) -> Self {
        Self::with_prev_art(policy, None)
    }

    /// Start a fresh campaign under `policy`, seeded with the previous track's
    /// emitted-artwork identity so the suspect-stale lag window can be detected.
    #[must_use]
    pub fn with_prev_art(policy: RetryPolicy, prev_art: Option<PrevArt>) -> Self {
        Self {
            policy,
            attempts: 0,
            done: None,
            suspect: None,
            prev_art,
        }
    }

    /// The next action to take. Does not mutate the driver; call [`record`]
    /// after actually performing a [`ArtworkStep::Fetch`].
    ///
    /// [`record`]: ArtworkDriver::record
    #[must_use]
    pub fn next_step(&self) -> ArtworkStep {
        if let Some(bytes) = &self.done {
            return ArtworkStep::Emit(bytes.clone());
        }
        if self.attempts >= self.policy.max_attempts {
            // Nothing confirmed; fall back to suspect-stale bytes if we held
            // any, else give up quietly.
            if let Some(bytes) = &self.suspect {
                return ArtworkStep::EmitStale(bytes.clone());
            }
            return ArtworkStep::GiveUp;
        }
        ArtworkStep::Fetch {
            delay: self.policy.delay_before(self.attempts),
        }
    }

    /// Record the outcome of one fetch attempt. `bytes` is whatever the stream
    /// read produced (possibly empty or `None`); `album` is the album the fetch's
    /// media properties reported (used only for the suspect-stale check).
    ///
    /// Usable bytes ([`is_usable_artwork`]) are accepted as confirmed art unless
    /// they are *suspect-stale* — byte-for-byte the previous track's emitted art
    /// while the album has changed — in which case they are held aside (not
    /// emitted) and the attempt still counts as a miss, so the campaign keeps
    /// retrying within its bounded policy for the real thumbnail to arrive. An
    /// empty, unusable, or `None` result is an ordinary miss.
    pub fn record(&mut self, bytes: Option<&[u8]>, album: Option<&str>) {
        self.attempts += 1;
        if let Some(b) = bytes
            && is_usable_artwork(b)
        {
            if self.is_suspect_stale(b, album) {
                self.suspect = Some(b.to_vec());
            } else {
                self.done = Some(b.to_vec());
            }
        }
    }

    /// Whether `bytes` are suspect-stale against the seeded [`PrevArt`]: identical
    /// hash to the previous emission **and** a different album. When either album
    /// is absent, or the albums match, the bytes are *not* suspect — two tracks
    /// on the same album legitimately share art, so an identical-hash match there
    /// is expected, not a lag artefact (and emitting it is harmless: the image is
    /// the same one either way).
    fn is_suspect_stale(&self, bytes: &[u8], album: Option<&str>) -> bool {
        let Some(prev) = &self.prev_art else {
            return false;
        };
        if art_hash(bytes) != prev.hash {
            return false;
        }
        match (album, prev.album.as_deref()) {
            (Some(cur), Some(prev_album)) => cur != prev_album,
            _ => false,
        }
    }

    /// How many attempts have been performed so far.
    #[must_use]
    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}

/// The minimum plausible size for a real thumbnail; anything smaller is treated
/// as a not-yet-ready placeholder.
const MIN_ARTWORK_BYTES: usize = 64;

/// A recognised encoded-image container, sniffed from the leading bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// PNG.
    Png,
    /// JPEG.
    Jpeg,
    /// GIF (87a/89a).
    Gif,
    /// Windows bitmap.
    Bmp,
    /// WebP (RIFF container with a `WEBP` fourcc).
    Webp,
}

/// Sniff the encoded-image format from magic bytes, or `None` if unrecognised.
///
/// Backends hand the palette module encoded bytes untouched; this only
/// classifies the container so [`is_usable_artwork`] can reject non-image
/// payloads. It never decodes pixels.
#[must_use]
pub fn sniff_image_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.len() >= 8 && bytes[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some(ImageFormat::Png);
    }
    if bytes.len() >= 3 && bytes[..3] == [0xFF, 0xD8, 0xFF] {
        return Some(ImageFormat::Jpeg);
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        return Some(ImageFormat::Gif);
    }
    if bytes.len() >= 2 && &bytes[..2] == b"BM" {
        return Some(ImageFormat::Bmp);
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(ImageFormat::Webp);
    }
    None
}

/// Whether `bytes` are a usable album-art image: large enough to be real and a
/// recognisable encoded-image container. An empty, tiny, or non-image payload
/// fails, which is what drives the retry loop while a player is still swapping
/// its thumbnail.
#[must_use]
pub fn is_usable_artwork(bytes: &[u8]) -> bool {
    bytes.len() >= MIN_ARTWORK_BYTES && sniff_image_format(bytes).is_some()
}

/// The distinct stages of one SMTC artwork fetch, in the order they run.
///
/// A fetch walks: re-read the session's media properties, take the thumbnail
/// reference off them, open a read stream, read its size, create a reader, load
/// the bytes, then copy them out. Any stage can fail with its own WinRT
/// `HRESULT`; the SMTC backend tags the failing stage with this enum so a probe
/// can report *which* step broke and distinguish, say, a `Thumbnail()` error
/// from an `OpenReadAsync` error. Platform-neutral so the label mapping is
/// unit-tested off Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchStage {
    /// `TryGetMediaPropertiesAsync` — re-read the winner's media properties.
    Props,
    /// `props.Thumbnail()` — take the thumbnail stream reference.
    Thumbnail,
    /// `OpenReadAsync` — open a read stream over the thumbnail.
    OpenRead,
    /// `stream.Size()` — read the stream length.
    Size,
    /// `DataReader::CreateDataReader` — wrap the stream in a reader.
    CreateReader,
    /// `LoadAsync` — pull the bytes into the reader's buffer.
    Load,
    /// `ReadBytes` — copy the loaded bytes out.
    ReadBytes,
}

impl FetchStage {
    /// A short, stable, log-safe label for the stage.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            FetchStage::Props => "props",
            FetchStage::Thumbnail => "thumbnail",
            FetchStage::OpenRead => "open-read",
            FetchStage::Size => "size",
            FetchStage::CreateReader => "create-reader",
            FetchStage::Load => "load",
            FetchStage::ReadBytes => "read-bytes",
        }
    }
}

/// What an artwork campaign should do next, decided by [`ArtCampaignTracker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtAction {
    /// Do not run a campaign this evaluation.
    Skip,
    /// The track identity or winning app changed: run a fresh campaign.
    Fresh,
    /// Same track, its previous campaign produced no art, and a properties
    /// event for the winning session arrived: run the one allowed follow-up
    /// campaign (covers a player that swaps its thumbnail late).
    Recampaign,
}

/// The terminal outcome of one artwork campaign, fed back to the tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CampaignOutcome {
    /// Confirmed artwork was obtained and emitted.
    Emitted,
    /// Only suspect-stale artwork was available; it was emitted as a best-effort
    /// fallback, but no *confirmed* art was obtained. Treated like [`GaveUp`] for
    /// re-campaign bookkeeping so the late-properties follow-up stays armed and
    /// can heal the display once the player swaps in the real thumbnail.
    ///
    /// [`GaveUp`]: CampaignOutcome::GaveUp
    EmittedStale,
    /// Every bounded attempt was exhausted with no usable artwork.
    GaveUp,
    /// A newer change superseded the campaign before it finished; nothing was
    /// concluded about this track's artwork.
    Abandoned,
}

/// Per-track artwork-campaign bookkeeping that makes late thumbnails recoverable
/// without ever looping.
///
/// The SMTC contract fetches art only when the track (or winning app) changes.
/// But some players — Spotify notably — publish the new title first and swap the
/// thumbnail a beat later, sometimes *after* the bounded campaign has already
/// given up. When that happens the player emits a `MediaPropertiesChanged` for
/// the same track; without this tracker that event does nothing and the track is
/// stuck art-less until an app restart.
///
/// The tracker grants **exactly one** follow-up campaign per track: it remembers
/// the track a campaign last ran for, whether that campaign obtained art, and
/// whether the single re-campaign has already been spent. A properties event for
/// the current art-less track triggers the re-campaign once; any further
/// properties events are ignored until the track changes, so a chatty player
/// cannot spin the backend.
#[derive(Debug, Default, Clone)]
pub struct ArtCampaignTracker {
    /// The track key the most recent campaign ran for.
    track: Option<String>,
    /// Whether that campaign obtained usable art.
    obtained: bool,
    /// Whether the one allowed follow-up campaign for `track` has been spent.
    recampaigned: bool,
}

impl ArtCampaignTracker {
    /// A fresh tracker that has seen no campaign yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide what campaign, if any, to run this evaluation.
    ///
    /// * `track_key` — the current winning track's key.
    /// * `changed` — the track identity or winning app changed since the last
    ///   emit (the existing "art needed" condition).
    /// * `props_for_winner` — this evaluation was triggered by a
    ///   `MediaPropertiesChanged` for the *winning* session (a bare play/pause,
    ///   a `SessionsChanged`, or the safety-net re-check are all `false`).
    #[must_use]
    pub fn decide(&self, track_key: &str, changed: bool, props_for_winner: bool) -> ArtAction {
        if changed {
            return ArtAction::Fresh;
        }
        if props_for_winner
            && self.track.as_deref() == Some(track_key)
            && !self.obtained
            && !self.recampaigned
        {
            return ArtAction::Recampaign;
        }
        ArtAction::Skip
    }

    /// Record that a campaign of `action` is about to start for `track_key`. A
    /// [`ArtAction::Fresh`] resets all state for the new track; a
    /// [`ArtAction::Recampaign`] spends the single follow-up; [`ArtAction::Skip`]
    /// is a no-op.
    pub fn begin(&mut self, track_key: &str, action: ArtAction) {
        match action {
            ArtAction::Fresh => {
                self.track = Some(track_key.to_string());
                self.obtained = false;
                self.recampaigned = false;
            }
            ArtAction::Recampaign => {
                self.recampaigned = true;
            }
            ArtAction::Skip => {}
        }
    }

    /// Record the [`CampaignOutcome`] of the campaign started by [`begin`]. An
    /// [`CampaignOutcome::Abandoned`] leaves the flags untouched: nothing was
    /// concluded, so a follow-up is still allowed once things settle.
    ///
    /// [`begin`]: ArtCampaignTracker::begin
    pub fn finish(&mut self, outcome: CampaignOutcome) {
        match outcome {
            CampaignOutcome::Emitted => self.obtained = true,
            // A stale best-effort emit does *not* count as confirmed art: leave
            // `obtained` false so a later winner-properties event still triggers
            // the one allowed re-campaign and heals the display.
            CampaignOutcome::EmittedStale | CampaignOutcome::GaveUp => self.obtained = false,
            CampaignOutcome::Abandoned => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(len: usize) -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        v.resize(len.max(8), 0);
        v
    }

    #[test]
    fn debounce_precedes_the_first_attempt() {
        let p = RetryPolicy::default();
        assert_eq!(p.delay_before(0), Duration::from_millis(250));
    }

    #[test]
    fn backoff_doubles_and_saturates_at_the_ceiling() {
        let p = RetryPolicy {
            debounce: Duration::from_millis(250),
            max_attempts: 8,
            base_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(2),
        };
        assert_eq!(p.delay_before(1), Duration::from_millis(250));
        assert_eq!(p.delay_before(2), Duration::from_millis(500));
        assert_eq!(p.delay_before(3), Duration::from_millis(1000));
        assert_eq!(p.delay_before(4), Duration::from_millis(2000));
        // Saturated at the ceiling, never overflowing.
        assert_eq!(p.delay_before(5), Duration::from_secs(2));
        assert_eq!(p.delay_before(31), Duration::from_secs(2));
        assert_eq!(p.delay_before(63), Duration::from_secs(2));
    }

    #[test]
    fn driver_emits_once_usable_bytes_arrive() {
        let mut d = ArtworkDriver::new(RetryPolicy::default());
        // First attempt returns nothing (player still swapping).
        assert!(matches!(d.next_step(), ArtworkStep::Fetch { .. }));
        d.record(None, None);
        // Second attempt returns an empty buffer -> still a miss.
        assert!(matches!(d.next_step(), ArtworkStep::Fetch { .. }));
        d.record(Some(&[]), None);
        // Third attempt returns real PNG bytes.
        let art = png(256);
        d.record(Some(&art), Some("Album"));
        match d.next_step() {
            ArtworkStep::Emit(bytes) => assert_eq!(bytes, art),
            other => panic!("expected Emit, got {other:?}"),
        }
        assert_eq!(d.attempts(), 3);
    }

    #[test]
    fn driver_gives_up_after_max_attempts() {
        let policy = RetryPolicy {
            max_attempts: 3,
            ..RetryPolicy::default()
        };
        let mut d = ArtworkDriver::new(policy);
        for _ in 0..3 {
            assert!(matches!(d.next_step(), ArtworkStep::Fetch { .. }));
            d.record(None, None);
        }
        assert_eq!(d.next_step(), ArtworkStep::GiveUp);
        assert_eq!(d.attempts(), 3);
    }

    #[test]
    fn fetch_delays_follow_debounce_then_backoff() {
        let policy = RetryPolicy {
            debounce: Duration::from_millis(250),
            max_attempts: 4,
            base_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(2),
        };
        let mut d = ArtworkDriver::new(policy);
        let mut delays = Vec::new();
        while let ArtworkStep::Fetch { delay } = d.next_step() {
            delays.push(delay);
            d.record(None, None);
        }
        assert_eq!(
            delays,
            vec![
                Duration::from_millis(250),  // debounce
                Duration::from_millis(250),  // base
                Duration::from_millis(500),  // 2x
                Duration::from_millis(1000)  // 4x
            ]
        );
    }

    #[test]
    fn sniffs_common_formats() {
        assert_eq!(sniff_image_format(&png(64)), Some(ImageFormat::Png));
        assert_eq!(
            sniff_image_format(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(sniff_image_format(b"GIF89a......"), Some(ImageFormat::Gif));
        assert_eq!(sniff_image_format(b"BM.........."), Some(ImageFormat::Bmp));
        let mut webp = b"RIFF\0\0\0\0WEBP".to_vec();
        webp.push(0);
        assert_eq!(sniff_image_format(&webp), Some(ImageFormat::Webp));
        assert_eq!(sniff_image_format(b"not an image"), None);
    }

    #[test]
    fn usable_requires_size_and_a_known_format() {
        assert!(!is_usable_artwork(&[]));
        assert!(!is_usable_artwork(&png(8))); // right magic, too small
        assert!(!is_usable_artwork(&vec![0u8; 1024])); // big enough, not an image
        assert!(is_usable_artwork(&png(256)));
    }

    #[test]
    fn fetch_stage_labels_are_distinct_and_stable() {
        let all = [
            FetchStage::Props,
            FetchStage::Thumbnail,
            FetchStage::OpenRead,
            FetchStage::Size,
            FetchStage::CreateReader,
            FetchStage::Load,
            FetchStage::ReadBytes,
        ];
        // Every stage maps to a unique label (a probe reader must be able to
        // tell the failing step apart).
        let mut labels: Vec<&str> = all.iter().map(|s| s.label()).collect();
        let count = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), count, "stage labels must be unique");
        // Spot-check the exact wording the probe log documents.
        assert_eq!(FetchStage::OpenRead.label(), "open-read");
        assert_eq!(FetchStage::ReadBytes.label(), "read-bytes");
    }

    #[test]
    fn tracker_fresh_campaign_on_change() {
        let t = ArtCampaignTracker::new();
        // A track/app change always runs a fresh campaign, regardless of trigger.
        assert_eq!(t.decide("a", true, false), ArtAction::Fresh);
        assert_eq!(t.decide("a", true, true), ArtAction::Fresh);
    }

    #[test]
    fn tracker_no_recampaign_without_a_properties_event() {
        let mut t = ArtCampaignTracker::new();
        t.begin("a", ArtAction::Fresh);
        t.finish(CampaignOutcome::GaveUp);
        // Same track, no art, but not a winner properties event (e.g. a bare
        // play/pause, a safety-net re-check): stay put.
        assert_eq!(t.decide("a", false, false), ArtAction::Skip);
    }

    #[test]
    fn tracker_recampaigns_once_on_late_properties_event() {
        let mut t = ArtCampaignTracker::new();
        t.begin("a", ArtAction::Fresh);
        t.finish(CampaignOutcome::GaveUp);
        // The late thumbnail swap arrives as a winner properties event.
        assert_eq!(t.decide("a", false, true), ArtAction::Recampaign);
        t.begin("a", ArtAction::Recampaign);
        t.finish(CampaignOutcome::GaveUp);
        // The single follow-up is spent; further properties events do nothing.
        assert_eq!(t.decide("a", false, true), ArtAction::Skip);
    }

    #[test]
    fn tracker_no_recampaign_after_success() {
        let mut t = ArtCampaignTracker::new();
        t.begin("a", ArtAction::Fresh);
        t.finish(CampaignOutcome::Emitted);
        // Art already obtained: a later properties event is not a retry trigger.
        assert_eq!(t.decide("a", false, true), ArtAction::Skip);
    }

    #[test]
    fn tracker_recampaign_budget_resets_on_new_track() {
        let mut t = ArtCampaignTracker::new();
        t.begin("a", ArtAction::Fresh);
        t.finish(CampaignOutcome::GaveUp);
        t.begin("a", ArtAction::Recampaign);
        t.finish(CampaignOutcome::GaveUp);
        assert_eq!(t.decide("a", false, true), ArtAction::Skip);
        // A new track gets its own fresh campaign and its own follow-up budget.
        assert_eq!(t.decide("b", true, false), ArtAction::Fresh);
        t.begin("b", ArtAction::Fresh);
        t.finish(CampaignOutcome::GaveUp);
        assert_eq!(t.decide("b", false, true), ArtAction::Recampaign);
    }

    #[test]
    fn tracker_abandoned_keeps_follow_up_available() {
        let mut t = ArtCampaignTracker::new();
        t.begin("a", ArtAction::Fresh);
        // Superseded before concluding — nothing decided about this track's art.
        t.finish(CampaignOutcome::Abandoned);
        // A properties event for the still-art-less track may still retry once.
        assert_eq!(t.decide("a", false, true), ArtAction::Recampaign);
    }

    // Two distinct real images: `A` is the previous track's art, `B` the new
    // track's real art. Different first bytes after the shared PNG magic keep
    // their hashes apart.
    fn art_a() -> Vec<u8> {
        let mut v = png(256);
        v[8] = 0xAA;
        v
    }
    fn art_b() -> Vec<u8> {
        let mut v = png(256);
        v[8] = 0xBB;
        v
    }

    #[test]
    fn art_hash_is_deterministic_and_discriminating() {
        // Stable across calls, equal for equal input.
        assert_eq!(art_hash(&art_a()), art_hash(&art_a()));
        // Different bytes hash differently (the whole discriminator's premise).
        assert_ne!(art_hash(&art_a()), art_hash(&art_b()));
        // A one-byte change is observed.
        let mut a2 = art_a();
        a2[8] = 0xAB;
        assert_ne!(art_hash(&art_a()), art_hash(&a2));
        // Known FNV-1a fixtures pin the constants (empty basis; "a").
        assert_eq!(art_hash(&[]), 0xcbf2_9ce4_8422_2325);
        assert_eq!(art_hash(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    fn prev_a() -> Option<PrevArt> {
        Some(PrevArt {
            hash: art_hash(&art_a()),
            album: Some("Album A".to_string()),
        })
    }

    #[test]
    fn suspect_stale_bytes_are_held_not_emitted_and_drive_a_retry() {
        let policy = RetryPolicy {
            max_attempts: 5,
            ..RetryPolicy::default()
        };
        let mut d = ArtworkDriver::with_prev_art(policy, prev_a());
        // The lagging player re-serves the PREVIOUS track's art, but the album
        // metadata has already advanced to the new track: suspect-stale.
        assert!(matches!(d.next_step(), ArtworkStep::Fetch { .. }));
        d.record(Some(&art_a()), Some("Album B"));
        // Not emitted; the campaign keeps retrying inside its bounded policy.
        assert!(matches!(d.next_step(), ArtworkStep::Fetch { .. }));
        assert_eq!(d.attempts(), 1);
    }

    #[test]
    fn campaign_recovers_when_the_real_thumbnail_finally_arrives() {
        let policy = RetryPolicy {
            max_attempts: 5,
            ..RetryPolicy::default()
        };
        let mut d = ArtworkDriver::with_prev_art(policy, prev_a());
        // First two attempts: still the stale previous art under the new album.
        d.record(Some(&art_a()), Some("Album B"));
        d.record(Some(&art_a()), Some("Album B"));
        assert!(matches!(d.next_step(), ArtworkStep::Fetch { .. }));
        // Third attempt: the real new art shows up (different hash) -> confirmed.
        let b = art_b();
        d.record(Some(&b), Some("Album B"));
        match d.next_step() {
            ArtworkStep::Emit(bytes) => assert_eq!(bytes, b),
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn same_album_identical_bytes_are_emitted_normally() {
        // Two tracks on the same album share art: an identical-hash fetch under
        // the SAME album is legitimate, not a lag artefact -> emit as confirmed.
        let mut d = ArtworkDriver::with_prev_art(RetryPolicy::default(), prev_a());
        d.record(Some(&art_a()), Some("Album A"));
        match d.next_step() {
            ArtworkStep::Emit(bytes) => assert_eq!(bytes, art_a()),
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn absent_album_disables_the_suspect_check() {
        // If either album is unknown we cannot claim the album changed, so an
        // identical-hash fetch is emitted rather than suppressed.
        let prev_no_album = Some(PrevArt {
            hash: art_hash(&art_a()),
            album: None,
        });
        let mut d = ArtworkDriver::with_prev_art(RetryPolicy::default(), prev_no_album);
        d.record(Some(&art_a()), Some("Album B"));
        assert!(matches!(d.next_step(), ArtworkStep::Emit(_)));

        // Symmetric: previous album known, current fetch reports none.
        let mut d = ArtworkDriver::with_prev_art(RetryPolicy::default(), prev_a());
        d.record(Some(&art_a()), None);
        assert!(matches!(d.next_step(), ArtworkStep::Emit(_)));
    }

    #[test]
    fn no_prev_art_context_never_flags_stale() {
        // The first-ever campaign (no previous emission) emits any usable bytes.
        let mut d = ArtworkDriver::new(RetryPolicy::default());
        d.record(Some(&art_a()), Some("Album A"));
        assert!(matches!(d.next_step(), ArtworkStep::Emit(_)));
    }

    #[test]
    fn exhausting_on_suspect_stale_emits_stale_as_a_last_resort() {
        let policy = RetryPolicy {
            max_attempts: 3,
            ..RetryPolicy::default()
        };
        let mut d = ArtworkDriver::with_prev_art(policy, prev_a());
        // Every attempt sees only the stale previous art under the new album.
        for _ in 0..3 {
            assert!(matches!(d.next_step(), ArtworkStep::Fetch { .. }));
            d.record(Some(&art_a()), Some("Album B"));
        }
        // Rather than show nothing, emit the held stale bytes — but as EmitStale.
        match d.next_step() {
            ArtworkStep::EmitStale(bytes) => assert_eq!(bytes, art_a()),
            other => panic!("expected EmitStale, got {other:?}"),
        }
        assert_eq!(d.attempts(), 3);
    }

    #[test]
    fn giveup_still_wins_when_no_suspect_bytes_were_held() {
        // Pure misses (no usable bytes at all) still give up quietly — the stale
        // fallback only triggers when suspect bytes were actually observed.
        let policy = RetryPolicy {
            max_attempts: 2,
            ..RetryPolicy::default()
        };
        let mut d = ArtworkDriver::with_prev_art(policy, prev_a());
        d.record(None, None);
        d.record(Some(&[]), None);
        assert_eq!(d.next_step(), ArtworkStep::GiveUp);
    }

    #[test]
    fn stale_success_leaves_the_recampaign_armed_and_heals_once() {
        // The healing path end to end at the tracker level: a stale best-effort
        // emit must behave, for re-campaign purposes, like it obtained no art.
        let mut t = ArtCampaignTracker::new();
        t.begin("z", ArtAction::Fresh);
        t.finish(CampaignOutcome::EmittedStale);
        // The late thumbnail swap arrives as a winner properties event: exactly
        // one fresh campaign is granted to replace the stale art.
        assert_eq!(t.decide("z", false, true), ArtAction::Recampaign);
        t.begin("z", ArtAction::Recampaign);
        t.finish(CampaignOutcome::Emitted);
        // Healed and confirmed: no further re-campaigns for this track.
        assert_eq!(t.decide("z", false, true), ArtAction::Skip);
    }

    #[test]
    fn stale_success_recampaign_budget_is_still_one() {
        // Even if the re-campaign itself only finds stale art again, the single
        // follow-up is spent — a chatty player cannot spin the backend.
        let mut t = ArtCampaignTracker::new();
        t.begin("z", ArtAction::Fresh);
        t.finish(CampaignOutcome::EmittedStale);
        assert_eq!(t.decide("z", false, true), ArtAction::Recampaign);
        t.begin("z", ArtAction::Recampaign);
        t.finish(CampaignOutcome::EmittedStale);
        assert_eq!(t.decide("z", false, true), ArtAction::Skip);
    }
}
