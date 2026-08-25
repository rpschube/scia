//! The machine-readable feature stream: a versioned wire form of
//! [`FeatureSnapshot`] and the two framings scia emits and ingests.
//!
//! [`FeatureFrame`] is the serialisable projection of a hop's
//! [`FeatureSnapshot`]. It carries the same fields under stable names, with the
//! display spectrum trimmed to its valid length so a JSON line stays compact.
//! Two encodings share it:
//!
//! * **JSON** — one [`FeatureFrame`] object per line (NDJSON). Every line
//!   carries a `schema` field so a reader can reject a version it does not
//!   understand.
//! * **Binary** — a one-time stream header ([`STREAM_MAGIC`] + a `u16` schema
//!   version + a `u16` reserved word), then `u32`-length-prefixed little-endian
//!   payloads. Round-trips exactly.
//!
//! The wire schema version is [`STREAM_SCHEMA_VERSION`], pinned to the
//! [`FEATURE_SCHEMA_VERSION`] of the snapshot it mirrors: a breaking layout
//! change bumps both. A reader accepts only its own version and rejects any
//! other with [`StreamError::UnsupportedSchema`] rather than mis-parsing.
//!
//! This module is deliberately free of any capture, UI or scripting dependency
//! so another workspace crate (e.g. a future bridge) can reuse the wire form.
//! The document `docs/feature-stream.md` is the human-facing reference for the
//! field list, both framings, and the versioning policy.

use std::io::{self, BufRead, Read, Write};

use serde::{Deserialize, Serialize};

use crate::features::{Activity, FEATURE_SCHEMA_VERSION, FeatureSnapshot, SPECTRUM_BINS};

/// The wire schema version. Pinned to [`FEATURE_SCHEMA_VERSION`]: the frame is a
/// projection of [`FeatureSnapshot`], so any change that bumps the snapshot
/// schema bumps the stream schema in lock-step. A reader rejects any other
/// version.
pub const STREAM_SCHEMA_VERSION: u32 = FEATURE_SCHEMA_VERSION;

/// Magic bytes opening a binary stream: the four ASCII bytes `SCIA`. They also
/// let an ingesting reader auto-detect the encoding — a JSON stream opens with
/// `{` (or whitespace), never `S`.
pub const STREAM_MAGIC: [u8; 4] = *b"SCIA";

/// The binary stream header length: [`STREAM_MAGIC`] (4) + schema `u16` (2) +
/// reserved `u16` (2).
pub const BINARY_HEADER_LEN: usize = 8;

/// A guard on the per-frame binary length prefix, so a corrupt or hostile
/// length cannot make a reader attempt a multi-gigabyte allocation. A real
/// payload is a few kilobytes; this is orders of magnitude above that.
const MAX_BINARY_PAYLOAD: u32 = 1 << 20; // 1 MiB

/// The serialisable projection of one hop's [`FeatureSnapshot`].
///
/// Field names are the stable wire contract (see `docs/feature-stream.md`).
/// Every fixed-size vector in the snapshot is carried as-is except the display
/// spectrum, which is trimmed to its valid length ([`FeatureSnapshot::spectrum_len`])
/// so a line does not carry hundreds of trailing zeros; the length is implicit
/// in the array and restored on decode.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FeatureFrame {
    /// Schema version of this frame; always [`STREAM_SCHEMA_VERSION`] on emit.
    pub schema: u32,
    /// Monotonic hop counter (never resets for the life of the source engine).
    pub generation: u64,
    /// When the hop was processed: ns since the source engine epoch.
    pub timestamp_ns: u64,
    /// Stream sample rate in Hz.
    pub sample_rate: u32,
    /// Stream channel count (1 or 2).
    pub channels: u16,
    /// `true` when the hop was synthesized during capture starvation.
    pub starved: bool,
    /// Coarse activity state (`active` / `quiet` / `idle`).
    pub activity: Activity,
    /// Milliseconds since the last non-quiet hop; `0.0` while active.
    pub quiet_ms: f32,
    /// Cumulative frames dropped to ring overflow, as of this hop.
    pub dropped_frames: u64,
    /// RMS level of the hop over the mono mix (`0.0..=1.0` for in-range audio).
    pub rms: f32,
    /// Peak absolute sample over the hop (`0.0..=1.0` for in-range audio).
    pub peak: f32,
    /// Momentary loudness (LUFS). Reserved, 0 in schema 1.
    pub lufs_momentary: f32,
    /// Display spectrum: the valid log-spaced bars in `0.0..=1.0`. Its length is
    /// the snapshot's `spectrum_len`; never more than [`SPECTRUM_BINS`].
    pub spectrum: Vec<f32>,
    /// Bass / mid / treble band levels, each normalised to its own average.
    pub bands: [f32; 3],
    /// Half-wave-rectified spectral flux, normalised (`0.0..=1.0`).
    pub flux: f32,
    /// `true` when an onset was detected on this hop.
    pub onset: bool,
    /// Milliseconds since the last onset, saturating at `60_000.0`.
    pub onset_age_ms: f32,
    /// Beat phase in `0.0..1.0`; `0.0` while unlocked.
    pub beat_phase: f32,
    /// Beat-tracker confidence in `0.0..=1.0`.
    pub beat_confidence: f32,
    /// Estimated tempo in BPM; `0.0` while unlocked.
    pub tempo_bpm: f32,
    /// Inter-channel correlation in `-1.0..=1.0`. Reserved, 0 in schema 1.
    pub stereo_correlation: f32,
    /// Mid/side energy ratio. Reserved, 0 in schema 1.
    pub mid_side_ratio: f32,
    /// 12-bin chroma vector. Reserved (all 0) in schema 1.
    pub chroma: [f32; 12],
}

