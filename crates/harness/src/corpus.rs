//! The golden-clip corpus: the `manifest.toml` model and the `corpus synth` /
//! `corpus verify` operations.
//!
//! The manifest is the catalogue of clips. Each entry records the clip's id,
//! genre, relative path, duration, content hash, notes and a `generated` flag:
//!
//! * A **committed fixture** (`generated = false`) is a real clip whose bytes
//!   live in the repo; `verify` hashes the committed file.
//! * A **generated** clip (`generated = true`) is a deterministic synthetic clip
//!   too large to commit; its file is regenerated on demand and `verify`
//!   regenerates it and compares the hash rather than reading a committed file.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::hash::sha256_hex;
use crate::synth::{SynthSpec, synth_spec};

/// Clips at or below this encoded size are committed as fixtures; larger ones are
/// marked `generated` and regenerated on demand.
pub const COMMIT_SIZE_LIMIT: usize = 1_000_000;

/// One clip in the corpus manifest.
///
/// Beyond the identity/hash fields, an entry may carry **provenance** so a clip
/// rendered offline from freely-licensed source audio is fully regenerable: the
/// source's title/artist/license/URL, the hash of the exact downloaded source
/// file, the segment cut from it, the render gain, and a free-text one-liner of
/// the reproduction pipeline. Every provenance field defaults to empty and is
/// omitted from the serialised TOML when unset, so a pure-synth entry that
/// carries none of them round-trips byte-identically.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ClipEntry {
    /// Stable clip id (also the file stem and the `run --clip` key).
    pub id: String,
    /// A one-word genre label.
    pub genre: String,
    /// Path to the clip file, relative to the manifest's directory.
    pub path: String,
    /// Clip duration in seconds.
    pub duration_s: f32,
    /// SHA-256 of the encoded FEATURE-CLIP file bytes, lowercase hex.
    pub sha256: String,
    /// Free-form notes.
    #[serde(default)]
    pub notes: String,
    /// When `true`, the clip is regenerated in-repo from a [`SynthSpec`] and
    /// hash-compared rather than hashed from a committed file.
    #[serde(default)]
    pub generated: bool,

    // --- Provenance (for clips rendered offline from downloaded source audio) ---
    /// Source work's title.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// Source work's artist / author.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artist: String,
    /// Source licence, e.g. "CC BY 4.0" or "Public Domain".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub license: String,
    /// Page or file URL the source audio came from.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_url: String,
    /// SHA-256 of the exact downloaded source audio file, pre-transcode,
    /// lowercase hex.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub audio_sha256: String,
    /// Start offset of the slice cut from the source, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_start_s: Option<f32>,
    /// Length of the slice cut from the source, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_len_s: Option<f32>,
    /// The `--gain-db` applied at render.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<f32>,
    /// Free-text one-liner of the exact reproduction pipeline (e.g. the ffmpeg
    /// transcode + `scia --from-file` command shapes).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub render_cmd: String,
}

/// The corpus manifest: a list of clip entries.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    /// The clips, one `[[clip]]` table each.
    #[serde(default, rename = "clip")]
    pub clips: Vec<ClipEntry>,
}

impl Manifest {
    /// Load a manifest from `path`, or an empty manifest if the file is absent.
    ///
    /// # Errors
    /// An I/O error other than "not found", or a TOML parse error.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Serialise the manifest to TOML text.
    ///
    /// # Errors
    /// A TOML serialisation error.
    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| e.to_string())
    }

    /// Write the manifest to `path` (creating parent directories).
    ///
    /// # Errors
    /// A serialisation or I/O error.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, self.to_toml()?).map_err(|e| e.to_string())
    }

    /// The entry for a clip id, if present.
    #[must_use]
    pub fn clip(&self, id: &str) -> Option<&ClipEntry> {
        self.clips.iter().find(|c| c.id == id)
    }

    /// Insert or replace the entry with the same id, keeping the list sorted by
    /// id for a stable diff.
    pub fn upsert(&mut self, entry: ClipEntry) {
        if let Some(existing) = self.clips.iter_mut().find(|c| c.id == entry.id) {
            *existing = entry;
        } else {
            self.clips.push(entry);
        }
        self.clips.sort_by(|a, b| a.id.cmp(&b.id));
    }
}

/// The outcome of generating one synthetic clip.
pub struct SynthOutcome {
    /// The manifest entry written.
    pub entry: ClipEntry,
    /// Whether the clip was committed as a fixture (`false` = generated-only).
    pub committed: bool,
    /// The encoded byte length.
    pub bytes: usize,
}

