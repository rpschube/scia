//! A test-only capture backend whose behaviour a shared [`FaultyControl`]
//! drives, so a test can exercise the engine's runtime reopen path without any
//! audio hardware. It can: set the format the *next* open negotiates; trip a
//! fault that flips the live stream's health to `Errored` and stops its
//! producer; change the route id it reports; make the next N opens fail with
//! `NoDevice`; and count opens.
//!
//! Its live stream runs a producer thread pushing a real-time-paced 440 Hz sine
//! at amplitude 0.5 in 256-frame chunks, following the pattern in
//! `synthetic.rs::generate` (which this test must not modify).
#![allow(dead_code)]

use std::f64::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use scia_core::capture::{
    CaptureBackend, CaptureError, CaptureStream, CaptureTarget, SampleSink, StreamFormat,
    StreamHealth,
};

/// Frames per generated chunk — matches the DSP hop size.
const CHUNK_FRAMES: usize = 256;

/// Per-stream state shared with its producer thread. Tripping a fault sets both
/// flags: `stop` halts the producer (capture goes silent), `errored` flips the
/// stream's health to [`StreamHealth::Errored`].
struct StreamState {
    errored: AtomicBool,
    stop: AtomicBool,
}

/// The shared control block the test holds to steer a [`FaultyBackend`].
pub struct FaultyControl {
    next_sample_rate: AtomicU32,
    next_channels: AtomicU16,
    opens: AtomicU64,
    fail_next: AtomicU32,
    route: Mutex<String>,
    /// State of the most recently opened live stream, so `trip_fault` hits the
    /// current stream even after a reopen swapped in a new one.
    current: Mutex<Option<Arc<StreamState>>>,
}

impl FaultyControl {
    /// A control block whose first open negotiates `sample_rate`/`channels` and
    /// reports `route_id`.
    #[must_use]
    pub fn new(sample_rate: u32, channels: u16, route_id: &str) -> Arc<Self> {
        Arc::new(Self {
            next_sample_rate: AtomicU32::new(sample_rate),
            next_channels: AtomicU16::new(channels),
            opens: AtomicU64::new(0),
            fail_next: AtomicU32::new(0),
            route: Mutex::new(route_id.to_owned()),
            current: Mutex::new(None),
        })
    }

    /// Set the format the next `open` will negotiate.
    pub fn set_next_format(&self, sample_rate: u32, channels: u16) {
        self.next_sample_rate.store(sample_rate, Ordering::Relaxed);
        self.next_channels.store(channels, Ordering::Relaxed);
    }

    /// Make the next `n` opens fail with [`CaptureError::NoDevice`].
    pub fn fail_next_opens(&self, n: u32) {
        self.fail_next.store(n, Ordering::Relaxed);
    }

    /// Change the route id the backend reports, simulating the OS default route
    /// moving to a different device.
    pub fn set_route_id(&self, route_id: &str) {
        *self.route.lock().unwrap() = route_id.to_owned();
    }

    /// Number of `open` calls so far (successful or failed).
    #[must_use]
    pub fn opens(&self) -> u64 {
        self.opens.load(Ordering::Relaxed)
    }

    /// Trip a fault on the current live stream: stop its producer and mark it
    /// errored, exactly as a device-loss error callback would.
    pub fn trip_fault(&self) {
        if let Some(state) = self.current.lock().unwrap().as_ref() {
            state.stop.store(true, Ordering::Release);
            state.errored.store(true, Ordering::Release);
        }
    }
}

/// A capture backend whose behaviour [`FaultyControl`] drives.
pub struct FaultyBackend {
    control: Arc<FaultyControl>,
}

impl FaultyBackend {
    #[must_use]
    pub fn new(control: Arc<FaultyControl>) -> Self {
        Self { control }
    }
}

impl CaptureBackend for FaultyBackend {
    fn open(
        &mut self,
        _target: CaptureTarget,
        mut sink: SampleSink,
    ) -> Result<Box<dyn CaptureStream>, CaptureError> {
        self.control.opens.fetch_add(1, Ordering::Relaxed);

        // Honour a pending failure request.
        let remaining = self.control.fail_next.load(Ordering::Relaxed);
        if remaining > 0 {
            self.control
                .fail_next
                .store(remaining - 1, Ordering::Relaxed);
            return Err(CaptureError::NoDevice);
        }

        let format = StreamFormat {
            sample_rate: self.control.next_sample_rate.load(Ordering::Relaxed),
            channels: self.control.next_channels.load(Ordering::Relaxed),
        };
        let state = Arc::new(StreamState {
            errored: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        *self.control.current.lock().unwrap() = Some(Arc::clone(&state));

        let thread_state = Arc::clone(&state);
        let handle = thread::Builder::new()
            .name("faulty-feed".into())
            .spawn(move || produce(format, &thread_state, &mut sink))
            .map_err(|e| CaptureError::Backend(e.to_string()))?;

        Ok(Box::new(FaultyStream {
            format,
            state,
            handle: Some(handle),
        }))
    }

    fn route_id(&self) -> Option<String> {
        Some(self.control.route.lock().unwrap().clone())
    }
}

/// The producer thread: fill 256-frame chunks with a 440 Hz sine at amp 0.5 and
/// push them real-time-paced until the stream is stopped.
fn produce(format: StreamFormat, state: &StreamState, sink: &mut SampleSink) {
    let channels = format.channels.max(1) as usize;
    let sample_rate = f64::from(format.sample_rate.max(1));
    let mut buffer = vec![0.0f32; CHUNK_FRAMES * channels];

    let start = Instant::now();
    let mut frame_index: u64 = 0;
    let mut chunks_pushed: u64 = 0;

    loop {
        if state.stop.load(Ordering::Acquire) {
            break;
        }
        for frame in 0..CHUNK_FRAMES {
            let t = (frame_index + frame as u64) as f64 / sample_rate;
            let sample = (0.5 * (2.0 * PI * 440.0 * t).sin()) as f32;
            let base = frame * channels;
            for ch in 0..channels {
                buffer[base + ch] = sample;
            }
        }
        sink.push(&buffer);
        frame_index += CHUNK_FRAMES as u64;
        chunks_pushed += 1;

        let target = start
            + Duration::from_secs_f64(CHUNK_FRAMES as f64 * chunks_pushed as f64 / sample_rate);
        let now = Instant::now();
        if target > now {
            thread::sleep(target - now);
        }
    }
}

/// The live stream handle. Dropping it stops the producer and joins it.
struct FaultyStream {
    format: StreamFormat,
    state: Arc<StreamState>,
    handle: Option<JoinHandle<()>>,
}

impl CaptureStream for FaultyStream {
    fn format(&self) -> StreamFormat {
        self.format
    }

    fn health(&self) -> StreamHealth {
        if self.state.errored.load(Ordering::Acquire) {
            StreamHealth::Errored("faulty backend: device lost".to_owned())
        } else {
            StreamHealth::Ok
        }
    }
}

impl Drop for FaultyStream {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
