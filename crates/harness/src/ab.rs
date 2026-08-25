//! A/B support: format a side-by-side metric comparison and print the two
//! ready-to-paste live-preview commands.

use crate::metrics::Metrics;

/// Render a metric comparison table for two candidate runs `a` and `b`.
///
/// Each row shows the metric, both values and their signed delta (`b - a`). The
/// table is plain monospace text meant for a terminal.
#[must_use]
pub fn compare_table(a: &Metrics, b: &Metrics) -> String {
    let pairs_a = a.as_pairs();
    let pairs_b = b.as_pairs();
    let name_w = pairs_a
        .iter()
        .map(|(n, _)| n.len())
        .max()
        .unwrap_or(6)
        .max("metric".len());

    let mut out = String::new();
    out.push_str(&format!(
        "{:<name_w$}  {:>14}  {:>14}  {:>14}\n",
        "metric", "A", "B", "Δ (B−A)"
    ));
    out.push_str(&format!(
        "{}  {}  {}  {}\n",
        "-".repeat(name_w),
        "-".repeat(14),
        "-".repeat(14),
        "-".repeat(14)
    ));
    for ((name, va), (_, vb)) in pairs_a.iter().zip(pairs_b.iter()) {
        out.push_str(&format!(
            "{name:<name_w$}  {va:>14.6}  {vb:>14.6}  {:>14.6}\n",
            vb - va
        ));
    }
    out
}

/// The two ready-to-paste `scia` commands for live side-by-side eyeballing of
/// candidates `a` and `b` on `clip`/`scene`.
///
/// Note: `scia --input` currently takes a stream address, so `clip` must be a
/// clip the maintainer serves on that address; the command form is what the
/// spec prescribes for the manual A/B step.
#[must_use]
pub fn paste_commands(clip: &str, scene: &str, preset_a: &str, preset_b: &str) -> (String, String) {
    (
        format!("scia --input {clip} --scene {scene} --preset {preset_a}"),
        format!("scia --input {clip} --scene {scene} --preset {preset_b}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(cov: f64) -> Metrics {
        Metrics {
            onset_response_latency_ms: 30.0,
            onset_response_gain: 0.4,
            loudness_motion_r: 0.7,
            loudness_brightness_r: 0.6,
            quiet_stillness: 0.02,
            coverage_mean: cov,
            coverage_p95: 0.5,
            flicker: 0.1,
            palette_churn: 0.05,
        }
    }

    #[test]
    fn table_has_a_row_per_metric_and_a_delta() {
        let t = compare_table(&metrics(0.3), &metrics(0.5));
        assert!(t.contains("coverage_mean"));
        assert!(t.contains("0.200000"), "expected the +0.2 delta:\n{t}");
        // Header + separator + 9 metric rows.
        assert_eq!(t.lines().count(), 11);
    }

    #[test]
    fn paste_commands_are_ready_to_run() {
        let (a, b) = paste_commands("synth-music", "spectra", "presets/a.toml", "presets/b.toml");
        assert_eq!(
            a,
            "scia --input synth-music --scene spectra --preset presets/a.toml"
        );
        assert!(b.ends_with("presets/b.toml"));
    }
}
