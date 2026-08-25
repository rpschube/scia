//! Metric envelopes: a per-scene `min/max` band per metric, frozen from an
//! approved run so a later run can be checked for regression.
//!
//! The v0 mechanism is a configurable margin around the approved run's values:
//! for a metric value `v` the band is `v ± (margin·|v| + floor)`, where `margin`
//! is a relative fraction and `floor` a small absolute cushion so a metric that
//! sits near zero still gets a usable band.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::metrics::Metrics;

/// A small absolute cushion added to every band half-width so a near-zero metric
/// still has room.
pub const DEFAULT_FLOOR: f64 = 0.01;

/// The default relative margin (15%).
pub const DEFAULT_MARGIN: f64 = 0.15;

/// One metric's accepted band.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Band {
    /// The approved centre value.
    pub center: f64,
    /// The inclusive lower bound.
    pub min: f64,
    /// The inclusive upper bound.
    pub max: f64,
}

impl Band {
    /// Build a band around `v` with the given relative `margin` and absolute
    /// `floor`.
    #[must_use]
    pub fn around(v: f64, margin: f64, floor: f64) -> Self {
        let half = margin.abs() * v.abs() + floor.abs();
        Self {
            center: v,
            min: v - half,
            max: v + half,
        }
    }

    /// Whether `value` lies within the band.
    #[must_use]
    pub fn contains(&self, value: f64) -> bool {
        value >= self.min && value <= self.max
    }
}

/// A frozen envelope for one scene: the source run, the freeze parameters and a
/// band per metric (in [`Metrics`] serialisation order).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// The scene the envelope constrains.
    pub scene: String,
    /// The `metrics.json` the envelope was frozen from.
    pub source: String,
    /// The relative margin used.
    pub margin: f64,
    /// The absolute floor used.
    pub floor: f64,
    /// The per-metric bands, keyed by metric name.
    pub bands: std::collections::BTreeMap<String, Band>,
}

impl Envelope {
    /// Freeze an envelope from a metrics summary.
    #[must_use]
    pub fn freeze(scene: &str, source: &str, metrics: &Metrics, margin: f64, floor: f64) -> Self {
        let mut bands = std::collections::BTreeMap::new();
        for (name, v) in metrics.as_pairs() {
            bands.insert(name.to_string(), Band::around(v, margin, floor));
        }
        Self {
            scene: scene.to_string(),
            source: source.to_string(),
            margin,
            floor,
            bands,
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

    /// Every metric of `metrics` that falls outside its band, as
    /// `(name, value, band)` triples. Empty means the run is within envelope.
    #[must_use]
    pub fn violations(&self, metrics: &Metrics) -> Vec<(String, f64, Band)> {
        let mut out = Vec::new();
        for (name, v) in metrics.as_pairs() {
            if let Some(band) = self.bands.get(name) {
                if !band.contains(v) {
                    out.push((name.to_string(), v, *band));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> Metrics {
        Metrics {
            onset_response_latency_ms: 50.0,
            onset_response_gain: 0.4,
            loudness_motion_r: 0.7,
            loudness_brightness_r: 0.6,
            quiet_stillness: 0.02,
            coverage_mean: 0.3,
            coverage_p95: 0.5,
            flicker: 0.1,
            palette_churn: 0.05,
        }
    }

    #[test]
    fn band_brackets_the_value() {
        let b = Band::around(50.0, 0.15, 0.01);
        assert!(b.contains(50.0));
        assert!(b.min < 50.0 && b.max > 50.0);
        assert!(!b.contains(50.0 + 0.15 * 50.0 + 0.1));
    }

    #[test]
    fn envelope_accepts_its_own_run_and_flags_a_drift() {
        let m = metrics();
        let env = Envelope::freeze("spectra", "metrics.json", &m, DEFAULT_MARGIN, DEFAULT_FLOOR);
        assert!(env.violations(&m).is_empty(), "own run must be within band");

        let mut drifted = m;
        drifted.coverage_mean = 0.9; // way outside the 15% band
        let v = env.violations(&drifted);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].0, "coverage_mean");
    }
}
