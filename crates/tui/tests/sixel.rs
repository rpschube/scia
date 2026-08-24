//! Sixel-encoder tests: parse the encoder's own emitted DCS stream with a small
//! hand-rolled sixel decoder and assert the framing, raster attributes, palette
//! definitions, run-length decoding and pixel-repeat. No terminal involved.

mod support {
    pub mod alloc_watch;
}

use scia_scenes::{Blend, Canvas, Palette, Style};
use scia_tui::{PixelBuffer, SIXEL_REGISTERS, SixelEncoder, sixel_quantize};

use support::alloc_watch::{CountingAllocator, watch};

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// A decoded sixel image: emit size and a row-major grid of register indices
/// (`-1` for a pixel no register wrote).
struct Decoded {
    w: u32,
    h: u32,
    /// `w * h` register indices, `grid[y * w + x]`.
    grid: Vec<i32>,
    /// Palette definitions seen: `palette[i] = Some((r, g, b))` on sixel's
    /// `0..=100` scale.
    palette: Vec<Option<(u16, u16, u16)>>,
}

impl Decoded {
    fn at(&self, x: u32, y: u32) -> i32 {
        self.grid[(y * self.w + x) as usize]
    }
}

/// Read a run of ASCII digits starting at `i`, returning the value and the index
/// just past it.
fn read_num(s: &[u8], mut i: usize) -> (u32, usize) {
    let mut n = 0u32;
    while i < s.len() && s[i].is_ascii_digit() {
        n = n * 10 + u32::from(s[i] - b'0');
        i += 1;
    }
    (n, i)
}

/// Parse a full sixel DCS stream. Panics on malformed framing so a test fails
/// loudly.
fn decode(s: &[u8]) -> Decoded {
    assert!(
        s.starts_with(b"\x1bP0;0;8q"),
        "DCS introducer `ESC P 0;0;8 q`"
    );
    let mut i = "\x1bP0;0;8q".len();

    // Raster attributes: `"Pan;Pad;Ph;Pv`.
    assert_eq!(s[i], b'"', "raster attributes begin with a quote");
    i += 1;
    let mut fields = Vec::new();
    while i < s.len() && (s[i].is_ascii_digit() || s[i] == b';') {
        if s[i] == b';' {
            i += 1;
            continue;
        }
        let (n, j) = read_num(s, i);
        fields.push(n);
        i = j;
    }
    assert_eq!(fields.len(), 4, "raster attributes are Pan;Pad;Ph;Pv");
    assert_eq!(fields[0], 1, "Pan = 1");
    assert_eq!(fields[1], 1, "Pad = 1");
    let (w, h) = (fields[2], fields[3]);

    let mut grid = vec![-1i32; (w * h) as usize];
    let mut palette = vec![None; SIXEL_REGISTERS];
    let mut cur_reg: i32 = -1;
    let mut x: u32 = 0;
    let mut band: u32 = 0;

    let apply = |grid: &mut [i32], reg: i32, x: u32, mask: u8, band: u32| {
        for row in 0..6u32 {
            if mask & (1u8 << row) != 0 {
                let y = band * 6 + row;
                if y < h && x < w {
                    grid[(y * w + x) as usize] = reg;
                }
            }
        }
    };

    while i < s.len() {
        match s[i] {
            0x1b => {
                assert_eq!(s.get(i + 1), Some(&b'\\'), "stream ends with ESC backslash");
                break;
            }
            b'#' => {
                let (n, j) = read_num(s, i + 1);
                if s.get(j) == Some(&b';') {
                    // Palette definition `#n;2;r;g;b`.
                    let mut nums = Vec::new();
                    let mut k = j;
                    while s.get(k) == Some(&b';') {
                        let (v, m) = read_num(s, k + 1);
                        nums.push(v);
                        k = m;
                    }
                    assert_eq!(nums.len(), 4, "palette def is `;2;r;g;b`");
                    assert_eq!(nums[0], 2, "colour space 2 = RGB");
                    palette[n as usize] = Some((nums[1] as u16, nums[2] as u16, nums[3] as u16));
                    i = k;
                } else {
                    // Colour selection.
                    cur_reg = n as i32;
                    i = j;
                }
            }
            b'!' => {
                let (count, j) = read_num(s, i + 1);
                let ch = s[j];
                assert!((0x3F..=0x7E).contains(&ch), "repeat targets a sixel byte");
                let mask = ch - 0x3F;
                for _ in 0..count {
                    apply(&mut grid, cur_reg, x, mask, band);
                    x += 1;
                }
                i = j + 1;
            }
            b'$' => {
                x = 0;
                i += 1;
            }
            b'-' => {
                x = 0;
                band += 1;
                i += 1;
            }
            ch @ 0x3F..=0x7E => {
                apply(&mut grid, cur_reg, x, ch - 0x3F, band);
                x += 1;
                i += 1;
            }
            other => panic!("unexpected sixel byte {other:#x} at {i}"),
        }
    }

    Decoded {
        w,
        h,
        grid,
        palette,
    }
}

