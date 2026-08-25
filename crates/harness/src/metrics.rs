//! Whole-run scene-quality metrics computed from a replay's per-hop series.
//!
//! Each metric is a documented, deterministic function of two aligned per-hop
//! series — the clip's feature envelope and the canvas stats the scene produced —
//! so the same clip + scene + params always yields the same numbers. The
//! [`Metrics`] summary is what `run` writes to `metrics.json` and what `ab` and
//! `freeze` compare.

use serde::{Deserialize, Serialize};

/// The per-hop series a run produces, in hop order. All slices are the same
/// length (one entry per emitted hop).
pub struct Series<'a> {
    /// Onset envelope per hop (the clip's normalised spectral flux).
    pub onset: &'a [f32],
    /// Hop RMS level.
    pub rms: &'a [f32],
    /// Canvas motion energy per hop.
    pub motion: &'a [f32],
    /// Canvas brightness per hop.
    pub brightness: &'a [f32],
    /// Canvas coverage fraction per hop.
    pub coverage: &'a [f32],
    /// Intensity-weighted mean drawn colour per hop (normalised RGB).
    pub color: &'a [[f32; 3]],
    /// Hop cadence in milliseconds.
    pub hop_ms: f32,
}

/// Tunables for the metrics that need a threshold or a search bound. Defaults
/// are the v0 calibration values; `run` can override them later.
#[derive(Clone, Copy, Debug)]
pub struct MetricParams {
    /// Longest onset→motion lag searched, in milliseconds.
    pub max_onset_lag_ms: f32,
    /// A hop counts as "quiet" when its RMS is at or below this fraction of the
    /// run's peak RMS.
    pub quiet_rms_frac: f32,
    /// Brightness deltas with magnitude below this are treated as flat (no sign)
    /// for the flicker sign-flip count.
    pub flicker_eps: f32,
}

impl Default for MetricParams {
    fn default() -> Self {
        Self {
            max_onset_lag_ms: 300.0,
            quiet_rms_frac: 0.2,
            flicker_eps: 1e-4,
        }
    }
}

/// The computed scene-quality metrics for one run.
///
/// Field order is the serialisation order in `metrics.json`; it is stable so two
/// identical runs produce byte-identical files.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    /// Onset→motion response latency in milliseconds (the lag maximising the
    /// onset-envelope↔motion cross-correlation).
    pub onset_response_latency_ms: f64,
    /// Onset→motion response gain: the regression slope of motion onto the
    /// onset envelope at the peak-correlation lag.
    pub onset_response_gain: f64,
    /// Pearson correlation of RMS vs canvas motion (loudness → movement).
    pub loudness_motion_r: f64,
    /// Pearson correlation of RMS vs canvas brightness (loudness → brightness).
    pub loudness_brightness_r: f64,
    /// Mean canvas motion during RMS-quiet hops (lower is calmer at rest).
    pub quiet_stillness: f64,
    /// Mean canvas coverage fraction over the run.
    pub coverage_mean: f64,
    /// 95th-percentile canvas coverage fraction over the run.
    pub coverage_p95: f64,
    /// Flicker penalty: the rate of brightness-delta sign flips (`0.0..=1.0`).
    pub flicker: f64,
    /// Palette churn: mean per-second Euclidean change of the mean drawn colour.
    pub palette_churn: f64,
}

impl Metrics {
    /// Every metric is finite (no `NaN`/`inf`).
    #[must_use]
    pub fn all_finite(&self) -> bool {
        [
            self.onset_response_latency_ms,
            self.onset_response_gain,
            self.loudness_motion_r,
            self.loudness_brightness_r,
            self.quiet_stillness,
            self.coverage_mean,
            self.coverage_p95,
            self.flicker,
            self.palette_churn,
        ]
        .iter()
        .all(|v| v.is_finite())
    }

    /// The metrics as `(name, value)` pairs in serialisation order, for tables
    /// and envelopes.
    #[must_use]
    pub fn as_pairs(&self) -> Vec<(&'static str, f64)> {
        vec![
            ("onset_response_latency_ms", self.onset_response_latency_ms),
            ("onset_response_gain", self.onset_response_gain),
            ("loudness_motion_r", self.loudness_motion_r),
            ("loudness_brightness_r", self.loudness_brightness_r),
            ("quiet_stillness", self.quiet_stillness),
            ("coverage_mean", self.coverage_mean),
            ("coverage_p95", self.coverage_p95),
            ("flicker", self.flicker),
            ("palette_churn", self.palette_churn),
        ]
    }
}

/// Compute the whole-run [`Metrics`] from a run's per-hop [`Series`].
#[must_use]
pub fn compute(series: &Series, params: &MetricParams) -> Metrics {
    let max_lag =
        ((params.max_onset_lag_ms / series.hop_ms.max(f32::MIN_POSITIVE)).round() as usize).max(1);
    let (lat_hops, gain) = onset_response(series.onset, series.motion, max_lag);
    let onset_response_latency_ms = f64::from(lat_hops) * f64::from(series.hop_ms);

    let loudness_motion_r = pearson(series.rms, series.motion);
    let loudness_brightness_r = pearson(series.rms, series.brightness);
    let quiet_stillness = quiet_stillness(series.rms, series.motion, params.quiet_rms_frac);
    let (coverage_mean, coverage_p95) = coverage_stats(series.coverage);
    let flicker = flicker(series.brightness, params.flicker_eps);
    let palette_churn = palette_churn(series.color, series.hop_ms);

    Metrics {
        onset_response_latency_ms,
        onset_response_gain: gain,
        loudness_motion_r,
        loudness_brightness_r,
        quiet_stillness,
        coverage_mean,
        coverage_p95,
        flicker,
        palette_churn,
    }
}

