//! The built-in scene registry.

use scia_scenes::{builtin_scenes, create_builtin};

#[test]
fn registry_lists_spectra() {
    let infos = builtin_scenes();
    let spectra = infos
        .iter()
        .find(|i| i.id == "spectra")
        .expect("spectra is listed");
    assert!(!spectra.mood.is_empty(), "mood is set");
    assert!(!spectra.summary.is_empty(), "summary is set");

    let scene = create_builtin("spectra").expect("spectra constructs");
    assert_eq!(scene.id(), "spectra");
    assert_eq!(scene.mood(), spectra.mood, "info mood matches the scene");

    assert!(
        create_builtin("does-not-exist").is_none(),
        "unknown id yields None"
    );
}

#[test]
fn registry_lists_phosphor() {
    let infos = builtin_scenes();
    let phosphor = infos
        .iter()
        .find(|i| i.id == "phosphor")
        .expect("phosphor is listed");
    assert!(!phosphor.mood.is_empty(), "mood is set");
    assert!(!phosphor.summary.is_empty(), "summary is set");
    assert!(
        !phosphor.params.is_empty(),
        "phosphor exposes a parameter manifest"
    );

    let scene = create_builtin("phosphor").expect("phosphor constructs");
    assert_eq!(scene.id(), "phosphor");
    assert_eq!(scene.mood(), phosphor.mood, "info mood matches the scene");
}

#[test]
fn registry_lists_sonar() {
    let infos = builtin_scenes();
    let sonar = infos
        .iter()
        .find(|i| i.id == "sonar")
        .expect("sonar is listed");
    assert!(!sonar.mood.is_empty(), "mood is set");
    assert!(!sonar.summary.is_empty(), "summary is set");
    assert!(
        !sonar.params.is_empty(),
        "sonar exposes a parameter manifest"
    );

    let scene = create_builtin("sonar").expect("sonar constructs");
    assert_eq!(scene.id(), "sonar");
    assert_eq!(scene.mood(), sonar.mood, "info mood matches the scene");
}

#[test]
fn registry_lists_ember_drift() {
    let infos = builtin_scenes();
    let ember = infos
        .iter()
        .find(|i| i.id == "ember-drift")
        .expect("ember-drift is listed");
    assert!(!ember.mood.is_empty(), "mood is set");
    assert!(!ember.summary.is_empty(), "summary is set");
    assert!(
        !ember.params.is_empty(),
        "ember-drift exposes a parameter manifest"
    );

    let scene = create_builtin("ember-drift").expect("ember-drift constructs");
    assert_eq!(scene.id(), "ember-drift");
    assert_eq!(scene.mood(), ember.mood, "info mood matches the scene");
}

#[test]
fn registry_lists_bloom() {
    let infos = builtin_scenes();
    let bloom = infos
        .iter()
        .find(|i| i.id == "bloom")
        .expect("bloom is listed");
    assert!(!bloom.mood.is_empty(), "mood is set");
    assert!(!bloom.summary.is_empty(), "summary is set");
    assert!(
        !bloom.params.is_empty(),
        "bloom exposes a parameter manifest"
    );

    let scene = create_builtin("bloom").expect("bloom constructs");
    assert_eq!(scene.id(), "bloom");
    assert_eq!(scene.mood(), bloom.mood, "info mood matches the scene");
}

#[test]
fn ember_drift_and_bloom_are_last_in_registry_order() {
    let ids: Vec<&str> = builtin_scenes().iter().map(|i| i.id).collect();
    let n = ids.len();
    assert_eq!(
        &ids[n - 2..],
        &["ember-drift", "bloom"],
        "the two new scenes are appended at the end, ember-drift then bloom"
    );
}
