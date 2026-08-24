//! The kitty graphics protocol encoder.
//!
//! Ghostty (and kitty) render images placed with the terminal graphics protocol
//! [1]. This encoder transmits one full RGB frame per call: there are no
//! animation frames, so every frame is a full retransmission that replaces the
//! previous image and its placement. A single fixed image id `i=1` with action
//! `a=T` (transmit + display) is reused, so each frame supersedes the last.
//!
//! The image data is zlib-compressed (`o=z`), base64-encoded, and split into
//! APC-framed chunks of at most [`CHUNK`] payload bytes each. Every chunk but the
//! last carries `m=1`; the last carries `m=0`. The first chunk carries the full
//! key set; continuation chunks carry only `m`.
//!
//! Framing per chunk: `ESC _ G <keys> ; <payload> ESC \`.
//!
//! Everything is written into caller-provided reusable buffers, and the internal
//! zlib compressor is reused across calls, so a warm encoder allocates nothing.
//!
//! [1]: https://sw.kovidgoyal.net/kitty/graphics-protocol/

use std::io::Write as _;

use flate2::{Compress, Compression, FlushCompress, Status};

/// Maximum payload (base64) bytes per APC chunk, per the protocol.
pub const CHUNK: usize = 4096;

/// The cleanup sequence: delete all images. Written on quit/suspend so no image
/// is left placed when the alternate screen is torn down.
pub const CLEANUP: &[u8] = b"\x1b_Ga=d\x1b\\";

/// Standard base64 alphabet.
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// A reusable kitty-graphics frame encoder.
///
/// Holds the zlib compressor and its scratch buffers across calls, so
/// [`encode`](Self::encode) does not allocate once the buffers have grown to a
/// steady frame size.
pub struct KittyEncoder {
    compress: Compress,
    /// zlib-compressed image bytes.
    zlib: Vec<u8>,
    /// base64 of `zlib`.
    b64: Vec<u8>,
}

impl Default for KittyEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl KittyEncoder {
    /// A fresh encoder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            compress: Compress::new(Compression::fast(), true),
            zlib: Vec::new(),
            b64: Vec::new(),
        }
    }

    /// Encode one full RGB frame into `out` (cleared first) as a sequence of
    /// APC-framed chunks ready to write to the terminal.
    ///
    /// `rgb` is `px.0 * px.1 * 3` row-major RGB8 bytes. `px` is the image size in
    /// pixels `(width, height)`; `cells` is the on-screen placement in terminal
    /// cells `(cols, rows)`. A zero-sized image produces no output.
    pub fn encode(&mut self, rgb: &[u8], px: (u16, u16), cells: (u16, u16), out: &mut Vec<u8>) {
        out.clear();
        let (w, h) = px;
        let (cols, rows) = cells;
        if w == 0 || h == 0 || rgb.is_empty() {
            return;
        }

        self.compress_rgb(rgb);
        base64_into(&self.zlib, &mut self.b64);

        let total = self.b64.len();
        let n_chunks = total.div_ceil(CHUNK).max(1);
        for i in 0..n_chunks {
            let start = i * CHUNK;
            let end = (start + CHUNK).min(total);
            let more = i + 1 < n_chunks;
            let m = u8::from(more);
            out.extend_from_slice(b"\x1b_G");
            if i == 0 {
                // First chunk: the full key set. `f=24` is 24-bit RGB, `o=z` is
                // zlib payload, `z=-1` places the image below the text layer,
                // `q=2` suppresses the terminal's responses, `c`/`r` scale it to
                // the cell area.
                let _ = write!(
                    out,
                    "a=T,i=1,f=24,s={w},v={h},c={cols},r={rows},z=-1,q=2,o=z,m={m}"
                );
            } else {
                let _ = write!(out, "m={m}");
            }
            out.push(b';');
            out.extend_from_slice(&self.b64[start..end]);
            out.extend_from_slice(b"\x1b\\");
        }
    }

    /// zlib-compress `rgb` into `self.zlib`, reusing the compressor and its
    /// output buffer. Pre-reserves the deflate worst-case bound so the common
    /// case is a single compress call into existing capacity.
    fn compress_rgb(&mut self, rgb: &[u8]) {
        // Deflate's absolute worst-case expansion is a few bytes per 16 KiB
        // block plus the zlib header; a generous margin keeps it single-shot.
        let bound = rgb.len() + rgb.len() / 1000 + 64;
        if self.zlib.capacity() < bound {
            let add = bound - self.zlib.len();
            self.zlib.reserve(add);
        }
        self.zlib.clear();
        self.compress.reset();
        loop {
            let consumed = self.compress.total_in() as usize;
            let status = self
                .compress
                .compress_vec(&rgb[consumed..], &mut self.zlib, FlushCompress::Finish)
                .expect("zlib compression of an in-memory buffer cannot fail");
            match status {
                Status::StreamEnd => break,
                Status::Ok | Status::BufError => {
                    // Give the compressor more room if it ran out; otherwise the
                    // next iteration consumes the rest of the input.
                    if self.zlib.len() == self.zlib.capacity() {
                        self.zlib.reserve(self.zlib.capacity().max(64));
                    }
                }
            }
        }
    }
}

/// Base64-encode `src` into `out` (cleared first), retaining `out`'s capacity.
fn base64_into(src: &[u8], out: &mut Vec<u8>) {
    out.clear();
    let mut chunks = src.chunks_exact(3);
    for c in &mut chunks {
        let n = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        out.push(B64[(n >> 18 & 63) as usize]);
        out.push(B64[(n >> 12 & 63) as usize]);
        out.push(B64[(n >> 6 & 63) as usize]);
        out.push(B64[(n & 63) as usize]);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = u32::from(rem[0]) << 16;
            out.push(B64[(n >> 18 & 63) as usize]);
            out.push(B64[(n >> 12 & 63) as usize]);
            out.push(b'=');
            out.push(b'=');
        }
        2 => {
            let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            out.push(B64[(n >> 18 & 63) as usize]);
            out.push(B64[(n >> 12 & 63) as usize]);
            out.push(B64[(n >> 6 & 63) as usize]);
            out.push(b'=');
        }
        _ => {}
    }
}
