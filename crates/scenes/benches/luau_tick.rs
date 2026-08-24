//! P5 probe: cost of one Luau scene tick against the real `FeatureSnapshot`
//! shapes. Measures three axes — feature-access shape (A1/A2/A3), canvas
//! call shape (B1/B2) and sandbox overhead (C) — so the scene trait can be
//! frozen with numbers. Nothing here is production API.
//!
//! Each tick refreshes a representative snapshot, calls the script's
//! `update(features, dt)` then `render(canvas)`, and reads the draw output
//! back. The representative scene advances 200 particles with simple physics
//! (velocity + a nudge from the matching spectrum bar) and draws 64 bars plus
//! 200 points. Lua sources live under `benches/lua/*.luau` and are loaded with
//! `include_str!`; each returns a constructor so `update`/`render` are closures
//! over local state, which runs identically with or without `sandbox(true)`.

use std::cell::RefCell;
use std::hint::black_box;
use std::rc::Rc;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use mlua::{
    AnyUserData, Function, Lua, Table, UserData, UserDataFields, UserDataMethods, Value, VmState,
};
use scia_core::{FEATURE_SCHEMA_VERSION, FeatureSnapshot, SPECTRUM_BINS};

const N_PARTICLES: usize = 200;
const N_BARS: usize = 64;
const DT: f32 = 1.0 / 144.0;
/// Fixed stride of the B2 flat display list, in numbers per primitive.
const DL_STRIDE: usize = 6;

/// One queued draw call, the Rust side of the canvas.
#[derive(Clone, Copy)]
struct Primitive {
    tag: u8,
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    slot: u32,
}

/// Feature snapshot exposed to Luau as userdata (shapes A2 and A3): scalar
/// field getters plus a `bar(i)` method. Shared with the host through
/// `Rc<RefCell<_>>` so the host mutates it in place each tick.
struct FeaturesUd(Rc<RefCell<FeatureSnapshot>>);

impl UserData for FeaturesUd {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("rms", |_, this| Ok(this.0.borrow().rms));
        fields.add_field_method_get("peak", |_, this| Ok(this.0.borrow().peak));
        fields.add_field_method_get("onset", |_, this| Ok(this.0.borrow().onset));
        fields.add_field_method_get("beat_phase", |_, this| Ok(this.0.borrow().beat_phase));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("bar", |_, this, i: usize| {
            let s = this.0.borrow();
            let idx = i.saturating_sub(1).min(SPECTRUM_BINS - 1);
            Ok(s.spectrum[idx])
        });
    }
}

/// Canvas exposed to Luau as userdata (shape B1): per-primitive method calls
/// that push into a host `Vec<Primitive>` (cleared per tick, capacity kept).
struct CanvasUd(Rc<RefCell<Vec<Primitive>>>);

impl UserData for CanvasUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "bar",
            |_, this, (x, y, w, h, slot): (f32, f32, f32, f32, u32)| {
                this.0.borrow_mut().push(Primitive {
                    tag: 0,
                    a: x,
                    b: y,
                    c: w,
                    d: h,
                    slot,
                });
                Ok(())
            },
        );
        methods.add_method(
            "point",
            |_, this, (x, y, slot, intensity): (f32, f32, u32, f32)| {
                this.0.borrow_mut().push(Primitive {
                    tag: 1,
                    a: x,
                    b: y,
                    c: intensity,
                    d: 0.0,
                    slot,
                });
                Ok(())
            },
        );
    }
}

/// Fill a snapshot with representative, per-frame-varying values so the tick
/// never degenerates to constant folding.
fn refresh_snapshot(s: &mut FeatureSnapshot, frame: u64) {
    let t = frame as f32 * 0.01;
    s.rms = 0.5 + 0.4 * t.sin();
    s.peak = 0.6 + 0.3 * (t * 1.3).sin();
    s.onset = frame % 16 == 0;
    s.beat_phase = (t * 0.5).fract();
    for (i, bin) in s.spectrum.iter_mut().take(N_BARS).enumerate() {
        *bin = 0.5 + 0.5 * (t + i as f32 * 0.1).sin();
    }
}

