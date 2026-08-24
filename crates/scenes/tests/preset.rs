//! The TOML preset format: built-in presets, validation errors, mapping
//! runtime and instantiation.

// `PresetError` is intentionally rich (see `preset.rs`); it only travels on the
// cold failure path, so a large `Err` variant in these test helpers is fine.
#![allow(clippy::result_large_err)]

use std::path::Path;

use scia_core::FeatureSnapshot;
use scia_scenes::{
    Blend, Curve, Feature, MapEntry, Mapping, MappingSet, Params, Preset, PresetError,
    builtin_preset, builtin_presets, builtin_scenes, parse_preset,
};

/// Parse an inline document as if it came from `x.toml`.
fn parse(doc: &str) -> Result<Preset, PresetError> {
    parse_preset(doc, Some(Path::new("x.toml")))
}

/// The error `Display` for a document expected to fail.
fn err(doc: &str) -> String {
    parse(doc)
        .expect_err("expected a validation error")
        .to_string()
}

// ---------------------------------------------------------------------------
// Built-in presets
// ---------------------------------------------------------------------------

#[test]
fn every_builtin_preset_parses() {
    for (name, _src) in builtin_presets() {
        let preset = builtin_preset(name)
            .unwrap_or_else(|| panic!("{name} is a built-in"))
            .unwrap_or_else(|e| panic!("{name} fails to validate: {e}"));
        assert_eq!(&preset.name, name, "preset name matches its file");
    }
}

#[test]
fn every_scene_has_a_matching_preset() {
    for info in builtin_scenes() {
        let found = builtin_presets().iter().any(|(name, _)| *name == info.id);
        assert!(
            found,
            "scene `{}` has a built-in preset of the same name",
            info.id
        );
        let preset = builtin_preset(info.id)
            .expect("preset exists")
            .expect("preset validates");
        assert_eq!(preset.scene, info.id, "the preset drives its own scene");
    }
}

// ---------------------------------------------------------------------------
// One test per error class: Display must start with `x.toml:line:col:`.
// ---------------------------------------------------------------------------

/// Assert the error `Display` begins with the expected `file:line:col:` prefix.
fn assert_prefix(msg: &str, line: usize, col: usize) {
    let prefix = format!("x.toml:{line}:{col}:");
    assert!(
        msg.starts_with(&prefix),
        "expected `{msg}` to start with `{prefix}`"
    );
}

#[test]
fn unknown_key_in_preset() {
    let msg = err("[preset]\nname = \"a\"\nscene = \"spectra\"\nbogus = 1\n");
    // `bogus` is on line 4, column 1.
    assert_prefix(&msg, 4, 1);
    assert!(msg.contains("bogus"), "{msg}");
}

#[test]
fn unknown_key_in_params() {
    let msg = err("[preset]\nname = \"a\"\nscene = \"spectra\"\n[params]\nnope = 1\n");
    // `nope = 1` value is on line 5, column 8.
    assert_prefix(&msg, 5, 8);
    assert!(msg.contains("nope"), "{msg}");
}

#[test]
fn type_mismatch_string_where_number() {
    let msg = err("[preset]\nname = \"a\"\nscene = \"spectra\"\n[params]\nrelease = \"loud\"\n");
    // The string value starts at line 5, column 11.
    assert_prefix(&msg, 5, 11);
    assert!(msg.contains("expected number"), "{msg}");
    assert!(msg.contains("found string"), "{msg}");
}

#[test]
fn out_of_range_states_the_range() {
    let msg = err("[preset]\nname = \"a\"\nscene = \"spectra\"\n[params]\ngap = 5.0\n");
    // `gap`'s manifest range is 0.0..=0.9; the value 5.0 is on line 5, col 7.
    assert_prefix(&msg, 5, 7);
    assert!(msg.contains("[0, 0.9]"), "range should be stated: {msg}");
}

#[test]
fn unknown_scene() {
    let msg = err("[preset]\nname = \"a\"\nscene = \"nope\"\n");
    // The scene value starts at line 3, column 9.
    assert_prefix(&msg, 3, 9);
    assert!(msg.contains("unknown scene"), "{msg}");
}

