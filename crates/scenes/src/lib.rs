//! Scene engine for scia: the TUI↔GPU portability seam.
//!
//! A [`Scene`] turns the feature stream from `scia-core` into a [`Canvas`] — an
//! abstract display list in normalized coordinates. Presenters (a terminal one
//! now, a GPU one later) rasterize the canvas; scenes never learn the physical
//! cell or pixel size, so a single scene drives every backend unchanged. This
//! crate carries the trait, the canvas, the scene context and continuity types,
//! the host [`Palette`], a registry of built-in scenes and the first such
//! scene, [`builtin::Spectra`].
//!
//! The design leaves room for a scripting rung (Luau presets) to bind to the
//! same [`Scene`] shape later: draw calls are per-primitive methods on the
//! canvas, feature scalars are read directly and the spectrum is a slice, so a
//! bound scene pays the same costs a Rust one does.

#![forbid(unsafe_code)]

pub mod builtin;
pub mod canvas;
pub mod palette;
pub mod preset;
pub mod registry;
pub mod scene;

pub use canvas::{Canvas, PALETTE_SLOTS, Primitive, Slot, Style};
pub use palette::{Palette, Rgb};
pub use preset::{
    Blend, Curve, Feature, Layer, LayerInstance, Mapping, MappingSet, PaletteSource, Preset,
    PresetError, PresetErrorKind, builtin_preset, builtin_presets, load_preset, parse_preset,
};
pub use registry::{SceneInfo, builtin_scenes, create_builtin, scene_info};
pub use scene::{ParamSpec, Params, Scene, SceneCtx, SceneState};

/// The crate name, resolved at compile time from Cargo metadata.
pub const NAME: &str = env!("CARGO_PKG_NAME");
