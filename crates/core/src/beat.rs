//! Causal tempo induction and beat-phase prediction from the onset detection
//! function.
//!
//! The [`BeatTracker`] consumes the per-hop half-wave-rectified spectral-flux
//! onset detection function (ODF) the [`OnsetDetector`](crate::onset::OnsetDetector)
//! already computes — it never runs an FFT of its own and never buffers audio.
//! It maintains a short ring of recent ODF values (~6 s), periodically induces
//! the dominant inter-beat interval by autocorrelation of that ring shaped by a
//! comb filterbank and a Rayleigh tempo prior, then predicts beats causally by
//! advancing a phase accumulator at the locked period and gently re-aligning it
//! to the recent ODF peaks. Everything is strictly causal (no lookahead) and,
//! after construction, allocation-free — the periodic induction pass reuses
//! preallocated scratch.
//!
//! The algorithm follows the design of BTrack (A. M. Stark, M. E. P. Davies and
//! M. D. Plumbley, "Real-Time Beat-Synchronous Analysis of Musical Audio",
//! Proc. DAFx-09, Como, 2009) and the tempo/beat-period model of M. E. P.
//! Davies and M. D. Plumbley, "Context-Dependent Beat Tracking of Musical
//! Audio", IEEE TASLP 15(3), 2007. It is a fresh implementation written from
//! those papers' descriptions of autocorrelation, the shift-invariant comb
//! filterbank and the Rayleigh tempo weighting; no third-party beat-tracking
//! source was consulted.

/// Length of the ODF ring, seconds. Long enough to hold several beats at the
/// slowest tracked tempo so the autocorrelation has multiple periods to work
/// with, short enough to follow tempo changes.
const RING_SECONDS: f32 = 6.0;

/// Slowest tracked tempo, BPM. Sets the longest beat period (largest lag).
const TEMPO_MIN_BPM: f32 = 60.0;

/// Fastest tracked tempo, BPM. Sets the shortest beat period (smallest lag).
const TEMPO_MAX_BPM: f32 = 180.0;

/// Tempo the Rayleigh prior peaks at, BPM. Biases the induction toward the
/// perceptually common 100–140 range without forbidding the extremes.
const RAYLEIGH_PEAK_BPM: f32 = 120.0;

/// Floor of the (max-normalized) Rayleigh weighting: the prior only ever scales
/// a candidate down to this fraction, so it biases rather than vetoes. A milder
/// prior than a raw Rayleigh curve, which would zero the tempo extremes.
const RAYLEIGH_FLOOR: f32 = 0.55;

/// How often the induction pass runs, seconds. The per-hop path only advances
/// the phase; the O(ring × lags) autocorrelation runs on this cadence.
const INDUCTION_SECONDS: f32 = 1.2;

/// Number of comb-filter harmonics summed per candidate period.
const HARMONICS: usize = 4;

/// Smoothed-confidence level at which an unlocked tracker locks on.
const LOCK_ON: f32 = 0.38;

/// Smoothed-confidence level below which a locked tracker unlocks.
const LOCK_OFF: f32 = 0.22;

/// EMA factor applied to the raw induction confidence each pass.
const CONF_SMOOTH: f32 = 0.5;

/// Fraction of the beat period the locked period EMA moves toward a new
/// estimate when the estimate is close (a smooth track); a far estimate jumps.
const PERIOD_TRACK: f32 = 0.25;

/// Relative gap between old and new period beyond which the period jumps rather
/// than tracks (a genuine tempo change, not jitter).
const PERIOD_JUMP_REL: f32 = 0.15;

/// Fraction of the phase error corrected per induction pass once locked (a soft
/// phase-locked loop toward the ODF peaks). The first lock aligns hard.
const PHASE_CORRECT: f32 = 0.5;

/// Time constant of the short-term ODF level EMA, seconds. Drives the fast
/// silence gate so confidence collapses within a fraction of a second of the
/// signal dropping out, without waiting for the next induction pass.
const ODF_LEVEL_TAU_S: f32 = 0.25;

/// ODF level below which the input counts as silent: confidence is pulled down
/// fast and the tracker unlocks.
const SILENCE_LEVEL: f32 = 1e-3;

/// Per-hop multiplicative decay applied to confidence while the input is silent.
const SILENCE_DECAY: f32 = 0.90;

/// Variance floor of the ODF window below which an induction pass reports no
/// confidence (the window is effectively flat / silent).
const VAR_FLOOR: f32 = 1e-9;

