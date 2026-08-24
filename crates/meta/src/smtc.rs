//! The **Windows now-playing backend**, built on the System Media Transport
//! Controls (SMTC) session manager.
//!
//! One dedicated `scia-smtc` thread owns the whole WinRT surface. It requests
//! the [`GlobalSystemMediaTransportControlsSessionManager`], subscribes to
//! `SessionsChanged` on the manager and to `MediaPropertiesChanged` /
//! `PlaybackInfoChanged` on every live session, and turns those callbacks into
//! [`MetaEvent`]s pushed over the shared [`MetaSender`] contract. The design is
//! event-driven per US-META-1 — there is no polling loop, only a ≤5 s
//! safety-net re-check that runs when the manager has been quiet, to paper over
//! any missed notification.
//!
//! **Threading.** WinRT delivers `*Changed` callbacks on its own threadpool
//! threads. Those callbacks do the absolute minimum — push a lightweight marker
//! onto an internal [`mpsc`] channel and return — so a callback never blocks and
//! never touches scia state. All real work (enumerating sessions, applying the
//! selection policy, reading metadata and artwork, emitting events) happens on
//! the single backend thread draining that internal channel. Only [`Send`]
//! values (the internal `Sender`, `String`s) cross into the callbacks; the COM
//! objects never leave the backend thread.
//!
//! **Selection.** The winner is chosen by [`crate::select::select_winner`] over
//! *all* sessions from `GetSessions` — `GetCurrentSession` is only a hint and is
//! not used as policy. Recency is tracked with a monotonic counter bumped each
//! time a session signals activity, so "last activity wins" among playing
//! sessions and ties fall to the lexicographically smallest `AppUserModelId`.
//!
//! **Artwork.** Album art is fetched through [`crate::artwork::ArtworkDriver`]:
//! a ~250 ms debounce then bounded exponential-backoff retries, re-querying the
//! session's thumbnail on each attempt so a late Spotify thumbnail swap is
//! still caught. Bytes are passed through untouched with the session's
//! `AppUserModelId` in [`Artwork::source_app`]; this module never decodes or
//! crops pixels (the palette module owns that, and needs the source app to
//! strip Spotify's letterbox padding).
//!
//! **Absence & failure.** No sessions is the normal idle state: the backend
//! emits [`MetaEvent::Cleared`] and goes quiet. If the manager request itself
//! fails, the backend emits `Cleared` once and exits without panicking — a
//! platform without SMTC is not an error.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as WinStatus,
};
use windows::Storage::Streams::{DataReader, IRandomAccessStreamReference};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::{HSTRING, Result as WinResult};

use crate::artwork::{ArtworkDriver, ArtworkStep, RetryPolicy};
use crate::model::{Artwork, MetaEvent, MetaSender, NowPlaying, PlaybackStatus};
use crate::select::{SessionSnapshot, select_winner};

/// The safety-net re-check interval. The backend is event-driven; this only
/// fires when the internal channel has been idle this long, catching any
/// notification WinRT might have dropped. US-META-1 asks for at most 5 s.
const SAFETY_NET: Duration = Duration::from_secs(5);

/// A live SMTC backend. Dropping it stops the backend thread (which unsubscribes
/// every handler and uninitialises COM) and joins it.
pub struct SmtcBackend {
    /// Sends the stop marker to the backend thread. `Option` so `Drop` can take
    /// it before joining.
    stop: Option<Sender<Internal>>,
    join: Option<JoinHandle<()>>,
}

impl SmtcBackend {
    /// Spawn the backend. It pushes [`MetaEvent`]s over `out` until the returned
    /// handle is dropped. Spawning never blocks on the WinRT request — the
    /// manager is acquired on the backend thread — and never fails: a machine
    /// without an SMTC manager simply emits [`MetaEvent::Cleared`] and idles.
    #[must_use]
    pub fn spawn(out: MetaSender) -> Self {
        Self::spawn_with_policy(out, RetryPolicy::default())
    }

    /// Spawn with an explicit artwork [`RetryPolicy`] (used by tests of the
    /// wiring; production uses [`RetryPolicy::default`] via [`spawn`]).
    ///
    /// [`spawn`]: SmtcBackend::spawn
    #[must_use]
    pub fn spawn_with_policy(out: MetaSender, policy: RetryPolicy) -> Self {
        let (tx, rx) = mpsc::channel::<Internal>();
        let tx_for_thread = tx.clone();
        let join = thread::Builder::new()
            .name("scia-smtc".into())
            .spawn(move || run(&out, &tx_for_thread, &rx, policy))
            .expect("spawn scia-smtc thread");
        Self {
            stop: Some(tx),
            join: Some(join),
        }
    }
}

