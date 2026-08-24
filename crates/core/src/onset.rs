//! Onset detection by half-wave-rectified spectral flux (Dixon, 2006).
//!
//! The [`OnsetDetector`] reads the per-hop main-FFT magnitude spectrum the
//! [`SpectrumAnalyzer`](crate::spectrum::SpectrumAnalyzer) already computes — it
//! never runs an FFT of its own. Each hop it measures the **spectral flux**: the
//! summed positive change in (log-compressed) magnitude across the low half of
//! the spectrum. A sudden broadband increase — the transient at a note or drum
//! hit — produces a flux spike; steady tones produce almost none.
//!
//! Flux is normalized against a slow peak tracker so the detector adapts to
//! level, then thresholded with a rising-edge test and a minimum inter-onset
//! interval so a single transient fires exactly one onset.

/// Log-compression scale in `log(1 + K·|X|)`. A large `K` pulls quiet bins up so
/// the flux is not dominated by a handful of loud bins.
const LOG_K: f32 = 100.0;

/// Absolute flux floor that must be exceeded at least once before any onset can
/// fire, so the detector stays quiet until it has actually seen signal (the
/// peak tracker is meaningless on the first few silent hops).
const FLUX_FLOOR: f32 = 1e-4;

/// Denominator floor for the flux normalization, so a tiny peak cannot inflate a
/// tiny flux into a spurious `1.0`.
const PEAK_FLOOR: f32 = 1e-6;

/// Saturation value of the onset-age clock, milliseconds (one minute). A value
/// at the cap means "no recent onset".
const AGE_SATURATION_MS: f32 = 60_000.0;

/// Tuning for the onset detector.
#[derive(Clone, Copy, Debug)]
pub struct OnsetConfig {
    /// Normalized-flux level an onset must exceed, in `0.0..=1.0`.
    pub threshold: f32,
    /// Minimum time between successive onsets, milliseconds.
    pub min_ioi_ms: f32,
    /// Time constant of the slow peak tracker used to normalize the flux,
    /// seconds.
    pub norm_tau_s: f32,
    /// Highest frequency included in the flux sum, Hz. Bins above this are
    /// ignored (high-frequency hiss carries little onset information).
    pub max_hz: f32,
}

impl Default for OnsetConfig {
    fn default() -> Self {
        Self {
            threshold: 0.3,
            min_ioi_ms: 20.0,
            norm_tau_s: 2.0,
            max_hz: 10_000.0,
        }
    }
}

/// Detects onsets from a stream of per-hop main-FFT magnitude spectra. The
/// previous-magnitude buffer is fixed-size; [`OnsetDetector::process_hop`] never
/// allocates.
pub struct OnsetDetector {
    threshold: f32,
    min_ioi_ms: f32,
    norm_tau_s: f32,
    /// Highest bin (inclusive) included in the flux sum.
    max_bin: usize,
    /// Log-compressed magnitudes of the previous hop, indexed by bin.
    prev_log: Vec<f32>,
    has_prev: bool,
    armed: bool,
    peak: f32,
    prev_flux_norm: f32,
    age_ms: f32,
}

impl OnsetDetector {
    /// Build a detector for `sample_rate` reading an `fft_main`-point spectrum.
    #[must_use]
    pub fn new(config: OnsetConfig, sample_rate: u32, fft_main: usize) -> Self {
        let sr = sample_rate.max(1) as f32;
        let top = (fft_main / 2).max(1);
        let max_bin =
            ((config.max_hz * fft_main as f32 / sr).round() as i64).clamp(1, top as i64) as usize;
        Self {
            threshold: config.threshold,
            min_ioi_ms: config.min_ioi_ms,
            norm_tau_s: config.norm_tau_s.max(1e-6),
            max_bin,
            prev_log: vec![0.0; max_bin + 1],
            has_prev: false,
            armed: false,
            peak: 0.0,
            prev_flux_norm: 0.0,
            age_ms: 0.0,
        }
    }

