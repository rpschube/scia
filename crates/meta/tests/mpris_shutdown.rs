//! Regression test for the shutdown hang: the MPRIS backend must stop promptly
//! even while a media player is connected and a property read has wedged.
//!
//! Before the fix, dropping a [`scia_meta::MetaHandle`] joined a backend thread
//! parked forever inside a D-Bus reconcile await — a per-player property read
//! that never returned. The reconcile loop only checks the stop flag *between*
//! passes, so once a pass is stuck inside an unanswered call the flag (and the
//! 1 s safety-net tick, which lives in the select the loop reaches only after
//! reconcile returns) can never be observed, and the join blocks forever. On a
//! machine with no player the backend erred out early into a flag-polled sleep
//! and the join returned at once, which is why the rest of the suite missed it.
//!
//! This test recreates the precondition faithfully: it serves a fake `Playing`
//! MPRIS player on a private bus whose `Metadata` getter answers the first read
//! (so the player is reconciled and a [`MetaEvent::TrackChanged`] is emitted)
//! and then blocks forever on the next read (modelling a player, or bus, that
//! stops answering). It waits until the backend is provably parked inside that
//! wedged read, then stops the handle on a watchdog thread and asserts the stop
//! completes within a hard 5 s deadline. A hang makes the watchdog's
//! `recv_timeout` lapse and the test panic (a clean failure) rather than wedge
//! the runner.
//!
//! Skips cleanly, with a printed message, when `dbus-daemon` is not on `PATH`:
//! some gate runners lack it, and the skip keeps the gate green there.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use scia_meta::MetaEvent;
use scia_meta::mpris;
use zbus::zvariant::Value;

/// A private session bus for the test, killed and reaped on drop so no daemon
/// leaks past the test even on a panic.
struct BusGuard {
    child: Child,
    address: String,
}

impl BusGuard {
    /// Spawn `dbus-daemon` as our own child and read the bus address it prints.
    /// Returns `None` when the binary is not on `PATH` (the caller then skips),
    /// or if it exits without printing an address.
    fn spawn() -> Option<Self> {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let stdout = child.stdout.take()?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        // `--print-address` writes the address on the first line, then the
        // daemon keeps running and serving the bus.
        match reader.read_line(&mut line) {
            Ok(n) if n > 0 => Some(BusGuard {
                child,
                address: line.trim().to_string(),
            }),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                None
            }
        }
    }
}

impl Drop for BusGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The `org.mpris.MediaPlayer2` root interface every player exposes.
struct Root;

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    #[zbus(property)]
    fn identity(&self) -> String {
        "fake".into()
    }
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }
}

/// The `org.mpris.MediaPlayer2.Player` interface. `PlaybackStatus` always
/// answers `Playing`; `Metadata` answers normally until `wedge` is set, then
/// blocks forever — modelling a player that stops answering a property read
/// mid-session. It fires `wedged_tx` the first time it blocks, so the test can
/// wait until the backend is provably parked inside the unanswered call.
struct Player {
    wedge: Arc<AtomicBool>,
    wedged_tx: SyncSender<()>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    #[zbus(property)]
    fn playback_status(&self) -> String {
        "Playing".into()
    }

    #[zbus(property)]
    async fn metadata(&self) -> HashMap<String, Value<'_>> {
        if self.wedge.load(Ordering::SeqCst) {
            // Announce that the backend is now parked here, then never reply —
            // exactly the wedged-call state the shutdown path has to survive.
            let _ = self.wedged_tx.try_send(());
            std::future::pending::<()>().await;
        }
        let mut m = HashMap::new();
        m.insert("xesam:title".to_string(), Value::from("Fake Song"));
        m.insert(
            "xesam:artist".to_string(),
            Value::from(vec!["Fake Artist".to_string()]),
        );
        m.insert("mpris:trackid".to_string(), Value::from("/track/1"));
        m
    }

    #[zbus(property)]
    fn position(&self) -> i64 {
        0
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }
}

/// Serve the fake player on `address` from a dedicated thread whose `block_on`
/// keeps the connection (and its executor) alive for the whole test. Returns a
/// receiver that fires once the player owns its bus name and is answerable. The
/// thread is detached and parks on `pending`; it dies when the test process exits.
fn serve_fake_player(
    address: String,
    wedge: Arc<AtomicBool>,
    wedged_tx: SyncSender<()>,
) -> mpsc::Receiver<()> {
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::Builder::new()
        .name("fake-mpris".into())
        .spawn(move || {
            async_io::block_on(async move {
                let _conn = zbus::connection::Builder::address(address.as_str())
                    .expect("valid bus address")
                    .name("org.mpris.MediaPlayer2.fake")
                    .expect("own the fake player name")
                    .serve_at("/org/mpris/MediaPlayer2", Root)
                    .expect("serve the root interface")
                    .serve_at("/org/mpris/MediaPlayer2", Player { wedge, wedged_tx })
                    .expect("serve the player interface")
                    .build()
                    .await
                    .expect("build the fake player connection");
                let _ = ready_tx.send(());
                // Hold the connection open (and keep this executor driving it)
                // until the process exits.
                std::future::pending::<()>().await;
            });
        })
        .expect("spawn the fake-mpris thread");
    ready_rx
}

#[test]
fn backend_stops_promptly_with_a_wedged_player() {
    let Some(bus) = BusGuard::spawn() else {
        eprintln!(
            "SKIP backend_stops_promptly_with_a_wedged_player: \
             dbus-daemon not on PATH, cannot spawn a private bus to exercise \
             the MPRIS shutdown path"
        );
        return;
    };

    let wedge = Arc::new(AtomicBool::new(false));
    // Capacity 1 is enough; the getter only needs to announce the first block.
    let (wedged_tx, wedged_rx) = mpsc::sync_channel::<()>(1);

    // Bring the fake Playing player up on the private bus first.
    let ready = serve_fake_player(bus.address.clone(), wedge.clone(), wedged_tx);
    ready
        .recv_timeout(Duration::from_secs(10))
        .expect("the fake MPRIS player did not come up within 10s");

    // Start the backend against that same private bus.
    let (tx, rx) = mpsc::channel::<MetaEvent>();
    let handle = mpris::start_at(tx, Some(bus.address.clone()));

    // Wait until the player has been reconciled: a TrackChanged proves the loop
    // read the player's metadata at least once (the wedge precondition).
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut reconciled = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(MetaEvent::TrackChanged(_)) => {
                reconciled = true;
                break;
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    assert!(
        reconciled,
        "backend never reported the fake player (no TrackChanged): the wedge \
         precondition was not established, so the test would not be meaningful"
    );

    // Arm the wedge: the next Metadata read blocks forever. The safety-net tick
    // (or any signal) drives the next reconcile into it.
    wedge.store(true, Ordering::SeqCst);

    // Wait until the backend is provably parked inside the unanswered read, so
    // the stop below exercises the real hang and not merely the select.
    wedged_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("backend never re-read Metadata into the wedge; cannot prove the hang path");

    // The crux: stopping the handle must return quickly even though the reconcile
    // loop is parked in an await that will never complete on its own. Run it on a
    // watchdog thread so a hang makes the deadline lapse and fails the test,
    // rather than blocking the whole run forever.
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        handle.stop();
        let _ = done_tx.send(());
    });

    match done_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(()) => {}
        Err(_) => panic!(
            "MetaHandle::stop() did not return within 5s: the MPRIS backend hung \
             on shutdown while parked in a wedged D-Bus read"
        ),
    }

    // Keep the bus (and thus the live, wedged player) up until here so the
    // backend faced the wedge throughout the stop.
    drop(bus);
}
