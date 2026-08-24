//! The Linux MPRIS metadata backend.
//!
//! It talks to `org.mpris.MediaPlayer2.*` players over the session bus with
//! [`zbus`], on its own thread, and pushes [`MetaEvent`]s to the channel handed
//! to [`start`]. It is event-driven: it registers D-Bus match rules for
//! `PropertiesChanged` (a player's `Metadata`/`PlaybackStatus` moved) and
//! `NameOwnerChanged` (a player appeared or left), and reconciles the full set
//! of players whenever one fires. A coarse ~1 s timer is only a safety net so a
//! missed signal cannot strand stale state — never the mechanism.
//!
//! # Multi-player policy
//!
//! Several players can be present at once (a browser tab and Spotify, say). The
//! winner is chosen by [`select_winner`]: a `Playing` player outranks a
//! `Paused` one; among equals the most-recently-active player wins (the one
//! whose status or track changed most recently); ties break on the
//! lexicographically smallest bus name, so the choice is deterministic. A
//! `Stopped` player counts as no session. Only the winner's events are emitted;
//! a losing player's changes are dropped, never surfaced as an error.
//!
//! # Absence
//!
//! No session bus, no players, or a D-Bus error are all normal: the backend
//! emits [`MetaEvent::Cleared`] when a previously-reported session goes away
//! and otherwise idles quietly. It never crashes and never log-spams — a
//! machine with no session bus (headless CI) simply produces no events.
//!
//! # Artwork
//!
//! On a track change the backend hands the art reference to a second worker
//! thread (a [`FetchScheduler`]) that debounces, fetches, and retries off the
//! event thread, then emits [`MetaEvent::Artwork`]. Spotify's stale
//! `open.spotify.com/image/` URL form is rewritten to the working
//! `i.scdn.co/image/` CDN form first (see [`rewrite_art_url`]).

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use zbus::zvariant::{OwnedValue, Value};

use crate::fetch::FetchScheduler;
use crate::{ArtworkRef, MetaEvent, MetaHandle, NowPlaying, PlaybackStatus, PositionInfo};

/// The well-known MPRIS object path every player exposes.
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
/// The player interface carrying `Metadata`, `PlaybackStatus` and `Position`.
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
/// The bus-name prefix every MPRIS player owns.
const NAME_PREFIX: &str = "org.mpris.MediaPlayer2.";
/// Safety-net reconcile interval; the mechanism is signals, this only backstops
/// a missed one and bounds how long a drop takes to notice the stop flag.
const FALLBACK_TICK: Duration = Duration::from_secs(1);
/// Cap on a fetched artwork body.
const MAX_ART_BYTES: u64 = 8 * 1024 * 1024;

/// Start the MPRIS backend on its own thread and return a handle that stops and
/// joins it on drop. Events are pushed to `tx`. A machine with no session bus
/// produces no events and never errors.
pub fn start(tx: Sender<MetaEvent>) -> MetaHandle {
    let stop = Arc::new(AtomicBool::new(false));

    let (art_tx, art_rx) = mpsc::channel::<ArtworkJob>();
    let fetch_stop = stop.clone();
    let fetch_tx = tx.clone();
    let fetch_join: JoinHandle<()> = thread::Builder::new()
        .name("scia-meta-art".into())
        .spawn(move || run_fetch_worker(art_rx, fetch_tx, fetch_stop))
        .expect("spawn scia-meta-art thread");

    let backend_stop = stop.clone();
    let backend_join: JoinHandle<()> = thread::Builder::new()
        .name("scia-meta-mpris".into())
        .spawn(move || {
            if run_backend(&tx, &art_tx, &backend_stop).is_err() {
                // No session bus or a fatal D-Bus error: metadata is simply
                // absent. Idle until asked to stop rather than spinning.
                while !backend_stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(200));
                }
            }
            // Dropping `art_tx` at thread exit lets the fetch worker drain and
            // finish once it has been told to stop.
        })
        .expect("spawn scia-meta-mpris thread");

    MetaHandle::new(stop, vec![backend_join, fetch_join])
}

