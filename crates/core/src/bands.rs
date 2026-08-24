//! Crossover band split: bass / mid / treble energies, ratio-normalized.
//!
//! The [`BandSplitter`] reads the same per-hop magnitude spectra the
//! [`SpectrumAnalyzer`](crate::spectrum::SpectrumAnalyzer) already computes — it
//! never runs an FFT of its own. It sums squared magnitudes over three
//! frequency bands (a low band from the long bass FFT for resolution, mid and
//! treble from the short main FFT) and then normalizes each band against its own
//! slow long-term average, the way projectM's audio-reactive presets scale the
//! current level against a running mean (its *current / long-average* technique,
//! **not** the equal-linear frequency split that is the documented anti-pattern).
//!
//! The output is a ratio per band: `1.0` means "at its recent average level",
//! `> 1.0` a swell above the average, `< 1.0` a dip below it. A steady tone
//! therefore relaxes toward `1.0` regardless of its absolute loudness, while a
//! transient swell reads high until the average catches up. Ratios are clamped
//! to `[0, max_ratio]`.

/// Below this RMS (−60 dBFS) a hop counts as silent and the long-term averages
/// are frozen, so a silent passage cannot drag the reference level to zero.
const SILENCE_RMS: f32 = 0.001;

/// Floor on the long-term average in the ratio denominator, so a band that has
/// never seen energy yields a finite (zero) ratio instead of a division blow-up.
const AVG_FLOOR: f32 = 1e-8;

/// Tuning for the crossover band split.
#[derive(Clone, Copy, Debug)]
pub struct BandConfig {
    /// Upper edge of the bass band, Hz. Bass is `[low edge, bass_hz)`.
    pub bass_hz: f32,
    /// Upper edge of the mid band, Hz. Mid is `[bass_hz, mid_hz)`, treble is
    /// `[mid_hz, nyquist]`.
    pub mid_hz: f32,
    /// Time constant of the per-band long-term average, seconds.
    pub avg_tau_s: f32,
    /// Upper clamp on the normalized band ratio.
    pub max_ratio: f32,
}

impl Default for BandConfig {
    fn default() -> Self {
        Self {
            bass_hz: 120.0,
            mid_hz: 2_000.0,
            avg_tau_s: 3.0,
            max_ratio: 4.0,
        }
    }
}

/// A half-open bin range `[lo, hi)` into a magnitude spectrum.
#[derive(Clone, Copy, Debug)]
struct BinRange {
    lo: usize,
    hi: usize,
}

impl BinRange {
    /// Sum of squared magnitudes over the range, clamped to the slice length.
    fn energy(self, mag: &[f32]) -> f32 {
        let hi = self.hi.min(mag.len());
        let lo = self.lo.min(hi);
        let mut sum = 0.0f32;
        for &m in &mag[lo..hi] {
            sum += m * m;
        }
        sum
    }
}

/// Splits each hop's spectra into three ratio-normalized band energies. Every
/// buffer is fixed-size; [`BandSplitter::process_hop`] never allocates.
pub struct BandSplitter {
    avg_tau_s: f32,
    max_ratio: f32,
    /// Bass band, read from the long (bass) FFT spectrum.
    bass: BinRange,
    /// Mid band, read from the short (main) FFT spectrum.
    mid: BinRange,
    /// Treble band, read from the short (main) FFT spectrum.
    treble: BinRange,
    levels: [f32; 3],
    averages: [f32; 3],
}

impl BandSplitter {
    /// Build a splitter for `sample_rate`, given the two FFT lengths whose
    /// spectra it will be fed (`fft_main` for mid/treble, `fft_bass` for bass).
    /// Bin ranges are resolved once here.
    #[must_use]
    pub fn new(config: BandConfig, sample_rate: u32, fft_main: usize, fft_bass: usize) -> Self {
        let sr = sample_rate.max(1) as f32;
        let main_top = (fft_main / 2).max(1); // highest (nyquist) bin of the main FFT
        let bass_top = (fft_bass / 2).max(1);
        let bin =
            |freq: f32, fft: usize| -> usize { (freq * fft as f32 / sr).round().max(0.0) as usize };

        // Bass: [bin 1, bin(bass_hz)) on the bass spectrum, DC (bin 0) excluded.
        let bass_hi = bin(config.bass_hz, fft_bass).clamp(2, bass_top + 1);
        let bass = BinRange { lo: 1, hi: bass_hi };

        // Mid: [bin(bass_hz), bin(mid_hz)) on the main spectrum.
        let mid_lo = bin(config.bass_hz, fft_main).max(1);
        let mid_hi = bin(config.mid_hz, fft_main).clamp(mid_lo + 1, main_top + 1);
        let mid = BinRange {
            lo: mid_lo,
            hi: mid_hi,
        };

        // Treble: [bin(mid_hz), nyquist] on the main spectrum (inclusive of the
        // nyquist bin, so the half-open upper edge is main_top + 1).
        let treble_lo = bin(config.mid_hz, fft_main).clamp(1, main_top);
        let treble = BinRange {
            lo: treble_lo,
            hi: main_top + 1,
        };

        Self {
            avg_tau_s: config.avg_tau_s.max(1e-6),
            max_ratio: config.max_ratio,
            bass,
            mid,
            treble,
            levels: [0.0; 3],
            averages: [0.0; 3],
        }
    }

