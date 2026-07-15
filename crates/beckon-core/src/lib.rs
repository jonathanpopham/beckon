//! beckon-core: the deterministic engine behind the beckon launcher.
//!
//! Everything in this crate is std only, allocation-honest, and byte
//! deterministic: same inputs, same outputs, on any platform. No floats in
//! ranking or money paths (fixed-point integers only), no FFI, no network,
//! no clocks read outside injected `now` parameters. The macOS shell is a
//! thin consumer of this crate; the engine is fully testable on Linux CI.
//!
//! Module map (each feature lives in its own file so parallel work does not
//! collide):
//!   fuzzy      subsequence matcher and scorer
//!   frecency   integer half-life usage ranking
//!   router     query parsing and command dispatch
//!   calc       calculator (fixed-point, units, bases, dates)
//!   clipstore  clipboard history model
//!   snippets   snippet store and placeholder expansion
//!   quicklinks parameterized links
//!   persist    canonical JSON codec and atomic file store

pub mod frecency;
pub mod fuzzy;
pub mod router;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_semver_shaped() {
        let parts: Vec<&str> = super::VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "expected major.minor.patch");
        for part in parts {
            part.parse::<u32>().expect("numeric version component");
        }
    }
}
