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
use crate::objects::odb::store::ObjectStream;
use crate::objects::{
    parse_commit, parse_tag, parse_tree, CommitData, Object, ObjectId, ObjectInfo, ObjectKind,
    TagData, TreeEntry,
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

    /// Reflog `@{date}` inner text could not be parsed as a date.
    #[error("invalid reflog date '{date}'")]
    ReflogDateUnparseable {
        /// Text inside `@{...}` that was treated as a date selector.
        date: String,
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
            RevParseError::ReflogDateUnparseable { date } => {
                ResolveError::ReflogDateUnparseable { date }
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

/// A branch or tag label for a revision, independent of its display format.
///
/// The label describes how a name was supplied, rather than the kind of object
/// it resolves to. A tag can, for example, point to a blob or another tag.
#[derive(Debug, Clone)]
pub enum RevisionLabel {
    /// A local branch, including the branch named by symbolic `HEAD`.
    Branch {
        /// Branch name without `refs/heads/`.
        name: String,
    },
    /// A lightweight or annotated tag.
    Tag {
        /// Ref name without `refs/tags/`, which can differ from the tag header.
        name: String,
        /// Parsed tag object, or `None` for a lightweight tag.
        annotation: Option<TagData>,
    },
}

impl Repository {
    /// Classify `spec` as a short local branch or tag name for display.
    ///
    /// Symbolic `HEAD` labels its local branch, including an unborn branch;
    /// detached `HEAD` has no label. Otherwise, short branch names take precedence
    /// over short tag names. Other inputs return `None` unless they themselves
    /// name a short branch or tag. This does not interpret revision expressions
    /// or fully qualified ref names; use [`Self::resolve`] to resolve a revision.
    ///
    /// Uses this repository's ref store, including injected backends.
    /// Annotated tags retain their raw metadata and message, without peeling
    /// the target or trimming whitespace. Ref lookup failures are treated as
    /// absent labels, so callers can resolve the revision independently.
    ///
    /// # Errors
    ///
    /// Returns an object-store error if a named tag's object cannot be read, or
    /// [`Error::CorruptObject`] if an annotated tag cannot be parsed.
    #[inline]
    pub fn revision_label(&self, spec: &str) -> Result<Option<RevisionLabel>> {
        if spec == "HEAD" {
            return Ok(crate::refs::read_symbolic_ref_for_repository(self, "HEAD")
                .ok()
                .flatten()
                .and_then(|target| {
                    target
                        .strip_prefix("refs/heads/")
                        .map(|name| RevisionLabel::Branch {
                            name: name.to_owned(),
                        })
                }));
        }

        if self.refs().resolve(&format!("refs/heads/{spec}")).is_ok() {
            return Ok(Some(RevisionLabel::Branch {
                name: spec.to_owned(),
            }));
        }

        let Ok(oid) = self.refs().resolve(&format!("refs/tags/{spec}")) else {
            return Ok(None);
        };
        let object = self.odb().read(&oid)?;
        let annotation = if object.kind == ObjectKind::Tag {
            Some(parse_tag(&object.data)?)
        } else {
            None
        };
        Ok(Some(RevisionLabel::Tag {
            name: spec.to_owned(),
            annotation,
        }))
    }

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

    /// Object kind and size for display, following replace refs when enabled.
    ///
    /// `oid` names the original object. Returns metadata for its replacement when
    /// replacement is enabled, without loading loose or whole packed blob payloads.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ObjectNotFound`] when the object is missing.
    pub fn read_object_info_for_display(&self, oid: &ObjectId) -> Result<ObjectInfo> {
        self.read_info_replaced(oid)
    }

    /// Stream an object's payload for display, following replace refs when enabled.
    ///
    /// `oid` names the original object and `f` consumes the effective object's stream.
    /// Returns `Ok(Some(result))` with the callback's result, or `Ok(None)` when the
    /// object is missing (without calling `f`). Loose and whole packed objects are
    /// inflated as the callback reads; pack deltas are reconstructed in memory.
    ///
    /// # Errors
    ///
    /// Propagates backend failures and errors from `f`. Corrupt or truncated payloads
    /// surface as [`std::io::Error`] values when reading the stream.
    pub fn with_object_stream_for_display<R>(
        &self,
        oid: &ObjectId,
        f: impl FnOnce(ObjectStream<'_>) -> Result<R>,
    ) -> Result<Option<R>> {
        let effective = self.object_id_after_replace(oid);
        self.odb().with_stream(&effective, f)
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
/// Git derives the type word from the mode bits (`object_type()` in Git's tree-walk code),
/// not from reading the referenced object.
#[must_use]
pub fn tree_entry_cat_file_type(mode: u32) -> &'static str {
    match mode & 0o170_000 {
        0o040_000 => "tree",
        0o160_000 => "commit",
        _ => "blob",
    }
}

/// Format parsed tree entries like `git cat-file -p` (one line per entry).
#[must_use]
pub fn format_tree_entries_cat_file_p(entries: &[TreeEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        out.extend_from_slice(tree_entry_mode_cat_file_p(e.mode).as_bytes());
        out.push(b' ');
        out.extend_from_slice(tree_entry_cat_file_type(e.mode).as_bytes());
        out.push(b' ');
        out.extend_from_slice(e.oid.to_hex().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(&e.name);
        out.push(b'\n');
    }
    out
}

/// Format a tree object's payload like `git cat-file -p` (one line per entry).
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when `data` is not a valid tree.
pub fn format_tree_cat_file_p(data: &[u8]) -> Result<Vec<u8>> {
    let entries = parse_tree(data)?;
    Ok(format_tree_entries_cat_file_p(&entries))
}

#[cfg(test)]
mod tree_entry_cat_file_type_tests {
    use super::tree_entry_cat_file_type;

    #[test]
    fn mode_mask_labels_match_git() {
        assert_eq!(tree_entry_cat_file_type(0o040_000), "tree");
        assert_eq!(tree_entry_cat_file_type(0o040_755), "tree");
        assert_eq!(tree_entry_cat_file_type(0o160_000), "commit");
        assert_eq!(tree_entry_cat_file_type(0o100_644), "blob");
        assert_eq!(tree_entry_cat_file_type(0o100_755), "blob");
        assert_eq!(tree_entry_cat_file_type(0o120_000), "blob");
        assert_eq!(tree_entry_cat_file_type(0o100_664), "blob");
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
    fn label_repo() -> (tempfile::TempDir, Repository, ObjectId) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new().with_initial_branch("main"),
        )
        .expect("init");
        let tree = repo.odb().write(ObjectKind::Tree, b"").expect("tree");
        let commit = repo.odb().write(ObjectKind::Commit, format!(
            "tree {tree}\nauthor A <a@example.com> 1 +0000\ncommitter A <a@example.com> 1 +0000\n\ninitial\n"
        ).as_bytes()).expect("commit");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &commit).expect("branch");
        (dir, repo, commit)
    }

    #[test]
    fn revision_labels_distinguish_names_from_expressions() {
        let (_dir, repo, oid) = label_repo();
        for spec in ["HEAD", "main"] {
            assert!(matches!(repo.revision_label(spec).expect("label"),
                Some(RevisionLabel::Branch { name }) if name == "main"));
        }
        for spec in [
            oid.to_hex().as_str(),
            "HEAD~1",
            "refs/heads/main",
            "missing",
        ] {
            assert!(
                repo.revision_label(spec).expect("label").is_none(),
                "{spec}"
            );
        }
        crate::refs::write_ref(repo.git_dir(), "HEAD", &oid).expect("detach");
        assert!(repo
            .revision_label("HEAD")
            .expect("detached label")
            .is_none());
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/tags/main")
            .expect("non-branch HEAD");
        assert!(repo
            .revision_label("HEAD")
            .expect("non-branch label")
            .is_none());
    }

    #[test]
    fn revision_label_keeps_unborn_branch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new().with_initial_branch("new"),
        )
        .expect("init");
        assert!(matches!(repo.revision_label("HEAD").expect("label"),
            Some(RevisionLabel::Branch { name }) if name == "new"));
        assert!(repo
            .revision_label("new")
            .expect("missing branch")
            .is_none());
    }

    #[test]
    fn revision_label_preserves_tag_metadata_and_branch_precedence() {
        let (_dir, repo, oid) = label_repo();
        crate::refs::write_ref(repo.git_dir(), "refs/tags/main", &oid).expect("tag");
        assert!(matches!(
            repo.revision_label("main").expect("branch wins"),
            Some(RevisionLabel::Branch { .. })
        ));
        crate::refs::write_ref(repo.git_dir(), "refs/tags/light", &oid).expect("tag");
        assert!(
            matches!(repo.revision_label("light").expect("lightweight tag"),
            Some(RevisionLabel::Tag { name, annotation: None }) if name == "light")
        );

        let blob = repo
            .odb()
            .write(ObjectKind::Blob, b"content")
            .expect("blob");
        let tag = repo
            .odb()
            .write(
                ObjectKind::Tag,
                format!("object {blob}\ntype blob\ntag original\n\nmessage\n\n").as_bytes(),
            )
            .expect("tag object");
        crate::refs::write_ref(repo.git_dir(), "refs/tags/alias", &tag).expect("tag ref");
        let Some(RevisionLabel::Tag {
            name,
            annotation: Some(tag),
        }) = repo.revision_label("alias").expect("annotated tag")
        else {
            panic!("expected annotation");
        };
        assert_eq!(name, "alias");
        assert_eq!(tag.tag, "original");
        assert_eq!(tag.object, blob);
        assert_eq!(tag.object_type, "blob");
        assert_eq!(tag.tagger, None);
        assert_eq!(tag.message, "message\n\n");
    }

    #[test]
    fn revision_label_propagates_tag_object_errors() {
        let (_dir, repo, _oid) = label_repo();
        let broken = repo
            .odb()
            .write(ObjectKind::Tag, b"invalid")
            .expect("broken object");
        crate::refs::write_ref(repo.git_dir(), "refs/tags/broken", &broken).expect("tag ref");
        assert!(matches!(
            repo.revision_label("broken"),
            Err(Error::CorruptObject(_))
        ));
        let missing = ObjectId::from_hex("1111111111111111111111111111111111111111").expect("oid");
        let dangling = crate::refs::store::RefTransaction::new()
            .update(crate::refs::store::RefUpdate {
                name: "refs/tags/missing".to_owned(),
                new_value: Some(RawRef::Direct(missing)),
                expected: crate::refs::store::Expected::Any,
                reflog: None,
                flags: crate::refs::store::RefUpdateFlags {
                    skip_oid_verification: true,
                    ..Default::default()
                },
            })
            .expect("txn");
        repo.refs()
            .prepare(dangling)
            .and_then(|prepared| prepared.commit())
            .expect("dangling tag ref");
        assert!(matches!(
            repo.revision_label("missing"),
            Err(Error::ObjectNotFound(_))
        ));
    }
    #[test]
    fn revision_labels_use_injected_ref_store() {
        use crate::refs::store::MemoryRefStore;
        use std::sync::Arc;

        let (_dir, repo, oid) = label_repo();
        let refs = Arc::new(MemoryRefStore::new());
        refs.set_ref("HEAD", RawRef::Symbolic("refs/heads/virtual".into()))
            .expect("HEAD");
        refs.set_ref("refs/heads/virtual", RawRef::Direct(oid))
            .expect("branch");
        refs.set_ref("refs/tags/virtual-tag", RawRef::Direct(oid))
            .expect("tag");
        let repo = repo.with_ref_store(refs);
        for spec in ["HEAD", "virtual"] {
            assert!(matches!(repo.revision_label(spec).expect("branch label"),
                Some(RevisionLabel::Branch { name }) if name == "virtual"));
        }
        assert!(
            matches!(repo.revision_label("virtual-tag").expect("tag label"),
            Some(RevisionLabel::Tag { name, annotation: None }) if name == "virtual-tag")
        );
        assert!(repo
            .revision_label("main")
            .expect("disk branch is excluded")
            .is_none());
    }
}