impl Drop for SmtcBackend {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(Internal::Stop);
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Markers the WinRT callbacks and the safety net push onto the backend
/// thread's internal channel. Everything here is [`Send`].
enum Internal {
    /// The manager's session set changed: re-enumerate and re-subscribe.
    SessionsChanged,
    /// A specific session signalled a metadata/playback change.
    SessionChanged(String),
    /// The safety-net timer elapsed: re-evaluate defensively.
    Recheck,
    /// The handle was dropped: unwind.
    Stop,
}

/// One session's live subscriptions, kept so they can be removed when the
/// session leaves the set or the backend shuts down.
struct SessionSub {
    session: Session,
    /// WinRT event-registration tokens. windows-rs 0.62 represents these as
    /// plain `i64` handles rather than an `EventRegistrationToken` struct.
    media_token: i64,
    playback_token: i64,
}

impl SessionSub {
    fn unsubscribe(&self) {
        let _ = self.session.RemoveMediaPropertiesChanged(self.media_token);
        let _ = self.session.RemovePlaybackInfoChanged(self.playback_token);
    }
}

/// Backend-thread state that persists across notifications.
struct BackendState {
    /// Monotonic recency counter; bumped whenever a session signals activity.
    counter: u64,
    /// Per-`AppUserModelId` recency marker fed to the selection policy.
    activity: HashMap<String, u64>,
    /// The last `NowPlaying` emitted, for de-duplication.
    last_track: Option<NowPlaying>,
    /// Whether the last emission was [`MetaEvent::Cleared`], for de-duplication.
    cleared: bool,
}

impl BackendState {
    fn new() -> Self {
        Self {
            counter: 0,
            activity: HashMap::new(),
            last_track: None,
            cleared: false,
        }
    }

    /// Record fresh activity for `app_id`, making it the most recent.
    fn bump(&mut self, app_id: &str) {
        self.counter += 1;
        self.activity.insert(app_id.to_string(), self.counter);
    }

    /// The recency marker for `app_id`, assigning one if unseen.
    fn recency(&mut self, app_id: &str) -> u64 {
        if let Some(v) = self.activity.get(app_id) {
            *v
        } else {
            self.bump(app_id);
            self.counter
        }
    }
}

/// The backend thread entry point.
fn run(out: &MetaSender, tx: &Sender<Internal>, rx: &Receiver<Internal>, policy: RetryPolicy) {
    // SMTC is delivered on the WinRT threadpool (MTA). Initialise COM in the
    // multithreaded apartment on this thread so async `.join()` calls and event
    // delivery behave. A benign S_FALSE/RPC_E_CHANGED_MODE is ignored.
    // SAFETY: `CoInitializeEx`/`CoUninitialize` are balanced on this one thread;
    // no COM object created here escapes it.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let result = run_inner(out, tx, rx, policy);

    // SAFETY: pairs with the `CoInitializeEx` above on the same thread.
    unsafe {
        CoUninitialize();
    }

    // A manager-request failure is a normal "no SMTC here" state: emit Cleared
    // once and exit quietly.
    if result.is_err() {
        let _ = out.send(MetaEvent::Cleared);
    }
}

/// The fallible body; any WinRT error unwinds to a quiet `Cleared` in [`run`].
fn run_inner(
    out: &MetaSender,
    tx: &Sender<Internal>,
    rx: &Receiver<Internal>,
    policy: RetryPolicy,
) -> WinResult<()> {
    let manager = Manager::RequestAsync()?.join()?;

    // Manager-level: the session set changed.
    let tx_sessions = tx.clone();
    let sessions_token = manager.SessionsChanged(&TypedEventHandler::new(move |_, _| {
        let _ = tx_sessions.send(Internal::SessionsChanged);
        Ok(())
    }))?;

    let mut state = BackendState::new();
    let mut subs: Vec<SessionSub> = Vec::new();

    // Initial subscription + evaluation.
    resubscribe(&manager, tx, &mut subs);
    let mut pending = evaluate(&manager, out, &mut state, rx, policy);

    loop {
        let msg = match pending.take() {
            Some(m) => m,
            None => match rx.recv_timeout(SAFETY_NET) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => Internal::Recheck,
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };

        match msg {
            Internal::Stop => break,
            Internal::SessionsChanged => {
                resubscribe(&manager, tx, &mut subs);
                pending = evaluate(&manager, out, &mut state, rx, policy);
            }
            Internal::SessionChanged(app_id) => {
                state.bump(&app_id);
                pending = evaluate(&manager, out, &mut state, rx, policy);
            }
            Internal::Recheck => {
                pending = evaluate(&manager, out, &mut state, rx, policy);
            }
        }
    }

    // Teardown: drop every per-session subscription and the manager handler.
    for sub in &subs {
        sub.unsubscribe();
    }
    let _ = manager.RemoveSessionsChanged(sessions_token);
    Ok(())
}

/// Remove the old per-session subscriptions and subscribe to the current set.
/// Called on startup and on every `SessionsChanged`.
fn resubscribe(manager: &Manager, tx: &Sender<Internal>, subs: &mut Vec<SessionSub>) {
    for sub in subs.drain(..) {
        sub.unsubscribe();
    }
    let sessions = match manager.GetSessions() {
        Ok(s) => s,
        Err(_) => return,
    };
    let count = sessions.Size().unwrap_or(0);
    for i in 0..count {
        let Ok(session) = sessions.GetAt(i) else {
            continue;
        };
        let app_id = app_id_of(&session);

        let tx_media = tx.clone();
        let app_media = app_id.clone();
        let media_token = session.MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
            let _ = tx_media.send(Internal::SessionChanged(app_media.clone()));
            Ok(())
        }));

        let tx_pb = tx.clone();
        let app_pb = app_id.clone();
        let playback_token = session.PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
            let _ = tx_pb.send(Internal::SessionChanged(app_pb.clone()));
            Ok(())
        }));

        if let (Ok(media_token), Ok(playback_token)) = (media_token, playback_token) {
            subs.push(SessionSub {
                session,
                media_token,
                playback_token,
            });
        }
    }
}

