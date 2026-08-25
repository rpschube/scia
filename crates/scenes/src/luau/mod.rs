//! Sandboxed Luau scenes (US-CFG-4): a `.lua` file that binds to the same
//! [`Scene`] trait as a built-in.
//!
//! A scene script is a Luau chunk that returns a **manifest table**:
//!
//! ```lua
//! return {
//!   id = "ripple",              -- stable machine id, ^[a-z0-9][a-z0-9-]*$
//!   mood = "serene",            -- one word, shown in the scene browser
//!   summary = "one-line …",     -- one line, shown in the scene browser
//!   params = {                  -- optional tuning manifest (see ParamSpec)
//!     { key = "gain", default = 1.0, min = 0.0, max = 4.0, doc = "…" },
//!   },
//!   init = function(ctx) end,   -- optional; ctx = { aspect, params }
//!   update = function(features, dt) end,  -- required
//!   render = function(canvas) end,        -- required
//!   state = function() return { k = 1.0 } end,   -- optional continuity out
//!   restore = function(t) end,                    -- optional continuity in
//! }
//! ```
//!
//! The functions close over the chunk's own locals, so per-instance state lives
//! in Lua upvalues — each [`LuauScene`] owns its own VM, so those locals are
//! private to the instance. See `crates/scenes/src/luau/scenes/*.lua` for the
//! two shipped, heavily-commented scenes.
//!
//! # Sandbox
//!
//! The VM is built with only the safe standard libraries (`math`, `string`,
//! `table`, `bit32`, `utf8`, `coroutine`) — no `os`, `io` or `package` — and the
//! remaining dynamic-code base functions (`load`, `loadstring`, `require`,
//! `dofile`, `loadfile`, `getfenv`, `setfenv`, `newproxy`, `collectgarbage`) are
//! niled before [`Lua::sandbox`] freezes the globals read-only. Three host
//! limits bound a tick: a per-VM **memory cap** (`Error::MemoryError` on
//! overrun), an **instruction interrupt** enforcing a per-tick **deadline**
//! (checked ~1 in 1024 safepoints, the cheap variant measured in probe P5), and
//! Luau's built-in **C-call depth** limit (deep recursion raises an error, never
//! a crash). Any of these surfaces as an `Err` that the scene catches: it latches
//! into an error state, reports the message, and holds its **last good frame** —
//! the same guarantee hot reload gives (US-CFG-2). The host is never panicked
//! and the canvas is never blanked.

mod bridge;
pub mod catalog;
mod discover;
mod watch;

use std::cell::Cell;
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use mlua::{AnyUserData, Function, Lua, LuaOptions, StdLib, Table, VmState};

use scia_core::{FeatureSnapshot, SPECTRUM_BINS};

use crate::canvas::Canvas;
use crate::palette::Palette;
use crate::registry::SceneInfo;
use crate::scene::{ParamSpec, Scene, SceneCtx, SceneState};

use bridge::{CanvasUd, FeaturesUd, write_params};

pub use catalog::{
    catalog_scene_info, catalog_scenes, create_scene, is_luau_scene, luau_scene_ids,
    luau_scene_path, scene_preset,
};
pub use discover::{DEFAULT_SCENES_SUBDIR, scenes_dir, shipped_scenes};
pub use watch::{LuauReloadEvent, LuauSource, LuauWatcher};

/// Base functions that permit dynamic code loading or environment escape. They
/// are set to `nil` before the sandbox freezes the globals, so a script can
/// neither load new code nor reach outside its VM.
const BANNED_GLOBALS: &[&str] = &[
    "load",
    "loadstring",
    "require",
    "dofile",
    "loadfile",
    "getfenv",
    "setfenv",
    "newproxy",
    "collectgarbage",
];

/// The host-enforced limits on a scripted tick.
#[derive(Clone, Copy, Debug)]
pub struct LuauLimits {
    /// Per-VM memory cap in bytes. An allocation that would cross it fails the
    /// tick rather than growing without bound.
    pub memory_bytes: usize,
    /// Per-tick wall-clock deadline. A tick (`update` or `render`) that runs
    /// past it is interrupted and reported.
    pub tick_budget: Duration,
}

