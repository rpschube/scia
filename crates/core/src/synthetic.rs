//! A synthetic capture backend so the whole pipeline is testable with no audio
//! hardware. It runs a producer thread that fills 256-frame chunks from a
//! [`Signal`] and pushes them through the [`SampleSink`], paced either in real
//! time or as fast as the ring accepts.

use std::f64::consts::PI;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::capture::{
    CaptureBackend, CaptureError, CaptureStream, CaptureTarget, SampleSink, StreamFormat,
};
use crate::latency::{Emission, EmitLog};

/// Frames per generated chunk — matches the DSP hop size.
const CHUNK_FRAMES: usize = 256;

/// The waveform a [`SyntheticBackend`] produces.
#[derive(Clone, Copy, Debug)]
pub enum Signal {
    /// Constant zero.
    Silence,
    /// A sine at `hz`, amplitude `amp` (peak).
    Sine {
        /// Frequency in Hz.
        hz: f32,
        /// Peak amplitude.
        amp: f32,
    },
    /// One-frame impulses of amplitude `amp` on a `bpm` beat grid, silence
    /// between.
    Clicks {
        /// Tempo in beats per minute.
        bpm: f32,
        /// Impulse amplitude.
        amp: f32,
    },
    /// A musically plausible mix on a `bpm` beat grid: a kick on every beat, a
    /// hi-hat pattern on the off-eighths, a bassline that changes each bar, a
    /// slowly breathing pad, and a periodic treble sparkle, arranged in an
    /// eight-bar section cycle (a breakdown bar, then a fill). Composed
    /// per-sample and soft-clipped to ±0.9, with a nonzero stereo width. Every
    /// sample is a pure function of the frame index, so it is deterministic and
    /// the fill stays allocation-free; it is designed to exercise the whole DSP
    /// pipeline — onsets, bands and spectrum — exactly like live audio.
    Music {
        /// Tempo in beats per minute.
        bpm: f32,
    },
}

/// How fast a [`SyntheticBackend`] delivers samples.
#[derive(Clone, Copy, Debug)]
pub enum Pacing {
    /// Push 256-frame chunks and sleep to track real time (jitter tolerated).
    Realtime,
    /// Push chunks as fast as the ring accepts (waiting briefly when full,
    /// never busy-spinning) until `total_frames` are delivered, then stop
    /// delivering anything more. Used to exercise starvation.
    Unpaced {
        /// Total frames to deliver before the source goes quiet.
        total_frames: u64,
    },
}

/// A hardware-free capture backend producing stereo `f32` audio.
///
/// Carrying an [`EmitLog`] (via [`emit_log`](SyntheticBackend::emit_log)) makes
/// it drop `Copy`; it stays `Clone`, and cloning shares the same log.
#[derive(Clone, Debug)]
pub struct SyntheticBackend {
    /// Stream format to report and generate at. Defaults to 48 kHz stereo.
    pub format: StreamFormat,
    /// Waveform to generate.
    pub signal: Signal,
    /// Delivery pacing.
    pub pacing: Pacing,
    /// Optional click-emission log for the latency probe. When set and
    /// [`signal`](SyntheticBackend::signal) is [`Signal::Clicks`], each
    /// generated click is recorded with `emit_ns` sampled immediately before
    /// the containing chunk is pushed. `None` for every other use.
    pub emit_log: Option<Arc<EmitLog>>,
}

impl Default for SyntheticBackend {
    fn default() -> Self {
        Self {
            format: StreamFormat {
                sample_rate: 48_000,
                channels: 2,
            },
            signal: Signal::Silence,
            pacing: Pacing::Realtime,
            emit_log: None,
        }
    }
}

