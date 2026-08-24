//! Pure sample-format conversion and channel downmix for capture backends.
//!
//! Nothing here depends on cpal: a callback hands over a slice of raw device
//! samples (`i16`, `u16`, `f32`, …) and a per-format scalar converter, and this
//! module turns them into the interleaved `f32` mono/stereo the ring expects.
//! Keeping it cpal-free makes the number-crunching unit-testable on any host
//! and lets the no-allocation test drive it directly.
//!
//! Every function is allocation-free after the caller has sized its output
//! buffer once; the capture callback preallocates that buffer at stream open
//! and this code only ever writes into it.

use crate::capture::SampleSink;

// --- Scalar sample converters -----------------------------------------------
//
// Each maps a device sample onto the `-1.0..=1.0` convention with `0.0` at the
// origin, matching cpal/dasp: signed formats divide by the magnitude of their
// most-negative value (so the origin is exact and the positive extreme lands
// one step short of `+1.0`); unsigned formats subtract their mid-scale origin
// first, then divide by that same magnitude.

/// Identity converter for streams already delivered as `f32`.
#[inline]
#[must_use]
pub fn f32_id(s: f32) -> f32 {
    s
}

/// `i16` → `f32`. `i16::MIN` → `-1.0`, `0` → `0.0`, `i16::MAX` → `32767/32768`.
#[inline]
#[must_use]
pub fn i16_to_f32(s: i16) -> f32 {
    s as f32 / 32_768.0
}

/// `u16` → `f32`. `0` → `-1.0`, `32768` → `0.0`, `u16::MAX` → `32767/32768`.
#[inline]
#[must_use]
pub fn u16_to_f32(s: u16) -> f32 {
    (s as f32 - 32_768.0) / 32_768.0
}

/// `i32` → `f32`. `i32::MIN` → `-1.0`, `0` → `0.0`, `i32::MAX` →
/// `2147483647/2147483648`.
#[inline]
#[must_use]
pub fn i32_to_f32(s: i32) -> f32 {
    s as f32 / 2_147_483_648.0
}

/// `u8` → `f32`. `0` → `-1.0`, `128` → `0.0`, `u8::MAX` → `127/128`.
#[inline]
#[must_use]
pub fn u8_to_f32(s: u8) -> f32 {
    (s as f32 - 128.0) / 128.0
}

// --- Channel downmix --------------------------------------------------------

/// Fixed channel-folding plan for one stream, decided once at open.
///
/// The ring and the whole DSP pipeline only ever see **mono or stereo**, so the
/// delivered channel count is `1` for a mono device and `2` for anything wider.
/// The fold rule for a device with `> 2` channels is:
///
/// - channel `0` is front-left, channel `1` is front-right;
/// - the remaining channels `2..N` (centre, LFE, surrounds, …) are averaged
///   into a single *rest* signal;
/// - that rest is mixed into **both** the left and right outputs at `-6 dB`
///   (a linear gain of `0.5`):
///   `left = ch0 + 0.5 * rest`, `right = ch1 + 0.5 * rest`.
///
/// A 1-channel device passes straight through as mono; a 2-channel device
/// passes straight through as stereo (no rest term). Outputs are left unclamped
/// — a hot multichannel fold can momentarily exceed `1.0`, which the downstream
/// RMS/peak stage reports faithfully rather than hiding.
#[derive(Clone, Copy, Debug)]
pub struct Downmix {
    /// Interleaved channel count the device delivers.
    pub device_channels: usize,
    /// Interleaved channel count pushed to the ring: `1` or `2`.
    pub out_channels: usize,
}

impl Downmix {
    /// Build the plan for a device delivering `device_channels` interleaved
    /// channels. The delivered (output) width is derived: `1` for a mono
    /// device, `2` for anything wider.
    #[must_use]
    pub fn new(device_channels: usize) -> Self {
        let device_channels = device_channels.max(1);
        let out_channels = if device_channels == 1 { 1 } else { 2 };
        Self {
            device_channels,
            out_channels,
        }
    }

