//! Pathspec helpers for workspace tests (not part of the stable semver surface).

/// Returns whether `spec` carries long-form `:(attr:…)` magic (see [`crate::pathspec`]).
#[must_use]
pub fn pathspec_has_attr_magic(spec: &str) -> bool {
    crate::pathspec::pathspec_has_attr_magic(spec)
}
