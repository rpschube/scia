//! `scia` command-line entry point.
//!
//! For now this simply reports the binary's name and version; the terminal
//! frontend and engine wiring land in later work.

fn main() {
    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
}
