//! Terminal frontend for scia: it owns the terminal, drives the render loop at
//! the target frame rate, and paints spectrum bars from the core's live
//! feature bus. Rendering ([`draw`]) is a pure function of a
//! [`FeatureSnapshot`] and a [`UiState`], so it is tested headless with
//! ratatui's `TestBackend`; everything stateful — the terminal, timing, input,
//! idle throttling — lives in [`run`].
//!
//! The loop paces itself to `TuiOptions::fps`, brackets each frame in a
//! synchronized-update sequence so a terminal that supports it shows no tear,
//! and downshifts to a low idle rate when the feed has been starved for a
//! while. It never queues frames: an overrunning frame simply skips ahead to
//! the next deadline.
//!
//! [`FeatureSnapshot`]: scia_core::FeatureSnapshot

#![forbid(unsafe_code)]

mod author;
mod chrome;
mod devicepick;
mod keymap;
mod kitty;
mod mapping_ui;
mod mosaic;
mod nowplaying;
mod pacing;
mod palette;
mod pixel;
mod presenter;
mod probe;
mod render;
mod sixel;
mod stats;
mod tuning;

use std::fmt;
use std::io::{self, Stdout, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use scia_core::engine::EngineHealth;
use scia_core::{
    Activity, DeviceInfo, DeviceSelector, EngineStats, FeatureReader, FeatureSnapshot, list_devices,
};
use scia_scenes::{
    Palette, Preset, ReloadEvent, SceneSource, builtin_presets, catalog_scenes,
    expression_vocabulary, scene_preset,
};

pub use author::{AuthorMode, ReloadStatus, SourceError, did_you_mean, draw_author};
pub use chrome::{ChromeMode, ChromeState, Fade};
pub use devicepick::{
    CaptureFilter, DevicePicker, DeviceRow, EnumState, Platform, apply_device_pin, build_rows,
    capture_filter, draw_devices, pin_device, platform_filter,
};
pub use keymap::{ChordParseError, InputAction, KeyChord, Keymap, parse_chord};
pub use kitty::{CLEANUP as KITTY_CLEANUP, KittyEncoder};
pub use mapping_ui::{MappingUi, SourceSignal, draw_mapping, table_display};
pub use mosaic::{Cell, CellGrid, FrameBuffer, TextRun, Tier};
pub use nowplaying::{
    ArtResult, DecodeJob, MetaRuntime, NowPlayingState, TrackArt, art_palette_to_scene,
    draw_now_playing, extrapolated_position,
};
pub use pixel::{FALLBACK_CELL_PX, PIXEL_BUDGET, PixelBuffer, image_dims};
pub use presenter::{
    PresenterMode, SceneError, ScenePresenter, build_scene_presenter, build_scene_presenter_mode,
};
pub use probe::{
    CapabilityReport, Da1, SyncSupport, TermFamily, classify_family, default_tier, parse_cell_size,
    parse_da1, parse_decrqm_2026, probe, truecolor_from,
};
pub use render::{SceneNav, UiState, VERSION, draw, draw_help, draw_notice};
pub use sixel::{SIXEL_REGISTERS, SixelEncoder, quantize as sixel_quantize};
pub use tuning::{
    TuningParam, TuningStrip, apply_map_edit, apply_params_edit, draw_tuning, write_back_export,
    write_back_file, write_map_export, write_map_file,
};

/// The crate name, resolved at compile time from Cargo metadata.
pub const NAME: &str = env!("CARGO_PKG_NAME");

/// Options for [`run`].
#[derive(Clone, Debug)]
pub struct TuiOptions {
    /// Target frame rate. `60` is the default cadence.
    pub fps: u32,
    /// Header label, e.g. `"DEMO — synthetic feed"`, shown highlighted so demo
    /// mode is never mistaken for live capture. `None` for live capture.
    pub label: Option<String>,
    /// Live-capture source description shown in the header centre when [`label`]
    /// is `None`, e.g. `"48000 Hz 2 ch"`. Ignored in demo mode.
    ///
    /// [`label`]: TuiOptions::label
    pub source: String,
    /// Exit after this many rendered frames. `None` runs until the user quits;
    /// `Some(n)` is used by cold-start timing and smoke tests.
    pub frames: Option<u64>,
    /// Start with the debug line visible.
    pub debug: bool,
    /// Start with the debug/performance overlay panel visible.
    pub overlay: bool,
    /// Built-in scene preset to render, by name. `None` runs the direct
    /// spectrum-bar renderer (the byte-identical legacy path); `Some(name)`
    /// drives the [`ScenePresenter`] on the selected [`tier`](Self::tier).
    /// Ignored when [`preset`](Self::preset) is set.
    pub scene: Option<String>,
    /// A preset already loaded from disk (the `--scene-file` path). When
    /// `Some`, [`run`] drives the [`ScenePresenter`] from it directly, ahead of
    /// [`scene`](Self::scene); live reloads then arrive on `run`'s `reload`
    /// receiver.
    pub preset: Option<Preset>,
    /// How to render the scene body: the cell mosaic on a [`Tier`], or the kitty
    /// graphics pixel presenter. Ignored when neither [`scene`](Self::scene) nor
    /// [`preset`](Self::preset) is set. Defaults to mosaic at [`Tier::default`].
    pub presenter_mode: PresenterMode,
    /// A one-shot status notice shown briefly at startup — e.g. a fallback note
    /// when a forced kitty presenter is unsupported. `None` for no notice.
    pub initial_notice: Option<String>,
    /// The active key bindings, built at startup from the built-in defaults plus
    /// any config overrides. The default is the built-in binding set.
    pub keymap: Keymap,
    /// The chrome personality to start in. Defaults to
    /// [`ChromeMode::Invisible`].
    pub chrome: ChromeMode,
    /// The `--scene-file` path, when a preset was loaded from disk. The tuning
    /// strip writes its adjustments back to this file, comment-preserving, when
    /// set; otherwise it exports the running builtin preset under
    /// [`config_dir`](Self::config_dir).
    pub scene_file: Option<PathBuf>,
    /// The base config directory (the one `config.toml` lives in), used as the
    /// root of the tuning strip's builtin-export path
    /// (`<config_dir>/presets/<name>.toml`). `None` disables export write-back.
    /// The device picker also pins into `<config_dir>/config.toml`.
    pub config_dir: Option<PathBuf>,
    /// The capture device the session started on, so the device picker can mark
    /// the active endpoint. Defaults to the platform default.
    pub device: DeviceSelector,
    /// Whether the capture backend prefers the PipeWire host, so the device
    /// picker filters to the matching capture direction. Defaults to `true`.
    pub prefer_pipewire: bool,
}

impl Default for TuiOptions {
    fn default() -> Self {
        Self {
            fps: 60,
            label: None,
            source: String::new(),
            frames: None,
            debug: false,
            overlay: false,
            scene: None,
            preset: None,
            presenter_mode: PresenterMode::Mosaic(Tier::default()),
            initial_notice: None,
            keymap: Keymap::default(),
            chrome: ChromeMode::Invisible,
            scene_file: None,
            config_dir: None,
            device: DeviceSelector::Default,
            prefer_pipewire: true,
        }
    }
}

/// What [`run`] reports back after the loop ends.
#[derive(Clone, Debug, PartialEq)]
pub struct RunSummary {
    /// Total frames rendered.
    pub frames: u64,
    /// Median frame render time in milliseconds.
    pub p50_frame_ms: f32,
    /// 99th-percentile frame render time in milliseconds.
    pub p99_frame_ms: f32,
    /// `Some(message)` when the loop aborted because the capture stream reported
    /// an error; `None` on a clean quit or frame-limit exit. The caller reports
    /// the message and exits non-zero.
    pub error: Option<String>,
}

/// Run the terminal frontend until the user quits (or `opts.frames` frames have
/// been rendered).
///
/// The terminal is put into raw mode on the alternate screen with the cursor
/// hidden, and is fully restored on every exit path — normal return, error, and
/// panic (a panic hook restores the terminal, then re-raises).
///
/// `reader` is polled once per frame for the freshest snapshot; `stats` is
/// called once per frame for the engine counters shown on the debug line;
/// `health` is polled once per frame — while it reports
/// [`EngineHealth::Reconnecting`] the loop shows a calm "reconnecting audio…"
/// notice and keeps rendering (scenes animate on the silence the engine keeps
/// publishing), clears the notice with "capture restored" on recovery, and only
/// leaves the terminal (returning a [`RunSummary`] whose
/// [`error`](RunSummary::error) carries the message) on
/// [`EngineHealth::Failed`]; and `clock` is the engine's snapshot clock
/// (monotonic ns since the ring epoch), sampled once per frame so the overlay
/// can show the newest feature's age as `clock() - snap.timestamp_ns`.
///
/// When `opts.scene` is `Some(name)`, the scene presenter is built from the
/// built-in preset *before* the terminal is touched, so an unknown or invalid
/// preset returns [`RunError::Scene`] cleanly with the terminal untouched. The
/// caller turns that into a usage error (exit 2) listing the available names.
///
/// # Errors
/// [`RunError::Scene`] for a missing/invalid `--scene` preset, or
/// [`RunError::Io`] for any I/O error from terminal setup, drawing, or input
/// polling. The terminal is restored before an I/O error is returned.
pub fn run(
    reader: FeatureReader,
    stats: impl FnMut() -> EngineStats,
    health: impl FnMut() -> EngineHealth,
    clock: impl FnMut() -> u64,
    switch_device: impl FnMut(DeviceSelector),
    reload: Option<Receiver<ReloadEvent>>,
    opts: TuiOptions,
) -> Result<RunSummary, RunError> {
    install_panic_hook();
    // Build the scene presenter first: a bad preset must fail with the terminal
    // still in its normal state. A disk preset (`--scene-file`) is already
    // validated by the caller and takes precedence over a built-in name.
    let presenter = match (&opts.preset, &opts.scene) {
        (Some(preset), _) => Some(ScenePresenter::with_mode(preset, opts.presenter_mode)),
        (None, Some(name)) => Some(build_scene_presenter_mode(name, opts.presenter_mode)?),
        (None, None) => None,
    };
    let mut guard = TerminalGuard::enter()?;
    // The guard restores the terminal on every exit path, including `?`.
    Ok(run_loop(
        &mut guard.terminal,
        reader,
        stats,
        health,
        clock,
        switch_device,
        reload,
        &opts,
        presenter,
    )?)
}

/// How [`run`] can fail: a bad `--scene` preset, or an I/O error.
#[derive(Debug)]
pub enum RunError {
    /// The requested scene preset was missing or invalid. The [`Display`] is a
    /// user-facing message; the caller reports it as a usage error (exit 2).
    ///
    /// [`Display`]: std::fmt::Display
    Scene(SceneError),
    /// An I/O error from terminal setup, drawing, or input polling.
    Io(io::Error),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Scene(e) => write!(f, "{e}"),
            RunError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RunError {}

impl From<SceneError> for RunError {
    fn from(e: SceneError) -> Self {
        RunError::Scene(e)
    }
}

impl From<io::Error> for RunError {
    fn from(e: io::Error) -> Self {
        RunError::Io(e)
    }
}

/// Owns the terminal for the lifetime of a run and restores it on drop, so
/// every early return (including `?`) leaves the terminal clean.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore_terminal();
    }
}

/// Restore the terminal to its normal state. Stateless so it can also run from
/// the panic hook, and idempotent enough that running it twice is harmless.
fn restore_terminal() -> io::Result<()> {
    let mut stdout = io::stdout();
    // Delete any placed kitty graphics before leaving the alternate screen. A
    // no-op on terminals that never displayed one (and ignored by terminals that
    // do not speak the protocol), so it is safe on every exit path, panic hook
    // included.
    let _ = stdout.write_all(kitty::CLEANUP);
    execute!(stdout, LeaveAlternateScreen, Show)?;
    disable_raw_mode()?;
    Ok(())
}

/// Install a panic hook that restores the terminal before the default hook
/// prints the panic, so a crash never leaves a scrambled terminal behind.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        previous(info);
    }));
}

