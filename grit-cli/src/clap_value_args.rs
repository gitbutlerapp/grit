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
//!
//! An optional positional that accepts hyphen values would also swallow any
//! unknown or misspelled flag, so free-text positionals use
//! [`FreeTextPositional`]: a hyphen-leading single token (`-q`, `--bogus`,
//! `--opt=value`) is rejected as an unexpected argument, while text with
//! whitespace (`"-leading dash message"`) is accepted. See GitHub issue #1906.

use std::ffi::OsStr;

use clap::builder::{StyledStr, TypedValueParser};
use clap::error::{ContextKind, ContextValue, Error, ErrorKind};
use clap::{Arg, Command};

/// Value parser for optional free-text positionals declared with
/// `allow_hyphen_values = true`.
#[derive(Clone, Copy, Debug, Default)]
pub struct FreeTextPositional {
    /// Option that takes the same value without the flag ambiguity (e.g. `-m`).
    pub flag_hint: &'static str,
}

impl FreeTextPositional {
    /// Parser whose rejection tip points at `flag_hint` (e.g. `"-m"`).
    #[must_use]
    pub const fn new(flag_hint: &'static str) -> Self {
        Self { flag_hint }
    }
}

/// Whether `value` is a single hyphen-leading token that reads as a flag.
fn looks_like_flag(value: &str) -> bool {
    value.len() > 1 && value.starts_with('-') && !value.chars().any(char::is_whitespace)
}

impl TypedValueParser for FreeTextPositional {
    type Value = String;

    fn parse_ref(&self, cmd: &Command, _arg: Option<&Arg>, value: &OsStr) -> Result<String, Error> {
        let Some(text) = value.to_str() else {
            return Err(Error::new(ErrorKind::InvalidUtf8).with_cmd(cmd));
        };
        if !looks_like_flag(text) {
            return Ok(text.to_owned());
        }
        let mut err = Error::new(ErrorKind::UnknownArgument).with_cmd(cmd);
        err.insert(
            ContextKind::InvalidArg,
            ContextValue::String(text.to_owned()),
        );
        err.insert(
            ContextKind::Suggested,
            ContextValue::StyledStrs(vec![StyledStr::from(format!(
                "to use '{text}' as text, pass it with '{} {text}'",
                self.flag_hint
            ))]),
        );
        err.insert(
            ContextKind::Usage,
            ContextValue::StyledStr(cmd.clone().render_usage()),
        );
        Err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::looks_like_flag;

    #[test]
    fn single_hyphen_tokens_look_like_flags() {
        for value in ["-q", "--bogus", "--verbose", "-abc", "--opt=value", "-1"] {
            assert!(looks_like_flag(value), "{value}");
        }
    }

    #[test]
    fn free_text_is_not_a_flag() {
        for value in ["-", "fix typo", "-leading dash message", "--- notes", "msg"] {
            assert!(!looks_like_flag(value), "{value}");
        }
    }
}
