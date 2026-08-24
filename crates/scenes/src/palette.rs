//! The host [`Palette`]: eight [`Rgb`] slots a scene addresses by index.
//!
//! Scenes never emit raw colours; they emit a [`crate::Slot`] and let the host
//! resolve it against a palette. The palette is static today and album-art
//! driven later, so the same scene re-themes without change. Eight slots is
//! enough for a gradient plus a few neutrals while staying small enough to pass
//! by value.

use crate::canvas::PALETTE_SLOTS;

/// A 24-bit colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// Eight colour slots addressed by a [`crate::Slot`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// The slots, indexed `0..8`.
    pub slots: [Rgb; PALETTE_SLOTS],
}

impl Palette {
    /// A tasteful default: a gradient from deep teal through cyan to amber and
    /// coral, followed by three neutrals (near-black, mid-grey, near-white).
    ///
    /// Slot layout:
    /// `0` deep teal, `1` teal, `2` cyan, `3` amber, `4` coral,
    /// `5` near-black neutral, `6` mid neutral, `7` near-white neutral.
    #[must_use]
    pub fn default_dark() -> Self {
        Self {
            slots: [
                Rgb(0x0d, 0x3b, 0x3b), // deep teal
                Rgb(0x1f, 0x8f, 0x8f), // teal
                Rgb(0x3f, 0xd0, 0xd0), // cyan
                Rgb(0xff, 0xb0, 0x20), // amber
                Rgb(0xff, 0x6b, 0x5b), // coral
                Rgb(0x14, 0x16, 0x1c), // near-black neutral
                Rgb(0x8a, 0x8f, 0x9c), // mid neutral
                Rgb(0xe6, 0xe8, 0xee), // near-white neutral
            ],
        }
    }

    /// The colour for `slot`, clamped into range so an out-of-range slot never
    /// panics.
    #[inline]
    #[must_use]
    pub fn color(&self, slot: crate::Slot) -> Rgb {
        self.slots[(slot as usize).min(PALETTE_SLOTS - 1)]
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::default_dark()
    }
}
