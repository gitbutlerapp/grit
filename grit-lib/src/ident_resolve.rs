//! Git-compatible author/committer identity resolution (see upstream `ident.c`).
//!
//! This module keeps environment access injectable so callers can test identity resolution
//! without mutating process-wide state.

use std::ffi::OsString;

use thiserror::Error;

#[cfg(unix)]
use crate::commit_encoding::decode_bytes;
use crate::config::ConfigSet;
use crate::ident_config::ident_default_name;

/// Environment access used for identity resolution.
pub trait IdentityEnv {
    /// Return a UTF-8 environment variable, if it exists and is valid Unicode.
    fn var(&self, key: &str) -> Option<String>;

    /// Return a raw environment variable value, preserving non-UTF-8 bytes on Unix.
    fn var_os(&self, key: &str) -> Option<OsString>;
}

/// Whether `GIT_AUTHOR_NAME` / `GIT_COMMITTER_NAME` is unset vs set (possibly empty).
///
/// Git treats a set-but-empty value as an explicit override: it must not fall through
/// to `user.name` or passwd/GECOS fallback (`t7518-ident-corner-cases`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitIdentityNameEnv {
    /// Variable is not present in the environment.
    Unset,
    /// Present after trimming whitespace (may be `""`).
    Set(String),
}

/// Author vs committer for `GIT_*` / `author.*` / `committer.*` lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentRole {
    /// Author identity.
    Author,
    /// Committer identity.
    Committer,
}

impl IdentRole {
    fn env_name_key(self) -> &'static str {
        match self {
            IdentRole::Author => "GIT_AUTHOR_NAME",
            IdentRole::Committer => "GIT_COMMITTER_NAME",
        }
    }

    fn env_email_key(self) -> &'static str {
        match self {
            IdentRole::Author => "GIT_AUTHOR_EMAIL",
            IdentRole::Committer => "GIT_COMMITTER_EMAIL",
        }
    }

    fn config_name_key(self) -> &'static str {
        match self {
            IdentRole::Author => "author.name",
            IdentRole::Committer => "committer.name",
        }
    }

    fn config_email_key(self) -> &'static str {
        match self {
            IdentRole::Author => "author.email",
            IdentRole::Committer => "committer.email",
        }
    }

    /// Heading Git prints before identity setup advice.
    #[must_use]
    pub fn missing_email_hint(self) -> &'static str {
        match self {
            IdentRole::Author => "Author identity unknown",
            IdentRole::Committer => "Committer identity unknown",
        }
    }
}

/// Errors returned by strict identity resolution.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum IdentityError {
    /// `user.useConfigOnly` disables email auto-detection and no config email was provided.
    ///
    /// The Display text here is a terse, functional description of the
    /// condition. The user-facing setup guidance is rendered by the
    /// (GPL-licensed) CLI layer, which maps this variant to its own message.
    #[error("email auto-detection is disabled (user.useConfigOnly) and no configured email is available")]
    AutoDetectionDisabled {
        /// Identity role being resolved.
        role: IdentRole,
    },
    /// Git rejects empty ident names.
    #[error("empty ident name (for <{email}>) not allowed")]
    EmptyName {
        /// Email address associated with the attempted identity.
        email: String,
        /// Identity role being resolved.
        role: IdentRole,
    },
    /// Git rejects names containing only "crud" characters.
    #[error("invalid ident name: '{name}'")]
    InvalidName {
        /// Rejected name.
        name: String,
    },
}

/// Read a `GIT_*_NAME` variable like Git's `getenv`: unset vs set, preserving explicit empty.
#[must_use]
pub fn read_git_identity_name_env_with<E: IdentityEnv>(env: &E, key: &str) -> GitIdentityNameEnv {
    let Some(os) = env.var_os(key) else {
        return GitIdentityNameEnv::Unset;
    };
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = os.as_bytes();
        let s = if std::str::from_utf8(bytes).is_ok() {
            String::from_utf8_lossy(bytes).into_owned()
        } else {
            decode_bytes(Some("ISO8859-1"), bytes)
        };
        GitIdentityNameEnv::Set(s.trim().to_owned())
    }
    #[cfg(not(unix))]
    {
        let s = os.to_str().map(|t| t.trim().to_owned()).unwrap_or_default();
        GitIdentityNameEnv::Set(s)
    }
}

/// Read `GIT_AUTHOR_NAME` / `GIT_COMMITTER_NAME` from the supplied environment.
///
/// Returns [`None`] when the variable is unset or set to whitespace only. A set-but-empty
/// value (after trim) is still [`None`] here; use [`read_git_identity_name_env_with`] when the
/// distinction matters.
#[must_use]
pub fn read_git_identity_name_from_env_with<E: IdentityEnv>(env: &E, key: &str) -> Option<String> {
    match read_git_identity_name_env_with(env, key) {
        GitIdentityNameEnv::Unset => None,
        GitIdentityNameEnv::Set(s) if s.is_empty() => None,
        GitIdentityNameEnv::Set(s) => Some(s),
    }
}

fn use_config_only(config: &ConfigSet) -> bool {
    match config.get_bool("user.useConfigOnly") {
        Some(Ok(b)) => b,
        Some(Err(_)) => false,
        None => false,
    }
}

fn config_mail_given(config: &ConfigSet) -> bool {
    ["user.email", "author.email", "committer.email"]
        .iter()
        .any(|key| config.get(key).is_some_and(|v| !v.trim().is_empty()))
}