/// Onset→motion response: search non-negative lags `0..=max_lag` for the lag at
/// which the onset envelope, shifted forward by that many hops, best correlates
/// (Pearson) with canvas motion. Returns `(best_lag_hops, gain)` where `gain` is
/// the least-squares slope of `motion ≈ gain · onset_shifted` at that lag.
///
/// Only non-negative lags are searched: the canvas responds *after* an onset,
/// never before. With too little data or a flat series the lag is `0` and the
/// gain `0`.
#[must_use]
pub fn onset_response(onset: &[f32], motion: &[f32], max_lag: usize) -> (u32, f64) {
    let n = onset.len().min(motion.len());
    if n < 3 {
        return (0, 0.0);
    }
    let mut best_lag = 0usize;
    let mut best_r = f64::NEG_INFINITY;
    let mut best_gain = 0.0f64;
    for lag in 0..=max_lag.min(n.saturating_sub(2)) {
        // Correlate onset[i] with motion[i + lag] over the overlap.
        let a = &onset[..n - lag];
        let b = &motion[lag..n];
        let (r, gain) = pearson_and_slope(a, b);
        if r > best_r {
            best_r = r;
            best_lag = lag;
            best_gain = gain;
        }
    }
    (best_lag as u32, best_gain)
}

/// Mean canvas motion over the hops whose RMS is at or below `quiet_frac × peak
/// RMS`. `0.0` when there are no quiet hops.
#[must_use]
pub fn quiet_stillness(rms: &[f32], motion: &[f32], quiet_frac: f32) -> f64 {
    let n = rms.len().min(motion.len());
    if n == 0 {
        return 0.0;
    }
    let peak = rms.iter().copied().fold(0.0f32, f32::max);
    let thresh = peak * quiet_frac;
    let mut sum = 0.0f64;
    let mut count = 0u64;
    for i in 0..n {
        if rms[i] <= thresh {
            sum += f64::from(motion[i]);
            count += 1;
        }
    }
    if count == 0 { 0.0 } else { sum / count as f64 }
}

/// Mean and 95th-percentile of the per-hop coverage series.
#[must_use]
pub fn coverage_stats(coverage: &[f32]) -> (f64, f64) {
    if coverage.is_empty() {
        return (0.0, 0.0);
    }
    let mean = coverage.iter().map(|&v| f64::from(v)).sum::<f64>() / coverage.len() as f64;
    (mean, percentile(coverage, 0.95))
}

/// Flicker penalty: the fraction of interior hops at which the sign of the
/// brightness delta flips relative to the previous delta. Deltas with magnitude
/// below `eps` are treated as flat and never count as a flip (so a static run
/// scores `0`). A hard alternating bright/dark run scores near `1`.
#[must_use]
pub fn flicker(brightness: &[f32], eps: f32) -> f64 {
    if brightness.len() < 3 {
        return 0.0;
    }
    let sign = |d: f32| -> i32 {
        if d > eps {
            1
        } else if d < -eps {
            -1
        } else {
            0
        }
    };
    let mut flips = 0u64;
    let mut pairs = 0u64;
    let mut prev = sign(brightness[1] - brightness[0]);
    for w in brightness.windows(2).skip(1) {
        let s = sign(w[1] - w[0]);
        pairs += 1;
        if s != 0 && prev != 0 && s != prev {
            flips += 1;
        }
        if s != 0 {
            prev = s;
        }
    }
    if pairs == 0 {
        0.0
    } else {
        flips as f64 / pairs as f64
    }
}

/// Palette churn: the mean per-second Euclidean distance between consecutive
/// hops' intensity-weighted mean drawn colours (normalised RGB). `0` for a
/// single frame.
#[must_use]
pub fn palette_churn(color: &[[f32; 3]], hop_ms: f32) -> f64 {
    if color.len() < 2 {
        return 0.0;
    }
    let hop_s = f64::from(hop_ms) / 1000.0;
    if hop_s <= 0.0 {
        return 0.0;
    }
    let mut sum = 0.0f64;
    for w in color.windows(2) {
        let d = ((f64::from(w[1][0]) - f64::from(w[0][0])).powi(2)
            + (f64::from(w[1][1]) - f64::from(w[0][1])).powi(2)
            + (f64::from(w[1][2]) - f64::from(w[0][2])).powi(2))
        .sqrt();
        sum += d;
    }
    let mean_per_hop = sum / (color.len() - 1) as f64;
    mean_per_hop / hop_s
}

