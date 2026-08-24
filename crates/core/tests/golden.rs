//! Golden-file DSP tests.
//!
//! Each committed WAV fixture in `tests/fixtures/` is driven through a default
//! [`HopProcessor`] on the mono path and its features are compared, at 250 ms
//! sample points, against the committed golden JSON in `tests/golden/` under
//! per-field tolerance bands (see `tests/golden/README.md`).
//!
//! # Blessing
//!
//! Set `SCIA_BLESS=1` to rewrite the golden files from the current DSP output
//! instead of asserting:
//!
//! ```text
//! SCIA_BLESS=1 cargo nextest run -p scia-core golden
//! ```
//!
//! Do this only when a deliberate DSP change has moved the numbers, and review
//! the JSON diff before committing. The WAV fixtures themselves are regenerated
//! separately with `cargo run --example gen_fixtures`.

#[path = "support/fixtures.rs"]
mod fixtures;

use std::path::PathBuf;

use fixtures::{Golden, compare, compute_golden, golden_dir, worst_offenders_table};

/// Path to a fixture's golden JSON.
fn golden_path(name: &str) -> PathBuf {
    golden_dir().join(format!("{name}.json"))
}

/// `true` when the suite is blessing rather than asserting.
fn blessing() -> bool {
    std::env::var("SCIA_BLESS").is_ok_and(|v| v == "1")
}

/// Load a fixture's committed golden.
fn load_golden(name: &str) -> Golden {
    let path = golden_path(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden {}: {e}\nrun `SCIA_BLESS=1 cargo nextest run -p scia-core golden` to create it",
            path.display()
        )
    });
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// Write a fixture's golden (blessing path).
fn write_golden(g: &Golden) {
    std::fs::create_dir_all(golden_dir()).expect("create golden dir");
    let path = golden_path(&g.fixture);
    let text = serde_json::to_string_pretty(g).expect("serialize golden");
    std::fs::write(&path, format!("{text}\n")).expect("write golden");
    eprintln!("blessed {}", path.display());
}

/// The shared body of every `golden_<fixture>` test: recompute and either bless
/// or assert against the committed golden.
fn check_fixture(name: &str) {
    let actual = compute_golden(name);
    if blessing() {
        write_golden(&actual);
        return;
    }
    let expected = load_golden(name);
    let mismatches = compare(&expected, &actual);
    assert!(
        mismatches.is_empty(),
        "golden mismatch for {name}:\n{}",
        worst_offenders_table(&mismatches)
    );
}

#[test]
fn golden_sine_1k_minus6db() {
    check_fixture("sine_1k_-6db");
}

#[test]
fn golden_sine_60hz_minus6db() {
    check_fixture("sine_60hz_-6db");
}

#[test]
fn golden_sine_5k_minus12db() {
    check_fixture("sine_5k_-12db");
}

#[test]
fn golden_clicks_120bpm() {
    check_fixture("clicks_120bpm");
}

#[test]
fn golden_noise_minus20db() {
    check_fixture("noise_-20db");
}

#[test]
fn golden_silence() {
    check_fixture("silence");
}

#[test]
fn golden_sweep_50_10k() {
    check_fixture("sweep_50_10k");
}

#[test]
fn golden_burst() {
    check_fixture("burst");
}

/// The committed WAV fixtures regenerate byte-for-byte. Guards the determinism
/// the whole scheme rests on: if the generator drifts, every golden goes stale
/// silently, so this catches it at the source.
#[test]
fn fixtures_are_deterministic() {
    let tmp = std::env::temp_dir().join(format!(
        "scia-golden-fixtures-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fixtures::generate_all(&tmp);

    for name in fixtures::FIXTURES {
        let regenerated = std::fs::read(tmp.join(format!("{name}.wav")))
            .unwrap_or_else(|e| panic!("read regenerated {name}: {e}"));
        let committed = std::fs::read(fixtures::fixtures_dir().join(format!("{name}.wav")))
            .unwrap_or_else(|e| panic!("read committed {name}.wav: {e}"));
        assert_eq!(
            regenerated, committed,
            "{name}.wav is not reproducible: regeneration differs from the committed file \
             (run `cargo run --example gen_fixtures` and re-bless if this was intended)"
        );
    }

    let _ = std::fs::remove_dir_all(&tmp);
}

/// The gate must bite: perturbing an expected value past its tolerance has to
/// register as a failure. Guards against a comparison that silently passes
/// everything (a green light that proves nothing).
#[test]
fn golden_tolerances_are_tight() {
    let base = compute_golden("sine_1k_-6db");

    // A clean baseline compares equal to itself.
    assert!(
        compare(&base, &base).is_empty(),
        "a golden must compare equal to itself"
    );

    // rms: perturb by 3x its (relative) tolerance.
    let mut bad_rms = base.clone();
    let rms_tol = (0.01 * base.samples[0].rms.abs()).max(1e-4);
    bad_rms.samples[0].rms += 3.0 * rms_tol;
    let m = compare(&bad_rms, &base);
    assert!(
        m.iter().any(|x| x.field == "rms"),
        "a 3x-tolerance rms perturbation must fail the comparison"
    );

    // spectrum: perturb one bar by 3x the absolute tolerance.
    let mut bad_spec = base.clone();
    bad_spec.samples[0].spectrum[0] += 3.0 * 0.02;
    let m = compare(&bad_spec, &base);
    assert!(
        m.iter().any(|x| x.field == "spectrum[0]"),
        "a 3x-tolerance spectrum perturbation must fail the comparison"
    );

    // onset hop indices: any change is an exact-match failure.
    let mut bad_onset = base.clone();
    bad_onset.onset_hops.push(9_999);
    let m = compare(&bad_onset, &base);
    assert!(
        m.iter().any(|x| x.field.starts_with("onset_hops")),
        "a changed onset-hop list must fail the comparison"
    );
}
