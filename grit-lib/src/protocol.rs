//! Protocol allow/deny policy and client wire-protocol version selection.
//!
//! Implements the `protocol.<name>.allow` config and `GIT_ALLOW_PROTOCOL`
//! environment semantics without reading process-global state directly.

use crate::config::ConfigSet;
use crate::error::{ConfigError, Error};
use thiserror::Error;

/// Errors returned when a transport protocol is not allowed.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    /// Protocol is not included in `GIT_ALLOW_PROTOCOL`.
    #[error("protocol '{protocol}' is not allowed by GIT_ALLOW_PROTOCOL")]
    NotAllowedByEnvironment {
        /// Protocol name that was denied.
        protocol: String,
    },
    /// Protocol is denied by config or default policy.
    #[error("protocol '{protocol}' is not allowed")]
    NotAllowed {
        /// Protocol name that was denied.
        protocol: String,
    },
    /// Config contains an unknown allow value.
    #[error("unknown protocol.allow value '{value}' for protocol '{protocol}'")]
    UnknownAllowValue {
        /// Protocol name whose policy was evaluated.
        protocol: String,
        /// Unknown config value.
        value: String,
    },
}

/// Explicit inputs for protocol allow/deny evaluation.
#[derive(Clone, Debug, Default)]
pub struct ProtocolPolicyInputs {
    /// `GIT_ALLOW_PROTOCOL` value, when set.
    pub git_allow_protocol: Option<String>,
    /// `GIT_PROTOCOL_FROM_USER` value, when set.
    pub git_protocol_from_user: Option<String>,
    /// `protocol.<name>.allow` config value.
    pub specific_allow: Option<String>,
    /// `protocol.allow` config value.
    pub blanket_allow: Option<String>,
}

/// Check whether a given protocol (e.g. `file`, `git`, `ssh`, `https`) is allowed.
///
/// Rules match Git's `transport.c` / `is_transport_allowed`:
/// 1. `GIT_ALLOW_PROTOCOL` is a colon- or comma-separated whitelist.
/// 2. `protocol.<name>.allow` overrides the blanket config.
/// 3. `protocol.allow` supplies a blanket default.
/// 4. Built-in defaults: `http`, `https`, `git`, and `ssh` are always allowed;
///    `ext` is never allowed; any other protocol is `user`.
pub fn check_protocol_allowed_with(
    protocol: &str,
    inputs: &ProtocolPolicyInputs,
) -> Result<(), ProtocolError> {
    if let Some(val) = inputs.git_allow_protocol.as_deref() {
        let allowed: Vec<&str> = val
            .split([':', ','])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if allowed.contains(&protocol) {
            return Ok(());
        }
        return Err(ProtocolError::NotAllowedByEnvironment {
            protocol: protocol.to_owned(),
        });
    }

    if let Some(ref val) = inputs.specific_allow {
        return check_allow_value(protocol, val, inputs.git_protocol_from_user.as_deref());
    }

    if let Some(ref val) = inputs.blanket_allow {
        return check_allow_value(protocol, val, inputs.git_protocol_from_user.as_deref());
    }

    match protocol.to_ascii_lowercase().as_str() {
        "http" | "https" | "git" | "ssh" => Ok(()),
        "ext" => Err(ProtocolError::NotAllowed {
            protocol: protocol.to_owned(),
        }),
        _ => check_allow_value(protocol, "user", inputs.git_protocol_from_user.as_deref()),
    }
}

fn check_allow_value(
    protocol: &str,
    value: &str,
    git_protocol_from_user: Option<&str>,
) -> Result<(), ProtocolError> {
    match value.to_lowercase().as_str() {
        "always" => Ok(()),
        "never" => Err(ProtocolError::NotAllowed {
            protocol: protocol.to_owned(),
        }),
        "user" => {
            if protocol_from_user(git_protocol_from_user) {
                Ok(())
            } else {
                Err(ProtocolError::NotAllowed {
                    protocol: protocol.to_owned(),
                })
            }
        }
        other => Err(ProtocolError::UnknownAllowValue {
            protocol: protocol.to_owned(),
            value: other.to_owned(),
        }),
    }
}

/// Whether `protocol.<name>.allow=user` should be considered allowed.
#[must_use]
pub fn protocol_from_user(raw: Option<&str>) -> bool {
    match raw {
        None => true,
        Some(v) => {
            let v = v.trim().to_ascii_lowercase();
            v.is_empty() || !matches!(v.as_str(), "0" | "false" | "no" | "off")
        }
    }
}