/// The render loop: pace, read, draw, handle input, repeat.
///
/// The parameter list mirrors the seams `run` exposes (engine closures, the
/// reload channel, options, presenter) — grouping them into a struct would
/// only relocate the same eight names, so the lint is waived here.
#[allow(clippy::too_many_arguments)]
fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut reader: FeatureReader,
    mut stats: impl FnMut() -> EngineStats,
    mut health: impl FnMut() -> EngineHealth,
    mut clock: impl FnMut() -> u64,
    mut switch_device: impl FnMut(DeviceSelector),
    reload: Option<Receiver<ReloadEvent>>,
    opts: &TuiOptions,
    mut presenter: Option<ScenePresenter>,
) -> io::Result<RunSummary> {
    let mut frame_times = stats::FrameTimes::new();
    // The browser and live cycling navigate the built-in scenes, so they are
    // enabled only on the built-in `--scene` path: a presenter is running and it
    // was built from a registry name, not a disk preset (whose live-authored
    // file must not be swapped out for a built-in). Cycling then starts from the
    // `--scene` id.
    let scene_mode = presenter.is_some() && opts.preset.is_none() && opts.scene.is_some();
    let initial_scene = opts
        .scene
        .as_deref()
        .and_then(|name| catalog_scenes().iter().position(|s| s.id == name))
        .unwrap_or(0);
    let mut ui = UiState {
        label: opts.label.clone(),
        source: opts.source.clone(),
        debug: opts.debug,
        overlay: opts.overlay,
        fps_measured: opts.fps as f32,
        // A scene presenter surfaces its mode (ladder rung, or "kitty") on the
        // debug line; the direct-bars renderer leaves it unset.
        tier: presenter.as_ref().map(|p| p.mode_label()),
        notice: opts.initial_notice.clone(),
        scene_mode,
        scene_nav: SceneNav::new(initial_scene),
        keymap: opts.keymap,
        chrome: ChromeState::new(opts.chrome),
        devices: DevicePicker::new(opts.device.clone(), opts.prefer_pipewire),
        ..UiState::default()
    };
    // The in-flight device-enumeration worker's result channel, alive only while
    // the picker is open and enumerating. Enumeration blocks (device probing), so
    // it runs off the UI thread and its result is folded in on a later frame.
    let mut device_rx: Option<std::sync::mpsc::Receiver<Result<Vec<DeviceInfo>, String>>> = None;
    // Tracks the now-playing line so a track change can reset the invisible-mode
    // fade. `track_line` is `None` until the metadata seam is wired, so this
    // stays inert today but is ready for it.
    let mut last_track: Option<String> = ui.track_line().map(str::to_owned);
    // Frame period fed to the scene presenter; seeded to the target period.
    let default_dt = 1.0 / opts.fps.max(1) as f32;
    // Holds the frozen snapshot while paused, so a paused scene renders an
    // identical frame every tick.
    let mut pause_state = PauseState::default();

    // The now-playing runtime: the platform backend (MPRIS/SMTC) plus a decode
    // worker, wired by channels. Absence is normal — no backend or no media
    // session leaves the state empty and the panel quiet. Dropped at loop exit,
    // which stops the backend and joins the worker.
    let meta = MetaRuntime::spawn();
    // The scene's own palette, remembered when an art palette is applied so the
    // palette key can crossfade back to it.
    let mut scene_base_palette: Option<Palette> = None;

    let mut frames: u64 = 0;
    // When the current continuous starvation began, if any.
    let mut starved_since: Option<Instant> = None;
    // Start of the previous frame, for the measured-fps EMA.
    let mut prev_frame_start: Option<Instant> = None;
    let mut fps_ema = opts.fps as f32;
    // The reload status notice auto-clears this long after the last event.
    const NOTICE_TTL: Duration = Duration::from_secs(3);
    // Deadline at which the current notice is cleared, tracked in-loop (no
    // background timer). A startup notice (e.g. a kitty fallback) starts its TTL
    // now.
    let mut notice_deadline: Option<Instant> = None;
    if ui.notice.is_some() {
        notice_deadline = Some(Instant::now() + NOTICE_TTL);
    }
    // Whether the loop is currently showing the "reconnecting audio…" degraded
    // notice, so recovery can clear it once with "capture restored".
    let mut reconnecting = false;

    // Kitty graphics state: the frame encoder, its reusable output buffer, and
    // the scene-body rect captured inside the draw closure so the image can be
    // placed at its origin. Inert unless a kitty presenter is active.
    let kitty_mode = matches!(opts.presenter_mode, PresenterMode::Kitty { .. });
    let mut kitty_encoder = KittyEncoder::new();
    let mut kitty_out: Vec<u8> = Vec::new();
    // Sixel graphics state: the frame encoder and its reusable output buffer.
    // Inert unless a sixel presenter is active.
    let sixel_mode = matches!(opts.presenter_mode, PresenterMode::Sixel { .. });
    let mut sixel_encoder = SixelEncoder::new();
    let mut sixel_out: Vec<u8> = Vec::new();

    loop {
        let frame_start = Instant::now();
        let mut dt = default_dt;
        if let Some(prev) = prev_frame_start {
            let period = frame_start.duration_since(prev).as_secs_f32();
            if period > 0.0 {
                dt = period;
                // Light EMA so the reading is stable but still tracks changes.
                fps_ema = fps_ema * 0.8 + (1.0 / period) * 0.2;
            }
        }
        prev_frame_start = Some(frame_start);

        // While paused the scene freezes on the snapshot captured at the moment
        // of pause and is advanced with `dt = 0`, so it renders an identical
        // frame every tick; capture keeps running underneath. `scene_dt` drives
        // the presenter and scene-nav timers, `snap` feeds the whole frame.
        let (snap, scene_dt) = pause_state.resolve(ui.paused, *reader.latest(), dt);

        // Apply a scene switch the browser/cycle keys requested on the previous
        // frame, then age the cycle toast on the frame clock. The switch reuses
        // the presenter's crossfade path (the same one hot reload uses); a rapid
        // sequence of moves collapses to the latest target, retargeting one fade
        // rather than blanking between them.
        if let Some(id) = ui.scene_nav.take_pending() {
            if let Some(p) = presenter.as_mut() {
                if let Some(Ok(preset)) = scene_preset(id) {
                    p.swap_preset(&preset);
                    // The new scene carries its own palette; a swap cancels the
                    // applied art palette so a later palette key re-captures the
                    // new base.
                    ui.palette_applied = false;
                    scene_base_palette = None;
                }
            }
        }
        ui.scene_nav.tick(scene_dt);

        // Drain now-playing backend events into the state, dispatching artwork to
        // the decode worker, then fold in any finished decodes. Both are
        // non-blocking, so a quiet or absent backend costs a couple of empty
        // `try_recv`s per frame.
        while let Some(ev) = meta.try_event() {
            if let Some(job) = ui.now_playing.apply_event(ev) {
                meta.submit(job);
            }
        }
        while let Some(res) = meta.try_result() {
            ui.now_playing
                .apply_art(res.track_key, res.preview, res.palette);
        }

        // Resolve a palette-key request here, where the presenter lives: apply the
        // current track's palette via crossfade, revert to the scene's own, or —
        // with no scene or no art — note it rather than erroring.
        if std::mem::take(&mut ui.palette_pending) {
            apply_palette_toggle(presenter.as_mut(), &mut ui, &mut scene_base_palette);
            notice_deadline = Some(frame_start + NOTICE_TTL);
        }

        // Fulfil a tuning-strip open request: seed the strip from the presenter's
        // first-layer manifest, current values and mappings. With no presenter
        // (direct-bars) or no tunable parameters it stays shut.
        if std::mem::take(&mut ui.tuning_open_pending) {
            if let Some(p) = presenter.as_ref() {
                let params = build_tuning_params(p);
                ui.tuning.open(params);
            }
        }

        // Fulfil a tuning-strip write request: write the adjusted values back to
        // the `--scene-file` (comments intact) or export the running builtin
        // preset under the config dir, leaving a status notice either way.
        if std::mem::take(&mut ui.tuning_write_pending) {
            ui.notice = Some(write_tuning(
                presenter.as_ref(),
                &ui.tuning,
                &ui.scene_nav,
                opts,
            ));
            notice_deadline = Some(frame_start + NOTICE_TTL);
        }

        // Fulfil an expression-mapping open request: seed the overlay from the
        // presenter's first-layer `[map]` rows. With no presenter (direct-bars)
        // or no mappings it stays shut.
        if std::mem::take(&mut ui.mapping_open_pending) {
            if let Some(p) = presenter.as_ref() {
                ui.mapping.open(p.layer0_mapping_entries());
            }
        }

        // Fulfil an expression-mapping write request: write the edited rows back
        // as expression strings to the `--scene-file` (comments intact) or the
        // builtin export under the config dir, leaving a status notice either way.
        if std::mem::take(&mut ui.mapping_write_pending) {
            ui.notice = Some(write_mapping(
                presenter.as_ref(),
                &ui.mapping,
                &ui.scene_nav,
                opts,
            ));
            notice_deadline = Some(frame_start + NOTICE_TTL);
        }
        // Fulfil a scene-author open request: build the source descriptor from
        // the `--scene-file` path or the running builtin preset, plus the
        // did-you-mean vocabulary (the signal names and the scene's params), and
        // open the mode on it. With no presenter (direct-bars) it stays shut.
        if std::mem::take(&mut ui.author_open_pending) {
            if let Some(p) = presenter.as_ref() {
                if let Some(source) = author_source(opts, &ui.scene_nav, p) {
                    ui.author.open(source, author_vocab(p));
                }
            }
        }

        // Open the device picker: spawn the (blocking) enumeration on a worker
        // thread and show the `enumerating…` placeholder until its result lands.
        // A re-open always re-enumerates.
        if std::mem::take(&mut ui.device_open_pending) {
            ui.devices.open_enumerating();
            let (tx, rx) = std::sync::mpsc::channel();
            device_rx = Some(rx);
            // The worker owns no UI state; it just enumerates and reports back.
            // If the receiver is gone (picker re-opened), the send is dropped.
            let _ = std::thread::Builder::new()
                .name("scia-devlist".into())
                .spawn(move || {
                    let result = list_devices().map_err(|e| e.to_string());
                    let _ = tx.send(result);
                });
        }
        // Fold in a finished enumeration, building the capture-target rows.
        if let Some(rx) = device_rx.as_ref() {
            if let Ok(result) = rx.try_recv() {
                ui.devices.set_devices(result);
                device_rx = None;
            }
        }
        // Switch capture to the selected device: call the engine seam, mark it
        // active, close the picker, and leave a notice. The route watcher does
        // the actual reopen, so the UI thread never blocks on it.
        if std::mem::take(&mut ui.device_switch_pending) {
            if let Some(row) = ui.devices.selected_row() {
                let selector = row.selector.clone();
                let label = row.name.clone();
                switch_device(selector.clone());
                ui.devices.set_active(selector);
                ui.devices.close();
                ui.notice = Some(format!("capture → {label}"));
                notice_deadline = Some(frame_start + NOTICE_TTL);
            }
        }
        // Pin the selected device into the config file (comment-preserving), or
        // note why it could not be pinned.
        if std::mem::take(&mut ui.device_pin_pending) {
            ui.notice = Some(pin_device_selection(&ui.devices, opts));
            notice_deadline = Some(frame_start + NOTICE_TTL);
        }

        // Seam: keep the chrome's track line current from the metadata state.
        // Only a session that is actually *Playing* reaches the ambient chrome
        // and the scene text; a paused (or stopped) session is definitionally
        // not the audio source, so it produces nothing here even though the
        // selection policy still keeps it (the explicit `n` panel shows it).
        ui.track = playing_track_line(ui.now_playing.current.as_ref());

        // Advance the chrome timers on the real frame period: the invisible-mode
        // fade tracks time-since-input, not scene motion, so it keeps counting
        // while the scene is paused. A track-line change resets the fade the same
        // way a keypress does.
        let cur_track = ui.track_line().map(str::to_owned);
        if cur_track != last_track {
            ui.chrome.on_track_change();
            // Push the new track line to the scenes so a text scene (verso) can
            // rebuild its letters; an empty value means "no track".
            if let Some(p) = presenter.as_mut() {
                p.set_text("track", cur_track.as_deref().unwrap_or(""));
            }
            last_track = cur_track;
        }
        ui.chrome.tick(dt);

        // Refresh the engine counters first: activity feeds the idle downshift.
        ui.stats = stats();
        ui.fps_measured = fps_ema;
        // Feature age against the engine's snapshot clock, for the overlay.
        ui.feature_age_ms = render::feature_age_ms(clock(), snap.timestamp_ns);

        // Apply at most one live-reload event per frame. Audio capture is never
        // touched here — only the presenter's scene layers. When no presenter is
        // active (direct-bars path), events are simply drained and ignored.
        if let Some(rx) = reload.as_ref() {
            if let Ok(event) = rx.try_recv() {
                if let Some(p) = presenter.as_mut() {
                    match &event.result {
                        Ok(preset) => {
                            p.swap_preset(preset);
                            // Rebuild the mapping overlay's rows against the new
                            // preset when it is open, so its list never lags the
                            // running scene.
                            if ui.mapping.is_open() {
                                ui.mapping.on_preset_swap(p.layer0_mapping_entries());
                            }
                            ui.notice = Some(format!("reloaded {:.0}ms", event.elapsed_ms));
                        }
                        // A broken edit keeps the running scene; the error's
                        // first line surfaces on the status row.
                        Err(err) => {
                            let msg = err.to_string();
                            let first = msg.lines().next().unwrap_or_default();
                            ui.notice = Some(first.to_string());
                        }
                    }
                    notice_deadline = Some(frame_start + NOTICE_TTL);
                }
                // Scene-author mode surfaces the reload: it re-reads the source so
                // the pane shows the just-saved bytes, records the reload time, and
                // on a failed validate highlights the failing line with the inline
                // message. It never drives the pipeline — the last good scene holds
                // above regardless. A no-op while author mode is closed.
                ui.author.on_reload(&event);
            }
        }
        // Auto-clear the notice once its deadline passes.
        if let Some(deadline) = notice_deadline {
            if frame_start >= deadline {
                ui.notice = None;
                notice_deadline = None;
            }
        }

        // Fold in capture health. A transient fault (a device switch the route
        // watcher is recovering from) shows a calm degraded notice and the loop
        // keeps rendering — scenes animate on the silence the engine keeps
        // publishing. The loop leaves the terminal only once reopen has failed
        // past the deadline (`EngineHealth::Failed`); the guard restores the
        // terminal on return and the caller reports the message.
        match health_transition(health(), &mut reconnecting) {
            HealthReaction::Steady => {}
            HealthReaction::Reconnecting => {
                // A sticky notice: no TTL, so it stays up for the whole episode
                // and is replaced on recovery or failure.
                ui.notice = Some(RECONNECT_NOTICE.to_string());
                notice_deadline = None;
            }
            HealthReaction::Restored => {
                ui.notice = Some(RESTORED_NOTICE.to_string());
                notice_deadline = Some(frame_start + NOTICE_TTL);
            }
            HealthReaction::Failed(msg) => {
                let (p50, p99) = frame_times.percentiles();
                return Ok(RunSummary {
                    frames,
                    p50_frame_ms: p50,
                    p99_frame_ms: p99,
                    error: Some(msg),
                });
            }
        }

        // Track continuous starvation for the idle downshift.
        if snap.starved {
            starved_since.get_or_insert(frame_start);
        } else {
            starved_since = None;
        }
        let starved_for = starved_since
            .map(|since| frame_start.duration_since(since))
            .unwrap_or_default();
        // Downshift to the idle rate once the engine reports `Idle`, or after the
        // starved-for-2-s rule trips — whichever comes first.
        let interval = if ui.stats.activity == Activity::Idle {
            pacing::active_interval(pacing::IDLE_FPS)
        } else {
            pacing::target_interval(opts.fps, starved_for)
        };
        let (p50, p99) = frame_times.percentiles();
        ui.p50_frame_ms = p50;
        ui.p99_frame_ms = p99;

        // Draw inside a synchronized-update bracket. Terminals without support
        // ignore the sequences.
        let render_start = Instant::now();
        {
            let mut out = io::stdout();
            let _ = execute!(out, BeginSynchronizedUpdate);
        }
        match presenter.as_mut() {
            // Scene path: draw the header/debug chrome, then rasterize the scene
            // into the body area the chrome left free.
            Some(p) => {
                // The scene-body rect, captured inside the draw closure so a
                // graphics image can be placed at its origin after the draw.
                let mut image_body: Option<Rect> = None;
                terminal.draw(|frame| {
                    if let Some(body) = render::draw_chrome(frame, &snap, &ui) {
                        p.resize(body.width, body.height);
                        // Push the tuning strip's working values into the layer-0
                        // bag before the frame advances, so an unmapped
                        // adjustment takes effect on this very frame.
                        if ui.tuning.is_open() {
                            for tp in ui.tuning.params() {
                                p.set_param(tp.key, tp.value);
                            }
                        }
                        // Feed the mapping overlay's sparklines from this frame's
                        // features and swap in any edited mapping before the frame
                        // advances, so a valid draft previews on this very frame.
                        if ui.mapping.is_open() {
                            ui.mapping.sample(&snap);
                            if let Some(entry) = ui.mapping.drain_apply() {
                                p.replace_layer0_mapping(entry);
                            }
                        }
                        p.frame(&snap, scene_dt);
                        // In a pixel mode `draw` paints only the text runs; the
                        // image is written as a graphics-protocol frame, placed at
                        // the body origin captured here.
                        image_body = Some(body);
                        p.draw(frame.buffer_mut(), body);
                        // The chrome personality paints over the scene, before
                        // the debug and help overlays layered above it.
                        chrome::render(frame.buffer_mut(), body, &snap, &ui);
                        // The overlay is drawn last, over the rasterized scene.
                        if ui.overlay {
                            render::render_overlay(frame.buffer_mut(), body, &snap, &ui);
                        }
                        // The browser panel and cycle toast paint over the live
                        // scene, like the meter bridge, so they draw after it.
                        render::draw_scene_nav(frame.buffer_mut(), body, &ui.scene_nav);
                        // The now-playing panel paints over the scene, like the
                        // meter bridge.
                        if ui.show_now_playing {
                            nowplaying::draw_now_playing(
                                frame.buffer_mut(),
                                body,
                                &ui.now_playing,
                                ui.palette_applied,
                            );
                        }
                        // The tuning strip paints over the body bottom, above the
                        // scene and now-playing panel; help still layers on top.
                        if ui.tuning.is_open() {
                            tuning::draw_tuning(frame.buffer_mut(), body, &ui.tuning);
                        }
                        // The expression-mapping overlay paints over the body
                        // bottom, the sibling of the tuning strip; help layers on
                        // top of it too.
                        if ui.mapping.is_open() {
                            mapping_ui::draw_mapping(frame.buffer_mut(), body, &ui.mapping);
                        }
                        // Scene-author mode: the source pane plus the reused meter
                        // bridge, split over the body. Drawn above the scene, below
                        // the help overlay.
                        if ui.author.is_open() {
                            author::draw_author(frame.buffer_mut(), body, &ui.author, &snap, &ui);
                        }
                        // The help overlay is the topmost body layer.
                        render::draw_help(frame.buffer_mut(), body, &ui);
                        // The device picker is a modal overlay drawn above the
                        // rest, like the help panel.
                        devicepick::draw_devices(frame.buffer_mut(), body, &ui.devices);
                    }
                    // The reload notice lands on top of the scene body, so it
                    // draws after the presenter rather than inside the chrome.
                    let area = frame.area();
                    render::draw_notice(frame.buffer_mut(), area, &ui);
                })?;
                // Kitty mode: after ratatui has painted the cells (text + chrome
                // above the image), write the image as a graphics-protocol frame
                // at the body origin — still inside the synchronized-update
                // bracket so a supporting terminal shows image and text together.
                if kitty_mode {
                    if let Some(body) = image_body {
                        let (iw, ih) = p.image_px();
                        if iw > 0 && ih > 0 {
                            kitty_encoder.encode(
                                p.image_rgb8(),
                                p.image_px(),
                                p.image_cells(),
                                &mut kitty_out,
                            );
                            let mut out = io::stdout();
                            let _ = execute!(out, MoveTo(body.x, body.y));
                            let _ = out.write_all(&kitty_out);
                            let _ = out.flush();
                        }
                    }
                }
                // Sixel mode: same seam as kitty, but the sixel bitmap paints
                // over the body cells (there is no z-index). The cursor is moved
                // to the body origin and the DCS stream is written there, still
                // inside the synchronized-update bracket. A full-body overlay that
                // ratatui drew this frame is covered until the next frame redraws
                // its cells on top — at 60 fps that is a single frame.
                if sixel_mode {
                    if let Some(body) = image_body {
                        let (iw, ih) = p.image_px();
                        if iw > 0 && ih > 0 {
                            sixel_encoder.encode(
                                p.image_rgb8(),
                                p.image_px(),
                                p.image_k(),
                                &mut sixel_out,
                            );
                            let mut out = io::stdout();
                            let _ = execute!(out, MoveTo(body.x, body.y));
                            let _ = out.write_all(&sixel_out);
                            let _ = out.flush();
                        }
                    }
                }
            }
            // Direct-bars path: byte-identical to before, plus the notice.
            None => {
                terminal.draw(|frame| draw(frame, &snap, &ui))?;
            }
        }
        {
            let mut out = io::stdout();
            let _ = execute!(out, EndSynchronizedUpdate);
        }
        frame_times.push(render_start.elapsed().as_secs_f32() * 1000.0);
        frames += 1;

        if let Some(limit) = opts.frames {
            if frames >= limit {
                break;
            }
        }

        // Input doubles as the frame sleep: poll until this frame's deadline so
        // keys are handled immediately. When a draw overruns the interval the
        // deadline has already passed, so `pump_input` still runs a bounded
        // zero-timeout drain of the buffered keys (see its docs) rather than
        // skipping input entirely and stranding every keystroke.
        let deadline = frame_start + interval;
        let mut poll = event::poll;
        let mut read = event::read;
        match pump_input(deadline, &mut poll, &mut read, &mut ui)? {
            PumpOutcome::Quit => {
                let (p50, p99) = frame_times.percentiles();
                return Ok(RunSummary {
                    frames,
                    p50_frame_ms: p50,
                    p99_frame_ms: p99,
                    error: None,
                });
            }
            // A resize or state toggle redraws promptly on the next frame; a
            // clean deadline exit simply advances the loop. Either way the loop
            // proceeds to the next frame — the drain, if any, already ran.
            PumpOutcome::Redraw | PumpOutcome::Deadline => {}
        }
    }

    let (p50, p99) = frame_times.percentiles();
    Ok(RunSummary {
        frames,
        p50_frame_ms: p50,
        p99_frame_ms: p99,
        error: None,
    })
}

