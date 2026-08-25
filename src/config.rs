//! Optional config file: defaults and rebindable keys.
//!
//! `scia` reads an optional TOML file for defaults and key bindings. Precedence,
//! lowest to highest, is built-in defaults < config file < command-line flags.
//! A missing file is never an error — it yields pure defaults. Any problem with
//! the file (unreadable, malformed, an unknown key action, an unparseable key,
//! an unknown presenter) degrades to defaults for the affected setting and emits
//! a single warning line at startup; it never aborts the program.
//!
//! Path (honoring the platform's config dir):
//!   * Unix:    `$XDG_CONFIG_HOME/scia/config.toml`, else `~/.config/scia/config.toml`
//!   * Windows: `%APPDATA%\scia\config.toml`

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::ValueEnum;
use serde::Deserialize;

use scia_tui::{ChromeMode, InputAction, Keymap, parse_chord};

use crate::PresenterTier;

/// The built-in demo tempo when neither a flag nor the config supplies one.
pub const DEFAULT_DEMO_BPM: f32 = 112.0;

/// The scene a bare `scia` lands in when neither a `--scene` flag nor a config
/// `[defaults] scene` supplies one: the canonical analyzer. The scene engine is
/// the product, so a first run must open in it rather than the legacy bars. The
/// legacy direct spectrum-bars renderer stays reachable through the reserved
/// name `bars` (see [`resolve`] and `main`'s scene seam).
pub const DEFAULT_SCENE: &str = "spectra";

/// The raw `[defaults]` and `[keys]` tables as parsed from the file. Every field
/// is optional; unknown `[defaults]` keys are ignored, unknown `[keys]` actions
/// are surfaced as warnings during resolution.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    defaults: RawDefaults,
    keys: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawDefaults {
    scene: Option<String>,
    presenter: Option<String>,
    overlay: Option<bool>,
    perf_mode: Option<bool>,
    demo_bpm: Option<f32>,
    chrome: Option<String>,
    device: Option<String>,
}

/// The config-file `[defaults]` after validation. Each is `None` when the file
/// left it unset (or it failed validation and was dropped with a warning).
#[derive(Debug, Default)]
pub struct FileDefaults {
    /// Default scene preset name.
    pub scene: Option<String>,
    /// Default forced presenter tier.
    pub presenter: Option<PresenterTier>,
    /// Default overlay-on state.
    pub overlay: Option<bool>,
    /// Default perf-mode-on state.
    pub perf_mode: Option<bool>,
    /// Default demo tempo.
    pub demo_bpm: Option<f32>,
    /// Default chrome personality.
    pub chrome: Option<ChromeMode>,
    /// Default (pinned) capture device name; `None` follows the system default.
    pub device: Option<String>,
}

/// The fully loaded config: validated defaults, the resolved keymap, and any
/// non-fatal warnings to print at startup.
#[derive(Debug)]
pub struct Config {
    /// The `[defaults]` layer, to be merged under the CLI flags.
    pub defaults: FileDefaults,
    /// The key bindings: built-in defaults with the file's `[keys]` applied.
    pub keymap: Keymap,
    /// Non-fatal warnings collected while loading; the caller prints each line.
    pub warnings: Vec<String>,
}

impl Config {
    /// A config with pure defaults and no warnings (missing file, or nowhere to
    /// look).
    fn defaults_only() -> Self {
        Self {
            defaults: FileDefaults::default(),
            keymap: Keymap::default(),
            warnings: Vec::new(),
        }
    }
}

/// The command-line layer of the config-overridable options, extracted from the
/// parsed CLI. `overlay` and `perf_mode` are presence flags: `true` means the
/// flag was given (and wins); `false` means fall through to the config, then the
/// built-in default.
pub struct CliLayer {
    /// `--scene NAME`.
    pub scene: Option<String>,
    /// `--presenter TIER`.
    pub presenter: Option<PresenterTier>,
    /// `--overlay` presence.
    pub overlay: bool,
    /// `--perf-mode` presence.
    pub perf_mode: bool,
    /// `--demo-bpm N`.
    pub demo_bpm: Option<f32>,
    /// `--chrome MODE`.
    pub chrome: Option<ChromeMode>,
    /// `--device NAME`.
    pub device: Option<String>,
}