/// Parse a protocol version digit (`0`, `1`, or `2`).
#[must_use]
pub(crate) fn parse_protocol_version_digit(s: &str) -> Option<u8> {
    match s.trim() {
        "0" => Some(0),
        "1" => Some(1),
        "2" => Some(2),
        _ => None,
    }
}

/// Parse `protocol.version` from config (strict: invalid values are rejected).
///
/// Returns [`Error::Config`] when the value is present but not `0`, `1`, or `2`.
pub fn try_protocol_version_from_config_value(raw: &str) -> Result<u8, Error> {
    parse_protocol_version_digit(raw).ok_or_else(|| {
        Error::Config(ConfigError::UnknownValue {
            key: "protocol.version".to_owned(),
            value: raw.to_owned(),
        })
    })
}

/// Effective client-side protocol version from `protocol.version` in `config`.
///
/// Defaults to `2` when unset. A value that is not `0`, `1`, or `2` is rejected with
/// [`Error::Config`].
pub fn try_effective_client_protocol_version_from_config(config: &ConfigSet) -> Result<u8, Error> {
    config
        .value("protocol.version")
        .map_or(Ok(2), |v| try_protocol_version_from_config_value(&v))
}

/// Value for the HTTP `Git-Protocol` request header for a client protocol version.
///
/// Version `0` means no header (classic v0 advertisement); `1` and `2` send
/// `version=1` / `version=2` respectively.
#[must_use]
#[cfg_attr(not(any(test, feature = "http-ureq")), allow(dead_code))]
pub(crate) fn client_git_protocol_header_value(version: u8) -> Option<String> {
    match version {
        0 => None,
        1 => Some("version=1".to_string()),
        _ => Some("version=2".to_string()),
    }
}

/// `Git-Protocol` header value implied by `protocol.version` in `config`.
///
/// # Errors
///
/// Returns [`Error::Config`] when `protocol.version` is set to an invalid value.
#[cfg_attr(not(any(test, feature = "http-ureq")), allow(dead_code))]
pub(crate) fn client_git_protocol_header_from_config(
    config: &ConfigSet,
) -> Result<Option<String>, Error> {
    Ok(client_git_protocol_header_value(
        try_effective_client_protocol_version_from_config(config)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_git_protocol_header_matches_version() {
        assert_eq!(client_git_protocol_header_value(0), None);
        assert_eq!(
            client_git_protocol_header_value(1).as_deref(),
            Some("version=1")
        );
        assert_eq!(
            client_git_protocol_header_value(2).as_deref(),
            Some("version=2")
        );
    }

    #[test]
    fn protocol_version_from_config_defaults_to_v2_header() {
        let config = ConfigSet::new();
        assert_eq!(
            client_git_protocol_header_from_config(&config)
                .expect("header")
                .as_deref(),
            Some("version=2")
        );
    }

    #[test]
    fn protocol_version_from_config_honors_protocol_version_key() {
        let mut config = ConfigSet::new();
        config
            .add_command_override("protocol.version", "1")
            .expect("override");
        assert_eq!(
            client_git_protocol_header_from_config(&config)
                .expect("header")
                .as_deref(),
            Some("version=1")
        );
    }

    #[test]
    fn protocol_version_rejects_invalid_config_value() {
        let mut config = ConfigSet::new();
        config
            .add_command_override("protocol.version", "9")
            .expect("override");
        let err = client_git_protocol_header_from_config(&config).expect_err("invalid");
        assert!(
            matches!(err, Error::Config(_)),
            "expected ConfigError, got {err:?}"
        );
    }

    #[test]
    fn protocol_version_defaults_to_two_and_honors_config() {
        let mut config = ConfigSet::new();
        assert_eq!(
            try_effective_client_protocol_version_from_config(&config).expect("default"),
            2
        );
        config
            .add_command_override("protocol.version", "0")
            .expect("override");
        assert_eq!(
            try_effective_client_protocol_version_from_config(&config).expect("v0"),
            0
        );
    }

    #[test]
    fn protocol_from_user_defaults_true() {
        assert!(protocol_from_user(None));
    }

    #[test]
    fn try_protocol_version_from_config_value_accepts_v2_and_rejects_unknown() {
        use crate::error::{ConfigError, Error};
        let err = try_protocol_version_from_config_value("3").expect_err("v3");
        match err {
            Error::Config(ConfigError::UnknownValue { key, value }) => {
                assert_eq!(key, "protocol.version");
                assert_eq!(value, "3");
            }
            other => panic!("expected UnknownValue for protocol.version, got {other:?}"),
        }
        assert_eq!(try_protocol_version_from_config_value("2").expect("v2"), 2);
    }
}