/// The degraded-state notice shown while capture is reconnecting after a device
/// switch or fault.
const RECONNECT_NOTICE: &str = "reconnecting audio…";
/// The one-shot notice shown when capture recovers from a reconnect.
const RESTORED_NOTICE: &str = "capture restored";

/// How the frame loop should react to an [`EngineHealth`] reading, decided by
/// [`health_transition`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum HealthReaction {
    /// Capture is healthy and was already; leave the notice untouched.
    Steady,
    /// Capture is reconnecting; show the sticky degraded notice.
    Reconnecting,
    /// Capture just recovered; show the one-shot "restored" notice.
    Restored,
    /// Capture failed past the deadline; leave the loop with this error.
    Failed(String),
}

/// Decide the loop's reaction to `health`, threading the "currently showing the
/// reconnecting notice" flag so recovery is detected exactly once. Pure, so the
/// loop's degraded-state behavior is unit-tested without a terminal:
/// [`EngineHealth::Reconnecting`] never exits (it returns [`HealthReaction::Reconnecting`]),
/// and only [`EngineHealth::Failed`] yields [`HealthReaction::Failed`].
fn health_transition(health: EngineHealth, reconnecting: &mut bool) -> HealthReaction {
    match health {
        EngineHealth::Ok => {
            if *reconnecting {
                *reconnecting = false;
                HealthReaction::Restored
            } else {
                HealthReaction::Steady
            }
        }
        EngineHealth::Reconnecting { .. } => {
            *reconnecting = true;
            HealthReaction::Reconnecting
        }
        EngineHealth::Failed { error } => {
            *reconnecting = false;
            HealthReaction::Failed(error)
        }
    }
}

/// What an input event asks the loop to do.
enum Action {
    /// Nothing of interest.
    None,
    /// Redraw as soon as possible (resize, or a state toggle).
    Redraw,
    /// Quit the loop.
    Quit,
}

/// The most input events [`pump_input`] will drain in a single overrunning
/// frame. The overrun drain polls with a zero timeout, so without a bound a
/// flood of buffered keystrokes (a stuck key, a paste) could keep it spinning
/// and starve the very redraw that is falling behind. Capping it processes the
/// backlog steadily — this many per frame — while guaranteeing the phase always
/// returns; anything past the cap waits in the tty buffer for the next frame.
const MAX_OVERRUN_EVENTS: usize = 32;

/// How one frame's input phase ([`pump_input`]) ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PumpOutcome {
    /// A quit was requested; the loop should return immediately.
    Quit,
    /// A redraw was requested (a resize or a state toggle); proceed to the next
    /// frame without finishing out the sleep.
    Redraw,
    /// The phase ran to the deadline with nothing forcing an early redraw.
    Deadline,
}

