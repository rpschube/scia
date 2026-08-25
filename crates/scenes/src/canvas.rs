//! The abstract [`Canvas`]: a resolution-independent display list a scene
//! writes and a presenter rasterizes.
//!
//! A canvas is a flat list of [`Primitive`]s in **normalized coordinates**:
//! every position is a fraction of the canvas in `0.0..=1.0`, the origin is the
//! top-left corner and `y` grows downward. Sizes are fractions too. A scene
//! never learns the physical cell or pixel size; the only shape hint it gets is
//! [`Canvas::aspect`] (width / height in physical units) so a circle can be
//! drawn round. The TUI presenter rasterizes primitives to terminal cells; a
//! future GPU presenter draws them as quads. Neither is visible from here.
//!
//! # Validation
//!
//! Every builder method **clamps its inputs** before storing them, so a buggy
//! scene can never push garbage downstream: coordinates and sizes are clamped
//! to `0.0..=1.0`, intensities to `0.0..=1.0`, and `NaN` becomes `0.0`. Palette
//! slots are clamped to the eight valid indices `0..=7`. Field values are
//! clamped as intensities. The presenter can therefore trust every value it
//! reads without re-checking.
//!
//! # Allocation
//!
//! The three backing stores — the primitive list, the field-value arena and the
//! text arena — retain their capacity across [`Canvas::clear`]. After a warm-up
//! frame a scene that draws the same shape budget every frame does not allocate;
//! all builder methods are `#[inline]` and allocation-free once capacity is
//! reached.

/// A palette slot: an index `0..8` into the host's [`crate::Palette`].
pub type Slot = u8;

/// The number of slots in a palette, and the exclusive upper bound for a
/// [`Slot`] after clamping.
pub const PALETTE_SLOTS: usize = 8;

/// Per-primitive style: which palette slot colours it and how intense it is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    /// Index into the host palette; clamped to `0..=7` when stored.
    pub slot: Slot,
    /// Intensity in `0.0..=1.0`; clamped (and `NaN`-guarded) when stored.
    pub intensity: f32,
}

impl Style {
    /// A convenience constructor. Values are **not** clamped here — clamping
    /// happens when the style is stored via a [`Canvas`] builder method.
    #[inline]
    #[must_use]
    pub fn new(slot: Slot, intensity: f32) -> Self {
        Self { slot, intensity }
    }

    /// Clamp the slot into `0..=7` and the intensity into `0.0..=1.0`.
    #[inline]
    fn normalized(self) -> Self {
        Self {
            slot: self.slot.min((PALETTE_SLOTS - 1) as Slot),
            intensity: clamp01(self.intensity),
        }
    }
}

/// One drawable element of a [`Canvas`], in normalized coordinates.
///
/// [`Primitive::Field`] and [`Primitive::Text`] do not carry their bulk data
/// inline; they hold `first..first + len` ranges into [`Canvas::field_data`]
/// and [`Canvas::text_data`] respectively. Use [`Canvas::field_of`] and
/// [`Canvas::text_of`] to resolve them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Primitive {
    /// An axis-aligned filled rectangle (bars, blocks). `w`/`h` are fractions
    /// of the canvas.
    Bar {
        /// Left edge in `0.0..=1.0`.
        x: f32,
        /// Top edge in `0.0..=1.0`.
        y: f32,
        /// Width as a fraction of the canvas width.
        w: f32,
        /// Height as a fraction of the canvas height.
        h: f32,
        /// Fill style.
        style: Style,
    },
    /// A straight line segment. `width` is a fraction of the canvas height.
    Line {
        /// Start `x` in `0.0..=1.0`.
        x0: f32,
        /// Start `y` in `0.0..=1.0`.
        y0: f32,
        /// End `x` in `0.0..=1.0`.
        x1: f32,
        /// End `y` in `0.0..=1.0`.
        y1: f32,
        /// Stroke width as a fraction of the canvas height.
        width: f32,
        /// Stroke style.
        style: Style,
    },
    /// A particle. `size` is a fraction of the canvas height.
    Point {
        /// Centre `x` in `0.0..=1.0`.
        x: f32,
        /// Centre `y` in `0.0..=1.0`.
        y: f32,
        /// Diameter as a fraction of the canvas height.
        size: f32,
        /// Point style.
        style: Style,
    },
    /// A coarse grid of intensities, row-major, `cols * rows` values stored in
    /// [`Canvas::field_data`] at `first..first + len`.
    Field {
        /// Column count.
        cols: u16,
        /// Row count.
        rows: u16,
        /// Offset of the first value in [`Canvas::field_data`].
        first: u32,
        /// Number of values (`cols * rows`).
        len: u32,
        /// Field style.
        style: Style,
    },
    /// A text run. The bytes live in [`Canvas::text_data`] at
    /// `first..first + len`.
    Text {
        /// Anchor `x` in `0.0..=1.0`.
        x: f32,
        /// Anchor `y` in `0.0..=1.0`.
        y: f32,
        /// Byte offset of the run in [`Canvas::text_data`].
        first: u32,
        /// Length of the run in bytes.
        len: u32,
        /// Text style.
        style: Style,
    },
}

/// The display list a scene writes and a presenter reads.
///
/// See the [module docs](self) for the coordinate system, the clamping
/// guarantees and the allocation contract.
#[derive(Clone, Debug, Default)]
pub struct Canvas {
    primitives: Vec<Primitive>,
    field_data: Vec<f32>,
    text_data: String,
    aspect: f32,
}