impl CaptureBackend for SyntheticBackend {
    fn open(
        &mut self,
        _target: CaptureTarget,
        mut sink: SampleSink,
    ) -> Result<Box<dyn CaptureStream>, CaptureError> {
        let format = self.format;
        if format.sample_rate == 0 {
            return Err(CaptureError::Unsupported(
                "sample rate must be non-zero".into(),
            ));
        }
        if format.channels == 0 || format.channels > 2 {
            return Err(CaptureError::Unsupported(format!(
                "channel count {} not in 1..=2",
                format.channels
            )));
        }

        let signal = self.signal;
        let pacing = self.pacing;
        let emit_log = self.emit_log.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);

        let handle = thread::Builder::new()
            .name("scia-synth".into())
            .spawn(move || {
                generate(
                    format,
                    signal,
                    pacing,
                    emit_log.as_deref(),
                    &thread_stop,
                    &mut sink,
                );
            })
            .map_err(|e| CaptureError::Backend(e.to_string()))?;

        Ok(Box::new(SyntheticStream {
            format,
            stop,
            handle: Some(handle),
        }))
    }
}

/// The producer thread: fill chunks and deliver them per the pacing policy.
///
/// When `emit_log` is set and the signal is [`Signal::Clicks`], each click in a
/// chunk is recorded — `emit_ns` sampled from `sink.stats().now_ns()`
/// immediately before that chunk is pushed, which is the same clock the DSP
/// thread stamps `FeatureSnapshot::timestamp_ns` with (`dsp::run` reads
/// `thread.stats.now_ns()`), so the two ends share one epoch. No allocation on
/// that path.
fn generate(
    format: StreamFormat,
    signal: Signal,
    pacing: Pacing,
    emit_log: Option<&EmitLog>,
    stop: &AtomicBool,
    sink: &mut SampleSink,
) {
    let channels = format.channels as usize;
    let sample_rate = f64::from(format.sample_rate);
    let mut buffer = vec![0.0f32; CHUNK_FRAMES * channels];

    // Click period in frames, for emission logging (Clicks only).
    let click_period = match signal {
        Signal::Clicks { bpm, .. } if bpm > 0.0 => (60.0 / f64::from(bpm) * sample_rate).round(),
        _ => 0.0,
    } as u64;

    let start = Instant::now();
    let mut frame_index: u64 = 0;
    let mut chunks_pushed: u64 = 0;
    let mut click_count: u32 = 0;

    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }

        let frames = match pacing {
            Pacing::Realtime => CHUNK_FRAMES,
            Pacing::Unpaced { total_frames } => {
                if frame_index >= total_frames {
                    break;
                }
                ((total_frames - frame_index).min(CHUNK_FRAMES as u64)) as usize
            }
        };

        fill(
            &mut buffer,
            frames,
            channels,
            signal,
            sample_rate,
            frame_index,
        );
        let slice = &buffer[..frames * channels];

        match pacing {
            Pacing::Realtime => {
                // Record every click in this chunk with a single clock read
                // taken immediately before the push (allocation-free).
                if let Some(log) = emit_log {
                    if click_period > 0 {
                        let emit_ns = sink.stats().now_ns();
                        for frame in 0..frames as u64 {
                            if (frame_index + frame) % click_period == 0 {
                                log.push(Emission {
                                    index: click_count,
                                    emit_ns,
                                    output_delay_ns: 0,
                                });
                                click_count += 1;
                            }
                        }
                    }
                }
                // The ring drops excess if the consumer is slow; that is fine
                // for a real-time source.
                sink.push(slice);
                frame_index += frames as u64;
                chunks_pushed += 1;
                let target = start
                    + Duration::from_secs_f64(
                        CHUNK_FRAMES as f64 * chunks_pushed as f64 / sample_rate,
                    );
                let now = Instant::now();
                if target > now {
                    thread::sleep(target - now);
                }
            }
            Pacing::Unpaced { .. } => {
                // Deliver everything without dropping: wait for room, then push.
                while !stop.load(Ordering::Acquire) && sink.free_samples() < slice.len() {
                    thread::sleep(Duration::from_millis(1));
                }
                if stop.load(Ordering::Acquire) {
                    break;
                }
                sink.push(slice);
                frame_index += frames as u64;
            }
        }
    }
}

