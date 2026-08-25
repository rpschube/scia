//! Photodiode-free end-to-end latency measurement (probe P7).
//!
//! The pieces here turn two streams of monotonic timestamps — when a click was
//! *emitted* into the system output and when the pipeline *published* the
//! feature snapshot that carries it — into a latency distribution, without any
//! external instrument. Both ends are stamped on the same clock: the ring epoch
//! that [`FeatureSnapshot::timestamp_ns`] already uses (see
//! [`crate::Engine::epoch`] / [`crate::Engine::now_ns`]), so subtracting them is
//! meaningful.
//!
//! The module is pure logic with no UI and no audio dependency:
//!
//! - [`EmitLog`] — a wait-free, allocation-free record of click emissions
//!   written from a producer (an output callback, or the synthetic generator)
//!   and drained by the observer.
//! - [`ClickDetector`] — an edge-triggered detector that turns the feature
//!   stream's `peak` into one [`Detection`] per click.
//! - [`Matcher`] — pairs emissions with detections in order, reporting misses
//!   and spurious detections.
//! - [`LatencyStats`] / [`Percentiles`] — the nearest-rank summary and its
//!   [`std::fmt::Display`] table.

use std::fmt;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::features::FeatureSnapshot;

/// Fixed number of emission slots. The log is drained once at the end of a run,
/// so this is also the maximum number of clicks a single run can record; beyond
/// it the oldest emissions are overwritten. Comfortably above any probe run
/// (the default is 25 clicks).
const EMIT_LOG_SLOTS: usize = 4096;

/// One recorded click emission: which click it was, when it was emitted, and —
/// on a real output stream — the measured callback-to-playback delay.
///
/// All times are monotonic nanoseconds since the ring epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Emission {
    /// Running click index, starting at 0.
    pub index: u32,
    /// When the click was emitted (ns since the ring epoch).
    pub emit_ns: u64,
    /// Output callback-to-playback delay for this click (ns); `0` when unknown
    /// (e.g. the synthetic backend, or a host that reports no timestamp).
    pub output_delay_ns: u64,
}

/// One atomic emission slot. Split into fields so the whole record can be
/// written and read without a lock and without `unsafe`.
#[derive(Default)]
struct EmitSlot {
    index: AtomicU32,
    emit_ns: AtomicU64,
    output_delay_ns: AtomicU64,
}

/// A wait-free, single-producer / single-consumer log of [`Emission`]s.
///
/// The producer (a real-time output/capture callback or the synthetic
/// generator) calls [`push`](EmitLog::push) with no allocation and no lock; the
/// consumer (the observer, off the hot path) calls [`drain`](EmitLog::drain).
/// Share it as an `Arc<EmitLog>`: one clone in the producer, one in the
/// consumer.
pub struct EmitLog {
    slots: Box<[EmitSlot]>,
    /// Total emissions ever pushed; the producer owns writes, the consumer
    /// reads it (Acquire) to learn how far to drain.
    write: AtomicU64,
    /// Total emissions ever drained; the consumer owns it.
    read: AtomicU64,
}

impl Default for EmitLog {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for EmitLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmitLog")
            .field("capacity", &self.slots.len())
            .field("pushed", &self.write.load(Ordering::Relaxed))
            .field("drained", &self.read.load(Ordering::Relaxed))
            .finish()
    }
}

impl EmitLog {
    /// Allocate the fixed emission ring.
    #[must_use]
    pub fn new() -> Self {
        let mut slots = Vec::with_capacity(EMIT_LOG_SLOTS);
        slots.resize_with(EMIT_LOG_SLOTS, EmitSlot::default);
        Self {
            slots: slots.into_boxed_slice(),
            write: AtomicU64::new(0),
            read: AtomicU64::new(0),
        }
    }

