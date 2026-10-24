//! Revision resolution metadata and typed object contents for display (`resolve` / `cat`).
//!
//! [`Repository::resolve`] resolves a user spec to an object id plus kind and size.
//! [`Repository::read_object_for_display`] loads and parses object bytes for programmatic use.
//!
//! ```rust
//! use grit_lib::object_inspect::ResolveError;
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let dir = tempfile::tempdir()?;
//! let repo = Repository::init(dir.path(), &InitOptions::default())?;
//! match repo.resolve("HEAD") {
//!     Ok(_resolved) => {}
//!     Err(ResolveError::UnknownRevision { .. }) => {}
//!     Err(e) => return Err(e.into()),
//! }
//! # Ok(())
//! # }
//! ```

use thiserror::Error;

use crate::diagnostics::Warning;
use crate::error::{Error, Result};
use crate::objects::{
    parse_commit, parse_tag, parse_tree, CommitData, Object, ObjectId, ObjectKind, TagData,
    TreeEntry,
};
use crate::refs::store::RawRef;
use crate::refs::{
    expand_ref_dwim_full_name, shorten_unambiguous, warn_ambiguous_refs_from_config,
};
use crate::repo::Repository;
use crate::rev_parse::{
    abbreviate_object_id, ambiguous_object_hint_lines, expand_at_minus_to_branch_name,
    parse_peel_suffix, resolve_revision, resolve_revision_for_verify,
    resolve_upstream_symbolic_name, split_double_dot_range, split_triple_dot_range,
    upstream_suffix_info,
};
use crate::rev_parse_error::{AmbiguousChangeIdHint, AmbiguousObjectHint, RevParseError};

/// Failure resolving a revision spec for object inspection.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ResolveError {
    /// The spec does not name a known revision or object.
    #[error("unknown revision '{spec}'")]
    UnknownRevision {
        /// User revision string.
        spec: String,
    },

    /// A short hex prefix matches more than one object and could not be disambiguated.
    #[error("ambiguous short object id '{spec}'")]
    AmbiguousShortObject {
        /// User revision string.
        spec: String,
        /// Candidate objects in Git hint order (type, then hex).
        candidates: Vec<AmbiguousObjectHint>,
    },

    /// A change-id prefix matches more than one commit.
    #[error("ambiguous change id '{spec}'")]
    AmbiguousChangeId {
        /// User revision string.
        spec: String,
        /// Matching commits (oid, change id, subject).
        candidates: Vec<AmbiguousChangeIdHint>,
    },

    /// A path component is missing from the tree named by the revision.
    #[error("path '{path}' does not exist in '{revision}'")]
    PathNotInTree {
        /// Path that is missing from the tree.
        path: String,
        /// Revision label for the tree that was searched.
        revision: String,
    },

    /// The spec names a revision range or set, not a single object (`git rev-parse --verify`).
    #[error("revision range or set '{spec}' is not a single object")]
    RevisionRange {
        /// User revision string.
        spec: String,
    },

    /// The resolved object is not of the expected kind.
    #[error("object '{spec}' is a {found}, not a {expected}")]
    WrongObjectKind {
        /// User revision string.
        spec: String,
        /// Kind that was resolved.
        found: ObjectKind,
        /// Kind the caller required.
        expected: ObjectKind,
    },
}

impl From<RevParseError> for ResolveError {
    fn from(value: RevParseError) -> Self {
        match value {
            RevParseError::PathNotInRevision { path, revision }
            | RevParseError::PathOnDiskNotInRevision { path, revision } => {
                ResolveError::PathNotInTree { path, revision }
            }
            RevParseError::InvalidObjectName { name } => {
                ResolveError::UnknownRevision { spec: name }
            }
            RevParseError::AmbiguousChangeId { prefix, candidates } => {
                ResolveError::AmbiguousChangeId {
                    spec: prefix,
                    candidates,
                }
            }
            other => ResolveError::UnknownRevision {
                spec: other.to_string(),
            },
        }
    }
}

/// Full and shortened ref names for a spec (`rev-parse --symbolic-full-name` / `--abbrev-ref`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRefName {
    /// Canonical full ref name (for example `refs/heads/main`).
    pub full: String,
    /// Shortest unambiguous short name (for example `main`).
    pub short: String,
}

