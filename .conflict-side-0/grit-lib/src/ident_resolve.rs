//! Git-compatible author/committer identity resolution (see upstream `ident.c`).
//!
//! This module keeps environment access injectable so callers can test identity resolution
//! without mutating process-wide state.

use std::ffi::OsString;

use thiserror::Error;
use time::OffsetDateTime;

use crate::commit::assemble_identity;
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

    /// Environment variable that overrides this role's timestamp
    /// (`GIT_AUTHOR_DATE` / `GIT_COMMITTER_DATE`).
    #[must_use]
    pub fn env_date_key(self) -> &'static str {
        match self {
            IdentRole::Author => "GIT_AUTHOR_DATE",
            IdentRole::Committer => "GIT_COMMITTER_DATE",
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
    /// No configured or environment email is available and auto-detection is not allowed.
    #[error("no configured email is available for auto-detection")]
    MissingConfiguredEmail {
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

    if honor_use_config_only {
        return Err(IdentityError::MissingConfiguredEmail { role });
    }

    Ok(synthetic_email_with(env))
}

/// Returns whether strict commit identity resolution can produce an email for `role`.
#[must_use]
pub fn identity_email_is_configured_with<E: IdentityEnv>(
    env: &E,
    config: &ConfigSet,
    role: IdentRole,
) -> bool {
    resolve_email_inner_with(env, config, role, true).is_ok()
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

/// Resolve the full identity line (`Name <email> <epoch> <offset>`) for `role` when
/// creating a commit, tag or notes commit.
///
/// Name and email come from [`resolve_name_with`] and [`resolve_email_with`], so this fails
/// exactly when they do. The timestamp is the role's [`IdentRole::env_date_key`] variable
/// when set and non-blank, otherwise `now`.
///
/// # Parameters
///
/// - `env`: environment supplying `GIT_*` identity and date variables.
/// - `config`: configuration supplying `user.*`, `author.*` and `committer.*`.
/// - `role`: whether to resolve the author or the committer.
/// - `now`: timestamp (with its UTC offset) used when no date override is set.
///
/// # Errors
///
/// Returns [`IdentityError`] when no usable name or email is configured.
pub fn resolve_identity_with<E: IdentityEnv>(
    env: &E,
    config: &ConfigSet,
    role: IdentRole,
    now: OffsetDateTime,
) -> Result<String, IdentityError> {
    let name = resolve_name_with(env, config, role)?;
    let email = resolve_email_with(env, config, role)?;
    let date = env
        .var(role.env_date_key())
        .filter(|d| !d.trim().is_empty());
    Ok(assemble_identity(&name, &email, date.as_deref(), now))
}

/// Best-effort committer identity line (`Name <email> <epoch> <offset>`) for reflog entries.
///
/// Uses [`resolve_loose_committer_parts_with`], so it never fails, and always stamps `now`
/// (callers that want `GIT_COMMITTER_DATE` honoured fold it into `now`).
///
/// # Parameters
///
/// - `env`: environment supplying `GIT_*` identity variables.
/// - `config`: configuration supplying `committer.*` and `user.*`.
/// - `now`: timestamp (with its UTC offset) recorded on the line.
#[must_use]
pub fn resolve_loose_committer_identity_with<E: IdentityEnv>(
    env: &E,
    config: &ConfigSet,
    now: OffsetDateTime,
) -> String {
    let (name, email) = resolve_loose_committer_parts_with(env, config);
    assemble_identity(&name, &email, None, now)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;

    use super::*;

    struct MapEnv(HashMap<String, String>);

    impl IdentityEnv for MapEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }

        fn var_os(&self, key: &str) -> Option<OsString> {
            self.var(key).map(OsString::from)
        }
    }

    #[test]
    fn strict_email_resolution_fails_without_configured_sources() {
        let env = MapEnv(HashMap::from([("USER".to_owned(), "alice".to_owned())]));
        let config = ConfigSet::default();
        let err = resolve_email_with(&env, &config, IdentRole::Author).expect_err("strict");
        assert_eq!(
            err,
            IdentityError::MissingConfiguredEmail {
                role: IdentRole::Author
            }
        );
        assert!(!identity_email_is_configured_with(
            &env,
            &config,
            IdentRole::Author
        ));
    }

    #[test]
    fn lenient_email_resolution_still_synthesizes_without_config() {
        let env = MapEnv(HashMap::from([
            ("USER".to_owned(), "alice".to_owned()),
            ("HOSTNAME".to_owned(), "host.example".to_owned()),
        ]));
        let config = ConfigSet::default();
        let email = resolve_email_lenient_with(&env, &config, IdentRole::Author);
        assert_eq!(email, "alice@host.example");
    }

    #[test]
    fn strict_email_honors_user_email_config() {
        let env = MapEnv(HashMap::new());
        let mut config = ConfigSet::default();
        config
            .add_command_override("user.email", "me@example.com")
            .expect("override");
        let email = resolve_email_with(&env, &config, IdentRole::Committer).expect("configured");
        assert_eq!(email, "me@example.com");
        assert!(identity_email_is_configured_with(
            &env,
            &config,
            IdentRole::Committer
        ));
    }

    fn vars(pairs: &[(&str, &str)]) -> MapEnv {
        MapEnv(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    fn fixed_now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_112_911_993)
            .expect("epoch")
            .to_offset(time::UtcOffset::from_hms(-7, 0, 0).expect("offset"))
    }

    #[test]
    fn identity_line_uses_now_without_date_override() {
        let env = vars(&[
            ("GIT_AUTHOR_NAME", "A U Thor"),
            ("GIT_AUTHOR_EMAIL", "author@example.com"),
        ]);
        let line =
            resolve_identity_with(&env, &ConfigSet::default(), IdentRole::Author, fixed_now())
                .expect("identity");
        assert_eq!(line, "A U Thor <author@example.com> 1112911993 -0700");
    }

    #[test]
    fn identity_line_honours_only_its_own_role_date() {
        let env = vars(&[
            ("GIT_AUTHOR_NAME", "A U Thor"),
            ("GIT_AUTHOR_EMAIL", "author@example.com"),
            ("GIT_COMMITTER_NAME", "C O Mitter"),
            ("GIT_COMMITTER_EMAIL", "committer@example.com"),
            ("GIT_COMMITTER_DATE", "@1234567890 +0100"),
        ]);
        let config = ConfigSet::default();
        let committer = resolve_identity_with(&env, &config, IdentRole::Committer, fixed_now())
            .expect("committer");
        assert_eq!(
            committer,
            "C O Mitter <committer@example.com> 1234567890 +0100"
        );
        let author =
            resolve_identity_with(&env, &config, IdentRole::Author, fixed_now()).expect("author");
        assert_eq!(author, "A U Thor <author@example.com> 1112911993 -0700");
    }

    #[test]
    fn identity_line_treats_blank_date_override_as_unset() {
        let env = vars(&[
            ("GIT_AUTHOR_NAME", "A U Thor"),
            ("GIT_AUTHOR_EMAIL", "author@example.com"),
            ("GIT_AUTHOR_DATE", "  "),
        ]);
        let line =
            resolve_identity_with(&env, &ConfigSet::default(), IdentRole::Author, fixed_now())
                .expect("identity");
        assert_eq!(line, "A U Thor <author@example.com> 1112911993 -0700");
    }

    #[test]
    fn identity_line_fails_like_strict_email_resolution() {
        let env = vars(&[("USER", "alice"), ("GIT_AUTHOR_NAME", "Alice")]);
        let err =
            resolve_identity_with(&env, &ConfigSet::default(), IdentRole::Author, fixed_now())
                .expect_err("no configured email");
        assert_eq!(
            err,
            IdentityError::MissingConfiguredEmail {
                role: IdentRole::Author
            }
        );
    }

    #[test]
    fn loose_committer_identity_never_fails_and_ignores_date_override() {
        let env = vars(&[
            ("USER", "alice"),
            ("HOSTNAME", "host.example"),
            ("GIT_COMMITTER_DATE", "@1234567890 +0100"),
        ]);
        let mut config = ConfigSet::default();
        config
            .add_command_override("user.name", "Alice")
            .expect("override");
        let line = resolve_loose_committer_identity_with(&env, &config, fixed_now());
        assert_eq!(line, "Alice <alice@host.example> 1112911993 -0700");
    }

    #[test]
    fn env_date_key_matches_role() {
        assert_eq!(IdentRole::Author.env_date_key(), "GIT_AUTHOR_DATE");
        assert_eq!(IdentRole::Committer.env_date_key(), "GIT_COMMITTER_DATE");
    }
}
