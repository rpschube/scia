//! The cpal-based capture backend: one code path that captures the system
//! output mix on every desktop platform cpal supports.
//!
//! - **Windows (WASAPI):** the system mix is captured by opening the *default
//!   output device* and building an **input** stream on it. cpal's WASAPI host
//!   turns an input stream on an output endpoint into a shared-mode loopback
//!   capture of everything that endpoint is playing.
//! - **Linux:** with [`prefer_pipewire`](CpalBackend::prefer_pipewire) and the
//!   `capture-pipewire` feature compiled in, the PipeWire host is selected and
//!   the default *output* device is opened as an input — a monitor of the sink,
//!   i.e. the real system mix. Otherwise the default host (ALSA) is used and the
//!   default *input* device is opened. **On plain ALSA that is the default
//!   capture device (microphone-level), not the system mix.** Capturing system
//!   audio on Linux needs either PipeWire (this feature) or an ALSA loopback /
//!   monitor device selected by name via [`DeviceSelector::Named`].
//! - **macOS (Core Audio):** the system mix is captured by opening the *default
//!   output device* and building an **input** stream on it, exactly like the
//!   Windows WASAPI loopback and the PipeWire sink monitor. cpal 0.18.2's
//!   Core Audio host recognises that an output endpoint has no input channels
//!   and transparently builds a **process tap** over that endpoint plus a
//!   private aggregate device (`AudioHardwareCreateProcessTap` /
//!   `CATapDescription`, macOS 14.4+), so the input stream carries the whole
//!   system output mix. This requires the user to grant the **System Audio
//!   Recording** TCC permission the first time; macOS exposes no API to query
//!   that permission's state, so a denied tap is detected by the engine's
//!   zero-delivery heuristic (see [`crate::engine`]). On macOS older than 14.4
//!   the tap APIs are absent and `open` fails cleanly; a user-installed
//!   loopback device (e.g. BlackHole) selected via [`DeviceSelector::Named`]
//!   is the fallback there. See `docs/macos.md`.
//!
//! The data callback does exactly one thing (Bencina rules — no allocation,
//! locks, logging or syscalls): convert the incoming buffer to interleaved
//! `f32` mono/stereo and push it to the ring. The conversion buffer is
//! preallocated at open; a callback larger than it is processed in chunks, never
//! reallocated. The error callback (which is allowed to lock, being off the
//! data path) records the fault for [`CaptureStream::health`].

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::convert::{
    Downmix, convert_and_push, f32_id, i16_to_f32, i32_to_f32, u8_to_f32, u16_to_f32,
};
use crate::capture::{
    CaptureBackend, CaptureError, CaptureStream, CaptureTarget, SampleSink, StreamFormat,
    StreamHealth,
};

/// Fallback upper bound (in frames) for the preallocated conversion buffer when
/// the backend reports no maximum buffer size. Matches the ring's frame span.
/// Preferred ALSA buffer size in frames (~10 ms at 48 kHz); clamped to the device range.
const PREFERRED_BUFFER_FRAMES: u32 = 512;
const DEFAULT_CAP_FRAMES: usize = 8192;

/// Which device a [`CpalBackend`] opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceSelector {
    /// The platform default (default output on Windows / PipeWire, default input
    /// on ALSA / Core Audio).
    Default,
    /// A device selected by its cpal name.
    Named(String),
}

/// A cpal capture backend. Construct it with the device to open and, on Linux,
/// whether to prefer the PipeWire sink monitor over the default host.
#[derive(Clone, Debug)]
pub struct CpalBackend {
    /// The device to open.
    pub device: DeviceSelector,
    /// Linux only: prefer the PipeWire host (sink monitor = system mix) when the
    /// `capture-pipewire` feature is compiled in and the host is available.
    /// Ignored on other platforms and when the feature is absent.
    pub prefer_pipewire: bool,
}

impl Default for CpalBackend {
    fn default() -> Self {
        Self {
            device: DeviceSelector::Default,
            prefer_pipewire: true,
        }
    }
}

/// Shared error state written by the stream's error callback and read back
/// through [`CaptureStream::health`].
#[derive(Default)]
struct StreamErrorState {
    /// Set once the error callback has fired at least once.
    errored: AtomicBool,
    /// Transient buffer under/overruns reported by the backend (not fatal).
    xruns: AtomicU64,
    /// The most recent error message. The error callback may lock — it does not
    /// run on the data path.
    last: Mutex<Option<String>>,
}

