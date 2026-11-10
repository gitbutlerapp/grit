//! Shared JSON shaping for the `serde-models` feature.

#[cfg(feature = "serde-models")]
use std::borrow::Cow;

#[cfg(feature = "serde-models")]
use time::OffsetDateTime;

#[cfg(feature = "serde-models")]
use crate::commit::format_git_timestamp;
#[cfg(feature = "serde-models")]
use crate::ident::Signature;

/// RFC 3339 for `OffsetDateTime`, matching [`Signature`] JSON dates.
#[cfg(feature = "serde-models")]
#[must_use]
pub(crate) fn offset_date_time_rfc3339(time: OffsetDateTime) -> String {
    Signature {
        name: String::new(),
        email: String::new(),
        time,
    }
    .format_rfc3339()
    .unwrap_or_else(|| format_git_timestamp(time))
}

#[cfg(feature = "serde-models")]
pub(crate) fn serialize_optional_offset_date_time<S>(
    time: &Option<OffsetDateTime>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match time {
        Some(t) => serializer.serialize_some(&offset_date_time_rfc3339(*t)),
        None => serializer.serialize_none(),
    }
}

/// Index or tree entry path bytes as a lossy UTF-8 string (repository-relative).
#[cfg(feature = "serde-models")]
#[must_use]
pub(crate) fn path_bytes_for_json(path: &[u8]) -> Cow<'_, str> {
    crate::index::index_path_outcome_key(path)
}
