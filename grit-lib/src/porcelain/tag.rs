//! Tag creation, listing, deletion and the list filter/sort helpers.
//!
//! The `grit tag` command parses argv, resolves identity, and formats output;
//! what lives here is presentation-free tag logic: creating lightweight and
//! annotated tags ([`create_tag`]), listing ([`list_tags`]) and deleting
//! ([`delete_tag`]) tag refs, filter/sort predicates, and `-m`/`-F` message
//! cleanup ([`strip_comments`]).
//!
//! # What this module owns
//!
//! - Tag creation ([`create_tag`], [`CreateTagRequest`], [`TagKind`]), listing
//!   ([`list_tags`], [`TagSummary`], [`tag_ref_metadata`]) and deletion ([`delete_tag`]).
//! - Ref list reachability filters (`--contains`, `--merged`, `--points-at`) live in [`crate::refs::listing`].
//! - Ref list sort/filter helpers live in [`crate::refs::listing`] (`compare_version_names`,
//!   [`crate::refs::listing::pattern_matches_short_name`], [`creator_date`],
//!   [`crate::refs::listing::epoch_from_identity`], [`crate::refs::listing::ref_query_options_for_repo`]).
//! - Tag message cleanup: [`strip_comments`] then [`crate::porcelain::commit::normalize_commit_message`]
//!   via [`normalize_tag_message`].

use crate::check_ref_format::validate_tag_short_name;
use crate::commit_pretty::message_subject;
use crate::diff_rename_parallel::parallel_map_indices;
use crate::error::{Error, Result};
use crate::hash::hash_object;
use crate::ident::signature_timestamp_for_pretty;
use crate::objects::{parse_tag, serialize_tag, ObjectId, ObjectKind, TagData};
use crate::parallel::ParallelOp;
use crate::refs;
use crate::refs::listing::{
    compare_version_names, creator_date as listing_creator_date, epoch_from_identity,
    pattern_matches_short_name, query_refs, ref_query_options_for_repo, RefListing, RefNamespace,
    RefQuery, RefQueryError,
};
use crate::repo::Repository;
use crate::signing::{should_sign_tag, sign_serialized_tag, GpgConfig};
use crate::tree_sha256::{TagTreeSha256Outcome, TreeSha256Claim, TreeSha256Options};
use std::time::Instant;

const TAGS_PREFIX: &str = "refs/tags/";

/// Tag create/delete validation failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TagError {
    /// `refs/tags/<name>` already exists and `force` was not set.
    #[error("tag '{name}' already exists")]
    AlreadyExists {
        /// Short tag name.
        name: String,
    },
    /// The tag short name failed ref-format checks.
    #[error("invalid tag name: {detail}")]
    InvalidName {
        /// Human-readable detail (from [`RefNameError`](crate::refs::check_ref_format::RefNameError)).
        detail: String,
    },
    /// `refs/tags/<name>` does not exist.
    #[error("no tag named '{name}'")]
    NotFound {
        /// Short tag name.
        name: String,
    },
    /// `-m` / `-F` input was empty after comment stripping and whitespace cleanup.
    #[error("tag message cannot be empty")]
    EmptyMessage,
}

/// Whether a `refs/tags/*` ref is lightweight or an annotated tag object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagKind {
    /// The ref points directly at a commit (or other object).
    Lightweight,
    /// The ref points at a tag object with message and tagger.
    Annotated,
}

/// Parsed name and email from a Git identity line (without timestamp).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ident {
    /// Display name before `<`.
    pub name: String,
    /// Email inside angle brackets.
    pub email: String,
}

/// Annotated tag message and tagger metadata for list output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagAnnotation {
    /// First-line subject (`format_subject` / `contents:subject`).
    pub subject: String,
    /// Full tag message body.
    pub message: String,
    /// Tagger identity (name and email).
    pub tagger: Ident,
    /// Tagger date as Unix seconds (`taggerdate:unix`).
    pub date: i64,
}

/// One tag in the result of [`list_tags`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagSummary {
    /// Short tag name (without `refs/tags/`).
    pub name: String,
    /// Object id stored at `refs/tags/<name>` (a tag object for annotated tags).
    pub oid: ObjectId,
    /// Whether the tag is lightweight or annotated.
    pub kind: TagKind,
    /// Commit the tag ultimately references, when it peels to one.
    pub peeled: Option<ObjectId>,
    /// Kind of object the tag ref points at (or the tag object's `object` header).
    pub target_kind: ObjectKind,
    /// Present for annotated tags after reading the tag object.
    pub annotation: Option<TagAnnotation>,
}