/// A unit of work for the artwork thread: fetch `art` and, on success, emit it
/// tagged with `track_key`.
struct ArtworkJob {
    track_key: String,
    art: ArtworkRef,
}

/// Rewrite Spotify's stale `open.spotify.com/image/<id>` art URL to the working
/// `i.scdn.co/image/<id>` CDN form. Every other URL passes through unchanged.
/// Spotify's Linux client publishes the former, which no longer resolves; the
/// rewrite is absorbed silently so callers never see the quirk.
pub fn rewrite_art_url(url: &str) -> String {
    for prefix in [
        "https://open.spotify.com/image/",
        "http://open.spotify.com/image/",
    ] {
        if let Some(id) = url.strip_prefix(prefix) {
            return format!("https://i.scdn.co/image/{id}");
        }
    }
    url.to_string()
}

/// A player's standing for selection: its bus name, current status, and a
/// monotonic activity stamp (higher = more recently changed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerState {
    /// The player's unique-ish bus name, e.g. `org.mpris.MediaPlayer2.spotify`.
    pub bus_name: String,
    /// The player's current playback status.
    pub status: PlaybackStatus,
    /// A monotonic stamp of when this player last changed; the selector's
    /// most-recent-activity tie-breaker.
    pub activity: u64,
}

fn status_rank(status: PlaybackStatus) -> u8 {
    match status {
        PlaybackStatus::Playing => 2,
        PlaybackStatus::Paused => 1,
        PlaybackStatus::Stopped => 0,
    }
}

/// Choose the winning player per the multi-player policy (see the module docs):
/// highest status rank (Playing > Paused), then most recent activity, then the
/// lexicographically smallest bus name. `Stopped` players are excluded, so an
/// all-stopped or empty set yields `None` (no active session).
pub fn select_winner(players: &[PlayerState]) -> Option<&PlayerState> {
    players
        .iter()
        .filter(|p| status_rank(p.status) > 0)
        .max_by(|a, b| {
            status_rank(a.status)
                .cmp(&status_rank(b.status))
                .then(a.activity.cmp(&b.activity))
                // Smaller bus name wins ties → reverse so it compares "greater".
                .then(b.bus_name.cmp(&a.bus_name))
        })
}

/// The metadata fields we pull out of an MPRIS `Metadata` dict.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedMetadata {
    /// `xesam:title`.
    pub title: Option<String>,
    /// `xesam:artist`, joined with `, ` when the player lists several.
    pub artist: Option<String>,
    /// `xesam:album`.
    pub album: Option<String>,
    /// `mpris:artUrl`, raw (before any backend rewrite).
    pub art_url: Option<String>,
    /// `mpris:length`, converted from microseconds.
    pub length: Option<Duration>,
}

/// Parse an MPRIS `Metadata` `a{sv}` dict into the fields we use. Missing or
/// wrong-typed entries are simply absent — partial metadata is normal.
pub fn parse_metadata(map: &HashMap<String, OwnedValue>) -> ParsedMetadata {
    ParsedMetadata {
        title: map.get("xesam:title").and_then(owned_to_string),
        artist: map.get("xesam:artist").and_then(owned_to_string_list),
        album: map.get("xesam:album").and_then(owned_to_string),
        art_url: map.get("mpris:artUrl").and_then(owned_to_string),
        length: map
            .get("mpris:length")
            .and_then(owned_to_i64)
            .map(|us| Duration::from_micros(us.max(0) as u64)),
    }
}

/// Map an MPRIS `PlaybackStatus` string to the enum; anything unrecognized is
/// treated as `Stopped`.
pub fn parse_status(status: Option<&str>) -> PlaybackStatus {
    match status {
        Some("Playing") => PlaybackStatus::Playing,
        Some("Paused") => PlaybackStatus::Paused,
        _ => PlaybackStatus::Stopped,
    }
}

