//! Version probing and Camoufox-specific compatibility layer.
//!
//! This module detects whether the connected browser is a Camoufox build
//! (as opposed to a vanilla Playwright-patched Firefox) and provides
//! method availability checks for version-specific features.

pub mod camoufox;

pub use camoufox::CamoufoxInfo;
