//! Kitty-graphics encoder tests: parse the encoder's own emitted APC stream and
//! assert the chunking, `m` flags and key set, then round-trip the payload
//! (base64 + zlib) back to the exact RGB bytes. No terminal involved.

use std::io::Read;

use flate2::read::ZlibDecoder;
use scia_tui::{KITTY_CLEANUP, KittyEncoder};

const CHUNK: usize = 4096;

/// One parsed APC chunk: its key string and raw (still base64) payload.
struct ApcChunk {
    keys: String,
    payload: Vec<u8>,
}

/// Find the first occurrence of `needle` in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Parse an APC stream (`ESC _ G <keys> ; <payload> ESC \` chunks) into its
/// chunks. Panics on malformed framing so a test fails loudly.
fn parse_apc(mut s: &[u8]) -> Vec<ApcChunk> {
    let mut out = Vec::new();
    while let Some(start) = find(s, b"\x1b_G") {
        let after = &s[start + 3..];
        let end = find(after, b"\x1b\\").expect("APC terminator ESC backslash");
        let body = &after[..end];
        let semi = body
            .iter()
            .position(|&b| b == b';')
            .expect("keys/payload separator ;");
        out.push(ApcChunk {
            keys: String::from_utf8(body[..semi].to_vec()).expect("keys are ASCII"),
            payload: body[semi + 1..].to_vec(),
        });
        s = &after[end + 2..];
    }
    out
}

/// Decode standard base64 (with padding) into bytes.
fn b64_decode(src: &[u8]) -> Vec<u8> {
    fn val(b: u8) -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some(u32::from(b - b'A')),
            b'a'..=b'z' => Some(u32::from(b - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(b - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &b in src {
        if b == b'=' {
            break;
        }
        let v = val(b).expect("valid base64 char");
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// Concatenate every chunk's payload, base64-decode and zlib-inflate it.
fn decode_payload(chunks: &[ApcChunk]) -> Vec<u8> {
    let mut b64 = Vec::new();
    for c in chunks {
        b64.extend_from_slice(&c.payload);
    }
    let deflated = b64_decode(&b64);
    let mut out = Vec::new();
    ZlibDecoder::new(&deflated[..])
        .read_to_end(&mut out)
        .expect("zlib inflate");
    out
}

/// The value of key `k` in an APC key string like `a=T,i=1,f=24,...`.
fn key_val<'a>(keys: &'a str, k: &str) -> Option<&'a str> {
    let prefix = format!("{k}=");
    keys.split(',').find_map(|kv| kv.strip_prefix(&prefix))
}

/// A deterministic RGB image with poorly-compressible content, so a large image
/// forces multiple transmission chunks.
fn noisy_rgb(w: u16, h: u16) -> Vec<u8> {
    let n = w as usize * h as usize * 3;
    let mut v = Vec::with_capacity(n);
    let mut state = 0x1234_5678u32;
    for _ in 0..n {
        // xorshift, so the bytes do not compress away to one chunk.
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        v.push((state & 0xff) as u8);
    }
    v
}

#[test]
fn single_chunk_keys_and_roundtrip() {
    // A small image fits in one chunk: full keys, m=0, payload round-trips.
    let (w, h) = (4u16, 2u16);
    let (cols, rows) = (10u16, 5u16);
    let rgb: Vec<u8> = (0..(w as usize * h as usize * 3))
        .map(|i| (i * 7 % 256) as u8)
        .collect();

    let mut enc = KittyEncoder::new();
    let mut out = Vec::new();
    enc.encode(&rgb, (w, h), (cols, rows), &mut out);

    let chunks = parse_apc(&out);
    assert_eq!(chunks.len(), 1, "small image is one chunk");
    let keys = &chunks[0].keys;
    assert_eq!(key_val(keys, "a"), Some("T"), "transmit+display");
    assert_eq!(key_val(keys, "i"), Some("1"), "fixed image id");
    assert_eq!(key_val(keys, "f"), Some("24"), "24-bit RGB");
    assert_eq!(
        key_val(keys, "s"),
        Some(w.to_string().as_str()),
        "pixel width"
    );
    assert_eq!(
        key_val(keys, "v"),
        Some(h.to_string().as_str()),
        "pixel height"
    );
    assert_eq!(key_val(keys, "c"), Some(cols.to_string().as_str()), "cols");
    assert_eq!(key_val(keys, "r"), Some(rows.to_string().as_str()), "rows");
    assert_eq!(key_val(keys, "z"), Some("-1"), "below the text layer");
    assert_eq!(key_val(keys, "q"), Some("2"), "quiet");
    assert_eq!(key_val(keys, "o"), Some("z"), "zlib payload");
    assert_eq!(key_val(keys, "m"), Some("0"), "last (only) chunk is m=0");

    assert_eq!(
        decode_payload(&chunks),
        rgb,
        "payload round-trips to the RGB"
    );
}

#[test]
fn large_image_chunks_with_correct_m_flags() {
    // A large, incompressible image spans several chunks.
    let (w, h) = (80u16, 80u16);
    let rgb = noisy_rgb(w, h);

    let mut enc = KittyEncoder::new();
    let mut out = Vec::new();
    enc.encode(&rgb, (w, h), (40, 20), &mut out);

    let chunks = parse_apc(&out);
    assert!(chunks.len() > 1, "incompressible image spans many chunks");

    for (i, c) in chunks.iter().enumerate() {
        assert!(
            c.payload.len() <= CHUNK,
            "chunk {i} payload {} exceeds {CHUNK}",
            c.payload.len()
        );
        let last = i + 1 == chunks.len();
        let expected_m = if last { "0" } else { "1" };
        assert_eq!(
            key_val(&c.keys, "m"),
            Some(expected_m),
            "chunk {i} m flag (last = {last})"
        );
        if i == 0 {
            assert_eq!(
                key_val(&c.keys, "a"),
                Some("T"),
                "first chunk has full keys"
            );
            assert_eq!(key_val(&c.keys, "f"), Some("24"));
        } else {
            // Continuation chunks carry only the m key.
            assert_eq!(c.keys, format!("m={expected_m}"), "continuation is m-only");
        }
    }

    // Every chunk but the last is exactly full.
    for c in &chunks[..chunks.len() - 1] {
        assert_eq!(c.payload.len(), CHUNK, "non-final chunks are full");
    }

    assert_eq!(
        decode_payload(&chunks),
        rgb,
        "payload round-trips to the RGB"
    );
}

#[test]
fn zero_sized_image_emits_nothing() {
    let mut enc = KittyEncoder::new();
    let mut out = vec![0xffu8; 4];
    enc.encode(&[], (0, 0), (0, 0), &mut out);
    assert!(out.is_empty(), "an empty image produces no output");
}

#[test]
fn cleanup_sequence_is_the_delete_apc() {
    assert_eq!(KITTY_CLEANUP, b"\x1b_Ga=d\x1b\\");
}
