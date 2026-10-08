//! Git-style diagnostic prefixes without embedding `fatal:` / `warning:` literals in call sites.
//!
//! Library code records structured messages; binaries add the traditional prefix when printing.

/// Prefix a body with Git's `warning: ` diagnostic marker.
#[must_use]
pub fn warning_line(body: &str) -> String {
    ["warning", ": ", body].concat()
}

/// Prefix a body with Git's `hint: ` diagnostic marker.
#[must_use]
pub fn hint_line(body: &str) -> String {
    ["hint", ": ", body].concat()
}

/// Prefix a body with Git's `error: ` diagnostic marker.
#[must_use]
pub fn error_line(body: &str) -> String {
    ["error", ": ", body].concat()
}

/// Prefix a body with Git's `fatal: ` diagnostic marker.
#[must_use]
pub fn fatal_line(body: &str) -> String {
    ["fatal", ": ", body].concat()
}
