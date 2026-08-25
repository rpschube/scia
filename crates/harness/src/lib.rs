//! `scia-harness`: the objective layer of the scene-quality iteration loop.
//!
//! The harness replays recorded feature-stream clips through scenes headlessly,
//! scores the resulting [`scia_scenes::Canvas`] display lists with per-run
//! metrics, and provides the A/B, preference-log and envelope-freeze plumbing
//! that the scene-calibration pass is built on. It depends on `scia-core` (the
//! feature-stream reader and the synthetic feed), `scia-scenes` (the scene
//! registry and the abstract canvas) and `scia-telemetry` (the run-record
//! schema); it drives no UI.
//!
//! The run-record schema v1 (JSON Lines) it replays and writes lives in the
//! shared [`scia_telemetry::record`] crate; the harness serialises through those
//! structs so its wire form stays byte-identical to the app's `--log-run`.
//!
//! # Modules
//!
//! * [`synth`] — deterministic synthetic golden clips.
//! * [`clip`] / [`corpus`] — the golden-clip corpus and its manifest.
//! * [`canvas_stats`] — per-hop stats rasterised from a scene's display list.
//! * [`metrics`] — the whole-run scene-quality metric formulas.
//! * [`replay`] — headless scene replay producing records + metrics.
//! * [`ab`] / [`verdict`] / [`freeze`] — the A/B rig, preference log and metric
//!   envelopes.
//! * [`hash`] — a dependency-free SHA-256 for corpus fingerprints.

#![forbid(unsafe_code)]

pub mod ab;
pub mod canvas_stats;
pub mod clip;
pub mod corpus;
pub mod freeze;
pub mod hash;
pub mod metrics;
pub mod replay;
pub mod synth;
pub mod util;
pub mod verdict;

/// The crate name, resolved at compile time from Cargo metadata.
pub const NAME: &str = env!("CARGO_PKG_NAME");
