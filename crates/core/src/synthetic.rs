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
#[derive(Clone, Copy, Debug)]
pub struct SyntheticBackend {
    /// Stream format to report and generate at. Defaults to 48 kHz stereo.
    pub format: StreamFormat,
    /// Waveform to generate.
    pub signal: Signal,
    /// Delivery pacing.
    pub pacing: Pacing,
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
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);

        let handle = thread::Builder::new()
            .name("scia-synth".into())
            .spawn(move || generate(format, signal, pacing, &thread_stop, &mut sink))
            .map_err(|e| CaptureError::Backend(e.to_string()))?;

        Ok(Box::new(SyntheticStream {
            format,
            stop,
            handle: Some(handle),
        }))
    }
}

/// The producer thread: fill chunks and deliver them per the pacing policy.
fn generate(
    format: StreamFormat,
    signal: Signal,
    pacing: Pacing,
    stop: &AtomicBool,
    sink: &mut SampleSink,
) {
    let channels = format.channels as usize;
    let sample_rate = f64::from(format.sample_rate);
    let mut buffer = vec![0.0f32; CHUNK_FRAMES * channels];

    let start = Instant::now();
    let mut frame_index: u64 = 0;
    let mut chunks_pushed: u64 = 0;

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
    }
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
