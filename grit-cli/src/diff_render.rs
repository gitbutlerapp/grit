//! Compatibility re-exports — diff rendering lives in [`crate::render::diff`].

pub use crate::output::stdout_color_enabled as use_color;
pub use crate::render::diff::*;