/// Generate the synthetic clip `spec`, write its file under
/// `corpus_root/clips/`, upsert its manifest entry and save the manifest.
///
/// A clip at or below [`COMMIT_SIZE_LIMIT`] is a committed fixture
/// (`generated = false`); a larger one is marked `generated`. Either way the
/// file is written so `run` can use it immediately.
///
/// # Errors
/// An I/O or serialisation error.
pub fn synth_clip(spec: &SynthSpec, corpus_root: &Path) -> Result<SynthOutcome, String> {
    let bytes = spec.encode_ndjson();
    let sha = sha256_hex(&bytes);
    let committed = bytes.len() <= COMMIT_SIZE_LIMIT;

    let rel_path = format!("clips/{}.ndjson", spec.id);
    let clip_path = corpus_root.join(&rel_path);
    if let Some(parent) = clip_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&clip_path, &bytes).map_err(|e| e.to_string())?;

    let entry = ClipEntry {
        id: spec.id.to_string(),
        genre: spec.genre.to_string(),
        path: rel_path,
        duration_s: spec.duration_s,
        sha256: sha,
        notes: format!(
            "deterministic synthetic clip: music @ {:.0} bpm, {} hz, {}ch",
            spec.bpm, spec.sample_rate, spec.channels
        ),
        generated: !committed,
        ..ClipEntry::default()
    };

    let manifest_path = corpus_root.join("manifest.toml");
    let mut manifest = Manifest::load(&manifest_path)?;
    manifest.upsert(entry.clone());
    manifest.save(&manifest_path)?;

    Ok(SynthOutcome {
        entry,
        committed,
        bytes: bytes.len(),
    })
}

/// The result of verifying one clip entry.
pub struct VerifyResult {
    /// The clip id.
    pub id: String,
    /// Whether the hash matched.
    pub ok: bool,
    /// A human-readable detail (the mismatch or the confirmation).
    pub detail: String,
}

/// Verify every clip in the manifest at `corpus_root/manifest.toml`.
///
/// A `generated` clip is regenerated from its [`SynthSpec`] and hash-compared; a
/// committed clip's file is read and hashed. Returns one [`VerifyResult`] per
/// entry.
///
/// # Errors
/// An error loading the manifest itself.
pub fn verify(corpus_root: &Path) -> Result<Vec<VerifyResult>, String> {
    let manifest_path = corpus_root.join("manifest.toml");
    let manifest = Manifest::load(&manifest_path)?;
    let mut results = Vec::with_capacity(manifest.clips.len());
    for entry in &manifest.clips {
        results.push(verify_entry(entry, corpus_root));
    }
    Ok(results)
}