/// Fill `frames` frames of `channels`-wide interleaved audio into `buffer`.
fn fill(
    buffer: &mut [f32],
    frames: usize,
    channels: usize,
    signal: Signal,
    sample_rate: f64,
    frame_index: u64,
) {
    match signal {
        Signal::Silence => {
            for value in &mut buffer[..frames * channels] {
                *value = 0.0;
            }
        }
        Signal::Sine { hz, amp } => {
            for frame in 0..frames {
                let t = (frame_index + frame as u64) as f64 / sample_rate;
                let sample = (f64::from(amp) * (2.0 * PI * f64::from(hz) * t).sin()) as f32;
                let base = frame * channels;
                for ch in 0..channels {
                    buffer[base + ch] = sample;
                }
            }
        }
        Signal::Clicks { bpm, amp } => {
            let period = if bpm > 0.0 {
                (60.0 / f64::from(bpm) * sample_rate).round() as u64
            } else {
                0
            };
            for frame in 0..frames {
                let index = frame_index + frame as u64;
                let sample = if period > 0 && index % period == 0 {
                    amp
                } else {
                    0.0
                };
                let base = frame * channels;
                for ch in 0..channels {
                    buffer[base + ch] = sample;
                }
            }
        }
        Signal::Music { bpm } => {
            // Samples per beat; a non-positive tempo degenerates to silence
            // rather than dividing by zero.
            let beat = if bpm > 0.0 {
                60.0 / f64::from(bpm) * sample_rate
            } else {
                0.0
            };
            for frame in 0..frames {
                let index = frame_index + frame as u64;
                let (left, right) = if beat > 0.0 {
                    music_sample(index, sample_rate, beat)
                } else {
                    (0.0, 0.0)
                };
                let base = frame * channels;
                if channels == 1 {
                    buffer[base] = 0.5 * (left + right);
                } else {
                    buffer[base] = left;
                    buffer[base + 1] = right;
                    // The backend only ever opens 1- or 2-channel streams, but
                    // stay safe if fed a wider layout: duplicate the left.
                    for ch in 2..channels {
                        buffer[base + ch] = left;
                    }
                }
            }
        }
    }
}

/// Deterministic per-index pseudo-random value in roughly `-1.0..=1.0`, from a
/// splitmix64 finalizer over the sample index. Stateless, so the hat noise it
/// feeds stays a pure function of the frame index (no RNG entropy).
fn noise(index: u64) -> f32 {
    let mut z = index.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // Top 24 bits → [0, 2) → [-1, 1).
    ((z >> 40) as f32 / (1u64 << 23) as f32) - 1.0
}