/// A live cpal capture stream. Dropping it stops and joins the cpal stream.
struct CpalStream {
    format: StreamFormat,
    error_state: Arc<StreamErrorState>,
    // Dropped last; keeps the callback (and its borrow of the sink) alive.
    _stream: cpal::Stream,
}

impl CaptureStream for CpalStream {
    fn format(&self) -> StreamFormat {
        self.format
    }

    fn xruns(&self) -> u64 {
        self.error_state.xruns.load(Ordering::Relaxed)
    }

    fn health(&self) -> StreamHealth {
        if self.error_state.errored.load(Ordering::Acquire) {
            let msg = self
                .error_state
                .last
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .unwrap_or_else(|| "stream error".to_string());
            StreamHealth::Errored(msg)
        } else {
            StreamHealth::Ok
        }
    }
}

impl CaptureBackend for CpalBackend {
    fn open(
        &mut self,
        _target: CaptureTarget,
        sink: SampleSink,
    ) -> Result<Box<dyn CaptureStream>, CaptureError> {
        let (device, supported) = self.resolve()?;
        let stream = build_stream(&device, supported, sink)?;
        Ok(Box::new(stream))
    }

    fn route_id(&self) -> Option<String> {
        // The stable identity of the device `open` would bind to right now: its
        // cpal display name. Shares the exact device-resolution logic `open`
        // uses (default output/input per host, or the named device), so a change
        // in the OS default route shows up here as a different name. Only the
        // name is resolved — no config negotiation — so this stays cheap enough
        // for the route watcher to poll every few hundred ms. Enumeration
        // failure or a missing device maps to `None`, and the watcher then
        // leans on stream health alone.
        self.resolve_device()
            .ok()
            .map(|(device, _)| device.to_string())
    }

    fn set_device(&mut self, selector: DeviceSelector) {
        // Record the new selector; the next `resolve`/`open` (driven by the
        // engine's reopen) binds it.
        self.device = selector;
    }
}

/// Which default-config direction a resolved device is opened with. WASAPI, the
/// PipeWire sink-monitor path and the macOS Core Audio process tap open the
/// default *output* endpoint as a loopback input, so they negotiate against its
/// output config; plain ALSA and the fallback open a real input device. Each
/// platform (and feature) only ever constructs one direction, so a variant is
/// dead on any single target — expected, like the enumeration helpers below.
#[derive(Clone, Copy)]
#[allow(dead_code)]
enum ConfigDir {
    Output,
    Input,
}

impl CpalBackend {
    /// Resolve the device and its default stream configuration for the current
    /// platform. Device selection is shared with [`route_id`](CpalBackend::route_id)
    /// through [`resolve_device`](CpalBackend::resolve_device); only the
    /// config-negotiation step is added here.
    fn resolve(&self) -> Result<(cpal::Device, cpal::SupportedStreamConfig), CaptureError> {
        let (device, dir) = self.resolve_device()?;
        let supported = match dir {
            ConfigDir::Output => device.default_output_config(),
            ConfigDir::Input => device.default_input_config(),
        }
        .map_err(map_cpal_err)?;
        Ok((device, supported))
    }

    /// Resolve just the device (and the config direction it is opened with) for
    /// the current platform, without negotiating a stream config. Shared by
    /// `open`/`resolve` and by `route_id`, so both always agree on which device
    /// the backend would bind to.
    #[cfg(target_os = "windows")]
    fn resolve_device(&self) -> Result<(cpal::Device, ConfigDir), CaptureError> {
        // System mix = the default output endpoint, opened as an input
        // (shared-mode loopback).
        let host = cpal::default_host();
        let device = match &self.device {
            DeviceSelector::Default => {
                host.default_output_device().ok_or(CaptureError::NoDevice)?
            }
            DeviceSelector::Named(name) => find_by_name(output_devices(&host)?, name)?,
        };
        Ok((device, ConfigDir::Output))
    }