/// Run one frame's input phase: poll for events until `deadline`, handling each
/// as it arrives, and — crucially — guarantee that pending input is drained even
/// when the frame has already overrun its budget.
///
/// The input phase doubles as the frame's sleep. On a healthy frame `deadline`
/// is still in the future, so this polls with the remaining budget and handles
/// events as they arrive, breaking at the deadline — behaviour identical to the
/// pre-fix loop. But when a draw overruns the frame interval the deadline has
/// already passed on entry, leaving zero remaining budget every frame; the old
/// code then broke out *before a single poll*, so input was never read and every
/// keystroke piled up unhandled in the tty buffer. To prevent that, once the
/// deadline has passed (on entry, or once it passes mid-phase) this switches to
/// a bounded zero-timeout drain: up to [`MAX_OVERRUN_EVENTS`] iterations of
/// `poll(Duration::ZERO)` → [`handle_event`], so buffered keys are processed
/// exactly once per frame even under chronic overrun, with the cap ensuring a
/// key flood can never spin here forever.
///
/// Quit/redraw semantics match the normal path: a `Quit` (in either phase)
/// returns [`PumpOutcome::Quit`] immediately, leaving any later events for the
/// caller to abandon; a `Redraw` in the normal phase returns at once, while a
/// `Redraw` mid-drain finishes the bounded drain before returning
/// [`PumpOutcome::Redraw`], so no buffered key is skipped on the way out.
///
/// `poll` and `read` are seams over [`event::poll`]/[`event::read`] so the phase
/// is unit-tested with scripted closures and no tty.
fn pump_input(
    deadline: Instant,
    poll: &mut impl FnMut(Duration) -> io::Result<bool>,
    read: &mut impl FnMut() -> io::Result<Event>,
    ui: &mut UiState,
) -> io::Result<PumpOutcome> {
    // Normal path: while budget remains, poll with it and handle events as they
    // arrive, breaking at the deadline. Byte-identical to the pre-fix loop.
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining == Duration::ZERO {
            break;
        }
        if poll(remaining)? {
            match handle_event(read()?, ui) {
                Action::Quit => return Ok(PumpOutcome::Quit),
                Action::Redraw => return Ok(PumpOutcome::Redraw),
                Action::None => {}
            }
        }
    }

    // Overrun path: the deadline has passed. Drain buffered input with a zero
    // timeout so keys are never stranded, bounded to MAX_OVERRUN_EVENTS so a
    // flood cannot spin the frame forever — any leftover waits for the next one.
    // A redraw does not short-circuit the drain: it is remembered and the drain
    // runs to completion so no queued key is skipped.
    let mut redraw = false;
    for _ in 0..MAX_OVERRUN_EVENTS {
        if !poll(Duration::ZERO)? {
            break;
        }
        match handle_event(read()?, ui) {
            Action::Quit => return Ok(PumpOutcome::Quit),
            Action::Redraw => redraw = true,
            Action::None => {}
        }
    }
    Ok(if redraw {
        PumpOutcome::Redraw
    } else {
        PumpOutcome::Deadline
    })
}

/// Holds the frozen snapshot across paused frames.
///
/// While paused, [`resolve`](Self::resolve) returns the snapshot captured on the
/// first paused frame and a `dt` of zero, so the presenter neither advances its
/// animation nor responds to new features — every paused frame is identical.
/// Resuming clears the freeze and passes the live snapshot and real `dt` through.
#[derive(Default)]
struct PauseState {
    frozen: Option<FeatureSnapshot>,
}

impl PauseState {
    /// The `(snapshot, dt)` the frame should render with, given the pause flag,
    /// the live snapshot, and the measured frame period.
    fn resolve(&mut self, paused: bool, live: FeatureSnapshot, dt: f32) -> (FeatureSnapshot, f32) {
        if paused {
            (*self.frozen.get_or_insert(live), 0.0)
        } else {
            self.frozen = None;
            (live, dt)
        }
    }
}

/// Translate one input event into a loop [`Action`], mutating [`UiState`] for
/// toggles.
///
/// The rebindable actions come from [`UiState::keymap`]; the browser's internal
/// navigation (highlight up/down, accept), Esc's context-sensitive quit/cancel,
/// Ctrl-C's always-on quit, the `s` debug-line toggle and the `?` help overlay
/// are structural and stay hard-coded.
fn handle_event(event: Event, ui: &mut UiState) -> Action {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            // Any keypress resets the invisible-mode fade, whatever it goes on to
            // do (including nothing).
            ui.chrome.on_input();
            let browsing = ui.scene_mode && ui.scene_nav.is_open();

            // Ctrl-C is a structural, always-on quit, independent of the keymap.
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return Action::Quit;
            }
            // While the tuning strip is open its keys take priority over scene
            // cycling and the browser: tab cycles the parameter, the arrows
            // adjust it, `w` writes back, and esc closes. Everything else (the
            // tuning key itself, `?`, `d`, the other rebindable actions) falls
            // through unchanged.
            if ui.tuning.is_open() {
                match key.code {
                    KeyCode::Esc => {
                        ui.tuning.close();
                        return Action::Redraw;
                    }
                    KeyCode::Tab => {
                        ui.tuning.select_next();
                        return Action::Redraw;
                    }
                    KeyCode::Left => {
                        ui.tuning.adjust_selected(-1);
                        return Action::Redraw;
                    }
                    KeyCode::Right => {
                        ui.tuning.adjust_selected(1);
                        return Action::Redraw;
                    }
                    KeyCode::Char('w') => {
                        ui.tuning_write_pending = true;
                        return Action::Redraw;
                    }
                    _ => {}
                }
            }
            // While the expression-mapping overlay is open its keys take
            // priority. In edit mode the line editor swallows every key (chars,
            // backspace, cursor moves, ⏎ commit, esc cancel) so typing `m`, `?`,
            // `d` or `w` edits the draft rather than triggering an action. In
            // browse mode ↑↓/tab move the selection, ⏎ opens an edit, `w` writes,
            // esc closes; the arrows are swallowed so scene cycling never fires
            // under the overlay, and everything else (the mapping key itself, `?`,
            // `d`, the other actions) falls through unchanged.
            if ui.mapping.is_open() {
                if ui.mapping.is_editing() {
                    match key.code {
                        KeyCode::Esc => ui.mapping.cancel_edit(),
                        KeyCode::Enter => ui.mapping.commit_edit(),
                        KeyCode::Left => ui.mapping.cursor_left(),
                        KeyCode::Right => ui.mapping.cursor_right(),
                        KeyCode::Backspace => ui.mapping.backspace(),
                        KeyCode::Char(c) => ui.mapping.insert_char(c),
                        _ => {}
                    }
                    return Action::Redraw;
                }
                match key.code {
                    KeyCode::Esc => {
                        ui.mapping.close();
                        return Action::Redraw;
                    }
                    KeyCode::Tab | KeyCode::Down => {
                        ui.mapping.select_next();
                        return Action::Redraw;
                    }
                    KeyCode::Up => {
                        ui.mapping.select_prev();
                        return Action::Redraw;
                    }
                    KeyCode::Enter => {
                        ui.mapping.begin_edit();
                        return Action::Redraw;
                    }
                    KeyCode::Left | KeyCode::Right => return Action::Redraw,
                    KeyCode::Char('w') => {
                        ui.mapping_write_pending = true;
                        return Action::Redraw;
                    }
                    _ => {}
                }
            }
            // While scene-author mode is open its keys take priority: the arrows
            // (and PageUp/PageDown) scroll the source pane, and esc closes. The
            // horizontal arrows are swallowed so scene cycling never fires under
            // the pane, keeping the shown source in step with the running scene.
            // Everything else (the author key itself, `?`, the other actions)
            // falls through unchanged.
            if ui.author.is_open() {
                match key.code {
                    KeyCode::Esc => {
                        ui.author.close();
                        return Action::Redraw;
                    }
                    KeyCode::Up => {
                        ui.author.scroll_up();
                        return Action::Redraw;
                    }
                    KeyCode::Down => {
                        ui.author.scroll_down();
                        return Action::Redraw;
                    }
                    KeyCode::PageUp => {
                        ui.author.page_up();
                        return Action::Redraw;
                    }
                    KeyCode::PageDown => {
                        ui.author.page_down();
                        return Action::Redraw;
                    }
                    KeyCode::Left | KeyCode::Right => return Action::Redraw,
                    _ => {}
                }
            }
            // While the device picker is open it is modal: its keys take priority
            // over scene cycling, the browser and Esc-quit. ↑↓ (or j/k) select,
            // ⏎ switches, `p` pins, esc closes. The devices key itself falls
            // through to the keymap, which closes an open picker.
            if ui.devices.is_open() {
                match key.code {
                    KeyCode::Esc => {
                        ui.devices.close();
                        return Action::Redraw;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        ui.devices.select_prev();
                        return Action::Redraw;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        ui.devices.select_next();
                        return Action::Redraw;
                    }
                    KeyCode::Enter => {
                        ui.device_switch_pending = true;
                        return Action::Redraw;
                    }
                    KeyCode::Char('p') => {
                        ui.device_pin_pending = true;
                        return Action::Redraw;
                    }
                    _ => {}
                }
            }
            // Esc is structural and context-sensitive: it closes the browser
            // (restoring the original scene) while open, otherwise it quits.
            if key.code == KeyCode::Esc {
                return if browsing {
                    ui.scene_nav.cancel();
                    Action::Redraw
                } else {
                    Action::Quit
                };
            }
            // While browsing, the navigation keys are structural.
            if browsing {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        ui.scene_nav.highlight_prev();
                        return Action::Redraw;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        ui.scene_nav.highlight_next();
                        return Action::Redraw;
                    }
                    KeyCode::Enter => {
                        ui.scene_nav.accept();
                        return Action::Redraw;
                    }
                    _ => {}
                }
            }
            // The `?` help overlay and the `d` debug line are structural toggles.
            if key.code == KeyCode::Char('?') {
                ui.help = !ui.help;
                return Action::Redraw;
            }
            if key.code == KeyCode::Char('s') {
                ui.debug = !ui.debug;
                return Action::Redraw;
            }
            // Everything else comes from the rebindable keymap.
            match ui.keymap.action_for(&key) {
                Some(action) => apply_action(action, ui, browsing),
                None => Action::None,
            }
        }
        Event::Resize(_, _) => Action::Redraw,
        _ => Action::None,
    }
}

/// Apply a rebindable [`InputAction`], honouring the same context guards the
/// hard-coded handler used: browser/cycle actions act only in scene mode, and
/// cycling only outside the browser.
fn apply_action(action: InputAction, ui: &mut UiState, browsing: bool) -> Action {
    match action {
        InputAction::Quit => Action::Quit,
        InputAction::Browser if ui.scene_mode => {
            ui.scene_nav.toggle_browser();
            Action::Redraw
        }
        InputAction::SceneNext if ui.scene_mode && !browsing => {
            ui.scene_nav.cycle_next();
            Action::Redraw
        }
        InputAction::ScenePrev if ui.scene_mode && !browsing => {
            ui.scene_nav.cycle_prev();
            Action::Redraw
        }
        InputAction::Overlay => {
            ui.overlay = !ui.overlay;
            Action::Redraw
        }
        InputAction::Pause => {
            ui.paused = !ui.paused;
            Action::Redraw
        }
        InputAction::Chrome => {
            ui.chrome.cycle();
            Action::Redraw
        }
        // Toggle the now-playing panel. Works in every mode, paused or not.
        InputAction::NowPlaying => {
            ui.show_now_playing = !ui.show_now_playing;
            Action::Redraw
        }
        // Request a palette apply/revert; the loop decides on the next tick
        // (it owns the presenter), so this only raises the one-shot flag.
        InputAction::Palette => {
            ui.palette_pending = true;
            Action::Redraw
        }
        // Toggle the tuning strip. Closing is immediate; opening is a one-shot
        // request the loop fulfils against the presenter (which it seeds the
        // strip from). Inert with no presenter or no tunable parameters.
        InputAction::Tuning => {
            if ui.tuning.is_open() {
                ui.tuning.close();
            } else {
                ui.tuning_open_pending = true;
            }
            Action::Redraw
        }
        // Toggle the expression-mapping overlay, the sibling of the tuning strip:
        // closing is immediate, opening is a one-shot request the loop fulfils
        // against the presenter's layer-0 `[map]` rows. Inert with no presenter or
        // no mappings.
        InputAction::Mapping => {
            if ui.mapping.is_open() {
                ui.mapping.close();
            } else {
                ui.mapping_open_pending = true;
            }
            Action::Redraw
        }
        // Toggle the device picker. Closing is immediate; opening is a one-shot
        // request the loop fulfils by spawning the off-thread enumeration.
        InputAction::Devices => {
            if ui.devices.is_open() {
                ui.devices.close();
            } else {
                ui.device_open_pending = true;
            }
            Action::Redraw
        }
        // Toggle scene-author mode, the sibling of the tuning strip and mapping
        // overlay: closing is immediate, opening is a one-shot request the loop
        // fulfils by building the source descriptor from the presenter and the
        // scene-file / builtin source. Inert with no presenter (direct-bars).
        InputAction::Author => {
            if ui.author.is_open() {
                ui.author.close();
            } else {
                ui.author_open_pending = true;
            }
            Action::Redraw
        }
        // The browser/cycle guards fall through here when not in scene mode.
        InputAction::Browser | InputAction::SceneNext | InputAction::ScenePrev => Action::None,
    }
}

/// The chrome track line for a now-playing snapshot, gated on playback status.
///
/// Only a session that is actually [`Playing`](scia_meta::PlaybackStatus::Playing)
/// yields a formatted `title — artist` line; a paused or stopped session yields
/// `None`, because such a session is not the audio source the ambient surfaces
/// should name (a paused player's metadata can even be a track that never played
/// on this machine, via cross-device sync). The selection policy still keeps
/// paused sessions — the explicit now-playing panel shows them — but this seam
/// is the one place the chrome and the scene text read, so gating here reverts
/// verso to its fallback word and leaves the invisible-chrome fade untouched by
/// paused-session churn, with no parallel gates elsewhere.
fn playing_track_line(np: Option<&scia_meta::NowPlaying>) -> Option<String> {
    let np = np?;
    if !np.status.is_playing() {
        return None;
    }
    let line = match (np.title.as_deref(), np.artist.as_deref()) {
        (Some(t), Some(a)) => format!("{t} — {a}"),
        (Some(t), None) => t.to_string(),
        (None, Some(a)) => a.to_string(),
        (None, None) => String::new(),
    };
    Some(line).filter(|s| !s.is_empty())
}

