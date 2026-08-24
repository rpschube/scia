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
