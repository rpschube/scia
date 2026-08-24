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

mod keymap;
mod mosaic;
mod pacing;
mod palette;
mod presenter;
mod probe;
mod render;
mod stats;

use std::fmt;
use std::io::{self, Stdout};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use scia_core::{Activity, EngineStats, FeatureReader, FeatureSnapshot, StreamHealth};
use scia_scenes::{Preset, ReloadEvent, builtin_preset, builtin_scenes};

pub use keymap::{ChordParseError, InputAction, KeyChord, Keymap, parse_chord};
pub use mosaic::{Cell, CellGrid, FrameBuffer, TextRun, Tier};
pub use presenter::{SceneError, ScenePresenter, build_scene_presenter};
pub use probe::{
    CapabilityReport, Da1, SyncSupport, TermFamily, classify_family, default_tier, parse_cell_size,
    parse_da1, parse_decrqm_2026, probe, truecolor_from,
};
pub use render::{SceneNav, UiState, VERSION, draw, draw_help, draw_notice};

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
    /// The mosaic tier to render a scene at. `None` means the caller did not
    /// force one; [`run`] then falls back to [`Tier::default`]. Ignored when
    /// neither [`scene`](Self::scene) nor [`preset`](Self::preset) is set.
    pub tier: Option<Tier>,
    /// The active key bindings, built at startup from the built-in defaults plus
    /// any config overrides. The default is the built-in binding set.
    pub keymap: Keymap,
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
            tier: None,
            keymap: Keymap::default(),
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
/// `health` is polled once per frame — when it reports
/// [`StreamHealth::Errored`] the loop leaves the terminal cleanly and returns a
/// [`RunSummary`] whose [`error`](RunSummary::error) carries the message; and
/// `clock` is the engine's snapshot clock (monotonic ns since the ring epoch),
/// sampled once per frame so the overlay can show the newest feature's age as
/// `clock() - snap.timestamp_ns`.
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
    health: impl FnMut() -> StreamHealth,
    clock: impl FnMut() -> u64,
    reload: Option<Receiver<ReloadEvent>>,
    opts: TuiOptions,
) -> Result<RunSummary, RunError> {
    install_panic_hook();
    // Build the scene presenter first: a bad preset must fail with the terminal
    // still in its normal state. A disk preset (`--scene-file`) is already
    // validated by the caller and takes precedence over a built-in name.
    let presenter = match (&opts.preset, &opts.scene) {
        (Some(preset), _) => Some(ScenePresenter::from_preset(
            preset,
            opts.tier.unwrap_or_default(),
        )),
        (None, Some(name)) => Some(build_scene_presenter(name, opts.tier.unwrap_or_default())?),
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
    mut health: impl FnMut() -> StreamHealth,
    mut clock: impl FnMut() -> u64,
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
        .and_then(|name| builtin_scenes().iter().position(|s| s.id == name))
        .unwrap_or(0);
    let mut ui = UiState {
        label: opts.label.clone(),
        source: opts.source.clone(),
        debug: opts.debug,
        overlay: opts.overlay,
        fps_measured: opts.fps as f32,
        // A scene presenter surfaces its ladder rung on the debug line; the
        // direct-bars renderer leaves it unset.
        tier: presenter.as_ref().map(|p| p.tier().label()),
        scene_mode,
        scene_nav: SceneNav::new(initial_scene),
        keymap: opts.keymap,
        ..UiState::default()
    };
    // Frame period fed to the scene presenter; seeded to the target period.
    let default_dt = 1.0 / opts.fps.max(1) as f32;
    // Holds the frozen snapshot while paused, so a paused scene renders an
    // identical frame every tick.
    let mut pause_state = PauseState::default();

    let mut frames: u64 = 0;
    // When the current continuous starvation began, if any.
    let mut starved_since: Option<Instant> = None;
    // Start of the previous frame, for the measured-fps EMA.
    let mut prev_frame_start: Option<Instant> = None;
    let mut fps_ema = opts.fps as f32;
    // The reload status notice auto-clears this long after the last event.
    const NOTICE_TTL: Duration = Duration::from_secs(3);
    // Deadline at which the current notice is cleared, tracked in-loop (no
    // background timer).
    let mut notice_deadline: Option<Instant> = None;

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
                if let Some(Ok(preset)) = builtin_preset(id) {
                    p.swap_preset(&preset);
                }
            }
        }
        ui.scene_nav.tick(scene_dt);

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
                    match event.result {
                        Ok(preset) => {
                            p.swap_preset(&preset);
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
            }
        }
        // Auto-clear the notice once its deadline passes.
        if let Some(deadline) = notice_deadline {
            if frame_start >= deadline {
                ui.notice = None;
                notice_deadline = None;
            }
        }

        // Abort cleanly if the capture stream faulted. The guard restores the
        // terminal on return; the caller reports the message.
        if let StreamHealth::Errored(msg) = health() {
            let (p50, p99) = frame_times.percentiles();
            return Ok(RunSummary {
                frames,
                p50_frame_ms: p50,
                p99_frame_ms: p99,
                error: Some(msg),
            });
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
                terminal.draw(|frame| {
                    if let Some(body) = render::draw_chrome(frame, &snap, &ui) {
                        p.resize(body.width, body.height);
                        p.frame(&snap, scene_dt);
                        p.draw(frame.buffer_mut(), body);
                        // The overlay is drawn last, over the rasterized scene.
                        if ui.overlay {
                            render::render_overlay(frame.buffer_mut(), body, &snap, &ui);
                        }
                        // The browser panel and cycle toast paint over the live
                        // scene, like the meter bridge, so they draw after it.
                        render::draw_scene_nav(frame.buffer_mut(), body, &ui.scene_nav);
                        // The help overlay is the topmost body layer.
                        render::draw_help(frame.buffer_mut(), body, &ui);
                    }
                    // The reload notice lands on top of the scene body, so it
                    // draws after the presenter rather than inside the chrome.
                    let area = frame.area();
                    render::draw_notice(frame.buffer_mut(), area, &ui);
                })?;
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
        // keys are handled immediately. An overrunning frame skips straight to
        // the next frame rather than queuing.
        let deadline = frame_start + interval;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining == Duration::ZERO {
                break;
            }
            if event::poll(remaining)? {
                match handle_event(event::read()?, &mut ui) {
                    Action::Quit => {
                        let (p50, p99) = frame_times.percentiles();
                        return Ok(RunSummary {
                            frames,
                            p50_frame_ms: p50,
                            p99_frame_ms: p99,
                            error: None,
                        });
                    }
                    // A resize or debug toggle: redraw promptly on the next
                    // frame rather than finishing out the sleep.
                    Action::Redraw => break,
                    Action::None => {}
                }
            }
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

/// What an input event asks the loop to do.
enum Action {
    /// Nothing of interest.
    None,
    /// Redraw as soon as possible (resize, or a state toggle).
    Redraw,
    /// Quit the loop.
    Quit,
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
/// Ctrl-C's always-on quit, the `d` debug-line toggle and the `?` help overlay
/// are structural and stay hard-coded.
fn handle_event(event: Event, ui: &mut UiState) -> Action {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            let browsing = ui.scene_mode && ui.scene_nav.is_open();

            // Ctrl-C is a structural, always-on quit, independent of the keymap.
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return Action::Quit;
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
            if key.code == KeyCode::Char('d') {
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
        // Reserved: the now-playing panel has not landed, so a bound key is a
        // no-op for now. The browser/cycle guards fall through here too.
        InputAction::NowPlaying
        | InputAction::Browser
        | InputAction::SceneNext
        | InputAction::ScenePrev => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

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
        // The debug-line key keeps its meaning and does not touch the overlay.
        let _ = handle_event(press(KeyCode::Char('d')), &mut ui);
        assert!(ui.debug);
        assert!(!ui.overlay);
        // The overlay key does not touch the debug line.
        let _ = handle_event(press(KeyCode::Char('`')), &mut ui);
        assert!(ui.debug);
        assert!(ui.overlay);
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
        let mut ui = scene_ui();
        for expected in ["lattice", "aurora", "starfall", "spectra"] {
            assert!(matches!(
                handle_event(press(KeyCode::Right), &mut ui),
                Action::Redraw
            ));
            assert_eq!(ui.scene_nav.take_pending(), Some(expected));
        }
        // Left wraps backward from spectra to the last scene.
        assert!(matches!(
            handle_event(press(KeyCode::Left), &mut ui),
            Action::Redraw
        ));
        assert_eq!(ui.scene_nav.take_pending(), Some("starfall"));
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
}
