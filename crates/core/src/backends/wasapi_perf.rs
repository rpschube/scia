//! Windows-only opt-in **perf mode**: a companion silent shared-mode render
//! stream, opened via `IAudioClient3::InitializeSharedAudioStream` at the
//! endpoint's *minimum* engine period.
//!
//! Windows runs the shared audio engine at a per-endpoint period (typically
//! 10 ms). A loopback capture inherits whatever period the endpoint is running
//! at. Because the period is a property of the endpoint — not of any one
//! stream — a client that opens a render stream at the endpoint's minimum
//! period (typically 128 frames ≈ 2.7 ms at 48 kHz, read from
//! `GetSharedModeEnginePeriod`) pulls the whole endpoint down to that period,
//! and a loopback capture on the same endpoint then delivers packets at the
//! fast cadence too. The cost is a ~2.7 ms CPU wake cadence for the companion
//! stream, so it is opt-in.
//!
//! The companion stream renders **silence** — zero real audio — on a dedicated
//! `scia-perf-render` thread that owns every COM object it touches. Dropping
//! [`PerfModeStream`] stops the stream and releases the COM objects.
//!
//! On non-Windows targets [`PerfModeStream::open`] returns
//! [`CaptureError::Unsupported`] so callers compile and run everywhere.

use crate::backends::cpal::DeviceSelector;
// Used by the non-Windows stub below; the Windows impl imports it in `win_impl`.
#[cfg(not(windows))]
use crate::capture::CaptureError;

/// Which endpoint the companion stream opens. `Default` is the default render
/// endpoint (the usual choice — the endpoint the system mix plays out of).
#[derive(Clone, Debug)]
pub struct PerfModeConfig {
    /// The render endpoint to open the companion stream on. `Default` selects
    /// the default render endpoint (`eRender`, `eConsole`).
    pub device: DeviceSelector,
    /// Refuse to open a useless companion stream on a driver-locked endpoint.
    /// When `true`, [`PerfModeStream::open`] returns
    /// [`CaptureError::Unsupported`] on an endpoint whose minimum engine period
    /// is not below its default (no faster period exists to pull the endpoint
    /// down to). When `false` (the default), `open` keeps its historical
    /// behaviour and falls back to the default period. The engine sets this
    /// `true`: it has already capability-detected the endpoint via
    /// [`availability`] and only opens when a faster period exists.
    pub require_fast: bool,
}

impl Default for PerfModeConfig {
    fn default() -> Self {
        Self {
            device: DeviceSelector::Default,
            require_fast: false,
        }
    }
}

/// The capability verdict for perf mode on an endpoint, decided before any
/// render stream is opened. See [`classify`] for the rule and [`availability`]
/// for the query that produces it.
#[derive(Clone, Debug)]
pub enum PerfModeAvailability {
    /// The endpoint advertises a minimum engine period below its default, so a
    /// companion stream can pull it down to a faster cadence.
    Available {
        /// The queried engine periods. `chosen_period_frames` is `0`: no stream
        /// has been opened yet.
        info: PerfModeInfo,
    },
    /// The endpoint's minimum engine period equals its default: no faster period
    /// exists, so a companion stream would bring nothing.
    DriverLocked {
        /// The queried engine periods. `chosen_period_frames` is `0`.
        info: PerfModeInfo,
    },
    /// Perf mode could not be evaluated: not Windows, no render endpoint, a COM
    /// failure, or a degenerate period report. The string is a one-line reason.
    Unsupported(String),
}

/// Classify an endpoint's engine periods into a [`PerfModeAvailability`]. Pure
/// and platform-independent — it decides only from the numbers, so it compiles
/// and unit-tests on every OS.
///
/// - Zero default or minimum period → [`PerfModeAvailability::Unsupported`]
///   (a degenerate report; nothing can be decided).
/// - `min < default` → [`PerfModeAvailability::Available`] (a faster period
///   exists).
/// - otherwise (`min == default`, or a degenerate `min > default`) →
///   [`PerfModeAvailability::DriverLocked`] (no faster period).
#[must_use]
pub fn classify(info: &PerfModeInfo) -> PerfModeAvailability {
    if info.default_period_frames == 0 || info.min_period_frames == 0 {
        return PerfModeAvailability::Unsupported(format!(
            "endpoint reported a degenerate engine period (default {} frames, min {} frames)",
            info.default_period_frames, info.min_period_frames
        ));
    }
    if info.min_period_frames < info.default_period_frames {
        PerfModeAvailability::Available { info: *info }
    } else {
        PerfModeAvailability::DriverLocked { info: *info }
    }
}

