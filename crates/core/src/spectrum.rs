//! The display spectrum: the second analysis path.
//!
//! The engine runs two analyses over the same audio. The hop-grid features in
//! [`crate::features`] describe *what happened* in each 256-frame hop; the
//! **display spectrum** here is the bar graph a renderer draws — a small number
//! of log-spaced magnitude bars, normalized, auto-ranged and smoothed for the
//! eye rather than for measurement.
//!
//! The design doc calls for a "display FFT per rendered frame over the newest
//! ring samples". In this architecture the render side never touches the sample
//! ring, so the display spectrum is instead computed **on the DSP thread, once
//! per hop**. At 48 kHz a 256-frame hop is 187.5 Hz — faster than any real
//! frame rate — so every rendered frame still reads a spectrum no older than a
//! single hop, and the render side stays a pure consumer of [`FeatureSnapshot`].
//!
//! [`FeatureSnapshot`]: crate::features::FeatureSnapshot
//!
//! # Pipeline (all of it allocation-free after [`SpectrumAnalyzer::new`])
//!
//! 1. Each hop's mono samples are appended to a circular history buffer holding
//!    the newest `fft_bass` samples.
//! 2. Two windowed FFTs run over the newest samples: a short `fft_main`-point
//!    FFT (good time resolution) and a long `fft_bass`-point FFT (good low
//!    frequency resolution). Both use a Hann window; magnitudes are normalized
//!    by the window sum so a full-scale sine reads ~1.0 at its bin.
//! 3. Magnitudes are folded into `bars` log-spaced bars (cava's cutoff
//!    formula). Bars whose center sits below `bass_split_hz` read from the long
//!    FFT, the rest from the short FFT. A bar's value is the **max** of its
//!    bins, not the mean: max keeps a pure tone from being diluted by the silent
//!    bins around it, which is what a spectrum display wants.
//! 4. Each bar is converted to dB and mapped `[db_floor, 0] -> [0, 1]`.
//! 5. A cava-style automatic sensitivity (AGC) applies a single linear gain
//!    before the dB map, chasing a full-scale peak: it backs off fast when a bar
//!    clips and creeps up slowly otherwise, gated to silence so it does not pump
//!    up the noise floor.
//! 6. Every bar is smoothed with an asymmetric exponential whose coefficients
//!    derive from the elapsed time `dt`, so the result is independent of the
//!    hop or frame rate.

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

/// Largest supported bar count. Matches [`crate::features::SPECTRUM_BINS`], the
/// width of the spectrum array in a snapshot.
const MAX_BARS: usize = crate::features::SPECTRUM_BINS;
/// Smallest supported bar count.
const MIN_BARS: usize = 16;

/// AGC step applied when a bar clips (multiplicative, per hop).
const AGC_CLIP_DOWN: f32 = 1.0 - 0.02;
/// AGC step applied when no bar clips and the hop is not silent (per hop).
const AGC_GAIN_UP: f32 = 1.0 + 0.001;
/// AGC gain is bounded to this range; the floor of 1.0 means the AGC only ever
/// amplifies quiet material, never attenuates a loud one.
const AGC_MIN_GAIN: f32 = 1.0;
/// Upper AGC bound.
const AGC_MAX_GAIN: f32 = 1000.0;
/// Below this RMS (−60 dBFS) a hop counts as silent and the AGC does not creep
/// up, so a quiet passage does not slowly amplify the noise floor.
const SILENCE_RMS: f32 = 0.001;

/// Configuration for the display spectrum. All fields have defaults tuned for a
/// 64-bar music visualizer; [`SpectrumConfig::default`] is what the DSP thread
/// uses unless the engine overrides it.
#[derive(Clone, Copy, Debug)]
pub struct SpectrumConfig {
    /// Number of output bars, clamped to `16..=256`. Equals the number of valid
    /// entries the analyzer writes into a snapshot's spectrum array.
    pub bars: usize,
    /// Low edge of the displayed range, Hz.
    pub low_hz: f32,
    /// High edge of the displayed range, Hz. Clamped to the Nyquist frequency
    /// at construction.
    pub high_hz: f32,
    /// Bars whose lower cutoff is below this frequency read from the long
    /// (`fft_bass`) FFT; the rest read from the short (`fft_main`) FFT.
    pub bass_split_hz: f32,
    /// Length of the short FFT (good time resolution, coarse in frequency).
    pub fft_main: usize,
    /// Length of the long FFT (fine frequency resolution for the bass).
    pub fft_bass: usize,
    /// dB value that maps to 0.0 at the bottom of a bar.
    pub db_floor: f32,
    /// Rise time constant, milliseconds (used when a bar is climbing).
    pub attack_ms: f32,
    /// Fall time constant, milliseconds (used when a bar is dropping).
    pub release_ms: f32,
    /// When `true`, the AGC auto-ranges the display; when `false`, gain is
    /// pinned to 1.0.
    pub autosens: bool,
}