/// Deprecated alias for [`TagSummary`].
#[deprecated(note = "use TagSummary instead")]
pub type TagInfo = TagSummary;

/// Inputs for [`create_tag`].
#[derive(Debug, Clone)]
pub struct CreateTagRequest {
    /// Short tag name (without `refs/tags/`).
    pub name: String,
    /// Object the tag should point at (usually a commit).
    pub target: ObjectId,
    /// When set, build an annotated tag object with this message (after [`strip_comments`]).
    pub message: Option<String>,
    /// Tagger identity line (`Name <email> <epoch> <tz>`); required for annotated tags.
    pub tagger: Option<String>,
    /// When `Some(true)` or `Some(false)`, force signing on or off. When `None`, honor `tag.gpgSign`.
    pub sign: Option<bool>,
    /// Override `user.signingkey` for this tag (same resolution as commit signing).
    pub signing_key: Option<String>,
    /// When set, attach a tree-sha256 claim (implies signing).
    pub tree_sha256: Option<TreeSha256Options>,
    /// Replace an existing tag ref when set.
    pub force: bool,
    /// Compute the outcome without updating refs (or writing new tag objects).
    pub dry_run: bool,
}

/// Result of [`create_tag`].
#[derive(Debug, Clone)]
pub struct CreateTagOutcome {
    /// Short tag name (without `refs/tags/`).
    pub name: String,
    /// Object id stored at `refs/tags/<name>` (commit oid or tag object).
    pub oid: ObjectId,
    /// Whether the tag is lightweight or annotated.
    pub kind: TagKind,
    /// Commit oid when the tag ultimately references a commit.
    pub peeled: Option<ObjectId>,
    /// Whether the tag object was signed (always false for lightweight tags).
    pub signed: bool,
    /// Present when a tree-sha256 claim was computed and written on the tag object.
    pub tree_sha256: Option<TagTreeSha256Outcome>,
    /// When `force` replaced an existing tag, the previous ref target.
    pub replaced: Option<ObjectId>,
    /// Whether this outcome was preview-only (no ref or object writes).
    pub dry_run: bool,
}

