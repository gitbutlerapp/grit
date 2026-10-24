//! Partial clone filter specs and on-disk promisor-remote configuration.

use std::path::Path;

use thiserror::Error;

use crate::config::{ConfigFile, ConfigScope, ConfigSet};
use crate::error::{Error, Result};
use crate::rev_list::{expand_object_filter_for_protocol, ObjectFilter};

/// Client-side object filter for fetch and clone (partial clone).
///
/// Only `blob:none` and `blob:limit=<n>` are supported on the wire today; other
/// [`ObjectFilter`] kinds remain server-side upload-pack policy only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectFilterSpec {
    /// Omit all blob objects (`blob:none`).
    BlobNone,
    /// Omit blobs whose inflated size is at or above the limit (`blob:limit=<n>`).
    BlobLimit(u64),
}

/// Errors parsing or applying partial-clone configuration.
#[derive(Debug, Error)]
pub enum PartialCloneError {
    /// The filter string is not a supported partial-clone spec.
    #[error("{0}")]
    InvalidFilter(String),
    /// Underlying I/O or config failure.
    #[error(transparent)]
    Library(#[from] Error),
}

impl ObjectFilterSpec {
    /// Parse `blob:none` or `blob:limit=<size>` (same grammar as Git `--filter=`).
    ///
    /// # Errors
    ///
    /// Returns [`PartialCloneError::InvalidFilter`] for unsupported filter kinds.
    pub fn parse(spec: &str) -> std::result::Result<Self, PartialCloneError> {
        let filter = ObjectFilter::parse(spec.trim()).map_err(PartialCloneError::InvalidFilter)?;
        Self::from_object_filter(&filter)
    }

    /// Narrow a general [`ObjectFilter`] to a partial-clone wire spec when possible.
    ///
    /// # Errors
    ///
    /// Returns [`PartialCloneError::InvalidFilter`] when `filter` is not `blob:none` or `blob:limit`.
    pub fn from_object_filter(
        filter: &ObjectFilter,
    ) -> std::result::Result<Self, PartialCloneError> {
        match filter {
            ObjectFilter::BlobNone => Ok(Self::BlobNone),
            ObjectFilter::BlobLimit(n) => Ok(Self::BlobLimit(*n)),
            other => Err(PartialCloneError::InvalidFilter(format!(
                "partial clone filter must be blob:none or blob:limit, not {other:?}"
            ))),
        }
    }

    /// Serialize for a `filter <spec>` wire line (limits expanded to byte counts).
    #[must_use]
    pub fn to_wire_spec(&self) -> String {
        match self {
            Self::BlobNone => "blob:none".to_owned(),
            Self::BlobLimit(n) => format!("blob:limit={n}"),
        }
    }

    /// Value stored in `remote.<name>.partialclonefilter` (Git-compatible).
    #[must_use]
    pub fn to_config_value(&self) -> String {
        self.to_wire_spec()
    }

    /// Expand for protocol (e.g. `blob:limit=1k` → `blob:limit=1024`).
    ///
    /// # Errors
    ///
    /// Returns [`PartialCloneError::InvalidFilter`] when expansion fails.
    pub fn expand_for_protocol(&self) -> std::result::Result<String, PartialCloneError> {
        expand_object_filter_for_protocol(&self.to_wire_spec())
            .map_err(PartialCloneError::InvalidFilter)
    }

    /// Parse `grit clone --blobs` values: `none` or a size with optional `k`/`m`/`g` suffix.
    ///
    /// Sizes map to `blob:limit=<n>` (same grammar as Git `--filter=blob:limit=…`).
    ///
    /// # Errors
    ///
    /// Returns [`PartialCloneError::InvalidFilter`] when `value` is not `none` or a valid limit.
    pub fn from_blobs_cli(value: &str) -> std::result::Result<Self, PartialCloneError> {
        let trimmed = value.trim();
        if trimmed.eq_ignore_ascii_case("none") {
            return Ok(Self::BlobNone);
        }
        Self::parse(&format!("blob:limit={trimmed}"))
    }

