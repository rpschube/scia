//! Windows-only **event-driven route-change notification**: an
//! [`IMMNotificationClient`] registered with the system device enumerator that
//! flips the engine's existing reopen-request flag the instant the default
//! render endpoint changes or a device is removed.
//!
//! Part A of runtime capture-reopen (`engine.rs`) reacts on a 250 ms route poll
//! plus stream-health faults. This module is part B: on Windows it makes the
//! switch event-driven, so a route change is picked up on the watcher's very
//! next tick instead of waiting out a poll cycle. It plugs into the same seam a
//! poll would — it only sets the reopen-request flag; the `scia-route` watcher
//! then re-resolves the default endpoint and reopens exactly as it does for a
//! polled change.
//!
//! The callback closure is deliberately trivial: it sets a flag and nothing
//! else. It never locks the engine's route mutex and never logs, so it is safe
//! to run on the audio system's own notification threads. A spurious wake is
//! harmless — the watcher's reopen no-ops when the resolved default has not
//! actually moved.
//!
//! [`RouteNotifier::start`] spawns a dedicated `scia-route-notify` thread that
//! initialises COM (MTA), creates the [`IMMDeviceEnumerator`], registers the
//! notification client, and then parks until the [`RouteNotifier`] is dropped.
//! The audio system delivers callbacks on its own threads, so the registration
//! thread only has to keep COM and the registration alive. Dropping the notifier
//! unregisters the client, uninitialises COM on that thread, and joins it.
//!
//! On non-Windows targets — and on Windows without the `route-notify` feature —
//! [`RouteNotifier::start`] returns [`CaptureError::Unsupported`], so callers
//! compile and run everywhere and simply fall back to polling.

// Used by the stub below; the Windows impl imports it inside `win_impl`.
#[cfg(not(all(windows, feature = "route-notify")))]
use crate::capture::CaptureError;

