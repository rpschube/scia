//! Offline render (`scia --from-file <wav> --output <json|binary>`).
//!
//! Drive an audio *file* through the exact live DSP chain into a feature-stream
//! clip, faster than realtime and **bit-for-bit deterministic**: the same file
//! and the same flags produce byte-identical output. This is what lets the
//! scene-quality golden corpus be regenerated from source audio — the manifest
//! stores the source plus hashes, no committed binaries, and no loopback jitter.
//!
//! The path deliberately does *not* spin up the realtime engine threads. It
//! reuses the same synchronous DSP seam the golden-file tests drive (a
//! [`HopProcessor`] fed period-sized chunks through a [`sample_ring`], each
//! published hop collected as a [`FeatureSnapshot`]), so the features are
//! computed by the identical code the live capture path runs. The only
//! differences from live are that nothing is paced by a clock and the per-hop
//! `timestamp_ns` is the deterministic **sample clock** (hop index × the hop
//! period), never `Instant::now()`.
//!
//! ## Cadence
//!
//! Two cadences matter and they are deliberately different:
//!
//! * **Capture cadence** — the offline input is fed to the ring in
//!   [`PERIOD_FRAMES`]-frame chunks, mirroring the periods a live capture
//!   backend delivers (≈10 ms at 48 kHz). This is a methodological constraint
//!   from the evaluation-methodology review: an offline stream must be chunked
//!   on the same period boundaries the live path uses so the two are comparable.
//!   The emitted frames are invariant to this chunk size (a hop is drained the
//!   moment a full hop is buffered, regardless of push boundaries), so it only
//!   keeps the ring from overflowing and keeps the structure faithful to live.
//! * **Hop cadence** — the DSP grid consumes fixed [`HOP_FRAMES`]-frame hops and
//!   one feature frame is emitted **per hop** (≈5.33 ms at 48 kHz, ~187 fps),
//!   the native analysis cadence. That is richer than the live `--output`
//!   default of 60 fps subsampling, so `--rate` has no meaning here and is
//!   rejected at the CLI.
//!
//! ## Input format
//!
//! WAV only: RIFF/PCM 16- or 24-bit integer, or IEEE 32-bit float; **48 000 Hz**;
//! 1 or 2 channels (a mono file is duplicated to stereo so the chain always sees
//! the stereo shape a live capture delivers). Anything else is a clear error
//! naming the constraint — corpus prep transcodes with external tools.

use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use scia_core::stream::{
    Encoding, FeatureFrame, to_json_line, write_binary_frame, write_binary_header,
};
use scia_core::{DspConfig, FeatureSnapshot, HopProcessor, StreamFormat, sample_ring};

/// The only sample rate the offline path accepts. Corpus prep transcodes
/// anything else with an external tool before rendering, so the DSP always runs
/// at its native rate and the golden numbers stay comparable.
pub const OFFLINE_SAMPLE_RATE: u32 = 48_000;

/// The DSP hop size — one feature frame is emitted per hop. Sourced from
/// [`DspConfig::default`] so it tracks the live pipeline's hop in lock-step.
fn hop_frames() -> usize {
    DspConfig::default().hop_frames
}

/// Capture-period chunk size, in frames: ≈10 ms at 48 kHz. The offline input is
/// pushed to the ring in chunks this size to mirror the cadence a live capture
/// backend delivers (see the module docs). Output is invariant to it.
const PERIOD_FRAMES: usize = 480;

/// The per-hop timestamp step, in nanoseconds: the hop period on the sample
/// clock, `hop_frames / sample_rate`, rounded to the nearest nanosecond. The
/// offline stamp is `hop_index × HOP_PERIOD_NS`, so inter-frame deltas are
/// exactly this constant — a deterministic sample clock, never wall time.
fn hop_period_ns(hop_frames: usize, sample_rate: u32) -> u64 {
    (hop_frames as u64 * 1_000_000_000) / u64::from(sample_rate.max(1))
}

