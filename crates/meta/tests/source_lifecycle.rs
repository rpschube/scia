//! Thread-lifecycle test for the audio-source observer, mirroring the metadata
//! backends' clean stop/join contract.
//!
//! [`scia_meta::source::start`] runs one dedicated thread and returns a
//! [`scia_meta::MetaHandle`] whose drop must stop and join that thread promptly
//! — the same contract the SMTC/MPRIS backends honour. This test asserts that a
//! dropped handle returns within a hard deadline rather than hanging, and that
//! the receiver end closes afterward (proving the sender was dropped, i.e. the
//! thread actually exited).
//!
//! It runs on every OS: on Windows the observer polls the real mixer, elsewhere
//! it runs the honest stub thread — either way the poll loop checks the stop
//! flag on a short cadence, so the join is prompt on all platforms.

use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::Duration;

use scia_meta::SourceEvent;
use scia_meta::source;

#[test]
fn observer_stops_and_joins_promptly() {
    let (tx, rx) = mpsc::channel::<SourceEvent>();
    let handle = source::start(tx);

    // Let the thread reach its poll/sleep loop.
    thread::sleep(Duration::from_millis(50));

    // Dropping the handle must set the stop flag and join the worker well within
    // this deadline. Run it on a watchdog so a hang fails the test cleanly rather
    // than wedging the runner.
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        drop(handle);
        let _ = done_tx.send(());
    });

    match done_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(()) => {}
        Err(_) => panic!(
            "dropping the source observer handle did not return within 5s: \
             the observer thread hung on shutdown"
        ),
    }

    // The observer thread has joined, so its sender is dropped: the receiver now
    // reports a disconnected channel (never a live one).
    match rx.try_recv() {
        Ok(_) | Err(TryRecvError::Empty) => {
            // Drain any events that were queued before shutdown, then confirm the
            // channel is closed.
            loop {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(_) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        panic!("channel still open after the observer thread joined")
                    }
                }
            }
        }
        Err(TryRecvError::Disconnected) => {}
    }
}