/// A flat RGB8 image of `w × h` built from a per-pixel colour function.
fn build_rgb(w: u16, h: u16, f: impl Fn(u16, u16) -> (u8, u8, u8)) -> Vec<u8> {
    let mut v = Vec::with_capacity(w as usize * h as usize * 3);
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = f(x, y);
            v.extend_from_slice(&[r, g, b]);
        }
    }
    v
}

#[test]
fn dcs_framing_and_raster_dims() {
    let (w, h) = (5u16, 4u16);
    let rgb = build_rgb(w, h, |x, _| ((x * 40) as u8, 0, 0));
    let mut enc = SixelEncoder::new();
    let mut out = Vec::new();
    enc.encode(&rgb, (w, h), 1, &mut out);

    assert!(out.starts_with(b"\x1bP0;0;8q"), "DCS introducer present");
    assert!(out.ends_with(b"\x1b\\"), "ST terminator present");
    let dec = decode(&out);
    assert_eq!(
        (dec.w, dec.h),
        (u32::from(w), u32::from(h)),
        "raster = w×h at k=1"
    );
}

#[test]
fn palette_definitions_are_present_and_in_scale() {
    let rgb = build_rgb(3, 3, |_, _| (200, 100, 50));
    let mut enc = SixelEncoder::new();
    let mut out = Vec::new();
    enc.encode(&rgb, (3, 3), 1, &mut out);
    let dec = decode(&out);

    // Every register in the fixed cube is defined, every frame.
    let defined = dec.palette.iter().filter(|p| p.is_some()).count();
    assert_eq!(defined, SIXEL_REGISTERS, "all 252 registers are defined");
    for (i, p) in dec.palette.iter().enumerate() {
        let (r, g, b) = p.expect("register defined");
        assert!(
            r <= 100 && g <= 100 && b <= 100,
            "register {i} within 0..=100"
        );
    }
}

#[test]
fn two_colour_image_reproduces_its_layout_and_registers() {
    // Left half pure red, right half pure blue, across several bands.
    let (w, h) = (8u16, 10u16);
    let rgb = build_rgb(
        w,
        h,
        |x, _| {
            if x < w / 2 { (255, 0, 0) } else { (0, 0, 255) }
        },
    );
    let mut enc = SixelEncoder::new();
    let mut out = Vec::new();
    enc.encode(&rgb, (w, h), 1, &mut out);
    let dec = decode(&out);

    let red = i32::from(sixel_quantize(255, 0, 0));
    let blue = i32::from(sixel_quantize(0, 0, 255));
    assert_ne!(red, blue, "the two colours map to different registers");
    for y in 0..u32::from(h) {
        for x in 0..u32::from(w) {
            let expected = if x < u32::from(w) / 2 { red } else { blue };
            assert_eq!(dec.at(x, y), expected, "pixel ({x},{y})");
        }
    }
}

#[test]
fn rle_expands_to_exactly_w_by_h() {
    // A diagonal gradient exercises varied runs; every pixel must decode to the
    // quantized source colour, and the grid is exactly w×h with no gaps.
    let (w, h) = (12u16, 9u16);
    let rgb = build_rgb(w, h, |x, y| {
        let r = (x as u32 * 255 / 11) as u8;
        let g = (y as u32 * 255 / 8) as u8;
        (r, g, 128)
    });
    let mut enc = SixelEncoder::new();
    let mut out = Vec::new();
    enc.encode(&rgb, (w, h), 1, &mut out);
    let dec = decode(&out);

    assert_eq!((dec.w, dec.h), (u32::from(w), u32::from(h)));
    for y in 0..u32::from(h) {
        for x in 0..u32::from(w) {
            let o = ((y * u32::from(w) + x) * 3) as usize;
            let expected = i32::from(sixel_quantize(rgb[o], rgb[o + 1], rgb[o + 2]));
            assert_eq!(dec.at(x, y), expected, "pixel ({x},{y}) has no gap");
        }
    }
}