/// One stereo sample of [`Signal::Music`] at absolute `index`, given the sample
/// rate and the beat length in samples. A pure function of its arguments — all
/// timing is recomputed from `index`, so the fill carries no state and is
/// deterministic. Composes kick, bass, pad, hats and sparkle, pans them for a
/// nonzero stereo width, and soft-clips each channel to ±0.9.
fn music_sample(index: u64, sample_rate: f64, beat: f64) -> (f32, f32) {
    let t = index as f64; // absolute sample position
    let ts = t / sample_rate; // absolute seconds (pad/LFO/sparkle phase)
    let bar = beat * 4.0; // four beats to the bar

    let beat_num = (t / beat).floor();
    let bar_num = (t / bar).floor();
    // Eight-bar section cycle: bars 0..=5 full, bar 6 a kick+bass breakdown,
    // bar 7 a doubled-hat fill re-entry.
    let section_bar = (bar_num as i64).rem_euclid(8);
    let kick_on = section_bar != 6;
    let bass_on = section_bar != 6;
    let double_hats = section_bar == 7;

    // Center bus: kick, bass and pad share the middle of the image.
    let mut center = 0.0f64;

    // ---- Kick: every beat, a pitch-dropping 110→55 Hz sine, ~120 ms decay. ----
    if kick_on {
        let te = (t - beat_num * beat) / sample_rate; // seconds since the beat
        let f0 = 110.0;
        let f1 = 55.0;
        let ptau = 0.012; // ~40 ms to settle to the low pitch
        // Phase is the integral of the swept frequency, so there is no click.
        let phase = 2.0 * PI * (f1 * te + (f0 - f1) * ptau * (1.0 - (-te / ptau).exp()));
        let env = (-te / 0.04).exp();
        center += 0.8 * env * phase.sin();
    }

    // ---- Bassline: one note per bar (A1/E2/D2/B1), square-ish, decaying. ----
    if bass_on {
        const NOTES: [f64; 4] = [55.0, 82.5, 73.4, 61.7];
        let f = NOTES[(bar_num as i64).rem_euclid(4) as usize];
        let ph = 2.0 * PI * f * ts;
        let te = (t - beat_num * beat) / sample_rate;
        // A slight per-beat decay: the note sustains through the beat (it does
        // not drop to near-zero), so the low end keeps the level up between kicks.
        let env = 0.7 + 0.3 * (-te / 0.25).exp();
        center += 0.3 * env * (ph.sin() + 0.3 * (3.0 * ph).sin());
    }

    // ---- Pad: a detuned chord (character) over a steady harmonic drone. ----
    // The chord's slow amplitude LFO keeps the mid bars breathing between hits.
    // The drone is two exact harmonics of the root (440 = 2×220, 660 = 3×220):
    // their sum is strictly periodic, so it holds a constant level with almost no
    // spectral flux — sustained mid/high energy that keeps every hop above the
    // noise floor (it cannot cancel against the low-frequency hits) without
    // adding spurious onsets, unlike the mutually-beating chord tones.
    {
        let lfo = 0.24 + 0.08 * (2.0 * PI * 0.1 * ts).sin(); // 0.16..0.32
        let chord = (2.0 * PI * 220.5 * ts).sin()
            + (2.0 * PI * 277.0 * ts).sin()
            + (2.0 * PI * 329.6 * ts).sin();
        let drone = (2.0 * PI * 440.0 * ts).sin() + (2.0 * PI * 660.0 * ts).sin();
        center += lfo * chord / 3.0 + 0.11 * drone;
    }

    // ---- Hats: 6 ms high-passed noise bursts on the off-eighths. ----
    let mut hat = 0.0f64;
    {
        let half = beat / 2.0; // eighth-note grid
        let e = (t / half).floor() as i64;
        // Play the off-eighths (odd), leaving the on-beats to the kick; the fill
        // bar plays every eighth.
        if e % 2 != 0 || double_hats {
            let ht = (t - (e as f64) * half) / sample_rate;
            if ht < 0.03 {
                // ~6 ms burst (env ≈ 0.05 at 6 ms).
                let env = (-ht / 0.002).exp();
                // One-pole high-pass: difference of successive noise samples.
                let hp = f64::from(noise(index) - noise(index.wrapping_sub(1)));
                hat += 0.25 * env * hp;
            }
        }
        // Fill bar: add a sixteenth-note layer between the eighths.
        if double_hats {
            let quarter = beat / 4.0;
            let s = (t / quarter).floor() as i64;
            if s % 2 != 0 {
                let st = (t - (s as f64) * quarter) / sample_rate;
                if st < 0.02 {
                    let env = (-st / 0.002).exp();
                    let hp = f64::from(noise(index) - noise(index.wrapping_sub(1)));
                    hat += 0.2 * env * hp;
                }
            }
        }
    }

    // ---- Sparkle: every 4 bars, a two-beat arpeggio 880→1760 Hz. ----
    let mut spark = 0.0f64;
    if (bar_num as i64).rem_euclid(4) == 3 {
        let in_bar = t - bar_num * bar; // samples into the bar
        let two_beats = 2.0 * beat;
        if in_bar < two_beats {
            let step = two_beats / 8.0; // eight ascending blips
            let k = (in_bar / step).floor();
            let bt = (in_bar - k * step) / sample_rate;
            if bt < 0.08 {
                let f = 880.0 * 2f64.powf(k / 7.0);
                let env = (-bt / 0.03).exp();
                spark += 0.2 * env * (2.0 * PI * f * ts).sin();
            }
        }
    }

    // ---- Constant-power panning: hats left, sparkle right, rest centered. ----
    // p ∈ [-1, 1] maps to an equal-power angle; the two gains keep l²+r² constant.
    let pan = |p: f64| -> (f64, f64) {
        let a = (p + 1.0) * (PI / 4.0);
        (a.cos(), a.sin())
    };
    let (cl, cr) = pan(0.0);
    let (hl, hr) = pan(-0.4);
    let (sl, sr) = pan(0.4);

    let left = center * cl + hat * hl + spark * sl;
    let right = center * cr + hat * hr + spark * sr;

    // Soft-clip each channel to ±0.9 (tanh knee).
    let clip = |x: f64| 0.9 * x.tanh();
    (clip(left) as f32, clip(right) as f32)
}