    /// Record one emission. Wait-free and allocation-free: three relaxed atomic
    /// stores plus one release store. Safe to call from a real-time callback.
    /// Single producer only.
    pub fn push(&self, emission: Emission) {
        // Sole producer: the relaxed load sees our own last write.
        let w = self.write.load(Ordering::Relaxed);
        let slot = &self.slots[(w as usize) % EMIT_LOG_SLOTS];
        slot.index.store(emission.index, Ordering::Relaxed);
        slot.emit_ns.store(emission.emit_ns, Ordering::Relaxed);
        slot.output_delay_ns
            .store(emission.output_delay_ns, Ordering::Relaxed);
        // Release so a consumer that reads `write` with Acquire sees the field
        // stores above.
        self.write.store(w.wrapping_add(1), Ordering::Release);
    }

    /// Drain every emission pushed since the last drain into `out`, in emission
    /// order. Single consumer only.
    pub fn drain(&self, out: &mut Vec<Emission>) {
        let w = self.write.load(Ordering::Acquire);
        let mut r = self.read.load(Ordering::Relaxed);
        while r < w {
            let slot = &self.slots[(r as usize) % EMIT_LOG_SLOTS];
            out.push(Emission {
                index: slot.index.load(Ordering::Relaxed),
                emit_ns: slot.emit_ns.load(Ordering::Relaxed),
                output_delay_ns: slot.output_delay_ns.load(Ordering::Relaxed),
            });
            r += 1;
        }
        self.read.store(r, Ordering::Relaxed);
    }
}

/// One detected click: the hop generation it fired on, when that hop was
/// published (its `timestamp_ns`), and when the observer saw it.
///
/// All times are monotonic nanoseconds since the ring epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Detection {
    /// The `generation` of the snapshot the detection fired on.
    pub generation: u64,
    /// When that snapshot was published: its `timestamp_ns` (ns since epoch).
    pub publish_ns: u64,
    /// When the observer read and detected it (ns since epoch).
    pub observe_ns: u64,
}

/// Edge-triggered click detector over the feature stream's `peak`.
///
/// It fires once per click: on the hop where `peak` first rises to or above
/// `threshold`, provided the previously observed hop was below it, at least
/// `refractory_ns` have passed since the last detection, and the snapshot's
/// `generation` advanced since the last call (so one snapshot is never counted
/// twice, no matter how often the observer polls).
pub struct ClickDetector {
    threshold: f32,
    refractory_ns: u64,
    started: bool,
    last_generation: u64,
    prev_above: bool,
    last_detection_ns: Option<u64>,
}

impl ClickDetector {
    /// A detector firing at `threshold` (peak, `0.0..=1.0`) with the given
    /// refractory gap in nanoseconds.
    #[must_use]
    pub fn new(threshold: f32, refractory_ns: u64) -> Self {
        Self {
            threshold,
            refractory_ns,
            started: false,
            last_generation: 0,
            prev_above: false,
            last_detection_ns: None,
        }
    }

    /// Observe one snapshot, read at `observe_ns` (ns since the ring epoch).
    /// Returns a [`Detection`] on a rising edge that clears the refractory gap,
    /// otherwise `None`. A snapshot whose `generation` has not advanced since
    /// the last call is ignored entirely.
    pub fn observe(&mut self, snapshot: &FeatureSnapshot, observe_ns: u64) -> Option<Detection> {
        if self.started && snapshot.generation == self.last_generation {
            return None;
        }
        self.started = true;
        self.last_generation = snapshot.generation;

        let above = snapshot.peak >= self.threshold;
        let rising = above && !self.prev_above;
        self.prev_above = above;

        if !rising {
            return None;
        }
        if let Some(last) = self.last_detection_ns {
            if observe_ns.saturating_sub(last) < self.refractory_ns {
                return None;
            }
        }
        self.last_detection_ns = Some(observe_ns);
        Some(Detection {
            generation: snapshot.generation,
            publish_ns: snapshot.timestamp_ns,
            observe_ns,
        })
    }
}

