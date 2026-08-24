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
//! # Lock / coast / unlock state machine
//!
//! The tracker is always in one of three states, tracked by the `locked` and
//! `coasting` flags:
//!
//! - **Unlocked** (`!locked && !coasting`): no tempo is published
//!   (`tempo_bpm = 0`, `phase = 0`). `confidence` is still published honestly.
//! - **Locked** (`locked`): a tempo is published and the phase free-runs at the
//!   held period, re-aligned to the ODF peaks on each induction pass.
//! - **Coasting** (`coasting`): evidence has weakened but the tempo is *held* —
//!   `tempo_bpm` stays published and `phase` keeps advancing at the last locked
//!   period — while `confidence` continues to decay honestly.
//!
//! The transitions are:
//!
//! - **Unlocked → Locked**: an induction pass smooths confidence to `LOCK_ON`.
//!   The period is adopted and the phase aligned hard to the recent ODF peaks.
//! - **Locked → Coasting**: confidence falls below `LOCK_OFF`. Rather than
//!   dropping the tempo immediately — a breakdown in a dance track suspends
//!   onsets for a few bars while the tempo persists — the tracker holds the
//!   period and starts a coast timer.
//! - **Coasting → Locked**: confidence recovers to `LOCK_OFF` again within the
//!   coast window. Normal locked tracking resumes; the period EMA re-engages on
//!   the next induction pass and the phase is only ever *soft*-aligned, so the
//!   held phase carries across the gap without a jump.
//! - **Coasting → Unlocked**: the coast window (`COAST_SECONDS`) expires, or the
//!   fast silence gate observes true silence (`odf_level < SILENCE_LEVEL`) for
//!   `SILENCE_COAST_SECONDS` — a much shorter cut, so a track *ending* unlocks
//!   in about a second instead of ghost-publishing the full coast window. On
//!   this transition the just-held period is *remembered* for `MEMORY_SECONDS`
//!   (see below), unless true silence wipes the memory first.
//! - **Unlocked → Locked (warm re-lock)**: while unlocked with a live tempo
//!   memory, an induction pass whose winning candidate period sits within
//!   `MEMORY_RELOCK_REL` of the remembered period and whose smoothed confidence
//!   has reached only `LOCK_OFF` (not the full `LOCK_ON` a cold lock needs)
//!   re-locks immediately at the candidate period. Real music holds a long
//!   breakdown's tempo steady while the kurtosis gate pins confidence between
//!   `LOCK_OFF` and `LOCK_ON`; the memory lets that consistent evidence re-lock
//!   cheaply. Unlike the coast resume, the phase is aligned **hard**: seconds of
//!   no publishing have elapsed since the coast ended, so the free-run phase is
//!   stale and must be snapped to the current ODF grid rather than carried. A
//!   fresh lock — cold or warm — clears the memory; it is re-armed only by the
//!   next coast expiry.
//!
//! The tempo memory is armed when a coast expires on its window and cleared when
//! it expires (`MEMORY_SECONDS`), when a fresh lock is taken, or the instant the
//! fast silence gate sees true silence — a new track must never inherit the
//! previous one's tempo. A candidate that stays outside `MEMORY_RELOCK_REL` of
//! the remembered period neither re-locks nor extends the memory window.
//!
//! Because `confidence` is always published honestly — decaying through a coast
//! — a scene gating on `beat_confidence` still sees the evidence weaken and can
//! choose to fade out, even while a coasted `tempo_bpm` keeps the beat grid
//! moving.
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

/// Longest a lock coasts on weak evidence before unlocking fully, seconds. A
/// breakdown in a dance track suspends onsets for a few bars while the tempo
/// persists; the coast keeps the tempo published across that gap. Converted to a
/// hop count in [`BeatTracker::new`].
const COAST_SECONDS: f32 = 6.0;

/// A coast is cut short to this length, seconds, once the fast silence gate sees
/// true silence — a track *ending* must unlock in about a second, not
/// ghost-publish the full [`COAST_SECONDS`] window. Converted to a hop count in
/// [`BeatTracker::new`].
const SILENCE_COAST_SECONDS: f32 = 1.0;

/// How long the tempo is *remembered* after a coast expires, seconds. Real music
/// (e.g. a long house breakdown) can hold the winning candidate steady while the
/// kurtosis gate keeps confidence below [`LOCK_ON`] for many seconds after the
/// coast window has run out. Remembering the coasted period for this window lets
/// consistent evidence re-lock cheaply (see [`MEMORY_RELOCK_REL`]) instead of
/// waiting for a full cold lock. Converted to a hop count in [`BeatTracker::new`].
const MEMORY_SECONDS: f32 = 30.0;

/// Relative tolerance a winning candidate period must fall within, against the
/// remembered period, to trigger a warm re-lock. A candidate outside this band
/// does not re-lock off the memory (it must earn a fresh cold lock at its own
/// period), so a genuine tempo change is never dragged back to the old tempo.
const MEMORY_RELOCK_REL: f32 = 0.03;

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

/// One comb candidate the last induction pass considered: its tempo and the
/// weighted comb score that ranked it. Diagnostic only.
#[derive(Clone, Copy, Debug, Default)]
pub struct BeatCandidate {
    /// Candidate tempo in BPM (the period's fundamental).
    pub bpm: f32,
    /// The Rayleigh-weighted comb score that ranked this candidate.
    pub score: f32,
}

