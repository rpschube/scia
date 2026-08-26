//! Shared support for the golden-file DSP tests.
//!
//! This module is `#[path]`-included by both `examples/gen_fixtures.rs` (which
//! writes the committed WAV fixtures) and `tests/golden.rs` (which recomputes
//! features from those fixtures and checks them against the committed golden
//! JSON). Keeping it out of `src/` is deliberate: `hound`/`serde` are
//! dev-dependencies only and must never reach the shipped library.
//!
//! # Determinism
//!
//! Every fixture is generated from integer sample indices with plain IEEE-754
//! `f64` arithmetic and rounded exactly once to 16-bit PCM. Basic `f64` ops
//! (`*`, `/`, `+`) are bit-reproducible across platforms; the only transcendental
//! calls are `sin`/`powf`, whose worst-case cross-platform disagreement is a
//! handful of ULPs (~1e-16 relative) — far smaller than the ~1.5e-5 needed to
//! flip a rounding decision at 16-bit depth. The WAV bytes are therefore
//! byte-identical on every OS, which `fixtures_are_deterministic` pins.

#![allow(dead_code)] // Included by targets that use different subsets of this module.

use std::path::{Path, PathBuf};
use std::time::Instant;

use scia_core::{HopProcessor, StreamFormat, sample_ring};
use serde::{Deserialize, Serialize};

/// Fixture sample rate (Hz). Mono, 16-bit PCM throughout.
pub const SAMPLE_RATE: u32 = 48_000;
/// Every fixture is exactly this long.
pub const DURATION_S: f64 = 2.0;
/// Total samples per fixture (`SAMPLE_RATE * DURATION_S`, an exact integer).
pub const FIXTURE_SAMPLES: usize = 96_000;
/// DSP hop size the processor is driven at.
pub const HOP: usize = 256;
/// Full-scale value for 16-bit PCM. Amplitudes are scaled against this.
const FULL_SCALE: f64 = 32_767.0;
/// Mono 48 kHz stream format handed to the processor.
const FORMAT: StreamFormat = StreamFormat {
    sample_rate: SAMPLE_RATE,
    channels: 1,
};

/// The eight fixtures, in a stable order. The `&str` is both the WAV basename
/// (`<name>.wav`) and the golden basename (`<name>.json`).
pub const FIXTURES: [&str; 8] = [
    "sine_1k_-6db",
    "sine_60hz_-6db",
    "sine_5k_-12db",
    "clicks_120bpm",
    "noise_-20db",
    "silence",
    "sweep_50_10k",
    "burst",
];