    /// See the Windows [`resolve_device`](CpalBackend::resolve_device).
    #[cfg(target_os = "linux")]
    fn resolve_device(&self) -> Result<(cpal::Device, ConfigDir), CaptureError> {
        // Prefer the PipeWire sink monitor (the real system mix) when asked and
        // available. This whole branch only compiles with the feature; the
        // fallback below is the plain-ALSA default-input path. The undocumented
        // cpal behaviour this relies on (a sink opened as an input becomes a
        // capture of the sink monitor) is pinned and verified by the `pipewire`
        // CI job — see docs/probes/p2-pipewire-pin.md.
        #[cfg(feature = "capture-pipewire")]
        if self.prefer_pipewire && cpal::available_hosts().contains(&cpal::HostId::PipeWire) {
            if let Ok(host) = cpal::host_from_id(cpal::HostId::PipeWire) {
                let device = match &self.device {
                    DeviceSelector::Default => {
                        host.default_output_device().ok_or(CaptureError::NoDevice)?
                    }
                    DeviceSelector::Named(name) => find_by_name(output_devices(&host)?, name)?,
                };
                return Ok((device, ConfigDir::Output));
            }
        }

        // Default host (ALSA): the default *input* device — microphone-level on
        // plain ALSA, not the system mix.
        let host = cpal::default_host();
        let device = match &self.device {
            DeviceSelector::Default => host.default_input_device().ok_or(CaptureError::NoDevice)?,
            DeviceSelector::Named(name) => find_by_name(input_devices(&host)?, name)?,
        };
        Ok((device, ConfigDir::Input))
    }

    /// See the Windows [`resolve_device`](CpalBackend::resolve_device).
    #[cfg(target_os = "macos")]
    fn resolve_device(&self) -> Result<(cpal::Device, ConfigDir), CaptureError> {
        // System mix = the default output endpoint, opened as an input. cpal's
        // Core Audio host sees the endpoint has no input channels and builds a
        // process tap + aggregate device over it (macOS 14.4+), so the input
        // stream carries the whole system output mix — the same output-as-input
        // shape as the Windows WASAPI loopback and the PipeWire sink monitor.
        // Negotiating against the endpoint's *output* config (ConfigDir::Output)
        // matches the format the tap delivers. A `Named` selector picks an output
        // endpoint by name and taps that endpoint's mix.
        let host = cpal::default_host();
        let device = match &self.device {
            DeviceSelector::Default => {
                host.default_output_device().ok_or(CaptureError::NoDevice)?
            }
            DeviceSelector::Named(name) => find_by_name(output_devices(&host)?, name)?,
        };
        Ok((device, ConfigDir::Output))
    }

    /// See the Windows [`resolve_device`](CpalBackend::resolve_device).
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    fn resolve_device(&self) -> Result<(cpal::Device, ConfigDir), CaptureError> {
        let host = cpal::default_host();
        let device = match &self.device {
            DeviceSelector::Default => host.default_input_device().ok_or(CaptureError::NoDevice)?,
            DeviceSelector::Named(name) => find_by_name(input_devices(&host)?, name)?,
        };
        Ok((device, ConfigDir::Input))
    }
}

