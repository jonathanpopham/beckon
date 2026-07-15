//! beckon: a keyboard launcher for macOS. Free, open source, airgap pure.
//!
//! This crate is the platform shell: it owns the floating panel, the global
//! hotkey, the indexers, and the Accessibility integrations, all through
//! hand-rolled Objective-C runtime FFI (no wrapper crates). All decisions
//! about what a query means and how results rank live in beckon-core.
//!
//! On non-macOS targets this builds as a stub so the whole workspace
//! compiles and tests on Linux CI.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("beckon {}", beckon_core::VERSION);
        return;
    }

    #[cfg(target_os = "macos")]
    {
        println!(
            "beckon {}: shell not yet wired; see docs/PLAN.md",
            beckon_core::VERSION
        );
    }

    #[cfg(not(target_os = "macos"))]
    {
        eprintln!(
            "beckon {} core built OK; the shell runs on macOS only",
            beckon_core::VERSION
        );
    }
}