/// One matched click: an emission paired with the detection it produced.
///
/// All times are monotonic nanoseconds since the ring epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// The emission's click index.
    pub index: u32,
    /// When the click was emitted.
    pub emit_ns: u64,
    /// When the carrying hop was published (`timestamp_ns`).
    pub publish_ns: u64,
    /// When the observer detected it.
    pub observe_ns: u64,
    /// The emission's measured output delay (`0` when unknown).
    pub output_delay_ns: u64,
}

/// The result of pairing emissions with detections.
#[derive(Clone, Debug, Default)]
pub struct Matched {
    /// Paired emissions and detections, in emission order.
    pub samples: Vec<Sample>,
    /// Emissions that never matched a detection (clicks that were emitted but
    /// never observed).
    pub missed: u32,
    /// Detections that never matched an emission (observed peaks with no
    /// corresponding click).
    pub spurious: u32,
}

/// Pairs emissions with detections within a time window.
pub struct Matcher {
    window_ns: u64,
}

impl Matcher {
    /// A matcher pairing a detection with an emission at most `window_ns`
    /// earlier.
    #[must_use]
    pub fn new(window_ns: u64) -> Self {
        Self { window_ns }
    }

    /// Pair `emissions` with `detections`. Each detection, in observe order,
    /// takes the latest still-unmatched emission whose `emit_ns <= observe_ns`
    /// and `observe_ns - emit_ns <= window_ns`. Emissions left over are
    /// [`Matched::missed`]; detections left over are [`Matched::spurious`].
    #[must_use]
    pub fn match_events(&self, emissions: &[Emission], detections: &[Detection]) -> Matched {
        let mut emis = emissions.to_vec();
        emis.sort_by_key(|e| e.emit_ns);
        let mut dets = detections.to_vec();
        dets.sort_by_key(|d| d.observe_ns);

        let mut used = vec![false; emis.len()];
        let mut samples = Vec::with_capacity(dets.len().min(emis.len()));
        let mut spurious = 0u32;

        for d in &dets {
            let mut best: Option<usize> = None;
            for (i, e) in emis.iter().enumerate() {
                if e.emit_ns > d.observe_ns {
                    // Sorted ascending: nothing later can qualify either.
                    break;
                }
                if used[i] {
                    continue;
                }
                if d.observe_ns - e.emit_ns <= self.window_ns {
                    // Keep scanning: we want the latest qualifying emission.
                    best = Some(i);
                }
            }
            match best {
                Some(i) => {
                    used[i] = true;
                    samples.push(Sample {
                        index: emis[i].index,
                        emit_ns: emis[i].emit_ns,
                        publish_ns: d.publish_ns,
                        observe_ns: d.observe_ns,
                        output_delay_ns: emis[i].output_delay_ns,
                    });
                }
                None => spurious += 1,
            }
        }

        let missed = used.iter().filter(|matched| !**matched).count() as u32;
        samples.sort_by_key(|s| s.emit_ns);
        Matched {
            samples,
            missed,
            spurious,
        }
    }
}

/// Nearest-rank summary of a latency interval, in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Percentiles {
    /// Smallest value (ms).
    pub min: f32,
    /// Median (50th percentile, nearest-rank) (ms).
    pub median: f32,
    /// 95th percentile (nearest-rank) (ms).
    pub p95: f32,
    /// Largest value (ms).
    pub max: f32,
}

impl Percentiles {
    /// Nearest-rank percentiles of `values` (in ms). An empty input yields all
    /// zeros.
    #[must_use]
    pub fn nearest_rank(mut values: Vec<f32>) -> Self {
        if values.is_empty() {
            return Self::default();
        }
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = values.len();
        let at = |p: f32| -> f32 {
            // Nearest-rank: ceil(p/100 * n), 1-indexed, clamped to 1..=n.
            let rank = (p / 100.0 * n as f32).ceil() as usize;
            values[rank.clamp(1, n) - 1]
        };
        Self {
            min: values[0],
            median: at(50.0),
            p95: at(95.0),
            max: values[n - 1],
        }
    }
}

