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

mod mosaic;
mod pacing;
mod palette;
mod presenter;
mod probe;
mod render;
mod stats;

use std::fmt;
use std::io::{self, Stdout};
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

use scia_core::{Activity, EngineStats, FeatureReader, StreamHealth};

pub use mosaic::{Cell, CellGrid, FrameBuffer, TextRun, Tier};
pub use presenter::{SceneError, ScenePresenter, build_scene_presenter};
pub use probe::{
    CapabilityReport, Da1, SyncSupport, TermFamily, classify_family, default_tier, parse_cell_size,
    parse_da1, parse_decrqm_2026, probe, truecolor_from,
};
pub use render::{UiState, VERSION, draw};

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
    /// Built-in scene preset to render, by name. `None` runs the direct
    /// spectrum-bar renderer (the byte-identical legacy path); `Some(name)`
    /// drives the [`ScenePresenter`] on the selected [`tier`](Self::tier).
    pub scene: Option<String>,
    /// The mosaic tier to render a scene at. `None` means the caller did not
    /// force one; [`run`] then falls back to [`Tier::default`]. Ignored when
    /// [`scene`](Self::scene) is `None`.
    pub tier: Option<Tier>,
}

impl Default for TuiOptions {
    fn default() -> Self {
        Self {
            fps: 60,
            label: None,
            source: String::new(),
            frames: None,
            debug: false,
            scene: None,
            tier: None,
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
/// called once per frame for the engine counters shown on the debug line, and
/// `health` is polled once per frame — when it reports
/// [`StreamHealth::Errored`] the loop leaves the terminal cleanly and returns a
/// [`RunSummary`] whose [`error`](RunSummary::error) carries the message.
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
    opts: TuiOptions,
) -> Result<RunSummary, RunError> {
    install_panic_hook();
    // Build the scene presenter first: a bad preset must fail with the terminal
    // still in its normal state.
    let presenter = match &opts.scene {
        Some(name) => Some(build_scene_presenter(name, opts.tier.unwrap_or_default())?),
        None => None,
    };
    let mut guard = TerminalGuard::enter()?;
    // The guard restores the terminal on every exit path, including `?`.
    Ok(run_loop(
        &mut guard.terminal,
        reader,
        stats,
        health,
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
fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut reader: FeatureReader,
    mut stats: impl FnMut() -> EngineStats,
    mut health: impl FnMut() -> StreamHealth,
    opts: &TuiOptions,
    mut presenter: Option<ScenePresenter>,
) -> io::Result<RunSummary> {
    let mut frame_times = stats::FrameTimes::new();
    let mut ui = UiState {
        label: opts.label.clone(),
        source: opts.source.clone(),
        debug: opts.debug,
        fps_measured: opts.fps as f32,
        // A scene presenter surfaces its ladder rung on the debug line; the
        // direct-bars renderer leaves it unset.
        tier: presenter.as_ref().map(|p| p.tier().label()),
        ..UiState::default()
    };
    // Frame period fed to the scene presenter; seeded to the target period.
    let default_dt = 1.0 / opts.fps.max(1) as f32;

    let mut frames: u64 = 0;
    // When the current continuous starvation began, if any.
    let mut starved_since: Option<Instant> = None;
    // Start of the previous frame, for the measured-fps EMA.
    let mut prev_frame_start: Option<Instant> = None;
    let mut fps_ema = opts.fps as f32;

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

        let snap = *reader.latest();

        // Refresh the engine counters first: activity feeds the idle downshift.
        ui.stats = stats();
        ui.fps_measured = fps_ema;

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
                        p.frame(&snap, dt);
                        p.draw(frame.buffer_mut(), body);
                    }
                })?;
            }
            // Direct-bars path: byte-identical to before.
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

/// Translate one input event into a loop [`Action`], mutating [`UiState`] for
/// toggles.
fn handle_event(event: Event, ui: &mut UiState) -> Action {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
            KeyCode::Char('d') => {
                ui.debug = !ui.debug;
                Action::Redraw
            }
            _ => Action::None,
        },
        Event::Resize(_, _) => Action::Redraw,
        _ => Action::None,
    }
}