impl Default for LuauLimits {
    fn default() -> Self {
        Self {
            // Generous headroom for a busy particle scene, small enough that a
            // runaway allocation trips promptly.
            memory_bytes: 64 * 1024 * 1024,
            // A safety cap on a single tick, far above a real tick (which is
            // microseconds — see the budget test): a runaway is interrupted
            // within this window, costing at most one dropped frame, never a
            // hang. It is deliberately above the frame budget because it bounds
            // faults, not the normal path.
            tick_budget: Duration::from_millis(50),
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// What went wrong building or driving a Luau scene.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LuauErrorKind {
    /// The file could not be read.
    Io(String),
    /// The chunk failed to compile or evaluate.
    Compile(String),
    /// The returned value was not a well-formed manifest table.
    Manifest(String),
    /// A runtime error while running `init`, `update` or `render` (a deadline
    /// trip, a memory-cap overrun, or a script fault).
    Runtime(String),
}

impl fmt::Display for LuauErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(m) | Self::Compile(m) | Self::Manifest(m) | Self::Runtime(m) => f.write_str(m),
        }
    }
}

/// An error building or driving a Luau scene, naming the source file when known.
/// Its [`Display`](fmt::Display) mirrors [`crate::PresetError`]: `<file or
/// "\<luau\>">: <message>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LuauError {
    /// The file the scene was read from, if any.
    pub file: Option<PathBuf>,
    /// What went wrong.
    pub kind: LuauErrorKind,
}

impl LuauError {
    fn compile(msg: impl Into<String>) -> Self {
        Self {
            file: None,
            kind: LuauErrorKind::Compile(msg.into()),
        }
    }

    fn manifest(msg: impl Into<String>) -> Self {
        Self {
            file: None,
            kind: LuauErrorKind::Manifest(msg.into()),
        }
    }

    /// Attach a source file to an error that was raised without one.
    #[must_use]
    fn with_file(mut self, file: &Path) -> Self {
        self.file = Some(file.to_path_buf());
        self
    }
}

impl fmt::Display for LuauError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.file {
            Some(p) => write!(f, "{}: {}", p.display(), self.kind),
            None => write!(f, "<luau>: {}", self.kind),
        }
    }
}

impl std::error::Error for LuauError {}

/// Reduce an [`mlua::Error`] to a single-line message. The chunk name is set to
/// the scene id (never a filesystem path), so a Luau traceback carries no host
/// path — a public-repo privacy guard as much as a readability one.
fn one_line(err: &mlua::Error) -> String {
    err.to_string()
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("error")
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// One tunable parameter parsed from a manifest `params` entry (owned form).
#[derive(Clone, Debug, PartialEq)]
struct OwnedParam {
    key: String,
    default: f32,
    min: f32,
    max: f32,
    doc: String,
}

/// A validated scene manifest: everything the catalog needs to list a scene in
/// the browser, without keeping the VM that produced it alive.
#[derive(Clone, Debug, PartialEq)]
pub struct LuauManifest {
    /// The scene's stable machine id.
    pub id: String,
    /// The one-word mood.
    pub mood: String,
    /// The one-line summary.
    pub summary: String,
    /// The parameter manifest (owned).
    params: Vec<OwnedParam>,
}

impl LuauManifest {
    /// Leak this manifest into a `'static` [`SceneInfo`] for the catalog listing.
    ///
    /// A scene id is stable for the life of the process (the catalog leaks it
    /// exactly once at discovery; a hot reload of the same file keeps the same
    /// id and reuses this `SceneInfo`), so a bounded, one-time leak is the right
    /// trade for a `SceneInfo` whose `&'static str` fields the whole browser and
    /// tuning path already assume.
    fn leak_info(&self) -> &'static SceneInfo {
        let params: Vec<ParamSpec> = self
            .params
            .iter()
            .map(|p| ParamSpec {
                key: Box::leak(p.key.clone().into_boxed_str()),
                default: p.default,
                min: p.min,
                max: p.max,
                doc: Box::leak(p.doc.clone().into_boxed_str()),
            })
            .collect();
        let params: &'static [ParamSpec] = Box::leak(params.into_boxed_slice());
        Box::leak(Box::new(SceneInfo {
            id: Box::leak(self.id.clone().into_boxed_str()),
            mood: Box::leak(self.mood.clone().into_boxed_str()),
            summary: Box::leak(self.summary.clone().into_boxed_str()),
            params,
        }))
    }
}