/// The engine periods reported for the endpoint, plus the period the companion
/// stream actually runs at. All periods are in **frames**; divide by
/// [`sample_rate`](PerfModeInfo::sample_rate) for seconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PerfModeInfo {
    /// The endpoint's default engine period (the period a loopback capture sees
    /// without perf mode; typically ~10 ms).
    pub default_period_frames: u32,
    /// The fundamental period; every valid engine period is a multiple of it.
    pub fundamental_period_frames: u32,
    /// The minimum engine period the endpoint supports (the fast cadence perf
    /// mode aims for; typically ~2.7 ms).
    pub min_period_frames: u32,
    /// The maximum engine period the endpoint supports.
    pub max_period_frames: u32,
    /// The period the companion stream actually initialised at. Equal to
    /// [`min_period_frames`](PerfModeInfo::min_period_frames) on success, or
    /// [`default_period_frames`](PerfModeInfo::default_period_frames) if the
    /// endpoint refused the fast period and perf mode fell back.
    pub chosen_period_frames: u32,
    /// The endpoint mix-format sample rate in Hz.
    pub sample_rate: u32,
    /// The endpoint mix-format channel count.
    pub channels: u16,
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod win_impl {
    //! The real WASAPI implementation. Every COM object is created, used, and
    //! released on the single `scia-perf-render` thread; nothing COM crosses a
    //! thread boundary (COM interfaces are not `Send`), so `open` hands the
    //! thread a plain [`DeviceSelector`] and receives back a [`PerfModeInfo`]
    //! (or a [`CaptureError`]) over a channel.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};

    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Foundation::{
        CloseHandle, RPC_E_CHANGED_MODE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows::Win32::Media::Audio::{
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        DEVICE_STATE_ACTIVE, IAudioClient3, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
        MMDeviceEnumerator, WAVEFORMATEX, eConsole, eRender,
    };
    use windows::Win32::System::Com::StructuredStorage::{
        PropVariantClear, PropVariantToStringAlloc,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize, STGM_READ,
    };
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    /// How long the render thread waits for an engine wake before re-checking the stop flag.
    const STOP_POLL_MS: u32 = 200;
    use windows::core::PCWSTR;

    use super::PerfModeInfo;
    use crate::backends::cpal::DeviceSelector;
    use crate::capture::CaptureError;

    /// A live companion render stream. Dropping it signals the render thread to
    /// stop and joins it, which releases every COM object.
    pub struct PerfModeStream {
        stop: Arc<AtomicBool>,
        join: Option<JoinHandle<()>>,
        info: PerfModeInfo,
    }

    impl PerfModeStream {
        /// Open a silent shared-mode render stream on the endpoint at its
        /// minimum engine period.
        ///
        /// # Errors
        /// [`CaptureError::NoDevice`] when the requested endpoint does not
        /// exist, or [`CaptureError::Backend`] carrying the HRESULT for any COM
        /// failure.
        pub fn open(config: &super::PerfModeConfig) -> Result<PerfModeStream, CaptureError> {
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = Arc::clone(&stop);
            let selector = config.device.clone();
            let require_fast = config.require_fast;
            let (tx, rx) = mpsc::channel::<Result<PerfModeInfo, CaptureError>>();

            let join = thread::Builder::new()
                .name("scia-perf-render".into())
                .spawn(move || render_thread(&selector, require_fast, &stop_thread, &tx))
                .map_err(|e| {
                    CaptureError::Backend(format!("failed to spawn perf render thread: {e}"))
                })?;

            match rx.recv() {
                Ok(Ok(info)) => Ok(PerfModeStream {
                    stop,
                    join: Some(join),
                    info,
                }),
                Ok(Err(e)) => {
                    let _ = join.join();
                    Err(e)
                }
                Err(_) => {
                    let _ = join.join();
                    Err(CaptureError::Backend(
                        "perf render thread exited before reporting".to_string(),
                    ))
                }
            }
        }

        /// The engine periods and chosen period for the open stream.
        #[must_use]
        pub fn info(&self) -> PerfModeInfo {
            self.info
        }
    }

    impl Drop for PerfModeStream {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    /// Capability query: return the endpoint's engine periods **without opening
    /// a render stream**. Runs on a short-lived COM thread, exactly like
    /// [`PerfModeStream::open`], so the two share this query path.
    ///
    /// Off Windows this lives in the module's non-Windows arm; here it spawns a
    /// probe thread, initialises COM on it, queries, and tears COM down.
    pub fn availability(config: &super::PerfModeConfig) -> super::PerfModeAvailability {
        let selector = config.device.clone();
        let (tx, rx) = mpsc::channel::<Result<PerfModeInfo, CaptureError>>();

        let join = match thread::Builder::new()
            .name("scia-perf-probe".into())
            .spawn(move || probe_thread(&selector, &tx))
        {
            Ok(join) => join,
            Err(e) => {
                return super::PerfModeAvailability::Unsupported(format!(
                    "failed to spawn perf probe thread: {e}"
                ));
            }
        };

        let result = rx.recv();
        let _ = join.join();
        match result {
            Ok(Ok(info)) => super::classify(&info),
            Ok(Err(e)) => super::PerfModeAvailability::Unsupported(format!("{e}")),
            Err(_) => super::PerfModeAvailability::Unsupported(
                "perf probe thread exited before reporting".to_string(),
            ),
        }
    }

    /// Probe-thread entry point: initialise COM, query the endpoint periods, and
    /// tear COM down. Sends the [`PerfModeInfo`] (or a [`CaptureError`]) once.
    fn probe_thread(
        selector: &DeviceSelector,
        tx: &mpsc::Sender<Result<PerfModeInfo, CaptureError>>,
    ) {
        // SAFETY: COM is initialised and uninitialised on this thread only, and
        // every COM object created below is confined to this thread.
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            let com_owned = hr.is_ok();
            if hr.is_err() && hr != RPC_E_CHANGED_MODE {
                let _ = tx.send(Err(CaptureError::Backend(format!(
                    "CoInitializeEx failed: {hr:?}"
                ))));
                return;
            }

            let _ = tx.send(probe_periods(selector));

            if com_owned {
                CoUninitialize();
            }
        }
    }

    /// Create the enumerator, query the endpoint periods, and release the COM
    /// objects. No render stream is opened.
    ///
    /// # Safety
    /// Must run on a thread that has initialised COM (see [`probe_thread`]).
    unsafe fn probe_periods(selector: &DeviceSelector) -> Result<PerfModeInfo, CaptureError> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(backend_err)?;
            let (client, mix, info) = query_periods(&enumerator, selector)?;
            CoTaskMemFree(Some(mix.cast()));
            drop(client);
            Ok(info)
        }
    }

    /// Resolve the endpoint, activate an [`IAudioClient3`], and read its mix
    /// format and engine periods. Returns the live client and the caller-owned
    /// mix-format pointer (the caller frees it with [`CoTaskMemFree`]) alongside
    /// a [`PerfModeInfo`] whose `chosen_period_frames` is `0` — no stream is
    /// opened here. Shared by the capability probe and the render open path.
    ///
    /// # Safety
    /// Must run on a COM-initialised thread; the returned client and mix pointer
    /// must be used and released on that same thread.
    unsafe fn query_periods(
        enumerator: &IMMDeviceEnumerator,
        selector: &DeviceSelector,
    ) -> Result<(IAudioClient3, *mut WAVEFORMATEX, PerfModeInfo), CaptureError> {
        unsafe {
            let device = resolve_device(enumerator, selector)?;
            let client: IAudioClient3 = device.Activate(CLSCTX_ALL, None).map_err(backend_err)?;

            let mix = client.GetMixFormat().map_err(backend_err)?;
            if mix.is_null() {
                return Err(CaptureError::Backend(
                    "GetMixFormat returned a null format".to_string(),
                ));
            }
            let sample_rate = (*mix).nSamplesPerSec;
            let channels = (*mix).nChannels;

            let (mut default_p, mut fundamental_p, mut min_p, mut max_p) = (0u32, 0u32, 0u32, 0u32);
            if let Err(e) = client.GetSharedModeEnginePeriod(
                mix,
                &mut default_p,
                &mut fundamental_p,
                &mut min_p,
                &mut max_p,
            ) {
                CoTaskMemFree(Some(mix.cast()));
                return Err(backend_err(e));
            }

            let info = PerfModeInfo {
                default_period_frames: default_p,
                fundamental_period_frames: fundamental_p,
                min_period_frames: min_p,
                max_period_frames: max_p,
                chosen_period_frames: 0,
                sample_rate,
                channels,
            };
            Ok((client, mix, info))
        }
    }

    /// Render-thread entry point: initialise COM for the thread, run the stream,
    /// and tear COM down on exit. All COM objects live and die here.
    fn render_thread(
        selector: &DeviceSelector,
        require_fast: bool,
        stop: &Arc<AtomicBool>,
        tx: &mpsc::Sender<Result<PerfModeInfo, CaptureError>>,
    ) {
        // SAFETY: COM is initialised and uninitialised on this thread only, and
        // every COM object created below is confined to this thread.
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            // A fresh thread never has a conflicting apartment, but tolerate
            // RPC_E_CHANGED_MODE defensively: it means COM is already up in a
            // different mode (usable), and that call did not take a reference,
            // so it must not be balanced with CoUninitialize.
            let com_owned = hr.is_ok();
            if hr.is_err() && hr != RPC_E_CHANGED_MODE {
                let _ = tx.send(Err(CaptureError::Backend(format!(
                    "CoInitializeEx failed: {hr:?}"
                ))));
                return;
            }

            if let Err(e) = run_render(selector, require_fast, stop, tx) {
                let _ = tx.send(Err(e));
            }

            if com_owned {
                CoUninitialize();
            }
        }
    }

    /// Open, start and pump the companion stream. Reports the resolved
    /// [`PerfModeInfo`] over `tx` exactly once on success, then renders silence
    /// until `stop` is set. Returns `Err` only for a failure *before* the
    /// success report; once reported, later faults just end the loop.
    ///
    /// # Safety
    /// Must run on a thread that has initialised COM (see [`render_thread`]).
    unsafe fn run_render(
        selector: &DeviceSelector,
        require_fast: bool,
        stop: &Arc<AtomicBool>,
        tx: &mpsc::Sender<Result<PerfModeInfo, CaptureError>>,
    ) -> Result<(), CaptureError> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(backend_err)?;
            let (client, mix, info) = query_periods(&enumerator, selector)?;
            let default_p = info.default_period_frames;
            let fundamental_p = info.fundamental_period_frames;
            let min_p = info.min_period_frames;
            let sample_rate = info.sample_rate;
            let channels = info.channels;

            // When the caller requires a genuinely faster period, refuse a
            // driver-locked endpoint instead of opening a companion stream that
            // would run at the default period and bring nothing. The engine sets
            // this after capability-detecting via `availability`.
            if require_fast
                && !matches!(
                    super::classify(&info),
                    super::PerfModeAvailability::Available { .. }
                )
            {
                let ms = f64::from(default_p) * 1000.0 / f64::from(sample_rate.max(1));
                CoTaskMemFree(Some(mix.cast()));
                return Err(CaptureError::Unsupported(format!(
                    "endpoint offers no engine period below the default \
                     ({default_p} frames, {ms:.3} ms)"
                )));
            }

            // Aim for the minimum period, rounded up to a multiple of the
            // fundamental period as the API requires; fall back to the default
            // period if the minimum is unreported.
            let mut target = min_p;
            if fundamental_p > 0 && target % fundamental_p != 0 {
                target = target.div_ceil(fundamental_p).saturating_mul(fundamental_p);
            }
            if target == 0 {
                target = default_p;
            }

            let event = match CreateEventW(None, false, false, PCWSTR::null()) {
                Ok(h) => h,
                Err(e) => {
                    CoTaskMemFree(Some(mix.cast()));
                    return Err(backend_err(e));
                }
            };

            // Try the fast period first. If the endpoint refuses it (periodicity
            // locked, or a format-lock error), fall back to a plain shared-mode
            // event-driven init at the default period: the loopback still works,
            // just without the speed-up.
            let mut chosen = target;
            if let Err(fast_err) = client.InitializeSharedAudioStream(
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                target,
                mix,
                None,
            ) {
                match client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    0,
                    0,
                    mix,
                    None,
                ) {
                    Ok(()) => chosen = default_p,
                    Err(fb_err) => {
                        CoTaskMemFree(Some(mix.cast()));
                        let _ = CloseHandle(event);
                        return Err(CaptureError::Backend(format!(
                            "fast init failed ({fast_err}); fallback init failed ({fb_err})"
                        )));
                    }
                }
            }
            CoTaskMemFree(Some(mix.cast()));

            if let Err(e) = client.SetEventHandle(event) {
                let _ = CloseHandle(event);
                return Err(backend_err(e));
            }
            let render: IAudioRenderClient = match client.GetService() {
                Ok(r) => r,
                Err(e) => {
                    let _ = CloseHandle(event);
                    return Err(backend_err(e));
                }
            };
            let buffer_frames = match client.GetBufferSize() {
                Ok(n) => n,
                Err(e) => {
                    let _ = CloseHandle(event);
                    return Err(backend_err(e));
                }
            };
            if let Err(e) = client.Start() {
                let _ = CloseHandle(event);
                return Err(backend_err(e));
            }

            let info = PerfModeInfo {
                default_period_frames: default_p,
                fundamental_period_frames: fundamental_p,
                min_period_frames: min_p,
                max_period_frames: info.max_period_frames,
                chosen_period_frames: chosen,
                sample_rate,
                channels,
            };
            let _ = tx.send(Ok(info));

            // Silent render loop: on each engine wake, fill the newly-freed
            // portion of the buffer and release it flagged silent — no real
            // audio, no allocation.
            loop {
                // Bounded wait: the engine normally signals every period, but a
                // halted engine must not be able to hang shutdown.
                let waited = WaitForSingleObject(event, STOP_POLL_MS);
                if stop.load(Ordering::Acquire) {
                    break;
                }
                if waited == WAIT_TIMEOUT {
                    continue;
                }
                if waited != WAIT_OBJECT_0 {
                    break;
                }
                let padding = match client.GetCurrentPadding() {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let avail = buffer_frames.saturating_sub(padding);
                if avail > 0 {
                    match render.GetBuffer(avail) {
                        Ok(_buf) => {
                            if render
                                .ReleaseBuffer(avail, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }

            let _ = client.Stop();
            let _ = CloseHandle(event);
            Ok(())
        }
    }

    /// Resolve the render endpoint named by `selector`.
    ///
    /// # Safety
    /// Must run on the COM-initialised render thread.
    unsafe fn resolve_device(
        enumerator: &IMMDeviceEnumerator,
        selector: &DeviceSelector,
    ) -> Result<IMMDevice, CaptureError> {
        unsafe {
            match selector {
                DeviceSelector::Default => enumerator
                    .GetDefaultAudioEndpoint(eRender, eConsole)
                    // No default render endpoint is the "no device" case.
                    .map_err(|_| CaptureError::NoDevice),
                DeviceSelector::Named(name) => {
                    let collection = enumerator
                        .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                        .map_err(backend_err)?;
                    let count = collection.GetCount().map_err(backend_err)?;
                    for i in 0..count {
                        let device = collection.Item(i).map_err(backend_err)?;
                        if friendly_name(&device).as_deref() == Some(name.as_str()) {
                            return Ok(device);
                        }
                    }
                    Err(CaptureError::NoDevice)
                }
            }
        }
    }

    /// Read an endpoint's friendly name, or `None` if it cannot be read.
    ///
    /// # Safety
    /// Must run on the COM-initialised render thread.
    unsafe fn friendly_name(device: &IMMDevice) -> Option<String> {
        unsafe {
            let store = device.OpenPropertyStore(STGM_READ).ok()?;
            let mut prop = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
            let name = match PropVariantToStringAlloc(&prop) {
                Ok(pw) => {
                    let s = pw.to_string().ok();
                    CoTaskMemFree(Some(pw.0.cast()));
                    s
                }
                Err(_) => None,
            };
            let _ = PropVariantClear(&mut prop);
            name
        }
    }

    /// Map a `windows` COM error to a backend capture error, carrying its
    /// Display form (which includes the HRESULT).
    fn backend_err(e: windows::core::Error) -> CaptureError {
        CaptureError::Backend(format!("{e}"))
    }
}

#[cfg(windows)]
pub use win_impl::{PerfModeStream, availability};

/// Non-Windows stub: perf mode is a Windows-only capability. The type exists so
/// callers compile everywhere; [`open`](PerfModeStream::open) always reports
/// [`CaptureError::Unsupported`].
#[cfg(not(windows))]
pub struct PerfModeStream;

#[cfg(not(windows))]
impl PerfModeStream {
    /// Always returns [`CaptureError::Unsupported`] off Windows.
    ///
    /// # Errors
    /// Always [`CaptureError::Unsupported`].
    pub fn open(_config: &PerfModeConfig) -> Result<PerfModeStream, CaptureError> {
        Err(CaptureError::Unsupported(
            "perf mode is Windows-only".to_string(),
        ))
    }

    /// A zeroed [`PerfModeInfo`]; never reached, since `open` never succeeds.
    #[must_use]
    pub fn info(&self) -> PerfModeInfo {
        PerfModeInfo::default()
    }
}

/// Non-Windows stub: perf mode cannot be evaluated off Windows.
#[cfg(not(windows))]
#[must_use]
pub fn availability(_config: &PerfModeConfig) -> PerfModeAvailability {
    PerfModeAvailability::Unsupported("perf mode is Windows-only".to_string())
}

#[cfg(test)]
mod tests {
    use super::{PerfModeAvailability, PerfModeInfo, classify};

    /// An endpoint whose minimum period is below its default is `Available`.
    #[test]
    fn classify_available_when_min_below_default() {
        let info = PerfModeInfo {
            default_period_frames: 480,
            fundamental_period_frames: 128,
            min_period_frames: 128,
            max_period_frames: 480,
            chosen_period_frames: 0,
            sample_rate: 48_000,
            channels: 2,
        };
        match classify(&info) {
            PerfModeAvailability::Available { info: got } => assert_eq!(got, info),
            other => panic!("expected Available, got {other:?}"),
        }
    }

    /// An endpoint whose minimum period equals its default is `DriverLocked` —
    /// the P1 onboard-codec case: no faster period to pull the endpoint down to.
    #[test]
    fn classify_driver_locked_when_min_equals_default() {
        let info = PerfModeInfo {
            default_period_frames: 480,
            fundamental_period_frames: 480,
            min_period_frames: 480,
            max_period_frames: 480,
            chosen_period_frames: 0,
            sample_rate: 48_000,
            channels: 2,
        };
        match classify(&info) {
            PerfModeAvailability::DriverLocked { info: got } => assert_eq!(got, info),
            other => panic!("expected DriverLocked, got {other:?}"),
        }
    }

    /// A degenerate `min > default` still means no usable faster period, so it
    /// classifies as `DriverLocked` rather than `Available`.
    #[test]
    fn classify_driver_locked_when_min_above_default() {
        let info = PerfModeInfo {
            default_period_frames: 240,
            fundamental_period_frames: 240,
            min_period_frames: 480,
            max_period_frames: 480,
            chosen_period_frames: 0,
            sample_rate: 48_000,
            channels: 2,
        };
        assert!(matches!(
            classify(&info),
            PerfModeAvailability::DriverLocked { .. }
        ));
    }

    /// A zero default or minimum period is a degenerate report: `Unsupported`.
    #[test]
    fn classify_unsupported_on_zero_periods() {
        let zero_min = PerfModeInfo {
            default_period_frames: 480,
            min_period_frames: 0,
            ..PerfModeInfo::default()
        };
        assert!(matches!(
            classify(&zero_min),
            PerfModeAvailability::Unsupported(_)
        ));

        let zero_default = PerfModeInfo {
            default_period_frames: 0,
            min_period_frames: 128,
            ..PerfModeInfo::default()
        };
        assert!(matches!(
            classify(&zero_default),
            PerfModeAvailability::Unsupported(_)
        ));

        // The all-zero default (nothing queried) is also Unsupported.
        assert!(matches!(
            classify(&PerfModeInfo::default()),
            PerfModeAvailability::Unsupported(_)
        ));
    }
}