fn ident_name_has_non_crud(name: &str) -> bool {
    name.chars().any(|c| {
        let o = c as u32;
        !(o <= 32
            || c == ','
            || c == ':'
            || c == ';'
            || c == '<'
            || c == '>'
            || c == '"'
            || c == '\\'
            || c == '\'')
    })
}

fn synthetic_email_with<E: IdentityEnv>(env: &E) -> String {
    let user = env
        .var("USER")
        .or_else(|| env.var("USERNAME"))
        .unwrap_or_else(|| "unknown".to_owned());
    let host = env.var("HOSTNAME").unwrap_or_else(|| "unknown".to_owned());
    let domain = if host.contains('.') {
        host
    } else {
        format!("{host}.(none)")
    };
    format!("{user}@{domain}")
}

fn resolve_email_inner_with<E: IdentityEnv>(
    env: &E,
    config: &ConfigSet,
    role: IdentRole,
    honor_use_config_only: bool,
) -> Result<String, IdentityError> {
    if let Some(v) = env.var(role.env_email_key()) {
        let t = v.trim();
        if !t.is_empty() {
            return Ok(t.to_owned());
        }
    }

    if let Some(v) = config.get(role.config_email_key()) {
        let t = v.trim();
        if !t.is_empty() {
            return Ok(t.to_owned());
        }
    }

    if let Some(v) = config.get("user.email") {
        let t = v.trim();
        if !t.is_empty() {
            return Ok(t.to_owned());
        }
    }

    if honor_use_config_only && use_config_only(config) && !config_mail_given(config) {
        return Err(IdentityError::AutoDetectionDisabled { role });
    }

    if let Some(v) = env.var("EMAIL") {
        let t = v.trim();
        if !t.is_empty() {
            return Ok(t.to_owned());
        }
    }

    Ok(synthetic_email_with(env))
}

/// Resolve email for a role when creating commits (honors `user.useConfigOnly`).
pub fn resolve_email_with<E: IdentityEnv>(
    env: &E,
    config: &ConfigSet,
    role: IdentRole,
) -> Result<String, IdentityError> {
    resolve_email_inner_with(env, config, role, true)
}

/// Resolve email without failing on `user.useConfigOnly` (e.g. `git var -l`, reflog-style).
#[must_use]
pub fn resolve_email_lenient_with<E: IdentityEnv>(
    env: &E,
    config: &ConfigSet,
    role: IdentRole,
) -> String {
    resolve_email_inner_with(env, config, role, false).unwrap_or_else(|_| synthetic_email_with(env))
}

/// Name from env and config without erroring (for `git var -l`).
#[must_use]
pub fn peek_name_with<E: IdentityEnv>(
    env: &E,
    config: &ConfigSet,
    role: IdentRole,
) -> Option<String> {
    match read_git_identity_name_env_with(env, role.env_name_key()) {
        GitIdentityNameEnv::Set(s) => {
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
        GitIdentityNameEnv::Unset => {
            if let Some(v) = config.get(role.config_name_key()) {
                let t = v.trim();
                if !t.is_empty() {
                    return Some(t.to_owned());
                }
            }
            let d = ident_default_name(config, env);
            if d.is_empty() {
                None
            } else {
                Some(d)
            }
        }
    }
}

/// Resolve name for a role when creating commits.
pub fn resolve_name_with<E: IdentityEnv>(
    env: &E,
    config: &ConfigSet,
    role: IdentRole,
) -> Result<String, IdentityError> {
    let email = resolve_email_inner_with(env, config, role, true)?;

    let name: String = match read_git_identity_name_env_with(env, role.env_name_key()) {
        GitIdentityNameEnv::Set(s) => s,
        GitIdentityNameEnv::Unset => {
            if let Some(v) = config.get(role.config_name_key()) {
                let t = v.trim();
                if !t.is_empty() {
                    t.to_owned()
                } else {
                    ident_default_name(config, env)
                }
            } else {
                ident_default_name(config, env)
            }
        }
    };

    if name.is_empty() {
        return Err(IdentityError::EmptyName { email, role });
    }

    if !ident_name_has_non_crud(&name) {
        return Err(IdentityError::InvalidName { name });
    }

    Ok(name)
}

/// Committer name/email for reflog and other non-strict contexts: never errors; always has an email.
#[must_use]
pub fn resolve_loose_committer_parts_with<E: IdentityEnv>(
    env: &E,
    config: &ConfigSet,
) -> (String, String) {
    let name = match read_git_identity_name_env_with(env, "GIT_COMMITTER_NAME") {
        GitIdentityNameEnv::Set(s) => {
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
        GitIdentityNameEnv::Unset => read_git_identity_name_from_env_with(env, "GIT_AUTHOR_NAME"),
    }
    .or_else(|| {
        config
            .get("committer.name")
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    })
    .or_else(|| {
        config
            .get("user.name")
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    })
    .or_else(|| {
        let d = ident_default_name(config, env);
        if d.is_empty() {
            None
        } else {
            Some(d)
        }
    })
    .unwrap_or_else(|| "Unknown".to_owned());

    let email = env
        .var("GIT_COMMITTER_EMAIL")
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            env.var("GIT_AUTHOR_EMAIL")
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            config
                .get("committer.email")
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            config
                .get("user.email")
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            env.var("EMAIL")
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| synthetic_email_with(env));

    (name, email)
}