/// Result of resolving one revision spec to a concrete object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedObject {
    /// Resolved object id (after peel suffixes in the spec).
    pub oid: ObjectId,
    /// Kind of the resolved object.
    pub kind: ObjectKind,
    /// Uncompressed object payload size in bytes.
    pub size: u64,
    /// Short ref name when the spec named a ref directly (for example `main`, `v1.0`).
    pub ref_name: Option<String>,
    /// Full ref name when the spec named a ref directly (for example `refs/heads/main`).
    pub full_ref: Option<String>,
}

/// Parsed object payload for display or serialization.
#[derive(Debug, Clone)]
pub enum ObjectContents {
    /// Raw blob bytes.
    Blob(Vec<u8>),
    /// Tree entries in Git sort order.
    Tree(Vec<TreeEntry>),
    /// Parsed commit headers and message.
    Commit(CommitData),
    /// Parsed annotated tag.
    Tag(TagData),
}

impl Repository {
    /// Resolve `spec` to an object id, kind, and size.
    ///
    /// Supports the same revision forms as [`crate::revwalk::rev_parse::resolve_revision`]
    /// (for example `HEAD~2`, `v1^{tree}`, `main:path`, `@{1}`, abbreviated oids).
    ///
    /// # Errors
    ///
    /// Returns [`ResolveError::UnknownRevision`] when the spec cannot be resolved,
    /// [`ResolveError::AmbiguousShortObject`] when a short oid is ambiguous, and
    /// [`ResolveError::PathNotInTree`] when a `rev:path` path is missing.
    pub fn resolve(&self, spec: &str) -> std::result::Result<ResolvedObject, ResolveError> {
        let oid = match resolve_revision(self, spec) {
            Ok(o) => o,
            Err(Error::ObjectNotFound(_)) => {
                return Err(ResolveError::UnknownRevision {
                    spec: spec.to_owned(),
                });
            }
            Err(Error::RevParse(RevParseError::AmbiguousShortObject { spec: prefix })) => {
                let candidates =
                    ambiguous_object_hint_lines(self, &prefix, None).unwrap_or_default();
                return Err(ResolveError::AmbiguousShortObject {
                    spec: spec.to_owned(),
                    candidates,
                });
            }
            Err(Error::RevParse(RevParseError::AmbiguousChangeId { prefix, candidates })) => {
                return Err(ResolveError::AmbiguousChangeId {
                    spec: prefix,
                    candidates,
                });
            }
            Err(Error::RevParse(e)) => return Err(e.into()),
            Err(_) => {
                return Err(ResolveError::UnknownRevision {
                    spec: spec.to_owned(),
                });
            }
        };

        let info = self
            .read_info_replaced(&oid)
            .map_err(|_| ResolveError::UnknownRevision {
                spec: spec.to_owned(),
            })?;
        let ref_names = infer_ref_names(self, spec, oid);
        Ok(ResolvedObject {
            oid,
            kind: info.kind,
            size: info.size,
            ref_name: ref_names.as_ref().map(|n| n.short.clone()),
            full_ref: ref_names.map(|n| n.full),
        })
    }

    /// Strict single-object resolution for plumbing verify (`git rev-parse --verify`).
    ///
    /// Accepts one spec that names one existing object, honoring `^{type}` peels. Rejects
    /// revision ranges (`A..B`, `A...B`) and revision-set excludes (`^A`). Verifies the object exists
    /// in the object database (full hex ids must refer to stored objects, not merely parse).
    ///
    /// # Errors
    ///
    /// Returns [`ResolveError::RevisionRange`] for range or set syntax,
    /// [`ResolveError::UnknownRevision`] when the spec cannot be resolved or the object is missing,
    /// [`ResolveError::AmbiguousShortObject`] for ambiguous abbreviated ids, and
    /// [`ResolveError::WrongObjectKind`] when `expected` is set and the peeled kind differs.
    pub fn verify_object(
        &self,
        spec: &str,
        expected: Option<ObjectKind>,
    ) -> std::result::Result<ObjectId, ResolveError> {
        if split_double_dot_range(spec).is_some()
            || split_triple_dot_range(spec).is_some()
            || is_revision_set_exclude_spec(spec)
        {
            return Err(ResolveError::RevisionRange {
                spec: spec.to_owned(),
            });
        }

        let oid = match resolve_revision_for_verify(self, spec) {
            Ok(o) => o,
            Err(Error::ObjectNotFound(_)) => {
                return Err(ResolveError::UnknownRevision {
                    spec: spec.to_owned(),
                });
            }
            Err(Error::RevParse(RevParseError::AmbiguousShortObject { spec: prefix })) => {
                let candidates =
                    ambiguous_object_hint_lines(self, &prefix, None).unwrap_or_default();
                return Err(ResolveError::AmbiguousShortObject {
                    spec: spec.to_owned(),
                    candidates,
                });
            }
            Err(Error::RevParse(RevParseError::AmbiguousChangeId { prefix, candidates })) => {
                return Err(ResolveError::AmbiguousChangeId {
                    spec: prefix,
                    candidates,
                });
            }
            Err(Error::RevParse(e)) => return Err(e.into()),
            Err(_) => {
                return Err(ResolveError::UnknownRevision {
                    spec: spec.to_owned(),
                });
            }
        };

        let info = self
            .read_info_replaced(&oid)
            .map_err(|_| ResolveError::UnknownRevision {
                spec: spec.to_owned(),
            })?;

        if let Some(want) = expected {
            if info.kind != want {
                return Err(ResolveError::WrongObjectKind {
                    spec: spec.to_owned(),
                    found: info.kind,
                    expected: want,
                });
            }
        }

        Ok(oid)
    }