/// Create a lightweight or annotated tag ref.
///
/// # Errors
///
/// Returns [`TagError`] when the name is invalid, the tag exists without `force`,
/// or the message is empty after cleanup. Propagates object-store and signing errors.
pub fn create_tag(repo: &Repository, req: &CreateTagRequest) -> Result<CreateTagOutcome> {
    validate_tag_short_name(&req.name).map_err(|e| {
        Error::Tag(TagError::InvalidName {
            detail: e.to_string(),
        })
    })?;
    let tag_ref = format!("refs/tags/{}", req.name);
    let replaced = match refs::resolve_ref(repo.git_dir(), &tag_ref) {
        Ok(existing) => {
            if req.force {
                Some(existing)
            } else {
                return Err(Error::Tag(TagError::AlreadyExists {
                    name: req.name.clone(),
                }));
            }
        }
        Err(_) => None,
    };

    if let Some(ref raw) = req.message {
        if normalize_tag_message(raw).is_err() {
            return Err(Error::Tag(TagError::EmptyMessage));
        }
    }

    let target_kind = repo.odb().read_info(&req.target)?.kind;
    let peeled = repo.odb().peel_to_commit(req.target).ok().flatten();

    let config = repo.config()?;
    let tree_opts = req.tree_sha256.clone();
    if tree_opts.is_some() && req.sign == Some(false) {
        return Err(Error::TreeSha256RequiresSigning);
    }
    let mut sign = should_sign_tag(config.as_ref(), req.sign)?;
    if tree_opts.is_some() {
        sign = true;
    }
    let wants_annotated = req.message.is_some() || sign || tree_opts.is_some();
    let mut signed = false;
    let mut tree_sha256_outcome = None;
    let (ref_oid, kind) = if wants_annotated {
        let tagger = req
            .tagger
            .as_ref()
            .ok_or_else(|| Error::Message("annotated tag requires a tagger identity".into()))?;
        let message = req
            .message
            .as_deref()
            .map(|raw| normalize_tag_message(raw).unwrap_or_default())
            .unwrap_or_default();
        let write_tag_bytes = |bytes: &[u8]| -> Result<ObjectId> {
            if req.dry_run {
                hash_object(repo.odb().hash_algo(), ObjectKind::Tag, bytes)
            } else {
                repo.odb().write(ObjectKind::Tag, bytes)
            }
        };
        if let Some(opts) = tree_opts {
            let hash_start = Instant::now();
            let report = repo.tree_sha256(req.target, &opts)?;
            let hash_elapsed = hash_start.elapsed();
            let claim = TreeSha256Claim::from_report(&report);
            let mut tag = TagData {
                object: req.target,
                object_type: target_kind.as_str().to_owned(),
                tag: req.name.clone(),
                tagger: Some(tagger.clone()),
                extra_headers: Vec::new(),
                message,
            };
            claim.apply_to_tag(&mut tag);
            tree_sha256_outcome = Some(TagTreeSha256Outcome {
                claim,
                report,
                hash_elapsed,
            });
            let unsigned = serialize_tag(&tag);
            let gpg =
                GpgConfig::from_config(config.as_ref())?.with_command_runner(repo.command_runner());
            signed = true;
            let bytes = sign_serialized_tag(&gpg, &unsigned, tagger, req.signing_key.as_deref())?;
            (write_tag_bytes(&bytes)?, TagKind::Annotated)
        } else {
            let tag = TagData {
                object: req.target,
                object_type: target_kind.as_str().to_owned(),
                tag: req.name.clone(),
                tagger: Some(tagger.clone()),
                extra_headers: Vec::new(),
                message,
            };
            let unsigned = serialize_tag(&tag);
            let bytes = if sign {
                let gpg = GpgConfig::from_config(config.as_ref())?
                    .with_command_runner(repo.command_runner());
                signed = true;
                sign_serialized_tag(&gpg, &unsigned, tagger, req.signing_key.as_deref())?
            } else {
                unsigned
            };
            (write_tag_bytes(&bytes)?, TagKind::Annotated)
        }
    } else {
        (req.target, TagKind::Lightweight)
    };

    if !req.dry_run {
        refs::write_ref(repo.git_dir(), &tag_ref, &ref_oid)?;
    }

    Ok(CreateTagOutcome {
        name: req.name.clone(),
        oid: ref_oid,
        kind,
        peeled,
        signed,
        tree_sha256: tree_sha256_outcome,
        replaced,
        dry_run: req.dry_run,
    })
}

/// Classify a tag ref target and peel to a commit when possible.
#[must_use]
pub fn tag_ref_metadata(repo: &Repository, ref_oid: &ObjectId) -> (TagKind, Option<ObjectId>) {
    let Ok(info) = repo.odb().read_info(ref_oid) else {
        return (TagKind::Lightweight, None);
    };
    match info.kind {
        ObjectKind::Commit => (TagKind::Lightweight, Some(*ref_oid)),
        ObjectKind::Tag => (
            TagKind::Annotated,
            repo.odb().peel_to_commit(*ref_oid).ok().flatten(),
        ),
        ObjectKind::Tree | ObjectKind::Blob => (TagKind::Lightweight, None),
    }
}

/// List tags matching `query` (always scoped to [`RefNamespace::Tags`]).
///
/// Annotated tag objects are read in parallel when the thread policy allows.
/// Default CLI sort is `version`; pass [`RefQuery::sort`] explicitly in library callers.
///
/// # Errors
///
/// Propagates ref-query, config, and object read failures.
pub fn list_tags(repo: &Repository, query: &RefQuery) -> Result<Vec<TagSummary>> {
    let mut query = query.clone();
    query.namespaces = vec![RefNamespace::Tags];
    let options = ref_query_options_for_repo(repo)?;
    let listings = query_refs(repo, &query, &options).map_err(ref_query_to_error)?;
    if listings.is_empty() {
        return Ok(Vec::new());
    }
    let config = repo.config()?;
    let par = repo
        .thread_policy()
        .resolve(ParallelOp::DiffContent, config.as_ref());
    let odb = repo.odb();
    odb.with_pack_read_context(|| {
        parallel_map_indices(odb, listings.len(), par, listings.len() * 512, |i| {
            summarize_listing(repo, &listings[i])
        })
    })
}