/// Resolve a palette-key press: apply the current track's art palette to the
/// live scene via crossfade, or, if it is already applied, revert to the scene's
/// own palette. With no presenter (direct-bars / no `--scene`) or no decoded art
/// (demo, nothing playing) it is a no-op that leaves a short status note rather
/// than erroring. Sets `ui.notice` in every branch.
fn apply_palette_toggle(
    presenter: Option<&mut ScenePresenter>,
    ui: &mut UiState,
    scene_base: &mut Option<Palette>,
) {
    let Some(p) = presenter else {
        ui.notice = Some("no scene to theme".to_string());
        return;
    };
    if ui.palette_applied {
        // Revert: crossfade back to the remembered scene palette.
        let base = scene_base.take().unwrap_or_else(|| p.palette());
        p.fade_palette(base);
        ui.palette_applied = false;
        ui.notice = Some("scene palette".to_string());
    } else if let Some(art) = ui.now_playing.art_palette() {
        // Apply: remember the scene palette, then crossfade to the art palette.
        let art_palette = art_palette_to_scene(art);
        *scene_base = Some(p.palette());
        p.fade_palette(art_palette);
        ui.palette_applied = true;
        ui.notice = Some("art palette".to_string());
    } else {
        ui.notice = Some("nothing playing".to_string());
    }
}

/// Build the tuning-strip parameter list from the presenter's first layer: every
/// manifest key with its current bag value and whether a `[map]` entry drives it.
/// The strip itself slices this to the first few keys.
fn build_tuning_params(p: &ScenePresenter) -> Vec<TuningParam> {
    p.layer0_specs()
        .iter()
        .map(|s| TuningParam {
            key: s.key,
            min: s.min,
            max: s.max,
            value: p.layer0_value(s.key),
            mapped: p.layer0_mapped(s.key),
        })
        .collect()
}

/// Write the tuning strip's adjusted values back to the preset, returning the
/// status notice to show. With a `--scene-file` it edits that file in place
/// (comments intact); with a builtin preset it exports to
/// `<config_dir>/presets/<name>.toml`, the running scene naming the file. Never
/// panics — every failure is reported as a notice.
fn write_tuning(
    presenter: Option<&ScenePresenter>,
    tuning: &TuningStrip,
    nav: &SceneNav,
    opts: &TuiOptions,
) -> String {
    let edits = tuning.dirty_edits();
    if edits.is_empty() {
        return "nothing to write".to_string();
    }
    // An existing scene file is edited in place, comments preserved.
    if let Some(path) = &opts.scene_file {
        return match tuning::write_back_file(path, edits) {
            Ok(()) => format!("wrote {}", file_label(path)),
            Err(err) => format!("write failed: {err}"),
        };
    }
    // Otherwise export the running builtin preset under the config dir. The name
    // is the currently running scene (cycling may have moved it off `--scene`).
    let Some(base) = &opts.config_dir else {
        return "no config dir for export".to_string();
    };
    let name = nav
        .current_id()
        .or(opts.scene.as_deref())
        .or_else(|| presenter.and_then(ScenePresenter::layer0_scene_id));
    let Some(name) = name else {
        return "no preset to write".to_string();
    };
    let Some(src) = builtin_presets()
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| *s)
    else {
        return format!("no builtin source for {name}");
    };
    match tuning::write_back_export(base, name, src, edits) {
        Ok(path) => format!("wrote {}", path.display()),
        Err(err) => format!("write failed: {err}"),
    }
}

/// Write the expression-mapping overlay's committed rows back to the preset,
/// returning the status notice to show. The `[map]` sibling of [`write_tuning`]:
/// with a `--scene-file` it edits that file in place (comments intact), each
/// dirty row as an expression string; with a builtin it exports to
/// `<config_dir>/presets/<name>.toml`. Never panics — every failure is a notice.
fn write_mapping(
    presenter: Option<&ScenePresenter>,
    mapping: &MappingUi,
    nav: &SceneNav,
    opts: &TuiOptions,
) -> String {
    let edits = mapping.dirty_edits();
    if edits.is_empty() {
        return "nothing to write".to_string();
    }
    // An existing scene file is edited in place, comments preserved.
    if let Some(path) = &opts.scene_file {
        return match tuning::write_map_file(path, &edits) {
            Ok(()) => format!("wrote {}", file_label(path)),
            Err(err) => format!("write failed: {err}"),
        };
    }
    // Otherwise export the running builtin preset under the config dir. The name
    // is the currently running scene (cycling may have moved it off `--scene`).
    let Some(base) = &opts.config_dir else {
        return "no config dir for export".to_string();
    };
    let name = nav
        .current_id()
        .or(opts.scene.as_deref())
        .or_else(|| presenter.and_then(ScenePresenter::layer0_scene_id));
    let Some(name) = name else {
        return "no preset to write".to_string();
    };
    let Some(src) = builtin_presets()
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| *s)
    else {
        return format!("no builtin source for {name}");
    };
    match tuning::write_map_export(base, name, src, &edits) {
        Ok(path) => format!("wrote {}", path.display()),
        Err(err) => format!("write failed: {err}"),
    }
}

/// Build the source descriptor scene-author mode opens on: the `--scene-file`
/// on disk when one is set, otherwise the running builtin preset's embedded
/// source. The builtin name is the currently running scene (cycling may have
/// moved it off `--scene`), mirroring the write-back path's name resolution.
/// `None` when no source can be resolved.
fn author_source(
    opts: &TuiOptions,
    nav: &SceneNav,
    presenter: &ScenePresenter,
) -> Option<SceneSource> {
    if let Some(path) = &opts.scene_file {
        return Some(SceneSource::from_file(path));
    }
    let name = nav
        .current_id()
        .or(opts.scene.as_deref())
        .or_else(|| presenter.layer0_scene_id())?;
    SceneSource::builtin(name)
}

/// The did-you-mean vocabulary for scene-author mode: the expression signal
/// names plus the active scene's parameter keys (its `ParamSpec` manifest).
fn author_vocab(presenter: &ScenePresenter) -> Vec<String> {
    let mut vocab: Vec<String> = expression_vocabulary()
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    for spec in presenter.layer0_specs() {
        vocab.push(spec.key.to_string());
    }
    vocab
}

/// Pin the picker's selected device into the config file, returning the status
/// notice. Needs the config dir (`--headless`/no-config runs have none) and a
/// selected row; pinning the follow-system entry removes the key. Never panics —
/// every failure is a notice.
fn pin_device_selection(picker: &DevicePicker, opts: &TuiOptions) -> String {
    let Some(row) = picker.selected_row() else {
        return "no device to pin".to_string();
    };
    let Some(dir) = &opts.config_dir else {
        return "no config dir to pin into".to_string();
    };
    match devicepick::pin_device(dir, &row.selector) {
        Ok(_) => match &row.selector {
            DeviceSelector::Default => "unpinned (follow system)".to_string(),
            DeviceSelector::Named(_) => format!("pinned {}", row.name),
        },
        Err(err) => format!("pin failed: {err}"),
    }
}

