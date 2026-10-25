//! Hyprdeck's Proton layer: per-game profiles, runner variants and local GE-Proton builds.
//!
//! Kept free of GTK so it can be tested and reused by CI. See `docs/PROTON_BUILDS.md`.
pub mod builder;
pub mod profile;
pub mod recipe;
pub mod variant;
