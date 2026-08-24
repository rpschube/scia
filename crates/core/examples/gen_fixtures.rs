//! Regenerate the committed WAV fixtures for the golden-file DSP tests.
//!
//! Run with `cargo run --example gen_fixtures` (via `just`). The real work lives
//! in the shared `tests/support/fixtures.rs` module so `tests/golden.rs` can
//! regenerate the exact same bytes and prove the fixtures are deterministic.
//! Writes into `crates/core/tests/fixtures/`; the output is byte-identical on
//! every run and every platform. Re-run and commit only when a fixture's
//! definition intentionally changes (then re-bless the golden files).

#[path = "../tests/support/fixtures.rs"]
mod fixtures;

fn main() {
    let dir = fixtures::fixtures_dir();
    fixtures::generate_all(&dir);
    println!(
        "wrote {} fixtures to {}",
        fixtures::FIXTURES.len(),
        dir.display()
    );
}