/// Read-only snapshot of the beat tracker's internal induction state, purely
/// for diagnostics and calibration probes — nothing here is ever read back by
/// the tracker and it has no effect on tracking. The induction-derived fields
/// (`kurtosis`, `comb_energy`, `candidate_bpm`, `top`, `inductions`) are filled
/// on each induction pass; the per-hop fields (`odf_level`, `confidence`,
/// `locked`, `coasting`, `tempo_bpm`) reflect the live state at the moment
/// [`BeatTracker::debug_stats`] is called.
#[derive(Clone, Copy, Debug, Default)]
pub struct BeatDebug {
    /// Short-term ODF level EMA driving the fast silence gate (live).
    pub odf_level: f32,
    /// Kurtosis of the ODF window at the last induction pass (the impulsiveness
    /// the confidence gate reads).
    pub kurtosis: f32,
    /// Raw (un-normalized) comb energy summed at the winning period on the last
    /// pass — `best_comb`, before the tap-weight normalization and kurtosis gate.
    pub comb_energy: f32,
    /// Tempo (BPM) of the winning refined period on the last pass, whether or not
    /// the tracker locked it. `0.0` when the last window was flat/silent.
    pub candidate_bpm: f32,
    /// Smoothed confidence in `0.0..=1.0` (live).
    pub confidence: f32,
    /// Whether the tracker currently holds a firm tempo lock (live). Mutually
    /// exclusive with `coasting`.
    pub locked: bool,
    /// Whether the tracker is coasting: holding a previously locked tempo across
    /// a stretch of weak evidence while confidence decays (live). Mutually
    /// exclusive with `locked`; a coasted tracker still publishes `tempo_bpm`.
    pub coasting: bool,
    /// Published tempo in BPM, `0.0` while unlocked; held while coasting (live).
    pub tempo_bpm: f32,
    /// Remembered tempo in BPM held after a coast expiry, `0.0` when no memory is
    /// live (live). A within-tolerance candidate can warm-re-lock onto it while
    /// the tracker is unlocked; see the state-machine docs.
    pub remembered_bpm: f32,
    /// The three highest-scoring comb candidates on the last pass, best first.
    /// Zeroed entries pad a pass that found fewer (or a flat window).
    pub top: [BeatCandidate; 3],
    /// Count of induction passes run so far. A probe watches it change to know a
    /// fresh pass has landed.
    pub inductions: u64,
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
    /// Holding a previously locked tempo across weak evidence (see the module
    /// docs' state machine). Mutually exclusive with `locked`.
    coasting: bool,
    /// Hops elapsed in the current coast; unlocks fully at `coast_max`.
    coast_hops: usize,
    /// Consecutive hops the fast silence gate has seen true silence. A coast is
    /// cut short once this reaches `silence_coast_max`.
    silent_hops: usize,
    /// Coast window length in hops (`COAST_SECONDS`).
    coast_max: usize,
    /// Silence-cut coast length in hops (`SILENCE_COAST_SECONDS`).
    silence_coast_max: usize,
    /// Remembered beat period in hops after a coast expiry, or `0.0` when no
    /// memory is live. Armed by [`end_coast`](Self::end_coast) on a window
    /// expiry; drives the warm re-lock while the tracker is unlocked.
    remembered_period: f32,
    /// Hops elapsed in the current tempo-memory window; the memory clears at
    /// `memory_max`.
    memory_hops: usize,
    /// Tempo-memory window length in hops (`MEMORY_SECONDS`).
    memory_max: usize,
    period_hops: f32,
    phase: f32,
    confidence: f32,
    tempo_bpm: f32,
    odf_level: f32,

    /// Read-only diagnostic mirror of the last induction pass. Written only by
    /// `induct`, read only by [`debug_stats`](Self::debug_stats); never consulted
    /// by the tracker.
    debug: BeatDebug,
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
        let coast_max = ((COAST_SECONDS / dt).round() as usize).max(1);
        let silence_coast_max = ((SILENCE_COAST_SECONDS / dt).round() as usize).max(1);
        let memory_max = ((MEMORY_SECONDS / dt).round() as usize).max(1);

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
            coasting: false,
            coast_hops: 0,
            silent_hops: 0,
            coast_max,
            silence_coast_max,
            remembered_period: 0.0,
            memory_hops: 0,
            memory_max,
            period_hops: 0.0,
            phase: 0.0,
            confidence: 0.0,
            tempo_bpm: 0.0,
            odf_level: 0.0,
            debug: BeatDebug::default(),
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

        // Track how long the input has been truly silent: this drives the fast
        // coast cut so a track ending unlocks quickly instead of coasting a full
        // window.
        if self.odf_level < SILENCE_LEVEL {
            self.silent_hops = self.silent_hops.saturating_add(1);
        } else {
            self.silent_hops = 0;
        }

        // Periodic induction. It smooths confidence and, when confidence is
        // strong, (re-)adopts the period and re-aligns the phase.
        self.hops_since_induction += 1;
        if self.hops_since_induction >= self.induction_interval {
            self.hops_since_induction = 0;
            if self.filled >= self.min_fill {
                self.induct();
            }
        }

        // Fast silence gate: collapse confidence when the ODF has gone quiet,
        // without waiting for the next induction pass. The unlock itself is left
        // to the state machine below.
        if self.odf_level < SILENCE_LEVEL {
            self.confidence *= SILENCE_DECAY;
        }

        // Advance the lock / coast / unlock state machine from the current
        // confidence and silence run. A coast that expires on its window arms the
        // tempo memory from inside here.
        self.update_lock_state();

        // True silence wipes the tempo memory the instant it is seen — a new track
        // must never inherit the previous one's tempo. This runs *after*
        // `update_lock_state` so it dominates the memory a silence-cut coast just
        // armed: the silence-cut path funnels through `end_coast` like a window
        // expiry, then this clears what it armed, leaving no memory behind.
        if self.odf_level < SILENCE_LEVEL {
            self.clear_memory();
        }