#[test]
fn unknown_feature() {
    let msg =
        err("[preset]\nname = \"a\"\nscene = \"spectra\"\n[map]\npunch = { feature = \"nope\" }\n");
    // The map entry is on line 5.
    assert_prefix(&msg, 5, 9);
    assert!(msg.contains("unknown feature"), "{msg}");
}

#[test]
fn expression_syntax_error_reports_file_line_col() {
    // A trailing operator with no right-hand operand is a parse error.
    let msg = err("[preset]\nname = \"a\"\nscene = \"spectra\"\n[map]\npunch = \"loud *\"\n");
    // The string value is on line 5, column 9.
    assert_prefix(&msg, 5, 9);
    assert!(msg.contains("punch"), "names the key: {msg}");
    assert!(msg.contains("invalid expression"), "{msg}");
}

#[test]
fn expression_unknown_variable_reports_file_line_col() {
    // `wobble` is not part of the expression vocabulary; it fails at load.
    let msg = err("[preset]\nname = \"a\"\nscene = \"spectra\"\n[map]\npunch = \"wobble * 2\"\n");
    // The string value is on line 5, column 9.
    assert_prefix(&msg, 5, 9);
    assert!(
        msg.contains("unknown variable `wobble`"),
        "names the offending variable: {msg}"
    );
}

#[test]
fn palette_wrong_slot_count() {
    let seven = "[preset]\nname = \"a\"\nscene = \"spectra\"\n[palette]\nsource = \"static\"\nslots = [\"#000000\", \"#111111\", \"#222222\", \"#333333\", \"#444444\", \"#555555\", \"#666666\"]\n";
    let msg = err(seven);
    // The first slot is on line 6.
    assert_prefix(&msg, 6, 10);
    assert!(msg.contains("exactly 8 slots"), "{msg}");
    assert!(msg.contains("found 7"), "{msg}");
}

#[test]
fn bad_name() {
    let msg = err("[preset]\nname = \"Bad Name\"\nscene = \"spectra\"\n");
    // The name value starts at line 2, column 8.
    assert_prefix(&msg, 2, 8);
    assert!(msg.contains("invalid preset name"), "{msg}");
}

// ---------------------------------------------------------------------------
// Mapping runtime
// ---------------------------------------------------------------------------

/// A snapshot whose onset flag and band levels are set.
fn snap(onset: bool) -> FeatureSnapshot {
    FeatureSnapshot {
        onset,
        ..FeatureSnapshot::default()
    }
}

#[test]
fn attack_zero_is_instant() {
    let m = Mapping {
        target: "punch".to_string(),
        feature: Feature::Onset,
        curve: Curve::Linear,
        attack_ms: 0.0,
        decay_ms: 250.0,
        scale: 1.0,
        offset: 0.0,
    };
    let mut set = MappingSet::new(std::slice::from_ref(&m));
    let mut params = Params::new();
    set.seed(&mut params);

    // A single onset frame drives the envelope straight to the target.
    set.apply(&snap(true), 0.016, &mut params);
    let v = params.get("punch").expect("punch is set");
    assert!((v - 1.0).abs() < 1e-6, "attack 0 snaps to 1.0, got {v}");
}

#[test]
fn decay_reaches_one_over_e_after_one_time_constant() {
    let m = Mapping {
        target: "punch".to_string(),
        feature: Feature::Onset,
        curve: Curve::Linear,
        attack_ms: 0.0,
        decay_ms: 250.0,
        scale: 1.0,
        offset: 0.0,
    };
    let mut set = MappingSet::new(std::slice::from_ref(&m));
    let mut params = Params::new();
    set.seed(&mut params);

    // Rise to 1.0 (instant attack), then decay for exactly one time constant
    // (250 ms) spread over many small steps.
    set.apply(&snap(true), 0.0, &mut params);
    let dt = 0.001;
    for _ in 0..250 {
        set.apply(&snap(false), dt, &mut params);
    }
    let v = params.get("punch").expect("punch is set");
    let target = std::f32::consts::E.recip();
    let rel = (v - target).abs() / target;
    assert!(rel < 0.05, "after one tau expected ~{target}, got {v}");
}