impl Default for SpectrumConfig {
    fn default() -> Self {
        Self {
            bars: 64,
            low_hz: 50.0,
            high_hz: 10_000.0,
            bass_split_hz: 200.0,
            fft_main: 1024,
            fft_bass: 4096,
            db_floor: -70.0,
            attack_ms: 30.0,
            release_ms: 200.0,
            autosens: true,
        }
    }
}

/// The FFT bins a single bar folds together, plus the frequency range it covers.
/// Exposed only for tests and diagnostics.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct BarBins {
    /// `true` if this bar reads from the long (bass) FFT.
    pub use_bass: bool,
    /// First bin index owned by the bar (inclusive).
    pub lo_bin: usize,
    /// One past the last bin index owned by the bar (exclusive); always
    /// `hi_bin > lo_bin`, so every bar owns at least one bin.
    pub hi_bin: usize,
    /// Lower cutoff frequency of the bar, Hz.
    pub f_lo: f32,
    /// Upper cutoff frequency of the bar, Hz.
    pub f_hi: f32,
}

/// Turns per-hop mono audio into a normalized, smoothed bar spectrum. Every
/// buffer, window and FFT plan is allocated once in [`SpectrumAnalyzer::new`];
/// [`SpectrumAnalyzer::process_hop`] never allocates.
pub struct SpectrumAnalyzer {
    config: SpectrumConfig,
    sample_rate: u32,
    dt_attack_tau: f32,
    dt_release_tau: f32,

    // Circular history of the newest `fft_bass` mono samples.
    history: Vec<f32>,
    hist_pos: usize,
    // Chronological (oldest-first) copy of `history` for windowing.
    linear: Vec<f32>,

    win_main: Vec<f32>,
    win_bass: Vec<f32>,
    norm_main: f32,
    norm_bass: f32,

    plan_main: Arc<dyn RealToComplex<f32>>,
    plan_bass: Arc<dyn RealToComplex<f32>>,
    in_main: Vec<f32>,
    in_bass: Vec<f32>,
    out_main: Vec<Complex<f32>>,
    out_bass: Vec<Complex<f32>>,
    scratch_main: Vec<Complex<f32>>,
    scratch_bass: Vec<Complex<f32>>,
    mag_main: Vec<f32>,
    mag_bass: Vec<f32>,

    bars_meta: Vec<BarBins>,
    raw: Vec<f32>,
    smoothed: Vec<f32>,
    gain: f32,
}

impl SpectrumAnalyzer {
    /// Build an analyzer for `sample_rate`, allocating every buffer and FFT
    /// plan up front. `config.bars` is clamped to `16..=256` and `config.high_hz`
    /// to the Nyquist frequency.
    #[must_use]
    pub fn new(mut config: SpectrumConfig, sample_rate: u32) -> Self {
        let sr = sample_rate.max(1);
        let nyquist = sr as f32 / 2.0;

        config.bars = config.bars.clamp(MIN_BARS, MAX_BARS);
        // Keep the band inside a valid, ordered, sub-Nyquist range.
        config.high_hz = config.high_hz.min(nyquist * 0.99).max(2.0);
        config.low_hz = config.low_hz.clamp(1.0, config.high_hz * 0.5);

        let mut planner = RealFftPlanner::<f32>::new();
        let plan_main = planner.plan_fft_forward(config.fft_main);
        let plan_bass = planner.plan_fft_forward(config.fft_bass);

        let win_main = hann(config.fft_main);
        let win_bass = hann(config.fft_bass);
        // Normalize magnitudes by half the window sum so a full-scale sine on a
        // bin reads ~1.0 (its energy splits between the +f and -f images).
        let norm_main = 2.0 / win_main.iter().sum::<f32>();
        let norm_bass = 2.0 / win_bass.iter().sum::<f32>();

        let in_main = plan_main.make_input_vec();
        let in_bass = plan_bass.make_input_vec();
        let out_main = plan_main.make_output_vec();
        let out_bass = plan_bass.make_output_vec();
        let scratch_main = plan_main.make_scratch_vec();
        let scratch_bass = plan_bass.make_scratch_vec();
        let mag_main = vec![0.0; out_main.len()];
        let mag_bass = vec![0.0; out_bass.len()];

        let bars_meta = build_bars(&config, sr);

        let dt_attack_tau = (config.attack_ms / 1000.0).max(1e-6);
        let dt_release_tau = (config.release_ms / 1000.0).max(1e-6);

        Self {
            history: vec![0.0; config.fft_bass],
            hist_pos: 0,
            linear: vec![0.0; config.fft_bass],
            raw: vec![0.0; config.bars],
            smoothed: vec![0.0; config.bars],
            gain: 1.0,
            win_main,
            win_bass,
            norm_main,
            norm_bass,
            plan_main,
            plan_bass,
            in_main,
            in_bass,
            out_main,
            out_bass,
            scratch_main,
            scratch_bass,
            mag_main,
            mag_bass,
            bars_meta,
            dt_attack_tau,
            dt_release_tau,
            config,
            sample_rate: sr,
        }
    }

