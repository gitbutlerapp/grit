//! Reference store value types.

use crate::check_ref_format::{check_refname_format, RefNameError, RefNameOptions};
use crate::objects::ObjectId;

/// Validated full ref name (for example `refs/heads/main`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RefName(String);

impl RefName {
    /// Parse and validate `name` using Git refname rules.
    ///
    /// # Errors
    ///
    /// Returns [`RefNameError`] when the name is empty or otherwise invalid.
    pub fn try_from(name: &str) -> Result<Self, RefNameError> {
        if name == "HEAD" {
            return Ok(Self(name.to_owned()));
        }
        check_refname_format(name, &RefNameOptions::default())?;
        Ok(Self(name.to_owned()))
    }

    /// Borrow the stored ref name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for RefName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for RefName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Storage-level ref value without DWIM or resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawRef {
    /// Points directly at an object.
    Direct(ObjectId),
    /// Points at another ref by name (`ref: …` on disk).
    Symbolic(String),
}

impl RawRef {
    /// Whether this value is symbolic.
    #[must_use]
    pub fn is_symbolic(&self) -> bool {
        matches!(self, Self::Symbolic(_))
    }
}

/// One ref returned from iteration, optionally with a peeled object id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    /// Full ref name.
    pub name: String,
    /// Stored value (direct or symbolic).
    pub value: RawRef,
    /// Peeled oid when `value` is symbolic and resolves successfully.
    pub peeled: Option<ObjectId>,
}

/// Compare-and-swap expectation for a single ref update.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Expected {
    /// Any current value (including absent) is accepted.
    Any,
    /// Ref must not exist yet (create-only).
    Missing,
    /// Ref must exist with any value.
    Exists,
    /// Ref must be a direct ref pointing at this oid.
    Oid(ObjectId),
    /// Ref must be symbolic with this exact target name.
    Symref(String),
}

/// Metadata appended to a ref update for the reflog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogUpdate {
    /// Author/committer identity (name and email); timestamp is taken from [`Self::time`].
    pub identity: String,
    /// Reflog message (may be empty).
    pub message: String,
    /// Timestamp recorded in the reflog line.
    pub time: time::OffsetDateTime,
    /// When set, recorded as the reflog old OID instead of resolving from ref state.
    pub old_oid: Option<crate::objects::ObjectId>,
    /// When set, recorded as the reflog new OID instead of deriving from the update.
    pub new_oid: Option<crate::objects::ObjectId>,
}

/// Per-update flags (Git `REF_NO_DEREF` / `REF_LOG_ONLY` semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefUpdateFlags {
    /// Do not dereference symbolic refs when computing reflog old/new oids.
    pub no_deref: bool,
    /// Append reflog only; do not change the ref value.
    pub log_only: bool,
}

/// Rename a loose ref when only letter case changes (case-insensitive filesystems).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefCaseOnlyRename {
    /// Full ref name before rename (for example `refs/heads/topic`).
    pub from: String,
    /// Full ref name after rename (for example `refs/heads/Topic`).
    pub to: String,
}

/// Move a branch reflog to a new ref name and append one entry (branch rename).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefReflogRename {
    /// Full ref name whose reflog is moved (for example `refs/heads/topic`).
    pub from: String,
    /// Destination ref name (for example `refs/heads/new`).
    pub to: String,
    /// Rename entry appended as the newest reflog line on `to`.
    pub append: ReflogUpdate,
}

/// One ref create, update, delete, or reflog-only touch in a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    /// Full ref name.
    pub name: String,
    /// New stored value, or `None` to delete the ref.
    pub new_value: Option<RawRef>,
    /// Required current state for compare-and-swap.
    pub expected: Expected,
    /// Reflog line to append when the update commits (`None` skips reflog).
    pub reflog: Option<ReflogUpdate>,
    /// Update modifiers.
    pub flags: RefUpdateFlags,
}

/// Which physical backend backs a [`super::RefStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefStorageFormat {
    /// In-memory store for tests and embedders.
    Memory,
    /// Loose refs and `packed-refs` under a git directory.
    Files,
    /// On-disk reftable stack (`extensions.refStorage = reftable`).
    Reftable,
    /// A backend outside grit-lib, identified by a short name (for example `"sqlite"`).
    ///
    /// Library code treats it like [`Self::Memory`]: no files-backend shortcuts apply.
    Custom(&'static str),
}

impl std::fmt::Display for RefStorageFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Memory => f.write_str("memory"),
            Self::Files => f.write_str("files"),
            Self::Reftable => f.write_str("reftable"),
            Self::Custom(name) => f.write_str(name),
        }
    }
}