/// The full latency report: counts plus the three end-to-end intervals and the
/// measured output delay, each as [`Percentiles`] in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LatencyStats {
    /// Number of matched clicks.
    pub count: u32,
    /// Emitted clicks that were never observed.
    pub missed: u32,
    /// Observed peaks with no matching emission.
    pub spurious: u32,
    /// Emission → hop publish (`timestamp_ns`).
    pub emit_to_publish: Percentiles,
    /// Hop publish → observer read.
    pub publish_to_observe: Percentiles,
    /// Emission → observer read (the end-to-end audio→feature latency).
    pub emit_to_observe: Percentiles,
    /// Output callback → playback delay (`0` for the synthetic backend).
    pub output_delay: Percentiles,
}

impl LatencyStats {
    /// Compute the report from a [`Matched`] result.
    #[must_use]
    pub fn from_matched(matched: &Matched) -> Self {
        let ms = |ns: u64| ns as f32 / 1.0e6;
        let emit_to_publish = matched
            .samples
            .iter()
            .map(|s| ms(s.publish_ns.saturating_sub(s.emit_ns)))
            .collect();
        let publish_to_observe = matched
            .samples
            .iter()
            .map(|s| ms(s.observe_ns.saturating_sub(s.publish_ns)))
            .collect();
        let emit_to_observe = matched
            .samples
            .iter()
            .map(|s| ms(s.observe_ns.saturating_sub(s.emit_ns)))
            .collect();
        let output_delay = matched
            .samples
            .iter()
            .map(|s| ms(s.output_delay_ns))
            .collect();
        Self {
            count: matched.samples.len() as u32,
            missed: matched.missed,
            spurious: matched.spurious,
            emit_to_publish: Percentiles::nearest_rank(emit_to_publish),
            publish_to_observe: Percentiles::nearest_rank(publish_to_observe),
            emit_to_observe: Percentiles::nearest_rank(emit_to_observe),
            output_delay: Percentiles::nearest_rank(output_delay),
        }
    }
}

impl fmt::Display for LatencyStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{:<22} {:>7} {:>7} {:>7} {:>7}   (ms)",
            "", "min", "median", "p95", "max"
        )?;
        let row = |f: &mut fmt::Formatter<'_>,
                   label: &str,
                   p: &Percentiles,
                   suffix: &str|
         -> fmt::Result {
            writeln!(
                f,
                "{:<22} {:>7.2} {:>7.2} {:>7.2} {:>7.2}{}",
                label, p.min, p.median, p.p95, p.max, suffix
            )
        };
        row(f, "emit → publish", &self.emit_to_publish, "")?;
        row(f, "publish → observe", &self.publish_to_observe, "")?;
        row(f, "emit → observe", &self.emit_to_observe, "")?;
        row(
            f,
            "output delay (cb→play)",
            &self.output_delay,
            "   (live only)",
        )
    }
}

/// One click measured **both** ways in a single dual-tap run: from one emission,
/// `emit → raw-arrival` (the click's samples entering scia's ring, found by
/// cross-correlation on the teed capture stream) and `emit → publish` (the hop
/// that carries it, detected off the feature stream). Both are milliseconds
/// relative to the same emission, on the same capture-delivery clock, from the
/// same running engine — which is what lets the subset invariant be checked
/// within one process (see [`DualTapStats`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DualTapSample {
    /// The click's emission index.
    pub index: u32,
    /// `emit → raw-arrival` in ms (capture transport into the ring).
    pub emit_to_raw_arrival_ms: f32,
    /// `emit → publish` in ms (through the hop that carries the click).
    pub emit_to_publish_ms: f32,
}

impl DualTapSample {
    /// The hop-gather term: `emit → publish` minus `emit → raw-arrival` — the
    /// latency between a sample entering the ring and the hop that gathers it
    /// being published. Physically in `0 ..= one hop`.
    #[must_use]
    pub fn hop_gather_ms(&self) -> f32 {
        self.emit_to_publish_ms - self.emit_to_raw_arrival_ms
    }