impl Canvas {
    /// Create an empty canvas with the given aspect ratio (width / height in
    /// physical units).
    #[must_use]
    pub fn new(aspect: f32) -> Self {
        Self {
            primitives: Vec::new(),
            field_data: Vec::new(),
            text_data: String::new(),
            aspect,
        }
    }

    /// Drop every primitive and its backing data, **retaining capacity** so the
    /// next frame reuses the same allocations.
    #[inline]
    pub fn clear(&mut self) {
        self.primitives.clear();
        self.field_data.clear();
        self.text_data.clear();
    }

    /// Overwrite this canvas with a copy of `src`, **retaining backing
    /// capacity** on both stores (a warmed destination reuses its allocations,
    /// same contract as [`clear`](Self::clear)). Used to hold a scripted scene's
    /// last good frame across a failed tick without allocating on the hold path.
    #[inline]
    pub fn copy_from(&mut self, src: &Canvas) {
        self.primitives.clone_from(&src.primitives);
        self.field_data.clone_from(&src.field_data);
        self.text_data.clone_from(&src.text_data);
        self.aspect = src.aspect;
    }

    /// Set the aspect ratio (width / height in physical units).
    #[inline]
    pub fn set_aspect(&mut self, aspect: f32) {
        self.aspect = aspect;
    }

    /// The aspect ratio (width / height in physical units).
    #[inline]
    #[must_use]
    pub fn aspect(&self) -> f32 {
        self.aspect
    }

    /// Push an axis-aligned filled rectangle. Inputs are clamped to `0.0..=1.0`.
    #[inline]
    pub fn bar(&mut self, x: f32, y: f32, w: f32, h: f32, style: Style) {
        self.primitives.push(Primitive::Bar {
            x: clamp01(x),
            y: clamp01(y),
            w: clamp01(w),
            h: clamp01(h),
            style: style.normalized(),
        });
    }

    /// Push a line segment. `width` is a fraction of the canvas height. Inputs
    /// are clamped to `0.0..=1.0`.
    #[inline]
    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, style: Style) {
        self.primitives.push(Primitive::Line {
            x0: clamp01(x0),
            y0: clamp01(y0),
            x1: clamp01(x1),
            y1: clamp01(y1),
            width: clamp01(width),
            style: style.normalized(),
        });
    }

    /// Push a particle. `size` is a fraction of the canvas height. Inputs are
    /// clamped to `0.0..=1.0`.
    #[inline]
    pub fn point(&mut self, x: f32, y: f32, size: f32, style: Style) {
        self.primitives.push(Primitive::Point {
            x: clamp01(x),
            y: clamp01(y),
            size: clamp01(size),
            style: style.normalized(),
        });
    }

    /// Push a `cols * rows` field, copying `values` (row-major) into the field
    /// arena. Exactly `cols * rows` values are stored: missing entries are
    /// treated as `0.0` and each value is clamped to `0.0..=1.0`.
    #[inline]
    pub fn field(&mut self, cols: u16, rows: u16, values: &[f32], style: Style) {
        let count = cols as usize * rows as usize;
        let first = self.field_data.len() as u32;
        for i in 0..count {
            let v = values.get(i).copied().unwrap_or(0.0);
            self.field_data.push(clamp01(v));
        }
        self.primitives.push(Primitive::Field {
            cols,
            rows,
            first,
            len: count as u32,
            style: style.normalized(),
        });
    }

    /// Push a text run, copying the bytes into the text arena. Anchor
    /// coordinates are clamped to `0.0..=1.0`.
    #[inline]
    pub fn text(&mut self, x: f32, y: f32, text: &str, style: Style) {
        let first = self.text_data.len() as u32;
        self.text_data.push_str(text);
        let len = text.len() as u32;
        self.primitives.push(Primitive::Text {
            x: clamp01(x),
            y: clamp01(y),
            first,
            len,
            style: style.normalized(),
        });
    }

    /// The primitives written this frame, in draw order.
    #[inline]
    #[must_use]
    pub fn primitives(&self) -> &[Primitive] {
        &self.primitives
    }

    /// The whole field-value arena.
    #[inline]
    #[must_use]
    pub fn field_data(&self) -> &[f32] {
        &self.field_data
    }

    /// The whole text arena.
    #[inline]
    #[must_use]
    pub fn text_data(&self) -> &str {
        &self.text_data
    }

    /// Resolve a [`Primitive::Text`] to its string, or `None` for any other
    /// primitive.
    #[inline]
    #[must_use]
    pub fn text_of(&self, p: &Primitive) -> Option<&str> {
        if let Primitive::Text { first, len, .. } = p {
            let start = *first as usize;
            let end = start + *len as usize;
            self.text_data.get(start..end)
        } else {
            None
        }
    }

    /// Resolve a [`Primitive::Field`] to its value slice, or `None` for any
    /// other primitive.
    #[inline]
    #[must_use]
    pub fn field_of(&self, p: &Primitive) -> Option<&[f32]> {
        if let Primitive::Field { first, len, .. } = p {
            let start = *first as usize;
            let end = start + *len as usize;
            self.field_data.get(start..end)
        } else {
            None
        }
    }
}

/// Clamp to `0.0..=1.0`, mapping `NaN` to `0.0`.
#[inline]
fn clamp01(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}
