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
use crate::ident_config::{env_user_name, ident_default_name};

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
    /// condition. User-facing setup guidance is rendered by `grit-cli`.
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

/// The value of a `GIT_*_NAME` variable when it is set (possibly to `""`).
fn name_env_with<E: IdentityEnv>(env: &E, key: &str) -> Option<String> {
    match read_git_identity_name_env_with(env, key) {
        GitIdentityNameEnv::Set(name) => Some(name),
        GitIdentityNameEnv::Unset => None,
    }
}

/// `value` trimmed, or [`None`] when it is missing or blank.
fn non_blank(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

fn use_config_only(config: &ConfigSet) -> bool {
    matches!(config.get_bool("user.useConfigOnly"), Some(Ok(true)))
}

fn config_mail_given(config: &ConfigSet) -> bool {
    ["user.email", "author.email", "committer.email"]
        .iter()
        .any(|key| non_blank(config.get(key)).is_some())
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
    let user = env_user_name(env);
    let host = env.var("HOSTNAME").unwrap_or_else(|| "unknown".to_owned());
    let domain = if host.contains('.') {
        host
    } else {
        format!("{host}.(none)")
    };
    format!("{user}@{domain}")
}

/// Resolve email for a role when creating commits (honors `user.useConfigOnly`).
///
/// Looks at `GIT_<ROLE>_EMAIL`, `<role>.email`, `user.email`, then (unless
/// `user.useConfigOnly` is set without any configured email) `EMAIL`. Never synthesizes
/// an address from the user and host names.
///
/// # Errors
///
/// [`IdentityError::AutoDetectionDisabled`] when `user.useConfigOnly` blocks the `EMAIL`
/// fallback, [`IdentityError::MissingConfiguredEmail`] when no source provides an email.
pub fn resolve_email_with<E: IdentityEnv>(
    env: &E,
    config: &ConfigSet,
    role: IdentRole,
) -> Result<String, IdentityError> {
    let configured = non_blank(env.var(role.env_email_key()))
        .or_else(|| non_blank(config.get(role.config_email_key())))
        .or_else(|| non_blank(config.get("user.email")));
    if let Some(email) = configured {
        return Ok(email);
    }
    if use_config_only(config) && !config_mail_given(config) {
        return Err(IdentityError::AutoDetectionDisabled { role });
    }
    non_blank(env.var("EMAIL")).ok_or(IdentityError::MissingConfiguredEmail { role })
}

/// Resolve name for a role when creating commits.
///
/// A set `GIT_<ROLE>_NAME` wins even when empty; otherwise `<role>.name`, then Git's
/// default name (`user.name`, or the system user name).
///
/// # Errors
///
/// Fails like [`resolve_email_with`] when no email is available, with
/// [`IdentityError::EmptyName`] for an empty name and [`IdentityError::InvalidName`] for a
/// name made only of characters Git strips.
pub fn resolve_name_with<E: IdentityEnv>(
    env: &E,
    config: &ConfigSet,
    role: IdentRole,
) -> Result<String, IdentityError> {
    let email = resolve_email_with(env, config, role)?;

    let name = name_env_with(env, role.env_name_key()).unwrap_or_else(|| {
        non_blank(config.get(role.config_name_key()))
            .unwrap_or_else(|| ident_default_name(config, env))
    });

    if name.is_empty() {
        return Err(IdentityError::EmptyName { email, role });
    }

    if !ident_name_has_non_crud(&name) {
        return Err(IdentityError::InvalidName { name });
    }

    Ok(name)
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
/// Never fails: the name comes from `GIT_COMMITTER_NAME` (or, when that is unset,
/// `GIT_AUTHOR_NAME`), `committer.name`, `user.name`, the system user name, then
/// `"Unknown"`; the email from `GIT_COMMITTER_EMAIL`, `GIT_AUTHOR_EMAIL`, `committer.email`,
/// `user.email`, `EMAIL`, then a synthesized `user@host` address. It always stamps `now`
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
    let name = name_env_with(env, "GIT_COMMITTER_NAME")
        .or_else(|| name_env_with(env, "GIT_AUTHOR_NAME"))
        .filter(|name| !name.is_empty())
        .or_else(|| non_blank(config.get("committer.name")))
        .or_else(|| non_blank(config.get("user.name")))
        .or_else(|| Some(ident_default_name(config, env)).filter(|name| !name.is_empty()))
        .unwrap_or_else(|| "Unknown".to_owned());
    let email = non_blank(env.var("GIT_COMMITTER_EMAIL"))
        .or_else(|| non_blank(env.var("GIT_AUTHOR_EMAIL")))
        .or_else(|| non_blank(config.get("committer.email")))
        .or_else(|| non_blank(config.get("user.email")))
        .or_else(|| non_blank(env.var("EMAIL")))
        .unwrap_or_else(|| synthetic_email_with(env));
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
    }

    #[test]
    fn strict_email_falls_back_to_email_env_unless_config_only() {
        let env = vars(&[("EMAIL", " mail@example.com ")]);
        let mut config = ConfigSet::default();
        assert_eq!(
            resolve_email_with(&env, &config, IdentRole::Author).expect("EMAIL"),
            "mail@example.com"
        );
        config
            .add_command_override("user.useConfigOnly", "true")
            .expect("override");
        assert_eq!(
            resolve_email_with(&env, &config, IdentRole::Author).expect_err("config only"),
            IdentityError::AutoDetectionDisabled {
                role: IdentRole::Author
            }
        );
        config
            .add_command_override("author.email", "  ")
            .expect("override");
        config
            .add_command_override("committer.email", "c@example.com")
            .expect("override");
        assert_eq!(
            resolve_email_with(&env, &config, IdentRole::Author).expect("mail given"),
            "mail@example.com",
            "a configured email for any role re-enables the EMAIL fallback"
        );
    }

    #[test]
    fn strict_name_keeps_set_but_empty_env_override() {
        let env = vars(&[("GIT_AUTHOR_NAME", "  "), ("GIT_AUTHOR_EMAIL", "a@b.c")]);
        let mut config = ConfigSet::default();
        config
            .add_command_override("user.name", "Alice")
            .expect("override");
        assert_eq!(
            resolve_name_with(&env, &config, IdentRole::Author).expect_err("empty"),
            IdentityError::EmptyName {
                email: "a@b.c".to_owned(),
                role: IdentRole::Author
            }
        );
        let env = vars(&[("GIT_AUTHOR_EMAIL", "a@b.c")]);
        config
            .add_command_override("author.name", " Author Name ")
            .expect("override");
        assert_eq!(
            resolve_name_with(&env, &config, IdentRole::Author).expect("config"),
            "Author Name"
        );
        assert_eq!(
            resolve_name_with(
                &vars(&[("GIT_AUTHOR_EMAIL", "a@b.c"), ("GIT_AUTHOR_NAME", ",;")]),
                &config,
                IdentRole::Author
            )
            .expect_err("crud"),
            IdentityError::InvalidName {
                name: ",;".to_owned()
            }
        );
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
    fn loose_committer_identity_prefers_committer_then_author_env() {
        let env = vars(&[
            ("GIT_COMMITTER_NAME", "C O Mitter"),
            ("GIT_AUTHOR_NAME", "A U Thor"),
            ("GIT_COMMITTER_EMAIL", " "),
            ("GIT_AUTHOR_EMAIL", "author@example.com"),
        ]);
        let line = resolve_loose_committer_identity_with(&env, &ConfigSet::default(), fixed_now());
        assert_eq!(line, "C O Mitter <author@example.com> 1112911993 -0700");

        let env = vars(&[("GIT_AUTHOR_NAME", "A U Thor"), ("EMAIL", "e@example.com")]);
        let line = resolve_loose_committer_identity_with(&env, &ConfigSet::default(), fixed_now());
        assert_eq!(line, "A U Thor <e@example.com> 1112911993 -0700");
    }

    #[test]
    fn loose_committer_identity_empty_committer_name_skips_author_env() {
        let env = vars(&[
            ("GIT_COMMITTER_NAME", ""),
            ("GIT_AUTHOR_NAME", "A U Thor"),
            ("USER", "alice"),
            ("HOSTNAME", "box"),
        ]);
        let mut config = ConfigSet::default();
        config
            .add_command_override("user.name", "User Name")
            .expect("override");
        config
            .add_command_override("committer.name", " Committer Name ")
            .expect("override");
        config
            .add_command_override("user.email", "user@example.com")
            .expect("override");
        let line = resolve_loose_committer_identity_with(&env, &config, fixed_now());
        assert_eq!(line, "Committer Name <user@example.com> 1112911993 -0700");

        let mut config = ConfigSet::default();
        config
            .add_command_override("user.name", "User Name")
            .expect("override");
        let line = resolve_loose_committer_identity_with(&env, &config, fixed_now());
        assert_eq!(line, "User Name <alice@box.(none)> 1112911993 -0700");
    }

    #[test]
    fn env_date_key_matches_role() {
        assert_eq!(IdentRole::Author.env_date_key(), "GIT_AUTHOR_DATE");
        assert_eq!(IdentRole::Committer.env_date_key(), "GIT_COMMITTER_DATE");
    }
}