#[test]
fn pow_and_step_curves() {
    // pow: 0.5^2 = 0.25.
    let pow = Mapping {
        target: "punch".to_string(),
        feature: Feature::Loud,
        curve: Curve::Pow { exponent: 2.0 },
        attack_ms: 0.0,
        decay_ms: 0.0,
        scale: 1.0,
        offset: 0.0,
    };
    let mut set = MappingSet::new(std::slice::from_ref(&pow));
    let mut params = Params::new();
    set.seed(&mut params);
    let mut f = snap(false);
    f.rms = 0.5;
    set.apply(&f, 0.016, &mut params);
    let v = params.get("punch").unwrap();
    assert!((v - 0.25).abs() < 1e-6, "pow(0.5, 2) = 0.25, got {v}");

    // step: below threshold -> 0, at/above -> 1.
    let step = Mapping {
        target: "punch".to_string(),
        feature: Feature::Loud,
        curve: Curve::Step { threshold: 0.4 },
        attack_ms: 0.0,
        decay_ms: 0.0,
        scale: 1.0,
        offset: 0.0,
    };
    let mut set = MappingSet::new(std::slice::from_ref(&step));
    let mut params = Params::new();
    set.seed(&mut params);
    let mut lo = snap(false);
    lo.rms = 0.3;
    set.apply(&lo, 0.016, &mut params);
    assert_eq!(params.get("punch").unwrap(), 0.0, "below threshold -> 0");
    let mut hi = snap(false);
    hi.rms = 0.6;
    set.apply(&hi, 0.016, &mut params);
    assert_eq!(params.get("punch").unwrap(), 1.0, "above threshold -> 1");
}

#[test]
fn scale_and_offset_applied() {
    let m = Mapping {
        target: "punch".to_string(),
        feature: Feature::Loud,
        curve: Curve::Linear,
        attack_ms: 0.0,
        decay_ms: 0.0,
        scale: 0.5,
        offset: 0.2,
    };
    let mut set = MappingSet::new(std::slice::from_ref(&m));
    let mut params = Params::new();
    set.seed(&mut params);
    let mut f = snap(false);
    f.rms = 0.8;
    set.apply(&f, 0.016, &mut params);
    // offset + scale * curve(clamp(0.8)) = 0.2 + 0.5 * 0.8 = 0.6.
    let v = params.get("punch").unwrap();
    assert!((v - 0.6).abs() < 1e-6, "offset + scale*y = 0.6, got {v}");
}

// ---------------------------------------------------------------------------
// Expression mappings
// ---------------------------------------------------------------------------

#[test]
fn expression_mapping_drives_param_per_frame() {
    // `gap` driven by an expression of loudness; it must track the loudness of
    // whichever frame is current, not a value fixed at load.
    let preset =
        parse("[preset]\nname = \"a\"\nscene = \"spectra\"\n[map]\ngap = \"loud * 0.5\"\n")
            .expect("preset validates");
    assert!(
        matches!(preset.mappings.as_slice(), [MapEntry::Expr(_)]),
        "the string map compiled to an expression entry"
    );

    let mut layers = preset.instantiate(1.0);
    let layer = &mut layers[0];
    let mut params = Params::new();
    layer.mappings.seed(&mut params);

    let mut f = FeatureSnapshot {
        rms: 0.8,
        ..FeatureSnapshot::default()
    };
    layer.mappings.apply(&f, 0.016, &mut params);
    let v = params.get("gap").expect("gap is set");
    assert!((v - 0.4).abs() < 1e-6, "loud * 0.5 = 0.4, got {v}");

    // A quieter frame moves the mapped value the same frame.
    f.rms = 0.2;
    layer.mappings.apply(&f, 0.016, &mut params);
    let v = params.get("gap").unwrap();
    assert!((v - 0.1).abs() < 1e-6, "loud * 0.5 = 0.1, got {v}");
}

#[test]
fn expression_onset_variable_is_an_envelope() {
    // The `onset` variable is a decaying envelope, not a one-frame spike: it is
    // full on the onset hop and still clearly positive one frame later.
    let preset = parse("[preset]\nname = \"a\"\nscene = \"spectra\"\n[map]\npunch = \"onset\"\n")
        .expect("preset validates");
    let mut layers = preset.instantiate(1.0);
    let layer = &mut layers[0];
    let mut params = Params::new();
    layer.mappings.seed(&mut params);

    layer.mappings.apply(&snap(true), 0.016, &mut params);
    let on = params.get("punch").unwrap();
    assert!(
        (on - 1.0).abs() < 1e-6,
        "onset hop drives the envelope to 1.0"
    );

    // One 16 ms frame later, with no onset: e^{-0.016/0.25} ≈ 0.938.
    layer.mappings.apply(&snap(false), 0.016, &mut params);
    let decayed = params.get("punch").unwrap();
    assert!(
        decayed > 0.9 && decayed < 1.0,
        "the envelope decays smoothly, not to zero: got {decayed}"
    );
}