/// Convert a dBFS level to a linear amplitude multiplier.
fn dbfs_to_lin(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// Round a real value to a signed 16-bit PCM sample (round-half-away-from-zero,
/// saturating). The single rounding step per sample is what makes the fixtures
/// cross-platform deterministic.
fn to_i16(x: f64) -> i16 {
    let r = x.round();
    if r >= FULL_SCALE {
        i16::MAX
    } else if r <= -32_768.0 {
        i16::MIN
    } else {
        r as i16
    }
}

/// Generate the raw 16-bit PCM samples for a fixture by name.
///
/// # Panics
/// Panics on an unknown fixture name.
pub fn generate_samples(name: &str) -> Vec<i16> {
    let sr = f64::from(SAMPLE_RATE);
    let two_pi = std::f64::consts::PI * 2.0;
    match name {
        "silence" => vec![0i16; FIXTURE_SAMPLES],
        "sine_1k_-6db" => sine(1_000.0, dbfs_to_lin(-6.0)),
        "sine_60hz_-6db" => sine(60.0, dbfs_to_lin(-6.0)),
        "sine_5k_-12db" => sine(5_000.0, dbfs_to_lin(-12.0)),
        "clicks_120bpm" => {
            // One-sample impulses of linear amplitude 0.8 on a 120 BPM grid.
            let period = (60.0 / 120.0 * sr).round() as usize; // 24_000 samples
            let amp = 0.8 * FULL_SCALE;
            (0..FIXTURE_SAMPLES)
                .map(|n| {
                    if period > 0 && n % period == 0 {
                        to_i16(amp)
                    } else {
                        0
                    }
                })
                .collect()
        }
        "noise_-20db" => {
            // LCG uniform noise scaled to -20 dBFS RMS. Uniform on [-1, 1) has
            // RMS 1/sqrt(3); scale so the result lands at 0.1 linear RMS.
            let target_rms = dbfs_to_lin(-20.0); // 0.1
            let scale = target_rms / (1.0f64 / 3.0).sqrt();
            let mut state: u64 = 0x2545_F491_4F6C_DD1D;
            let mut out = Vec::with_capacity(FIXTURE_SAMPLES);
            for _ in 0..FIXTURE_SAMPLES {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                // Top 53 bits -> uniform [0, 1); map to [-1, 1).
                let u = (state >> 11) as f64 / 9_007_199_254_740_992.0;
                let pm = 2.0 * u - 1.0;
                out.push(to_i16(pm * scale * FULL_SCALE));
            }
            out
        }
        "sweep_50_10k" => {
            // Exponential sine sweep 50 Hz -> 10 kHz over the whole fixture at
            // -12 dBFS. Phase is the closed-form integral of the exponential
            // instantaneous frequency, evaluated per absolute sample index.
            let f0 = 50.0f64;
            let f1 = 10_000.0f64;
            let t_total = DURATION_S;
            let ln_k = (f1 / f0).ln();
            let amp = dbfs_to_lin(-12.0) * FULL_SCALE;
            (0..FIXTURE_SAMPLES)
                .map(|n| {
                    let t = n as f64 / sr;
                    let phase = two_pi * f0 * t_total / ln_k * ((f1 / f0).powf(t / t_total) - 1.0);
                    to_i16(amp * phase.sin())
                })
                .collect()
        }
        "burst" => {
            // 0.5 s silence, 1.0 s of a -6 dBFS 1 kHz tone, 0.5 s silence. The
            // tone starts at phase 0 so its leading edge is a clean onset.
            let start = FIXTURE_SAMPLES / 4; // 24_000
            let end = start + FIXTURE_SAMPLES / 2; // 72_000
            let amp = dbfs_to_lin(-6.0) * FULL_SCALE;
            (0..FIXTURE_SAMPLES)
                .map(|n| {
                    if n >= start && n < end {
                        let phase = two_pi * 1_000.0 * (n - start) as f64 / sr;
                        to_i16(amp * phase.sin())
                    } else {
                        0
                    }
                })
                .collect()
        }
        other => panic!("unknown fixture: {other}"),
    }
}

/// A steady full-length sine at `freq` Hz, `amp_lin` peak linear amplitude.
fn sine(freq: f64, amp_lin: f64) -> Vec<i16> {
    let sr = f64::from(SAMPLE_RATE);
    let two_pi = std::f64::consts::PI * 2.0;
    let amp = amp_lin * FULL_SCALE;
    (0..FIXTURE_SAMPLES)
        .map(|n| to_i16(amp * (two_pi * freq * n as f64 / sr).sin()))
        .collect()
}

/// Write one fixture's WAV into `dir`, returning the file path.
pub fn write_fixture(name: &str, dir: &Path) -> PathBuf {
    let samples = generate_samples(name);
    let path = dir.join(format!("{name}.wav"));
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("create WAV");
    for s in samples {
        writer.write_sample(s).expect("write sample");
    }
    writer.finalize().expect("finalize WAV");
    path
}

/// Write every fixture into `dir` (created if absent).
pub fn generate_all(dir: &Path) {
    std::fs::create_dir_all(dir).expect("create fixtures dir");
    for name in FIXTURES {
        write_fixture(name, dir);
    }
}

/// The committed fixtures directory (`crates/core/tests/fixtures`).
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// The committed golden directory (`crates/core/tests/golden`).
pub fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
}

/// One hop's worth of the fields the golden files snapshot.
#[derive(Clone, Debug)]
pub struct HopData {
    pub generation: u64,
    pub rms: f32,
    pub peak: f32,
    pub loudness: f32,
    pub spectrum: Vec<f32>,
    pub bands: [f32; 3],
    pub flux: f32,
    pub onset: bool,
    pub onset_age_ms: f32,
}

/// A WAV fixture read into f32 mono samples, ready to drive a [`HopProcessor`].
pub struct WavFixture {
    pub samples: Vec<f32>,
}

impl WavFixture {
    /// Load a mono 16-bit WAV and convert to f32 in `[-1, 1)`.
    pub fn load(path: &Path) -> Self {
        let mut reader =
            hound::WavReader::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
        let spec = reader.spec();
        assert_eq!(spec.channels, 1, "fixtures are mono");
        assert_eq!(spec.sample_rate, SAMPLE_RATE, "fixtures are 48 kHz");
        assert_eq!(spec.bits_per_sample, 16, "fixtures are 16-bit");
        let samples = reader
            .samples::<i16>()
            .map(|s| s.expect("read sample") as f32 / 32_768.0)
            .collect();
        Self { samples }
    }

