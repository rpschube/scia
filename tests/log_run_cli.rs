//! End-to-end coverage for `--log-run`, driven by the synthetic demo
//! feed under `--headless` so it needs no audio stack and no terminal. The
//! emitted JSON Lines are parsed back with the shared run-record schema.

use std::path::PathBuf;
use std::process::Command;

use scia_telemetry::record::Record;

/// The `scia` binary under test (cargo sets this for the integration harness).
const BIN: &str = env!("CARGO_BIN_EXE_scia");

/// A unique scratch file path under the system temp dir.
fn scratch(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "scia-logrun-{tag}-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    p
}

/// A short synthetic-feed run with `--log-run` produces a parseable run record:
/// a `run_start` first, at least one `hop`, a `run_end` last, and monotonic
/// hop timestamps.
#[test]
fn log_run_produces_a_parseable_monotonic_record() {
    let path = scratch("demo");
    let out = Command::new(BIN)
        .args([
            "--demo",
            "--headless",
            "--seconds",
            "1",
            "--log-run",
            path.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("spawn scia");
    assert!(out.status.success(), "exit: {:?}", out.status);

    let text = std::fs::read_to_string(&path).expect("run record written");
    let records: Vec<Record> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| Record::from_line(l).expect("parse run-record line"))
        .collect();
    std::fs::remove_file(&path).ok();

    assert!(records.len() >= 3, "expected run_start, hops, run_end");

    // First line is run_start with the frozen schema and the synthetic source.
    match &records[0] {
        Record::RunStart(rs) => {
            assert_eq!(rs.schema, scia_telemetry::record::SCHEMA);
            assert_eq!(rs.source, "synthetic");
            assert!(rs.hop_ms > 0.0, "hop period is set");
        }
        other => panic!("first record must be run_start, got {other:?}"),
    }

    // Last line is run_end, and its hop count matches the hop records.
    let hop_count = records
        .iter()
        .filter(|r| matches!(r, Record::Hop(_)))
        .count();
    assert!(hop_count > 0, "at least one hop was recorded");
    match records.last().expect("non-empty") {
        Record::RunEnd(re) => {
            assert_eq!(re.hops as usize, hop_count, "run_end hop count matches");
        }
        other => panic!("last record must be run_end, got {other:?}"),
    }

    // Hop timestamps are monotonically non-decreasing.
    let mut prev = f64::NEG_INFINITY;
    for r in &records {
        if let Record::Hop(h) = r {
            assert!(
                h.t_ms >= prev,
                "hop t_ms must be monotonic: {} then {}",
                prev,
                h.t_ms
            );
            prev = h.t_ms;
            assert_eq!(h.bands.len(), 3, "three bands in schema 1");
            assert!(h.canvas.is_none(), "canvas is null in --log-run mode");
        }
    }
}
