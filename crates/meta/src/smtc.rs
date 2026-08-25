//! The **Windows now-playing backend**, built on the System Media Transport
//! Controls (SMTC) session manager.
//!
//! One dedicated `scia-smtc` thread owns the whole WinRT surface. It requests
//! the [`GlobalSystemMediaTransportControlsSessionManager`], subscribes to
//! `SessionsChanged` on the manager and to `MediaPropertiesChanged` /
//! `PlaybackInfoChanged` on every live session, and turns those callbacks into
//! [`MetaEvent`]s pushed over the `Sender<MetaEvent>` the caller supplies —
//! exactly the shared backend contract the MPRIS backend also speaks. The
//! design is event-driven per US-META-1 — there is no polling loop, only a
//! ≤5 s safety-net re-check that runs when the manager has been quiet, to paper
//! over any missed notification.
//!
//! **Handle.** [`start`] returns a [`MetaHandle`]; dropping it flips a shared
//! stop flag and joins the backend thread (which unsubscribes every handler and
//! uninitialises COM). The thread polls that flag on a short cadence, so a drop
//! stops it promptly.
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
//! **Metadata.** The winner's title/artist/album and playback status become a
//! [`NowPlaying`] via [`NowPlaying::new`], tagged with the session's
//! `AppUserModelId` as [`NowPlaying::source_app`]. A [`MetaEvent::TrackChanged`]
//! is emitted whenever that snapshot changes (a track change *or* a bare
//! play/pause), matching the shared contract. `position` is left `None`: SMTC
//! exposes a `GetTimelineProperties` timeline that a later change can wire in,
//! but it is not read here yet.
//!
//! **Artwork.** Album art is fetched through [`crate::artwork::ArtworkDriver`]:
//! a ~250 ms debounce then bounded exponential-backoff retries, re-querying the
//! session's thumbnail on each attempt so a late Spotify thumbnail swap is
//! still caught, and only when the track identity (or the winning app) actually
//! changed — never on a bare play/pause. Bytes are passed through untouched as
//! [`MetaEvent::Artwork`] tagged with the track's `track_key` and the session's
//! `AppUserModelId` as `source_app`; this module never decodes or crops pixels
//! (the palette module owns that, and needs the source app to strip Spotify's
//! letterbox padding).
//!
//! **Absence & failure.** No sessions is the normal idle state: the backend
//! emits [`MetaEvent::Cleared`] and goes quiet. If the manager request itself
//! fails, the backend emits `Cleared` once and exits without panicking — a
//! platform without SMTC is not an error.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as WinStatus,
};
use windows::Storage::Streams::{
    DataReader, IRandomAccessStreamReference, IRandomAccessStreamWithContentType,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::{Error as WinError, HSTRING, Result as WinResult};

use crate::artwork::{
    ArtAction, ArtCampaignTracker, ArtworkDriver, ArtworkStep, CampaignOutcome, FetchStage,
    RetryPolicy,
};
use crate::select::{SessionSnapshot, select_winner};
use crate::types::{MetaEvent, MetaHandle, NowPlaying, PlaybackStatus};

/// A diagnostic trace sink used by the `meta_probe` example. Production
/// ([`start`]) installs none; the probe installs one that writes timestamped
/// lines to stderr. The hot path only formats a message when a sink is present.
pub type TraceFn = dyn Fn(&str) + Send;

/// The safety-net re-check interval. The backend is event-driven; this only
/// fires when the internal channel has been idle this long, catching any
/// notification WinRT might have dropped. US-META-1 asks for at most 5 s.
const SAFETY_NET: Duration = Duration::from_secs(5);

/// How long the backend thread will block on the internal channel before
/// looping to re-check the stop flag. It caps the safety-net wait so a dropped
/// [`MetaHandle`] stops the thread within this bound rather than after a full
/// [`SAFETY_NET`] period.
const POLL_CAP: Duration = Duration::from_millis(250);

/// Start the Windows SMTC backend on its own thread and return a
/// [`MetaHandle`] that stops and joins it on drop. Events are pushed to `out`.
/// Spawning never blocks on the WinRT request — the manager is acquired on the
/// backend thread — and never fails: a machine without an SMTC manager simply
/// emits [`MetaEvent::Cleared`] and idles.
#[must_use]
pub fn start(out: Sender<MetaEvent>) -> MetaHandle {
    start_with_policy(out, RetryPolicy::default())
}

/// Start with an explicit artwork [`RetryPolicy`] (used by tests of the wiring;
/// production uses [`RetryPolicy::default`] via [`start`]).
#[must_use]
pub fn start_with_policy(out: Sender<MetaEvent>, policy: RetryPolicy) -> MetaHandle {
    start_inner(out, policy, None)
}

/// Start with a diagnostic trace sink installed. Used by the `meta_probe`
/// example to log session changes, properties events and per-attempt artwork
/// fetch outcomes (including the exact failing WinRT stage and `HRESULT`).
/// Production never calls this.
#[must_use]
pub fn start_traced(
    out: Sender<MetaEvent>,
    policy: RetryPolicy,
    tracer: Box<TraceFn>,
) -> MetaHandle {
    start_inner(out, policy, Some(tracer))
}

fn start_inner(
    out: Sender<MetaEvent>,
    policy: RetryPolicy,
    tracer: Option<Box<TraceFn>>,
) -> MetaHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let join = thread::Builder::new()
        .name("scia-smtc".into())
        .spawn(move || {
            // The internal channel the WinRT callbacks push markers onto lives
            // and dies with this thread; both ends stay on it.
            let (tx, rx) = mpsc::channel::<Internal>();
            // Downgrade the owned box to a plain `&dyn Fn` borrow for the run.
            let tracer_ref = tracer.as_deref();
            run(&out, &thread_stop, &tx, &rx, policy, tracer_ref);
        })
        .expect("spawn scia-smtc thread");
    // No shutdown waker: the backend loop already polls the stop flag every
    // POLL_CAP (250 ms) — its blocking `recv_timeout` is capped to it, and the
    // artwork campaign checks the flag between attempts — so a dropped handle
    // stops the thread promptly without a wake trigger.
    MetaHandle::new(stop, Vec::new(), vec![join])
}

