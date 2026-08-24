//! `scia-bridge` companion binary.
//!
//! A small helper process that will front platform capture or metadata
//! sources for the main application. For now it reports its name and version.

fn main() {
    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
}