/// The resolved options after applying precedence: built-in defaults < config <
/// flags.
#[derive(Debug, PartialEq)]
pub struct Resolved {
    /// Scene to render. Always `Some` after [`resolve`] — CLI > config >
    /// [`DEFAULT_SCENE`] — so a bare run opens in the default scene. The
    /// reserved value `"bars"` selects the legacy direct-bars path in `main`.
    pub scene: Option<String>,
    /// Forced presenter tier, if any.
    pub presenter: Option<PresenterTier>,
    /// Whether the overlay starts visible.
    pub overlay: bool,
    /// Whether perf mode is requested.
    pub perf_mode: bool,
    /// Demo tempo in BPM (still clamped by the caller).
    pub demo_bpm: f32,
    /// The chrome personality to start in.
    pub chrome: ChromeMode,
    /// The capture device name to open, if any; `None` follows the system
    /// default.
    pub device: Option<String>,
}

/// Merge the CLI layer over the file defaults over the built-in defaults.
///
/// A flag always wins when present; otherwise the config value applies; failing
/// that, the built-in default. `overlay`/`perf_mode` presence flags can only
/// turn a setting on (there is no `--no-*` form), so a config value of `true`
/// cannot be overridden off from the command line.
///
/// The scene resolves CLI > config > the built-in [`DEFAULT_SCENE`], so it is
/// always `Some`: a bare invocation lands in the default scene, never the legacy
/// bars. The reserved name `bars` survives this merge unchanged; `main` maps it
/// to the legacy direct-bars path where it consumes the resolved scene.
#[must_use]
pub fn resolve(cli: CliLayer, file: &FileDefaults) -> Resolved {
    Resolved {
        scene: Some(
            cli.scene
                .or_else(|| file.scene.clone())
                .unwrap_or_else(|| DEFAULT_SCENE.to_string()),
        ),
        presenter: cli.presenter.or(file.presenter),
        overlay: cli.overlay || file.overlay.unwrap_or(false),
        perf_mode: cli.perf_mode || file.perf_mode.unwrap_or(false),
        demo_bpm: cli.demo_bpm.or(file.demo_bpm).unwrap_or(DEFAULT_DEMO_BPM),
        chrome: cli.chrome.or(file.chrome).unwrap_or_default(),
        device: cli.device.or_else(|| file.device.clone()),
    }
}

/// Load the config from its platform path. Missing or unreadable files yield
/// pure defaults (the latter with a warning); a present file is parsed by
/// [`parse`].
#[must_use]
pub fn load() -> Config {
    let Some(path) = config_path() else {
        return Config::defaults_only();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Config::defaults_only(),
        Err(err) => {
            let mut cfg = Config::defaults_only();
            cfg.warnings.push(format!(
                "config: cannot read {}: {err}; using defaults",
                path.display()
            ));
            cfg
        }
    }
}

/// Parse config text into a [`Config`]. Malformed TOML yields pure defaults plus
/// one warning; valid TOML with individual bad entries drops those entries with
/// a warning each.
#[must_use]
pub fn parse(text: &str) -> Config {
    let raw: RawConfig = match toml::from_str(text) {
        Ok(raw) => raw,
        Err(err) => {
            let mut cfg = Config::defaults_only();
            let first = err.to_string();
            let first = first.lines().next().unwrap_or("parse error");
            cfg.warnings.push(format!(
                "config: ignoring malformed file ({first}); using defaults"
            ));
            return cfg;
        }
    };

    let mut warnings = Vec::new();

    // Validate the forced presenter tier against the same names the flag accepts.
    let presenter = raw.defaults.presenter.as_deref().and_then(|name| {
        match <PresenterTier as ValueEnum>::from_str(name, true) {
            Ok(tier) => Some(tier),
            Err(_) => {
                warnings.push(format!(
                    "config: unknown presenter `{name}`; ignoring (valid: octant, sextant, quadrant, half, kitty, sixel)"
                ));
                None
            }
        }
    });

    // Validate the chrome personality against the same names the flag accepts.
    let chrome = raw.defaults.chrome.as_deref().and_then(|name| {
        ChromeMode::parse(name).or_else(|| {
            warnings.push(format!(
                "config: unknown chrome `{name}`; ignoring (valid: invisible, instrument, playful, utilitarian)"
            ));
            None
        })
    });

    let defaults = FileDefaults {
        scene: raw.defaults.scene,
        presenter,
        overlay: raw.defaults.overlay,
        perf_mode: raw.defaults.perf_mode,
        demo_bpm: raw.defaults.demo_bpm,
        chrome,
        device: raw.defaults.device,
    };

    // Apply the [keys] overrides on top of the built-in map.
    let mut keymap = Keymap::default();
    for (action, key) in &raw.keys {
        let Some(act) = InputAction::parse(action) else {
            warnings.push(format!("config: unknown key action `{action}`; ignoring"));
            continue;
        };
        match parse_chord(key) {
            Ok(chord) => keymap.rebind(act, Some(chord)),
            Err(_) => warnings.push(format!(
                "config: cannot parse key `{key}` for `{action}`; keeping the default"
            )),
        }
    }

    Config {
        defaults,
        keymap,
        warnings,
    }
}