#[cfg(all(windows, feature = "route-notify"))]
#[allow(unsafe_code)]
mod win_impl {
    //! The real implementation. The [`IMMDeviceEnumerator`] and the
    //! [`IMMNotificationClient`] are created, used, and released on the single
    //! `scia-route-notify` thread; nothing COM crosses a thread boundary. The
    //! notification callbacks themselves are invoked by the audio system on its
    //! own threads and only touch the `Send + Sync` change closure.

    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};

    use windows::Win32::Foundation::{PROPERTYKEY, RPC_E_CHANGED_MODE};
    use windows::Win32::Media::Audio::{
        DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow, ERole, IMMDeviceEnumerator,
        IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator, eConsole, eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    use windows::core::{ComObjectInner, IUnknownImpl, PCWSTR, implement};

    use crate::capture::CaptureError;

    /// The change signal the notifier fires. Trivial by contract: it sets the
    /// engine's reopen-request flag and returns.
    type OnChange = Arc<dyn Fn() + Send + Sync>;

    /// A live route-change notifier. Dropping it drops the stop sender (which
    /// unblocks the notify thread), then joins the thread — which unregisters
    /// the notification client and uninitialises COM.
    pub struct RouteNotifier {
        /// Dropped to signal the notify thread to unwind. `Option` so `Drop`
        /// can take and drop it before the join.
        stop_tx: Option<mpsc::Sender<()>>,
        join: Option<JoinHandle<()>>,
    }

    impl RouteNotifier {
        /// Register an [`IMMNotificationClient`] that calls `on_change` whenever
        /// the default render endpoint changes or a device leaves the active
        /// set.
        ///
        /// # Errors
        /// [`CaptureError::Backend`] carrying the HRESULT/message if COM cannot
        /// initialise, the device enumerator cannot be created, or the client
        /// cannot be registered.
        pub fn start(
            on_change: Box<dyn Fn() + Send + Sync>,
        ) -> Result<RouteNotifier, CaptureError> {
            let on_change: OnChange = Arc::from(on_change);
            let (ready_tx, ready_rx) = mpsc::channel::<Result<(), CaptureError>>();
            let (stop_tx, stop_rx) = mpsc::channel::<()>();

            let join = thread::Builder::new()
                .name("scia-route-notify".into())
                .spawn(move || notify_thread(&on_change, &ready_tx, &stop_rx))
                .map_err(|e| {
                    CaptureError::Backend(format!("failed to spawn route-notify thread: {e}"))
                })?;

            match ready_rx.recv() {
                Ok(Ok(())) => Ok(RouteNotifier {
                    stop_tx: Some(stop_tx),
                    join: Some(join),
                }),
                Ok(Err(e)) => {
                    let _ = join.join();
                    Err(e)
                }
                Err(_) => {
                    let _ = join.join();
                    Err(CaptureError::Backend(
                        "route-notify thread exited before reporting".to_string(),
                    ))
                }
            }
        }
    }

    impl Drop for RouteNotifier {
        fn drop(&mut self) {
            // Closing the stop channel unblocks the thread's `recv`, which then
            // unregisters the client and uninitialises COM before exiting.
            drop(self.stop_tx.take());
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    /// Notify-thread entry point: initialise COM for the thread, register and
    /// hold the client, and tear COM down on exit.
    fn notify_thread(
        on_change: &OnChange,
        ready: &mpsc::Sender<Result<(), CaptureError>>,
        stop: &mpsc::Receiver<()>,
    ) {
        // SAFETY: COM is initialised and uninitialised on this thread only, and
        // every COM object created below is confined to this thread.
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            // A fresh thread never has a conflicting apartment, but tolerate
            // RPC_E_CHANGED_MODE defensively: it means COM is already up in a
            // different mode (usable), and that call took no reference, so it
            // must not be balanced with CoUninitialize.
            let com_owned = hr.is_ok();
            if hr.is_err() && hr != RPC_E_CHANGED_MODE {
                let _ = ready.send(Err(CaptureError::Backend(format!(
                    "CoInitializeEx failed: {hr:?}"
                ))));
                return;
            }

            if let Err(e) = run_notify(on_change, ready, stop) {
                let _ = ready.send(Err(e));
            }

            if com_owned {
                CoUninitialize();
            }
        }
    }

    /// Create the enumerator, register the notification client, report readiness
    /// once, then park until `stop` closes and unregister. Returns `Err` only
    /// for a failure *before* the readiness report.
    ///
    /// # Safety
    /// Must run on a thread that has initialised COM (see [`notify_thread`]).
    unsafe fn run_notify(
        on_change: &OnChange,
        ready: &mpsc::Sender<Result<(), CaptureError>>,
        stop: &mpsc::Receiver<()>,
    ) -> Result<(), CaptureError> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(backend_err)?;

            let client: IMMNotificationClient = RouteCallback {
                on_change: Arc::clone(on_change),
            }
            .into_object()
            .into_interface();

            enumerator
                .RegisterEndpointNotificationCallback(&client)
                .map_err(backend_err)?;

            // Registration is live; readiness is reported exactly once here.
            let _ = ready.send(Ok(()));

            // Park until the RouteNotifier is dropped (the stop sender closes).
            // Callbacks fire on the audio system's own threads meanwhile.
            let _ = stop.recv();

            // Unregister before the client and enumerator are released.
            let _ = enumerator.UnregisterEndpointNotificationCallback(&client);
            Ok(())
        }
    }

    /// The registered notification client. Every callback is trivial — it sets
    /// the engine's reopen-request flag through `on_change` and returns S_OK,
    /// never locking or logging.
    #[implement(IMMNotificationClient)]
    struct RouteCallback {
        on_change: OnChange,
    }

    impl IMMNotificationClient_Impl for RouteCallback_Impl {
        /// A device left the active set (removed, unplugged, disabled) — request
        /// a reopen so the watcher re-resolves the default endpoint.
        fn OnDeviceStateChanged(
            &self,
            _device_id: &PCWSTR,
            new_state: DEVICE_STATE,
        ) -> windows::core::Result<()> {
            if new_state != DEVICE_STATE_ACTIVE {
                (self.get_impl().on_change)();
            }
            Ok(())
        }

        /// A device appeared. Not itself a route change; the default only moves
        /// via OnDefaultDeviceChanged. Nothing to do.
        fn OnDeviceAdded(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
            Ok(())
        }

        /// A device was removed — request a reopen.
        fn OnDeviceRemoved(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
            (self.get_impl().on_change)();
            Ok(())
        }

        /// The default endpoint moved. Filter to the render flow and the console
        /// role: the OS fires this once per role (console/multimedia/comms), so
        /// keying on eConsole collapses the up-to-3× burst into a single
        /// request for the endpoint the system mix plays out of.
        fn OnDefaultDeviceChanged(
            &self,
            flow: EDataFlow,
            role: ERole,
            _default_device_id: &PCWSTR,
        ) -> windows::core::Result<()> {
            if flow == eRender && role == eConsole {
                (self.get_impl().on_change)();
            }
            Ok(())
        }

        /// A device property changed — not route-relevant. Nothing to do.
        fn OnPropertyValueChanged(
            &self,
            _device_id: &PCWSTR,
            _key: &PROPERTYKEY,
        ) -> windows::core::Result<()> {
            Ok(())
        }
    }

    /// Map a `windows` COM error to a backend capture error, carrying its
    /// Display form (which includes the HRESULT).
    fn backend_err(e: windows::core::Error) -> CaptureError {
        CaptureError::Backend(format!("{e}"))
    }
}

#[cfg(all(windows, feature = "route-notify"))]
pub use win_impl::RouteNotifier;

/// Stub for platforms/builds without event-driven route notification (every
/// non-Windows target, and Windows without the `route-notify` feature). The type
/// exists so the engine compiles everywhere; [`start`](RouteNotifier::start)
/// always reports [`CaptureError::Unsupported`], and the engine falls back to
/// polling.
#[cfg(not(all(windows, feature = "route-notify")))]
pub struct RouteNotifier;

#[cfg(not(all(windows, feature = "route-notify")))]
impl RouteNotifier {
    /// Always returns [`CaptureError::Unsupported`] off the Windows
    /// `route-notify` build.
    ///
    /// # Errors
    /// Always [`CaptureError::Unsupported`].
    pub fn start(_on_change: Box<dyn Fn() + Send + Sync>) -> Result<RouteNotifier, CaptureError> {
        Err(CaptureError::Unsupported(
            "event-driven route notification is Windows-only".to_string(),
        ))
    }
}