/// Markers the WinRT callbacks and the safety net push onto the backend
/// thread's internal channel. Everything here is [`Send`]. Shutdown is signalled
/// out-of-band by the [`MetaHandle`]'s stop flag, not by a channel message.
enum Internal {
    /// The manager's session set changed: re-enumerate and re-subscribe.
    SessionsChanged,
    /// A session's `MediaPropertiesChanged` fired (title/artist/album/thumbnail
    /// — the signal a late thumbnail swap arrives on). Carries its app id.
    SessionProperties(String),
    /// A session's `PlaybackInfoChanged` fired (play/pause/stop). Carries its
    /// app id. Never itself a reason to refetch art.
    SessionPlayback(String),
    /// The safety-net timer elapsed: re-evaluate defensively.
    Recheck,
}

/// Why [`evaluate`] was invoked, so it can tell a late thumbnail swap (a
/// `MediaPropertiesChanged` for the winner) apart from a play/pause or a
/// structural re-check when deciding whether to re-run an artwork campaign.
#[derive(Debug, Clone)]
enum EvalTrigger {
    /// Startup, `SessionsChanged`, or the safety-net re-check.
    Structural,
    /// A `MediaPropertiesChanged` for the named app.
    Properties(String),
    /// A `PlaybackInfoChanged` for the named app.
    Playback(String),
}

/// The invariant per-evaluation context: where to send events, how to know when
/// to stop, the internal channel to drain mid-campaign, the retry policy, and an
/// optional diagnostic trace sink. Bundled so the hot-path functions stay under
/// the argument-count limit.
struct Ctx<'a> {
    out: &'a Sender<MetaEvent>,
    stop: &'a AtomicBool,
    rx: &'a Receiver<Internal>,
    policy: RetryPolicy,
    tracer: Option<&'a TraceFn>,
}

