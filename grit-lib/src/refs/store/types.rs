//! Reference store value types.

pub use crate::check_ref_format::{RefName, RefNameRef};

use crate::objects::ObjectId;

/// Storage-level ref value without DWIM or resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize, serde::Deserialize))]
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
    pub name: RefName,
    /// Stored value (direct or symbolic).
    pub value: RawRef,
    /// Peeled tip when `value` is symbolic and resolves, or when a direct ref's
    /// `packed-refs` record includes a `^` peel line.
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
    Symref(RefName),
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

impl ReflogUpdate {
    /// Build a reflog line from a committer [`crate::ident::Signature`].
    ///
    /// Old and new object ids are left unset so the ref store derives them from the update.
    #[must_use]
    pub fn from_signature(signature: &crate::ident::Signature, message: impl Into<String>) -> Self {
        Self {
            identity: signature.reflog_name_email(),
            message: message.into(),
            time: signature.time,
            old_oid: None,
            new_oid: None,
        }
    }
}

/// Per-update flags (Git `REF_NO_DEREF` / `REF_LOG_ONLY` / `REF_SKIP_OID_VERIFICATION`
/// semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefUpdateFlags {
    /// Do not dereference symbolic refs when computing reflog old/new oids.
    pub no_deref: bool,
    /// Append reflog only; do not change the ref value.
    pub log_only: bool,
    /// Store the direct value without checking that the object exists (and, for a branch or
    /// `HEAD`, that it is a commit).
    ///
    /// For deliberately writing a dangling ref, such as a corrupt-repository fixture. Every
    /// other caller should leave this off so a lost race with a prune fails at the update
    /// instead of leaving a broken ref.
    pub skip_oid_verification: bool,
}

/// Rename a loose ref when only letter case changes (case-insensitive filesystems).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefCaseOnlyRename {
    /// Full ref name before rename (for example `refs/heads/topic`).
    pub from: RefName,
    /// Full ref name after rename (for example `refs/heads/Topic`).
    pub to: RefName,
}

/// Move a branch reflog to a new ref name and append one entry (branch rename).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefReflogRename {
    /// Full ref name whose reflog is moved (for example `refs/heads/topic`).
    pub from: RefName,
    /// Destination ref name (for example `refs/heads/new`).
    pub to: RefName,
    /// Rename entry appended as the newest reflog line on `to`.
    pub append: ReflogUpdate,
}

/// One ref create, update, delete, or reflog-only touch in a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    /// Full ref name.
    pub name: RefName,
    /// New stored value, or `None` to delete the ref.
    pub new_value: Option<RawRef>,
    /// Required current state for compare-and-swap.
    pub expected: Expected,
    /// Reflog line to append when the update commits (`None` skips reflog). Ignored when
    /// [`Self::new_value`] is `None` and the update is not log-only: deleting a ref drops
    /// its reflog, matching Git.
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