impl FeatureFrame {
    /// Project a [`FeatureSnapshot`] onto the wire frame. The display spectrum is
    /// trimmed to the snapshot's valid length.
    #[must_use]
    pub fn from_snapshot(snap: &FeatureSnapshot) -> Self {
        let len = (snap.spectrum_len as usize).min(SPECTRUM_BINS);
        Self {
            schema: snap.schema_version,
            generation: snap.generation,
            timestamp_ns: snap.timestamp_ns,
            sample_rate: snap.sample_rate,
            channels: snap.channels,
            starved: snap.starved,
            activity: snap.activity,
            quiet_ms: snap.quiet_ms,
            dropped_frames: snap.dropped_frames,
            rms: snap.rms,
            peak: snap.peak,
            lufs_momentary: snap.lufs_momentary,
            spectrum: snap.spectrum[..len].to_vec(),
            bands: snap.bands,
            flux: snap.flux,
            onset: snap.onset,
            onset_age_ms: snap.onset_age_ms,
            beat_phase: snap.beat_phase,
            beat_confidence: snap.beat_confidence,
            tempo_bpm: snap.tempo_bpm,
            stereo_correlation: snap.stereo_correlation,
            mid_side_ratio: snap.mid_side_ratio,
            chroma: snap.chroma,
        }
    }

    /// Rebuild a [`FeatureSnapshot`] from the wire frame. The spectrum vector is
    /// copied into the fixed array (clamped to [`SPECTRUM_BINS`]) and
    /// `spectrum_len` set to its length; trailing bars are zero.
    #[must_use]
    pub fn to_snapshot(&self) -> FeatureSnapshot {
        let len = self.spectrum.len().min(SPECTRUM_BINS);
        let mut spectrum = [0.0f32; SPECTRUM_BINS];
        spectrum[..len].copy_from_slice(&self.spectrum[..len]);
        FeatureSnapshot {
            schema_version: self.schema,
            generation: self.generation,
            timestamp_ns: self.timestamp_ns,
            sample_rate: self.sample_rate,
            channels: self.channels,
            starved: self.starved,
            activity: self.activity,
            quiet_ms: self.quiet_ms,
            dropped_frames: self.dropped_frames,
            rms: self.rms,
            peak: self.peak,
            lufs_momentary: self.lufs_momentary,
            spectrum,
            spectrum_len: len as u16,
            bands: self.bands,
            flux: self.flux,
            onset: self.onset,
            onset_age_ms: self.onset_age_ms,
            beat_phase: self.beat_phase,
            beat_confidence: self.beat_confidence,
            tempo_bpm: self.tempo_bpm,
            stereo_correlation: self.stereo_correlation,
            mid_side_ratio: self.mid_side_ratio,
            chroma: self.chroma,
        }
    }
}

