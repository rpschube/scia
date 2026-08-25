//! End-to-end coverage for the `scia-bridge` serve path: spawn the bridge on the
//! synthetic feed, connect over TCP, and decode the frames it serves with the
//! library's own wire reader. Mirrors the main binary's `tests/stream_cli.rs`,
//! but exercises the bridge's listener (the extracted `scia_core::stream`
//! serving loop) rather than stdout. No audio hardware — `--demo` drives it.

use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use scia_core::stream::{Encoding, FrameStreamReader};

/// The `scia-bridge` binary under test (cargo sets this for the integration
/// harness of the crate that defines the binary).
const BIN: &str = env!("CARGO_BIN_EXE_scia-bridge");

/// Grab a free TCP port by binding an ephemeral listener and releasing it, so
/// the bridge can bind it a moment later. A small connect-retry in the test
/// absorbs the window between release and re-bind.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

/// Connect to `addr`, retrying briefly while the child process comes up.
fn connect_with_retry(addr: &str) -> TcpStream {
    for _ in 0..100 {
        if let Ok(stream) = TcpStream::connect(addr) {
            return stream;
        }
        sleep(Duration::from_millis(50));
    }
    panic!("bridge never accepted a connection on {addr}");
}

/// `scia-bridge --demo --encoding json` serves valid, parseable NDJSON frames to
/// a connected client, each carrying the current schema, with real (non-seed)
/// synthetic content coming through.
#[test]
fn bridge_serves_parseable_json_frames() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let mut child = Command::new(BIN)
        .args([
            "--demo",
            "--encoding",
            "json",
            "--rate",
            "240",
            "--listen",
            &addr,
        ])
        .spawn()
        .expect("spawn scia-bridge");

    let stream = connect_with_retry(&addr);
    // A read timeout turns a stalled stream into an error the test reports,
    // rather than hanging CI.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");

    let mut reader = FrameStreamReader::new(BufReader::new(stream)).expect("detect encoding");
    assert_eq!(reader.encoding(), Encoding::Json, "json was requested");

    let mut frames = Vec::new();
    while frames.len() < 16 {
        match reader.next_frame() {
            Ok(Some(frame)) => frames.push(frame),
            Ok(None) => break,
            Err(err) => panic!("stream read error after {} frames: {err}", frames.len()),
        }
    }

    child.kill().ok();
    child.wait().ok();

    assert!(frames.len() >= 16, "the bridge served a run of frames");
    assert!(
        frames
            .iter()
            .all(|f| f.schema == scia_core::STREAM_SCHEMA_VERSION),
        "every frame carries the current schema"
    );
    assert!(
        frames
            .iter()
            .any(|f| f.sample_rate == 48_000 && !f.spectrum.is_empty()),
        "the synthetic feed produced real feature content over the bridge"
    );
}

/// `scia-bridge --demo --encoding binary` serves a well-framed binary stream
/// (header + length-prefixed payloads) that round-trips through the reader.
#[test]
fn bridge_serves_binary_frames() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let mut child = Command::new(BIN)
        .args([
            "--demo",
            "--encoding",
            "binary",
            "--rate",
            "240",
            "--listen",
            &addr,
        ])
        .spawn()
        .expect("spawn scia-bridge");

    let stream = connect_with_retry(&addr);
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");

    let mut reader = FrameStreamReader::new(BufReader::new(stream)).expect("valid binary header");
    assert_eq!(reader.encoding(), Encoding::Binary);

    let mut count = 0;
    while count < 16 {
        match reader.next_frame() {
            Ok(Some(frame)) => {
                assert_eq!(frame.schema, scia_core::STREAM_SCHEMA_VERSION);
                count += 1;
            }
            Ok(None) => break,
            Err(err) => panic!("binary decode error after {count} frames: {err}"),
        }
    }

    child.kill().ok();
    child.wait().ok();

    assert!(count >= 16, "every served binary frame decoded back out");
}