/// A short label for a written file: its file name, or the full path when it has
/// none.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
    use scia_scenes::builtin_preset;

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })
    }

    fn press_ctrl(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })
    }

    #[test]
    fn backtick_toggles_overlay() {
        let mut ui = UiState::default();
        assert!(!ui.overlay);
        assert!(matches!(
            handle_event(press(KeyCode::Char('`')), &mut ui),
            Action::Redraw
        ));
        assert!(ui.overlay, "backtick should turn the overlay on");
        assert!(matches!(
            handle_event(press(KeyCode::Char('`')), &mut ui),
            Action::Redraw
        ));
        assert!(!ui.overlay, "backtick should turn the overlay back off");
    }

    #[test]
    fn overlay_and_debug_toggle_independently() {
        let mut ui = UiState::default();
        // The debug-line key (`s`, since `d` now opens the device picker) keeps
        // its meaning and does not touch the overlay.
        let _ = handle_event(press(KeyCode::Char('s')), &mut ui);
        assert!(ui.debug);
        assert!(!ui.overlay);
        // The overlay key does not touch the debug line.
        let _ = handle_event(press(KeyCode::Char('`')), &mut ui);
        assert!(ui.debug);
        assert!(ui.overlay);
    }

    #[test]
    fn d_opens_the_device_picker_and_toggles_it_shut() {
        let mut ui = UiState::default();
        assert!(!ui.device_open_pending);
        // `d` requests the picker (the loop spawns the enumeration).
        assert!(matches!(
            handle_event(press(KeyCode::Char('d')), &mut ui),
            Action::Redraw
        ));
        assert!(ui.device_open_pending, "d asks the loop to open the picker");
        assert!(!ui.debug, "d no longer toggles the debug line");
        // With the picker open, `d` closes it (mirrors the tuning `t` toggle).
        ui.devices.open_enumerating();
        let _ = handle_event(press(KeyCode::Char('d')), &mut ui);
        assert!(!ui.devices.is_open(), "d closes an open picker");
    }

    #[test]
    fn device_picker_keys_are_modal_while_open() {
        use crate::devicepick::{CaptureFilter, build_rows};
        use scia_core::{DeviceInfo, DeviceKind};
        let mut ui = scene_ui();
        // Seed the picker with rows so selection has somewhere to go.
        ui.devices.open_enumerating();
        let devices = vec![
            DeviceInfo {
                name: "one".into(),
                is_default_input: true,
                is_default_output: true,
                kind: DeviceKind::Output,
                host: "pipewire".into(),
            },
            DeviceInfo {
                name: "two".into(),
                is_default_input: false,
                is_default_output: false,
                kind: DeviceKind::Output,
                host: "pipewire".into(),
            },
        ];
        let filter = CaptureFilter {
            kind: DeviceKind::Output,
            host: Some("pipewire".into()),
        };
        let _ = build_rows(&devices, &filter, &DeviceSelector::Default);
        ui.devices.set_devices(Ok(devices));
        let n = ui.devices.rows().len();
        assert!(n >= 2, "fixture yields at least two rows");

        // Down moves the selection, it does not cycle the scene.
        let start = ui.devices.selected();
        let _ = handle_event(press(KeyCode::Down), &mut ui);
        assert_eq!(ui.devices.selected(), (start + 1) % n);
        assert_eq!(ui.scene_nav.take_pending(), None, "no scene cycle");
        // Enter raises the one-shot switch request.
        let _ = handle_event(press(KeyCode::Enter), &mut ui);
        assert!(ui.device_switch_pending, "enter requests a switch");
        // `p` pins rather than applying the palette.
        let _ = handle_event(press(KeyCode::Char('p')), &mut ui);
        assert!(ui.device_pin_pending, "p requests a pin");
        assert!(
            !ui.palette_pending,
            "p does not apply the palette while modal"
        );
        // Esc closes the picker instead of quitting.
        assert!(matches!(
            handle_event(press(KeyCode::Esc), &mut ui),
            Action::Redraw
        ));
        assert!(!ui.devices.is_open());
    }

    #[test]
    fn rebinding_devices_drives_the_new_key() {
        let mut ui = UiState {
            keymap: Keymap {
                devices: Some(KeyChord::plain(KeyCode::Char('g'))),
                ..Keymap::default()
            },
            ..UiState::default()
        };
        // The rebound key requests the picker; the old `d` no longer does.
        let _ = handle_event(press(KeyCode::Char('g')), &mut ui);
        assert!(ui.device_open_pending);
        ui.device_open_pending = false;
        let _ = handle_event(press(KeyCode::Char('d')), &mut ui);
        assert!(
            !ui.device_open_pending,
            "the old devices key is inert after a rebind"
        );
    }

    /// A UiState with the browser/cycle keys live, as the loop sets it on the
    /// built-in `--scene` path. The registry order is spectra, lattice, aurora,
    /// starfall.
    fn scene_ui() -> UiState {
        UiState {
            scene_mode: true,
            ..UiState::default()
        }
    }

    #[test]
    fn tab_toggles_browser_and_esc_restores_original() {
        let mut ui = scene_ui();
        // Closed: Esc quits.
        assert!(matches!(
            handle_event(press(KeyCode::Esc), &mut ui),
            Action::Quit
        ));
        // Tab opens the browser on the committed scene (spectra).
        assert!(matches!(
            handle_event(press(KeyCode::Tab), &mut ui),
            Action::Redraw
        ));
        assert!(ui.scene_nav.is_open());
        // Highlight down previews the next scene: a switch to lattice is asked.
        let _ = handle_event(press(KeyCode::Down), &mut ui);
        assert_eq!(ui.scene_nav.highlight(), 1);
        assert_eq!(ui.scene_nav.take_pending(), Some("lattice"));
        // Esc closes and crossfades back to the original scene, not a quit.
        assert!(matches!(
            handle_event(press(KeyCode::Esc), &mut ui),
            Action::Redraw
        ));
        assert!(!ui.scene_nav.is_open());
        assert_eq!(
            ui.scene_nav.take_pending(),
            Some("spectra"),
            "Esc asks the presenter for the original scene id"
        );
        // Closed again: Esc quits only now that the browser is shut.
        assert!(matches!(
            handle_event(press(KeyCode::Esc), &mut ui),
            Action::Quit
        ));
    }

    #[test]
    fn highlight_moves_crossfade_to_the_highlighted_scene() {
        let mut ui = scene_ui();
        let _ = handle_event(press(KeyCode::Tab), &mut ui);
        // `j` moves down like Down; `k` moves up like Up.
        let _ = handle_event(press(KeyCode::Char('j')), &mut ui);
        assert_eq!(ui.scene_nav.take_pending(), Some("lattice"));
        let _ = handle_event(press(KeyCode::Char('j')), &mut ui);
        assert_eq!(ui.scene_nav.take_pending(), Some("aurora"));
        let _ = handle_event(press(KeyCode::Char('k')), &mut ui);
        assert_eq!(ui.scene_nav.take_pending(), Some("lattice"));
        // The highlight clamps at the top rather than wrapping.
        let _ = handle_event(press(KeyCode::Char('k')), &mut ui);
        assert_eq!(ui.scene_nav.take_pending(), Some("spectra"));
        let _ = handle_event(press(KeyCode::Char('k')), &mut ui);
        assert_eq!(ui.scene_nav.highlight(), 0);
        assert_eq!(
            ui.scene_nav.take_pending(),
            None,
            "clamped move asks nothing"
        );
    }

    #[test]
    fn enter_keeps_the_highlighted_scene() {
        let mut ui = scene_ui();
        let _ = handle_event(press(KeyCode::Tab), &mut ui);
        let _ = handle_event(press(KeyCode::Down), &mut ui); // preview lattice
        let _ = ui.scene_nav.take_pending(); // drain the preview switch
        assert!(matches!(
            handle_event(press(KeyCode::Enter), &mut ui),
            Action::Redraw
        ));
        assert!(!ui.scene_nav.is_open());
        assert_eq!(ui.scene_nav.current(), 1, "Enter commits the highlight");
        assert_eq!(
            ui.scene_nav.take_pending(),
            None,
            "the kept scene is already live; nothing to re-switch"
        );
        // A later Esc quits: the browser is closed.
        assert!(matches!(
            handle_event(press(KeyCode::Esc), &mut ui),
            Action::Quit
        ));
    }

    #[test]
    fn arrows_cycle_scenes_in_registry_order_and_wrap() {
        use scia_scenes::{builtin_scenes, catalog_scenes};

        // Cycling follows the catalogue order and wraps. The catalogue is the
        // built-ins first, in their locked order, then the Luau scenes (the two
        // shipped ones, plus any drop-ins on this machine), so the expected
        // sequence is derived from it — robust to whatever is installed — while
        // the two explicit checks below pin the guarantees that matter: the
        // built-in prefix is in its locked order, and the shipped Luau scenes
        // are catalogued.
        let ids: Vec<&str> = catalog_scenes().iter().map(|i| i.id).collect();
        let builtins = builtin_scenes();
        for (i, b) in builtins.iter().enumerate() {
            assert_eq!(ids[i], b.id, "built-ins keep their locked order");
        }
        assert!(
            ids.contains(&"ripple") && ids.contains(&"swarm"),
            "the shipped Luau scenes are catalogued: {ids:?}"
        );

        let mut ui = scene_ui();
        // Starting committed at index 0, each Right advances one step and wraps
        // after the last back to the first.
        for k in 1..=ids.len() {
            assert!(matches!(
                handle_event(press(KeyCode::Right), &mut ui),
                Action::Redraw
            ));
            assert_eq!(ui.scene_nav.take_pending(), Some(ids[k % ids.len()]));
        }
        // Left wraps backward from the first scene to the last.
        assert!(matches!(
            handle_event(press(KeyCode::Left), &mut ui),
            Action::Redraw
        ));
        assert_eq!(ui.scene_nav.take_pending(), Some(*ids.last().unwrap()));
    }

    #[test]
    fn cycle_raises_a_toast_that_expires_on_the_frame_clock() {
        let mut ui = scene_ui();
        let _ = handle_event(press(KeyCode::Right), &mut ui);
        let toast = ui.scene_nav.toast_text().expect("cycling raises a toast");
        assert!(
            toast.contains("lattice"),
            "toast names the scene: {toast:?}"
        );
        assert!(
            toast.contains('●'),
            "toast shows the filled position dot: {toast:?}"
        );
        // Just short of the ~2 s timer: still shown.
        ui.scene_nav.tick(1.9);
        assert!(ui.scene_nav.toast_text().is_some());
        // Past the timer: it expires.
        ui.scene_nav.tick(0.2);
        assert!(
            ui.scene_nav.toast_text().is_none(),
            "toast expires after its timer"
        );
    }

    #[test]
    fn browser_keys_are_inert_without_a_scene() {
        // Direct-bars mode leaves scene_mode off; the new keys fall through and
        // Esc keeps its quit meaning.
        let mut ui = UiState::default();
        assert!(matches!(
            handle_event(press(KeyCode::Tab), &mut ui),
            Action::None
        ));
        assert!(!ui.scene_nav.is_open());
        assert!(matches!(
            handle_event(press(KeyCode::Right), &mut ui),
            Action::None
        ));
        assert_eq!(ui.scene_nav.take_pending(), None);
        assert!(matches!(
            handle_event(press(KeyCode::Esc), &mut ui),
            Action::Quit
        ));
    }

    #[test]
    fn space_toggles_pause() {
        let mut ui = UiState::default();
        assert!(!ui.paused);
        assert!(matches!(
            handle_event(press(KeyCode::Char(' ')), &mut ui),
            Action::Redraw
        ));
        assert!(ui.paused, "space should pause");
        assert!(matches!(
            handle_event(press(KeyCode::Char(' ')), &mut ui),
            Action::Redraw
        ));
        assert!(!ui.paused, "space should resume");
    }

    #[test]
    fn n_toggles_the_now_playing_panel() {
        let mut ui = UiState::default();
        assert!(!ui.show_now_playing);
        assert!(matches!(
            handle_event(press(KeyCode::Char('n')), &mut ui),
            Action::Redraw
        ));
        assert!(ui.show_now_playing, "n opens the now-playing panel");
        assert!(matches!(
            handle_event(press(KeyCode::Char('n')), &mut ui),
            Action::Redraw
        ));
        assert!(!ui.show_now_playing, "n closes it again");
    }

    #[test]
    fn now_playing_toggles_while_paused() {
        // The panel toggle is independent of pause: a paused scene can still open
        // and close the now-playing panel.
        let mut ui = UiState {
            paused: true,
            ..UiState::default()
        };
        let _ = handle_event(press(KeyCode::Char('n')), &mut ui);
        assert!(ui.show_now_playing, "panel opens while paused");
        assert!(ui.paused, "toggling the panel does not resume");
        let _ = handle_event(press(KeyCode::Char('n')), &mut ui);
        assert!(!ui.show_now_playing, "panel closes while still paused");
        assert!(ui.paused);
    }

    #[test]
    fn p_raises_a_palette_request() {
        // `p` only raises the one-shot request; the loop resolves it against the
        // presenter on the next tick.
        let mut ui = UiState::default();
        assert!(!ui.palette_pending);
        assert!(matches!(
            handle_event(press(KeyCode::Char('p')), &mut ui),
            Action::Redraw
        ));
        assert!(ui.palette_pending, "p requests a palette apply/revert");
    }

    #[test]
    fn palette_toggle_no_ops_without_a_scene() {
        // With no presenter (direct-bars) the toggle notes it and never flips.
        let mut ui = UiState::default();
        let mut base = None;
        apply_palette_toggle(None, &mut ui, &mut base);
        assert!(!ui.palette_applied);
        assert_eq!(ui.notice.as_deref(), Some("no scene to theme"));
    }

    #[test]
    fn palette_toggle_notes_nothing_playing_when_no_art() {
        // A presenter exists but nothing is playing: a no-op with a status note.
        let preset = builtin_preset("spectra").expect("preset").expect("parses");
        let mut p = presenter::ScenePresenter::from_preset(&preset, Tier::Half);
        let mut ui = UiState::default();
        let mut base = None;
        apply_palette_toggle(Some(&mut p), &mut ui, &mut base);
        assert!(!ui.palette_applied, "no art means nothing to apply");
        assert_eq!(ui.notice.as_deref(), Some("nothing playing"));
    }

    #[test]
    fn palette_toggle_applies_and_reverts_with_art() {
        use scia_meta::ArtPalette;
        let preset = builtin_preset("spectra").expect("preset").expect("parses");
        let mut p = presenter::ScenePresenter::from_preset(&preset, Tier::Half);
        let scene_palette = p.palette();

        // Seed a now-playing track with a decoded art palette.
        let track = scia_meta::NowPlaying::new(
            Some("T".into()),
            Some("A".into()),
            None,
            scia_meta::PlaybackStatus::Playing,
            None,
            None,
        );
        let key = track.track_key.clone();
        let mut ui = UiState::default();
        ui.now_playing.current = Some(track);
        ui.now_playing.apply_art(
            key,
            scia_meta::PreviewImage {
                width: 1,
                height: 1,
                pixels: vec![[9, 9, 9]],
            },
            ArtPalette {
                dominant: [200, 10, 10],
                accents: vec![],
                light: [255, 60, 60],
                dark: [90, 0, 0],
                slots: [[200, 10, 10]; 8],
            },
        );

        let mut base = None;
        // Apply: remembers the scene palette and starts a fade toward the art one.
        apply_palette_toggle(Some(&mut p), &mut ui, &mut base);
        assert!(ui.palette_applied);
        assert_eq!(base, Some(scene_palette), "the scene palette is remembered");
        assert!(p.is_palette_fading());

        // Revert: fades back to the remembered scene palette.
        apply_palette_toggle(Some(&mut p), &mut ui, &mut base);
        assert!(!ui.palette_applied);
        for _ in 0..12 {
            p.frame(&scia_core::FeatureSnapshot::default(), 0.05);
        }
        assert_eq!(p.palette(), scene_palette, "reverts to the scene palette");
    }

    /// A now-playing snapshot with the given title/artist and playback status.
    fn np_status(
        title: &str,
        artist: &str,
        status: scia_meta::PlaybackStatus,
    ) -> scia_meta::NowPlaying {
        scia_meta::NowPlaying::new(
            Some(title.into()),
            Some(artist.into()),
            None,
            status,
            None,
            None,
        )
    }

    #[test]
    fn paused_session_produces_no_chrome_track_line() {
        // A paused session is not the audio source, so the ambient seam yields
        // nothing even though the metadata carries a title and artist.
        let paused = np_status(
            "Ghost Track",
            "Someone Else",
            scia_meta::PlaybackStatus::Paused,
        );
        assert_eq!(playing_track_line(Some(&paused)), None);

        // A stopped session likewise yields nothing.
        let stopped = np_status(
            "Ghost Track",
            "Someone Else",
            scia_meta::PlaybackStatus::Stopped,
        );
        assert_eq!(playing_track_line(Some(&stopped)), None);

        // Absence yields nothing.
        assert_eq!(playing_track_line(None), None);
    }

    #[test]
    fn playing_session_produces_the_formatted_track_line() {
        let playing = np_status("Real Song", "Real Band", scia_meta::PlaybackStatus::Playing);
        assert_eq!(
            playing_track_line(Some(&playing)).as_deref(),
            Some("Real Song — Real Band"),
        );
    }

    #[test]
    fn play_to_pause_flip_clears_the_chrome_track_line() {
        // The single seam: flipping Playing→Paused turns the chrome line from the
        // formatted string to `None`; the loop's track-change path then pushes an
        // empty `set_text("track", "")`, which the verso test below verifies.
        let np = np_status("Real Song", "Real Band", scia_meta::PlaybackStatus::Playing);
        assert!(playing_track_line(Some(&np)).is_some());
        let np = np_status("Real Song", "Real Band", scia_meta::PlaybackStatus::Paused);
        assert_eq!(playing_track_line(Some(&np)), None);
    }

    #[test]
    fn pausing_reverts_verso_to_its_fallback_word() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        // Rasterize a verso presenter into a fresh buffer and flatten it to text.
        fn text_of(p: &ScenePresenter, cols: u16, rows: u16) -> String {
            let area = Rect::new(0, 0, cols, rows);
            let mut buf = Buffer::empty(area);
            p.draw(&mut buf, area);
            buf.content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect()
        }

        let preset = builtin_preset("verso")
            .expect("verso is a built-in preset")
            .expect("verso parses");
        let mut p = presenter::ScenePresenter::from_preset(&preset, Tier::Half);
        let (cols, rows) = (48u16, 16u16);
        p.resize(cols, rows);

        let mut snap = FeatureSnapshot {
            spectrum_len: scia_core::SPECTRUM_BINS as u16,
            ..FeatureSnapshot::default()
        };
        for b in &mut snap.spectrum {
            *b = 0.8;
        }

        // A Playing session drives the seam, and the loop forwards its line to the
        // scene: verso draws the title's glyphs.
        let playing = np_status("Zephyr", "Band", scia_meta::PlaybackStatus::Playing);
        let line = playing_track_line(Some(&playing));
        p.set_text("track", line.as_deref().unwrap_or(""));
        for _ in 0..8 {
            p.frame(&snap, 0.05);
        }
        let showing = text_of(&p, cols, rows);
        assert!(
            showing.contains('Z') && showing.contains('y'),
            "verso shows the playing title: {showing:?}"
        );

        // The session pauses: the seam yields `None`, the loop pushes an empty
        // track line, and verso reverts to its fallback word `scia`.
        let paused = np_status("Zephyr", "Band", scia_meta::PlaybackStatus::Paused);
        let line = playing_track_line(Some(&paused));
        assert_eq!(line, None, "a paused session clears the chrome line");
        p.set_text("track", line.as_deref().unwrap_or(""));
        for _ in 0..8 {
            p.frame(&snap, 0.05);
        }
        let fallback = text_of(&p, cols, rows);
        for ch in ['s', 'c', 'i', 'a'] {
            assert!(
                fallback.contains(ch),
                "verso reverts to the fallback word `scia` (`{ch}`): {fallback:?}"
            );
        }
    }

    #[test]
    fn a_paused_session_change_does_not_reset_the_invisible_fade() {
        // The invisible-mode fade resets only when the chrome track line changes.
        // Two different paused sessions both gate to `None`, so the loop's
        // `cur_track != last_track` guard never fires and the fade keeps counting.
        let mut cs = ChromeState::new(ChromeMode::Invisible);
        cs.tick(5.0);
        assert_eq!(cs.fade(), Fade::Hidden);

        let last = playing_track_line(Some(&np_status(
            "First Paused",
            "A",
            scia_meta::PlaybackStatus::Paused,
        )));
        let next = playing_track_line(Some(&np_status(
            "Second Paused",
            "B",
            scia_meta::PlaybackStatus::Paused,
        )));
        assert_eq!(last, None);
        assert_eq!(next, None);
        if last != next {
            cs.on_track_change();
        }
        assert_eq!(
            cs.fade(),
            Fade::Hidden,
            "a paused-session metadata change must not reset the fade"
        );

        // For contrast: a session that starts Playing does change the line and so
        // resets the fade, exactly as a track change does today.
        let playing = playing_track_line(Some(&np_status(
            "Now Live",
            "C",
            scia_meta::PlaybackStatus::Playing,
        )));
        if next != playing {
            cs.on_track_change();
        }
        assert_eq!(
            cs.fade(),
            Fade::Full,
            "a session becoming Playing resets the fade like a track change"
        );
    }

    /// A scene-mode UiState with the tuning strip already open on two params, as
    /// the loop would have seeded it from the presenter.
    fn ui_with_open_strip() -> UiState {
        let mut ui = UiState {
            scene_mode: true,
            ..UiState::default()
        };
        ui.tuning.open(vec![
            TuningParam {
                key: "gap",
                min: 0.0,
                max: 1.0,
                value: 0.5,
                mapped: false,
            },
            TuningParam {
                key: "punch",
                min: 0.0,
                max: 2.0,
                value: 0.3,
                mapped: true,
            },
        ]);
        ui
    }

    #[test]
    fn t_requests_the_tuning_strip() {
        let mut ui = UiState::default();
        assert!(!ui.tuning_open_pending);
        assert!(matches!(
            handle_event(press(KeyCode::Char('t')), &mut ui),
            Action::Redraw
        ));
        assert!(ui.tuning_open_pending, "t asks the loop to open the strip");
    }

    #[test]
    fn t_closes_the_strip_when_open() {
        let mut ui = ui_with_open_strip();
        assert!(ui.tuning.is_open());
        let _ = handle_event(press(KeyCode::Char('t')), &mut ui);
        assert!(!ui.tuning.is_open(), "t closes an open strip");
        assert!(
            !ui.tuning_open_pending,
            "closing does not re-request an open"
        );
    }

    #[test]
    fn tuning_keys_take_priority_over_scene_cycling_while_open() {
        let mut ui = ui_with_open_strip();
        // Tab cycles the selected parameter, it does not open the browser.
        let _ = handle_event(press(KeyCode::Tab), &mut ui);
        assert_eq!(ui.tuning.selected(), 1);
        assert!(!ui.scene_nav.is_open(), "tab did not open the browser");
        // Right adjusts the selected value; it does not cycle the scene.
        let before = ui.tuning.params()[1].value;
        let _ = handle_event(press(KeyCode::Right), &mut ui);
        assert!(ui.tuning.params()[1].value >= before);
        assert_eq!(
            ui.scene_nav.take_pending(),
            None,
            "right did not cycle the scene"
        );
        // `w` raises a one-shot write request.
        let _ = handle_event(press(KeyCode::Char('w')), &mut ui);
        assert!(ui.tuning_write_pending, "w requests a write-back");
        // Esc closes the strip rather than quitting.
        assert!(matches!(
            handle_event(press(KeyCode::Esc), &mut ui),
            Action::Redraw
        ));
        assert!(!ui.tuning.is_open());
    }

    /// A scene-mode UiState with the expression-mapping overlay already open on
    /// two rows, as the loop would have seeded it from the presenter.
    fn ui_with_open_map() -> UiState {
        use scia_scenes::{ExprMapping, MapEntry};
        let mut ui = UiState {
            scene_mode: true,
            ..UiState::default()
        };
        ui.mapping.open(vec![
            MapEntry::Expr(ExprMapping::compile("gap", "bass").expect("compiles")),
            MapEntry::Expr(ExprMapping::compile("punch", "onset").expect("compiles")),
        ]);
        ui
    }

    #[test]
    fn m_requests_the_mapping_overlay() {
        let mut ui = UiState::default();
        assert!(!ui.mapping_open_pending);
        assert!(matches!(
            handle_event(press(KeyCode::Char('m')), &mut ui),
            Action::Redraw
        ));
        assert!(
            ui.mapping_open_pending,
            "m asks the loop to open the overlay"
        );
    }

    #[test]
    fn m_closes_the_overlay_when_open() {
        let mut ui = ui_with_open_map();
        assert!(ui.mapping.is_open());
        let _ = handle_event(press(KeyCode::Char('m')), &mut ui);
        assert!(!ui.mapping.is_open(), "m closes an open overlay");
        assert!(
            !ui.mapping_open_pending,
            "closing does not re-request an open"
        );
    }

    #[test]
    fn mapping_keys_take_priority_and_edit_swallows_chars() {
        let mut ui = ui_with_open_map();
        // Tab moves the selection; it does not open the browser.
        let _ = handle_event(press(KeyCode::Tab), &mut ui);
        assert_eq!(ui.mapping.selected(), 1);
        assert!(!ui.scene_nav.is_open(), "tab did not open the browser");
        // Down also moves the selection (wraps back to 0).
        let _ = handle_event(press(KeyCode::Down), &mut ui);
        assert_eq!(ui.mapping.selected(), 0);

        // Enter opens an inline edit of the selected row.
        let _ = handle_event(press(KeyCode::Enter), &mut ui);
        assert!(ui.mapping.is_editing());

        // While editing, `m`, `w` and `?` are literal characters, not actions.
        for c in ['m', 'w', '?'] {
            let _ = handle_event(press(KeyCode::Char(c)), &mut ui);
        }
        assert!(ui.mapping.is_open(), "typing m did not toggle the overlay");
        assert!(
            !ui.mapping_write_pending,
            "typing w did not request a write"
        );
        assert!(!ui.help, "typing ? did not open help");
        assert!(
            ui.mapping.edit_buffer().unwrap().ends_with("mw?"),
            "the chars landed in the draft: {:?}",
            ui.mapping.edit_buffer()
        );

        // Esc cancels the edit but keeps the overlay open.
        let _ = handle_event(press(KeyCode::Esc), &mut ui);
        assert!(!ui.mapping.is_editing(), "esc left edit mode");
        assert!(ui.mapping.is_open(), "esc did not close the overlay");

        // `w` now raises a one-shot write request.
        let _ = handle_event(press(KeyCode::Char('w')), &mut ui);
        assert!(ui.mapping_write_pending, "w requests a write-back");

        // Esc closes the overlay rather than quitting.
        assert!(matches!(
            handle_event(press(KeyCode::Esc), &mut ui),
            Action::Redraw
        ));
        assert!(!ui.mapping.is_open());
    }

    #[test]
    fn mapping_edit_commits_and_dirties_a_row() {
        let mut ui = ui_with_open_map();
        // Edit the first row: append " * 0.5" and commit with Enter.
        let _ = handle_event(press(KeyCode::Enter), &mut ui);
        for c in " * 0.5".chars() {
            let _ = handle_event(press(KeyCode::Char(c)), &mut ui);
        }
        let _ = handle_event(press(KeyCode::Enter), &mut ui);
        assert!(!ui.mapping.is_editing(), "enter committed the edit");
        assert!(ui.mapping.is_dirty("gap"), "the row is dirty after commit");
        assert_eq!(ui.mapping.dirty_edits(), vec![("gap", "bass * 0.5")]);
    }

    /// A scene-mode UiState with author mode already open on a builtin source,
    /// as the loop would have seeded it from the presenter.
    fn ui_with_open_author() -> UiState {
        let mut ui = UiState {
            scene_mode: true,
            ..UiState::default()
        };
        let source = SceneSource::builtin("spectra").expect("spectra builtin");
        let vocab: Vec<String> = expression_vocabulary()
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        ui.author.open(source, vocab);
        ui
    }

    #[test]
    fn a_requests_scene_author_mode() {
        let mut ui = UiState::default();
        assert!(!ui.author_open_pending);
        assert!(matches!(
            handle_event(press(KeyCode::Char('a')), &mut ui),
            Action::Redraw
        ));
        assert!(
            ui.author_open_pending,
            "a asks the loop to open scene-author mode"
        );
    }

    #[test]
    fn a_closes_author_mode_when_open() {
        let mut ui = ui_with_open_author();
        assert!(ui.author.is_open());
        let _ = handle_event(press(KeyCode::Char('a')), &mut ui);
        assert!(!ui.author.is_open(), "a closes an open author mode");
        assert!(
            !ui.author_open_pending,
            "closing does not re-request an open"
        );
    }

    #[test]
    fn author_keys_scroll_and_take_priority_over_cycling() {
        let mut ui = ui_with_open_author();
        // Down scrolls the source; it does not cycle the scene.
        let _ = handle_event(press(KeyCode::Down), &mut ui);
        assert_eq!(ui.author.scroll(), 1);
        // Left/Right are swallowed so scene cycling never fires under the pane.
        let _ = handle_event(press(KeyCode::Right), &mut ui);
        assert_eq!(
            ui.scene_nav.take_pending(),
            None,
            "right did not cycle the scene"
        );
        assert!(!ui.scene_nav.is_open(), "no browser opened");
        // Up scrolls back.
        let _ = handle_event(press(KeyCode::Up), &mut ui);
        assert_eq!(ui.author.scroll(), 0);
        // Esc closes author mode rather than quitting.
        assert!(matches!(
            handle_event(press(KeyCode::Esc), &mut ui),
            Action::Redraw
        ));
        assert!(!ui.author.is_open());
    }

    #[test]
    fn question_mark_toggles_help() {
        let mut ui = UiState::default();
        assert!(!ui.help);
        let _ = handle_event(press(KeyCode::Char('?')), &mut ui);
        assert!(ui.help, "? should open the help overlay");
        let _ = handle_event(press(KeyCode::Char('?')), &mut ui);
        assert!(!ui.help, "? should close the help overlay");
    }

    #[test]
    fn ctrl_c_quits_regardless_of_the_keymap() {
        // Even with quit rebound elsewhere, Ctrl-C stays a structural quit.
        let mut ui = UiState {
            keymap: Keymap {
                quit: Some(KeyChord::plain(KeyCode::Char('x'))),
                ..Keymap::default()
            },
            ..UiState::default()
        };
        assert!(matches!(
            handle_event(press_ctrl(KeyCode::Char('c')), &mut ui),
            Action::Quit
        ));
        // The rebound quit key works too.
        assert!(matches!(
            handle_event(press(KeyCode::Char('x')), &mut ui),
            Action::Quit
        ));
        // The old quit key is now inert.
        assert!(matches!(
            handle_event(press(KeyCode::Char('q')), &mut ui),
            Action::None
        ));
    }

    #[test]
    fn rebinding_scene_next_drives_the_new_key() {
        let mut ui = UiState {
            scene_mode: true,
            keymap: Keymap {
                scene_next: Some(KeyChord::plain(KeyCode::Char('n'))),
                ..Keymap::default()
            },
            ..UiState::default()
        };
        // The rebound key cycles forward.
        assert!(matches!(
            handle_event(press(KeyCode::Char('n')), &mut ui),
            Action::Redraw
        ));
        assert_eq!(ui.scene_nav.take_pending(), Some("lattice"));
        // The old Right key no longer cycles.
        assert!(matches!(
            handle_event(press(KeyCode::Right), &mut ui),
            Action::None
        ));
        assert_eq!(ui.scene_nav.take_pending(), None);
    }

    #[test]
    fn chrome_key_cycles_the_mode_and_raises_a_toast() {
        let mut ui = UiState::default();
        assert_eq!(ui.chrome.mode(), ChromeMode::Invisible);
        // The default chrome key `c` cycles to the next personality.
        assert!(matches!(
            handle_event(press(KeyCode::Char('c')), &mut ui),
            Action::Redraw
        ));
        assert_eq!(ui.chrome.mode(), ChromeMode::Instrument);
        let toast = ui.chrome.toast_text().expect("cycling raises a toast");
        assert!(
            toast.contains("instrument"),
            "toast names the new mode: {toast:?}"
        );
        // Cycling wraps back to invisible after all four.
        let _ = handle_event(press(KeyCode::Char('c')), &mut ui);
        let _ = handle_event(press(KeyCode::Char('c')), &mut ui);
        assert_eq!(ui.chrome.mode(), ChromeMode::Utilitarian);
        let _ = handle_event(press(KeyCode::Char('c')), &mut ui);
        assert_eq!(ui.chrome.mode(), ChromeMode::Invisible);
    }

    #[test]
    fn any_keypress_returns_the_faded_invisible_line() {
        let mut ui = UiState::default();
        // Drive the fade all the way out (as the loop would, on dt).
        ui.chrome.tick(5.0);
        assert_eq!(ui.chrome.fade(), Fade::Hidden);
        // Any key — even an unbound one that does nothing else — returns it.
        assert!(matches!(
            handle_event(press(KeyCode::Char('z')), &mut ui),
            Action::None
        ));
        assert_eq!(ui.chrome.fade(), Fade::Full, "a keypress resets the fade");
    }

    #[test]
    fn rebinding_chrome_drives_the_new_key() {
        let mut ui = UiState {
            keymap: Keymap {
                chrome: Some(KeyChord::plain(KeyCode::Char('m'))),
                ..Keymap::default()
            },
            ..UiState::default()
        };
        // The rebound key cycles chrome; the old `c` no longer does.
        let _ = handle_event(press(KeyCode::Char('m')), &mut ui);
        assert_eq!(ui.chrome.mode(), ChromeMode::Instrument);
        let _ = handle_event(press(KeyCode::Char('c')), &mut ui);
        assert_eq!(
            ui.chrome.mode(),
            ChromeMode::Instrument,
            "the old chrome key is inert after a rebind"
        );
    }

    #[test]
    fn pause_state_freezes_the_snapshot_and_zeroes_dt() {
        use scia_core::FeatureSnapshot;
        let a = FeatureSnapshot {
            generation: 1,
            ..FeatureSnapshot::default()
        };
        let b = FeatureSnapshot {
            generation: 2,
            ..FeatureSnapshot::default()
        };

        let mut ps = PauseState::default();
        // Running: the live snapshot and the real dt pass straight through.
        let (s, dt) = ps.resolve(false, a, 0.016);
        assert_eq!(s.generation, 1);
        assert_eq!(dt, 0.016);
        // Paused: the first paused snapshot is captured and dt is zeroed.
        let (s, dt) = ps.resolve(true, a, 0.016);
        assert_eq!(s.generation, 1);
        assert_eq!(dt, 0.0);
        // A newer live snapshot does not leak in while paused.
        let (s, dt) = ps.resolve(true, b, 0.033);
        assert_eq!(s.generation, 1, "the frozen snapshot is held");
        assert_eq!(dt, 0.0);
        // Resuming clears the freeze and passes the live snapshot again.
        let (s, dt) = ps.resolve(false, b, 0.02);
        assert_eq!(s.generation, 2);
        assert_eq!(dt, 0.02);
    }

    #[test]
    fn reconnecting_health_never_exits_the_loop() {
        // A transient device switch must not tear the loop down: the reaction is
        // Reconnecting, not Failed.
        let mut reconnecting = false;
        let r = health_transition(
            EngineHealth::Reconnecting {
                since_ms: 120,
                attempts: 1,
            },
            &mut reconnecting,
        );
        assert_eq!(r, HealthReaction::Reconnecting);
        assert!(reconnecting, "the reconnecting flag is now set");
        // Still reconnecting on the next frame: still not an exit.
        let r = health_transition(
            EngineHealth::Reconnecting {
                since_ms: 400,
                attempts: 3,
            },
            &mut reconnecting,
        );
        assert_eq!(r, HealthReaction::Reconnecting);
    }

    #[test]
    fn reconnect_then_recovery_shows_and_clears_the_notice() {
        // Drive Ok → Reconnecting → Ok and apply the reactions to a UiState the
        // way the loop does, checking the degraded notice appears then clears.
        let mut ui = UiState::default();
        let mut reconnecting = false;

        // Healthy: nothing shown.
        assert_eq!(
            health_transition(EngineHealth::Ok, &mut reconnecting),
            HealthReaction::Steady
        );
        assert_eq!(ui.notice, None);

        // Reconnecting: the sticky degraded notice appears.
        match health_transition(
            EngineHealth::Reconnecting {
                since_ms: 50,
                attempts: 1,
            },
            &mut reconnecting,
        ) {
            HealthReaction::Reconnecting => ui.notice = Some(RECONNECT_NOTICE.to_string()),
            other => panic!("expected Reconnecting, got {other:?}"),
        }
        assert_eq!(ui.notice.as_deref(), Some(RECONNECT_NOTICE));

        // Recovery: the "restored" notice replaces it once.
        match health_transition(EngineHealth::Ok, &mut reconnecting) {
            HealthReaction::Restored => ui.notice = Some(RESTORED_NOTICE.to_string()),
            other => panic!("expected Restored, got {other:?}"),
        }
        assert_eq!(ui.notice.as_deref(), Some(RESTORED_NOTICE));

        // Steady again afterward: the loop stops forcing a health notice, so a
        // TTL (applied by the loop) can clear the "restored" line normally.
        assert_eq!(
            health_transition(EngineHealth::Ok, &mut reconnecting),
            HealthReaction::Steady
        );
    }

    #[test]
    fn failed_health_exits_with_the_error() {
        let mut reconnecting = true;
        let r = health_transition(
            EngineHealth::Failed {
                error: "device gone".to_string(),
            },
            &mut reconnecting,
        );
        assert_eq!(r, HealthReaction::Failed("device gone".to_string()));
        assert!(!reconnecting, "failing clears the reconnecting flag");
    }

    /// A scripted input source for [`pump_input`]: `poll` reports whether an
    /// event is queued (ignoring its timeout, so the normal path spins to a real
    /// short deadline), and `read` pops the next one. Returning the queue lets a
    /// test assert how many events were consumed.
    fn scripted(events: Vec<Event>) -> std::cell::RefCell<std::collections::VecDeque<Event>> {
        std::cell::RefCell::new(events.into_iter().collect())
    }

    #[test]
    fn overrun_drain_handles_all_pending_events() {
        // The deadline is already in the past, so the normal loop breaks at once
        // and the zero-timeout drain runs. All three queued keys are handled.
        let q = scripted(vec![
            press(KeyCode::Char('z')),
            press(KeyCode::Char('z')),
            press(KeyCode::Char('z')),
        ]);
        let mut poll = |_: Duration| Ok(!q.borrow().is_empty());
        let mut read = || Ok(q.borrow_mut().pop_front().unwrap());
        let mut ui = UiState::default();
        let deadline = Instant::now() - Duration::from_millis(1);

        let outcome = pump_input(deadline, &mut poll, &mut read, &mut ui).unwrap();
        assert_eq!(outcome, PumpOutcome::Deadline);
        assert!(q.borrow().is_empty(), "the drain handled all three events");

        // A second call shows no residue: nothing left to drain.
        let outcome = pump_input(deadline, &mut poll, &mut read, &mut ui).unwrap();
        assert_eq!(outcome, PumpOutcome::Deadline);
        assert!(q.borrow().is_empty());
    }

    #[test]
    fn overrun_drain_is_capped_per_frame() {
        // More than the cap is queued: exactly MAX_OVERRUN_EVENTS are handled
        // this frame and the remainder is left for the next one — no infinite
        // drain.
        let total = MAX_OVERRUN_EVENTS + 5;
        let q = scripted(
            std::iter::repeat_with(|| press(KeyCode::Char('z')))
                .take(total)
                .collect(),
        );
        let mut poll = |_: Duration| Ok(!q.borrow().is_empty());
        let mut read = || Ok(q.borrow_mut().pop_front().unwrap());
        let mut ui = UiState::default();
        let deadline = Instant::now() - Duration::from_millis(1);

        let outcome = pump_input(deadline, &mut poll, &mut read, &mut ui).unwrap();
        assert_eq!(outcome, PumpOutcome::Deadline);
        assert_eq!(
            q.borrow().len(),
            5,
            "exactly the cap was handled; the remainder waits for the next frame"
        );

        // The next frame drains what was left.
        let outcome = pump_input(deadline, &mut poll, &mut read, &mut ui).unwrap();
        assert_eq!(outcome, PumpOutcome::Deadline);
        assert!(q.borrow().is_empty(), "the remainder drained next frame");
    }

    #[test]
    fn quit_mid_drain_returns_immediately() {
        // A Quit during the drain returns at once, leaving the events after it
        // unconsumed.
        let q = scripted(vec![
            press(KeyCode::Char('z')),
            press_ctrl(KeyCode::Char('c')),
            press(KeyCode::Char('z')),
            press(KeyCode::Char('z')),
        ]);
        let mut poll = |_: Duration| Ok(!q.borrow().is_empty());
        let mut read = || Ok(q.borrow_mut().pop_front().unwrap());
        let mut ui = UiState::default();
        let deadline = Instant::now() - Duration::from_millis(1);

        let outcome = pump_input(deadline, &mut poll, &mut read, &mut ui).unwrap();
        assert_eq!(outcome, PumpOutcome::Quit);
        assert_eq!(
            q.borrow().len(),
            2,
            "the two events after the quit are left unconsumed"
        );
    }

    #[test]
    fn redraw_mid_drain_finishes_the_drain() {
        // A redraw during the drain does not short-circuit: the buffered key
        // after it is still handled this frame, and the outcome is Redraw.
        let q = scripted(vec![
            press(KeyCode::Char('z')),
            press(KeyCode::Char('?')), // toggles help → Action::Redraw
            press(KeyCode::Char('z')),
        ]);
        let mut poll = |_: Duration| Ok(!q.borrow().is_empty());
        let mut read = || Ok(q.borrow_mut().pop_front().unwrap());
        let mut ui = UiState::default();
        let deadline = Instant::now() - Duration::from_millis(1);

        let outcome = pump_input(deadline, &mut poll, &mut read, &mut ui).unwrap();
        assert_eq!(outcome, PumpOutcome::Redraw);
        assert!(q.borrow().is_empty(), "the drain ran on despite the redraw");
        assert!(ui.help, "the help toggle was applied mid-drain");
    }

    #[test]
    fn normal_path_handles_events_then_exits_at_deadline() {
        // Time remains: the queued key is handled, then with the poll closure
        // reporting nothing the loop runs to the (short, real) deadline and
        // exits with Deadline.
        let q = scripted(vec![press(KeyCode::Char('z'))]);
        let mut poll = |_: Duration| Ok(!q.borrow().is_empty());
        let mut read = || Ok(q.borrow_mut().pop_front().unwrap());
        let mut ui = UiState::default();
        let deadline = Instant::now() + Duration::from_millis(5);

        let outcome = pump_input(deadline, &mut poll, &mut read, &mut ui).unwrap();
        assert_eq!(outcome, PumpOutcome::Deadline);
        assert!(q.borrow().is_empty(), "the queued event was handled");
    }

    #[test]
    fn normal_path_redraw_returns_before_the_deadline() {
        // A redraw-triggering key returns Redraw promptly, without finishing out
        // the (here, far-off) sleep.
        let q = scripted(vec![press(KeyCode::Char('?'))]);
        let mut poll = |_: Duration| Ok(!q.borrow().is_empty());
        let mut read = || Ok(q.borrow_mut().pop_front().unwrap());
        let mut ui = UiState::default();
        let deadline = Instant::now() + Duration::from_secs(10);

        let outcome = pump_input(deadline, &mut poll, &mut read, &mut ui).unwrap();
        assert_eq!(outcome, PumpOutcome::Redraw);
        assert!(ui.help, "the help toggle was applied");
    }
}