fn ref_query_to_error(err: RefQueryError) -> Error {
    match err {
        RefQueryError::Store(e) => e,
        RefQueryError::MissingObjectDatabase => {
            Error::Message("ref query requires an object database".into())
        }
        RefQueryError::MissingObject(oid) => Error::Message(format!("object {oid} not found")),
        RefQueryError::NotCommit(oid) => Error::Message(format!("object {oid} is not a commit")),
    }
}

/// Build the [`TagSummary`] for one listed tag ref.
///
/// The listing's peeled value is only ever set for annotated tags (from `packed-refs` peel
/// lines or by peeling the tag object), so those go straight to a single read of the tag
/// object; every other ref costs one object-header lookup.
fn summarize_listing(repo: &Repository, listing: &RefListing) -> Result<TagSummary> {
    let name = listing.short_name.clone();
    let oid = listing.oid;
    let odb = repo.odb();
    if let Some(peeled) = listing.peeled {
        match odb.read(&oid) {
            Ok(obj) if obj.kind == ObjectKind::Tag => {
                return annotated_summary(name, oid, Some(peeled), &obj.data);
            }
            Ok(obj) => return Ok(direct_summary(name, oid, obj.kind)),
            Err(err) => {
                if odb.read_info(&oid).is_ok() {
                    return Err(err);
                }
                return Ok(missing_summary(name, oid, Some(peeled)));
            }
        }
    }
    let Ok(info) = odb.read_info(&oid) else {
        return Ok(missing_summary(name, oid, None));
    };
    if info.kind != ObjectKind::Tag {
        return Ok(direct_summary(name, oid, info.kind));
    }
    let obj = odb.read(&oid)?;
    let peeled = odb.peel_to_commit(oid).ok().flatten();
    annotated_summary(name, oid, peeled, &obj.data)
}

fn annotated_summary(
    name: String,
    oid: ObjectId,
    peeled: Option<ObjectId>,
    data: &[u8],
) -> Result<TagSummary> {
    let tag = parse_tag(data)?;
    Ok(TagSummary {
        name,
        oid,
        kind: TagKind::Annotated,
        peeled,
        target_kind: object_kind_from_type_name(&tag.object_type),
        annotation: tag_annotation_from_data(&tag),
    })
}

/// Summary for a ref that names a non-tag object directly (a lightweight tag).
fn direct_summary(name: String, oid: ObjectId, kind: ObjectKind) -> TagSummary {
    TagSummary {
        name,
        oid,
        kind: TagKind::Lightweight,
        peeled: (kind == ObjectKind::Commit).then_some(oid),
        target_kind: kind,
        annotation: None,
    }
}

/// Summary for a ref whose object cannot be read.
fn missing_summary(name: String, oid: ObjectId, peeled: Option<ObjectId>) -> TagSummary {
    TagSummary {
        name,
        oid,
        kind: TagKind::Lightweight,
        peeled,
        target_kind: ObjectKind::Blob,
        annotation: None,
    }
}

fn tag_annotation_from_data(tag: &TagData) -> Option<TagAnnotation> {
    let tagger_line = tag.tagger.as_deref()?;
    let tagger = ident_from_line(tagger_line);
    let date = match signature_timestamp_for_pretty(tagger_line) {
        crate::ident::SignatureTimestamp::Valid(ts) => ts,
        crate::ident::SignatureTimestamp::Sentinel => epoch_from_identity(tagger_line),
    };
    Some(TagAnnotation {
        subject: message_subject(&tag.message),
        message: tag.message.clone(),
        tagger,
        date,
    })
}

fn ident_from_line(line: &str) -> Ident {
    let (name, rest) = line.split_once(" <").unwrap_or((line, ""));
    let email = rest
        .split_once('>')
        .map_or(rest.trim_end_matches('>').trim(), |(e, _)| e.trim());
    Ident {
        name: name.trim().to_owned(),
        email: email.to_owned(),
    }
}

fn object_kind_from_type_name(s: &str) -> ObjectKind {
    match s {
        "commit" => ObjectKind::Commit,
        "tree" => ObjectKind::Tree,
        "blob" => ObjectKind::Blob,
        "tag" => ObjectKind::Tag,
        _ => ObjectKind::Blob,
    }
}