/// A wire encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// NDJSON: one [`FeatureFrame`] object per line.
    Json,
    /// Length-prefixed little-endian binary with a one-time header.
    Binary,
}

/// Something that went wrong reading or writing the stream.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// An I/O error on the underlying reader or writer.
    #[error("stream I/O error: {0}")]
    Io(#[from] io::Error),
    /// A JSON line could not be parsed into a [`FeatureFrame`].
    #[error("malformed JSON frame: {0}")]
    Json(#[from] serde_json::Error),
    /// The binary header did not open with [`STREAM_MAGIC`].
    #[error("not a scia binary stream (bad magic)")]
    BadMagic,
    /// The stream (header or a frame) declares a schema version this build does
    /// not understand.
    #[error("unsupported feature-stream schema {found}; this build speaks {expected}")]
    UnsupportedSchema {
        /// The version the stream declared.
        found: u32,
        /// The version this build accepts ([`STREAM_SCHEMA_VERSION`]).
        expected: u32,
    },
    /// A binary payload was truncated (the length prefix outran the data).
    #[error("truncated binary frame")]
    Truncated,
    /// A binary length prefix exceeded [`MAX_BINARY_PAYLOAD`].
    #[error("binary frame length {0} exceeds the {MAX_BINARY_PAYLOAD}-byte cap")]
    FrameTooLarge(u32),
}

/// Reject a frame whose schema this build does not speak.
fn check_schema(found: u32) -> Result<(), StreamError> {
    if found == STREAM_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(StreamError::UnsupportedSchema {
            found,
            expected: STREAM_SCHEMA_VERSION,
        })
    }
}

// ---------------------------------------------------------------------------
// JSON (NDJSON)
// ---------------------------------------------------------------------------

/// Serialise a frame to a single NDJSON line (no trailing newline).
///
/// # Errors
/// [`StreamError::Json`] if serialisation fails (it does not for a valid frame).
pub fn to_json_line(frame: &FeatureFrame) -> Result<String, StreamError> {
    Ok(serde_json::to_string(frame)?)
}

/// Parse one NDJSON line into a frame, rejecting an unrecognised schema.
///
/// # Errors
/// [`StreamError::Json`] on malformed JSON, or [`StreamError::UnsupportedSchema`]
/// when the line's `schema` is not [`STREAM_SCHEMA_VERSION`].
pub fn from_json_line(line: &str) -> Result<FeatureFrame, StreamError> {
    let frame: FeatureFrame = serde_json::from_str(line)?;
    check_schema(frame.schema)?;
    Ok(frame)
}

// ---------------------------------------------------------------------------
// Binary framing
// ---------------------------------------------------------------------------

/// Write the one-time binary stream header (magic + schema + reserved word).
///
/// # Errors
/// Propagates any writer I/O error.
pub fn write_binary_header<W: Write>(w: &mut W) -> Result<(), StreamError> {
    let mut header = [0u8; BINARY_HEADER_LEN];
    header[..4].copy_from_slice(&STREAM_MAGIC);
    header[4..6].copy_from_slice(&(STREAM_SCHEMA_VERSION as u16).to_le_bytes());
    // header[6..8] reserved (zero).
    w.write_all(&header)?;
    Ok(())
}

/// Read and validate a binary stream header, returning the stream's schema
/// version.
///
/// # Errors
/// [`StreamError::BadMagic`] if the magic is wrong, or
/// [`StreamError::UnsupportedSchema`] if the version is unrecognised.
pub fn read_binary_header<R: Read>(r: &mut R) -> Result<u32, StreamError> {
    let mut header = [0u8; BINARY_HEADER_LEN];
    r.read_exact(&mut header)?;
    if header[..4] != STREAM_MAGIC {
        return Err(StreamError::BadMagic);
    }
    let schema = u32::from(u16::from_le_bytes([header[4], header[5]]));
    check_schema(schema)?;
    Ok(schema)
}

