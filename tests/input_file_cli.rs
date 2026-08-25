//! End-to-end coverage for the clip-file recording flow: a
//! `scia --output binary` capture redirected to a file is a valid clip that the
//! shared wire reader decodes frame-for-frame — the same bytes `scia --input
//! <clip>` replays. Driven by the synthetic demo feed so it needs no audio stack
//! and runs on CI. The replay-onto-the-bus half (which drives the TUI, needing a
//! terminal) is covered by the unit tests in `src/stream.rs`.

use std::fs::File;
use std::path::PathBuf;
use std::process::Command;

use scia_core::stream::{Encoding, FrameStreamReader};

/// The `scia` binary under test (cargo sets this for the integration harness).
const BIN: &str = env!("CARGO_BIN_EXE_scia");

/// A unique scratch clip path under the system temp dir.
fn scratch(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "scia-clip-cli-{tag}-{}-{}.bin",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    p
}

/// `scia --demo --output binary > clip.bin` writes a well-formed binary clip:
/// the one-time header plus one length-prefixed frame per emit, every frame
/// decoding back through the reader with the current schema. This is exactly the
/// on-disk form `scia --input <clip>` replays.
#[test]
fn recorded_binary_clip_is_a_valid_replayable_stream() {
    let path = scratch("record");
    let file = File::create(&path).expect("create clip file");
    let status = Command::new(BIN)
        .args([
            "--demo", "--output", "binary", "--rate", "240", "--frames", "16",
        ])
        .stdout(file)
        .status()
        .expect("spawn scia");
    assert!(status.success(), "exit: {status:?}");

    let clip = File::open(&path).expect("reopen clip");
    let mut reader = FrameStreamReader::new(std::io::BufReader::new(clip)).expect("valid header");
    assert_eq!(reader.encoding(), Encoding::Binary);
    let mut count = 0;
    let mut saw_content = false;
    while let Some(frame) = reader.next_frame().expect("decode frame") {
        assert_eq!(frame.schema, scia_core::STREAM_SCHEMA_VERSION);
        if frame.sample_rate == 48_000 && !frame.spectrum.is_empty() {
            saw_content = true;
        }
        count += 1;
    }
    std::fs::remove_file(&path).ok();

    assert_eq!(count, 16, "every recorded frame decodes back out");
    assert!(saw_content, "the demo feed recorded real feature content");
}
