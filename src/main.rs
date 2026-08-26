//! `scia` command-line entry point.
//!
//! Parses the CLI and starts the engine. With `--demo` the built-in synthetic
//! feed drives the pipeline; otherwise `scia` captures real system audio through
//! the cpal backend. The feature bus feeds either the terminal frontend or, with
//! `--headless`, a once-a-second status line on stderr. `--list-devices` prints
//! the device table and exits.
//!
//! Two machine-readable modes (US-UX-2) bypass the TUI: `--output json|binary`
//! serialises the live feature bus to stdout or a `--listen` socket (see
//! [`mod@stream`] and `docs/feature-stream.md`), and `--input <addr>` renders
//! the full TUI from a remote feature stream instead of a local capture engine.
//!
//! An optional config file supplies defaults and rebindable keys (see
//! [`mod@config`]); precedence is built-in defaults < config < CLI flags.
//!
//! Exit codes: `0` success, `1` runtime error, `2` usage / unsupported, `3` no
//! capture device.

mod config;
mod logging;
mod offline;
mod runrec;
mod stream;

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::thread::sleep;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

use scia_core::engine::EngineHealth;
use scia_core::{
    Activity, CaptureError, CpalBackend, DeviceKind, DeviceSelector, Encoding, Engine,
    EngineConfig, EngineError, EngineStats, FeatureReader, FullscreenWatch, Pacing, PerfModeState,
    Signal, StreamHealth, SyntheticBackend, list_devices,
};
use scia_scenes::{Preset, PresetWatcher, ReloadEvent, load_preset};
use scia_tui::{
    ChromeMode, Keymap, NowPlayingMode, PresenterMode, RunError, Tier, TuiOptions, run,
};

use config::Resolved;

/// The `CONFIG`, `KEYS` and `EXIT CODES` sections appended to the long `--help`.
const CONFIG_HELP: &str = "\
CONFIG:
  scia reads an optional TOML config for defaults and key bindings. Precedence,
  lowest to highest: built-in defaults < config file < command-line flags. A
  missing file is not an error.
    Unix:     $XDG_CONFIG_HOME/scia/config.toml  (else ~/.config/scia/config.toml)
    Windows:  %APPDATA%\\scia\\config.toml
  [defaults]  scene, presenter, overlay, perf_mode, demo_bpm, chrome, device,
              now_playing
              (scene = spectra (default) | any --list-scenes name | bars for the
              legacy spectrum-bars renderer;
              presenter = octant | sextant | quadrant | half | kitty | sixel;
              chrome = invisible | instrument | playful | utilitarian;
              now_playing = media-then-sources (default) | media | sources | off;
              device = exact capture device name; all overridden by their flags)
  [keys]      rebind actions scene_next, scene_prev, browser, overlay, pause,
              quit, chrome, now_playing, palette, tuning, mapping, devices. A
              value is a single character, a named
              key (tab, esc, left, right, up, down, enter, space, backtick), or
              ctrl+<key>. Unknown actions or unparseable keys warn and are
              ignored.

