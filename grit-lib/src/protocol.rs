//! Protocol allow/deny policy and client wire-protocol version selection.
//!
//! Implements the `protocol.<name>.allow` config and `GIT_ALLOW_PROTOCOL`
//! environment semantics without reading process-global state directly.

use crate::config::ConfigSet;
use crate::error::Error;
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

/// Explicit inputs for selecting the client-side Git wire protocol version.
#[derive(Clone, Debug, Default)]
pub struct ClientProtocolVersionInputs {
    /// `protocol.version` from command-line config overrides such as `-c`.
    pub config_param_version: Option<String>,
    /// `protocol.version` from repository config.
    pub repo_config_version: Option<String>,
    /// `GIT_TEST_PROTOCOL_VERSION`, when set.
    pub git_test_protocol_version: Option<String>,
}

/// Parse a protocol version digit (`0`, `1`, or `2`).
#[must_use]
pub fn parse_protocol_version_digit(s: &str) -> Option<u8> {
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
    parse_protocol_version_digit(raw)
        .ok_or_else(|| Error::Config(format!("bad protocol version '{raw}'").into()))
}

/// Select the effective client-side protocol version (strict).
///
/// Command-line config takes precedence over repository config, which takes precedence over
/// `GIT_TEST_PROTOCOL_VERSION`; when none are set the default is `2`. Any configured value
/// that is not `0`, `1`, or `2` is rejected.
pub fn try_effective_client_protocol_version_from_inputs(
    inputs: &ClientProtocolVersionInputs,
) -> Result<u8, Error> {
    if let Some(v) = inputs.config_param_version.as_deref() {
        return try_protocol_version_from_config_value(v);
    }
    if let Some(v) = inputs.repo_config_version.as_deref() {
        return try_protocol_version_from_config_value(v);
    }
    if let Some(raw) = inputs
        .git_test_protocol_version
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        return try_protocol_version_from_config_value(raw);
    }
    Ok(2)
}

/// Select the effective client-side protocol version.
///
/// Unknown values are treated as `2`, matching legacy CLI behavior. Prefer
/// [`try_effective_client_protocol_version_from_inputs`] when invalid config must be rejected.
#[must_use]
pub fn effective_client_protocol_version_from_inputs(inputs: &ClientProtocolVersionInputs) -> u8 {
    try_effective_client_protocol_version_from_inputs(inputs).unwrap_or(2)
}

/// Effective client-side protocol version from `protocol.version` in `config` only.
///
/// Does not read process environment; callers that honor `GIT_TEST_PROTOCOL_VERSION` must
/// supply it via [`ClientProtocolVersionInputs`].
pub fn try_effective_client_protocol_version_from_config(config: &ConfigSet) -> Result<u8, Error> {
    try_effective_client_protocol_version_from_inputs(&ClientProtocolVersionInputs {
        repo_config_version: config.get("protocol.version"),
        ..Default::default()
    })
}

/// Value for the HTTP `Git-Protocol` request header for a client protocol version.
///
/// Version `0` means no header (classic v0 advertisement); `1` and `2` send
/// `version=1` / `version=2` respectively.
#[must_use]
pub fn client_git_protocol_header_value(version: u8) -> Option<String> {
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
pub fn client_git_protocol_header_from_config(config: &ConfigSet) -> Result<Option<String>, Error> {
    Ok(client_git_protocol_header_value(
        try_effective_client_protocol_version_from_config(config)?,
    ))
}

/// Server-side protocol version: highest `version=N` from `GIT_PROTOCOL`, or `0` if unset.
#[must_use]
pub fn server_protocol_version_from_git_protocol(raw: Option<&str>) -> u8 {
    let Some(raw) = raw else {
        return 0;
    };
    let mut best = 0u8;
    for part in raw.split(':') {
        let Some(rest) = part.strip_prefix("version=") else {
            continue;
        };
        let v = rest.parse::<u8>().unwrap_or(0);
        if v > best {
            best = v;
        }
    }
    best
}

fn strip_protocol_version_entries(s: &str) -> String {
    s.split(':')
        .filter(|p| !p.trim_start().starts_with("version="))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(":")
}

/// Merge a requested client protocol version into an existing `GIT_PROTOCOL` value.
///
/// Existing `version=N` entries are removed before appending the requested version.
/// Returns [`None`] when `client_wants` is `0`.
#[must_use]
pub fn merged_git_protocol_value(client_wants: u8, existing: Option<&str>) -> Option<String> {
    if client_wants == 0 {
        return None;
    }
    let entry = format!("version={client_wants}");
    Some(match existing {
        Some(e) if !e.is_empty() => {
            let base = strip_protocol_version_entries(e.trim());
            if base.is_empty() {
                entry
            } else {
                format!("{base}:{entry}")
            }
        }
        _ => entry,
    })
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
    fn protocol_version_inputs_honor_git_test_without_env_read_in_config_helper() {
        let inputs = ClientProtocolVersionInputs {
            git_test_protocol_version: Some("0".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            try_effective_client_protocol_version_from_inputs(&inputs).expect("v0"),
            0
        );
        let config = ConfigSet::new();
        assert_eq!(
            try_effective_client_protocol_version_from_config(&config).expect("default"),
            2,
            "config helper must not read GIT_TEST_PROTOCOL_VERSION from the environment"
        );
    }
}
