//! End-to-end CLI coverage for the headless `--output` feature stream
//! (US-UX-2), driven by the synthetic demo feed so it needs no audio stack and
//! runs on CI. Frames are parsed back with the library's own wire decoders.

use std::io::Cursor;
use std::process::Command;
use std::time::Instant;

use scia_core::stream::{Encoding, FrameStreamReader, from_json_line};

/// The `scia` binary under test (cargo sets this for the integration harness).
const BIN: &str = env!("CARGO_BIN_EXE_scia");

/// `--demo --output json` emits valid, parseable NDJSON frames — one per line,
/// each carrying the current schema — and the synthetic feed drives real
/// (non-seed) content through.
#[test]
fn output_json_demo_produces_valid_frames() {
    let out = Command::new(BIN)
        .args([
            "--demo", "--output", "json", "--rate", "240", "--frames", "24",
        ])
        .output()
        .expect("spawn scia");
    assert!(out.status.success(), "exit: {:?}", out.status);

    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let frames: Vec<_> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| from_json_line(l).expect("parse + schema-check each line"))
        .collect();

    assert_eq!(frames.len(), 24, "one frame per requested emit");
    assert!(
        frames
            .iter()
            .all(|f| f.schema == scia_core::STREAM_SCHEMA_VERSION),
        "every frame carries the current schema"
    );
    // The synthetic feed is live audio: past the seed frame the format and a
    // non-empty spectrum come through.
    assert!(
        frames
            .iter()
            .any(|f| f.sample_rate == 48_000 && !f.spectrum.is_empty()),
        "the demo feed produced real feature content"
    );
}

/// `--demo --output binary` emits a well-framed binary stream (header + one
/// length-prefixed payload per frame) that round-trips through the reader.
#[test]
fn output_binary_demo_roundtrips() {
    let out = Command::new(BIN)
        .args([
            "--demo", "--output", "binary", "--rate", "240", "--frames", "16",
        ])
        .output()
        .expect("spawn scia");
    assert!(out.status.success(), "exit: {:?}", out.status);

    let mut reader = FrameStreamReader::new(Cursor::new(out.stdout)).expect("valid header");
    assert_eq!(reader.encoding(), Encoding::Binary);
    let mut count = 0;
    while let Some(frame) = reader.next_frame().expect("decode frame") {
        assert_eq!(frame.schema, scia_core::STREAM_SCHEMA_VERSION);
        count += 1;
    }
    assert_eq!(count, 16, "every frame decoded back out");
}

/// `--rate` limits the emission cadence: five frames at 10 fps take at least the
/// four inter-frame gaps (400 ms nominal; asserted with a generous 300 ms floor
/// to tolerate scheduling), proving the stream is paced and not free-running.
#[test]
fn output_rate_is_limited() {
    let start = Instant::now();
    let out = Command::new(BIN)
        .args([
            "--demo", "--output", "json", "--rate", "10", "--frames", "5",
        ])
        .output()
        .expect("spawn scia");
    let elapsed = start.elapsed();
    assert!(out.status.success(), "exit: {:?}", out.status);

    let lines = String::from_utf8(out.stdout)
        .expect("utf8")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    assert_eq!(lines, 5, "exactly the requested number of frames");
    assert!(
        elapsed.as_millis() >= 300,
        "5 frames at 10 fps should be paced (took {elapsed:?}, expected >= 300ms)"
    );
}