fn base_snapshot() -> FeatureSnapshot {
    let mut s = FeatureSnapshot {
        schema_version: FEATURE_SCHEMA_VERSION,
        sample_rate: 48_000,
        channels: 2,
        spectrum_len: N_BARS as u16,
        ..FeatureSnapshot::default()
    };
    refresh_snapshot(&mut s, 0);
    s
}

#[derive(Clone, Copy)]
enum FeatMode {
    /// A1: features rebuilt as a fresh Lua table every tick.
    A1,
    /// A2: userdata field getters + a `bar(i)` method.
    A2,
    /// A3: userdata scalars + a preallocated bars table updated in place.
    A3,
}

#[derive(Clone, Copy)]
enum CanvasMode {
    /// B1: per-primitive userdata method calls.
    B1,
    /// B2: script fills a flat number array; host reads it back once per tick.
    B2,
}

/// How, if at all, the host installs a Luau interrupt. Luau fires the interrupt
/// at every VM safepoint (loop back-edges, calls, returns) — there is no
/// "every N instructions" knob — so each flavor pays the C→Rust crossing on
/// every safepoint and differs only in the work the callback does.
#[derive(Clone, Copy)]
enum InterruptKind {
    /// No interrupt installed.
    None,
    /// Callback returns immediately — isolates the pure crossing cost.
    Noop,
    /// Reads a monotonic clock every safepoint (the naive deadline check).
    Clock,
    /// Reads the clock only every 1024th safepoint, counter-gated.
    Counted,
}

#[derive(Clone, Copy)]
struct Opts {
    sandbox: bool,
    interrupt: InterruptKind,
    memlimit: bool,
}

impl Opts {
    fn plain() -> Self {
        Opts {
            sandbox: false,
            interrupt: InterruptKind::None,
            memlimit: false,
        }
    }
}

/// Host state for one feature-access shape.
enum Feat {
    A1 {
        // Boxed: the snapshot dwarfs the other variant, and the A1 path builds
        // a fresh Lua table from it each tick anyway.
        snap: Box<FeatureSnapshot>,
    },
    Ud {
        rc: Rc<RefCell<FeatureSnapshot>>,
        ud: AnyUserData,
        bars: Option<Table>,
    },
}

/// Host state for one canvas shape.
enum Canvas {
    B1 {
        prims: Rc<RefCell<Vec<Primitive>>>,
        ud: AnyUserData,
    },
    B2 {
        dl: Table,
    },
}

struct Scene {
    lua: Lua,
    update: Function,
    render: Function,
    feat: Feat,
    canvas: Canvas,
    frame: u64,
}

