//! `scia-telemetry`: the schema and low-level writers for scia's structured
//! telemetry.
//!
//! Two independent pieces live here, both dependency-free beyond `serde`:
//!
//! * [`record`] — the **frozen run-record schema (v1)** ([`RunStart`], [`Hop`],
//!   [`Event`], [`RunEnd`], [`CanvasStats`]) and [`RecordWriter`], the buffered
//!   JSON Lines writer the `--log-run` mode and the scene-quality harness share.
//! * [`rotate`] — [`RotatingFile`], the small size-rotating file sink the
//!   structured-logging subscriber writes its JSON log lines to.
//!
//! This crate takes **no dependency on any other workspace crate**: the app maps
//! a `scia-core` `FeatureSnapshot` into these plain structs, so the schema can be
//! mirrored by a sibling tool without pulling the engine in.

#![deny(unsafe_code)]

pub mod record;
pub mod rotate;

pub use record::{
    CanvasStats, Event, Hop, Record, RecordWriter, RunEnd, RunStart, SCHEMA, to_line,
};
pub use rotate::RotatingFile;

/// The crate name, resolved at compile time from Cargo metadata.
pub const NAME: &str = env!("CARGO_PKG_NAME");