impl Ctx<'_> {
    /// Emit a diagnostic line, formatting the message only when a sink exists.
    fn trace(&self, args: std::fmt::Arguments) {
        if let Some(t) = self.tracer {
            t(&args.to_string());
        }
    }
}

/// The result of one [`fetch_artwork_once`] attempt, with the failing WinRT
/// stage tagged so a probe can report exactly which step broke.
enum FetchResult {
    /// The thumbnail bytes were read (the driver still judges usability).
    Bytes(Vec<u8>),
    /// The thumbnail stream was empty (size 0) — a normal "not yet" miss.
    Empty,
    /// A WinRT stage failed with this error. A track that simply has no art
    /// surfaces here as a [`FetchStage::Thumbnail`] failure — indistinguishable
    /// at the API from a genuine error, but its `HRESULT` tells them apart in a
    /// probe log; either way the campaign retries then gives up.
    Failed(FetchStage, WinError),
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
    /// The last `NowPlaying` emitted, for de-duplication and change detection.
    last_track: Option<NowPlaying>,
    /// Whether the last emission was [`MetaEvent::Cleared`], for de-duplication.
    cleared: bool,
    /// Per-track artwork-campaign bookkeeping: enables exactly one follow-up
    /// campaign when a player swaps its thumbnail after the first gave up.
    art: ArtCampaignTracker,
}