impl Scene {
    fn tick(&mut self) {
        let frame = self.frame;
        self.frame = self.frame.wrapping_add(1);

        let features_arg: Value = match &mut self.feat {
            Feat::A1 { snap } => {
                refresh_snapshot(snap.as_mut(), frame);
                let t = self.lua.create_table_with_capacity(0, 5).unwrap();
                t.raw_set("rms", snap.rms).unwrap();
                t.raw_set("peak", snap.peak).unwrap();
                t.raw_set("onset", snap.onset).unwrap();
                t.raw_set("beat_phase", snap.beat_phase).unwrap();
                let spec = self.lua.create_table_with_capacity(N_BARS, 0).unwrap();
                for i in 0..N_BARS {
                    spec.raw_set((i + 1) as i64, snap.spectrum[i]).unwrap();
                }
                t.raw_set("spectrum", spec).unwrap();
                Value::Table(t)
            }
            Feat::Ud { rc, ud, bars } => {
                {
                    let mut s = rc.borrow_mut();
                    refresh_snapshot(&mut s, frame);
                    if let Some(bars) = bars {
                        for i in 0..N_BARS {
                            bars.raw_set((i + 1) as i64, s.spectrum[i]).unwrap();
                        }
                    }
                }
                Value::UserData(ud.clone())
            }
        };

        self.update.call::<()>((features_arg, DT)).unwrap();

        match &self.canvas {
            Canvas::B1 { prims, ud } => {
                prims.borrow_mut().clear();
                self.render.call::<()>(ud.clone()).unwrap();
                // Read every primitive back, mirroring what a real frontend
                // does with the queued draw list (and symmetric with B2).
                let p = prims.borrow();
                let mut acc = 0.0f32;
                for prim in p.iter() {
                    acc +=
                        f32::from(prim.tag) + prim.a + prim.b + prim.c + prim.d + prim.slot as f32;
                }
                black_box(acc);
            }
            Canvas::B2 { dl } => {
                self.render.call::<()>(()).unwrap();
                let n: i64 = dl.get("n").unwrap();
                let mut acc = 0.0f64;
                for i in 1..=n {
                    let v: f64 = dl.raw_get(i).unwrap();
                    acc += v;
                }
                black_box(acc);
            }
        }
    }
}

fn build(feat_mode: FeatMode, canvas_mode: CanvasMode, opts: Opts) -> Scene {
    let lua = Lua::new();

    // A deadline the tick never reaches: these measure the cost of an installed
    // interrupt, not a trip.
    let deadline = Instant::now() + Duration::from_secs(3600);
    match opts.interrupt {
        InterruptKind::None => {}
        InterruptKind::Noop => {
            lua.set_interrupt(move |_| Ok(VmState::Continue));
        }
        InterruptKind::Clock => {
            lua.set_interrupt(move |_| {
                if Instant::now() >= deadline {
                    return Err(mlua::Error::runtime("deadline"));
                }
                Ok(VmState::Continue)
            });
        }
        InterruptKind::Counted => {
            let counter = std::cell::Cell::new(0u32);
            lua.set_interrupt(move |_| {
                let c = counter.get().wrapping_add(1);
                counter.set(c);
                if c % 1024 == 0 && Instant::now() >= deadline {
                    return Err(mlua::Error::runtime("deadline"));
                }
                Ok(VmState::Continue)
            });
        }
    }
    if opts.memlimit {
        lua.set_memory_limit(8 * 1024 * 1024).unwrap();
    }
    if opts.sandbox {
        lua.sandbox(true).unwrap();
    }

    let rc = Rc::new(RefCell::new(base_snapshot()));
    let ctx = lua.create_table().unwrap();

    let bars = if matches!(feat_mode, FeatMode::A3) {
        let b = lua.create_table_with_capacity(N_BARS, 0).unwrap();
        for i in 0..N_BARS {
            b.raw_set((i + 1) as i64, 0.0f32).unwrap();
        }
        ctx.raw_set("bars", &b).unwrap();
        Some(b)
    } else {
        None
    };

    let dl = if matches!(canvas_mode, CanvasMode::B2) {
        let cap = N_BARS * DL_STRIDE + N_PARTICLES * DL_STRIDE;
        let d = lua.create_table_with_capacity(cap, 1).unwrap();
        ctx.raw_set("dl", &d).unwrap();
        Some(d)
    } else {
        None
    };

    let src = match (feat_mode, canvas_mode) {
        (FeatMode::A1, CanvasMode::B1) => include_str!("lua/a1_b1.luau"),
        (FeatMode::A2, CanvasMode::B1) => include_str!("lua/a2_b1.luau"),
        (FeatMode::A3, CanvasMode::B1) => include_str!("lua/a3_b1.luau"),
        (FeatMode::A3, CanvasMode::B2) => include_str!("lua/a3_b2.luau"),
        _ => panic!("unsupported feature/canvas combination for this probe"),
    };
    let ctor: Function = lua.load(src).eval().unwrap();
    let (update, render): (Function, Function) = ctor.call(ctx).unwrap();

    let feat = match feat_mode {
        FeatMode::A1 => Feat::A1 {
            snap: Box::new(base_snapshot()),
        },
        FeatMode::A2 => {
            let ud = lua.create_userdata(FeaturesUd(rc.clone())).unwrap();
            Feat::Ud { rc, ud, bars: None }
        }
        FeatMode::A3 => {
            let ud = lua.create_userdata(FeaturesUd(rc.clone())).unwrap();
            Feat::Ud { rc, ud, bars }
        }
    };

    let canvas = match canvas_mode {
        CanvasMode::B1 => {
            let prims = Rc::new(RefCell::new(Vec::with_capacity(N_BARS + N_PARTICLES)));
            let ud = lua.create_userdata(CanvasUd(prims.clone())).unwrap();
            Canvas::B1 { prims, ud }
        }
        CanvasMode::B2 => Canvas::B2 {
            dl: dl.expect("B2 allocates a display list"),
        },
    };

    Scene {
        lua,
        update,
        render,
        feat,
        canvas,
        frame: 0,
    }
}