/// Decoded PCM from a WAV file: interleaved `f32` samples in `[-1, 1)` plus the
/// stream shape read from the header.
#[derive(Debug)]
struct WavData {
    sample_rate: u32,
    channels: u16,
    /// Interleaved samples, `channels` per frame.
    samples: Vec<f32>,
}

/// The WAV sample encodings the offline path understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WavEncoding {
    /// PCM signed 16-bit little-endian.
    Pcm16,
    /// PCM signed 24-bit little-endian (3 bytes per sample).
    Pcm24,
    /// IEEE 32-bit float little-endian.
    Float32,
}

/// `WAVE_FORMAT_PCM` audio-format tag.
const WAVE_FORMAT_PCM: u16 = 0x0001;
/// `WAVE_FORMAT_IEEE_FLOAT` audio-format tag.
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
/// `WAVE_FORMAT_EXTENSIBLE` audio-format tag; the real format sits in the
/// subformat GUID's first two bytes.
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Read a little-endian `u16` at `off`, erroring on underrun.
fn read_u16(bytes: &[u8], off: usize) -> Result<u16, String> {
    bytes
        .get(off..off + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or_else(|| "truncated WAV: header ends mid-field".to_string())
}

/// Read a little-endian `u32` at `off`, erroring on underrun.
fn read_u32(bytes: &[u8], off: usize) -> Result<u32, String> {
    bytes
        .get(off..off + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| "truncated WAV: header ends mid-field".to_string())
}

/// Parse a WAV byte buffer into interleaved `f32` PCM, validating the format
/// against the offline constraints. Pure, so it is unit-tested directly.
fn parse_wav(bytes: &[u8]) -> Result<WavData, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file (missing RIFF/WAVE header)".to_string());
    }

    // Walk the chunk list, capturing `fmt ` and `data`.
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // (format, channels, rate, bits)
    let mut data: Option<&[u8]> = None;
    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = read_u32(bytes, pos + 4)? as usize;
        let body_start = pos + 8;
        let body_end = body_start
            .checked_add(size)
            .filter(|&e| e <= bytes.len())
            .ok_or_else(|| "truncated WAV: chunk runs past end of file".to_string())?;
        let body = &bytes[body_start..body_end];
        if id == b"fmt " {
            if body.len() < 16 {
                return Err("malformed WAV: fmt chunk too short".to_string());
            }
            let mut audio_format = read_u16(body, 0)?;
            let channels = read_u16(body, 2)?;
            let sample_rate = read_u32(body, 4)?;
            let bits = read_u16(body, 14)?;
            // WAVE_FORMAT_EXTENSIBLE carries the real tag in the subformat GUID's
            // first two bytes (at offset 24 of the fmt body, after cbSize).
            if audio_format == WAVE_FORMAT_EXTENSIBLE {
                audio_format = read_u16(body, 24).map_err(|_| {
                    "malformed WAV: extensible fmt chunk missing its subformat".to_string()
                })?;
            }
            fmt = Some((audio_format, channels, sample_rate, bits));
        } else if id == b"data" {
            data = Some(body);
        }
        // Chunks are word-aligned: an odd size is followed by a pad byte.
        pos = body_end + (size & 1);
    }

    let (audio_format, channels, sample_rate, bits) =
        fmt.ok_or_else(|| "malformed WAV: no fmt chunk".to_string())?;
    let data = data.ok_or_else(|| "malformed WAV: no data chunk".to_string())?;

    // Validate against the offline constraints, each with a clear, actionable
    // message (corpus prep transcodes to satisfy them).
    if sample_rate != OFFLINE_SAMPLE_RATE {
        return Err(format!(
            "unsupported sample rate {sample_rate} Hz: --from-file requires {OFFLINE_SAMPLE_RATE} Hz \
             (resample with an external tool first)"
        ));
    }
    if channels != 1 && channels != 2 {
        return Err(format!(
            "unsupported channel count {channels}: --from-file requires mono or stereo"
        ));
    }
    let encoding = match (audio_format, bits) {
        (WAVE_FORMAT_PCM, 16) => WavEncoding::Pcm16,
        (WAVE_FORMAT_PCM, 24) => WavEncoding::Pcm24,
        (WAVE_FORMAT_IEEE_FLOAT, 32) => WavEncoding::Float32,
        _ => {
            return Err(format!(
                "unsupported WAV encoding (format 0x{audio_format:04x}, {bits}-bit): --from-file \
                 supports 16- or 24-bit PCM and 32-bit IEEE float"
            ));
        }
    };

    let samples = decode_samples(data, encoding)?;
    Ok(WavData {
        sample_rate,
        channels,
        samples,
    })
}