/// Encode a frame's fixed little-endian payload (no length prefix).
#[must_use]
pub fn encode_binary_payload(frame: &FeatureFrame) -> Vec<u8> {
    let mut out = Vec::with_capacity(160 + frame.spectrum.len() * 4);
    out.extend_from_slice(&frame.schema.to_le_bytes());
    out.extend_from_slice(&frame.generation.to_le_bytes());
    out.extend_from_slice(&frame.timestamp_ns.to_le_bytes());
    out.extend_from_slice(&frame.sample_rate.to_le_bytes());
    out.extend_from_slice(&frame.channels.to_le_bytes());
    out.push(u8::from(frame.starved));
    out.push(frame.activity as u8);
    out.extend_from_slice(&frame.quiet_ms.to_le_bytes());
    out.extend_from_slice(&frame.dropped_frames.to_le_bytes());
    out.extend_from_slice(&frame.rms.to_le_bytes());
    out.extend_from_slice(&frame.peak.to_le_bytes());
    out.extend_from_slice(&frame.lufs_momentary.to_le_bytes());
    // Spectrum: a u16 count then that many f32 bars.
    let len = frame.spectrum.len().min(SPECTRUM_BINS) as u16;
    out.extend_from_slice(&len.to_le_bytes());
    for &bar in &frame.spectrum[..len as usize] {
        out.extend_from_slice(&bar.to_le_bytes());
    }
    for &b in &frame.bands {
        out.extend_from_slice(&b.to_le_bytes());
    }
    out.extend_from_slice(&frame.flux.to_le_bytes());
    out.push(u8::from(frame.onset));
    out.extend_from_slice(&frame.onset_age_ms.to_le_bytes());
    out.extend_from_slice(&frame.beat_phase.to_le_bytes());
    out.extend_from_slice(&frame.beat_confidence.to_le_bytes());
    out.extend_from_slice(&frame.tempo_bpm.to_le_bytes());
    out.extend_from_slice(&frame.stereo_correlation.to_le_bytes());
    out.extend_from_slice(&frame.mid_side_ratio.to_le_bytes());
    for &c in &frame.chroma {
        out.extend_from_slice(&c.to_le_bytes());
    }
    out
}

/// A little cursor over a byte slice that yields fixed-width little-endian
/// scalars, erroring on underrun rather than panicking.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], StreamError> {
        let end = self.pos.checked_add(N).ok_or(StreamError::Truncated)?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(StreamError::Truncated)?;
        let mut buf = [0u8; N];
        buf.copy_from_slice(slice);
        self.pos = end;
        Ok(buf)
    }

    fn u16(&mut self) -> Result<u16, StreamError> {
        Ok(u16::from_le_bytes(self.take::<2>()?))
    }
    fn u32(&mut self) -> Result<u32, StreamError> {
        Ok(u32::from_le_bytes(self.take::<4>()?))
    }
    fn u64(&mut self) -> Result<u64, StreamError> {
        Ok(u64::from_le_bytes(self.take::<8>()?))
    }
    fn f32(&mut self) -> Result<f32, StreamError> {
        Ok(f32::from_le_bytes(self.take::<4>()?))
    }
    fn u8(&mut self) -> Result<u8, StreamError> {
        Ok(self.take::<1>()?[0])
    }
    fn bool(&mut self) -> Result<bool, StreamError> {
        Ok(self.u8()? != 0)
    }
}

/// Decode a frame from a fixed little-endian payload, rejecting an unrecognised
/// schema.
///
/// # Errors
/// [`StreamError::Truncated`] if the payload underruns, or
/// [`StreamError::UnsupportedSchema`] on an unrecognised schema.
pub fn decode_binary_payload(bytes: &[u8]) -> Result<FeatureFrame, StreamError> {
    let mut c = Cursor::new(bytes);
    let schema = c.u32()?;
    check_schema(schema)?;
    let generation = c.u64()?;
    let timestamp_ns = c.u64()?;
    let sample_rate = c.u32()?;
    let channels = c.u16()?;
    let starved = c.bool()?;
    let activity = match c.u8()? {
        1 => Activity::Quiet,
        2 => Activity::Idle,
        _ => Activity::Active,
    };
    let quiet_ms = c.f32()?;
    let dropped_frames = c.u64()?;
    let rms = c.f32()?;
    let peak = c.f32()?;
    let lufs_momentary = c.f32()?;
    let spec_len = (c.u16()? as usize).min(SPECTRUM_BINS);
    let mut spectrum = Vec::with_capacity(spec_len);
    for _ in 0..spec_len {
        spectrum.push(c.f32()?);
    }
    let bands = [c.f32()?, c.f32()?, c.f32()?];
    let flux = c.f32()?;
    let onset = c.bool()?;
    let onset_age_ms = c.f32()?;
    let beat_phase = c.f32()?;
    let beat_confidence = c.f32()?;
    let tempo_bpm = c.f32()?;
    let stereo_correlation = c.f32()?;
    let mid_side_ratio = c.f32()?;
    let mut chroma = [0.0f32; 12];
    for slot in &mut chroma {
        *slot = c.f32()?;
    }
    Ok(FeatureFrame {
        schema,
        generation,
        timestamp_ns,
        sample_rate,
        channels,
        starved,
        activity,
        quiet_ms,
        dropped_frames,
        rms,
        peak,
        lufs_momentary,
        spectrum,
        bands,
        flux,
        onset,
        onset_age_ms,
        beat_phase,
        beat_confidence,
        tempo_bpm,
        stereo_correlation,
        mid_side_ratio,
        chroma,
    })
}