    /// The effective configuration (after clamping).
    #[must_use]
    pub fn config(&self) -> &SpectrumConfig {
        &self.config
    }

    /// Number of output bars.
    #[must_use]
    pub fn bars(&self) -> usize {
        self.config.bars
    }

    /// The current AGC gain (1.0 when `autosens` is off).
    #[must_use]
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// The sample rate the analyzer was built for.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The per-bar cutoff / bin table. For tests and diagnostics only.
    #[doc(hidden)]
    #[must_use]
    pub fn bar_bins(&self) -> &[BarBins] {
        &self.bars_meta
    }

    /// The most recent main-FFT magnitude spectrum (window-sum normalized),
    /// indexed by bin `0..=fft_main/2`. Bin `k` sits at `k * sample_rate /
    /// fft_main` Hz. Exposed so sibling analyses (bands, onsets) can consume the
    /// spectrum without recomputing an FFT; valid after the first
    /// [`SpectrumAnalyzer::process_hop`].
    pub(crate) fn mag_main(&self) -> &[f32] {
        &self.mag_main
    }

    /// The most recent bass-FFT magnitude spectrum (window-sum normalized),
    /// indexed by bin `0..=fft_bass/2`. Bin `k` sits at `k * sample_rate /
    /// fft_bass` Hz. See [`SpectrumAnalyzer::mag_main`].
    pub(crate) fn mag_bass(&self) -> &[f32] {
        &self.mag_bass
    }