    /// Consume one hop's main-FFT magnitudes. Returns the normalized flux
    /// (`0.0..=1.0`) and whether an onset fires on this hop, and advances the
    /// onset-age clock by `dt_seconds`. Allocation-free.
    pub fn process_hop(&mut self, mag_main: &[f32], dt_seconds: f32) -> (f32, bool) {
        let top = self.max_bin.min(mag_main.len().saturating_sub(1));
        let mut flux = 0.0f32;
        // Bins 1..=top: skip DC, cap at the max-frequency bin.
        let pairs = mag_main
            .iter()
            .zip(self.prev_log.iter_mut())
            .take(top + 1)
            .skip(1);
        for (&m, prev) in pairs {
            let cur = (1.0 + LOG_K * m).ln();
            let diff = cur - *prev;
            if diff > 0.0 {
                flux += diff;
            }
            *prev = cur;
        }
        // The first hop has no predecessor: record it but emit no flux.
        if !self.has_prev {
            self.has_prev = true;
            flux = 0.0;
        }

        if flux > FLUX_FLOOR {
            self.armed = true;
        }

        // Slow peak tracker: follows the loudest recent flux, decaying with the
        // configured time constant.
        let decay = (-dt_seconds / self.norm_tau_s).exp();
        self.peak = flux.max(self.peak * decay);
        let flux_norm = (flux / self.peak.max(PEAK_FLOOR)).clamp(0.0, 1.0);

        self.age_ms = (self.age_ms + dt_seconds * 1000.0).min(AGE_SATURATION_MS);
        let onset = self.armed
            && flux_norm > self.threshold
            && flux_norm > self.prev_flux_norm
            && self.age_ms >= self.min_ioi_ms;
        if onset {
            self.age_ms = 0.0;
        }
        self.prev_flux_norm = flux_norm;

        (flux_norm, onset)
    }

    /// Milliseconds since the last onset, saturating at 60 000. Counts up from
    /// engine start; a value at (or approaching) the cap means no recent onset.
    #[must_use]
    pub fn onset_age_ms(&self) -> f32 {
        self.age_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;
    const FFT_MAIN: usize = 1024;
    const DT: f32 = 256.0 / SR as f32;

    fn detector() -> OnsetDetector {
        OnsetDetector::new(OnsetConfig::default(), SR, FFT_MAIN)
    }

    fn flat(amp: f32) -> Vec<f32> {
        vec![amp; FFT_MAIN / 2 + 1]
    }

    #[test]
    fn steady_magnitudes_produce_no_flux() {
        let mut d = detector();
        let mag = flat(0.2);
        // Prime, then run: constant magnitudes give zero positive flux.
        let (_, first) = d.process_hop(&mag, DT);
        assert!(!first);
        for _ in 0..50 {
            let (flux, onset) = d.process_hop(&mag, DT);
            assert!(flux < 1e-6, "steady flux {flux} should be ~0");
            assert!(!onset);
        }
    }

    #[test]
    fn a_step_up_fires_one_onset() {
        let mut d = detector();
        let quiet = flat(0.0);
        let loud = flat(0.3);
        // Settle at silence past min_ioi.
        for _ in 0..10 {
            d.process_hop(&quiet, DT);
        }
        // The step to loud is a rising broadband edge: one onset.
        let (_, on1) = d.process_hop(&loud, DT);
        assert!(on1, "the step up should fire an onset");
        // Holding loud is steady again: no further onsets.
        let mut extra = 0;
        for _ in 0..20 {
            if d.process_hop(&loud, DT).1 {
                extra += 1;
            }
        }
        assert_eq!(extra, 0, "a held level fired {extra} spurious onsets");
    }

    #[test]
    fn age_saturates_at_one_minute() {
        let mut d = detector();
        let quiet = flat(0.0);
        let mut prev = d.onset_age_ms();
        // 61 s of silence: the age must never decrease and must cap at 60 000.
        for _ in 0..((61.0 / DT) as usize) {
            d.process_hop(&quiet, DT);
            let now = d.onset_age_ms();
            assert!(now >= prev, "age went backwards {prev} -> {now}");
            assert!(now <= AGE_SATURATION_MS + 1e-3);
            prev = now;
        }
        assert!(
            (d.onset_age_ms() - AGE_SATURATION_MS).abs() < 1e-3,
            "age did not saturate: {}",
            d.onset_age_ms()
        );
    }
}