/// Build, start and wrap the cpal input stream. Selects the per-sample-format
/// converter once, preallocates the conversion buffer, and installs the data
/// and error callbacks.
fn build_stream(
    device: &cpal::Device,
    supported: cpal::SupportedStreamConfig,
    mut sink: SampleSink,
) -> Result<CpalStream, CaptureError> {
    let sample_format = supported.sample_format();
    let device_channels = supported.channels() as usize;
    let sample_rate = supported.sample_rate();
    if sample_rate == 0 || device_channels == 0 {
        return Err(CaptureError::Unsupported(format!(
            "device reported {sample_rate} Hz / {device_channels} channels"
        )));
    }
    let mut config: cpal::StreamConfig = supported.config();
    // On ALSA the device's default period can be enormous (16k+ frames, hundreds
    // of milliseconds of latency). Ask for a small buffer when the device says it
    // supports one; other hosts keep their engine-managed default.
    #[cfg(target_os = "linux")]
    if let cpal::SupportedBufferSize::Range { min, max } = supported.buffer_size() {
        let want = PREFERRED_BUFFER_FRAMES.clamp(*min, *max);
        config.buffer_size = cpal::BufferSize::Fixed(want);
    }

    let downmix = Downmix::new(device_channels);
    let out_channels = downmix.out_channels;

    // Preallocate the conversion buffer for the largest plausible callback: the
    // backend's max buffer size if it reports one, else DEFAULT_CAP_FRAMES.
    let cap_frames = match supported.buffer_size() {
        cpal::SupportedBufferSize::Range { max, .. } => (*max as usize).max(DEFAULT_CAP_FRAMES),
        cpal::SupportedBufferSize::Unknown => DEFAULT_CAP_FRAMES,
    };
    let mut out_buf = vec![0.0f32; cap_frames * out_channels];

    let format = StreamFormat {
        sample_rate,
        channels: out_channels as u16,
    };

    let error_state = Arc::new(StreamErrorState::default());
    let err_state = Arc::clone(&error_state);
    // Device-loss behaviour, confirmed against the cpal 0.18.2 sources in the
    // cargo registry, so the engine never waits for a stream to heal itself:
    //
    // - WASAPI: when the active endpoint is invalidated, the run loop's
    //   process_input/process_output surfaces `AUDCLNT_E_DEVICE_INVALIDATED`,
    //   which `From<windows::core::Error>` maps to `ErrorKind::DeviceNotAvailable`
    //   (src/host/wasapi/mod.rs:62-64); `run_input`/`run_output` then call
    //   `emit_error` and `break`, so the worker thread exits and never resumes
    //   (src/host/wasapi/stream.rs:659-712). A default-device change with no
    //   replacement reports `DeviceNotAvailable`, one with a replacement reports
    //   `StreamInvalidated` (src/host/wasapi/stream.rs:730-736) — cpal 0.18 does
    //   not silently reroute, so a rebuild is always required.
    // - ALSA: on unplug the PCM enters `State::Disconnected`, mapped to
    //   `ErrorKind::DeviceNotAvailable` (src/host/alsa/mod.rs:1087-1092); the
    //   input/output worker then does `error_callback(err); return;` and exits
    //   (src/host/alsa/mod.rs:956-958, 1015-1017). Only `Xrun` is recovered in
    //   place (prepare()/start()); `DeviceNotAvailable` is terminal.
    //
    // Both hosts therefore deliver device invalidation as a non-Xrun error and
    // stop their stream thread for good. The branch below records that as a
    // health fault, and the engine's route watcher rebuilds the stream — it must
    // not expect the dead stream to recover on its own.
    //
    // PipeWire default-sink switch (CAP-2 on Linux): the native PipeWire host
    // opens the default output as an `is_default_device` monitor stream and
    // subscribes to the session-manager "default" metadata; a `pactl
    // set-default-sink` fires this error callback with `ErrorKind::DeviceChanged`
    // ("default device changed"). It is not `Xrun`, so it marks the stream
    // errored, and the route watcher reopens. The reopen must drop this errored
    // stream *before* opening its replacement (see `engine::Shared::reopen`): a
    // fresh `host_from_id(PipeWire)` re-enumerates and resolves the new default,
    // but a replacement opened while the errored stream is still connected does
    // not settle on the PipeWire graph across the switch. Dropping the dead
    // stream first is what makes recovery work here.
    let error_callback = move |err: cpal::Error| {
        // Off the data path: locking here is allowed.
        // A buffer under/overrun is a transient glitch (the engine dropped or
        // repeated a packet); it is counted, never fatal. Anything else — the
        // device going away, a backend failure — marks the stream errored.
        if matches!(err.kind(), cpal::ErrorKind::Xrun) {
            err_state.xruns.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if let Ok(mut slot) = err_state.last.lock() {
            *slot = Some(err.to_string());
        }
        err_state.errored.store(true, Ordering::Release);
    };

    // Timeout `None`: wait for the backend to initialise the stream.
    let timeout = None;

    // One closure per sample format, selected here so the hot path carries no
    // per-callback branch on the format.
    let stream = match sample_format {
        cpal::SampleFormat::F32 => device.build_input_stream(
            config,
            move |data: &[f32], _| {
                convert_and_push(data, f32_id, &downmix, &mut out_buf, &mut sink)
            },
            error_callback,
            timeout,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _| {
                convert_and_push(data, i16_to_f32, &downmix, &mut out_buf, &mut sink)
            },
            error_callback,
            timeout,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            config,
            move |data: &[u16], _| {
                convert_and_push(data, u16_to_f32, &downmix, &mut out_buf, &mut sink)
            },
            error_callback,
            timeout,
        ),
        cpal::SampleFormat::I32 => device.build_input_stream(
            config,
            move |data: &[i32], _| {
                convert_and_push(data, i32_to_f32, &downmix, &mut out_buf, &mut sink)
            },
            error_callback,
            timeout,
        ),
        cpal::SampleFormat::U8 => device.build_input_stream(
            config,
            move |data: &[u8], _| {
                convert_and_push(data, u8_to_f32, &downmix, &mut out_buf, &mut sink)
            },
            error_callback,
            timeout,
        ),
        other => {
            return Err(CaptureError::Unsupported(format!(
                "sample format {other:?} is not handled"
            )));
        }
    }
    .map_err(map_cpal_err)?;

    stream.play().map_err(map_cpal_err)?;

    Ok(CpalStream {
        format,
        error_state,
        _stream: stream,
    })
}

// The two enumeration helpers are each used only by some platforms' `resolve`
// (output on Windows / PipeWire / macOS Core Audio, input on ALSA), so one is
// dead on any single target — that is expected, not a bug.

/// Enumerate output devices, mapping enumeration failure to a capture error.
#[allow(dead_code)]
fn output_devices(host: &cpal::Host) -> Result<Vec<cpal::Device>, CaptureError> {
    Ok(host.output_devices().map_err(map_cpal_err)?.collect())
}

/// Enumerate input devices, mapping enumeration failure to a capture error.
#[allow(dead_code)]
fn input_devices(host: &cpal::Host) -> Result<Vec<cpal::Device>, CaptureError> {
    Ok(host.input_devices().map_err(map_cpal_err)?.collect())
}

/// Find a device by its cpal display name (the name printed by `--list`).
#[allow(dead_code)]
fn find_by_name(devices: Vec<cpal::Device>, name: &str) -> Result<cpal::Device, CaptureError> {
    devices
        .into_iter()
        .find(|d| d.to_string() == name)
        .ok_or(CaptureError::NoDevice)
}

/// Map a cpal error onto a [`CaptureError`], treating a missing device as
/// [`CaptureError::NoDevice`] and everything else as a backend fault.
fn map_cpal_err(err: cpal::Error) -> CaptureError {
    match err.kind() {
        cpal::ErrorKind::DeviceNotAvailable => CaptureError::NoDevice,
        cpal::ErrorKind::UnsupportedConfig | cpal::ErrorKind::UnsupportedOperation => {
            CaptureError::Unsupported(err.to_string())
        }
        _ => CaptureError::Backend(err.to_string()),
    }
}

/// Which direction a [`DeviceInfo`] entry can serve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    /// A capture (input) device.
    Input,
    /// A playback (output) device.
    Output,
}

