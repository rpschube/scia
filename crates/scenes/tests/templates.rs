//! The shipped authoring templates must always load through the real loaders,
//! so a change to a loader or to the format can never silently rot them: the
//! `.lua` template compiles on the Luau path and renders a frame, and the
//! `.toml` template parses and instantiates as a valid preset.

use std::path::PathBuf;

use scia_core::FeatureSnapshot;
use scia_scenes::{Canvas, LuauLimits, LuauScene, Scene, SceneCtx, load_preset};

/// The repository `templates/` directory, resolved from this crate's manifest
/// dir (`crates/scenes`).
fn templates_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../templates")
}

#[test]
fn scene_template_compiles_and_renders_one_frame() {
    let path = templates_dir().join("scene-template.lua");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    // Through the real Luau path: compile the manifest, init, update, render.
    let mut scene = LuauScene::from_source(&src, "scene-template", LuauLimits::default())
        .expect("the scene template compiles as a well-formed manifest");
    scene.init(&SceneCtx::default());
    scene.update(&FeatureSnapshot::default(), 1.0 / 60.0);
    let mut canvas = Canvas::new(1.0);
    scene.render(&mut canvas);

    assert!(
        !scene.is_errored(),
        "the scene template renders without faulting: {:?}",
        scene.last_error()
    );
    assert!(
        !canvas.primitives().is_empty(),
        "the scene template draws at least one primitive"
    );
}

#[test]
fn preset_template_parses_as_a_valid_preset() {
    let path = templates_dir().join("preset-template.toml");

    // Through the real preset loader: read, parse and validate.
    let preset = load_preset(&path).expect("the preset template parses and validates as a preset");
    assert_eq!(
        preset.scene, "spectra",
        "the template drives the scene it names"
    );

    // It instantiates into at least one live layer at a nominal aspect.
    let layers = preset.instantiate(1.0);
    assert!(
        !layers.is_empty(),
        "the preset template instantiates a live layer"
    );
}