    /// Resolve `spec` to ref names when it names a ref (`rev-parse --abbrev-ref` / `--symbolic-full-name`).
    ///
    /// Returns `Ok(None)` when the spec does not name a ref (for example a commit oid or `HEAD^{tree}`).
    ///
    /// # Errors
    ///
    /// When DWIM finds multiple refs and `core.warnAmbiguousRefs` is enabled, emits
    /// [`Warning::AmbiguousRefname`] and returns `Ok(None)` (matching `git rev-parse` stdout).
    /// Other [`RevParseError`] variants cover invalid upstream / `@{-N}` specs.
    pub fn resolve_ref_name(
        &self,
        spec: &str,
    ) -> std::result::Result<Option<ResolvedRefName>, RevParseError> {
        let strict = warn_ambiguous_refs_from_config(self);
        if upstream_suffix_info(spec).is_some() {
            let full = resolve_upstream_symbolic_name(self, spec).map_err(map_resolve_ref_error)?;
            let short = shorten_unambiguous(self, &full, strict).map_err(map_resolve_ref_error)?;
            return Ok(Some(ResolvedRefName { full, short }));
        }
        if let Some(branch) =
            expand_at_minus_to_branch_name(self, spec).map_err(map_resolve_ref_error)?
        {
            let full = format!("refs/heads/{branch}");
            if self.resolve_ref_oid(&full).is_err() {
                return Ok(None);
            }
            let short = shorten_unambiguous(self, &full, strict).map_err(map_resolve_ref_error)?;
            return Ok(Some(ResolvedRefName { full, short }));
        }
        let (count, full) = expand_ref_dwim_full_name(self, spec).map_err(map_resolve_ref_error)?;
        if count == 0 {
            if spec == "HEAD" {
                match self
                    .refs()
                    .read_raw("HEAD")
                    .map_err(map_resolve_ref_error)?
                {
                    Some(RawRef::Direct(_)) => {
                        return Ok(Some(ResolvedRefName {
                            full: "HEAD".to_owned(),
                            short: "HEAD".to_owned(),
                        }));
                    }
                    Some(RawRef::Symbolic(target)) if self.resolve_ref_oid(&target).is_err() => {
                        return Ok(None);
                    }
                    Some(RawRef::Symbolic(_)) | None => {}
                }
            }
            return Ok(None);
        }
        if count > 1 && strict {
            self.warn(Warning::AmbiguousRefname {
                spec: spec.to_owned(),
            });
            return Ok(None);
        }
        let full = full.ok_or_else(|| RevParseError::AmbiguousRefname {
            spec: spec.to_owned(),
        })?;
        let short = shorten_unambiguous(self, &full, strict).map_err(map_resolve_ref_error)?;
        Ok(Some(ResolvedRefName { full, short }))
    }

    /// Read uncompressed object bytes for display, following replace refs when enabled.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ObjectNotFound`] when the object is missing.
    pub fn read_object_raw_for_display(&self, oid: &ObjectId) -> Result<Object> {
        self.read_replaced(oid)
    }