/// Write one length-prefixed binary frame (`u32` little-endian length, then the
/// payload). The header must already have been written once.
///
/// # Errors
/// Propagates any writer I/O error.
pub fn write_binary_frame<W: Write>(w: &mut W, frame: &FeatureFrame) -> Result<(), StreamError> {
    let payload = encode_binary_payload(frame);
    let len = payload.len() as u32;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&payload)?;
    Ok(())
}

/// Read one length-prefixed binary frame, or `None` at a clean end of stream
/// (EOF exactly on a frame boundary).
///
/// # Errors
/// [`StreamError::FrameTooLarge`] if the prefix exceeds the cap,
/// [`StreamError::Truncated`] on a short read mid-frame, or a decode error.
pub fn read_binary_frame<R: Read>(r: &mut R) -> Result<Option<FeatureFrame>, StreamError> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(StreamError::Io(e)),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_BINARY_PAYLOAD {
        return Err(StreamError::FrameTooLarge(len));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            StreamError::Truncated
        } else {
            StreamError::Io(e)
        }
    })?;
    Ok(Some(decode_binary_payload(&payload)?))
}

// ---------------------------------------------------------------------------
// Reading an incoming stream (auto-detecting the encoding)
// ---------------------------------------------------------------------------

/// A reader over an incoming feature stream that auto-detects the encoding from
/// the first byte and yields [`FeatureFrame`]s. The exact seam `--input` drives
/// to inject decoded frames onto the feature bus.
pub struct FrameStreamReader<R: BufRead> {
    inner: R,
    encoding: Encoding,
    json_line: String,
}

impl<R: BufRead> FrameStreamReader<R> {
    /// Detect the encoding and, for a binary stream, consume and validate the
    /// one-time header.
    ///
    /// Detection peeks the first byte without consuming it: [`STREAM_MAGIC`]'s
    /// `S` (`0x53`) opens a binary stream; anything else is treated as JSON (a
    /// JSON stream opens with `{` or whitespace). An empty stream detects as
    /// JSON and immediately yields `None`.
    ///
    /// # Errors
    /// [`StreamError::BadMagic`] / [`StreamError::UnsupportedSchema`] from the
    /// binary header, or an I/O error.
    pub fn new(mut inner: R) -> Result<Self, StreamError> {
        let first = {
            let buf = inner.fill_buf()?;
            buf.first().copied()
        };
        let encoding = match first {
            Some(b) if b == STREAM_MAGIC[0] => Encoding::Binary,
            _ => Encoding::Json,
        };
        if encoding == Encoding::Binary {
            read_binary_header(&mut inner)?;
        }
        Ok(Self {
            inner,
            encoding,
            json_line: String::new(),
        })
    }