/// The `scia` config directory for this platform, or `None` when no base
/// directory is known (no `HOME`/`XDG_CONFIG_HOME`, or no `APPDATA` on Windows).
/// This is the directory `config.toml` lives in, and the root the tuning strip
/// exports builtin presets under (`<config_dir>/presets/<name>.toml`).
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let mut path = PathBuf::from(std::env::var_os("APPDATA")?);
        path.push("scia");
        Some(path)
    }
    #[cfg(not(windows))]
    {
        let mut path = match std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
        {
            Some(dir) => dir,
            None => {
                let mut home = PathBuf::from(std::env::var_os("HOME")?);
                home.push(".config");
                home
            }
        };
        path.push("scia");
        Some(path)
    }
}

/// The config file path for this platform, or `None` when no base directory is
/// known. `config.toml` inside [`config_dir`].
fn config_path() -> Option<PathBuf> {
    let mut path = config_dir()?;
    path.push("config.toml");
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_file_is_pure_defaults() {
        // An empty document is the same as no file: every default is None and the
        // keymap is the built-in one.
        let cfg = parse("");
        assert!(cfg.warnings.is_empty());
        assert_eq!(cfg.defaults.scene, None);
        assert_eq!(cfg.defaults.overlay, None);
        assert_eq!(cfg.keymap, Keymap::default());
    }

    #[test]
    fn partial_file_sets_only_what_is_present() {
        let cfg = parse(
            r#"
            [defaults]
            scene = "aurora"
            overlay = true
            "#,
        );
        assert!(cfg.warnings.is_empty(), "warnings: {:?}", cfg.warnings);
        assert_eq!(cfg.defaults.scene.as_deref(), Some("aurora"));
        assert_eq!(cfg.defaults.overlay, Some(true));
        // Untouched fields stay None.
        assert_eq!(cfg.defaults.perf_mode, None);
        assert_eq!(cfg.defaults.demo_bpm, None);
    }

    #[test]
    fn malformed_file_falls_back_to_defaults_with_one_warning() {
        let cfg = parse("this is not = = valid toml [[[");
        assert_eq!(cfg.warnings.len(), 1, "exactly one warning");
        assert!(cfg.warnings[0].contains("malformed"));
        assert_eq!(cfg.defaults.scene, None);
        assert_eq!(cfg.keymap, Keymap::default());
    }

    #[test]
    fn keys_rebind_and_unknowns_warn() {
        let cfg = parse(
            r#"
            [keys]
            scene_next = "n"
            quit = "ctrl+x"
            bogus_action = "z"
            pause = "not-a-key"
            "#,
        );
        // The two good bindings applied.
        assert_eq!(
            cfg.keymap.scene_next,
            Some(parse_chord("n").unwrap()),
            "scene_next rebound to n"
        );
        assert_eq!(cfg.keymap.quit, Some(parse_chord("ctrl+x").unwrap()));
        // pause kept its default because its value did not parse.
        assert_eq!(cfg.keymap.pause, Keymap::default().pause);
        // Two warnings: the unknown action and the unparseable key.
        assert_eq!(cfg.warnings.len(), 2, "warnings: {:?}", cfg.warnings);
        assert!(cfg.warnings.iter().any(|w| w.contains("bogus_action")));
        assert!(cfg.warnings.iter().any(|w| w.contains("not-a-key")));
    }

    #[test]
    fn unknown_presenter_warns_and_drops() {
        let cfg = parse(
            r#"
            [defaults]
            presenter = "megatier"
            "#,
        );
        assert_eq!(cfg.defaults.presenter, None);
        assert_eq!(cfg.warnings.len(), 1);
        assert!(cfg.warnings[0].contains("megatier"));
    }

    #[test]
    fn valid_presenter_parses() {
        let cfg = parse(
            r#"
            [defaults]
            presenter = "quadrant"
            "#,
        );
        assert_eq!(cfg.defaults.presenter, Some(PresenterTier::Quadrant));
        assert!(cfg.warnings.is_empty());
    }

    #[test]
    fn kitty_presenter_parses() {
        // The kitty presenter value validates like the mosaic tiers do.
        let cfg = parse(
            r#"
            [defaults]
            presenter = "kitty"
            "#,
        );
        assert_eq!(cfg.defaults.presenter, Some(PresenterTier::Kitty));
        assert!(cfg.warnings.is_empty(), "warnings: {:?}", cfg.warnings);
    }

    #[test]
    fn sixel_presenter_parses() {
        // The sixel presenter value validates like the mosaic tiers do.
        let cfg = parse(
            r#"
            [defaults]
            presenter = "sixel"
            "#,
        );
        assert_eq!(cfg.defaults.presenter, Some(PresenterTier::Sixel));
        assert!(cfg.warnings.is_empty(), "warnings: {:?}", cfg.warnings);
    }

    #[test]
    fn every_mosaic_tier_still_validates() {
        for (name, tier) in [
            ("octant", PresenterTier::Octant),
            ("sextant", PresenterTier::Sextant),
            ("quadrant", PresenterTier::Quadrant),
            ("half", PresenterTier::Half),
        ] {
            let cfg = parse(&format!("[defaults]\npresenter = \"{name}\"\n"));
            assert_eq!(cfg.defaults.presenter, Some(tier));
            assert!(cfg.warnings.is_empty(), "warnings: {:?}", cfg.warnings);
        }
    }

    #[test]
    fn flag_beats_config_beats_default() {
        let file = FileDefaults {
            scene: Some("aurora".to_string()),
            presenter: Some(PresenterTier::Half),
            overlay: Some(true),
            perf_mode: Some(true),
            demo_bpm: Some(90.0),
            chrome: Some(ChromeMode::Instrument),
            device: Some("Speakers".to_string()),
        };

        // No flags: the config layer wins over the built-in defaults.
        let from_config = resolve(
            CliLayer {
                scene: None,
                presenter: None,
                overlay: false,
                perf_mode: false,
                demo_bpm: None,
                chrome: None,
                device: None,
            },
            &file,
        );
        assert_eq!(from_config.scene.as_deref(), Some("aurora"));
        assert_eq!(from_config.presenter, Some(PresenterTier::Half));
        assert!(from_config.overlay);
        assert!(from_config.perf_mode);
        assert_eq!(from_config.demo_bpm, 90.0);
        assert_eq!(from_config.chrome, ChromeMode::Instrument);
        assert_eq!(
            from_config.device.as_deref(),
            Some("Speakers"),
            "the config device applies with no flag"
        );

        // Flags present: they beat the config layer.
        let from_flags = resolve(
            CliLayer {
                scene: Some("starfall".to_string()),
                presenter: Some(PresenterTier::Octant),
                overlay: true,
                perf_mode: true,
                demo_bpm: Some(140.0),
                chrome: Some(ChromeMode::Playful),
                device: Some("Headset".to_string()),
            },
            &file,
        );
        assert_eq!(from_flags.scene.as_deref(), Some("starfall"));
        assert_eq!(
            from_flags.device.as_deref(),
            Some("Headset"),
            "the --device flag beats the config device"
        );
        assert_eq!(from_flags.presenter, Some(PresenterTier::Octant));
        assert_eq!(from_flags.demo_bpm, 140.0);
        assert_eq!(
            from_flags.chrome,
            ChromeMode::Playful,
            "the flag beats the config"
        );

        // Nothing anywhere: the built-in defaults.
        let bare = resolve(
            CliLayer {
                scene: None,
                presenter: None,
                overlay: false,
                perf_mode: false,
                demo_bpm: None,
                chrome: None,
                device: None,
            },
            &FileDefaults::default(),
        );
        assert_eq!(
            bare.scene.as_deref(),
            Some(DEFAULT_SCENE),
            "with nothing set the scene defaults to spectra, not the legacy bars"
        );
        assert_eq!(bare.device, None, "no device anywhere is None");
        assert_eq!(bare.presenter, None);
        assert!(!bare.overlay);
        assert!(!bare.perf_mode);
        assert_eq!(bare.demo_bpm, DEFAULT_DEMO_BPM);
        assert_eq!(
            bare.chrome,
            ChromeMode::Invisible,
            "the built-in default is invisible"
        );
    }

    #[test]
    fn device_key_parses_and_resolves_under_a_flag() {
        // The config `[defaults] device` is read as the pinned device name.
        let cfg = parse(
            r#"
            [defaults]
            device = "Speakers (USB Audio)"
            "#,
        );
        assert!(cfg.warnings.is_empty(), "warnings: {:?}", cfg.warnings);
        assert_eq!(cfg.defaults.device.as_deref(), Some("Speakers (USB Audio)"));

        // With no --device flag the config device wins; a flag beats it.
        let no_flag = resolve(
            CliLayer {
                scene: None,
                presenter: None,
                overlay: false,
                perf_mode: false,
                demo_bpm: None,
                chrome: None,
                device: None,
            },
            &cfg.defaults,
        );
        assert_eq!(no_flag.device.as_deref(), Some("Speakers (USB Audio)"));

        let with_flag = resolve(
            CliLayer {
                scene: None,
                presenter: None,
                overlay: false,
                perf_mode: false,
                demo_bpm: None,
                chrome: None,
                device: Some("Line In".to_string()),
            },
            &cfg.defaults,
        );
        assert_eq!(with_flag.device.as_deref(), Some("Line In"));
    }

    #[test]
    fn chrome_parses_each_mode() {
        for (name, mode) in [
            ("invisible", ChromeMode::Invisible),
            ("instrument", ChromeMode::Instrument),
            ("playful", ChromeMode::Playful),
            ("utilitarian", ChromeMode::Utilitarian),
        ] {
            let cfg = parse(&format!("[defaults]\nchrome = \"{name}\"\n"));
            assert!(cfg.warnings.is_empty(), "warnings: {:?}", cfg.warnings);
            assert_eq!(cfg.defaults.chrome, Some(mode));
        }
    }

    /// A [`CliLayer`] with nothing set — the shape of a bare invocation.
    fn empty_cli() -> CliLayer {
        CliLayer {
            scene: None,
            presenter: None,
            overlay: false,
            perf_mode: false,
            demo_bpm: None,
            chrome: None,
            device: None,
        }
    }

    #[test]
    fn no_cli_no_config_scene_defaults_to_spectra() {
        // The headline change: a bare `scia` lands in the default scene, so the
        // scene engine — not the legacy bars — is what a first run sees.
        let resolved = resolve(empty_cli(), &FileDefaults::default());
        assert_eq!(resolved.scene.as_deref(), Some("spectra"));
        assert_eq!(resolved.scene.as_deref(), Some(DEFAULT_SCENE));
    }

    #[test]
    fn config_scene_beats_the_default() {
        let file = FileDefaults {
            scene: Some("aurora".to_string()),
            ..FileDefaults::default()
        };
        let resolved = resolve(empty_cli(), &file);
        assert_eq!(
            resolved.scene.as_deref(),
            Some("aurora"),
            "a config scene wins over the built-in default"
        );
    }

    #[test]
    fn cli_scene_beats_config_and_default() {
        let file = FileDefaults {
            scene: Some("aurora".to_string()),
            ..FileDefaults::default()
        };
        let resolved = resolve(
            CliLayer {
                scene: Some("starfall".to_string()),
                ..empty_cli()
            },
            &file,
        );
        assert_eq!(resolved.scene.as_deref(), Some("starfall"));
    }

    #[test]
    fn reserved_bars_name_survives_resolution_from_cli_and_config() {
        // `main` maps the reserved name to the legacy path; `resolve` must pass
        // it through unchanged from either layer.
        let from_cli = resolve(
            CliLayer {
                scene: Some("bars".to_string()),
                ..empty_cli()
            },
            &FileDefaults::default(),
        );
        assert_eq!(from_cli.scene.as_deref(), Some("bars"));

        let from_config = resolve(
            empty_cli(),
            &FileDefaults {
                scene: Some("bars".to_string()),
                ..FileDefaults::default()
            },
        );
        assert_eq!(from_config.scene.as_deref(), Some("bars"));
    }

    #[test]
    fn no_builtin_scene_is_named_bars() {
        // The escape hatch relies on `bars` never colliding with a registered
        // scene id; guard against a future scene claiming the reserved name.
        assert!(
            scia_scenes::builtin_scenes()
                .iter()
                .all(|info| info.id != "bars"),
            "the reserved name `bars` must not be a registered scene id"
        );
    }

    #[test]
    fn unknown_chrome_warns_and_drops() {
        let cfg = parse(
            r#"
            [defaults]
            chrome = "hologram"
            "#,
        );
        assert_eq!(cfg.defaults.chrome, None);
        assert_eq!(cfg.warnings.len(), 1);
        assert!(cfg.warnings[0].contains("hologram"));
    }
}