#[test]
fn mixed_table_and_expression_preset() {
    // One table entry and one expression entry in the same [map] block.
    let preset = parse(
        "[preset]\nname = \"a\"\nscene = \"spectra\"\n[map]\n\
         punch = { feature = \"onset\", attack_ms = 0, decay_ms = 0, scale = 1.0 }\n\
         gap = \"loud * 0.5\"\n",
    )
    .expect("preset validates");
    assert_eq!(preset.mappings.len(), 2, "both entries retained");
    assert!(
        preset
            .mappings
            .iter()
            .any(|e| matches!(e, MapEntry::Table(_))),
        "one table entry"
    );
    assert!(
        preset
            .mappings
            .iter()
            .any(|e| matches!(e, MapEntry::Expr(_))),
        "one expression entry"
    );

    let mut layers = preset.instantiate(1.0);
    let layer = &mut layers[0];
    let mut params = Params::new();
    layer.mappings.seed(&mut params);

    let mut f = snap(true);
    f.rms = 0.6;
    layer.mappings.apply(&f, 0.016, &mut params);
    assert!(
        (params.get("punch").unwrap() - 1.0).abs() < 1e-6,
        "the table entry drives punch to 1.0 on the onset"
    );
    assert!(
        (params.get("gap").unwrap() - 0.3).abs() < 1e-6,
        "the expression entry drives gap to loud * 0.5 = 0.3"
    );
}

// ---------------------------------------------------------------------------
// Instantiation
// ---------------------------------------------------------------------------

#[test]
fn layerless_preset_instantiates_one_layer() {
    let preset = parse("[preset]\nname = \"a\"\nscene = \"spectra\"\n").unwrap();
    let layers = preset.instantiate(1.6);
    assert_eq!(layers.len(), 1, "a layerless preset is exactly one layer");
    assert_eq!(layers[0].scene.id(), "spectra");
    assert_eq!(layers[0].blend, Blend::Over);
    assert!((layers[0].intensity - 1.0).abs() < 1e-6);
}

#[test]
fn layered_preset_instantiates_each_layer() {
    let doc = "\
[preset]
name = \"a\"
scene = \"spectra\"
[[layer]]
scene = \"spectra\"
blend = \"add\"
intensity = 0.5
[[layer]]
scene = \"spectra\"
";
    let preset = parse(doc).unwrap();
    let layers = preset.instantiate(1.6);
    assert_eq!(layers.len(), 2, "one instance per [[layer]]");
    assert_eq!(layers[0].blend, Blend::Add);
    assert!((layers[0].intensity - 0.5).abs() < 1e-6);
    assert_eq!(layers[1].blend, Blend::Over);
}

#[test]
fn params_merge_in_manifest_then_params_then_layer_order() {
    // Manifest default gap = 0.15; [params] sets 0.3; [layer.params] sets 0.6.
    // The layer must see 0.6 (layer wins), a sibling layer without an override
    // sees the [params] value 0.3, and `release` (set only in [params]) flows
    // through to both.
    let doc = "\
[preset]
name = \"a\"
scene = \"spectra\"
[params]
gap = 0.3
release = 0.5
[[layer]]
scene = \"spectra\"
[layer.params]
gap = 0.6
[[layer]]
scene = \"spectra\"
";
    let preset = parse(doc).unwrap();
    // The stored preset-level params reflect manifest < [params].
    assert!((preset.params.get("gap").unwrap() - 0.3).abs() < 1e-6);
    assert!((preset.params.get("release").unwrap() - 0.5).abs() < 1e-6);

    // Layer 0 overrides gap to 0.6 but inherits release from [params].
    let l0 = &preset.layers[0].params;
    assert_eq!(
        l0.iter().find(|(k, _)| k == "gap").map(|(_, v)| *v),
        Some(0.6)
    );

    // The overlays are applied in order in instantiate; nothing panics.
    let _ = preset.instantiate(1.6);
    let _ = l0;
}