/// Apply the selection policy over the current sessions, emit any metadata
/// change, and run the artwork campaign for a new winner.
///
/// Returns an [`Internal`] message that arrived on `rx` during the (blocking)
/// artwork campaign and must be handled next, so a rapid follow-up change is
/// not lost while album art is being fetched.
fn evaluate(
    manager: &Manager,
    out: &MetaSender,
    state: &mut BackendState,
    rx: &Receiver<Internal>,
    policy: RetryPolicy,
) -> Option<Internal> {
    let sessions = match manager.GetSessions() {
        Ok(s) => s,
        Err(_) => {
            emit_cleared(out, state);
            return None;
        }
    };
    let count = sessions.Size().unwrap_or(0);

    // Build a policy snapshot for every session, remembering the concrete
    // Session handle alongside it so the winner can be re-queried for artwork.
    let mut snaps: Vec<SessionSnapshot> = Vec::with_capacity(count as usize);
    let mut handles: Vec<Session> = Vec::with_capacity(count as usize);
    for i in 0..count {
        let Ok(session) = sessions.GetAt(i) else {
            continue;
        };
        let app_id = app_id_of(&session);
        let status = status_of(&session);
        let recency = state.recency(&app_id);
        snaps.push(SessionSnapshot::new(app_id, status, recency));
        handles.push(session);
    }

    // Prune recency markers for apps that are gone, so the map cannot grow
    // without bound across long sessions.
    let live: std::collections::HashSet<&str> = snaps.iter().map(|s| s.app_id.as_str()).collect();
    state.activity.retain(|k, _| live.contains(k.as_str()));

    let Some(winner_idx) = select_winner(&snaps) else {
        emit_cleared(out, state);
        return None;
    };

    let winner_app = snaps[winner_idx].app_id.clone();
    let winner_session = &handles[winner_idx];

    // Read the winner's textual metadata and emit it if it changed.
    let now = read_now_playing(winner_session, &winner_app);
    let changed = state.last_track.as_ref() != Some(&now);
    if changed || state.cleared {
        let _ = out.send(MetaEvent::Track(now.clone()));
        state.last_track = Some(now);
        state.cleared = false;
    }

    // Fetch artwork only when the track actually changed; a bare playback-state
    // flip does not need a re-fetch.
    if changed {
        return run_artwork_campaign(winner_session, &winner_app, out, rx, policy);
    }
    None
}

/// Emit [`MetaEvent::Cleared`] unless it was already the last thing emitted.
fn emit_cleared(out: &MetaSender, state: &mut BackendState) {
    if !state.cleared {
        let _ = out.send(MetaEvent::Cleared);
        state.cleared = true;
        state.last_track = None;
    }
}

