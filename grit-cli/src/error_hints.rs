//! Hint text helpers for structured errors; path formatting used when building failures.
//!
//! Rendering of errors lives in [`crate::render::error`].

pub use crate::error_map::{indented_paths, would_overwrite_hint};