/// ODF-window kurtosis below which the confidence gate is fully closed. A
/// beat-driven ODF is impulsive — it sits near zero between onsets and spikes on
/// them — so it is strongly leptokurtic (kurtosis ≫ 3); a sustained tone's
/// spectral-leakage ripple is roughly sinusoidal (kurtosis ~1.5) and broadband
/// noise is roughly Gaussian (kurtosis ~3). Kurtosis is normalized by the
/// variance, so it measures the ODF's *shape* independent of level: it stays low
/// for a tone even as the onset detector's slow normalization creeps that tone's
/// flux up to full scale, which a peak- or duty-based gate cannot.
const KURT_LO: f32 = 6.0;

/// ODF-window kurtosis at (or above) which the impulsiveness gate is fully open.
const KURT_HI: f32 = 12.0;

/// Smooth 0→1 ramp over `[lo, hi]` (Hermite), clamped outside. `lo < hi`.
fn smoothstep(lo: f32, hi: f32, x: f32) -> f32 {
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Per-hop tempo/phase/confidence estimate published into the snapshot.
#[derive(Clone, Copy, Debug, Default)]
pub struct BeatEstimate {
    /// Locked tempo in BPM, or `0.0` while unlocked.
    pub tempo_bpm: f32,
    /// Beat phase in `0.0..1.0` (0 at a predicted beat), or `0.0` while unlocked.
    pub phase: f32,
    /// Confidence in `0.0..=1.0`; always published, low on silence/noise.
    pub confidence: f32,
}

/// Causal beat tracker over a stream of ODF (spectral-flux) values. Construct
/// once for a given sample rate and hop size; [`process_hop`](Self::process_hop)
/// is then allocation-free.
pub struct BeatTracker {
    /// Seconds per hop.
    dt: f32,

    // --- ODF ring (chronological via `head`/`filled`) ---
    ring: Vec<f32>,
    head: usize,
    filled: usize,

    // --- preallocated induction scratch ---
    /// Mean-subtracted window in chronological order (index 0 = oldest kept).
    work: Vec<f32>,
    /// Normalized autocorrelation, indexed by lag (`acf[0]` unused).
    acf: Vec<f32>,
    /// Weighted comb score per candidate lag (index 0 = `min_lag`).
    score: Vec<f32>,
    /// Max-normalized, floored Rayleigh weight per candidate lag.
    rayleigh: Vec<f32>,

    // --- lag geometry ---
    min_lag: usize,
    max_tap: usize,
    /// Below this fill count no lock is attempted (need a few periods).
    min_fill: usize,

    // --- scheduling ---
    induction_interval: usize,
    hops_since_induction: usize,

    // --- state ---
    locked: bool,
    period_hops: f32,
    phase: f32,
    confidence: f32,
    tempo_bpm: f32,
    odf_level: f32,
}

impl BeatTracker {
    /// Build a tracker for `sample_rate` Hz audio processed in `hop_frames`-frame
    /// hops. Allocates all scratch up front.
    #[must_use]
    pub fn new(sample_rate: u32, hop_frames: usize) -> Self {
        let sr = sample_rate.max(1) as f32;
        let dt = hop_frames.max(1) as f32 / sr;

        // Lag = beat period in hops = 60 / (bpm · dt).
        let min_lag = (60.0 / (TEMPO_MAX_BPM * dt)).floor().max(2.0) as usize;
        let max_lag = (60.0 / (TEMPO_MIN_BPM * dt)).ceil() as usize;
        let max_lag = max_lag.max(min_lag + 1);
        // Highest autocorrelation lag any comb tap reaches.
        let max_tap = HARMONICS * max_lag + (HARMONICS - 1);

        // Ring holds RING_SECONDS, but never fewer than a couple of the longest
        // periods so the autocorrelation always has room for its taps.
        let ring_len = ((RING_SECONDS / dt).round() as usize).max(max_tap + 2);
        let n_cand = max_lag - min_lag + 1;

        let mut rayleigh = vec![0.0f32; n_cand];
        // Rayleigh over lag, peaking at the lag of RAYLEIGH_PEAK_BPM, then
        // max-normalized and lifted to RAYLEIGH_FLOOR so it biases, not vetoes.
        let sigma = 60.0 / (RAYLEIGH_PEAK_BPM * dt);
        let mut rmax = 0.0f32;
        for (i, w) in rayleigh.iter_mut().enumerate() {
            let lag = (min_lag + i) as f32;
            let v = (lag / (sigma * sigma)) * (-(lag * lag) / (2.0 * sigma * sigma)).exp();
            *w = v;
            rmax = rmax.max(v);
        }
        let rmax = rmax.max(1e-12);
        for w in &mut rayleigh {
            let norm = *w / rmax;
            *w = RAYLEIGH_FLOOR + (1.0 - RAYLEIGH_FLOOR) * norm;
        }

        let induction_interval = ((INDUCTION_SECONDS / dt).round() as usize).max(1);
        let min_fill = (3 * max_lag).min(ring_len);

        Self {
            dt,
            ring: vec![0.0; ring_len],
            head: 0,
            filled: 0,
            work: vec![0.0; ring_len],
            acf: vec![0.0; max_tap + 1],
            score: vec![0.0; n_cand],
            rayleigh,
            min_lag,
            max_tap,
            min_fill,
            induction_interval,
            hops_since_induction: 0,
            locked: false,
            period_hops: 0.0,
            phase: 0.0,
            confidence: 0.0,
            tempo_bpm: 0.0,
            odf_level: 0.0,
        }
    }

    /// Consume one hop's ODF value and return the current beat estimate.
    /// Strictly causal and allocation-free.
    pub fn process_hop(&mut self, odf: f32) -> BeatEstimate {
        // Push into the ring.
        self.ring[self.head] = odf;
        self.head = (self.head + 1) % self.ring.len();
        if self.filled < self.ring.len() {
            self.filled += 1;
        }

        // Short-term ODF level for the fast silence gate.
        let alpha = 1.0 - (-self.dt / ODF_LEVEL_TAU_S).exp();
        self.odf_level += alpha * (odf - self.odf_level);

        // Periodic induction.
        self.hops_since_induction += 1;
        if self.hops_since_induction >= self.induction_interval {
            self.hops_since_induction = 0;
            if self.filled >= self.min_fill {
                self.induct();
            }
        }

        // Fast silence gate: collapse confidence and unlock when the ODF has
        // gone quiet, without waiting for the next induction pass.
        if self.odf_level < SILENCE_LEVEL {
            self.confidence *= SILENCE_DECAY;
            if self.confidence < LOCK_OFF {
                self.locked = false;
                self.tempo_bpm = 0.0;
            }
        }

        // Advance the beat phase (free-running between induction passes).
        if self.locked && self.period_hops > 0.0 {
            self.phase += 1.0 / self.period_hops;
            if self.phase >= 1.0 {
                self.phase -= self.phase.floor();
            }
        }

        if self.locked {
            BeatEstimate {
                tempo_bpm: self.tempo_bpm,
                phase: self.phase,
                confidence: self.confidence,
            }
        } else {
            BeatEstimate {
                tempo_bpm: 0.0,
                phase: 0.0,
                confidence: self.confidence,
            }
        }
    }

    /// Reset all runtime state (ring, lock, phase). Called on a format change.
    pub fn reset(&mut self) {
        for v in &mut self.ring {
            *v = 0.0;
        }
        self.head = 0;
        self.filled = 0;
        self.hops_since_induction = 0;
        self.locked = false;
        self.period_hops = 0.0;
        self.phase = 0.0;
        self.confidence = 0.0;
        self.tempo_bpm = 0.0;
        self.odf_level = 0.0;
    }

    /// One induction pass: autocorrelate the ODF window, comb-filter and
    /// Rayleigh-weight it to pick the dominant beat period, update the smoothed
    /// confidence and the lock state, and (when locked) track the period and
    /// re-align the phase to the recent ODF peaks. Allocation-free.
    fn induct(&mut self) {
        let w = self.filled;

        // Copy the window into `work` in chronological order (oldest → newest)
        // and subtract its mean, so the autocorrelation measures periodicity,
        // not the ODF's DC level.
        let mut sum = 0.0f32;
        for i in 0..w {
            // Age from newest: newest is age 0.
            let age = w - 1 - i;
            let idx = (self.head + self.ring.len() - 1 - age) % self.ring.len();
            let v = self.ring[idx];
            self.work[i] = v;
            sum += v;
        }
        let mean = sum / w as f32;
        // Center the window; accumulate the 2nd and 4th moments for the variance
        // (r0) and the kurtosis-based impulsiveness gate.
        let mut r0 = 0.0f32;
        let mut m4 = 0.0f32;
        for v in &mut self.work[..w] {
            *v -= mean;
            let d2 = *v * *v;
            r0 += d2;
            m4 += d2 * d2;
        }
        // Kurtosis = W·Σd⁴ / (Σd²)², scale-invariant. Guard the flat window.
        let kurtosis = if r0 > VAR_FLOOR {
            w as f32 * m4 / (r0 * r0)
        } else {
            0.0
        };

        // Effectively flat / silent window: no periodicity to report.
        if r0 < VAR_FLOOR {
            self.confidence += CONF_SMOOTH * (0.0 - self.confidence);
            if self.confidence < LOCK_OFF {
                self.locked = false;
                self.tempo_bpm = 0.0;
            }
            return;
        }

        // Normalized autocorrelation for every lag a comb tap can reach.
        let top_tap = self.max_tap.min(w.saturating_sub(1));
        for tau in 1..=top_tap {
            let mut acc = 0.0f32;
            for i in tau..w {
                acc += self.work[i] * self.work[i - tau];
            }
            self.acf[tau] = acc / r0;
        }
        for tau in (top_tap + 1)..self.acf.len() {
            self.acf[tau] = 0.0;
        }

        // Comb filterbank over candidate periods, weighted by the Rayleigh prior.
        // comb(L) = Σ_m w_m · max_{|δ|<m} acf[m·L+δ]; w_m = 1/m favours the
        // fundamental so a metrical subdivision cannot outscore the true beat.
        const TAP_WEIGHT_SUM: f32 = {
            // 1/1 + 1/2 + 1/3 + 1/4
            1.0 + 1.0 / 2.0 + 1.0 / 3.0 + 1.0 / 4.0
        };
        let mut best_idx = 0usize;
        let mut best_weighted = f32::MIN;
        let mut best_comb = 0.0f32;
        for idx in 0..self.score.len() {
            let lag = self.min_lag + idx;
            let mut comb = 0.0f32;
            for m in 1..=HARMONICS {
                let center = m * lag;
                let tol = m - 1;
                let lo = center.saturating_sub(tol);
                let hi = (center + tol).min(self.max_tap);
                let mut peak = 0.0f32;
                for tau in lo..=hi {
                    if self.acf[tau] > peak {
                        peak = self.acf[tau];
                    }
                }
                comb += peak / m as f32;
            }
            let weighted = comb * self.rayleigh[idx];
            self.score[idx] = weighted;
            if weighted > best_weighted {
                best_weighted = weighted;
                best_idx = idx;
                best_comb = comb;
            }
        }

        // Sub-lag refinement by parabolic interpolation on the weighted score.
        let mut refined = (self.min_lag + best_idx) as f32;
        if best_idx > 0 && best_idx + 1 < self.score.len() {
            let sm = self.score[best_idx - 1];
            let s0 = self.score[best_idx];
            let sp = self.score[best_idx + 1];
            let denom = sm - 2.0 * s0 + sp;
            if denom.abs() > 1e-12 {
                let delta = 0.5 * (sm - sp) / denom;
                if delta.abs() < 1.0 {
                    refined += delta;
                }
            }
        }

        // Raw confidence: average normalized comb energy at the winning period,
        // in 0..1. Flat/noise windows give a near-zero comb; a clear periodicity
        // gives a high one. Then gated by the ODF's impulsiveness so a sustained
        // tone or noise, whose ODF is periodic-but-busy (high normalized ACF, no
        // real beats), cannot masquerade as confident music.
        let gate = smoothstep(KURT_LO, KURT_HI, kurtosis);
        let raw_conf = (best_comb / TAP_WEIGHT_SUM).clamp(0.0, 1.0) * gate;
        self.confidence += CONF_SMOOTH * (raw_conf - self.confidence);

        if self.confidence >= LOCK_ON {
            let was_locked = self.locked;
            if !was_locked {
                // Fresh lock: adopt the period and align the phase hard.
                self.locked = true;
                self.period_hops = refined;
                self.align_phase_hard(refined);
            } else {
                // Track the period: EMA when close, jump on a real tempo change.
                let rel = (refined - self.period_hops).abs() / self.period_hops.max(1e-6);
                if rel > PERIOD_JUMP_REL {
                    self.period_hops = refined;
                } else {
                    self.period_hops += PERIOD_TRACK * (refined - self.period_hops);
                }
                self.align_phase_soft(self.period_hops);
            }
            self.tempo_bpm = 60.0 / (self.period_hops * self.dt);
        } else if self.confidence < LOCK_OFF {
            self.locked = false;
            self.tempo_bpm = 0.0;
        }
    }

    /// Best beat-grid offset (in hops before the newest sample) for period
    /// `period`, by summing the ODF along candidate beat grids. Returns the
    /// offset in `0..round(period)` whose grid best hits the recent ODF peaks.
    fn best_offset(&self, period: f32) -> usize {
        let pr = period.round().max(1.0) as usize;
        let kmax = (self.filled / pr).clamp(1, 8);
        let mut best_off = 0usize;
        let mut best_sum = f32::MIN;
        for off in 0..pr {
            let mut acc = 0.0f32;
            for k in 0..kmax {
                let age = off + k * pr;
                if age >= self.filled {
                    break;
                }
                let idx = (self.head + self.ring.len() - 1 - age) % self.ring.len();
                acc += self.ring[idx];
            }
            if acc > best_sum {
                best_sum = acc;
                best_off = off;
            }
        }
        best_off
    }

    /// Hard phase alignment: set the phase so the last beat sits on the strongest
    /// recent ODF grid position.
    fn align_phase_hard(&mut self, period: f32) {
        let off = self.best_offset(period) as f32;
        self.phase = (off / period).fract();
    }

    /// Soft phase alignment: nudge the phase a fraction of the way toward the
    /// grid the recent ODF prefers, so a locked tracker stays aligned without
    /// jumping.
    fn align_phase_soft(&mut self, period: f32) {
        let off = self.best_offset(period) as f32;
        let desired = (off / period).fract();
        // Shortest signed distance on the phase circle.
        let mut err = desired - self.phase;
        if err > 0.5 {
            err -= 1.0;
        } else if err < -0.5 {
            err += 1.0;
        }
        self.phase += PHASE_CORRECT * err;
        self.phase -= self.phase.floor();
    }

    /// Current locked tempo in BPM, or `0.0` while unlocked. For tests/tooling.
    #[must_use]
    pub fn tempo_bpm(&self) -> f32 {
        if self.locked { self.tempo_bpm } else { 0.0 }
    }

    /// Whether the tracker currently holds a tempo lock.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.locked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;
    const HOP: usize = 256;

    fn dt() -> f32 {
        HOP as f32 / SR as f32
    }

    /// Feed an impulse-per-beat ODF at `bpm` for `seconds` and return the final
    /// estimate. This mimics the flux stream a click/kick track produces: a
    /// spike on the hop that contains each beat, near-zero between.
    fn run_pulses(bpm: f32, seconds: f32) -> (BeatTracker, BeatEstimate) {
        let mut bt = BeatTracker::new(SR, HOP);
        let period_hops = 60.0 / (bpm * dt());
        let total = (seconds / dt()) as usize;
        let mut acc = 0.0f32;
        let mut est = BeatEstimate::default();
        for _ in 0..total {
            acc += 1.0;
            let odf = if acc >= period_hops {
                acc -= period_hops;
                1.0
            } else {
                0.0
            };
            est = bt.process_hop(odf);
        }
        (bt, est)
    }

    #[test]
    fn locks_pulse_train_120() {
        let (_, est) = run_pulses(120.0, 10.0);
        println!(
            "beat::locks_pulse_train_120: tempo {:.2} bpm, confidence {:.3}",
            est.tempo_bpm, est.confidence
        );
        assert!(
            (est.tempo_bpm - 120.0).abs() <= 3.0,
            "expected ~120 bpm, got {:.2}",
            est.tempo_bpm
        );
        assert!(
            est.confidence > 0.5,
            "confidence {:.3} too low for a clean pulse train",
            est.confidence
        );
    }

    #[test]
    fn silence_stays_unconfident() {
        let mut bt = BeatTracker::new(SR, HOP);
        let mut est = BeatEstimate::default();
        for _ in 0..2000 {
            est = bt.process_hop(0.0);
        }
        println!(
            "beat::silence_stays_unconfident: tempo {:.2}, confidence {:.3}, locked {}",
            est.tempo_bpm,
            est.confidence,
            bt.is_locked()
        );
        assert!(
            est.confidence < LOCK_OFF,
            "silence produced confidence {:.3}",
            est.confidence
        );
        assert_eq!(est.tempo_bpm, 0.0, "silence should stay unlocked");
    }
}