    /// Append one hop of mono samples, recompute the spectrum, and write the
    /// `bars()` bar values (each in `0.0..=1.0`) into `out[..bars()]`.
    /// `dt_seconds` is the wall-clock duration of the hop, used for the
    /// frame-rate-independent smoothing. Allocation-free.
    ///
    /// # Panics
    /// Panics if `out.len() < bars()`.
    pub fn process_hop(&mut self, mono_hop: &[f32], dt_seconds: f32, out: &mut [f32]) {
        assert!(out.len() >= self.config.bars, "out too small");

        let fft_bass = self.config.fft_bass;
        let fft_main = self.config.fft_main;

        // 1. Push the hop into the circular history buffer.
        for &sample in mono_hop {
            self.history[self.hist_pos] = sample;
            self.hist_pos += 1;
            if self.hist_pos == fft_bass {
                self.hist_pos = 0;
            }
        }
        // Chronological copy: `hist_pos` points at the oldest sample.
        let split = fft_bass - self.hist_pos;
        self.linear[..split].copy_from_slice(&self.history[self.hist_pos..]);
        self.linear[split..].copy_from_slice(&self.history[..self.hist_pos]);

        // 2. Two windowed FFTs over the newest samples.
        let off = fft_bass - fft_main;
        for j in 0..fft_main {
            self.in_main[j] = self.linear[off + j] * self.win_main[j];
        }
        self.plan_main
            .process_with_scratch(
                &mut self.in_main,
                &mut self.out_main,
                &mut self.scratch_main,
            )
            .expect("main FFT length is fixed at construction");
        for (m, c) in self.mag_main.iter_mut().zip(self.out_main.iter()) {
            *m = c.norm() * self.norm_main;
        }

        for j in 0..fft_bass {
            self.in_bass[j] = self.linear[j] * self.win_bass[j];
        }
        self.plan_bass
            .process_with_scratch(
                &mut self.in_bass,
                &mut self.out_bass,
                &mut self.scratch_bass,
            )
            .expect("bass FFT length is fixed at construction");
        for (m, c) in self.mag_bass.iter_mut().zip(self.out_bass.iter()) {
            *m = c.norm() * self.norm_bass;
        }

        // 3. Fold FFT bins into bars (max over each bar's bins).
        for (i, bar) in self.bars_meta.iter().enumerate() {
            let mags = if bar.use_bass {
                &self.mag_bass
            } else {
                &self.mag_main
            };
            let mut peak = 0.0f32;
            for &m in &mags[bar.lo_bin..bar.hi_bin] {
                if m > peak {
                    peak = m;
                }
            }
            self.raw[i] = peak;
        }

        // Hop RMS for the AGC silence gate.
        let hop_rms = if mono_hop.is_empty() {
            0.0
        } else {
            let sum_sq: f32 = mono_hop.iter().map(|&s| s * s).sum();
            (sum_sq / mono_hop.len() as f32).sqrt()
        };

        // 5. Update the AGC gain (before mapping this hop, so the freshest gain
        // is applied and the debug overlay sees the value the bars used).
        if self.config.autosens {
            let raw_max = self.raw.iter().copied().fold(0.0f32, f32::max);
            let candidate = (self.gain * AGC_GAIN_UP).min(AGC_MAX_GAIN);
            if raw_max * candidate > 1.0 {
                // Even a step up would clip; only step down if already clipping.
                if raw_max * self.gain > 1.0 {
                    self.gain *= AGC_CLIP_DOWN;
                }
            } else if hop_rms >= SILENCE_RMS {
                self.gain = candidate;
            }
            self.gain = self.gain.clamp(AGC_MIN_GAIN, AGC_MAX_GAIN);
        } else {
            self.gain = 1.0;
        }

        // 4 + 6. dB map and asymmetric exponential smoothing.
        let span = -self.config.db_floor;
        let gain = self.gain;
        let (attack, release) = (self.dt_attack_tau, self.dt_release_tau);
        for ((raw, smoothed), out) in self
            .raw
            .iter()
            .zip(self.smoothed.iter_mut())
            .zip(out.iter_mut())
        {
            let linear = raw * gain;
            let db = if linear > 0.0 {
                20.0 * linear.log10()
            } else {
                self.config.db_floor
            };
            let target = ((db - self.config.db_floor) / span).clamp(0.0, 1.0);

            let tau = if target > *smoothed { attack } else { release };
            let alpha = 1.0 - (-dt_seconds / tau).exp();
            *smoothed += alpha * (target - *smoothed);
            *out = *smoothed;
        }
    }
}

/// A periodic Hann window of length `n`.
fn hann(n: usize) -> Vec<f32> {
    use std::f32::consts::PI;
    if n == 0 {
        return Vec::new();
    }
    (0..n)
        .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / n as f32).cos())
        .collect()
}

/// Build the per-bar cutoff / bin table (cava's log cutoff formula with a
/// min-bandwidth guard so every bar owns at least one FFT bin).
fn build_bars(config: &SpectrumConfig, sample_rate: u32) -> Vec<BarBins> {
    let bars = config.bars;
    let sr = sample_rate as f32;
    let ratio = (config.high_hz / config.low_hz) as f64;

    // Highest valid (non-DC) bin index for each FFT, inclusive.
    let main_top = config.fft_main / 2;
    let bass_top = config.fft_bass / 2;

    let cutoff = |i: usize| -> f32 { config.low_hz * ratio.powf(i as f64 / bars as f64) as f32 };
    let bin_of = |freq: f32, fft: usize, top: usize| -> usize {
        let b = (freq * fft as f32 / sr).round() as i64;
        b.clamp(1, top as i64) as usize
    };

    let mut out = Vec::with_capacity(bars);
    for i in 0..bars {
        let f_lo = cutoff(i);
        let f_hi = cutoff(i + 1);
        let use_bass = f_lo < config.bass_split_hz;
        let (fft, top) = if use_bass {
            (config.fft_bass, bass_top)
        } else {
            (config.fft_main, main_top)
        };
        let lo_bin = bin_of(f_lo, fft, top);
        let mut hi_bin = bin_of(f_hi, fft, top);
        // Min-bandwidth guard: a bar whose range collapses to a single bin
        // still owns that bin (it shares it with the neighbour cutoff).
        if hi_bin <= lo_bin {
            hi_bin = (lo_bin + 1).min(top + 1);
        }
        out.push(BarBins {
            use_bass,
            lo_bin,
            hi_bin,
            f_lo,
            f_hi,
        });
    }
    out
}