KEYS (defaults, all rebindable; press ? in-app for the active map):
  right/left  next / prev scene      tab    scene browser
  `           debug overlay          space  pause         q  quit
  n           now-playing panel      p      apply palette
  c           cycle chrome           t      tuning strip
  m           expression map         d      device picker  s  debug line
  a           scene author           esc    back (overlay) / quit
  ?           toggle help
  tuning:     tab param · ←/→ adjust · w write preset · esc done
  mapping:    ↑↓ row · ⏎ edit (⏎ apply · esc cancel) · w write · esc done
  devices:    ↑↓ select · ⏎ switch · p pin · esc close
  author:     ↑↓ scroll · PgUp/PgDn page · esc close

EXIT CODES:
  0  success            1  runtime error         2  usage / unsupported
  3  no capture device";

/// Per-query timeout for capability probing; the four queries stay well under
/// ~600 ms total.
const PROBE_TIMEOUT: Duration = Duration::from_millis(150);

/// The reserved scene name that selects the legacy direct spectrum-bars
/// renderer. It is deliberately *not* a registered scene id (see
/// [`scene_for_tui`]): asking for it maps to the TUI's internal none/direct
/// path, the escape hatch from the default scene engine.
const LEGACY_BARS_SCENE: &str = "bars";

/// Translate the resolved scene name into the value the TUI consumes.
///
/// The TUI treats `TuiOptions.scene == None` as the legacy direct-bars renderer
/// and `Some(name)` as a scene it must build (validating the name). Since the
/// scene now always resolves to a name (defaulting to `spectra`), the only way
/// to reach the legacy path is the reserved name [`LEGACY_BARS_SCENE`], which
/// this maps to `None`. Every other name passes through for the TUI to validate.
/// Pure, so the seam is unit-tested directly.
fn scene_for_tui(resolved: Option<&str>) -> Option<String> {
    match resolved {
        Some(name) if name == LEGACY_BARS_SCENE => None,
        other => other.map(str::to_owned),
    }
}

/// How an `--input` argument was interpreted by [`classify_input`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum InputSource {
    /// A TCP address to connect to (an `--output --listen` server).
    Tcp(String),
    /// A recorded clip file on disk to replay.
    File(PathBuf),
}

/// Whether an `--input` argument looks like a socket address (`ip:port` or
/// `host:port`) rather than a filesystem path — decided syntactically, without
/// touching the network or the filesystem, so it is unit-tested directly.
///
/// An `ip:port` (v4 or bracketed v6) parses straight to a [`std::net::SocketAddr`].
/// A `host:port` with a DNS name (which does not parse as a `SocketAddr`) is
/// recognised by shape: the last `:` splits a non-empty host from a numeric
/// `u16` port. A path with no such trailing `:port` (`clip.bin`,
/// `/clips/song.bin`, `./take-2`) is not address-like.
fn looks_like_socket_addr(arg: &str) -> bool {
    if arg.parse::<std::net::SocketAddr>().is_ok() {
        return true;
    }
    match arg.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && port.parse::<u16>().is_ok(),
        None => false,
    }
}

/// Decide what an `--input` argument names: an address-looking string is a TCP
/// endpoint (as before); otherwise an existing path is a clip file to replay;
/// otherwise it is an error naming both interpretations. The address check comes
/// first so the socket path is unchanged. Pure apart from the path-existence
/// probe, so the address/garbage arms are unit-tested directly.
fn classify_input(arg: &str) -> Result<InputSource, String> {
    if looks_like_socket_addr(arg) {
        return Ok(InputSource::Tcp(arg.to_string()));
    }
    let path = Path::new(arg);
    if path.exists() {
        return Ok(InputSource::File(path.to_path_buf()));
    }
    Err(format!(
        "--input '{arg}' is neither a socket address (host:port) nor a path to an existing clip file"
    ))
}

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

    /// Automatically pause rendering while a fullscreen-exclusive app (a game) is
    /// foreground, resuming when it exits (US-PERF-3). On by default; this flag
    /// forces it on over a config `[defaults] fullscreen_pause = false`. Windows
    /// only — every other platform reports no fullscreen app in v1.
    #[arg(long, overrides_with = "no_fullscreen_pause")]
    fullscreen_pause: bool,

    /// Disable the automatic fullscreen-app pause (US-PERF-3): keep rendering
    /// even while a fullscreen game is foreground. Overrides the config
    /// `[defaults] fullscreen_pause`.
    #[arg(long, overrides_with = "fullscreen_pause")]
    no_fullscreen_pause: bool,

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

    /// Which scene to render (from --list-scenes). Defaults to `spectra` when
    /// unset. The reserved name `bars` selects the legacy direct spectrum-bars
    /// renderer instead of a scene. Invalid names exit 2 listing the available
    /// presets. Not valid with --headless.
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

    /// What the now-playing surfaces name: `media-then-sources` (a playing media
    /// session, else the app feeding the mix — a game — by name), `media` (media
    /// sessions only), `sources` (the audio source only), or `off`. Overrides the
    /// config `[defaults] now_playing`; the built-in default is
    /// `media-then-sources`.
    #[arg(long = "now-playing", value_enum, value_name = "MODE")]
    now_playing: Option<NowPlayingArg>,

    /// Headless machine-readable output: emit versioned per-frame feature
    /// frames (the FeatureSnapshot contract) instead of running the TUI. `json`
    /// writes NDJSON (one frame object per line); `binary` writes a
    /// length-prefixed little-endian stream. Frames go to stdout, or to a socket
    /// with --listen, paced at --rate. See docs/feature-stream.md for the
    /// schema. Works with --demo (needs no audio hardware).
    #[arg(
        long,
        value_enum,
        value_name = "FORMAT",
        conflicts_with_all = ["input", "headless", "scene", "scene_file", "presenter", "chrome", "now_playing", "overlay", "debug"]
    )]
    output: Option<OutputFormat>,

    /// Serve the feature stream on a TCP address (e.g. 127.0.0.1:9000) instead
    /// of stdout; every connected client receives the stream. Only valid with
    /// --output.
    #[arg(long, value_name = "ADDR", requires = "output")]
    listen: Option<String>,

    /// Feature frames per second for --output (1..=1000, default 60). While the
    /// engine is idle the stream drops to a slower keepalive cadence regardless.
    /// Only valid with --output. Rejected with --from-file (offline mode emits
    /// one frame per DSP hop, not a subsampled rate).
    #[arg(long, value_name = "N", requires = "output")]
    rate: Option<u32>,

    /// Render an audio FILE through the exact live DSP chain into a feature
    /// stream, faster than realtime and bit-for-bit deterministic (US-CORPUS).
    /// The input is WAV only: 16- or 24-bit PCM or 32-bit IEEE float, 48000 Hz,
    /// mono or stereo (mono is duplicated to stereo). Requires --output; the
    /// stream is one frame per DSP hop (the native ~187 fps hop cadence, richer
    /// than the live --rate subsampling), so --rate is rejected. Mutually
    /// exclusive with --input, --listen and every live-capture flag. Redirect
    /// stdout to record a clip: `scia --from-file take.wav --output binary >
    /// clip.bin`.
    #[arg(
        long,
        value_name = "PATH",
        requires = "output",
        conflicts_with_all = ["input", "listen", "rate", "demo", "demo_signal", "demo_bpm", "device", "pipewire", "no_pipewire", "perf_mode", "no_route_watch", "headless", "seconds", "list_devices"]
    )]
    from_file: Option<PathBuf>,

    /// Linear gain in decibels applied to the input samples before the DSP, so
    /// corpus prep can loudness-normalize externally-measured files without
    /// re-encoding. Only valid with --from-file; the default is 0 dB (no change).
    /// A negative value attenuates (e.g. `--gain-db -6`).
    #[arg(
        long,
        value_name = "DB",
        requires = "from_file",
        allow_hyphen_values = true
    )]
    gain_db: Option<f32>,

    /// Render the full TUI from a feature stream instead of capturing local
    /// audio. The argument is either a TCP address (e.g. 127.0.0.1:9000) served
    /// by another scia running `--output --listen` — reconnecting automatically
    /// if the stream drops — or the path to a recorded clip file on disk (a
    /// `scia --output binary > clip.bin` capture), replayed live at its recorded
    /// cadence. An address is tried first; otherwise an existing path replays;
    /// anything else errors naming both interpretations. Mutually exclusive with
    /// the local-capture flags.
    #[arg(
        long,
        value_name = "ADDR|PATH",
        conflicts_with_all = ["demo", "demo_signal", "demo_bpm", "device", "pipewire", "no_pipewire", "perf_mode", "no_route_watch", "headless", "seconds", "list_devices"]
    )]
    input: Option<String>,

    /// When --input names a clip file (not a TCP address), seamlessly restart it
    /// at end of file for extended A/B listening. Only valid with a file
    /// `--input`; an error otherwise.
    #[arg(long = "input-loop", requires = "input")]
    input_loop: bool,

    /// List every registered scene and built-in preset, then exit.
    #[arg(long)]
    list_scenes: bool,

    /// Enable structured logging at this level (`error|warn|info|debug|trace`).
    /// Overrides `SCIA_LOG` and the config `[log] level`. Logs go to a rotating
    /// JSON-lines file under the config dir, and also to stderr in headless /
    /// stream modes (never while the TUI owns the screen). Off by default. See
    /// docs/logging.md.
    #[arg(long, value_enum, value_name = "LEVEL")]
    log: Option<logging::LogLevel>,

    /// Write a machine-readable per-run record (JSON Lines) to this path: a
    /// `run_start`, one `hop` per recorded hop (every 4th live, every hop when
    /// replaying via `--input`), `event`s for scene/preset swaps and device
    /// switches, and a `run_end`. The data plane for the scene-quality harness.
    /// See docs/logging.md.
    #[arg(long, value_name = "PATH")]
    log_run: Option<PathBuf>,
}

