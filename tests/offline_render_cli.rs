//! End-to-end coverage for the offline render flow (`scia --from-file <wav>
//! --output <json|binary>`): a WAV file driven through the exact live DSP chain
//! into a feature-stream clip, faster than realtime and bit-for-bit
//! deterministic. Exercised by spawning the binary and decoding its output with
//! the shared wire reader — the same bytes `scia --input <clip>` replays.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use scia_core::stream::{Encoding, FrameStreamReader};

/// The `scia` binary under test (cargo sets this for the integration harness).
const BIN: &str = env!("CARGO_BIN_EXE_scia");

/// The DSP hop size and the sample rate the offline path renders at.
const HOP: u64 = 256;
const SAMPLE_RATE: u32 = 48_000;

/// A unique scratch WAV path under the system temp dir.
fn scratch(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "scia-offline-{tag}-{}-{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    p
}

/// Write a minimal 16-bit PCM WAV (mono, 48 kHz) from i16 samples.
fn write_wav16_mono(path: &Path, samples: &[i16]) {
    let sample_rate = SAMPLE_RATE;
    let channels = 1u16;
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = sample_rate * u32::from(channels) * 2;
    let block_align = channels * 2;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVE");
    b.extend_from_slice(b"fmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&sample_rate.to_le_bytes());
    b.extend_from_slice(&byte_rate.to_le_bytes());
    b.extend_from_slice(&block_align.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, b).expect("write WAV");
}

/// A 1 kHz sine at the given peak amplitude with a click (full-scale impulse)
/// every 100 ms — steady RMS plus transients, enough to exercise the whole
/// chain. `frames` long.
fn sine_with_clicks(frames: usize, amp: f32) -> Vec<i16> {
    let two_pi = std::f32::consts::PI * 2.0;
    let click_period = (SAMPLE_RATE / 10) as usize; // every 100 ms
    (0..frames)
        .map(|n| {
            if n % click_period == 0 && n > 0 {
                i16::MAX
            } else {
                let v = amp * (two_pi * 1_000.0 * n as f32 / SAMPLE_RATE as f32).sin();
                (v * 32_767.0) as i16
            }
        })
        .collect()
}

/// Decode every binary frame from `bytes` via the shared wire reader.
fn decode_all(bytes: Vec<u8>) -> Vec<scia_core::stream::FeatureFrame> {
    let mut reader = FrameStreamReader::new(Cursor::new(bytes)).expect("valid header");
    assert_eq!(reader.encoding(), Encoding::Binary);
    let mut out = Vec::new();
    while let Some(frame) = reader.next_frame().expect("decode frame") {
        out.push(frame);
    }
    out
}

/// Run `scia --from-file <path> --output <fmt>` with optional extra args,
/// returning the raw stdout and the exit success flag.
fn render(path: &Path, fmt: &str, extra: &[&str]) -> (Vec<u8>, bool, Option<i32>) {
    let mut args = vec!["--from-file", path.to_str().unwrap(), "--output", fmt];
    args.extend_from_slice(extra);
    let out = Command::new(BIN).args(&args).output().expect("spawn scia");
    (out.stdout, out.status.success(), out.status.code())
}

/// Same file, same flags → byte-identical output. This is the hard determinism
/// requirement the regenerable golden corpus rests on.
#[test]
fn same_file_renders_byte_identical() {
    let path = scratch("determinism");
    write_wav16_mono(&path, &sine_with_clicks(24_000, 0.5));

    let (first, ok1, _) = render(&path, "binary", &[]);
    let (second, ok2, _) = render(&path, "binary", &[]);
    std::fs::remove_file(&path).ok();

    assert!(ok1 && ok2, "both renders succeed");
    assert!(!first.is_empty(), "the render produced output");
    assert_eq!(
        first, second,
        "same file + same flags must be byte-identical"
    );
}

/// The rendered stream is one frame per DSP hop: the frame count matches the
/// file duration divided by the hop, and every inter-frame timestamp delta is
/// exactly the hop period on the sample clock.
#[test]
fn cadence_is_one_frame_per_hop_on_the_sample_clock() {
    let frames_in = 24_000usize;
    let path = scratch("cadence");
    write_wav16_mono(&path, &sine_with_clicks(frames_in, 0.5));
    let (bytes, ok, _) = render(&path, "binary", &[]);
    std::fs::remove_file(&path).ok();
    assert!(ok, "render succeeds");

    let frames = decode_all(bytes);
    let expected = frames_in as u64 / HOP;
    assert_eq!(
        frames.len() as u64,
        expected,
        "one frame per full hop (duration / hop)"
    );

    // The inter-frame timestamp delta is exactly the hop period, every time.
    let hop_period_ns = HOP * 1_000_000_000 / u64::from(SAMPLE_RATE);
    for pair in frames.windows(2) {
        let delta = pair[1].timestamp_ns - pair[0].timestamp_ns;
        assert_eq!(delta, hop_period_ns, "each delta is the sample-clock hop");
    }
    // Every frame carries the current schema and the stereo format.
    assert!(
        frames
            .iter()
            .all(|f| f.schema == scia_core::STREAM_SCHEMA_VERSION)
    );
    assert!(
        frames
            .iter()
            .all(|f| f.sample_rate == SAMPLE_RATE && f.channels == 2)
    );
}

/// The rendered clip round-trips through the shared frame reader frame-for-frame
/// in both encodings — it is a valid clip for `scia --input` and the harness.
#[test]
fn rendered_clip_decodes_frame_for_frame() {
    let path = scratch("compat");
    write_wav16_mono(&path, &sine_with_clicks(12_800, 0.5));

    let (binary, ok_b, _) = render(&path, "binary", &[]);
    let (json, ok_j, _) = render(&path, "json", &[]);
    std::fs::remove_file(&path).ok();
    assert!(ok_b && ok_j, "both encodings render");

    let bin_frames = decode_all(binary);
    let json_frames: Vec<_> = String::from_utf8(json)
        .expect("utf8")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| scia_core::stream::from_json_line(l).expect("parse each line"))
        .collect();

    assert!(!bin_frames.is_empty(), "produced feature content");
    assert_eq!(
        bin_frames.len(),
        json_frames.len(),
        "both encodings emit the same frame count"
    );
    // The two encodings of the same render carry the same generations and stamps.
    for (b, j) in bin_frames.iter().zip(&json_frames) {
        assert_eq!(b.generation, j.generation);
        assert_eq!(b.timestamp_ns, j.timestamp_ns);
    }
}