/// Delete `refs/tags/<name>` through the repository's ref store.
///
/// The tag object of an annotated tag stays in the object database.
///
/// # Returns
///
/// The object id the tag ref pointed at before deletion.
///
/// # Errors
///
/// Returns [`TagError::NotFound`] when the tag does not exist, and propagates ref
/// store failures.
pub fn delete_tag(repo: &Repository, name: &str, dry_run: bool) -> Result<ObjectId> {
    let tag_ref = format!("{TAGS_PREFIX}{name}");
    let existing = repo.references().read_existing(&tag_ref)?.ok_or_else(|| {
        Error::Tag(TagError::NotFound {
            name: name.to_owned(),
        })
    })?;
    let oid = existing.oid;
    if !dry_run {
        repo.references().delete_verified(&tag_ref, &existing)?;
    }
    Ok(oid)
}

/// Extract the "creator date" for a tag object.
///
/// For annotated tags, this is the tagger date.  For lightweight tags
/// (which point directly at a commit), this is the committer date.
/// Returns 0 if the date cannot be determined.
#[must_use]
pub fn creator_date(repo: &Repository, oid: &ObjectId) -> i64 {
    listing_creator_date(repo.odb(), oid).unwrap_or(0)
}

/// Extract the epoch timestamp from a Git identity string.
///
/// Format: `Name <email> <epoch> <offset>`
#[must_use]
pub fn parse_epoch_from_ident(ident: &str) -> i64 {
    epoch_from_identity(ident)
}

/// Compare two ref names using Git `version:refname` ordering (no configured suffix list).
#[must_use]
pub fn compare_version(a: &str, b: &str) -> std::cmp::Ordering {
    compare_version_names(a, b, &[])
}

/// Split a version string into segments at `.` and `-` boundaries.
///
/// Deprecated: prefer [`compare_version_names`] from [`crate::refs::listing`].
#[must_use]
#[deprecated(note = "use grit_lib::refs::listing::compare_version_names instead")]
pub fn version_segments(s: &str) -> Vec<&str> {
    s.split(['.', '-']).filter(|seg| !seg.is_empty()).collect()
}

/// Strip `#comment` lines from a message and normalize whitespace.
///
/// Also: strip trailing whitespace from each line, collapse multiple blank lines
/// to one, and drop leading/trailing blank lines. This is the `whitespace`
/// cleanup `git tag` applies to `-m`/`-F` messages (anything but `--cleanup=verbatim`).
#[must_use]
pub fn strip_comments(s: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in s.lines() {
        if line.starts_with('#') {
            continue;
        }
        lines.push(line.trim_end().to_string());
    }
    while lines.first().map(|l| l.is_empty()).unwrap_or(false) {
        lines.remove(0);
    }
    while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut result = Vec::new();
    let mut last_blank = false;
    for line in &lines {
        let is_blank = line.is_empty();
        if is_blank && last_blank {
            continue;
        }
        result.push(line.clone());
        last_blank = is_blank;
    }
    result.join("\n") + "\n"
}

/// Git tag `-m`/`-F` cleanup: drop `#` comment lines, then commit-style stripspace.
///
/// # Errors
///
/// Returns [`Error::Message`] when the body is empty after cleanup (same as
/// [`crate::porcelain::commit::normalize_commit_message`]).
pub fn normalize_tag_message(raw: &str) -> Result<String> {
    let stripped = strip_comments(raw);
    crate::porcelain::commit::normalize_commit_message(&stripped)
}