fn owned_to_string(v: &OwnedValue) -> Option<String> {
    match &**v {
        Value::Str(s) => Some(s.as_str().to_string()),
        _ => None,
    }
}

fn owned_to_string_list(v: &OwnedValue) -> Option<String> {
    match &**v {
        Value::Str(s) => Some(s.as_str().to_string()),
        Value::Array(a) => {
            let mut parts = Vec::new();
            for item in a.iter() {
                if let Value::Str(s) = item {
                    if !s.as_str().is_empty() {
                        parts.push(s.as_str().to_string());
                    }
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(", "))
            }
        }
        _ => None,
    }
}

fn owned_to_i64(v: &OwnedValue) -> Option<i64> {
    match &**v {
        Value::I64(n) => Some(*n),
        Value::U64(n) => Some(*n as i64),
        Value::I32(n) => Some(*n as i64),
        Value::U32(n) => Some(*n as i64),
        Value::I16(n) => Some(*n as i64),
        Value::U16(n) => Some(*n as i64),
        _ => None,
    }
}

/// The event-thread state: what we last emitted, plus per-player activity
/// bookkeeping so the selector can prefer the most-recently-changed player.
struct Reconciler {
    /// Last emitted winner's bus name (`None` = we last emitted `Cleared`).
    last_bus: Option<String>,
    /// Last emitted winner's track key.
    last_key: Option<String>,
    /// Last emitted winner's status.
    last_status: Option<PlaybackStatus>,
    /// Monotonic activity counter.
    activity: u64,
    /// Per-player last-seen `(status, track_key)`, to detect a change.
    snapshots: HashMap<String, (PlaybackStatus, String)>,
    /// Per-player activity stamp, bumped when its snapshot changes.
    activities: HashMap<String, u64>,
}

impl Reconciler {
    fn new() -> Self {
        Self {
            last_bus: None,
            last_key: None,
            last_status: None,
            activity: 0,
            snapshots: HashMap::new(),
            activities: HashMap::new(),
        }
    }
}