/// Fill `frames` frames of `channels`-wide interleaved audio for `signal` into
/// `buffer`, starting at absolute `frame_index` — the exact routine the
/// backend's producer thread runs, exposed for tests and tooling. Deterministic
/// and allocation-free given a `buffer` of at least `frames * channels`.
#[doc(hidden)]
pub fn fill_signal(
    buffer: &mut [f32],
    frames: usize,
    channels: usize,
    signal: Signal,
    sample_rate: f64,
    frame_index: u64,
) {
    fill(buffer, frames, channels, signal, sample_rate, frame_index);
}

/// The stream handle. Dropping it stops the producer thread and joins it.
struct SyntheticStream {
    format: StreamFormat,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl CaptureStream for SyntheticStream {
    fn format(&self) -> StreamFormat {
        self.format
    }
}

impl Drop for SyntheticStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The music mix is soft-clipped: no sample escapes ±0.95 over a dense
    /// sweep of the first four bars (which includes a full-mix bar, so it
    /// exercises the loudest layering).
    #[test]
    fn music_soft_clip_bounds_the_signal() {
        let sr = 48_000.0f64;
        let bpm = 112.0f32;
        let beat = 60.0 / f64::from(bpm) * sr;
        let four_bars = (beat * 4.0 * 4.0).ceil() as u64; // 4 bars × 4 beats
        let channels = 2usize;
        let chunk = CHUNK_FRAMES;
        let mut buffer = vec![0.0f32; chunk * channels];
        let mut frame_index = 0u64;
        let mut worst = 0.0f32;
        while frame_index < four_bars {
            let frames = ((four_bars - frame_index).min(chunk as u64)) as usize;
            fill_signal(
                &mut buffer,
                frames,
                channels,
                Signal::Music { bpm },
                sr,
                frame_index,
            );
            for &s in &buffer[..frames * channels] {
                worst = worst.max(s.abs());
                assert!(s.abs() <= 0.95, "sample {s} exceeded ±0.95");
            }
            frame_index += frames as u64;
        }
        // Sanity: the signal is actually driven, not accidentally silent.
        assert!(worst > 0.1, "music never rose above 0.1 (worst {worst})");
    }

    /// Filling the same frame range twice yields byte-identical samples: the
    /// music generator is deterministic and carries no state between chunks.
    #[test]
    fn music_fill_is_deterministic() {
        let sr = 48_000.0f64;
        let signal = Signal::Music { bpm: 112.0 };
        let channels = 2usize;
        let frames = 4096usize;
        let mut a = vec![0.0f32; frames * channels];
        let mut b = vec![0.0f32; frames * channels];
        fill_signal(&mut a, frames, channels, signal, sr, 10_000);
        fill_signal(&mut b, frames, channels, signal, sr, 10_000);
        assert_eq!(a, b, "two fills of the same range differed");
    }
}
