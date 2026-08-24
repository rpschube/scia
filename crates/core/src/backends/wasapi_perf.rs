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
}

impl Default for PerfModeConfig {
    fn default() -> Self {
        Self {
            device: DeviceSelector::Default,
        }
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
        MMDeviceEnumerator, eConsole, eRender,
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
            let (tx, rx) = mpsc::channel::<Result<PerfModeInfo, CaptureError>>();

            let join = thread::Builder::new()
                .name("scia-perf-render".into())
                .spawn(move || render_thread(&selector, &stop_thread, &tx))
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

    /// Render-thread entry point: initialise COM for the thread, run the stream,
    /// and tear COM down on exit. All COM objects live and die here.
    fn render_thread(
        selector: &DeviceSelector,
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

            if let Err(e) = run_render(selector, stop, tx) {
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
        stop: &Arc<AtomicBool>,
        tx: &mpsc::Sender<Result<PerfModeInfo, CaptureError>>,
    ) -> Result<(), CaptureError> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(backend_err)?;
            let device = resolve_device(&enumerator, selector)?;
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
                max_period_frames: max_p,
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
pub use win_impl::PerfModeStream;

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