impl BackendState {
    fn new() -> Self {
        Self {
            counter: 0,
            activity: HashMap::new(),
            last_track: None,
            cleared: false,
            art: ArtCampaignTracker::new(),
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
fn run(
    out: &Sender<MetaEvent>,
    stop: &AtomicBool,
    tx: &Sender<Internal>,
    rx: &Receiver<Internal>,
    policy: RetryPolicy,
    tracer: Option<&TraceFn>,
) {
    // SMTC is delivered on the WinRT threadpool (MTA). Initialise COM in the
    // multithreaded apartment on this thread so async `.join()` calls and event
    // delivery behave. A benign S_FALSE/RPC_E_CHANGED_MODE is ignored.
    // SAFETY: `CoInitializeEx`/`CoUninitialize` are balanced on this one thread;
    // no COM object created here escapes it.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let ctx = Ctx {
        out,
        stop,
        rx,
        policy,
        tracer,
    };
    let result = run_inner(&ctx, tx);

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
fn run_inner(ctx: &Ctx, tx: &Sender<Internal>) -> WinResult<()> {
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
    ctx.trace(format_args!("startup: initial evaluation"));
    let mut pending = evaluate(&manager, &mut state, ctx, &EvalTrigger::Structural);
    let mut last_eval = Instant::now();

    loop {
        if ctx.stop.load(Ordering::Relaxed) {
            break;
        }

        let msg = match pending.take() {
            Some(m) => Some(m),
            None => {
                // Wake on the next internal marker, but never sleep longer than
                // POLL_CAP so the stop flag is observed promptly; only after a
                // full SAFETY_NET of quiet does the timeout mean "re-check".
                let wait = SAFETY_NET.saturating_sub(last_eval.elapsed()).min(POLL_CAP);
                match ctx.rx.recv_timeout(wait) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => {
                        if last_eval.elapsed() >= SAFETY_NET {
                            Some(Internal::Recheck)
                        } else {
                            None
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        };

        let Some(msg) = msg else {
            continue;
        };

        match msg {
            Internal::SessionsChanged => {
                ctx.trace(format_args!("event: sessions-changed"));
                resubscribe(&manager, tx, &mut subs);
                pending = evaluate(&manager, &mut state, ctx, &EvalTrigger::Structural);
            }
            Internal::SessionProperties(app_id) => {
                ctx.trace(format_args!("event: properties-changed app={app_id}"));
                state.bump(&app_id);
                pending = evaluate(&manager, &mut state, ctx, &EvalTrigger::Properties(app_id));
            }
            Internal::SessionPlayback(app_id) => {
                ctx.trace(format_args!("event: playback-changed app={app_id}"));
                state.bump(&app_id);
                pending = evaluate(&manager, &mut state, ctx, &EvalTrigger::Playback(app_id));
            }
            Internal::Recheck => {
                ctx.trace(format_args!("event: safety-net recheck"));
                pending = evaluate(&manager, &mut state, ctx, &EvalTrigger::Structural);
            }
        }
        last_eval = Instant::now();
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
            let _ = tx_media.send(Internal::SessionProperties(app_media.clone()));
            Ok(())
        }));

        let tx_pb = tx.clone();
        let app_pb = app_id.clone();
        let playback_token = session.PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
            let _ = tx_pb.send(Internal::SessionPlayback(app_pb.clone()));
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
/// change, and run the artwork campaign when the winning track (or app) changed.
///
/// Returns an [`Internal`] message that arrived on `rx` during the (blocking)
/// artwork campaign and must be handled next, so a rapid follow-up change is
/// not lost while album art is being fetched.
fn evaluate(
    manager: &Manager,
    state: &mut BackendState,
    ctx: &Ctx,
    trigger: &EvalTrigger,
) -> Option<Internal> {
    let sessions = match manager.GetSessions() {
        Ok(s) => s,
        Err(_) => {
            emit_cleared(ctx.out, state);
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
        emit_cleared(ctx.out, state);
        return None;
    };

    let winner_app = snaps[winner_idx].app_id.clone();
    let winner_status = snaps[winner_idx].status;
    let winner_session = &handles[winner_idx];

    // Read the winner's textual metadata + status into a NowPlaying.
    let now = read_now_playing(winner_session, &winner_app, winner_status);

    // Art is (re)fetched when the track identity or the winning app changed — a
    // bare play/pause updates the snapshot but keeps the same art.
    let prev = state.last_track.as_ref();
    let track_changed = prev.map(|p| p.track_key.as_str()) != Some(now.track_key.as_str());
    let app_changed = prev.and_then(|p| p.source_app.as_deref()) != now.source_app.as_deref();
    let changed = track_changed || app_changed;
    let track_key = now.track_key.clone();

    // A `MediaPropertiesChanged` for the *winning* session is the signal a late
    // thumbnail swap rides in on; the tracker turns it into one follow-up
    // campaign when the previous one gave up art-less.
    let props_for_winner = matches!(trigger, EvalTrigger::Properties(app) if *app == winner_app);

    // Emit whenever the snapshot differs from what we last sent (or we were
    // cleared): this covers a new track, a status flip, or a new winner.
    if prev != Some(&now) || state.cleared {
        let _ = ctx.out.send(MetaEvent::TrackChanged(now.clone()));
        state.cleared = false;
    }
    state.last_track = Some(now);

    let action = state.art.decide(&track_key, changed, props_for_winner);
    if action == ArtAction::Skip {
        return None;
    }
    ctx.trace(format_args!(
        "artwork: {} campaign app={winner_app} track_key={track_key}",
        match action {
            ArtAction::Fresh => "fresh",
            ArtAction::Recampaign => "re-",
            ArtAction::Skip => "skip",
        }
    ));
    state.art.begin(&track_key, action);
    let (outcome, pending) = run_artwork_campaign(winner_session, &winner_app, &track_key, ctx);
    state.art.finish(outcome);
    pending
}

/// Emit [`MetaEvent::Cleared`] unless it was already the last thing emitted.
fn emit_cleared(out: &Sender<MetaEvent>, state: &mut BackendState) {
    if !state.cleared {
        let _ = out.send(MetaEvent::Cleared);
        state.cleared = true;
        state.last_track = None;
    }
}

/// Drive the [`ArtworkDriver`] for one track: debounce, then bounded retries,
/// re-querying the thumbnail each attempt. Between the driver's sleeps it drains
/// the internal channel; if a message arrives it abandons the campaign and
/// returns that message so the caller processes it next (a new track supersedes
/// a stale artwork fetch). It also bails if the stop flag is set.
///
/// Returns the campaign's [`CampaignOutcome`] (so the caller's
/// [`ArtCampaignTracker`] knows whether art was obtained) and any [`Internal`]
/// message that superseded it.
fn run_artwork_campaign(
    session: &Session,
    app_id: &str,
    track_key: &str,
    ctx: &Ctx,
) -> (CampaignOutcome, Option<Internal>) {
    let mut driver = ArtworkDriver::new(ctx.policy);
    let mut attempt: u32 = 0;
    loop {
        if ctx.stop.load(Ordering::Relaxed) {
            return (CampaignOutcome::Abandoned, None);
        }
        match driver.next_step() {
            ArtworkStep::Emit(bytes) => {
                ctx.trace(format_args!(
                    "artwork: emit track_key={track_key} bytes={}",
                    bytes.len()
                ));
                let _ = ctx.out.send(MetaEvent::Artwork {
                    track_key: track_key.to_string(),
                    bytes,
                    source_app: Some(app_id.to_string()),
                });
                return (CampaignOutcome::Emitted, None);
            }
            ArtworkStep::GiveUp => {
                ctx.trace(format_args!(
                    "artwork: give-up track_key={track_key} after {attempt} attempt(s)"
                ));
                return (CampaignOutcome::GaveUp, None);
            }
            ArtworkStep::Fetch { delay } => {
                // Wait the debounce/backoff, but bail out early if a new marker
                // arrives — the callbacks never block, so this is the only place
                // a fresh change is observed mid-campaign.
                match ctx.rx.recv_timeout(delay) {
                    Ok(msg) => return (CampaignOutcome::Abandoned, Some(msg)),
                    Err(RecvTimeoutError::Disconnected) => {
                        return (CampaignOutcome::Abandoned, None);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                }
                attempt += 1;
                let result = fetch_artwork_once(session);
                let bytes: Option<&[u8]> = match &result {
                    FetchResult::Bytes(b) => {
                        ctx.trace(format_args!(
                            "fetch attempt={attempt} stage=read-bytes ok bytes={}",
                            b.len()
                        ));
                        Some(b.as_slice())
                    }
                    FetchResult::Empty => {
                        ctx.trace(format_args!(
                            "fetch attempt={attempt} stage=size miss=empty-stream"
                        ));
                        None
                    }
                    FetchResult::Failed(stage, err) => {
                        ctx.trace(format_args!(
                            "fetch attempt={attempt} stage={} FAILED hresult={} msg={}",
                            stage.label(),
                            err.code(),
                            err.message()
                        ));
                        None
                    }
                };
                driver.record(bytes);
            }
        }
    }
}

/// Read the winner's title/artist/album and status into a [`NowPlaying`],
/// tagging it with the source app. Missing or empty fields become `None`; a
/// failed metadata call yields a snapshot with only status and source app set.
/// `position` is `None` — the SMTC timeline is not read here yet.
fn read_now_playing(session: &Session, app_id: &str, status: PlaybackStatus) -> NowPlaying {
    let mut title = None;
    let mut artist = None;
    let mut album = None;
    if let Ok(props) = session
        .TryGetMediaPropertiesAsync()
        .and_then(|op| op.join())
    {
        title = props.Title().ok().and_then(hstr_opt);
        artist = props.Artist().ok().and_then(hstr_opt);
        album = props.AlbumTitle().ok().and_then(hstr_opt);
    }
    NowPlaying::new(title, artist, album, status, None, Some(app_id.to_string()))
}

/// Perform one artwork fetch: re-query the session's media properties, take the
/// thumbnail reference, open its stream and read every byte. Each WinRT stage's
/// failure is tagged with its [`FetchStage`] so a probe can report exactly which
/// step broke; a missing thumbnail or an empty stream are the ordinary
/// not-ready-yet misses that drive a retry.
fn fetch_artwork_once(session: &Session) -> FetchResult {
    let props = match session
        .TryGetMediaPropertiesAsync()
        .and_then(|op| op.join())
    {
        Ok(p) => p,
        Err(e) => return FetchResult::Failed(FetchStage::Props, e),
    };
    let thumb: IRandomAccessStreamReference = match props.Thumbnail() {
        Ok(t) => t,
        // No thumbnail published (or the getter erred): a miss that drives a
        // retry. Tagged with the stage so a probe sees the exact HRESULT.
        Err(e) => return FetchResult::Failed(FetchStage::Thumbnail, e),
    };
    read_stream_ref(&thumb)
}

/// RAII guard closing an [`IRandomAccessStreamWithContentType`] on every path
/// out of [`read_stream_ref`]. Dropping the windows-rs wrapper only releases the
/// COM reference; the underlying OS stream handle is released by
/// `IClosable::Close`. Without this, each attempt (up to five per track) leaks a
/// platform stream handle, which is what starves the thumbnail path after a
/// few tracks until the process is restarted.
struct StreamGuard<'a>(&'a IRandomAccessStreamWithContentType);
impl Drop for StreamGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.Close();
    }
}

/// RAII guard closing a [`DataReader`]. `DetachStream` first so closing the
/// reader releases only the reader's own resources and does **not** also close
/// the stream the [`StreamGuard`] owns (a `DataReader` otherwise closes its
/// underlying stream on `Close`); the stream is then closed exactly once by its
/// own guard. Detaching after `Close` would be invalid, so the order is
/// detach-then-close, and this guard (declared last) drops before the stream
/// guard, i.e. the dependent reader is torn down before the stream it used.
struct ReaderGuard<'a>(&'a DataReader);
impl Drop for ReaderGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.DetachStream();
        let _ = self.0.Close();
    }
}

/// Read every byte of an [`IRandomAccessStreamReference`], closing the stream and
/// reader on every path out. Returns [`FetchResult::Empty`] for a zero-length
/// stream and tags any WinRT failure with the stage it happened at.
fn read_stream_ref(reference: &IRandomAccessStreamReference) -> FetchResult {
    let stream = match reference.OpenReadAsync().and_then(|op| op.join()) {
        Ok(s) => s,
        Err(e) => return FetchResult::Failed(FetchStage::OpenRead, e),
    };
    // Declared first, dropped last: the stream is closed after the reader.
    let _stream_guard = StreamGuard(&stream);

    let size = match stream.Size() {
        Ok(s) => s,
        Err(e) => return FetchResult::Failed(FetchStage::Size, e),
    };
    if size == 0 {
        return FetchResult::Empty;
    }

    let reader = match DataReader::CreateDataReader(&stream) {
        Ok(r) => r,
        Err(e) => return FetchResult::Failed(FetchStage::CreateReader, e),
    };
    // Declared last, dropped first: the reader is detached + closed before the
    // stream guard closes the stream.
    let _reader_guard = ReaderGuard(&reader);

    if let Err(e) = reader.LoadAsync(size as u32).and_then(|op| op.join()) {
        return FetchResult::Failed(FetchStage::Load, e);
    }
    let mut buf = vec![0u8; size as usize];
    if let Err(e) = reader.ReadBytes(&mut buf) {
        return FetchResult::Failed(FetchStage::ReadBytes, e);
    }
    FetchResult::Bytes(buf)
}

/// The session's `AppUserModelId`, or an empty string if it cannot be read.
fn app_id_of(session: &Session) -> String {
    session
        .SourceAppUserModelId()
        .map(|h| h.to_string())
        .unwrap_or_default()
}

/// Map a session's SMTC playback status onto the neutral [`PlaybackStatus`].
/// The neutral enum has three states, so `Changing`, `Opened` and `Closed` — as
/// well as unreadable playback info — all fold onto `Stopped` (no advancing,
/// no held track worth distinguishing here).
fn status_of(session: &Session) -> PlaybackStatus {
    let Ok(raw) = session
        .GetPlaybackInfo()
        .and_then(|info| info.PlaybackStatus())
    else {
        return PlaybackStatus::Stopped;
    };
    if raw == WinStatus::Playing {
        PlaybackStatus::Playing
    } else if raw == WinStatus::Paused {
        PlaybackStatus::Paused
    } else {
        PlaybackStatus::Stopped
    }
}

/// Turn an `HSTRING` into `Some(String)`, or `None` if it is empty.
fn hstr_opt(h: HSTRING) -> Option<String> {
    let s = h.to_string();
    if s.is_empty() { None } else { Some(s) }
}