/// Convert a `data` chunk's bytes into interleaved `f32` in `[-1, 1)`, matching
/// the golden fixtures' `i16 / 32768.0` convention for 16-bit and the analogous
/// full-scale divisor for 24-bit; float samples pass through unchanged.
fn decode_samples(data: &[u8], encoding: WavEncoding) -> Result<Vec<f32>, String> {
    match encoding {
        WavEncoding::Pcm16 => Ok(data
            .chunks_exact(2)
            .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0)
            .collect()),
        WavEncoding::Pcm24 => Ok(data
            .chunks_exact(3)
            .map(|b| {
                // Sign-extend the 24-bit little-endian sample into an i32.
                let raw = i32::from(b[0]) | (i32::from(b[1]) << 8) | (i32::from(b[2]) << 16);
                let signed = (raw << 8) >> 8;
                signed as f32 / 8_388_608.0
            })
            .collect()),
        WavEncoding::Float32 => Ok(data
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect()),
    }
}

/// Duplicate/interleave `wav`'s frames into a stereo buffer and apply `gain`
/// (a linear multiplier) to every sample. A mono file is duplicated L=R so the
/// chain always sees the stereo shape a live capture delivers; a stereo file is
/// carried through as-is. A trailing partial frame in the input is dropped.
fn to_stereo_with_gain(wav: &WavData, gain: f32) -> Vec<f32> {
    let ch = wav.channels as usize;
    let frames = wav.samples.len() / ch;
    let mut out = Vec::with_capacity(frames * 2);
    for f in 0..frames {
        let base = f * ch;
        let left = wav.samples[base] * gain;
        let right = if ch > 1 {
            wav.samples[base + 1] * gain
        } else {
            left
        };
        out.push(left);
        out.push(right);
    }
    out
}

/// Write one frame to `w` in the chosen encoding (an NDJSON line, or a
/// length-prefixed binary payload) via the shared `scia_core::stream` writer.
fn write_frame<W: Write>(w: &mut W, encoding: Encoding, snap: &FeatureSnapshot) -> io::Result<()> {
    let frame = FeatureFrame::from_snapshot(snap);
    match encoding {
        Encoding::Json => {
            let line = to_json_line(&frame).map_err(|e| io::Error::other(e.to_string()))?;
            w.write_all(line.as_bytes())?;
            w.write_all(b"\n")
        }
        Encoding::Binary => write_binary_frame(w, &frame).map_err(|e| match e {
            scia_core::stream::StreamError::Io(io) => io,
            other => io::Error::other(other.to_string()),
        }),
    }
}

