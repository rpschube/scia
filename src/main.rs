//! `scia` command-line entry point.
//!
//! Parses the CLI and starts the engine. With `--demo` the built-in synthetic
//! feed drives the pipeline; otherwise `scia` captures real system audio through
//! the cpal backend. The feature bus feeds either the terminal frontend or, with
//! `--headless`, a once-a-second status line on stderr. `--list-devices` prints
//! the device table and exits.
//!
//! An optional config file supplies defaults and rebindable keys (see
//! [`mod@config`]); precedence is built-in defaults < config < CLI flags.
//!
//! Exit codes: `0` success, `1` runtime error, `2` usage / unsupported, `3` no
//! capture device.

mod config;

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::Receiver;
use std::thread::sleep;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

use scia_core::{
    Activity, CaptureError, CpalBackend, DeviceKind, DeviceSelector, Engine, EngineConfig,
    EngineError, FeatureReader, Pacing, PerfModeState, Signal, StreamHealth, SyntheticBackend,
    list_devices,
};
use scia_scenes::{Preset, PresetWatcher, ReloadEvent, load_preset};
use scia_tui::{ChromeMode, Keymap, PresenterMode, RunError, Tier, TuiOptions, run};

use config::Resolved;

/// The `CONFIG`, `KEYS` and `EXIT CODES` sections appended to the long `--help`.
const CONFIG_HELP: &str = "\
CONFIG:
  scia reads an optional TOML config for defaults and key bindings. Precedence,
  lowest to highest: built-in defaults < config file < command-line flags. A
  missing file is not an error.
    Unix:     $XDG_CONFIG_HOME/scia/config.toml  (else ~/.config/scia/config.toml)
    Windows:  %APPDATA%\\scia\\config.toml
  [defaults]  scene, presenter, overlay, perf_mode, demo_bpm, chrome
              (presenter = octant | sextant | quadrant | half | kitty | sixel;
              chrome = invisible | instrument | playful | utilitarian;
              both overridden by their flags)
  [keys]      rebind actions scene_next, scene_prev, browser, overlay, pause,
              quit, chrome, now_playing, palette. A value is a single
              character, a named
              key (tab, esc, left, right, up, down, enter, space, backtick), or
              ctrl+<key>. Unknown actions or unparseable keys warn and are
              ignored.