/// Drive the artwork [`ArtworkDriver`] for one track: debounce, then bounded
/// retries, re-querying the thumbnail each attempt. Between the driver's sleeps
/// it drains the internal channel; if a message arrives it abandons the
/// campaign and returns that message so the caller processes it next (a new
/// track supersedes a stale artwork fetch).
fn run_artwork_campaign(
    session: &Session,
    app_id: &str,
    out: &MetaSender,
    rx: &Receiver<Internal>,
    policy: RetryPolicy,
) -> Option<Internal> {
    let mut driver = ArtworkDriver::new(policy);
    loop {
        match driver.next_step() {
            ArtworkStep::Emit(bytes) => {
                let _ = out.send(MetaEvent::Artwork(Artwork {
                    bytes,
                    source_app: Some(app_id.to_string()),
                }));
                return None;
            }
            ArtworkStep::GiveUp => return None,
            ArtworkStep::Fetch { delay } => {
                // Wait the debounce/backoff, but bail out early if a new marker
                // arrives — the callbacks never block, so this is the only place
                // a fresh change is observed mid-campaign.
                match rx.recv_timeout(delay) {
                    Ok(msg) => return Some(msg),
                    Err(RecvTimeoutError::Disconnected) => return None,
                    Err(RecvTimeoutError::Timeout) => {}
                }
                let bytes = fetch_artwork_once(session).ok().flatten();
                driver.record(bytes.as_deref());
            }
        }
    }
}

/// Read the winner's title/artist/album into a [`NowPlaying`], tagging it with
/// the source app. Missing or empty fields become `None`; a failed metadata
/// call yields a snapshot with only the source app set.
fn read_now_playing(session: &Session, app_id: &str) -> NowPlaying {
    let mut np = NowPlaying {
        source_app: Some(app_id.to_string()),
        ..NowPlaying::default()
    };
    if let Ok(props) = session
        .TryGetMediaPropertiesAsync()
        .and_then(|op| op.join())
    {
        np.title = props.Title().ok().and_then(hstr_opt);
        np.artist = props.Artist().ok().and_then(hstr_opt);
        np.album = props.AlbumTitle().ok().and_then(hstr_opt);
    }
    np
}

/// Perform one artwork fetch: re-query the session's media properties, open the
/// thumbnail stream and read all its bytes. `Ok(None)` means "no thumbnail yet"
/// (drives a retry); `Err` is a transient WinRT failure, also treated as a miss.
fn fetch_artwork_once(session: &Session) -> WinResult<Option<Vec<u8>>> {
    let props = session.TryGetMediaPropertiesAsync()?.join()?;
    let thumb: IRandomAccessStreamReference = match props.Thumbnail() {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    read_stream_ref(&thumb)
}

/// Read every byte of an [`IRandomAccessStreamReference`], or `Ok(None)` if the
/// stream is empty.
fn read_stream_ref(reference: &IRandomAccessStreamReference) -> WinResult<Option<Vec<u8>>> {
    let stream = reference.OpenReadAsync()?.join()?;
    let size = stream.Size()?;
    if size == 0 {
        return Ok(None);
    }
    let reader = DataReader::CreateDataReader(&stream)?;
    reader.LoadAsync(size as u32)?.join()?;
    let mut buf = vec![0u8; size as usize];
    reader.ReadBytes(&mut buf)?;
    Ok(Some(buf))
}

/// The session's `AppUserModelId`, or an empty string if it cannot be read.
fn app_id_of(session: &Session) -> String {
    session
        .SourceAppUserModelId()
        .map(|h| h.to_string())
        .unwrap_or_default()
}

/// Map a session's SMTC playback status onto the neutral [`PlaybackStatus`].
fn status_of(session: &Session) -> PlaybackStatus {
    let Ok(raw) = session
        .GetPlaybackInfo()
        .and_then(|info| info.PlaybackStatus())
    else {
        // Unreadable playback info counts as closed.
        return PlaybackStatus::Closed;
    };
    if raw == WinStatus::Playing {
        PlaybackStatus::Playing
    } else if raw == WinStatus::Paused {
        PlaybackStatus::Paused
    } else if raw == WinStatus::Stopped {
        PlaybackStatus::Stopped
    } else if raw == WinStatus::Changing {
        PlaybackStatus::Changing
    } else if raw == WinStatus::Opened {
        PlaybackStatus::Opened
    } else {
        PlaybackStatus::Closed
    }
}

/// Turn an `HSTRING` into `Some(String)`, or `None` if it is empty.
fn hstr_opt(h: HSTRING) -> Option<String> {
    let s = h.to_string();
    if s.is_empty() { None } else { Some(s) }
}
