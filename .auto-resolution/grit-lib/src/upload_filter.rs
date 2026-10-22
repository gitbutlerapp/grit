//! Upload-pack object filter policy.

use crate::config::ConfigSet;
use crate::rev_list::ObjectFilter;
use thiserror::Error;

/// Error returned when an upload-pack filter request is not allowed by config.
#[derive(Debug, Error)]
pub enum UploadFilterError {
    /// `uploadpackfilter.tree.maxDepth` exists but is not a non-negative integer.
    #[error("unable to parse uploadpackfilter.tree.maxdepth")]
    InvalidTreeMaxDepth,
    /// A requested filter kind is disabled by `uploadpackfilter.*.allow`.
    #[error("filter '{0}' not supported")]
    UnsupportedFilter(String),
    /// A `tree:<depth>` request exceeds `uploadpackfilter.tree.maxDepth`.
    #[error("tree filter allows max depth {max}, but got {actual}")]
    TreeDepthTooLarge {
        /// Maximum tree depth allowed by repository config.
        max: u64,
        /// Requested tree filter depth.
        actual: u64,
    },
    /// The filter specification could not be parsed.
    #[error("{0}")]
    InvalidFilterSpec(String),
}

/// Validate upload-pack filter-related config that must be well-formed even
/// Validate a requested upload-pack object filter against repository config.
///
/// # Errors
///
/// Returns an error if the filter cannot be parsed, is disabled by
/// `uploadpackfilter.*.allow`, or exceeds `uploadpackfilter.tree.maxDepth`.
pub fn validate_upload_filter_request(
    config: &ConfigSet,
    spec: &str,
) -> Result<(), UploadFilterError> {
    let filter = ObjectFilter::parse(spec).map_err(UploadFilterError::InvalidFilterSpec)?;
    validate_filter(config, &filter)
}

fn tree_max_depth(config: &ConfigSet) -> Result<Option<u64>, UploadFilterError> {
    config
        .get("uploadpackfilter.tree.maxdepth")
        .map(|value| {
            value
                .trim()
                .parse::<u64>()
                .map_err(|_| UploadFilterError::InvalidTreeMaxDepth)
        })
        .transpose()
}

fn validate_filter(config: &ConfigSet, filter: &ObjectFilter) -> Result<(), UploadFilterError> {
    match filter {
        ObjectFilter::BlobNone => ensure_filter_allowed(config, "blob:none", "blob:none"),
        ObjectFilter::BlobLimit(_) => ensure_filter_allowed(config, "blob:limit", "blob:limit"),
        ObjectFilter::TreeDepth(depth) => {
            ensure_filter_allowed(config, "tree", "tree")?;
            if let Some(max) = tree_max_depth(config)? {
                if *depth > max {
                    return Err(UploadFilterError::TreeDepthTooLarge {
                        max,
                        actual: *depth,
                    });
                }
            }
            Ok(())
        }
        ObjectFilter::SparseOid(_) => ensure_filter_allowed(config, "sparse:oid", "sparse:oid"),
        ObjectFilter::ObjectType(_) => ensure_filter_allowed(config, "object:type", "object:type"),
        ObjectFilter::Combine(filters) => {
            ensure_filter_allowed(config, "combine", "combine")?;
            for filter in filters {
                validate_filter(config, filter)?;
            }
            Ok(())
        }
    }
}

fn ensure_filter_allowed(
    config: &ConfigSet,
    config_kind: &str,
    display_kind: &str,
) -> Result<(), UploadFilterError> {
    let key = format!("uploadpackfilter.{config_kind}.allow");
    if let Some(value) = config.get_bool(&key) {
        if value.unwrap_or(false) {
            return Ok(());
        }
        return Err(UploadFilterError::UnsupportedFilter(
            display_kind.to_owned(),
        ));
    }

    if config
        .get_bool("uploadpackfilter.allow")
        .map(|value| value.unwrap_or(false))
        .unwrap_or(true)
    {
        Ok(())
    } else {
        Err(UploadFilterError::UnsupportedFilter(
            display_kind.to_owned(),
        ))
    }
}