fn run_backend(
    tx: &Sender<MetaEvent>,
    art_tx: &Sender<ArtworkJob>,
    stop: &Arc<AtomicBool>,
) -> zbus::Result<()> {
    async_io::block_on(async move {
        let conn = zbus::Connection::session().await?;
        let dbus = zbus::fdo::DBusProxy::new(&conn).await?;

        // Route the two signal classes we care about to this connection. We use
        // the signals only as a wake — correctness comes from the reconcile
        // query, not from parsing the signal body.
        for rule in [
            "type='signal',interface='org.freedesktop.DBus.Properties',\
             member='PropertiesChanged',path='/org/mpris/MediaPlayer2'",
            "type='signal',sender='org.freedesktop.DBus',\
             interface='org.freedesktop.DBus',member='NameOwnerChanged'",
        ] {
            let rule: zbus::MatchRule = rule.try_into()?;
            dbus.add_match_rule(rule).await?;
        }

        let mut stream = zbus::MessageStream::from(conn.clone());
        let mut recon = Reconciler::new();

        // Emit the initial state, then wake on a signal or the safety-net tick.
        reconcile(&mut recon, &conn, &dbus, tx, art_tx).await;
        loop {
            {
                use futures_lite::{FutureExt, StreamExt};
                let signal = async {
                    let _ = stream.next().await;
                };
                let tick = async {
                    async_io::Timer::after(FALLBACK_TICK).await;
                };
                signal.or(tick).await;
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            reconcile(&mut recon, &conn, &dbus, tx, art_tx).await;
        }
        zbus::Result::Ok(())
    })
}

/// Query every player, select the winner, and emit any resulting events.
async fn reconcile(
    recon: &mut Reconciler,
    conn: &zbus::Connection,
    dbus: &zbus::fdo::DBusProxy<'_>,
    tx: &Sender<MetaEvent>,
    art_tx: &Sender<ArtworkJob>,
) {
    let Ok(names) = dbus.list_names().await else {
        return;
    };
    let players: Vec<String> = names
        .iter()
        .map(|n| n.as_str().to_string())
        .filter(|n| n.starts_with(NAME_PREFIX))
        .collect();

    // Forget players that have gone away.
    recon.snapshots.retain(|k, _| players.contains(k));
    recon.activities.retain(|k, _| players.contains(k));

    let mut states = Vec::with_capacity(players.len());
    let mut metas: HashMap<String, (NowPlaying, Option<String>)> = HashMap::new();

    for bus in &players {
        let Ok(proxy) = zbus::Proxy::new(conn, bus.clone(), MPRIS_PATH, PLAYER_IFACE).await else {
            continue;
        };
        let status = proxy.get_property::<String>("PlaybackStatus").await.ok();
        let metadata = proxy
            .get_property::<HashMap<String, OwnedValue>>("Metadata")
            .await
            .unwrap_or_default();
        let position = proxy.get_property::<i64>("Position").await.ok();

        let pstatus = parse_status(status.as_deref());
        let parsed = parse_metadata(&metadata);
        let posinfo = position.map(|us| PositionInfo {
            position: Duration::from_micros(us.max(0) as u64),
            length: parsed.length,
            reported_at: Instant::now(),
        });
        let np = NowPlaying::new(
            parsed.title.clone(),
            parsed.artist.clone(),
            parsed.album.clone(),
            pstatus,
            posinfo,
        );

        // Bump the activity stamp when this player's status or track changed.
        let snap = (pstatus, np.track_key.clone());
        let changed = recon.snapshots.get(bus).is_none_or(|prev| *prev != snap);
        if changed {
            recon.activity += 1;
            recon.activities.insert(bus.clone(), recon.activity);
            recon.snapshots.insert(bus.clone(), snap);
        }
        let activity = recon.activities.get(bus).copied().unwrap_or(0);

        states.push(PlayerState {
            bus_name: bus.clone(),
            status: pstatus,
            activity,
        });
        metas.insert(bus.clone(), (np, parsed.art_url));
    }

    match select_winner(&states).map(|s| s.bus_name.clone()) {
        None => {
            if recon.last_bus.is_some() {
                let _ = tx.send(MetaEvent::Cleared);
                recon.last_bus = None;
                recon.last_key = None;
                recon.last_status = None;
            }
        }
        Some(bus) => {
            let (np, art_url) = metas.get(&bus).cloned().expect("winner has metadata");
            let key_changed = recon.last_key.as_deref() != Some(np.track_key.as_str());
            let bus_changed = recon.last_bus.as_deref() != Some(bus.as_str());
            let status_changed = recon.last_status != Some(np.status);

            if key_changed || bus_changed || status_changed {
                let _ = tx.send(MetaEvent::TrackChanged(np.clone()));
            }
            // Only (re)fetch art when the track itself (or the winning player)
            // changed, not on a mere play/pause.
            if key_changed || bus_changed {
                if let Some(raw) = art_url {
                    let rewritten = rewrite_art_url(&raw);
                    if let Some(art) = ArtworkRef::parse(&rewritten) {
                        let _ = art_tx.send(ArtworkJob {
                            track_key: np.track_key.clone(),
                            art,
                        });
                    }
                }
            }

            recon.last_bus = Some(bus);
            recon.last_key = Some(np.track_key);
            recon.last_status = Some(np.status);
        }
    }
}

/// The artwork thread: owns a [`FetchScheduler`], performs the blocking fetch
/// off the event thread, and emits [`MetaEvent::Artwork`] on success.
fn run_fetch_worker(rx: Receiver<ArtworkJob>, tx: Sender<MetaEvent>, stop: Arc<AtomicBool>) {
    let mut sched = FetchScheduler::default();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Absorb any queued requests.
        loop {
            match rx.try_recv() {
                Ok(job) => sched.request(Instant::now(), job.track_key, job.art),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        if let Some((key, art)) = sched.due(Instant::now()) {
            match fetch_artwork(&art) {
                Some(bytes) if !bytes.is_empty() => {
                    sched.on_success(&key);
                    let _ = tx.send(MetaEvent::Artwork {
                        track_key: key,
                        bytes,
                    });
                }
                _ => sched.on_failure(Instant::now(), &key),
            }
            // Loop back promptly to drain and re-check.
            continue;
        }

        // Wait until the next attempt is due (capped so the stop flag is
        // checked) or a new request arrives.
        let cap = Duration::from_millis(200);
        let wait = sched
            .next_deadline()
            .map(|d| d.saturating_duration_since(Instant::now()))
            .unwrap_or(cap)
            .min(cap);
        match rx.recv_timeout(wait) {
            Ok(job) => sched.request(Instant::now(), job.track_key, job.art),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Fetch the encoded artwork bytes for a reference. `file://` and `data:`
/// resolve with no network; `http(s)` is fetched with a size cap. Any failure
/// returns `None` (the scheduler will retry or abandon).
fn fetch_artwork(art: &ArtworkRef) -> Option<Vec<u8>> {
    match art {
        ArtworkRef::File(path) => std::fs::read(path).ok(),
        ArtworkRef::Inline(bytes) => Some(bytes.clone()),
        ArtworkRef::Url(url) => {
            let resp = ureq::get(url).call().ok()?;
            let mut buf = Vec::new();
            resp.into_reader()
                .take(MAX_ART_BYTES)
                .read_to_end(&mut buf)
                .ok()?;
            Some(buf)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ps(bus: &str, status: PlaybackStatus, activity: u64) -> PlayerState {
        PlayerState {
            bus_name: bus.into(),
            status,
            activity,
        }
    }

    fn owned(v: Value<'static>) -> OwnedValue {
        OwnedValue::try_from(v).unwrap()
    }

    #[test]
    fn no_players_no_winner() {
        assert!(select_winner(&[]).is_none());
    }

    #[test]
    fn all_stopped_no_winner() {
        let players = [
            ps("org.mpris.MediaPlayer2.a", PlaybackStatus::Stopped, 5),
            ps("org.mpris.MediaPlayer2.b", PlaybackStatus::Stopped, 9),
        ];
        assert!(select_winner(&players).is_none());
    }

    #[test]
    fn playing_beats_paused_regardless_of_activity() {
        let players = [
            ps("org.mpris.MediaPlayer2.paused", PlaybackStatus::Paused, 100),
            ps("org.mpris.MediaPlayer2.playing", PlaybackStatus::Playing, 1),
        ];
        assert_eq!(
            select_winner(&players).unwrap().bus_name,
            "org.mpris.MediaPlayer2.playing"
        );
    }

    #[test]
    fn most_recent_activity_wins_among_playing() {
        let players = [
            ps("org.mpris.MediaPlayer2.old", PlaybackStatus::Playing, 3),
            ps("org.mpris.MediaPlayer2.new", PlaybackStatus::Playing, 7),
        ];
        assert_eq!(
            select_winner(&players).unwrap().bus_name,
            "org.mpris.MediaPlayer2.new"
        );
    }

    #[test]
    fn ties_break_lexicographically() {
        // Equal status and activity → smallest bus name wins, deterministically.
        let players = [
            ps("org.mpris.MediaPlayer2.zzz", PlaybackStatus::Playing, 4),
            ps("org.mpris.MediaPlayer2.aaa", PlaybackStatus::Playing, 4),
        ];
        assert_eq!(
            select_winner(&players).unwrap().bus_name,
            "org.mpris.MediaPlayer2.aaa"
        );
    }

    #[test]
    fn paused_winner_when_nothing_playing() {
        let players = [
            ps("org.mpris.MediaPlayer2.a", PlaybackStatus::Stopped, 2),
            ps("org.mpris.MediaPlayer2.b", PlaybackStatus::Paused, 1),
        ];
        assert_eq!(
            select_winner(&players).unwrap().bus_name,
            "org.mpris.MediaPlayer2.b"
        );
    }

    #[test]
    fn rewrite_spotify_stale_art_url() {
        assert_eq!(
            rewrite_art_url("https://open.spotify.com/image/ab12"),
            "https://i.scdn.co/image/ab12"
        );
        assert_eq!(
            rewrite_art_url("http://open.spotify.com/image/ab12"),
            "https://i.scdn.co/image/ab12"
        );
    }

    #[test]
    fn rewrite_leaves_other_urls_alone() {
        assert_eq!(
            rewrite_art_url("https://i.scdn.co/image/ab12"),
            "https://i.scdn.co/image/ab12"
        );
        assert_eq!(rewrite_art_url("file:///tmp/a.png"), "file:///tmp/a.png");
    }

    #[test]
    fn parse_status_maps_known_and_unknown() {
        assert_eq!(parse_status(Some("Playing")), PlaybackStatus::Playing);
        assert_eq!(parse_status(Some("Paused")), PlaybackStatus::Paused);
        assert_eq!(parse_status(Some("Stopped")), PlaybackStatus::Stopped);
        assert_eq!(parse_status(Some("weird")), PlaybackStatus::Stopped);
        assert_eq!(parse_status(None), PlaybackStatus::Stopped);
    }

    #[test]
    fn parse_metadata_full_shape() {
        // A captured Spotify-shaped Metadata a{sv}.
        let mut m: HashMap<String, OwnedValue> = HashMap::new();
        m.insert("xesam:title".into(), owned(Value::from("One More Time")));
        m.insert(
            "xesam:artist".into(),
            owned(Value::from(vec!["Daft Punk", "Someone"])),
        );
        m.insert("xesam:album".into(), owned(Value::from("Discovery")));
        m.insert(
            "mpris:artUrl".into(),
            owned(Value::from("https://open.spotify.com/image/xyz")),
        );
        m.insert("mpris:length".into(), owned(Value::from(320_000_000i64)));

        let p = parse_metadata(&m);
        assert_eq!(p.title.as_deref(), Some("One More Time"));
        assert_eq!(p.artist.as_deref(), Some("Daft Punk, Someone"));
        assert_eq!(p.album.as_deref(), Some("Discovery"));
        assert_eq!(
            p.art_url.as_deref(),
            Some("https://open.spotify.com/image/xyz")
        );
        assert_eq!(p.length, Some(Duration::from_secs(320)));
    }

    #[test]
    fn parse_metadata_partial_is_normal() {
        // Only a title — everything else absent, no error.
        let mut m: HashMap<String, OwnedValue> = HashMap::new();
        m.insert("xesam:title".into(), owned(Value::from("Just A Title")));
        let p = parse_metadata(&m);
        assert_eq!(p.title.as_deref(), Some("Just A Title"));
        assert!(p.artist.is_none());
        assert!(p.album.is_none());
        assert!(p.art_url.is_none());
        assert!(p.length.is_none());
    }

    #[test]
    fn parse_metadata_length_from_u64() {
        let mut m: HashMap<String, OwnedValue> = HashMap::new();
        m.insert("mpris:length".into(), owned(Value::from(1_000_000u64)));
        assert_eq!(parse_metadata(&m).length, Some(Duration::from_secs(1)));
    }

    #[test]
    fn parse_metadata_single_artist_string() {
        // Some players publish xesam:artist as a bare string, not an array.
        let mut m: HashMap<String, OwnedValue> = HashMap::new();
        m.insert("xesam:artist".into(), owned(Value::from("Solo")));
        assert_eq!(parse_metadata(&m).artist.as_deref(), Some("Solo"));
    }

    #[test]
    fn empty_metadata_yields_default() {
        let m: HashMap<String, OwnedValue> = HashMap::new();
        assert_eq!(parse_metadata(&m), ParsedMetadata::default());
    }
}