    /// Byte limit when this spec is [`Self::BlobLimit`]; `None` for [`Self::BlobNone`].
    #[must_use]
    pub const fn blob_limit_bytes(&self) -> Option<u64> {
        match self {
            Self::BlobNone => None,
            Self::BlobLimit(n) => Some(*n),
        }
    }
}

/// Writes Git partial-clone configuration for `remote` after a filtered fetch/clone.
pub struct PartialCloneConfig;

impl PartialCloneConfig {
    /// Set promisor-remote keys matching `git clone --filter=…`.
    ///
    /// Writes `remote.<remote>.promisor=true`, `remote.<remote>.partialclonefilter`,
    /// `extensions.partialclone=<remote>`, and `core.repositoryformatversion=1`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error`] on config I/O failure.
    pub fn write(git_dir: &Path, remote: &str, spec: &ObjectFilterSpec) -> Result<()> {
        let path = git_dir.join("config");
        let content = std::fs::read_to_string(&path).map_err(Error::Io)?;
        let mut config = ConfigFile::parse(&path, &content, ConfigScope::Local)?;
        let filter = spec.to_config_value();
        config.set(&format!("remote.{remote}.promisor"), "true")?;
        config.set(
            &format!("remote.{remote}.partialclonefilter"),
            filter.as_str(),
        )?;
        config.set("extensions.partialclone", remote)?;
        config.set("core.repositoryformatversion", "1")?;
        config.write()?;
        Ok(())
    }
}

/// Stored `remote.<name>.partialclonefilter` when present and valid.
#[must_use]
pub fn stored_remote_filter(config: &ConfigSet, remote: &str) -> Option<ObjectFilterSpec> {
    config
        .get(&format!("remote.{remote}.partialclonefilter"))
        .and_then(|s| ObjectFilterSpec::parse(&s).ok())
}

/// Whether `remote` is configured as a promisor partial-clone source.
#[must_use]
pub fn remote_is_promisor(config: &ConfigSet, remote: &str) -> bool {
    config
        .get(&format!("remote.{remote}.promisor"))
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        || config
            .get(&format!("remote.{remote}.partialclonefilter"))
            .is_some()
}

/// Decide whether to send `filter` on the wire and return the expanded spec string.
///
/// When the server lacks filter support, returns `(None, false)` so the client
/// fetches unfiltered, matching Git's `fetch-pack.c` behavior.
#[must_use]
pub fn wire_filter_for_capabilities(
    spec: Option<&ObjectFilterSpec>,
    v2_fetch_caps: Option<&[String]>,
    v0_v1_caps: Option<&str>,
) -> (Option<String>, bool) {
    let Some(spec) = spec else {
        return (None, false);
    };
    let Ok(wire) = spec.expand_for_protocol() else {
        return (None, false);
    };
    let supported = match v2_fetch_caps {
        Some(caps) => crate::protocol_v2::fetch_supports_filter(caps),
        None => {
            v0_v1_caps.is_some_and(|caps| caps.split_whitespace().any(|token| token == "filter"))
        }
    };
    if supported {
        (Some(wire), true)
    } else {
        (None, false)
    }
}

/// Fetch/clone must use upload-pack on the wire (not local ODB copy) when a filter
/// is requested or the remote is already a promisor partial-clone source.
#[must_use]
pub fn fetch_requires_upload_pack(
    opts_filter: Option<&ObjectFilterSpec>,
    config: &ConfigSet,
    remote_name: Option<&str>,
) -> bool {
    if opts_filter.is_some() {
        return true;
    }
    remote_name.is_some_and(|name| remote_is_promisor(config, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_blobs_cli_none_and_suffixes() {
        assert_eq!(
            ObjectFilterSpec::from_blobs_cli("none").expect("none"),
            ObjectFilterSpec::BlobNone
        );
        assert_eq!(
            ObjectFilterSpec::from_blobs_cli("1k").expect("1k"),
            ObjectFilterSpec::BlobLimit(1024)
        );
        assert!(ObjectFilterSpec::from_blobs_cli("not-a-size").is_err());
    }

    #[test]
    fn parse_blob_none_and_limit() {
        assert_eq!(
            ObjectFilterSpec::parse("blob:none").expect("none"),
            ObjectFilterSpec::BlobNone
        );
        assert_eq!(
            ObjectFilterSpec::parse("blob:limit=512").expect("limit"),
            ObjectFilterSpec::BlobLimit(512)
        );
        assert!(ObjectFilterSpec::parse("tree:0").is_err());
    }

    #[test]
    fn wire_filter_dropped_without_server_cap() {
        let spec = ObjectFilterSpec::BlobNone;
        let (wire, send) = wire_filter_for_capabilities(Some(&spec), Some(&[]), None);
        assert!(!send);
        assert!(wire.is_none());

        let (wire, send) =
            wire_filter_for_capabilities(Some(&spec), None, Some("multi_ack side-band-64k"));
        assert!(!send);
        assert!(wire.is_none());

        let (wire, send) =
            wire_filter_for_capabilities(Some(&spec), Some(&["fetch=filter".to_owned()]), None);
        assert!(send);
        assert_eq!(wire.as_deref(), Some("blob:none"));
    }
}
