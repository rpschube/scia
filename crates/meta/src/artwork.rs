//! The cross-platform **artwork debounce / retry driver** and byte-plumbing
//! helpers.
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
    /// A usable image was obtained; emit these bytes and stop.
    Emit(Vec<u8>),
    /// Every attempt was exhausted without usable bytes; stop quietly.
    GiveUp,
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
    /// Set once usable bytes have been recorded.
    done: Option<Vec<u8>>,
}

impl ArtworkDriver {
    /// Start a fresh campaign under `policy`.
    #[must_use]
    pub fn new(policy: RetryPolicy) -> Self {
        Self {
            policy,
            attempts: 0,
            done: None,
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
            return ArtworkStep::GiveUp;
        }
        ArtworkStep::Fetch {
            delay: self.policy.delay_before(self.attempts),
        }
    }

    /// Record the outcome of one fetch attempt. `bytes` is whatever the stream
    /// read produced (possibly empty or `None`); it is accepted only if
    /// [`is_usable_artwork`] passes, otherwise the attempt counts as a miss and
    /// the campaign retries until `max_attempts` is reached.
    pub fn record(&mut self, bytes: Option<&[u8]>) {
        self.attempts += 1;
        if let Some(b) = bytes
            && is_usable_artwork(b)
        {
            self.done = Some(b.to_vec());
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
        d.record(None);
        // Second attempt returns an empty buffer -> still a miss.
        assert!(matches!(d.next_step(), ArtworkStep::Fetch { .. }));
        d.record(Some(&[]));
        // Third attempt returns real PNG bytes.
        let art = png(256);
        d.record(Some(&art));
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
            d.record(None);
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
            d.record(None);
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
}
