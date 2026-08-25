//! The Rust↔Luau bridge: the two userdata objects a scripted scene talks to.
//!
//! A [`FeaturesUd`] exposes the newest [`FeatureSnapshot`] to the script as
//! cached scalar field getters plus a `bar(i)` method; the spectrum itself
//! crosses as **one** Lua table (`ctx.bars`) the host rewrites in place each
//! tick, so a per-frame feature read allocates nothing on either side. A
//! [`CanvasUd`] exposes the abstract [`Canvas`] as per-primitive methods that
//! batch straight into the host display list — no intermediate Lua tables per
//! primitive. Both share their host state through `Rc<RefCell<_>>`, which is why
//! a scripted scene is single-threaded (and the `Scene` trait carries no `Send`
//! bound).

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{UserData, UserDataFields, UserDataMethods};
use scia_core::{FeatureSnapshot, SPECTRUM_BINS};

use crate::canvas::{Canvas, Style};
use crate::scene::{ParamSpec, Params};

/// The newest features, shared with the host. The host rewrites `*borrow_mut()`
/// in place at the top of every `update`, so the getters below always read the
/// current hop without allocating or rebuilding a table.
pub(crate) struct FeaturesUd(pub(crate) Rc<RefCell<FeatureSnapshot>>);

impl UserData for FeaturesUd {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        // The documented scalar feature API. Each getter clones one `f32` (or
        // `bool`) out of the shared snapshot — no allocation, no table build.
        fields.add_field_method_get("rms", |_, this| Ok(this.0.borrow().rms));
        // `loud` is an alias for `rms`, the everyday name for loudness.
        fields.add_field_method_get("loud", |_, this| Ok(this.0.borrow().rms));
        fields.add_field_method_get("peak", |_, this| Ok(this.0.borrow().peak));
        fields.add_field_method_get("flux", |_, this| Ok(this.0.borrow().flux));
        fields.add_field_method_get("onset", |_, this| Ok(this.0.borrow().onset));
        fields.add_field_method_get("onset_age", |_, this| Ok(this.0.borrow().onset_age_ms));
        fields.add_field_method_get("bass", |_, this| Ok(this.0.borrow().bands[0]));
        fields.add_field_method_get("mid", |_, this| Ok(this.0.borrow().bands[1]));
        fields.add_field_method_get("treb", |_, this| Ok(this.0.borrow().bands[2]));
        fields.add_field_method_get("beat_phase", |_, this| Ok(this.0.borrow().beat_phase));
        fields.add_field_method_get("beat", |_, this| Ok(this.0.borrow().beat_confidence));
        fields.add_field_method_get("tempo", |_, this| Ok(this.0.borrow().tempo_bpm));
        fields.add_field_method_get("quiet_ms", |_, this| Ok(this.0.borrow().quiet_ms));
        fields.add_field_method_get("width", |_, this| {
            let s = this.0.borrow();
            Ok(((1.0 - s.stereo_correlation) / 2.0).clamp(0.0, 1.0))
        });
        // How many spectrum bars are valid this hop (`ctx.bars[1..=bar_count]`).
        fields.add_field_method_get("bar_count", |_, this| {
            Ok(this.0.borrow().spectrum_len as i64)
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // `features:bar(i)` — a 1-based spectrum read, for scenes that prefer a
        // method over the `ctx.bars` table. Out-of-range indices clamp rather
        // than error, so a scripting slip can never fault the tick.
        methods.add_method("bar", |_, this, i: i64| {
            let s = this.0.borrow();
            let n = s.spectrum_len as i64;
            if n <= 0 || i < 1 || i > n {
                return Ok(0.0f32);
            }
            let idx = (i as usize - 1).min(SPECTRUM_BINS - 1);
            Ok(s.spectrum[idx])
        });
    }
}

/// The abstract canvas, shared with the host. Each method clamps through the
/// real [`Canvas`] builders (coordinates/sizes to `0.0..=1.0`, `NaN`→`0.0`,
/// palette slot to `0..=7`), so a scripted scene can never push garbage into the
/// display list. A slot is a **0-based palette index**, not a Lua array index.
pub(crate) struct CanvasUd {
    pub(crate) canvas: Rc<RefCell<Canvas>>,
    /// Reused scratch for `field()` so a field draw copies into a kept buffer
    /// instead of allocating a fresh `Vec` per call.
    scratch: RefCell<Vec<f32>>,
}

impl CanvasUd {
    pub(crate) fn new(canvas: Rc<RefCell<Canvas>>) -> Self {
        Self {
            canvas,
            scratch: RefCell::new(Vec::new()),
        }
    }
}

/// Clamp a Lua number to a palette slot index `0..=7`.
fn to_slot(v: f64) -> u8 {
    if v.is_nan() {
        0
    } else {
        v.clamp(0.0, 7.0) as u8
    }
}

impl UserData for CanvasUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // The aspect ratio (width / height) of the drawing surface, so a scene
        // can keep a circle round. The only shape hint a scene ever gets.
        methods.add_method("aspect", |_, this, ()| Ok(this.canvas.borrow().aspect()));

