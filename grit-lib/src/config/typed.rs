//! Typed config reads and canonical write values (`--type bool|int|path`).

use crate::environment::Environment;
use crate::error::{BadNumericSource, ConfigError, Error, Result};

use super::{
    canonical_key, parse_bool, parse_git_config_int_strict, parse_path, ConfigSet,
    GitConfigIntStrictError,
};

/// Value type for typed config operations (Git `--type` subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigValueType {
    /// Boolean (`true` / `false`).
    Bool,
    /// Signed integer with optional `k`/`m`/`g` suffix on input.
    Int,
    /// Path (tilde and environment expansion).
    Path,
}

/// A config value after typed parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedConfigValue {
    /// Parsed boolean.
    Bool(bool),
    /// Parsed integer.
    Int(i64),
    /// Expanded path string.
    Path(String),
}

impl TypedConfigValue {
    /// Canonical string form for writing to a config file (matches Git `--type` output).
    #[must_use]
    pub fn to_canonical_string(&self) -> String {
        match self {
            Self::Bool(b) => if *b { "true" } else { "false" }.to_owned(),
            Self::Int(n) => n.to_string(),
            Self::Path(p) => p.clone(),
        }
    }
}

/// Parse and canonicalize a value for writing with `--type`.
///
/// # Errors
///
/// Returns [`Error::Config`] when the value is not valid for the requested type.
pub fn canonicalize_config_write_value(
    key: &str,
    ty: ConfigValueType,
    value: &str,
    env: &Environment,
) -> Result<String> {
    let typed = parse_typed_value(key, ty, value, env)?;
    Ok(typed.to_canonical_string())
}

/// Parse a raw config string according to `ty` (for reads and writes).
///
/// # Errors
///
/// Returns [`Error::Config`] when the value is not valid for the requested type.
pub fn parse_typed_value(
    key: &str,
    ty: ConfigValueType,
    value: &str,
    env: &Environment,
) -> Result<TypedConfigValue> {
    match ty {
        ConfigValueType::Bool => parse_bool(value).map(TypedConfigValue::Bool).map_err(|_| {
            Error::Config(ConfigError::Other(format!(
                "bad boolean config value '{value}' for '{key}'"
            )))
        }),
        ConfigValueType::Int => parse_git_config_int_strict(value)
            .map(TypedConfigValue::Int)
            .map_err(|e| int_parse_error(key, value, e)),
        ConfigValueType::Path => Ok(TypedConfigValue::Path(parse_path(env, value))),
    }
}

fn int_parse_error(key: &str, value: &str, err: GitConfigIntStrictError) -> Error {
    let reason = match err {
        GitConfigIntStrictError::InvalidUnit => BadNumericSource::InvalidUnit,
        GitConfigIntStrictError::OutOfRange => BadNumericSource::OutOfRange,
    };
    Error::Config(ConfigError::BadNumericValue {
        key: key.to_owned(),
        value: value.to_owned(),
        reason,
    })
}

impl ConfigSet {
    /// Read the winning value for `key`, parsed as `ty`.
    ///
    /// Bare boolean keys are treated as `true`. Missing keys return `Ok(None)`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when `key` is invalid or the stored value does not match `ty`.
    pub fn get_typed(
        &self,
        key: &str,
        ty: ConfigValueType,
        env: &Environment,
    ) -> Result<Option<TypedConfigValue>> {
        let _canon = canonical_key(key)?;
        let Some(entry) = self.get_last_entry(key) else {
            return Ok(None);
        };
        let raw = entry.value.as_deref().unwrap_or("true");
        parse_typed_value(key, ty, raw, env).map(Some)
    }

    /// Entries whose canonical key starts with `prefix`.
    ///
    /// `prefix` may be a full key, a section (`remote.origin`), or a section prefix ending in `.`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when `prefix` is empty or invalid.
    pub fn entries_with_prefix(&self, prefix: &str) -> Result<Vec<super::ConfigEntry>> {
        let needle = list_prefix_needle(prefix)?;
        Ok(self
            .entries()
            .iter()
            .filter(|e| {
                if needle.is_empty() {
                    true
                } else {
                    e.key.starts_with(&needle)
                }
            })
            .cloned()
            .collect())
    }
}

fn list_prefix_needle(prefix: &str) -> Result<String> {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return Err(Error::Config(ConfigError::Other(
            "list prefix must not be empty".into(),
        )));
    }
    if let Ok(canon) = canonical_key(prefix) {
        return Ok(canon);
    }
    let section = prefix.trim_end_matches('.');
    if section.is_empty() {
        return Err(Error::Config(ConfigError::Other(
            "list prefix must not be empty".into(),
        )));
    }
    Ok(format!("{section}."))
}
