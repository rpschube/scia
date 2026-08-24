//! core
pub const NAME: \&str = env!("CARGO_PKG_NAME");
#[cfg(test)]
mod tests { #[test] fn must_fail() { assert_eq!(1, 2); } }