KEYS (defaults, all rebindable; press ? in-app for the active map):
  right/left  next / prev scene      tab    scene browser
  `           debug overlay          space  pause         q  quit
  n           now-playing panel      p      apply palette
  c           cycle chrome           esc    back (browser) / quit
  ?           toggle help

EXIT CODES:
  0  success            1  runtime error         2  usage / unsupported
  3  no capture device";

/// Per-query timeout for capability probing; the four queries stay well under
/// ~600 ms total.
const PROBE_TIMEOUT: Duration = Duration::from_millis(150);

/// A live, terminal audio spectrum.
#[derive(Parser, Debug)]
#[command(name = "scia", version, about, long_about = None, after_long_help = CONFIG_HELP)]
struct Cli {
    /// Subcommand (optional). `list-scenes` mirrors `--list-scenes`.
    #[command(subcommand)]
    command: Option<Command>,

    /// Use the built-in synthetic feed (no audio capture).
    #[arg(long)]
    demo: bool,

    /// Which synthetic waveform the demo feed generates. Defaults to the
    /// musically plausible mix; `sine` and `clicks` keep the old probe signals.
    #[arg(long, value_enum, default_value_t = DemoSignal::Music)]
    demo_signal: DemoSignal,

    /// Tempo for the `music` demo signal, in BPM (clamped 40..=220). Defaults to
    /// 112 (overridable via the config `[defaults] demo_bpm`). Ignored by the
    /// `sine` and `clicks` signals.
    #[arg(long)]
    demo_bpm: Option<f32>,

    /// Capture device name (exact match from --list-devices). Defaults to the
    /// system mix (Windows loopback / PipeWire sink) or the default input.
    #[arg(long)]
    device: Option<String>,

    /// Prefer the PipeWire host on Linux (the sink monitor = system mix) when
    /// built with the `capture-pipewire` feature. This is the default.
    #[arg(long, overrides_with = "no_pipewire")]
    pipewire: bool,

    /// Use the default host (ALSA on Linux) instead of PipeWire.
    #[arg(long, overrides_with = "pipewire")]
    no_pipewire: bool,

    /// Print the host/device table and exit.
    #[arg(long)]
    list_devices: bool,

    /// No TUI: print one status line per second to stderr until --seconds
    /// elapses (or the process is killed).
    #[arg(long)]
    headless: bool,

    /// Disable the automatic capture-reopen watcher (no device-switch or fault
    /// recovery). The watcher is on by default.
    #[arg(long)]
    no_route_watch: bool,

    /// Windows only: opt in to perf mode. When the default render endpoint
    /// advertises an engine period below its default, hold a companion silent
    /// render stream that pulls the endpoint — and the loopback capture — down
    /// to that faster period. Reports its state on start; no effect on --demo or
    /// off Windows.
    #[arg(long)]
    perf_mode: bool,

    /// With --headless, exit after N seconds. `0` (the default) runs until the
    /// process is killed.
    #[arg(long, default_value_t = 0)]
    seconds: u64,

    /// Target frame rate.
    #[arg(long, default_value_t = 60)]
    fps: u32,

    /// Exit after N rendered frames (testing).
    #[arg(long)]
    frames: Option<u64>,

    /// Start with the debug line visible.
    #[arg(long)]
    debug: bool,

    /// Start with the debug/performance overlay panel visible (toggle at runtime
    /// with the backtick key).
    #[arg(long)]
    overlay: bool,

    /// Render a built-in scene preset (from --list-scenes) instead of the plain
    /// spectrum bars. Invalid names exit 2 listing the available presets. Not
    /// valid with --headless.
    #[arg(long, value_name = "NAME")]
    scene: Option<String>,

    /// Render a preset loaded from a TOML file on disk, live-reloading it on
    /// save (a validated edit cross-fades in under 500 ms; a broken edit keeps
    /// the running scene and shows the error). Mutually exclusive with --scene;
    /// not valid with --headless. A failed initial load exits 2.
    #[arg(long, value_name = "PATH", conflicts_with = "scene")]
    scene_file: Option<PathBuf>,

    /// Force the presenter: a mosaic tier (octant|sextant|quadrant|half, which
    /// skips capability probing), or `kitty`/`sixel` for the graphics presenters
    /// (each probes for support and falls back to mosaic when absent). Without it
    /// the mosaic tier is chosen by probing the terminal.
    #[arg(long, value_enum, value_name = "PRESENTER")]
    presenter: Option<PresenterTier>,

    /// Chrome personality: the now-playing / status chrome drawn over the scene.
    /// Cycle it at runtime with the chrome key (default `c`). Overrides the
    /// config `[defaults] chrome`; the built-in default is `invisible`.
    #[arg(long, value_enum, value_name = "MODE")]
    chrome: Option<ChromeArg>,

    /// List every registered scene and built-in preset, then exit.
    #[arg(long)]
    list_scenes: bool,
}

/// The subcommands. Kept minimal: `list-scenes` is a subcommand alias for the
/// `--list-scenes` flag (both work).
#[derive(Subcommand, Debug)]
enum Command {
    /// List every registered scene and built-in preset, then exit.
    ListScenes,
}

/// The presenters selectable with `--presenter` (or the config
/// `[defaults] presenter`): the four mosaic tiers, plus the kitty graphics
/// presenter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum PresenterTier {
    /// `2×4` block octants.
    Octant,
    /// `2×3` block sextants.
    Sextant,
    /// `2×2` quadrant blocks.
    Quadrant,
    /// `1×2` half blocks (the universally safe rung).
    Half,
    /// Kitty graphics protocol (ghostty/kitty). Falls back to the mosaic default
    /// tier when the terminal does not support it.
    Kitty,
    /// Sixel graphics protocol (Windows Terminal and others). Falls back to the
    /// mosaic default tier when the terminal does not support it.
    Sixel,
}

impl PresenterTier {
    /// The mosaic [`Tier`] this value selects, or `None` for the kitty presenter
    /// (which is not a mosaic tier).
    fn as_tier(self) -> Option<Tier> {
        match self {
            PresenterTier::Octant => Some(Tier::Octant),
            PresenterTier::Sextant => Some(Tier::Sextant),
            PresenterTier::Quadrant => Some(Tier::Quadrant),
            PresenterTier::Half => Some(Tier::Half),
            PresenterTier::Kitty | PresenterTier::Sixel => None,
        }
    }
}

/// The chrome personalities selectable with `--chrome` (or the config
/// `[defaults] chrome`). Mirrors [`scia_tui::ChromeMode`] so clap owns the flag
/// parsing without the TUI crate taking a clap dependency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ChromeArg {
    /// A single dim now-playing line that fades after ~4 s of no input.
    Invisible,
    /// A persistent one-row instrument rail.
    Instrument,
    /// The now-playing text rides the beat.
    Playful,
    /// A dense, always-visible status row.
    Utilitarian,
}

impl ChromeArg {
    /// The [`ChromeMode`] this flag value selects.
    fn mode(self) -> ChromeMode {
        match self {
            ChromeArg::Invisible => ChromeMode::Invisible,
            ChromeArg::Instrument => ChromeMode::Instrument,
            ChromeArg::Playful => ChromeMode::Playful,
            ChromeArg::Utilitarian => ChromeMode::Utilitarian,
        }
    }
}

/// The synthetic waveform choices for `--demo`.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum DemoSignal {
    /// A musically plausible mix (kick, hats, bass, pad, sparkle) on a beat grid.
    Music,
    /// A 220 Hz sine at amplitude 0.5.
    Sine,
    /// 120 bpm clicks at amplitude 0.8.
    Clicks,
}

impl DemoSignal {
    /// The [`Signal`] to generate. `bpm` (already clamped) sets the tempo of the
    /// music signal and is ignored by the others.
    fn signal(self, bpm: f32) -> Signal {
        match self {
            DemoSignal::Music => Signal::Music { bpm },
            DemoSignal::Sine => Signal::Sine {
                hz: 220.0,
                amp: 0.5,
            },
            DemoSignal::Clicks => Signal::Clicks {
                bpm: 120.0,
                amp: 0.8,
            },
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if cli.list_devices {
        return print_device_table();
    }

    // The `list-scenes` subcommand mirrors the `--list-scenes` flag.
    if cli.list_scenes || matches!(cli.command, Some(Command::ListScenes)) {
        return print_scene_list();
    }

    // A scene needs the TUI body; there is nothing to render in the headless
    // status loop. Only an *explicit* scene flag conflicts — a config scene
    // default is simply ignored under --headless.
    if cli.headless && (cli.scene.is_some() || cli.scene_file.is_some()) {
        eprintln!("--scene/--scene-file cannot be combined with --headless");
        return ExitCode::from(2);
    }

    // Load the config (one small file read; a missing file is not an error) and
    // merge it under the flags: built-in defaults < config < CLI flags.
    let cfg = config::load();
    for warning in &cfg.warnings {
        eprintln!("{warning}");
    }
    let resolved = config::resolve(
        config::CliLayer {
            scene: cli.scene.clone(),
            presenter: cli.presenter,
            overlay: cli.overlay,
            perf_mode: cli.perf_mode,
            demo_bpm: cli.demo_bpm,
            chrome: cli.chrome.map(ChromeArg::mode),
        },
        &cfg.defaults,
    );

    if cli.demo {
        run_demo(&cli, &resolved, cfg.keymap)
    } else {
        run_live(&cli, &resolved, cfg.keymap)
    }
}

/// Print the registered scenes and the built-in preset names, then exit 0.
fn print_scene_list() -> ExitCode {
    println!("{:<12}  {:<10}  summary", "scene", "mood");
    for info in scia_scenes::builtin_scenes() {
        println!("{:<12}  {:<10}  {}", info.id, info.mood, info.summary);
    }
    println!("\npresets:");
    for (name, _) in scia_scenes::builtin_presets() {
        println!("  {name}");
    }
    ExitCode::SUCCESS
}

/// Choose the presenter mode for a TUI run, plus an optional one-shot startup
/// notice.
///
/// A forced mosaic `--presenter` tier skips probing (as before). A forced
/// `--presenter kitty` or `--presenter sixel` probes the terminal for that
/// graphics protocol and the cell size: on support it uses the matching pixel
/// presenter; otherwise it falls back to the probed default mosaic tier and
/// returns a notice explaining the fallback. With no force, the terminal is
/// probed and the default mosaic tier is used — auto-selection is deliberately
/// unchanged, so the graphics presenters are only ever opt-in. Prints the
/// capability one-liner to stderr when probing a real terminal. Called only on
/// the TUI path (never headless).
fn select_presenter(resolved: &Resolved) -> (PresenterMode, Option<String>) {
    match resolved.presenter {
        Some(PresenterTier::Kitty) => {
            let report = scia_tui::probe(PROBE_TIMEOUT);
            if io::stdout().is_terminal() {
                eprintln!("{report}");
            }
            if report.kitty_graphics {
                let cell_px = report.cell_px.unwrap_or(scia_tui::FALLBACK_CELL_PX);
                (PresenterMode::Kitty { cell_px }, None)
            } else {
                let tier = scia_tui::default_tier(&report);
                (
                    PresenterMode::Mosaic(tier),
                    Some("kitty graphics unavailable; using mosaic".to_string()),
                )
            }
        }
        Some(PresenterTier::Sixel) => {
            let report = scia_tui::probe(PROBE_TIMEOUT);
            if io::stdout().is_terminal() {
                eprintln!("{report}");
            }
            if report.sixel {
                let cell_px = report.cell_px.unwrap_or(scia_tui::FALLBACK_CELL_PX);
                (PresenterMode::Sixel { cell_px }, None)
            } else {
                let tier = scia_tui::default_tier(&report);
                (
                    PresenterMode::Mosaic(tier),
                    Some("sixel graphics unavailable; using mosaic".to_string()),
                )
            }
        }
        Some(forced) => {
            // A forced mosaic tier skips probing entirely, as before.
            let tier = forced.as_tier().unwrap_or_default();
            (PresenterMode::Mosaic(tier), None)
        }
        None => {
            let report = scia_tui::probe(PROBE_TIMEOUT);
            if io::stdout().is_terminal() {
                eprintln!("{report}");
            }
            (PresenterMode::Mosaic(scia_tui::default_tier(&report)), None)
        }
    }
}

/// Print every device on every cpal host and exit 0. Enumeration failure is a
/// runtime error (exit 1).
fn print_device_table() -> ExitCode {
    let devices = match list_devices() {
        Ok(devices) => devices,
        Err(err) => {
            eprintln!("device enumeration failed: {err}");
            return ExitCode::from(1);
        }
    };
    println!("{:<8}  {:<8}  {:<7}  name", "host", "kind", "default");
    for d in &devices {
        let kind = match d.kind {
            DeviceKind::Input => "input",
            DeviceKind::Output => "output",
        };
        let default = if d.is_default_input {
            "in"
        } else if d.is_default_output {
            "out"
        } else {
            ""
        };
        println!("{:<8}  {:<8}  {:<7}  {}", d.host, kind, default, d.name);
    }
    println!("\n{} device(s) across all hosts", devices.len());
    ExitCode::SUCCESS
}

/// Start the engine on the built-in synthetic feed and run the TUI.
fn run_demo(cli: &Cli, resolved: &Resolved, keymap: Keymap) -> ExitCode {
    let bpm = resolved.demo_bpm.clamp(40.0, 220.0);
    let backend = SyntheticBackend {
        signal: cli.demo_signal.signal(bpm),
        pacing: Pacing::Realtime,
        ..SyntheticBackend::default()
    };

    let (engine, reader) = match Engine::start(Box::new(backend), EngineConfig::default()) {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("failed to start engine: {err}");
            return ExitCode::from(1);
        }
    };

    // Headless demo: the same once-a-second status loop the live path uses, so
    // the synthetic feed can be exercised with no terminal (CI, probes).
    if cli.headless {
        return run_headless(engine, reader, cli.seconds);
    }

    // A `--scene-file` preset is loaded and its watcher started before the TUI
    // takes the terminal; `_watcher` must outlive `run` to keep the watch alive.
    let (preset, reload, _watcher) = match scene_file_setup(cli) {
        Ok(parts) => parts,
        Err(code) => {
            engine.stop();
            return code;
        }
    };

    let (presenter_mode, initial_notice) = select_presenter(resolved);
    let opts = TuiOptions {
        fps: cli.fps,
        label: Some("DEMO — synthetic feed".to_string()),
        source: String::new(),
        frames: cli.frames,
        debug: cli.debug,
        overlay: resolved.overlay,
        scene: resolved.scene.clone(),
        preset,
        presenter_mode,
        initial_notice,
        keymap,
        chrome: resolved.chrome,
    };

    let outcome = run(
        reader,
        || engine.stats(),
        || engine.health(),
        || engine.now_ns(),
        reload,
        opts,
    );
    engine.stop();
    report_tui_outcome(outcome)
}

/// Start live capture on the cpal backend and run the TUI or the headless
/// status loop.
fn run_live(cli: &Cli, resolved: &Resolved, keymap: Keymap) -> ExitCode {
    let selector = match &cli.device {
        Some(name) => DeviceSelector::Named(name.clone()),
        None => DeviceSelector::Default,
    };
    // With the mutually-exclusive flags, later wins; the default (neither set)
    // prefers PipeWire.
    let prefer_pipewire = cli.pipewire || !cli.no_pipewire;
    let backend = CpalBackend {
        device: selector,
        prefer_pipewire,
    };

    let config = EngineConfig {
        route_watch: !cli.no_route_watch,
        perf_mode: resolved.perf_mode,
        ..EngineConfig::default()
    };
    let (engine, reader) = match Engine::start(Box::new(backend), config) {
        Ok(pair) => pair,
        Err(EngineError::Capture(CaptureError::NoDevice)) => {
            eprintln!("no capture device available; try --list-devices, or --demo");
            return ExitCode::from(3);
        }
        Err(err) => {
            eprintln!("failed to start capture: {err}");
            return ExitCode::from(1);
        }
    };

    // Print the negotiated format once. The host is a best-effort lookup: known
    // for a named device, omitted otherwise.
    let format = engine.format();
    match capture_host(&cli.device) {
        Some(host) => eprintln!(
            "capture: {} Hz, {} ch via {}",
            format.sample_rate, format.channels, host
        ),
        None => eprintln!("capture: {} Hz, {} ch", format.sample_rate, format.channels),
    }

    // A fault reported at open aborts before the frontend takes the terminal.
    if let StreamHealth::Errored(msg) = engine.health() {
        eprintln!("capture stream error: {msg}");
        engine.stop();
        return ExitCode::from(1);
    }

    // Report perf-mode state (one line, only when it was requested — Off prints
    // nothing). Common to headless and TUI.
    let perf_state = engine.perf_mode_state();
    match &perf_state {
        PerfModeState::Active {
            period_frames,
            sample_rate,
        } => {
            let ms = f64::from(*period_frames) * 1000.0 / f64::from((*sample_rate).max(1));
            eprintln!("perf mode: active — {period_frames}-frame engine period ({ms:.2} ms)");
        }
        PerfModeState::Unavailable { reason } => {
            eprintln!("perf mode: unavailable — {reason}");
        }
        PerfModeState::Off => {}
    }

    if cli.headless {
        return run_headless(engine, reader, cli.seconds);
    }

    // A `--scene-file` preset is loaded and its watcher started before the TUI
    // takes the terminal; `_watcher` must outlive `run` to keep the watch alive.
    let (preset, reload, _watcher) = match scene_file_setup(cli) {
        Ok(parts) => parts,
        Err(code) => {
            engine.stop();
            return code;
        }
    };

    // The source line gets a ` · perf` marker only when perf mode is actually
    // active.
    let mut source = format!("{} Hz {} ch", format.sample_rate, format.channels);
    if matches!(perf_state, PerfModeState::Active { .. }) {
        source.push_str(" · perf");
    }
    let (presenter_mode, initial_notice) = select_presenter(resolved);
    let opts = TuiOptions {
        fps: cli.fps,
        label: None,
        source,
        frames: cli.frames,
        debug: cli.debug,
        overlay: resolved.overlay,
        scene: resolved.scene.clone(),
        preset,
        presenter_mode,
        initial_notice,
        keymap,
        chrome: resolved.chrome,
    };

    let outcome = run(
        reader,
        || engine.stats(),
        || engine.health(),
        || engine.now_ns(),
        reload,
        opts,
    );
    engine.stop();
    report_tui_outcome(outcome)
}

/// Load a `--scene-file` preset and start its live-reload watcher.
///
/// Returns `(None, None, None)` when `--scene-file` is not set. Otherwise the
/// preset is validated up front — a failed load reports the validator's
/// `file:line:col` message and yields exit 2 — and a [`PresetWatcher`] is
/// started on it; the returned watcher must be kept alive for as long as the
/// TUI runs. A watcher that cannot start is a runtime error (exit 1).
#[allow(clippy::type_complexity)]
fn scene_file_setup(
    cli: &Cli,
) -> Result<
    (
        Option<Preset>,
        Option<Receiver<ReloadEvent>>,
        Option<PresetWatcher>,
    ),
    ExitCode,
> {
    let Some(path) = &cli.scene_file else {
        return Ok((None, None, None));
    };
    let path: &Path = path.as_ref();
    let preset = match load_preset(path) {
        Ok(preset) => preset,
        Err(err) => {
            eprintln!("{err}");
            return Err(ExitCode::from(2));
        }
    };
    match PresetWatcher::start(path) {
        Ok((watcher, reload)) => Ok((Some(preset), Some(reload), Some(watcher))),
        Err(err) => {
            eprintln!("failed to watch {}: {err}", path.display());
            Err(ExitCode::from(1))
        }
    }
}

/// Report a completed TUI run: the timing summary on success, the stream error
/// (exit 1) when the loop aborted, an invalid `--scene` (exit 2 with the
/// available presets), or an I/O error (exit 1).
fn report_tui_outcome(outcome: Result<scia_tui::RunSummary, RunError>) -> ExitCode {
    match outcome {
        Ok(summary) => {
            eprintln!(
                "frames={} p50={:.2}ms p99={:.2}ms",
                summary.frames, summary.p50_frame_ms, summary.p99_frame_ms
            );
            if let Some(msg) = summary.error {
                eprintln!("capture stream error: {msg}");
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Err(RunError::Scene(err)) => {
            eprintln!("{err}");
            ExitCode::from(2)
        }
        Err(RunError::Io(err)) => {
            eprintln!("runtime error: {err}");
            ExitCode::from(1)
        }
    }
}

/// Headless status loop: one line per second on stderr with the same numbers
/// the TUI debug line reports. Runs `seconds` seconds, or until killed when
/// `seconds` is `0`. Returns exit 1 if the stream faults.
fn run_headless(engine: Engine, mut reader: FeatureReader, seconds: u64) -> ExitCode {
    let mut t: u64 = 0;
    loop {
        sleep(Duration::from_secs(1));
        t += 1;

        let stats = engine.stats();
        let snap = *reader.latest();
        let (bar, val) = loudest_bar(&snap.spectrum[..snap.spectrum_len as usize]);
        let reopens = if stats.reopens > 0 {
            format!("  reopens {}", stats.reopens)
        } else {
            String::new()
        };
        eprintln!(
            "act {}  gen {}  rms {:.4}  peak {:.4}  loudest {}({:.3})  push {}  gap {:.1}ms  \
             dropped {}{}",
            activity_label(stats.activity),
            snap.generation,
            snap.rms,
            snap.peak,
            bar,
            val,
            stats.pushes,
            stats.max_gap_ms,
            stats.dropped_frames,
            reopens,
        );

        if let StreamHealth::Errored(msg) = engine.health() {
            eprintln!("capture stream error: {msg}");
            engine.stop();
            return ExitCode::from(1);
        }

        if seconds != 0 && t >= seconds {
            break;
        }
    }
    engine.stop();
    ExitCode::SUCCESS
}

/// Best-effort host name for the capture device. Known for a named device (its
/// host in the device table); `None` for the platform default, where the
/// backend chooses the host and no accessor exposes it.
fn capture_host(device: &Option<String>) -> Option<String> {
    let name = device.as_ref()?;
    let devices = list_devices().ok()?;
    devices
        .into_iter()
        .find(|d| &d.name == name)
        .map(|d| d.host)
}

/// The short indicator word for an [`Activity`], matching the TUI header.
fn activity_label(activity: Activity) -> &'static str {
    match activity {
        Activity::Active => "active",
        Activity::Quiet => "quiet",
        Activity::Idle => "idle",
    }
}

/// Index and value of the loudest display-spectrum bar (`(0, 0.0)` when empty).
fn loudest_bar(spectrum: &[f32]) -> (usize, f32) {
    let mut best = 0usize;
    let mut best_val = 0.0f32;
    for (i, &v) in spectrum.iter().enumerate() {
        if v > best_val {
            best_val = v;
            best = i;
        }
    }
    (best, best_val)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presenter_kitty_flag_parses() {
        let cli = Cli::try_parse_from(["scia", "--presenter", "kitty"]).expect("parses");
        assert_eq!(cli.presenter, Some(PresenterTier::Kitty));
    }

    #[test]
    fn presenter_sixel_flag_parses() {
        let cli = Cli::try_parse_from(["scia", "--presenter", "sixel"]).expect("parses");
        assert_eq!(cli.presenter, Some(PresenterTier::Sixel));
    }

    #[test]
    fn presenter_mosaic_flags_still_parse() {
        for (arg, expected) in [
            ("octant", PresenterTier::Octant),
            ("sextant", PresenterTier::Sextant),
            ("quadrant", PresenterTier::Quadrant),
            ("half", PresenterTier::Half),
        ] {
            let cli = Cli::try_parse_from(["scia", "--presenter", arg]).expect("parses");
            assert_eq!(cli.presenter, Some(expected));
        }
    }

    #[test]
    fn unknown_presenter_flag_is_rejected() {
        assert!(Cli::try_parse_from(["scia", "--presenter", "megatier"]).is_err());
    }
}