/// One enumerated device, for a future `--list-devices` surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The cpal device name (also the string [`DeviceSelector::Named`] matches).
    pub name: String,
    /// Whether this device is the host's default input.
    pub is_default_input: bool,
    /// Whether this device is the host's default output.
    pub is_default_output: bool,
    /// Input or output.
    pub kind: DeviceKind,
    /// The host that owns the device (e.g. `"alsa"`, `"wasapi"`, `"pipewire"`).
    pub host: String,
}

/// Enumerate every device on every available cpal host, both directions.
///
/// Returns [`CaptureError::NoDevice`] only when no host yields any device; an
/// otherwise-empty host contributes nothing but is not an error. Intended for a
/// `--list-devices` command and diagnostics, never the hot path.
///
/// # Errors
/// Propagates a [`CaptureError::Backend`] if a host cannot be initialised and no
/// device could be listed at all.
pub fn list_devices() -> Result<Vec<DeviceInfo>, CaptureError> {
    let mut out = Vec::new();
    let mut last_err: Option<CaptureError> = None;

    for host_id in cpal::available_hosts() {
        let host = match cpal::host_from_id(host_id) {
            Ok(h) => h,
            Err(e) => {
                last_err = Some(map_cpal_err(e));
                continue;
            }
        };
        let host_name = host_id.to_string();

        let default_in = host
            .default_input_device()
            .map(|d| d.to_string())
            .unwrap_or_default();
        let default_out = host
            .default_output_device()
            .map(|d| d.to_string())
            .unwrap_or_default();

        if let Ok(devices) = host.input_devices() {
            for d in devices {
                let name = d.to_string();
                out.push(DeviceInfo {
                    is_default_input: !default_in.is_empty() && name == default_in,
                    is_default_output: false,
                    kind: DeviceKind::Input,
                    host: host_name.clone(),
                    name,
                });
            }
        }
        if let Ok(devices) = host.output_devices() {
            for d in devices {
                let name = d.to_string();
                out.push(DeviceInfo {
                    is_default_input: false,
                    is_default_output: !default_out.is_empty() && name == default_out,
                    kind: DeviceKind::Output,
                    host: host_name.clone(),
                    name,
                });
            }
        }
    }

    if out.is_empty() {
        return Err(last_err.unwrap_or(CaptureError::NoDevice));
    }
    Ok(out)
}