/// Whether `name` matches `pattern` (Git tag/branch list glob on the short name).
#[must_use]
pub fn glob_matches(pattern: &str, name: &str) -> bool {
    pattern_matches_short_name(name, pattern, false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::porcelain::commit::{create_commit, CommitRequest, HookPolicy};
    use crate::progress::NullProgress;
    use crate::refs::listing::{RefQuery, RefSort, SortKey};
    use crate::repo::InitOptions;

    fn repo_with_commit() -> (tempfile::TempDir, Repository, ObjectId) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(tmp.path(), &InitOptions::default()).expect("init");
        let req = CommitRequest {
            message: "one".into(),
            author: "T <t@example.com> 1700000000 +0000".into(),
            committer: "T <t@example.com> 1700000000 +0000".into(),
            allow_empty: true,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
            ..Default::default()
        };
        let oid = create_commit(&repo, &req, &mut NullProgress)
            .expect("commit")
            .oid;
        (tmp, repo, oid)
    }

    fn tag(repo: &Repository, name: &str, target: ObjectId, message: Option<&str>) -> ObjectId {
        create_tag(
            repo,
            &CreateTagRequest {
                name: name.into(),
                target,
                message: message.map(str::to_owned),
                tagger: Some("T <t@example.com> 1700000000 +0000".into()),
                sign: None,
                signing_key: None,
                tree_sha256: None,
                force: false,
                dry_run: false,
            },
        )
        .expect("create tag")
        .oid
    }

    fn name_sorted_query() -> RefQuery {
        RefQuery {
            namespaces: vec![RefNamespace::Tags],
            sort: RefSort {
                key: SortKey::Name,
                reverse: false,
            },
            ..RefQuery::default()
        }
    }

    #[test]
    fn list_tags_is_empty_without_tags() {
        let (_tmp, repo, _) = repo_with_commit();
        assert!(list_tags(&repo, &name_sorted_query()).unwrap().is_empty());
    }

    #[test]
    fn list_tags_sorts_and_classifies() {
        let (_tmp, repo, commit) = repo_with_commit();
        let annotated = tag(&repo, "v2", commit, Some("release"));
        tag(&repo, "v1", commit, None);

        let tags = list_tags(&repo, &name_sorted_query()).unwrap();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].name, "v1");
        assert_eq!(tags[0].oid, commit);
        assert_eq!(tags[0].kind, TagKind::Lightweight);
        assert_eq!(tags[0].target_kind, ObjectKind::Commit);
        assert_eq!(tags[1].name, "v2");
        assert_eq!(tags[1].oid, annotated);
        assert_eq!(tags[1].kind, TagKind::Annotated);
        assert!(tags[1].annotation.is_some());
    }

    #[test]
    fn delete_tag_returns_old_target_and_keeps_tag_object() {
        let (_tmp, repo, commit) = repo_with_commit();
        let annotated = tag(&repo, "v1", commit, Some("release"));

        assert_eq!(delete_tag(&repo, "v1", false).unwrap(), annotated);
        assert!(list_tags(&repo, &name_sorted_query()).unwrap().is_empty());
        assert!(repo.odb().exists(&annotated));
    }

    #[test]
    fn delete_missing_tag_is_typed() {
        let (_tmp, repo, _) = repo_with_commit();
        let err = delete_tag(&repo, "nope", false).unwrap_err();
        assert!(
            matches!(&err, Error::Tag(TagError::NotFound { name }) if name == "nope"),
            "{err:?}"
        );
        assert_eq!(err.to_string(), "no tag named 'nope'");
    }

    #[test]
    fn create_tag_already_exists_is_typed() {
        let (_tmp, repo, commit) = repo_with_commit();
        tag(&repo, "v1", commit, None);
        let err = create_tag(
            &repo,
            &CreateTagRequest {
                name: "v1".into(),
                target: commit,
                message: None,
                tagger: None,
                sign: None,
                signing_key: None,
                tree_sha256: None,
                force: false,
                dry_run: false,
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            Error::Tag(TagError::AlreadyExists { name }) if name == "v1"
        ));
    }

    #[test]
    fn create_tag_invalid_name_is_typed() {
        let (_tmp, repo, commit) = repo_with_commit();
        let err = create_tag(
            &repo,
            &CreateTagRequest {
                name: "bad..tag".into(),
                target: commit,
                message: None,
                tagger: None,
                sign: None,
                signing_key: None,
                tree_sha256: None,
                force: false,
                dry_run: false,
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::Tag(TagError::InvalidName { .. })));
    }

    #[test]
    fn create_tag_empty_message_is_typed() {
        let (_tmp, repo, commit) = repo_with_commit();
        let err = create_tag(
            &repo,
            &CreateTagRequest {
                name: "v1".into(),
                target: commit,
                message: Some("# only comment\n".into()),
                tagger: Some("T <t@example.com> 1700000000 +0000".into()),
                sign: None,
                signing_key: None,
                tree_sha256: None,
                force: false,
                dry_run: false,
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::Tag(TagError::EmptyMessage)));
    }
}