/// Render a stereo sample buffer through the DSP chain, writing one frame per
/// hop to `w`. Shared by the CLI entry point and the tests; returns the number
/// of hops emitted so callers can assert the cadence. Deterministic: the ring
/// epoch is inert here (the per-hop stamp is the sample clock, and `try_process`
/// never reads the ring's wall clock), so the output bytes depend only on the
/// samples.
fn render_stereo<W: Write>(
    stereo: &[f32],
    sample_rate: u32,
    encoding: Encoding,
    w: &mut W,
) -> io::Result<u64> {
    let hop = hop_frames();
    let period_ns = hop_period_ns(hop, sample_rate);
    let format = StreamFormat {
        sample_rate,
        channels: 2,
    };

    if encoding == Encoding::Binary {
        write_binary_header(w).map_err(|e| match e {
            scia_core::stream::StreamError::Io(io) => io,
            other => io::Error::other(other.to_string()),
        })?;
    }

    // The synchronous DSP seam: a sample ring the golden tests drive too. The
    // epoch is required by the ring API but never reaches the output — the stamp
    // below is the sample clock and `try_process` does not read the ring's clock.
    let (mut sink, mut consumer) = sample_ring(Instant::now());
    let mut processor = HopProcessor::new(hop, 2, sample_rate);

    let mut hop_index: u64 = 0;
    // Feed the input in capture-period chunks, draining every full hop the push
    // makes available before the next push, exactly as the live capture→DSP
    // handoff does. Output is invariant to the chunk size; the chunking keeps the
    // ring from overflowing and mirrors the live cadence (see the module docs).
    for chunk in stereo.chunks(PERIOD_FRAMES * 2) {
        sink.push(chunk);
        hop_index = drain_hops(
            &mut processor,
            &mut consumer,
            format,
            period_ns,
            hop_index,
            encoding,
            w,
        )?;
    }
    // Any whole hops still buffered after the last push (the loop above already
    // drains after each push, so this is normally a no-op).
    hop_index = drain_hops(
        &mut processor,
        &mut consumer,
        format,
        period_ns,
        hop_index,
        encoding,
        w,
    )?;

    w.flush()?;
    Ok(hop_index)
}

/// Drain and emit every full hop currently buffered in `consumer`, stamping each
/// with the deterministic sample clock (`hop_index × period_ns`). Returns the
/// advanced `hop_index`.
#[allow(clippy::too_many_arguments)]
fn drain_hops<W: Write>(
    processor: &mut HopProcessor,
    consumer: &mut scia_core::SampleConsumer,
    format: StreamFormat,
    period_ns: u64,
    mut hop_index: u64,
    encoding: Encoding,
    w: &mut W,
) -> io::Result<u64> {
    loop {
        let timestamp = hop_index.saturating_mul(period_ns);
        match processor.try_process(consumer, format, timestamp, 0) {
            Some(snap) => {
                write_frame(w, encoding, &snap)?;
                hop_index += 1;
            }
            None => return Ok(hop_index),
        }
    }
}