/// Pearson correlation of two equal-length series. `0` when either has no
/// variance or fewer than two points.
#[must_use]
pub fn pearson(a: &[f32], b: &[f32]) -> f64 {
    pearson_and_slope(a, b).0
}

/// Pearson correlation `r` and the least-squares slope of `b` onto `a`
/// (`b ≈ slope · a + c`). Returns `(0, 0)` for a degenerate input.
fn pearson_and_slope(a: &[f32], b: &[f32]) -> (f64, f64) {
    let n = a.len().min(b.len());
    if n < 2 {
        return (0.0, 0.0);
    }
    let nf = n as f64;
    let mut sa = 0.0;
    let mut sb = 0.0;
    for i in 0..n {
        sa += f64::from(a[i]);
        sb += f64::from(b[i]);
    }
    let ma = sa / nf;
    let mb = sb / nf;
    let mut cov = 0.0;
    let mut va = 0.0;
    let mut vb = 0.0;
    for i in 0..n {
        let da = f64::from(a[i]) - ma;
        let db = f64::from(b[i]) - mb;
        cov += da * db;
        va += da * da;
        vb += db * db;
    }
    if va <= 0.0 || vb <= 0.0 {
        return (0.0, 0.0);
    }
    let r = cov / (va.sqrt() * vb.sqrt());
    let slope = cov / va;
    (r, slope)
}

/// Linear-interpolated percentile `p` (`0.0..=1.0`) of a series. Sorts a copy.
#[must_use]
pub fn percentile(values: &[f32], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v: Vec<f64> = values.iter().map(|&x| f64::from(x)).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if v.len() == 1 {
        return v[0];
    }
    let rank = p.clamp(0.0, 1.0) * (v.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    let frac = rank - lo as f64;
    v[lo] + (v[hi] - v[lo]) * frac
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onset_delay_is_recovered_within_one_hop() {
        // A deterministic spiky onset envelope; motion is that envelope delayed
        // by exactly K hops and scaled. The search must recover lag ≈ K.
        const K: usize = 7;
        const GAIN: f32 = 0.5;
        let n = 400;
        let mut onset = vec![0.0f32; n];
        for (i, o) in onset.iter_mut().enumerate() {
            // A varied, non-periodic drive so correlation has a unique peak.
            *o = ((i as f32 * 0.37).sin() * 0.5 + 0.5) * if i % 5 == 0 { 1.0 } else { 0.2 };
        }
        let mut motion = vec![0.0f32; n];
        for i in K..n {
            motion[i] = onset[i - K] * GAIN;
        }
        let (lag, gain) = onset_response(&onset, &motion, 40);
        assert!(
            (lag as i64 - K as i64).abs() <= 1,
            "recovered lag {lag}, expected ~{K}"
        );
        assert!((gain - f64::from(GAIN)).abs() < 0.05, "gain {gain}");
    }

    #[test]
    fn static_brightness_has_zero_flicker() {
        let b = vec![0.4f32; 100];
        assert_eq!(flicker(&b, 1e-4), 0.0);
    }

    #[test]
    fn ramp_brightness_has_zero_flicker() {
        // Monotone rise: deltas never change sign.
        let b: Vec<f32> = (0..100).map(|i| i as f32 / 100.0).collect();
        assert_eq!(flicker(&b, 1e-4), 0.0);
    }

    #[test]
    fn alternating_brightness_has_max_flicker() {
        let b: Vec<f32> = (0..100)
            .map(|i| if i % 2 == 0 { 0.0 } else { 1.0 })
            .collect();
        assert!(flicker(&b, 1e-4) > 0.95, "flicker {}", flicker(&b, 1e-4));
    }

    #[test]
    fn coverage_stats_match_known_series() {
        let cov = [0.1f32, 0.2, 0.3, 0.4, 0.5];
        let (mean, p95) = coverage_stats(&cov);
        // f32→f64 widening of the inputs introduces ~1e-8 error.
        assert!((mean - 0.3).abs() < 1e-6, "mean {mean}");
        assert!(p95 > 0.4 && p95 <= 0.5, "p95 {p95}");
    }

    #[test]
    fn quiet_stillness_averages_only_quiet_hops() {
        // Loud hops (rms 1.0) move a lot; quiet hops (rms 0.0) barely move.
        let rms = [1.0f32, 0.0, 1.0, 0.0];
        let motion = [0.9f32, 0.01, 0.9, 0.03];
        let q = quiet_stillness(&rms, &motion, 0.2);
        assert!((q - 0.02).abs() < 1e-6, "quiet stillness {q}");
    }

    #[test]
    fn pearson_perfect_and_anti() {
        let a = [1.0f32, 2.0, 3.0, 4.0];
        let up = [2.0f32, 4.0, 6.0, 8.0];
        let down = [8.0f32, 6.0, 4.0, 2.0];
        assert!((pearson(&a, &up) - 1.0).abs() < 1e-9);
        assert!((pearson(&a, &down) + 1.0).abs() < 1e-9);
    }

    #[test]
    fn palette_churn_zero_when_static() {
        let c = vec![[0.2f32, 0.4, 0.6]; 50];
        assert_eq!(palette_churn(&c, 5.0), 0.0);
    }
}