/// Whether `s` is a valid scene id: `^[a-z0-9][a-z0-9-]*$`.
fn valid_id(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Build a fresh sandboxed VM with the safe library set and frozen globals.
fn sandboxed_vm(limits: LuauLimits) -> Result<Lua, LuauError> {
    // Only the safe, side-effect-free libraries: no `os`, `io` or `package`.
    let libs = StdLib::MATH
        | StdLib::STRING
        | StdLib::TABLE
        | StdLib::BIT
        | StdLib::UTF8
        | StdLib::COROUTINE;
    let lua = Lua::new_with(libs, LuaOptions::default())
        .map_err(|e| LuauError::compile(format!("cannot create VM: {}", one_line(&e))))?;

    // Nil the dynamic-code / environment-escape base functions before the
    // sandbox freezes the globals read-only.
    {
        let globals = lua.globals();
        for name in BANNED_GLOBALS {
            globals
                .set(*name, mlua::Value::Nil)
                .map_err(|e| LuauError::compile(one_line(&e)))?;
        }
    }

    lua.set_memory_limit(limits.memory_bytes)
        .map_err(|e| LuauError::compile(format!("cannot set memory limit: {}", one_line(&e))))?;
    lua.sandbox(true)
        .map_err(|e| LuauError::compile(format!("cannot sandbox VM: {}", one_line(&e))))?;
    Ok(lua)
}

/// Evaluate `source` in a sandboxed VM and return its manifest table (validated).
///
/// The VM is thrown away after the manifest is read — this is the cheap
/// discovery path that lists a scene without keeping a live VM per candidate.
///
/// # Errors
/// [`LuauError`] if the chunk fails to compile/evaluate, does not return a
/// table, or the table is not a well-formed manifest.
pub fn compile_manifest(source: &str, name: &str) -> Result<LuauManifest, LuauError> {
    let lua = sandboxed_vm(LuauLimits::default())?;
    let chunk_name = format!("={name}");
    let table: Table = lua
        .load(source)
        .set_name(chunk_name)
        .eval()
        .map_err(|e| LuauError::compile(one_line(&e)))?;
    read_manifest(&table)
}

/// Read and validate a manifest table.
fn read_manifest(table: &Table) -> Result<LuauManifest, LuauError> {
    let id: String = table
        .get("id")
        .map_err(|_| LuauError::manifest("manifest is missing a string `id`"))?;
    if !valid_id(&id) {
        return Err(LuauError::manifest(format!(
            "invalid scene id `{id}`; must match ^[a-z0-9][a-z0-9-]*$"
        )));
    }
    let mood: String = table
        .get("mood")
        .map_err(|_| LuauError::manifest("manifest is missing a string `mood`"))?;
    let summary: String = table
        .get("summary")
        .map_err(|_| LuauError::manifest("manifest is missing a string `summary`"))?;

    // update / render must be functions.
    if !matches!(
        table.get::<mlua::Value>("update"),
        Ok(mlua::Value::Function(_))
    ) {
        return Err(LuauError::manifest("manifest `update` must be a function"));
    }
    if !matches!(
        table.get::<mlua::Value>("render"),
        Ok(mlua::Value::Function(_))
    ) {
        return Err(LuauError::manifest("manifest `render` must be a function"));
    }

    let params = read_params(table)?;
    Ok(LuauManifest {
        id,
        mood,
        summary,
        params,
    })
}

/// Read and validate the optional `params` manifest array.
fn read_params(table: &Table) -> Result<Vec<OwnedParam>, LuauError> {
    let raw: mlua::Value = table
        .get("params")
        .map_err(|e| LuauError::manifest(one_line(&e)))?;
    let list = match raw {
        mlua::Value::Nil => return Ok(Vec::new()),
        mlua::Value::Table(t) => t,
        _ => return Err(LuauError::manifest("manifest `params` must be an array")),
    };
    let mut out = Vec::new();
    for (i, entry) in list.sequence_values::<Table>().enumerate() {
        let entry = entry.map_err(|e| LuauError::manifest(one_line(&e)))?;
        let key: String = entry.get("key").map_err(|_| {
            LuauError::manifest(format!("params[{}] is missing a string `key`", i + 1))
        })?;
        let default: f32 = entry.get("default").unwrap_or(0.0);
        let min: f32 = entry.get("min").unwrap_or(0.0);
        let max: f32 = entry.get("max").unwrap_or(1.0);
        let doc: String = entry.get("doc").unwrap_or_default();
        if min.is_nan() || max.is_nan() || min > max {
            return Err(LuauError::manifest(format!(
                "param `{key}`: invalid range min {min}, max {max}"
            )));
        }
        out.push(OwnedParam {
            key,
            default: default.clamp(min, max),
            min,
            max,
            doc,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// The live scene
// ---------------------------------------------------------------------------

/// A live, sandboxed Luau scene. Built from a source chunk plus the `'static`
/// [`SceneInfo`] the catalog leaked for it; drives the script's `update` and
/// `render` each frame, and holds the last good frame when a tick faults.
pub struct LuauScene {
    lua: Lua,
    update_fn: Function,
    render_fn: Function,
    init_fn: Option<Function>,
    state_fn: Option<Function>,
    restore_fn: Option<Function>,
    /// The newest features, shared with the `features` userdata.
    features_rc: Rc<std::cell::RefCell<FeatureSnapshot>>,
    features_ud: AnyUserData,
    /// The in-place spectrum table handed to the script as `ctx.bars`.
    bars: Table,
    /// The in-place tuning table handed to the script as `ctx.params`.
    params_table: Table,
    /// The scratch canvas the `canvas` userdata draws into; swapped with the
    /// host canvas around each `render` so the script draws straight into it.
    canvas_rc: Rc<std::cell::RefCell<Canvas>>,
    canvas_ud: AnyUserData,
    /// Shared per-tick deadline the interrupt reads; set before each call.
    deadline: Rc<Cell<Instant>>,
    budget: Duration,
    info: &'static SceneInfo,
    /// The most recent successfully rendered frame; served while errored.
    last_good: Canvas,
    /// Latched on the first fault: the scene freezes and holds `last_good`.
    errored: bool,
    /// The message from the fault that latched [`errored`](Self::errored).
    last_error: Option<String>,
}

impl LuauScene {
    /// Compile `source` into a live scene bound to `info`, applying `limits`.
    ///
    /// The scene is created but not yet initialized — call [`Scene::init`]
    /// (which the host does) before driving it. `info` must describe this same
    /// source (its id/mood/params); the catalog guarantees that.
    ///
    /// # Errors
    /// [`LuauError`] if the chunk fails to compile/evaluate or is not a
    /// well-formed manifest.
    pub fn compile(
        source: &str,
        info: &'static SceneInfo,
        limits: LuauLimits,
    ) -> Result<Self, LuauError> {
        let lua = sandboxed_vm(limits)?;

        // The interrupt: a counted deadline check. The clock is read only every
        // 1024th safepoint (probe P5's cheap variant); between checks the closure
        // is a counter bump. The deadline is a shared cell the host sets before
        // each call.
        let deadline = Rc::new(Cell::new(Instant::now()));
        let dl = Rc::clone(&deadline);
        let counter = Cell::new(0u32);
        lua.set_interrupt(move |_| {
            let c = counter.get().wrapping_add(1);
            counter.set(c);
            if c % 1024 == 0 && Instant::now() >= dl.get() {
                return Err(mlua::Error::runtime("scene tick exceeded its deadline"));
            }
            Ok(VmState::Continue)
        });

        let chunk_name = format!("={}", info.id);
        let table: Table = lua
            .load(source)
            .set_name(chunk_name)
            .eval()
            .map_err(|e| LuauError::compile(one_line(&e)))?;
        // Re-validate against this VM (the catalog validated in a throwaway VM).
        read_manifest(&table)?;

        let update_fn: Function = table
            .get("update")
            .map_err(|e| LuauError::manifest(one_line(&e)))?;
        let render_fn: Function = table
            .get("render")
            .map_err(|e| LuauError::manifest(one_line(&e)))?;
        let init_fn: Option<Function> = table.get("init").ok();
        let state_fn: Option<Function> = table.get("state").ok();
        let restore_fn: Option<Function> = table.get("restore").ok();

        // Feature bridge: the shared snapshot + its userdata + the in-place bars.
        let features_rc = Rc::new(std::cell::RefCell::new(FeatureSnapshot::default()));
        let features_ud = lua
            .create_userdata(FeaturesUd(Rc::clone(&features_rc)))
            .map_err(|e| LuauError::compile(one_line(&e)))?;
        let bars = lua
            .create_table_with_capacity(SPECTRUM_BINS, 1)
            .map_err(|e| LuauError::compile(one_line(&e)))?;
        for i in 1..=SPECTRUM_BINS {
            bars.set(i as i64, 0.0f32)
                .map_err(|e| LuauError::compile(one_line(&e)))?;
        }

        // Canvas bridge: the scratch canvas + its userdata.
        let canvas_rc = Rc::new(std::cell::RefCell::new(Canvas::new(1.0)));
        let canvas_ud = lua
            .create_userdata(CanvasUd::new(Rc::clone(&canvas_rc)))
            .map_err(|e| LuauError::compile(one_line(&e)))?;

        // Tuning table (populated at init from the ctx params).
        let params_table = lua
            .create_table()
            .map_err(|e| LuauError::compile(one_line(&e)))?;

        Ok(Self {
            lua,
            update_fn,
            render_fn,
            init_fn,
            state_fn,
            restore_fn,
            features_rc,
            features_ud,
            bars,
            params_table,
            canvas_rc,
            canvas_ud,
            deadline,
            budget: limits.tick_budget,
            info,
            last_good: Canvas::new(1.0),
            errored: false,
            last_error: None,
        })
    }

    /// Compile a scene from `source`, leaking a fresh `'static` [`SceneInfo`]
    /// from its manifest. Convenient for tests and one-off scenes; production
    /// discovery leaks the `SceneInfo` once and calls [`compile`](Self::compile).
    ///
    /// # Errors
    /// [`LuauError`] if the source fails to compile or validate.
    pub fn from_source(source: &str, name: &str, limits: LuauLimits) -> Result<Self, LuauError> {
        let manifest = compile_manifest(source, name)?;
        let info = manifest.leak_info();
        Self::compile(source, info, limits)
    }

    /// The last fault message, if the scene has latched into an error state.
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Whether the scene has faulted and is now holding its last good frame.
    #[must_use]
    pub fn is_errored(&self) -> bool {
        self.errored
    }

    /// Latch a fault: freeze the scene and remember the message.
    fn fail(&mut self, err: &mlua::Error) {
        self.errored = true;
        self.last_error = Some(one_line(err));
    }

    /// Rewrite the shared feature snapshot and the in-place bars table.
    fn refresh_features(&self, f: &FeatureSnapshot) -> mlua::Result<()> {
        *self.features_rc.borrow_mut() = *f;
        let n = (f.spectrum_len as usize).min(SPECTRUM_BINS);
        for i in 0..n {
            self.bars.set((i + 1) as i64, f.spectrum[i])?;
        }
        self.bars.set("n", n as i64)?;
        Ok(())
    }

    /// Arm the deadline for the call about to run.
    fn arm(&self) {
        self.deadline.set(Instant::now() + self.budget);
    }
}

impl Scene for LuauScene {
    fn id(&self) -> &'static str {
        self.info.id
    }

    fn mood(&self) -> &'static str {
        self.info.mood
    }

    fn init(&mut self, ctx: &SceneCtx) {
        self.canvas_rc.borrow_mut().set_aspect(ctx.aspect);
        // Seed the tuning table from the manifest defaults / preset params.
        if let Err(e) = write_params(&self.params_table, &ctx.params, self.info.params) {
            self.fail(&e);
            return;
        }
        if let Some(init) = &self.init_fn {
            // Hand the script a ctx table: { aspect, params }.
            let ctx_tbl = match self.lua.create_table() {
                Ok(t) => t,
                Err(e) => {
                    self.fail(&e);
                    return;
                }
            };
            let ok = ctx_tbl.set("aspect", ctx.aspect).and_then(|()| {
                ctx_tbl.set("params", self.params_table.clone())?;
                ctx_tbl.set("bars", self.bars.clone())
            });
            if let Err(e) = ok {
                self.fail(&e);
                return;
            }
            self.arm();
            if let Err(e) = init.call::<()>(ctx_tbl) {
                self.fail(&e);
                return;
            }
        }
        // Prime the last good frame with one render so a first-frame fault (or a
        // paused first frame) still has a non-blank hold. Uses a default-feature
        // update first so a render that reads features sees sane zeros.
        let snap = FeatureSnapshot::default();
        self.update(&snap, 0.0);
        let mut prime = Canvas::new(ctx.aspect);
        self.render(&mut prime);
    }

    fn apply_params(&mut self, params: &crate::scene::Params) {
        if self.errored {
            return;
        }
        if let Err(e) = write_params(&self.params_table, params, self.info.params) {
            self.fail(&e);
        }
    }

    fn update(&mut self, f: &FeatureSnapshot, dt: f32) {
        if self.errored {
            return;
        }
        if let Err(e) = self.refresh_features(f) {
            self.fail(&e);
            return;
        }
        self.arm();
        if let Err(e) = self.update_fn.call::<()>((self.features_ud.clone(), dt)) {
            self.fail(&e);
        }
    }

    fn render(&mut self, out: &mut Canvas) {
        if self.errored {
            out.copy_from(&self.last_good);
            return;
        }
        // Move the host canvas (cleared, aspect set) into the shared cell so the
        // script draws straight into it, then move it back — an O(1) swap, no
        // per-frame copy of the display list.
        std::mem::swap(out, &mut self.canvas_rc.borrow_mut());
        self.arm();
        let res = self.render_fn.call::<()>(self.canvas_ud.clone());
        std::mem::swap(out, &mut self.canvas_rc.borrow_mut());
        match res {
            Ok(()) => self.last_good.copy_from(out),
            Err(e) => {
                self.fail(&e);
                out.copy_from(&self.last_good);
            }
        }
    }

    fn state(&self) -> SceneState {
        let mut st = SceneState::new();
        if self.errored {
            return st;
        }
        let Some(state_fn) = &self.state_fn else {
            return st;
        };
        self.arm();
        // Continuity is best-effort: a faulting `state` yields no carry, never a
        // host error.
        if let Ok(table) = state_fn.call::<Table>(()) {
            for pair in table.pairs::<String, f32>().flatten() {
                st.set(&pair.0, pair.1);
            }
        }
        st
    }

    fn restore(&mut self, s: SceneState) {
        if self.errored {
            return;
        }
        let Some(restore_fn) = &self.restore_fn else {
            return;
        };
        let Ok(table) = self.lua.create_table() else {
            return;
        };
        for (k, v) in &s.values {
            if table.set(k.as_str(), *v).is_err() {
                return;
            }
        }
        self.arm();
        // Best-effort: a faulting `restore` is ignored rather than latching the
        // whole scene into an error (continuity is not worth failing a frame).
        let _ = restore_fn.call::<()>(table);
    }
}

impl fmt::Debug for LuauScene {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LuauScene")
            .field("id", &self.info.id)
            .field("errored", &self.errored)
            .field("last_error", &self.last_error)
            .finish()
    }
}

/// The default drawing palette a bare `--scene <luau-id>` run uses (until a
/// preset or album-art palette overrides it). Shared so the synthesized Luau
/// preset and any direct construction agree.
static DEFAULT_LUAU_PALETTE: LazyLock<Palette> = LazyLock::new(Palette::default_dark);

/// The palette a directly-constructed Luau scene initializes against.
#[must_use]
pub fn default_palette() -> Palette {
    *DEFAULT_LUAU_PALETTE
}
