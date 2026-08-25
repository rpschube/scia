//! Deterministic synthetic golden clips.
//!
//! A [`SynthSpec`] names a hardware-free clip: it drives the core synthetic feed
//! ([`scia_core::Signal`]) through the real per-hop DSP seam
//! ([`scia_core::HopProcessor`]) and projects each hop onto a
//! [`scia_core::FeatureFrame`]. Every sample of the source is a pure function of
//! its frame index and the DSP is deterministic, so [`SynthSpec::frames`]
//! regenerates byte-identical output on the same toolchain — which is what lets
//! a large synthetic clip live in the manifest as `generated = true` and be
//! checked by regenerate-and-compare instead of a committed fixture.

use std::time::Instant;

use scia_core::{
    FeatureFrame, HopProcessor, Signal, StreamFormat, sample_ring, synthetic::fill_signal,
};

/// The canonical hop size the whole DSP pipeline is built around.
pub const HOP_FRAMES: usize = 256;

/// A named synthetic clip: a signal, a tempo, a duration and a stream format.
#[derive(Clone, Copy, Debug)]
pub struct SynthSpec {
    /// The clip id (its manifest key and file stem).
    pub id: &'static str,
    /// A one-word genre label for the manifest.
    pub genre: &'static str,
    /// Tempo in BPM for the music/click signals.
    pub bpm: f32,
    /// Clip length in seconds (rounded to a whole number of hops).
    pub duration_s: f32,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel count (1 or 2).
    pub channels: u16,
}

impl SynthSpec {
    /// The hop cadence in milliseconds ([`HOP_FRAMES`] at the clip's rate).
    #[must_use]
    pub fn hop_ms(&self) -> f32 {
        HOP_FRAMES as f32 / self.sample_rate as f32 * 1000.0
    }

    /// Number of whole hops in the clip.
    #[must_use]
    pub fn hops(&self) -> u64 {
        (f64::from(self.duration_s) * f64::from(self.sample_rate) / HOP_FRAMES as f64) as u64
    }

    /// Generate the clip's feature frames by running the synthetic music signal
    /// through the real per-hop DSP. Deterministic: same spec → same frames.
    #[must_use]
    pub fn frames(&self) -> Vec<FeatureFrame> {
        let channels = self.channels as usize;
        let format = StreamFormat {
            sample_rate: self.sample_rate,
            channels: self.channels,
        };
        let signal = Signal::Music { bpm: self.bpm };
        let hop_ns = (HOP_FRAMES as f64 / f64::from(self.sample_rate) * 1.0e9) as u64;

        let (mut sink, mut consumer) = sample_ring(Instant::now());
        let mut processor = HopProcessor::new(HOP_FRAMES, self.channels, self.sample_rate);
        let total = self.hops();
        let mut buf = vec![0.0f32; HOP_FRAMES * channels];
        let mut frame_index = 0u64;
        let mut out = Vec::with_capacity(total as usize);

        for hop in 0..total {
            fill_signal(
                &mut buf,
                HOP_FRAMES,
                channels,
                signal,
                f64::from(self.sample_rate),
                frame_index,
            );
            sink.push(&buf);
            // A deterministic timestamp derived purely from the hop index, so the
            // projected frames carry no wall-clock state.
            let timestamp_ns = hop * hop_ns;
            let snap = processor
                .try_process(&mut consumer, format, timestamp_ns, 0)
                .expect("a full hop is buffered after each push");
            out.push(FeatureFrame::from_snapshot(&snap));
            frame_index += HOP_FRAMES as u64;
        }
        out
    }

    /// Encode the clip as an NDJSON feature-stream (the on-disk clip form, and
    /// the bytes the corpus hash covers).
    #[must_use]
    pub fn encode_ndjson(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for frame in self.frames() {
            let line = scia_core::to_json_line(&frame).expect("frame serialises");
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
        out
    }
}

/// The built-in synthetic clips shipped with the harness. Real genre clips are
/// recorded later by the maintainer with `run --output`/`--input` capture and
/// added to the manifest as committed fixtures.
pub static SYNTH_SPECS: &[SynthSpec] = &[SynthSpec {
    id: "synth-music",
    genre: "synthetic",
    bpm: 112.0,
    duration_s: 30.0,
    sample_rate: 48_000,
    channels: 2,
}];

/// Look up a synthetic clip spec by id.
#[must_use]
pub fn synth_spec(id: &str) -> Option<&'static SynthSpec> {
    SYNTH_SPECS.iter().find(|s| s.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synth_music_is_deterministic() {
        let spec = synth_spec("synth-music").unwrap();
        // A short slice is enough to prove determinism without a 30 s build.
        let short = SynthSpec {
            duration_s: 2.0,
            ..*spec
        };
        let a = short.encode_ndjson();
        let b = short.encode_ndjson();
        assert_eq!(a, b, "two regenerations differ");
        assert!(!a.is_empty());
    }

    #[test]
    fn frames_count_matches_hops() {
        let spec = SynthSpec {
            duration_s: 1.0,
            ..*synth_spec("synth-music").unwrap()
        };
        assert_eq!(spec.frames().len() as u64, spec.hops());
    }
}
