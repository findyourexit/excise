//! The operating systems the harness distinguishes.
//!
//! A scenario restricts where it runs (`platforms`) and where an expected failure applies
//! (`fails_on`); the headless oracle's expectations restrict where a known discrepancy applies.
//! Both read this one list, so a platform is added in one place.

/// The operating systems a scenario or an expected failure can name, as `std::env::consts::OS`
/// spells them.
pub(crate) const PLATFORMS: [&str; 3] = ["linux", "macos", "windows"];