/// The machine-readable output encodings selectable with `--output`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    /// NDJSON: one feature-frame object per line.
    Json,
    /// Length-prefixed little-endian binary with a one-time stream header.
    Binary,
}

impl OutputFormat {
    /// The [`Encoding`] this flag value selects.
    fn encoding(self) -> Encoding {
        match self {
            OutputFormat::Json => Encoding::Json,
            OutputFormat::Binary => Encoding::Binary,
        }
    }
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

/// The now-playing source policies selectable with `--now-playing` (or the
/// config `[defaults] now_playing`). Mirrors [`scia_tui::NowPlayingMode`] so clap
/// owns the flag parsing without the TUI crate taking a clap dependency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum NowPlayingArg {
    /// A playing media session, else the dominant audio source by name.
    MediaThenSources,
    /// Media sessions only; never name a bare audio source.
    Media,
    /// The dominant audio source only; never show media metadata.
    Sources,
    /// Show nothing.
    Off,
}

impl NowPlayingArg {
    /// The [`NowPlayingMode`] this flag value selects.
    fn mode(self) -> NowPlayingMode {
        match self {
            NowPlayingArg::MediaThenSources => NowPlayingMode::MediaThenSources,
            NowPlayingArg::Media => NowPlayingMode::Media,
            NowPlayingArg::Sources => NowPlayingMode::Sources,
            NowPlayingArg::Off => NowPlayingMode::Off,
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
    // The two fullscreen-pause flags fold into one tri-state for the config
    // layer: force-off wins over force-on (both cannot be set — `overrides_with`
    // makes the later flag win — but force-off is the safe reading if that ever
    // changed), and neither set falls through to config, then the default.
    let cli_fullscreen_pause = if cli.no_fullscreen_pause {
        Some(false)
    } else if cli.fullscreen_pause {
        Some(true)
    } else {
        None
    };
    let resolved = config::resolve(
        config::CliLayer {
            scene: cli.scene.clone(),
            presenter: cli.presenter,
            overlay: cli.overlay,
            perf_mode: cli.perf_mode,
            fullscreen_pause: cli_fullscreen_pause,
            demo_bpm: cli.demo_bpm,
            chrome: cli.chrome.map(ChromeArg::mode),
            device: cli.device.clone(),
            now_playing: cli.now_playing.map(NowPlayingArg::mode),
        },
        &cfg.defaults,
    );

    // Structured logging (off by default). The stderr sink is gated off whenever
    // the TUI owns the screen, so a log line can never corrupt it: `--output`
    // and `--headless` runs have no TUI; `--input` and a bare demo/live run do.
    let tui_active = if cli.output.is_some() {
        false
    } else if cli.input.is_some() {
        true
    } else {
        !cli.headless
    };
    let mut log_warnings = Vec::new();
    let level = logging::resolve_level(cli.log, &cfg.log, &mut log_warnings);
    for warning in &log_warnings {
        eprintln!("{warning}");
    }
    if let Some(note) = logging::init(
        level,
        tui_active,
        cfg.log.file.unwrap_or(true),
        config::config_dir().as_deref(),
    ) {
        eprintln!("{note}");
    }

    // Headless machine-readable output: no TUI. Drives the same engine (demo or
    // live capture) and serialises its feature bus to stdout or a socket. With
    // --from-file the source is an audio file rendered offline through the DSP
    // chain instead of a live engine (--from-file requires --output, so the
    // format is always present here).
    if let Some(format) = cli.output {
        if let Some(path) = cli.from_file.clone() {
            return offline::run_from_file(&path, format.encoding(), cli.gain_db.unwrap_or(0.0));
        }
        return run_output_mode(&cli, &resolved, format.encoding());
    }

    // Thin frontend: render the full TUI from a remote feature stream instead of
    // a local capture engine.
    if let Some(spec) = cli.input.clone() {
        return run_input_mode(&cli, &resolved, spec, cfg.keymap);
    }

    if cli.demo {
        run_demo(&cli, &resolved, cfg.keymap)
    } else {
        run_live(&cli, &resolved, cfg.keymap)
    }
}

/// Start the engine for a headless `--output` run: the synthetic feed under
/// `--demo`, otherwise live capture (mirroring [`run_live`]'s backend and
/// config). Capture info and any perf-mode note go to stderr so stdout carries
/// only the data stream.
fn start_stream_engine(
    cli: &Cli,
    resolved: &Resolved,
) -> Result<(Engine, FeatureReader), ExitCode> {
    if cli.demo {
        let bpm = resolved.demo_bpm.clamp(40.0, 220.0);
        let backend = SyntheticBackend {
            signal: cli.demo_signal.signal(bpm),
            pacing: Pacing::Realtime,
            ..SyntheticBackend::default()
        };
        return Engine::start(Box::new(backend), EngineConfig::default()).map_err(|err| {
            eprintln!("failed to start engine: {err}");
            ExitCode::from(1)
        });
    }

    let selector = match &resolved.device {
        Some(name) => DeviceSelector::Named(name.clone()),
        None => DeviceSelector::Default,
    };
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
    match Engine::start(Box::new(backend), config) {
        Ok((engine, reader)) => {
            let format = engine.format();
            eprintln!("capture: {} Hz, {} ch", format.sample_rate, format.channels);
            if let StreamHealth::Errored(msg) = engine.health() {
                eprintln!("capture stream error: {msg}");
                engine.stop();
                return Err(ExitCode::from(1));
            }
            Ok((engine, reader))
        }
        Err(EngineError::Capture(CaptureError::NoDevice)) => {
            eprintln!("no capture device available; try --list-devices, or --demo");
            Err(ExitCode::from(3))
        }
        Err(err) => {
            eprintln!("failed to start capture: {err}");
            Err(ExitCode::from(1))
        }
    }
}

/// Headless `--output` mode: start the engine and stream its feature bus, no
/// TUI. The rate is clamped to `1..=1000`.
fn run_output_mode(cli: &Cli, resolved: &Resolved, encoding: Encoding) -> ExitCode {
    let rate = cli
        .rate
        .unwrap_or(stream::DEFAULT_STREAM_RATE)
        .clamp(1, 1000);
    let (engine, reader) = match start_stream_engine(cli, resolved) {
        Ok(pair) => pair,
        Err(code) => return code,
    };
    stream::run_output(
        encoding,
        cli.listen.clone(),
        rate,
        cli.frames,
        engine,
        reader,
    )
}

/// `--input` mode: render the full TUI from a feature stream — a remote socket
/// or a recorded clip file on disk. The local feature bus is fed by a background
/// producer (connecting to the address, or replaying the file); the TUI polls
/// the producer's state for its health/quiet display.
fn run_input_mode(cli: &Cli, resolved: &Resolved, spec: String, keymap: Keymap) -> ExitCode {
    let source = match classify_input(&spec) {
        Ok(source) => source,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };
    // `--input-loop` only means something for a clip file; reject it on a socket
    // rather than silently ignoring it.
    if cli.input_loop && matches!(source, InputSource::Tcp(_)) {
        eprintln!("--input-loop is only valid when --input names a clip file, not a TCP address");
        return ExitCode::from(2);
    }

    // Spawn the matching producer and derive the TUI's source/label strings.
    let (reader, handle, source_str, label) = match source {
        InputSource::Tcp(addr) => {
            let (reader, handle) = stream::start_input(addr.clone());
            let label = format!("INPUT — {addr}");
            (reader, handle, addr, label)
        }
        InputSource::File(path) => {
            let (reader, handle) = stream::start_input_file(path.clone(), cli.input_loop);
            // Name the clip by its file name on the local screen — a full path is
            // both noisy and needless in the label.
            let name = path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            let label = if cli.input_loop {
                format!("CLIP — {name} (loop)")
            } else {
                format!("CLIP — {name}")
            };
            (reader, handle, name, label)
        }
    };

    // A `--scene-file` preset (if any) is loaded before the TUI takes the
    // terminal; `_watcher` must outlive `run` to keep the watch alive.
    let (preset, reload, _watcher) = match scene_file_setup(cli) {
        Ok(parts) => parts,
        Err(code) => {
            handle.stop();
            return code;
        }
    };

    // No local capture engine in `--input` mode, so the pause only stops the
    // render loop; the remote producer downshifts its own idle path. The watch
    // drives a standalone flag; `_fs_watch` must outlive `run`.
    let (fullscreen_pause, _fs_watch) =
        fullscreen_pause_setup(resolved.fullscreen_pause, Arc::new(AtomicBool::new(false)));

    let (presenter_mode, initial_notice) = select_presenter(resolved);
    tracing::info!(target: "scia::tui", presenter = ?presenter_mode, "presenter tier selected");
    let opts = TuiOptions {
        fps: cli.fps,
        label: Some(label),
        source: source_str,
        frames: cli.frames,
        debug: cli.debug,
        overlay: resolved.overlay,
        scene: scene_for_tui(resolved.scene.as_deref()),
        preset,
        presenter_mode,
        initial_notice,
        keymap,
        chrome: resolved.chrome,
        now_playing_mode: resolved.now_playing,
        scene_file: cli.scene_file.clone(),
        config_dir: config::config_dir(),
        device: DeviceSelector::Default,
        prefer_pipewire: true,
        // A remote stream is not a local WSL capture; the WSL notice never applies.
        wsl: false,
        fullscreen_pause,
    };

    let stats = handle.stats_fn();
    let health = handle.health_fn();
    let clock = handle.clock_fn();
    // Replaying a remote stream records every hop (the stream is paced well
    // below the live hop rate). The remote format is unknown until frames
    // arrive, so the nominal hop period assumes the standard 48 kHz.
    let observer = run_observer(build_run_recorder(
        cli,
        resolved,
        "replay",
        48_000,
        runrec::Throttle::EveryHop,
    ));
    // A remote stream has no local device to switch; the picker is inert.
    let outcome = run(
        reader,
        stats,
        health,
        clock,
        |_sel| {},
        reload,
        opts,
        observer,
    );
    handle.stop();
    report_tui_outcome(outcome)
}

/// Print every scene (built-in and Luau) and the built-in preset names, then
/// exit 0.
fn print_scene_list() -> ExitCode {
    let luau: Vec<&str> = scia_scenes::luau_scene_ids();
    println!("{:<12}  {:<10}  summary", "scene", "mood");
    for info in scia_scenes::catalog_scenes() {
        let tag = if luau.contains(&info.id) {
            " [luau]"
        } else {
            ""
        };
        println!("{:<12}  {:<10}  {}{tag}", info.id, info.mood, info.summary);
    }
    println!("\npresets:");
    for (name, _) in scia_scenes::builtin_presets() {
        println!("  {name}");
    }
    for name in scia_scenes::discovered_preset_names() {
        println!("  {name} [drop-in]");
    }
    if let Some(dir) = scia_scenes::scenes_dir() {
        println!(
            "\nDrop `.lua` scenes in {} to add your own; they list above and load with `--scene <id>`.",
            dir.display()
        );
    }
    if let Some(dir) = scia_scenes::presets_dir() {
        println!(
            "Drop `.toml` presets in {} to add your own; they list above and load with `--scene <name>`.",
            dir.display()
        );
    }
    println!(
        "\nBare `scia` opens the {} scene; pass `--scene {}` for the legacy spectrum-bars renderer.",
        config::DEFAULT_SCENE,
        LEGACY_BARS_SCENE
    );
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

/// Build the `--log-run` recorder for a session, or `None` when the flag is
/// unset. A file that cannot be opened is warned about and then ignored, so a
/// logging failure never aborts the visualizer. `source` tags the input
/// (`"synthetic"`, `"live"`, `"replay"`); `sample_rate` sets the nominal hop
/// period; `throttle` records every hop for a clip replay, every fourth live.
fn build_run_recorder(
    cli: &Cli,
    resolved: &Resolved,
    source: &str,
    sample_rate: u32,
    throttle: runrec::Throttle,
) -> Option<runrec::RunRecorder> {
    let path = cli.log_run.as_ref()?;
    let scene = resolved
        .scene
        .clone()
        .unwrap_or_else(|| LEGACY_BARS_SCENE.to_string());
    let preset = cli.scene_file.as_ref().map(|p| p.display().to_string());
    let hop_ms = 256_000.0 / sample_rate.max(1) as f32;
    match runrec::RunRecorder::create(
        path,
        throttle,
        &scene,
        preset,
        std::collections::BTreeMap::new(),
        source,
        hop_ms,
    ) {
        Ok(recorder) => Some(recorder),
        Err(err) => {
            eprintln!("--log-run: cannot write {}: {err}", path.display());
            None
        }
    }
}

/// Box a recorder as the TUI's run observer (the loop drives it per frame).
fn run_observer(recorder: Option<runrec::RunRecorder>) -> Option<Box<dyn scia_tui::RunObserver>> {
    recorder.map(|r| Box::new(r) as Box<dyn scia_tui::RunObserver>)
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
        let recorder = build_run_recorder(
            cli,
            resolved,
            "synthetic",
            engine.format().sample_rate,
            runrec::Throttle::EveryFourth,
        );
        return run_headless(engine, reader, cli.seconds, recorder);
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

    // The pause forces the engine's DSP idle downshift and stops the render loop
    // on one shared flag; `_fs_watch` must outlive `run`.
    let (fullscreen_pause, _fs_watch) =
        fullscreen_pause_setup(resolved.fullscreen_pause, engine.pause_flag());

    let (presenter_mode, initial_notice) = select_presenter(resolved);
    tracing::info!(target: "scia::tui", presenter = ?presenter_mode, "presenter tier selected");
    let opts = TuiOptions {
        fps: cli.fps,
        label: Some("DEMO — synthetic feed".to_string()),
        source: String::new(),
        frames: cli.frames,
        debug: cli.debug,
        overlay: resolved.overlay,
        scene: scene_for_tui(resolved.scene.as_deref()),
        preset,
        presenter_mode,
        initial_notice,
        keymap,
        chrome: resolved.chrome,
        now_playing_mode: resolved.now_playing,
        scene_file: cli.scene_file.clone(),
        config_dir: config::config_dir(),
        device: DeviceSelector::Default,
        prefer_pipewire: true,
        // The demo feed needs no audio hardware, so the WSL notice never applies.
        wsl: false,
        fullscreen_pause,
    };

    let observer = run_observer(build_run_recorder(
        cli,
        resolved,
        "synthetic",
        engine.format().sample_rate,
        runrec::Throttle::EveryFourth,
    ));
    let outcome = run(
        reader,
        || engine.stats(),
        || engine.engine_health(),
        || engine.now_ns(),
        // The synthetic demo feed has no device to switch; the picker is inert.
        |_sel| {},
        reload,
        opts,
        observer,
    );
    engine.stop();
    report_tui_outcome(outcome)
}

/// Start live capture on the cpal backend and run the TUI or the headless
/// status loop.
fn run_live(cli: &Cli, resolved: &Resolved, keymap: Keymap) -> ExitCode {
    // A Linux process inside WSL cannot see the Windows system mix (WSL carries
    // only WSL-app audio). Detect it and say so plainly — here on the way in and
    // in-app via the guidance overlay — rather than presenting a black screen
    // reacting only to WSL sounds. Capture still proceeds: WSL-app audio is
    // legitimate, just labeled.
    let wsl = scia_core::detect_wsl();
    if wsl {
        eprintln!(
            "note: running inside WSL — Windows system audio is not visible here \
             (WSL carries only WSL-app audio)."
        );
        eprintln!(
            "  · run scia.exe from this shell to visualize Windows audio directly \
             (the Windows PATH is on your WSL PATH), or"
        );
        eprintln!(
            "  · run scia-bridge on Windows and render it with `scia --input <windows-host>:7526`."
        );
        eprintln!("  see docs/wsl.md for both paths.");
    }

    // macOS captures the system mix through a Core Audio process tap, which is
    // gated by the "System Audio Recording" TCC permission. Pre-explain the
    // prompt before it fires, so a first run is not a surprise dialog over a
    // black screen; if the tap then delivers nothing (denied or unanswered) the
    // in-app notice repeats the recovery path (see docs/macos.md).
    #[cfg(target_os = "macos")]
    {
        eprintln!(
            "note: capturing system audio on macOS uses a Core Audio process tap (macOS 14.4+)."
        );
        eprintln!(
            "  · macOS will ask to allow \"System Audio Recording\" — click Allow to visualize \
             system audio."
        );
        eprintln!(
            "  · if you denied it before, enable scia under System Settings > Privacy & Security \
             > Screen & System Audio Recording."
        );
        eprintln!(
            "  · on macOS older than 14.4, install a loopback device (e.g. BlackHole) and select \
             it with --device. see docs/macos.md."
        );
    }

    // Device precedence is already merged (flag > config > default) in `resolved`.
    let selector = match &resolved.device {
        Some(name) => DeviceSelector::Named(name.clone()),
        None => DeviceSelector::Default,
    };
    // With the mutually-exclusive flags, later wins; the default (neither set)
    // prefers PipeWire.
    let prefer_pipewire = cli.pipewire || !cli.no_pipewire;
    let backend = CpalBackend {
        device: selector.clone(),
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
    match capture_host(&resolved.device) {
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
        let recorder = build_run_recorder(
            cli,
            resolved,
            "live",
            format.sample_rate,
            runrec::Throttle::EveryFourth,
        );
        return run_headless(engine, reader, cli.seconds, recorder);
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
    // The pause forces the engine's DSP idle downshift and stops the render loop
    // on one shared flag; `_fs_watch` must outlive `run`.
    let (fullscreen_pause, _fs_watch) =
        fullscreen_pause_setup(resolved.fullscreen_pause, engine.pause_flag());

    let (presenter_mode, initial_notice) = select_presenter(resolved);
    tracing::info!(target: "scia::tui", presenter = ?presenter_mode, "presenter tier selected");
    let opts = TuiOptions {
        fps: cli.fps,
        label: None,
        source,
        frames: cli.frames,
        debug: cli.debug,
        overlay: resolved.overlay,
        scene: scene_for_tui(resolved.scene.as_deref()),
        preset,
        presenter_mode,
        initial_notice,
        keymap,
        chrome: resolved.chrome,
        now_playing_mode: resolved.now_playing,
        scene_file: cli.scene_file.clone(),
        config_dir: config::config_dir(),
        device: selector,
        prefer_pipewire,
        // Opens the WSL guidance overlay at startup when detected above.
        wsl,
        fullscreen_pause,
    };

    let observer = run_observer(build_run_recorder(
        cli,
        resolved,
        "live",
        format.sample_rate,
        runrec::Throttle::EveryFourth,
    ));
    let outcome = run(
        reader,
        || engine.stats(),
        || engine.engine_health(),
        || engine.now_ns(),
        |sel| {
            // Record the new selector and drive the route watcher to reopen on
            // the newly chosen device; the reopen never blocks the UI thread.
            engine.set_device(sel);
            engine.request_reopen();
        },
        reload,
        opts,
        observer,
    );
    engine.stop();
    // The WSL overlay's `[s]` key asks to leave live capture for the demo feed:
    // the live engine is torn down above; relaunch in demo mode.
    if let Ok(summary) = &outcome {
        if summary.demo_requested {
            return run_demo(cli, resolved, keymap);
        }
    }
    report_tui_outcome(outcome)
}

/// Wire up the fullscreen-app pause (US-PERF-3) for a TUI run.
///
/// When `enabled`, spawns the platform [`FullscreenWatch`] driving `flag` (the
/// same `Arc` the render loop reads to stop drawing and — for a local engine —
/// the DSP thread reads to force its idle downshift), and returns the flag to
/// hand the TUI plus the watch guard, which must outlive `run` to keep polling.
/// When disabled, returns `(None, None)` so the loop never pauses and no thread
/// is started.
fn fullscreen_pause_setup(
    enabled: bool,
    flag: Arc<AtomicBool>,
) -> (Option<Arc<AtomicBool>>, Option<FullscreenWatch>) {
    if enabled {
        let watch = FullscreenWatch::spawn(
            scia_core::fullscreen_detector(),
            scia_core::FULLSCREEN_POLL,
            Arc::clone(&flag),
        );
        (Some(flag), Some(watch))
    } else {
        (None, None)
    }
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
/// `seconds` is `0`. A device switch or fault is ridden out as a `reconnecting…`
/// state; the loop exits 1 only once capture has failed past the reconnect
/// deadline ([`EngineHealth::Failed`]).
fn run_headless(
    engine: Engine,
    mut reader: FeatureReader,
    seconds: u64,
    mut recorder: Option<runrec::RunRecorder>,
) -> ExitCode {
    let mut t: u64 = 0;
    loop {
        // Wait out the one-second status tick. With a `--log-run` recorder
        // attached, spend that second sampling the feature bus fast enough to
        // catch the per-hop generations (the recorder de-duplicates and applies
        // its own throttle); headless has no scene engine, so the scene id is
        // `None` and no swap events are emitted.
        wait_one_second(&mut reader, recorder.as_mut());
        t += 1;

        let stats = engine.stats();
        let snap = *reader.latest();
        let (bar, val) = loudest_bar(&snap.spectrum[..snap.spectrum_len as usize]);
        let health = engine.engine_health();
        let last_err = engine.last_reopen_error();
        eprintln!(
            "{}",
            format_status_line(
                &stats,
                snap.generation,
                snap.rms,
                snap.peak,
                bar,
                val,
                &health,
                last_err.as_deref()
            )
        );

        if let EngineHealth::Failed { error } = &health {
            eprintln!("capture stream error: {error}");
            // The recorder drops here, terminating the run record.
            engine.stop();
            return ExitCode::from(1);
        }

        if seconds != 0 && t >= seconds {
            break;
        }
    }
    if let Some(recorder) = recorder {
        let _ = recorder.finish();
    }
    engine.stop();
    ExitCode::SUCCESS
}

/// Sleep for one second. When a run recorder is attached, poll the feature bus
/// every couple of milliseconds across that second so the recorder observes each
/// hop generation; otherwise a plain one-second sleep.
fn wait_one_second(reader: &mut FeatureReader, recorder: Option<&mut runrec::RunRecorder>) {
    let Some(recorder) = recorder else {
        sleep(Duration::from_secs(1));
        return;
    };
    let until = std::time::Instant::now() + Duration::from_secs(1);
    while std::time::Instant::now() < until {
        recorder.observe(reader.latest(), None);
        sleep(Duration::from_millis(2));
    }
}

/// Format one headless status line. Pure (no engine handle) so the formatting —
/// the reopen counters, the reconnecting/failed suffix, and the last reopen
/// error — is unit-tested directly.
///
/// `reopens N fail M` is appended whenever either counter is nonzero;
/// `reconnecting… <ms>ms attempt <n>` while [`EngineHealth::Reconnecting`],
/// `FAILED: <err>` on [`EngineHealth::Failed`]; and the last reopen error text
/// is appended as `last-err "<msg>"` whenever a reopen has failed.
#[allow(clippy::too_many_arguments)]
fn format_status_line(
    stats: &EngineStats,
    generation: u64,
    rms: f32,
    peak: f32,
    bar: usize,
    val: f32,
    health: &EngineHealth,
    last_reopen_error: Option<&str>,
) -> String {
    let mut suffix = String::new();
    if stats.reopens > 0 || stats.reopen_failures > 0 {
        suffix.push_str(&format!(
            "  reopens {} fail {}",
            stats.reopens, stats.reopen_failures
        ));
    }
    match health {
        EngineHealth::Reconnecting { since_ms, attempts } => {
            suffix.push_str(&format!("  reconnecting… {since_ms}ms attempt {attempts}"));
        }
        EngineHealth::Unavailable { message } => {
            suffix.push_str(&format!("  UNAVAILABLE: {message}"));
        }
        EngineHealth::Failed { error } => {
            suffix.push_str(&format!("  FAILED: {error}"));
        }
        EngineHealth::Ok => {}
    }
    if stats.reopen_failures > 0 {
        if let Some(err) = last_reopen_error {
            suffix.push_str(&format!("  last-err {err:?}"));
        }
    }
    format!(
        "act {}  gen {}  rms {:.4}  peak {:.4}  loudest {}({:.3})  push {}  gap {:.1}ms  \
         dropped {}{}",
        activity_label(stats.activity),
        generation,
        rms,
        peak,
        bar,
        val,
        stats.pushes,
        stats.max_gap_ms,
        stats.dropped_frames,
        suffix,
    )
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
    fn scene_for_tui_maps_bars_to_the_legacy_none_path() {
        // The reserved name reaches the TUI as `None`, which the TUI renders as
        // the legacy direct-bars presenter.
        assert_eq!(scene_for_tui(Some(LEGACY_BARS_SCENE)), None);
        assert_eq!(scene_for_tui(Some("bars")), None);
    }

    #[test]
    fn scene_for_tui_passes_real_scene_names_through() {
        // The default and every other name pass through for the TUI to build and
        // validate.
        assert_eq!(
            scene_for_tui(Some(config::DEFAULT_SCENE)),
            Some("spectra".to_string())
        );
        assert_eq!(scene_for_tui(Some("aurora")), Some("aurora".to_string()));
        assert_eq!(scene_for_tui(None), None);
    }

    #[test]
    fn reserved_bars_name_is_not_a_registered_scene() {
        // The escape hatch depends on `bars` never colliding with a real scene
        // id, so the mapping in `scene_for_tui` can own the name unambiguously.
        assert!(
            scia_scenes::builtin_scenes()
                .iter()
                .all(|info| info.id != LEGACY_BARS_SCENE),
            "no builtin scene may be named `{LEGACY_BARS_SCENE}`"
        );
    }

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

    #[test]
    fn now_playing_flag_parses_each_mode() {
        for (arg, expected) in [
            ("media-then-sources", NowPlayingArg::MediaThenSources),
            ("media", NowPlayingArg::Media),
            ("sources", NowPlayingArg::Sources),
            ("off", NowPlayingArg::Off),
        ] {
            let cli = Cli::try_parse_from(["scia", "--now-playing", arg]).expect("parses");
            assert_eq!(cli.now_playing, Some(expected));
            assert_eq!(cli.now_playing.unwrap().mode(), expected.mode());
        }
    }

    #[test]
    fn unknown_now_playing_flag_is_rejected() {
        assert!(Cli::try_parse_from(["scia", "--now-playing", "everything"]).is_err());
    }

    #[test]
    fn now_playing_conflicts_with_output() {
        // A TUI-only concept has no meaning under headless output.
        assert!(Cli::try_parse_from(["scia", "--output", "json", "--now-playing", "off"]).is_err());
    }

    #[test]
    fn output_flag_parses_both_formats() {
        let json = Cli::try_parse_from(["scia", "--output", "json"]).expect("parses");
        assert_eq!(json.output, Some(OutputFormat::Json));
        assert_eq!(json.output.unwrap().encoding(), Encoding::Json);
        let binary = Cli::try_parse_from(["scia", "--output", "binary"]).expect("parses");
        assert_eq!(binary.output.unwrap().encoding(), Encoding::Binary);
    }

    #[test]
    fn output_conflicts_with_input() {
        assert!(
            Cli::try_parse_from(["scia", "--output", "json", "--input", "127.0.0.1:9000"]).is_err()
        );
    }

    #[test]
    fn output_conflicts_with_ui_only_flags() {
        // A UI-only flag has no meaning under headless output.
        assert!(Cli::try_parse_from(["scia", "--output", "json", "--overlay"]).is_err());
        assert!(Cli::try_parse_from(["scia", "--output", "json", "--scene", "aurora"]).is_err());
    }

    #[test]
    fn listen_and_rate_require_output() {
        // `--listen` / `--rate` are meaningless without `--output`.
        assert!(Cli::try_parse_from(["scia", "--listen", "127.0.0.1:9000"]).is_err());
        assert!(Cli::try_parse_from(["scia", "--rate", "30"]).is_err());
        // ...and valid alongside it.
        assert!(
            Cli::try_parse_from(["scia", "--output", "json", "--listen", "127.0.0.1:9000"]).is_ok()
        );
        assert!(Cli::try_parse_from(["scia", "--output", "json", "--rate", "30"]).is_ok());
    }

    #[test]
    fn socket_addr_shapes_are_recognised_as_addresses() {
        // ip:port (v4 and bracketed v6) and host:port all read as addresses.
        for s in [
            "127.0.0.1:9000",
            "0.0.0.0:7526",
            "[::1]:9000",
            "localhost:9000",
            "windows-host:7526",
            "stream-host:1234",
        ] {
            assert!(looks_like_socket_addr(s), "{s} should look like an address");
        }
    }

    #[test]
    fn file_paths_are_not_recognised_as_addresses() {
        // No trailing `:port`, or a non-numeric/oversized port → a path.
        for s in [
            "clip.bin",
            "/clips/song.bin",
            "./take-2",
            "recording",
            "clip:notaport",
            "host:99999", // > u16::MAX
            "trailing:",
        ] {
            assert!(
                !looks_like_socket_addr(s),
                "{s} should not look like an address"
            );
        }
    }

    #[test]
    fn classify_input_routes_addresses_files_and_garbage() {
        // An address-looking string is TCP without touching the filesystem.
        assert_eq!(
            classify_input("127.0.0.1:9000"),
            Ok(InputSource::Tcp("127.0.0.1:9000".to_string()))
        );
        assert_eq!(
            classify_input("windows-host:7526"),
            Ok(InputSource::Tcp("windows-host:7526".to_string()))
        );

        // An existing, non-address path replays as a file.
        let mut path = std::env::temp_dir();
        path.push(format!("scia-classify-{}.bin", std::process::id()));
        std::fs::write(&path, b"SCIA").expect("write temp clip");
        let arg = path.to_str().expect("utf8 path");
        assert_eq!(
            classify_input(arg),
            Ok(InputSource::File(path.clone())),
            "an existing file path replays as a clip"
        );
        std::fs::remove_file(&path).ok();

        // Neither an address nor an existing path is a clear error naming both.
        let err = classify_input("no-such-clip-xyz").expect_err("garbage errors");
        assert!(err.contains("socket address"), "err: {err}");
        assert!(err.contains("clip file"), "err: {err}");
    }

    #[test]
    fn input_loop_requires_input() {
        // `--input-loop` on its own is rejected (it requires `--input`).
        assert!(Cli::try_parse_from(["scia", "--input-loop"]).is_err());
        // Alongside `--input` it parses (the file-vs-address runtime check is in
        // run_input_mode).
        let cli =
            Cli::try_parse_from(["scia", "--input", "clip.bin", "--input-loop"]).expect("parses");
        assert!(cli.input_loop);
    }

    #[test]
    fn input_conflicts_with_local_capture_flags() {
        // `--input` renders from a remote stream, so local-capture flags conflict.
        assert!(Cli::try_parse_from(["scia", "--input", "127.0.0.1:9000", "--demo"]).is_err());
        assert!(
            Cli::try_parse_from(["scia", "--input", "127.0.0.1:9000", "--device", "x"]).is_err()
        );
        // A bare `--input` with a TUI option is fine.
        assert!(Cli::try_parse_from(["scia", "--input", "127.0.0.1:9000", "--overlay"]).is_ok());
    }

    #[test]
    fn status_line_hides_reopen_counters_when_zero() {
        let stats = EngineStats::default();
        let line = format_status_line(&stats, 7, 0.1, 0.2, 3, 0.5, &EngineHealth::Ok, None);
        assert!(line.contains("gen 7"));
        assert!(
            !line.contains("reopens"),
            "no reopen suffix when both zero: {line}"
        );
        assert!(!line.contains("reconnecting"));
    }

    #[test]
    fn status_line_shows_reopen_counters_and_reconnecting() {
        let stats = EngineStats {
            reopens: 2,
            reopen_failures: 5,
            ..EngineStats::default()
        };
        let line = format_status_line(
            &stats,
            9,
            0.0,
            0.0,
            0,
            0.0,
            &EngineHealth::Reconnecting {
                since_ms: 800,
                attempts: 5,
            },
            Some("no capture device available"),
        );
        assert!(line.contains("reopens 2 fail 5"), "line: {line}");
        assert!(
            line.contains("reconnecting… 800ms attempt 5"),
            "line: {line}"
        );
        assert!(
            line.contains("last-err \"no capture device available\""),
            "line: {line}"
        );
    }

    #[test]
    fn status_line_shows_failed() {
        let stats = EngineStats {
            reopens: 0,
            reopen_failures: 40,
            ..EngineStats::default()
        };
        let line = format_status_line(
            &stats,
            1,
            0.0,
            0.0,
            0,
            0.0,
            &EngineHealth::Failed {
                error: "device gone".to_string(),
            },
            Some("device gone"),
        );
        assert!(line.contains("reopens 0 fail 40"), "line: {line}");
        assert!(line.contains("FAILED: device gone"), "line: {line}");
    }
}
