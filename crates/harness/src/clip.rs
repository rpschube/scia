//! Loading golden clips: decode a feature-stream file into frames, and resolve a
//! `--clip <id|path>` argument against the corpus manifest (regenerating a
//! `generated` synthetic clip on demand).

use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use scia_core::{FeatureFrame, FrameStreamReader};

use crate::corpus::Manifest;
use crate::synth::synth_spec;

/// Something that went wrong loading or resolving a clip.
#[derive(Debug)]
pub enum ClipError {
    /// The clip id was not found in the manifest.
    UnknownId(String),
    /// A clip file could not be read.
    Io(String),
    /// The feature stream could not be decoded.
    Decode(String),
    /// A manifest id resolved to a generated clip with no known generator.
    NoGenerator(String),
}

impl std::fmt::Display for ClipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownId(id) => write!(f, "clip id '{id}' is not in the manifest"),
            Self::Io(e) => write!(f, "clip I/O error: {e}"),
            Self::Decode(e) => write!(f, "clip decode error: {e}"),
            Self::NoGenerator(id) => {
                write!(f, "clip '{id}' is marked generated but has no generator")
            }
        }
    }
}

impl std::error::Error for ClipError {}

/// Decode every frame of a feature-stream file (NDJSON or binary, auto-detected)
/// into a vector.
///
/// # Errors
/// [`ClipError::Io`] if the file cannot be opened, [`ClipError::Decode`] on a
/// malformed or unsupported stream.
pub fn load_file(path: &Path) -> Result<Vec<FeatureFrame>, ClipError> {
    let file =
        fs::File::open(path).map_err(|e| ClipError::Io(format!("{}: {e}", path.display())))?;
    decode(BufReader::new(file))
}

/// Decode every frame from a feature-stream byte buffer.
///
/// # Errors
/// [`ClipError::Decode`] on a malformed or unsupported stream.
pub fn load_bytes(bytes: &[u8]) -> Result<Vec<FeatureFrame>, ClipError> {
    decode(BufReader::new(std::io::Cursor::new(bytes)))
}

fn decode<R: std::io::BufRead>(reader: R) -> Result<Vec<FeatureFrame>, ClipError> {
    let mut reader =
        FrameStreamReader::new(reader).map_err(|e| ClipError::Decode(e.to_string()))?;
    let mut frames = Vec::new();
    while let Some(frame) = reader
        .next_frame()
        .map_err(|e| ClipError::Decode(e.to_string()))?
    {
        frames.push(frame);
    }
    Ok(frames)
}

/// A resolved clip: its frames plus a `source` label for the run record (the
/// manifest id, or the file name for a bare path).
pub struct ResolvedClip {
    /// The decoded feature frames.
    pub frames: Vec<FeatureFrame>,
    /// The `source` label for the run record.
    pub source: String,
    /// The hop cadence in milliseconds, inferred from the clip's frames.
    pub hop_ms: f32,
}

/// Resolve a `--clip <id|path>` argument.
///
/// An existing file path is loaded directly. Otherwise the argument is a
/// manifest id: its file is loaded, and a `generated` synthetic clip whose file
/// is missing is regenerated from its [`crate::synth::SynthSpec`] first.
///
/// `corpus_root` is the directory holding `manifest.toml` (clip paths are
/// relative to it).
///
/// # Errors
/// [`ClipError`] variants for a missing id, a read failure, or a decode failure.
pub fn resolve(arg: &str, corpus_root: &Path) -> Result<ResolvedClip, ClipError> {
    let as_path = Path::new(arg);
    if as_path.is_file() {
        let frames = load_file(as_path)?;
        let source = as_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(arg)
            .to_string();
        let hop_ms = infer_hop_ms(&frames);
        return Ok(ResolvedClip {
            frames,
            source,
            hop_ms,
        });
    }

    // Treat as a manifest id.
    let manifest_path = corpus_root.join("manifest.toml");
    let manifest = Manifest::load(&manifest_path).map_err(|e| ClipError::Io(e.to_string()))?;
    let entry = manifest
        .clip(arg)
        .ok_or_else(|| ClipError::UnknownId(arg.to_string()))?;

    let clip_path = corpus_root.join(&entry.path);
    if !clip_path.is_file() {
        if entry.generated {
            let spec = synth_spec(arg).ok_or_else(|| ClipError::NoGenerator(arg.to_string()))?;
            let bytes = spec.encode_ndjson();
            if let Some(parent) = clip_path.parent() {
                fs::create_dir_all(parent).map_err(|e| ClipError::Io(e.to_string()))?;
            }
            fs::write(&clip_path, &bytes).map_err(|e| ClipError::Io(e.to_string()))?;
        } else {
            return Err(ClipError::Io(format!(
                "clip file {} is missing (and not marked generated)",
                clip_path.display()
            )));
        }
    }

    let frames = load_file(&clip_path)?;
    let hop_ms = infer_hop_ms(&frames);
    Ok(ResolvedClip {
        frames,
        source: entry.id.clone(),
        hop_ms,
    })
}

/// Infer the hop cadence in milliseconds from the first two frames' timestamps,
/// falling back to the canonical 256-frame hop at the clip's sample rate.
#[must_use]
pub fn infer_hop_ms(frames: &[FeatureFrame]) -> f32 {
    if let (Some(a), Some(b)) = (frames.first(), frames.get(1)) {
        let dt_ns = b.timestamp_ns.saturating_sub(a.timestamp_ns);
        if dt_ns > 0 {
            return dt_ns as f32 / 1.0e6;
        }
    }
    let sr = frames.first().map_or(48_000, |f| f.sample_rate).max(1);
    crate::synth::HOP_FRAMES as f32 / sr as f32 * 1000.0
}

/// Build a `PathBuf` for a clip id's default NDJSON file under `corpus_root`.
#[must_use]
pub fn default_clip_path(corpus_root: &Path, id: &str) -> PathBuf {
    corpus_root.join("clips").join(format!("{id}.ndjson"))
}
