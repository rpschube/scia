//! Engine core for scia: system-audio capture, the lock-free sample ring,
//! the DSP stages that turn samples into spectra and onsets, and the feature
//! bus that fans those features out to consumers. This crate is the headless
//! heart of the project and carries no user-interface dependencies of any
//! kind — no terminal, GPU, windowing or scripting crates — so it can back a
//! TUI today and a GPU window or wallpaper mode later without change.

/// The crate name, resolved at compile time from Cargo metadata.
pub const NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_crate_name() {
        assert_eq!(NAME, "scia-core");
    }
}