/// `--from-file` entry point: read and validate the WAV at `path`, apply the
/// linear gain (`gain_db` decibels), render it through the DSP chain, and write
/// the feature stream to stdout in `encoding`.
///
/// Exit codes mirror the rest of the CLI: `0` success, `1` runtime/I-O error,
/// `2` an unsupported or malformed input (the constraint is named in the error).
pub fn run_from_file(path: &Path, encoding: Encoding, gain_db: f32) -> ExitCode {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("--from-file: cannot read the input file: {err}");
            return ExitCode::from(2);
        }
    };
    let wav = match parse_wav(&bytes) {
        Ok(wav) => wav,
        Err(msg) => {
            eprintln!("--from-file: {msg}");
            return ExitCode::from(2);
        }
    };

    let gain = 10f32.powf(gain_db / 20.0);
    let stereo = to_stereo_with_gain(&wav, gain);

    let frames = (wav.samples.len() / wav.channels as usize) as u64;
    eprintln!(
        "rendering {} frames ({:.2}s) through the DSP chain at the native hop cadence",
        frames,
        frames as f64 / f64::from(wav.sample_rate.max(1)),
    );

    let mut out = BufWriter::new(io::stdout());
    match render_stereo(&stereo, wav.sample_rate, encoding, &mut out) {
        Ok(hops) => {
            eprintln!("rendered {hops} feature frames (one per DSP hop)");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("--from-file: output error: {err}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal 16-bit PCM WAV byte buffer from interleaved i16 samples.
    fn wav16(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data_len = samples.len() * 2;
        let byte_rate = sample_rate * u32::from(channels) * 2;
        let block_align = channels * 2;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        b.extend_from_slice(b"WAVE");
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&WAVE_FORMAT_PCM.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&sample_rate.to_le_bytes());
        b.extend_from_slice(&byte_rate.to_le_bytes());
        b.extend_from_slice(&block_align.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data_len as u32).to_le_bytes());
        for &s in samples {
            b.extend_from_slice(&s.to_le_bytes());
        }
        b
    }

    #[test]
    fn parses_a_mono_16bit_48k_wav() {
        let wav = parse_wav(&wav16(48_000, 1, &[0, 16_384, -16_384, 32_767])).expect("parse");
        assert_eq!(wav.sample_rate, 48_000);
        assert_eq!(wav.channels, 1);
        assert_eq!(wav.samples.len(), 4);
        assert!((wav.samples[1] - 0.5).abs() < 1e-4);
        assert!((wav.samples[2] + 0.5).abs() < 1e-4);
    }

    #[test]
    fn parses_a_stereo_wav_interleaved() {
        let wav = parse_wav(&wav16(48_000, 2, &[100, 200, 300, 400])).expect("parse");
        assert_eq!(wav.channels, 2);
        assert_eq!(wav.samples.len(), 4);
    }

    #[test]
    fn rejects_non_48k() {
        let err = parse_wav(&wav16(44_100, 2, &[0, 0])).unwrap_err();
        assert!(err.contains("44100"), "message names the rate: {err}");
        assert!(err.contains("48000"), "message names the constraint: {err}");
    }

    #[test]
    fn rejects_too_many_channels() {
        // Hand-build a 6-channel header (the helper's data need not be coherent).
        let mut bytes = wav16(48_000, 2, &[0, 0]);
        // Patch the channel count field (fmt body offset 2 → absolute 22).
        bytes[22..24].copy_from_slice(&6u16.to_le_bytes());
        let err = parse_wav(&bytes).unwrap_err();
        assert!(err.contains("channel"), "message names channels: {err}");
    }

    #[test]
    fn rejects_unsupported_bit_depth() {
        let mut bytes = wav16(48_000, 1, &[0, 0]);
        // Patch bits-per-sample (fmt body offset 14 → absolute 34) to 8.
        bytes[34..36].copy_from_slice(&8u16.to_le_bytes());
        let err = parse_wav(&bytes).unwrap_err();
        assert!(err.contains("unsupported WAV encoding"), "message: {err}");
    }

    #[test]
    fn rejects_non_wav() {
        let err = parse_wav(b"not a wav at all!!!!").unwrap_err();
        assert!(err.contains("RIFF/WAVE"), "message: {err}");
    }

    #[test]
    fn mono_is_duplicated_to_stereo_at_unit_gain() {
        let wav = parse_wav(&wav16(48_000, 1, &[16_384, -16_384])).expect("parse");
        let stereo = to_stereo_with_gain(&wav, 1.0);
        assert_eq!(stereo.len(), 4);
        assert_eq!(stereo[0], stereo[1], "L and R match for a mono source");
        assert_eq!(stereo[2], stereo[3]);
    }

    #[test]
    fn gain_scales_samples_linearly() {
        let wav = parse_wav(&wav16(48_000, 1, &[16_384])).expect("parse");
        let unit = to_stereo_with_gain(&wav, 1.0);
        let half = to_stereo_with_gain(&wav, 0.5);
        assert!((half[0] - unit[0] * 0.5).abs() < 1e-7);
    }

    #[test]
    fn hop_period_is_the_sample_clock_hop() {
        // 256 frames at 48 kHz ≈ 5.333 ms → 5_333_333 ns to the nearest ns.
        assert_eq!(hop_period_ns(256, 48_000), 5_333_333);
    }

    #[test]
    fn zero_db_gain_is_identity() {
        // The default gain must not perturb samples at all (0 dB → ×1.0).
        assert_eq!(10f32.powf(0.0 / 20.0), 1.0);
    }
}