#[test]
fn pixel_repeat_expands_dimensions() {
    let (w, h) = (4u16, 3u16);
    let k = 3u16;
    let rgb = build_rgb(w, h, |x, y| ((x * 60) as u8, (y * 80) as u8, 20));
    let mut enc = SixelEncoder::new();
    let mut out = Vec::new();
    enc.encode(&rgb, (w, h), k, &mut out);
    let dec = decode(&out);

    assert_eq!(
        (dec.w, dec.h),
        (u32::from(w) * u32::from(k), u32::from(h) * u32::from(k)),
        "emit size is (w·k)×(h·k)"
    );
    // Each source pixel spans a k×k block of emit pixels.
    for ey in 0..dec.h {
        for ex in 0..dec.w {
            let sx = ex / u32::from(k);
            let sy = ey / u32::from(k);
            let o = ((sy * u32::from(w) + sx) * 3) as usize;
            let expected = i32::from(sixel_quantize(rgb[o], rgb[o + 1], rgb[o + 2]));
            assert_eq!(dec.at(ex, ey), expected, "emit pixel ({ex},{ey})");
        }
    }
}

#[test]
fn quantize_hits_known_corners() {
    assert_eq!(sixel_quantize(0, 0, 0), 0, "black");
    assert_eq!(sixel_quantize(255, 255, 255), 251, "white");
    assert_eq!(sixel_quantize(255, 0, 0), 210, "pure red");
    assert_eq!(sixel_quantize(0, 255, 0), 36, "pure green");
    assert_eq!(sixel_quantize(0, 0, 255), 5, "pure blue");
    assert_eq!(sixel_quantize(128, 128, 128), 147, "mid grey");
}

#[test]
fn zero_sized_image_emits_nothing() {
    let mut enc = SixelEncoder::new();
    let mut out = vec![0xffu8; 4];
    enc.encode(&[], (0, 0), 1, &mut out);
    assert!(out.is_empty(), "an empty image produces no output");
}

/// Build a representative scene into a canvas, matching the pixel no-alloc test.
fn build_scene(canvas: &mut Canvas) {
    for i in 0..16u16 {
        let x = i as f32 / 16.0;
        canvas.bar(x, 0.2, 0.05, 0.6, Style::new(1, 0.9));
    }
    canvas.line(0.0, 0.0, 1.0, 1.0, 0.02, Style::new(4, 1.0));
    canvas.point(0.5, 0.5, 0.1, Style::new(2, 0.7));
    canvas.field(4, 4, &[0.5; 16], Style::new(3, 0.6));
}

#[test]
fn warm_rasterize_and_encode_frame_does_not_allocate() {
    let palette = Palette::default_dark();
    let mut px = PixelBuffer::new();
    px.resize(320, 200);
    px.set_cells(32, 20);
    let mut canvas = Canvas::new(1.6);
    build_scene(&mut canvas);

    let mut rgb: Vec<u8> = Vec::new();
    let mut encoder = SixelEncoder::new();
    let mut out: Vec<u8> = Vec::new();

    // Warm up: grow every backing store (pixel arena, rgb8, band scratch, DCS
    // output) to steady state, mirroring the presenter's per-frame sequence.
    for _ in 0..8 {
        px.clear();
        px.rasterize(&canvas, &palette, Blend::Over, 1.0);
        px.write_rgb8(&mut rgb);
        encoder.encode(&rgb, px.dims(), 2, &mut out);
    }

    let ((), stray_count, strays) = watch(|| {
        for _ in 0..50 {
            px.clear();
            px.rasterize(&canvas, &palette, Blend::Over, 1.0);
            px.write_rgb8(&mut rgb);
            encoder.encode(&rgb, px.dims(), 2, &mut out);
        }
    });
    assert!(
        stray_count == 0,
        "warm rasterize+encode allocated {stray_count} time(s):\n{}",
        strays.join("\n---\n")
    );
}