    /// Convert and fold `input` (interleaved device samples) into `out`
    /// (interleaved `f32` at [`out_channels`]), returning the number of `f32`
    /// values written. `out` must hold at least `frames * out_channels` values,
    /// where `frames = input.len() / device_channels`. Allocation-free.
    ///
    /// [`out_channels`]: Downmix::out_channels
    pub fn mix_frames<T: Copy>(&self, input: &[T], conv: fn(T) -> f32, out: &mut [f32]) -> usize {
        let dc = self.device_channels;
        let frames = input.len() / dc;
        if self.out_channels == 1 {
            for (f, slot) in out[..frames].iter_mut().enumerate() {
                *slot = conv(input[f * dc]);
            }
            return frames;
        }
        for f in 0..frames {
            let base = f * dc;
            let left = conv(input[base]);
            let right = if dc >= 2 { conv(input[base + 1]) } else { left };
            let rest = if dc > 2 {
                let mut acc = 0.0f32;
                for c in 2..dc {
                    acc += conv(input[base + c]);
                }
                acc / (dc - 2) as f32
            } else {
                0.0
            };
            out[f * 2] = left + 0.5 * rest;
            out[f * 2 + 1] = right + 0.5 * rest;
        }
        frames * 2
    }
}

/// Convert `data` (interleaved device samples of type `T`) with `conv`, fold it
/// to mono/stereo per `downmix`, and push it to `sink`. Processes the callback
/// in chunks of at most `out.len() / downmix.out_channels` frames so a callback
/// larger than the preallocated `out` buffer is handled without ever
/// allocating. Allocation-, lock- and syscall-free on the audio thread.
pub fn convert_and_push<T: Copy>(
    data: &[T],
    conv: fn(T) -> f32,
    downmix: &Downmix,
    out: &mut [f32],
    sink: &mut SampleSink,
) {
    let dc = downmix.device_channels;
    let cap_frames = out.len() / downmix.out_channels;
    if dc == 0 || cap_frames == 0 {
        return;
    }
    let total_frames = data.len() / dc;
    let mut done = 0;
    while done < total_frames {
        let n = (total_frames - done).min(cap_frames);
        let produced = downmix.mix_frames(&data[done * dc..(done + n) * dc], conv, out);
        sink.push(&out[..produced]);
        done += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_i16_u16_to_f32() {
        // i16: exact origin and full-scale extremes.
        assert_eq!(i16_to_f32(i16::MIN), -1.0);
        assert_eq!(i16_to_f32(0), 0.0);
        assert_eq!(i16_to_f32(i16::MAX), 32_767.0 / 32_768.0);

        // u16: mid-scale is the origin; both rails map symmetrically.
        assert_eq!(u16_to_f32(0), -1.0);
        assert_eq!(u16_to_f32(32_768), 0.0);
        assert_eq!(u16_to_f32(u16::MAX), 32_767.0 / 32_768.0);

        // The cheap extras carry the same convention.
        assert_eq!(i32_to_f32(i32::MIN), -1.0);
        assert_eq!(i32_to_f32(0), 0.0);
        assert_eq!(u8_to_f32(0), -1.0);
        assert_eq!(u8_to_f32(128), 0.0);
        assert_eq!(u8_to_f32(u8::MAX), 127.0 / 128.0);
        assert_eq!(f32_id(0.25), 0.25);
    }

    #[test]
    fn downmix_rules() {
        // 1 channel -> mono passthrough.
        let dm = Downmix::new(1);
        assert_eq!(dm.out_channels, 1);
        let mut out = [0.0f32; 4];
        let n = dm.mix_frames(&[0.5f32, -0.25, 1.0, -1.0], f32_id, &mut out);
        assert_eq!(n, 4);
        assert_eq!(&out[..4], &[0.5, -0.25, 1.0, -1.0]);

        // 2 channels -> stereo passthrough (no rest term).
        let dm = Downmix::new(2);
        assert_eq!(dm.out_channels, 2);
        let mut out = [0.0f32; 4];
        let n = dm.mix_frames(&[0.1f32, 0.2, 0.3, 0.4], f32_id, &mut out);
        assert_eq!(n, 4);
        assert_eq!(&out[..4], &[0.1, 0.2, 0.3, 0.4]);

        // 6 channels [L, R, C, LFE, Ls, Rs] -> stereo with the rest folded in
        // at 0.5. rest = mean(C, LFE, Ls, Rs).
        let dm = Downmix::new(6);
        assert_eq!(dm.out_channels, 2);
        let frame = [0.2f32, -0.2, 0.4, 0.0, 0.8, -0.4];
        let rest = (0.4 + 0.0 + 0.8 - 0.4) / 4.0; // 0.2
        let mut out = [0.0f32; 2];
        let n = dm.mix_frames(&frame, f32_id, &mut out);
        assert_eq!(n, 2);
        assert!((out[0] - (0.2 + 0.5 * rest)).abs() < 1e-6);
        assert!((out[1] - (-0.2 + 0.5 * rest)).abs() < 1e-6);
    }
}
