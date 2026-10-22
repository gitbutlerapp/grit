//! Conventions for clap arguments that accept free-form user text.
//!
//! Values such as `-fix typo`, `-leading dash`, or `-Name <email>` must not be
//! parsed as flags. Set `allow_hyphen_values = true` on every option and
//! positional that accepts:
//!
//! - `-m` / `--message` and positional commit messages
//! - `--message-file` / `-F` (paths may start with `-`)
//! - `--author`, `--trailer`, and similar untrusted strings
//!
//! See GitHub issue #1519.
