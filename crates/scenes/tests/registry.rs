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