    /// Whether raw-arrival is a subset of publish for this click within `eps_ms`
    /// (`raw-arrival ≤ publish`). This is the **hard** invariant: a sample enters
    /// the ring strictly before any hop that carries it is published, so within one
    /// run this cannot fail unless a clock lies. A violation localizes a defect.
    #[must_use]
    pub fn subset_holds(&self, eps_ms: f32) -> bool {
        self.emit_to_raw_arrival_ms <= self.emit_to_publish_ms + eps_ms
    }

    /// Whether publish sits within one hop of raw-arrival (`publish ≤ raw-arrival +
    /// hop_ms`) inside `eps_ms`. Physically the gather adds at most one hop; on
    /// real hardware a detection landing a hop late (a partial hop's peak missing
    /// the threshold) can exceed it by up to another hop without any clock being
    /// wrong — read it together with [`subset_holds`](DualTapSample::subset_holds).
    #[must_use]
    pub fn within_one_hop(&self, hop_ms: f32, eps_ms: f32) -> bool {
        self.emit_to_publish_ms <= self.emit_to_raw_arrival_ms + hop_ms + eps_ms
    }
}

/// The dual-tap verdict over all clicks measured both ways in one run: the two
/// intervals and the hop-gather delta as [`Percentiles`], plus how many clicks
/// satisfy each half of the subset invariant
/// `raw-arrival ≤ publish ≤ raw-arrival + one hop`. Because both ends come from
/// one engine on one capture-delivery clock, the invariant holds by construction;
/// a break is a localizable defect, which is exactly the discriminator the P7
/// dual-tap probe exists to report.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DualTapStats {
    /// Clicks measured both ways.
    pub count: u32,
    /// `emit → raw-arrival` distribution (ms).
    pub raw_arrival: Percentiles,
    /// `emit → publish` distribution (ms).
    pub publish: Percentiles,
    /// `publish − raw-arrival` (the hop-gather term) distribution (ms).
    pub hop_gather: Percentiles,
    /// One hop in ms, the invariant's upper span.
    pub hop_ms: f32,
    /// Tolerance the invariant was checked within (ms).
    pub eps_ms: f32,
    /// Clicks satisfying `raw-arrival ≤ publish` (the hard subset invariant).
    pub subset_holds: u32,
    /// Clicks satisfying `publish ≤ raw-arrival + one hop`.
    pub within_one_hop: u32,
}

impl DualTapStats {
    /// Summarize `samples` against a `hop_ms` upper span, checking the invariant
    /// within `eps_ms`.
    #[must_use]
    pub fn from_samples(samples: &[DualTapSample], hop_ms: f32, eps_ms: f32) -> Self {
        let raw_arrival =
            Percentiles::nearest_rank(samples.iter().map(|s| s.emit_to_raw_arrival_ms).collect());
        let publish =
            Percentiles::nearest_rank(samples.iter().map(|s| s.emit_to_publish_ms).collect());
        let hop_gather =
            Percentiles::nearest_rank(samples.iter().map(DualTapSample::hop_gather_ms).collect());
        let subset_holds = samples.iter().filter(|s| s.subset_holds(eps_ms)).count() as u32;
        let within_one_hop = samples
            .iter()
            .filter(|s| s.within_one_hop(hop_ms, eps_ms))
            .count() as u32;
        Self {
            count: samples.len() as u32,
            raw_arrival,
            publish,
            hop_gather,
            hop_ms,
            eps_ms,
            subset_holds,
            within_one_hop,
        }
    }

    /// Whether the full subset invariant held for every measured click: at least
    /// one click, and all of them within both bounds.
    #[must_use]
    pub fn all_hold(&self) -> bool {
        self.count > 0 && self.subset_holds == self.count && self.within_one_hop == self.count
    }
}