    /// Drive the fixture through a default [`HopProcessor`] on the mono path,
    /// 256 frames per hop, returning one [`HopData`] per produced hop.
    pub fn run_features(&self) -> Vec<HopData> {
        let (mut sink, mut consumer) = sample_ring(Instant::now());
        let mut processor = HopProcessor::new(HOP, 1, SAMPLE_RATE);
        let mut out = Vec::new();
        for chunk in self.samples.chunks_exact(HOP) {
            sink.push(chunk);
            if let Some(snap) = processor.try_process(&mut consumer, FORMAT, 0, 0) {
                let len = snap.spectrum_len as usize;
                out.push(HopData {
                    generation: snap.generation,
                    rms: snap.rms,
                    peak: snap.peak,
                    loudness: snap.loudness,
                    spectrum: snap.spectrum[..len].to_vec(),
                    bands: snap.bands,
                    flux: snap.flux,
                    onset: snap.onset,
                    onset_age_ms: snap.onset_age_ms,
                });
            }
        }
        out
    }
}

/// Sample points, milliseconds: every 250 ms from 0.25 s to 2.0 s.
pub const SAMPLE_MS: [u32; 8] = [250, 500, 750, 1000, 1250, 1500, 1750, 2000];

/// One golden sample point. All feature values are stored at six significant
/// figures so the JSON is small, diffable and stable.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SamplePoint {
    /// Sample time in milliseconds (250, 500, …, 2000).
    pub t_ms: u32,
    /// Hop generation nearest that time.
    #[serde(rename = "gen")]
    pub hop_gen: u64,
    pub rms: f64,
    pub peak: f64,
    pub loudness: f64,
    pub spectrum: Vec<f64>,
    pub bands: [f64; 3],
    pub flux: f64,
    pub onset_age_ms: f64,
}

/// A whole fixture's golden record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Golden {
    pub fixture: String,
    pub hop_frames: usize,
    pub sample_rate: u32,
    pub spectrum_len: usize,
    pub onset_count: usize,
    pub onset_hops: Vec<u64>,
    pub samples: Vec<SamplePoint>,
}

/// Round to six significant figures, returning a clean `f64` for JSON. Values
/// that round to zero (and non-finite inputs) become `0.0`.
fn round6(x: f32) -> f64 {
    let x = x as f64;
    if !x.is_finite() || x == 0.0 {
        return 0.0;
    }
    let digits = x.abs().log10().floor();
    let p = 10f64.powf(5.0 - digits);
    let r = (x * p).round() / p;
    if r == 0.0 { 0.0 } else { r }
}

/// Recompute a fixture's golden record from its committed WAV.
pub fn compute_golden(name: &str) -> Golden {
    let hops = WavFixture::load(&fixtures_dir().join(format!("{name}.wav"))).run_features();
    let dt = HOP as f64 / f64::from(SAMPLE_RATE);
    let n = hops.len();
    let spectrum_len = hops.first().map_or(0, |h| h.spectrum.len());

    let onset_hops: Vec<u64> = hops
        .iter()
        .filter(|h| h.onset)
        .map(|h| h.generation)
        .collect();

    let samples = SAMPLE_MS
        .iter()
        .map(|&t_ms| {
            let target = (f64::from(t_ms) / 1000.0 / dt).round() as usize;
            let idx = target.clamp(1, n) - 1;
            let h = &hops[idx];
            SamplePoint {
                t_ms,
                hop_gen: h.generation,
                rms: round6(h.rms),
                peak: round6(h.peak),
                loudness: round6(h.loudness),
                spectrum: h.spectrum.iter().map(|&v| round6(v)).collect(),
                bands: [round6(h.bands[0]), round6(h.bands[1]), round6(h.bands[2])],
                flux: round6(h.flux),
                onset_age_ms: round6(h.onset_age_ms),
            }
        })
        .collect();

    Golden {
        fixture: name.to_string(),
        hop_frames: HOP,
        sample_rate: SAMPLE_RATE,
        spectrum_len,
        onset_count: onset_hops.len(),
        onset_hops,
        samples,
    }
}