    /// Read and parse the object named by `oid` for display.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ObjectNotFound`] when the object is missing and
    /// [`Error::CorruptObject`] when parsing fails.
    pub fn read_object_for_display(&self, oid: &ObjectId) -> Result<ObjectContents> {
        let obj = self.read_object_raw_for_display(oid)?;
        match obj.kind {
            ObjectKind::Blob => Ok(ObjectContents::Blob(obj.data)),
            ObjectKind::Tree => Ok(ObjectContents::Tree(parse_tree(&obj.data)?)),
            ObjectKind::Commit => Ok(ObjectContents::Commit(parse_commit(&obj.data)?)),
            ObjectKind::Tag => Ok(ObjectContents::Tag(parse_tag(&obj.data)?)),
        }
    }
}

/// Format a tree entry mode for `git cat-file -p` output (six-digit tree modes).
#[must_use]
pub fn tree_entry_mode_cat_file_p(mode: u32) -> String {
    if mode == 0o040000 {
        "040000".to_owned()
    } else {
        format!("{:o}", mode)
    }
}

/// Object type label for one tree entry in `git cat-file -p` / `ls-tree` output.
///
/// Git uses mode for trees and gitlinks; other entries use the stored object kind when present.
#[must_use]
pub fn tree_entry_cat_file_type(repo: &Repository, mode: u32, oid: &ObjectId) -> &'static str {
    if mode == 0o040000 {
        return "tree";
    }
    if mode == 0o160000 {
        return "commit";
    }
    if let Ok(info) = repo.odb().read_info(oid) {
        return object_kind_cat_file_label(info.kind);
    }
    "blob"
}

/// Format a tree object's payload like `git cat-file -p` (one line per entry).
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when `data` is not a valid tree.
pub fn format_tree_cat_file_p(repo: &Repository, data: &[u8]) -> Result<Vec<u8>> {
    let entries = parse_tree(data)?;
    let mut out = Vec::new();
    for e in &entries {
        out.extend_from_slice(tree_entry_mode_cat_file_p(e.mode).as_bytes());
        out.push(b' ');
        out.extend_from_slice(tree_entry_cat_file_type(repo, e.mode, &e.oid).as_bytes());
        out.push(b' ');
        out.extend_from_slice(e.oid.to_hex().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(&e.name);
        out.push(b'\n');
    }
    Ok(out)
}

fn object_kind_cat_file_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

/// Abbreviate `oid` for human output using repository uniqueness rules.
pub fn abbreviate_for_display(repo: &Repository, oid: ObjectId, short: bool) -> Result<String> {
    if short {
        abbreviate_object_id(repo, oid, 7)
    } else {
        Ok(oid.to_hex())
    }
}

fn map_resolve_ref_error(err: Error) -> RevParseError {
    match err {
        Error::RevParse(e) => e,
        other => RevParseError::InvalidObjectName {
            name: other.to_string(),
        },
    }
}

/// Leading `^` revision-set exclude (`^ref`), not parent shorthand (`rev^` / `rev^1`).
fn is_revision_set_exclude_spec(spec: &str) -> bool {
    spec.starts_with('^')
}

fn infer_ref_names(repo: &Repository, spec: &str, oid: ObjectId) -> Option<ResolvedRefName> {
    let rev_part = spec.split(':').next().unwrap_or(spec);
    let (base, _) = parse_peel_suffix(rev_part);
    if base.is_empty() || looks_like_abbrev_hex(base, repo) {
        return None;
    }
    let resolved = match repo.resolve_ref_name(base) {
        Ok(v) => v?,
        Err(RevParseError::InvalidObjectName { .. }) => return None,
        Err(_) => return None,
    };
    if repo.resolve_ref_oid(&resolved.full).ok() == Some(oid) {
        Some(resolved)
    } else {
        None
    }
}

fn looks_like_abbrev_hex(base: &str, repo: &Repository) -> bool {
    if base.len() >= repo.odb().hash_algo().hex_len() {
        return base.chars().all(|c| c.is_ascii_hexdigit());
    }
    base.len() >= 4 && base.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_error_from_path_not_in_revision() {
        let err = ResolveError::from(RevParseError::PathNotInRevision {
            path: "missing.txt".into(),
            revision: "HEAD".into(),
        });
        assert!(matches!(err, ResolveError::PathNotInTree { .. }));
    }

    #[test]
    fn tree_entry_mode_cat_file_p_uses_six_digit_tree_mode() {
        assert_eq!(tree_entry_mode_cat_file_p(0o040000), "040000");
        assert_eq!(tree_entry_mode_cat_file_p(0o100644), "100644");
    }
}