/// A. Feature-access shape (all with canvas B1).
fn bench_feature_access(c: &mut Criterion) {
    let mut g = c.benchmark_group("feature_access");
    g.sample_size(30);
    for (name, fm) in [
        ("A1_table", FeatMode::A1),
        ("A2_userdata", FeatMode::A2),
        ("A3_inplace", FeatMode::A3),
    ] {
        let mut scene = build(fm, CanvasMode::B1, Opts::plain());
        g.bench_function(name, |b| b.iter(|| scene.tick()));
    }
    g.finish();
}

/// B. Canvas call shape (with the best feature-access shape, A3).
fn bench_canvas(c: &mut Criterion) {
    let mut g = c.benchmark_group("canvas");
    g.sample_size(30);
    let mut b1 = build(FeatMode::A3, CanvasMode::B1, Opts::plain());
    g.bench_function("B1_methods", |b| b.iter(|| b1.tick()));
    let mut b2 = build(FeatMode::A3, CanvasMode::B2, Opts::plain());
    g.bench_function("B2_displaylist", |b| b.iter(|| b2.tick()));
    g.finish();
}

/// C. Sandbox overhead on the best combination (A3 + B1).
fn bench_sandbox(c: &mut Criterion) {
    let mut g = c.benchmark_group("sandbox");
    g.sample_size(30);
    let variants = [
        (
            "plain",
            Opts {
                sandbox: false,
                interrupt: InterruptKind::None,
                memlimit: false,
            },
        ),
        (
            "sandbox",
            Opts {
                sandbox: true,
                interrupt: InterruptKind::None,
                memlimit: false,
            },
        ),
        (
            "sandbox_interrupt_noop",
            Opts {
                sandbox: true,
                interrupt: InterruptKind::Noop,
                memlimit: false,
            },
        ),
        (
            "sandbox_interrupt_clock",
            Opts {
                sandbox: true,
                interrupt: InterruptKind::Clock,
                memlimit: false,
            },
        ),
        (
            "sandbox_interrupt_counted",
            Opts {
                sandbox: true,
                interrupt: InterruptKind::Counted,
                memlimit: false,
            },
        ),
        (
            "full",
            Opts {
                sandbox: true,
                interrupt: InterruptKind::Clock,
                memlimit: true,
            },
        ),
    ];
    for (name, opts) in variants {
        let mut scene = build(FeatMode::A3, CanvasMode::B1, opts);
        g.bench_function(name, |b| b.iter(|| scene.tick()));
    }
    g.finish();
}

fn config() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2))
}

criterion_group! {
    name = benches;
    config = config();
    targets = bench_feature_access, bench_canvas, bench_sandbox
}
criterion_main!(benches);