        // Age the tempo memory and expire it after its window. Memory only ever
        // lives while unlocked (a fresh lock clears it), so this simply counts
        // down the remembering window.
        if self.remembered_period > 0.0 {
            self.memory_hops += 1;
            if self.memory_hops >= self.memory_max {
                self.clear_memory();
            }
        }

        // Advance the beat phase (free-running between induction passes) whenever
        // a tempo is being published — locked or coasting.
        if self.publishing() && self.period_hops > 0.0 {
            self.phase += 1.0 / self.period_hops;
            if self.phase >= 1.0 {
                self.phase -= self.phase.floor();
            }
        }

        if self.publishing() {
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

    /// Whether a tempo is currently being published — the tracker is firmly
    /// locked or coasting a held tempo across weak evidence.
    #[inline]
    fn publishing(&self) -> bool {
        self.locked || self.coasting
    }

    /// Advance the lock / coast / unlock state machine from the live
    /// `confidence`, the coast timer and the true-silence run. See the module
    /// docs for the full transition table. The firm-lock adoption of a period
    /// and hard phase alignment lives in [`induct`](Self::induct); this only
    /// handles weakening (lock → coast), recovery (coast → lock) and the two
    /// ways a coast ends (window expiry, silence cut).
    fn update_lock_state(&mut self) {
        if self.locked {
            // A firm lock supersedes any lingering coast flag.
            self.coasting = false;
            self.coast_hops = 0;
            if self.confidence < LOCK_OFF {
                // Evidence weakened: hold the tempo and start coasting.
                self.locked = false;
                self.coasting = true;
                self.coast_hops = 0;
            }
            return;
        }
        if self.coasting {
            if self.confidence >= LOCK_OFF {
                // Evidence recovered within the window: resume the lock. The
                // period EMA and soft phase alignment re-engage on the next
                // induction pass, so the held phase carries across with no jump.
                self.coasting = false;
                self.locked = true;
                self.coast_hops = 0;
            } else if self.silent_hops >= self.silence_coast_max
                || self.coast_hops >= self.coast_max
            {
                // True silence cut the coast short, or the window expired: drop
                // the tempo entirely.
                self.end_coast();
            } else {
                self.coast_hops += 1;
            }
        }
    }

    /// Fully unlock at the end of a coast: no tempo, phase reset. Arms the tempo
    /// memory from the just-held period so consistent later evidence can warm-
    /// re-lock cheaply. Both coast-end paths (window expiry and silence cut) reach
    /// here; the silence-cut path's memory is wiped immediately afterward by the
    /// true-silence gate in [`process_hop`](Self::process_hop), so only a
    /// window-expiry coast actually leaves a live memory.
    fn end_coast(&mut self) {
        self.coasting = false;
        self.locked = false;
        self.coast_hops = 0;
        if self.period_hops > 0.0 {
            self.remembered_period = self.period_hops;
            self.memory_hops = 0;
        }
        self.tempo_bpm = 0.0;
        self.phase = 0.0;
    }

    /// Clear the tempo memory (no remembered period). Called on a fresh lock, on
    /// memory-window expiry, on true silence, and on [`reset`](Self::reset).
    fn clear_memory(&mut self) {
        self.remembered_period = 0.0;
        self.memory_hops = 0;
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
        self.coasting = false;
        self.coast_hops = 0;
        self.silent_hops = 0;
        self.remembered_period = 0.0;
        self.memory_hops = 0;
        self.period_hops = 0.0;
        self.phase = 0.0;
        self.confidence = 0.0;
        self.tempo_bpm = 0.0;
        self.odf_level = 0.0;
        self.debug = BeatDebug::default();
    }

    /// One induction pass: autocorrelate the ODF window, comb-filter and
    /// Rayleigh-weight it to pick the dominant beat period, update the smoothed
    /// confidence and the lock state, and (when locked) track the period and
    /// re-align the phase to the recent ODF peaks. Allocation-free.
    fn induct(&mut self) {
        // Count every pass (including a flat-window early return) so a probe can
        // tell a fresh pass has landed. Diagnostic only.
        self.debug.inductions += 1;
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

        // Effectively flat / silent window: no periodicity to report. Decay
        // confidence honestly; the lock / coast / unlock transition is left to
        // [`update_lock_state`](Self::update_lock_state) so a flat window during
        // a breakdown coasts rather than dropping the tempo outright.
        if r0 < VAR_FLOOR {
            self.confidence += CONF_SMOOTH * (0.0 - self.confidence);
            // Diagnostic: a flat pass has no candidates.
            self.debug.kurtosis = kurtosis;
            self.debug.comb_energy = 0.0;
            self.debug.candidate_bpm = 0.0;
            self.debug.top = [BeatCandidate::default(); 3];
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
            // A tracker that was locked *or* coasting is continuing an existing
            // lock: track the period and only ever soft-align, so a coast's held
            // phase carries across the gap without a jump. A cold tracker takes a
            // fresh lock: adopt the period and align hard.
            let continuing = self.locked || self.coasting;
            self.locked = true;
            self.coasting = false;
            self.coast_hops = 0;
            if continuing {
                // Track the period: EMA when close, jump on a real tempo change.
                let rel = (refined - self.period_hops).abs() / self.period_hops.max(1e-6);
                if rel > PERIOD_JUMP_REL {
                    self.period_hops = refined;
                } else {
                    self.period_hops += PERIOD_TRACK * (refined - self.period_hops);
                }
                self.align_phase_soft(self.period_hops);
            } else {
                self.period_hops = refined;
                self.align_phase_hard(refined);
            }
            self.tempo_bpm = 60.0 / (self.period_hops * self.dt);
            // A fresh lock supersedes any tempo memory; it is re-armed only by the
            // next coast expiry.
            self.clear_memory();
        } else if self.remembered_period > 0.0
            && !self.locked
            && !self.coasting
            && self.confidence >= LOCK_OFF
        {
            // Warm re-lock: unlocked with a live tempo memory. A cold lock would
            // need LOCK_ON, but real music can hold a candidate steady while the
            // kurtosis gate pins confidence between LOCK_OFF and LOCK_ON. If the
            // winning candidate matches the remembered period, re-lock at the
            // reduced threshold. The phase is aligned *hard*, not soft like a
            // coast resume: seconds of no publishing have elapsed since the coast
            // ended, so the free-run phase is stale and must snap to the current
            // ODF grid rather than carry across. A candidate outside the band
            // does not re-lock and does not extend the memory window.
            let rel = (refined - self.remembered_period).abs() / self.remembered_period;
            if rel <= MEMORY_RELOCK_REL {
                self.locked = true;
                self.coasting = false;
                self.coast_hops = 0;
                self.period_hops = refined;
                self.align_phase_hard(refined);
                self.tempo_bpm = 60.0 / (self.period_hops * self.dt);
                self.clear_memory();
            }
        }
        // Weakening below LOCK_OFF and the coast/unlock decision are handled by
        // [`update_lock_state`](Self::update_lock_state) after this pass returns.

        // Diagnostic mirror of this pass: the winning-period energy, the tempo of
        // the refined winning period, and the three top-scoring candidates. This
        // is a read-only pass over the scores already computed above — it writes
        // only `self.debug` and never feeds back into tracking.
        let mut top = [BeatCandidate::default(); 3];
        for idx in 0..self.score.len() {
            let cand = BeatCandidate {
                bpm: 60.0 / ((self.min_lag + idx) as f32 * self.dt),
                score: self.score[idx],
            };
            for slot in 0..top.len() {
                if cand.score > top[slot].score {
                    for j in (slot + 1..top.len()).rev() {
                        top[j] = top[j - 1];
                    }
                    top[slot] = cand;
                    break;
                }
            }
        }
        self.debug.kurtosis = kurtosis;
        self.debug.comb_energy = best_comb;
        self.debug.candidate_bpm = 60.0 / (refined * self.dt);
        self.debug.top = top;
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

    /// Current published tempo in BPM, or `0.0` while unlocked. Held (non-zero)
    /// while coasting. For tests/tooling.
    #[must_use]
    pub fn tempo_bpm(&self) -> f32 {
        if self.publishing() {
            self.tempo_bpm
        } else {
            0.0
        }
    }

    /// Whether the tracker currently holds a firm tempo lock. `false` while
    /// coasting (see [`is_coasting`](Self::is_coasting)).
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Whether the tracker is coasting a held tempo across weak evidence.
    #[must_use]
    pub fn is_coasting(&self) -> bool {
        self.coasting
    }

    /// Read-only snapshot of the tracker's internal induction state, for
    /// diagnostics and calibration probes only (see [`BeatDebug`]). It combines
    /// the last induction pass's mirror with the live per-hop state; it is
    /// allocation-free, never consulted by the tracker, and has no effect on
    /// tracking or on the published [`BeatEstimate`].
    #[must_use]
    pub fn debug_stats(&self) -> BeatDebug {
        BeatDebug {
            odf_level: self.odf_level,
            confidence: self.confidence,
            locked: self.locked,
            coasting: self.coasting,
            tempo_bpm: self.tempo_bpm(),
            remembered_bpm: if self.remembered_period > 0.0 {
                60.0 / (self.remembered_period * self.dt)
            } else {
                0.0
            },
            ..self.debug
        }
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

    /// Deterministic pseudo-noise in `-1.0..=1.0` from a splitmix64 finalizer, so
    /// the coast tests carry no RNG state and reproduce exactly.
    fn pseudo_noise(i: usize) -> f32 {
        let mut z = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        ((z >> 40) as f32 / (1u64 << 23) as f32) - 1.0
    }

    /// One per-hop record from a coast run: the published estimate plus the live
    /// debug flags and the live tempo-memory state.
    #[derive(Clone, Copy)]
    struct Rec {
        est: BeatEstimate,
        locked: bool,
        coasting: bool,
        remembered_bpm: f32,
    }

    /// Drive a pulse train at `bpm` that, over the hop range `gap`, is replaced by
    /// a weak floor (`gap_level == 0.0` → true silence, else a noise floor around
    /// `gap_level`) while the underlying beat grid keeps running, so pulses resume
    /// on the same grid after the gap. Returns one [`Rec`] per hop.
    fn run_pulse_gap(
        bpm: f32,
        total_hops: usize,
        gap: std::ops::Range<usize>,
        gap_level: f32,
    ) -> Vec<Rec> {
        let mut bt = BeatTracker::new(SR, HOP);
        let period_hops = 60.0 / (bpm * dt());
        let mut acc = 0.0f32;
        let mut out = Vec::with_capacity(total_hops);
        for i in 0..total_hops {
            acc += 1.0;
            let is_beat = acc >= period_hops;
            if is_beat {
                acc -= period_hops;
            }
            let odf = if gap.contains(&i) {
                if gap_level == 0.0 {
                    0.0
                } else {
                    (gap_level * (1.0 + 0.5 * pseudo_noise(i))).max(0.0)
                }
            } else if is_beat {
                1.0
            } else {
                0.0
            };
            let est = bt.process_hop(odf);
            out.push(Rec {
                est,
                locked: bt.is_locked(),
                coasting: bt.is_coasting(),
                remembered_bpm: bt.debug_stats().remembered_bpm,
            });
        }
        out
    }

    /// Parameters for [`run_memory_scenario`], a three-phase memory scenario.
    struct MemScenario {
        /// Tempo of the initial lock phase and the gap grid, BPM.
        bpm: f32,
        /// Seconds of clean unit pulses at `bpm` (the initial lock).
        lock_s: f32,
        /// Seconds of gap replacing the pulses.
        gap_s: f32,
        /// Gap floor: `0.0` → true silence, else a noise floor around this level.
        gap_level: f32,
        /// Seconds of resumed pulses after the gap.
        resume_s: f32,
        /// Tempo of the resumed pulses, BPM (may differ from `bpm`).
        resume_bpm: f32,
        /// Amplitude of each resumed pulse.
        resume_amp: f32,
        /// Steady noise floor under the resumed pulses (tunes kurtosis/confidence).
        resume_floor: f32,
        /// Phase offset of the resumed beat grid, in fractions of the resume
        /// period, so the resumed grid can be shifted off the pre-gap phase.
        resume_shift: f32,
    }

    /// One [`Rec`] per hop for a [`MemScenario`], plus `(resume_start_hop,
    /// resume_period_hops)`. The lock/gap grid runs at `bpm`; the resume grid runs
    /// at `resume_bpm`, offset by `resume_shift` of its period, on a `resume_floor`
    /// noise bed — so kurtosis and comb energy (hence confidence) in the resume
    /// window can be tuned to sit below `LOCK_ON` the way real music does.
    fn run_memory_scenario(s: &MemScenario) -> (Vec<Rec>, usize, f32) {
        let mut bt = BeatTracker::new(SR, HOP);
        let period_hops = 60.0 / (s.bpm * dt());
        let resume_period = 60.0 / (s.resume_bpm * dt());
        let lock = hops(s.lock_s);
        let gap = hops(s.gap_s);
        let resume = hops(s.resume_s);
        let total = lock + gap + resume;
        let resume_start = lock + gap;
        let mut acc = 0.0f32;
        // Resume grid accumulator, seeded so the first resumed beat lands
        // `resume_shift` of a period off the grid start.
        let mut racc = s.resume_shift.rem_euclid(1.0) * resume_period;
        let mut out = Vec::with_capacity(total);
        for i in 0..total {
            let odf = if i < resume_start {
                acc += 1.0;
                let is_beat = acc >= period_hops;
                if is_beat {
                    acc -= period_hops;
                }
                if i < lock {
                    if is_beat { 1.0 } else { 0.0 }
                } else if s.gap_level == 0.0 {
                    0.0
                } else {
                    (s.gap_level * (1.0 + 0.5 * pseudo_noise(i))).max(0.0)
                }
            } else {
                racc += 1.0;
                let is_beat = racc >= resume_period;
                if is_beat {
                    racc -= resume_period;
                }
                let floor = if s.resume_floor == 0.0 {
                    0.0
                } else {
                    (s.resume_floor * (1.0 + 0.5 * pseudo_noise(i))).max(0.0)
                };
                if is_beat { floor + s.resume_amp } else { floor }
            };
            let est = bt.process_hop(odf);
            out.push(Rec {
                est,
                locked: bt.is_locked(),
                coasting: bt.is_coasting(),
                remembered_bpm: bt.debug_stats().remembered_bpm,
            });
        }
        (out, resume_start, resume_period)
    }

    /// Index and published confidence of the first hop at or after `from` where
    /// the tracker holds a firm lock, or `None` if it never locks in that span.
    fn first_lock_after(recs: &[Rec], from: usize) -> Option<(usize, f32)> {
        (from..recs.len())
            .find(|&i| recs[i].locked)
            .map(|i| (i, recs[i].est.confidence))
    }

    /// Mean published tempo and locked fraction over the final `secs` of a run.
    fn tail_stats(recs: &[Rec], secs: f32) -> (f32, f32) {
        let tail = &recs[recs.len() - hops(secs)..];
        let tempo = tail.iter().map(|r| r.est.tempo_bpm).sum::<f32>() / tail.len() as f32;
        let locked = tail.iter().filter(|r| r.locked).count() as f32 / tail.len() as f32;
        (tempo, locked)
    }

    /// (a) After a coast expires, the held tempo is remembered; when consistent
    /// evidence returns it re-locks at the reduced `LOCK_OFF` threshold — while
    /// the smoothed confidence is still well below the `LOCK_ON` a cold lock
    /// needs. A 124 BPM lock, a 10 s weak-but-not-silent gap (coast expires
    /// mid-gap, arming the memory), then pulses on a noise bed that holds
    /// confidence in the real-music band (kurtosis gate keeps it under `LOCK_ON`).
    #[test]
    fn warm_relock_from_memory_below_cold_threshold() {
        let s = MemScenario {
            bpm: 124.0,
            lock_s: 8.0,
            gap_s: 10.0,
            gap_level: 0.30,
            resume_s: 8.0,
            resume_bpm: 124.0,
            resume_amp: 1.0,
            resume_floor: 0.25,
            resume_shift: 0.0,
        };
        let (recs, rs, _) = run_memory_scenario(&s);

        // The memory was armed by the coast expiry and is live at resume.
        let mem_gapmax = recs[..rs]
            .iter()
            .map(|r| r.remembered_bpm)
            .fold(0.0f32, f32::max);
        let mem_at_resume = recs[rs].remembered_bpm;

        let (relock, relock_conf) =
            first_lock_after(&recs, rs).expect("never re-locked after the gap");
        let relock_dt = (relock - rs) as f32 * dt();
        let (tail_tempo, tail_locked) = tail_stats(&recs, 1.0);

        println!(
            "beat::warm_relock_from_memory_below_cold_threshold: mem_gapmax {mem_gapmax:.1}, \
             mem_at_resume {mem_at_resume:.1}, relock_conf {relock_conf:.3} \
             (LOCK_OFF {LOCK_OFF}, LOCK_ON {LOCK_ON}), relock_dt {relock_dt:.2}s, \
             tail_tempo {tail_tempo:.2}, tail_locked {tail_locked:.2}"
        );

        assert!(
            (mem_gapmax - 124.0).abs() <= 4.0,
            "memory not armed at ~124 during the gap (max {mem_gapmax:.1})"
        );
        assert!(
            (mem_at_resume - 124.0).abs() <= 4.0,
            "memory not live at resume (was {mem_at_resume:.1})"
        );
        // The decisive property: a firm lock at a confidence below LOCK_ON is only
        // reachable through the warm-re-lock path — a cold lock requires LOCK_ON.
        assert!(
            relock_conf < LOCK_ON,
            "re-lock confidence {relock_conf:.3} reached the cold LOCK_ON — the memory did \
             not lower the threshold"
        );
        assert!(
            relock_conf >= LOCK_OFF,
            "re-lock confidence {relock_conf:.3} below LOCK_OFF — warm re-lock fired too eagerly"
        );
        assert!(
            (tail_tempo - 124.0).abs() <= 3.0,
            "re-locked tempo {tail_tempo:.2} not within ±3 of 124"
        );
        assert!(
            tail_locked > 0.9,
            "tracker did not stay firmly locked after the warm re-lock (frac {tail_locked:.2})"
        );
    }

    /// (b) The memory expires after `MEMORY_SECONDS`: with the gap stretched past
    /// the memory window, resumed pulses get no warm discount and must earn a full
    /// cold lock at `LOCK_ON` again.
    #[test]
    fn memory_expires_then_requires_cold_lock() {
        let s = MemScenario {
            bpm: 124.0,
            lock_s: 8.0,
            // Coast arms the memory ~15 s in; MEMORY_SECONDS=30 → expires ~45 s.
            // A 40 s gap resumes at ~48 s, comfortably past expiry.
            gap_s: 40.0,
            gap_level: 0.30,
            resume_s: 8.0,
            resume_bpm: 124.0,
            resume_amp: 1.0,
            resume_floor: 0.0,
            resume_shift: 0.0,
        };
        let (recs, rs, _) = run_memory_scenario(&s);

        let mem_at_resume = recs[rs].remembered_bpm;
        let (_, relock_conf) =
            first_lock_after(&recs, rs).expect("never re-locked after the long gap");
        let (tail_tempo, _) = tail_stats(&recs, 1.0);

        println!(
            "beat::memory_expires_then_requires_cold_lock: mem_at_resume {mem_at_resume:.1}, \
             relock_conf {relock_conf:.3} (LOCK_ON {LOCK_ON}), tail_tempo {tail_tempo:.2}"
        );

        assert_eq!(
            mem_at_resume, 0.0,
            "memory did not expire across a gap longer than MEMORY_SECONDS"
        );
        assert!(
            relock_conf >= LOCK_ON,
            "re-lock confidence {relock_conf:.3} < LOCK_ON — a stale memory still discounted \
             the lock after it should have expired"
        );
        assert!(
            (tail_tempo - 124.0).abs() <= 3.0,
            "cold-re-locked tempo {tail_tempo:.2} not within ±3 of 124"
        );
    }

    /// (c) True silence during the memory window wipes it immediately, so the next
    /// train cannot inherit the previous tempo: it must cold-lock at `LOCK_ON`.
    #[test]
    fn true_silence_during_memory_clears_it() {
        let s = MemScenario {
            bpm: 124.0,
            lock_s: 8.0,
            gap_s: 4.0,
            gap_level: 0.0, // true silence
            resume_s: 8.0,
            resume_bpm: 124.0,
            resume_amp: 1.0,
            resume_floor: 0.0,
            resume_shift: 0.0,
        };
        let (recs, rs, _) = run_memory_scenario(&s);

        // The silence gate wipes the memory the same hop the coast arms it, so it
        // never becomes observable at all.
        let mem_gapmax = recs.iter().map(|r| r.remembered_bpm).fold(0.0f32, f32::max);
        let (_, relock_conf) =
            first_lock_after(&recs, rs).expect("never re-locked after the silence");
        let (tail_tempo, _) = tail_stats(&recs, 1.0);

        println!(
            "beat::true_silence_during_memory_clears_it: mem_max {mem_gapmax:.1}, \
             relock_conf {relock_conf:.3} (LOCK_ON {LOCK_ON}), tail_tempo {tail_tempo:.2}"
        );

        assert_eq!(
            mem_gapmax, 0.0,
            "a tempo memory survived true silence (max {mem_gapmax:.1})"
        );
        assert!(
            relock_conf >= LOCK_ON,
            "re-lock confidence {relock_conf:.3} < LOCK_ON — silence failed to clear the memory"
        );
        assert!(
            (tail_tempo - 124.0).abs() <= 3.0,
            "cold-re-locked tempo {tail_tempo:.2} not within ±3 of 124"
        );
    }

    /// (d) A genuinely different tempo after a coast expiry does not warm-re-lock
    /// off the stale 124 memory: the 90 BPM candidate sits outside `MEMORY_RELOCK_REL`,
    /// so the tracker cold-locks at 90 only once confidence reaches `LOCK_ON`.
    #[test]
    fn different_tempo_does_not_warm_relock() {
        let s = MemScenario {
            bpm: 124.0,
            lock_s: 8.0,
            gap_s: 10.0,
            gap_level: 0.30,
            resume_s: 8.0,
            resume_bpm: 90.0,
            resume_amp: 1.0,
            resume_floor: 0.0,
            resume_shift: 0.0,
        };
        let (recs, rs, _) = run_memory_scenario(&s);

        // The 124 memory is live at resume — but must go unused by the 90 train.
        let mem_at_resume = recs[rs].remembered_bpm;
        let (_, relock_conf) =
            first_lock_after(&recs, rs).expect("never re-locked at the new tempo");
        let (tail_tempo, tail_locked) = tail_stats(&recs, 1.0);

        println!(
            "beat::different_tempo_does_not_warm_relock: mem_at_resume {mem_at_resume:.1}, \
             relock_conf {relock_conf:.3} (LOCK_ON {LOCK_ON}), tail_tempo {tail_tempo:.2}, \
             tail_locked {tail_locked:.2}"
        );

        assert!(
            (mem_at_resume - 124.0).abs() <= 4.0,
            "the 124 memory was not live at resume (was {mem_at_resume:.1})"
        );
        assert!(
            relock_conf >= LOCK_ON,
            "re-lock at the new tempo happened below LOCK_ON (conf {relock_conf:.3}) — the 124 \
             memory wrongly discounted a different tempo"
        );
        assert!(
            (tail_tempo - 90.0).abs() <= 3.0,
            "tracker did not cold-lock at 90 (tail_tempo {tail_tempo:.2}) — the memory dragged \
             it back toward 124"
        );
        assert!(
            tail_locked > 0.9,
            "tracker did not firmly lock at the new tempo (frac {tail_locked:.2})"
        );
    }

    /// (f) A warm re-lock aligns the phase *hard*, not soft: the free-run phase is
    /// stale after seconds of no publishing, so it must snap to the current ODF
    /// grid. The resumed grid is shifted half a period off the pre-gap grid; after
    /// the warm re-lock the published phase tracks the *new* grid (≈0 at its
    /// beats), which a carried/soft phase — still referenced to the old grid —
    /// could not do (it would sit ≈0.5 at the new grid's beats).
    #[test]
    fn warm_relock_hard_aligns_phase() {
        let s = MemScenario {
            bpm: 124.0,
            lock_s: 8.0,
            gap_s: 10.0,
            gap_level: 0.30,
            resume_s: 8.0,
            resume_bpm: 124.0,
            resume_amp: 1.0,
            resume_floor: 0.25,
            resume_shift: 0.5, // resumed grid is half a period off the old one
        };
        let (recs, rs, rp) = run_memory_scenario(&s);

        let (relock, relock_conf) =
            first_lock_after(&recs, rs).expect("never re-locked after the gap");
        // Confirm it is genuinely the warm path (below the cold threshold).
        assert!(
            relock_conf < LOCK_ON,
            "re-lock confidence {relock_conf:.3} was not a warm re-lock"
        );

        // Reconstruct the resumed beat grid (the same schedule the harness drove).
        let mut racc = 0.5f32.rem_euclid(1.0) * rp;
        let mut beats = Vec::new();
        for i in rs..recs.len() {
            racc += 1.0;
            if racc >= rp {
                racc -= rp;
                beats.push(i);
            }
        }

        // Over the ~2 s after re-lock, published phase at the new grid's beats vs.
        // at the old grid's positions (the midpoints, half a period away).
        let win_end = (relock + hops(2.0)).min(recs.len());
        let dist0 = |ph: f32| ph.min(1.0 - ph);
        let mut worst_on_grid = 0.0f32;
        let mut best_off_grid = 1.0f32; // smallest dist at the old-grid midpoints
        let mut n = 0;
        for &b in &beats {
            if b <= relock || b >= win_end {
                continue;
            }
            worst_on_grid = worst_on_grid.max(dist0(recs[b].est.phase));
            let mid = b + (rp / 2.0).round() as usize;
            if mid < recs.len() {
                best_off_grid = best_off_grid.min(dist0(recs[mid].est.phase));
            }
            n += 1;
        }
        assert!(n >= 2, "not enough post-relock beats to judge phase ({n})");

        println!(
            "beat::warm_relock_hard_aligns_phase: relock_conf {relock_conf:.3}, \
             worst_on_grid {worst_on_grid:.3}, best_off_grid {best_off_grid:.3} (n {n})"
        );

        assert!(
            worst_on_grid <= 0.12,
            "phase did not hard-snap to the resumed grid (worst dist {worst_on_grid:.3}) — \
             a stale phase would not track the shifted grid"
        );
        assert!(
            best_off_grid >= 0.35,
            "phase sat near the *old* grid ({best_off_grid:.3}) — the re-lock carried the stale \
             phase instead of aligning hard"
        );
    }

    fn hops(seconds: f32) -> usize {
        (seconds / dt()) as usize
    }

    /// Count phase wraps (a forward drop of more than half a turn) across a slice.
    fn count_wraps(recs: &[Rec]) -> usize {
        recs.windows(2)
            .filter(|w| w[1].est.phase < w[0].est.phase - 0.3)
            .count()
    }

    /// A weak-but-not-silent gap mid-run keeps the tempo published and the phase
    /// advancing through the gap, exercises the coasting state, and re-locks
    /// cleanly afterward.
    #[test]
    fn coasts_through_weak_gap_and_relocks() {
        let bpm = 124.0;
        let lock = hops(8.0);
        let gap_len = hops(4.0);
        let resume = hops(8.0);
        let total = lock + gap_len + resume;
        let gap = lock..(lock + gap_len);
        // A weak noise floor: well above SILENCE_LEVEL (so the fast silence cut
        // never fires) but far below the unit beat impulses, so the induction
        // windows lose their periodicity and confidence honestly decays below
        // LOCK_OFF — the tracker enters the coasting state rather than unlocking.
        let recs = run_pulse_gap(bpm, total, gap.clone(), 0.30);

        let gap_recs = &recs[gap.clone()];
        let published_all_gap = gap_recs.iter().all(|r| r.est.tempo_bpm > 0.0);
        let coasted = gap_recs.iter().any(|r| r.coasting);
        let gap_conf_min = gap_recs
            .iter()
            .map(|r| r.est.confidence)
            .fold(f32::MAX, f32::min);
        let gap_wraps = count_wraps(gap_recs);
        let expected_gap_wraps = 4.0 * bpm / 60.0;

        // Re-lock tail: the last second.
        let tail = &recs[total - hops(1.0)..];
        let tail_tempo = tail.iter().map(|r| r.est.tempo_bpm).sum::<f32>() / tail.len() as f32;
        let tail_locked = tail.iter().filter(|r| r.locked).count() as f32 / tail.len() as f32;

        println!(
            "beat::coasts_through_weak_gap_and_relocks: published_all_gap {published_all_gap}, \
             coasted {coasted}, gap_conf_min {gap_conf_min:.3}, gap_wraps {gap_wraps} \
             (expected ~{expected_gap_wraps:.1}), tail_tempo {tail_tempo:.2}, \
             tail_locked_frac {tail_locked:.2}"
        );

        assert!(
            published_all_gap,
            "tempo dropped to 0 somewhere in the weak gap"
        );
        assert!(
            coasted,
            "confidence never fell far enough to enter the coasting state"
        );
        assert!(
            gap_conf_min < LOCK_OFF,
            "confidence {gap_conf_min:.3} never decayed below LOCK_OFF in the gap"
        );
        assert!(
            (gap_wraps as f32) >= 0.6 * expected_gap_wraps
                && (gap_wraps as f32) <= 1.4 * expected_gap_wraps,
            "phase wrapped {gap_wraps} times in the gap, expected ~{expected_gap_wraps:.1}"
        );
        assert!(
            (tail_tempo - bpm).abs() <= 3.0,
            "re-lock tempo {tail_tempo:.2} not within ±3 of {bpm}"
        );
        assert!(
            tail_locked > 0.9,
            "tracker did not firmly re-lock after the gap (locked frac {tail_locked:.2})"
        );
    }

    /// True silence cuts a coast short: the tempo unlocks within about 1.5 s of
    /// the signal going silent, far sooner than the full coast window.
    #[test]
    fn true_silence_cuts_coast_short() {
        let bpm = 124.0;
        let lock = hops(8.0);
        let silence_len = hops(8.0);
        let total = lock + silence_len;
        let recs = run_pulse_gap(bpm, total, lock..total, 0.0);

        // First fully-unlocked hop at or after silence onset.
        let unlock_at = (lock..total).find(|&i| recs[i].est.tempo_bpm == 0.0);
        let unlock_at = unlock_at.expect("silence never unlocked the tracker");
        let delay_s = (unlock_at - lock) as f32 * dt();

        // It should still have coasted briefly (not unlocked on the first silent
        // hop) but well before the full COAST_SECONDS window.
        println!(
            "beat::true_silence_cuts_coast_short: unlocked {delay_s:.2}s after silence onset \
             (coast window {COAST_SECONDS:.1}s)"
        );
        assert!(
            delay_s <= 1.6,
            "silence took {delay_s:.2}s to unlock (expected ~1.5s)"
        );
        assert!(
            delay_s < COAST_SECONDS - 1.0,
            "silence unlock {delay_s:.2}s did not beat the full coast window"
        );
        // Once unlocked, it stays unlocked through the remaining silence.
        assert!(
            recs[unlock_at..].iter().all(|r| r.est.tempo_bpm == 0.0),
            "tempo came back after unlocking on silence"
        );
    }

    /// The phase carries continuously across a coast: no single-hop jump beyond a
    /// small tolerance while a tempo is published (the natural once-per-beat wrap
    /// aside), so the beat grid does not visibly hitch at re-lock.
    #[test]
    fn phase_continuous_across_coast() {
        let bpm = 124.0;
        let lock = hops(8.0);
        let gap_len = hops(4.0);
        let resume = hops(8.0);
        let total = lock + gap_len + resume;
        let gap = lock..(lock + gap_len);
        let recs = run_pulse_gap(bpm, total, gap, 0.30);

        let period_hops = 60.0 / (bpm * dt());
        let step = 1.0 / period_hops; // ~0.011 turn per hop
        // A jump far larger than one hop's advance but well short of a hard
        // re-align to an arbitrary grid position.
        let tol = 0.12f32;

        let mut worst = 0.0f32;
        let mut worst_at = 0usize;
        // Only inspect the stretch from the first lock onward, where a tempo is
        // continuously published (locked or coasting).
        let start = recs
            .iter()
            .position(|r| r.est.tempo_bpm > 0.0)
            .expect("never locked");
        for i in (start + 1)..total {
            if recs[i].est.tempo_bpm == 0.0 || recs[i - 1].est.tempo_bpm == 0.0 {
                continue;
            }
            // Forward distance on the phase circle; a clean advance is ~step, a
            // wrap reads as ~step too, only a real discontinuity is large.
            let d = (recs[i].est.phase - recs[i - 1].est.phase).rem_euclid(1.0);
            let jump = (d - step).abs().min((d - step + 1.0).abs());
            if jump > worst {
                worst = jump;
                worst_at = i;
            }
        }
        println!(
            "beat::phase_continuous_across_coast: worst per-hop phase jump {worst:.4} turn \
             at hop {worst_at} (step {step:.4}, tol {tol})"
        );
        assert!(
            worst <= tol,
            "phase jumped {worst:.4} turn at hop {worst_at} (> tol {tol}) — the coast/re-lock \
             hitched the grid"
        );
    }
}