/// A single tolerance failure between an expected and an actual golden.
#[derive(Clone, Debug)]
pub struct Mismatch {
    pub field: String,
    pub t_ms: u32,
    pub expected: f64,
    pub actual: f64,
    pub delta: f64,
    pub tol: f64,
}

impl Mismatch {
    /// How far past tolerance the failure is (>= 1.0 means it failed).
    fn severity(&self) -> f64 {
        if self.tol > 0.0 {
            self.delta / self.tol
        } else {
            f64::INFINITY
        }
    }
}

// Tolerance bands (see tests/golden/README.md for the rationale).
/// Relative tolerance on rms/peak.
const REL_LEVEL: f64 = 0.01;
/// Absolute floor under the relative rms/peak tolerance.
const ABS_LEVEL_FLOOR: f64 = 1e-4;
/// Absolute tolerance on each spectrum bar (`0..=1`).
const ABS_SPECTRUM: f64 = 0.02;
/// Absolute tolerance on each band ratio.
const ABS_BANDS: f64 = 0.05;
/// Absolute tolerance on normalized flux.
const ABS_FLUX: f64 = 0.05;
/// Absolute tolerance on normalized loudness (`0..=1`, same band as flux).
const ABS_LOUDNESS: f64 = 0.02;
/// Absolute tolerance on the onset-age clock (one hop at 48 kHz ≈ 5.33 ms).
const ABS_ONSET_AGE_MS: f64 = 6.0;

/// Level tolerance: relative, with an absolute floor.
fn level_tol(expected: f64) -> f64 {
    (REL_LEVEL * expected.abs()).max(ABS_LEVEL_FLOOR)
}

/// Compare two golden records under the tolerance bands, returning every
/// failure. Onset hop indices must match exactly; a mismatch there is reported
/// as a single always-failing entry (`tol = 0`).
pub fn compare(expected: &Golden, actual: &Golden) -> Vec<Mismatch> {
    let mut out = Vec::new();

    if expected.onset_hops != actual.onset_hops {
        out.push(Mismatch {
            field: format!(
                "onset_hops (expected {:?}, actual {:?})",
                expected.onset_hops, actual.onset_hops
            ),
            t_ms: 0,
            expected: expected.onset_hops.len() as f64,
            actual: actual.onset_hops.len() as f64,
            delta: 1.0,
            tol: 0.0,
        });
    }

    for (e, a) in expected.samples.iter().zip(actual.samples.iter()) {
        let mut check = |field: &str, ev: f64, av: f64, tol: f64| {
            let delta = (av - ev).abs();
            if delta > tol {
                out.push(Mismatch {
                    field: field.to_string(),
                    t_ms: e.t_ms,
                    expected: ev,
                    actual: av,
                    delta,
                    tol,
                });
            }
        };

        check("rms", e.rms, a.rms, level_tol(e.rms));
        check("peak", e.peak, a.peak, level_tol(e.peak));
        check("loudness", e.loudness, a.loudness, ABS_LOUDNESS);
        check("flux", e.flux, a.flux, ABS_FLUX);
        check(
            "onset_age_ms",
            e.onset_age_ms,
            a.onset_age_ms,
            ABS_ONSET_AGE_MS,
        );
        for i in 0..3 {
            check(&format!("bands[{i}]"), e.bands[i], a.bands[i], ABS_BANDS);
        }
        let bars = e.spectrum.len().min(a.spectrum.len());
        for i in 0..bars {
            check(
                &format!("spectrum[{i}]"),
                e.spectrum[i],
                a.spectrum[i],
                ABS_SPECTRUM,
            );
        }
    }

    out
}

/// Render the worst offenders as an aligned table for a failing test.
pub fn worst_offenders_table(mismatches: &[Mismatch]) -> String {
    let mut sorted: Vec<&Mismatch> = mismatches.iter().collect();
    sorted.sort_by(|a, b| {
        b.severity()
            .partial_cmp(&a.severity())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut s = format!(
        "{} tolerance failure(s); worst offenders:\n",
        mismatches.len()
    );
    s.push_str(&format!(
        "  {:<22} {:>7} {:>14} {:>14} {:>14} {:>12}\n",
        "field", "t_ms", "expected", "actual", "delta", "tol"
    ));
    for m in sorted.iter().take(15) {
        s.push_str(&format!(
            "  {:<22} {:>7} {:>14.6} {:>14.6} {:>14.6} {:>12.6}\n",
            m.field, m.t_ms, m.expected, m.actual, m.delta, m.tol
        ));
    }
    s
}