        // canvas:bar(x, y, w, h, slot, intensity) — an axis-aligned rectangle.
        methods.add_method(
            "bar",
            |_, this, (x, y, w, h, slot, intensity): (f32, f32, f32, f32, f64, f32)| {
                this.canvas
                    .borrow_mut()
                    .bar(x, y, w, h, Style::new(to_slot(slot), intensity));
                Ok(())
            },
        );

        // canvas:line(x0, y0, x1, y1, width, slot, intensity) — a segment.
        methods.add_method(
            "line",
            |_,
             this,
             (x0, y0, x1, y1, width, slot, intensity): (f32, f32, f32, f32, f32, f64, f32)| {
                this.canvas
                    .borrow_mut()
                    .line(x0, y0, x1, y1, width, Style::new(to_slot(slot), intensity));
                Ok(())
            },
        );

        // canvas:point(x, y, size, slot, intensity) — a particle.
        methods.add_method(
            "point",
            |_, this, (x, y, size, slot, intensity): (f32, f32, f32, f64, f32)| {
                this.canvas
                    .borrow_mut()
                    .point(x, y, size, Style::new(to_slot(slot), intensity));
                Ok(())
            },
        );

        // canvas:text(x, y, string, slot, intensity) — a text run.
        methods.add_method(
            "text",
            |_, this, (x, y, text, slot, intensity): (f32, f32, mlua::String, f64, f32)| {
                let s = text.to_str()?;
                this.canvas
                    .borrow_mut()
                    .text(x, y, &s, Style::new(to_slot(slot), intensity));
                Ok(())
            },
        );

        // canvas:field(cols, rows, values, slot, intensity) — a coarse grid of
        // intensities from a flat, row-major Lua array of `cols*rows` numbers.
        methods.add_method(
            "field",
            |_, this, (cols, rows, values, slot, intensity): (u16, u16, mlua::Table, f64, f32)| {
                let count = cols as usize * rows as usize;
                let mut scratch = this.scratch.borrow_mut();
                scratch.clear();
                for i in 1..=count {
                    // Missing / non-number entries read as 0.0; the canvas clamps.
                    let v: f32 = values.get(i as i64).unwrap_or(0.0);
                    scratch.push(v);
                }
                this.canvas.borrow_mut().field(
                    cols,
                    rows,
                    &scratch,
                    Style::new(to_slot(slot), intensity),
                );
                Ok(())
            },
        );
    }
}

/// Write the manifest params into the Lua table the script reads each frame,
/// in place (no allocation once the keys exist). A key absent from the bag
/// falls back to its manifest default, and every value is clamped to the
/// manifest range — a `[map]` write can be `offset + scale * env`, outside the
/// range the manifest declared. Called from `init` and every `apply_params`.
pub(crate) fn write_params(
    table: &mlua::Table,
    params: &Params,
    manifest: &[ParamSpec],
) -> mlua::Result<()> {
    for spec in manifest {
        let v = params.get(spec.key).unwrap_or(spec.default);
        table.set(spec.key, v.clamp(spec.min, spec.max))?;
    }
    Ok(())
}