impl fmt::Display for DualTapStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{:<22} {:>7} {:>7} {:>7} {:>7}   (ms)",
            "", "min", "median", "p95", "max"
        )?;
        let row = |f: &mut fmt::Formatter<'_>, label: &str, p: &Percentiles| -> fmt::Result {
            writeln!(
                f,
                "{label:<22} {:>7.2} {:>7.2} {:>7.2} {:>7.2}",
                p.min, p.median, p.p95, p.max
            )
        };
        row(f, "emit → raw-arrival", &self.raw_arrival)?;
        row(f, "emit → publish", &self.publish)?;
        row(f, "hop gather (Δ)", &self.hop_gather)?;
        write!(
            f,
            "invariant  raw-arrival ≤ publish ≤ raw-arrival + one hop ({:.2} ms): \
             subset {}/{} · within-one-hop {}/{}",
            self.hop_ms, self.subset_holds, self.count, self.within_one_hop, self.count
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(generation: u64, peak: f32, timestamp_ns: u64) -> FeatureSnapshot {
        FeatureSnapshot {
            generation,
            peak,
            timestamp_ns,
            ..FeatureSnapshot::default()
        }
    }

    #[test]
    fn emit_log_push_drain_roundtrip() {
        let log = EmitLog::new();
        log.push(Emission {
            index: 0,
            emit_ns: 10,
            output_delay_ns: 1,
        });
        log.push(Emission {
            index: 1,
            emit_ns: 20,
            output_delay_ns: 2,
        });
        let mut out = Vec::new();
        log.drain(&mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].index, 0);
        assert_eq!(out[1].emit_ns, 20);
        // A second drain with nothing new yields nothing.
        log.drain(&mut out);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn detector_edge_trigger_and_same_generation_suppression() {
        let mut det = ClickDetector::new(0.3, 0);
        // Below threshold: no detection.
        assert!(det.observe(&snap(1, 0.05, 1_000), 1_500).is_none());
        // Rising above threshold: fires, carrying publish/observe times.
        let d = det
            .observe(&snap(2, 0.8, 2_000), 2_400)
            .expect("rising edge fires");
        assert_eq!(d.generation, 2);
        assert_eq!(d.publish_ns, 2_000);
        assert_eq!(d.observe_ns, 2_400);
        // Re-observing the SAME generation (a repeated poll) never re-detects.
        assert!(det.observe(&snap(2, 0.8, 2_000), 2_450).is_none());
        // Still above on the next hop: not a rising edge, no detection.
        assert!(det.observe(&snap(3, 0.7, 3_000), 3_400).is_none());
        // Drop below, then rise again: a fresh edge fires.
        assert!(det.observe(&snap(4, 0.01, 4_000), 4_400).is_none());
        assert!(det.observe(&snap(5, 0.9, 5_000), 5_400).is_some());
    }

    #[test]
    fn detector_refractory_suppresses_a_close_second_edge() {
        // Refractory gap of 1000 ns.
        let mut det = ClickDetector::new(0.3, 1_000);
        assert!(det.observe(&snap(1, 0.0, 0), 0).is_none());
        assert!(det.observe(&snap(2, 0.8, 100), 200).is_some());
        // Fall and rise again only 300 ns after the detection: refractory blocks it.
        assert!(det.observe(&snap(3, 0.0, 300), 300).is_none());
        assert!(det.observe(&snap(4, 0.8, 400), 400).is_none());
        // Fall, then rise well past the refractory gap: fires.
        assert!(det.observe(&snap(5, 0.0, 1_400), 1_400).is_none());
        assert!(det.observe(&snap(6, 0.8, 1_500), 1_500).is_some());
    }

    #[test]
    fn matcher_reports_one_missed_and_one_spurious() {
        let matcher = Matcher::new(100);
        // Emissions at 0, 50, 200. Detections near 0 and 50 match; the 200
        // emission is missed; a detection at 1000 with no emission in window is
        // spurious.
        let emissions = vec![
            Emission {
                index: 0,
                emit_ns: 0,
                output_delay_ns: 5,
            },
            Emission {
                index: 1,
                emit_ns: 50,
                output_delay_ns: 6,
            },
            Emission {
                index: 2,
                emit_ns: 200,
                output_delay_ns: 7,
            },
        ];
        let detections = vec![
            Detection {
                generation: 1,
                publish_ns: 20,
                observe_ns: 30,
            },
            Detection {
                generation: 2,
                publish_ns: 70,
                observe_ns: 80,
            },
            Detection {
                generation: 9,
                publish_ns: 990,
                observe_ns: 1_000,
            },
        ];
        let m = matcher.match_events(&emissions, &detections);
        assert_eq!(m.samples.len(), 2);
        assert_eq!(m.missed, 1);
        assert_eq!(m.spurious, 1);
        assert_eq!(m.samples[0].index, 0);
        assert_eq!(m.samples[1].index, 1);
        assert_eq!(m.samples[1].output_delay_ns, 6);
    }

    #[test]
    fn percentiles_nearest_rank_on_a_known_vector() {
        // 1..=10 ms. Nearest-rank: median = ceil(0.5*10)=5th = 5.0;
        // p95 = ceil(0.95*10)=10th = 10.0.
        let p = Percentiles::nearest_rank((1..=10).map(|v| v as f32).collect());
        assert_eq!(p.min, 1.0);
        assert_eq!(p.median, 5.0);
        assert_eq!(p.p95, 10.0);
        assert_eq!(p.max, 10.0);
    }

    #[test]
    fn latency_stats_from_matched_computes_intervals() {
        let matched = Matched {
            samples: vec![Sample {
                index: 0,
                emit_ns: 1_000_000,         // 1 ms
                publish_ns: 7_000_000,      // 7 ms  -> emit→publish 6 ms
                observe_ns: 8_000_000,      // 8 ms  -> publish→observe 1 ms, emit→observe 7 ms
                output_delay_ns: 2_000_000, // 2 ms
            }],
            missed: 0,
            spurious: 0,
        };
        let stats = LatencyStats::from_matched(&matched);
        assert_eq!(stats.count, 1);
        assert!((stats.emit_to_publish.median - 6.0).abs() < 1e-4);
        assert!((stats.publish_to_observe.median - 1.0).abs() < 1e-4);
        assert!((stats.emit_to_observe.median - 7.0).abs() < 1e-4);
        assert!((stats.output_delay.median - 2.0).abs() < 1e-4);
        // Display renders four data rows plus a header.
        let rendered = stats.to_string();
        assert_eq!(rendered.lines().count(), 5);
        assert!(rendered.contains("emit → observe"));
    }

    #[test]
    fn dual_tap_stats_scores_the_subset_invariant() {
        let hop_ms = 5.33f32;
        let eps = 0.2f32;
        let samples = vec![
            // Well inside the sandwich: raw < publish < raw + hop.
            DualTapSample {
                index: 0,
                emit_to_raw_arrival_ms: 100.0,
                emit_to_publish_ms: 103.0,
            },
            // Raw == publish (gather ~0): still holds.
            DualTapSample {
                index: 1,
                emit_to_raw_arrival_ms: 100.0,
                emit_to_publish_ms: 100.0,
            },
        ];
        let stats = DualTapStats::from_samples(&samples, hop_ms, eps);
        assert_eq!(stats.count, 2);
        assert_eq!(stats.subset_holds, 2);
        assert_eq!(stats.within_one_hop, 2);
        assert!(stats.all_hold());
        // Hop-gather values are {3.0, 0.0}: min 0, max 3.
        assert!((stats.hop_gather.min - 0.0).abs() < 1e-3);
        assert!((stats.hop_gather.max - 3.0).abs() < 1e-3);

        // A run where raw-arrival sits ABOVE publish (the impossible inversion the
        // probe hunts) breaks the subset half and fails `all_hold`.
        let broken = vec![DualTapSample {
            index: 0,
            emit_to_raw_arrival_ms: 108.7,
            emit_to_publish_ms: 79.3,
        }];
        let stats = DualTapStats::from_samples(&broken, hop_ms, eps);
        assert_eq!(stats.subset_holds, 0);
        assert!(!stats.all_hold());
        assert!(stats.to_string().contains("subset 0/1"));
    }
}
