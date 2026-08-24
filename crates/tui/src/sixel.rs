//! The sixel graphics encoder.
//!
//! Sixel [1] is the DEC bitmap protocol Windows Terminal (≥1.24) and other
//! terminals draw images with. Like [`crate::kitty`] this encoder transmits one
//! full RGB frame per call — there are no animation frames, so every frame is a
//! self-contained full retransmission: the DCS stream carries its own palette
//! register definitions and pixel data, and paints over whatever cells sit under
//! its rectangle.
//!
//! # Palette
//!
//! A fixed `6×7×6` RGB cube — 252 colour registers, indices `0..252` — is used
//! for every frame. Each pixel is quantized to a register by integer math
//! ([`quantize`]); there is no per-frame palette computation and no LUT
//! allocation. The register definitions (`#<i>;2;<r>;<g>;<b>` on sixel's `0..100`
//! colour scale) are re-emitted in full at the head of every stream.
//!
//! # Stream shape
//!
//! `ESC P 0;0;8 q` (DCS), then raster attributes `"1;1;<w>;<h>`, then the 252
//! palette definitions, then the pixel data as 6-row bands. Within a band each
//! register that appears is written as `#<i>` followed by run-length-encoded
//! sixel bytes (`!<n><char>` for runs ≥ 4, literal `<char>`s otherwise; every
//! `char` is `0x3F` plus a 6-bit column mask). Registers within a band are
//! separated by `$` (carriage return); bands are separated by `-` (advance six
//! rows); the stream ends with `ESC \`. Registers absent from a band are skipped,
//! and a register's trailing all-zero columns are omitted.
//!
//! # Emit size
//!
//! Sixel has no placement scaling — the emitted bitmap covers exactly the pixels
//! it declares — so the caller rasterizes into the same budgeted pixel size the
//! kitty path uses (see [`crate::pixel::image_dims`]) and passes the integer
//! downscale factor `k` ([`crate::pixel::image_downscale`]). The encoder
//! pixel-repeats each source pixel `k` columns wide and `k` rows tall so the
//! on-screen image fills the body area; run-length encoding keeps the repeated
//! columns cheap.
//!
//! # Cleanup
//!
//! Nothing persists: a sixel is ordinary cell content, not an entry in an image
//! store, so there is no analogue of the kitty `a=d` delete — leaving the
//! alternate screen discards it with the rest of the scrollback. No cleanup
//! sequence is emitted.
//!
//! Everything is written into caller-provided reusable buffers and the encoder's
//! own scratch, so a warm encoder allocates nothing.
//!
//! [1]: https://en.wikipedia.org/wiki/Sixel

use std::io::Write as _;

/// Cube levels along the red axis.
const CUBE_R: u16 = 6;
/// Cube levels along the green axis (the eye is most sensitive here).
const CUBE_G: u16 = 7;
/// Cube levels along the blue axis.
const CUBE_B: u16 = 6;

/// The number of palette registers: the `6×7×6` colour cube.
pub const SIXEL_REGISTERS: usize = (CUBE_R * CUBE_G * CUBE_B) as usize;

/// Sentinel for an emit row that lies past the image bottom (the final band can
/// hold fewer than six rows). Never a valid register index.
const NONE: u16 = u16::MAX;

/// Quantize an RGB8 triple to a cube register index in `0..252`.
///
/// Each channel is bucketed by integer math to its cube level, then folded into
/// the flat index `(r_idx * 7 + g_idx) * 6 + b_idx`. Pure and allocation-free.
#[must_use]
pub fn quantize(r: u8, g: u8, b: u8) -> u16 {
    let ri = (u16::from(r) * CUBE_R / 256).min(CUBE_R - 1);
    let gi = (u16::from(g) * CUBE_G / 256).min(CUBE_G - 1);
    let bi = (u16::from(b) * CUBE_B / 256).min(CUBE_B - 1);
    (ri * CUBE_G + gi) * CUBE_B + bi
}

/// The representative colour of register `idx` on sixel's `0..=100` scale, as
/// `(r, g, b)`. Evenly spaces each cube level across the range.
#[must_use]
fn register_rgb100(idx: u16) -> (u16, u16, u16) {
    let bi = idx % CUBE_B;
    let gi = (idx / CUBE_B) % CUBE_G;
    let ri = idx / (CUBE_B * CUBE_G);
    (
        ri * 100 / (CUBE_R - 1),
        gi * 100 / (CUBE_G - 1),
        bi * 100 / (CUBE_B - 1),
    )
}

/// A reusable sixel frame encoder.
///
/// Holds the per-band scratch across calls, so [`encode`](Self::encode) does not
/// allocate once its buffers have grown to a steady frame width.
pub struct SixelEncoder {
    /// Quantized register per source column and band row: `band[sc * 6 + row]`,
    /// or [`NONE`] for an emit row past the image bottom.
    band: Vec<u16>,
    /// Which registers appear in the current band; `SIXEL_REGISTERS` long.
    present: Vec<bool>,
}

