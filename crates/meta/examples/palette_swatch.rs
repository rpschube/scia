//! Album-art palette A/B: old population scoring vs new vibrancy scoring.
//!
//! ```text
//! palette_swatch <image> [<image> ...]
//! ```
//!
//! For each image path it decodes the artwork once and prints two palettes side
//! by side — [`Scoring::Population`] (the pre-vibrancy ranking, which lets large
//! dull regions win) above [`Scoring::Vibrancy`] (the salience ranking that
//! biases toward the colours that perceptually define the art). Each colour is
//! shown as an ANSI truecolour swatch with its hex value, so a maintainer can
//! eyeball how much more vibrant the new palette reads on real covers.
//!
//! This is a print-only diagnostic: it reads the image files it is handed and
//! writes only to stdout/stderr. It never writes a file, touches the network,
//! or mutates anything. A terminal that does not support 24-bit colour will
//! show the hex values without the colour blocks.

use std::process::ExitCode;

use scia_meta::palette::{ArtPalette, Scoring, extract_scored};

/// A 3-cell truecolour block in the given sRGB colour.
fn block(c: [u8; 3]) -> String {
    format!("\x1b[48;2;{};{};{}m   \x1b[0m", c[0], c[1], c[2])
}

/// A colour block followed by its hex value drawn in that colour.
fn labeled(c: [u8; 3]) -> String {
    format!(
        "{} \x1b[38;2;{};{};{}m#{:02x}{:02x}{:02x}\x1b[0m",
        block(c),
        c[0],
        c[1],
        c[2],
        c[0],
        c[1],
        c[2],
    )
}

/// Print one scoring's palette as a small block of labelled swatch rows.
fn print_palette(tag: &str, pal: &ArtPalette) {
    println!("  {tag}");
    println!("    dominant  {}", labeled(pal.dominant));
    if pal.accents.is_empty() {
        println!("    accents   (none)");
    } else {
        let accents: Vec<String> = pal.accents.iter().map(|c| labeled(*c)).collect();
        println!("    accents   {}", accents.join("  "));
    }
    println!(
        "    light/dk  {}  {}",
        labeled(pal.light),
        labeled(pal.dark)
    );
    let slots: Vec<String> = pal.slots.iter().map(|c| block(*c)).collect();
    println!("    slots     {}", slots.join(""));
}

fn main() -> ExitCode {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: palette_swatch <image> [<image> ...]");
        eprintln!("prints old population-scoring vs new vibrancy-scoring palettes");
        return ExitCode::FAILURE;
    }

    let mut had_error = false;
    for path in &paths {
        println!("=== {path} ===");
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("  cannot read {path}: {e}");
                had_error = true;
                continue;
            }
        };
        let pop = match extract_scored(&bytes, Scoring::Population) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("  cannot extract {path}: {e}");
                had_error = true;
                continue;
            }
        };
        // Decoding succeeded above, so the vibrancy pass cannot newly fail.
        let vib = extract_scored(&bytes, Scoring::Vibrancy).expect("vibrancy extract");
        print_palette("population (old)", &pop);
        print_palette("vibrancy   (new)", &vib);
        println!();
    }

    if had_error {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