fn verify_entry(entry: &ClipEntry, corpus_root: &Path) -> VerifyResult {
    let actual = if entry.generated {
        match synth_spec(&entry.id) {
            Some(spec) => sha256_hex(&spec.encode_ndjson()),
            None => {
                return VerifyResult {
                    id: entry.id.clone(),
                    ok: false,
                    detail: "generated clip has no registered generator".to_string(),
                };
            }
        }
    } else {
        let path = corpus_root.join(&entry.path);
        match std::fs::read(&path) {
            Ok(bytes) => sha256_hex(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return VerifyResult {
                    id: entry.id.clone(),
                    ok: false,
                    detail: format!(
                        "file missing — re-render from source, see manifest provenance ({})",
                        path.display()
                    ),
                };
            }
            Err(e) => {
                return VerifyResult {
                    id: entry.id.clone(),
                    ok: false,
                    detail: format!("cannot read {}: {e}", path.display()),
                };
            }
        }
    };

    if actual == entry.sha256 {
        VerifyResult {
            id: entry.id.clone(),
            ok: true,
            detail: format!(
                "{} ({})",
                &entry.sha256[..entry.sha256.len().min(12)],
                if entry.generated {
                    "regenerated"
                } else {
                    "committed"
                }
            ),
        }
    } else {
        VerifyResult {
            id: entry.id.clone(),
            ok: false,
            detail: format!(
                "sha256 mismatch: manifest {} != actual {actual}",
                entry.sha256
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_through_toml() {
        let mut m = Manifest::default();
        m.upsert(ClipEntry {
            id: "b-clip".to_string(),
            genre: "rock".to_string(),
            path: "clips/b-clip.ndjson".to_string(),
            duration_s: 12.0,
            sha256: "abc123".to_string(),
            notes: "n".to_string(),
            generated: false,
            ..ClipEntry::default()
        });
        m.upsert(ClipEntry {
            id: "a-clip".to_string(),
            genre: "synthetic".to_string(),
            path: "clips/a-clip.ndjson".to_string(),
            duration_s: 30.0,
            sha256: "def456".to_string(),
            notes: String::new(),
            generated: true,
            ..ClipEntry::default()
        });
        // Sorted by id.
        assert_eq!(m.clips[0].id, "a-clip");
        let text = m.to_toml().unwrap();
        let back: Manifest = toml::from_str(&text).unwrap();
        assert_eq!(back.clips, m.clips);
    }

    /// Path to the committed corpus manifest, relative to this crate.
    fn committed_manifest_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../quality/corpus/manifest.toml")
    }

    #[test]
    fn committed_manifest_loads_and_resaves_byte_stable() {
        let path = committed_manifest_path();
        let original = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let manifest = Manifest::load(&path).expect("committed manifest loads");
        // The pure-synth entry must carry no provenance fields.
        assert!(!manifest.clips.is_empty(), "manifest has at least one clip");
        for clip in &manifest.clips {
            assert!(clip.title.is_empty());
            assert!(clip.artist.is_empty());
            assert!(clip.license.is_empty());
            assert!(clip.source_url.is_empty());
            assert!(clip.audio_sha256.is_empty());
            assert!(clip.segment_start_s.is_none());
            assert!(clip.segment_len_s.is_none());
            assert!(clip.gain_db.is_none());
            assert!(clip.render_cmd.is_empty());
        }
        // Re-serialising loses nothing: byte-identical to the committed file.
        assert_eq!(manifest.to_toml().unwrap(), original);
    }

    #[test]
    fn entry_with_all_provenance_fields_round_trips() {
        let mut m = Manifest::default();
        let entry = ClipEntry {
            id: "rendered-clip".to_string(),
            genre: "jazz".to_string(),
            path: "clips/rendered-clip.ndjson".to_string(),
            duration_s: 20.0,
            sha256: "feedface".to_string(),
            notes: "rendered offline from source".to_string(),
            generated: false,
            title: "Some Tune".to_string(),
            artist: "Some Artist".to_string(),
            license: "CC BY 4.0".to_string(),
            source_url: "https://example.org/audio.flac".to_string(),
            audio_sha256: "0badc0de".to_string(),
            segment_start_s: Some(12.5),
            segment_len_s: Some(20.0),
            gain_db: Some(-3.0),
            render_cmd: "ffmpeg -i src.flac -ar 48000 out.wav && scia --from-file out.wav \
                --gain-db -3"
                .to_string(),
        };
        m.upsert(entry.clone());
        let text = m.to_toml().unwrap();
        let back: Manifest = toml::from_str(&text).unwrap();
        assert_eq!(back.clips, m.clips);
        assert_eq!(back.clips[0], entry);
    }

    #[test]
    fn entry_without_provenance_serialises_without_those_keys() {
        let mut m = Manifest::default();
        m.upsert(ClipEntry {
            id: "plain".to_string(),
            genre: "synthetic".to_string(),
            path: "clips/plain.ndjson".to_string(),
            duration_s: 30.0,
            sha256: "abc123".to_string(),
            notes: String::new(),
            generated: true,
            ..ClipEntry::default()
        });
        let text = m.to_toml().unwrap();
        // None of the provenance keys are emitted when unset.
        for key in [
            "title",
            "artist",
            "license",
            "source_url",
            "audio_sha256",
            "segment_start_s",
            "segment_len_s",
            "gain_db",
            "render_cmd",
        ] {
            assert!(
                !text.contains(key),
                "unset provenance key `{key}` leaked into TOML:\n{text}"
            );
        }
        // And it still round-trips.
        let back: Manifest = toml::from_str(&text).unwrap();
        assert_eq!(back.clips, m.clips);
    }

    #[test]
    fn verify_missing_non_generated_clip_fails_but_others_still_checked() {
        let root =
            std::env::temp_dir().join(format!("scia-corpus-verify-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // One committed clip that exists on disk, one that is missing.
        let clips_dir = root.join("clips");
        std::fs::create_dir_all(&clips_dir).unwrap();
        let present_bytes = b"present clip bytes\n";
        std::fs::write(clips_dir.join("present.ndjson"), present_bytes).unwrap();
        let present_sha = sha256_hex(present_bytes);

        let mut m = Manifest::default();
        m.upsert(ClipEntry {
            id: "present".to_string(),
            genre: "rock".to_string(),
            path: "clips/present.ndjson".to_string(),
            duration_s: 5.0,
            sha256: present_sha,
            notes: String::new(),
            generated: false,
            ..ClipEntry::default()
        });
        m.upsert(ClipEntry {
            id: "missing".to_string(),
            genre: "jazz".to_string(),
            path: "clips/missing.ndjson".to_string(),
            duration_s: 5.0,
            sha256: "deadbeef".to_string(),
            notes: String::new(),
            generated: false,
            source_url: "https://example.org/src.flac".to_string(),
            ..ClipEntry::default()
        });
        m.save(&root.join("manifest.toml")).unwrap();

        let results = verify(&root).expect("verify runs to completion");
        assert_eq!(
            results.len(),
            2,
            "both clips are reported, none aborts the run"
        );

        let present = results.iter().find(|r| r.id == "present").unwrap();
        assert!(present.ok, "present clip verifies: {}", present.detail);

        let missing = results.iter().find(|r| r.id == "missing").unwrap();
        assert!(!missing.ok, "missing clip fails");
        assert!(
            missing.detail.contains("file missing")
                && missing.detail.contains("re-render from source"),
            "missing-file detail is a clear provenance hint: {}",
            missing.detail
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