impl Default for SixelEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl SixelEncoder {
    /// A fresh encoder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            band: Vec::new(),
            present: vec![false; SIXEL_REGISTERS],
        }
    }

    /// Encode one full RGB frame into `out` (cleared first) as a sixel DCS stream
    /// ready to write to the terminal at the image origin.
    ///
    /// `rgb` is `px.0 * px.1 * 3` row-major RGB8 bytes. `px` is the *source*
    /// image size in pixels `(width, height)`; `k` is the integer pixel-repeat
    /// factor (`≥ 1`), so the emitted bitmap is `px.0 * k` wide and `px.1 * k`
    /// tall. A zero-sized image produces no output.
    pub fn encode(&mut self, rgb: &[u8], px: (u16, u16), k: u16, out: &mut Vec<u8>) {
        out.clear();
        let (w, h) = px;
        let k = u32::from(k.max(1));
        if w == 0 || h == 0 || rgb.is_empty() {
            return;
        }
        let sw = w as usize;
        let ew = u32::from(w) * k;
        let eh = u32::from(h) * k;

        // DCS introducer and raster attributes (1:1 aspect, exact emit size).
        out.extend_from_slice(b"\x1bP0;0;8q");
        let _ = write!(out, "\"1;1;{ew};{eh}");

        // Full palette retransmission: every register, every frame.
        for i in 0..SIXEL_REGISTERS as u16 {
            let (r, g, b) = register_rgb100(i);
            let _ = write!(out, "#{i};2;{r};{g};{b}");
        }

        // Grow the per-band scratch once; warm frames of the same width reuse it.
        let need = sw * 6;
        if self.band.len() != need {
            self.band.resize(need, NONE);
        }

        let bands = eh.div_ceil(6);
        for band in 0..bands {
            if band > 0 {
                out.push(b'-');
            }

            // Map the six emit rows of this band back to source rows and quantize
            // each source column. An emit row past the bottom stays NONE.
            for v in &mut self.band {
                *v = NONE;
            }
            for row in 0..6u32 {
                let er = band * 6 + row;
                if er >= eh {
                    continue;
                }
                let sr = (er / k) as usize;
                let base = sr * sw * 3;
                let r = row as usize;
                for sc in 0..sw {
                    let o = base + sc * 3;
                    self.band[sc * 6 + r] = quantize(rgb[o], rgb[o + 1], rgb[o + 2]);
                }
            }

            // Which registers appear in the band, in ascending index order.
            for p in &mut self.present {
                *p = false;
            }
            for &q in &self.band {
                if q != NONE {
                    self.present[q as usize] = true;
                }
            }

            let mut first = true;
            for reg in 0..SIXEL_REGISTERS as u16 {
                if !self.present[reg as usize] {
                    continue;
                }
                if !first {
                    out.push(b'$');
                }
                first = false;
                let _ = write!(out, "#{reg}");
                emit_register_row(out, &self.band, sw, reg, k);
            }
        }

        out.extend_from_slice(b"\x1b\\");
    }
}

/// Emit register `reg`'s run-length-encoded row for the current band.
///
/// Walks the `sw` source columns; each contributes `k` emit columns of the same
/// 6-bit mask (`0x3F` base). Runs of identical bytes are collapsed with `!<n>`
/// once they reach four. Interior all-zero (`?`) runs are emitted to keep later
/// pixels positioned; the trailing all-zero run is dropped.
fn emit_register_row(out: &mut Vec<u8>, band: &[u16], sw: usize, reg: u16, k: u32) {
    let mut run_char: u8 = 0;
    let mut run_len: u32 = 0;
    let mut have_run = false;
    // A held zero-run, flushed only once a non-zero run follows it, so a trailing
    // zero run is simply never written.
    let mut pending_zero: u32 = 0;

    for sc in 0..sw {
        let mut mask = 0u8;
        for row in 0..6 {
            if band[sc * 6 + row] == reg {
                mask |= 1u8 << row;
            }
        }
        let ch = 0x3Fu8 + mask;
        if have_run && ch == run_char {
            run_len += k;
            continue;
        }
        if have_run {
            flush_run(out, run_char, run_len, &mut pending_zero);
        }
        run_char = ch;
        run_len = k;
        have_run = true;
    }
    if have_run && run_char != b'?' {
        if pending_zero > 0 {
            write_run(out, b'?', pending_zero);
        }
        write_run(out, run_char, run_len);
    }
    // A trailing zero run (run_char == '?') is intentionally discarded.
}

/// Flush one completed run, holding zero-runs in `pending_zero` so a trailing one
/// can be dropped by the caller.
fn flush_run(out: &mut Vec<u8>, ch: u8, len: u32, pending_zero: &mut u32) {
    if ch == b'?' {
        *pending_zero += len;
    } else {
        if *pending_zero > 0 {
            write_run(out, b'?', *pending_zero);
            *pending_zero = 0;
        }
        write_run(out, ch, len);
    }
}

/// Write a run of `len` copies of `ch`: `!<len><ch>` when it pays for itself
/// (four or more), otherwise the literal bytes.
fn write_run(out: &mut Vec<u8>, ch: u8, len: u32) {
    if len == 0 {
        return;
    }
    if len >= 4 {
        out.push(b'!');
        let _ = write!(out, "{len}");
        out.push(ch);
    } else {
        for _ in 0..len {
            out.push(ch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_hits_the_cube_corners() {
        assert_eq!(quantize(0, 0, 0), 0, "black");
        // white: r_idx=5, g_idx=6, b_idx=5 → (5*7+6)*6+5 = 251
        assert_eq!(quantize(255, 255, 255), 251, "white");
        assert_eq!(quantize(255, 0, 0), 210, "pure red");
        assert_eq!(quantize(0, 255, 0), 36, "pure green");
        assert_eq!(quantize(0, 0, 255), 5, "pure blue");
        assert_eq!(quantize(128, 128, 128), 147, "mid grey");
    }

    #[test]
    fn register_colours_stay_within_the_sixel_scale() {
        for i in 0..SIXEL_REGISTERS as u16 {
            let (r, g, b) = register_rgb100(i);
            assert!(r <= 100 && g <= 100 && b <= 100, "register {i}");
        }
    }
}