    /// The detected encoding.
    #[must_use]
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// The next frame, or `None` at end of stream. Blank JSON lines are skipped.
    ///
    /// # Errors
    /// A decode error, a schema rejection, or an I/O error.
    pub fn next_frame(&mut self) -> Result<Option<FeatureFrame>, StreamError> {
        match self.encoding {
            Encoding::Binary => read_binary_frame(&mut self.inner),
            Encoding::Json => loop {
                self.json_line.clear();
                let n = self.inner.read_line(&mut self.json_line)?;
                if n == 0 {
                    return Ok(None);
                }
                let trimmed = self.json_line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                return Ok(Some(from_json_line(trimmed)?));
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame with distinctive, non-default values in every field, including
    /// edge values (NaN-free extremes, a full-length spectrum, negative
    /// correlation) to exercise the codecs.
    fn sample_frame() -> FeatureFrame {
        let spectrum: Vec<f32> = (0..SPECTRUM_BINS).map(|i| i as f32 / 255.0).collect();
        FeatureFrame {
            schema: STREAM_SCHEMA_VERSION,
            generation: 9_876_543_210,
            timestamp_ns: 1_234_567_890_123,
            sample_rate: 44_100,
            channels: 2,
            starved: true,
            activity: Activity::Idle,
            quiet_ms: 4321.5,
            dropped_frames: 42,
            rms: 0.123_45,
            peak: 1.0,
            lufs_momentary: -14.0,
            spectrum,
            bands: [0.0, 1.5, 4.0],
            flux: 0.999,
            onset: true,
            onset_age_ms: 60_000.0,
            beat_phase: 0.75,
            beat_confidence: 0.5,
            tempo_bpm: 128.0,
            stereo_correlation: -1.0,
            mid_side_ratio: 0.25,
            chroma: [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 0.0, 0.05],
        }
    }

    #[test]
    fn snapshot_frame_roundtrip_preserves_valid_spectrum() {
        let mut snap = FeatureSnapshot {
            generation: 7,
            sample_rate: 48_000,
            channels: 2,
            spectrum_len: 3,
            ..FeatureSnapshot::default()
        };
        snap.spectrum[0] = 0.1;
        snap.spectrum[1] = 0.2;
        snap.spectrum[2] = 0.3;
        snap.spectrum[3] = 0.9; // beyond spectrum_len: must be dropped

        let frame = FeatureFrame::from_snapshot(&snap);
        assert_eq!(frame.spectrum, vec![0.1, 0.2, 0.3]);
        let back = frame.to_snapshot();
        assert_eq!(back.spectrum_len, 3);
        assert_eq!(&back.spectrum[..3], &[0.1, 0.2, 0.3]);
        assert_eq!(back.spectrum[3], 0.0, "trailing bars zeroed on decode");
        assert_eq!(back.generation, 7);
    }

    #[test]
    fn json_line_roundtrips_exactly() {
        let frame = sample_frame();
        let line = to_json_line(&frame).expect("encode");
        assert!(!line.contains('\n'), "a frame is one line");
        let back = from_json_line(&line).expect("decode");
        assert_eq!(frame, back);
    }

    #[test]
    fn binary_payload_roundtrips_exactly() {
        let frame = sample_frame();
        let bytes = encode_binary_payload(&frame);
        let back = decode_binary_payload(&bytes).expect("decode");
        assert_eq!(frame, back);
    }

    #[test]
    fn binary_frame_stream_roundtrips_over_a_buffer() {
        let frames = [sample_frame(), {
            let mut f = sample_frame();
            f.generation = 1;
            f.activity = Activity::Active;
            f.spectrum.clear();
            f
        }];
        let mut buf = Vec::new();
        write_binary_header(&mut buf).unwrap();
        for f in &frames {
            write_binary_frame(&mut buf, f).unwrap();
        }
        let mut reader = FrameStreamReader::new(io::Cursor::new(buf)).expect("header");
        assert_eq!(reader.encoding(), Encoding::Binary);
        let mut got = Vec::new();
        while let Some(f) = reader.next_frame().unwrap() {
            got.push(f);
        }
        assert_eq!(got, frames);
    }

    #[test]
    fn ndjson_stream_reader_detects_and_parses() {
        let mut buf = Vec::new();
        let a = sample_frame();
        let mut b = sample_frame();
        b.generation = 2;
        writeln!(buf, "{}", to_json_line(&a).unwrap()).unwrap();
        writeln!(buf).unwrap(); // a blank keepalive-ish line, skipped
        writeln!(buf, "{}", to_json_line(&b).unwrap()).unwrap();
        let mut reader = FrameStreamReader::new(io::Cursor::new(buf)).expect("detect");
        assert_eq!(reader.encoding(), Encoding::Json);
        assert_eq!(reader.next_frame().unwrap(), Some(a));
        assert_eq!(reader.next_frame().unwrap(), Some(b));
        assert_eq!(reader.next_frame().unwrap(), None);
    }

    #[test]
    fn empty_stream_is_clean_eof() {
        let mut reader = FrameStreamReader::new(io::Cursor::new(Vec::new())).expect("detect");
        assert_eq!(reader.encoding(), Encoding::Json);
        assert_eq!(reader.next_frame().unwrap(), None);
    }

    #[test]
    fn unknown_json_schema_is_rejected_not_panicked() {
        let mut frame = sample_frame();
        frame.schema = STREAM_SCHEMA_VERSION + 1;
        let line = serde_json::to_string(&frame).unwrap();
        match from_json_line(&line) {
            Err(StreamError::UnsupportedSchema { found, expected }) => {
                assert_eq!(found, STREAM_SCHEMA_VERSION + 1);
                assert_eq!(expected, STREAM_SCHEMA_VERSION);
            }
            other => panic!("expected UnsupportedSchema, got {other:?}"),
        }
    }

    #[test]
    fn unknown_binary_header_schema_is_rejected() {
        let mut header = [0u8; BINARY_HEADER_LEN];
        header[..4].copy_from_slice(&STREAM_MAGIC);
        header[4..6].copy_from_slice(&99u16.to_le_bytes());
        let mut cur = io::Cursor::new(header.to_vec());
        match read_binary_header(&mut cur) {
            Err(StreamError::UnsupportedSchema { found, .. }) => assert_eq!(found, 99),
            other => panic!("expected UnsupportedSchema, got {other:?}"),
        }
    }

    #[test]
    fn bad_magic_is_rejected() {
        let bytes = *b"XXXX\x01\x00\x00\x00";
        let mut cur = io::Cursor::new(bytes.to_vec());
        assert!(matches!(
            read_binary_header(&mut cur),
            Err(StreamError::BadMagic)
        ));
    }

    #[test]
    fn truncated_binary_payload_errors() {
        let frame = sample_frame();
        let mut bytes = encode_binary_payload(&frame);
        bytes.truncate(bytes.len() - 4);
        assert!(matches!(
            decode_binary_payload(&bytes),
            Err(StreamError::Truncated)
        ));
    }

    #[test]
    fn oversized_binary_length_is_capped() {
        // A frame whose length prefix claims more than the cap must be refused
        // before any allocation.
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_BINARY_PAYLOAD + 1).to_le_bytes());
        let mut cur = io::Cursor::new(buf);
        assert!(matches!(
            read_binary_frame(&mut cur),
            Err(StreamError::FrameTooLarge(_))
        ));
    }

    /// Golden NDJSON line: a fixed default-ish snapshot serialises to exactly
    /// this text. A field rename, reorder or added field breaks it — the wire
    /// contract is frozen until the schema version bumps.
    #[test]
    fn ndjson_golden_line_is_stable() {
        let mut snap = FeatureSnapshot {
            generation: 3,
            timestamp_ns: 1_000_000,
            sample_rate: 48_000,
            channels: 2,
            rms: 0.5,
            peak: 0.8,
            spectrum_len: 4,
            bands: [1.0, 0.5, 0.25],
            flux: 0.1,
            beat_confidence: 0.9,
            tempo_bpm: 120.0,
            ..FeatureSnapshot::default()
        };
        snap.spectrum[0] = 0.0;
        snap.spectrum[1] = 0.25;
        snap.spectrum[2] = 0.5;
        snap.spectrum[3] = 1.0;
        let frame = FeatureFrame::from_snapshot(&snap);
        let line = to_json_line(&frame).unwrap();
        let expected = concat!(
            r#"{"schema":1,"generation":3,"timestamp_ns":1000000,"sample_rate":48000,"#,
            r#""channels":2,"starved":false,"activity":"active","quiet_ms":0.0,"#,
            r#""dropped_frames":0,"rms":0.5,"peak":0.8,"lufs_momentary":0.0,"#,
            r#""spectrum":[0.0,0.25,0.5,1.0],"bands":[1.0,0.5,0.25],"flux":0.1,"#,
            r#""onset":false,"onset_age_ms":0.0,"beat_phase":0.0,"beat_confidence":0.9,"#,
            r#""tempo_bpm":120.0,"stereo_correlation":0.0,"mid_side_ratio":0.0,"#,
            r#""chroma":[0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0]}"#,
        );
        assert_eq!(line, expected);
    }
}