/// A non-48 kHz input is rejected with a clear error naming the constraint, and
/// an unsupported bit depth likewise — corpus prep transcodes to satisfy them.
#[test]
fn unsupported_format_errors_clearly() {
    // A 44.1 kHz WAV: hand-patch the sample-rate field of a 48 kHz file.
    let path = scratch("badrate");
    write_wav16_mono(&path, &sine_with_clicks(2_560, 0.5));
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[24..28].copy_from_slice(&44_100u32.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let out = Command::new(BIN)
        .args(["--from-file", path.to_str().unwrap(), "--output", "binary"])
        .output()
        .expect("spawn scia");
    std::fs::remove_file(&path).ok();

    assert!(!out.status.success(), "an unsupported rate must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("44100") && stderr.contains("48000"),
        "error names the rate and the constraint: {stderr}"
    );
}

/// `--gain-db` applies a linear gain: rendering the same file at 0 dB and −6 dB
/// halves the linear RMS (−6 dB ≈ ×0.501).
#[test]
fn gain_db_scales_rms() {
    let path = scratch("gain");
    // A steady sine so every hop has a well-defined, nonzero RMS.
    let two_pi = std::f32::consts::PI * 2.0;
    let samples: Vec<i16> = (0..24_000)
        .map(|n| {
            let v = 0.5 * (two_pi * 1_000.0 * n as f32 / SAMPLE_RATE as f32).sin();
            (v * 32_767.0) as i16
        })
        .collect();
    write_wav16_mono(&path, &samples);

    let (full, ok0, _) = render(&path, "binary", &[]);
    let (attenuated, ok6, _) = render(&path, "binary", &["--gain-db", "-6"]);
    std::fs::remove_file(&path).ok();
    assert!(ok0 && ok6, "both renders succeed");

    let mean_rms = |bytes: Vec<u8>| -> f64 {
        let frames = decode_all(bytes);
        let sum: f64 = frames.iter().map(|f| f64::from(f.rms)).sum();
        sum / frames.len().max(1) as f64
    };
    let rms_full = mean_rms(full);
    let rms_att = mean_rms(attenuated);
    assert!(rms_full > 0.0, "the sine has nonzero RMS");
    let ratio = rms_att / rms_full;
    assert!(
        (ratio - 0.501).abs() < 0.03,
        "−6 dB should ~halve the linear RMS (ratio {ratio:.3})"
    );
}

/// `--rate` is meaningless offline (one frame per hop) and is rejected — the
/// clap conflict exits with the usage code without rendering.
#[test]
fn rate_is_rejected_with_from_file() {
    let path = scratch("rate");
    write_wav16_mono(&path, &sine_with_clicks(2_560, 0.5));
    let (_out, ok, code) = render(&path, "binary", &["--rate", "60"]);
    std::fs::remove_file(&path).ok();
    assert!(!ok, "--rate with --from-file must be rejected");
    assert_eq!(code, Some(2), "clap usage error exits 2");
}