    /// Consume one hop: compute the three band energies from the spectra, update
    /// the per-band long-term averages (unless the hop is silent), and write the
    /// ratio-normalized band values into `out`. Allocation-free.
    ///
    /// `mag_main` supplies the mid and treble bands, `mag_bass` the bass band;
    /// `dt_seconds` is the hop's wall-clock duration, used to keep the averaging
    /// time constant independent of the hop rate.
    pub fn process_hop(
        &mut self,
        mag_main: &[f32],
        mag_bass: &[f32],
        dt_seconds: f32,
        out: &mut [f32; 3],
    ) {
        self.levels = [
            self.bass.energy(mag_bass),
            self.mid.energy(mag_main),
            self.treble.energy(mag_main),
        ];

        // Silence gate: a Parseval-style RMS proxy from the band energies. The
        // spectra are window-sum-normalized so a full-scale sine reads ~1.0 at
        // its bin; the mean-square of the signal is then ~0.5 * Σ|X|², so this
        // tracks true RMS closely enough to gate at −60 dBFS. (The specified
        // gate is "RMS ≥ −60 dBFS"; process_hop is handed spectra, not samples,
        // so the RMS is reconstructed here rather than passed in.)
        let total = self.levels[0] + self.levels[1] + self.levels[2];
        let rms_proxy = (0.5 * total).sqrt();
        let not_silent = rms_proxy >= SILENCE_RMS;

        let alpha = 1.0 - (-dt_seconds / self.avg_tau_s).exp();
        for (i, o) in out.iter_mut().enumerate() {
            if not_silent {
                self.averages[i] += alpha * (self.levels[i] - self.averages[i]);
            }
            let ratio = self.levels[i] / self.averages[i].max(AVG_FLOOR);
            *o = ratio.clamp(0.0, self.max_ratio);
        }
    }

    /// Cheap idle update for a silent hop: drop the instantaneous levels to zero
    /// (a silent spectrum carries no band energy), leave the long-term averages
    /// frozen (exactly as the silence gate in
    /// [`process_hop`](Self::process_hop) would), and write the resulting
    /// zero ratios into `out`. Skips the energy sums entirely. Allocation-free.
    pub fn relax(&mut self, out: &mut [f32; 3]) {
        self.levels = [0.0; 3];
        // Averages stay put; ratio = 0 / avg = 0, matching the full path on a
        // silent hop.
        *out = [0.0; 3];
    }

    /// Instantaneous linear band energies (bass, mid, treble) from the last hop.
    #[must_use]
    pub fn levels(&self) -> [f32; 3] {
        self.levels
    }

    /// Current per-band long-term averages (bass, mid, treble).
    #[must_use]
    pub fn averages(&self) -> [f32; 3] {
        self.averages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;
    const FFT_MAIN: usize = 1024;
    const FFT_BASS: usize = 4096;

    /// Bin index of `freq` in an `fft`-point spectrum at `SR`.
    fn bin(freq: f32, fft: usize) -> usize {
        (freq * fft as f32 / SR as f32).round() as usize
    }

    /// A main/bass magnitude pair with `amp` placed on the bins nearest `freq`.
    fn tone(freq: f32, amp: f32) -> (Vec<f32>, Vec<f32>) {
        let mut main = vec![0.0f32; FFT_MAIN / 2 + 1];
        let mut bass = vec![0.0f32; FFT_BASS / 2 + 1];
        main[bin(freq, FFT_MAIN)] = amp;
        bass[bin(freq, FFT_BASS)] = amp;
        (main, bass)
    }

    fn splitter() -> BandSplitter {
        BandSplitter::new(BandConfig::default(), SR, FFT_MAIN, FFT_BASS)
    }

    #[test]
    fn tones_land_in_the_expected_band() {
        let dt = 256.0 / SR as f32;
        for (freq, want) in [(60.0f32, 0usize), (1_000.0, 1), (5_000.0, 2)] {
            let mut s = splitter();
            let (main, bass) = tone(freq, 0.5);
            let mut out = [0.0; 3];
            // Half a second of the tone.
            for _ in 0..((0.5 / dt) as usize) {
                s.process_hop(&main, &bass, dt, &mut out);
            }
            let levels = s.levels();
            let (dom, _) = levels
                .iter()
                .enumerate()
                .fold(
                    (0, f32::MIN),
                    |(bi, bv), (i, &v)| {
                        if v > bv { (i, v) } else { (bi, bv) }
                    },
                );
            assert_eq!(
                dom, want,
                "{freq} Hz put its energy in band {dom}, want {want}"
            );
        }
    }

    #[test]
    fn ratio_relaxes_to_one() {
        let dt = 256.0 / SR as f32;
        let mut s = splitter();
        let (main, bass) = tone(1_000.0, 0.5);
        let mut out = [0.0; 3];
        for _ in 0..((10.0 / dt) as usize) {
            s.process_hop(&main, &bass, dt, &mut out);
        }
        assert!(
            (out[1] - 1.0).abs() <= 0.1,
            "steady mid ratio {} not within 1.0 ± 0.1",
            out[1]
        );
    }

    #[test]
    fn silence_freezes_the_average() {
        let dt = 256.0 / SR as f32;
        let mut s = splitter();
        let (main, bass) = tone(1_000.0, 0.5);
        let silence_main = vec![0.0f32; FFT_MAIN / 2 + 1];
        let silence_bass = vec![0.0f32; FFT_BASS / 2 + 1];
        let mut out = [0.0; 3];

        for _ in 0..((5.0 / dt) as usize) {
            s.process_hop(&main, &bass, dt, &mut out);
        }
        let avg_before = s.averages()[1];
        for _ in 0..((5.0 / dt) as usize) {
            s.process_hop(&silence_main, &silence_bass, dt, &mut out);
        }
        let avg_after = s.averages()[1];
        assert!(
            (avg_after - avg_before).abs() <= avg_before * 1e-3,
            "silence moved the mid average {avg_before} -> {avg_after}"
        );
    }
}
