//! The preference log: structured human verdicts that steer scene calibration.
//!
//! Each [`Verdict`] records who preferred what and why for a scene/clip pair. The
//! `verdict` subcommand appends A/B outcomes; the log is seeded with four
//! standing maintainer verdicts that express the current calibration direction.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::util::today_iso;

/// One entry in the preference log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    /// ISO `YYYY-MM-DD` date the verdict was recorded.
    pub date: String,
    /// Who recorded it.
    pub by: String,
    /// The scene the verdict is about (`*` = all scenes).
    pub scene: String,
    /// The clip the verdict is about (`*` = all clips / general).
    pub clip: String,
    /// The chosen side: `a`, `b`, or `neither`.
    pub winner: String,
    /// The reasoning.
    pub why: String,
    /// The `a` candidate (a preset name/path), when this came from an A/B.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_a: Option<String>,
    /// The `b` candidate, when this came from an A/B.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_b: Option<String>,
}

/// The whole preference log.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PreferenceLog {
    /// The verdicts, oldest first.
    #[serde(default, rename = "verdict")]
    pub verdicts: Vec<Verdict>,
}

impl PreferenceLog {
    /// The four standing maintainer verdicts dated 2026-08-25 that express the
    /// current calibration direction.
    #[must_use]
    pub fn seeded() -> Self {
        let date = "2026-08-25".to_string();
        let by = "maintainer".to_string();
        let mk = |scene: &str, why: &str| Verdict {
            date: date.clone(),
            by: by.clone(),
            scene: scene.to_string(),
            clip: "*".to_string(),
            winner: "neither".to_string(),
            why: why.to_string(),
            preset_a: None,
            preset_b: None,
        };
        Self {
            verdicts: vec![
                mk("spectra", "too sensitive overall"),
                mk(
                    "*",
                    "every scene other than spectra is not sensitive enough",
                ),
                mk(
                    "*",
                    "most scenes don't fill enough of the canvas — motion/energy should fill the frame (tiled/field-type scenes are fine)",
                ),
                mk(
                    "*",
                    "album-art palettes lean too grey — bias vibrant unless the art is truly neutral",
                ),
            ],
        }
    }

    /// Load the log from `path`, or the seeded log if the file is absent.
    ///
    /// # Errors
    /// An I/O error other than "not found", or a TOML parse error.
    pub fn load_or_seed(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::seeded()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Serialise to TOML.
    ///
    /// # Errors
    /// A TOML serialisation error.
    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| e.to_string())
    }

    /// Write to `path`, creating parent directories.
    ///
    /// # Errors
    /// A serialisation or I/O error.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, self.to_toml()?).map_err(|e| e.to_string())
    }
}

/// Append a verdict to the log at `path` (seeding the file first if it does not
/// exist), stamped with today's date and attributed to `maintainer`.
///
/// # Errors
/// An I/O or serialisation error.
pub fn append(
    path: &Path,
    scene: &str,
    clip: &str,
    winner: &str,
    why: &str,
) -> Result<Verdict, String> {
    let mut log = PreferenceLog::load_or_seed(path)?;
    let verdict = Verdict {
        date: today_iso(),
        by: "maintainer".to_string(),
        scene: scene.to_string(),
        clip: clip.to_string(),
        winner: winner.to_string(),
        why: why.to_string(),
        preset_a: None,
        preset_b: None,
    };
    log.verdicts.push(verdict.clone());
    log.save(path)?;
    Ok(verdict)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_log_round_trips_and_has_four_verdicts() {
        let log = PreferenceLog::seeded();
        assert_eq!(log.verdicts.len(), 4);
        assert_eq!(log.verdicts[0].scene, "spectra");
        assert!(log.verdicts.iter().all(|v| v.date == "2026-08-25"));
        assert!(log.verdicts.iter().all(|v| v.by == "maintainer"));
        let text = log.to_toml().unwrap();
        let back: PreferenceLog = toml::from_str(&text).unwrap();
        assert_eq!(back.verdicts, log.verdicts);
    }
}
